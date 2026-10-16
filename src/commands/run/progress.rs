//! An instance's `progress` while `run`/`rebuild` materialize it, and what a
//! failure leaves behind (docs/architecture/instances/lifecycle.md): a failed
//! instance is kept as `failed`, its build log with it, so a run nobody
//! watched (the dashboard's) still leaves a trace; only a cancelled `run`
//! rolls back whatever it made (`cancel`).

use std::path::Path;

use anyhow::Result;

use crate::runtime::backend;
use crate::state::{Instance, Phase, Progress, State};

use super::host_git;

/// The phase a materialization is in, shared by `run` and `rebuild`: the
/// caller owns it so it can read where a failed `materialize` stopped. Every
/// phase change is written to the record (found by `instance_id`, which a
/// rename mid-run doesn't move).
pub(crate) struct Tracker {
    instance_id: String,
    phase: Phase,
    boot: Option<String>,
}

impl Tracker {
    /// Track `instance_id`, whose record already carries `phase` (`run`
    /// inserts it so). No write.
    pub(crate) fn new(instance_id: &str, phase: Phase) -> Self {
        Tracker {
            instance_id: instance_id.to_string(),
            phase,
            boot: crate::commands::autostart::boot_id(),
        }
    }

    /// The in-flight `progress` for the current phase.
    pub(crate) fn progress(&self) -> Progress {
        Progress { phase: self.phase.clone(), pid: std::process::id(), boot: self.boot.clone(), error: None }
    }

    /// Move to `phase` and record it. A phase boundary is also where a
    /// cancel that didn't kill the last step stops the command
    /// ([`super::cancel::check`]); the phase stays the one that completed.
    pub(crate) fn enter(&mut self, phase: Phase) -> Result<()> {
        super::cancel::check()?;
        self.phase = phase;
        let progress = self.progress();
        self.record(|i| i.progress = Some(progress))
    }

    /// Record the current phase on the existing entry (`rebuild`'s start).
    pub(crate) fn start(&self) -> Result<()> {
        let progress = self.progress();
        self.record(|i| i.progress = Some(progress))
    }

    /// Done: the instance is ready. Unless cancelled meanwhile: a Ctrl-C'd
    /// `docker exec` may still exit 0, so the last step's success proves
    /// nothing.
    pub(crate) fn finish(&self) -> Result<()> {
        super::cancel::check()?;
        self.record(|i| i.progress = None)
    }

    /// Mark the record `failed` at the current phase with `error`. Best
    /// effort: the caller is already returning `error`, which must win.
    pub(crate) fn fail(&self, error: &anyhow::Error) {
        let progress = Progress {
            phase: Phase::Failed { at: self.phase.label().to_string() },
            error: Some(format!("{error:#}")),
            ..self.progress()
        };
        if let Err(e) = self.record(|i| i.progress = Some(progress)) {
            eprintln!("warning: cannot record the failure in state: {e:#}");
        }
    }

    /// Apply `f` to this instance's record in the current state file. A
    /// missing record (removed meanwhile) is left missing.
    pub(crate) fn record(&self, f: impl FnOnce(&mut Instance)) -> Result<()> {
        State::update(|fresh| {
            if let Some(i) = fresh.instances.values_mut().find(|i| i.instance_id == self.instance_id) {
                f(i);
            }
            Ok(())
        })
    }

    pub(crate) fn on_failure(&self, fresh: bool) -> OnFailure {
        on_failure(fresh, super::cancel::requested())
    }
}

/// What a failed materialization leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnFailure {
    /// Undo everything recorded and drop the record ([`rollback`]).
    RollBack,
    /// Keep it all, marked `failed`: retry with `rebuild`, remove with `rm`.
    KeepFailed,
}

/// `fresh`: the record was made by this command (`run`), not a `rebuild` of an
/// instance that existed before, which is never rolled back. `cancelled`: the
/// user stopped it (SIGINT/SIGTERM) and asked for nothing to be left. Any
/// other failure, before the container exists included, is kept with its
/// build log: `rebuild` retries, `rm` removes.
pub(crate) fn on_failure(fresh: bool, cancelled: bool) -> OnFailure {
    if fresh && cancelled { OnFailure::RollBack } else { OnFailure::KeepFailed }
}

