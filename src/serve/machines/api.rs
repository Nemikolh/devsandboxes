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
//! on the same probe: every check first, then the machine's `member.set`,
//! this host's registry entry, and this host's `hub`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::inbound::{self, Neighbors};
use super::join::{self, Fleet};
use super::link::{self, Link};
use super::service::Handle;
use super::ssh::{self, Dial};
use super::{Entry, MachineKind, check_name};
use crate::serve::api::{ApiError, RootCandidate, parse};
use crate::serve::capacity::Capacity;
use crate::serve::fleet::member::{self, Hub, MemberInfo};
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
    /// hub, and writes `hub` when this host joins or becomes a hub. `None`
    /// without a data dir (`add` refuses).
    pub member: Option<PathBuf>,
    /// What names this host when `member.toml` doesn't.
    pub hostname: String,
    /// `add` wrote this host's `hub`: the daemon follows it as after a
    /// `member.set` (`daemon::Shared::set_hub`).
    pub hub_changed: Box<dyn Fn(Hub) + Send + Sync>,
    /// End inbound neighbor `name`'s link: its `fleet-link`'s held
    /// `links.inbound` connection gets `links.cycle` and hangs up
    /// (`rm`, `fleet.forget`).
    pub end_inbound: Box<dyn Fn(&str) + Send + Sync>,
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

/// `machines.probe`'s answer: the machine's devsandbox and its candidate
/// roots.
#[derive(Debug, Serialize, Deserialize)]
pub struct Probed {
    pub version: String,
    pub protocol: u32,
    pub roots: Vec<RootCandidate>,
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
    let remote = link.remote();
    to_value(Probed { version: remote.version.clone(), protocol: remote.protocol, roots })
}

#[derive(Deserialize)]
struct AddParams {
    target: String,
    #[serde(default)]
    name: Option<String>,
    roots: Vec<String>,
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
    if p.roots.is_empty() {
        return Err(ApiError::invalid("no roots: pass --config <path on the machine>"));
    }
    // A `--name` is the registry name if the add goes through: checked
    // before the ssh, so a taken one fails fast; again at the save.
    if let Some(name) = &p.name {
        check_free(ctx, name, true)?;
    }
    let (this_name, this_hub) = own_member(ctx)?;

