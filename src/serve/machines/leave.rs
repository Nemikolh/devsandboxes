//! `machine rm` and leaving (docs/architecture/daemon/fleet.md, *How a
//! machine leaves*; fleet plan decision 32): `machines.rm` here, and the
//! `fleet.rm`, `fleet.leave` and `fleet.forget` calls between members over
//! their links (admitted by `fleet::lent::admit_fleet`).
//!
//! A removal runs on the hub: it tells the member (`fleet.leave`: it clears
//! its `hub`, drops its membership log and its own registry entries), tells every other member
//! it links to (`fleet.forget`: each drops its entry for it, or ends its
//! inbound link), then unlinks it here. A member's `machine rm` is sent to
//! the hub as `fleet.rm`; `machine rm` of this host's own name leaves.
//! [`plan`] is the pure part; the rest is the I/O over [`Ctx`].
//!
//! A change that ends the link a request came in on (the member answering
//! `fleet.leave`, the hub answering a leaving member's own `fleet.rm`) is
//! written at once but applied to the links ([`After`]) only once the
//! answer is out, so the caller reads it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::api::{self, Ctx};
use crate::serve::api::{ApiError, parse};
use crate::serve::fleet::member::{self, Hub};
use crate::serve::fleet::membership;
use crate::serve::snapshots::LinkState;

type Answer = Result<Value, ApiError>;

/// What refuses a hub's own leave while it has machines.
pub const HUB_HAS_MEMBERS: &str = "the hub can't leave while it has members: machine rm each first";

/// This host as a removal sees it: its member name, its hub, the machines
/// it links to directly (registry and inbound neighbors), and whether the
/// name asked about is one it sees only through its hub.
pub struct Here<'a> {
    pub name: &'a str,
    pub hub: &'a Hub,
    pub machines: &'a [String],
    pub seen_via_hub: bool,
}

/// What a `rm` of a name does here.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// This host is the hub: take `name` out of the fleet. `tell`: send it
    /// `fleet.leave` (not when it asked itself: it's leaving).
    Remove { name: String, tell: bool },
    /// A member's: the hub runs it (`fleet.rm`).
    AskHub { hub: String },
    /// This host leaves its fleet, telling `hub` first (`None`: this host is
    /// the hub, alone).
    Leave { hub: Option<String> },
}

/// `rm name` here (pure): `from` is the link peer that asked (`fleet.rm`,
/// only ever admitted on the hub), `None` for a local `machines.rm`.
pub fn plan(here: &Here, name: &str, from: Option<&str>) -> Result<Plan, ApiError> {
    let known = here.machines.iter().any(|m| m == name);
    if name == here.name {
        return match here.hub {
            Hub::None => Err(ApiError::failed("this host is in no fleet")),
            Hub::This if from.is_some() => Err(hub_leaves_itself(name)),
            Hub::This if !here.machines.is_empty() => Err(ApiError::denied(HUB_HAS_MEMBERS)),
            Hub::This => Ok(Plan::Leave { hub: None }),
            Hub::Other(hub) => Ok(Plan::Leave { hub: Some(hub.clone()) }),
        };
    }
    match here.hub {
        Hub::This if known => Ok(Plan::Remove { name: name.into(), tell: from != Some(name) }),
        Hub::This => Err(unknown(name)),
        Hub::Other(_) if from.is_some() => Err(ApiError::denied("`fleet.rm` runs on the hub only")),
        Hub::Other(hub) if hub == name => Err(hub_leaves_itself(name)),
        Hub::Other(hub) if known || here.seen_via_hub => Ok(Plan::AskHub { hub: hub.clone() }),
        Hub::None if known => Err(ApiError::failed(format!(
            "this host is in no fleet: `devsandbox machine unlink {name}` drops its link"
        ))),
        _ => Err(unknown(name)),
    }
}

fn hub_leaves_itself(hub: &str) -> ApiError {
    ApiError::denied(format!("`{hub}` is the hub: it leaves on its own host"))
}

fn unknown(name: &str) -> ApiError {
    ApiError::not_found(format!("no machine named `{name}`"))
}

/// How the removed member heard of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Heard {
    /// It answered `fleet.leave`.
    Told,
    /// It asked itself: it's leaving.
    Leaving,
    /// Not reached (`Removed::error` says why): it keeps its `hub` and its
    /// entries until it is.
    Unreachable,
}

