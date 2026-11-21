//! `devsbd git-check`: whether git in the container still reaches the
//! claims' git hosts through the credential broker after the user's own
//! settings (docs/architecture/credentials/git.md, *The boot check*). Run
//! by the host as the exec user, with the exec env, after the helper's
//! install and again after the create-time lifecycle commands (dotfiles
//! land there), and at every `start`. Prints one warning per line; the host
//! shows them. Never fails: no git, or a folder that isn't a repo, is
//! nothing to check.
//!
//! ```text
//! devsbd git-check --proxy <url> --ca <path> --host <h>... [--no-signing-key] [--] <dir>...
//! ```
//!
//! `--no-signing-key`: the sandbox's `ssh-keys` names no key that signs git,
//! so ssh commit signing is checked too.

use std::process::{Command, Stdio};

const USAGE: &str =
    "usage: devsbd git-check --proxy <url> --ca <path> --host <host>... [--no-signing-key] [--] <dir>...";

/// What git's settings should resolve to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Want {
    pub proxy: String,
    pub ca: String,
    /// Lowercase.
    pub hosts: Vec<String>,
    pub no_signing_key: bool,
    pub dirs: Vec<String>,
}

pub fn run(args: &[String]) -> i32 {
    let want = match parse_args(args) {
        Ok(want) => want,
        Err(e) => {
            eprintln!("devsbd: git-check: {e}\n{USAGE}");
            return 2;
        }
    };
    let env = |k: &str| std::env::var(k).ok();
    for line in check(&want, &env, &mut run_git) {
        println!("{line}");
    }
    0
}

pub fn parse_args(args: &[String]) -> Result<Want, String> {
    let mut want = Want::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().cloned().ok_or_else(|| format!("`{flag}` wants a value"));
        match arg.as_str() {
            "--proxy" => want.proxy = value("--proxy")?,
            "--ca" => want.ca = value("--ca")?,
            "--host" => want.hosts.push(value("--host")?.to_ascii_lowercase()),
            "--no-signing-key" => want.no_signing_key = true,
            "--" => {
                want.dirs.extend(it.by_ref().cloned());
                break;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag `{flag}`")),
            dir => want.dirs.push(dir.to_string()),
        }
    }
    if want.proxy.is_empty() || want.ca.is_empty() || want.hosts.is_empty() {
        return Err("`--proxy`, `--ca` and `--host` are required".into());
    }
    Ok(want)
}

/// git's stdout for `args` in `dir`, `None` when it fails or isn't there.
fn run_git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    String::from_utf8(out.stdout).ok()
}

/// Env variables git reads ahead of every config scope.
const SSL_ENV: [&str; 3] = ["GIT_SSL_CAINFO", "GIT_SSL_CAPATH", "GIT_SSL_NO_VERIFY"];

