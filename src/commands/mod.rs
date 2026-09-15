pub mod exec;
pub mod ls;
pub mod ps;
pub mod rm;
pub mod run;
pub mod services;
pub mod stop;

use std::io::{IsTerminal, Write};

use anyhow::{bail, Context, Result};

use crate::state::State;

/// Resolve a user-supplied `name` to a single instance key. `name` may be an
/// instance name, a sandbox config name, or a repository (folder basename);
/// every instance it could refer to is collected. Zero matches or (on a
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

    use super::{resolve_instance, resolve_instance_with};

    /// State with two instances of the same sandbox, in two folders.
    fn state() -> State {
        let mut state = State::default();
        let mk = |sandbox: &str, folder: &str| Instance {
            sandbox: sandbox.to_string(),
            project: "proj".into(),
            container: "devsandbox-x".into(),
            folder: folder.into(),
            base_folder: folder.into(),
            worktree: None,
            shell_history: None,
            workspace: "/w".into(),
            workspace_file: None,
            remote_env: BTreeMap::new(),
            remote_user: None,
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
                project: "proj".into(),
                container: "devsandbox-x".into(),
                folder: "/home/u/api".into(),
                base_folder: "/home/u/api".into(),
                worktree: None,
                shell_history: None,
                workspace: "/w".into(),
                workspace_file: None,
                remote_env: BTreeMap::new(),
                remote_user: None,
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
