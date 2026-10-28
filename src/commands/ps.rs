use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::commands::status::Envelope;
use crate::render::table;
use crate::runtime::{backend, ContainerRow, NAME_PREFIX};
use crate::snapshot::PhaseRow;
use crate::state::{Instance, PhaseView, State};

const HEADERS: [&str; 3] = ["NAME", "IMAGE", "STATUS"];

/// One `ps --json` row: the runtime's listing plus the state facts `ps`
/// shows: whether the container's instance is done, and its phase while it
/// is being materialized or failed (`snapshot::PhaseRow`, absent when ready).
/// Also the API's `containers.list` rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PsRow {
    #[serde(flatten)]
    pub row: ContainerRow,
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<PhaseRow>,
}

pub fn ps(all: bool, json: bool) -> Result<()> {
    // Best-effort: `ps` is a runtime listing, so an unreadable state file
    // only loses the done marks.
    let rows = rows(backend().list(all, NAME_PREFIX)?, &State::load().unwrap_or_default());
    print!("{}", render(rows, json)?);
    Ok(())
}

/// `ps`'s rows from the runtime's `listed` containers: sorted by name,
/// joined with `state` ([`join_state`]). Shared with `containers.list`.
pub(crate) fn rows(mut listed: Vec<ContainerRow>, state: &State) -> Vec<PsRow> {
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    join_state(listed, state, Instance::phase_view)
}

/// What `ps` prints for `rows`: the `--json` envelope (one line ending), or
/// the table. Shared with the run on an offered machine
/// (`machines::cli::offered`).
pub(crate) fn render(rows: Vec<PsRow>, json: bool) -> Result<String> {
    if json {
        let out = serde_json::to_string_pretty(&Envelope::new(rows)).context("serialize ps")?;
        return Ok(format!("{out}\n"));
    }
    let cells: Vec<[String; 3]> = rows.into_iter().map(cells).collect();
    Ok(table(&HEADERS, &cells))
}

/// Join the rows with their instance by container name: done marks and the
/// phase (`phase` is `Instance::phase_view`, injected so tests don't probe
/// pids). Service containers are never in state, so neither applies to them.
/// A listing, so an instance without a container (yet) has no row.
fn join_state(rows: Vec<ContainerRow>, state: &State, phase: impl Fn(&Instance) -> Option<PhaseView>) -> Vec<PsRow> {
    let done: BTreeSet<&str> =
        state.instances.values().filter(|i| i.done.is_some()).map(|i| i.container.as_str()).collect();
    rows.into_iter()
        .map(|row| {
            let inst = state.instances.values().find(|i| i.container == row.name);
            let phase = inst.and_then(&phase).map(PhaseRow::from);
            PsRow { done: done.contains(row.name.as_str()), phase, row }
        })
        .collect()
}

/// Table cells. STATUS is the phase while set (the step in flight, or
/// `failed (<step>)`) instead of the runtime's text; a done instance's gets
/// a `(done)` suffix.
fn cells(r: PsRow) -> [String; 3] {
    let status = match r.phase {
        Some(PhaseRow::InFlight { step }) => step,
        Some(PhaseRow::Failed { step, .. }) => format!("failed ({step})"),
        None => r.row.status,
    };
    let status = if r.done { format!("{status} (done)") } else { status };
    [r.row.name, r.row.image, status]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> ContainerRow {
        ContainerRow { name: name.into(), image: "img".into(), status: "Up 1 hour".into(), ..Default::default() }
    }

    #[test]
    fn done_instances_are_marked_by_container_name() {
        let mut state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"devsandbox-a\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n\
             [instance.b]\nsandbox = \"web\"\ncontainer = \"devsandbox-b\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        state.instances.get_mut("a").unwrap().done = Some(1);
        let rows = join_state(vec![row("devsandbox-a"), row("devsandbox-b"), row("devsandbox-svc-redis")], &state, |_| None);
        let done: Vec<bool> = rows.iter().map(|r| r.done).collect();
        assert_eq!(done, [true, false, false]);
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(json["done"], true);
        assert_eq!(json["name"], "devsandbox-a", "runtime fields stay top-level: {json}");
        let table: Vec<[String; 3]> = rows.into_iter().map(cells).collect();
        assert_eq!(table[0][2], "Up 1 hour (done)");
        assert_eq!(table[1][2], "Up 1 hour");
    }

    #[test]
    fn the_phase_replaces_the_runtime_status() {
        let state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"devsandbox-a\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n\
             [instance.b]\nsandbox = \"web\"\ncontainer = \"devsandbox-b\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        let phase = |i: &Instance| match i.container.as_str() {
            "devsandbox-a" => Some(PhaseView::InFlight("postCreateCommand".into())),
            _ => Some(PhaseView::Failed { at: "postStartCommand".into(), error: "exit status 1".into() }),
        };
        let rows = join_state(vec![row("devsandbox-a"), row("devsandbox-b"), row("devsandbox-svc-redis")], &state, phase);
        let json = serde_json::to_value(&rows[1]).unwrap();
        assert_eq!(json["phase"], serde_json::json!({"state": "failed", "step": "postStartCommand", "error": "exit status 1"}));
        assert!(serde_json::to_value(&rows[2]).unwrap().get("phase").is_none(), "service: no phase");
        let table: Vec<[String; 3]> = rows.into_iter().map(cells).collect();
        assert_eq!(table[0][2], "postCreateCommand");
        assert_eq!(table[1][2], "failed (postStartCommand)");
        assert_eq!(table[2][2], "Up 1 hour");
    }
}
