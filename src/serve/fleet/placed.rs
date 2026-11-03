//! The daemon's `run` verb placed (docs/architecture/daemon/fleet.md,
//! *Placement*; fleet step 3c): a `run` that names no `machine` and asks
//! for it (`place`, `run --place`: [`asked`]; fleet plan decision 38) goes
//! where [`place::place`] says among this host, every
//! machine whose link is up (ssh or inbound) and every routed machine a
//! path reaches (fleet plan decision 60). On a
//! machine, its sandbox's code moves first, into this host's fleet root
//! there (`code::prepare`; [`fleet_root`] says where that is), then the
//! request is forwarded as a `machine` one would be, in the moved config
//! root.
//!
//! Pure but for the `prepare` the daemon passes [`decide`]: the
//! candidates ([`candidates`], each worker's fresh `capacity.get` answer
//! over its saved report, [`with_fresh`]), the cheap check that placing
//! can matter ([`worth_placing`]), the request the worker gets
//! ([`forwarded`]), its answer tagged ([`tag_answer`]) and the lines the
//! verb's output starts with.
//!
//! On a member (fleet plan decision 31) the hub decides: the member sends
//! `fleet.place` ([`PlaceParams`]: its spec, capacity and the machines it
//! reaches, over a link or a route), the hub scores those it reaches too
//! among them ([`for_member`]) and answers a [`PlaceAnswer`], and the
//! member moves the code and forwards over its own paths ([`by_hub`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::code::{Prepared, Unprepared};
use super::place::{self, Candidate, MachineRef, NoPlace, Placement, Spec};
use crate::serve::api::RootCandidate;
use crate::serve::capacity::Capacity;
use crate::serve::snapshots::{Machine, Routed};
use crate::serve::verbs;

/// The machines placement may pick besides this host: every one, whatever
/// its kind (the candidates are every neighbor); [`place::reachable`]
/// keeps those whose link is up.
pub fn workers(machines: &[Machine]) -> impl Iterator<Item = &Machine> {
    machines.iter()
}

/// This host's fleet root on a machine, from its `roots.list` answer over
/// the link (the row flagged `fleet`, `fleet::lent::with_fleet_root`):
/// where placement moves code to. `None` when none is flagged.
pub fn fleet_root(rows: &[RootCandidate]) -> Option<PathBuf> {
    rows.iter().find(|r| r.fleet).map(|r| r.root.clone())
}

/// How long a worker's `capacity.get` (its dial, hello and answer) may
/// take before its saved report is used instead.
pub const CAPACITY_WAIT: Duration = Duration::from_secs(2);

/// How long a member waits for the hub's `fleet.place` answer once its
/// hello is through: the hub asks its own machines' `capacity.get` first,
/// each up to twice [`CAPACITY_WAIT`].
pub const PLACE_WAIT: Duration = Duration::from_secs(6);

/// Whether the `run` request `params` asks to be placed (`place`, `run
/// --place`). A run the user typed without it runs on this host, whose
/// config it names (fleet plan decision 38): nothing asks the hub or
/// probes a worker's capacity for it.
pub fn asked(params: &Value) -> bool {
    params.get("place").and_then(Value::as_bool) == Some(true)
}

/// A placed run whose sandbox has `ties` (`code::ties`, fleet plan decision
/// 40) runs here with a note naming them, before the hub is asked or any
/// worker scored; none: placement goes on (`None`).
pub fn tied(ties: &[String]) -> Option<Placed> {
    (!ties.is_empty()).then(|| Placed::LocalWithNote(format!("note: not placed: {}; running here", ties.join(", "))))
}

/// The workers placement asks for a fresh report: the reachable ones
/// (their link up, or a `routed` one a path reaches), whatever their saved
/// report says.
pub fn reachable_workers(machines: &[Machine], routed: &[Routed]) -> Vec<String> {
    let direct = workers(machines).filter(|m| place::reachable(m.state)).map(|m| m.name.clone());
    direct.chain(routed.iter().filter(|r| place::reachable(r.state)).map(|r| r.name.clone())).collect()
}

/// Whether placing can pick anything but this host: some machine is
/// reachable. Checked first (no I/O: the links' and the graph's last
/// states), so a host with no machine linked pays nothing, not even a
/// `capacity.get`.
pub fn worth_placing(machines: &[Machine], routed: &[Routed]) -> bool {
    !reachable_workers(machines, routed).is_empty()
}

/// `c` with its machine's fresh report, when one came: its capacity
/// replaces the saved report's (pure). None came (a timeout, an error):
/// the saved one stands.
pub fn with_fresh(c: Candidate, fresh: Option<&Capacity>) -> Candidate {
    match fresh {
        Some(f) => Candidate { capacity: Some(f.clone()), ..c },
        None => c,
    }
}

/// What placement places: `sandbox`, its hashes from `root`'s snapshot
/// (`instances.snapshot` params; none known: no image locality), asked
/// for on this host, pinned nowhere.
pub fn spec(sandbox: &str, root: Option<&Value>) -> Spec {
    let row = root
        .and_then(|r| r.get("sandboxes")?.as_array()?.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(sandbox)));
    let hash = |k: &str| row.and_then(|s| s.get(k)?.as_str()).filter(|h| !h.is_empty()).map(str::to_owned);
    Spec {
        sandbox: sandbox.to_string(),
        config_hash: hash("config_hash"),
        build_hash: hash("build_hash"),
        repo: None,
        requester: MachineRef::Local,
        pinned: None,
    }
}

