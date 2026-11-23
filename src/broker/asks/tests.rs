use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;

use super::*;

/// A registry whose clock moves only by [`Clock::advance`].
struct Clock(Arc<AtomicU64>);

impl Clock {
    fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }
}

fn manual() -> (Registry, Clock) {
    let base = Instant::now();
    let offset = Arc::new(AtomicU64::new(0));
    let read = Arc::clone(&offset);
    let r = Registry::with_clock(Box::new(move || base + Duration::from_millis(read.load(Ordering::SeqCst))));
    (r, Clock(offset))
}

fn ask(scope: &str, repo: Option<&str>, hold: Duration) -> Ask {
    Ask {
        key: AskKey {
            instance_id: "web-1-id".into(),
            credential: "github-work".into(),
            scope: scope.into(),
            repo: repo.map(String::from),
        },
        instance: "web-1".into(),
        kind: "github".into(),
        method: "POST".into(),
        route: "/repos/o/r/pulls".into(),
        hold,
    }
}

const LONG: Duration = Duration::from_secs(30);

#[test]
fn ids_are_r_and_twelve_crockford_characters() {
    let r = Registry::default();
    let id = r.ask(&ask("pr.write", None, LONG)).unwrap().id;
    let rest = id.strip_prefix("R-").unwrap();
    assert_eq!(rest.len(), 12, "{id}");
    assert!(rest.bytes().all(|b| CROCKFORD.contains(&b)), "{id}");
    assert_ne!(fresh_id().unwrap(), fresh_id().unwrap());
}

#[test]
fn the_same_key_joins_and_another_repo_opens_its_own() {
    let (r, _) = manual();
    let a = r.ask(&ask("pr.write", Some("o/r"), LONG)).unwrap();
    let b = r.ask(&ask("pr.write", Some("o/r"), LONG)).unwrap();
    assert_eq!(a.id, b.id);
    let c = r.ask(&ask("pr.write", Some("o/s"), LONG)).unwrap();
    assert_ne!(a.id, c.id);
    let pending = r.pending();
    assert_eq!(pending.iter().map(|p| (p.id.as_str(), p.waiting)).collect::<Vec<_>>(), [(a.id(), 2), (c.id(), 1)]);
    let p = &pending[0];
    assert_eq!((p.instance.as_str(), p.method.as_str(), p.route.as_str()), ("web-1", "POST", "/repos/o/r/pulls"));
    assert_eq!(p.key.repo.as_deref(), Some("o/r"));
}

#[test]
fn past_the_cap_a_new_request_is_refused_at_once() {
    let (r, clock) = manual();
    let first = r.ask(&ask("a", None, LONG)).unwrap();
    r.ask(&ask("b", None, LONG)).unwrap();
    r.ask(&ask("c", None, LONG)).unwrap();
    let e = r.ask(&ask("d", None, LONG)).unwrap_err();
    assert_eq!((e.status, e.verdict), (403, "pending"));
    assert_eq!(e.reason, format!("another request pending for this instance: {}", first.id));
    assert_eq!(e.request.as_deref(), Some(first.id()));
    assert_eq!(e.todo, Some(format!("devsandbox access approve {}", first.id)));
    assert_eq!(r.pending().len(), 3, "no new request");
    // A retry still joins; another instance has its own cap.
    assert_eq!(r.ask(&ask("a", None, LONG)).unwrap().id, first.id);
    let mut other = ask("d", None, LONG);
    other.key.instance_id = "other-id".into();
    r.ask(&other).unwrap();
    // Requests whose hold ended don't count.
    clock.advance(LONG);
    r.ask(&ask("d", None, LONG)).unwrap();
}

#[test]
fn an_answer_wakes_every_waiter() {
    let r = Arc::new(Registry::default());
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&changes);
    r.on_change(Box::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    let tickets = [r.ask(&ask("pr.write", None, LONG)).unwrap(), r.ask(&ask("pr.write", None, LONG)).unwrap()];
    let id = tickets[0].id.clone();
    let waiters: Vec<_> = tickets
        .into_iter()
        .map(|t| {
            let r = Arc::clone(&r);
            thread::spawn(move || r.wait(t))
        })
        .collect();
    thread::sleep(Duration::from_millis(50));
    let answered = r.answer(&id, Answer::Once).unwrap();
    assert_eq!(answered.waiting, 2);
    for w in waiters {
        assert_eq!(w.join().unwrap(), Some(Answer::Once));
    }
    assert!(r.pending().is_empty());
    assert!(r.answer(&id, Answer::Once).is_none(), "answered once");
    assert!(!r.granted(&ask("pr.write", None, LONG).key), "answered in the hold: no grant left over");
    // Two opens/joins, one answer, two waiters leaving.
    assert_eq!(changes.load(Ordering::SeqCst), 5);
}

