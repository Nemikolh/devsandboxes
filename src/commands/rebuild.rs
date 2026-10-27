use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{container_drifted, resolve_instance, run, services};
use crate::config::{sandbox_build_hash, Config, ResolvedSandbox};
use crate::runtime::backend;
use crate::state::{Instance, Phase, PhaseView, State};

/// Recreate an instance's container from the *current* config, preserving the
/// worktree, branch, instance identity (name *and* persistent id, so shell
/// history and `${instance}`-anchored mounts stay put even after a rename).
/// No config drift is a no-op with a message, unless `force` — the escape
/// hatch for devsandbox-side behavior changes (rc wiring, mount layout, …)
/// that the drift hashes cannot see.
/// `--all` rebuilds every drifted instance (every instance with `force`),
/// skipping (with a note) ones that don't resolve here instead of aborting the
/// batch.
///
/// Unlike `start`, which falls back to a bare container start when the config
/// no longer resolves, `rebuild` bails: re-materializing against a config that
/// doesn't match the instance would silently rebuild it wrong.
///
/// A `failed` instance (docs/architecture/instances/lifecycle.md) is always
/// rebuilt when named: that's how it's retried. One still being materialized
/// is refused, and `--all` skips both.
///
/// `reprobe` re-runs the identity probe (`RunExtras::reprobe`); it doesn't
/// count as drift.
pub fn rebuild(dir: &Path, name: Option<String>, all: bool, force: bool, reprobe: bool) -> Result<()> {
    // Ctrl-C/SIGTERM mid-rebuild leaves the instance `failed` ("cancelled").
    let _cancel = run::cancel::catch();
    let mut state = State::load()?;
    let config = Config::load(dir).context("cannot load config to rebuild against")?;
    let project = services::project_id(dir)?;

    if all {
        let mut rebuilt = 0;
        // Keys first: `rebuild_instance` needs `&mut state` (materialize
        // replaces the entry), so we can't hold an iterator over it.
        for key in state.instances.keys().cloned().collect::<Vec<_>>() {
            run::cancel::check()?;
            let info = state.instances.get(&key).expect("key came from state");
            if let Some(why) = super::not_ready(&key, info.phase_view().as_ref()) {
                eprintln!("skipping: {why}");
                continue;
            }
            // A batch shouldn't die on the first foreign instance; note and go on.
            let sandbox = match resolve(&config, info, &project, &key) {
                Ok(sandbox) => sandbox,
                Err(e) => {
                    eprintln!("skipping `{key}`: {e:#}");
                    continue;
                }
            };
            if !force && !needs_rebuild(dir, &info.container, &sandbox)? {
                continue;
            }
            rebuild_instance(dir, &config, &sandbox, &key, reprobe, &mut state)?;
            rebuilt += 1;
        }
        if rebuilt == 0 {
            println!("nothing to rebuild");
        }
        return Ok(());
    }

    let name = name.expect("clap requires a name without --all");
    let key = resolve_instance(&state, &name)?;
    let info = state.instances.get(&key).expect("key came from state");
    let view = info.phase_view();
    if let Some(why) = super::in_flight(&key, info.progress.as_ref(), view.as_ref()) {
        bail!("{why}");
    }
    let failed = matches!(view, Some(PhaseView::Failed { .. }));
    if failed {
        resumable(&key, info)?;
    }
    let sandbox = resolve(&config, info, &project, &key)?;

    // A failed one may have its container with no drift (it failed in a
    // lifecycle step): retrying is the point, so it's always rebuilt.
    if !force && !failed && !needs_rebuild(dir, &info.container, &sandbox)? {
        println!("no config drift for `{key}`; nothing to do (--force overrides)");
        return Ok(());
    }
    rebuild_instance(dir, &config, &sandbox, &key, reprobe, &mut state)
}

