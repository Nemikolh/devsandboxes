//! `ps`: the instances the dashboard's Instances tree shows
//! ([`InstanceRow`]s of a [`Snapshot`]), as a table, or the snapshot itself
//! as `--json`. It reads what the dashboard reads: the daemon's federated
//! snapshot of the root (`instances.watch {machines: true}`, one push waited
//! for), machines' rows included. Under `--in-process`, off unix, or with no
//! daemon to ask, this host's own collection instead.
//! The runtime being down is data, not a failure: the rows still come from
//! state and the snapshot's `error` says why.

use std::path::Path;
#[cfg(unix)]
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
#[cfg(unix)]
use serde_json::json;

use crate::commands::Envelope;
use crate::render::{humanize_secs, table};
use crate::snapshot::{collect_from, ContainerStatus, Depth, InstanceRow, PhaseRow, RuntimeView, Snapshot};
#[cfg(unix)]
use crate::serve::{
    client::Conn,
    proto::{Notification, Response},
    snapshots::{LinkState, SnapshotMachine},
};

/// MACHINE of this host's rows: `@`, as `@/<name>` names one of them.
pub(crate) const LOCAL_MACHINE: &str = "@";

pub fn ps(dir: &Path, all: bool, json: bool) -> Result<()> {
    #[cfg(unix)]
    if !crate::serve::client::in_process() {
        match from_daemon(dir) {
            Ok(params) => return show_push(params, all, json),
            Err(why) => eprintln!("warning: devsandbox serve: {why} (listing this host only)"),
        }
    }
    let view = RuntimeView::gather(Depth::Full);
    let snapshot = collect_from(dir, &view);
    if !json {
        no_state(&snapshot)?;
        for e in &view.errors {
            eprintln!("warning: {e}");
        }
    }
    print!("{}", render(snapshot, &[], all, json)?);
    Ok(())
}

/// Without the state there are no rows to show.
fn no_state(snapshot: &Snapshot) -> Result<()> {
    if let Some(e) = snapshot.error.as_deref().filter(|e| e.starts_with("state: ")) {
        bail!("{e}");
    }
    Ok(())
}

/// How long `ps` waits for a complete push, connecting included.
#[cfg(unix)]
const WAIT: Duration = Duration::from_secs(5);

/// The root's federated push, through the daemon (started if needed); why
/// not when there's none to show.
#[cfg(unix)]
fn from_daemon(dir: &Path) -> Result<Value, String> {
    let deadline = Instant::now() + WAIT;
    // The daemon's cwd isn't ours: `instances.watch` takes absolute roots.
    let dir = dir.canonicalize().or_else(|_| std::path::absolute(dir)).unwrap_or_else(|_| dir.to_path_buf());
    let mut conn = crate::serve::endpoint::socket_dir()
        .and_then(|sock| crate::serve::client::connect(&sock, "cli"))
        .map_err(|e| format!("{e:#}"))?;
    watch_once(&mut conn, &dir, deadline, &Instant::now)
}

/// Print an `instances.snapshot`'s params as `ps` prints its own
/// collection, each machine's trouble a `warning:` line.
#[cfg(unix)]
fn show_push(mut params: Value, all: bool, json: bool) -> Result<()> {
    let machines = match params.as_object_mut().and_then(|o| o.remove("machines")) {
        Some(Value::Array(machines)) => machines,
        _ => Vec::new(),
    };
    let list: crate::serve::api::InstancesList =
        serde_json::from_value(params).context("bad instances.snapshot from devsandbox serve")?;
    let snapshot = list.snapshot;
    if !json {
        no_state(&snapshot)?;
        for e in runtime_errors(&snapshot) {
            eprintln!("warning: {e}");
        }
        if snapshot.depth == Depth::Disk && !snapshot.no_runtime {
            eprintln!("warning: the runtime wasn't listed in time: statuses unknown");
        }
    }
    for line in machine_warnings(&machines) {
        eprintln!("{line}");
    }
    print!("{}", render(snapshot, &machines, all, json)?);
    Ok(())
}

