//! Verbs as requests (docs/api.md, *Verbs*; docs/verbs-as-requests.md):
//! `run`, `rebuild`, `start`, `stop` and `rm` as API methods. The daemon runs
//! each as a `devsandbox --in-process -C <root> <verb> …` subprocess built
//! from the request's typed params ([`argv`]), so a verb that panics or hangs
//! takes down one request, not the daemon, and a handoff doesn't wait for a
//! build (docs/client-server.md, *Execution, internally*).
//!
//! The subprocess gets the request's env and nothing of the daemon's
//! (`${localEnv:…}`, `SSH_AUTH_SOCK`, `GIT_*` must be the caller's), its own
//! process group (a cancel reaches docker and git under it), null stdin (no
//! prompt can block) and `GIT_TERMINAL_PROMPT=0`. Its stdout and stderr go
//! to spool files, `<data-dir>/devsandbox/logs/verbs/<pid>.out|.err`, not
//! pipes: a daemon exiting mid-verb (handoff, crash) would EPIPE a pipe, and
//! `stop`/`rm` have no build-log tee to fall back on. The files are removed
//! once the exit is reported; a successor finds live verbs by them.
//!
//! One loop on the connection's thread per request ([`serve`]): copy new
//! spool bytes into `output` notifications, re-read `state.toml` for
//! `progress`, reap the child, and watch the socket ([`Peer`]): a hang-up
//! SIGINTs the group (the verb rolls back), unless the daemon is draining,
//! whose read-side shutdown looks the same and must not cancel anything.
//! Socket-free, so the loop is tested with a fake [`Peer`] and `sh` scripts.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::api::{self, ApiError};
use crate::state::{PhaseView, Progress, State};

/// How long one turn of the loop waits on the socket: the output latency.
const TICK: Duration = Duration::from_millis(50);
/// How often `state.toml` is re-read for `progress`.
const STATE_POLL: Duration = Duration::from_millis(250);
/// Most bytes one `output` notification carries (escaped, well under
/// `proto::MAX_LINE`).
const CHUNK: usize = 64 * 1024;
/// How much of stderr is kept for a `failed` message.
const ERR_TAIL: usize = 16 * 1024;

pub fn is_verb(method: &str) -> bool {
    matches!(method, "run" | "rebuild" | "start" | "stop" | "rm")
}

// ---- params ---------------------------------------------------------------

/// `run` params. `env` is required on every verb: never the daemon's own.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunParams {
    pub root: PathBuf,
    pub env: BTreeMap<String, String>,
    pub sandbox: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub json: bool,
    #[serde(default)]
    pub detach: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuildParams {
    pub root: PathBuf,
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub detach: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartParams {
    pub root: PathBuf,
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub detach: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopParams {
    pub root: PathBuf,
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RmParams {
    pub root: PathBuf,
    pub env: BTreeMap<String, String>,
    pub instance: String,
    /// `true` deletes the branch, `false` keeps it, absent takes the
    /// non-TTY default (merged or gone branches deleted, local work kept).
    #[serde(default)]
    pub delete_branch: Option<bool>,
    #[serde(default)]
    pub force: bool,
    /// Answer `confirm` (the branch question) instead of deciding off a TTY,
    /// having removed nothing: the client asks and resends with
    /// `deleteBranch`.
    #[serde(default)]
    pub ask: bool,
}

/// What a verb acts on: an instance as the CLI's `<name>` takes it, or `--all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    One(String),
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verb {
    Run { sandbox: String, name: Option<String>, branch: Option<String>, base: Option<String>, json: bool },
    Rebuild { target: Target, force: bool },
    Start { target: Target },
    Stop { target: Target },
    Rm { instance: String, delete_branch: Option<bool>, force: bool, ask: bool },
}

impl Verb {
    pub fn name(&self) -> &'static str {
        match self {
            Verb::Run { .. } => "run",
            Verb::Rebuild { .. } => "rebuild",
            Verb::Start { .. } => "start",
            Verb::Stop { .. } => "stop",
            Verb::Rm { .. } => "rm",
        }
    }

    fn target(&self) -> Option<&Target> {
        match self {
            Verb::Rebuild { target, .. } | Verb::Start { target } | Verb::Stop { target } => Some(target),
            Verb::Run { .. } | Verb::Rm { .. } => None,
        }
    }

    /// The instance name the request gave, if it names one.
    fn named(&self) -> Option<&str> {
        match self {
            Verb::Rm { instance, .. } => Some(instance),
            _ => match self.target() {
                Some(Target::One(name)) => Some(name),
                _ => None,
            },
        }
    }
}

/// A checked verb request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbRequest {
    /// Absolute config root (`-C`).
    pub root: PathBuf,
    /// The subprocess's whole env.
    pub env: BTreeMap<String, String>,
    pub verb: Verb,
    pub detach: bool,
}

/// Params of verb `method` into a [`VerbRequest`]: `invalid` for bad params
/// (a relative `root`, a missing `env`, `instance` and `all` both or
/// neither), `not-found` for a `root` that isn't a directory.
pub fn parse(method: &str, params: Value) -> Result<VerbRequest, ApiError> {
    let (root, env, verb, detach) = match method {
        "run" => {
            let p: RunParams = api::parse(params)?;
            let verb = Verb::Run { sandbox: p.sandbox, name: p.name, branch: p.branch, base: p.base, json: p.json };
            (p.root, p.env, verb, p.detach)
        }
        "rebuild" => {
            let p: RebuildParams = api::parse(params)?;
            (p.root, p.env, Verb::Rebuild { target: target(p.instance, p.all)?, force: p.force }, p.detach)
        }
        "start" => {
            let p: StartParams = api::parse(params)?;
            (p.root, p.env, Verb::Start { target: target(p.instance, p.all)? }, p.detach)
        }
        "stop" => {
            let p: StopParams = api::parse(params)?;
            (p.root, p.env, Verb::Stop { target: target(p.instance, p.all)? }, false)
        }
        "rm" => {
            let p: RmParams = api::parse(params)?;
            let verb = Verb::Rm { instance: p.instance, delete_branch: p.delete_branch, force: p.force, ask: p.ask };
            (p.root, p.env, verb, false)
        }
        other => return Err(ApiError::unknown_method(other)),
    };
    if !root.is_absolute() {
        return Err(ApiError::invalid(format!("`root` must be absolute, got `{}`", root.display())));
    }
    if !root.is_dir() {
        return Err(ApiError::not_found(format!("no directory {}", root.display())));
    }
    for (key, value) in &env {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(ApiError::invalid(format!("bad `env` entry `{key}`")));
        }
    }
    Ok(VerbRequest { root, env, verb, detach })
}