/// Drift check against the container's recorded config *and* build hashes.
/// - config or build hash differs → rebuild (the shared [`container_drifted`]
///   rule; a dockerfile edit shows up as build-hash drift).
/// - config label `None` (container or label gone, e.g. removed manually) →
///   rebuild anyway. It is the only worktree-preserving way back to a live
///   instance: `run` refuses the taken name and `rm` destroys the worktree.
///   This recovery case is unique to `rebuild`, so it lives here rather than in
///   the shared rule.
fn needs_rebuild(dir: &Path, container: &str, sandbox: &ResolvedSandbox) -> Result<bool> {
    if backend().label(container, "devsandbox.config_hash")?.is_none() {
        return Ok(true);
    }
    let build = sandbox_build_hash(dir, &sandbox.properties);
    container_drifted(container, &sandbox.config_hash, &build)
}

/// Remove `key`'s container and re-materialize it from `config`, reusing the
/// stored source/worktree/branch so the working tree survives. Its output
/// goes to the instance's build log too (one log per instance under `--all`).
fn rebuild_instance(
    dir: &Path,
    config: &Config,
    sandbox: &ResolvedSandbox,
    key: &str,
    reprobe: bool,
    state: &mut State,
) -> Result<()> {
    let instance_id = state.instances.get(key).expect("key came from state").instance_id.clone();
    let log = crate::build_log::open(&instance_id);
    log.outcome(recreate(dir, config, sandbox, key, reprobe, state))
}

/// [`rebuild_instance`]'s work, inside the build log.
fn recreate(
    dir: &Path,
    config: &Config,
    sandbox: &ResolvedSandbox,
    key: &str,
    reprobe: bool,
    state: &mut State,
) -> Result<()> {
    // Clone everything materialize needs out of the entry before borrowing
    // state mutably; `folder` is the mounted working tree (worktree or base
    // folder), `base_folder` the canonicalized base.
    let info = state.instances.get(key).expect("key came from state");
    let sandbox_name = info.sandbox.clone();
    let instance_id = info.instance_id.clone();
    let container = info.container.clone();
    let source = info.folder.clone();
    let base_folder = info.base_folder.clone();
    let worktree = info.worktree.clone();
    let branch = info.branch.clone();
    let branch_created = info.branch_created;

    // Remove the old container; ignore failure (it may already be gone).
    let _ = backend().remove_force(&container);

    // In flight from here (docs/architecture/instances/lifecycle.md). The
    // instance existed before, so a failure is never rolled back: it stays as
    // `failed`, the worktree and branch with it, for another rebuild or an
    // `rm`.
    let mut tracker = run::Tracker::new(&instance_id, Phase::Building);
    tracker.start()?;

    // materialize replaces the state entry (insert overwrites) and refreshes
    // config-derived fields (workspace, remote_env, workspace_file) and the
    // identity plan's (remote_user, home) — intended: the point of rebuild is
    // to re-apply the current config.
    let result = run::materialize(
        dir,
        config,
        &sandbox_name,
        sandbox,
        key,
        &instance_id,
        &source,
        &base_folder,
        worktree,
        branch,
        branch_created,
        // No env or dispatcher: `materialize` keeps the recorded ones
        // (`Instance::extra_env`, `effective_extra_env`).
        &run::RunExtras { reprobe, ..Default::default() },
        state,
        &mut tracker,
    );
    if let Err(e) = result {
        debug_assert_eq!(tracker.on_failure(false), run::OnFailure::KeepFailed);
        if run::cancel::requested() {
            tracker.fail(&anyhow::anyhow!(run::cancel::CANCELLED));
            bail!("rebuild of `{key}` {}; {}", run::cancel::CANCELLED, run::kept_hint(key));
        }
        tracker.fail(&e);
        bail!("{e:#}; {}", run::kept_hint(key));
    }

    println!("rebuilt {key}");
    Ok(())
}

