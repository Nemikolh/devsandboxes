//! `devsandbox serve install|uninstall`: boot start through the user's
//! service manager (docs/serve.md, *Boot start*): a systemd user unit on
//! Linux, a LaunchAgent on macOS, running `devsandbox serve --keep-alive`.
//!
//! The unit text is rendered by pure functions ([`render_systemd`],
//! [`render_launchd`]); [`install`] / [`uninstall`] take every path, the uid
//! and the command runner as a [`Setup`], so tests never touch the real
//! service manager or `~/.config/systemd` / `~/Library/LaunchAgents`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::client;
use super::endpoint;
use super::proto::Version;

/// The systemd user unit's file name.
pub const SYSTEMD_UNIT: &str = "devsandbox.service";
/// The LaunchAgent's label; its plist is `<label>.plist`. The repo had no
/// reverse-DNS id to reuse.
pub const LAUNCHD_LABEL: &str = "dev.devsandbox.serve";

/// Environment the unit carries, captured at install time when set and
/// non-empty. systemd and launchd start services with a minimal `PATH`, so
/// without it the runtime CLIs (docker, podman, OrbStack's) wouldn't
/// resolve. The XDG data/state dirs keep the managed daemon on the same
/// `state.toml`, `inbox.toml` and `serve.log` as the shell's clients.
/// Not `SSH_AUTH_SOCK`: it rotates, and clients report theirs through
/// `bridges.ensure`.
const CAPTURED_ENV: &[&str] = &[
    "PATH",
    crate::runtime::RUNTIME_ENV,
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "CONTAINER_HOST",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
];

/// How long install waits for a running daemon to stop after `shutdown`.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// How long install waits for the manager's daemon to answer.
const START_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);
/// `launchctl bootstrap` right after a `bootout` can fail while launchd
/// still tears the old job down; it's retried this many times.
const BOOTSTRAP_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    pub fn current() -> Result<Self> {
        if cfg!(target_os = "linux") {
            Ok(Self::Systemd)
        } else if cfg!(target_os = "macos") {
            Ok(Self::Launchd)
        } else {
            bail!("`devsandbox serve install` supports Linux (systemd user units) and macOS (LaunchAgents) only")
        }
    }

    fn file_name(self) -> String {
        match self {
            Self::Systemd => SYSTEMD_UNIT.to_string(),
            Self::Launchd => format!("{LAUNCHD_LABEL}.plist"),
        }
    }
}

/// Where the unit file goes: `$XDG_CONFIG_HOME/systemd/user` (absolute values
/// only) else `~/.config/systemd/user`; `~/Library/LaunchAgents`.
pub fn unit_dir_from(manager: Manager, config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let home = home.map(PathBuf::from).filter(|h| h.is_absolute());
    match manager {
        Manager::Systemd => {
            let config = config_home.map(PathBuf::from).filter(|c| c.is_absolute()).or_else(|| Some(home?.join(".config")))?;
            Some(config.join("systemd/user"))
        }
        Manager::Launchd => Some(home?.join("Library/LaunchAgents")),
    }
}

/// What the unit runs: `exe serve --keep-alive --socket-dir <socket_dir>`,
/// with `env`, output appended to `log`. UTF-8 only: unit files are text.
#[derive(Debug, Clone)]
pub struct Spec {
    pub exe: String,
    pub socket_dir: String,
    pub log: String,
    pub env: Vec<(String, String)>,
}

impl Spec {
    /// The real values: this binary (canonicalized), the socket dir and log
    /// this environment resolves, the [`CAPTURED_ENV`] set.
    pub fn current() -> Result<Self> {
        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("cannot locate the devsandbox binary")?;
        Ok(Self {
            exe: utf8(exe, "the devsandbox binary path")?,
            socket_dir: utf8(endpoint::socket_dir()?, "the socket dir")?,
            log: utf8(endpoint::log_path()?, "the serve.log path")?,
            env: capture(|k| std::env::var(k).ok()),
        })
    }

    fn argv(&self) -> [&str; 5] {
        [&self.exe, "serve", "--keep-alive", "--socket-dir", &self.socket_dir]
    }
}

