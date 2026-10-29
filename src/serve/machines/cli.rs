//! The CLI side of machines (docs/architecture/daemon/machines.md, *The CLI:
//! `cli.rs`*): where a command's `<name>` points, and how the commands that
//! don't go through the daemon as verbs (`logs`, `exec`, `inspect`, `done`,
//! `rename`, …) run when it points at a machine, as do the nameless ones
//! given `--machine`: over the machine's link (`fleet-<name>.*.sock`), so
//! within its trust (fleet plan decision 15), never a `devsandbox …` run
//! there.
//!
//! The verbs (`run`, `rebuild`, …, `gc`) resolve in `verbs_client` instead,
//! from the same pieces ([`resolve_params`], [`found_of`], [`choose`]);
//! [`route`] only checks their names here.
//!
//! The decisions are pure ([`plan`], [`machine_use`], [`root_banners`],
//! [`info_params`], the exec modes); [`route`] does the I/O: the daemon
//! lookups, then API calls through the daemon ([`calls`]) or `PTY` dials
//! from this process (`fleet::pty_client`). The lookup (`machines.get`)
//! passes this process's `SSH_AUTH_SOCK`, so the agent the daemon answers
//! the machine's `AGENT` streams with is this terminal's while the command
//! runs there.
//!
//! A tty `exec` on a machine runs in a helper session this process names
//! ([`reconnect_session`]), in a loop: a lost link (the dial's 255) re-dials
//! an attach to it ([`Reconnect`], [`dials`]). Its `--attach` is one dial,
//! `--sessions` a `sessions.list` quick call. An exec without a tty is one
//! piped dial ([`pipe_mode`]; a session one, `--attach` or `--session`,
//! runs the helper's session client piped, as a local `exec` does), an
//! `exec --service` one service dial ([`service_mode`]); neither is
//! re-dialed.

pub(crate) mod calls;

pub(crate) use calls::config_roots;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::api::MachineInfo;
use super::{Entry, Where, check_name, no_machine, parse_name};
use crate::serve::api::{Found, Resolved};
use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::serve::fleet::pty_client;
use crate::{Command, ServiceCommand};

/// What `--machine` does with a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MachineUse {
    /// API calls through the daemon (`ps`, `stats`, and per registered
    /// root `ls`, `status`, `service ls`): [`calls::use_of`] says which.
    Calls,
    /// A daemon verb (`stop/start/rebuild --all`, `service rebuild`, `gc`):
    /// `verbs_client` forwards it.
    Verb,
    /// `port --service <global-svc>` with no instance name: forwarded here
    /// from the machine's first root ([`Routed::Port`]).
    Port,
    /// A command taking a `<name>`: qualify the name instead.
    Named,
    /// Not for machines (yet): what to call the command.
    Refused(&'static str),
}

/// `--machine`'s use for `command` (`None`: the dashboard).
pub(crate) fn machine_use(command: Option<&Command>) -> MachineUse {
    let Some(command) = command else { return MachineUse::Refused("the dashboard") };
    match command {
        Command::Ps { .. }
        | Command::Stats { .. }
        | Command::Ls { .. }
        | Command::Status { .. }
        | Command::Service { cmd: ServiceCommand::Ls { .. } } => MachineUse::Calls,
        Command::Gc { .. } | Command::Service { cmd: ServiceCommand::Rebuild { .. } } => MachineUse::Verb,
        Command::Rebuild { all: true, .. } | Command::Stop { all: true, .. } | Command::Start { all: true, .. } => {
            MachineUse::Verb
        }
        Command::Run { .. }
        | Command::Rebuild { .. }
        | Command::Stop { .. }
        | Command::Start { .. }
        | Command::Rm { .. }
        | Command::Rename { .. }
        | Command::Logs { .. }
        | Command::Inspect { .. }
        | Command::Done { .. }
        | Command::Undone { .. }
        | Command::Exec { .. } => MachineUse::Named,
        Command::Inbox { .. } => MachineUse::Refused("inbox"),
        Command::Vscode { .. } => MachineUse::Refused("vscode"),
        Command::Port { .. } if port_nameless(command) => MachineUse::Port,
        Command::Port { .. } => MachineUse::Named,
        Command::Serve { .. } => MachineUse::Refused("serve"),
        Command::Api { .. } => MachineUse::Refused("api"),
        Command::Tunnel { .. } => MachineUse::Refused("tunnel"),
        Command::AgentRelay { .. } => MachineUse::Refused("agent-relay"),
        Command::FleetLink { .. } => MachineUse::Refused("fleet-link"),
        Command::FleetHello => MachineUse::Refused("fleet-hello"),
        Command::Machine { .. } => MachineUse::Refused("machine"),
        // This host's own lending: `lend.toml` and its link are local.
        Command::Lend { .. } => MachineUse::Refused("lend"),
        Command::Fleet { .. } => MachineUse::Refused("fleet"),
    }
}

/// The `<name>` `command` takes, if it has one (a sandbox for `run`).
fn name_mut(command: &mut Command) -> Option<&mut String> {
    match command {
        Command::Run { sandbox: Some(n), .. }
        | Command::Rebuild { name: Some(n), .. }
        | Command::Stop { name: Some(n), .. }
        | Command::Start { name: Some(n), .. }
        | Command::Port { name: Some(n), .. }
        | Command::Rename { name: n, .. }
        | Command::Rm { name: n, .. }
        | Command::Logs { name: n, .. }
        | Command::Inspect { name: n, .. }
        | Command::Vscode { name: n, .. }
        | Command::Done { name: n }
        | Command::Undone { name: n }
        | Command::Exec { name: n, .. } => Some(n),
        _ => None,
    }
}

fn name_of(command: &Command) -> Option<&str> {
    match command {
        Command::Run { sandbox: Some(n), .. }
        | Command::Rebuild { name: Some(n), .. }
        | Command::Stop { name: Some(n), .. }
        | Command::Start { name: Some(n), .. }
        | Command::Port { name: Some(n), .. }
        | Command::Rename { name: n, .. }
        | Command::Rm { name: n, .. }
        | Command::Logs { name: n, .. }
        | Command::Inspect { name: n, .. }
        | Command::Vscode { name: n, .. }
        | Command::Done { name: n }
        | Command::Undone { name: n }
        | Command::Exec { name: n, .. } => Some(n),
        _ => None,
    }
}

/// `port` naming no instance: no name, or (`--service`) a leading positional
/// that is a port, which clap put in `name` (port.rs's `split_target`).
fn port_nameless(command: &Command) -> bool {
    match command {
        Command::Port { name: None, .. } => true,
        Command::Port { name: Some(n), service: Some(_), .. } => crate::commands::port::parse_port_spec(n).is_ok(),
        _ => false,
    }
}

/// The verbs `verbs_client` sends, which resolve their names there.
fn is_verb(command: &Command) -> bool {
    matches!(command, Command::Run { .. } | Command::Rebuild { .. } | Command::Stop { .. } | Command::Start { .. } | Command::Rm { .. })
}

/// What [`route`] does with a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Run it as given: no machine involved, or `verbs_client` decides.
    Here,
    /// Run it here with this name (an `@/` dropped).
    HereAs(String),
    /// `--machine m` with API calls ([`MachineUse::Calls`]) or a `port`.
    Machine(String, MachineUse),
    /// Look the name up through the daemon, then run it where it is.
    Resolve(Where, String),
}

