//! End to end: `devsandbox serve install|uninstall` under the real service
//! manager (systemd user instance / launchd `gui/<uid>`), where
//! `src/serve/install.rs`'s tests only have a fake one. Checks what a fake
//! can't: the manager parses the unit we write (escapes included), runs the
//! binary it names, respawns a crash, and does *not* respawn a handoff's
//! clean exit.
//!
//! The unit name is fixed and the manager is per user, so this replaces the
//! user's daemon: it only runs with `DEVSANDBOX_SERVICE_MANAGER_E2E=1` (CI sets
//! it), and refuses to touch an already-installed unit. Like the
//! `#[test_utils::*]` gates (which this crate can't reach), an unmet
//! requirement skips locally and fails on CI.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const OPT_IN: &str = "DEVSANDBOX_SERVICE_MANAGER_E2E";
const SYSTEMD_UNIT: &str = "devsandbox.service";
const LAUNCHD_LABEL: &str = "dev.devsandbox.serve";

/// Longer than systemd's `RestartSec=5` and launchd's 10 s respawn throttle.
const RESPAWN_WAIT: Duration = Duration::from_secs(30);

#[test]
fn serve_install_runs_under_the_real_service_manager() {
    if let Err(why) = e2e() {
        let test = "serve_install_runs_under_the_real_service_manager";
        if on_ci() {
            panic!("{test}: {why}; environment-gated tests must never skip on CI (`CI` is set)");
        }
        eprintln!("skipping {test}: {why}");
    }
}

fn on_ci() -> bool {
    std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// The user's service manager, as the test sees it.
struct Manager {
    uid: u32,
    unit: PathBuf,
}

impl Manager {
    fn probe() -> Result<Self, String> {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("$HOME is unset")?);
        let unit = if cfg!(target_os = "linux") {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|c| c.is_absolute())
                .unwrap_or_else(|| home.join(".config"));
            config.join("systemd/user").join(SYSTEMD_UNIT)
        } else {
            home.join("Library/LaunchAgents").join(format!("{LAUNCHD_LABEL}.plist"))
        };
        let m = Self { uid, unit };
        let reach = if cfg!(target_os = "linux") {
            tool("systemctl", &["--user", "show-environment"])
        } else {
            tool("launchctl", &["print", &format!("gui/{uid}")])
        }?;
        if !reach.status.success() {
            return Err(format!("no user service manager: {}", String::from_utf8_lossy(&reach.stderr).trim()));
        }
        Ok(m)
    }

    fn target(&self) -> String {
        format!("gui/{}/{LAUNCHD_LABEL}", self.uid)
    }

    /// The managed daemon's pid, if it runs.
    fn pid(&self) -> Option<u32> {
        if cfg!(target_os = "linux") {
            let out = stdout("systemctl", &["--user", "show", "-p", "MainPID", "--value", SYSTEMD_UNIT]);
            out.trim().parse().ok().filter(|&p| p != 0)
        } else {
            let out = stdout("launchctl", &["print", &self.target()]);
            out.lines().find_map(|l| l.trim().strip_prefix("pid = ")).and_then(|p| p.parse().ok())
        }
    }

    /// After a clean exit: stopped, and not as a failure.
    fn assert_stopped_cleanly(&self) {
        if cfg!(target_os = "linux") {
            let state = stdout("systemctl", &["--user", "show", "-p", "ActiveState,SubState", SYSTEMD_UNIT]);
            assert_eq!(state.trim(), "ActiveState=inactive\nSubState=dead", "the manager respawned or failed it");
        } else {
            let print = stdout("launchctl", &["print", &self.target()]);
            assert!(!print.contains("pid = "), "launchd respawned it:\n{print}");
            assert!(print.contains("last exit code = 0"), "{print}");
        }
    }

    /// After `serve uninstall`: the manager no longer knows the unit.
    fn assert_unloaded(&self) {
        if cfg!(target_os = "linux") {
            let load = stdout("systemctl", &["--user", "show", "-p", "LoadState", "--value", SYSTEMD_UNIT]);
            assert_eq!(load.trim(), "not-found");
        } else {
            let out = tool("launchctl", &["print", &self.target()]).unwrap();
            assert!(!out.status.success(), "{} is still loaded", self.target());
        }
    }

    /// The manager's own linter accepts the unit file.
    fn lint(&self) -> Result<(), String> {
        let unit = self.unit.to_str().unwrap();
        let out = if cfg!(target_os = "linux") {
            tool("systemd-analyze", &["--user", "--man=no", "verify", unit])
        } else {
            tool("plutil", &["-lint", unit])
        }?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && !stderr.contains(SYSTEMD_UNIT), "{}{stderr}", String::from_utf8_lossy(&out.stdout));
        Ok(())
    }
}

/// Run `program`; not installed is an environment problem.
fn tool(program: &str, args: &[&str]) -> Result<Output, String> {
    Command::new(program).args(args).output().map_err(|e| format!("cannot run `{program}`: {e}"))
}

