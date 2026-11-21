//! The folder's remotes (docs/architecture/credentials/fence.md): read once,
//! at a folder's first `run` ever, into `State::folder_remotes`, and the
//! GitHub fence each instance snapshots from those records.
//!
//! From its first container on, a folder's `.git/config` is writable from
//! the sandbox, so a remote added or rewritten there must never widen the
//! fence: the record is never re-read, `rm` leaves it, and only
//! `access forget` changes it, narrowing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::credentials::fence;
use crate::state::{FolderRemotes, Instance, State};

/// The state key of a folder: its canonical path.
pub(crate) fn folder_key(folder: &Path) -> String {
    folder.to_string_lossy().into_owned()
}

/// Record the remotes of each of `folders` (canonical) that has no record
/// yet, then return the GitHub fence they make, for the instance's
/// `github_repos`. Prints the fence of each folder recorded here (a git
/// repo's, or a warning when it couldn't be read).
pub(crate) fn record(folders: &[&Path]) -> Result<Vec<String>> {
    let (fence, lines) = record_at(&State::path()?, folders, Instance::now(), &mut fence::ssh_hostname)?;
    for line in lines {
        eprintln!("{line}");
    }
    Ok(fence)
}

/// [`record`] against the state file `state`: `(fence, lines to print)`.
/// The folders are read outside the state lock and inserted under it only
/// where still absent, so a racing `run` recording first wins.
pub(crate) fn record_at(
    state: &Path,
    folders: &[&Path],
    now: u64,
    resolve: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<(Vec<String>, Vec<String>)> {
    let known = State::load_at(state)?.folder_remotes;
    let mut cached: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut resolve = |alias: &str| cached.entry(alias.to_string()).or_insert_with(|| resolve(alias)).clone();
    let mut read: Vec<(PathBuf, FolderRemotes, bool)> = Vec::new();
    for folder in folders {
        if known.contains_key(&folder_key(folder)) || read.iter().any(|(f, ..)| f == folder) {
            continue;
        }
        let (record, repo) = read_folder(folder, now, &mut resolve);
        read.push((folder.to_path_buf(), record, repo));
    }
    State::update_at(state, |s| {
        let mut lines = Vec::new();
        for (folder, record, repo) in &read {
            if s.folder_remotes.contains_key(&folder_key(folder)) {
                continue;
            }
            if *repo {
                lines.push(recorded_line(folder, record));
            }
            s.folder_remotes.insert(folder_key(folder), record.clone());
        }
        Ok((fence_of(s, folders.iter().copied()), lines))
    })
}

/// A folder's record as read now, and whether it is a git repo (a folder
/// without `.git` is recorded empty, silently: a sandbox's `git init` there
/// later must not count either).
fn read_folder(folder: &Path, now: u64, resolve: &mut dyn FnMut(&str) -> Option<String>) -> (FolderRemotes, bool) {
    if folder.join(".git").symlink_metadata().is_err() {
        let unread = Some("not a git repo when first run".to_string());
        return (FolderRemotes { unread, recorded_at: now, ..Default::default() }, false);
    }
    let keep = |url: &str| fence::parse(url).is_some();
    match super::git::read_remotes(folder, keep) {
        Ok(remotes) => {
            let github: BTreeSet<String> = remotes
                .iter()
                .flat_map(|r| r.urls.iter().chain(&r.pushurls))
                .filter_map(|url| fence::github_repo(url, resolve))
                .collect();
            (FolderRemotes { remotes, github: github.into_iter().collect(), unread: None, recorded_at: now }, true)
        }
        Err(e) => (FolderRemotes { unread: Some(format!("{e:#}")), recorded_at: now, ..Default::default() }, true),
    }
}

fn recorded_line(folder: &Path, record: &FolderRemotes) -> String {
    let folder = folder.display();
    match &record.unread {
        Some(why) => format!(
            "warning: recorded no remotes for `{folder}` ({why}): its GitHub fence is empty; \
             `access.github.repos` adds repos"
        ),
        None if record.github.is_empty() => format!("recorded `{folder}`'s remotes: no GitHub repo"),
        None => format!(
            "recorded `{folder}`'s GitHub fence: {} (`devsandbox access forget {folder} <repo>` drops one)",
            record.github.join(", ")
        ),
    }
}

/// The owners of `fence` as `folders`' records spell them, for git's
/// rewrites (`credentials::git::owners`); none when the state can't be read.
pub(crate) fn git_owners(folders: &[&Path], fence: &[String]) -> Vec<String> {
    let Ok(state) = State::load() else { return Vec::new() };
    crate::credentials::git::owners(folders.iter().filter_map(|f| state.folder_remotes.get(&folder_key(f))), fence)
}

/// The GitHub fence of `folders`' records: their `github` entries, sorted,
/// deduplicated. A folder with no record adds nothing.
pub(crate) fn fence_of<'a>(state: &State, folders: impl IntoIterator<Item = &'a Path>) -> Vec<String> {
    let repos: BTreeSet<&String> = folders
        .into_iter()
        .filter_map(|f| state.folder_remotes.get(&folder_key(f)))
        .flat_map(|r| &r.github)
        .collect();
    repos.into_iter().cloned().collect()
}

/// The folders an instance's fence comes from: its base folder, then each
/// `folders` entry's.
pub(crate) fn instance_folders(inst: &Instance) -> impl Iterator<Item = &Path> {
    std::iter::once(inst.base_folder.as_path()).chain(inst.folders.iter().map(|f| f.base.as_path()))
}

