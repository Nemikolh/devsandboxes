use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{container_drifted, pick, services};
use crate::config::{
    build_hash, parse_shorthand, substitute, Config, FeatureOptions, LifecycleCommand,
    MountContext, ResolvedMount, ResolvedSandbox, SandboxProperties, SimpleCommand, CONFIG_FILE,
};
use crate::features::{self, FeatureMetadata};
use crate::runtime::{backend, ServiceEndpoint, NAME_PREFIX};
use crate::state::{Instance, State};

/// Branch pattern for worktree instances when neither `--branch` nor the
/// sandbox's `worktree-branch` is set. Also the TUI prompt's completion base
/// after `--branch`.
pub const DEFAULT_WORKTREE_BRANCH: &str = "sandbox/${instance}";

/// Fixed container path the forwarded ssh-agent socket is mounted at, so Linux
/// and macOS converge on one target regardless of the host source (see
/// docs/ssh-agent.md).
pub(crate) const SSH_AGENT_TARGET: &str = "/run/devsandbox/ssh-agent.sock";

pub fn run(
    dir: &Path,
    sandbox_name: Option<String>,
    instance_name: Option<String>,
    branch_override: Option<String>,
    base_override: Option<String>,
) -> Result<()> {
    let config = match Config::load(dir) {
        Ok(config) if !config.sandboxes.is_empty() => config,
        _ => return offer_example_config(dir),
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
        bail!(
            "container `{container_name}` already exists but is not in state; \
             remove it or pick another --name"
        );
    }

    let var_ctx = MountContext {
        config_dir: &config_dir_str,
        workspace_folder: &folder_str,
        workspace_folder_basename: &basename,
        shared_volumes: &shared_volumes,
        instance: &instance_id,
    };

    let (source, worktree, branch) = if base_in_use(&state, &folder) {
        // Branch for the worktree: `--branch` override, else the sandbox's
        // `worktree-branch`, else the default. `${instance}` (and the other mount
        // variables) are substituted so each instance gets a unique branch.
        let pattern = branch_override
            .or_else(|| props.worktree_branch.clone())
            .unwrap_or_else(|| DEFAULT_WORKTREE_BRANCH.to_string());
        let branch = substitute(&pattern, &var_ctx);
        // Must be absolute: `create_worktree` runs `git -C <base>`, so a
        // `dir`-relative path would resolve under the base repo instead of here,
        // and the mount/state would point at a different (empty) directory.
        // Keyed by id: a renamed instance keeps its worktree dir, so a new
        // instance reusing the freed name must not collide with it.
        let wt = config_dir.join(".worktrees").join(&instance_id);
        // Start point: `--base`, else the sandbox's `worktree-base`, else
        // detected from the remote (see `worktree_start_point`).
        let base_ref = base_override.or_else(|| props.worktree_base.clone());
        create_worktree(&folder, &wt, &branch, base_ref.as_deref())?;
        (wt.clone(), Some(wt), Some(branch))
    } else {
        (folder.clone(), None, None)
    };

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
        &mut state,
    )?;

    println!("{instance}");
    Ok(())
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
/// git companion mount.
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
    state: &mut State,
) -> Result<()> {
    let props = &sandbox.properties;
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

    // initializeCommand runs on the host, before anything is created.
    if let Some(cmd) = &props.initialize_command {
        run_host_commands(dir, cmd).context("initializeCommand failed")?;
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
    // ssh-agent: relay-first. On unix with an embedded helper we never mount
    // the socket — the in-container `devsbd` daemon serves it over exec stdio
    // (docs/sandbox-helper.md), which also fixes rotation-while-running and
    // works on runtimes that can't bind files. Only builds without a helper
    // (`cargo install`) and non-unix hosts fall back to the bind mount.
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
    let mut mounts = resolve_mounts(dir, folder, &basename, instance_id, sandbox)?;
    for (target, host) in &extra_folders {
        mounts.push(format!("type=bind,source={},target={target}", host.display()));
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
    // `shell-rc`: host snippets mounted read-only, sourced by the rc files.
    let shell_rc = resolve_shell_rc(dir, &var_ctx, props)?;
    mounts.extend(shell_rc.iter().map(|(mount, _)| mount.clone()));
    // Feature-declared mounts come last: any target claimed above (or the
    // workspace itself) wins, so the config can override a feature's mount.
    let feature = feature_container_opts(props, &var_ctx, &mounts, &workspace)?;
    mounts.extend(feature.mounts);
    sort_parents_first(&mut mounts);
    let (privileged, warnings) = privileged_decision(
        props.privileged == Some(true),
        &feature.privileged_by,
        backend().supports_privileged(),
    );
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    run_container(
        dir,
        sandbox,
        &container_name,
        source,
        folder,
        &workspace,
        &extra_mounts,
        &mounts,
        &extra_env,
        &networks,
        &endpoints,
        privileged,
    )?;
    let container = container_name.clone();
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
    let workspace_file = write_workspace_file(
        &container,
        instance,
        &workspace,
        &extra_folders,
        props.vscode_extensions().unwrap_or(&[]),
    );

    let persist_history = shell_history.is_some();
    state.instances.insert(
        instance.to_string(),
        Instance {
            sandbox: sandbox_name.to_string(),
            instance_id: instance_id.to_string(),
            project,
            container: container.clone(),
            folder: source.to_path_buf(),
            base_folder: folder.to_path_buf(),
            worktree,
            branch,
            shell_history,
            workspace: workspace.clone(),
            workspace_file,
            remote_env: props.remote_env.clone().unwrap_or_default(),
            remote_user: props.remote_user.clone(),
            ssh_auth_sock,
            devsbd_arch,
            created_unix: Instance::now(),
        },
    );
    state.save()?;

    let extensions = props.vscode_extensions().unwrap_or(&[]);
    if !extensions.is_empty() || props.remote_user.is_some() {
        write_vscode_name_config(&container, extensions, props.remote_user.as_deref())?;
    }

    // ssh-agent for the lifecycle chain. The instance was just saved, so read
    // the agent rule off it (mount → always; relay → only with a host agent).
    // In relay mode one bridge (RAII-dropped before this fn returns, `?` paths
    // included) spans shell-rc wiring + all five commands so each exec reaches
    // the host agent; its handshake failure is reported once, after the chain.
    let inst = state.instances.get(instance).expect("instance just inserted");
    let host_agent = crate::commands::exec::has_host_agent();
    let ssh_auth_sock = crate::commands::exec::ssh_auth_sock_env(inst, host_agent);
    #[cfg(unix)]
    let bridge = (crate::devsbd::relay_mode(inst) && host_agent)
        .then(|| crate::devsbd::bridge::spawn(inst))
        .flatten();

    if !shell_rc.is_empty() || persist_history {
        let paths: Vec<&str> = shell_rc.iter().map(|(_, path)| path.as_str()).collect();
        exec_lifecycle(
            &container,
            &workspace,
            props.remote_env.as_ref(),
            props.remote_user.as_deref(),
            ssh_auth_sock,
            &shell_rc_wiring(&paths, persist_history),
        )
        .with_context(|| format!("shell-rc wiring failed (container `{container}` kept)"))?;
    }

    for (name, cmd) in [
        ("onCreateCommand", &props.on_create_command),
        ("updateContentCommand", &props.update_content_command),
        ("postCreateCommand", &props.post_create_command),
        ("postStartCommand", &props.post_start_command),
        ("postAttachCommand", &props.post_attach_command),
    ] {
        if let Some(cmd) = cmd {
            exec_lifecycle(
                &container,
                &workspace,
                props.remote_env.as_ref(),
                props.remote_user.as_deref(),
                ssh_auth_sock,
                cmd,
            )
            .with_context(|| format!("{name} failed (container `{container}` kept)"))?;
        }
    }

    // Chain succeeded: surface a relay handshake failure once (as `exec_status`
    // does). On the error paths above the error is what matters, not this note.
    #[cfg(unix)]
    if let Some(Err(e)) = bridge.as_ref().and_then(|b| b.outcome(std::time::Duration::ZERO)) {
        eprintln!("note: ssh-agent relay unavailable in `{container}`: {e}");
    }

    Ok(())
}

/// Whether some instance (running or stopped — a stopped one can be started
/// anytime) already mounts `folder` as its working tree; the new instance then
/// gets a git worktree so two containers never share a working tree. Matched on
/// `folder` (the mounted source), not `base_folder`: worktree instances carry
/// the base's `base_folder` but have their own working tree, so they must not
/// keep the base checkout reserved after its direct-mount instance is removed.
fn base_in_use(state: &State, folder: &Path) -> bool {
    state.instances.values().any(|i| i.folder == folder)
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

/// Bind mount for the base repo's `.git` at the identical host path, so a
/// worktree's absolute `gitdir` pointer resolves inside the container.
fn git_companion_mount(base: &Path) -> String {
    let git = base.join(".git");
    format!("{}:{}", git.display(), git.display())
}

/// `<data-dir>/agent`, the dir holding per-instance agent symlinks. `None` when
/// the state path can't be resolved. One recipe so `run`, `rm`, and `gc` don't
/// each re-derive it (docs/ssh-agent.md).
pub(crate) fn ssh_agent_dir() -> Option<PathBuf> {
    Some(State::path().ok()?.parent()?.join("agent"))
}

/// The agent symlink path for one instance, `<data-dir>/agent/<id>.sock`. `None`
/// when the state path can't be resolved. Callers that clean up (`rm`, `gc`)
/// share this rather than duplicating the join.
pub(crate) fn ssh_agent_link_path(instance_id: &str) -> Option<PathBuf> {
    Some(ssh_agent_dir()?.join(format!("{instance_id}.sock")))
}

/// Create or re-point `agent_dir/<instance_id>.sock` at the host agent
/// socket. A symlink (not the raw path) because bind sources are
/// re-resolved at every container start, so `start` can re-point it after
/// the host agent rotates (docs/ssh-agent.md).
pub(crate) fn ssh_agent_link(agent_dir: &Path, instance_id: &str, sock: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(agent_dir)
        .with_context(|| format!("cannot create {}", agent_dir.display()))?;
    let link = agent_dir.join(format!("{instance_id}.sock"));
    match std::fs::remove_file(&link) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot replace {}", link.display())),
    }
    symlink(sock, &link)
        .with_context(|| format!("cannot link {} -> {}", link.display(), sock.display()))?;
    Ok(link)
}

#[cfg(unix)]
fn symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

/// Never reached: `ssh_agent_forward`/`ssh_agent_refresh` bail out first on
/// non-unix hosts, whose agent is a named pipe no runtime can bind
/// (docs/ssh-agent.md, _Windows_). Exists only so the crate compiles.
#[cfg(not(unix))]
fn symlink(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "ssh-agent forwarding is unix-only",
    ))
}

