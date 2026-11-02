//! The `machines.*` methods (docs/architecture/daemon/machines.md,
//! *`devsandbox machine add|ls|set|unlink|rm|trust|roots`*): `probe`, `add`,
//! `list`, `get`, `set`, `unlink`, and `rm` (taking a machine out of the
//! fleet, [`super::leave`]).
//! Connection-free like `serve::api`: a function of the method, its params
//! and a [`Ctx`] (the registry path and the [`Dial`]s), so tests run them
//! against a second test daemon standing in as the machine.
//!
//! `probe` and `add` open a fresh connection each (ssh at add time,
//! [`Ctx::probe`]: the hello, one or two calls) and drop it; the held link
//! per machine is `service`'s, which
//! `add`, `set`, `unlink` and `rm` tell to reload and `list` reads the live state of. `add`
//! checks the typed roots *on the machine* (`roots.list {paths}`): only it
//! can tell whether they exist, expand a quoted `~`, and canonicalize them.
//! It also applies the join rule ([`super::join`]) over this host's
//! `member.toml` and the machine's `member.get` and `machines.list`, asked
//! on the same probe, checks the machine's key is the one the user
//! confirmed (`key`), and plans both ends' membership logs
//! ([`join::logs`]) over this host's and the machine's `membership.get`:
//! every check first, then the machine's `member.set {name}` and its
//! membership (`membership.adopt`, or `membership.sign` of this host),
//! this host's registry entry, and this host's membership, whose adopt
//! writes its `hub`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::inbound::{self, Neighbors};
use super::join::{self, Fleet, Logs};
use super::link::{self, Link};
use super::service::Handle;
use super::ssh::{self, Dial};
use super::{Entry, MachineKind, check_name};
use crate::serve::api::{ApiError, RootCandidate, parse};
use crate::serve::capacity::Capacity;
use crate::serve::fleet::identity::{PublicKey, Signer};
use crate::serve::fleet::member::{self, Hub, MemberInfo};
use crate::serve::fleet::membership::{Introduction, Stored, Wire};
use crate::serve::fleet::proposals::Change;
use crate::serve::fleet::membership_api::{self, MembershipInfo};
use crate::serve::snapshots::LinkState;

/// The hello's wait: ssh's connect and auth, the remote login shell, and a
/// lazy start of the remote daemon.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

type Answer = Result<Value, ApiError>;

/// What the methods need from the daemon.
pub struct Ctx {
    /// `machines.toml` ([`super::path`] outside tests).
    pub registry: PathBuf,
    /// A registered machine's link (`fleet::dialer::dial`): forwarded
    /// verbs and placement's capacity asks dial it.
    pub dial: Dial,
    /// A target at add time, before it has a link (`probe`, `add`): ssh.
    pub probe: Dial,
    /// This daemon's semver, which `add` warns a differing machine's against.
    pub version: String,
    /// The links: `add`/`set`/`rm` reload them, `list` reports their state. `None`
    /// only where no service runs (then every machine lists as `idle`).
    pub links: Option<Handle>,
    /// The inbound neighbors up now (`links.inbound`): `list` shows them,
    /// `add` refuses their names.
    pub inbound: Arc<Neighbors>,
    /// This host's `member.toml` (the daemon's): `add` reads its name and
    /// hub, its key (`member.key`) and membership log (`membership.json`)
    /// beside it, and adopts the log the add makes, which writes `hub`.
    /// `None` without a data dir (`add` refuses).
    pub member: Option<PathBuf>,
    /// What names this host when `member.toml` doesn't.
    pub hostname: String,
    /// `add` (an adopted log) or a leave wrote this host's `hub`: the
    /// daemon follows it as after a `membership.adopt`
    /// (`daemon::Shared::set_hub`).
    pub hub_changed: Box<dyn Fn(Hub) + Send + Sync>,
    /// This host's next key was promoted (a rotate of it stored): every
    /// link it's on ends and redials proving the new key
    /// (`daemon::Shared::key_rotated`).
    pub key_rotated: Box<dyn Fn() + Send + Sync>,
    /// End inbound neighbor `name`'s link: its `fleet-link`'s held
    /// `links.inbound` connection gets `links.cycle` and hangs up
    /// (`rm`, `fleet.forget`).
    pub end_inbound: Box<dyn Fn(&str) + Send + Sync>,
    /// Tell the user on this host's desktop (a proposal waiting for
    /// approval, `changes`): `devsbd::desktop` on the host side, else only
    /// logged.
    pub notify: Box<dyn Fn(&str) + Send + Sync>,
}

/// Run `method` if it's a `machines.*` one; `None` otherwise.
pub fn call(method: &str, params: Value, ctx: &Ctx) -> Option<Answer> {
    let answer = match method {
        "machines.probe" => parse(params).and_then(|p| probe(p, ctx)),
        "machines.add" => parse(params).and_then(|p| add(p, ctx)),
        "machines.list" => list(ctx),
        "machines.get" => parse(params).and_then(|p| get(p, ctx)),
        "machines.set" => parse(params).and_then(|p| set(p, ctx)),
        "machines.unlink" => parse(params).and_then(|p| unlink(p, ctx)),
        "machines.rm" => super::leave::rm(params, ctx),
        _ => return None,
    };
    Some(answer)
}

/// `machines.probe`'s answer: the machine's devsandbox, its candidate
/// roots, and its member key (what `machine add` shows the user to
/// confirm, then passes as `machines.add`'s `key`).
#[derive(Debug, Serialize, Deserialize)]
pub struct Probed {
    pub version: String,
    pub protocol: u32,
    pub roots: Vec<RootCandidate>,
    pub key: PublicKey,
    pub fingerprint: String,
}

/// `machines.add`'s answer.
#[derive(Debug, Serialize, Deserialize)]
pub struct Added {
    pub name: String,
    /// As saved: the machine's canonical paths, deduplicated.
    pub roots: Vec<PathBuf>,
    /// The machine's devsandbox semver.
    pub version: String,
    /// Set when that semver differs from this daemon's (same protocol, so
    /// allowed).
    pub warning: Option<String>,
    /// The fleet both ends are in now, and who joined it.
    pub fleet: Fleet,
    /// When this host (not the authority) introduced the machine: that
    /// introduction proposed to the authority (`fleet.propose`, best
    /// effort), its id there or why not.
    #[serde(default)]
    pub proposed: Option<super::changes::Proposed>,
}

/// One `machines.list` row: the registry entry, or an inbound neighbor up
/// now (`kind` `inbound`: no `target`, `roots` its `roots.list`), and its
/// held link's live state (`error` the last failure while `unreachable`;
/// `version` from the last hello).
#[derive(Debug, Serialize, Deserialize)]
pub struct MachineRow {
    pub name: String,
    pub kind: MachineKind,
    /// `None` for an inbound neighbor.
    pub target: Option<String>,
    pub roots: Vec<PathBuf>,
    pub command: Option<String>,
    pub agent: bool,
    pub state: LinkState,
    pub error: Option<String>,
    pub version: Option<String>,
    /// Its host's capacity, from its last pushed snapshot
    /// (`snapshots::Machine::capacity`).
    #[serde(default)]
    pub capacity: Option<Capacity>,
    /// Who introduced it, while this host's membership holds it as an
    /// introduction, not in the log (capped: it introduces no one, fleet
    /// plan decision 51).
    #[serde(default)]
    pub introduced_by: Option<String>,
}

/// Who introduced member `name`, when `stored` holds it as an introduction
/// (pure).
pub fn introducer_of(stored: Option<&Stored>, name: &str) -> Option<String> {
    stored?.introductions.iter().find(|i| i.name == name).map(|i| i.introducer.clone())
}

#[derive(Deserialize)]
struct ProbeParams {
    target: String,
    #[serde(default)]
    command: Option<String>,
}

fn probe(p: ProbeParams, ctx: &Ctx) -> Answer {
    ssh::parse_target(&p.target).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    check_command(p.command.as_deref())?;
    // No name yet: errors say the target.
    let entry = Entry { command: p.command, ..Entry::ssh(p.target.clone(), p.target) };
    let mut link = connect(&entry, ctx)?;
    let roots: Vec<RootCandidate> = remote_call(&mut link, &entry.name, "roots.list", json!({}))?;
    let member: MemberInfo = remote_call(&mut link, &entry.name, "member.get", json!({}))?;
    let remote = link.remote();
    to_value(Probed { version: remote.version.clone(), protocol: remote.protocol, roots, key: member.key, fingerprint: member.fingerprint })
}

