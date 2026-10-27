//! `lend.get` / `lend.set` (docs/architecture/daemon/fleet.md, *Lending:
//! `lend.*`*) and the daemon's live lending state behind them: `lend.toml`
//! is written by the daemon only (decision 3 of the fleet plan), and every
//! `lend.set` is applied at once, without watching the file: the lending
//! thread (`fleet::worker::Lending`) starts, stops, or is replaced by one to
//! the new hub or name. `on` ↔ `drain` keeps the link up (what's placed here
//! stays reachable); the next link offers the new state.
//!
//! How a lending thread is started is the daemon's ([`Spawn`]: the fleet
//! root's `Worker` connections, the transport), so tests drive a `Lender`
//! with an injected transport and no daemon.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::lend::{self, Lend, State};
use super::worker::{Lending, Status};
use crate::serve::api::{ApiError, parse};
use crate::serve::machines::check_name;

type Answer = Result<Value, ApiError>;

/// What a lending thread is started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub hub: String,
    pub name: String,
    /// The fleet root, created and canonical.
    pub root: PathBuf,
    pub draining: bool,
}

/// How the daemon starts a lending thread.
pub type Spawn = Box<dyn Fn(Start) -> Result<Lending> + Send + Sync>;

/// The link's state as `lend.get` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkState {
    /// Not lending (or lending couldn't start: `error` says why).
    Off,
    Connecting,
    Connected,
    /// The last attempt failed or the link ended: `error` says why.
    Retrying,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Off => "off",
            LinkState::Connecting => "connecting",
            LinkState::Connected => "connected",
            LinkState::Retrying => "retrying",
        }
    }
}

/// `lend.get`'s answer (and `lend.set`'s, after the change).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LendInfo {
    pub state: State,
    /// The hub's ssh target; `None` when none was ever set.
    pub hub: Option<String>,
    /// The name this host offers itself under: `lend.toml`'s, else the
    /// hostname's first label; `None` when neither is a valid name.
    pub name: Option<String>,
    /// `<data>/devsandbox/fleet/<hub>/`: canonical while lending.
    pub fleet_root: Option<PathBuf>,
    pub link: LinkState,
    /// Why the link is `retrying`, or `off` while the file says lend.
    pub error: Option<String>,
}

/// `lend.set`'s params: each one present is changed, the rest kept.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetParams {
    #[serde(default)]
    pub state: Option<State>,
    /// An ssh target, as `machine add` takes one.
    #[serde(default)]
    pub hub: Option<String>,
    /// A machine name; `""` clears it (the hostname's first label).
    #[serde(default)]
    pub name: Option<String>,
}

/// `lend` with `p` applied, checked (pure).
pub fn merge(mut lend: Lend, p: &SetParams) -> Result<Lend, ApiError> {
    if let Some(hub) = &p.hub {
        if hub.trim().is_empty() {
            return Err(ApiError::invalid("`hub` is empty"));
        }
        lend.hub = hub.clone();
    }
    if let Some(name) = &p.name {
        if !name.is_empty() {
            check_name(name).map_err(|e| ApiError::invalid(format!("`name`: {e:#}")))?;
        }
        lend.name = name.clone();
    }
    if let Some(state) = p.state {
        lend.state = state;
    }
    if lend.lends() && lend.hub.is_empty() {
        return Err(ApiError::invalid("no hub to lend to: `devsandbox fleet join <hub>` sets one"));
    }
    lend::check(&lend).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    Ok(lend)
}

/// What applying a `lend.toml` does to the lending thread (pure), given the
/// running one's and the wanted `(hub, name)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// Nothing runs and nothing should, or the right one runs (its
    /// `draining` is updated).
    Keep,
    Start,
    Stop,
    /// Another hub or name: the old link stops (its ssh killed), a new one
    /// starts.
    Restart,
}

pub fn transition(running: Option<(&str, &str)>, want: Option<(&str, &str)>) -> Transition {
    match (running, want) {
        (None, None) => Transition::Keep,
        (None, Some(_)) => Transition::Start,
        (Some(_), None) => Transition::Stop,
        (Some(r), Some(w)) if r == w => Transition::Keep,
        (Some(_), Some(_)) => Transition::Restart,
    }
}

