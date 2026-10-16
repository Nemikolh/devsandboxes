//! The CLI side of verbs as requests (docs/architecture/daemon/verbs.md, *The
//! CLI side*): `run`, `rebuild`, `start`, `stop`, `rm` and `service rebuild`
//! sent to the daemon ([`super::verbs`] runs them) instead of run in this
//! process, so a build survives its terminal and runs with this shell's env
//! either way.
//!
//! Every prompt happens here, on the user's TTY, before the verb is sent
//! (the daemon's subprocess has no stdin): the sandbox pick and the example
//! config (`sandboxes.list`), the instance pick (`instances.resolve`), rm's
//! branch question (`ask` → `confirm` → resend). Then the request streams:
//! `output` goes to our stdout/stderr as it comes, and the answer decides the
//! exit code (F9). Ctrl-C cancels through a second connection
//! (`instances.cancel`) so the rollback still streams; when there's nothing
//! to name yet, or the cancel is refused, it hangs up, which the daemon
//! takes as a cancel. A second Ctrl-C kills this process: a hang-up too.
//!
//! With machines registered ([`Fed`], docs/architecture/daemon/machines.md
//! *The CLI: `cli.rs`*) a name is looked up on every machine too, and the
//! request carries the match's `machine` and its root there; without any,
//! every request is as before.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::api::SandboxEntry;
use super::client::{self, Conn, Streamed};
use super::endpoint;
use super::machines::cli::{choose, found_of, machine_info, resolve_params};
use super::machines::{Where, parse_name};
use super::proto::{Notification, Response, Version};
use super::verbs::{RebuildParams, RmParams, RunParams, ServicesRebuildParams, StartParams, StopParams, TtySize};
use crate::Command;
use crate::commands::run::cancel;
use crate::state::PhaseView;

/// How often the streaming loop wakes without a line to check for Ctrl-C.
const TICK: Duration = Duration::from_millis(100);
/// The exit code after a Ctrl-C, as a shell reports one.
const CANCELLED_EXIT: i32 = 130;
/// How often a verb the daemon lost is re-checked in `state.toml`.
const SETTLE_POLL: Duration = Duration::from_millis(500);

/// A verb the CLI was given, before its prompts. A `None` target is `--all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CliVerb {
    Run { sandbox: Option<String>, name: Option<String>, branch: Option<String>, base: Option<String>, json: bool, detach: bool },
    Rebuild { target: Option<String>, force: bool, detach: bool },
    Start { target: Option<String>, detach: bool },
    Stop { target: Option<String> },
    Rm { name: String, delete_branch: Option<bool>, force: bool },
    /// `service rebuild <name>`: names a service, so no prompt, no cancel by
    /// name (the first Ctrl-C hangs up, as `stop --all`'s).
    ServicesRebuild { name: String },
}

impl CliVerb {
    /// The command as the user typed it, for messages.
    fn command(&self) -> &'static str {
        match self {
            CliVerb::Run { .. } => "run",
            CliVerb::Rebuild { .. } => "rebuild",
            CliVerb::Start { .. } => "start",
            CliVerb::Stop { .. } => "stop",
            CliVerb::Rm { .. } => "rm",
            CliVerb::ServicesRebuild { .. } => "service rebuild",
        }
    }

    fn detach(&self) -> bool {
        matches!(
            self,
            CliVerb::Run { detach: true, .. } | CliVerb::Rebuild { detach: true, .. } | CliVerb::Start { detach: true, .. }
        )
    }

    /// Whether it writes a build log a lost request can be followed by (F10).
    fn builds(&self) -> bool {
        matches!(self, CliVerb::Run { .. } | CliVerb::Rebuild { .. } | CliVerb::Start { .. })
    }
}

/// The verb `command` sends to the daemon, or `None`: it runs in-process.
/// `run`'s hidden `--env`/`--dispatcher` aren't on the wire (only the
/// dispatcher passes them, from inside the daemon already), nor is `rm
/// --fail-on-ask` (the daemon's own flag).
pub(crate) fn from_command(command: &Command) -> Option<CliVerb> {
    Some(match command {
        Command::Run { env, dispatcher, .. } if !env.is_empty() || dispatcher.is_some() => return None,
        Command::Run { sandbox, name, branch, base, json, detach, .. } => CliVerb::Run {
            sandbox: sandbox.clone(),
            name: name.clone(),
            branch: branch.clone(),
            base: base.clone(),
            json: *json,
            detach: *detach,
        },
        Command::Rebuild { name, force, detach, .. } => {
            CliVerb::Rebuild { target: name.clone(), force: *force, detach: *detach }
        }
        Command::Start { name, detach, .. } => CliVerb::Start { target: name.clone(), detach: *detach },
        Command::Stop { name, .. } => CliVerb::Stop { target: name.clone() },
        Command::Rm { fail_on_ask: true, .. } => return None,
        Command::Rm { name, delete_branch, keep_branch, force, .. } => CliVerb::Rm {
            name: name.clone(),
            delete_branch: crate::rm_branch_flag(*delete_branch, *keep_branch),
            force: *force,
        },
        Command::Service { cmd: crate::ServiceCommand::Rebuild { name } } => CliVerb::ServicesRebuild { name: name.clone() },
        _ => return None,
    })
}

/// What machines change for a verb (docs/architecture/daemon/machines.md,
/// *The CLI: `cli.rs`*).
#[derive(Debug, Clone, Default)]
pub(crate) struct Fed {
    /// Some machine is registered: names resolve across machines. Without
    /// one (F15) every request is what it was before machines.
    pub machines: bool,
    /// `--machine`: an `--all` verb or `service rebuild` sent there.
    pub machine: Option<String>,
}

impl CliVerb {
    /// The `<name>` given, as typed (maybe qualified).
    fn name(&self) -> Option<&str> {
        match self {
            CliVerb::Run { sandbox: name, .. }
            | CliVerb::Rebuild { target: name, .. }
            | CliVerb::Start { target: name, .. }
            | CliVerb::Stop { target: name } => name.as_deref(),
            CliVerb::Rm { name, .. } => Some(name),
            CliVerb::ServicesRebuild { .. } => None,
        }
    }
}

/// How a client verb ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Done {
    /// Exit with this code: everything is printed.
    Exit(i32),
    /// No daemon took it (unreachable, or too old for verbs): run it
    /// in-process. The reason, for `-d`'s error; the `warning:` is printed.
    InProcess(String),
}

/// Send `verb` for config root `dir` to the daemon (lazy start), prompting
/// first, streaming its output, and say how to exit. An `Err` is the CLI's
/// error (exit 1).
pub(crate) fn run(dir: &Path, verb: CliVerb, fed: &Fed) -> Result<Done> {
    let root = dir.canonicalize().with_context(|| format!("no directory {}", dir.display()))?;
    let env = caller_env(std::env::vars_os());
    let mut conn = match endpoint::socket_dir().and_then(|socket| client::connect(&socket, "cli")) {
        Ok(conn) => conn,
        Err(e) => return in_process(&verb, fed, format!("{e:#}")),
    };
    let tty = std::io::stdin().is_terminal();
    // As the in-process tee decides (`build_log`): stderr too, or the pty
    // would merge the errors into stdout.
    let term = crate::build_log::terminal_size(1).filter(|_| std::io::stderr().is_terminal());
    let mut ui = Ui {
        tty,
        term,
        pick: &mut |prompt, options| {
            let refs: Vec<&str> = options.iter().map(String::as_str).collect();
            crate::commands::pick(prompt, &refs)
        },
        offer_example: &mut || crate::commands::run::offer_example_config(&root),
    };
    let list = match prepare(&verb, &root, env, fed, &mut |method, params| conn.call(method, params), &mut ui)? {
        Prep::Send(prepared) => vec![prepared],
        Prep::Each(list) => list,
        Prep::Exit(code) => return Ok(Done::Exit(code)),
        Prep::Old(why) => return in_process(&verb, fed, why),
    };
    // Several requests: a machine's roots, one after the other (F16). The
    // first failure's exit is the exit; a Ctrl-C or a lost daemon stops.
    let mut code = 0;
    for prepared in list {
        if let Some(banner) = &prepared.banner {
            eprintln!("{banner}");
        }
        let (done, go_on) = send(&mut conn, &verb, prepared)?;
        match done {
            Done::Exit(c) if code == 0 => code = c,
            Done::Exit(_) => {}
            Done::InProcess(why) => return in_process(&verb, fed, why),
        }
        if !go_on {
            break;
        }
    }
    Ok(Done::Exit(code))
}

