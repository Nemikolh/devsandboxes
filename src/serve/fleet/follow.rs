//! Waiting for a hub's dev build on a link (docs/architecture/daemon/fleet.md,
//! *Versions*): once a link's `Hello`s say `update::Decision::Receive`, the
//! end that follows serves nothing but the one `binary` transfer the hub
//! pushes (`update::push`), into a private scratch dir, then ends the link
//! so the binary can be installed and handed off to
//! (`update::install_received`). Shared by the lending link's worker
//! (`fleet::worker`) and a peer link's dialed end (`commands::fleet_link`,
//! *Peer links*).

use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::transfer::Receivers;
use super::update::{BINARY_KIND, BINARY_LIMIT};
use crate::serve::endpoint::Stream;

/// How long a link waiting for the hub's binary waits for its transfer to
/// start; then it ends (the lending link retries after the backoff, a peer
/// link is redialed).
pub const PUSH_WAIT: Duration = Duration::from_secs(60);

/// How a wait ends its link: a lending link's stream shut, a peer link's
/// `link::Outlet::hang_up`.
pub type EndLink = Box<dyn Fn() + Send + Sync>;

/// The [`EndLink`] of a link over `stream`: shut both ways.
pub fn shutting(stream: Stream) -> EndLink {
    Box::new(move || {
        let _ = stream.shutdown(Shutdown::Both);
    })
}

/// The hub's binary on its way (`Decision::Receive`): what the `binary`
/// receiver takes, for the one link waiting for it.
#[derive(Default)]
pub struct Incoming {
    want: Mutex<Option<Want>>,
    wake: Condvar,
    seq: std::sync::atomic::AtomicU64,
}

pub struct Want {
    seq: u64,
    /// The hub's `Hello` hash: what the binary must report.
    pub hello: String,
    /// The private scratch dir it lands in.
    pub dir: PathBuf,
    /// Ends the link once the wait is over (received or not).
    end: Option<EndLink>,
    /// Its transfer's header was taken: no other is.
    started: bool,
    pub received: Option<PathBuf>,
    /// Why it didn't come.
    pub why: Option<String>,
}

impl Want {
    fn waiting(&self) -> bool {
        !self.started && self.received.is_none() && self.why.is_none()
    }
}

impl Incoming {
    /// Wait for the hub's binary on the link `end` ends; the wait's `seq`.
    pub fn expect(&self, hello: String, dir: PathBuf, end: Option<EndLink>) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        *self.want.lock().unwrap() = Some(Want { seq, hello, dir, end, started: false, received: None, why: None });
        seq
    }

    /// End the current wait with `f`: its link ended.
    fn finish(&self, f: impl FnOnce(&mut Want)) {
        if let Some(w) = self.want.lock().unwrap().as_mut() {
            f(w);
            if let Some(end) = &w.end {
                end();
            }
        }
        self.wake.notify_all();
    }

    /// The wait, over (its link ended).
    pub fn take(&self) -> Option<Want> {
        let want = self.want.lock().unwrap().take();
        self.wake.notify_all();
        want
    }

    /// After `wait` without the transfer starting, wait `seq` ends with why.
    pub fn watch(self: &Arc<Self>, seq: u64, wait: Duration) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new().name("fleet-push-wait".into()).spawn(move || {
            let current = |w: &Option<Want>| w.as_ref().is_some_and(|w| w.seq == seq && w.waiting());
            let guard = me.want.lock().unwrap();
            let (guard, _) = me.wake.wait_timeout_while(guard, wait, |w| current(w)).unwrap();
            if current(&guard) {
                drop(guard);
                me.finish(|w| w.why = Some(format!("the hub sent no binary within {wait:?}")));
            }
        });
    }

    /// `receivers` taking the hub's binary ([`BINARY_KIND`]) into the
    /// current wait's dir, once; with no wait, refused at the header.
    pub fn receiver(self: &Arc<Self>, receivers: Receivers) -> Receivers {
        let (take, done, failed) = (Arc::clone(self), Arc::clone(self), Arc::clone(self));
        receivers
            .with(
                BINARY_KIND,
                BINARY_LIMIT,
                move |_| {
                    let mut want = take.want.lock().unwrap();
                    match want.as_mut() {
                        Some(w) if w.waiting() => {
                            w.started = true;
                            take.wake.notify_all();
                            Ok(w.dir.join("devsandbox"))
                        }
                        _ => anyhow::bail!("this worker isn't waiting for a binary"),
                    }
                },
                move |r| done.finish(|w| w.received = Some(r.path)),
            )
            .on_failure(BINARY_KIND, move |why| failed.finish(|w| w.why = Some(format!("the hub's binary didn't arrive: {why}"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `binary` receiver takes one transfer, only while a link waits
    /// for it; a wait whose transfer never starts ends with why, its link
    /// shut.
    #[test]
    fn the_binary_receiver_takes_one_while_waited_for() {
        use crate::serve::fleet::transfer::{self, Header};
        let dir = std::env::temp_dir().join(format!("dsv-follow-incoming-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let incoming = Arc::new(Incoming::default());
        let receivers = Arc::new(incoming.receiver(Receivers::new(|_| {})));
        let (size, sha256) = transfer::digest(&b"bin"[..]).unwrap();
        let header = Header { kind: BINARY_KIND.into(), name: "devsandbox".into(), size, sha256, to: None };
        let err = transfer::send(receivers.accept().unwrap(), &header, &b"bin"[..]).unwrap_err();
        assert_eq!(format!("{err:#}"), "transfer refused: this worker isn't waiting for a binary");

        let (link, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        incoming.expect("h".into(), dir.clone(), Some(shutting(link.into())));
        transfer::send(receivers.accept().unwrap(), &header, &b"bin"[..]).unwrap();
        let err = transfer::send(receivers.accept().unwrap(), &header, &b"bin"[..]).unwrap_err();
        assert_eq!(format!("{err:#}"), "transfer refused: this worker isn't waiting for a binary", "once");
        let start = std::time::Instant::now();
        while incoming.want.lock().unwrap().as_ref().unwrap().received.is_none() {
            assert!(start.elapsed() < Duration::from_secs(5), "received");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(std::io::Read::read(&mut &peer, &mut [0u8; 1]).unwrap(), 0, "the link is shut");
        let want = incoming.take().unwrap();
        assert_eq!(want.received, Some(dir.join("devsandbox")));
        assert_eq!(std::fs::read(dir.join("devsandbox")).unwrap(), b"bin");

        let (link, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let seq = incoming.expect("h".into(), dir.clone(), Some(shutting(link.into())));
        incoming.watch(seq, Duration::from_millis(20));
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(std::io::Read::read(&mut &peer, &mut [0u8; 1]).unwrap(), 0, "shut by the watch");
        assert_eq!(incoming.take().unwrap().why.as_deref(), Some("the hub sent no binary within 20ms"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
