//! Git worktrees for repeat instances of one repo, so working trees are never
//! shared.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::git::{check_repo, host_git};

/// Bind mount for the base repo's `.git` at the identical host path, so a
/// worktree's absolute `gitdir` pointer resolves inside the container.
pub(super) fn git_companion_mount(base: &Path) -> String {
    let git = base.join(".git");
    format!("{}:{}", git.display(), git.display())
}

/// Where a worktree's branch comes from, decided by [`branch_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchSource {
    /// `refs/heads/<branch>` exists: check it out as is.
    Local,
    /// Only `refs/remotes/origin/<branch>` exists (e.g. a PR head): create a
    /// local branch tracking it, so a plain `git push` updates the remote.
    Remote,
    /// Neither: a fresh branch off the start point.
    New,
}

/// A local branch wins over the remote one (it may hold unpushed work).
fn branch_source(local: bool, remote: bool) -> BranchSource {
    match (local, remote) {
        (true, _) => BranchSource::Local,
        (false, true) => BranchSource::Remote,
        (false, false) => BranchSource::New,
    }
}

/// Create a git worktree on `branch`, reusing it when it exists (see
/// [`BranchSource`]). Returns whether the branch was created (`rm` only offers
/// to delete those). Git refuses a branch checked out in another worktree or
/// the base checkout; that is reported with where it is checked out, and which
/// instance owns it when `state` knows.
///
/// A new branch starts from `base_ref` when given, else the remote's default
/// branch (see [`worktree_start_point`]), not the base repo's HEAD: another
/// agent may be mid-work on a feature branch in the base checkout.
///
/// `explicit_branch` (the user or a dispatcher named the branch) gates a
/// targeted `git fetch` of that one branch when the regular fetch didn't bring
/// it: pattern-generated names (`sandbox/${instance}`) are new by
/// construction, so they never pay for the extra round-trip.
pub(super) fn create_worktree(
    base: &Path,
    worktree: &Path,
    branch: &str,
    base_ref: Option<&str>,
    explicit_branch: bool,
    state: &crate::state::State,
) -> Result<bool> {
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let has_ref = |r: &str| git_query(base, &["show-ref", "--verify", "--quiet", r]).is_some();
    let local_ref = format!("refs/heads/{branch}");
    let remote_ref = format!("refs/remotes/origin/{branch}");
    let wt = worktree.to_string_lossy();
    // An existing local branch needs no start point, so no fetch either.
    let source = if has_ref(&local_ref) {
        BranchSource::Local
    } else {
        // Refreshes `origin` (throttled) before the remote-tracking check too.
        let start = worktree_start_point(base, base_ref)?;
        if !has_ref(&remote_ref) && explicit_branch {
            fetch_origin_branch(base, branch);
        }
        match branch_source(false, has_ref(&remote_ref)) {
            BranchSource::New => {
                create_new_branch_worktree(base, worktree, branch, base_ref, &start)?;
                return Ok(true);
            }
            other => other,
        }
    };
    if let Some(r) = base_ref {
        eprintln!("note: `{branch}` already exists; ignoring worktree base `{r}`");
    }
    if source == BranchSource::Local {
        if let Some(at) = checked_out_at(base, branch) {
            let owner = instance_at(state, &at)
                .map(|i| format!(" (instance `{i}`)"))
                .unwrap_or_default();
            bail!(
                "branch `{branch}` is already checked out at `{}`{owner}; git allows a branch \
                 in only one worktree: switch that checkout to another branch, or pick another `--branch`",
                at.display()
            );
        }
    }
    let upstream = format!("origin/{branch}");
    let mut add = host_git(base)?;
    add.args(["worktree", "add"]);
    match source {
        BranchSource::Local => add.args([&*wt, branch]),
        _ => add.args(["--track", "-b", branch, &*wt, upstream.as_str()]),
    };
    let out = add.output().context("failed to run git (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "git worktree add failed for `{}`: {}",
            worktree.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    ensure_populated(worktree)?;
    Ok(false)
}

/// Today's path for a branch that exists nowhere: `-b <branch> --no-track`
/// off `start`, after checking `start` is a commit.
fn create_new_branch_worktree(
    base: &Path,
    worktree: &Path,
    branch: &str,
    base_ref: Option<&str>,
    start: &str,
) -> Result<()> {
    // With an unborn HEAD, `git worktree add -b` infers `--orphan`, exits 0, and
    // lays down a worktree with no files; guard against handing that empty path
    // to the container as a bind mount.
    let head = host_git(base)?
        .args([
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
    let status = host_git(base)?
        .args([
            "worktree",
            "add",
            &worktree.to_string_lossy(),
            "-b",
            branch,
            // Starting from `origin/<default>` would otherwise set it as the
            // upstream, aiming a bare `git push` at main.
            "--no-track",
            start,
        ])
        .status()
        .context("failed to run git (is it installed?)")?;
    if !status.success() {
        bail!("git worktree add failed for `{}`", worktree.display());
    }
    ensure_populated(worktree)
}

/// A real worktree always has a `.git` entry pointing back at the base repo;
/// its absence means git exited 0 without checking anything out.
fn ensure_populated(worktree: &Path) -> Result<()> {
    if !worktree.join(".git").exists() {
        bail!(
            "git worktree add produced an empty worktree at `{}`",
            worktree.display()
        );
    }
    Ok(())
}

/// Best-effort, quiet fetch of one branch into `origin/<branch>`. The explicit
/// refspec also covers single-branch clones, whose configured refspec would
/// leave the remote-tracking ref alone. `--no-write-fetch-head` keeps this
/// narrow fetch from marking the full one fresh (see [`fetch_origin`]). A
/// branch missing on the remote (or no `origin`, or offline) just fails.
fn fetch_origin_branch(base: &Path, branch: &str) {
    if git_query(base, &["remote", "get-url", "origin"]).is_none() {
        return;
    }
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    if let Ok(mut git) = host_git(base) {
        let _ = git
            .args(["fetch", "--quiet", "--no-write-fetch-head", "origin", &refspec])
            .output();
    }
}

/// Where `branch` is checked out among `base`'s worktrees (the base checkout
/// included), if anywhere.
fn checked_out_at(base: &Path, branch: &str) -> Option<PathBuf> {
    let list = git_query(base, &["worktree", "list", "--porcelain"])?;
    worktree_with_branch(&list, branch)
}

/// Parse `git worktree list --porcelain`: blank-line-separated records, each
/// `worktree <path>` then e.g. `branch refs/heads/<name>`.
fn worktree_with_branch(porcelain: &str, branch: &str) -> Option<PathBuf> {
    let want = format!("refs/heads/{branch}");
    porcelain.split("\n\n").find_map(|record| {
        let mut path = None;
        let mut hit = false;
        for line in record.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if line.strip_prefix("branch ") == Some(want.as_str()) {
                hit = true;
            }
        }
        path.filter(|_| hit)
    })
}

/// The instance whose working tree is `path` (a worktree, or a base-folder
/// instance's checkout). Paths compare canonicalized: git prints real paths.
fn instance_at(state: &crate::state::State, path: &Path) -> Option<String> {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let want = canon(path);
    state
        .instances
        .iter()
        .find(|(_, i)| canon(&i.folder) == want)
        .map(|(k, _)| k.clone())
}

/// Seed a fresh worktree with the base repo's gitignored files listed in its
/// `.worktreeinclude` (docs/worktreeinclude.md): a checkout has no untracked
/// files, so `.env` and friends would otherwise be missing. Git does the
/// matching (include patterns, then gitignore), so semantics equal the other
/// tools reading the same file. `patterns` (the sandbox's `worktree-include`)
/// apply after the file, so they can negate it. Best effort: problems warn,
/// never fail `run`.
pub(super) fn copy_worktree_includes(base: &Path, worktree: &Path, patterns: &[String]) {
    let include = base.join(".worktreeinclude");
    let include = include.is_file().then_some(include);
    if include.is_none() && patterns.is_empty() {
        return;
    }
    let paths = match worktree_include_paths(base, include.as_deref(), patterns) {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("warning: cannot apply worktree includes: {e:#}");
            return;
        }
    };
    for rel in paths {
        if let Err(e) = copy_entry(&base.join(&rel), &worktree.join(&rel)) {
            eprintln!("warning: cannot copy `{rel}` into worktree: {e:#}");
        }
    }
}

/// Untracked paths (relative to `base`) matching the include patterns that git
/// also ignores. `ls-files` given only these exclude sources ignores
/// `.gitignore`, so negation behaves as written (command-line `--exclude`
/// outranks `--exclude-from`); `check-ignore` then drops anything not
/// gitignored, which also keeps files under an ignored directory.
fn worktree_include_paths(
    base: &Path,
    include: Option<&Path>,
    patterns: &[String],
) -> Result<Vec<String>> {
    let mut ls = host_git(base)?;
    ls.args(["ls-files", "-z", "--others", "--ignored"]);
    if let Some(include) = include {
        ls.arg(format!("--exclude-from={}", include.display()));
    }
    for p in patterns {
        ls.arg(format!("--exclude={p}"));
    }
    let listed = ls.output().context("failed to run git (is it installed?)")?;
    if !listed.status.success() {
        bail!("git ls-files failed: {}", String::from_utf8_lossy(&listed.stderr).trim());
    }
    if listed.stdout.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = host_git(base)?
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to run git (is it installed?)")?;
    // Write on a thread: a large list could fill the stdout pipe while we
    // are still feeding stdin.
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = listed.stdout;
    let writer = std::thread::spawn(move || std::io::Write::write_all(&mut stdin, &input));
    let out = child.wait_with_output().context("git check-ignore failed")?;
    writer.join().expect("writer thread panicked").context("git check-ignore failed")?;
    // Exit 1 means "nothing ignored", not an error.
    if !out.status.success() && out.status.code() != Some(1) {
        bail!("git check-ignore failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// Copy one file or symlink, never overwriting: an existing destination
/// (dangling symlinks included) is left alone.
fn copy_entry(src: &Path, dst: &Path) -> Result<()> {
    if dst.symlink_metadata().is_ok() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let meta = src.symlink_metadata()?;
    #[cfg(unix)]
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
        return Ok(());
    }
    if meta.is_file() {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// Share `entries` (repo-relative, gitignored paths) live across every
/// instance of a sandbox (docs/worktreeinclude.md): the real file lives in
/// `store`, each of `trees` (base repo first, then the worktree if any) gets
/// an absolute symlink to it, and the caller mounts `store` at its host path
/// so the links resolve in the container. Idempotent (runs on every `run` and
/// `rebuild`). A bad entry is a config error; anything on disk that doesn't
/// fit (tracked path, conflicting file) only warns and is left untouched.
///
/// `store` sits in a directory of stores (`shared-files/`); a link into a
/// sibling store is devsandbox's own, left by the old one-store-per-sandbox
/// layout, so it is migrated rather than reported as a conflict (see
/// [`stale_link_target`]).
pub(super) fn link_shared_files(store: &Path, trees: &[&Path], entries: &[String]) -> Result<()> {
    let Some((base, _)) = trees.split_first() else { return Ok(()) };
    let stores = store.parent().unwrap_or(store);
    // Every check below would silently read as "not gitignored"; say why.
    if let Err(e) = check_repo(base) {
        eprintln!("warning: worktree-link skipped: {e:#}");
        return Ok(());
    }
    std::fs::create_dir_all(store).with_context(|| format!("cannot create {}", store.display()))?;
    // Validate every entry before touching the disk: a config error must not
    // leave half the entries adopted.
    let rels = entries.iter().map(|e| link_entry_path(e)).collect::<Result<Vec<_>>>()?;
    let mut seen = std::collections::BTreeSet::new();
    let expanded = rels.iter().flat_map(|rel| expand_link_glob(rel, &[*base, store]));
    for rel in expanded {
        if !seen.insert(rel.clone()) {
            continue;
        }
        let entry = rel.display();
        // Replacing a tracked (or merely untracked) path with a symlink would
        // show up in `git status`; only gitignored, untracked paths qualify.
        if !git_ignores(base, &rel) {
            eprintln!(
                "warning: worktree-link `{entry}` skipped: not gitignored (or tracked) in `{}`",
                base.display()
            );
            continue;
        }
        let shared = store.join(&rel);
        if let Err(e) = adopt(&base.join(&rel), &shared, stores) {
            eprintln!("warning: worktree-link `{entry}`: {e:#}");
            continue;
        }
        if shared.symlink_metadata().is_err() {
            // Neither the store nor the base has it yet; a later run links it.
            continue;
        }
        for tree in trees {
            match link_into(&tree.join(&rel), &shared, stores) {
                Ok(true) if !git_ignores(tree, &rel) => eprintln!(
                    "warning: worktree-link `{entry}` shows as untracked in `{}`: a `dir/` \
                     gitignore pattern doesn't match a symlink, drop the trailing `/`",
                    tree.display()
                ),
                Ok(_) => {}
                Err(e) => eprintln!("warning: worktree-link `{entry}` in `{}`: {e:#}", tree.display()),
            }
        }
    }
    Ok(())
}

/// Normalize a `worktree-link` entry to a plain relative path: leading `./`
/// and trailing `/` dropped, interior `.` collapsed. Anything that could
/// escape the tree or the store is rejected, as is `**` (globs are one
/// segment only, see [`expand_link_glob`]).
fn link_entry_path(entry: &str) -> Result<PathBuf> {
    use std::path::Component;
    let mut trimmed = entry.trim_end_matches('/');
    while let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest.trim_start_matches('/');
    }
    let mut rel = PathBuf::new();
    for c in Path::new(trimmed).components() {
        match c {
            Component::Normal(n) if n != ".git" && n != "**" => rel.push(n),
            Component::Normal(n) if n == "**" => {
                bail!("invalid worktree-link `{entry}`: `**` is not supported, use `*` per path segment")
            }
            _ => bail!(
                "invalid worktree-link `{entry}`: must be a relative path inside the repo (no `..`, `.git`, or absolute path)"
            ),
        }
    }
    if rel.as_os_str().is_empty() {
        bail!("invalid worktree-link `{entry}`: empty path");
    }
    Ok(rel)
}

/// Expand `*` / `?` in `rel`, one path segment at a time, against what exists
/// under any of `roots` (the base repo and the store, so a file already moved
/// into the store still matches). Only the directories the pattern names are
/// listed, never a recursive walk: this runs on every `run`. Wildcards match
/// dotfiles (as in gitignore) but never `.git`. A literal entry passes through
/// unchanged, existing or not; a glob yields only existing matches.
fn expand_link_glob(rel: &Path, roots: &[&Path]) -> Vec<PathBuf> {
    let mut frontier = vec![PathBuf::new()];
    let segments: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    for (i, seg) in segments.iter().enumerate() {
        if !seg.contains(['*', '?']) {
            frontier.iter_mut().for_each(|p| p.push(seg));
            continue;
        }
        let last = i + 1 == segments.len();
        let mut next = std::collections::BTreeSet::new();
        for prefix in &frontier {
            for root in roots {
                let Ok(dir) = std::fs::read_dir(root.join(prefix)) else { continue };
                for entry in dir.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // Intermediate segments must lead somewhere (`metadata`
                    // follows symlinks, so a linked dir still counts).
                    let descends = last || entry.path().metadata().is_ok_and(|m| m.is_dir());
                    if name != ".git" && descends && wildcard_match(seg, &name) {
                        next.insert(prefix.join(&name));
                    }
                }
            }
        }
        frontier = next.into_iter().collect();
    }
    frontier
}

/// Shell-style match of one path segment: `*` any run of chars, `?` one char,
/// everything else literal.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni) = (0, 0);
    // Backtrack point: pattern index after the last `*`, and the name index it
    // is currently assumed to have consumed up to.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi + 1, ni));
                pi += 1;
            }
            Some(&c) if c == '?' || c == n[ni] => {
                pi += 1;
                ni += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    pi = sp;
                    ni = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Whether git ignores `rel` in `tree` (false for tracked paths: `check-ignore`
/// only reports untracked ones). A missing git, or a repo [`host_git`]
/// refuses, counts as not ignored.
fn git_ignores(tree: &Path, rel: &Path) -> bool {
    host_git(tree).is_ok_and(|mut git| {
        git.args(["check-ignore", "-q", "--"])
            .arg(rel)
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// First sighting: move the base repo's real file/dir into the empty store
/// slot (its symlink is laid down by [`link_into`]). A base link into a
/// sibling store (the old per-sandbox layout) has that store's copy moved
/// over instead. No-op once the store has it, or when the base has nothing
/// (or a foreign symlink) there.
fn adopt(base_path: &Path, shared: &Path, stores: &Path) -> Result<()> {
    if shared.symlink_metadata().is_ok() {
        return Ok(());
    }
    let Ok(meta) = base_path.symlink_metadata() else { return Ok(()) };
    let src = if meta.file_type().is_symlink() {
        match stale_link_target(base_path, shared, stores) {
            Some(target) if target.symlink_metadata().is_ok() => target,
            _ => return Ok(()),
        }
    } else {
        base_path.to_path_buf()
    };
    let meta = src.symlink_metadata()?;
    if let Some(parent) = shared.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(&src, shared).is_ok() {
        return Ok(());
    }
    // rename fails across filesystems (repo and config on different mounts);
    // a file can be copied over, a dir is left for the user to move.
    if !meta.is_file() {
        bail!(
            "cannot move `{}` into `{}`; move it there yourself",
            src.display(),
            shared.display()
        );
    }
    std::fs::copy(&src, shared)?;
    std::fs::remove_file(&src)?;
    Ok(())
}

/// Where `path` links to, when that is another devsandbox store under
/// `stores` rather than `shared`: a link left by the old one-store-per-sandbox
/// layout. Links anywhere else are the user's and never touched.
fn stale_link_target(path: &Path, shared: &Path, stores: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(path).ok()?;
    (target.is_absolute() && target.starts_with(stores) && target != shared).then_some(target)
}

/// Point `path` at `shared` with an absolute symlink. `Ok(true)` when it
/// created the link, `Ok(false)` when the right link already exists. A stale
/// link into another store (see [`stale_link_target`]) is replaced; anything
/// else at `path` (a real file, a foreign symlink) is an error, left alone.
fn link_into(path: &Path, shared: &Path, stores: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Err(_) => {}
        Ok(m) if m.file_type().is_symlink() && std::fs::read_link(path)? == shared => {
            return Ok(false)
        }
        Ok(m) if m.file_type().is_symlink() && stale_link_target(path, shared, stores).is_some() => {
            let old = std::fs::read_link(path)?;
            // Two old stores held diverging copies: the other one's is kept.
            if old.symlink_metadata().is_ok() {
                eprintln!(
                    "warning: `{}` now links to `{}`; its previous copy is left at `{}`",
                    path.display(),
                    shared.display(),
                    old.display()
                );
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => bail!(
            "`{}` already exists and is not a link to `{}`; merge it into the shared copy and delete it",
            path.display(),
            shared.display()
        ),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(shared, path)?;
    #[cfg(not(unix))]
    bail!("symlinks are only supported on unix hosts");
    #[cfg(unix)]
    Ok(true)
}

/// Git's output for `git -C base <args>` on success, trimmed; None on failure.
fn git_query(base: &Path, args: &[&str]) -> Option<String> {
    let out = host_git(base)
        .ok()?
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
    let fetch = host_git(base)?
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
    fn worktree_includes_copy_only_matching_ignored_files() {
        let root = std::env::temp_dir().join(format!("devsandbox-wti-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (base, wt) = (root.join("base"), root.join("wt"));
        std::fs::create_dir_all(&base).unwrap();
        git(&base, &["init", "-q", "-b", "main"]);
        let write = |rel: &str, body: &str| {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        write(".gitignore", ".env*\nsecrets/\n.vscode/\n");
        write(".worktreeinclude", "# secrets\n.env*\n!.env.skip\nsecrets/\nnotes.txt\n.vscode/tasks.json\n");
        // `-f`: a tracked file matching both lists must still not be copied.
        write(".env.example", "tracked");
        git(&base, &["add", ".gitignore", ".worktreeinclude"]);
        git(&base, &["add", "-f", ".env.example"]);
        git(&base, &["commit", "-q", "-m", "init"]);
        write(".env", "base");
        write(".env.local", "local");
        write(".env.skip", "negated");
        write("secrets/x/key", "k");
        write("notes.txt", "untracked, not ignored");
        write(".vscode/tasks.json", "{}");
        write(".vscode/other.json", "{}");
        create_worktree(&base, &wt, "sandbox/i", None, false, &Default::default()).unwrap();
        // Pre-existing destination is never overwritten.
        std::fs::write(wt.join(".env.local"), "mine").unwrap();

        copy_worktree_includes(&base, &wt, &[]);

        let read = |rel: &str| std::fs::read_to_string(wt.join(rel)).ok();
        assert_eq!(read(".env").as_deref(), Some("base"));
        assert_eq!(read(".env.local").as_deref(), Some("mine"));
        assert_eq!(read("secrets/x/key").as_deref(), Some("k"));
        assert_eq!(read(".vscode/tasks.json").as_deref(), Some("{}"));
        assert_eq!(read(".env.skip"), None);
        assert_eq!(read("notes.txt"), None);
        assert_eq!(read(".vscode/other.json"), None);
        assert_eq!(git(&wt, &["status", "--porcelain"]), "");

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A committed repo at `root/base` with `.gitignore` = `ignore`.
    fn repo(root: &Path, ignore: &str) -> std::path::PathBuf {
        let _ = std::fs::remove_dir_all(root);
        let base = root.join("base");
        std::fs::create_dir_all(&base).unwrap();
        git(&base, &["init", "-q", "-b", "main"]);
        std::fs::write(base.join(".gitignore"), ignore).unwrap();
        git(&base, &["add", ".gitignore"]);
        git(&base, &["commit", "-q", "-m", "init"]);
        base
    }

    #[test]
    fn worktree_include_config_patterns_extend_and_negate_file() {
        let root = std::env::temp_dir().join(format!("devsandbox-wtip-{}", std::process::id()));
        let base = repo(&root, ".env*\n");
        let wt = root.join("wt");
        std::fs::write(base.join(".worktreeinclude"), ".env\n.env.local\n").unwrap();
        for f in [".env", ".env.local", ".env.test"] {
            std::fs::write(base.join(f), f).unwrap();
        }
        create_worktree(&base, &wt, "sandbox/p", None, false, &Default::default()).unwrap();

        copy_worktree_includes(&base, &wt, &["!.env.local".into(), ".env.test".into()]);

        assert!(wt.join(".env").is_file());
        assert!(!wt.join(".env.local").exists(), "config negation must win over the file");
        assert!(wt.join(".env.test").is_file());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn link_shared_files_adopts_links_and_guards() {
        let root = std::env::temp_dir().join(format!("devsandbox-wtl-{}", std::process::id()));
        let base = repo(&root, ".env*\nsecrets\n");
        let (wt, store) = (root.join("wt"), root.join("store"));
        std::fs::write(base.join(".env"), "base").unwrap();
        std::fs::create_dir_all(base.join("secrets")).unwrap();
        std::fs::write(base.join("secrets/key"), "k").unwrap();
        std::fs::write(base.join("tracked.txt"), "t").unwrap();
        git(&base, &["add", "tracked.txt"]);
        git(&base, &["commit", "-q", "-m", "t"]);
        create_worktree(&base, &wt, "sandbox/l", None, false, &Default::default()).unwrap();
        // The worktree already holds its own `.env.local`: a conflict to keep.
        std::fs::write(wt.join(".env.local"), "mine").unwrap();
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join(".env.local"), "shared").unwrap();
        let entries: Vec<String> =
            [".env", "secrets/", ".env.local", "tracked.txt", ".env.missing", ".env"]
                .map(String::from)
                .into();

        for _ in 0..2 {
            // Twice: the second pass must be a no-op.
            link_shared_files(&store, &[&base, &wt], &entries).unwrap();
        }

        let link = |p: &Path| std::fs::read_link(p).ok();
        assert_eq!(std::fs::read_to_string(store.join(".env")).unwrap(), "base");
        assert_eq!(link(&base.join(".env")), Some(store.join(".env")));
        assert_eq!(link(&wt.join(".env")), Some(store.join(".env")));
        assert_eq!(link(&wt.join("secrets")), Some(store.join("secrets")));
        assert_eq!(std::fs::read_to_string(wt.join("secrets/key")).unwrap(), "k");
        // Store copy linked into the base; the worktree's own file is kept.
        assert_eq!(link(&base.join(".env.local")), Some(store.join(".env.local")));
        assert_eq!(std::fs::read_to_string(wt.join(".env.local")).unwrap(), "mine");
        // Tracked path untouched, missing everywhere skipped.
        assert!(link(&base.join("tracked.txt")).is_none());
        assert!(!store.join("tracked.txt").exists());
        assert!(base.join(".env.missing").symlink_metadata().is_err());
        // Live: an edit through one tree is seen by the other.
        std::fs::write(wt.join(".env"), "rotated").unwrap();
        assert_eq!(std::fs::read_to_string(base.join(".env")).unwrap(), "rotated");
        assert_eq!(git(&base, &["status", "--porcelain"]), "");
        assert_eq!(git(&wt, &["status", "--porcelain"]), "");

        for bad in ["../x", "/abs", ".git/config", "a/../../x", "", "./", "**/.env", "a/**"] {
            let err = link_shared_files(&store, &[&base], &[bad.into()]).unwrap_err();
            assert!(err.to_string().contains("invalid worktree-link"), "{bad}: {err}");
        }
        // A bad entry anywhere fails before any other entry is adopted.
        std::fs::write(base.join(".env.late"), "x").unwrap();
        assert!(link_shared_files(&store, &[&base], &[".env.late".into(), "../x".into()]).is_err());
        assert!(!store.join(".env.late").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The old layout kept one store per sandbox, so a second sandbox on the
    /// same repo found the base already linked into the first's store and got
    /// nothing. The per-repo store takes over those copies and links.
    #[test]
    fn link_shared_files_migrates_per_sandbox_stores() {
        let root = std::env::temp_dir().join(format!("devsandbox-wtm-{}", std::process::id()));
        let base = repo(&root, ".env*\n");
        let stores = root.join("shared-files");
        let (old_a, old_b, store) = (stores.join("web"), stores.join("web22"), stores.join("repo-1234"));
        let (wt_a, wt_b) = (root.join("wt-a"), root.join("wt-b"));
        create_worktree(&base, &wt_a, "sandbox/a", None, false, &Default::default()).unwrap();
        create_worktree(&base, &wt_b, "sandbox/b", None, false, &Default::default()).unwrap();
        std::fs::create_dir_all(&old_a).unwrap();
        std::fs::create_dir_all(&old_b).unwrap();
        std::fs::write(old_a.join(".env"), "a").unwrap();
        std::fs::write(old_b.join(".env"), "b").unwrap();
        let symlink = |target: &Path, at: &Path| std::os::unix::fs::symlink(target, at).unwrap();
        symlink(&old_a.join(".env"), &base.join(".env"));
        symlink(&old_a.join(".env"), &wt_a.join(".env"));
        // Diverged: a copy only the other old store has.
        symlink(&old_b.join(".env"), &wt_b.join(".env"));
        // A user's own link outside the stores is never touched.
        std::fs::write(root.join("mine"), "m").unwrap();
        symlink(&root.join("mine"), &base.join(".env.local"));
        let entries: Vec<String> = [".env", ".env.local"].map(String::from).into();

        for _ in 0..2 {
            link_shared_files(&store, &[&base, &wt_a, &wt_b], &entries).unwrap();
        }

        let link = |p: &Path| std::fs::read_link(p).ok();
        assert_eq!(std::fs::read_to_string(store.join(".env")).unwrap(), "a");
        assert!(!old_a.join(".env").exists(), "moved, not copied");
        for tree in [&base, &wt_a, &wt_b] {
            assert_eq!(link(&tree.join(".env")), Some(store.join(".env")), "{}", tree.display());
        }
        assert_eq!(std::fs::read_to_string(old_b.join(".env")).unwrap(), "b");
        assert_eq!(link(&base.join(".env.local")), Some(root.join("mine")));
        assert!(!store.join(".env.local").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn link_entry_path_normalizes() {
        for (entry, want) in [
            ("./packages/bolt/.env", "packages/bolt/.env"),
            ("././.env", ".env"),
            (".//x/", "x"),
            ("a/./b", "a/b"),
            ("packages/*/.env", "packages/*/.env"),
        ] {
            assert_eq!(link_entry_path(entry).unwrap(), Path::new(want), "{entry}");
        }
    }

    #[test]
    fn wildcard_matches_one_segment() {
        for (p, n, ok) in [
            ("*", "bolt", true),
            ("*", ".hidden", true),
            ("*", "", true),
            (".env*", ".env.local", true),
            (".env*", ".en", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("*-proxy", "claude-code-proxy", true),
            ("*a*b", "xaxxb", true),
            ("*a*b", "xaxxbc", false),
            ("lit", "lit", true),
            ("lit", "lits", false),
        ] {
            assert_eq!(wildcard_match(p, n), ok, "{p} vs {n}");
        }
    }

    #[test]
    fn link_glob_expands_against_base_and_store() {
        let root = std::env::temp_dir().join(format!("devsandbox-wtg-{}", std::process::id()));
        let base = repo(&root, ".env\nnotadir\n");
        let (wt, store) = (root.join("wt"), root.join("store"));
        for pkg in ["bolt", "proxy", "empty"] {
            std::fs::create_dir_all(base.join("packages").join(pkg)).unwrap();
        }
        std::fs::write(base.join("packages/bolt/.env"), "b").unwrap();
        std::fs::write(base.join("packages/proxy/.env"), "p").unwrap();
        // A plain file where a directory is expected is not descended into.
        std::fs::write(base.join("packages/notadir"), "").unwrap();
        // Only in the store (base copy gone): the glob must still find it.
        std::fs::create_dir_all(store.join("packages/gone")).unwrap();
        std::fs::write(store.join("packages/gone/.env"), "g").unwrap();
        create_worktree(&base, &wt, "sandbox/g", None, false, &Default::default()).unwrap();

        let mut got = expand_link_glob(Path::new("packages/*/.env"), &[&base, &store]);
        got.sort();
        let want: Vec<PathBuf> = ["bolt", "empty", "gone", "proxy"]
            .iter()
            .map(|p| Path::new("packages").join(p).join(".env"))
            .collect();
        assert_eq!(got, want);

        link_shared_files(&store, &[&base, &wt], &["./packages/*/.env".into()]).unwrap();

        let link = |p: PathBuf| std::fs::read_link(p).ok();
        for pkg in ["bolt", "proxy", "gone"] {
            let rel = format!("packages/{pkg}/.env");
            assert_eq!(link(wt.join(&rel)), Some(store.join(&rel)), "{rel}");
        }
        assert_eq!(std::fs::read_to_string(wt.join("packages/gone/.env")).unwrap(), "g");
        // No file anywhere: nothing created, not even an empty store slot.
        assert!(wt.join("packages/empty/.env").symlink_metadata().is_err());
        assert!(!store.join("packages/empty").exists());
        assert_eq!(git(&base, &["status", "--porcelain"]), "");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The security review's PoC: a sandbox plants a hook, or command-running
    /// config, in the shared `.git`; host git must run neither.
    #[cfg(unix)]
    #[test]
    fn create_worktree_runs_no_repo_controlled_code() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("devsandbox-wtsec-{}", std::process::id()));
        let base = repo(&root, "");
        let marker = root.join("pwned");
        let hook = base.join(".git/hooks/post-checkout");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        create_worktree(&base, &root.join("wt"), "sandbox/h", None, false, &Default::default()).unwrap();
        assert!(!marker.exists(), "post-checkout hook ran on the host");

        git(&base, &["config", "core.fsmonitor", &hook.to_string_lossy()]);
        let err = create_worktree(&base, &root.join("wt2"), "sandbox/f", None, false, &Default::default()).unwrap_err().to_string();
        assert!(err.contains("refusing to run git on"), "{err}");
        assert!(err.contains(".git/config sets core.fsmonitor"), "{err}");
        assert!(!marker.exists(), "fsmonitor ran on the host");
        assert!(!root.join("wt2").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn worktree_includes_absent_file_is_noop() {
        let root = std::env::temp_dir().join(format!("devsandbox-wti0-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let wt = root.join("wt");
        std::fs::create_dir_all(&root).unwrap();
        copy_worktree_includes(&root, &wt, &[]);
        assert!(!wt.exists());
        std::fs::remove_dir_all(&root).unwrap();
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

        create_worktree(&base, &wt, "sandbox/x", None, false, &Default::default()).unwrap();

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
        create_worktree(&base, &wt2, "sandbox/y", Some("feature"), false, &Default::default()).unwrap();
        assert_eq!(git(&wt2, &["rev-parse", "HEAD"]), feature_tip);
        let err = create_worktree(&base, &root.join("wt3"), "sandbox/z", Some("nope"), false, &Default::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("worktree base `nope`"), "{err}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn branch_source_prefers_local_then_remote() {
        assert_eq!(branch_source(true, true), BranchSource::Local);
        assert_eq!(branch_source(true, false), BranchSource::Local);
        assert_eq!(branch_source(false, true), BranchSource::Remote);
        assert_eq!(branch_source(false, false), BranchSource::New);
    }

    #[test]
    fn worktree_list_porcelain_finds_branch() {
        let list = "worktree /r\nHEAD 1111\nbranch refs/heads/main\n\n\
                    worktree /w/a\nHEAD 2222\ndetached\n\n\
                    worktree /w/b\nHEAD 3333\nbranch refs/heads/feat/x\n";
        assert_eq!(worktree_with_branch(list, "main"), Some(PathBuf::from("/r")));
        assert_eq!(worktree_with_branch(list, "feat/x"), Some(PathBuf::from("/w/b")));
        assert_eq!(worktree_with_branch(list, "feat"), None);
        assert_eq!(worktree_with_branch(list, "x"), None);
    }

    fn head_branch(wt: &Path) -> String {
        git(wt, &["rev-parse", "--abbrev-ref", "HEAD"])
    }

    #[test]
    fn worktree_reuses_existing_local_branch() {
        let root = std::env::temp_dir().join(format!("devsandbox-wtlocal-{}", std::process::id()));
        let base = repo(&root, "");
        git(&base, &["branch", "feat/a"]);
        git(&base, &["checkout", "-q", "feat/a"]);
        git(&base, &["commit", "-q", "--allow-empty", "-m", "wip"]);
        let tip = git(&base, &["rev-parse", "HEAD"]);
        git(&base, &["checkout", "-q", "main"]);
        let branches = git(&base, &["for-each-ref", "--format=%(refname)", "refs/heads"]);
        let wt = root.join("wt");

        // An explicit base is ignored (noted), not an error.
        let created = create_worktree(&base, &wt, "feat/a", Some("main"), true, &Default::default()).unwrap();

        assert!(!created);
        assert_eq!(head_branch(&wt), "feat/a");
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]), tip);
        assert_eq!(git(&base, &["for-each-ref", "--format=%(refname)", "refs/heads"]), branches);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn worktree_refuses_branch_checked_out_elsewhere_naming_where() {
        let root = std::env::temp_dir().join(format!("devsandbox-wtbusy-{}", std::process::id()));
        let base = repo(&root, "");
        let canon = base.canonicalize().unwrap();

        let err = create_worktree(&base, &root.join("wt"), "main", None, true, &Default::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("already checked out at `{}`", canon.display())), "{err}");
        assert!(!err.contains("instance"), "{err}");
        assert!(!root.join("wt").join(".git").exists());

        // Known to state: the owning instance is named too.
        let state: crate::state::State = toml::from_str(&format!(
            "[instance.web]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = {:?}\nworkspace = \"/w\"\ncreated_unix = 0\n",
            base.to_string_lossy()
        ))
        .unwrap();
        let err = create_worktree(&base, &root.join("wt"), "main", None, true, &state)
            .unwrap_err()
            .to_string();
        assert!(err.contains("(instance `web`)"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn upstream_of(wt: &Path, branch: &str) -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["rev-parse", "--abbrev-ref", &format!("{branch}@{{upstream}}")])
            .output()
            .unwrap();
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    #[test]
    fn worktree_tracks_remote_only_branch() {
        let root = std::env::temp_dir().join(format!("devsandbox-wtremote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (origin, seed, base) = (root.join("origin.git"), root.join("seed"), root.join("base"));
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "--bare", "-b", "main", &origin.to_string_lossy()]);
        git(&root, &["clone", "-q", &origin.to_string_lossy(), "seed"]);
        git(&seed, &["checkout", "-q", "-b", "main"]);
        git(&seed, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        git(&seed, &["checkout", "-q", "-b", "pr/1"]);
        git(&seed, &["commit", "-q", "--allow-empty", "-m", "pr"]);
        git(&seed, &["push", "-q", "origin", "pr/1"]);
        let pr_tip = git(&seed, &["rev-parse", "HEAD"]);
        git(&root, &["clone", "-q", &origin.to_string_lossy(), "base"]);

        let wt = root.join("wt");
        let created = create_worktree(&base, &wt, "pr/1", None, true, &Default::default()).unwrap();
        assert!(!created);
        assert_eq!(head_branch(&wt), "pr/1");
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]), pr_tip);
        assert_eq!(upstream_of(&wt, "pr/1").as_deref(), Some("origin/pr/1"));

        // Pushed after the last full fetch, which is still fresh (so skipped):
        // only the targeted fetch of a named branch can find it.
        git(&base, &["fetch", "-q", "origin"]);
        for b in ["pr/2", "sandbox/gen"] {
            git(&seed, &["checkout", "-q", "-b", b, "main"]);
            git(&seed, &["commit", "-q", "--allow-empty", "-m", b]);
            git(&seed, &["push", "-q", "origin", b]);
        }
        let wt2 = root.join("wt2");
        assert!(!create_worktree(&base, &wt2, "pr/2", None, true, &Default::default()).unwrap());
        assert_eq!(upstream_of(&wt2, "pr/2").as_deref(), Some("origin/pr/2"));
        // A pattern-generated name is never fetched for: a fresh branch.
        let wt3 = root.join("wt3");
        assert!(create_worktree(&base, &wt3, "sandbox/gen", None, false, &Default::default()).unwrap());
        assert_eq!(upstream_of(&wt3, "sandbox/gen"), None);

        // Neither local nor remote: today's fresh, untracked branch.
        let wt4 = root.join("wt4");
        assert!(create_worktree(&base, &wt4, "feat/new", None, true, &Default::default()).unwrap());
        assert_eq!(head_branch(&wt4), "feat/new");
        assert_eq!(upstream_of(&wt4, "feat/new"), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