#[derive(Deserialize)]
struct AddParams {
    target: String,
    #[serde(default)]
    name: Option<String>,
    roots: Vec<String>,
    /// `--hub`: the machine is to be the hub (`join::plan`).
    hub: bool,
    /// The machine's member key as the user confirmed it (its fingerprint,
    /// from `machines.probe`): the add is refused if the machine's differs.
    key: PublicKey,
    #[serde(default)]
    agent: bool,
    #[serde(default)]
    command: Option<String>,
}

fn add(p: AddParams, ctx: &Ctx) -> Answer {
    let invalid = |e: anyhow::Error| ApiError::invalid(format!("{e:#}"));
    ssh::parse_target(&p.target).map_err(invalid)?;
    if let Some(name) = &p.name {
        check_name(name).map_err(invalid)?;
    }
    check_command(p.command.as_deref())?;
    // A `--name` is the registry name if the add goes through: checked
    // before the ssh, so a taken one fails fast; again at the save.
    if let Some(name) = &p.name {
        check_free(ctx, name, true)?;
    }
    let (this_name, this_hub) = own_member(ctx)?;
    let member_toml = ctx.member.as_deref().expect("own_member checked it");

    // No name yet: until the machine says its own, errors say the target.
    let probe = Entry { command: p.command.clone(), ..Entry::ssh(p.target.clone(), p.target.clone()) };
    let mut link = connect(&probe, ctx)?;
    let target: MemberInfo = remote_call(&mut link, &p.target, "member.get", json!({}))?;
    if target.key != p.key {
        return Err(ApiError::denied(format!(
            "{}'s key fingerprint is {}, not the confirmed {}: nothing written",
            p.target,
            target.fingerprint,
            p.key.fingerprint()
        )));
    }
    let target_rows: Vec<MachineRow> = remote_call(&mut link, &p.target, "machines.list", Value::Null)?;
    let target_known: Vec<String> = target_rows.into_iter().map(|r| r.name).collect();
    let this_known: Vec<String> = machines(ctx)?.into_iter().map(|e| e.name).collect();
    let target_hub = Hub::of(target.hub.as_deref().unwrap_or_default());
    let plan = join::plan(
        &join::This { name: &this_name, hub: &this_hub, known: &this_known },
        &join::Target { label: &p.target, name: target.name.as_deref(), hub: &target_hub, known: &target_known },
        p.name.as_deref(),
        p.hub,
    )?;
    check_free(ctx, &plan.name, plan.target_set.is_some())?;

    let mut entry = Entry { command: p.command, agent: p.agent, ..Entry::ssh(plan.name.clone(), p.target) };
    let checked: Vec<RootCandidate> = remote_call(&mut link, &entry.name, "roots.list", json!({ "paths": p.roots }))?;
    // A daemon that ignores `paths` answers its candidates instead.
    if checked.len() != p.roots.len() {
        return Err(ApiError::failed(format!("{}: its devsandbox can't check roots; upgrade it", entry.name)));
    }
    let refused: Vec<String> = checked
        .iter()
        .zip(&p.roots)
        .filter_map(|(c, given)| c.error.as_ref().map(|e| format!("`{given}`: {e}")))
        .collect();
    if !refused.is_empty() {
        return Err(ApiError::invalid(format!("{}: {}", entry.name, refused.join("; "))));
    }
    for c in checked {
        if !entry.roots.contains(&c.root) {
            entry.roots.push(c.root);
        }
    }
    let version = link.remote().version.clone();

    // Both ends' logs, checked before anything is written.
    let (_, signer) = membership_api::own(member_toml, &ctx.hostname)?;
    let ours = membership_api::load(member_toml)?.unwrap_or_default();
    let theirs: MembershipInfo = remote_call(&mut link, &entry.name, "membership.get", json!({}))?;
    let theirs = theirs
        .verified()
        .map_err(|e| ApiError::failed(format!("{}: membership.get: a log that doesn't verify: {e:#}", entry.name)))?;
    let this_side = join::Side { name: &this_name, key: signer.public_key(), stored: &ours };
    let target_side = join::Side { name: &plan.name, key: p.key, stored: &theirs };
    let logs = join::logs(&plan.fleet, &this_side, &signer, &target_side, &entry.name)?;

    // Every check passed: the writes, the machine's first.
    let mut written: Vec<String> = Vec::new();
    if let Some(set) = &plan.target_set {
        let _: MemberInfo = remote_call(&mut link, &entry.name, "member.set", json!({ "name": set.name }))?;
        written.push(format!("{}'s member.toml (name `{}`)", entry.name, set.name));
    }
    let adopt_there = |link: &mut Link, stored: &Stored, written: &[String]| -> Result<(), ApiError> {
        let _: MembershipInfo =
            remote_call(link, &entry.name, "membership.adopt", to_value(stored.to_wire())?).map_err(|e| after(e, written))?;
        Ok(())
    };
    // What this host adopts once its entry is saved.
    let here: Stored = match logs {
        Logs::Both(next) => {
            adopt_there(&mut link, &next, &written)?;
            next
        }
        Logs::Exchange => {
            adopt_there(&mut link, &ours, &written)?;
            theirs
        }
        Logs::Signed => {
            let params = json!({ "name": this_name, "key": signer.public_key() });
            let signed: Wire = remote_call(&mut link, &entry.name, "membership.sign", params).map_err(|e| after(e, &written))?;
            Stored::from_wire(signed)
                .map_err(|e| after(ApiError::failed(format!("{}: membership.sign: a log that doesn't verify: {e:#}", entry.name)), &written))?
        }
    };
    written.push(format!("{}'s membership.json", entry.name));
    drop(link);

    let saved = super::update(&ctx.registry, |entries| {
        if entries.iter().any(|e| e.name == entry.name) {
            return Err(taken(&entry.name, false));
        }
        if ctx.inbound.contains(&entry.name) {
            return Err(inbound::taken(&entry.name));
        }
        entries.push(entry.clone());
        Ok(())
    });
    saved.map_err(ApiError::internal).and_then(|r| r).map_err(|e| after(e, &written))?;
    written.push(format!("this host's machines.toml (`{}`)", entry.name));
    // Its `hub` (and the name the fleet knows it by) from the log.
    let taken = membership_api::adopt(member_toml, &ctx.hostname, &here).map_err(|e| after(e, &written))?;
    let hub = taken.hub;
    if taken.rotated {
        (ctx.key_rotated)();
    }
    (ctx.hub_changed)(hub);
    if let Some(links) = &ctx.links {
        links.reload();
    }
    let proposed = introduced_here(&ours, &here, &this_name, &entry.name).map(|(authority, introduction)| {
        let name = introduction.name.clone();
        match super::changes::propose(ctx, &authority, Change::Add { introduction }) {
            Ok(id) => super::changes::Proposed { name, authority, id: Some(id), error: None },
            Err(e) => super::changes::Proposed { name, authority, id: None, error: Some(e.message) },
        }
    });
    let warning = (version != ctx.version)
        .then(|| format!("{} runs devsandbox {version}, this is {}", entry.name, ctx.version));
    to_value(Added { name: entry.name, roots: entry.roots, version, warning, fleet: plan.fleet, proposed })
}

/// The introduction of `name` this add signed here (by `me`, in `after`, not
/// in `before`), with the authority to propose it to (pure).
fn introduced_here(before: &Stored, after: &Stored, me: &str, name: &str) -> Option<(String, Introduction)> {
    let i = after.introductions.iter().find(|i| i.name == name && i.introducer == me && !before.introductions.contains(i))?;
    let authority = after.log.membership()?.authority.0.clone();
    Some((authority, i.clone()))
}

/// This host's member name and hub, from its `member.toml`.
pub(super) fn own_member(ctx: &Ctx) -> Result<(String, Hub), ApiError> {
    let Some(path) = &ctx.member else {
        return Err(ApiError::failed("this daemon has no member.toml (no data dir)"));
    };
    let own = member::load(path).map_err(ApiError::internal)?.unwrap_or_default();
    let name = own.name_or(&ctx.hostname).map_err(|e| ApiError::failed(format!("{e:#}")))?;
    Ok((name, own.hub()))
}

/// `name` is neither a registered machine's nor an inbound neighbor's.
/// `rename`: `--name` would get around it.
fn check_free(ctx: &Ctx, name: &str, rename: bool) -> Result<(), ApiError> {
    if registered(ctx)?.iter().any(|e| e.name == name) {
        return Err(taken(name, rename));
    }
    if ctx.inbound.contains(name) {
        return Err(inbound::taken(name));
    }
    Ok(())
}

