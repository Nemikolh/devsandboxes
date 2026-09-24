//! `devsbd bridge`: run by the host as `exec -i <c> devsbd bridge`. Does the
//! `Hello` exchange with the daemon (control socket) and the host (stdio),
//! then copies bytes verbatim between the two; the daemon parses frames.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::daemon::CTL;
use crate::proto;

pub fn run(hash: &str) -> io::Result<()> {
    // Daemon first, so a missing or mismatched daemon surfaces to the host as
    // a failed handshake rather than a bridge that silently routes nowhere.
    let ctl = connect_ctl()
        .map_err(|e| io::Error::new(e.kind(), format!("daemon not running ({CTL}: {e})")))?;
    // On a daemon/helper mismatch, print the direction from the helper's own
    // point of view (the host surfaces this stderr, preferring it when its own
    // handshake with us hits EOF — what a dying bridge here produces) and exit
    // with `proto::MISMATCH_EXIT` so the host can classify the failure as a
    // mismatch from the exit code alone, without string-matching, and stop
    // retrying until a restart rewrites the helper.
    if let Err(e) = proto::handshake(&mut ctl.try_clone()?, &mut ctl.try_clone()?, hash, 0) {
        if let Some(vm) = proto::version_mismatch(&e) {
            eprintln!("devsbd: {}", daemon_mismatch(vm));
            std::process::exit(proto::MISMATCH_EXIT);
        }
        return Err(e);
    }
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    proto::handshake(&mut stdin, &mut stdout, hash, 0)?;
    drop(stdout);

    let mut from_daemon = ctl.try_clone()?;
    std::thread::spawn(move || {
        // Stdout is line-buffered; flush after every chunk so frames never
        // sit in the buffer waiting for a newline.
        let mut out = io::stdout().lock();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match from_daemon.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.write_all(&buf[..n]).and_then(|()| out.flush()).is_err() {
                        break;
                    }
                }
            }
        }
        // Daemon gone: nothing left to relay.
        std::process::exit(0);
    });
    // Keep the same (buffered) stdin lock: it may already hold bytes read
    // past the host's `Hello`.
    let mut to_daemon = ctl;
    io::copy(&mut stdin, &mut to_daemon)?;
    Ok(())
}

/// Daemon/helper protocol mismatch, phrased from the helper's side (`peer` is
/// the daemon, `ours` this bridge). The host surfaces this verbatim.
fn daemon_mismatch(vm: &proto::VersionMismatch) -> String {
    let dir = if vm.peer < vm.ours { "older" } else { "newer" };
    format!(
        "daemon in container is {dir} (protocol {}) than this helper ({})",
        vm.peer, vm.ours
    )
}

/// Connect to the daemon, retrying briefly: the host starts it with `exec -d`,
/// which returns before the daemon is listening.
///
/// Self-heal: if the very first connect fails because nothing is listening
/// (`NotFound` = socket gone, `ConnectionRefused` = stale socket, no daemon),
/// start one ourselves and keep retrying. This covers containers restarted
/// outside devsandbox (restart policy, `docker restart`, VM restart), where
/// the binary survives but the daemon does not. Racing self-starts are safe:
/// the daemon's pidfile flock (`daemon::take_over`) lets only one of this
/// build win; the losers exit `Ok`.
fn connect_ctl() -> io::Result<UnixStream> {
    let mut tries = 40; // 40 x 50ms = 2s
    let mut spawned = false;
    loop {
        match UnixStream::connect(CTL) {
            Ok(s) => return Ok(s),
            Err(e) if tries == 0 => return Err(e),
            Err(e) => {
                if !spawned && matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) {
                    // At most once per bridge; on spawn failure fall through to
                    // the normal retry/error path ("daemon not running").
                    spawned = true;
                    let _ = spawn_daemon();
                }
                tries -= 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

/// Start a detached daemon: our own binary, `daemon` subcommand, its own
/// process group so it outlives this bridge, all stdio null. stdout MUST be
/// null — the bridge's own stdout is the frame stream to the host. Not waited:
/// the daemon is our child until we exit, then reparented. A daemon that lost
/// the pidfile race exits at once and stays a zombie until this bridge exits,
/// which is harmless.
fn spawn_daemon() -> io::Result<()> {
    Command::new(std::env::current_exe()?)
        .arg("daemon")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