/// The runtime's parts of the snapshot's `error`, as the local collection
/// warns with them: a config error is about the root, which `ps` lists
/// regardless of.
#[cfg(unix)]
fn runtime_errors(snapshot: &Snapshot) -> Vec<&str> {
    let runtime = format!("{} ", snapshot.runtime_name);
    snapshot.error.as_deref().into_iter().flat_map(|e| e.split("; ")).filter(|e| e.starts_with(&runtime)).collect()
}

/// What [`watch_once`] needs of a daemon connection, so tests feed it
/// scripted answers.
#[cfg(unix)]
pub(crate) trait Watcher {
    fn call(&mut self, method: &str, params: Value) -> Result<Response>;
    fn next_notification(&mut self, timeout: Duration) -> Result<Option<Notification>>;
}

#[cfg(unix)]
impl Watcher for Conn {
    /// Bounded: a daemon that stops answering can't hang `ps`.
    fn call(&mut self, method: &str, params: Value) -> Result<Response> {
        self.call_within(method, params, WAIT)?.with_context(|| format!("devsandbox serve didn't answer {method}"))
    }

    fn next_notification(&mut self, timeout: Duration) -> Result<Option<Notification>> {
        Conn::next_notification(self, timeout)
    }
}

/// Subscribe to `instances`, watch root `dir` with machines, and keep the
/// `instances.snapshot` pushes until one is [`complete`] or `deadline`
/// passes (`now` the clock); then unwatch (best effort) and return the last
/// push. `Err`: why there's none (a refused subscribe or watch, a lost
/// connection, no push in time), for the local fallback's warning.
#[cfg(unix)]
pub(crate) fn watch_once(conn: &mut dyn Watcher, dir: &Path, deadline: Instant, now: &dyn Fn() -> Instant) -> Result<Value, String> {
    let ask = |conn: &mut dyn Watcher, method: &str, params: Value| match conn.call(method, params) {
        Err(e) => Err(format!("{e:#}")),
        Ok(Response { error: Some(e), .. }) => Err(format!("{method}: {}", e.message)),
        Ok(_) => Ok(()),
    };
    ask(conn, "subscribe", json!({ "topics": ["instances"] }))?;
    let dir = dir.to_string_lossy();
    ask(conn, "instances.watch", json!({ "dir": dir, "machines": true }))?;
    let (mut last, mut lost) = (None, None);
    loop {
        let left = deadline.saturating_duration_since(now());
        if left.is_zero() {
            break;
        }
        match conn.next_notification(left) {
            Ok(Some(n)) if n.method == "instances.snapshot" => {
                let done = complete(&n.params);
                last = Some(n.params);
                if done {
                    break;
                }
            }
            Ok(_) => {}
            Err(e) => {
                lost = Some(format!("{e:#}"));
                break;
            }
        }
    }
    let _ = conn.call("instances.unwatch", json!({ "dir": dir }));
    last.ok_or_else(|| lost.unwrap_or_else(|| format!("no snapshot within {}s", WAIT.as_secs())))
}

/// A push worth printing: this host's rows have their statuses and cpu/mem
/// (`depth` `full`, which the daemon also says when it carried them over;
/// or nothing more is coming: no runtime installed, or a listing that
/// failed with the runtime down, where no stats pass follows), and every
/// machine is [`settled`].
#[cfg(unix)]
pub(crate) fn complete(params: &Value) -> bool {
    let depth: Depth = params.get("depth").cloned().and_then(|d| serde_json::from_value(d).ok()).unwrap_or_default();
    let no_runtime = params.get("no_runtime").and_then(Value::as_bool) == Some(true);
    let error = params.get("error").is_some_and(|e| !e.is_null());
    let here = no_runtime || depth == Depth::Full || (depth == Depth::Listing && error);
    here && machines_of(params).iter().all(settled)
}

