//! UI-independent snapshot collection: joins docker/state/config into a
//! [`Snapshot`] that the TUI and any machine-readable consumer share verbatim.
//! Plain blocking I/O (run off the UI thread by the TUI); parsing lives in free
//! functions to stay unit-testable without docker.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::commands::{container_drifted, drift_decision};
use crate::commands::services::{isolated_service_container, project_id, service_container};
use crate::config::{
    build_hash, parse_shorthand, Config, Mount, MountContext, ResolvedSandbox, ServiceScope,
    INSTANCE_VARS,
};
use crate::runtime::{backend, ContainerRow, NAME_PREFIX};
use crate::state::{Instance, State};

/// Container liveness, joined from `docker ps` against the state's instance list.
/// Serializes as an internally-tagged object so JSON consumers switch on
/// `state` and read the runtime text from `text` (absent for `missing`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "text", rename_all = "lowercase")]
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
#[derive(Clone, Debug, Serialize)]
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
    /// Persistent id (`state::Instance::instance_id`), what `dispatcher`
    /// refers to.
    pub instance_id: String,
    /// `instance_id` of the dispatcher that created this instance
    /// (docs/automations.md), `None` for user-created ones. May name an
    /// instance that no longer exists (an orphaned child).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatcher: Option<String>,
}

/// One sandbox row from `config.toml`, everything the Instances tree needs to
/// render a sandbox node. Every configured sandbox gets one, even with zero
/// instances; a sandbox that fails to resolve still gets a row with source `?`.
#[derive(Clone, Debug, Serialize)]
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
    /// Short hash of the build dockerfile's contents, empty when the sandbox is
    /// image-based, has no dockerfile, or failed to resolve. Drift-only; carried
    /// here so the instance join computes it once per sandbox, not per instance.
    pub build_hash: String,
    /// Config-validation problems for this sandbox (folder / mount resolution),
    /// shown in the Detail panel. Empty when the sandbox validates or failed to
    /// resolve at all (the resolve error is reported separately).
    pub issues: Vec<String>,
    /// Whether the sandbox has a `dispatcher` table (the TUI's TYPE column).
    #[serde(skip)]
    pub dispatcher: bool,
}

/// One service row, everything the Services view needs pre-joined.
#[derive(Clone, Debug, Serialize)]
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
    /// True when any backing container drifted from the current config or
    /// dockerfile (OR over `containers`). Computed in the pure join from the
    /// labels the `ps` listing already carries, so it costs zero extra docker
    /// calls. `service ls --json` serializes it for free.
    pub drift: bool,
}

/// A point-in-time view of instances plus any collection error (docker/config
/// unavailable). Rows are always present from state even when docker is down.
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub instances: Vec<InstanceRow>,
    /// Sandboxes from `config.toml`, in config order. Empty when config is
    /// absent or unreadable. Roots of the Instances tree.
    pub sandboxes: Vec<SandboxRow>,
    pub services: Vec<ServiceRow>,
    /// Number of sandboxes defined in `config.toml` (0 when config is absent or
    /// unreadable). Feeds the header totals line.
    pub sandbox_count: usize,
    /// Runtime name for the header (`docker`, `podman`, `container`).
    pub runtime_name: &'static str,
    /// Runtime server version (e.g. `24.0.7`), or `None` when it is down or
    /// the version probe failed. Feeds the header totals line.
    pub runtime_version: Option<String>,
    /// When collection finished. Drives the staleness indicator in the header.
    /// Skipped in JSON: it is a monotonic `Instant` with no wall-clock meaning
    /// off-process; a JSON consumer prints at collection time itself.
    #[serde(skip)]
    pub collected_at: Instant,
    pub error: Option<String>,
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
    /// Short hash of the service's dockerfile contents, empty when image-based or
    /// no dockerfile. Drift-only; resolved once per service by `service_rows` (it
    /// has the `dir`) so the pure join can apply the shared drift rule.
    build_hash: String,
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

