use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::container_drifted;
use crate::commands::status::Envelope;
use crate::config::{build_hash, short_hash, Build, Config, ResolvedService, ServiceScope};
use crate::render::{bold_cyan, dim, green, magenta, yellow};
use crate::runtime::{backend, Backend, ServiceEndpoint, NAME_PREFIX};
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
    if backend().network_exists(network)? {
        return Ok(());
    }
    backend()
        .run_checked(&["network", "create", network])
        .with_context(|| format!("cannot create network `{network}` (services need runtime network support)"))
}

/// Ensure the networks exist and every service the sandbox declares is up:
/// `global` services on the shared network, `isolated` ones on a per-instance
/// network. `instance_id` (the persistent id, not the display name) names and
/// labels the isolated containers/network so they survive renames. Returns the
/// networks the instance container must join and the endpoints it must be able
/// to resolve by service name.
pub fn ensure_services(
    config: &Config,
    dir: &Path,
    project: &str,
    instance_id: &str,
    service_names: &[String],
) -> Result<(Vec<String>, Vec<ServiceEndpoint>)> {
    let mut endpoints = Vec::with_capacity(service_names.len());
    if service_names.is_empty() {
        return Ok((Vec::new(), endpoints));
    }
    let global_net = network_name(project);
    let instance_net = instance_network(project, instance_id);
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
                    format!(
                        "devsandbox.build_hash={}",
                        build_hash(dir, service.spec.build.as_ref())
                    ),
                ];
                ensure_service(dir, &container, &global_net, &service, &labels)?;
                endpoints.push(ServiceEndpoint { alias: name.clone(), container });
            }
            ServiceScope::Isolated => {
                if !used_isolated {
                    ensure_network(&instance_net)?;
                    used_isolated = true;
                }
                let container = isolated_service_container(project, instance_id, name);
                let labels = [
                    format!("devsandbox.service={name}"),
                    format!("devsandbox.project={project}"),
                    format!("devsandbox.instance={instance_id}"),
                    "devsandbox.scope=isolated".to_string(),
                    format!("devsandbox.config_hash={}", service.config_hash),
                    format!(
                        "devsandbox.build_hash={}",
                        build_hash(dir, service.spec.build.as_ref())
                    ),
                ];
                ensure_service(dir, &container, &instance_net, &service, &labels)?;
                endpoints.push(ServiceEndpoint { alias: name.clone(), container });
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
    Ok((networks, endpoints))
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
    match backend().is_running(container)? {
        Some(running) => {
            let build = build_hash(dir, service.spec.build.as_ref());
            if container_drifted(container, &service.config_hash, &build)? {
                eprintln!(
                    "warning: service `{}` ({container}) config changed since it started; \
                     `devsandbox gc` (or remove the instance) then re-run to recreate",
                    service.name
                );
            }
            if !running {
                backend().run_checked(&["start", container])?;
            }
        }
        None => {
            let ports = prefer_same_host_ports(container, &service.spec.ports)?;
            check_port_conflicts(container, &ports)?;
            let image = service_image(dir, service)?;
            let args = service_run_args(backend(), container, network, &image, service, &ports, labels);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            backend().run_checked(&refs)?;
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
    let tag = format!("{NAME_PREFIX}img-svc-{}", service.name);

    let args = build_args(backend(), dir, &tag, dockerfile, build, &format!("service `{}`", service.name));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    backend().run_checked(&refs)?;
    Ok(tag)
}

/// `build` argv for a `build` block: `-t tag -f dockerfile [--build-arg …]
/// [--target …] [--cache-from …] [options…] context`. `--cache-from` is dropped
/// with a warning on runtimes without it (`what` names the owner in the
/// message). Shared with sandbox image builds.
pub fn build_args(
    rt: &dyn Backend,
    dir: &Path,
    tag: &str,
    dockerfile: &str,
    build: &Build,
    what: &str,
) -> Vec<String> {
    let context = build.context.as_deref().unwrap_or(".");
    let mut args: Vec<String> = vec![
        "build".into(),
        "-t".into(),
        tag.to_string(),
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
        if rt.supports_cache_from() {
            for image in cache_from.to_vec() {
                args.extend(["--cache-from".into(), image.to_string()]);
            }
        } else {
            eprintln!(
                "warning: {what}: `build.cacheFrom` is not supported by {}; ignoring",
                rt.name()
            );
        }
    }
    if let Some(options) = &build.options {
        args.extend(options.iter().cloned());
    }
    args.push(dir.join(context).to_string_lossy().into_owned());
    args
}

/// Build the `run` argv for a service container. Pure apart from the
/// backend's alias flag, so it can be tested without a runtime.
pub fn service_run_args(
    rt: &dyn Backend,
    container: &str,
    network: &str,
    image: &str,
    service: &ResolvedService,
    ports: &[String],
    labels: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), container.into()];
    for label in labels {
        args.push("--label".into());
        args.push(label.clone());
    }
    args.push("--network".into());
    args.push(network.to_string());
    // Alias = service name, so sandboxes reach it as `<name>:<port>`. Runtimes
    // without aliases wire it into the sandbox's /etc/hosts instead.
    args.extend(rt.service_alias_args(&service.name));
    for (key, value) in &service.spec.env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    for port in ports {
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

/// A `-p` spec that leaves the host port to the runtime: `5432`, `5432/udp`,
/// `127.0.0.1::5432`. Ranges and other shapes aren't bare.
#[derive(Debug, PartialEq)]
struct BarePort<'a> {
    ip: Option<&'a str>,
    port: u16,
    proto: Option<&'a str>,
}

impl BarePort<'_> {
    fn parse(spec: &str) -> Option<BarePort<'_>> {
        let (body, proto) = match spec.split_once('/') {
            Some((body, proto)) => (body, Some(proto)),
            None => (spec, None),
        };
        if !matches!(proto, None | Some("tcp") | Some("udp")) {
            return None;
        }
        let parts: Vec<&str> = body.split(':').collect();
        let (ip, port) = match parts[..] {
            [port] => (None, port),
            [ip, "", port] if !ip.is_empty() => (Some(ip), port),
            _ => return None,
        };
        let port = port.parse().ok().filter(|p| *p != 0)?;
        Some(BarePort { ip, port, proto })
    }

    fn udp(&self) -> bool {
        self.proto == Some("udp")
    }

    /// The spec with the host port pinned to the container port.
    fn pinned(&self) -> String {
        let ip = self.ip.map(|ip| format!("{ip}:")).unwrap_or_default();
        let proto = self.proto.map(|p| format!("/{p}")).unwrap_or_default();
        format!("{ip}{0}:{0}{proto}", self.port)
    }
}

