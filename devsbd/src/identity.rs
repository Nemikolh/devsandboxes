//! `devsbd identity apply`: fit the adopted account to the identity plan
//! (docs/plans/active/identity.md, section 4), run once per container by a
//! root exec from the host after create. The edits live in the container
//! layer, so a restart keeps them; a repeat run is a no-op.
//!
//! `/etc/passwd` and `/etc/group` are rewritten here rather than with
//! `usermod`, which images needn't have. `/etc/shadow` and `/etc/gshadow` are
//! keyed by name and no account or group is renamed, so they stay as they
//! are; a group added here has no gshadow line, which only `grpck` and
//! `newgrp`/`gpasswd` notice (supplementary groups come from `/etc/group`).
//!
//! The text edits are pure fns over file contents; the walk's per-entry
//! choice is too, and every path is under a root prefix so tests run the
//! whole thing against a temp dir.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: devsbd identity apply <user> [--remap UID:GID | --expect UID:GID | --root-home] [--socket-gid GID] [--volume PATH]...";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ids {
    uid: u32,
    gid: u32,
}

/// What happens to the account's ids. At most one per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No flag: ids kept (VM-backed runtimes).
    Keep,
    /// `--remap`: move the account to these ids (rootful).
    Remap(Ids),
    /// `--expect`: ids kept; warn when they aren't these (keep-id).
    Expect(Ids),
    /// `--root-home`: ids kept, the home handed to root (rootless docker).
    RootHome,
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    user: String,
    mode: Mode,
    socket_gid: Option<u32>,
    volumes: Vec<String>,
}

pub fn run(args: &[String]) -> i32 {
    let parsed = match args.split_first() {
        Some((verb, rest)) if verb == "apply" => parse_args(rest),
        _ => None,
    };
    let Some(args) = parsed else {
        eprintln!("{USAGE}");
        return 2;
    };
    match apply(Path::new("/"), &args) {
        Ok(home) => {
            println!("home={home}");
            0
        }
        Err(e) => {
            eprintln!("devsbd: identity: {e}");
            1
        }
    }
}

fn parse_args(args: &[String]) -> Option<Args> {
    let (mut user, mut mode, mut socket_gid, mut volumes) = (None, None, None, vec![]);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let next_mode = match a.as_str() {
            "--remap" => Some(Mode::Remap(parse_ids(it.next()?)?)),
            "--expect" => Some(Mode::Expect(parse_ids(it.next()?)?)),
            "--root-home" => Some(Mode::RootHome),
            "--socket-gid" if socket_gid.is_none() => {
                socket_gid = Some(it.next()?.parse().ok()?);
                None
            }
            "--volume" => {
                let v = it.next()?;
                if !v.starts_with('/') {
                    return None;
                }
                volumes.push(v.clone());
                None
            }
            s if s.starts_with('-') => return None,
            s if user.is_none() && !s.is_empty() && !s.contains(':') => {
                user = Some(s.to_string());
                None
            }
            _ => return None,
        };
        if let Some(m) = next_mode {
            if mode.replace(m).is_some() {
                return None;
            }
        }
    }
    Some(Args { user: user?, mode: mode.unwrap_or(Mode::Keep), socket_gid, volumes })
}

/// `UID:GID`.
fn parse_ids(s: &str) -> Option<Ids> {
    let (u, g) = s.split_once(':')?;
    Some(Ids { uid: u.parse().ok()?, gid: g.parse().ok()? })
}

