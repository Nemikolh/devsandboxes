//! `machine rm` and leaving (docs/architecture/daemon/fleet.md, *How a
//! machine leaves*; fleet plan decisions 32 and 49, step 10a.6):
//! `machines.rm` here, and the `fleet.rm`, `fleet.leave` and `fleet.forget`
//! calls between members over their links (admitted by
//! `fleet::lent::admit_fleet`).
//!
//! A machine leaves by a revoke first: `machine rm <m>` on the fleet's
//! authority signs `revoke m`, pushes it (`super::changes`), then runs the
//! removal; typed on another member it's proposed to the authority
//! (`fleet.propose`) and runs when the user approves it there. The removal
//! runs on the control plane: it sends the member and every other member it links to
//! its log first (`fleet.sync`), then tells the member (`fleet.leave`: it
//! clears its `control_plane`, drops its membership log and its own registry
//! entries), tells every other member (`fleet.forget`: each drops its entry
//! for it, or ends its inbound link), then unlinks it here. Each of those
//! follows only a revoke its own log holds; an authority that isn't the control plane
//! asks the control plane with `fleet.rm`. `machine rm` of this host's own name
//! leaves. [`plan`] is the pure part; the rest is the I/O over [`Ctx`].
//!
//! A change that ends the link a request came in on (the member answering
//! `fleet.leave`, the control plane answering a leaving member's own `fleet.rm`) is
//! written at once but applied to the links ([`After`]) only once the
//! answer is out, so the caller reads it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::api::{self, Ctx};
use super::changes::{self, Proposed, Signed};
use super::service::Handle;
use crate::serve::api::{ApiError, parse};
use crate::serve::fleet::member::{self, ControlPlane};
use crate::serve::fleet::membership::{self, Stored};
use crate::serve::fleet::membership_api;
use crate::serve::fleet::proposals::Change;
use crate::serve::snapshots::LinkState;

type Answer = Result<Value, ApiError>;

/// What refuses a control plane's own leave while it has machines.
pub const CONTROL_PLANE_HAS_MEMBERS: &str = "the control plane can't leave while it has members: machine rm each first";

/// Who signs this host's fleet's membership changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority<'a> {
    /// No membership log here.
    None,
    /// This host (its name and key are the log's authority's).
    This,
    Other(&'a str),
}

/// This host as a removal sees it: its member name, its control plane, the machines
/// it links to directly (registry and inbound neighbors), whether the name
/// asked about is a routed machine here (no link of its own,
/// `service::Handle::routed`), the fleet's authority,
/// and whether the name is in its membership log (a member, or revoked).
pub struct Here<'a> {
    pub name: &'a str,
    pub control_plane: &'a ControlPlane,
    pub machines: &'a [String],
    pub routed: bool,
    pub authority: Authority<'a>,
    pub in_log: bool,
}

/// What a `rm` of a name does here.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// `fleet.rm` on the control plane: take `name` out of the fleet. `tell`: send it
    /// `fleet.leave` (not when it asked itself: it's leaving).
    Remove { name: String, tell: bool },
    /// This host is the authority: sign `name`'s revoke, push it, then the
    /// removal (here on the control plane, else the control plane's `fleet.rm`).
    Revoke { name: String },
    /// Another member's: proposed to the authority (`fleet.propose`).
    Propose { authority: String },
    /// This host leaves its fleet, telling `control_plane` first (`None`: this host is
    /// the control plane, alone).
    Leave { control_plane: Option<String> },
}

