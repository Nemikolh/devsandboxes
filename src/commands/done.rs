use std::path::Path;

use anyhow::Result;

use super::resolve_instance;
use crate::state::{Instance, State};

/// No instance by that exact key: typed so the daemon's `instances.done`
/// answers `not-found` (downcast it); the message is the CLI's error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownInstance(pub String);

impl std::fmt::Display for UnknownInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no instance `{}`", self.0)
    }
}

impl std::error::Error for UnknownInstance {}

/// `devsandbox done|undone <instance>`: set or clear the instance's done flag
/// (docs/architecture/instances/lifecycle.md, "rename and done"). Nothing
/// else changes: the container, worktree and runs stay until the user
/// removes them.
pub fn done(name: &str, done: bool) -> Result<()> {
    let state = State::load()?;
    let key = resolve_instance(&state, name)?;
    println!("{}", set_saved(&key, done)?);
    Ok(())
}

/// Set or clear `key`'s flag in the saved state and return a one-line
/// status; prints nothing, so the daemon's `instances.done` shares it with
/// the CLI. `key` is an exact state key.
pub fn set_saved(key: &str, done: bool) -> Result<String> {
    set_saved_at(&State::path()?, key, done)
}

/// [`set_saved`] on the state file at `state`, for the daemon (its tests use
/// a scratch file). An unknown `key` is an [`UnknownInstance`].
pub fn set_saved_at(state: &Path, key: &str, done: bool) -> Result<String> {
    State::update_at(state, |state| Ok(set(state, key, done, Instance::now()).map(|(_, msg)| msg)?))
}

/// The pure core: set (`since` = `now`) or clear `key`'s flag in `state`.
/// Idempotent: marking a done instance done keeps its original since.
/// Returns whether `state` changed, and the status line.
fn set(state: &mut State, key: &str, done: bool, now: u64) -> Result<(bool, String), UnknownInstance> {
    let info = state.instances.get_mut(key).ok_or_else(|| UnknownInstance(key.to_string()))?;
    let line = status_line(key, done, info.done.is_some());
    let changed = done != info.done.is_some();
    if changed {
        info.done = done.then_some(now);
    }
    Ok((changed, line))
}

/// The status line of setting (`done`) or clearing `key`'s flag when it
/// `was` set, shared with the re-run on a machine
/// (`machines::cli::calls`, which reads `was` from the machine's rows).
pub(crate) fn status_line(key: &str, done: bool, was: bool) -> String {
    match (done, was) {
        (true, true) => format!("`{key}` is already done"),
        (false, false) => format!("`{key}` is not done"),
        (true, false) => format!("marked {key} done"),
        (false, true) => format!("marked {key} not done"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap()
    }

    #[test]
    fn set_and_clear_are_idempotent() {
        let mut s = state();
        assert_eq!(set(&mut s, "a", true, 10).unwrap(), (true, "marked a done".into()));
        assert_eq!(s.instances["a"].done, Some(10));
        // Again: unchanged, the original since kept.
        assert_eq!(set(&mut s, "a", true, 20).unwrap(), (false, "`a` is already done".into()));
        assert_eq!(s.instances["a"].done, Some(10));
        assert_eq!(set(&mut s, "a", false, 30).unwrap(), (true, "marked a not done".into()));
        assert_eq!(s.instances["a"].done, None);
        assert_eq!(set(&mut s, "a", false, 40).unwrap(), (false, "`a` is not done".into()));
    }

    #[test]
    fn unknown_instance_is_an_error() {
        let mut s = state();
        assert!(set(&mut s, "zz", true, 1).unwrap_err().to_string().contains("no instance `zz`"));
    }
}
