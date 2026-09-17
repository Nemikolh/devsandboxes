use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{resolve_instance, run, services};
use crate::config::{Config, ResolvedSandbox};
use crate::runtime::backend;
use crate::state::{Instance, State};

/// Recreate an instance's container from the *current* config, preserving the
/// worktree, branch, instance name, and per-instance state (shell history,
/// `${instance}`-anchored mounts). No config drift is a no-op with a message.
///
/// Unlike `start`, which falls back to a bare container start when the config
/// no longer resolves, `rebuild` bails: re-materializing against a config that
/// doesn't match the instance would silently rebuild it wrong.
pub fn rebuild(dir: &Path, name: &str) -> Result<()> {
    let mut state = State::load()?;
    let key = resolve_instance(&state, name)?;
    let info = state.instances.get(&key).expect("key came from state");

    let config = Config::load(dir)
        .with_context(|| format!("cannot load config to rebuild `{key}`"))?;
    let project = services::project_id(dir)?;
    let sandbox = resolve(&config, info, &project, &key)?;

    // Drift check against the container's recorded hash.
    // - `Some(hash)` equal   → nothing to do.
    // - `Some(hash)` differ  → rebuild.
    // - `None` (container or label gone, e.g. removed manually) → rebuild anyway.
    //   It is the only worktree-preserving way back to a live instance: `run`
    //   refuses the taken name and `rm` destroys the worktree.
    if let Some(hash) = backend().label(&info.container, "devsandbox.config_hash")?
        && hash == sandbox.config_hash
    {
        println!("no config drift for `{key}`; nothing to do");
        return Ok(());
    }

    // Clone everything materialize needs out of `info` before borrowing state
    // mutably; `folder` is the mounted working tree (worktree or base folder),
    // `base_folder` the canonicalized base.
    let sandbox_name = info.sandbox.clone();
    let container = info.container.clone();
    let source = info.folder.clone();
    let base_folder = info.base_folder.clone();
    let worktree = info.worktree.clone();
    let branch = info.branch.clone();

    // Remove the old container; ignore failure (it may already be gone).
    let _ = backend().remove_force(&container);

    // materialize replaces the state entry (insert overwrites) and refreshes
    // config-derived fields (workspace, remote_env, remote_user, workspace_file)
    // — intended: the point of rebuild is to re-apply the current config.
    run::materialize(
        dir,
        &config,
        &sandbox_name,
        &sandbox,
        &key,
        &source,
        &base_folder,
        worktree,
        branch,
        &mut state,
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
            project: project.into(),
            container: "devsandbox-web".into(),
            folder: "/home/u/site".into(),
            base_folder: "/home/u/site".into(),
            worktree: None,
            branch: None,
            shell_history: None,
            workspace: "/w".into(),
            workspace_file: None,
            remote_env: BTreeMap::new(),
            remote_user: None,
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
