//! Docker/state data collection for the dashboard. Runs off the UI thread
//! (see `mod::run`), so everything here is plain blocking I/O producing a
//! [`Snapshot`] that the state machine consumes verbatim. Parsing lives in
//! free functions to stay unit-testable without docker.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use serde::Deserialize;

use super::procs::{ProcState, MESSAGE_ROW};
use crate::commands::services::{isolated_service_container, project_id, service_container};
use crate::config::{Config, ServiceScope};
use crate::docker::{self, NAME_PREFIX};
use crate::state::{Instance, State};

/// Container liveness, joined from `docker ps` against the state's instance list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerStatus {
    /// Container exists and is up. Carries docker's raw `Status` text (e.g.
    /// "Up 3 minutes").
    Running(String),
    /// Container exists but is stopped. Carries the raw `Status` text.
    Exited(String),
    /// No container by that name in `docker ps --all`.
    Missing,
}

impl ContainerStatus {
    /// Short label for the STATUS column.
    pub fn label(&self) -> &str {
        match self {
            ContainerStatus::Running(_) => "running",
            ContainerStatus::Exited(_) => "exited",
            ContainerStatus::Missing => "missing",
        }
    }
}

/// One instance row, everything the Instances view needs pre-joined.
#[derive(Clone, Debug)]
pub struct InstanceRow {
    pub name: String,
    pub sandbox: String,
    pub container: String,
    pub status: ContainerStatus,
    pub uptime_secs: u64,
    pub cpu: Option<String>,
    pub mem: Option<String>,
    pub folder: String,
    pub worktree: bool,
    pub services: Vec<String>,
    pub workspace: String,
    pub remote_user: Option<String>,
    pub remote_env_len: usize,
    pub base_folder: String,
    pub drift: bool,
}

/// One sandbox row from `config.toml`, everything the Instances tree needs to
/// render a sandbox node. Every configured sandbox gets one, even with zero
/// instances; a sandbox that fails to resolve still gets a row with source `?`.
#[derive(Clone, Debug)]
pub struct SandboxRow {
    pub name: String,
    /// Like `ResolvedSandbox::source`: `image X` / `dockerfile Y`, or `?` when
    /// the sandbox failed to resolve.
    pub source: String,
    pub folder: Option<String>,
    pub services: Vec<String>,
    /// `extends` template chain from the raw table (`-` list empty when none).
    pub extends: Vec<String>,
    /// Short config hash of the resolved table, empty when unresolved.
    pub config_hash: String,
}

/// One service row, everything the Services view needs pre-joined.
#[derive(Clone, Debug)]
pub struct ServiceRow {
    pub name: String,
    /// `"global"` or `"isolated"`.
    pub scope: &'static str,
    /// Like `ResolvedSandbox::source`: `image X` / `dockerfile Y`, or `?` when
    /// the service failed to resolve.
    pub source: String,
    pub ports: Vec<String>,
    /// Backing containers (one for global, one per referencing instance for
    /// isolated) with their liveness.
    pub containers: Vec<(String, ContainerStatus)>,
    /// Instance names whose resolved sandbox lists this service.
    pub used_by: Vec<String>,
    /// Number of `env` entries in the service spec (`0` when unresolved).
    pub env_len: usize,
    /// Service command, if set.
    pub command: Option<String>,
    /// Short config hash, empty when unresolved.
    pub config_hash: String,
}

/// A point-in-time view of instances plus any collection error (docker/config
/// unavailable). Rows are always present from state even when docker is down.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub instances: Vec<InstanceRow>,
    /// Sandboxes from `config.toml`, in config order. Empty when config is
    /// absent or unreadable. Roots of the Instances tree.
    pub sandboxes: Vec<SandboxRow>,
    pub services: Vec<ServiceRow>,
    /// Number of sandboxes defined in `config.toml` (0 when config is absent or
    /// unreadable). Feeds the header totals line.
    pub sandbox_count: usize,
    /// Docker server version (e.g. `24.0.7`), or `None` when docker is down or
    /// the version probe failed. Feeds the header totals line.
    pub docker_version: Option<String>,
    /// When collection finished. Drives the staleness indicator in the header.
    pub collected_at: Instant,
    pub error: Option<String>,
}

/// The literal sandbox name of the synthetic group holding instances whose
/// sandbox is not in config. Keyed by this string in the collapsed set.
pub const ORPHANS_NAME: &str = "(not in config)";

