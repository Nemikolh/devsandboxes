//! WIP snapshots (fleet plan decision 73; docs/architecture/daemon/fleet.md,
//! *Moving code*): a checkout's working tree as one commit over `HEAD`,
//! tracked changes, deletions and untracked non-ignored files included,
//! made through a temp index so the user's index, branches, stash and
//! working tree stay as they are; and the other way, [`apply_wip`], a
//! snapshot's tree put back into a checkout as uncommitted changes. All
//! through `host_git` (docs/decisions/0003-host-git-never-runs-repo-controlled-code.md),
//! every child bounded as [`super::repo`]'s are.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::repo::{
    IDENTITY, QUICK, ROOT_REF, Repo, TempPath, WHOLE, check_branch, check_oid, check_rev, ensure_root, head_commit,
    optional, run,
};
use crate::commands::run::host_git;

/// What [`snapshot`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The commit the snapshot sits on: `HEAD`'s, or [`ROOT_REF`]'s in a
    /// repo with no commit yet.
    pub base: String,
    /// The checkout's branch (`None` detached), what `HEAD` names even
    /// when it has no commit yet.
    pub branch: Option<String>,
    /// The snapshot commit, its one parent `base`.
    pub wip: String,
}

/// The message of every snapshot commit.
pub const WIP_MESSAGE: &str = "devsandbox wip";

/// Snapshot `checkout`'s working tree onto `wip_ref` (`refs/devsandbox/…`,
/// [`check_wip_ref`]): a temp index in the checkout's own git dir (a linked
/// worktree's for one) seeded from the base (`read-tree`), `add -A`
/// (ignored files left out, by the usual ignore rules), `write-tree`,
/// `commit-tree` with devsandbox's [`IDENTITY`], unsigned, then
/// `update-ref`. The temp index is removed whatever happens. Only the
/// object store and `wip_ref` (and, unborn, [`ROOT_REF`]) change.
pub fn snapshot(checkout: &Path, wip_ref: &str) -> Result<Snapshot> {
    check_wip_ref(wip_ref)?;
    let base = match head_commit(checkout)? {
        Some(head) => head,
        None => ensure_root(checkout)?.context("HEAD has no commit and no devsandbox root was made")?,
    };
    let branch = optional(host_git(checkout)?.args(["symbolic-ref", "-q", "--short", "HEAD"]))?;
    let index = TempPath::new(&Repo::Checkout(checkout).git_dir()?, "wip-index");
    let indexed = || -> Result<std::process::Command> {
        let mut cmd = host_git(checkout)?;
        cmd.env("GIT_INDEX_FILE", &index.0);
        Ok(cmd)
    };
    run(indexed()?.arg("read-tree").arg(&base), QUICK).context("cannot seed the snapshot's index")?;
    run(indexed()?.args(["add", "-A"]), WHOLE).context("cannot add the working tree to the snapshot")?;
    let tree = run(indexed()?.arg("write-tree"), WHOLE).context("cannot write the snapshot's tree")?;
    let wip = run(
        host_git(checkout)?
            .args(["-c", "commit.gpgsign=false", "commit-tree"])
            .arg(tree.trim())
            .arg("-p")
            .arg(&base)
            .args(["-m", WIP_MESSAGE])
            .envs(IDENTITY),
        QUICK,
    )
    .context("cannot commit the snapshot")?
    .trim()
    .to_string();
    check_oid(&wip)?;
    run(host_git(checkout)?.args(["update-ref", wip_ref, &wip]), QUICK)
        .with_context(|| format!("cannot set {wip_ref}"))?;
    Ok(Snapshot { base, branch, wip })
}

/// Make `checkout`'s working tree `wip`'s tree (a commit id or full ref,
/// [`check_rev`]) and leave its index at `HEAD`: what the snapshot had
/// modified, deleted or added shows as uncommitted changes (added files
/// as untracked). `read-tree -u --reset` then a mixed `reset`, LFS smudge
/// off as `repo::land_from`'s. It overwrites the working tree: meant for a
/// checkout the transfer just made (decision 73).
pub fn apply_wip(checkout: &Path, wip: &str) -> Result<()> {
    check_rev(wip)?;
    run(host_git(checkout)?.args(["rev-parse", "-q", "--verify"]).arg(format!("{wip}^{{commit}}")), QUICK)
        .with_context(|| format!("{wip} isn't a commit in {}", checkout.display()))?;
    run(host_git(checkout)?.args(["read-tree", "-u", "--reset"]).arg(wip).env("GIT_LFS_SKIP_SMUDGE", "1"), WHOLE)
        .with_context(|| format!("cannot check out {wip}'s tree"))?;
    run(host_git(checkout)?.args(["reset", "-q"]).env("GIT_LFS_SKIP_SMUDGE", "1"), WHOLE)
        .context("cannot reset the index to HEAD")?;
    Ok(())
}

