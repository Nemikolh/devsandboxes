//! Whether a `folders` entry gets its own detached worktree or the live host
//! dir (docs/folders-worktrees.md). Each host folder has one owner of its
//! direct checkout; everyone else gets a worktree so no two instances write
//! to one working tree. Pure over config + state so it's testable without git.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{short_hash, Config, FolderWorktree};
use crate::state::{FolderMount, State};

use super::mounts::ResolvedFolder;
use super::worktree::create_detached_worktree;

/// The [`FolderMount`] for each current `folders` entry of `instance`, creating
/// detached worktrees where [`needs_worktree`] says so. A `prior` record with
/// the same target and base is reused as is, so a rebuild keeps the working
/// tree (as it keeps the primary worktree) and the decision isn't re-made
/// against a state that now contains this instance's own holds. A reused
/// worktree that vanished from disk is recreated: the record is ours, and
/// mounting a missing dir would fail (or, on some runtimes, create an empty
/// one) less clearly than git does.
#[allow(clippy::too_many_arguments)]
pub(super) fn mount_folders(
    config: &Config,
    dir: &Path,
    config_dir: &Path,
    state: &State,
    instance: &str,
    instance_id: &str,
    dispatched: bool,
    prior: &[FolderMount],
    resolved: &[ResolvedFolder],
) -> Result<Vec<FolderMount>> {
    let mut out = Vec::with_capacity(resolved.len());
    for entry in resolved {
        let host = &entry.host;
        let context = || format!("`folders` entry `{}` ({})", entry.target, host.display());
        if let Some(fm) = reusable(prior, &entry.target, host, entry.mode) {
            if let Some(wt) = fm.worktree.as_deref().filter(|wt| !wt.exists()) {
                eprintln!("warning: worktree `{}` is missing; recreating it", wt.display());
                super::check_repo(host).with_context(context)?;
                create_detached_worktree(host, wt).with_context(context)?;
            }
            out.push(fm.clone());
            continue;
        }
        let repo = is_repo_root(host);
        // Only `auto` on a repo outside a dispatcher looks at who else holds
        // it; skip the config/state scans otherwise.
        let contested = entry.mode == FolderWorktree::Auto && repo && !dispatched;
        let owned = contested && folder_owned(config, dir, state, host);
        let direct = contested && !owned && folder_directly_mounted(state, host, Some(instance));
        let worktree = if needs_worktree(entry.mode, repo, dispatched, owned, direct)
            .with_context(context)?
        {
            let wt = folder_worktree_path(config_dir, instance_id, host);
            super::check_repo(host).with_context(context)?;
            if wt.exists() {
                // A leftover of a failed run (materialize doesn't clean up):
                // reuse it only if it's still a live worktree of this base.
                if !is_worktree_of(&wt, host) {
                    bail!(
                        "{}: `{}` already exists and is not a worktree of it; remove it and retry",
                        context(),
                        wt.display()
                    );
                }
            } else {
                create_detached_worktree(host, &wt).with_context(context)?;
            }
            Some(wt)
        } else {
            None
        };
        out.push(FolderMount { target: entry.target.clone(), base: host.clone(), worktree });
    }
    Ok(out)
}

/// The prior record `target` -> `base` can be reused from, if any. Only one
/// that fits `mode`: after switching an entry to `always`/`never`, the rebuild
/// that drift points at must apply it. `auto` keeps whichever was mounted
/// last (current records come first, see [`merge_folder_mounts`]); a worktree
/// left unmounted by `never` stays recorded, so `always` picks it back up.
fn reusable<'a>(
    prior: &'a [FolderMount],
    target: &str,
    base: &Path,
    mode: FolderWorktree,
) -> Option<&'a FolderMount> {
    prior.iter().find(|f| {
        f.target == target
            && f.base == base
            && match mode {
                FolderWorktree::Auto => true,
                FolderWorktree::Always => f.worktree.is_some(),
                FolderWorktree::Never => f.worktree.is_none(),
            }
    })
}

/// `<config_dir>/.worktrees/<instance_id>.folders/<basename>-<hash8>`: beside
/// the primary worktree (which *is* `.worktrees/<instance_id>/`), the path
/// hash keeping two same-named repos apart (as `link_store` does).
fn folder_worktree_path(config_dir: &Path, instance_id: &str, base: &Path) -> PathBuf {
    let basename = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let hash = short_hash(&base.to_string_lossy());
    config_dir
        .join(".worktrees")
        .join(format!("{instance_id}.folders"))
        .join(format!("{basename}-{}", &hash[..8]))
}

/// Whether `wt` is a registered worktree of `base`: its `.git` file points
/// into `<base>/.git/worktrees/`, and that admin dir still exists (not pruned).
fn is_worktree_of(wt: &Path, base: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(wt.join(".git")) else {
        return false;
    };
    let Some(gitdir) = text.trim().strip_prefix("gitdir:").map(|g| PathBuf::from(g.trim())) else {
        return false;
    };
    gitdir.starts_with(base.join(".git").join("worktrees")) && gitdir.is_dir()
}

