//! The worker's end of a fleet link's `PTY` stream
//! (docs/architecture/daemon/fleet.md, *PTY*): a terminal into a lent
//! instance, as a session attach through its helper's `session.attach`
//! (`HelperClient::call_with_input`, decision 13 of
//! docs/plans/active/fleet.md: no `exec -it`, no in-container client).
//!
//! The stream carries `rpc` records (`src/devsbd/rpc.rs`). The hub's first
//! is a `call` of [`METHOD`] whose params are the [`Header`]; then its
//! `stdin`, `resize` and `stdin-eof` records go into the attach, and the
//! helper's records come back unchanged: `stdout`…, then one end record,
//! `exit` or `error`. A stream refused before the attach gets one `error`
//! record ([`code`]). The hub closing the stream detaches (the call is
//! cancelled; the session runs on in the container); an attach that ends
//! without an end record is [`code::LOST`].
//!
//! A header with `pipe` instead of `session` is a piped exec ([`Mode::Pipe`],
//! a non-tty `exec`): no session and no helper, a `<runtime> exec` without
//! `-t` on this host ([`runtime_exec`]) with its stdio piped. The hub's
//! `stdin` records go to its stdin (`stdin-eof` closes it; only when
//! `interactive`, else its stdin is empty); its stdout and stderr come back
//! as `stdout` and `stderr` records, then one `exit` once both are drained
//! and it was reaped ([`exit_record`]). The hub closing the stream first
//! kills and reaps it, and nothing more is written.
//!
//! A session header for an instance whose helper can't attach (none, or its
//! daemon doesn't offer `session.attach`: not root, or older) runs a tty
//! exec instead when it creates ([`decide`], [`Terminal::Tty`]): `<runtime>
//! exec -it` under a host pty ([`run_tty`]), not resumable (one `stderr`
//! note says so first); an attach-only one gets `no-session`. A header with
//! `service` ([`Mode::Service`]) is the same tty exec into a service
//! container the instance uses ([`service_container`]). The pty's output
//! comes back as `stdout` records, the hub's `stdin` and `resize` go into
//! it, its exit is one `exit` record; the hub closing first kills and reaps
//! it.
//!
//! [`parse_header`], [`gate`], [`decide`], [`attach_params`],
//! [`exit_record`] and [`tty_exit_record`] are pure (`gate` canonicalizes
//! the header's root); [`serve`] is the I/O.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::PathBuf;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::lent::{self, Files, Peer, Reach};
use crate::commands::exec::{SHELL_FALLBACK_CMD, exec_argv_with, exec_context};
use crate::devsbd::control::valid_run_id;
use crate::devsbd::rpc::{self, Exit, Json, Obj, Record};
use crate::serve::endpoint::Stream;
use crate::serve::helper::{Call, CallError, Cancel, HelperClient, Input};
use crate::state::{Instance, State};

/// The header's method, and the helper's.
pub const METHOD: &str = "session.attach";

/// The `error` codes a `PTY` stream ends with besides the helper's own
/// (`rpc::code`: `no-session`, `detached`, `version`, `bad-request`,
/// `failed`), which are relayed as it sent them. The worker's refusals use
/// `bad-request` and `failed` too.
pub mod code {
    pub use crate::devsbd::rpc::code::{BAD_REQUEST, FAILED};
    /// The header's root is outside the fleet root, or not this hub's to
    /// reach.
    pub const DENIED: &str = "denied";
    /// No such root, or no lent instance by that name or id in it.
    pub const NOT_FOUND: &str = "not-found";
    /// The instance isn't running (no live bridge to its helper).
    pub const NOT_RUNNING: &str = "not-running";
    /// The helper stopped offering `session.attach` between the check
    /// ([`super::decide`]) and the call.
    pub const UNSUPPORTED: &str = "unsupported";
    /// The attach ended without an end record (the bridge to the instance
    /// was lost): neither an exit nor a refusal; the session may live on.
    pub const LOST: &str = "lost";
}

/// How long the hub has to send the header once it opened the stream.
const HEADER_WAIT: Duration = Duration::from_secs(30);

/// How often a piped exec is checked for its exit and the hub's hang-up.
const PIPE_TICK: Duration = Duration::from_millis(100);

/// The most bytes one `stdout` / `stderr` record of a piped exec carries.
const PIPE_CHUNK: usize = 64 * 1024;

/// What the hub asks: `{"root", "instance", "session", "create"?:
/// {"argv"?}}`, `{"root", "instance", "pipe": {"argv",
/// "interactive"?}}`, or `{"root", "instance", "service", "create":
/// {"argv"?}}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    /// The instance's config root (absolute, under the fleet root).
    pub root: PathBuf,
    /// An exact instance name or instance id.
    pub instance: String,
    pub mode: Mode,
}

/// A session attach, a piped exec, or a service terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Session `id` (`control::valid_run_id`). `create`: `None`, attach
    /// only; `Some(argv)`, create it running `argv` (empty: the login shell,
    /// `SHELL_FALLBACK_CMD`) unless it exists, then attach.
    Session { id: String, create: Option<Vec<String>> },
    /// A non-tty exec of `argv` (never empty: the hub sends the login shell
    /// itself), the hub's stdin forwarded when `interactive` (`exec -i`).
    Pipe { argv: Vec<String>, interactive: bool },
    /// A tty exec of `argv` (empty: the login shell) in the container of
    /// service `name` that the header's instance uses; always a new
    /// terminal (no sessions).
    Service { name: String, argv: Vec<String> },
}

fn bad(message: impl Into<String>) -> Record {
    Record::error(code::BAD_REQUEST, message)
}

/// A record's name, for a refusal.
fn kind(rec: &Record) -> &'static str {
    match rec {
        Record::Call(_) => "call",
        Record::Stdin(_) => "stdin",
        Record::StdinEof => "stdin-eof",
        Record::Resize { .. } => "resize",
        Record::Signal(_) => "signal",
        Record::Stdout(_) => "stdout",
        Record::Stderr(_) => "stderr",
        Record::Event(_) => "event",
        Record::Result(_) => "result",
        Record::Error { .. } => "error",
        Record::Exit(_) => "exit",
    }
}

/// The hub's first record as a [`Header`], or the `bad-request` to end the
/// stream with.
pub fn parse_header(rec: Record) -> Result<Header, Record> {
    let call = match rec {
        Record::Call(call) => call,
        other => return Err(bad(format!("a PTY stream starts with a `{METHOD}` call, not a `{}` record", kind(&other)))),
    };
    if call.method != METHOD {
        return Err(bad(format!("a PTY stream's call is `{METHOD}`, not `{}`", call.method)));
    }
    let p = &call.params;
    let text = |key: &str| p.str(key).filter(|s| !s.is_empty()).ok_or_else(|| bad(format!("the PTY header needs `{key}`")));
    let root = PathBuf::from(text("root")?);
    let instance = text("instance")?.to_string();
    let present = |key: &str| !matches!(p.get(key), None | Some(Json::Null));
    let create_argv = |c: &Obj| match c.get("argv") {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Strs(argv)) => Ok(argv.clone()),
        Some(_) => Err(bad("`create.argv` must be an array of strings")),
    };
    if present("service") {
        if present("session") || present("pipe") {
            return Err(bad("the PTY header takes one of `session`, `pipe` and `service`"));
        }
        let name = text("service")?.to_string();
        let argv = match p.get("create") {
            Some(Json::Obj(c)) => create_argv(c)?,
            None | Some(Json::Null) => return Err(bad("a service terminal needs `create` (there are no service sessions)")),
            Some(_) => return Err(bad("`create` must be an object")),
        };
        return Ok(Header { root, instance, mode: Mode::Service { name, argv } });
    }
    if present("pipe") {
        if present("session") || present("create") {
            return Err(bad("the PTY header takes `pipe` or `session`, not both"));
        }
        let Some(Json::Obj(pipe)) = p.get("pipe") else { return Err(bad("`pipe` must be an object")) };
        let argv = match pipe.get("argv") {
            Some(Json::Strs(argv)) if !argv.is_empty() => argv.clone(),
            Some(Json::Strs(_)) | None | Some(Json::Null) => return Err(bad("`pipe.argv` needs a command")),
            Some(_) => return Err(bad("`pipe.argv` must be an array of strings")),
        };
        let interactive = match pipe.get("interactive") {
            None | Some(Json::Null) => false,
            Some(Json::Bool(b)) => *b,
            Some(_) => return Err(bad("`pipe.interactive` must be a boolean")),
        };
        return Ok(Header { root, instance, mode: Mode::Pipe { argv, interactive } });
    }
    let id = text("session")?.to_string();
    if !valid_run_id(&id) {
        return Err(bad(format!("bad session id `{id}`")));
    }
    let create = match p.get("create") {
        None | Some(Json::Null) => None,
        Some(Json::Obj(c)) => Some(create_argv(c)?),
        Some(_) => return Err(bad("`create` must be an object")),
    };
    Ok(Header { root, instance, mode: Mode::Session { id, create } })
}