/// Decide ssh-agent forwarding for a new instance. Returns the `-v
/// <link>:<target>` mount to push and the container target to persist, or
/// `None` when forwarding is off. Gate (all silent on miss — an absent agent
/// is the common case): the runtime binds files, `$SSH_AUTH_SOCK` is set, and
/// its socket exists on the host. On a link error, warn and skip: forwarding
/// is best-effort and must not fail container creation (docs/ssh-agent.md).
fn ssh_agent_forward(instance_id: &str) -> Result<Option<(String, String)>> {
    if !cfg!(unix) || !backend().supports_file_binds() {
        return Ok(None);
    }
    let Some(sock) = std::env::var_os("SSH_AUTH_SOCK") else {
        return Ok(None);
    };
    let sock = PathBuf::from(sock);
    if std::fs::metadata(&sock).is_err() {
        return Ok(None);
    }
    let agent_dir = ssh_agent_dir().context("state path has no parent")?;
    match ssh_agent_link(&agent_dir, instance_id, &sock) {
        Ok(link) => Ok(Some((
            format!("{}:{SSH_AGENT_TARGET}", link.display()),
            SSH_AGENT_TARGET.to_string(),
        ))),
        Err(e) => {
            eprintln!("warning: ssh-agent forwarding disabled: {e:#}");
            Ok(None)
        }
    }
}

/// Re-point `info`'s agent symlink at the current host agent before its
/// container starts, so a restart after agent rotation (reboot, re-login)
/// re-captures the live socket — bind sources re-resolve at every container
/// start (docs/ssh-agent.md). Gate mirrors `ssh_agent_forward`: forwarding was
/// on for this instance, `$SSH_AUTH_SOCK` is set, and its socket exists.
/// Fully silent — every gate miss and error is a no-op (the link stays as-is
/// and forwarding is degraded for this run only): callers include the TUI
/// background thread that owns the alternate screen, where a stray warning
/// would corrupt the display, so screen safety wins over a lost warning.
pub(crate) fn ssh_agent_refresh(info: &Instance) {
    if !cfg!(unix) || info.ssh_auth_sock.is_none() {
        return;
    }
    let Some(sock) = std::env::var_os("SSH_AUTH_SOCK") else {
        return;
    };
    let sock = PathBuf::from(sock);
    if std::fs::metadata(&sock).is_err() {
        return;
    }
    let Some(agent_dir) = ssh_agent_dir() else {
        return;
    };
    let _ = ssh_agent_link(&agent_dir, &info.instance_id, &sock);
}

