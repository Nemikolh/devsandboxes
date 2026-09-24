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

use std::collections::BTreeSet;

use crate::config::{Config, ServiceScope};
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
    /// service name (its network alias / `/etc/hosts` entry).
    ViaInstance { key: String, container: String, alias: String },
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
                    if let Some(via) = via_instance(config, state, running, key, service) {
                        plans.push(via);
                    }
                    let id = instance_id(state, key);
                    plans.push(Planned::Inject {
                        service_container: services::isolated_service_container(project, id, service),
                    });
                }
                ServiceScope::Global => {
                    let via = instance_key
                        .and_then(|key| via_instance(config, state, running, key, service))
                        .or_else(|| first_referencing(config, state, running, service));
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

/// The persistent id for a state key (falls back to the key when the instance
/// is absent, which only happens for a stale user-supplied key).
fn instance_id<'a>(state: &'a State, key: &'a str) -> &'a str {
    state.instances.get(key).map(|i| i.instance_id.as_str()).unwrap_or(key)
}

/// A `ViaInstance` candidate for `key` iff that instance exists, is running,
/// belongs to this config root, and its sandbox references `service`.
fn via_instance(
    config: &Config,
    state: &State,
    running: &BTreeSet<String>,
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
    })
}

/// First (by state key) running instance of this config root that references
/// the `service`, as a `ViaInstance`. Deterministic tie-break: `state.instances`
/// is a `BTreeMap`, so iteration is by key.
fn first_referencing(
    config: &Config,
    state: &State,
    running: &BTreeSet<String>,
    service: &str,
) -> Option<Planned> {
    state.instances.iter().find_map(|(key, info)| {
        (running.contains(key) && references(config, &info.sandbox, service)).then(|| {
            Planned::ViaInstance {
                key: key.clone(),
                container: info.container.clone(),
                alias: service.to_string(),
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
/// (re)spawn of its bridge. Unix-only, since [`Route`] is.
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
                Planned::ViaInstance { key, container, alias } => {
                    let info = state.instances.get(&key).ok_or_else(|| format!("no sandbox instance `{key}`"))?;
                    match crate::devsbd::ensure_recorded(&key, info, true) {
                        Some(arch) => {
                            return Ok(crate::devsbd::forward::Route {
                                container,
                                arch: Some(arch),
                                host: alias.clone(),
                                port,
                                label: format!("{alias}:{port} (via instance {key})"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Instance;

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
            shell_history: None,
            workspace: String::new(),
            workspace_file: None,
            remote_env: Default::default(),
            remote_user: None,
            ssh_auth_sock: None,
            devsbd_arch: None,
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
            }
        );
    }
}