/// This host (its `capacity`, its watched roots' snapshots `local_roots`),
/// every worker among `machines` and every `routed` machine, each with its
/// `fresh` report by name ([`with_fresh`]).
pub fn candidates(
    spec: &Spec,
    capacity: Option<Capacity>,
    local_roots: &[Arc<Value>],
    machines: &[Machine],
    routed: &[Routed],
    fresh: &BTreeMap<String, Capacity>,
) -> Vec<Candidate> {
    let mut out = vec![Candidate::local(spec, capacity, local_roots.iter().map(|v| v.as_ref()))];
    out.extend(workers(machines).map(|m| with_fresh(Candidate::from_machine(spec, m), fresh.get(&m.name))));
    out.extend(routed.iter().map(|r| with_fresh(Candidate::from_routed(r), fresh.get(&r.name))));
    out
}

/// What the request asked that placement reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ask<'a> {
    /// `--branch`.
    pub branch: Option<&'a str>,
    /// `--base`.
    pub base: Option<&'a str>,
}

/// Where a `run` goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placed {
    /// Here, as without a fleet.
    Local,
    /// Here, after placement picked a worker the sandbox can't move to
    /// (or a member's hub couldn't place it, or the sandbox is tied here,
    /// [`tied`]): this note first in the verb's output.
    LocalWithNote(String),
    /// Here, where a member's hub placed it: the hub's [`lines`] first.
    Here(Vec<String>),
    /// On worker `name`, in its copy of the config root `root`; `base` is
    /// the `--base` it gets ([`start_point`]); `lines` start the output.
    Worker { name: String, root: PathBuf, base: Option<String>, lines: Vec<String> },
    /// Moving the code failed: the verb fails with this, it doesn't run
    /// anywhere else.
    Failed(String),
}

/// Where `run` goes: here when placement picks this host (or nothing,
/// which a candidate list with this host in it never gives); else `prepare(worker)` moves the code there, and its
/// answer decides: there, here with a note (`NotMovable`), or failed.
pub fn decide(
    ask: &Ask,
    spec: &Spec,
    candidates: &[Candidate],
    prepare: impl FnOnce(&str) -> Result<Prepared, Unprepared>,
) -> Placed {
    let Ok(placement) = place::place(spec, candidates) else { return Placed::Local };
    let MachineRef::Machine(name) = &placement.machine else { return Placed::Local };
    moved(ask, name, lines(&placement), prepare)
}

/// Placement picked worker `name` (`lines` start the output once there):
/// `prepare(name)` moves the code, and its answer decides: there, here
/// with a note (`NotMovable`), or failed. Shared by a hub's own placement
/// ([`decide`]) and a member's placed by the hub ([`by_hub`]).
pub fn moved(ask: &Ask, name: &str, lines: Vec<String>, prepare: impl FnOnce(&str) -> Result<Prepared, Unprepared>) -> Placed {
    match prepare(name) {
        Err(Unprepared::NotMovable(why)) => Placed::LocalWithNote(format!("note: not placed on {name}: {why}; running here")),
        Err(Unprepared::Failed(e)) => Placed::Failed(format!("cannot place on {name}: {e:#}")),
        Ok(prepared) => Placed::Worker {
            name: name.to_string(),
            root: prepared.remote_root.clone(),
            base: start_point(ask, &prepared),
            lines,
        },
    }
}

/// What a member's `fleet.place` names of its spec: the sandbox and its
/// hashes ([`spec`]); the hub fills in the requester.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceSpec {
    pub sandbox: String,
    pub config_hash: Option<String>,
    pub build_hash: Option<String>,
}

/// `fleet.place`'s params (docs/reference/api.md, *fleet*): the member's
/// spec, its own capacity now, and the machines it reaches
/// ([`reachable_workers`]: a connected link, or a route; its hub among
/// them): those it can move code to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceParams {
    pub spec: PlaceSpec,
    pub capacity: Capacity,
    pub reachable: Vec<String>,
}

impl PlaceParams {
    /// A member's ask: `spec` as [`spec`] built it.
    pub fn of(spec: &Spec, capacity: Capacity, reachable: Vec<String>) -> PlaceParams {
        let spec = PlaceSpec { sandbox: spec.sandbox.clone(), config_hash: spec.config_hash.clone(), build_hash: spec.build_hash.clone() };
        PlaceParams { spec, capacity, reachable }
    }
}

/// Where the hub placed a member's run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaceOn {
    /// The member that asked.
    Requester,
    /// The hub itself.
    Hub,
    /// Another machine (`PlaceAnswer::machine`).
    Machine,
}

/// `fleet.place`'s answer: where, by the hub's name for it (the
/// requester's, the hub's own member name, or the machine's), and the
/// lines the verb's output starts with ([`lines`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceAnswer {
    pub on: PlaceOn,
    pub machine: String,
    pub lines: Vec<String>,
}

/// The hub's machines `fleet.place` from `from` asks a fresh
/// `capacity.get`: those it reaches ([`reachable_workers`]: a link up
/// here, or a route), that the requester reaches too (`ask.reachable`),
/// the requester itself left out (it sent its own).
pub fn member_workers(ask: &PlaceParams, from: &str, machines: &[Machine], routed: &[Routed]) -> Vec<String> {
    reachable_workers(machines, routed).into_iter().filter(|m| m != from && ask.reachable.contains(m)).collect()
}

