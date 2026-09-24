use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A running (or previously started) sandbox instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    /// Sandbox config name from config.toml.
    pub sandbox: String,
    /// Persistent identity: the instance's name at creation, unique across all
    /// instances and never changed by `rename` (which only moves the state
    /// key). Everything host- or runtime-addressed keys off this — `${instance}`
    /// in mounts/branches, the container name, isolated services/networks,
    /// managed shell history — so a rename never moves per-instance state.
    /// Backfilled to the state key by [`State::load`] for pre-upgrade entries.
    #[serde(default)]
    pub instance_id: String,
    /// Config-root id (see `services::project_id`); scopes this instance's
    /// isolated services and networks so `rm` can find them without the dir.
    #[serde(default)]
    pub project: String,
    /// Docker container name (prefixed).
    pub container: String,
    /// Absolute path of the mounted project folder on the host (the worktree
    /// path for worktree instances, otherwise the base folder).
    pub folder: PathBuf,
    /// Absolute path of the base repo folder this instance derives from. Equals
    /// `folder` for the first instance; differs when `worktree` is set.
    #[serde(default)]
    pub base_folder: PathBuf,
    /// Git worktree path, when this instance runs against a worktree rather than
    /// the base folder directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    /// Git branch created for this instance's worktree, recorded so `rm` deletes
    /// exactly the branch `run` created even if the config changed since. Only
    /// set for worktree instances.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Host path of this instance's managed `.zsh_history` (inside the
    /// per-instance history dir bind-mounted at `/commandhistory`), when
    /// `persist-shell-history` is on. Kept on `rm` so a rebuilt instance with
    /// the same name inherits its history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_history: Option<PathBuf>,
    /// Workspace folder inside the container.
    pub workspace: String,
    /// Container path of the generated `.code-workspace` file, when one was
    /// written for this instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_file: Option<String>,
    /// devcontainer `remoteEnv`, applied to every exec in the container.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub remote_env: BTreeMap<String, String>,
    /// devcontainer `remoteUser`: the user every exec runs as, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_user: Option<String>,
    /// Container path of the forwarded ssh-agent socket, set when `run`
    /// mounted the host agent (see docs/ssh-agent.md); `exec_argv` uses it
    /// to inject `SSH_AUTH_SOCK` per exec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_auth_sock: Option<String>,
    /// Arch of the devsbd helper that ran in this container (see
    /// docs/sandbox-helper.md); `None` = helper unavailable. Tried first on
    /// the next install so emulated images skip the host-arch attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devsbd_arch: Option<crate::devsbd::Arch>,
    pub created_unix: u64,
}

impl Instance {
    pub fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Host-side runtime state, stored in the user data dir. No daemon: docker is
/// the source of truth for liveness, this only maps names to containers.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default, rename = "instance")]
    pub instances: BTreeMap<String, Instance>,
}

impl State {
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .context("cannot determine user data dir ($XDG_DATA_HOME or $HOME)")?;
        Ok(base.join("devsandbox/state.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let mut state: Self = toml::from_str(&contents)
                    .with_context(|| format!("invalid state file {}", path.display()))?;
                // Pre-upgrade entries carry no id; the key is the best available
                // identity (equal to the creation name unless the instance was
                // renamed before ids existed).
                for (key, instance) in &mut state.instances {
                    if instance.instance_id.is_empty() {
                        instance.instance_id = key.clone();
                    }
                }
                Ok(state)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let contents = toml::to_string_pretty(self).context("cannot serialize state")?;
        std::fs::write(&path, contents)
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let mut state = State::default();
        state.instances.insert(
            "repo-abc1".into(),
            Instance {
                sandbox: "repository-1".into(),
                instance_id: "repo-abc1".into(),
                project: "abc12345".into(),
                container: "devsandbox-repo-abc1".into(),
                folder: "/home/u/repository-1".into(),
                base_folder: "/home/u/repository-1".into(),
                worktree: None,
                branch: None,
                shell_history: None,
                workspace: "/workspaces/repository-1".into(),
                workspace_file: None,
                remote_env: BTreeMap::new(),
                remote_user: None,
                ssh_auth_sock: None,
                devsbd_arch: None,
                created_unix: Instance::now(),
            },
        );
        let text = toml::to_string_pretty(&state).unwrap();
        let back: State = toml::from_str(&text).unwrap();
        assert_eq!(back.instances["repo-abc1"].sandbox, "repository-1");
        assert_eq!(back.instances["repo-abc1"].instance_id, "repo-abc1");
    }

    #[test]
    fn missing_instance_id_deserializes_empty() {
        // Pre-upgrade state files have no `instance_id`; serde defaults it to
        // empty and `load()` backfills it from the key (exercised there, not
        // here, since `load()` reads the real state path).
        let state: State = toml::from_str(
            r#"
            [instance.repo]
            sandbox = "web"
            container = "devsandbox-repo"
            folder = "/home/u/web"
            workspace = "/workspaces/web"
            created_unix = 0
            "#,
        )
        .unwrap();
        assert_eq!(state.instances["repo"].instance_id, "");
    }
}
