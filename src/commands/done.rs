use anyhow::{Context, Result};

use super::resolve_instance;
use crate::state::{Instance, State};

/// `devsandbox done|undone <instance>`: set or clear the instance's done flag
/// (docs/inbox-threads.md, "Done instances"). Nothing else changes: the
/// container, worktree and runs stay until the user removes them.
pub fn done(name: &str, done: bool) -> Result<()> {
    let state = State::load()?;
    let key = resolve_instance(&state, name)?;
    println!("{}", set_saved(&key, done)?);
    Ok(())
}

/// Set or clear `key`'s flag in the saved state and return a one-line
/// status; prints nothing, so the TUI (which owns the terminal) shares it
/// with the CLI. `key` is an exact state key. Reloads state right before
/// the write to keep the window for racing another writer small.
pub fn set_saved(key: &str, done: bool) -> Result<String> {
    let mut state = State::load()?;
    let (changed, msg) = set(&mut state, key, done, Instance::now())?;
    if changed {
        state.save()?;
    }
    Ok(msg)
}

/// The pure core: set (`since` = `now`) or clear `key`'s flag in `state`.
/// Idempotent: marking a done instance done keeps its original since.
/// Returns whether `state` changed, and the status line.
fn set(state: &mut State, key: &str, done: bool, now: u64) -> Result<(bool, String)> {
    let info = state.instances.get_mut(key).with_context(|| format!("no instance `{key}`"))?;
    Ok(match (done, info.done.is_some()) {
        (true, true) => (false, format!("`{key}` is already done")),
        (false, false) => (false, format!("`{key}` is not done")),
        (true, false) => {
            info.done = Some(now);
            (true, format!("marked {key} done"))
        }
        (false, true) => {
            info.done = None;
            (true, format!("marked {key} not done"))
        }
    })
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