/// The instance the header names, and its `state.toml` key: `root`
/// confined to `reach` as `lent::confine_path` does (the fleet root or an
/// own root under `worker`, any under `full`); the instance by exact name,
/// else by id, among those lent (`lent::lent_instances`) whose config root
/// is under `root`. Neither the helper nor running is checked here: a
/// session's helper is asked ([`decide`]); a runtime exec (a pipe, a tty)
/// says so itself.
pub fn gate<'a>(h: &Header, state: &'a State, reach: &Reach) -> Result<(&'a str, &'a Instance), Record> {
    let root = lent::confine_path(reach, &h.root, true).map_err(|e| match e.code {
        "invalid" => bad(e.message),
        code => Record::error(code, e.message),
    })?;
    let lent = lent::lent_instances(state, reach);
    let by_name = state.instances.get_key_value(&h.instance);
    let found = by_name
        .or_else(|| state.instances.iter().find(|(_, i)| i.instance_id == h.instance))
        .filter(|(name, i)| lent.names.contains(*name) && i.config_dir.as_deref().is_some_and(|d| lent::root_under(d, &root)));
    let Some((name, instance)) = found else {
        return Err(Record::error(code::NOT_FOUND, format!("no instance `{}` in {}", h.instance, root.display())));
    };
    Ok((name.as_str(), instance))
}

/// How a session header's terminal runs ([`decide`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    /// A session attach through the helper (resumable).
    Attach,
    /// A tty exec under a host pty ([`run_tty`]): not resumable, and why.
    Tty(NotResumable),
}

/// Why an instance's terminals over the link aren't sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotResumable {
    /// No devsbd helper in it.
    NoHelper,
    /// Its helper's daemon doesn't offer `session.attach` (decision 13: it
    /// is root only; a `containerUser` instance, or an older helper).
    HelperNotRoot,
}

impl NotResumable {
    pub fn why(self) -> &'static str {
        match self {
            NotResumable::NoHelper => "no helper",
            NotResumable::HelperNotRoot => "helper not root",
        }
    }

    /// The `stderr` record's text a fallback terminal starts with (`\r\n`:
    /// the hub's terminal is raw by then).
    pub fn note(self) -> String {
        format!("this terminal isn't resumable ({}): a dropped link ends it\r\n", self.why())
    }
}

/// The terminal a session header for instance `name` gets: `create`
/// whether it creates one; `offers` the helper's answer to whether it
/// offers `session.attach` (`HelperClient::offers`), `None` without a
/// helper. A helper that takes it: [`Terminal::Attach`]. One that can't
/// (none, not offered): a creating header falls back to
/// [`Terminal::Tty`], an attach-only one gets `no-session` (nothing
/// resumable exists). No bridge is `not-running`.
pub fn decide(create: bool, offers: Option<Result<bool, CallError>>, name: &str) -> Result<Terminal, Record> {
    let why = match offers {
        Some(Ok(true)) => return Ok(Terminal::Attach),
        None => NotResumable::NoHelper,
        Some(Ok(false) | Err(CallError::Unsupported(_))) => NotResumable::HelperNotRoot,
        Some(Err(e)) => return Err(refusal(e, name)),
    };
    if create {
        return Ok(Terminal::Tty(why));
    }
    Err(Record::error(
        rpc::code::NO_SESSION,
        format!("`{name}`'s terminals over the fleet link aren't resumable ({}): no session to attach to", why.why()),
    ))
}

/// The refusal for a helper call that couldn't be made.
fn refusal(e: CallError, name: &str) -> Record {
    match e {
        CallError::NoBridge(_) => {
            Record::error(code::NOT_RUNNING, format!("`{name}` isn't running (or its helper's bridge isn't up yet)"))
        }
        CallError::Unsupported(_) => Record::error(code::UNSUPPORTED, format!("`{name}`'s helper no longer offers `{METHOD}`")),
        e => Record::error(code::FAILED, format!("cannot ask `{name}`'s helper: {e}")),
    }
}

/// The container of service `service` that lent instance `name` uses: the
/// config read from the instance's own config root (`gate` put it under
/// the header's), named as the forwards name it
/// (`port::service_container_for`), never by the hub.
pub fn service_container(instance: &Instance, name: &str, state: &State, service: &str) -> Result<String, Record> {
    let failed = |e: anyhow::Error| Record::error(code::FAILED, format!("{e:#}"));
    let dir = instance
        .config_dir
        .as_deref()
        .ok_or_else(|| Record::error(code::NOT_FOUND, format!("`{name}` has no config root recorded")))?;
    let config = crate::config::Config::load(dir).map_err(failed)?;
    let project = crate::commands::services::project_id(dir).map_err(failed)?;
    crate::commands::port::service_container_for(&config, state, &project, name, service)
        .map_err(|e| Record::error(code::NOT_FOUND, e))
}

/// `session.attach`'s params (devsbd's `rpc_sessions`) for session `id`:
/// `{id, create?: {argv, cwd, env, user?}}`, `create` as an exec into
/// `instance` runs (`exec::exec_context`: workspace, remote user, env with
/// `SSH_AUTH_SOCK`; `host_agent` as there).
pub fn attach_params(id: &str, create: Option<&[String]>, instance: &Instance, host_agent: bool) -> Obj {
    let params = Obj::new().with("id", id);
    let Some(argv) = create else { return params };
    let argv = if argv.is_empty() { SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect() } else { argv.to_vec() };
    let ctx = exec_context(instance, host_agent);
    // `with` replaces a repeated name (the decoder refuses repeats), the
    // later one winning as with repeated `-e`s.
    let env = ctx.env.iter().fold(Obj::new(), |env, (k, v)| env.with(k, v.as_str()));
    let mut create = Obj::new().with("argv", argv).with("cwd", ctx.cwd).with("env", env);
    if let Some(user) = ctx.user {
        create = create.with("user", user);
    }
    params.with("create", create)
}

/// What a worker's exec runs in.
pub enum Target<'a> {
    /// Instance `instance`, as a local `exec` (`-i` when `interactive`, `-t`
    /// when `tty`; `host_agent` as `exec_argv_with` takes it).
    Instance { instance: &'a Instance, interactive: bool, tty: bool, argv: &'a [String], host_agent: bool },
    /// A service container, as the dashboard's local service tab: `-i -t`.
    Service { container: &'a str, argv: &'a [String] },
}

/// The argv (program first) of an exec into a [`Target`]: a piped exec's
/// and a tty one's. [`runtime_exec`] in the daemon; tests run a program of
/// their own.
pub type ExecCommand = fn(&Target) -> Vec<String>;

/// The runtime's `exec` into `t`: for an instance, the argv a local `exec`
/// builds (`exec::exec_argv_with`); for a service, `exec -i -t <container>
/// <argv…>`.
pub fn runtime_exec(t: &Target) -> Vec<String> {
    let mut argv = vec![crate::runtime::backend().bin().to_string()];
    match t {
        Target::Instance { instance, interactive, tty, argv: command, host_agent } => {
            argv.extend(exec_argv_with(instance, *interactive, *tty, command, *host_agent));
        }
        Target::Service { container, argv: command } => {
            argv.extend(["exec", "-i", "-t", container].map(String::from));
            argv.extend(command.iter().cloned());
        }
    }
    argv
}

