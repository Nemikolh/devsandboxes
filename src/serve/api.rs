//! The daemon's API methods (docs/api.md): the method table and one thin
//! handler per method, each a call into `inbox::ops`, `snapshot`, or the
//! daemon's bridges and forwards (behind [`Bridging`] / [`Forwarding`]). Params
//! parse through serde structs; results are explicit wire views
//! ([`ThreadSummary`], [`ThreadDetail`]), never the store model, whose JSON
//! shape is internal (and carries what the wire must not, e.g. the client
//! behind each user item).
//!
//! No sockets here: [`call`] is a function of the method, its params and a
//! [`Ctx`] (the store path), so every handler is tested against a temp store.
//! Connection-level methods (`hello`, `subscribe`, `unsubscribe`,
//! `instances.watch`, `instances.unwatch`) live in `daemon.rs`, which owns
//! the connection; [`topics`] and [`watch_dir`] parse theirs.
//!
//! Mutations check and apply in one locked write (`ops::apply_if`), so a
//! thread removed between the check and the write can't turn `not-found`
//! into a silent no-op.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::devsbd::bridge::Readiness;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::forwards::{AddRequest, Added, ForwardRow, Removed};
use crate::inbox::wire::{self, Addr, resolve};
use crate::inbox::{Inbox, Kind, Op, View, ops};
// The wire views and errors live in `inbox::wire` (the CLI shares them off
// unix too); this module serves them.
pub use crate::inbox::wire::{ApiError, ThreadDetail};

/// What a handler needs from the daemon. The store path is injectable so
/// tests (and a test daemon) use their own file.
pub struct Ctx<'a> {
    pub inbox: &'a Path,
    /// Who's calling, as the store records it on the user's feed items
    /// ([`audit_client`] of the hello's `client`). Never shown to owners.
    pub client: &'a str,
    /// The bridges `bridges.ensure` acts on; `None` on a daemon that runs no
    /// host side (tests).
    pub bridges: Option<&'a dyn Bridging>,
    /// The port forwards `forwards.*` act on; `None` like `bridges`.
    pub forwards: Option<&'a dyn Forwarding>,
}

/// The daemon's forward registry as `forwards.*` sees it
/// (`forwards::Handle` in production), so the handlers are tested with a
/// fake. Params are validated before these are called.
pub trait Forwarding {
    /// Every forward, or those of config root `dir` (absolute).
    fn list(&self, dir: Option<PathBuf>) -> Result<Vec<ForwardRow>, ApiError>;
    /// Start an ad-hoc forward: `not-found` (instance, dir), `bind-failed`.
    fn add(&self, req: AddRequest) -> Result<Added, ApiError>;
    /// Stop forward `id`: `not-found`.
    fn rm(&self, id: u64) -> Result<Removed, ApiError>;
}

/// The daemon's bridges as `bridges.ensure` sees them (`host::Waker` in
/// production), so the handler is tested without state or a runtime.
pub trait Bridging {
    /// The container of instance `instance` (a `state.toml` key), `None`
    /// when there's no such instance.
    fn container(&self, instance: &str) -> anyhow::Result<Option<String>>;
    /// A client's host agent socket, now the preferred candidate
    /// (`devsbd::bridge::AgentCandidates`).
    fn report_agent(&self, path: PathBuf);
    /// Reconcile `container`'s bridge now; doesn't wait for it. Returns the
    /// request's ticket, for [`wait_ready`](Self::wait_ready).
    fn reconcile_now(&self, container: String) -> u64;
    /// Block up to `timeout` for `container`'s bridge as of `ticket`.
    fn wait_ready(&self, container: &str, ticket: u64, timeout: Duration) -> Readiness;
}

/// The longest `bridges.ensure` `wait`: a bit over the bridge's own 10 s
/// handshake timeout, so a handshake that times out still reports why.
pub const MAX_ENSURE_WAIT: Duration = Duration::from_secs(15);


type Answer = Result<Value, ApiError>;

/// Run API method `method`. Unknown methods answer `unknown-method`.
pub fn call(method: &str, params: Value, ctx: &Ctx) -> Answer {
    match method {
        "inbox.threads.list" => threads_list(parse(params)?, ctx),
        "inbox.thread.get" => thread_get(parse(params)?, ctx),
        "inbox.thread.markRead" => {
            let addr: Addr = parse(params)?;
            mutate(ctx, |inbox| Ok(vec![Op::MarkRead(resolve(inbox, &addr)?.id)]))
        }
        "inbox.thread.act" => act(parse(params)?, ctx),
        "inbox.thread.reply" => reply(parse(params)?, ctx),
        "inbox.thread.done" => {
            let addr: Addr = parse(params)?;
            mutate(ctx, |inbox| wire::decide_done(inbox, &addr))
        }
        "inbox.thread.reopen" => {
            let addr: Addr = parse(params)?;
            mutate(ctx, |inbox| wire::decide_reopen(inbox, &addr))
        }
        "inbox.form.saveDraft" => form_save_draft(parse(params)?, ctx),
        "inbox.form.submit" => form_submit(parse(params)?, ctx),
        "inbox.notify.dismiss" => dismiss(parse(params)?, ctx),
        "inbox.notify.markRead" => notify_mark_read(parse(params)?, ctx),
        "instances.list" => instances_list(parse(params)?, &state_path()?),
        "instances.resolve" => instances_resolve(parse(params)?, &state_path()?),
        "instances.cancel" => {
            instances_cancel(parse(params)?, &state_path()?, crate::commands::run::cancel::interrupt)
        }
        "instances.procs" => instances_procs(parse(params)?, &state_path()?, fetch_procs),
        "instances.signal" => instances_signal(parse(params)?, &state_path()?, |c, pid, n| {
            crate::runtime::backend().signal_proc(c, pid, n)
        }),
        "instances.rename" => instances_rename(parse(params)?, &state_path()?),
        "instances.done" => instances_done(parse(params)?, &state_path()?),
        "instances.buildLog" => instances_build_log(parse(params)?, &state_path()?),
        "containers.logs" => {
            containers_logs(parse(params)?, |c, n| crate::runtime::backend().logs_tail(c, n).map_err(ApiError::internal))
        }
        "containers.inspect" => {
            containers_inspect(parse(params)?, |c| crate::runtime::backend().inspect_json(c).map_err(ApiError::internal))
        }
        "sandboxes.list" => sandboxes_list(parse(params)?),
        "sandboxes.get" => sandboxes_get(parse(params)?),
        "services.get" => services_get(parse(params)?),
        "bridges.ensure" => bridges_ensure(parse(params)?, ctx),
        "forwards.list" => forwards_list(parse(params)?, ctx),
        "forwards.add" => forwards_add(parse(params)?, ctx),
        "forwards.rm" => forwards_rm(parse(params)?, ctx),
        other => Err(ApiError::unknown_method(other)),
    }
}

/// Params into `T`; absent params count as `{}`. Unknown fields are
/// ignored, so a newer client's additive fields don't break an older daemon.
pub(crate) fn parse<T: DeserializeOwned>(params: Value) -> Result<T, ApiError> {
    let params = if params.is_null() { Value::Object(Default::default()) } else { params };
    serde_json::from_value(params).map_err(|e| ApiError::invalid(format!("bad params: {e}")))
}

/// A notification topic for `subscribe` / `unsubscribe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Topic {
    /// `inbox.changed`: the store changed, re-fetch.
    Inbox,
    /// `instances.changed`: the running containers or `state.toml` changed,
    /// re-fetch.
    Instances,
    /// `forwards.changed`: the forwards' rows changed, re-fetch; and
    /// `forwards.status` lines.
    Forwards,
}

