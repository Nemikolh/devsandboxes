//! The trust table (docs/architecture/daemon/fleet.md, *The trust table*):
//! what a remote caller (a [`Peer`]: a fleet hub over its lending link's
//! `API` streams, or the other end of a peer link over the connections its
//! `link.attach` turned) may do on this member.
//! Each such connection is served with a [`Peer`] (the daemon's
//! `Table::Peer`); every request on it goes through
//! [`admit`] first and its answer through [`filter_answer`], every
//! `instances.snapshot` pushed to it through [`filter_snapshot`], each
//! against the [`Reach`] read from this host's files for that request
//! ([`Reach::load`]: `member.toml`'s `trust` and `roots`, `state.toml`).
//!
//! Under `trust = "worker"` (the default) a peer reaches this member's own
//! roots ([`own_roots`] plus `member.toml`'s `roots`: each exactly, not
//! what's nested in one) and its fleet root (all under it):
//!
//! - [`decide`] (pure, over the method and params): hello, `subscribe` /
//!   `unsubscribe` to `instances` only, `instances.watch` / `unwatch`, the
//!   quick calls (`machines::forward::QUICK`), `capacity.get`, `roots.list`
//!   and the verbs but `gc` (host maintenance); never a `machine` param or a federated (`machines:
//!   true`) request; everything else (`machines.*`,
//!   `fleet.*`, `lend.*`, `shutdown`, `helper.call`, `forwards.*`, the
//!   Inbox, …) is `denied`.
//! - [`admit`] then confines the params: every `root` / `dir` canonicalized
//!   and admitted by the [`Reach`] (rewritten to its canonical form), every
//!   instance or container named one whose `config_dir` the reach admits (a
//!   service container: one of those roots'), a verb's `all` only while
//!   every instance in `state.toml` is reached, `config.migrate` /
//!   `config.ack` (edits of a config) only under the fleet root, and a
//!   verb's `env` replaced by the peer's when it has one.
//!
//! Under `trust = "full"` none of that: the peer has the API a local client
//! has, except `member.*`, `link.*` and `links.*` (this member's own settings, and its
//! links), refused under both; a verb's `env` is still the peer's when it
//! has one.
//!
//! - [`refuse_new_work`]: while lending `drain`, a `run` from the lending
//!   link's peer (the one with a fleet root) is `denied` ([`DRAINING`]),
//!   under either trust; verbs on instances already here go on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::serve::api::ApiError;
use crate::serve::fleet::member::{self, Member};
use crate::serve::machines::forward as fwd;
use crate::serve::verbs;
use crate::state::State;
use crate::worker_policy::Trust;

/// A remote caller of this member: one per lending link, one per attached
/// connection (`link.attach {channel}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Who it is (the hub's ssh target, a link peer's machine name), for
    /// the log.
    pub name: String,
    /// This member's fleet root for it, canonical: the lending link's, a
    /// link peer's ([`peer_fleet_root`]). `None` only in tests.
    pub fleet_root: Option<PathBuf>,
    /// `Some`: the env its verbs run with, whatever it sent (the lending
    /// link's and a link peer's: this host's own, the daemon's). `None`: a
    /// verb's env as sent.
    pub env: Option<BTreeMap<String, String>>,
    /// The linking host's agent relayed here (a peer link's
    /// `agent-link-<peer>.sock`, `endpoint::link_agent_socket`): with `env`,
    /// a verb's `SSH_AUTH_SOCK` while it answers, so host git there (a
    /// worktree's clone) uses the agent the linking host serves. `None`:
    /// `env` as it is.
    pub agent: Option<PathBuf>,
}

/// The longest `link.attach` `peer` taken: a hostname fits well within it.
pub const PEER_NAME_MAX: usize = 255;

/// `link.attach`'s `peer` (pure): a non-empty string of at most
/// [`PEER_NAME_MAX`] bytes without control characters ([`attach_params`]
/// then holds it to `machines::check_name`).
pub fn attach_peer(params: &Value) -> Result<String, ApiError> {
    let Some(name) = params.get("peer").and_then(Value::as_str) else {
        return Err(ApiError::invalid("`link.attach` needs `peer`, the link peer's machine name"));
    };
    if name.is_empty() || name.len() > PEER_NAME_MAX || name.chars().any(char::is_control) {
        return Err(ApiError::invalid(format!(
            "`peer` must be 1 to {PEER_NAME_MAX} bytes without control characters"
        )));
    }
    Ok(name.to_string())
}

/// What a `link.attach {channel}` stream carries: one link channel's
/// streams handed to the daemon (docs/reference/api.md, `link.attach`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// API requests, served with the trust table.
    Api,
    /// A session attach or an exec (`fleet::pty::serve`).
    Pty,
    /// A forward's bridge (`fleet::tunnel::serve`).
    Tunnel,
    /// One bulk transfer into the peer's fleet root (`fleet::code::receivers`).
    Transfer,
    /// This host's ssh agent, while its registry entry for the peer has
    /// `agent` on.
    Agent,
}

impl Channel {
    pub fn parse(s: &str) -> Option<Channel> {
        Some(match s {
            "api" => Channel::Api,
            "pty" => Channel::Pty,
            "tunnel" => Channel::Tunnel,
            "transfer" => Channel::Transfer,
            "agent" => Channel::Agent,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Api => "api",
            Channel::Pty => "pty",
            Channel::Tunnel => "tunnel",
            Channel::Transfer => "transfer",
            Channel::Agent => "agent",
        }
    }
}

/// `link.attach`'s params (pure): [`attach_peer`]'s `peer` and the
/// required `channel`. The peer's name also names its fleet root
/// ([`peer_fleet_root`]): it must pass `machines::check_name`.
pub fn attach_params(params: &Value) -> Result<(String, Channel), ApiError> {
    let name = attach_peer(params)?;
    let channel = match params.get("channel") {
        None | Some(Value::Null) => return Err(ApiError::invalid("`link.attach` needs `channel`")),
        Some(Value::String(s)) => Channel::parse(s)
            .ok_or_else(|| ApiError::invalid(format!("no channel `{s}` (api, pty, tunnel, transfer or agent)")))?,
        Some(_) => return Err(ApiError::invalid("`channel` must be a string")),
    };
    crate::serve::machines::check_name(&name).map_err(|e| ApiError::invalid(format!("`peer`: {e:#}")))?;
    Ok((name, channel))
}

/// A fleet member `peer`'s fleet root under data dir `data`:
/// `<data>/fleet/<peer>/` (decision 24 of the fleet plan), created,
/// canonical. `peer` must be a machine name (one plain path component).
pub fn peer_fleet_root(data: &Path, peer: &str) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    crate::serve::machines::check_name(peer)?;
    let root = data.join("fleet").join(peer);
    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
    std::fs::canonicalize(&root).with_context(|| format!("cannot resolve {}", root.display()))
}

/// Where a [`Reach`] is read from: this host's files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Files {
    pub state: PathBuf,
    /// `member.toml`; `None` without a data dir (then: `worker`, no picked
    /// roots).
    pub member: Option<PathBuf>,
    /// The home `~/.devsandboxes` is found in ([`own_roots`]).
    pub home: Option<PathBuf>,
}

