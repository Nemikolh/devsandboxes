//! CLI re-runs on a machine (docs/architecture/daemon/machines.md, *The
//! CLI*; fleet plan decision 15): API calls through this host's daemon with
//! `machine` set (the quick calls, `machines::forward::QUICK`), which go
//! over the machine's link and its trust table, never a `devsandbox …` run
//! there; printed by the local commands' own render fns so the output reads
//! as theirs. What has no such call yet is refused ([`refusal`]).
//!
//! [`use_of`] is the pure choice; [`named`], [`per_root`] and [`listing`] make the calls
//! through an injected [`Call`] and write to injected streams, so the tests
//! feed them fixture answers ([`follow_build_log`] sleeps through an
//! injected fn too, [`follow_next`] its pure step).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::serve::api::{ContainerLogs, ContainersList, ContainersStats, InstancesList, MAX_LINES, Renamed};
use crate::serve::proto::Response;
use crate::snapshot::InstanceRow;
use crate::{Command, ServiceCommand};

/// One daemon request: method, params → its response.
pub(crate) type Call<'a> = dyn FnMut(&str, Value) -> Result<Response> + 'a;

/// What a command does on a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Use {
    /// `logs` (no `--build`): `containers.logs` of the instance's container.
    Logs { lines: usize },
    /// `logs --build`: `instances.buildLog`; `--follow`, its pages
    /// ([`follow_build_log`]).
    BuildLog { follow: bool },
    /// `inspect`: `containers.inspect`.
    Inspect { json: bool },
    /// `done` (`true`) / `undone`: `instances.done`.
    Done(bool),
    /// `rename`: `instances.rename` to this name.
    Rename(String),
    /// `--machine m ls`: `instances.list` per config root, its sandboxes.
    Ls { json: bool },
    /// `--machine m status --json`: `instances.list` per config root.
    Status,
    /// `--machine m service ls`: `instances.list` per config root, its services.
    ServiceLs { json: bool },
    /// `--machine m ps`: `containers.list`.
    Ps { all: bool, json: bool },
    /// `--machine m stats`: `containers.stats`.
    Stats { json: bool },
    /// Not an API call made here: what goes over the link's streams
    /// (`exec`, its sessions, `port`) and the verbs (`gc` among them, sent
    /// by `verbs_client`).
    Link,
    Refused(Refused),
}

/// What a machine refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    Vscode,
}

/// [`Use`] of `command` on a machine. Every `exec` is [`Use::Link`] here:
/// whether it gets a tty, so a session `PTY` dial or a piped one, is the
/// caller's to tell (`cli::reconnect_session`, `cli::pipe_mode`).
pub(crate) fn use_of(command: &Command) -> Use {
    match command {
        Command::Logs { build: true, follow, .. } => Use::BuildLog { follow: *follow },
        Command::Logs { lines, .. } => Use::Logs { lines: *lines },
        Command::Inspect { json, .. } => Use::Inspect { json: *json },
        Command::Done { .. } => Use::Done(true),
        Command::Undone { .. } => Use::Done(false),
        Command::Rename { new_name, .. } => Use::Rename(new_name.clone()),
        Command::Ls { json } => Use::Ls { json: *json },
        Command::Status { .. } => Use::Status,
        Command::Ps { all, json } => Use::Ps { all: *all, json: *json },
        Command::Stats { json } => Use::Stats { json: *json },
        Command::Service { cmd: ServiceCommand::Ls { json } } => Use::ServiceLs { json: *json },
        Command::Vscode { .. } => Use::Refused(Refused::Vscode),
        _ => Use::Link,
    }
}

/// Why machine `m` refuses `what`.
pub(crate) fn refusal(m: &str, what: Refused) -> anyhow::Error {
    let what = match what {
        Refused::Vscode => "`devsandbox vscode`",
    };
    anyhow::anyhow!("machine `{m}`: {what} isn't available over its link yet")
}

/// An error answer of machine `m` as ours: its message, naming `m` when
/// it's the machine's trust refusing (`denied`), whose advice (`devsandbox
/// machine trust full` there, `machine roots add`) is about that machine.
fn answer_error(m: &str, e: crate::serve::proto::ErrorBody) -> anyhow::Error {
    match e.code.as_str() {
        "denied" => anyhow::anyhow!("machine `{m}`: {}", e.message),
        _ => anyhow::anyhow!("{}", e.message),
    }
}

/// `method` with `params` (plus `machine: m`) through `call`: its result, or
/// its error as ours ([`answer_error`]).
fn call_on<T: DeserializeOwned>(call: &mut Call, m: &str, method: &str, mut params: Value) -> Result<T> {
    params["machine"] = json!(m);
    let r = call(method, params)?;
    if let Some(e) = r.error {
        return Err(answer_error(m, e));
    }
    serde_json::from_value(r.result.unwrap_or(Value::Null)).with_context(|| format!("bad {method} result from devsandbox serve"))
}

