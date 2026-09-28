use std::path::Path;

use super::{run, services, start};
use crate::config::{Autostart, Config};
use crate::runtime::backend;
use crate::state::State;

/// Bring up every `autostart` sandbox of the config root in `dir`, once per
/// host boot (docs/automations.md). Called after `run`/`start` and on TUI
/// load; never fails the caller — problems are warnings on stderr. No
/// loadable config or no readable boot id is a silent no-op.
///
/// The boot is recorded (and saved) *before* acting, so a start that keeps
/// failing can't retrigger on every invocation; the next boot retries.
pub fn autostart(dir: &Path) {
    let Ok(config) = Config::load(dir) else { return };
    let Some(boot) = boot_id() else { return };
    let Ok(project) = services::project_id(dir) else { return };
    let mut state = match State::load() {
        Ok(state) => state,
        Err(e) => {
            eprintln!("warning: autostart: {e:#}");
            return;
        }
    };
    if !due(state.autostart_boot.get(&project).map(String::as_str), &boot) {
        return;
    }

    let sandboxes: Vec<String> = config
        .sandboxes
        .keys()
        .filter(|name| match config.resolve_sandbox(name) {
            Ok(sandbox) => sandbox.properties.autostart.unwrap_or_default() != Autostart::Off,
            // Surfaced by the command that uses it; autostart just skips it.
            Err(_) => false,
        })
        .cloned()
        .collect();
    // Not recorded: adding `autostart` later this boot must still take effect.
    if sandboxes.is_empty() {
        return;
    }
    // Not recorded either: with the runtime down (Docker Desktop not up yet,
    // Apple `container system start` not run) every inspect reads as "no such
    // container", so acting now would only print misleading notes and burn
    // this boot's pass.
    if let Err(e) = backend().list(false, crate::runtime::NAME_PREFIX) {
        eprintln!(
            "warning: autostart: {} is not reachable ({e:#}); retried on the next \
             `run`/`start` or dashboard load",
            runtime_label(backend().name())
        );
        return;
    }

    state.autostart_boot.insert(project.clone(), boot);
    if let Err(e) = state.save() {
        // Acting without the record would repeat on every invocation.
        eprintln!("warning: autostart skipped: {e:#}");
        return;
    }

    let rows: Vec<Row> = state
        .instances
        .iter()
        .map(|(key, info)| Row { key, sandbox: &info.sandbox, project: &info.project })
        .collect();
    let actions = actions(&project, &sandboxes, &rows, |key| {
        match backend().is_running(&state.instances[key].container) {
            Ok(Some(true)) => Liveness::Running,
            Ok(Some(false)) => Liveness::Stopped,
            Ok(None) => Liveness::Missing,
            Err(e) => {
                eprintln!("warning: autostart: cannot inspect `{key}`: {e:#}");
                Liveness::Unknown
            }
        }
    });

    for action in actions {
        match action {
            Action::Start(key) => {
                println!("autostart: starting {key}");
                let info = &state.instances[&key];
                if let Err(e) = start::start_instance(dir, &key, info) {
                    eprintln!("warning: autostart: cannot start `{key}`: {e:#}");
                }
            }
            Action::Run(sandbox) => {
                println!("autostart: creating an instance of {sandbox}");
                let extras = Default::default();
                if let Err(e) = run::run(dir, Some(sandbox.clone()), None, None, None, extras) {
                    eprintln!("warning: autostart: cannot run `{sandbox}`: {e:#}");
                }
            }
            Action::SkipMissing(key) => {
                eprintln!(
                    "note: autostart: `{key}` has no container; `devsandbox run` recreates it"
                );
            }
        }
    }
}

/// An instance row from state, as the decision sees it.
struct Row<'a> {
    key: &'a str,
    sandbox: &'a str,
    project: &'a str,
}

/// Container state of an instance. `Unknown` (the runtime query failed) is
/// left alone but still counts as an existing instance, so no duplicate is
/// created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Running,
    Stopped,
    Missing,
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Start this stopped instance (full `start` path).
    Start(String),
    /// Create a first instance of this sandbox (`run` semantics).
    Run(String),
    /// The instance's container is gone; tell the user instead of recreating.
    SkipMissing(String),
}

/// Whether this boot still needs an autostart pass for the project.
fn due(recorded: Option<&str>, boot: &str) -> bool {
    recorded != Some(boot)
}