/// Resolve every configured service into [`ServiceInput`]s and join them against
/// docker state, returning the rows plus one message per service that failed to
/// resolve (`?` placeholder rows still render). The config resolution is the only
/// impure-ish part (pure config work, no docker); the docker listing `ps` and the
/// `instance_services` pairing are passed in so `collect` and `commands::services::ls`
/// build the Services view identically and cannot drift. `dir` is the config
/// root, used to hash each service's dockerfile for drift. `project` scopes
/// container names; `instance_services` is `(display name, persistent id,
/// services of the resolved sandbox)` per relevant instance — container names
/// join on the id, `used_by` shows the name.
pub fn service_rows(
    dir: &Path,
    project: &str,
    config: &Config,
    instance_services: &[(String, String, Vec<String>)],
    ps: &[ContainerRow],
) -> (Vec<ServiceRow>, Vec<String>) {
    let mut errors = Vec::new();
    let inputs: Vec<ServiceInput> = config
        .services
        .keys()
        .map(|name| match config.resolve_service(name) {
            Ok(rs) => ServiceInput {
                name: name.clone(),
                resolved: Some(ResolvedServiceInput {
                    scope: rs.spec.scope,
                    source: service_source(&rs.spec),
                    ports: rs.spec.ports.clone(),
                    env_len: rs.spec.env.len(),
                    command: rs.spec.command.as_ref().map(|c| c.to_vec().join(" ")),
                    config_hash: rs.config_hash,
                    build_hash: build_hash(dir, rs.spec.build.as_ref()),
                }),
            },
            Err(e) => {
                errors.push(format!("service `{name}`: {e:#}"));
                ServiceInput { name: name.clone(), resolved: None }
            }
        })
        .collect();
    (build_service_rows(project, &inputs, instance_services, ps), errors)
}

/// Join config services against docker state. Pure: all docker/config I/O is
/// done by the caller and passed in. `project` scopes container names;
/// `instance_services` is `(display name, persistent id, services)` per live
/// instance — isolated container names derive from the id (they survive
/// renames), while `used_by` reports the display name.
fn build_service_rows(
    project: &str,
    services: &[ServiceInput],
    instance_services: &[(String, String, Vec<String>)],
    ps: &[ContainerRow],
) -> Vec<ServiceRow> {
    services
        .iter()
        .map(|svc| {
            let users: Vec<(&str, &str)> = instance_services
                .iter()
                .filter(|(_, _, list)| list.iter().any(|s| s == &svc.name))
                .map(|(name, id, _)| (name.as_str(), id.as_str()))
                .collect();
            let used_by: Vec<String> = users.iter().map(|(name, _)| name.to_string()).collect();

            let (scope, source, ports, env_len, command, config_hash, build_hash) =
                match &svc.resolved {
                    Some(r) => (
                        r.scope,
                        r.source.clone(),
                        r.ports.clone(),
                        r.env_len,
                        r.command.clone(),
                        r.config_hash.clone(),
                        r.build_hash.clone(),
                    ),
                    None => (
                        ServiceScope::Isolated,
                        "?".to_string(),
                        Vec::new(),
                        0,
                        None,
                        String::new(),
                        String::new(),
                    ),
                };

            let containers: Vec<(String, ContainerStatus)> = match scope {
                ServiceScope::Global => {
                    let c = service_container(project, &svc.name);
                    let status = classify(&c, ps);
                    vec![(c, status)]
                }
                ServiceScope::Isolated => users
                    .iter()
                    .map(|(_, id)| {
                        let c = isolated_service_container(project, id, &svc.name);
                        let status = classify(&c, ps);
                        (c, status)
                    })
                    .collect(),
            };

            // Drift: OR the shared drift rule over every backing container found
            // in the `ps` listing. The labels are already on the row (`gc` reads
            // them the same way), so this join adds no docker calls. A missing
            // container or missing label contributes nothing — the lenient rule
            // shared with the instance drift path.
            let drift = containers.iter().any(|(name, _)| {
                service_container_drifted(name, &config_hash, &build_hash, ps)
            });

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
                drift,
            }
        })
        .collect()
}

