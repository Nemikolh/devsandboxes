use anyhow::{bail, Result};

use crate::state::State;

/// Rename an instance: move its state entry to a new key and persist. Unlike
/// other verbs, `name` must be the exact instance key — a rename changes
/// identity, so fuzzy sandbox/folder resolution (and its interactive ambiguity
/// prompt) is deliberately not used here.
pub fn rename(name: &str, new_name: &str) -> Result<()> {
    let new_name = rename_exact(name, new_name)?;
    println!("renamed {name} -> {new_name}");
    Ok(())
}

/// Silent core shared with the TUI, which runs it in place on the alternate
/// screen — so it must never touch stdout/stderr or prompt on stdin. Purely
/// cosmetic: only the state key moves. `instance_id` — and with it the
/// container name, `${instance}`-anchored mounts, worktree, shell history, and
/// isolated services, on this run *and* every later `rebuild` — stays what it
/// was at creation; all ops go through the stored `container` field. Returns
/// the trimmed new name.
pub fn rename_exact(name: &str, new_name: &str) -> Result<String> {
    let mut state = State::load()?;
    if !state.instances.contains_key(name) {
        bail!("no instance named `{name}` (see `devsandbox ps -a`)");
    }
    let new_name = rename_in_state(&mut state, name, new_name)?;
    state.save()?;
    Ok(new_name)
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
                instance_id: name.into(),
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
                ssh_auth_sock: None,
                devsbd_arch: None,
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
