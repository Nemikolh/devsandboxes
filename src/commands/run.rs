use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{pick, services};
use crate::config::{
    substitute, Config, LifecycleCommand, MountContext, ResolvedSandbox, SandboxProperties,
    CONFIG_FILE,
};
use crate::runtime::{backend, ServiceEndpoint, NAME_PREFIX};
use crate::state::{Instance, State};

pub fn run(dir: &Path, sandbox_name: Option<String>, instance_name: Option<String>) -> Result<()> {
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
    // `${configDir}` / `${localWorkspaceFolder(Basename)}` are usable in
    // `workspaceFolder`, `mounts`, and cache sources; build the context once.
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let config_dir_str = config_dir.to_string_lossy().into_owned();
    let folder_str = folder.to_string_lossy().into_owned();
    let var_ctx = MountContext {
        config_dir: &config_dir_str,
        workspace_folder: &folder_str,
        workspace_folder_basename: &basename,
    };
    let workspace = substitute(
        &props
            .workspace_folder
            .clone()
            .unwrap_or_else(|| format!("/workspaces/{basename}")),
        &var_ctx,
    );

    let mut state = State::load()?;
    // Deterministic name: explicit --name, or the sandbox name for the first
    // instance. When the base instance is already live, default to the next
    // free ordinal so a repeat `run` yields a worktree instance (see below).
    let instance = match instance_name {
        Some(name) => name,
        None => default_instance_name(&state, &sandbox_name),
    };
    let container_name = format!("{NAME_PREFIX}{instance}");

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
    let (source, worktree, extra_mounts) = if base_in_use {
        let wt = dir.join(".worktrees").join(&instance);
        create_worktree(&folder, &wt, &instance)?;
        // The worktree's `.git` file points at `<base>/.git/worktrees/..`
        // by absolute host path; mount the base `.git` at the identical
        // path so git works inside the container.
        (wt.clone(), Some(wt), vec![git_companion_mount(&folder)])
    } else {
        (folder.clone(), None, Vec::new())
    };
    let mut mounts = resolve_mounts(dir, &folder, &basename, &sandbox)?;
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

    state.instances.insert(
        instance.clone(),
        Instance {
            sandbox: sandbox_name,
            project,
            container: container.clone(),
            folder: source,
            base_folder: folder,
            worktree,
            shell_history,
            workspace: workspace.clone(),
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

/// Create a git worktree with a fresh `sandbox/<instance>` branch. Git refuses
/// to check out a branch already checked out elsewhere, so the branch is unique
/// per instance.
fn create_worktree(base: &Path, worktree: &Path, instance: &str) -> Result<()> {
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let branch = format!("sandbox/{instance}");
    let status = std::process::Command::new("git")
        .args([
            "-C",
            &base.to_string_lossy(),
            "worktree",
            "add",
            &worktree.to_string_lossy(),
            "-b",
            &branch,
        ])
        .status()
        .context("failed to run git (is it installed?)")?;
    if !status.success() {
        bail!("git worktree add failed for `{}`", worktree.display());
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
    sandbox: &ResolvedSandbox,
) -> Result<Vec<String>> {
    let Some(mounts) = &sandbox.properties.mounts else {
        return Ok(Vec::new());
    };
    // `${configDir}` anchors host-backed volumes; make it absolute.
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let ctx = MountContext {
        config_dir: &config_dir.to_string_lossy(),
        workspace_folder: &folder.to_string_lossy(),
        workspace_folder_basename: basename,
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
/// `build.dockerfile` sandboxes.
fn image_for(dir: &Path, sandbox: &ResolvedSandbox) -> Result<String> {
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
            shell_history: None,
            workspace: "/workspaces/repo".into(),
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

    #[test]
    fn git_companion_mount_uses_identical_paths() {
        let mount = git_companion_mount(Path::new("/home/u/repo"));
        assert_eq!(mount, "/home/u/repo/.git:/home/u/repo/.git");
    }
}