fn list(call: &mut Call, m: &str, dir: &Path, depth: Option<&str>) -> Result<InstancesList> {
    let mut params = json!({ "dir": dir });
    if let Some(depth) = depth {
        params["depth"] = json!(depth);
    }
    call_on(call, m, "instances.list", params)
}

/// Instance `name`'s row on `m`, from a listing at `root` (any root there:
/// a snapshot lists every instance the machine's trust admits, whatever its
/// `dir`).
fn row_of(call: &mut Call, m: &str, root: &Path, name: &str) -> Result<Option<InstanceRow>> {
    Ok(list(call, m, root, Some("disk"))?.snapshot.instances.into_iter().find(|r| r.name == name))
}

/// The container of `name`: its row's (a rename keeps the container), else
/// `name` taken as a container name, as `commands::resolve_container`
/// matches one (with or without the prefix).
fn container_of(row: Option<&InstanceRow>, name: &str) -> String {
    match row {
        Some(row) => row.container.clone(),
        None if name.starts_with(crate::runtime::NAME_PREFIX) => name.to_string(),
        None => format!("{}{name}", crate::runtime::NAME_PREFIX),
    }
}

/// Run named `u` (not [`Use::Ls`] / `Status` / `Link` / `Refused`) on
/// instance or container `name` of machine `m`, `root` its config
/// root there (or the machine's root). `tty`: stdout is a terminal (a pty
/// build log keeps its colours). The exit code.
pub(crate) fn named(call: &mut Call, u: &Use, m: &str, name: &str, root: &Path, tty: bool, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    match u {
        Use::Logs { lines } => {
            let container = container_of(row_of(call, m, root, name)?.as_ref(), name);
            let logs: ContainerLogs = call_on(call, m, "containers.logs", json!({ "container": container, "lines": lines }))?;
            writeln!(out, "{}", crate::commands::logs::render_logs(&logs.text))?;
        }
        Use::BuildLog { follow: true } => {
            // By id: a rename while it's followed doesn't lose it.
            let id = row_of(call, m, root, name)?.map_or_else(|| name.to_string(), |r| r.instance_id);
            follow_build_log(call, m, name, &id, tty, out, &mut std::thread::sleep)?;
        }
        Use::BuildLog { follow: false } => {
            // The whole log, as `logs --build` prints it, up to the API's cap.
            let tail: Option<crate::build_log::Tail> =
                call_on(call, m, "instances.buildLog", json!({ "instance": name, "lines": MAX_LINES, "ansi": tty }))?;
            let Some(tail) = tail else { bail!("`{m}/{name}` has no build log") };
            if tail.cut {
                writeln!(err, "(the last {MAX_LINES} lines of `{m}/{name}`'s build log)")?;
            }
            out.write_all(tail.text.as_bytes())?;
            if !tail.text.is_empty() && !tail.text.ends_with('\n') {
                writeln!(out)?;
            }
        }
        Use::Inspect { json } => {
            let container = container_of(row_of(call, m, root, name)?.as_ref(), name);
            let doc: Value = call_on(call, m, "containers.inspect", json!({ "container": container }))?;
            writeln!(out, "{}", crate::commands::inspect::render(&doc.to_string(), *json))?;
        }
        Use::Done(done) => {
            // The line says whether it changed: the flag as the rows had it.
            let was = row_of(call, m, root, name)?.map(|r| r.done).unwrap_or(!*done);
            let _: Value = call_on(call, m, "instances.done", json!({ "instance": name, "done": done }))?;
            writeln!(out, "{}", crate::commands::done::status_line(name, *done, was))?;
        }
        Use::Rename(new_name) => {
            let r: Renamed = call_on(call, m, "instances.rename", json!({ "instance": name, "newName": new_name }))?;
            writeln!(out, "{}", crate::commands::rename::renamed_line(name, &r.instance))?;
        }
        Use::Ls { .. }
        | Use::Status
        | Use::ServiceLs { .. }
        | Use::Ps { .. }
        | Use::Stats { .. }
        | Use::Link
        | Use::Refused(_) => {
            bail!("not a named re-run")
        }
    }
    Ok(0)
}

/// What [`follow_build_log`] does after a page read at `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Follow {
    /// Ask for the next page now: this one brought something.
    Fetch,
    /// Nothing new yet: ask again after `FOLLOW_POLL`.
    Wait,
    /// The log is whole and printed.
    Done,
}

/// [`Follow`] after `page`, asked for at `offset`.
pub(crate) fn follow_next(page: &crate::build_log::Page, offset: u64) -> Follow {
    if page.complete {
        Follow::Done
    } else if page.next == offset {
        Follow::Wait
    } else {
        Follow::Fetch
    }
}

