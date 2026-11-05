//! A sandbox folder's repo moved as a git bundle (docs/architecture/daemon/fleet.md,
//! *Moving code*; fleet plan decision 12): the hub fetches the folder's
//! `origin` into its own bare repo cache with its own credentials and
//! bundles one branch from there, `--not` what the worker has; the worker
//! takes the bundle into its own cache for that repo ([`unbundle`],
//! [`set_refs`]) and cuts a checkout with no remote from it
//! ([`land_from`]), never needing credentials. All git runs through
//! `commands::run`'s `host_git` family
//! (docs/decisions/0003-host-git-never-runs-repo-controlled-code.md):
//! hooks and fsmonitor off, the repo's config checked first. Every git child
//! is bounded ([`run`], [`optional`]: `runtime::bounded`, a timeout and a
//! stdout cap).
//!
//! The fleet step 12 plumbing (fleet plan decisions 71, 73) sits here too:
//! a repo's [`identity`], the unborn repo's [`ensure_root`], bundles of refs
//! `--not` the receiver's haves ([`bundle_refs`]) and the [`tips`] a repo
//! answers as its haves; the WIP snapshot is [`super::wip`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::commands::run::{host_git, host_git_bare, host_git_new};
use crate::runtime::bounded;

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
/// by root commits, which needs no URL, is fleet step 12.)
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
        run(host_git_new(cache)?.args(["init", "-q", "--bare"]), QUICK)?;
    }
    run(
        host_git_bare(cache)?
            .args(["-c", "protocol.ext.allow=never", "fetch", "-q", "--no-tags", "--prune", "--"])
            .arg(origin)
            .arg("+refs/heads/*:refs/heads/*"),
        WHOLE,
    )
    .with_context(|| format!("cannot fetch {origin}"))?;
    Ok(())
}

/// `branch` is in `cache`.
pub fn has_branch(cache: &Path, branch: &str) -> Result<bool> {
    check_branch(branch)?;
    Ok(commit_at(Repo::Bare(cache), &format!("refs/heads/{branch}"))?.is_some())
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

/// A repo identity ([`identity`]) as caches are named by it: 16 lowercase
/// hex.
pub fn check_id(id: &str) -> Result<()> {
    if id.len() != 16 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        bail!("`{id}` isn't a repo identity");
    }
    Ok(())
}

/// The WIP ref a branch's snapshot travels on (fleet plan decision 73),
/// in a bundle and in the receiver's cache: `refs/devsandbox/wip/<branch>`.
pub fn wip_ref(branch: &str) -> String {
    format!("refs/devsandbox/wip/{branch}")
}

/// Make the bare repo cache at `cache` (an existing, real dir the caller
/// made) if it isn't one yet.
pub fn ensure_bare(cache: &Path) -> Result<()> {
    if !cache.join("HEAD").exists() {
        run(host_git_new(cache)?.args(["init", "-q", "--bare"]), QUICK)
            .with_context(|| format!("cannot make the repo cache {}", cache.display()))?;
    }
    Ok(())
}

/// The objects of the bundle at `bundle` into the bare repo `cache`, no
/// ref touched (`bundle unbundle`, refs are [`set_refs`]'). A bundle
/// whose prerequisites `cache` lacks fails here, with git's why.
pub fn unbundle(cache: &Path, bundle: &Path) -> Result<()> {
    run(host_git_bare(cache)?.args(["bundle", "unbundle"]).arg(bundle), WHOLE).context("cannot take the bundle")?;
    Ok(())
}

/// Set each of `refs` (full name → commit id, both checked) in the bare
/// repo `cache`, forced: the sender's copy wins in its cache. Every commit
/// must be one `cache` has, checked before any ref moves.
pub fn set_refs(cache: &Path, refs: &std::collections::BTreeMap<String, String>) -> Result<()> {
    for (name, id) in refs {
        check_ref(name)?;
        check_oid(id)?;
        if commit_at(Repo::Bare(cache), id)?.as_deref() != Some(id.as_str()) {
            bail!("{name}'s commit {id} is neither in the bundle nor already here");
        }
    }
    for (name, id) in refs {
        run(host_git_bare(cache)?.args(["update-ref", name, id]), QUICK).with_context(|| format!("cannot set {name}"))?;
    }
    Ok(())
}

