//! The daemon's `run` verb placed (docs/architecture/daemon/fleet.md,
//! *Placement*; fleet step 3c): a `run` that names no `machine` and isn't
//! `here` goes where [`place::place`] says among this host and every
//! machine whose link is up (ssh, inbound or offered: fleet step 8d). On a
//! machine, its sandbox's code moves first, into this host's fleet root
//! there (`code::prepare`; [`fleet_root`] says where that is), then the
//! request is forwarded as a `machine` one would be, in the moved config
//! root.
//!
//! Pure but for the `prepare` the daemon passes [`decide`]: the
//! candidates ([`candidates`], each worker's fresh `capacity.get` answer
//! over its saved report, [`with_fresh`]), the cheap check that placing
//! can matter ([`worth_placing`]), the request the worker gets
//! ([`forwarded`]), its answer read ([`after_forward`]) and the lines the
//! verb's output starts with.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::code::{Prepared, Unprepared};
use super::place::{self, Candidate, MachineRef, Placement, Spec};
use crate::serve::api::RootCandidate;
use crate::serve::capacity::Capacity;
use crate::serve::machines::MachineKind;
use crate::serve::snapshots::Machine;
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

/// The workers placement asks for a fresh report: the reachable ones
/// (their link up), whatever their saved report says.
pub fn reachable_workers(machines: &[Machine]) -> Vec<String> {
    workers(machines).filter(|m| place::reachable(m.state)).map(|m| m.name.clone()).collect()
}

/// Whether placing can pick anything but this host: some machine is
/// reachable. Checked first (no I/O: the links' last states), so a host
/// with no machine linked pays nothing, not even a `capacity.get`.
pub fn worth_placing(machines: &[Machine]) -> bool {
    !reachable_workers(machines).is_empty()
}

