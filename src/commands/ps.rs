//! `ps`: the instances the dashboard's Instances tree shows
//! ([`InstanceRow`]s of a [`Snapshot`]), as a table, or the snapshot itself
//! as `--json`. With machines registered, their rows join this host's.
//! The runtime being down is data, not a failure: the rows still come from
//! state and the snapshot's `error` says why.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::commands::Envelope;
use crate::render::{humanize_secs, table};
use crate::snapshot::{collect_from, ContainerStatus, Depth, InstanceRow, PhaseRow, RuntimeView, Snapshot};

/// MACHINE of this host's rows: `@`, as `@/<name>` names one of them.
pub(crate) const LOCAL_MACHINE: &str = "@";

/// `machines`: some machine is registered, so the table gets a MACHINE
/// column and every machine's rows are fetched through the daemon.
pub fn ps(dir: &Path, all: bool, json: bool, machines: bool) -> Result<()> {
    let view = RuntimeView::gather(Depth::Full);
    #[allow(unused_mut)]
    let mut snapshot = collect_from(dir, &view);
    if !json {
        // Without the state there are no rows to show.
        if let Some(e) = snapshot.error.as_deref().filter(|e| e.starts_with("state: ")) {
            bail!("{e}");
        }
        // Only the runtime's: a config error is about `dir`, which `ps` lists regardless of.
        for e in &view.errors {
            eprintln!("warning: {e}");
        }
    }
    #[cfg(unix)]
    if machines {
        snapshot.instances.extend(machine_rows());
    }
    print!("{}", render(snapshot, all, json, machines)?);
    Ok(())
}

/// Every machine's rows through the daemon; what fails is a `warning:` line
/// on stderr, never `ps`'s error: this host's rows still print.
#[cfg(unix)]
fn machine_rows() -> Vec<InstanceRow> {
    let conn = crate::serve::endpoint::socket_dir().and_then(|dir| crate::serve::client::connect(&dir, "cli"));
    let mut conn = match conn {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("warning: machines: needs devsandbox serve: {e:#}");
            return Vec::new();
        }
    };
    let mut err = std::io::stderr();
    match crate::serve::machines::cli::calls::machines_rows(&mut |m, p| conn.call(m, p), &mut err) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("warning: machines: {e:#}");
            Vec::new()
        }
    }
}

/// What `ps` prints for `snapshot`: its instances cut and sorted ([`rows`]),
/// as the `--json` envelope (one line ending) or the table ([`table_of`],
/// a MACHINE column when `machines`). Shared with the run on a machine
/// (`machines::cli::calls`).
pub(crate) fn render(mut snapshot: Snapshot, all: bool, json: bool, machines: bool) -> Result<String> {
    snapshot.instances = rows(std::mem::take(&mut snapshot.instances), all);
    if json {
        let out = serde_json::to_string_pretty(&Envelope::new(&snapshot)).context("serialize ps")?;
        return Ok(format!("{out}\n"));
    }
    Ok(table_of(&snapshot.instances, machines))
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
            stats: true,
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
        let out = render(snapshot(vec![done, building, stopped]), true, false, false).unwrap();
        assert_eq!(
            out,
            "NAME  SANDBOX  STATUS          UPTIME  CPU   MEM\n\
             a     web      running (done)  2h05m   1.5%  20MiB / 1GiB\n\
             b     web      building        -       -     -\n\
             c     web      exited          -       -     -\n"
        );
    }

    /// PROFILE only when some row has one; MACHINE only with machines, `@`
    /// for this host's rows.
    #[test]
    fn profile_and_machine_columns_show_when_they_apply() {
        let mut a = up("a");
        a.profile = Some("work".into());
        let mut b = up("b");
        b.profile = Some("work".into());
        b.profile_disallowed = true;
        b.machine = Some("lap".into());
        let c = up("c");
        let out = render(snapshot(vec![a, b, c.clone()]), false, false, true).unwrap();
        assert_eq!(
            out,
            "NAME  SANDBOX  STATUS   UPTIME  CPU  MEM  PROFILE             MACHINE\n\
             a     web      running  2h05m   -    -    work                @\n\
             c     web      running  2h05m   -    -                        @\n\
             b     web      running  2h05m   -    -    work (not allowed)  lap\n"
        );
        let out = render(snapshot(vec![c.clone()]), false, false, false).unwrap();
        assert_eq!(out, "NAME  SANDBOX  STATUS   UPTIME  CPU  MEM\nc     web      running  2h05m   -    -\n");
        let mut p = c;
        p.profile = Some("home".into());
        let out = render(snapshot(vec![p, up("d")]), false, false, false).unwrap();
        assert!(out.lines().all(|l| l == l.trim_end()), "no trailing padding: {out:?}");
    }

    /// `--json` is the snapshot's envelope, its instances cut as the table's.
    #[test]
    fn json_is_the_snapshot_envelope_with_the_shown_instances() {
        let rows = vec![up("up"), row("gone", ContainerStatus::Missing)];
        let json: serde_json::Value = serde_json::from_str(&render(snapshot(rows.clone()), false, true, false).unwrap()).unwrap();
        assert_eq!(json["schema"], 1);
        let names: Vec<&str> = json["data"]["instances"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["up"]);
        assert_eq!(json["data"]["instances"][0]["status"], serde_json::json!({"state": "running", "text": "Up 2 hours"}));
        assert!(json["data"].get("collected_at").is_none());
        let json: serde_json::Value = serde_json::from_str(&render(snapshot(rows), true, true, false).unwrap()).unwrap();
        assert_eq!(json["data"]["instances"][0]["status"], serde_json::json!({"state": "missing"}), "sorted by name");
    }
}
