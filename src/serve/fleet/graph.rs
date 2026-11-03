//! The fleet's link graph (docs/architecture/daemon/fleet.md, *Link graph*;
//! fleet plan step 10b, decisions 55, 59), pure: each member's signed
//! [`Report`] of its links, a [`Graph`] holding the latest verified one per
//! member, and [`Graph::paths`], the routes from one member to another in
//! the order a requester tries them.
//!
//! A report is an [`identity::Statement`](Statement) of kind [`KIND`] by
//! the member's key over [`encode`] (the fleet's genesis hash, then the
//! report), its version [`next_version`]: it only goes up, and [`Seen`]
//! refuses an older one whoever carries it.

// No caller yet: the reports go on the wire in fleet plan step 10b.2, the
// paths are tried in 10b.5.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::serve::fleet::identity::{Accepted, Seen, Signer, Statement};
use crate::serve::fleet::membership::Stored;
use crate::serve::fleet::proof;
use crate::serve::snapshots::LinkState;
use crate::worker_policy::Trust;

/// A link report's [`Statement::kind`].
pub const KIND: &str = "links";

/// The longest path [`Graph::paths`] answers, in hops: a routed stream's
/// TTL (decision 55).
pub const MAX_HOPS: usize = 4;

/// One member's word on its links (decision 59): its trust, whether it's
/// always on, and each link it has. On the wire `{"member", "trust",
/// "always_on", "links": [{"peer", "dials", "state", "since_ms"}]}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub member: String,
    pub trust: Trust,
    pub always_on: bool,
    pub links: Vec<LinkReport>,
}

/// One link of a [`Report`]: to `peer`, in `state` since `since_ms` (unix
/// ms, the reporter's clock).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkReport {
    pub peer: String,
    /// The reporting member dials the peer (has a shell there, over ssh):
    /// what exposure follows (fleet plan step 10b.8), not routing.
    pub dials: bool,
    pub state: LinkState,
    pub since_ms: u64,
}

/// `field` length-prefixed (u32 big-endian) onto `out`, so no two field
/// lists encode alike.
fn put(out: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).expect("a link report field under 4 GiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

fn trust_str(t: Trust) -> &'static str {
    match t {
        Trust::Worker => "worker",
        Trust::Full => "full",
    }
}

/// What a report's statement signs: the fleet's genesis hash (a report is
/// one fleet's), the member, trust, always-on and the link count, then
/// each link's peer, dials, state and since, every field length-prefixed
/// as the membership log's are (not JSON: no serializer's choices in what's
/// signed).
pub fn encode(genesis: &[u8; 32], r: &Report) -> Vec<u8> {
    let mut out = Vec::new();
    let count = u32::try_from(r.links.len()).expect("under 4 Gi links");
    for f in [&genesis[..], r.member.as_bytes(), trust_str(r.trust).as_bytes(), &[u8::from(r.always_on)], &count.to_be_bytes()] {
        put(&mut out, f);
    }
    for l in &r.links {
        for f in [l.peer.as_bytes(), &[u8::from(l.dials)], l.state.as_str().as_bytes(), &l.since_ms.to_be_bytes()] {
            put(&mut out, f);
        }
    }
    out
}

/// A new report's version: the time, unless that isn't past the last one
/// this member signed (`last`, 0 for none; a clock that went back).
pub fn next_version(last: u64, now_ms: u64) -> u64 {
    now_ms.max(last + 1)
}

/// `report` signed by `key` for the fleet of genesis hash `genesis`, at
/// [`next_version`], its links in peer order.
pub fn sign_report(key: &impl Signer, genesis: &[u8; 32], report: &Report, last: u64, now_ms: u64) -> (Statement, Report) {
    let mut report = report.clone();
    report.links.sort_by(|a, b| a.peer.cmp(&b.peer));
    let s = Statement::sign(&report.member, key, KIND, &encode(genesis, &report), next_version(last, now_ms));
    (s, report)
}