/// `logs --build --follow` of instance `name` (`id`: its `instance_id`, or
/// the name) on machine `m`: `instances.buildLog` pages from the
/// log's start until one is complete, printed as `commands::logs::build_log`
/// follows a local one (as they are on a terminal, `tty`; a pty log's
/// escapes stripped as they come elsewhere, `build_log::Strip`). The record
/// gone meanwhile (`not-found`, no log) ends it, as locally.
pub(crate) fn follow_build_log(
    call: &mut Call,
    m: &str,
    name: &str,
    id: &str,
    tty: bool,
    out: &mut dyn Write,
    sleep: &mut dyn FnMut(Duration),
) -> Result<()> {
    let mut offset = 0;
    let mut strip: Option<Option<crate::build_log::Strip>> = None;
    loop {
        let r = call("instances.buildLog", json!({ "instance": id, "offset": offset, "machine": m }))?;
        let page: Option<crate::build_log::Page> = match r.error {
            Some(e) if strip.is_some() && e.code == "not-found" => return Ok(()),
            Some(e) => return Err(answer_error(m, e)),
            None => serde_json::from_value(r.result.unwrap_or(Value::Null))
                .context("bad instances.buildLog result from devsandbox serve")?,
        };
        let Some(page) = page else {
            if strip.is_none() {
                bail!("`{m}/{name}` has no build log");
            }
            return Ok(());
        };
        let strip = strip.get_or_insert_with(|| (!tty && page.tty).then(crate::build_log::Strip::default));
        match strip {
            Some(strip) => out.write_all(&strip.feed(page.text.as_bytes()))?,
            None => out.write_all(page.text.as_bytes())?,
        }
        out.flush()?;
        let next = follow_next(&page, offset);
        offset = page.next;
        match next {
            Follow::Done => return Ok(()),
            Follow::Wait => sleep(crate::commands::logs::FOLLOW_POLL),
            Follow::Fetch => {}
        }
    }
}

/// `--machine m ps|stats` (`u`) on machine `m`: one `containers.list` /
/// `containers.stats` (the machine cuts the rows to what its trust admits),
/// printed by the local command's render. The exit code.
pub(crate) fn listing(call: &mut Call, u: &Use, m: &str, out: &mut dyn Write) -> Result<i32> {
    match u {
        Use::Ps { all, json } => {
            let list: ContainersList = call_on(call, m, "containers.list", json!({ "all": all }))?;
            write!(out, "{}", crate::commands::ps::render(list.rows, *json)?)?;
        }
        Use::Stats { json } => {
            let stats: ContainersStats = call_on(call, m, "containers.stats", json!({}))?;
            write!(out, "{}", crate::commands::stats::render(stats.rows, *json)?)?;
        }
        _ => bail!("not a listing re-run"),
    }
    Ok(0)
}

/// The config roots a machine root `at` stands for, from a listing of it:
/// `at` itself when it is one (an ssh machine's registered roots), and the
/// config roots under it its instance rows name (a fleet root's, one per
/// placed config; a listing lists every admitted instance, so rows outside
/// `at` are other roots'). A root without a config of its own (the
/// listing's `config: ` error) stands only for those under it, when it has
/// any.
pub(crate) fn config_roots(at: &Path, list: &InstancesList) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = list
        .snapshot
        .instances
        .iter()
        .filter_map(|r| r.root.as_ref().map(PathBuf::from))
        .filter(|r| r.starts_with(at))
        .collect();
    let configless = list.snapshot.error.as_deref().is_some_and(|e| e.contains("config: "));
    if roots.is_empty() || !configless {
        roots.push(at.to_path_buf());
    }
    roots.sort();
    roots.dedup();
    roots
}

