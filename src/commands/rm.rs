use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{confirm, resolve_instance};
use crate::commands::run::host_git;
use crate::runtime::backend;
use crate::state::State;

/// `delete_branch` is the answer to "delete the branch `run` created?":
/// `Some` from `--delete-branch` / `--keep-branch`, `None` to ask (see
/// [`branch_decision`]).
pub fn rm(name: &str, delete_branch: Option<bool>) -> Result<()> {
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
    // repo would be refused by `host_git` later, or when `git worktree remove`
    // would refuse a dirty worktree (otherwise rm stops half-way, leaving an
    // instance without its container).
    if let Some(worktree) = &worktree {
        crate::commands::run::check_repo(&base_folder)?;
        check_worktree_removable(worktree)?;
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
            if branch_decision(delete_branch, || confirm(&format!("delete branch `{branch}`?")))? {
                match host_git(&base_folder) {
                    Ok(mut git) => {
                        // `-D`: the branch is sandbox scratch `run` created, so
                        // unmerged commits are deleted too, as a yes always did.
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

/// Whether to delete the branch `run` created: an explicit flag answers
/// without prompting, so scripted callers (off a TTY `confirm` is always no)
/// can still clean up; no flag asks.
fn branch_decision(flag: Option<bool>, ask: impl FnOnce() -> Result<bool>) -> Result<bool> {
    match flag {
        Some(delete) => Ok(delete),
        None => ask(),
    }
}

/// Refuse when `git worktree remove` (no `--force`) would: it rejects a
/// worktree with modified, staged or untracked files, using this same
/// `status` query (ignored files don't count). Run before any teardown so a
/// refused rm leaves the instance intact. A worktree already gone from disk
/// passes: `worktree remove` then just drops git's record of it.
fn check_worktree_removable(worktree: &Path) -> Result<()> {
    if !worktree.exists() {
        return Ok(());
    }
    let out = host_git(worktree)?
        .args(["status", "--porcelain", "--ignore-submodules=none"])
        .output()
        .context("failed to run git (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "cannot check worktree `{}` for changes: {}",
            worktree.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let status = String::from_utf8_lossy(&out.stdout);
    let changes: Vec<&str> = status.lines().filter(|l| !l.trim().is_empty()).collect();
    if changes.is_empty() {
        return Ok(());
    }
    const SHOWN: usize = 10;
    let mut list: String = changes.iter().take(SHOWN).map(|l| format!("\n  {l}")).collect();
    if changes.len() > SHOWN {
        list.push_str(&format!("\n  … and {} more", changes.len() - SHOWN));
    }
    bail!(
        "worktree `{}` has uncommitted or untracked changes; nothing was removed \
         (commit, stash or discard them in the worktree, then retry):{list}",
        worktree.display()
    )
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

    #[test]
    fn branch_flags_answer_without_asking() {
        let never = || -> Result<bool> { panic!("prompted despite a flag") };
        assert!(branch_decision(Some(true), never).unwrap());
        assert!(!branch_decision(Some(false), never).unwrap());
        // No flag: the prompt's answer (off a TTY, `confirm` says no).
        assert!(branch_decision(None, || Ok(true)).unwrap());
        assert!(!branch_decision(None, || Ok(false)).unwrap());
    }

    /// The preflight refuses exactly what `git worktree remove` refuses, so rm
    /// can bail before tearing anything down.
    #[test]
    fn worktree_preflight_matches_git_worktree_remove() {
        let root = std::env::temp_dir().join(format!("devsandbox-rmdirty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (base, wt) = (root.join("base"), root.join("wt"));
        std::fs::create_dir_all(&base).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&base, &["init", "-q", "-b", "main"]);
        std::fs::write(base.join(".gitignore"), "ignored\n").unwrap();
        std::fs::write(base.join("tracked"), "a\n").unwrap();
        git(&base, &["add", "."]);
        git(&base, &["commit", "-q", "-m", "init"]);
        git(&base, &["worktree", "add", "-q", &wt.to_string_lossy(), "-b", "sandbox/r"]);

        // Ignored files don't block removal.
        std::fs::write(wt.join("ignored"), "x").unwrap();
        check_worktree_removable(&wt).unwrap();

        // Untracked, then modified: refused with the change listed, and the
        // worktree left in place.
        std::fs::write(wt.join("new"), "x").unwrap();
        let err = check_worktree_removable(&wt).unwrap_err().to_string();
        assert!(err.contains("uncommitted or untracked changes; nothing was removed"), "{err}");
        assert!(err.contains("?? new"), "{err}");
        assert!(remove_worktree(&base, &wt).is_err(), "git agrees it is dirty");
        std::fs::remove_file(wt.join("new")).unwrap();
        std::fs::write(wt.join("tracked"), "b\n").unwrap();
        let err = check_worktree_removable(&wt).unwrap_err().to_string();
        assert!(err.contains("M tracked"), "{err}");
        assert!(wt.exists());

        // Committed: passes, and git removes it.
        git(&wt, &["commit", "-q", "-am", "change"]);
        check_worktree_removable(&wt).unwrap();
        remove_worktree(&base, &wt).unwrap();
        // Already gone from disk: nothing to refuse.
        check_worktree_removable(&wt).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
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