/// The routing decision, pure. `machines`: the registered machine names
/// (from the file). Without any, and always under `--in-process` (the
/// daemon's verb subprocesses and the dispatcher, whose names are resolved
/// already, on a host that may have machines of its own), nothing fans out.
pub(crate) fn plan(command: &Command, machine: Option<&str>, in_process: bool, machines: &[String]) -> Result<Plan> {
    if in_process {
        if machine.is_some() {
            bail!("--machine isn't for --in-process");
        }
        return Ok(Plan::Here);
    }
    if let Some(m) = machine {
        check_name(m)?;
        if matches!(command, Command::Run { here: true, .. }) {
            bail!("--here runs on this host; it doesn't go with --machine {m}");
        }
        let named = name_of(command).is_some();
        let usage = machine_use(Some(command));
        match &usage {
            MachineUse::Named => bail!("--machine is for commands without a name; qualify the name instead: {m}/<name>"),
            MachineUse::Refused(what) if named => {
                bail!("--machine is for commands without a name; `{what}` doesn't run on machines yet")
            }
            MachineUse::Refused(what) => bail!("--machine isn't for `devsandbox {what}`"),
            _ => {}
        }
        if !machines.iter().any(|n| n == m) {
            return Err(no_machine(m));
        }
        return Ok(match usage {
            MachineUse::Verb => Plan::Here,
            usage => Plan::Machine(m.to_string(), usage),
        });
    }
    let Some(arg) = name_of(command) else { return Ok(Plan::Here) };
    if port_nameless(command) {
        return Ok(Plan::Here);
    }
    let (wherever, name) = parse_name(arg)?;
    if let Where::Machine(m) = &wherever
        && !machines.contains(m)
    {
        return Err(no_machine(m));
    }
    // With machines, a verb keeps its qualifier: `verbs_client` resolves it
    // (an `@/` dropped here would make it look everywhere).
    if is_verb(command) && !machines.is_empty() {
        return Ok(Plan::Here);
    }
    match wherever {
        Where::Local => return Ok(Plan::HereAs(name)),
        Where::Any if machines.is_empty() => return Ok(Plan::Here),
        _ => {}
    }
    if is_verb(command) {
        return Ok(Plan::Here);
    }
    // A container name stays local unless qualified.
    if matches!(command, Command::Logs { .. } | Command::Inspect { .. })
        && wherever == Where::Any
        && name.starts_with(crate::runtime::NAME_PREFIX)
    {
        return Ok(Plan::Here);
    }
    Ok(Plan::Resolve(wherever, name))
}

/// `command` with its name replaced (the resolved one, an `@/` dropped).
pub(crate) fn renamed(mut command: Command, name: String) -> Command {
    if let Some(n) = name_mut(&mut command) {
        *n = name;
    }
    command
}

/// For the in-process fallback of a verb: `@/x` is `x`; anything naming a
/// machine needs the daemon.
pub(crate) fn localize(command: Command) -> Result<Command> {
    let Some(arg) = name_of(&command) else { return Ok(command) };
    match parse_name(arg)? {
        (Where::Local, name) => Ok(renamed(command, name)),
        (Where::Machine(m), _) => bail!("`{arg}` is on machine `{m}`: that needs devsandbox serve"),
        (Where::Any, _) => Ok(command),
    }
}

// ---- resolving a name --------------------------------------------------------

/// `instances.resolve`'s params for a name: a bare name everywhere,
/// a qualified one on its machine, `@/` this host only.
pub(crate) fn resolve_params(wherever: &Where, name: &str) -> Value {
    match wherever {
        Where::Any => json!({ "name": name, "machines": true }),
        Where::Machine(m) => json!({ "name": name, "machine": m }),
        Where::Local => json!({ "name": name }),
    }
}

/// The matches of an `instances.resolve` answer, each with its machine. A
/// machine's own answer (a qualified lookup) says `machine: null`: it's
/// that machine's. An older daemon's has no `found`: its matches are local.
pub(crate) fn found_of(answer: Value, wherever: &Where) -> Result<Vec<Found>> {
    let r: Resolved = serde_json::from_value(answer).context("bad instances.resolve result from devsandbox serve")?;
    let found = if r.found.len() == r.matches.len() {
        r.found
    } else {
        r.matches.into_iter().map(|name| Found { name, machine: None, root: None, sandbox: None }).collect()
    };
    Ok(match wherever {
        Where::Machine(m) => found.into_iter().map(|f| Found { machine: Some(m.clone()), ..f }).collect(),
        _ => found,
    })
}

/// How matches are shown: qualified when any is a machine's (a local one as
/// `@/x`, so it reads as the choice it is), plain as today otherwise.
pub(crate) fn labels(found: &[Found]) -> Vec<String> {
    let remote = found.iter().any(|f| f.machine.is_some());
    found
        .iter()
        .map(|f| match &f.machine {
            Some(m) => format!("{m}/{}", f.name),
            None if remote => format!("@/{}", f.name),
            None => f.name.clone(),
        })
        .collect()
}

/// A numbered pick among options; the chosen index.
pub(crate) type Pick<'a> = dyn FnMut(&str, &[String]) -> Result<usize> + 'a;

/// One match among `found`: `None` when there's none; several pick on a TTY
/// and are an error listing them otherwise, as the in-process CLI does.
pub(crate) fn choose(name: &str, mut found: Vec<Found>, tty: bool, pick: &mut Pick) -> Result<Option<Found>> {
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => {
            let labels = labels(&found);
            if !tty {
                bail!("{}", crate::commands::ambiguous_instance(name, &labels));
            }
            let at = pick(&crate::commands::ambiguous_prompt(name), &labels)?;
            if at >= found.len() {
                bail!("selection out of range");
            }
            Ok(Some(found.swap_remove(at)))
        }
    }
}

/// A machine as `machines.get` describes it.
pub(crate) fn machine_info(call: &mut dyn FnMut(&str, Value) -> Result<crate::serve::proto::Response>, m: &str) -> Result<Entry> {
    machine_info_with(call, m, None)
}

/// [`machine_info`] reporting `own_agent` (this process's `SSH_AUTH_SOCK`)
/// to the daemon ([`info_params`]): the lookup before a re-run on the
/// machine.
fn machine_info_with(
    call: &mut dyn FnMut(&str, Value) -> Result<crate::serve::proto::Response>,
    m: &str,
    own_agent: Option<PathBuf>,
) -> Result<Entry> {
    let r = call("machines.get", info_params(m, own_agent))?;
    if let Some(e) = r.error {
        if e.code == "not-found" {
            return Err(no_machine(m));
        }
        bail!("{}", e.message);
    }
    let info: MachineInfo =
        serde_json::from_value(r.result.unwrap_or(Value::Null)).context("bad machines.get result from devsandbox serve")?;
    let kind = super::Kind::from_wire(info.kind, info.target).with_context(|| format!("machine `{m}`"))?;
    Ok(Entry { name: info.name, kind, roots: info.roots, command: info.command, agent: info.agent })
}

// ---- reconnecting a tty exec ------------------------------------------------

/// ssh's exit code for its own failure (a lost or refused connection).
pub(crate) const SSH_FAILED: i32 = 255;

/// Whether a failed `instances.resolve` (error `code`) of `command`'s name
/// exits [`SSH_FAILED`], as a lost dial does, instead of failing: for a
/// qualified name under `--reserve-255` (a dashboard tab's dials, which
/// re-dial on a 255: `tui::app::terminal::Redial`), any failure but
/// `not-found` is the machine's link being down, an outage to wait out.
pub(crate) fn resolve_lost(qualified: bool, command: &Command, code: &str) -> bool {
    qualified && matches!(command, Command::Exec { reserve_255: true, .. }) && code != "not-found"
}

/// The helper's `NO_SESSION_EXIT` (`devsbd/src/sessions.rs`): an attach to
/// a session that is unknown or `lost`. Like every code but 255 it ends
/// [`Reconnect`]'s loop as the session's answer, with the helper's own note
/// on stderr; this one also says the session is gone ([`gone_note`]).
pub(crate) const NO_SESSION_EXIT: i32 = 250;

/// The helper's `DETACHED_EXIT`: another client took the session over (it
/// runs on, attached elsewhere). What a `PTY` stream's `detached` end
/// exits with (`fleet::pty_client::exit_of`).
pub(crate) const DETACHED_EXIT: i32 = 251;