/// `argv`, or the login shell (`SHELL_FALLBACK_CMD`) when it is empty.
fn or_shell(argv: &[String]) -> Vec<String> {
    if argv.is_empty() { SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect() } else { argv.to_vec() }
}

/// The `exit` record of a piped exec that ended with `status`: its code, or
/// the signal that killed it.
pub fn exit_record(status: ExitStatus) -> Record {
    use std::os::unix::process::ExitStatusExt;
    Record::Exit(match (status.code(), status.signal()) {
        (Some(code), _) => Exit::Code(code),
        (None, Some(signal)) => Exit::Signal(signal),
        (None, None) => Exit::Code(1),
    })
}

/// The `exit` record of a tty exec that ended with `status`: its code, or
/// the signal that killed it (portable-pty keeps only the signal's
/// `strsignal` name, [`signal_number`] maps it back).
pub fn tty_exit_record(status: &portable_pty::ExitStatus) -> Record {
    Record::Exit(match status.signal() {
        Some(name) => signal_number(name).map(Exit::Signal).unwrap_or(Exit::Code(i32::try_from(status.exit_code()).unwrap_or(1))),
        None => Exit::Code(i32::try_from(status.exit_code()).unwrap_or(i32::MAX)),
    })
}

/// The signal `strsignal` names `name` (portable-pty's `Signal <n>` for one
/// it doesn't).
fn signal_number(name: &str) -> Option<i32> {
    if let Some(n) = name.strip_prefix("Signal ") {
        return n.parse().ok();
    }
    (1..65).find(|&n| {
        // SAFETY: `strsignal` returns a NUL-terminated string (or null),
        // read at once, before another call could reuse its buffer.
        let s = unsafe { libc::strsignal(n) };
        !s.is_null() && unsafe { std::ffi::CStr::from_ptr(s) }.to_string_lossy() == name
    })
}

/// A program on a host pty: the master (for `resize`), the child, the
/// pty's read half and write half.
pub struct Tty {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

/// Spawn `argv` on a fresh host pty (80×24 until the hub's first `resize`),
/// as the dashboard's terminal tabs do (`tui::term`).
pub fn spawn_tty(argv: Vec<String>) -> anyhow::Result<Tty> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })?;
    let mut cmd = CommandBuilder::from_argv(argv.into_iter().map(Into::into).collect());
    // Only the runtime client sees it, as in a dashboard tab.
    cmd.env("TERM", "xterm-256color");
    let child = pair.slave.spawn_command(cmd)?;
    // Only the child holds the slave: its exit ends the reader.
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    Ok(Tty { master: pair.master, child, reader, writer })
}

/// What a stream's header started.
enum Started {
    Attach(Call, Input),
    Pipe(Child),
    /// A tty exec, and the note to send before its output.
    Tty(Tty, Option<String>),
}

/// Serve one `PTY` stream, `link` the worker's end, to its end (see the
/// module doc). `files` are this host's (its `state.toml`, and what the
/// peer's [`Reach`] is read from), `peer` the link's;
/// `host_agent` probes for a host ssh-agent (`exec::has_host_agent`);
/// `exec` builds a piped or tty exec's argv ([`runtime_exec`]). Refusals
/// with `denied` are logged.
pub fn serve(
    link: Stream,
    helper: &HelperClient,
    files: &Files,
    peer: &Peer,
    host_agent: fn() -> bool,
    exec: ExecCommand,
    log: fn(&str),
) {
    let caller = format!("fleet hub {}", peer.name);
    let _ = link.set_read_timeout(Some(HEADER_WAIT));
    let first = rpc::read_record(&mut &link);
    let _ = link.set_read_timeout(None);
    let started = match first {
        Ok(Some(rec)) => start(rec, &link, helper, files, peer, host_agent, exec, &caller, log),
        Ok(None) => return,
        Err(e) => Err(bad(format!("no PTY header: {e}"))),
    };
    match started {
        Ok(Started::Attach(call, input)) => pump(&link, call, input),
        Ok(Started::Pipe(child)) => run_pipe(&link, child),
        Ok(Started::Tty(tty, note)) => run_tty(&link, tty, note.as_deref()),
        Err(rec) => {
            if let Record::Error { code: c, message } = &rec
                && c == code::DENIED
            {
                log(&format!("fleet: refused a PTY stream from hub {}: {message}", peer.name));
            }
            let _ = rpc::write_record(&mut &link, &rec);
        }
    }
    let _ = link.shutdown(Shutdown::Both);
}

/// Header, gate, then the call (the attach and its input), the piped
/// exec's spawn or the tty exec's, or the `error` to end the stream with.
#[allow(clippy::too_many_arguments)]
fn start(
    first: Record,
    link: &Stream,
    helper: &HelperClient,
    files: &Files,
    peer: &Peer,
    host_agent: fn() -> bool,
    exec: ExecCommand,
    caller: &str,
    log: fn(&str),
) -> Result<Started, Record> {
    let h = parse_header(first)?;
    let state = State::load_at(&files.state).map_err(|e| Record::error(code::FAILED, format!("{e:#}")))?;
    let (reach, warning) = Reach::load(files, peer.fleet_root.as_deref());
    if let Some(w) = warning {
        log(&format!("fleet: a PTY stream from hub {}: {w}", peer.name));
    }
    let (name, instance) = gate(&h, &state, &reach)?;
    let tty = |argv: Vec<String>, note: Option<String>| {
        spawn_tty(argv)
            .map(|t| Started::Tty(t, note))
            .map_err(|e| Record::error(code::FAILED, format!("cannot run a terminal in `{name}`: {e:#}")))
    };
    let (id, create) = match &h.mode {
        Mode::Pipe { argv, interactive } => {
            let target = Target::Instance { instance, interactive: *interactive, tty: false, argv, host_agent: host_agent() };
            let argv = exec(&target);
            let (program, args) = argv.split_first().ok_or_else(|| Record::error(code::FAILED, "an empty exec"))?;
            let mut command = std::process::Command::new(program);
            command
                .args(args)
                .stdin(if *interactive { Stdio::piped() } else { Stdio::null() })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            return command
                .spawn()
                .map(Started::Pipe)
                .map_err(|e| Record::error(code::FAILED, format!("cannot run an exec in `{name}`: {e}")));
        }
        Mode::Service { name: service, argv } => {
            let container = service_container(instance, name, &state, service)?;
            return tty(exec(&Target::Service { container: &container, argv: &or_shell(argv) }), None);
        }
        Mode::Session { id, create } => (id, create),
    };
    let offers = instance.devsbd_arch.is_some().then(|| helper.offers(&instance.container, METHOD, caller));
    if let Terminal::Tty(why) = decide(create.is_some(), offers, name)? {
        let argv = or_shell(create.as_deref().unwrap_or_default());
        let target = Target::Instance { instance, interactive: true, tty: true, argv: &argv, host_agent: host_agent() };
        return tty(exec(&target), Some(why.note()));
    }
    let params = attach_params(id, create.as_deref(), instance, host_agent());
    // While it queues: whether the hub closed its end.
    let abandoned = || crate::serve::daemon::hung_up(link, Duration::ZERO);
    let (call, input) = helper.call_with_input(&instance.container, METHOD, params, Obj::new(), caller, &abandoned).map_err(|e| match e {
        CallError::NoBridge(_) | CallError::Unsupported(_) => refusal(e, name),
        CallError::Abandoned => Record::error(code::FAILED, "the hub hung up while the attach queued"),
        CallError::Io(e) => Record::error(code::LOST, format!("cannot reach `{name}`'s helper: {e}")),
    })?;
    Ok(Started::Attach(call, input))
}

