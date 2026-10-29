//! Placement (docs/architecture/daemon/fleet.md, *Placement*; fleet plan
//! decision 11: it runs in the daemon): which machine a new instance goes
//! on. [`place`] is pure over a [`Spec`] and the [`Candidate`]s the daemon
//! fills from its snapshots ([`Candidate::local`], [`Candidate::from_machine`]).
//!
//! Soft caps never refuse: a machine over one scores below every machine
//! that isn't, and when all are over the best is still used with an
//! `over cap: …` warning. New work never goes to a machine that is
//! unreachable or lending `off` / `drain`, unless the spec pins it; a pin
//! is refused only when the machine is unreachable (or unknown).

use std::cmp::Ordering;
use std::fmt;

use serde_json::Value;

use super::lend::State;
use crate::serve::capacity::{Capacity, over_caps, over_note};
use crate::serve::machines::MachineKind;
use crate::serve::snapshots::{LinkState, Machine};

/// A machine as placement names it: this host, or a registered machine.
/// Orders this host first, then machines by name (the tie-break).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MachineRef {
    Local,
    Machine(String),
}

impl fmt::Display for MachineRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MachineRef::Local => f.write_str("this host"),
            MachineRef::Machine(name) => f.write_str(name),
        }
    }
}

/// What is being placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub sandbox: String,
    /// The resolved sandbox's config hash (`SandboxRow::config_hash`);
    /// `None` when unknown (no image locality then).
    pub config_hash: Option<String>,
    /// Its build dockerfile's hash (`SandboxRow::build_hash`); `None` is
    /// not compared.
    pub build_hash: Option<String>,
    /// The repo's identity (its `origin` URL for now); what a candidate's
    /// `has_repo` is about.
    pub repo: Option<String>,
    /// Who asked: gets [`REQUESTER`]'s bias.
    pub requester: MachineRef,
    /// `--machine m` or a qualified name: wins over the score.
    pub pinned: Option<MachineRef>,
}

/// One machine new work could go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub machine: MachineRef,
    pub reachable: bool,
    /// `On` but for an offered worker's reported `off` / `drain`
    /// ([`lends`]).
    pub lending: State,
    /// `None` before a report came: every headroom item counts as neutral.
    pub capacity: Option<Capacity>,
    /// An instance of the spec's sandbox runs there at the spec's hashes
    /// ([`has_image`]).
    pub has_image: bool,
    /// The spec's repo is already there (cloned or cached).
    pub has_repo: bool,
}

impl Candidate {
    /// This host: reachable and lending by definition. `roots` are its
    /// roots' `instances.snapshot` params, for [`has_image`]; `has_repo` is
    /// the caller's (no snapshot says which repos are here).
    pub fn local<'a>(spec: &Spec, capacity: Option<Capacity>, roots: impl IntoIterator<Item = &'a Value>) -> Candidate {
        Candidate {
            machine: MachineRef::Local,
            reachable: true,
            lending: State::On,
            capacity,
            has_image: has_image(spec, roots),
            has_repo: false,
        }
    }

    /// A machine (any kind), from what the daemon keeps of it: its link
    /// state ([`reachable`]), its pushed rows for [`has_image`], and, for
    /// an offered worker only ([`lends`]), its capacity report's lending
    /// state (`On` before a report comes: it links only while lending; its
    /// `drain` shows once one does). Any other machine takes new work
    /// (`On`) whatever its report says. `has_repo` is the caller's.
    pub fn from_machine(spec: &Spec, m: &Machine) -> Candidate {
        let capacity = m.capacity();
        let reported = capacity.as_ref().and_then(|c| c.lending).filter(|_| lends(m.kind));
        Candidate {
            machine: MachineRef::Machine(m.name.clone()),
            reachable: reachable(m.state),
            lending: reported.unwrap_or(State::On),
            capacity,
            has_image: has_image(spec, m.rows.values().map(|v| v.as_ref())),
            has_repo: false,
        }
    }
}

/// Whether new work can reach a machine: while its held link is up,
/// whatever its kind (the code moves over it before the run does).
pub fn reachable(state: LinkState) -> bool {
    state == LinkState::Connected
}

