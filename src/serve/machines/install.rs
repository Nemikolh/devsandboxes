//! `machine add`'s install (docs/architecture/daemon/machines.md,
//! *Install*): check which devsandbox a machine runs, decide whether to
//! copy this binary there, and copy it.
//!
//! The CLI does this, not the daemon: the daemon's ssh is `BatchMode` with
//! nobody to answer a prompt or to ask whether to install, and the bytes to
//! copy are the CLI's own binary. Both scripts run over
//! [`ssh::script_argv`], through the login shell, so `PATH` is the user's;
//! their report follows a marker line, since the login shell may print a
//! motd first (as [`ssh::skip_lines`] skips for the link).

use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::{Entry, ssh};

/// The line [`CHECK`]'s report follows.
const CHECK_MARKER: &str = "@devsandbox-check@";
/// The line [`INSTALL`]'s installed path follows.
const INSTALL_MARKER: &str = "@devsandbox-installed@";

/// Where a missing devsandbox goes, as shell text (expanded remote-side).
/// [`INSTALL`] spells it out; `machine add`'s tests pin its `command` to it.
#[cfg_attr(not(test), allow(dead_code))]
pub const DEFAULT_DEST: &str = "$HOME/.local/bin/devsandbox";

/// POSIX sh (Linux and macOS), one line ([`ssh::script_argv`]). `$1`: the
/// program to check, a path or a name looked up on `PATH`; empty is
/// `devsandbox`, then [`DEFAULT_DEST`] when the login `PATH` has none (it
/// often misses `~/.local/bin`: added in `.bashrc`, which a login shell
/// skips). Prints [`CHECK_MARKER`], then `os <uname -s>`, `arch <uname
/// -m>`, and when the program is found `path <path>`, `offpath yes` when
/// that's the fallback and, when its `--version` runs, `version <its
/// output>`.
pub const CHECK: &str = "p=$(command -v \"${1:-devsandbox}\" 2>/dev/null) || p=; \
    o=; if [ -z \"$p\" ] && [ -z \"$1\" ]; then p=$HOME/.local/bin/devsandbox; o=1; fi; \
    echo @devsandbox-check@; \
    printf 'os %s\\n' \"$(uname -s)\"; \
    printf 'arch %s\\n' \"$(uname -m)\"; \
    if [ -n \"$p\" ] && [ -f \"$p\" ] && [ -x \"$p\" ]; then \
    printf 'path %s\\n' \"$p\"; \
    if [ -n \"$o\" ]; then echo 'offpath yes'; fi; \
    v=$(\"$p\" --version </dev/null 2>/dev/null) && printf 'version %s\\n' \"$v\"; \
    fi; \
    exit 0";

/// POSIX sh (Linux and macOS: BSD `mktemp` takes the template too), one
/// line. `$1`: the destination path, empty for [`DEFAULT_DEST`]. Copies
/// stdin into a temp file next to it, checks that `--version` runs there
/// (a binary linked against a libc the machine lacks fails here, not
/// later), then renames it over the destination: atomic, and no "text file
/// busy" over a running binary. Prints [`INSTALL_MARKER`] and the path.
/// The temp file is removed on any failure.
pub const INSTALL: &str = "set -e; \
    d=${1:-$HOME/.local/bin/devsandbox}; \
    if [ -d \"$d\" ]; then printf 'devsandbox: %s is a directory\\n' \"$d\" >&2; exit 1; fi; \
    dir=$(dirname -- \"$d\"); \
    mkdir -p -- \"$dir\"; \
    t=$(mktemp \"$dir/.devsandbox.XXXXXX\"); \
    trap 'rm -f -- \"$t\"' EXIT; \
    trap 'exit 1' HUP INT TERM; \
    cat > \"$t\"; \
    chmod 755 \"$t\"; \
    \"$t\" --version </dev/null >/dev/null || { \
    printf 'devsandbox: the copied binary does not run on this machine (%s %s)\\n' \"$(uname -s)\" \"$(uname -m)\" >&2; \
    exit 1; }; \
    mv -f -- \"$t\" \"$d\"; \
    trap - EXIT; \
    printf '@devsandbox-installed@\\n%s\\n' \"$d\"";