/// A piped exec to its end: the hub's input into its stdin and its stdout
/// and stderr out as records, each on a thread of its own; once it exited
/// and both pipes are drained, its `exit` record. The hub hanging up first
/// (`feed_pipe` reads the stream's end, or a peek between waits does)
/// kills and reaps it, and nothing more is written.
fn run_pipe(link: &Stream, mut child: Child) {
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    // One writer at a time: a record is one write, never interleaved.
    let writer = Mutex::new(());
    let send = |rec: &Record| {
        let _turn = writer.lock().unwrap_or_else(|e| e.into_inner());
        rpc::write_record(&mut &*link, rec).is_ok()
    };
    let done = AtomicBool::new(false);
    let gone = AtomicBool::new(false);
    std::thread::scope(|s| {
        let send = &send;
        let out = stdout.map(|r| s.spawn(move || drain(r, Record::Stdout, send)));
        let err = stderr.map(|r| s.spawn(move || drain(r, Record::Stderr, send)));
        if let Ok(reader) = link.try_clone() {
            let (done, gone) = (&done, &gone);
            s.spawn(move || feed_pipe(reader, stdin, done, gone));
        }
        let ended = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(e),
            }
            if gone.load(Ordering::SeqCst) || crate::serve::daemon::hung_up(link, PIPE_TICK) {
                gone.store(true, Ordering::SeqCst);
                let _ = child.kill();
                let _ = child.wait();
                break Err(std::io::Error::other("the hub hung up"));
            }
        };
        for drained in [out, err].into_iter().flatten() {
            let _ = drained.join();
        }
        if !gone.load(Ordering::SeqCst) {
            let last = match ended {
                Ok(status) => exit_record(status),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    Record::error(code::FAILED, format!("cannot wait for the exec: {e}"))
                }
            };
            send(&last);
        }
        done.store(true, Ordering::SeqCst);
        // Ends `feed_pipe`'s read.
        let _ = link.shutdown(Shutdown::Both);
    });
}

/// A tty exec to its end: `note` first (as a `stderr` record), then the
/// pty's output as `stdout` records on a thread of its own, the hub's
/// `stdin` into the pty and its `resize`s onto it on another; once the
/// child exited and the output is drained, its `exit` record
/// ([`tty_exit_record`]). The hub hanging up first kills (SIGHUP, then
/// SIGKILL: portable-pty's `kill`) and reaps it, and nothing more is
/// written.
fn run_tty(link: &Stream, tty: Tty, note: Option<&str>) {
    let Tty { master, mut child, reader, writer } = tty;
    let master = Mutex::new(master);
    // One writer at a time: a record is one write, never interleaved.
    let turn = Mutex::new(());
    let send = |rec: &Record| {
        let _turn = turn.lock().unwrap_or_else(|e| e.into_inner());
        rpc::write_record(&mut &*link, rec).is_ok()
    };
    if let Some(note) = note {
        send(&Record::Stderr(note.as_bytes().to_vec()));
    }
    let done = AtomicBool::new(false);
    let gone = AtomicBool::new(false);
    std::thread::scope(|s| {
        let send = &send;
        let out = s.spawn(move || drain(reader, Record::Stdout, send));
        if let Ok(r) = link.try_clone() {
            let (master, done, gone) = (&master, &done, &gone);
            s.spawn(move || feed_tty(r, writer, master, done, gone));
        }
        let ended = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(e),
            }
            if gone.load(Ordering::SeqCst) || crate::serve::daemon::hung_up(link, PIPE_TICK) {
                gone.store(true, Ordering::SeqCst);
                let _ = child.kill();
                let _ = child.wait();
                break Err(std::io::Error::other("the hub hung up"));
            }
        };
        // The child gone, its pty's last bytes, then the read's end.
        let _ = out.join();
        if !gone.load(Ordering::SeqCst) {
            let last = match ended {
                Ok(status) => tty_exit_record(&status),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    Record::error(code::FAILED, format!("cannot wait for the terminal: {e}"))
                }
            };
            send(&last);
        }
        done.store(true, Ordering::SeqCst);
        // Ends `feed_tty`'s read.
        let _ = link.shutdown(Shutdown::Both);
    });
}

/// The hub's records into a tty exec until the hub closes its end, which
/// flags `gone` unless the exec is over (`done`): `stdin` to the pty,
/// `resize` onto it. `stdin-eof` is ignored (a terminal has no end of its
/// own to give), as is anything else.
fn feed_tty(
    mut reader: Stream,
    mut writer: Box<dyn Write + Send>,
    master: &Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    done: &AtomicBool,
    gone: &AtomicBool,
) {
    loop {
        match rpc::read_record(&mut reader) {
            Ok(Some(Record::Stdin(bytes))) => {
                let _ = writer.write_all(&bytes).and_then(|()| writer.flush());
            }
            Ok(Some(Record::Resize { cols, rows })) => {
                let size = portable_pty::PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
                let _ = master.lock().unwrap_or_else(|e| e.into_inner()).resize(size);
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                if !done.load(Ordering::SeqCst) {
                    gone.store(true, Ordering::SeqCst);
                }
                return;
            }
        }
    }
}