/// Cut the checkout at `at` from the bare repo `cache`: its `branch`
/// (`refs/heads/<branch>` there). A new checkout (no `.git`; `at` made
/// with its parents if missing, refused when it exists non-empty) gets the
/// branch checked out and, with `wip` (a commit `cache` has, the branch's
/// snapshot), that snapshot's tree applied as uncommitted changes
/// (`wip::apply_wip`; the ref fetched for it is dropped after). An
/// existing one (a checkout a transfer made before; an instance may be
/// using it) only gets the branch moved: fast-forwarded when it's the one
/// checked out, else set to the cache's; `wip` is left out (decision 73).
/// The checkout has no remote.
pub fn land_from(cache: &Path, at: &Path, branch: &str, wip: Option<&str>) -> Result<()> {
    check_branch(branch)?;
    if let Some(w) = wip {
        check_oid(w)?;
    }
    let fresh = !at.join(".git").exists();
    if fresh {
        std::fs::create_dir_all(at).with_context(|| format!("cannot create {}", at.display()))?;
        if std::fs::read_dir(at)?.next().is_some() {
            bail!("{} exists and isn't a git checkout", at.display());
        }
        run(host_git_new(at)?.args(["init", "-q"]), QUICK)?;
    }
    let head = optional(host_git(at)?.args(["symbolic-ref", "-q", "HEAD"]))?;
    let full = format!("refs/heads/{branch}");
    let fetch = |refspecs: &[String]| -> Result<()> {
        run(host_git(at)?.args(["fetch", "-q", "--no-tags", "--update-head-ok"]).arg(cache).args(refspecs), WHOLE)
            .with_context(|| format!("cannot fetch {branch} from the repo cache"))?;
        Ok(())
    };
    if fresh {
        run(host_git(at)?.args(["symbolic-ref", "HEAD"]).arg(&full), QUICK)?;
        let wip_at = wip.map(|_| wip_ref(branch));
        let mut refspecs = vec![format!("+{full}:{full}")];
        refspecs.extend(wip_at.iter().map(|w| format!("+{w}:{w}")));
        fetch(&refspecs)?;
        run(host_git(at)?.args(["reset", "-q", "--hard"]).env("GIT_LFS_SKIP_SMUDGE", "1"), WHOLE)?;
        if let (Some(w), Some(wip_at)) = (wip, wip_at) {
            super::wip::apply_wip(at, w)?;
            run(host_git(at)?.args(["update-ref", "-d", &wip_at]), QUICK)?;
        }
    } else if head.as_deref() == Some(full.as_str()) {
        fetch(std::slice::from_ref(&full))?;
        run(host_git(at)?.args(["merge", "-q", "--ff-only", "FETCH_HEAD"]).env("GIT_LFS_SKIP_SMUDGE", "1"), WHOLE)
            .with_context(|| format!("cannot fast-forward {branch} in {}", at.display()))?;
    } else {
        fetch(&[format!("+{full}:{full}")])?;
    }
    Ok(())
}

/// Which repo a git call is for: a checkout ([`host_git`]) or a bare repo
/// devsandbox keeps, as a repo cache ([`host_git_bare`]).
#[derive(Debug, Clone, Copy)]
pub enum Repo<'a> {
    Checkout(&'a Path),
    Bare(&'a Path),
}

impl Repo<'_> {
    fn git(&self) -> Result<Command> {
        match self {
            Repo::Checkout(p) => host_git(p),
            Repo::Bare(p) => host_git_bare(p),
        }
    }

    /// Its git dir, absolute (a linked worktree's own, not the common one).
    pub(super) fn git_dir(&self) -> Result<PathBuf> {
        match self {
            Repo::Bare(p) => Ok(p.to_path_buf()),
            Repo::Checkout(p) => {
                let dir = run(host_git(p)?.args(["rev-parse", "--absolute-git-dir"]), QUICK)?;
                Ok(PathBuf::from(dir.trim()))
            }
        }
    }
}