/// The latest verified report of each member, and the rollback memory
/// they were checked against.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    reports: BTreeMap<String, (Statement, Report)>,
    seen: Seen,
}

impl Graph {
    /// `report` with its statement `s` taken, against `stored`: by a member
    /// of the log or a held introduction (`proof::resolve`) with the key it
    /// resolves to (a revoked one refused), for this fleet, no link to
    /// itself or two to one peer, and newer than any from that key
    /// ([`Seen`]). Whether it changed the graph: the report held already
    /// is `false`; an older one, or another at its version, is refused.
    /// A refusal changes nothing.
    pub fn apply(&mut self, s: &Statement, report: &Report, stored: &Stored) -> Result<bool> {
        let who = &report.member;
        if s.kind != KIND || s.signer != *who {
            bail!("not a link report by `{who}`");
        }
        let m = stored.log.membership().context("in no fleet's membership log: no link reports")?;
        if m.revoked.contains(&s.key) {
            bail!("`{who}`'s report is signed with {}, which is revoked", s.key.fingerprint());
        }
        let Some((key, _)) = proof::resolve(stored, who) else {
            bail!("`{who}` is not a member of the log nor a held introduction");
        };
        if s.key != key {
            bail!("`{who}`'s report is signed with {}, not its key {}", s.key.fingerprint(), key.fingerprint());
        }
        let mut peers = BTreeSet::new();
        for l in &report.links {
            if l.peer == *who {
                bail!("`{who}` reports a link to itself");
            }
            if !peers.insert(&l.peer) {
                bail!("`{who}` reports two links to `{}`", l.peer);
            }
        }
        s.verify_payload(&encode(&m.genesis, report))
            .with_context(|| format!("`{who}`'s link report isn't signed for this fleet (another fleet's, or tampered)"))?;
        // `Seen` is per key: a report by a key since rotated away doesn't
        // hold back the new key's, and is replaced by it here.
        match self.seen.accept(s)? {
            Accepted::Again => Ok(false),
            Accepted::Newer => {
                self.reports.insert(who.clone(), (s.clone(), report.clone()));
                Ok(true)
            }
        }
    }

    /// The reports whose member no longer resolves to the key that signed
    /// them dropped (revoked, rotated, or gone from the log and the held
    /// introductions). Whether any was.
    pub fn prune(&mut self, stored: &Stored) -> bool {
        let before = self.reports.len();
        self.reports.retain(|name, (s, _)| proof::resolve(stored, name).is_some_and(|(k, _)| k == s.key));
        self.reports.len() != before
    }

    /// The edge between `a` and `b`: up when the newer of the two ends'
    /// reports (by statement version) says the link is connected; a report
    /// that doesn't list the other end says it isn't. At equal versions,
    /// up when either says so (a path tried and failed over costs less
    /// than one hidden). `Some(since_ms)` of the deciding link when up.
    fn edge(&self, a: &str, b: &str) -> Option<u64> {
        let link = |from: &str, to: &str| {
            self.reports.get(from).map(|(s, r)| (s.version, r.links.iter().find(|l| l.peer == to).filter(|l| l.state == LinkState::Connected)))
        };
        let up = |l: Option<&LinkReport>| l.map(|l| l.since_ms);
        match (link(a, b), link(b, a)) {
            (None, None) => None,
            (Some((_, l)), None) | (None, Some((_, l))) => up(l),
            (Some((va, la)), Some((vb, lb))) => match va.cmp(&vb) {
                std::cmp::Ordering::Greater => up(la),
                std::cmp::Ordering::Less => up(lb),
                std::cmp::Ordering::Equal => match (up(la), up(lb)) {
                    (Some(x), Some(y)) => Some(x.min(y)),
                    (x, y) => x.or(y),
                },
            },
        }
    }

