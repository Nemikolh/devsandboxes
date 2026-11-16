//! `machine add`'s join rule (docs/architecture/daemon/fleet.md, *How a
//! machine joins*): from both ends' member name and control plane, what the add
//! writes where, and the name it registers the machine under. Pure: the
//! daemon's `machines.add` (`super::api`) reads both sides and does the
//! writes.
//!
//! | this host | the machine | result |
//! |---|---|---|
//! | in a fleet (control plane K) | in none | it joins K's fleet |
//! | in none | in a fleet (control plane K) | this host joins K's fleet |
//! | in none | in none | a fleet of two, this host the control plane (`--control-plane`: the machine) |
//! | control plane K | control plane K | an extra link in the same fleet |
//! | control plane K | control plane L | refused, naming both |
//!
//! `--control-plane` (fleet plan decision 36) only chooses the control plane of a new fleet:
//! with either end in a fleet it is refused unless the machine already is
//! the control plane, since moving the role is `machine control-plane`'s.
//!
//! Names (fleet plan decisions 2 and 35): the machine is registered under
//! its member name; `--name` renames it only while it joins by this add.
//! The joining side's name must be new to the fleet it joins.
//!
//! Neither end's `control_plane` is written from the plan: [`logs`] says what each
//! end's membership log becomes (decisions 48-50), and each end's `control_plane`
//! follows the log it adopts (`fleet::membership_api::adopt`).

use serde::{Deserialize, Serialize};

use crate::serve::api::ApiError;
use crate::serve::fleet::identity::{PublicKey, Signer};
use crate::serve::fleet::member::{CONTROL_PLANE_SELF, ControlPlane, check_member_name};
use crate::serve::fleet::membership::{self, Compare, Log, Op, Stored};
use crate::serve::fleet::membership_api::{Own, adopt_into, can_sign, lists, sign_into};

/// This host's side: its member name, its control plane, and the names of the
/// machines it knows (registry and inbound neighbors).
pub struct This<'a> {
    pub name: &'a str,
    pub control_plane: &'a ControlPlane,
    pub known: &'a [String],
}

/// The machine's side, as its `member.get` and `machines.list` over the
/// add-time probe answer: `name` is `None` when it has none (no `name` and
/// a hostname that isn't one).
pub struct Target<'a> {
    /// What messages call it (the typed target).
    pub label: &'a str,
    pub name: Option<&'a str>,
    pub control_plane: &'a ControlPlane,
    pub known: &'a [String],
}

/// What the add does, every check passed.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// The registry name: the machine's member name once the add is done.
    pub name: String,
    /// The machine's `member.set {name}`, when it joins.
    pub target_set: Option<TargetSet>,
    pub fleet: Fleet,
}

/// The machine's `member.set {name}`.
#[derive(Debug, PartialEq, Eq)]
pub struct TargetSet {
    /// Its member name, always written: `--name`, or the name it goes by
    /// now, pinned so a later hostname change can't rename a member the
    /// fleet knows by name.
    pub name: String,
}

/// `machines.add`'s `fleet`: the fleet both ends are in after the add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fleet {
    /// The control plane's member name.
    pub control_plane: String,
    pub joined: Joined,
}

/// Who joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Joined {
    /// The machine joined this host's fleet.
    Target,
    /// This host joined the machine's fleet.
    This,
    /// A new fleet of two: `Fleet::control_plane` says which end is the control plane.
    New,
    /// Both were in it already: an extra link.
    Same,
}

/// A control plane as a name: `self` is the member's own.
fn resolve(control_plane: &ControlPlane, own: Option<&str>) -> Option<String> {
    match control_plane {
        ControlPlane::None => None,
        ControlPlane::This => Some(own.unwrap_or(CONTROL_PLANE_SELF).to_string()),
        ControlPlane::Other(name) => Some(name.clone()),
    }
}

