use anyhow::{Context, Result};

use crate::commands::status::Envelope;
use crate::render::table;
use crate::runtime::{backend, StatsRow, NAME_PREFIX};

const HEADERS: [&str; 3] = ["NAME", "CPU", "MEM"];

/// CPU/memory usage of running devsandbox containers (instances and services),
/// from the same backend call that feeds the TUI's live columns.
pub fn stats(json: bool) -> Result<()> {
    print!("{}", render(rows(backend().stats()?), json)?);
    Ok(())
}

/// `stats`' rows from the runtime's: devsandbox's containers only, sorted by
/// name. Shared with `containers.stats`.
pub(crate) fn rows(mut rows: Vec<StatsRow>) -> Vec<StatsRow> {
    rows.retain(|r| r.name.starts_with(NAME_PREFIX));
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// What `stats` prints for `rows`: the `--json` envelope (one line ending),
/// or the table. Shared with the run on an offered machine
/// (`machines::cli::offered`).
pub(crate) fn render(rows: Vec<StatsRow>, json: bool) -> Result<String> {
    if json {
        let out = serde_json::to_string_pretty(&Envelope::new(rows)).context("serialize stats")?;
        return Ok(format!("{out}\n"));
    }
    let cells: Vec<[String; 3]> = rows.into_iter().map(|r| [r.name, r.cpu, r.mem]).collect();
    Ok(table(&HEADERS, &cells))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> StatsRow {
        StatsRow { name: name.into(), cpu: "1.0%".into(), mem: "2MiB".into() }
    }

    #[test]
    fn rows_are_devsandboxs_sorted() {
        let got = rows(vec![row("devsandbox-b"), row("other"), row("devsandbox-a")]);
        let names: Vec<&str> = got.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["devsandbox-a", "devsandbox-b"]);
    }
}