/// Whether the rollback deletes the worktree's branch: only one this run
/// created (`created`) whose tip is still the commit it was created at
/// (`start`): no commits beyond its start point, so nothing is lost. An
/// unknown start or tip keeps it.
pub(crate) fn delete_rolled_back_branch(created: bool, start: Option<&str>, tip: Option<&str>) -> bool {
    created && start.is_some() && start == tip
}

/// The commit `rev` resolves to in `dir`, if any.
pub(super) fn commit_of(dir: &Path, rev: &str) -> Option<String> {
    let out = host_git(dir).ok()?.args(["rev-parse", "--verify", "--quiet", rev]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Undo a failed `run`: everything the record of `instance_id` says was made,
/// then the record itself. `branch_start` is the commit a branch this run
/// created started at (see [`delete_rolled_back_branch`]). Every step is
/// best effort, warning on stderr: the caller returns the original error.
/// The worktrees are this run's own and nothing ran in them yet, so they're
/// removed with `--force` (copied `.worktreeinclude` files would block a
/// plain remove). Kept, as `rm` keeps them: the shell-history dir and the
/// shared `worktree-link` store; an ssh-agent link is left to `gc`.
pub(crate) fn rollback(instance_id: &str, branch_start: Option<&str>) {
    let Ok(state) = State::load() else {
        eprintln!("warning: rollback: cannot read state; `{instance_id}`'s resources are left");
        return;
    };
    let Some(info) = state.instances.values().find(|i| i.instance_id == instance_id).cloned() else {
        return;
    };
    let warn = |what: &str, e: anyhow::Error| eprintln!("warning: rollback: {what}: {e:#}");
    // Harmless when absent; covers a container `docker run` created and
    // failed to start, plus isolated services and the per-instance network.
    crate::commands::rm::remove_container(&info.container, &info.project, &info.instance_id);
    for volume in &info.volumes {
        if let Err(e) = backend().remove_volume(volume) {
            warn(&format!("volume `{volume}`"), e);
        }
    }
    if let Err(e) = crate::commands::rm::remove_folder_worktrees(&info.folders, true) {
        warn("`folders` worktrees", e);
    }
    if let Some(wt) = &info.worktree {
        match crate::commands::rm::remove_or_prune(&info.base_folder, wt, true) {
            Ok(()) => {
                if let Some(branch) = &info.branch {
                    let tip = commit_of(&info.base_folder, &format!("refs/heads/{branch}"));
                    if delete_rolled_back_branch(info.branch_created, branch_start, tip.as_deref()) {
                        crate::commands::rm::delete_branch_ref(&info.base_folder, branch);
                    }
                }
            }
            // A worktree left on disk keeps its branch (checked out there).
            Err(e) => warn(&format!("worktree `{}`", wt.display()), e),
        }
    }
    let removed = State::update(|fresh| {
        fresh.instances.retain(|_, i| i.instance_id != instance_id);
        Ok(())
    });
    match removed {
        // The build log goes with the record, as on `rm` (a `logs --build
        // --follow` reading it still gets the rest: it holds the file open).
        Ok(()) => crate::build_log::remove(instance_id),
        Err(e) => warn("state record", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_cancelled_run_rolls_back() {
        assert_eq!(on_failure(true, true), OnFailure::RollBack);
        assert_eq!(on_failure(true, false), OnFailure::KeepFailed);
        // `rebuild`: the instance existed before, never rolled back.
        assert_eq!(on_failure(false, true), OnFailure::KeepFailed);
        assert_eq!(on_failure(false, false), OnFailure::KeepFailed);
    }

    #[test]
    fn rollback_deletes_only_an_untouched_branch_it_created() {
        assert!(delete_rolled_back_branch(true, Some("abc"), Some("abc")));
        // Moved on since it was created: has commits of its own.
        assert!(!delete_rolled_back_branch(true, Some("abc"), Some("def")));
        // Reused (local or remote-tracking): never ours to delete.
        assert!(!delete_rolled_back_branch(false, Some("abc"), Some("abc")));
        // Unknown start or tip: keep.
        assert!(!delete_rolled_back_branch(true, None, Some("abc")));
        assert!(!delete_rolled_back_branch(true, Some("abc"), None));
        assert!(!delete_rolled_back_branch(true, None, None));
    }
}
