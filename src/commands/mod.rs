#[cfg(unix)]
pub mod agent_relay;
pub mod autostart;
pub mod dispatch;
pub mod done;
pub mod exec;
pub mod inbox;
pub mod inspect;
pub mod logs;
pub mod ls;
#[cfg(unix)]
pub mod machine;
pub mod port;
pub mod ps;
pub mod rebuild;
pub mod rename;
pub mod rm;
pub mod run;
pub mod services;
pub mod start;
pub mod stats;
pub mod status;
pub mod stop;
#[cfg(unix)]
pub mod tunnel;
pub mod vscode;

use std::io::{IsTerminal, Write};

use anyhow::{bail, Context, Result};

use crate::runtime::{backend, NAME_PREFIX};
use crate::state::{Instance, PhaseView, Progress, State};

/// Why `key` isn't a ready instance (docs/architecture/instances/lifecycle.md),
/// for verbs that need one (`start`; `rebuild --all` and autostart skip with
/// it): still being materialized, or failed (an interrupted one included).
/// `None` = ready. `view` is [`Instance::phase_view`].
pub(crate) fn not_ready(key: &str, view: Option<&PhaseView>) -> Option<String> {
    match view? {
        PhaseView::InFlight(label) => Some(format!("`{key}` is still {}", doing(label))),
        PhaseView::Failed { at, error } => Some(format!(
            "`{key}` failed at {at}: {error}; `devsandbox rebuild {key}` retries, \
             `devsandbox rm {key}` removes it"
        )),
    }
}

/// Why `exec` (and the dashboard's terminal) can't reach `key`'s container:
/// it doesn't exist yet, or a failure came before it did. A failed or
/// still-running lifecycle step passes: shelling in is how a failed
/// `postCreateCommand` gets debugged, or a slow one watched. `None` = go.
pub(crate) fn no_container_yet(key: &str, view: Option<&PhaseView>) -> Option<String> {
    let pre = |label: &str| matches!(label, "preparing" | "building" | "creating");
    match view? {
        PhaseView::InFlight(label) if pre(label) => Some(format!("`{key}` is still {label}: no container yet")),
        PhaseView::Failed { at, .. } if pre(at) => not_ready(key, view),
        _ => None,
    }
}

/// Why `key` can't be touched at all right now: a live process is
/// materializing it (`rm`, even forced, `rename`, `rebuild`). Failed and
/// interrupted ones pass. `None` = not in flight.
pub(crate) fn in_flight(key: &str, progress: Option<&Progress>, view: Option<&PhaseView>) -> Option<String> {
    match (progress, view) {
        (Some(p), Some(PhaseView::InFlight(label))) => Some(format!(
            "`{key}` is still {} (pid {}): wait for it, or Ctrl-C it",
            doing(label),
            p.pid
        )),
        _ => None,
    }
}

/// A phase label as "what it's doing": the named phases read as verbs
/// (`building`), a lifecycle step is a noun (`running postCreateCommand`).
pub(crate) fn doing(label: &str) -> String {
    match label {
        "preparing" | "building" | "creating" => label.to_string(),
        step => format!("running {step}"),
    }
}

/// Resolve a user-supplied `name` to a single instance key. `name` may be an
/// instance name, its persistent id (they diverge after a rename — the id is
/// what container/mount names show), a sandbox config name, or a repository
/// (folder basename). An exact instance name or id wins outright: with
/// instances `web` and `web-2` of sandbox `web`, `web` must mean the instance,
/// not an ambiguity that fails off a TTY and prompts on one (a programmatic
/// caller's pty is a TTY). Only when nothing matches exactly is every instance
/// the sandbox or folder name could refer to collected. Zero matches or (on a
/// non-TTY) an ambiguous match bail; a TTY prompts interactively. Shared by
/// every verb taking an instance `<name>` (`exec`, `rm`, `stop`, …).
pub(crate) fn resolve_instance(state: &State, name: &str) -> Result<String> {
    resolve_instance_with(state, name, std::io::stdin().is_terminal())
}

