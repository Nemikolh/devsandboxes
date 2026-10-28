use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{
    sandbox_build_hash, short_hash, substitute, Config, ContainerEnv, MountContext, ResolvedSandbox, CONFIG_FILE,
};
use crate::runtime::{backend, NAME_PREFIX};
use crate::state::{Instance, Phase, State};

use super::{container_drifted, pick, services};

pub(crate) mod cancel;
mod editor;
mod facts;
mod folders;
mod git;
mod identity;
mod image;
mod lifecycle;
mod mounts;
mod progress;
mod ssh_agent;
pub(crate) mod tool_env;
mod worktree;

pub(crate) use editor::write_vscode_name_config;
pub(crate) use git::{check_repo, host_git, host_git_bare, host_git_new};
pub(crate) use lifecycle::exec_lifecycle;
pub(crate) use ssh_agent::{
    ssh_agent_dir, ssh_agent_link_path, ssh_agent_refresh, SSH_AGENT_TARGET,
};
use image::{feature_container_opts, image_for, privileged_decision};
use lifecycle::run_host_commands;
use mounts::{
    merge_volumes, provision_shell_history, resolve_caches, resolve_folders, resolve_mounts,
    resolve_shell_rc, shell_rc_wiring, sort_parents_first, write_workspace_file, ResolvedMounts,
};
pub(crate) use progress::{OnFailure, Tracker};
pub(crate) use tool_env::BrowserSandbox;
use ssh_agent::ssh_agent_forward;
use worktree::{copy_worktree_includes, create_worktree, git_companion_mount, link_shared_files, lock_worktree};
pub(crate) use worktree::{instance_at, lock_instance_worktrees, parse_worktree_list, unlock_worktree};

/// Branch pattern for worktree instances when neither `--branch` nor the
/// sandbox's `worktree-branch` is set. Also the TUI prompt's completion base
/// after `--branch`.
pub const DEFAULT_WORKTREE_BRANCH: &str = "sandbox/${instance}";

/// Per-instance additions from the hidden `run` flags, which a dispatcher's
/// control requests use (`commands::dispatch`), plus `--reprobe`. Not part
/// of the config, so not hashed.
#[derive(Debug, Clone, Default)]
pub struct RunExtras {
    /// `run`/`rebuild --reprobe`: probe the runtime's identity mode afresh
    /// (`identity::probe`) instead of trusting the cache. Not recorded.
    pub reprobe: bool,
    /// Extra `-e K=V` for this instance only, after `containerEnv` (so they
    /// win on a duplicate key). Recorded in state (`Instance::extra_env`) so a
    /// `rebuild`, which passes none, keeps them.
    pub env: Vec<(String, String)>,
    /// `instance_id` of the owning dispatcher.
    pub dispatcher: Option<String>,
}

/// Create and start an instance; returns its name (state key), or `None` when
/// there was nothing to run and an example config was written instead.
/// Printing is the caller's: plain `run` prints the name, `run --json` a
/// [`RunRecord`], autostart and the dashboard their own lines.
pub fn run(
    dir: &Path,
    sandbox_name: Option<String>,
    instance_name: Option<String>,
    branch_override: Option<String>,
    base_override: Option<String>,
    extras: RunExtras,
) -> Result<Option<String>> {
    // The dispatcher boundary (`commands::dispatch`) checks this too; `run`'s
    // hidden flags are callable directly, so check again before any work.
    if extras.dispatcher.is_some()
        && branch_override.as_deref().is_some_and(|b| !crate::devsbd::control::valid_branch(b))
    {
        bail!("bad --branch for a dispatcher child: use {}", crate::devsbd::control::BRANCH_RULES);
    }
    if extras.dispatcher.is_some() {
        if let Some((k, _)) = extras.env.iter().find(|(k, _)| crate::devsbd::control::denied_env(k)) {
            bail!("--env `{k}` may not be set by a dispatcher");
        }
    }
    let config = match Config::load(dir) {
        Ok(config) if !config.sandboxes.is_empty() => config,
        _ => return offer_example_config(dir).map(|()| None),
    };

    let sandbox_name = match sandbox_name {
        Some(name) => name,
        None => {
            let names: Vec<&str> = config.sandboxes.keys().map(String::as_str).collect();
            if !std::io::stdin().is_terminal() {
                bail!("no sandbox specified; available: {}", names.join(", "));
            }
            names[pick("Select a sandbox", &names)?].to_string()
        }
    };
    let sandbox = config.resolve_sandbox(&sandbox_name)?;
    let props = &sandbox.properties;
    // Under a fleet root: refused before anything is done, on the host or
    // the runtime (`materialize` checks again for `rebuild`).
    crate::worker_policy::enforce(dir, |p| p.check_config(dir, &config, &sandbox))?;

    let ignored = props.ignored();
    if !ignored.is_empty() {
        eprintln!("warning: ignoring unsupported properties: {}", ignored.join(", "));
    }

    let folder = sandbox
        .folder()
        .with_context(|| format!("sandbox `{sandbox_name}` has no `folder`"))?;
    let folder = dir
        .join(folder)
        .canonicalize()
        .with_context(|| format!("sandbox folder `{folder}` does not exist"))?;
    // Before its worktree: host git writes into the base repo.
    crate::worker_policy::enforce(dir, |p| p.check_path(&folder, "`folder`"))?;
    let basename = folder
        .file_name()
        .context("sandbox folder has no basename")?
        .to_string_lossy()
        .into_owned();
    // The mount context is rebuilt inside `materialize` for the actual mounts;
    // here it only substitutes `${instance}` into the worktree branch pattern,
    // which is why instance naming (and id allocation) comes first.
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let config_dir_str = config_dir.to_string_lossy().into_owned();
    let folder_str = folder.to_string_lossy().into_owned();
    let shared_volumes = config_dir.join("shared-volumes").to_string_lossy().into_owned();

    let mut state = State::load()?;
    // Deterministic name: explicit --name, or the sandbox name when free, else
    // the first free ordinal. `run` always creates a fresh instance; `start`
    // restarts a stopped one.
    let instance = match instance_name {
        Some(name) => name,
        None => default_instance_name(&state, &sandbox_name),
    };

    // `run` always creates a fresh instance; it never restarts a stopped one
    // (that is `devsandbox start`).
    if state.instances.contains_key(&instance) {
        bail!(
            "instance `{instance}` already exists; `devsandbox start {instance}` restarts it, \
             `devsandbox rm {instance}` frees the name"
        );
    }
    // Persistent id (see `Instance::instance_id`): the name, unless a rename
    // left that id taken. Everything host/runtime-addressed — container name,
    // `${instance}` substitutions, worktree, services — keys off the id so a
    // later rename moves nothing.
    let instance_id = unique_instance_id(&state, &instance);
    let container_name = format!("{NAME_PREFIX}{instance_id}");
    if backend().is_running(&container_name)?.is_some() {
        let labeled = backend().label(&container_name, DISPATCHER_LABEL)?;
        let leftovers: Vec<PathBuf> = [
            config_dir.join(".worktrees").join(&instance_id),
            config_dir.join(".worktrees").join(format!("{instance_id}.folders")),
        ]
        .into_iter()
        .filter(|p| p.exists())
        .collect();
        orphan_check(&container_name, extras.dispatcher.as_deref(), labeled.as_deref(), &leftovers)?;
        eprintln!(
            "warning: removing orphaned container `{container_name}`: a child of this dispatcher \
             no longer in state, with no worktree left"
        );
        let project = services::project_id(&config_dir)?;
        super::rm::remove_container(&container_name, &project, &instance_id);
    }

    let var_ctx = MountContext {
        config_dir: &config_dir_str,
        workspace_folder: &folder_str,
        workspace_folder_basename: &basename,
        shared_volumes: &shared_volumes,
        instance: &instance_id,
        container_env: None,
    };

    // A dispatcher's child always gets a worktree, even as the folder's first
    // instance: an unattended agent must never work in the user's checkout.
    let use_worktree = base_in_use(&state, &folder) || extras.dispatcher.is_some();
    // Keyed by id: a renamed instance keeps its worktree dir, so a new
    // instance reusing the freed name must not collide with it.
    let planned_worktree = config_dir.join(".worktrees").join(&instance_id);

    // Record first (docs/architecture/instances/lifecycle.md): whatever is
    // made from here on is on the record, so a failure can roll it back and an
    // interrupted run leaves something `rm` finds. `state` stays the snapshot
    // *without* it: `materialize` reads its absence as "not a rebuild" (no
    // prior entry to keep folders, ports or env from), and the
    // worktree/`folders` decisions below must see the other instances only,
    // as before.
    let project = services::project_id(dir)?;
    let provisional = Instance {
        sandbox: sandbox_name.clone(),
        instance_id: instance_id.clone(),
        project,
        container: container_name.clone(),
        folder: if use_worktree { planned_worktree.clone() } else { folder.clone() },
        base_folder: folder.clone(),
        worktree: None,
        branch: None,
        branch_created: true,
        folders: Vec::new(),
        shell_history: None,
        // Not known before `materialize`; nothing reads it until the final save.
        workspace: String::new(),
        workspace_file: None,
        remote_env: Default::default(),
        remote_user: None,
        home: None,
        browser_sandbox: None,
        tool_env: Default::default(),
        ssh_auth_sock: None,
        devsbd_arch: None,
        volumes: Vec::new(),
        dispatcher: extras.dispatcher.clone(),
        config_dir: Some(config_dir.clone()),
        extra_env: effective_extra_env(&extras.env, None),
        forwarded_ports: Default::default(),
        done: None,
        progress: None,
        created_unix: Instance::now(),
    };
    let mut tracker = Tracker::new(&instance_id, Phase::Preparing);
    let provisional = Instance { progress: Some(tracker.progress()), ..provisional };
    // From the record on, Ctrl-C/SIGTERM cancels: roll back, then exit.
    let _cancel = cancel::catch();
    // Re-checked under the lock: the snapshot may be stale, and two `run`s
    // picking one default name must not both get it.
    State::update(|fresh| {
        claim_name(fresh, &instance, &instance_id)?;
        fresh.instances.insert(instance.clone(), provisional);
        Ok(())
    })?;
    // From here everything printed (children's output included) also goes
    // to the instance's build log, the rollback's included.
    let log = crate::build_log::open(&instance_id);

    let mut branch_start = None;
    let result = (|| {
        let (source, worktree, branch, branch_created) =
            prepare_worktree(&folder, use_worktree, &planned_worktree, branch_override, base_override, &extras, props, &var_ctx, &state)?;
        if branch_created && worktree.is_some() {
            branch_start = worktree.as_deref().and_then(|wt| progress::commit_of(wt, "HEAD"));
        }
        tracker.record(|i| {
            i.folder = source.clone();
            i.worktree = worktree.clone();
            i.branch = branch.clone();
            i.branch_created = branch_created;
        })?;
        materialize(
            dir,
            &config,
            &sandbox_name,
            &sandbox,
            &instance,
            &instance_id,
            &source,
            &folder,
            worktree,
            branch,
            branch_created,
            &extras,
            &mut state,
            &mut tracker,
        )
    })();
    let result = result.map_err(|e| {
        let outcome = tracker.on_failure(true);
        match outcome {
            OnFailure::RollBack => progress::rollback(&instance_id, branch_start.as_deref()),
            OnFailure::KeepFailed => tracker.fail(&e),
        }
        // Not the step's own error: a child killed by the same Ctrl-C
        // reports some failure of its own.
        if cancel::requested() {
            return anyhow::anyhow!("`{instance}` {}: rolled back", cancel::CANCELLED);
        }
        match outcome {
            OnFailure::RollBack => e,
            OnFailure::KeepFailed => anyhow::anyhow!("{e:#}; {}", kept_hint(&instance)),
        }
    });
    log.outcome(result).map(|()| Some(instance))
}