fn target(instance: Option<String>, all: bool) -> Result<Target, ApiError> {
    match (instance, all) {
        (Some(name), false) => Ok(Target::One(name)),
        (None, true) => Ok(Target::All),
        (Some(_), true) => Err(ApiError::invalid("give `instance` or `all: true`, not both")),
        (None, false) => Err(ApiError::invalid("missing `instance` (or `all: true`)")),
    }
}

/// The subprocess's arguments: `--in-process -C <root> <verb> --flag=value …
/// -- <target>`. `--flag=value` and `--` keep a value starting with `-` a
/// value; the target always goes in, so no pick prompt can come up.
pub fn argv(req: &VerbRequest) -> Vec<String> {
    let mut args = vec!["--in-process".to_string(), "-C".into(), req.root.to_string_lossy().into_owned()];
    args.push(req.verb.name().into());
    let flag = |args: &mut Vec<String>, name: &str, value: &Option<String>| {
        if let Some(value) = value {
            args.push(format!("--{name}={value}"));
        }
    };
    let target = match &req.verb {
        Verb::Run { sandbox, name, branch, base, json } => {
            flag(&mut args, "name", name);
            flag(&mut args, "branch", branch);
            flag(&mut args, "base", base);
            if *json {
                args.push("--json".into());
            }
            Some(sandbox)
        }
        Verb::Rebuild { target, force } => {
            if *force {
                args.push("--force".into());
            }
            one_or_all(&mut args, target)
        }
        Verb::Start { target } | Verb::Stop { target } => one_or_all(&mut args, target),
        Verb::Rm { instance, delete_branch, force, ask } => {
            match delete_branch {
                Some(true) => args.push("--delete-branch".into()),
                Some(false) => args.push("--keep-branch".into()),
                None => {}
            }
            if *force {
                args.push("--force".into());
            }
            if *ask {
                args.push("--fail-on-ask".into());
            }
            Some(instance)
        }
    };
    if let Some(target) = target {
        args.push("--".into());
        args.push(target.clone());
    }
    args
}

fn one_or_all<'a>(args: &mut Vec<String>, target: &'a Target) -> Option<&'a String> {
    match target {
        Target::One(name) => Some(name),
        Target::All => {
            args.push("--all".into());
            None
        }
    }
}

/// When a `detach` request is answered (docs/api.md, *Verbs*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetachAt {
    /// Not detached: at the exit.
    Never,
    /// Right after the spawn: nothing records the child's pid on one
    /// instance (`start`, `--all`).
    Spawn,
    /// Once an instance's `progress.pid` is the child's (`run`, `rebuild`).
    Registered,
}

fn detach_at(req: &VerbRequest) -> DetachAt {
    if !req.detach {
        return DetachAt::Never;
    }
    match &req.verb {
        Verb::Run { .. } | Verb::Rebuild { target: Target::One(_), .. } => DetachAt::Registered,
        Verb::Rebuild { .. } | Verb::Start { .. } => DetachAt::Spawn,
        Verb::Stop { .. } | Verb::Rm { .. } => DetachAt::Never,
    }
}

// ---- wire values ----------------------------------------------------------

fn output_params(request: Option<u64>, stream: &str, data: &str) -> Value {
    json!({ "request": request, "stream": stream, "data": data })
}

fn progress_params(request: Option<u64>, instance: &str, phase: &str) -> Value {
    json!({ "request": request, "instance": instance, "phase": phase })
}

fn done_result(instance: Option<&str>) -> Value {
    json!({ "instance": instance, "exit": 0 })
}

fn detached_result(instance: Option<&str>, record: Option<Value>) -> Value {
    let mut v = json!({ "instance": instance, "detached": true });
    if let Some(record) = record {
        v["record"] = record;
    }
    v
}

fn failed_data(exit: i32, instance: Option<&str>) -> Value {
    json!({ "exit": exit, "instance": instance })
}

// ---- execution ------------------------------------------------------------

/// How the daemon runs verbs. Injectable so tests run `sh -c` scripts
/// against scratch files instead of devsandbox against the user's state.
#[derive(Debug, Clone)]
pub struct Runner {
    /// The program and its leading args ([`argv`] follows):
    /// `[current_exe]` in production.
    pub program: Vec<OsString>,
    /// The spool dir: `<data-dir>/devsandbox/logs/verbs`.
    pub spool: PathBuf,
    /// The `state.toml` `progress` is read from.
    pub state: PathBuf,
}

impl Runner {
    pub fn new() -> anyhow::Result<Self> {
        let exe = std::env::current_exe().context("cannot locate the devsandbox binary")?;
        let state = State::path()?;
        let spool = state.parent().context("no data dir")?.join("logs").join("verbs");
        Ok(Self { program: vec![exe.into()], spool, state })
    }
}

/// The daemon's verb subprocesses not reaped yet, by pid, detached ones
/// included: the `verbs` holder count.
#[derive(Debug, Default)]
pub struct InFlight(Mutex<BTreeMap<u32, &'static str>>);

impl InFlight {
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn insert(&self, pid: u32, verb: &'static str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(pid, verb);
    }

    fn remove(&self, pid: u32) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
    }
}

/// The requesting connection, as the loop sees it.
pub trait Peer {
    /// Write one notification, whole. An error means the client is gone.
    fn notify(&self, method: &str, params: Value) -> io::Result<()>;
    /// Wait up to `tick` for the client to hang up; `true` once it has.
    /// Pending data (a pipelined request) is not a hang-up.
    fn hung_up(&self, tick: Duration) -> bool;
    /// The daemon is draining: its read-side shutdown is what looked like a
    /// hang-up.
    fn draining(&self) -> bool;
}

/// A verb's answer, or `Gone`: the client hung up (or the daemon is
/// draining), so nothing is sent and the connection closes.
#[derive(Debug)]
pub enum Outcome {
    Answer(Result<Value, Failure>),
    Gone,
}

/// An error response, with `data` for `failed`.
#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
    pub data: Option<Value>,
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Self {
        Failure { code: e.code, message: e.message, data: None }
    }
}