/// A machine's devsandbox, as [`CHECK`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// `uname -s`: `Linux`, `Darwin`.
    pub os: String,
    /// `uname -m`: `x86_64`, `aarch64`, `arm64`.
    pub arch: String,
    /// Where the program resolved; `None`: not found.
    pub path: Option<String>,
    /// `path` is [`DEFAULT_DEST`], found there since the login `PATH` has
    /// no devsandbox: the machine's `command` must name it.
    pub off_path: bool,
    /// Its semver; `None` when `--version` failed (a build from before it
    /// existed) or said something else.
    pub version: Option<String>,
}

/// [`CHECK`]'s stdout: the lines after its marker. Keys after the first of
/// each are ignored, so a multi-line `--version` can't overwrite one.
pub fn parse_check(stdout: &str) -> Result<Remote> {
    let report = after_marker(stdout, CHECK_MARKER)
        .with_context(|| format!("unexpected output from the check: {}", first_line(stdout)))?;
    let (mut os, mut arch, mut path, mut version, mut off_path) = (None, None, None, None, None);
    for line in report {
        let Some((key, value)) = line.split_once(' ') else { continue };
        let slot = match key {
            "os" => &mut os,
            "arch" => &mut arch,
            "path" => &mut path,
            "version" => &mut version,
            "offpath" => &mut off_path,
            _ => continue,
        };
        slot.get_or_insert_with(|| value.trim().to_string());
    }
    let (Some(os), Some(arch)) = (os, arch) else {
        bail!("the check reported no `uname`");
    };
    let version = version.and_then(|v| {
        let semver = v.strip_prefix("devsandbox ")?.trim();
        (!semver.is_empty()).then(|| semver.to_string())
    });
    let path = path.filter(|p| !p.is_empty());
    let off_path = path.is_some() && off_path.is_some();
    Ok(Remote { os, arch, path, off_path, version })
}

/// [`INSTALL`]'s stdout: the installed path.
fn parse_install(stdout: &str) -> Result<String> {
    after_marker(stdout, INSTALL_MARKER)
        .and_then(|mut lines| lines.find(|l| !l.trim().is_empty()))
        .map(|l| l.trim().to_string())
        .with_context(|| format!("unexpected output from the install: {}", first_line(stdout)))
}

/// The lines after the first one that is `marker`.
fn after_marker<'a>(stdout: &'a str, marker: &str) -> Option<impl Iterator<Item = &'a str>> {
    let mut lines = stdout.lines();
    lines.by_ref().find(|l| l.trim_end() == marker)?;
    Some(lines)
}

fn first_line(stdout: &str) -> String {
    let line = stdout.lines().find(|l| !l.trim().is_empty()).unwrap_or("(nothing)");
    line.trim().chars().take(200).collect()
}

/// Whether this binary can run on a machine reporting `uname -s` `os` and
/// `uname -m` `arch`.
pub fn platform_matches(os: &str, arch: &str) -> bool {
    platform_matches_on(os, arch, std::env::consts::OS, std::env::consts::ARCH)
}

/// [`platform_matches`] against a given local `std::env::consts` pair.
fn platform_matches_on(os: &str, arch: &str, local_os: &str, local_arch: &str) -> bool {
    let os = match os {
        "Linux" => "linux",
        "Darwin" => "macos",
        _ => return false,
    };
    let arch = match arch {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return false,
    };
    os == local_os && arch == local_arch
}

/// Where [`INSTALL`] puts the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dest {
    /// The found devsandbox, updated in place.
    Found(String),
    /// [`DEFAULT_DEST`]: none was found.
    Default,
}

impl Dest {
    /// [`install`]'s `dest`.
    pub fn path(&self) -> Option<&str> {
        match self {
            Dest::Found(p) => Some(p),
            Dest::Default => None,
        }
    }
}

/// What [`decide`] says to do about a machine's devsandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Nothing,
    /// `from`: the version being replaced, when known.
    Install { dest: Dest, from: Option<String> },
}