/// A ref a snapshot goes on: under `refs/devsandbox/`, the rest a name
/// [`check_branch`] accepts, not [`ROOT_REF`].
pub fn check_wip_ref(name: &str) -> Result<()> {
    match name.strip_prefix("refs/devsandbox/") {
        Some(rest) if name != ROOT_REF && check_branch(rest).is_ok() => Ok(()),
        _ => bail!("`{name}` isn't a ref devsandbox snapshots onto"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::repo::tests::{git, scratch};
    use super::super::repo::{bundle_refs, identity};
    use super::*;

    /// What a snapshot must leave alone: status, the index file's bytes,
    /// HEAD, every branch tip and the stash.
    fn user_state(repo: &Path, index: &Path) -> (String, Vec<u8>, String, String, String) {
        (
            git(repo, &["status", "--porcelain=v1", "-uall"]),
            std::fs::read(index).unwrap(),
            git(repo, &["rev-parse", "HEAD"]),
            git(repo, &["for-each-ref", "refs/heads"]),
            git(repo, &["stash", "list"]),
        )
    }

    /// A checkout at `<dir>/a` on `main` with `keep`, `edit`, `gone`,
    /// `staged` and a `.gitignore` of `*.log` committed; then `edit`
    /// modified, `gone` deleted, `staged` changed and staged, `new`
    /// untracked and `x.log` ignored.
    fn dirty(dir: &Path) -> std::path::PathBuf {
        let a = dir.join("a");
        std::fs::create_dir_all(&a).unwrap();
        git(&a, &["init", "-q", "-b", "main"]);
        for (f, body) in [("keep", "k\n"), ("edit", "e\n"), ("gone", "g\n"), ("staged", "s\n"), (".gitignore", "*.log\n")] {
            std::fs::write(a.join(f), body).unwrap();
        }
        git(&a, &["add", "-A"]);
        git(&a, &["commit", "-q", "-m", "one"]);
        std::fs::write(a.join("edit"), "e2\n").unwrap();
        std::fs::remove_file(a.join("gone")).unwrap();
        std::fs::write(a.join("staged"), "s2\n").unwrap();
        git(&a, &["add", "staged"]);
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::write(a.join("sub/new"), "n\n").unwrap();
        std::fs::write(a.join("x.log"), "ignored\n").unwrap();
        a
    }

    fn leftovers(gitdir: &Path) -> Vec<String> {
        std::fs::read_dir(gitdir)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .filter(|n| n.starts_with("devsandbox-"))
            .collect()
    }

    #[test]
    fn a_snapshot_captures_the_working_tree_and_leaves_the_user_alone() {
        let dir = scratch("wip-snap");
        let a = dirty(&dir);
        let before = user_state(&a, &a.join(".git/index"));
        let snap = snapshot(&a, "refs/devsandbox/wip/main").unwrap();
        assert_eq!(user_state(&a, &a.join(".git/index")), before, "user's state untouched");
        assert_eq!(before.4, "", "no stash");
        assert!(leftovers(&a.join(".git")).is_empty(), "temp index removed");

        assert_eq!(snap.base, git(&a, &["rev-parse", "HEAD"]));
        assert_eq!(snap.branch.as_deref(), Some("main"));
        assert_eq!(git(&a, &["rev-parse", "refs/devsandbox/wip/main"]), snap.wip);
        assert_eq!(git(&a, &["rev-parse", &format!("{}^", snap.wip)]), snap.base);
        let files = git(&a, &["ls-tree", "-r", "--name-only", &snap.wip]);
        assert_eq!(files.lines().collect::<Vec<_>>(), [".gitignore", "edit", "keep", "staged", "sub/new"]);
        assert_eq!(git(&a, &["show", &format!("{}:edit", snap.wip)]), "e2");
        assert_eq!(git(&a, &["show", &format!("{}:staged", snap.wip)]), "s2");
        assert_eq!(git(&a, &["log", "-1", "--format=%an <%ae>|%cn|%s", &snap.wip]), "devsandbox <devsandbox@localhost>|devsandbox|devsandbox wip");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_linked_worktree_snapshots_its_own_state() {
        let dir = scratch("wip-wt");
        let a = dirty(&dir);
        let wt = dir.join("wt");
        git(&a, &["worktree", "add", "-q", "-b", "other", wt.to_str().unwrap()]);
        std::fs::write(wt.join("keep"), "wt\n").unwrap();
        let main_before = user_state(&a, &a.join(".git/index"));
        let wt_index = a.join(".git/worktrees/wt/index");
        let wt_before = user_state(&wt, &wt_index);

        let snap = snapshot(&wt, "refs/devsandbox/wip/other").unwrap();
        assert_eq!(user_state(&wt, &wt_index), wt_before);
        assert_eq!(user_state(&a, &a.join(".git/index")), main_before);
        assert!(leftovers(&a.join(".git/worktrees/wt")).is_empty());
        assert_eq!(snap.branch.as_deref(), Some("other"));
        assert_eq!(snap.base, git(&wt, &["rev-parse", "HEAD"]));
        assert_eq!(git(&a, &["show", &format!("{}:keep", snap.wip)]), "wt");
        // The worktree's tree, not the main checkout's: `gone` is still there.
        assert_eq!(git(&a, &["show", &format!("{}:gone", snap.wip)]), "g");
        assert!(git(&a, &["ls-tree", "-r", "--name-only", &snap.wip]).lines().all(|f| f != "sub/new"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unborn_repo_snapshots_over_its_devsandbox_root() {
        let dir = scratch("wip-unborn");
        let (e1, e2) = (dir.join("e1"), dir.join("e2"));
        for e in [&e1, &e2] {
            std::fs::create_dir_all(e).unwrap();
            git(e, &["init", "-q", "-b", "main"]);
        }
        std::fs::write(e1.join("f"), "f\n").unwrap();
        let snap = snapshot(&e1, "refs/devsandbox/wip/main").unwrap();
        let root = git(&e1, &["rev-parse", ROOT_REF]);
        assert_eq!(snap.base, root);
        assert_eq!(snap.branch.as_deref(), Some("main"));
        assert_eq!(git(&e1, &["ls-tree", "-r", "--name-only", &snap.wip]), "f");
        assert_eq!(git(&e1, &["ls-tree", &root]), "", "the root is the empty tree");
        assert_eq!(git(&e1, &["status", "--porcelain=v1", "-uall"]), "?? f", "still unborn, f untracked");
        // Made once, reused; another empty repo gets its own.
        assert_eq!(ensure_root(&e1).unwrap().as_deref(), Some(root.as_str()));
        assert_eq!(snapshot(&e1, "refs/devsandbox/wip/main").unwrap().base, root);
        let root2 = ensure_root(&e2).unwrap().unwrap();
        assert_ne!(root2, root);
        assert_ne!(identity(Repo::Checkout(&e1), ROOT_REF).unwrap(), identity(Repo::Checkout(&e2), ROOT_REF).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_comes_back_as_uncommitted_changes() {
        let dir = scratch("wip-apply");
        let a = dirty(&dir);
        let snap = snapshot(&a, "refs/devsandbox/wip/main").unwrap();
        let out = dir.join("x.bundle");
        assert!(bundle_refs(Repo::Checkout(&a), &["refs/heads/main", "refs/devsandbox/wip/main"], &[], &out).unwrap());

        let b = dir.join("b");
        std::fs::create_dir_all(&b).unwrap();
        git(&b, &["init", "-q", "-b", "main"]);
        git(&b, &["fetch", "-q", "--update-head-ok", out.to_str().unwrap(), "+refs/heads/main:refs/heads/main", "+refs/devsandbox/wip/main:refs/devsandbox/wip/main"]);
        git(&b, &["reset", "-q", "--hard"]);
        apply_wip(&b, "refs/devsandbox/wip/main").unwrap();

        // A's `staged` was staged; B has it as a plain modification.
        assert_eq!(git(&b, &["diff", "--cached", "--name-only"]), "", "index at HEAD");
        assert_eq!(git(&b, &["diff", "--name-status"]), "M\tedit\nD\tgone\nM\tstaged");
        assert_eq!(git(&b, &["ls-files", "--others", "--exclude-standard"]), "sub/new");
        for f in ["edit", "keep", "staged", "sub/new", ".gitignore"] {
            assert_eq!(std::fs::read(a.join(f)).unwrap(), std::fs::read(b.join(f)).unwrap(), "{f}");
        }
        assert!(!b.join("x.log").exists(), "ignored files don't travel");
        assert_eq!(git(&b, &["rev-parse", "HEAD"]), snap.base);
        assert!(apply_wip(&b, "-x").is_err());
        assert!(apply_wip(&b, &"0".repeat(40)).is_err(), "not a commit there");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wip_refs_are_checked() {
        for ok in ["refs/devsandbox/wip/main", "refs/devsandbox/wip/0123abc"] {
            check_wip_ref(ok).unwrap();
        }
        for bad in ["refs/heads/main", "refs/devsandbox/", ROOT_REF, "refs/devsandbox/-x", "refs/devsandbox/a..b", "wip"] {
            assert!(check_wip_ref(bad).is_err(), "{bad:?}");
        }
        let dir = scratch("wip-bad-ref");
        git(&dir, &["init", "-q"]);
        assert!(snapshot(&dir, "refs/heads/main").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