/// One visible row of the Instances tree, flattened from
/// (sandboxes, instances, collapsed) by [`visible_nodes`]. Payloads are indices
/// into `Snapshot::sandboxes` / `Snapshot::instances`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
    /// A configured sandbox root (index into `sandboxes`).
    Sandbox(usize),
    /// An instance row (index into `instances`), a child of the sandbox or the
    /// orphan group above it.
    Instance(usize),
    /// The dim "no instances" child under an expanded, empty sandbox (index into
    /// `sandboxes`).
    Empty(usize),
    /// The synthetic `(not in config)` group header, holding orphan instances.
    /// Only emitted when such instances exist.
    Orphans,
    /// A process row under an expanded instance. `instance` indexes into
    /// `instances`; `row` indexes into that instance's [`ProcState::Rows`], or is
    /// [`MESSAGE_ROW`](super::procs::MESSAGE_ROW) for the single placeholder row
    /// of a [`ProcState::Message`] (not fetched / not running / error).
    Proc { instance: usize, row: usize },
}

/// Flatten the tree into the visible node list, in render/selection order.
///
/// Sandboxes appear in config order; an expanded sandbox lists its instances (in
/// `instances` order) as children, or one [`Node::Empty`] child when it has
/// none. Instances whose `sandbox` matches no [`SandboxRow`] group under a
/// trailing [`Node::Orphans`] header (keyed by [`ORPHANS_NAME`] in `collapsed`),
/// which only appears when such instances exist. A name in `collapsed` hides a
/// node's children.
pub fn visible_nodes(
    sandboxes: &[SandboxRow],
    instances: &[InstanceRow],
    collapsed: &BTreeSet<String>,
    expanded_procs: &BTreeSet<String>,
    procs: &BTreeMap<String, ProcState>,
) -> Vec<Node> {
    let mut nodes = Vec::new();
    let configured: BTreeSet<&str> = sandboxes.iter().map(|s| s.name.as_str()).collect();

    for (si, sb) in sandboxes.iter().enumerate() {
        nodes.push(Node::Sandbox(si));
        if collapsed.contains(&sb.name) {
            continue;
        }
        let mut any = false;
        for (ii, inst) in instances.iter().enumerate() {
            if inst.sandbox == sb.name {
                nodes.push(Node::Instance(ii));
                push_proc_nodes(&mut nodes, ii, inst, expanded_procs, procs);
                any = true;
            }
        }
        if !any {
            nodes.push(Node::Empty(si));
        }
    }

    // Orphans: instances whose sandbox is not configured, grouped at the bottom.
    let orphans: Vec<usize> = instances
        .iter()
        .enumerate()
        .filter(|(_, inst)| !configured.contains(inst.sandbox.as_str()))
        .map(|(ii, _)| ii)
        .collect();
    if !orphans.is_empty() {
        nodes.push(Node::Orphans);
        if !collapsed.contains(ORPHANS_NAME) {
            for ii in orphans {
                nodes.push(Node::Instance(ii));
                push_proc_nodes(&mut nodes, ii, &instances[ii], expanded_procs, procs);
            }
        }
    }

    nodes
}

/// Emit the process child nodes for one visible instance, if its procs are
/// expanded. A running container with a fetched forest emits one
/// [`Node::Proc`] per [`ProcRow`]; every other case (not fetched yet, container
/// not running, fetch error) emits the single [`MESSAGE_ROW`] placeholder.
fn push_proc_nodes(
    nodes: &mut Vec<Node>,
    instance: usize,
    inst: &InstanceRow,
    expanded_procs: &BTreeSet<String>,
    procs: &BTreeMap<String, ProcState>,
) {
    if !expanded_procs.contains(&inst.name) {
        return;
    }
    match procs.get(&inst.name) {
        Some(ProcState::Rows(rows)) => {
            for row in 0..rows.len() {
                nodes.push(Node::Proc { instance, row });
            }
        }
        // Message state, or not fetched yet: one placeholder row.
        _ => nodes.push(Node::Proc { instance, row: MESSAGE_ROW }),
    }
}

/// The `N/M running` stats string for a sandbox row: `M` instances, `N` running.
/// Pure so the rendering layer and tests share one format.
pub fn sandbox_stats(instances: usize, running: usize) -> String {
    format!("{running}/{instances} running")
}

/// Parsed line of `docker ps --format {{json .}}`.
#[derive(Debug, Deserialize)]
struct PsLine {
    #[serde(rename = "Names")]
    names: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "State")]
    state: String,
}

/// Parsed line of `docker stats --no-stream --format {{json .}}`.
#[derive(Debug, Deserialize)]
struct StatsLine {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "CPUPerc")]
    cpu: String,
    #[serde(rename = "MemUsage")]
    mem: String,
}

