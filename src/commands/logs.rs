use std::io::Write;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::{resolve_container, resolve_instance};
use crate::runtime::backend;
use crate::state::{PhaseView, State};

/// How often `--follow` checks for new output and whether the build ended.
const FOLLOW_POLL: Duration = Duration::from_millis(500);

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

/// `logs --build`: print an instance's build log (`crate::build_log`).
/// `follow` keeps printing what's appended while the instance is in flight,
/// and stops once it isn't (done, failed, interrupted) or its record is gone
/// (rolled back, removed), after printing what's left.
pub fn build_log(name: &str, follow: bool) -> Result<()> {
    let state = State::load()?;
    let key = resolve_instance(&state, name)?;
    let instance_id = state.instances.get(&key).expect("key came from state").instance_id.clone();
    let path = crate::build_log::path(&instance_id)?;
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The dashboard runs verbs in-process, without a build log.
            bail!("`{key}` has no build log (no {})", path.display())
        }
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
    };
    let mut out = std::io::stdout().lock();
    // Kept open while following: a rotation or `rm` moves or unlinks the
    // path, not the file this reads.
    let mut drain = |file: &mut std::fs::File| -> Result<()> {
        std::io::copy(file, &mut out).context("cannot print the build log")?;
        out.flush()?;
        Ok(())
    };
    drain(&mut file)?;
    if !follow {
        return Ok(());
    }
    loop {
        let view = State::load()?
            .instances
            .values()
            .find(|i| i.instance_id == instance_id)
            .map(|i| i.phase_view());
        if !in_flight(view.as_ref()) {
            // Whatever the command wrote before it recorded its end.
            return drain(&mut file);
        }
        std::thread::sleep(FOLLOW_POLL);
        drain(&mut file)?;
    }
}

/// Whether `--follow` waits on: the record exists (`Some`) and its phase is in
/// flight. Done (`Some(None)`), failed and gone (`None`) all end it.
fn in_flight(view: Option<&Option<PhaseView>>) -> bool {
    matches!(view, Some(Some(PhaseView::InFlight(_))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_stops_unless_the_record_is_in_flight() {
        assert!(in_flight(Some(&Some(PhaseView::InFlight("building".into())))));
        assert!(!in_flight(Some(&None)));
        let failed = PhaseView::Failed { at: "building".into(), error: "boom".into() };
        assert!(!in_flight(Some(&Some(failed))));
        assert!(!in_flight(None));
    }
}
