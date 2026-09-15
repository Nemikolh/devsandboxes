use anyhow::Result;

use super::resolve_instance;
use crate::docker;
use crate::state::State;

/// Stop a sandbox instance: `docker stop` the container plus its isolated
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

    let services = service_containers(&project, &key, false);
    stop_containers(&container, &services, false);

    println!("stopped {key}");
    Ok(())
}

/// Container ids of this instance's isolated services, via the project+instance
/// label filters (same filters as `rm`). Empty when `project` is empty
/// (pre-upgrade state) or on any docker error. `quiet` captures stderr for
/// callers that own the screen (the TUI background thread).
pub(crate) fn service_containers(project: &str, instance: &str, quiet: bool) -> Vec<String> {
    if project.is_empty() {
        return Vec::new();
    }
    let filter_project = format!("label=devsandbox.project={project}");
    let filter_instance = format!("label=devsandbox.instance={instance}");
    let args = ["ps", "-aq", "--filter", &filter_project, "--filter", &filter_instance];
    let listing = if quiet { docker::output_quiet(&args) } else { docker::output(&args) };
    match listing {
        Ok(listing) => listing
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `docker stop` the main `container` then each service container. Exit codes are
/// ignored throughout (already-stopped / missing containers must not fail the
/// stop). `quiet` routes through the screen-safe [`docker::output_quiet`] so
/// stderr can't corrupt the TUI's alternate screen; otherwise stdio is inherited.
pub(crate) fn stop_containers(container: &str, services: &[String], quiet: bool) {
    let mut targets: Vec<&str> = Vec::with_capacity(services.len() + 1);
    targets.push(container);
    targets.extend(services.iter().map(String::as_str));
    for target in targets {
        if quiet {
            let _ = docker::output_quiet(&["stop", target]);
        } else {
            let _ = docker::run_inherit(&["stop", target]);
        }
    }
}