impl Topic {
    pub fn parse(s: &str) -> Option<Topic> {
        match s {
            "inbox" => Some(Topic::Inbox),
            "instances" => Some(Topic::Instances),
            "forwards" => Some(Topic::Forwards),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct TopicsParams {
    topics: Vec<String>,
}

/// `subscribe` / `unsubscribe` params: `{"topics":[…]}`, every one known.
pub fn topics(params: Value) -> Result<Vec<Topic>, ApiError> {
    let p: TopicsParams = parse(params)?;
    p.topics
        .iter()
        .map(|t| Topic::parse(t).ok_or_else(|| ApiError::invalid(format!("unknown topic `{t}` (inbox, instances, forwards)"))))
        .collect()
}

#[derive(Deserialize)]
struct WatchParams {
    dir: PathBuf,
}

/// `instances.watch` / `instances.unwatch` params: `{"dir": …}`, checked
/// like `instances.list`'s (absolute, an existing directory). Unwatching
/// (`!exists`) only needs it absolute: a root deleted meanwhile can still be
/// let go.
pub fn watch_dir(params: Value, exists: bool) -> Result<PathBuf, ApiError> {
    let p: WatchParams = parse(params)?;
    if !p.dir.is_absolute() {
        return Err(ApiError::invalid(format!("`dir` must be absolute, got `{}`", p.dir.display())));
    }
    if exists && !p.dir.is_dir() {
        return Err(ApiError::not_found(format!("no directory {}", p.dir.display())));
    }
    Ok(p.dir)
}

/// The client name the store records for a connection whose hello said
/// `hello`: the dashboard (`tui`) and the CLI (`cli`) as themselves, every
/// other client as `api:<name>` (an `api:` prefix isn't doubled).
pub fn audit_client(hello: &str) -> String {
    match hello {
        "tui" | "cli" => hello.to_string(),
        "" => "api".to_string(),
        name if name.starts_with("api:") => name.to_string(),
        name => format!("api:{name}"),
    }
}

/// `forwards.rm`'s answer.
#[derive(Debug, Serialize)]
pub struct ForwardRemoved {
    pub ok: bool,
    /// The host address it was bound to.
    pub local: String,
    /// It was a configured forward: stopped until its owner runs again.
    pub configured: bool,
}

/// `instances.resolve`'s answer.
#[derive(Debug, Serialize)]
pub struct Resolved {
    /// Every instance the name could mean, sorted: one, or the choices.
    pub matches: Vec<String>,
}

/// `instances.cancel`'s answer.
#[derive(Debug, Serialize)]
pub struct Cancelled {
    pub ok: bool,
    /// The pid SIGINTed.
    pub pid: u32,
}

/// One `sandboxes.list` entry. An object, not a name, so fields can be added.
#[derive(Debug, Serialize)]
pub struct SandboxEntry {
    pub name: String,
}

// ---- handlers -------------------------------------------------------------

fn ok() -> Answer {
    Ok(json!({ "ok": true }))
}

/// Check and apply in one store write; see the module doc.
fn mutate(ctx: &Ctx, decide: impl FnOnce(&Inbox) -> Result<Vec<Op>, ApiError>) -> Answer {
    ops::apply_if(ctx.inbox, ctx.client, decide).map_err(ApiError::internal)??;
    ok()
}

fn load(ctx: &Ctx) -> Result<Inbox, ApiError> {
    ops::load(ctx.inbox).map_err(ApiError::internal)
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    view: Option<String>,
}

/// Threads in the view (default `all`), last change first, like the
/// dashboard's list.
fn threads_list(p: ListParams, ctx: &Ctx) -> Answer {
    let view = match p.view.as_deref() {
        None => View::All,
        Some(name) => View::parse(name).ok_or_else(|| {
            let names: Vec<&str> = View::ALL.iter().map(|v| v.as_str()).collect();
            ApiError::invalid(format!("unknown view `{name}` ({})", names.join(", ")))
        })?,
    };
    let inbox = load(ctx)?;
    serde_json::to_value(wire::summaries(&inbox, view)).map_err(|e| ApiError::internal(e.into()))
}

fn thread_get(addr: Addr, ctx: &Ctx) -> Answer {
    let inbox = load(ctx)?;
    let t = resolve(&inbox, &addr)?;
    serde_json::to_value(ThreadDetail::of(t)).map_err(|e| ApiError::internal(e.into()))
}

#[derive(Deserialize)]
struct ActParams {
    #[serde(flatten)]
    addr: Addr,
    action: String,
}

/// A dispatcher action's event half ([`wire::decide_act`]).
fn act(p: ActParams, ctx: &Ctx) -> Answer {
    mutate(ctx, |inbox| wire::decide_act(inbox, &p.addr, &p.action))
}

#[derive(Deserialize)]
struct ReplyParams {
    #[serde(flatten)]
    addr: Addr,
    text: String,
}

fn reply(p: ReplyParams, ctx: &Ctx) -> Answer {
    mutate(ctx, |inbox| wire::decide_reply(inbox, &p.addr, &p.text))
}

#[derive(Deserialize)]
struct FormParams {
    #[serde(flatten)]
    addr: Addr,
    /// The message holding the form (the owner's id for it).
    message: String,
    /// Answers by question id: a subset for a draft, any subset for a
    /// submission (the draft and the defaults fill in the rest).
    #[serde(default)]
    answers: serde_json::Map<String, Value>,
}

/// Save a partial set of answers as the form's draft (no event).
fn form_save_draft(p: FormParams, ctx: &Ctx) -> Answer {
    mutate(ctx, |inbox| wire::decide_save_draft(inbox, &p.addr, &p.message, &p.answers))
}

/// Submit the form: the owner gets one `submit` event with every answer.
fn form_submit(p: FormParams, ctx: &Ctx) -> Answer {
    mutate(ctx, |inbox| wire::decide_submit(inbox, &p.addr, &p.message, &p.answers))
}

#[derive(Deserialize)]
struct DismissParams {
    #[serde(default)]
    all: bool,
    #[serde(flatten)]
    addr: Addr,
}

/// Dismiss one notify thread, or every one (`all`). Dispatcher threads are
/// state their owner re-asserts, so they can't be dismissed.
fn dismiss(p: DismissParams, ctx: &Ctx) -> Answer {
    match (p.all, p.addr.is_empty()) {
        (true, true) => return mutate(ctx, |_| Ok(vec![Op::ClearNotify])),
        (true, false) => return Err(ApiError::invalid("`all` takes no thread")),
        (false, _) => {}
    }
    mutate(ctx, |inbox| {
        let t = resolve(inbox, &p.addr)?;
        if t.kind != Kind::Notify {
            return Err(ApiError::invalid(format!("thread {} is an owner thread; only notifications are dismissed", t.id)));
        }
        Ok(vec![Op::RemoveThread(t.id)])
    })
}

/// Mark one notify thread read, or every one (`all`): what the dashboard
/// does with the notifications it showed when the user leaves the Inbox.
fn notify_mark_read(p: DismissParams, ctx: &Ctx) -> Answer {
    match (p.all, p.addr.is_empty()) {
        (true, true) => return mutate(ctx, |_| Ok(vec![Op::MarkNotifyRead])),
        (true, false) => return Err(ApiError::invalid("`all` takes no thread")),
        (false, _) => {}
    }
    mutate(ctx, |inbox| {
        let t = resolve(inbox, &p.addr)?;
        if t.kind != Kind::Notify {
            return Err(ApiError::invalid(format!("thread {} is an owner thread, not a notification", t.id)));
        }
        Ok(vec![Op::MarkRead(t.id)])
    })
}

#[derive(Deserialize)]
struct InstancesParams {
    dir: PathBuf,
    /// `disk` | `listing` | `full`; absent is `full`.
    #[serde(default)]
    depth: Option<String>,
}

/// `instances.list`'s `depth` as a [`Depth`](crate::snapshot::Depth):
/// how much of the runtime to ask (absent: everything, stats included).
fn depth(name: Option<&str>) -> Result<crate::snapshot::Depth, ApiError> {
    use crate::snapshot::Depth;
    match name {
        None | Some("full") => Ok(Depth::Full),
        Some("listing") => Ok(Depth::Listing),
        Some("disk") => Ok(Depth::Disk),
        Some(other) => Err(ApiError::invalid(format!("unknown depth `{other}` (disk, listing, full)"))),
    }
}

/// `instances.list`'s answer: the snapshot's fields at top level, plus the
/// dispatcher children the Inbox thread pane links to (owner `instance_id`
/// → key → child instance name, `dispatch::thread_children`). Not part of
/// `status --json`. Read back by the dashboard (`tui::poll`).
#[derive(Debug, Serialize, Deserialize)]
pub struct InstancesList {
    #[serde(flatten)]
    pub snapshot: crate::snapshot::Snapshot,
    #[serde(default)]
    pub thread_children: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

/// The `status --json` snapshot (without its envelope: the API versions
/// itself through `hello`) for config root `dir`, down to `depth`, plus
/// `thread_children` from `state` (injected for the tests; the snapshot
/// itself reads the user's state, as `status` does). Absolute only: the
/// daemon's cwd is `/`, not the client's.
fn instances_list(p: InstancesParams, state: &Path) -> Answer {
    instances_list_with(p, state, crate::snapshot::collect_with)
}

/// [`instances_list`] with the collection injected, so the tests never ask
/// the runtime or read the user's state.
fn instances_list_with(
    p: InstancesParams,
    state: &Path,
    collect: impl FnOnce(&Path, crate::snapshot::Depth) -> crate::snapshot::Snapshot,
) -> Answer {
    if !p.dir.is_absolute() {
        return Err(ApiError::invalid(format!("`dir` must be absolute, got `{}`", p.dir.display())));
    }
    let depth = depth(p.depth.as_deref())?;
    if !p.dir.is_dir() {
        return Err(ApiError::not_found(format!("no directory {}", p.dir.display())));
    }
    let list = InstancesList::of(collect(&p.dir, depth), state);
    serde_json::to_value(list).map_err(|e| ApiError::internal(e.into()))
}

impl InstancesList {
    /// `snapshot` plus `thread_children` from `state`: what `instances.list`
    /// answers and `instances.snapshot` pushes.
    pub(crate) fn of(snapshot: crate::snapshot::Snapshot, state: &Path) -> Self {
        // An unreadable state resolves no child, like the dashboard's own
        // collection; the snapshot's `error` already says why.
        let thread_children = crate::state::State::load_at(state)
            .map(|s| crate::commands::dispatch::thread_children(&s))
            .unwrap_or_default();
        InstancesList { snapshot, thread_children }
    }
}

/// The user's `state.toml`, as the instance methods read it.
fn state_path() -> Result<PathBuf, ApiError> {
    crate::state::State::path().map_err(ApiError::internal)
}

fn load_state(path: &Path) -> Result<crate::state::State, ApiError> {
    crate::state::State::load_at(path).map_err(ApiError::internal)
}

#[derive(Deserialize)]
struct NameParams {
    name: String,
}

/// The instances `name` could mean, in the CLI's tiers
/// (`commands::instance_matches`): a client picks among several itself, so
/// ambiguity isn't an error here. `state` is injected for the tests.
fn instances_resolve(p: NameParams, state: &Path) -> Answer {
    let matches = crate::commands::instance_matches(&load_state(state)?, &p.name);
    if matches.is_empty() {
        return Err(ApiError::not_found(crate::commands::no_instance_matches(&p.name)));
    }
    serde_json::to_value(Resolved { matches }).map_err(|e| ApiError::internal(e.into()))
}

/// `pub(crate)` for the dashboard's test that its call parses.
#[derive(Deserialize)]
pub(crate) struct CancelParams {
    pub(crate) instance: String,
}

/// SIGINT an instance's in-flight `run`/`rebuild` by its recorded pid
/// (`cancel::cancel_instance`), whoever started it: the CLI's Ctrl-C on a
/// second connection, the dashboard, a verb another daemon left running.
/// `state` and `signal` are injected for the tests.
fn instances_cancel(p: CancelParams, state: &Path, signal: fn(u32) -> Result<(), String>) -> Answer {
    use crate::commands::run::cancel::{NotCancelled, cancel_instance};
    let pid = cancel_instance(&load_state(state)?, &p.instance, signal).map_err(|e| match e {
        NotCancelled::Unknown => ApiError::not_found(format!("no instance `{}`", p.instance)),
        NotCancelled::Done => ApiError::invalid(format!("`{}` is not building any more", p.instance)),
        NotCancelled::Signal(e) => ApiError::internal(anyhow::anyhow!(e)),
    })?;
    serde_json::to_value(Cancelled { ok: true, pid }).map_err(|e| ApiError::internal(e.into()))
}

#[derive(Deserialize)]
struct SignalParams {
    instance: String,
    pid: String,
    signal: String,
}

/// The signals `instances.signal` sends, by wire name: the dashboard's kill
/// keys. Numbers, as the runtime's `kill -<n>` in the container takes them.
fn signal_number(name: &str) -> Option<i32> {
    match name {
        "TERM" => Some(15),
        "KILL" => Some(9),
        _ => None,
    }
}

/// Signal process `pid` inside an instance's container (`instances.procs`
/// pids, when `signalable`). The pid goes into the runtime's `kill` argv,
/// so it must be a plain positive number: no `-1`, no `0` (the exec's own
/// group). `state` and `send` (the runtime's `signal_proc`) are injected
/// for the tests.
fn instances_signal(
    p: SignalParams,
    state: &Path,
    send: impl FnOnce(&str, &str, i32) -> anyhow::Result<()>,
) -> Answer {
    let num = signal_number(&p.signal)
        .ok_or_else(|| ApiError::invalid(format!("unknown signal `{}` (TERM, KILL)", p.signal)))?;
    if p.pid.is_empty() || !p.pid.bytes().all(|b| b.is_ascii_digit()) || p.pid.bytes().all(|b| b == b'0') {
        return Err(ApiError::invalid(format!("`pid` must be a process id, got `{}`", p.pid)));
    }
    let s = load_state(state)?;
    let instance =
        s.instances.get(&p.instance).ok_or_else(|| ApiError::not_found(format!("no instance `{}`", p.instance)))?;
    send(&instance.container, &p.pid, num).map_err(ApiError::internal)?;
    ok()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameParams {
    instance: String,
    new_name: String,
}

/// `instances.rename`'s answer: the new (trimmed) name.
#[derive(Debug, Serialize, Deserialize)]
pub struct Renamed {
    pub instance: String,
}

/// Rename an instance by its exact name (`rename::rename_exact`: only the
/// state key moves). Refusals keep their CLI wording: `not-found` for no
/// such instance, `invalid` for an empty / spaced / unchanged / taken name
/// or an instance mid-`run`/`rebuild`. `state` is injected for the tests.
fn instances_rename(p: RenameParams, state: &Path) -> Answer {
    use crate::commands::rename::{Refused, rename_exact_at};
    let instance = rename_exact_at(state, &p.instance, &p.new_name).map_err(|e| match e.downcast::<Refused>() {
        Ok(Refused::Unknown(m)) => ApiError::not_found(m),
        Ok(Refused::Invalid(m)) => ApiError::invalid(m),
        Err(e) => ApiError::internal(e),
    })?;
    serde_json::to_value(Renamed { instance }).map_err(|e| ApiError::internal(e.into()))
}

#[derive(Deserialize)]
struct DoneParams {
    instance: String,
    done: bool,
}

/// `instances.done`'s answer: the instance's flag as it now is.
#[derive(Debug, Serialize, Deserialize)]
pub struct DoneSet {
    pub instance: String,
    pub done: bool,
}

/// Set or clear an instance's done flag (`done::set_saved`) by its exact
/// name; idempotent, as the CLI's. The answer is data: the client words the
/// status line. `state` is injected for the tests.
fn instances_done(p: DoneParams, state: &Path) -> Answer {
    use crate::commands::done::{UnknownInstance, set_saved_at};
    set_saved_at(state, &p.instance, p.done).map_err(|e| match e.downcast::<UnknownInstance>() {
        Ok(unknown) => ApiError::not_found(unknown.to_string()),
        Err(e) => ApiError::internal(e),
    })?;
    serde_json::to_value(DoneSet { instance: p.instance, done: p.done }).map_err(|e| ApiError::internal(e.into()))
}

#[derive(Deserialize)]
struct SandboxesParams {
    root: PathBuf,
}

/// The sandbox names of config root `root`, sorted. No config file is no
/// sandboxes (the CLI then offers the example config); one that doesn't
/// load is `invalid`, with why.
fn sandboxes_list(p: SandboxesParams) -> Answer {
    let rows: Vec<SandboxEntry> = match config_at(&p.root)? {
        None => Vec::new(),
        // A BTreeMap: already sorted.
        Some(config) => config.sandboxes.into_keys().map(|name| SandboxEntry { name }).collect(),
    };
    serde_json::to_value(rows).map_err(|e| ApiError::internal(e.into()))
}

/// Config root `root`'s config, `None` when it has no config file. Errors:
/// `invalid` (relative root, a config that doesn't load), `not-found` (no
/// such directory).
fn config_at(root: &Path) -> Result<Option<crate::config::Config>, ApiError> {
    if !root.is_absolute() {
        return Err(ApiError::invalid(format!("`root` must be absolute, got `{}`", root.display())));
    }
    if !root.is_dir() {
        return Err(ApiError::not_found(format!("no directory {}", root.display())));
    }
    match crate::config::find(root) {
        Ok(None) => Ok(None),
        Ok(Some(_)) => crate::config::Config::load(root).map(Some).map_err(|e| ApiError::invalid(format!("{e:#}"))),
        Err(e) => Err(ApiError::invalid(format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
struct ConfigEntryParams {
    root: PathBuf,
    name: String,
}

/// Sandbox `name` of config root `root` as TOML text: as written, merged,
/// and the merged table's hash. One that doesn't merge (a bad `extends`)
/// still answers, with why in `resolved` and no hash, as the view shows it.
/// The answer is a [`SandboxText`](crate::config::SandboxText).
fn sandboxes_get(p: ConfigEntryParams) -> Answer {
    let config = config_at(&p.root)?.unwrap_or_default();
    if !config.sandboxes.contains_key(&p.name) {
        return Err(ApiError::not_found(format!("no sandbox `{}` in {}", p.name, p.root.display())));
    }
    serde_json::to_value(config.sandbox_text(&p.name)).map_err(|e| ApiError::internal(e.into()))
}

/// `services.get`'s answer. Services don't `extends`: as written is resolved.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub original: String,
}

fn services_get(p: ConfigEntryParams) -> Answer {
    let config = config_at(&p.root)?.unwrap_or_default();
    let original = config
        .service_text(&p.name)
        .ok_or_else(|| ApiError::not_found(format!("no service `{}` in {}", p.name, p.root.display())))?;
    serde_json::to_value(ServiceConfig { original }).map_err(|e| ApiError::internal(e.into()))
}

/// Most lines `containers.logs` / `instances.buildLog` answer; more is capped.
pub const MAX_LINES: usize = 10_000;

/// Lines answered when a request names none: what the dashboard's modals show.
const DEFAULT_LOG_LINES: usize = 50;
const DEFAULT_BUILD_LOG_LINES: usize = 500;

#[derive(Deserialize)]
struct ProcsParams {
    instances: Vec<String>,
}

/// One process of an `instances.procs` listing. Strings as `ps` printed
/// them: the runtime's `top` fallback isn't guaranteed numeric.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: String,
    pub ppid: String,
    pub args: String,
}

/// A running instance's processes and its helper's runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcsListing {
    pub processes: Vec<Process>,
    /// The pids are container-namespace (`exec ps`), so `instances.signal`
    /// can target them; false for the runtime's host-side `top` fallback.
    pub signalable: bool,
    /// Oldest first; none when the helper is missing or predates runs.
    pub runs: Vec<crate::runtime::procs::Run>,
}

/// One instance's entry in `instances.procs`: its listing, or why there's
/// none (no such instance, not running, the listing failed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InstanceProcs {
    Listed(ProcsListing),
    Failed { error: String },
}

/// `instances.procs`'s answer, keyed by instance name.
pub type ProcsAnswer = std::collections::BTreeMap<String, InstanceProcs>;

/// Each named instance's processes, one entry per name: a per-instance
/// failure is its entry's `error`, never the call's, so one stopped
/// instance doesn't hide the others. Containers from `state` (injected for
/// the tests, as is `fetch`, the per-container listing).
fn instances_procs(
    p: ProcsParams,
    state: &Path,
    fetch: impl Fn(&str) -> Result<ProcsListing, String>,
) -> Answer {
    let state = load_state(state)?;
    let answer: ProcsAnswer = p
        .instances
        .into_iter()
        .map(|name| {
            let entry = match state.instances.get(&name) {
                None => Err(format!("no instance `{name}`")),
                Some(instance) => fetch(&instance.container),
            };
            let entry = entry.map_or_else(|error| InstanceProcs::Failed { error }, InstanceProcs::Listed);
            (name, entry)
        })
        .collect();
    serde_json::to_value(answer).map_err(|e| ApiError::internal(e.into()))
}

/// `container`'s listing from the runtime: `ps` in it (or `top` outside),
/// then its helper's runs. A stopped container says so rather than
/// quoting the runtime's exec error.
fn fetch_procs(container: &str) -> Result<ProcsListing, String> {
    use crate::runtime::procs::{parse_runs, parse_top, run_ls};
    let backend = crate::runtime::backend();
    if let Ok(Some(false)) = backend.is_running(container) {
        return Err("not running".to_string());
    }
    let list = backend.proc_list(container).map_err(|e| format!("{e:#}"))?;
    let processes =
        parse_top(&list.text).into_iter().map(|(pid, ppid, args)| Process { pid, ppid, args }).collect();
    let runs = run_ls(container).map(|text| parse_runs(&text)).unwrap_or_default();
    Ok(ProcsListing { processes, signalable: list.container_pids, runs })
}

#[derive(Deserialize)]
struct BuildLogParams {
    instance: String,
    #[serde(default)]
    lines: Option<usize>,
    /// A pty log's lines with SGR escapes (colours, bold…), else plain.
    #[serde(default)]
    ansi: bool,
}

/// The end of an instance's build log (`build_log::tail_in`): `{text, cut}`,
/// or `null` when it has none yet. `instance` is an exact name or
/// instance id in `state` (injected for the tests; the logs live next to
/// it); a cancelled `run` drops its record, and its log is `not-found` then.
fn instances_build_log(p: BuildLogParams, state: &Path) -> Answer {
    let s = load_state(state)?;
    let id = match s.instances.get(&p.instance) {
        Some(instance) => &instance.instance_id,
        None => s
            .instances
            .values()
            .map(|i| &i.instance_id)
            .find(|id| **id == p.instance)
            .ok_or_else(|| ApiError::not_found(format!("no instance `{}`", p.instance)))?,
    };
    let lines = p.lines.unwrap_or(DEFAULT_BUILD_LOG_LINES).min(MAX_LINES);
    let data = state.parent().unwrap_or(Path::new("."));
    let tail = crate::build_log::tail_in(data, id, lines, p.ansi).map_err(ApiError::internal)?;
    serde_json::to_value(tail).map_err(|e| ApiError::internal(e.into()))
}

#[derive(Deserialize)]
struct ContainerParams {
    container: String,
    #[serde(default)]
    lines: Option<usize>,
}

/// `containers.*` act on devsandbox's own containers only, so the API is
/// no generic runtime proxy.
fn own_container(container: &str) -> Result<(), ApiError> {
    if container.starts_with(crate::runtime::NAME_PREFIX) {
        Ok(())
    } else {
        let prefix = crate::runtime::NAME_PREFIX;
        Err(ApiError::invalid(format!("`{container}` is not a devsandbox container (`{prefix}…`)")))
    }
}

/// `containers.logs`'s answer.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContainerLogs {
    pub text: String,
}

/// The last `lines` lines of a container's output. `logs` is the runtime
/// (injected for the tests); its errors are `internal` with the runtime's
/// message, a missing container included: the runtimes don't tell "no
/// such container" apart from a runtime that's down reliably enough to
/// answer `not-found`.
fn containers_logs(p: ContainerParams, logs: impl FnOnce(&str, usize) -> Result<String, ApiError>) -> Answer {
    own_container(&p.container)?;
    let lines = p.lines.unwrap_or(DEFAULT_LOG_LINES).min(MAX_LINES);
    let text = logs(&p.container, lines)?;
    serde_json::to_value(ContainerLogs { text }).map_err(|e| ApiError::internal(e.into()))
}

/// The runtime's inspect JSON for a container, parsed (docker's is an
/// array of one object; Apple's `container` its own shape). Output that
/// isn't JSON is `internal`; runtime errors as [`containers_logs`].
fn containers_inspect(p: ContainerParams, inspect: impl FnOnce(&str) -> Result<String, ApiError>) -> Answer {
    own_container(&p.container)?;
    let raw = inspect(&p.container)?;
    serde_json::from_str(&raw)
        .map_err(|e| ApiError::internal(anyhow::anyhow!("`{}`'s inspect output isn't JSON: {e}", p.container)))
}

#[derive(Deserialize)]
struct EnsureParams {
    instance: String,
    #[serde(default)]
    agent: Option<PathBuf>,
    /// Seconds to wait for the bridge's handshake (capped at
    /// [`MAX_ENSURE_WAIT`]); absent: don't wait.
    #[serde(default)]
    wait: Option<f64>,
}

/// Record the caller's agent (if any), then have the daemon reconcile the
/// instance's bridge now. Without `wait` it doesn't wait for the bridge: the
/// in-container daemon holds agent clients briefly until one attaches
/// (docs/sandbox-helper.md). With it (lifecycle commands, which may `git
/// clone` over ssh at once), it answers once the bridge's handshake is
/// decided or the wait ran out: `ready`, and `error` saying why not. The
/// params are checked first, so a bad request records nothing.
fn bridges_ensure(p: EnsureParams, ctx: &Ctx) -> Answer {
    let Some(bridges) = ctx.bridges else {
        return Err(ApiError::internal(anyhow::anyhow!("this daemon runs no bridges")));
    };
    if let Some(agent) = &p.agent {
        if !agent.is_absolute() {
            return Err(ApiError::invalid(format!("`agent` must be an absolute path, got `{}`", agent.display())));
        }
    }
    let wait = match p.wait {
        None => None,
        Some(secs) if secs.is_finite() && secs >= 0.0 => {
            Some(Duration::from_secs_f64(secs.min(MAX_ENSURE_WAIT.as_secs_f64())))
        }
        Some(secs) => return Err(ApiError::invalid(format!("`wait` must be seconds >= 0, got {secs}"))),
    };
    let container = bridges
        .container(&p.instance)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("no instance `{}`", p.instance)))?;
    if let Some(agent) = p.agent {
        bridges.report_agent(agent);
    }
    let ticket = bridges.reconcile_now(container.clone());
    let Some(wait) = wait else { return ok() };
    let r = bridges.wait_ready(&container, ticket, wait);
    Ok(json!({ "ok": true, "ready": r.ready, "error": r.error }))
}

fn forwarding<'a>(ctx: &Ctx<'a>) -> Result<&'a dyn Forwarding, ApiError> {
    ctx.forwards.ok_or_else(|| ApiError::internal(anyhow::anyhow!("this daemon runs no forwards")))
}