/// The warnings for `want`, `env` the process env, `git` running git in a
/// directory ([`run_git`]). Each warning once, in the order found.
pub fn check(
    want: &Want,
    env: &dyn Fn(&str) -> Option<String>,
    git: &mut dyn FnMut(&str, &[&str]) -> Option<String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if git("/", &["--version"]).is_none() {
        return out;
    }
    let hosts = want.hosts.join(", ");
    for var in SSL_ENV {
        if env(var).is_some() {
            out.push(ssl_env_warning(var, &hosts));
        }
    }
    for var in ["NO_PROXY", "no_proxy"] {
        let Some(value) = env(var) else { continue };
        for host in want.hosts.iter().filter(|h| crate::proxy::bypass(&value, h, 443)) {
            out.push(format!(
                "`{var}` covers {host}: git dials it directly, past the credential broker, and the host refuses \
                 the phantom git's credential helper sends (\"Invalid username or token\"); drop that entry"
            ));
        }
    }
    for dir in &want.dirs {
        for w in check_dir(want, dir, git) {
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

fn ssl_env_warning(var: &str, hosts: &str) -> String {
    match var {
        "GIT_SSL_NO_VERIFY" => format!(
            "`{var}` is set: git skips TLS verification, {hosts} included, ahead of every git config scope; unset it"
        ),
        _ => format!(
            "`{var}` is set: it replaces, ahead of every git config scope, the CA bundle git's settings for {hosts} \
             name, which carries the member CA, so TLS to {hosts} can fail; unset it"
        ),
    }
}

fn check_dir(want: &Want, dir: &str, git: &mut dyn FnMut(&str, &[&str]) -> Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    if git(dir, &["rev-parse", "--git-dir"]).is_none() {
        return out;
    }
    let remotes = git(dir, &["remote"]).unwrap_or_default();
    let mut checked: Vec<String> = Vec::new();
    for remote in remotes.lines().filter(|r| !r.is_empty()) {
        for (kind, flags) in [("fetch", &["remote", "get-url", "--all"][..]), ("push", &["remote", "get-url", "--push", "--all"][..])] {
            let args: Vec<&str> = flags.iter().copied().chain([remote]).collect();
            for url in git(dir, &args).unwrap_or_default().lines().filter(|u| !u.is_empty()) {
                match on_git_host(url, &want.hosts) {
                    Some(Place::Https(_)) if checked.iter().any(|c| c == url) => {}
                    Some(Place::Https(host)) => {
                        checked.push(url.to_string());
                        out.extend(check_url(want, dir, url, &host, git));
                    }
                    Some(Place::Elsewhere(host)) => out.push(format!(
                        "{dir}: remote `{remote}`'s {kind} URL `{url}` isn't on https://{host}/ after git's url \
                         rewrites: under the git-host claim's key filtering, ssh to {host} fails (`Permission denied \
                         (publickey)`); check the remote's URL and any `url.*.insteadOf` / `pushInsteadOf`"
                    )),
                    None => {}
                }
            }
        }
    }
    if want.no_signing_key
        && git(dir, &["config", "--bool", "--get", "commit.gpgsign"]).is_some_and(|v| v.trim() == "true")
        && git(dir, &["config", "--get", "gpg.format"]).is_some_and(|v| v.trim() == "ssh")
    {
        out.push(
            "`commit.gpgsign` is on with `gpg.format = ssh`, but the sandbox's `ssh-keys` names no key that signs \
             git: under the git-host claim the agent hides every other key, so commits fail to sign; name one \
             `signing-only`"
                .to_string(),
        );
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum Place {
    /// `https://<host>/…` (port 443 or none).
    Https(String),
    /// Any other way to the host: ssh, `git://`, `http://`, another port.
    Elsewhere(String),
}

/// Where remote URL `url` goes, when it names one of `hosts`.
fn on_git_host(url: &str, hosts: &[String]) -> Option<Place> {
    let (scheme, authority) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme.to_ascii_lowercase()), rest.split('/').next().unwrap_or(rest)),
        None => {
            let colon = url.find(':')?;
            if url[..colon].contains('/') {
                return None;
            }
            (None, &url[..colon])
        }
    };
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = match (scheme.is_some(), hostport.rsplit_once(':')) {
        (true, Some((h, p))) => (h, Some(p)),
        _ => (hostport, None),
    };
    let host = host.to_ascii_lowercase();
    if !hosts.contains(&host) {
        return None;
    }
    if scheme.as_deref() == Some("https") && port.is_none_or(|p| p == "443") {
        Some(Place::Https(host))
    } else {
        Some(Place::Elsewhere(host))
    }
}

/// The effective proxy and CA bundle for `url` (`--get-urlmatch` with the
/// real URL: a user's `http.https://<host>/<owner>/.proxy` only shows there).
fn check_url(
    want: &Want,
    dir: &str,
    url: &str,
    host: &str,
    git: &mut dyn FnMut(&str, &[&str]) -> Option<String>,
) -> Vec<String> {
    let matched = git(dir, &["config", "--get-urlmatch", "http", url]).unwrap_or_default();
    let effective = |var: &str| -> Option<String> {
        matched.lines().filter_map(|l| l.split_once(' ')).filter(|(k, _)| *k == var).map(|(_, v)| v.to_string()).next_back()
    };
    let mut out = Vec::new();
    // `--show-scope` prints `unknown` for `--get-urlmatch` on git 2.39, so
    // the origins come from `--show-origin`.
    let mut origins: Option<String> = None;
    for (var, wanted, what) in [
        ("proxy", &want.proxy, "sends git past the credential broker"),
        ("sslcainfo", &want.ca, "lacks the member CA, so TLS to it fails"),
    ] {
        let value = effective(&format!("http.{var}"));
        if value.as_deref() == Some(wanted.as_str()) {
            continue;
        }
        let Some(value) = value else {
            out.push(format!(
                "{dir}: git has no `http.{var}` for `{url}`: devsandbox's git settings for {host} are missing here \
                 (an env that drops `GIT_CONFIG_*`?)"
            ));
            continue;
        };
        let origins = origins.get_or_insert_with(|| {
            git(dir, &["config", "--show-origin", "--get-regexp", r"^http\..+\.(proxy|sslcainfo)$"]).unwrap_or_default()
        });
        let setting = match origin_of(origins, var, &value) {
            Some((file, key)) => format!("`{key}` in `{file}`"),
            None => format!("an `http.<url>.{var}` setting"),
        };
        out.push(format!("{dir}: {setting} (`{value}`) applies to `{url}` and {what}; drop or narrow it"));
    }
    out
}

/// Of `--show-origin --get-regexp` output, the last non-command-line
/// setting of `var` with `value`: `(file, key)`.
fn origin_of(origins: &str, var: &str, value: &str) -> Option<(String, String)> {
    origins
        .lines()
        .filter_map(|l| {
            let (origin, rest) = l.split_once('\t')?;
            let (key, v) = rest.split_once(' ')?;
            let file = origin.strip_prefix("file:")?;
            (key.rsplit_once('.').is_some_and(|(_, k)| k == var) && v == value).then(|| (file.to_string(), key.to_string()))
        })
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn want() -> Want {
        Want {
            proxy: "http://127.0.0.1:29001".into(),
            ca: "/etc/ssl/certs/ca-certificates.crt".into(),
            hosts: vec!["github.com".into()],
            no_signing_key: false,
            dirs: vec!["/w".into()],
        }
    }

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    /// A fake git: `answers` keyed on `dir` + args joined by spaces.
    fn fake(answers: &[(&str, &str)]) -> impl FnMut(&str, &[&str]) -> Option<String> {
        let map: BTreeMap<String, String> = answers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |dir, args| map.get(&format!("{dir} {}", args.join(" "))).cloned()
    }

    const OURS: &str = "http.proxy http://127.0.0.1:29001\nhttp.sslcainfo /etc/ssl/certs/ca-certificates.crt\n";

    #[test]
    fn args_parse() {
        let w = parse_args(&args(&[
            "--proxy", "http://127.0.0.1:29001", "--ca", "/b", "--host", "GitHub.com", "--no-signing-key", "/a", "--", "--x",
        ]))
        .unwrap();
        assert_eq!(w.hosts, ["github.com"]);
        assert!(w.no_signing_key);
        assert_eq!(w.dirs, ["/a", "--x"]);
        assert!(parse_args(&args(&["--proxy", "p", "--ca", "c"])).unwrap_err().contains("required"));
        assert!(parse_args(&args(&["--proxy"])).unwrap_err().contains("wants a value"));
        assert!(parse_args(&args(&["--bogus"])).unwrap_err().contains("unknown flag"));
    }

    #[test]
    fn urls_place_on_or_off_the_https_host() {
        let hosts = vec!["github.com".to_string()];
        let on = |u: &str| on_git_host(u, &hosts);
        assert_eq!(on("https://github.com/o/r"), Some(Place::Https("github.com".into())));
        assert_eq!(on("https://x@GitHub.com:443/o/r"), Some(Place::Https("github.com".into())));
        for off in ["git@github.com:o/r", "ssh://git@github.com/o/r", "http://github.com/o/r", "https://github.com:8443/o/r", "git://github.com/o/r"] {
            assert_eq!(on(off), Some(Place::Elsewhere("github.com".into())), "{off}");
        }
        for other in ["https://gitlab.com/o/r", "git@github-work:o/r", "/srv/repo", "./a:b", "https://api.github.com/x"] {
            assert_eq!(on(other), None, "{other}");
        }
    }

    #[test]
    fn a_clean_repo_warns_of_nothing_and_no_git_checks_nothing() {
        let mut git = fake(&[
            ("/ --version", "git version 2.39.5\n"),
            ("/w rev-parse --git-dir", ".git\n"),
            ("/w remote", "origin\nup\n"),
            ("/w remote get-url --all origin", "https://github.com/o/r\n"),
            ("/w remote get-url --push --all origin", "https://github.com/o/r\n"),
            ("/w remote get-url --all up", "https://gitlab.com/x/y\n"),
            ("/w remote get-url --push --all up", "https://gitlab.com/x/y\n"),
            ("/w config --get-urlmatch http https://github.com/o/r", OURS),
        ]);
        assert!(check(&want(), &|_| None, &mut git).is_empty());
        // A folder that isn't a repo.
        let mut w = want();
        w.dirs = vec!["/plain".into()];
        assert!(check(&w, &|_| None, &mut git).is_empty());
        let env = |k: &str| (k == "GIT_SSL_NO_VERIFY").then(|| "1".to_string());
        assert!(check(&want(), &env, &mut fake(&[])).is_empty(), "no git");
    }

    #[test]
    fn warns_of_a_winning_user_setting_naming_its_file() {
        let mut git = fake(&[
            ("/ --version", "git version 2.39.5\n"),
            ("/w rev-parse --git-dir", ".git\n"),
            ("/w remote", "origin\n"),
            ("/w remote get-url --all origin", "https://github.com/Org/r.git\n"),
            ("/w remote get-url --push --all origin", "git@github.com:Org/r.git\n"),
            (
                "/w config --get-urlmatch http https://github.com/Org/r.git",
                "http.proxy http://corp:3128\nhttp.sslcainfo /home/u/corp.pem\n",
            ),
            (
                r"/w config --show-origin --get-regexp ^http\..+\.(proxy|sslcainfo)$",
                "file:/home/u/.gitconfig\thttp.https://github.com/Org/.proxy http://corp:3128\n\
                 command line:\thttp.https://github.com/.proxy http://127.0.0.1:29001\n\
                 command line:\thttp.https://github.com/.sslcainfo /etc/ssl/certs/ca-certificates.crt\n",
            ),
            ("/w config --bool --get commit.gpgsign", "true\n"),
            ("/w config --get gpg.format", "ssh\n"),
        ]);
        let mut w = want();
        w.no_signing_key = true;
        let got = check(&w, &|_| None, &mut git);
        assert_eq!(got.len(), 4, "{got:#?}");
        assert_eq!(
            got[0],
            "/w: `http.https://github.com/Org/.proxy` in `/home/u/.gitconfig` (`http://corp:3128`) applies to \
             `https://github.com/Org/r.git` and sends git past the credential broker; drop or narrow it"
        );
        assert!(got[1].starts_with("/w: an `http.<url>.sslcainfo` setting (`/home/u/corp.pem`)"), "{}", got[1]);
        assert!(got[1].contains("lacks the member CA"), "{}", got[1]);
        assert!(got[2].starts_with("/w: remote `origin`'s push URL `git@github.com:Org/r.git` isn't on https://github.com/"), "{}", got[2]);
        assert!(got[2].contains("`Permission denied (publickey)`"), "{}", got[2]);
        assert!(got[3].starts_with("`commit.gpgsign` is on with `gpg.format = ssh`"), "{}", got[3]);
        let got = check(&want(), &|_| None, &mut git);
        assert_eq!(got.len(), 3, "{got:#?}");
    }

    #[test]
    fn missing_settings_and_the_env_warn() {
        let mut git = fake(&[
            ("/ --version", "git version 2.39.5\n"),
            ("/w rev-parse --git-dir", ".git\n"),
            ("/w remote", "origin\n"),
            ("/w remote get-url --all origin", "https://github.com/o/r\n"),
            ("/w remote get-url --push --all origin", "https://github.com/o/r\n"),
            ("/w config --get-urlmatch http https://github.com/o/r", ""),
        ]);
        let env = |k: &str| match k {
            "GIT_SSL_NO_VERIFY" => Some("1".to_string()),
            "GIT_SSL_CAINFO" => Some("/x.pem".to_string()),
            "no_proxy" => Some("localhost,.github.com".to_string()),
            "NO_PROXY" => Some("localhost,api.github.com".to_string()),
            _ => None,
        };
        let got = check(&want(), &env, &mut git);
        assert_eq!(got.len(), 5, "{got:#?}");
        assert!(got[0].starts_with("`GIT_SSL_CAINFO` is set: it replaces"), "{}", got[0]);
        assert!(got[1].starts_with("`GIT_SSL_NO_VERIFY` is set: git skips TLS verification"), "{}", got[1]);
        assert!(got[2].starts_with("`no_proxy` covers github.com: git dials it directly"), "{}", got[2]);
        assert!(got[3].starts_with("/w: git has no `http.proxy` for `https://github.com/o/r`"), "{}", got[3]);
        assert!(got[4].starts_with("/w: git has no `http.sslcainfo`"), "{}", got[4]);
        let star = |k: &str| (k == "NO_PROXY").then(|| "*".to_string());
        let got = check(&want(), &star, &mut git);
        assert!(got[0].starts_with("`NO_PROXY` covers github.com"), "{got:#?}");
    }

    #[test]
    fn origins_parse() {
        let out = "file:/etc/gitconfig\thttp.https://github.com/.proxy http://a\n\
                   file:/home/u/.gitconfig\thttp.https://github.com/o/.proxy http://a\n\
                   command line:\thttp.https://github.com/.proxy http://a\n\
                   file:/x\thttp.https://github.com/.sslcainfo http://a\n";
        assert_eq!(
            origin_of(out, "proxy", "http://a"),
            Some(("/home/u/.gitconfig".into(), "http.https://github.com/o/.proxy".into()))
        );
        assert_eq!(origin_of(out, "proxy", "http://b"), None);
    }
}