/// The hub's placement of member `from`'s run (pure): the candidates are
/// the requester (its own `capacity`, its image as the hub's view of its
/// rows shows; `place`'s [`place::REQUESTER`] bias), the hub `hub` itself
/// (`capacity`, its watched roots `hub_roots`) and each of
/// [`member_workers`] (among `machines` and the hub's `routed` ones) with
/// its `fresh` report. Every candidate is named as
/// the hub knows it, the hub by its member name, so the answer's lines
/// read the same on the member.
#[allow(clippy::too_many_arguments)]
pub fn for_member(
    ask: &PlaceParams,
    from: &str,
    hub: &str,
    capacity: Capacity,
    hub_roots: &[Arc<Value>],
    machines: &[Machine],
    routed: &[Routed],
    fresh: &BTreeMap<String, Capacity>,
) -> Result<PlaceAnswer, NoPlace> {
    let requester = MachineRef::Machine(from.to_string());
    let spec = Spec {
        sandbox: ask.spec.sandbox.clone(),
        config_hash: ask.spec.config_hash.clone(),
        build_hash: ask.spec.build_hash.clone(),
        repo: None,
        requester: requester.clone(),
        pinned: None,
    };
    // The request came over its link: reachable, whatever the last state.
    let asker = match machines.iter().find(|m| m.name == from) {
        Some(m) => Candidate { reachable: true, capacity: Some(ask.capacity.clone()), ..Candidate::from_machine(&spec, m) },
        None => Candidate { machine: requester.clone(), reachable: true, capacity: Some(ask.capacity.clone()), has_image: false, has_repo: false },
    };
    let this = Candidate { machine: MachineRef::Machine(hub.to_string()), ..Candidate::local(&spec, Some(capacity), hub_roots.iter().map(|v| v.as_ref())) };
    let others = member_workers(ask, from, machines, routed);
    let mut candidates = vec![asker, this];
    candidates.extend(
        workers(machines)
            .filter(|m| others.contains(&m.name))
            .map(|m| with_fresh(Candidate::from_machine(&spec, m), fresh.get(&m.name))),
    );
    candidates.extend(
        routed.iter().filter(|r| others.contains(&r.name)).map(|r| with_fresh(Candidate::from_routed(r), fresh.get(&r.name))),
    );
    let placement = place::place(&spec, &candidates)?;
    let MachineRef::Machine(name) = &placement.machine else { unreachable!("every candidate is named") };
    let on = if placement.machine == requester {
        PlaceOn::Requester
    } else if name == hub {
        PlaceOn::Hub
    } else {
        PlaceOn::Machine
    };
    Ok(PlaceAnswer { on, machine: name.clone(), lines: lines(&placement) })
}

/// Why a member's hub didn't place its run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unplaced {
    /// Its link is down, or the call didn't make it or timed out.
    Unreachable(String),
    /// It answered an error (or an answer this host can't read).
    Refused(String),
}

/// A member's `run` as hub `hub` answered it (`reachable`: the machines
/// this host reaches, [`reachable_workers`]): here (the hub's lines
/// first), or on a worker this host reaches, `prepare` moving the code
/// there as for a hub's own placement ([`moved`]). No answer, or a worker
/// this host can't reach: here, with a note naming the hub.
pub fn by_hub(
    ask: &Ask,
    hub: &str,
    answer: Result<PlaceAnswer, Unplaced>,
    reachable: &[String],
    prepare: impl FnOnce(&str) -> Result<Prepared, Unprepared>,
) -> Placed {
    let a = match answer {
        Ok(a) => a,
        Err(Unplaced::Unreachable(why)) => return Placed::LocalWithNote(format!("note: the hub `{hub}` is unreachable ({why}): running here")),
        Err(Unplaced::Refused(why)) => return Placed::LocalWithNote(format!("note: the hub `{hub}` didn't place it ({why}): running here")),
    };
    let name = match a.on {
        PlaceOn::Requester => return Placed::Here(a.lines),
        PlaceOn::Hub => hub.to_string(),
        PlaceOn::Machine => a.machine,
    };
    if !reachable.contains(&name) {
        return Placed::LocalWithNote(format!("note: the hub `{hub}` placed it on {name}, which this host has no path to: running here"));
    }
    moved(ask, &name, a.lines, prepare)
}

/// The worker's `--base`: the user's, else, for a `--branch` its origin
/// doesn't have (a new branch), the branch the bundle carried (the hub
/// checkout's own, `code::offer`), so the new branch starts at that tip.
/// The moved checkout has no `origin` to start one from, and its `HEAD`
/// is whichever branch first landed there.
pub fn start_point(ask: &Ask, prepared: &Prepared) -> Option<String> {
    if let Some(base) = ask.base {
        return Some(base.to_string());
    }
    let (_, _, sent) = prepared.folders.first()?;
    match ask.branch {
        Some(b) if b != sent => Some(sent.clone()),
        _ => None,
    }
}

/// `placed on <worker> (<reasons>)`, then each warning
/// (`warning: <worker>: over cap: …`); the over-cap note is the warning's,
/// not repeated among the reasons.
pub fn lines(p: &Placement) -> Vec<String> {
    let reasons: Vec<&str> = p.reasons.iter().map(String::as_str).filter(|r| !r.starts_with("over cap")).collect();
    let mut out = vec![if reasons.is_empty() {
        format!("placed on {}", p.machine)
    } else {
        format!("placed on {} ({})", p.machine, reasons.join(", "))
    }];
    out.extend(p.warnings.iter().map(|w| format!("warning: {w}")));
    out
}