/// Rewrite bare `-p` specs so the host port matches the container port when
/// it's free (not published by another container, not bound on the host);
/// otherwise leave the spec bare and let the runtime pick one. Nobody wants
/// `5432` to land on a random host port when `5432` was available.
fn prefer_same_host_ports(container: &str, ports: &[String]) -> Result<Vec<String>> {
    if !ports.iter().any(|p| BarePort::parse(p).is_some()) {
        return Ok(ports.to_vec());
    }
    let running = backend().list(false, "")?;
    let published: BTreeSet<&str> = running
        .iter()
        .filter(|r| r.name != container)
        .flat_map(|r| r.host_ports.iter().map(String::as_str))
        .collect();
    Ok(ports
        .iter()
        .map(|spec| match BarePort::parse(spec) {
            Some(bare) if !published.contains(bare.port.to_string().as_str()) && host_port_free(&bare) => {
                bare.pinned()
            }
            _ => spec.clone(),
        })
        .collect())
}

/// Best-effort probe that nothing on the host holds `bare`'s port. std's
/// `TcpListener` sets `SO_REUSEADDR`, which on macOS lets a wildcard bind
/// coexist with a loopback-only listener, so TCP also tries to connect. Any
/// error (in use, privileged port) counts as taken; the runtime has the final
/// say either way.
fn host_port_free(bare: &BarePort) -> bool {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
    let ip: IpAddr = match bare.ip {
        Some(ip) => match ip.parse() {
            Ok(ip) => ip,
            Err(_) => return false,
        },
        None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    };
    let addr = SocketAddr::new(ip, bare.port);
    if bare.udp() {
        return UdpSocket::bind(addr).is_ok();
    }
    if TcpListener::bind(addr).is_err() {
        return false;
    }
    let target = if ip.is_unspecified() { IpAddr::V4(Ipv4Addr::LOCALHOST) } else { ip };
    TcpStream::connect_timeout(&SocketAddr::new(target, bare.port), std::time::Duration::from_millis(200)).is_err()
}

