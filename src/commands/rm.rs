use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{confirm, resolve_instance};
use crate::commands::run::host_git;
use crate::runtime::backend;
use crate::state::{FolderMount, State};

/// `delete_branch` is the answer to "delete the branch `run` created?":
/// `Some` from `--delete-branch` / `--keep-branch`, `None` to ask (see
/// [`branch_decision`]).
///
/// `force` skips the dirty-worktree preflight, discards uncommitted changes
/// (`git worktree remove --force`) and turns a failed teardown step into a
/// warning, so the instance always leaves state. `host_git`'s repo checks
/// still apply: a refused repo keeps its worktree on disk, with a warning.
pub fn rm(name: &str, delete_branch: Option<bool>, force: bool) -> Result<()> {
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
    let folders = info.folders.clone();

    // Refuse up front, before anything is torn down, when the worktree's base
    // repo would be refused by `host_git` later, or when `git worktree remove`
    // would refuse a dirty worktree (otherwise rm stops half-way, leaving an
    // instance without its container). `--force` gets past both later.
    if !force {
        if let Some(worktree) = &worktree {
            crate::commands::run::check_repo(&base_folder)?;
            check_worktree_removable(worktree)?;
        }
        check_folder_worktrees(&folders)?;
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

    // A failure keeps the instance in state so rm can be retried (unless
    // forced); worktrees a previous pass already removed are pruned, not
    // removed (see `remove_or_prune`), so the retry gets past them.
    remove_folder_worktrees(&folders, force)?;

    if let Some(worktree) = worktree {
        let removed = tolerate(force, remove_or_prune(&base_folder, &worktree, force))?;
        // A forced rm that left the worktree keeps its branch: `branch -D`
        // fails on a checked-out branch anyway.
        if let (Some(branch), true) = (branch, removed) {
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
    archive_inbox(&key, &instance_id);
    println!("removed {key}");
    Ok(())
}

/// Mark the removed instance's Inbox threads read-only, after its state entry
/// is gone: it can never send again, and instance ids are never reused, so the
/// threads stay as history until retention drops them. A store failure is a
/// warning — the instance is already removed, and failing here would say
/// otherwise.
fn archive_inbox(key: &str, instance_id: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let updated = crate::inbox::store::update(|inbox| {
        inbox.archive_owner(instance_id);
        inbox.prune(now);
    });
    if let Err(e) = updated {
        eprintln!("warning: cannot archive `{key}`'s inbox threads: {e:#}");
    }
}

/// A teardown step's result: an error aborts rm, or with `force` only warns.
/// `Ok(false)` = the step failed and was skipped.
fn tolerate(force: bool, result: Result<()>) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(e) if force => {
            eprintln!("warning: {e:#}");
            Ok(false)
        }
        Err(e) => Err(e),
    }
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
/// passes: [`remove_or_prune`] then drops any record git still has of it.
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

/// The `folders` records that carry a worktree, first record per worktree
/// path (several records may name one target, e.g. a worktree still recorded
/// after its entry switched to `worktree = "never"`).
fn folder_worktrees(folders: &[FolderMount]) -> Vec<(&FolderMount, &Path)> {
    let mut seen = std::collections::HashSet::new();
    folders
        .iter()
        .filter_map(|f| f.worktree.as_deref().map(|wt| (f, wt)))
        .filter(|(_, wt)| seen.insert(*wt))
        .collect()
}

/// Preflight for the `folders` worktrees, same checks as the primary's.
fn check_folder_worktrees(folders: &[FolderMount]) -> Result<()> {
    for (f, wt) in folder_worktrees(folders) {
        crate::commands::run::check_repo(&f.base)
            .and_then(|()| check_worktree_removable(wt))
            .with_context(|| format!("folders entry `{}`", f.target))?;
    }
    Ok(())
}

/// Remove the detached `folders` worktrees (no branches to offer), then
/// their `<id>.folders/` parent dirs once empty. `force` discards changes and
/// moves on past a failed entry, warning.
fn remove_folder_worktrees(folders: &[FolderMount], force: bool) -> Result<()> {
    let worktrees = folder_worktrees(folders);
    for (f, wt) in &worktrees {
        let result = remove_or_prune(&f.base, wt, force)
            .with_context(|| format!("folders entry `{}`", f.target));
        tolerate(force, result)?;
    }
    let mut parents: Vec<&Path> = worktrees.iter().filter_map(|(_, wt)| wt.parent()).collect();
    parents.dedup();
    for parent in parents {
        if parent.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".folders")) {
            // Non-recursive: anything left in there is not ours to delete.
            let _ = std::fs::remove_dir(parent);
        }
    }
    Ok(())
}

