//! The CLI side of verbs as requests (docs/verbs-as-requests.md, step 3):
//! `run`, `rebuild`, `start`, `stop`, `rm` and `service rebuild` sent to the daemon
//! ([`super::verbs`] runs them) instead of run in this process, so a build
//! survives its terminal and runs with this shell's env either way.
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

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::client::{self, Conn, Streamed};
use super::endpoint;
use super::proto::{Notification, Response, Version};
use super::verbs::{RebuildParams, RmParams, RunParams, ServicesRebuildParams, StartParams, StopParams};
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
pub(crate) fn run(dir: &Path, verb: CliVerb) -> Result<Done> {
    let root = dir.canonicalize().with_context(|| format!("no directory {}", dir.display()))?;
    let env = caller_env(std::env::vars_os());
    let mut conn = match endpoint::socket_dir().and_then(|socket| client::connect(&socket, "cli")) {
        Ok(conn) => conn,
        Err(e) => return Ok(in_process(&verb, format!("{e:#}"))),
    };
    let tty = std::io::stdin().is_terminal();
    let mut ui = Ui {
        tty,
        pick: &mut |prompt, options| {
            let refs: Vec<&str> = options.iter().map(String::as_str).collect();
            crate::commands::pick(prompt, &refs)
        },
        offer_example: &mut || crate::commands::run::offer_example_config(&root),
    };
    let prepared = match prepare(&verb, &root, env, &mut |method, params| conn.call(method, params), &mut ui)? {
        Prep::Send(prepared) => prepared,
        Prep::Exit(code) => return Ok(Done::Exit(code)),
        Prep::Old(why) => return Ok(in_process(&verb, why)),
    };

    let instance = prepared.instance.clone();
    let mut last = Streaming { instance: instance.clone(), cancelled: false, track_progress: false, held: None };
    let answer = with_confirm(prepared.params, &mut |params| {
        let (answer, streaming) = stream(&mut conn, &verb, prepared.method, params, instance.clone())?;
        last = streaming;
        Ok(answer)
    }, &mut |question| crate::commands::confirm(question))?;
    drop(conn);

    let ending = ending(answer, last.cancelled);
    if !matches!(ending, Ok(Ending::Detached(_))) {
        last.replay();
    }
    match ending? {
        Ending::Exit(code) => Ok(Done::Exit(code)),
        Ending::Detached(result) => {
            print_detached(&verb, &result)?;
            Ok(Done::Exit(0))
        }
        Ending::Lost => Ok(Done::Exit(lost(&verb, last.instance.as_deref())?)),
        Ending::Old(why) => Ok(in_process(&verb, why)),
    }
}

/// The fallback, announced unless `-d` (which refuses it with `why`).
fn in_process(verb: &CliVerb, why: String) -> Done {
    if !verb.detach() {
        eprintln!("warning: devsandbox serve: {why}; running in-process");
    }
    Done::InProcess(why)
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
    /// A numbered pick among `options`; the chosen index.
    pick: &'a mut dyn FnMut(&str, &[String]) -> Result<usize>,
    /// `run` on a config without sandboxes (`offer_example_config`).
    offer_example: &'a mut dyn FnMut() -> Result<()>,
}

