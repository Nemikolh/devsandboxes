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
    /// Whether `run` created `branch` (false when the worktree reused an
    /// existing local branch or one tracking `origin/<branch>`), so `rm` only
    /// offers to delete branches devsandbox made. Defaults to true: entries
    /// from before reuse existed always had a created branch.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub branch_created: bool,
    /// Every `folders` entry as mounted (direct binds included), recorded so
    /// the ownership rule (docs/folders-worktrees.md) sees other instances'
    /// direct extra mounts and `rm` removes exactly the extra worktrees `run`
    /// created. Empty for pre-upgrade instances until they're rebuilt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<FolderMount>,
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
    /// Per-instance named volumes (sources using `${instance}` /
    /// `${devcontainerId}`, e.g. docker-in-docker's `/var/lib/docker`),
    /// recorded so `rm` deletes exactly these even if the config changed since.
    /// Accumulated across rebuilds, which keep them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    /// `instance_id` of the dispatcher that created this instance through the
    /// control API (docs/automations.md); also the container's
    /// `devsandbox.dispatcher` label. `None` for user-created instances.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatcher: Option<String>,
    /// Canonical config root this instance was created from, so a request
    /// arriving with only the instance (the control API) can re-read its
    /// config. `None` for instances created before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<PathBuf>,
    /// Extra `-e K=V` from `run --env` (a dispatcher's `ensure --env`),
    /// recorded so `rebuild` recreates the container with them. `ensure` on an
    /// existing child replaces them; every devsandbox exec applies the saved
    /// values ([`Instance::exec_env`]), so they take effect before a rebuild.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_env: BTreeMap<String, String>,
    /// Host ports the TUI picked for this instance's `forwardPorts`, keyed by
    /// `ForwardPort::key` (`"8080:3000"` = 8081), so the instance gets the same
    /// ports every time and no other instance takes them while it's stopped.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub forwarded_ports: BTreeMap<String, u16>,
    pub created_unix: u64,
}

/// One `folders` entry of an instance: where it's mounted, the host folder it
/// derives from, and the detached worktree mounted instead, if any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderMount {
    /// Container path.
    pub target: String,
    /// Canonical host folder the entry names.
    pub base: PathBuf,
    /// Detached worktree of `base` mounted at `target`; `None` = `base` is
    /// bind-mounted directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

impl Instance {
    pub fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// The env every devsandbox exec into this instance sets (CLI/TUI exec,
    /// dispatcher run ops, lifecycle commands, the boot file): `remoteEnv`
    /// overlaid by `extra_env`. The instance-specific value wins, as at
    /// `docker run` where `--env` comes after `containerEnv`; and the saved
    /// `extra_env` may be newer than the container's own env (an `ensure`
    /// replaced it), so it must override whatever the image or config set.
    pub fn exec_env(&self) -> BTreeMap<String, String> {
        let mut env = self.remote_env.clone();
        env.extend(self.extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env
    }
}

/// Host-side runtime state, stored in the user data dir. No daemon: docker is
/// the source of truth for liveness, this only maps names to containers.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default, rename = "instance")]
    pub instances: BTreeMap<String, Instance>,
    /// Project id (`services::project_id`) -> host boot id of the last
    /// autostart pass for that config root, so autostart runs once per boot.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub autostart_boot: BTreeMap<String, String>,
    /// Project id -> `forwardPorts` entry key -> host port, for entries that
    /// forward a `global` service: one forward per config root however many
    /// instances declare it, so its port can't live on any one instance.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub global_forwarded_ports: BTreeMap<String, BTreeMap<String, u16>>,
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
                branch_created: true,
                folders: Vec::new(),
                shell_history: None,
                workspace: "/workspaces/repository-1".into(),
                workspace_file: None,
                remote_env: BTreeMap::new(),
                remote_user: None,
                ssh_auth_sock: None,
                devsbd_arch: None,
                volumes: Vec::new(),
                dispatcher: Some("pr-dispatcher".into()),
                config_dir: Some("/home/u/.devsandboxes".into()),
                extra_env: BTreeMap::from([("PR_NUMBER".into(), "42".into())]),
                forwarded_ports: BTreeMap::from([("8080:3000".into(), 8081)]),
                created_unix: Instance::now(),
            },
        );
        state.autostart_boot.insert("abc12345".into(), "3f2c-boot".into());
        let text = toml::to_string_pretty(&state).unwrap();
        let back: State = toml::from_str(&text).unwrap();
        assert_eq!(back.instances["repo-abc1"].sandbox, "repository-1");
        assert_eq!(back.instances["repo-abc1"].instance_id, "repo-abc1");
        assert_eq!(back.autostart_boot["abc12345"], "3f2c-boot");
        assert_eq!(back.instances["repo-abc1"].dispatcher.as_deref(), Some("pr-dispatcher"));
        assert_eq!(
            back.instances["repo-abc1"].config_dir.as_deref(),
            Some(std::path::Path::new("/home/u/.devsandboxes"))
        );
        assert_eq!(back.instances["repo-abc1"].extra_env["PR_NUMBER"], "42");
        assert_eq!(back.instances["repo-abc1"].forwarded_ports["8080:3000"], 8081);
    }

    #[test]
    fn empty_autostart_boot_is_omitted() {
        let text = toml::to_string_pretty(&State::default()).unwrap();
        assert!(!text.contains("autostart_boot"), "{text}");
        let back: State = toml::from_str(&text).unwrap();
        assert!(back.autostart_boot.is_empty());
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
        assert_eq!(state.instances["repo"].dispatcher, None);
        assert_eq!(state.instances["repo"].config_dir, None);
        assert!(state.instances["repo"].extra_env.is_empty());
        // Pre-reuse entries always had a created branch: `rm` keeps offering it.
        assert!(state.instances["repo"].branch_created);
        let text = toml::to_string_pretty(&state).unwrap();
        assert!(!text.contains("dispatcher") && !text.contains("config_dir"), "{text}");
        assert!(!text.contains("branch_created"), "{text}");
        assert!(!text.contains("extra_env"), "{text}");
    }

    #[test]
    fn exec_env_overlays_extra_env_on_remote_env() {
        let mut state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        let inst = state.instances.get_mut("a").unwrap();
        assert!(inst.exec_env().is_empty());
        inst.remote_env = BTreeMap::from([("A".into(), "remote".into()), ("B".into(), "b".into())]);
        inst.extra_env = BTreeMap::from([("A".into(), "extra".into()), ("C".into(), "c".into())]);
        assert_eq!(
            inst.exec_env(),
            BTreeMap::from([
                ("A".into(), "extra".into()),
                ("B".into(), "b".into()),
                ("C".into(), "c".into()),
            ])
        );
    }

    #[test]
    fn reused_branch_roundtrips() {
        let mut state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        state.instances.get_mut("a").unwrap().branch_created = false;
        let text = toml::to_string_pretty(&state).unwrap();
        let back: State = toml::from_str(&text).unwrap();
        assert!(!back.instances["a"].branch_created, "{text}");
    }
}