/// What a [`Peer`] reaches here, for one request (see the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reach {
    pub trust: Trust,
    /// Canonical; everything under it is reached.
    pub fleet_root: Option<PathBuf>,
    /// This member's own roots (canonical where they exist): each is
    /// reached exactly, not what's nested in it.
    pub own: BTreeSet<PathBuf>,
}

/// This host's own config roots: every distinct `config_dir` in `state`,
/// plus `~/.devsandboxes` when `home` has one, canonicalized (a root that's
/// gone stays as recorded). What `roots.list` answers, and the base of a
/// [`Reach`]'s `own`.
pub fn own_roots(state: &State, home: Option<&Path>) -> BTreeSet<PathBuf> {
    let recorded = state.instances.values().filter_map(|i| i.config_dir.clone());
    let home = home.map(|h| h.join(".devsandboxes")).filter(|d| d.is_dir());
    recorded.chain(home).map(|r| std::fs::canonicalize(&r).unwrap_or(r)).collect()
}

impl Reach {
    /// Over what's read (pure but for canonicalizing): `member`'s `trust`
    /// (none: `worker`), [`own_roots`] of `state` plus `member`'s `roots`.
    pub fn of(member: Option<&Member>, state: &State, home: Option<&Path>, fleet_root: Option<&Path>) -> Reach {
        let mut own = own_roots(state, home);
        if let Some(m) = member {
            own.extend(m.roots.iter().map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone())));
        }
        let trust = member.map(|m| m.trust).unwrap_or_default();
        Reach { trust, fleet_root: fleet_root.map(Path::to_path_buf), own }
    }

    /// [`Reach::of`] this host's `files`. Fails closed: a `member.toml` that
    /// doesn't load is `worker` with no picked roots, a `state.toml` that
    /// doesn't load is no instances' roots; either comes back as the error
    /// to log.
    pub fn load(files: &Files, fleet_root: Option<&Path>) -> (Reach, Option<String>) {
        let mut warning = None;
        let member = match files.member.as_deref().map(member::load) {
            None | Some(Ok(None)) => None,
            Some(Ok(Some(m))) => Some(m),
            Some(Err(e)) => {
                warning = Some(format!("{e:#}; taken as `trust = \"worker\"`, no `roots`"));
                None
            }
        };
        let state = State::load_at(&files.state).unwrap_or_else(|e| {
            warning = Some(format!("{e:#}; taken as no instances"));
            State::default()
        });
        (Reach::of(member.as_ref(), &state, files.home.as_deref(), fleet_root), warning)
    }

    /// Whether the peer reaches canonical path `real`: anything under
    /// `full`; else under the fleet root, or exactly an own root.
    pub fn admits(&self, real: &Path) -> bool {
        self.trust == Trust::Full || self.in_fleet_root(real) || self.own.contains(real)
    }

    /// Whether canonical path `real` is under the fleet root.
    pub fn in_fleet_root(&self, real: &Path) -> bool {
        self.fleet_root.as_deref().is_some_and(|r| real.starts_with(r))
    }

    /// Whether recorded config root `dir` is admitted: canonicalized when it
    /// still exists, else as recorded; never with a `..`.
    pub fn admits_recorded(&self, dir: &Path) -> bool {
        if dir.components().any(|c| c == Component::ParentDir) {
            return false;
        }
        self.admits(&std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
    }
}

/// What kind of request [`decide`] let through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Hello,
    Subscribe,
    Watch,
    Unwatch,
    Quick,
    /// `capacity.get`: this host's report, for the hub's placement.
    Capacity,
    /// `roots.list`, with or without `paths`: read-only (`machines.add`
    /// checks typed roots through it).
    Roots,
    Verb,
}

/// The refusal of a hub's `run` while this host drains.
pub const DRAINING: &str = "this member is draining: no new work";

/// `drain` is the one hard no for new work (pure): a `run` from the
/// lending link's `peer` (one with a fleet root) while `draining` is
/// `denied` with [`DRAINING`]; everything else (`start`, `stop`, `rm`,
/// `rebuild` of what's placed here, any request of a peer without a fleet
/// root) is left to [`admit`].
pub fn refuse_new_work(method: &str, peer: &Peer, draining: bool) -> Result<(), ApiError> {
    if draining && peer.fleet_root.is_some() && method == "run" {
        return Err(ApiError::denied(DRAINING));
    }
    Ok(())
}

/// Whether a `worker` member's peer may send `method` with `params` at all
/// (pure).
pub fn decide(method: &str, params: &Value) -> Result<Gate, ApiError> {
    if fwd::machine_of(params)?.is_some() {
        return Err(ApiError::denied("a remote caller can't reach past this member (`machine`)"));
    }
    if fwd::federated(params) {
        return Err(ApiError::denied("a remote caller can't reach past this member (`machines`)"));
    }
    let gate = match method {
        "hello" => Gate::Hello,
        "subscribe" | "unsubscribe" => {
            let topics = params.get("topics").and_then(Value::as_array);
            if topics.is_some_and(|t| t.iter().any(|t| t.as_str() != Some("instances"))) {
                return Err(ApiError::denied("a remote caller subscribes to `instances` only"));
            }
            Gate::Subscribe
        }
        "instances.watch" => Gate::Watch,
        "instances.unwatch" => Gate::Unwatch,
        m if fwd::is_quick(m) => Gate::Quick,
        "capacity.get" => Gate::Capacity,
        "roots.list" => Gate::Roots,
        "gc" => return Err(ApiError::denied(format!(
            "`gc` is host maintenance: not open to remote callers under this member's `worker` trust ({FULL_HINT})"
        ))),
        m if verbs::is_verb(m) => Gate::Verb,
        m => return Err(ApiError::denied(format!("`{m}` isn't open to remote callers under this member's `worker` trust ({FULL_HINT})"))),
    };
    Ok(gate)
}

/// How a `worker` refusal says what opens it.
const FULL_HINT: &str = "`devsandbox machine trust full` there opens it";

/// The params naming an instance (an exact name or id, as the quick calls
/// take it), per method.
fn instance_keys(method: &str) -> &'static [&'static str] {
    match method {
        "instances.cancel" | "instances.signal" | "instances.rename" | "instances.done" | "instances.buildLog"
        | "sessions.list" | "sessions.rm" => &["instance"],
        "rebuild" | "start" | "stop" | "rm" => &["instance"],
        _ => &[],
    }
}

/// The verbs that act on an existing instance, which a hub must name.
fn needs_instance(method: &str) -> bool {
    matches!(method, "rebuild" | "start" | "stop" | "rm")
}

/// `path` checked against `reach`: absolute, no `..`, and canonicalized
/// (symlinks resolved) to one the reach admits ([`Reach::admits`]). A path
/// that doesn't exist is `not-found`, unless `!exists` (an unwatch of a
/// removed root), where it is checked as written.
pub fn confine_path(reach: &Reach, path: &Path, exists: bool) -> Result<PathBuf, ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::invalid(format!("`{}` must be absolute", path.display())));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(ApiError::denied(format!("`{}` has a `..`", path.display())));
    }
    let real = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(_) if !exists => path.to_path_buf(),
        Err(_) => return Err(ApiError::not_found(format!("no directory {}", path.display()))),
    };
    if !reach.admits(&real) {
        return Err(ApiError::denied(format!(
            "`{}` isn't one of this member's roots (`devsandbox machine roots add <path>` there adds one)",
            path.display()
        )));
    }
    Ok(real)
}

