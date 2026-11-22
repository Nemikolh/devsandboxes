//! The daemon's scheduled feature updates (docs/architecture/daemon/updates.md):
//! each update a config root's sandboxes declare runs as `devsandbox
//! --in-process update <name>` once its interval has passed since its last
//! success. [`pass`] is one round over every root, its I/O behind [`Env`];
//! `host.rs` runs it at start and every [`CHECK_EVERY`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::Config;
use crate::devsbd::notify::{Level, Record};
use crate::inbox::SinkAction;
use crate::runtime::{backend, bounded};
use crate::state::{Instance, State, UpdateRecord};
use crate::updates::Interval;

pub const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

/// The gap after a failed (or interrupted) attempt before the next one,
/// whatever the interval.
pub const RETRY: Duration = Duration::from_secs(6 * 60 * 60);

/// One update's subprocess is killed past this.
pub const TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The Inbox owner of the failure notices: not an instance (instance ids
/// never hold a `:`), so no `rm` archives them.
pub const INBOX_OWNER: &str = "devsandbox:updates";

/// `State.updates`.
pub type Records = BTreeMap<String, BTreeMap<String, UpdateRecord>>;

/// What one config root declares, as the pass needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// `services::project_id`: the `State.updates` key.
    pub project: String,
    /// Each update's interval; `None` when the config turns it off.
    pub updates: Vec<(String, Option<Duration>)>,
}

/// The config's interval for an update, else the feature's `every`.
pub fn interval(config: Option<Interval>, every: Duration) -> Option<Duration> {
    match config {
        Some(Interval::Never) => None,
        Some(Interval::Every(d)) => Some(d),
        None => Some(every),
    }
}

/// The last attempt failed or never finished: a success at least as recent
/// clears it.
fn failing(record: &UpdateRecord) -> bool {
    record.tried.is_some_and(|tried| record.ok.is_none_or(|ok| ok < tried))
}

/// Due once `every` has passed since the last success, and, while failing,
/// [`RETRY`] since the last attempt.
pub fn due(every: Duration, record: &UpdateRecord, now: u64) -> bool {
    let since = |t: Option<u64>| t.map_or(u64::MAX, |t| now.saturating_sub(t));
    since(record.ok) >= every.as_secs() && (!failing(record) || since(record.tried) >= RETRY.as_secs())
}

/// Claim a run of `name` at `now` if it's still [`due`] in `records` (the
/// state under its lock): the record before the claim, or `None`.
pub fn claim(records: &mut Records, project: &str, name: &str, every: Duration, now: u64) -> Option<UpdateRecord> {
    let record = records.entry(project.to_string()).or_default().entry(name.to_string()).or_default();
    let before = *record;
    if !due(every, &before, now) {
        return None;
    }
    record.tried = Some(now);
    Some(before)
}

/// The pass's I/O, injectable for tests.
pub trait Env {
    fn now(&self) -> u64;
    fn roots(&mut self) -> Vec<PathBuf>;
    fn declared(&mut self, root: &Path) -> Result<Root>;
    /// A snapshot of the records, to skip what isn't due without the lock.
    fn records(&mut self) -> Records;
    /// Whether anything can run now (the runtime answers).
    fn ready(&mut self) -> Result<(), String>;
    /// [`claim`] under the state lock.
    fn claim(&mut self, project: &str, name: &str, every: Duration, now: u64) -> Result<Option<UpdateRecord>>;
    fn run(&mut self, root: &Path, name: &str) -> Result<(), String>;
    fn notify(&mut self, root: &Path, name: &str, why: &str);
    /// The daemon is stopping: the pass ends, a run it cut short is no
    /// failure to report.
    fn stopped(&self) -> bool;
}

