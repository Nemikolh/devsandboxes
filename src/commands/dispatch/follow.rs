//! `events-follow` (`devsbd events --follow`, docs/automations.md, *Following
//! events*): the requester's events pushed down one held-open control stream
//! instead of pulled by `events --wait`.
//!
//! The stream is the response header (`status ok`, empty body) followed by
//! JSON lines (`control`'s module doc): every pending event first, then each
//! new one as it's enqueued, each at most once per stream (acks still go
//! through `events-ack`, so a reconnecting follower gets everything unacked
//! again: at least once). A ping line after [`control::FOLLOW_PING`] without
//! a line keeps the stream warm and finds a dead one: the next write fails.
//!
//! One live follower per owner: [`Followers`] hands each new one a newer
//! generation, and an older one that sees it was superseded writes
//! [`control::FOLLOW_REPLACED_LINE`] and ends, so two copies of a dispatcher
//! can't both act on one event. Wakes on in-process store writes
//! (`inbox_ops::wait_changed`) and checks the store's stamp every
//! [`EVENTS_POLL`] for other processes' writes, like `events --wait`. Reads
//! only.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{event_json, failed, on_thread, EVENTS_POLL};
use crate::devsbd::control::{self, Response, Status};
use crate::inbox::{ops as inbox_ops, Event};

/// The live follower generation per owner `instance_id`. Process-wide in
/// production (one host daemon serves every bridge); a local one in tests.
pub(crate) struct Followers {
    /// `(last generation handed out, owner → its current follower's)`.
    inner: Mutex<(u64, BTreeMap<String, u64>)>,
}

impl Followers {
    pub(crate) const fn new() -> Followers {
        Followers { inner: Mutex::new((0, BTreeMap::new())) }
    }

    /// Make a new follower `owner`'s current one, superseding any other.
    pub(crate) fn claim(&self, owner: &str) -> Claim<'_> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.0 += 1;
        let generation = inner.0;
        inner.1.insert(owner.to_string(), generation);
        Claim { followers: self, owner: owner.to_string(), generation }
    }

    fn current(&self, owner: &str) -> Option<u64> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).1.get(owner).copied()
    }
}

/// One follower's place in [`Followers`]; dropping it forgets the owner
/// unless a newer follower took over meanwhile.
pub(crate) struct Claim<'a> {
    followers: &'a Followers,
    owner: String,
    generation: u64,
}

impl Claim<'_> {
    /// Whether no newer follower for this owner started since.
    pub(crate) fn is_current(&self) -> bool {
        self.followers.current(&self.owner) == Some(self.generation)
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut inner = self.followers.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.1.get(&self.owner) == Some(&self.generation) {
            inner.1.remove(&self.owner);
        }
    }
}

/// The events in `pending` not sent on this stream yet, oldest first,
/// recorded in `sent`. Ids no longer pending (acked, or dropped by the
/// per-thread cap) are forgotten, so `sent` stays bounded by the queue.
fn fresh(pending: Vec<(String, Event)>, sent: &mut HashSet<String>) -> Vec<(String, Event)> {
    sent.retain(|id| pending.iter().any(|(_, e)| &e.id == id));
    pending.into_iter().filter(|(_, e)| sent.insert(e.id.clone())).collect()
}

/// Follow `owner_id`'s events in the store at `path` (only `thread`'s if
/// given) on `out` until a write fails (the stream closed) or a newer
/// follower claims the owner (then the replaced line is the last one). `Err`
/// is a refusal written instead of a stream: the store didn't read at all.
pub(crate) fn follow_events(
    path: &Path,
    owner_id: &str,
    thread: Option<&str>,
    out: &mut dyn Write,
    followers: &Followers,
    ping: Duration,
) -> Result<(), Response> {
    let claim = followers.claim(owner_id);
    let mut follower = Follower { path, owner_id, thread, seen: None };
    let first = follower.read().map_err(|e| failed(format!("{e:#}")))?;
    // From here on the answer is the stream; it ends on a closed stream or a
    // store that stopped reading, and the client reconnects either way.
    let _ = follower.stream(out, &claim, first, ping);
    Ok(())
}

/// What one follower reads, and the store stamp it last read at.
struct Follower<'a> {
    path: &'a Path,
    owner_id: &'a str,
    thread: Option<&'a str>,
    seen: Option<(u64, Option<(std::time::SystemTime, u64)>)>,
}