/// One prepared request, streamed to its end; whether `conn` can take the
/// next (not after a hang-up, a lost daemon, a Ctrl-C).
fn send(conn: &mut Conn, verb: &CliVerb, prepared: Prepared) -> Result<(Done, bool)> {
    let instance = prepared.instance.clone();
    let machine = prepared.machine.clone();
    let mut last = Streaming::new(instance.clone(), false, None);
    let answer = with_confirm(prepared.params, &mut |params| {
        let (answer, streaming) = stream(conn, verb, prepared.method, params, instance.clone(), machine.clone())?;
        last = streaming;
        Ok(answer)
    }, &mut |question| crate::commands::confirm(question))?;
    if let Answer::Ok(v) = &answer
        && last.instance.is_none()
    {
        last.instance = v["instance"].as_str().map(str::to_string);
    }
    last.finish_stdout();

    let ending = ending(answer, last.cancelled);
    if !matches!(ending, Ok(Ending::Detached(_))) {
        last.replay();
    }
    Ok(match ending? {
        Ending::Exit(code) => (Done::Exit(code), code != CANCELLED_EXIT),
        Ending::Detached(result) => {
            print_detached(verb, &result, machine.as_deref())?;
            (Done::Exit(0), true)
        }
        Ending::Lost => (Done::Exit(lost(verb, last.instance.as_deref(), machine.as_deref())?), false),
        Ending::Old(why) => (Done::InProcess(why), false),
    })
}

/// The fallback, announced unless `-d` (which refuses it with `why`). A
/// verb on a machine has none: the registry and the links are the daemon's
/// (F13).
fn in_process(verb: &CliVerb, fed: &Fed, why: String) -> Result<Done> {
    if let Some(m) = &fed.machine {
        bail!("--machine {m} needs devsandbox serve: {why}");
    }
    if fed.machines
        && let Some(name) = verb.name()
        && let Ok((Where::Machine(m), _)) = parse_name(name)
    {
        bail!("`{name}` is on machine `{m}`, which needs devsandbox serve: {why}");
    }
    if !verb.detach() {
        eprintln!("warning: devsandbox serve: {why}; running in-process");
    }
    Ok(Done::InProcess(why))
}

/// The request env: this process's whole env (`${localEnv:…}`,
/// `SSH_AUTH_SOCK`, `PATH`, … must be the caller's, never the daemon's).
/// A non-UTF-8 var can't go on the wire and is left out. The dashboard sends
/// its own the same way.
pub(crate) fn caller_env(vars: impl IntoIterator<Item = (OsString, OsString)>) -> BTreeMap<String, String> {
    vars.into_iter().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect()
}

// ---- prompts --------------------------------------------------------------

/// The prompts, injectable for the tests.
struct Ui<'a> {
    /// stdin is a TTY: prompts may ask.
    tty: bool,
    /// The terminal's size when stdout and stderr are one: the build verbs
    /// run on a pty that size, so their output streams back as a terminal
    /// would show it (colour, docker build's progress). Not for `run
    /// --json`, whose stdout is the record.
    term: Option<TtySize>,
    /// A numbered pick among `options`; the chosen index.
    pick: &'a mut dyn FnMut(&str, &[String]) -> Result<usize>,
    /// `run` on a config without sandboxes (`offer_example_config`).
    offer_example: &'a mut dyn FnMut() -> Result<()>,
}

/// What [`prepare`] decided.
#[derive(Debug)]
enum Prep {
    Send(Prepared),
    /// One request per config root of a machine (`rebuild --all`, `service
    /// rebuild` with `--machine`), each with its banner.
    Each(Vec<Prepared>),
    /// Nothing to send (the example config was written instead).
    Exit(i32),
    /// The daemon is too old for this (`unknown-method`): why.
    Old(String),
}

/// The verb request, after the prompts.
#[derive(Debug)]
struct Prepared {
    method: &'static str,
    params: Value,
    /// The instance it acts on, when known before it runs (a Ctrl-C cancels
    /// it by name); `run` learns its own from `progress`.
    instance: Option<String>,
    /// The machine it's sent to (also in `params`), for the cancel, the
    /// printed name and a lost request.
    machine: Option<String>,
    /// The `==> m:<root>` line before it, when one of several.
    banner: Option<String>,
}

/// A daemon response, sorted for the client.
enum Reply {
    Ok(Value),
    /// `unknown-method`: an older daemon.
    Old(String),
    Err(super::proto::ErrorBody),
}

fn reply(r: Response) -> Reply {
    match r.error {
        Some(e) if e.code == "unknown-method" => Reply::Old(format!("{} ({})", e.message, e.code)),
        Some(e) => Reply::Err(e),
        None => Reply::Ok(r.result.unwrap_or(Value::Null)),
    }
}

type Call<'a> = dyn FnMut(&str, Value) -> Result<Response> + 'a;

/// Ask what has to be asked, then build the request. `call` is one plain
/// request on the connection.
fn prepare(
    verb: &CliVerb,
    root: &Path,
    env: BTreeMap<String, String>,
    fed: &Fed,
    call: &mut Call,
    ui: &mut Ui,
) -> Result<Prep> {
    let root: PathBuf = root.to_path_buf();
    macro_rules! resolved {
        ($name:expr) => {
            match target_of($name, &root, fed, call, ui)? {
                Ok(target) => target,
                Err(old) => return Ok(Prep::Old(old)),
            }
        };
    }
    // `stop`/`start --all` act on every instance in the machine's
    // `state.toml`, whatever the root: once, at its first. `rebuild --all`
    // resolves each against the root's config (others are skipped): once
    // per root.
    let machine_roots = |call: &mut Call| -> Result<Option<(String, Vec<PathBuf>)>> {
        let Some(m) = &fed.machine else { return Ok(None) };
        let roots = machine_info(call, m)?.roots;
        if roots.is_empty() {
            bail!("machine `{m}` has no config roots (devsandbox machine ls)");
        }
        Ok(Some((m.clone(), roots)))
    };
    let one = |method, params: Value, target: Option<&Target>| Prepared {
        method,
        params: routed(params, target.and_then(|t| t.machine.as_deref())),
        instance: target.map(|t| t.name.clone()),
        machine: target.and_then(|t| t.machine.clone()),
        banner: None,
    };
    let prepared = match verb.clone() {
        CliVerb::Run { sandbox, name, branch, base, json, detach } => {
            let Some(target) = run_target(sandbox, &root, fed, call, ui, json)? else {
                return Ok(Prep::Exit(0));
            };
            let target = match target {
                Ok(target) => target,
                Err(old) => return Ok(Prep::Old(old)),
            };
            let tty = if json { None } else { ui.term };
            let p = RunParams { root: target.root.clone(), env, sandbox: target.name.clone(), name, branch, base, json, detach, tty };
            // `run` names no instance before it runs: `progress` does.
            Prepared { instance: None, ..one("run", serde_json::to_value(p)?, Some(&target)) }
        }
        CliVerb::Rebuild { target, force, detach } => match target {
            Some(name) => {
                let t = resolved!(&name);
                let p = RebuildParams { root: t.root.clone(), env, instance: Some(t.name.clone()), all: false, force, detach, tty: ui.term };
                one("rebuild", serde_json::to_value(p)?, Some(&t))
            }
            None => match machine_roots(call)? {
                None => {
                    let p = RebuildParams { root, env, instance: None, all: true, force, detach, tty: ui.term };
                    one("rebuild", serde_json::to_value(p)?, None)
                }
                Some((m, roots)) => {
                    let mut each = Vec::new();
                    for root in roots {
                        let banner = super::machines::cli::banner(&m, &root);
                        let p = RebuildParams { root, env: env.clone(), instance: None, all: true, force, detach, tty: ui.term };
                        each.push(on_machine("rebuild", serde_json::to_value(p)?, &m, banner));
                    }
                    return Ok(Prep::Each(each));
                }
            },
        },
        CliVerb::Start { target, detach } => match target {
            Some(name) => {
                let t = resolved!(&name);
                let p = StartParams { root: t.root.clone(), env, instance: Some(t.name.clone()), all: false, detach, tty: ui.term };
                one("start", serde_json::to_value(p)?, Some(&t))
            }
            None => {
                let (root, m) = match machine_roots(call)? {
                    None => (root, None),
                    Some((m, roots)) => (roots[0].clone(), Some(m)),
                };
                let p = StartParams { root, env, instance: None, all: true, detach, tty: ui.term };
                Prepared { machine: m.clone(), ..one("start", routed(serde_json::to_value(p)?, m.as_deref()), None) }
            }
        },
        CliVerb::Stop { target } => match target {
            // Never `detach`: Ctrl-C (the hang-up) cancels a held verb.
            Some(name) => {
                let t = resolved!(&name);
                let p = StopParams { root: t.root.clone(), env, instance: Some(t.name.clone()), all: false, detach: false };
                one("stop", serde_json::to_value(p)?, Some(&t))
            }
            None => {
                let (root, m) = match machine_roots(call)? {
                    None => (root, None),
                    Some((m, roots)) => (roots[0].clone(), Some(m)),
                };
                let p = StopParams { root, env, instance: None, all: true, detach: false };
                Prepared { machine: m.clone(), ..one("stop", routed(serde_json::to_value(p)?, m.as_deref()), None) }
            }
        },
        CliVerb::Rm { name, delete_branch, force } => {
            let t = resolved!(&name);
            // Off a TTY nothing could answer: the non-TTY default, as today.
            let ask = ui.tty && delete_branch.is_none();
            let p = RmParams { root: t.root.clone(), env, instance: t.name.clone(), delete_branch, force, ask, detach: false };
            one("rm", serde_json::to_value(p)?, Some(&t))
        }
        CliVerb::ServicesRebuild { name } => match machine_roots(call)? {
            None => {
                let p = ServicesRebuildParams { root, env, name, detach: false };
                one("services.rebuild", serde_json::to_value(p)?, None)
            }
            // The machine's roots that define the service.
            Some((m, roots)) => {
                let mut each = Vec::new();
                for root in &roots {
                    match reply(call("services.get", json!({ "root": root, "name": name, "machine": m }))?) {
                        Reply::Ok(_) => {}
                        Reply::Err(e) if e.code == "not-found" => continue,
                        Reply::Old(why) => return Ok(Prep::Old(why)),
                        Reply::Err(e) => bail!("{}", e.message),
                    }
                    let banner = super::machines::cli::banner(&m, root);
                    let p = ServicesRebuildParams { root: root.clone(), env: env.clone(), name: name.clone(), detach: false };
                    each.push(on_machine("services.rebuild", serde_json::to_value(p)?, &m, banner));
                }
                if each.is_empty() {
                    let list: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
                    bail!("no service `{name}` on machine `{m}` (roots: {})", list.join(", "));
                }
                return Ok(Prep::Each(each));
            }
        },
    };
    Ok(Prep::Send(prepared))
}