fn absolute_dir(dir: &Path) -> Result<(), ApiError> {
    if dir.is_absolute() {
        Ok(())
    } else {
        Err(ApiError::invalid(format!("`dir` must be absolute, got `{}`", dir.display())))
    }
}

#[derive(Deserialize)]
struct ForwardsListParams {
    #[serde(default)]
    dir: Option<PathBuf>,
}

fn forwards_list(p: ForwardsListParams, ctx: &Ctx) -> Answer {
    let forwards = forwarding(ctx)?;
    if let Some(dir) = &p.dir {
        absolute_dir(dir)?;
    }
    serde_json::to_value(forwards.list(p.dir)?).map_err(|e| ApiError::internal(e.into()))
}

#[derive(Deserialize)]
struct ForwardsAddParams {
    dir: PathBuf,
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    address: Option<String>,
    spec: String,
}

/// `forwards.add`'s params, checked: everything that needs no state or
/// socket. Empty `instance` / `service` count as absent.
fn add_request(p: ForwardsAddParams) -> Result<AddRequest, ApiError> {
    absolute_dir(&p.dir)?;
    let instance = p.instance.filter(|s| !s.is_empty());
    let service = p.service.filter(|s| !s.is_empty());
    if instance.is_none() && service.is_none() {
        return Err(ApiError::invalid("name an `instance`, a `service`, or both"));
    }
    let (host_port, container_port) = crate::commands::port::parse_port_spec(&p.spec).map_err(ApiError::invalid)?;
    let bind = match &p.address {
        Some(a) => a.parse().map_err(|_| ApiError::invalid(format!("bad address `{a}`")))?,
        None => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    Ok(AddRequest { dir: p.dir, instance, service, bind, host_port, container_port })
}

fn forwards_add(p: ForwardsAddParams, ctx: &Ctx) -> Answer {
    let forwards = forwarding(ctx)?;
    let added = forwards.add(add_request(p)?)?;
    serde_json::to_value(added).map_err(|e| ApiError::internal(e.into()))
}

#[derive(Deserialize)]
struct ForwardsRmParams {
    id: u64,
}

fn forwards_rm(p: ForwardsRmParams, ctx: &Ctx) -> Answer {
    let removed = forwarding(ctx)?.rm(p.id)?;
    let view = ForwardRemoved { ok: true, local: removed.local, configured: removed.configured };
    serde_json::to_value(view).map_err(|e| ApiError::internal(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::notify::{Level, Record};
    use crate::inbox::form;
    use crate::inbox::thread::HostVerb;
    use crate::inbox::wire::*;
    use crate::inbox::{Block, FormRecord};
    use crate::inbox::{store, Action, Compose, State, ThreadPut};

    fn store_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-api-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("inbox.json")
    }

    fn action(id: &str) -> Action {
        Action { id: id.into(), label: id.to_uppercase(), ..Action::default() }
    }

    fn put(path: &Path, owner: &str, name: &str, key: &str, state: State) {
        let put = ThreadPut {
            key: key.into(),
            title: format!("{key} title"),
            state,
            compose: Some(Compose::default()),
            actions: vec![action("go")],
            ..ThreadPut::default()
        };
        store::update_at(path, |i| i.put(owner, name, 10, put)).unwrap();
    }

    fn notify(path: &Path, owner: &str, msg: &str) {
        let record = Record { level: Level::Warn, key: None, link: None, msg: msg.into(), at: 5 };
        store::update_at(path, |i| i.push(owner.into(), owner.into(), record, true)).unwrap();
    }

    fn run(path: &Path, method: &str, params: Value) -> Answer {
        call(method, params, &Ctx { inbox: path, client: "api:test", bridges: None, forwards: None })
    }

    /// Records what `bridges.ensure` asked for; knows instance `web` only.
    /// Waits answer `readiness` at once, recording `(ticket, timeout)`.
    #[derive(Default)]
    struct FakeBridges {
        agents: std::cell::RefCell<Vec<PathBuf>>,
        woken: std::cell::RefCell<Vec<String>>,
        waits: std::cell::RefCell<Vec<(u64, Duration)>>,
        readiness: Option<Readiness>,
    }

    impl Bridging for FakeBridges {
        fn container(&self, instance: &str) -> anyhow::Result<Option<String>> {
            match instance {
                "web" => Ok(Some("devsandbox-web".into())),
                "broken" => anyhow::bail!("state unreadable"),
                _ => Ok(None),
            }
        }
        fn report_agent(&self, path: PathBuf) {
            self.agents.borrow_mut().push(path);
        }
        fn reconcile_now(&self, container: String) -> u64 {
            self.woken.borrow_mut().push(container);
            self.woken.borrow().len() as u64
        }
        fn wait_ready(&self, container: &str, ticket: u64, timeout: Duration) -> Readiness {
            assert_eq!(container, "devsandbox-web");
            self.waits.borrow_mut().push((ticket, timeout));
            self.readiness.clone().unwrap()
        }
    }

    #[test]
    fn bridges_ensure_waits_only_when_asked_and_caps_the_wait() {
        let path = store_path("ensure-wait");
        let not = Readiness { ready: false, error: Some("helper in devsandbox-web is outdated".into()) };
        let fake = FakeBridges { readiness: Some(not), ..FakeBridges::default() };
        let ensure = |params: Value| call("bridges.ensure", params, &Ctx { inbox: &path, client: "api:test", bridges: Some(&fake), forwards: None });

        assert_eq!(ensure(json!({"instance": "web"})).unwrap(), json!({"ok": true}), "no wait: unchanged");
        assert!(fake.waits.borrow().is_empty());
        assert_eq!(
            ensure(json!({"instance": "web", "wait": 10})).unwrap(),
            json!({"ok": true, "ready": false, "error": "helper in devsandbox-web is outdated"})
        );
        ensure(json!({"instance": "web", "wait": 0.5})).unwrap();
        ensure(json!({"instance": "web", "wait": 3600})).unwrap();
        // Each wait is on its own request's ticket, capped.
        assert_eq!(
            *fake.waits.borrow(),
            [(2, Duration::from_secs(10)), (3, Duration::from_millis(500)), (4, MAX_ENSURE_WAIT)]
        );
        assert_eq!(code(ensure(json!({"instance": "web", "wait": -1}))), "invalid");
        assert_eq!(code(ensure(json!({"instance": "web", "wait": "soon"}))), "invalid");
        assert_eq!(fake.woken.borrow().len(), 4, "a bad wait wakes nothing");

        let ready = FakeBridges { readiness: Some(Readiness { ready: true, error: None }), ..FakeBridges::default() };
        let r = call("bridges.ensure", json!({"instance": "web", "wait": 10}), &Ctx { inbox: &path, client: "api:test", bridges: Some(&ready), forwards: None });
        assert_eq!(r.unwrap(), json!({"ok": true, "ready": true, "error": null}));
    }

    #[test]
    fn bridges_ensure_records_the_agent_and_reconciles_the_instance() {
        let path = store_path("ensure");
        let fake = FakeBridges::default();
        let ensure = |params: Value| call("bridges.ensure", params, &Ctx { inbox: &path, client: "api:test", bridges: Some(&fake), forwards: None });

        assert_eq!(ensure(json!({"instance": "web", "agent": "/tmp/ssh-x/agent.1"})).unwrap(), json!({"ok": true}));
        assert_eq!(ensure(json!({"instance": "web", "agent": null})).unwrap(), json!({"ok": true}));
        assert_eq!(ensure(json!({"instance": "web"})).unwrap(), json!({"ok": true}));
        assert_eq!(*fake.agents.borrow(), [PathBuf::from("/tmp/ssh-x/agent.1")]);
        assert_eq!(*fake.woken.borrow(), ["devsandbox-web"; 3]);

        // Refused before anything is recorded or woken.
        assert_eq!(code(ensure(json!({"instance": "nope", "agent": "/a"}))), "not-found");
        assert_eq!(code(ensure(json!({"instance": "web", "agent": "rel/agent"}))), "invalid");
        assert_eq!(code(ensure(json!({"instance": "web", "agent": ""}))), "invalid");
        assert_eq!(code(ensure(json!({"agent": "/a"}))), "invalid");
        assert_eq!(code(ensure(json!({"instance": "broken"}))), "internal");
        assert_eq!(fake.agents.borrow().len(), 1);
        assert_eq!(fake.woken.borrow().len(), 3);

        // A daemon without a host side says so.
        assert_eq!(code(run(&path, "bridges.ensure", json!({"instance": "web"}))), "internal");
    }

    fn code(r: Answer) -> &'static str {
        r.unwrap_err().code
    }

    fn id_of(path: &Path, owner: &str, key: &str) -> u64 {
        ops::thread_id(&ops::load(path).unwrap(), owner, key).unwrap()
    }

    #[test]
    fn list_filters_like_the_dashboard_views() {
        let path = store_path("views");
        put(&path, "d-id", "d", "asks", State::NeedsYou);
        put(&path, "d-id", "d", "busy", State::Active);
        put(&path, "d-id", "d", "over", State::Done);
        put(&path, "gone-id", "gone", "old", State::Active);
        store::update_at(&path, |i| i.archive_owner("gone-id")).unwrap();
        notify(&path, "w-id", "built\nsecond line");
        let inbox = ops::load(&path).unwrap();
        for view in View::ALL {
            let r = run(&path, "inbox.threads.list", json!({ "view": view.as_str() })).unwrap();
            let mut got: Vec<u64> = r.as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect();
            let mut want: Vec<u64> = inbox.threads.iter().filter(|t| view.shows(t)).map(|t| t.id).collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "{view:?}");
        }
        // Default view is all.
        let all = run(&path, "inbox.threads.list", Value::Null).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 5);
        let needs = run(&path, "inbox.threads.list", json!({"view":"needs-you"})).unwrap();
        let titles: Vec<&str> = needs.as_array().unwrap().iter().map(|t| t["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["asks title", "built"], "notify title is its first line; newest change first");
        assert_eq!(code(run(&path, "inbox.threads.list", json!({"view":"nope"}))), "invalid");
        assert_eq!(code(run(&path, "inbox.threads.list", json!({"view":3}))), "invalid");
    }

    #[test]
    fn summary_wire_shape() {
        let path = store_path("shape");
        put(&path, "d-id", "d", "pr-1", State::NeedsYou);
        let r = run(&path, "inbox.threads.list", json!({})).unwrap();
        let t = &r[0];
        assert_eq!(t["owner"], "d-id");
        assert_eq!(t["owner_name"], "d");
        assert_eq!(t["key"], "pr-1");
        assert_eq!(t["kind"], "thread");
        assert_eq!(t["state"], "needs-you");
        assert_eq!(t["status"], Value::Null);
        assert_eq!(t["unread"], true);
        assert_eq!(t["archived"], false);
        assert_eq!(t["needs_you"], true);
        assert_eq!(t["changed_at"], 10);
        assert!(t["id"].is_u64());
    }

    #[test]
    fn get_addresses_by_id_or_owner_and_key() {
        let path = store_path("get");
        put(&path, "d-id", "d", "pr-1", State::NeedsYou);
        notify(&path, "w-id", "hello");
        let id = id_of(&path, "d-id", "pr-1");

        let by_id = run(&path, "inbox.thread.get", json!({ "thread": id })).unwrap();
        let by_owner_id = run(&path, "inbox.thread.get", json!({"owner":"d-id","key":"pr-1"})).unwrap();
        let by_name = run(&path, "inbox.thread.get", json!({"owner":"d","key":"pr-1"})).unwrap();
        assert_eq!(by_id, by_owner_id);
        assert_eq!(by_id, by_name);
        assert_eq!(by_id["title"], "pr-1 title");
        assert_eq!(by_id["actions"][0]["id"], "go");
        assert_eq!(by_id["actions"][0]["sends_event"], true);
        assert_eq!(by_id["actions"][0]["host"], Value::Null);
        assert_eq!(by_id["compose"], json!({"placeholder": null, "hint": null}));
        assert_eq!(by_id["feed"], json!([]), "a new thread's feed is empty");
        assert_eq!(by_id["events_pending"], 0);
        assert_eq!(by_id["notes"], json!([]));

        let n = ops::load(&path).unwrap().threads.iter().find(|t| t.kind == Kind::Notify).unwrap().id;
        let note = run(&path, "inbox.thread.get", json!({ "thread": n })).unwrap();
        assert_eq!((note["kind"].as_str(), note["level"].as_str()), (Some("notify"), Some("warn")));
        assert_eq!(note["notes"][0]["msg"], "hello");

        assert_eq!(code(run(&path, "inbox.thread.get", json!({"thread": 999}))), "not-found");
        assert_eq!(code(run(&path, "inbox.thread.get", json!({"owner":"d","key":"pr-9"}))), "not-found");
        assert_eq!(code(run(&path, "inbox.thread.get", json!({"owner":"d"}))), "invalid");
        assert_eq!(code(run(&path, "inbox.thread.get", json!({"thread": id, "owner":"d","key":"pr-1"}))), "invalid");
        assert_eq!(code(run(&path, "inbox.thread.get", json!({}))), "invalid");
        assert_eq!(code(run(&path, "inbox.thread.get", json!({"thread": "x"}))), "invalid");
    }

    /// The detail's feed on the wire: every item kind, tagged by `type`,
    /// and never the client that made a user item.
    #[test]
    fn detail_feed_wire_shape() {
        let path = store_path("feed");
        let header = |state: State, status: &str| ThreadPut {
            key: "pr-1".into(),
            title: "t".into(),
            state,
            status: Some(status.into()),
            compose: Some(Compose { placeholder: Some("p".into()), hint: Some("h".into()) }),
            actions: vec![action("go"), Action { done: true, ..action("fin") }],
            ..ThreadPut::default()
        };
        let send = |text: &str| crate::inbox::MessageSend {
            thread: "pr-1".into(),
            id: "m".into(),
            blocks: vec![Block::Markdown { text: text.into() }],
        };
        store::update_at(&path, |i| {
            i.put("d-id", "d", 10, header(State::Active, "a"));
            i.send("d-id", 10, send("hello"));
        })
        .unwrap();
        let id = id_of(&path, "d-id", "pr-1");
        run(&path, "inbox.thread.act", json!({"thread": id, "action": "go"})).unwrap();
        run(&path, "inbox.thread.reply", json!({"thread": id, "text": "hi"})).unwrap();
        run(&path, "inbox.thread.done", json!({"thread": id})).unwrap();
        run(&path, "inbox.thread.reopen", json!({"thread": id})).unwrap();
        store::update_at(&path, |i| {
            i.put("d-id", "d", 20, header(State::NeedsYou, "b"));
            i.send("d-id", 20, send("hello again"));
        })
        .unwrap();

        let t = run(&path, "inbox.thread.get", json!({"thread": id})).unwrap();
        assert_eq!(t["compose"], json!({"placeholder": "p", "hint": "h"}));
        let mut feed = t["feed"].clone();
        for item in feed.as_array_mut().unwrap() {
            assert!(item["seq"].is_u64(), "{item}");
            assert!(item["at"].is_u64(), "{item}");
            let item = item.as_object_mut().unwrap();
            item.remove("seq");
            item.remove("at");
        }
        assert_eq!(
            feed,
            json!([
                {"type": "message", "id": "m", "blocks": [{"type": "markdown", "text": "hello again"}], "edited": true, "withdrawn": false},
                {"type": "action", "action": "go", "label": "GO"},
                {"type": "reply", "text": "hi"},
                {"type": "marker", "marker": "done", "from": null, "to": null},
                {"type": "marker", "marker": "reopen", "from": null, "to": null},
                {"type": "marker", "marker": "state", "from": "active", "to": "needs-you"},
                {"type": "marker", "marker": "status", "from": "a", "to": "b"},
            ])
        );
        let wire = t.to_string();
        assert!(!wire.contains("client") && !wire.contains("api:test"), "{wire}");
        // The store does know who did it.
        let stored = ops::load(&path).unwrap().to_json().unwrap();
        assert!(stored.contains("api:test"));
    }

    #[test]
    fn audit_client_names() {
        assert_eq!(audit_client("tui"), "tui");
        assert_eq!(audit_client("cli"), "cli");
        assert_eq!(audit_client("npm:my-ext"), "api:npm:my-ext");
        assert_eq!(audit_client("api:x"), "api:x");
        assert_eq!(audit_client(""), "api");
    }

    #[test]
    fn owner_name_prefers_the_live_thread() {
        let path = store_path("names");
        put(&path, "old-id", "d", "pr-1", State::Active);
        store::update_at(&path, |i| i.archive_owner("old-id")).unwrap();
        put(&path, "new-id", "d", "pr-1", State::NeedsYou);
        let t = run(&path, "inbox.thread.get", json!({"owner":"d","key":"pr-1"})).unwrap();
        assert_eq!(t["owner"], "new-id");
        // The removed instance's id still reaches its archived thread.
        let t = run(&path, "inbox.thread.get", json!({"owner":"old-id","key":"pr-1"})).unwrap();
        assert_eq!(t["archived"], true);
    }

    #[test]
    fn user_ops_enqueue_events_and_check_their_target() {
        let path = store_path("ops");
        put(&path, "d-id", "d", "pr-1", State::NeedsYou);
        let id = id_of(&path, "d-id", "pr-1");
        let by_key = json!({"owner":"d","key":"pr-1"});

        assert_eq!(run(&path, "inbox.thread.markRead", by_key.clone()).unwrap(), json!({"ok": true}));
        assert!(!ops::load(&path).unwrap().threads[0].unread);

        run(&path, "inbox.thread.act", json!({"thread": id, "action": "go"})).unwrap();
        run(&path, "inbox.thread.reply", json!({"thread": id, "text": " hi "})).unwrap();
        run(&path, "inbox.thread.done", by_key.clone()).unwrap();
        run(&path, "inbox.thread.reopen", json!({ "thread": id })).unwrap();
        let events: Vec<(String, Option<String>, Option<String>)> = ops::events(&path, "d-id")
            .unwrap()
            .into_iter()
            .map(|(_, e)| (e.kind.as_str().to_string(), e.action, e.text))
            .collect();
        assert_eq!(
            events,
            [
                ("action".into(), Some("go".into()), None),
                ("reply".into(), None, Some("hi".into())),
                ("done".into(), None, None),
                ("reopen".into(), None, None),
            ]
        );

        assert_eq!(code(run(&path, "inbox.thread.act", json!({"thread": id, "action": "nope"}))), "not-found");
        assert_eq!(code(run(&path, "inbox.thread.act", json!({ "thread": id }))), "invalid");
        assert_eq!(code(run(&path, "inbox.thread.reply", json!({"thread": id, "text": "  "}))), "invalid");
        assert_eq!(code(run(&path, "inbox.thread.done", json!({"thread": 999}))), "not-found");
        assert_eq!(code(run(&path, "inbox.thread.markRead", json!({"thread": 999}))), "not-found");
        assert_eq!(code(run(&path, "inbox.thread.reopen", json!({"owner":"x","key":"pr-1"}))), "not-found");

        // A notify thread takes no user ops; an archived one is read-only.
        notify(&path, "w-id", "n");
        let n = ops::load(&path).unwrap().threads.iter().find(|t| t.kind == Kind::Notify).unwrap().id;
        assert_eq!(code(run(&path, "inbox.thread.done", json!({ "thread": n }))), "invalid");
        store::update_at(&path, |i| i.archive_owner("d-id")).unwrap();
        assert_eq!(code(run(&path, "inbox.thread.reply", json!({"thread": id, "text": "x"}))), "denied");
        assert_eq!(ops::events(&path, "d-id").unwrap().len(), 4, "refused ops enqueue nothing");
    }

    /// Thread `pr-1` of `d-id` with message `run-1` carrying `mixed()`:
    /// `pick` (required choice a|b), `tags` (optional multiple x|y), `note`
    /// (required text, max 5), `sure` (optional confirm).
    fn with_form(path: &Path) -> u64 {
        put(path, "d-id", "d", "pr-1", State::NeedsYou);
        let blocks = vec![Block::Form(form::tests::mixed())];
        let send = crate::inbox::MessageSend { thread: "pr-1".into(), id: "run-1".into(), blocks };
        store::update_at(path, |i| i.send("d-id", 11, send)).unwrap();
        id_of(path, "d-id", "pr-1")
    }

    #[test]
    fn form_methods_check_before_applying() {
        let path = store_path("forms");
        let id = with_form(&path);
        let call = |method: &str, answers: Value| run(&path, method, json!({"thread": id, "message": "run-1", "answers": answers}));

        // Drafts: partial, merged, no event.
        assert_eq!(call("inbox.form.saveDraft", json!({"pick": "b"})).unwrap(), json!({"ok": true}));
        assert_eq!(call("inbox.form.saveDraft", json!({"tags": ["y"]})).unwrap(), json!({"ok": true}));
        let t = run(&path, "inbox.thread.get", json!({"thread": id})).unwrap();
        let block = &t["feed"][0]["blocks"][0];
        assert_eq!((block["state"].clone(), block["draft"].clone()), (json!("open"), json!({"pick": "b", "tags": ["y"]})));
        assert_eq!(block["answers"], Value::Null);
        for bad in [json!({"nope": "a"}), json!({"pick": "z"}), json!({"pick": ["a"]}), json!({"note": "toolong"}), json!({"sure": "yes"})] {
            assert_eq!(code(call("inbox.form.saveDraft", bad.clone())), "invalid", "{bad}");
        }
        // Submit: required answers checked, listing what's missing.
        let missing = call("inbox.form.submit", json!({})).unwrap_err();
        assert_eq!((missing.code, missing.message.as_str()), ("invalid", "required questions unanswered: note"));
        assert_eq!(code(call("inbox.form.submit", json!({"note": "hi", "tags": "x"}))), "invalid");
        assert!(ops::events(&path, "d-id").unwrap().is_empty(), "refused ops enqueue nothing");
        assert_eq!(call("inbox.form.submit", json!({"note": "hi"})).unwrap(), json!({"ok": true}));
        let events = ops::events(&path, "d-id").unwrap();
        let [(_, e)] = &events[..] else { panic!("{events:?}") };
        assert_eq!(e.answers, Some(json!({"pick": "b", "tags": ["y"], "note": "hi", "sure": null})));

        // Closed now; the view shows the answers and the submission item.
        assert_eq!(code(call("inbox.form.submit", json!({}))), "closed-form");
        assert_eq!(code(call("inbox.form.saveDraft", json!({"note": "x"}))), "closed-form");
        let t = run(&path, "inbox.thread.get", json!({"thread": id})).unwrap();
        let block = &t["feed"][0]["blocks"][0];
        assert_eq!(block["state"], "submitted");
        assert_eq!(block["draft"], json!({}));
        assert_eq!(block["answers"], json!({"pick": "b", "tags": ["y"], "note": "hi", "sure": null}));
        let sub = &t["feed"][1];
        assert_eq!((sub["type"].as_str(), sub["message"].as_str(), sub["form"].as_str()), (Some("submission"), Some("run-1"), Some("f")));
        assert_eq!(sub["answers"], json!({"pick": "b", "tags": ["y"], "note": "hi", "sure": null}));
        assert!(!t.to_string().contains("api:test"), "{t}");

        // Addressing: by owner and key too; unknown message or no form.
        let by_key = json!({"owner": "d", "key": "pr-1", "message": "run-1", "answers": {}});
        assert_eq!(code(run(&path, "inbox.form.submit", by_key)), "closed-form");
        let none = json!({"thread": id, "message": "nope", "answers": {}});
        assert_eq!(code(run(&path, "inbox.form.saveDraft", none)), "not-found");
        assert_eq!(code(run(&path, "inbox.form.submit", json!({"thread": 999, "message": "run-1"}))), "not-found");
        assert_eq!(code(run(&path, "inbox.form.submit", json!({"thread": id}))), "invalid", "no message");
        store::update_at(&path, |i| {
            let send = crate::inbox::MessageSend { thread: "pr-1".into(), id: "plain".into(), blocks: vec![Block::Markdown { text: "x".into() }] };
            i.send("d-id", 12, send)
        })
        .unwrap();
        assert_eq!(code(run(&path, "inbox.form.submit", json!({"thread": id, "message": "plain"}))), "not-found");
        // A withdrawn form is closed too.
        let other = crate::inbox::MessageSend { thread: "pr-1".into(), id: "run-2".into(), blocks: vec![Block::Form(form::tests::mixed())] };
        store::update_at(&path, |i| {
            i.send("d-id", 13, other);
            i.withdraw("d-id", "pr-1", "run-2")
        })
        .unwrap();
        assert_eq!(code(run(&path, "inbox.form.submit", json!({"thread": id, "message": "run-2"}))), "closed-form");
    }

    #[test]
    fn host_only_actions_and_replyless_threads_are_refused() {
        let path = store_path("refused");
        let put = ThreadPut {
            key: "k".into(),
            title: "t".into(),
            actions: vec![Action {
                host: Some(HostVerb::Logs(Default::default())),
                ..action("logs")
            }],
            ..ThreadPut::default()
        };
        store::update_at(&path, |i| i.put("d-id", "d", 1, put)).unwrap();
        let addr = json!({"owner":"d-id","key":"k","action":"logs","text":"hi"});
        let t = run(&path, "inbox.thread.get", addr.clone()).unwrap();
        assert_eq!((t["actions"][0]["host"].as_str(), t["actions"][0]["sends_event"].as_bool()), (Some("logs"), Some(false)));
        assert_eq!(code(run(&path, "inbox.thread.act", addr.clone())), "invalid");
        assert_eq!(code(run(&path, "inbox.thread.reply", addr)), "denied");
        assert!(ops::events(&path, "d-id").unwrap().is_empty());
    }

    #[test]
    fn dismiss_removes_notifications_only() {
        let path = store_path("dismiss");
        put(&path, "d-id", "d", "pr-1", State::Active);
        notify(&path, "w-id", "a");
        notify(&path, "w-id", "b");
        let inbox = ops::load(&path).unwrap();
        let notes: Vec<u64> = inbox.threads.iter().filter(|t| t.kind == Kind::Notify).map(|t| t.id).collect();
        let thread = id_of(&path, "d-id", "pr-1");

        run(&path, "inbox.notify.dismiss", json!({ "thread": notes[0] })).unwrap();
        assert_eq!(ops::load(&path).unwrap().threads.len(), 2);
        assert_eq!(code(run(&path, "inbox.notify.dismiss", json!({ "thread": notes[0] }))), "not-found");
        assert_eq!(code(run(&path, "inbox.notify.dismiss", json!({ "thread": thread }))), "invalid");
        assert_eq!(code(run(&path, "inbox.notify.dismiss", json!({"all": true, "thread": notes[1]}))), "invalid");
        assert_eq!(code(run(&path, "inbox.notify.dismiss", json!({}))), "invalid");
        run(&path, "inbox.notify.dismiss", json!({"all": true})).unwrap();
        let left = ops::load(&path).unwrap();
        assert_eq!(left.threads.len(), 1);
        assert_eq!(left.threads[0].id, thread);
    }

    #[test]
    fn notify_mark_read_reads_notifications_only() {
        let path = store_path("markread");
        put(&path, "d-id", "d", "pr-1", State::NeedsYou);
        notify(&path, "w-id", "a");
        notify(&path, "v-id", "b");
        let unread = |path: &Path| -> Vec<(Kind, bool)> {
            ops::load(path).unwrap().threads.iter().map(|t| (t.kind, t.unread)).collect()
        };
        let thread = id_of(&path, "d-id", "pr-1");
        let notes: Vec<u64> = ops::load(&path).unwrap().threads.iter().filter(|t| t.kind == Kind::Notify).map(|t| t.id).collect();

        run(&path, "inbox.notify.markRead", json!({ "thread": notes[0] })).unwrap();
        assert_eq!(unread(&path).iter().filter(|(_, u)| *u).count(), 2);
        assert_eq!(code(run(&path, "inbox.notify.markRead", json!({ "thread": thread }))), "invalid");
        assert_eq!(code(run(&path, "inbox.notify.markRead", json!({ "thread": 999 }))), "not-found");
        assert_eq!(code(run(&path, "inbox.notify.markRead", json!({"all": true, "thread": notes[1]}))), "invalid");
        assert_eq!(code(run(&path, "inbox.notify.markRead", json!({}))), "invalid");

        assert_eq!(run(&path, "inbox.notify.markRead", json!({"all": true})).unwrap(), json!({"ok": true}));
        // Every notification read; the dispatcher thread is only read by opening it.
        for (kind, unread) in unread(&path) {
            assert_eq!(unread, kind == Kind::Thread, "{kind:?}");
        }
    }

    #[test]
    fn instances_list_checks_its_dir_and_unknown_methods_say_so() {
        let path = store_path("instances");
        assert_eq!(code(run(&path, "instances.list", json!({}))), "invalid");
        assert_eq!(code(run(&path, "instances.list", json!({"dir": "rel/dir"}))), "invalid");
        assert_eq!(code(run(&path, "instances.list", json!({"dir": "/nonexistent/devsandbox-api"}))), "not-found");
        assert_eq!(code(run(&path, "inbox.poll.vote", json!({}))), "unknown-method");
    }

    fn state_file(name: &str, toml: &str) -> PathBuf {
        let path = store_path(name).with_file_name("state.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, toml).unwrap();
        path
    }

    fn instance(key: &str, sandbox: &str, progress: &str) -> String {
        format!(
            "[instance.{key}]\nsandbox = \"{sandbox}\"\ncontainer = \"devsandbox-{key}\"\nfolder = \"/w\"\n\
             workspace = \"/w\"\ncreated_unix = 1\n{progress}"
        )
    }

    fn empty_snapshot() -> crate::snapshot::Snapshot {
        crate::snapshot::Snapshot {
            instances: Vec::new(),
            sandboxes: Vec::new(),
            services: Vec::new(),
            sandbox_count: 0,
            runtime_name: "docker".into(),
            runtime_version: None,
            collected_at: std::time::Instant::now(),
            stats: false,
            error: None,
        }
    }

    #[test]
    fn instances_list_maps_depth_and_adds_thread_children() {
        use crate::snapshot::Depth;
        let state = state_file(
            "list",
            &[instance("d", "disp", ""), instance("web-one", "web", "dispatcher = \"d\"\n"), instance("p", "web", "")].concat(),
        );
        let dir = state.parent().unwrap().to_path_buf();
        let list = |params: Value| {
            let mut asked = None;
            let r = instances_list_with(parse(params)?, &state, |_: &Path, d| {
                asked = Some(d);
                empty_snapshot()
            });
            r.map(|v| (v, asked))
        };
        for (depth, want) in [(json!(null), Depth::Full), (json!("full"), Depth::Full), (json!("listing"), Depth::Listing), (json!("disk"), Depth::Disk)] {
            let (_, asked) = list(json!({"dir": dir, "depth": depth})).unwrap();
            assert_eq!(asked, Some(want), "{depth}");
        }
        let (v, _) = list(json!({"dir": dir})).unwrap();
        assert_eq!(v["thread_children"], json!({"d": {"one": "web-one"}}));
        // The snapshot's fields stay at top level, as in `status --json`.
        assert_eq!(v["runtime_name"], "docker");
        assert_eq!(v["instances"], json!([]));
        let bad = list(json!({"dir": dir, "depth": "deep"})).unwrap_err();
        assert_eq!((bad.code, bad.message.as_str()), ("invalid", "unknown depth `deep` (disk, listing, full)"));
        // An unreadable state resolves no child; the snapshot still answers.
        let none = instances_list_with(parse(json!({"dir": dir})).unwrap(), &state.with_file_name("absent.toml"), |_: &Path, _| empty_snapshot());
        assert_eq!(none.unwrap()["thread_children"], json!({}));
    }

    #[test]
    fn instances_resolve_returns_every_match_of_the_winning_tier() {
        let state = state_file("resolve", &[instance("web", "web", ""), instance("web-2", "web", ""), instance("x", "web", "")].concat());
        let resolve = |params: Value| instances_resolve(parse(params)?, &state);
        assert_eq!(resolve(json!({"name": "web"})).unwrap(), json!({"matches": ["web"]}), "exact wins");
        assert_eq!(resolve(json!({"name": "w"})).unwrap(), json!({"matches": ["web", "web-2", "x"]}), "folder basename");
        assert_eq!(code(resolve(json!({"name": "nope"}))), "not-found");
        assert_eq!(code(resolve(json!({}))), "invalid");
        // A missing state file is no instances.
        let none = instances_resolve(NameParams { name: "web".into() }, &state.with_file_name("absent.toml"));
        assert_eq!(code(none), "not-found");
    }

    #[test]
    fn instances_cancel_signals_only_while_in_flight() {
        // Our own pid reads as alive; the signal fns send nothing.
        let me = std::process::id();
        let progress = |key: &str, phase: &str| format!("[instance.{key}.progress]\nphase = \"{phase}\"\npid = {me}\n");
        let state = state_file(
            "cancel",
            &[instance("w", "web", &progress("w", "building")), instance("f", "web", &progress("f", "failed")), instance("r", "web", "")].concat(),
        );
        fn sent(_: u32) -> Result<(), String> {
            Ok(())
        }
        fn refused(_: u32) -> Result<(), String> {
            Err("EPERM".into())
        }
        let cancel = |params: Value, signal| instances_cancel(parse(params)?, &state, signal);
        assert_eq!(cancel(json!({"instance": "w"}), sent).unwrap(), json!({"ok": true, "pid": me}));
        let done = cancel(json!({"instance": "f"}), sent).unwrap_err();
        assert_eq!((done.code, done.message.as_str()), ("invalid", "`f` is not building any more"));
        assert_eq!(code(cancel(json!({"instance": "r"}), sent)), "invalid", "ready");
        assert_eq!(code(cancel(json!({"instance": "nope"}), sent)), "not-found");
        assert_eq!(code(cancel(json!({"instance": "w"}), refused)), "internal");
        assert_eq!(code(cancel(json!({}), sent)), "invalid");
    }

    #[test]
    fn instances_signal_checks_before_reaching_the_runtime() {
        let state = state_file("signal", &instance("web", "web", ""));
        let sent = std::cell::RefCell::new(Vec::new());
        let signal = |params: Value| {
            instances_signal(parse(params)?, &state, |c, pid, n| {
                sent.borrow_mut().push((c.to_string(), pid.to_string(), n));
                Ok(())
            })
        };
        assert_eq!(signal(json!({"instance": "web", "pid": "42", "signal": "TERM"})).unwrap(), json!({"ok": true}));
        signal(json!({"instance": "web", "pid": "7", "signal": "KILL"})).unwrap();
        assert_eq!(*sent.borrow(), [("devsandbox-web".to_string(), "42".to_string(), 15), ("devsandbox-web".to_string(), "7".to_string(), 9)]);
        for bad in [
            json!({"instance": "web", "pid": "42", "signal": "INT"}),
            json!({"instance": "web", "pid": "42", "signal": "SIGTERM"}),
            json!({"instance": "web", "pid": "-1", "signal": "TERM"}),
            json!({"instance": "web", "pid": "0", "signal": "TERM"}),
            json!({"instance": "web", "pid": "", "signal": "TERM"}),
            json!({"instance": "web", "pid": "4 2", "signal": "TERM"}),
            json!({"instance": "web", "pid": 42, "signal": "TERM"}),
            json!({"instance": "web", "signal": "TERM"}),
        ] {
            assert_eq!(code(signal(bad.clone())), "invalid", "{bad}");
        }
        assert_eq!(code(signal(json!({"instance": "nope", "pid": "42", "signal": "TERM"}))), "not-found");
        assert_eq!(sent.borrow().len(), 2, "refused before the runtime");
        let failing = instances_signal(parse(json!({"instance": "web", "pid": "42", "signal": "TERM"})).unwrap(), &state, |_, _, _| {
            anyhow::bail!("No such container: devsandbox-web")
        });
        assert_eq!(code(failing), "internal");
    }

    #[test]
    fn instances_rename_moves_the_key_and_maps_refusals() {
        let me = std::process::id();
        let building = format!("[instance.b.progress]\nphase = \"building\"\npid = {me}\n");
        let state = state_file("rename", &[instance("web", "web", ""), instance("api", "web", ""), instance("b", "web", &building)].concat());
        let rename = |params: Value| instances_rename(parse(params)?, &state);
        assert_eq!(rename(json!({"instance": "web", "newName": " web-x "})).unwrap(), json!({"instance": "web-x"}));
        let s = crate::state::State::load_at(&state).unwrap();
        assert!(!s.instances.contains_key("web"));
        assert_eq!(s.instances["web-x"].container, "devsandbox-web", "only the key moves");
        let taken = rename(json!({"instance": "web-x", "newName": "api"})).unwrap_err();
        assert_eq!((taken.code, taken.message.as_str()), ("invalid", "an instance named `api` already exists"));
        for new in ["", "a b", "web-x"] {
            assert_eq!(code(rename(json!({"instance": "web-x", "newName": new}))), "invalid", "{new:?}");
        }
        assert_eq!(code(rename(json!({"instance": "b", "newName": "b2"}))), "invalid", "in flight");
        assert_eq!(code(rename(json!({"instance": "web", "newName": "w"}))), "not-found");
        assert_eq!(code(rename(json!({"instance": "api", "new_name": "w"}))), "invalid", "camelCase");
        // Refusals wrote nothing.
        let after = crate::state::State::load_at(&state).unwrap();
        assert_eq!(after.instances.keys().collect::<Vec<_>>(), ["api", "b", "web-x"]);
        std::fs::write(&state, "not = [toml").unwrap();
        assert_eq!(code(rename(json!({"instance": "api", "newName": "w"}))), "internal");
    }

    #[test]
    fn instances_done_sets_and_clears_the_flag() {
        let state = state_file("done", &instance("web", "web", ""));
        let done = |params: Value| instances_done(parse(params)?, &state);
        let flag = || crate::state::State::load_at(&state).unwrap().instances["web"].done;
        assert_eq!(done(json!({"instance": "web", "done": true})).unwrap(), json!({"instance": "web", "done": true}));
        let since = flag().expect("marked done");
        // Idempotent: the original since is kept.
        done(json!({"instance": "web", "done": true})).unwrap();
        assert_eq!(flag(), Some(since));
        assert_eq!(done(json!({"instance": "web", "done": false})).unwrap(), json!({"instance": "web", "done": false}));
        assert_eq!(flag(), None);
        let unknown = done(json!({"instance": "nope", "done": true})).unwrap_err();
        assert_eq!((unknown.code, unknown.message.as_str()), ("not-found", "no instance `nope`"));
        assert_eq!(code(done(json!({"instance": "web"}))), "invalid");
    }

    #[test]
    fn sandboxes_list_names_the_configs_sandboxes() {
        let root = store_path("sandboxes").with_file_name("cfg");
        std::fs::create_dir_all(&root).unwrap();
        let list = |root: &Path| run(&store_path("sandboxes-x"), "sandboxes.list", json!({"root": root}));
        assert_eq!(list(&root).unwrap(), json!([]), "no config: none");
        std::fs::write(root.join(crate::config::CONFIG_FILE), "[sandbox.web]\nimage = \"alpine\"\n[sandbox.api]\nimage = \"alpine\"\n").unwrap();
        assert_eq!(list(&root).unwrap(), json!([{"name": "api"}, {"name": "web"}]));
        std::fs::write(root.join(crate::config::CONFIG_FILE), "[sandbox.web\n").unwrap();
        let bad = list(&root).unwrap_err();
        assert!(bad.code == "invalid" && bad.message.contains("devsandboxes.toml"), "{bad:?}");
        assert_eq!(code(list(Path::new("rel"))), "invalid");
        assert_eq!(code(list(Path::new("/nonexistent/devsandbox-api"))), "not-found");
        assert_eq!(code(run(&root, "sandboxes.list", json!({}))), "invalid");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    fn listing(pid: &str) -> ProcsListing {
        ProcsListing {
            processes: vec![Process { pid: pid.into(), ppid: "0".into(), args: "sleep 9".into() }],
            signalable: true,
            runs: vec![crate::runtime::procs::Run {
                id: "1790000000-a1b2".into(),
                state: "exited 4".into(),
                started: "2026-09-21T14:13:20Z".into(),
                argv: "sh -c x".into(),
            }],
        }
    }

    #[test]
    fn instances_procs_answers_every_instance_failures_included() {
        let state = state_file("procs", &[instance("web", "web", ""), instance("off", "web", "")].concat());
        let fetch = |container: &str| match container {
            "devsandbox-web" => Ok(listing("1")),
            _ => Err("not running".to_string()),
        };
        let procs = |params: Value| instances_procs(parse(params)?, &state, fetch);
        let r = procs(json!({"instances": ["web", "off", "nope", "web"]})).unwrap();
        assert_eq!(
            r,
            json!({
                "web": {
                    "processes": [{"pid": "1", "ppid": "0", "args": "sleep 9"}],
                    "signalable": true,
                    "runs": [{"id": "1790000000-a1b2", "state": "exited 4", "started": "2026-09-21T14:13:20Z", "argv": "sh -c x"}],
                },
                "off": {"error": "not running"},
                "nope": {"error": "no instance `nope`"},
            })
        );
        // What the dashboard will parse it back into.
        let back: ProcsAnswer = serde_json::from_value(r).unwrap();
        assert_eq!(back["web"], InstanceProcs::Listed(listing("1")));
        assert_eq!(back["off"], InstanceProcs::Failed { error: "not running".into() });
        assert_eq!(procs(json!({"instances": []})).unwrap(), json!({}));
        assert_eq!(code(procs(json!({}))), "invalid");
        // An unreadable state fails the call: no entry could be right.
        std::fs::write(&state, "not = [toml").unwrap();
        assert_eq!(code(procs(json!({"instances": ["web"]}))), "internal");
    }

    #[test]
    fn instances_build_log_reads_by_name_or_id_next_to_the_state() {
        let state = state_file(
            "buildlog",
            &[instance("web", "web", "instance_id = \"web-id\"\n"), instance("new", "web", "instance_id = \"new-id\"\n")].concat(),
        );
        let log = state.with_file_name("logs").join("web-id");
        std::fs::create_dir_all(&log).unwrap();
        std::fs::write(log.join(crate::build_log::FILE), "one\ntwo\nthree\n").unwrap();
        let read = |params: Value| instances_build_log(parse(params)?, &state);
        assert_eq!(read(json!({"instance": "web"})).unwrap(), json!({"text": "one\ntwo\nthree", "cut": false}));
        assert_eq!(read(json!({"instance": "web-id", "lines": 2})).unwrap(), json!({"text": "two\nthree", "cut": true}));
        assert_eq!(read(json!({"instance": "new"})).unwrap(), Value::Null, "no log yet");
        assert_eq!(code(read(json!({"instance": "gone"}))), "not-found");
        assert_eq!(code(read(json!({"instance": "web", "lines": -1}))), "invalid");
        assert_eq!(code(read(json!({"instance": "web", "ansi": "yes"}))), "invalid");
        assert_eq!(code(read(json!({}))), "invalid");
        // A plain log reads the same with `ansi`.
        assert_eq!(read(json!({"instance": "web", "ansi": true})).unwrap(), json!({"text": "one\ntwo\nthree", "cut": false}));
        // A pty log is replayed: SGR with `ansi`, plain without.
        let pty = "$ devsandbox run\n# 1970-01-01T00:00:00Z\n# tty 80x24\n\x1b[32mok\x1b[0m\r\n";
        std::fs::write(log.join(crate::build_log::FILE), pty).unwrap();
        let ask = |ansi: bool| read(json!({"instance": "web", "lines": 1, "ansi": ansi})).unwrap();
        assert_eq!(ask(false), json!({"text": "ok", "cut": true}));
        assert_eq!(ask(true), json!({"text": "\u{1b}[0;32mok\u{1b}[0m", "cut": true}));
    }

    #[test]
    fn containers_methods_take_only_devsandbox_containers() {
        let asked = std::cell::RefCell::new(Vec::new());
        let logs = |params: Value| {
            containers_logs(parse(params)?, |c, n| {
                asked.borrow_mut().push((c.to_string(), n));
                Ok("hello\n".to_string())
            })
        };
        assert_eq!(logs(json!({"container": "devsandbox-web"})).unwrap(), json!({"text": "hello\n"}));
        logs(json!({"container": "devsandbox-web", "lines": 5})).unwrap();
        logs(json!({"container": "devsandbox-web", "lines": 1_000_000})).unwrap();
        assert_eq!(
            *asked.borrow(),
            [("devsandbox-web".to_string(), 50), ("devsandbox-web".to_string(), 5), ("devsandbox-web".to_string(), MAX_LINES)]
        );
        for bad in [json!({"container": "postgres"}), json!({"container": "x-devsandbox-web"}), json!({}), json!({"container": "devsandbox-web", "lines": "all"})] {
            assert_eq!(code(logs(bad.clone())), "invalid", "{bad}");
        }
        assert_eq!(asked.borrow().len(), 3, "refused before the runtime");
        let failing = containers_logs(parse(json!({"container": "devsandbox-x"})).unwrap(), |_, _| {
            Err(ApiError::internal(anyhow::anyhow!("No such container: devsandbox-x")))
        });
        assert_eq!(failing.unwrap_err().message, "No such container: devsandbox-x");

        let inspect = |container: &str, out: &'static str| {
            containers_inspect(parse(json!({"container": container}))?, |_| Ok(out.to_string()))
        };
        assert_eq!(inspect("devsandbox-web", r#"[{"Id": "abc"}]"#).unwrap(), json!([{"Id": "abc"}]));
        assert_eq!(code(inspect("devsandbox-web", "not json")), "internal");
        assert_eq!(code(inspect("web", "[]")), "invalid");
    }

    #[test]
    fn sandboxes_get_and_services_get_answer_the_config_text() {
        let root = store_path("config-get").with_file_name("cfg");
        std::fs::create_dir_all(&root).unwrap();
        let get = |method: &str, name: &str| run(&root.join("x"), method, json!({"root": root, "name": name}));
        assert_eq!(code(get("sandboxes.get", "web")), "not-found", "no config file: no sandbox");
        assert_eq!(code(get("services.get", "db")), "not-found");
        std::fs::write(
            root.join(crate::config::CONFIG_FILE),
            "[template.base]\nimage = \"alpine\"\n[sandbox.web]\nextends = [\"base\"]\nfolder = \"/w\"\n\
             [sandbox.broken]\nextends = [\"missing\"]\n[services.db]\nimage = \"postgres\"\n",
        )
        .unwrap();
        let web = get("sandboxes.get", "web").unwrap();
        assert_eq!(web["original"], "extends = [\"base\"]\nfolder = \"/w\"\n");
        assert_eq!(web["resolved"], "folder = \"/w\"\nimage = \"alpine\"\n");
        let cfg = crate::config::Config::load(&root).unwrap();
        assert_eq!(web["hash"], cfg.resolved_table("web").unwrap().1);
        let back: crate::config::SandboxText = serde_json::from_value(web).unwrap();
        assert_eq!(back, cfg.sandbox_text("web"));
        // One that doesn't merge still answers, with why.
        let broken = get("sandboxes.get", "broken").unwrap();
        assert!(broken["resolved"].as_str().unwrap().contains("missing"), "{broken}");
        assert_eq!(broken["hash"], "");
        assert_eq!(code(get("sandboxes.get", "nope")), "not-found");
        assert_eq!(get("services.get", "db").unwrap(), json!({"original": "image = \"postgres\"\n"}));
        assert_eq!(code(get("services.get", "web")), "not-found", "a sandbox isn't a service");
        // Root checks as `sandboxes.list`.
        assert_eq!(code(run(&root, "sandboxes.get", json!({"root": "rel", "name": "web"}))), "invalid");
        assert_eq!(code(run(&root, "services.get", json!({"root": "/nonexistent/devsandbox-api", "name": "db"}))), "not-found");
        assert_eq!(code(run(&root, "sandboxes.get", json!({"root": root}))), "invalid");
        std::fs::write(root.join(crate::config::CONFIG_FILE), "[sandbox.web\n").unwrap();
        assert_eq!(code(get("sandboxes.get", "web")), "invalid");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// Against a real throwaway container: its logs, its inspect JSON, and
    /// its processes through the production fetch (no helper: no runs).
    #[test_utils::docker_test]
    fn containers_and_procs_against_docker() -> Result<(), &'static str> {
        use std::process::Command;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-api-test-{stamp}");
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            let up = Command::new("docker")
                .args(["run", "-d", "--name", &name, "alpine:3.20", "sh", "-c", "echo hello-api; exec sleep 300"])
                .output();
            if !matches!(up, Ok(o) if o.status.success()) {
                return Err("docker run alpine:3.20 failed");
            }
            let backend = crate::runtime::backend();
            let mut text = String::new();
            for _ in 0..50 {
                let r = containers_logs(parse(json!({"container": name})).unwrap(), |c, n| {
                    backend.logs_tail(c, n).map_err(ApiError::internal)
                })
                .unwrap();
                text = r["text"].as_str().unwrap().to_string();
                if text.contains("hello-api") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(text.contains("hello-api"), "{text:?}");
            let inspected = containers_inspect(parse(json!({"container": name})).unwrap(), |c| {
                backend.inspect_json(c).map_err(ApiError::internal)
            })
            .unwrap();
            assert_eq!(inspected[0]["Name"], format!("/{name}"));
            let listing = fetch_procs(&name).unwrap();
            assert!(listing.signalable);
            assert!(listing.processes.iter().any(|p| p.args == "sleep 300"), "{listing:?}");
            assert!(listing.runs.is_empty());
            let missing = containers_inspect(parse(json!({"container": format!("{name}-gone")})).unwrap(), |c| {
                backend.inspect_json(c).map_err(ApiError::internal)
            });
            assert_eq!(code(missing), "internal");
            Command::new("docker").args(["stop", "-t", "0", &name]).output().map_err(|_| "docker stop failed")?;
            assert_eq!(fetch_procs(&name).unwrap_err(), "not running");
            Ok(())
        })
    }

    /// The resolve / cancel / sandboxes answers match `index.d.ts`.
    #[test]
    fn npm_typings_match_the_instance_and_sandbox_answers() {
        use crate::serve::dts::{assert_matches, prop_type, union_members};
        fn v(x: &impl Serialize) -> Value {
            serde_json::to_value(x).unwrap()
        }
        assert_matches("InstancesResolved", &v(&Resolved { matches: vec!["web".into()] }));
        assert_matches("InstanceCancelled", &v(&Cancelled { ok: true, pid: 1 }));
        assert_matches("SandboxEntry", &v(&SandboxEntry { name: "web".into() }));
        let list = InstancesList { snapshot: empty_snapshot(), thread_children: Default::default() };
        assert_matches("InstancesList", &v(&list));
        let cfg = crate::config::Config::parse("[sandbox.web]\nimage = \"alpine\"\n").unwrap();
        let (rows, _) = crate::snapshot::sandbox_rows(&std::env::temp_dir(), &cfg);
        assert_matches("SandboxRow", &v(&rows[0]));
        let listed = listing("1");
        assert_matches("ProcsListing", &v(&listed));
        assert_matches("ProcessRow", &v(&listed.processes[0]));
        assert_matches("RunRow", &v(&listed.runs[0]));
        assert_matches("ProcsError", &v(&InstanceProcs::Failed { error: "not running".into() }));
        assert_eq!(union_members("InstanceProcs"), ["ProcsListing", "ProcsError"]);
        assert_matches("BuildLogTail", &v(&crate::build_log::Tail { text: "t".into(), cut: false }));
        assert_matches("ContainerLogs", &v(&ContainerLogs { text: "t".into() }));
        assert_matches("SandboxConfig", &v(&cfg.sandbox_text("web")));
        assert_matches("ServiceConfig", &v(&ServiceConfig { original: "t".into() }));
        assert_matches("InstanceRenamed", &v(&Renamed { instance: "w".into() }));
        assert_matches("InstanceDone", &v(&DoneSet { instance: "w".into(), done: true }));
        for (method, result) in [
            ("instances.list", "InstancesList"),
            ("instances.watch", "OkResult"),
            ("instances.unwatch", "OkResult"),
            ("instances.resolve", "InstancesResolved"),
            ("instances.cancel", "InstanceCancelled"),
            ("instances.procs", "Record<string, InstanceProcs>"),
            ("instances.buildLog", "BuildLogTail | null"),
            ("instances.signal", "OkResult"),
            ("instances.rename", "InstanceRenamed"),
            ("instances.done", "InstanceDone"),
            ("containers.logs", "ContainerLogs"),
            ("containers.inspect", "unknown"),
            ("sandboxes.list", "SandboxEntry[]"),
            ("sandboxes.get", "SandboxConfig"),
            ("services.get", "ServiceConfig"),
        ] {
            let ty = prop_type("ApiMethods", method).unwrap_or_else(|| panic!("ApiMethods lacks `{method}`"));
            assert!(ty.contains(&format!("result: {result} ")), "{method}: {ty}");
        }
    }

    #[test]
    fn watch_dirs_are_absolute_and_exist_to_watch() {
        let tmp = std::env::temp_dir();
        assert_eq!(watch_dir(json!({"dir": tmp}), true).unwrap(), tmp);
        assert_eq!(watch_dir(json!({"dir": "rel"}), true).unwrap_err().code, "invalid");
        assert_eq!(watch_dir(json!({"dir": "rel"}), false).unwrap_err().code, "invalid");
        assert_eq!(watch_dir(json!({}), true).unwrap_err().code, "invalid");
        let gone = tmp.join(format!("dsv-gone-{}", std::process::id()));
        assert_eq!(watch_dir(json!({"dir": gone}), true).unwrap_err().code, "not-found");
        // Letting go of a root deleted meanwhile is fine.
        assert_eq!(watch_dir(json!({"dir": gone}), false).unwrap(), gone);
    }

    #[test]
    fn topics_parse_and_reject_unknown_ones() {
        assert_eq!(
            topics(json!({"topics":["inbox","instances","forwards"]})).unwrap(),
            [Topic::Inbox, Topic::Instances, Topic::Forwards]
        );
        assert_eq!(topics(json!({"topics":["ports"]})).unwrap_err().code, "invalid");
        assert_eq!(topics(Value::Null).unwrap_err().code, "invalid");
    }

    /// Records what `forwards.*` asked for; forward 1 exists (configured),
    /// instance `web` exists.
    #[derive(Default)]
    struct FakeForwards {
        adds: std::cell::RefCell<Vec<AddRequest>>,
        lists: std::cell::RefCell<Vec<Option<PathBuf>>>,
    }

    impl Forwarding for FakeForwards {
        fn list(&self, dir: Option<PathBuf>) -> Result<Vec<ForwardRow>, ApiError> {
            self.lists.borrow_mut().push(dir);
            Ok(vec![ForwardRow {
                id: 1,
                dir: "/cfg".into(),
                local: "127.0.0.1:3000".into(),
                target: "web:3000".into(),
                process: None,
                state: "active".into(),
                conns: 0,
                configured: true,
            }])
        }
        fn add(&self, req: AddRequest) -> Result<Added, ApiError> {
            if req.instance.as_deref().is_some_and(|i| i != "web") {
                return Err(ApiError::not_found("no instance"));
            }
            self.adds.borrow_mut().push(req);
            Ok(Added { id: 2, local: "127.0.0.1:3001".into(), target: "web:3000".into() })
        }
        fn rm(&self, id: u64) -> Result<Removed, ApiError> {
            match id {
                1 => Ok(Removed { local: "127.0.0.1:3000".into(), configured: true }),
                _ => Err(ApiError::not_found(format!("no forward {id}"))),
            }
        }
    }

    #[test]
    fn forwards_methods_validate_then_call_the_registry() {
        use crate::devsbd::forward::HostPort;
        let path = store_path("forwards");
        let fake = FakeForwards::default();
        let fwd = |method: &str, params: Value| call(method, params, &Ctx { inbox: &path, client: "api:test", bridges: None, forwards: Some(&fake) });

        let rows = fwd("forwards.list", json!({"dir": "/cfg"})).unwrap();
        assert_eq!(rows[0]["id"], 1);
        assert_eq!(rows[0]["configured"], true);
        fwd("forwards.list", Value::Null).unwrap();
        assert_eq!(*fake.lists.borrow(), [Some(PathBuf::from("/cfg")), None]);
        assert_eq!(code(fwd("forwards.list", json!({"dir": "rel"}))), "invalid");

        let added = fwd("forwards.add", json!({"dir": "/cfg", "instance": "web", "spec": "8080:3000"})).unwrap();
        assert_eq!(added, json!({"id": 2, "local": "127.0.0.1:3001", "target": "web:3000"}));
        fwd("forwards.add", json!({"dir": "/cfg", "instance": "", "service": "db", "address": "0.0.0.0", "spec": "5432"}))
            .unwrap();
        let adds = fake.adds.borrow().clone();
        assert_eq!(
            adds,
            [
                AddRequest {
                    dir: "/cfg".into(),
                    instance: Some("web".into()),
                    service: None,
                    bind: "127.0.0.1".parse().unwrap(),
                    host_port: HostPort::Fixed(8080),
                    container_port: 3000,
                },
                AddRequest {
                    dir: "/cfg".into(),
                    instance: None,
                    service: Some("db".into()),
                    bind: "0.0.0.0".parse().unwrap(),
                    host_port: HostPort::Prefer(5432),
                    container_port: 5432,
                },
            ]
        );
        // Refused before the registry sees them.
        for bad in [
            json!({"dir": "rel", "instance": "web", "spec": "3000"}),
            json!({"dir": "/cfg", "spec": "3000"}),
            json!({"dir": "/cfg", "instance": "web", "spec": "0"}),
            json!({"dir": "/cfg", "instance": "web", "spec": "a:b"}),
            json!({"dir": "/cfg", "instance": "web", "spec": "3000", "address": "localhost"}),
            json!({"dir": "/cfg", "instance": "web"}),
        ] {
            assert_eq!(code(fwd("forwards.add", bad.clone())), "invalid", "{bad}");
        }
        assert_eq!(fake.adds.borrow().len(), 2);
        assert_eq!(code(fwd("forwards.add", json!({"dir": "/cfg", "instance": "nope", "spec": "3000"}))), "not-found");

        assert_eq!(
            fwd("forwards.rm", json!({"id": 1})).unwrap(),
            json!({"ok": true, "local": "127.0.0.1:3000", "configured": true})
        );
        assert_eq!(code(fwd("forwards.rm", json!({"id": 9}))), "not-found");
        assert_eq!(code(fwd("forwards.rm", json!({}))), "invalid");

        // A daemon without a host side says so.
        assert_eq!(code(run(&path, "forwards.list", json!({}))), "internal");
    }

    /// Every wire struct the npm package types matches its `index.d.ts`
    /// interface (`serve::dts`). Option fields serialize as `null`, so one
    /// value of each shows every key.
    #[test]
    fn npm_typings_match_the_wire_structs() {
        use crate::serve::dts::{assert_matches, assert_variant, union_members};
        use crate::serve::proto::{ErrorBody, HelloParams, HelloResult};
        fn v(x: &impl Serialize) -> Value {
            serde_json::to_value(x).unwrap()
        }
        let summary = || ThreadSummary {
            id: 1,
            owner: "o-id".into(),
            owner_name: "o".into(),
            key: Some("k".into()),
            kind: "thread",
            state: Some("active"),
            status: Some("s".into()),
            title: "t".into(),
            level: None,
            unread: true,
            archived: false,
            needs_you: false,
            changed_at: 1,
        };
        let detail = ThreadDetail {
            summary: summary(),
            link: None,
            child: None,
            compose: Some(ComposeView { placeholder: None, hint: None }),
            actions: vec![],
            feed: vec![],
            notes: vec![],
            events_pending: 0,
        };
        let action = ActionView { id: "go".into(), label: "Go".into(), done: false, host: None, sends_event: true };
        let note = NoteView { id: 1, level: "info", msg: "m".into(), link: None, at: 1 };
        let row = ForwardRow {
            id: 1,
            dir: "/cfg".into(),
            local: "127.0.0.1:3000".into(),
            target: "web:3000".into(),
            process: None,
            state: "active".into(),
            conns: 0,
            configured: false,
        };
        let added = Added { id: 1, local: "127.0.0.1:3000".into(), target: "web:3000".into() };
        let removed = ForwardRemoved { ok: true, local: "127.0.0.1:3000".into(), configured: false };
        let hello = HelloParams { version: "0.6.0".into(), build: 1, client: "api:x".into() };
        let hello_result = HelloResult { version: "0.6.0".into(), build: 1, protocol: 1, handoff: true };
        let error = ErrorBody { code: "failed".into(), message: "m".into(), data: Some(json!({"exit": 1})) };

        let cases: [(&str, Value); 12] = [
            ("ThreadSummary", v(&summary())),
            ("ThreadDetail", v(&detail)),
            ("ThreadCompose", v(&ComposeView { placeholder: None, hint: None })),
            ("ThreadAction", v(&action)),
            ("ThreadNote", v(&note)),
            ("ForwardRow", v(&row)),
            ("ForwardAdded", v(&added)),
            ("ForwardRemoved", v(&removed)),
            ("HelloParams", v(&hello)),
            ("HelloResult", v(&hello_result)),
            ("ApiErrorBody", v(&error)),
            ("OkResult", ok().unwrap()),
        ];
        for (iface, value) in &cases {
            assert_matches(iface, value);
        }
        assert_matches("MessageField", &v(&FieldView { label: "Head".into(), value: "36b1".into() }));
        // Tagged unions: one interface per variant, its `type` (and a
        // marker's `marker`) a literal the d.ts union is built from.
        let blocks = vec![BlockView::Markdown { text: "t".into() }];
        let variants: [(&str, Value); 6] = [
            ("FeedMessage", v(&FeedItemView::Message { seq: 1, at: 1, id: "m".into(), blocks, edited: false, withdrawn: false })),
            ("FeedReply", v(&FeedItemView::Reply { seq: 1, at: 1, text: "t".into() })),
            ("FeedAction", v(&FeedItemView::Action { seq: 1, at: 1, action: "go".into(), label: "Go".into() })),
            ("FeedMarker", v(&FeedItemView::Marker { seq: 1, at: 1, marker: "done", from: None, to: None })),
            ("MarkdownBlock", v(&BlockView::Markdown { text: "t".into() })),
            ("FieldsBlock", v(&BlockView::Fields { items: vec![FieldView { label: "l".into(), value: "v".into() }] })),
        ];
        for (iface, value) in &variants {
            assert_variant(iface, value);
        }
        // Forms: the block (open and submitted), each question type, the
        // submission item.
        let mixed = form::tests::mixed();
        let open = FormRecord::open("f");
        let submitted = FormRecord {
            state: form::FormState::Submitted { at: 1, answers: Default::default(), client: "tui".into() },
            ..open.clone()
        };
        let example = form::tests::example();
        let questions: Vec<Value> = example.questions.iter().chain(&mixed.questions).map(|q| v(&QuestionView::of(q))).collect();
        let forms: Vec<(&str, Value)> = [
            ("FormBlock", v(&BlockView::of(&Block::Form(mixed.clone()), Some(&open)))),
            ("FormBlock", v(&BlockView::of(&Block::Form(mixed.clone()), Some(&submitted)))),
            ("FeedSubmission", v(&FeedItemView::Submission { seq: 1, at: 1, message: "m".into(), form: "f".into(), answers: json!({}) })),
        ]
        .into_iter()
        .chain(questions.into_iter().map(|q| {
            let iface = match q["type"].as_str() {
                Some("choice") => "ChoiceQuestion",
                Some("text") => "TextQuestion",
                _ => "ConfirmQuestion",
            };
            (iface, q)
        }))
        .collect();
        for (iface, value) in &forms {
            assert_variant(iface, value);
        }
        assert_matches("ChoiceOption", &v(&OptionView { id: "a".into(), label: "A".into(), description: None }));
        assert_eq!(union_members("FeedItem"), ["FeedMessage", "FeedReply", "FeedAction", "FeedSubmission", "FeedMarker"]);
        assert_eq!(union_members("MessageBlock"), ["MarkdownBlock", "FieldsBlock", "FormBlock"]);
        assert_eq!(union_members("FormQuestion"), ["ChoiceQuestion", "TextQuestion", "ConfirmQuestion"]);
        assert_eq!(crate::serve::dts::prop_type("FormBlock", "state").as_deref(), Some("'open' | 'submitted' | 'withdrawn'"));
        assert_eq!(union_members("FormAnswer"), ["string", "string[]", "boolean"]);
        assert_eq!(
            crate::serve::dts::prop_type("FeedMarker", "marker").as_deref(),
            Some("'done' | 'reopen' | 'state' | 'status'")
        );
    }
}
