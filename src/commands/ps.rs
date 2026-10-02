use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::commands::status::Envelope;
use crate::render::table;
use crate::runtime::{backend, ContainerRow, NAME_PREFIX};
use crate::state::State;

const HEADERS: [&str; 3] = ["NAME", "IMAGE", "STATUS"];

/// One `ps --json` row: the runtime's listing plus the one state fact `ps`
/// shows, whether the container's instance is done.
#[derive(Debug, Serialize)]
struct PsRow {
    #[serde(flatten)]
    row: ContainerRow,
    done: bool,
}

pub fn ps(all: bool, json: bool) -> Result<()> {
    let mut rows = backend().list(all, NAME_PREFIX)?;
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    // Best-effort: `ps` is a runtime listing, so an unreadable state file
    // only loses the done marks.
    let rows = join_done(rows, &State::load().unwrap_or_default());
    if json {
        let out = serde_json::to_string_pretty(&Envelope::new(rows)).context("serialize ps")?;
        println!("{out}");
        return Ok(());
    }
    let cells: Vec<[String; 3]> = rows.into_iter().map(cells).collect();
    print!("{}", table(&HEADERS, &cells));
    Ok(())
}

/// Mark the rows whose container is a done instance's, by container name
/// (service containers are never in state, so never done).
fn join_done(rows: Vec<ContainerRow>, state: &State) -> Vec<PsRow> {
    let done: BTreeSet<&str> =
        state.instances.values().filter(|i| i.done.is_some()).map(|i| i.container.as_str()).collect();
    rows.into_iter().map(|row| PsRow { done: done.contains(row.name.as_str()), row }).collect()
}

/// Table cells; a done instance's STATUS gets a `(done)` suffix.
fn cells(r: PsRow) -> [String; 3] {
    let status = if r.done { format!("{} (done)", r.row.status) } else { r.row.status };
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
        let rows = join_done(vec![row("devsandbox-a"), row("devsandbox-b"), row("devsandbox-svc-redis")], &state);
        let done: Vec<bool> = rows.iter().map(|r| r.done).collect();
        assert_eq!(done, [true, false, false]);
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(json["done"], true);
        assert_eq!(json["name"], "devsandbox-a", "runtime fields stay top-level: {json}");
        let table: Vec<[String; 3]> = rows.into_iter().map(cells).collect();
        assert_eq!(table[0][2], "Up 1 hour (done)");
        assert_eq!(table[1][2], "Up 1 hour");
    }
}
