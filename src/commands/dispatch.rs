//! Host side of the dispatcher control API (docs/automations.md,
//! "Dispatchers"): authorize and carry out one decoded [`Request`] from a
//! dispatcher instance, answering a [`Response`]. The transport (a `CONTROL`
//! stream on the dispatcher's bridge) only decodes, calls [`handle`], and
//! encodes.
//!
//! Every request is checked against the config as it is *now*: the
//! dispatcher's sandbox (resolved from the config root recorded on its
//! instance) must declare `dispatcher`, `ensure` needs the target in its
//! `spawn` list, `stop`/`rm`/`done` only reach instances it owns, and `ensure` of a
//! new child respects `max-instances` (all owned children in state count,
//! stopped ones included; unset = `Dispatcher::DEFAULT_MAX_INSTANCES`).
//! Children can't be dispatchers: `ensure` of a sandbox declaring
//! `dispatcher` is denied whatever `spawn` says, and a request from a child
//! instance (old state) is denied, so dispatch can't recurse. `--env` names
//! on the control::denied_env list are denied. `ensure --env` on an existing
//! child replaces its saved env (`Instance::extra_env`) before (re)starting
//! it, so every later devsandbox exec there sees the new values. `done`
//! marks a child done (docs/inbox-threads.md, "Done instances"); an `ensure`
//! that reuses a done child clears the flag, since the dispatcher is putting
//! it back to work.
//!
//! Runs (`exec`, `run-ls|logs|wait`) live in the child: the host execs the
//! child's own helper (`devsbd run start|ls|logs|wait`, `devsbd/src/runs.rs`)
//! there, as the child's remote user in its workspace with its `remoteEnv` and
//! saved env (the `devsandbox exec` argv), output captured, and answers with what it
//! printed. Only owned, running children with a helper are reachable.
//!
//! `branches` (read-only, a sandbox in `spawn`) lists the branches checked out
//! in that sandbox's base repo, live from host `git worktree list`, each with
//! its holder (this dispatcher's child, another instance, the base checkout,
//! or a worktree devsandbox doesn't know), never a host path.
//!
//! `events`, `events-ack` and `thread-ls` (docs/inbox-threads.md) read the
//! shared Inbox store (`crate::inbox::store`) for the requester's own
//! `instance_id` only: another instance never sees or acks them. `events`
//! may long-poll up to [`MAX_WAIT`] on the bridge handler thread.
//! `events-follow` streams them instead, for the stream's life ([`follow`],
//! `follow.rs`): the one op that doesn't answer a single [`Response`].
//!
//! Children are ordinary instances named `<sandbox>-<key>`. Operations run as
//! `devsandbox -C <config root> run|start|rebuild|stop|rm …` subprocesses, not
//! in-process: the handler runs on the TUI's bridge worker and those commands
//! print (and stream docker/git output) to inherited stdio, which would land
//! on the alternate screen. Their output goes to
//! `<data>/devsandbox/logs/dispatch-<unix>-<op>.log`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::commands::run::{instance_at, parse_worktree_list};
use crate::config::{Config, Dispatcher, SandboxProperties};
use crate::devsbd::control::{self, Op, Request, Response, Status};
use crate::inbox::{ops as inbox_ops, store, Event};
use crate::runtime::{backend, bounded};
use crate::state::{Instance, State};

// The key rule lives in `control`, which the helper compiles too: `devsbd
// thread put|rm` checks its key before queuing, and both sides must agree.
pub use crate::devsbd::control::{valid_key, MAX_KEY};

/// Cap on a `run-wait`/`events` request's `timeout`, seconds (a `run-wait`
/// also holds a `docker exec`). Lives in `control` so the helper caps
/// `events --wait` with the same number.
pub use crate::devsbd::control::MAX_WAIT;

mod follow;
pub(crate) use follow::{follow_events, Followers};

/// How often a waiting `events` request re-checks the store's stamp. Writes
/// made in this process (the daemon's notify sink and acks) wake it at once
/// (`inbox_ops::wait_changed`); this fallback is for the other processes
/// still writing the store (dashboard ops, `devsandbox rm`).
const EVENTS_POLL: Duration = Duration::from_millis(500);

/// Wall-clock limit on one dispatch `devsandbox` subprocess (`ensure`,
/// `stop`, `rm`): it holds the host-wide control lock, so a wedged build or
/// git call must not hold it forever. Killed with its process group.
const DEVSANDBOX_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Wall-clock limit on one run-op exec in a child ([`Executor::exec_in`]).
const EXEC_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Wall-clock limit on one host git call of `branches` ([`Executor::git`]).
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Room over [`MAX_WAIT`] for a `run wait` exec, whose helper-side wait is
/// already capped at that.
const WAIT_SLACK: Duration = Duration::from_secs(30);

/// The `exec_in` timeout for helper argv `command`: `run wait` may block up
/// to [`MAX_WAIT`] by design, everything else gets [`EXEC_TIMEOUT`].
fn exec_timeout(command: &[String]) -> Duration {
    match command {
        [_, run, wait, ..] if run == "run" && wait == "wait" => Duration::from_secs(MAX_WAIT) + WAIT_SLACK,
        _ => EXEC_TIMEOUT,
    }
}

/// What an in-container command left behind (see [`Executor::exec_in`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Side effects of a request, injectable so the decisions are testable
/// without a runtime.
pub trait Executor {
    /// Container liveness, as `Backend::is_running`: `None` = no container.
    fn is_running(&self, container: &str) -> Result<Option<bool>, String>;
    /// Run `devsandbox -C <config_dir> <args…>`; `Err` is a short message
    /// for the response body.
    fn devsandbox(&mut self, config_dir: &Path, args: &[String]) -> Result<(), String>;
    /// Run `command` in `child`'s container as `devsandbox exec` would (user,
    /// workspace, `Instance::exec_env`), stdin null, output captured; `Err` only when it
    /// couldn't run at all.
    fn exec_in(&mut self, child: &Instance, command: &[String]) -> Result<ExecOutput, String>;
    /// After `ensure` created or (re)started child `name`: wait (bounded) for
    /// its ssh-agent bridge when an exec there gets `SSH_AUTH_SOCK` (relay
    /// mode with a host agent), so the dispatcher's next `exec` can use ssh.
    /// Why the relay isn't usable, if it isn't; never fails the op.
    fn wait_bridge(&mut self, name: &str) -> Option<String>;
    /// Replace state entry `name`'s `extra_env` with `env` and save. Called
    /// under the bridge's host-wide control lock, before the op's subprocess
    /// (which then loads the updated entry).
    fn save_env(&mut self, name: &str, env: &BTreeMap<String, String>) -> Result<(), String>;
    /// Set state entry `name`'s done flag (`Instance::done`) and save. Under
    /// the control lock like [`Executor::save_env`]: `done` is a
    /// state-writing op (`bridge::writes_state`).
    fn set_done(&mut self, name: &str, done: Option<u64>) -> Result<(), String>;
    /// Run host git (`host_git`) on `repo` with `args`, output captured;
    /// stdout on success, else a short message.
    fn git(&mut self, repo: &Path, args: &[String]) -> Result<String, String>;
    /// The Inbox store file (`inbox::store::path`) the event ops read and
    /// ack; a temp file in tests.
    fn inbox_path(&self) -> Result<PathBuf, String>;
}

/// Load state and the dispatcher's config, then [`handle_with`] the real
/// executor. `dispatcher_key` is the state key of the instance whose bridge
/// the request arrived on. Blocking (it waits for the subprocess). Called by
/// the host daemon's bridges' `CONTROL` handler
/// (`devsbd::bridge`), which is unix-only.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn handle(dispatcher_key: &str, req: &Request) -> Response {
    match load(dispatcher_key) {
        Ok((state, config)) => handle_with(&state, &config, dispatcher_key, req, &mut Subprocess),
        Err(resp) => resp,
    }
}

/// State, and the config root recorded on instance `key`, as [`handle`] and
/// [`follow`] need them.
fn load(key: &str) -> Result<(State, Config), Response> {
    let state = State::load().map_err(|e| failed(format!("{e:#}")))?;
    let config_dir = match state.instances.get(key) {
        None => return Err(denied(format!("`{key}` is not an instance"))),
        Some(owner) => match &owner.config_dir {
            Some(dir) => dir.clone(),
            None => return Err(no_config_dir(key)),
        },
    };
    let config = Config::load(&config_dir).map_err(|e| failed(format!("{e:#}")))?;
    Ok((state, config))
}

/// Serve an `events-follow` request from instance `key` on `out` (the
/// bridge's control stream) until the stream ends or a newer follower for
/// the same owner replaces it: authorized like every inbox op, then
/// [`follow_events`] against the process-wide [`Followers`]. A refusal is
/// one encoded [`Response`], as for any op. Blocking; reads only, so it
/// never takes the bridge's control lock.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn follow(key: &str, req: &Request, out: &mut dyn Write) {
    static FOLLOWERS: Followers = Followers::new();
    let resp = match load(key) {
        Ok((state, config)) => {
            match follow_with(&state, &config, key, req, &Subprocess, out, &FOLLOWERS, control::FOLLOW_PING) {
                Ok(()) => return,
                Err(resp) => resp,
            }
        }
        Err(resp) => resp,
    };
    let _ = out.write_all(control::encode_response(&resp).as_bytes());
}

/// The testable core of [`follow`]: `Err` is the refusal to answer instead
/// of a stream (nothing written yet); `Ok` once the stream ended.
#[allow(clippy::too_many_arguments)]
pub(crate) fn follow_with(
    state: &State,
    config: &Config,
    key: &str,
    req: &Request,
    exec: &dyn Executor,
    out: &mut dyn Write,
    followers: &Followers,
    ping: Duration,
) -> Result<(), Response> {
    if req.op != Op::Subscribe {
        return Err(usage(format!("`{}` doesn't stream", req.op.as_str())));
    }
    let (owner, _, _) = authorize(state, config, key, req)?;
    let path = exec.inbox_path().map_err(failed)?;
    follow_events(&path, &owner.instance_id, req.key.as_deref(), out, followers, ping)
}

/// Whether `key` is an instance whose sandbox declares `dispatcher`. Lets the
/// bridge skip its host-wide lock for requests `handle` will deny anyway, so a
/// child's lifecycle command calling `devsbd` can't stall behind the parent
/// op that is waiting for it. A dispatcher's child is never one (see
/// [`handle_with`]), even if its sandbox declares `dispatcher`.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn declares_dispatcher(key: &str) -> bool {
    current_sandbox(key).is_some_and(|(info, props)| is_dispatcher(&info, &props))
}

/// Whether `key` is an instance whose sandbox declares `inbox = true`: the
/// right to own Inbox threads (`thread put|rm|ls`, `events`). Unlike
/// [`declares_dispatcher`], a dispatcher's child may: it owns its own threads.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn declares_inbox(key: &str) -> bool {
    current_sandbox(key).is_some_and(|(_, props)| is_inbox_owner(&props))
}

/// [`declares_dispatcher`] over a loaded instance and its sandbox.
pub(crate) fn is_dispatcher(info: &Instance, props: &SandboxProperties) -> bool {
    info.dispatcher.is_none() && props.dispatcher.is_some()
}

/// [`declares_inbox`] over a loaded sandbox.
pub(crate) fn is_inbox_owner(props: &SandboxProperties) -> bool {
    props.inbox == Some(true)
}

/// Instance `key` and its sandbox as its recorded config root resolves it now;
/// `None` when any of that is missing or fails to load.
pub(crate) fn current_sandbox(key: &str) -> Option<(Instance, SandboxProperties)> {
    let mut state = State::load().ok()?;
    let info = state.instances.remove(key)?;
    let dir = info.config_dir.as_deref()?;
    let props = Config::load(dir).and_then(|c| c.resolve_sandbox(&info.sandbox)).ok()?.properties;
    Some((info, props))
}

/// The control ops on the requester's own Inbox threads, gated on
/// `inbox = true` rather than `dispatcher` (docs/inbox-redesign.md, _Ownership_).
fn is_inbox_op(op: Op) -> bool {
    matches!(op, Op::Events | Op::EventsAck | Op::Subscribe | Op::ThreadLs)
}

fn denied(msg: impl Into<String>) -> Response {
    Response::new(Status::Denied, msg)
}

fn usage(msg: impl Into<String>) -> Response {
    Response::new(Status::Usage, msg)
}

fn failed(msg: impl Into<String>) -> Response {
    Response::new(Status::Failed, msg)
}

fn no_config_dir(key: &str) -> Response {
    denied(format!(
        "`{key}` predates dispatcher support (no recorded config root); \
         `devsandbox rebuild --force {key}` records it"
    ))
}

/// The checks every request passes before its op runs: the requester exists
/// with a recorded config root, declares what the op needs (`inbox = true`
/// for inbox ops, `dispatcher` for child ops), and sent well-formed fields.
/// The requester, its config root, and its `dispatcher` declaration (`None`
/// only for inbox ops).
fn authorize<'a>(
    state: &'a State,
    config: &Config,
    dispatcher_key: &str,
    req: &Request,
) -> Result<(&'a Instance, &'a Path, Option<Dispatcher>), Response> {
    let Some(owner) = state.instances.get(dispatcher_key) else {
        return Err(denied(format!("`{dispatcher_key}` is not an instance")));
    };
    let Some(config_dir) = owner.config_dir.as_deref() else {
        return Err(no_config_dir(dispatcher_key));
    };
    let props = match config.resolve_sandbox(&owner.sandbox) {
        Ok(sandbox) => sandbox.properties,
        Err(e) => return Err(denied(format!("{e:#}"))),
    };
    // Inbox ops need `inbox = true` (a child may own threads too); child ops
    // need `dispatcher`. `decl` is only `None` for inbox ops.
    let decl = if is_inbox_op(req.op) {
        if !is_inbox_owner(&props) {
            return Err(denied(format!("`{dispatcher_key}` doesn't declare inbox = true")));
        }
        None
    } else {
        // Only state from before children of dispatcher sandboxes were refused.
        if owner.dispatcher.is_some() {
            return Err(denied(format!(
                "`{dispatcher_key}` is a dispatcher's child; children can't be dispatchers"
            )));
        }
        let Some(decl) = props.dispatcher else {
            return Err(denied(format!("sandbox `{}` does not declare `dispatcher`", owner.sandbox)));
        };
        Some(decl)
    };
    check_fields(req).map_err(usage)?;
    if let Some((k, _)) = req.env.iter().find(|(k, _)| control::denied_env(k)) {
        return Err(denied(format!("env `{k}` may not be set by a dispatcher")));
    }
    Ok((owner, config_dir, decl))
}