/// The `run` request `params` as worker `worker` gets it: `machine` set
/// (the daemon forwards it as a `machine` request), `root` its copy of the
/// config root, no `place` (it runs there, never placed again), `base`
/// when [`start_point`] gave one.
pub fn forwarded(mut params: Value, worker: &str, root: &Path, base: Option<&str>) -> Value {
    if let Some(p) = params.as_object_mut() {
        p.insert("machine".into(), json!(worker));
        p.insert("root".into(), json!(root));
        p.remove("place");
        if let Some(base) = base {
            p.insert("base".into(), json!(base));
        }
    }
    params
}

/// A placed run's answer with `machine` (the worker) in its result, so the
/// client names the instance where it is (`lap/web-2`).
pub fn tag_answer(mut r: crate::serve::proto::Response, worker: &str) -> crate::serve::proto::Response {
    if let Some(Value::Object(result)) = &mut r.result {
        result.insert("machine".into(), json!(worker));
    }
    r
}

/// The client's [`verbs::Peer`] with `machine` (the worker) added to each
/// `progress`: the CLI learns where the instance is before the answer (its
/// Ctrl-C cancels it there, its name prints qualified).
pub struct Tagged<'a> {
    pub inner: &'a dyn verbs::Peer,
    pub machine: &'a str,
}