/// The join rule over both sides, `--name` (`asked`) and `--control-plane`
/// (`target_control_plane`: the machine is to be the control plane).
pub fn plan(this: &This, target: &Target, asked: Option<&str>, target_control_plane: bool) -> Result<Plan, ApiError> {
    let label = target.label;
    if let Some(asked) = asked {
        check_member_name(asked).map_err(|e| ApiError::invalid(format!("--name: {e:#}")))?;
    }
    if matches!(target.control_plane, ControlPlane::This) && target.name.is_none() {
        return Err(ApiError::failed(format!("{label} is a control plane with no member name; set `name` in its member.toml")));
    }
    let this_fleet = resolve(this.control_plane, Some(this.name));
    let target_fleet = resolve(target.control_plane, target.name);
    if target_control_plane && !matches!(target.control_plane, ControlPlane::This) {
        if let Some(k) = this_fleet.as_ref().or(target_fleet.as_ref()) {
            let at = if this_fleet.is_some() {
                format!("this host is in `{k}`'s fleet and {label} isn't its control plane")
            } else {
                format!("{label} is in `{k}`'s fleet and isn't its control plane")
            };
            return Err(ApiError::denied(format!(
                "--control-plane: {at}; move the control-plane role with `devsandbox machine control-plane <m>` instead"
            )));
        }
    }
    match (this_fleet, target_fleet) {
        (Some(k), Some(l)) if k != l => Err(ApiError::denied(format!(
            "this host is in `{k}`'s fleet, {label} in `{l}`'s: a machine is in one fleet only"
        ))),
        (Some(k), Some(_)) => {
            let name = fixed_name(target, asked, &k)?;
            not_this(this, label, &name, false)?;
            Ok(Plan { name, target_set: None, fleet: Fleet { control_plane: k, joined: Joined::Same } })
        }
        (Some(k), None) => {
            let name = joining_name(target, asked)?;
            not_this(this, label, &name, true)?;
            new_to(&name, &k, this.known, label, true)?;
            let target_set = TargetSet { name: name.clone() };
            Ok(Plan { name, target_set: Some(target_set), fleet: Fleet { control_plane: k, joined: Joined::Target } })
        }
        (None, Some(k)) => {
            let name = fixed_name(target, asked, &k)?;
            not_this(this, label, &name, false)?;
            if this.name == k || target.known.iter().any(|n| n == this.name) {
                return Err(ApiError::invalid(format!(
                    "this host's member name `{}` is already in `{k}`'s fleet: rename this host (`name` in its member.toml)",
                    this.name
                )));
            }
            Ok(Plan { name, target_set: None, fleet: Fleet { control_plane: k, joined: Joined::This } })
        }
        (None, None) => {
            let name = joining_name(target, asked)?;
            not_this(this, label, &name, true)?;
            new_to(&name, this.name, this.known, label, true)?;
            let control_plane = if target_control_plane { name.clone() } else { this.name.to_string() };
            Ok(Plan { target_set: Some(TargetSet { name: name.clone() }), fleet: Fleet { control_plane, joined: Joined::New }, name })
        }
    }
}

/// One end of an add as [`logs`] sees it: its member name once the add is
/// done, its key (the machine's as the user confirmed it), and its stored
/// membership log.
pub struct Side<'a> {
    pub name: &'a str,
    pub key: PublicKey,
    pub stored: &'a Stored,
}

impl Side<'_> {
    fn own(&self) -> Own<'_> {
        Own { name: self.name, key: self.key, next: None }
    }
}

/// What the add does to the membership logs.
#[derive(Debug, PartialEq, Eq)]
pub enum Logs {
    /// Both ends adopt this (`new`: the genesis and the machine's `add`
    /// signed here; `target`: the machine's `add`, or its introduction,
    /// signed here).
    Both(Stored),
    /// `this`: the machine lets this host in (`membership.sign` there), and
    /// this host adopts its answer.
    Signed,
    /// `same`: each end adopts the other's.
    Exchange,
}

/// A fleet made before member keys: no log to sign into (decision 52).
fn no_log(who: &str, control_plane: &str) -> ApiError {
    ApiError::failed(format!(
        "{who} is in `{control_plane}`'s fleet but holds no membership log (a fleet made before member keys): make the fleet again \
         (`devsandbox machine rm` each member, then `machine add`)"
    ))
}

