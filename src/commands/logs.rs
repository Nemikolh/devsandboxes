use anyhow::Result;

use super::resolve_container;
use crate::runtime::backend;
use crate::state::State;

/// Print the last `lines` lines of a container's stdout+stderr. Mirrors the
/// TUI logs modal: empty output is flagged rather than silent. Logs are shown
/// verbatim (no coloring).
pub fn logs(name: &str, lines: usize) -> Result<()> {
    let state = State::load()?;
    let container = resolve_container(&state, name)?;
    let out = backend().logs_tail(&container, lines)?;
    if out.trim().is_empty() {
        println!("(no log output)");
    } else {
        println!("{out}");
    }
    Ok(())
}