/// A write of `add` failed after `written`: the error says what stays.
fn after(mut e: ApiError, written: &[String]) -> ApiError {
    if !written.is_empty() {
        e.message = format!("{} (already written: {})", e.message, written.join(", "));
    }
    e
}

/// The registry's machines, then the inbound neighbors up now, each with
/// its held link's state.
/// Each row's `introduced_by` from this host's `membership.json` (one that
/// doesn't load: none).
fn list(ctx: &Ctx) -> Answer {
    let stored = ctx.member.as_deref().and_then(|m| membership_api::load(m).ok().flatten());
    let rows: Vec<MachineRow> = machines(ctx)?
        .into_iter()
        .map(|e| {
            let live = ctx.links.as_ref().and_then(|l| l.remotes().get(&e.name)).unwrap_or_default();
            let (kind, target) = e.kind.wire();
            MachineRow {
                introduced_by: introducer_of(stored.as_ref(), &e.name),
                name: e.name,
                kind,
                target,
                roots: e.roots,
                command: e.command,
                agent: e.agent,
                capacity: live.capacity(),
                state: live.state,
                error: live.error,
                version: live.version,
            }
        })
        .collect();
    to_value(rows)
}

/// The registry's entries, then an inbound one per neighbor up now whose
/// name the registry hasn't (`Entry::inbound`).
pub(super) fn machines(ctx: &Ctx) -> Result<Vec<Entry>, ApiError> {
    let mut entries = registered(ctx)?;
    let inbound: Vec<Entry> = ctx
        .inbound
        .list()
        .into_iter()
        .filter(|n| !entries.iter().any(|e| e.name == n.name))
        .map(|n| Entry::inbound(n.name, n.roots))
        .collect();
    entries.extend(inbound);
    Ok(entries)
}

/// `machines.get`'s answer: the machine's registry entry, or an inbound
/// neighbor's (what the CLI routes a command on it with, `machines::cli`).
/// No argv on the wire: a client wanting an ssh command builds it from
/// `target` and `command`. An inbound machine has no `target`: no ssh to
/// it.
#[derive(Debug, Serialize, Deserialize)]
pub struct MachineInfo {
    pub name: String,
    pub kind: MachineKind,
    pub target: Option<String>,
    pub command: Option<String>,
    pub roots: Vec<PathBuf>,
    pub agent: bool,
}

#[derive(Deserialize)]
struct GetParams {
    name: String,
    #[serde(default)]
    agent: Option<PathBuf>,
}

/// One machine (registered or inbound; one seen only through this host's
/// hub is `denied`, naming the hub). `agent` is the caller's `SSH_AUTH_SOCK`
/// (absolute), reported to this daemon (`bridge::report_agent`), so the
/// agent this host answers the machine's `AGENT` streams with
/// (`bridge::host_agent`) is the caller's: what a CLI re-run over ssh on
/// the machine passes, for its terminal's agent.
fn get(p: GetParams, ctx: &Ctx) -> Answer {
    if let Some(agent) = &p.agent
        && !agent.is_absolute()
    {
        return Err(ApiError::invalid(format!("`agent` must be absolute, got `{}`", agent.display())));
    }
    // A machine seen through the hub is no entry: refused naming the hub.
    let entry = machines(ctx)?.into_iter().find(|e| e.name == p.name).ok_or_else(|| match &ctx.links {
        Some(links) => links.unknown(&p.name),
        None => ApiError::not_found(format!("no machine named `{}`", p.name)),
    })?;
    if let Some(agent) = p.agent {
        crate::devsbd::bridge::report_agent(agent);
    }
    let (kind, target) = entry.kind.wire();
    to_value(MachineInfo {
        name: entry.name,
        kind,
        target,
        command: entry.command,
        roots: entry.roots,
        agent: entry.agent,
    })
}

#[derive(Deserialize)]
struct SetParams {
    name: String,
    agent: bool,
}

