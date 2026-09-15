use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A running (or previously started) sandbox instance.
#[derive(Debug, Serialize, Deserialize)]
pub struct Instance {
    /// Sandbox config name from config.toml.
    pub sandbox: String,
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
    /// Host path of this instance's managed `.zsh_history`, when
    /// `persist-shell-history` is on. Removed with the instance.
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
            Ok(contents) => toml::from_str(&contents)
                .with_context(|| format!("invalid state file {}", path.display())),
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
                project: "abc12345".into(),
                container: "devsandbox-repo-abc1".into(),
                folder: "/home/u/repository-1".into(),
                base_folder: "/home/u/repository-1".into(),
                worktree: None,
                shell_history: None,
                workspace: "/workspaces/repository-1".into(),
                workspace_file: None,
                remote_env: BTreeMap::new(),
                remote_user: None,
                created_unix: Instance::now(),
            },
        );
        let text = toml::to_string_pretty(&state).unwrap();
        let back: State = toml::from_str(&text).unwrap();
        assert_eq!(back.instances["repo-abc1"].sandbox, "repository-1");
    }
}