/// Whether recorded config root `dir` is under `root`: canonicalized when it
/// still exists, else as recorded; never with a `..`.
pub(crate) fn root_under(dir: &Path, root: &Path) -> bool {
    if dir.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()).starts_with(root)
}

/// The instances of `state` the peer may name: names and ids of those whose
/// `config_dir` `reach` admits ([`Reach::admits_recorded`]).
pub fn lent_instances(state: &State, reach: &Reach) -> Lent {
    let mut lent = Lent::default();
    for (name, i) in &state.instances {
        if i.config_dir.as_deref().is_some_and(|d| reach.admits_recorded(d)) {
            lent.names.insert(name.clone());
            lent.ids.insert(i.instance_id.clone());
            lent.containers.insert(i.container.clone());
            if let Some(dir) = &i.config_dir {
                lent.roots.insert(dir.clone());
            }
        }
    }
    lent
}

/// [`lent_instances`]' answer.
#[derive(Debug, Default)]
pub struct Lent {
    pub names: BTreeSet<String>,
    pub ids: BTreeSet<String>,
    pub containers: BTreeSet<String>,
    /// Their config roots, as recorded.
    pub roots: BTreeSet<PathBuf>,
}

impl Lent {
    /// The `project_id`s of [`Lent::roots`] (`commands::services`): what
    /// their service containers are labelled and named by. A root that no
    /// longer resolves has none.
    fn projects(&self) -> BTreeSet<String> {
        self.roots.iter().filter_map(|d| crate::commands::services::project_id(d).ok()).collect()
    }
}

/// Gate, then confine, a peer's request (see the module doc): the params
/// to run it with. `state` is this host's `state.toml`, read only when the
/// request names an instance or a container under `worker`.
pub fn admit(method: &str, mut params: Value, peer: &Peer, reach: &Reach, state: &Path) -> Result<Value, ApiError> {
    if method.starts_with("member.") || method.starts_with("link.") || method.starts_with("links.") {
        return Err(ApiError::denied(format!("`{method}` is this member's own: local clients only")));
    }
    if reach.trust == Trust::Full {
        if verbs::is_verb(method)
            && let Some(obj) = params.as_object_mut()
        {
            peer_env(obj, peer)?;
        }
        return Ok(params);
    }
    let gate = decide(method, &params)?;
    let Some(obj) = params.as_object_mut() else {
        return Ok(params);
    };
    for key in ["root", "dir"] {
        let Some(v) = obj.get(key) else { continue };
        let Some(path) = v.as_str() else {
            return Err(ApiError::invalid(format!("`{key}` must be a path")));
        };
        let real = confine_path(reach, Path::new(path), gate != Gate::Unwatch)?;
        if matches!(method, "config.migrate" | "config.ack") && !reach.in_fleet_root(&real) {
            return Err(ApiError::denied(format!(
                "`{method}` edits this member's own config at {}: that needs `devsandbox machine trust full` on it",
                real.display()
            )));
        }
        obj.insert(key.into(), Value::String(real.to_string_lossy().into_owned()));
    }
    // The methods whose path is how they find what they act on: without
    // one they'd fall back to something outside the scope, or fail anyway.
    let path_key = match gate {
        Gate::Watch | Gate::Unwatch => Some("dir"),
        Gate::Verb => Some("root"),
        Gate::Quick if method == "instances.list" => Some("dir"),
        Gate::Quick
            if method.starts_with("sandboxes.")
                || method.starts_with("config.")
                || method.starts_with("sessions.")
                || method == "services.get" =>
        {
            Some("root")
        }
        _ => None,
    };
    if let Some(key) = path_key
        && !obj.contains_key(key)
    {
        return Err(ApiError::invalid(format!("`{method}` needs `{key}`")));
    }

    let keys = instance_keys(method);
    let named_container = matches!(method, "containers.logs" | "containers.inspect");
    // `--all` is every instance in `state.toml`, not the root's.
    let all = gate == Gate::Verb && obj.get("all").and_then(Value::as_bool) == Some(true);
    if !keys.is_empty() || method == "instances.procs" || named_container || all {
        let state = State::load_at(state).map_err(ApiError::internal)?;
        let lent = lent_instances(&state, reach);
        let unknown = |what: &str| ApiError::not_found(format!("no instance `{what}`"));
        for key in keys {
            let Some(name) = obj.get(*key).and_then(Value::as_str) else {
                if needs_instance(method) && !all {
                    return Err(ApiError::invalid(format!("`{method}` from a remote caller needs `{key}`")));
                }
                // Not a string: the method's own parse refuses it.
                continue;
            };
            if gate == Gate::Verb {
                // Resolved as the CLI resolves it (`commands::resolve_instance`):
                // every candidate must be lent.
                let matches = crate::commands::instance_matches(&state, name);
                if matches.iter().any(|m| !lent.names.contains(m)) {
                    return Err(unknown(name));
                }
            } else if !lent.names.contains(name) && !lent.ids.contains(name) {
                return Err(unknown(name));
            }
        }
        if method == "instances.procs"
            && let Some(names) = obj.get("instances").and_then(Value::as_array)
            && let Some(bad) = names.iter().filter_map(Value::as_str).find(|n| !lent.names.contains(*n))
        {
            return Err(unknown(bad));
        }
        if named_container
            && let Some(c) = obj.get("container").and_then(Value::as_str)
            && !lent.containers.contains(c)
            && !lent.projects().iter().any(|p| c.starts_with(&crate::commands::services::service_container(p, "")))
        {
            return Err(ApiError::not_found(format!("no container `{c}` in this member's reach")));
        }
        if all && state.instances.len() != lent.names.len() {
            return Err(ApiError::denied(format!(
                "`{method}` with `all` reaches instances outside this member's roots ({FULL_HINT})"
            )));
        }
    }
    if gate == Gate::Verb {
        peer_env(obj, peer)?;
    }
    Ok(params)
}

/// A verb's `env` replaced by the peer's, when it has one, its
/// `SSH_AUTH_SOCK` the peer's relayed agent while that answers
/// ([`Peer::agent`]).
fn peer_env(obj: &mut serde_json::Map<String, Value>, peer: &Peer) -> Result<(), ApiError> {
    if let Some(env) = &peer.env {
        let mut env = env.clone();
        if let Some(agent) = peer.agent.as_deref().filter(|a| std::os::unix::net::UnixStream::connect(a).is_ok()) {
            env.insert("SSH_AUTH_SOCK".into(), agent.to_string_lossy().into_owned());
        }
        obj.insert("env".into(), serde_json::to_value(env).map_err(|e| ApiError::internal(e.into()))?);
    }
    Ok(())
}