/// The helper's `VERSION_EXIT`: the session was started by another devsbd
/// (still there, not attachable by this one). A `PTY` stream's `version`
/// end.
pub(crate) const VERSION_EXIT: i32 = 252;

/// How long a dial must have run to count as connected: a 255 sooner, on a
/// first dial, is ssh failing to connect at all (bad host, auth), which no
/// retry fixes; on a re-dial, one more failed attempt of the same outage.
pub(crate) const CONNECTED_AFTER: Duration = Duration::from_secs(5);

/// The longest wait between two re-dials.
pub(crate) const BACKOFF_CAP: Duration = Duration::from_secs(10);

/// What follows one dial's exit ([`Reconnect::after`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Next {
    /// Re-dial with an attach after `wait`; `attempt` counts this outage's
    /// re-dials from 1.
    Retry { wait: Duration, attempt: u32 },
    /// The loop ends with `code`; `gone`: the session isn't there to attach
    /// to ([`NO_SESSION_EXIT`]).
    Done { code: i32, gone: bool },
}

/// The reconnect decision of a session exec on a machine, shared by the CLI
/// loop ([`route`], `PTY` dials) and the dashboard's remote tabs
/// (`tui::app::terminal::Redial`, ssh ones): pure over each dial's exit
/// code and run time. A 255 is the connection's alone (a `PTY` dial's lost
/// link, ssh's own failure; the program's 255 is reported as 254 under
/// `--reserve-255`), so it means the connection went: re-dial, with backoff,
/// for as long as it takes. Any other code is the session's answer and ends
/// the loop.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Reconnect {
    /// Some dial ran [`CONNECTED_AFTER`]: a later 255 is an outage.
    connected: bool,
    /// Re-dials since the last connected dial.
    attempt: u32,
}

impl Reconnect {
    /// After a dial exited with `code` having run `ran`.
    pub(crate) fn after(&mut self, code: i32, ran: Duration) -> Next {
        if code != SSH_FAILED {
            return Next::Done { code, gone: code == NO_SESSION_EXIT };
        }
        if ran >= CONNECTED_AFTER {
            self.connected = true;
            self.attempt = 0;
        }
        if !self.connected {
            // Never got through: ssh's own error (on stderr) is the answer.
            return Next::Done { code, gone: false };
        }
        self.attempt += 1;
        Next::Retry { wait: backoff(self.attempt), attempt: self.attempt }
    }
}

/// The wait before re-dial `attempt` (from 1): 1, 2, 4, 8 s, then
/// [`BACKOFF_CAP`].
pub(crate) fn backoff(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.saturating_sub(1).min(8);
    Duration::from_secs(secs).min(BACKOFF_CAP)
}

/// The CLI's line on stderr before re-dial `attempt`: the outage once, then
/// a counter.
pub(crate) fn retry_note(attempt: u32) -> String {
    match attempt {
        1 => "connection lost, reconnecting… (Ctrl-C to stop)".to_string(),
        n => format!("reconnecting… (attempt {n})"),
    }
}

/// The line after a loop that ended on [`Next::Done`] with `gone`.
pub(crate) fn gone_note(id: &str) -> String {
    format!("session `{id}` is gone (unknown or lost there, as after a container restart): nothing to reattach to")
}

/// The session id a machine `exec` reconnects with: `None` when it gets no
/// tty (a piped exec, nothing to re-attach to) or is an `--attach` /
/// `--sessions` already, or a `--service` terminal (never a session). The
/// tty rule is a local `exec`'s (`commands::exec::shell_flags`), `terminal`
/// standing for its stdin: a `PTY` dial has a terminal exactly when this
/// process is on one. A `--session` given is kept, else one is minted here.
pub(crate) fn reconnect_session(command: &Command, terminal: bool, mint: impl FnOnce() -> String) -> Result<Option<String>> {
    let Command::Exec { interactive, tty, attach: None, sessions: false, session, service: None, command, .. } = command else {
        return Ok(None);
    };
    let tty = if command.is_empty() { crate::commands::exec::shell_flags(*interactive, *tty, terminal).1 } else { *tty };
    if !tty {
        return Ok(None);
    }
    Ok(Some(match session {
        Some(id) => crate::commands::exec::checked_session_id(id)?.to_string(),
        None => mint(),
    }))
}

/// The piped `PTY` mode of a machine `exec` that gets no tty (the rule
/// [`reconnect_session`]'s): `argv` as typed, `-i` as given; no command,
/// the login shell (`exec::SHELL_FALLBACK_CMD`) with `exec::shell_flags`'
/// `-i`, as a local `exec` runs it. With `--session <id>`, that `argv` in
/// the helper's session `id` (`exec::session_new_command`), as a local
/// `exec --session` without a tty runs it. `None` for one that gets a tty,
/// an `--attach` / `--sessions` one ([`session_exec`]), a `--service` one
/// ([`service_mode`]) and any other command.
pub(crate) fn pipe_mode(command: &Command, terminal: bool) -> Result<Option<pty_client::Mode>> {
    let Command::Exec { interactive, tty, attach: None, sessions: false, session, service: None, command, .. } = command else {
        return Ok(None);
    };
    let (argv, interactive) = if command.is_empty() {
        let (interactive, tty) = crate::commands::exec::shell_flags(*interactive, *tty, terminal);
        if tty {
            return Ok(None);
        }
        (crate::commands::exec::SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect(), interactive)
    } else {
        if *tty {
            return Ok(None);
        }
        (command.clone(), *interactive)
    };
    let argv = match session {
        Some(id) => crate::commands::exec::session_new_command(crate::commands::exec::checked_session_id(id)?, &argv),
        None => argv,
    };
    Ok(Some(pty_client::Mode::Pipe { argv, interactive }))
}

/// The service `PTY` mode of an `exec --service <name>` on a machine: the
/// command as typed (empty: the machine's login shell). One dial, never
/// re-dialed (no session to go back to). `None` for any other command.
pub(crate) fn service_mode(command: &Command) -> Option<pty_client::Mode> {
    let Command::Exec { service: Some(name), command, .. } = command else { return None };
    Some(pty_client::Mode::Service { name: name.clone(), argv: command.clone() })
}

/// Instance `name`'s config root on machine `m`, which every `PTY` dial
/// and named call needs.
fn root_on<'a>(m: &str, root: Option<&'a Path>) -> Result<&'a Path> {
    root.with_context(|| format!("machine `{m}` has no config roots (devsandbox machine ls)"))
}

/// The dials of session exec `command` (an `Exec`) in session `id` on
/// machine `m`, instance `name` at `root` there: the first creating the
/// session running the command (none: the login shell) or attaching to it,
/// every re-dial attaching only (a reconnect never starts a second program).
pub(crate) fn dials(m: &str, command: &Command, name: &str, root: Option<&Path>, id: &str) -> Result<(pty_client::Request, pty_client::Request)> {
    let Command::Exec { command, .. } = command else { bail!("not an exec") };
    let root = root_on(m, root)?;
    let request = |create| pty_client::Request {
        root: root.to_path_buf(),
        instance: name.to_string(),
        mode: pty_client::Mode::Session { id: id.to_string(), create },
    };
    Ok((request(Some(command.clone())), request(None)))
}

/// Dial (`dial(false)`), then re-dial (`dial(true)`) after each lost
/// connection, per [`Reconnect`]. Between dials SIGINT is ours, so Ctrl-C
/// during a wait ends it.
fn reconnect_loop(id: &str, mut dial: impl FnMut(bool) -> Result<i32>) -> Result<Routed> {
    let mut state = Reconnect::default();
    let mut redial = false;
    loop {
        let started = Instant::now();
        let code = dial(redial)?;
        match state.after(code, started.elapsed()) {
            Next::Retry { wait, attempt } => {
                eprintln!("{}", retry_note(attempt));
                std::thread::sleep(wait);
                redial = true;
            }
            Next::Done { code, gone } => {
                if gone {
                    eprintln!("{}", gone_note(id));
                }
                return Ok(Routed::Exit(code));
            }
        }
    }
}

