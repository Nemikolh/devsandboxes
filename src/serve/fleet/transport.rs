//! How one host reaches another's devsandbox (docs/architecture/daemon/fleet.md,
//! *Transport*): [`Transport::dial`] of a target and a devsandbox command
//! line, a [`Duplex`] out: one byte pipe to that command's stdio, which a
//! link (`fleet::link`) runs on. The seam exists so that what carries the
//! bytes is one module's business: ssh is the only implementation
//! ([`Transport::ssh`]), and swapping it, or spreading a link over more than
//! one connection for bandwidth, changes this module and nothing that dials.
//! Tests hand back one end of a socket pair whose other end runs in-process
//! ([`Transport::new`]), seeing the target and the args they were asked
//! for.

use std::ffi::OsString;
use std::sync::Arc;

use anyhow::Result;

use crate::serve::machines::ssh::{self, Duplex};

/// The dial behind a [`Transport`]: the target, the program (`devsandbox`,
/// or a machine's `command`) and its args in, the connection out.
type Dial = dyn Fn(&str, &str, &[String]) -> Result<Duplex> + Send + Sync;

/// A way to reach a devsandbox on another host.
#[derive(Clone)]
pub struct Transport(Arc<Dial>);

impl Transport {
    /// The production transport: `ssh -T -o BatchMode=yes [-p N] -- <host>
    /// <devsandbox <args>>` through the target's login shell, as every
    /// machine argv is (`ssh::target_argv`, `ssh::remote_command`), spawned
    /// (`ssh::spawn`: its stdio behind a socket, its stderr's tail kept).
    /// `BatchMode`: the daemon has no TTY to prompt on.
    pub fn ssh() -> Transport {
        Transport(Arc::new(|target, program, args| ssh::spawn(&ssh_argv(target, program, args)?)))
    }

    /// A transport dialing with `dial` (tests: an in-process far end, which
    /// is devsandbox whatever the program).
    #[cfg(test)]
    pub fn new(dial: impl Fn(&str, &[String]) -> Result<Duplex> + Send + Sync + 'static) -> Transport {
        Transport(Arc::new(move |target, _, args| dial(target, args)))
    }

    /// A connection to `devsandbox <args>` on `target` (`[user@]host`, an
    /// ssh-config alias or `ssh://[user@]host[:port]`, [`check_target`]).
    pub fn dial(&self, target: &str, args: &[String]) -> Result<Duplex> {
        (self.0)(target, "devsandbox", args)
    }

    /// [`dial`](Self::dial) running `program` instead of `devsandbox` (a
    /// machine's `command`, as its login shell reads it).
    pub fn dial_with(&self, target: &str, program: &str, args: &[String]) -> Result<Duplex> {
        (self.0)(target, program, args)
    }
}

/// `Ok` when `target` is one [`Transport::ssh`] can dial; else why not.
pub fn check_target(target: &str) -> Result<()> {
    ssh::parse_target(target).map(drop)
}

/// [`Transport::ssh`]'s argv.
fn ssh_argv(target: &str, program: &str, args: &[String]) -> Result<Vec<OsString>> {
    ssh::target_argv(target, &["-T", "-o", "BatchMode=yes"], program, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_ssh_argv() {
        let link = args(&["fleet-link", "--worker", "lap", "--root", "/d/fleet/me@hub"]);
        let argv = ssh_argv("ssh://me@hub:2200", "devsandbox", &link).unwrap();
        let want = [
            "ssh",
            "-T",
            "-o",
            "BatchMode=yes",
            "-p",
            "2200",
            "--",
            "me@hub",
            r#"exec "$SHELL" -lc 'exec devsandbox '\''fleet-link'\'' '\''--worker'\'' '\''lap'\'' '\''--root'\'' '\''/d/fleet/me@hub'\'''"#,
        ];
        assert_eq!(argv, want.map(OsString::from));
        assert_eq!(ssh_argv("hub", "devsandbox", &link).unwrap()[4..6], [OsString::from("--"), "hub".into()]);
        assert!(ssh_argv("-oProxyCommand=x", "devsandbox", &link).is_err());
        let custom = ssh_argv("hub", "~/bin/ds", &args(&["fleet-link"])).unwrap();
        assert_eq!(custom.last().unwrap(), r#"exec "$SHELL" -lc 'exec ~/bin/ds '\''fleet-link'\'''"#);
    }

    #[test]
    fn targets_are_checked_as_ssh_reads_them() {
        assert!(check_target("me@hub").is_ok());
        assert!(check_target("ssh://me@hub:2200").is_ok());
        assert!(check_target("-oProxyCommand=x").is_err());
        assert!(check_target("").is_err());
    }

    #[test]
    fn a_fake_sees_the_target_and_args() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let saw = Arc::clone(&seen);
        let transport = Transport::new(move |target, args| {
            *saw.lock().unwrap() = Some((target.to_string(), args.to_vec()));
            anyhow::bail!("no route")
        });
        let err = transport.clone().dial("me@hub", &args(&["fleet-link"])).err().unwrap();
        assert_eq!(err.to_string(), "no route");
        assert_eq!(*seen.lock().unwrap(), Some(("me@hub".to_string(), args(&["fleet-link"]))));
    }
}