/// One round: every due update of every root, one at a time. A failure is
/// logged; the first of a streak also goes to the Inbox.
pub fn pass(env: &mut impl Env, log: &dyn Fn(&str)) {
    let mut ready: Option<bool> = None;
    for root in env.roots() {
        let declared = match env.declared(&root) {
            Ok(declared) => declared,
            Err(e) => {
                log(&format!("updates: skipping {}: {e:#}", root.display()));
                continue;
            }
        };
        let records = env.records();
        for (name, every) in &declared.updates {
            let Some(every) = *every else { continue };
            let record = records.get(&declared.project).and_then(|r| r.get(name)).copied().unwrap_or_default();
            if !due(every, &record, env.now()) {
                continue;
            }
            if env.stopped() {
                return;
            }
            // Asked once a run is due, so a pass with nothing to do asks nothing.
            let ok = *ready.get_or_insert_with(|| match env.ready() {
                Ok(()) => true,
                Err(why) => {
                    log(&format!("updates: skipped this round: {why}"));
                    false
                }
            });
            if !ok {
                return;
            }
            let before = match env.claim(&declared.project, name, every, env.now()) {
                Ok(Some(before)) => before,
                Ok(None) => continue,
                Err(e) => {
                    log(&format!("update `{name}` ({}): cannot claim it: {e:#}", root.display()));
                    continue;
                }
            };
            log(&format!("update `{name}` ({}): running", root.display()));
            match env.run(&root, name) {
                Ok(()) => log(&format!("update `{name}` ({}): done", root.display())),
                Err(why) => {
                    log(&format!("update `{name}` ({}): {why}", root.display()));
                    if env.stopped() {
                        return;
                    }
                    if !failing(&before) {
                        env.notify(&root, name, &why);
                    }
                }
            }
        }
    }
}

/// The real [`Env`]: `state.toml`, the config roots, a logged subprocess per
/// run, the Inbox store.
pub struct Live<'a> {
    /// Cuts a running update short (the daemon stopping).
    pub stopped: &'a dyn Fn() -> bool,
    /// Per root, the sandboxes with an instance: only their updates run, as
    /// an instance means its image is built.
    sandboxes: BTreeMap<PathBuf, BTreeSet<String>>,
}

impl<'a> Live<'a> {
    pub fn new(stopped: &'a dyn Fn() -> bool) -> Live<'a> {
        Live { stopped, sandboxes: BTreeMap::new() }
    }
}

impl Env for Live<'_> {
    fn now(&self) -> u64 {
        Instance::now()
    }

    fn roots(&mut self) -> Vec<PathBuf> {
        let Ok(state) = State::load() else { return Vec::new() };
        self.sandboxes = BTreeMap::new();
        for info in state.instances.values() {
            if let Some(root) = &info.config_dir {
                self.sandboxes.entry(root.clone()).or_default().insert(info.sandbox.clone());
            }
        }
        super::host::config_roots(&state)
    }

    fn declared(&mut self, root: &Path) -> Result<Root> {
        let config = Config::load(root)?;
        let project = crate::commands::services::project_id(root)?;
        let (declared, skipped) = crate::commands::update::declared(root, &config);
        for (sandbox, e) in skipped {
            eprintln!("updates: {}: sandbox `{sandbox}`: {e:#}", root.display());
        }
        let running = self.sandboxes.get(root);
        let updates = declared
            .into_iter()
            .filter(|(_, d)| d.sandboxes.iter().any(|s| running.is_some_and(|r| r.contains(s))))
            .map(|(name, d)| {
                let every = interval(config.updates.get(&name).copied(), d.update.every);
                (name, every)
            })
            .collect();
        Ok(Root { project, updates })
    }

    fn records(&mut self) -> Records {
        State::load().map(|s| s.updates).unwrap_or_default()
    }

    fn ready(&mut self) -> Result<(), String> {
        let b = backend();
        if b.name() == "container" {
            return Err(format!("updates on Apple's container: {}", crate::NOT_SUPPORTED_YET));
        }
        b.list(false, crate::runtime::NAME_PREFIX).map(|_| ()).map_err(|e| format!("the runtime is not reachable ({e:#})"))
    }

    fn claim(&mut self, project: &str, name: &str, every: Duration, now: u64) -> Result<Option<UpdateRecord>> {
        State::update(|state| Ok(claim(&mut state.updates, project, name, every, now)))
    }