/// `--machine m` on a root-bound command: one listing per config root,
/// each after its `==> m:<root>` banner (these, in `roots`' order).
/// `--json` with several roots is an error: concatenated documents don't
/// parse.
pub(crate) fn root_banners(m: &str, roots: &[PathBuf], json: bool) -> Result<Vec<String>> {
    if roots.is_empty() {
        bail!("machine `{m}` has no config roots (devsandbox machine ls)");
    }
    if json && roots.len() > 1 {
        let list: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        bail!("--json with machine `{m}`'s {} roots would print one document each: {}", roots.len(), list.join(", "));
    }
    Ok(roots.iter().map(|root| banner(m, root)).collect())
}

/// The line on stderr before each of a machine's roots' runs.
pub(crate) fn banner(m: &str, root: &Path) -> String {
    format!("==> {m}:{}", root.display())
}

/// What [`route`] leaves to `main`.
pub(crate) enum Routed {
    /// Run this command here.
    Here(Command),
    /// It ran elsewhere: exit with this code.
    Exit(i32),
    /// `devsandbox port` into a machine's instance (`name`, as the machine
    /// knows it) or global service: forwarded from here over ssh
    /// (`commands::port::port_remote`), `root` the config root there. The
    /// ports are already split from the name.
    Port {
        entry: Entry,
        root: PathBuf,
        name: Option<String>,
        service: Option<String>,
        address: std::net::IpAddr,
        ports: Vec<String>,
    },
}

/// [`Routed::Port`] for `command` (a `Port`) on `entry` at `root`.
fn routed_port(command: Command, entry: Entry, root: PathBuf, name: Option<String>) -> Result<Routed> {
    let Command::Port { name: typed, service, address, ports } = command else { bail!("not a port command") };
    let ports = match name {
        // The instance resolved: the typed name was it, the ports are as given.
        Some(_) => ports,
        None => crate::commands::port::split_target(typed, service.is_some(), ports).1,
    };
    Ok(Routed::Port { entry, root, name, service, address, ports })
}

fn on_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn connect() -> Result<Conn> {
    endpoint::socket_dir().and_then(|dir| client::connect(&dir, "cli"))
}

/// `machines.get`'s params for machine `m`: with `own_agent` when absolute
/// (the daemon then serves the machine's `AGENT` streams with it).
pub(crate) fn info_params(m: &str, own_agent: Option<PathBuf>) -> Value {
    let mut params = json!({ "name": m });
    if let Some(agent) = own_agent.filter(|a| a.is_absolute()) {
        params["agent"] = json!(agent);
    }
    params
}

/// Where `command` runs (module doc): here (maybe with its name resolved),
/// or on a machine over its link: API calls through the daemon, `PTY` dials
/// from this process (a tty `exec` re-dialed after a lost link:
/// [`reconnect_loop`]). `machine` is `--machine`, `machines` the registered
/// names ([`super::registered_names`]).
pub(crate) fn route(machine: Option<&str>, in_process: bool, machines: &[String], command: Command) -> Result<Routed> {
    match plan(&command, machine, in_process, machines)? {
        Plan::Here => Ok(Routed::Here(command)),
        Plan::HereAs(name) => Ok(Routed::Here(renamed(command, name))),
        Plan::Machine(m, usage) => {
            let mut conn = connect().context("--machine needs devsandbox serve")?;
            let own_agent = crate::devsbd::bridge::own_agent();
            let entry = machine_info_with(&mut |method, params| conn.call(method, params), &m, own_agent)?;
            match usage {
                MachineUse::Calls => {
                    let u = calls::use_of(&command);
                    let (mut out, mut err) = (std::io::stdout().lock(), std::io::stderr());
                    let call = &mut |method: &str, params| conn.call(method, params);
                    let code = match u {
                        calls::Use::Ps { .. } | calls::Use::Stats { .. } => calls::listing(call, &u, &m, &mut out)?,
                        _ => calls::per_root(call, &u, &m, &entry.roots, &mut out, &mut err)?,
                    };
                    Ok(Routed::Exit(code))
                }
                MachineUse::Port => {
                    drop(conn);
                    let root = root_on(&m, entry.roots.first().map(PathBuf::as_path))?.to_path_buf();
                    routed_port(command, entry, root, None)
                }
                MachineUse::Verb | MachineUse::Named | MachineUse::Refused(_) => unreachable!("plan never routes these"),
            }
        }
        Plan::Resolve(wherever, name) => resolve_and_route(command, wherever, name),
    }
}

/// [`Plan::Resolve`]: the daemon says where `name` is. A bare name it can't
/// look up (no daemon, too old, nothing anywhere for a container name) runs
/// here, so the local error or container lookup applies.
fn resolve_and_route(command: Command, wherever: Where, name: String) -> Result<Routed> {
    let qualified = matches!(wherever, Where::Machine(_));
    let containers = matches!(command, Command::Logs { .. } | Command::Inspect { .. });
    let mut conn = match connect() {
        Ok(conn) => conn,
        Err(e) if qualified => return Err(e.context(format!("`{}` needs devsandbox serve", name_of(&command).unwrap_or_default()))),
        Err(_) => return Ok(Routed::Here(command)),
    };
    let r = conn.call("instances.resolve", resolve_params(&wherever, &name))?;
    let found = match (r.error, r.result) {
        (None, result) => found_of(result.unwrap_or(Value::Null), &wherever)?,
        (Some(e), _) if e.code == "unknown-method" && !qualified => return Ok(Routed::Here(command)),
        (Some(e), _) if resolve_lost(qualified, &command, &e.code) => {
            eprintln!("{}", e.message);
            return Ok(Routed::Exit(SSH_FAILED));
        }
        // Not an instance: maybe a container name, which the machine looks up.
        (Some(e), _) if e.code == "not-found" && containers => {
            if !qualified {
                return Ok(Routed::Here(command));
            }
            Vec::new()
        }
        (Some(e), _) => bail!("{}", e.message),
    };
    // `rename` takes the exact name only (no sandbox/folder matching).
    let found: Vec<Found> = match &command {
        Command::Rename { .. } => found.into_iter().filter(|f| f.name == name).collect(),
        _ => found,
    };
    let chosen = match choose(&name, found, std::io::stdin().is_terminal(), &mut |prompt, options| {
        let refs: Vec<&str> = options.iter().map(String::as_str).collect();
        crate::commands::pick(prompt, &refs)
    })? {
        Some(found) => found,
        // A machine's container: as typed, at its first root.
        None if qualified && containers => Found { name: name.clone(), machine: None, root: None, sandbox: None },
        None if !qualified => return Ok(Routed::Here(command)),
        None => bail!("{}", crate::commands::no_instance_matches(name_of(&command).unwrap_or_default())),
    };
    let m = match (&chosen.machine, &wherever) {
        (Some(m), _) | (None, Where::Machine(m)) => m.clone(),
        (None, _) => return Ok(Routed::Here(renamed(command, chosen.name))),
    };
    let own_agent = crate::devsbd::bridge::own_agent();
    let entry = machine_info_with(&mut |method, params| conn.call(method, params), &m, own_agent)?;
    let u = calls::use_of(&command);
    if let calls::Use::Refused(what) = u {
        return Err(calls::refusal(&m, what));
    }
    if matches!(command, Command::Port { .. }) {
        drop(conn);
        let root = chosen.root.clone().or_else(|| entry.roots.first().cloned());
        let root = root_on(&m, root.as_deref())?.to_path_buf();
        return routed_port(command, entry, root, Some(chosen.name));
    }
    let root = chosen.root.as_deref().or(entry.roots.first().map(PathBuf::as_path));
    if u != calls::Use::Link {
        let root = root_on(&m, root)?;
        let tty = std::io::stdout().is_terminal();
        let (mut out, mut err) = (std::io::stdout().lock(), std::io::stderr());
        let call = &mut |method: &str, params| conn.call(method, params);
        return Ok(Routed::Exit(calls::named(call, &u, &m, &chosen.name, root, tty, &mut out, &mut err)?));
    }
    // An exec: `PTY` dials from here.
    let terminal = on_terminal();
    let reserve_255 = matches!(command, Command::Exec { reserve_255: true, .. });
    let one_dial = |mode| -> Result<Routed> {
        let request = pty_client::Request { root: root_on(&m, root)?.to_path_buf(), instance: chosen.name.clone(), mode };
        Ok(Routed::Exit(pty_client::dial(&endpoint::socket_dir()?, &m, &request, reserve_255)))
    };
    if let Some(mode) = service_mode(&command) {
        // A service terminal: one dial, its 255 reported once.
        if !terminal {
            bail!("`exec --service` on machine `{m}` opens a terminal in the service's container: it needs a TTY");
        }
        drop(conn);
        return one_dial(mode);
    }
    if let Some(routed) = session_exec(&command, &mut conn, &m, &chosen.name, root, terminal)? {
        return Ok(routed);
    }
    if let Some(id) = reconnect_session(&command, terminal, || crate::commands::exec::mint_session_id(crate::state::Instance::now()))? {
        let (first, again) = dials(&m, &command, &chosen.name, root, &id)?;
        drop(conn);
        let dir = endpoint::socket_dir()?;
        return reconnect_loop(&id, |redial| Ok(pty_client::dial(&dir, &m, if redial { &again } else { &first }, true)));
    }
    // What's left: an exec without a tty, one piped dial (never re-dialed:
    // nothing to go back to).
    let mode = pipe_mode(&command, terminal)?.context("this command doesn't run on a machine")?;
    drop(conn);
    one_dial(mode)
}

