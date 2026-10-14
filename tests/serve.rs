//! End to end: the real `devsandbox serve` binary on a temp socket dir.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the daemon even when an assertion fails.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(runtime: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_devsandbox"));
    cmd.arg("serve")
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", runtime.join("state"))
        // The daemon's host side reads state.toml (bridges, autostart) and
        // restores forwards.toml from here: never the user's real ones.
        .env("XDG_DATA_HOME", runtime.join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn connect(sock: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = UnixStream::connect(sock) {
            return s;
        }
        assert!(Instant::now() < deadline, "serve never listened on {}", sock.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn hello(stream: &UnixStream, version: &str) -> serde_json::Value {
    let mut w = stream.try_clone().unwrap();
    writeln!(w, r#"{{"id":0,"method":"hello","params":{{"version":"{version}","build":0,"client":"e2e"}}}}"#).unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "serve did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn serve_binds_privately_refuses_a_second_copy_and_hands_off() {
    let runtime: PathBuf = std::env::temp_dir().join(format!("dsv-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    let dir = runtime.join("devsandbox");
    let sock = dir.join("serve.sock");

    let mut daemon = Daemon(serve(&runtime).spawn().unwrap());
    let stream = connect(&sock);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode(&dir), mode(&sock)), (0o700, 0o600));

    let reply = hello(&stream, env!("CARGO_PKG_VERSION"));
    assert_eq!(reply["id"], 0);
    assert_eq!(reply["result"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(reply["result"].get("handoff").is_none(), "{reply}");

    // A second daemon sees the first and exits 0 at once.
    let mut second = serve(&runtime).spawn().unwrap();
    assert!(wait_exit(&mut second).success());

    let newer = hello(&connect(&sock), "999.0.0");
    assert_eq!(newer["result"]["handoff"], true, "{newer}");
    assert!(wait_exit(&mut daemon.0).success());
    assert!(!sock.exists());
    let _ = std::fs::remove_dir_all(&runtime);
}

/// The CLI as a client of a scratch daemon, no docker: `rm` of a missing
/// instance is the daemon's `not-found`, printed as the CLI's error (exit
/// 1), and `stop --all` with an empty state runs in the daemon (exit 0, its
/// output streamed back). Neither falls back to running in-process.
#[test]
fn cli_verbs_go_through_the_daemon() {
    let runtime: PathBuf = std::env::temp_dir().join(format!("dsv-e2e-verbs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    let root = runtime.join("cfg");
    std::fs::create_dir_all(&root).unwrap();
    let home = runtime.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let _daemon = Daemon(serve(&runtime).env("HOME", &home).spawn().unwrap());
    // Answering before any CLI runs: none of them may lazy start one.
    let reply = hello(&connect(&runtime.join("devsandbox/serve.sock")), env!("CARGO_PKG_VERSION"));
    assert_eq!(reply["id"], 0, "{reply}");

    // The same isolated dirs as the daemon: the request's env is the verb's.
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_devsandbox"))
            .arg("-C")
            .arg(&root)
            .args(args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", runtime.join("state"))
            .env("XDG_DATA_HOME", runtime.join("data"))
            .env("HOME", &home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };

    let rm = cli(&["rm", "nope"]);
    let stderr = String::from_utf8_lossy(&rm.stderr);
    assert_eq!(rm.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("no sandbox instance matches `nope` (see `devsandbox ps -a`)"), "{stderr}");
    assert!(!stderr.contains("in-process"), "fell back: {stderr}");

    let stop = cli(&["stop", "--all"]);
    let stderr = String::from_utf8_lossy(&stop.stderr);
    assert_eq!(stop.status.code(), Some(0), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&stop.stdout), "nothing to stop\n");
    assert!(!stderr.contains("in-process"), "fell back: {stderr}");

    // `-d` where it doesn't apply is clap's error, before any request.
    assert_eq!(cli(&["stop", "-d", "--all"]).status.code(), Some(2));
    drop(_daemon);
    let _ = std::fs::remove_dir_all(&runtime);
}
