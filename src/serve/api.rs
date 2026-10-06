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
//! Connection-level methods (`hello`, `subscribe`, `unsubscribe`) live in
//! `daemon.rs`, which owns the connection; [`topics`] parses theirs.
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
        "instances.list" => instances_list(parse(params)?),
        "bridges.ensure" => bridges_ensure(parse(params)?, ctx),
        "forwards.list" => forwards_list(parse(params)?, ctx),
        "forwards.add" => forwards_add(parse(params)?, ctx),
        "forwards.rm" => forwards_rm(parse(params)?, ctx),
        other => Err(ApiError::unknown_method(other)),
    }
}

/// Params into `T`; absent params count as `{}`. Unknown fields are
/// ignored, so a newer client's additive fields don't break an older daemon.
fn parse<T: DeserializeOwned>(params: Value) -> Result<T, ApiError> {
    let params = if params.is_null() { Value::Object(Default::default()) } else { params };
    serde_json::from_value(params).map_err(|e| ApiError::invalid(format!("bad params: {e}")))
}

/// A notification topic for `subscribe` / `unsubscribe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Topic {
    /// `inbox.changed`: the store changed, re-fetch.
    Inbox,
    /// `instances.changed`: the running containers changed, re-fetch.
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
}

/// The `status --json` snapshot (without its envelope: the API versions
/// itself through `hello`) for config root `dir`. Absolute only: the
/// daemon's cwd is `/`, not the client's.
fn instances_list(p: InstancesParams) -> Answer {
    if !p.dir.is_absolute() {
        return Err(ApiError::invalid(format!("`dir` must be absolute, got `{}`", p.dir.display())));
    }
    if !p.dir.is_dir() {
        return Err(ApiError::not_found(format!("no directory {}", p.dir.display())));
    }
    serde_json::to_value(crate::snapshot::collect(&p.dir)).map_err(|e| ApiError::internal(e.into()))
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
        let error = ErrorBody { code: "invalid".into(), message: "m".into() };

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
