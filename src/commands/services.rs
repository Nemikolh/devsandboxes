use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::{short_hash, Config, ResolvedService};
use crate::docker::{self, NAME_PREFIX};
use crate::state::State;

/// Short, stable id for a config root, scoping its network and services so two
/// projects never collide.
pub fn project_id(dir: &Path) -> Result<String> {
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", dir.display()))?;
    Ok(short_hash(&canonical.to_string_lossy())[..8].to_string())
}

pub fn network_name(project: &str) -> String {
    format!("{NAME_PREFIX}net-{project}")
}

pub fn service_container(project: &str, name: &str) -> String {
    format!("{NAME_PREFIX}svc-{project}-{name}")
}

/// Create the project network if it does not already exist.
pub fn ensure_network(network: &str) -> Result<()> {
    if docker::inspect(network, "{{.Id}}")?.is_some() {
        return Ok(());
    }
    docker::run_checked(&["network", "create", network])
}

/// Idempotently bring a shared service up: create it if absent, start it if
/// stopped, warn on config drift, and no-op if already running.
pub fn ensure_service(project: &str, network: &str, service: &ResolvedService) -> Result<()> {
    let container = service_container(project, &service.name);
    match docker::inspect(&container, "{{.State.Running}}")? {
        Some(running) => {
            let label = docker::inspect(
                &container,
                "{{index .Config.Labels \"devsandbox.config_hash\"}}",
            )?;
            if label.as_deref().unwrap_or_default() != service.config_hash {
                eprintln!(
                    "warning: service `{}` config changed since it started; other instances \
                     may be using it — `devsandbox gc` then re-run to recreate",
                    service.name
                );
            }
            if running != "true" {
                docker::run_checked(&["start", &container])?;
            }
        }
        None => {
            check_port_conflicts(&container, &service.spec.ports)?;
            let args = service_run_args(project, network, service);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            docker::run_checked(&refs)?;
        }
    }
    Ok(())
}

/// Build the `docker run` argv for a service container. Pure, so it can be
/// tested without invoking docker.
pub fn service_run_args(project: &str, network: &str, service: &ResolvedService) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        service_container(project, &service.name),
        "--label".into(),
        format!("devsandbox.service={}", service.name),
        "--label".into(),
        format!("devsandbox.project={project}"),
        "--label".into(),
        format!("devsandbox.config_hash={}", service.config_hash),
        "--network".into(),
        network.to_string(),
        // Alias = service name, so sandboxes reach it as `<name>:<port>`.
        "--network-alias".into(),
        service.name.clone(),
    ];
    for (key, value) in &service.spec.env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    for port in &service.spec.ports {
        args.push("-p".into());
        args.push(port.clone());
    }
    args.push(service.spec.image.clone());
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

/// Ensure the project network exists and every service the sandbox declares is
/// up. Returns the network name when any service is declared, so the caller can
/// join the sandbox container to it.
pub fn ensure_services(
    config: &Config,
    project: &str,
    service_names: &[String],
) -> Result<Option<String>> {
    if service_names.is_empty() {
        return Ok(None);
    }
    let network = network_name(project);
    ensure_network(&network)?;
    for name in service_names {
        let service = config.resolve_service(name)?;
        ensure_service(project, &network, &service)?;
    }
    Ok(Some(network))
}

/// Stop and remove service containers of this project that no live sandbox
/// instance references. Refcounting is derived from state + docker, never
/// stored. Removes the network too when nothing is left using it.
pub fn gc(dir: &Path) -> Result<()> {
    let project = project_id(dir)?;
    let config = Config::load(dir).unwrap_or_default();
    let state = State::load()?;

    // Services still referenced by a live instance of this config root.
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut live_instances = false;
    for instance in state.instances.values() {
        if !config.sandboxes.contains_key(&instance.sandbox) {
            continue; // belongs to a different config root
        }
        if !container_running(&instance.container) {
            continue;
        }
        live_instances = true;
        if let Ok(sandbox) = config.resolve_sandbox(&instance.sandbox) {
            if let Some(services) = &sandbox.properties.services {
                referenced.extend(services.iter().cloned());
            }
        }
    }

    let filter = format!("label=devsandbox.project={project}");
    let listing = docker::output(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"])?;
    let mut removed = 0;
    for container in listing.lines().filter(|l| !l.is_empty()) {
        let service = docker::inspect(container, "{{index .Config.Labels \"devsandbox.service\"}}")?
            .unwrap_or_default();
        if referenced.contains(&service) {
            continue;
        }
        docker::run_checked(&["rm", "-f", container])?;
        println!("removed service {container}");
        removed += 1;
    }

    // Drop the network once no service and no live instance need it.
    if !live_instances {
        let network = network_name(&project);
        let _ = docker::run_inherit(&["network", "rm", &network]);
    }
    if removed == 0 {
        println!("no unused services to remove");
    }
    Ok(())
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
        assert_eq!(service_container("abc123", "db"), "devsandbox-svc-abc123-db");
    }

    #[test]
    fn service_run_args_wire_network_alias_env_ports() {
        let svc = service(
            r#"
[services.db]
image = "postgres:16"
env = { POSTGRES_PASSWORD = "x" }
ports = ["5432:5432"]
"#,
        );
        let args = service_run_args("proj", "devsandbox-net-proj", &svc);
        let joined = args.join(" ");
        assert_eq!(args[0], "run");
        assert!(joined.contains("--name devsandbox-svc-proj-db"));
        assert!(joined.contains("--network devsandbox-net-proj"));
        assert!(joined.contains("--network-alias db"));
        assert!(joined.contains("-e POSTGRES_PASSWORD=x"));
        assert!(joined.contains("-p 5432:5432"));
        assert!(joined.contains("devsandbox.service=db"));
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
