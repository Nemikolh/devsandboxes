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
//! [`per_root`]); [`route`] does the I/O: the daemon lookups, then an `exec`
//! of ssh (or a spawn per root).

use std::ffi::OsString;
use std::io::IsTerminal;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};

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
        r.matches.into_iter().map(|name| Found { name, machine: None, root: None }).collect()
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
    Ok(Entry { name: info.name, target: info.target, roots: info.roots, command: info.command })
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
        Command::Exec { interactive, tty, command, .. } => {
            out.push(s("exec"));
            out.extend([flag(*interactive, "-i"), flag(*tty, "-t")].into_iter().flatten());
            // The name before `--`: after it, it would be the command.
            out.push(s(name));
            if !command.is_empty() {
                out.push(s("--"));
                out.extend(command.iter().cloned());
            }
        }
        _ => return None,
    }
    Some(out)
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

/// Where `command` runs (module doc): here (maybe with its name resolved),
/// or on a machine over ssh, which replaces this process. `machine` is
/// `--machine`, `machines` the registered names ([`super::registered_names`]).
pub(crate) fn route(machine: Option<&str>, in_process: bool, machines: &[String], command: Command) -> Result<Routed> {
    match plan(&command, machine, in_process, machines)? {
        Plan::Here => Ok(Routed::Here(command)),
        Plan::HereAs(name) => Ok(Routed::Here(renamed(command, name))),
        Plan::Machine(m, usage) => {
            let mut conn = connect().context("--machine needs devsandbox serve")?;
            let entry = machine_info(&mut |method, params| conn.call(method, params), &m)?;
            drop(conn);
            match usage {
                MachineUse::Once(args) => Err(exec_ssh(&ssh::cli_argv(&entry, on_terminal(), &args)?)),
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
        None if qualified && containers => Found { name: name.clone(), machine: None, root: None },
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
    drop(conn);
    if matches!(command, Command::Port { .. }) {
        let root = chosen.root.clone().or_else(|| entry.roots.first().cloned());
        let root = root.with_context(|| format!("machine `{m}` has no config roots (devsandbox machine ls)"))?;
        return routed_port(command, entry, root, Some(chosen.name));
    }
    let root = chosen.root.as_deref().or(entry.roots.first().map(PathBuf::as_path));
    let args = remote_args(&command, &chosen.name, root).context("this command doesn't run on a machine")?;
    Err(exec_ssh(&ssh::cli_argv(&entry, on_terminal(), &args)?))
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
    Ok(status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(1))
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
        Entry { name: "box".into(), target: "me@box".into(), roots: vec![PathBuf::from("/r")], command: None }
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
        assert_eq!(remote_args(&command(&["stop", "web"]), "web", None), None);
        assert_eq!(remote_args(&command(&["vscode", "web"]), "web", None), None);
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
        Found { name: name.into(), machine: machine.map(Into::into), root: root.map(PathBuf::from) }
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
