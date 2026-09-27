//! Git worktrees for repeat instances of one repo, so working trees are never
//! shared.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// Bind mount for the base repo's `.git` at the identical host path, so a
/// worktree's absolute `gitdir` pointer resolves inside the container.
pub(super) fn git_companion_mount(base: &Path) -> String {
    let git = base.join(".git");
    format!("{}:{}", git.display(), git.display())
}

/// Create a git worktree on a fresh `branch`. Git refuses to check out a branch
/// already checked out elsewhere, so the branch must be unique per instance
/// (the default `sandbox/${instance}` pattern guarantees this).
///
/// The branch starts from `base_ref` when given, else the remote's default
/// branch (see [`worktree_start_point`]), not the base repo's HEAD: another
/// agent may be mid-work on a feature branch in the base checkout.
pub(super) fn create_worktree(
    base: &Path,
    worktree: &Path,
    branch: &str,
    base_ref: Option<&str>,
) -> Result<()> {
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let start = worktree_start_point(base, base_ref)?;
    // With an unborn HEAD, `git worktree add -b` infers `--orphan`, exits 0, and
    // lays down a worktree with no files; guard against handing that empty path
    // to the container as a bind mount.
    let head = std::process::Command::new("git")
        .args([
            "-C",
            &base.to_string_lossy(),
            "rev-parse",
            "--verify",
            &format!("{start}^{{commit}}"),
        ])
        .output()
        .context("failed to run git (is it installed?)")?;
    if !head.status.success() {
        // An explicit ref is the user's choice: never silently swap it out.
        if let Some(r) = base_ref {
            bail!("worktree base `{r}` is not a commit in `{}`", base.display());
        }
        bail!(
            "base repo `{}` has no commits; cannot create a worktree",
            base.display()
        );
    }
    // A worktree branch must be unique: `git worktree add -b` refuses a branch
    // that already exists. Catch it here with a message that points at the
    // likely cause (a constant `worktree-branch`/`--branch` with no `${instance}`).
    let exists = std::process::Command::new("git")
        .args([
            "-C",
            &base.to_string_lossy(),
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .context("failed to run git (is it installed?)")?;
    if exists.success() {
        bail!(
            "branch `{branch}` already exists; a worktree needs a unique branch \
             (include `${{instance}}` in `worktree-branch` or pass a distinct `--branch`)"
        );
    }
    let status = std::process::Command::new("git")
        .args([
            "-C",
            &base.to_string_lossy(),
            "worktree",
            "add",
            &worktree.to_string_lossy(),
            "-b",
            branch,
            // Starting from `origin/<default>` would otherwise set it as the
            // upstream, aiming a bare `git push` at main.
            "--no-track",
            &start,
        ])
        .status()
        .context("failed to run git (is it installed?)")?;
    if !status.success() {
        bail!("git worktree add failed for `{}`", worktree.display());
    }
    // A real worktree always has a `.git` entry pointing back at the base repo;
    // its absence means git exited 0 without checking anything out.
    if !worktree.join(".git").exists() {
        bail!(
            "git worktree add produced an empty worktree at `{}`",
            worktree.display()
        );
    }
    Ok(())
}

/// Git's output for `git -C base <args>` on success, trimmed; None on failure.
fn git_query(base: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(base)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Skip window for [`fetch_origin`]: back-to-back `run`s (or an IDE's
/// auto-fetch) shouldn't each pay a network round-trip.
const FETCH_FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether a fetch that last wrote `FETCH_HEAD` at `modified` is recent enough
/// to skip another. A future mtime (clock skew) counts as fresh.
fn fetch_is_fresh(modified: std::time::SystemTime, now: std::time::SystemTime) -> bool {
    now.duration_since(modified).map_or(true, |age| age < FETCH_FRESH_FOR)
}

/// `git fetch origin` unless one ran within [`FETCH_FRESH_FOR`]. Git records no
/// fetch timestamp, but every fetch rewrites `FETCH_HEAD`, so its mtime stands
/// in (missing file = never fetched). `--git-path` resolves it for a base that
/// is itself a linked worktree.
fn fetch_origin(base: &Path) -> Result<()> {
    let fresh = git_query(base, &["rev-parse", "--git-path", "FETCH_HEAD"])
        .and_then(|p| std::fs::metadata(base.join(p)).ok())
        .and_then(|m| m.modified().ok())
        .is_some_and(|t| fetch_is_fresh(t, std::time::SystemTime::now()));
    if fresh {
        return Ok(());
    }
    let fetch = std::process::Command::new("git")
        .arg("-C")
        .arg(base)
        .args(["fetch", "--quiet", "origin"])
        .output()
        .context("failed to run git (is it installed?)")?;
    if !fetch.status.success() {
        eprintln!(
            "warning: `git fetch origin` failed in `{}`; worktree may start from a stale ref: {}",
            base.display(),
            String::from_utf8_lossy(&fetch.stderr).trim()
        );
    }
    Ok(())
}

/// Refresh `origin` and pick the ref a new worktree branch starts from.
///
/// `fetch` only moves `refs/remotes/origin/*` and adds objects — the base
/// checkout's HEAD, index and files are untouched, so it is safe while
/// another agent works there. It is skipped when one ran in the last two
/// minutes (see [`fetch_origin`]). Its output is captured (the TUI calls this
/// off-screen); a failed fetch (offline, auth) only warns and falls back to
/// the last-fetched refs.
///
/// Resolution: `explicit` (`--base`/`worktree-base`) → `origin/HEAD` →
/// `origin/main` → `origin/master` → `HEAD`. The fetch still runs for an
/// explicit ref so e.g. `origin/develop` is fresh too. `origin/HEAD` is unset
/// for repos that added the remote after `git init`; we don't `remote
/// set-head` since that writes to the base repo. Repos with no `origin` keep
/// the old start-from-HEAD behavior.
fn worktree_start_point(base: &Path, explicit: Option<&str>) -> Result<String> {
    let fallback = || explicit.unwrap_or("HEAD").to_string();
    if git_query(base, &["remote", "get-url", "origin"]).is_none() {
        return Ok(fallback());
    }
    fetch_origin(base)?;
    if let Some(r) = explicit {
        return Ok(r.to_string());
    }
    if let Some(r) = git_query(base, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]) {
        return Ok(r);
    }
    for r in ["origin/main", "origin/master"] {
        let full = format!("refs/remotes/{r}");
        if git_query(base, &["show-ref", "--verify", "--quiet", &full]).is_some() {
            return Ok(r.to_string());
        }
    }
    Ok("HEAD".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_companion_mount_uses_identical_paths() {
        let mount = git_companion_mount(Path::new("/home/u/repo"));
        assert_eq!(mount, "/home/u/repo/.git:/home/u/repo/.git");
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn fetch_freshness_window() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::now();
        assert!(fetch_is_fresh(now - Duration::from_secs(60), now));
        assert!(!fetch_is_fresh(now - Duration::from_secs(120), now));
        assert!(!fetch_is_fresh(now - Duration::from_secs(3600), now));
        assert!(fetch_is_fresh(now + Duration::from_secs(5), now));
    }

    #[test]
    fn worktree_starts_from_fetched_origin_default_untracked() {
        let root = std::env::temp_dir().join(format!("devsandbox-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (origin, base, wt) = (root.join("origin"), root.join("base"), root.join("wt"));
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "main"]);
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&root, &["clone", "-q", &origin.to_string_lossy(), "base"]);
        // Base checkout sits on a feature branch, as a working agent's would.
        git(&base, &["checkout", "-q", "-b", "feature"]);
        git(&base, &["commit", "-q", "--allow-empty", "-m", "wip"]);
        let feature_tip = git(&base, &["rev-parse", "HEAD"]);
        // Upstream moves on after the clone: only a fetch can see this.
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let main_tip = git(&origin, &["rev-parse", "HEAD"]);

        create_worktree(&base, &wt, "sandbox/x", None).unwrap();

        assert_eq!(git(&wt, &["rev-parse", "HEAD"]), main_tip);
        let upstream = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
            .output()
            .unwrap();
        assert!(!upstream.status.success(), "branch must not track origin");
        assert_eq!(git(&base, &["rev-parse", "HEAD"]), feature_tip);
        assert_eq!(git(&base, &["branch", "--show-current"]), "feature");

        // An explicit base wins over detection; a bad one errors, no fallback.
        let wt2 = root.join("wt2");
        create_worktree(&base, &wt2, "sandbox/y", Some("feature")).unwrap();
        assert_eq!(git(&wt2, &["rev-parse", "HEAD"]), feature_tip);
        let err = create_worktree(&base, &root.join("wt3"), "sandbox/z", Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("worktree base `nope`"), "{err}");

        std::fs::remove_dir_all(&root).unwrap();
    }
}
