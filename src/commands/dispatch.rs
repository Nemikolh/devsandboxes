//! Host side of the dispatcher control API (docs/automations.md,
//! "Dispatchers"): authorize and carry out one decoded [`Request`] from a
//! dispatcher instance, answering a [`Response`]. The transport (a `CONTROL`
//! stream on the dispatcher's bridge) only decodes, calls [`handle`], and
//! encodes.
//!
//! Every request is checked against the config as it is *now*: the
//! dispatcher's sandbox (resolved from the config root recorded on its
//! instance) must declare `dispatcher`, `ensure` needs the target in its
//! `spawn` list, `stop`/`rm` only reach instances it owns, and `ensure` of a
//! new child respects `max-instances` (all owned children in state count,
//! stopped ones included; unset = `Dispatcher::DEFAULT_MAX_INSTANCES`).
//! Children can't be dispatchers: `ensure` of a sandbox declaring
//! `dispatcher` is denied whatever `spawn` says, and a request from a child
//! instance (old state) is denied, so dispatch can't recurse. `--env` names
//! on the control::denied_env list are denied. `ensure --env` on an existing
//! child replaces its saved env (`Instance::extra_env`) before (re)starting
//! it, so every later devsandbox exec there sees the new values.
//!
//! Runs (`exec`, `run-ls|logs|wait`) live in the child: the host execs the
//! child's own helper (`devsbd run start|ls|logs|wait`, `devsbd/src/runs.rs`)
//! there, as the child's remote user in its workspace with its `remoteEnv` and
//! saved env (the `devsandbox exec` argv), output captured, and answers with what it
//! printed. Only owned, running children with a helper are reachable.
//!
//! Children are ordinary instances named `<sandbox>-<key>`. Operations run as
//! `devsandbox -C <config root> run|start|rebuild|stop|rm …` subprocesses, not
//! in-process: the handler runs on the TUI's bridge worker and those commands
//! print (and stream docker/git output) to inherited stdio, which would land
//! on the alternate screen. Their output goes to
//! `<data>/devsandbox/logs/dispatch-<unix>-<op>.log`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::Config;
use crate::devsbd::control::{self, Op, Request, Response, Status};
use crate::runtime::{backend, bounded};
use crate::state::{Instance, State};

/// Longest accepted child key (the charset is checked by [`valid_key`]).
pub const MAX_KEY: usize = 40;

/// Cap on a `run-wait` request's `timeout`, seconds: each wait holds a bridge
/// handler thread and a `docker exec`; clients loop (`devsbd run wait`), and
/// the daemon gives up on a reply after 30 minutes anyway.
pub const MAX_WAIT: u64 = 300;

/// Wall-clock limit on one dispatch `devsandbox` subprocess (`ensure`,
/// `stop`, `rm`): it holds the host-wide control lock, so a wedged build or
/// git call must not hold it forever. Killed with its process group.
const DEVSANDBOX_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Wall-clock limit on one run-op exec in a child ([`Executor::exec_in`]).
const EXEC_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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
    /// Replace state entry `name`'s `extra_env` with `env` and save. Called
    /// under the bridge's host-wide control lock, before the op's subprocess
    /// (which then loads the updated entry).
    fn save_env(&mut self, name: &str, env: &BTreeMap<String, String>) -> Result<(), String>;
}

/// Load state and the dispatcher's config, then [`handle_with`] the real
/// executor. `dispatcher_key` is the state key of the instance whose bridge
/// the request arrived on. Blocking (it waits for the subprocess): run it off
/// the UI thread. Called by the TUI bridges' `CONTROL` handler
/// (`devsbd::bridge`), which is unix-only.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn handle(dispatcher_key: &str, req: &Request) -> Response {
    let state = match State::load() {
        Ok(state) => state,
        Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
    };
    let config_dir = match state.instances.get(dispatcher_key) {
        None => return denied(format!("`{dispatcher_key}` is not an instance")),
        Some(owner) => match &owner.config_dir {
            Some(dir) => dir.clone(),
            None => return no_config_dir(dispatcher_key),
        },
    };
    let config = match Config::load(&config_dir) {
        Ok(config) => config,
        Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
    };
    handle_with(&state, &config, dispatcher_key, req, &mut Subprocess)
}