struct Live {
    hub: String,
    name: String,
    root: PathBuf,
    lending: Lending,
}

#[derive(Default)]
struct Slot {
    live: Option<Live>,
    /// Why lending didn't start, while the file says lend.
    error: Option<String>,
    /// The daemon is exiting: nothing starts again.
    closed: bool,
}

/// The daemon's lending: `lend.toml` and the thread that follows it.
pub struct Lender {
    path: PathBuf,
    hostname: String,
    spawn: Spawn,
    slot: Mutex<Slot>,
    log: fn(&str),
}

impl Lender {
    /// `path` is `lend.toml` (its dir holds the fleet roots); `hostname`
    /// names this worker when the file doesn't.
    pub fn new(path: PathBuf, hostname: String, spawn: Spawn, log: fn(&str)) -> Lender {
        Lender { path, hostname, spawn, slot: Mutex::default(), log }
    }

    /// At daemon start: lend as the file says. A bad file (or hostname,
    /// without `name`) is logged and nothing lends.
    pub fn start(&self) {
        let lend = match lend::load(&self.path) {
            Ok(lend) => lend.unwrap_or_default(),
            Err(e) => {
                (self.log)(&format!("warning: not lending: {e:#}"));
                return;
            }
        };
        let mut slot = self.slot.lock().unwrap();
        if let Err(e) = self.apply(&mut slot, &lend) {
            (self.log)(&format!("warning: not lending: {e:#}"));
        }
    }

    /// Whether a lending thread runs: a lending daemon never idles out.
    pub fn lends(&self) -> bool {
        self.slot.lock().unwrap().live.is_some()
    }

    /// Whether the running lending thread is draining (`lend drain`), as
    /// applied now, not as the file says: a hub's `run` is refused then.
    pub fn draining(&self) -> bool {
        self.slot.lock().unwrap().live.as_ref().is_some_and(|l| l.lending.draining())
    }

    /// Stop lending for good (the daemon exiting, after its drain).
    pub fn stop(&self) {
        let live = {
            let mut slot = self.slot.lock().unwrap();
            slot.closed = true;
            slot.live.take()
        };
        drop(live);
    }

    /// `lend.toml` as it is now, for the capacity report: `None` when it's
    /// missing or doesn't parse. Read each time, so a hand edit of `[caps]`
    /// shows on the next poll.
    pub fn file(&self) -> Option<Lend> {
        lend::load(&self.path).ok().flatten()
    }

    pub fn get(&self) -> Result<LendInfo, ApiError> {
        let lend = lend::load(&self.path).map_err(ApiError::internal)?.unwrap_or_default();
        let slot = self.slot.lock().unwrap();
        Ok(self.info(&lend, &slot))
    }

    /// `lend.set`: `p` merged into the file, checked, written, applied.
    pub fn set(&self, p: SetParams) -> Result<LendInfo, ApiError> {
        let mut slot = self.slot.lock().unwrap();
        if slot.closed {
            return Err(ApiError::failed("the daemon is exiting"));
        }
        let next = lend::update(&self.path, |cur| -> Result<Lend, ApiError> {
            let next = merge(cur.clone(), &p)?;
            if next.lends() {
                self.name_of(&next).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
            }
            *cur = next.clone();
            Ok(next)
        })
        .map_err(ApiError::internal)??;
        self.apply(&mut slot, &next).map_err(ApiError::internal)?;
        Ok(self.info(&next, &slot))
    }

    fn name_of(&self, lend: &Lend) -> Result<String> {
        match lend.name.as_str() {
            "" => lend::default_name(&self.hostname),
            name => Ok(name.to_string()),
        }
    }