/// Fail before creating a service if another running container already
/// publishes one of its host ports (best-effort; the runtime is the final
/// arbiter).
fn check_port_conflicts(container: &str, ports: &[String]) -> Result<()> {
    let wanted: Vec<&str> = ports.iter().filter_map(|p| host_port(p)).collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let running = backend().list(false, "")?;
    for port in wanted {
        let other = running
            .iter()
            .find(|r| r.name != container && r.host_ports.iter().any(|p| p == port));
        if let Some(other) = other.map(|r| &r.name) {
            bail!(
                "host port {port} (service `{container}`) is already published by `{other}`; \
                 change the service `ports` or stop the other container"
            );
        }
    }
    Ok(())
}

/// Which services of this config root the given running instances reference,
/// split by scope. Pure over `(config, state, running)` — the impure
/// `container_running` liveness check lives at the call sites — so `gc` and
/// `rebuild` derive references the same way and it stays unit-testable without a
/// runtime. `running` is the set of instance keys whose container is up.
///
/// Isolated refs and live instances are keyed by the *persistent id*, not the
/// state key: service containers, labels, and networks all carry the id, so a
/// renamed instance still owns its services.
///
/// - `global_refs`: global service names any running instance references (one
///   shared container regardless of how many reference it).
/// - `isolated_refs`: isolated service names per running instance id.
/// - `live_instances`: ids of running instances belonging to this config root.
fn service_refs(config: &Config, state: &State, running: &BTreeSet<String>) -> ServiceRefs {
    let mut refs = ServiceRefs::default();
    for (name, instance) in &state.instances {
        if !config.sandboxes.contains_key(&instance.sandbox) {
            continue; // belongs to a different config root
        }
        if !running.contains(name) {
            continue;
        }
        refs.live_instances.insert(instance.instance_id.clone());
        let Ok(sandbox) = config.resolve_sandbox(&instance.sandbox) else {
            continue;
        };
        let Some(services) = &sandbox.properties.services else {
            continue;
        };
        for service in services {
            match config.resolve_service(service).map(|s| s.spec.scope) {
                Ok(ServiceScope::Global) => {
                    refs.global.insert(service.clone());
                }
                Ok(ServiceScope::Isolated) => {
                    refs.isolated
                        .entry(instance.instance_id.clone())
                        .or_default()
                        .insert(service.clone());
                }
                Err(_) => {}
            }
        }
    }
    refs
}

/// The service references a set of running instances hold, keyed by scope.
#[derive(Default)]
struct ServiceRefs {
    /// Global service names referenced by any running instance.
    global: BTreeSet<String>,
    /// Isolated service names per running instance id.
    isolated: BTreeMap<String, BTreeSet<String>>,
    /// Persistent ids of running instances belonging to this config root.
    live_instances: BTreeSet<String>,
}

/// Stop and remove service containers of this project that no live sandbox
/// instance references, and networks nothing needs. Refcounting is derived from
/// state + the runtime, never stored. Also reaps orphaned managed shell-history
/// files (confirmed per file unless `force`).
pub fn gc(dir: &Path, force: bool) -> Result<()> {
    let project = project_id(dir)?;
    let config = Config::load(dir).unwrap_or_default();
    let state = State::load()?;

    // What live instances of this config root still reference, split by scope.
    let running: BTreeSet<String> = state
        .instances
        .iter()
        .filter(|(_, i)| container_running(&i.container))
        .map(|(name, _)| name.clone())
        .collect();
    let ServiceRefs { global: global_refs, isolated: isolated_refs, live_instances } =
        service_refs(&config, &state, &running);

    let mut removed = 0;
    for row in backend().list(true, NAME_PREFIX)? {
        if row.label("devsandbox.project") != Some(project.as_str()) {
            continue;
        }
        let service = row.label("devsandbox.service").unwrap_or_default();
        let keep = match row.label("devsandbox.scope") {
            Some("global") => global_refs.contains(service),
            Some("isolated") => {
                let instance = row.label("devsandbox.instance").unwrap_or_default();
                isolated_refs.get(instance).is_some_and(|s| s.contains(service))
            }
            _ => false,
        };
        if keep {
            continue;
        }
        let container = &row.name;
        if backend().remove_force(container)? != 0 {
            bail!("{} rm {container} failed", backend().name());
        }
        println!("removed service {container}");
        removed += 1;
    }

    // Drop networks nothing needs: the shared net once no live instance uses a
    // global service, and each instance net whose instance is gone.
    let global_net = network_name(&project);
    let nets = backend().output(&["network", "ls", "-q"])?;
    for net in nets.lines().map(str::trim).filter(|l| l.starts_with(global_net.as_str())) {
        let drop = if net == global_net {
            global_refs.is_empty()
        } else if let Some(instance) = net.strip_prefix(&format!("{global_net}-")) {
            !live_instances.contains(instance)
        } else {
            false
        };
        if drop {
            let _ = backend().run_inherit(&["network", "rm", net]);
        }
    }

    if removed == 0 {
        println!("no unused services to remove");
    }

    gc_shell_history(dir, &state, force)?;
    gc_agent_links(&state)?;
    Ok(())
}

