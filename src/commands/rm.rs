use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{confirm, resolve_instance};
use crate::runtime::backend;
use crate::state::State;

pub fn rm(name: &str) -> Result<()> {
    let mut state = State::load()?;

    let key = resolve_instance(&state, name)?;

    let info = state.instances.get(&key).expect("key came from state");
    let instance_id = info.instance_id.clone();
    let container = info.container.clone();
    let worktree = info.worktree.clone();
    let branch = info.branch.clone();
    let base_folder = info.base_folder.clone();
    let project = info.project.clone();
    let volumes = info.volumes.clone();

    // Remove the container; ignore failure (it may already be gone).
    let _ = backend().remove_force(&container);

    // Per-instance volumes (e.g. docker-in-docker's /var/lib/docker) die with
    // the instance; `rebuild` never comes through here, so they survive it.
    // After the container: runtimes refuse to delete a volume in use. A
    // failure warns and still removes the instance, like the container above.
    for volume in &volumes {
        match backend().remove_volume(volume) {
            Ok(true) => println!("removed volume {volume}"),
            Ok(false) => {}
            Err(e) => eprintln!("warning: cannot remove volume `{volume}`: {e:#}"),
        }
    }

    // The managed shell-history dir is deliberately kept: `run` re-provisions
    // the same per-instance path, so history survives an rm + run rebuild.

    // Reap this instance's isolated services and its per-instance network,
    // addressed by the persistent id (their names/labels carry the id, not the
    // possibly-renamed key). Global services are shared and left to `gc`.
    // Empty `project` = pre-upgrade state.
    if !project.is_empty() {
        for svc in super::stop::service_containers(&project, &instance_id) {
            let _ = backend().remove_force(&svc);
        }
        let network = crate::commands::services::instance_network(&project, &instance_id);
        if backend().network_exists(&network).unwrap_or(false) {
            let _ = backend().run_inherit(&["network", "rm", &network]);
        }
    }

    // Drop this instance's ssh-agent link: it is worthless without its
    // instance, and `run` recreates it. Best-effort — a missing link is a
    // harmless no-op; `gc` sweeps any that outlive a crashed rm.
    if let Some(link) = crate::commands::run::ssh_agent_link_path(&instance_id) {
        let _ = std::fs::remove_file(link);
    }

    if let Some(worktree) = worktree {
        remove_worktree(&base_folder, &worktree)?;
        // Recorded branch for instances created since it was tracked; older state
        // entries fall back to the legacy default.
        let branch = branch.unwrap_or_else(|| format!("sandbox/{key}"));
        if confirm(&format!("delete branch `{branch}`?"))? {
            let _ = std::process::Command::new("git")
                .args(["-C", &base_folder.to_string_lossy(), "branch", "-D", &branch])
                .status();
        }
    }

    state.instances.remove(&key);
    state.save()?;
    println!("removed {key}");
    Ok(())
}

fn remove_worktree(base: &Path, worktree: &Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .args([
            "-C",
            &base.to_string_lossy(),
            "worktree",
            "remove",
            &worktree.to_string_lossy(),
        ])
        .status()
        .context("failed to run git (is it installed?)")?;
    if !status.success() {
        bail!(
            "git worktree remove failed for `{}` (commit or discard changes, \
             then retry, or remove it manually)",
            worktree.display()
        );
    }
    Ok(())
}

