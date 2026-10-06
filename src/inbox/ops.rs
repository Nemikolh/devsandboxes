//! Every Inbox read and mutation, as a function over a store path: the one
//! layer the TUI, the bridge's notify sink, `devsandbox rm` and the control
//! handler go through, and what the daemon's API handlers will be thin calls
//! into (docs/inbox-redesign.md, *One API, served by the daemon*). One code
//! path per verb is what makes every client produce identical events.
//!
//! Each mutation is one locked read-modify-write ([`store::update_at`]), and
//! times are minted inside it, so stamps follow the order writers serialize
//! in rather than the order they happened to read the clock. Reads take the
//! store's shared lock ([`store::load_at`]). The path is a parameter so tests
//! and a daemon can point at their own file; callers that want the default
//! pass [`store::path`].
//!
//! User ops address threads by their store id ([`super::Thread::id`]),
//! which is persisted and stable across dashboards; [`thread_id`] maps the
//! `(owner, key)` pair an external client knows to that id.

use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;

use super::{store, Event, Inbox, Kind, Op, Shown, SinkAction, Thread};

/// Unix seconds now: the stamp of every entry and event a mutation adds.
fn now() -> u64 {
    crate::state::Instance::now()
}

/// The whole Inbox, for a client rendering it.
pub fn load(path: &Path) -> Result<Inbox> {
    store::load_at(path)
}

/// The store's change stamp ([`store::stamp_at`]): reload when it moves.
pub fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    store::stamp_at(path)
}

/// This process's store write generation (`store::generation`).
pub fn generation() -> u64 {
    store::generation()
}

/// Wait for a store write by this process after generation `seen`, up to
/// `timeout` (`store::wait_changed`).
pub fn wait_changed(seen: u64, timeout: std::time::Duration) -> u64 {
    store::wait_changed(seen, timeout)
}

/// Apply a batch of user [`Op`]s in one write, all stamped with one `now`
/// minted under the lock (event ids and times are never minted by a client).
pub fn apply(path: &Path, ops: &[Op]) -> Result<()> {
    store::update_at(path, |inbox| {
        let now = now();
        ops.iter().for_each(|op| inbox.apply(op, now));
    })
}

/// [`apply`] with a check against the current store, in the same write:
/// `decide` sees the Inbox under the lock and returns the ops to apply, or
/// why not. What an API client needs, since [`Inbox::apply`] treats a stale
/// id or action as a silent no-op and the client must hear `not-found`.
pub fn apply_if<E>(path: &Path, decide: impl FnOnce(&Inbox) -> Result<Vec<Op>, E>) -> Result<Result<(), E>> {
    store::update_at(path, |inbox| {
        let ops = decide(inbox)?;
        let now = now();
        ops.iter().for_each(|op| inbox.apply(op, now));
        Ok(())
    })
}

/// The notify sink: store one decoded container message from `owner`
/// (`instance_id`; `owner_name` for display) and say what to surface.
pub fn sink(path: &Path, owner: &str, owner_name: &str, action: SinkAction) -> Result<Option<Shown>> {
    store::update_at(path, |inbox| inbox.apply_sink(owner, owner_name, now(), action))
}

/// `devsandbox rm`: mark `owner`'s threads read-only and run retention; how
/// many threads were archived.
pub fn archive(path: &Path, owner: &str) -> Result<usize> {
    store::update_at(path, |inbox| {
        let archived = inbox.archive_owner(owner);
        inbox.prune(now());
        archived
    })
}

/// `owner`'s pending events with their thread keys, oldest first.
pub fn events(path: &Path, owner: &str) -> Result<Vec<(String, Event)>> {
    Ok(store::load_at(path)?.events_for(owner))
}

/// Drop `owner`'s events with these ids; how many were dropped.
pub fn ack(path: &Path, owner: &str, ids: &[String]) -> Result<usize> {
    store::update_at(path, |inbox| inbox.ack(owner, ids))
}

/// `owner`'s live dispatcher threads.
pub fn threads(path: &Path, owner: &str) -> Result<Vec<Thread>> {
    Ok(store::load_at(path)?.threads_for(owner).into_iter().cloned().collect())
}