/// `run`'s name check, against the current state: the name must be free (a
/// stopped instance keeps it), and so must the id allocated for it.
fn claim_name(state: &State, instance: &str, instance_id: &str) -> Result<()> {
    if state.instances.contains_key(instance) {
        bail!(
            "instance `{instance}` already exists; `devsandbox start {instance}` restarts it, \
             `devsandbox rm {instance}` frees the name"
        );
    }
    if state.instances.values().any(|i| i.instance_id == instance_id) {
        bail!("instance id `{instance_id}` was just taken by another `run`; retry");
    }
    Ok(())
}

/// The working tree a new instance mounts: a fresh worktree at `planned` when
/// `use_worktree`, else the base `folder` itself. Returns `(source, worktree,
/// branch, branch_created)`.
#[allow(clippy::too_many_arguments)]
fn prepare_worktree(
    folder: &Path,
    use_worktree: bool,
    planned: &Path,
    branch_override: Option<String>,
    base_override: Option<String>,
    extras: &RunExtras,
    props: &crate::config::SandboxProperties,
    var_ctx: &MountContext,
    state: &State,
) -> Result<(PathBuf, Option<PathBuf>, Option<String>, bool)> {
    Ok(if use_worktree {
        // Only a named branch can already exist on the remote; generated
        // pattern names are new by construction (see `create_worktree`).
        let explicit_branch = branch_override.is_some();
        let branch = worktree_branch(
            branch_override,
            extras.dispatcher.is_some(),
            props.worktree_branch.as_deref(),
            var_ctx,
        );
        // Must be absolute: `create_worktree` runs `git -C <base>`, so a
        // `dir`-relative path would resolve under the base repo instead of here,
        // and the mount/state would point at a different (empty) directory.
        let wt = planned.to_path_buf();
        // Start point: `--base`, else the sandbox's `worktree-base`, else
        // detected from the remote (see `worktree_start_point`).
        let base_ref = base_override.or_else(|| props.worktree_base.clone());
        let created = create_worktree(folder, &wt, &branch, base_ref.as_deref(), explicit_branch, state)?;
        (wt.clone(), Some(wt), Some(branch), created)
    } else {
        (folder.to_path_buf(), None, None, true)
    })
}

/// Whether `run` may replace `container`, which exists but has no state
/// entry. Only for a dispatcher's run, when the container carries that
/// dispatcher's label (`labeled`): its children have deterministic names, so
/// without this an orphaned child blocks its key's `ensure` for good, and
/// the dispatcher can't reach the host to clear it. And only with no
/// worktree left (`leftovers`), so nothing `rm`'s dirty-worktree check
/// protects is lost; the work of a devcontainer lives in its worktree, not
/// in the container. Everything else is refused, with what to do on the host.
fn orphan_check(
    container: &str,
    dispatcher: Option<&str>,
    labeled: Option<&str>,
    leftovers: &[PathBuf],
) -> Result<()> {
    let Some(dispatcher) = dispatcher.filter(|d| labeled == Some(*d)) else {
        bail!(
            "container `{container}` already exists but is not in state; \
             remove it or pick another --name"
        );
    };
    if let Some(path) = leftovers.first() {
        bail!(
            "container `{container}` (a child of dispatcher `{dispatcher}`) already exists but is \
             not in state, and its worktree `{}` is still on disk; check it for work, then remove \
             both on the host (`git worktree remove`, then the container)",
            path.display()
        );
    }
    Ok(())
}

/// `run --json` payload: where the new instance lives, so a programmatic
/// caller can exec into it or open its folder without a follow-up `status`.
/// Field names and meanings match [`crate::snapshot::InstanceRow`] where they
/// overlap, except `worktree`, which is the path here (not a flag): a caller
/// that just created the instance needs to know where it is. Runtime-derived
/// fields (status, cpu, drift) are left to `status --json`.
#[derive(Debug, serde::Serialize)]
pub struct RunRecord {
    /// Instance name (state key); what every `<name>` argument accepts.
    pub name: String,
    /// Persistent id (`Instance::instance_id`).
    pub instance_id: String,
    pub sandbox: String,
    pub container: String,
    /// Workspace folder inside the container.
    pub workspace: String,
    /// Host folder mounted as the workspace (the worktree for a worktree instance).
    pub folder: String,
    /// Host base repo folder the instance derives from.
    pub base_folder: String,
    /// Host worktree path, `None` when the instance runs on `base_folder`.
    pub worktree: Option<String>,
    /// Worktree branch, `None` without a worktree.
    pub branch: Option<String>,
}

impl RunRecord {
    pub fn new(name: &str, info: &Instance) -> Self {
        let path = |p: &Path| p.display().to_string();
        RunRecord {
            name: name.to_string(),
            instance_id: info.instance_id.clone(),
            sandbox: info.sandbox.clone(),
            container: info.container.clone(),
            workspace: info.workspace.clone(),
            folder: path(&info.folder),
            base_folder: path(&info.base_folder),
            worktree: info.worktree.as_deref().map(path),
            branch: info.branch.clone(),
        }
    }
}

/// The enveloped `run --json` document for instance `key`, read back from
/// state (what `run` just recorded).
pub fn run_record_json(key: &str) -> Result<String> {
    let state = State::load()?;
    let info = state
        .instances
        .get(key)
        .with_context(|| format!("instance `{key}` missing from state after run"))?;
    serde_json::to_string_pretty(&super::status::Envelope::new(RunRecord::new(key, info)))
        .context("serialize run record")
}

/// Branch for a new worktree: the `--branch` override, else the sandbox's
/// `worktree-branch`, else the default. Patterns get `${instance}` (and the
/// other mount variables) substituted so each instance gets a unique branch.
/// A dispatcher's override is used verbatim: it comes from a container, and
/// substitution would expand `${localEnv:…}` from the host's environment.
fn worktree_branch(
    branch_override: Option<String>,
    dispatched: bool,
    pattern: Option<&str>,
    ctx: &MountContext,
) -> String {
    match branch_override {
        Some(branch) if dispatched => branch,
        Some(pattern) => substitute(&pattern, ctx),
        None => substitute(pattern.unwrap_or(DEFAULT_WORKTREE_BRANCH), ctx),
    }
}

/// The `run --env` set an instance gets and records: the given one (last
/// value wins on a duplicate key, as with `-e`), or, when none is given (a
/// `rebuild`), the one recorded on the entry being replaced. Recorded values
/// passed the `run` boundary's checks when first given, so they aren't
/// re-checked.
fn effective_extra_env(
    given: &[(String, String)],
    prior: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    if given.is_empty() {
        prior.cloned().unwrap_or_default()
    } else {
        given.iter().cloned().collect()
    }
}