    fn info(&self, lend: &Lend, slot: &Slot) -> LendInfo {
        let (link, error) = match &slot.live {
            None => (LinkState::Off, slot.error.clone()),
            Some(live) => match live.lending.status() {
                Status::Connecting => (LinkState::Connecting, None),
                Status::Connected => (LinkState::Connected, None),
                Status::Retrying(why) => (LinkState::Retrying, Some(why)),
            },
        };
        let fleet_root = match &slot.live {
            Some(live) => Some(live.root.clone()),
            None if lend.hub.is_empty() => None,
            None => self.path.parent().and_then(|data| lend::fleet_root(data, &lend.hub).ok()),
        };
        LendInfo {
            state: lend.state,
            hub: (!lend.hub.is_empty()).then(|| lend.hub.clone()),
            name: self.name_of(lend).ok(),
            fleet_root,
            link,
            error,
        }
    }

    /// Make the thread follow `lend`.
    fn apply(&self, slot: &mut Slot, lend: &Lend) -> Result<()> {
        slot.error = None;
        let want = if lend.lends() && !slot.closed { Some((lend.hub.clone(), self.name_of(lend)?)) } else { None };
        let running = slot.live.as_ref().map(|l| (l.hub.as_str(), l.name.as_str()));
        let draining = lend.state == State::Drain;
        match transition(running, want.as_ref().map(|(h, n)| (h.as_str(), n.as_str()))) {
            Transition::Keep => {
                if let Some(live) = &slot.live {
                    live.lending.set_draining(draining);
                }
                return Ok(());
            }
            Transition::Stop => {
                let old = slot.live.take();
                drop(old);
                (self.log)("fleet: lending stopped");
                return Ok(());
            }
            Transition::Start | Transition::Restart => {
                // The old link first: its ssh killed before the new one dials.
                let old = slot.live.take();
                drop(old);
            }
        }
        let (hub, name) = want.expect("a start wants a hub");
        let started = self.launch(&hub, &name, draining, lend.state);
        match started {
            Ok(live) => {
                slot.live = Some(live);
                Ok(())
            }
            Err(e) => {
                slot.error = Some(format!("{e:#}"));
                Err(e)
            }
        }
    }

    fn launch(&self, hub: &str, name: &str, draining: bool, state: State) -> Result<Live> {
        let data = self.path.parent().context("lend.toml has no dir")?;
        let root = fleet_root_created(data, hub)?;
        (self.log)(&format!("fleet: lending to {hub} as {name} ({}, fleet root {})", state.as_str(), root.display()));
        let lending = (self.spawn)(Start { hub: hub.to_string(), name: name.to_string(), root: root.clone(), draining })?;
        Ok(Live { hub: hub.to_string(), name: name.to_string(), root, lending })
    }
}

/// The fleet root for `hub` under `data`, created, canonical.
fn fleet_root_created(data: &Path, hub: &str) -> Result<PathBuf> {
    let root = lend::fleet_root(data, hub)?;
    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
    std::fs::canonicalize(&root).with_context(|| format!("cannot resolve {}", root.display()))
}

