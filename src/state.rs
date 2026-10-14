use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A running (or previously started) sandbox instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    /// Sandbox config name from devsandboxes.toml.
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
    /// Unix secs since the instance was marked done (docs/inbox-threads.md
    /// "Done instances"): the user (or its dispatcher) is finished with it,
    /// but it's kept as is until removed. Only display and dispatcher policy
    /// read it; `start`/autostart leave it set. `None` = not done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<u64>,
    /// Set while a `run`/`rebuild` is materializing this instance, and kept
    /// when one fails from the container on (docs/instance-phase.md). It's
    /// what lets `run` record the instance before any of its resources
    /// exist, so a failed or killed run leaves a record `rm` can find
    /// instead of an orphaned worktree, branch and services. `None` = ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    pub created_unix: u64,
}

/// Where an in-flight (or failed) materialization is, and who's doing it.
/// Serialized flat (see [`ProgressToml`]) so it reads as one small table:
///
/// ```toml
/// [instance.web.progress]
/// phase = "lifecycle"
/// step = "postCreateCommand"
/// pid = 4242
/// boot = "3f2c…"
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(into = "ProgressToml", try_from = "ProgressToml")]
pub struct Progress {
    pub phase: Phase,
    /// The process doing the work: when it's gone the phase can't advance,
    /// so readers show it as interrupted (SIGKILL, OOM) without anyone
    /// rewriting the record.
    pub pid: u32,
    /// Host boot id when the phase was recorded: after a reboot `pid` may
    /// name an unrelated process, so a changed boot id means interrupted too.
    pub boot: Option<String>,
    /// The error a `Failed` phase failed with.
    pub error: Option<String>,
}

/// The materialization phases, in order (`Failed` aside).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Worktree, branch, `folders`, links, services.
    Preparing,
    /// The image.
    Building,
    /// The container.
    Creating,
    /// A lifecycle step (`onCreateCommand` … `postAttachCommand`, or
    /// `shell-rc` for the rc wiring), by name.
    Lifecycle(String),
    /// Failed at `at` (an in-flight phase's [`Phase::label`]); the error is
    /// [`Progress::error`].
    Failed { at: String },
}

impl Phase {
    /// What readers show for this phase; a lifecycle step shows as its name.
    pub fn label(&self) -> &str {
        match self {
            Phase::Preparing => "preparing",
            Phase::Building => "building",
            Phase::Creating => "creating",
            Phase::Lifecycle(step) => step,
            Phase::Failed { .. } => "failed",
        }
    }
}

/// What a reader shows for a [`Progress`], liveness and boot id applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseView {
    /// Still being worked on: the phase's [`Phase::label`].
    InFlight(String),
    /// Failed (or interrupted) at `at`.
    Failed { at: String, error: String },
}

impl Progress {
    /// An in-flight phase whose process is dead, or that was recorded in an
    /// earlier boot, is interrupted: derived here at read time rather than
    /// written back, so no daemon (or later command) has to clean it up.
    /// `alive` and `boot` are injected to keep this pure.
    pub fn view(&self, alive: impl Fn(u32) -> bool, boot: Option<&str>) -> PhaseView {
        if let Phase::Failed { at } = &self.phase {
            return PhaseView::Failed {
                at: at.clone(),
                error: self.error.clone().unwrap_or_else(|| "failed".into()),
            };
        }
        let rebooted = matches!((self.boot.as_deref(), boot), (Some(then), Some(now)) if then != now);
        if rebooted || !alive(self.pid) {
            return PhaseView::Failed { at: self.phase.label().into(), error: "interrupted".into() };
        }
        PhaseView::InFlight(self.phase.label().into())
    }

    /// [`Progress::view`] against this host: real pid liveness, current boot.
    pub fn current_view(&self) -> PhaseView {
        self.view(pid_alive, crate::commands::autostart::boot_id().as_deref())
    }
}