fn utf8(path: PathBuf, what: &str) -> Result<String> {
    path.into_string().map_err(|p| anyhow::anyhow!("{what} is not UTF-8: {}", p.to_string_lossy()))
}

/// [`CAPTURED_ENV`] through `get`, in that order, skipping unset and empty.
pub fn capture(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    CAPTURED_ENV
        .iter()
        .filter_map(|&k| get(k).filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
        .collect()
}

/// The systemd user unit. `Restart=on-failure`, not `always`: a version
/// handoff exits 0, and the manager must not respawn the (now stale) binary
/// in a loop against the newer daemon.
pub fn render_systemd(spec: &Spec) -> String {
    let exec: Vec<String> = spec.argv().iter().map(|w| systemd_word(w)).collect();
    let mut out = String::from(
        "# Written by `devsandbox serve install`; run it again to refresh,\n\
         # `devsandbox serve uninstall` to remove.\n\
         [Unit]\n\
         Description=devsandbox host daemon\n\
         \n\
         [Service]\n\
         Type=simple\n",
    );
    out += &format!("ExecStart={}\n", exec.join(" "));
    for (k, v) in &spec.env {
        out += &format!("Environment=\"{}\"\n", systemd_quoted(&format!("{k}={v}")));
    }
    let log = spec.log.replace('%', "%%");
    out += &format!(
        "Restart=on-failure\n\
         RestartSec=5\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    );
    out
}

/// Inside `"…"` in a unit file: C-style escapes, `%` specifiers doubled.
fn systemd_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out += "\\\\",
            '"' => out += "\\\"",
            '\n' => out += "\\n",
            '%' => out += "%%",
            c => out.push(c),
        }
    }
    out
}

/// One `ExecStart=` word: `%` and `$` doubled (specifiers and variable
/// expansion), quoted when it holds anything but plain path characters.
fn systemd_word(w: &str) -> String {
    let plain = !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+:=,@%$".contains(c));
    if plain {
        w.replace('%', "%%").replace('$', "$$")
    } else {
        format!("\"{}\"", systemd_quoted(w).replace('$', "$$"))
    }
}

/// The LaunchAgent plist. `KeepAlive {SuccessfulExit false}` restarts it on
/// a crash but not after a handoff's clean exit, like systemd's
/// `Restart=on-failure`.
pub fn render_launchd(spec: &Spec) -> String {
    let s = |v: &str| format!("<string>{}</string>", xml(v));
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by `devsandbox serve install`; run it again to refresh, `devsandbox serve uninstall` to remove. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n",
    );
    out += &format!("\t<key>Label</key>\n\t{}\n", s(LAUNCHD_LABEL));
    out += "\t<key>ProgramArguments</key>\n\t<array>\n";
    for w in spec.argv() {
        out += &format!("\t\t{}\n", s(w));
    }
    out += "\t</array>\n";
    if !spec.env.is_empty() {
        out += "\t<key>EnvironmentVariables</key>\n\t<dict>\n";
        for (k, v) in &spec.env {
            out += &format!("\t\t<key>{}</key>\n\t\t{}\n", xml(k), s(v));
        }
        out += "\t</dict>\n";
    }
    out += "\t<key>RunAtLoad</key>\n\t<true/>\n";
    out += "\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n";
    out += &format!("\t<key>StandardOutPath</key>\n\t{}\n", s(&spec.log));
    out += &format!("\t<key>StandardErrorPath</key>\n\t{}\n", s(&spec.log));
    out += "</dict>\n</plist>\n";
    out
}

fn xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out += "&amp;",
            '<' => out += "&lt;",
            '>' => out += "&gt;",
            '"' => out += "&quot;",
            '\'' => out += "&apos;",
            c => out.push(c),
        }
    }
    out
}

/// Runs a manager command (`systemctl`, `launchctl`) to completion.
pub type Runner<'a> = &'a dyn Fn(&str, &[&str]) -> Result<Output>;

/// Everything [`install`] / [`uninstall`] touch besides the socket.
pub struct Setup<'a> {
    pub manager: Manager,
    /// The unit file's dir (see [`unit_dir_from`]).
    pub unit_dir: PathBuf,
    /// For launchd's `gui/<uid>` domain.
    pub uid: u32,
    /// The hello's version when probing the daemon.
    pub version: Version,
    pub run: Runner<'a>,
    pub stop_wait: Duration,
    pub start_wait: Duration,
    /// Between `launchctl bootstrap` attempts.
    pub retry_gap: Duration,
}