/// What devsandbox's own commits (the root, WIP snapshots) say made them:
/// fixed, so they don't carry the host user's name and don't depend on a
/// configured identity.
pub(super) const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "devsandbox"),
    ("GIT_AUTHOR_EMAIL", "devsandbox@localhost"),
    ("GIT_COMMITTER_NAME", "devsandbox"),
    ("GIT_COMMITTER_EMAIL", "devsandbox@localhost"),
];

/// The ref an unborn repo's devsandbox root commit lives on (decision 71).
pub const ROOT_REF: &str = "refs/devsandbox/root";

/// At most this many haves are considered, and [`tips`] answers at most
/// this many: a receiver's haves only make a bundle smaller, so the cap
/// trades a bigger bundle for a bounded argv and query.
pub const HAVES_CAP: usize = 256;

/// A repo's identity (fleet plan decision 71): the first 16 hex of the
/// sha256 over the sorted root commit ids of `rev`'s history, each followed
/// by `\n` (as `rev-list` prints them). The same in every clone, no remote
/// needed. A merge of an unrelated history adds a root and so changes it.
/// `rev` is `HEAD`, a full ref or a commit id ([`check_rev`]). Not a trust
/// boundary: it names caches.
pub fn identity(repo: Repo, rev: &str) -> Result<String> {
    check_rev(rev)?;
    let out = run(repo.git()?.args(["rev-list", "--max-parents=0"]).arg(rev).arg("--"), WHOLE)
        .with_context(|| format!("cannot list the root commits of {rev}"))?;
    let mut roots: Vec<&str> = out.lines().collect();
    if roots.is_empty() {
        bail!("{rev} has no root commit");
    }
    roots.sort_unstable();
    let text: String = roots.iter().map(|r| format!("{r}\n")).collect();
    Ok(super::short_hash(text.as_bytes()))
}

/// For a checkout whose `HEAD` has no commit yet (a fresh `git init`):
/// [`ROOT_REF`], one empty-tree commit made once with devsandbox's
/// [`IDENTITY`], the time and a nonce in its message, so two empty repos
/// never share an identity; the snapshot starts there. `None` when `HEAD`
/// has a commit (the history is its own root).
#[cfg_attr(not(test), allow(dead_code))]
pub fn ensure_root(checkout: &Path) -> Result<Option<String>> {
    if head_commit(checkout)?.is_some() {
        return Ok(None);
    }
    if let Some(root) = commit_at(Repo::Checkout(checkout), ROOT_REF)? {
        return Ok(Some(root));
    }
    // `mktree` with no input (stdin is null) is the empty tree, written.
    let tree = run(host_git(checkout)?.arg("mktree"), QUICK)?.trim().to_string();
    let commit = run(
        host_git(checkout)?
            .args(["-c", "commit.gpgsign=false", "commit-tree"])
            .arg(&tree)
            .arg("-m")
            .arg(format!("devsandbox root {}", nonce()))
            .envs(IDENTITY),
        QUICK,
    )?
    .trim()
    .to_string();
    check_oid(&commit)?;
    // The empty old value: create only. Another caller winning the race keeps its root.
    let made = run(host_git(checkout)?.args(["update-ref", ROOT_REF, &commit, ""]), QUICK);
    match (made, commit_at(Repo::Checkout(checkout), ROOT_REF)?) {
        (_, Some(root)) => Ok(Some(root)),
        (Err(e), None) => Err(e).with_context(|| format!("cannot make {ROOT_REF}")),
        (Ok(_), None) => bail!("{ROOT_REF} vanished after it was made"),
    }
}