/// `--machine m ls|status|service ls` (`u`) on machine `m` with registered
/// `roots`: one `instances.list` per config root ([`config_roots`]), each
/// after its banner on `err` (`cli::root_banners`' rules: `--json` with
/// several roots is an error). The exit code.
pub(crate) fn per_root(call: &mut Call, u: &Use, m: &str, roots: &[PathBuf], out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    let json = match u {
        Use::Ls { json } | Use::ServiceLs { json } => *json,
        Use::Status => true,
        _ => bail!("not a per-root re-run"),
    };
    let mut config = Vec::new();
    for at in roots {
        config.extend(config_roots(at, &list(call, m, at, Some("disk"))?));
    }
    config.sort();
    config.dedup();
    let banners = super::root_banners(m, &config, json)?;
    for (root, banner) in config.iter().zip(banners) {
        writeln!(err, "{banner}")?;
        match u {
            Use::Status => writeln!(out, "{}", crate::commands::status::render(&list(call, m, root, None)?.snapshot)?)?,
            Use::ServiceLs { json } => {
                // A listing for the statuses; `used_by` already cut to what the trust admits.
                let snapshot = list(call, m, root, Some("listing"))?.snapshot;
                if let Some(e) = snapshot.error {
                    if snapshot.services.is_empty() && e.contains("config: ") {
                        // Where the local `service ls` fails loading the config.
                        bail!("{m}:{}: {e}", root.display());
                    }
                    // The runtime's and the resolve failures, as the local one's stderr.
                    writeln!(err, "{e}")?;
                }
                let config = root.join(crate::config::CONFIG_FILE);
                write!(out, "{}", crate::commands::services::ls_render(&snapshot.services, *json, &config)?)?;
            }
            _ => {
                let snapshot = list(call, m, root, Some("disk"))?.snapshot;
                if json {
                    writeln!(out, "{}", crate::commands::ls::render_json(&snapshot.sandboxes)?)?;
                } else if !snapshot.sandboxes.is_empty() {
                    let lines: Vec<_> = snapshot.sandboxes.iter().map(crate::commands::ls::Line::of_row).collect();
                    write!(out, "{}", crate::commands::ls::table(&lines))?;
                } else if let Some(e) = snapshot.error.filter(|e| e.contains("config: ")) {
                    // Where the local `ls` fails loading the config.
                    bail!("{m}:{}: {e}", root.display());
                } else {
                    writeln!(out, "{}", crate::commands::ls::none_defined(root))?;
                }
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::Cli;
    use crate::serve::proto::ErrorBody;

    fn command(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("devsandbox").chain(args.iter().copied())).unwrap_or_else(|e| panic!("{e}")).command.unwrap()
    }

    #[test]
    fn each_command_maps_to_a_call_or_a_refusal() {
        let u = |args: &[&str]| use_of(&command(args));
        assert_eq!(u(&["logs", "lap/web"]), Use::Logs { lines: 50 });
        assert_eq!(u(&["logs", "-n", "7", "lap/web"]), Use::Logs { lines: 7 });
        assert_eq!(u(&["logs", "--build", "lap/web"]), Use::BuildLog { follow: false });
        assert_eq!(u(&["logs", "--build", "-f", "lap/web"]), Use::BuildLog { follow: true });
        assert_eq!(u(&["inspect", "lap/web"]), Use::Inspect { json: false });
        assert_eq!(u(&["inspect", "--json", "lap/web"]), Use::Inspect { json: true });
        assert_eq!(u(&["done", "lap/web"]), Use::Done(true));
        assert_eq!(u(&["undone", "lap/web"]), Use::Done(false));
        assert_eq!(u(&["rename", "lap/web", "api"]), Use::Rename("api".into()));
        assert_eq!(u(&["ls"]), Use::Ls { json: false });
        assert_eq!(u(&["ls", "--json"]), Use::Ls { json: true });
        assert_eq!(u(&["status", "--json"]), Use::Status);
        assert_eq!(u(&["ps", "-a"]), Use::Ps { all: true, json: false });
        assert_eq!(u(&["ps", "--json"]), Use::Ps { all: false, json: true });
        assert_eq!(u(&["stats"]), Use::Stats { json: false });
        assert_eq!(u(&["stats", "--json"]), Use::Stats { json: true });
        // `gc` is a verb: `verbs_client` sends it.
        assert_eq!(u(&["gc", "--force"]), Use::Link);
        assert_eq!(u(&["service", "ls"]), Use::ServiceLs { json: false });
        assert_eq!(u(&["service", "ls", "--json"]), Use::ServiceLs { json: true });
        assert_eq!(u(&["vscode", "lap/web"]), Use::Refused(Refused::Vscode));
        for args in [&["exec", "lap/web"][..], &["exec", "lap/web", "--", "ls"], &["port", "lap/web", "3000"], &["stop", "lap/web"]] {
            assert_eq!(u(args), Use::Link, "{args:?}");
        }
    }

    #[test]
    fn refusals_name_the_command_and_why() {
        let r = |what| refusal("lap", what).to_string();
        assert_eq!(r(Refused::Vscode), "machine `lap`: `devsandbox vscode` isn't available over its link yet");
    }

    fn row(name: &str, container: &str, root: &str, done: bool) -> Value {
        json!({"name": name, "sandbox": "web", "container": container, "status": {"state": "running", "text": "Up 3 minutes"}, "uptime_secs": 0,
            "cpu": null, "mem": null, "folder": "", "worktree": false, "services": [], "workspace": "",
            "remote_user": null, "remote_env_len": 0, "base_folder": "", "drift": false, "instance_id": "id",
            "done": done, "root": root})
    }

    fn sandbox(name: &str, source: &str, folder: Option<&str>, services: &[&str], extends: &[&str]) -> Value {
        json!({"name": name, "source": source, "folder": folder, "services": services, "extends": extends,
            "config_hash": "h", "build_hash": "", "issues": [], "dispatcher": false, "worktree_branch": null, "root": "/f/cfg"})
    }

    fn listing(rows: Vec<Value>, sandboxes: Vec<Value>, error: Option<&str>) -> Value {
        json!({"instances": rows, "sandboxes": sandboxes, "services": [], "sandbox_count": 0, "runtime_name": "docker",
            "runtime_version": null, "error": error})
    }

    /// A fake daemon: each call recorded, answered by `answer`.
    struct Fake<F> {
        calls: Vec<(String, Value)>,
        answer: F,
    }

    impl<F: FnMut(&str, &Value) -> Result<Value, (&'static str, String)>> Fake<F> {
        fn call(&mut self, method: &str, params: Value) -> Result<Response> {
            let r = (self.answer)(method, &params);
            self.calls.push((method.to_string(), params));
            Ok(match r {
                Ok(v) => Response { id: None, result: Some(v), error: None },
                Err((code, message)) => {
                    Response { id: None, result: None, error: Some(ErrorBody { code: code.into(), message, data: Default::default() }) }
                }
            })
        }
    }

    fn fake<F: FnMut(&str, &Value) -> Result<Value, (&'static str, String)>>(answer: F) -> Fake<F> {
        Fake { calls: Vec::new(), answer }
    }

    fn run_named(u: Use, name: &str, answer: impl FnMut(&str, &Value) -> Result<Value, (&'static str, String)>) -> (Result<i32>, String, String, Vec<(String, Value)>) {
        let mut f = fake(answer);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let r = named(&mut |m, p| f.call(m, p), &u, "lap", name, Path::new("/f/cfg"), false, &mut out, &mut err);
        (r, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap(), f.calls)
    }

    /// The renamed instance keeps its container: logs and inspect ask for
    /// the row's, and print what the local commands print.
    #[test]
    fn logs_and_inspect_print_as_the_local_commands() {
        let rows = || listing(vec![row("web", "devsandbox-old", "/f/cfg", false)], vec![], None);
        let (r, out, _, calls) = run_named(Use::Logs { lines: 7 }, "web", |m, _| {
            Ok(if m == "instances.list" { rows() } else { json!({"text": "a\nb"}) })
        });
        assert_eq!(r.unwrap(), 0);
        assert_eq!(out, format!("{}\n", crate::commands::logs::render_logs("a\nb")));
        assert_eq!(calls[0], ("instances.list".into(), json!({"dir": "/f/cfg", "depth": "disk", "machine": "lap"})));
        assert_eq!(calls[1], ("containers.logs".into(), json!({"container": "devsandbox-old", "lines": 7, "machine": "lap"})));

        let (_, out, ..) = run_named(Use::Logs { lines: 50 }, "web", |m, _| Ok(if m == "instances.list" { rows() } else { json!({"text": " \n"}) }));
        assert_eq!(out, "(no log output)\n");

        let doc = json!([{"Id": "abc", "State": {"Running": true}}]);
        for json in [false, true] {
            let (_, out, _, calls) =
                run_named(Use::Inspect { json }, "web", |m, _| Ok(if m == "instances.list" { rows() } else { doc.clone() }));
            assert_eq!(out, format!("{}\n", crate::commands::inspect::render(&doc.to_string(), json)));
            assert_eq!(calls[1].1, json!({"container": "devsandbox-old", "machine": "lap"}));
        }
        // A container name, not an instance: as typed, prefixed if need be.
        let (_, _, _, calls) = run_named(Use::Inspect { json: true }, "x", |m, _| Ok(if m == "instances.list" { rows() } else { json!({}) }));
        assert_eq!(calls[1].1["container"], "devsandbox-x");
    }

    #[test]
    fn build_logs_print_the_tail_or_say_there_is_none() {
        let (r, out, err, calls) = run_named(Use::BuildLog { follow: false }, "web", |_, _| Ok(json!({"text": "$ run\nok", "cut": true})));
        assert_eq!((r.unwrap(), out.as_str()), (0, "$ run\nok\n"));
        assert_eq!(err, format!("(the last {MAX_LINES} lines of `lap/web`'s build log)\n"));
        assert_eq!(calls, [("instances.buildLog".into(), json!({"instance": "web", "lines": MAX_LINES, "ansi": false, "machine": "lap"}))]);
        let (r, ..) = run_named(Use::BuildLog { follow: false }, "web", |_, _| Ok(Value::Null));
        assert_eq!(r.unwrap_err().to_string(), "`lap/web` has no build log");
        let (r, ..) = run_named(Use::BuildLog { follow: false }, "web", |_, _| Err(("not-found", "no instance `web`".into())));
        assert_eq!(r.unwrap_err().to_string(), "no instance `web`");
    }

    fn page(text: &str, next: u64, tty: bool, complete: bool) -> crate::build_log::Page {
        crate::build_log::Page { text: text.into(), next, tty, complete }
    }

    #[test]
    fn a_follow_fetches_while_pages_bring_something_and_waits_when_none_do() {
        assert_eq!(follow_next(&page("abc", 3, false, false), 0), Follow::Fetch);
        assert_eq!(follow_next(&page("", 3, false, false), 3), Follow::Wait);
        assert_eq!(follow_next(&page("abc", 6, false, true), 3), Follow::Done);
        assert_eq!(follow_next(&page("", 6, false, true), 6), Follow::Done);
    }

    /// `answers` in order (the last one again once they run out): what the
    /// follow printed, its calls' params, its sleeps and its result.
    fn run_follow(tty: bool, answers: Vec<Result<Value, (&'static str, String)>>) -> (Result<()>, String, Vec<Value>, usize) {
        let mut answers = answers.into_iter();
        let mut last = None;
        let mut f = fake(|m: &str, _: &Value| {
            assert_eq!(m, "instances.buildLog");
            match answers.next() {
                Some(a) => {
                    last = Some(a.clone());
                    a
                }
                None => last.clone().unwrap(),
            }
        });
        let (mut out, mut sleeps) = (Vec::new(), 0);
        let r = follow_build_log(&mut |m, p| f.call(m, p), "lap", "web", "id-web", tty, &mut out, &mut |d| {
            assert_eq!(d, crate::commands::logs::FOLLOW_POLL);
            sleeps += 1;
        });
        (r, String::from_utf8(out).unwrap(), f.calls.into_iter().map(|(_, p)| p).collect(), sleeps)
    }

    #[test]
    fn a_followed_build_log_prints_its_pages_until_one_is_complete() {
        let v = |p: crate::build_log::Page| Ok(serde_json::to_value(p).unwrap());
        let pages = || {
            vec![
                v(page("$ run\n", 6, false, false)),
                v(page("", 6, false, false)),
                v(page("\x1b[32mok\x1b[0m\r\n", 19, false, false)),
                v(page("end\n", 23, false, true)),
            ]
        };
        let (r, out, params, sleeps) = run_follow(false, pages());
        r.unwrap();
        assert_eq!(out, "$ run\n\x1b[32mok\x1b[0m\r\nend\n", "a pipe log as it is");
        assert_eq!(sleeps, 1);
        let offsets: Vec<_> = params.iter().map(|p| p["offset"].as_u64().unwrap()).collect();
        assert_eq!(offsets, [0, 6, 6, 19]);
        assert_eq!(params[0], json!({"instance": "id-web", "offset": 0, "machine": "lap"}));

        // A pty log: as it is on a terminal, its escapes stripped elsewhere.
        let pty = || vec![v(page("\x1b[32mok\x1b[0m\r\n", 13, true, false)), v(page("50%\r100%\r\n", 24, true, true))];
        assert_eq!(run_follow(true, pty()).1, "\x1b[32mok\x1b[0m\r\n50%\r100%\r\n");
        let mut strip = crate::build_log::Strip::default();
        let want = [strip.feed(b"\x1b[32mok\x1b[0m\r\n"), strip.feed(b"50%\r100%\r\n")].concat();
        assert_eq!(run_follow(false, pty()).1, String::from_utf8(want).unwrap());

        // No log at first is an error; the record gone later ends it.
        assert_eq!(run_follow(false, vec![Ok(Value::Null)]).0.unwrap_err().to_string(), "`lap/web` has no build log");
        let gone = || Err(("not-found", "no instance `id-web`".to_string()));
        assert_eq!(run_follow(false, vec![gone()]).0.unwrap_err().to_string(), "no instance `id-web`");
        let (r, out, ..) = run_follow(false, vec![v(page("a", 1, false, false)), gone()]);
        assert_eq!((r.is_ok(), out.as_str()), (true, "a"));
        let (r, ..) = run_follow(false, vec![v(page("a", 1, false, false)), Ok(Value::Null)]);
        assert!(r.is_ok());
    }

    /// The follow asks by the row's `instance_id`, so a rename doesn't lose it.
    #[test]
    fn a_named_follow_asks_by_instance_id() {
        let (r, out, _, calls) = run_named(Use::BuildLog { follow: true }, "web", |m, _| {
            Ok(if m == "instances.list" {
                listing(vec![row("web", "c", "/f/cfg", false)], vec![], None)
            } else {
                serde_json::to_value(page("done\n", 5, false, true)).unwrap()
            })
        });
        assert_eq!((r.unwrap(), out.as_str()), (0, "done\n"));
        assert_eq!(calls[1], ("instances.buildLog".into(), json!({"instance": "id", "offset": 0, "machine": "lap"})));
    }

    /// `done` words its line from the flag the rows had, as the local one
    /// from the state; `rename` prints the trimmed name the daemon answers.
    #[test]
    fn done_and_rename_print_as_the_local_commands() {
        for (done, was) in [(true, false), (true, true), (false, true), (false, false)] {
            let (r, out, _, calls) = run_named(Use::Done(done), "web", |m, _| {
                Ok(if m == "instances.list" { listing(vec![row("web", "c", "/f/cfg", was)], vec![], None) } else { json!({"instance": "web", "done": done}) })
            });
            assert_eq!(r.unwrap(), 0);
            assert_eq!(out, format!("{}\n", crate::commands::done::status_line("web", done, was)));
            assert_eq!(calls[1], ("instances.done".into(), json!({"instance": "web", "done": done, "machine": "lap"})));
        }
        let (_, out, _, calls) = run_named(Use::Rename(" api ".into()), "web", |_, _| Ok(json!({"instance": "api"})));
        assert_eq!(out, "renamed web -> api\n");
        assert_eq!(calls, [("instances.rename".into(), json!({"instance": "web", "newName": " api ", "machine": "lap"}))]);
        let (r, ..) = run_named(Use::Rename("x".into()), "web", |_, _| Err(("invalid", "`x` is taken".into())));
        assert_eq!(r.unwrap_err().to_string(), "`x` is taken");
    }

    fn list_of(v: Value) -> InstancesList {
        serde_json::from_value(v).unwrap()
    }

    /// A fleet root (no config of its own) stands for the config roots its
    /// rows name under it; an ssh machine's config root for itself, never
    /// for the machine's other roots its listing's rows name.
    #[test]
    fn a_root_stands_for_itself_or_the_config_roots_under_it() {
        let p = |s: &[&str]| s.iter().map(PathBuf::from).collect::<Vec<_>>();
        let rows = || vec![row("a", "c", "/f/b", false), row("b", "c", "/f/a", false), row("c", "c", "/f/a", false), row("d", "c", "/r", false)];
        let configless = Some("config: no devsandboxes.toml");
        assert_eq!(config_roots(Path::new("/f"), &list_of(listing(rows(), vec![], configless))), p(&["/f/a", "/f/b"]));
        assert_eq!(config_roots(Path::new("/f"), &list_of(listing(vec![], vec![], configless))), p(&["/f"]));
        assert_eq!(config_roots(Path::new("/r"), &list_of(listing(rows(), vec![], None))), p(&["/r"]));
        assert_eq!(config_roots(Path::new("/s"), &list_of(listing(rows(), vec![], None))), p(&["/s"]), "no instances there yet");
        // A config root with others nested in it: itself and those.
        assert_eq!(config_roots(Path::new("/f"), &list_of(listing(rows(), vec![], None))), p(&["/f", "/f/a", "/f/b"]));
    }

    fn run_per_root(u: Use, answer: impl FnMut(&str, &Value) -> Result<Value, (&'static str, String)>) -> (Result<i32>, String, String) {
        let mut f = fake(answer);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let r = per_root(&mut |m, p| f.call(m, p), &u, "lap", &[PathBuf::from("/f")], &mut out, &mut err);
        (r, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn ls_and_status_print_as_the_local_commands_per_config_root() {
        let sandboxes = || vec![sandbox("web", "image node", Some("app"), &["db", "redis"], &["base"]), sandbox("cli", "dockerfile D", None, &[], &[])];
        let answer = |m: &str, p: &Value| {
            assert_eq!(m, "instances.list");
            Ok(match p["dir"].as_str().unwrap() {
                "/f" => listing(vec![row("w", "c", "/f/cfg", false)], vec![], Some("config: no devsandboxes.toml")),
                _ => listing(vec![row("w", "c", "/f/cfg", false)], sandboxes(), None),
            })
        };
        let (r, out, err) = run_per_root(Use::Ls { json: false }, answer);
        assert_eq!(r.unwrap(), 0);
        assert_eq!(err, "==> lap:/f/cfg\n");
        let rows: Vec<crate::snapshot::SandboxRow> = serde_json::from_value(json!(sandboxes())).unwrap();
        let lines: Vec<_> = rows.iter().map(crate::commands::ls::Line::of_row).collect();
        assert_eq!(out, crate::commands::ls::table(&lines));
        assert_eq!(
            lines[0],
            crate::commands::ls::Line {
                name: "web".into(),
                source: "image node".into(),
                folder: Some("app".into()),
                services: Some("db, redis".into()),
                extends: Some("base".into())
            }
        );
        assert_eq!((lines[1].folder.as_deref(), lines[1].services.as_deref(), lines[1].extends.as_deref()), (None, None, None));

        let (_, out, _) = run_per_root(Use::Ls { json: true }, answer);
        assert_eq!(out, format!("{}\n", crate::commands::ls::render_json(&rows).unwrap()));
        let (_, out, _) = run_per_root(Use::Status, answer);
        let snapshot = list_of(listing(vec![row("w", "c", "/f/cfg", false)], sandboxes(), None)).snapshot;
        assert_eq!(out, format!("{}\n", crate::commands::status::render(&snapshot).unwrap()));
    }

    /// `ps` and `stats` print what the local commands print for the same rows.
    #[test]
    fn ps_and_stats_print_as_the_local_commands() {
        use crate::commands::ps::PsRow;
        use crate::runtime::{ContainerRow, StatsRow};
        let ps_rows = vec![
            PsRow { row: ContainerRow { name: "devsandbox-a".into(), image: "img".into(), status: "Up 1 hour".into(), ..Default::default() }, done: true, phase: None },
            PsRow {
                row: ContainerRow { name: "devsandbox-b".into(), image: "img".into(), status: "Up".into(), ..Default::default() },
                done: false,
                phase: Some(crate::snapshot::PhaseRow::Failed { step: "postStartCommand".into(), error: "exit 1".into() }),
            },
        ];
        let stats_rows = vec![StatsRow { name: "devsandbox-a".into(), cpu: "1.5%".into(), mem: "20MiB / 1GiB".into() }];
        let run = |u: Use| {
            let mut f = fake(|m: &str, _: &Value| {
                Ok(match m {
                    "containers.list" => json!({ "rows": ps_rows }),
                    _ => json!({ "rows": stats_rows }),
                })
            });
            let mut out = Vec::new();
            let r = super::listing(&mut |m, p| f.call(m, p), &u, "lap", &mut out);
            (r.unwrap(), String::from_utf8(out).unwrap(), f.calls)
        };
        for json in [false, true] {
            let (code, out, calls) = run(Use::Ps { all: true, json });
            assert_eq!((code, out), (0, crate::commands::ps::render(ps_rows.clone(), json).unwrap()));
            assert_eq!(calls, [("containers.list".into(), json!({"all": true, "machine": "lap"}))]);
            let (_, out, calls) = run(Use::Stats { json });
            assert_eq!(out, crate::commands::stats::render(stats_rows.clone(), json).unwrap());
            assert_eq!(calls, [("containers.stats".into(), json!({"machine": "lap"}))]);
        }
        let (_, out, _) = run(Use::Ps { all: false, json: false });
        assert!(out.contains("Up 1 hour (done)") && out.contains("failed (postStartCommand)"), "{out}");

        // A trust refusal is the error, naming the machine it's about.
        let denied = "`/x` isn't one of this member's roots (`devsandbox machine roots add <path>` there adds one)";
        let mut f = fake(|_: &str, _: &Value| Err(("denied", denied.into())));
        let r = super::listing(&mut |m, p| f.call(m, p), &Use::Ps { all: false, json: false }, "lap", &mut Vec::new());
        assert_eq!(r.unwrap_err().to_string(), format!("machine `lap`: {denied}"));
    }

    /// `service ls` prints the listing's service rows as the local one
    /// prints its own, the snapshot's error on stderr.
    #[test]
    fn service_ls_prints_as_the_local_command_per_config_root() {
        let services = json!([
            {"name": "db", "scope": "isolated", "source": "image postgres", "ports": ["5432"],
             "containers": [["devsandbox-svc-p-db-id", {"state": "running", "text": "Up 1 hour"}]], "used_by": ["w"],
             "env_len": 0, "command": null, "config_hash": "h", "drift": false, "root": "/f/cfg"},
            {"name": "cache", "scope": "global", "source": "?", "ports": [], "containers": [["devsandbox-svc-p-cache", {"state": "missing"}]],
             "used_by": [], "env_len": 0, "command": null, "config_hash": "", "drift": false, "root": "/f/cfg"},
        ]);
        let depths = std::cell::RefCell::new(Vec::new());
        let answer = |error: Option<&'static str>, services: Value| {
            let depths = &depths;
            move |_: &str, p: &Value| {
                depths.borrow_mut().push((p["dir"].clone(), p["depth"].clone()));
                // The fleet root has no config of its own.
                let error = if p["dir"] == "/f" { Some("config: no devsandboxes.toml") } else { error };
                let mut l = listing(vec![row("w", "c", "/f/cfg", false)], vec![], error);
                l["services"] = services.clone();
                Ok(l)
            }
        };
        let rows: Vec<crate::snapshot::ServiceRow> = serde_json::from_value(services.clone()).unwrap();
        let config = Path::new("/f/cfg/devsandboxes.toml");
        for json in [false, true] {
            let (r, out, err) = run_per_root(Use::ServiceLs { json }, answer(Some("service `cache`: no image"), services.clone()));
            assert_eq!(r.unwrap(), 0);
            assert_eq!(out, crate::commands::services::ls_render(&rows, json, config).unwrap());
            assert_eq!(err, "==> lap:/f/cfg\nservice `cache`: no image\n");
        }
        assert_eq!(depths.borrow().last().unwrap(), &(json!("/f/cfg"), json!("listing")));
        assert!(crate::commands::services::ls_render(&rows, false, config).unwrap().contains("running"));

        let (_, out, err) = run_per_root(Use::ServiceLs { json: false }, answer(None, json!([])));
        assert_eq!((out.as_str(), err.as_str()), ("no services defined in /f/cfg/devsandboxes.toml\n", "==> lap:/f/cfg\n"));
        let (r, ..) = run_per_root(Use::ServiceLs { json: true }, answer(Some("config: no devsandboxes.toml"), json!([])));
        assert_eq!(r.unwrap_err().to_string(), "lap:/f/cfg: config: no devsandboxes.toml");
    }

    #[test]
    fn ls_without_sandboxes_or_with_several_roots() {
        // Nothing placed: the fleet root itself, which has no config.
        let (r, ..) = run_per_root(Use::Ls { json: false }, |_, _| Ok(listing(vec![], vec![], Some("config: no devsandboxes.toml"))));
        assert_eq!(r.unwrap_err().to_string(), "lap:/f: config: no devsandboxes.toml");
        let (_, out, _) = run_per_root(Use::Ls { json: false }, |_, _| Ok(listing(vec![], vec![], None)));
        assert_eq!(out, "no sandboxes defined in /f\n");
        let two = |_: &str, p: &Value| {
            let error = (p["dir"] == "/f").then_some("config: no devsandboxes.toml");
            Ok(listing(vec![row("a", "c", "/f/a", false), row("b", "c", "/f/b", false)], vec![], error))
        };
        let (r, ..) = run_per_root(Use::Status, two);
        assert!(r.unwrap_err().to_string().contains("/f/a, /f/b"));
        let (r, _, err) = run_per_root(Use::Ls { json: false }, two);
        assert_eq!((r.unwrap(), err.as_str()), (0, "==> lap:/f/a\n==> lap:/f/b\n"));
    }
}