/// Run `method` if it's a `lend.*` one; `None` otherwise.
pub fn call(method: &str, params: Value, lender: &Lender) -> Option<Answer> {
    let to_value = |info: LendInfo| serde_json::to_value(info).map_err(|e| ApiError::internal(e.into()));
    let answer = match method {
        "lend.get" => lender.get().and_then(to_value),
        "lend.set" => parse(params).and_then(|p| lender.set(p)).and_then(to_value),
        _ => return None,
    };
    Some(answer)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;
    use crate::serve::fleet::worker::Transport;
    use crate::serve::machines::service::Timing;

    fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-lender-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Every dial's argv, each refused: no ssh, no hub.
    #[derive(Clone, Default)]
    struct Dials(Arc<Mutex<Vec<Vec<String>>>>);

    impl Dials {
        fn transport(&self) -> Transport {
            let dials = self.clone();
            Arc::new(move |argv: &[OsString]| {
                dials.0.lock().unwrap().push(argv.iter().map(|a| a.to_string_lossy().into_owned()).collect());
                anyhow::bail!("no route")
            })
        }

        fn last(&self) -> Option<Vec<String>> {
            self.0.lock().unwrap().last().cloned()
        }

        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    fn lender(path: PathBuf, dials: &Dials) -> Lender {
        let transport = dials.transport();
        let timing = Timing { backoff: Duration::from_millis(5), backoff_cap: Duration::from_millis(10), ..Timing::default() };
        let spawn: Spawn = Box::new(move |s: Start| {
            let follow = crate::serve::fleet::worker::Follow {
                steps: Arc::new(crate::serve::fleet::update::tests::Fake::default()),
                dev_builds: Arc::new(|| false),
            };
            Lending::spawn(s.hub, s.name, s.root, s.draining, transport.clone(), Arc::new(|| None), crate::serve::fleet::transfer::Receivers::new(|_| {}), follow, timing, |_| {})
        });
        Lender::new(path, "lap.local".into(), spawn, |_| {})
    }

    fn set(l: &Lender, params: Value) -> Result<LendInfo, ApiError> {
        l.set(serde_json::from_value(params).unwrap())
    }

    #[test]
    fn merge_checks_what_it_changes() {
        let p = |v: Value| -> SetParams { serde_json::from_value(v).unwrap() };
        let on = merge(Lend::default(), &p(json!({"hub": "me@hub", "state": "on"}))).unwrap();
        assert_eq!((on.state, on.hub.as_str(), on.name.as_str()), (State::On, "me@hub", ""));
        let named = merge(on.clone(), &p(json!({"name": "lap"}))).unwrap();
        assert_eq!((named.state, named.name.as_str()), (State::On, "lap"), "the rest kept");
        assert_eq!(merge(named, &p(json!({"name": ""}))).unwrap().name, "", "cleared");
        let code = |lend: &Lend, v: Value| merge(lend.clone(), &p(v)).unwrap_err().code;
        assert_eq!(code(&Lend::default(), json!({"state": "on"})), "invalid", "no hub");
        assert_eq!(code(&on, json!({"hub": " "})), "invalid");
        assert_eq!(code(&on, json!({"hub": "-oProxyCommand=x"})), "invalid");
        assert_eq!(code(&on, json!({"name": "a/b"})), "invalid");
        assert_eq!(merge(on.clone(), &p(json!({"state": "off"}))).unwrap().hub, "me@hub", "off keeps the hub");
        assert!(serde_json::from_value::<SetParams>(json!({"state": "maybe"})).is_err());
        assert!(serde_json::from_value::<SetParams>(json!({"hubb": "x"})).is_err());
    }

    #[test]
    fn transitions() {
        use Transition::*;
        let (a, b) = (("hub", "lap"), ("hub2", "lap"));
        assert_eq!(transition(None, None), Keep);
        assert_eq!(transition(None, Some(a)), Start);
        assert_eq!(transition(Some(a), None), Stop);
        assert_eq!(transition(Some(a), Some(a)), Keep);
        assert_eq!(transition(Some(a), Some(b)), Restart);
        assert_eq!(transition(Some(a), Some(("hub", "box"))), Restart, "a new name");
    }

    /// off → on dials, on → drain keeps the link (the next dial offers
    /// `--drain`), a new hub restarts it, off stops it; `lend.get` says the
    /// link's state all along.
    #[test]
    fn set_applies_live() {
        let dir = scratch("live");
        let path = dir.join("lend.toml");
        let dials = Dials::default();
        let l = lender(path.clone(), &dials);
        l.start();
        let off = l.get().unwrap();
        assert_eq!(off, LendInfo { state: State::Off, hub: None, name: Some("lap".into()), fleet_root: None, link: LinkState::Off, error: None });
        assert!(!l.lends() && !path.exists());

        let on = set(&l, json!({"hub": "me@hub", "state": "on"})).unwrap();
        assert_eq!(on.state, State::On);
        assert!(l.lends());
        let root = std::fs::canonicalize(dir.join("fleet/me@hub")).unwrap();
        assert_eq!(on.fleet_root, Some(root.clone()), "created, canonical");
        assert_eq!(lend::load(&path).unwrap().unwrap().hub, "me@hub", "written");
        wait_for(|| dials.count() >= 1, "a dial");
        let argv = dials.last().unwrap();
        assert_eq!(argv[5], "me@hub");
        assert!(argv[6].contains("'\\''--worker'\\'' '\\''lap'\\''"), "the hostname's label: {argv:?}");
        assert!(argv[6].contains(&format!("'\\''--root'\\'' '\\''{}'\\''", root.display())), "the fleet root: {argv:?}");
        wait_for(|| l.get().unwrap().link == LinkState::Retrying, "retrying");
        assert_eq!(l.get().unwrap().error.as_deref(), Some("no route"));

        let drain = set(&l, json!({"state": "drain"})).unwrap();
        assert_eq!((drain.state, drain.link), (State::Drain, LinkState::Retrying), "the same thread");
        wait_for(|| dials.last().unwrap()[6].contains("--drain"), "the next dial offers --drain");

        set(&l, json!({"hub": "me@hub2", "name": "box"})).unwrap();
        wait_for(|| dials.last().unwrap()[5] == "me@hub2", "a dial to the new hub");
        let argv = dials.last().unwrap();
        assert!(argv[6].contains("box") && argv[6].contains("--drain"), "{argv:?}");
        assert_eq!(l.get().unwrap().fleet_root, Some(std::fs::canonicalize(dir.join("fleet/me@hub2")).unwrap()));

        let off = set(&l, json!({"state": "off"})).unwrap();
        assert_eq!((off.link, off.hub.as_deref()), (LinkState::Off, Some("me@hub2")));
        assert!(!l.lends());
        let n = dials.count();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(dials.count(), n, "stopped dialing");

        // A refused set changes nothing.
        assert_eq!(set(&l, json!({"name": "a/b"})).unwrap_err().code, "invalid");
        assert_eq!(lend::load(&path).unwrap().unwrap().name, "box");

        // A new daemon lends as the file says.
        set(&l, json!({"state": "on"})).unwrap();
        l.stop();
        assert!(!l.lends());
        assert_eq!(set(&l, json!({"state": "off"})).unwrap_err().code, "failed", "exiting");
        let again = lender(path.clone(), &dials);
        again.start();
        assert!(again.lends());
        again.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hostname_that_is_no_name_needs_name() {
        let dir = scratch("host");
        let dials = Dials::default();
        let mut l = lender(dir.join("lend.toml"), &dials);
        l.hostname = "a b".into();
        assert_eq!(l.get().unwrap().name, None);
        let e = set(&l, json!({"hub": "hub", "state": "on"})).unwrap_err();
        assert!(e.code == "invalid" && e.message.contains("set `name`"), "{e:?}");
        assert!(!dir.join("lend.toml").exists(), "nothing written");
        assert!(set(&l, json!({"hub": "hub", "state": "on", "name": "lap"})).is_ok());
        l.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_wire_matches_the_typings() {
        use crate::serve::dts::{assert_matches, prop_type, union_members};
        let info = LendInfo {
            state: State::Drain,
            hub: Some("me@hub".into()),
            name: Some("lap".into()),
            fleet_root: Some("/d/fleet/me@hub".into()),
            link: LinkState::Retrying,
            error: Some("x".into()),
        };
        assert_matches("LendInfo", &serde_json::to_value(info).unwrap());
        assert_eq!(union_members("LendState"), ["'on'", "'off'", "'drain'"]);
        assert_eq!(union_members("LendLinkState"), ["'off'", "'connecting'", "'connected'", "'retrying'"]);
        assert!(prop_type("ApiMethods", "lend.get").is_some());
        assert!(prop_type("ApiMethods", "lend.set").is_some());
    }

    #[test]
    fn call_routes_lend_methods_only() {
        let dir = scratch("call");
        let l = lender(dir.join("lend.toml"), &Dials::default());
        assert_eq!(call("lend.get", Value::Null, &l).unwrap().unwrap()["link"], "off");
        assert_eq!(call("lend.set", json!({"state": 3}), &l).unwrap().unwrap_err().code, "invalid");
        assert!(call("lend.nope", Value::Null, &l).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