/// `rm name` here (pure): `from` is the link peer that asked (`fleet.rm`,
/// only ever admitted on the control plane), `None` for a local `machines.rm`.
pub fn plan(here: &Here, name: &str, from: Option<&str>) -> Result<Plan, ApiError> {
    let known = here.machines.iter().any(|m| m == name) || here.in_log;
    if name == here.name {
        return match here.control_plane {
            ControlPlane::None => Err(ApiError::failed("this host is in no fleet")),
            ControlPlane::This if from.is_some() => Err(control_plane_leaves_itself(name)),
            ControlPlane::This if !here.machines.is_empty() => Err(ApiError::denied(CONTROL_PLANE_HAS_MEMBERS)),
            ControlPlane::This => Ok(Plan::Leave { control_plane: None }),
            ControlPlane::Other(control_plane) => Ok(Plan::Leave { control_plane: Some(control_plane.clone()) }),
        };
    }
    if from.is_some() {
        return match here.control_plane {
            ControlPlane::This if known => Ok(Plan::Remove { name: name.into(), tell: from != Some(name) }),
            ControlPlane::This => Err(unknown(name)),
            _ => Err(ApiError::denied("`fleet.rm` runs on the control plane only")),
        };
    }
    match here.control_plane {
        ControlPlane::None if known => {
            return Err(ApiError::failed(format!("this host is in no fleet: `devsandbox machine unlink {name}` drops its link")));
        }
        ControlPlane::None => return Err(unknown(name)),
        ControlPlane::Other(control_plane) if control_plane == name => return Err(control_plane_named(name)),
        _ => {}
    }
    match here.authority {
        Authority::None => Err(ApiError::failed(
            "this host's fleet has no membership log: make the fleet again with `devsandbox machine add`",
        )),
        Authority::Other(a) if a == name => Err(ApiError::denied(format!("`{name}` is the fleet's authority: it can't be removed"))),
        _ if !(known || here.routed) => Err(unknown(name)),
        Authority::This => Ok(Plan::Revoke { name: name.into() }),
        Authority::Other(a) => Ok(Plan::Propose { authority: a.into() }),
    }
}

fn control_plane_leaves_itself(control_plane: &str) -> ApiError {
    ApiError::denied(format!("`{control_plane}` is the control plane: it leaves on its own host"))
}

fn control_plane_named(control_plane: &str) -> ApiError {
    ApiError::denied(format!("`{control_plane}` is the control plane: move the role first (`devsandbox machine control-plane <m>`)"))
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
    /// Not reached (`Removed::error` says why): it keeps its `control_plane` and its
    /// entries until it is.
    Unreachable,
}

/// A member a removal (or a push) didn't reach, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Missed {
    pub name: String,
    pub error: String,
}

/// `fleet.rm`'s answer, and the removal part of `machines.rm`'s: what
/// reached whom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    /// Who's out of the fleet.
    pub name: String,
    /// The control plane that ran it.
    pub control_plane: String,
    pub leave: Heard,
    /// Why `name` wasn't reached (`leave` is `unreachable`).
    pub error: Option<String>,
    /// The other members the control plane links to that dropped it (`fleet.forget`).
    pub forgot: Vec<String>,
    /// Those it couldn't reach, or that refused.
    pub missed: Vec<Missed>,
}

/// `machines.rm`'s answer when this host left its fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Left {
    /// This host's member name.
    pub name: String,
    /// The control plane of the fleet it left (its own name when it was the control plane).
    pub control_plane: String,
    /// The control plane's answer to this host's `fleet.rm`; `None` when this host was
    /// the control plane, or the control plane wasn't reached (`error`).
    pub removed: Option<Removed>,
    pub error: Option<String>,
    /// This host's registry entries, dropped.
    pub dropped: Vec<String>,
    /// Its own revoke proposed to the authority (another host's), best
    /// effort: its id, or why not.
    pub proposed: Option<Proposed>,
}

/// `machines.rm`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RmAnswer {
    /// On the authority: the revoke signed and pushed, then the removal.
    Revoked(Signed),
    /// On another member: the revoke waits for approval on the authority.
    Proposed(Proposed),
    /// This host left its fleet.
    Left(Left),
}

/// What the links do once an answer is out ([`apply`]).
#[derive(Debug, PartialEq, Eq)]
pub enum After {
    /// This host's `control_plane` was cleared and its entries dropped: every link
    /// ends (`Ctx::control_plane_changed`), the registry reloads.
    Left,
    /// This machine's entry was dropped, or it's an inbound neighbor whose
    /// link ends.
    Unlinked(String),
    /// A `fleet.sync` promoted this host's next key: every link it's on
    /// ends (`Ctx::key_rotated`), after the answer to the sync is out.
    Rotated,
    /// A `fleet.sync` moved this host's control plane: it's followed
    /// (`Ctx::control_plane_changed`, every link ending so both ends redial saying
    /// it) after the answer to the sync is out and the log was sent on.
    ControlPlane(ControlPlane),
}

