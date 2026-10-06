//! The daemon's API methods (docs/api.md): the method table and one thin
//! handler per method, each a call into `inbox::ops` or `snapshot`. Params
//! parse through serde structs; results are explicit wire views
//! ([`ThreadSummary`], [`ThreadDetail`]), never the store model, whose TOML
//! shape is internal and is replaced at store v3.
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

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::inbox::thread::HostVerb;
use crate::inbox::{EntryKind, Inbox, Kind, Op, Thread, View, ops};

/// What a handler needs from the daemon. The store path is injectable so
/// tests (and a test daemon) use their own file.
pub struct Ctx<'a> {
    pub inbox: &'a Path,
}

/// An error answer: a stable machine `code` (docs/api.md, *Errors*) and a
/// human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not-found", message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid", message)
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self::new("denied", message)
    }

    pub fn internal(e: anyhow::Error) -> Self {
        Self::new("internal", format!("{e:#}"))
    }

    pub fn unknown_method(method: &str) -> Self {
        Self::new("unknown-method", format!("unknown method `{method}`"))
    }
}

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
            mutate(ctx, |inbox| Ok(vec![Op::MarkDone(user_target(inbox, &addr)?.id)]))
        }
        "inbox.thread.reopen" => {
            let addr: Addr = parse(params)?;
            mutate(ctx, |inbox| Ok(vec![Op::Reopen(user_target(inbox, &addr)?.id)]))
        }
        "inbox.notify.dismiss" => dismiss(parse(params)?, ctx),
        "inbox.notify.markRead" => notify_mark_read(parse(params)?, ctx),
        "instances.list" => instances_list(parse(params)?),
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
}

impl Topic {
    pub fn parse(s: &str) -> Option<Topic> {
        match s {
            "inbox" => Some(Topic::Inbox),
            "instances" => Some(Topic::Instances),
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
        .map(|t| Topic::parse(t).ok_or_else(|| ApiError::invalid(format!("unknown topic `{t}` (inbox, instances)"))))
        .collect()
}

// ---- wire views -----------------------------------------------------------

/// One row of `inbox.threads.list`, and the head of `inbox.thread.get`.
/// Carries both addresses: `id`, and `owner` (instance id) / `owner_name` +
/// `key`.
#[derive(Debug, Serialize)]
pub struct ThreadSummary {
    /// Store id: stable for the thread's life, what `{"thread": id}` takes.
    pub id: u64,
    /// The owner's `instance_id`.
    pub owner: String,
    /// The owner's instance name when it last wrote (display; kept after
    /// `devsandbox rm`).
    pub owner_name: String,
    pub key: Option<String>,
    /// `notify` (`devsbd notify` records) or `thread` (`devsbd thread put`).
    pub kind: &'static str,
    /// `needs-you` | `active` | `done`; `null` on a notify thread.
    pub state: Option<&'static str>,
    pub status: Option<String>,
    /// A dispatcher thread's title, or a notify thread's newest message,
    /// first line only (what the dashboard's list shows).
    pub title: String,
    /// A notify thread's newest record level (`info` | `warn` | `error`).
    pub level: Option<&'static str>,
    pub unread: bool,
    pub archived: bool,
    /// Whether it's in the `needs-you` view (what the badges count).
    pub needs_you: bool,
    /// Unix seconds of the last change.
    pub changed_at: u64,
}

/// `inbox.thread.get`: the summary plus everything the pane shows.
#[derive(Debug, Serialize)]
pub struct ThreadDetail {
    #[serde(flatten)]
    pub summary: ThreadSummary,
    pub link: Option<String>,
    /// Key of the dispatcher child the thread is about.
    pub child: Option<String>,
    pub message: Option<String>,
    /// Present when the thread takes free-text replies.
    pub reply: Option<ReplyView>,
    pub actions: Vec<ActionView>,
    /// A dispatcher thread's timeline, oldest first.
    pub entries: Vec<EntryView>,
    /// A notify thread's records, newest first (the first is the head).
    pub notes: Vec<NoteView>,
    /// Events the owner hasn't acked yet.
    pub events_pending: usize,
}

#[derive(Debug, Serialize)]
pub struct ReplyView {
    pub placeholder: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ActionView {
    pub id: String,
    pub label: String,
    /// Pressing it sets the thread done.
    pub done: bool,
    /// The host verb it runs (`vscode`, `terminal`, `logs`, `forward`,
    /// `open`, `rm`), or `null` for a dispatcher-only action.
    pub host: Option<&'static str>,
    /// Pressing it sends the owner an event: what `inbox.thread.act` does.
    /// `false` means host-only, which the API refuses.
    pub sends_event: bool,
}

#[derive(Debug, Serialize)]
pub struct EntryView {
    /// Arrival order across the whole Inbox.
    pub seq: u64,
    pub at: u64,
    /// `message` | `state` | `status` | `action` | `reply` | `done` | `reopen`.
    pub kind: &'static str,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct NoteView {
    pub id: u64,
    pub level: &'static str,
    pub msg: String,
    pub link: Option<String>,
    pub at: u64,
}

fn kind_str(kind: Kind) -> &'static str {
    match kind {
        Kind::Notify => "notify",
        Kind::Thread => "thread",
    }
}

fn entry_kind_str(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Message => "message",
        EntryKind::State => "state",
        EntryKind::Status => "status",
        EntryKind::Action => "action",
        EntryKind::Reply => "reply",
        EntryKind::Done => "done",
        EntryKind::Reopen => "reopen",
    }
}

fn host_verb_str(verb: &HostVerb) -> &'static str {
    match verb {
        HostVerb::Vscode(_) => "vscode",
        HostVerb::Terminal(_) => "terminal",
        HostVerb::Logs(_) => "logs",
        HostVerb::Forward(_) => "forward",
        HostVerb::Open(_) => "open",
        HostVerb::Rm(_) => "rm",
    }
}