/// Create (and start) the instance container and record it: run
/// `initializeCommand`, bring up services, build the mount set (rebuilding the
/// `${instance}` context from `instance_id` so per-instance mounts/history
/// resolve to the same host paths across renames), start the container, upsert
/// state (saved before the lifecycle chain so a crash mid-lifecycle leaves the
/// container tracked), then run the lifecycle commands. `instance` is the
/// display name (state key); `instance_id` the persistent identity every
/// container/service/mount name derives from. `source` is what gets mounted as
/// the working tree (a worktree when `worktree.is_some()`, else `folder`);
/// `folder` is the canonicalized base folder, used for `base_folder` and the
/// git companion mount. A worktree gets the `.worktreeinclude` copies on every
/// call, `rebuild` included, so adding a pattern and rebuilding (the fix the
/// resulting drift points at) actually delivers the file; the copy never
/// overwrites, so edits in the worktree survive. `branch_created` is recorded for `rm` (see `Instance::branch_created`).
/// `extras.dispatcher` falls back to the entry being replaced, as do the
/// recorded `config_dir` and extra env (`effective_extra_env`), so a
/// `rebuild` keeps all three.
///
/// `tracker` records each phase on the instance's record as it's entered, so
/// on an error the caller reads where it stopped (and whether the container
/// exists yet) off it. The record saved after the container is created stays
/// in flight; only a lifecycle chain that ran through clears `progress`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize(
    dir: &Path,
    config: &Config,
    sandbox_name: &str,
    sandbox: &ResolvedSandbox,
    instance: &str,
    instance_id: &str,
    source: &Path,
    folder: &Path,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    branch_created: bool,
    extras: &RunExtras,
    state: &mut State,
    tracker: &mut Tracker,
) -> Result<()> {
    let props = &sandbox.properties;
    // The worker policy's config checks (`run` made them too; `rebuild`
    // comes straight here), then `folder`, before anything touches it.
    crate::worker_policy::enforce(dir, |p| {
        p.check_config(dir, config, sandbox)?;
        p.check_path(folder, "`folder`")
    })?;
    // Not the prompt (that's the verb client's, on a terminal): this line is
    // what every other path gets (off a TTY, `--json`, the dispatcher's
    // subprocesses, autostart), and a skipped prompt too. stderr only.
    let acks = services::project_id(dir).ok().and_then(|p| state.identity_acks.get(&p));
    if let Some(line) = crate::config_identity::warning(&crate::config_identity::identity_findings(config), sandbox_name, acks) {
        eprintln!("warning: {line}");
    }
    let prior = state.instances.get(instance);
    let dispatcher = extras
        .dispatcher
        .clone()
        .or_else(|| prior.and_then(|i| i.dispatcher.clone()));
    let prior_config_dir = prior.and_then(|i| i.config_dir.clone());
    // A rebuild keeps the instance's host ports
    // (docs/architecture/daemon/forwards.md).
    let forwarded_ports = prior.map(|i| i.forwarded_ports.clone()).unwrap_or_default();
    // A rebuild keeps the done flag: recreating the container doesn't mean
    // the user is back on it (only `undone`/`ensure` clear it).
    let done = prior.and_then(|i| i.done);
    // A rebuild keeps its `folders` worktrees (see `folders::mount_folders`).
    let prior_folders = prior.map(|i| i.folders.clone()).unwrap_or_default();
    let run_env = effective_extra_env(&extras.env, prior.map(|i| &i.extra_env));
    let container_name = format!("{NAME_PREFIX}{instance_id}");
    let basename = folder
        .file_name()
        .context("sandbox folder has no basename")?
        .to_string_lossy()
        .into_owned();
    // `${configDir}` / `${localWorkspaceFolder(Basename)}` / `${sharedVolumes}`
    // / `${instance}` are usable in `workspaceFolder`, `mounts`, and cache
    // sources; rebuild the context from the persistent id so
    // `${instance}`-anchored mounts/history resolve to the same host paths
    // across rebuilds *and* renames.
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let config_dir_str = config_dir.to_string_lossy().into_owned();
    let folder_str = folder.to_string_lossy().into_owned();
    let shared_volumes = config_dir.join("shared-volumes").to_string_lossy().into_owned();
    let var_ctx = MountContext {
        config_dir: &config_dir_str,
        workspace_folder: &folder_str,
        workspace_folder_basename: &basename,
        shared_volumes: &shared_volumes,
        instance: instance_id,
        container_env: None,
    };
    let workspace = substitute(
        &props
            .workspace_folder
            .clone()
            .unwrap_or_else(|| format!("/workspaces/{basename}")),
        &var_ctx,
    );
    // Extra workspace roots (`folders`): bind-mounted at their container path
    // and listed in the generated `.code-workspace` after the primary folder.
    let extra_folders = resolve_folders(dir, &var_ctx, &workspace, props)?;
    crate::worker_policy::enforce(dir, |p| {
        extra_folders.iter().try_for_each(|f| p.check_path(&f.host, &format!("`folders` entry `{}`", f.target)))
    })?;

    // initializeCommand runs on the host, before anything is created.
    if let Some(cmd) = &props.initialize_command {
        run_host_commands(dir, cmd).context("initializeCommand failed")?;
    }

    // Shared files (`worktree-link`), then per-worktree copies: after
    // initializeCommand so files it generates get adopted, before the
    // container so lifecycle commands see them. Links go first because the
    // copy never overwrites, so a path in both lists ends up linked.
    let links = props.worktree_link.clone().unwrap_or_default();
    let link_store = if links.is_empty() {
        None
    } else {
        let store = link_store(&config_dir, folder, &basename);
        let mut trees = vec![folder];
        if worktree.is_some() {
            trees.push(source);
        }
        link_shared_files(&store, &trees, &links)?;
        Some(store)
    };
    if worktree.is_some() {
        let patterns = props.worktree_include.clone().unwrap_or_default();
        copy_worktree_includes(folder, source, &patterns);
    }

    // `folders` worktrees: after initializeCommand (it may create the repos),
    // before the container, like the primary's links above.
    let folder_mounts = folders::mount_folders(
        config,
        dir,
        &config_dir,
        state,
        instance,
        instance_id,
        dispatcher.is_some(),
        &prior_folders,
        &extra_folders,
    )?;
    let recorded_folders = folders::merge_folder_mounts(&prior_folders, folder_mounts.clone());
    tracker.record(|i| i.folders = recorded_folders.clone())?;
    // Before the container exists, so nothing in it can prune them; a rebuild
    // of an instance from before locking gets locked here.
    if let Some(wt) = &worktree {
        lock_worktree(folder, wt);
    }
    for fm in &folder_mounts {
        if let Some(wt) = &fm.worktree {
            lock_worktree(&fm.base, wt);
        }
    }

    // Bring up the sandbox's services (global shared + this instance's isolated)
    // and their networks; the instance container joins them to reach services by
    // name.
    let project = services::project_id(dir)?;
    let service_names = props.services.clone().unwrap_or_default();
    let (networks, endpoints) =
        services::ensure_services(config, dir, &project, instance_id, &service_names)?;

    // The worktree's `.git` file points at `<base>/.git/worktrees/..` by absolute
    // host path; mount the base `.git` at the identical path so git works inside
    // the container.
    let mut extra_mounts = match &worktree {
        Some(_) => vec![git_companion_mount(folder)],
        None => Vec::new(),
    };
    // The links are absolute symlinks into the store; mounting the store dir at
    // the same path makes them resolve in the container. A dir mount, so it
    // works on Apple `container` too.
    if let Some(store) = &link_store {
        extra_mounts.push(format!("{}:{}", store.display(), store.display()));
    }
    // Each `folders` worktree needs its base's `.git` too; once per base (the
    // primary may already mount the same one).
    for fm in folder_mounts.iter().filter(|fm| fm.worktree.is_some()) {
        let companion = git_companion_mount(&fm.base);
        if !extra_mounts.contains(&companion) {
            extra_mounts.push(companion);
        }
    }
    // ssh-agent: relay-first. On unix with an embedded helper we never mount
    // the socket — the in-container `devsbd` daemon serves it over exec stdio
    // (docs/architecture/helper/ssh-agent.md), which also fixes
    // rotation-while-running and works on runtimes that can't bind files.
    // Only builds without a helper (`cargo install`) and non-unix hosts fall
    // back to the bind mount.
    let relay_first = cfg!(unix) && crate::devsbd::embedded();
    let ssh_auth_sock = if relay_first {
        None
    } else {
        match ssh_agent_forward(instance_id)? {
            Some((mount, target)) => {
                extra_mounts.push(mount);
                Some(target)
            }
            None => None,
        }
    };
    // The image before the mounts: `${containerEnv:…}` (mounts, containerEnv,
    // remoteEnv) reads the built image's env and passwd.
    tracker.enter(Phase::Building)?;
    let image = image_for(dir, &project, sandbox)?;
    let facts = facts::image_facts(&image);
    let container_env =
        ContainerEnv::resolve(props.container_env.as_ref(), facts.env.clone(), facts::home(props, &facts), &var_ctx);
    let var_ctx = MountContext { container_env: Some(&container_env), ..var_ctx };
    let ResolvedMounts { args: mut mounts, volumes: mut instance_volumes } =
        resolve_mounts(dir, folder, &basename, instance_id, sandbox, Some(&container_env))?;
    for fm in &folder_mounts {
        mounts.push(format!(
            "type=bind,source={},target={}",
            fm.worktree.as_deref().unwrap_or(&fm.base).display(),
            fm.target
        ));
    }
    // Package-manager caches: shared bind mounts + the env vars pointing at them.
    let (cache_mounts, cache_env) = resolve_caches(&config_dir, props)?;
    mounts.extend(cache_mounts);
    // Managed per-instance shell history: mount the host dir, point zsh at it
    // (env var for rc-less shells, rc export below for shells whose rc chain
    // clobbers the env, e.g. VS Code's injected shell integration).
    let mut extra_env = cache_env;
    let shell_history = if props.persist_shell_history == Some(true) {
        let (path, mount, env) = provision_shell_history(dir, instance_id)?;
        mounts.push(mount);
        extra_env.push(env);
        Some(path)
    } else {
        None
    };
    extra_env.extend(run_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    // `shell-rc`: host snippets mounted read-only, sourced by the rc files.
    let shell_rc = resolve_shell_rc(dir, &var_ctx, props)?;
    mounts.extend(shell_rc.iter().map(|(mount, _)| mount.clone()));
    // Feature-declared mounts come last: any target claimed above (or the
    // workspace itself) wins, so the config can override a feature's mount.
    let feature = feature_container_opts(props, &var_ctx, &mounts, &workspace)?;
    mounts.extend(feature.mounts);
    instance_volumes.extend(feature.volumes);
    // Keep volumes a previous run recorded: a rebuild keeps them on disk even
    // when the config no longer mounts them, and `rm` must still find them.
    let volumes = merge_volumes(
        state.instances.get(instance).map(|i| i.volumes.as_slice()).unwrap_or_default(),
        instance_volumes,
    );
    tracker.record(|i| i.volumes = volumes.clone())?;
    sort_parents_first(&mut mounts);
    let (privileged, warnings) = privileged_decision(
        props.privileged == Some(true),
        &feature.privileged_by,
        backend().supports_privileged(),
    );
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    let (restart, warnings) = crate::commands::autostart::restart_decision(
        props.autostart,
        backend().supports_restart_policy(),
        backend().name(),
    );
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    // Who the container runs as (docs/architecture/instances/identity.md).
    let (plan, seccomp) = identity::plan_for(&image, facts::accounts(props, &facts), props, extras.reprobe, privileged);
    let root_plan = tool_env::root_plan(plan.exec_user.as_deref());
    tracker.enter(Phase::Creating)?;
    run_container(
        dir,
        sandbox,
        image,
        &container_name,
        source,
        folder,
        &workspace,
        &extra_mounts,
        &mounts,
        &container_env.sandbox,
        &extra_env,
        &networks,
        privileged,
        restart,
        dispatcher.as_deref(),
        &plan,
        seccomp.as_deref(),
    )?;
    let container = container_name.clone();
    backend().connect_networks(&container, &networks)?;
    backend().wire_service_dns(&container, &endpoints)?;
    // Before lifecycle commands, so they could already rely on the helper. In
    // relay-first mode a failure means no ssh-agent forwarding at all (no mount
    // fallback), so word the note that way; otherwise the plain install note.
    let devsbd_arch = match crate::devsbd::ensure_or_reason(&container, None) {
        Ok(arch) => Some(arch),
        Err(reason) => {
            if relay_first {
                eprintln!("note: ssh-agent forwarding off in `{container}`: {reason}");
            } else {
                eprintln!("note: {reason}");
            }
            None
        }
    };
    // Fit the adopted account to the plan, before anything runs as it. No
    // helper means no remap: the container runs on, unfitted.
    let mut home = None;
    let volume_targets = identity::volume_targets(&mounts);
    let socket_gid = identity::socket_gid(plan.mode, &mounts);
    if let Some(argv) = identity::apply_argv(&container, &plan, socket_gid, &volume_targets) {
        if devsbd_arch.is_some() {
            let account_home = identity::apply(&argv)?;
            if plan.root_home {
                home = Some(account_home);
            }
        } else {
            eprintln!(
                "warning: the devsbd helper isn't installed in `{container}`, so `{}` couldn't be fitted to the identity plan",
                plan.account.as_deref().unwrap_or_default()
            );
        }
    }
    // Whether the browser sandbox can work here, checked as the exec user in
    // the real container (its seccomp, user, AppArmor); the knobs follow.
    let browser_sandbox = devsbd_arch
        .is_some()
        .then(|| tool_env::check(&tool_env::check_argv(&container, plan.exec_user.as_deref())))
        .flatten();
    let user_keys = props
        .container_env
        .iter()
        .chain(props.remote_env.iter())
        .flat_map(|env| env.keys().map(String::as_str))
        .collect();
    // A root plan's tool env puts the browser wrapper's dir before the
    // image's PATH (`exec -e PATH` replaces it); unreadable reads as unset.
    let tool_env =
        tool_env::tool_env(root_plan, browser_sandbox.as_ref(), tool_env::image_path(&facts.env), &user_keys);
    eprintln!("{}", tool_env::summary(root_plan, browser_sandbox.as_ref()));
    let workspace_file = write_workspace_file(
        &container,
        instance,
        &workspace,
        &extra_folders,
        props.vscode_extensions().unwrap_or(&[]),
    );

    let persist_history = shell_history.is_some();
    let record = Instance {
        sandbox: sandbox_name.to_string(),
        instance_id: instance_id.to_string(),
        project,
        container: container.clone(),
        folder: source.to_path_buf(),
        base_folder: folder.to_path_buf(),
        worktree,
        branch,
        branch_created,
        folders: recorded_folders,
        shell_history,
        workspace: workspace.clone(),
        workspace_file,
        remote_env: props.remote_env.iter().flatten().map(|(k, v)| (k.clone(), substitute(v, &var_ctx))).collect(),
        remote_user: plan.exec_user.clone(),
        home,
        browser_sandbox,
        tool_env,
        ssh_auth_sock,
        devsbd_arch,
        volumes,
        dispatcher,
        config_dir: Some(prior_config_dir.unwrap_or(config_dir)),
        extra_env: run_env,
        forwarded_ports,
        done,
        // Still in flight: the lifecycle chain runs after this save.
        progress: Some(tracker.progress()),
        created_unix: Instance::now(),
    };
    // Into the current file, not `state` (loaded before the build): saving that
    // snapshot would drop instances other commands recorded meanwhile.
    State::update(|fresh| {
        fresh.instances.insert(instance.to_string(), record.clone());
        Ok(())
    })?;
    state.instances.insert(instance.to_string(), record);

    // Every exec below runs as the plan's user, recorded on the instance.
    let exec_user = plan.exec_user.as_deref();
    let extensions = props.vscode_extensions().unwrap_or(&[]);
    if !extensions.is_empty() || exec_user.is_some() {
        write_vscode_name_config(&container, Some(extensions), exec_user)?;
    }

    // ssh-agent for the lifecycle chain. The instance was just saved, so read
    // the agent rule off it (mount → always; relay → only with a host agent).
    // In relay mode the daemon's bridge (`bridges.ensure`; saved state is what
    // the daemon reads) is waited for before the chain starts (its commands
    // may clone over ssh at once) and spans shell-rc wiring + all five
    // commands, the connection held until this fn returns (`?` paths
    // included). Not ready: one note before the chain, which runs anyway.
    let inst = state.instances.get(instance).expect("instance just inserted");
    // Same env as `devsandbox exec`: remoteEnv, then the saved `--env`.
    let exec_env = inst.exec_env();
    let host_agent = crate::commands::exec::has_host_agent();
    let ssh_auth_sock = crate::commands::exec::ssh_auth_sock_env(inst, host_agent);
    // Record the postStartCommand for the boot hook (the host still runs it
    // below: the hook can't have, the helper wasn't installed yet).
    let boot = boot_spec_for(
        props.autostart,
        devsbd_arch.is_some(),
        props.post_start_command.as_ref(),
        &workspace,
        Some(&exec_env),
        exec_user,
        ssh_auth_sock,
    );
    crate::devsbd::sync_boot(&container, boot.as_ref(), false);
    // Nothing to run in the container: no bridge to wait for.
    let lifecycle = !shell_rc.is_empty()
        || persist_history
        || [
            &props.on_create_command,
            &props.update_content_command,
            &props.post_create_command,
            &props.post_start_command,
            &props.post_attach_command,
        ]
        .iter()
        .any(|c| c.is_some());
    #[cfg(unix)]
    let _relay = (lifecycle && crate::devsbd::relay_mode(inst) && host_agent)
        .then(|| crate::commands::exec::AgentRelay::ready(instance, &container));
    #[cfg(not(unix))]
    let _ = lifecycle;

    if !shell_rc.is_empty() || persist_history {
        tracker.enter(Phase::Lifecycle("shell-rc".into()))?;
        let paths: Vec<&str> = shell_rc.iter().map(|(_, path)| path.as_str()).collect();
        exec_lifecycle(
            &container,
            &workspace,
            Some(&exec_env),
            exec_user,
            ssh_auth_sock,
            &shell_rc_wiring(&paths, persist_history),
        )
        .context("shell-rc wiring failed")?;
    }

    for (name, cmd) in [
        ("onCreateCommand", &props.on_create_command),
        ("updateContentCommand", &props.update_content_command),
        ("postCreateCommand", &props.post_create_command),
        ("postStartCommand", &props.post_start_command),
        ("postAttachCommand", &props.post_attach_command),
    ] {
        if let Some(cmd) = cmd {
            tracker.enter(Phase::Lifecycle(name.into()))?;
            exec_lifecycle(
                &container,
                &workspace,
                Some(&exec_env),
                exec_user,
                ssh_auth_sock,
                cmd,
            )
            .with_context(|| format!("{name} failed"))?;
        }
    }

    tracker.finish()?;
    if let Some(i) = state.instances.get_mut(instance) {
        i.progress = None;
    }
    Ok(())
}

/// What a failure from the container on adds to its error: everything
/// stays, as `failed`. Added by the caller (`run`, `rebuild`) rather than
/// inside `materialize`, so the error recorded on the instance is just what
/// failed (readers add their own hint: `commands::not_ready`).
pub(crate) fn kept_hint(instance: &str) -> String {
    format!(
        "instance `{instance}` kept as failed: `devsandbox rebuild {instance}` retries, \
         `devsandbox rm {instance}` removes it"
    )
}

/// The boot file a container should carry: the `postStartCommand` when the
/// runtime restarts the container itself (`autostart = "runtime"` on a backend
/// with restart policies) and the helper that runs it is installed; `None`
/// (remove the file) otherwise. `env` is the instance's `Instance::exec_env`.
/// The file is rewritten on `run`/`start`/`rebuild` only (and on `start`
/// after the hook already fired, see `devsbd::sync_boot`), so a saved env an
/// `ensure` changed reaches the hook from the start after the next one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn boot_spec_for(
    autostart: Option<crate::config::Autostart>,
    helper: bool,
    post_start: Option<&crate::config::LifecycleCommand>,
    workspace: &str,
    env: Option<&std::collections::BTreeMap<String, String>>,
    remote_user: Option<&str>,
    ssh_auth_sock: Option<&str>,
) -> Option<crate::devsbd::bootfile::BootSpec> {
    let runtime = crate::commands::autostart::runtime_restarts(
        autostart,
        backend().supports_restart_policy(),
    );
    let cmd = post_start.filter(|_| runtime && helper)?;
    Some(crate::devsbd::boot_spec(cmd, workspace, env, remote_user, ssh_auth_sock))
}