/// `exec --sessions` and `exec --attach [ID]` of instance `name` on machine
/// `m` (at `root` there): the session list through a `sessions.list` quick
/// call on `conn`; the attach one `PTY` dial ([`attach_mode`]; no loop, as
/// a local `--attach`: its 255 is the caller's to retry). `None` for any
/// other exec, which [`resolve_and_route`] goes on with.
fn session_exec(command: &Command, conn: &mut Conn, m: &str, name: &str, root: Option<&Path>, terminal: bool) -> Result<Option<Routed>> {
    let Command::Exec { attach, sessions, reserve_255, .. } = command else { return Ok(None) };
    if !*sessions && attach.is_none() {
        return Ok(None);
    }
    let key = format!("{m}/{name}");
    let root = root_on(m, root)?;
    let mut list = || -> Result<Vec<String>> {
        let r = conn.call("sessions.list", json!({ "root": root, "instance": name, "machine": m }))?;
        if let Some(e) = r.error {
            bail!("`{key}`: {}", e.message);
        }
        let lines = r.result.as_ref().and_then(|r| r.get("sessions")).cloned().unwrap_or(Value::Null);
        serde_json::from_value(lines).context("bad sessions.list result from devsandbox serve")
    };
    if *sessions {
        let lines = list()?;
        if lines.is_empty() {
            eprintln!("no sessions in `{key}`");
        }
        for line in lines {
            println!("{line}");
        }
        return Ok(Some(Routed::Exit(0)));
    }
    let id = match attach {
        Some(Some(id)) => crate::commands::exec::checked_session_id(id)?.to_string(),
        _ => {
            let listing = list()?.join("\n");
            crate::commands::exec::choose_detached(&key, &listing, std::io::stdin().is_terminal(), &mut |prompt, options| {
                crate::commands::pick(prompt, options)
            })?
        }
    };
    let request = pty_client::Request { root: root.to_path_buf(), instance: name.to_string(), mode: attach_mode(id, terminal) };
    Ok(Some(Routed::Exit(pty_client::dial(&endpoint::socket_dir()?, m, &request, *reserve_255))))
}

