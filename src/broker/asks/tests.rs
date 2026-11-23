use std::path::PathBuf;
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

/// A fresh data dir for `tag`'s kept answers.
fn data(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dsv-asks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn registry(tag: &str) -> Registry {
    Registry::new(data(tag))
}

fn manual(tag: &str) -> (Registry, Clock) {
    let base = Instant::now();
    let offset = Arc::new(AtomicU64::new(0));
    let read = Arc::clone(&offset);
    let r = Registry::with_clock(data(tag), Box::new(move || base + Duration::from_millis(read.load(Ordering::SeqCst))));
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
        sandbox: "web".into(),
        kind: "github".into(),
        method: "POST".into(),
        route: "/repos/o/r/pulls".into(),
        hold,
    }
}

const LONG: Duration = Duration::from_secs(30);

#[test]
fn ids_are_r_and_twelve_crockford_characters() {
    let r = registry("ids");
    let id = r.ask(&ask("pr.write", None, LONG)).unwrap().id;
    let rest = id.strip_prefix("R-").unwrap();
    assert_eq!(rest.len(), 12, "{id}");
    assert!(rest.bytes().all(|b| CROCKFORD.contains(&b)), "{id}");
    assert!(is_request_id(&id));
    assert_ne!(fresh_id().unwrap(), fresh_id().unwrap());
    for not in ["github-work", "R-", "R-0123456789A", "R-0123456789ABC", "R-0123456789AI", "r-0123456789AB"] {
        assert!(!is_request_id(not), "{not}");
    }
}

#[test]
fn the_same_key_joins_and_another_repo_opens_its_own() {
    let (r, _) = manual("join");
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
    let (r, clock) = manual("cap");
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
    let r = Arc::new(registry("wake"));
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
    let answered = r.answer(&id, Answer::Once).unwrap().unwrap();
    assert_eq!(answered.waiting, 2);
    for w in waiters {
        assert_eq!(w.join().unwrap(), Some(Answer::Once));
    }
    assert!(r.pending().is_empty());
    assert!(r.answer(&id, Answer::Once).unwrap().is_none(), "answered once");
    assert!(!answers::path(&r.data).exists(), "a once keeps nothing");
    assert!(!r.granted(&ask("pr.write", None, LONG).key), "answered in the hold: no grant left over");
    // Two opens/joins, one answer, two waiters leaving.
    assert_eq!(changes.load(Ordering::SeqCst), 5);
}

#[test]
fn deny_and_this_instance_are_kept_in_access_json_and_wake_the_waiters() {
    let r = Arc::new(registry("kept"));
    for (answer, kind, repo) in [(Answer::Deny, Kind::Deny, None), (Answer::Instance, Kind::Instance, Some("o/r"))] {
        let t = r.ask(&ask("pr.write", repo, LONG)).unwrap();
        let id = t.id.clone();
        let waiter = {
            let r = Arc::clone(&r);
            thread::spawn(move || r.wait(t))
        };
        thread::sleep(Duration::from_millis(50));
        r.answer(&id, answer).unwrap().unwrap();
        assert_eq!(waiter.join().unwrap(), Some(answer));
        assert!(r.pending().is_empty());
        let kept = answers::load(&answers::path(&r.data)).unwrap();
        let found = answers::lookup(&kept, &ask("pr.write", repo, LONG).key).unwrap();
        assert_eq!((found.id.as_str(), found.answer, found.instance.as_str()), (id.as_str(), kind, "web-1"));
        assert!(crate::broker::lfs::parse_rfc3339(&found.at).is_some(), "{}", found.at);
    }
    assert!(!home::path(&r.data).exists(), "no rule");
}

#[test]
fn always_is_a_rule_on_the_credential_and_wakes_the_waiters() {
    let r = Arc::new(registry("always"));
    let path = home::path(&r.data);
    home::update(&path, |h| Ok(h.insert("github-work".into(), home::Entry::new("github", None)))).unwrap();
    let t = r.ask(&ask("pr.write", Some("o/r"), LONG)).unwrap();
    let id = t.id.clone();
    let waiter = {
        let r = Arc::clone(&r);
        thread::spawn(move || r.wait(t))
    };
    thread::sleep(Duration::from_millis(50));
    r.answer(&id, Answer::Always).unwrap().unwrap();
    assert_eq!(waiter.join().unwrap(), Some(Answer::Always));
    let rules = home::load(&path).unwrap()["github-work"].rules.clone();
    let want = Rule { scope: "pr.write".into(), repo: Some("o/r".into()), sandbox: Some("web".into()), profile: None, id: Some(id) };
    assert_eq!(rules, [want]);
    assert!(!answers::path(&r.data).exists(), "nothing in access.json");

    // With its credential gone the answer fails and the request stays.
    home::update(&path, |h| {
        h.clear();
        Ok(())
    })
    .unwrap();
    let t = r.ask(&ask("pr.read", None, LONG)).unwrap();
    let err = format!("{:#}", r.answer(t.id(), Answer::Always).unwrap_err());
    assert!(err.contains("credential `github-work` is gone"), "{err}");
    assert_eq!(r.pending()[0].id, t.id);
}