/// The `worktree-link` store for base repo `folder` (canonical). One per repo,
/// not per sandbox: the base checkout holds a single link per path, so two
/// sandboxes on one folder must share the copy it points at. The basename
/// keeps it readable; the path hash keeps two same-named repos apart.
fn link_store(config_dir: &Path, folder: &Path, basename: &str) -> PathBuf {
    let hash = short_hash(&folder.to_string_lossy());
    config_dir.join("shared-files").join(format!("{basename}-{}", &hash[..8]))
}

/// Whether some instance (running or stopped — a stopped one can be started
/// anytime) already mounts `folder` directly, as its working tree or as a
/// `folders` entry without a worktree; the new instance then gets a git
/// worktree so two containers never share a working tree. Matched on the
/// mounted source, not `base_folder`: worktree instances carry the base's
/// `base_folder` but have their own working tree, so they must not keep the
/// base checkout reserved after its direct-mount instance is removed.
fn base_in_use(state: &State, folder: &Path) -> bool {
    folders::folder_directly_mounted(state, folder, None)
}

/// Default instance name: the sandbox name when it is free in state, else the
/// first free `<sandbox>-<n>` ordinal (n >= 2). Stopped instances keep their
/// name (`start` restarts them), so a taken name always means a new ordinal.
fn default_instance_name(state: &State, sandbox: &str) -> String {
    if !state.instances.contains_key(sandbox) {
        return sandbox.to_string();
    }
    first_free_ordinal(state, sandbox)
}

/// First free `<sandbox>-<n>` (n >= 2) not present in state.
fn first_free_ordinal(state: &State, sandbox: &str) -> String {
    (2..)
        .map(|n| format!("{sandbox}-{n}"))
        .find(|name| !state.instances.contains_key(name))
        .expect("infinite range yields a free name")
}

/// Persistent id for a new instance named `name`: `name` itself when no
/// existing instance holds that id, else `name-<i>` (first free i >= 1). Ids
/// survive renames, so a rename can free a *name* whose id is still taken —
/// the suffix keeps ids (and everything derived from them: container, mounts,
/// worktree, services) unique while the instance still displays as `name`.
fn unique_instance_id(state: &State, name: &str) -> String {
    let taken = |id: &str| state.instances.values().any(|i| i.instance_id == id);
    if !taken(name) {
        return name.to_string();
    }
    (1..)
        .map(|i| format!("{name}-{i}"))
        .find(|id| !taken(id))
        .expect("infinite range yields a free id")
}

