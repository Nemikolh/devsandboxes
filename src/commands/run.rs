use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{pick, services};
use crate::config::{
    substitute, Config, FeatureOptions, LifecycleCommand, MountContext, ResolvedSandbox,
    SandboxProperties, CONFIG_FILE,
};
use crate::features::{self, FeatureMetadata};
use crate::runtime::{backend, ServiceEndpoint, NAME_PREFIX};
use crate::state::{Instance, State};

pub fn run(
    dir: &Path,
    sandbox_name: Option<String>,
    instance_name: Option<String>,
    branch_override: Option<String>,
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
    // `${configDir}` / `${localWorkspaceFolder(Basename)}` / `${sharedVolumes}`
    // / `${instance}` are usable in `workspaceFolder`, `mounts`, and cache
    // sources; build the context once. Instance naming comes first so
    // `${instance}` can anchor per-instance state in mount sources.
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let config_dir_str = config_dir.to_string_lossy().into_owned();
    let folder_str = folder.to_string_lossy().into_owned();
    let shared_volumes = config_dir.join("shared-volumes").to_string_lossy().into_owned();

    let mut state = State::load()?;
    // Deterministic name: explicit --name, or the sandbox name for the first
    // instance. When the base instance is already live, default to the next
    // free ordinal so a repeat `run` yields a worktree instance (see below).
    let instance = match instance_name {
        Some(name) => name,
        None => default_instance_name(&state, &sandbox_name),
    };
    let container_name = format!("{NAME_PREFIX}{instance}");

    let var_ctx = MountContext {
        config_dir: &config_dir_str,
        workspace_folder: &folder_str,
        workspace_folder_basename: &basename,
        shared_volumes: &shared_volumes,
        instance: &instance,
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

    // Reuse an existing instance whose container still exists: start it if
    // stopped, refresh config, run postStartCommand, done.
    let container_status = backend().is_running(&container_name)?;
    if let (true, Some(running)) = (state.instances.contains_key(&instance), container_status) {
        warn_on_drift(&container_name, &sandbox.config_hash)?;
        if !running {
            backend().run_checked(&["start", &container_name])?;
        }
        // Services may have been recreated with new addresses since the
        // sandbox was created; refresh how it resolves them.
        let project = services::project_id(dir)?;
        let service_names = props.services.clone().unwrap_or_default();
        let (_, endpoints) =
            services::ensure_services(&config, dir, &project, &instance, &service_names)?;
        backend().wire_service_dns(&container_name, &endpoints)?;
        let extensions = props.vscode_extensions().unwrap_or(&[]);
        if !extensions.is_empty() || props.remote_user.is_some() {
            write_vscode_name_config(&container_name, extensions, props.remote_user.as_deref())?;
        }
        // Refresh the generated workspace file (and record it for instances
        // created before it existed).
        let workspace_file =
            write_workspace_file(&container_name, &instance, &workspace, &extra_folders);
        if let Some(inst) = state.instances.get_mut(&instance)
            && inst.workspace_file != workspace_file
        {
            inst.workspace_file = workspace_file;
            state.save()?;
        }
        if let Some(cmd) = &props.post_start_command {
            exec_lifecycle(
                &container_name,
                &workspace,
                props.remote_env.as_ref(),
                props.remote_user.as_deref(),
                cmd,
            )
            .context("postStartCommand failed")?;
        }
        println!("{instance}");
        return Ok(());
    }

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
        services::ensure_services(&config, dir, &project, &instance, &service_names)?;

    // First instance for this base folder mounts it directly; a base folder
    // already live in another instance gets a git worktree so the two containers
    // never share a working tree.
    let base_in_use = state
        .instances
        .values()
        .filter(|i| i.base_folder == folder)
        .any(|i| container_running(&i.container));
    let (source, worktree, branch, extra_mounts) = if base_in_use {
        // Branch for the worktree: `--branch` override, else the sandbox's
        // `worktree-branch`, else the default. `${instance}` (and the other mount
        // variables) are substituted so each instance gets a unique branch.
        let pattern = branch_override
            .or_else(|| props.worktree_branch.clone())
            .unwrap_or_else(|| "sandbox/${instance}".to_string());
        let branch = substitute(&pattern, &var_ctx);
        // Must be absolute: `create_worktree` runs `git -C <base>`, so a
        // `dir`-relative path would resolve under the base repo instead of here,
        // and the mount/state would point at a different (empty) directory.
        let wt = config_dir.join(".worktrees").join(&instance);
        create_worktree(&folder, &wt, &branch)?;
        // The worktree's `.git` file points at `<base>/.git/worktrees/..`
        // by absolute host path; mount the base `.git` at the identical
        // path so git works inside the container.
        (wt.clone(), Some(wt), Some(branch), vec![git_companion_mount(&folder)])
    } else {
        (folder.clone(), None, None, Vec::new())
    };
    let mut mounts = resolve_mounts(dir, &folder, &basename, &instance, &sandbox)?;
    for (target, host) in &extra_folders {
        mounts.push(format!("type=bind,source={},target={target}", host.display()));
    }
    // Package-manager caches: shared bind mounts + the env vars pointing at them.
    let (cache_mounts, cache_env) = resolve_caches(&config_dir, props)?;
    mounts.extend(cache_mounts);
    // Managed per-instance shell history: provision the host file and mount it.
    let shell_history = if props.persist_shell_history == Some(true) {
        let (path, mount) = provision_shell_history(dir, &instance)?;
        mounts.push(mount);
        Some(path)
    } else {
        None
    };
    run_container(
        dir,
        &sandbox,
        &container_name,
        &source,
        &folder,
        &workspace,
        &extra_mounts,
        &mounts,
        &cache_env,
        &networks,
        &endpoints,
    )?;
    let container = container_name.clone();
    let workspace_file = write_workspace_file(&container, &instance, &workspace, &extra_folders);

    state.instances.insert(
        instance.clone(),
        Instance {
            sandbox: sandbox_name,
            project,
            container: container.clone(),
            folder: source,
            base_folder: folder,
            worktree,
            branch,
            shell_history,
            workspace: workspace.clone(),
            workspace_file,
            remote_env: props.remote_env.clone().unwrap_or_default(),
            remote_user: props.remote_user.clone(),
            created_unix: Instance::now(),
        },
    );
    state.save()?;

    let extensions = props.vscode_extensions().unwrap_or(&[]);
    if !extensions.is_empty() || props.remote_user.is_some() {
        write_vscode_name_config(&container, extensions, props.remote_user.as_deref())?;
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
                cmd,
            )
            .with_context(|| format!("{name} failed (container `{container}` kept)"))?;
        }
    }

    println!("{instance}");
    Ok(())
}