impl Follower<'_> {
    /// The pending events, recording the stamp they were read at. Stamped
    /// before reading (the generation too, so two writes inside one mtime
    /// tick still read): a write in between only costs one more read.
    fn read(&mut self) -> anyhow::Result<Vec<(String, Event)>> {
        self.seen = Some((inbox_ops::generation(), inbox_ops::stamp(self.path)));
        Ok(on_thread(inbox_ops::events(self.path, self.owner_id)?, self.thread))
    }

    /// The pending events if the store changed since the last read.
    fn read_if_changed(&mut self) -> anyhow::Result<Option<Vec<(String, Event)>>> {
        let stamp = Some((inbox_ops::generation(), inbox_ops::stamp(self.path)));
        if stamp == self.seen {
            return Ok(None);
        }
        self.read().map(Some)
    }

    fn stream(
        &mut self,
        out: &mut dyn Write,
        claim: &Claim<'_>,
        first: Vec<(String, Event)>,
        ping: Duration,
    ) -> io::Result<()> {
        out.write_all(control::encode_response(&Response::new(Status::Ok, "")).as_bytes())?;
        out.flush()?;
        let mut line = |text: &str| -> io::Result<()> {
            out.write_all(format!("{text}\n").as_bytes())?;
            out.flush()
        };
        let mut sent = HashSet::new();
        let mut pending = Some(first);
        let mut last_line = Instant::now();
        loop {
            if !claim.is_current() {
                return line(control::FOLLOW_REPLACED_LINE);
            }
            // Only on a fresh read: `fresh` forgets ids missing from it.
            if let Some(pending) = pending.take() {
                for (thread, e) in fresh(pending, &mut sent) {
                    if let Some(json) = event_json(&thread, &e) {
                        line(&json)?;
                        last_line = Instant::now();
                    }
                }
            }
            if last_line.elapsed() >= ping {
                line(control::FOLLOW_PING_LINE)?;
                last_line = Instant::now();
            }
            let until_ping = ping.saturating_sub(last_line.elapsed());
            let generation = self.seen.map_or(0, |s| s.0);
            inbox_ops::wait_changed(generation, until_ping.min(EVENTS_POLL));
            pending = self.read_if_changed().map_err(|e| io::Error::other(format!("{e:#}")))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::{store, Compose, EventKind, State as TState, ThreadPut};
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::Arc;

    /// A writer sending each write as one string; fails once the receiver
    /// is gone, as a closed control stream does.
    struct Lines(mpsc::Sender<String>);

    impl Write for Lines {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let text = String::from_utf8(buf.to_vec()).unwrap();
            self.0.send(text).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn store_at(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-follow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("inbox.json")
    }

    /// The user replies `text` on `owner`'s thread `key`, putting it first.
    fn reply(path: &Path, owner: &str, key: &str, text: &str) {
        store::update_at(path, |i| {
            let find = |i: &crate::inbox::Inbox| {
                i.threads.iter().find(|t| t.owner == owner && t.key.as_deref() == Some(key)).map(|t| t.id)
            };
            if find(i).is_none() {
                let put = ThreadPut {
                    key: key.into(),
                    title: key.into(),
                    state: TState::NeedsYou,
                    compose: Some(Compose::default()),
                    ..ThreadPut::default()
                };
                i.put(owner, owner, 1, put);
            }
            let id = find(i).unwrap();
            i.apply(&crate::inbox::Op::Reply { thread: id, text: text.into() }, 2, "tui");
        })
        .unwrap();
    }

    fn event(id: &str) -> (String, Event) {
        let e = Event {
            id: id.into(),
            seq: 0,
            kind: EventKind::Reply,
            action: None,
            text: None,
            message: None,
            form: None,
            answers: None,
            at: 1,
        };
        ("t".into(), e)
    }

    /// A follower on its own thread; its lines arrive on the receiver, and
    /// the handle returns once it ended.
    fn spawn(
        path: &Path,
        owner: &str,
        thread: Option<&str>,
        followers: &Arc<Followers>,
        ping: Duration,
    ) -> (mpsc::Receiver<String>, std::thread::JoinHandle<Result<(), Response>>) {
        let (tx, rx) = mpsc::channel();
        let (path, owner, thread, followers) =
            (path.to_path_buf(), owner.to_string(), thread.map(String::from), Arc::clone(followers));
        let handle = std::thread::spawn(move || {
            follow_events(&path, &owner, thread.as_deref(), &mut Lines(tx), &followers, ping)
        });
        (rx, handle)
    }

    fn next(rx: &mpsc::Receiver<String>) -> String {
        rx.recv_timeout(Duration::from_secs(5)).expect("a line")
    }

    fn text_of(line: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        v["text"].as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn followers_hand_out_newer_generations_and_forget_on_drop() {
        let f = Followers::new();
        let a = f.claim("o");
        assert!(a.is_current());
        let other = f.claim("p");
        assert!(a.is_current(), "another owner doesn't replace it");
        let b = f.claim("o");
        assert!(!a.is_current() && b.is_current());
        // The superseded one leaving doesn't forget the newer one.
        drop(a);
        assert!(b.is_current());
        assert_eq!(f.current("o"), Some(b.generation));
        drop(b);
        assert_eq!(f.current("o"), None);
        assert!(other.is_current());
    }

    #[test]
    fn fresh_sends_each_once_and_forgets_acked_ids() {
        let mut sent = HashSet::new();
        let ids = |v: Vec<(String, Event)>| v.into_iter().map(|(_, e)| e.id).collect::<Vec<_>>();
        assert_eq!(ids(fresh(vec![event("a"), event("b")], &mut sent)), ["a", "b"]);
        assert_eq!(ids(fresh(vec![event("a"), event("b"), event("c")], &mut sent)), ["c"]);
        // `a` acked: forgotten, so the set stays bounded by the queue.
        assert_eq!(ids(fresh(vec![event("b"), event("c")], &mut sent)), Vec::<String>::new());
        assert_eq!(sent, HashSet::from(["b".to_string(), "c".to_string()]));
    }

    /// Pending first (filtered, the owner's own), then each new one once;
    /// a ping after the interval; a newer follower ends this one with
    /// `replaced`, after which it returns.
    #[test]
    fn streams_pending_then_new_events_pings_and_is_replaced() {
        let path = store_at("stream");
        reply(&path, "o", "pr-1", "one");
        reply(&path, "o", "pr-2", "other thread");
        reply(&path, "x", "pr-1", "not yours");
        let followers = Arc::new(Followers::new());
        let ping = Duration::from_millis(300);
        let (rx, first) = spawn(&path, "o", Some("pr-1"), &followers, ping);
        assert_eq!(next(&rx), "status ok\nbody \n");
        assert_eq!(text_of(&next(&rx)), "one");

        // In-process write: heard at once. Each new event once, not the old.
        reply(&path, "o", "pr-1", "two");
        assert_eq!(text_of(&next(&rx)), "two");
        reply(&path, "o", "pr-2", "elsewhere");
        let line = next(&rx);
        assert_eq!(line, format!("{}\n", control::FOLLOW_PING_LINE), "filtered out, then a ping");

        let (rx2, second) = spawn(&path, "o", Some("pr-1"), &followers, Duration::from_secs(30));
        assert_eq!(next(&rx), format!("{}\n", control::FOLLOW_REPLACED_LINE));
        assert_eq!(first.join().unwrap(), Ok(()));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "nothing after replaced");
        // The new stream gets everything unacked again.
        assert_eq!(next(&rx2), "status ok\nbody \n");
        assert_eq!(text_of(&next(&rx2)), "one");
        assert_eq!(text_of(&next(&rx2)), "two");
        // Closing the stream ends it at the next write.
        drop(rx2);
        reply(&path, "o", "pr-1", "three");
        assert_eq!(second.join().unwrap(), Ok(()));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A write by another process (a rename that bypasses `update_at`) is
    /// seen by the stamp fallback.
    #[test]
    fn hears_other_processes_by_the_stamp() {
        let path = store_at("stamp");
        reply(&path, "o", "pr-1", "one");
        let next_store = path.with_file_name("next.json");
        std::fs::copy(&path, &next_store).unwrap();
        reply(&next_store, "o", "pr-1", "two");
        let followers = Arc::new(Followers::new());
        let (rx, _handle) = spawn(&path, "o", None, &followers, Duration::from_secs(30));
        assert_eq!(next(&rx), "status ok\nbody \n");
        assert_eq!(text_of(&next(&rx)), "one");
        std::fs::rename(&next_store, &path).unwrap();
        assert_eq!(text_of(&next(&rx)), "two");
        drop(rx);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