/// Whether one backing service container drifted, applying the shared
/// [`drift_decision`] rule against the labels the `ps` listing already carries.
/// A container absent from `ps` (never created, or removed) is not drift — you
/// can't be stale against a config you were never built from. Zero docker calls:
/// the labels ride along on the listing rows.
fn service_container_drifted(
    container: &str,
    expected_config: &str,
    expected_build: &str,
    ps: &[ContainerRow],
) -> bool {
    match ps.iter().find(|row| row.name == container) {
        Some(row) => drift_decision(
            row.label("devsandbox.config_hash"),
            expected_config,
            row.label("devsandbox.build_hash"),
            expected_build,
        ),
        None => false,
    }
}

/// Classify a container by name against the runtime's listing. Treat
/// `running` as up, every other state (created/exited/stopped/…) as stopped.
fn classify(container: &str, ps: &[ContainerRow]) -> ContainerStatus {
    for row in ps {
        if row.name == container {
            if row.is_running() {
                return ContainerStatus::Running(row.status.clone());
            }
            return ContainerStatus::Exited(row.status.clone());
        }
    }
    ContainerStatus::Missing
}

/// Validate a resolved sandbox's host-facing paths, mirroring what
/// `commands::run` does at run time but reporting instead of acting:
///
/// - **source**: must have an `image` or a `build.dockerfile` (i.e. the same
///   condition that makes [`ResolvedSandbox::source`] return `?`).
/// - **folder**: must be set and resolve relative to the config dir
///   (`dir.join(folder).canonicalize()`).
/// - **mounts**: each must resolve (`${…}` substitution + a source for binds);
///   a bind source that does not exist on the host is flagged as a warning
///   (`run` would create it — see `ensure_bind_source`), except under
///   `${sharedVolumes}` or templated on the instance id: those are expected to
///   be absent until an instance has run, and the instance path is only a guess
///   here anyway.
///
/// Returns one message per problem, in check order; empty means it validates.
fn validate_sandbox(dir: &Path, sb: &ResolvedSandbox) -> Vec<String> {
    let mut issues = Vec::new();

    // Source: neither `image` nor a `build.dockerfile` — the same condition that
    // renders `?` in the source column. Tie the red line to that marker so the
    // user sees what is missing rather than just the `?`.
    if sb.source() == "?" {
        issues.push("no `image` or `build.dockerfile` set".to_string());
    }

    // Folder: presence + resolution relative to the config dir. On success the
    // canonical path + basename anchor the mount context below.
    let folder = match sb.folder() {
        None => {
            issues.push("folder: not set".to_string());
            None
        }
        Some(f) => match dir.join(f).canonicalize() {
            Ok(path) => Some(path),
            Err(_) => {
                issues.push(format!("folder `{f}` does not resolve relative to config"));
                None
            }
        },
    };

    // Mounts: only when the sandbox declares any. `${configDir}` /
    // `${localWorkspaceFolder}` mirror the run-time context; a folder that did
    // not resolve leaves the workspace vars empty, so those mounts still surface
    // as unresolved rather than silently passing.
    if let Some(mounts) = &sb.properties.mounts {
        let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let folder_str = folder.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let basename = folder
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let shared_volumes = config_dir.join("shared-volumes");
        let ctx = MountContext {
            config_dir: &config_dir.to_string_lossy(),
            workspace_folder: &folder_str,
            workspace_folder_basename: &basename,
            shared_volumes: &shared_volumes.to_string_lossy(),
            // The sandbox name doubles as the default first-instance name; a
            // real instance name only exists at run time.
            instance: &sb.name,
        };
        for mount in mounts {
            match mount.resolve(&ctx) {
                Ok(rm) if rm.kind == "bind" => {
                    if let Some(source) = &rm.source
                        && !Path::new(source).exists()
                        && !Path::new(source).starts_with(&shared_volumes)
                        && !mount_source_uses_instance(mount)
                    {
                        issues.push(format!(
                            "mount source `{source}` not found on host (run creates it)"
                        ));
                    }
                }
                Ok(_) => {}
                Err(e) => issues.push(format!("mount: {e}")),
            }
        }
    }

    issues
}