fn stdout(program: &str, args: &[&str]) -> String {
    String::from_utf8_lossy(&tool(program, args).unwrap().stdout).into_owned()
}

/// The devsandbox under test, with its XDG data/state (which `install`
/// captures into the unit) under `root`.
struct Sandbox {
    root: PathBuf,
    exe: PathBuf,
    socket: PathBuf,
}

impl Sandbox {
    fn devsandbox(&self, args: &[&str]) -> Output {
        Command::new(&self.exe)
            .args(args)
            .env("XDG_STATE_HOME", self.root.join("state dir"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .output()
            .unwrap()
    }

    fn install(&self) {
        let out = self.devsandbox(&["serve", "install"]);
        assert!(out.status.success(), "serve install: {}\n{}", String::from_utf8_lossy(&out.stderr), self.log());
    }

    fn log(&self) -> String {
        let log = self.root.join("state dir/devsandbox/serve.log");
        format!("serve.log:\n{}", std::fs::read_to_string(log).unwrap_or_default())
    }

    /// A hello as `version`: `Some(handoff)` when a daemon answers.
    fn hello(&self, version: &str) -> Option<bool> {
        let stream = UnixStream::connect(&self.socket).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut w = stream.try_clone().unwrap();
        writeln!(w, r#"{{"id":0,"method":"hello","params":{{"version":"{version}","build":0,"client":"e2e"}}}}"#).ok()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).ok()?;
        let reply: serde_json::Value = serde_json::from_str(&line).ok()?;
        Some(reply["result"]["handoff"] == true)
    }

    fn answers(&self) -> bool {
        self.hello(env!("CARGO_PKG_VERSION")).is_some()
    }
}

/// Uninstalls and removes the scratch dir even when an assertion fails.
struct Cleanup<'a>(&'a Sandbox);

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = self.0.devsandbox(&["serve", "uninstall"]);
        let _ = std::fs::remove_dir_all(&self.0.root);
    }
}

fn wait_for(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ok() {
        assert!(Instant::now() < deadline, "{what} within {within:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn e2e() -> Result<(), String> {
    if std::env::var(OPT_IN).as_deref() != Ok("1") {
        return Err(format!("{OPT_IN}=1 not set (this replaces your installed devsandbox serve)"));
    }
    let m = Manager::probe()?;
    if m.unit.exists() {
        return Err(format!("{} is already installed; not replacing it", m.unit.display()));
    }

    // A short root (macOS caps socket paths at 104 bytes) and a binary path
    // that needs systemd's quoting and specifier escapes.
    let root = PathBuf::from(format!("/tmp/dsv-sm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin 100% $HOME");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("devsandbox");
    std::fs::copy(env!("CARGO_BIN_EXE_devsandbox"), &exe).unwrap();
    // Where the binary puts it: `$XDG_RUNTIME_DIR` (not overridden, systemctl
    // needs the real one), else the state dir.
    let socket_dir = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|r| r.is_absolute()) {
        Some(runtime) => runtime.join("devsandbox"),
        None => root.join("state dir/devsandbox"),
    };
    let sb = Sandbox { root, exe, socket: socket_dir.join("serve.sock") };
    let _cleanup = Cleanup(&sb);

    sb.install();
    assert!(m.unit.is_file());
    m.lint()?;
    let first = m.pid().unwrap_or_else(|| panic!("installed, but the manager runs nothing\n{}", sb.log()));
    assert!(sb.answers());
    #[cfg(target_os = "linux")]
    assert_eq!(std::fs::read_link(format!("/proc/{first}/exe")).unwrap(), sb.exe, "the unit runs another binary");

    // A crash is the manager's to restart.
    // SAFETY: plain syscall on a pid the manager just reported.
    assert_eq!(unsafe { libc::kill(first as i32, libc::SIGKILL) }, 0);
    wait_for("the manager respawns a killed daemon", RESPAWN_WAIT, || {
        m.pid().is_some_and(|p| p != first) && sb.answers()
    });

    // A handoff exits 0, and the manager must leave it down: the newer
    // client restarts the unit itself, which this hello doesn't.
    assert_eq!(sb.hello("999.0.0"), Some(true), "no handoff offered");
    wait_for("the handed-off daemon exits", Duration::from_secs(15), || m.pid().is_none() && !sb.answers());
    std::thread::sleep(Duration::from_secs(12));
    m.assert_stopped_cleanly();
    assert!(!sb.answers());

    // Install brings a stopped unit back, and restarts a running one.
    sb.install();
    let again = m.pid().expect("reinstalled, but nothing runs");
    sb.install();
    wait_for("a reinstall restarts the daemon", Duration::from_secs(10), || {
        m.pid().is_some_and(|p| p != again) && sb.answers()
    });

    let out = sb.devsandbox(&["serve", "uninstall"]);
    assert!(out.status.success(), "serve uninstall: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!m.unit.exists());
    wait_for("uninstall stops the managed daemon", Duration::from_secs(10), || !sb.answers());
    m.assert_unloaded();
    Ok(())
}