    fn run(&mut self, root: &Path, name: &str) -> Result<(), String> {
        let exe = crate::exe::devsandbox().map_err(|e| format!("cannot find devsandbox: {e}"))?;
        let path = log_path(name).ok_or("cannot create the update log")?;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let _ = writeln!(log, "$ devsandbox --in-process -C {} update {name}", root.display());
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let mut note = log.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(exe);
        // Own process group, so a kill takes the runtime CLI with it.
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd
            // It runs for the daemon: never lazy-start one.
            .arg("--in-process")
            .arg("-C")
            .arg(root)
            .args(["update", name])
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err)
            .env(crate::runtime::RUNTIME_ENV, backend().name())
            .spawn()
            .map_err(|e| format!("cannot run devsandbox: {e}"))?;
        let status = match bounded::wait_until(&mut child, Instant::now() + TIMEOUT, self.stopped) {
            Ok(Some(status)) => status,
            Ok(None) => {
                bounded::kill_group(&mut child);
                let why = match (self.stopped)() {
                    true => "stopped with the daemon".to_string(),
                    false => format!("timed out after {}", bounded::human(TIMEOUT)),
                };
                let _ = writeln!(note, "devsandbox: {why}, killed");
                return Err(format!("{why}; log: {}", path.display()));
            }
            Err(e) => {
                bounded::kill_group(&mut child);
                return Err(format!("cannot wait for devsandbox: {e}"));
            }
        };
        if status.success() {
            return Ok(());
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        Err(format!("failed ({status}): {last}; log: {}", path.display()))
    }

    fn stopped(&self) -> bool {
        (self.stopped)()
    }

    fn notify(&mut self, root: &Path, name: &str, why: &str) {
        let record = Record {
            level: Level::Error,
            key: Some(format!("update-{name}")),
            link: None,
            msg: format!(
                "update `{name}` ({}) {why}; retried every {}h until it works",
                root.display(),
                RETRY.as_secs() / 3600
            ),
            at: Instance::now(),
        };
        let stored = crate::inbox::store::path()
            .and_then(|path| crate::inbox::ops::sink(&path, INBOX_OWNER, "devsandbox", SinkAction::Push(record)));
        if let Err(e) = stored {
            eprintln!("updates: cannot post to the Inbox: {e:#}");
        }
    }
}

