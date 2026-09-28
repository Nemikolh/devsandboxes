//! The one way the host runs git on a sandbox's repo.
//!
//! A sandbox bind-mounts the repo (and, for worktree instances, the base
//! repo's `.git`) read-write, so anything in the container can write
//! `.git/config` and `.git/hooks`. Host git reads both: a planted hook, or a
//! config key such as `core.fsmonitor`, `core.sshCommand`, a filter driver or
//! `include.path`, would run as the host user during `worktree add`, `fetch`,
//! `worktree remove`, … — and dispatchers let a container trigger those calls
//! itself. [`host_git`] therefore disables hooks and fsmonitor on the command
//! line and refuses repos whose local config holds anything outside a small
//! allowlist of keys known not to run commands. The config is read as files,
//! never via `git config`, which would follow includes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// A `git` command for `repo`: its local config checked first (see the module
/// docs), hooks and fsmonitor off, no stdin and no credential prompts.
pub(crate) fn host_git(repo: &Path) -> Result<Command> {
    check_repo(repo)?;
    Ok(git_command(repo))
}

fn git_command(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(repo)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");
    // These would make git use another repo than the one `check_repo` read.
    for var in ["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE"] {
        cmd.env_remove(var);
    }
    cmd
}

/// Refuse `repo` when its local config (or an alternates file) could make
/// host git run commands or read objects from elsewhere.
pub(crate) fn check_repo(repo: &Path) -> Result<()> {
    let refuse = |what: String| -> anyhow::Error {
        anyhow::anyhow!("refusing to run git on {}: {what}", repo.display())
    };
    let shown = |p: &Path| p.strip_prefix(repo).unwrap_or(p).display().to_string();
    let (gitdir, common) = git_dirs(repo)?;
    let alternates = common.join("objects/info/alternates");
    if alternates.symlink_metadata().is_ok() {
        return Err(refuse(format!(
            "{} exists, which points host git at another object store; a sandbox may have \
             written it — review and remove it",
            shown(&alternates)
        )));
    }
    for file in config_files(&gitdir, &common).map_err(|e| refuse(format!("{e:#}")))? {
        let bytes = match std::fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(refuse(format!("cannot read {}: {e}", shown(&file)))),
        };
        let entries = parse_config(&String::from_utf8_lossy(&bytes)).map_err(|why| {
            refuse(format!("cannot parse {} ({why}); review and fix it by hand", shown(&file)))
        })?;
        if let Some(what) = entries.iter().find_map(refusal) {
            return Err(refuse(format!(
                "{} sets {what}, which can run commands on the host; a sandbox may have \
                 written it — review and remove it",
                shown(&file)
            )));
        }
    }
    Ok(())
}

