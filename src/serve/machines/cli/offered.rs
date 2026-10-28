//! CLI re-runs to an offered fleet worker (docs/architecture/daemon/machines.md,
//! *The CLI*; fleet plan decision 15): API calls through this host's daemon
//! with `machine` set (the quick calls, `machines::forward::QUICK`), never
//! a `devsandbox …` run there, printed by the local commands' own render
//! fns so the output reads as theirs. What has no such call yet is refused
//! ([`refusal`]).
//!
//! [`use_of`] is the pure choice; [`named`] and [`per_root`] make the calls
//! through an injected [`Call`] and write to injected streams, so the tests
//! feed them fixture answers.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::serve::api::{ContainerLogs, InstancesList, MAX_LINES, Renamed};
use crate::serve::proto::Response;
use crate::snapshot::InstanceRow;
use crate::{Command, ServiceCommand};

/// One daemon request: method, params → its response.
pub(crate) type Call<'a> = dyn FnMut(&str, Value) -> Result<Response> + 'a;

/// What a command does on an offered machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Use {
    /// `logs` (no `--build`): `containers.logs` of the instance's container.
    Logs { lines: usize },
    /// `logs --build`: `instances.buildLog`.
    BuildLog,
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
    /// As on any machine: what goes over the fleet link already (a tty
    /// `exec`, its sessions, `port`) and the verbs.
    Link,
    Refused(Refused),
}

/// What an offered machine refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    Ps,
    Stats,
    Gc,
    ServiceLs,
    LogsFollow,
    /// An `exec` that gets no tty (piped, or a command without `-t`): only
    /// session execs (`PTY`) go over the link.
    ExecNoTty,
    Vscode,
}

/// [`Use`] of `command` on an offered machine. A non-tty `exec` is
/// [`Use::Link`] here: whether it gets a tty is the caller's to tell
/// (`cli::reconnect_session`), and [`Refused::ExecNoTty`] its refusal.
pub(crate) fn use_of(command: &Command) -> Use {
    match command {
        Command::Logs { follow: true, .. } => Use::Refused(Refused::LogsFollow),
        Command::Logs { build: true, .. } => Use::BuildLog,
        Command::Logs { lines, .. } => Use::Logs { lines: *lines },
        Command::Inspect { json, .. } => Use::Inspect { json: *json },
        Command::Done { .. } => Use::Done(true),
        Command::Undone { .. } => Use::Done(false),
        Command::Rename { new_name, .. } => Use::Rename(new_name.clone()),
        Command::Ls { json } => Use::Ls { json: *json },
        Command::Status { .. } => Use::Status,
        Command::Ps { .. } => Use::Refused(Refused::Ps),
        Command::Stats { .. } => Use::Refused(Refused::Stats),
        Command::Gc { .. } => Use::Refused(Refused::Gc),
        Command::Service { cmd: ServiceCommand::Ls { .. } } => Use::Refused(Refused::ServiceLs),
        Command::Vscode { .. } => Use::Refused(Refused::Vscode),
        _ => Use::Link,
    }
}

/// Why offered machine `m` refuses `what`.
pub(crate) fn refusal(m: &str, what: Refused) -> anyhow::Error {
    let what = match what {
        Refused::Gc => {
            return anyhow::anyhow!(
                "machine `{m}` is an offered fleet worker: `gc` there is host maintenance, outside a fleet worker's trust"
            );
        }
        Refused::Ps => "`ps`",
        Refused::Stats => "`stats`",
        Refused::ServiceLs => "`service ls`",
        Refused::LogsFollow => "`logs --follow`",
        Refused::ExecNoTty => "an `exec` without a tty",
        Refused::Vscode => "`vscode`",
    };
    anyhow::anyhow!("machine `{m}` is an offered fleet worker: {what} isn't available over a fleet link yet")
}

