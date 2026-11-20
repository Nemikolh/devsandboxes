//! Every host fetch of a repo's `origin`, against a URL pinned on first use.
//!
//! A sandbox can rewrite `remote.origin.url` in its bind-mounted checkout;
//! host git fetching that URL would use the user's credential helpers and
//! keys on another private repo or a host path, and land the result in
//! `refs/remotes/origin/*`, which the sandbox reads. So the first host fetch
//! pins the URL the config holds (`State::trusted_origins`), later ones fetch
//! the pinned URL by value with devsandbox's own refspec (a rewrite between
//! check and fetch changes nothing, and the config's `remote.origin.fetch`
//! is never used), and a config that disagrees with the pin is not fetched
//! until `devsandbox trust-origin` re-pins it.

use std::path::Path;

use anyhow::{Context, Result};

use super::git::{host_git, read_origin};
use crate::state::State;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refs<'a> {
    /// Every branch into `refs/remotes/origin/*`, dropping deleted ones when
    /// `prune`.
    All { prune: bool },
    /// One branch into `refs/remotes/origin/<branch>`, without writing
    /// `FETCH_HEAD`: its mtime marks a *full* fetch fresh.
    Branch(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// The repo has no `origin`: nothing to fetch.
    NoOrigin,
    Done,
    /// The config's URL isn't the pinned one: not fetched, warned.
    Untrusted,
    /// git failed (offline, auth, a branch missing upstream): its stderr.
    Failed(String),
}

/// What to do with a repo whose config holds origin URL `current`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Trust {
    Fetch(String),
    /// First fetch: pin `current`, then fetch it.
    Pin(String),
    Mismatch { pinned: String, current: String },
}

pub(crate) fn decide(pinned: Option<&str>, current: &str) -> Trust {
    match pinned {
        None => Trust::Pin(current.to_string()),
        Some(p) if p == current => Trust::Fetch(p.to_string()),
        Some(p) => Trust::Mismatch { pinned: p.to_string(), current: current.to_string() },
    }
}

/// The state key of a repo: its canonical common dir.
pub(crate) fn trust_key(common: &Path) -> String {
    common.to_string_lossy().into_owned()
}

/// Fetch `repo`'s origin into `refs/remotes/origin/*` if its URL is trusted
/// (pins in the state file `state`), quietly. A mismatch warns on stderr
/// here; every other outcome is the caller's to report. `Err`: the repo is
/// refused (`check_repo`) or the state unreadable.
pub(crate) fn fetch_trusted_origin(state: &Path, repo: &Path, refs: Refs) -> Result<Fetched> {
    let origin = read_origin(repo)?;
    let Some(current) = origin.url else { return Ok(Fetched::NoOrigin) };
    let key = trust_key(&origin.common);
    let trust = State::update_at(state, |s| {
        let trust = decide(s.trusted_origins.get(&key).map(String::as_str), &current);
        if let Trust::Pin(url) = &trust {
            s.trusted_origins.insert(key.clone(), url.clone());
        }
        Ok(trust)
    })?;
    let url = match trust {
        Trust::Fetch(url) | Trust::Pin(url) => url,
        Trust::Mismatch { pinned, current } => {
            eprintln!("{}", mismatch_warning(repo, &pinned, &current));
            return Ok(Fetched::Untrusted);
        }
    };
    let out = host_git(repo)?
        .args(fetch_args(&url, refs))
        .output()
        .context("failed to run git (is it installed?)")?;
    Ok(if out.status.success() {
        Fetched::Done
    } else {
        Fetched::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string())
    })
}

pub(crate) fn mismatch_warning(repo: &Path, pinned: &str, current: &str) -> String {
    format!(
        "warning: not fetching origin in `{}`: its URL is now `{current}`, not the trusted `{pinned}` (a \
         sandbox may have rewritten it); if you changed it, trust it with `devsandbox trust-origin {}`",
        repo.display(),
        repo.display()
    )
}

