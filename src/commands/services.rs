use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::{short_hash, Config, ResolvedService, ServiceScope};
use crate::docker::{self, NAME_PREFIX};
use crate::state::State;

/// Short, stable id for a config root, scoping its networks and services so two
/// projects never collide.
pub fn project_id(dir: &Path) -> Result<String> {
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", dir.display()))?;
    Ok(short_hash(&canonical.to_string_lossy())[..8].to_string())
}

/// Shared network for a config root's `global` services.
pub fn network_name(project: &str) -> String {
    format!("{NAME_PREFIX}net-{project}")
}

/// Per-instance network for a config root's `isolated` services.
pub fn instance_network(project: &str, instance: &str) -> String {
    format!("{NAME_PREFIX}net-{project}-{instance}")
}

/// Container name for a `global` service (one per config root).
pub fn service_container(project: &str, name: &str) -> String {
    format!("{NAME_PREFIX}svc-{project}-{name}")
}

/// Container name for an `isolated` service (one per sandbox instance).
pub fn isolated_service_container(project: &str, instance: &str, name: &str) -> String {
    format!("{NAME_PREFIX}svc-{project}-{instance}-{name}")
}

/// Create the given network if it does not already exist.
pub fn ensure_network(network: &str) -> Result<()> {
    if docker::inspect(network, "{{.Id}}")?.is_some() {
        return Ok(());
    }
    docker::run_checked(&["network", "create", network])
}

/// Ensure the networks exist and every service the sandbox declares is up:
/// `global` services on the shared network, `isolated` ones on a per-instance
/// network. Returns the networks the instance container must join.
pub fn ensure_services(
    config: &Config,
    dir: &Path,
    project: &str,
    instance: &str,
    service_names: &[String],
) -> Result<Vec<String>> {
    if service_names.is_empty() {
        return Ok(Vec::new());
    }
    let global_net = network_name(project);
    let instance_net = instance_network(project, instance);
    let (mut used_global, mut used_isolated) = (false, false);

    for name in service_names {
        let service = config.resolve_service(name)?;
        match service.spec.scope {
            ServiceScope::Global => {
                if !used_global {
                    ensure_network(&global_net)?;
                    used_global = true;
                }
                let container = service_container(project, name);
                let labels = [
                    format!("devsandbox.service={name}"),
                    format!("devsandbox.project={project}"),
                    "devsandbox.scope=global".to_string(),
                    format!("devsandbox.config_hash={}", service.config_hash),
                ];
                ensure_service(dir, &container, &global_net, &service, &labels)?;
            }
            ServiceScope::Isolated => {
                if !used_isolated {
                    ensure_network(&instance_net)?;
                    used_isolated = true;
                }
                let container = isolated_service_container(project, instance, name);
                let labels = [
                    format!("devsandbox.service={name}"),
                    format!("devsandbox.project={project}"),
                    format!("devsandbox.instance={instance}"),
                    "devsandbox.scope=isolated".to_string(),
                    format!("devsandbox.config_hash={}", service.config_hash),
                ];
                ensure_service(dir, &container, &instance_net, &service, &labels)?;
            }
        }
    }

    let mut networks = Vec::new();
    if used_global {
        networks.push(global_net);
    }
    if used_isolated {
        networks.push(instance_net);
    }
    Ok(networks)
}

/// Idempotently bring a service container up: create it if absent, start it if
/// stopped, warn on config drift, and no-op if already running.
fn ensure_service(
    dir: &Path,
    container: &str,
    network: &str,
    service: &ResolvedService,
    labels: &[String],
) -> Result<()> {
    match docker::inspect(container, "{{.State.Running}}")? {
        Some(running) => {
            let label = docker::inspect(
                container,
                "{{index .Config.Labels \"devsandbox.config_hash\"}}",
            )?;
            if label.as_deref().unwrap_or_default() != service.config_hash {
                eprintln!(
                    "warning: service `{}` ({container}) config changed since it started; \
                     `devsandbox gc` (or remove the instance) then re-run to recreate",
                    service.name
                );
            }
            if running != "true" {
                docker::run_checked(&["start", container])?;
            }
        }
        None => {
            check_port_conflicts(container, &service.spec.ports)?;
            let image = service_image(dir, service)?;
            let args = service_run_args(container, network, &image, service, labels);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            docker::run_checked(&refs)?;
        }
    }
    Ok(())
}

