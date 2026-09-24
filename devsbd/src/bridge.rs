//! `devsbd bridge`: run by the host as `exec -i <c> devsbd bridge`. Does the
//! `Hello` exchange with the daemon (control socket) and the host (stdio),
//! then copies bytes verbatim between the two; the daemon parses frames.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use crate::daemon::CTL;
use crate::proto;

pub fn run(hash: &str) -> io::Result<()> {
    // Daemon first, so a missing or mismatched daemon surfaces to the host as
    // a failed handshake rather than a bridge that silently routes nowhere.
    let ctl = connect_ctl()
        .map_err(|e| io::Error::new(e.kind(), format!("daemon not running ({CTL}: {e})")))?;
    // On a daemon/helper mismatch, print the direction from the helper's own
    // point of view; the host then surfaces this stderr (it prefers the
    // bridge's stderr when its handshake with us hits EOF, which is what a
    // dying bridge here produces).
    proto::handshake(&mut ctl.try_clone()?, &mut ctl.try_clone()?, hash).map_err(|e| {
        match proto::version_mismatch(&e) {
            Some(vm) => io::Error::new(e.kind(), daemon_mismatch(vm)),
            None => e,
        }
    })?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    proto::handshake(&mut stdin, &mut stdout, hash)?;
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
fn connect_ctl() -> io::Result<UnixStream> {
    let mut tries = 40; // 40 x 50ms = 2s
    loop {
        match UnixStream::connect(CTL) {
            Ok(s) => return Ok(s),
            Err(e) if tries == 0 => return Err(e),
            Err(_) => {
                tries -= 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}