/// The `folders` records to store: the current ones, plus prior records with
/// a worktree that none of them carries on (entry removed, or its base
/// changed), so `rm` still finds and cleans those worktrees. Prior direct
/// records that aren't current are dropped: nothing mounts them anymore, and
/// keeping them would keep the checkout reserved.
pub(super) fn merge_folder_mounts(prior: &[FolderMount], current: Vec<FolderMount>) -> Vec<FolderMount> {
    let kept: Vec<FolderMount> = prior
        .iter()
        .filter(|p| p.worktree.is_some() && !current.iter().any(|c| c.worktree == p.worktree))
        .cloned()
        .collect();
    let mut out = current;
    out.extend(kept);
    out
}

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

    fn fm(target: &str, base: &str, worktree: Option<&str>) -> FolderMount {
        FolderMount { target: target.into(), base: base.into(), worktree: worktree.map(Into::into) }
    }

    #[test]
    fn merge_keeps_orphaned_worktrees_only() {
        let prior = vec![
            fm("/w/reused", "/r", Some("/wt/r")),
            fm("/w/gone-wt", "/g", Some("/wt/g")),
            fm("/w/gone-direct", "/d", None),
            fm("/w/rebased", "/old", Some("/wt/old")),
        ];
        let current = vec![
            fm("/w/reused", "/r", Some("/wt/r")),
            fm("/w/rebased", "/new", None),
            fm("/w/new", "/n", Some("/wt/n")),
        ];
        assert_eq!(
            merge_folder_mounts(&prior, current),
            vec![
                fm("/w/reused", "/r", Some("/wt/r")),
                fm("/w/rebased", "/new", None),
                fm("/w/new", "/n", Some("/wt/n")),
                // Removed entry, and the one whose base changed: rm must still see them.
                fm("/w/gone-wt", "/g", Some("/wt/g")),
                fm("/w/rebased", "/old", Some("/wt/old")),
            ]
        );
    }

    /// A mode switch must take effect on rebuild; `auto` keeps the last mount.
    #[test]
    fn reuse_respects_the_mode() {
        // After `never` left the worktree recorded but unmounted.
        let prior = vec![fm("/w/a", "/a", None), fm("/w/a", "/a", Some("/wt/a"))];
        let base = Path::new("/a");
        let pick = |mode| reusable(&prior, "/w/a", base, mode).cloned();
        assert_eq!(pick(FolderWorktree::Auto), Some(fm("/w/a", "/a", None)));
        assert_eq!(pick(FolderWorktree::Never), Some(fm("/w/a", "/a", None)));
        assert_eq!(pick(FolderWorktree::Always), Some(fm("/w/a", "/a", Some("/wt/a"))));
        let direct_only = vec![fm("/w/a", "/a", None)];
        assert_eq!(reusable(&direct_only, "/w/a", base, FolderWorktree::Always), None);
    }

    #[test]
    fn mount_folders_reuses_prior_and_mounts_non_repos_directly() {
        let root = temp_root("mount");
        for d in ["plain", "repo/.git", "wt"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let (plain, repo) = (root.join("plain"), root.join("repo"));
        let wt = root.join("wt");
        let resolved = |target: &str, host: &Path, mode| ResolvedFolder {
            target: target.into(),
            host: host.to_path_buf(),
            mode,
        };
        // Owned by a state instance, so `auto` would want a worktree: the
        // matching prior record wins and no git runs.
        let mut state = State::default();
        let mut owner = instance("owner");
        owner.base_folder = repo.clone();
        state.instances.insert("owner".into(), owner);
        let prior = vec![FolderMount {
            target: "/w/repo".into(),
            base: repo.clone(),
            worktree: Some(wt.clone()),
        }];
        let out = mount_folders(
            &Config::default(),
            &root,
            &root,
            &state,
            "me",
            "me",
            true,
            &prior,
            &[resolved("/w/repo", &repo, Auto), resolved("/w/plain", &plain, Auto)],
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                prior[0].clone(),
                FolderMount { target: "/w/plain".into(), base: plain.clone(), worktree: None },
            ]
        );
        // `always` on a non-repo names the entry.
        let err = mount_folders(
            &Config::default(),
            &root,
            &root,
            &state,
            "me",
            "me",
            false,
            &[],
            &[resolved("/w/plain", &plain, Always)],
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("`folders` entry `/w/plain`") && msg.contains("git repo root"), "{msg}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn folder_worktree_path_is_per_instance_and_per_base() {
        let a = folder_worktree_path(Path::new("/cfg"), "b-2", Path::new("/src/lib"));
        let b = folder_worktree_path(Path::new("/cfg"), "b-2", Path::new("/other/lib"));
        assert!(a.starts_with("/cfg/.worktrees/b-2.folders"), "{}", a.display());
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("lib-") && name.len() == "lib-".len() + 8, "{name}");
        assert_ne!(a, b);
    }

    #[test]
    fn worktree_of_checks_the_gitdir_pointer() {
        let root = temp_root("wt-of");
        let base = root.join("base");
        let admin = base.join(".git/worktrees/x");
        std::fs::create_dir_all(&admin).unwrap();
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        assert!(!is_worktree_of(&wt, &base));
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        assert!(is_worktree_of(&wt, &base));
        assert!(!is_worktree_of(&wt, &root.join("elsewhere")));
        // Pruned admin dir: stale, not reusable.
        std::fs::remove_dir_all(&admin).unwrap();
        assert!(!is_worktree_of(&wt, &base));
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