/// Resolve a service to a runnable image: its `image` as-is, or a local build
/// of its `build.dockerfile` (built once, tagged by service name).
fn service_image(dir: &Path, service: &ResolvedService) -> Result<String> {
    if let Some(image) = &service.spec.image {
        return Ok(image.clone());
    }
    let build = service
        .spec
        .build
        .as_ref()
        .expect("resolve_service validates image xor build");
    let dockerfile = build
        .dockerfile
        .as_deref()
        .with_context(|| format!("service `{}`: `build.dockerfile` is required", service.name))?;
    let context = build.context.as_deref().unwrap_or(".");
    let tag = format!("{NAME_PREFIX}img-svc-{}", service.name);

    let mut args: Vec<String> = vec![
        "build".into(),
        "-t".into(),
        tag.clone(),
        "-f".into(),
        dir.join(dockerfile).to_string_lossy().into_owned(),
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

    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    docker::run_checked(&refs)?;
    Ok(tag)
}

/// Build the `docker run` argv for a service container. Pure, so it can be
/// tested without invoking docker.
pub fn service_run_args(
    container: &str,
    network: &str,
    image: &str,
    service: &ResolvedService,
    labels: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), container.into()];
    for label in labels {
        args.push("--label".into());
        args.push(label.clone());
    }
    args.push("--network".into());
    args.push(network.to_string());
    // Alias = service name, so sandboxes reach it as `<name>:<port>`.
    args.push("--network-alias".into());
    args.push(service.name.clone());
    for (key, value) in &service.spec.env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    for port in &service.spec.ports {
        args.push("-p".into());
        args.push(port.clone());
    }
    args.push(image.to_string());
    if let Some(command) = &service.spec.command {
        for part in command.to_vec() {
            args.push(part.to_string());
        }
    }
    args
}

/// Host port of a `-p` spec (`8080:80`, `127.0.0.1:8080:80`), or None when the
/// host port is left to docker (`80`).
fn host_port(spec: &str) -> Option<&str> {
    let parts: Vec<&str> = spec.split(':').collect();
    (parts.len() >= 2).then(|| parts[parts.len() - 2])
}

/// Fail before creating a service if another running container already
/// publishes one of its host ports (best-effort; docker is the final arbiter).
fn check_port_conflicts(container: &str, ports: &[String]) -> Result<()> {
    for spec in ports {
        let Some(port) = host_port(spec) else { continue };
        let filter = format!("publish={port}");
        let holder = docker::output(&["ps", "--filter", &filter, "--format", "{{.Names}}"])?;
        let other = holder.lines().find(|name| !name.is_empty() && *name != container);
        if let Some(other) = other {
            bail!(
                "host port {port} (service `{container}`) is already published by `{other}`; \
                 change the service `ports` or stop the other container"
            );
        }
    }
    Ok(())
}

