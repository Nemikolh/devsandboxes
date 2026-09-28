use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{confirm, resolve_instance};
use crate::commands::run::host_git;
use crate::runtime::backend;
use crate::state::State;

pub fn rm(name: &str) -> Result<()> {
    let mut state = State::load()?;

    let key = resolve_instance(&state, name)?;

    let info = state.instances.get(&key).expect("key came from state");
    let instance_id = info.instance_id.clone();
    let container = info.container.clone();
    let worktree = info.worktree.clone();
    let branch = branch_to_offer(info.branch.clone(), info.branch_created, &key);
    let base_folder = info.base_folder.clone();
    let project = info.project.clone();
    let volumes = info.volumes.clone();

    // Refuse up front, before anything is torn down, when the worktree's base
    // repo would be refused by `host_git` later (otherwise rm stops half-way).
    if worktree.is_some() {
        crate::commands::run::check_repo(&base_folder)?;
    }

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
        if let Some(branch) = branch {
            if confirm(&format!("delete branch `{branch}`?"))? {
                match host_git(&base_folder) {
                    Ok(mut git) => {
                        let _ = git.args(["branch", "-D", &branch]).status();
                    }
                    Err(e) => eprintln!("warning: branch `{branch}` not deleted: {e:#}"),
                }
            }
        }
    }

    state.instances.remove(&key);
    state.save()?;
    println!("removed {key}");
    Ok(())
}

/// The branch `rm` offers to delete: only one `run` created (a reused PR or
/// feature branch is the user's). Entries from before branches were recorded
/// fall back to the legacy default.
fn branch_to_offer(branch: Option<String>, created: bool, key: &str) -> Option<String> {
    created.then(|| branch.unwrap_or_else(|| format!("sandbox/{key}")))
}

fn remove_worktree(base: &Path, worktree: &Path) -> Result<()> {
    let status = host_git(base)?
        .args([
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_created_branches_are_offered_for_deletion() {
        assert_eq!(branch_to_offer(Some("feat/x".into()), true, "k").as_deref(), Some("feat/x"));
        assert_eq!(branch_to_offer(None, true, "k").as_deref(), Some("sandbox/k"));
        assert_eq!(branch_to_offer(Some("pr/1".into()), false, "k"), None);
    }

    /// A sandbox-planted `core.fsmonitor` must not run during `worktree remove`
    /// (the security review's PoC): the removal is refused instead.
    #[cfg(unix)]
    #[test]
    fn remove_worktree_refuses_planted_fsmonitor() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("devsandbox-rmsec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (base, wt) = (root.join("base"), root.join("wt"));
        std::fs::create_dir_all(&base).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&base)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&["worktree", "add", "-q", &wt.to_string_lossy(), "-b", "sandbox/r"]);
        let marker = root.join("pwned");
        let monitor = root.join("monitor.sh");
        std::fs::write(&monitor, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&monitor, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&["config", "core.fsmonitor", &monitor.to_string_lossy()]);

        let err = remove_worktree(&base, &wt).unwrap_err().to_string();
        assert!(err.contains(".git/config sets core.fsmonitor"), "{err}");
        assert!(!marker.exists(), "fsmonitor ran on the host");

        // Once the key is removed, the removal goes through.
        git(&["config", "--unset", "core.fsmonitor"]);
        remove_worktree(&base, &wt).unwrap();
        assert!(!wt.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