#[test]
fn a_late_instance_answer_is_kept_and_leaves_no_one_shot_grant() {
    let (r, clock) = manual("late-kept");
    let key = ask("pr.write", None, LONG).key;
    let t = r.ask(&ask("pr.write", None, Duration::from_secs(1))).unwrap();
    let id = t.id.clone();
    clock.advance(Duration::from_secs(1));
    r.wait(t);
    r.answer(&id, Answer::Instance).unwrap().unwrap();
    assert!(!r.granted(&key));
    let kept = answers::load(&answers::path(&r.data)).unwrap();
    assert_eq!(answers::lookup(&kept, &key).map(|a| a.answer), Some(Kind::Instance));
}

#[test]
fn the_hold_ends_unanswered_and_the_request_stays_listed() {
    let r = registry("unanswered");
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
    let (r, clock) = manual("late-once");
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
    let (r, clock) = manual("late-window");
    let t = r.ask(&ask("pr.write", None, Duration::from_secs(1))).unwrap();
    let id = t.id.clone();
    clock.advance(Duration::from_secs(1));
    r.wait(t);
    clock.advance(LATE_GRANT - Duration::from_millis(1));
    assert_eq!(r.pending().len(), 1);
    clock.advance(Duration::from_millis(1));
    assert!(r.pending().is_empty());
    assert!(r.answer(&id, Answer::Once).unwrap().is_none());
}

#[test]
fn a_request_leaving_its_late_window_calls_the_hook_once() {
    let (r, clock) = manual("prune-hook");
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&changes);
    r.on_change(Box::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    let t = r.ask(&ask("pr.write", None, Duration::from_secs(1))).unwrap();
    clock.advance(Duration::from_secs(1));
    r.wait(t);
    let before = changes.load(Ordering::SeqCst);
    r.prune();
    assert_eq!(changes.load(Ordering::SeqCst), before, "nothing dropped");
    clock.advance(LATE_GRANT);
    r.prune();
    assert_eq!(changes.load(Ordering::SeqCst), before + 1);
    assert!(r.pending().is_empty());
    assert_eq!(changes.load(Ordering::SeqCst), before + 1, "dropped once");
}

#[test]
fn answers_parse_from_their_markers() {
    for a in [Answer::Once, Answer::Instance, Answer::Always, Answer::Deny] {
        assert_eq!(Answer::parse(a.marker()), Some(a));
    }
    assert_eq!(Answer::parse("yes"), None);
}