/// The whole verb with every path under `root`. Returns the account's home
/// as `/etc/passwd` has it.
fn apply(root: &Path, args: &Args) -> io::Result<String> {
    let under = |p: &str| root.join(p.trim_start_matches('/'));
    let passwd_path = under("/etc/passwd");
    let group_path = under("/etc/group");
    let passwd = read(&passwd_path)?;
    let group = read(&group_path)?;
    let account = find_account(&passwd, &args.user)
        .ok_or_else(|| io::Error::other(format!("no account {} in /etc/passwd", args.user)))?;
    let old = Ids { uid: account.uid, gid: account.gid };

    let (new_passwd, mut new_group, now) = match args.mode {
        Mode::Remap(to) => {
            let p = remap_passwd(&passwd, &account.name, to).map_err(io::Error::other)?;
            (p, remap_group(&group, &account.name, to.gid), to)
        }
        Mode::Expect(want) => {
            if want != old {
                eprintln!("{}", expect_warning(&account.name, old, want));
            }
            (passwd.clone(), group.clone(), old)
        }
        Mode::Keep | Mode::RootHome => (passwd.clone(), group.clone(), old),
    };
    if let Some(gid) = args.socket_gid {
        new_group = add_member(&new_group, &account.name, now.gid, gid).map_err(io::Error::other)?;
    }

    // Who the home and volumes belong to from now on: root when root is the
    // dev user (rootless docker: root is the host user).
    let owner = if args.mode == Mode::RootHome { Ids { uid: 0, gid: 0 } } else { now };
    // The walk before the /etc writes: a run cut short leaves /etc naming the
    // old ids, so the next run walks again and finishes the job; after the
    // writes the old ids are gone and there'd be nothing left to compare to.
    if owner != old && account.home != "/" {
        chown_tree(&under(&account.home), old, owner, &mut |p, ids| {
            std::os::unix::fs::lchown(p, Some(ids.uid), Some(ids.gid))
        })?;
    }
    for v in &args.volumes {
        claim_volume(&under(v), owner)?;
    }
    if new_passwd != passwd {
        write_replacing(&passwd_path, &new_passwd)?;
    }
    if new_group != group {
        write_replacing(&group_path, &new_group)?;
    }
    Ok(account.home)
}

fn read(path: &Path) -> io::Result<String> {
    fs::read_to_string(path).map_err(|e| io::Error::other(format!("{}: {e}", path.display())))
}

fn expect_warning(user: &str, real: Ids, want: Ids) -> String {
    format!(
        "devsbd: identity: warning: account {user} is {}:{}, not the assumed {}:{}: the host user maps to {}:{} \
         (keep-id), so files {user} writes into the workspace come out owned by a subuid on the host",
        real.uid, real.gid, want.uid, want.gid, want.uid, want.gid
    )
}

#[derive(Debug, PartialEq, Eq)]
struct Account {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
}

/// A passwd/group line's fields, `None` for comments, blanks and lines of
/// the wrong shape (kept verbatim by every edit).
fn fields(line: &str, n: usize) -> Option<Vec<&str>> {
    let f: Vec<&str> = line.split(':').collect();
    (f.len() == n && !line.starts_with('#') && !f[0].is_empty()).then_some(f)
}

/// The account named `user`, else (a numeric `user`, as `containerUser`
/// may be) the first with that uid.
fn find_account(passwd: &str, user: &str) -> Option<Account> {
    let entries = passwd.lines().filter_map(|l| {
        let f = fields(l, 7)?;
        Some(Account { name: f[0].into(), uid: f[2].parse().ok()?, gid: f[3].parse().ok()?, home: f[5].into() })
    });
    let all: Vec<Account> = entries.collect();
    let by_uid = user.parse::<u32>().ok();
    let pos = all.iter().position(|a| a.name == user).or_else(|| all.iter().position(|a| Some(a.uid) == by_uid))?;
    all.into_iter().nth(pos)
}

/// Rebuild `text` with `edit` applied to each well-formed line's fields;
/// keeps everything else, including a missing final newline.
fn edit_lines(text: &str, n: usize, mut edit: impl FnMut(&mut Vec<String>)) -> String {
    let mut out: Vec<String> = text
        .split('\n')
        .map(|l| match fields(l, n) {
            Some(f) => {
                let mut f: Vec<String> = f.into_iter().map(str::to_string).collect();
                edit(&mut f);
                f.join(":")
            }
            None => l.to_string(),
        })
        .collect();
    if out.last().is_some_and(String::is_empty) && text.ends_with('\n') {
        out.pop();
        return out.join("\n") + "\n";
    }
    out.join("\n")
}

fn append_line(text: &str, line: &str) -> String {
    let sep = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
    format!("{text}{sep}{line}\n")
}

/// Move `user` to `to`. Another account holding `to.uid` moves to a free uid;
/// its files aren't chowned (they could be anywhere, and scanning the whole
/// filesystem is what this verb avoids), so they now belong to `user`,
/// which on the images this targets is a default account nobody uses (e.g.
/// `ubuntu` in `ubuntu:24.04`).
fn remap_passwd(passwd: &str, user: &str, to: Ids) -> Result<String, String> {
    if to.uid == 0 {
        return Err(format!("refusing to remap {user} to uid 0"));
    }
    let account = find_account(passwd, user).ok_or_else(|| format!("no account {user} in /etc/passwd"))?;
    if account.uid == 0 {
        return Err(format!("refusing to remap {user}: it is uid 0"));
    }
    let used: Vec<u32> = passwd.lines().filter_map(|l| fields(l, 7)?[2].parse().ok()).collect();
    // Not the account's old uid either: files outside its home keep it.
    let free = (1000..u32::MAX).find(|n| *n != to.uid && *n != account.uid && !used.contains(n));
    let mut err = None;
    let out = edit_lines(passwd, 7, |f| {
        if f[0] == account.name {
            f[2] = to.uid.to_string();
            f[3] = to.gid.to_string();
        } else if f[2] == to.uid.to_string() {
            match free {
                Some(n) => f[2] = n.to_string(),
                None => err = Some(format!("no free uid to move {} aside", f[0])),
            }
        }
    });
    err.map_or(Ok(out), Err)
}

