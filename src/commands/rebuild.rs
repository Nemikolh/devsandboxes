use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{container_drifted, resolve_instance, run, services};
use crate::config::{build_hash, Config, ResolvedSandbox};
use crate::runtime::backend;
use crate::state::{Instance, State};

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
pub fn rebuild(dir: &Path, name: Option<String>, all: bool, force: bool) -> Result<()> {
    let mut state = State::load()?;
    let config = Config::load(dir).context("cannot load config to rebuild against")?;
    let project = services::project_id(dir)?;

    if all {
        let mut rebuilt = 0;
        // Keys first: `rebuild_instance` needs `&mut state` (materialize
        // replaces the entry), so we can't hold an iterator over it.
        for key in state.instances.keys().cloned().collect::<Vec<_>>() {
            let info = state.instances.get(&key).expect("key came from state");
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
            rebuild_instance(dir, &config, &sandbox, &key, &mut state)?;
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
    let sandbox = resolve(&config, info, &project, &key)?;

    if !force && !needs_rebuild(dir, &info.container, &sandbox)? {
        println!("no config drift for `{key}`; nothing to do (--force overrides)");
        return Ok(());
    }
    rebuild_instance(dir, &config, &sandbox, &key, &mut state)
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
    let build = build_hash(dir, sandbox.properties.build.as_ref());
    container_drifted(container, &sandbox.config_hash, &build)
}

/// Remove `key`'s container and re-materialize it from `config`, reusing the
/// stored source/worktree/branch so the working tree survives.
fn rebuild_instance(
    dir: &Path,
    config: &Config,
    sandbox: &ResolvedSandbox,
    key: &str,
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

    // materialize replaces the state entry (insert overwrites) and refreshes
    // config-derived fields (workspace, remote_env, remote_user, workspace_file)
    // — intended: the point of rebuild is to re-apply the current config.
    run::materialize(
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
        // No extras: `materialize` keeps the recorded dispatcher; `--env`s
        // aren't recorded, so they are dropped.
        &run::RunExtras::default(),
        state,
    )?;

    println!("rebuilt {key}");
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
            ssh_auth_sock: None,
            devsbd_arch: None,
            volumes: Vec::new(),
            dispatcher: None,
            config_dir: None,
            extra_env: Default::default(),
            forwarded_ports: Default::default(),
            done: None,
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
    fn resolve_bails_when_sandbox_gone() {
        let cfg = config();
        let mut info = instance("proj1234");
        info.sandbox = "api".into();
        let err = resolve(&cfg, &info, "proj1234", "api").unwrap_err().to_string();
        assert!(err.contains("gone from the config"), "{err}");
    }
}