/// A local answer to a peer's request, cut to `reach` (nothing cut under
/// `full`): an `instances.list` like a snapshot ([`filter_snapshot`]); an
/// `instances.resolve`'s matches to lent instances (none left is
/// `not-found`); `containers.list` / `containers.stats` rows to the lent
/// instances' containers and their config roots' services
/// ([`cut_containers`]; `state` is this host's `state.toml`, read for
/// those two only). Others pass as they are: their params were confined.
pub fn filter_answer(method: &str, mut answer: Value, reach: &Reach, state: &Path) -> Result<Value, ApiError> {
    if reach.trust == Trust::Full {
        return Ok(answer);
    }
    match method {
        "instances.list" => filter_snapshot(&mut answer, reach),
        "containers.list" | "containers.stats" => {
            let state = State::load_at(state).map_err(ApiError::internal)?;
            let lent = lent_instances(&state, reach);
            cut_containers(method, &mut answer, &lent.containers, &lent.projects());
        }
        "instances.resolve" => {
            let found: Vec<Value> = answer
                .get("found")
                .and_then(Value::as_array)
                .map(|f| f.iter().filter(|f| row_lent(f, reach)).cloned().collect())
                .unwrap_or_default();
            if found.is_empty() {
                return Err(ApiError::not_found("no instance matches"));
            }
            let names: Vec<Value> = found.iter().filter_map(|f| f.get("name").cloned()).collect();
            answer["found"] = Value::Array(found);
            answer["matches"] = Value::Array(names);
        }
        _ => {}
    }
    Ok(answer)
}

/// A `roots.list` answer (the listing, not a `paths` check) to a peer with
/// a fleet root, that root added last unless listed already (as
/// `{root, sandboxes: [], error: null, fleet: true}`: the placed configs
/// are under it), flagged `fleet` either way: what the peer reaches here
/// besides this member's own roots, so a peer link's other end lists it
/// among this member's roots (an inbound machine's roots) and knows where
/// its placement moves code to (`fleet::placed::fleet_root`).
pub fn with_fleet_root(mut answer: Value, reach: &Reach) -> Value {
    let (Some(root), Some(rows)) = (&reach.fleet_root, answer.as_array_mut()) else { return answer };
    match rows.iter_mut().find(|r| r.get("root").and_then(Value::as_str) == root.to_str()) {
        Some(row) => {
            if let Some(row) = row.as_object_mut() {
                row.insert("fleet".into(), Value::Bool(true));
            }
        }
        None => rows.push(serde_json::json!({ "root": root, "sandboxes": [], "error": null, "fleet": true })),
    }
    answer
}

/// A `containers.list` / `containers.stats` answer's `rows` cut, in place,
/// to `containers` (the lent instances') and the service containers of
/// `projects` (the lent instances' config roots' `project_id`s): a
/// listing row by its `devsandbox.project` label (none: dropped), a stats
/// row, which has no labels, by its name (`services::service_container`'s
/// `devsandbox-svc-<project>-` prefix, isolated ones' too).
pub fn cut_containers(method: &str, answer: &mut Value, containers: &BTreeSet<String>, projects: &BTreeSet<String>) {
    let Some(rows) = answer.get_mut("rows").and_then(Value::as_array_mut) else { return };
    let prefixes: Vec<String> = projects.iter().map(|p| crate::commands::services::service_container(p, "")).collect();
    rows.retain(|r| {
        let Some(name) = r.get("name").and_then(Value::as_str) else { return false };
        if containers.contains(name) {
            return true;
        }
        if method == "containers.stats" {
            return prefixes.iter().any(|p| name.starts_with(p.as_str()));
        }
        let project = r.get("labels").and_then(|l| l.get("devsandbox.project")).and_then(Value::as_str);
        project.is_some_and(|p| projects.contains(p))
    });
}

/// Whether a row (`InstanceRow`, a resolve's `found`) has a `root` `reach`
/// admits; a row without one isn't lent.
fn row_lent(row: &Value, reach: &Reach) -> bool {
    row.get("root").and_then(Value::as_str).is_some_and(|r| reach.admits_recorded(Path::new(r)))
}

