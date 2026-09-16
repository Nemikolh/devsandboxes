use anyhow::Result;

use crate::render::table;
use crate::runtime::{backend, NAME_PREFIX};

const HEADERS: [&str; 3] = ["NAME", "CPU", "MEM"];

/// CPU/memory usage of running devsandbox containers (instances and services),
/// from the same backend call that feeds the TUI's live columns.
pub fn stats() -> Result<()> {
    let mut rows = backend().stats()?;
    rows.retain(|r| r.name.starts_with(NAME_PREFIX));
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let cells: Vec<[String; 3]> = rows.into_iter().map(|r| [r.name, r.cpu, r.mem]).collect();
    print!("{}", table(&HEADERS, &cells));
    Ok(())
}