/// Run `req` to its answer, streaming to `peer` (`request` is the id the
/// notifications carry). `who` names the connection in `log` lines.
pub fn serve(
    req: &VerbRequest,
    request: Option<u64>,
    runner: &Runner,
    table: &Arc<InFlight>,
    peer: &dyn Peer,
    who: &str,
    log: fn(&str),
) -> Outcome {
    let verb = req.verb.name();
    let before = State::load_at(&runner.state).unwrap_or_default();
    // What the answer names when no instance records the child's pid (a
    // no-op rebuild, `start`, `stop`, `rm`): the request's name, resolved
    // as the verb will (it gets the same name and never prompts).
    let resolved =
        req.verb.named().and_then(|name| crate::commands::resolve_instance_noninteractive(&before, name).ok());
    let mut running = match Running::spawn(runner, req) {
        Ok(running) => running,
        Err(e) => return Outcome::Answer(Err(ApiError::internal(e).into())),
    };
    let pid = running.pid;
    table.insert(pid, verb);
    log(&format!("{who}: {verb} started (pid {pid})"));
    let label = format!("{who}: {verb} (pid {pid})");
    let all = req.verb.target() == Some(&Target::All);
    let detach = detach_at(req);
    if detach == DetachAt::Spawn {
        let instance = if all { None } else { resolved.as_deref() };
        running.reap_later(Arc::clone(table), label, log);
        return Outcome::Answer(Ok(detached_result(instance, None)));
    }

    let mut tracker = Tracker::new(pid, before.instances.keys().cloned().collect());
    let mut next_poll = Instant::now();
    let status = loop {
        if let Err(e) = running.pump(request, peer) {
            return running.gone(peer, cancels(&e), Arc::clone(table), label, log);
        }
        match running.child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        if Instant::now() >= next_poll {
            next_poll = Instant::now() + STATE_POLL;
            if let Ok(state) = State::load_at(&runner.state) {
                for (instance, phase) in tracker.observe(&state, Progress::current_view) {
                    if let Err(e) = peer.notify("progress", progress_params(request, &instance, &phase)) {
                        return running.gone(peer, cancels(&e), Arc::clone(table), label, log);
                    }
                }
                if let (DetachAt::Registered, Some(key)) = (detach, tracker.instance.clone()) {
                    let record = match &req.verb {
                        Verb::Run { json: true, .. } => state.instances.get(&key).and_then(|info| {
                            serde_json::to_value(crate::commands::run::RunRecord::new(&key, info)).ok()
                        }),
                        _ => None,
                    };
                    running.reap_later(Arc::clone(table), label, log);
                    return Outcome::Answer(Ok(detached_result(Some(&key), record)));
                }
            }
        }
        if peer.hung_up(TICK) {
            return running.gone(peer, true, Arc::clone(table), label, log);
        }
    };

    // Output before the answer: the files are complete now.
    let _ = running.pump(request, peer);
    let _ = running.flush(request, peer);
    let mut added = None;
    if let Ok(state) = State::load_at(&runner.state) {
        for (instance, phase) in tracker.observe(&state, Progress::current_view) {
            let _ = peer.notify("progress", progress_params(request, &instance, &phase));
        }
        added = matches!(req.verb, Verb::Run { .. }).then(|| tracker.added(&state)).flatten();
    }
    let stderr = String::from_utf8_lossy(&running.err.tail).into_owned();
    running.finish(table);
    let status = match status {
        Ok(status) => status,
        Err(e) => return Outcome::Answer(Err(ApiError::internal(anyhow::Error::new(e).context("cannot wait")).into())),
    };
    let exit = exit_code(status);
    log(&format!("{label} exited {exit}"));
    let instance = if all { None } else { tracker.instance.or(added).or(resolved) };
    if exit == 0 {
        return Outcome::Answer(Ok(done_result(instance.as_deref())));
    }
    let message = error_message(&stderr);
    if matches!(req.verb, Verb::Rm { ask: true, .. }) && exit == crate::commands::rm::NEEDS_ANSWER_EXIT {
        if let Some(question) = message {
            return Outcome::Answer(Err(Failure { code: "confirm", message: question, data: None }));
        }
    }
    Outcome::Answer(Err(Failure {
        code: "failed",
        message: message.unwrap_or_else(|| format!("`devsandbox {verb}` exited with {exit}")),
        data: Some(failed_data(exit, instance.as_deref())),
    }))
}

/// A spawned verb: the child and its two spool files.
struct Running {
    child: Child,
    pid: u32,
    out: Spool,
    err: Spool,
}

/// One spool file and the read side the loop tails.
struct Spool {
    path: PathBuf,
    file: File,
    /// The start of a UTF-8 sequence cut by the last read, sent with the next.
    held: Vec<u8>,
    /// The last [`ERR_TAIL`] bytes, kept for stderr only.
    tail: Vec<u8>,
    keep_tail: bool,
}

