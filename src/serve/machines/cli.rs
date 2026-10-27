//! The CLI side of machines (docs/architecture/daemon/machines.md, *The CLI:
//! `cli.rs`*): where a command's `<name>` points, and the commands that don't
//! go through the daemon (`logs`, `exec`, `inspect`, `done`, `rename`, …)
//! re-run over ssh when it points at a machine, as do the nameless ones given
//! `--machine`.
//!
//! The verbs (`run`, `rebuild`, …) resolve in `verbs_client` instead, from
//! the same pieces ([`resolve_params`], [`found_of`], [`choose`]); [`route`]
//! only checks their names here.
//!
//! The decisions are pure ([`plan`], [`machine_use`], [`remote_args`],
//! [`per_root`], [`hold_params`]); [`route`] does the I/O: the daemon
//! lookups, then an `exec` of ssh (or a spawn per root). For a machine with
//! `agent` on, ssh is spawned and waited for instead of `exec`ed: this
//! process keeps a daemon connection holding the machine (`machines.hold`),
//! so its agent relay is up while ssh runs, and ssh has no `-A`.
//!
//! A tty `exec` on a machine is spawned too, in a loop: it runs in a helper
//! session this process names ([`reconnect_session`]), and a lost connection
//! (ssh's 255) re-dials an attach to it ([`Reconnect`], [`session_dials`]).

use std::ffi::OsString;
use std::io::IsTerminal;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::api::MachineInfo;
use super::{Entry, Where, check_name, no_machine, parse_name, ssh};
use crate::serve::api::{Found, Resolved};
use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::{Command, ServiceCommand};

/// What `--machine` does with a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MachineUse {
    /// Over ssh once, with these args (`ps`, `stats`).
    Once(Vec<String>),
    /// Over ssh once per registered root, `-C <root>` first. `json`: its
    /// output is one document, so several roots can't be concatenated.
    PerRoot { args: Vec<String>, json: bool },
    /// A daemon verb (`stop/start/rebuild --all`, `service rebuild`):
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

fn flag(on: bool, name: &str) -> Option<String> {
    on.then(|| name.to_string())
}

fn args(fixed: &[&str], flags: impl IntoIterator<Item = Option<String>>) -> Vec<String> {
    fixed.iter().map(|s| s.to_string()).chain(flags.into_iter().flatten()).collect()
}