/// Apply `after` to the links.
pub fn apply(after: After, ctx: &Ctx) {
    match after {
        After::Left => {
            (ctx.control_plane_changed)(ControlPlane::None);
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
        After::Rotated => (ctx.key_rotated)(),
        After::ControlPlane(control_plane) => (ctx.control_plane_changed)(control_plane),
    }
}

#[derive(Deserialize)]
struct NameParams {
    name: String,
}

/// This host's `member.toml`.
pub(super) fn member_toml(ctx: &Ctx) -> Result<&std::path::Path, ApiError> {
    ctx.member.as_deref().ok_or_else(|| ApiError::failed("this daemon has no member.toml (no data dir)"))
}

/// Who signs `stored`'s changes, seen from `me` (key `key`).
pub fn authority_of<'a>(stored: Option<&'a Stored>, me: &str, key: &crate::serve::fleet::identity::PublicKey) -> Authority<'a> {
    match stored.and_then(|s| s.log.membership()) {
        None => Authority::None,
        Some(m) if m.authority.0 == me && m.is_authority(key) => Authority::This,
        Some(m) => Authority::Other(&m.authority.0),
    }
}

/// Whether `stored`'s log has `name`: a member, or revoked.
fn in_log(stored: Option<&Stored>, name: &str) -> bool {
    stored.is_some_and(|s| s.log.membership().is_some_and(|m| m.members.contains_key(name)) || s.log.revoked_name(name))
}

/// `machines.rm {name}`, a local client's: a revoke on the authority, a
/// proposal from another member, or this host leaving.
pub fn rm(params: Value, ctx: &Ctx) -> Answer {
    let p: NameParams = parse(params)?;
    let (me, control_plane) = api::own_member(ctx)?;
    let path = member_toml(ctx)?;
    let machines: Vec<String> = api::machines(ctx)?.into_iter().map(|e| e.name).collect();
    let routed = ctx.links.as_ref().is_some_and(|l| l.routed_one(&p.name).is_some());
    let stored = membership_api::load(path)?;
    let (_, key) = membership_api::own(path, &ctx.hostname)?;
    let authority = authority_of(stored.as_ref(), &me, &crate::serve::fleet::identity::Signer::public_key(&key));
    let here = Here { name: &me, control_plane: &control_plane, machines: &machines, routed, authority, in_log: in_log(stored.as_ref(), &p.name) };
    let answer = match plan(&here, &p.name, None)? {
        Plan::Revoke { name } => RmAnswer::Revoked(changes::revoke(ctx, &name)?),
        Plan::Propose { authority } => {
            let id = changes::propose(ctx, &authority, Change::Revoke { name: p.name.clone() })?;
            RmAnswer::Proposed(Proposed { name: p.name, authority, id: Some(id), error: None })
        }
        Plan::Leave { control_plane } => {
            // Its own revoke, while the links are still up.
            let proposed = match authority {
                Authority::Other(a) => Some(match changes::propose(ctx, a, Change::Revoke { name: me.clone() }) {
                    Ok(id) => Proposed { name: me.clone(), authority: a.into(), id: Some(id), error: None },
                    Err(e) => Proposed { name: me.clone(), authority: a.into(), id: None, error: Some(e.message) },
                }),
                _ => None,
            };
            RmAnswer::Left(leave(ctx, &me, control_plane, proposed)?)
        }
        Plan::Remove { .. } => unreachable!("only `fleet.rm` removes without a revoke here"),
    };
    serde_json::to_value(answer).map_err(|e| ApiError::internal(e.into()))
}

/// This host leaves its fleet: the control plane told first (best effort), then its
/// `control_plane` cleared and its registry entries dropped.
fn leave(ctx: &Ctx, me: &str, control_plane: Option<String>, proposed: Option<Proposed>) -> Result<Left, ApiError> {
    let (removed, error) = match &control_plane {
        None => (None, None),
        Some(control_plane) => match call(ctx, control_plane, "fleet.rm", json!({ "name": me })) {
            Ok(v) => match serde_json::from_value::<Removed>(v) {
                Ok(r) => (Some(r), None),
                Err(e) => (None, Some(format!("fleet.rm: unexpected answer: {e}"))),
            },
            Err(why) => (None, Some(why)),
        },
    };
    let dropped = drop_all(ctx)?;
    apply(After::Left, ctx);
    Ok(Left { name: me.into(), control_plane: control_plane.unwrap_or_else(|| me.into()), removed, error, dropped, proposed })
}