/// Pre-extracted service definition fed into [`build_service_rows`]. Keeping the
/// join pure (no docker, no `ResolvedService`) makes it unit-testable: `collect`
/// fills these from `Config::resolve_service`, tests construct them directly.
struct ServiceInput {
    name: String,
    /// `Some` when the service resolved; `None` when it failed (row renders with
    /// source `?`, scope defaulting to isolated).
    resolved: Option<ResolvedServiceInput>,
}

/// The resolved half of a [`ServiceInput`].
struct ResolvedServiceInput {
    scope: ServiceScope,
    source: String,
    ports: Vec<String>,
    env_len: usize,
    command: Option<String>,
    config_hash: String,
}

/// The `extends` template chain from a raw sandbox table value, as a name list
/// (same shape as `commands::ls::extends_names`, but returning a `Vec`). A bare
/// string is a single-element chain; anything else yields an empty chain.
fn extends_names(value: &toml::Value) -> Vec<String> {
    match value {
        toml::Value::String(s) => vec![s.clone()],
        toml::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Source description for a service, mirroring `ResolvedSandbox::source`.
fn service_source(spec: &crate::config::Service) -> String {
    if let Some(image) = &spec.image {
        return format!("image {image}");
    }
    if let Some(dockerfile) = spec.build.as_ref().and_then(|b| b.dockerfile.as_deref()) {
        return format!("dockerfile {dockerfile}");
    }
    "?".into()
}

/// Join config services against docker state. Pure: all docker/config I/O is
/// done by the caller and passed in. `project` scopes container names;
/// `instance_services` pairs every live instance name with the service list of
/// its resolved sandbox.
fn build_service_rows(
    project: &str,
    services: &[ServiceInput],
    instance_services: &[(String, Vec<String>)],
    ps: &[PsLine],
) -> Vec<ServiceRow> {
    services
        .iter()
        .map(|svc| {
            let used_by: Vec<String> = instance_services
                .iter()
                .filter(|(_, list)| list.iter().any(|s| s == &svc.name))
                .map(|(inst, _)| inst.clone())
                .collect();

            let (scope, source, ports, env_len, command, config_hash) = match &svc.resolved {
                Some(r) => (
                    r.scope,
                    r.source.clone(),
                    r.ports.clone(),
                    r.env_len,
                    r.command.clone(),
                    r.config_hash.clone(),
                ),
                None => (
                    ServiceScope::Isolated,
                    "?".to_string(),
                    Vec::new(),
                    0,
                    None,
                    String::new(),
                ),
            };

            let containers: Vec<(String, ContainerStatus)> = match scope {
                ServiceScope::Global => {
                    let c = service_container(project, &svc.name);
                    let status = classify(&c, ps);
                    vec![(c, status)]
                }
                ServiceScope::Isolated => used_by
                    .iter()
                    .map(|inst| {
                        let c = isolated_service_container(project, inst, &svc.name);
                        let status = classify(&c, ps);
                        (c, status)
                    })
                    .collect(),
            };

            ServiceRow {
                name: svc.name.clone(),
                scope: match scope {
                    ServiceScope::Global => "global",
                    ServiceScope::Isolated => "isolated",
                },
                source,
                ports,
                containers,
                used_by,
                env_len,
                command,
                config_hash,
            }
        })
        .collect()
}

/// Classify a container by name against parsed `docker ps` lines. `docker ps`
/// may list several comma-separated names per container; match any of them.
fn classify(container: &str, ps: &[PsLine]) -> ContainerStatus {
    for line in ps {
        if line.names.split(',').any(|n| n.trim() == container) {
            // docker's State is one of created/restarting/running/removing/
            // paused/exited/dead. Treat "running" as up, everything else stopped.
            if line.state == "running" {
                return ContainerStatus::Running(line.status.clone());
            }
            return ContainerStatus::Exited(line.status.clone());
        }
    }
    ContainerStatus::Missing
}

/// Parse `docker ps --format {{json .}}` output (one JSON object per line).
/// Unparseable lines are skipped rather than failing the whole collection.
fn parse_ps(out: &str) -> Vec<PsLine> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<PsLine>(l).ok())
        .collect()
}

/// Parse `docker stats --no-stream --format {{json .}}` output into a
/// name→(cpu, mem) map. Unparseable lines are skipped.
fn parse_stats(out: &str) -> BTreeMap<String, (String, String)> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<StatsLine>(l).ok())
        .map(|s| (s.name, (s.cpu, s.mem)))
        .collect()
}

/// Humanize an elapsed-seconds duration: `3d4h`, `2h05m`, `12m`, `40s`.
pub fn humanize_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