/// Same semver as `ours` → nothing to do; found with another or unknown
/// version → update it in place; missing → [`DEFAULT_DEST`].
pub fn decide(remote: &Remote, ours: &str) -> Plan {
    match &remote.path {
        None => Plan::Install { dest: Dest::Default, from: None },
        Some(_) if remote.version.as_deref() == Some(ours) => Plan::Nothing,
        Some(path) => Plan::Install { dest: Dest::Found(path.clone()), from: remote.version.clone() },
    }
}

/// Run [`CHECK`] on `entry` for `program` (`None`: `devsandbox` on the
/// login `PATH`).
pub fn check(entry: &Entry, program: Option<&str>) -> Result<Remote> {
    let argv = ssh::script_argv(entry, CHECK, &[program.unwrap_or("")])?;
    check_with(&argv).with_context(|| format!("cannot check devsandbox on {}", entry.label()))
}

/// Copy `binary` to `dest` on `entry` (`None`: [`DEFAULT_DEST`]) with
/// [`INSTALL`]; the installed path, expanded.
pub fn install(entry: &Entry, dest: Option<&str>, binary: &Path) -> Result<String> {
    let argv = ssh::script_argv(entry, INSTALL, &[dest.unwrap_or("")])?;
    install_with(&argv, binary).with_context(|| format!("cannot install devsandbox on {}", entry.label()))
}

fn check_with(argv: &[OsString]) -> Result<Remote> {
    parse_check(&run(argv, Stdio::null())?)
}

fn install_with(argv: &[OsString], binary: &Path) -> Result<String> {
    let file = File::open(binary).with_context(|| format!("cannot open {}", binary.display()))?;
    parse_install(&run(argv, file.into())?)
}