    // No name yet: until the machine says its own, errors say the target.
    let probe = Entry { command: p.command.clone(), ..Entry::ssh(p.target.clone(), p.target.clone()) };
    let mut link = connect(&probe, ctx)?;
    let target: MemberInfo = remote_call(&mut link, &p.target, "member.get", json!({}))?;
    let target_rows: Vec<MachineRow> = remote_call(&mut link, &p.target, "machines.list", Value::Null)?;
    let target_known: Vec<String> = target_rows.into_iter().map(|r| r.name).collect();
    let this_known: Vec<String> = machines(ctx)?.into_iter().map(|e| e.name).collect();
    let target_hub = Hub::of(target.hub.as_deref().unwrap_or_default());
    let plan = join::plan(
        &join::This { name: &this_name, hub: &this_hub, known: &this_known },
        &join::Target { label: &p.target, name: target.name.as_deref(), hub: &target_hub, known: &target_known },
        p.name.as_deref(),
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

    // Every check passed: the writes, the machine's first.
    let mut written: Vec<String> = Vec::new();
    if let Some(set) = &plan.target_set {
        let params = json!({ "name": set.name, "hub": set.hub });
        let _: MemberInfo = remote_call(&mut link, &entry.name, "member.set", params)?;
        written.push(format!("{}'s member.toml (name `{}`, hub `{}`)", entry.name, set.name, set.hub));
    }
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
    if let Some(hub) = &plan.this_hub {
        let path = ctx.member.as_deref().expect("own_member checked it");
        // The name the fleet now knows this host by is pinned with it.
        let set = member::SetParams { name: Some(this_name.clone()), hub: Some(hub.clone()), ..Default::default() };
        let info = member::set(path, &ctx.hostname, &set).map_err(|e| after(e, &written))?;
        (ctx.hub_changed)(Hub::of(info.hub.as_deref().unwrap_or_default()));
    }
    if let Some(links) = &ctx.links {
        links.reload();
    }
    let warning = (version != ctx.version)
        .then(|| format!("{} runs devsandbox {version}, this is {}", entry.name, ctx.version));
    to_value(Added { name: entry.name, roots: entry.roots, version, warning, fleet: plan.fleet })
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
fn list(ctx: &Ctx) -> Answer {
    let rows: Vec<MachineRow> = machines(ctx)?
        .into_iter()
        .map(|e| {
            let live = ctx.links.as_ref().and_then(|l| l.remotes().get(&e.name)).unwrap_or_default();
            let (kind, target) = e.kind.wire();
            MachineRow {
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
    use crate::serve::fleet::member::Member;
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
    /// (`base/remote-home`), member file and registry, reporting `semver`.
    fn remote_opts(base: &Path, semver: &str) -> Options {
        let mut o = opts(1, 1500);
        o.version.semver = semver.into();
        o.state = Some(base.join("remote-state.toml"));
        o.home = Some(base.join("remote-home"));
        o.member = Some(base.join("remote-member.toml"));
        o.machines = Some(base.join("remote-machines.toml"));
        o
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
            let base = scratch(tag);
            std::fs::create_dir_all(&base).unwrap();
            member::write_for_test(&base.join("member.toml"), &this);
            member::write_for_test(&base.join("remote-member.toml"), &machine);
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
            member::load(&self.base.join("remote-member.toml")).unwrap().unwrap()
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
        assert_eq!(
            r,
            json!({"version": "0.6.0", "protocol": 1, "roots": [{"root": dot, "sandboxes": [{"name": "api"}, {"name": "web"}], "error": null}]})
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
        let (code, _) = err(&mut local, "machines.add", json!({"target": "me@box", "roots": ["/x"]}));
        assert_eq!(code, "failed");
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
        let (code, message) =
            err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes", "~/gone", "rel"]}));
        assert_eq!(code, "invalid");
        assert!(message.starts_with("box: `~/gone`: no directory ") && message.ends_with("; `rel`: not absolute"), "{message}");
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/broken"]}));
        assert!(code == "invalid" && message.contains("devsandboxes.toml"), "{code}: {message}");
        assert_eq!(err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": []})).0, "invalid");
        let (code, message) =
            err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~"], "command": " "}));
        assert!(code == "invalid" && message.contains("`command`"), "{code}: {message}");
        assert!(!registry.exists(), "nothing saved");

        assert_eq!((pair.this(), pair.machine()), (named("lap", ""), named("box", "")), "nothing written");

        // The default name is the machine's member name; roots are its
        // canonical paths, deduplicated in order. Neither in a fleet: a
        // fleet of two, this host the hub.
        let dot = std::fs::canonicalize(pair.home.join(".devsandboxes")).unwrap();
        let home = std::fs::canonicalize(&pair.home).unwrap();
        let via = format!("{}/broken/../.devsandboxes", pair.home.display());
        let r = ok(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes", "~", via]}));
        let fleet = json!({"hub": "lap", "joined": "new"});
        assert_eq!(r, json!({"name": "box", "roots": [dot, home], "version": "0.6.0", "warning": null, "fleet": fleet}));
        assert_eq!(pair.registry(), vec![Entry { roots: vec![dot.clone(), home.clone()], ..Entry::ssh("box", "me@box") }]);
        assert_eq!((pair.this(), pair.machine()), (named("lap", "self"), named("box", "lap")));

        // Taken: the machine is `box` in this fleet, whatever the target
        // or `--name` says.
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "ssh://box:2200", "roots": ["~"]}));
        assert_eq!((code.as_str(), message.as_str()), ("invalid", "a machine named `box` exists"));
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "ssh://box:2200", "name": "box2", "roots": ["~"]}));
        assert_eq!(code, "invalid");
        assert_eq!(message, "ssh://box:2200 is in `lap`'s fleet as `box`: pass --name box, or no --name");
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "x", "name": "box", "roots": ["~"]}));
        assert_eq!((code.as_str(), message.as_str()), ("invalid", "a machine named `box` exists; pass --name"), "fast, before the ssh");
        let (code, _) = err(&mut pair.local, "machines.add", json!({"target": "x", "name": "a/b", "roots": ["~"]}));
        assert_eq!(code, "invalid");