/// Clear this host's `control_plane`, drop its membership log (`membership.json`:
/// leaving a fleet drops its log) and every registry entry (written, not
/// applied: [`After::Left`]). The entries dropped.
fn drop_all(ctx: &Ctx) -> Result<Vec<String>, ApiError> {
    let path = member_toml(ctx)?;
    let dropped = super::update(&ctx.registry, |entries| std::mem::take(entries).into_iter().map(|e| e.name).collect());
    let dropped: Vec<String> = dropped.map_err(ApiError::internal)?;
    member::set_fleet(path, None, &ControlPlane::None).map_err(ApiError::internal)?;
    let log = membership::path(path);
    match std::fs::remove_file(&log) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ApiError::internal(anyhow::Error::new(e).context(format!("cannot remove {}", log.display())))),
    }
    Ok(dropped)
}

/// On the control plane `control_plane`: take `name` out of the fleet (see the module doc).
/// `machines` are this host's direct ones; the members it tells are those
/// and its routed ones ([`forget_targets`]), each over any path ([`call`]).
/// Each it tells gets this host's log first (`fleet.sync`, best effort),
/// so the revoke its `fleet.leave` / `fleet.forget` follows is there even
/// before a push reached it. The unlink here is written; the links follow
/// with the returned [`After`].
pub(super) fn remove(ctx: &Ctx, control_plane: &str, name: &str, tell: bool, machines: &[String]) -> Result<(Removed, After), ApiError> {
    let log = membership_api::load(member_toml(ctx)?)?.map(|s| serde_json::to_value(s.to_wire()));
    let log = log.transpose().map_err(|e| ApiError::internal(e.into()))?;
    let sync = |to: &str| {
        if let Some(log) = &log {
            let _ = call(ctx, to, "fleet.sync", log.clone());
        }
    };
    let (leave, error) = if !tell {
        (Heard::Leaving, None)
    } else {
        sync(name);
        match call(ctx, name, "fleet.leave", json!({})) {
            Ok(_) => (Heard::Told, None),
            Err(why) => (Heard::Unreachable, Some(why)),
        }
    };
    let (mut forgot, mut missed) = (Vec::new(), Vec::new());
    let routed: Vec<String> = ctx.links.as_ref().map(|l| l.routed().into_iter().map(|r| r.name).collect()).unwrap_or_default();
    for other in &forget_targets(machines, &routed, control_plane, name) {
        sync(other);
        match call(ctx, other, "fleet.forget", json!({ "name": name })) {
            Ok(_) => forgot.push(other.clone()),
            Err(error) => missed.push(Missed { name: other.clone(), error }),
        }
    }
    unregister(ctx, name)?;
    let removed = Removed { name: name.into(), control_plane: control_plane.into(), leave, error, forgot, missed };
    Ok((removed, After::Unlinked(name.into())))
}

/// Who a removal of `name` on control plane `control_plane` sends `fleet.forget` (pure): the
/// control plane's direct `machines`, then its `routed` members (graph members with
/// no link of their own here) that aren't among them; never `name` or the
/// control plane itself.
fn forget_targets(machines: &[String], routed: &[String], control_plane: &str, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in machines.iter().chain(routed) {
        if m != name && m != control_plane && !out.contains(m) {
            out.push(m.clone());
        }
    }
    out
}

/// Drop `name`'s registry entry, if it has one (written, not applied).
fn unregister(ctx: &Ctx, name: &str) -> Result<(), ApiError> {
    super::update(&ctx.registry, |entries| entries.retain(|e| e.name != name)).map_err(ApiError::internal)
}

/// `method` on member `name` over any path: its link while connected,
/// else a route over this host's link graph (`service::Handle::call_member`):
/// its result, or why not. Every membership call to a member goes this
/// way (`fleet.propose`, `fleet.rm`, `fleet.leave`, `fleet.forget` and the
/// `fleet.sync` before them).
pub(super) fn call(ctx: &Ctx, name: &str, method: &str, params: Value) -> Result<Value, String> {
    match &ctx.links {
        Some(links) => answer_of(method, links.call_member(name, method, params)?),
        None => Err("no links here".into()),
    }
}