/// Whether a mount's unsubstituted source references the instance id.
fn mount_source_uses_instance(mount: &Mount) -> bool {
    let source = match mount {
        Mount::Shorthand(s) => parse_shorthand(s).ok().and_then(|parts| parts.1),
        Mount::Object(o) => o.source.clone(),
    };
    source.is_some_and(|s| INSTANCE_VARS.iter().any(|v| s.contains(v)))
}

/// Build one [`SandboxRow`] per configured sandbox, in config order. A sandbox
/// that resolves gets its real source/folder/services/hash plus any
/// [`validate_sandbox`] issues; one that fails to resolve gets a placeholder row
/// (source `?`, empty everything) and its resolve error is returned alongside so
/// the caller can surface it (`collect` folds these into `Snapshot.error`; `ls
/// --json` ignores them, the `?` row is the signal). Pure config work — no
/// docker — shared verbatim by [`collect`] and `commands::ls`.
pub fn sandbox_rows(dir: &Path, config: &Config) -> (Vec<SandboxRow>, Vec<String>) {
    let mut rows = Vec::with_capacity(config.sandboxes.len());
    let mut errors = Vec::new();
    for name in config.sandboxes.keys() {
        let extends = config
            .sandboxes
            .get(name)
            .and_then(|t| t.get("extends"))
            .map(extends_names)
            .unwrap_or_default();
        match config.resolve_sandbox(name) {
            Ok(rs) => {
                let services = rs.properties.services.clone().unwrap_or_default();
                let issues = validate_sandbox(dir, &rs);
                let build_hash = build_hash(dir, rs.properties.build.as_ref());
                rows.push(SandboxRow {
                    name: name.clone(),
                    source: rs.source(),
                    folder: rs.folder().map(str::to_string),
                    services,
                    extends,
                    config_hash: rs.config_hash,
                    build_hash,
                    issues,
                    dispatcher: rs.properties.dispatcher.is_some(),
                });
            }
            Err(e) => {
                errors.push(format!("sandbox `{name}`: {e:#}"));
                rows.push(SandboxRow {
                    name: name.clone(),
                    source: "?".into(),
                    folder: None,
                    services: Vec::new(),
                    extends,
                    config_hash: String::new(),
                    build_hash: String::new(),
                    issues: Vec::new(),
                    dispatcher: false,
                });
            }
        }
    }
    (rows, errors)
}