/// `params` sent to `machine`, when there is one (F6).
fn routed(mut params: Value, machine: Option<&str>) -> Value {
    if let Some(m) = machine {
        params["machine"] = json!(m);
    }
    params
}

/// One of a machine's per-root requests.
fn on_machine(method: &'static str, params: Value, m: &str, banner: String) -> Prepared {
    Prepared { method, params: routed(params, Some(m)), instance: None, machine: Some(m.to_string()), banner: Some(banner) }
}

/// Where a verb acts: the name on its host, the machine (`None`: here) and
/// the config root the request carries (here: `-C`, as before machines;
/// there: the instance's or sandbox's root on the machine, F12).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    name: String,
    machine: Option<String>,
    root: PathBuf,
}

/// `name` as one instance (`instances.resolve`), picking on a TTY among
/// several as the in-process CLI does; `Err(why)` from an older daemon.
/// With machines (F11) a bare name is looked up everywhere, a qualified one
/// on its machine, `@/` here only.
fn target_of(name: &str, root: &Path, fed: &Fed, call: &mut Call, ui: &mut Ui) -> Result<Result<Target, String>> {
    let (wherever, name) = if fed.machines { parse_name(name)? } else { (Where::Local, name.to_string()) };
    let params = match wherever {
        Where::Local => json!({ "name": name }),
        _ => resolve_params(&wherever, &name),
    };
    let found = match reply(call("instances.resolve", params)?) {
        Reply::Ok(v) => found_of(v, &wherever)?,
        Reply::Old(why) => return Ok(Err(why)),
        Reply::Err(e) => bail!("{}", e.message),
    };
    let Some(found) = choose(&name, found, ui.tty, &mut *ui.pick)? else {
        bail!("{}", crate::commands::no_instance_matches(&name));
    };
    let Some(m) = found.machine else {
        return Ok(Ok(Target { name: found.name, machine: None, root: root.to_path_buf() }));
    };
    let root = match found.root {
        Some(root) => root,
        None => machine_info(call, &m)?
            .roots
            .into_iter()
            .next()
            .with_context(|| format!("machine `{m}` has no config roots (devsandbox machine ls)"))?,
    };
    Ok(Ok(Target { name: found.name, machine: Some(m), root }))
}

/// `run`'s sandbox: where it's defined, after the pick. `None`: nothing to
/// send (the example config was offered); `Err(why)` from an older daemon.
fn run_target(
    sandbox: Option<String>,
    root: &Path,
    fed: &Fed,
    call: &mut Call,
    ui: &mut Ui,
    json: bool,
) -> Result<Option<Result<Target, String>>> {
    // Always listed, not only for the pick: an empty config is the
    // example offer, which writes into the user's dir (F6).
    let params = if fed.machines { json!({ "root": root, "machines": true }) } else { json!({ "root": root }) };
    let rows: Vec<SandboxEntry> = match reply(call("sandboxes.list", params)?) {
        Reply::Ok(rows) => serde_json::from_value(rows).context("bad sandboxes.list result from devsandbox serve")?,
        Reply::Old(why) => return Ok(Some(Err(why))),
        Reply::Err(e) => bail!("{}", e.message),
    };
    let wanted = match sandbox {
        Some(s) if fed.machines => Some(parse_name(&s)?),
        Some(s) => Some((Where::Any, s)),
        None => None,
    };
    let here = |name: String| Target { name, machine: None, root: root.to_path_buf() };
    let there = |row: SandboxEntry| -> Result<Target> {
        let root = row.root.context("bad sandboxes.list entry from devsandbox serve: no root")?;
        Ok(Target { name: row.name, machine: row.machine, root })
    };
    let target = match choose_sandbox(rows, wanted) {
        SandboxChoice::Example { unmatched } => {
            if let Some(name) = unmatched.filter(|_| fed.machines) {
                note_unreachable(&name, call);
            }
            (ui.offer_example)()?;
            // As in-process `run` returning `None`: no instance to report.
            if json {
                bail!("no instance created");
            }
            return Ok(None);
        }
        SandboxChoice::Here { sandbox, matched } => {
            if !matched && fed.machines {
                note_unreachable(&sandbox, call);
            }
            here(sandbox)
        }
        SandboxChoice::There(row) => there(row)?,
        SandboxChoice::Missing { machine, sandbox } => {
            bail!("no sandbox `{sandbox}` on machine `{machine}`{}", unreachable_suffix(&unreachable(call), Some(&machine)))
        }
        SandboxChoice::Pick { prompt, error, mut options } => {
            if !ui.tty {
                bail!("{error}");
            }
            let labels: Vec<String> = options.iter().map(|(label, _)| label.clone()).collect();
            let at = (ui.pick)(&prompt, &labels)?;
            if at >= options.len() {
                bail!("selection out of range");
            }
            match options.swap_remove(at).1 {
                row if row.machine.is_none() => here(row.name),
                row => there(row)?,
            }
        }
    };
    Ok(Some(Ok(target)))
}

/// What `run` does with a sandbox name and the sandboxes listed (F12).
#[derive(Debug, PartialEq)]
enum SandboxChoice {
    /// Nothing defined here, nothing matched elsewhere: the example config.
    /// `unmatched`: the name asked for, if one was.
    Example { unmatched: Option<String> },
    /// Run it here; `matched`: this root defines it (else the local `run`'s
    /// own error comes back, as before machines).
    Here { sandbox: String, matched: bool },
    /// Run it on its machine.
    There(SandboxEntry),
    /// A qualified `<machine>/<sandbox>` that machine doesn't define.
    Missing { machine: String, sandbox: String },
    /// Several, or none asked for: a pick on a TTY, else `error`.
    Pick { prompt: String, error: String, options: Vec<(String, SandboxEntry)> },
}