/// Run `argv` to its end: its stdout, or an error carrying the tail of its
/// stderr (ssh's `Permission denied (publickey).`, the script's reason).
fn run(argv: &[OsString], stdin: Stdio) -> Result<String> {
    let (program, args) = argv.split_first().context("empty argv")?;
    let out = Command::new(program)
        .args(args)
        .stdin(stdin)
        .output()
        .with_context(|| format!("cannot run {}", program.to_string_lossy()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        let mut cut = stderr.len().saturating_sub(ssh::STDERR_TAIL);
        while !stderr.is_char_boundary(cut) {
            cut += 1;
        }
        match stderr[cut..].trim() {
            "" => bail!("{} exited with {}", program.to_string_lossy(), out.status),
            tail => bail!("{tail}"),
        }
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    fn remote(path: Option<&str>, version: Option<&str>) -> Remote {
        Remote { os: "Linux".into(), arch: "x86_64".into(), path: path.map(Into::into), off_path: false, version: version.map(Into::into) }
    }

    #[test]
    fn check_reports_parse() {
        let found = parse_check("@devsandbox-check@\nos Linux\narch x86_64\npath /usr/bin/devsandbox\nversion devsandbox 0.7.1\n");
        assert_eq!(found.unwrap(), remote(Some("/usr/bin/devsandbox"), Some("0.7.1")));
        // An old build: `--version` failed, or printed something else.
        let old = parse_check("@devsandbox-check@\nos Linux\narch x86_64\npath /usr/bin/devsandbox\n").unwrap();
        assert_eq!(old, remote(Some("/usr/bin/devsandbox"), None));
        let odd = parse_check("@devsandbox-check@\nos Linux\narch x86_64\npath /x\nversion error: unexpected\n").unwrap();
        assert_eq!(odd.version, None);
        let missing = parse_check("@devsandbox-check@\nos Darwin\narch arm64\n").unwrap();
        assert_eq!(missing, Remote { os: "Darwin".into(), arch: "arm64".into(), path: None, off_path: false, version: None });
        let fallback = parse_check("@devsandbox-check@\nos Linux\narch x86_64\npath /h/.local/bin/devsandbox\noffpath yes\n").unwrap();
        assert_eq!(fallback, Remote { off_path: true, ..remote(Some("/h/.local/bin/devsandbox"), None) });
        // A motd before the marker; a multi-line `--version` after.
        let noisy = parse_check(
            "Welcome!\npath /evil\nos Plan9\n@devsandbox-check@\nos Linux\narch x86_64\npath /p\nversion devsandbox 1.0.0\npath /other\n",
        )
        .unwrap();
        assert_eq!(noisy, remote(Some("/p"), Some("1.0.0")));
        for bad in ["", "garbage\n", "os Linux\narch x86_64\n", "@devsandbox-check@\nos Linux\n"] {
            assert!(parse_check(bad).is_err(), "{bad:?} parsed");
        }
        let err = parse_check("sh: 1: syntax error\n").unwrap_err();
        assert!(format!("{err:#}").contains("sh: 1: syntax error"), "{err:#}");
    }

    #[test]
    fn install_reports_parse() {
        assert_eq!(parse_install("motd\n@devsandbox-installed@\n/home/me/.local/bin/devsandbox\n").unwrap(), "/home/me/.local/bin/devsandbox");
        assert!(parse_install("/home/me/.local/bin/devsandbox\n").is_err());
        assert!(parse_install("@devsandbox-installed@\n").is_err());
    }

    #[test]
    fn platforms_map_uname_to_rust() {
        let on = platform_matches_on;
        assert!(on("Linux", "x86_64", "linux", "x86_64"));
        assert!(on("Linux", "amd64", "linux", "x86_64"));
        assert!(on("Linux", "aarch64", "linux", "aarch64"));
        assert!(on("Darwin", "arm64", "macos", "aarch64"));
        assert!(on("Darwin", "x86_64", "macos", "x86_64"));
        assert!(!on("Darwin", "arm64", "linux", "aarch64"));
        assert!(!on("Linux", "x86_64", "linux", "aarch64"));
        assert!(!on("FreeBSD", "x86_64", "freebsd", "x86_64"));
        assert!(!on("Linux", "armv7l", "linux", "arm"));
    }

    #[test]
    fn decisions() {
        assert_eq!(decide(&remote(None, None), "1.0.0"), Plan::Install { dest: Dest::Default, from: None });
        assert_eq!(decide(&remote(Some("/b/devsandbox"), Some("1.0.0")), "1.0.0"), Plan::Nothing);
        assert_eq!(
            decide(&remote(Some("/b/devsandbox"), Some("0.9.0")), "1.0.0"),
            Plan::Install { dest: Dest::Found("/b/devsandbox".into()), from: Some("0.9.0".into()) }
        );
        assert_eq!(
            decide(&remote(Some("/b/devsandbox"), None), "1.0.0"),
            Plan::Install { dest: Dest::Found("/b/devsandbox".into()), from: None }
        );
        assert_eq!(Dest::Found("/b".into()).path(), Some("/b"));
        assert_eq!(Dest::Default.path(), None);
    }

    /// A temp home for the real-shell tests, its `.profile` setting the
    /// login `PATH` (after `/etc/profile`, as a user's would) and printing a
    /// motd, plus a dir with a space and a `'` in it.
    struct Machine {
        base: PathBuf,
        home: PathBuf,
        odd: PathBuf,
    }

    impl Machine {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!("dsv-install-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let (home, odd) = (base.join("home"), base.join("it's here"));
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&odd).unwrap();
            let profile = format!(
                "PATH=\"$HOME/.local/bin:{}:/usr/bin:/bin\"; export PATH; echo 'Welcome to the box!'\n",
                odd.display()
            );
            std::fs::write(home.join(".profile"), profile).unwrap();
            Machine { base, home, odd }
        }

        /// `script` as ssh would run it there: `sh -c` of [`ssh::script_argv`]'s
        /// remote string, under a bare env.
        fn argv(&self, script: &str, arg: &str) -> Vec<OsString> {
            let remote = ssh::remote_command("sh", &["-c", script, "sh", arg]);
            let env = |k: &str, v: &Path| format!("{k}={}", v.display());
            ["env", "-i", env("HOME", &self.home).as_str(), "SHELL=/bin/sh", "PATH=/usr/bin:/bin", "/bin/sh", "-c", remote.as_str()]
                .into_iter()
                .map(OsString::from)
                .collect()
        }

        fn check(&self, program: &str) -> Remote {
            check_with(&self.argv(CHECK, program)).unwrap()
        }

        fn install(&self, dest: &str, binary: &Path) -> Result<String> {
            install_with(&self.argv(INSTALL, dest), binary)
        }

        /// An executable `#!/bin/sh` script at `path`.
        fn script(&self, path: &Path, body: &str) {
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// A devsandbox reporting `9.9.9`.
        fn binary(&self) -> PathBuf {
            let path = self.base.join("new-devsandbox");
            self.script(&path, "case \"$1\" in --version) echo 'devsandbox 9.9.9';; *) exit 2;; esac");
            path
        }
    }

    impl Drop for Machine {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn temp_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".devsandbox."))
            .collect()
    }

    #[test]
    fn missing_is_installed_to_local_bin_then_found() {
        let b = Machine::new("missing");
        let before = b.check("");
        assert_eq!((before.path.as_deref(), before.version.as_deref()), (None, None));
        assert!(platform_matches(&before.os, &before.arch), "{before:?}");
        assert_eq!(decide(&before, "9.9.9"), Plan::Install { dest: Dest::Default, from: None });

        let installed = b.install("", &b.binary()).unwrap();
        let want = b.home.join(".local/bin/devsandbox");
        assert_eq!(installed, want.to_str().unwrap());
        assert_eq!(std::fs::metadata(&want).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(temp_files(want.parent().unwrap()).is_empty());

        let after = b.check("");
        assert_eq!((after.path.as_deref(), after.version.as_deref()), (Some(installed.as_str()), Some("9.9.9")));
        assert!(!after.off_path);
        assert_eq!(decide(&after, "9.9.9"), Plan::Nothing);
    }

    #[test]
    fn local_bin_off_the_login_path_is_found() {
        let b = Machine::new("offpath");
        std::fs::write(b.home.join(".profile"), "PATH=/usr/bin:/bin; export PATH\n").unwrap();
        let installed = b.install("", &b.binary()).unwrap();
        let found = b.check("");
        assert_eq!((found.path.as_deref(), found.off_path), (Some(installed.as_str()), true));
        assert_eq!(found.version.as_deref(), Some("9.9.9"));
        // An explicit program doesn't fall back.
        assert_eq!(b.check("devsandbox").path, None);
    }

    #[test]
    fn found_is_updated_in_place() {
        let b = Machine::new("update");
        // A build from before `--version`.
        let old = b.odd.join("devsandbox");
        b.script(&old, "exit 2");
        let path = old.to_str().unwrap();
        let before = b.check("");
        assert_eq!((before.path.as_deref(), before.version.as_deref()), (Some(path), None));
        assert_eq!(b.check(path), before, "an explicit program checks the same");
        let Plan::Install { dest, from: None } = decide(&before, "9.9.9") else { panic!("{before:?}") };

        assert_eq!(b.install(dest.path().unwrap(), &b.binary()).unwrap(), path);
        assert_eq!(b.check("").version.as_deref(), Some("9.9.9"));
        assert!(temp_files(&b.odd).is_empty());
        assert!(!b.home.join(".local").exists());
    }

    #[test]
    fn a_binary_that_does_not_run_changes_nothing() {
        let b = Machine::new("broken");
        let dest = b.odd.join("devsandbox");
        std::fs::write(&dest, "old").unwrap();
        let broken = b.base.join("broken");
        b.script(&broken, "echo 'not found: libc.so.6' >&2; exit 127");
        let err = b.install(dest.to_str().unwrap(), &broken).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("does not run on this machine") && err.contains("libc.so.6"), "{err}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old");
        assert!(temp_files(&b.odd).is_empty());
        // Nor over a directory.
        let err = b.install(b.odd.to_str().unwrap(), &b.binary()).unwrap_err();
        assert!(format!("{err:#}").contains("is a directory"), "{err:#}");
    }
}