/// `git worktree remove`, or `git worktree prune` when the dir is already
/// gone (a previous rm removed it and then failed before dropping the
/// instance from state, or it was deleted by hand): `remove` fails once git's
/// record is gone, which would make every retry of rm fail; `prune` drops a
/// leftover record and is a no-op otherwise.
fn remove_or_prune(base: &Path, worktree: &Path, force: bool) -> Result<()> {
    if worktree.exists() {
        remove_worktree(base, worktree, force)
    } else {
        prune_worktrees(base)
    }
}

fn prune_worktrees(base: &Path) -> Result<()> {
    let status = host_git(base)?
        .args(["worktree", "prune"])
        .status()
        .context("failed to run git (is it installed?)")?;
    if !status.success() {
        bail!("git worktree prune failed in `{}`", base.display());
    }
    Ok(())
}

/// `force`: `git worktree remove --force`, discarding modified and untracked
/// files (a locked worktree still refuses: that needs `--force` twice).
fn remove_worktree(base: &Path, worktree: &Path, force: bool) -> Result<()> {
    let mut git = host_git(base)?;
    git.args(["worktree", "remove"]);
    if force {
        git.arg("--force");
    }
    let status = git
        .arg(worktree)
        .status()
        .context("failed to run git (is it installed?)")?;
    if !status.success() {
        bail!(
            "git worktree remove failed for `{}` (commit or discard changes, \
             then retry, retry with --force, or remove it manually)",
            worktree.display()
        );
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn only_created_branches_are_offered_for_deletion() {
        assert_eq!(branch_to_offer(Some("feat/x".into()), true, "k").as_deref(), Some("feat/x"));
        assert_eq!(branch_to_offer(None, true, "k").as_deref(), Some("sandbox/k"));
        assert_eq!(branch_to_offer(Some("pr/1".into()), false, "k"), None);
    }

    #[test]
    fn force_turns_step_failures_into_warnings() {
        assert!(tolerate(false, Ok(())).unwrap());
        assert!(tolerate(false, Err(anyhow::anyhow!("boom"))).is_err());
        assert!(!tolerate(true, Err(anyhow::anyhow!("boom"))).unwrap());
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
        assert!(remove_worktree(&base, &wt, false).is_err(), "git agrees it is dirty");
        std::fs::remove_file(wt.join("new")).unwrap();
        std::fs::write(wt.join("tracked"), "b\n").unwrap();
        let err = check_worktree_removable(&wt).unwrap_err().to_string();
        assert!(err.contains("M tracked"), "{err}");
        assert!(wt.exists());

        // Committed: passes, and git removes it.
        git(&wt, &["commit", "-q", "-am", "change"]);
        check_worktree_removable(&wt).unwrap();
        remove_worktree(&base, &wt, false).unwrap();
        // Already gone from disk: nothing to refuse.
        check_worktree_removable(&wt).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A retried rm (the previous pass removed the worktree, then failed
    /// before dropping the instance) must get past the gone worktree, where a
    /// second `git worktree remove` fails.
    #[test]
    fn worktree_removal_is_retryable() {
        let root = std::env::temp_dir().join(format!("devsandbox-rmretry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (base, wt) = (root.join("base"), root.join("wt"));
        std::fs::create_dir_all(&base).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap()
        };
        assert!(git(&base, &["init", "-q", "-b", "main"]).status.success());
        std::fs::write(base.join("tracked"), "a\n").unwrap();
        assert!(git(&base, &["add", "."]).status.success());
        assert!(git(&base, &["commit", "-q", "-m", "init"]).status.success());
        let add = git(&base, &["worktree", "add", "-q", &wt.to_string_lossy(), "-b", "sandbox/r"]);
        assert!(add.status.success());

        remove_or_prune(&base, &wt, false).unwrap();
        assert!(remove_worktree(&base, &wt, false).is_err(), "git refuses a second remove");
        check_worktree_removable(&wt).unwrap();
        remove_or_prune(&base, &wt, false).unwrap();

        // Deleted by hand with git's record left: pruned away.
        let wt2 = root.join("wt2");
        let add = git(&base, &["worktree", "add", "-q", "--detach", &wt2.to_string_lossy()]);
        assert!(add.status.success());
        std::fs::remove_dir_all(&wt2).unwrap();
        remove_or_prune(&base, &wt2, false).unwrap();
        let list = git(&base, &["worktree", "list", "--porcelain"]);
        assert!(!String::from_utf8_lossy(&list.stdout).contains("wt2"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Temp repos `a` and `b` with detached worktrees under
    /// `<root>/.worktrees/i.folders/`, as `run` lays them out.
    fn folder_fixture(tag: &str) -> (PathBuf, Vec<FolderMount>) {
        let root = std::env::temp_dir()
            .join(format!("devsandbox-rmfolders-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let parent = root.join(".worktrees/i.folders");
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
        let mut folders = Vec::new();
        for name in ["a", "b"] {
            let base = root.join(name);
            std::fs::create_dir_all(&base).unwrap();
            git(&base, &["init", "-q", "-b", "main"]);
            std::fs::write(base.join("tracked"), "a\n").unwrap();
            git(&base, &["add", "."]);
            git(&base, &["commit", "-q", "-m", "init"]);
            let wt = parent.join(format!("{name}-0123abcd"));
            git(&base, &["worktree", "add", "-q", "--detach", &wt.to_string_lossy()]);
            folders.push(FolderMount { target: format!("/w/{name}"), base, worktree: Some(wt) });
        }
        // A direct mount and a duplicate record of `a`'s worktree are no-ops.
        folders.push(FolderMount { target: "/w/c".into(), base: root.join("a"), worktree: None });
        folders.push(folders[0].clone());
        (root, folders)
    }

    fn worktree_listed(base: &Path, wt: &Path) -> bool {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(base)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&*wt.file_name().unwrap().to_string_lossy())
    }

    #[test]
    fn folder_worktrees_are_removed_with_their_parent() {
        let (root, folders) = folder_fixture("ok");
        check_folder_worktrees(&folders).unwrap();
        remove_folder_worktrees(&folders, false).unwrap();
        for f in &folders[..2] {
            let wt = f.worktree.as_deref().unwrap();
            assert!(!wt.exists());
            assert!(!worktree_listed(&f.base, wt), "git still records {}", wt.display());
        }
        assert!(!root.join(".worktrees/i.folders").exists());
        assert!(root.join(".worktrees").exists(), "only the `.folders` dir goes");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dirty_folder_worktree_fails_preflight() {
        let (root, folders) = folder_fixture("dirty");
        let wt = folders[1].worktree.clone().unwrap();
        std::fs::write(wt.join("new"), "x").unwrap();
        let err = format!("{:#}", check_folder_worktrees(&folders).unwrap_err());
        assert!(err.contains("folders entry `/w/b`"), "{err}");
        assert!(err.contains("?? new"), "{err}");
        assert!(wt.exists());

        // `--force` discards the changes and removes it all the same.
        remove_folder_worktrees(&folders, true).unwrap();
        assert!(!wt.exists());
        assert!(!worktree_listed(&folders[1].base, &wt));
        assert!(!root.join(".worktrees/i.folders").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A retry after an interrupted rm (or a worktree deleted by hand) must
    /// not trip on the missing dir, and still drops git's record of it.
    #[test]
    fn folder_worktree_removal_retries_after_partial_removal() {
        let (root, folders) = folder_fixture("retry");
        let (a, b) = (&folders[0], &folders[1]);
        let (wt_a, wt_b) = (a.worktree.as_deref().unwrap(), b.worktree.as_deref().unwrap());
        // `a` removed by a previous pass, `b` deleted by hand (record left).
        remove_worktree(&a.base, wt_a, false).unwrap();
        std::fs::remove_dir_all(wt_b).unwrap();
        assert!(worktree_listed(&b.base, wt_b));

        check_folder_worktrees(&folders).unwrap();
        remove_folder_worktrees(&folders, false).unwrap();
        assert!(!worktree_listed(&b.base, wt_b), "stale record pruned");
        assert!(!root.join(".worktrees/i.folders").exists());
        // And once more, with everything already gone.
        remove_folder_worktrees(&folders, false).unwrap();
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

        let err = remove_worktree(&base, &wt, false).unwrap_err().to_string();
        assert!(err.contains(".git/config sets core.fsmonitor"), "{err}");
        assert!(!marker.exists(), "fsmonitor ran on the host");

        // Once the key is removed, the removal goes through.
        git(&["config", "--unset", "core.fsmonitor"]);
        remove_worktree(&base, &wt, false).unwrap();
        assert!(!wt.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