/// Collect a fresh [`Snapshot`]. Blocking; run off the UI thread.
///
/// State is the source of truth for which rows exist; the runtime enriches them
/// with liveness and resource usage. When it or config is unavailable the rows
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
                runtime_name: backend().name(),
                runtime_version: None,
                collected_at,
                error: Some(format!("state: {e:#}")),
            };
        }
    };

    let mut errors: Vec<String> = Vec::new();

    // One listing and one stats call for the whole snapshot.
    let rt = backend();
    let ps = match rt.list(true, NAME_PREFIX) {
        Ok(rows) => rows,
        Err(e) => {
            errors.push(format!("{} unavailable: {e:#}", rt.name()));
            Vec::new()
        }
    };

    let stats: BTreeMap<String, (String, String)> = match rt.stats() {
        Ok(rows) => rows.into_iter().map(|s| (s.name, (s.cpu, s.mem))).collect(),
        // Only report a stats error if the listing succeeded; otherwise that
        // error already covers "runtime is down".
        Err(e) => {
            if errors.is_empty() {
                errors.push(format!("{} stats: {e:#}", rt.name()));
            }
            BTreeMap::new()
        }
    };

    // Runtime version for the header; best-effort (None when down). Kept off
    // the UI thread like every other runtime call here. Cheap enough to run
    // each collection, so no caching is threaded through.
    let runtime_version = rt.server_version().ok().filter(|v| !v.is_empty());

    // Load config once; resolve every sandbox for the tree, services + drift
    // hash. `resolved` maps sandbox name → (services, hash) for the instance join.
    let config = Config::load(dir);
    let mut resolved: BTreeMap<String, (Vec<String>, String, String)> = BTreeMap::new();
    let mut sandboxes: Vec<SandboxRow> = Vec::new();
    match &config {
        Ok(cfg) => {
            let (rows, mut resolve_errors) = sandbox_rows(dir, cfg);
            errors.append(&mut resolve_errors);
            // The instance join needs each sandbox's services + hash; take them
            // from the rows so nothing is resolved twice. Placeholder rows (empty
            // hash) are skipped to match the old resolved-only map: comparing a
            // container's hash label against "" would flag phantom drift.
            for row in rows.iter().filter(|r| !r.config_hash.is_empty()) {
                resolved.insert(
                    row.name.clone(),
                    (row.services.clone(), row.config_hash.clone(), row.build_hash.clone()),
                );
            }
            sandboxes = rows;
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
            Some((svcs, hash, build)) => {
                (svcs.clone(), drifted(&inst.container, hash, build))
            }
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
            instance_id: inst.instance_id.clone(),
            dispatcher: inst.dispatcher.clone(),
        });
    }

    // Services view: (instance name, persistent id, service list of its
    // resolved sandbox), then the config services resolved into pure inputs for
    // the join. Built from state so the id — which container names derive from
    // — rides along with the display name.
    let instance_services: Vec<(String, String, Vec<String>)> = state
        .instances
        .iter()
        .map(|(name, inst)| {
            let services = resolved
                .get(&inst.sandbox)
                .map(|(svcs, _, _)| svcs.clone())
                .unwrap_or_default();
            (name.clone(), inst.instance_id.clone(), services)
        })
        .collect();

    let services = match (&config, project_id(dir)) {
        (Ok(cfg), Ok(project)) => {
            let (rows, mut svc_errors) = service_rows(dir, &project, cfg, &instance_services, &ps);
            errors.append(&mut svc_errors);
            rows
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
        runtime_name: rt.name(),
        runtime_version,
        collected_at,
        error: if errors.is_empty() { None } else { Some(errors.join("; ")) },
    }
}

