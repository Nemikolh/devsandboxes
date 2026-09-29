//! Port forwarding route resolution (docs/port-forwarding.md, step 6). Turns a
//! `(instance?, service?, port)` request into a concrete dial target for the
//! host forwarder (`src/devsbd/forward.rs`): which container carries the tunnel
//! and what its daemon dials there.
//!
//! Split into a pure planner and an impure edge:
//!
//! - [`plan`] is pure over `(config, state, running, project, instance_key,
//!   service)`. It returns an ordered list of [`Planned`] candidates, following
//!   the _Routes_ rules in the plan (instance forward; service via a running
//!   referencing instance; injection fallback). No I/O, fully table-tested.
//! - [`resolver`] (unix-only, since `Route` is) builds the closure the forwarder
//!   calls on every (re)spawn: it reloads config + state, recomputes liveness,
//!   plans, and tries the candidates in order, running the helper (`devsbd`) in
//!   the chosen container. Reloading each call is what lets a `service rebuild`
//!   or an instance restart heal without restarting the forward.

// Forwarding itself is unix-only (the mux is), so off unix only the planner
// compiles and nothing calls it.
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::BTreeSet;

use anyhow::Result;

use crate::config::{Config, ServiceScope};
#[cfg(unix)]
use crate::devsbd::forward::HostPort;
use crate::state::State;

use super::services;

/// A resolved forwarding candidate: where the tunnel lands and how it dials.
/// The impure edge tries a plan's candidates in order until one's helper runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Planned {
    /// Instance forward: tunnel into `container`, daemon dials `127.0.0.1:<port>`.
    /// `key` is the instance's state key (for `devsbd::ensure_recorded`).
    Instance { key: String, container: String },
    /// Service via a running instance: tunnel into that instance's `container`,
    /// daemon dials `<alias>:<port>` over the shared network. `alias` is the
    /// service name (its network alias / `/etc/hosts` entry). `service_container`
    /// is where the port actually lives — the *service* container, not the
    /// instance — for the listening-process lookup (the instance can't see the
    /// service's processes).
    ViaInstance { key: String, container: String, alias: String, service_container: String },
    /// Injection fallback: tunnel into the service's own container and dial
    /// `127.0.0.1:<port>` there. No instance available (or its helper is unusable).
    Inject { service_container: String },
}

/// Plan the ordered candidate list for a forward request. Pure: all liveness is
/// pre-computed in `running` (the set of instance *state keys* whose container
/// is up), so this is unit-testable without a runtime.
///
/// `instance_key` is an *exact* state key (a caller resolves a user-supplied
/// name via [`super::resolve_instance`] up front — never here, since that
/// prompts on a TTY and this runs on the forwarder's worker thread). `project`
/// scopes service container names.
///
/// Rules (docs/port-forwarding.md, _Routes_):
///
/// - No service → **instance forward**: the instance must exist and be running.
/// - Isolated service → needs a named instance that references it (the owner);
///   `[ViaInstance?, Inject]`.
/// - Global service → the named instance if it's running and references it,
///   else the first (by key) running instance of this config root that
///   references it; then always `Inject`. A service with no via-instance
///   candidate still yields `[Inject]` — the edge reports the not-running error.
pub fn plan(
    config: &Config,
    state: &State,
    running: &BTreeSet<String>,
    project: &str,
    instance_key: Option<&str>,
    service: Option<&str>,
) -> Result<Vec<Planned>, String> {
    match service {
        None => {
            let key = instance_key.ok_or("no instance or service named")?;
            let info = state
                .instances
                .get(key)
                .ok_or_else(|| format!("no sandbox instance `{key}`"))?;
            if !running.contains(key) {
                return Err(format!("instance `{key}` is not running (devsandbox start {key})"));
            }
            Ok(vec![Planned::Instance { key: key.to_string(), container: info.container.clone() }])
        }
        Some(service) => {
            // Unknown service name → surface config's own error.
            let scope = config
                .resolve_service(service)
                .map_err(|e| format!("{e:#}"))?
                .spec
                .scope;

            let mut plans = Vec::new();
            match scope {
                ServiceScope::Isolated => {
                    let key = instance_key.ok_or_else(|| {
                        format!("service `{service}` is isolated: name the instance")
                    })?;
                    if let Some(info) = state.instances.get(key) {
                        if !references(config, &info.sandbox, service) {
                            return Err(format!("instance `{key}` doesn't use service `{service}`"));
                        }
                    }
                    if let Some(via) = via_instance(config, state, running, scope, project, key, service) {
                        plans.push(via);
                    }
                    let id = instance_id(state, key);
                    plans.push(Planned::Inject {
                        service_container: services::isolated_service_container(project, id, service),
                    });
                }
                ServiceScope::Global => {
                    let via = instance_key
                        .and_then(|key| via_instance(config, state, running, scope, project, key, service))
                        .or_else(|| first_referencing(config, state, running, scope, project, service));
                    if let Some(via) = via {
                        plans.push(via);
                    }
                    plans.push(Planned::Inject {
                        service_container: services::service_container(project, service),
                    });
                }
            }
            Ok(plans)
        }
    }
}