/// Recreate a service's container(s) from the current config and rewire every
/// running sandbox that references it — no sandbox restart on any runtime.
///
/// Recreating (rather than restarting) is what applies dockerfile edits: the
/// create path in `ensure_services` re-runs `docker build`, whose cache picks up
/// the changed dockerfile, and stamps fresh labels. A recreated container gets a
/// new address, so referencing sandboxes must be re-pointed at it. On
/// docker/podman that is free — the service keeps its network alias, so DNS
/// resolves the new container with no action (`wire_service_dns` is a no-op
/// there). On Apple, aliases don't exist and the sandbox's `/etc/hosts` holds a
/// baked address, so `wire_service_dns` rewrites that file in place on the live
/// container. Either way the sandbox never restarts. Stopped sandboxes are
/// rewired by their next `start`.
pub fn rebuild(dir: &Path, name: &str) -> Result<()> {
    let project = project_id(dir)?;
    let config = Config::load(dir)?;
    let service = config.resolve_service(name)?; // unknown service bails here
    let state = State::load()?;

    // Remove every backing container of this project+service; the referencing
    // instances (or the global arm below) recreate them fresh.
    let mut removed = 0;
    for row in backend().list(true, NAME_PREFIX)? {
        if row.label("devsandbox.project") != Some(project.as_str())
            || row.label("devsandbox.service") != Some(name)
        {
            continue;
        }
        let container = &row.name;
        if backend().remove_force(container)? != 0 {
            bail!("{} rm {container} failed", backend().name());
        }
        println!("removed {container}");
        removed += 1;
    }

    // Recreate on each running instance that references the service, then rewire
    // that live container to the new backing container.
    let running: BTreeSet<String> = state
        .instances
        .iter()
        .filter(|(_, i)| container_running(&i.container))
        .map(|(name, _)| name.clone())
        .collect();
    let refs = service_refs(&config, &state, &running);
    let names = [name.to_string()];
    let mut recreated = 0;

    // Iterate state (not `live_instances`) so both the display key (messages)
    // and the persistent id (service names/labels) are at hand.
    for (key, inst) in &state.instances {
        if !refs.live_instances.contains(&inst.instance_id) {
            continue;
        }
        let referenced = match service.spec.scope {
            ServiceScope::Global => refs.global.contains(name),
            ServiceScope::Isolated => {
                refs.isolated.get(&inst.instance_id).is_some_and(|s| s.contains(name))
            }
        };
        if !referenced {
            continue;
        }
        let container = &inst.container;
        let (_, endpoints) = ensure_services(&config, dir, &project, &inst.instance_id, &names)?;
        backend()
            .wire_service_dns(container, &endpoints)
            .with_context(|| format!("rewiring service `{name}` into `{key}` failed"))?;
        println!("recreated {container} (wired into {key})");
        recreated += 1;
    }

    // A global service with no running referencer still has one shared container;
    // recreate it once on the global network (mirrors the `Global` arm of
    // `ensure_services`) so `rebuild` applies drift even with nothing to wire.
    if service.spec.scope == ServiceScope::Global && !refs.global.contains(name) {
        let global_net = network_name(&project);
        ensure_network(&global_net)?;
        let container = service_container(&project, name);
        let labels = [
            format!("devsandbox.service={name}"),
            format!("devsandbox.project={project}"),
            "devsandbox.scope=global".to_string(),
            format!("devsandbox.config_hash={}", service.config_hash),
            format!("devsandbox.build_hash={}", build_hash(dir, service.spec.build.as_ref())),
        ];
        ensure_service(dir, &container, &global_net, &service, &labels)?;
        println!("recreated {container}");
        recreated += 1;
    }

    if removed == 0 && recreated == 0 {
        // Isolated service with no running instances: containers exist only
        // per instance, so there was nothing to remove or recreate.
        println!("no containers for service `{name}`");
    }
    Ok(())
}

const LS_HEADERS: [&str; 6] = ["NAME", "SCOPE", "SOURCE", "PORTS", "USED BY", "STATUS"];

