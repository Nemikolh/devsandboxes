use anyhow::Result;

use super::resolve_instance;
use crate::runtime::{backend, NAME_PREFIX};
use crate::state::State;

/// Stop a sandbox instance: stop the container plus its isolated
/// service containers. The container and its state entry survive (`run`
/// restarts a stopped container); networks and global services are left alone.
/// Forgiving by design (idempotent): an already-stopped or missing container is
/// not an error.
pub fn stop(name: &str) -> Result<()> {
    let state = State::load()?;
    let key = resolve_instance(&state, name)?;
    let info = state.instances.get(&key).expect("key came from state");
    let container = info.container.clone();
    let project = info.project.clone();

    let services = service_containers(&project, &key);
    stop_containers(&container, &services, false);

    println!("stopped {key}");
    Ok(())
}

/// Container names of this instance's isolated services, matched on the
/// project+instance labels (same rule as `rm`). Empty when `project` is empty
/// (pre-upgrade state) or on any runtime error. Listing always captures
/// stderr, so this is safe from callers that own the screen (the TUI).
pub(crate) fn service_containers(project: &str, instance: &str) -> Vec<String> {
    if project.is_empty() {
        return Vec::new();
    }
    match backend().list(true, NAME_PREFIX) {
        Ok(rows) => rows
            .into_iter()
            .filter(|r| {
                r.label("devsandbox.project") == Some(project)
                    && r.label("devsandbox.instance") == Some(instance)
            })
            .map(|r| r.name)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Stop the main `container` then each service container. Exit codes are
/// ignored throughout (already-stopped / missing containers must not fail the
/// stop). `quiet` routes through the screen-safe `output_quiet` so stderr
/// can't corrupt the TUI's alternate screen; otherwise stdio is inherited.
pub(crate) fn stop_containers(container: &str, services: &[String], quiet: bool) {
    let mut targets: Vec<&str> = Vec::with_capacity(services.len() + 1);
    targets.push(container);
    targets.extend(services.iter().map(String::as_str));
    for target in targets {
        if quiet {
            let _ = backend().output_quiet(&["stop", target]);
        } else {
            let _ = backend().run_inherit(&["stop", target]);
        }
    }
}