/// Non-interactive [`resolve_instance`]: an ambiguous `name` bails instead of
/// prompting. For callers where prompting is impossible or wrong even though
/// stdin is a TTY — the daemon's forward registry (`serve::forwards`) owns no
/// console, and a prompt there would block a `forwards.add` forever — and
/// `devsandbox inbox`, whose owner falls back to a thread's last-known name
/// rather than asking.
pub(crate) fn resolve_instance_noninteractive(state: &State, name: &str) -> Result<String> {
    resolve_instance_with(state, name, false)
}

fn resolve_instance_with(state: &State, name: &str, interactive: bool) -> Result<String> {
    let mut matches = instance_matches(state, name);
    match matches.len() {
        0 => bail!("{}", no_instance_matches(name)),
        1 => Ok(matches.remove(0)),
        _ => {
            if !interactive {
                bail!("{}", ambiguous_instance(name, &matches));
            }
            let refs: Vec<&str> = matches.iter().map(String::as_str).collect();
            Ok(matches.remove(pick(&ambiguous_prompt(name), &refs)?))
        }
    }
}

/// The no-match error, shared with the daemon's `instances.resolve` so a
/// client prints what the in-process CLI would.
pub(crate) fn no_instance_matches(name: &str) -> String {
    format!("no sandbox instance matches `{name}` (see `devsandbox ps -a`)")
}

/// The off-a-TTY ambiguity error, shared with the verb client.
pub(crate) fn ambiguous_instance(name: &str, matches: &[String]) -> String {
    format!("`{name}` is ambiguous: {}", matches.join(", "))
}

/// The TTY prompt among several matches, shared with the verb client.
pub(crate) fn ambiguous_prompt(name: &str) -> String {
    format!("`{name}` matches multiple instances")
}

/// Every instance key `name` could mean, sorted, in [`resolve_instance`]'s
/// tiers: the exact ones if any, else the sandbox / folder ones. Shared with
/// the daemon's `instances.resolve`, so a client picking among them (on its
/// own TTY) sees what the CLI would have offered.
pub(crate) fn instance_matches(state: &State, name: &str) -> Vec<String> {
    let keys = |pred: &dyn Fn(&str, &Instance) -> bool| -> Vec<String> {
        let mut keys: Vec<String> = state
            .instances
            .iter()
            .filter(|(instance, info)| pred(instance, info))
            .map(|(instance, _)| instance.clone())
            .collect();
        keys.sort();
        keys
    };
    // The exact tier can still hold two entries (a renamed instance's id equal
    // to another's name); that stays ambiguous rather than guessing.
    let matches = keys(&|instance, info| instance == name || info.instance_id == name);
    if !matches.is_empty() {
        return matches;
    }
    keys(&|_, info| info.sandbox == name || info.folder.file_name().is_some_and(|f| f == name))
}