impl ThreadSummary {
    pub fn of(t: &Thread) -> Self {
        let title = match t.kind {
            Kind::Thread => t.title.clone(),
            Kind::Notify => t.head().map_or("", |r| r.msg.lines().next().unwrap_or("")).to_string(),
        };
        Self {
            id: t.id,
            owner: t.owner.clone(),
            owner_name: t.owner_name.clone(),
            key: t.key.clone(),
            kind: kind_str(t.kind),
            state: t.state.map(|s| s.as_str()),
            status: t.status.clone(),
            title,
            level: t.head().map(|r| r.level.as_str()),
            unread: t.unread,
            archived: t.archived,
            needs_you: t.needs_you(),
            changed_at: t.changed_at(),
        }
    }
}

impl ThreadDetail {
    pub fn of(t: &Thread) -> Self {
        Self {
            summary: ThreadSummary::of(t),
            link: t.link.clone().or_else(|| t.head().and_then(|r| r.link.clone())),
            child: t.child.clone(),
            message: t.message.clone(),
            reply: t.reply.as_ref().map(|r| ReplyView { placeholder: r.placeholder.clone() }),
            actions: t
                .actions
                .iter()
                .map(|a| ActionView {
                    id: a.id.clone(),
                    label: a.label.clone(),
                    done: a.done,
                    host: a.host.as_ref().map(host_verb_str),
                    sends_event: a.enqueues_event(),
                })
                .collect(),
            entries: t
                .entries
                .iter()
                .map(|e| EntryView { seq: e.seq, at: e.at, kind: entry_kind_str(e.kind), text: e.text.clone() })
                .collect(),
            notes: t
                .notes
                .iter()
                .map(|n| NoteView {
                    id: n.id,
                    level: n.record.level.as_str(),
                    msg: n.record.msg.clone(),
                    link: n.record.link.clone(),
                    at: n.record.at,
                })
                .collect(),
            events_pending: t.events.len(),
        }
    }
}

// ---- addressing -----------------------------------------------------------

/// How every `inbox.thread.*` names its thread: `{"thread": id}`, or
/// `{"owner": <instance name or id>, "key": …}` for a dispatcher thread.
#[derive(Debug, Default, Deserialize)]
struct Addr {
    #[serde(default)]
    thread: Option<u64>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

impl Addr {
    fn is_empty(&self) -> bool {
        self.thread.is_none() && self.owner.is_none() && self.key.is_none()
    }
}

/// The thread `addr` names. `owner` matches the instance id first, then the
/// instance name (`owner_name`); a name can have named several instances
/// over time (removed ones keep theirs on archived threads), so a live
/// thread wins, then the most recently changed.
fn resolve<'a>(inbox: &'a Inbox, addr: &Addr) -> Result<&'a Thread, ApiError> {
    let id = match (addr.thread, &addr.owner, &addr.key) {
        (Some(id), None, None) => Some(id),
        (None, Some(owner), Some(key)) => ops::thread_id(inbox, owner, key).or_else(|| {
            inbox
                .threads
                .iter()
                .filter(|t| t.kind == Kind::Thread && t.owner_name == *owner && t.key.as_deref() == Some(key))
                .min_by_key(|t| (t.archived, std::cmp::Reverse(t.changed_at())))
                .map(|t| t.id)
        }),
        _ => return Err(ApiError::invalid("name the thread by `thread` (its id), or by `owner` and `key`")),
    };
    let found = id.and_then(|id| inbox.threads.iter().find(|t| t.id == id));
    found.ok_or_else(|| match (addr.thread, &addr.owner, &addr.key) {
        (Some(id), ..) => ApiError::not_found(format!("no thread {id}")),
        (_, Some(owner), Some(key)) => ApiError::not_found(format!("no thread `{key}` from `{owner}`")),
        _ => ApiError::not_found("no such thread"),
    })
}