/// Stop and remove service containers of this project that no live sandbox
/// instance references, and networks nothing needs. Refcounting is derived from
/// state + docker, never stored.
pub fn gc(dir: &Path) -> Result<()> {
    let project = project_id(dir)?;
    let config = Config::load(dir).unwrap_or_default();
    let state = State::load()?;

    // What live instances of this config root still reference: global service
    // names, and per-instance isolated service names.
    let mut global_refs: BTreeSet<String> = BTreeSet::new();
    let mut isolated_refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut live_instances: BTreeSet<String> = BTreeSet::new();
    for (name, instance) in &state.instances {
        if !config.sandboxes.contains_key(&instance.sandbox) {
            continue; // belongs to a different config root
        }
        if !container_running(&instance.container) {
            continue;
        }
        live_instances.insert(name.clone());
        let Ok(sandbox) = config.resolve_sandbox(&instance.sandbox) else {
            continue;
        };
        let Some(services) = &sandbox.properties.services else {
            continue;
        };
        for service in services {
            match config.resolve_service(service).map(|s| s.spec.scope) {
                Ok(ServiceScope::Global) => {
                    global_refs.insert(service.clone());
                }
                Ok(ServiceScope::Isolated) => {
                    isolated_refs.entry(name.clone()).or_default().insert(service.clone());
                }
                Err(_) => {}
            }
        }
    }

    let filter = format!("label=devsandbox.project={project}");
    let listing = docker::output(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"])?;
    let mut removed = 0;
    for container in listing.lines().filter(|l| !l.is_empty()) {
        let scope = label(container, "devsandbox.scope")?;
        let service = label(container, "devsandbox.service")?;
        let keep = match scope.as_str() {
            "global" => global_refs.contains(&service),
            "isolated" => {
                let instance = label(container, "devsandbox.instance")?;
                isolated_refs.get(&instance).is_some_and(|s| s.contains(&service))
            }
            _ => false,
        };
        if keep {
            continue;
        }
        docker::run_checked(&["rm", "-f", container])?;
        println!("removed service {container}");
        removed += 1;
    }

    // Drop networks nothing needs: the shared net once no live instance uses a
    // global service, and each instance net whose instance is gone.
    let global_net = network_name(&project);
    let nets = docker::output(&["network", "ls", "--filter", &format!("name={global_net}"), "--format", "{{.Name}}"])?;
    for net in nets.lines().filter(|l| !l.is_empty()) {
        let drop = if net == global_net {
            global_refs.is_empty()
        } else if let Some(instance) = net.strip_prefix(&format!("{global_net}-")) {
            !live_instances.contains(instance)
        } else {
            false
        };
        if drop {
            let _ = docker::run_inherit(&["network", "rm", net]);
        }
    }

    if removed == 0 {
        println!("no unused services to remove");
    }
    Ok(())
}

fn label(container: &str, key: &str) -> Result<String> {
    Ok(docker::inspect(container, &format!("{{{{index .Config.Labels \"{key}\"}}}}"))?
        .unwrap_or_default())
}

fn container_running(container: &str) -> bool {
    docker::inspect(container, "{{.State.Running}}")
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn service(toml: &str) -> ResolvedService {
        Config::parse(toml).unwrap().resolve_service("db").unwrap()
    }

    #[test]
    fn names_are_deterministic_and_scoped() {
        assert_eq!(network_name("abc123"), "devsandbox-net-abc123");
        assert_eq!(instance_network("abc123", "repo-2"), "devsandbox-net-abc123-repo-2");
        assert_eq!(service_container("abc123", "db"), "devsandbox-svc-abc123-db");
        assert_eq!(
            isolated_service_container("abc123", "repo-2", "db"),
            "devsandbox-svc-abc123-repo-2-db"
        );
    }

    #[test]
    fn service_run_args_wire_network_alias_env_ports_labels() {
        let svc = service(
            r#"
[services.db]
image = "postgres:16"
env = { POSTGRES_PASSWORD = "x" }
ports = ["5432:5432"]
"#,
        );
        let labels = ["devsandbox.service=db".to_string(), "devsandbox.scope=isolated".to_string()];
        let args = service_run_args(
            "devsandbox-svc-proj-repo-db",
            "devsandbox-net-proj-repo",
            "postgres:16",
            &svc,
            &labels,
        );
        let joined = args.join(" ");
        assert_eq!(args[0], "run");
        assert!(joined.contains("--name devsandbox-svc-proj-repo-db"));
        assert!(joined.contains("--network devsandbox-net-proj-repo"));
        assert!(joined.contains("--network-alias db"));
        assert!(joined.contains("-e POSTGRES_PASSWORD=x"));
        assert!(joined.contains("-p 5432:5432"));
        assert!(joined.contains("--label devsandbox.scope=isolated"));
        // Image is the final positional argument.
        assert_eq!(args.last().unwrap(), "postgres:16");
    }

    #[test]
    fn host_port_parses_forms() {
        assert_eq!(host_port("8080:80"), Some("8080"));
        assert_eq!(host_port("127.0.0.1:8080:80"), Some("8080"));
        assert_eq!(host_port("80"), None);
    }
}
