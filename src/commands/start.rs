use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{autostart, resolve_instance, run, services, stop};
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

pub(crate) fn start_instance(dir: &Path, key: &str, info: &Instance) -> Result<()> {
    // Re-point the agent symlink before either start path so a restart after
    // host agent rotation re-captures the live socket (docs/ssh-agent.md).
    run::ssh_agent_refresh(info);
    match resolved_sandbox(dir, info) {
        Some((config, sandbox)) => {
            run::warn_on_drift(dir, &info.container, &sandbox)?;
            backend().run_checked(&["start", &info.container])?;
            autostart::apply_restart_policy(&info.container, key, sandbox.properties.autostart);
            // `ensure_recorded` may install a different-arch helper than the
            // pre-start `info` snapshot recorded; use the arch it establishes
            // for the relay decision below, not the possibly-stale `info`.
            let arch = crate::devsbd::ensure_recorded(key, info, false);
            // Services may have been recreated with new addresses (or gc'd)
            // since the instance last ran; bring them up and refresh resolution.
            let project = services::project_id(dir)?;
            let service_names = sandbox.properties.services.clone().unwrap_or_default();
            let (_, endpoints) =
                services::ensure_services(&config, dir, &project, &info.instance_id, &service_names)?;
            backend().wire_service_dns(&info.container, &endpoints)?;
            // Same ssh-agent rule as `run`'s lifecycle chain, read off an
            // instance carrying the just-established arch.
            let mut fresh = info.clone();
            fresh.devsbd_arch = arch;
            let host_agent = crate::commands::exec::has_host_agent();
            let ssh_auth_sock = crate::commands::exec::ssh_auth_sock_env(&fresh, host_agent);
            let props = &sandbox.properties;
            // remoteEnv, then the saved `--env` (possibly newer than the
            // container's own env: `ensure` replaces it without a rebuild).
            let exec_env = info.exec_env();
            let boot = run::boot_spec_for(
                props.autostart,
                arch.is_some(),
                props.post_start_command.as_ref(),
                &info.workspace,
                Some(&exec_env),
                info.remote_user.as_deref(),
                ssh_auth_sock,
            );
            crate::devsbd::sync_boot(&info.container, boot.as_ref(), false);
            let runtime = autostart::runtime_restarts(props.autostart, backend().supports_restart_policy());
            let post_start = match &props.post_start_command {
                None => None,
                Some(cmd) => {
                    let hooked = runtime
                        && backend().label(&info.container, run::BOOT_HOOK_LABEL)?.is_some();
                    match post_start_runner(runtime, hooked) {
                        PostStart::Hook => None,
                        PostStart::HostPredatesHook => {
                            eprintln!(
                                "warning: {key} predates the runtime boot hook; recreate with \
                                 `devsandbox rebuild --force {key}`"
                            );
                            Some(cmd)
                        }
                        PostStart::Host => Some(cmd),
                    }
                }
            };
            if let Some(cmd) = post_start {
                // Hold one bridge (relay mode) for the command, report its
                // failure after.
                #[cfg(unix)]
                let bridge = (crate::devsbd::relay_mode(&fresh) && host_agent)
                    .then(|| crate::devsbd::bridge::spawn(&fresh))
                    .flatten();
                run::exec_lifecycle(
                    &info.container,
                    &info.workspace,
                    Some(&exec_env),
                    info.remote_user.as_deref(),
                    ssh_auth_sock,
                    cmd,
                )
                .context("postStartCommand failed")?;
                #[cfg(unix)]
                if let Some(Err(e)) =
                    bridge.as_ref().and_then(|b| b.outcome(std::time::Duration::ZERO))
                {
                    eprintln!("note: ssh-agent relay unavailable in `{}`: {e}", info.container);
                }
            }
        }
        None => {
            let services = stop::service_containers(&info.project, &info.instance_id);
            start_containers(&info.container, &services, false);
            crate::devsbd::ensure_recorded(key, info, false);
        }
    }
    println!("started {key}");
    Ok(())
}

/// Who runs `postStartCommand` on `start`.
#[derive(Debug, PartialEq, Eq)]
enum PostStart {
    /// The host, via exec (the usual path).
    Host,
    /// The container's boot hook already did on `docker start`; the host skips.
    Hook,
    /// Runtime mode, but the container predates the hook: the host, plus a
    /// warning to recreate.
    HostPredatesHook,
}

/// `runtime`: the runtime restarts the container itself
/// (`autostart::runtime_restarts`); `hooked`: its command runs the boot hook
/// (`run::BOOT_HOOK_LABEL`).
fn post_start_runner(runtime: bool, hooked: bool) -> PostStart {
    match (runtime, hooked) {
        (false, _) => PostStart::Host,
        (true, true) => PostStart::Hook,
        (true, false) => PostStart::HostPredatesHook,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_start_runner_per_mode() {
        assert_eq!(post_start_runner(false, false), PostStart::Host);
        // Not runtime mode: the hook has no boot file, the host runs it.
        assert_eq!(post_start_runner(false, true), PostStart::Host);
        assert_eq!(post_start_runner(true, true), PostStart::Hook);
        assert_eq!(post_start_runner(true, false), PostStart::HostPredatesHook);
    }
}