/// Collect a fresh [`Snapshot`]. Blocking; run off the UI thread.
///
/// State is the source of truth for which rows exist; docker enriches them with
/// liveness and resource usage. When docker or config is unavailable the rows
/// still render (status `Missing`, no cpu/mem) and `error` is set.
pub fn collect(dir: &Path) -> Snapshot {
    let collected_at = Instant::now();
    let now_unix = Instance::now();

    let state = match State::load() {
        Ok(s) => s,
        Err(e) => {
            return Snapshot {
                instances: Vec::new(),
                sandboxes: Vec::new(),
                services: Vec::new(),
                sandbox_count: 0,
                docker_version: None,
                collected_at,
                error: Some(format!("state: {e:#}")),
            };
        }
    };

    let mut errors: Vec<String> = Vec::new();

    // One `docker ps` and one `docker stats` call for the whole snapshot.
    let ps = match docker::output_quiet(&[
        "ps",
        "--all",
        "--filter",
        &format!("name=^/{NAME_PREFIX}"),
        "--format",
        "{{json .}}",
    ]) {
        Ok(out) => parse_ps(&out),
        Err(e) => {
            errors.push(format!("docker unavailable: {e:#}"));
            Vec::new()
        }
    };

    let stats = match docker::output_quiet(&["stats", "--no-stream", "--format", "{{json .}}"]) {
        Ok(out) => parse_stats(&out),
        // Only report a stats error if ps succeeded; otherwise the ps error
        // already covers "docker is down".
        Err(e) => {
            if errors.is_empty() {
                errors.push(format!("docker stats: {e:#}"));
            }
            BTreeMap::new()
        }
    };

    // Docker server version for the header; best-effort (None when down). Kept
    // off the UI thread like every other docker call here. Cheap enough to run
    // each collection, so no caching is threaded through.
    let docker_version = docker::output_quiet(&["version", "--format", "{{.Server.Version}}"])
        .ok()
        .filter(|v| !v.is_empty());

    // Load config once; resolve every sandbox for the tree, services + drift
    // hash. `resolved` maps sandbox name → (services, hash) for the instance join.
    let config = Config::load(dir);
    let mut resolved: BTreeMap<String, (Vec<String>, String)> = BTreeMap::new();
    let mut sandboxes: Vec<SandboxRow> = Vec::new();
    match &config {
        Ok(cfg) => {
            for name in cfg.sandboxes.keys() {
                let extends = cfg
                    .sandboxes
                    .get(name)
                    .and_then(|t| t.get("extends"))
                    .map(extends_names)
                    .unwrap_or_default();
                match cfg.resolve_sandbox(name) {
                    Ok(rs) => {
                        let services = rs.properties.services.clone().unwrap_or_default();
                        sandboxes.push(SandboxRow {
                            name: name.clone(),
                            source: rs.source(),
                            folder: rs.folder().map(str::to_string),
                            services: services.clone(),
                            extends,
                            config_hash: rs.config_hash.clone(),
                        });
                        resolved.insert(name.clone(), (services, rs.config_hash));
                    }
                    Err(e) => {
                        errors.push(format!("sandbox `{name}`: {e:#}"));
                        sandboxes.push(SandboxRow {
                            name: name.clone(),
                            source: "?".into(),
                            folder: None,
                            services: Vec::new(),
                            extends,
                            config_hash: String::new(),
                        });
                    }
                }
            }
        }
        Err(e) => errors.push(format!("config: {e:#}")),
    }
    let sandbox_count = sandboxes.len();

    let mut instances = Vec::with_capacity(state.instances.len());
    for (name, inst) in &state.instances {
        let status = classify(&inst.container, &ps);
        let (cpu, mem) = match stats.get(&inst.container) {
            Some((c, m)) => (Some(c.clone()), Some(m.clone())),
            None => (None, None),
        };
        let (services, drift) = match resolved.get(&inst.sandbox) {
            Some((svcs, hash)) => (svcs.clone(), drifted(&inst.container, hash)),
            None => (Vec::new(), false),
        };

        instances.push(InstanceRow {
            name: name.clone(),
            sandbox: inst.sandbox.clone(),
            container: inst.container.clone(),
            status,
            uptime_secs: now_unix.saturating_sub(inst.created_unix),
            cpu,
            mem,
            folder: inst.folder.display().to_string(),
            worktree: inst.worktree.is_some(),
            services,
            workspace: inst.workspace.clone(),
            remote_user: inst.remote_user.clone(),
            remote_env_len: inst.remote_env.len(),
            base_folder: inst.base_folder.display().to_string(),
            drift,
        });
    }

    // Services view: (instance name → the service list of its resolved sandbox),
    // then the config services resolved into pure inputs for the join.
    let instance_services: Vec<(String, Vec<String>)> = instances
        .iter()
        .map(|r| (r.name.clone(), r.services.clone()))
        .collect();

    let services = match (&config, project_id(dir)) {
        (Ok(cfg), Ok(project)) => {
            let inputs: Vec<ServiceInput> = cfg
                .services
                .keys()
                .map(|name| match cfg.resolve_service(name) {
                    Ok(rs) => ServiceInput {
                        name: name.clone(),
                        resolved: Some(ResolvedServiceInput {
                            scope: rs.spec.scope,
                            source: service_source(&rs.spec),
                            ports: rs.spec.ports.clone(),
                            env_len: rs.spec.env.len(),
                            command: rs
                                .spec
                                .command
                                .as_ref()
                                .map(|c| c.to_vec().join(" ")),
                            config_hash: rs.config_hash,
                        }),
                    },
                    Err(e) => {
                        errors.push(format!("service `{name}`: {e:#}"));
                        ServiceInput { name: name.clone(), resolved: None }
                    }
                })
                .collect();
            build_service_rows(&project, &inputs, &instance_services, &ps)
        }
        // No config already reported above; a project_id failure (e.g. dir does
        // not resolve) means no services either. Report it once.
        (Ok(_), Err(e)) => {
            errors.push(format!("project id: {e:#}"));
            Vec::new()
        }
        _ => Vec::new(),
    };

    Snapshot {
        instances,
        sandboxes,
        services,
        sandbox_count,
        docker_version,
        collected_at,
        error: if errors.is_empty() { None } else { Some(errors.join("; ")) },
    }
}