    /// The simple paths from `from` to `to` over up edges, at most
    /// `max_hops` long, each the hops after `from` ending in `to`; only
    /// members of `stored`'s log or held introductions are on one. In the
    /// order to try (decision 21, fleet plan step 10b): fewest hops (the
    /// direct link first), then fewest hops through members not always on
    /// (or with no report), then the longest up (the most recent `since`
    /// of its edges oldest), then by name. None for `from == to`.
    pub fn paths(&self, stored: &Stored, from: &str, to: &str, max_hops: usize) -> Vec<Vec<String>> {
        let known = |n: &str| proof::resolve(stored, n).is_some();
        if from == to || !known(from) || !known(to) {
            return vec![];
        }
        let mut nodes: BTreeSet<&str> = BTreeSet::new();
        for (name, (_, r)) in &self.reports {
            nodes.insert(name);
            nodes.extend(r.links.iter().map(|l| l.peer.as_str()));
        }
        nodes.retain(|n| known(n));
        let mut found: Vec<(Vec<String>, u64)> = Vec::new();
        let mut path = vec![from.to_string()];
        self.walk(&nodes, to, max_hops, &mut path, 0, &mut found);

        let always_on = |n: &str| self.reports.get(n).is_some_and(|(_, r)| r.always_on);
        let mut keyed: Vec<_> = found
            .into_iter()
            .map(|(hops, newest)| {
                let not_on = hops[..hops.len() - 1].iter().filter(|h| !always_on(h)).count();
                ((hops.len(), not_on, newest), hops)
            })
            .collect();
        keyed.sort();
        keyed.into_iter().map(|(_, hops)| hops).collect()
    }