/// What to do for each autostart sandbox (in `sandboxes` order): its
/// instances from `project` are started when stopped, skipped when running,
/// noted when containerless; a sandbox with none gets a `Run`. Instances of
/// other config roots are ignored. `liveness` is only queried for relevant
/// instances, so unrelated containers cost no runtime call.
fn actions(
    project: &str,
    sandboxes: &[String],
    rows: &[Row],
    mut liveness: impl FnMut(&str) -> Liveness,
) -> Vec<Action> {
    let mut out = Vec::new();
    for sandbox in sandboxes {
        let mut any = false;
        for row in rows.iter().filter(|r| r.sandbox == sandbox && r.project == project) {
            any = true;
            match liveness(row.key) {
                Liveness::Stopped => out.push(Action::Start(row.key.to_string())),
                Liveness::Missing => out.push(Action::SkipMissing(row.key.to_string())),
                Liveness::Running | Liveness::Unknown => {}
            }
        }
        if !any {
            out.push(Action::Run(sandbox.clone()));
        }
    }
    out
}

/// Restart policy an `autostart = "runtime"` container carries; `unless-stopped`
/// so a container the user stopped stays stopped across a runtime restart.
const RUNTIME_RESTART: &str = "unless-stopped";

/// The `--restart` policy to create a container with (only on backends that
/// support one) and the warnings to print at `run`. Podman only honors the
/// policy at boot through `podman-restart.service`, which devsandbox never
/// installs; Apple `container` has no restart policy, so the sandbox falls
/// back to the devsandbox-driven pass (`autostart = true`).
pub(crate) fn restart_decision(
    mode: Option<Autostart>,
    supported: bool,
    backend: &str,
) -> (Option<&'static str>, Vec<String>) {
    if mode != Some(Autostart::Runtime) {
        return (None, Vec::new());
    }
    if !supported {
        let runtime = runtime_label(backend);
        return (
            None,
            vec![format!(
                "autostart = \"runtime\" is unsupported on {runtime}; behaving as `true`"
            )],
        );
    }
    let mut warnings = Vec::new();
    if backend == "podman" {
        warnings.push(
            "autostart = \"runtime\": podman restarts containers at boot only with \
             `podman-restart.service` enabled (devsandbox never installs it)"
                .to_string(),
        );
    }
    (Some(RUNTIME_RESTART), warnings)
}

/// Backend name as users know it in messages.
fn runtime_label(backend: &str) -> &str {
    if backend == "container" { "Apple container" } else { backend }
}

/// Whether the runtime itself restarts the container (`"runtime"` on a backend
/// with restart policies). Then the in-container boot hook, not the host, owns
/// `postStartCommand` on `start` (docs/automations.md); elsewhere `"runtime"`
/// behaves as `true` (see [`restart_decision`]).
pub(crate) fn runtime_restarts(mode: Option<Autostart>, supported: bool) -> bool {
    mode == Some(Autostart::Runtime) && supported
}

/// The policy to switch an existing container to so it matches `mode`, or
/// `None` when `current` already does. `autostart` isn't hashed for drift, so
/// a flip is applied in place on start instead of requiring a rebuild. An
/// unset policy (empty) counts as `no`; an unknown current policy is updated.
fn restart_update(mode: Option<Autostart>, current: Option<&str>) -> Option<&'static str> {
    let desired = if mode == Some(Autostart::Runtime) { RUNTIME_RESTART } else { "no" };
    let current = current.map(|c| if c.is_empty() { "no" } else { c });
    (current != Some(desired)).then_some(desired)
}

/// Bring `container`'s restart policy in line with `mode` (no-op on backends
/// without restart policies). Never fails the caller: an old podman without
/// `update --restart` gets a warning pointing at a recreate.
pub(crate) fn apply_restart_policy(container: &str, instance: &str, mode: Option<Autostart>) {
    if !backend().supports_restart_policy() {
        return;
    }
    let current = backend().restart_policy(container).ok().flatten();
    let Some(policy) = restart_update(mode, current.as_deref()) else { return };
    if let Err(e) = backend().set_restart_policy(container, policy) {
        eprintln!(
            "warning: cannot change restart policy of {container}: {e:#}; \
             recreate with `devsandbox rebuild --force {instance}`"
        );
    }
}