/// Whether a machine's lending state counts: an offered worker's only (its
/// `drain`, until lending goes in fleet step 8e).
pub fn lends(kind: MachineKind) -> bool {
    kind == MachineKind::Offered
}

/// Whether some root's snapshot in `roots` (`instances.snapshot` params)
/// shows the spec's image built: a sandbox row of that name at the spec's
/// `config_hash` (and `build_hash`, when the spec has one) with a ready,
/// non-drifted instance of it from the same root. Image tags are per config
/// root (`run::image::sandbox_image`), so the instance's root must match.
pub fn has_image<'a>(spec: &Spec, roots: impl IntoIterator<Item = &'a Value>) -> bool {
    let Some(config_hash) = spec.config_hash.as_deref().filter(|h| !h.is_empty()) else { return false };
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    roots.into_iter().any(|snap| {
        let rows = |k: &str| snap.get(k).and_then(Value::as_array).into_iter().flatten();
        rows("sandboxes").any(|s| {
            let matches = str_of(s, "name").as_deref() == Some(&spec.sandbox)
                && str_of(s, "config_hash").as_deref() == Some(config_hash)
                && spec.build_hash.as_ref().is_none_or(|b| str_of(s, "build_hash").as_ref() == Some(b));
            let root = str_of(s, "root").or_else(|| str_of(snap, "dir"));
            matches
                && root.is_some()
                && rows("instances").any(|i| {
                    str_of(i, "sandbox").as_deref() == Some(&spec.sandbox)
                        && str_of(i, "root") == root
                        && i.get("drift").and_then(Value::as_bool) == Some(false)
                        && i.get("phase").is_none_or(Value::is_null)
                })
        })
    })
}

/// Where it goes and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub machine: MachineRef,
    /// For the verb's output: `over cap: mem 18G/16G`, a pinned machine
    /// draining, ….
    pub warnings: Vec<String>,
    pub score: i64,
    /// The score's parts, as text (`mem 60% free`, `image built`, …).
    pub reasons: Vec<String>,
}

/// Why nothing was placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoPlace {
    /// The pinned machine isn't a candidate.
    UnknownPin(MachineRef),
    /// The pinned machine can't be reached.
    PinUnreachable(MachineRef),
    /// No candidate takes new work: each with why.
    NoCandidate(Vec<(MachineRef, &'static str)>),
}

impl fmt::Display for NoPlace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoPlace::UnknownPin(m) => write!(f, "no machine `{m}`"),
            NoPlace::PinUnreachable(m) => write!(f, "{m} is unreachable"),
            NoPlace::NoCandidate(skipped) if skipped.is_empty() => f.write_str("no machine to place on"),
            NoPlace::NoCandidate(skipped) => {
                let why: Vec<String> = skipped.iter().map(|(m, why)| format!("{m}: {why}")).collect();
                write!(f, "no machine takes new work ({})", why.join(", "))
            }
        }
    }
}

impl std::error::Error for NoPlace {}

/// Per-item headroom in points, `0..=100`; unknown is [`NEUTRAL`].
pub const NEUTRAL: i64 = 50;
/// Weights of the headroom items.
pub const W_MEM: i64 = 3;
pub const W_DISK: i64 = 2;
pub const W_SLOTS: i64 = 2;
pub const W_CPU: i64 = 1;
/// Subtracted per exceeded cap: more than any in-cap machine can score
/// (headroom 800 + locality 450 + bias 50), so over-cap always ranks last.
pub const OVER: i64 = 2000;
/// Locality.
pub const IMAGE: i64 = 300;
pub const REPO: i64 = 150;
/// The requester's own host.
pub const REQUESTER: i64 = 50;