/// Parse `lsof -F` (field) output into the listening `(pid, command)` pairs
/// (docs/port-forwarding.md, _Listening process_). `-F` emits one field per
/// line: a `p<pid>` line opens a process set, a `c<command>` line names its
/// command; other field lines (file descriptors, addresses) are ignored. A
/// `c` line with no preceding `p` is dropped. Duplicates — the same process
/// listening on both v4 and v6, or under `SO_REUSEPORT` — are deduped, order
/// preserved.
fn parse_lsof_f(out: &str) -> Vec<(u32, String)> {
    let mut procs = Vec::new();
    let mut pid: Option<u32> = None;
    for line in out.lines() {
        let Some((tag, rest)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => pid = rest.parse::<u32>().ok(),
            "c" => {
                if let Some(pid) = pid {
                    let pair = (pid, rest.to_string());
                    if !procs.contains(&pair) {
                        procs.push(pair);
                    }
                }
            }
            _ => {}
        }
    }
    procs
}

/// Render listening processes for the UI: `node (pid 412)`, several joined with
/// `, `. Empty → `None` (nothing to show).
fn format_procs(procs: &[(u32, String)]) -> Option<String> {
    if procs.is_empty() {
        return None;
    }
    Some(procs.iter().map(|(pid, cmd)| format!("{cmd} (pid {pid})")).collect::<Vec<_>>().join(", "))
}

/// Look up the process listening on `port` inside `container`, quietly (never
/// stderr — the TUI owns the screen), via a plain root `exec` (no devsbd
/// needed). `-F` output is machine-parseable; any failure at all — `lsof`
/// missing (exit 127), no match (exit 1), or the container gone — yields
/// `None`, never an error (docs/port-forwarding.md, _Listening process_).
pub(crate) fn listening_procs(container: &str, port: u16) -> Option<String> {
    let out = crate::runtime::backend()
        .output_quiet(&[
            "exec",
            "-u",
            "root",
            container,
            "lsof",
            "-nP",
            &format!("-iTCP:{port}"),
            "-sTCP:LISTEN",
            "-Fpc",
        ])
        .ok()?;
    format_procs(&parse_lsof_f(&out))
}

/// The persistent id for a state key (falls back to the key when the instance
/// is absent, which only happens for a stale user-supplied key).
fn instance_id<'a>(state: &'a State, key: &'a str) -> &'a str {
    state.instances.get(key).map(|i| i.instance_id.as_str()).unwrap_or(key)
}

/// The service container backing a via-instance candidate — where the port
/// actually lives, for the listening-process lookup. Uses the same helpers as
/// the injection fallback: an isolated service is per-instance (named by the
/// instance's persistent id), a global one is project-level.
fn via_service_container(
    scope: ServiceScope,
    state: &State,
    project: &str,
    key: &str,
    service: &str,
) -> String {
    match scope {
        ServiceScope::Isolated => {
            services::isolated_service_container(project, instance_id(state, key), service)
        }
        ServiceScope::Global => services::service_container(project, service),
    }
}

/// A `ViaInstance` candidate for `key` iff that instance exists, is running,
/// belongs to this config root, and its sandbox references `service`.
fn via_instance(
    config: &Config,
    state: &State,
    running: &BTreeSet<String>,
    scope: ServiceScope,
    project: &str,
    key: &str,
    service: &str,
) -> Option<Planned> {
    let info = state.instances.get(key)?;
    if !running.contains(key) || !references(config, &info.sandbox, service) {
        return None;
    }
    Some(Planned::ViaInstance {
        key: key.to_string(),
        container: info.container.clone(),
        alias: service.to_string(),
        service_container: via_service_container(scope, state, project, key, service),
    })
}

/// First (by state key) running instance of this config root that references
/// the `service`, as a `ViaInstance`. Deterministic tie-break: `state.instances`
/// is a `BTreeMap`, so iteration is by key.
fn first_referencing(
    config: &Config,
    state: &State,
    running: &BTreeSet<String>,
    scope: ServiceScope,
    project: &str,
    service: &str,
) -> Option<Planned> {
    state.instances.iter().find_map(|(key, info)| {
        (running.contains(key) && references(config, &info.sandbox, service)).then(|| {
            Planned::ViaInstance {
                key: key.clone(),
                container: info.container.clone(),
                alias: service.to_string(),
                service_container: via_service_container(scope, state, project, key, service),
            }
        })
    })
}

/// Whether `sandbox` (a config sandbox name) declares `service`. A sandbox from
/// a foreign config root won't resolve, so this is also the config-root filter.
fn references(config: &Config, sandbox: &str, service: &str) -> bool {
    config
        .resolve_sandbox(sandbox)
        .ok()
        .and_then(|s| s.properties.services)
        .is_some_and(|svcs| svcs.iter().any(|s| s == service))
}

