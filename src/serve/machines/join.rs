//! `machine add`'s join rule (docs/architecture/daemon/fleet.md, *How a
//! machine joins*): from both ends' member name and hub, what the add
//! writes where, and the name it registers the machine under. Pure: the
//! daemon's `machines.add` (`super::api`) reads both sides and does the
//! writes.
//!
//! | this host | the machine | result |
//! |---|---|---|
//! | in a fleet (hub K) | in none | it joins K's fleet |
//! | in none | in a fleet (hub K) | this host joins K's fleet |
//! | in none | in none | a fleet of two, this host the hub |
//! | hub K | hub K | an extra link in the same fleet |
//! | hub K | hub L | refused, naming both |
//!
//! Names (fleet plan decisions 2 and 35): the machine is registered under
//! its member name; `--name` renames it only while it joins by this add.
//! The joining side's name must be new to the fleet it joins.

use serde::{Deserialize, Serialize};

use crate::serve::api::ApiError;
use crate::serve::fleet::member::{HUB_SELF, Hub, check_member_name};

/// This host's side: its member name, its hub, and the names of the
/// machines it knows (registry and inbound neighbors).
pub struct This<'a> {
    pub name: &'a str,
    pub hub: &'a Hub,
    pub known: &'a [String],
}

/// The machine's side, as its `member.get` and `machines.list` over the
/// add-time probe answer: `name` is `None` when it has none (no `name` and
/// a hostname that isn't one).
pub struct Target<'a> {
    /// What messages call it (the typed target).
    pub label: &'a str,
    pub name: Option<&'a str>,
    pub hub: &'a Hub,
    pub known: &'a [String],
}

/// What the add does, every check passed.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// The registry name: the machine's member name once the add is done.
    pub name: String,
    /// The machine's `member.set`, when it joins.
    pub target_set: Option<TargetSet>,
    /// This host's new `hub`, when it joins or becomes a hub.
    pub this_hub: Option<String>,
    pub fleet: Fleet,
}

/// The machine's `member.set {name, hub}`.
#[derive(Debug, PartialEq, Eq)]
pub struct TargetSet {
    /// Its member name, always written: `--name`, or the name it goes by
    /// now, pinned so a later hostname change can't rename a member the
    /// fleet knows by name.
    pub name: String,
    pub hub: String,
}

/// `machines.add`'s `fleet`: the fleet both ends are in after the add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fleet {
    /// The hub's member name.
    pub hub: String,
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
    /// A new fleet of two, this host the hub.
    New,
    /// Both were in it already: an extra link.
    Same,
}

/// A hub as a name: `self` is the member's own.
fn resolve(hub: &Hub, own: Option<&str>) -> Option<String> {
    match hub {
        Hub::None => None,
        Hub::This => Some(own.unwrap_or(HUB_SELF).to_string()),
        Hub::Other(name) => Some(name.clone()),
    }
}