/// The membership logs for `fleet` (the plan's), this host signing with
/// `signer` (`this.key`'s), every check made on both ends' logs before
/// anything is written: what each end adopts must take on its log and list
/// it. `label` names the machine in messages.
pub fn logs(fleet: &Fleet, this: &Side, signer: &impl Signer, target: &Side, label: &str) -> Result<Logs, ApiError> {
    let both = |next: Stored| -> Result<Logs, ApiError> {
        adopt_into(this.stored, &next, &this.own())?;
        adopt_into(target.stored, &next, &target.own()).map_err(|e| on(label, e))?;
        Ok(Logs::Both(next))
    };
    match fleet.joined {
        Joined::New => {
            // Decision 48: the host running the fleet-creating add is the
            // authority, whichever end is the control plane.
            let invalid = |e: anyhow::Error| ApiError::invalid(format!("{e:#}"));
            let mut log = Log::default();
            log.append(Op::genesis(this.name, this.key, &fleet.control_plane).map_err(ApiError::internal)?, this.name, signer).map_err(invalid)?;
            log.append(Op::Add { name: target.name.to_string(), key: target.key }, this.name, signer).map_err(invalid)?;
            both(Stored { log, introductions: vec![] })
        }
        Joined::Target => {
            if this.stored.log.membership().is_none() {
                return Err(no_log("this host", &fleet.control_plane));
            }
            both(sign_into(this.stored, &this.own(), signer, target.name, &target.key)?)
        }
        Joined::This => {
            if target.stored.log.membership().is_none() {
                return Err(no_log(label, &fleet.control_plane));
            }
            can_sign(target.stored, &target.own(), this.name, &this.key).map_err(|e| on(label, e))?;
            if this.stored.log.membership().is_some()
                && let fork @ Compare::Fork { .. } = membership::compare(&this.stored.log, target.stored.log.head())
            {
                return Err(ApiError::denied(format!("{label}: {fork}")));
            }
            Ok(Logs::Signed)
        }
        Joined::Same => {
            for (who, side) in [("this host", this), (label, target)] {
                if side.stored.log.membership().is_none() {
                    return Err(no_log(who, &fleet.control_plane));
                }
            }
            let here = adopt_into(this.stored, target.stored, &this.own())?;
            adopt_into(target.stored, this.stored, &target.own()).map_err(|e| on(label, e))?;
            if !lists(&here, target.name, &target.key) {
                return Err(ApiError::denied(format!(
                    "{label} (`{}`, {}) isn't in `{}`'s membership log nor introduced: it isn't this fleet's member",
                    target.name,
                    target.key.fingerprint(),
                    fleet.control_plane
                )));
            }
            Ok(Logs::Exchange)
        }
    }
}

/// `e` as the machine's refusal.
fn on(label: &str, mut e: ApiError) -> ApiError {
    e.message = format!("{label}: {}", e.message);
    e
}

/// A machine already in fleet `k` keeps its member name: `--name` must say
/// it.
fn fixed_name(target: &Target, asked: Option<&str>, k: &str) -> Result<String, ApiError> {
    let label = target.label;
    let Some(name) = target.name else {
        return Err(ApiError::failed(format!("{label} is in `{k}`'s fleet with no member name; set `name` in its member.toml")));
    };
    match asked {
        Some(asked) if asked != name => Err(ApiError::invalid(format!(
            "{label} is in `{k}`'s fleet as `{name}`: pass --name {name}, or no --name"
        ))),
        _ => Ok(name.to_string()),
    }
}

/// A machine joining by this add: `--name`, else its member name.
fn joining_name(target: &Target, asked: Option<&str>) -> Result<String, ApiError> {
    match (asked, target.name) {
        (Some(asked), _) => Ok(asked.to_string()),
        (None, Some(name)) => Ok(name.to_string()),
        (None, None) => Err(ApiError::invalid(format!(
            "{} has no member name (its hostname isn't a valid one): pass --name",
            target.label
        ))),
    }
}

/// The machine's name isn't this host's.
fn not_this(this: &This, label: &str, name: &str, renamable: bool) -> Result<(), ApiError> {
    if name != this.name {
        return Ok(());
    }
    let hint = if renamable { "pass --name" } else { "rename one of them (`name` in member.toml)" };
    Err(ApiError::invalid(format!("{label}'s member name `{name}` is this host's too: {hint}")))
}