/// The host's current boot id, or `None` when it can't be read (or the OS has
/// no supported source).
fn boot_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        parse_linux_boot_id(&text)
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "kern.boottime"])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_macos_boottime(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_linux_boot_id(text: &str) -> Option<String> {
    let id = text.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// `sysctl -n kern.boottime` prints `{ sec = 1695000000, usec = 123456 } Mon
/// Sep 18 …`; the braced part is the stable id (the date suffix is
/// locale/timezone-dependent).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_macos_boottime(text: &str) -> Option<String> {
    let text = text.trim_start();
    if !text.starts_with('{') {
        return None;
    }
    let end = text.find('}')?;
    Some(text[..=end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(key: &'a str, sandbox: &'a str, project: &'a str) -> Row<'a> {
        Row { key, sandbox, project }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn due_once_per_boot() {
        assert!(due(None, "b1"));
        assert!(due(Some("b0"), "b1"));
        assert!(!due(Some("b1"), "b1"));
    }

    #[test]
    fn stopped_started_running_skipped_missing_noted() {
        let rows = [row("web", "web", "p"), row("web-2", "web", "p"), row("web-3", "web", "p")];
        let got = actions("p", &names(&["web"]), &rows, |key| match key {
            "web" => Liveness::Running,
            "web-2" => Liveness::Stopped,
            _ => Liveness::Missing,
        });
        assert_eq!(got, [Action::Start("web-2".into()), Action::SkipMissing("web-3".into())]);
    }

    #[test]
    fn no_instance_runs_one() {
        let got = actions("p", &names(&["triage"]), &[], |_| unreachable!());
        assert_eq!(got, [Action::Run("triage".into())]);
    }

    #[test]
    fn other_project_ignored_and_does_not_suppress_run() {
        let rows = [row("web", "web", "other")];
        let got = actions("p", &names(&["web"]), &rows, |_| panic!("not queried"));
        assert_eq!(got, [Action::Run("web".into())]);
    }

    #[test]
    fn non_autostart_sandboxes_ignored() {
        let rows = [row("api", "api", "p")];
        let got = actions("p", &names(&["web"]), &rows, |_| panic!("not queried"));
        assert_eq!(got, [Action::Run("web".into())]);
    }

    #[test]
    fn unknown_liveness_neither_starts_nor_runs() {
        let rows = [row("web", "web", "p")];
        assert!(actions("p", &names(&["web"]), &rows, |_| Liveness::Unknown).is_empty());
    }

    #[test]
    fn parses_linux_boot_id() {
        assert_eq!(parse_linux_boot_id("3f2c-9a\n").as_deref(), Some("3f2c-9a"));
        assert_eq!(parse_linux_boot_id(" \n"), None);
    }

    #[test]
    fn parses_macos_boottime() {
        let out = "{ sec = 1695000000, usec = 123456 } Mon Sep 18 10:00:00 2023\n";
        assert_eq!(
            parse_macos_boottime(out).as_deref(),
            Some("{ sec = 1695000000, usec = 123456 }")
        );
        assert_eq!(parse_macos_boottime("garbage"), None);
        assert_eq!(parse_macos_boottime("{ sec = 1"), None);
    }

    #[test]
    fn restart_flag_on_docker_for_runtime_only() {
        assert_eq!(
            restart_decision(Some(Autostart::Runtime), true, "docker"),
            (Some("unless-stopped"), Vec::new())
        );
        for mode in [None, Some(Autostart::Off), Some(Autostart::Devsandbox)] {
            for (supported, name) in [(true, "docker"), (true, "podman"), (false, "container")] {
                assert_eq!(restart_decision(mode, supported, name), (None, Vec::new()));
            }
        }
    }

    #[test]
    fn restart_flag_on_podman_warns_about_service() {
        let (policy, warnings) = restart_decision(Some(Autostart::Runtime), true, "podman");
        assert_eq!(policy, Some("unless-stopped"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("podman-restart.service"), "{warnings:?}");
    }

    #[test]
    fn restart_unsupported_on_apple_falls_back() {
        assert_eq!(
            restart_decision(Some(Autostart::Runtime), false, "container"),
            (
                None,
                vec![
                    "autostart = \"runtime\" is unsupported on Apple container; behaving as `true`"
                        .to_string()
                ]
            )
        );
    }

    #[test]
    fn runtime_restarts_needs_runtime_mode_and_support() {
        assert!(runtime_restarts(Some(Autostart::Runtime), true));
        assert!(!runtime_restarts(Some(Autostart::Runtime), false));
        for mode in [None, Some(Autostart::Off), Some(Autostart::Devsandbox)] {
            assert!(!runtime_restarts(mode, true));
        }
    }

    #[test]
    fn restart_update_only_on_mismatch() {
        let rt = Some(Autostart::Runtime);
        assert_eq!(restart_update(rt, Some("unless-stopped")), None);
        assert_eq!(restart_update(rt, Some("no")), Some("unless-stopped"));
        assert_eq!(restart_update(rt, Some("")), Some("unless-stopped"));
        assert_eq!(restart_update(rt, Some("always")), Some("unless-stopped"));
        assert_eq!(restart_update(rt, None), Some("unless-stopped"));
        for mode in [None, Some(Autostart::Off), Some(Autostart::Devsandbox)] {
            assert_eq!(restart_update(mode, Some("no")), None);
            assert_eq!(restart_update(mode, Some("")), None);
            assert_eq!(restart_update(mode, Some("unless-stopped")), Some("no"));
            assert_eq!(restart_update(mode, None), Some("no"));
        }
    }
}