/// List the services defined in this config root, mirroring `commands::ls` for
/// sandboxes. Shares `snapshot::service_rows` with the TUI so the two views
/// cannot drift. Docker being down is data, not failure: the runtime error goes
/// to stderr and rows still render (statuses `missing`), matching the snapshot.
pub fn ls(dir: &Path, json: bool) -> Result<()> {
    let config = Config::load(dir)?;
    let project = project_id(dir)?;

    // Container statuses come from one listing; a runtime that is down leaves an
    // empty ps so every status renders `missing` rather than aborting the command.
    let ps = match backend().list(true, NAME_PREFIX) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("{} unavailable: {e:#}", backend().name());
            Vec::new()
        }
    };

    // Which services each instance of this config root references, derived from
    // state+config directly (no full snapshot): every state instance whose
    // sandbox resolves in this config, paired with that sandbox's service list.
    // Same config-root filter as `service_refs`.
    let state = State::load()?;
    let instance_services: Vec<(String, String, Vec<String>)> = state
        .instances
        .iter()
        .filter(|(_, inst)| config.sandboxes.contains_key(&inst.sandbox))
        .map(|(name, inst)| {
            let services = config
                .resolve_sandbox(&inst.sandbox)
                .ok()
                .and_then(|sb| sb.properties.services.clone())
                .unwrap_or_default();
            (name.clone(), inst.instance_id.clone(), services)
        })
        .collect();

    let (rows, errors) =
        crate::snapshot::service_rows(dir, &project, &config, &instance_services, &ps);

    // JSON path mirrors `ls --json`: emit the same `ServiceRow`s the TUI has, an
    // empty config yields `data: []`, and resolve failures are ignored (the `?`
    // source row is the signal), not surfaced as errors.
    if json {
        let out = serde_json::to_string_pretty(&Envelope::new(rows))
            .context("serialize services")?;
        println!("{out}");
        return Ok(());
    }

    // Table path surfaces resolve failures on stderr (like the snapshot folds
    // them into `error`), but still prints the rows.
    for err in &errors {
        eprintln!("{err}");
    }

    if rows.is_empty() {
        println!("no services defined in {}/config.toml", dir.display());
        return Ok(());
    }

    // Plain cells (width math) and colored cells (display) kept in lockstep so
    // widths derive from visible text, not escapes — same shape as `commands::ls`.
    let mut plain: Vec<[String; 6]> = Vec::with_capacity(rows.len());
    let mut colored: Vec<[String; 6]> = Vec::with_capacity(rows.len());

    for row in &rows {
        let ports = if row.ports.is_empty() { None } else { Some(row.ports.join(", ")) };
        let used_by = if row.used_by.is_empty() { None } else { Some(row.used_by.join(", ")) };
        let status = ls_status_cell(&row.containers);

        plain.push([
            row.name.clone(),
            row.scope.to_string(),
            row.source.clone(),
            ports.clone().unwrap_or_else(|| "-".to_string()),
            used_by.clone().unwrap_or_else(|| "-".to_string()),
            status.clone(),
        ]);
        colored.push([
            bold_cyan(&row.name),
            row.scope.to_string(),
            ls_color_source(&row.source),
            match ports {
                Some(p) => magenta(&p),
                None => dim("-"),
            },
            match used_by {
                Some(u) => u,
                None => dim("-"),
            },
            ls_color_status(&status),
        ]);
    }

    let widths = ls_column_widths(&LS_HEADERS, &plain);
    print!("{}", ls_format_row(&LS_HEADERS.map(|h| dim(h)), &LS_HEADERS.map(str::to_string), &widths));
    for (colored_row, plain_row) in colored.iter().zip(&plain) {
        print!("{}", ls_format_row(colored_row, plain_row, &widths));
    }
    Ok(())
}

/// STATUS cell from a service's backing containers: `-` when it has none,
/// `missing` when none of them exist (never created, removed, or the runtime
/// is down — calling that `stopped` would overstate), `running`/`stopped` when
/// all existing ones share one state, else `<up>/<total> running`.
fn ls_status_cell(containers: &[(String, crate::snapshot::ContainerStatus)]) -> String {
    use crate::snapshot::ContainerStatus;
    if containers.is_empty() {
        return "-".to_string();
    }
    if containers.iter().all(|(_, s)| matches!(s, ContainerStatus::Missing)) {
        return "missing".to_string();
    }
    let running = containers
        .iter()
        .filter(|(_, s)| matches!(s, ContainerStatus::Running(_)))
        .count();
    let total = containers.len();
    if running == total {
        "running".to_string()
    } else if running == 0 {
        "stopped".to_string()
    } else {
        format!("{running}/{total} running")
    }
}