/// Pick the machine for `spec` among `candidates`. A pin wins (refused only
/// unknown or unreachable; warned over cap, draining or off); otherwise the
/// highest [`score`] among reachable candidates lending `on`, ties to the
/// lowest [`MachineRef`].
pub fn place(spec: &Spec, candidates: &[Candidate]) -> Result<Placement, NoPlace> {
    if let Some(pin) = &spec.pinned {
        let c = candidates.iter().find(|c| &c.machine == pin).ok_or_else(|| NoPlace::UnknownPin(pin.clone()))?;
        if !c.reachable {
            return Err(NoPlace::PinUnreachable(pin.clone()));
        }
        let mut p = scored(spec, c);
        match c.lending {
            State::On => {}
            State::Drain => p.warnings.insert(0, format!("{pin} is draining")),
            State::Off => p.warnings.insert(0, format!("{pin} isn't lending")),
        }
        return Ok(p);
    }
    let mut skipped = Vec::new();
    let mut best: Option<Placement> = None;
    for c in candidates {
        let why = match (c.reachable, c.lending) {
            (false, _) => Some("unreachable"),
            (true, State::Off) => Some("not lending"),
            (true, State::Drain) => Some("draining"),
            (true, State::On) => None,
        };
        if let Some(why) = why {
            skipped.push((c.machine.clone(), why));
            continue;
        }
        let p = scored(spec, c);
        let better = best.as_ref().is_none_or(|b| match p.score.cmp(&b.score) {
            Ordering::Greater => true,
            Ordering::Equal => p.machine < b.machine,
            Ordering::Less => false,
        });
        if better {
            best = Some(p);
        }
    }
    best.ok_or(NoPlace::NoCandidate(skipped))
}

fn scored(spec: &Spec, c: &Candidate) -> Placement {
    let (score, reasons) = score(spec, c);
    let warnings = c.capacity.as_ref().and_then(over_note).into_iter().map(|n| format!("{}: {n}", c.machine)).collect();
    Placement { machine: c.machine.clone(), warnings, score, reasons }
}

/// `c`'s score for `spec` and its parts: headroom (mem·3 + disk·2 +
/// slots·2 + cpu·1, each `0..=100`, unknown [`NEUTRAL`]), minus [`OVER`]
/// per exceeded cap, plus [`IMAGE`] / [`REPO`] locality and the
/// [`REQUESTER`] bias.
pub fn score(spec: &Spec, c: &Candidate) -> (i64, Vec<String>) {
    let mut reasons = Vec::new();
    let mut total = 0;
    let h = headroom(c.capacity.as_ref());
    for (name, w, pts) in [("mem", W_MEM, h.mem), ("disk", W_DISK, h.disk), ("slots", W_SLOTS, h.slots), ("cpu", W_CPU, h.cpu)] {
        match pts {
            Some(p) => {
                total += w * p;
                reasons.push(format!("{name} {p}% free"));
            }
            None => total += w * NEUTRAL,
        }
    }
    if let Some(cap) = &c.capacity {
        let over = over_caps(cap);
        total -= OVER * over.len() as i64;
        if let Some(note) = over_note(cap) {
            reasons.push(note);
        }
    } else {
        reasons.push("no capacity report".into());
    }
    if c.has_image {
        total += IMAGE;
        reasons.push("image built".into());
    }
    if c.has_repo {
        total += REPO;
        reasons.push("repo there".into());
    }
    if c.machine == spec.requester {
        total += REQUESTER;
        reasons.push("requester's host".into());
    }
    (total, reasons)
}

/// Each headroom item in points, `None` unknown.
#[derive(Debug, Default, PartialEq, Eq)]
struct Headroom {
    mem: Option<i64>,
    disk: Option<i64>,
    slots: Option<i64>,
    cpu: Option<i64>,
}