/// Give `gid` a group for `user`'s primary gid: an existing group with that
/// gid is used as is (renumbering a system group like `dialout` or `staff`
/// would orphan the files it owns); else `user`'s own group is renumbered;
/// else one is added.
fn remap_group(group: &str, user: &str, gid: u32) -> String {
    let gids = || group.lines().filter_map(|l| fields(l, 4));
    if gids().any(|f| f[2] == gid.to_string()) {
        return group.to_string();
    }
    if gids().any(|f| f[0] == user) {
        return edit_lines(group, 4, |f| {
            if f[0] == user {
                f[2] = gid.to_string();
            }
        });
    }
    append_line(group, &format!("{user}:x:{gid}:"))
}

/// Put `user` (primary gid `primary`) in a group with `gid`: the existing
/// one, else a new `docker` (or `docker-<gid>` when that name is taken).
fn add_member(group: &str, user: &str, primary: u32, gid: u32) -> Result<String, String> {
    if primary == gid {
        return Ok(group.to_string());
    }
    let lines = || group.lines().filter_map(|l| fields(l, 4));
    if lines().any(|f| f[2] == gid.to_string()) {
        return Ok(edit_lines(group, 4, |f| {
            if f[2] == gid.to_string() && !f[3].split(',').any(|m| m == user) {
                f[3] = if f[3].is_empty() { user.to_string() } else { format!("{},{user}", f[3]) };
            }
        }));
    }
    let name = ["docker".to_string(), format!("docker-{gid}")]
        .into_iter()
        .find(|n| !lines().any(|f| f[0] == n))
        .ok_or_else(|| format!("no free group name for gid {gid}"))?;
    Ok(append_line(group, &format!("{name}:x:{gid}:{user}")))
}

/// What the walk does with one entry.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// On another device (a bind mount or volume under the home): left
    /// alone, not descended into.
    OtherDevice,
    Keep,
    Chown(Ids),
}

/// Entries owned by `from.uid` move to `to.uid`, their group too when it's
/// `from.gid`; nothing else changes owner.
fn step(uid: u32, gid: u32, dev: u64, root_dev: u64, from: Ids, to: Ids) -> Step {
    if dev != root_dev {
        return Step::OtherDevice;
    }
    if uid != from.uid {
        return Step::Keep;
    }
    let new = Ids { uid: to.uid, gid: if gid == from.gid { to.gid } else { gid } };
    if new == (Ids { uid, gid }) { Step::Keep } else { Step::Chown(new) }
}

/// Chown what `step` picks under `root` (itself included), never following
/// symlinks: not a blind `chown -R`, which is slow on big homes and would
/// take over mounts. A missing `root` is nothing to do.
fn chown_tree(root: &Path, from: Ids, to: Ids, chown: &mut dyn FnMut(&Path, Ids) -> io::Result<()>) -> io::Result<()> {
    let meta = match fs::symlink_metadata(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        r => r.map_err(|e| io::Error::other(format!("{}: {e}", root.display())))?,
    };
    let root_dev = meta.dev();
    let mut stack = vec![(root.to_path_buf(), meta)];
    while let Some((path, meta)) = stack.pop() {
        let ctx = |e: io::Error| io::Error::other(format!("{}: {e}", path.display()));
        match step(meta.uid(), meta.gid(), meta.dev(), root_dev, from, to) {
            Step::OtherDevice => continue,
            Step::Chown(ids) => chown(&path, ids).map_err(ctx)?,
            Step::Keep => {}
        }
        if !meta.file_type().is_dir() {
            continue;
        }
        for entry in fs::read_dir(&path).map_err(ctx)? {
            let entry = entry.map_err(ctx)?;
            // DirEntry::metadata doesn't follow symlinks.
            match entry.metadata() {
                Ok(m) => stack.push((entry.path(), m)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(ctx(e)),
            }
        }
    }
    Ok(())
}

/// `--volume`: a mount point that's root-owned and empty goes to `owner`
/// (named volumes start root-owned unless the image has the path).
fn claim_volume(path: &Path, owner: Ids) -> io::Result<()> {
    let ctx = |e: io::Error| io::Error::other(format!("{}: {e}", path.display()));
    let meta = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("devsbd: identity: warning: volume {} does not exist", path.display());
            return Ok(());
        }
        r => r.map_err(ctx)?,
    };
    let empty = meta.is_dir() && fs::read_dir(path).map_err(ctx)?.next().is_none();
    if claimable(meta.uid(), empty, owner) {
        std::os::unix::fs::lchown(path, Some(owner.uid), Some(owner.gid)).map_err(ctx)?;
    }
    Ok(())
}