/// `git fetch`'s arguments for `refs` from `url`. `--end-of-options`: a URL
/// starting with `-` stays a URL.
fn fetch_args(url: &str, refs: Refs) -> Vec<String> {
    let mut args = vec!["fetch".to_string(), "--quiet".to_string()];
    let refspec = match refs {
        Refs::All { prune } => {
            if prune {
                args.push("--prune".into());
            }
            "+refs/heads/*:refs/remotes/origin/*".to_string()
        }
        Refs::Branch(b) => {
            args.push("--no-write-fetch-head".into());
            format!("+refs/heads/{b}:refs/remotes/origin/{b}")
        }
    };
    args.extend(["--end-of-options".to_string(), url.to_string(), refspec]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn decide_pins_fetches_or_refuses() {
        assert_eq!(decide(None, "/a"), Trust::Pin("/a".into()));
        assert_eq!(decide(Some("/a"), "/a"), Trust::Fetch("/a".into()));
        assert_eq!(
            decide(Some("/a"), "file:///b"),
            Trust::Mismatch { pinned: "/a".into(), current: "file:///b".into() }
        );
    }

    #[test]
    fn fetch_args_name_the_url_and_own_refspec() {
        assert_eq!(
            fetch_args("git@h:o/r.git", Refs::All { prune: true }),
            ["fetch", "--quiet", "--prune", "--end-of-options", "git@h:o/r.git", "+refs/heads/*:refs/remotes/origin/*"]
        );
        assert_eq!(
            fetch_args("/r", Refs::All { prune: false }),
            ["fetch", "--quiet", "--end-of-options", "/r", "+refs/heads/*:refs/remotes/origin/*"]
        );
        assert_eq!(
            fetch_args("/r", Refs::Branch("pr/1")),
            ["fetch", "--quiet", "--no-write-fetch-head", "--end-of-options", "/r", "+refs/heads/pr/1:refs/remotes/origin/pr/1"]
        );
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `<root>/upstream.git` with `main`, cloned to `<root>/base`, plus
    /// `<root>/other.git` with a `secret` branch: a repo a sandbox would
    /// like host git to fetch.
    fn repos(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("devsandbox-origin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (bare, branch) in [("upstream.git", "main"), ("other.git", "secret")] {
            let seed = root.join(format!("seed-{branch}"));
            git(&root, &["init", "-q", "--bare", "-b", "main", bare]);
            git(&root, &["init", "-q", "-b", branch, seed.to_str().unwrap()]);
            git(&seed, &["commit", "-q", "--allow-empty", "-m", branch]);
            git(&seed, &["push", "-q", root.join(bare).to_str().unwrap(), branch]);
        }
        git(&root, &["clone", "-q", root.join("upstream.git").to_str().unwrap(), "base"]);
        (root.clone(), root.join("base"), root.join("state.toml"))
    }

    fn remote_refs(base: &Path) -> String {
        git(base, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/remotes"])
    }

    #[test]
    fn pins_on_first_fetch_then_refuses_a_rewritten_url() {
        let (root, base, state) = repos("pin");
        let upstream = root.join("upstream.git");
        let seed = root.join("seed-main");
        git(&seed, &["commit", "-q", "--allow-empty", "-m", "two"]);
        git(&seed, &["push", "-q", upstream.to_str().unwrap(), "main"]);

        assert_eq!(fetch_trusted_origin(&state, &base, Refs::All { prune: false }).unwrap(), Fetched::Done);
        let key = trust_key(&base.join(".git").canonicalize().unwrap());
        let pins = State::load_at(&state).unwrap().trusted_origins;
        assert_eq!(pins.get(&key).map(String::as_str), upstream.to_str());
        assert_eq!(git(&base, &["rev-parse", "origin/main"]), git(&seed, &["rev-parse", "HEAD"]));
        // `origin/HEAD` survives, as with `git fetch --prune origin`.
        assert_eq!(fetch_trusted_origin(&state, &base, Refs::All { prune: true }).unwrap(), Fetched::Done);
        assert_eq!(git(&base, &["symbolic-ref", "refs/remotes/origin/HEAD"]), "refs/remotes/origin/main");

        // A sandbox points origin at another repo: no fetch, refs untouched,
        // pin unchanged.
        let before = remote_refs(&base);
        let other = root.join("other.git");
        git(&base, &["remote", "set-url", "origin", &format!("file://{}", other.display())]);
        for refs in [Refs::All { prune: true }, Refs::Branch("secret")] {
            assert_eq!(fetch_trusted_origin(&state, &base, refs).unwrap(), Fetched::Untrusted);
        }
        assert_eq!(remote_refs(&base), before);
        assert_eq!(State::load_at(&state).unwrap().trusted_origins, pins);

        git(&base, &["remote", "remove", "origin"]);
        assert_eq!(fetch_trusted_origin(&state, &base, Refs::All { prune: false }).unwrap(), Fetched::NoOrigin);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn fetches_the_pinned_url_not_the_config_refspec() {
        let (root, base, state) = repos("refspec");
        fetch_trusted_origin(&state, &base, Refs::All { prune: false }).unwrap();
        // A configured refspec writing local branches is ignored.
        git(&base, &["config", "remote.origin.fetch", "+refs/heads/*:refs/heads/x/*"]);
        git(&base, &["config", "--add", "remote.origin.url", "/elsewhere"]);
        assert_eq!(fetch_trusted_origin(&state, &base, Refs::Branch("main")).unwrap(), Fetched::Done);
        assert_eq!(git(&base, &["for-each-ref", "--format=%(refname)", "refs/heads/x"]), "");
        assert!(matches!(
            fetch_trusted_origin(&state, &base, Refs::Branch("missing")).unwrap(),
            Fetched::Failed(_)
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