/// Whether process `pid` exists. EPERM means it exists under another user.
/// Non-unix has no cheap probe, so it's assumed alive (never a false
/// "interrupted").
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // 0 would probe our own process group, not a process.
        let Ok(pid @ 1..) = libc::pid_t::try_from(pid) else { return false };
        // SAFETY: signal 0 only checks for existence and permission.
        let ok = unsafe { libc::kill(pid, 0) } == 0;
        ok || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// [`Progress`] as stored: `phase` names the variant, `step` carries a
/// lifecycle step's name or the label a `failed` phase failed at. Flat
/// scalars, so it's plain TOML whatever the variant (an externally tagged
/// enum would mix a string and tables under one key).
#[derive(Serialize, Deserialize)]
struct ProgressToml {
    phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step: Option<String>,
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl From<Progress> for ProgressToml {
    fn from(p: Progress) -> Self {
        let (phase, step) = match p.phase {
            Phase::Preparing => ("preparing", None),
            Phase::Building => ("building", None),
            Phase::Creating => ("creating", None),
            Phase::Lifecycle(step) => ("lifecycle", Some(step)),
            Phase::Failed { at } => ("failed", Some(at)),
        };
        ProgressToml { phase: phase.into(), step, pid: p.pid, boot: p.boot, error: p.error }
    }
}

impl TryFrom<ProgressToml> for Progress {
    type Error = String;

    /// Lenient on purpose: a bad `progress` would otherwise fail loading the
    /// whole `state.toml`, so a phase a newer binary added (or a missing
    /// `step`) reads as an in-flight step named after it, which the pid and
    /// boot checks in [`Progress::view`] still resolve.
    fn try_from(t: ProgressToml) -> Result<Self, String> {
        let step = |phase: &str| t.step.clone().unwrap_or_else(|| phase.to_string());
        let phase = match t.phase.as_str() {
            "preparing" => Phase::Preparing,
            "building" => Phase::Building,
            "creating" => Phase::Creating,
            "lifecycle" => Phase::Lifecycle(step("lifecycle")),
            "failed" => Phase::Failed { at: step("failed") },
            other => Phase::Lifecycle(other.to_string()),
        };
        Ok(Progress { phase, pid: t.pid, boot: t.boot, error: t.error })
    }
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