/// The pure core of [`handle`], over already-loaded `state` and `config`
/// (the config root recorded on the dispatcher instance).
pub(crate) fn handle_with(
    state: &State,
    config: &Config,
    dispatcher_key: &str,
    req: &Request,
    exec: &mut dyn Executor,
) -> Response {
    let (owner, config_dir, decl) = match authorize(state, config, dispatcher_key, req) {
        Ok(gate) => gate,
        Err(resp) => return resp,
    };
    let owner_id = owner.instance_id.as_str();

    match req.op {
        Op::Ls => ls(state, owner_id, exec),
        Op::Ensure => {
            let decl = decl.expect("child op");
            let sandbox = req.sandbox.as_deref().expect("checked by check_fields");
            let key = req.key.as_deref().expect("checked by check_fields");
            if !decl.may_spawn(sandbox) {
                return denied(format!(
                    "sandbox `{}` may not spawn `{sandbox}` (not in `dispatcher.spawn`)",
                    owner.sandbox
                ));
            }
            if !config.sandboxes.contains_key(sandbox) {
                return denied(format!("unknown sandbox `{sandbox}`"));
            }
            match config.resolve_sandbox(sandbox) {
                Ok(target) if target.properties.dispatcher.is_some() => {
                    return denied(format!(
                        "sandbox `{sandbox}` declares `dispatcher`; children can't be dispatchers"
                    ));
                }
                Ok(_) => {}
                Err(e) => return denied(format!("{e:#}")),
            }
            let name = child_name(sandbox, key);
            let child = state.instances.get(&name);
            let owned = children(state, owner_id).count();
            let action = ensure_action(&name, child, owner_id, sandbox, owned, decl.cap(), |c| {
                exec.is_running(c)
            });
            if let (Ok(_), Some(child)) = (&action, child) {
                if let Some(env) = replaced_env(child, &req.env) {
                    if let Err(e) = exec.save_env(&name, &env) {
                        return failed(e);
                    }
                }
                // Reusing a done child puts it back to work. Before the
                // subprocess, which loads (and a rebuild keeps) the flag.
                if child.done.is_some() {
                    if let Err(e) = exec.set_done(&name, None) {
                        return failed(e);
                    }
                }
            }
            let ensured = |action: &Ensure, note: Option<String>| {
                answer(&Ensured {
                    name: &name,
                    key,
                    sandbox,
                    state: ChildState::Running,
                    created: *action == Ensure::Run,
                    started: matches!(action, Ensure::Start | Ensure::Rebuild),
                    note: note.map(|why| format!("ssh-agent relay unavailable: {why}")),
                })
            };
            match action {
                Err(resp) => resp,
                Ok(Ensure::Nothing) => ensured(&Ensure::Nothing, None),
                Ok(action) => {
                    let args = ensure_args(&action, sandbox, &name, req, owner_id);
                    match exec.devsandbox(config_dir, &args) {
                        // `start` without a `postStartCommand` never waits
                        // for the bridge, and the daemon's poll may not have
                        // bridged the fresh container yet: wait here, so
                        // "ensured" means ssh works in the next `exec`.
                        Ok(()) => ensured(&action, exec.wait_bridge(&name)),
                        Err(e) => failed(e),
                    }
                }
            }
        }
        Op::Stop | Op::Rm => {
            let key = req.key.as_deref().expect("checked by check_fields");
            let name = match find_child(state, owner_id, key, req.sandbox.as_deref()) {
                Ok(name) => name,
                Err(resp) => return resp,
            };
            let args = vec![req.op.as_str().to_string(), name.clone()];
            match exec.devsandbox(config_dir, &args) {
                Ok(()) if req.op == Op::Stop => {
                    answer(&Stopped { name: &name, key, state: ChildState::Stopped })
                }
                Ok(()) => answer(&Removed { name: &name, key, removed: true }),
                Err(e) => failed(e),
            }
        }
        Op::Done => {
            let key = req.key.as_deref().expect("checked by check_fields");
            let name = match find_child(state, owner_id, key, req.sandbox.as_deref()) {
                Ok(name) => name,
                Err(resp) => return resp,
            };
            // Idempotent, keeping the original since: a dispatcher may repeat
            // it every pass.
            let marked = answer(&Marked { name: &name, key, done: true });
            if state.instances[&name].done.is_some() {
                return marked;
            }
            match exec.set_done(&name, Some(Instance::now())) {
                Ok(()) => marked,
                Err(e) => failed(e),
            }
        }
        Op::Exec | Op::RunLs | Op::RunLogs | Op::RunWait | Op::RunRm | Op::RunPrune => {
            let key = req.key.as_deref().expect("checked by check_fields");
            let name = match find_child(state, owner_id, key, req.sandbox.as_deref()) {
                Ok(name) => name,
                Err(resp) => return resp,
            };
            run_op(&name, &state.instances[&name], req, exec)
        }
        Op::Branches => {
            let decl = decl.expect("child op");
            let sandbox = req.sandbox.as_deref().expect("checked by check_fields");
            if !decl.may_spawn(sandbox) {
                return denied(format!(
                    "sandbox `{}` may not spawn `{sandbox}` (not in `dispatcher.spawn`)",
                    owner.sandbox
                ));
            }
            if !config.sandboxes.contains_key(sandbox) {
                return denied(format!("unknown sandbox `{sandbox}`"));
            }
            let target = match config.resolve_sandbox(sandbox) {
                Ok(target) => target,
                Err(e) => return failed(format!("{e:#}")),
            };
            // As `run` resolves it.
            let Some(folder) = target.folder() else {
                return failed(format!("sandbox `{sandbox}` has no `folder`"));
            };
            let Ok(base) = config_dir.join(folder).canonicalize() else {
                return failed(format!("sandbox folder `{folder}` does not exist"));
            };
            branches(state, owner_id, &base, req.ahead, exec)
        }
        // Served by `follow` on the bridge; never answered as one response.
        Op::Subscribe => usage("`events-follow` streams; send it on its own control stream"),
        Op::Events | Op::EventsAck | Op::ThreadLs => {
            let path = match exec.inbox_path() {
                Ok(path) => path,
                Err(e) => return failed(e),
            };
            match req.op {
                Op::Events => {
                    let timeout = req.timeout.unwrap_or(0).min(MAX_WAIT);
                    events(&path, owner_id, req.key.as_deref(), timeout)
                }
                Op::EventsAck => match inbox_ops::ack(&path, owner_id, &req.ack) {
                    Ok(n) => Response::new(Status::Ok, n.to_string()),
                    Err(e) => failed(format!("{e:#}")),
                },
                _ => thread_ls(&path, owner_id, req.feed),
            }
        }
    }
}

/// Room left in a response for the event ops' JSON: the response encoding
/// escapes backslashes and newlines again, so a body may double in size.
const EVENTS_BODY_CAP: usize = control::MAX_RESPONSE / 2 - 1024;

/// One `events` line: the module doc of `devsbd::control` is the format.
#[derive(Serialize)]
struct EventLine<'a> {
    id: &'a str,
    thread: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    form: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    answers: Option<&'a serde_json::Value>,
    at: String,
}

/// The `events` body for `pending` (oldest first): JSON lines, cut before
/// [`EVENTS_BODY_CAP`] — the rest is still pending and comes with the next
/// call, once the owner acked these.
fn events_body(pending: &[(String, Event)]) -> String {
    let mut body = String::new();
    for (thread, e) in pending {
        let Some(json) = event_json(thread, e) else { continue };
        if !body.is_empty() && body.len() + json.len() + 1 > EVENTS_BODY_CAP {
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&json);
    }
    body
}

/// Event `e` on `thread` as one `events` line (no newline); `None` only if
/// it doesn't serialize.
fn event_json(thread: &str, e: &Event) -> Option<String> {
    let line = EventLine {
        id: &e.id,
        thread,
        kind: e.kind.as_str(),
        action: e.action.as_deref(),
        text: e.text.as_deref(),
        message: e.message.as_deref(),
        form: e.form.as_deref(),
        answers: e.answers.as_ref(),
        at: crate::inbox::rfc3339(e.at),
    };
    serde_json::to_string(&line).ok()
}

/// `pending` narrowed to `thread`'s events, if one is given. An unknown
/// thread is just none: a dispatcher may filter before its first put lands.
fn on_thread(mut pending: Vec<(String, Event)>, thread: Option<&str>) -> Vec<(String, Event)> {
    if let Some(thread) = thread {
        pending.retain(|(t, _)| t == thread);
    }
    pending
}

/// `events`: `owner_id`'s pending events in the store at `path`, only
/// `thread`'s if given (so a wait ends on an event there, not on any). With
/// nothing pending and a `timeout`, wait on the handler thread, re-reading
/// the store only when this process wrote it or its stamp moved (checked
/// every [`EVENTS_POLL`]); an empty body on timeout. Reads only: no lock
/// beyond the store's own shared one, so a waiting `events` never holds up
/// a click being written.
fn events(path: &Path, owner_id: &str, thread: Option<&str>, timeout: u64) -> Response {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut seen = None;
    loop {
        // Stamp before reading: a write in between only costs one more read.
        // The generation too, so two writes inside one mtime tick still read.
        let generation = inbox_ops::generation();
        let stamp = Some((generation, inbox_ops::stamp(path)));
        if stamp != seen {
            seen = stamp;
            let pending = match inbox_ops::events(path, owner_id) {
                Ok(pending) => on_thread(pending, thread),
                Err(e) => return failed(format!("{e:#}")),
            };
            if !pending.is_empty() {
                return Response::new(Status::Ok, events_body(&pending));
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Response::new(Status::Ok, "");
        }
        inbox_ops::wait_changed(generation, left.min(EVENTS_POLL));
    }
}

/// One `thread-ls` row: the put body, plus the owner's messages with `feed`.
#[derive(serde::Serialize)]
struct ThreadLsRow<'a> {
    #[serde(flatten)]
    put: crate::inbox::ThreadPut,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<Vec<MessageRow<'a>>>,
}

/// An owner message as `thread ls --feed` lists it: `blocks` in the shape
/// `thread send` takes, so a dispatcher can compare or re-send them.
#[derive(serde::Serialize)]
struct MessageRow<'a> {
    id: &'a str,
    at: u64,
    blocks: &'a [crate::inbox::Block],
    edited: bool,
    withdrawn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    form: Option<FormRow<'a>>,
}

/// A message's form as `thread ls --feed` lists it: enough for a
/// dispatcher that lost its state to see what the user already answered.
#[derive(serde::Serialize)]
struct FormRow<'a> {
    id: &'a str,
    /// `open` | `submitted` | `withdrawn`.
    state: &'static str,
    /// When submitted: the answers, as the `submit` event carried them.
    #[serde(skip_serializing_if = "Option::is_none")]
    answers: Option<serde_json::Value>,
}

impl<'a> FormRow<'a> {
    fn of(r: &'a crate::inbox::FormRecord, blocks: &[crate::inbox::Block]) -> Self {
        use crate::inbox::{form, FormState};
        let answers = match &r.state {
            FormState::Submitted { answers, .. } => Some(match form::form_in(blocks) {
                Some(f) => form::answers_json(f, answers),
                None => form::partial_json(answers),
            }),
            _ => None,
        };
        FormRow { id: &r.id, state: r.state.as_str(), answers }
    }
}

/// The owner's messages in `t`'s feed, first-insert order, each with its
/// form's state. Replies, actions, submissions and markers are the user's
/// or the host's, not what the owner sent (a submission's answers are on
/// its message's `form`).
fn message_rows(t: &crate::inbox::Thread) -> Vec<MessageRow<'_>> {
    t.feed
        .iter()
        .filter_map(|i| match &i.kind {
            crate::inbox::ItemKind::Message { id, blocks, edited, withdrawn, form } => Some(MessageRow {
                id,
                at: i.at,
                blocks,
                edited: *edited,
                withdrawn: *withdrawn,
                form: form.as_ref().map(|r| FormRow::of(r, blocks)),
            }),
            _ => None,
        })
        .collect()
}

/// `thread-ls`: `owner_id`'s live threads as a JSON array of put bodies;
/// with `feed`, each with its `messages` too.
fn thread_ls(path: &Path, owner_id: &str, feed: bool) -> Response {
    let threads = match inbox_ops::threads(path, owner_id) {
        Ok(threads) => threads,
        Err(e) => return failed(format!("{e:#}")),
    };
    let puts: Vec<_> = threads
        .iter()
        .map(|t| ThreadLsRow { put: t.to_put(), messages: feed.then(|| message_rows(t)) })
        .collect();
    match serde_json::to_string(&puts) {
        Ok(json) if json.len() > EVENTS_BODY_CAP => failed(format!("{} threads: too large to list", puts.len())),
        Ok(json) => Response::new(Status::Ok, json),
        Err(e) => failed(e.to_string()),
    }
}

/// Which fields each op takes; the codec only checks syntax. Also validates
/// the key's charset and the run id's shape.
fn check_fields(req: &Request) -> Result<(), String> {
    let op = req.op.as_str();
    // Some(true) = required, Some(false) = forbidden, None = optional.
    let (sandbox, key) = match req.op {
        Op::Ls => (Some(false), Some(false)),
        Op::Ensure => (Some(true), Some(true)),
        Op::Branches => (Some(true), Some(false)),
        // Always the requester's own: nothing to name. `events` takes `key`
        // as its optional thread filter (`devsbd events --thread`).
        Op::Events | Op::Subscribe => (Some(false), None),
        Op::EventsAck | Op::ThreadLs => (Some(false), Some(false)),
        _ => (None, Some(true)),
    };
    let only = |ops: &[Op], optional: bool| match (ops.contains(&req.op), optional) {
        (true, true) => None,
        (true, false) => Some(true),
        (false, _) => Some(false),
    };
    let field = |name: &str, want: Option<bool>, has: bool| match want {
        Some(true) if !has => Err(format!("`{op}` needs `{name}`")),
        Some(false) if has => Err(format!("`{op}` takes no `{name}`")),
        _ => Ok(()),
    };
    field("sandbox", sandbox, req.sandbox.is_some())?;
    field("key", key, req.key.is_some())?;
    if req.op != Op::Ensure && (req.branch.is_some() || !req.env.is_empty()) {
        return Err(format!("`{op}` takes no `branch`/`env`"));
    }
    field("arg", only(&[Op::Exec], false), !req.argv.is_empty())?;
    field("id", only(&[Op::RunLogs, Op::RunWait, Op::RunRm], false), req.id.is_some())?;
    field("offset", only(&[Op::RunLogs], true), req.offset.is_some())?;
    field("timeout", only(&[Op::RunWait, Op::Events], true), req.timeout.is_some())?;
    field("ack", only(&[Op::EventsAck], false), !req.ack.is_empty())?;
    field("force", only(&[Op::RunRm], true), req.force)?;
    field("keep", only(&[Op::RunPrune], true), req.keep.is_some())?;
    field("ahead", only(&[Op::Branches], true), req.ahead)?;
    field("feed", only(&[Op::ThreadLs], true), req.feed)?;
    if let Some(key) = &req.key {
        if !valid_key(key) {
            return Err(format!(
                "bad key `{key}`: use lowercase letters, digits and `-`, starting with a \
                 letter or digit, at most {MAX_KEY} chars"
            ));
        }
    }
    if let Some(id) = req.id.as_deref().filter(|id| !control::valid_run_id(id)) {
        return Err(format!("bad run id `{id}`"));
    }
    if let Some(id) = req.ack.iter().find(|id| !control::valid_event_id(id)) {
        return Err(format!("bad event id `{id}`"));
    }
    // Not echoed: it may hold control characters or template syntax.
    if req.branch.as_deref().is_some_and(|b| !control::valid_branch(b)) {
        return Err(format!("bad branch: use {}", control::BRANCH_RULES));
    }
    Ok(())
}

/// The child helper's argv for a run op (after the checks in
/// [`check_fields`]).
fn helper_argv(req: &Request) -> Vec<String> {
    let s = |v: &str| v.to_string();
    let mut argv = vec![s(crate::devsbd::BIN), s("run")];
    let id = || req.id.clone().expect("checked by check_fields");
    match req.op {
        Op::Exec => {
            argv.extend([s("start"), s("--")]);
            argv.extend(req.argv.iter().cloned());
        }
        Op::RunLs => argv.push(s("ls")),
        Op::RunLogs => {
            argv.extend([s("logs"), id(), s("--offset"), req.offset.unwrap_or(0).to_string()]);
        }
        Op::RunWait => {
            argv.extend([s("wait"), id()]);
            if let Some(t) = req.timeout {
                argv.extend([s("--timeout"), t.min(MAX_WAIT).to_string()]);
            }
        }
        Op::RunRm => {
            argv.extend([s("rm"), id()]);
            if req.force {
                argv.push(s("--force"));
            }
        }
        Op::RunPrune => {
            argv.push(s("prune"));
            if let Some(keep) = req.keep {
                argv.extend([s("--keep"), keep.to_string()]);
            }
        }
        Op::Ensure | Op::Ls | Op::Stop | Op::Rm | Op::Done | Op::Branches | Op::Events | Op::EventsAck
        | Op::Subscribe | Op::ThreadLs => {
            unreachable!("not a run op")
        }
    }
    argv
}