/// How stale a snapshot may be before the header flags it, in seconds. Normal
/// refresh cadence is 2s (see `mod::TICK_INTERVAL`); 10s means collection is
/// slow or failing.
pub const STALE_AFTER_SECS: u64 = 10;

/// Right-aligned totals summary for the header, e.g.
/// `3 sandboxes · 1 running / 2 stopped · 2 service containers · docker 24.0.7`.
/// A trailing ` (stale Ns)` is appended when `collected_at` is older than
/// [`STALE_AFTER_SECS`]. Pure over the snapshot so it is unit-testable; `age` is
/// passed in rather than read from the clock.
pub fn totals_line(snapshot: &Snapshot, age: std::time::Duration) -> String {
    let running = snapshot
        .instances
        .iter()
        .filter(|r| matches!(r.status, ContainerStatus::Running(_)))
        .count();
    let stopped = snapshot.instances.len() - running;
    // Service containers that actually exist (any non-Missing backing container).
    let service_containers: usize = snapshot
        .services
        .iter()
        .flat_map(|s| &s.containers)
        .filter(|(_, st)| !matches!(st, ContainerStatus::Missing))
        .count();
    let version = snapshot.docker_version.as_deref().unwrap_or("?");

    let mut line = format!(
        "{} {} · {running} running / {stopped} stopped · {service_containers} service {} · docker {version}",
        snapshot.sandbox_count,
        plural(snapshot.sandbox_count, "sandbox", "sandboxes"),
        plural(service_containers, "container", "containers"),
    );
    if age.as_secs() >= STALE_AFTER_SECS {
        line.push_str(&format!(" (stale {}s)", age.as_secs()));
    }
    line
}

/// `n singular` / `n plural` word choice (the count itself is rendered by the
/// caller; this only returns the noun).
fn plural<'a>(n: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if n == 1 { singular } else { plural }
}