/// Memory: available over total, and over the `mem` cap's room when set,
/// the smaller. Disk the same with free / total / `disk`. Slots: room left
/// under the `instances` cap (unknown without one). Cpu: `running` per cpu
/// (the `cpus` cap lowers the count), `0` at one instance per cpu.
fn headroom(c: Option<&Capacity>) -> Headroom {
    let Some(c) = c else { return Headroom::default() };
    let caps = c.caps.clone().unwrap_or_default();
    let pct = |num: u64, den: u64| (den > 0).then(|| (num.min(den) as u128 * 100 / den as u128) as i64);
    let room = |used: Option<u64>, cap: Option<u64>| pct(cap?.saturating_sub(used?), cap?);
    let min = |a: Option<i64>, b: Option<i64>| match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let used = |total: Option<u64>, free: Option<u64>| Some(total?.saturating_sub(free?));
    let running = c.running as u64;
    let cpus = match (c.cpus, caps.cpus) {
        (Some(n), Some(cap)) => Some(n.min(cap)),
        (n, cap) => n.or(cap),
    };
    Headroom {
        mem: min(c.mem_total.zip(c.mem_available).and_then(|(t, a)| pct(a, t)), room(used(c.mem_total, c.mem_available), caps.mem)),
        disk: min(c.disk_total.zip(c.disk_free).and_then(|(t, f)| pct(f, t)), room(used(c.disk_total, c.disk_free), caps.disk)),
        slots: room(Some(running), caps.instances),
        cpu: cpus.and_then(|n| pct(n.saturating_sub(running), n)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::serve::capacity::Caps;

    const G: u64 = 1 << 30;

    fn m(name: &str) -> MachineRef {
        MachineRef::Machine(name.into())
    }

    fn spec() -> Spec {
        Spec {
            sandbox: "web".into(),
            config_hash: Some("c1".into()),
            build_hash: None,
            repo: None,
            requester: MachineRef::Local,
            pinned: None,
        }
    }

    /// 8 cpus, 32G with `avail` free, 500G disk with `free_disk` free.
    fn cap(avail: u64, free_disk: u64, running: usize) -> Capacity {
        Capacity {
            cpus: Some(8),
            mem_total: Some(32 * G),
            mem_available: Some(avail * G),
            disk_free: Some(free_disk * G),
            disk_total: Some(500 * G),
            running,
            lending: Some(State::On),
            caps: None,
        }
    }

    fn cand(machine: MachineRef, capacity: Option<Capacity>) -> Candidate {
        Candidate { machine, reachable: true, lending: State::On, capacity, has_image: false, has_repo: false }
    }

    fn over_mem(c: Capacity) -> Capacity {
        Capacity { caps: Some(Caps { mem: Some(16 * G), ..Caps::default() }), ..c }
    }

    #[test]
    fn the_rules() {
        struct Case {
            name: &'static str,
            spec: Spec,
            candidates: Vec<Candidate>,
            want: Result<MachineRef, NoPlace>,
            warnings: &'static [&'static str],
        }
        let local = || cand(MachineRef::Local, Some(cap(16, 250, 2)));
        let roomy = || cand(m("big"), Some(cap(28, 450, 0)));
        let pinned = |p: MachineRef| Spec { pinned: Some(p), ..spec() };
        let cases = vec![
            Case { name: "more headroom wins", spec: spec(), candidates: vec![local(), roomy()], want: Ok(m("big")), warnings: &[] },
            Case {
                name: "the requester's bias breaks a near tie",
                spec: spec(),
                candidates: vec![cand(MachineRef::Local, Some(cap(16, 250, 2))), cand(m("a"), Some(cap(16, 250, 2)))],
                want: Ok(MachineRef::Local),
                warnings: &[],
            },
            Case {
                name: "a remote requester's own host",
                spec: Spec { requester: m("b"), ..spec() },
                candidates: vec![local(), cand(m("b"), Some(cap(16, 250, 2)))],
                want: Ok(m("b")),
                warnings: &[],
            },
            Case {
                name: "an exact tie goes to the lowest name",
                spec: Spec { requester: m("nobody"), ..spec() },
                candidates: vec![cand(m("b"), Some(cap(16, 250, 2))), cand(m("a"), Some(cap(16, 250, 2)))],
                want: Ok(m("a")),
                warnings: &[],
            },
            Case {
                name: "this host sorts before any machine in a tie",
                spec: Spec { requester: m("nobody"), ..spec() },
                candidates: vec![cand(m("a"), None), cand(MachineRef::Local, None)],
                want: Ok(MachineRef::Local),
                warnings: &[],
            },
            Case {
                name: "an image built beats more headroom",
                spec: spec(),
                candidates: vec![roomy(), Candidate { has_image: true, ..local() }],
                want: Ok(MachineRef::Local),
                warnings: &[],
            },
            Case {
                name: "a repo there tips a close call",
                spec: Spec { requester: m("nobody"), ..spec() },
                candidates: vec![cand(m("a"), Some(cap(20, 250, 1))), Candidate { has_repo: true, ..cand(m("b"), Some(cap(16, 250, 2))) }],
                want: Ok(m("b")),
                warnings: &[],
            },
            Case {
                name: "over a cap ranks below in-cap, even with its image",
                spec: spec(),
                candidates: vec![Candidate { has_image: true, ..cand(MachineRef::Local, Some(over_mem(cap(4, 250, 2)))) }, cand(m("small"), Some(cap(2, 10, 7)))],
                want: Ok(m("small")),
                warnings: &[],
            },
            Case {
                name: "all over cap: the best, warned",
                spec: spec(),
                candidates: vec![cand(MachineRef::Local, Some(over_mem(cap(14, 250, 2)))), cand(m("a"), Some(over_mem(cap(4, 250, 2))))],
                want: Ok(MachineRef::Local),
                warnings: &["this host: over cap: mem 18G/16G"],
            },
            Case {
                name: "unknown capacity is neutral, not zero",
                spec: Spec { requester: m("nobody"), ..spec() },
                candidates: vec![cand(m("a"), Some(cap(4, 40, 6))), cand(m("b"), None)],
                want: Ok(m("b")),
                warnings: &[],
            },
            Case {
                name: "unknown capacity loses to plenty",
                spec: Spec { requester: m("nobody"), ..spec() },
                candidates: vec![cand(m("a"), Some(cap(28, 450, 0))), cand(m("b"), None)],
                want: Ok(m("a")),
                warnings: &[],
            },
            Case {
                name: "drain, off and unreachable take no new work",
                spec: spec(),
                candidates: vec![
                    Candidate { lending: State::Drain, ..roomy() },
                    Candidate { lending: State::Off, ..cand(m("off"), Some(cap(30, 490, 0))) },
                    Candidate { reachable: false, ..cand(m("gone"), Some(cap(30, 490, 0))) },
                    local(),
                ],
                want: Ok(MachineRef::Local),
                warnings: &[],
            },
            Case {
                name: "nothing takes new work",
                spec: spec(),
                candidates: vec![Candidate { lending: State::Drain, ..roomy() }, Candidate { reachable: false, ..local() }],
                want: Err(NoPlace::NoCandidate(vec![(m("big"), "draining"), (MachineRef::Local, "unreachable")])),
                warnings: &[],
            },
            Case {
                name: "no candidates",
                spec: spec(),
                candidates: vec![],
                want: Err(NoPlace::NoCandidate(vec![])),
                warnings: &[],
            },
            Case { name: "a pin wins over the score", spec: pinned(MachineRef::Local), candidates: vec![roomy(), local()], want: Ok(MachineRef::Local), warnings: &[] },
            Case {
                name: "a pin over cap is warned",
                spec: pinned(m("a")),
                candidates: vec![local(), cand(m("a"), Some(over_mem(cap(4, 250, 2))))],
                want: Ok(m("a")),
                warnings: &["a: over cap: mem 28G/16G"],
            },
            Case {
                name: "a pin on a draining machine is warned",
                spec: pinned(m("big")),
                candidates: vec![local(), Candidate { lending: State::Drain, ..roomy() }],
                want: Ok(m("big")),
                warnings: &["big is draining"],
            },
            Case {
                name: "a pin on a machine not lending is warned",
                spec: pinned(m("big")),
                candidates: vec![Candidate { lending: State::Off, ..roomy() }],
                want: Ok(m("big")),
                warnings: &["big isn't lending"],
            },
            Case {
                name: "a pin on an unreachable machine is refused",
                spec: pinned(m("big")),
                candidates: vec![local(), Candidate { reachable: false, ..roomy() }],
                want: Err(NoPlace::PinUnreachable(m("big"))),
                warnings: &[],
            },
            Case { name: "a pin on an unknown machine is refused", spec: pinned(m("nope")), candidates: vec![local()], want: Err(NoPlace::UnknownPin(m("nope"))), warnings: &[] },
        ];
        for case in cases {
            let got = place(&case.spec, &case.candidates);
            match (&got, &case.want) {
                (Ok(p), Ok(want)) => {
                    assert_eq!(&p.machine, want, "{}: {p:?}", case.name);
                    assert_eq!(p.warnings, case.warnings, "{}", case.name);
                }
                (got, want) => assert_eq!(got.as_ref().map(|p| &p.machine), want.as_ref(), "{}", case.name),
            }
        }
    }

    #[test]
    fn the_score_and_its_reasons() {
        // 16/32G free (50), 250/500G (50), no instances cap (neutral 50),
        // 2 running of 8 cpus (75): 150 + 100 + 100 + 75.
        let c = cand(MachineRef::Local, Some(cap(16, 250, 2)));
        let (s, reasons) = score(&spec(), &c);
        assert_eq!(s, 425 + REQUESTER);
        assert_eq!(reasons, ["mem 50% free", "disk 50% free", "cpu 75% free", "requester's host"]);
        // Caps narrow the room: mem cap 20G with 16G used is 20%; 2 of 4
        // instances is 50%; a 4-cpu cap with 2 running is 50%.
        let capped = Capacity { caps: Some(Caps { cpus: Some(4), mem: Some(20 * G), disk: None, instances: Some(4) }), ..cap(16, 250, 2) };
        let (s, reasons) = score(&Spec { requester: m("x"), ..spec() }, &Candidate { has_image: true, has_repo: true, ..cand(m("a"), Some(capped)) });
        assert_eq!(s, 3 * 20 + 2 * 50 + 2 * 50 + 50 + IMAGE + REPO);
        assert_eq!(reasons, ["mem 20% free", "disk 50% free", "slots 50% free", "cpu 50% free", "image built", "repo there"]);
        // No report: all neutral.
        let (s, reasons) = score(&Spec { requester: m("x"), ..spec() }, &cand(m("a"), None));
        assert_eq!(s, (W_MEM + W_DISK + W_SLOTS + W_CPU) * NEUTRAL);
        assert_eq!(reasons, ["no capacity report"]);
        // Over: one penalty per exceeded cap, with the note.
        let over = Capacity { caps: Some(Caps { instances: Some(1), mem: Some(8 * G), ..Caps::default() }), ..cap(16, 250, 2) };
        let (s, reasons) = score(&Spec { requester: m("x"), ..spec() }, &cand(m("a"), Some(over)));
        assert_eq!(s, 2 * 50 + 75 - 2 * OVER, "mem and slots at 0");
        assert!(reasons.contains(&"over cap: mem 16G/8G, instances 2/1".to_string()), "{reasons:?}");
    }

    #[test]
    fn reachable_while_the_link_is_up_and_only_offered_ones_lend() {
        use LinkState::*;
        for (state, want) in [(Idle, false), (Connecting, false), (Connected, true), (Unreachable, false)] {
            assert_eq!(reachable(state), want, "{state:?}");
        }
        let lending: Vec<_> = [MachineKind::Ssh, MachineKind::Offered, MachineKind::Inbound].into_iter().map(lends).collect();
        assert_eq!(lending, [false, true, false]);
    }

    fn root_snapshot(drift: bool, phase: Value) -> Value {
        json!({
            "dir": "/r",
            "sandboxes": [{"name": "web", "config_hash": "c1", "build_hash": "b1", "root": "/r"}, {"name": "api", "config_hash": "c9", "build_hash": "", "root": "/r"}],
            "instances": [
                {"name": "web-1", "sandbox": "web", "root": "/r", "drift": drift, "phase": phase},
                {"name": "other", "sandbox": "web", "root": "/elsewhere", "drift": false},
            ],
        })
    }

    #[test]
    fn has_image_wants_a_ready_matching_instance_from_the_same_root() {
        let ok = root_snapshot(false, Value::Null);
        assert!(has_image(&spec(), [&ok]));
        assert!(has_image(&Spec { build_hash: Some("b1".into()), ..spec() }, [&ok]));
        assert!(!has_image(&Spec { build_hash: Some("b2".into()), ..spec() }, [&ok]), "build hash differs");
        assert!(!has_image(&Spec { config_hash: Some("c2".into()), ..spec() }, [&ok]), "config hash differs");
        assert!(!has_image(&Spec { config_hash: None, ..spec() }, [&ok]), "unknown hash");
        assert!(!has_image(&Spec { sandbox: "api".into(), config_hash: Some("c9".into()), ..spec() }, [&ok]), "no instance of it");
        assert!(!has_image(&spec(), [&root_snapshot(true, Value::Null)]), "drifted");
        assert!(!has_image(&spec(), [&root_snapshot(false, json!({"in_flight": {"step": "build"}}))]), "not ready");
        assert!(!has_image(&spec(), []));
        // Only another root's instance: its image is another root's tag.
        let mut moved = ok.clone();
        moved["instances"][0]["root"] = json!("/x");
        assert!(!has_image(&spec(), [&moved]));
        // No `root` on the sandbox row: the snapshot's `dir`.
        let mut bare = ok.clone();
        bare["sandboxes"][0].as_object_mut().unwrap().remove("root");
        assert!(has_image(&spec(), [&bare]));
    }

    #[test]
    fn candidates_from_the_daemons_state() {
        let ok = root_snapshot(false, Value::Null);
        let local = Candidate::local(&spec(), Some(cap(16, 250, 2)), [&ok]);
        assert_eq!((local.machine, local.reachable, local.lending, local.has_image), (MachineRef::Local, true, State::On, true));

        let mut snap = ok.clone();
        snap["capacity"] = serde_json::to_value(Capacity { lending: Some(State::Drain), ..cap(8, 100, 1) }).unwrap();
        let w = Machine {
            name: "w".into(),
            kind: MachineKind::Offered,
            roots: vec![PathBuf::from("/r")],
            state: LinkState::Connected,
            rows: BTreeMap::from([(PathBuf::from("/r"), Arc::new(snap))]),
            ..Machine::default()
        };
        let c = Candidate::from_machine(&spec(), &w);
        assert_eq!((c.machine.clone(), c.reachable, c.lending, c.has_image, c.has_repo), (m("w"), true, State::Drain, true, false));
        assert_eq!(c.capacity.as_ref().map(|c| c.running), Some(1));
        // An ssh machine: takes work whatever its report's lending says;
        // its link down: unreachable.
        let bare = Machine { name: "s".into(), state: LinkState::Idle, ..Machine::default() };
        let c = Candidate::from_machine(&spec(), &bare);
        assert_eq!((c.reachable, c.lending, c.capacity, c.has_image), (false, State::On, None, false));
        let ssh = Machine { kind: MachineKind::Ssh, ..w.clone() };
        let c = Candidate::from_machine(&spec(), &ssh);
        assert_eq!((c.reachable, c.lending, c.has_image), (true, State::On, true), "its `drain` is lending's");
        let inbound = Machine { kind: MachineKind::Inbound, ..w.clone() };
        assert_eq!(Candidate::from_machine(&spec(), &inbound).lending, State::On);
        // An offered one without a report: lending (it linked), neutral.
        let fresh = Machine { name: "f".into(), kind: MachineKind::Offered, state: LinkState::Connected, ..Machine::default() };
        let c = Candidate::from_machine(&spec(), &fresh);
        assert_eq!((c.reachable, c.lending, c.capacity), (true, State::On, None));
        let down = Machine { state: LinkState::Idle, ..w };
        assert!(!Candidate::from_machine(&spec(), &down).reachable);
    }

    #[test]
    fn no_place_says_why() {
        assert_eq!(NoPlace::UnknownPin(m("x")).to_string(), "no machine `x`");
        assert_eq!(NoPlace::PinUnreachable(m("x")).to_string(), "x is unreachable");
        assert_eq!(NoPlace::NoCandidate(vec![]).to_string(), "no machine to place on");
        assert_eq!(
            NoPlace::NoCandidate(vec![(m("a"), "draining"), (MachineRef::Local, "unreachable")]).to_string(),
            "no machine takes new work (a: draining, this host: unreachable)"
        );
    }
}