/// One of a piped exec's outputs to its end, each read as one `wrap`
/// record, until the hub refuses a write (a tty's too).
fn drain(mut r: impl Read, wrap: fn(Vec<u8>) -> Record, send: &(dyn Fn(&Record) -> bool + Sync)) {
    let mut buf = vec![0u8; PIPE_CHUNK];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if !send(&wrap(buf[..n].to_vec())) {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// The hub's records into a piped exec's `stdin` (`None`: not interactive,
/// its input ignored) until the hub closes its end, which flags `gone`
/// unless the exec is over (`done`). `stdin-eof`, or a write the exec
/// refuses, closes its stdin.
fn feed_pipe(mut reader: Stream, mut stdin: Option<std::process::ChildStdin>, done: &AtomicBool, gone: &AtomicBool) {
    loop {
        match rpc::read_record(&mut reader) {
            Ok(Some(Record::Stdin(bytes))) => {
                if let Some(w) = &mut stdin
                    && w.write_all(&bytes).is_err()
                {
                    stdin = None;
                }
            }
            Ok(Some(Record::StdinEof)) => stdin = None,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                if !done.load(Ordering::SeqCst) {
                    gone.store(true, Ordering::SeqCst);
                }
                return;
            }
        }
    }
}

/// The attach's last record.
fn is_end(rec: &Record) -> bool {
    matches!(rec, Record::Exit(_) | Record::Error { .. } | Record::Result(_))
}

/// Relay until the attach ends: the hub's records into `input` on a thread
/// of their own, the call's records back to the hub, unchanged, up to and
/// including its end record. The hub closing its end cancels the call; a
/// call that ends without an end record is answered with [`code::LOST`].
fn pump(link: &Stream, mut call: Call, input: Input) {
    let done = AtomicBool::new(false);
    let gone = AtomicBool::new(false);
    let cancel = call.canceller();
    std::thread::scope(|s| {
        if let Ok(reader) = link.try_clone() {
            let (done, gone, cancel) = (&done, &gone, &cancel);
            s.spawn(move || feed(reader, input, done, gone, cancel));
        }
        let last = loop {
            match call.next() {
                Ok(Some(rec)) => {
                    let end = is_end(&rec);
                    // A failed write: the hub is gone; dropping the call
                    // (unfinished) cancels it.
                    if rpc::write_record(&mut &*link, &rec).is_err() || end {
                        break None;
                    }
                }
                Ok(None) => break Some(Record::error(code::LOST, "the attach ended without an end record")),
                Err(_) if gone.load(Ordering::SeqCst) => break None,
                Err(e) => break Some(Record::error(code::LOST, format!("the bridge to the instance was lost: {e}"))),
            }
        };
        if let Some(rec) = last {
            let _ = rpc::write_record(&mut &*link, &rec);
        }
        done.store(true, Ordering::SeqCst);
        // Ends `feed`'s read.
        let _ = link.shutdown(Shutdown::Both);
    });
}

/// The hub's records into the attach until the hub closes its end (or
/// sends something undecodable), which cancels the call unless it is over.
/// Records other than input are ignored, as the helper ignores them.
fn feed(mut reader: Stream, mut input: Input, done: &AtomicBool, gone: &AtomicBool, cancel: &Cancel) {
    loop {
        let sent = match rpc::read_record(&mut reader) {
            Ok(Some(Record::Stdin(bytes))) => input.stdin(&bytes),
            Ok(Some(Record::Resize { cols, rows })) => input.resize(cols, rows),
            Ok(Some(Record::StdinEof)) => input.stdin_eof(),
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(_) => {
                if !done.load(Ordering::SeqCst) {
                    gone.store(true, Ordering::SeqCst);
                    cancel.cancel();
                }
                return;
            }
        };
        // The call's stream is gone: it's ending, `pump` answers.
        if sent.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    use super::*;
    use crate::devsbd::proto::caps;
    use crate::devsbd::rpc::{Call as RpcCall, Exit};
    use crate::serve::helper::tests::{Fake, log, logged};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-pty-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    const SESSION: &str = "1700000000-ab12";

    fn header(params: Obj) -> Record {
        Record::Call(RpcCall { method: METHOD.into(), params, meta: Obj::new() })
    }

    fn base(root: &str) -> Obj {
        Obj::new().with("root", root).with("instance", "web-1").with("session", SESSION)
    }

    #[test]
    fn the_header_parses_and_is_checked() {
        let h = parse_header(header(base("/f/cfg"))).unwrap();
        assert_eq!(h, Header { root: "/f/cfg".into(), instance: "web-1".into(), mode: session(None) });
        let shell = parse_header(header(base("/f").with("create", Obj::new()))).unwrap();
        assert_eq!(shell.mode, session(Some(Vec::new())), "no argv: the login shell");
        let argv = parse_header(header(base("/f").with("create", Obj::new().with("argv", vec!["htop".to_string()])))).unwrap();
        assert_eq!(argv.mode, session(Some(vec!["htop".to_string()])));
        assert_eq!(parse_header(header(base("/f").with("create", Json::Null))).unwrap().mode, session(None));

        let refused = |rec: Record| match parse_header(rec) {
            Err(Record::Error { code, message }) => (code, message),
            other => panic!("{other:?}"),
        };
        let (c, m) = refused(Record::Stdin(b"x".to_vec()));
        assert_eq!((c.as_str(), m.contains("not a `stdin` record")), (code::BAD_REQUEST, true));
        let wrong = Record::Call(RpcCall { method: "run.start".into(), params: base("/f"), meta: Obj::new() });
        assert!(refused(wrong).1.contains("not `run.start`"));
        assert!(refused(header(base("/f").with("session", "nope"))).1.contains("bad session id"));
        assert!(refused(header(base("/f").with("instance", ""))).1.contains("needs `instance`"));
        assert!(refused(header(Obj::new().with("instance", "a").with("session", SESSION))).1.contains("needs `root`"));
        assert!(refused(header(base("/f").with("create", "sh"))).1.contains("`create` must be"));
        assert!(refused(header(base("/f").with("create", Obj::new().with("argv", "sh")))).1.contains("`create.argv`"));
    }

    fn session(create: Option<Vec<String>>) -> Mode {
        Mode::Session { id: SESSION.into(), create }
    }

    fn pipe_base(root: &str) -> Obj {
        Obj::new().with("root", root).with("instance", "web-1")
    }

    #[test]
    fn a_pipe_header_parses_and_excludes_a_session() {
        let ls = || Obj::new().with("argv", vec!["ls".to_string(), "-l".into()]);
        let h = parse_header(header(pipe_base("/f").with("pipe", ls()))).unwrap();
        assert_eq!(h.mode, Mode::Pipe { argv: vec!["ls".into(), "-l".into()], interactive: false }, "no session needed");
        let i = parse_header(header(pipe_base("/f").with("pipe", ls().with("interactive", true)))).unwrap();
        assert_eq!(i.mode, Mode::Pipe { argv: vec!["ls".into(), "-l".into()], interactive: true });

        let refused = |params: Obj| match parse_header(header(params)) {
            Err(Record::Error { code, message }) => {
                assert_eq!(code, code::BAD_REQUEST);
                message
            }
            other => panic!("{other:?}"),
        };
        assert!(refused(base("/f").with("pipe", ls())).contains("not both"), "with a session");
        assert!(refused(pipe_base("/f").with("pipe", ls()).with("create", Obj::new())).contains("not both"));
        assert!(refused(pipe_base("/f").with("pipe", Obj::new())).contains("needs a command"));
        assert!(refused(pipe_base("/f").with("pipe", Obj::new().with("argv", Vec::<String>::new()))).contains("needs a command"));
        assert!(refused(pipe_base("/f").with("pipe", Obj::new().with("argv", "ls"))).contains("array of strings"));
        assert!(refused(pipe_base("/f").with("pipe", ls().with("interactive", "yes"))).contains("boolean"));
        assert!(refused(pipe_base("/f").with("pipe", "ls")).contains("must be an object"));
        assert!(refused(pipe_base("/f")).contains("needs `session`"), "neither");
    }

    #[test]
    fn a_service_header_parses_creates_and_excludes_the_others() {
        let svc = || pipe_base("/f").with("service", "db");
        let h = parse_header(header(svc().with("create", Obj::new()))).unwrap();
        assert_eq!(h, Header { root: "/f".into(), instance: "web-1".into(), mode: Mode::Service { name: "db".into(), argv: vec![] } });
        let psql = parse_header(header(svc().with("create", Obj::new().with("argv", vec!["psql".to_string()])))).unwrap();
        assert_eq!(psql.mode, Mode::Service { name: "db".into(), argv: vec!["psql".into()] });

        let refused = |params: Obj| match parse_header(header(params)) {
            Err(Record::Error { code, message }) => {
                assert_eq!(code, code::BAD_REQUEST);
                message
            }
            other => panic!("{other:?}"),
        };
        assert!(refused(svc()).contains("needs `create`"), "no attach-only service terminal");
        assert!(refused(svc().with("create", "sh")).contains("`create` must be"));
        assert!(refused(svc().with("create", Obj::new()).with("session", SESSION)).contains("one of"));
        let ls = Obj::new().with("argv", vec!["ls".to_string()]);
        assert!(refused(svc().with("create", Obj::new()).with("pipe", ls)).contains("one of"));
        assert!(refused(pipe_base("/f").with("service", "").with("create", Obj::new())).contains("needs `service`"));
        assert!(refused(svc().with("create", Obj::new().with("argv", "sh"))).contains("`create.argv`"));
    }

    #[test]
    fn exits_encode_a_code_or_a_signal() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit_record(ExitStatus::from_raw(3 << 8)), Record::Exit(Exit::Code(3)));
        assert_eq!(exit_record(ExitStatus::from_raw(0)), Record::Exit(Exit::Code(0)));
        assert_eq!(exit_record(ExitStatus::from_raw(9)), Record::Exit(Exit::Signal(9)));
        let rec = exit_record(ExitStatus::from_raw(15));
        assert_eq!(rpc::read_record(&mut &rec.encode()[..]).unwrap(), Some(rec), "as rpc carries it");
    }

    /// `state.toml` text: `web-1` lent (under `cfg`, with a helper), `bare`
    /// lent without one, `own` outside the fleet root.
    fn state_with(base: &Path, root: &Path) -> State {
        let inst = |name: &str, dir: &Path, helper: bool| {
            format!(
                "[instance.{name}]\ninstance_id = \"id-{name}\"\nsandbox = \"web\"\ncontainer = \"devsandbox-{name}\"\n\
                 folder = \"/w\"\nworkspace = \"/workspaces/{name}\"\ncreated_unix = 1\nconfig_dir = {:?}\n{}",
                dir.to_str().unwrap(),
                if helper { "devsbd_arch = \"x86_64\"\n" } else { "" }
            )
        };
        let text = inst("web-1", &root.join("cfg"), true) + &inst("bare", &root.join("cfg"), false) + &inst("own", &base.join("mine"), true);
        let path = base.join("state.toml");
        std::fs::write(&path, text).unwrap();
        State::load_at(&path).unwrap()
    }

    fn code_of(r: Result<impl std::fmt::Debug, Record>) -> String {
        match r {
            Err(Record::Error { code, .. }) => code,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_gate_takes_lent_instances_with_a_helper() {
        let b = scratch("gate");
        let root = b.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::create_dir_all(b.join("mine")).unwrap();
        let state = state_with(&b, &root);
        let cfg = root.join("cfg");
        let h = |root: &Path, instance: &str| Header { root: root.to_path_buf(), instance: instance.into(), mode: session(None) };
        let reach = |own: &[PathBuf]| Reach { trust: crate::worker_policy::Trust::Worker, fleet_root: Some(root.clone()), own: own.iter().cloned().collect() };
        // An own root reaches its instances, exactly.
        let mine = b.join("mine");
        std::fs::create_dir_all(mine.join("nested")).unwrap();
        let with_own = reach(&[mine.clone()]);
        assert_eq!(gate(&h(&mine, "own"), &state, &with_own).unwrap().0, "own", "an own root's");
        assert_eq!(gate(&h(&mine, "id-own"), &state, &with_own).unwrap().0, "own", "by id");
        assert_eq!(code_of(gate(&h(&mine.join("nested"), "own"), &state, &with_own)), code::DENIED, "not what's nested in it");
        assert_eq!(code_of(gate(&h(&cfg, "own"), &state, &with_own)), code::NOT_FOUND, "not another root's");
        let full = Reach { trust: crate::worker_policy::Trust::Full, ..reach(&[]) };
        assert_eq!(gate(&h(&mine, "own"), &state, &full).unwrap().0, "own", "`full`: any root");
        let r = reach(&[]);
        let (name, i) = gate(&h(&cfg, "web-1"), &state, &r).unwrap();
        assert_eq!((name, i.container.as_str()), ("web-1", "devsandbox-web-1"));
        assert_eq!(gate(&h(&cfg, "id-web-1"), &state, &r).unwrap().0, "web-1", "by id");
        assert_eq!(gate(&h(&root, "web-1"), &state, &r).unwrap().0, "web-1", "the fleet root holds it too");
        assert_eq!(code_of(gate(&h(&cfg, "web"), &state, &r)), code::NOT_FOUND, "exact names only");
        assert_eq!(code_of(gate(&h(&cfg, "own"), &state, &r)), code::NOT_FOUND, "not lent");
        assert_eq!(code_of(gate(&h(&root.join("other"), "web-1"), &state, &r)), code::NOT_FOUND, "another root's");
        assert_eq!(code_of(gate(&h(&b.join("mine"), "own"), &state, &r)), code::DENIED, "outside the fleet root");
        assert_eq!(code_of(gate(&h(&root.join("gone"), "web-1"), &state, &r)), code::NOT_FOUND);
        assert_eq!(code_of(gate(&h(Path::new("rel"), "web-1"), &state, &r)), code::BAD_REQUEST);
        assert_eq!(gate(&h(&cfg, "bare"), &state, &r).unwrap().0, "bare", "no helper: `decide`'s to say");
        let pipe = |instance: &str| Header { mode: Mode::Pipe { argv: vec!["ls".into()], interactive: false }, ..h(&cfg, instance) };
        assert_eq!(gate(&pipe("bare"), &state, &r).unwrap().0, "bare", "a pipe needs no helper");
        assert_eq!(code_of(gate(&pipe("own"), &state, &r)), code::NOT_FOUND, "but the same lending");
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn a_helper_that_cant_attach_falls_back_to_a_tty_when_creating() {
        use NotResumable::*;
        assert_eq!(decide(true, Some(Ok(true)), "w"), Ok(Terminal::Attach));
        assert_eq!(decide(false, Some(Ok(true)), "w"), Ok(Terminal::Attach), "an attach is the helper's");
        assert_eq!(decide(true, None, "w"), Ok(Terminal::Tty(NoHelper)));
        assert_eq!(decide(true, Some(Ok(false)), "w"), Ok(Terminal::Tty(HelperNotRoot)));
        assert_eq!(decide(true, Some(Err(CallError::Unsupported("c".into()))), "w"), Ok(Terminal::Tty(HelperNotRoot)), "an older helper");
        // Attach only: nothing resumable exists.
        for offers in [None, Some(Ok(false))] {
            let Err(Record::Error { code, message }) = decide(false, offers, "w") else { panic!() };
            assert_eq!(code, rpc::code::NO_SESSION);
            assert!(message.contains("aren't resumable"), "{message}");
        }
        assert_eq!(code_of(decide(true, Some(Err(CallError::NoBridge("c".into()))), "w")), code::NOT_RUNNING);
        assert_eq!(code_of(decide(true, Some(Err(CallError::Io(std::io::Error::other("x")))), "w")), code::FAILED);
        assert_eq!(NoHelper.note(), "this terminal isn't resumable (no helper): a dropped link ends it\r\n");
        assert!(HelperNotRoot.note().contains("(helper not root)"));
    }

    #[test]
    fn tty_exits_keep_the_signal_number() {
        use std::os::unix::process::ExitStatusExt;
        let of = |raw| tty_exit_record(&portable_pty::ExitStatus::from(ExitStatus::from_raw(raw)));
        assert_eq!(of(3 << 8), Record::Exit(Exit::Code(3)));
        assert_eq!(of(9), Record::Exit(Exit::Signal(9)));
        assert_eq!(of(1), Record::Exit(Exit::Signal(1)));
        assert_eq!(tty_exit_record(&portable_pty::ExitStatus::with_signal("Signal 50")), Record::Exit(Exit::Signal(50)));
    }

    #[test]
    fn the_exec_argv_is_a_local_execs() {
        let b = scratch("argv");
        let root = b.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        let state = state_with(&b, &root);
        let bin = crate::runtime::backend().bin().to_string();
        let argv = vec!["ls".to_string()];
        let inst = &state.instances["web-1"];
        let tty = runtime_exec(&Target::Instance { instance: inst, interactive: true, tty: true, argv: &argv, host_agent: false });
        assert_eq!(tty[0], bin);
        assert_eq!(tty[1..], exec_argv_with(inst, true, true, &argv, false));
        let svc = runtime_exec(&Target::Service { container: "devsandbox-svc-p-db", argv: &argv });
        assert_eq!(svc, [bin.as_str(), "exec", "-i", "-t", "devsandbox-svc-p-db", "ls"]);
        assert_eq!(or_shell(&[]), SHELL_FALLBACK_CMD);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn create_runs_as_an_exec_would() {
        let b = scratch("params");
        let root = b.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        let state = state_with(&b, &root);
        let mut inst = state.instances["web-1"].clone();
        assert_eq!(attach_params(SESSION, None, &inst, false), Obj::new().with("id", SESSION), "attach only");

        inst.remote_user = Some("vscode".into());
        inst.remote_env = BTreeMap::from([("A".into(), "1".into()), ("SSH_AUTH_SOCK".into(), "/mine".into())]);
        inst.ssh_auth_sock = Some("/ssh-agent".into());
        let p = attach_params(SESSION, Some(&[]), &inst, false);
        let c = p.obj("create").unwrap();
        let shell: Vec<String> = SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect();
        assert_eq!(c.strs("argv"), Some(shell.as_slice()), "empty argv: the login shell");
        assert_eq!(c.str("cwd"), Some("/workspaces/web-1"));
        assert_eq!(c.str("user"), Some("vscode"));
        assert_eq!(c.map("env").unwrap(), [("A".to_string(), "1".to_string()), ("SSH_AUTH_SOCK".into(), "/ssh-agent".into())], "one each, the agent's last");
        // What the helper decodes: env sits at the RPC JSON's depth cap.
        let rec = Record::Call(RpcCall { method: METHOD.into(), params: p, meta: Obj::new() });
        assert_eq!(rpc::read_record(&mut &rec.encode()[..]).unwrap(), Some(rec));

        inst.remote_user = None;
        let c = attach_params(SESSION, Some(&["htop".to_string()]), &inst, false).obj("create").unwrap().clone();
        assert_eq!((c.strs("argv"), c.str("user")), (Some(&["htop".to_string()][..]), None), "no user: the container's");
        let _ = std::fs::remove_dir_all(&b);
    }

    /// A container's helper for [`serve`]: `rpc.methods` offering
    /// `session.attach`; the attach sends its params' `id` and `create`'s
    /// `cwd` as `stdout`, then `stdin` back as `stdout`, a `resize` as
    /// `stdout` `<cols>x<rows>`, `exit 7` at `stdin-eof`. Session
    /// `1700000000-0105` (lost) sends one `stdout` and closes without an end.
    fn sessions(s: UnixStream) {
        let mut r = &s;
        let Ok(Some(Record::Call(call))) = rpc::read_record(&mut r) else { return };
        let mut w = &s;
        let mut send = |rec: Record| rpc::write_record(&mut w, &rec);
        match call.method.as_str() {
            "rpc.methods" => {
                let _ = send(Record::Result(Obj::new().with("methods", vec!["session.attach@1".to_string()])));
            }
            "session.attach" => {
                let id = call.params.str("id").unwrap_or_default().to_string();
                let cwd = call.params.obj("create").and_then(|c| c.str("cwd")).unwrap_or("-").to_string();
                let _ = send(Record::Stdout(format!("{id} {cwd}\n").into_bytes()));
                if id == "1700000000-0105" {
                    let _ = s.shutdown(Shutdown::Both);
                    return;
                }
                loop {
                    let rec = match rpc::read_record(&mut r) {
                        Ok(Some(Record::Stdin(b))) => Record::Stdout(b),
                        Ok(Some(Record::Resize { cols, rows })) => Record::Stdout(format!("{cols}x{rows}").into_bytes()),
                        Ok(Some(Record::StdinEof)) => Record::Exit(Exit::Code(7)),
                        Ok(Some(_)) => continue,
                        // The caller hung up: detached.
                        _ => return,
                    };
                    let last = matches!(rec, Record::Exit(_));
                    let _ = send(rec);
                    if last {
                        break;
                    }
                }
            }
            m => {
                let _ = send(Record::error(rpc::code::UNKNOWN_METHOD, format!("no method `{m}`")));
            }
        }
        let _ = s.shutdown(Shutdown::Write);
        let _ = std::io::copy(&mut r, &mut std::io::sink());
    }

    /// A non-root helper: offers nothing of the sessions.
    fn not_root(s: UnixStream) {
        let mut r = &s;
        let Ok(Some(Record::Call(_))) = rpc::read_record(&mut r) else { return };
        let _ = rpc::write_record(&mut &s, &Record::Result(Obj::new().with("methods", vec!["rpc.methods@1".to_string()])));
    }

    struct World {
        base: PathBuf,
        root: PathBuf,
        state: PathBuf,
        helper: HelperClient,
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// The config at `cfg`: sandbox `web` (every instance's) uses `db`.
    const CONFIG: &str = "[services.db]\nimage = \"postgres\"\nscope = \"isolated\"\n\n\
                          [services.cache]\nimage = \"redis\"\nscope = \"global\"\n\n\
                          [sandbox.web]\nimage = \"node\"\nservices = [\"db\"]\n";

    fn world(tag: &str) -> World {
        let base = scratch(tag);
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::write(root.join("cfg").join(crate::config::CONFIG_FILE), CONFIG).unwrap();
        state_with(&base, &root);
        let fake = Fake::default();
        fake.add("devsandbox-web-1", caps::RPC, 1, sessions);
        let helper = HelperClient::new(fake.clone(), log);
        // `bare` has no helper in `state.toml`; `own` isn't lent.
        World { state: base.join("state.toml"), base, root, helper }
    }

    /// The hub's end of a stream served on a thread, its header sent.
    fn open(w: &World, header: Record, helper: HelperClient) -> (UnixStream, std::thread::JoinHandle<()>) {
        let (hub, ours) = UnixStream::pair().unwrap();
        let peer = Peer { name: "me@hub".into(), fleet_root: Some(w.root.clone()), env: None };
        let files = Files { state: w.state.clone(), member: None, home: None };
        let t = std::thread::spawn(move || serve(ours, &helper, &files, &peer, || false, sh, log));
        rpc::write_record(&mut &hub, &header).unwrap();
        hub.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        (hub, t)
    }

    fn rest(mut hub: &UnixStream) -> Vec<Record> {
        let mut recs = Vec::new();
        while let Ok(Some(rec)) = rpc::read_record(&mut hub) {
            recs.push(rec);
        }
        recs
    }

    fn create_header(w: &World, session: &str) -> Record {
        let cfg = w.root.join("cfg");
        header(base(cfg.to_str().unwrap()).with("session", session).with("create", Obj::new()))
    }

    #[test]
    fn a_stream_relays_input_and_output_to_the_end_record() {
        let w = world("relay");
        let (hub, t) = open(&w, create_header(&w, SESSION), w.helper.clone());
        let mut out = &hub;
        rpc::write_record(&mut out, &Record::Stdin(b"ls\n".to_vec())).unwrap();
        rpc::write_record(&mut out, &Record::Resize { cols: 120, rows: 40 }).unwrap();
        rpc::write_record(&mut out, &Record::Signal(2)).unwrap();
        rpc::write_record(&mut out, &Record::StdinEof).unwrap();
        let recs = rest(&hub);
        t.join().unwrap();
        let stdout = |s: &str| Record::Stdout(s.as_bytes().to_vec());
        assert_eq!(
            recs,
            [stdout(&format!("{SESSION} /workspaces/web-1\n")), stdout("ls\n"), stdout("120x40"), Record::Exit(Exit::Code(7))],
            "the helper's records, unchanged, then nothing"
        );
    }

    #[test]
    fn a_lost_bridge_ends_with_lost_and_a_hang_up_cancels() {
        let w = world("lost");
        let (hub, t) = open(&w, create_header(&w, "1700000000-0105"), w.helper.clone());
        let recs = rest(&hub);
        t.join().unwrap();
        assert_eq!(recs.len(), 2, "{recs:?}");
        assert!(matches!(&recs[1], Record::Error { code, .. } if code == code::LOST), "{recs:?}");

        // The hub closes mid-attach: the call is cancelled (the session
        // detaches and runs on), nothing more is written.
        let (hub, t) = open(&w, create_header(&w, "1700000000-0106"), w.helper.clone());
        assert!(matches!(rpc::read_record(&mut &hub), Ok(Some(Record::Stdout(_)))));
        drop(hub);
        t.join().unwrap();
        assert!(logged("helper devsandbox-web-1: session.attach from fleet hub me@hub: cancelled ("));
    }

    /// The tests' exec: `sh -c <argv[0]>` on this host; into a service,
    /// its container's name echoed first.
    fn sh(t: &Target) -> Vec<String> {
        let script = match t {
            Target::Instance { argv, .. } => argv[0].clone(),
            Target::Service { container, argv } => format!("echo {container}; {}", argv[0]),
        };
        vec!["sh".into(), "-c".into(), script]
    }

    fn pipe_header(w: &World, instance: &str, script: &str, interactive: bool) -> Record {
        let cfg = w.root.join("cfg");
        let pipe = Obj::new().with("argv", vec![script.to_string()]).with("interactive", interactive);
        header(Obj::new().with("root", cfg.to_str().unwrap()).with("instance", instance).with("pipe", pipe))
    }

    /// The `stdout` and `stderr` bytes of `recs`, and their last record.
    fn split(recs: &[Record]) -> (String, String, Option<Record>) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        for rec in recs {
            match rec {
                Record::Stdout(b) => out.extend_from_slice(b),
                Record::Stderr(b) => err.extend_from_slice(b),
                _ => {}
            }
        }
        (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap(), recs.last().cloned())
    }

    #[test]
    fn a_pipe_splits_its_outputs_and_ends_with_its_exit() {
        let w = world("pipe-out");
        // `bare` has no helper: a pipe doesn't need one.
        let (hub, t) = open(&w, pipe_header(&w, "bare", "echo out; echo err >&2; cat; exit 5", false), w.helper.clone());
        // It may be over already: the write's outcome doesn't matter.
        let _ = rpc::write_record(&mut &hub, &Record::Stdin(b"ignored\n".to_vec()));
        let recs = rest(&hub);
        t.join().unwrap();
        assert_eq!(split(&recs), ("out\n".into(), "err\n".into(), Some(Record::Exit(Exit::Code(5)))), "not interactive: empty stdin");
        assert_eq!(recs.iter().filter(|r| matches!(r, Record::Exit(_))).count(), 1);

        let (hub, t) = open(&w, pipe_header(&w, "web-1", "kill -9 $$", false), w.helper.clone());
        let recs = rest(&hub);
        t.join().unwrap();
        assert_eq!(recs, [Record::Exit(Exit::Signal(9))]);
    }

    #[test]
    fn a_pipe_forwards_stdin_to_its_eof() {
        let w = world("pipe-in");
        let (hub, t) = open(&w, pipe_header(&w, "web-1", "cat; echo done", true), w.helper.clone());
        rpc::write_record(&mut &hub, &Record::Stdin(b"abc".to_vec())).unwrap();
        rpc::write_record(&mut &hub, &Record::Resize { cols: 80, rows: 24 }).unwrap();
        rpc::write_record(&mut &hub, &Record::Stdin(b"def\n".to_vec())).unwrap();
        rpc::write_record(&mut &hub, &Record::StdinEof).unwrap();
        let recs = rest(&hub);
        t.join().unwrap();
        assert_eq!(split(&recs), ("abcdef\ndone\n".into(), String::new(), Some(Record::Exit(Exit::Code(0)))));
    }

    #[test]
    fn a_hang_up_kills_the_pipe() {
        let w = world("pipe-hup");
        let (hub, t) = open(&w, pipe_header(&w, "web-1", "echo $$; exec sleep 30", true), w.helper.clone());
        let Ok(Some(Record::Stdout(pid))) = rpc::read_record(&mut &hub) else { panic!("no pid") };
        let pid = String::from_utf8(pid).unwrap();
        let pid: libc::pid_t = pid.trim().parse().unwrap_or_else(|_| panic!("{pid:?}"));
        let started = std::time::Instant::now();
        drop(hub);
        t.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "killed, not waited out");
        // SAFETY: signal 0 only checks the pid; reaped, it's gone.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "killed and reaped");
    }

    #[test]
    fn refusals_are_one_error_record() {
        let w = world("refuse");
        let refusal = |header: Record, helper: HelperClient| {
            let (hub, t) = open(&w, header, helper);
            let recs = rest(&hub);
            t.join().unwrap();
            match recs.as_slice() {
                [Record::Error { code, .. }] => code.clone(),
                other => panic!("{other:?}"),
            }
        };
        let cfg = w.root.join("cfg");
        let cfg = cfg.to_str().unwrap();
        assert_eq!(refusal(Record::StdinEof, w.helper.clone()), code::BAD_REQUEST);
        assert_eq!(refusal(header(base(cfg).with("instance", "own")), w.helper.clone()), code::NOT_FOUND);
        let other = w.base.join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(refusal(header(base(other.to_str().unwrap()).with("instance", "own")), w.helper.clone()), code::DENIED);
        assert!(logged("fleet: refused a PTY stream from hub me@hub"));
        // An own root's instance is reached (its helper isn't running here).
        let mine = w.base.join("mine");
        std::fs::create_dir_all(&mine).unwrap();
        assert_eq!(refusal(header(base(mine.to_str().unwrap()).with("instance", "own")), w.helper.clone()), code::NOT_RUNNING);
        // Attach only, without a helper that attaches: nothing to resume.
        assert_eq!(refusal(header(base(cfg).with("instance", "bare")), w.helper.clone()), rpc::code::NO_SESSION);

        // Not running: no bridge; not root: `session.attach` not offered.
        let none = HelperClient::new(Fake::default(), log);
        assert_eq!(refusal(header(base(cfg)), none), code::NOT_RUNNING);
        let fake = Fake::default();
        fake.add("devsandbox-web-1", caps::RPC, 1, not_root);
        assert_eq!(refusal(header(base(cfg)), HelperClient::new(fake, log)), rpc::code::NO_SESSION);

        let svc = |service: &str| header(pipe_base(cfg).with("service", service).with("create", Obj::new()));
        assert_eq!(refusal(svc("cache"), w.helper.clone()), code::NOT_FOUND, "not one `web-1` uses");
        assert_eq!(refusal(svc("nope"), w.helper.clone()), code::NOT_FOUND);
        let own = header(base(cfg).with("instance", "own").with("session", Json::Null).with("service", "db").with("create", Obj::new()));
        assert_eq!(refusal(own, w.helper.clone()), code::NOT_FOUND, "the instance gates the service");
    }

    fn tty_header(w: &World, instance: &str, script: &str) -> Record {
        let cfg = w.root.join("cfg");
        let create = Obj::new().with("argv", vec![script.to_string()]);
        header(base(cfg.to_str().unwrap()).with("instance", instance).with("create", create))
    }

    #[test]
    fn a_helperless_instance_gets_a_tty_that_says_it_isnt_resumable() {
        let w = world("tty");
        let (hub, t) = open(&w, tty_header(&w, "bare", "read x; stty size; echo got $x; exit 3"), w.helper.clone());
        rpc::write_record(&mut &hub, &Record::Resize { cols: 100, rows: 30 }).unwrap();
        rpc::write_record(&mut &hub, &Record::Stdin(b"hi\n".to_vec())).unwrap();
        let recs = rest(&hub);
        t.join().unwrap();
        assert_eq!(recs[0], Record::Stderr(NotResumable::NoHelper.note().into_bytes()), "the note first: {recs:?}");
        let (out, err, last) = split(&recs);
        assert_eq!(err, NotResumable::NoHelper.note(), "only the note");
        assert!(out.contains("30 100"), "the resize reached the pty: {out:?}");
        assert!(out.contains("got hi"), "{out:?}");
        assert_eq!(last, Some(Record::Exit(Exit::Code(3))));

        let (hub, t) = open(&w, tty_header(&w, "bare", "kill -9 $$"), w.helper.clone());
        let recs = rest(&hub);
        t.join().unwrap();
        assert_eq!(recs.last(), Some(&Record::Exit(Exit::Signal(9))));
    }

    #[test]
    fn a_non_root_helper_falls_back_too() {
        let w = world("tty-root");
        let fake = Fake::default();
        fake.add("devsandbox-web-1", caps::RPC, 1, not_root);
        let (hub, t) = open(&w, tty_header(&w, "web-1", "echo ok"), HelperClient::new(fake, log));
        let recs = rest(&hub);
        t.join().unwrap();
        assert_eq!(recs[0], Record::Stderr(NotResumable::HelperNotRoot.note().into_bytes()));
        let (out, _, last) = split(&recs);
        assert!(out.contains("ok"), "{out:?}");
        assert_eq!(last, Some(Record::Exit(Exit::Code(0))));
    }

    #[test]
    fn a_hang_up_kills_the_tty() {
        let w = world("tty-hup");
        let (hub, t) = open(&w, tty_header(&w, "bare", "echo $$; exec sleep 30"), w.helper.clone());
        let mut out = String::new();
        let pid: libc::pid_t = loop {
            match rpc::read_record(&mut &hub) {
                Ok(Some(Record::Stdout(b))) => out.push_str(&String::from_utf8_lossy(&b)),
                Ok(Some(Record::Stderr(_))) => {}
                other => panic!("{other:?} after {out:?}"),
            }
            if let Some(line) = out.lines().next().filter(|_| out.contains('\n')) {
                break line.trim().parse().unwrap_or_else(|_| panic!("{out:?}"));
            }
        };
        let started = std::time::Instant::now();
        drop(hub);
        t.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "killed, not waited out");
        // SAFETY: signal 0 only checks the pid; reaped, it's gone.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "killed and reaped");
    }

    #[test]
    fn a_service_terminal_runs_in_the_container_the_worker_names() {
        let w = world("svc");
        let cfg = w.root.join("cfg");
        let create = Obj::new().with("argv", vec!["exit 4".to_string()]);
        let h = header(pipe_base(cfg.to_str().unwrap()).with("service", "db").with("create", create));
        let (hub, t) = open(&w, h, w.helper.clone());
        let recs = rest(&hub);
        t.join().unwrap();
        let (out, err, last) = split(&recs);
        let project = crate::commands::services::project_id(&cfg).unwrap();
        let container = crate::commands::services::isolated_service_container(&project, "id-web-1", "db");
        assert!(out.contains(&container), "{out:?}");
        assert_eq!(err, "", "no note: a service terminal was never a session");
        assert_eq!(last, Some(Record::Exit(Exit::Code(4))));
    }
}