#[test]
fn deny_wakes_with_deny() {
    let r = Arc::new(Registry::default());
    let t = r.ask(&ask("pr.write", None, LONG)).unwrap();
    let id = t.id.clone();
    let waiter = {
        let r = Arc::clone(&r);
        thread::spawn(move || r.wait(t))
    };
    thread::sleep(Duration::from_millis(50));
    r.answer(&id, Answer::Deny).unwrap();
    assert_eq!(waiter.join().unwrap(), Some(Answer::Deny));
    assert!(r.pending().is_empty());
}

#[test]
fn the_hold_ends_unanswered_and_the_request_stays_listed() {
    let r = Registry::default();
    let started = Instant::now();
    let t = r.ask(&ask("pr.write", None, Duration::from_millis(50))).unwrap();
    let id = t.id.clone();
    assert_eq!(r.wait(t), None);
    assert!(started.elapsed() >= Duration::from_millis(50));
    let pending = r.pending();
    assert_eq!((pending[0].id.as_str(), pending[0].waiting), (id.as_str(), 0));
    // A retry joins it, same id.
    assert_eq!(r.ask(&ask("pr.write", None, LONG)).unwrap().id, id);
}

#[test]
fn a_late_once_grants_the_next_matching_call_once() {
    let (r, clock) = manual();
    let key = ask("pr.write", Some("o/r"), LONG).key;
    let t = r.ask(&ask("pr.write", Some("o/r"), Duration::from_secs(1))).unwrap();
    let id = t.id.clone();
    clock.advance(Duration::from_secs(1));
    assert_eq!(r.wait(t), None);
    assert!(!r.granted(&key), "not answered yet");
    r.answer(&id, Answer::Once).unwrap();
    assert!(r.pending().is_empty());
    let mut other_repo = key.clone();
    other_repo.repo = Some("o/s".into());
    assert!(!r.granted(&other_repo));
    assert!(r.granted(&key));
    assert!(!r.granted(&key), "one call only");

    // Unused, it expires.
    let t = r.ask(&ask("pr.write", Some("o/r"), Duration::from_secs(1))).unwrap();
    let id = t.id.clone();
    clock.advance(Duration::from_secs(1));
    r.wait(t);
    r.answer(&id, Answer::Once).unwrap();
    clock.advance(LATE_GRANT);
    assert!(!r.granted(&key));

    // A late deny leaves nothing.
    let t = r.ask(&ask("pr.write", Some("o/r"), Duration::from_secs(1))).unwrap();
    let id = t.id.clone();
    clock.advance(Duration::from_secs(1));
    r.wait(t);
    r.answer(&id, Answer::Deny).unwrap();
    assert!(!r.granted(&key));
}

#[test]
fn an_unanswered_request_goes_once_its_late_window_passes() {
    let (r, clock) = manual();
    let t = r.ask(&ask("pr.write", None, Duration::from_secs(1))).unwrap();
    let id = t.id.clone();
    clock.advance(Duration::from_secs(1));
    r.wait(t);
    clock.advance(LATE_GRANT - Duration::from_millis(1));
    assert_eq!(r.pending().len(), 1);
    clock.advance(Duration::from_millis(1));
    assert!(r.pending().is_empty());
    assert!(r.answer(&id, Answer::Once).is_none());
}

#[test]
fn a_call_keeps_the_grants_it_took() {
    let (r, clock) = manual();
    let key = ask("pr.write", None, LONG).key;
    let t = r.ask(&ask("pr.write", None, Duration::from_secs(1))).unwrap();
    let id = t.id.clone();
    clock.advance(Duration::from_secs(1));
    r.wait(t);
    r.answer(&id, Answer::Once).unwrap();
    let grants = Grants::new(&r);
    assert!(grants.granted(&key));
    assert!(grants.granted(&key), "the same call, decided again");
    assert!(!Grants::new(&r).granted(&key), "taken from the registry");
    let held = ask("x", None, LONG).key;
    assert!(!grants.granted(&held));
    grants.allow(held.clone());
    assert!(grants.granted(&held));
}