fn claimable(uid: u32, empty_dir: bool, owner: Ids) -> bool {
    uid == 0 && empty_dir && owner.uid != 0
}

/// Replace `path` by a temp file next to it, renamed over it, with the
/// original's mode and owner.
fn write_replacing(path: &Path, text: &str) -> io::Result<()> {
    let ctx = |e: io::Error| io::Error::other(format!("{}: {e}", path.display()));
    let meta = fs::metadata(path).map_err(ctx)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp: PathBuf = path.with_file_name(format!(".{name}.devsbd"));
    let result = (|| {
        fs::write(&tmp, text)?;
        fs::set_permissions(&tmp, meta.permissions())?;
        std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()))?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(uid: u32, gid: u32) -> Ids {
        Ids { uid, gid }
    }

    fn a(v: &[&str]) -> Option<Args> {
        parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn args_take_one_mode() {
        let args = |user: &str, mode, socket_gid, volumes: &[&str]| Args {
            user: user.into(),
            mode,
            socket_gid,
            volumes: volumes.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(a(&["vscode"]), Some(args("vscode", Mode::Keep, None, &[])));
        assert_eq!(a(&["vscode", "--remap", "501:20"]), Some(args("vscode", Mode::Remap(ids(501, 20)), None, &[])));
        assert_eq!(
            a(&["--expect", "1000:1000", "vscode", "--socket-gid", "998", "--volume", "/v", "--volume", "/w"]),
            Some(args("vscode", Mode::Expect(ids(1000, 1000)), Some(998), &["/v", "/w"]))
        );
        assert_eq!(a(&["vscode", "--root-home"]), Some(args("vscode", Mode::RootHome, None, &[])));
        assert_eq!(a(&[]), None);
        assert_eq!(a(&["--root-home"]), None, "no user");
        assert_eq!(a(&["vscode", "--root-home", "--remap", "1:1"]), None, "two modes");
        assert_eq!(a(&["vscode", "--remap", "1:1", "--remap", "1:1"]), None);
        assert_eq!(a(&["vscode", "--remap", "501"]), None);
        assert_eq!(a(&["vscode", "--remap"]), None);
        assert_eq!(a(&["vscode", "501", "20"]), None, "positional ids");
        assert_eq!(a(&["vscode", "--volume", "rel"]), None);
        assert_eq!(a(&["vscode", "--socket-gid", "x"]), None);
        assert_eq!(a(&["vscode", "--socket-gid", "1", "--socket-gid", "2"]), None);
        assert_eq!(a(&["vscode", "--bogus"]), None);
        assert_eq!(a(&["vscode:vscode"]), None);
    }

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
# a comment
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
ubuntu:x:1000:1000:Ubuntu:/home/ubuntu:/bin/bash
vscode:x:1001:1001::/home/vscode:/bin/bash
";
    const GROUP: &str = "\
root:x:0:
dialout:x:20:
docker:x:999:
ubuntu:x:1000:
vscode:x:1001:
";

    #[test]
    fn finds_accounts_by_name_then_uid() {
        let want = Account { name: "vscode".into(), uid: 1001, gid: 1001, home: "/home/vscode".into() };
        assert_eq!(find_account(PASSWD, "vscode"), Some(want));
        assert_eq!(find_account(PASSWD, "1000").map(|a| a.name), Some("ubuntu".into()));
        assert_eq!(find_account(PASSWD, "nobody"), None);
        assert_eq!(find_account(PASSWD, "# a comment"), None);
    }

    #[test]
    fn remap_moves_a_clashing_account_aside() {
        let p = remap_passwd(PASSWD, "vscode", ids(1000, 1000)).unwrap();
        assert_eq!(
            p,
            "\
root:x:0:0:root:/root:/bin/bash
# a comment
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
ubuntu:x:1002:1000:Ubuntu:/home/ubuntu:/bin/bash
vscode:x:1000:1000::/home/vscode:/bin/bash
"
        );
        // Idempotent.
        assert_eq!(remap_passwd(&p, "vscode", ids(1000, 1000)).unwrap(), p);
    }

    #[test]
    fn remap_without_clash_touches_one_line() {
        let p = remap_passwd(PASSWD, "vscode", ids(501, 20)).unwrap();
        assert_eq!(p, PASSWD.replace("vscode:x:1001:1001:", "vscode:x:501:20:"));
        assert_eq!(remap_passwd(&p, "vscode", ids(501, 20)).unwrap(), p);
        // No trailing newline survives as is.
        let trimmed = PASSWD.trim_end();
        assert!(!remap_passwd(trimmed, "vscode", ids(501, 20)).unwrap().ends_with('\n'));
    }

    #[test]
    fn remap_refuses_root() {
        assert!(remap_passwd(PASSWD, "vscode", ids(0, 0)).is_err());
        assert!(remap_passwd(PASSWD, "root", ids(1000, 1000)).is_err());
        assert!(remap_passwd(PASSWD, "nobody", ids(1000, 1000)).is_err());
    }

    #[test]
    fn group_prefers_an_existing_gid() {
        // dialout keeps its gid; vscode's primary gid becomes 20 (passwd).
        assert_eq!(remap_group(GROUP, "vscode", 20), GROUP);
        // No group with the gid: vscode's own group is renumbered.
        let g = remap_group(GROUP, "vscode", 4242);
        assert_eq!(g, GROUP.replace("vscode:x:1001:", "vscode:x:4242:"));
        assert_eq!(remap_group(&g, "vscode", 4242), g);
        // No own group either: one is added.
        let g = remap_group("root:x:0:", "dev", 4242);
        assert_eq!(g, "root:x:0:\ndev:x:4242:\n");
        assert_eq!(remap_group(&g, "dev", 4242), g);
    }

    #[test]
    fn socket_group_reuses_or_adds() {
        let g = add_member(GROUP, "vscode", 1001, 999).unwrap();
        assert_eq!(g, GROUP.replace("docker:x:999:", "docker:x:999:vscode"));
        assert_eq!(add_member(&g, "vscode", 1001, 999).unwrap(), g);
        let g2 = add_member(&g.replace("docker:x:999:vscode", "docker:x:999:a,b"), "vscode", 1001, 999).unwrap();
        assert!(g2.contains("docker:x:999:a,b,vscode\n"));
        // gid unknown, `docker` taken by another gid.
        let g = add_member(GROUP, "vscode", 1001, 968).unwrap();
        assert!(g.ends_with("vscode:x:1001:\ndocker-968:x:968:vscode\n"));
        assert_eq!(add_member(&g, "vscode", 1001, 968).unwrap(), g);
        assert_eq!(add_member("root:x:0:", "dev", 1000, 968).unwrap(), "root:x:0:\ndocker:x:968:dev\n");
        // Already the primary gid.
        assert_eq!(add_member(GROUP, "vscode", 999, 999).unwrap(), GROUP);
    }

    #[test]
    fn step_picks_the_old_uids_entries_on_the_same_device() {
        let (from, to) = (ids(1001, 1001), ids(1000, 1000));
        assert_eq!(step(1001, 1001, 7, 7, from, to), Step::Chown(ids(1000, 1000)));
        assert_eq!(step(1001, 50, 7, 7, from, to), Step::Chown(ids(1000, 50)), "other group kept");
        assert_eq!(step(0, 1001, 7, 7, from, to), Step::Keep, "not the account's");
        assert_eq!(step(1001, 1001, 8, 7, from, to), Step::OtherDevice);
        assert_eq!(step(1000, 1000, 7, 7, from, to), Step::Keep, "already moved");
        assert_eq!(step(1001, 1001, 7, 7, from, from), Step::Keep);
        assert_eq!(step(1001, 1001, 7, 7, from, ids(0, 0)), Step::Chown(ids(0, 0)));
    }

    #[test]
    fn volumes_claimed_only_when_root_owned_and_empty() {
        assert!(claimable(0, true, ids(1000, 1000)));
        assert!(!claimable(0, false, ids(1000, 1000)));
        assert!(!claimable(1000, true, ids(1000, 1000)));
        assert!(!claimable(0, true, ids(0, 0)));
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsbd-identity-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// This process's ids, as the files it creates get them.
    fn me(dir: &Path) -> Ids {
        let m = fs::metadata(dir).unwrap();
        ids(m.uid(), m.gid())
    }

    #[test]
    fn walk_visits_the_tree_without_following_symlinks() {
        let root = temp_root("walk");
        let outside = temp_root("walk-outside");
        fs::write(outside.join("target"), "").unwrap();
        let home = root.join("home");
        fs::create_dir_all(home.join(".cache/deep")).unwrap();
        fs::write(home.join(".cache/deep/f"), "").unwrap();
        fs::write(home.join(".bashrc"), "").unwrap();
        std::os::unix::fs::symlink(&outside, home.join("link")).unwrap();
        let me = me(&home);
        let to = ids(me.uid.wrapping_add(1), me.gid);
        let mut seen = vec![];
        chown_tree(&home, me, to, &mut |p, ids| {
            seen.push((p.strip_prefix(&home).unwrap().to_path_buf(), ids));
            Ok(())
        })
        .unwrap();
        seen.sort_by(|x, y| x.0.cmp(&y.0));
        let want: Vec<(PathBuf, Ids)> =
            ["", ".bashrc", ".cache", ".cache/deep", ".cache/deep/f", "link"].iter().map(|p| (PathBuf::from(p), to)).collect();
        assert_eq!(seen, want, "the link itself, not its target's content");
        // Missing home: nothing.
        chown_tree(&root.join("nope"), me, to, &mut |_, _| panic!("visited")).unwrap();
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    fn setup(tag: &str, passwd: &str) -> PathBuf {
        let root = temp_root(tag);
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::create_dir_all(root.join("home/vscode")).unwrap();
        fs::write(root.join("home/vscode/.bashrc"), "").unwrap();
        fs::write(root.join("etc/passwd"), passwd).unwrap();
        fs::write(root.join("etc/group"), GROUP).unwrap();
        root
    }

    #[test]
    fn apply_remaps_and_is_idempotent() {
        // The account's uid is no one's here, so the walk finds nothing it
        // owns and runs unprivileged; the /etc rewrite is the real one.
        let root = setup("apply", &PASSWD.replace("vscode:x:1001:1001", "vscode:x:54321:54321"));
        fs::set_permissions(root.join("etc/passwd"), std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        let args = Args {
            user: "vscode".into(),
            mode: Mode::Remap(ids(1000, 1000)),
            socket_gid: Some(998),
            volumes: vec!["/missing".into()],
        };
        assert_eq!(apply(&root, &args).unwrap(), "/home/vscode");
        let passwd = fs::read_to_string(root.join("etc/passwd")).unwrap();
        let group = fs::read_to_string(root.join("etc/group")).unwrap();
        assert!(passwd.contains("vscode:x:1000:1000::/home/vscode:/bin/bash\n"));
        assert!(passwd.contains("ubuntu:x:1001:1000:"));
        assert!(group.ends_with("docker-998:x:998:vscode\n"));
        let mode = fs::metadata(root.join("etc/passwd")).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o644);
        assert!(!root.join("etc/.passwd.devsbd").exists());

        apply(&root, &args).unwrap();
        assert_eq!(fs::read_to_string(root.join("etc/passwd")).unwrap(), passwd);
        assert_eq!(fs::read_to_string(root.join("etc/group")).unwrap(), group);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_without_remap_leaves_etc_alone() {
        let passwd = PASSWD.replace("vscode:x:1001:1001", "vscode:x:54321:54321");
        let root = setup("keep", &passwd);
        for mode in [Mode::Keep, Mode::Expect(ids(1000, 1000))] {
            let args = Args { user: "vscode".into(), mode, socket_gid: None, volumes: vec![] };
            assert_eq!(apply(&root, &args).unwrap(), "/home/vscode");
            assert_eq!(fs::read_to_string(root.join("etc/passwd")).unwrap(), passwd);
            assert_eq!(fs::read_to_string(root.join("etc/group")).unwrap(), GROUP);
        }
        let missing = Args { user: "nobody".into(), mode: Mode::Keep, socket_gid: None, volumes: vec![] };
        assert!(apply(&root, &missing).unwrap_err().to_string().contains("no account nobody"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn expect_warning_names_account_ids_and_consequence() {
        let w = expect_warning("vscode", ids(1001, 1001), ids(1000, 1000));
        assert!(w.contains("account vscode is 1001:1001, not the assumed 1000:1000"));
        assert!(w.contains("subuid"));
    }
}