/// `method` with `params` (plus `machine: m`) through `call`: its result, or
/// its error's message as ours.
fn call_on<T: DeserializeOwned>(call: &mut Call, m: &str, method: &str, mut params: Value) -> Result<T> {
    params["machine"] = json!(m);
    let r = call(method, params)?;
    if let Some(e) = r.error {
        bail!("{}", e.message);
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
/// a snapshot lists every instance the worker lends, whatever its `dir`).
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
/// instance or container `name` of offered machine `m`, `root` its config
/// root there (or the machine's root). `tty`: stdout is a terminal (a pty
/// build log keeps its colours). The exit code.
pub(crate) fn named(call: &mut Call, u: &Use, m: &str, name: &str, root: &Path, tty: bool, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    match u {
        Use::Logs { lines } => {
            let container = container_of(row_of(call, m, root, name)?.as_ref(), name);
            let logs: ContainerLogs = call_on(call, m, "containers.logs", json!({ "container": container, "lines": lines }))?;
            writeln!(out, "{}", crate::commands::logs::render_logs(&logs.text))?;
        }
        Use::BuildLog => {
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
        Use::Ls { .. } | Use::Status | Use::Link | Use::Refused(_) => bail!("not a named re-run"),
    }
    Ok(0)
}

/// The config roots a machine root `at` stands for, from a listing of it:
/// an offered worker's one root is its fleet root, whose config roots (one
/// per placed config) its instance rows name. None named: `at` itself.
pub(crate) fn config_roots(at: &Path, list: &InstancesList) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = list.snapshot.instances.iter().filter_map(|r| r.root.as_ref().map(PathBuf::from)).collect();
    roots.sort();
    roots.dedup();
    if roots.is_empty() {
        roots.push(at.to_path_buf());
    }
    roots
}

/// `--machine m ls|status` (`u`) on offered machine `m` with registered
/// `roots`: one `instances.list` per config root ([`config_roots`]), each
/// after its banner on `err` (`cli::per_root`'s rules: `--json` with
/// several roots is an error). The exit code.
pub(crate) fn per_root(call: &mut Call, u: &Use, m: &str, roots: &[PathBuf], out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    let json = match u {
        Use::Ls { json } => *json,
        Use::Status => true,
        _ => bail!("not a per-root re-run"),
    };
    let mut config = Vec::new();
    for at in roots {
        config.extend(config_roots(at, &list(call, m, at, Some("disk"))?));
    }
    // Same checks (no roots, `--json` with several) and banners as an ssh machine's runs.
    let runs = super::per_root(m, &config, &[], json)?;
    for (root, (banner, _)) in config.iter().zip(runs) {
        writeln!(err, "{banner}")?;
        match u {
            Use::Status => writeln!(out, "{}", crate::commands::status::render(&list(call, m, root, None)?.snapshot)?)?,
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
        assert_eq!(u(&["logs", "--build", "lap/web"]), Use::BuildLog);
        assert_eq!(u(&["logs", "--build", "-f", "lap/web"]), Use::Refused(Refused::LogsFollow));
        assert_eq!(u(&["inspect", "lap/web"]), Use::Inspect { json: false });
        assert_eq!(u(&["inspect", "--json", "lap/web"]), Use::Inspect { json: true });
        assert_eq!(u(&["done", "lap/web"]), Use::Done(true));
        assert_eq!(u(&["undone", "lap/web"]), Use::Done(false));
        assert_eq!(u(&["rename", "lap/web", "api"]), Use::Rename("api".into()));
        assert_eq!(u(&["ls"]), Use::Ls { json: false });
        assert_eq!(u(&["ls", "--json"]), Use::Ls { json: true });
        assert_eq!(u(&["status", "--json"]), Use::Status);
        assert_eq!(u(&["ps", "-a"]), Use::Refused(Refused::Ps));
        assert_eq!(u(&["stats"]), Use::Refused(Refused::Stats));
        assert_eq!(u(&["gc", "--force"]), Use::Refused(Refused::Gc));
        assert_eq!(u(&["service", "ls"]), Use::Refused(Refused::ServiceLs));
        assert_eq!(u(&["vscode", "lap/web"]), Use::Refused(Refused::Vscode));
        for args in [&["exec", "lap/web"][..], &["exec", "lap/web", "--", "ls"], &["port", "lap/web", "3000"], &["stop", "lap/web"]] {
            assert_eq!(u(args), Use::Link, "{args:?}");
        }
    }

    #[test]
    fn refusals_name_the_command_and_why() {
        let r = |what| refusal("lap", what).to_string();
        for (what, label) in [
            (Refused::Ps, "`ps`"),
            (Refused::Stats, "`stats`"),
            (Refused::ServiceLs, "`service ls`"),
            (Refused::LogsFollow, "`logs --follow`"),
            (Refused::ExecNoTty, "an `exec` without a tty"),
            (Refused::Vscode, "`vscode`"),
        ] {
            assert_eq!(r(what), format!("machine `lap` is an offered fleet worker: {label} isn't available over a fleet link yet"));
        }
        assert_eq!(r(Refused::Gc), "machine `lap` is an offered fleet worker: `gc` there is host maintenance, outside a fleet worker's trust");
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
        let (r, out, err, calls) = run_named(Use::BuildLog, "web", |_, _| Ok(json!({"text": "$ run\nok", "cut": true})));
        assert_eq!((r.unwrap(), out.as_str()), (0, "$ run\nok\n"));
        assert_eq!(err, format!("(the last {MAX_LINES} lines of `lap/web`'s build log)\n"));
        assert_eq!(calls, [("instances.buildLog".into(), json!({"instance": "web", "lines": MAX_LINES, "ansi": false, "machine": "lap"}))]);
        let (r, ..) = run_named(Use::BuildLog, "web", |_, _| Ok(Value::Null));
        assert_eq!(r.unwrap_err().to_string(), "`lap/web` has no build log");
        let (r, ..) = run_named(Use::BuildLog, "web", |_, _| Err(("not-found", "no instance `web`".into())));
        assert_eq!(r.unwrap_err().to_string(), "no instance `web`");
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

    #[test]
    fn a_fleet_root_stands_for_its_rows_config_roots() {
        let rows = vec![row("a", "c", "/f/b", false), row("b", "c", "/f/a", false), row("c", "c", "/f/a", false)];
        assert_eq!(config_roots(Path::new("/f"), &list_of(listing(rows, vec![], None))), [PathBuf::from("/f/a"), PathBuf::from("/f/b")]);
        assert_eq!(config_roots(Path::new("/f"), &list_of(listing(vec![], vec![], None))), [PathBuf::from("/f")]);
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

    #[test]
    fn ls_without_sandboxes_or_with_several_roots() {
        // Nothing placed: the fleet root itself, which has no config.
        let (r, ..) = run_per_root(Use::Ls { json: false }, |_, _| Ok(listing(vec![], vec![], Some("config: no devsandboxes.toml"))));
        assert_eq!(r.unwrap_err().to_string(), "lap:/f: config: no devsandboxes.toml");
        let (_, out, _) = run_per_root(Use::Ls { json: false }, |_, _| Ok(listing(vec![], vec![], None)));
        assert_eq!(out, "no sandboxes defined in /f\n");
        let two = |_: &str, _: &Value| Ok(listing(vec![row("a", "c", "/f/a", false), row("b", "c", "/f/b", false)], vec![], None));
        let (r, ..) = run_per_root(Use::Status, two);
        assert!(r.unwrap_err().to_string().contains("/f/a, /f/b"));
        let (r, _, err) = run_per_root(Use::Ls { json: false }, two);
        assert_eq!((r.unwrap(), err.as_str()), (0, "==> lap:/f/a\n==> lap:/f/b\n"));
    }
}