/// `HEAD`'s commit, `None` when it has none yet.
pub(super) fn head_commit(checkout: &Path) -> Result<Option<String>> {
    commit_at(Repo::Checkout(checkout), "HEAD")
}

/// The commit `rev` names in `repo`, `None` when there's none.
pub(super) fn commit_at(repo: Repo, rev: &str) -> Result<Option<String>> {
    optional(repo.git()?.args(["rev-parse", "-q", "--verify"]).arg(format!("{rev}^{{commit}}")))
}

/// Bundle `refs` (full ref names, [`check_ref`]) of `repo` at `out`, `--not`
/// those of `haves` (commit ids, [`check_oid`]; at most [`HAVES_CAP`]
/// considered) that name commits `repo` has: unknown ones are dropped, so a
/// receiver's haves from elsewhere never fail the bundle. `false` and no
/// file when the receiver has everything already.
pub fn bundle_refs(repo: Repo, refs: &[&str], haves: &[String], out: &Path) -> Result<bool> {
    if refs.is_empty() {
        bail!("nothing to bundle");
    }
    for r in refs {
        check_ref(r)?;
    }
    let haves = known_commits(repo, &haves[..haves.len().min(HAVES_CAP)])?;
    let count = run(repo.git()?.args(["rev-list", "--count"]).args(refs).arg("--not").args(&haves).arg("--"), WHOLE)
        .context("cannot count what to bundle")?;
    if count.trim() == "0" {
        return Ok(false);
    }
    run(repo.git()?.args(["bundle", "create", "-q"]).arg(out).args(refs).arg("--not").args(&haves), WHOLE)
        .with_context(|| format!("cannot bundle {}", refs.join(" ")))?;
    Ok(true)
}

/// The ones of `haves` that are commits in `repo`, in order, deduped: one
/// `cat-file --batch-check` reading them from a temp file in its git dir
/// (stdin from a file, so the bounded runner needs no writer).
fn known_commits(repo: Repo, haves: &[String]) -> Result<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let haves: Vec<&String> = haves.iter().filter(|h| seen.insert(h.as_str())).collect();
    for h in &haves {
        check_oid(h)?;
    }
    if haves.is_empty() {
        return Ok(Vec::new());
    }
    let list = TempPath::new(&repo.git_dir()?, "haves");
    let text: String = haves.iter().map(|h| format!("{h}\n")).collect();
    std::fs::write(&list.0, text).with_context(|| format!("cannot write {}", list.0.display()))?;
    let input = std::fs::File::open(&list.0).with_context(|| format!("cannot read {}", list.0.display()))?;
    let out = run(
        repo.git()?.args(["cat-file", "--batch-check=%(objectname) %(objecttype)"]).stdin(Stdio::from(input)),
        QUICK,
    )?;
    let known: std::collections::BTreeSet<&str> =
        out.lines().filter_map(|l| l.strip_suffix(" commit")).collect();
    Ok(haves.into_iter().filter(|h| known.contains(h.as_str())).cloned().collect())
}

/// The commits `repo`'s refs point at (tags and other non-commits left
/// out), deduped, the most recently committed first, at most
/// [`HAVES_CAP`]: what a receiver answers as its haves.
pub fn tips(repo: Repo) -> Result<Vec<String>> {
    let out = run(
        repo.git()?.args(["for-each-ref", "--sort=-committerdate", "--format=%(objecttype) %(objectname)"]),
        QUICK,
    )?;
    let mut seen = std::collections::BTreeSet::new();
    Ok(out
        .lines()
        .filter_map(|l| l.strip_prefix("commit "))
        .filter(|id| seen.insert(*id))
        .take(HAVES_CAP)
        .map(str::to_string)
        .collect())
}

/// A full commit id: 40 (sha1) or 64 (sha256) lowercase hex.
pub fn check_oid(id: &str) -> Result<()> {
    if !matches!(id.len(), 40 | 64) || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        bail!("`{id}` isn't a commit id");
    }
    Ok(())
}

