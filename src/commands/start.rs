use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{resolve_instance, run, services, stop};
use crate::config::{Config, ResolvedSandbox};
use crate::runtime::backend;
use crate::state::{Instance, State};

/// Start a stopped sandbox instance (the counterpart of `stop`). When the
/// instance's sandbox resolves in this config root the full path runs: warn on
/// config drift, start the container, bring services up (recreating gc'd ones)
/// and refresh how the container resolves them, then run `postStartCommand`.
/// Otherwise (orphan, or another config root) its containers are simply
/// started. `--all` starts every instance whose container exists and is
/// stopped.
pub fn start(dir: &Path, name: Option<String>, all: bool) -> Result<()> {
    let state = State::load()?;
    if all {
        let mut started = 0;
        for (key, info) in &state.instances {
            if backend().is_running(&info.container)? == Some(false) {
                start_instance(dir, key, info)?;
                started += 1;
            }
        }
        if started == 0 {
            println!("nothing to start");
        }
        return Ok(());
    }

    let name = name.expect("clap requires a name without --all");
    let key = resolve_instance(&state, &name)?;
    let info = state.instances.get(&key).expect("key came from state");
    if backend().is_running(&info.container)?.is_none() {
        bail!("`{key}` has no container (see `devsandbox ps -a`); `devsandbox run` recreates it");
    }
    start_instance(dir, &key, info)
}

fn start_instance(dir: &Path, key: &str, info: &Instance) -> Result<()> {
    match resolved_sandbox(dir, info) {
        Some((config, sandbox)) => {
            run::warn_on_drift(dir, &info.container, &sandbox)?;
            backend().run_checked(&["start", &info.container])?;
            // Services may have been recreated with new addresses (or gc'd)
            // since the instance last ran; bring them up and refresh resolution.
            let project = services::project_id(dir)?;
            let service_names = sandbox.properties.services.clone().unwrap_or_default();
            let (_, endpoints) =
                services::ensure_services(&config, dir, &project, &info.instance_id, &service_names)?;
            backend().wire_service_dns(&info.container, &endpoints)?;
            if let Some(cmd) = &sandbox.properties.post_start_command {
                run::exec_lifecycle(
                    &info.container,
                    &info.workspace,
                    Some(&info.remote_env),
                    info.remote_user.as_deref(),
                    cmd,
                )
                .context("postStartCommand failed")?;
            }
        }
        None => {
            let services = stop::service_containers(&info.project, &info.instance_id);
            start_containers(&info.container, &services, false);
        }
    }
    println!("started {key}");
    Ok(())
}

/// The instance's sandbox resolved against this config root, or `None` when
/// the config doesn't load, the sandbox is gone from it, or the instance was
/// created from a different config root (project mismatch) — the caller then
/// falls back to a bare container start.
fn resolved_sandbox(dir: &Path, info: &Instance) -> Option<(Config, ResolvedSandbox)> {
    let config = Config::load(dir).ok()?;
    let project = services::project_id(dir).ok()?;
    if !info.project.is_empty() && info.project != project {
        return None;
    }
    let sandbox = config.resolve_sandbox(&info.sandbox).ok()?;
    Some((config, sandbox))
}

/// Start each service container then the main `container` (services first so
/// they are reachable when the instance comes up). Exit codes are ignored
/// throughout (already-running / missing containers must not fail the start).
/// `quiet` routes through the screen-safe `output_quiet` so stderr can't
/// corrupt the TUI's alternate screen; otherwise stdio is inherited.
pub(crate) fn start_containers(container: &str, services: &[String], quiet: bool) {
    let mut targets: Vec<&str> = services.iter().map(String::as_str).collect();
    targets.push(container);
    for target in targets {
        if quiet {
            let _ = backend().output_quiet(&["start", target]);
        } else {
            let _ = backend().run_inherit(&["start", target]);
        }
    }
}