/// Color the SOURCE cell like `commands::ls::color_source`.
fn ls_color_source(source: &str) -> String {
    if source.starts_with("image ") {
        green(source)
    } else if source.starts_with("dockerfile ") {
        yellow(source)
    } else {
        source.to_string()
    }
}

/// Color the STATUS cell: green when fully running, dim when absent or never
/// created, else yellow.
fn ls_color_status(status: &str) -> String {
    match status {
        "running" => green(status),
        "-" | "missing" => dim(status),
        _ => yellow(status),
    }
}

/// Widest plain cell per column, header included (mirrors `commands::ls`).
fn ls_column_widths(headers: &[&str; 6], rows: &[[String; 6]]) -> [usize; 6] {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    widths
}

/// Render one row with a two-space gutter, padding colored cells to plain widths,
/// no trailing pad on the last column (mirrors `commands::ls`).
fn ls_format_row(colored: &[String; 6], plain: &[String; 6], widths: &[usize; 6]) -> String {
    let mut out = String::new();
    for i in 0..6 {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&colored[i]);
        if i < 5 {
            let pad = widths[i] - plain[i].len();
            out.extend(std::iter::repeat(' ').take(pad));
        }
    }
    out.push('\n');
    out
}

/// Delete `shared-volumes/history/<instance_id>/` dirs (and legacy flat
/// `<instance_id>.zsh_history` files from the pre-directory layout) whose id no
/// instance in state holds (any config root: ids are global, so a match
/// anywhere means the entry is still owned). Matched on the persistent id, not
/// the key, so a renamed instance keeps its history. Kept on `rm` so history
/// survives rebuilds; this is the explicit reaping path. Confirmed per entry
/// unless `force`.
fn gc_shell_history(dir: &Path, state: &State, force: bool) -> Result<()> {
    let history_dir = dir.join("shared-volumes").join("history");
    let Ok(entries) = std::fs::read_dir(&history_dir) else {
        return Ok(()); // never provisioned
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_dir = path.is_dir();
        let instance = if is_dir {
            name
        } else {
            match name.strip_suffix(".zsh_history") {
                Some(instance) => instance,
                None => continue,
            }
        };
        if state.instances.values().any(|i| i.instance_id == instance) {
            continue;
        }
        if !force && !super::confirm(&format!("delete shell history `{}`?", path.display()))? {
            continue;
        }
        if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .with_context(|| format!("cannot remove {}", path.display()))?;
        println!("removed history {}", path.display());
    }
    Ok(())
}

/// Delete `<data-dir>/agent/<instance_id>.sock` symlinks whose id no instance in
/// state holds. Links leak when an `rm` crashed mid-way or predate this feature;
/// matched on the persistent id (like `gc_shell_history`, since ids are global
/// across config roots). No confirm prompt: unlike history, a dangling symlink
/// carries no data (docs/ssh-agent.md).
fn gc_agent_links(state: &State) -> Result<()> {
    let Some(agent_dir) = crate::commands::run::ssh_agent_dir() else {
        return Ok(()); // no data dir → nothing to sweep
    };
    gc_agent_links_in(&agent_dir, state)
}

fn gc_agent_links_in(agent_dir: &Path, state: &State) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(agent_dir) else {
        return Ok(()); // never provisioned
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".sock"))
        else {
            continue;
        };
        if state.instances.values().any(|i| i.instance_id == id) {
            continue;
        }
        std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
        println!("removed agent link {}", path.display());
    }
    Ok(())
}