/// A joining `name` isn't control plane `k`'s nor one of `known`.
fn new_to(name: &str, k: &str, known: &[String], label: &str, renamable: bool) -> Result<(), ApiError> {
    if name != k && !known.iter().any(|n| n == name) {
        return Ok(());
    }
    let hint = if renamable { ": pass --name" } else { "" };
    Err(ApiError::invalid(format!("`{name}` ({label}) is already a member of `{k}`'s fleet{hint}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn other(k: &str) -> ControlPlane {
        ControlPlane::Other(k.into())
    }

    /// `plan` with this host `lap` (knowing `this_known`) and the machine
    /// `box` (or `target_name`) at `me@box`, knowing `target_known`.
    fn run(this_control_plane: ControlPlane, target_name: Option<&str>, target_control_plane: ControlPlane, asked: Option<&str>) -> Result<Plan, ApiError> {
        run_control_plane(this_control_plane, target_name, target_control_plane, asked, false)
    }

    /// `run` with `--control-plane` as `make_control_plane`.
    fn run_control_plane(this_control_plane: ControlPlane, target_name: Option<&str>, target_control_plane: ControlPlane, asked: Option<&str>, make_control_plane: bool) -> Result<Plan, ApiError> {
        let (this_known, target_known) = (names(&["old"]), names(&["peer"]));
        let this = This { name: "lap", control_plane: &this_control_plane, known: &this_known };
        let target = Target { label: "me@box", name: target_name, control_plane: &target_control_plane, known: &target_known };
        plan(&this, &target, asked, make_control_plane)
    }

    fn message(r: Result<Plan, ApiError>) -> String {
        r.unwrap_err().message
    }

    #[test]
    fn the_machine_joins_this_hosts_fleet() {
        for (control_plane, k) in [(ControlPlane::This, "lap"), (other("k"), "k")] {
            let p = run(control_plane, Some("box"), ControlPlane::None, None).unwrap();
            assert_eq!(
                p,
                Plan {
                    name: "box".into(),
                    target_set: Some(TargetSet { name: "box".into() }),
                    fleet: Fleet { control_plane: k.into(), joined: Joined::Target },
                }
            );
        }
        // `--name` renames it as it joins; it may have no name of its own.
        for own in [Some("box"), None] {
            let p = run(other("k"), own, ControlPlane::None, Some("b2")).unwrap();
            assert_eq!(p.name, "b2");
            assert_eq!(p.target_set, Some(TargetSet { name: "b2".into() }));
        }
        let p = run(other("k"), Some("box"), ControlPlane::None, Some("box")).unwrap();
        assert_eq!(p.target_set.unwrap().name, "box", "its own name, pinned");
    }

    #[test]
    fn this_host_joins_the_machines_fleet() {
        // Its control plane `self`: the control plane is the machine, by its member name.
        let p = run(ControlPlane::None, Some("box"), ControlPlane::This, None).unwrap();
        assert_eq!(p, Plan { name: "box".into(), target_set: None, fleet: Fleet { control_plane: "box".into(), joined: Joined::This } });
        let p = run(ControlPlane::None, Some("box"), other("k"), Some("box")).unwrap();
        assert_eq!((p.target_set, p.fleet), (None, Fleet { control_plane: "k".into(), joined: Joined::This }));
    }

    #[test]
    fn neither_in_a_fleet_makes_one_of_two() {
        let p = run(ControlPlane::None, Some("box"), ControlPlane::None, None).unwrap();
        assert_eq!(
            p,
            Plan {
                name: "box".into(),
                target_set: Some(TargetSet { name: "box".into() }),
                fleet: Fleet { control_plane: "lap".into(), joined: Joined::New },
            }
        );
        assert_eq!(run(ControlPlane::None, None, ControlPlane::None, Some("b")).unwrap().target_set.unwrap().name, "b");
    }

    #[test]
    fn control_plane_makes_the_machine_the_control_plane_of_a_new_fleet() {
        let p = run_control_plane(ControlPlane::None, Some("box"), ControlPlane::None, None, true).unwrap();
        assert_eq!(
            p,
            Plan {
                name: "box".into(),
                target_set: Some(TargetSet { name: "box".into() }),
                fleet: Fleet { control_plane: "box".into(), joined: Joined::New },
            }
        );
        // Named as a joining machine is: `--name`, else its own.
        let p = run_control_plane(ControlPlane::None, None, ControlPlane::None, Some("srv"), true).unwrap();
        assert_eq!((p.target_set.unwrap().name, p.fleet.control_plane), ("srv".into(), "srv".into()));
        assert!(message(run_control_plane(ControlPlane::None, None, ControlPlane::None, None, true)).ends_with("pass --name"));
        assert!(message(run_control_plane(ControlPlane::None, Some("lap"), ControlPlane::None, None, true)).contains("`lap` is this host's too"));
        assert!(message(run_control_plane(ControlPlane::None, Some("old"), ControlPlane::None, None, true)).contains("`old` (me@box) is already a member"));
    }

    #[test]
    fn control_plane_in_a_fleet_needs_the_machine_to_be_its_control_plane() {
        // Refused whichever end is in a fleet, the machine not its control plane.
        for (this, target, m) in [
            (ControlPlane::This, ControlPlane::None, "--control-plane: this host is in `lap`'s fleet and me@box isn't its control plane"),
            (other("k"), ControlPlane::None, "--control-plane: this host is in `k`'s fleet and me@box isn't its control plane"),
            (other("k"), other("k"), "--control-plane: this host is in `k`'s fleet and me@box isn't its control plane"),
            (ControlPlane::None, other("k"), "--control-plane: me@box is in `k`'s fleet and isn't its control plane"),
        ] {
            let e = run_control_plane(this, Some("box"), target, None, true).unwrap_err();
            assert_eq!(e.code, "denied");
            assert_eq!(e.message, format!("{m}; move the control-plane role with `devsandbox machine control-plane <m>` instead"));
        }
        // The machine is the control plane: the table as without `--control-plane`.
        for this in [ControlPlane::None, other("box")] {
            assert_eq!(run_control_plane(this.clone(), Some("box"), ControlPlane::This, None, true).unwrap(), run(this, Some("box"), ControlPlane::This, None).unwrap());
        }
        assert_eq!(run_control_plane(other("k"), Some("box"), ControlPlane::This, None, true).unwrap_err().message, message(run(other("k"), Some("box"), ControlPlane::This, None)));
    }

    #[test]
    fn the_same_control_plane_is_an_extra_link() {
        for (this, target) in [(other("k"), other("k")), (ControlPlane::This, other("lap")), (other("box"), ControlPlane::This)] {
            let p = run(this, Some("box"), target, None).unwrap();
            assert_eq!((p.target_set, p.fleet.joined), (None, Joined::Same), "nothing pinned");
        }
    }

    #[test]
    fn two_control_planes_are_refused_naming_both() {
        let e = run(other("k"), Some("box"), other("l"), None).unwrap_err();
        assert_eq!(e.code, "denied");
        assert_eq!(e.message, "this host is in `k`'s fleet, me@box in `l`'s: a machine is in one fleet only");
        assert!(message(run(ControlPlane::This, Some("box"), ControlPlane::This, None)).contains("`lap`'s fleet, me@box in `box`'s"));
    }

    #[test]
    fn names_are_checked() {
        // In a fleet: `--name` must be its name.
        let m = message(run(ControlPlane::None, Some("box"), other("k"), Some("b2")));
        assert_eq!(m, "me@box is in `k`'s fleet as `box`: pass --name box, or no --name");
        assert!(message(run(other("k"), Some("box"), other("k"), Some("b2"))).contains("as `box`"));
        assert!(message(run(ControlPlane::None, None, other("k"), None)).contains("no member name"));
        assert!(message(run(ControlPlane::None, None, ControlPlane::This, None)).contains("no member name"));
        // Joining: a name or `--name`; never `self`.
        assert!(message(run(ControlPlane::None, None, ControlPlane::None, None)).ends_with("pass --name"));
        assert_eq!(run(ControlPlane::None, Some("box"), ControlPlane::None, Some("self")).unwrap_err().code, "invalid");
        assert_eq!(run(ControlPlane::None, Some("box"), ControlPlane::None, Some("a/b")).unwrap_err().code, "invalid");
        // Never this host's name, in any row.
        for (this, target) in [(ControlPlane::None, ControlPlane::None), (ControlPlane::This, ControlPlane::None), (ControlPlane::None, other("k")), (other("k"), other("k"))] {
            assert!(message(run(this, Some("lap"), target, None)).contains("`lap` is this host's too"));
        }
        // The joiner's name is new to the fleet it joins: this host's
        // machines and control plane when the machine joins, the machine's when this
        // host does.
        for (this, known) in [(ControlPlane::None, "old"), (other("k"), "old"), (other("k"), "k")] {
            let m = message(run(this, Some(known), ControlPlane::None, None));
            assert!(m.contains(&format!("`{known}` (me@box) is already a member")) && m.ends_with("pass --name"), "{m}");
        }
        assert!(run(ControlPlane::None, Some("box"), ControlPlane::None, Some("old")).is_err(), "--name too");
        let this_known = names(&["old"]);
        let target_known = names(&["peer", "lap"]);
        let this = This { name: "lap", control_plane: &ControlPlane::None, known: &this_known };
        let target = Target { label: "me@box", name: Some("box"), control_plane: &ControlPlane::This, known: &target_known };
        assert!(plan(&this, &target, None, false).unwrap_err().message.starts_with("this host's member name `lap` is already in `box`'s fleet"));
        let this = This { name: "k", control_plane: &ControlPlane::None, known: &this_known };
        let target = Target { label: "me@box", name: Some("box"), control_plane: &other("k"), known: &target_known };
        assert!(plan(&this, &target, None, false).is_err(), "this host named as the control plane");
    }

    mod logs {
        use super::super::*;
        use crate::serve::fleet::identity::SigningKey;
        use crate::serve::fleet::membership_api::test_support::fleet as made;

        fn key(n: u8) -> SigningKey {
            SigningKey::from_seed([n; 32])
        }

        fn pk(n: u8) -> PublicKey {
            key(n).public_key()
        }

        /// `k`'s fleet (key 7) with member `m` (key 8), which introduces
        /// `name` (key `n`): capped there.
        fn capped(name: &str, n: u8) -> Stored {
            let s = made("k", &key(7), "k", &[("m", pk(8))]);
            sign_into(&s, &Own { name: "m", key: pk(8), next: None }, &key(8), name, &pk(n)).unwrap()
        }

        fn f(control_plane: &str, joined: Joined) -> Fleet {
            Fleet { control_plane: control_plane.into(), joined }
        }

        /// [`logs`] with this host `lap` (key 1, holding `this`) and the
        /// machine `box` (key 2, holding `target`).
        fn run(fleet: Fleet, this: &Stored, target: &Stored) -> Result<Logs, ApiError> {
            let this_side = Side { name: "lap", key: pk(1), stored: this };
            let target_side = Side { name: "box", key: pk(2), stored: target };
            logs(&fleet, &this_side, &key(1), &target_side, "me@box")
        }

        fn both(r: Result<Logs, ApiError>) -> Stored {
            match r.unwrap() {
                Logs::Both(s) => s,
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn a_new_fleet_is_signed_here_whichever_end_is_the_control_plane() {
            let none = Stored::default();
            for control_plane in ["lap", "box"] {
                let s = both(run(f(control_plane, Joined::New), &none, &none));
                let m = s.log.membership().unwrap();
                assert_eq!((&m.authority, m.control_plane.as_str(), m.head.len), (&("lap".to_string(), pk(1)), control_plane, 2));
                assert_eq!(m.key_of("box"), Some(pk(2)));
            }
            // A stale log on either end: another fleet's, refused.
            let stale = made("x", &key(9), "x", &[("lap", pk(1)), ("box", pk(2))]);
            assert_eq!(run(f("lap", Joined::New), &stale, &none).unwrap_err().code, "denied");
            let e = run(f("lap", Joined::New), &none, &stale).unwrap_err();
            assert!(e.code == "denied" && e.message.starts_with("me@box: "), "{}", e.message);
        }

        #[test]
        fn the_machine_joining_is_added_or_introduced_here() {
            let none = Stored::default();
            // This host the authority: an `add`.
            let ours = made("lap", &key(1), "lap", &[]);
            let s = both(run(f("lap", Joined::Target), &ours, &none));
            assert_eq!((s.log.head().len, s.log.membership().unwrap().key_of("box")), (2, Some(pk(2))));
            // A member: an introduction, the log as it was.
            let ours = made("k", &key(7), "k", &[("lap", pk(1))]);
            let s = both(run(f("k", Joined::Target), &ours, &none));
            assert_eq!(s.log, ours.log);
            assert_eq!((s.introductions[0].name.as_str(), s.introductions[0].introducer.as_str()), ("box", "lap"));
            // Capped here: refused.
            assert_eq!(run(f("k", Joined::Target), &capped("lap", 1), &none).unwrap_err().code, "denied");
            // No log here (a fleet made before keys).
            let e = run(f("k", Joined::Target), &none, &none).unwrap_err();
            assert!(e.code == "failed" && e.message.contains("no membership log"), "{}", e.message);
        }

        #[test]
        fn this_host_joining_is_checked_against_the_machines_log() {
            let none = Stored::default();
            let theirs = made("box", &key(2), "box", &[]);
            assert_eq!(run(f("box", Joined::This), &none, &theirs), Ok(Logs::Signed));
            let e = run(f("box", Joined::This), &none, &none).unwrap_err();
            assert!(e.code == "failed" && e.message.starts_with("me@box is in `box`'s fleet but holds no membership log"), "{}", e.message);
            // The machine capped, or this host's name taken there.
            let e = run(f("k", Joined::This), &none, &capped("box", 2)).unwrap_err();
            assert!(e.code == "denied" && e.message.starts_with("me@box: "), "{}", e.message);
            let taken = made("box", &key(2), "box", &[("lap", pk(5))]);
            assert_eq!(run(f("box", Joined::This), &none, &taken).unwrap_err().code, "invalid");
            // A log of another fleet here.
            let stale = made("x", &key(9), "x", &[("lap", pk(1))]);
            assert_eq!(run(f("box", Joined::This), &stale, &theirs).unwrap_err().code, "denied");
        }

        #[test]
        fn an_extra_link_exchanges_logs_both_known() {
            let s = made("lap", &key(1), "lap", &[("box", pk(2))]);
            assert_eq!(run(f("lap", Joined::Same), &s, &s), Ok(Logs::Exchange));
            let longer = sign_into(&s, &Own { name: "lap", key: pk(1), next: None }, &key(1), "pi", &pk(3)).unwrap();
            assert_eq!(run(f("lap", Joined::Same), &s, &longer), Ok(Logs::Exchange));
            assert_eq!(run(f("lap", Joined::Same), &longer, &s), Ok(Logs::Exchange));
            // Another fleet; a fork.
            let other = made("lap", &key(1), "lap", &[("box", pk(2))]);
            let e = run(f("lap", Joined::Same), &s, &other).unwrap_err();
            assert!(e.code == "denied" && e.message.contains("another fleet"), "{}", e.message);
            let fork = sign_into(&s, &Own { name: "lap", key: pk(1), next: None }, &key(1), "pi", &pk(4)).unwrap();
            assert_eq!(run(f("lap", Joined::Same), &longer, &fork).unwrap_err().code, "denied");
            // Not each other's: the machine isn't in the log.
            let alone = made("lap", &key(1), "lap", &[]);
            assert_eq!(run(f("lap", Joined::Same), &alone, &alone).unwrap_err().code, "denied");
            let e = run(f("lap", Joined::Same), &Stored::default(), &s).unwrap_err();
            assert!(e.code == "failed" && e.message.starts_with("this host is in"), "{}", e.message);
        }
    }

    #[test]
    fn the_wire() {
        let f = Fleet { control_plane: "k".into(), joined: Joined::Target };
        assert_eq!(serde_json::to_value(&f).unwrap(), serde_json::json!({"control_plane": "k", "joined": "target"}));
    }
}