/// Default instance name: the sandbox name for the first instance, or when that
/// instance is already live, the first free `<sandbox>-<n>` ordinal (n >= 2).
/// A stopped base instance is reused rather than duplicated.
fn default_instance_name(state: &State, sandbox: &str) -> String {
    if !state.instances.contains_key(sandbox)
        || !container_running(&format!("{NAME_PREFIX}{sandbox}"))
    {
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

/// Bind mount for the base repo's `.git` at the identical host path, so a
/// worktree's absolute `gitdir` pointer resolves inside the container.
fn git_companion_mount(base: &Path) -> String {
    let git = base.join(".git");
    format!("{}:{}", git.display(), git.display())
}

/// True when the named container exists and is running.
fn container_running(container: &str) -> bool {
    backend().is_running(container).ok().flatten() == Some(true)
}

/// Create a git worktree on a fresh `branch`. Git refuses to check out a branch
/// already checked out elsewhere, so the branch must be unique per instance
/// (the default `sandbox/${instance}` pattern guarantees this).
fn create_worktree(base: &Path, worktree: &Path, branch: &str) -> Result<()> {
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    // With an unborn HEAD, `git worktree add -b` infers `--orphan`, exits 0, and
    // lays down a worktree with no files; guard against handing that empty path
    // to the container as a bind mount.
    let head = std::process::Command::new("git")
        .args(["-C", &base.to_string_lossy(), "rev-parse", "--verify", "HEAD"])
        .output()
        .context("failed to run git (is it installed?)")?;
    if !head.status.success() {
        bail!(
            "base repo `{}` has no commits; cannot create a worktree",
            base.display()
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

/// Warn when the reused container's recorded config hash differs from the
/// freshly resolved one. Compose containers carry no such label (None) — skip.
fn warn_on_drift(container: &str, expected: &str) -> Result<()> {
    if let Some(hash) = backend().label(container, "devsandbox.config_hash")?
        && hash != expected
    {
        eprintln!(
            "warning: config for `{container}` changed since it was created; \
             remove and re-run to apply changes"
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
) -> Result<()> {
    let props = &sandbox.properties;
    let image = image_for(dir, sandbox)?;
    let mount = format!("{}:{workspace}", source.display());
    let sandbox_label = format!("devsandbox.sandbox={}", sandbox.name);
    let instance = container.strip_prefix(NAME_PREFIX).unwrap_or(container);
    let instance_label = format!("devsandbox.instance={instance}");
    let hash_label = format!("devsandbox.config_hash={}", sandbox.config_hash);
    let base_label = format!("devsandbox.base_folder={}", base_folder.display());
    let mut args: Vec<String> = [
        "run", "-d", "--name", container,
        "--label", &sandbox_label, "--label", &instance_label,
        "--label", &hash_label, "--label", &base_label,
        "-v", &mount, "-w", workspace,
    ]
    .map(str::to_string)
    .into();
    if props.init == Some(true) {
        args.push("--init".into());
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
/// fail on a non-existent host path.
fn resolve_mounts(
    dir: &Path,
    folder: &Path,
    basename: &str,
    instance: &str,
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
        instance,
    };
    let mut args = Vec::with_capacity(mounts.len());
    for mount in mounts {
        let resolved = mount
            .resolve(&ctx)
            .with_context(|| format!("sandbox `{}`: invalid mount", sandbox.name))?;
        if resolved.kind == "bind" {
            if let Some(source) = &resolved.source {
                ensure_bind_source(Path::new(source))?;
            }
        }
        args.push(resolved.to_arg());
    }
    Ok(args)
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
/// first, then the extra `folders` roots.
fn workspace_file_json(workspace: &str, extra_folders: &[(String, PathBuf)]) -> String {
    let folders: Vec<serde_json::Value> = std::iter::once(workspace)
        .chain(extra_folders.iter().map(|(target, _)| target.as_str()))
        .map(|path| serde_json::json!({ "path": path }))
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({ "folders": folders }))
        .expect("workspace json serializes")
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
) -> Option<String> {
    let path = format!("/workspaces/{instance}.code-workspace");
    let json = workspace_file_json(workspace, extra_folders);
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

/// Provision this instance's managed `.zsh_history` under
/// `${configDir}/shared-volumes/history/<instance>.zsh_history` (touched so the
/// bind mounts as a file, not a directory) and return (host path, `--mount` arg).
/// An existing file is reused untouched, so history survives rm + run rebuilds.
fn provision_shell_history(dir: &Path, instance: &str) -> Result<(PathBuf, String)> {
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let path = config_dir
        .join("shared-volumes")
        .join("history")
        .join(format!("{instance}.zsh_history"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    let mount = format!("type=bind,source={},target=/root/.zsh_history", path.display());
    Ok((path, mount))
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
fn exec_lifecycle(
    container: &str,
    workspace: &str,
    remote_env: Option<&BTreeMap<String, String>>,
    remote_user: Option<&str>,
    cmd: &LifecycleCommand,
) -> Result<()> {
    for argv in cmd.commands() {
        if argv.is_empty() {
            continue;
        }
        let mut args: Vec<String> = vec!["exec".into(), "-w".into(), workspace.into()];
        if let Some(user) = remote_user {
            args.push("-u".into());
            args.push(user.to_string());
        }
        for (key, value) in remote_env.into_iter().flatten() {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        args.push(container.into());
        args.extend(argv);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        backend().run_checked(&arg_refs)?;
    }
    Ok(())
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
        let json = workspace_file_json("/workspaces/app", &extras);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["folders"],
            serde_json::json!([
                { "path": "/workspaces/app" },
                { "path": "/workspaces/.shared" }
            ])
        );
    }

    #[test]
    fn git_companion_mount_uses_identical_paths() {
        let mount = git_companion_mount(Path::new("/home/u/repo"));
        assert_eq!(mount, "/home/u/repo/.git:/home/u/repo/.git");
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
    /// build on CI.
    #[test]
    fn derived_image_builds_with_docker() {
        use std::process::Command;

        let probe = Command::new("docker").arg("info").output();
        let docker_ok = matches!(probe, Ok(o) if o.status.success());
        if !docker_ok {
            eprintln!("skipping derived_image_builds_with_docker: docker unavailable");
            return;
        }

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
    }
}