impl Running {
    fn spawn(runner: &Runner, req: &VerbRequest) -> anyhow::Result<Running> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(&runner.spool).with_context(|| format!("cannot create {}", runner.spool.display()))?;
        // Temp names until the pid is known; the daemon's pid keeps two
        // daemons' (a draining one and its successor) apart.
        let stem = format!(".{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
        let (out_w, out) = Spool::create(&runner.spool.join(format!("{stem}.out.tmp")), false)?;
        let (err_w, err) = match Spool::create(&runner.spool.join(format!("{stem}.err.tmp")), true) {
            Ok(created) => created,
            Err(e) => {
                let _ = std::fs::remove_file(&out.path);
                return Err(e);
            }
        };
        let (program, lead) = runner.program.split_first().context("no verb program")?;
        let mut cmd = Command::new(program);
        cmd.args(lead)
            .args(argv(req))
            .stdin(Stdio::null())
            .stdout(out_w)
            .stderr(err_w)
            .env_clear()
            .envs(&req.env)
            .env("GIT_TERMINAL_PROMPT", "0");
        // Own process group: a cancel's SIGINT reaches docker and git under
        // it, and nothing aimed at the daemon's group reaches it.
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = std::fs::remove_file(&out.path);
                let _ = std::fs::remove_file(&err.path);
                return Err(e).with_context(|| format!("cannot run {}", Path::new(program).display()));
            }
        };
        let pid = child.id();
        let mut running = Running { child, pid, out, err };
        running.out.rename(&runner.spool.join(format!("{pid}.out")));
        running.err.rename(&runner.spool.join(format!("{pid}.err")));
        Ok(running)
    }

    /// Send what both files gained, as `output`. An error: the client is gone.
    fn pump(&mut self, request: Option<u64>, peer: &dyn Peer) -> io::Result<()> {
        self.out.pump("stdout", request, peer)?;
        self.err.pump("stderr", request, peer)
    }

    /// Send the held-back bytes too (the child is done).
    fn flush(&mut self, request: Option<u64>, peer: &dyn Peer) -> io::Result<()> {
        for (spool, stream) in [(&mut self.out, "stdout"), (&mut self.err, "stderr")] {
            if !spool.held.is_empty() {
                let data = std::mem::take(&mut spool.held);
                peer.notify("output", output_params(request, stream, &String::from_utf8_lossy(&data)))?;
            }
        }
        Ok(())
    }

    /// The client is gone (hung up, or its writes fail): SIGINT the group
    /// (the verb rolls back) when `cancel`, unless the daemon is draining;
    /// otherwise the verb runs on. Either way it's reaped off this thread.
    fn gone(self, peer: &dyn Peer, cancel: bool, table: Arc<InFlight>, label: String, log: fn(&str)) -> Outcome {
        if peer.draining() {
            log(&format!("{label}: the daemon is exiting, left running"));
        } else if !cancel {
            log(&format!("{label}: client stopped reading, connection cut, left running"));
        } else {
            // The child leads its own group and isn't reaped yet, so its
            // pid can't have been reused.
            match crate::commands::run::cancel::interrupt(self.pid) {
                Ok(()) => log(&format!("{label}: client hung up, SIGINT sent")),
                Err(e) => log(&format!("{label}: client hung up, {e}")),
            }
        }
        self.reap_later(table, label, log);
        Outcome::Gone
    }

    /// Hand the child to a thread that reaps it and cleans up, so the
    /// connection is free (a detached answer) or can close.
    fn reap_later(self, table: Arc<InFlight>, label: String, log: fn(&str)) {
        type Slot = Mutex<Option<(Running, Arc<InFlight>, String)>>;
        fn reap(slot: &Slot, log: fn(&str)) {
            let taken = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some((mut running, table, label)) = taken {
                let status = running.child.wait();
                running.finish(&table);
                match status {
                    Ok(status) => log(&format!("{label} exited {}", exit_code(status))),
                    Err(e) => log(&format!("{label}: cannot wait: {e}")),
                }
            }
        }
        let name = format!("serve-verb-{}", self.pid);
        let slot: Arc<Slot> = Arc::new(Mutex::new(Some((self, table, label))));
        let theirs = Arc::clone(&slot);
        if let Err(e) = std::thread::Builder::new().name(name).spawn(move || reap(&theirs, log)) {
            // Never leave a zombie and a holder behind: wait here instead.
            log(&format!("cannot start a reaper thread ({e}), waiting inline"));
            reap(&slot, log);
        }
    }

    /// The exit is reported (or nobody is left to report it to): the spool
    /// files and the table entry go.
    fn finish(&mut self, table: &InFlight) {
        let _ = std::fs::remove_file(&self.out.path);
        let _ = std::fs::remove_file(&self.err.path);
        table.remove(self.pid);
    }
}

impl Spool {
    /// The writable file for the child and the read side, 0600: verb output
    /// can carry secrets.
    fn create(path: &Path, keep_tail: bool) -> anyhow::Result<(File, Spool)> {
        use std::os::unix::fs::OpenOptionsExt;
        let write = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        Ok((write, Spool { path: path.to_path_buf(), file, held: Vec::new(), tail: Vec::new(), keep_tail }))
    }

    /// Best effort: an unrenamed file still works (the fds stay valid), it
    /// just isn't findable by pid.
    fn rename(&mut self, to: &Path) {
        if std::fs::rename(&self.path, to).is_ok() {
            self.path = to.to_path_buf();
        }
    }

    fn pump(&mut self, stream: &str, request: Option<u64>, peer: &dyn Peer) -> io::Result<()> {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = match self.file.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // An unreadable spool loses output, not the verb.
                Err(_) => return Ok(()),
            };
            self.held.extend_from_slice(&buf[..n]);
            let cut = complete_utf8(&self.held);
            if cut == 0 {
                continue;
            }
            let data: Vec<u8> = self.held.drain(..cut).collect();
            if self.keep_tail {
                self.tail.extend_from_slice(&data);
                let over = self.tail.len().saturating_sub(ERR_TAIL);
                self.tail.drain(..over);
            }
            peer.notify("output", output_params(request, stream, &String::from_utf8_lossy(&data)))?;
        }
    }
}

/// How much of `buf` to send now: all of it but a trailing UTF-8 sequence
/// the read cut short, so a character split across reads isn't mangled.
/// Invalid bytes aren't held (the lossy decode replaces them).
fn complete_utf8(buf: &[u8]) -> usize {
    let len = buf.len();
    for back in 1..=len.min(3) {
        let b = buf[len - back];
        if b & 0xC0 == 0x80 {
            continue; // a continuation byte: look further back for the lead
        }
        let need = match b {
            0xF0.. => 4,
            0xE0.. => 3,
            0xC0.. => 2,
            _ => 1,
        };
        return if need > back { len - back } else { len };
    }
    len
}

/// The verb's subprocess tracked through `state.toml`: every instance whose
/// `progress.pid` is the child's, and the phase last sent for each.
struct Tracker {
    pid: u32,
    /// The instances before the spawn (for [`Tracker::added`]).
    before: BTreeSet<String>,
    sent: BTreeMap<String, String>,
    /// The first instance seen with the child's pid: the verb's instance.
    instance: Option<String>,
}

impl Tracker {
    fn new(pid: u32, before: BTreeSet<String>) -> Self {
        Self { pid, before, sent: BTreeMap::new(), instance: None }
    }

    /// The `(instance, phase)` changes since the last call; `phase` is the
    /// `PhaseView` label, or `failed`. `view` is injected to keep it pure.
    fn observe(&mut self, state: &State, view: impl Fn(&Progress) -> PhaseView) -> Vec<(String, String)> {
        let mut changes = Vec::new();
        for (key, info) in &state.instances {
            let Some(progress) = info.progress.as_ref().filter(|p| p.pid == self.pid) else { continue };
            let phase = match view(progress) {
                PhaseView::InFlight(label) => label,
                PhaseView::Failed { .. } => "failed".to_string(),
            };
            self.instance.get_or_insert_with(|| key.clone());
            if self.sent.get(key) != Some(&phase) {
                self.sent.insert(key.clone(), phase.clone());
                changes.push((key.clone(), phase));
            }
        }
        changes
    }

    /// The one instance added since the spawn: a `run` that went from start
    /// to ready between two polls.
    fn added(&self, state: &State) -> Option<String> {
        let mut new = state.instances.keys().filter(|k| !self.before.contains(*k));
        let first = new.next()?;
        new.next().is_none().then(|| first.clone())
    }
}