/// What [`prepare`] decided.
#[derive(Debug)]
enum Prep {
    Send(Prepared),
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
fn prepare(verb: &CliVerb, root: &Path, env: BTreeMap<String, String>, call: &mut Call, ui: &mut Ui) -> Result<Prep> {
    let root: PathBuf = root.to_path_buf();
    macro_rules! resolved {
        ($name:expr) => {
            match resolve($name, call, ui)? {
                Ok(name) => name,
                Err(old) => return Ok(Prep::Old(old)),
            }
        };
    }
    let (method, params, instance) = match verb.clone() {
        CliVerb::Run { sandbox, name, branch, base, json, detach } => {
            // Always listed, not only for the pick: an empty config is the
            // example offer, which writes into the user's dir (F6).
            let names = match reply(call("sandboxes.list", json!({ "root": root }))?) {
                Reply::Ok(rows) => sandbox_names(&rows)?,
                Reply::Old(why) => return Ok(Prep::Old(why)),
                Reply::Err(e) => bail!("{}", e.message),
            };
            if names.is_empty() {
                (ui.offer_example)()?;
                // As in-process `run` returning `None`: no instance to report.
                if json {
                    bail!("no instance created");
                }
                return Ok(Prep::Exit(0));
            }
            let sandbox = match sandbox {
                Some(sandbox) => sandbox,
                None if !ui.tty => bail!("no sandbox specified; available: {}", names.join(", ")),
                None => names[(ui.pick)("Select a sandbox", &names)?].clone(),
            };
            let p = RunParams { root, env, sandbox, name, branch, base, json, detach };
            ("run", serde_json::to_value(p)?, None)
        }
        CliVerb::Rebuild { target, force, detach } => {
            let instance = match target {
                Some(name) => Some(resolved!(&name)),
                None => None,
            };
            let all = instance.is_none();
            let p = RebuildParams { root, env, instance: instance.clone(), all, force, detach };
            ("rebuild", serde_json::to_value(p)?, instance)
        }
        CliVerb::Start { target, detach } => {
            let instance = match target {
                Some(name) => Some(resolved!(&name)),
                None => None,
            };
            let p = StartParams { root, env, instance: instance.clone(), all: instance.is_none(), detach };
            ("start", serde_json::to_value(p)?, instance)
        }
        CliVerb::Stop { target } => {
            let instance = match target {
                Some(name) => Some(resolved!(&name)),
                None => None,
            };
            // Never `detach`: Ctrl-C (the hang-up) cancels a held verb.
            let p = StopParams { root, env, instance: instance.clone(), all: instance.is_none(), detach: false };
            ("stop", serde_json::to_value(p)?, instance)
        }
        CliVerb::Rm { name, delete_branch, force } => {
            let instance = resolved!(&name);
            // Off a TTY nothing could answer: the non-TTY default, as today.
            let ask = ui.tty && delete_branch.is_none();
            let p = RmParams { root, env, instance: instance.clone(), delete_branch, force, ask, detach: false };
            ("rm", serde_json::to_value(p)?, Some(instance))
        }
        CliVerb::ServicesRebuild { name } => {
            let p = ServicesRebuildParams { root, env, name, detach: false };
            ("services.rebuild", serde_json::to_value(p)?, None)
        }
    };
    Ok(Prep::Send(Prepared { method, params, instance }))
}

fn sandbox_names(rows: &Value) -> Result<Vec<String>> {
    let rows = rows.as_array().context("bad sandboxes.list result from devsandbox serve")?;
    rows.iter()
        .map(|row| row["name"].as_str().map(str::to_string).context("bad sandboxes.list entry from devsandbox serve"))
        .collect()
}

/// `name` as one instance (`instances.resolve`), picking on a TTY among
/// several as the in-process CLI does; `Err(why)` from an older daemon.
fn resolve(name: &str, call: &mut Call, ui: &mut Ui) -> Result<Result<String, String>> {
    let matches: Vec<String> = match reply(call("instances.resolve", json!({ "name": name }))?) {
        Reply::Ok(v) => serde_json::from_value(v["matches"].clone())
            .context("bad instances.resolve result from devsandbox serve")?,
        Reply::Old(why) => return Ok(Err(why)),
        Reply::Err(e) => bail!("{}", e.message),
    };
    Ok(Ok(match matches.len() {
        0 => bail!("{}", crate::commands::no_instance_matches(name)),
        1 => matches.into_iter().next().expect("one match"),
        _ if !ui.tty => bail!("{}", crate::commands::ambiguous_instance(name, &matches)),
        _ => {
            let at = (ui.pick)(&crate::commands::ambiguous_prompt(name), &matches)?;
            matches.into_iter().nth(at).context("selection out of range")?
        }
    }))
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
}

/// Send one verb request on `conn` and stream it to its answer, with
/// Ctrl-C caught meanwhile (module doc).
fn stream(conn: &mut Conn, verb: &CliVerb, method: &str, params: Value, instance: Option<String>) -> Result<(Answer, Streaming)> {
    let track_progress = matches!(verb, CliVerb::Run { .. });
    // rm's `ask` round too: on `confirm` its output is just the question,
    // as an `Error:` line, which the prompt asks properly.
    let held = (verb.detach() || params["ask"] == true).then(Vec::new);
    let mut s = Streaming { instance, cancelled: false, track_progress, held };
    let _guard = cancel::catch();
    let streamed = conn.call_streaming(method, params, TICK, &mut |note| {
        if let Some(note) = note {
            s.observe(note);
        }
        if cancel::requested() && !s.cancelled {
            s.cancelled = true;
            return match &s.instance {
                Some(instance) => cancel_by_name(instance),
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
fn cancel_by_name(instance: &str) -> bool {
    let Ok(socket) = endpoint::socket_dir() else { return false };
    let conn = client::connect_running(&socket, "cli", &Version::current(), client::START_TIMEOUT);
    let Ok(Some(mut conn)) = conn else { return false };
    match conn.call("instances.cancel", json!({ "instance": instance })) {
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
/// `run -d --json` the run record, enveloped as `run --json` prints it.
fn print_detached(verb: &CliVerb, result: &Value) -> Result<()> {
    let instance = result["instance"].as_str();
    if let CliVerb::Run { json: true, .. } = verb {
        let doc = match (result.get("record"), instance) {
            (Some(record), _) => serde_json::to_string_pretty(&crate::commands::status::Envelope::new(record))?,
            (None, Some(key)) => crate::commands::run::run_record_json(key)?,
            (None, None) => bail!("no instance created"),
        };
        println!("{doc}");
    } else if let Some(instance) = instance {
        println!("{instance}");
    }
    Ok(())
}

/// The daemon went away before the answer (a handoff, a crash): the verb
/// runs on, orphaned. A build with a known instance is followed through its
/// build log, then exits by its phase; anything else can't be watched.
fn lost(verb: &CliVerb, instance: Option<&str>) -> Result<i32> {
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
        let calls = RefCell::new(Vec::new());
        let mut call = |method: &str, params: Value| {
            calls.borrow_mut().push(format!("{method} {params}"));
            let r = answers.iter().find(|(m, _)| *m == method).map(|(_, r)| r).expect("unexpected call");
            Ok(Response { id: r.id, result: r.result.clone(), error: r.error.clone() })
        };
        let offered = RefCell::new(false);
        let mut ui = Ui {
            tty,
            pick: &mut |_, options| {
                assert!(options.len() > 1);
                Ok(pick)
            },
            offer_example: &mut || {
                *offered.borrow_mut() = true;
                Ok(())
            },
        };
        let env = BTreeMap::from([("A".to_string(), "1".to_string())]);
        let out = prepare(verb, Path::new("/cfg"), env, &mut call, &mut ui);
        (out, calls.into_inner(), offered.into_inner())
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

    #[test]
    fn progress_names_a_runs_instance_once() {
        let note = |instance: &str| Notification {
            method: "progress".into(),
            params: json!({"request": 1, "instance": instance, "phase": "building"}),
        };
        let mut s = Streaming { instance: None, cancelled: false, track_progress: true, held: None };
        s.observe(&note("web-2"));
        s.observe(&note("other"));
        assert_eq!(s.instance.as_deref(), Some("web-2"));
        // Not a run: the resolved name stands.
        let mut s = Streaming { instance: None, cancelled: false, track_progress: false, held: None };
        s.observe(&note("web-2"));
        assert_eq!(s.instance, None);
    }

    #[test]
    fn detach_holds_output_back() {
        let out = |stream: &str, data: &str| Notification {
            method: "output".into(),
            params: json!({"request": 1, "stream": stream, "data": data}),
        };
        let mut s = Streaming { instance: None, cancelled: false, track_progress: true, held: Some(Vec::new()) };
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
}