/// Carry out a run op in child `name`: it must be running and have a helper.
/// The body is the helper's output (`run-logs`: a [`control::logs_body`]).
fn run_op(name: &str, child: &Instance, req: &Request, exec: &mut dyn Executor) -> Response {
    match exec.is_running(&child.container) {
        Ok(Some(true)) => {}
        Ok(Some(false)) => return failed(format!("child {name} is stopped; ensure it first")),
        Ok(None) => return failed(format!("child {name} has no container; ensure it first")),
        Err(e) => return failed(e),
    }
    if child.devsbd_arch.is_none() {
        return failed(format!("child {name} has no devsbd helper (installed on `start`)"));
    }
    let out = match exec.exec_in(child, &helper_argv(req)) {
        Ok(out) => out,
        Err(e) => return failed(e),
    };
    if out.code != 0 {
        let why = out.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        if out.code == control::EXIT_USAGE && why.starts_with("usage: devsbd version") {
            return failed(format!("child {name}'s devsbd predates runs; restart the child"));
        }
        // The host built this argv, so a usage error means an older helper.
        if out.code == control::EXIT_USAGE && matches!(req.op, Op::RunRm | Op::RunPrune) {
            return failed(format!(
                "child {name}'s devsbd predates `run rm`/`run prune`; restart the child ({why})"
            ));
        }
        return failed(format!("in {name}: {}", if why.is_empty() { "devsbd run failed" } else { why }));
    }
    let body = match req.op {
        Op::RunLogs => control::logs_body(req.offset.unwrap_or(0), &out.stdout),
        _ => String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
    };
    Response::new(Status::Ok, body)
}

pub fn child_name(sandbox: &str, key: &str) -> String {
    format!("{sandbox}-{key}")
}

/// The key a child was ensured with, from its name; `None` once renamed away
/// from `<sandbox>-<key>`.
fn child_key<'a>(name: &'a str, info: &Instance) -> Option<&'a str> {
    name.strip_prefix(info.sandbox.as_str())?.strip_prefix('-').filter(|k| valid_key(k))
}

/// `owner_id`'s children in state, by name.
fn children<'a>(
    state: &'a State,
    owner_id: &'a str,
) -> impl Iterator<Item = (&'a String, &'a Instance)> {
    state.instances.iter().filter(move |(_, i)| i.dispatcher.as_deref() == Some(owner_id))
}

/// `owner_id`'s children by the key they were ensured with, for callers that
/// need to name a child without being a control op. Pure over state: the
/// dashboard refreshes this when its snapshot moves, so the Inbox thread pane
/// can resolve a thread's `child` without reading `state.toml` every frame.
/// A child renamed away from `<sandbox>-<key>` has no key left and is skipped.
pub fn child_names(state: &State, owner_id: &str) -> BTreeMap<String, String> {
    children(state, owner_id)
        .filter_map(|(name, info)| child_key(name, info).map(|k| (k.to_string(), name.clone())))
        .collect()
}

/// The instance `owner_id`'s thread `child` key names, by the same rule as
/// the `stop`/`rm` control ops ([`find_child`]): never another dispatcher's
/// child, and `None` when the key is ambiguous across sandboxes, since the
/// pane would otherwise show (and step 5 act on) a guess.
pub fn resolve_child(state: &State, owner_id: &str, key: &str) -> Option<String> {
    find_child(state, owner_id, key, None).ok()
}

/// Every dispatcher's resolvable children, owner id → key → instance name.
/// The dashboard computes this off the UI thread with each snapshot, so the
/// thread pane resolves a `child` from memory.
pub fn thread_children(state: &State) -> BTreeMap<String, BTreeMap<String, String>> {
    let owners: std::collections::BTreeSet<&str> =
        state.instances.values().filter_map(|i| i.dispatcher.as_deref()).collect();
    owners
        .into_iter()
        .map(|owner| {
            let mut keys = child_names(state, owner);
            keys.retain(|key, _| resolve_child(state, owner, key).is_some());
            (owner.to_string(), keys)
        })
        .filter(|(_, keys)| !keys.is_empty())
        .collect()
}

/// What `ensure` must do for the child it names.
#[derive(Debug, PartialEq, Eq)]
enum Ensure {
    /// Not in state: create it.
    Run,
    /// Stopped: start it.
    Start,
    /// In state without a container: recreate it, keeping its worktree.
    Rebuild,
    /// Running: nothing to do.
    Nothing,
}

/// The `ensure` decision: ownership, cap, and the child's state. An existing
/// child is only (re)started; a changed `branch` is ignored (it applies at
/// creation). A given `env` replaces the saved one ([`replaced_env`], saved by
/// the caller whatever the action): every devsandbox exec applies it from then
/// on, the container's own env (PID 1, running processes) only changes on
/// `rebuild` — which the containerless case does, from the saved env.
/// `liveness` is only consulted for an owned child.
fn ensure_action(
    name: &str,
    child: Option<&Instance>,
    owner_id: &str,
    sandbox: &str,
    owned: usize,
    max: u32,
    liveness: impl FnOnce(&str) -> Result<Option<bool>, String>,
) -> Result<Ensure, Response> {
    let Some(child) = child else {
        if owned >= max as usize {
            return Err(denied(format!(
                "`max-instances` reached ({owned}); `rm` a child first"
            )));
        }
        return Ok(Ensure::Run);
    };
    if child.dispatcher.as_deref() != Some(owner_id) {
        return Err(denied(format!("`{name}` exists and is not this dispatcher's child")));
    }
    if child.sandbox != sandbox {
        return Err(denied(format!(
            "that name is taken by this dispatcher's child of sandbox `{}`",
            child.sandbox
        )));
    }
    match liveness(&child.container) {
        Ok(Some(true)) => Ok(Ensure::Nothing),
        Ok(Some(false)) => Ok(Ensure::Start),
        Ok(None) => Ok(Ensure::Rebuild),
        Err(e) => Err(failed(e)),
    }
}

/// The saved env an `ensure` of existing `child` leaves: exactly `given`
/// (replace, not merge, so a dispatcher can drop a variable; last value wins
/// on a duplicate key, as with `-e`). `None` = keep the saved env: no env
/// given, or the same set.
fn replaced_env(child: &Instance, given: &[(String, String)]) -> Option<BTreeMap<String, String>> {
    if given.is_empty() {
        return None;
    }
    let env: BTreeMap<String, String> = given.iter().cloned().collect();
    (env != child.extra_env).then_some(env)
}

/// `devsandbox` argv (after `-C <dir>`) carrying out `action`.
fn ensure_args(action: &Ensure, sandbox: &str, name: &str, req: &Request, owner: &str) -> Vec<String> {
    let s = |v: &str| v.to_string();
    match action {
        Ensure::Run => {
            let mut args = vec![s("run"), s(sandbox), s("--name"), s(name)];
            if let Some(branch) = &req.branch {
                args.extend([s("--branch"), branch.clone()]);
            }
            for (k, v) in &req.env {
                args.extend([s("--env"), format!("{k}={v}")]);
            }
            args.extend([s("--dispatcher"), s(owner)]);
            args
        }
        Ensure::Start => vec![s("start"), s(name)],
        Ensure::Rebuild => vec![s("rebuild"), s(name)],
        Ensure::Nothing => Vec::new(),
    }
}

/// Resolve `stop`/`rm`/`done`'s `key` (optionally narrowed by `sandbox`) to one of
/// `owner_id`'s children.
fn find_child(
    state: &State,
    owner_id: &str,
    key: &str,
    sandbox: Option<&str>,
) -> Result<String, Response> {
    let matches = |name: &str, info: &Instance| {
        child_key(name, info) == Some(key) && sandbox.is_none_or(|s| s == info.sandbox)
    };
    let mine: Vec<&String> =
        children(state, owner_id).filter(|(n, i)| matches(n, i)).map(|(n, _)| n).collect();
    match mine.as_slice() {
        [one] => Ok((*one).clone()),
        [] => {
            let foreign = state.instances.iter().find(|(n, i)| matches(n, i));
            Err(match foreign {
                Some((name, _)) => denied(format!("`{name}` is not this dispatcher's child")),
                None => failed(format!("no child with key `{key}`")),
            })
        }
        many => {
            let names: Vec<&str> = many.iter().map(|n| n.as_str()).collect();
            Err(usage(format!(
                "key `{key}` matches {}; pass the sandbox",
                names.join(", ")
            )))
        }
    }
}

/// A child op's answer as one compact JSON line, so a dispatcher never
/// parses prose (docs/automations.md, *Control API*).
fn answer(body: &impl Serialize) -> Response {
    match serde_json::to_string(body) {
        Ok(json) => Response::new(Status::Ok, json),
        Err(e) => failed(e.to_string()),
    }
}

/// The `ensure` answer. `created`: the child didn't exist (`run`);
/// `started`: it existed but wasn't up (`start`, or `rebuild` without a
/// container). Both false: it was already running. `note`: the child is up
/// but its ssh-agent relay isn't ([`Executor::wait_bridge`]); omitted when
/// fine.
#[derive(Serialize)]
struct Ensured<'a> {
    name: &'a str,
    key: &'a str,
    sandbox: &'a str,
    state: ChildState,
    created: bool,
    started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// The `stop` answer.
#[derive(Serialize)]
struct Stopped<'a> {
    name: &'a str,
    key: &'a str,
    state: ChildState,
}

/// The `rm` answer.
#[derive(Serialize)]
struct Removed<'a> {
    name: &'a str,
    key: &'a str,
    removed: bool,
}

/// The `done` answer (also when it already was).
#[derive(Serialize)]
struct Marked<'a> {
    name: &'a str,
    key: &'a str,
    done: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum ChildState {
    Running,
    Stopped,
    Missing,
}

/// One `ls` entry.
#[derive(Debug, Serialize)]
struct ChildRow<'a> {
    name: &'a str,
    sandbox: &'a str,
    key: Option<&'a str>,
    state: ChildState,
    branch: Option<&'a str>,
    /// Marked done (`Instance::done`): the dispatcher may evict it.
    done: bool,
}

fn ls(state: &State, owner_id: &str, exec: &mut dyn Executor) -> Response {
    let mut rows = Vec::new();
    for (name, info) in children(state, owner_id) {
        let live = match exec.is_running(&info.container) {
            Ok(Some(true)) => ChildState::Running,
            Ok(Some(false)) => ChildState::Stopped,
            Ok(None) => ChildState::Missing,
            Err(e) => return failed(e),
        };
        rows.push(ChildRow {
            name,
            sandbox: &info.sandbox,
            key: child_key(name, info),
            state: live,
            branch: info.branch.as_deref(),
            done: info.done.is_some(),
        });
    }
    match serde_json::to_string_pretty(&rows) {
        Ok(json) => Response::new(Status::Ok, json),
        Err(e) => failed(e.to_string()),
    }
}

/// Who has a `branches` row's branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Holder {
    /// A worktree of one of this dispatcher's children.
    Child,
    /// A worktree of any other instance, from any config root.
    Instance,
    /// The base checkout itself.
    Base,
    /// A worktree devsandbox doesn't know.
    External,
    /// (`ahead` only) In no worktree, with commits `origin` doesn't have.
    Local,
}

/// One `branches` entry. No path: host paths stay on the host.
#[derive(Debug, Serialize)]
struct BranchRow {
    branch: String,
    holder: Holder,
    /// The holding instance's name (`child`, `instance`).
    #[serde(skip_serializing_if = "Option::is_none")]
    instance: Option<String>,
    /// Commits on the local branch not on `origin/<branch>`; `None` without
    /// `ahead`, or when `origin/<branch>` doesn't exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    ahead: Option<u64>,
}

/// The rows for `git worktree list --porcelain` output: one per worktree
/// with a branch, detached and prunable ones left out. A worktree some
/// instance's folder is reports that instance, even the base checkout.
fn holder_rows(state: &State, owner_id: &str, porcelain: &str) -> Vec<BranchRow> {
    parse_worktree_list(porcelain)
        .into_iter()
        .enumerate()
        .filter(|(_, w)| !w.prunable)
        .filter_map(|(i, w)| {
            let branch = w.branch?;
            let instance = instance_at(state, &w.path);
            let holder = match &instance {
                Some(name) if state.instances[name].dispatcher.as_deref() == Some(owner_id) => Holder::Child,
                Some(_) => Holder::Instance,
                // Git always lists the main worktree first.
                None if i == 0 => Holder::Base,
                None => Holder::External,
            };
            Some(BranchRow { branch, holder, instance, ahead: None })
        })
        .collect()
}

/// `for-each-ref --format=%(refname) refs/heads refs/remotes/origin` output
/// → (local branches, branches on `origin`). `origin/HEAD` is not a branch.
fn split_refs(refs: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut local, mut pushed) = (BTreeSet::new(), BTreeSet::new());
    for line in refs.lines().map(str::trim) {
        if let Some(b) = line.strip_prefix("refs/heads/") {
            local.insert(b.to_string());
        } else if let Some(b) = line.strip_prefix("refs/remotes/origin/").filter(|b| *b != "HEAD") {
            pushed.insert(b.to_string());
        }
    }
    (local, pushed)
}

/// The `branches` op on base repo `base` (see [`Holder`]). With `ahead`,
/// one `rev-list` per local branch that has an `origin` counterpart.
fn branches(state: &State, owner_id: &str, base: &Path, ahead: bool, exec: &mut dyn Executor) -> Response {
    let s = |v: &str| v.to_string();
    let porcelain = match exec.git(base, &[s("worktree"), s("list"), s("--porcelain")]) {
        Ok(out) => out,
        Err(e) => return failed(e),
    };
    let mut rows = holder_rows(state, owner_id, &porcelain);
    if ahead {
        let refs_args = [s("for-each-ref"), s("--format=%(refname)"), s("refs/heads"), s("refs/remotes/origin")];
        let refs = match exec.git(base, &refs_args) {
            Ok(out) => out,
            Err(e) => return failed(e),
        };
        let (local, pushed) = split_refs(&refs);
        let mut counts = BTreeMap::new();
        for b in local.intersection(&pushed) {
            // Full refs: a branch name can't read as an option.
            let range = format!("refs/remotes/origin/{b}..refs/heads/{b}");
            let out = match exec.git(base, &[s("rev-list"), s("--count"), range]) {
                Ok(out) => out,
                Err(e) => return failed(e),
            };
            match out.trim().parse::<u64>() {
                Ok(n) => counts.insert(b.as_str(), n),
                Err(_) => return failed(format!("`git rev-list --count` printed `{}`", out.trim())),
            };
        }
        for row in &mut rows {
            row.ahead = counts.get(row.branch.as_str()).copied();
        }
        let held: BTreeSet<String> = rows.iter().map(|r| r.branch.clone()).collect();
        for (b, n) in counts {
            if n > 0 && !held.contains(b) {
                rows.push(BranchRow { branch: b.to_string(), holder: Holder::Local, instance: None, ahead: Some(n) });
            }
        }
    }
    rows.sort_by(|a, b| a.branch.cmp(&b.branch));
    match serde_json::to_string_pretty(&rows) {
        Ok(json) => Response::new(Status::Ok, json),
        Err(e) => failed(e.to_string()),
    }
}

/// The real executor: the runtime for liveness, a quiet `devsandbox`
/// subprocess for everything else.
struct Subprocess;

/// Lines of the log quoted back in a failure response.
const TAIL_LINES: usize = 5;

impl Executor for Subprocess {
    fn is_running(&self, container: &str) -> Result<Option<bool>, String> {
        backend().is_running(container).map_err(|e| format!("{e:#}"))
    }