/// A member a removal didn't reach, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Missed {
    pub name: String,
    pub error: String,
}

/// `fleet.rm`'s answer, and the hub's part of `machines.rm`'s: what reached
/// whom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    /// Who's out of the fleet.
    pub name: String,
    /// The hub that ran it.
    pub hub: String,
    pub leave: Heard,
    /// Why `name` wasn't reached (`leave` is `unreachable`).
    pub error: Option<String>,
    /// The other members the hub links to that dropped it (`fleet.forget`).
    pub forgot: Vec<String>,
    /// Those it couldn't reach.
    pub missed: Vec<Missed>,
}

/// `machines.rm`'s answer when this host left its fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Left {
    /// This host's member name.
    pub name: String,
    /// The hub of the fleet it left (its own name when it was the hub).
    pub hub: String,
    /// The hub's answer to this host's `fleet.rm`; `None` when this host was
    /// the hub, or the hub wasn't reached (`error`).
    pub removed: Option<Removed>,
    pub error: Option<String>,
    /// This host's registry entries, dropped.
    pub dropped: Vec<String>,
}

/// `machines.rm`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RmAnswer {
    /// The hub took the machine out of the fleet.
    Removed(Removed),
    /// This host left its fleet.
    Left(Left),
}

/// What the links do once an answer is out ([`apply`]).
#[derive(Debug, PartialEq, Eq)]
pub enum After {
    /// This host's `hub` was cleared and its entries dropped: every link
    /// ends (`Ctx::hub_changed`), the registry reloads.
    Left,
    /// This machine's entry was dropped, or it's an inbound neighbor whose
    /// link ends.
    Unlinked(String),
}

/// Apply `after` to the links.
pub fn apply(after: After, ctx: &Ctx) {
    match after {
        After::Left => {
            (ctx.hub_changed)(Hub::None);
            if let Some(links) = &ctx.links {
                links.reload();
            }
        }
        After::Unlinked(name) => {
            if let Some(links) = &ctx.links {
                links.reload();
            }
            if ctx.inbound.contains(&name) {
                (ctx.end_inbound)(&name);
            }
        }
    }
}

#[derive(Deserialize)]
struct NameParams {
    name: String,
}

/// `machines.rm {name}`, a local client's: run on the hub, sent there from
/// a member, or this host leaving.
pub fn rm(params: Value, ctx: &Ctx) -> Answer {
    let p: NameParams = parse(params)?;
    let (me, hub) = api::own_member(ctx)?;
    let machines: Vec<String> = api::machines(ctx)?.into_iter().map(|e| e.name).collect();
    let seen_via_hub = ctx.links.as_ref().is_some_and(|l| l.remotes().via(&p.name).is_some());
    let here = Here { name: &me, hub: &hub, machines: &machines, seen_via_hub };
    let answer = match plan(&here, &p.name, None)? {
        Plan::Remove { name, tell } => {
            let (removed, after) = remove(ctx, &me, &name, tell, &machines)?;
            apply(after, ctx);
            RmAnswer::Removed(removed)
        }
        Plan::AskHub { hub } => {
            let result = call(ctx, &hub, "fleet.rm", json!({ "name": p.name }))
                .map_err(|why| ApiError::failed(format!("the hub `{hub}` can't be reached: {why}")))?;
            RmAnswer::Removed(serde_json::from_value(result).map_err(|e| ApiError::failed(format!("{hub}: fleet.rm: {e}")))?)
        }
        Plan::Leave { hub } => RmAnswer::Left(leave(ctx, &me, hub)?),
    };
    serde_json::to_value(answer).map_err(|e| ApiError::internal(e.into()))
}

/// This host leaves its fleet: the hub told first (best effort), then its
/// `hub` cleared and its registry entries dropped.
fn leave(ctx: &Ctx, me: &str, hub: Option<String>) -> Result<Left, ApiError> {
    let (removed, error) = match &hub {
        None => (None, None),
        Some(hub) => match call(ctx, hub, "fleet.rm", json!({ "name": me })) {
            Ok(v) => match serde_json::from_value::<Removed>(v) {
                Ok(r) => (Some(r), None),
                Err(e) => (None, Some(format!("fleet.rm: unexpected answer: {e}"))),
            },
            Err(why) => (None, Some(why)),
        },
    };
    let dropped = drop_all(ctx)?;
    apply(After::Left, ctx);
    Ok(Left { name: me.into(), hub: hub.unwrap_or_else(|| me.into()), removed, error, dropped })
}

