//! `devsandbox lend [show|on|off|drain]` and `devsandbox fleet join|leave`
//! (docs/architecture/daemon/fleet.md, *Lending: `lend.*`*): clients of the
//! daemon's `lend.get` / `lend.set`, never in-process, since `lend.toml` and
//! the lending link are the daemon's (decision 3 of the fleet plan).
//!
//! `fleet leave` drains first, waits until no instance placed here (under
//! the fleet root) is running or being built ([`busy`], over
//! `instances.list`), then turns lending off. Ctrl-C stops the wait and
//! leaves the worker draining.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use super::machine::call;
use crate::render::table;
use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::serve::fleet::lend::State;
use crate::serve::fleet::lender::{LendInfo, LinkState};
use crate::snapshot::{ContainerStatus, InstanceRow, PhaseRow};

const HEADERS: [&str; 5] = ["STATE", "HUB", "NAME", "LINK", "FLEET ROOT"];

/// How often `fleet leave` re-reads the placed instances.
const LEAVE_POLL: Duration = Duration::from_secs(2);

pub fn show() -> Result<()> {
    let info: LendInfo = call(&mut connect()?, "lend.get", Value::Null)?;
    print_info(&info);
    Ok(())
}

/// `lend on|off|drain`.
pub fn set_state(state: State) -> Result<()> {
    let info: LendInfo = call(&mut connect()?, "lend.set", json!({ "state": state }))?;
    println!("{}", summary(&info));
    Ok(())
}

/// `fleet join <hub> [--name N]`: name this host (`member.set`), then lend
/// to `hub`, `on`.
pub fn join(hub: &str, name: Option<&str>) -> Result<()> {
    let mut conn = connect()?;
    if let Some(name) = name {
        let _: Value = call(&mut conn, "member.set", json!({ "name": name }))?;
    }
    let info: LendInfo = call(&mut conn, "lend.set", json!({ "hub": hub, "state": State::On }))?;
    println!("{}", summary(&info));
    Ok(())
}

/// `fleet leave`: `drain`, wait for the placed instances, then `off`.
pub fn leave() -> Result<()> {
    let mut conn = connect()?;
    let info: LendInfo = call(&mut conn, "lend.get", Value::Null)?;
    if info.state == State::Off {
        println!("not lending");
        return Ok(());
    }
    let info: LendInfo = call(&mut conn, "lend.set", json!({ "state": State::Drain }))?;
    let hub = info.hub.clone().unwrap_or_default();
    let root = info.fleet_root.clone().context("lend.get has no fleet root while lending")?;
    let mut last: Option<Vec<String>> = None;
    loop {
        let rows: Rows = call(&mut conn, "instances.list", json!({ "dir": root, "depth": "listing" }))?;
        let waiting = busy(&rows.instances, &root);
        if waiting.is_empty() {
            break;
        }
        if last.as_ref() != Some(&waiting) {
            eprintln!("{}", waiting_note(&hub, &waiting));
            last = Some(waiting);
        }
        std::thread::sleep(LEAVE_POLL);
    }
    let _: LendInfo = call(&mut conn, "lend.set", json!({ "state": State::Off }))?;
    println!("left {hub}");
    Ok(())
}

/// `instances.list`'s rows (the rest of the answer is ignored).
#[derive(Deserialize)]
struct Rows {
    instances: Vec<InstanceRow>,
}

/// The instances placed here (their config root under the fleet root
/// `root`) that `fleet leave` waits for: running, or being built (pure).
pub fn busy(rows: &[InstanceRow], root: &Path) -> Vec<String> {
    rows.iter()
        .filter(|r| r.root.as_deref().is_some_and(|d| Path::new(d).starts_with(root)))
        .filter(|r| matches!(r.status, ContainerStatus::Running(_)) || matches!(r.phase, Some(PhaseRow::InFlight { .. })))
        .map(|r| r.name.clone())
        .collect()
}

fn waiting_note(hub: &str, waiting: &[String]) -> String {
    format!(
        "draining: waiting for {} placed by {hub} to stop: {} (Ctrl-C stops waiting; lending stays `drain`)",
        if waiting.len() == 1 { "1 instance".to_string() } else { format!("{} instances", waiting.len()) },
        waiting.join(", ")
    )
}