/// The store id of `owner`'s dispatcher thread `key`, archived or not. Only
/// thread-kind threads: a notify record may share the key, but `(owner,
/// key)` names a `thread put`, as it does for [`Inbox::put`].
pub fn thread_id(inbox: &Inbox, owner: &str, key: &str) -> Option<u64> {
    inbox
        .threads
        .iter()
        .find(|t| t.owner == owner && t.kind == Kind::Thread && t.key.as_deref() == Some(key))
        .map(|t| t.id)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::devsbd::notify::{Level, Record};
    use crate::inbox::{Reply, State, ThreadPut, RETENTION};

    /// A fresh store path in a unique empty directory under the temp dir.
    fn store_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-inbox-ops-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("inbox.toml")
    }

    /// A reply-taking dispatcher thread `key`, put by `owner` at time `at`.
    fn put(path: &Path, owner: &str, key: &str, at: u64) {
        let put = ThreadPut {
            key: key.into(),
            title: key.into(),
            state: State::NeedsYou,
            reply: Some(Reply::default()),
            ..ThreadPut::default()
        };
        store::update_at(path, |i| i.put(owner, owner, at, put)).unwrap();
    }

    fn id(path: &Path, owner: &str, key: &str) -> u64 {
        thread_id(&load(path).unwrap(), owner, key).unwrap()
    }

    #[test]
    fn a_user_batch_enqueues_events_stamped_at_apply() {
        let path = store_path("batch");
        put(&path, "d", "pr-1", 1);
        let thread = id(&path, "d", "pr-1");
        let before = now();
        let batch = [Op::MarkRead(thread), Op::Reply { thread, text: "hi".into() }, Op::MarkDone(thread)];
        apply(&path, &batch).unwrap();
        let after = now();

        let events = events(&path, "d").unwrap();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events.iter().all(|(key, _)| key == "pr-1"));
        assert_eq!(events[0].1.text.as_deref(), Some("hi"));
        for (_, e) in &events {
            assert!((before..=after).contains(&e.at), "{} not in {before}..={after}", e.at);
        }
        // One `now` for the whole batch.
        assert_eq!(events[0].1.at, events[1].1.at);
        let inbox = load(&path).unwrap();
        assert!(!inbox.threads[0].unread);
        assert_eq!(inbox.threads[0].state, Some(State::Done));
    }

    #[test]
    fn ack_drops_only_the_owners_events() {
        let path = store_path("ack");
        put(&path, "d", "pr-1", 1);
        put(&path, "a", "pr-1", 1);
        let (d, a) = (id(&path, "d", "pr-1"), id(&path, "a", "pr-1"));
        apply(&path, &[Op::Reply { thread: d, text: "x".into() }, Op::Reply { thread: a, text: "y".into() }])
            .unwrap();
        let d_ids: Vec<String> = events(&path, "d").unwrap().into_iter().map(|(_, e)| e.id).collect();
        let a_ids: Vec<String> = events(&path, "a").unwrap().into_iter().map(|(_, e)| e.id).collect();
        assert_eq!(ack(&path, "d", &a_ids).unwrap(), 0, "another owner's ids are skipped");
        assert_eq!(ack(&path, "d", &d_ids).unwrap(), 1);
        assert_eq!(ack(&path, "d", &d_ids).unwrap(), 0, "a retried ack is harmless");
        assert!(events(&path, "d").unwrap().is_empty());
        assert_eq!(events(&path, "a").unwrap().len(), 1);
    }

    #[test]
    fn archive_hides_the_owners_threads_and_prunes_old_ones() {
        let path = store_path("archive");
        // Archived long ago: retention drops it on the next archive pass.
        put(&path, "old", "pr-1", 1);
        store::update_at(&path, |i| i.archive_owner("old")).unwrap();
        put(&path, "d", "pr-1", now());
        put(&path, "d", "pr-2", now());
        put(&path, "a", "pr-1", now());

        assert_eq!(archive(&path, "d").unwrap(), 2);
        assert_eq!(archive(&path, "d").unwrap(), 0, "already archived");
        let inbox = load(&path).unwrap();
        assert!(thread_id(&inbox, "old", "pr-1").is_none(), "pruned past {RETENTION}s");
        assert!(thread_id(&inbox, "d", "pr-1").is_some(), "kept as history");
        assert!(threads(&path, "d").unwrap().is_empty(), "archived threads aren't live");
        assert_eq!(threads(&path, "a").unwrap().len(), 1);
    }

    #[test]
    fn sink_stores_and_reports_what_to_show() {
        let path = store_path("sink");
        let record = Record { level: Level::Info, key: None, link: None, msg: "built".into(), at: 0 };
        let shown = sink(&path, "web-id", "web", SinkAction::Push(record)).unwrap().unwrap();
        assert_eq!(shown.line, "web: built");
        let inbox = load(&path).unwrap();
        assert_eq!(inbox.threads[0].owner, "web-id");
        assert!(stamp(&path).is_some());
    }

    #[test]
    fn thread_id_names_the_owners_dispatcher_thread() {
        let path = store_path("lookup");
        // A notify record with the same key is a different object.
        let record = Record { level: Level::Info, key: Some("pr-1".into()), link: None, msg: "n".into(), at: 0 };
        sink(&path, "d", "d", SinkAction::Push(record)).unwrap();
        put(&path, "d", "pr-1", 1);
        put(&path, "a", "pr-1", 1);
        let inbox = load(&path).unwrap();
        let d = thread_id(&inbox, "d", "pr-1").unwrap();
        let t = inbox.threads.iter().find(|t| t.id == d).unwrap();
        assert_eq!((t.owner.as_str(), t.kind), ("d", Kind::Thread));
        assert_ne!(Some(d), thread_id(&inbox, "a", "pr-1"));
        assert_eq!(thread_id(&inbox, "d", "pr-9"), None);
        assert_eq!(thread_id(&inbox, "x", "pr-1"), None);
    }
}