/// Clear this host's `hub`, drop its membership log (`membership.json`:
/// leaving a fleet drops its log) and every registry entry (written, not
/// applied: [`After::Left`]). The entries dropped.
fn drop_all(ctx: &Ctx) -> Result<Vec<String>, ApiError> {
    let path = ctx.member.as_deref().ok_or_else(|| ApiError::failed("this daemon has no member.toml (no data dir)"))?;
    let dropped = super::update(&ctx.registry, |entries| std::mem::take(entries).into_iter().map(|e| e.name).collect());
    let dropped: Vec<String> = dropped.map_err(ApiError::internal)?;
    member::set_fleet(path, None, &Hub::None).map_err(ApiError::internal)?;
    let log = membership::path(path);
    match std::fs::remove_file(&log) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ApiError::internal(anyhow::Error::new(e).context(format!("cannot remove {}", log.display())))),
    }
    Ok(dropped)
}

/// On the hub `hub`: take `name` out of the fleet (see the module doc).
/// `machines` are this host's direct ones. The unlink here is written; the
/// links follow with the returned [`After`].
fn remove(ctx: &Ctx, hub: &str, name: &str, tell: bool, machines: &[String]) -> Result<(Removed, After), ApiError> {
    let (leave, error) = if !tell {
        (Heard::Leaving, None)
    } else {
        match call(ctx, name, "fleet.leave", json!({})) {
            Ok(_) => (Heard::Told, None),
            Err(why) => (Heard::Unreachable, Some(why)),
        }
    };
    let (mut forgot, mut missed) = (Vec::new(), Vec::new());
    for other in machines.iter().filter(|m| *m != name) {
        match call(ctx, other, "fleet.forget", json!({ "name": name })) {
            Ok(_) => forgot.push(other.clone()),
            Err(error) => missed.push(Missed { name: other.clone(), error }),
        }
    }
    unregister(ctx, name)?;
    let removed = Removed { name: name.into(), hub: hub.into(), leave, error, forgot, missed };
    Ok((removed, After::Unlinked(name.into())))
}

/// Drop `name`'s registry entry, if it has one (written, not applied).
fn unregister(ctx: &Ctx, name: &str) -> Result<(), ApiError> {
    super::update(&ctx.registry, |entries| entries.retain(|e| e.name != name)).map_err(ApiError::internal)
}

/// `method` on machine `name` over its link, while it's connected: its
/// result, or why not.
fn call(ctx: &Ctx, name: &str, method: &str, params: Value) -> Result<Value, String> {
    let Some(links) = &ctx.links else { return Err("no links here".into()) };
    match links.remotes().get(name) {
        Some(m) if m.state == LinkState::Connected => {}
        Some(m) => return Err(m.error.unwrap_or_else(|| "its link isn't up".into())),
        None => return Err("no link to it".into()),
    }
    let r = links.forward(name, method, params).map_err(|e| e.message)?;
    if let Some(e) = r.error {
        return Err(format!("{method}: {} ({})", e.message, e.code));
    }
    Ok(r.result.unwrap_or(Value::Null))
}

/// A `fleet.*` call from link peer `from`, admitted
/// (`fleet::lent::admit_fleet`): its answer, and what the links do once
/// it's out.
pub fn fleet_call(method: &str, params: Value, from: &str, ctx: &Ctx) -> (Answer, Option<After>) {
    match fleet_call_inner(method, params, from, ctx) {
        Ok((value, after)) => (Ok(value), after),
        Err(e) => (Err(e), None),
    }
}