/// The impure edge: build the `resolve` closure `Forward` calls on every
/// (re)spawn of its bridge. Unix-only, since
/// [`Route`](crate::devsbd::forward::Route) is.
///
/// `instance_key` must be an **exact state key** — the CLI resolves a
/// user-supplied name via [`super::resolve_instance`] once, before building the
/// resolver, because that path prompts on a TTY and this closure runs on the
/// forwarder's worker thread (never the CLI/UI thread). `service` is `None` for
/// an instance forward, `Some(name)` for a service forward; `port` is the
/// container-side port to dial.
///
/// Each call reloads config + state and recomputes liveness, so a `service
/// rebuild` or an instance restart heals without restarting the forward. It
/// plans the candidates and tries them in order: instance / via-instance
/// candidates run `devsbd::ensure_recorded` (persisting the arch); the injection
/// fallback runs `devsbd::ensure` with the arch cached in memory for the
/// forward's lifetime. Always quiet — nothing is written to stderr (the TUI owns
/// the screen); failures surface as the returned `Err` string.
#[cfg(unix)]
pub fn resolver(
    dir: std::path::PathBuf,
    instance_key: Option<String>,
    service: Option<String>,
    port: u16,
) -> Box<dyn Fn() -> Result<crate::devsbd::forward::Route, String> + Send + Sync> {
    use std::sync::Mutex;

    // Arch of the injection target, cached across (re)spawns so the fallback
    // doesn't re-probe the image every reconnect.
    let inject_arch: Mutex<Option<crate::devsbd::Arch>> = Mutex::new(None);

    Box::new(move || {
        let config = Config::load(&dir).map_err(|e| format!("{e:#}"))?;
        let state = State::load().map_err(|e| format!("{e:#}"))?;
        let project = services::project_id(&dir).map_err(|e| format!("{e:#}"))?;
        let running: BTreeSet<String> = state
            .instances
            .iter()
            .filter(|(_, i)| services::container_running(&i.container))
            .map(|(key, _)| key.clone())
            .collect();

        let plans = plan(
            &config,
            &state,
            &running,
            &project,
            instance_key.as_deref(),
            service.as_deref(),
        )?;

        let mut last_err = "no route".to_string();
        for planned in plans {
            match planned {
                Planned::Instance { key, container } => {
                    let info = state.instances.get(&key).ok_or_else(|| format!("no sandbox instance `{key}`"))?;
                    match crate::devsbd::ensure_recorded(&key, info, true) {
                        Some(arch) => {
                            return Ok(crate::devsbd::forward::Route {
                                process_container: container.clone(),
                                container,
                                arch: Some(arch),
                                host: "127.0.0.1".into(),
                                port,
                                label: format!("{key}:{port}"),
                            });
                        }
                        None => return Err(format!("devsbd couldn't run in `{container}`")),
                    }
                }
                Planned::ViaInstance { key, container, alias, service_container } => {
                    let info = state.instances.get(&key).ok_or_else(|| format!("no sandbox instance `{key}`"))?;
                    match crate::devsbd::ensure_recorded(&key, info, true) {
                        Some(arch) => {
                            return Ok(crate::devsbd::forward::Route {
                                container,
                                arch: Some(arch),
                                host: alias.clone(),
                                port,
                                label: format!("{alias}:{port} (via instance {key})"),
                                // The port lives in the service container, not
                                // the instance, so probe there.
                                process_container: service_container,
                            });
                        }
                        // Helper unavailable in this instance: fall through to
                        // the next candidate (the injection fallback).
                        None => last_err = format!("devsbd couldn't run in `{container}`"),
                    }
                }
                Planned::Inject { service_container } => {
                    if !services::container_running(&service_container) {
                        let name = service.as_deref().unwrap_or_default();
                        return Err(format!("no running container for service `{name}`"));
                    }
                    let cached = *inject_arch.lock().unwrap();
                    match crate::devsbd::ensure(&service_container, cached, true) {
                        Some(arch) => {
                            *inject_arch.lock().unwrap() = Some(arch);
                            let alias = service.as_deref().unwrap_or_default();
                            return Ok(crate::devsbd::forward::Route {
                                process_container: service_container.clone(),
                                container: service_container,
                                arch: Some(arch),
                                host: "127.0.0.1".into(),
                                port,
                                label: format!("{alias}:{port}"),
                            });
                        }
                        None => {
                            return Err(format!(
                                "devsbd couldn't run in `{service_container}` (image has no /bin/sh?)"
                            ));
                        }
                    }
                }
            }
        }
        Err(last_err)
    })
}