/// Create a git worktree on a fresh `branch`. Git refuses to check out a branch
/// already checked out elsewhere, so the branch must be unique per instance
/// (the default `sandbox/${instance}` pattern guarantees this).
///
/// The branch starts from `base_ref` when given, else the remote's default
/// branch (see [`worktree_start_point`]), not the base repo's HEAD: another
/// agent may be mid-work on a feature branch in the base checkout.
fn create_worktree(
    base: &Path,
    worktree: &Path,
    branch: &str,
    base_ref: Option<&str>,
) -> Result<()> {
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let start = worktree_start_point(base, base_ref)?;
    // With an unborn HEAD, `git worktree add -b` infers `--orphan`, exits 0, and
    // lays down a worktree with no files; guard against handing that empty path
    // to the container as a bind mount.
    let head = std::process::Command::new("git")
        .args([
            "-C",
            &base.to_string_lossy(),
            "rev-parse",
            "--verify",
            &format!("{start}^{{commit}}"),
        ])
        .output()
        .context("failed to run git (is it installed?)")?;
    if !head.status.success() {
        // An explicit ref is the user's choice: never silently swap it out.
        if let Some(r) = base_ref {
            bail!("worktree base `{r}` is not a commit in `{}`", base.display());
        }
        bail!(
            "base repo `{}` has no commits; cannot create a worktree",
            base.display()
        );
    }
    // A worktree branch must be unique: `git worktree add -b` refuses a branch
    // that already exists. Catch it here with a message that points at the
    // likely cause (a constant `worktree-branch`/`--branch` with no `${instance}`).
    let exists = std::process::Command::new("git")
        .args([
            "-C",
            &base.to_string_lossy(),
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .context("failed to run git (is it installed?)")?;
    if exists.success() {
        bail!(
            "branch `{branch}` already exists; a worktree needs a unique branch \
             (include `${{instance}}` in `worktree-branch` or pass a distinct `--branch`)"
        );
    }
    let status = std::process::Command::new("git")
        .args([
            "-C",
            &base.to_string_lossy(),
            "worktree",
            "add",
            &worktree.to_string_lossy(),
            "-b",
            branch,
            // Starting from `origin/<default>` would otherwise set it as the
            // upstream, aiming a bare `git push` at main.
            "--no-track",
            &start,
        ])
        .status()
        .context("failed to run git (is it installed?)")?;
    if !status.success() {
        bail!("git worktree add failed for `{}`", worktree.display());
    }
    // A real worktree always has a `.git` entry pointing back at the base repo;
    // its absence means git exited 0 without checking anything out.
    if !worktree.join(".git").exists() {
        bail!(
            "git worktree add produced an empty worktree at `{}`",
            worktree.display()
        );
    }
    Ok(())
}

/// Git's output for `git -C base <args>` on success, trimmed; None on failure.
fn git_query(base: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(base)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Skip window for [`fetch_origin`]: back-to-back `run`s (or an IDE's
/// auto-fetch) shouldn't each pay a network round-trip.
const FETCH_FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether a fetch that last wrote `FETCH_HEAD` at `modified` is recent enough
/// to skip another. A future mtime (clock skew) counts as fresh.
fn fetch_is_fresh(modified: std::time::SystemTime, now: std::time::SystemTime) -> bool {
    now.duration_since(modified).map_or(true, |age| age < FETCH_FRESH_FOR)
}

/// `git fetch origin` unless one ran within [`FETCH_FRESH_FOR`]. Git records no
/// fetch timestamp, but every fetch rewrites `FETCH_HEAD`, so its mtime stands
/// in (missing file = never fetched). `--git-path` resolves it for a base that
/// is itself a linked worktree.
fn fetch_origin(base: &Path) -> Result<()> {
    let fresh = git_query(base, &["rev-parse", "--git-path", "FETCH_HEAD"])
        .and_then(|p| std::fs::metadata(base.join(p)).ok())
        .and_then(|m| m.modified().ok())
        .is_some_and(|t| fetch_is_fresh(t, std::time::SystemTime::now()));
    if fresh {
        return Ok(());
    }
    let fetch = std::process::Command::new("git")
        .arg("-C")
        .arg(base)
        .args(["fetch", "--quiet", "origin"])
        .output()
        .context("failed to run git (is it installed?)")?;
    if !fetch.status.success() {
        eprintln!(
            "warning: `git fetch origin` failed in `{}`; worktree may start from a stale ref: {}",
            base.display(),
            String::from_utf8_lossy(&fetch.stderr).trim()
        );
    }
    Ok(())
}

/// Refresh `origin` and pick the ref a new worktree branch starts from.
///
/// `fetch` only moves `refs/remotes/origin/*` and adds objects — the base
/// checkout's HEAD, index and files are untouched, so it is safe while
/// another agent works there. It is skipped when one ran in the last two
/// minutes (see [`fetch_origin`]). Its output is captured (the TUI calls this
/// off-screen); a failed fetch (offline, auth) only warns and falls back to
/// the last-fetched refs.
///
/// Resolution: `explicit` (`--base`/`worktree-base`) → `origin/HEAD` →
/// `origin/main` → `origin/master` → `HEAD`. The fetch still runs for an
/// explicit ref so e.g. `origin/develop` is fresh too. `origin/HEAD` is unset
/// for repos that added the remote after `git init`; we don't `remote
/// set-head` since that writes to the base repo. Repos with no `origin` keep
/// the old start-from-HEAD behavior.
fn worktree_start_point(base: &Path, explicit: Option<&str>) -> Result<String> {
    let fallback = || explicit.unwrap_or("HEAD").to_string();
    if git_query(base, &["remote", "get-url", "origin"]).is_none() {
        return Ok(fallback());
    }
    fetch_origin(base)?;
    if let Some(r) = explicit {
        return Ok(r.to_string());
    }
    if let Some(r) = git_query(base, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]) {
        return Ok(r);
    }
    for r in ["origin/main", "origin/master"] {
        let full = format!("refs/remotes/{r}");
        if git_query(base, &["show-ref", "--verify", "--quiet", &full]).is_some() {
            return Ok(r.to_string());
        }
    }
    Ok("HEAD".to_string())
}

/// Warn when the reused container's recorded config or build hash differs from
/// the freshly resolved one (the shared [`container_drifted`] rule, so a
/// dockerfile edit warns too). Containers without the labels (None) — skip.
pub(crate) fn warn_on_drift(dir: &Path, container: &str, sandbox: &ResolvedSandbox) -> Result<()> {
    let build = build_hash(dir, sandbox.properties.build.as_ref());
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

fn run_container(
    dir: &Path,
    sandbox: &ResolvedSandbox,
    container: &str,
    source: &Path,
    base_folder: &Path,
    workspace: &str,
    extra_mounts: &[String],
    mounts: &[String],
    extra_env: &[(String, String)],
    networks: &[String],
    endpoints: &[ServiceEndpoint],
    privileged: bool,
) -> Result<()> {
    let props = &sandbox.properties;
    let image = image_for(dir, sandbox)?;
    let mount = format!("{}:{workspace}", source.display());
    let sandbox_label = format!("devsandbox.sandbox={}", sandbox.name);
    let instance = container.strip_prefix(NAME_PREFIX).unwrap_or(container);
    let instance_label = format!("devsandbox.instance={instance}");
    let hash_label = format!("devsandbox.config_hash={}", sandbox.config_hash);
    let build_hash_label = format!(
        "devsandbox.build_hash={}",
        build_hash(dir, sandbox.properties.build.as_ref())
    );
    let base_label = format!("devsandbox.base_folder={}", base_folder.display());
    let mut args: Vec<String> = [
        "run", "-d", "--name", container,
        "--label", &sandbox_label, "--label", &instance_label,
        "--label", &hash_label, "--label", &build_hash_label, "--label", &base_label,
        "-v", &mount, "-w", workspace,
    ]
    .map(str::to_string)
    .into();
    if props.init == Some(true) {
        args.push("--init".into());
    }
    if privileged {
        args.push("--privileged".into());
    }
    if let Some(user) = &props.container_user {
        args.push("--user".into());
        args.push(user.clone());
    }
    for mount in extra_mounts {
        args.push("-v".into());
        args.push(mount.clone());
    }
    for mount in mounts {
        args.push("--mount".into());
        args.push(mount.clone());
    }
    // Runtimes differ in how many networks a container can start on; the
    // backend attaches the rest after creation.
    args.extend(backend().network_run_args(networks));
    if let Some(env) = &props.container_env {
        for (key, value) in env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
    }
    for (key, value) in extra_env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    args.push(image);
    // Same trick as devcontainers: keep the container alive, work happens via exec.
    args.extend(["sleep".into(), "infinity".into()]);

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    backend().run_checked(&arg_refs)?;

    backend().connect_networks(container, networks)?;
    backend().wire_service_dns(container, endpoints)?;
    Ok(())
}

/// Resolve the sandbox's `mounts` into `docker run --mount` values, substituting
/// `${…}` variables and creating any missing bind sources so a first run doesn't
/// fail on a non-existent host path. Entries sharing a target collapse to the
/// last one (see [`last_wins_by_target`]) before any source is created, so an
/// overridden template mount leaves no stub behind.
fn resolve_mounts(
    dir: &Path,
    folder: &Path,
    basename: &str,
    instance_id: &str,
    sandbox: &ResolvedSandbox,
) -> Result<Vec<String>> {
    let Some(mounts) = &sandbox.properties.mounts else {
        return Ok(Vec::new());
    };
    // `${configDir}` anchors host-backed volumes; make it absolute.
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let shared_volumes = config_dir.join("shared-volumes");
    let ctx = MountContext {
        config_dir: &config_dir.to_string_lossy(),
        workspace_folder: &folder.to_string_lossy(),
        workspace_folder_basename: basename,
        shared_volumes: &shared_volumes.to_string_lossy(),
        instance: instance_id,
    };
    let resolved = mounts
        .iter()
        .map(|mount| mount.resolve(&ctx))
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("sandbox `{}`: invalid mount", sandbox.name))?;
    let resolved = last_wins_by_target(resolved);
    let mut args = Vec::with_capacity(resolved.len());
    for resolved in resolved {
        if resolved.kind == "bind" {
            if let Some(source) = &resolved.source {
                ensure_bind_source(Path::new(source))?;
            }
        }
        args.push(resolved.to_arg());
    }
    Ok(args)
}

/// Drop every mount whose target a later entry claims again, keeping the order
/// of the survivors. `extends` concatenates `mounts` base-first, so "later" is
/// the more downstream table: a sandbox (or a template further down the chain)
/// replaces an inherited mount by re-mounting its target, instead of the
/// runtime rejecting the pair as a duplicate mount point.
fn last_wins_by_target(mounts: Vec<ResolvedMount>) -> Vec<ResolvedMount> {
    let mut seen = BTreeSet::new();
    let mut kept: Vec<ResolvedMount> = mounts
        .into_iter()
        .rev()
        .filter(|m| seen.insert(m.target.clone()))
        .collect();
    kept.reverse();
    kept
}

/// Order `--mount` args parent-first (by target depth, then path), so a mount
/// nested inside another is applied after it and stays visible. docker sorts
/// this way itself (moby `container.SortMounts`), but that is undocumented and
/// not something to rely on for other runtimes (Apple `container`). Stable, so
/// equal targets keep their order; unparsable args sort as depth 0.
fn sort_parents_first(mounts: &mut [String]) {
    let key = |arg: &String| {
        let target = parse_shorthand(arg).map(|(_, _, t, _)| t).unwrap_or_default();
        let depth = target.split('/').filter(|c| !c.is_empty()).count();
        (depth, target)
    };
    mounts.sort_by_cached_key(key);
}

/// Resolve `folders` (container path -> host folder) into
/// `(container path, host path)` pairs. Host paths behave like `folder`:
/// relative to the config dir, must exist. Container paths must be absolute
/// and distinct from `workspaceFolder`, which is always the first root.
fn resolve_folders(
    dir: &Path,
    ctx: &MountContext,
    workspace: &str,
    props: &SandboxProperties,
) -> Result<Vec<(String, PathBuf)>> {
    let Some(folders) = &props.folders else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(folders.len());
    for (target, source) in folders {
        let target = substitute(target, ctx);
        if !target.starts_with('/') {
            bail!("`folders` key `{target}` must be an absolute container path");
        }
        if target == workspace {
            bail!(
                "`folders` key `{target}` duplicates `workspaceFolder`; \
                 the primary folder is added automatically"
            );
        }
        let source = substitute(source, ctx);
        let host = dir
            .join(&source)
            .canonicalize()
            .with_context(|| format!("`folders` entry `{source}` does not exist"))?;
        out.push((target, host));
    }
    Ok(out)
}

/// JSON body of the generated `.code-workspace`: the primary workspace folder
/// first, then the extra `folders` roots. `recommendations`, when non-empty, is
/// written as `extensions.recommendations` so VS Code offers to install them —
/// the fallback for backends where auto-install via `nameConfigs` is unavailable
/// (see [`write_workspace_file`]).
///
/// The primary folder is named after the instance and `terminal.integrated.cwd`
/// is pinned to it via `${workspaceFolder:<name>}`: in a multi-root workspace
/// VS Code otherwise picks the terminal cwd from whichever root holds the
/// active editor, which lands new terminals in the extra roots.
fn workspace_file_json(
    instance: &str,
    workspace: &str,
    extra_folders: &[(String, PathBuf)],
    recommendations: &[String],
) -> String {
    let folders: Vec<serde_json::Value> =
        std::iter::once(serde_json::json!({ "name": instance, "path": workspace }))
            .chain(
                extra_folders
                    .iter()
                    .map(|(target, _)| serde_json::json!({ "path": target })),
            )
            .collect();
    let mut root = serde_json::json!({
        "folders": folders,
        "settings": {
            "terminal.integrated.cwd": format!("${{workspaceFolder:{instance}}}"),
        },
    });
    if !recommendations.is_empty() {
        root["extensions"] = serde_json::json!({ "recommendations": recommendations });
    }
    serde_json::to_string_pretty(&root).expect("workspace json serializes")
}

/// Write `/workspaces/<instance>.code-workspace` inside the container so VS
/// Code opens the instance as a workspace named after it (the window title is
/// the file name; there is no separate name property). Best-effort: a container
/// without `sh` still runs, and `code` falls back to a folder open when no file
/// was recorded. Returns the container path on success.
fn write_workspace_file(
    container: &str,
    instance: &str,
    workspace: &str,
    extra_folders: &[(String, PathBuf)],
    extensions: &[String],
) -> Option<String> {
    let path = format!("/workspaces/{instance}.code-workspace");
    // The Remote-Containers `nameConfigs` file (see `write_vscode_name_config`)
    // installs extensions automatically only for Docker containers; on Apple
    // `container` it is ignored, so fall back to workspace recommendations.
    let recommendations: &[String] = if backend().name() == "container" {
        extensions
    } else {
        &[]
    };
    let json = workspace_file_json(instance, workspace, extra_folders, recommendations);
    let script = r#"mkdir -p "${2%/*}" && printf '%s\n' "$1" > "$2""#;
    match backend().run_checked(&["exec", container, "sh", "-c", script, "sh", &json, &path]) {
        Ok(()) => Some(path),
        Err(e) => {
            eprintln!("warning: cannot write {path} in `{container}`: {e:#}");
            None
        }
    }
}

/// Expand `caches` into (extra `--mount` args, extra `(key, value)` env vars).
/// Each cache is a shared bind mount under `${configDir}/shared-volumes/<name>`
/// (auto-created) plus the env vars that point the tool at its mount target.
fn resolve_caches(
    config_dir: &Path,
    props: &SandboxProperties,
) -> Result<(Vec<String>, Vec<(String, String)>)> {
    let mut mounts = Vec::new();
    let mut env = Vec::new();
    for name in props.caches.iter().flatten() {
        let (source_sub, target, vars) = crate::config::known_cache(name).with_context(|| {
            format!(
                "unknown cache `{name}`; supported: {}",
                crate::config::SUPPORTED_CACHES.join(", ")
            )
        })?;
        let source = config_dir.join("shared-volumes").join(source_sub);
        std::fs::create_dir_all(&source)
            .with_context(|| format!("cannot create {}", source.display()))?;
        mounts.push(format!("type=bind,source={},target={target}", source.display()));
        for var in vars {
            env.push((var.to_string(), target.to_string()));
        }
    }
    Ok((mounts, env))
}

/// Container mount point of the managed shell-history directory.
const HISTORY_TARGET: &str = "/commandhistory";

/// Provision this instance's managed shell history under
/// `${configDir}/shared-volumes/history/<instance_id>/.zsh_history` and return
/// (host file path, `--mount` arg, `HISTFILE` env pair). The *directory* is
/// bind-mounted (Apple's `container` cannot bind a single file) and zsh is
/// pointed at the file inside it via `HISTFILE`. An existing file is reused
/// untouched, so history survives rm + run rebuilds; keying by the persistent
/// id keeps it attached across renames too.
fn provision_shell_history(
    dir: &Path,
    instance_id: &str,
) -> Result<(PathBuf, String, (String, String))> {
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let history_dir = config_dir.join("shared-volumes").join("history").join(instance_id);
    std::fs::create_dir_all(&history_dir)
        .with_context(|| format!("cannot create {}", history_dir.display()))?;
    let path = history_dir.join(".zsh_history");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    let mount = format!("type=bind,source={},target={HISTORY_TARGET}", history_dir.display());
    let env = ("HISTFILE".to_string(), format!("{HISTORY_TARGET}/.zsh_history"));
    Ok((path, mount, env))
}

const SHELL_RC_TARGET: &str = "/devsandbox/rc";

/// Resolve `shell-rc` entries into `(--mount arg, container file path)` pairs.
/// Entry `i`'s *parent directory* is bind-mounted read-only at
/// `/devsandbox/rc/<i>` (Apple's `container` cannot bind a single file) and the
/// file is addressed by basename inside it. A missing host file is an error:
/// unlike `mounts`, auto-creating an empty snippet would only hide a typo.
fn resolve_shell_rc(
    dir: &Path,
    ctx: &MountContext,
    props: &SandboxProperties,
) -> Result<Vec<(String, String)>> {
    let Some(entries) = &props.shell_rc else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        let source = substitute(entry, ctx);
        let host = dir
            .join(&source)
            .canonicalize()
            .with_context(|| format!("`shell-rc` entry `{source}` does not exist"))?;
        if !host.is_file() {
            bail!("`shell-rc` entry `{source}` is not a file");
        }
        let parent = host.parent().context("shell-rc entry has no parent dir")?;
        let name = host
            .file_name()
            .context("shell-rc entry has no file name")?
            .to_string_lossy();
        let target = format!("{SHELL_RC_TARGET}/{i}");
        out.push((
            format!("type=bind,source={},target={target},readonly", parent.display()),
            format!("{target}/{name}"),
        ));
    }
    Ok(out)
}

/// Shell snippet that makes the remote user's interactive rc files source each
/// `shell-rc` path and, with `persist_history`, pin `HISTFILE` to the managed
/// location. The rc-level export exists because the container-env `HISTFILE`
/// alone is not enough: VS Code's injected shell integration resets `HISTFILE`
/// to `$HOME/.zsh_history` before sourcing `~/.zshrc`, so only a line inside
/// the rc file survives every shell startup path. Runs as `remoteUser`, so
/// `$HOME` is theirs rather than a hard-coded `/root`. Idempotent: each line is
/// appended only when the rc file doesn't mention it yet, and a missing rc file
/// is created. The source line is guarded with `[ -r … ]` so a shell still
/// starts if the mount is gone.
fn shell_rc_wiring(paths: &[&str], persist_history: bool) -> LifecycleCommand {
    let mut script = String::from("set -eu\nfor rc in \"$HOME/.zshrc\" \"$HOME/.bashrc\"; do\n");
    if persist_history {
        let histfile = format!("{HISTORY_TARGET}/.zsh_history");
        script.push_str(&format!(
            "  grep -qsF 'HISTFILE={histfile}' \"$rc\" || printf '\\nexport HISTFILE={histfile}\\n' >> \"$rc\"\n"
        ));
    }
    for path in paths {
        script.push_str(&format!(
            "  grep -qsF '{path}' \"$rc\" || printf '\\n[ -r {path} ] && . {path}\\n' >> \"$rc\"\n"
        ));
    }
    script.push_str("done\n");
    LifecycleCommand::Simple(SimpleCommand::Shell(script))
}

/// What the sandbox's enabled features ask of the container itself (the image
/// side lives in [`build_features_image`]).
struct FeatureContainerOpts {
    /// `--mount` args.
    mounts: Vec<String>,
    /// Refs of the enabled features declaring `privileged: true`; the backend
    /// check happens in [`privileged_decision`].
    privileged_by: Vec<String>,
}

/// Whether to pass `--privileged`, plus the warnings to print. Requested by the
/// sandbox's `privileged = true` or any enabled feature's `privileged: true`;
/// on a backend without the flag every request is skipped with a warning, like
/// an unsatisfiable feature mount.
fn privileged_decision(sandbox: bool, features: &[String], supported: bool) -> (bool, Vec<String>) {
    let requested = sandbox || !features.is_empty();
    if supported || !requested {
        return (requested, Vec::new());
    }
    let skip = "this runtime cannot run privileged containers; skipping `privileged`";
    let mut warnings: Vec<String> =
        features.iter().map(|r| format!("feature `{r}`: {skip}")).collect();
    if sandbox {
        warnings.insert(0, skip.to_string());
    }
    (false, warnings)
}

/// Resolve the container-side settings declared by the sandbox's enabled
/// features (fetches hit the per-user cache also used by the image build).
///
/// Mounts are turned into `--mount` args. The
/// sandbox always wins: a feature mount whose target is already claimed is
/// dropped, so the config can replace e.g. docker-outside-of-docker's hardcoded
/// `/var/run/docker.sock` source with a rootless socket path. Mounts the
/// backend cannot apply, or whose bind source is missing on the host, are
/// skipped with a warning — auto-creating the source (the config-mount
/// behavior) would hand the container an empty stub instead of a clear signal.
fn feature_container_opts(
    props: &SandboxProperties,
    ctx: &MountContext,
    existing: &[String],
    workspace: &str,
) -> Result<FeatureContainerOpts> {
    let mut opts = FeatureContainerOpts {
        mounts: Vec::new(),
        privileged_by: Vec::new(),
    };
    let Some(features) = &props.features else {
        return Ok(opts);
    };
    let mut taken: BTreeSet<String> = existing
        .iter()
        .filter_map(|arg| parse_shorthand(arg).ok().map(|(_, _, target, _)| target))
        .collect();
    taken.insert(workspace.to_string());
    for (reference, options) in features {
        if feature_option_values(options)
            .with_context(|| format!("feature `{reference}`: invalid options"))?
            .is_none()
        {
            continue; // `= false`: disabled.
        }
        let parsed = features::FeatureRef::parse(reference)?;
        let feature =
            features::fetch(&parsed).with_context(|| format!("fetching feature `{reference}`"))?;
        if feature.metadata.privileged == Some(true) {
            opts.privileged_by.push(reference.clone());
        }
        for mount in &feature.metadata.mounts {
            let resolved = mount.resolve(ctx)?;
            let probe = |source: &str| std::fs::metadata(source).ok().map(|m| !m.is_dir());
            match feature_mount_decision(
                &resolved,
                &taken,
                backend().supports_file_binds(),
                probe,
            ) {
                MountDecision::Apply => {
                    taken.insert(resolved.target.clone());
                    opts.mounts.push(resolved.to_arg());
                }
                MountDecision::Overridden => {}
                MountDecision::Skip(reason) => {
                    eprintln!("warning: feature `{reference}`: {reason}");
                }
            }
        }
    }
    Ok(opts)
}

/// Outcome for one feature-declared mount.
#[derive(Debug, PartialEq)]
enum MountDecision {
    Apply,
    /// Target already claimed by the sandbox (config wins, silently).
    Overridden,
    /// Not applicable here; skipped with a printed warning.
    Skip(String),
}

/// Pure applicability check for a feature mount: `taken` holds the targets
/// already mounted, `file_binds` whether the backend can bind single files, and
/// `source_probe` reports a bind source as `Some(is_file_like)` (`!is_dir`, so
/// sockets count as files) or `None` when it does not exist on the host.
fn feature_mount_decision(
    mount: &ResolvedMount,
    taken: &BTreeSet<String>,
    file_binds: bool,
    source_probe: impl Fn(&str) -> Option<bool>,
) -> MountDecision {
    if taken.contains(&mount.target) {
        return MountDecision::Overridden;
    }
    if mount.kind != "bind" {
        return MountDecision::Apply;
    }
    // `Mount::resolve` already rejects sourceless bind mounts.
    let Some(source) = &mount.source else {
        return MountDecision::Skip(format!("bind mount to `{}` has no source", mount.target));
    };
    match source_probe(source) {
        None => MountDecision::Skip(format!(
            "mount source `{source}` does not exist on the host; skipping"
        )),
        Some(true) if !file_binds => MountDecision::Skip(format!(
            "cannot bind file `{source}` on this runtime; skipping"
        )),
        _ => MountDecision::Apply,
    }
}

/// Create a missing bind-mount source. A final path component containing a dot
/// (other than a leading one) is treated as a file (touched); anything else as a
/// directory — so `credentials.json` becomes a file and `pnpm-store` a dir.
fn ensure_bind_source(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let is_file = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.trim_start_matches('.').contains('.'))
        .unwrap_or(false);
    if is_file {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
    } else {
        std::fs::create_dir_all(path).with_context(|| format!("cannot create {}", path.display()))?;
    }
    Ok(())
}

/// Image to run: the `image` property as-is, or a local build for
/// `build.dockerfile` sandboxes. When the sandbox declares `features`, the base
/// (image or build) is extended into a derived image tagged
/// `devsandbox-img-<sandbox>-feat`.
fn image_for(dir: &Path, sandbox: &ResolvedSandbox) -> Result<String> {
    let base = base_image_for(dir, sandbox)?;
    let props = &sandbox.properties;
    match &props.features {
        Some(features) if !features.is_empty() => build_features_image(sandbox, &base, features),
        _ => Ok(base),
    }
}

/// The base image: the `image` property as-is, or a local build for
/// `build.dockerfile` sandboxes.
fn base_image_for(dir: &Path, sandbox: &ResolvedSandbox) -> Result<String> {
    let props = &sandbox.properties;
    if let Some(image) = &props.image {
        return Ok(image.clone());
    }
    let build = props
        .build
        .as_ref()
        .with_context(|| format!("sandbox `{}` has neither `image` nor `build`", sandbox.name))?;
    let dockerfile = build
        .dockerfile
        .as_deref()
        .with_context(|| format!("sandbox `{}`: `build.dockerfile` is required", sandbox.name))?;
    let tag = format!("{NAME_PREFIX}img-{}", sandbox.name);
    let args = services::build_args(
        backend(),
        dir,
        &tag,
        dockerfile,
        build,
        &format!("sandbox `{}`", sandbox.name),
    );
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    backend().run_checked(&arg_refs)?;
    Ok(tag)
}

// ------------------------------------------------------------------------
// Derived-image build from devcontainer `features`.
//
// The impure orchestration (`build_features_image`) fetches + orders the
// features, assembles a build context, and shells out to the runtime. The
// generation of the Dockerfile, env-files, and install wrapper is factored into
// pure functions (below) so they're unit-testable without docker or network.
// The layout mirrors the official devcontainers CLI
// (`containerFeaturesConfiguration.ts`, `containerFeatures.ts`).
// ------------------------------------------------------------------------

/// A feature ready to be baked in: its build-context directory name, its id,
/// metadata, and the resolved option map (defaults merged under user values).
struct FeatureBuild {
    /// `<idx>-<id>`, the per-feature directory name in the build context.
    dir_name: String,
    metadata: FeatureMetadata,
}

/// Fetch + order the sandbox's features, assemble a build context under the
/// features cache, and build the derived image. Returns its tag.
fn build_features_image(
    sandbox: &ResolvedSandbox,
    base: &str,
    features: &BTreeMap<String, FeatureOptions>,
) -> Result<String> {
    let props = &sandbox.properties;

    // Resolve each config entry to a fetched feature + its option map, skipping
    // features explicitly disabled with `= false`.
    let mut resolved = Vec::new();
    let mut options_by_key: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (reference, opts) in features {
        let Some(values) = feature_option_values(opts)
            .with_context(|| format!("feature `{reference}`: invalid options"))?
        else {
            continue; // `= false`: skip entirely.
        };
        let parsed = features::FeatureRef::parse(reference)?;
        let feature =
            features::fetch(&parsed).with_context(|| format!("fetching feature `{reference}`"))?;
        options_by_key.insert(feature.reference.full(), values);
        resolved.push(feature);
    }
    if resolved.is_empty() {
        return Ok(base.to_string()); // all features disabled.
    }

    let ordered = features::install_order(resolved)?;

    let container_user = props.container_user.as_deref().unwrap_or("root");
    let remote_user = props.remote_user.as_deref().unwrap_or(container_user);

    // Fresh build context.
    let context = features::build_context_dir(&sandbox.name)?;
    std::fs::write(
        context.join("devcontainer-features.builtin.env"),
        builtin_env(container_user, remote_user),
    )
    .with_context(|| format!("cannot write builtin env in {}", context.display()))?;

    // Per feature: copy its cached dir in under `<idx>-<id>`, write the env-file
    // and install wrapper, and collect what the Dockerfile generator needs.
    let mut builds = Vec::with_capacity(ordered.len());
    for (idx, feature) in ordered.iter().enumerate() {
        let id = feature
            .metadata
            .id
            .clone()
            .unwrap_or_else(|| feature.reference.id.clone());
        let dir_name = format!("{idx}-{id}");
        let dest = context.join(&dir_name);
        copy_dir(&feature.dir, &dest).with_context(|| {
            format!("copying feature `{}` into build context", feature.reference.full())
        })?;

        let values = options_by_key.remove(&feature.reference.full()).unwrap_or_default();
        let env_file = feature_env_file(&feature.metadata, &values);
        std::fs::write(dest.join("devcontainer-features.env"), &env_file)
            .with_context(|| format!("cannot write env file for `{}`", feature.reference.full()))?;
        let wrapper =
            install_wrapper(&feature.metadata, &feature.reference.without_tag(), &env_file);
        std::fs::write(dest.join("devcontainer-features-install.sh"), wrapper).with_context(
            || format!("cannot write install wrapper for `{}`", feature.reference.full()),
        )?;

        builds.push(FeatureBuild {
            dir_name,
            metadata: feature.metadata.clone(),
        });
    }

    let dockerfile =
        generate_dockerfile(base, &builds, props.container_user.as_deref(), remote_user);
    let dockerfile_path = context.join("Dockerfile.devsandbox-features");
    std::fs::write(&dockerfile_path, dockerfile)
        .with_context(|| format!("cannot write {}", dockerfile_path.display()))?;

    let tag = format!("{NAME_PREFIX}img-{}-feat", sandbox.name);
    backend().run_checked(&[
        "build",
        "-t",
        &tag,
        "-f",
        &dockerfile_path.to_string_lossy(),
        &context.to_string_lossy(),
    ])?;
    Ok(tag)
}

/// Turn a config `FeatureOptions` into a stringified option map, or `None` when
/// the feature is disabled (`= false`). `Options` tables reject array/table
/// values with a clear error; scalars stringify (strings as-is, bools
/// `true`/`false`, ints/floats via their `Display`).
fn feature_option_values(opts: &FeatureOptions) -> Result<Option<BTreeMap<String, String>>> {
    match opts {
        FeatureOptions::Enabled(false) => Ok(None),
        FeatureOptions::Enabled(true) => Ok(Some(BTreeMap::new())),
        FeatureOptions::Version(v) => {
            Ok(Some(BTreeMap::from([("version".to_string(), v.clone())])))
        }
        FeatureOptions::Options(table) => {
            let mut out = BTreeMap::new();
            for (key, value) in table {
                out.insert(key.clone(), toml_option_value(key, value)?);
            }
            Ok(Some(out))
        }
    }
}

/// Stringify a scalar TOML option value. Arrays and tables are rejected: feature
/// options are flat scalars.
fn toml_option_value(key: &str, value: &toml::Value) -> Result<String> {
    match value {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Boolean(b) => Ok(b.to_string()),
        toml::Value::Integer(i) => Ok(i.to_string()),
        toml::Value::Float(f) => Ok(f.to_string()),
        toml::Value::Datetime(d) => Ok(d.to_string()),
        toml::Value::Array(_) | toml::Value::Table(_) => {
            bail!("option `{key}` must be a string, bool, or number, not an array or table")
        }
    }
}

/// The `devcontainer-features.builtin.env` body: `_CONTAINER_USER` and
/// `_REMOTE_USER`. The `_*_HOME` lines are appended in-container by the Dockerfile.
fn builtin_env(container_user: &str, remote_user: &str) -> String {
    format!("_CONTAINER_USER={container_user}\n_REMOTE_USER={remote_user}\n")
}

/// A feature's option env-name, mirroring the CLI's `getSafeId`: non-word chars
/// (outside `[A-Za-z0-9_]`) become `_`, then any leading run of digits and
/// underscores collapses to a single `_`, then the whole thing is uppercased.
fn safe_id(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let trimmed = mapped.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');
    let out = if trimmed.len() == mapped.len() {
        mapped
    } else {
        format!("_{trimmed}")
    };
    out.to_uppercase()
}

/// Escape a value for a double-quoted context: backslash and double-quote only.
/// A literal dollar sign is left intact so a value like /usr/local/go/bin:${PATH}
/// still expands at build time when emitted as an ENV instruction.
fn escape_dq(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The `devcontainer-features.env` body: one `NAME="value"` per option, names via
/// [`safe_id`], values = metadata defaults overridden by user-provided values.
fn feature_env_file(metadata: &FeatureMetadata, values: &BTreeMap<String, String>) -> String {
    // Start from metadata defaults, override with user values. BTreeMap keeps a
    // stable (alphabetical) order.
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for (name, option) in &metadata.options {
        if let Some(default) = &option.default {
            merged.insert(name.clone(), json_scalar(default));
        }
    }
    for (name, value) in values {
        merged.insert(name.clone(), value.clone());
    }
    merged
        .iter()
        .map(|(name, value)| format!("{}=\"{}\"", safe_id(name), escape_dq(value)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Stringify a JSON scalar option default: strings as-is, everything else via its
/// JSON rendering (bool becomes `true`/`false`, numbers become their text).
fn json_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Single-quote escape for a value printed inside `'...'` in the wrapper banner,
/// mirroring the CLI's `escapeQuotesForShell`: each `'` becomes `'\''`.
fn escape_sq(value: &str) -> String {
    value.replace('\'', "'\\''")
}

/// The per-feature `devcontainer-features-install.sh` wrapper: a banner echoing
/// the feature identity + options, then `set -a` sourcing of the builtin and
/// feature env-files, then `./install.sh`. `env_file` is the already-generated
/// `devcontainer-features.env` body, echoed (indented) in the banner.
fn install_wrapper(metadata: &FeatureMetadata, id: &str, env_file: &str) -> String {
    let name = metadata.name.as_deref().unwrap_or("Unknown");
    let version = metadata.version.as_deref().unwrap_or("");
    let documentation = metadata.documentation_url.as_deref().unwrap_or("");
    let options_indented = env_file
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "#!/bin/sh\n\
set -e\n\
\n\
echo ===========================================================================\n\
echo 'Feature       : {name}'\n\
echo 'Id            : {id}'\n\
echo 'Version       : {version}'\n\
echo 'Documentation : {documentation}'\n\
echo 'Options       :'\n\
echo '{options}'\n\
echo ===========================================================================\n\
\n\
set -a\n\
. ../devcontainer-features.builtin.env\n\
. ./devcontainer-features.env\n\
set +a\n\
\n\
chmod +x ./install.sh\n\
./install.sh\n",
        name = escape_sq(name),
        id = escape_sq(id),
        version = escape_sq(version),
        documentation = escape_sq(documentation),
        options = escape_sq(&options_indented),
    )
}

/// Generate the derived-image Dockerfile. `container_user` is `Some` only when
/// the sandbox set `containerUser`; when `None` the final stage stays `root`.
/// `remote_user` is the effective remote user (used for the `_*_HOME` probe).
fn generate_dockerfile(
    base: &str,
    features: &[FeatureBuild],
    container_user: Option<&str>,
    remote_user: &str,
) -> String {
    let effective_container_user = container_user.unwrap_or("root");
    let mut out = String::new();
    out.push_str(&format!("FROM {base}\n"));
    out.push_str("USER root\n");
    out.push_str("RUN mkdir -p /tmp/dev-container-features\n");
    out.push_str("COPY devcontainer-features.builtin.env /tmp/dev-container-features/\n");
    // Append the user home dirs to the builtin env, resolved in-container.
    out.push_str(&format!(
        "RUN echo \"_CONTAINER_USER_HOME=$(getent passwd {cu} | cut -d: -f6)\" \
>> /tmp/dev-container-features/devcontainer-features.builtin.env && \
echo \"_REMOTE_USER_HOME=$(getent passwd {ru} | cut -d: -f6)\" \
>> /tmp/dev-container-features/devcontainer-features.builtin.env\n",
        cu = effective_container_user,
        ru = remote_user,
    ));
    for feature in features {
        for (key, value) in &feature.metadata.container_env {
            out.push_str(&format!("ENV {key}=\"{}\"\n", escape_dq(value)));
        }
        let dir = &feature.dir_name;
        out.push_str(&format!("COPY {dir} /tmp/dev-container-features/{dir}\n"));
        out.push_str(&format!(
            "RUN chmod -R 0755 /tmp/dev-container-features/{dir} && \
cd /tmp/dev-container-features/{dir} && \
chmod +x ./devcontainer-features-install.sh && \
./devcontainer-features-install.sh\n",
        ));
    }
    if let Some(user) = container_user {
        out.push_str(&format!("USER {user}\n"));
    }
    out
}

/// Recursively copy `src` into `dst` (created if missing), using plain `std::fs`
/// (no external deps). Files are copied; nested dirs recursed.
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("cannot create {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("cannot read {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("cannot copy {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Run a lifecycle command's argv lists on the host (initializeCommand).
fn run_host_commands(dir: &Path, cmd: &LifecycleCommand) -> Result<()> {
    for argv in cmd.commands() {
        let Some((program, rest)) = argv.split_first() else { continue };
        let status = std::process::Command::new(program)
            .args(rest)
            .current_dir(dir)
            .status()
            .with_context(|| format!("cannot run `{program}`"))?;
        if !status.success() {
            bail!("`{}` exited with status {}", argv.join(" "), status.code().unwrap_or(1));
        }
    }
    Ok(())
}

/// Run a lifecycle command inside the container via `exec`.
pub(crate) fn exec_lifecycle(
    container: &str,
    workspace: &str,
    remote_env: Option<&BTreeMap<String, String>>,
    remote_user: Option<&str>,
    ssh_auth_sock: Option<&str>,
    cmd: &LifecycleCommand,
) -> Result<()> {
    for argv in cmd.commands() {
        if argv.is_empty() {
            continue;
        }
        let args = lifecycle_argv(container, workspace, remote_env, remote_user, ssh_auth_sock, argv);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        backend().run_checked(&arg_refs)?;
    }
    Ok(())
}

/// Pure argv builder for one lifecycle exec, mirroring [`exec_argv_with`]'s
/// flag order (`-w`, `-u`, remote_env, `SSH_AUTH_SOCK`, container, command) so
/// lifecycle execs carry the same agent env the CLI/TUI do. `ssh_auth_sock` is
/// the shared rule's result (`commands::exec::ssh_auth_sock_env`).
fn lifecycle_argv(
    container: &str,
    workspace: &str,
    remote_env: Option<&BTreeMap<String, String>>,
    remote_user: Option<&str>,
    ssh_auth_sock: Option<&str>,
    argv: Vec<String>,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into(), "-w".into(), workspace.into()];
    if let Some(user) = remote_user {
        args.push("-u".into());
        args.push(user.to_string());
    }
    for (key, value) in remote_env.into_iter().flatten() {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    if let Some(sock) = ssh_auth_sock {
        args.push("-e".into());
        args.push(format!("SSH_AUTH_SOCK={sock}"));
    }
    args.push(container.into());
    args.extend(argv);
    args
}

/// Register `customizations.vscode.extensions` and `remoteUser` with the
/// Remote-Containers extension by writing its per-container-name config file.
/// Without `remoteUser`, VS Code attaches as the editor's default user and
/// hits EACCES on root-owned files (e.g. rootless docker).
pub(crate) fn write_vscode_name_config(
    container: &str,
    extensions: &[String],
    remote_user: Option<&str>,
) -> Result<()> {
    let base = editor_config_base()?;
    // Every installed VS Code-family product keys nameConfigs by container name.
    for product in ["Code", "Cursor", "VSCodium"] {
        let product_dir = base.join(product);
        if !product_dir.is_dir() {
            continue; // not installed
        }
        let dir =
            product_dir.join("User/globalStorage/ms-vscode-remote.remote-containers/nameConfigs");
        std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!("{container}.json"));
        let existing = match std::fs::read_to_string(&path) {
            Ok(contents) => Some(contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let contents = merged_name_config(existing.as_deref(), extensions, remote_user)
            .with_context(|| format!("in {}", path.display()))?;
        std::fs::write(&path, contents)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(())
}

/// Per-OS base dir that holds each editor product's config directory:
/// `~/Library/Application Support` on macOS, `$XDG_CONFIG_HOME`/`~/.config`
/// elsewhere.
fn editor_config_base() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        return std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support"))
            .context("cannot determine config dir ($HOME)");
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("cannot determine config dir ($XDG_CONFIG_HOME or $HOME)")
}

/// Existing name-config JSON (if any) with `extensions` and `remoteUser`
/// replaced. `remoteUser: None` removes the key so the file tracks the config.
fn merged_name_config(
    existing: Option<&str>,
    extensions: &[String],
    remote_user: Option<&str>,
) -> Result<String> {
    let mut root = match existing {
        Some(contents) => {
            serde_json::from_str::<serde_json::Value>(contents).context("invalid JSON")?
        }
        None => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .context("existing config is not a JSON object")?;
    obj.insert("extensions".into(), serde_json::json!(extensions));
    match remote_user {
        Some(user) => {
            obj.insert("remoteUser".into(), serde_json::json!(user));
        }
        None => {
            obj.remove("remoteUser");
        }
    }
    Ok(serde_json::to_string_pretty(&root)?)
}

fn offer_example_config(dir: &Path) -> Result<()> {
    let path = dir.join(CONFIG_FILE);
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
    if path.exists() {
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

    #[test]
    fn name_config_from_scratch() {
        let json = merged_name_config(None, &["a.b".into(), "c.d".into()], None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["extensions"], serde_json::json!(["a.b", "c.d"]));
        assert!(parsed.get("remoteUser").is_none());
    }

    #[test]
    fn name_config_preserves_other_keys() {
        let existing = r#"{"settings": {"x": 1}, "extensions": ["old.ext"]}"#;
        let json = merged_name_config(Some(existing), &["new.ext".into()], None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["extensions"], serde_json::json!(["new.ext"]));
        assert_eq!(parsed["settings"]["x"], 1);
    }

    #[test]
    fn name_config_sets_and_clears_remote_user() {
        let json = merged_name_config(None, &[], Some("root")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["remoteUser"], "root");

        // remote_user gone from config → key removed from an existing file.
        let json = merged_name_config(Some(&json), &[], None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("remoteUser").is_none());
    }

    fn instance(name: &str) -> Instance {
        Instance {
            sandbox: "repo".into(),
            instance_id: name.into(),
            project: "proj1234".into(),
            container: format!("{NAME_PREFIX}{name}"),
            folder: "/tmp/repo".into(),
            base_folder: "/tmp/repo".into(),
            worktree: None,
            branch: None,
            shell_history: None,
            workspace: "/workspaces/repo".into(),
            workspace_file: None,
            remote_env: Default::default(),
            remote_user: None,
            ssh_auth_sock: None,
            devsbd_arch: None,
            created_unix: 0,
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

    fn ctx() -> MountContext<'static> {
        MountContext {
            config_dir: "/cfg",
            workspace_folder: "/host/app",
            workspace_folder_basename: "app",
            shared_volumes: "/cfg/shared-volumes",
            instance: "app",
        }
    }

    fn props_with_folders(entries: &[(&str, &str)]) -> SandboxProperties {
        SandboxProperties {
            folders: Some(
                entries
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn folders_resolve_relative_to_config_dir() {
        let dir = std::env::temp_dir();
        let props = props_with_folders(&[("/workspaces/.shared", ".")]);
        let resolved = resolve_folders(&dir, &ctx(), "/workspaces/app", &props).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "/workspaces/.shared");
        assert_eq!(resolved[0].1, dir.canonicalize().unwrap());
    }

    // --- shell-rc ---

    #[test]
    fn shell_rc_mounts_parent_dir_readonly_and_addresses_file() {
        let dir = std::env::temp_dir().join(format!("devsandbox-rc-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("shell")).unwrap();
        std::fs::write(dir.join("shell/aliases.sh"), "alias ll='ls -l'\n").unwrap();
        let props = SandboxProperties {
            shell_rc: Some(vec!["./shell/aliases.sh".into()]),
            ..Default::default()
        };
        let resolved = resolve_shell_rc(&dir, &ctx(), &props).unwrap();
        let parent = dir.join("shell").canonicalize().unwrap();
        assert_eq!(
            resolved,
            vec![(
                format!("type=bind,source={},target=/devsandbox/rc/0,readonly", parent.display()),
                "/devsandbox/rc/0/aliases.sh".to_string(),
            )]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shell_rc_missing_file_is_an_error() {
        let props = SandboxProperties {
            shell_rc: Some(vec!["./nope.sh".into()]),
            ..Default::default()
        };
        let err = resolve_shell_rc(&std::env::temp_dir(), &ctx(), &props).unwrap_err();
        assert!(err.to_string().contains("`shell-rc` entry `./nope.sh` does not exist"));
    }

    #[test]
    fn shell_rc_directory_is_rejected() {
        let props = SandboxProperties {
            shell_rc: Some(vec![".".into()]),
            ..Default::default()
        };
        let err = resolve_shell_rc(&std::env::temp_dir(), &ctx(), &props).unwrap_err();
        assert!(err.to_string().contains("is not a file"));
    }

    #[test]
    fn shell_rc_wiring_is_guarded_and_idempotent_per_path() {
        let cmd = shell_rc_wiring(&["/devsandbox/rc/0/a.sh", "/devsandbox/rc/1/b.sh"], false);
        let argv = &cmd.commands()[0];
        assert_eq!(&argv[..2], &["sh".to_string(), "-c".to_string()]);
        let script = &argv[2];
        assert!(script.contains(r#"for rc in "$HOME/.zshrc" "$HOME/.bashrc""#));
        assert!(script.contains(
            r#"grep -qsF '/devsandbox/rc/0/a.sh' "$rc" || printf '\n[ -r /devsandbox/rc/0/a.sh ] && . /devsandbox/rc/0/a.sh\n' >> "$rc""#
        ));
        assert!(script.contains("grep -qsF '/devsandbox/rc/1/b.sh'"));
        assert!(!script.contains("HISTFILE"));
    }

    #[test]
    fn lifecycle_argv_flag_order_and_env() {
        let mut env = BTreeMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        env.insert("BAZ".to_string(), "qux".to_string());
        let argv = lifecycle_argv(
            "devsandbox-repo-abc1",
            "/workspaces/repository-1",
            Some(&env),
            Some("vscode"),
            Some("/run/devsandbox/ssh-agent.sock"),
            vec!["sh".into(), "-c".into(), "echo hi".into()],
        );
        assert_eq!(
            argv,
            vec![
                "exec",
                "-w",
                "/workspaces/repository-1",
                "-u",
                "vscode",
                // BTreeMap iterates sorted: BAZ before FOO,
                "-e",
                "BAZ=qux",
                "-e",
                "FOO=bar",
                // then SSH_AUTH_SOCK, before the container name.
                "-e",
                "SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock",
                "devsandbox-repo-abc1",
                "sh",
                "-c",
                "echo hi",
            ]
        );
    }

    #[test]
    fn lifecycle_argv_omits_sock_when_none() {
        let argv = lifecycle_argv(
            "devsandbox-repo-abc1",
            "/workspaces/repository-1",
            None,
            None,
            None,
            vec!["true".into()],
        );
        assert_eq!(
            argv,
            vec!["exec", "-w", "/workspaces/repository-1", "devsandbox-repo-abc1", "true"]
        );
    }

    fn feature_mount(kind: &str, source: Option<&str>, target: &str) -> ResolvedMount {
        ResolvedMount {
            kind: kind.into(),
            source: source.map(Into::into),
            target: target.into(),
            readonly: false,
        }
    }

    #[test]
    fn last_wins_by_target_keeps_downstream_mount() {
        let mounts = vec![
            feature_mount("bind", Some("/tpl/gh"), "/root/.config/gh"),
            feature_mount("bind", Some("/tpl/bin"), "/usr/local/bin/tool"),
            feature_mount("bind", Some("/sandbox/gh"), "/root/.config/gh"),
        ];
        let kept = last_wins_by_target(mounts);
        assert_eq!(
            kept,
            vec![
                feature_mount("bind", Some("/tpl/bin"), "/usr/local/bin/tool"),
                feature_mount("bind", Some("/sandbox/gh"), "/root/.config/gh"),
            ]
        );
    }

    #[test]
    fn sort_parents_first_orders_by_depth_then_path() {
        let mut mounts = vec![
            "type=bind,source=/h/creds.json,target=/root/.zidane/credentials.json".to_string(),
            "type=bind,source=/s/zidane,target=/root/.zidane".to_string(),
            "type=bind,source=/s/b,target=/b".to_string(),
            "type=bind,source=/s/a,target=/a/x".to_string(),
        ];
        sort_parents_first(&mut mounts);
        let targets: Vec<_> = mounts
            .iter()
            .map(|m| parse_shorthand(m).unwrap().2)
            .collect();
        assert_eq!(
            targets,
            ["/b", "/a/x", "/root/.zidane", "/root/.zidane/credentials.json"]
        );
    }

    #[test]
    fn last_wins_by_target_keeps_nested_targets() {
        // Layering inside an inherited mount is not an override.
        let mounts = vec![
            feature_mount("bind", Some("/s/zidane"), "/root/.zidane"),
            feature_mount("bind", Some("/h/creds.json"), "/root/.zidane/credentials.json"),
        ];
        assert_eq!(last_wins_by_target(mounts.clone()), mounts);
    }

    #[test]
    fn privileged_decision_ors_sandbox_and_features() {
        let dind = ["ghcr.io/devcontainers/features/docker-in-docker:2".to_string()];
        assert_eq!(privileged_decision(false, &[], true), (false, vec![]));
        assert_eq!(privileged_decision(true, &[], true), (true, vec![]));
        assert_eq!(privileged_decision(false, &dind, true), (true, vec![]));
        assert_eq!(privileged_decision(true, &dind, true), (true, vec![]));
    }

    #[test]
    fn privileged_decision_unsupported_warns_per_request() {
        // Apple `container` has no `--privileged`.
        let dind = ["ghcr.io/devcontainers/features/docker-in-docker:2".to_string()];
        assert_eq!(privileged_decision(false, &[], false), (false, vec![]));
        let (on, warnings) = privileged_decision(true, &dind, false);
        assert!(!on);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].starts_with("this runtime cannot"), "{warnings:?}");
        assert!(warnings[1].contains("docker-in-docker"), "{warnings:?}");
    }

    #[test]
    fn feature_mount_decision_config_target_wins_silently() {
        let taken = BTreeSet::from(["/var/run/docker-host.sock".to_string()]);
        let m = feature_mount("bind", Some("/var/run/docker.sock"), "/var/run/docker-host.sock");
        assert_eq!(
            feature_mount_decision(&m, &taken, true, |_| Some(true)),
            MountDecision::Overridden
        );
    }

    #[test]
    fn feature_mount_decision_missing_source_warns() {
        let m = feature_mount("bind", Some("/var/run/docker.sock"), "/var/run/docker-host.sock");
        match feature_mount_decision(&m, &BTreeSet::new(), true, |_| None) {
            MountDecision::Skip(reason) => assert!(reason.contains("does not exist"), "{reason}"),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn feature_mount_decision_file_bind_unsupported_warns() {
        // Apple `container` cannot bind single files (sockets included).
        let m = feature_mount("bind", Some("/var/run/docker.sock"), "/var/run/docker-host.sock");
        match feature_mount_decision(&m, &BTreeSet::new(), false, |_| Some(true)) {
            MountDecision::Skip(reason) => assert!(reason.contains("cannot bind file"), "{reason}"),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn feature_mount_decision_applies_dirs_and_volumes() {
        let dir = feature_mount("bind", Some("/opt/data"), "/data");
        assert_eq!(
            feature_mount_decision(&dir, &BTreeSet::new(), false, |_| Some(false)),
            MountDecision::Apply
        );
        // Non-bind mounts have no host source to probe.
        let vol = feature_mount("volume", Some("dind-var-lib-docker"), "/var/lib/docker");
        assert_eq!(
            feature_mount_decision(&vol, &BTreeSet::new(), false, |_| None),
            MountDecision::Apply
        );
    }

    #[test]
    fn shell_rc_wiring_pins_histfile_when_history_persisted() {
        let cmd = shell_rc_wiring(&[], true);
        let script = &cmd.commands()[0][2];
        assert!(script.contains(
            r#"grep -qsF 'HISTFILE=/commandhistory/.zsh_history' "$rc" || printf '\nexport HISTFILE=/commandhistory/.zsh_history\n' >> "$rc""#
        ), "{script}");
    }

    #[test]
    fn folders_reject_relative_container_path() {
        let props = props_with_folders(&[("workspaces/x", ".")]);
        let err = resolve_folders(Path::new("/tmp"), &ctx(), "/workspaces/app", &props)
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute container path"), "{err}");
    }

    #[test]
    fn folders_reject_workspace_folder_duplicate() {
        let props = props_with_folders(&[("/workspaces/app", ".")]);
        let err = resolve_folders(Path::new("/tmp"), &ctx(), "/workspaces/app", &props)
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicates `workspaceFolder`"), "{err}");
    }

    #[test]
    fn folders_reject_missing_host_folder() {
        let props = props_with_folders(&[("/workspaces/x", "does-not-exist-9f3a")]);
        let err = resolve_folders(Path::new("/tmp"), &ctx(), "/workspaces/app", &props)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn workspace_json_lists_primary_first() {
        let extras = vec![("/workspaces/.shared".to_string(), PathBuf::from("/x"))];
        let json = workspace_file_json("app-2", "/workspaces/app", &extras, &[]);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["folders"],
            serde_json::json!([
                { "name": "app-2", "path": "/workspaces/app" },
                { "path": "/workspaces/.shared" }
            ])
        );
        assert_eq!(
            parsed["settings"]["terminal.integrated.cwd"],
            "${workspaceFolder:app-2}"
        );
        // No recommendations passed → no `extensions` key at all.
        assert!(parsed.get("extensions").is_none());
    }

    #[test]
    fn workspace_json_includes_extension_recommendations() {
        let recs = vec!["dbaeumer.vscode-eslint".to_string()];
        let json = workspace_file_json("app", "/workspaces/app", &[], &recs);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["extensions"]["recommendations"],
            serde_json::json!(["dbaeumer.vscode-eslint"])
        );
    }

    #[test]
    fn git_companion_mount_uses_identical_paths() {
        let mount = git_companion_mount(Path::new("/home/u/repo"));
        assert_eq!(mount, "/home/u/repo/.git:/home/u/repo/.git");
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

    #[test]
    fn fetch_freshness_window() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::now();
        assert!(fetch_is_fresh(now - Duration::from_secs(60), now));
        assert!(!fetch_is_fresh(now - Duration::from_secs(120), now));
        assert!(!fetch_is_fresh(now - Duration::from_secs(3600), now));
        assert!(fetch_is_fresh(now + Duration::from_secs(5), now));
    }

    #[test]
    fn worktree_starts_from_fetched_origin_default_untracked() {
        let root = std::env::temp_dir().join(format!("devsandbox-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (origin, base, wt) = (root.join("origin"), root.join("base"), root.join("wt"));
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "main"]);
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&root, &["clone", "-q", &origin.to_string_lossy(), "base"]);
        // Base checkout sits on a feature branch, as a working agent's would.
        git(&base, &["checkout", "-q", "-b", "feature"]);
        git(&base, &["commit", "-q", "--allow-empty", "-m", "wip"]);
        let feature_tip = git(&base, &["rev-parse", "HEAD"]);
        // Upstream moves on after the clone: only a fetch can see this.
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let main_tip = git(&origin, &["rev-parse", "HEAD"]);

        create_worktree(&base, &wt, "sandbox/x", None).unwrap();

        assert_eq!(git(&wt, &["rev-parse", "HEAD"]), main_tip);
        let upstream = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
            .output()
            .unwrap();
        assert!(!upstream.status.success(), "branch must not track origin");
        assert_eq!(git(&base, &["rev-parse", "HEAD"]), feature_tip);
        assert_eq!(git(&base, &["branch", "--show-current"]), "feature");

        // An explicit base wins over detection; a bad one errors, no fallback.
        let wt2 = root.join("wt2");
        create_worktree(&base, &wt2, "sandbox/y", Some("feature")).unwrap();
        assert_eq!(git(&wt2, &["rev-parse", "HEAD"]), feature_tip);
        let err = create_worktree(&base, &root.join("wt3"), "sandbox/z", Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("worktree base `nope`"), "{err}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn ssh_agent_link_creates_and_repoints() {
        let dir = std::env::temp_dir().join(format!("devsandbox-agent-{}", std::process::id()));
        let first = dir.join("agent-a.sock");
        let second = dir.join("agent-b.sock");

        let link = ssh_agent_link(&dir, "repo-abc1", &first).unwrap();
        assert_eq!(link, dir.join("repo-abc1.sock"));
        assert_eq!(std::fs::read_link(&link).unwrap(), first);

        // A second call re-points the same link at the new target.
        let link2 = ssh_agent_link(&dir, "repo-abc1", &second).unwrap();
        assert_eq!(link2, link);
        assert_eq!(std::fs::read_link(&link).unwrap(), second);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ssh_agent_refresh_noop_without_forwarding() {
        // `ssh_auth_sock: None` means forwarding was never on for this instance;
        // the gate returns before any env/fs access, so no link is created even
        // if the data dir and a live agent are present. Uses a unique id so the
        // agent dir (if it exists at all) can't already hold a matching link.
        let id = format!("refresh-gate-{}", std::process::id());
        let mut info = instance("repo");
        info.instance_id = id.clone();
        assert!(info.ssh_auth_sock.is_none());
        ssh_agent_refresh(&info);
        if let Ok(path) = State::path() {
            if let Some(parent) = path.parent() {
                let link = parent.join("agent").join(format!("{id}.sock"));
                assert!(!link.exists(), "gate should not create {}", link.display());
            }
        }
    }

    // --- features: pure generators ---

    use crate::features::{FeatureMetadata, FeatureOption};

    fn md() -> FeatureMetadata {
        FeatureMetadata::default()
    }

    fn opt_default(v: serde_json::Value) -> FeatureOption {
        FeatureOption { default: Some(v) }
    }

    #[test]
    fn safe_id_maps_like_get_safe_id() {
        // Dashes and dots become underscores; uppercased.
        assert_eq!(safe_id("install-jq"), "INSTALL_JQ");
        assert_eq!(safe_id("version"), "VERSION");
        // A leading digit run collapses to a single underscore.
        assert_eq!(safe_id("2fa"), "_FA");
        assert_eq!(safe_id("123abc"), "_ABC");
        // Leading underscores/digits collapse together to one underscore.
        assert_eq!(safe_id("_1_x"), "_X");
        // Already-safe names just uppercase.
        assert_eq!(safe_id("NODE_gyp"), "NODE_GYP");
    }

    #[test]
    fn escape_dq_escapes_quote_and_backslash_not_dollar() {
        assert_eq!(escape_dq(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_dq(r"a\b"), r"a\\b");
        // Dollar stays literal so ENV expands ${PATH} at build time.
        assert_eq!(escape_dq("/usr/local/go/bin:${PATH}"), "/usr/local/go/bin:${PATH}");
    }

    #[test]
    fn feature_option_values_scalar_forms() {
        assert_eq!(
            feature_option_values(&FeatureOptions::Version("1.22".into())).unwrap(),
            Some(BTreeMap::from([("version".to_string(), "1.22".to_string())]))
        );
        assert_eq!(
            feature_option_values(&FeatureOptions::Enabled(true)).unwrap(),
            Some(BTreeMap::new())
        );
        // Disabled => skip the feature entirely.
        assert_eq!(feature_option_values(&FeatureOptions::Enabled(false)).unwrap(), None);

        let table = BTreeMap::from([
            ("version".to_string(), toml::Value::String("lts".into())),
            ("installTools".to_string(), toml::Value::Boolean(true)),
            ("uid".to_string(), toml::Value::Integer(1000)),
        ]);
        let values = feature_option_values(&FeatureOptions::Options(table)).unwrap().unwrap();
        assert_eq!(values["version"], "lts");
        assert_eq!(values["installTools"], "true");
        assert_eq!(values["uid"], "1000");
    }

    #[test]
    fn feature_option_values_rejects_arrays_and_tables() {
        let table = BTreeMap::from([(
            "list".to_string(),
            toml::Value::Array(vec![toml::Value::String("a".into())]),
        )]);
        let err = feature_option_values(&FeatureOptions::Options(table)).unwrap_err().to_string();
        assert!(err.contains("array or table"), "{err}");
    }

    #[test]
    fn feature_env_file_merges_defaults_then_user_override() {
        let mut m = md();
        m.options.insert("version".into(), opt_default(serde_json::json!("latest")));
        m.options.insert("installTools".into(), opt_default(serde_json::json!(true)));
        m.options.insert("uid".into(), opt_default(serde_json::json!(1000)));
        // User overrides `version`; leaves the others at their default.
        let values = BTreeMap::from([("version".to_string(), "1.22".to_string())]);
        let env = feature_env_file(&m, &values);
        // BTreeMap => alphabetical option order (installTools, uid, version).
        assert_eq!(
            env,
            "INSTALLTOOLS=\"true\"\nUID=\"1000\"\nVERSION=\"1.22\""
        );
    }

    #[test]
    fn feature_env_file_escapes_values() {
        let values = BTreeMap::from([("flag".to_string(), r#"a"b\c"#.to_string())]);
        let env = feature_env_file(&md(), &values);
        assert_eq!(env, r#"FLAG="a\"b\\c""#);
    }

    #[test]
    fn install_wrapper_has_banner_sourcing_and_exec() {
        let mut m = md();
        m.name = Some("O'Brien's Feature".into());
        m.version = Some("1.0.0".into());
        let env_file = "VERSION=\"1.22\"";
        let w = install_wrapper(&m, "ghcr.io/x/y", env_file);
        assert!(w.starts_with("#!/bin/sh\nset -e\n"), "{w}");
        // Single-quote escaping in the banner name.
        assert!(w.contains("echo 'Feature       : O'\\''Brien'\\''s Feature'"), "{w}");
        assert!(w.contains("echo 'Id            : ghcr.io/x/y'"), "{w}");
        // Options echoed, indented.
        assert!(w.contains("echo '    VERSION=\"1.22\"'"), "{w}");
        // set -a sourcing block, in order.
        assert!(w.contains("set -a\n. ../devcontainer-features.builtin.env\n. ./devcontainer-features.env\nset +a"), "{w}");
        // Runs install.sh at the end.
        assert!(w.trim_end().ends_with("chmod +x ./install.sh\n./install.sh"), "{w}");
    }

    #[test]
    fn builtin_env_lines() {
        assert_eq!(builtin_env("root", "vscode"), "_CONTAINER_USER=root\n_REMOTE_USER=vscode\n");
    }

    fn fb(dir_name: &str, container_env: &[(&str, &str)]) -> FeatureBuild {
        let mut m = md();
        m.container_env = container_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        FeatureBuild { dir_name: dir_name.into(), metadata: m }
    }

    #[test]
    fn dockerfile_base_env_escaping_and_copy_run() {
        let features = vec![fb(
            "0-go",
            &[
                ("PATH", "/usr/local/go/bin:${PATH}"),
                ("QUOTED", r#"a"b\c"#),
            ],
        )];
        let df = generate_dockerfile("alpine:3.20", &features, None, "root");
        assert!(df.starts_with("FROM alpine:3.20\nUSER root\n"), "{df}");
        assert!(df.contains("RUN mkdir -p /tmp/dev-container-features\n"), "{df}");
        assert!(
            df.contains("COPY devcontainer-features.builtin.env /tmp/dev-container-features/\n"),
            "{df}"
        );
        // Home probe uses getent for both users (root here).
        assert!(
            df.contains("_CONTAINER_USER_HOME=$(getent passwd root | cut -d: -f6)")
                && df.contains("_REMOTE_USER_HOME=$(getent passwd root | cut -d: -f6)"),
            "{df}"
        );
        // ENV: $ preserved, " and \ escaped.
        assert!(df.contains("ENV PATH=\"/usr/local/go/bin:${PATH}\"\n"), "{df}");
        assert!(df.contains("ENV QUOTED=\"a\\\"b\\\\c\"\n"), "{df}");
        // Per-feature COPY + RUN.
        assert!(df.contains("COPY 0-go /tmp/dev-container-features/0-go\n"), "{df}");
        assert!(
            df.contains(
                "RUN chmod -R 0755 /tmp/dev-container-features/0-go && \
cd /tmp/dev-container-features/0-go && \
chmod +x ./devcontainer-features-install.sh && \
./devcontainer-features-install.sh\n"
            ),
            "{df}"
        );
        // No containerUser => stays root, no trailing USER line.
        assert!(!df.contains("\nUSER root\nUSER"), "{df}");
        assert_eq!(df.matches("USER ").count(), 1, "only the initial USER root: {df}");
    }

    #[test]
    fn dockerfile_restores_container_user_when_set() {
        let features = vec![fb("0-common-utils", &[])];
        let df = generate_dockerfile("debian:bookworm", &features, Some("vscode"), "vscode");
        // Final USER line restores the configured container user.
        assert!(df.trim_end().ends_with("USER vscode"), "{df}");
        // Home probe uses the configured users.
        assert!(df.contains("_CONTAINER_USER_HOME=$(getent passwd vscode | cut -d: -f6)"), "{df}");
        assert!(df.contains("_REMOTE_USER_HOME=$(getent passwd vscode | cut -d: -f6)"), "{df}");
    }

    /// Docker-gated: builds a derived image from a synthetic pre-extracted feature
    /// (no network) using the pure generators, then runs it and checks the option
    /// value and remote user landed. Skips (with a message) when `docker info`
    /// fails — so it's a no-op in a sandbox without docker, but exercises the real
    /// build on CI (where it fails instead of skipping).
    #[test_utils::docker_test]
    fn derived_image_builds_with_docker() -> Result<(), &'static str> {
        use std::process::Command;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let ctx = std::env::temp_dir().join(format!("devsandbox-feat-build-{stamp}"));
        std::fs::create_dir_all(&ctx).unwrap();
        let tag = format!("devsandbox-feat-test-{stamp}");

        let cleanup = |ctx: &Path, tag: &str| {
            let _ = std::fs::remove_dir_all(ctx);
            let _ = Command::new("docker").args(["rmi", "-f", tag]).output();
        };

        // Synthetic feature: one option `myopt` (default "fallback"), a
        // containerEnv, and an install.sh writing a marker with the option and
        // remote user. Uses /bin/sh (alpine busybox).
        let mut m = md();
        m.id = Some("marker".into());
        m.name = Some("Marker".into());
        m.version = Some("1.0.0".into());
        m.options.insert("myopt".into(), opt_default(serde_json::json!("fallback")));
        m.container_env = BTreeMap::from([("MARKER_ENV".to_string(), "from-env".to_string())]);

        let feat_dir = ctx.join("0-marker");
        std::fs::create_dir_all(&feat_dir).unwrap();
        std::fs::write(
            feat_dir.join("install.sh"),
            "#!/bin/sh\nset -e\nmkdir -p /opt\n\
             echo \"myopt=$MYOPT remote=$_REMOTE_USER env=$MARKER_ENV\" > /opt/marker\n",
        )
        .unwrap();

        // Env-file: user overrides myopt to "chosen".
        let values = BTreeMap::from([("myopt".to_string(), "chosen".to_string())]);
        let env_file = feature_env_file(&m, &values);
        std::fs::write(feat_dir.join("devcontainer-features.env"), &env_file).unwrap();
        std::fs::write(
            feat_dir.join("devcontainer-features-install.sh"),
            install_wrapper(&m, "local/marker", &env_file),
        )
        .unwrap();
        std::fs::write(
            ctx.join("devcontainer-features.builtin.env"),
            builtin_env("root", "root"),
        )
        .unwrap();

        let builds = vec![FeatureBuild { dir_name: "0-marker".into(), metadata: m }];
        let dockerfile = generate_dockerfile("alpine:3.20", &builds, None, "root");
        let dockerfile_path = ctx.join("Dockerfile.devsandbox-features");
        std::fs::write(&dockerfile_path, dockerfile).unwrap();

        let build = Command::new("docker")
            .args([
                "build",
                "-t",
                &tag,
                "-f",
                &dockerfile_path.to_string_lossy(),
                &ctx.to_string_lossy(),
            ])
            .output()
            .unwrap();
        if !build.status.success() {
            let stderr = String::from_utf8_lossy(&build.stderr).into_owned();
            cleanup(&ctx, &tag);
            panic!("docker build failed: {stderr}");
        }

        let run = Command::new("docker")
            .args(["run", "--rm", &tag, "cat", "/opt/marker"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
        let ok = run.status.success();
        cleanup(&ctx, &tag);
        assert!(ok, "docker run failed: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(stdout.trim(), "myopt=chosen remote=root env=from-env", "marker: {stdout}");
        Ok(())
    }
}