/// The target of a user op (act, reply, done, reopen): a live dispatcher
/// thread. A notify record has no owner to answer it; an archived thread's
/// owner is gone, so it's read-only.
fn user_target<'a>(inbox: &'a Inbox, addr: &Addr) -> Result<&'a Thread, ApiError> {
    let t = resolve(inbox, addr)?;
    if t.kind != Kind::Thread {
        return Err(ApiError::invalid(format!("thread {} is a notification, not a dispatcher thread", t.id)));
    }
    if t.archived {
        return Err(ApiError::denied(format!("thread {} is archived: its owner was removed", t.id)));
    }
    Ok(t)
}

// ---- handlers -------------------------------------------------------------

fn ok() -> Answer {
    Ok(json!({ "ok": true }))
}

/// Check and apply in one store write; see the module doc.
fn mutate(ctx: &Ctx, decide: impl FnOnce(&Inbox) -> Result<Vec<Op>, ApiError>) -> Answer {
    ops::apply_if(ctx.inbox, decide).map_err(ApiError::internal)??;
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
    let mut rows: Vec<&Thread> = inbox.threads.iter().filter(|t| view.shows(t)).collect();
    // Stable: ties keep the store's order (newest arrival first).
    rows.sort_by_key(|t| std::cmp::Reverse(t.changed_at()));
    let rows: Vec<ThreadSummary> = rows.into_iter().map(ThreadSummary::of).collect();
    serde_json::to_value(rows).map_err(|e| ApiError::internal(e.into()))
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

/// A dispatcher action's event half. Host verbs run in the client that
/// shows the button (the dashboard); the API refuses a host-only action
/// rather than pretend it ran.
fn act(p: ActParams, ctx: &Ctx) -> Answer {
    mutate(ctx, |inbox| {
        let t = user_target(inbox, &p.addr)?;
        let Some(a) = t.actions.iter().find(|a| a.id == p.action) else {
            return Err(ApiError::not_found(format!("thread {} has no action `{}`", t.id, p.action)));
        };
        if !a.enqueues_event() {
            return Err(ApiError::invalid(format!(
                "action `{}` only runs on the host ({}); the API doesn't run host verbs",
                a.id,
                a.host.as_ref().map_or("?", host_verb_str)
            )));
        }
        Ok(vec![Op::Act { thread: t.id, action: a.id.clone() }])
    })
}

#[derive(Deserialize)]
struct ReplyParams {
    #[serde(flatten)]
    addr: Addr,
    text: String,
}

fn reply(p: ReplyParams, ctx: &Ctx) -> Answer {
    if p.text.trim().is_empty() {
        return Err(ApiError::invalid("empty reply"));
    }
    mutate(ctx, |inbox| {
        let t = user_target(inbox, &p.addr)?;
        if t.reply.is_none() {
            return Err(ApiError::denied(format!("thread {} takes no replies", t.id)));
        }
        Ok(vec![Op::Reply { thread: t.id, text: p.text }])
    })
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
            return Err(ApiError::invalid(format!("thread {} is a dispatcher thread; only notifications are dismissed", t.id)));
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
            return Err(ApiError::invalid(format!("thread {} is a dispatcher thread, not a notification", t.id)));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::notify::{Level, Record};
    use crate::inbox::{store, Action, Reply, State, ThreadPut};

    fn store_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-api-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("inbox.toml")
    }

    fn action(id: &str) -> Action {
        Action { id: id.into(), label: id.to_uppercase(), ..Action::default() }
    }

    fn put(path: &Path, owner: &str, name: &str, key: &str, state: State) {
        let put = ThreadPut {
            key: key.into(),
            title: format!("{key} title"),
            state,
            reply: Some(Reply::default()),
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
        call(method, params, &Ctx { inbox: path })
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
        assert_eq!(by_id["reply"], json!({"placeholder": null}));
        assert_eq!(by_id["entries"][0]["kind"], "state");
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
        assert_eq!(code(run(&path, "inbox.form.submit", json!({}))), "unknown-method");
    }

    #[test]
    fn topics_parse_and_reject_unknown_ones() {
        assert_eq!(topics(json!({"topics":["inbox","instances"]})).unwrap(), [Topic::Inbox, Topic::Instances]);
        assert_eq!(topics(json!({"topics":["forwards"]})).unwrap_err().code, "invalid");
        assert_eq!(topics(Value::Null).unwrap_err().code, "invalid");
    }
}