impl<'a> Setup<'a> {
    /// The real values for this user and platform.
    pub fn current(run: Runner<'a>) -> Result<Self> {
        let manager = Manager::current()?;
        let var = std::env::var_os;
        let unit_dir = unit_dir_from(manager, var("XDG_CONFIG_HOME").as_deref(), var("HOME").as_deref())
            .context("cannot determine where the unit file goes ($HOME is unset or relative)")?;
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        Ok(Self {
            manager,
            unit_dir,
            uid,
            version: Version::current(),
            run,
            stop_wait: STOP_WAIT,
            start_wait: START_WAIT,
            retry_gap: Duration::from_secs(1),
        })
    }

    pub fn unit_path(&self) -> PathBuf {
        self.unit_dir.join(self.manager.file_name())
    }

    fn launchd_target(&self) -> String {
        format!("gui/{}/{LAUNCHD_LABEL}", self.uid)
    }

    /// Run `program args`; a non-zero exit is an error with its stderr.
    fn check(&self, program: &str, args: &[&str]) -> Result<()> {
        let out = (self.run)(program, args)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("`{program} {}` failed ({}): {}", args.join(" "), out.status, stderr.trim());
        }
        Ok(())
    }
}

/// `devsandbox serve install` with the real environment.
pub fn install_cli() -> Result<()> {
    let setup = Setup::current(&run_command)?;
    println!("{}", install(&setup, &Spec::current()?)?);
    Ok(())
}

/// `devsandbox serve uninstall` with the real environment.
pub fn uninstall_cli() -> Result<()> {
    let setup = Setup::current(&run_command)?;
    println!("{}", uninstall(&setup)?);
    Ok(())
}

fn run_command(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run `{program}`"))
}

/// Write the unit, stop a daemon already answering (`shutdown`, so the
/// manager's own can bind), then have the manager (re)start it and check it
/// answers. Idempotent: running it again rewrites the unit and restarts.
/// Returns the line to print.
pub fn install(s: &Setup, spec: &Spec) -> Result<String> {
    let unit = s.unit_path();
    std::fs::create_dir_all(&s.unit_dir).with_context(|| format!("cannot create {}", s.unit_dir.display()))?;
    // The manager appends to serve.log; its dir must exist.
    if let Some(parent) = Path::new(&spec.log).parent() {
        endpoint::ensure_private_dir(parent)?;
    }
    let text = match s.manager {
        Manager::Systemd => render_systemd(spec),
        Manager::Launchd => render_launchd(spec),
    };
    std::fs::write(&unit, text).with_context(|| format!("cannot write {}", unit.display()))?;

    let dir = Path::new(&spec.socket_dir);
    let started = stop_running(dir, &s.version, s.stop_wait).and_then(|()| start(s, &unit));
    started.with_context(|| format!("wrote {}, but couldn't start it", unit.display()))?;

    let socket = endpoint::socket_path(dir);
    if !wait_answering(dir, &s.version, s.start_wait) {
        let log = &spec.log;
        bail!(
            "installed {}, but devsandbox serve didn't answer on {} within {:?}; see {log}",
            unit.display(),
            socket.display(),
            s.start_wait
        );
    }
    Ok(format!("installed {}; devsandbox serve is running (keep-alive) on {}", unit.display(), socket.display()))
}