/// One line after a change: `lending: on, to me@hub as lap`.
fn summary(info: &LendInfo) -> String {
    match (&info.hub, info.state) {
        (_, State::Off) | (None, _) => "lending: off".to_string(),
        (Some(hub), state) => {
            format!("lending: {}, to {hub} as {}", state.as_str(), info.name.as_deref().unwrap_or("-"))
        }
    }
}

fn cells(info: &LendInfo) -> [String; 5] {
    let dash = || "-".to_string();
    [
        info.state.as_str().into(),
        info.hub.clone().unwrap_or_else(dash),
        info.name.clone().unwrap_or_else(dash),
        info.link.as_str().into(),
        info.fleet_root.as_ref().map(|r| r.display().to_string()).unwrap_or_else(dash),
    ]
}

fn print_info(info: &LendInfo) {
    print!("{}", table(&HEADERS, &[cells(info)]));
    if let Some(error) = &info.error {
        let what = if info.link == LinkState::Retrying { "link" } else { "not lending" };
        println!("{what}: {error}");
    }
}

fn connect() -> Result<Conn> {
    client::connect(&endpoint::socket_dir()?, "cli").context("`devsandbox lend` / `fleet` need devsandbox serve")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn row(name: &str, root: Option<&str>, status: ContainerStatus, phase: Option<PhaseRow>) -> InstanceRow {
        let mut r: InstanceRow = serde_json::from_value(json!({
            "name": name, "sandbox": "web", "container": format!("devsandbox-{name}"), "status": {"state": "missing"},
            "uptime_secs": 0, "cpu": null, "mem": null, "folder": "/w", "worktree": false, "services": [],
            "workspace": "/w", "remote_user": null, "remote_env_len": 0, "base_folder": "/w", "drift": false,
            "instance_id": name, "done": false, "root": root,
        }))
        .unwrap();
        r.status = status;
        r.phase = phase;
        r
    }

    #[test]
    fn leave_waits_for_running_or_building_placed_instances_only() {
        let root = Path::new("/d/fleet/me@hub");
        let up = || ContainerStatus::Running("Up 1s".into());
        let rows = [
            row("placed-up", Some("/d/fleet/me@hub/roots/a"), up(), None),
            row("placed-down", Some("/d/fleet/me@hub/roots/a"), ContainerStatus::Exited("Exited (0)".into()), None),
            row("placed-building", Some("/d/fleet/me@hub/b"), ContainerStatus::Missing, Some(PhaseRow::InFlight { step: "building".into() })),
            row("placed-failed", Some("/d/fleet/me@hub/b"), ContainerStatus::Missing, Some(PhaseRow::Failed { step: "building".into(), error: "x".into() })),
            row("mine", Some("/home/me/.devsandboxes"), up(), None),
            row("sibling", Some("/d/fleet/me@hub2/a"), up(), None),
            row("old", None, up(), None),
        ];
        assert_eq!(busy(&rows, root), ["placed-up", "placed-building"]);
        assert!(busy(&rows[1..2], root).is_empty(), "nothing running: leave");
        assert!(waiting_note("me@hub", &["a".into()]).contains("1 instance placed by me@hub to stop: a"));
    }

    #[test]
    fn summaries_and_cells() {
        let info = LendInfo {
            state: State::Drain,
            hub: Some("me@hub".into()),
            name: Some("lap".into()),
            fleet_root: Some(PathBuf::from("/d/fleet/me@hub")),
            link: LinkState::Connected,
            error: None,
        };
        assert_eq!(summary(&info), "lending: drain, to me@hub as lap");
        assert_eq!(cells(&info), ["drain", "me@hub", "lap", "connected", "/d/fleet/me@hub"].map(String::from));
        let off = LendInfo { state: State::Off, hub: None, fleet_root: None, link: LinkState::Off, ..info };
        assert_eq!(summary(&off), "lending: off");
        assert_eq!(cells(&off)[1], "-");
    }
}