impl verbs::Peer for Tagged<'_> {
    fn notify(&self, method: &str, mut params: Value) -> std::io::Result<()> {
        if method == "progress"
            && let Some(p) = params.as_object_mut()
        {
            p.insert("machine".into(), json!(self.machine));
        }
        self.inner.notify(method, params)
    }

    fn hung_up(&self, tick: std::time::Duration) -> bool {
        self.inner.hung_up(tick)
    }

    fn draining(&self) -> bool {
        self.inner.draining()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;
    use crate::serve::capacity::Caps;
    use crate::serve::machines::MachineKind;
    use crate::serve::snapshots::LinkState;

    const G: u64 = 1 << 30;

    #[test]
    fn a_tied_sandbox_runs_here_with_its_ties_named() {
        assert_eq!(tied(&[]), None);
        assert_eq!(
            tied(&["`caches`".into(), "privileged".into()]),
            Some(Placed::LocalWithNote("note: not placed: `caches`, privileged; running here".into()))
        );
    }

    fn cap(avail: u64, running: usize, caps: Option<Caps>) -> Capacity {
        Capacity {
            cpus: Some(8),
            mem_total: Some(32 * G),
            mem_available: Some(avail * G),
            disk_free: Some(250 * G),
            disk_total: Some(500 * G),
            running,
            caps,
        }
    }

    fn worker(name: &str, kind: MachineKind, state: LinkState, capacity: Option<Capacity>) -> Machine {
        let root = PathBuf::from("/fleet/hub");
        let mut rows = BTreeMap::new();
        if let Some(c) = capacity {
            rows.insert(root.clone(), Arc::new(json!({ "dir": root, "capacity": c })));
        }
        Machine { name: name.into(), kind, state, roots: vec![root], rows, ..Machine::default() }
    }

    fn prepared(branch: &str) -> Prepared {
        Prepared {
            remote_root: "/fleet/hub/roots/0123456789abcdef/cfg".into(),
            folders: vec![("/w/web".into(), "/fleet/hub/roots/0123456789abcdef/web".into(), branch.into())],
        }
    }

    #[test]
    fn worth_placing_wants_a_connected_machine() {
        use LinkState::*;
        let roomy = || Some(cap(28, 0, None));
        assert!(!worth_placing(&[], &[]), "no machines");
        for kind in [MachineKind::Ssh, MachineKind::Inbound] {
            assert!(worth_placing(&[worker("m", kind, Connected, roomy())], &[]), "{kind:?}: every kind is a candidate");
            for down in [Idle, Connecting, Unreachable] {
                assert!(!worth_placing(&[worker("m", kind, down, roomy())], &[]), "{kind:?} {down:?}: its link is down");
            }
        }
        // No saved report: asked afresh either way.
        assert!(worth_placing(&[worker("lap", MachineKind::Inbound, Connected, None)], &[]));
        let all = [
            worker("a", MachineKind::Inbound, Connected, None),
            worker("b", MachineKind::Inbound, Unreachable, None),
            worker("c", MachineKind::Inbound, Connected, None),
            worker("d", MachineKind::Ssh, Connected, None),
            worker("e", MachineKind::Ssh, Idle, None),
        ];
        assert_eq!(reachable_workers(&all, &[]), ["a", "c", "d"]);
    }

    #[test]
    fn the_fleet_root_is_the_flagged_row() {
        let row = |root: &str, fleet| RootCandidate { root: root.into(), sandboxes: vec![], error: None, fleet };
        assert_eq!(fleet_root(&[row("/own", false), row("/d/fleet/hub", true)]), Some(PathBuf::from("/d/fleet/hub")));
        assert_eq!(fleet_root(&[row("/own", false)]), None);
        // As the wire has it: absent is unflagged.
        let rows: Vec<RootCandidate> = serde_json::from_value(json!([
            {"root": "/own", "sandboxes": [], "error": null},
            {"root": "/d/fleet/hub", "sandboxes": [], "error": null, "fleet": true},
        ]))
        .unwrap();
        assert_eq!(fleet_root(&rows), Some(PathBuf::from("/d/fleet/hub")));
        crate::serve::dts::assert_matches("RootCandidate", &serde_json::to_value(&rows[1]).unwrap());
        crate::serve::dts::assert_matches("RootCandidate", &serde_json::to_value(&rows[0]).unwrap());
    }

    /// A fresh `capacity.get` answer over the saved report; none: the saved one.
    #[test]
    fn a_fresh_report_replaces_the_saved_one() {
        let s = spec("web", None);
        let saved = cap(4, 6, None);
        let m = worker("lap", MachineKind::Inbound, LinkState::Connected, Some(saved.clone()));
        let fresh = cap(28, 0, None);
        let c = with_fresh(Candidate::from_machine(&s, &m), Some(&fresh));
        assert_eq!(c.capacity.as_ref(), Some(&fresh));
        let c = with_fresh(Candidate::from_machine(&s, &m), None);
        assert_eq!(c.capacity.as_ref(), Some(&saved));
        // Through `candidates`, by name; the fresh one decides the placement.
        let neutral = worker("lap", MachineKind::Inbound, LinkState::Connected, None);
        let stale = candidates(&s, None, &[], std::slice::from_ref(&neutral), &[], &BTreeMap::new());
        assert_eq!(decide(&Ask::default(), &s, &stale, |_| unreachable!()), Placed::Local, "neutral both: this host's bias");
        let fresh_map = BTreeMap::from([("lap".to_string(), fresh)]);
        let c = candidates(&s, None, &[], &[neutral], &[], &fresh_map);
        assert!(matches!(decide(&Ask::default(), &s, &c, |_| Ok(prepared("main"))), Placed::Worker { .. }));
    }

    #[test]
    fn the_spec_takes_the_roots_hashes() {
        let root = json!({"dir": "/r", "sandboxes": [{"name": "api", "config_hash": "c9"}, {"name": "web", "config_hash": "c1", "build_hash": ""}]});
        let s = spec("web", Some(&root));
        assert_eq!((s.config_hash.as_deref(), s.build_hash, s.requester, s.pinned), (Some("c1"), None, MachineRef::Local, None));
        assert_eq!(spec("web", None).config_hash, None);
        assert_eq!(spec("nope", Some(&root)).config_hash, None);
    }

    #[test]
    fn candidates_are_this_host_and_every_machine() {
        let machines = [
            worker("srv", MachineKind::Ssh, LinkState::Connected, None),
            worker("lap", MachineKind::Inbound, LinkState::Connected, None),
            worker("nb", MachineKind::Inbound, LinkState::Connected, None),
            worker("off", MachineKind::Ssh, LinkState::Unreachable, None),
        ];
        let c = candidates(&spec("web", None), Some(cap(16, 2, None)), &[], &machines, &[], &BTreeMap::new());
        let names: Vec<_> = c.iter().map(|c| (c.machine.to_string(), c.reachable)).collect();
        let want = [("this host", true), ("srv", true), ("lap", true), ("nb", true), ("off", false)];
        assert_eq!(names, want.map(|(n, r)| (n.to_string(), r)));

        // A hub's view of its machines (on a member) adds no candidate of
        // itself: those are the routed ones (`Handle::routed`), passed apart.
        let mut hub = worker("k", MachineKind::Ssh, LinkState::Connected, None);
        hub.fleet = Some(Arc::new(json!({"machines": [{"name": "x", "state": "connected"}], "instances": []})));
        let seen = candidates(&spec("web", None), None, &[], std::slice::from_ref(&hub), &[], &BTreeMap::new());
        assert_eq!(seen.iter().map(|c| c.machine.to_string()).collect::<Vec<_>>(), ["this host", "k"]);
        assert_eq!(reachable_workers(std::slice::from_ref(&hub), &[]), ["k"]);
        assert_eq!(c[0].capacity.as_ref().map(|c| c.running), Some(2));

        // An inbound or ssh machine with more room wins over a busy host.
        let over = Some(Caps { instances: Some(1), ..Caps::default() });
        for kind in [MachineKind::Inbound, MachineKind::Ssh] {
            let roomy = [worker("m", kind, LinkState::Connected, Some(cap(28, 0, None)))];
            let c = candidates(&spec("web", None), Some(cap(4, 3, over.clone())), &[], &roomy, &[], &BTreeMap::new());
            let placed = decide(&Ask::default(), &spec("web", None), &c, |_| Ok(prepared("main")));
            assert!(matches!(&placed, Placed::Worker { name, .. } if name == "m"), "{kind:?}: {placed:?}");
        }
    }

    fn routed(name: &str, state: LinkState, capacity: Option<Capacity>) -> Routed {
        Routed { name: name.into(), via: Some("k".into()), state, capacity, ..Routed::default() }
    }

    /// Routed machines (no link of their own, a path in the graph) are
    /// workers too: reachable while a path starts with a link up here,
    /// scored by the hub view's capacity or a fresh one, picked and moved
    /// to like any other.
    #[test]
    fn routed_machines_are_candidates() {
        let machines = [worker("k", MachineKind::Ssh, LinkState::Connected, None)];
        let r = [routed("b", LinkState::Connected, None), routed("far", LinkState::Unreachable, Some(cap(30, 0, None)))];
        assert_eq!(reachable_workers(&machines, &r), ["k", "b"]);
        assert!(worth_placing(&[], &r[..1]), "a routed one alone is worth placing");
        assert!(!worth_placing(&[], &r[1..]), "no path up: not");
        let s = spec("web", None);
        let c = candidates(&s, Some(cap(16, 2, None)), &[], &machines, &r, &BTreeMap::new());
        let names: Vec<_> = c.iter().map(|c| (c.machine.to_string(), c.reachable, c.has_image)).collect();
        assert_eq!(names, [("this host", true, false), ("k", true, false), ("b", true, false), ("far", false, false)].map(|(n, r, i)| (n.to_string(), r, i)));
        assert_eq!(c[3].capacity.as_ref().map(|c| c.running), Some(0), "the hub view's report");

        // A busy host, b roomy by its fresh report: placed on b, moved there.
        let over = Some(Caps { instances: Some(1), ..Caps::default() });
        let fresh = BTreeMap::from([("b".to_string(), cap(28, 0, None))]);
        let c = candidates(&s, Some(cap(4, 3, over.clone())), &[], &[], &r, &fresh);
        let placed = decide(&Ask::default(), &s, &c, |w| {
            assert_eq!(w, "b");
            Ok(prepared("main"))
        });
        assert!(matches!(&placed, Placed::Worker { name, .. } if name == "b"), "{placed:?}");
        // Unreachable, however roomy: never picked.
        let c = candidates(&s, Some(cap(4, 3, over)), &[], &[], &r[1..], &BTreeMap::new());
        assert_eq!(decide(&Ask::default(), &s, &c, |_| unreachable!()), Placed::Local);
    }

    /// On the hub: a member's run may go to a machine the hub reaches over
    /// a route, when the member reaches it too (it says so in `reachable`).
    #[test]
    fn the_hub_places_on_a_routed_machine_both_reach() {
        let over = Some(Caps { instances: Some(1), ..Caps::default() });
        let machines = [worker("lap", MachineKind::Inbound, LinkState::Connected, None)];
        let r = [routed("b", LinkState::Connected, None), routed("c", LinkState::Connected, None)];
        let a = PlaceParams::of(&spec("web", None), cap(4, 3, over.clone()), names(&["box", "b"]));
        assert_eq!(member_workers(&a, "lap", &machines, &r), ["b"], "c: lap doesn't reach it");
        let fresh = BTreeMap::from([("b".to_string(), cap(30, 0, None)), ("c".to_string(), cap(31, 0, None))]);
        let got = for_member(&a, "lap", "box", cap(4, 3, over), &[], &machines, &r, &fresh).unwrap();
        assert_eq!((got.on, got.machine.as_str()), (PlaceOn::Machine, "b"));
        // The member moves the code there: b is among what it reaches.
        let Placed::Worker { name, .. } = by_hub(&Ask::default(), "box", Ok(got), &a.reachable, |_| Ok(prepared("main"))) else { panic!() };
        assert_eq!(name, "b");
    }

    /// The decision over (request, candidates), `prepare` faked: what it
    /// was asked and what it answered.
    #[test]
    fn the_decision() {
        let over = Some(Caps { instances: Some(1), ..Caps::default() });
        let busy_here = || candidates(&spec("web", None), Some(cap(4, 3, over.clone())), &[], &[worker("lap", MachineKind::Inbound, LinkState::Connected, Some(cap(28, 0, None)))], &[], &BTreeMap::new());
        let idle_here = || candidates(&spec("web", None), Some(cap(28, 0, None)), &[], &[worker("lap", MachineKind::Inbound, LinkState::Connected, Some(cap(4, 6, None)))], &[], &BTreeMap::new());
        let asked = RefCell::new(Vec::new());
        let ok = |branch: &'static str| {
            let asked = &asked;
            move |w: &str| {
                asked.borrow_mut().push(w.to_string());
                Ok(prepared(branch))
            }
        };
        let s = spec("web", None);
        let ask = Ask::default();

        // This host has more room: here, nothing moved.
        assert_eq!(decide(&ask, &s, &idle_here(), ok("main")), Placed::Local);
        assert!(asked.borrow().is_empty());

        // The worker wins: moved, then there.
        let Placed::Worker { name, root, base, lines } = decide(&ask, &s, &busy_here(), ok("main")) else { panic!() };
        assert_eq!((name.as_str(), root, base), ("lap", PathBuf::from("/fleet/hub/roots/0123456789abcdef/cfg"), None));
        assert_eq!(lines, ["placed on lap (mem 87% free, disk 50% free, cpu 100% free)"]);
        assert_eq!(*asked.borrow(), ["lap"]);

        // A sandbox that can't move: here, with the note.
        let not_movable = |_: &str| Err(Unprepared::NotMovable("/w/web has no `origin`".into()));
        assert_eq!(
            decide(&ask, &s, &busy_here(), not_movable),
            Placed::LocalWithNote("note: not placed on lap: /w/web has no `origin`; running here".into())
        );
        // A move that fails: the verb fails, nowhere else.
        let failed = |_: &str| Err(Unprepared::Failed(anyhow::anyhow!("transfer refused: full")));
        assert_eq!(decide(&ask, &s, &busy_here(), failed), Placed::Failed("cannot place on lap: transfer refused: full".into()));

        // Every candidate over a cap: the best, warned.
        let over_mem = Some(Caps { mem: Some(G), ..Caps::default() });
        let all_over = candidates(&s, Some(cap(4, 3, over.clone())), &[], &[worker("lap", MachineKind::Inbound, LinkState::Connected, Some(cap(28, 0, over_mem)))], &[], &BTreeMap::new());
        let Placed::Worker { lines, .. } = decide(&ask, &s, &all_over, ok("main")) else { panic!() };
        assert_eq!(lines, ["placed on lap (mem 0% free, disk 50% free, cpu 100% free)", "warning: lap: over cap: mem 4G/1G"]);
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    /// The hub's side of `fleet.place`: the requester (its own report, the
    /// bias), the hub (by its member name) and the hub's connected machines
    /// the requester also links to, each with its fresh report.
    #[test]
    fn the_hub_places_a_members_run() {
        let over = Some(Caps { instances: Some(1), ..Caps::default() });
        let ask = |capacity: Capacity, reachable: &[&str]| PlaceParams::of(&spec("web", None), capacity, names(reachable));
        let machines = [
            worker("lap", MachineKind::Inbound, LinkState::Connected, Some(cap(28, 0, None))),
            worker("srv", MachineKind::Ssh, LinkState::Connected, None),
            worker("nb", MachineKind::Inbound, LinkState::Connected, None),
            worker("off", MachineKind::Ssh, LinkState::Unreachable, None),
        ];
        // Asked afresh: the hub's connected ones lap reaches, never lap itself.
        let a = ask(cap(4, 3, over.clone()), &["box", "srv", "off", "lap"]);
        assert_eq!(member_workers(&a, "lap", &machines, &[]), ["srv"]);

        // A busy requester (its own report, not the hub's saved one), a roomy hub.
        let none = BTreeMap::new();
        let busy_srv = BTreeMap::from([("srv".to_string(), cap(2, 6, over.clone()))]);
        let got = for_member(&a, "lap", "box", cap(28, 0, None), &[], &machines, &[], &busy_srv).unwrap();
        assert_eq!((got.on, got.machine.as_str()), (PlaceOn::Hub, "box"));
        assert_eq!(got.lines, ["placed on box (mem 87% free, disk 50% free, cpu 100% free)"]);

        // A roomy requester: it stays there, the bias among its reasons.
        let a = ask(cap(28, 0, None), &["box"]);
        let got = for_member(&a, "lap", "box", cap(28, 0, None), &[], &machines, &[], &none).unwrap();
        assert_eq!((got.on, got.machine.as_str()), (PlaceOn::Requester, "lap"));
        assert!(got.lines[0].starts_with("placed on lap (") && got.lines[0].contains("requester's host"), "{:?}", got.lines);

        // Another machine, roomier by its fresh report, which lap links to.
        let a = ask(cap(4, 3, over.clone()), &["box", "srv"]);
        let roomy_srv = BTreeMap::from([("srv".to_string(), cap(30, 0, None))]);
        let got = for_member(&a, "lap", "box", cap(4, 3, over.clone()), &[], &machines, &[], &roomy_srv).unwrap();
        assert_eq!((got.on, got.machine.as_str()), (PlaceOn::Machine, "srv"));
        // One lap doesn't link to is no candidate, however roomy.
        let a = ask(cap(4, 3, over.clone()), &["box"]);
        let got = for_member(&a, "lap", "box", cap(16, 1, None), &[], &machines, &[], &roomy_srv).unwrap();
        assert_eq!(got.on, PlaceOn::Hub);

        // Every candidate over a cap: the best, warned by name.
        let a = ask(cap(4, 3, over.clone()), &["box"]);
        let got = for_member(&a, "lap", "box", cap(28, 2, over.clone()), &[], &[], &[], &none).unwrap();
        assert_eq!(got.machine, "box");
        assert_eq!(got.lines[1], "warning: box: over cap: instances 2/1");

        // The wire, both ways.
        let params = serde_json::to_value(&a).unwrap();
        assert_eq!(serde_json::from_value::<PlaceParams>(params.clone()).unwrap(), a);
        crate::serve::dts::assert_matches("FleetPlaceParams", &params);
        let answer = serde_json::to_value(&got).unwrap();
        assert_eq!(answer["on"], "hub");
        crate::serve::dts::assert_matches("FleetPlaced", &answer);
        crate::serve::dts::assert_matches("FleetPlaceSpec", &params["spec"]);
        let on: Vec<String> = [PlaceOn::Requester, PlaceOn::Hub, PlaceOn::Machine]
            .iter()
            .map(|o| format!("'{}'", serde_json::to_value(o).unwrap().as_str().unwrap()))
            .collect();
        assert_eq!(crate::serve::dts::union_members("FleetPlaceOn"), on);
    }

    /// The member's side: the hub's answer, or why none came.
    #[test]
    fn a_member_runs_where_the_hub_says() {
        let ask = Ask::default();
        let answer = |on, machine: &str| Ok(PlaceAnswer { on, machine: machine.into(), lines: names(&[&format!("placed on {machine}")]) });
        let reach = names(&["box", "srv"]);
        let asked = RefCell::new(Vec::new());
        let ok = |w: &str| {
            asked.borrow_mut().push(w.to_string());
            Ok(prepared("main"))
        };
        assert_eq!(by_hub(&ask, "box", answer(PlaceOn::Requester, "lap"), &reach, ok), Placed::Here(names(&["placed on lap"])));
        assert!(asked.borrow().is_empty());
        let Placed::Worker { name, lines, .. } = by_hub(&ask, "box", answer(PlaceOn::Hub, "k-name"), &reach, ok) else { panic!() };
        assert_eq!((name.as_str(), lines), ("box", names(&["placed on k-name"])), "the hub, by this host's name for it");
        let Placed::Worker { name, .. } = by_hub(&ask, "box", answer(PlaceOn::Machine, "srv"), &reach, ok) else { panic!() };
        assert_eq!(name, "srv");
        assert_eq!(*asked.borrow(), ["box", "srv"]);
        assert_eq!(
            by_hub(&ask, "box", answer(PlaceOn::Machine, "nb"), &reach, ok),
            Placed::LocalWithNote("note: the hub `box` placed it on nb, which this host has no path to: running here".into())
        );
        assert_eq!(
            by_hub(&ask, "box", Err(Unplaced::Unreachable("its link is down".into())), &reach, ok),
            Placed::LocalWithNote("note: the hub `box` is unreachable (its link is down): running here".into())
        );
        assert_eq!(
            by_hub(&ask, "box", Err(Unplaced::Refused("fleet.place: denied".into())), &reach, ok),
            Placed::LocalWithNote("note: the hub `box` didn't place it (fleet.place: denied): running here".into())
        );
        // A sandbox that can't move to the hub's pick: here, as on the hub.
        let not_movable = |_: &str| Err(Unprepared::NotMovable("/w/web has no `origin`".into()));
        assert_eq!(
            by_hub(&ask, "box", answer(PlaceOn::Hub, "box"), &reach, not_movable),
            Placed::LocalWithNote("note: not placed on box: /w/web has no `origin`; running here".into())
        );
        assert_eq!(*asked.borrow(), ["box", "srv"]);
    }

    /// Only `run --place` is placed: a typed run's request (no `place`,
    /// or `false`) runs here without asking anyone.
    #[test]
    fn only_a_place_request_is_placed() {
        assert!(asked(&json!({"root": "/w/cfg", "sandbox": "web", "place": true})));
        for params in [json!({"root": "/w/cfg", "sandbox": "web"}), json!({"sandbox": "web", "place": false}), json!({"place": "yes"})] {
            assert!(!asked(&params), "{params}");
        }
    }

    #[test]
    fn a_new_branch_starts_at_the_bundled_tip() {
        let ask = |branch, base| Ask { branch, base };
        // Origin has it: the bundle carried it, no base needed.
        assert_eq!(start_point(&ask(Some("feature"), None), &prepared("feature")), None);
        // A new one: the bundle carried the checkout's branch, its start.
        assert_eq!(start_point(&ask(Some("new-one"), None), &prepared("main")).as_deref(), Some("main"));
        // No `--branch`: nothing to start.
        assert_eq!(start_point(&ask(None, None), &prepared("main")), None);
        // The user's `--base` stays theirs.
        assert_eq!(start_point(&ask(Some("new-one"), Some("v1")), &prepared("main")).as_deref(), Some("v1"));
    }

    #[test]
    fn the_worker_gets_the_request_in_its_root() {
        let params = json!({"root": "/w/cfg", "env": {}, "sandbox": "web", "branch": "new-one", "base": null, "place": true});
        let p = forwarded(params, "lap", Path::new("/fleet/hub/roots/h/cfg"), Some("main"));
        // No `place`: the worker runs it, never places it again.
        assert_eq!(
            p,
            json!({"root": "/fleet/hub/roots/h/cfg", "env": {}, "sandbox": "web", "branch": "new-one", "base": "main", "machine": "lap"})
        );
        assert!(!asked(&p));
        let p = forwarded(json!({"root": "/w/cfg", "base": "v1"}), "lap", Path::new("/x"), None);
        assert_eq!(p["base"], "v1");

        let r = tag_answer(crate::serve::proto::Response::ok(Some(1), json!({"instance": "web-2", "exit": 0})), "lap");
        assert_eq!(r.result, Some(json!({"instance": "web-2", "exit": 0, "machine": "lap"})));
        crate::serve::dts::assert_matches("VerbResult", r.result.as_ref().unwrap());
        let r = tag_answer(crate::serve::proto::Response::err(Some(1), "failed", "x"), "lap");
        assert!(r.result.is_none());
    }

    #[test]
    fn progress_names_the_worker() {
        struct Seen(RefCell<Vec<(String, Value)>>);
        impl verbs::Peer for Seen {
            fn notify(&self, method: &str, params: Value) -> std::io::Result<()> {
                self.0.borrow_mut().push((method.into(), params));
                Ok(())
            }
            fn hung_up(&self, _: std::time::Duration) -> bool {
                false
            }
            fn draining(&self) -> bool {
                false
            }
        }
        let seen = Seen(RefCell::default());
        let tagged = Tagged { inner: &seen, machine: "lap" };
        verbs::Peer::notify(&tagged, "progress", json!({"request": 1, "instance": "web-2", "phase": "building"})).unwrap();
        verbs::Peer::notify(&tagged, "output", json!({"request": 1, "stream": "stdout", "data": "x"})).unwrap();
        let seen = seen.0.into_inner();
        assert_eq!(seen[0].1["machine"], "lap");
        crate::serve::dts::assert_matches("VerbProgress", &seen[0].1);
        assert!(seen[1].1.get("machine").is_none());
    }
}