        // Unregistered (the fleet stays), added again: an extra link in the
        // same fleet, nothing written but the entry.
        assert_eq!(ok(&mut pair.local, "machines.unlink", json!({"name": "box"})), json!({"ok": true}));
        let cmd = "~/.local/bin/devsandbox";
        let r = ok(
            &mut pair.local,
            "machines.add",
            json!({"target": "ssh://box:2200", "name": "box", "roots": ["~/.devsandboxes"], "agent": true, "command": cmd}),
        );
        assert_eq!((&r["name"], &r["fleet"]), (&json!("box"), &json!({"hub": "lap", "joined": "same"})));
        assert_eq!((pair.this(), pair.machine()), (named("lap", "self"), named("box", "lap")));
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
                {"name": "box", "kind": "ssh", "target": "ssh://box:2200", "roots": [dot], "command": cmd, "agent": true, "version": null, "capacity": null},
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
    fn the_machine_joins_this_hosts_fleet_under_its_name() {
        let mut pair = Pair::members("mach-join-target", "0.6.0", named("lap", "k"), named("box", ""), |_| {});
        let r = ok(&mut pair.local, "machines.add", json!({"target": "me@box", "name": "b2", "roots": ["~/.devsandboxes"]}));
        assert_eq!((&r["name"], &r["fleet"]), (&json!("b2"), &json!({"hub": "k", "joined": "target"})));
        assert_eq!(pair.machine(), named("b2", "k"), "renamed as it joined");
        assert_eq!(pair.this(), named("lap", "k"), "this host unchanged");
        assert_eq!(pair.registry()[0].name, "b2");
        pair.finish();
    }

    #[test]
    fn this_host_joins_the_machines_fleet() {
        let mut pair = Pair::members("mach-join-this", "0.6.0", named("lap", ""), named("box", "self"), |_| {});
        let r = ok(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes"]}));
        assert_eq!((&r["name"], &r["fleet"]), (&json!("box"), &json!({"hub": "box", "joined": "this"})));
        assert_eq!((pair.this(), pair.machine()), (named("lap", "box"), named("box", "self")));
        // Through the daemon's own `member.set` path: `member.get` says it.
        assert_eq!(ok(&mut pair.local, "member.get", Value::Null)["hub"], "box");
        pair.finish();
    }

    #[test]
    fn two_hubs_are_refused_and_nothing_written() {
        let (this, machine) = (named("lap", "self"), named("box", "k"));
        let mut pair = Pair::members("mach-two-hubs", "0.6.0", this.clone(), machine.clone(), |_| {});
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes"]}));
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
            let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes"]}));
            assert_eq!((code.as_str(), message.as_str()), ("invalid", says));
            assert_eq!((pair.this(), pair.machine()), (this, machine), "nothing written");
            assert!(!pair.base.join("machines.toml").exists());
            pair.finish();
        }
        // This host joining: its name is one the machine's fleet knows.
        let mut pair = Pair::members("mach-name-known", "0.6.0", named("lap", ""), named("box", "self"), |_| {});
        let theirs = Entry { roots: vec!["/r".into()], ..Entry::ssh("lap", "me@other") };
        crate::serve::machines::update(&pair.base.join("remote-machines.toml"), |e| e.push(theirs)).unwrap();
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes"]}));
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
        ok(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes"], "command": cmd}));
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
        let r = ok(&mut pair.local, "machines.add", json!({"target": "box", "roots": ["~/.devsandboxes"]}));
        assert_eq!(r["warning"], "box runs devsandbox 0.5.0, this is 0.6.0");
        pair.finish();
    }

    #[test]
    fn npm_typings_match() {
        use crate::serve::dts::{assert_matches, prop_type, union_members};
        let root = || RootCandidate { root: "/cfg".into(), sandboxes: vec![], error: None, fleet: false };
        assert_matches("MachineProbe", &to_value(Probed { version: "0.6.0".into(), protocol: 1, roots: vec![root()] }).unwrap());
        let fleet = Fleet { hub: "k".into(), joined: join::Joined::Target };
        let added = Added { name: "m".into(), roots: vec!["/cfg".into()], version: "0.6.0".into(), warning: None, fleet };
        let added = to_value(added).unwrap();
        assert_matches("MachineAdded", &added);
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
        assert_variant("MachineRmRemoved", &to_value(RmAnswer::Removed(removed.clone())).unwrap());
        let left = Left { name: "lap".into(), hub: "k".into(), removed: Some(removed), error: None, dropped: vec!["k".into()] };
        assert_variant("MachineRmLeft", &to_value(RmAnswer::Left(left)).unwrap());
        assert_eq!(union_members("MachineRmResult"), ["MachineRmRemoved", "MachineRmLeft"]);
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
            ("fleet.leave", "OkResult"),
            ("fleet.forget", "OkResult"),
            ("machines.set", "OkResult"),
            ("machines.get", "MachineInfo"),
        ] {
            let ty = prop_type("ApiMethods", method).unwrap_or_else(|| panic!("ApiMethods lacks `{method}`"));
            assert!(ty.contains(&format!("result: {result} ")), "{method}: {ty}");
        }
    }
}