    fn devsandbox(&mut self, config_dir: &Path, args: &[String]) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot find devsandbox: {e}"))?;
        let op = args.first().map(String::as_str).unwrap_or("?");
        let path = log_path(op).ok_or("cannot create the dispatch log")?;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let _ = writeln!(log, "$ devsandbox -C {} {}", config_dir.display(), args.join(" "));
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let mut timeout_log = log.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(exe);
        // Own process group, so a timeout kills docker/git grandchildren too.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd
            .arg("-C")
            .arg(config_dir)
            .args(args)
            // Null stdin: every prompt (`rm`'s delete-branch `confirm`,
            // ambiguous-name `pick`) takes its non-TTY default. Git would
            // still ask for credentials on the controlling terminal — the
            // TUI's — so forbid that too.
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err)
            .env("GIT_TERMINAL_PROMPT", "0")
            // Pin the child to this process's backend rather than letting it
            // re-run default detection (macOS PATH probe for `docker`).
            .env(crate::runtime::RUNTIME_ENV, backend().name())
            .spawn()
            .map_err(|e| format!("cannot run devsandbox: {e}"))?;
        let waited = bounded::wait_until(&mut child, Instant::now() + DEVSANDBOX_TIMEOUT, || false);
        let status = match waited {
            Ok(Some(status)) => status,
            Ok(None) => {
                bounded::kill_group(&mut child);
                let why = format!("timed out after {}", bounded::human(DEVSANDBOX_TIMEOUT));
                let _ = writeln!(timeout_log, "devsandbox: {why}, killed");
                return Err(format!("`devsandbox {op}` {why}; log: {}", path.display()));
            }
            Err(e) => {
                bounded::kill_group(&mut child);
                return Err(format!("cannot wait for devsandbox: {e}"));
            }
        };
        if status.success() {
            return Ok(());
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        Err(format!(
            "`devsandbox {op}` failed ({status}); log: {}\n{}",
            path.display(),
            tail(&text, TAIL_LINES)
        ))
    }

    fn exec_in(&mut self, child: &Instance, command: &[String]) -> Result<ExecOutput, String> {
        let args = crate::commands::exec::exec_argv(child, false, false, command);
        let bin = backend().bin();
        // Captured, never inherited: this runs on the TUI's bridge worker.
        // Bounded: the child's helper (container-controlled) picks the size
        // and duration of its output.
        let cap = control::MAX_RESPONSE;
        let out = bounded::run(Command::new(bin).args(&args).stdin(Stdio::null()), cap, exec_timeout(command))
            .map_err(|e| match e {
                bounded::Error::Io(e) => format!("cannot run {bin}: {e}"),
                e => format!("`devsbd {}` in the child {e}", command.get(1..3).unwrap_or_default().join(" ")),
            })?;
        if out.truncated {
            return Err(format!("`devsbd` output in the child is longer than {cap} bytes"));
        }
        Ok(ExecOutput {
            code: out.status.code().unwrap_or(1),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn wait_bridge(&mut self, name: &str) -> Option<String> {
        #[cfg(unix)]
        {
            // Reloaded: `run` just created the entry, `start` recorded the helper.
            let state = match State::load() {
                Ok(state) => state,
                Err(e) => return Some(format!("{e:#}")),
            };
            let child = state.instances.get(name)?;
            // The gate `exec_in`'s `SSH_AUTH_SOCK` injection uses.
            if !(crate::devsbd::relay_mode(child) && crate::commands::exec::has_host_agent()) {
                return None;
            }
            // This runs in the daemon, which this reaches over its own
            // socket. No agent to report: its own `$SSH_AUTH_SOCK` is already
            // its last candidate, and reporting it would rank it above the
            // clients' fresher ones.
            crate::commands::exec::AgentRelay::wait(name, None).1
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            None
        }
    }

    fn save_env(&mut self, name: &str, env: &BTreeMap<String, String>) -> Result<(), String> {
        let mut state = State::load().map_err(|e| format!("{e:#}"))?;
        let info = state
            .instances
            .get_mut(name)
            .ok_or_else(|| format!("`{name}` vanished from state"))?;
        info.extra_env = env.clone();
        state.save().map_err(|e| format!("{e:#}"))
    }

    fn set_done(&mut self, name: &str, done: Option<u64>) -> Result<(), String> {
        let mut state = State::load().map_err(|e| format!("{e:#}"))?;
        let info = state
            .instances
            .get_mut(name)
            .ok_or_else(|| format!("`{name}` vanished from state"))?;
        info.done = done;
        state.save().map_err(|e| format!("{e:#}"))
    }

    fn git(&mut self, repo: &Path, args: &[String]) -> Result<String, String> {
        let mut cmd = crate::commands::run::host_git(repo).map_err(|e| format!("{e:#}"))?;
        // Captured, never inherited: this runs on the TUI's bridge worker.
        // Bounded like `exec_in`: a wedged git must not pin the handler.
        let cap = control::MAX_RESPONSE;
        let out = bounded::run(cmd.args(args).stdin(Stdio::null()), cap, GIT_TIMEOUT).map_err(|e| match e {
            bounded::Error::Io(e) => format!("cannot run git: {e}"),
            e => format!("`git {}` {e}", args.first().map(String::as_str).unwrap_or("?")),
        })?;
        if out.truncated {
            return Err(format!("`git` output is longer than {cap} bytes"));
        }
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let why = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            let verb = args.first().map(String::as_str).unwrap_or("?");
            return Err(format!("`git {verb}` failed ({}): {why}", out.status));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn inbox_path(&self) -> Result<PathBuf, String> {
        store::path().map_err(|e| format!("{e:#}"))
    }
}

/// `<data>/devsandbox/logs/dispatch-<unix>-<op>.log` (next to `state.toml`,
/// like the TUI's failed-command logs), its dir created.
fn log_path(op: &str) -> Option<PathBuf> {
    let dir = State::path().ok()?.parent()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("dispatch-{}-{op}.log", Instance::now())))
}

/// The last `n` non-empty lines of `text`.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const CONFIG: &str = r#"
[sandbox.disp]
folder = "."
dispatcher = { spawn = ["web", "any"], max-instances = 2 }
inbox = true

[sandbox.any]
folder = "."
dispatcher = { spawn = ["*"] }
inbox = true

[sandbox.owner]
folder = "."
inbox = true

[sandbox.plain]
folder = "."

[sandbox.web]
folder = "."

[sandbox.api]
folder = "."
"#;

    fn inst(sandbox: &str, id: &str, owner: Option<&str>) -> Instance {
        Instance {
            sandbox: sandbox.into(),
            instance_id: id.into(),
            project: "proj".into(),
            container: format!("devsandbox-{id}"),
            folder: format!("/w/{id}").into(),
            base_folder: "/w".into(),
            worktree: None,
            branch: owner.map(|_| format!("sandbox/{id}")),
            branch_created: true,
            folders: Vec::new(),
            shell_history: None,
            workspace: "/workspaces/w".into(),
            workspace_file: None,
            remote_env: BTreeMap::new(),
            remote_user: None,
            ssh_auth_sock: None,
            devsbd_arch: None,
            volumes: Vec::new(),
            dispatcher: owner.map(str::to_string),
            config_dir: Some("/cfg".into()),
            extra_env: Default::default(),
            forwarded_ports: Default::default(),
            done: None,
            created_unix: 0,
        }
    }

    /// Dispatchers `d` (sandbox `disp`), `a` (`any`), plain `p`; `d` owns
    /// `web-one` (running) and `web-two` (stopped); `web-other` belongs to
    /// `a`; `web-mine` is a user instance.
    fn state() -> State {
        let mut state = State::default();
        for (key, sandbox, owner) in [
            ("d", "disp", None),
            ("a", "any", None),
            ("p", "plain", None),
            ("web-one", "web", Some("d")),
            ("web-two", "web", Some("d")),
            ("web-other", "web", Some("a")),
            ("web-mine", "web", None),
        ] {
            state.instances.insert(key.into(), inst(sandbox, key, owner));
        }
        state
    }

    /// Records calls; `running`: container -> liveness (absent = no container).
    /// `exec_in` answers `helper` and records `(container, argv)` in `execs`;
    /// `save_env` records `(name, env)` in `saved`; `git` answers `git_out`
    /// by argv (absent = an error) and records `(repo, argv)` in `gits`.
    #[derive(Default)]
    struct Fake {
        running: BTreeMap<String, bool>,
        calls: Vec<Vec<String>>,
        fail: bool,
        helper: ExecOutput,
        execs: Vec<(String, Vec<String>)>,
        saved: Vec<(String, BTreeMap<String, String>)>,
        /// `set_done` calls, `(name, done)`.
        dones: Vec<(String, Option<u64>)>,
        git_out: BTreeMap<Vec<String>, String>,
        gits: Vec<(PathBuf, Vec<String>)>,
        /// The Inbox store the event ops use; `None` = the ops fail.
        inbox: Option<PathBuf>,
        /// `wait_bridge` calls, by child name.
        bridge_waits: Vec<String>,
        /// What `wait_bridge` answers.
        bridge_note: Option<String>,
    }

    impl Fake {
        fn new() -> Fake {
            let mut fake = Fake::default();
            fake.running.insert("devsandbox-web-one".into(), true);
            fake.running.insert("devsandbox-web-two".into(), false);
            fake
        }
    }

    impl Executor for Fake {
        fn is_running(&self, container: &str) -> Result<Option<bool>, String> {
            Ok(self.running.get(container).copied())
        }
        fn devsandbox(&mut self, config_dir: &Path, args: &[String]) -> Result<(), String> {
            assert_eq!(config_dir, Path::new("/cfg"));
            self.calls.push(args.to_vec());
            if self.fail { Err("boom".into()) } else { Ok(()) }
        }
        fn exec_in(&mut self, child: &Instance, command: &[String]) -> Result<ExecOutput, String> {
            self.execs.push((child.container.clone(), command.to_vec()));
            if self.fail { Err("no docker".into()) } else { Ok(self.helper.clone()) }
        }
        fn wait_bridge(&mut self, name: &str) -> Option<String> {
            self.bridge_waits.push(name.to_string());
            self.bridge_note.clone()
        }
        fn save_env(&mut self, name: &str, env: &BTreeMap<String, String>) -> Result<(), String> {
            self.saved.push((name.to_string(), env.clone()));
            Ok(())
        }
        fn set_done(&mut self, name: &str, done: Option<u64>) -> Result<(), String> {
            self.dones.push((name.to_string(), done));
            if self.fail { Err("boom".into()) } else { Ok(()) }
        }
        fn git(&mut self, repo: &Path, args: &[String]) -> Result<String, String> {
            self.gits.push((repo.to_path_buf(), args.to_vec()));
            self.git_out.get(args).cloned().ok_or_else(|| format!("no canned `git {}`", args.join(" ")))
        }
        fn inbox_path(&self) -> Result<PathBuf, String> {
            self.inbox.clone().ok_or_else(|| "no inbox in this test".into())
        }
    }

    fn req(op: Op, sandbox: Option<&str>, key: Option<&str>) -> Request {
        Request {
            sandbox: sandbox.map(str::to_string),
            key: key.map(str::to_string),
            ..Request::new(op)
        }
    }

    fn call(state: &State, who: &str, r: &Request, fake: &mut Fake) -> Response {
        let config = Config::parse(CONFIG).unwrap();
        handle_with(state, &config, who, r, fake)
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The JSON answer of an `ensure` of `<sandbox>-<key>`.
    fn ensured(sandbox: &str, key: &str, created: bool, started: bool) -> Response {
        let body = format!(
            r#"{{"name":"{sandbox}-{key}","key":"{key}","sandbox":"{sandbox}","state":"running","created":{created},"started":{started}}}"#
        );
        Response::new(Status::Ok, body)
    }

    /// The JSON answer of `stop`/`rm`/`done` on child `name`.
    fn answered(op: Op, name: &str, key: &str) -> Response {
        let fact = match op {
            Op::Stop => r#""state":"stopped""#,
            Op::Rm => r#""removed":true"#,
            Op::Done => r#""done":true"#,
            _ => unreachable!(),
        };
        Response::new(Status::Ok, format!(r#"{{"name":"{name}","key":"{key}",{fact}}}"#))
    }

    #[test]
    fn authorization_matrix() {
        let s = state();
        let ensure = |sb: &str| req(Op::Ensure, Some(sb), Some("new"));
        let cases: &[(&str, Request, Status, &str)] = &[
            // Not a dispatcher / not an instance at all.
            ("p", ensure("web"), Status::Denied, "does not declare `dispatcher`"),
            ("p", req(Op::Ls, None, None), Status::Denied, "does not declare"),
            ("ghost", ensure("web"), Status::Denied, "not an instance"),
            // Not in `spawn`, even though it exists.
            ("d", ensure("api"), Status::Denied, "may not spawn `api`"),
            // `*` allows any sandbox of the root, but only existing ones.
            ("a", ensure("api"), Status::Ok, "api-new"),
            ("a", ensure("nope"), Status::Denied, "unknown sandbox `nope`"),
            // Children can't be dispatchers: not via `*` (itself or another), not
            // when named explicitly.
            ("a", ensure("any"), Status::Denied, "children can't be dispatchers"),
            ("a", ensure("disp"), Status::Denied, "children can't be dispatchers"),
            ("d", ensure("any"), Status::Denied, "children can't be dispatchers"),
            // Name taken by a foreign instance (user-made or another dispatcher's).
            ("d", req(Op::Ensure, Some("web"), Some("mine")), Status::Denied, "not this dispatcher's"),
            ("d", req(Op::Ensure, Some("web"), Some("other")), Status::Denied, "not this dispatcher's"),
            // stop/rm only reach owned children.
            ("d", req(Op::Stop, Some("web"), Some("other")), Status::Denied, "`web-other` is not"),
            ("d", req(Op::Rm, None, Some("mine")), Status::Denied, "`web-mine` is not"),
            ("a", req(Op::Rm, None, Some("one")), Status::Denied, "`web-one` is not"),
            ("d", req(Op::Stop, None, Some("zzz")), Status::Failed, "no child with key `zzz`"),
        ];
        for (who, r, status, needle) in cases {
            let mut fake = Fake::new();
            let resp = call(&s, who, r, &mut fake);
            assert_eq!(resp.status, *status, "{who} {r:?}: {resp:?}");
            assert!(resp.body.contains(needle), "{who} {r:?}: {resp:?}");
            if *status != Status::Ok {
                assert!(fake.calls.is_empty(), "denied requests run nothing: {r:?}");
            }
        }
    }

    /// A child of a dispatcher sandbox (only possible in old state) can't
    /// dispatch, whatever the op.
    #[test]
    fn requests_from_a_child_are_denied() {
        let mut s = state();
        s.instances.insert("any-kid".into(), inst("any", "any-kid", Some("a")));
        for r in [req(Op::Ls, None, None), req(Op::Ensure, Some("web"), Some("x")), req(Op::Rm, None, Some("one"))] {
            let mut fake = Fake::new();
            let resp = call(&s, "any-kid", &r, &mut fake);
            assert_eq!(resp.status, Status::Denied, "{r:?}: {resp:?}");
            assert!(resp.body.contains("children can't be dispatchers"), "{resp:?}");
            assert!(fake.calls.is_empty() && fake.execs.is_empty());
        }
    }

    #[test]
    fn denied_env_names_are_refused() {
        // Room under `d`'s cap, so only the env can deny.
        let mut s = state();
        s.instances.remove("web-two");
        for name in ["PATH", "LD_PRELOAD", "path", "GIT_DIR", "PYTHONPATH", "BASH_ENV"] {
            let mut r = req(Op::Ensure, Some("web"), Some("new"));
            r.env = vec![("OK".into(), "1".into()), (name.into(), "/tmp/p".into())];
            let mut fake = Fake::new();
            let resp = call(&s, "d", &r, &mut fake);
            assert_eq!(resp.status, Status::Denied, "{name}: {resp:?}");
            assert!(resp.body.contains(&format!("`{name}`")), "names the variable: {resp:?}");
            assert!(fake.calls.is_empty());
        }
    }

    #[test]
    fn missing_config_dir_or_sandbox_is_denied() {
        let mut s = state();
        s.instances.get_mut("d").unwrap().config_dir = None;
        let resp = call(&s, "d", &req(Op::Ls, None, None), &mut Fake::new());
        assert_eq!(resp.status, Status::Denied);
        assert!(resp.body.contains("rebuild --force d"), "{resp:?}");
        let mut s = state();
        s.instances.get_mut("d").unwrap().sandbox = "gone".into();
        let resp = call(&s, "d", &req(Op::Ls, None, None), &mut Fake::new());
        assert_eq!(resp.status, Status::Denied);
        assert!(resp.body.contains("unknown sandbox `gone`"), "{resp:?}");
    }

    #[test]
    fn ensure_per_child_state() {
        let mut fake = Fake::new();
        let mut s = state();
        // Room for one more under the cap: drop `web-two`.
        s.instances.remove("web-two");
        let mut r = req(Op::Ensure, Some("web"), Some("pr-1"));
        r.branch = Some("feat/x".into());
        r.env = vec![("A".into(), "1".into())];
        let resp = call(&s, "d", &r, &mut fake);
        assert_eq!(resp, ensured("web", "pr-1", true, false));
        assert_eq!(
            fake.calls,
            vec![argv(&[
                "run", "web", "--name", "web-pr-1", "--branch", "feat/x", "--env", "A=1",
                "--dispatcher", "d",
            ])]
        );

        // Running: nothing, still the answer.
        let s = state();
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("one")), &mut fake);
        assert_eq!(resp, ensured("web", "one", false, false));
        assert!(fake.calls.is_empty());
        assert!(fake.saved.is_empty(), "no env given: saved env kept");

        // Stopped: start (the branch of an existing child is ignored).
        let resp = call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(resp, ensured("web", "two", false, true));
        assert_eq!(fake.calls, vec![argv(&["start", "web-two"])]);

        // No container: rebuild, keeping the worktree.
        let mut fake = Fake::new();
        fake.running.remove("devsandbox-web-two");
        let resp = call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(resp, ensured("web", "two", false, true));
        assert_eq!(fake.calls, vec![argv(&["rebuild", "web-two"])]);

        // A failing subprocess is Failed with its message.
        let mut fake = Fake { fail: true, ..Fake::new() };
        let resp = call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(resp, Response::new(Status::Failed, "boom"));
        assert!(fake.bridge_waits.is_empty(), "no child came up: no bridge wait");
    }

    /// An `ensure` that brought a child up answers only once its ssh-agent
    /// bridge was waited for, so an `exec` right after can use ssh; a relay
    /// that isn't ready is a `note`, not a failure. An already running child
    /// waits for nothing.
    #[test]
    fn ensure_waits_for_the_bridge_of_a_child_it_brought_up() {
        let mut s = state();
        s.instances.remove("web-two");
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("pr-1")), &mut fake);
        assert_eq!(resp, ensured("web", "pr-1", true, false));
        assert_eq!(fake.bridge_waits, vec!["web-pr-1".to_string()], "created");

        let s = state();
        for running in [Some(false), None] {
            let mut fake = Fake::new();
            match running {
                Some(r) => fake.running.insert("devsandbox-web-two".into(), r),
                None => fake.running.remove("devsandbox-web-two"),
            };
            let resp = call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
            assert_eq!(resp, ensured("web", "two", false, true), "{running:?}");
            assert_eq!(fake.bridge_waits, vec!["web-two".to_string()], "{running:?}");
        }

        let mut fake = Fake::new();
        call(&s, "d", &req(Op::Ensure, Some("web"), Some("one")), &mut fake);
        assert!(fake.bridge_waits.is_empty(), "already running");

        let mut fake = Fake { bridge_note: Some("helper in c is outdated".into()), ..Fake::new() };
        let resp = call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        assert!(
            resp.body.ends_with(r#","started":true,"note":"ssh-agent relay unavailable: helper in c is outdated"}"#),
            "{resp:?}"
        );
    }

    /// `--env` on an existing child replaces its saved env (not merged), before
    /// the op runs, whatever the child's liveness; no env, or the same set,
    /// writes nothing.
    #[test]
    fn ensure_env_replaces_the_saved_env_of_an_existing_child() {
        let mut s = state();
        for key in ["web-one", "web-two"] {
            s.instances.get_mut(key).unwrap().extra_env =
                BTreeMap::from([("OLD".into(), "1".into()), ("X".into(), "old".into())]);
        }
        let want = BTreeMap::from([("X".to_string(), "y".to_string())]);
        // Running, stopped, containerless.
        for (key, running, call_args) in [
            ("one", Some(true), None),
            ("two", Some(false), Some(argv(&["start", "web-two"]))),
            ("two", None, Some(argv(&["rebuild", "web-two"]))),
        ] {
            let mut fake = Fake::new();
            match running {
                Some(r) => {
                    fake.running.insert(format!("devsandbox-web-{key}"), r);
                }
                None => {
                    fake.running.remove(&format!("devsandbox-web-{key}"));
                }
            }
            let resp = call(&s, "d", &r_with(Op::Ensure, key), &mut fake);
            assert_eq!(resp.status, Status::Ok, "{key}: {resp:?}");
            assert_eq!(fake.saved, vec![(format!("web-{key}"), want.clone())], "{key}");
            assert_eq!(fake.calls, call_args.into_iter().collect::<Vec<_>>(), "{key}");
        }
        // Duplicate keys: last wins, as with `-e`.
        let mut r = req(Op::Ensure, Some("web"), Some("one"));
        r.env = vec![("A".into(), "1".into()), ("A".into(), "2".into())];
        let mut fake = Fake::new();
        call(&s, "d", &r, &mut fake);
        assert_eq!(fake.saved, vec![("web-one".into(), BTreeMap::from([("A".into(), "2".into())]))]);
        // No env, or the saved set again: nothing written.
        let mut fake = Fake::new();
        call(&s, "d", &req(Op::Ensure, Some("web"), Some("one")), &mut fake);
        s.instances.get_mut("web-one").unwrap().extra_env = want.clone();
        call(&s, "d", &r_with(Op::Ensure, "one"), &mut fake);
        assert!(fake.saved.is_empty());
        // Denied requests save nothing (not owned; a denied env name).
        let mut fake = Fake::new();
        call(&s, "d", &r_with(Op::Ensure, "other"), &mut fake);
        let mut r = r_with(Op::Ensure, "one");
        r.env.push(("PATH".into(), "/x".into()));
        call(&s, "d", &r, &mut fake);
        assert!(fake.saved.is_empty() && fake.calls.is_empty());
        // A new child gets its env through `run --env`, not `save_env`.
        let mut fake = Fake::new();
        call(&s, "d", &r_with(Op::Ensure, "new"), &mut fake);
        assert!(fake.saved.is_empty());
    }

    fn r_with(op: Op, key: &str) -> Request {
        Request {
            branch: Some("ignored".into()),
            env: vec![("X".into(), "y".into())],
            ..req(op, Some("web"), Some(key))
        }
    }

    const RUN_ID: &str = "1790000000-a1b2";

    #[test]
    fn exec_timeout_allows_run_wait_its_wait() {
        let wait = helper_argv(&run_req(Op::RunWait, "one"));
        assert_eq!(exec_timeout(&wait), Duration::from_secs(MAX_WAIT) + WAIT_SLACK);
        for op in [Op::Exec, Op::RunLs, Op::RunLogs, Op::RunRm, Op::RunPrune] {
            assert_eq!(exec_timeout(&helper_argv(&run_req(op, "one"))), EXEC_TIMEOUT, "{op:?}");
        }
    }

    fn run_req(op: Op, key: &str) -> Request {
        let mut r = req(op, None, Some(key));
        match op {
            Op::Exec => r.argv = argv(&["sh", "-c", "echo hi"]),
            Op::RunLogs | Op::RunWait | Op::RunRm => r.id = Some(RUN_ID.into()),
            _ => {}
        }
        r
    }

    /// `state()` with helpers recorded on the children.
    fn helper_state() -> State {
        let mut s = state();
        for i in s.instances.values_mut() {
            i.devsbd_arch = Some(crate::devsbd::Arch::X86_64);
        }
        s
    }

    fn helper(code: i32, stdout: &str, stderr: &str) -> ExecOutput {
        ExecOutput { code, stdout: stdout.as_bytes().to_vec(), stderr: stderr.into() }
    }

    #[test]
    fn run_ops_reach_only_owned_running_children_with_a_helper() {
        let s = helper_state();
        let run_ops = [Op::Exec, Op::RunLs, Op::RunLogs, Op::RunWait, Op::RunRm, Op::RunPrune];
        let cases: &[(&str, &str, Status, &str)] = &[
            ("p", "one", Status::Denied, "does not declare `dispatcher`"),
            ("d", "other", Status::Denied, "`web-other` is not this dispatcher's child"),
            ("d", "mine", Status::Denied, "`web-mine` is not"),
            ("a", "one", Status::Denied, "`web-one` is not"),
            ("d", "zzz", Status::Failed, "no child with key `zzz`"),
            ("d", "two", Status::Failed, "child web-two is stopped; ensure it first"),
        ];
        for op in run_ops {
            for (who, key, status, needle) in cases {
                let mut fake = Fake::new();
                let resp = call(&s, who, &run_req(op, key), &mut fake);
                assert_eq!(resp.status, *status, "{op:?} {who} {key}: {resp:?}");
                assert!(resp.body.contains(needle), "{op:?} {who} {key}: {resp:?}");
                assert!(fake.execs.is_empty() && fake.calls.is_empty());
            }
            // No container, or no helper recorded.
            let mut fake = Fake::new();
            fake.running.remove("devsandbox-web-one");
            let resp = call(&s, "d", &run_req(op, "one"), &mut fake);
            assert!(resp.body.contains("has no container"), "{resp:?}");
            let resp = call(&state(), "d", &run_req(op, "one"), &mut Fake::new());
            assert_eq!(resp.status, Status::Failed);
            assert!(resp.body.contains("no devsbd helper"), "{resp:?}");
        }
    }

    #[test]
    fn run_ops_exec_the_childs_helper() {
        let s = helper_state();
        let bin = crate::devsbd::BIN;
        let cases: Vec<(Request, Vec<&str>, &str, &str)> = vec![
            (run_req(Op::Exec, "one"), vec![bin, "run", "start", "--", "sh", "-c", "echo hi"],
             "1790000000-a1b2\n", "1790000000-a1b2"),
            (run_req(Op::RunLs, "one"), vec![bin, "run", "ls"], "a\nb\n", "a\nb"),
            (run_req(Op::RunLogs, "one"), vec![bin, "run", "logs", RUN_ID, "--offset", "0"],
             "out\n", "4\nout\n"),
            (Request { offset: Some(10), ..run_req(Op::RunLogs, "one") },
             vec![bin, "run", "logs", RUN_ID, "--offset", "10"], "x", "11\nx"),
            (run_req(Op::RunWait, "one"), vec![bin, "run", "wait", RUN_ID], "exited 4\n", "exited 4"),
            (Request { timeout: Some(5), ..run_req(Op::RunWait, "one") },
             vec![bin, "run", "wait", RUN_ID, "--timeout", "5"], "running\n", "running"),
            // Capped.
            (Request { timeout: Some(99_999), ..run_req(Op::RunWait, "one") },
             vec![bin, "run", "wait", RUN_ID, "--timeout", "300"], "running\n", "running"),
            (run_req(Op::RunRm, "one"), vec![bin, "run", "rm", RUN_ID], "", ""),
            (Request { force: true, ..run_req(Op::RunRm, "one") },
             vec![bin, "run", "rm", RUN_ID, "--force"], "", ""),
            (run_req(Op::RunPrune, "one"), vec![bin, "run", "prune"], "removed 3\n", "removed 3"),
            (Request { keep: Some(5), ..run_req(Op::RunPrune, "one") },
             vec![bin, "run", "prune", "--keep", "5"], "removed 0\n", "removed 0"),
        ];
        for (r, want, stdout, body) in cases {
            let mut fake = Fake { helper: helper(0, stdout, ""), ..Fake::new() };
            let resp = call(&s, "d", &r, &mut fake);
            assert_eq!(resp, Response::new(Status::Ok, body), "{r:?}");
            assert_eq!(fake.execs, vec![("devsandbox-web-one".to_string(), argv(&want))]);
            assert!(fake.calls.is_empty(), "no devsandbox subprocess for runs");
        }
        // The sandbox filter narrows the child lookup like stop/rm.
        let resp = call(&s, "d", &Request { sandbox: Some("api".into()), ..run_req(Op::RunLs, "one") }, &mut Fake::new());
        assert_eq!(resp.status, Status::Failed);
    }

    #[test]
    fn run_op_failures() {
        let s = helper_state();
        let r = run_req(Op::RunLogs, "one");
        let mut fake = Fake { helper: helper(1, "", "\ndevsbd run: no run `1790000000-a1b2`\n"), ..Fake::new() };
        let resp = call(&s, "d", &r, &mut fake);
        assert_eq!(resp, Response::new(Status::Failed, "in web-one: devsbd run: no run `1790000000-a1b2`"));
        let mut fake = Fake { helper: helper(2, "", "usage: devsbd version|daemon|bridge\n"), ..Fake::new() };
        let resp = call(&s, "d", &r, &mut fake);
        assert!(resp.body.contains("predates runs"), "{resp:?}");
        // A helper with runs but without `run rm`/`run prune`.
        let old = helper(2, "", "devsbd run: unknown subcommand `rm`\nusage: devsbd run start\n");
        for r in [run_req(Op::RunRm, "one"), run_req(Op::RunPrune, "one")] {
            let resp = call(&s, "d", &r, &mut Fake { helper: old.clone(), ..Fake::new() });
            assert!(resp.body.contains("predates `run rm`/`run prune`; restart the child"), "{resp:?}");
        }
        // A refusal from a current helper is passed through.
        let busy = helper(1, "", "devsbd run: run `1790000000-a1b2` is running; `--force` kills it first\n");
        let resp = call(&s, "d", &run_req(Op::RunRm, "one"), &mut Fake { helper: busy, ..Fake::new() });
        assert_eq!(resp.status, Status::Failed);
        assert!(resp.body.contains("is running; `--force`"), "{resp:?}");
        let mut fake = Fake { helper: helper(3, "", ""), ..Fake::new() };
        assert!(call(&s, "d", &r, &mut fake).body.contains("devsbd run failed"));
        let mut fake = Fake { fail: true, ..Fake::new() };
        assert_eq!(call(&s, "d", &r, &mut fake), Response::new(Status::Failed, "no docker"));
    }

    #[test]
    fn run_op_fields() {
        let s = helper_state();
        let cases: &[(Request, &str)] = &[
            (req(Op::Exec, None, Some("one")), "`exec` needs `arg`"),
            (req(Op::RunLogs, None, Some("one")), "`run-logs` needs `id`"),
            (req(Op::RunWait, None, None), "needs `key`"),
            (Request { id: Some("../x".into()), ..run_req(Op::RunWait, "one") }, "bad run id `../x`"),
            (Request { id: Some(RUN_ID.into()), ..run_req(Op::Exec, "one") }, "`exec` takes no `id`"),
            (Request { offset: Some(1), ..run_req(Op::RunWait, "one") }, "takes no `offset`"),
            (Request { timeout: Some(1), ..run_req(Op::RunLogs, "one") }, "takes no `timeout`"),
            (Request { argv: argv(&["x"]), ..run_req(Op::RunLs, "one") }, "`run-ls` takes no `arg`"),
            (Request { argv: argv(&["x"]), ..req(Op::Stop, None, Some("one")) }, "`stop` takes no `arg`"),
            (Request { id: Some(RUN_ID.into()), ..req(Op::Ls, None, None) }, "`ls` takes no `id`"),
            (r_with(Op::Exec, "one"), "takes no `branch`/`env`"),
            (req(Op::RunRm, None, Some("one")), "`run-rm` needs `id`"),
            (Request { id: Some(RUN_ID.into()), ..run_req(Op::RunPrune, "one") }, "`run-prune` takes no `id`"),
            (Request { force: true, ..run_req(Op::RunPrune, "one") }, "`run-prune` takes no `force`"),
            (Request { force: true, ..run_req(Op::RunLs, "one") }, "`run-ls` takes no `force`"),
            (Request { keep: Some(1), ..run_req(Op::RunRm, "one") }, "`run-rm` takes no `keep`"),
            (Request { keep: Some(1), ..req(Op::Rm, None, Some("one")) }, "`rm` takes no `keep`"),
            (Request { id: Some("../x".into()), ..run_req(Op::RunRm, "one") }, "bad run id `../x`"),
        ];
        for (r, needle) in cases {
            let mut fake = Fake::new();
            let resp = call(&s, "d", r, &mut fake);
            assert_eq!(resp.status, Status::Usage, "{r:?}: {resp:?}");
            assert!(resp.body.contains(needle), "{r:?}: {resp:?}");
            assert!(fake.execs.is_empty());
        }
    }

    /// The real executor end to end: a throwaway container with the helper,
    /// a fabricated child instance owning it, and every run op through
    /// `handle_with` (argv from `exec_argv`, the helper's `run` commands).
    #[test_utils::docker_test(helper)]
    fn runs_in_a_child_container_with_docker() -> Result<(), &'static str> {
        use std::process::Command;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("devsandbox-dispatch-runs-test-{stamp}");
        let up = Command::new("docker")
            .args(["run", "-d", "--rm", "--name", &name, "alpine:3.20", "sleep", "300"])
            .output()
            .unwrap();
        assert!(up.status.success(), "{}", String::from_utf8_lossy(&up.stderr));
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            let arch = crate::devsbd::install(&name, None).unwrap();
            let mut s = state();
            let child = s.instances.get_mut("web-one").unwrap();
            child.container = name.clone();
            child.workspace = "/tmp".into();
            child.devsbd_arch = Some(arch);
            let config = Config::parse(CONFIG).unwrap();
            let call = |r: Request| handle_with(&s, &config, "d", &r, &mut Subprocess);

            let exec = Request {
                argv: argv(&["sh", "-c", "echo out; pwd; exit 4"]),
                ..req(Op::Exec, None, Some("one"))
            };
            let resp = call(exec);
            assert_eq!(resp.status, Status::Ok, "{resp:?}");
            let id = resp.body;
            assert!(control::valid_run_id(&id), "{id}");
            let with_id = |op, timeout| Request {
                id: Some(id.clone()),
                timeout,
                ..req(op, None, Some("one"))
            };
            let resp = call(with_id(Op::RunWait, Some(30)));
            assert_eq!(resp, Response::new(Status::Ok, "exited 4"));
            let resp = call(with_id(Op::RunLogs, None));
            assert_eq!(resp, Response::new(Status::Ok, "9\nout\n/tmp\n"), "cwd is the workspace");
            let resp = call(Request { offset: Some(4), ..with_id(Op::RunLogs, None) });
            assert_eq!(resp, Response::new(Status::Ok, "9\n/tmp\n"));
            let resp = call(req(Op::RunLs, None, Some("one")));
            assert_eq!(resp.status, Status::Ok);
            assert!(resp.body.starts_with(&format!("{id} exited 4 ")), "{resp:?}");
            // An unknown run is a clean failure from the helper.
            let resp = call(Request { id: Some("0000000000-0000".into()), ..with_id(Op::RunWait, None) });
            assert_eq!(resp.status, Status::Failed);
            assert!(resp.body.contains("no run `0000000000-0000`"), "{resp:?}");
            // Clearing: a second run pruned, the first removed by id.
            let exec = Request { argv: argv(&["true"]), ..req(Op::Exec, None, Some("one")) };
            let id2 = call(exec).body;
            assert_eq!(call(Request { id: Some(id2.clone()), timeout: Some(30), ..req(Op::RunWait, None, Some("one")) }).body, "exited 0");
            let resp = call(Request { keep: Some(1), ..req(Op::RunPrune, None, Some("one")) });
            assert_eq!(resp, Response::new(Status::Ok, "removed 1"));
            // Newest by id: both may share a start second, so either is left.
            let resp = call(req(Op::RunLs, None, Some("one")));
            assert_eq!(resp.body.lines().count(), 1, "{resp:?}");
            let left = resp.body.split(' ').next().unwrap().to_string();
            assert!(left == id || left == id2, "{resp:?}");
            let resp = call(Request { id: Some(left), ..req(Op::RunRm, None, Some("one")) });
            assert_eq!(resp, Response::new(Status::Ok, ""));
            assert_eq!(call(req(Op::RunLs, None, Some("one"))).body, "");
        });
        Ok(())
    }

    #[test]
    fn cap_counts_owned_children_only() {
        // `d` owns two (one stopped) with max-instances = 2.
        let s = state();
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("three")), &mut fake);
        assert_eq!(resp.status, Status::Denied);
        assert!(resp.body.contains("max-instances"), "{resp:?}");
        assert!(fake.calls.is_empty());
        // Existing children are still ensured at the cap.
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("two")), &mut fake);
        assert_eq!(resp.status, Status::Ok);
        // No `max-instances`: `a` gets the default cap; others' children don't count.
        let resp = call(&s, "a", &req(Op::Ensure, Some("web"), Some("three")), &mut fake);
        assert_eq!(resp, ensured("web", "three", true, false));
        let mut s = state();
        let cap = crate::config::Dispatcher::DEFAULT_MAX_INSTANCES as usize;
        for i in 1..cap {
            let name = format!("api-{i}");
            s.instances.insert(name.clone(), inst("api", &name, Some("a")));
        }
        let resp = call(&s, "a", &req(Op::Ensure, Some("web"), Some("three")), &mut fake);
        assert_eq!(resp.status, Status::Denied, "{resp:?}");
        assert!(resp.body.contains(&format!("`max-instances` reached ({cap})")), "{resp:?}");
    }

    #[test]
    fn same_name_from_another_sandbox_is_denied() {
        // `web-pr` + `1` and `web` + `pr-1` both name `web-pr-1`.
        let mut s = state();
        s.instances.insert("web-pr-1".into(), inst("web", "web-pr-1", Some("a")));
        let mut config = Config::parse(CONFIG).unwrap();
        config.sandboxes.insert("web-pr".into(), Default::default());
        let r = req(Op::Ensure, Some("web-pr"), Some("1"));
        let resp = handle_with(&s, &config, "a", &r, &mut Fake::new());
        assert_eq!(resp.status, Status::Denied);
        assert!(resp.body.contains("child of sandbox `web`"), "{resp:?}");
    }

    #[test]
    fn key_and_field_validation() {
        // The rule itself is tested in `control`; this covers how `handle` uses it.
        let s = state();
        let cases: &[(Request, &str)] = &[
            (req(Op::Ensure, Some("web"), Some("Bad")), "bad key `Bad`"),
            (req(Op::Ensure, None, Some("k")), "needs `sandbox`"),
            (req(Op::Ensure, Some("web"), None), "needs `key`"),
            (req(Op::Stop, None, None), "needs `key`"),
            (req(Op::Ls, None, Some("k")), "takes no `key`"),
            (req(Op::Ls, Some("web"), None), "takes no `sandbox`"),
            (r_with(Op::Rm, "one"), "takes no `branch`/`env`"),
        ];
        for (r, needle) in cases {
            let mut fake = Fake::new();
            let resp = call(&s, "d", r, &mut fake);
            assert_eq!(resp.status, Status::Usage, "{r:?}: {resp:?}");
            assert!(resp.body.contains(needle), "{r:?}: {resp:?}");
            assert!(fake.calls.is_empty());
        }
    }

    /// A branch is data: anything `run` could expand (`${localEnv:…}`) or git
    /// could read as an option or revision expression is refused up front.
    #[test]
    fn ensure_branch_validation() {
        let s = state();
        let with = |b: &str| Request { branch: Some(b.into()), ..req(Op::Ensure, Some("web"), Some("one")) };
        for bad in ["${localEnv:X}", "x-${localEnv:GITHUB_TOKEN}", "--upload-pack=x", "a..b", "x.lock", "-x"] {
            let mut fake = Fake::new();
            let resp = call(&s, "d", &with(bad), &mut fake);
            assert_eq!(resp.status, Status::Usage, "{bad:?}: {resp:?}");
            assert!(resp.body.starts_with("bad branch: use "), "{resp:?}");
            assert!(!resp.body.contains(bad), "value not echoed: {resp:?}");
            assert!(fake.calls.is_empty());
        }
        for good in ["feat/x", "joan/pr-12", "release-1.2"] {
            let mut fake = Fake::new();
            let resp = call(&s, "d", &with(good), &mut fake);
            assert_eq!(resp, ensured("web", "one", false, false), "{good:?}");
        }
    }

    #[test]
    fn stop_and_rm_run_on_owned_children() {
        let s = state();
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Stop, None, Some("one")), &mut fake);
        assert_eq!(resp, answered(Op::Stop, "web-one", "one"));
        let resp = call(&s, "d", &req(Op::Rm, Some("web"), Some("two")), &mut fake);
        assert_eq!(resp, answered(Op::Rm, "web-two", "two"));
        assert_eq!(fake.calls, vec![argv(&["stop", "web-one"]), argv(&["rm", "web-two"])]);
        // The sandbox filter narrows the lookup.
        let resp = call(&s, "d", &req(Op::Stop, Some("api"), Some("one")), &mut fake);
        assert_eq!(resp.status, Status::Failed);
    }

    #[test]
    fn done_marks_owned_children_only() {
        let s = state();
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Done, None, Some("one")), &mut fake);
        assert_eq!(resp, answered(Op::Done, "web-one", "one"));
        let resp = call(&s, "d", &req(Op::Done, Some("web"), Some("two")), &mut fake);
        assert_eq!(resp, answered(Op::Done, "web-two", "two"));
        let names: Vec<&str> = fake.dones.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["web-one", "web-two"]);
        assert!(fake.dones.iter().all(|(_, d)| d.is_some()));
        assert!(fake.calls.is_empty(), "no subprocess: an in-process state write");

        // Not owned / unknown / not a dispatcher: nothing written.
        for (who, r, status, needle) in [
            ("d", req(Op::Done, None, Some("other")), Status::Denied, "`web-other` is not"),
            ("d", req(Op::Done, None, Some("mine")), Status::Denied, "`web-mine` is not"),
            ("d", req(Op::Done, None, Some("zzz")), Status::Failed, "no child with key `zzz`"),
            ("p", req(Op::Done, None, Some("one")), Status::Denied, "does not declare"),
            ("d", req(Op::Done, None, None), Status::Usage, "needs `key`"),
            ("d", r_with(Op::Done, "one"), Status::Usage, "takes no `branch`/`env`"),
        ] {
            let mut fake = Fake::new();
            let resp = call(&s, who, &r, &mut fake);
            assert_eq!(resp.status, status, "{r:?}: {resp:?}");
            assert!(resp.body.contains(needle), "{r:?}: {resp:?}");
            assert!(fake.dones.is_empty(), "{r:?}");
        }

        // Already done: Ok, the original since kept (no write).
        let mut s = state();
        s.instances.get_mut("web-one").unwrap().done = Some(7);
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Done, None, Some("one")), &mut fake);
        assert_eq!(resp, answered(Op::Done, "web-one", "one"));
        assert!(fake.dones.is_empty());

        // A failed write is reported.
        let mut fake = Fake { fail: true, ..Fake::new() };
        let resp = call(&state(), "d", &req(Op::Done, None, Some("one")), &mut fake);
        assert_eq!(resp, Response::new(Status::Failed, "boom"));
    }

    #[test]
    fn ensure_reusing_a_done_child_clears_it() {
        let mut s = state();
        s.instances.get_mut("web-one").unwrap().done = Some(7);
        s.instances.get_mut("web-two").unwrap().done = Some(7);
        // Running: nothing to run, but the flag is cleared.
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("one")), &mut fake);
        assert_eq!(resp, ensured("web", "one", false, false));
        assert_eq!(fake.dones, vec![("web-one".to_string(), None)]);
        assert!(fake.calls.is_empty());
        // Stopped: cleared, then started.
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("two")), &mut fake);
        assert_eq!(resp, ensured("web", "two", false, true));
        assert_eq!(fake.dones, vec![("web-two".to_string(), None)]);
        assert_eq!(fake.calls, vec![argv(&["start", "web-two"])]);
        // Not done: no write.
        let mut fake = Fake::new();
        call(&state(), "d", &req(Op::Ensure, Some("web"), Some("one")), &mut fake);
        assert!(fake.dones.is_empty());
        // Denied (foreign child): nothing cleared.
        let mut s = state();
        s.instances.get_mut("web-other").unwrap().done = Some(7);
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("other")), &mut fake);
        assert_eq!(resp.status, Status::Denied);
        assert!(fake.dones.is_empty());
    }

    #[test]
    fn stop_with_a_key_shared_across_sandboxes_is_ambiguous() {
        let mut s = state();
        s.instances.insert("api-one".into(), inst("api", "api-one", Some("d")));
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Stop, None, Some("one")), &mut fake);
        assert_eq!(resp.status, Status::Usage);
        assert!(resp.body.contains("api-one, web-one"), "{resp:?}");
        let resp = call(&s, "d", &req(Op::Stop, Some("api"), Some("one")), &mut fake);
        assert_eq!(resp, answered(Op::Stop, "api-one", "one"));
    }

    #[test]
    fn ls_lists_owned_children_as_json() {
        let mut s = state();
        // A renamed child keeps its owner but loses its key.
        let mut renamed = inst("web", "web-x", Some("d"));
        renamed.branch = None;
        s.instances.insert("renamed".into(), renamed);
        s.instances.get_mut("web-two").unwrap().done = Some(5);
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ls, None, None), &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        let rows: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(
            rows,
            serde_json::json!([
                {"name": "renamed", "sandbox": "web", "key": null, "state": "missing",
                 "branch": null, "done": false},
                {"name": "web-one", "sandbox": "web", "key": "one", "state": "running",
                 "branch": "sandbox/web-one", "done": false},
                {"name": "web-two", "sandbox": "web", "key": "two", "state": "stopped",
                 "branch": "sandbox/web-two", "done": true},
            ])
        );
        assert!(fake.calls.is_empty());
        // No children: an empty array.
        let resp = call(&s, "a", &req(Op::Ls, None, None), &mut Fake::default());
        let rows: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1, "only `web-other`");
        let mut s = state();
        s.instances.remove("web-other");
        let resp = call(&s, "a", &req(Op::Ls, None, None), &mut Fake::default());
        assert_eq!(resp.body, "[]");
    }

    /// The Inbox pane's resolver: a thread's `child` key to its instance name,
    /// over state alone (no runtime, no control op).
    #[test]
    fn child_names_maps_keys_to_instances() {
        let mut s = state();
        // A child renamed away from `<sandbox>-<key>` has no key to map.
        let mut renamed = inst("web", "web-x", Some("d"));
        renamed.branch = None;
        s.instances.insert("renamed".into(), renamed);

        let mine = child_names(&s, "d");
        assert_eq!(mine.get("one").map(String::as_str), Some("web-one"));
        assert_eq!(mine.get("two").map(String::as_str), Some("web-two"));
        assert_eq!(mine.len(), 2, "the renamed child is unresolvable: {mine:?}");
        // Another dispatcher's child is never visible, and a dispatcher with
        // no children resolves nothing.
        assert!(!mine.contains_key("other"));
        assert_eq!(child_names(&s, "a").get("other").map(String::as_str), Some("web-other"));
        assert!(child_names(&s, "p").is_empty());
    }

    /// The pane resolves a thread's `child` like `stop`/`rm` would: the owner's
    /// own child only, and nothing on an ambiguous key.
    #[test]
    fn resolve_child_is_owned_and_unambiguous() {
        let mut s = state();
        assert_eq!(resolve_child(&s, "d", "one").as_deref(), Some("web-one"));
        assert_eq!(resolve_child(&s, "d", "other"), None, "another dispatcher's child");
        assert_eq!(resolve_child(&s, "d", "nope"), None);
        assert_eq!(resolve_child(&s, "p", "one"), None, "not a dispatcher");

        let all = thread_children(&s);
        assert_eq!(all.keys().collect::<Vec<_>>(), ["a", "d"], "owners with children only");
        assert_eq!(all["d"].get("two").map(String::as_str), Some("web-two"));
        assert_eq!(all["a"].get("other").map(String::as_str), Some("web-other"));

        // Same key in a second sandbox: `stop one` would need the sandbox, so
        // the pane doesn't guess either.
        s.instances.insert("api-one".into(), inst("api", "api-one", Some("d")));
        assert_eq!(resolve_child(&s, "d", "one"), None);
        assert!(!thread_children(&s)["d"].contains_key("one"));
    }

    const WORKTREES: &[&str] = &["worktree", "list", "--porcelain"];
    const REFS: &[&str] = &["for-each-ref", "--format=%(refname)", "refs/heads", "refs/remotes/origin"];

    fn rev_list(b: &str) -> Vec<String> {
        argv(&["rev-list", "--count", &format!("refs/remotes/origin/{b}..refs/heads/{b}")])
    }

    /// `state()` with `d`'s config root a real dir (the base repo path is
    /// canonicalized, as `run` does), and the base checkout: `/w/base`.
    fn branches_state() -> (State, PathBuf) {
        let mut s = state();
        let root = std::env::temp_dir();
        s.instances.get_mut("d").unwrap().config_dir = Some(root.clone());
        (s, root.canonicalize().unwrap())
    }

    /// One record per holder kind, plus a detached and a prunable worktree.
    /// `web-one` was created on `sandbox/web-one` but switched by hand.
    const PORCELAIN: &str = "worktree /w/base\nHEAD 1\nbranch refs/heads/master\n\n\
        worktree /w/web-one\nHEAD 2\nbranch refs/heads/feat/switched\n\n\
        worktree /w/web-mine\nHEAD 3\nbranch refs/heads/joan/mine\n\n\
        worktree /w/web-other\nHEAD 4\nbranch refs/heads/other/kid\n\n\
        worktree /elsewhere/hand\nHEAD 5\nbranch refs/heads/joan/hand-made\n\n\
        worktree /w/web-two\nHEAD 6\ndetached\n\n\
        worktree /gone\nHEAD 7\nbranch refs/heads/joan/gone\nprunable gitdir file points to non-existent location\n\n";

    fn branches_fake() -> Fake {
        let mut fake = Fake::new();
        fake.git_out.insert(argv(WORKTREES), PORCELAIN.into());
        fake
    }

    fn branches_req(sandbox: &str, ahead: bool) -> Request {
        Request { ahead, ..req(Op::Branches, Some(sandbox), None) }
    }

    #[test]
    fn branches_lists_holders_from_live_worktrees() {
        let (s, base) = branches_state();
        let mut fake = branches_fake();
        let resp = call(&s, "d", &branches_req("web", false), &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        let rows: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        // Sorted by branch; no `ahead`, no `local` rows; detached `web-two`
        // and the prunable record left out; the live branch, not `sandbox/web-one`.
        assert_eq!(
            rows,
            serde_json::json!([
                {"branch": "feat/switched", "holder": "child", "instance": "web-one"},
                {"branch": "joan/hand-made", "holder": "external"},
                {"branch": "joan/mine", "holder": "instance", "instance": "web-mine"},
                {"branch": "master", "holder": "base"},
                {"branch": "other/kid", "holder": "instance", "instance": "web-other"},
            ])
        );
        assert!(!resp.body.contains("/w/") && !resp.body.contains("/elsewhere"), "no host paths");
        assert_eq!(fake.gits, vec![(base, argv(WORKTREES))], "no ref or rev-list calls");
        assert!(fake.calls.is_empty() && fake.execs.is_empty());
        // An instance on the base checkout itself reports as `instance`.
        let mut s = s;
        s.instances.get_mut("web-mine").unwrap().folder = "/w/base".into();
        let resp = call(&s, "d", &branches_req("web", false), &mut branches_fake());
        let rows: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(rows[3], serde_json::json!({"branch": "master", "holder": "instance", "instance": "web-mine"}));
    }

    #[test]
    fn branches_ahead_counts_and_local_rows() {
        let (s, _) = branches_state();
        let mut fake = branches_fake();
        let refs = [
            "refs/heads/master", "refs/heads/feat/switched", "refs/heads/joan/mine",
            "refs/heads/other/kid", "refs/heads/joan/hand-made", "refs/heads/joan/wip",
            "refs/heads/joan/synced", "refs/heads/never-pushed",
            "refs/remotes/origin/HEAD", "refs/remotes/origin/master", "refs/remotes/origin/feat/switched",
            "refs/remotes/origin/joan/wip", "refs/remotes/origin/joan/synced", "refs/remotes/origin/only-remote",
        ];
        fake.git_out.insert(argv(REFS), refs.join("\n") + "\n");
        for (b, n) in [("master", "0"), ("feat/switched", "2"), ("joan/wip", "3"), ("joan/synced", "0")] {
            fake.git_out.insert(rev_list(b), format!("{n}\n"));
        }
        let resp = call(&s, "d", &branches_req("web", true), &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        let rows: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(
            rows,
            serde_json::json!([
                {"branch": "feat/switched", "holder": "child", "instance": "web-one", "ahead": 2},
                // Never pushed: no `ahead`.
                {"branch": "joan/hand-made", "holder": "external"},
                {"branch": "joan/mine", "holder": "instance", "instance": "web-mine"},
                // Unpushed commits in no worktree; `joan/synced` (0) and
                // `never-pushed` (no origin) get no row.
                {"branch": "joan/wip", "holder": "local", "ahead": 3},
                {"branch": "master", "holder": "base", "ahead": 0},
                {"branch": "other/kid", "holder": "instance", "instance": "web-other"},
            ])
        );
        // One ref listing, then rev-list only where `origin/B` exists.
        let mut called: Vec<Vec<String>> = fake.gits.into_iter().map(|(_, a)| a).collect();
        assert_eq!(called.drain(..2).collect::<Vec<_>>(), vec![argv(WORKTREES), argv(REFS)]);
        called.sort();
        let mut want: Vec<_> = ["feat/switched", "joan/synced", "joan/wip", "master"].map(rev_list).into();
        want.sort();
        assert_eq!(called, want);
    }

    #[test]
    fn branches_auth_and_fields() {
        let (mut s, _) = branches_state();
        let cases: &[(&str, Request, Status, &str)] = &[
            ("d", branches_req("api", false), Status::Denied, "may not spawn `api`"),
            ("a", branches_req("nope", false), Status::Denied, "unknown sandbox `nope`"),
            ("p", branches_req("web", false), Status::Denied, "does not declare `dispatcher`"),
            ("d", req(Op::Branches, None, None), Status::Usage, "`branches` needs `sandbox`"),
            ("d", req(Op::Branches, Some("web"), Some("one")), Status::Usage, "`branches` takes no `key`"),
            ("d", Request { ahead: true, ..req(Op::Ls, None, None) }, Status::Usage, "`ls` takes no `ahead`"),
            ("d", Request { ahead: true, ..req(Op::Ensure, Some("web"), Some("x")) }, Status::Usage, "`ensure` takes no `ahead`"),
            ("d", Request { branch: Some("x".into()), ..branches_req("web", false) }, Status::Usage, "takes no `branch`/`env`"),
        ];
        for (who, r, status, needle) in cases {
            let mut fake = branches_fake();
            let resp = call(&s, who, r, &mut fake);
            assert_eq!(resp.status, *status, "{who} {r:?}: {resp:?}");
            assert!(resp.body.contains(needle), "{who} {r:?}: {resp:?}");
            assert!(fake.gits.is_empty(), "no git call: {r:?}");
        }
        // A missing base folder (as `run`) and a git failure are `failed`.
        let resp = call(&state(), "d", &branches_req("web", false), &mut branches_fake());
        assert_eq!(resp, Response::new(Status::Failed, "sandbox folder `.` does not exist"));
        let resp = call(&s, "d", &branches_req("web", false), &mut Fake::new());
        assert_eq!(resp.status, Status::Failed, "{resp:?}");
        let resp = call(&s, "d", &branches_req("web", true), &mut branches_fake());
        assert!(resp.body.contains("no canned `git for-each-ref"), "{resp:?}");
        s.instances.get_mut("a").unwrap().config_dir = s.instances["d"].config_dir.clone();
        assert_eq!(call(&s, "a", &branches_req("web", false), &mut branches_fake()).status, Status::Ok);
    }

    /// A fresh store under the temp dir with a reply-taking thread `pr-1`
    /// for each of dispatchers `d` and `a` (owner id = state key here), and
    /// a Fake pointing at it.
    fn inbox_fake(name: &str) -> (PathBuf, Fake) {
        use crate::inbox::{Compose, State as TState, ThreadPut};
        let dir = std::env::temp_dir().join(format!("devsandbox-dispatch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("inbox.json");
        store::update_at(&path, |i| {
            for owner in ["d", "a"] {
                let put = ThreadPut {
                    key: "pr-1".into(),
                    title: "PR 1".into(),
                    state: TState::NeedsYou,
                    compose: Some(Compose::default()),
                    ..ThreadPut::default()
                };
                i.put(owner, owner, 1, put);
            }
        })
        .unwrap();
        let fake = Fake { inbox: Some(path.clone()), ..Fake::new() };
        (path, fake)
    }

    /// The user replies `text` on `owner`'s thread, as a dashboard would.
    fn reply(path: &Path, owner: &str, text: &str, at: u64) {
        store::update_at(path, |i| {
            let id = i.threads.iter().find(|t| t.owner == owner).unwrap().id;
            i.apply(&crate::inbox::Op::Reply { thread: id, text: text.into() }, at, "api:some-gui");
        })
        .unwrap();
    }

    fn lines(resp: &Response) -> Vec<serde_json::Value> {
        resp.body.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn events_are_the_owners_own_as_json_lines() {
        let s = state();
        let (path, mut fake) = inbox_fake("lines");
        reply(&path, "d", "first \"one\"", 1_790_900_001);
        reply(&path, "a", "not yours", 1_790_900_002);
        reply(&path, "d", "second", 1_790_900_042);

        let resp = call(&s, "d", &Request::new(Op::Events), &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        let got = lines(&resp);
        assert_eq!(got.len(), 2, "{}", resp.body);
        assert_eq!(got[0]["thread"], "pr-1");
        assert!(got[0].get("key").is_none(), "`thread` only");
        assert_eq!(got[0]["kind"], "reply");
        assert_eq!(got[0]["text"], "first \"one\"");
        assert_eq!(got[0]["at"], "2026-10-02T00:13:21Z");
        assert!(got[0].get("action").is_none(), "absent fields are left out");
        // Which client the user replied from is the user's: never the owner's.
        assert!(!resp.body.contains("client") && !resp.body.contains("some-gui"), "{}", resp.body);
        assert!(control::valid_event_id(got[0]["id"].as_str().unwrap()));
        assert_eq!(got[1]["text"], "second", "oldest first");
        // Survives the response encoding.
        let wire = control::decode_response(&control::encode_response(&resp)).unwrap();
        assert_eq!(wire, resp);

        // At least once: asking again returns the same, until acked.
        assert_eq!(call(&s, "d", &Request::new(Op::Events), &mut fake), resp);
        let ids: Vec<String> = got.iter().map(|e| e["id"].as_str().unwrap().to_string()).collect();
        let theirs = lines(&call(&s, "a", &Request::new(Op::Events), &mut fake));
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0]["text"], "not yours");

        // Acking another dispatcher's id does nothing to it.
        let foreign = Request { ack: vec![theirs[0]["id"].as_str().unwrap().into()], ..Request::new(Op::EventsAck) };
        assert_eq!(call(&s, "d", &foreign, &mut fake).body, "0");
        assert_eq!(lines(&call(&s, "a", &Request::new(Op::Events), &mut fake)).len(), 1);
        let ack = Request { ack: ids, ..Request::new(Op::EventsAck) };
        assert_eq!(call(&s, "d", &ack, &mut fake), Response::new(Status::Ok, "2"));
        assert_eq!(call(&s, "d", &ack, &mut fake), Response::new(Status::Ok, "0"), "idempotent");
        assert_eq!(call(&s, "d", &Request::new(Op::Events), &mut fake), Response::new(Status::Ok, ""));
    }

    #[test]
    fn event_ops_need_inbox_and_well_formed_fields() {
        let mut s = state();
        // `o`: inbox only; `owner-one`: a child of `d` whose sandbox declares inbox.
        s.instances.insert("o".into(), inst("owner", "o", None));
        s.instances.insert("owner-one".into(), inst("owner", "owner-one", Some("d")));
        let (_path, mut fake) = inbox_fake("auth");
        let id = "e-1790900001-3f2a";
        let ack = |ids: &[&str]| Request { ack: ids.iter().map(|s| s.to_string()).collect(), ..Request::new(Op::EventsAck) };
        let no_inbox = "doesn't declare inbox = true";
        let cases: &[(&str, Request, Status, &str)] = &[
            ("p", Request::new(Op::Events), Status::Denied, "`p` doesn't declare inbox = true"),
            ("p", ack(&[id]), Status::Denied, no_inbox),
            ("p", Request::new(Op::ThreadLs), Status::Denied, no_inbox),
            ("web-one", Request::new(Op::Events), Status::Denied, no_inbox),
            // Inbox without dispatcher: thread/event ops only.
            ("o", Request::new(Op::Events), Status::Ok, ""),
            ("o", ack(&[id]), Status::Ok, "0"),
            ("o", Request::new(Op::ThreadLs), Status::Ok, "[]"),
            ("o", req(Op::Ls, None, None), Status::Denied, "does not declare `dispatcher`"),
            ("o", req(Op::Ensure, Some("web"), Some("x")), Status::Denied, "does not declare `dispatcher`"),
            // A child may own threads, but never runs child ops.
            ("owner-one", Request::new(Op::ThreadLs), Status::Ok, "[]"),
            ("owner-one", req(Op::Ls, None, None), Status::Denied, "children can't be dispatchers"),
            ("ghost", Request::new(Op::ThreadLs), Status::Denied, "not an instance"),
            ("d", Request::new(Op::EventsAck), Status::Usage, "`events-ack` needs `ack`"),
            ("d", ack(&["e-1"]), Status::Usage, "bad event id `e-1`"),
            ("d", Request { ack: vec![id.into()], ..Request::new(Op::Events) }, Status::Usage, "`events` takes no `ack`"),
            ("d", Request { timeout: Some(1), ..Request::new(Op::ThreadLs) }, Status::Usage, "`thread-ls` takes no `timeout`"),
            // `key` is `events`' thread filter only: no other inbox op takes it.
            ("d", Request { key: Some("pr-1".into()), ..Request::new(Op::EventsAck) }, Status::Usage, "`events-ack` takes no `key`"),
            ("d", Request { key: Some("pr-1".into()), ..Request::new(Op::ThreadLs) }, Status::Usage, "`thread-ls` takes no `key`"),
            ("d", Request { key: Some("PR 1".into()), ..Request::new(Op::Events) }, Status::Usage, "bad key `PR 1`"),
            ("d", Request { key: Some("pr-1".into()), sandbox: Some("web".into()), ..Request::new(Op::Events) }, Status::Usage, "`events` takes no `sandbox`"),
            ("d", Request { sandbox: Some("web".into()), ..Request::new(Op::ThreadLs) }, Status::Usage, "takes no `sandbox`"),
            ("d", Request { timeout: Some(1), ..run_req(Op::RunLs, "one") }, Status::Usage, "`run-ls` takes no `timeout`"),
            ("d", Request { ack: vec![id.into()], ..run_req(Op::RunLs, "one") }, Status::Usage, "takes no `ack`"),
        ];
        for (who, r, status, needle) in cases {
            let resp = call(&s, who, r, &mut fake);
            assert_eq!(resp.status, *status, "{who} {r:?}: {resp:?}");
            assert!(resp.body.contains(needle), "{who} {r:?}: {resp:?}");
        }
        // No store: a failure, not a panic.
        let resp = call(&s, "d", &Request::new(Op::Events), &mut Fake::new());
        assert_eq!(resp, Response::new(Status::Failed, "no inbox in this test"));
    }

    /// `events-follow` is authorized like the other inbox ops and takes
    /// only the thread filter; a refusal comes back instead of a stream.
    /// Never answered by `handle_with`.
    #[test]
    fn follow_needs_inbox_and_takes_only_a_thread() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let s = state();
        let config = Config::parse(CONFIG).unwrap();
        let (_path, fake) = inbox_fake("follow-auth");
        let followers = Followers::new();
        let follow = |who: &str, r: &Request| {
            follow_with(&s, &config, who, r, &fake, &mut Closed, &followers, Duration::from_secs(30))
        };
        let sub = Request::new(Op::Subscribe);
        // Authorized: the stream starts (and ends at once on the closed writer).
        assert_eq!(follow("d", &sub), Ok(()));
        assert_eq!(follow("d", &Request { key: Some("pr-1".into()), ..sub.clone() }), Ok(()));
        let err = |who: &str, r: &Request| follow(who, r).unwrap_err();
        assert_eq!(err("p", &sub).status, Status::Denied);
        assert_eq!(err("ghost", &sub).status, Status::Denied);
        let resp = err("d", &Request { timeout: Some(5), ..sub.clone() });
        assert_eq!((resp.status, resp.body.as_str()), (Status::Usage, "`events-follow` takes no `timeout`"));
        assert!(err("d", &Request { sandbox: Some("web".into()), ..sub.clone() }).body.contains("takes no `sandbox`"));
        assert!(err("d", &Request { key: Some("PR 1".into()), ..sub.clone() }).body.contains("bad key"));
        assert_eq!(err("d", &Request::new(Op::Events)).status, Status::Usage, "only events-follow streams");
        let no_store = follow_with(&s, &config, "d", &sub, &Fake::new(), &mut Closed, &followers, Duration::from_secs(30));
        assert_eq!(no_store, Err(Response::new(Status::Failed, "no inbox in this test")));

        let resp = call(&s, "d", &sub, &mut Fake::new());
        assert_eq!(resp.status, Status::Usage, "{resp:?}");
    }

    #[test]
    fn events_wait_wakes_on_an_enqueue_and_times_out_empty() {
        let s = state();
        let (path, mut fake) = inbox_fake("wait");
        // Nothing pending: an empty body once the timeout passes.
        let started = Instant::now();
        let resp = call(&s, "d", &Request { timeout: Some(1), ..Request::new(Op::Events) }, &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, ""));
        assert!(started.elapsed() >= Duration::from_secs(1));
        // No timeout: at once.
        let started = Instant::now();
        assert_eq!(call(&s, "d", &Request::new(Op::Events), &mut fake).body, "");
        assert!(started.elapsed() < Duration::from_millis(500));

        // Another dispatcher's event doesn't wake this one; ours does, well
        // before the timeout.
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                reply(&path, "a", "theirs", 10);
                std::thread::sleep(Duration::from_millis(700));
                reply(&path, "d", "ours", 11);
            })
        };
        let started = Instant::now();
        let resp = call(&s, "d", &Request { timeout: Some(30), ..Request::new(Op::Events) }, &mut fake);
        writer.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        let got = lines(&resp);
        assert_eq!(got.len(), 1, "{resp:?}");
        assert_eq!(got[0]["text"], "ours");
    }

    /// An in-process write (the daemon's sink, an ack) wakes a waiter at once,
    /// not at the next [`EVENTS_POLL`]; a write by another process (here: a
    /// rename that bypasses `update_at`) is still seen by the stamp fallback.
    #[test]
    fn events_wait_hears_in_process_writes_at_once_and_others_by_polling() {
        let s = state();
        let (path, mut fake) = inbox_fake("notify");
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                reply(&path, "d", "fast", 10);
            })
        };
        let started = Instant::now();
        let resp = call(&s, "d", &Request { timeout: Some(30), ..Request::new(Op::Events) }, &mut fake);
        writer.join().unwrap();
        assert!(started.elapsed() < Duration::from_millis(450), "{:?}", started.elapsed());
        assert_eq!(lines(&resp)[0]["text"], "fast");
        let ack = Request { ack: vec![lines(&resp)[0]["id"].as_str().unwrap().into()], ..Request::new(Op::EventsAck) };
        assert_eq!(call(&s, "d", &ack, &mut fake).body, "1");

        // Prepare the next store beside it (this bumps the generation now,
        // before the wait starts), then swap it in without a bump.
        let next = path.with_file_name("next.json");
        std::fs::copy(&path, &next).unwrap();
        reply(&next, "d", "slow", 11);
        let writer = {
            let (path, next) = (path.clone(), next.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                std::fs::rename(&next, &path).unwrap();
            })
        };
        let started = Instant::now();
        let resp = call(&s, "d", &Request { timeout: Some(30), ..Request::new(Op::Events) }, &mut fake);
        writer.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(lines(&resp)[0]["text"], "slow");
    }

    /// The user replies `text` on `owner`'s thread `key`, putting it first
    /// (reply-taking) if it doesn't exist yet.
    fn reply_on(path: &Path, owner: &str, key: &str, text: &str, at: u64) {
        use crate::inbox::{Compose, State as TState, ThreadPut};
        store::update_at(path, |i| {
            let find = |i: &crate::inbox::Inbox| {
                i.threads.iter().find(|t| t.owner == owner && t.key.as_deref() == Some(key)).map(|t| t.id)
            };
            if find(i).is_none() {
                let put = ThreadPut {
                    key: key.into(),
                    title: key.into(),
                    state: TState::NeedsYou,
                    compose: Some(Compose::default()),
                    ..ThreadPut::default()
                };
                i.put(owner, owner, 1, put);
            }
            let id = find(i).unwrap();
            i.apply(&crate::inbox::Op::Reply { thread: id, text: text.into() }, at, "tui");
        })
        .unwrap();
    }

    #[test]
    fn events_thread_filter_selects_one_thread() {
        let s = state();
        let (path, mut fake) = inbox_fake("filter");
        reply(&path, "d", "one", 1);
        reply_on(&path, "d", "pr-2", "two", 2);
        reply_on(&path, "d", "pr-1", "three", 4);
        reply(&path, "a", "theirs", 3);
        let on = |t: &str| Request { key: Some(t.into()), ..Request::new(Op::Events) };
        let texts = |resp: &Response| -> Vec<String> {
            lines(resp).iter().map(|e| e["text"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(texts(&call(&s, "d", &Request::new(Op::Events), &mut fake)), ["one", "two", "three"]);
        let resp = call(&s, "d", &on("pr-2"), &mut fake);
        assert_eq!(texts(&resp), ["two"]);
        assert_eq!(lines(&resp)[0]["thread"], "pr-2");
        assert_eq!(texts(&call(&s, "d", &on("pr-1"), &mut fake)), ["one", "three"]);
        // Unknown (not put yet) or another owner's: nothing, not an error.
        assert_eq!(call(&s, "d", &on("pr-9"), &mut fake), Response::new(Status::Ok, ""));
        assert_eq!(texts(&call(&s, "a", &on("pr-2"), &mut fake)), Vec::<String>::new());
    }

    /// A filtered wait isn't ended by the owner's event on another thread:
    /// it keeps waiting for one on its own.
    #[test]
    fn events_wait_with_a_thread_filter_waits_for_that_thread() {
        let s = state();
        let (path, mut fake) = inbox_fake("filter-wait");
        reply_on(&path, "d", "pr-2", "seed", 1);
        let pending = lines(&call(&s, "d", &Request::new(Op::Events), &mut fake));
        let ack = Request { ack: vec![pending[0]["id"].as_str().unwrap().into()], ..Request::new(Op::EventsAck) };
        assert_eq!(call(&s, "d", &ack, &mut fake).body, "1");

        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                reply_on(&path, "d", "pr-1", "other thread", 10);
                std::thread::sleep(Duration::from_millis(600));
                reply_on(&path, "d", "pr-2", "ours", 11);
            })
        };
        let started = Instant::now();
        let req = Request { key: Some("pr-2".into()), timeout: Some(30), ..Request::new(Op::Events) };
        let resp = call(&s, "d", &req, &mut fake);
        writer.join().unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(700) && elapsed < Duration::from_secs(10), "{elapsed:?}");
        let got = lines(&resp);
        assert_eq!(got.len(), 1, "{resp:?}");
        assert_eq!((got[0]["thread"].as_str(), got[0]["text"].as_str()), (Some("pr-2"), Some("ours")));

        // Only another thread's event pending: the filtered wait times out empty.
        let started = Instant::now();
        let req = Request { key: Some("pr-2".into()), timeout: Some(1), ..Request::new(Op::Events) };
        let ours = lines(&call(&s, "d", &Request { key: Some("pr-2".into()), ..Request::new(Op::Events) }, &mut fake));
        let ack = Request { ack: vec![ours[0]["id"].as_str().unwrap().into()], ..Request::new(Op::EventsAck) };
        assert_eq!(call(&s, "d", &ack, &mut fake).body, "1");
        assert_eq!(call(&s, "d", &req, &mut fake), Response::new(Status::Ok, ""));
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert_eq!(lines(&call(&s, "d", &Request::new(Op::Events), &mut fake)).len(), 1, "the other is still pending");
    }

    #[test]
    fn thread_ls_lists_the_owners_threads_in_put_shape() {
        let s = state();
        let (path, mut fake) = inbox_fake("ls");
        let resp = call(&s, "d", &Request::new(Op::ThreadLs), &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        let got: Vec<crate::inbox::ThreadPut> = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].key.as_str(), got[0].title.as_str()), ("pr-1", "PR 1"));
        assert!(got[0].compose.is_some());
        // Archived (the owner was removed) is history, not listed.
        store::update_at(&path, |i| i.archive_owner("d")).unwrap();
        assert_eq!(call(&s, "d", &Request::new(Op::ThreadLs), &mut fake).body, "[]");
        assert_eq!(call(&s, "a", &Request::new(Op::ThreadLs), &mut fake).body.matches("\"key\"").count(), 1);
    }

    #[test]
    fn thread_ls_feed_adds_the_owners_messages() {
        use crate::inbox::{Block, Field, MessageSend};
        let s = state();
        let (path, mut fake) = inbox_fake("ls-feed");
        store::update_at(&path, |i| {
            let blocks = vec![
                Block::Markdown { text: "hi".into() },
                Block::Fields { items: vec![Field { label: "Head".into(), value: "36b1".into() }] },
            ];
            i.send("d", 5, MessageSend { thread: "pr-1".into(), id: "run-1".into(), blocks });
            i.send("d", 6, MessageSend { thread: "pr-1".into(), id: "run-2".into(), blocks: vec![Block::Markdown { text: "x".into() }] });
            i.withdraw("d", "pr-1", "run-2");
            // A user reply is not an owner message.
            let id = i.threads.iter().find(|t| t.owner == "d").unwrap().id;
            i.apply(&crate::inbox::Op::Reply { thread: id, text: "ok".into() }, 7, "tui");
        })
        .unwrap();
        let plain = call(&s, "d", &Request::new(Op::ThreadLs), &mut fake);
        assert!(!plain.body.contains("\"messages\""), "{}", plain.body);
        let resp = call(&s, "d", &Request { feed: true, ..Request::new(Op::ThreadLs) }, &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        let got: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(got[0]["key"], "pr-1", "still the put shape");
        assert_eq!(
            got[0]["messages"],
            serde_json::json!([
                {"id": "run-1", "at": 5, "edited": false, "withdrawn": false, "blocks": [
                    {"type": "markdown", "text": "hi"},
                    {"type": "fields", "items": [{"label": "Head", "value": "36b1"}]},
                ]},
                {"id": "run-2", "at": 6, "edited": false, "withdrawn": true, "blocks": [{"type": "markdown", "text": "x"}]},
            ])
        );
        // An older store's `header-message` item is an ordinary message now.
        store::update_at(&path, |i| {
            let t = i.threads.iter_mut().find(|t| t.owner == "d").unwrap();
            t.feed.push(crate::inbox::FeedItem::markdown(99, 8, "header-message", "old"));
        })
        .unwrap();
        let resp = call(&s, "d", &Request { feed: true, ..Request::new(Op::ThreadLs) }, &mut fake);
        let got: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(got[0]["messages"][2]["id"], "header-message", "{}", resp.body);
        // Another owner's thread with no messages lists an empty array.
        let other = call(&s, "a", &Request { feed: true, ..Request::new(Op::ThreadLs) }, &mut fake);
        assert!(other.body.contains("\"messages\":[]"), "{}", other.body);
        // Only `thread-ls` takes `feed`.
        let resp = call(&s, "d", &Request { feed: true, ..Request::new(Op::Events) }, &mut fake);
        assert_eq!((resp.status, resp.body.as_str()), (Status::Usage, "`events` takes no `feed`"));
    }

    /// A form's submission reaches the owner as one `submit` line with every
    /// answer and no client, and `thread ls --feed` shows each form's state
    /// (answers once submitted), so a dispatcher can recover.
    #[test]
    fn forms_show_in_events_and_thread_ls() {
        use crate::inbox::{message, Answer, Op as InboxOp};
        let s = state();
        let (path, mut fake) = inbox_fake("forms");
        let form = |id: &str| {
            format!(
                r#"{{"type":"form","id":"{id}","questions":[{{"id":"ok","label":"Merge?","type":"confirm"}},{{"id":"why","label":"Why","type":"text"}},{{"id":"pick","label":"P","type":"choice","required":false,"options":[{{"id":"a","label":"A"}}]}}]}}"#
            )
        };
        store::update_at(&path, |i| {
            for (msg, f) in [("run-1", "f1"), ("run-2", "f2"), ("run-3", "f3")] {
                let body = format!(r#"{{"thread":"pr-1","id":"{msg}","blocks":[{}]}}"#, form(f));
                i.send("d", 5, message::parse(&body).unwrap());
            }
            i.withdraw("d", "pr-1", "run-3");
            let id = i.threads.iter().find(|t| t.owner == "d").unwrap().id;
            let answers = [("ok".to_string(), Answer::Confirm(true))].into();
            i.apply(&InboxOp::Submit { thread: id, message: "run-1".into(), answers }, 1_790_900_001, "api:some-gui");
        })
        .unwrap();

        let resp = call(&s, "d", &Request::new(Op::Events), &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        let mut got = lines(&resp);
        assert_eq!(got.len(), 1, "{}", resp.body);
        assert!(control::valid_event_id(got[0]["id"].as_str().unwrap()));
        got[0].as_object_mut().unwrap().remove("id");
        assert_eq!(
            got[0],
            serde_json::json!({"thread": "pr-1", "kind": "submit", "message": "run-1", "form": "f1",
                "answers": {"ok": true, "why": "", "pick": null}, "at": "2026-10-02T00:13:21Z"})
        );
        assert!(!resp.body.contains("client") && !resp.body.contains("some-gui"), "{}", resp.body);

        let resp = call(&s, "d", &Request { feed: true, ..Request::new(Op::ThreadLs) }, &mut fake);
        let got: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        let forms: Vec<_> = got[0]["messages"].as_array().unwrap().iter().map(|m| m["form"].clone()).collect();
        assert_eq!(
            forms,
            [
                serde_json::json!({"id": "f1", "state": "submitted", "answers": {"ok": true, "why": "", "pick": null}}),
                serde_json::json!({"id": "f2", "state": "open"}),
                serde_json::json!({"id": "f3", "state": "withdrawn"}),
            ]
        );
        assert!(!resp.body.contains("some-gui"), "{}", resp.body);
    }

    #[test]
    fn events_body_stops_at_the_size_cap() {
        let big = "x".repeat(1000);
        let pending: Vec<(String, Event)> = (0..2000)
            .map(|i| {
                let e = Event {
                    id: format!("e-0000000000-{:04x}", i),
                    seq: i,
                    kind: crate::inbox::EventKind::Reply,
                    action: None,
                    text: Some(big.clone()),
                    message: None,
                    form: None,
                    answers: None,
                    at: 0,
                };
                ("k".to_string(), e)
            })
            .collect();
        let body = events_body(&pending);
        assert!(body.len() <= EVENTS_BODY_CAP);
        let n = body.lines().count();
        assert!(n > 100 && n < 2000, "{n}");
        assert!(body.lines().next().unwrap().contains("e-0000000000-0000"), "oldest kept");
    }

    #[test]
    fn tail_keeps_last_non_empty_lines() {
        assert_eq!(tail("a\nb\n\nc\nd\n", 2), "c\nd");
        assert_eq!(tail("only\n", 5), "only");
        assert_eq!(tail("", 5), "");
    }
}