/// Whether a failed `info` can be retried by a rebuild: `materialize` mounts
/// the recorded working tree, which a `run` interrupted in `preparing` may
/// never have made (its record still names the planned worktree path).
fn resumable(key: &str, info: &Instance) -> Result<()> {
    if info.worktree.is_none() && info.folder != info.base_folder {
        bail!(
            "`{key}` was interrupted before its worktree was made; nothing to retry: \
             `devsandbox rm {key}` removes it, then `devsandbox run` again"
        );
    }
    Ok(())
}

/// Resolve the instance's sandbox against this config root, bailing (rather than
/// falling back like `start`) when the instance was created from a different
/// config root or the sandbox is gone from the config.
fn resolve(
    config: &Config,
    info: &Instance,
    project: &str,
    key: &str,
) -> Result<ResolvedSandbox> {
    // Empty `project` is pre-upgrade state with no recorded root; skip the check.
    if !info.project.is_empty() && info.project != project {
        bail!(
            "`{key}` was created from a different config root; \
             rerun with `-C <root>`"
        );
    }
    config
        .resolve_sandbox(&info.sandbox)
        .with_context(|| format!("sandbox `{}` is gone from the config", info.sandbox))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::state::Instance;

    use super::*;

    fn instance(project: &str) -> Instance {
        Instance {
            sandbox: "web".into(),
            instance_id: "web".into(),
            project: project.into(),
            container: "devsandbox-web".into(),
            folder: "/home/u/site".into(),
            base_folder: "/home/u/site".into(),
            worktree: None,
            branch: None,
            branch_created: true,
            folders: Vec::new(),
            shell_history: None,
            workspace: "/w".into(),
            workspace_file: None,
            remote_env: BTreeMap::new(),
            remote_user: None,
            home: None,
            browser_sandbox: None,
            tool_env: Default::default(),
            ssh_auth_sock: None,
            devsbd_arch: None,
            volumes: Vec::new(),
            dispatcher: None,
            config_dir: None,
            extra_env: Default::default(),
            forwarded_ports: Default::default(),
            done: None,
            progress: None,
            created_unix: 0,
        }
    }

    fn config() -> Config {
        // A config with a single `web` sandbox pointing at an image.
        toml::from_str(
            r#"
            [sandbox.web]
            folder = "."
            image = "alpine:3.20"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn resolve_ok_when_project_matches() {
        let cfg = config();
        let info = instance("proj1234");
        assert!(resolve(&cfg, &info, "proj1234", "web").is_ok());
    }

    #[test]
    fn resolve_ok_when_project_empty() {
        // Pre-upgrade state with no recorded root skips the mismatch check.
        let cfg = config();
        let info = instance("");
        assert!(resolve(&cfg, &info, "proj1234", "web").is_ok());
    }

    #[test]
    fn resolve_bails_on_project_mismatch() {
        let cfg = config();
        let info = instance("other-root");
        let err = resolve(&cfg, &info, "proj1234", "web").unwrap_err().to_string();
        assert!(err.contains("different config root"), "{err}");
        assert!(err.contains("-C <root>"), "{err}");
    }

    #[test]
    fn resumable_needs_the_recorded_working_tree() {
        // Base-folder instance: always resumable.
        assert!(resumable("web", &instance("p")).is_ok());
        // A worktree instance with its worktree recorded.
        let mut info = instance("p");
        info.folder = "/cfg/.worktrees/web".into();
        info.worktree = Some("/cfg/.worktrees/web".into());
        assert!(resumable("web", &info).is_ok());
        // Interrupted before `create_worktree` recorded it.
        info.worktree = None;
        let err = resumable("web", &info).unwrap_err().to_string();
        assert!(err.contains("devsandbox rm web"), "{err}");
    }

    #[test]
    fn resolve_bails_when_sandbox_gone() {
        let cfg = config();
        let mut info = instance("proj1234");
        info.sandbox = "api".into();
        let err = resolve(&cfg, &info, "proj1234", "api").unwrap_err().to_string();
        assert!(err.contains("gone from the config"), "{err}");
    }
}
