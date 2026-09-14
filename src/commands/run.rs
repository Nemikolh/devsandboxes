use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::pick;
use crate::config::{Config, LifecycleCommand, ResolvedSandbox, StringOrList, CONFIG_FILE};
use crate::docker::{self, NAME_PREFIX};
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

    if props.docker_compose_file.is_some() && (props.image.is_some() || props.build.is_some()) {
        bail!(
            "sandbox `{sandbox_name}`: `dockerComposeFile` cannot be combined with `image` or `build`"
        );
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
    let workspace = props
        .workspace_folder
        .clone()
        .unwrap_or_else(|| format!("/workspaces/{basename}"));

    let mut state = State::load()?;
    // Deterministic name: explicit --name, or the sandbox name for the first
    // instance. When the base instance is already live, default to the next
    // free ordinal so a repeat `run` yields a worktree instance (see below).
    let instance = match instance_name {
        Some(name) => name,
        None => default_instance_name(&state, &sandbox_name),
    };
    let container_name = format!("{NAME_PREFIX}{instance}");

    // Reuse an existing instance whose container still exists: docker start it
    // if stopped, refresh config, run postStartCommand, done.
    let container_status = docker::inspect(&container_name, "{{.State.Running}}")?;
    if let (true, Some(running)) = (state.instances.contains_key(&instance), &container_status) {
        warn_on_drift(&container_name, &sandbox.config_hash)?;
        if running != "true" {
            docker::run_checked(&["start", &container_name])?;
        }
        if let Some(extensions) = props.vscode_extensions() {
            write_vscode_name_config(&container_name, extensions)?;
        }
        if let Some(cmd) = &props.post_start_command {
            exec_lifecycle(&container_name, &workspace, props.remote_env.as_ref(), cmd)
                .context("postStartCommand failed")?;
        }
        println!("{instance}");
        return Ok(());
    }

    // initializeCommand runs on the host, before anything is created.
    if let Some(cmd) = &props.initialize_command {
        run_host_commands(dir, cmd).context("initializeCommand failed")?;
    }

    let (container, source, worktree) = match &props.docker_compose_file {
        Some(files) => (compose_up(dir, &sandbox, files, &container_name)?, folder.clone(), None),
        None => {
            // First instance for this base folder mounts it directly; a base
            // folder already live in another instance gets a git worktree so
            // the two containers never share a working tree.
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
                let git = folder.join(".git");
                let mount = format!("{}:{}", git.display(), git.display());
                (wt.clone(), Some(wt), vec![mount])
            } else {
                (folder.clone(), None, Vec::new())
            };
            docker_run(dir, &sandbox, &container_name, &source, &folder, &workspace, &extra_mounts)?;
            (container_name.clone(), source, worktree)
        }
    };

    state.instances.insert(
        instance.clone(),
        Instance {
            sandbox: sandbox_name,
            container: container.clone(),
            folder: source,
            base_folder: folder,
            worktree,
            workspace: workspace.clone(),
            remote_env: props.remote_env.clone().unwrap_or_default(),
            created_unix: Instance::now(),
        },
    );
    state.save()?;

    if let Some(extensions) = props.vscode_extensions() {
        write_vscode_name_config(&container, extensions)?;
    }

    for (name, cmd) in [
        ("onCreateCommand", &props.on_create_command),
        ("updateContentCommand", &props.update_content_command),
        ("postCreateCommand", &props.post_create_command),
        ("postStartCommand", &props.post_start_command),
        ("postAttachCommand", &props.post_attach_command),
    ] {
        if let Some(cmd) = cmd {
            exec_lifecycle(&container, &workspace, props.remote_env.as_ref(), cmd)
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
    (2..)
        .map(|n| format!("{sandbox}-{n}"))
        .find(|name| !state.instances.contains_key(name))
        .expect("infinite range yields a free name")
}

/// True when the named container exists and is running.
fn container_running(container: &str) -> bool {
    docker::inspect(container, "{{.State.Running}}")
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
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
    let label = docker::inspect(container, "{{index .Config.Labels \"devsandbox.config_hash\"}}")?;
    if let Some(hash) = label {
        if !hash.is_empty() && hash != expected {
            eprintln!(
                "warning: config for `{container}` changed since it was created; \
                 remove and re-run to apply changes"
            );
        }
    }
    Ok(())
}

fn docker_run(
    dir: &Path,
    sandbox: &ResolvedSandbox,
    container: &str,
    source: &Path,
    base_folder: &Path,
    workspace: &str,
    extra_mounts: &[String],
) -> Result<()> {
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
    for mount in extra_mounts {
        args.push("-v".into());
        args.push(mount.clone());
    }
    if let Some(env) = &sandbox.properties.container_env {
        for (key, value) in env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
    }
    args.push(image);
    // Same trick as devcontainers: keep the container alive, work happens via exec.
    args.extend(["sleep".into(), "infinity".into()]);

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    docker::run_checked(&arg_refs)
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
    let context = build.context.as_deref().unwrap_or(".");
    let tag = format!("{NAME_PREFIX}img-{}", sandbox.name);

    let mut args: Vec<String> = vec![
        "build".into(),
        "-t".into(), tag.clone(),
        "-f".into(), dir.join(dockerfile).to_string_lossy().into_owned(),
    ];
    if let Some(build_args) = &build.args {
        for (key, value) in build_args {
            args.push("--build-arg".into());
            args.push(format!("{key}={value}"));
        }
    }
    if let Some(target) = &build.target {
        args.extend(["--target".into(), target.clone()]);
    }
    if let Some(cache_from) = &build.cache_from {
        for image in cache_from.to_vec() {
            args.extend(["--cache-from".into(), image.to_string()]);
        }
    }
    if let Some(options) = &build.options {
        args.extend(options.iter().cloned());
    }
    args.push(dir.join(context).to_string_lossy().into_owned());

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    docker::run_checked(&arg_refs)?;
    Ok(tag)
}

/// `docker compose up` the sandbox's service (plus `runServices`), using the
/// prefixed instance name as the compose project so `ps` filtering still
/// works. Returns the service's container name.
fn compose_up(
    dir: &Path,
    sandbox: &ResolvedSandbox,
    files: &StringOrList,
    project: &str,
) -> Result<String> {
    let props = &sandbox.properties;
    let service = props.service.as_deref().with_context(|| {
        format!("sandbox `{}`: `service` is required with `dockerComposeFile`", sandbox.name)
    })?;

    let mut args: Vec<String> = vec!["compose".into()];
    for file in files.to_vec() {
        args.push("-f".into());
        args.push(dir.join(file).to_string_lossy().into_owned());
    }
    args.extend(["-p".into(), project.into(), "up".into(), "-d".into(), service.into()]);
    if let Some(run_services) = &props.run_services {
        args.extend(run_services.iter().cloned());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    docker::run_checked(&arg_refs)?;

    // Resolve the service's container name for exec / vscode attach.
    let ids = docker::output(&["compose", "-p", project, "ps", "-q", service])?;
    let id = ids
        .lines()
        .next()
        .with_context(|| format!("no container found for compose service `{service}`"))?;
    let name = docker::output(&["inspect", "-f", "{{.Name}}", id])?;
    Ok(name.trim_start_matches('/').to_string())
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

/// Run a lifecycle command inside the container via docker exec.
fn exec_lifecycle(
    container: &str,
    workspace: &str,
    remote_env: Option<&BTreeMap<String, String>>,
    cmd: &LifecycleCommand,
) -> Result<()> {
    for argv in cmd.commands() {
        if argv.is_empty() {
            continue;
        }
        let mut args: Vec<String> = vec!["exec".into(), "-w".into(), workspace.into()];
        for (key, value) in remote_env.into_iter().flatten() {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        args.push(container.into());
        args.extend(argv);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        docker::run_checked(&arg_refs)?;
    }
    Ok(())
}

/// Register `customizations.vscode.extensions` with the Remote-Containers
/// extension by writing its per-container-name config file.
fn write_vscode_name_config(container: &str, extensions: &[String]) -> Result<()> {
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
        let contents = merged_name_config(existing.as_deref(), extensions)
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

/// Existing name-config JSON (if any) with `extensions` replaced.
fn merged_name_config(existing: Option<&str>, extensions: &[String]) -> Result<String> {
    let mut root = match existing {
        Some(contents) => {
            serde_json::from_str::<serde_json::Value>(contents).context("invalid JSON")?
        }
        None => serde_json::json!({}),
    };
    root.as_object_mut()
        .context("existing config is not a JSON object")?
        .insert("extensions".into(), serde_json::json!(extensions));
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
cache-folder = ".pnpm-store"

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
        let json = merged_name_config(None, &["a.b".into(), "c.d".into()]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["extensions"], serde_json::json!(["a.b", "c.d"]));
    }

    #[test]
    fn name_config_preserves_other_keys() {
        let existing = r#"{"settings": {"x": 1}, "extensions": ["old.ext"]}"#;
        let json = merged_name_config(Some(existing), &["new.ext".into()]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["extensions"], serde_json::json!(["new.ext"]));
        assert_eq!(parsed["settings"]["x"], 1);
    }
}