/// Whether `key` is an instance whose sandbox declares `dispatcher`. Lets the
/// bridge skip its host-wide lock for requests `handle` will deny anyway, so a
/// child's lifecycle command calling `devsbd` can't stall behind the parent
/// op that is waiting for it. A dispatcher's child is never one (see
/// [`handle_with`]), even if its sandbox declares `dispatcher`.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn declares_dispatcher(key: &str) -> bool {
    let Ok(state) = State::load() else { return false };
    let Some(info) = state.instances.get(key) else { return false };
    if info.dispatcher.is_some() {
        return false;
    }
    let Some(dir) = info.config_dir.as_deref() else { return false };
    Config::load(dir)
        .and_then(|c| c.resolve_sandbox(&info.sandbox))
        .is_ok_and(|s| s.properties.dispatcher.is_some())
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

/// The pure core of [`handle`], over already-loaded `state` and `config`
/// (the config root recorded on the dispatcher instance).
pub(crate) fn handle_with(
    state: &State,
    config: &Config,
    dispatcher_key: &str,
    req: &Request,
    exec: &mut dyn Executor,
) -> Response {
    let Some(owner) = state.instances.get(dispatcher_key) else {
        return denied(format!("`{dispatcher_key}` is not an instance"));
    };
    let Some(config_dir) = owner.config_dir.as_deref() else {
        return no_config_dir(dispatcher_key);
    };
    // Only state from before children of dispatcher sandboxes were refused.
    if owner.dispatcher.is_some() {
        return denied(format!("`{dispatcher_key}` is a dispatcher's child; children can't be dispatchers"));
    }
    let decl = match config.resolve_sandbox(&owner.sandbox) {
        Ok(sandbox) => sandbox.properties.dispatcher,
        Err(e) => return denied(format!("{e:#}")),
    };
    let Some(decl) = decl else {
        return denied(format!("sandbox `{}` does not declare `dispatcher`", owner.sandbox));
    };
    if let Err(msg) = check_fields(req) {
        return usage(msg);
    }
    if let Some((k, _)) = req.env.iter().find(|(k, _)| control::denied_env(k)) {
        return denied(format!("env `{k}` may not be set by a dispatcher"));
    }
    let owner_id = owner.instance_id.as_str();

    match req.op {
        Op::Ls => ls(state, owner_id, exec),
        Op::Ensure => {
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
            }
            match action {
                Err(resp) => resp,
                Ok(Ensure::Nothing) => Response::new(Status::Ok, name),
                Ok(action) => {
                    let args = ensure_args(&action, sandbox, &name, req, owner_id);
                    match exec.devsandbox(config_dir, &args) {
                        Ok(()) => Response::new(Status::Ok, name),
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
                Ok(()) => Response::new(Status::Ok, name),
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
    field("timeout", only(&[Op::RunWait], true), req.timeout.is_some())?;
    field("force", only(&[Op::RunRm], true), req.force)?;
    field("keep", only(&[Op::RunPrune], true), req.keep.is_some())?;
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
        Op::Ensure | Op::Ls | Op::Stop | Op::Rm => unreachable!("not a run op"),
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

/// `[a-z0-9][a-z0-9-]{0,39}`: always a valid tail for an instance name, a
/// container name (`devsandbox-<sandbox>-<key>`), and a path segment
/// (`.worktrees/<id>`).
pub fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    let lower_alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    key.len() <= MAX_KEY
        && chars.next().is_some_and(lower_alnum)
        && chars.all(|c| lower_alnum(c) || c == '-')
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

/// Resolve `stop`/`rm`'s `key` (optionally narrowed by `sandbox`) to one of
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
        });
    }
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

    fn save_env(&mut self, name: &str, env: &BTreeMap<String, String>) -> Result<(), String> {
        let mut state = State::load().map_err(|e| format!("{e:#}"))?;
        let info = state
            .instances
            .get_mut(name)
            .ok_or_else(|| format!("`{name}` vanished from state"))?;
        info.extra_env = env.clone();
        state.save().map_err(|e| format!("{e:#}"))
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

[sandbox.any]
folder = "."
dispatcher = { spawn = ["*"] }

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
    /// `save_env` records `(name, env)` in `saved`.
    #[derive(Default)]
    struct Fake {
        running: BTreeMap<String, bool>,
        calls: Vec<Vec<String>>,
        fail: bool,
        helper: ExecOutput,
        execs: Vec<(String, Vec<String>)>,
        saved: Vec<(String, BTreeMap<String, String>)>,
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
        fn save_env(&mut self, name: &str, env: &BTreeMap<String, String>) -> Result<(), String> {
            self.saved.push((name.to_string(), env.clone()));
            Ok(())
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
        assert_eq!(resp, Response::new(Status::Ok, "web-pr-1"));
        assert_eq!(
            fake.calls,
            vec![argv(&[
                "run", "web", "--name", "web-pr-1", "--branch", "feat/x", "--env", "A=1",
                "--dispatcher", "d",
            ])]
        );

        // Running: nothing, still the name.
        let s = state();
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("one")), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-one"));
        assert!(fake.calls.is_empty());
        assert!(fake.saved.is_empty(), "no env given: saved env kept");

        // Stopped: start (the branch of an existing child is ignored).
        let resp = call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-two"));
        assert_eq!(fake.calls, vec![argv(&["start", "web-two"])]);

        // No container: rebuild, keeping the worktree.
        let mut fake = Fake::new();
        fake.running.remove("devsandbox-web-two");
        call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(fake.calls, vec![argv(&["rebuild", "web-two"])]);

        // A failing subprocess is Failed with its message.
        let mut fake = Fake { fail: true, ..Fake::new() };
        let resp = call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(resp, Response::new(Status::Failed, "boom"));
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
        assert_eq!(resp, Response::new(Status::Ok, "web-three"));
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
        for good in ["a", "0", "pr-123", "a-", &"x".repeat(MAX_KEY)] {
            assert!(valid_key(good), "{good}");
        }
        for bad in ["", "-a", "PR-1", "a_b", "a.b", "a/b", "a b", "é", &"x".repeat(MAX_KEY + 1)] {
            assert!(!valid_key(bad), "{bad}");
        }
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
            assert_eq!(resp, Response::new(Status::Ok, "web-one"), "{good:?}");
        }
    }

    #[test]
    fn stop_and_rm_run_on_owned_children() {
        let s = state();
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Stop, None, Some("one")), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-one"));
        let resp = call(&s, "d", &req(Op::Rm, Some("web"), Some("two")), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-two"));
        assert_eq!(fake.calls, vec![argv(&["stop", "web-one"]), argv(&["rm", "web-two"])]);
        // The sandbox filter narrows the lookup.
        let resp = call(&s, "d", &req(Op::Stop, Some("api"), Some("one")), &mut fake);
        assert_eq!(resp.status, Status::Failed);
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
        assert_eq!(resp, Response::new(Status::Ok, "api-one"));
    }

    #[test]
    fn ls_lists_owned_children_as_json() {
        let mut s = state();
        // A renamed child keeps its owner but loses its key.
        let mut renamed = inst("web", "web-x", Some("d"));
        renamed.branch = None;
        s.instances.insert("renamed".into(), renamed);
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ls, None, None), &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        let rows: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(
            rows,
            serde_json::json!([
                {"name": "renamed", "sandbox": "web", "key": null, "state": "missing",
                 "branch": null},
                {"name": "web-one", "sandbox": "web", "key": "one", "state": "running",
                 "branch": "sandbox/web-one"},
                {"name": "web-two", "sandbox": "web", "key": "two", "state": "stopped",
                 "branch": "sandbox/web-two"},
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

    #[test]
    fn tail_keeps_last_non_empty_lines() {
        assert_eq!(tail("a\nb\n\nc\nd\n", 2), "c\nd");
        assert_eq!(tail("only\n", 5), "only");
        assert_eq!(tail("", 5), "");
    }
}