/// Re-take instance `key`'s `github_repos` from the records (`start`), so
/// an `access forget` since applies. Never records: a folder this instance
/// mounted before records existed waits for a `run` or `rebuild`.
pub(crate) fn refresh(key: &str) {
    if let Err(e) = State::path().and_then(|p| refresh_at(&p, key)) {
        eprintln!("warning: cannot refresh `{key}`'s GitHub fence: {e:#}");
    }
}

pub(crate) fn refresh_at(state: &Path, key: &str) -> Result<()> {
    State::update_at(state, |s| {
        let Some(inst) = s.instances.get(key) else { return Ok(()) };
        let fence = fence_of(s, instance_folders(inst));
        if let Some(inst) = s.instances.get_mut(key) {
            inst.github_repos = fence;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success();
        assert!(ok, "git {args:?}");
    }

    fn temp(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("devsandbox-remotes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    fn repo(root: &Path, name: &str, remotes: &[(&str, &str)]) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        for (name, url) in remotes {
            git(&dir, &["remote", "add", name, url]);
        }
        dir
    }

    fn work_alias(alias: &str) -> Option<String> {
        (alias == "github-work").then(|| "github.com".into())
    }

    #[test]
    fn records_on_the_first_run_only() {
        let root = temp("first");
        let state = root.join("state.toml");
        let app = repo(&root, "app", &[("origin", "git@github-work:Org/App.git"), ("up", "https://github.com/up/app")]);
        git(&app, &["config", "--add", "remote.origin.pushurl", "/home/u/elsewhere"]);
        git(&app, &["config", "--add", "remote.origin.url", "https://gitlab.com/o/r"]);
        let lib = repo(&root, "lib", &[("origin", "https://github.com/o/lib")]);
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        let (fence, lines) = record_at(&state, &[&app, &lib, &plain, &app], 5, &mut work_alias).unwrap();
        assert_eq!(fence, ["o/lib", "org/app", "up/app"]);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("GitHub fence: org/app, up/app"), "{}", lines[0]);
        let s = State::load_at(&state).unwrap();
        let rec = &s.folder_remotes[&folder_key(&app)];
        assert_eq!(rec.recorded_at, 5);
        assert_eq!(rec.remotes[0].urls, ["git@github-work:Org/App.git", "https://gitlab.com/o/r"]);
        assert!(rec.remotes[0].pushurls.is_empty(), "a host path is dropped");
        assert_eq!(s.folder_remotes[&folder_key(&plain)].unread.as_deref(), Some("not a git repo when first run"));

        // A sandbox rewrites the remotes and inits the plain folder: nothing
        // is re-read, nothing printed.
        git(&app, &["remote", "add", "planted", "https://github.com/victim/private"]);
        git(&plain, &["init", "-q"]);
        git(&plain, &["remote", "add", "origin", "https://github.com/victim/other"]);
        let mut asked = false;
        let (again, lines) = record_at(&state, &[&app, &plain], 9, &mut |_| {
            asked = true;
            None
        })
        .unwrap();
        assert_eq!(again, ["org/app", "up/app"]);
        assert!(lines.is_empty() && !asked);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unreadable_config_records_an_empty_fence() {
        let root = temp("unread");
        let state = root.join("state.toml");
        let app = repo(&root, "app", &[("origin", "https://github.com/o/r")]);
        std::fs::write(app.join(".git/config"), "[remote \"origin\"\n\turl = https://github.com/o/r\n").unwrap();
        let (fence, lines) = record_at(&state, &[&app], 1, &mut work_alias).unwrap();
        assert!(fence.is_empty());
        assert!(lines[0].starts_with("warning: recorded no remotes"), "{}", lines[0]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rm_keeps_the_record_and_start_takes_a_forget() {
        let root = temp("start");
        let state = root.join("state.toml");
        let app = repo(&root, "app", &[("origin", "https://github.com/o/app"), ("planted", "https://github.com/v/x")]);
        let (fence, _) = record_at(&state, &[&app], 1, &mut work_alias).unwrap();
        assert_eq!(fence, ["o/app", "v/x"]);
        let inst: Instance = toml::from_str(&format!(
            "sandbox = \"app\"\ncontainer = \"devsandbox-app\"\nfolder = \"{0}\"\nbase_folder = \"{0}\"\n\
             workspace = \"/workspaces/app\"\ncreated_unix = 1\ngithub_repos = [\"o/app\", \"v/x\"]\n",
            app.display()
        ))
        .unwrap();
        State::update_at(&state, |s| Ok(s.instances.insert("app".into(), inst))).unwrap();

        State::update_at(&state, |s| {
            let rec = s.folder_remotes.get_mut(&folder_key(&app)).unwrap();
            rec.github.retain(|r| r != "v/x");
            Ok(())
        })
        .unwrap();
        assert_eq!(State::load_at(&state).unwrap().instances["app"].github_repos, ["o/app", "v/x"], "until start");
        refresh_at(&state, "app").unwrap();
        assert_eq!(State::load_at(&state).unwrap().instances["app"].github_repos, ["o/app"]);

        // `rm` drops the instance, never the record: the next run is no
        // first run.
        State::update_at(&state, |s| Ok(s.instances.remove("app"))).unwrap();
        let (fence, lines) = record_at(&state, &[&app], 2, &mut work_alias).unwrap();
        assert_eq!(fence, ["o/app"]);
        assert!(lines.is_empty());
        refresh_at(&state, "gone").unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }
}