/// True when the container's recorded config hash differs from the freshly
/// resolved one (same rule as `commands::run::warn_on_drift`). A missing label
/// or a failed inspect is treated as "no drift".
fn drifted(container: &str, expected: &str) -> bool {
    match docker::inspect(container, "{{index .Config.Labels \"devsandbox.config_hash\"}}") {
        Ok(Some(hash)) => !hash.is_empty() && hash != expected,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_secs_scales() {
        assert_eq!(humanize_secs(0), "0s");
        assert_eq!(humanize_secs(40), "40s");
        assert_eq!(humanize_secs(60), "1m");
        assert_eq!(humanize_secs(12 * 60), "12m");
        assert_eq!(humanize_secs(2 * 3600 + 5 * 60), "2h05m");
        assert_eq!(humanize_secs(3 * 86_400 + 4 * 3600 + 30 * 60), "3d4h");
    }

    #[test]
    fn classify_from_ps_lines() {
        let out = concat!(
            r#"{"Names":"devsandbox-repo-abc1","Status":"Up 3 minutes","State":"running"}"#,
            "\n",
            r#"{"Names":"devsandbox-repo-xyz2","Status":"Exited (0) 1 hour ago","State":"exited"}"#,
            "\n",
            "not json\n",
        );
        let ps = parse_ps(out);
        assert_eq!(ps.len(), 2); // bad line skipped

        assert_eq!(
            classify("devsandbox-repo-abc1", &ps),
            ContainerStatus::Running("Up 3 minutes".into()),
        );
        assert_eq!(
            classify("devsandbox-repo-xyz2", &ps),
            ContainerStatus::Exited("Exited (0) 1 hour ago".into()),
        );
        assert_eq!(classify("devsandbox-nope", &ps), ContainerStatus::Missing);
    }

    #[test]
    fn classify_matches_multiname() {
        let out =
            r#"{"Names":"other-name,devsandbox-repo-abc1","Status":"Up 1s","State":"running"}"#;
        let ps = parse_ps(out);
        assert_eq!(
            classify("devsandbox-repo-abc1", &ps),
            ContainerStatus::Running("Up 1s".into()),
        );
    }

    #[test]
    fn stats_parse_maps_by_name() {
        let out = concat!(
            r#"{"Name":"devsandbox-repo-abc1","CPUPerc":"1.20%","MemUsage":"50MiB / 2GiB"}"#,
            "\n",
            "garbage\n",
        );
        let stats = parse_stats(out);
        assert_eq!(
            stats.get("devsandbox-repo-abc1"),
            Some(&("1.20%".to_string(), "50MiB / 2GiB".to_string())),
        );
        assert_eq!(stats.len(), 1);
    }

    fn ps_line(names: &str, status: &str, state: &str) -> PsLine {
        PsLine {
            names: names.to_string(),
            status: status.to_string(),
            state: state.to_string(),
        }
    }

    fn resolved(scope: ServiceScope, source: &str, ports: &[&str]) -> ResolvedServiceInput {
        ResolvedServiceInput {
            scope,
            source: source.to_string(),
            ports: ports.iter().map(|p| p.to_string()).collect(),
            env_len: 2,
            command: Some("redis-server".to_string()),
            config_hash: "deadbeef".to_string(),
        }
    }

    #[test]
    fn global_service_joins_single_container() {
        let inputs = vec![ServiceInput {
            name: "db".into(),
            resolved: Some(resolved(ServiceScope::Global, "image postgres:16", &["5432"])),
        }];
        let instance_services = vec![
            ("repo".to_string(), vec!["db".to_string()]),
            ("other".to_string(), vec![]),
        ];
        // Global container name is devsandbox-svc-<project>-<name>.
        let ps = vec![ps_line("devsandbox-svc-proj-db", "Up 2 minutes", "running")];

        let rows = build_service_rows("proj", &inputs, &instance_services, &ps);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.scope, "global");
        assert_eq!(row.source, "image postgres:16");
        assert_eq!(row.used_by, vec!["repo".to_string()]);
        assert_eq!(row.containers.len(), 1);
        assert_eq!(row.containers[0].0, "devsandbox-svc-proj-db");
        assert_eq!(
            row.containers[0].1,
            ContainerStatus::Running("Up 2 minutes".into())
        );
    }

    #[test]
    fn isolated_service_has_one_container_per_user() {
        let inputs = vec![ServiceInput {
            name: "cache".into(),
            resolved: Some(resolved(ServiceScope::Isolated, "image redis:7", &["6379"])),
        }];
        let instance_services = vec![
            ("repo".to_string(), vec!["cache".to_string()]),
            ("repo-2".to_string(), vec!["cache".to_string()]),
            ("nope".to_string(), vec![]),
        ];
        // repo's container is running, repo-2's is exited, none for `nope`.
        let ps = vec![
            ps_line("devsandbox-svc-proj-repo-cache", "Up 1s", "running"),
            ps_line(
                "devsandbox-svc-proj-repo-2-cache",
                "Exited (0) 5s ago",
                "exited",
            ),
        ];

        let rows = build_service_rows("proj", &inputs, &instance_services, &ps);
        let row = &rows[0];
        assert_eq!(row.scope, "isolated");
        assert_eq!(row.used_by, vec!["repo".to_string(), "repo-2".to_string()]);
        assert_eq!(row.containers.len(), 2);
        assert_eq!(row.containers[0].0, "devsandbox-svc-proj-repo-cache");
        assert_eq!(row.containers[0].1, ContainerStatus::Running("Up 1s".into()));
        assert_eq!(row.containers[1].0, "devsandbox-svc-proj-repo-2-cache");
        assert_eq!(
            row.containers[1].1,
            ContainerStatus::Exited("Exited (0) 5s ago".into())
        );
    }

    #[test]
    fn unresolved_service_renders_placeholder_row() {
        let inputs = vec![ServiceInput { name: "broken".into(), resolved: None }];
        let rows = build_service_rows("proj", &inputs, &[], &[]);
        let row = &rows[0];
        assert_eq!(row.source, "?");
        assert_eq!(row.scope, "isolated"); // default when unresolved
        assert!(row.containers.is_empty());
        assert!(row.config_hash.is_empty());
        assert_eq!(row.env_len, 0);
        assert!(row.command.is_none());
    }

    fn inst(name: &str, status: ContainerStatus) -> InstanceRow {
        InstanceRow {
            name: name.into(),
            sandbox: "s".into(),
            container: format!("devsandbox-{name}"),
            status,
            uptime_secs: 0,
            cpu: None,
            mem: None,
            folder: "/f".into(),
            worktree: false,
            services: Vec::new(),
            workspace: "/w".into(),
            remote_user: None,
            remote_env_len: 0,
            base_folder: "/f".into(),
            drift: false,
        }
    }

    fn svc_row(containers: Vec<ContainerStatus>) -> ServiceRow {
        ServiceRow {
            name: "db".into(),
            scope: "global",
            source: "image x".into(),
            ports: Vec::new(),
            containers: containers
                .into_iter()
                .enumerate()
                .map(|(i, s)| (format!("c{i}"), s))
                .collect(),
            used_by: Vec::new(),
            env_len: 0,
            command: None,
            config_hash: String::new(),
        }
    }

    fn snap(
        instances: Vec<InstanceRow>,
        services: Vec<ServiceRow>,
        sandbox_count: usize,
        docker_version: Option<&str>,
    ) -> Snapshot {
        Snapshot {
            instances,
            sandboxes: Vec::new(),
            services,
            sandbox_count,
            docker_version: docker_version.map(str::to_string),
            collected_at: Instant::now(),
            error: None,
        }
    }

    #[test]
    fn totals_line_counts_running_stopped_and_service_containers() {
        let s = snap(
            vec![
                inst("a", ContainerStatus::Running("Up".into())),
                inst("b", ContainerStatus::Exited("Exited".into())),
                inst("c", ContainerStatus::Missing),
            ],
            vec![svc_row(vec![
                ContainerStatus::Running("Up".into()),
                ContainerStatus::Missing,
            ])],
            3,
            Some("24.0.7"),
        );
        assert_eq!(
            totals_line(&s, std::time::Duration::from_secs(0)),
            "3 sandboxes · 1 running / 2 stopped · 1 service container · docker 24.0.7",
        );
    }

    #[test]
    fn totals_line_pluralizes_and_marks_missing_version() {
        let s = snap(vec![inst("a", ContainerStatus::Running("Up".into()))], Vec::new(), 1, None);
        assert_eq!(
            totals_line(&s, std::time::Duration::from_secs(0)),
            "1 sandbox · 1 running / 0 stopped · 0 service containers · docker ?",
        );
    }

    #[test]
    fn totals_line_flags_stale_data() {
        let s = snap(Vec::new(), Vec::new(), 0, Some("24.0.7"));
        let out = totals_line(&s, std::time::Duration::from_secs(STALE_AFTER_SECS + 2));
        assert!(out.ends_with(" (stale 12s)"), "got: {out}");
        // Fresh data has no staleness suffix.
        let fresh = totals_line(&s, std::time::Duration::from_secs(1));
        assert!(!fresh.contains("stale"), "got: {fresh}");
    }

    #[test]
    fn isolated_service_with_no_users_has_no_containers() {
        let inputs = vec![ServiceInput {
            name: "cache".into(),
            resolved: Some(resolved(ServiceScope::Isolated, "image redis:7", &["6379"])),
        }];
        let rows = build_service_rows("proj", &inputs, &[], &[]);
        assert!(rows[0].used_by.is_empty());
        assert!(rows[0].containers.is_empty());
    }

    fn sb(name: &str) -> SandboxRow {
        SandboxRow {
            name: name.into(),
            source: "image x".into(),
            folder: None,
            services: Vec::new(),
            extends: Vec::new(),
            config_hash: "hash".into(),
        }
    }

    /// An instance in a given sandbox (extends the `inst` helper, which pins
    /// sandbox to "s").
    fn inst_in(name: &str, sandbox: &str) -> InstanceRow {
        let mut row = inst(name, ContainerStatus::Missing);
        row.sandbox = sandbox.into();
        row
    }

    fn collapsed(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// The no-processes-expanded argument pair for `visible_nodes` callers that
    /// only exercise the sandbox/instance levels.
    fn no_procs() -> (BTreeSet<String>, BTreeMap<String, ProcState>) {
        (BTreeSet::new(), BTreeMap::new())
    }

    #[test]
    fn visible_nodes_expands_sandboxes_with_instances() {
        let sandboxes = vec![sb("a"), sb("b")];
        let instances = vec![
            inst_in("a1", "a"),
            inst_in("b1", "b"),
            inst_in("a2", "a"),
        ];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &ep, &pm);
        assert_eq!(
            nodes,
            vec![
                Node::Sandbox(0),
                Node::Instance(0), // a1
                Node::Instance(2), // a2 (instances order preserved)
                Node::Sandbox(1),
                Node::Instance(1), // b1
            ],
        );
    }

    #[test]
    fn visible_nodes_empty_sandbox_gets_marker() {
        let sandboxes = vec![sb("a")];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &[], &BTreeSet::new(), &ep, &pm);
        assert_eq!(nodes, vec![Node::Sandbox(0), Node::Empty(0)]);
    }

    #[test]
    fn visible_nodes_collapsed_sandbox_hides_children() {
        let sandboxes = vec![sb("a"), sb("b")];
        let instances = vec![inst_in("a1", "a"), inst_in("b1", "b")];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &instances, &collapsed(&["a"]), &ep, &pm);
        // a is collapsed (no children, not even a marker); b stays expanded.
        assert_eq!(
            nodes,
            vec![Node::Sandbox(0), Node::Sandbox(1), Node::Instance(1)],
        );
    }

    #[test]
    fn visible_nodes_orphans_group_at_bottom() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a"), inst_in("x1", "gone")];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &ep, &pm);
        assert_eq!(
            nodes,
            vec![
                Node::Sandbox(0),
                Node::Instance(0),
                Node::Orphans,
                Node::Instance(1),
            ],
        );

        // Collapsing the orphan group hides its children but keeps the header.
        let nodes = visible_nodes(&sandboxes, &instances, &collapsed(&[ORPHANS_NAME]), &ep, &pm);
        assert_eq!(
            nodes,
            vec![Node::Sandbox(0), Node::Instance(0), Node::Orphans],
        );
    }

    #[test]
    fn visible_nodes_no_orphan_group_when_all_configured() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a")];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &ep, &pm);
        assert!(!nodes.contains(&Node::Orphans));
    }

    fn proc_row(pid: &str) -> super::super::procs::ProcRow {
        super::super::procs::ProcRow { pid: pid.into(), depth: 0, args: "x".into() }
    }

    #[test]
    fn visible_nodes_emits_proc_rows_under_expanded_instance() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a")];
        let expanded: BTreeSet<String> = ["a1".to_string()].into_iter().collect();
        let mut procs = BTreeMap::new();
        procs.insert(
            "a1".to_string(),
            ProcState::Rows(vec![proc_row("1"), proc_row("2")]),
        );
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &expanded, &procs);
        assert_eq!(
            nodes,
            vec![
                Node::Sandbox(0),
                Node::Instance(0),
                Node::Proc { instance: 0, row: 0 },
                Node::Proc { instance: 0, row: 1 },
            ],
        );
    }

    #[test]
    fn visible_nodes_message_row_when_not_fetched_or_error() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a")];
        let expanded: BTreeSet<String> = ["a1".to_string()].into_iter().collect();

        // Not fetched yet: one placeholder row.
        let nodes =
            visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &expanded, &BTreeMap::new());
        assert_eq!(nodes.last(), Some(&Node::Proc { instance: 0, row: MESSAGE_ROW }));

        // Message state (e.g. not running / error): still one placeholder row.
        let mut procs = BTreeMap::new();
        procs.insert("a1".to_string(), ProcState::Message("(not running)".into()));
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &expanded, &procs);
        assert_eq!(
            nodes,
            vec![
                Node::Sandbox(0),
                Node::Instance(0),
                Node::Proc { instance: 0, row: MESSAGE_ROW },
            ],
        );
    }

    #[test]
    fn visible_nodes_collapsed_sandbox_hides_expanded_procs() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a")];
        let expanded: BTreeSet<String> = ["a1".to_string()].into_iter().collect();
        let mut procs = BTreeMap::new();
        procs.insert("a1".to_string(), ProcState::Rows(vec![proc_row("1")]));
        // Sandbox collapsed → its instance and procs are hidden entirely.
        let nodes = visible_nodes(&sandboxes, &instances, &collapsed(&["a"]), &expanded, &procs);
        assert_eq!(nodes, vec![Node::Sandbox(0)]);
    }

    #[test]
    fn sandbox_stats_formats_running_over_total() {
        assert_eq!(sandbox_stats(0, 0), "0/0 running");
        assert_eq!(sandbox_stats(3, 1), "1/3 running");
    }
}