/// `--machine`'s use for `command` (`None`: the dashboard). The remote args
/// are rebuilt from the parsed command, never edited out of our argv.
pub(crate) fn machine_use(command: Option<&Command>) -> MachineUse {
    let Some(command) = command else { return MachineUse::Refused("the dashboard") };
    match command {
        Command::Ps { all, json } => MachineUse::Once(args(&["ps"], [flag(*all, "--all"), flag(*json, "--json")])),
        Command::Stats { json } => MachineUse::Once(args(&["stats"], [flag(*json, "--json")])),
        Command::Ls { json } => MachineUse::PerRoot { args: args(&["ls"], [flag(*json, "--json")]), json: *json },
        Command::Status { json: _ } => MachineUse::PerRoot { args: args(&["status", "--json"], []), json: true },
        Command::Gc { force } => MachineUse::PerRoot { args: args(&["gc"], [flag(*force, "--force")]), json: false },
        Command::Service { cmd: ServiceCommand::Ls { json } } => {
            MachineUse::PerRoot { args: args(&["service", "ls"], [flag(*json, "--json")]), json: *json }
        }
        Command::Service { cmd: ServiceCommand::Rebuild { .. } } => MachineUse::Verb,
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
        Command::Machine { .. } => MachineUse::Refused("machine"),
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
    /// `--machine m` with an ssh use ([`MachineUse::Once`] / `PerRoot`).
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
    let r = call("machines.get", json!({ "name": m }))?;
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

// ---- the ssh re-run -----------------------------------------------------------

/// The args a named command re-runs with on its machine: `-C <root>` (the
/// instance's root there; `-C` is a local path, never forwarded), the
/// subcommand and its flags, the unqualified `name`, `exec`'s command after
/// `--`. `None` for a command that doesn't re-run (the verbs, `vscode`, `port`).
pub(crate) fn remote_args(command: &Command, name: &str, root: Option<&Path>) -> Option<Vec<String>> {
    let mut out: Vec<String> = match root {
        Some(root) => vec!["-C".into(), root.to_string_lossy().into_owned()],
        None => Vec::new(),
    };
    let s = |v: &str| v.to_string();
    match command {
        Command::Logs { lines, build, follow, .. } => {
            out.extend([s("logs"), s("-n"), lines.to_string()]);
            out.extend([flag(*build, "--build"), flag(*follow, "--follow")].into_iter().flatten());
            out.extend([s("--"), s(name)]);
        }
        Command::Inspect { json, .. } => {
            out.push(s("inspect"));
            out.extend(flag(*json, "--json"));
            out.extend([s("--"), s(name)]);
        }
        Command::Done { .. } => out.extend([s("done"), s("--"), s(name)]),
        Command::Undone { .. } => out.extend([s("undone"), s("--"), s(name)]),
        Command::Rename { new_name, .. } => out.extend([s("rename"), s("--"), s(name), new_name.clone()]),
        Command::Exec { interactive, tty, attach, sessions, session, reserve_255, command, .. } => {
            out.push(s("exec"));
            let flags = [flag(*interactive, "-i"), flag(*tty, "-t"), flag(*sessions, "--sessions"), flag(*reserve_255, "--reserve-255")];
            out.extend(flags.into_iter().flatten());
            out.extend(session.as_ref().map(|id| format!("--session={id}")));
            // The name before `--`: after it, it would be the command.
            out.push(s(name));
            // After the name: `--attach`'s value is optional, so a bare one
            // before it would take the name as its id.
            out.extend(attach.as_ref().map(|id| match id {
                Some(id) => format!("--attach={id}"),
                None => s("--attach"),
            }));
            if !command.is_empty() {
                out.push(s("--"));
                out.extend(command.iter().cloned());
            }
        }
        _ => return None,
    }
    Some(out)
}

// ---- reconnecting a tty exec ------------------------------------------------

/// ssh's exit code for its own failure (a lost or refused connection).
pub(crate) const SSH_FAILED: i32 = 255;

/// The helper's `NO_SESSION_EXIT` (`devsbd/src/sessions.rs`): an attach to
/// a session that is unknown or `lost`. Its siblings need no mirror: like
/// every code but 255 they end [`Reconnect`]'s loop as the session's answer,
/// with the helper's own note on stderr (`DETACHED_EXIT` 251: attached
/// elsewhere; `VERSION_EXIT` 252: started by another devsbd, still there).
pub(crate) const NO_SESSION_EXIT: i32 = 250;

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

/// The reconnect decision of a session exec over ssh, shared by the CLI loop
/// ([`route`]) and the dashboard's remote tabs (`tui::app::terminal::Redial`):
/// pure over each dial's exit code and run time. A 255 is ssh's alone (the
/// remote `exec --reserve-255` reports a program's 255 as 254), so it means
/// the connection went: re-dial, with backoff, for as long as it takes. Any
/// other code is the session's answer and ends the loop.
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
/// `--sessions` already. The tty rule is the remote `exec`'s
/// (`commands::exec::shell_flags`), `terminal` standing for the remote
/// stdin: ssh gives it a tty ([`ssh::cli_argv`]) exactly when this process is
/// on a terminal. A `--session` given is kept, else one is minted here.
pub(crate) fn reconnect_session(command: &Command, terminal: bool, mint: impl FnOnce() -> String) -> Result<Option<String>> {
    let Command::Exec { interactive, tty, attach: None, sessions: false, session, command, .. } = command else {
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

/// The two remote commands of a session exec (`command`, an `Exec`) in
/// session `id`: the first dial, `exec --session=<id>` as typed
/// (create-or-attach), and every re-dial, `exec --attach=<id>` (attach only:
/// a reconnect never starts a second program). Both `--reserve-255`.
pub(crate) fn session_dials(command: &Command, id: &str) -> Option<(Command, Command)> {
    let Command::Exec { interactive, tty, name, command, .. } = command else { return None };
    let first = Command::Exec {
        interactive: *interactive,
        tty: *tty,
        attach: None,
        sessions: false,
        session: Some(id.to_string()),
        reserve_255: true,
        name: name.clone(),
        command: command.clone(),
    };
    let again = Command::Exec {
        interactive: false,
        tty: false,
        attach: Some(Some(id.to_string())),
        sessions: false,
        session: None,
        reserve_255: true,
        name: name.clone(),
        command: Vec::new(),
    };
    Some((first, again))
}

/// Run `first` (ssh), then `again` after each lost connection, per
/// [`Reconnect`], with `held` kept to the end. Each dial is spawned
/// ([`spawn_ssh_owning_signals`]): this process outlives ssh. Between dials
/// SIGINT is ours again, so Ctrl-C during a wait ends it.
fn reconnect_loop(first: &[OsString], again: &[OsString], id: &str, held: Option<Conn>) -> Result<Routed> {
    let mut state = Reconnect::default();
    let mut argv = first;
    loop {
        let started = Instant::now();
        let code = spawn_ssh_owning_signals(argv)?;
        match state.after(code, started.elapsed()) {
            Next::Retry { wait, attempt } => {
                eprintln!("{}", retry_note(attempt));
                std::thread::sleep(wait);
                argv = again;
            }
            Next::Done { code, gone } => {
                if gone {
                    eprintln!("{}", gone_note(id));
                }
                drop(held);
                return Ok(Routed::Exit(code));
            }
        }
    }
}

/// `--machine m` on a root-bound command: one run per registered root,
/// `-C <root>` first, each with its `==> m:<root>` banner. `--json` with
/// several roots is an error: concatenated documents don't parse.
pub(crate) fn per_root(m: &str, roots: &[PathBuf], args: &[String], json: bool) -> Result<Vec<(String, Vec<String>)>> {
    if roots.is_empty() {
        bail!("machine `{m}` has no config roots (devsandbox machine ls)");
    }
    if json && roots.len() > 1 {
        let list: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        bail!("--json with machine `{m}`'s {} roots would print one document each: {}", roots.len(), list.join(", "));
    }
    Ok(roots
        .iter()
        .map(|root| {
            let mut run = vec!["-C".to_string(), root.to_string_lossy().into_owned()];
            run.extend(args.iter().cloned());
            (banner(m, root), run)
        })
        .collect())
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

/// `machines.hold`'s params for a re-run on `entry`, `None` when it needs no
/// hold (`agent` off: nothing to keep up). `own_agent` is this process's
/// `SSH_AUTH_SOCK`, passed on when absolute, so the relay serves the agent
/// of the terminal the command runs in.
pub(crate) fn hold_params(entry: &Entry, own_agent: Option<PathBuf>) -> Option<Value> {
    if !entry.agent {
        return None;
    }
    let mut params = json!({ "name": entry.name });
    if let Some(agent) = own_agent.filter(|a| a.is_absolute()) {
        params["agent"] = json!(agent);
    }
    Some(params)
}

/// Hold `entry` on `conn` for a re-run ([`hold_params`]): `conn` back when
/// held, to keep until ssh exits. A hold that fails (a daemon without the
/// method, an error) is a warning: the command runs without the agent.
fn hold(mut conn: Conn, entry: &Entry) -> Option<Conn> {
    let params = hold_params(entry, crate::devsbd::bridge::own_agent())?;
    let failed = match conn.call("machines.hold", params) {
        Ok(r) => match r.error {
            None => return Some(conn),
            Some(e) => e.message,
        },
        Err(e) => format!("{e:#}"),
    };
    eprintln!("warning: cannot hold machine `{}` for its ssh-agent relay: {failed}", entry.name);
    None
}

/// Run `argv` (ssh) on `entry`: with `held` (a [`hold`]ing connection),
/// spawned and waited for with the connection kept to its end; else
/// `exec`ed, as without an agent.
fn run_ssh(argv: &[OsString], held: Option<Conn>) -> Result<Routed> {
    let Some(conn) = held else { return Err(exec_ssh(argv)) };
    let code = spawn_ssh_owning_signals(argv);
    drop(conn);
    Ok(Routed::Exit(code?))
}

/// Where `command` runs (module doc): here (maybe with its name resolved),
/// or on a machine over ssh, which replaces this process (or, `agent` on,
/// runs as its child: [`hold`]; a tty `exec` always does, re-dialed after a
/// lost connection: [`reconnect_loop`]). `machine` is
/// `--machine`, `machines` the registered names ([`super::registered_names`]).
pub(crate) fn route(machine: Option<&str>, in_process: bool, machines: &[String], command: Command) -> Result<Routed> {
    match plan(&command, machine, in_process, machines)? {
        Plan::Here => Ok(Routed::Here(command)),
        Plan::HereAs(name) => Ok(Routed::Here(renamed(command, name))),
        Plan::Machine(m, usage) => {
            let mut conn = connect().context("--machine needs devsandbox serve")?;
            let entry = machine_info(&mut |method, params| conn.call(method, params), &m)?;
            let held = match usage {
                MachineUse::Once(_) | MachineUse::PerRoot { .. } => hold(conn, &entry),
                _ => {
                    drop(conn);
                    None
                }
            };
            match usage {
                MachineUse::Once(args) => run_ssh(&ssh::cli_argv(&entry, on_terminal(), &args)?, held),
                MachineUse::PerRoot { args, json } => {
                    let runs = per_root(&m, &entry.roots, &args, json)?;
                    let mut code = 0;
                    for (banner, args) in runs {
                        eprintln!("{banner}");
                        let status = spawn_ssh(&ssh::cli_argv(&entry, on_terminal(), &args)?)?;
                        if code == 0 {
                            code = status;
                        }
                    }
                    drop(held);
                    Ok(Routed::Exit(code))
                }
                MachineUse::Port => {
                    let root = entry.roots.first().cloned().with_context(|| format!("machine `{m}` has no config roots (devsandbox machine ls)"))?;
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
    if matches!(command, Command::Vscode { .. }) {
        bail!("`{m}/{}` is on a machine: `devsandbox vscode` is not supported on machines yet", chosen.name);
    }
    let entry = machine_info(&mut |method, params| conn.call(method, params), &m)?;
    if matches!(command, Command::Port { .. }) {
        drop(conn);
        let root = chosen.root.clone().or_else(|| entry.roots.first().cloned());
        let root = root.with_context(|| format!("machine `{m}` has no config roots (devsandbox machine ls)"))?;
        return routed_port(command, entry, root, Some(chosen.name));
    }
    let root = chosen.root.as_deref().or(entry.roots.first().map(PathBuf::as_path));
    let terminal = on_terminal();
    if let Some(id) = reconnect_session(&command, terminal, || crate::commands::exec::mint_session_id(crate::state::Instance::now()))? {
        let (first, again) = session_dials(&command, &id).context("not an exec")?;
        let first = ssh::cli_argv(&entry, terminal, &remote_args(&first, &chosen.name, root).context("not an exec")?)?;
        let again = ssh::cli_argv(&entry, terminal, &remote_args(&again, &chosen.name, root).context("not an exec")?)?;
        return reconnect_loop(&first, &again, &id, hold(conn, &entry));
    }
    let args = remote_args(&command, &chosen.name, root).context("this command doesn't run on a machine")?;
    let argv = ssh::cli_argv(&entry, terminal, &args)?;
    run_ssh(&argv, hold(conn, &entry))
}

/// Replace this process with `argv` (ssh): its exit is ours, its terminal
/// handling (`-t`) the user's. Returns only on failure.
fn exec_ssh(argv: &[OsString]) -> anyhow::Error {
    let Some((program, args)) = argv.split_first() else { return anyhow::anyhow!("empty ssh argv") };
    let e = std::process::Command::new(program).args(args).exec();
    anyhow::Error::new(e).context(format!("cannot run {}", program.to_string_lossy()))
}

/// Run `argv` (ssh) to its end; its exit code (a signal as a shell reports it).
fn spawn_ssh(argv: &[OsString]) -> Result<i32> {
    let (program, args) = argv.split_first().context("empty ssh argv")?;
    let status =
        std::process::Command::new(program).args(args).status().with_context(|| format!("cannot run {}", program.to_string_lossy()))?;
    Ok(exit_code(status))
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(1)
}

/// [`spawn_ssh`] in place of an `exec`: ssh owns the terminal (`-t`), and a
/// Ctrl-C with `-T` reaches the whole process group, so this process
/// ignores SIGINT and SIGQUIT while ssh runs and lives to exit with its
/// status. ssh gets them back as they were here (SIG_IGN would survive the
/// exec, and an ssh ignoring Ctrl-C can't be stopped).
fn spawn_ssh_owning_signals(argv: &[OsString]) -> Result<i32> {
    const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGQUIT];
    let (program, args) = argv.split_first().context("empty ssh argv")?;
    // SAFETY: `signal` swaps a disposition; SIG_IGN is always valid.
    let prev = SIGNALS.map(|sig| unsafe { libc::signal(sig, libc::SIG_IGN) });
    // What the child starts with: ignored only if it was here before; a
    // handler of ours means default after the exec anyway.
    let child = prev.map(|p| if p == libc::SIG_IGN { libc::SIG_IGN } else { libc::SIG_DFL });
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    // SAFETY: only `signal`, which is async-signal-safe, between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            for (sig, disposition) in SIGNALS.into_iter().zip(child) {
                libc::signal(sig, disposition);
            }
            Ok(())
        });
    }
    let status = cmd.status();
    for (sig, p) in SIGNALS.into_iter().zip(prev) {
        // SAFETY: `p` is what `signal` returned for `sig`.
        unsafe { libc::signal(sig, p) };
    }
    let status = status.with_context(|| format!("cannot run {}", program.to_string_lossy()))?;
    Ok(exit_code(status))
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
        assert_eq!(
            plan_of(&["ps", "-a"], Some("box"), m).unwrap(),
            Plan::Machine("box".into(), MachineUse::Once(ms(&["ps", "--all"])))
        );
        assert_eq!(
            plan_of(&["service", "ls", "--json"], Some("box"), m).unwrap(),
            Plan::Machine("box".into(), MachineUse::PerRoot { args: ms(&["service", "ls", "--json"]), json: true })
        );
        assert_eq!(plan_of(&["stop", "--all"], Some("box"), m).unwrap(), Plan::Here);
        assert_eq!(plan_of(&["service", "rebuild", "db"], Some("box"), m).unwrap(), Plan::Here);
        let e = plan_of(&["exec", "web"], Some("box"), m).unwrap_err().to_string();
        assert!(e.ends_with("qualify the name instead: box/<name>"), "{e}");
        let e = plan_of(&["stop", "web"], Some("box"), m).unwrap_err().to_string();
        assert!(e.ends_with("qualify the name instead: box/<name>"), "{e}");
        for (args, what) in [(&["inbox", "ls"][..], "inbox"), (&["machine", "ls"], "machine"), (&["serve"], "serve"), (&["api", "--stdio"], "api"),
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
    fn nameless_args_are_rebuilt_from_the_command() {
        let once = |args: &[&str]| machine_use(Some(&command(args)));
        assert_eq!(once(&["ps", "--json"]), MachineUse::Once(ms(&["ps", "--json"])));
        assert_eq!(once(&["stats"]), MachineUse::Once(ms(&["stats"])));
        assert_eq!(once(&["ls"]), MachineUse::PerRoot { args: ms(&["ls"]), json: false });
        assert_eq!(once(&["status", "--json"]), MachineUse::PerRoot { args: ms(&["status", "--json"]), json: true });
        assert_eq!(once(&["gc", "--force"]), MachineUse::PerRoot { args: ms(&["gc", "--force"]), json: false });
        assert_eq!(once(&["rebuild", "--all"]), MachineUse::Verb);
        assert_eq!(once(&["start", "--all", "-d"]), MachineUse::Verb);
        assert_eq!(once(&["port", "--service", "redis", "6379"]), MachineUse::Port);
        assert_eq!(once(&["port", "--service", "redis"]), MachineUse::Port);
        assert_eq!(once(&["port", "web", "3000"]), MachineUse::Named);
        assert_eq!(once(&["port", "web", "--service", "db", "5432"]), MachineUse::Named);
    }

    fn entry() -> Entry {
        Entry { roots: vec![PathBuf::from("/r")], ..Entry::ssh("box", "me@box") }
    }

    /// A re-run holds its machine only with `agent` on, passing this
    /// process's agent when it's an absolute path.
    #[test]
    fn re_runs_hold_machines_with_the_agent_on() {
        assert_eq!(hold_params(&entry(), Some("/tmp/agent.sock".into())), None, "agent off: exec as ever");
        let on = Entry { agent: true, ..entry() };
        assert_eq!(hold_params(&on, Some("/tmp/agent.sock".into())), Some(json!({"name": "box", "agent": "/tmp/agent.sock"})));
        assert_eq!(hold_params(&on, Some("rel.sock".into())), Some(json!({"name": "box"})));
        assert_eq!(hold_params(&on, None), Some(json!({"name": "box"})));
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
    fn root_bound_commands_run_once_per_root() {
        let roots = vec![PathBuf::from("/r/a"), PathBuf::from("/r/b b")];
        let runs = per_root("box", &roots, &ms(&["ls"]), false).unwrap();
        assert_eq!(
            runs,
            vec![
                ("==> box:/r/a".to_string(), ms(&["-C", "/r/a", "ls"])),
                ("==> box:/r/b b".to_string(), ms(&["-C", "/r/b b", "ls"])),
            ]
        );
        let e = per_root("box", &roots, &ms(&["ls", "--json"]), true).unwrap_err().to_string();
        assert!(e.contains("/r/a, /r/b b"), "{e}");
        assert_eq!(per_root("box", &roots[..1], &ms(&["status", "--json"]), true).unwrap().len(), 1);
        assert!(per_root("box", &[], &ms(&["ls"]), false).is_err());
    }

    #[test]
    fn remote_args_per_command() {
        let root = Some(Path::new("/srv/it's"));
        let ra = |args: &[&str], name: &str, root| remote_args(&command(args), name, root).unwrap();
        assert_eq!(ra(&["logs", "box/web"], "web-2", root), ms(&["-C", "/srv/it's", "logs", "-n", "50", "--", "web-2"]));
        assert_eq!(
            ra(&["logs", "-n", "5", "--build", "-f", "web"], "web", None),
            ms(&["logs", "-n", "5", "--build", "--follow", "--", "web"])
        );
        assert_eq!(ra(&["inspect", "--json", "web"], "web", root), ms(&["-C", "/srv/it's", "inspect", "--json", "--", "web"]));
        assert_eq!(ra(&["done", "web"], "w", None), ms(&["done", "--", "w"]));
        assert_eq!(ra(&["undone", "web"], "w", None), ms(&["undone", "--", "w"]));
        assert_eq!(ra(&["rename", "box/old", "new"], "old", root), ms(&["-C", "/srv/it's", "rename", "--", "old", "new"]));
        // exec's command is kept word for word, spaces and flags included.
        assert_eq!(
            ra(&["exec", "-i", "box/web", "--", "sh", "-c", "echo 'a b'", "-x"], "web", root),
            ms(&["-C", "/srv/it's", "exec", "-i", "web", "--", "sh", "-c", "echo 'a b'", "-x"])
        );
        assert_eq!(ra(&["exec", "web", "uname", "-a"], "web", None), ms(&["exec", "web", "--", "uname", "-a"]));
        assert_eq!(ra(&["exec", "-t", "web"], "web", None), ms(&["exec", "-t", "web"]));
        // Session flags ride along; a bare `--attach` stays after the name.
        assert_eq!(ra(&["exec", "box/web", "--attach"], "web", None), ms(&["exec", "web", "--attach"]));
        assert_eq!(
            ra(&["exec", "box/web", "--attach", "1790000000-a1b2"], "web", None),
            ms(&["exec", "web", "--attach=1790000000-a1b2"])
        );
        assert_eq!(ra(&["exec", "--sessions", "box/web"], "web", None), ms(&["exec", "--sessions", "web"]));
        assert_eq!(
            ra(&["exec", "--session", "1790000000-a1b2", "box/web", "zidane"], "web", root),
            ms(&["-C", "/srv/it's", "exec", "--session=1790000000-a1b2", "web", "--", "zidane"])
        );
        // What it emits parses back to the same flags.
        let back = command(&ra(&["exec", "box/web", "--attach"], "web", None).iter().map(String::as_str).collect::<Vec<_>>());
        assert!(matches!(back, Command::Exec { attach: Some(None), ref name, .. } if name == "web"));
        assert_eq!(remote_args(&command(&["stop", "web"]), "web", None), None);
        assert_eq!(remote_args(&command(&["vscode", "web"]), "web", None), None);
    }

    const ID: &str = "1790000000-a1b2";

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

    /// The first dial creates (or attaches), every re-dial only attaches,
    /// both with `--reserve-255`; each parses back on the machine.
    #[test]
    fn first_dial_and_re_dials() {
        let root = Some(Path::new("/r"));
        let (first, again) = session_dials(&command(&["exec", "-it", "box/web", "zidane", "-c"]), ID).unwrap();
        let first = remote_args(&first, "web-2", root).unwrap();
        let again = remote_args(&again, "web-2", root).unwrap();
        assert_eq!(first, ms(&["-C", "/r", "exec", "-i", "-t", "--reserve-255", "--session=1790000000-a1b2", "web-2", "--", "zidane", "-c"]));
        assert_eq!(again, ms(&["-C", "/r", "exec", "--reserve-255", "web-2", "--attach=1790000000-a1b2"]));
        let parse = |args: &[String]| command(&args.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(matches!(parse(&first), Command::Exec { reserve_255: true, session: Some(ref s), ref command, .. } if s == ID && command == &ms(&["zidane", "-c"])));
        assert!(matches!(parse(&again), Command::Exec { reserve_255: true, attach: Some(Some(ref a)), ref name, .. } if a == ID && name == "web-2"));
        assert!(session_dials(&command(&["logs", "web"]), ID).is_none());
        let entry = entry();
        let argv = ssh::cli_argv(&entry, true, &again).unwrap();
        assert_eq!(argv[..4], ["ssh", "-t", "--", "me@box"].map(OsString::from));
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