    /// Depth-first over up edges from `path`'s last node: every simple path
    /// to `to` within `max_hops`, with its edges' most recent `since`.
    fn walk(&self, nodes: &BTreeSet<&str>, to: &str, max_hops: usize, path: &mut Vec<String>, newest: u64, out: &mut Vec<(Vec<String>, u64)>) {
        if path.len() > max_hops {
            return;
        }
        let here = path.last().expect("a path starts at `from`").clone();
        for &next in nodes {
            if path.iter().any(|p| p == next) {
                continue;
            }
            let Some(since) = self.edge(&here, next) else { continue };
            let newest = newest.max(since);
            path.push(next.to_string());
            if next == to {
                out.push((path[1..].to_vec(), newest));
            } else {
                self.walk(nodes, to, max_hops, path, newest, out);
            }
            path.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::fleet::identity::{PublicKey, SigningKey};
    use crate::serve::fleet::membership::{Introduction, Log, Op};

    fn key(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    fn pk(n: u8) -> PublicKey {
        key(n).public_key()
    }

    /// Each test member's name and key seed.
    const MEMBERS: [(&str, u8); 5] = [("lap", 1), ("box", 2), ("pi", 3), ("k", 4), ("m", 5)];

    fn seed(name: &str) -> u8 {
        MEMBERS.iter().chain(&[("new", 6)]).find(|(n, _)| *n == name).unwrap().1
    }

    /// `lap` (key 1) the authority, `box` (key 2) the hub, then `pi`, `k`,
    /// `m` (keys 3-5); fleet id `[id; 16]`.
    fn fleet(id: u8) -> Stored {
        let mut log = Log::default();
        log.append(Op::genesis_with_id([id; 16], "lap", pk(1), "box"), "lap", &key(1)).unwrap();
        for (name, n) in &MEMBERS[1..] {
            log.append(Op::Add { name: name.to_string(), key: pk(*n) }, "lap", &key(1)).unwrap();
        }
        Stored { log, introductions: vec![] }
    }

    fn genesis(s: &Stored) -> [u8; 32] {
        s.log.membership().unwrap().genesis
    }

    fn link(peer: &str, up: bool, since_ms: u64) -> LinkReport {
        let state = if up { LinkState::Connected } else { LinkState::Unreachable };
        LinkReport { peer: peer.into(), dials: false, state, since_ms }
    }

    fn report(member: &str, always_on: bool, links: Vec<LinkReport>) -> Report {
        Report { member: member.into(), trust: Trust::Worker, always_on, links }
    }

    /// `r` signed by its member's own key for `stored`'s fleet at `version`.
    fn signed(stored: &Stored, r: &Report, version: u64) -> (Statement, Report) {
        sign_report(&key(seed(&r.member)), &genesis(stored), r, version - 1, 0)
    }

    fn apply(g: &mut Graph, stored: &Stored, r: &Report, version: u64) -> Result<bool> {
        let (s, r) = signed(stored, r, version);
        g.apply(&s, &r, stored)
    }

    /// A graph of `edges` (`a`, `b`, since), both ends reporting each one
    /// connected at version 1; `always_on` members say so.
    fn graph(stored: &Stored, edges: &[(&str, &str, u64)], always_on: &[&str]) -> Graph {
        let mut links: BTreeMap<&str, Vec<LinkReport>> = MEMBERS.iter().map(|(n, _)| (*n, vec![])).collect();
        for &(a, b, since) in edges {
            links.get_mut(a).unwrap().push(link(b, true, since));
            links.get_mut(b).unwrap().push(link(a, true, since));
        }
        let mut g = Graph::default();
        for (name, links) in links {
            assert!(apply(&mut g, stored, &report(name, always_on.contains(&name), links), 1).unwrap());
        }
        g
    }

    fn paths(g: &Graph, stored: &Stored, from: &str, to: &str) -> Vec<String> {
        g.paths(stored, from, to, MAX_HOPS).iter().map(|p| p.join(">")).collect()
    }

    #[test]
    fn versions_follow_the_clock_and_only_go_up() {
        assert_eq!(next_version(0, 1000), 1000);
        assert_eq!(next_version(999, 1000), 1000);
        assert_eq!(next_version(1000, 1000), 1001);
        assert_eq!(next_version(5000, 1000), 5001, "a clock that went back");
        let stored = fleet(1);
        let r = report("pi", false, vec![link("lap", true, 1), link("box", true, 2)]);
        let (s, sorted) = sign_report(&key(3), &genesis(&stored), &r, 0, 77);
        assert_eq!((s.kind.as_str(), s.signer.as_str(), s.key, s.version), (KIND, "pi", pk(3), 77));
        assert_eq!(sorted.links.iter().map(|l| l.peer.as_str()).collect::<Vec<_>>(), ["box", "lap"]);
        s.verify_payload(&encode(&genesis(&stored), &sorted)).unwrap();
        let wire = serde_json::to_value(&sorted).unwrap();
        assert_eq!(wire["links"][0], serde_json::json!({"peer": "box", "dials": false, "state": "connected", "since_ms": 2}));
        assert_eq!(serde_json::from_value::<Report>(wire).unwrap(), sorted);
    }

    #[test]
    fn newer_reports_replace_older_are_refused() {
        let stored = fleet(1);
        let mut g = Graph::default();
        let r = report("pi", false, vec![link("box", true, 1)]);
        assert!(apply(&mut g, &stored, &r, 10).unwrap());
        assert!(!apply(&mut g, &stored, &r, 10).unwrap(), "the one held: unchanged");
        let e = format!("{:#}", apply(&mut g, &stored, &report("pi", false, vec![]), 9).unwrap_err());
        assert!(e.contains("rollback"), "{e}");
        let e = format!("{:#}", apply(&mut g, &stored, &report("pi", true, vec![]), 10).unwrap_err());
        assert!(e.contains("two payloads"), "{e}");
        assert_eq!(g.reports["pi"].1, r, "refusals change nothing");
        let newer = report("pi", true, vec![]);
        assert!(apply(&mut g, &stored, &newer, 11).unwrap());
        assert_eq!(g.reports["pi"].1, newer);
    }

    #[test]
    fn forged_misattributed_and_foreign_reports_are_refused() {
        let stored = fleet(1);
        let mut g = Graph::default();
        let r = report("pi", false, vec![link("box", true, 1)]);
        let (s, r) = signed(&stored, &r, 5);

        // Another key's signature under pi's key.
        let forged = Statement { key: pk(3), ..sign_report(&key(4), &genesis(&stored), &r, 4, 0).0 };
        assert!(g.apply(&forged, &r, &stored).is_err());
        // k's key signing a report for pi.
        let (by_k, _) = sign_report(&key(4), &genesis(&stored), &r, 4, 0);
        let e = format!("{:#}", g.apply(&by_k, &r, &stored).unwrap_err());
        assert!(e.contains("not its key"), "{e}");
        // The statement's signer isn't the report's member.
        let (as_k, _) = sign_report(&key(4), &genesis(&stored), &report("k", false, vec![]), 4, 0);
        assert!(g.apply(&as_k, &r, &stored).is_err());
        // The report changed after signing.
        let mut changed = r.clone();
        changed.links[0].since_ms = 2;
        assert!(format!("{:#}", g.apply(&s, &changed, &stored).unwrap_err()).contains("tampered"));
        // Another fleet's report, the same keys.
        let other = fleet(2);
        let (foreign, _) = signed(&other, &r, 5);
        assert!(g.apply(&foreign, &r, &stored).is_err());
        // Not a member at all.
        let (ghost, gr) = sign_report(&key(9), &genesis(&stored), &report("ghost", false, vec![]), 0, 1);
        assert!(format!("{:#}", g.apply(&ghost, &gr, &stored).unwrap_err()).contains("not a member"));
        // Links to itself, or two to one peer.
        for links in [vec![link("pi", true, 1)], vec![link("box", true, 1), link("box", false, 2)]] {
            assert!(apply(&mut g, &stored, &report("pi", false, links), 6).is_err());
        }
        assert!(g.reports.is_empty());
        assert!(g.apply(&s, &r, &stored).unwrap(), "the genuine one");
    }

    #[test]
    fn revoked_keys_are_refused_and_pruned() {
        let mut stored = fleet(1);
        let mut g = Graph::default();
        let (s, r) = signed(&stored, &report("pi", false, vec![link("box", true, 1)]), 5);
        assert!(g.apply(&s, &r, &stored).unwrap());
        assert!(apply(&mut g, &stored, &report("box", true, vec![link("pi", true, 1)]), 5).unwrap());
        assert!(!g.prune(&stored), "everyone still resolves");

        stored.log.append(Op::Revoke { name: "pi".into(), key: pk(3) }, "lap", &key(1)).unwrap();
        assert!(g.prune(&stored));
        assert_eq!(g.reports.keys().collect::<Vec<_>>(), ["box"]);
        let e = format!("{:#}", g.apply(&s, &r, &stored).unwrap_err());
        assert!(e.contains("revoked"), "{e}");
        assert!(g.paths(&stored, "box", "pi", MAX_HOPS).is_empty(), "a revoked member is no destination");
    }

    #[test]
    fn an_introduced_member_reports() {
        let mut stored = fleet(1);
        let mut g = Graph::default();
        let r = report("new", false, vec![link("box", true, 1)]);
        assert!(apply(&mut g, &stored, &r, 1).is_err(), "not introduced yet");
        stored.introduce(Introduction::sign(&genesis(&stored), "new", pk(6), "box", &key(2))).unwrap();
        assert!(apply(&mut g, &stored, &r, 1).unwrap());
        assert!(apply(&mut g, &stored, &report("box", true, vec![link("new", true, 1)]), 1).unwrap());
        assert_eq!(paths(&g, &stored, "box", "new"), ["new"]);
        stored.introductions.clear();
        assert!(g.prune(&stored));
        assert!(paths(&g, &stored, "box", "new").is_empty());
    }

    #[test]
    fn paths_go_direct_then_fewest_hops() {
        let stored = fleet(1);
        let g = graph(&stored, &[("lap", "box", 1), ("lap", "pi", 1), ("pi", "box", 1), ("pi", "k", 1), ("k", "box", 1)], &[]);
        assert_eq!(paths(&g, &stored, "lap", "box"), ["box", "pi>box", "pi>k>box"]);
        assert_eq!(g.paths(&stored, "lap", "box", 2).len(), 2, "max_hops bounds the length");
        assert_eq!(g.paths(&stored, "lap", "box", 1), [vec!["box".to_string()]]);
    }

    #[test]
    fn ties_go_to_always_on_hops_then_the_longest_up_then_names() {
        let stored = fleet(1);
        // lap>k>m and lap>pi>m, two hops each.
        let edges = [("lap", "k", 1), ("k", "m", 1), ("lap", "pi", 1), ("pi", "m", 1)];
        assert_eq!(paths(&graph(&stored, &edges, &[]), &stored, "lap", "m"), ["k>m", "pi>m"], "by name");
        assert_eq!(paths(&graph(&stored, &edges, &["pi"]), &stored, "lap", "m"), ["pi>m", "k>m"], "always on");
        assert_eq!(paths(&graph(&stored, &edges, &["m"]), &stored, "lap", "m"), ["k>m", "pi>m"], "the destination doesn't count");
        // k's path came up more recently (its newest edge since 50).
        let edges = [("lap", "k", 1), ("k", "m", 50), ("lap", "pi", 10), ("pi", "m", 20)];
        assert_eq!(paths(&graph(&stored, &edges, &[]), &stored, "lap", "m"), ["pi>m", "k>m"], "the longest up");
        assert_eq!(paths(&graph(&stored, &edges, &["k"]), &stored, "lap", "m"), ["k>m", "pi>m"], "always on before since");
    }

    #[test]
    fn the_newer_end_says_whether_an_edge_is_up() {
        let stored = fleet(1);
        let mut g = Graph::default();
        let lap_up = report("lap", false, vec![link("box", true, 1)]);
        let box_down = report("box", false, vec![link("lap", false, 1)]);
        assert!(apply(&mut g, &stored, &lap_up, 10).unwrap());
        assert_eq!(paths(&g, &stored, "lap", "box"), ["box"], "one end's report");
        assert!(apply(&mut g, &stored, &box_down, 20).unwrap());
        assert!(paths(&g, &stored, "lap", "box").is_empty(), "box's is newer: down");
        assert!(apply(&mut g, &stored, &lap_up, 30).unwrap());
        assert_eq!(paths(&g, &stored, "box", "lap"), ["lap"], "lap's is newer: up, either way");
        assert!(apply(&mut g, &stored, &report("box", false, vec![]), 40).unwrap());
        assert!(paths(&g, &stored, "lap", "box").is_empty(), "a newer report without the link: down");
        assert!(apply(&mut g, &stored, &box_down, 30).is_err(), "box's older report doesn't come back");
        // Equal versions disagreeing: up.
        let mut g = Graph::default();
        assert!(apply(&mut g, &stored, &lap_up, 10).unwrap());
        assert!(apply(&mut g, &stored, &box_down, 10).unwrap());
        assert_eq!(paths(&g, &stored, "lap", "box"), ["box"]);
    }

    #[test]
    fn cycles_unknown_members_and_the_trivial_cases() {
        let stored = fleet(1);
        let mut g = graph(&stored, &[("lap", "box", 1), ("box", "pi", 1), ("pi", "lap", 1), ("pi", "k", 1)], &[]);
        // Simple paths only: around the triangle once, never back through lap.
        assert_eq!(paths(&g, &stored, "lap", "k"), ["pi>k", "box>pi>k"]);
        assert!(paths(&g, &stored, "lap", "lap").is_empty());
        assert!(paths(&g, &stored, "lap", "ghost").is_empty());
        assert!(paths(&g, &stored, "ghost", "lap").is_empty());
        assert!(paths(&g, &stored, "lap", "m").is_empty(), "no edge to m");
        // A peer no log names is never a hop, even reported up by both
        // sides it would join.
        let lap = report("lap", true, vec![link("box", true, 1), link("ghost", true, 1), link("pi", true, 1)]);
        let m = report("m", true, vec![link("ghost", true, 1)]);
        assert!(apply(&mut g, &stored, &lap, 2).unwrap());
        assert!(apply(&mut g, &stored, &m, 2).unwrap());
        assert!(paths(&g, &stored, "lap", "m").is_empty());
    }
}