/// `<data>/devsandbox/logs/update-<unix>-<name>.log`, its dir created.
fn log_path(name: &str) -> Option<PathBuf> {
    let dir = State::path().ok()?.parent()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("update-{}-{name}.log", Instance::now())))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const H: u64 = 3600;
    const DAY: Duration = Duration::from_secs(24 * H);

    fn rec(ok: Option<u64>, tried: Option<u64>) -> UpdateRecord {
        UpdateRecord { ok, tried }
    }

    #[test]
    fn intervals() {
        assert_eq!(interval(None, DAY), Some(DAY));
        assert_eq!(interval(Some(Interval::Never), DAY), None);
        assert_eq!(interval(Some(Interval::Every(Duration::from_secs(H))), DAY), Some(Duration::from_secs(H)));
    }

    #[test]
    fn due_after_the_interval_and_retry_gap() {
        let now = 100 * 24 * H;
        let hour = Duration::from_secs(H);
        // Never run, never tried.
        assert!(due(DAY, &rec(None, None), now));
        // Ran recently; due once the interval has passed.
        assert!(!due(DAY, &rec(Some(now - 23 * H), Some(now - 23 * H)), now));
        assert!(due(DAY, &rec(Some(now - 24 * H), Some(now - 24 * H)), now));
        // A success clears the retry gap: an hourly update runs hourly.
        assert!(due(hour, &rec(Some(now - H), Some(now - H - 10)), now));
        // Failed (or still running): retried only after the gap.
        assert!(!due(hour, &rec(Some(now - 30 * H), Some(now - 5 * H)), now));
        assert!(due(hour, &rec(Some(now - 30 * H), Some(now - 6 * H)), now));
        assert!(!due(DAY, &rec(None, Some(now - 5 * H)), now));
        assert!(due(DAY, &rec(None, Some(now - 6 * H)), now));
        // A clock that went back is not due.
        assert!(!due(DAY, &rec(Some(now + 10), None), now));
    }

    #[test]
    fn claim_records_the_attempt_only_when_due() {
        let mut records = Records::new();
        assert_eq!(claim(&mut records, "p", "gh", DAY, 1000), Some(UpdateRecord::default()));
        assert_eq!(records["p"]["gh"], rec(None, Some(1000)));
        // A second daemon racing it finds it claimed.
        assert_eq!(claim(&mut records, "p", "gh", DAY, 1001), None);
        assert_eq!(records["p"]["gh"], rec(None, Some(1000)));
    }

    struct Fake {
        now: RefCell<u64>,
        roots: Vec<(PathBuf, Result<Root, String>)>,
        records: Records,
        ready: Result<(), String>,
        fail: BTreeSet<String>,
        /// Stops the daemon during the run of this update.
        stop_in: Option<String>,
        stop: bool,
        runs: Vec<String>,
        notices: Vec<String>,
    }

    impl Fake {
        fn new(updates: &[(&str, Option<Duration>)]) -> Fake {
            let updates = updates.iter().map(|(n, d)| (n.to_string(), *d)).collect();
            Fake {
                now: RefCell::new(1_000_000),
                roots: vec![(PathBuf::from("/cfg"), Ok(Root { project: "p".into(), updates }))],
                records: Records::new(),
                ready: Ok(()),
                fail: BTreeSet::new(),
                stop_in: None,
                stop: false,
                runs: Vec::new(),
                notices: Vec::new(),
            }
        }

        fn advance(&self, secs: u64) {
            *self.now.borrow_mut() += secs;
        }
    }

    impl Env for Fake {
        fn now(&self) -> u64 {
            *self.now.borrow()
        }
        fn roots(&mut self) -> Vec<PathBuf> {
            self.roots.iter().map(|(r, _)| r.clone()).collect()
        }
        fn declared(&mut self, root: &Path) -> Result<Root> {
            let (_, found) = self.roots.iter().find(|(r, _)| r == root).unwrap();
            found.clone().map_err(|e| anyhow::anyhow!(e))
        }
        fn records(&mut self) -> Records {
            self.records.clone()
        }
        fn ready(&mut self) -> Result<(), String> {
            self.ready.clone()
        }
        fn claim(&mut self, project: &str, name: &str, every: Duration, now: u64) -> Result<Option<UpdateRecord>> {
            Ok(claim(&mut self.records, project, name, every, now))
        }
        fn run(&mut self, _root: &Path, name: &str) -> Result<(), String> {
            self.runs.push(name.to_string());
            // The verb takes time and records its own success.
            self.advance(60);
            if self.stop_in.as_deref() == Some(name) {
                self.stop = true;
                return Err("stopped with the daemon".into());
            }
            if self.fail.contains(name) {
                return Err("failed (exit 1)".into());
            }
            let now = self.now();
            self.records.get_mut("p").unwrap().get_mut(name).unwrap().ok = Some(now);
            Ok(())
        }
        fn notify(&mut self, root: &Path, name: &str, why: &str) {
            self.notices.push(format!("{} {name}: {why}", root.display()));
        }
        fn stopped(&self) -> bool {
            self.stop
        }
    }

    fn quiet(_: &str) {}

    #[test]
    fn pass_runs_what_is_due_and_skips_never() {
        let mut env = Fake::new(&[("gh", Some(DAY)), ("off", None), ("node", Some(DAY))]);
        let now = env.now();
        env.records.entry("p".into()).or_default().insert("node".into(), rec(Some(now - H), None));
        pass(&mut env, &quiet);
        assert_eq!(env.runs, ["gh"]);
        assert!(env.records["p"]["gh"].ok.is_some());
        assert!(!env.records["p"].contains_key("off"));

        // Nothing due an hour later; both once a day has passed.
        env.advance(H);
        pass(&mut env, &quiet);
        assert_eq!(env.runs, ["gh"]);
        env.advance(24 * H);
        pass(&mut env, &quiet);
        assert_eq!(env.runs, ["gh", "gh", "node"]);
        assert!(env.notices.is_empty());
    }

    #[test]
    fn failure_retries_after_the_gap_and_notifies_once_per_streak() {
        let mut env = Fake::new(&[("gh", Some(DAY))]);
        env.fail.insert("gh".into());
        pass(&mut env, &quiet);
        assert_eq!(env.runs.len(), 1);
        assert_eq!(env.records["p"]["gh"].ok, None);
        assert_eq!(env.notices, ["/cfg gh: failed (exit 1)"]);

        env.advance(5 * H);
        pass(&mut env, &quiet);
        assert_eq!(env.runs.len(), 1);
        env.advance(H);
        pass(&mut env, &quiet);
        assert_eq!(env.runs.len(), 2);
        // Same streak: no second notice.
        assert_eq!(env.notices.len(), 1);

        // A success ends the streak; the next failure notifies again.
        env.fail.clear();
        env.advance(6 * H);
        pass(&mut env, &quiet);
        assert_eq!(env.runs.len(), 3);
        assert!(env.records["p"]["gh"].ok.is_some());
        env.fail.insert("gh".into());
        env.advance(24 * H);
        pass(&mut env, &quiet);
        assert_eq!(env.runs.len(), 4);
        assert_eq!(env.notices.len(), 2);
    }

    #[test]
    fn pass_skips_a_bad_root_and_waits_for_the_runtime() {
        let mut env = Fake::new(&[("gh", Some(DAY))]);
        env.roots.insert(0, (PathBuf::from("/bad"), Err("invalid config".into())));
        env.ready = Err("down".into());
        let lines = RefCell::new(Vec::new());
        let log = |l: &str| lines.borrow_mut().push(l.to_string());
        pass(&mut env, &log);
        assert!(env.runs.is_empty());
        // Not claimed: the next round after the runtime is back runs it.
        assert!(env.records.is_empty());
        assert_eq!(*lines.borrow(), ["updates: skipping /bad: invalid config", "updates: skipped this round: down"]);

        env.ready = Ok(());
        pass(&mut env, &log);
        assert_eq!(env.runs, ["gh"]);
    }

    #[test]
    fn a_stop_ends_the_pass_without_a_notice() {
        let mut env = Fake::new(&[("a", Some(DAY)), ("b", Some(DAY))]);
        env.stop_in = Some("a".into());
        pass(&mut env, &quiet);
        assert_eq!(env.runs, ["a"]);
        assert!(env.notices.is_empty());
        // Its claim stays: retried after the gap, like a failure.
        assert_eq!(env.records["p"]["a"].tried, Some(1_000_000));
        assert!(!env.records["p"].contains_key("b"));
    }

    #[test]
    fn claimed_elsewhere_is_left_alone() {
        let mut env = Fake::new(&[("gh", Some(DAY))]);
        // Another daemon claimed it after this one's snapshot.
        struct Racing(Fake);
        impl Env for Racing {
            fn now(&self) -> u64 {
                self.0.now()
            }
            fn roots(&mut self) -> Vec<PathBuf> {
                self.0.roots()
            }
            fn declared(&mut self, root: &Path) -> Result<Root> {
                self.0.declared(root)
            }
            fn records(&mut self) -> Records {
                let snapshot = self.0.records();
                let now = self.0.now();
                claim(&mut self.0.records, "p", "gh", DAY, now);
                snapshot
            }
            fn ready(&mut self) -> Result<(), String> {
                self.0.ready()
            }
            fn claim(&mut self, project: &str, name: &str, every: Duration, now: u64) -> Result<Option<UpdateRecord>> {
                self.0.claim(project, name, every, now)
            }
            fn run(&mut self, root: &Path, name: &str) -> Result<(), String> {
                self.0.run(root, name)
            }
            fn notify(&mut self, root: &Path, name: &str, why: &str) {
                self.0.notify(root, name, why)
            }
            fn stopped(&self) -> bool {
                false
            }
        }
        env.records.clear();
        let mut racing = Racing(env);
        pass(&mut racing, &quiet);
        assert!(racing.0.runs.is_empty());
    }
}