#[cfg(unix)]
fn machines_of(params: &Value) -> Vec<SnapshotMachine> {
    let entries = params.get("machines").and_then(Value::as_array).into_iter().flatten();
    entries.filter_map(|m| serde_json::from_value(m.clone()).ok()).collect()
}

/// A machine whose rows won't get better by waiting: it has an error, or
/// it's connected with every root's statuses in (a host with no runtime
/// never lists: its rows in are as far as they go).
#[cfg(unix)]
fn settled(m: &SnapshotMachine) -> bool {
    m.error.is_some() || (m.state == LinkState::Connected && m.depth.is_some_and(|d| d >= Depth::Listing || m.runtime.is_none()))
}

/// One `warning:` per machine with an error or not [`settled`].
#[cfg(unix)]
fn machine_warnings(machines: &[Value]) -> Vec<String> {
    let machines: Vec<SnapshotMachine> = machines.iter().filter_map(|m| serde_json::from_value(m.clone()).ok()).collect();
    machines
        .iter()
        .filter_map(|m| {
            let why = match &m.error {
                // The link's messages start with the machine's name.
                Some(e) => e.strip_prefix(m.name.as_str()).and_then(|e| e.strip_prefix(": ")).unwrap_or(e.as_str()),
                None if settled(m) => return None,
                None if m.state == LinkState::Connected => "its rows aren't in yet",
                None => "not connected yet",
            };
            Some(format!("warning: machine `{}`: {why}", m.name))
        })
        .collect()
}

/// `ps --json`'s data: the snapshot, plus the federated push's `machines`
/// when it has any.
#[derive(Serialize)]
struct Listed<'a> {
    #[serde(flatten)]
    snapshot: &'a Snapshot,
    #[serde(skip_serializing_if = "no_machines")]
    machines: &'a [Value],
}

fn no_machines(machines: &&[Value]) -> bool {
    machines.is_empty()
}

/// What `ps` prints for `snapshot`: its instances cut and sorted ([`rows`]),
/// as the `--json` envelope (one line ending, `machines` beside the
/// snapshot's fields) or the table ([`table_of`], a MACHINE column when
/// there are `machines`). Shared with the run on a machine
/// (`machines::cli::calls`).
pub(crate) fn render(mut snapshot: Snapshot, machines: &[Value], all: bool, json: bool) -> Result<String> {
    snapshot.instances = rows(std::mem::take(&mut snapshot.instances), all);
    if json {
        let out = serde_json::to_string_pretty(&Envelope::new(Listed { snapshot: &snapshot, machines })).context("serialize ps")?;
        return Ok(format!("{out}\n"));
    }
    Ok(table_of(&snapshot.instances, !machines.is_empty()))
}

/// The running instances, plus those a `run`/`rebuild` is materializing
/// (no container up yet); every one with `all`. This host's first, then by
/// machine, each by name.
pub(crate) fn rows(mut rows: Vec<InstanceRow>, all: bool) -> Vec<InstanceRow> {
    rows.retain(|r| all || matches!(r.status, ContainerStatus::Running(_)) || matches!(r.phase, Some(PhaseRow::InFlight { .. })));
    rows.sort_by(|a, b| (&a.machine, &a.name).cmp(&(&b.machine, &b.name)));
    rows
}