fn choose_sandbox(rows: Vec<SandboxEntry>, wanted: Option<(Where, String)>) -> SandboxChoice {
    let local_empty = !rows.iter().any(|r| r.machine.is_none());
    let Some((wherever, name)) = wanted else {
        if rows.is_empty() {
            return SandboxChoice::Example { unmatched: None };
        }
        let labels: Vec<String> = rows.iter().map(|r| sandbox_label(r, &rows, false)).collect();
        let error = format!("no sandbox specified; available: {}", labels.join(", "));
        let options = labels.into_iter().zip(rows).collect();
        return SandboxChoice::Pick { prompt: "Select a sandbox".into(), error, options };
    };
    let fits = |r: &SandboxEntry| {
        r.name == name
            && match &wherever {
                Where::Any => true,
                Where::Local => r.machine.is_none(),
                Where::Machine(m) => r.machine.as_ref() == Some(m),
            }
    };
    let mut candidates: Vec<SandboxEntry> = rows.iter().filter(|r| fits(r)).cloned().collect();
    match candidates.len() {
        0 => match wherever {
            Where::Machine(machine) => SandboxChoice::Missing { machine, sandbox: name },
            _ if local_empty => SandboxChoice::Example { unmatched: Some(name) },
            _ => SandboxChoice::Here { sandbox: name, matched: false },
        },
        1 => match candidates.pop().expect("one") {
            row if row.machine.is_none() => SandboxChoice::Here { sandbox: row.name, matched: true },
            row => SandboxChoice::There(row),
        },
        _ => {
            let labels: Vec<String> = candidates.iter().map(|r| sandbox_label(r, &candidates, true)).collect();
            let error = format!("`{name}` is defined on several machines: {}; qualify it", labels.join(", "));
            let prompt = format!("`{name}` is defined on several machines");
            SandboxChoice::Pick { prompt, error, options: labels.into_iter().zip(candidates).collect() }
        }
    }
}

/// A sandbox as the pick shows it: a machine's qualified, this host's plain
/// (or `@/` among others of the same name), and the root when the machine
/// defines the name in two of them.
fn sandbox_label(row: &SandboxEntry, rows: &[SandboxEntry], local_qualified: bool) -> String {
    let base = match &row.machine {
        Some(m) => format!("{m}/{}", row.name),
        None if local_qualified => format!("@/{}", row.name),
        None => row.name.clone(),
    };
    let twice = rows.iter().filter(|r| r.machine == row.machine && r.name == row.name).count() > 1;
    match (&row.root, twice) {
        (Some(root), true) => format!("{base} ({})", root.display()),
        _ => base,
    }
}

/// The machines not reached (`machines.list`), as `name (why)`: a federated
/// listing adds nothing for them, so a name nothing matched may be there.
fn unreachable(call: &mut Call) -> Vec<String> {
    let Ok(Reply::Ok(rows)) = call("machines.list", json!({})).map(reply) else { return Vec::new() };
    let rows = rows.as_array().cloned().unwrap_or_default();
    rows.iter()
        .filter(|r| r["state"] == "unreachable")
        .filter_map(|r| {
            let name = r["name"].as_str()?;
            Some(match r["error"].as_str() {
                Some(why) => format!("{name} ({why})"),
                None => name.to_string(),
            })
        })
        .collect()
}