/// True when the container's recorded config *or* build hash differs from the
/// freshly resolved one (the shared [`container_drifted`] rule, so dockerfile
/// edits flag too). A missing label or a failed inspect is treated as "no
/// drift" — the TUI stays lenient rather than surfacing an inspect error as
/// phantom drift.
fn drifted(container: &str, expected_config: &str, expected_build: &str) -> bool {
    container_drifted(container, expected_config, expected_build).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_rows_empty_config_is_empty() {
        let cfg = Config::parse("").unwrap();
        let (rows, errors) = sandbox_rows(std::env::temp_dir().as_path(), &cfg);
        assert!(rows.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn sandbox_rows_resolved_and_placeholder() {
        // One sandbox with an image (resolves) and one that references a missing
        // template (fails to resolve ⇒ `?` placeholder row + one error).
        let cfg = Config::parse(
            r#"
[sandbox.web]
image = "node:22"

[sandbox.broken]
extends = "does-not-exist"
"#,
        )
        .unwrap();
        let (rows, errors) = sandbox_rows(std::env::temp_dir().as_path(), &cfg);

        assert_eq!(rows.len(), 2);
        let web = rows.iter().find(|r| r.name == "web").unwrap();
        assert_eq!(web.source, "image node:22");
        assert!(!web.config_hash.is_empty());

        let broken = rows.iter().find(|r| r.name == "broken").unwrap();
        assert_eq!(broken.source, "?");
        assert!(broken.config_hash.is_empty());
        assert_eq!(broken.extends, vec!["does-not-exist".to_string()]);
        assert!(broken.issues.is_empty());

        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(errors[0].contains("sandbox `broken`"), "got: {errors:?}");
    }

    #[test]
    fn classify_from_rows() {
        let ps = vec![
            ps_line("devsandbox-repo-abc1", "Up 3 minutes", "running"),
            ps_line("devsandbox-repo-xyz2", "Exited (0) 1 hour ago", "exited"),
        ];

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

    fn ps_line(name: &str, status: &str, state: &str) -> ContainerRow {
        ContainerRow {
            name: name.to_string(),
            status: status.to_string(),
            state: state.to_string(),
            ..Default::default()
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
            build_hash: String::new(),
        }
    }

    /// A `ps` row carrying the two drift labels, mirroring what the runtime
    /// listing attaches at container-create time.
    fn ps_labeled(name: &str, config_hash: &str, build_hash: &str) -> ContainerRow {
        let mut row = ps_line(name, "Up 1s", "running");
        row.labels
            .insert("devsandbox.config_hash".into(), config_hash.into());
        row.labels
            .insert("devsandbox.build_hash".into(), build_hash.into());
        row
    }

    #[test]
    fn global_service_joins_single_container() {
        let inputs = vec![ServiceInput {
            name: "db".into(),
            resolved: Some(resolved(ServiceScope::Global, "image postgres:16", &["5432"])),
        }];
        let instance_services = vec![
            ("repo".to_string(), "repo".to_string(), vec!["db".to_string()]),
            ("other".to_string(), "other".to_string(), vec![]),
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
        // `renamed` was created as `repo-2` then renamed: containers keep the
        // id-derived name while `used_by` shows the display name.
        let instance_services = vec![
            ("repo".to_string(), "repo".to_string(), vec!["cache".to_string()]),
            ("renamed".to_string(), "repo-2".to_string(), vec!["cache".to_string()]),
            ("nope".to_string(), "nope".to_string(), vec![]),
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
        assert_eq!(row.used_by, vec!["repo".to_string(), "renamed".to_string()]);
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

    /// Drift builder: a global service whose container's `config_hash` label
    /// differs from the resolved hash flags drift; matching labels do not; a
    /// missing label (pre-upgrade container) stays lenient; and a differing
    /// build hash flags even when the config hash matches.
    #[test]
    fn service_drift_follows_container_labels() {
        let mut input = resolved(ServiceScope::Global, "dockerfile Dockerfile", &["5432"]);
        input.config_hash = "cfg".into();
        input.build_hash = "bld".into();
        let inputs = vec![ServiceInput { name: "db".into(), resolved: Some(input) }];
        let instance_services =
            vec![("repo".to_string(), "repo".to_string(), vec!["db".to_string()])];
        let name = "devsandbox-svc-proj-db";

        // Both labels match → no drift.
        let ps = vec![ps_labeled(name, "cfg", "bld")];
        assert!(!build_service_rows("proj", &inputs, &instance_services, &ps)[0].drift);

        // Config label differs → drift.
        let ps = vec![ps_labeled(name, "old", "bld")];
        assert!(build_service_rows("proj", &inputs, &instance_services, &ps)[0].drift);

        // Build label differs (config matches) → drift.
        let ps = vec![ps_labeled(name, "cfg", "old")];
        assert!(build_service_rows("proj", &inputs, &instance_services, &ps)[0].drift);

        // No labels at all (pre-upgrade container) → lenient, no drift.
        let ps = vec![ps_line(name, "Up 1s", "running")];
        assert!(!build_service_rows("proj", &inputs, &instance_services, &ps)[0].drift);

        // Container absent from ps → no drift (nothing to be stale against).
        assert!(!build_service_rows("proj", &inputs, &instance_services, &[])[0].drift);
    }

    /// Isolated drift ORs over every backing container: one stale instance
    /// container flags the whole row.
    #[test]
    fn isolated_service_drift_ors_over_containers() {
        let mut input = resolved(ServiceScope::Isolated, "dockerfile Dockerfile", &["6379"]);
        input.config_hash = "cfg".into();
        input.build_hash = String::new();
        let inputs = vec![ServiceInput { name: "cache".into(), resolved: Some(input) }];
        let instance_services = vec![
            ("repo".to_string(), "repo".to_string(), vec!["cache".to_string()]),
            ("repo-2".to_string(), "repo-2".to_string(), vec!["cache".to_string()]),
        ];
        // repo current, repo-2 stale on config.
        let ps = vec![
            ps_labeled("devsandbox-svc-proj-repo-cache", "cfg", ""),
            ps_labeled("devsandbox-svc-proj-repo-2-cache", "old", ""),
        ];
        assert!(build_service_rows("proj", &inputs, &instance_services, &ps)[0].drift);
    }

    #[test]
    fn validate_sandbox_flags_folder_and_mount_problems() {
        use crate::config::{Mount, ResolvedSandbox, SandboxProperties};

        // Unique temp config dir with real `repo` and `data` folders inside it.
        let root = std::env::temp_dir().join(format!("devsandbox-validate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("repo")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();

        // `mk` gives a valid source (image) so cases isolate folder/mount checks.
        let mk = |folder: Option<&str>, mounts: Option<Vec<Mount>>| ResolvedSandbox {
            name: "s".into(),
            properties: SandboxProperties {
                image: Some("node:22".into()),
                folder: folder.map(str::to_string),
                mounts,
                ..Default::default()
            },
            config_hash: "h".into(),
        };

        // No image and no build ⇒ the source issue (matches the `?` column).
        let no_source = ResolvedSandbox {
            name: "s".into(),
            properties: SandboxProperties { folder: Some("repo".into()), ..Default::default() },
            config_hash: "h".into(),
        };
        assert_eq!(
            validate_sandbox(&root, &no_source),
            vec!["no `image` or `build.dockerfile` set".to_string()],
        );

        // Resolvable folder + an existing bind source ⇒ no issues.
        let ok = mk(
            Some("repo"),
            Some(vec![Mount::Shorthand(format!(
                "source={},target=/data",
                root.join("data").display()
            ))]),
        );
        assert!(validate_sandbox(&root, &ok).is_empty());

        // Missing folder value ⇒ a single folder issue.
        assert_eq!(
            validate_sandbox(&root, &mk(None, None)),
            vec!["folder: not set".to_string()],
        );

        // Bogus folder + missing bind source ⇒ both messages, in check order.
        let bad = mk(
            Some("nope"),
            Some(vec![Mount::Shorthand("source=/no/such/path,target=/x".into())]),
        );
        let issues = validate_sandbox(&root, &bad);
        assert_eq!(issues.len(), 2, "got: {issues:?}");
        assert!(issues[0].contains("folder `nope` does not resolve"), "got: {issues:?}");
        assert!(issues[1].contains("not found on host"), "got: {issues:?}");

        // Auto-created sources (shared volumes, per-instance paths) never warn.
        let auto = mk(
            Some("repo"),
            Some(vec![
                Mount::Shorthand("source=${sharedVolumes}/agent,target=/a".into()),
                Mount::Shorthand("source=/no/such/${instance}/x,target=/b".into()),
                Mount::Shorthand("source=/no/such/${devcontainerId},target=/c".into()),
            ]),
        );
        assert!(validate_sandbox(&root, &auto).is_empty(), "{:?}", validate_sandbox(&root, &auto));

        let _ = std::fs::remove_dir_all(&root);
    }
}