/// An `instances.list` answer / `instances.snapshot` params cut to the
/// instances `reach` admits (by their rows' `root`), in place: other rows
/// gone, and their names from services' `used_by` and from
/// `thread_children`. The snapshot lists every instance in `state.toml`,
/// whatever `dir`, so this is what keeps what isn't reached out. Nothing
/// is cut under `full`.
pub fn filter_snapshot(snapshot: &mut Value, reach: &Reach) {
    if reach.trust == Trust::Full {
        return;
    }
    let mut kept = BTreeSet::new();
    let mut ids = BTreeSet::new();
    if let Some(rows) = snapshot.get_mut("instances").and_then(Value::as_array_mut) {
        rows.retain(|r| row_lent(r, reach));
        kept.extend(rows.iter().filter_map(|r| r.get("name").and_then(Value::as_str).map(String::from)));
        ids.extend(rows.iter().filter_map(|r| r.get("instance_id").and_then(Value::as_str).map(String::from)));
    }
    let keep = |v: &Value| v.as_str().is_some_and(|n| kept.contains(n));
    // An isolated service's container is per instance
    // (`services::isolated_service_container`: `…svc-<project>-<instance id>-<name>`),
    // so another root's instance of a same-named sandbox would show by name.
    let lent_container = |c: &Value| {
        let name = c.get(0).and_then(Value::as_str).unwrap_or_default();
        ids.iter().any(|id| name.contains(&format!("-{id}-")))
    };
    if let Some(services) = snapshot.get_mut("services").and_then(Value::as_array_mut) {
        for s in services {
            if let Some(used) = s.get_mut("used_by").and_then(Value::as_array_mut) {
                used.retain(keep);
            }
            if s.get("scope").and_then(Value::as_str) == Some("isolated")
                && let Some(containers) = s.get_mut("containers").and_then(Value::as_array_mut)
            {
                containers.retain(lent_container);
            }
        }
    }
    if let Some(owners) = snapshot.get_mut("thread_children").and_then(Value::as_object_mut) {
        for children in owners.values_mut() {
            if let Some(c) = children.as_object_mut() {
                c.retain(|_, child| keep(child));
            }
        }
        owners.retain(|_, c| c.as_object().is_some_and(|c| !c.is_empty()));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-lent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// A `worker` reach of fleet root `root` and own roots `own`.
    fn worker(root: &Path, own: &[&Path]) -> Reach {
        Reach { trust: Trust::Worker, fleet_root: Some(root.to_path_buf()), own: own.iter().map(|p| p.to_path_buf()).collect() }
    }

    /// The fleet root only.
    fn fleet(root: &Path) -> Reach {
        worker(root, &[])
    }

    #[test]
    fn the_allow_list() {
        use Gate::*;
        let p = json!({});
        let cases: &[(&str, Value, Result<Gate, &str>)] = &[
            ("hello", p.clone(), Ok(Hello)),
            ("subscribe", json!({"topics": ["instances"]}), Ok(Subscribe)),
            ("unsubscribe", json!({"topics": ["instances"]}), Ok(Subscribe)),
            ("subscribe", json!({"topics": ["instances", "inbox"]}), Err("denied")),
            ("subscribe", json!({"topics": ["forwards"]}), Err("denied")),
            ("instances.watch", json!({"dir": "/x"}), Ok(Watch)),
            ("instances.watch", json!({"dir": "/x", "machines": true}), Err("denied")),
            ("instances.unwatch", json!({"dir": "/x"}), Ok(Unwatch)),
            ("instances.list", json!({"dir": "/x"}), Ok(Quick)),
            ("instances.resolve", json!({"name": "a"}), Ok(Quick)),
            ("instances.resolve", json!({"name": "a", "machines": true}), Err("denied")),
            ("containers.logs", json!({"container": "c"}), Ok(Quick)),
            ("containers.list", json!({"all": true}), Ok(Quick)),
            ("containers.stats", p.clone(), Ok(Quick)),
            ("containers.list", json!({"machine": "m"}), Err("denied")),
            ("sandboxes.list", json!({"root": "/x"}), Ok(Quick)),
            ("config.migrate", json!({"root": "/x"}), Ok(Quick)),
            ("capacity.get", p.clone(), Ok(Capacity)),
            ("capacity.get", json!({"machine": "m"}), Err("denied")),
            ("run", json!({"root": "/x"}), Ok(Verb)),
            ("rm", json!({"root": "/x"}), Ok(Verb)),
            ("services.rebuild", json!({"root": "/x"}), Ok(Verb)),
            ("gc", json!({"root": "/x"}), Err("denied")),
            ("gc", json!({"root": "/x", "force": true}), Err("denied")),
            ("stop", json!({"root": "/x", "all": true}), Ok(Verb)),
            ("stop", json!({"root": "/x", "machine": "m"}), Err("denied")),
            ("instances.list", json!({"dir": "/x", "machine": "m"}), Err("denied")),
            ("instances.list", json!({"machine": 3}), Err("invalid")),
            ("machines.list", p.clone(), Err("denied")),
            ("machines.get", json!({"name": "m"}), Err("denied")),
            ("fleet.offer", p.clone(), Err("denied")),
            ("fleet.agent", json!({"worker": "w"}), Err("denied")),
            ("lend.get", p.clone(), Err("denied")),
            ("lend.set", json!({"state": "off"}), Err("denied")),
            ("member.get", p.clone(), Err("denied")),
            ("member.set", json!({"trust": "full"}), Err("denied")),
            ("shutdown", p.clone(), Err("denied")),
            ("helper.call", p.clone(), Err("denied")),
            ("forwards.add", p.clone(), Err("denied")),
            ("forwards.list", p.clone(), Err("denied")),
            ("roots.list", p.clone(), Ok(Roots)),
            ("roots.list", json!({"paths": ["~/x"]}), Ok(Roots)),
            ("roots.list", json!({"machine": "m"}), Err("denied")),
            ("bridges.ensure", p.clone(), Err("denied")),
            ("inbox.threads.list", p.clone(), Err("denied")),
            ("nope", p.clone(), Err("denied")),
        ];
        for (method, params, want) in cases {
            let got = decide(method, params).map_err(|e| e.code);
            assert_eq!(got, *want, "{method} {params}");
        }
        // A refusal says what opens it.
        let e = decide("machines.list", &p).unwrap_err();
        assert_eq!(
            e.message,
            "`machines.list` isn't open to remote callers under this member's `worker` trust (`devsandbox machine trust full` there opens it)"
        );
        let e = decide("gc", &json!({"root": "/x"})).unwrap_err();
        assert_eq!(
            e.message,
            "`gc` is host maintenance: not open to remote callers under this member's `worker` trust (`devsandbox machine trust full` there opens it)"
        );
        // Every quick call and verb but `gc` is lent (with confined params).
        for m in fwd::QUICK {
            assert_eq!(decide(m, &p).unwrap(), Quick, "{m}");
        }
        for m in ["run", "rebuild", "start", "stop", "rm", "services.rebuild"] {
            assert!(verbs::is_verb(m));
            assert_eq!(decide(m, &p).unwrap(), Verb, "{m}");
        }
    }

    #[test]
    fn an_attach_names_its_peer() {
        assert_eq!(attach_peer(&json!({"peer": "lap.local"})).unwrap(), "lap.local");
        let long = "x".repeat(PEER_NAME_MAX + 1);
        for bad in [json!({}), json!({"peer": 3}), json!({"peer": ""}), json!({"peer": "a\nb"}), json!({"peer": long})] {
            assert_eq!(attach_peer(&bad).unwrap_err().code, "invalid", "{bad}");
        }
    }

    #[test]
    fn an_attach_names_a_channel_and_a_machine_name() {
        for c in ["api", "pty", "tunnel", "transfer", "agent"] {
            let (name, channel) = attach_params(&json!({"peer": "lap.local", "channel": c})).unwrap();
            assert_eq!((name.as_str(), channel.as_str()), ("lap.local", c));
        }
        for bad in [
            json!({"peer": "lap"}),
            json!({"peer": "lap", "channel": null}),
            json!({"peer": "lap", "channel": "rpc"}),
            json!({"peer": "lap", "channel": 1}),
            // The name names a directory: one plain component.
            json!({"peer": "lap/../x", "channel": "api"}),
            json!({"peer": "..", "channel": "pty"}),
            json!({"peer": "a b", "channel": "transfer"}),
            json!({"channel": "api"}),
        ] {
            assert_eq!(attach_params(&bad).unwrap_err().code, "invalid", "{bad}");
        }
    }

    #[test]
    fn a_peers_fleet_root_is_created_under_fleet() {
        let data = scratch("peer-root");
        let root = peer_fleet_root(&data, "lap").unwrap();
        assert_eq!(root, std::fs::canonicalize(data.join("fleet/lap")).unwrap());
        assert!(root.is_dir());
        assert_eq!(peer_fleet_root(&data, "lap").unwrap(), root, "again: the same");
        assert!(peer_fleet_root(&data, "..").is_err());
        assert!(peer_fleet_root(&data, "a/b").is_err());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn draining_refuses_new_work_only() {
        let lending = peer(Path::new("/fleet/hub"));
        let e = refuse_new_work("run", &lending, true).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), ("denied", DRAINING));
        for m in ["start", "stop", "rm", "rebuild", "services.rebuild", "capacity.get", "instances.list"] {
            assert!(refuse_new_work(m, &lending, true).is_ok(), "{m}");
        }
        assert!(refuse_new_work("run", &lending, false).is_ok());
        // `drain` is the lending link's: a peer without a fleet root still runs.
        let ssh = Peer { name: "lap".into(), fleet_root: None, env: None, agent: None };
        assert!(refuse_new_work("run", &ssh, true).is_ok());
    }

    #[test]
    fn paths_are_confined_to_the_fleet_root() {
        let base = scratch("paths");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg/sub")).unwrap();
        std::fs::create_dir_all(base.join("mine")).unwrap();
        std::os::unix::fs::symlink(base.join("mine"), root.join("escape")).unwrap();
        std::os::unix::fs::symlink(root.join("cfg"), base.join("into")).unwrap();

        let reach = fleet(&root);
        assert_eq!(confine_path(&reach, &root.join("cfg"), true).unwrap(), root.join("cfg"));
        assert_eq!(confine_path(&reach, &root, true).unwrap(), root, "the root itself");
        // A symlink from outside into the root resolves inside: fine.
        assert_eq!(confine_path(&reach, &base.join("into/sub"), true).unwrap(), root.join("cfg/sub"));
        let code = |p: &Path, exists| confine_path(&reach, p, exists).unwrap_err().code;
        assert_eq!(code(&base.join("mine"), true), "denied", "outside");
        assert_eq!(code(&root.join("escape"), true), "denied", "a symlink out");
        assert_eq!(code(&root.join("cfg/../../../mine"), true), "denied", "`..`");
        assert_eq!(code(&root.join("cfg/.."), true), "denied", "`..` even when it stays inside");
        assert_eq!(code(Path::new("rel"), true), "invalid");
        assert_eq!(code(&root.join("gone"), true), "not-found");
        assert_eq!(confine_path(&reach, &root.join("gone"), false).unwrap(), root.join("gone"), "an unwatch");
        assert_eq!(code(&base.join("gone"), false), "denied");
        // A sibling whose name extends the root's isn't under it.
        std::fs::create_dir_all(base.join("fleet/hub2")).unwrap();
        assert_eq!(code(&base.join("fleet/hub2"), true), "denied");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `state.toml` with `in` under the fleet root and `own` outside.
    fn state_with(base: &Path, root: &Path) -> PathBuf {
        let state = base.join("state.toml");
        let inst = |name: &str, dir: &Path| {
            format!(
                "[instance.{name}]\ninstance_id = \"id-{name}\"\nsandbox = \"web\"\ncontainer = \"devsandbox-{name}\"\n\
                 folder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                dir.to_str().unwrap()
            )
        };
        std::fs::write(&state, inst("in", &root.join("cfg")) + &inst("own", &base.join("mine"))).unwrap();
        state
    }

    /// The lending link's peer: this host's env.
    fn peer(root: &Path) -> Peer {
        Peer {
            name: "me@hub".into(),
            fleet_root: Some(root.to_path_buf()),
            env: Some(BTreeMap::from([("PATH".into(), "/worker/bin".into())])),
            agent: None,
        }
    }

    #[test]
    fn admit_confines_paths_instances_and_containers() {
        let base = scratch("admit");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(base.join("mine")).unwrap();
        let state = state_with(&base, &root);
        let (s, r) = (peer(&root), fleet(&root));
        let ok = |m: &str, p: Value| admit(m, p, &s, &r, &state).unwrap();
        let code = |m: &str, p: Value| admit(m, p, &s, &r, &state).unwrap_err().code;
        let admit = |m: &str, p: Value, s: &Peer, state: &Path| admit(m, p, s, &r, state);

        let cfg = root.join("cfg");
        assert_eq!(ok("instances.list", json!({"dir": cfg})), json!({"dir": cfg}));
        assert_eq!(code("instances.list", json!({"dir": base.join("mine")})), "denied");
        assert_eq!(code("instances.list", json!({})), "invalid", "a path is needed");
        assert_eq!(code("sandboxes.list", json!({"root": 3})), "invalid");
        assert_eq!(code("config.findings", json!({"root": base.join("mine")})), "denied");

        assert!(admit("instances.done", json!({"instance": "in", "done": true}), &s, &state).is_ok());
        assert!(admit("instances.buildLog", json!({"instance": "id-in"}), &s, &state).is_ok(), "by id");
        assert_eq!(code("instances.done", json!({"instance": "own", "done": true})), "not-found");
        assert_eq!(code("instances.buildLog", json!({"instance": "id-own"})), "not-found");
        assert_eq!(code("instances.signal", json!({"instance": "own", "pid": "1", "signal": "TERM"})), "not-found");
        assert!(admit("instances.procs", json!({"instances": ["in"]}), &s, &state).is_ok());
        assert_eq!(code("instances.procs", json!({"instances": ["in", "own"]})), "not-found");
        assert!(admit("containers.logs", json!({"container": "devsandbox-in"}), &s, &state).is_ok());
        assert_eq!(code("containers.inspect", json!({"container": "devsandbox-own"})), "not-found");
        // A reached root's service containers, global or isolated; not another root's.
        let svc = |dir: &Path, rest: &str| {
            crate::commands::services::service_container(&crate::commands::services::project_id(dir).unwrap(), rest)
        };
        assert!(admit("containers.logs", json!({"container": svc(&cfg, "db")}), &s, &state).is_ok());
        assert!(admit("containers.inspect", json!({"container": svc(&cfg, "id-in-db")}), &s, &state).is_ok());
        assert_eq!(code("containers.logs", json!({"container": svc(&base.join("mine"), "db")})), "not-found");

        // `all`: only while every instance in `state.toml` is reached.
        let e = admit("stop", json!({"root": cfg, "all": true}), &s, &state).unwrap_err();
        assert_eq!(e.code, "denied");
        assert!(e.message.contains("machine trust full"), "{}", e.message);

        // Sessions: the root confined and needed, the instance lent.
        assert_eq!(ok("sessions.list", json!({"root": cfg, "instance": "id-in"})), json!({"root": cfg, "instance": "id-in"}));
        assert!(admit("sessions.rm", json!({"root": cfg, "instance": "in", "id": "1700000000-ab12"}), &s, &state).is_ok());
        assert_eq!(code("sessions.list", json!({"root": cfg, "instance": "own"})), "not-found");
        assert_eq!(code("sessions.rm", json!({"root": base.join("mine"), "instance": "own", "id": "x"})), "denied");
        assert_eq!(code("sessions.list", json!({"instance": "in"})), "invalid", "a root is needed");

        // Verbs: the root confined, the instance resolved (a sandbox name
        // matching both is refused), and named.
        assert_eq!(code("stop", json!({"root": base.join("mine"), "instance": "in"})), "denied");
        assert_eq!(code("stop", json!({"root": cfg, "instance": "own"})), "not-found");
        assert_eq!(code("stop", json!({"root": cfg, "instance": "web"})), "not-found", "matches `own` too");
        assert_eq!(code("rm", json!({"root": cfg})), "invalid");
        assert_eq!(code("start", json!({"root": cfg, "instance": null})), "invalid");
        assert!(admit("run", json!({"root": cfg, "sandbox": "web", "env": {}}), &s, &state).is_ok(), "run names none");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_verbs_env_is_this_hosts() {
        let base = scratch("env");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        let state = state_with(&base, &root);
        let p = json!({"root": root.join("cfg"), "instance": "in", "env": {"PATH": "/hub/bin", "SSH_AUTH_SOCK": "/hub/agent"}});
        let r = fleet(&root);
        let got = admit("stop", p.clone(), &peer(&root), &r, &state).unwrap();
        assert_eq!(got["env"], json!({"PATH": "/worker/bin"}));
        // Missing too: the verb gets it all the same.
        let got = admit("run", json!({"root": root.join("cfg"), "sandbox": "web"}), &peer(&root), &r, &state).unwrap();
        assert_eq!(got["env"], json!({"PATH": "/worker/bin"}));
        // Not a verb: params untouched.
        let got = admit("sandboxes.list", json!({"root": root.join("cfg"), "env": {"A": "b"}}), &peer(&root), &r, &state).unwrap();
        assert_eq!(got["env"], json!({"A": "b"}));
        // A peer without one: the verb's as sent, under either trust.
        let as_sent = Peer { env: None, ..peer(&root) };
        assert_eq!(admit("stop", p.clone(), &as_sent, &r, &state).unwrap()["env"], p["env"]);
        let full = Reach { trust: Trust::Full, ..r.clone() };
        assert_eq!(admit("stop", p.clone(), &as_sent, &full, &state).unwrap()["env"], p["env"]);
        // `full` still takes the peer's.
        assert_eq!(admit("stop", p.clone(), &peer(&root), &full, &state).unwrap()["env"], json!({"PATH": "/worker/bin"}));
        // A relayed agent: the verb's `SSH_AUTH_SOCK` while it answers.
        let sock = base.join("agent-link-lap.sock");
        let linked = Peer { agent: Some(sock.clone()), ..peer(&root) };
        assert_eq!(admit("stop", p.clone(), &linked, &r, &state).unwrap()["env"], json!({"PATH": "/worker/bin"}), "not bound");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let want = json!({"PATH": "/worker/bin", "SSH_AUTH_SOCK": sock});
        assert_eq!(admit("stop", p, &linked, &r, &state).unwrap()["env"], want);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Own roots are reached exactly, beside the fleet root.
    #[test]
    fn worker_admits_own_roots_exactly() {
        let base = scratch("own");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(base.join("mine/nested")).unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        let state = state_with(&base, &root);
        let (s, r) = (peer(&root), worker(&root, &[&base.join("mine")]));
        let ok = |m: &str, p: Value| admit(m, p, &s, &r, &state).unwrap();
        let code = |m: &str, p: Value| admit(m, p, &s, &r, &state).unwrap_err().code;
        let mine = base.join("mine");
        assert_eq!(ok("sandboxes.list", json!({"root": mine})), json!({"root": mine}), "an own root");
        assert_eq!(code("sandboxes.list", json!({"root": mine.join("nested")})), "denied", "not what's nested in one");
        assert_eq!(code("sandboxes.list", json!({"root": base.join("other")})), "denied", "nor another path");
        assert_eq!(code("sandboxes.list", json!({"root": mine.join("nested/..")})), "denied", "`..`");
        // Every instance reached: `all` goes (`own` here, `in` under the fleet root).
        assert!(admit("start", json!({"root": mine, "all": true}), &s, &r, &state).is_ok());
        // A peer without a fleet root: `in`'s root is reached as an own root
        // (it's in `state.toml`), and so is `all`.
        let ssh = Reach { fleet_root: None, ..r.clone() };
        let own = own_roots(&State::load_at(&state).unwrap(), None);
        let ssh = Reach { own: ssh.own.union(&own).cloned().collect(), ..ssh };
        assert!(admit("stop", json!({"root": root.join("cfg"), "all": true}), &s, &ssh, &state).is_ok());
        // Without a `Peer.env` its verbs keep the env they sent.
        let attached = Peer { name: "lap".into(), fleet_root: None, env: None, agent: None };
        let sent = json!({"root": mine, "instance": "own", "env": {"SSH_AUTH_SOCK": "/run/agent-machine_lap.sock"}});
        assert_eq!(admit("stop", sent.clone(), &attached, &ssh, &state).unwrap(), sent);
        assert_eq!(admit("config.migrate", json!({"root": mine}), &s, &ssh, &state).unwrap_err().code, "denied");
        // Its instances are named, verbs reach them.
        assert!(admit("instances.done", json!({"instance": "own", "done": true}), &s, &r, &state).is_ok());
        assert!(admit("stop", json!({"root": mine, "instance": "web"}), &s, &r, &state).is_ok(), "both matches reached");
        // Edits of a config: under the fleet root only.
        for m in ["config.migrate", "config.ack"] {
            let e = admit(m, json!({"root": mine}), &s, &r, &state).unwrap_err();
            assert_eq!(e.code, "denied", "{m}");
            assert!(e.message.contains("machine trust full"), "{}", e.message);
            assert!(admit(m, json!({"root": root.join("cfg")}), &s, &r, &state).is_ok(), "{m} under the fleet root");
        }
        assert!(admit("config.findings", json!({"root": mine}), &s, &r, &state).is_ok(), "reading is fine");
        assert_eq!(ok("roots.list", json!({"paths": ["/anywhere"]})), json!({"paths": ["/anywhere"]}));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `full`: nothing confined, but `member.*`, `link.*` and `links.*` stay local.
    #[test]
    fn full_skips_the_table_but_not_member_or_link() {
        let base = scratch("full");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(&root).unwrap();
        let state = base.join("absent.toml");
        let (s, r) = (peer(&root), Reach { trust: Trust::Full, ..fleet(&root) });
        for (m, p) in [
            ("sandboxes.list", json!({"root": "/anywhere/at/all"})),
            ("config.migrate", json!({"root": "/etc"})),
            ("machines.list", json!({})),
            ("shutdown", json!({})),
            ("instances.done", json!({"instance": "nobody"})),
            ("stop", json!({"all": true})),
            ("instances.list", json!({"machine": "m"})),
            ("gc", json!({"root": "/anywhere", "force": true})),
        ] {
            let as_sent = Peer { env: None, ..s.clone() };
            assert_eq!(admit(m, p.clone(), &as_sent, &r, &state).unwrap(), p, "{m}: as sent");
        }
        // `gc`: a verb, so the peer's env; under `worker` refused whatever the root.
        let gc = json!({"root": root, "env": {"PATH": "/hub/bin"}});
        assert_eq!(admit("gc", gc.clone(), &s, &r, &state).unwrap()["env"], json!({"PATH": "/worker/bin"}));
        let e = admit("gc", gc, &s, &fleet(&root), &state).unwrap_err();
        assert_eq!(e.code, "denied");
        assert!(e.message.contains("host maintenance"), "{}", e.message);
        for m in ["member.get", "member.set", "link.attach", "link.anything", "links.inbound"] {
            for reach in [&r, &fleet(&root)] {
                assert_eq!(admit(m, json!({}), &s, reach, &state).unwrap_err().code, "denied", "{m}");
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_reach_comes_from_member_toml_and_state_toml() {
        let base = scratch("load");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(base.join("mine")).unwrap();
        std::fs::create_dir_all(base.join("picked")).unwrap();
        std::fs::create_dir_all(base.join("home/.devsandboxes")).unwrap();
        let state = state_with(&base, &root);
        let files = Files { state, member: Some(base.join("member.toml")), home: Some(base.join("home")) };
        let (r, warn) = Reach::load(&files, Some(&root));
        assert_eq!(warn, None);
        assert_eq!(r.trust, Trust::Worker, "no member.toml");
        let own = |p: &[&str]| p.iter().map(|p| base.join(p)).collect::<BTreeSet<_>>();
        assert_eq!(r.own, [root.join("cfg")].into_iter().chain(own(&["mine", "home/.devsandboxes"])).collect());
        member::write_for_test(
            files.member.as_ref().unwrap(),
            &Member { trust: Trust::Full, roots: vec![base.join("picked"), base.join("gone")], ..Default::default() },
        );
        let (r, warn) = Reach::load(&files, Some(&root));
        assert_eq!((r.trust, warn), (Trust::Full, None));
        assert!(r.own.is_superset(&own(&["picked", "gone"])), "picked ones, a missing one as written");
        // A member.toml that doesn't load: `worker`, no picked roots, said.
        std::fs::write(files.member.as_ref().unwrap(), "trust = 1\n").unwrap();
        let (r, warn) = Reach::load(&files, Some(&root));
        assert_eq!(r.trust, Trust::Worker);
        assert!(!r.own.contains(&base.join("picked")));
        assert!(warn.unwrap().contains("trust = \"worker\""));
        // No data dir: `worker`.
        let (r, _) = Reach::load(&Files { member: None, ..files }, None);
        assert_eq!((r.trust, r.fleet_root), (Trust::Worker, None));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rows_are_cut_to_the_fleet_root() {
        let root = Path::new("/nonexistent/fleet/hub");
        let mut snap = json!({
            "instances": [
                {"name": "in", "root": "/nonexistent/fleet/hub/cfg", "instance_id": "i1"},
                {"name": "own", "root": "/nonexistent/mine", "instance_id": "o1"},
                {"name": "sneaky", "root": "/nonexistent/fleet/hub/../../mine"},
                {"name": "old", "root": null},
            ],
            "services": [
                {"name": "db", "used_by": ["in", "own"]},
                {"name": "cache", "scope": "isolated", "used_by": ["in", "own"], "containers": [
                    ["devsandbox-svc-p-i1-cache", {"Running": "Up"}], ["devsandbox-svc-p-o1-cache", {"Running": "Up"}]]},
                {"name": "pg", "scope": "global", "used_by": ["in"], "containers": [["devsandbox-svc-p-pg", {"Running": "Up"}]]},
            ],
            "sandboxes": [{"name": "web"}],
            "thread_children": {"id-own": {"k": "own"}, "id-in": {"k": "in", "j": "own"}},
        });
        let full = Reach { trust: Trust::Full, ..fleet(root) };
        let mut whole = snap.clone();
        filter_snapshot(&mut whole, &full);
        assert_eq!(whole, snap, "nothing cut under `full`");
        let mut own = snap.clone();
        filter_snapshot(&mut own, &worker(root, &[Path::new("/nonexistent/mine")]));
        let names: Vec<&str> = own["instances"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["in", "own"], "an own root's rows kept, the others dropped");
        assert_eq!(own["thread_children"], json!({"id-own": {"k": "own"}, "id-in": {"k": "in", "j": "own"}}));
        let root = &fleet(root);
        filter_snapshot(&mut snap, root);
        assert_eq!(snap["instances"], json!([{"name": "in", "root": "/nonexistent/fleet/hub/cfg", "instance_id": "i1"}]));
        assert_eq!(snap["services"][0]["used_by"], json!(["in"]));
        assert_eq!(snap["services"][1]["containers"], json!([["devsandbox-svc-p-i1-cache", {"Running": "Up"}]]), "another root's isolated copy cut");
        assert_eq!(snap["services"][2]["containers"].as_array().unwrap().len(), 1, "a global one kept");
        assert_eq!(snap["thread_children"], json!({"id-in": {"k": "in"}}));
        assert_eq!(snap["sandboxes"], json!([{"name": "web"}]), "the root's own config, as is");

        let resolved = json!({"matches": ["in", "own"], "found": [
            {"name": "in", "root": "/nonexistent/fleet/hub/cfg"}, {"name": "own", "root": "/nonexistent/mine"}]});
        let got = filter_answer("instances.resolve", resolved, root, Path::new("/nonexistent/state.toml")).unwrap();
        assert_eq!(got, json!({"matches": ["in"], "found": [{"name": "in", "root": "/nonexistent/fleet/hub/cfg"}]}));
        let own = json!({"matches": ["own"], "found": [{"name": "own", "root": "/nonexistent/mine"}]});
        assert_eq!(filter_answer("instances.resolve", own, root, Path::new("/nonexistent/state.toml")).unwrap_err().code, "not-found");
        assert_eq!(filter_answer("sandboxes.list", json!({"a": 1}), root, Path::new("/nonexistent/state.toml")).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn container_rows_are_cut_to_lent_instances_and_their_services() {
        let containers = BTreeSet::from(["devsandbox-in".to_string()]);
        let projects = BTreeSet::from(["aaaa1111".to_string()]);
        let svc = |name: &str, project: &str| json!({"name": name, "labels": {"devsandbox.project": project}});
        let mut list = json!({"rows": [
            {"name": "devsandbox-in", "labels": {}},
            {"name": "devsandbox-own", "labels": {}},
            svc("devsandbox-svc-aaaa1111-db", "aaaa1111"),
            svc("devsandbox-svc-bbbb2222-db", "bbbb2222"),
            // Named like a lent service, labelled otherwise: the label decides.
            svc("devsandbox-svc-aaaa1111-x", "bbbb2222"),
            {"name": "devsandbox-svc-aaaa1111-nolabels"},
            {"labels": {"devsandbox.project": "aaaa1111"}},
        ]});
        cut_containers("containers.list", &mut list, &containers, &projects);
        let names: Vec<&str> = list["rows"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["devsandbox-in", "devsandbox-svc-aaaa1111-db"]);

        let row = |name: &str| json!({"name": name, "cpu": "1%", "mem": "2MiB"});
        let mut stats = json!({"rows": [row("devsandbox-in"), row("devsandbox-own"), row("devsandbox-svc-aaaa1111-in-db"),
            row("devsandbox-svc-bbbb2222-db"), row("devsandbox-svc-aaaa11112-db")]});
        cut_containers("containers.stats", &mut stats, &containers, &projects);
        assert_eq!(stats["rows"], json!([row("devsandbox-in"), row("devsandbox-svc-aaaa1111-in-db")]));
    }

    /// The projects come from the lent instances' config roots in `state.toml`.
    #[test]
    fn container_answers_are_cut_by_the_state() {
        let base = scratch("containers");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(base.join("mine")).unwrap();
        let state = state_with(&base, &root);
        let lent = crate::commands::services::project_id(&root.join("cfg")).unwrap();
        let mine = crate::commands::services::project_id(&base.join("mine")).unwrap();
        let svc = |p: &str| json!({"name": format!("devsandbox-svc-{p}-db"), "labels": {"devsandbox.project": p}});
        let answer = json!({"rows": [{"name": "devsandbox-in", "labels": {}}, {"name": "devsandbox-own", "labels": {}}, svc(&lent), svc(&mine)]});
        let with_own = filter_answer("containers.list", answer.clone(), &worker(&root, &[&base.join("mine")]), &state).unwrap();
        assert_eq!(with_own, answer, "an own root's instance and services kept");
        let reach = fleet(&root);
        let root = &reach;
        let got = filter_answer("containers.list", answer, root, &state).unwrap();
        assert_eq!(got, json!({"rows": [{"name": "devsandbox-in", "labels": {}}, svc(&lent)]}));
        let stats = json!({"rows": [{"name": "devsandbox-own"}, {"name": format!("devsandbox-svc-{lent}-db")}]});
        let got = filter_answer("containers.stats", stats, root, &state).unwrap();
        assert_eq!(got, json!({"rows": [{"name": format!("devsandbox-svc-{lent}-db")}]}));
        assert_eq!(filter_answer("containers.list", json!({"rows": []}), root, &base.join("bad")).unwrap(), json!({"rows": []}), "no state: none lent");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_peers_roots_listing_gets_its_fleet_root_once() {
        let reach = worker(Path::new("/d/fleet/hub"), &[]);
        let own = json!([{"root": "/own", "sandboxes": [{"name": "web"}], "error": null}]);
        let got = with_fleet_root(own.clone(), &reach);
        assert_eq!(got, json!([own[0], {"root": "/d/fleet/hub", "sandboxes": [], "error": null, "fleet": true}]));
        assert_eq!(with_fleet_root(got.clone(), &reach), got, "listed already");
        // Listed among the own roots already: flagged there.
        let listed = json!([{"root": "/d/fleet/hub", "sandboxes": [], "error": null, "fleet": false}]);
        assert_eq!(with_fleet_root(listed, &reach), json!([{"root": "/d/fleet/hub", "sandboxes": [], "error": null, "fleet": true}]));
        let relay = Reach { fleet_root: None, ..reach.clone() };
        assert_eq!(with_fleet_root(own.clone(), &relay), own, "no fleet root: as is");
        assert_eq!(with_fleet_root(json!({"x": 1}), &reach), json!({"x": 1}), "not a listing");
    }
}