/// Warn when the reused container's recorded config or build hash differs from
/// the freshly resolved one (the shared [`container_drifted`] rule, so a
/// dockerfile edit warns too). Containers without the labels (None) — skip.
pub(crate) fn warn_on_drift(dir: &Path, container: &str, sandbox: &ResolvedSandbox) -> Result<()> {
    let build = sandbox_build_hash(dir, &sandbox.properties);
    if container_drifted(container, &sandbox.config_hash, &build)? {
        // Suggest a copy-pasteable command: strip the container prefix to the
        // instance name, falling back to the container name if it is unprefixed.
        let instance = container.strip_prefix(NAME_PREFIX).unwrap_or(container);
        eprintln!(
            "warning: config for `{container}` changed since it was created; \
             run `devsandbox rebuild {instance}` to apply changes"
        );
    }
    Ok(())
}

/// `docker run` the instance container from `image` (built by the caller, so
/// the phase can tell building from creating). Network attachment beyond the
/// first and service DNS are the caller's too: they run once the container
/// exists, when a failure keeps it. `plan` decides the user, the user
/// namespace, the seccomp profile (`seccomp`, its path) and the binds'
/// SELinux labels (`identity::create_args`). `container_env` is the
/// sandbox's `containerEnv`, substituted (`ContainerEnv::resolve`).
#[allow(clippy::too_many_arguments)]
fn run_container(
    dir: &Path,
    sandbox: &ResolvedSandbox,
    image: String,
    container: &str,
    source: &Path,
    base_folder: &Path,
    workspace: &str,
    extra_mounts: &[String],
    mounts: &[String],
    container_env: &BTreeMap<String, String>,
    extra_env: &[(String, String)],
    networks: &[String],
    privileged: bool,
    restart: Option<&str>,
    dispatcher: Option<&str>,
    plan: &crate::runtime::identity::Plan,
    seccomp: Option<&Path>,
) -> Result<()> {
    let props = &sandbox.properties;
    let mount = format!("{}:{workspace}", source.display());
    let sandbox_label = format!("devsandbox.sandbox={}", sandbox.name);
    let instance = container.strip_prefix(NAME_PREFIX).unwrap_or(container);
    let instance_label = format!("devsandbox.instance={instance}");
    let hash_label = format!("devsandbox.config_hash={}", sandbox.config_hash);
    let build_hash_label = format!(
        "devsandbox.build_hash={}",
        sandbox_build_hash(dir, &sandbox.properties)
    );
    let base_label = format!("devsandbox.base_folder={}", base_folder.display());
    let boot_hook_label = format!("{BOOT_HOOK_LABEL}=1");
    let mut args: Vec<String> = [
        "run", "-d", "--name", container,
        "--label", &sandbox_label, "--label", &instance_label,
        "--label", &hash_label, "--label", &build_hash_label, "--label", &base_label,
        "--label", &boot_hook_label,
        "-w", workspace,
    ]
    .map(str::to_string)
    .into();
    if props.init == Some(true) {
        args.push("--init".into());
    }
    if privileged {
        args.push("--privileged".into());
    }
    if let Some(policy) = restart {
        args.push("--restart".into());
        args.push(policy.into());
    }
    if let Some(id) = dispatcher {
        args.push("--label".into());
        args.push(format!("{DISPATCHER_LABEL}={id}"));
    }
    let volumes: Vec<String> = std::iter::once(mount).chain(extra_mounts.iter().cloned()).collect();
    let (identity_args, warning) = identity::create_args(plan, backend().name() == "podman", seccomp, &volumes, mounts);
    if let Some(warning) = warning {
        eprintln!("warning: {warning}");
    }
    args.extend(identity_args);
    // Runtimes differ in how many networks a container can start on; the
    // backend attaches the rest after creation.
    args.extend(backend().network_run_args(networks));
    for (key, value) in container_env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    for (key, value) in extra_env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    let set = container_env.iter().chain(extra_env.iter().map(|(k, v)| (k, v)));
    for (key, value) in gc_keeps_worktrees_env(set) {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    args.push(image);
    args.extend(container_command());

    // The worker policy's authority: this argv is everything the runtime
    // gets (features' mounts and `privileged`, identity, caches).
    crate::worker_policy::enforce_argv(dir, &args)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    backend().run_checked(&arg_refs)?;
    Ok(())
}

/// `gc.worktreePruneExpire=never` for git in the container, via git's
/// `GIT_CONFIG_COUNT`/`_KEY_<n>`/`_VALUE_<n>` env. The container sees the
/// repo's worktree records but not the other checkouts, so a `git gc` there
/// would prune them as missing. [`worktree::lock_worktree`] guards explicit
/// prunes too, but only devsandbox's own worktrees; this covers the rest
/// against gc. Extends a count the given env (`-e` order, last wins) already
/// sets; an unparsable one is left alone, git rejects it anyway.
fn gc_keeps_worktrees_env<'a>(
    env: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Vec<(String, String)> {
    let count = env
        .into_iter()
        .filter(|(k, _)| *k == "GIT_CONFIG_COUNT")
        .last()
        .map(|(_, v)| v.trim().parse::<usize>());
    let n = match count {
        None => 0,
        Some(Ok(n)) => n,
        Some(Err(_)) => return Vec::new(),
    };
    vec![
        (format!("GIT_CONFIG_KEY_{n}"), "gc.worktreePruneExpire".into()),
        (format!("GIT_CONFIG_VALUE_{n}"), "never".into()),
        ("GIT_CONFIG_COUNT".into(), (n + 1).to_string()),
    ]
}

/// Label marking containers whose command runs the boot hook
/// ([`container_command`]); older ones run a bare `sleep infinity`, so `start`
/// must keep running their `postStartCommand` from the host.
pub(crate) const BOOT_HOOK_LABEL: &str = "devsandbox.boot_hook";

/// Label recording the identity plan a container was created with
/// (`identity::Plan::label`; docs/architecture/instances/identity.md).
pub(crate) const IDENTITY_LABEL: &str = "devsandbox.identity";

/// Label carrying the owning dispatcher's `instance_id` on a child container.
pub(crate) const DISPATCHER_LABEL: &str = "devsandbox.dispatcher";

/// The container command. Same trick as devcontainers: keep the container
/// alive, work happens via exec. Before the keep-alive, in the background,
/// `devsbd boot` brings back what the host set up via exec (the daemon, and
/// `postStartCommand` in runtime mode) when the runtime restarts the container
/// by itself (docs/architecture/helper/in-container.md). On first create the
/// helper isn't installed yet, so the `-x` test makes the hook a no-op. It
/// reaches a feature entrypoint chain as its `"$@"` like any command.
fn container_command() -> Vec<String> {
    let bin = crate::devsbd::BIN;
    vec!["sh".into(), "-c".into(), format!("[ -x {bin} ] && {bin} boot & exec sleep infinity")]
}

/// Shared with the verb client (`serve::verbs_client`): it writes into the
/// user's dir, so it runs on the client, never in the daemon.
pub(crate) fn offer_example_config(dir: &Path) -> Result<()> {
    let existing = crate::config::find(dir)?;
    let path = existing.clone().unwrap_or_else(|| dir.join(CONFIG_FILE));
    eprintln!("No sandboxes defined in {}.", path.display());
    if !std::io::stdin().is_terminal() {
        bail!("nothing to run");
    }
    eprint!("Generate an example config? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y") {
        bail!("nothing to run");
    }
    if existing.is_some() {
        bail!("{} already exists, not overwriting", path.display());
    }
    std::fs::write(&path, EXAMPLE_CONFIG)
        .with_context(|| format!("cannot write {}", path.display()))?;
    eprintln!("Wrote {}. Edit it, then re-run `devsandbox run`.", path.display());
    Ok(())
}

const EXAMPLE_CONFIG: &str = r#"# devsandbox example config

[template.base]
caches = ["pnpm"]

[sandbox.example]
extends = "base"
folder = "../example"
image = "mcr.microsoft.com/devcontainers/base:ubuntu"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn instance(name: &str) -> Instance {
        Instance {
            sandbox: "repo".into(),
            instance_id: name.into(),
            project: "proj1234".into(),
            container: format!("{NAME_PREFIX}{name}"),
            folder: "/tmp/repo".into(),
            base_folder: "/tmp/repo".into(),
            worktree: None,
            branch: None,
            branch_created: true,
            folders: Vec::new(),
            shell_history: None,
            workspace: "/workspaces/repo".into(),
            workspace_file: None,
            remote_env: Default::default(),
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

    /// Only this dispatcher's own orphaned child, with no worktree left, is
    /// replaced; anything else keeps the "not in state" refusal.
    #[test]
    fn orphan_check_reclaims_only_a_bare_child_of_this_dispatcher() {
        let c = "devsandbox-web-pr-1";
        let err = |d, l, left: &[PathBuf]| orphan_check(c, d, l, left).unwrap_err().to_string();
        orphan_check(c, Some("disp"), Some("disp"), &[]).unwrap();
        // A user's run, or a container with no or another dispatcher's label.
        assert!(err(None, Some("disp"), &[]).contains("not in state; remove it or pick another --name"));
        assert!(err(Some("disp"), None, &[]).contains("pick another --name"));
        assert!(err(Some("disp"), Some("other"), &[]).contains("pick another --name"));
        // Its worktree is still there: may hold work, so the host decides.
        let wt = PathBuf::from("/cfg/.worktrees/web-pr-1");
        let msg = err(Some("disp"), Some("disp"), &[wt]);
        assert!(msg.contains("worktree `/cfg/.worktrees/web-pr-1` is still on disk"), "{msg}");
    }

    /// The `run --json` document: schema-1 envelope, host paths as strings,
    /// `worktree`/`branch` null without a worktree and set with one.
    #[test]
    fn run_record_serializes_in_the_envelope() {
        let doc = |key: &str, info: &Instance| {
            let json = serde_json::to_string(&crate::commands::status::Envelope::new(RunRecord::new(key, info)));
            serde_json::from_str::<serde_json::Value>(&json.unwrap()).unwrap()
        };
        let base = doc("repo", &instance("repo"));
        assert_eq!(
            base,
            serde_json::json!({"schema": 1, "data": {
                "name": "repo",
                "instance_id": "repo",
                "sandbox": "repo",
                "container": "devsandbox-repo",
                "workspace": "/workspaces/repo",
                "folder": "/tmp/repo",
                "base_folder": "/tmp/repo",
                "worktree": null,
                "branch": null,
            }})
        );

        let mut wt = instance("repo-2");
        wt.folder = "/cfg/.worktrees/repo-2".into();
        wt.worktree = Some("/cfg/.worktrees/repo-2".into());
        wt.branch = Some("sandbox/repo-2".into());
        let data = &doc("renamed", &wt)["data"];
        assert_eq!(data["name"], "renamed");
        assert_eq!(data["instance_id"], "repo-2");
        assert_eq!(data["folder"], "/cfg/.worktrees/repo-2");
        assert_eq!(data["worktree"], "/cfg/.worktrees/repo-2");
        assert_eq!(data["branch"], "sandbox/repo-2");
    }

    #[test]
    fn extra_env_falls_back_to_the_prior_entry_only_when_none_given() {
        let prior = BTreeMap::from([("PR_NUMBER".to_string(), "7".to_string())]);
        // `rebuild` passes no env: the recorded one is kept.
        assert_eq!(effective_extra_env(&[], Some(&prior)), prior);
        assert!(effective_extra_env(&[], None).is_empty());
        // Given env replaces the recorded one (not merged); a duplicate key
        // keeps the last value, as `-e` would.
        let given = vec![
            ("A".to_string(), "1".to_string()),
            ("A".to_string(), "2".to_string()),
        ];
        assert_eq!(
            effective_extra_env(&given, Some(&prior)),
            BTreeMap::from([("A".to_string(), "2".to_string())])
        );
    }

    #[test]
    fn worktree_branch_substitutes_only_trusted_patterns() {
        let ctx = MountContext {
            config_dir: "/cfg",
            workspace_folder: "/src/repo",
            workspace_folder_basename: "repo",
            shared_volumes: "/cfg/shared-volumes",
            instance: "web-1",
            container_env: None,
        };
        let own = |s: &str| Some(s.to_string());
        // Config pattern and the default: substituted.
        assert_eq!(worktree_branch(None, true, Some("agent/${instance}"), &ctx), "agent/web-1");
        assert_eq!(worktree_branch(None, false, None, &ctx), "sandbox/web-1");
        // A user's own `--branch` is a pattern too.
        assert_eq!(worktree_branch(own("u/${instance}"), false, Some("x"), &ctx), "u/web-1");
        // A dispatcher's override is data, never expanded.
        assert_eq!(worktree_branch(own("d/${instance}"), true, Some("x"), &ctx), "d/${instance}");
        assert_eq!(
            worktree_branch(own("x-${localEnv:HOME}"), true, None, &ctx),
            "x-${localEnv:HOME}"
        );
    }

    #[test]
    fn run_rejects_a_bad_dispatcher_branch_before_any_work() {
        let extras = RunExtras { dispatcher: Some("d".into()), ..Default::default() };
        for bad in ["x-${localEnv:HOME}", "-x", "a..b"] {
            let dir = Path::new("/nonexistent/devsandbox-test");
            let err = run(dir, Some("web".into()), None, Some(bad.into()), None, extras.clone())
                .unwrap_err()
                .to_string();
            assert!(err.starts_with("bad --branch for a dispatcher child"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn run_rejects_a_denied_dispatcher_env_before_any_work() {
        let dir = Path::new("/nonexistent/devsandbox-test");
        for name in ["PATH", "LD_PRELOAD", "Path", "GIT_SSH_COMMAND"] {
            let extras = RunExtras {
                env: vec![("OK".into(), "1".into()), (name.into(), "/tmp/p".into())],
                dispatcher: Some("d".into()),
                ..Default::default()
            };
            let err = run(dir, Some("web".into()), None, None, None, extras).unwrap_err().to_string();
            assert_eq!(err, format!("--env `{name}` may not be set by a dispatcher"));
        }
    }

    #[test]
    fn ordinal_naming_skips_taken_names() {
        let mut state = State::default();
        state.instances.insert("repo".into(), instance("repo"));
        state.instances.insert("repo-2".into(), instance("repo-2"));
        assert_eq!(first_free_ordinal(&state, "repo"), "repo-3");
        // Deterministic: no random component.
        assert_eq!(first_free_ordinal(&state, "repo"), "repo-3");
    }

    #[test]
    fn default_name_is_sandbox_when_absent() {
        let state = State::default();
        assert_eq!(default_instance_name(&state, "repo"), "repo");
    }

    #[test]
    fn link_store_is_per_repo() {
        let cfg = Path::new("/cfg");
        let a = link_store(cfg, Path::new("/src/a/app"), "app");
        assert!(a.starts_with("/cfg/shared-files"), "{}", a.display());
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("app-"));
        // Same folder, same store (whichever sandbox asks); same basename
        // elsewhere, a different one.
        assert_eq!(a, link_store(cfg, Path::new("/src/a/app"), "app"));
        assert_ne!(a, link_store(cfg, Path::new("/src/b/app"), "app"));
    }

    #[test]
    fn base_in_use_ignores_worktree_instances() {
        let base = Path::new("/tmp/repo");
        let mut state = State::default();
        // A worktree instance references the base via `base_folder` but mounts
        // its own tree: the base stays reclaimable for a direct mount.
        let mut wt = instance("repo-2");
        wt.folder = "/cfg/.worktrees/repo-2".into();
        wt.worktree = Some("/cfg/.worktrees/repo-2".into());
        state.instances.insert("repo-2".into(), wt);
        assert!(!base_in_use(&state, base));

        // A `folders` entry mounted through a worktree doesn't hold it either...
        let mut other = instance("other");
        other.folder = "/tmp/other".into();
        other.base_folder = "/tmp/other".into();
        other.folders.push(crate::state::FolderMount {
            target: "/workspaces/repo".into(),
            base: base.into(),
            worktree: Some("/cfg/.worktrees/other.folders/repo-12345678".into()),
        });
        state.instances.insert("other".into(), other);
        assert!(!base_in_use(&state, base));

        // ...but a direct `folders` mount does.
        state.instances.get_mut("other").unwrap().folders[0].worktree = None;
        assert!(base_in_use(&state, base));
        state.instances.remove("other");

        // A direct-mount instance (even stopped) reserves it.
        state.instances.insert("repo".into(), instance("repo"));
        assert!(base_in_use(&state, base));
    }

    #[test]
    fn default_name_ordinal_when_taken_even_if_stopped() {
        // A stopped instance keeps its name (`start` restarts it); `run` must
        // pick the next ordinal, never reuse it.
        let mut state = State::default();
        state.instances.insert("repo".into(), instance("repo"));
        assert_eq!(default_instance_name(&state, "repo"), "repo-2");
    }

    #[test]
    fn instance_id_is_name_when_free() {
        let state = State::default();
        assert_eq!(unique_instance_id(&state, "repo"), "repo");
    }

    #[test]
    fn instance_id_suffixes_when_rename_kept_the_id() {
        // `repo` was created (id `repo`) then renamed to `other`: the *name*
        // is free again but the id is not — a new `repo` gets `repo-1`.
        let mut state = State::default();
        state.instances.insert("other".into(), instance("repo"));
        assert_eq!(unique_instance_id(&state, "repo"), "repo-1");

        // A second collision (id `repo-1` also taken) moves to `repo-2`.
        state.instances.insert("other-2".into(), instance("repo-1"));
        assert_eq!(unique_instance_id(&state, "repo"), "repo-2");
    }

    // --- folders / generated workspace file ---

    #[test]
    fn container_command_hooks_then_keeps_alive() {
        assert_eq!(
            container_command(),
            vec![
                "sh",
                "-c",
                "[ -x /run/devsandbox/bin/devsbd ] && /run/devsandbox/bin/devsbd boot & exec sleep infinity",
            ]
        );
    }

    /// Docker-gated: a container running the hook command, with the helper and
    /// a boot file installed, reruns the recorded command (as the recorded
    /// user, with its env) and brings the daemon back on a runtime restart.
    #[test_utils::docker_test(helper)]
    fn boot_hook_reruns_post_start_on_restart_with_docker() -> Result<(), &'static str> {
        use std::time::{Duration, Instant};
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("devsandbox-boot-test-{stamp}");
        let mut args = vec!["run".to_string(), "-d".into(), "--name".into(), name.clone(), "alpine:3.20".into()];
        args.extend(container_command());
        let up = crate::test_support::runtime_cmd().args(&args).output().unwrap();
        assert!(up.status.success(), "{}", String::from_utf8_lossy(&up.stderr));
        let in_container = |script: &str| {
            let out = crate::test_support::runtime_cmd().args(["exec", &name, "sh", "-c", script]).output().unwrap();
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        let poll = |script: &str| {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Some(out) = in_container(script) {
                    return Some(out);
                }
                if Instant::now() > deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };

        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            crate::devsbd::install(&name, None).unwrap();
            let spec = crate::devsbd::bootfile::BootSpec {
                user: Some("nobody".into()),
                cwd: Some("/tmp".into()),
                env: vec![("MARK".into(), "ok".into())],
                cmds: vec![vec!["sh".into(), "-c".into(), "id -u > marker-$MARK; echo ran".into()]],
            };
            crate::devsbd::sync_boot(&name, Some(&spec), false);
            // First start had no helper: the hook was a no-op.
            assert_eq!(in_container("cat /tmp/marker-ok"), None);

            let restart = crate::test_support::runtime_cmd().args(["restart", "-t", "0", &name]).output().unwrap();
            assert!(restart.status.success(), "{}", String::from_utf8_lossy(&restart.stderr));
            assert_eq!(poll("cat /tmp/marker-ok").as_deref(), Some("65534"), "postStartCommand not rerun");
            assert!(
                poll("kill -0 \"$(cut -d' ' -f1 /run/devsandbox/devsbd.pid)\"").is_some(),
                "daemon not running after restart"
            );
            let log = in_container("cat /run/devsandbox/boot.log").unwrap_or_default();
            assert!(log.contains("devsbd boot ===") && log.contains("\nran"), "{log}");

            // No boot file: the hook only restarts the daemon.
            crate::devsbd::sync_boot(&name, None, false);
            assert_eq!(in_container("test -e /run/devsandbox/boot && echo present"), None);
        });
        Ok(())
    }

    /// Env var naming the child test [`isolated`] re-runs, and the root it
    /// gets (its data and runtime dirs live there).
    const ISOLATED_TEST: &str = "DEVSANDBOX_ISOLATED_TEST";
    const ISOLATED_ROOT: &str = "DEVSANDBOX_ISOLATED_ROOT";

    /// Re-run `child` (a test of this module) in a child process of this test
    /// binary with its own `XDG_DATA_HOME`/`XDG_RUNTIME_DIR` and no ssh agent:
    /// `run` reads and writes `state.toml` and may reach the serve daemon,
    /// and neither may be the user's. Env is process-wide, hence the child.
    fn isolated(child: &str) -> Result<(), String> {
        isolated_with(child, &[])
    }

    /// [`isolated`] with `env` set on the child too.
    fn isolated_with(child: &str, env: &[(&str, &str)]) -> Result<(), String> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("devsandbox-phase-{child}-{stamp}"));
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        crate::test_support::share_runtime_dirs(&root.join("data"), &root.join("runtime"));
        // libtest names drop the crate segment of `module_path!`.
        let module = module_path!().split_once("::").map_or(module_path!(), |(_, rest)| rest);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::{child}"), "--nocapture", "--test-threads=1"])
            .env(ISOLATED_TEST, child)
            .env(ISOLATED_ROOT, &root)
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env_remove("SSH_AUTH_SOCK")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "child `{child}` failed:\n{text}");
        assert!(text.contains("1 passed"), "child `{child}` did not run:\n{text}");
        Ok(())
    }

    /// A config under a fleet root (`<data>/devsandbox/fleet/<hub>/`, here or
    /// through a symlink) asking for `privileged` is refused by `run` before
    /// any runtime call: the child has no runtime on its `PATH`, so a call
    /// would fail with another error, and nothing is recorded. Allowed in
    /// `member.toml`, `run` gets past the policy (to the missing runtime).
    #[cfg(unix)]
    #[test]
    fn run_under_a_fleet_root_is_refused_before_the_runtime() {
        isolated_with("fleet_policy_child", &[("PATH", "/nonexistent"), ("DOCKER_HOST", "unix:///nonexistent")])
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fleet_policy_child() {
        let Some(root) = isolated_root("fleet_policy_child") else { return };
        let data = root.join("data/devsandbox");
        let cfg = data.join("fleet/me@hub/cfg");
        std::fs::create_dir_all(cfg.join("repo")).unwrap();
        std::fs::write(
            cfg.join(CONFIG_FILE),
            "[sandbox.t]\nfolder = \"repo\"\nimage = \"alpine:3.20\"\nprivileged = true\n",
        )
        .unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&cfg, &link).unwrap();
        let want = "refused by this machine's lending policy: privileged \
                    (allow with privileged = true under [allow] in member.toml)";
        for dir in [&cfg, &link] {
            let err = run(dir, Some("t".into()), None, None, None, RunExtras::default()).unwrap_err();
            assert_eq!(format!("{err:#}"), want, "{}", dir.display());
        }
        assert!(State::load().unwrap().instances.is_empty(), "nothing recorded");

        // `${localEnv:…}`: refused by `run`, and never expanded for a config
        // under the root unless its name is allowed.
        let env_cfg = data.join("fleet/me@hub/env");
        std::fs::create_dir_all(env_cfg.join("repo")).unwrap();
        std::fs::write(
            env_cfg.join(CONFIG_FILE),
            "[sandbox.t]\nfolder = \"repo\"\nimage = \"alpine:3.20\"\ncontainerEnv = { T = \"${localEnv:HOME}\" }\n",
        )
        .unwrap();
        let err = run(&env_cfg, Some("t".into()), None, None, None, RunExtras::default()).unwrap_err();
        assert!(format!("{err:#}").contains("`${localEnv:HOME}`"), "{err:#}");
        assert!(format!("{err:#}").contains("local-env = [\"HOME\"]"), "{err:#}");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let expand = |dir: &Path| {
            let dir = dir.to_string_lossy().into_owned();
            let ctx = MountContext {
                config_dir: &dir,
                workspace_folder: "/w",
                workspace_folder_basename: "w",
                shared_volumes: "/sv",
                instance: "i",
                container_env: None,
            };
            substitute("${localEnv:HOME}|${localEnv:USER}", &ctx)
        };
        let home = std::env::var("HOME").unwrap_or_default();
        let user = std::env::var("USER").unwrap_or_default();
        assert_eq!(expand(&env_cfg), "${localEnv:HOME}|${localEnv:USER}");
        assert_eq!(expand(&outside), format!("{home}|{user}"));
        std::fs::write(data.join("member.toml"), "[allow]\nlocal-env = [\"HOME\"]\n").unwrap();
        assert_eq!(expand(&env_cfg), format!("{home}|${{localEnv:USER}}"));

        std::fs::write(data.join("member.toml"), "[allow]\nprivileged = true\n").unwrap();
        let err = run(&cfg, Some("t".into()), None, None, None, RunExtras::default()).unwrap_err();
        assert!(!format!("{err:#}").contains("lending policy"), "{err:#}");
    }

    /// The child's root when this process is [`isolated`]'s child for `test`.
    fn isolated_root(test: &str) -> Option<PathBuf> {
        (std::env::var(ISOLATED_TEST).ok()? == test).then(|| std::env::var_os(ISOLATED_ROOT).map(PathBuf::from))?
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `root/repo` (one commit) and `root/cfg/devsandboxes.toml` with sandbox
    /// `t` on it, `extra` appended to its table. Returns (cfg, canonical repo).
    fn phase_fixture(root: &Path, extra: &str) -> (PathBuf, PathBuf) {
        let (repo, cfg) = (root.join("repo"), root.join("cfg"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        std::fs::write(
            cfg.join(CONFIG_FILE),
            format!("[sandbox.t]\nfolder = \"../repo\"\nimage = \"alpine:3.20\"\n{extra}\n"),
        )
        .unwrap();
        (cfg, repo.canonicalize().unwrap())
    }

    /// A `run` failing before the container (in `initializeCommand`, after
    /// its worktree and branch were made) is kept as failed, with everything
    /// on its record, so `rm` then leaves nothing: no worktree, no branch.
    #[test_utils::docker_test]
    fn run_failing_before_the_container_is_kept_until_rm_with_docker() -> Result<(), String> {
        isolated("rollback_child")
    }

    #[test]
    fn rollback_child() {
        let Some(root) = isolated_root("rollback_child") else { return };
        let (cfg, repo) = phase_fixture(&root, "initializeCommand = \"false\"");
        // Another instance mounts the repo directly, so this run gets a worktree.
        let mut holder = instance("holder");
        holder.folder = repo.clone();
        holder.base_folder = repo.clone();
        State::update(|s| {
            s.instances.insert("holder".into(), holder);
            Ok(())
        })
        .unwrap();

        let err = run(&cfg, Some("t".into()), Some("rb".into()), None, None, RunExtras::default()).unwrap_err();
        assert!(format!("{err:#}").contains("initializeCommand failed"), "{err:#}");
        assert!(format!("{err:#}").contains("`devsandbox rm rb` removes it"), "{err:#}");

        let state = State::load().unwrap();
        let info = state.instances.get("rb").expect("failed instance kept");
        let progress = info.progress.as_ref().expect("progress kept");
        assert_eq!(progress.phase, Phase::Failed { at: "preparing".into() });
        assert_eq!(info.worktree.as_deref(), Some(cfg.join(".worktrees/rb").as_path()));

        crate::commands::rm::rm("rb", Some(true), false, false).unwrap();
        let state = State::load().unwrap();
        assert_eq!(state.instances.keys().collect::<Vec<_>>(), ["holder"]);
        assert!(!cfg.join(".worktrees/rb").exists(), "worktree left behind");
        assert!(!git(&repo, &["worktree", "list", "--porcelain"]).contains(".worktrees/rb"));
        assert_eq!(git(&repo, &["branch", "--list", "sandbox/rb"]), "", "branch left behind");
    }

    /// A `run` failing from the container on (in `postCreateCommand`) keeps
    /// the instance, recorded as failed at that step.
    #[test_utils::docker_test]
    fn run_failing_in_the_lifecycle_keeps_a_failed_record_with_docker() -> Result<(), String> {
        isolated("failed_child")
    }

    #[test]
    fn failed_child() {
        let Some(root) = isolated_root("failed_child") else { return };
        let (cfg, _repo) = phase_fixture(&root, "postCreateCommand = \"false\"");
        // Unique per run: the container name is global to the runtime.
        let name = format!("phase-{}", std::process::id());
        let project = services::project_id(&cfg).unwrap();
        let container = format!("{NAME_PREFIX}{name}");
        let cleanup = || crate::commands::rm::remove_container(&container, &project, &name);
        crate::test_support::with_cleanup(cleanup, || {
            let err = run(&cfg, Some("t".into()), Some(name.clone()), None, None, RunExtras::default())
                .unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("postCreateCommand failed"), "{msg}");
            assert!(msg.contains(&format!("`devsandbox rebuild {name}` retries")), "{msg}");

            let state = State::load().unwrap();
            let info = state.instances.get(&name).expect("failed instance kept");
            let progress = info.progress.as_ref().expect("progress kept");
            assert_eq!(progress.phase, Phase::Failed { at: "postCreateCommand".into() });
            assert!(progress.error.as_deref().is_some_and(|e| e.contains("postCreateCommand failed")));
            assert_eq!(progress.pid, std::process::id());
            assert_eq!(info.workspace, "/workspaces/repo");
            assert!(backend().is_running(&container).unwrap().is_some(), "container kept");
        });
    }

    /// `run` under the default `identity = "host"` fits the image's account
    /// to the host (docs/architecture/instances/identity.md), so a file the
    /// exec user writes into the workspace is the host user's: remapped on
    /// rootful docker, root-is-you on rootless, the share on a VM.
    #[cfg(unix)]
    #[test_utils::docker_test(helper)]
    fn workspace_files_are_host_owned_with_docker() -> Result<(), String> {
        isolated("identity_child")
    }

    #[cfg(unix)]
    #[test]
    fn identity_child() {
        let Some(root) = isolated_root("identity_child") else { return };
        let (repo, cfg) = (root.join("repo"), root.join("cfg"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        // Unique per run: container and image names are global to the runtime.
        let name = format!("ident-{}", std::process::id());
        std::fs::write(cfg.join("Dockerfile"), "FROM alpine:3.20\nRUN adduser -D -u 1000 dev\n").unwrap();
        std::fs::write(
            cfg.join(CONFIG_FILE),
            format!("[sandbox.{name}]\nfolder = \"../repo\"\nbuild.dockerfile = \"Dockerfile\"\nremoteUser = \"dev\"\n"),
        )
        .unwrap();
        let project = services::project_id(&cfg).unwrap();
        let container = format!("{NAME_PREFIX}{name}");
        let image = image::sandbox_image(&project, &name);
        let cleanup = || {
            crate::commands::rm::remove_container(&container, &project, &name);
            let _ = backend().output_quiet(&["rmi", "-f", &image]);
        };
        crate::test_support::with_cleanup(cleanup, || {
            run(&cfg, Some(name.clone()), Some(name.clone()), None, None, RunExtras::default()).unwrap();
            let state = State::load().unwrap();
            let info = state.instances.get(&name).expect("instance recorded");
            let label = backend().label(&container, IDENTITY_LABEL).unwrap().expect("identity label");
            let plan = crate::runtime::identity::Plan::parse_label(&label).expect("parsable label");
            assert_eq!(info.remote_user, plan.exec_user, "{label}");
            let touch = ["sh".to_string(), "-c".into(), "touch owned-by-me".into()];
            let argv = crate::commands::exec::exec_argv_with(info, false, false, &touch, false);
            backend().run_checked(&argv.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
            use std::os::unix::fs::MetadataExt;
            let owner = std::fs::metadata(repo.join("owned-by-me")).unwrap().uid();
            // SAFETY: getuid has no preconditions and can't fail.
            assert_eq!(owner, unsafe { libc::getuid() }, "{label}");
        });
    }

    /// `run` checks user namespaces in the new container, records the verdict
    /// and the tool env, and every exec gets that env under the sandbox's own
    /// keys (docs/architecture/instances/identity.md, *Tool environment*).
    #[test_utils::docker_test(helper)]
    fn tool_env_reaches_exec_with_docker() -> Result<(), String> {
        isolated("tool_env_child")
    }

    #[test]
    fn tool_env_child() {
        let Some(root) = isolated_root("tool_env_child") else { return };
        let (repo, cfg) = (root.join("repo"), root.join("cfg"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        // Unique per run: container and image names are global to the runtime.
        let name = format!("toolenv-{}", std::process::id());
        std::fs::write(cfg.join("Dockerfile"), "FROM alpine:3.20\nRUN adduser -D -u 1000 dev\n").unwrap();
        std::fs::write(
            cfg.join(CONFIG_FILE),
            format!(
                "[sandbox.{name}]\nfolder = \"../repo\"\nbuild.dockerfile = \"Dockerfile\"\nremoteUser = \"dev\"\n\
                 containerEnv = {{ CHROMIUM_FLAGS = \"--mine\" }}\nremoteEnv = {{ PIP_ROOT_USER_ACTION = \"warn\" }}\n"
            ),
        )
        .unwrap();
        let project = services::project_id(&cfg).unwrap();
        let container = format!("{NAME_PREFIX}{name}");
        let image = image::sandbox_image(&project, &name);
        let cleanup = || {
            crate::commands::rm::remove_container(&container, &project, &name);
            let _ = backend().output_quiet(&["rmi", "-f", &image]);
        };
        crate::test_support::with_cleanup(cleanup, || {
            run(&cfg, Some(name.clone()), Some(name.clone()), None, None, RunExtras::default()).unwrap();
            let state = State::load().unwrap();
            let info = state.instances.get(&name).expect("instance recorded");
            assert!(info.browser_sandbox.is_some(), "the helper ran the check");
            let env_cmd = ["env".to_string()];
            let argv = crate::commands::exec::exec_argv_with(info, false, false, &env_cmd, false);
            let out = backend().output_quiet(&argv.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
            let env: BTreeMap<&str, &str> = out.lines().filter_map(|l| l.split_once('=')).collect();
            // The sandbox's own keys: containerEnv's untouched, remoteEnv's on top.
            assert_eq!(env.get("CHROMIUM_FLAGS"), Some(&"--mine"), "{out}");
            assert!(!info.tool_env.contains_key("CHROMIUM_FLAGS"), "{:?}", info.tool_env);
            let root = tool_env::root_plan(info.remote_user.as_deref());
            let knobs = info.browser_sandbox != Some(BrowserSandbox::On);
            assert_eq!(env.get("PUPPETEER_DANGEROUS_NO_SANDBOX") == Some(&"1"), knobs, "{out}");
            assert_eq!(env.get("ELECTRON_DISABLE_SANDBOX") == Some(&"1"), knobs, "{out}");
            assert_eq!(env.get("DEVSANDBOX_BROWSER_SANDBOX") == Some(&"1"), !knobs, "{out}");
            if root {
                // Rootless docker (this host's case): the silencers too.
                assert_eq!(env.get("BUNDLE_SILENCE_ROOT_WARNING"), Some(&"1"), "{out}");
                assert_eq!(env.get("PIP_ROOT_USER_ACTION"), Some(&"warn"), "{out}");
            }
            if root && !knobs {
                // Through the wrapper, ahead of alpine's own PATH.
                let wrapper = crate::devsbd::BROWSER_WRAPPER;
                assert_eq!(env.get("CHROME_PATH"), Some(&wrapper), "{out}");
                assert_eq!(env.get("DEVSANDBOX_CHROME"), Some(&wrapper), "{out}");
                assert_eq!(
                    env.get("PATH"),
                    Some(&"/run/devsandbox/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
                    "{out}"
                );
                let which = ["sh".to_string(), "-c".into(), "command -v chromium".into()];
                let argv = crate::commands::exec::exec_argv_with(info, false, false, &which, false);
                let out = backend().output_quiet(&argv.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
                assert_eq!(out.trim(), "/run/devsandbox/bin/chromium");
            }
        });
    }

    /// `${containerEnv:HOME}` resolves to the adopted account's home as the
    /// built image's passwd has it, in a mount target and in containerEnv /
    /// remoteEnv values, and the facts get cached by image id. The account's
    /// home and uid are off the conventions, so no fallback could pass.
    #[test_utils::docker_test]
    fn container_env_home_reaches_mounts_and_env_with_docker() -> Result<(), String> {
        isolated("container_env_child")
    }

    #[test]
    fn container_env_child() {
        let Some(root) = isolated_root("container_env_child") else { return };
        let (repo, cfg) = (root.join("repo"), root.join("cfg"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        // Unique per run: container, image and volume names are global to the runtime.
        let name = format!("cenv-{}", std::process::id());
        std::fs::write(cfg.join("Dockerfile"), "FROM alpine:3.20\nRUN adduser -D -u 1234 -h /srv/dev dev\n").unwrap();
        std::fs::write(
            cfg.join(CONFIG_FILE),
            format!(
                "[sandbox.{name}]\nfolder = \"../repo\"\nbuild.dockerfile = \"Dockerfile\"\nremoteUser = \"dev\"\n\
                 mounts = [\"type=volume,source=cenv-${{instance}},target=${{containerEnv:HOME}}/.cfg\"]\n\
                 containerEnv = {{ CFG = \"${{containerEnv:HOME}}/.cfg\" }}\n\
                 remoteEnv = {{ WHERE = \"${{containerEnv:CFG}}\" }}\n"
            ),
        )
        .unwrap();
        let project = services::project_id(&cfg).unwrap();
        let container = format!("{NAME_PREFIX}{name}");
        let image = image::sandbox_image(&project, &name);
        let volume = format!("cenv-{name}");
        let cleanup = || {
            crate::commands::rm::remove_container(&container, &project, &name);
            let _ = backend().remove_volume(&volume);
            let _ = backend().output_quiet(&["rmi", "-f", &image]);
        };
        crate::test_support::with_cleanup(cleanup, || {
            run(&cfg, Some(name.clone()), Some(name.clone()), None, None, RunExtras::default()).unwrap();
            let state = State::load().unwrap();
            let info = state.instances.get(&name).expect("instance recorded");
            assert_eq!(info.remote_env.get("WHERE").map(String::as_str), Some("/srv/dev/.cfg"));
            let probe = ["sh".to_string(), "-c".into(), "echo \"$CFG|$WHERE\"; grep -c ' /srv/dev/.cfg ' /proc/mounts".into()];
            let argv = crate::commands::exec::exec_argv_with(info, false, false, &probe, false);
            let out = backend().output_quiet(&argv.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
            assert_eq!(out.lines().collect::<Vec<_>>(), ["/srv/dev/.cfg|/srv/dev/.cfg", "1"], "{out}");
            let id = backend().image_id(&image).unwrap().expect("image id");
            let facts = &state.image_facts[&id];
            let passwd = facts.passwd.as_deref().expect("passwd cached");
            let dev = crate::runtime::identity::account(passwd, "dev").expect("dev in passwd");
            assert_eq!((dev.uid, dev.home.as_str()), (1234, "/srv/dev"));
        });
    }

    #[test]
    fn claim_name_rechecks_name_and_id() {
        let mut state = State::default();
        claim_name(&state, "repo", "repo").unwrap();
        state.instances.insert("repo".into(), instance("repo"));
        let err = claim_name(&state, "repo", "repo-1").unwrap_err().to_string();
        assert!(err.starts_with("instance `repo` already exists"), "{err}");
        // Renamed away, id still held: the freed name can't reuse that id.
        let mut renamed = State::default();
        renamed.instances.insert("other".into(), instance("repo"));
        assert!(claim_name(&renamed, "repo", "repo").is_err());
        claim_name(&renamed, "repo", "repo-1").unwrap();
    }

    #[test]
    fn gc_keeps_worktrees_env_extends_a_given_count() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        let guard = |given: &[(String, String)]| gc_keeps_worktrees_env(given.iter().map(|(k, v)| (k, v)));
        assert_eq!(
            guard(&env(&[("FOO", "1")])),
            env(&[
                ("GIT_CONFIG_KEY_0", "gc.worktreePruneExpire"),
                ("GIT_CONFIG_VALUE_0", "never"),
                ("GIT_CONFIG_COUNT", "1"),
            ])
        );
        // The last value wins, as with repeated `-e`.
        let given = env(&[("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_COUNT", "2")]);
        assert_eq!(guard(&given)[0].0, "GIT_CONFIG_KEY_2");
        assert_eq!(guard(&given)[2], ("GIT_CONFIG_COUNT".to_string(), "3".to_string()));
        assert!(guard(&env(&[("GIT_CONFIG_COUNT", "x")])).is_empty());
    }
}
