//! A sandbox folder's repo moved as a git bundle (docs/architecture/daemon/fleet.md,
//! *Moving code*; fleet plan decision 12): the hub fetches the folder's
//! `origin` into its own bare repo cache with its own credentials and
//! bundles one branch from there; the worker unpacks the bundle into a
//! checkout with no remote, never needing credentials. All git runs through
//! `commands::run`'s `host_git` family
//! (docs/decisions/0003-host-git-never-runs-repo-controlled-code.md):
//! hooks and fsmonitor off, the repo's config checked first.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::commands::run::{host_git, host_git_bare, host_git_new};

/// What a checkout on the hub says: its `origin` URL and its branch (`None`
/// detached).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub origin: Option<String>,
    pub branch: Option<String>,
}

/// `folder`'s origin and current branch. A folder that isn't a checkout's
/// root (no `.git` in it) is `None`.
pub fn probe(folder: &Path) -> Result<Option<Checkout>> {
    if folder.join(".git").symlink_metadata().is_err() {
        return Ok(None);
    }
    let origin = optional(host_git(folder)?.args(["remote", "get-url", "origin"]))?;
    let branch = optional(host_git(folder)?.args(["symbolic-ref", "-q", "--short", "HEAD"]))?;
    Ok(Some(Checkout { origin, branch }))
}