/// `(gitdir, common dir)` of the checkout at `repo`, found the way git does
/// for `git -C repo` without running it: `<repo>/.git` is the git dir, or a
/// `gitdir: <path>` file pointing at it (a linked worktree); a `commondir`
/// file in the git dir points at the shared one. Anything else (no `.git`, a
/// bare repo) is refused rather than guessed at.
fn git_dirs(repo: &Path) -> Result<(PathBuf, PathBuf)> {
    let dotgit = repo.join(".git");
    let meta = std::fs::metadata(&dotgit)
        .with_context(|| format!("refusing to run git on {}: no .git found", repo.display()))?;
    let gitdir = if meta.is_dir() {
        dotgit
    } else {
        let text = std::fs::read_to_string(&dotgit)
            .with_context(|| format!("cannot read {}", dotgit.display()))?;
        let Some(path) = text.strip_prefix("gitdir:").map(str::trim) else {
            bail!("refusing to run git on {}: .git is not a `gitdir:` file", repo.display());
        };
        repo.join(path)
    };
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(text) => gitdir.join(text.trim()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => gitdir.clone(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", gitdir.join("commondir").display())),
    };
    Ok((gitdir, common))
}

/// Every config file git may read for any checkout of this repo: the shared
/// `config`, the main worktree's `config.worktree`, each linked worktree's
/// `config.worktree`, and the current git dir's own if it lives elsewhere.
/// Missing ones are skipped by the reader.
fn config_files(gitdir: &Path, common: &Path) -> Result<Vec<PathBuf>> {
    let mut files = std::collections::BTreeSet::new();
    files.insert(common.join("config"));
    files.insert(common.join("config.worktree"));
    files.insert(gitdir.join("config.worktree"));
    let worktrees = common.join("worktrees");
    match std::fs::read_dir(&worktrees) {
        Ok(dir) => {
            for entry in dir {
                let entry = entry.with_context(|| format!("cannot list {}", worktrees.display()))?;
                files.insert(entry.path().join("config.worktree"));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot list {}", worktrees.display())),
    }
    Ok(files.into_iter().collect())
}

/// One `key [= value]` line of a git config file. Section and key are
/// lowercased (git compares them case-insensitively); the subsection keeps
/// its case, except in the legacy `[section.sub]` form, which git lowercases.
#[derive(Debug, PartialEq)]
struct Entry {
    section: String,
    subsection: Option<String>,
    key: String,
    /// `None` for a bare `key` (boolean true).
    value: Option<String>,
}

impl Entry {
    fn name(&self) -> String {
        match &self.subsection {
            Some(sub) => format!("{}.{sub}.{}", self.section, self.key),
            None => format!("{}.{}", self.section, self.key),
        }
    }
}

/// Why `e` makes the repo unsafe for host git, or `None` when it is on the
/// allowlist (docs/automations.md step 17). Anything unknown is refused.
fn refusal(e: &Entry) -> Option<String> {
    let key = e.key.as_str();
    let value = e.value.as_deref().unwrap_or("").trim_start();
    let ext_url = || value.to_ascii_lowercase().starts_with("ext::");
    let allowed = match (e.section.as_str(), e.subsection.as_deref()) {
        ("core", None) => matches!(
            key,
            "repositoryformatversion"
                | "filemode"
                | "bare"
                | "logallrefupdates"
                | "ignorecase"
                | "precomposeunicode"
                | "symlinks"
                | "autocrlf"
                | "eol"
                | "safecrlf"
        ),
        ("extensions" | "worktree" | "rerere" | "color" | "advice", _) => true,
        ("remote", Some(_)) => match key {
            "url" | "pushurl" if ext_url() => return Some(format!("{} to an ext:: URL", e.name())),
            "url" | "pushurl" | "fetch" | "tagopt" | "prune" | "mirror" => true,
            _ => false,
        },
        // `vscode-merge-base` is written by VS Code's git extension, the
        // `github-pr-*` data keys by its GitHub Pull Requests extension.
        ("branch", Some(_)) => {
            matches!(
                key,
                "remote" | "merge" | "rebase" | "pushremote" | "description" | "vscode-merge-base"
            ) || key.starts_with("github-pr-")
        }
        ("user", None) => matches!(key, "name" | "email"),
        ("init", None) => key == "defaultbranch",
        ("pull", None) => matches!(key, "rebase" | "ff"),
        ("push", None) => matches!(key, "default" | "autosetupremote"),
        ("fetch", None) => key == "prune",
        ("gc", _) => !key.contains("hook"),
        // git-lfs runs custom transfer agents and extensions from its config
        // (`lfs.customtransfer.<name>.path`, `lfs.extension.<name>.clean`).
        ("lfs", sub) => {
            let sub = sub.unwrap_or("").to_ascii_lowercase();
            !(key == "standalonetransferagent"
                || sub.starts_with("customtransfer")
                || sub.starts_with("extension"))
        }
        ("submodule", Some(_)) => match key {
            "url" if ext_url() => return Some(format!("{} to an ext:: URL", e.name())),
            "update" if value.starts_with('!') => {
                return Some(format!("{} to a `!command`", e.name()))
            }
            "url" | "active" | "update" => true,
            _ => false,
        },
        _ => false,
    };
    (!allowed).then(|| e.name())
}

/// Parse git config syntax into its entries. Errors (`line N: why`) are
/// meant to fail closed: whatever git might read differently is refused.
fn parse_config(text: &str) -> std::result::Result<Vec<Entry>, String> {
    let s: Vec<char> = text.strip_prefix('\u{feff}').unwrap_or(text).chars().collect();
    let mut p = Parser { s: &s, i: 0, line: 1 };
    let mut section: Option<(String, Option<String>)> = None;
    let mut entries = Vec::new();
    while let Some(c) = p.peek() {
        match c {
            c if c.is_whitespace() => p.bump(),
            '#' | ';' => p.skip_comment(),
            '[' => {
                p.bump();
                section = Some(p.header()?);
            }
            c if c.is_ascii_alphabetic() => {
                let Some((sec, sub)) = &section else {
                    return Err(p.err("key before any [section]"));
                };
                let key = p.key();
                p.skip_blanks();
                let value = match p.peek() {
                    Some('=') => {
                        p.bump();
                        Some(p.value()?)
                    }
                    None | Some('\n' | '\r' | '#' | ';') => None,
                    Some(c) => return Err(p.err(&format!("unexpected `{c}` after key `{key}`"))),
                };
                entries.push(Entry { section: sec.clone(), subsection: sub.clone(), key, value });
            }
            c => return Err(p.err(&format!("unexpected `{c}`"))),
        }
    }
    Ok(entries)
}

struct Parser<'a> {
    s: &'a [char],
    i: usize,
    line: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn bump(&mut self) {
        if self.peek() == Some('\n') {
            self.line += 1;
        }
        self.i += 1;
    }

    fn err(&self, why: &str) -> String {
        format!("line {}: {why}", self.line)
    }

    fn skip_comment(&mut self) {
        while self.peek().is_some_and(|c| c != '\n') {
            self.bump();
        }
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.bump();
        }
    }

    /// After `[`: `name]`, `name "sub"]`, or legacy `name.sub]`.
    fn header(&mut self) -> std::result::Result<(String, Option<String>), String> {
        let mut name = String::new();
        while let Some(c) = self.peek().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.')) {
            name.push(c.to_ascii_lowercase());
            self.bump();
        }
        if name.is_empty() {
            return Err(self.err("empty section name"));
        }
        match self.peek() {
            Some(']') => {
                self.bump();
                Ok(match name.split_once('.') {
                    Some((sec, sub)) => (sec.to_string(), Some(sub.to_string())),
                    None => (name, None),
                })
            }
            Some(' ' | '\t') => {
                self.skip_blanks();
                if self.peek() != Some('"') {
                    return Err(self.err("expected `\"` in section header"));
                }
                self.bump();
                let mut sub = String::new();
                loop {
                    match self.peek() {
                        None | Some('\n') => return Err(self.err("unterminated subsection")),
                        Some('"') => break,
                        Some('\\') => {
                            self.bump();
                            match self.peek() {
                                None | Some('\n') => return Err(self.err("unterminated subsection")),
                                Some(c) => sub.push(c),
                            }
                        }
                        Some(c) => sub.push(c),
                    }
                    self.bump();
                }
                self.bump();
                if self.peek() != Some(']') {
                    return Err(self.err("expected `]` after subsection"));
                }
                self.bump();
                Ok((name, Some(sub)))
            }
            _ => Err(self.err("malformed section header")),
        }
    }

    fn key(&mut self) -> String {
        let mut key = String::new();
        while let Some(c) = self.peek().filter(|c| c.is_ascii_alphanumeric() || *c == '-') {
            key.push(c.to_ascii_lowercase());
            self.bump();
        }
        key
    }

    /// After `=`: leading and unquoted trailing blanks dropped, quotes
    /// stripped, `\n \t \b \\ \"` unescaped, `\` + newline continues the
    /// value, unquoted `#`/`;` start a comment.
    fn value(&mut self) -> std::result::Result<String, String> {
        self.skip_blanks();
        let (mut v, mut keep, mut quoted) = (String::new(), 0, false);
        loop {
            match self.peek() {
                None | Some('\n') if quoted => return Err(self.err("unterminated quote")),
                None | Some('\n') => break,
                Some('#' | ';') if !quoted => {
                    self.skip_comment();
                    break;
                }
                Some('"') => quoted = !quoted,
                Some('\\') => {
                    self.bump();
                    match self.peek() {
                        Some('\n') => {}
                        Some('\r') if self.s.get(self.i + 1) == Some(&'\n') => self.bump(),
                        Some(c @ ('n' | 't' | 'b' | '\\' | '"')) => {
                            v.push(match c {
                                'n' => '\n',
                                't' => '\t',
                                'b' => '\u{8}',
                                c => c,
                            });
                            keep = v.len();
                        }
                        _ => return Err(self.err("invalid escape in value")),
                    }
                }
                Some(c @ (' ' | '\t' | '\r')) if !quoted => v.push(c),
                Some(c) => {
                    v.push(c);
                    keep = v.len();
                }
            }
            self.bump();
        }
        v.truncate(keep);
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(section: &str, sub: Option<&str>, key: &str, value: Option<&str>) -> Entry {
        Entry {
            section: section.into(),
            subsection: sub.map(Into::into),
            key: key.into(),
            value: value.map(Into::into),
        }
    }

    #[test]
    fn parses_git_config_syntax() {
        let text = "\u{feff}# comment\n\
            [Core]\n\
            \trepositoryFormatVersion = 0\n\
            \tBare\n\
            ; another\n\
            [remote \"Or\\\"ig\\\\in\"]\n\
            \turl = git@h:o/r.git   # trailing comment\n\
            \tfetch = \"+refs/heads/*:refs/remotes/origin/*\" \n\
            [branch.Main] remote = origin\n\
            [user]\n\
            \tname = \"a ; b\" c\\\n\
            d\n\
            \temail=x\\t@y\r\n";
        let got = parse_config(text).unwrap();
        assert_eq!(
            got,
            [
                entry("core", None, "repositoryformatversion", Some("0")),
                entry("core", None, "bare", None),
                entry("remote", Some("Or\"ig\\in"), "url", Some("git@h:o/r.git")),
                entry("remote", Some("Or\"ig\\in"), "fetch", Some("+refs/heads/*:refs/remotes/origin/*")),
                entry("branch", Some("main"), "remote", Some("origin")),
                entry("user", None, "name", Some("a ; b cd")),
                entry("user", None, "email", Some("x\t@y")),
            ]
        );
    }

    #[test]
    fn unparseable_config_errors() {
        for bad in [
            "bare = true\n",
            "[core\n",
            "[]\n",
            "[remote \"x]\n",
            "[remote x]\n",
            "[core]\n\tname = \"open\n",
            "[core]\n\tname = bad\\q\n",
            "[core]\n\t!key = 1\n",
            "[core]\n\tkey value\n",
        ] {
            assert!(parse_config(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_config("[core]\n\tx = \"a\n").unwrap_err(), "line 2: unterminated quote");
    }

    /// `(config, refused key or None)`.
    #[test]
    fn allowlist_accepts_and_refuses() {
        let cases: &[(&str, Option<&str>)] = &[
            ("[core]\nrepositoryformatversion = 0\nfilemode\nbare = false\nlogallrefupdates = true", None),
            ("[Core]\nIgnoreCase = true\nsymlinks = false\nautocrlf = input\neol = lf", None),
            ("[extensions]\nworktreeConfig = true\nobjectFormat = sha256", None),
            ("[remote \"origin\"]\nurl = git@github.com:o/r.git\nfetch = +refs/heads/*:refs/remotes/origin/*\nprune = true", None),
            ("[branch \"main\"]\nremote = origin\nmerge = refs/heads/main\nvscode-merge-base = origin/main\ngithub-pr-owner-number = o#r#1", None),
            ("[user]\nname = a\nemail = b", None),
            ("[init]\ndefaultBranch = main\n[pull]\nrebase = true\n[push]\nautoSetupRemote = true\n[fetch]\nprune = true", None),
            ("[gc]\nauto = 0\n[gc \"refs/x\"]\nreflogExpire = never", None),
            ("[lfs]\nrepositoryformatversion = 0\n[lfs \"https://h/r.git/info/lfs\"]\naccess = basic", None),
            ("[submodule \"s\"]\nurl = ../s.git\nactive = true\nupdate = rebase", None),
            ("[worktree]\nguessRemote = true\n[rerere]\nenabled = true\n[color]\nui = auto\n[advice]\ndetachedHead = false", None),
            ("[core]\nfsmonitor = /tmp/x", Some("core.fsmonitor")),
            ("[CORE]\nFsMonitor = true", Some("core.fsmonitor")),
            ("[core]\nsshCommand = sh", Some("core.sshcommand")),
            ("[core]\nhooksPath = /tmp", Some("core.hookspath")),
            ("[core]\nalternateRefsCommand = x", Some("core.alternaterefscommand")),
            ("[core \"x\"]\nbare = false", Some("core.x.bare")),
            ("[include]\npath = /tmp/evil", Some("include.path")),
            ("[includeIf \"gitdir:x\"]\npath = /tmp/evil", Some("includeif.gitdir:x.path")),
            ("[filter \"lfs\"]\nprocess = git-lfs filter-process", Some("filter.lfs.process")),
            ("[diff \"x\"]\ntextconv = sh", Some("diff.x.textconv")),
            ("[credential]\nhelper = !sh", Some("credential.helper")),
            ("[url \"ext::sh\"]\ninsteadOf = https://", Some("url.ext::sh.insteadof")),
            ("[remote \"origin\"]\nurl = ext::sh -c x", Some("remote.origin.url to an ext:: URL")),
            ("[remote \"origin\"]\npushurl = EXT::sh", Some("remote.origin.pushurl to an ext:: URL")),
            ("[remote \"origin\"]\nuploadpack = sh", Some("remote.origin.uploadpack")),
            ("[remote]\npushDefault = origin", Some("remote.pushdefault")),
            ("[branch \"main\"]\nsomething = x", Some("branch.main.something")),
            ("[gc]\npreAutoHook = x", Some("gc.preautohook")),
            ("[lfs \"customtransfer.x\"]\npath = sh", Some("lfs.customtransfer.x.path")),
            ("[lfs \"extension.x\"]\nclean = sh", Some("lfs.extension.x.clean")),
            ("[lfs]\nstandalonetransferagent = x", Some("lfs.standalonetransferagent")),
            ("[submodule \"s\"]\nupdate = !sh -c x", Some("submodule.s.update to a `!command`")),
            ("[submodule \"s\"]\nurl = ext::sh", Some("submodule.s.url to an ext:: URL")),
            ("[http]\nproxy = x", Some("http.proxy")),
        ];
        for (text, want) in cases {
            let entries = parse_config(text).unwrap();
            assert_eq!(entries.iter().find_map(refusal).as_deref(), *want, "{text}");
        }
    }

    #[test]
    fn wrapper_disables_hooks_fsmonitor_and_prompts() {
        let cmd = git_command(Path::new("/r"));
        assert_eq!(cmd.get_program(), "git");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-C", "/r"]);
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&("GIT_TERMINAL_PROMPT".as_ref(), Some("0".as_ref()))));
        assert!(envs.contains(&("GIT_DIR".as_ref(), None)));
    }

    fn temp(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("devsandbox-git-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn git_dirs_for_checkout_and_linked_worktree() {
        let root = temp("dirs");
        let (base, wt) = (root.join("base"), root.join("wt"));
        let wtdir = base.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wtdir).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wtdir.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wtdir.display())).unwrap();

        assert_eq!(git_dirs(&base).unwrap(), (base.join(".git"), base.join(".git")));
        assert_eq!(git_dirs(&wt).unwrap(), (wtdir.clone(), wtdir.join("../..")));
        // A relative `gitdir:` resolves against the checkout.
        std::fs::write(wt.join(".git"), "gitdir: ../base/.git/worktrees/wt\n").unwrap();
        assert_eq!(git_dirs(&wt).unwrap().0, wt.join("../base/.git/worktrees/wt"));
        assert!(git_dirs(&root).is_err(), "no .git");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn check_repo_reads_every_config_and_alternates() {
        let root = temp("check");
        let git = |args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(&root).args(args).status().unwrap().success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        check_repo(&root).unwrap();

        let wtconf = root.join(".git/worktrees/x/config.worktree");
        std::fs::create_dir_all(wtconf.parent().unwrap()).unwrap();
        std::fs::write(&wtconf, "[core]\n\tfsmonitor = /tmp/pwn\n").unwrap();
        let err = check_repo(&root).unwrap_err().to_string();
        assert!(err.contains(".git/worktrees/x/config.worktree sets core.fsmonitor"), "{err}");
        std::fs::remove_file(&wtconf).unwrap();

        std::fs::write(root.join(".git/config.worktree"), "[core\n").unwrap();
        let err = check_repo(&root).unwrap_err().to_string();
        assert!(err.contains("cannot parse .git/config.worktree (line 1:"), "{err}");
        std::fs::remove_file(root.join(".git/config.worktree")).unwrap();

        std::fs::write(root.join(".git/objects/info/alternates"), "/elsewhere\n").unwrap();
        let err = check_repo(&root).unwrap_err().to_string();
        assert!(err.contains(".git/objects/info/alternates exists"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Read-only sanity check against this checkout's real config:
    /// `cargo test -- --ignored real_repo_passes`.
    #[test]
    #[ignore]
    fn real_repo_passes() {
        check_repo(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    }
}