/// Change a registered machine's settings (today only `agent`). The links
/// reload only when something changed (the link's `AGENT` streams follow
/// `agent`, `fleet::dialer`).
fn set(p: SetParams, ctx: &Ctx) -> Answer {
    let changed = super::update(&ctx.registry, |entries| {
        let entry = entries.iter_mut().find(|e| e.name == p.name)?;
        let changed = entry.agent != p.agent;
        entry.agent = p.agent;
        Some(changed)
    });
    let Some(changed) = changed.map_err(ApiError::internal)? else {
        if ctx.inbound.contains(&p.name) {
            return Err(inbound_agent(&p.name));
        }
        if let Some(links) = &ctx.links {
            return Err(links.unknown(&p.name));
        }
        return Err(ApiError::not_found(format!("no machine named `{}`", p.name)));
    };
    if changed && let Some(links) = &ctx.links {
        links.reload();
    }
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct NameParams {
    name: String,
}

/// Drop this host's link to `name`: its registry entry (it stays a fleet
/// member). Refused for an inbound neighbor: its link is dialed from there.
fn unlink(p: NameParams, ctx: &Ctx) -> Answer {
    let removed = super::update(&ctx.registry, |entries| {
        let before = entries.len();
        entries.retain(|e| e.name != p.name);
        entries.len() != before
    });
    if !removed.map_err(ApiError::internal)? {
        if ctx.inbound.contains(&p.name) {
            return Err(ApiError::denied(format!(
                "`{}` links to this host from its end (inbound): it goes with that link \
                 (`devsandbox machine unlink <this host>` run on `{}`)",
                p.name, p.name
            )));
        }
        return Err(ApiError::not_found(format!("no machine named `{}`", p.name)));
    }
    if let Some(links) = &ctx.links {
        links.reload();
    }
    Ok(json!({ "ok": true }))
}

fn registered(ctx: &Ctx) -> Result<Vec<Entry>, ApiError> {
    super::load(&ctx.registry).map_err(ApiError::internal)
}

/// `command` is shell text the machine runs, like the registry's hand-set
/// field, so the only check is that there's something to run.
fn check_command(command: Option<&str>) -> Result<(), ApiError> {
    match command {
        Some(c) if c.trim().is_empty() => Err(ApiError::invalid("empty `command`: omit it for `devsandbox`")),
        _ => Ok(()),
    }
}

/// `machines.set` on inbound machine `name`: this host's agent never
/// reaches it (fleet plan decision 30), so there's no `agent` to set here.
pub fn inbound_agent(name: &str) -> ApiError {
    ApiError::denied(format!(
        "`{name}` links to this host from its end (inbound): this host's agent never reaches it; \
         its agent reaches this host's instances while its entry for this host has `agent` on (set it there)"
    ))
}

fn taken(name: &str, rename: bool) -> ApiError {
    let hint = if rename { "; pass --name" } else { "" };
    ApiError::invalid(format!("a machine named `{name}` exists{hint}"))
}

/// Dial `entry` and say hello: `failed` with ssh's or the hello's reason.
fn connect(entry: &Entry, ctx: &Ctx) -> Result<Link, ApiError> {
    let duplex = (ctx.probe)(entry).map_err(|e| ApiError::failed(format!("{}: {e:#}", entry.name)))?;
    link::handshake(&entry.name, duplex, HANDSHAKE_TIMEOUT, link::Over::Ssh).map_err(|e| ApiError::failed(e.to_string()))
}

/// One call on `link`; any error answer, or an answer that doesn't parse,
/// is `failed`, named after `machine`.
fn remote_call<T: DeserializeOwned>(link: &mut Link, machine: &str, method: &str, params: Value) -> Result<T, ApiError> {
    let r = link.conn.call(method, params).map_err(|e| ApiError::failed(format!("{machine}: {e:#}")))?;
    if let Some(e) = r.error {
        return Err(ApiError::failed(format!("{machine}: {method}: {} ({})", e.message, e.code)));
    }
    serde_json::from_value(r.result.unwrap_or(Value::Null))
        .map_err(|e| ApiError::failed(format!("{machine}: {method}: unexpected answer: {e}")))
}

fn to_value(x: impl Serialize) -> Answer {
    serde_json::to_value(x).map_err(|e| ApiError::internal(e.into()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::serve::client::{self, Conn, Hello};
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::daemon::{Exit, Options};
    use crate::serve::endpoint;
    use crate::serve::fleet::identity::SigningKey;
    use crate::serve::fleet::member::Member;
    use crate::serve::fleet::membership_api::test_support::{self, fleet};
    use crate::serve::machines::ssh;
    use crate::serve::proto::Version;

    /// Connect to the test daemon in `dir` as a client, retrying while it binds.
    fn client_at(dir: &Path) -> Conn {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(mut stream) = endpoint::connect(dir) {
                // Older than any daemon: never a handoff, whatever its semver.
                let version = Version { semver: "0.0.0".into(), build: 0 };
                match client::hello(&mut stream, &version, "test").unwrap() {
                    Hello::Ready(reply) => return Conn::new(stream, reply).unwrap(),
                    other => panic!("{other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A machine: a test daemon on `base/remote` with its own state, home
    /// (`base/remote-home`), member file (in `base/remote-data`, so its key
    /// and membership log are its own) and registry, reporting `semver`.
    fn remote_opts(base: &Path, semver: &str) -> Options {
        let mut o = opts(1, 1500);
        o.version.semver = semver.into();
        o.state = Some(base.join("remote-state.toml"));
        o.home = Some(base.join("remote-home"));
        o.member = Some(remote_member(base));
        o.machines = Some(base.join("remote-machines.toml"));
        o
    }

    fn remote_member(base: &Path) -> PathBuf {
        base.join("remote-data").join("member.toml")
    }

    /// This host's test daemon on `base/local`, its registry and member
    /// file in `base`, dialing the machine's socket in `remote`.
    fn local_opts(base: &Path, remote: Option<PathBuf>) -> Options {
        let mut o = opts(1, 1500);
        o.machines = Some(base.join("machines.toml"));
        o.member = Some(base.join("member.toml"));
        o.probe = Some(match remote {
            Some(remote) => Arc::new(move |_: &Entry| ssh::test_support::relayed(endpoint::connect(&remote)?)),
            None => Arc::new(|e: &Entry| anyhow::bail!("ssh: Could not resolve hostname {}", e.label())),
        });
        o
    }

    fn ok(conn: &mut Conn, method: &str, params: Value) -> Value {
        let r = conn.call(method, params).unwrap();
        assert!(r.error.is_none(), "{method}: {:?}", r.error);
        r.result.unwrap()
    }

    fn err(conn: &mut Conn, method: &str, params: Value) -> (String, String) {
        let e = conn.call(method, params).unwrap().error.unwrap_or_else(|| panic!("{method} succeeded"));
        (e.code, e.message)
    }

    /// A member file naming its host `name`, in hub `hub` (`""`: none).
    fn named(name: &str, hub: &str) -> Member {
        Member { name: name.into(), hub: hub.into(), ..Default::default() }
    }

    /// `k`, a third member's key (the authority of the fleets the pair
    /// joins as members).
    fn k_key() -> SigningKey {
        SigningKey::from_seed([7; 32])
    }

    /// `k`'s fleet with member `m`, which introduces `name` (`key`): a
    /// capped member there.
    fn capped(name: &str, key: PublicKey) -> Stored {
        let m = SigningKey::from_seed([8; 32]);
        let s = fleet("k", &k_key(), "k", &[("m", m.public_key())]);
        membership_api::sign_into(&s, &membership_api::Own { name: "m", key: m.public_key(), next: None }, &m, name, &key).unwrap()
    }

    /// What a test fleet's setup writes: this host's and the machine's
    /// `member.toml` paths, to sign and store their logs with.
    type Setup = Box<dyn FnOnce(&Path, &Path)>;

    /// Two daemons: the machine (with `~/.devsandboxes` holding `web` and
    /// `api`, and `~/broken` whose config doesn't parse) and this host's.
    /// Both hosts have a hostname of their own only through `member.toml`
    /// (they share the test's): `lap` and `box` unless [`Pair::members`].
    struct Pair {
        base: PathBuf,
        home: PathBuf,
        local: Conn,
        daemons: Vec<std::thread::JoinHandle<anyhow::Result<Exit>>>,
    }

    impl Pair {
        fn new(tag: &str, remote_semver: &str) -> Self {
            Self::with(tag, remote_semver, |_| {})
        }

        /// As [`Pair::new`], `local` adjusting this host's daemon's options.
        fn with(tag: &str, remote_semver: &str, local: impl FnOnce(&mut Options)) -> Self {
            Self::members(tag, remote_semver, named("lap", ""), named("box", ""), local)
        }

        /// As [`Pair::with`], this host's and the machine's `member.toml`
        /// written first.
        fn members(tag: &str, remote_semver: &str, this: Member, machine: Member, local: impl FnOnce(&mut Options)) -> Self {
            Self::build(tag, remote_semver, this, machine, Box::new(|_, _| {}), local)
        }

        /// As [`Pair::members`], `setup` then writing their membership logs.
        fn fleets(tag: &str, this: Member, machine: Member, setup: impl FnOnce(&Path, &Path) + 'static) -> Self {
            Self::build(tag, "0.6.0", this, machine, Box::new(setup), |_| {})
        }

        fn build(tag: &str, remote_semver: &str, this: Member, machine: Member, setup: Setup, local: impl FnOnce(&mut Options)) -> Self {
            let base = scratch(tag);
            std::fs::create_dir_all(&base).unwrap();
            member::write_for_test(&base.join("member.toml"), &this);
            member::write_for_test(&remote_member(&base), &machine);
            setup(&base.join("member.toml"), &remote_member(&base));
            let home = base.join("remote-home");
            std::fs::create_dir_all(home.join(".devsandboxes")).unwrap();
            std::fs::create_dir_all(home.join("broken")).unwrap();
            let cfg = crate::config::CONFIG_FILE;
            std::fs::write(home.join(".devsandboxes").join(cfg), "[sandbox.web]\nimage = \"a\"\n[sandbox.api]\nimage = \"a\"\n").unwrap();
            std::fs::write(home.join("broken").join(cfg), "[sandbox.web\n").unwrap();
            let remote = base.join("remote");
            let mut local_o = local_opts(&base, Some(remote.clone()));
            local(&mut local_o);
            let daemons =
                vec![spawn_daemon(&remote, remote_opts(&base, remote_semver)), spawn_daemon(&base.join("local"), local_o)];
            // The dial connects straight away: the machine must be up.
            drop(client_at(&base.join("remote")));
            let local = client_at(&base.join("local"));
            Pair { base, home, local, daemons }
        }

        /// This host's `member.toml`.
        fn this(&self) -> Member {
            member::load(&self.base.join("member.toml")).unwrap().unwrap()
        }

        /// The machine's `member.toml`.
        fn machine(&self) -> Member {
            member::load(&remote_member(&self.base)).unwrap().unwrap()
        }

        /// This host's and the machine's membership logs.
        fn logs(&self) -> (Option<Stored>, Option<Stored>) {
            (test_support::read(&self.base.join("member.toml")), test_support::read(&remote_member(&self.base)))
        }

        /// This host's key.
        fn this_key(&self) -> SigningKey {
            test_support::key_of(&self.base.join("member.toml"))
        }

        /// The machine's key.
        fn machine_key(&self) -> SigningKey {
            test_support::key_of(&remote_member(&self.base))
        }

        /// `machines.add`'s `params` with the machine's key, as confirmed.
        fn keyed(&self, mut params: Value) -> Value {
            params["key"] = json!(self.machine_key().public_key());
            params
        }

        fn add(&mut self, params: Value) -> Value {
            let params = self.keyed(params);
            ok(&mut self.local, "machines.add", params)
        }

        fn add_err(&mut self, params: Value) -> (String, String) {
            let params = self.keyed(params);
            err(&mut self.local, "machines.add", params)
        }

        fn registry(&self) -> Vec<Entry> {
            crate::serve::machines::load(&self.base.join("machines.toml")).unwrap()
        }

        /// Both shut down: a daemon with a machine registered holds its
        /// link and never idles out (`idle::Holders::links`).
        fn finish(self) {
            drop(self.local);
            for dir in [self.base.join("local"), self.base.join("remote")] {
                assert!(client_at(&dir).call("shutdown", json!({})).unwrap().error.is_none());
            }
            for d in self.daemons {
                assert_eq!(d.join().unwrap().unwrap(), Exit::Shutdown);
            }
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn probe_answers_the_machines_roots() {
        let mut pair = Pair::new("mach-probe", "0.6.0");
        let r = ok(&mut pair.local, "machines.probe", json!({"target": "me@box"}));
        let dot = std::fs::canonicalize(pair.home.join(".devsandboxes")).unwrap();
        let key = pair.machine_key().public_key();
        assert_eq!(
            r,
            json!({
                "version": "0.6.0", "protocol": 1, "roots": [{"root": dot, "sandboxes": [{"name": "api"}, {"name": "web"}], "error": null}],
                "key": key, "fingerprint": key.fingerprint(),
            })
        );
        assert_eq!(err(&mut pair.local, "machines.probe", json!({"target": "-oProxyCommand=x"})).0, "invalid");
        assert_eq!(err(&mut pair.local, "machines.probe", json!({})).0, "invalid");
        for empty in ["", "  "] {
            let (code, message) = err(&mut pair.local, "machines.probe", json!({"target": "me@box", "command": empty}));
            assert!(code == "invalid" && message.contains("`command`"), "{code}: {message}");
        }
        pair.finish();
    }

    #[test]
    fn a_failing_dial_is_failed_with_its_reason() {
        let base = scratch("mach-nodial");
        let daemon = spawn_daemon(&base.join("local"), local_opts(&base, None));
        let mut local = client_at(&base.join("local"));
        let (code, message) = err(&mut local, "machines.probe", json!({"target": "me@box"}));
        assert_eq!((code.as_str(), message.as_str()), ("failed", "me@box: ssh: Could not resolve hostname me@box"));
        let key = SigningKey::from_seed([2; 32]).public_key();
        let (code, _) = err(&mut local, "machines.add", json!({"hub": false, "target": "me@box", "roots": ["/x"], "key": key}));
        assert_eq!(code, "failed");
        let (code, _) = err(&mut local, "machines.add", json!({"hub": false, "target": "me@box", "roots": ["/x"]}));
        assert_eq!(code, "invalid", "`key` is mandatory");
        assert!(!base.join("machines.toml").exists(), "nothing saved");
        drop(local);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn add_checks_the_roots_on_the_machine_then_saves() {
        let mut pair = Pair::new("mach-add", "0.6.0");
        let registry = pair.base.join("machines.toml");
        // A typed bad root is refused, naming it; nothing is saved.
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": ["~/.devsandboxes", "~/gone", "rel"]}));
        assert_eq!(code, "invalid");
        assert!(message.starts_with("box: `~/gone`: no directory ") && message.ends_with("; `rel`: not absolute"), "{message}");
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": ["~/broken"]}));
        assert!(code == "invalid" && message.contains("devsandboxes.toml"), "{code}: {message}");
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": ["~"], "command": " "}));
        assert!(code == "invalid" && message.contains("`command`"), "{code}: {message}");
        assert!(!registry.exists(), "nothing saved");

        assert_eq!((pair.this(), pair.machine()), (named("lap", ""), named("box", "")), "nothing written");
        assert_eq!(pair.logs(), (None, None), "no log");

        // The default name is the machine's member name; roots are its
        // canonical paths, deduplicated in order. Neither in a fleet: a
        // fleet of two, this host the hub.
        let dot = std::fs::canonicalize(pair.home.join(".devsandboxes")).unwrap();
        let home = std::fs::canonicalize(&pair.home).unwrap();
        let via = format!("{}/broken/../.devsandboxes", pair.home.display());
        let r = pair.add(json!({"hub": false, "target": "me@box", "roots": ["~/.devsandboxes", "~", via]}));
        let fleet = json!({"hub": "lap", "joined": "new"});
        assert_eq!(r, json!({"name": "box", "roots": [dot, home], "version": "0.6.0", "warning": null, "fleet": fleet, "proposed": null}));
        assert_eq!(pair.registry(), vec![Entry { roots: vec![dot.clone(), home.clone()], ..Entry::ssh("box", "me@box") }]);
        assert_eq!((pair.this(), pair.machine()), (named("lap", "self"), named("box", "lap")));
        // One log on both ends, signed here: `lap` the authority and the
        // hub, `box` added by it.
        let (Some(here), Some(there)) = pair.logs() else { panic!("no logs") };
        assert_eq!(here, there);
        let m = here.log.membership().unwrap();
        assert_eq!((&m.authority, m.hub.as_str(), m.head.len), (&("lap".to_string(), pair.this_key().public_key()), "lap", 2));
        assert_eq!(m.key_of("box"), Some(pair.machine_key().public_key()));
        assert!(here.introductions.is_empty());

        // Taken: the machine is `box` in this fleet, whatever the target
        // or `--name` says.
        let (code, message) = pair.add_err(json!({"hub": false, "target": "ssh://box:2200", "roots": ["~"]}));
        assert_eq!((code.as_str(), message.as_str()), ("invalid", "a machine named `box` exists"));
        let (code, message) = pair.add_err(json!({"hub": false, "target": "ssh://box:2200", "name": "box2", "roots": ["~"]}));
        assert_eq!(code, "invalid");
        assert_eq!(message, "ssh://box:2200 is in `lap`'s fleet as `box`: pass --name box, or no --name");
        let (code, message) = pair.add_err(json!({"hub": false, "target": "x", "name": "box", "roots": ["~"]}));
        assert_eq!((code.as_str(), message.as_str()), ("invalid", "a machine named `box` exists; pass --name"), "fast, before the ssh");
        let (code, _) = pair.add_err(json!({"hub": false, "target": "x", "name": "a/b", "roots": ["~"]}));
        assert_eq!(code, "invalid");

        // Unregistered (the fleet stays), added again: an extra link in the
        // same fleet, the logs exchanged (the same), nothing written but
        // the entry.
        assert_eq!(ok(&mut pair.local, "machines.unlink", json!({"name": "box"})), json!({"ok": true}));
        let cmd = "~/.local/bin/devsandbox";
        let r = pair.add(json!({"hub": false, "target": "ssh://box:2200", "name": "box", "roots": ["~/.devsandboxes"], "agent": true, "command": cmd}));
        assert_eq!((&r["name"], &r["fleet"]), (&json!("box"), &json!({"hub": "lap", "joined": "same"})));
        assert_eq!((pair.this(), pair.machine()), (named("lap", "self"), named("box", "lap")));
        assert_eq!(pair.logs(), (Some(here.clone()), Some(here)));
        let saved = &pair.registry()[0];
        assert!(saved.agent, "`agent` saved");
        assert_eq!(saved.command.as_deref(), Some(cmd), "`command` saved");
        assert!(std::fs::read_to_string(&registry).unwrap().contains("command = \"~/.local/bin/devsandbox\""));

        // Listed, its link held at once (a test daemon's transport fails:
        // `state` and `error` are its); then removed.
        let mut rows = ok(&mut pair.local, "machines.list", Value::Null);
        for row in rows.as_array_mut().unwrap() {
            let row = row.as_object_mut().unwrap();
            assert_ne!(row.remove("state").unwrap(), "connected");
            row.remove("error");
        }
        assert_eq!(
            rows,
            json!([
                {"name": "box", "kind": "ssh", "target": "ssh://box:2200", "roots": [dot], "command": cmd, "agent": true, "version": null, "capacity": null, "introduced_by": null},
            ])
        );
        let info = ok(&mut pair.local, "machines.get", json!({"name": "box"}));
        assert_eq!(info, json!({"name": "box", "kind": "ssh", "target": "ssh://box:2200", "command": cmd, "roots": [dot], "agent": true}));
        assert_eq!(err(&mut pair.local, "machines.get", json!({"name": "nope"})).0, "not-found");
        assert_eq!(ok(&mut pair.local, "machines.unlink", json!({"name": "box"})), json!({"ok": true}));
        let (code, _) = err(&mut pair.local, "machines.unlink", json!({"name": "box"}));
        assert_eq!(code, "not-found");
        assert_eq!(ok(&mut pair.local, "machines.list", Value::Null).as_array().unwrap().len(), 0);
        pair.finish();
    }

    #[test]
    fn add_takes_no_roots() {
        let mut pair = Pair::new("mach-add-noroots", "0.6.0");
        let r = pair.add(json!({"hub": false, "target": "me@box", "roots": []}));
        assert_eq!((&r["name"], &r["roots"]), (&json!("box"), &json!([])));
        assert_eq!(pair.registry(), vec![Entry::ssh("box", "me@box")]);
        pair.finish();
    }

    #[test]
    fn add_hub_makes_the_machine_the_hub() {
        let mut pair = Pair::new("mach-add-hub", "0.6.0");
        // `hub` is mandatory.
        let (code, _) = pair.add_err(json!({"target": "me@box", "roots": []}));
        assert_eq!(code, "invalid");
        assert!(!pair.base.join("machines.toml").exists(), "nothing saved");
        let r = pair.add(json!({"hub": true, "target": "me@box", "roots": []}));
        assert_eq!((&r["name"], &r["fleet"]), (&json!("box"), &json!({"hub": "box", "joined": "new"})));
        assert_eq!((pair.this(), pair.machine()), (named("lap", "box"), named("box", "self")));
        assert_eq!(ok(&mut pair.local, "member.get", Value::Null)["hub"], "box");
        // Decision 48: the laptop that made the fleet is its authority, the
        // machine it added the hub.
        let (Some(here), Some(there)) = pair.logs() else { panic!("no logs") };
        assert_eq!(here, there);
        let m = here.log.membership().unwrap();
        assert_eq!((m.authority.0.as_str(), m.hub.as_str(), m.members.len()), ("lap", "box", 2));
        pair.finish();
    }

    #[test]
    fn add_hub_in_a_fleet_is_refused_unless_the_machine_is_its_hub() {
        let (this, machine) = (named("lap", "self"), named("box", ""));
        let mut pair = Pair::members("mach-add-hub-fleet", "0.6.0", this.clone(), machine.clone(), |_| {});
        let (code, message) = pair.add_err(json!({"hub": true, "target": "me@box", "roots": []}));
        assert_eq!(code, "denied");
        assert!(message.starts_with("--hub: this host is in `lap`'s fleet and me@box isn't its hub"), "{message}");
        assert_eq!((pair.this(), pair.machine()), (this, machine), "nothing written");
        assert!(!pair.base.join("machines.toml").exists(), "no entry");
        pair.finish();
        // The machine is the hub already: this host joins, as without it.
        let mut pair = Pair::fleets("mach-add-hub-is", named("lap", ""), named("box", "self"), |_, machine| {
            test_support::write(machine, &fleet("box", &test_support::key_of(machine), "box", &[]));
        });
        let r = pair.add(json!({"hub": true, "target": "me@box", "roots": []}));
        assert_eq!(r["fleet"], json!({"hub": "box", "joined": "this"}));
        assert_eq!((pair.this(), pair.machine()), (named("lap", "box"), named("box", "self")));
        pair.finish();
    }

    /// A member of `k`'s fleet (not its authority) adds a machine: it
    /// introduces it (signed here), and the machine takes the log and the
    /// introduction, its hub `k` from the log. The introduction isn't
    /// proposed to `k`, which this host has no link to: the answer says why.
    #[test]
    fn the_machine_joins_this_hosts_fleet_under_its_name() {
        let mut pair = Pair::fleets("mach-join-target", named("lap", "k"), named("box", ""), |this, _| {
            test_support::write(this, &fleet("k", &k_key(), "k", &[("lap", test_support::key_of(this).public_key())]));
        });
        let r = pair.add(json!({"hub": false, "target": "me@box", "name": "b2", "roots": ["~/.devsandboxes"]}));
        assert_eq!((&r["name"], &r["fleet"]), (&json!("b2"), &json!({"hub": "k", "joined": "target"})));
        assert_eq!(pair.machine(), named("b2", "k"), "renamed as it joined; its hub from the log");
        assert_eq!(pair.this(), named("lap", "k"), "this host unchanged");
        assert_eq!(pair.registry()[0].name, "b2");
        let (Some(here), Some(there)) = pair.logs() else { panic!("no logs") };
        assert_eq!(here, there);
        assert_eq!(here.log.head().len, 2, "the log as it was: only `k` signs it");
        let i = &here.introductions[0];
        assert_eq!((i.name.as_str(), i.key, i.introducer.as_str()), ("b2", pair.machine_key().public_key(), "lap"));
        let proposed = &r["proposed"];
        assert_eq!((&proposed["name"], &proposed["authority"], &proposed["id"]), (&json!("b2"), &json!("k"), &Value::Null));
        assert!(proposed["error"].as_str().unwrap().contains("isn't directly linked"), "{proposed}");
        pair.finish();
    }

    /// The authority adds a machine: an `add` entry.
    #[test]
    fn the_authority_adds_the_machine() {
        let mut pair = Pair::fleets("mach-join-auth", named("lap", "self"), named("box", ""), |this, _| {
            test_support::write(this, &fleet("lap", &test_support::key_of(this), "lap", &[]));
        });
        let r = pair.add(json!({"hub": false, "target": "me@box", "roots": []}));
        assert_eq!(r["fleet"], json!({"hub": "lap", "joined": "target"}));
        assert_eq!(r["proposed"], Value::Null, "signed here: nothing to propose");
        assert_eq!((pair.this(), pair.machine()), (named("lap", "self"), named("box", "lap")));
        let (Some(here), Some(there)) = pair.logs() else { panic!("no logs") };
        assert_eq!(here, there);
        assert_eq!(here.log.membership().unwrap().key_of("box"), Some(pair.machine_key().public_key()));
        pair.finish();
    }

    /// A capped member (only introduced) adds no one: refused before
    /// anything is written.
    #[test]
    fn a_capped_member_adds_no_one() {
        let mut pair = Pair::fleets("mach-join-capped", named("lap", "k"), named("box", ""), |this, _| {
            test_support::write(this, &capped("lap", test_support::key_of(this).public_key()));
        });
        let before = pair.logs();
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": []}));
        assert!(code == "denied" && message.contains("capped"), "{code}: {message}");
        assert_eq!(pair.logs(), before, "nothing written");
        assert_eq!(pair.machine(), named("box", ""));
        assert!(!pair.base.join("machines.toml").exists());
        pair.finish();
    }

    /// The machine is the hub and authority: it signs this host's `add`
    /// over the probe; this host takes the log, its hub from it.
    #[test]
    fn this_host_joins_the_machines_fleet() {
        let mut pair = Pair::fleets("mach-join-this", named("lap", ""), named("box", "self"), |_, machine| {
            test_support::write(machine, &fleet("box", &test_support::key_of(machine), "box", &[]));
        });
        let r = pair.add(json!({"hub": false, "target": "me@box", "roots": ["~/.devsandboxes"]}));
        assert_eq!((&r["name"], &r["fleet"]), (&json!("box"), &json!({"hub": "box", "joined": "this"})));
        assert_eq!((pair.this(), pair.machine()), (named("lap", "box"), named("box", "self")));
        // Through the daemon's own path: `member.get` says it.
        assert_eq!(ok(&mut pair.local, "member.get", Value::Null)["hub"], "box");
        let (Some(here), Some(there)) = pair.logs() else { panic!("no logs") };
        assert_eq!(here, there);
        let m = here.log.membership().unwrap();
        assert_eq!((m.authority.0.as_str(), m.key_of("lap")), ("box", Some(pair.this_key().public_key())));
        pair.finish();
    }

    /// The machine is a member of `k`'s fleet: it introduces this host
    /// over the probe. A capped machine can't: refused, nothing written.
    #[test]
    fn this_host_is_introduced_by_the_machine() {
        let mut pair = Pair::fleets("mach-join-intro", named("lap", ""), named("box", "k"), |_, machine| {
            test_support::write(machine, &fleet("k", &k_key(), "k", &[("box", test_support::key_of(machine).public_key())]));
        });
        let r = pair.add(json!({"hub": false, "target": "me@box", "roots": []}));
        assert_eq!(r["fleet"], json!({"hub": "k", "joined": "this"}));
        assert_eq!((pair.this(), pair.machine()), (named("lap", "k"), named("box", "k")));
        let (Some(here), Some(there)) = pair.logs() else { panic!("no logs") };
        assert_eq!(here, there);
        assert_eq!((here.introductions[0].name.as_str(), here.introductions[0].introducer.as_str()), ("lap", "box"));
        pair.finish();

        let mut pair = Pair::fleets("mach-join-intro-capped", named("lap", ""), named("box", "k"), |_, machine| {
            test_support::write(machine, &capped("box", test_support::key_of(machine).public_key()));
        });
        let before = pair.logs();
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": []}));
        assert!(code == "denied" && message.starts_with("box: ") && message.contains("capped"), "{code}: {message}");
        assert_eq!(pair.logs(), before, "nothing written");
        assert_eq!(pair.this(), named("lap", ""));
        pair.finish();
    }

    /// The key the user confirmed isn't the machine's: refused, naming both
    /// fingerprints, nothing written.
    #[test]
    fn another_key_is_refused() {
        let mut pair = Pair::new("mach-add-key", "0.6.0");
        let wrong = SigningKey::from_seed([9; 32]).public_key();
        let params = json!({"hub": false, "target": "me@box", "roots": [], "key": wrong});
        let (code, message) = err(&mut pair.local, "machines.add", params);
        assert_eq!(code, "denied");
        let theirs = pair.machine_key().public_key().fingerprint();
        assert!(message.contains(&theirs) && message.contains(&wrong.fingerprint()), "{message}");
        assert_eq!((pair.this(), pair.machine()), (named("lap", ""), named("box", "")));
        assert_eq!(pair.logs(), (None, None));
        assert!(!pair.base.join("machines.toml").exists());
        pair.finish();
    }

    /// An extra link whose ends hold another fleet's log, or a log that
    /// doesn't list the other end: refused before anything is written.
    #[test]
    fn an_extra_link_needs_one_fleet_both_known() {
        // The same hub by name, another fleet by its log (made again).
        let mut pair = Pair::fleets("mach-same-fork", named("lap", "self"), named("box", "lap"), |this, machine| {
            let (lap, box_key) = (test_support::key_of(this), test_support::key_of(machine).public_key());
            test_support::write(this, &fleet("lap", &lap, "lap", &[("box", box_key)]));
            test_support::write(machine, &fleet("lap", &lap, "lap", &[("box", box_key)]));
        });
        let before = pair.logs();
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": []}));
        assert!(code == "denied" && message.contains("another fleet"), "{code}: {message}");
        assert_eq!(pair.logs(), before);
        assert!(!pair.base.join("machines.toml").exists());
        pair.finish();

        // This host's log lacks the machine, which holds an older copy
        // without it: not each other's.
        let mut pair = Pair::fleets("mach-same-unknown", named("lap", "self"), named("box", "lap"), |this, machine| {
            let s = fleet("lap", &test_support::key_of(this), "lap", &[]);
            test_support::write(this, &s);
            test_support::write(machine, &s);
        });
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": []}));
        assert!(code == "denied" && message.contains("doesn't list this host"), "{code}: {message}");
        assert!(!pair.base.join("machines.toml").exists());
        // Asked directly, the machine refuses that log too.
        let s = pair.logs().0.unwrap();
        let mut machine = client_at(&pair.base.join("remote"));
        let (code, message) = err(&mut machine, "membership.adopt", serde_json::to_value(s.to_wire()).unwrap());
        assert!(code == "denied" && message.contains("doesn't list this host"), "{code}: {message}");
        drop(machine);
        pair.finish();

        // A fleet made before member keys: no logs.
        let mut pair = Pair::members("mach-same-nolog", "0.6.0", named("lap", "self"), named("box", "lap"), |_| {});
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": []}));
        assert!(code == "failed" && message.contains("no membership log"), "{code}: {message}");
        pair.finish();
    }

    /// An extra link exchanges logs: the end behind takes the other's
    /// later entries.
    #[test]
    fn an_extra_link_brings_the_end_behind_up() {
        let mut pair = Pair::fleets("mach-same-behind", named("lap", "self"), named("box", "lap"), |this, machine| {
            let lap = test_support::key_of(this);
            let s = fleet("lap", &lap, "lap", &[("box", test_support::key_of(machine).public_key())]);
            let mut longer = s.clone();
            longer.log.append(crate::serve::fleet::membership::Op::Add { name: "pi".into(), key: k_key().public_key() }, "lap", &lap).unwrap();
            test_support::write(this, &longer);
            test_support::write(machine, &s);
        });
        let r = pair.add(json!({"hub": false, "target": "me@box", "roots": []}));
        assert_eq!(r["fleet"], json!({"hub": "lap", "joined": "same"}));
        let (Some(here), Some(there)) = pair.logs() else { panic!("no logs") };
        assert_eq!((here.log.head().len, there), (3, here.clone()));
        pair.finish();
    }

    #[test]
    fn two_hubs_are_refused_and_nothing_written() {
        let (this, machine) = (named("lap", "self"), named("box", "k"));
        let mut pair = Pair::members("mach-two-hubs", "0.6.0", this.clone(), machine.clone(), |_| {});
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": ["~/.devsandboxes"]}));
        assert_eq!(code, "denied");
        assert_eq!(message, "this host is in `lap`'s fleet, me@box in `k`'s: a machine is in one fleet only");
        assert_eq!((pair.this(), pair.machine()), (this, machine));
        assert!(!pair.base.join("machines.toml").exists(), "no entry");
        pair.finish();
    }

    #[test]
    fn a_name_already_in_the_fleet_is_refused() {
        // The machine's name is this host's, or its hub's.
        for (tag, this, machine, says) in [
            ("mach-name-this", named("lap", "self"), named("lap", ""), "me@box's member name `lap` is this host's too: pass --name"),
            ("mach-name-hub", named("lap", "k"), named("k", ""), "`k` (me@box) is already a member of `k`'s fleet: pass --name"),
        ] {
            let mut pair = Pair::members(tag, "0.6.0", this.clone(), machine.clone(), |_| {});
            let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": ["~/.devsandboxes"]}));
            assert_eq!((code.as_str(), message.as_str()), ("invalid", says));
            assert_eq!((pair.this(), pair.machine()), (this, machine), "nothing written");
            assert!(!pair.base.join("machines.toml").exists());
            pair.finish();
        }
        // This host joining: its name is one the machine's fleet knows.
        let mut pair = Pair::members("mach-name-known", "0.6.0", named("lap", ""), named("box", "self"), |_| {});
        let theirs = Entry { roots: vec!["/r".into()], ..Entry::ssh("lap", "me@other") };
        crate::serve::machines::update(&pair.base.join("remote-machines.toml"), |e| e.push(theirs)).unwrap();
        let (code, message) = pair.add_err(json!({"hub": false, "target": "me@box", "roots": ["~/.devsandboxes"]}));
        assert_eq!(code, "invalid");
        assert!(message.starts_with("this host's member name `lap` is already in `box`'s fleet"), "{message}");
        assert_eq!(pair.this(), named("lap", ""), "nothing written");
        pair.finish();
    }

    #[test]
    fn probe_and_add_dial_with_the_command() {
        let dialed: Arc<std::sync::Mutex<Vec<Option<String>>>> = Arc::default();
        let log = dialed.clone();
        let mut pair = Pair::with("mach-cmd", "0.6.0", move |o| {
            let inner = o.probe.take().unwrap();
            o.probe = Some(Arc::new(move |e: &Entry| {
                log.lock().unwrap().push(e.command.clone());
                inner(e)
            }));
        });
        let cmd = "~/.local/bin/devsandbox";
        ok(&mut pair.local, "machines.probe", json!({"target": "me@box", "command": cmd}));
        ok(&mut pair.local, "machines.probe", json!({"target": "me@box"}));
        pair.add(json!({"hub": false, "target": "me@box", "roots": ["~/.devsandboxes"], "command": cmd}));
        assert_eq!(*dialed.lock().unwrap(), [Some(cmd.to_string()), None, Some(cmd.to_string())]);
        pair.finish();
    }

    #[test]
    fn set_flips_agent_and_saves_it() {
        let base = scratch("mach-set");
        let registry = base.join("machines.toml");
        let entry = Entry { roots: vec!["/r".into()], ..Entry::ssh("box", "me@box") };
        crate::serve::machines::update(&registry, |e| e.push(entry.clone())).unwrap();
        let daemon = spawn_daemon(&base.join("local"), local_opts(&base, None));
        let mut local = client_at(&base.join("local"));
        let agent = || crate::serve::machines::load(&registry).unwrap()[0].agent;
        assert_eq!(ok(&mut local, "machines.set", json!({"name": "box", "agent": true})), json!({"ok": true}));
        assert!(agent());
        assert!(std::fs::read_to_string(&registry).unwrap().contains("agent = true"));
        assert_eq!(ok(&mut local, "machines.get", json!({"name": "box"}))["agent"], true);
        // Again is a no-op, not an error.
        assert_eq!(ok(&mut local, "machines.set", json!({"name": "box", "agent": true})), json!({"ok": true}));
        assert_eq!(ok(&mut local, "machines.set", json!({"name": "box", "agent": false})), json!({"ok": true}));
        assert!(!agent());
        assert!(!std::fs::read_to_string(&registry).unwrap().contains("agent"));
        let (code, message) = err(&mut local, "machines.set", json!({"name": "nope", "agent": true}));
        assert_eq!((code.as_str(), message.as_str()), ("not-found", "no machine named `nope`"));
        assert_eq!(err(&mut local, "machines.set", json!({"name": "box"})).0, "invalid");
        assert_eq!(crate::serve::machines::load(&registry).unwrap(), vec![entry]);
        // A registered machine's link holds the daemon up.
        assert!(local.call("shutdown", json!({})).unwrap().error.is_none());
        drop(local);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn another_semver_is_a_warning() {
        let mut pair = Pair::new("mach-skew", "0.5.0");
        let r = pair.add(json!({"hub": false, "target": "box", "roots": ["~/.devsandboxes"]}));
        assert_eq!(r["warning"], "box runs devsandbox 0.5.0, this is 0.6.0");
        pair.finish();
    }

    #[test]
    fn npm_typings_match() {
        use crate::serve::dts::{assert_matches, prop_type, union_members};
        let root = || RootCandidate { root: "/cfg".into(), sandboxes: vec![], error: None, fleet: false };
        let key = SigningKey::from_seed([1; 32]).public_key();
        let probed = Probed { version: "0.6.0".into(), protocol: 1, roots: vec![root()], key, fingerprint: key.fingerprint() };
        assert_matches("MachineProbe", &to_value(probed).unwrap());
        let fleet = Fleet { hub: "k".into(), joined: join::Joined::Target };
        let proposed = super::super::changes::Proposed { name: "m".into(), authority: "lap".into(), id: Some(1), error: None };
        let added = Added { name: "m".into(), roots: vec!["/cfg".into()], version: "0.6.0".into(), warning: None, fleet, proposed: Some(proposed) };
        let added = to_value(added).unwrap();
        assert_matches("MachineAdded", &added);
        assert_matches("FleetProposed", &added["proposed"]);
        assert_matches("MachineFleet", &added["fleet"]);
        assert_eq!(union_members("FleetJoined"), ["'target'", "'this'", "'new'", "'same'"]);
        for joined in [join::Joined::Target, join::Joined::This, join::Joined::New, join::Joined::Same] {
            let wire = to_value(joined).unwrap();
            assert!(union_members("FleetJoined").contains(&format!("'{}'", wire.as_str().unwrap())));
        }
        let row = MachineRow {
            name: "m".into(),
            kind: MachineKind::Inbound,
            target: None,
            roots: vec![],
            command: None,
            agent: false,
            state: LinkState::Idle,
            error: None,
            version: None,
            capacity: Some(Capacity::default()),
            introduced_by: Some("lap".into()),
        };
        let row = to_value(row).unwrap();
        assert_matches("MachineRow", &row);
        assert_matches("Capacity", &row["capacity"]);
        let info = MachineInfo { name: "m".into(), kind: MachineKind::Ssh, target: Some("m".into()), command: None, roots: vec![], agent: false };
        assert_matches("MachineInfo", &to_value(info).unwrap());
        assert_eq!(union_members("MachineKind"), ["'ssh'", "'inbound'"]);
        use super::super::leave::{Heard, Left, Missed, Removed, RmAnswer};
        use crate::serve::dts::assert_variant;
        let missed = Missed { name: "x".into(), error: "no link to it".into() };
        assert_matches("FleetMissed", &to_value(missed.clone()).unwrap());
        let removed = Removed {
            name: "m".into(),
            hub: "k".into(),
            leave: Heard::Unreachable,
            error: Some("down".into()),
            forgot: vec!["y".into()],
            missed: vec![missed],
        };
        assert_matches("FleetRemoved", &to_value(removed.clone()).unwrap());
        use super::super::changes::{Approved, Proposed, Signed};
        let signed = Signed {
            name: "m".into(),
            change: "revoke `m`".into(),
            pushed: vec!["k".into()],
            unpushed: vec![Missed { name: "w".into(), error: "down".into() }],
            removed: Some(removed.clone()),
            error: None,
        };
        assert_variant("MachineRmRevoked", &to_value(RmAnswer::Revoked(signed.clone())).unwrap());
        assert_matches("FleetSigned", &to_value(signed.clone()).unwrap());
        assert_matches("MembershipApproved", &to_value(Approved { id: 2, signed }).unwrap());
        let proposed = Proposed { name: "m".into(), authority: "lap".into(), id: Some(2), error: None };
        assert_variant("MachineRmProposed", &to_value(RmAnswer::Proposed(proposed.clone())).unwrap());
        let left = Left { name: "lap".into(), hub: "k".into(), removed: Some(removed), error: None, dropped: vec!["k".into()], proposed: Some(proposed) };
        assert_variant("MachineRmLeft", &to_value(RmAnswer::Left(left)).unwrap());
        assert_eq!(union_members("MachineRmResult"), ["MachineRmRevoked", "MachineRmProposed", "MachineRmLeft"]);
        use crate::serve::fleet::proposals::{Change, Proposal};
        let p = Proposal { id: 2, from: "box".into(), op: Change::Revoke { name: "m".into() }, at: 1 };
        let p = to_value(p).unwrap();
        assert_matches("FleetProposal", &p);
        assert_variant_kind("FleetChangeRevoke", &p["op"]);
        let s = crate::serve::fleet::membership_api::test_support::fleet("lap", &SigningKey::from_seed([1; 32]), "lap", &[]);
        let g = s.log.membership().unwrap().genesis;
        let i = crate::serve::fleet::membership::Introduction::sign(&g, "new", key, "lap", &SigningKey::from_seed([1; 32]));
        assert_variant_kind("FleetChangeAdd", &to_value(Change::Add { introduction: i }).unwrap());
        let old = SigningKey::from_seed([1; 32]);
        let by_old = crate::serve::fleet::membership::countersign_rotate(&g, "lap", &key, &old);
        assert_variant_kind("FleetChangeRotate", &to_value(Change::Rotate { name: "lap".into(), old: old.public_key(), new: key, by_old }).unwrap());
        assert_eq!(union_members("FleetChange"), ["FleetChangeAdd", "FleetChangeRevoke", "FleetChangeRotate"]);
        use super::super::changes::{RotateProposed, Rotated, Rotation};
        let signed = Signed { name: "lap".into(), change: "rotate `lap`'s key".into(), pushed: vec![], unpushed: vec![], removed: None, error: None };
        let rotated = Rotation::Rotated(Rotated { key, fingerprint: key.fingerprint(), signed });
        assert_variant("MembershipRotated", &to_value(&rotated).unwrap());
        let proposed = Proposed { name: "pi".into(), authority: "lap".into(), id: Some(3), error: None };
        let rp = Rotation::Proposed(RotateProposed { key, fingerprint: key.fingerprint(), proposed });
        assert_variant("MembershipRotateProposed", &to_value(&rp).unwrap());
        for r in [rotated, rp] {
            assert_eq!(serde_json::from_value::<Rotation>(to_value(&r).unwrap()).unwrap(), r, "round trip");
        }
        assert_eq!(union_members("MembershipRotateResult"), ["MembershipRotated", "MembershipRotateProposed"]);
        let synced = super::super::changes::Synced { changed: true, head: s.log.head() };
        assert_matches("FleetSynced", &to_value(synced).unwrap());
        assert_matches("FleetQueued", &to_value(super::super::changes::Queued { id: 1, pending: true }).unwrap());
        assert_eq!(union_members("FleetHeard"), ["'told'", "'leaving'", "'unreachable'"]);
        for heard in [Heard::Told, Heard::Leaving, Heard::Unreachable] {
            let wire = to_value(heard).unwrap();
            assert!(union_members("FleetHeard").contains(&format!("'{}'", wire.as_str().unwrap())));
        }
        assert_eq!(union_members("LinkState"), ["'idle'", "'connecting'", "'connected'", "'unreachable'"]);
        for (method, result) in [
            ("machines.probe", "MachineProbe"),
            ("machines.add", "MachineAdded"),
            ("machines.list", "MachineRow[]"),
            ("machines.unlink", "OkResult"),
            ("machines.rm", "MachineRmResult"),
            ("fleet.rm", "FleetRemoved"),
            ("fleet.place", "FleetPlaced"),
            ("fleet.leave", "OkResult"),
            ("fleet.forget", "OkResult"),
            ("fleet.propose", "FleetQueued"),
            ("fleet.sync", "FleetSynced"),
            ("membership.pending", "FleetProposal[]"),
            ("membership.approve", "MembershipApproved"),
            ("membership.deny", "OkResult"),
            ("membership.rotate", "MembershipRotateResult"),
            ("machines.set", "OkResult"),
            ("machines.get", "MachineInfo"),
        ] {
            let ty = prop_type("ApiMethods", method).unwrap_or_else(|| panic!("ApiMethods lacks `{method}`"));
            assert!(ty.contains(&format!("result: {result} ")), "{method}: {ty}");
        }
    }

    /// One variant of a `kind`-tagged union, as `dts::assert_variant` checks
    /// a `type`-tagged one.
    fn assert_variant_kind(iface: &str, value: &Value) {
        crate::serve::dts::assert_matches(iface, value);
        let tag = value["kind"].as_str().unwrap();
        assert_eq!(crate::serve::dts::prop_type(iface, "kind"), Some(format!("'{tag}'")), "{iface}");
    }
}