/// The join rule over both sides and `--name` (`asked`).
pub fn plan(this: &This, target: &Target, asked: Option<&str>) -> Result<Plan, ApiError> {
    let label = target.label;
    if let Some(asked) = asked {
        check_member_name(asked).map_err(|e| ApiError::invalid(format!("--name: {e:#}")))?;
    }
    if matches!(target.hub, Hub::This) && target.name.is_none() {
        return Err(ApiError::failed(format!("{label} is a hub with no member name; set `name` in its member.toml")));
    }
    let this_fleet = resolve(this.hub, Some(this.name));
    let target_fleet = resolve(target.hub, target.name);
    match (this_fleet, target_fleet) {
        (Some(k), Some(l)) if k != l => Err(ApiError::denied(format!(
            "this host is in `{k}`'s fleet, {label} in `{l}`'s: a machine is in one fleet only"
        ))),
        (Some(k), Some(_)) => {
            let name = fixed_name(target, asked, &k)?;
            not_this(this, label, &name, false)?;
            Ok(Plan { name, target_set: None, this_hub: None, fleet: Fleet { hub: k, joined: Joined::Same } })
        }
        (Some(k), None) => {
            let name = joining_name(target, asked)?;
            not_this(this, label, &name, true)?;
            new_to(&name, &k, this.known, label, true)?;
            let target_set = TargetSet { name: name.clone(), hub: k.clone() };
            Ok(Plan { name, target_set: Some(target_set), this_hub: None, fleet: Fleet { hub: k, joined: Joined::Target } })
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
            Ok(Plan { name, target_set: None, this_hub: Some(k.clone()), fleet: Fleet { hub: k, joined: Joined::This } })
        }
        (None, None) => {
            let name = joining_name(target, asked)?;
            not_this(this, label, &name, true)?;
            new_to(&name, this.name, this.known, label, true)?;
            let target_set = TargetSet { name: name.clone(), hub: this.name.to_string() };
            Ok(Plan {
                name,
                target_set: Some(target_set),
                this_hub: Some(HUB_SELF.to_string()),
                fleet: Fleet { hub: this.name.to_string(), joined: Joined::New },
            })
        }
    }
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

/// A joining `name` isn't hub `k`'s nor one of `known`.
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

    fn other(k: &str) -> Hub {
        Hub::Other(k.into())
    }

    /// `plan` with this host `lap` (knowing `this_known`) and the machine
    /// `box` (or `target_name`) at `me@box`, knowing `target_known`.
    fn run(this_hub: Hub, target_name: Option<&str>, target_hub: Hub, asked: Option<&str>) -> Result<Plan, ApiError> {
        let (this_known, target_known) = (names(&["old"]), names(&["peer"]));
        let this = This { name: "lap", hub: &this_hub, known: &this_known };
        let target = Target { label: "me@box", name: target_name, hub: &target_hub, known: &target_known };
        plan(&this, &target, asked)
    }

    fn message(r: Result<Plan, ApiError>) -> String {
        r.unwrap_err().message
    }

    #[test]
    fn the_machine_joins_this_hosts_fleet() {
        for (hub, k) in [(Hub::This, "lap"), (other("k"), "k")] {
            let p = run(hub, Some("box"), Hub::None, None).unwrap();
            assert_eq!(
                p,
                Plan {
                    name: "box".into(),
                    target_set: Some(TargetSet { name: "box".into(), hub: k.into() }),
                    this_hub: None,
                    fleet: Fleet { hub: k.into(), joined: Joined::Target },
                }
            );
        }
        // `--name` renames it as it joins; it may have no name of its own.
        for own in [Some("box"), None] {
            let p = run(other("k"), own, Hub::None, Some("b2")).unwrap();
            assert_eq!(p.name, "b2");
            assert_eq!(p.target_set, Some(TargetSet { name: "b2".into(), hub: "k".into() }));
        }
        let p = run(other("k"), Some("box"), Hub::None, Some("box")).unwrap();
        assert_eq!(p.target_set.unwrap().name, "box", "its own name, pinned");
    }

    #[test]
    fn this_host_joins_the_machines_fleet() {
        // Its hub `self`: the hub is the machine, by its member name.
        let p = run(Hub::None, Some("box"), Hub::This, None).unwrap();
        assert_eq!(
            p,
            Plan { name: "box".into(), target_set: None, this_hub: Some("box".into()), fleet: Fleet { hub: "box".into(), joined: Joined::This } }
        );
        let p = run(Hub::None, Some("box"), other("k"), Some("box")).unwrap();
        assert_eq!((p.this_hub.as_deref(), p.fleet), (Some("k"), Fleet { hub: "k".into(), joined: Joined::This }));
    }

    #[test]
    fn neither_in_a_fleet_makes_one_of_two() {
        let p = run(Hub::None, Some("box"), Hub::None, None).unwrap();
        assert_eq!(
            p,
            Plan {
                name: "box".into(),
                target_set: Some(TargetSet { name: "box".into(), hub: "lap".into() }),
                this_hub: Some("self".into()),
                fleet: Fleet { hub: "lap".into(), joined: Joined::New },
            }
        );
        assert_eq!(run(Hub::None, None, Hub::None, Some("b")).unwrap().target_set.unwrap().name, "b");
    }

    #[test]
    fn the_same_hub_is_an_extra_link() {
        for (this, target) in [(other("k"), other("k")), (Hub::This, other("lap")), (other("box"), Hub::This)] {
            let p = run(this, Some("box"), target, None).unwrap();
            assert_eq!((p.target_set, p.this_hub, p.fleet.joined), (None, None, Joined::Same), "nothing written");
        }
    }

    #[test]
    fn two_hubs_are_refused_naming_both() {
        let e = run(other("k"), Some("box"), other("l"), None).unwrap_err();
        assert_eq!(e.code, "denied");
        assert_eq!(e.message, "this host is in `k`'s fleet, me@box in `l`'s: a machine is in one fleet only");
        assert!(message(run(Hub::This, Some("box"), Hub::This, None)).contains("`lap`'s fleet, me@box in `box`'s"));
    }

    #[test]
    fn names_are_checked() {
        // In a fleet: `--name` must be its name.
        let m = message(run(Hub::None, Some("box"), other("k"), Some("b2")));
        assert_eq!(m, "me@box is in `k`'s fleet as `box`: pass --name box, or no --name");
        assert!(message(run(other("k"), Some("box"), other("k"), Some("b2"))).contains("as `box`"));
        assert!(message(run(Hub::None, None, other("k"), None)).contains("no member name"));
        assert!(message(run(Hub::None, None, Hub::This, None)).contains("no member name"));
        // Joining: a name or `--name`; never `self`.
        assert!(message(run(Hub::None, None, Hub::None, None)).ends_with("pass --name"));
        assert_eq!(run(Hub::None, Some("box"), Hub::None, Some("self")).unwrap_err().code, "invalid");
        assert_eq!(run(Hub::None, Some("box"), Hub::None, Some("a/b")).unwrap_err().code, "invalid");
        // Never this host's name, in any row.
        for (this, target) in [(Hub::None, Hub::None), (Hub::This, Hub::None), (Hub::None, other("k")), (other("k"), other("k"))] {
            assert!(message(run(this, Some("lap"), target, None)).contains("`lap` is this host's too"));
        }
        // The joiner's name is new to the fleet it joins: this host's
        // machines and hub when the machine joins, the machine's when this
        // host does.
        for (this, known) in [(Hub::None, "old"), (other("k"), "old"), (other("k"), "k")] {
            let m = message(run(this, Some(known), Hub::None, None));
            assert!(m.contains(&format!("`{known}` (me@box) is already a member")) && m.ends_with("pass --name"), "{m}");
        }
        assert!(run(Hub::None, Some("box"), Hub::None, Some("old")).is_err(), "--name too");
        let this_known = names(&["old"]);
        let target_known = names(&["peer", "lap"]);
        let this = This { name: "lap", hub: &Hub::None, known: &this_known };
        let target = Target { label: "me@box", name: Some("box"), hub: &Hub::This, known: &target_known };
        assert!(plan(&this, &target, None).unwrap_err().message.starts_with("this host's member name `lap` is already in `box`'s fleet"));
        let this = This { name: "k", hub: &Hub::None, known: &this_known };
        let target = Target { label: "me@box", name: Some("box"), hub: &other("k"), known: &target_known };
        assert!(plan(&this, &target, None).is_err(), "this host named as the hub");
    }

    #[test]
    fn the_wire() {
        let f = Fleet { hub: "k".into(), joined: Joined::Target };
        assert_eq!(serde_json::to_value(&f).unwrap(), serde_json::json!({"hub": "k", "joined": "target"}));
    }
}