/// Validate a port spec's shape, returning `(host_port?, container_port)` on
/// success. `<port>` → `(None, port)`; `<host>:<port>` → `(Some(host), port)`.
/// Both ports must be non-zero decimals; extra colons, empty parts, or garbage
/// are rejected with a message that names the offending spec's shape.
///
/// Platform-independent (no `HostPort`), so the TUI can validate the same way
/// the CLI does without pulling in the unix-only forwarder types. [`parse_port_spec`]
/// wraps it into `HostPort` on unix.
pub fn validate_port_spec(spec: &str) -> Result<(Option<u16>, u16), String> {
    let parse = |s: &str, what: &str| -> Result<u16, String> {
        if s.is_empty() {
            return Err(format!("port spec `{spec}`: {what} is empty"));
        }
        match s.parse::<u16>() {
            Ok(0) => Err(format!("port spec `{spec}`: {what} must not be 0")),
            Ok(p) => Ok(p),
            Err(_) => Err(format!("port spec `{spec}`: {what} `{s}` is not a port number")),
        }
    };
    match spec.split(':').collect::<Vec<_>>().as_slice() {
        [port] => Ok((None, parse(port, "port")?)),
        [host, container] => Ok((Some(parse(host, "host port")?), parse(container, "container port")?)),
        _ => Err(format!("port spec `{spec}`: expected `<port>` or `<host>:<port>`")),
    }
}

/// Parse a CLI port spec into `(host binding, container port)`.
///
/// - `<port>` → `(Prefer(port), port)`: same host port, or the next free one
///   up if taken (docs/port-forwarding.md, _Binding_).
/// - `<host>:<port>` → `(Fixed(host), port)`: explicit host port, fail if taken.
///
/// Shape validation is shared with the TUI via [`validate_port_spec`].
#[cfg(unix)]
pub fn parse_port_spec(spec: &str) -> Result<(HostPort, u16), String> {
    match validate_port_spec(spec)? {
        (None, p) => Ok((HostPort::Prefer(p), p)),
        (Some(h), c) => Ok((HostPort::Fixed(h), c)),
    }
}

/// A stderr status line for a forward whose coarse state changed, or `None` when
/// there's nothing worth printing. Pure over `(previous, new, local_port)` so
/// the poll loop can dedupe without re-printing an unchanged state. `Active` is
/// silent — the initial `->` line already announced it; only `Connecting`
/// (a retry in flight) and `Error` (with the reason) are surfaced.
#[cfg(unix)]
fn state_change_line(
    prev: &crate::devsbd::forward::ForwardState,
    new: &crate::devsbd::forward::ForwardState,
    local_port: u16,
) -> Option<String> {
    use crate::devsbd::forward::ForwardState::*;
    if prev == new {
        return None;
    }
    match new {
        Active => Some(format!("note: {local_port}: reconnected")),
        Connecting => Some(format!("note: {local_port}: reconnecting")),
        Error(reason) => Some(format!("note: {local_port}: {reason}")),
    }
}

/// clap fills the optional leading `name` positional first, so
/// `port --service redis 6379` arrives as `name = "6379"`. With `--service`, a
/// leading positional that parses as a port spec is a port, not an instance
/// (instance names that are bare port numbers aren't worth the ambiguity).
#[cfg(unix)]
fn split_target(name: Option<String>, has_service: bool, mut ports: Vec<String>) -> (Option<String>, Vec<String>) {
    match name {
        Some(n) if has_service && parse_port_spec(&n).is_ok() => {
            ports.insert(0, n);
            (None, ports)
        }
        other => (other, ports),
    }
}