fn fleet_call_inner(method: &str, params: Value, from: &str, ctx: &Ctx) -> Result<(Value, Option<After>), ApiError> {
    let ok = json!({ "ok": true });
    match method {
        // From this host's hub: it ends the link the call came on.
        "fleet.leave" => {
            drop_all(ctx)?;
            Ok((ok, Some(After::Left)))
        }
        "fleet.forget" => {
            let p: NameParams = parse(params)?;
            let (me, _) = api::own_member(ctx)?;
            if p.name == me || p.name == from {
                return Err(ApiError::invalid(format!("`fleet.forget` of `{}`: that's this host or its hub", p.name)));
            }
            unregister(ctx, &p.name)?;
            apply(After::Unlinked(p.name), ctx);
            Ok((ok, None))
        }
        "fleet.rm" => {
            let p: NameParams = parse(params)?;
            let (me, hub) = api::own_member(ctx)?;
            let machines: Vec<String> = api::machines(ctx)?.into_iter().map(|e| e.name).collect();
            let here = Here { name: &me, hub: &hub, machines: &machines, seen_via_hub: false };
            let Plan::Remove { name, tell } = plan(&here, &p.name, Some(from))? else {
                return Err(ApiError::denied("`fleet.rm` removes another member"));
            };
            let (removed, after) = remove(ctx, &me, &name, tell, &machines)?;
            let value = serde_json::to_value(removed).map_err(|e| ApiError::internal(e.into()))?;
            // A leaving member's own `fleet.rm` came over the link this ends.
            if tell {
                apply(after, ctx);
                Ok((value, None))
            } else {
                Ok((value, Some(after)))
            }
        }
        m => Err(ApiError::unknown_method(m)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    /// `plan` on host `lap` with hub `hub`, direct machines `machines`.
    fn run(hub: Hub, machines: &[&str], seen: bool, name: &str, from: Option<&str>) -> Result<Plan, ApiError> {
        let machines = names(machines);
        plan(&Here { name: "lap", hub: &hub, machines: &machines, seen_via_hub: seen }, name, from)
    }

    fn code(r: Result<Plan, ApiError>) -> (&'static str, String) {
        let e = r.unwrap_err();
        (e.code, e.message)
    }

    #[test]
    fn the_hub_removes_a_member() {
        assert_eq!(run(Hub::This, &["box"], false, "box", None), Ok(Plan::Remove { name: "box".into(), tell: true }));
        // Asked by another member, or by the member itself (leaving).
        assert_eq!(run(Hub::This, &["box", "x"], false, "box", Some("x")), Ok(Plan::Remove { name: "box".into(), tell: true }));
        assert_eq!(run(Hub::This, &["box"], false, "box", Some("box")), Ok(Plan::Remove { name: "box".into(), tell: false }));
        assert_eq!(code(run(Hub::This, &["box"], false, "nope", None)).0, "not-found");
        // Nobody removes the hub from elsewhere.
        assert_eq!(code(run(Hub::This, &["box"], false, "lap", Some("box"))), ("denied", "`lap` is the hub: it leaves on its own host".into()));
    }

    #[test]
    fn a_member_asks_its_hub() {
        let k = || Hub::Other("k".into());
        assert_eq!(run(k(), &["k", "box"], false, "box", None), Ok(Plan::AskHub { hub: "k".into() }));
        assert_eq!(run(k(), &["k"], true, "x", None), Ok(Plan::AskHub { hub: "k".into() }), "seen through the hub");
        assert_eq!(code(run(k(), &["k"], false, "x", None)).0, "not-found");
        assert_eq!(code(run(k(), &["k"], false, "k", None)), ("denied", "`k` is the hub: it leaves on its own host".into()));
        assert_eq!(code(run(k(), &["k", "box"], false, "box", Some("k"))).0, "denied", "`fleet.rm` on a member");
    }

    #[test]
    fn leaving() {
        assert_eq!(run(Hub::Other("k".into()), &["k", "box"], false, "lap", None), Ok(Plan::Leave { hub: Some("k".into()) }));
        assert_eq!(run(Hub::This, &[], false, "lap", None), Ok(Plan::Leave { hub: None }));
        assert_eq!(code(run(Hub::This, &["box"], false, "lap", None)), ("denied", HUB_HAS_MEMBERS.into()));
        assert_eq!(code(run(Hub::None, &[], false, "lap", None)), ("failed", "this host is in no fleet".into()));
    }

    #[test]
    fn in_no_fleet() {
        let (c, m) = code(run(Hub::None, &["box"], false, "box", None));
        assert_eq!((c, m.as_str()), ("failed", "this host is in no fleet: `devsandbox machine unlink box` drops its link"));
        assert_eq!(code(run(Hub::None, &[], false, "box", None)).0, "not-found");
    }
}