/// Resolve `name` to a container: an instance (same rules as
/// [`resolve_instance`]) or, when no instance matches, an exact devsandbox
/// container name with or without the `devsandbox-` prefix — so service
/// containers are reachable too. Shared by `logs` and `inspect`.
pub(crate) fn resolve_container(state: &State, name: &str) -> Result<String> {
    let instance_err = match resolve_instance(state, name) {
        Ok(key) => {
            let info = state.instances.get(&key).expect("key came from state");
            return Ok(info.container.clone());
        }
        Err(e) => e,
    };
    let prefixed = format!("{NAME_PREFIX}{name}");
    backend()
        .list(true, NAME_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .map(|row| row.name)
        .find(|n| n == name || n == &prefixed)
        .ok_or(instance_err)
}

/// The single drift rule shared by every comparison site (`rebuild`, `start`'s
/// `warn_on_drift`, `services`, and the instance snapshot) so they can't
/// diverge. A container has drifted when either recorded label is present
/// *and* differs from what the current config produces:
///
/// - config: label `Some(h)` and `h != expected_config`.
/// - build:  label `Some(h)` and `expected_build` non-empty and `h != expected_build`.
///
/// A missing label (pre-upgrade container, or an image-based sandbox that
/// carries an empty build hash) or an empty expected build hash contributes no
/// drift — the lenient rule that keeps old containers from flapping.
///
/// `rebuild`'s "config label gone → rebuild anyway" recovery is *not* folded in
/// here; it stays local to `needs_rebuild`.
pub(crate) fn container_drifted(
    container: &str,
    expected_config: &str,
    expected_build: &str,
) -> Result<bool> {
    let rt = backend();
    let config_label = rt.label(container, "devsandbox.config_hash")?;
    let build_label = rt.label(container, "devsandbox.build_hash")?;
    Ok(drift_decision(
        config_label.as_deref(),
        expected_config,
        build_label.as_deref(),
        expected_build,
    ))
}

/// The pure drift decision behind [`container_drifted`], split out so it can be
/// table-tested without a runtime. See [`container_drifted`] for the rule.
///
/// Shared with the snapshot's service join (`snapshot::build_service_rows`),
/// which applies the same rule against the labels already carried by the `ps`
/// listing rather than paying a per-container `inspect`.
pub(crate) fn drift_decision(
    config_label: Option<&str>,
    expected_config: &str,
    build_label: Option<&str>,
    expected_build: &str,
) -> bool {
    let config_drift = config_label.is_some_and(|h| h != expected_config);
    let build_drift =
        !expected_build.is_empty() && build_label.is_some_and(|h| h != expected_build);
    config_drift || build_drift
}

/// Yes/no prompt on stderr; `false` on a non-TTY. Shared by `rm` and `gc`.
pub(crate) fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Numbered menu on stderr; returns the selected index.
pub(crate) fn pick(prompt: &str, options: &[&str]) -> Result<usize> {
    eprintln!("{prompt}:");
    for (i, option) in options.iter().enumerate() {
        eprintln!("  {}) {option}", i + 1);
    }
    eprint!("> ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let choice: usize = answer.trim().parse().context("invalid selection")?;
    if choice == 0 || choice > options.len() {
        bail!("selection out of range");
    }
    Ok(choice - 1)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::state::{Instance, State};

    use super::{drift_decision, in_flight, no_container_yet, not_ready, instance_matches, resolve_instance, resolve_instance_with};
    use crate::state::{Phase, PhaseView, Progress};

    #[test]
    fn not_ready_says_what_the_instance_is_doing_or_how_it_failed() {
        assert_eq!(not_ready("web", None), None);
        let building = PhaseView::InFlight("building".into());
        assert_eq!(not_ready("web", Some(&building)).unwrap(), "`web` is still building");
        let step = PhaseView::InFlight("postCreateCommand".into());
        assert_eq!(not_ready("web", Some(&step)).unwrap(), "`web` is still running postCreateCommand");
        let failed = PhaseView::Failed { at: "creating".into(), error: "interrupted".into() };
        let why = not_ready("web", Some(&failed)).unwrap();
        assert!(why.starts_with("`web` failed at creating: interrupted;"), "{why}");
        assert!(why.contains("`devsandbox rebuild web` retries"), "{why}");
        assert!(why.contains("`devsandbox rm web` removes it"), "{why}");
    }

    #[test]
    fn exec_waits_only_for_the_container() {
        assert_eq!(no_container_yet("web", None), None);
        let building = PhaseView::InFlight("building".into());
        assert_eq!(no_container_yet("web", Some(&building)).unwrap(), "`web` is still building: no container yet");
        let pre = PhaseView::Failed { at: "creating".into(), error: "boom".into() };
        assert!(no_container_yet("web", Some(&pre)).unwrap().starts_with("`web` failed at creating"));
        // A lifecycle step, running or failed: the container is there to debug.
        let step = PhaseView::InFlight("postCreateCommand".into());
        assert_eq!(no_container_yet("web", Some(&step)), None);
        let failed = PhaseView::Failed { at: "postCreateCommand".into(), error: "exit 1".into() };
        assert_eq!(no_container_yet("web", Some(&failed)), None);
    }

    #[test]
    fn only_a_live_in_flight_instance_is_untouchable() {
        let p = Progress { phase: Phase::Building, pid: 4242, boot: None, error: None };
        let live = PhaseView::InFlight("building".into());
        assert_eq!(
            in_flight("web", Some(&p), Some(&live)).unwrap(),
            "`web` is still building (pid 4242): wait for it, or Ctrl-C it"
        );
        // Interrupted (dead pid) or failed: removable, renamable, rebuildable.
        let dead = PhaseView::Failed { at: "building".into(), error: "interrupted".into() };
        assert_eq!(in_flight("web", Some(&p), Some(&dead)), None);
        assert_eq!(in_flight("web", None, None), None);
    }

    #[test]
    fn drift_rule_covers_label_presence_and_expected_emptiness() {
        // (config_label, expected_config, build_label, expected_build) → drift.
        // "cfg" matches expected config; "b" matches expected build.
        let cases: &[(Option<&str>, &str, Option<&str>, &str, bool)] = &[
            // No labels at all → no drift regardless of expected.
            (None, "cfg", None, "b", false),
            (None, "cfg", None, "", false),
            // Config label present and matching, build absent → no drift.
            (Some("cfg"), "cfg", None, "b", false),
            // Config label present and differing → drift (even with empty build).
            (Some("old"), "cfg", None, "", true),
            // Build label present, non-empty expected, differing → drift.
            (Some("cfg"), "cfg", Some("old"), "b", true),
            // Build label present but expected build empty → build contributes nothing.
            (Some("cfg"), "cfg", Some("anything"), "", false),
            // Build label present and matching → no drift.
            (Some("cfg"), "cfg", Some("b"), "b", false),
            // Build label absent, non-empty expected → no drift (missing label lenient).
            (Some("cfg"), "cfg", None, "b", false),
        ];
        for &(cfg_label, exp_cfg, build_label, exp_build, want) in cases {
            assert_eq!(
                drift_decision(cfg_label, exp_cfg, build_label, exp_build),
                want,
                "cfg_label={cfg_label:?} exp_cfg={exp_cfg} build_label={build_label:?} exp_build={exp_build}"
            );
        }
    }

    /// State with two instances of the same sandbox, in two folders.
    fn state() -> State {
        let mut state = State::default();
        let mk = |sandbox: &str, folder: &str| Instance {
            sandbox: sandbox.to_string(),
            instance_id: String::new(),
            project: "proj".into(),
            container: "devsandbox-x".into(),
            folder: folder.into(),
            base_folder: folder.into(),
            worktree: None,
            branch: None,
            branch_created: true,
            folders: Vec::new(),
            shell_history: None,
            workspace: "/w".into(),
            workspace_file: None,
            remote_env: BTreeMap::new(),
            remote_user: None,
            ssh_auth_sock: None,
            devsbd_arch: None,
            volumes: Vec::new(),
            dispatcher: None,
            config_dir: None,
            extra_env: Default::default(),
            forwarded_ports: Default::default(),
            done: None,
            progress: None,
            created_unix: 0,
        };
        state.instances.insert("web-aaaa".into(), mk("web", "/home/u/site"));
        state.instances.insert("web-bbbb".into(), mk("web", "/home/u/other"));
        state
    }

    #[test]
    fn matches_exact_instance_name() {
        let s = state();
        assert_eq!(resolve_instance(&s, "web-aaaa").unwrap(), "web-aaaa");
    }

    #[test]
    fn matches_unique_sandbox_name() {
        // A sandbox name owned by exactly one instance resolves to it.
        let mut s = State::default();
        s.instances.insert(
            "api-cccc".into(),
            Instance {
                sandbox: "api".into(),
                instance_id: "api-cccc".into(),
                project: "proj".into(),
                container: "devsandbox-x".into(),
                folder: "/home/u/api".into(),
                base_folder: "/home/u/api".into(),
                worktree: None,
                branch: None,
                branch_created: true,
                folders: Vec::new(),
                shell_history: None,
                workspace: "/w".into(),
                workspace_file: None,
                remote_env: BTreeMap::new(),
                remote_user: None,
                ssh_auth_sock: None,
                devsbd_arch: None,
                volumes: Vec::new(),
                dispatcher: None,
                config_dir: None,
                extra_env: Default::default(),
                forwarded_ports: Default::default(),
                done: None,
                progress: None,
                created_unix: 0,
            },
        );
        assert_eq!(resolve_instance(&s, "api").unwrap(), "api-cccc");
    }

    #[test]
    fn matches_folder_basename() {
        // Unique folder basename resolves to the single owning instance.
        let s = state();
        assert_eq!(resolve_instance(&s, "site").unwrap(), "web-aaaa");
    }

    #[test]
    fn matches_persistent_id_after_rename() {
        // Renamed instance: key `new-name`, id `web-aaaa` — the id (what the
        // container name shows) still resolves to the entry.
        let mut s = State::default();
        let mut info = state().instances.remove("web-aaaa").unwrap();
        info.instance_id = "web-aaaa".into();
        s.instances.insert("new-name".into(), info);
        assert_eq!(resolve_instance(&s, "web-aaaa").unwrap(), "new-name");
    }

    #[test]
    fn no_match_errors() {
        let s = state();
        let err = resolve_instance(&s, "nope").unwrap_err().to_string();
        assert!(err.contains("no sandbox instance matches `nope`"), "{err}");
    }

    #[test]
    fn ambiguous_sandbox_name_errors_without_tty() {
        // Two instances share sandbox `web`; non-interactive resolution must
        // bail rather than prompt (interactivity is injected so the test does
        // not depend on whether cargo test itself has a TTY).
        let s = state();
        let err = resolve_instance_with(&s, "web", false).unwrap_err().to_string();
        assert!(err.contains("is ambiguous"), "{err}");
        assert!(err.contains("web-aaaa"), "{err}");
        assert!(err.contains("web-bbbb"), "{err}");
    }

    /// Instances `web` and `web-2` of sandbox `web`: `web` is the instance,
    /// even interactively (no prompt), and the sandbox tier never runs.
    #[test]
    fn exact_name_beats_sandbox_name() {
        let mut s = state();
        let a = s.instances.remove("web-aaaa").unwrap();
        let b = s.instances.remove("web-bbbb").unwrap();
        s.instances.insert("web".into(), Instance { instance_id: "web".into(), ..a });
        s.instances.insert("web-2".into(), Instance { instance_id: "web-2".into(), ..b });
        assert_eq!(resolve_instance_with(&s, "web", false).unwrap(), "web");
        assert_eq!(resolve_instance_with(&s, "web", true).unwrap(), "web");
        assert_eq!(resolve_instance_with(&s, "web-2", false).unwrap(), "web-2");
    }

    /// A persistent id beats a folder basename it happens to equal.
    #[test]
    fn exact_id_beats_folder_basename() {
        let mut s = state();
        // `web-bbbb` lives in folder `other`; rename `web-aaaa` so its id is `other`.
        let mut a = s.instances.remove("web-aaaa").unwrap();
        a.instance_id = "other".into();
        s.instances.insert("renamed".into(), a);
        assert_eq!(resolve_instance_with(&s, "other", false).unwrap(), "renamed");
    }

    /// An id equal to another instance's name is an exact-tier tie: ambiguous,
    /// not silently one of them.
    #[test]
    fn exact_tier_tie_is_ambiguous() {
        let mut s = state();
        let mut a = s.instances.remove("web-aaaa").unwrap();
        a.instance_id = "web-bbbb".into();
        s.instances.insert("renamed".into(), a);
        let err = resolve_instance_with(&s, "web-bbbb", false).unwrap_err().to_string();
        assert!(err.contains("is ambiguous: renamed, web-bbbb"), "{err}");
    }

    /// The matches `instances.resolve` returns: one tier, sorted.
    #[test]
    fn instance_matches_lists_the_winning_tier_sorted() {
        let s = state();
        assert_eq!(instance_matches(&s, "web"), ["web-aaaa", "web-bbbb"]);
        assert_eq!(instance_matches(&s, "web-bbbb"), ["web-bbbb"]);
        assert!(instance_matches(&s, "nope").is_empty());
    }
}
