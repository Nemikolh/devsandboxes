pub mod exec;
pub mod inspect;
pub mod logs;
pub mod ls;
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
pub mod vscode;

use std::io::{IsTerminal, Write};

use anyhow::{bail, Context, Result};

use crate::runtime::{backend, NAME_PREFIX};
use crate::state::State;

/// Resolve a user-supplied `name` to a single instance key. `name` may be an
/// instance name, its persistent id (they diverge after a rename — the id is
/// what container/mount names show), a sandbox config name, or a repository
/// (folder basename); every instance it could refer to is collected. Zero matches or (on a
/// non-TTY) an ambiguous match bail; a TTY prompts interactively. Shared by
/// `rm` and `stop`.
pub(crate) fn resolve_instance(state: &State, name: &str) -> Result<String> {
    resolve_instance_with(state, name, std::io::stdin().is_terminal())
}

fn resolve_instance_with(state: &State, name: &str, interactive: bool) -> Result<String> {
    let mut matches: Vec<String> = state
        .instances
        .iter()
        .filter(|(instance, info)| {
            *instance == name
                || info.instance_id == name
                || info.sandbox == name
                || info.folder.file_name().is_some_and(|f| f == name)
        })
        .map(|(instance, _)| instance.clone())
        .collect();
    matches.sort();

    match matches.len() {
        0 => bail!("no sandbox instance matches `{name}` (see `devsandbox ps -a`)"),
        1 => Ok(matches.remove(0)),
        _ => {
            if !interactive {
                bail!("`{name}` is ambiguous: {}", matches.join(", "));
            }
            let refs: Vec<&str> = matches.iter().map(String::as_str).collect();
            Ok(matches.remove(pick(&format!("`{name}` matches multiple instances"), &refs)?))
        }
    }
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

/// The single drift rule shared by every comparison site (`rebuild`, `run`
/// reuse, `services`, and the TUI snapshot) so they can't diverge. A container
/// has drifted when either recorded label is present *and* differs from what
/// the current config produces:
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

    use super::{drift_decision, resolve_instance, resolve_instance_with};

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
            shell_history: None,
            workspace: "/w".into(),
            workspace_file: None,
            remote_env: BTreeMap::new(),
            remote_user: None,
            ssh_auth_sock: None,
            devsbd_arch: None,
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
                shell_history: None,
                workspace: "/w".into(),
                workspace_file: None,
                remote_env: BTreeMap::new(),
                remote_user: None,
                ssh_auth_sock: None,
                devsbd_arch: None,
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
}