/// The table: `NAME SANDBOX STATUS UPTIME CPU MEM`, then PROFILE when some
/// row has a profile and MACHINE when `machines`. STATUS is
/// [`InstanceRow::status_label`], ` (done)` added for a done instance;
/// PROFILE `<p> (not allowed)` when its sandbox no longer allows it.
fn table_of(rows: &[InstanceRow], machines: bool) -> String {
    let profiles = rows.iter().any(|r| r.profile.is_some());
    let mut headers = vec!["NAME", "SANDBOX", "STATUS", "UPTIME", "CPU", "MEM"];
    if profiles {
        headers.push("PROFILE");
    }
    if machines {
        headers.push("MACHINE");
    }
    let dash = || "-".to_string();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let status = r.status_label();
            let status = if r.done { format!("{status} (done)") } else { status.to_string() };
            let uptime = match r.status {
                ContainerStatus::Running(_) => humanize_secs(r.uptime_secs),
                _ => dash(),
            };
            let mut row =
                vec![r.name.clone(), r.sandbox.clone(), status, uptime, r.cpu.clone().unwrap_or_else(dash), r.mem.clone().unwrap_or_else(dash)];
            if profiles {
                row.push(match (&r.profile, r.profile_disallowed) {
                    (Some(p), true) => format!("{p} (not allowed)"),
                    (Some(p), false) => p.clone(),
                    (None, _) => String::new(),
                });
            }
            if machines {
                row.push(r.machine.clone().unwrap_or_else(|| LOCAL_MACHINE.to_string()));
            }
            row
        })
        .collect();
    // An empty PROFILE last would leave MEM's padding behind.
    table(&headers, &cells).lines().map(|l| format!("{}\n", l.trim_end())).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn row(name: &str, status: ContainerStatus) -> InstanceRow {
        InstanceRow {
            name: name.into(),
            sandbox: "web".into(),
            container: format!("devsandbox-{name}"),
            status,
            uptime_secs: 2 * 3600 + 5 * 60,
            cpu: None,
            mem: None,
            folder: "/w".into(),
            worktree: false,
            services: Vec::new(),
            workspace: "/w".into(),
            remote_user: None,
            remote_env_len: 0,
            base_folder: "/w".into(),
            drift: false,
            instance_id: String::new(),
            dispatcher: None,
            done: false,
            profile: None,
            profile_disallowed: false,
            browser_sandbox: None, github_repos: Vec::new(), build_notes: Vec::new(),
            phase: None,
            root: None,
            mesh_root: None,
            machine: None,
        }
    }

    fn up(name: &str) -> InstanceRow {
        row(name, ContainerStatus::Running("Up 2 hours".into()))
    }

    fn snapshot(instances: Vec<InstanceRow>) -> Snapshot {
        Snapshot {
            instances,
            sandboxes: Vec::new(),
            services: Vec::new(),
            sandbox_count: 0,
            runtime_name: "docker".into(),
            runtime_version: None,
            collected_at: Instant::now(),
            depth: crate::snapshot::Depth::Full,
            error: None,
            no_runtime: false,
        }
    }

    fn names(rows: &[InstanceRow]) -> Vec<(Option<&str>, &str)> {
        rows.iter().map(|r| (r.machine.as_deref(), r.name.as_str())).collect()
    }

    /// Running and in-flight by default, everything with `all`; this host's
    /// rows first, then each machine's, by name.
    #[test]
    fn rows_are_the_running_ones_unless_all() {
        let mut building = row("b", ContainerStatus::Missing);
        building.phase = Some(PhaseRow::InFlight { step: "building".into() });
        let mut failed = row("f", ContainerStatus::Exited("Exited (1)".into()));
        failed.phase = Some(PhaseRow::Failed { step: "postStartCommand".into(), error: "exit 1".into() });
        let mut remote = up("a");
        remote.machine = Some("lap".into());
        let all = || vec![up("z"), remote.clone(), row("s", ContainerStatus::Exited("Exited (0)".into())), building.clone(), failed.clone(), row("m", ContainerStatus::Missing)];
        assert_eq!(names(&rows(all(), false)), [(None, "b"), (None, "z"), (Some("lap"), "a")]);
        assert_eq!(names(&rows(all(), true)), [(None, "b"), (None, "f"), (None, "m"), (None, "s"), (None, "z"), (Some("lap"), "a")]);
    }

    #[test]
    fn the_table_shows_status_uptime_and_usage() {
        let mut done = up("a");
        done.done = true;
        done.cpu = Some("1.5%".into());
        done.mem = Some("20MiB / 1GiB".into());
        let mut building = row("b", ContainerStatus::Missing);
        building.phase = Some(PhaseRow::InFlight { step: "building".into() });
        let stopped = row("c", ContainerStatus::Exited("Exited (0)".into()));
        let out = render(snapshot(vec![done, building, stopped]), &[], true, false).unwrap();
        assert_eq!(
            out,
            "NAME  SANDBOX  STATUS          UPTIME  CPU   MEM\n\
             a     web      running (done)  2h05m   1.5%  20MiB / 1GiB\n\
             b     web      building        -       -     -\n\
             c     web      exited          -       -     -\n"
        );
    }

    /// PROFILE only when some row has one; MACHINE only with `machines`
    /// entries, `@` for this host's rows.
    #[test]
    fn profile_and_machine_columns_show_when_they_apply() {
        let mut a = up("a");
        a.profile = Some("work".into());
        let mut b = up("b");
        b.profile = Some("work".into());
        b.profile_disallowed = true;
        b.machine = Some("lap".into());
        let c = up("c");
        let out = render(snapshot(vec![a, b, c.clone()]), &[serde_json::json!({"name": "lap"})], false, false).unwrap();
        assert_eq!(
            out,
            "NAME  SANDBOX  STATUS   UPTIME  CPU  MEM  PROFILE             MACHINE\n\
             a     web      running  2h05m   -    -    work                @\n\
             c     web      running  2h05m   -    -                        @\n\
             b     web      running  2h05m   -    -    work (not allowed)  lap\n"
        );
        let out = render(snapshot(vec![c.clone()]), &[], false, false).unwrap();
        assert_eq!(out, "NAME  SANDBOX  STATUS   UPTIME  CPU  MEM\nc     web      running  2h05m   -    -\n");
        let mut p = c;
        p.profile = Some("home".into());
        let out = render(snapshot(vec![p, up("d")]), &[], false, false).unwrap();
        assert!(out.lines().all(|l| l == l.trim_end()), "no trailing padding: {out:?}");
    }

    /// `--json` is the snapshot's envelope, its instances cut as the table's.
    #[test]
    fn json_is_the_snapshot_envelope_with_the_shown_instances() {
        let rows = vec![up("up"), row("gone", ContainerStatus::Missing)];
        let json: serde_json::Value = serde_json::from_str(&render(snapshot(rows.clone()), &[], false, true).unwrap()).unwrap();
        assert_eq!(json["schema"], 1);
        let names: Vec<&str> = json["data"]["instances"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["up"]);
        assert_eq!(json["data"]["instances"][0]["status"], serde_json::json!({"state": "running", "text": "Up 2 hours"}));
        assert!(json["data"].get("collected_at").is_none());
        let json: serde_json::Value = serde_json::from_str(&render(snapshot(rows.clone()), &[], true, true).unwrap()).unwrap();
        assert_eq!(json["data"]["instances"][0]["status"], serde_json::json!({"state": "missing"}), "sorted by name");
        assert_eq!(json["data"]["depth"], "full");
        assert!(json["data"].get("machines").is_none(), "no machines: none");
    }

    #[cfg(unix)]
    mod watch {
        use std::cell::Cell;
        use std::collections::VecDeque;
        use std::rc::Rc;

        use serde_json::json;

        use super::super::*;

        fn machine(name: &str, state: &str, error: Option<&str>, depth: Option<&str>) -> Value {
            json!({"name": name, "kind": "ssh", "target": format!("me@{name}"), "state": state, "error": error,
                "behind": null, "behind_here": null, "version": null, "runtime": "docker", "depth": depth,
                "command": null, "agent": false, "capacity": null, "via": null, "exposed_by": null})
        }

        fn push(depth: &str, machines: Vec<Value>) -> Value {
            json!({"dir": "/cfg", "instances": [], "sandboxes": [], "services": [], "sandbox_count": 0,
                "runtime_name": "docker", "runtime_version": null, "error": null, "no_runtime": false,
                "depth": depth, "thread_children": {}, "machines": machines})
        }

        fn note(params: Value) -> Result<Option<Notification>, String> {
            Ok(Some(Notification { method: "instances.snapshot".into(), params }))
        }

        /// Scripted notifications; an empty script or `Ok(None)` waits out
        /// the timeout on the fake clock.
        struct Fake {
            calls: Vec<(String, Value)>,
            refuse: Option<&'static str>,
            notes: VecDeque<Result<Option<Notification>, String>>,
            clock: Rc<Cell<Instant>>,
        }

        impl Watcher for Fake {
            fn call(&mut self, method: &str, params: Value) -> Result<Response> {
                self.calls.push((method.into(), params));
                match self.refuse {
                    Some(m) if m == method => Ok(Response::err(None, "invalid", "no")),
                    Some("io") => bail!("closed"),
                    _ => Ok(Response::ok(None, json!({"ok": true}))),
                }
            }

            fn next_notification(&mut self, timeout: Duration) -> Result<Option<Notification>> {
                match self.notes.pop_front() {
                    Some(Ok(Some(n))) => Ok(Some(n)),
                    Some(Err(e)) => bail!("{e}"),
                    Some(Ok(None)) | None => {
                        self.clock.set(self.clock.get() + timeout);
                        Ok(None)
                    }
                }
            }
        }

        fn run(refuse: Option<&'static str>, notes: Vec<Result<Option<Notification>, String>>) -> (Result<Value, String>, Fake, Instant) {
            let t0 = Instant::now();
            let clock = Rc::new(Cell::new(t0));
            let mut fake = Fake { calls: Vec::new(), refuse, notes: notes.into(), clock: Rc::clone(&clock) };
            let got = watch_once(&mut fake, Path::new("/cfg"), t0 + WAIT, &|| clock.get());
            (got, fake, t0)
        }

        fn methods(f: &Fake) -> Vec<&str> {
            f.calls.iter().map(|(m, _)| m.as_str()).collect()
        }

        #[test]
        fn a_complete_first_push_is_taken_at_once_and_unwatched() {
            let (got, fake, t0) = run(None, vec![note(push("full", vec![])), note(push("disk", vec![]))]);
            assert_eq!(got.unwrap()["depth"], "full");
            assert_eq!(methods(&fake), ["subscribe", "instances.watch", "instances.unwatch"]);
            assert_eq!(fake.calls[0].1, json!({"topics": ["instances"]}));
            assert_eq!(fake.calls[1].1, json!({"dir": "/cfg", "machines": true}));
            assert_eq!(fake.calls[2].1, json!({"dir": "/cfg"}));
            assert_eq!(fake.notes.len(), 1, "stopped at the first complete push");
            assert_eq!(fake.clock.get(), t0, "no waiting");
        }

        #[test]
        fn a_disk_only_push_is_waited_through() {
            let other = Ok(Some(Notification { method: "instances.changed".into(), params: json!({}) }));
            let (got, fake, _) = run(None, vec![note(push("disk", vec![])), other, note(push("listing", vec![])), note(push("full", vec![]))]);
            assert_eq!(got.unwrap()["depth"], "full");
            assert!(fake.notes.is_empty());
            assert_eq!(methods(&fake).last(), Some(&"instances.unwatch"));
        }

        /// Past the deadline: the last push, and its unsettled machine a warning.
        #[test]
        fn the_deadline_takes_the_last_push_with_a_machine_still_connecting() {
            let connecting = push("full", vec![machine("box", "connecting", None, None), machine("lap", "connected", None, Some("full"))]);
            let (got, fake, t0) = run(None, vec![note(connecting.clone())]);
            let got = got.unwrap();
            assert_eq!(got, connecting);
            assert!(fake.clock.get() >= t0 + WAIT);
            assert_eq!(methods(&fake).last(), Some(&"instances.unwatch"));
            let machines = got["machines"].as_array().unwrap();
            assert_eq!(machine_warnings(machines), ["warning: machine `box`: not connected yet"]);
        }

        #[test]
        fn a_refusal_or_a_lost_connection_is_the_fallbacks_reason() {
            let (got, fake, _) = run(Some("subscribe"), vec![]);
            assert_eq!(got.unwrap_err(), "subscribe: no");
            assert_eq!(methods(&fake), ["subscribe"]);
            let (got, fake, _) = run(Some("instances.watch"), vec![]);
            assert_eq!(got.unwrap_err(), "instances.watch: no");
            assert_eq!(methods(&fake), ["subscribe", "instances.watch"]);
            let (got, _, _) = run(Some("io"), vec![]);
            assert_eq!(got.unwrap_err(), "closed");
            // Lost before any push: why; after one: that push.
            let (got, fake, _) = run(None, vec![Err("devsandbox serve closed the connection".into())]);
            assert_eq!(got.unwrap_err(), "devsandbox serve closed the connection");
            assert_eq!(methods(&fake).last(), Some(&"instances.unwatch"), "best effort");
            let (got, _, _) = run(None, vec![note(push("disk", vec![])), Err("gone".into())]);
            assert_eq!(got.unwrap()["depth"], "disk");
            let (got, _, _) = run(None, vec![]);
            assert_eq!(got.unwrap_err(), "no snapshot within 5s");
        }

        #[test]
        fn completeness_needs_this_hosts_stats_and_every_machine_settled() {
            assert!(!complete(&push("listing", vec![])));
            let mut down = push("listing", vec![]);
            down["error"] = json!("docker unavailable: refused");
            assert!(complete(&down), "runtime down: no stats pass follows");
            let mut driver = push("disk", vec![]);
            driver["no_runtime"] = json!(true);
            assert!(complete(&driver), "no runtime: nothing more to ask");
            let unreachable = machine("box", "unreachable", Some("box: Connection refused"), None);
            assert!(complete(&push("full", vec![unreachable.clone(), machine("lap", "connected", None, Some("listing"))])));
            assert!(!complete(&push("full", vec![machine("lap", "connected", None, None)])), "rows not in");
            assert!(!complete(&push("full", vec![machine("lap", "connected", None, Some("disk"))])));
            let mut no_runtime = machine("drv", "connected", None, Some("disk"));
            no_runtime["runtime"] = Value::Null;
            assert!(complete(&push("full", vec![no_runtime])), "a driver machine never lists");
            assert_eq!(
                machine_warnings(&[unreachable, machine("lap", "connected", None, None)]),
                ["warning: machine `box`: Connection refused", "warning: machine `lap`: its rows aren't in yet"]
            );
        }

        /// `--json`'s data with machines: the federated push's entries beside
        /// the snapshot, as `PsSnapshot` types them; MACHINE in the table.
        #[test]
        fn a_federated_push_renders_with_its_machines() {
            let mut params = push("full", vec![machine("box", "unreachable", Some("x"), None)]);
            let machines = params.as_object_mut().unwrap().remove("machines").unwrap();
            let machines = machines.as_array().unwrap();
            let list: crate::serve::api::InstancesList = serde_json::from_value(params).unwrap();
            let out = render(list.snapshot.clone(), machines, true, true).unwrap();
            let json: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(json["data"]["machines"][0]["name"], "box");
            crate::serve::dts::assert_matches("PsSnapshot", &json["data"]);
            assert!(render(list.snapshot, machines, true, false).unwrap().starts_with("NAME  SANDBOX  STATUS  UPTIME  CPU  MEM  MACHINE"));
        }
    }
}