/// A full ref name devsandbox passes to git: `refs/` then a name
/// [`check_branch`] accepts.
pub fn check_ref(name: &str) -> Result<()> {
    match name.strip_prefix("refs/") {
        Some(rest) if check_branch(rest).is_ok() => Ok(()),
        _ => bail!("`{name}` isn't a ref devsandbox moves"),
    }
}

/// A revision devsandbox names: `HEAD`, a full ref ([`check_ref`]) or a
/// commit id ([`check_oid`]).
pub fn check_rev(rev: &str) -> Result<()> {
    if rev == "HEAD" || check_ref(rev).is_ok() || check_oid(rev).is_ok() {
        return Ok(());
    }
    bail!("`{rev}` isn't a revision devsandbox names")
}

/// Unique enough for a temp file name or a root commit's message: the time
/// in ns, the pid and a per-process counter.
pub(super) fn nonce() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let ns = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{ns:x}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A uniquely named file in `dir`, removed (with git's `.lock` beside it) on
/// drop, errors included.
pub(super) struct TempPath(pub PathBuf);

impl TempPath {
    pub(super) fn new(dir: &Path, tag: &str) -> Self {
        TempPath(dir.join(format!("devsandbox-{tag}-{}", nonce())))
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// Queries and local plumbing (`rev-parse`, `update-ref`, `commit-tree`,
/// `for-each-ref`, …): quick on any repo, so a minute means git is stuck.
pub(super) const QUICK: Duration = Duration::from_secs(60);

/// Work over a whole repo or tree (`fetch`, `bundle create`, `rev-list`
/// of a history, `add -A`, a checkout's `reset --hard`): a big repo takes
/// minutes, but never this long unless stuck.
pub(super) const WHOLE: Duration = Duration::from_secs(30 * 60);

/// Stdout kept from a git child. Bundles go to a file named on the argv,
/// so only queries print; more than this is an error, never a silent cut.
const STDOUT_CAP: usize = 16 * 1024 * 1024;

/// `cmd` bounded: at most `timeout`, stdout at most [`STDOUT_CAP`].
fn bounded_git(cmd: &mut Command, timeout: Duration) -> Result<bounded::Output> {
    use std::os::unix::process::CommandExt;
    // Its own group, so a timeout kills git's children (`pack-objects`,
    // `index-pack`) with it.
    cmd.process_group(0);
    let out = bounded::run(cmd, STDOUT_CAP, timeout).map_err(|e| match e {
        bounded::Error::TimedOut(d) => anyhow::anyhow!("git timed out after {}", bounded::human(d)),
        bounded::Error::Io(e) => anyhow::Error::new(e).context("cannot run git"),
    })?;
    if out.truncated {
        bail!("git printed more than {} MiB", STDOUT_CAP >> 20);
    }
    Ok(out)
}

/// Run `cmd` bounded by `timeout`; its stdout, or an error with its stderr.
pub(super) fn run(cmd: &mut Command, timeout: Duration) -> Result<String> {
    let out = bounded_git(cmd, timeout)?;
    if !out.status.success() {
        bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `cmd` (a query, [`QUICK`]); its trimmed stdout, `None` when it exits
/// 1 or 2 with nothing on stdout (git's "no such thing" for these queries).
pub(super) fn optional(cmd: &mut Command) -> Result<Option<String>> {
    let out = bounded_git(cmd, QUICK)?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match out.status.code() {
        Some(0) if !text.is_empty() => Ok(Some(text)),
        Some(0 | 1 | 2) if text.is_empty() => Ok(None),
        _ => bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-code-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// Plain git on a test's own temp repos, bounded like the module's.
    pub fn git(dir: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null());
        let out = bounded_git(&mut cmd, QUICK).unwrap();
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

        // The worker's cache, and a checkout cut from it.
        let worker = dir.join("worker/c.git");
        std::fs::create_dir_all(&worker).unwrap();
        ensure_bare(&worker).unwrap();
        ensure_bare(&worker).unwrap();
        let send = |branch: &str, name: &str| {
            let full = format!("refs/heads/{branch}");
            let file = dir.join(name);
            if bundle_refs(Repo::Bare(&cache), &[&full], &tips(Repo::Bare(&worker)).unwrap(), &file).unwrap() {
                unbundle(&worker, &file).unwrap();
            }
            let id = commit_at(Repo::Bare(&cache), &full).unwrap().unwrap();
            set_refs(&worker, &[(full, id)].into()).unwrap();
        };
        send("feature", "f.bundle");
        let at = dir.join("worker/web");
        land_from(&worker, &at, "feature", None).unwrap();
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
        send("feature", "f2.bundle");
        land_from(&worker, &at, "feature", None).unwrap();
        assert_eq!(std::fs::read_to_string(at.join("F")).unwrap(), "feature 2\n");
        // Another branch into it: updated, not checked out (`main` is in
        // the worker's cache already: no bundle).
        send("main", "m.bundle");
        assert!(!dir.join("m.bundle").exists());
        land_from(&worker, &at, "main", None).unwrap();
        assert_eq!(git(&at, &["symbolic-ref", "--short", "HEAD"]), "feature");
        assert_eq!(git(&at, &["rev-parse", "main"]), git(&origin, &["rev-parse", "main"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_refuses_bad_bundles_and_missing_commits_and_land_a_dir_that_isnt_a_checkout() {
        let dir = scratch("land-refuses");
        let cache = dir.join("c.git");
        std::fs::create_dir_all(&cache).unwrap();
        ensure_bare(&cache).unwrap();
        std::fs::write(dir.join("junk.bundle"), "not a bundle").unwrap();
        let err = format!("{:#}", unbundle(&cache, &dir.join("junk.bundle")).unwrap_err());
        assert!(err.contains("cannot take the bundle"), "{err}");
        // A delta whose prerequisite the cache lacks.
        let a = one_commit(&dir, "a", "one\n");
        let base = git(&a, &["rev-parse", "HEAD"]);
        std::fs::write(a.join("f"), "two\n").unwrap();
        git(&a, &["commit", "-q", "-am", "two"]);
        let delta = dir.join("d.bundle");
        assert!(bundle_refs(Repo::Checkout(&a), &["refs/heads/main"], &[base], &delta).unwrap());
        assert!(unbundle(&cache, &delta).is_err());
        // A ref naming a commit the cache doesn't have: no ref moves.
        let head = git(&a, &["rev-parse", "HEAD"]);
        let err = format!("{:#}", set_refs(&cache, &[("refs/heads/main".to_string(), head)].into()).unwrap_err());
        assert!(err.contains("is neither in the bundle nor already here"), "{err}");
        assert_eq!(tips(Repo::Bare(&cache)).unwrap(), Vec::<String>::new());

        std::fs::create_dir_all(dir.join("full")).unwrap();
        std::fs::write(dir.join("full/x"), "x").unwrap();
        let err = format!("{:#}", land_from(&cache, &dir.join("full"), "main", None).unwrap_err());
        assert!(err.contains("isn't a git checkout"), "{err}");
        let err = format!("{:#}", land_from(&cache, &dir.join("empty"), "main", None).unwrap_err());
        assert!(err.contains("cannot fetch main from the repo cache"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_are_checked() {
        check_id("0123456789abcdef").unwrap();
        for bad in ["", "0123456789abcde", "0123456789ABCDEF", "0123456789abcdefa", "../../../../etc/x"] {
            assert!(check_id(bad).is_err(), "{bad:?}");
        }
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

    /// A repo at `<dir>/<name>` on `main` with one commit of `f` = `body`.
    fn one_commit(dir: &Path, name: &str, body: &str) -> PathBuf {
        let at = dir.join(name);
        std::fs::create_dir_all(&at).unwrap();
        git(&at, &["init", "-q", "-b", "main"]);
        std::fs::write(at.join("f"), body).unwrap();
        git(&at, &["add", "f"]);
        git(&at, &["commit", "-q", "-m", body]);
        at
    }

    #[test]
    fn identity_is_the_roots_across_clones() {
        let dir = scratch("identity");
        let a = one_commit(&dir, "a", "a\n");
        std::fs::write(a.join("f"), "a2\n").unwrap();
        git(&a, &["commit", "-q", "-am", "two"]);
        let id = identity(Repo::Checkout(&a), "HEAD").unwrap();
        assert_eq!(id.len(), 16);
        let root = git(&a, &["rev-list", "--max-parents=0", "HEAD"]);
        assert_eq!(id, crate::serve::fleet::code::short_hash(format!("{root}\n").as_bytes()));
        assert_eq!(identity(Repo::Checkout(&a), "refs/heads/main").unwrap(), id);
        assert_eq!(identity(Repo::Checkout(&a), &git(&a, &["rev-parse", "HEAD~1"])).unwrap(), id);

        git(&dir, &["clone", "-q", a.to_str().unwrap(), "clone"]);
        assert_eq!(identity(Repo::Checkout(&dir.join("clone")), "HEAD").unwrap(), id);
        git(&dir, &["clone", "-q", "--bare", a.to_str().unwrap(), "bare.git"]);
        assert_eq!(identity(Repo::Bare(&dir.join("bare.git")), "refs/heads/main").unwrap(), id);

        let b = one_commit(&dir, "b", "b\n");
        assert_ne!(identity(Repo::Checkout(&b), "HEAD").unwrap(), id);
        // Merging an unrelated history adds a root: a new identity.
        git(&a, &["fetch", "-q", b.to_str().unwrap(), "main:b"]);
        git(&a, &["merge", "-q", "--allow-unrelated-histories", "-X", "ours", "-m", "m", "b"]);
        assert_ne!(identity(Repo::Checkout(&a), "HEAD").unwrap(), id);

        assert!(identity(Repo::Checkout(&a), "--all").is_err());
        assert!(identity(Repo::Checkout(&a), "main").is_err(), "full refs only");
        let e = dir.join("empty");
        std::fs::create_dir_all(&e).unwrap();
        git(&e, &["init", "-q"]);
        assert!(identity(Repo::Checkout(&e), "HEAD").is_err());
        assert_eq!(ensure_root(&a).unwrap(), None, "a repo with commits needs no root");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundles_carry_only_what_the_haves_lack() {
        let dir = scratch("bundle-haves");
        let a = one_commit(&dir, "a", "one\n");
        let first = dir.join("1.bundle");
        assert!(bundle_refs(Repo::Checkout(&a), &["refs/heads/main"], &[], &first).unwrap());
        let b = dir.join("b.git");
        std::fs::create_dir_all(&b).unwrap();
        git(&b, &["init", "-q", "--bare"]);
        git(&b, &["fetch", "-q", first.to_str().unwrap(), "refs/heads/main:refs/heads/main"]);

        std::fs::write(a.join("f"), "two\n").unwrap();
        git(&a, &["commit", "-q", "-am", "two"]);
        let mut haves = tips(Repo::Bare(&b)).unwrap();
        assert_eq!(haves, [git(&a, &["rev-parse", "HEAD~1"])]);
        // Unknown ones are dropped, repeats ignored.
        haves.insert(0, "0".repeat(40));
        haves.push(haves[1].clone());
        let delta = dir.join("2.bundle");
        assert!(bundle_refs(Repo::Checkout(&a), &["refs/heads/main"], &haves, &delta).unwrap());
        // Its prerequisite is the have: verifies in B, not in an empty repo.
        git(&b, &["bundle", "verify", "-q", delta.to_str().unwrap()]);
        let empty = dir.join("e.git");
        std::fs::create_dir_all(&empty).unwrap();
        git(&empty, &["init", "-q", "--bare"]);
        let mut verify = host_git_bare(&empty).unwrap();
        verify.args(["bundle", "verify", "-q"]).arg(&delta);
        assert!(!bounded_git(&mut verify, QUICK).unwrap().status.success());
        let bytes = std::fs::read(&delta).unwrap();
        let header = String::from_utf8_lossy(&bytes[..bytes.windows(2).position(|w| w == b"\n\n").unwrap()]).into_owned();
        assert!(header.lines().any(|l| l.starts_with(&format!("-{}", haves[1]))), "{header}");
        assert!(git(&b, &["bundle", "list-heads", delta.to_str().unwrap()]).ends_with(" refs/heads/main"));
        git(&b, &["fetch", "-q", delta.to_str().unwrap(), "refs/heads/main:refs/heads/main"]);

        // Nothing new: no bundle.
        let none = dir.join("3.bundle");
        assert!(!bundle_refs(Repo::Checkout(&a), &["refs/heads/main"], &tips(Repo::Bare(&b)).unwrap(), &none).unwrap());
        assert!(!none.exists());

        for bad in ["-x", "abc", &"A".repeat(40), &"0".repeat(41), "HEAD"] {
            assert!(bundle_refs(Repo::Checkout(&a), &["refs/heads/main"], &[bad.to_string()], &none).is_err(), "{bad}");
        }
        for bad in ["main", "-x", "refs/../x", "refs/", "HEAD"] {
            assert!(bundle_refs(Repo::Checkout(&a), &[bad], &[], &none).is_err(), "{bad}");
        }
        assert!(!none.exists());
        assert!(leftovers(&a.join(".git")).is_empty(), "the haves list is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn leftovers(gitdir: &Path) -> Vec<String> {
        std::fs::read_dir(gitdir)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .filter(|n| n.starts_with("devsandbox-"))
            .collect()
    }

    #[test]
    fn tips_are_commits_capped() {
        let dir = scratch("tips");
        let r = dir.join("r.git");
        std::fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q", "--bare"]);
        // 300 root commits on their own branches, one also tagged
        // (annotated: a tag object, left out) and one ref repeating a tip.
        let mut stream = String::new();
        for i in 0..300 {
            let msg = format!("c{i}");
            stream.push_str(&format!("commit refs/heads/b{i}\ncommitter t <t@t> {} +0000\ndata {}\n{msg}\n", 1_700_000_000 + i, msg.len()));
        }
        stream.push_str("reset refs/heads/same\nfrom refs/heads/b0\n\n");
        stream.push_str("tag v1\nfrom refs/heads/b0\ntagger t <t@t> 1700000000 +0000\ndata 1\nx\n");
        let file = dir.join("stream");
        std::fs::write(&file, stream).unwrap();
        let mut cmd = host_git_bare(&r).unwrap();
        cmd.args(["fast-import", "--quiet"]).stdin(Stdio::from(std::fs::File::open(&file).unwrap()));
        let out = bounded_git(&mut cmd, QUICK).unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

        let got = tips(Repo::Bare(&r)).unwrap();
        assert_eq!(got.len(), HAVES_CAP);
        assert_eq!(got[0], git(&r, &["rev-parse", "refs/heads/b299"]), "newest first");
        assert_eq!(got.iter().collect::<std::collections::BTreeSet<_>>().len(), got.len(), "deduped");
        assert!(!got.contains(&git(&r, &["rev-parse", "refs/tags/v1"])), "tag objects left out");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revs_and_oids_are_checked() {
        for ok in ["HEAD", "refs/heads/main", "refs/devsandbox/root", &"a".repeat(40), &"0".repeat(64)] {
            check_rev(ok).unwrap();
        }
        for bad in ["", "main", "-x", "--all", "HEAD~1", "refs/heads/a..b", &"a".repeat(39), &"g".repeat(40), "refs/heads/x y"] {
            assert!(check_rev(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_stuck_git_child_is_killed() {
        let start = std::time::Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec sleep 30"]).stdin(Stdio::null());
        let err = run(&mut cmd, Duration::from_millis(200)).unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(10));
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
