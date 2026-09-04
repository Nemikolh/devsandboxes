use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    let instance = match instance_name {
        Some(name) => {
            if state.instances.contains_key(&name) {
                bail!("instance `{name}` already exists");
            }
            name
        }
        None => format!("{sandbox_name}-{}", suffix()),
    };
    let container_name = format!("{NAME_PREFIX}{instance}");

    // initializeCommand runs on the host, before anything is created.
    if let Some(cmd) = &props.initialize_command {
        run_host_commands(dir, cmd).context("initializeCommand failed")?;
    }

    let container = match &props.docker_compose_file {
        Some(files) => compose_up(dir, &sandbox, files, &container_name)?,
        None => {
            docker_run(dir, &sandbox, &container_name, &folder, &workspace)?;
            container_name.clone()
        }
    };

    state.instances.insert(
        instance.clone(),
        Instance {
            sandbox: sandbox_name,
            container: container.clone(),
            folder,
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

fn docker_run(
    dir: &Path,
    sandbox: &ResolvedSandbox,
    container: &str,
    folder: &Path,
    workspace: &str,
) -> Result<()> {
    let image = image_for(dir, sandbox)?;
    let mount = format!("{}:{workspace}", folder.display());
    let sandbox_label = format!("devsandbox.sandbox={}", sandbox.name);
    let instance = container.strip_prefix(NAME_PREFIX).unwrap_or(container);
    let instance_label = format!("devsandbox.instance={instance}");
    let mut args: Vec<String> = [
        "run", "-d", "--name", container,
        "--label", &sandbox_label, "--label", &instance_label,
        "-v", &mount, "-w", workspace,
    ]
    .map(str::to_string)
    .into();
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
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("cannot determine config dir ($XDG_CONFIG_HOME or $HOME)")?;
    let dir = base.join("Code/User/globalStorage/ms-vscode-remote.remote-containers/nameConfigs");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(format!("{container}.json"));
    let existing = match std::fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let contents = merged_name_config(existing.as_deref(), extensions)
        .with_context(|| format!("in {}", path.display()))?;
    std::fs::write(&path, contents).with_context(|| format!("cannot write {}", path.display()))
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

/// Short unique-enough suffix for generated instance names.
fn suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs() * 1_000_000_000)
        .unwrap_or(0);
    let mut n = nanos % 36u64.pow(4);
    let mut out = String::new();
    for _ in 0..4 {
        out.push(char::from_digit((n % 36) as u32, 36).unwrap());
        n /= 36;
    }
    out
}

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