/// The cache of `origin` in `repos` (`<data>/devsandbox/repos`): a bare
/// repo named by a hash of the URL, `<sha256 prefix>.git`. (Repo identity
/// by root commits, which needs no URL, is fleet step 5.)
pub fn cache_dir(repos: &Path, origin: &str) -> PathBuf {
    let hash: String = Sha256::digest(origin.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    repos.join(format!("{hash}.git"))
}

/// Make `cache` (bare) if it isn't there and fetch every branch of `origin`
/// into it (`+refs/heads/*:refs/heads/*`, pruned, no tags). The hub's own
/// credentials, no prompt; `ext::` transports off.
pub fn fetch(cache: &Path, origin: &str) -> Result<()> {
    if origin.starts_with('-') {
        bail!("origin `{origin}` isn't a URL");
    }
    if !cache.join("HEAD").exists() {
        std::fs::create_dir_all(cache).with_context(|| format!("cannot create {}", cache.display()))?;
        run(host_git_new(cache)?.args(["init", "-q", "--bare"]))?;
    }
    run(host_git_bare(cache)?
        .args(["-c", "protocol.ext.allow=never", "fetch", "-q", "--no-tags", "--prune", "--"])
        .arg(origin)
        .arg("+refs/heads/*:refs/heads/*"))
    .with_context(|| format!("cannot fetch {origin}"))?;
    Ok(())
}

/// `branch` is in `cache`.
pub fn has_branch(cache: &Path, branch: &str) -> Result<bool> {
    check_branch(branch)?;
    Ok(optional(host_git_bare(cache)?.args(["rev-parse", "-q", "--verify"]).arg(format!("refs/heads/{branch}^{{commit}}")))?.is_some())
}

/// A bundle of `cache`'s `branch` (every commit of it) at `out`.
pub fn bundle(cache: &Path, branch: &str, out: &Path) -> Result<()> {
    check_branch(branch)?;
    run(host_git_bare(cache)?.args(["bundle", "create", "-q"]).arg(out).arg(format!("refs/heads/{branch}")))
        .with_context(|| format!("cannot bundle {branch}"))?;
    Ok(())
}

/// A branch name as devsandbox moves it: plain characters
/// (`[A-Za-z0-9._+/-]`), `/`-separated components that don't start with
/// `.` or `-`, no `..`, not ending `.lock`; at most 200 bytes. Narrower than
/// git's rule, so a name that passes is also one no option parser reads as
/// a flag.
pub fn check_branch(branch: &str) -> Result<()> {
    let ok_char = |c: char| c.is_ascii_alphanumeric() || "._+/-".contains(c);
    let ok_part = |p: &str| !p.is_empty() && !p.starts_with(['.', '-']) && !p.ends_with(".lock");
    if branch.is_empty() || branch.len() > 200 || !branch.chars().all(ok_char) || branch.contains("..") || !branch.split('/').all(ok_part) {
        bail!("`{branch}` isn't a branch name devsandbox moves");
    }
    Ok(())
}

/// The worker's end: unpack `bundle` (one `refs/heads/<branch>`) into the
/// checkout at `at`, made (with its parents) if missing; the branch's name.
/// A new checkout gets the branch checked out; an existing one (a checkout
/// a bundle made before) gets the branch updated: fast-forwarded when it's
/// the one checked out, else moved to the bundle's (it's the hub's copy).
/// The checkout has no remote. A dir that exists and isn't a checkout is
/// refused, unless empty.
pub fn land(bundle: &Path, at: &Path) -> Result<String> {
    let fresh = !at.join(".git").exists();
    if fresh {
        std::fs::create_dir_all(at).with_context(|| format!("cannot create {}", at.display()))?;
        if std::fs::read_dir(at)?.next().is_some() {
            bail!("{} exists and isn't a git checkout", at.display());
        }
        run(host_git_new(at)?.args(["init", "-q"]))?;
    }
    let heads = run(host_git(at)?.args(["bundle", "list-heads"]).arg(bundle)).context("not a git bundle")?;
    let refs: Vec<&str> = heads.lines().filter_map(|l| l.split_whitespace().nth(1)).collect();
    let branch = match refs.as_slice() {
        [one] => one.strip_prefix("refs/heads/").with_context(|| format!("the bundle carries {one}, not a branch"))?,
        _ => bail!("the bundle carries {} refs, not one branch", refs.len()),
    };
    check_branch(branch)?;
    let head = optional(host_git(at)?.args(["symbolic-ref", "-q", "HEAD"]))?;
    let full = format!("refs/heads/{branch}");
    let fetch = |refspec: &str| -> Result<()> {
        run(host_git(at)?.args(["fetch", "-q", "--no-tags", "--update-head-ok"]).arg(bundle).arg(refspec))
            .with_context(|| format!("cannot fetch {branch} from the bundle"))?;
        Ok(())
    };
    if fresh {
        run(host_git(at)?.args(["symbolic-ref", "HEAD"]).arg(&full))?;
        fetch(&format!("+{full}:{full}"))?;
        run(host_git(at)?.args(["reset", "-q", "--hard"]).env("GIT_LFS_SKIP_SMUDGE", "1"))?;
    } else if head.as_deref() == Some(full.as_str()) {
        fetch(&full)?;
        run(host_git(at)?.args(["merge", "-q", "--ff-only", "FETCH_HEAD"]).env("GIT_LFS_SKIP_SMUDGE", "1"))
            .with_context(|| format!("cannot fast-forward {branch} in {}", at.display()))?;
    } else {
        fetch(&format!("+{full}:{full}"))?;
    }
    Ok(branch.to_string())
}

/// Run `cmd`; its stdout, or an error with its stderr.
fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().context("cannot run git")?;
    if !out.status.success() {
        bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `cmd`; its trimmed stdout, `None` when it exits 1 or 2 with nothing
/// on stdout (git's "no such thing" for these queries).
fn optional(cmd: &mut Command) -> Result<Option<String>> {
    let out = cmd.output().context("cannot run git")?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match out.status.code() {
        Some(0) if !text.is_empty() => Ok(Some(text)),
        Some(0 | 1 | 2) if text.is_empty() => Ok(None),
        _ => bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-code-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// Plain git on a test's own temp repos.
    pub fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare "origin" at `<dir>/origin.git` with `main` (one file) and
    /// `feature`, and the hub's checkout of it at `<dir>/<name>` on `main`.
    pub fn origin_and_checkout(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
        let origin = dir.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare"]);
        let seed = dir.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        std::fs::write(seed.join("README"), "main\n").unwrap();
        git(&seed, &["add", "README"]);
        git(&seed, &["commit", "-q", "-m", "main"]);
        git(&seed, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(seed.join("F"), "feature\n").unwrap();
        git(&seed, &["add", "F"]);
        git(&seed, &["commit", "-q", "-m", "feature"]);
        git(&seed, &["push", "-q", origin.to_str().unwrap(), "main", "feature"]);
        let checkout = dir.join(name);
        git(dir, &["clone", "-q", "-b", "main", origin.to_str().unwrap(), name]);
        (origin, checkout)
    }

    #[test]
    fn a_branch_goes_from_origin_through_the_cache_into_a_new_checkout() {
        let dir = scratch("bundle");
        let (origin, checkout) = origin_and_checkout(&dir, "web");
        let probed = probe(&checkout).unwrap().unwrap();
        assert_eq!(probed, Checkout { origin: Some(origin.to_str().unwrap().into()), branch: Some("main".into()) });
        assert_eq!(probe(&dir).unwrap(), None, "not a checkout");

        let cache = cache_dir(&dir.join("repos"), probed.origin.as_deref().unwrap());
        assert!(cache.file_name().unwrap().to_str().unwrap().ends_with(".git"));
        fetch(&cache, probed.origin.as_deref().unwrap()).unwrap();
        fetch(&cache, probed.origin.as_deref().unwrap()).unwrap();
        assert!(has_branch(&cache, "feature").unwrap());
        assert!(!has_branch(&cache, "nope").unwrap());
        bundle(&cache, "feature", &dir.join("f.bundle")).unwrap();

        let at = dir.join("worker/web");
        assert_eq!(land(&dir.join("f.bundle"), &at).unwrap(), "feature");
        assert_eq!(std::fs::read_to_string(at.join("F")).unwrap(), "feature\n");
        assert_eq!(git(&at, &["symbolic-ref", "--short", "HEAD"]), "feature");
        assert_eq!(git(&at, &["remote"]), "", "no remote on the worker");
        assert_eq!(git(&at, &["status", "--porcelain"]), "");

        // Again, after a new commit on origin: the checked-out branch fast-forwards.
        let seed = dir.join("seed");
        std::fs::write(seed.join("F"), "feature 2\n").unwrap();
        git(&seed, &["commit", "-q", "-am", "two"]);
        git(&seed, &["push", "-q", origin.to_str().unwrap(), "feature"]);
        fetch(&cache, origin.to_str().unwrap()).unwrap();
        bundle(&cache, "feature", &dir.join("f2.bundle")).unwrap();
        land(&dir.join("f2.bundle"), &at).unwrap();
        assert_eq!(std::fs::read_to_string(at.join("F")).unwrap(), "feature 2\n");
        // Another branch into it: updated, not checked out.
        bundle(&cache, "main", &dir.join("m.bundle")).unwrap();
        assert_eq!(land(&dir.join("m.bundle"), &at).unwrap(), "main");
        assert_eq!(git(&at, &["symbolic-ref", "--short", "HEAD"]), "feature");
        assert_eq!(git(&at, &["rev-parse", "main"]), git(&origin, &["rev-parse", "main"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn land_refuses_a_dir_that_isnt_a_checkout_and_bad_bundles() {
        let dir = scratch("land-refuses");
        std::fs::create_dir_all(dir.join("full")).unwrap();
        std::fs::write(dir.join("full/x"), "x").unwrap();
        std::fs::write(dir.join("junk.bundle"), "not a bundle").unwrap();
        let err = format!("{:#}", land(&dir.join("junk.bundle"), &dir.join("full")).unwrap_err());
        assert!(err.contains("isn't a git checkout"), "{err}");
        let err = format!("{:#}", land(&dir.join("junk.bundle"), &dir.join("empty")).unwrap_err());
        assert!(err.contains("not a git bundle"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_names_are_checked() {
        for ok in ["main", "feature/x-1", "v1.2", "a+b_c"] {
            check_branch(ok).unwrap();
        }
        for bad in ["", "-x", "a/.b", "a..b", "a b", "x.lock", "a//b", "/a", "a/", "a@{1}", "a~1", "a:b"] {
            assert!(check_branch(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_cache_is_refused_like_a_checkout() {
        let dir = scratch("bare-check");
        let cache = dir.join("c.git");
        std::fs::create_dir_all(&cache).unwrap();
        git(&cache, &["init", "-q", "--bare"]);
        host_git_bare(&cache).unwrap();
        std::fs::write(cache.join("config"), "[core]\n\tbare = true\n\tfsmonitor = /tmp/x\n").unwrap();
        let err = format!("{:#}", host_git_bare(&cache).unwrap_err());
        assert!(err.contains("sets core.fsmonitor"), "{err}");
        assert!(host_git_new(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