/// `c` with its machine's fresh report, when one came: its capacity
/// replaces the saved report's, and for an offered worker (of `kind`) its
/// lending state too (pure; lending is lending's alone, `place::lends`).
/// None came (a timeout, an error): the saved one stands.
pub fn with_fresh(c: Candidate, kind: MachineKind, fresh: Option<&Capacity>) -> Candidate {
    match fresh {
        Some(f) => {
            let lending = if place::lends(kind) { f.lending.unwrap_or(c.lending) } else { c.lending };
            Candidate { lending, capacity: Some(f.clone()), ..c }
        }
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

/// This host (its `capacity`, its watched roots' snapshots `local_roots`)
/// and every worker among `machines`, each with its `fresh` report by name
/// ([`with_fresh`]).
pub fn candidates(
    spec: &Spec,
    capacity: Option<Capacity>,
    local_roots: &[Arc<Value>],
    machines: &[Machine],
    fresh: &BTreeMap<String, Capacity>,
) -> Vec<Candidate> {
    let mut out = vec![Candidate::local(spec, capacity, local_roots.iter().map(|v| v.as_ref()))];
    out.extend(workers(machines).map(|m| with_fresh(Candidate::from_machine(spec, m), m.kind, fresh.get(&m.name))));
    out
}

/// What the request asked that placement reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ask<'a> {
    /// `--here`: never placed.
    pub here: bool,
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
    /// Here, after placement picked a worker the sandbox can't move to:
    /// this note first in the verb's output.
    LocalWithNote(String),
    /// On worker `name`, in its copy of the config root `root`; `base` is
    /// the `--base` it gets ([`start_point`]); `lines` start the output.
    Worker { name: String, root: PathBuf, base: Option<String>, lines: Vec<String> },
    /// Moving the code failed: the verb fails with this, it doesn't run
    /// anywhere else.
    Failed(String),
}

/// Where `run` goes: here when asked (`here`) or when placement picks
/// this host (or nothing, which a candidate list with this host in it
/// never gives); else `prepare(worker)` moves the code there, and its
/// answer decides: there, here with a note (`NotMovable`), or failed.
pub fn decide(
    ask: &Ask,
    spec: &Spec,
    candidates: &[Candidate],
    prepare: impl FnOnce(&str) -> Result<Prepared, Unprepared>,
) -> Placed {
    if ask.here {
        return Placed::Local;
    }
    let Ok(placement) = place::place(spec, candidates) else { return Placed::Local };
    let MachineRef::Machine(name) = &placement.machine else { return Placed::Local };
    match prepare(name) {
        Err(Unprepared::NotMovable(why)) => Placed::LocalWithNote(format!("note: not placed on {name}: {why}; running here")),
        Err(Unprepared::Failed(e)) => Placed::Failed(format!("cannot place on {name}: {e:#}")),
        Ok(prepared) => Placed::Worker {
            name: name.clone(),
            root: prepared.remote_root.clone(),
            base: start_point(ask, &prepared),
            lines: lines(&placement),
        },
    }
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
/// config root, `here` (it runs there), `base` when [`start_point`] gave
/// one.
pub fn forwarded(mut params: Value, worker: &str, root: &Path, base: Option<&str>) -> Value {
    if let Some(p) = params.as_object_mut() {
        p.insert("machine".into(), json!(worker));
        p.insert("root".into(), json!(root));
        p.insert("here".into(), json!(true));
        if let Some(base) = base {
            p.insert("base".into(), json!(base));
        }
    }
    params
}

/// What the forwarded run's answer means.
#[derive(Debug)]
pub enum Forwarded {
    /// The worker's answer, `machine` in its result ([`tag_answer`]).
    Answer(crate::serve::proto::Response),
    /// The worker refused new work (`denied`, `lent::DRAINING`): run here
    /// with this note. The code it was sent stays there.
    Here(String),
}

/// Read the forwarded run's answer `r` from worker `worker` (pure).
pub fn after_forward(r: crate::serve::proto::Response, worker: &str) -> Forwarded {
    if let Some(e) = &r.error
        && e.code == "denied"
        && e.message == super::lent::DRAINING
    {
        return Forwarded::Here(format!("note: not placed on {worker}: draining; running here"));
    }
    Forwarded::Answer(tag_answer(r, worker))
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
    use crate::serve::fleet::lend::State;
    use crate::serve::snapshots::LinkState;

    const G: u64 = 1 << 30;

    fn cap(avail: u64, running: usize, caps: Option<Caps>) -> Capacity {
        Capacity {
            cpus: Some(8),
            mem_total: Some(32 * G),
            mem_available: Some(avail * G),
            disk_free: Some(250 * G),
            disk_total: Some(500 * G),
            running,
            lending: Some(State::On),
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
        let drain = Some(Capacity { lending: Some(State::Drain), ..cap(28, 0, None) });
        assert!(!worth_placing(&[]), "no machines");
        for kind in [MachineKind::Ssh, MachineKind::Inbound, MachineKind::Offered] {
            assert!(worth_placing(&[worker("m", kind, Connected, roomy())]), "{kind:?}: every kind is a candidate");
            for down in [Idle, Connecting, Unreachable] {
                assert!(!worth_placing(&[worker("m", kind, down, roomy())]), "{kind:?} {down:?}: its link is down");
            }
        }
        // Its saved report may be stale: asked afresh either way.
        assert!(worth_placing(&[worker("lap", MachineKind::Offered, Connected, drain)]));
        assert!(worth_placing(&[worker("lap", MachineKind::Offered, Connected, None)]));
        let all = [
            worker("a", MachineKind::Offered, Connected, None),
            worker("b", MachineKind::Offered, Unreachable, None),
            worker("c", MachineKind::Inbound, Connected, None),
            worker("d", MachineKind::Ssh, Connected, None),
            worker("e", MachineKind::Ssh, Idle, None),
        ];
        assert_eq!(reachable_workers(&all), ["a", "c", "d"]);
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
        let saved = Capacity { lending: Some(State::Drain), ..cap(4, 6, None) };
        let m = worker("lap", MachineKind::Offered, LinkState::Connected, Some(saved.clone()));
        let fresh = cap(28, 0, None);
        let offered = MachineKind::Offered;
        let c = with_fresh(Candidate::from_machine(&s, &m), offered, Some(&fresh));
        assert_eq!((c.lending, c.capacity.as_ref()), (State::On, Some(&fresh)));
        let c = with_fresh(Candidate::from_machine(&s, &m), offered, None);
        assert_eq!((c.lending, c.capacity.as_ref()), (State::Drain, Some(&saved)));
        // No lending in the fresh one (no `lend.toml` there): the saved state.
        let c = with_fresh(Candidate::from_machine(&s, &m), offered, Some(&Capacity { lending: None, ..fresh.clone() }));
        assert_eq!(c.lending, State::Drain);
        // Any other kind's lending state isn't placement's: a drain there takes work.
        for kind in [MachineKind::Ssh, MachineKind::Inbound] {
            let other = Machine { kind, ..m.clone() };
            let drained = Capacity { lending: Some(State::Drain), ..fresh.clone() };
            let c = with_fresh(Candidate::from_machine(&s, &other), kind, Some(&drained));
            assert_eq!((c.lending, c.capacity.as_ref()), (State::On, Some(&drained)), "{kind:?}");
        }
        // Through `candidates`, by name; the fresh one decides the placement.
        let neutral = worker("lap", MachineKind::Offered, LinkState::Connected, None);
        let stale = candidates(&s, None, &[], std::slice::from_ref(&neutral), &BTreeMap::new());
        assert_eq!(decide(&Ask::default(), &s, &stale, |_| unreachable!()), Placed::Local, "neutral both: this host's bias");
        let fresh_map = BTreeMap::from([("lap".to_string(), fresh)]);
        let c = candidates(&s, None, &[], &[neutral], &fresh_map);
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
            worker("lap", MachineKind::Offered, LinkState::Connected, None),
            worker("nb", MachineKind::Inbound, LinkState::Connected, None),
            worker("off", MachineKind::Ssh, LinkState::Unreachable, None),
        ];
        let c = candidates(&spec("web", None), Some(cap(16, 2, None)), &[], &machines, &BTreeMap::new());
        let names: Vec<_> = c.iter().map(|c| (c.machine.to_string(), c.reachable)).collect();
        let want = [("this host", true), ("srv", true), ("lap", true), ("nb", true), ("off", false)];
        assert_eq!(names, want.map(|(n, r)| (n.to_string(), r)));
        assert_eq!(c[0].capacity.as_ref().map(|c| c.running), Some(2));

        // An inbound or ssh machine with more room wins over a busy host.
        let over = Some(Caps { instances: Some(1), ..Caps::default() });
        for kind in [MachineKind::Inbound, MachineKind::Ssh] {
            let roomy = [worker("m", kind, LinkState::Connected, Some(cap(28, 0, None)))];
            let c = candidates(&spec("web", None), Some(cap(4, 3, over.clone())), &[], &roomy, &BTreeMap::new());
            let placed = decide(&Ask::default(), &spec("web", None), &c, |_| Ok(prepared("main")));
            assert!(matches!(&placed, Placed::Worker { name, .. } if name == "m"), "{kind:?}: {placed:?}");
        }
    }

    /// The decision over (request, candidates), `prepare` faked: what it
    /// was asked and what it answered.
    #[test]
    fn the_decision() {
        let over = Some(Caps { instances: Some(1), ..Caps::default() });
        let busy_here = || candidates(&spec("web", None), Some(cap(4, 3, over.clone())), &[], &[worker("lap", MachineKind::Offered, LinkState::Connected, Some(cap(28, 0, None)))], &BTreeMap::new());
        let idle_here = || candidates(&spec("web", None), Some(cap(28, 0, None)), &[], &[worker("lap", MachineKind::Offered, LinkState::Connected, Some(cap(4, 6, None)))], &BTreeMap::new());
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
        // `--here`: here whatever the score.
        assert_eq!(decide(&Ask { here: true, ..ask }, &s, &busy_here(), ok("main")), Placed::Local);
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
        let all_over = candidates(&s, Some(cap(4, 3, over.clone())), &[], &[worker("lap", MachineKind::Offered, LinkState::Connected, Some(cap(28, 0, over_mem)))], &BTreeMap::new());
        let Placed::Worker { lines, .. } = decide(&ask, &s, &all_over, ok("main")) else { panic!() };
        assert_eq!(lines, ["placed on lap (mem 0% free, disk 50% free, cpu 100% free)", "warning: lap: over cap: mem 4G/1G"]);
    }

    #[test]
    fn a_new_branch_starts_at_the_bundled_tip() {
        let ask = |branch, base| Ask { here: false, branch, base };
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
        let params = json!({"root": "/w/cfg", "env": {}, "sandbox": "web", "branch": "new-one", "base": null});
        let p = forwarded(params, "lap", Path::new("/fleet/hub/roots/h/cfg"), Some("main"));
        assert_eq!(
            p,
            json!({"root": "/fleet/hub/roots/h/cfg", "env": {}, "sandbox": "web", "branch": "new-one", "base": "main", "machine": "lap", "here": true})
        );
        let p = forwarded(json!({"root": "/w/cfg", "base": "v1"}), "lap", Path::new("/x"), None);
        assert_eq!(p["base"], "v1");

        let r = tag_answer(crate::serve::proto::Response::ok(Some(1), json!({"instance": "web-2", "exit": 0})), "lap");
        assert_eq!(r.result, Some(json!({"instance": "web-2", "exit": 0, "machine": "lap"})));
        crate::serve::dts::assert_matches("VerbResult", r.result.as_ref().unwrap());
        let r = tag_answer(crate::serve::proto::Response::err(Some(1), "failed", "x"), "lap");
        assert!(r.result.is_none());
    }

    #[test]
    fn a_draining_workers_refusal_runs_here() {
        use crate::serve::proto::Response;
        let refused = Response::err(Some(1), "denied", crate::serve::fleet::lent::DRAINING);
        assert!(matches!(after_forward(refused, "lap"), Forwarded::Here(n) if n == "note: not placed on lap: draining; running here"));
        // Another denial is the answer, as is.
        let other = Response::err(Some(1), "denied", "`run` with `all` isn't lent to a fleet hub");
        assert!(matches!(after_forward(other, "lap"), Forwarded::Answer(r) if r.error.is_some()));
        let Forwarded::Answer(ok) = after_forward(Response::ok(Some(1), json!({"instance": "w", "exit": 0})), "lap") else { panic!() };
        assert_eq!(ok.result.unwrap()["machine"], "lap");
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