    /// Where this instance's materialization stands on this host
    /// ([`Progress::current_view`]); `None` = ready.
    pub fn phase_view(&self) -> Option<PhaseView> {
        self.progress.as_ref().map(Progress::current_view)
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

    /// The saved state; a missing file is an empty one. Unlocked: writes are
    /// atomic, so a read always sees a complete file. A snapshot is only for
    /// reading; changes go through [`State::update`].
    pub fn load() -> Result<Self> {
        Self::load_at(&Self::path()?)
    }

    /// [`State::load`] from `path`: the daemon's verb tracking reads an
    /// injected path, so its tests never touch the user's state.
    pub(crate) fn load_at(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
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

    /// Read-modify-write under an exclusive lock: re-read the file, apply `f`,
    /// and write it back (atomically, only if it changed). The only way to
    /// change state: saving a snapshot loaded earlier wrote back everything
    /// as of that load, so a long `run`/`rm` racing another command dropped
    /// the instance that command had recorded meanwhile, leaving its
    /// container and worktree with no state entry. `f` must apply only its
    /// own change, and must not call back into `update` (the lock isn't
    /// reentrant). An `Err` from `f` writes nothing.
    pub fn update<T>(f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        Self::update_at(&Self::path()?, f)
    }

    /// [`State::update`] of the file at `path`: the daemon's instance
    /// mutations take an injected path, so their tests never touch the
    /// user's state.
    pub(crate) fn update_at<T>(path: &Path, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let dir = path.parent().context("state path has no parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        // Separate from the state file, which the atomic write replaces.
        let lock_path = path.with_file_name("state.lock");
        let _lock = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .and_then(|file| file.lock().map(|()| file))
            .with_context(|| format!("cannot lock {}", lock_path.display()))?;
        let mut state = Self::load_at(path)?;
        let before = toml::to_string_pretty(&state).context("cannot serialize state")?;
        let out = f(&mut state)?;
        let after = toml::to_string_pretty(&state).context("cannot serialize state")?;
        if after != before {
            // Pid in the temp name: a stale temp from a killed writer is never reused.
            let tmp = path.with_file_name(format!(".state.toml.{}.tmp", std::process::id()));
            std::fs::write(&tmp, after).with_context(|| format!("cannot write {}", tmp.display()))?;
            std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
        }
        Ok(out)
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
                done: Some(1_790_900_000),
                progress: None,
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
        assert_eq!(back.instances["repo-abc1"].done, Some(1_790_900_000));
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
        assert_eq!(state.instances["repo"].done, None);
        // Pre-reuse entries always had a created branch: `rm` keeps offering it.
        assert!(state.instances["repo"].branch_created);
        let text = toml::to_string_pretty(&state).unwrap();
        assert!(!text.contains("dispatcher") && !text.contains("config_dir"), "{text}");
        assert!(!text.contains("branch_created"), "{text}");
        assert!(!text.contains("extra_env"), "{text}");
        assert!(!text.contains("done"), "{text}");
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

    /// The orphaned-child bug: two writers holding snapshots from before each
    /// other's change must both land, which a snapshot `save` didn't do.
    #[test]
    fn updates_apply_to_the_current_file_not_a_snapshot() {
        let dir = std::env::temp_dir().join(format!("devsandbox-state-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state.toml");
        let entry = |name: &str| -> Instance {
            toml::from_str(&format!(
                "sandbox = \"web\"\ninstance_id = \"{name}\"\ncontainer = \"devsandbox-{name}\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n"
            ))
            .unwrap()
        };
        State::update_at(&path, |s| {
            s.instances.insert("a".into(), entry("a"));
            Ok(())
        })
        .unwrap();
        // A long command loaded state now, before the child below exists.
        let stale = State::load_at(&path).unwrap();
        State::update_at(&path, |s| {
            s.instances.insert("child".into(), entry("child"));
            Ok(())
        })
        .unwrap();
        // Then it records its own change: the child must survive.
        assert!(!stale.instances.contains_key("child"));
        State::update_at(&path, |s| {
            s.instances.remove("a");
            Ok(())
        })
        .unwrap();
        let now = State::load_at(&path).unwrap();
        assert_eq!(now.instances.keys().collect::<Vec<_>>(), ["child"]);

        // A failed update writes nothing; an unchanged one doesn't rewrite.
        let mtime = || std::fs::metadata(&path).unwrap().modified().unwrap();
        let before = mtime();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let err = State::update_at(&path, |s| -> Result<()> {
            s.instances.clear();
            anyhow::bail!("boom")
        });
        assert!(err.is_err());
        State::update_at(&path, |_| Ok(())).unwrap();
        assert_eq!(mtime(), before);
        assert!(State::load_at(&path).unwrap().instances.contains_key("child"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn progress(phase: Phase) -> Progress {
        Progress { phase, pid: 4242, boot: Some("boot-a".into()), error: None }
    }

    #[test]
    fn progress_roundtrips_every_phase() {
        let phases = [
            Phase::Preparing,
            Phase::Building,
            Phase::Creating,
            Phase::Lifecycle("postCreateCommand".into()),
            Phase::Lifecycle("shell-rc".into()),
            Phase::Failed { at: "building".into() },
        ];
        for phase in phases {
            let mut state: State = toml::from_str(
                "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
            )
            .unwrap();
            let mut p = progress(phase);
            if matches!(p.phase, Phase::Failed { .. }) {
                p.error = Some("docker build exited 1".into());
            }
            state.instances.get_mut("a").unwrap().progress = Some(p.clone());
            let text = toml::to_string_pretty(&state).unwrap();
            let back: State = toml::from_str(&text).unwrap();
            assert_eq!(back.instances["a"].progress, Some(p), "{text}");
        }
    }

    #[test]
    fn progress_toml_shape() {
        let mut state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        state.instances.get_mut("a").unwrap().progress = Some(progress(Phase::Lifecycle("postCreateCommand".into())));
        let text = toml::to_string_pretty(&state).unwrap();
        let back: toml::Table = toml::from_str(&text).unwrap();
        let p = back["instance"]["a"]["progress"].as_table().unwrap();
        assert_eq!(p["phase"].as_str(), Some("lifecycle"), "{text}");
        assert_eq!(p["step"].as_str(), Some("postCreateCommand"), "{text}");
        assert_eq!(p["pid"].as_integer(), Some(4242), "{text}");
        assert_eq!(p["boot"].as_str(), Some("boot-a"), "{text}");
        assert!(!p.contains_key("error"), "{text}");
    }

    #[test]
    fn no_progress_loads_and_is_omitted() {
        let state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        assert_eq!(state.instances["a"].progress, None);
        let text = toml::to_string_pretty(&state).unwrap();
        assert!(!text.contains("progress"), "{text}");
    }

    #[test]
    fn odd_progress_still_loads() {
        let base = "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n";
        for (progress, phase) in [
            ("phase = \"lifecycle\"\npid = 1\n", Phase::Lifecycle("lifecycle".into())),
            ("phase = \"failed\"\npid = 1\n", Phase::Failed { at: "failed".into() }),
            ("phase = \"teleporting\"\npid = 1\n", Phase::Lifecycle("teleporting".into())),
        ] {
            let text = format!("{base}[instance.a.progress]\n{progress}");
            let state: State = toml::from_str(&text).unwrap();
            assert_eq!(state.instances["a"].progress.as_ref().unwrap().phase, phase, "{text}");
        }
    }

    #[test]
    fn view_in_flight_when_alive_and_same_boot() {
        let alive = |_| true;
        assert_eq!(progress(Phase::Preparing).view(alive, Some("boot-a")), PhaseView::InFlight("preparing".into()));
        assert_eq!(progress(Phase::Building).view(alive, Some("boot-a")), PhaseView::InFlight("building".into()));
        assert_eq!(progress(Phase::Creating).view(alive, Some("boot-a")), PhaseView::InFlight("creating".into()));
        assert_eq!(
            progress(Phase::Lifecycle("postCreateCommand".into())).view(alive, Some("boot-a")),
            PhaseView::InFlight("postCreateCommand".into())
        );
        // An unknown boot on either side can't prove a reboot.
        assert_eq!(progress(Phase::Building).view(alive, None), PhaseView::InFlight("building".into()));
        let mut unbooted = progress(Phase::Building);
        unbooted.boot = None;
        assert_eq!(unbooted.view(alive, Some("boot-b")), PhaseView::InFlight("building".into()));
    }

    #[test]
    fn view_interrupted_when_dead_or_rebooted() {
        let interrupted = |at: &str| PhaseView::Failed { at: at.into(), error: "interrupted".into() };
        let p = progress(Phase::Lifecycle("shell-rc".into()));
        assert_eq!(p.view(|pid| pid != 4242, Some("boot-a")), interrupted("shell-rc"));
        assert_eq!(p.view(|_| true, Some("boot-b")), interrupted("shell-rc"));
        assert_eq!(progress(Phase::Creating).view(|_| false, None), interrupted("creating"));
    }

    #[test]
    fn view_failed_stays_failed() {
        let mut p = progress(Phase::Failed { at: "postCreateCommand".into() });
        p.error = Some("exit status 1".into());
        let failed = PhaseView::Failed { at: "postCreateCommand".into(), error: "exit status 1".into() };
        // Liveness and boot don't matter once it's recorded failed.
        assert_eq!(p.view(|_| true, Some("boot-a")), failed);
        assert_eq!(p.view(|_| false, Some("boot-b")), failed);
        p.error = None;
        assert_eq!(
            p.view(|_| true, Some("boot-a")),
            PhaseView::Failed { at: "postCreateCommand".into(), error: "failed".into() }
        );
    }

    #[test]
    fn pid_alive_probes_real_processes() {
        assert!(pid_alive(std::process::id()));
        #[cfg(unix)]
        {
            // Above any pid_max, and not representable as a pid_t at all.
            assert!(!pid_alive(i32::MAX as u32));
            assert!(!pid_alive(u32::MAX));
            assert!(!pid_alive(0));
            // pid 1 exists; as non-root that's EPERM, still alive.
            assert!(pid_alive(1));
        }
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