/// `method` on machine `name` over its own link only, while it's
/// connected: its result, or why not. What stays on direct links: a push's
/// flood (`changes::push`) and the link reports (`fleet::reports`).
pub(crate) fn call_on(links: &Handle, name: &str, method: &str, params: Value) -> Result<Value, String> {
    direct_up(links, name)?;
    answer_of(method, links.forward(name, method, params).map_err(|e| e.message)?)
}

/// [`call_on`], its answer waited for up to `wait` (`Handle::forward_within`).
pub(crate) fn call_on_within(links: &Handle, name: &str, method: &str, params: Value, wait: std::time::Duration) -> Result<Value, String> {
    direct_up(links, name)?;
    answer_of(method, links.forward_within(name, method, params, wait).map_err(|e| e.message)?)
}

fn direct_up(links: &Handle, name: &str) -> Result<(), String> {
    match links.remotes().get(name) {
        Some(m) if m.state == LinkState::Connected => Ok(()),
        Some(m) => Err(m.error.unwrap_or_else(|| "its link isn't up".into())),
        None => Err("no link to it".into()),
    }
}

fn answer_of(method: &str, r: crate::serve::proto::Response) -> Result<Value, String> {
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
        // From this host's control plane: it ends the link the call came on.
        "fleet.leave" => {
            drop_all(ctx)?;
            Ok((ok, Some(After::Left)))
        }
        "fleet.forget" => {
            let p: NameParams = parse(params)?;
            let (me, _) = api::own_member(ctx)?;
            if p.name == me || p.name == from {
                return Err(ApiError::invalid(format!("`fleet.forget` of `{}`: that's this host or its control plane", p.name)));
            }
            unregister(ctx, &p.name)?;
            apply(After::Unlinked(p.name), ctx);
            Ok((ok, None))
        }
        "fleet.rm" => {
            let p: NameParams = parse(params)?;
            let (me, control_plane) = api::own_member(ctx)?;
            let machines: Vec<String> = api::machines(ctx)?.into_iter().map(|e| e.name).collect();
            let stored = membership_api::load(member_toml(ctx)?)?;
            let in_log = in_log(stored.as_ref(), &p.name);
            let here = Here { name: &me, control_plane: &control_plane, machines: &machines, routed: false, authority: Authority::None, in_log };
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
        "fleet.propose" | "fleet.sync" => changes::fleet_call(method, params, from, ctx),
        m => Err(ApiError::unknown_method(m)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    /// `plan` on host `lap` with control plane `control_plane`, direct machines `machines`, the
    /// fleet's authority `authority`, `in_log` whether the name is in its log.
    fn plan_on(control_plane: ControlPlane, machines: &[&str], seen: bool, authority: Authority, in_log: bool, name: &str, from: Option<&str>) -> Result<Plan, ApiError> {
        let machines = names(machines);
        plan(&Here { name: "lap", control_plane: &control_plane, machines: &machines, routed: seen, authority, in_log }, name, from)
    }

    /// [`plan_on`] with `lap` the authority, the name not in the log.
    fn run(control_plane: ControlPlane, machines: &[&str], seen: bool, name: &str, from: Option<&str>) -> Result<Plan, ApiError> {
        plan_on(control_plane, machines, seen, Authority::This, false, name, from)
    }

    fn code(r: Result<Plan, ApiError>) -> (&'static str, String) {
        let e = r.unwrap_err();
        (e.code, e.message)
    }

    #[test]
    fn the_authority_revokes() {
        assert_eq!(run(ControlPlane::This, &["box"], false, "box", None), Ok(Plan::Revoke { name: "box".into() }));
        // In the log, not linked: revoked all the same.
        assert_eq!(plan_on(ControlPlane::This, &[], false, Authority::This, true, "pi", None), Ok(Plan::Revoke { name: "pi".into() }));
        assert_eq!(run(ControlPlane::Other("k".into()), &["k", "box"], false, "box", None), Ok(Plan::Revoke { name: "box".into() }));
        assert_eq!(code(run(ControlPlane::This, &["box"], false, "nope", None)).0, "not-found");
        // The control plane: another control plane first.
        assert_eq!(
            code(run(ControlPlane::Other("k".into()), &["k"], false, "k", None)),
            ("denied", "`k` is the control plane: move the role first (`devsandbox machine control-plane <m>`)".into())
        );
    }

    #[test]
    fn the_control_plane_removes_on_fleet_rm() {
        // Asked by another member (the authority, after its revoke), or by the member itself (leaving).
        assert_eq!(run(ControlPlane::This, &["box", "x"], false, "box", Some("x")), Ok(Plan::Remove { name: "box".into(), tell: true }));
        assert_eq!(run(ControlPlane::This, &["box"], false, "box", Some("box")), Ok(Plan::Remove { name: "box".into(), tell: false }));
        assert_eq!(
            plan_on(ControlPlane::This, &[], false, Authority::None, true, "pi", Some("x")),
            Ok(Plan::Remove { name: "pi".into(), tell: true }),
            "in the log only"
        );
        assert_eq!(code(run(ControlPlane::This, &["box"], false, "nope", Some("x"))).0, "not-found");
        // Nobody removes the control plane from elsewhere.
        assert_eq!(code(run(ControlPlane::This, &["box"], false, "lap", Some("box"))), ("denied", "`lap` is the control plane: it leaves on its own host".into()));
        assert_eq!(code(run(ControlPlane::Other("k".into()), &["k", "box"], false, "box", Some("k"))).0, "denied", "`fleet.rm` on a member");
    }

    #[test]
    fn a_member_proposes_to_the_authority() {
        let k = || ControlPlane::Other("k".into());
        let a = Authority::Other("auth");
        let propose = Ok(Plan::Propose { authority: "auth".into() });
        assert_eq!(plan_on(k(), &["k", "box"], false, a, false, "box", None), propose);
        assert_eq!(plan_on(k(), &["k"], true, a, false, "x", None), propose, "seen through the control plane");
        assert_eq!(plan_on(k(), &["k"], false, a, true, "x", None), propose, "in the log");
        assert_eq!(code(plan_on(k(), &["k"], false, a, false, "x", None)).0, "not-found");
        assert_eq!(code(plan_on(k(), &["k", "auth"], false, a, true, "auth", None)), ("denied", "`auth` is the fleet's authority: it can't be removed".into()));
        assert_eq!(code(plan_on(k(), &["k"], false, a, true, "k", None)).0, "denied", "the control plane");
        // A fleet with no log.
        assert_eq!(code(plan_on(k(), &["k", "box"], false, Authority::None, false, "box", None)).0, "failed");
    }

    #[test]
    fn leaving() {
        assert_eq!(run(ControlPlane::Other("k".into()), &["k", "box"], false, "lap", None), Ok(Plan::Leave { control_plane: Some("k".into()) }));
        assert_eq!(plan_on(ControlPlane::Other("k".into()), &["k"], false, Authority::Other("k"), true, "lap", None), Ok(Plan::Leave { control_plane: Some("k".into()) }));
        assert_eq!(run(ControlPlane::This, &[], false, "lap", None), Ok(Plan::Leave { control_plane: None }));
        assert_eq!(code(run(ControlPlane::This, &["box"], false, "lap", None)), ("denied", CONTROL_PLANE_HAS_MEMBERS.into()));
        assert_eq!(code(run(ControlPlane::None, &[], false, "lap", None)), ("failed", "this host is in no fleet".into()));
    }

    #[test]
    fn the_control_plane_forgets_on_direct_and_routed_members() {
        assert_eq!(forget_targets(&names(&["box", "x"]), &names(&["lap", "x", "pi"]), "b", "pi"), names(&["box", "x", "lap"]));
        assert_eq!(forget_targets(&[], &names(&["b", "lap"]), "b", "zed"), names(&["lap"]), "never the control plane itself");
    }

    #[test]
    fn in_no_fleet() {
        let (c, m) = code(run(ControlPlane::None, &["box"], false, "box", None));
        assert_eq!((c, m.as_str()), ("failed", "this host is in no fleet: `devsandbox machine unlink box` drops its link"));
        assert_eq!(code(run(ControlPlane::None, &[], false, "box", None)).0, "not-found");
    }
}