/// `; unreachable: …`, only `machine`'s when given; empty when none.
fn unreachable_suffix(items: &[String], machine: Option<&str>) -> String {
    let items: Vec<&String> =
        items.iter().filter(|i| machine.is_none_or(|m| i.as_str() == m || i.starts_with(&format!("{m} (")))).collect();
    match items.is_empty() {
        true => String::new(),
        false => format!("; unreachable: {}", items.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

fn note_unreachable(sandbox: &str, call: &mut Call) {
    let suffix = unreachable_suffix(&unreachable(call), None);
    if let Some(list) = suffix.strip_prefix("; ") {
        eprintln!("note: no machine reached defines `{sandbox}`; {list}");
    }
}

// ---- the streamed request -------------------------------------------------

/// A verb request's answer, sorted.
#[derive(Debug, PartialEq)]
enum Answer {
    /// Exit 0, or a `detached` answer.
    Ok(Value),
    /// `failed`: the verb's exit code (its stderr already streamed).
    Failed(i32),
    /// `confirm`: rm's branch question.
    Confirm(String),
    /// Another error: its message, printed as ours.
    Error(String),
    Old(String),
    /// We hung up (Ctrl-C before anything could be cancelled by name).
    HungUp,
    /// The connection ended first: the verb runs on without us (F10).
    Lost,
}

impl Answer {
    fn of(streamed: Streamed) -> Answer {
        let r = match streamed {
            Streamed::Answer(r) => r,
            Streamed::HungUp => return Answer::HungUp,
            Streamed::Lost => return Answer::Lost,
        };
        match reply(r) {
            Reply::Ok(v) => Answer::Ok(v),
            Reply::Old(why) => Answer::Old(why),
            Reply::Err(e) if e.code == "failed" => {
                let exit = e.data.as_ref().and_then(|d| d["exit"].as_i64()).and_then(|n| i32::try_from(n).ok());
                // A non-zero exit is what `failed` means; never report 0.
                Answer::Failed(exit.filter(|&n| n != 0).unwrap_or(1))
            }
            Reply::Err(e) if e.code == "confirm" => Answer::Confirm(e.message),
            Reply::Err(e) => Answer::Error(e.message),
        }
    }
}

/// rm's question loop (F5): an `ask` that comes back `confirm` is put to the
/// user (`confirm`), then resent once with the answer as `deleteBranch`.
/// Any other answer, of either send, is the result.
fn with_confirm(
    params: Value,
    send: &mut dyn FnMut(Value) -> Result<Answer>,
    confirm: &mut dyn FnMut(&str) -> Result<bool>,
) -> Result<Answer> {
    let answer = send(params.clone())?;
    let Answer::Confirm(question) = answer else { return Ok(answer) };
    let mut params = params;
    params["deleteBranch"] = confirm(&question)?.into();
    params["ask"] = false.into();
    match send(params)? {
        Answer::Confirm(question) => bail!("devsandbox serve asked again: {question}"),
        answer => Ok(answer),
    }
}

/// What one streamed request learnt besides its answer.
#[derive(Debug, Clone)]
struct Streaming {
    /// The instance a Ctrl-C cancels, and a lost request follows.
    instance: Option<String>,
    cancelled: bool,
    /// `run`: the instance comes from `progress`.
    track_progress: bool,
    /// `-d` (and rm's `ask` round): output held back as `(stdout?, data)`
    /// instead of printed, so stdout is the name alone (`NAME=$(devsandbox
    /// run -d web)`). Dropped on the detached answer (the build log has it)
    /// or a `confirm` (a resend follows), replayed otherwise.
    held: Option<Vec<(bool, String)>>,
    /// A remote `run`'s machine: the instance name it prints on stdout is
    /// printed qualified (`m/web-2`), so it can be pasted into the next
    /// command. Its stdout goes out a line late for that: the name is the
    /// last line, known for sure only at the answer.
    qualify: Option<String>,
    /// That stdout's line in progress, and the last whole line, unprinted.
    partial: String,
    pending: Option<String>,
}

impl Streaming {
    fn new(instance: Option<String>, track_progress: bool, held: Option<Vec<(bool, String)>>) -> Self {
        Self { instance, cancelled: false, track_progress, held, qualify: None, partial: String::new(), pending: None }
    }
}

/// Send one verb request on `conn` and stream it to its answer, with
/// Ctrl-C caught meanwhile (module doc).
fn stream(
    conn: &mut Conn,
    verb: &CliVerb,
    method: &str,
    params: Value,
    instance: Option<String>,
    machine: Option<String>,
) -> Result<(Answer, Streaming)> {
    let track_progress = matches!(verb, CliVerb::Run { .. });
    // rm's `ask` round too: on `confirm` its output is just the question,
    // as an `Error:` line, which the prompt asks properly.
    let held = (verb.detach() || params["ask"] == true).then(Vec::new);
    let mut s = Streaming::new(instance, track_progress, held);
    // `run --json`'s stdout is the record, unchanged.
    s.qualify = machine.clone().filter(|_| matches!(verb, CliVerb::Run { json: false, .. }));
    let _guard = cancel::catch();
    let streamed = conn.call_streaming(method, params, TICK, &mut |note| {
        if let Some(note) = note {
            s.observe(note);
        }
        if cancel::requested() && !s.cancelled {
            s.cancelled = true;
            return match &s.instance {
                Some(instance) => cancel_by_name(instance, machine.as_deref()),
                None => false,
            };
        }
        true
    })?;
    Ok((Answer::of(streamed), s))
}

impl Streaming {
    /// Print `output` at once; note the instance `progress` names.
    fn observe(&mut self, note: &Notification) {
        match note.method.as_str() {
            "output" => {
                let data = note.params["data"].as_str().unwrap_or_default();
                let stdout = note.params["stream"] == "stdout";
                match &mut self.held {
                    Some(held) => held.push((stdout, data.to_string())),
                    None if stdout && self.qualify.is_some() => self.stdout_lines(data),
                    None => print_output(stdout, data),
                }
            }
            "progress" if self.track_progress && self.instance.is_none() => {
                self.instance = note.params["instance"].as_str().map(str::to_string);
            }
            _ => {}
        }
    }
}

impl Streaming {
    /// A remote run's stdout, a line late (`qualify`).
    fn stdout_lines(&mut self, data: &str) {
        self.partial.push_str(data);
        while let Some(at) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=at).collect();
            if let Some(before) = self.pending.replace(line) {
                print_output(true, &before);
            }
        }
    }

    /// The end of a remote run's stdout: the last line, qualified if it's
    /// the instance's name, and what followed it.
    fn finish_stdout(&mut self) {
        let last = self.pending.take().map(|line| self.qualified(line));
        let rest = std::mem::take(&mut self.partial);
        print_output(true, &(last.unwrap_or_default() + &rest));
    }

    fn qualified(&self, line: String) -> String {
        match (&self.qualify, &self.instance) {
            (Some(m), Some(instance)) if line.trim_end_matches(['\n', '\r']) == instance => format!("{m}/{line}"),
            _ => line,
        }
    }

    /// Print what `-d` held back: the verb didn't detach (it failed first).
    fn replay(&mut self) {
        for (stdout, data) in self.held.take().unwrap_or_default() {
            print_output(stdout, &data);
        }
    }
}

fn print_output(stdout: bool, data: &str) {
    // A closed pipe (`| head`) loses output, not the verb.
    if stdout {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(data.as_bytes()).and_then(|()| out.flush());
    } else {
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(data.as_bytes()).and_then(|()| err.flush());
    }
}

/// The first Ctrl-C with a name: `instances.cancel` on a second connection
/// (this one is busy streaming), so the rollback still shows here. `false`
/// (hang up instead) when that can't be done: no daemon answering, or the
/// instance isn't building (`start`, `stop`, `rm` record no builder; the
/// hang-up SIGINTs their process group just the same).
fn cancel_by_name(instance: &str, machine: Option<&str>) -> bool {
    let Ok(socket) = endpoint::socket_dir() else { return false };
    let conn = client::connect_running(&socket, "cli", &Version::current(), client::START_TIMEOUT);
    let Ok(Some(mut conn)) = conn else { return false };
    match conn.call("instances.cancel", routed(json!({ "instance": instance }), machine)) {
        Ok(Response { error: None, .. }) => {
            eprintln!("cancelling {instance}…");
            true
        }
        _ => false,
    }
}

// ---- the end --------------------------------------------------------------

/// What the CLI does with a verb's final answer.
#[derive(Debug, PartialEq)]
enum Ending {
    Exit(i32),
    /// A `detached` answer: print the instance (or `run --json`'s record).
    Detached(Value),
    /// The daemon went away mid-verb (F10).
    Lost,
    Old(String),
}

/// The exit code rules (F9): success 0, `failed` its exit, 130 after a
/// Ctrl-C whatever the verb then answered, other errors ours (exit 1).
fn ending(answer: Answer, cancelled: bool) -> Result<Ending> {
    Ok(match answer {
        Answer::Old(why) => Ending::Old(why),
        Answer::HungUp => Ending::Exit(CANCELLED_EXIT),
        Answer::Error(message) => bail!("{message}"),
        Answer::Confirm(question) => bail!("devsandbox serve asked, unasked: {question}"),
        _ if cancelled => Ending::Exit(CANCELLED_EXIT),
        Answer::Lost => Ending::Lost,
        Answer::Failed(exit) => Ending::Exit(exit),
        Answer::Ok(v) if v["detached"] == true => Ending::Detached(v),
        Answer::Ok(_) => Ending::Exit(0),
    })
}

/// `-d`'s output: the instance name, nothing when there's none (`--all`);
/// `run -d --json` the run record, enveloped as `run --json` prints it. A
/// machine's instance is named qualified (`m/web-2`).
fn print_detached(verb: &CliVerb, result: &Value, machine: Option<&str>) -> Result<()> {
    let instance = result["instance"].as_str();
    if let CliVerb::Run { json: true, .. } = verb {
        let doc = match (result.get("record"), instance, machine) {
            (Some(record), _, _) => serde_json::to_string_pretty(&crate::commands::status::Envelope::new(record))?,
            (None, Some(key), None) => crate::commands::run::run_record_json(key)?,
            (None, Some(_), Some(m)) => bail!("{m} sent no run record (its devsandbox is older)"),
            (None, None, _) => bail!("no instance created"),
        };
        println!("{doc}");
    } else if let Some(instance) = instance {
        match machine {
            Some(m) => println!("{m}/{instance}"),
            None => println!("{instance}"),
        }
    }
    Ok(())
}

/// The daemon went away before the answer (a handoff, a crash): the verb
/// runs on, orphaned. A build with a known instance is followed through its
/// build log, then exits by its phase; anything else can't be watched.
/// A machine's verb can't be followed from here: its build log is there.
fn lost(verb: &CliVerb, instance: Option<&str>, machine: Option<&str>) -> Result<i32> {
    if let Some(m) = machine {
        eprintln!(
            "the connection to devsandbox serve was lost; `devsandbox {}` may still be running on {m}",
            verb.command()
        );
        return Ok(1);
    }
    eprintln!("devsandbox serve went away; `devsandbox {}` keeps running in the background", verb.command());
    let Some(instance) = instance.filter(|_| verb.builds()) else { return Ok(1) };
    // `run --json`'s stdout is for the document: follow quietly.
    if !matches!(verb, CliVerb::Run { json: true, .. })
        && let Err(e) = crate::commands::logs::build_log(instance, true)
    {
        eprintln!("warning: {e:#}");
    }
    loop {
        let state = crate::state::State::load()?;
        if let Some(code) = settled(state.instances.get(instance).map(|i| i.phase_view())) {
            return Ok(code);
        }
        std::thread::sleep(SETTLE_POLL);
    }
}

/// A lost verb's exit by its instance's phase: `None` while still in flight;
/// 0 once clear; 1 failed, or gone (a cancelled `run` rolled back).
fn settled(view: Option<Option<PhaseView>>) -> Option<i32> {
    match view {
        Some(None) => Some(0),
        Some(Some(PhaseView::InFlight(_))) => None,
        Some(Some(PhaseView::Failed { .. })) | None => Some(1),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use clap::Parser;

    use super::*;
    use crate::Cli;
    use crate::serve::proto::ErrorBody;

    fn verb(args: &[&str]) -> Option<CliVerb> {
        let cli = Cli::try_parse_from(std::iter::once("devsandbox").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{e}"));
        from_command(&cli.command.unwrap())
    }

    #[test]
    fn verbs_from_the_parsed_cli() {
        assert_eq!(
            verb(&["run", "web", "--name", "w2", "--branch", "b", "--base", "origin/x", "--json", "-d"]),
            Some(CliVerb::Run {
                sandbox: Some("web".into()),
                name: Some("w2".into()),
                branch: Some("b".into()),
                base: Some("origin/x".into()),
                json: true,
                detach: true,
            })
        );
        assert_eq!(
            verb(&["run"]),
            Some(CliVerb::Run { sandbox: None, name: None, branch: None, base: None, json: false, detach: false })
        );
        assert_eq!(verb(&["rebuild", "w", "--force"]), Some(CliVerb::Rebuild { target: Some("w".into()), force: true, detach: false }));
        assert_eq!(verb(&["recreate", "--all", "--detach"]), Some(CliVerb::Rebuild { target: None, force: false, detach: true }));
        assert_eq!(verb(&["start", "-d", "w"]), Some(CliVerb::Start { target: Some("w".into()), detach: true }));
        assert_eq!(verb(&["stop", "--all"]), Some(CliVerb::Stop { target: None }));
        assert_eq!(verb(&["rm", "w"]), Some(CliVerb::Rm { name: "w".into(), delete_branch: None, force: false }));
        assert_eq!(
            verb(&["rm", "-f", "--keep-branch", "w"]),
            Some(CliVerb::Rm { name: "w".into(), delete_branch: Some(false), force: true })
        );
        assert_eq!(verb(&["service", "rebuild", "db"]), Some(CliVerb::ServicesRebuild { name: "db".into() }));
        // `-d` is only for the verbs that build.
        assert!(Cli::try_parse_from(["devsandbox", "service", "rebuild", "-d", "db"]).is_err());
        assert!(Cli::try_parse_from(["devsandbox", "stop", "-d", "w"]).is_err());
        assert!(Cli::try_parse_from(["devsandbox", "rm", "-d", "w"]).is_err());
    }

    /// What stays in this process: the hidden dispatcher flags, rm's
    /// `--fail-on-ask`, every other command. (`--in-process` is `main`'s
    /// filter on top.)
    #[test]
    fn in_process_verbs() {
        assert_eq!(verb(&["run", "web", "--env", "A=1"]), None);
        assert_eq!(verb(&["run", "web", "--dispatcher", "id"]), None);
        assert_eq!(verb(&["rm", "--fail-on-ask", "w"]), None);
        assert_eq!(verb(&["ps"]), None);
        assert_eq!(verb(&["logs", "w"]), None);
        assert_eq!(verb(&["service", "ls"]), None);
        assert_eq!(verb(&["gc"]), None);
        let cli = Cli::try_parse_from(["devsandbox", "--in-process", "stop", "w"]).unwrap();
        assert!(cli.in_process);
    }

    #[test]
    fn caller_env_skips_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0xff]);
        let env = caller_env([
            ("PATH".into(), "/bin".into()),
            (bad.clone(), "x".into()),
            ("HOME".into(), bad),
        ]);
        assert_eq!(env, BTreeMap::from([("PATH".to_string(), "/bin".to_string())]));
    }

    fn ok(v: Value) -> Response {
        Response::ok(Some(1), v)
    }

    fn err(code: &str, message: &str, data: Option<Value>) -> Response {
        Response { id: Some(1), result: None, error: Some(ErrorBody { code: code.into(), message: message.into(), data }) }
    }

    #[test]
    fn answers_map_to_exit_codes() {
        let end = |r: Response, cancelled| ending(Answer::of(Streamed::Answer(r)), cancelled);
        assert_eq!(end(ok(json!({"instance": "w", "exit": 0})), false).unwrap(), Ending::Exit(0));
        let detached = json!({"instance": "w", "detached": true});
        assert_eq!(end(ok(detached.clone()), false).unwrap(), Ending::Detached(detached));
        let failed = err("failed", "boom", Some(json!({"exit": 7, "instance": "w"})));
        assert_eq!(end(failed, false).unwrap(), Ending::Exit(7));
        // A `failed` without usable data still fails.
        assert_eq!(end(err("failed", "boom", None), false).unwrap(), Ending::Exit(1));
        let e = end(err("not-found", "no sandbox instance matches `x`", None), false).unwrap_err();
        assert_eq!(e.to_string(), "no sandbox instance matches `x`");
        assert!(matches!(end(err("unknown-method", "no method `rm`", None), false).unwrap(), Ending::Old(_)));
        // After a Ctrl-C: 130, whatever the verb answered (its rollback's
        // failure included).
        let failed = err("failed", "cancelled", Some(json!({"exit": 1, "instance": "w"})));
        assert_eq!(end(failed, true).unwrap(), Ending::Exit(CANCELLED_EXIT));
        assert_eq!(ending(Answer::HungUp, true).unwrap(), Ending::Exit(CANCELLED_EXIT));
        assert_eq!(ending(Answer::Lost, false).unwrap(), Ending::Lost);
        assert_eq!(ending(Answer::Lost, true).unwrap(), Ending::Exit(CANCELLED_EXIT));
    }

    #[test]
    fn rm_asks_once_on_confirm_and_resends_with_the_answer() {
        let sent = RefCell::new(Vec::new());
        let mut send = |p: Value| {
            sent.borrow_mut().push(p.clone());
            Ok(if p["ask"] == true { Answer::Confirm("Delete branch `b`?".into()) } else { Answer::Ok(json!({"exit": 0})) })
        };
        let asked = RefCell::new(Vec::new());
        let mut confirm = |q: &str| {
            asked.borrow_mut().push(q.to_string());
            Ok(true)
        };
        let params = json!({"instance": "w", "ask": true});
        assert_eq!(with_confirm(params, &mut send, &mut confirm).unwrap(), Answer::Ok(json!({"exit": 0})));
        assert_eq!(*asked.borrow(), ["Delete branch `b`?"]);
        let sent = sent.into_inner();
        assert_eq!(sent.len(), 2);
        assert_eq!((&sent[1]["deleteBranch"], &sent[1]["ask"]), (&json!(true), &json!(false)));

        // No question: one send, nothing asked.
        let mut once = |_: Value| Ok(Answer::Failed(2));
        let mut never = |_: &str| -> Result<bool> { panic!("asked") };
        assert_eq!(with_confirm(json!({}), &mut once, &mut never).unwrap(), Answer::Failed(2));
        // A daemon asking twice is an error, not a loop.
        let mut again = |_: Value| Ok(Answer::Confirm("q".into()));
        let mut no = |_: &str| Ok(false);
        assert!(with_confirm(json!({"ask": true}), &mut again, &mut no).is_err());
    }

    /// A fake daemon for [`prepare`]: canned answers by method, calls logged.
    fn prep(verb: &CliVerb, tty: bool, answers: &[(&str, Response)], pick: usize) -> (Result<Prep>, Vec<String>, bool) {
        prep_on(verb, tty, Some(TERM), answers, pick)
    }

    const TERM: TtySize = TtySize { cols: 100, rows: 30 };

    fn prep_on(
        verb: &CliVerb,
        tty: bool,
        term: Option<TtySize>,
        answers: &[(&str, Response)],
        pick: usize,
    ) -> (Result<Prep>, Vec<String>, bool) {
        let (out, calls, offered, _) = prep_fed(verb, &Fed::default(), tty, term, answers, pick);
        (out, calls, offered)
    }

    /// [`prep_on`] with machines: also the options each pick offered.
    fn prep_fed(
        verb: &CliVerb,
        fed: &Fed,
        tty: bool,
        term: Option<TtySize>,
        answers: &[(&str, Response)],
        pick: usize,
    ) -> (Result<Prep>, Vec<String>, bool, Vec<Vec<String>>) {
        let calls = RefCell::new(Vec::new());
        let picks = RefCell::new(Vec::new());
        let mut call = |method: &str, params: Value| {
            calls.borrow_mut().push(format!("{method} {params}"));
            // `"method"`, or `"method <text>"` for a call whose params hold `<text>`.
            let fits = |key: &str| match key.split_once(' ') {
                Some((m, text)) => m == method && params.to_string().contains(text),
                None => key == method,
            };
            let r = answers.iter().find(|(m, _)| fits(m)).map(|(_, r)| r).unwrap_or_else(|| panic!("unexpected {method} {params}"));
            Ok(Response { id: r.id, result: r.result.clone(), error: r.error.clone() })
        };
        let offered = RefCell::new(false);
        let mut ui = Ui {
            tty,
            term,
            pick: &mut |_, options| {
                assert!(options.len() > 1);
                picks.borrow_mut().push(options.to_vec());
                Ok(pick)
            },
            offer_example: &mut || {
                *offered.borrow_mut() = true;
                Ok(())
            },
        };
        let env = BTreeMap::from([("A".to_string(), "1".to_string())]);
        let out = prepare(verb, Path::new("/cfg"), env, fed, &mut call, &mut ui);
        (out, calls.into_inner(), offered.into_inner(), picks.into_inner())
    }

    fn sent(p: Result<Prep>) -> Prepared {
        match p.unwrap() {
            Prep::Send(p) => p,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn names_resolve_and_pick_before_sending() {
        let stop = CliVerb::Stop { target: Some("web".into()) };
        let one = [("instances.resolve", ok(json!({"matches": ["web-2"]})))];
        let (p, calls, _) = prep(&stop, false, &one, 0);
        let p = sent(p);
        assert_eq!(calls, [r#"instances.resolve {"name":"web"}"#]);
        assert_eq!((p.method, p.instance.as_deref()), ("stop", Some("web-2")));
        assert_eq!(p.params, json!({"root": "/cfg", "env": {"A": "1"}, "instance": "web-2", "all": false, "detach": false}));

        let two = [("instances.resolve", ok(json!({"matches": ["web", "web-2"]})))];
        let e = prep(&stop, false, &two, 0).0.unwrap_err();
        assert_eq!(e.to_string(), "`web` is ambiguous: web, web-2");
        assert_eq!(sent(prep(&stop, true, &two, 1).0).instance.as_deref(), Some("web-2"));

        let none = [("instances.resolve", err("not-found", "no sandbox instance matches `web` (see `devsandbox ps -a`)", None))];
        let e = prep(&stop, true, &none, 0).0.unwrap_err();
        assert_eq!(e.to_string(), crate::commands::no_instance_matches("web"));

        let old = [("instances.resolve", err("unknown-method", "unknown method", None))];
        assert!(matches!(prep(&stop, true, &old, 0).0.unwrap(), Prep::Old(_)));

        // `--all` resolves nothing.
        let (p, calls, _) = prep(&CliVerb::Rebuild { target: None, force: true, detach: true }, true, &[], 0);
        assert!(calls.is_empty());
        let p = sent(p);
        assert_eq!(p.instance, None);
        assert_eq!((&p.params["all"], &p.params["force"], &p.params["detach"]), (&json!(true), &json!(true), &json!(true)));
    }

    #[test]
    fn service_rebuild_sends_the_name_unresolved() {
        let (p, calls, _) = prep(&CliVerb::ServicesRebuild { name: "db".into() }, true, &[], 0);
        assert!(calls.is_empty(), "no prompt, no resolve");
        let p = sent(p);
        assert_eq!((p.method, p.instance), ("services.rebuild", None));
        assert_eq!(p.params, json!({"root": "/cfg", "env": {"A": "1"}, "name": "db", "detach": false}));
    }

    #[test]
    fn rm_asks_only_on_a_tty_without_a_branch_flag() {
        let one = [("instances.resolve", ok(json!({"matches": ["w"]})))];
        let rm = |delete_branch| CliVerb::Rm { name: "w".into(), delete_branch, force: false };
        let params = |verb, tty| sent(prep(&verb, tty, &one, 0).0).params;
        assert_eq!(params(rm(None), true)["ask"], true);
        assert_eq!(params(rm(None), false)["ask"], false);
        assert_eq!(params(rm(None), false).get("deleteBranch"), Some(&Value::Null));
        let keep = params(rm(Some(false)), true);
        assert_eq!((&keep["ask"], &keep["deleteBranch"]), (&json!(false), &json!(false)));
    }

    #[test]
    fn run_lists_sandboxes_picks_and_offers_the_example() {
        let run = |sandbox: Option<&str>, json| CliVerb::Run {
            sandbox: sandbox.map(str::to_string),
            name: None,
            branch: None,
            base: None,
            json,
            detach: false,
        };
        let two = [("sandboxes.list", ok(json!([{"name": "api"}, {"name": "web"}])))];
        let p = sent(prep(&run(None, false), true, &two, 1).0);
        assert_eq!((p.method, &p.params["sandbox"]), ("run", &json!("web")));
        let e = prep(&run(None, false), false, &two, 0).0.unwrap_err();
        assert_eq!(e.to_string(), "no sandbox specified; available: api, web");
        assert_eq!(sent(prep(&run(Some("api"), false), false, &two, 0).0).params["sandbox"], "api");

        let empty = [("sandboxes.list", ok(json!([])))];
        let (p, _, offered) = prep(&run(Some("web"), false), true, &empty, 0);
        assert!(offered);
        assert!(matches!(p.unwrap(), Prep::Exit(0)));
        let (p, _, offered) = prep(&run(None, true), true, &empty, 0);
        assert!(offered);
        assert_eq!(p.unwrap_err().to_string(), "no instance created");

        let bad = [("sandboxes.list", err("invalid", "bad config: x", None))];
        assert_eq!(prep(&run(None, false), true, &bad, 0).0.unwrap_err().to_string(), "bad config: x");
    }

    /// The build verbs carry the terminal's size, but not `run --json`
    /// (its stdout is the record) and not off a terminal.
    #[test]
    fn build_verbs_send_the_terminal_size() {
        let run = |json| CliVerb::Run { sandbox: Some("web".into()), name: None, branch: None, base: None, json, detach: false };
        let rebuild = CliVerb::Rebuild { target: None, force: false, detach: false };
        let start = CliVerb::Start { target: None, detach: true };
        let list = [("sandboxes.list", ok(json!([{"name": "web"}])))];
        let size = json!({"cols": 100, "rows": 30});
        for (verb, tty) in [(run(false), &size), (run(true), &Value::Null), (rebuild.clone(), &size), (start.clone(), &size)] {
            let p = sent(prep(&verb, true, &list, 0).0);
            assert_eq!(&p.params["tty"], tty, "{verb:?}");
            let p = sent(prep_on(&verb, true, None, &list, 0).0);
            assert_eq!(p.params["tty"], Value::Null, "{verb:?} off a terminal");
        }
    }

    #[test]
    fn progress_names_a_runs_instance_once() {
        let note = |instance: &str| Notification {
            method: "progress".into(),
            params: json!({"request": 1, "instance": instance, "phase": "building"}),
        };
        let mut s = Streaming::new(None, true, None);
        s.observe(&note("web-2"));
        s.observe(&note("other"));
        assert_eq!(s.instance.as_deref(), Some("web-2"));
        // Not a run: the resolved name stands.
        let mut s = Streaming::new(None, false, None);
        s.observe(&note("web-2"));
        assert_eq!(s.instance, None);
    }

    #[test]
    fn detach_holds_output_back() {
        let out = |stream: &str, data: &str| Notification {
            method: "output".into(),
            params: json!({"request": 1, "stream": stream, "data": data}),
        };
        let mut s = Streaming::new(None, true, Some(Vec::new()));
        s.observe(&out("stdout", "3c66ab93\n"));
        s.observe(&out("stderr", "Preparing worktree\n"));
        assert_eq!(
            s.held.as_deref(),
            Some(&[(true, "3c66ab93\n".to_string()), (false, "Preparing worktree\n".to_string())][..])
        );
    }

    #[test]
    fn a_lost_verb_settles_by_phase() {
        assert_eq!(settled(Some(None)), Some(0));
        assert_eq!(settled(Some(Some(PhaseView::InFlight("building".into())))), None);
        assert_eq!(settled(Some(Some(PhaseView::Failed { at: "building".into(), error: "x".into() }))), Some(1));
        assert_eq!(settled(None), Some(1));
    }

    // ---- machines -------------------------------------------------------------

    const FED: Fed = Fed { machines: true, machine: None };

    fn on(m: &str) -> Fed {
        Fed { machines: true, machine: Some(m.into()) }
    }

    fn run_of(sandbox: Option<&str>) -> CliVerb {
        CliVerb::Run { sandbox: sandbox.map(str::to_string), name: None, branch: None, base: None, json: false, detach: false }
    }

    fn row(name: &str, machine: Option<&str>, root: &str) -> SandboxEntry {
        SandboxEntry { name: name.into(), machine: machine.map(Into::into), root: Some(root.into()) }
    }

    #[test]
    fn run_chooses_where_the_sandbox_is() {
        let rows = || vec![row("api", None, "/cfg"), row("web", None, "/cfg"), row("web", Some("a"), "/r"), row("db", Some("a"), "/r")];
        let pick = |w: Where, n: &str| choose_sandbox(rows(), Some((w, n.into())));
        assert_eq!(pick(Where::Any, "api"), SandboxChoice::Here { sandbox: "api".into(), matched: true });
        assert_eq!(pick(Where::Any, "db"), SandboxChoice::There(row("db", Some("a"), "/r")));
        assert_eq!(pick(Where::Machine("a".into()), "web"), SandboxChoice::There(row("web", Some("a"), "/r")));
        assert_eq!(pick(Where::Local, "web"), SandboxChoice::Here { sandbox: "web".into(), matched: true });
        let SandboxChoice::Pick { prompt, error, options } = pick(Where::Any, "web") else { panic!() };
        assert_eq!(prompt, "`web` is defined on several machines");
        assert_eq!(error, "`web` is defined on several machines: @/web, a/web; qualify it");
        assert_eq!(options.iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["@/web", "a/web"]);
        // Nothing matching: here, so the local `run` says so as before.
        assert_eq!(pick(Where::Any, "zzz"), SandboxChoice::Here { sandbox: "zzz".into(), matched: false });
        assert_eq!(pick(Where::Local, "db"), SandboxChoice::Here { sandbox: "db".into(), matched: false });
        assert_eq!(pick(Where::Machine("a".into()), "api"), SandboxChoice::Missing { machine: "a".into(), sandbox: "api".into() });
        // None asked: all of them, a machine's qualified.
        let SandboxChoice::Pick { error, .. } = choose_sandbox(rows(), None) else { panic!() };
        assert_eq!(error, "no sandbox specified; available: api, web, a/web, a/db");
        // One machine defining it in two roots: the roots tell them apart.
        let two = vec![row("web", Some("a"), "/r1"), row("web", Some("a"), "/r2")];
        let SandboxChoice::Pick { options, .. } = choose_sandbox(two, Some((Where::Any, "web".into()))) else { panic!() };
        assert_eq!(options.iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["a/web (/r1)", "a/web (/r2)"]);
        // No local config: the example, unless a machine has it.
        let remote_only = || vec![row("web", Some("a"), "/r")];
        assert_eq!(choose_sandbox(remote_only(), Some((Where::Any, "web".into()))), SandboxChoice::There(row("web", Some("a"), "/r")));
        assert_eq!(choose_sandbox(remote_only(), Some((Where::Any, "x".into()))), SandboxChoice::Example { unmatched: Some("x".into()) });
        assert_eq!(choose_sandbox(vec![], None), SandboxChoice::Example { unmatched: None });
    }

    #[test]
    fn run_on_a_machine_sends_its_root_and_machine() {
        let list = [("sandboxes.list", ok(json!([{"name": "loc", "root": "/cfg"}, {"name": "web", "machine": "a", "root": "/r"}])))];
        let (p, calls, _, _) = prep_fed(&run_of(Some("web")), &FED, false, None, &list, 0);
        assert_eq!(calls, [r#"sandboxes.list {"machines":true,"root":"/cfg"}"#]);
        let p = sent(p);
        assert_eq!((p.method, p.machine.as_deref(), p.instance.as_deref()), ("run", Some("a"), None));
        assert_eq!((&p.params["root"], &p.params["sandbox"], &p.params["machine"]), (&json!("/r"), &json!("web"), &json!("a")));
        // Local: as before machines (no `machine`, `-C`'s root).
        let p = sent(prep_fed(&run_of(Some("@/loc")), &FED, false, None, &list, 0).0);
        assert_eq!((p.params.get("machine"), &p.params["root"], p.machine), (None, &json!("/cfg"), None));
        // A TTY pick without a sandbox lists both, a machine's qualified.
        let (p, _, _, picks) = prep_fed(&run_of(None), &FED, true, None, &list, 1);
        assert_eq!(picks, [vec!["loc".to_string(), "a/web".to_string()]]);
        assert_eq!(sent(p).params["machine"], "a");
        // `a/loc` isn't there: the error names what wasn't reached.
        let down = [
            ("sandboxes.list", ok(json!([{"name": "loc", "root": "/cfg"}]))),
            ("machines.list", ok(json!([{"name": "a", "state": "unreachable", "error": "ssh: no route"}, {"name": "b", "state": "connected"}]))),
        ];
        let e = prep_fed(&run_of(Some("a/loc")), &FED, false, None, &down, 0).0.unwrap_err();
        assert_eq!(e.to_string(), "no sandbox `loc` on machine `a`; unreachable: a (ssh: no route)");
        // Without machines: the request of before, byte for byte.
        let (_, calls, _, _) = prep_fed(&run_of(Some("loc")), &Fed::default(), false, None, &list, 0);
        assert_eq!(calls, [r#"sandboxes.list {"root":"/cfg"}"#]);
    }

    #[test]
    fn instance_verbs_resolve_across_machines() {
        let stop = |name: &str| CliVerb::Stop { target: Some(name.into()) };
        let both = [(
            "instances.resolve",
            ok(json!({"matches": ["a/web-2", "web-1"], "found": [
                {"name": "web-2", "machine": "a", "root": "/r"}, {"name": "web-1", "machine": null, "root": "/cfg"}]})),
        )];
        let e = prep_fed(&stop("web"), &FED, false, None, &both, 0).0.unwrap_err();
        assert_eq!(e.to_string(), "`web` is ambiguous: a/web-2, @/web-1");
        let (p, calls, _, picks) = prep_fed(&stop("web"), &FED, true, None, &both, 0);
        assert_eq!(calls, [r#"instances.resolve {"machines":true,"name":"web"}"#]);
        assert_eq!(picks, [vec!["a/web-2".to_string(), "@/web-1".to_string()]]);
        let p = sent(p);
        assert_eq!((p.instance.as_deref(), p.machine.as_deref()), (Some("web-2"), Some("a")));
        assert_eq!(p.params, json!({"root": "/r", "env": {"A": "1"}, "instance": "web-2", "all": false, "detach": false, "machine": "a"}));
        // The local one: today's request.
        let p = sent(prep_fed(&stop("web"), &FED, true, None, &both, 1).0);
        assert_eq!(p.params, json!({"root": "/cfg", "env": {"A": "1"}, "instance": "web-1", "all": false, "detach": false}));

        // Qualified: asked of that machine only; a root it didn't say is the
        // machine's first.
        let there = [
            ("instances.resolve", ok(json!({"matches": ["w"], "found": [{"name": "w", "machine": null, "root": null}]}))),
            ("machines.get", ok(json!({"name": "a", "target": "me@a", "command": null, "roots": ["/r1", "/r2"]}))),
        ];
        let (p, calls, _, _) = prep_fed(&CliVerb::Rm { name: "a/w".into(), delete_branch: None, force: true }, &FED, false, None, &there, 0);
        assert_eq!(calls, [r#"instances.resolve {"machine":"a","name":"w"}"#, r#"machines.get {"name":"a"}"#]);
        let p = sent(p);
        assert_eq!((&p.params["root"], &p.params["machine"], &p.params["instance"]), (&json!("/r1"), &json!("a"), &json!("w")));
        // `@/`: here only, plain.
        let here = [("instances.resolve", ok(json!({"matches": ["w"], "found": [{"name": "w", "machine": null, "root": "/x"}]})))];
        let (p, calls, _, _) = prep_fed(&stop("@/w"), &FED, false, None, &here, 0);
        assert_eq!(calls, [r#"instances.resolve {"name":"w"}"#]);
        assert_eq!(sent(p).params["root"], "/cfg");
    }

    #[test]
    fn all_verbs_with_a_machine_go_by_its_roots() {
        let machine = || ("machines.get", ok(json!({"name": "a", "target": "a", "command": null, "roots": ["/r1", "/r2"]})));
        let get = [machine()];
        // Every instance in the machine's state: once, at the first root.
        for verb in [CliVerb::Stop { target: None }, CliVerb::Start { target: None, detach: false }] {
            let p = sent(prep_fed(&verb, &on("a"), false, None, &get, 0).0);
            assert_eq!((&p.params["root"], &p.params["machine"], &p.params["all"]), (&json!("/r1"), &json!("a"), &json!(true)));
            assert_eq!((p.machine.as_deref(), p.banner), (Some("a"), None));
        }
        // `rebuild --all` resolves against the root's config: each root.
        let Prep::Each(each) = prep_fed(&CliVerb::Rebuild { target: None, force: false, detach: false }, &on("a"), false, None, &get, 0).0.unwrap()
        else {
            panic!()
        };
        let roots: Vec<_> = each.iter().map(|p| (p.params["root"].clone(), p.banner.clone().unwrap())).collect();
        assert_eq!(roots, [(json!("/r1"), "==> a:/r1".to_string()), (json!("/r2"), "==> a:/r2".to_string())]);
        assert!(each.iter().all(|p| p.params["machine"] == "a"));

        // `service rebuild`: the roots that define it.
        let svc = [
            machine(),
            ("services.get /r1", err("not-found", "no service `db` in /r1", None)),
            ("services.get /r2", ok(json!({"original": "image = \"x\""}))),
        ];
        let Prep::Each(each) = prep_fed(&CliVerb::ServicesRebuild { name: "db".into() }, &on("a"), false, None, &svc, 0).0.unwrap() else {
            panic!()
        };
        assert_eq!(each.len(), 1);
        assert_eq!(each[0].params, json!({"root": "/r2", "env": {"A": "1"}, "name": "db", "detach": false, "machine": "a"}));
        let none = [machine(), ("services.get", err("not-found", "no service", None))];
        let e = prep_fed(&CliVerb::ServicesRebuild { name: "db".into() }, &on("a"), false, None, &none, 0).0.unwrap_err();
        assert_eq!(e.to_string(), "no service `db` on machine `a` (roots: /r1, /r2)");
        let missing = [("machines.get", err("not-found", "no machine named `z`", None))];
        let e = prep_fed(&CliVerb::Stop { target: None }, &on("z"), false, None, &missing, 0).0.unwrap_err();
        assert_eq!(e.to_string(), "no machine named `z` (devsandbox machine ls)");
    }

    #[test]
    fn a_machine_verb_has_no_in_process_fallback() {
        let stop = |name: &str| CliVerb::Stop { target: Some(name.into()) };
        assert!(in_process(&stop("a/w"), &FED, "down".into()).unwrap_err().to_string().contains("needs devsandbox serve: down"));
        assert!(in_process(&CliVerb::Stop { target: None }, &on("a"), "down".into()).is_err());
        assert_eq!(in_process(&stop("w"), &FED, "down".into()).unwrap(), Done::InProcess("down".into()));
        assert_eq!(in_process(&stop("@/w"), &FED, "down".into()).unwrap(), Done::InProcess("down".into()));
    }

    #[test]
    fn a_remote_runs_name_is_printed_qualified() {
        let mut s = Streaming::new(None, true, None);
        s.qualify = Some("a".into());
        s.stdout_lines("first\nweb");
        assert_eq!(s.pending.as_deref(), Some("first\n"));
        s.stdout_lines("-2\n");
        assert_eq!(s.pending.as_deref(), Some("web-2\n"), "the last line waits");
        s.instance = Some("web-2".into());
        assert_eq!(s.qualified("web-2\n".into()), "a/web-2\n");
        assert_eq!(s.qualified("web-2\r\n".into()), "a/web-2\r\n");
        assert_eq!(s.qualified("other\n".into()), "other\n");
        s.qualify = None;
        assert_eq!(s.qualified("web-2\n".into()), "web-2\n");
    }
}
