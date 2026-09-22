use anyhow::{bail, Result};

use super::resolve_instance;
use crate::state::State;

/// Rename an instance: move its state entry to a new key and persist. `name`
/// resolves like every other verb (instance / sandbox / folder). The container
/// keeps its old `devsandbox-<old>` name — nothing is renamed in the runtime;
/// all ops go through the stored `container` field, and a later `rebuild`
/// re-derives the container from the (new) key. Pure state I/O, so the TUI runs
/// it without suspending.
pub fn rename(name: &str, new_name: &str) -> Result<()> {
    let mut state = State::load()?;
    let key = resolve_instance(&state, name)?;
    let new_name = rename_in_state(&mut state, &key, new_name)?;
    state.save()?;
    println!("renamed {key} -> {new_name}");
    Ok(())
}

/// Move `key`'s entry to `new_name` in `state`, returning the trimmed name.
/// Split out so validation (empty / whitespace / collision) is testable without
/// touching disk.
fn rename_in_state(state: &mut State, key: &str, new_name: &str) -> Result<String> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        bail!("new instance name is empty");
    }
    if new_name.chars().any(char::is_whitespace) {
        bail!("new instance name `{new_name}` must not contain spaces");
    }
    if new_name == key {
        bail!("instance is already named `{new_name}`");
    }
    if state.instances.contains_key(new_name) {
        bail!("an instance named `{new_name}` already exists");
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
                project: "abc12345".into(),
                container: format!("devsandbox-{name}"),
                folder: "/home/u/web".into(),
                base_folder: "/home/u/web".into(),
                worktree: None,
                branch: None,
                shell_history: None,
                workspace: "/workspaces/web".into(),
                workspace_file: None,
                remote_env: Default::default(),
                remote_user: None,
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
        // Container name is deliberately preserved.
        assert_eq!(state.instances["new"].container, "devsandbox-old");
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
