use std::path::Path;

use anyhow::Result;

use crate::state::State;

/// Why a rename changed nothing (I/O errors aside), typed so the daemon's
/// `instances.rename` can answer `not-found` vs `invalid`; the message is
/// the CLI's error either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// No instance by that exact name.
    Unknown(String),
    /// A bad or taken new name, or the instance is mid-`run`/`rebuild`.
    Invalid(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Unknown(m) | Refused::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Refused {}

/// Rename an instance: move its state entry to a new key and persist. Unlike
/// other verbs, `name` must be the exact instance key — a rename changes
/// identity, so fuzzy sandbox/folder resolution (and its interactive ambiguity
/// prompt) is deliberately not used here.
pub fn rename(name: &str, new_name: &str) -> Result<()> {
    let new_name = rename_exact(name, new_name)?;
    println!("renamed {name} -> {new_name}");
    Ok(())
}

/// Silent core shared with the daemon's `instances.rename` (via
/// [`rename_exact_at`]) — so it must never touch stdout/stderr or prompt on
/// stdin. Purely
/// cosmetic: only the state key moves. `instance_id` — and with it the
/// container name, `${instance}`-anchored mounts, worktree, shell history, and
/// isolated services, on this run *and* every later `rebuild` — stays what it
/// was at creation; all ops go through the stored `container` field. Returns
/// the trimmed new name.
pub fn rename_exact(name: &str, new_name: &str) -> Result<String> {
    rename_exact_at(&State::path()?, name, new_name)
}

/// [`rename_exact`] on the state file at `state`, for the daemon (its tests
/// use a scratch file). A refusal is a [`Refused`] (downcast it).
pub fn rename_exact_at(state: &Path, name: &str, new_name: &str) -> Result<String> {
    State::update_at(state, |state| {
        let Some(info) = state.instances.get(name) else {
            return Err(Refused::Unknown(format!("no instance named `{name}` (see `devsandbox ps -a`)")).into());
        };
        // A `run` mid-flight saves its full record under the original key,
        // which would bring the old name back next to the new one.
        if let Some(why) = crate::commands::in_flight(name, info.progress.as_ref(), info.phase_view().as_ref()) {
            return Err(Refused::Invalid(why).into());
        }
        Ok(rename_in_state(state, name, new_name)?)
    })
}

/// Move `key`'s entry to `new_name` in `state`, returning the trimmed name.
/// Split out so validation (empty / whitespace / collision) is testable without
/// touching disk.
fn rename_in_state(state: &mut State, key: &str, new_name: &str) -> Result<String, Refused> {
    let new_name = new_name.trim();
    let invalid = |m: String| Err(Refused::Invalid(m));
    if new_name.is_empty() {
        return invalid("new instance name is empty".into());
    }
    if new_name.chars().any(char::is_whitespace) {
        return invalid(format!("new instance name `{new_name}` must not contain spaces"));
    }
    if new_name == key {
        return invalid(format!("instance is already named `{new_name}`"));
    }
    if state.instances.contains_key(new_name) {
        return invalid(format!("an instance named `{new_name}` already exists"));
    }
    let info = state.instances.remove(key).expect("key came from state");
    state.instances.insert(new_name.to_string(), info);
    Ok(new_name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Instance;

    fn insert(state: &mut State, name: &str) {
        state.instances.insert(
            name.into(),
            Instance {
                sandbox: "web".into(),
                instance_id: name.into(),
                project: "abc12345".into(),
                container: format!("devsandbox-{name}"),
                folder: "/home/u/web".into(),
                base_folder: "/home/u/web".into(),
                worktree: None,
                branch: None,
                branch_created: true,
                folders: Vec::new(),
                shell_history: None,
                workspace: "/workspaces/web".into(),
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
                done: None,
                progress: None,
                created_unix: 0,
            },
        );
    }

    #[test]
    fn moves_entry_and_keeps_container() {
        let mut state = State::default();
        insert(&mut state, "old");
        assert_eq!(rename_in_state(&mut state, "old", " new ").unwrap(), "new");
        assert!(!state.instances.contains_key("old"));
        // Container name and persistent id are deliberately preserved: every
        // runtime/host name derives from the id, so a rename moves nothing.
        assert_eq!(state.instances["new"].container, "devsandbox-old");
        assert_eq!(state.instances["new"].instance_id, "old");
    }

    #[test]
    fn rejects_empty_whitespace_and_collision() {
        let mut state = State::default();
        insert(&mut state, "old");
        insert(&mut state, "taken");
        assert!(rename_in_state(&mut state, "old", "   ").is_err());
        assert!(rename_in_state(&mut state, "old", "a b").is_err());
        assert!(rename_in_state(&mut state, "old", "old").is_err());
        assert!(rename_in_state(&mut state, "old", "taken").is_err());
        // A failed rename leaves the entry in place.
        assert!(state.instances.contains_key("old"));
    }
}
