use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{build_hash, substitute, Config, MountContext, ResolvedSandbox, CONFIG_FILE};
use crate::runtime::{backend, ServiceEndpoint, NAME_PREFIX};
use crate::state::{Instance, State};

use super::{container_drifted, pick, services};

mod editor;
mod image;
mod lifecycle;
mod mounts;
mod ssh_agent;
mod worktree;

pub(crate) use editor::write_vscode_name_config;
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
use ssh_agent::ssh_agent_forward;
use worktree::{create_worktree, git_companion_mount};

/// Branch pattern for worktree instances when neither `--branch` nor the
/// sandbox's `worktree-branch` is set. Also the TUI prompt's completion base
/// after `--branch`.
pub const DEFAULT_WORKTREE_BRANCH: &str = "sandbox/${instance}";

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
    let ResolvedMounts { args: mut mounts, volumes: mut instance_volumes } =
        resolve_mounts(dir, folder, &basename, instance_id, sandbox)?;
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
    instance_volumes.extend(feature.volumes);
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
    // Keep volumes a previous run recorded: a rebuild keeps them on disk even
    // when the config no longer mounts them, and `rm` must still find them.
    let volumes = merge_volumes(
        state.instances.get(instance).map(|i| i.volumes.as_slice()).unwrap_or_default(),
        instance_volumes,
    );
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
            volumes,
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
            shell_history: None,
            workspace: "/workspaces/repo".into(),
            workspace_file: None,
            remote_env: Default::default(),
            remote_user: None,
            ssh_auth_sock: None,
            devsbd_arch: None,
            volumes: Vec::new(),
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
}