/// Whether a failed write to the client means it hung up (cancel), not that
/// it stopped reading for the write timeout: a paused `devsandbox run | less`
/// mustn't roll a build back. Its connection is cut either way (a timed-out
/// write may have left half a line).
fn cancels(e: &io::Error) -> bool {
    !matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// The `exit` of a `failed`: the code, or 128 + the signal, as shells say.
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// A failed verb's message from its stderr: `main`'s last `Error: …` line
/// without the prefix, its `Caused by:` chain joined as `{:#}` would. Else
/// the last non-empty line (a panic, a signal).
fn error_message(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr.lines().collect();
    let Some(at) = lines.iter().rposition(|l| l.starts_with("Error: ") || l.starts_with("error: ")) else {
        return lines.iter().map(|l| l.trim()).rfind(|l| !l.is_empty()).map(str::to_string);
    };
    let mut message = lines[at]["Error: ".len()..].trim().to_string();
    let rest = &lines[at + 1..];
    if let Some(c) = rest.iter().position(|l| l.trim() == "Caused by:") {
        if rest[..c].iter().all(|l| l.trim().is_empty()) {
            for line in rest[c + 1..].iter().map(|l| l.trim()).take_while(|l| !l.is_empty()) {
                // Several causes are numbered (`0: …`), a single one isn't.
                let cause = match line.split_once(": ") {
                    Some((n, cause)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => cause,
                    _ => line,
                };
                message.push_str(": ");
                message.push_str(cause);
            }
        }
    }
    Some(message)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::serve::daemon::tests::scratch;

    fn env() -> BTreeMap<String, String> {
        // `sh` scripts need a PATH; a real client sends its whole env.
        BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap_or_default())])
    }

    fn req(verb: Verb, detach: bool) -> VerbRequest {
        VerbRequest { root: "/cfg".into(), env: env(), verb, detach }
    }

    fn one(name: &str) -> Target {
        Target::One(name.into())
    }

    #[test]
    fn argv_maps_each_verb_to_its_cli_flags() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let lead = ["--in-process", "-C", "/cfg"];
        let with = |rest: &[&str]| s(&[&lead[..], rest].concat());
        let run = Verb::Run { sandbox: "web".into(), name: None, branch: None, base: None, json: false };
        assert_eq!(argv(&req(run, false)), with(&["run", "--", "web"]));
        let run = Verb::Run {
            sandbox: "-web".into(),
            name: Some("w2".into()),
            branch: Some("feat/x".into()),
            base: Some("-odd".into()),
            json: true,
        };
        assert_eq!(
            argv(&req(run, true)),
            with(&["run", "--name=w2", "--branch=feat/x", "--base=-odd", "--json", "--", "-web"])
        );
        assert_eq!(argv(&req(Verb::Rebuild { target: one("w"), force: true }, false)), with(&["rebuild", "--force", "--", "w"]));
        assert_eq!(argv(&req(Verb::Rebuild { target: Target::All, force: false }, false)), with(&["rebuild", "--all"]));
        assert_eq!(argv(&req(Verb::Start { target: Target::All }, false)), with(&["start", "--all"]));
        assert_eq!(argv(&req(Verb::Stop { target: one("w") }, false)), with(&["stop", "--", "w"]));
        let rm = |delete_branch, force, ask| Verb::Rm { instance: "w".into(), delete_branch, force, ask };
        assert_eq!(argv(&req(rm(None, false, false), false)), with(&["rm", "--", "w"]));
        assert_eq!(argv(&req(rm(Some(true), true, false), false)), with(&["rm", "--delete-branch", "--force", "--", "w"]));
        assert_eq!(argv(&req(rm(Some(false), false, false), false)), with(&["rm", "--keep-branch", "--", "w"]));
        assert_eq!(argv(&req(rm(None, false, true), false)), with(&["rm", "--fail-on-ask", "--", "w"]));
    }

    /// The argv parses as the CLI's own verb with the request's values, so a
    /// flag rename can't drift.
    #[test]
    fn argv_parses_as_the_cli() {
        use crate::{Cli, Command};
        use clap::Parser;
        let parse = |verb: Verb| {
            let argv = std::iter::once("devsandbox".to_string()).chain(argv(&req(verb, false)));
            let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{e}"));
            assert!(cli.in_process);
            assert_eq!(cli.dir, Path::new("/cfg"));
            cli.command.unwrap()
        };
        let run = Verb::Run {
            sandbox: "-web".into(),
            name: Some("w2".into()),
            branch: Some("-b".into()),
            base: Some("origin/main".into()),
            json: true,
        };
        match parse(run) {
            Command::Run { sandbox, name, branch, base, env, dispatcher, json, detach } => {
                assert_eq!(sandbox.as_deref(), Some("-web"));
                assert_eq!((name.as_deref(), branch.as_deref(), base.as_deref()), (Some("w2"), Some("-b"), Some("origin/main")));
                // The daemon detaches by itself: the subprocess never gets `-d`.
                assert!(json && env.is_empty() && dispatcher.is_none() && !detach);
            }
            _ => panic!("not run"),
        }
        let run = Verb::Run { sandbox: "web".into(), name: None, branch: None, base: None, json: false };
        assert!(matches!(parse(run), Command::Run { name: None, branch: None, base: None, json: false, .. }));
        match parse(Verb::Rebuild { target: one("-x"), force: true }) {
            Command::Rebuild { name, all, force, detach: false } => {
                assert_eq!((name.as_deref(), all, force), (Some("-x"), false, true))
            }
            _ => panic!("not rebuild"),
        }
        assert!(matches!(parse(Verb::Rebuild { target: Target::All, force: false }), Command::Rebuild { name: None, all: true, force: false, detach: false }));
        assert!(matches!(parse(Verb::Start { target: one("w") }), Command::Start { name: Some(n), all: false, detach: false } if n == "w"));
        assert!(matches!(parse(Verb::Start { target: Target::All }), Command::Start { name: None, all: true, detach: false }));
        assert!(matches!(parse(Verb::Stop { target: one("w") }), Command::Stop { name: Some(n), all: false } if n == "w"));
        assert!(matches!(parse(Verb::Stop { target: Target::All }), Command::Stop { name: None, all: true }));
        for (delete_branch, flags) in [(None, (false, false)), (Some(true), (true, false)), (Some(false), (false, true))] {
            for ask in [false, true] {
                match parse(Verb::Rm { instance: "w".into(), delete_branch, force: true, ask }) {
                    Command::Rm { name, delete_branch, keep_branch, force, fail_on_ask } => assert_eq!(
                        (name.as_str(), (delete_branch, keep_branch), force, fail_on_ask),
                        ("w", flags, true, ask)
                    ),
                    _ => panic!("not rm"),
                }
            }
        }
    }

    #[test]
    fn params_are_checked() {
        let root = scratch("verbs-params");
        std::fs::create_dir_all(&root).unwrap();
        let code = |method: &str, params: Value| parse(method, params).map(|_| ()).map_err(|e| e.code);
        let r = root.to_str().unwrap();
        assert_eq!(code("stop", json!({"root": r, "env": {}, "instance": "w"})), Ok(()));
        assert_eq!(code("stop", json!({"root": r, "env": {}, "all": true})), Ok(()));
        assert_eq!(code("stop", json!({"root": "rel", "env": {}, "all": true})), Err("invalid"));
        assert_eq!(code("stop", json!({"root": "/nonexistent/dsv", "env": {}, "all": true})), Err("not-found"));
        // Never the daemon's env.
        assert_eq!(code("stop", json!({"root": r, "all": true})), Err("invalid"));
        assert_eq!(code("stop", json!({"root": r, "env": {"A": 1}, "all": true})), Err("invalid"));
        assert_eq!(code("stop", json!({"root": r, "env": {"A=B": "x"}, "all": true})), Err("invalid"));
        assert_eq!(code("stop", json!({"root": r, "env": {}})), Err("invalid"));
        assert_eq!(code("rebuild", json!({"root": r, "env": {}, "instance": "w", "all": true})), Err("invalid"));
        assert_eq!(code("run", json!({"root": r, "env": {}})), Err("invalid"));
        assert_eq!(code("rm", json!({"root": r, "env": {}})), Err("invalid"));
        let p = parse("rm", json!({"root": r, "env": {}, "instance": "w", "deleteBranch": false})).unwrap();
        assert_eq!(p.verb, Verb::Rm { instance: "w".into(), delete_branch: Some(false), force: false, ask: false });
        let p = parse("rm", json!({"root": r, "env": {}, "instance": "w", "ask": true})).unwrap();
        assert_eq!(p.verb, Verb::Rm { instance: "w".into(), delete_branch: None, force: false, ask: true });
        let p = parse("start", json!({"root": r, "env": {"X": "1"}, "instance": "w", "detach": true})).unwrap();
        assert!(p.detach && p.env["X"] == "1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detach_answers_at_registration_or_spawn() {
        let run = Verb::Run { sandbox: "web".into(), name: None, branch: None, base: None, json: false };
        assert_eq!(detach_at(&req(run.clone(), false)), DetachAt::Never);
        assert_eq!(detach_at(&req(run, true)), DetachAt::Registered);
        assert_eq!(detach_at(&req(Verb::Rebuild { target: one("w"), force: false }, true)), DetachAt::Registered);
        assert_eq!(detach_at(&req(Verb::Rebuild { target: Target::All, force: false }, true)), DetachAt::Spawn);
        assert_eq!(detach_at(&req(Verb::Start { target: one("w") }, true)), DetachAt::Spawn);
    }

    #[test]
    fn utf8_split_across_reads_is_held_back() {
        let euro = "€".as_bytes(); // 3 bytes
        assert_eq!(complete_utf8(b"abc"), 3);
        assert_eq!(complete_utf8(&[b'a', euro[0]]), 1);
        assert_eq!(complete_utf8(&[b'a', euro[0], euro[1]]), 1);
        assert_eq!(complete_utf8(&[b'a', euro[0], euro[1], euro[2]]), 4);
        // Garbage isn't held forever.
        assert_eq!(complete_utf8(&[0x80, 0x80, 0x80, 0x80]), 4);
        assert_eq!(complete_utf8(&[]), 0);
    }

    #[test]
    fn error_message_is_mains_error_with_its_causes() {
        assert_eq!(error_message("warning: x\nError: `web` cancelled: rolled back\n").as_deref(), Some("`web` cancelled: rolled back"));
        let chain = "building\nError: cannot start web\n\nCaused by:\n    0: docker run failed\n    1: exit 125\n";
        assert_eq!(error_message(chain).as_deref(), Some("cannot start web: docker run failed: exit 125"));
        let single = "Error: cannot start web\n\nCaused by:\n    no space left\n";
        assert_eq!(error_message(single).as_deref(), Some("cannot start web: no space left"));
        assert_eq!(error_message("thread 'main' panicked\n\n").as_deref(), Some("thread 'main' panicked"));
        assert_eq!(error_message(" \n"), None);
    }

    fn state(toml: &str) -> State {
        toml::from_str(toml).unwrap()
    }

    fn instance_toml(key: &str, progress: &str) -> String {
        format!(
            "[instance.{key}]\nsandbox = \"web\"\ncontainer = \"devsandbox-{key}\"\nfolder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\n{progress}"
        )
    }

    #[test]
    fn tracker_reports_phase_changes_of_the_childs_instances() {
        let mut t = Tracker::new(42, BTreeSet::from(["old".to_string()]));
        let alive = |p: &Progress| p.view(|_| true, None);
        let progress = |key: &str, phase: &str, pid: u32| {
            instance_toml(key, &format!("[instance.{key}.progress]\nphase = \"{phase}\"\npid = {pid}\n"))
        };
        let s = state(&format!("{}{}", progress("web-2", "building", 42), progress("other", "building", 7)));
        assert_eq!(t.observe(&s, alive), vec![("web-2".to_string(), "building".to_string())]);
        assert_eq!(t.observe(&s, alive), vec![], "unchanged: nothing");
        assert_eq!(t.instance.as_deref(), Some("web-2"));
        let s = state(&progress("web-2", "failed", 42));
        assert_eq!(t.observe(&s, alive), vec![("web-2".to_string(), "failed".to_string())]);
        // A dead pid's in-flight phase reads as failed (interrupted).
        let mut t = Tracker::new(42, BTreeSet::new());
        let dead = |p: &Progress| p.view(|_| false, None);
        assert_eq!(t.observe(&state(&progress("w", "creating", 42)), dead), vec![("w".to_string(), "failed".to_string())]);

        let t = Tracker::new(42, BTreeSet::from(["old".to_string()]));
        assert_eq!(t.added(&state(&format!("{}{}", instance_toml("old", ""), instance_toml("new", "")))).as_deref(), Some("new"));
        assert_eq!(t.added(&state(&format!("{}{}", instance_toml("a", ""), instance_toml("b", "")))), None, "two: unknown");
    }

    /// A fake connection: records notifications, hangs up on cue.
    #[derive(Default)]
    pub(crate) struct FakePeer {
        notes: Mutex<Vec<(String, Value)>>,
        hang_up_at: Mutex<Option<Instant>>,
        draining: std::sync::atomic::AtomicBool,
    }

    impl Peer for FakePeer {
        fn notify(&self, method: &str, params: Value) -> io::Result<()> {
            self.notes.lock().unwrap().push((method.to_string(), params));
            Ok(())
        }

        fn hung_up(&self, tick: Duration) -> bool {
            if self.hang_up_at.lock().unwrap().is_some_and(|at| Instant::now() >= at) {
                return true;
            }
            std::thread::sleep(tick);
            false
        }

        fn draining(&self) -> bool {
            self.draining.load(Ordering::SeqCst)
        }
    }

    impl FakePeer {
        fn output(&self, stream: &str) -> String {
            let notes = self.notes.lock().unwrap();
            notes
                .iter()
                .filter(|(m, p)| m == "output" && p["stream"] == stream)
                .map(|(_, p)| p["data"].as_str().unwrap().to_string())
                .collect()
        }

        fn progress(&self) -> Vec<(String, String)> {
            let notes = self.notes.lock().unwrap();
            notes
                .iter()
                .filter(|(m, _)| m == "progress")
                .map(|(_, p)| (p["instance"].as_str().unwrap().into(), p["phase"].as_str().unwrap().into()))
                .collect()
        }
    }

    /// A runner whose verbs are `script` (`sh -c`, the argv as `$1…`), with
    /// a scratch spool and state; `$STATE` and `$DIR` point at them.
    pub(crate) fn script_runner(tag: &str, script: &str) -> (Runner, PathBuf) {
        let dir = scratch(&format!("verbs-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!("STATE='{}'; DIR='{}'; {script}", dir.join("state.toml").display(), dir.display());
        let runner = Runner {
            program: vec!["/bin/sh".into(), "-c".into(), script.into(), "sh".into()],
            spool: dir.join("spool"),
            state: dir.join("state.toml"),
        };
        (runner, dir)
    }

    fn wait_for(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(name: &str) -> VerbRequest {
        req(Verb::Stop { target: one(name) }, false)
    }

    #[test]
    fn a_verb_streams_its_output_and_answers_its_exit() {
        let (runner, dir) = script_runner("ok", r#"echo "args: $*"; echo e1 >&2; printf 'o2\n'; printf 'e2 €\n' >&2"#);
        let table = Arc::new(InFlight::default());
        let peer = FakePeer::default();
        let out = serve(&stop("web"), Some(7), &runner, &table, &peer, "test", |_| {});
        // No such instance in the (empty) state: unknown.
        assert_eq!(out.answer().unwrap(), json!({"instance": null, "exit": 0}));
        assert_eq!(peer.output("stdout"), "args: --in-process -C /cfg stop -- web\no2\n");
        assert_eq!(peer.output("stderr"), "e1\ne2 €\n");
        assert!(peer.notes.lock().unwrap().iter().all(|(_, p)| p["request"] == 7));
        assert_eq!(table.len(), 0);
        // Spool files are gone once the exit is reported.
        assert_eq!(std::fs::read_dir(&runner.spool).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_is_the_requests_only() {
        let (runner, dir) = script_runner("env", r#"echo "${HOME:-nohome} $GIT_TERMINAL_PROMPT $MINE""#);
        let mut r = stop("web");
        r.env.insert("MINE".into(), "mine".into());
        let peer = FakePeer::default();
        serve(&r, None, &runner, &Arc::default(), &peer, "test", |_| {}).answer().unwrap();
        assert_eq!(peer.output("stdout"), "nohome 0 mine\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_verb_answers_failed_with_its_exit_and_error() {
        let (runner, dir) = script_runner(
            "fail",
            &format!("printf '{}' > \"$STATE\"; echo 'Error: no space' >&2; exit 3", instance_toml("web-2", "")),
        );
        let peer = FakePeer::default();
        let out = serve(&stop("web"), Some(1), &runner, &Arc::default(), &peer, "test", |_| {});
        let f = out.answer().unwrap_err();
        assert_eq!((f.code, f.message.as_str()), ("failed", "no space"));
        assert_eq!(f.data, Some(json!({"exit": 3, "instance": null})));
        // A name that resolves (sandbox `web` → its one instance) is named.
        let out = serve(&stop("web"), Some(2), &runner, &Arc::default(), &peer, "test", |_| {});
        assert_eq!(out.answer().unwrap_err().data, Some(json!({"exit": 3, "instance": "web-2"})));
        // Killed by a signal: 128 + it.
        let (runner, dir2) = script_runner("killed", "kill -9 $$");
        let f = serve(&stop("x"), None, &runner, &Arc::default(), &peer, "test", |_| {}).answer().unwrap_err();
        assert_eq!(f.data.unwrap()["exit"], 137);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn an_rm_that_must_ask_answers_confirm_with_the_question() {
        let q = "delete branch `b`? it has commits not on its remote";
        let (runner, dir) = script_runner("confirm", &format!("echo 'Error: {q}' >&2; exit 3"));
        let rm = |ask| req(Verb::Rm { instance: "w".into(), delete_branch: None, force: false, ask }, false);
        let peer = FakePeer::default();
        let f = serve(&rm(true), None, &runner, &Arc::default(), &peer, "test", |_| {}).answer().unwrap_err();
        assert_eq!((f.code, f.message.as_str(), f.data), ("confirm", q, None));
        // Not asked for: exit 3 is any failure.
        let f = serve(&rm(false), None, &runner, &Arc::default(), &peer, "test", |_| {}).answer().unwrap_err();
        assert_eq!((f.code, f.data), ("failed", Some(json!({"exit": 3, "instance": null}))));
        let (runner, dir2) = script_runner("confirm-other", "echo 'Error: boom' >&2; exit 1");
        let f = serve(&rm(true), None, &runner, &Arc::default(), &peer, "test", |_| {}).answer().unwrap_err();
        assert_eq!((f.code, f.message.as_str()), ("failed", "boom"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// `$` is the child's pid: the script records itself as the verb does.
    fn phase_script(steps: &[&str]) -> String {
        let mut script = String::new();
        for phase in steps {
            let progress = match *phase {
                "ready" => String::new(),
                phase => format!("[instance.web-2.progress]\nphase = \"{phase}\"\npid = %s\n"),
            };
            let toml = instance_toml("web-2", &progress);
            script.push_str(&format!("printf '{toml}' \"$$\" > \"$STATE.tmp\"; mv \"$STATE.tmp\" \"$STATE\"; sleep 0.4; "));
        }
        script
    }

    #[test]
    fn progress_follows_the_childs_instance() {
        let (runner, dir) = script_runner("progress", &phase_script(&["preparing", "building", "ready"]));
        let run = Verb::Run { sandbox: "web".into(), name: None, branch: None, base: None, json: false };
        let peer = FakePeer::default();
        let out = serve(&req(run, false), Some(3), &runner, &Arc::default(), &peer, "test", |_| {});
        assert_eq!(out.answer().unwrap(), json!({"instance": "web-2", "exit": 0}));
        assert_eq!(
            peer.progress(),
            [("web-2".to_string(), "preparing".to_string()), ("web-2".to_string(), "building".to_string())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detach_answers_once_registered_and_the_verb_runs_on() {
        let script = format!("{} touch \"$DIR/finished\"", phase_script(&["building", "building", "building", "ready"]));
        let (runner, dir) = script_runner("detach", &script);
        let run = Verb::Run { sandbox: "web".into(), name: None, branch: None, base: None, json: true };
        let table = Arc::new(InFlight::default());
        let peer = FakePeer::default();
        let started = Instant::now();
        let out = serve(&req(run, true), Some(4), &runner, &table, &peer, "test", |_| {});
        let answer = out.answer().unwrap();
        assert!(started.elapsed() < Duration::from_millis(1000), "{:?}", started.elapsed());
        assert_eq!((&answer["instance"], &answer["detached"]), (&json!("web-2"), &json!(true)));
        assert_eq!(answer["record"]["name"], "web-2");
        assert_eq!(answer["record"]["container"], "devsandbox-web-2");
        // Still a holder until reaped, then cleaned up.
        assert_eq!(table.len(), 1);
        wait_for("the reaper", || table.len() == 0);
        assert!(dir.join("finished").exists());
        assert_eq!(std::fs::read_dir(&runner.spool).unwrap().count(), 0);

        // `start`: right after the spawn.
        let (runner, dir2) = script_runner("detach-start", "sleep 0.5");
        let started = Instant::now();
        let out = serve(&req(Verb::Start { target: Target::All }, true), None, &runner, &table, &peer, "test", |_| {});
        assert_eq!(out.answer().unwrap(), json!({"instance": null, "detached": true}));
        assert!(started.elapsed() < Duration::from_millis(400));
        wait_for("the reaper", || table.len() == 0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    const TRAPPED: &str = r#"trap 'echo int > "$DIR/int"; exit 130' INT; echo ready; sleep 1; touch "$DIR/done""#;

    #[test]
    fn a_hang_up_sigints_the_group() {
        let (runner, dir) = script_runner("hangup", TRAPPED);
        let table = Arc::new(InFlight::default());
        let peer = FakePeer::default();
        *peer.hang_up_at.lock().unwrap() = Some(Instant::now() + Duration::from_millis(300));
        let out = serve(&stop("w"), None, &runner, &table, &peer, "test", |_| {});
        assert!(matches!(out, Outcome::Gone));
        wait_for("the trap", || dir.join("int").exists());
        wait_for("the reaper", || table.len() == 0);
        assert!(!dir.join("done").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drain_is_not_a_hang_up() {
        let (runner, dir) = script_runner("drain", TRAPPED);
        let table = Arc::new(InFlight::default());
        let peer = FakePeer::default();
        *peer.hang_up_at.lock().unwrap() = Some(Instant::now() + Duration::from_millis(300));
        peer.draining.store(true, Ordering::SeqCst);
        let out = serve(&stop("w"), None, &runner, &table, &peer, "test", |_| {});
        assert!(matches!(out, Outcome::Gone));
        wait_for("the verb", || dir.join("done").exists());
        assert!(!dir.join("int").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_hang_up_write_error_cancels() {
        use io::ErrorKind::*;
        for kind in [BrokenPipe, ConnectionReset, NotConnected] {
            assert!(cancels(&kind.into()), "{kind:?}");
        }
        // The write timeout: a client that stopped reading (a paused pager).
        for kind in [WouldBlock, TimedOut] {
            assert!(!cancels(&kind.into()), "{kind:?}");
        }
    }

    impl Outcome {
        fn answer(self) -> Result<Value, Failure> {
            match self {
                Outcome::Answer(a) => a,
                Outcome::Gone => panic!("no answer"),
            }
        }
    }

    /// Every verb's params, results and notifications match `index.d.ts`.
    #[test]
    fn npm_typings_match_the_verb_wire() {
        use crate::serve::dts::{assert_matches, prop_type};
        let v = |x: &dyn erased::Ser| x.value();
        let (root, e) = (PathBuf::from("/cfg"), BTreeMap::new());
        let run = RunParams {
            root: root.clone(),
            env: e.clone(),
            sandbox: "web".into(),
            name: None,
            branch: None,
            base: None,
            json: true,
            detach: true,
        };
        let rebuild =
            RebuildParams { root: root.clone(), env: e.clone(), instance: None, all: true, force: false, detach: false };
        let start = StartParams { root: root.clone(), env: e.clone(), instance: None, all: true, detach: false };
        let stop = StopParams { root: root.clone(), env: e.clone(), instance: None, all: true };
        let rm = RmParams { root, env: e, instance: "w".into(), delete_branch: None, force: false, ask: true };
        for (iface, value) in [
            ("RunParams", v(&run)),
            ("RebuildParams", v(&rebuild)),
            ("StartParams", v(&start)),
            ("StopParams", v(&stop)),
            ("RmParams", v(&rm)),
            ("VerbResult", done_result(Some("w"))),
            ("VerbDetached", detached_result(Some("w"), Some(json!({})))),
            ("VerbFailedData", failed_data(1, None)),
            ("VerbOutput", output_params(Some(1), "stdout", "x")),
            ("VerbProgress", progress_params(Some(1), "w", "building")),
        ] {
            assert_matches(iface, &value);
        }
        assert_eq!(prop_type("VerbDetached", "record").as_deref(), Some("RunRecord"));
        for method in ["run", "rebuild", "start", "stop", "rm"] {
            assert!(prop_type("ApiMethods", method).is_some(), "ApiMethods lacks `{method}`");
        }
        assert_eq!(prop_type("ApiEvents", "output").as_deref(), Some("VerbOutput"));
        assert_eq!(prop_type("ApiEvents", "progress").as_deref(), Some("VerbProgress"));
    }

    mod erased {
        pub trait Ser {
            fn value(&self) -> serde_json::Value;
        }
        impl<T: serde::Serialize> Ser for T {
            fn value(&self) -> serde_json::Value {
                serde_json::to_value(self).unwrap()
            }
        }
    }
}