pub(crate) fn container_running(container: &str) -> bool {
    backend().is_running(container).ok().flatten() == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn service(toml: &str) -> ResolvedService {
        Config::parse(toml).unwrap().resolve_service("db").unwrap()
    }

    #[test]
    fn ls_status_cell_summarizes_containers() {
        use crate::snapshot::ContainerStatus::{Exited, Missing, Running};
        let up = |n: &str| (n.to_string(), Running("Up".into()));
        let down = |n: &str| (n.to_string(), Exited("Exited".into()));
        let gone = |n: &str| (n.to_string(), Missing);

        assert_eq!(ls_status_cell(&[]), "-");
        assert_eq!(ls_status_cell(&[up("a")]), "running");
        assert_eq!(ls_status_cell(&[down("a")]), "stopped");
        assert_eq!(ls_status_cell(&[gone("a")]), "missing");
        assert_eq!(ls_status_cell(&[gone("a"), gone("b")]), "missing");
        assert_eq!(ls_status_cell(&[up("a"), up("b")]), "running");
        assert_eq!(ls_status_cell(&[up("a"), down("b")]), "1/2 running");
        assert_eq!(ls_status_cell(&[up("a"), gone("b"), down("c")]), "1/3 running");
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
            &crate::runtime::Dockerlike::DOCKER,
            "devsandbox-svc-proj-repo-db",
            "devsandbox-net-proj-repo",
            "postgres:16",
            &svc,
            &svc.spec.ports,
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
    fn service_run_args_apple_has_no_alias() {
        let svc = service("[services.db]\nimage = \"postgres:16\"");
        let args = service_run_args(
            &crate::runtime::AppleContainer,
            "devsandbox-svc-proj-repo-db",
            "devsandbox-net-proj-repo",
            "postgres:16",
            &svc,
            &[],
            &[],
        );
        let joined = args.join(" ");
        assert!(joined.contains("--network devsandbox-net-proj-repo"));
        assert!(!joined.contains("--network-alias"));
    }

    #[test]
    fn build_args_drop_cache_from_where_unsupported() {
        let cfg = Config::parse(
            "[services.db]\nbuild = { dockerfile = \"Dockerfile\", cacheFrom = \"reg/db:cache\", target = \"dev\" }",
        )
        .unwrap();
        let svc = cfg.resolve_service("db").unwrap();
        let build = svc.spec.build.as_ref().unwrap();
        let dir = Path::new("/cfg");
        let docker = build_args(&crate::runtime::Dockerlike::DOCKER, dir, "t", "Dockerfile", build, "x");
        assert!(docker.join(" ").contains("--cache-from reg/db:cache"));
        assert!(docker.join(" ").contains("--target dev"));
        assert_eq!(docker.last().unwrap(), "/cfg/.");
        let apple = build_args(&crate::runtime::AppleContainer, dir, "t", "Dockerfile", build, "x");
        assert!(!apple.join(" ").contains("--cache-from"));
        assert!(apple.join(" ").contains("--target dev"));
    }

    #[test]
    fn host_port_parses_forms() {
        assert_eq!(host_port("8080:80"), Some("8080"));
        assert_eq!(host_port("127.0.0.1:8080:80"), Some("8080"));
        assert_eq!(host_port("80"), None);
    }

    #[test]
    fn bare_port_parses_and_pins() {
        let pin = |s: &str| BarePort::parse(s).map(|b| b.pinned());
        assert_eq!(pin("5432").as_deref(), Some("5432:5432"));
        assert_eq!(pin("53/udp").as_deref(), Some("53:53/udp"));
        assert_eq!(pin("80/tcp").as_deref(), Some("80:80/tcp"));
        assert_eq!(pin("127.0.0.1::5432").as_deref(), Some("127.0.0.1:5432:5432"));
        // Already has a host port, or a shape we don't touch.
        for spec in ["8080:80", "127.0.0.1:8080:80", "8000-8010", "0", "80/sctp", "::80", "x"] {
            assert_eq!(BarePort::parse(spec), None, "{spec}");
        }
    }

    #[test]
    fn host_port_free_sees_loopback_listener() {
        let taken = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        assert!(!host_port_free(&BarePort::parse(&port.to_string()).unwrap()));
        assert!(!host_port_free(&BarePort::parse(&format!("127.0.0.1::{port}")).unwrap()));
    }

    #[test]
    #[cfg(unix)]
    fn gc_agent_links_sweeps_orphans_keeps_owned() {
        let agent_dir =
            std::env::temp_dir().join(format!("devsandbox-gc-agent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&agent_dir);
        std::fs::create_dir_all(&agent_dir).unwrap();
        let owned = agent_dir.join("owned-1.sock");
        let orphan = agent_dir.join("orphan-1.sock");
        // Targets need not exist — a dangling symlink is enough for the sweep.
        std::os::unix::fs::symlink("/nonexistent/agent-a.sock", &owned).unwrap();
        std::os::unix::fs::symlink("/nonexistent/agent-b.sock", &orphan).unwrap();

        let mut state = State::default();
        state.instances.insert("owned-1".into(), instance("owned", "owned-1"));

        gc_agent_links_in(&agent_dir, &state).unwrap();

        // `exists()` follows the (dangling) link and reports false; probe the
        // link itself with `symlink_metadata`.
        assert!(std::fs::symlink_metadata(&owned).is_ok(), "owned link removed");
        assert!(std::fs::symlink_metadata(&orphan).is_err(), "orphan link kept");

        std::fs::remove_dir_all(&agent_dir).unwrap();
    }

    fn instance(sandbox: &str, id: &str) -> crate::state::Instance {
        crate::state::Instance {
            sandbox: sandbox.into(),
            instance_id: id.into(),
            project: String::new(),
            container: format!("devsandbox-{id}"),
            folder: Default::default(),
            base_folder: Default::default(),
            worktree: None,
            branch: None,
            branch_created: true,
            folders: Vec::new(),
            shell_history: None,
            workspace: String::new(),
            workspace_file: None,
            remote_env: Default::default(),
            remote_user: None,
            ssh_auth_sock: None,
            devsbd_arch: None,
            volumes: Vec::new(),
            dispatcher: None,
            config_dir: None,
            extra_env: Default::default(),
            forwarded_ports: Default::default(),
            created_unix: 0,
        }
    }

    const REFS_CONFIG: &str = r#"
[services.db]
image = "postgres"
scope = "isolated"

[services.cache]
image = "redis"
scope = "global"

[sandbox.app]
folder = "../app"
image = "node"
services = ["db", "cache"]

[sandbox.plain]
folder = "../plain"
image = "node"
"#;

    fn refs_state() -> State {
        let mut state = State::default();
        state.instances.insert("app-1".into(), instance("app", "app-1"));
        state.instances.insert("app-2".into(), instance("app", "app-2"));
        state.instances.insert("plain-1".into(), instance("plain", "plain-1"));
        state.instances.insert("foreign".into(), instance("elsewhere", "foreign"));
        state
    }

    #[test]
    fn service_refs_splits_isolated_and_global_by_running_instance() {
        let config = Config::parse(REFS_CONFIG).unwrap();
        let state = refs_state();
        let running: BTreeSet<String> = ["app-1", "app-2"].iter().map(|s| s.to_string()).collect();

        let refs = service_refs(&config, &state, &running);

        // Global service: one entry regardless of how many instances reference it.
        assert_eq!(refs.global, BTreeSet::from(["cache".to_string()]));
        // Isolated service: recorded per running instance.
        assert_eq!(refs.isolated["app-1"], BTreeSet::from(["db".to_string()]));
        assert_eq!(refs.isolated["app-2"], BTreeSet::from(["db".to_string()]));
        assert_eq!(refs.live_instances, BTreeSet::from(["app-1".to_string(), "app-2".to_string()]));
    }

    #[test]
    fn service_refs_skips_non_running_instances() {
        let config = Config::parse(REFS_CONFIG).unwrap();
        let state = refs_state();
        // Only app-1 is running; app-2 is stopped, so its isolated ref is absent.
        let running: BTreeSet<String> = ["app-1"].iter().map(|s| s.to_string()).collect();

        let refs = service_refs(&config, &state, &running);

        assert!(refs.isolated.contains_key("app-1"));
        assert!(!refs.isolated.contains_key("app-2"));
        assert_eq!(refs.live_instances, BTreeSet::from(["app-1".to_string()]));
    }

    #[test]
    fn service_refs_skips_non_referencing_and_foreign_instances() {
        let config = Config::parse(REFS_CONFIG).unwrap();
        let state = refs_state();
        // plain-1 references no service; foreign belongs to another config root.
        let running: BTreeSet<String> =
            ["plain-1", "foreign"].iter().map(|s| s.to_string()).collect();

        let refs = service_refs(&config, &state, &running);

        assert!(refs.global.is_empty());
        assert!(refs.isolated.is_empty());
        // plain-1 is live (this config root); foreign is not counted at all.
        assert_eq!(refs.live_instances, BTreeSet::from(["plain-1".to_string()]));
    }

    #[test]
    fn service_refs_key_by_persistent_id_not_state_key() {
        // A renamed instance: state key `app-renamed`, id still `app-1`. Refs
        // must carry the id — service containers/labels/networks are named by
        // it — so gc keeps the right containers and reaps the right network.
        let config = Config::parse(REFS_CONFIG).unwrap();
        let mut state = State::default();
        state.instances.insert("app-renamed".into(), instance("app", "app-1"));
        let running: BTreeSet<String> = ["app-renamed"].iter().map(|s| s.to_string()).collect();

        let refs = service_refs(&config, &state, &running);

        assert_eq!(refs.isolated["app-1"], BTreeSet::from(["db".to_string()]));
        assert!(!refs.isolated.contains_key("app-renamed"));
        assert_eq!(refs.live_instances, BTreeSet::from(["app-1".to_string()]));
    }
}