/// Have the manager load the unit and (re)start its daemon.
fn start(s: &Setup, unit: &Path) -> Result<()> {
    match s.manager {
        Manager::Systemd => {
            s.check("systemctl", &["--user", "daemon-reload"])?;
            s.check("systemctl", &["--user", "enable", SYSTEMD_UNIT])?;
            // `restart`, not `enable --now`: a managed daemon still draining
            // after the shutdown counts as active, and `start` would no-op.
            s.check("systemctl", &["--user", "restart", SYSTEMD_UNIT])
        }
        Manager::Launchd => {
            let target = s.launchd_target();
            // Not loaded is fine (a first install).
            let _ = (s.run)("launchctl", &["bootout", &target]);
            let domain = format!("gui/{}", s.uid);
            let plist = unit.to_str().context("the plist path is not UTF-8")?;
            let mut attempt = 1;
            loop {
                match s.check("launchctl", &["bootstrap", &domain, plist]) {
                    Ok(()) => return Ok(()),
                    Err(e) if attempt >= BOOTSTRAP_ATTEMPTS => return Err(e),
                    Err(_) => {
                        attempt += 1;
                        std::thread::sleep(s.retry_gap);
                    }
                }
            }
        }
    }
}

/// Send `shutdown` to the daemon answering in `dir`, if one does, and wait
/// up to `wait` until nothing answers.
fn stop_running(dir: &Path, version: &Version, wait: Duration) -> Result<()> {
    if let Some(mut conn) = client::connect_running(dir, "cli", version, wait)? {
        let r = conn.call("shutdown", json!({}))?;
        if let Some(e) = r.error {
            bail!(
                "the running devsandbox serve refused `shutdown`: {} ({}); stop it, then run `devsandbox serve install` again",
                e.message,
                e.code
            );
        }
    }
    let deadline = Instant::now() + wait;
    while endpoint::connect(dir).is_ok() {
        if Instant::now() >= deadline {
            bail!("devsandbox serve still answers on {} after {wait:?}", endpoint::socket_path(dir).display());
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

/// A daemon answers a hello in `dir` within `wait`.
fn wait_answering(dir: &Path, version: &Version, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).max(POLL);
        if let Ok(Some(_)) = client::connect_running(dir, "cli", version, left) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Stop the managed daemon and remove the unit. A daemon a client started
/// on demand isn't the manager's and is left alone. Returns the line to print.
pub fn uninstall(s: &Setup) -> Result<String> {
    let unit = s.unit_path();
    if !unit.exists() {
        return Ok(format!("devsandbox serve is not installed (no {})", unit.display()));
    }
    let warn = |program: &str, args: &[&str]| {
        if let Err(e) = s.check(program, args) {
            eprintln!("warning: {e:#}");
        }
    };
    match s.manager {
        Manager::Systemd => {
            // A failure (no user manager, unit not loaded) still removes the file.
            warn("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
            std::fs::remove_file(&unit).with_context(|| format!("cannot remove {}", unit.display()))?;
            warn("systemctl", &["--user", "daemon-reload"]);
        }
        Manager::Launchd => {
            // Not loaded is fine.
            let _ = (s.run)("launchctl", &["bootout", &s.launchd_target()]);
            std::fs::remove_file(&unit).with_context(|| format!("cannot remove {}", unit.display()))?;
        }
    }
    Ok(format!(
        "removed {}; the managed daemon is stopped (commands start one on demand again)",
        unit.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::daemon::{Exit, Options};

    fn spec() -> Spec {
        Spec {
            exe: "/opt/dev sandbox/bin/devsandbox".into(),
            socket_dir: "/run/user/1000/devsandbox".into(),
            log: "/home/u/.local/state/devsandbox/serve.log".into(),
            env: vec![
                ("PATH".into(), "/usr/bin:/opt/my bin".into()),
                ("DOCKER_HOST".into(), r#"unix:///tmp/100%/"a&b"<c>\d$e"#.into()),
            ],
        }
    }

    #[test]
    fn systemd_unit_escapes_quotes_percent_and_spaces() {
        let unit = render_systemd(&spec());
        assert_eq!(
            unit,
            r#"# Written by `devsandbox serve install`; run it again to refresh,
# `devsandbox serve uninstall` to remove.
[Unit]
Description=devsandbox host daemon

[Service]
Type=simple
ExecStart="/opt/dev sandbox/bin/devsandbox" serve --keep-alive --socket-dir /run/user/1000/devsandbox
Environment="PATH=/usr/bin:/opt/my bin"
Environment="DOCKER_HOST=unix:///tmp/100%%/\"a&b\"<c>\\d$e"
Restart=on-failure
RestartSec=5
StandardOutput=append:/home/u/.local/state/devsandbox/serve.log
StandardError=append:/home/u/.local/state/devsandbox/serve.log

[Install]
WantedBy=default.target
"#
        );
    }

    #[test]
    fn systemd_exec_words_escape_specifiers_and_expansion() {
        assert_eq!(systemd_word("/usr/bin/devsandbox"), "/usr/bin/devsandbox");
        assert_eq!(systemd_word("/a/100%/$HOME"), "/a/100%%/$$HOME");
        assert_eq!(systemd_word(r#"/a b/"q"\$x%"#), r#""/a b/\"q\"\\$$x%%""#);
        assert_eq!(systemd_word(""), r#""""#);
        let log = Spec { log: "/st/100%/serve.log".into(), ..spec() };
        assert!(render_systemd(&log).contains("StandardOutput=append:/st/100%%/serve.log\n"));
    }

    #[test]
    fn launchd_plist_escapes_xml() {
        let plist = render_launchd(&spec());
        assert_eq!(
            plist,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `devsandbox serve install`; run it again to refresh, `devsandbox serve uninstall` to remove. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>dev.devsandbox.serve</string>
	<key>ProgramArguments</key>
	<array>
		<string>/opt/dev sandbox/bin/devsandbox</string>
		<string>serve</string>
		<string>--keep-alive</string>
		<string>--socket-dir</string>
		<string>/run/user/1000/devsandbox</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/usr/bin:/opt/my bin</string>
		<key>DOCKER_HOST</key>
		<string>unix:///tmp/100%/&quot;a&amp;b&quot;&lt;c&gt;\d$e</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>StandardOutPath</key>
	<string>/home/u/.local/state/devsandbox/serve.log</string>
	<key>StandardErrorPath</key>
	<string>/home/u/.local/state/devsandbox/serve.log</string>
</dict>
</plist>
"#
        );
        let bare = render_launchd(&Spec { env: vec![], ..spec() });
        assert!(!bare.contains("EnvironmentVariables"));
    }

    #[test]
    fn capture_takes_the_set_vars_in_order_and_never_the_agent() {
        let env = |k: &str| match k {
            "PATH" => Some("/usr/bin".to_string()),
            "DOCKER_HOST" => Some(String::new()),
            "DOCKER_CONTEXT" => Some("orbstack".to_string()),
            "DEVSANDBOX_RUNTIME" => Some("podman".to_string()),
            "SSH_AUTH_SOCK" => Some("/tmp/agent".to_string()),
            _ => None,
        };
        let got = capture(env);
        let keys: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["PATH", "DEVSANDBOX_RUNTIME", "DOCKER_CONTEXT"]);
    }

    #[test]
    fn unit_dirs_respect_xdg_config_home() {
        let os = |s: &'static str| Some(OsStr::new(s));
        let p = |m, c, h| unit_dir_from(m, c, h).map(|p| p.display().to_string());
        assert_eq!(p(Manager::Systemd, os("/cfg"), os("/home/u")).as_deref(), Some("/cfg/systemd/user"));
        assert_eq!(p(Manager::Systemd, os("rel"), os("/home/u")).as_deref(), Some("/home/u/.config/systemd/user"));
        assert_eq!(p(Manager::Systemd, None, None), None);
        assert_eq!(p(Manager::Launchd, os("/cfg"), os("/home/u")).as_deref(), Some("/home/u/Library/LaunchAgents"));
        assert_eq!(p(Manager::Launchd, None, os("")), None);
    }

    fn output(code: i32, stderr: &str) -> Output {
        Output { status: ExitStatus::from_raw(code << 8), stdout: vec![], stderr: stderr.as_bytes().to_vec() }
    }

    /// A fake service manager: records every call; `respond` decides the
    /// outcome (and may start a daemon, as the real manager would).
    struct Fake {
        calls: Mutex<Vec<String>>,
        respond: Box<dyn Fn(&str, usize) -> Output + Send + Sync>,
    }

    impl Fake {
        fn new(respond: impl Fn(&str, usize) -> Output + Send + Sync + 'static) -> Self {
            Self { calls: Mutex::new(Vec::new()), respond: Box::new(respond) }
        }

        fn run(&self, program: &str, args: &[&str]) -> Result<Output> {
            let call = format!("{program} {}", args.join(" "));
            let mut calls = self.calls.lock().unwrap();
            calls.push(call.clone());
            let n = calls.iter().filter(|c| **c == call).count();
            drop(calls);
            Ok((self.respond)(&call, n))
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn setup<'a>(manager: Manager, unit_dir: &Path, run: Runner<'a>) -> Setup<'a> {
        Setup {
            manager,
            unit_dir: unit_dir.to_owned(),
            uid: 501,
            version: Version { semver: "0.6.0".into(), build: 1 },
            run,
            stop_wait: Duration::from_secs(10),
            start_wait: Duration::from_secs(5),
            retry_gap: Duration::ZERO,
        }
    }

    fn spec_at(socket_dir: &Path, root: &Path) -> Spec {
        Spec {
            exe: "/usr/bin/devsandbox".into(),
            socket_dir: socket_dir.to_str().unwrap().into(),
            log: root.join("state/serve.log").to_str().unwrap().into(),
            env: vec![("PATH".into(), "/usr/bin".into())],
        }
    }

    fn keep_alive() -> Options {
        Options { keep_alive: true, ..opts(1, 50) }
    }

    type Daemon = Mutex<Option<std::thread::JoinHandle<Result<Exit>>>>;

    /// Stop the daemon in `dir` the way install does, and join it.
    fn stop(dir: &Path, daemon: &Daemon) -> Exit {
        stop_running(dir, &Version { semver: "0.6.0".into(), build: 1 }, Duration::from_secs(10)).unwrap();
        daemon.lock().unwrap().take().expect("no daemon started").join().unwrap().unwrap()
    }

    #[test]
    fn systemd_install_shuts_the_running_daemon_down_then_restarts_the_unit() {
        let root = scratch("inst-sd");
        let sock = scratch("inst-sd-sock");
        let units = root.join("systemd/user");
        let lazy = spawn_daemon(&sock, keep_alive());
        assert!(wait_answering(&sock, &Version { semver: "0.6.0".into(), build: 1 }, Duration::from_secs(5)));

        let managed: Arc<Daemon> = Arc::new(Mutex::new(None));
        let fake = {
            let (managed, sock) = (Arc::clone(&managed), sock.clone());
            Fake::new(move |call, _| {
                if call == "systemctl --user restart devsandbox.service" {
                    *managed.lock().unwrap() = Some(spawn_daemon(&sock, keep_alive()));
                }
                output(0, "")
            })
        };
        let run = |p: &str, a: &[&str]| fake.run(p, a);
        let s = setup(Manager::Systemd, &units, &run);
        let spec = spec_at(&sock, &root);

        let line = install(&s, &spec).unwrap();
        assert_eq!(lazy.join().unwrap().unwrap(), Exit::Shutdown);
        let unit = units.join("devsandbox.service");
        assert_eq!(std::fs::read_to_string(&unit).unwrap(), render_systemd(&spec));
        assert!(root.join("state").is_dir());
        assert_eq!(
            fake.calls(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable devsandbox.service",
                "systemctl --user restart devsandbox.service",
            ]
        );
        assert!(line.starts_with(&format!("installed {}; devsandbox serve is running", unit.display())), "{line}");

        // Again: rewrites the unit and restarts, stopping the managed daemon first.
        let first = managed.lock().unwrap().take().unwrap();
        install(&s, &spec).unwrap();
        assert_eq!(first.join().unwrap().unwrap(), Exit::Shutdown);
        assert_eq!(fake.calls().len(), 6);

        assert_eq!(stop(&sock, &managed), Exit::Shutdown);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&sock);
    }

    #[test]
    fn launchd_install_boots_out_then_bootstraps_with_retries() {
        let root = scratch("inst-ld");
        let sock = scratch("inst-ld-sock");
        let agents = root.join("Library/LaunchAgents");
        let managed: Arc<Daemon> = Arc::new(Mutex::new(None));
        let fake = {
            let (managed, sock) = (Arc::clone(&managed), sock.clone());
            Fake::new(move |call, n| {
                if call.starts_with("launchctl bootout") {
                    return output(3, "Boot-out failed: 3: No such process");
                }
                if n == 1 {
                    return output(5, "Bootstrap failed: 5: Input/output error");
                }
                *managed.lock().unwrap() = Some(spawn_daemon(&sock, keep_alive()));
                output(0, "")
            })
        };
        let run = |p: &str, a: &[&str]| fake.run(p, a);
        let s = setup(Manager::Launchd, &agents, &run);
        let spec = spec_at(&sock, &root);

        install(&s, &spec).unwrap();
        let plist = agents.join("dev.devsandbox.serve.plist");
        assert_eq!(std::fs::read_to_string(&plist).unwrap(), render_launchd(&spec));
        let bootstrap = format!("launchctl bootstrap gui/501 {}", plist.display());
        assert_eq!(fake.calls(), ["launchctl bootout gui/501/dev.devsandbox.serve".to_string(), bootstrap.clone(), bootstrap]);

        assert_eq!(stop(&sock, &managed), Exit::Shutdown);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&sock);
    }

    #[test]
    fn install_reports_a_failing_manager_and_a_daemon_that_never_answers() {
        let root = scratch("inst-fail");
        let sock = scratch("inst-fail-sock");
        let units = root.join("units");
        let spec = spec_at(&sock, &root);

        let fake = Fake::new(|call, _| match call {
            "systemctl --user daemon-reload" => output(1, "Failed to connect to bus: No medium found\n"),
            _ => output(0, ""),
        });
        let run = |p: &str, a: &[&str]| fake.run(p, a);
        let e = install(&setup(Manager::Systemd, &units, &run), &spec).unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("wrote ") && msg.contains("No medium found"), "{msg}");
        assert!(units.join("devsandbox.service").exists());
        assert_eq!(fake.calls(), ["systemctl --user daemon-reload"]);

        // The manager says yes, but no daemon shows up.
        let fake = Fake::new(|_, _| output(0, ""));
        let run = |p: &str, a: &[&str]| fake.run(p, a);
        let s = Setup { start_wait: Duration::from_millis(200), ..setup(Manager::Systemd, &units, &run) };
        let msg = format!("{:#}", install(&s, &spec).unwrap_err());
        assert!(msg.contains("didn't answer") && msg.contains("serve.log"), "{msg}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn uninstall_disables_and_removes_and_leaves_a_lazy_daemon_alone() {
        let root = scratch("uninst");
        let sock = scratch("uninst-sock");
        let fake = Fake::new(|_, _| output(0, ""));
        let run = |p: &str, a: &[&str]| fake.run(p, a);

        // Nothing installed: one line, no manager calls.
        let s = setup(Manager::Systemd, &root.join("systemd/user"), &run);
        assert!(uninstall(&s).unwrap().starts_with("devsandbox serve is not installed"));
        assert!(fake.calls().is_empty());

        let lazy = spawn_daemon(&sock, keep_alive());
        let version = Version { semver: "0.6.0".into(), build: 1 };
        assert!(wait_answering(&sock, &version, Duration::from_secs(5)));
        std::fs::create_dir_all(&s.unit_dir).unwrap();
        std::fs::write(s.unit_path(), "x").unwrap();
        assert!(uninstall(&s).unwrap().starts_with("removed "));
        assert!(!s.unit_path().exists());
        assert_eq!(
            fake.calls(),
            ["systemctl --user disable --now devsandbox.service", "systemctl --user daemon-reload"]
        );
        assert!(wait_answering(&sock, &version, Duration::from_secs(1)), "uninstall stopped an unmanaged daemon");

        let fake = Fake::new(|_, _| output(3, "Boot-out failed"));
        let run = |p: &str, a: &[&str]| fake.run(p, a);
        let s = setup(Manager::Launchd, &root.join("Library/LaunchAgents"), &run);
        std::fs::create_dir_all(&s.unit_dir).unwrap();
        std::fs::write(s.unit_path(), "x").unwrap();
        uninstall(&s).unwrap();
        assert!(!s.unit_path().exists());
        assert_eq!(fake.calls(), ["launchctl bootout gui/501/dev.devsandbox.serve"]);

        assert_eq!(stop(&sock, &Mutex::new(Some(lazy))), Exit::Shutdown);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&sock);
    }
}