#[test]
fn a_call_keeps_the_grants_it_took() {
    let (r, clock) = manual("keeps");
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

/// A request whose only call's hold ended: answers no longer wake anyone.
fn unheld(r: &Registry, clock: &Clock) -> (String, AskKey) {
    let a = ask("pr.write", None, Duration::from_secs(1));
    let t = r.ask(&a).unwrap();
    let id = t.id.clone();
    clock.advance(Duration::from_secs(1));
    assert_eq!(r.wait(t), None);
    (id, a.key)
}

#[test]
fn wait_answer_reads_an_answer_given_before_it_and_leaves_the_late_grant() {
    let (r, clock) = manual("wait-before");
    let (id, key) = unheld(&r, &clock);
    r.answer(&id, Answer::Once).unwrap();
    assert_eq!(r.wait_answer(&id, "web-1-id", r.now()), Outcome::Answered(Answer::Once));
    assert!(r.granted(&key), "the retry still gets its one-shot grant");
    // Kept for the late window, then forgotten.
    clock.advance(LATE_GRANT - Duration::from_millis(1));
    assert_eq!(r.wait_answer(&id, "web-1-id", r.now()), Outcome::Answered(Answer::Once));
    clock.advance(Duration::from_millis(1));
    assert_eq!(r.wait_answer(&id, "web-1-id", r.now()), Outcome::Unknown);
}

#[test]
fn wait_answer_wakes_on_an_answer_given_while_it_waits() {
    let r = Arc::new(registry("wait-during"));
    let t = r.ask(&ask("pr.write", None, Duration::from_millis(10))).unwrap();
    let id = t.id.clone();
    assert_eq!(r.wait(t), None);
    let waiter = {
        let (r, id) = (Arc::clone(&r), id.clone());
        thread::spawn(move || r.wait_answer(&id, "web-1-id", r.now() + LONG))
    };
    thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    r.answer(&id, Answer::Deny).unwrap();
    assert_eq!(waiter.join().unwrap(), Outcome::Answered(Answer::Deny));
    assert!(started.elapsed() < LONG);
    assert_eq!(r.pending().len(), 0, "never counted as a waiter");
}

#[test]
fn wait_answer_times_out_pending_and_hides_other_instances_requests() {
    let (r, clock) = manual("wait-pending");
    let (id, _) = unheld(&r, &clock);
    assert_eq!(r.wait_answer(&id, "web-1-id", r.now()), Outcome::Pending);
    assert_eq!(r.pending()[0].waiting, 0);
    assert_eq!(r.wait_answer(&id, "other-id", r.now() + LONG), Outcome::Unknown, "at once, not at the deadline");
    assert_eq!(r.wait_answer("R-0000000000AB", "web-1-id", r.now() + LONG), Outcome::Unknown);
    r.answer(&id, Answer::Instance).unwrap();
    assert_eq!(r.wait_answer(&id, "other-id", r.now()), Outcome::Unknown, "nor their answers");
    assert_eq!(r.wait_answer(&id, "web-1-id", r.now()), Outcome::Answered(Answer::Instance));
    // Past its late window unanswered, a request is gone.
    let (id, _) = unheld(&r, &clock);
    clock.advance(LATE_GRANT);
    assert_eq!(r.wait_answer(&id, "web-1-id", r.now()), Outcome::Unknown);
}

fn wait_request(id: &str, timeout: Option<u64>) -> control::Request {
    control::Request { id: Some(id.into()), timeout, ..control::Request::new(Op::AccessWait) }
}

fn served(asks: Option<&Registry>, instance_id: &str, req: &control::Request, ping: Duration) -> String {
    let mut out = Vec::new();
    serve_wait(asks, instance_id, req, &mut out, ping);
    String::from_utf8(out).unwrap()
}

#[test]
fn serve_wait_pings_then_answers_one_response() {
    let r = registry("serve-wait");
    let t = r.ask(&ask("pr.write", None, Duration::from_millis(10))).unwrap();
    let id = t.id.clone();
    r.wait(t);
    let ping = format!("{}\n", control::FOLLOW_PING_LINE);
    let started = Instant::now();
    let out = served(Some(&r), "web-1-id", &wait_request(&id, Some(1)), Duration::from_millis(300));
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(out, format!("{}status ok\nbody pending\n", ping.repeat(3)));
    r.answer(&id, Answer::Once).unwrap();
    assert_eq!(served(Some(&r), "web-1-id", &wait_request(&id, None), LONG), "status ok\nbody once\n");
    assert_eq!(served(Some(&r), "other-id", &wait_request(&id, None), LONG), "status ok\nbody unknown\n");
    assert_eq!(served(None, "web-1-id", &wait_request(&id, None), LONG), "status ok\nbody unknown\n");
}

#[test]
fn serve_wait_takes_only_a_request_id_and_a_timeout() {
    let r = registry("serve-wait-usage");
    let usage = |req: &control::Request| served(Some(&r), "web-1-id", req, LONG);
    assert_eq!(usage(&control::Request::new(Op::AccessWait)), "status usage\nbody `access-wait` needs `id`\n");
    assert_eq!(usage(&wait_request("1790000000-a1b2", None)), "status usage\nbody bad request id `1790000000-a1b2`\n");
    let keyed = control::Request { key: Some("pr-1".into()), ..wait_request("R-0123456789AB", None) };
    assert_eq!(usage(&keyed), "status usage\nbody `access-wait` takes only `id` and `timeout`\n");
}

/// A client gone mid-wait: the first ping fails, and the wait ends there.
#[test]
fn serve_wait_ends_when_a_ping_cannot_be_written() {
    struct Gone;
    impl Write for Gone {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let r = registry("serve-wait-gone");
    let t = r.ask(&ask("pr.write", None, Duration::from_millis(10))).unwrap();
    let id = t.id.clone();
    r.wait(t);
    let started = Instant::now();
    serve_wait(Some(&r), "web-1-id", &wait_request(&id, Some(30)), &mut Gone, Duration::from_millis(20));
    assert!(started.elapsed() < LONG);
}

/// Every answer reads as an approval or a deny to the helper.
#[test]
fn every_answer_marker_has_an_exit_code() {
    for a in [Answer::Once, Answer::Instance, Answer::Always, Answer::Deny] {
        let code = control::access_exit_code(a.marker());
        assert_eq!(code, Some(if a.allows() { control::EXIT_OK } else { control::EXIT_ACCESS_DENY }), "{a:?}");
    }
}