/// CLI entry point for `devsandbox port`. Starts one forward per spec, waits for
/// each to reach its first outcome, prints the mapping, then blocks until SIGINT
/// (unix-only). See docs/port-forwarding.md, step 7.
#[cfg(unix)]
pub fn port(
    dir: std::path::PathBuf,
    name: Option<String>,
    service: Option<String>,
    address: std::net::IpAddr,
    ports: Vec<String>,
) -> Result<()> {
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context};

    use crate::devsbd::forward::{Forward, ForwardSpec, ForwardState};

    let (name, ports) = split_target(name, service.is_some(), ports);
    if name.is_none() && service.is_none() {
        bail!("name an instance or a service: `devsandbox port <instance> <port>` or `devsandbox port --service <svc> <port>`");
    }
    if ports.is_empty() {
        bail!("give at least one port to forward, e.g. `devsandbox port <instance> 3000`");
    }

    // Parse every spec up front so a typo fails before we bind or touch docker.
    let specs: Vec<(HostPort, u16)> = ports
        .iter()
        .map(|s| parse_port_spec(s))
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!(e))?;

    // Resolve the user-supplied name once, on this (TTY) thread — the resolver
    // closure runs on the forwarder's worker thread and must never prompt.
    let instance_key = match &name {
        Some(name) => {
            let state = State::load()?;
            Some(super::resolve_instance(&state, name)?)
        }
        None => None,
    };

    let mut forwards: Vec<Forward> = Vec::with_capacity(specs.len());
    for (host_port, container_port) in &specs {
        let resolve = resolver(dir.clone(), instance_key.clone(), service.clone(), *container_port);
        let probe = Box::new(listening_procs);
        let forward = Forward::start(ForwardSpec { bind: address, host_port: *host_port, resolve, probe })
            .with_context(|| match host_port {
                HostPort::Fixed(p) => format!("host port {p} is in use"),
                HostPort::Prefer(p) => format!("binding host port {p}"),
            })?;
        forwards.push(forward);
    }

    // Wait for each forward's first outcome (Active or Error), with a bound, so
    // a bad route fails fast instead of hanging. Any Error aborts (drops all).
    let deadline = Instant::now() + Duration::from_secs(20);
    for forward in &forwards {
        loop {
            let status = forward.status();
            match &status.state {
                ForwardState::Active => break,
                ForwardState::Error(reason) => {
                    let label = first_outcome_label(&status);
                    bail!("{label}: {reason}");
                }
                ForwardState::Connecting => {
                    if Instant::now() >= deadline {
                        let label = first_outcome_label(&status);
                        bail!("{label}: timed out waiting to connect");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    // All active: print the mappings, then announce we're blocking. The
    // listening process usually isn't known yet at this point (the first probe
    // lands ~100ms later), so the suffix is re-printed by the loop below.
    let mut prev_states: Vec<ForwardState> = Vec::with_capacity(forwards.len());
    let mut prev_procs: Vec<Option<String>> = Vec::with_capacity(forwards.len());
    for (forward, (host_port, _)) in forwards.iter().zip(&specs) {
        let status = forward.status();
        println!("{}", mapping_line(&status, *host_port));
        prev_states.push(status.state.clone());
        prev_procs.push(status.process.clone());
    }
    eprintln!("forwarding; press Ctrl-C to stop");

    // Block until SIGINT (kills this process group, so the exec children die
    // with us — a plain poll loop needs no signal crate). Surface state changes
    // and per-connection notes, deduplicated.
    loop {
        std::thread::sleep(Duration::from_millis(250));
        for ((forward, (host_port, _)), (prev, prev_proc)) in forwards
            .iter()
            .zip(&specs)
            .zip(prev_states.iter_mut().zip(prev_procs.iter_mut()))
        {
            let status = forward.status();
            let local_port = status.local_addr.port();
            if let Some(line) = state_change_line(prev, &status.state, local_port) {
                eprintln!("{line}");
            }
            *prev = status.state.clone();
            // Re-print the mapping (on stdout) when the listening process is
            // first discovered or changes to a new pid.
            if let Some(line) = process_reprint_line(prev_proc, &status, *host_port) {
                println!("{line}");
            }
            *prev_proc = status.process.clone();
            for note in forward.drain_notes() {
                eprintln!("note: {local_port}: {note}");
            }
        }
    }
}

/// Best label for a forward that failed its first outcome: the resolved route
/// label once known, else the bound local address (the route may never have
/// resolved, so `route_label` can be empty).
#[cfg(unix)]
fn first_outcome_label(status: &crate::devsbd::forward::ForwardStatus) -> String {
    if status.route_label.is_empty() {
        status.local_addr.to_string()
    } else {
        status.route_label.clone()
    }
}

/// The stdout mapping line for an active forward: `127.0.0.1:3000 -> api:3000`,
/// suffixed with ` [node (pid 412)]` once the listening process is known.
/// Uses the real bound port (a `Prefer` fallback shows the OS-assigned port);
/// when it differs from the requested port, say why.
#[cfg(unix)]
fn mapping_line(status: &crate::devsbd::forward::ForwardStatus, host_port: HostPort) -> String {
    let local = status.local_addr;
    let base = format!("{local} -> {}", status.route_label);
    let base = match host_port {
        HostPort::Prefer(requested) if local.port() != requested => {
            format!("{base} ({requested} was in use)")
        }
        _ => base,
    };
    match &status.process {
        Some(process) => format!("{base} [{process}]"),
        None => base,
    }
}

/// Whether to re-print the mapping line because the listening process changed.
/// The lookup usually lands after the first mapping print (and a dev server can
/// restart under a new pid), so re-emit the line whenever `process` becomes a
/// *new* `Some` value; going back to `None` (or unchanged) prints nothing. Pure
/// over `(prev, new)` so the poll loop dedupes.
#[cfg(unix)]
fn process_reprint_line(
    prev: &Option<String>,
    status: &crate::devsbd::forward::ForwardStatus,
    host_port: HostPort,
) -> Option<String> {
    match &status.process {
        Some(new) if prev.as_deref() != Some(new.as_str()) => Some(mapping_line(status, host_port)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Instance;
    use std::time::Duration;

    const CONFIG: &str = r#"
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

    const PROJECT: &str = "proj1234";

    fn config() -> Config {
        Config::parse(CONFIG).unwrap()
    }

    fn instance(sandbox: &str, id: &str) -> Instance {
        Instance {
            sandbox: sandbox.into(),
            instance_id: id.into(),
            project: String::new(),
            container: format!("devsandbox-{id}"),
            folder: Default::default(),
            base_folder: Default::default(),
            worktree: None,
            branch: None,
            branch_created: true,
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

    /// State with two `app` instances, one `plain`, and a foreign instance
    /// (belongs to a config root that doesn't define its sandbox).
    fn state() -> State {
        let mut state = State::default();
        state.instances.insert("app-1".into(), instance("app", "app-1"));
        state.instances.insert("app-2".into(), instance("app", "app-2"));
        state.instances.insert("plain-1".into(), instance("plain", "plain-1"));
        state.instances.insert("foreign".into(), instance("elsewhere", "foreign"));
        state
    }

    fn running(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|s| s.to_string()).collect()
    }

    // ---- instance forward (no service) ----

    #[test]
    fn instance_forward_running_yields_instance() {
        let plans =
            plan(&config(), &state(), &running(&["app-1"]), PROJECT, Some("app-1"), None).unwrap();
        assert_eq!(
            plans,
            vec![Planned::Instance { key: "app-1".into(), container: "devsandbox-app-1".into() }]
        );
    }

    #[test]
    fn instance_forward_not_running_names_the_fix() {
        let err = plan(&config(), &state(), &running(&[]), PROJECT, Some("app-1"), None).unwrap_err();
        assert_eq!(err, "instance `app-1` is not running (devsandbox start app-1)");
    }

    #[test]
    fn instance_forward_unknown_key_errors() {
        let err =
            plan(&config(), &state(), &running(&["app-1"]), PROJECT, Some("ghost"), None).unwrap_err();
        assert_eq!(err, "no sandbox instance `ghost`");
    }

    #[test]
    fn no_instance_and_no_service_errors() {
        let err = plan(&config(), &state(), &running(&[]), PROJECT, None, None).unwrap_err();
        assert_eq!(err, "no instance or service named");
    }

    // ---- unknown service ----

    #[test]
    fn unknown_service_surfaces_config_error() {
        let err =
            plan(&config(), &state(), &running(&["app-1"]), PROJECT, Some("app-1"), Some("nope"))
                .unwrap_err();
        assert!(err.contains("unknown service `nope`"), "{err}");
    }

    // ---- isolated service ----

    #[test]
    fn isolated_needs_a_named_instance() {
        let err =
            plan(&config(), &state(), &running(&["app-1"]), PROJECT, None, Some("db")).unwrap_err();
        assert_eq!(err, "service `db` is isolated: name the instance");
    }

    #[test]
    fn isolated_running_referencing_instance_via_then_inject() {
        let plans =
            plan(&config(), &state(), &running(&["app-1"]), PROJECT, Some("app-1"), Some("db"))
                .unwrap();
        assert_eq!(
            plans,
            vec![
                Planned::ViaInstance {
                    key: "app-1".into(),
                    container: "devsandbox-app-1".into(),
                    alias: "db".into(),
                    service_container: services::isolated_service_container(PROJECT, "app-1", "db"),
                },
                Planned::Inject {
                    service_container: services::isolated_service_container(PROJECT, "app-1", "db"),
                },
            ]
        );
    }

    #[test]
    fn isolated_stopped_instance_yields_only_inject() {
        // app-1 named but not running: no via candidate, inject as the sole
        // fallback (the edge reports the not-running error).
        let plans = plan(&config(), &state(), &running(&[]), PROJECT, Some("app-1"), Some("db")).unwrap();
        assert_eq!(
            plans,
            vec![Planned::Inject {
                service_container: services::isolated_service_container(PROJECT, "app-1", "db"),
            }]
        );
    }

    #[test]
    fn isolated_uses_persistent_id_for_container_after_rename() {
        // Renamed instance: state key `app-renamed`, id `app-1`. The isolated
        // service container is named by the id.
        let mut state = State::default();
        state.instances.insert("app-renamed".into(), instance("app", "app-1"));
        let plans = plan(
            &config(),
            &state,
            &running(&["app-renamed"]),
            PROJECT,
            Some("app-renamed"),
            Some("db"),
        )
        .unwrap();
        assert_eq!(
            plans[1],
            Planned::Inject {
                service_container: services::isolated_service_container(PROJECT, "app-1", "db"),
            }
        );
    }

    #[test]
    fn isolated_instance_not_referencing_errors() {
        // plain-1 is running but its sandbox doesn't declare `db`: it owns no
        // `db` container, so there is nothing to inject into either.
        let err =
            plan(&config(), &state(), &running(&["plain-1"]), PROJECT, Some("plain-1"), Some("db"))
                .unwrap_err();
        assert_eq!(err, "instance `plain-1` doesn't use service `db`");
    }

    // ---- global service ----

    #[test]
    fn global_named_running_referencing_instance_wins() {
        let plans =
            plan(&config(), &state(), &running(&["app-1", "app-2"]), PROJECT, Some("app-2"), Some("cache"))
                .unwrap();
        assert_eq!(
            plans,
            vec![
                Planned::ViaInstance {
                    key: "app-2".into(),
                    container: "devsandbox-app-2".into(),
                    alias: "cache".into(),
                    service_container: services::service_container(PROJECT, "cache"),
                },
                Planned::Inject { service_container: services::service_container(PROJECT, "cache") },
            ]
        );
    }

    #[test]
    fn global_no_instance_falls_to_first_running_referencing_by_key() {
        // No instance named: pick the first (by state key) running instance that
        // references `cache`. app-1 < app-2, both running and referencing.
        let plans =
            plan(&config(), &state(), &running(&["app-1", "app-2"]), PROJECT, None, Some("cache"))
                .unwrap();
        assert_eq!(
            plans[0],
            Planned::ViaInstance {
                key: "app-1".into(),
                container: "devsandbox-app-1".into(),
                alias: "cache".into(),
                service_container: services::service_container(PROJECT, "cache"),
            }
        );
    }

    #[test]
    fn global_named_stopped_falls_back_to_another_running_referencer() {
        // app-2 named but stopped; app-1 running and referencing → it's the via.
        let plans =
            plan(&config(), &state(), &running(&["app-1"]), PROJECT, Some("app-2"), Some("cache"))
                .unwrap();
        assert_eq!(
            plans[0],
            Planned::ViaInstance {
                key: "app-1".into(),
                container: "devsandbox-app-1".into(),
                alias: "cache".into(),
                service_container: services::service_container(PROJECT, "cache"),
            }
        );
    }

    #[test]
    fn global_no_running_referencer_yields_only_inject() {
        // Nothing running references cache: inject is the sole candidate.
        let plans = plan(&config(), &state(), &running(&["plain-1"]), PROJECT, None, Some("cache")).unwrap();
        assert_eq!(
            plans,
            vec![Planned::Inject { service_container: services::service_container(PROJECT, "cache") }]
        );
    }

    #[test]
    fn global_named_non_referencing_still_finds_another_referencer() {
        // plain-1 named but doesn't reference cache; app-1 does and is running.
        let plans = plan(
            &config(),
            &state(),
            &running(&["plain-1", "app-1"]),
            PROJECT,
            Some("plain-1"),
            Some("cache"),
        )
        .unwrap();
        assert_eq!(
            plans[0],
            Planned::ViaInstance {
                key: "app-1".into(),
                container: "devsandbox-app-1".into(),
                alias: "cache".into(),
                service_container: services::service_container(PROJECT, "cache"),
            }
        );
    }

    // ---- split_target ----

    #[test]
    fn split_target_moves_a_leading_port_to_ports_only_with_service() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // `port --service redis 6379`: clap put the port in `name`.
        assert_eq!(split_target(Some("6379".into()), true, vec![]), (None, s(&["6379"])));
        assert_eq!(split_target(Some("1:2".into()), true, s(&["3"])), (None, s(&["1:2", "3"])));
        // A real instance name stays a name.
        assert_eq!(split_target(Some("api".into()), true, s(&["5432"])), (Some("api".into()), s(&["5432"])));
        // Without --service a numeric name is left alone (the port guard reports it).
        assert_eq!(split_target(Some("3000".into()), false, vec![]), (Some("3000".into()), vec![]));
    }

    // ---- parse_port_spec ----

    #[test]
    fn parse_port_spec_table() {
        use HostPort::{Fixed, Prefer};

        let ok: &[(&str, (HostPort, u16))] = &[
            ("3000", (Prefer(3000), 3000)),
            ("8080:3000", (Fixed(8080), 3000)),
            ("1:65535", (Fixed(1), 65535)),
        ];
        for (spec, want) in ok {
            assert_eq!(parse_port_spec(spec), Ok(*want), "spec `{spec}`");
        }

        let err: &[&str] = &[
            "",           // empty
            "0",          // zero container/host port
            "8080:0",     // zero container port
            "0:3000",     // zero host port
            "abc",        // non-numeric
            "8080:abc",   // non-numeric container
            "8080:3000:1", // extra colon
            ":3000",      // empty host part
            "8080:",      // empty container part
            "70000",      // out of u16 range
        ];
        for spec in err {
            assert!(parse_port_spec(spec).is_err(), "spec `{spec}` should be rejected");
        }
    }

    // ---- state_change_line ----

    #[test]
    fn state_change_line_transitions() {
        use crate::devsbd::forward::ForwardState::*;

        // No change → nothing.
        assert_eq!(state_change_line(&Active, &Active, 3000), None);
        assert_eq!(state_change_line(&Connecting, &Connecting, 3000), None);

        // Transitions carry the local port and (for Error) the reason.
        assert_eq!(
            state_change_line(&Active, &Connecting, 3000),
            Some("note: 3000: reconnecting".to_string())
        );
        assert_eq!(
            state_change_line(&Connecting, &Active, 3000),
            Some("note: 3000: reconnected".to_string())
        );
        assert_eq!(
            state_change_line(&Active, &Error("dead".into()), 5432),
            Some("note: 5432: dead".to_string())
        );
    }

    // ---- parse_lsof_f / format_procs ----

    #[test]
    fn parse_lsof_f_table() {
        // Empty output → no processes.
        assert_eq!(parse_lsof_f(""), Vec::<(u32, String)>::new());

        // One process, with an interleaved (ignored) fd/address field line.
        assert_eq!(
            parse_lsof_f("p412\ncnode\nf5\nn127.0.0.1:3000\n"),
            vec![(412, "node".to_string())]
        );

        // Multiple distinct processes.
        assert_eq!(
            parse_lsof_f("p1\ncnginx\np2\ncredis\n"),
            vec![(1, "nginx".to_string()), (2, "redis".to_string())]
        );

        // v4 + v6 listing of the *same* pid/command is deduped, order kept.
        assert_eq!(
            parse_lsof_f("p412\ncnode\nfIPv4\np412\ncnode\nfIPv6\n"),
            vec![(412, "node".to_string())]
        );

        // A stray `c` line with no preceding `p` is dropped.
        assert_eq!(parse_lsof_f("cghost\np7\ncsh\n"), vec![(7, "sh".to_string())]);
    }

    #[test]
    fn format_procs_table() {
        assert_eq!(format_procs(&[]), None);
        assert_eq!(format_procs(&[(412, "node".into())]), Some("node (pid 412)".to_string()));
        assert_eq!(
            format_procs(&[(1, "nginx".into()), (2, "redis".into())]),
            Some("nginx (pid 1), redis (pid 2)".to_string())
        );
    }

    // ---- process_reprint_line ----

    #[cfg(unix)]
    #[test]
    fn process_reprint_line_only_on_new_some() {
        use crate::devsbd::forward::{ForwardState, ForwardStatus};
        use std::net::{Ipv4Addr, SocketAddr};

        let status = |process: Option<&str>| ForwardStatus {
            local_addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 3000),
            route_label: "api:3000".into(),
            state: ForwardState::Active,
            open_conns: 0,
            process: process.map(str::to_string),
        };
        let hp = HostPort::Prefer(3000);

        // First discovery (None → Some) re-prints the mapping with the suffix.
        assert_eq!(
            process_reprint_line(&None, &status(Some("node (pid 412)")), hp),
            Some("127.0.0.1:3000 -> api:3000 [node (pid 412)]".to_string())
        );
        // Unchanged Some → nothing.
        assert_eq!(
            process_reprint_line(&Some("node (pid 412)".into()), &status(Some("node (pid 412)")), hp),
            None
        );
        // New pid → re-print.
        assert_eq!(
            process_reprint_line(&Some("node (pid 412)".into()), &status(Some("node (pid 999)")), hp),
            Some("127.0.0.1:3000 -> api:3000 [node (pid 999)]".to_string())
        );
        // Back to None → nothing (don't print when it disappears).
        assert_eq!(process_reprint_line(&Some("node (pid 412)".into()), &status(None), hp), None);
    }

    // ---- docker-gated: listening_procs ----

    use std::process::Command;

    /// Docker-gated: a container with `lsof` running a loopback listener yields
    /// the listening process; a container without `lsof` yields `None`. Skips
    /// (fails on CI) without docker, without network (apk fails), or on any
    /// hiccup.
    #[test_utils::docker_test]
    fn listening_procs_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        // (1) lsof present, a loopback TCP listener on 8080.
        let with = format!("devsandbox-lsof-yes-{stamp}");
        // (2) no lsof: plain alpine sleeping.
        let without = format!("devsandbox-lsof-no-{stamp}");
        let cleanup = || {
            for n in [&with, &without] {
                let _ = Command::new("docker").args(["rm", "-f", n]).output();
            }
        };
        let run_ok = |args: &[&str]| matches!(Command::new("docker").args(args).output(), Ok(o) if o.status.success());

        crate::test_support::with_cleanup(cleanup, || {
            // Listener: keep the container up on a trivial command, then add
            // lsof + a *real* nc (netcat-openbsd, so lsof reports the command
            // as `nc`, not `busybox`) and start the listener as a distinct pid.
            assert!(run_ok(&["run", "-d", "--name", &with, "alpine:3.20", "sleep", "300"]));
            // Add lsof + nc; if apk can't reach the network, skip cleanly.
            if !run_ok(&["exec", "-u", "root", &with, "apk", "add", "--no-cache", "lsof", "netcat-openbsd"]) {
                return Err("apk add failed (no network?)");
            }
            // netcat-openbsd's nc holds 127.0.0.1:8080 LISTENing in the background.
            if !run_ok(&["exec", "-d", "-u", "root", &with, "nc", "-lk", "-s", "127.0.0.1", "-p", "8080"]) {
                return Err("could not start nc listener");
            }
            // nc may take a moment to bind; retry the lookup.
            let mut found = None;
            for _ in 0..50 {
                if let Some(p) = listening_procs(&with, 8080) {
                    found = Some(p);
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let found = found.ok_or("listening process never found")?;
            assert!(found.contains("nc"), "expected nc, got `{found}`");
            assert!(found.contains("pid "), "expected a pid, got `{found}`");

            // No lsof in this container → None, never an error.
            assert!(run_ok(&["run", "-d", "--name", &without, "alpine:3.20", "sleep", "300"]));
            assert_eq!(listening_procs(&without, 8080), None, "no lsof → None");
            Ok(())
        })
    }
}
