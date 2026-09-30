//! Whether a `folders` entry gets its own detached worktree or the live host
//! dir (docs/folders-worktrees.md). Each host folder has one owner of its
//! direct checkout; everyone else gets a worktree so no two instances write
//! to one working tree. Pure over config + state so it's testable without git.

// Wired in by materialize (step 3).
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;

use anyhow::{bail, Result};

use crate::config::{Config, FolderWorktree};
use crate::state::State;

/// The mount decision for one `folders` entry: `true` = detached worktree.
/// `auto` checks, in order: a non-repo can't have one; a dispatcher child
/// never touches a live checkout; an owned folder stays its owner's; one
/// another instance mounts directly is taken. Otherwise this instance holds it.
pub(super) fn needs_worktree(
    mode: FolderWorktree,
    is_repo_root: bool,
    dispatched: bool,
    owned: bool,
    direct_in_use: bool,
) -> Result<bool> {
    Ok(match mode {
        FolderWorktree::Never => false,
        FolderWorktree::Always if !is_repo_root => {
            bail!("`worktree = \"always\"` needs a git repo root (no `.git` directory)")
        }
        FolderWorktree::Always => true,
        FolderWorktree::Auto => is_repo_root && (dispatched || owned || direct_in_use),
    })
}

/// A repo root has a `.git` directory. A `.git` file (itself a worktree or a
/// submodule) doesn't count: `git worktree add` from it would nest worktrees
/// of some other repo.
pub(super) fn is_repo_root(path: &Path) -> bool {
    path.join(".git").is_dir()
}

/// Whether `path` (canonical) is some sandbox's own project: a sandbox in
/// `config` (rooted at `dir`) whose `folder` resolves to it, or any instance
/// in the global state whose `base_folder` is it. The state check catches a
/// second config root whose sandbox owns the same folder. Sandboxes that fail
/// to resolve, or whose folder doesn't exist, can't own anything and are
/// skipped rather than failing someone else's `run`.
pub(super) fn folder_owned(config: &Config, dir: &Path, state: &State, path: &Path) -> bool {
    let by_config = config.sandboxes.keys().any(|name| {
        let Ok(sandbox) = config.resolve_sandbox(name) else {
            return false;
        };
        sandbox
            .folder()
            .and_then(|f| dir.join(f).canonicalize().ok())
            .is_some_and(|f| f == path)
    });
    by_config
        || state.instances.values().any(|i| {
            i.base_folder.canonicalize().unwrap_or_else(|_| i.base_folder.clone()) == path
        })
}

/// Whether an instance other than `except_instance` (a state key) mounts
/// `path` directly: as its primary checkout (`folder`, which is the worktree
/// for worktree instances), or as a `folders` entry without a worktree.
pub(super) fn folder_directly_mounted(state: &State, path: &Path, except_instance: Option<&str>) -> bool {
    state
        .instances
        .iter()
        .filter(|(key, _)| Some(key.as_str()) != except_instance)
        .any(|(_, i)| {
            i.folder == path || i.folders.iter().any(|f| f.base == path && f.worktree.is_none())
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::tests::instance;
    use super::*;
    use crate::state::FolderMount;

    use FolderWorktree::{Always, Auto, Never};

    #[test]
    fn never_is_always_direct() {
        for flags in 0..16u8 {
            let b = |i: u8| flags & (1 << i) != 0;
            assert!(!needs_worktree(Never, b(0), b(1), b(2), b(3)).unwrap());
        }
    }

    #[test]
    fn always_needs_a_repo_root() {
        assert!(needs_worktree(Always, true, false, false, false).unwrap());
        let err = needs_worktree(Always, false, true, true, true).unwrap_err().to_string();
        assert!(err.contains("git repo root"), "{err}");
    }

    #[test]
    fn auto_follows_the_table() {
        // Not a repo: direct, whatever else holds.
        assert!(!needs_worktree(Auto, false, true, true, true).unwrap());
        // Repo, nobody else on it: this instance holds it.
        assert!(!needs_worktree(Auto, true, false, false, false).unwrap());
        // Any one reason is enough.
        assert!(needs_worktree(Auto, true, true, false, false).unwrap());
        assert!(needs_worktree(Auto, true, false, true, false).unwrap());
        assert!(needs_worktree(Auto, true, false, false, true).unwrap());
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("devsandbox-folders-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn repo_root_needs_a_git_dir() {
        let root = temp_root("root");
        assert!(!is_repo_root(&root));
        // A worktree's or submodule's `.git` file is not a root.
        std::fs::write(root.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert!(!is_repo_root(&root));
        std::fs::remove_file(root.join(".git")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        assert!(is_repo_root(&root));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn owned_by_a_config_sandbox() {
        let root = temp_root("owned");
        let cfg = root.join("cfg");
        for d in ["cfg", "lib", "docs"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let config = Config::parse(
            r#"
[sandbox.lib]
folder = "../lib"
image = "x"

[sandbox.gone]
folder = "../missing"
image = "x"

[sandbox.broken]
folder = "../docs"
image = "x"
extends = "nope"
"#,
        )
        .unwrap();
        let state = State::default();
        // `cfg/../lib` canonicalizes to the entry's canonical path.
        assert!(folder_owned(&config, &cfg, &state, &root.join("lib")));
        // A sandbox that fails to resolve owns nothing (and doesn't fail others).
        assert!(!folder_owned(&config, &cfg, &state, &root.join("docs")));
        assert!(!folder_owned(&config, &cfg, &state, &root.join("missing")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn owned_by_a_state_instance_of_another_root() {
        let root = temp_root("owned-state");
        std::fs::create_dir_all(root.join("lib")).unwrap();
        let config = Config::default();
        let mut state = State::default();
        let path = root.join("lib");
        assert!(!folder_owned(&config, &root, &state, &path));

        // A recorded non-canonical base still matches once canonicalized.
        let mut i = instance("lib");
        i.base_folder = root.join("lib/../lib");
        state.instances.insert("lib".into(), i);
        assert!(folder_owned(&config, &root, &state, &path));

        // A base that no longer exists is compared raw.
        let mut i = instance("lib");
        i.base_folder = "/gone/lib".into();
        state.instances.insert("lib".into(), i);
        assert!(folder_owned(&config, &root, &state, Path::new("/gone/lib")));
        assert!(!folder_owned(&config, &root, &state, &path));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn directly_mounted_sees_primary_and_direct_extras() {
        let lib = Path::new("/tmp/lib");
        let mut state = State::default();
        let mut other = instance("other");
        other.folders.push(FolderMount {
            target: "/workspaces/lib".into(),
            base: lib.into(),
            worktree: Some("/cfg/.worktrees/other.folders/lib-12345678".into()),
        });
        state.instances.insert("other".into(), other);
        // Through a worktree: not a direct mount.
        assert!(!folder_directly_mounted(&state, lib, None));

        state.instances.get_mut("other").unwrap().folders[0].worktree = None;
        assert!(folder_directly_mounted(&state, lib, None));
        // The instance being built doesn't count against itself.
        assert!(!folder_directly_mounted(&state, lib, Some("other")));

        // A primary checkout counts too.
        let mut owner = instance("lib");
        owner.folder = lib.into();
        state.instances.insert("lib".into(), owner);
        assert!(folder_directly_mounted(&state, lib, Some("other")));
        assert!(!folder_directly_mounted(&state, Path::new("/tmp/elsewhere"), None));
    }
}