/// The `PTY` mode of an `--attach` to session `id`: the session itself on a
/// terminal, else its helper client piped with stdin kept (a local
/// `exec --attach`'s `shell_flags(false, false, false)`).
pub(crate) fn attach_mode(id: String, terminal: bool) -> pty_client::Mode {
    if terminal {
        return pty_client::Mode::Session { id, create: None };
    }
    pty_client::Mode::Pipe { argv: crate::commands::exec::session_attach_command(&id), interactive: true }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::Cli;

    fn command(args: &[&str]) -> Command {
        let cli = Cli::try_parse_from(std::iter::once("devsandbox").chain(args.iter().copied())).unwrap_or_else(|e| panic!("{e}"));
        cli.command.unwrap()
    }

    fn ms(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn plan_of(args: &[&str], machine: Option<&str>, machines: &[&str]) -> Result<Plan> {
        plan(&command(args), machine, false, &ms(machines))
    }

    /// Without machines nothing resolves through the daemon; a bare
    /// name runs as typed, `@/` drops, a qualified one or `--machine` fails.
    #[test]
    fn without_machines_everything_stays_here() {
        for args in [&["exec", "web"][..], &["logs", "web"], &["stop", "web"], &["run", "web"], &["ps"], &["rename", "a", "b"]] {
            assert_eq!(plan_of(args, None, &[]).unwrap(), Plan::Here, "{args:?}");
        }
        assert_eq!(plan_of(&["exec", "@/web"], None, &[]).unwrap(), Plan::HereAs("web".into()));
        assert_eq!(plan_of(&["stop", "@/web"], None, &[]).unwrap(), Plan::HereAs("web".into()));
        let e = plan_of(&["exec", "m/web"], None, &[]).unwrap_err();
        assert_eq!(e.to_string(), "no machine named `m` (devsandbox machine ls)");
        assert_eq!(plan_of(&["ps"], Some("m"), &[]).unwrap_err().to_string(), "no machine named `m` (devsandbox machine ls)");
        assert!(plan_of(&["exec", "a b/web"], None, &[]).is_err());
    }

    /// `--in-process`: names are already resolved, and the host may have
    /// machines of its own: nothing parses, nothing fans out.
    #[test]
    fn in_process_never_fans_out() {
        for args in [&["exec", "web"][..], &["exec", "m/web"], &["stop", "@/w"], &["rm", "a/b"]] {
            assert_eq!(plan(&command(args), None, true, &ms(&["m"])).unwrap(), Plan::Here, "{args:?}");
        }
        assert!(plan(&command(&["ps"]), Some("m"), true, &ms(&["m"])).is_err());
    }

    #[test]
    fn with_machines_names_resolve_and_verbs_are_left_to_the_client() {
        let m = &["box"];
        assert_eq!(plan_of(&["exec", "web"], None, m).unwrap(), Plan::Resolve(Where::Any, "web".into()));
        assert_eq!(plan_of(&["done", "box/web"], None, m).unwrap(), Plan::Resolve(Where::Machine("box".into()), "web".into()));
        assert_eq!(plan_of(&["exec", "@/web"], None, m).unwrap(), Plan::HereAs("web".into()));
        assert_eq!(plan_of(&["exec", "nope/web"], None, m).unwrap_err().to_string(), "no machine named `nope` (devsandbox machine ls)");
        // The verbs resolve in verbs_client; their machines are checked here.
        assert_eq!(plan_of(&["stop", "box/web"], None, m).unwrap(), Plan::Here);
        assert_eq!(plan_of(&["run", "web"], None, m).unwrap(), Plan::Here);
        // `@/` reaches verbs_client as typed, or it'd look everywhere.
        assert_eq!(plan_of(&["run", "@/web"], None, m).unwrap(), Plan::Here);
        assert_eq!(plan_of(&["rm", "@/web"], None, m).unwrap(), Plan::Here);
        assert!(plan_of(&["rm", "nope/web"], None, m).is_err());
        // A container name stays local unless qualified.
        assert_eq!(plan_of(&["logs", "devsandbox-web"], None, m).unwrap(), Plan::Here);
        assert_eq!(
            plan_of(&["inspect", "box/devsandbox-web"], None, m).unwrap(),
            Plan::Resolve(Where::Machine("box".into()), "devsandbox-web".into())
        );
        assert_eq!(plan_of(&["vscode", "web"], None, m).unwrap(), Plan::Resolve(Where::Any, "web".into()));
        assert_eq!(plan_of(&["ps"], None, m).unwrap(), Plan::Here);
        assert_eq!(plan_of(&["port", "--service", "db", "5432"], None, m).unwrap(), Plan::Here);
        // `port` on a machine's instance resolves like the rest, then
        // forwards from here (`Routed::Port`).
        assert_eq!(plan_of(&["port", "box/web", "3000"], None, m).unwrap(), Plan::Resolve(Where::Machine("box".into()), "web".into()));
        assert_eq!(
            plan_of(&["port", "box/web", "--service", "db", "5432"], None, m).unwrap(),
            Plan::Resolve(Where::Machine("box".into()), "web".into())
        );
        assert_eq!(plan_of(&["port", "web", "3000"], None, m).unwrap(), Plan::Resolve(Where::Any, "web".into()));
        assert_eq!(plan_of(&["port", "@/web", "3000"], None, m).unwrap(), Plan::HereAs("web".into()));
    }

    #[test]
    fn the_machine_flag_is_for_nameless_commands() {
        let m = &["box"];
        assert_eq!(plan_of(&["ps", "-a"], Some("box"), m).unwrap(), Plan::Machine("box".into(), MachineUse::Calls));
        assert_eq!(plan_of(&["service", "ls", "--json"], Some("box"), m).unwrap(), Plan::Machine("box".into(), MachineUse::Calls));
        assert_eq!(plan_of(&["stop", "--all"], Some("box"), m).unwrap(), Plan::Here);
        // `gc` is the daemon's verb: `verbs_client` sends it (`CliVerb::Gc`).
        assert_eq!(plan_of(&["gc", "--force"], Some("box"), m).unwrap(), Plan::Here);
        assert_eq!(plan_of(&["gc"], Some("nope"), m).unwrap_err().to_string(), "no machine named `nope` (devsandbox machine ls)");
        assert_eq!(plan_of(&["service", "rebuild", "db"], Some("box"), m).unwrap(), Plan::Here);
        let e = plan_of(&["exec", "web"], Some("box"), m).unwrap_err().to_string();
        assert!(e.ends_with("qualify the name instead: box/<name>"), "{e}");
        let e = plan_of(&["stop", "web"], Some("box"), m).unwrap_err().to_string();
        assert!(e.ends_with("qualify the name instead: box/<name>"), "{e}");
        for args in [&["run", "web", "--here"][..], &["run", "--here"]] {
            let e = plan_of(args, Some("box"), m).unwrap_err().to_string();
            assert_eq!(e, "--here runs on this host; it doesn't go with --machine box", "{args:?}");
        }
        for (args, what) in [(&["inbox", "ls"][..], "inbox"), (&["machine", "ls"], "machine"), (&["machine", "trust", "full"], "machine"),
            (&["machine", "roots", "add", "/x"], "machine"), (&["serve"], "serve"), (&["api", "--stdio"], "api"),
            (&["tunnel", "--service", "db", "--port", "5432"], "tunnel")]
        {
            let e = plan_of(args, Some("box"), m).unwrap_err().to_string();
            assert_eq!(e, format!("--machine isn't for `devsandbox {what}`"));
        }
        // A nameless `port` (a global service) runs on the machine; a named
        // one qualifies the name.
        assert_eq!(plan_of(&["port", "--service", "redis", "6379"], Some("box"), m).unwrap(), Plan::Machine("box".into(), MachineUse::Port));
        assert_eq!(
            plan_of(&["port", "--service", "redis", "1:6379", "6380"], Some("box"), m).unwrap(),
            Plan::Machine("box".into(), MachineUse::Port)
        );
        let e = plan_of(&["port", "--service", "redis", "6379"], Some("nope"), m).unwrap_err().to_string();
        assert_eq!(e, "no machine named `nope` (devsandbox machine ls)");
        for args in [&["port", "web", "3000"][..], &["port", "web", "--service", "db", "5432"], &["port", "3000"]] {
            let e = plan_of(args, Some("box"), m).unwrap_err().to_string();
            assert!(e.ends_with("qualify the name instead: box/<name>"), "{args:?}: {e}");
        }
        let e = plan_of(&["vscode", "web"], Some("box"), m).unwrap_err().to_string();
        assert!(e.contains("`vscode` doesn't run on machines yet"), "{e}");
        assert_eq!(machine_use(None), MachineUse::Refused("the dashboard"));
        assert!(plan_of(&["ps"], Some("a/b"), m).is_err(), "a bad machine name");
    }

    #[test]
    fn each_nameless_command_has_its_use() {
        let once = |args: &[&str]| machine_use(Some(&command(args)));
        for args in [&["ps", "--json"][..], &["stats"], &["ls"], &["status", "--json"], &["service", "ls"]] {
            assert_eq!(once(args), MachineUse::Calls, "{args:?}");
        }
        assert_eq!(once(&["gc", "--force"]), MachineUse::Verb);
        assert_eq!(once(&["rebuild", "--all"]), MachineUse::Verb);
        assert_eq!(once(&["start", "--all", "-d"]), MachineUse::Verb);
        assert_eq!(once(&["port", "--service", "redis", "6379"]), MachineUse::Port);
        assert_eq!(once(&["port", "--service", "redis"]), MachineUse::Port);
        assert_eq!(once(&["port", "web", "3000"]), MachineUse::Named);
        assert_eq!(once(&["port", "web", "--service", "db", "5432"]), MachineUse::Named);
        // This host's own lending.
        assert_eq!(once(&["lend"]), MachineUse::Refused("lend"));
        assert_eq!(once(&["lend", "drain"]), MachineUse::Refused("lend"));
        assert_eq!(once(&["fleet", "join", "me@hub"]), MachineUse::Refused("fleet"));
        assert_eq!(once(&["fleet", "leave"]), MachineUse::Refused("fleet"));
    }

    /// Per command, on any machine: an API call, a link stream (an exec's
    /// `PTY`, a `port`'s `TUNNEL`) or a refusal; never a `devsandbox …`
    /// there.
    #[test]
    fn every_machine_command_is_a_call_a_stream_or_refused() {
        use calls::{Refused, Use};
        let nameless = |args: &[&str]| (machine_use(Some(&command(args))), calls::use_of(&command(args)));
        assert_eq!(nameless(&["ls"]), (MachineUse::Calls, Use::Ls { json: false }));
        assert_eq!(nameless(&["status", "--json"]), (MachineUse::Calls, Use::Status));
        assert_eq!(nameless(&["ps"]), (MachineUse::Calls, Use::Ps { all: false, json: false }));
        assert_eq!(nameless(&["stats"]), (MachineUse::Calls, Use::Stats { json: false }));
        assert_eq!(nameless(&["service", "ls"]), (MachineUse::Calls, Use::ServiceLs { json: false }));
        assert_eq!(nameless(&["gc"]), (MachineUse::Verb, Use::Link));
        assert_eq!(nameless(&["port", "--service", "redis", "6379"]), (MachineUse::Port, Use::Link));

        let named = |args: &[&str]| calls::use_of(&command(args));
        assert_eq!(named(&["logs", "box/web"]), Use::Logs { lines: 50 });
        assert_eq!(named(&["logs", "--build", "box/web"]), Use::BuildLog { follow: false });
        assert_eq!(named(&["logs", "--build", "-f", "box/web"]), Use::BuildLog { follow: true });
        assert_eq!(named(&["inspect", "box/web"]), Use::Inspect { json: false });
        assert_eq!(named(&["done", "box/web"]), Use::Done(true));
        assert_eq!(named(&["undone", "box/web"]), Use::Done(false));
        assert_eq!(named(&["rename", "box/web", "x"]), Use::Rename("x".into()));
        assert_eq!(named(&["vscode", "box/web"]), Use::Refused(Refused::Vscode));
        // exec: a tty one dials a `PTY` session (`dials`), a non-tty one a
        // piped `PTY` after `reconnect_session` says it gets none.
        let ls = command(&["exec", "box/web", "--", "ls"]);
        assert_eq!(named(&["exec", "box/web", "--", "ls"]), Use::Link);
        assert_eq!(reconnect_session(&ls, true, || ID.into()).unwrap(), None);
        assert_eq!(pipe_mode(&ls, true).unwrap(), Some(pty_client::Mode::Pipe { argv: ms(&["ls"]), interactive: false }));
    }

    fn entry() -> Entry {
        Entry { roots: vec![PathBuf::from("/r")], ..Entry::ssh("box", "me@box") }
    }

    /// A re-run's lookup passes this process's agent when it's an
    /// absolute path.
    #[test]
    fn a_re_runs_lookup_reports_the_agent() {
        assert_eq!(info_params("box", Some("/tmp/agent.sock".into())), json!({"name": "box", "agent": "/tmp/agent.sock"}));
        assert_eq!(info_params("box", Some("rel.sock".into())), json!({"name": "box"}));
        assert_eq!(info_params("box", None), json!({"name": "box"}));
    }

    /// What `main` gets for a port: a nameless one's port comes out of
    /// `name`, a resolved instance keeps the ports as given.
    #[test]
    fn port_routes_carry_the_split_ports() {
        let Routed::Port { root, name, service, ports, .. } =
            routed_port(command(&["port", "--service", "redis", "6379", "7000:6380"]), entry(), "/r".into(), None).unwrap()
        else {
            panic!("not a port route")
        };
        assert_eq!((root, name, service, ports), (PathBuf::from("/r"), None, Some("redis".into()), ms(&["6379", "7000:6380"])));
        let Routed::Port { name, service, ports, entry: e, .. } =
            routed_port(command(&["port", "box/web", "--service", "db", "5432"]), entry(), "/r".into(), Some("web-2".into())).unwrap()
        else {
            panic!("not a port route")
        };
        assert_eq!((name, service, ports, e.name), (Some("web-2".into()), Some("db".into()), ms(&["5432"]), "box".to_string()));
    }

    #[test]
    fn root_bound_commands_list_once_per_root() {
        let roots = vec![PathBuf::from("/r/a"), PathBuf::from("/r/b b")];
        assert_eq!(root_banners("box", &roots, false).unwrap(), ["==> box:/r/a", "==> box:/r/b b"]);
        let e = root_banners("box", &roots, true).unwrap_err().to_string();
        assert!(e.contains("/r/a, /r/b b"), "{e}");
        assert_eq!(root_banners("box", &roots[..1], true).unwrap().len(), 1);
        assert!(root_banners("box", &[], false).is_err());
    }

    /// `exec --service`: never a session nor a pipe; one service `PTY` dial.
    #[test]
    fn service_execs_are_one_terminal() {
        let svc = command(&["exec", "box/web", "--service", "db"]);
        assert_eq!(reconnect_session(&svc, true, || ID.into()).unwrap(), None, "no session to reconnect to");
        assert_eq!(pipe_mode(&svc, false).unwrap(), None);
        assert_eq!(service_mode(&svc), Some(pty_client::Mode::Service { name: "db".into(), argv: vec![] }));
        let psql = command(&["exec", "-it", "box/web", "--service", "db", "psql", "-l"]);
        assert_eq!(service_mode(&psql), Some(pty_client::Mode::Service { name: "db".into(), argv: ms(&["psql", "-l"]) }));
        assert_eq!(reconnect_session(&psql, true, || ID.into()).unwrap(), None);
        assert_eq!(service_mode(&command(&["exec", "box/web"])), None);
        assert_eq!(calls::use_of(&svc), calls::Use::Link);
    }

    const ID: &str = "1790000000-a1b2";

    /// A dashboard tab's re-dial (`--reserve-255`) whose machine's link is
    /// down fails its resolve: that's a 255 to wait out, not the tab's end.
    /// A name the machine doesn't know, or any other exec, still fails.
    #[test]
    fn a_tab_dials_resolve_failure_is_a_lost_link() {
        let again = command(&["exec", "--reserve-255", "--attach=1790000000-a1b2", "--", "box/web"]);
        assert!(resolve_lost(true, &again, "failed"));
        assert!(!resolve_lost(true, &again, "not-found"));
        assert!(!resolve_lost(false, &again, "failed"), "a bare name isn't one machine's link");
        assert!(!resolve_lost(true, &command(&["exec", "box/web"]), "failed"));
    }

    /// 255 after a connected dial retries with backoff until another code
    /// comes; a dial that never connected ends on ssh's own 255.
    #[test]
    fn reconnect_retries_lost_connections_only() {
        let long = CONNECTED_AFTER;
        let short = Duration::from_millis(200);
        // A first dial refused at once: no loop (bad host, auth).
        assert_eq!(Reconnect::default().after(255, short), Next::Done { code: 255, gone: false });
        // The program's code, whatever ran: done.
        assert_eq!(Reconnect::default().after(0, short), Next::Done { code: 0, gone: false });
        assert_eq!(Reconnect::default().after(254, long), Next::Done { code: 254, gone: false });
        assert_eq!(Reconnect::default().after(1, long), Next::Done { code: 1, gone: false });

        let mut r = Reconnect::default();
        assert_eq!(r.after(255, long), Next::Retry { wait: Duration::from_secs(1), attempt: 1 });
        // The re-dials of one outage fail fast: the counter and the wait grow.
        let waits: Vec<Next> = (0..6).map(|_| r.after(255, short)).collect();
        let secs = [2, 4, 8, 10, 10, 10];
        for (i, (next, s)) in waits.into_iter().zip(secs).enumerate() {
            assert_eq!(next, Next::Retry { wait: Duration::from_secs(s), attempt: i as u32 + 2 });
        }
        // Connected again, then lost: a new outage from attempt 1.
        assert_eq!(r.after(255, long), Next::Retry { wait: Duration::from_secs(1), attempt: 1 });
        // The session is gone, or the program ended: done.
        assert_eq!(r.clone().after(NO_SESSION_EXIT, short), Next::Done { code: 250, gone: true });
        assert_eq!(r.clone().after(1, short), Next::Done { code: 1, gone: false });
        // Another devsbd's session: there, not gone.
        assert_eq!(r.clone().after(252, short), Next::Done { code: 252, gone: false });
        assert_eq!(backoff(1000), BACKOFF_CAP);
        assert_eq!(retry_note(1), "connection lost, reconnecting… (Ctrl-C to stop)");
        assert_eq!(retry_note(3), "reconnecting… (attempt 3)");
    }

    /// Only a machine exec with a tty reconnects, keeping a given id.
    #[test]
    fn tty_execs_get_a_session_to_reconnect_to() {
        let mint = || ID.to_string();
        let rs = |args: &[&str], terminal| reconnect_session(&command(args), terminal, mint).unwrap();
        assert_eq!(rs(&["exec", "box/web"], true).as_deref(), Some(ID), "the shell on a terminal");
        assert_eq!(rs(&["exec", "box/web"], false), None, "piped: no tty");
        assert_eq!(rs(&["exec", "-i", "box/web"], true), None, "explicit flags as given");
        assert_eq!(rs(&["exec", "-t", "box/web"], false).as_deref(), Some(ID));
        assert_eq!(rs(&["exec", "box/web", "zidane"], true), None, "a command takes -t as given");
        assert_eq!(rs(&["exec", "-it", "box/web", "zidane"], true).as_deref(), Some(ID));
        assert_eq!(rs(&["exec", "--session", "1790000000-0000", "box/web"], true).as_deref(), Some("1790000000-0000"));
        assert_eq!(rs(&["exec", "box/web", "--attach"], true), None);
        assert_eq!(rs(&["exec", "box/web", "--sessions"], true), None);
        assert_eq!(rs(&["logs", "box/web"], true), None);
        assert!(reconnect_session(&command(&["exec", "--session", "nope", "box/web"]), true, mint).is_err());
    }

    /// A machine exec without a tty is piped, `-i` and the login shell as a
    /// local `exec` decides them, a `--session` one in the helper's session
    /// client as a local one runs it; one with a tty, an `--attach` or a
    /// `--sessions` one isn't.
    #[test]
    fn non_tty_execs_are_piped() {
        use pty_client::Mode::Pipe;
        let pm = |args: &[&str], terminal| pipe_mode(&command(args), terminal).unwrap();
        let shell: Vec<String> = crate::commands::exec::SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect();
        assert_eq!(pm(&["exec", "box/web", "ls"], false), Some(Pipe { argv: ms(&["ls"]), interactive: false }));
        assert_eq!(pm(&["exec", "box/web", "ls"], true), Some(Pipe { argv: ms(&["ls"]), interactive: false }), "no -t: none");
        assert_eq!(pm(&["exec", "-i", "box/web", "cat"], false), Some(Pipe { argv: ms(&["cat"]), interactive: true }));
        assert_eq!(pm(&["exec", "box/web"], false), Some(Pipe { argv: shell.clone(), interactive: true }), "the shell keeps stdin");
        assert_eq!(pm(&["exec", "-i", "box/web"], true), Some(Pipe { argv: shell.clone(), interactive: true }), "explicit -i: no tty");
        assert_eq!(pm(&["exec", "box/web"], true), None, "the shell on a terminal: a session");
        assert_eq!(pm(&["exec", "-t", "box/web", "ls"], false), None);
        let new = |argv: &[String]| crate::commands::exec::session_new_command("1790000000-0000", argv);
        assert_eq!(
            pm(&["exec", "--session", "1790000000-0000", "box/web", "ls"], false),
            Some(Pipe { argv: new(&ms(&["ls"])), interactive: false })
        );
        assert_eq!(pm(&["exec", "--session", "1790000000-0000", "box/web"], false), Some(Pipe { argv: new(&shell), interactive: true }));
        assert_eq!(pm(&["exec", "--session", "1790000000-0000", "box/web"], true), None, "a tty: the reconnecting session");
        assert!(pipe_mode(&command(&["exec", "--session", "nope", "box/web", "ls"]), false).is_err());
        assert_eq!(pm(&["exec", "box/web", "--attach"], false), None);
        assert_eq!(pm(&["exec", "box/web", "--sessions"], false), None);
        assert_eq!(pm(&["logs", "box/web"], false), None);
    }

    /// An attach on a terminal is the session; without one, its helper
    /// client piped, stdin kept.
    #[test]
    fn attaches_without_a_terminal_are_piped() {
        assert_eq!(attach_mode(ID.into(), true), pty_client::Mode::Session { id: ID.into(), create: None });
        let piped = pty_client::Mode::Pipe { argv: crate::commands::exec::session_attach_command(ID), interactive: true };
        assert_eq!(attach_mode(ID.into(), false), piped);
    }

    /// A tty exec's dials: create-or-attach with the command (none: the
    /// login shell), then attach only, both at the instance's root there.
    #[test]
    fn tty_execs_dial_over_the_link() {
        let root = Some(Path::new("/w/fleet/hub/cfg"));
        let exec = command(&["exec", "-it", "box/web", "zidane", "-c"]);
        let request = |create: Option<Vec<String>>| pty_client::Request {
            root: "/w/fleet/hub/cfg".into(),
            instance: "web-2".into(),
            mode: pty_client::Mode::Session { id: ID.into(), create },
        };
        assert_eq!(dials("box", &exec, "web-2", root, ID).unwrap(), (request(Some(ms(&["zidane", "-c"]))), request(None)));
        let shell = dials("box", &command(&["exec", "box/web"]), "web-2", root, ID).unwrap();
        assert_eq!(shell, (request(Some(Vec::new())), request(None)));
        let e = dials("box", &exec, "web-2", None, ID).unwrap_err().to_string();
        assert_eq!(e, "machine `box` has no config roots (devsandbox machine ls)");
        assert!(dials("box", &command(&["logs", "web"]), "web", root, ID).is_err());
    }

    #[test]
    fn renaming_and_localizing() {
        let c = renamed(command(&["exec", "web", "ls"]), "web-2".into());
        assert!(matches!(&c, Command::Exec { name, command, .. } if name == "web-2" && command == &["ls"]));
        let c = localize(command(&["stop", "@/web"])).unwrap();
        assert!(matches!(&c, Command::Stop { name: Some(n), .. } if n == "web"));
        assert!(matches!(localize(command(&["run", "web"])).unwrap(), Command::Run { sandbox: Some(s), .. } if s == "web"));
        let Err(e) = localize(command(&["rm", "box/web"])) else { panic!("localized") };
        assert!(e.to_string().contains("needs devsandbox serve"), "{e}");
        assert!(matches!(localize(command(&["stop", "--all"])).unwrap(), Command::Stop { name: None, .. }));
    }

    fn found(name: &str, machine: Option<&str>, root: Option<&str>) -> Found {
        Found { name: name.into(), machine: machine.map(Into::into), root: root.map(PathBuf::from), sandbox: None }
    }

    #[test]
    fn resolve_answers_become_matches() {
        assert_eq!(resolve_params(&Where::Any, "w"), json!({"name": "w", "machines": true}));
        assert_eq!(resolve_params(&Where::Machine("a".into()), "w"), json!({"name": "w", "machine": "a"}));
        assert_eq!(resolve_params(&Where::Local, "w"), json!({"name": "w"}));

        let fed = json!({"matches": ["a/w-2", "w-1"], "found": [
            {"name": "w-2", "machine": "a", "root": "/r"}, {"name": "w-1", "machine": null, "root": "/here"}]});
        let f = found_of(fed, &Where::Any).unwrap();
        assert_eq!(f, vec![found("w-2", Some("a"), Some("/r")), found("w-1", None, Some("/here"))]);
        assert_eq!(labels(&f), ["a/w-2", "@/w-1"]);
        // A machine's own answer: its matches are that machine's.
        let there = json!({"matches": ["w"], "found": [{"name": "w", "machine": null, "root": "/r"}]});
        assert_eq!(found_of(there, &Where::Machine("a".into())).unwrap(), vec![found("w", Some("a"), Some("/r"))]);
        // An older daemon: no `found`.
        let old = found_of(json!({"matches": ["w", "w-2"]}), &Where::Any).unwrap();
        assert_eq!(old, vec![found("w", None, None), found("w-2", None, None)]);
        assert_eq!(labels(&old), ["w", "w-2"], "all local: as today");
    }

    #[test]
    fn several_matches_pick_on_a_tty_else_name_them() {
        let both = || vec![found("w-2", Some("a"), None), found("w-1", None, None)];
        let mut never = |_: &str, _: &[String]| -> Result<usize> { panic!("picked") };
        assert_eq!(choose("w", vec![], false, &mut never).unwrap(), None);
        assert_eq!(choose("w", vec![found("w", None, None)], false, &mut never).unwrap(), Some(found("w", None, None)));
        let e = choose("w", both(), false, &mut never).unwrap_err().to_string();
        assert_eq!(e, "`w` is ambiguous: a/w-2, @/w-1");
        let mut second = |prompt: &str, options: &[String]| {
            assert_eq!((prompt, options), ("`w` matches multiple instances", &ms(&["a/w-2", "@/w-1"])[..]));
            Ok(1)
        };
        assert_eq!(choose("w", both(), true, &mut second).unwrap(), Some(found("w-1", None, None)));
    }
}
