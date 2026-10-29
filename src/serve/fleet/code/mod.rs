//! Moving a config root and its repos onto a worker (docs/architecture/daemon/fleet.md,
//! *Moving code*; fleet step 3c1). The plan's `Transfer` seam, in its git +
//! tar implementation:
//!
//! - **offer** ([`offer`], on the hub): what a sandbox needs on the worker:
//!   its config root as a tar ([`tar`], content-hashed) and, for its
//!   `folder` and each `folders` entry outside the root, one branch of the
//!   folder's `origin` as a bundle from the hub's repo cache ([`repo`]).
//!   Anything that can't move yet is [`Unprepared::NotMovable`]: the run
//!   stays where it is.
//! - **haves** ([`Transfer::haves`]): what the worker has already; nothing
//!   asks yet, the worker keeps a root it has.
//! - **send** ([`send`]): the tar, then each bundle, one `TRANSFER` each,
//!   through the worker's link ([`LinkTransfer`]).
//! - **receive** ([`receivers`], on the worker): `root-tar` unpacked into
//!   `<fleet root>/roots/<hash>/`, `bundle` into a checkout below it.
//!
//! The layout keeps every `folder` where the config says, relative to the
//! root: the tar's members sit under the root's last components on the hub
//! (as many as the folders climb with `..`, [`layout`]), so `../web` from
//! `/w/cfg` is `<hash>/cfg` and `<hash>/web` on the worker.
//! [`prepare`] is offer + send.

pub mod repo;
pub mod tar;

use std::collections::BTreeSet;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::transfer::{self, Header, Received, Receivers};
use crate::config::Config;
use crate::serve::endpoint;

/// A config root's tar: [`tar`], unpacked under `roots/<hash>/`.
pub const ROOT_TAR_KIND: &str = "root-tar";
/// One branch of a folder's repo: a git bundle, landed at its header's `to`.
pub const BUNDLE_KIND: &str = "bundle";
/// The largest config root tar either end takes.
pub const ROOT_TAR_LIMIT: u64 = 256 << 20;
/// The largest bundle a worker takes.
pub const BUNDLE_LIMIT: u64 = 4 << 30;

/// Top-level entries of a config root that never travel: per-host and
/// per-instance state (worktrees, shared volumes, `worktree-link` stores).
/// `.git` is skipped at every depth.
pub const ROOT_SKIPS: [&str; 3] = [".worktrees", "shared-volumes", "shared-files"];

/// Why a sandbox doesn't go to a worker.
#[derive(Debug)]
pub enum Unprepared {
    /// It can't move yet (a folder with no `origin`, …): it runs where it
    /// is; the reason is for the user.
    NotMovable(String),
    /// Moving it failed.
    Failed(anyhow::Error),
}

impl std::fmt::Display for Unprepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unprepared::NotMovable(why) => write!(f, "runs where it is: {why}"),
            Unprepared::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for Unprepared {
    fn from(e: anyhow::Error) -> Self {
        Unprepared::Failed(e)
    }
}

fn not_movable<T>(why: impl Into<String>) -> Result<T, Unprepared> {
    Err(Unprepared::NotMovable(why.into()))
}

/// What the receiver has already.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Haves {
    /// Root hashes unpacked under `roots/`.
    pub roots: BTreeSet<String>,
}

/// The seam bodies cross: production sends through a worker's link
/// ([`LinkTransfer`]); another implementation (a sync layer) replaces it
/// without changing [`prepare`]'s callers.
pub trait Transfer {
    /// What the receiver has already (nothing is asked yet: an empty set,
    /// and the receiver keeps a root it has).
    fn haves(&self) -> Result<Haves> {
        Ok(Haves::default())
    }
    /// One transfer: `header`, then `body`; the receiver's refusal is the
    /// error.
    fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()>;
}

/// [`Transfer`] over a worker's link: each send one connection to its
/// `fleet-link`'s transfer socket (`endpoint::fleet_transfer_socket`).
pub struct LinkTransfer {
    pub socket: PathBuf,
}

impl LinkTransfer {
    /// The transfer socket of `worker`'s link in the socket dir `dir`.
    pub fn for_worker(dir: &Path, worker: &str) -> LinkTransfer {
        LinkTransfer { socket: endpoint::fleet_transfer_socket(dir, worker) }
    }
}

impl Transfer for LinkTransfer {
    fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()> {
        let stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("cannot reach the worker's link at {}", self.socket.display()))?;
        transfer::send(stream, header, body)
    }
}

/// Where things land below `roots/<hash>/` on the worker ([`layout`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The config root.
    pub root: PathBuf,
    /// Each folder, in the order given.
    pub folders: Vec<PathBuf>,
}

/// The layout of config root `root` and `folders` outside it (absolute,
/// canonical) below the transferred tree, pure. Each folder's path relative
/// to `root` climbs `n` times (`..`) then goes down; the root sits under
/// its last `max(n)` components (at least one, its own name), so every
/// folder's relative path resolves inside the tree, with the hub's names.
/// Refused (as why): a folder at or in the root, one holding the root, two
/// folders where one holds the other, a root too shallow for its folders.
pub fn layout(root: &Path, folders: &[PathBuf]) -> Result<Layout, String> {
    let names = |p: &Path| -> Vec<String> {
        p.components().filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
    };
    let root_names = names(root);
    let mut climbs = Vec::new();
    for f in folders {
        let f_names = names(f);
        let common = root_names.iter().zip(&f_names).take_while(|(a, b)| a == b).count();
        let ups = root_names.len() - common;
        let down = f_names[common..].to_vec();
        if ups == 0 {
            return Err(format!("folder {} is in the config root", f.display()));
        }
        if down.is_empty() {
            return Err(format!("folder {} holds the config root", f.display()));
        }
        climbs.push((ups, down));
    }
    let depth = climbs.iter().map(|(ups, _)| *ups).max().unwrap_or(0).max(1);
    if root_names.len() < depth {
        return Err(format!("config root {} is too shallow for its folders", root.display()));
    }
    let root_at: Vec<String> = root_names[root_names.len() - depth..].to_vec();
    let mut out = Vec::new();
    for (ups, down) in climbs {
        let at: PathBuf = root_at[..depth - ups].iter().chain(&down).collect();
        out.push(at);
    }
    for (i, a) in out.iter().enumerate() {
        for b in &out[i + 1..] {
            if a.starts_with(b) || b.starts_with(a) {
                return Err(format!("folders {} and {} nest", folders[i].display(), b.display()));
            }
        }
    }
    Ok(Layout { root: root_at.iter().collect(), folders: out })
}

/// One repo an [`Offer`] carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOffer {
    /// The folder on the hub.
    pub host: PathBuf,
    /// Where it lands below `roots/<hash>/`.
    pub at: PathBuf,
    /// The hub's cache of its `origin`.
    pub cache: PathBuf,
    /// The branch the bundle carries.
    pub branch: String,
}

/// What [`send`] sends for one sandbox.
#[derive(Debug, Clone)]
pub struct Offer {
    /// The config root's tar and its hash (the sha256's first 16 hex).
    pub tar: Vec<u8>,
    pub hash: String,
    pub layout: Layout,
    pub repos: Vec<RepoOffer>,
}

/// Where a sandbox is on the worker once [`prepare`]d.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The config root there (`<fleet root>/roots/<hash>/<root>`): the
    /// `root` a placed `run` names.
    pub remote_root: PathBuf,
    /// Each moved folder: its host path, its path there, the branch its
    /// bundle carried (checked out in a new checkout; an existing one keeps
    /// what it has checked out and gets that branch updated, `repo::land`).
    pub folders: Vec<(PathBuf, PathBuf, String)>,
}

/// What sandbox `sandbox` of `config_root` needs on a worker, its repos
/// fetched into the hub's cache under `repos` (`<data>/devsandbox/repos`).
/// `branch` (a `run --branch`) is the `folder`'s branch when its origin
/// has it; else, and for `folders` entries, the checkout's current branch.
/// `folders` entries inside the config root travel in its tar (their
/// `.git` doesn't); one with a `${…}` variable doesn't move yet.
pub fn offer(repos: &Path, config_root: &Path, sandbox: &str, branch: Option<&str>) -> Result<Offer, Unprepared> {
    let root = config_root.canonicalize().with_context(|| format!("config root {} does not exist", config_root.display()))?;
    let config = Config::load(&root)?;
    let resolved = config.resolve_sandbox(sandbox)?;
    let props = &resolved.properties;
    let folder = props.folder.as_deref().with_context(|| format!("sandbox `{sandbox}` has no `folder`"))?;
    let resolve = |f: &str| root.join(f).canonicalize().with_context(|| format!("sandbox folder `{f}` does not exist"));
    let main = resolve(folder)?;
    if main.starts_with(&root) {
        return not_movable(format!("its folder {} is in the config root (a root that's a repo moves in fleet step 12)", main.display()));
    }
    let mut outside = vec![main];
    for entry in props.folders.iter().flat_map(|f| f.values()) {
        if entry.path().contains("${") {
            return not_movable(format!("`folders` entry `{}` uses a variable", entry.path()));
        }
        let host = resolve(entry.path())?;
        if !host.starts_with(&root) && !outside.contains(&host) {
            outside.push(host);
        }
    }
    let layout = layout(&root, &outside).map_err(Unprepared::NotMovable)?;
    let mut repos_out = Vec::new();
    for (i, (host, at)) in outside.iter().zip(&layout.folders).enumerate() {
        let Some(checkout) = repo::probe(host)? else {
            return not_movable(format!("{} isn't a git checkout", host.display()));
        };
        let Some(origin) = checkout.origin else {
            return not_movable(format!("{} has no `origin` (only committed, pushed work moves for now)", host.display()));
        };
        let cache = repo::cache_dir(repos, &origin);
        if let Err(e) = repo::fetch(&cache, &origin) {
            return not_movable(format!("{}'s origin isn't reachable from here: {e:#}", host.display()));
        }
        let wanted: Vec<&str> = (if i == 0 { branch } else { None }).into_iter().chain(checkout.branch.as_deref()).collect();
        let mut found = None;
        for b in &wanted {
            repo::check_branch(b)?;
            if repo::has_branch(&cache, b)? {
                found = Some(b.to_string());
                break;
            }
        }
        let Some(branch) = found else {
            let names = if wanted.is_empty() { "no branch (detached)".to_string() } else { wanted.join(" / ") };
            return not_movable(format!("{}: {names} isn't on origin (push it first)", host.display()));
        };
        repos_out.push(RepoOffer { host: host.clone(), at: at.clone(), cache, branch });
    }
    let tar = pack_root(&root, &layout.root)?;
    let hash = short_hash(&tar);
    Ok(Offer { tar, hash, layout, repos: repos_out })
}

/// The first 16 hex of `bytes`' sha256.
pub fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The config root `root` as a [`tar`] with its members under `at`: every
/// dir, regular file (exec bit kept) and symlink (stored, never followed)
/// in sorted order, but [`ROOT_SKIPS`] at the top and `.git` anywhere;
/// other kinds (sockets, fifos) are left out. A symlink pointing out of the
/// root ([`tar::check_link`] over its path in the root) or a non-UTF-8
/// name refuses the root; so does a tar over [`ROOT_TAR_LIMIT`].
pub fn pack_root(root: &Path, at: &Path) -> Result<Vec<u8>> {
    let at = at.to_str().context("the root's name isn't UTF-8")?.to_string();
    let mut w = tar::Writer::default();
    let mut prefix = String::new();
    for part in at.split('/') {
        prefix = if prefix.is_empty() { part.to_string() } else { format!("{prefix}/{part}") };
        w.dir(&prefix)?;
    }
    walk(root, "", &at, &mut w)?;
    Ok(w.finish())
}

fn walk(dir: &Path, rel: &str, at: &str, w: &mut tar::Writer) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot list {}", dir.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("cannot list {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let name = name.to_str().with_context(|| format!("{} has a non-UTF-8 name", e.path().display()))?;
        if name == ".git" || (rel.is_empty() && ROOT_SKIPS.contains(&name)) {
            continue;
        }
        let rel = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
        let member = format!("{at}/{rel}");
        let path = e.path();
        let meta = path.symlink_metadata().with_context(|| format!("cannot read {}", path.display()))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            let target = std::fs::read_link(&path).with_context(|| format!("cannot read {}", path.display()))?;
            let target = target.to_str().with_context(|| format!("{} points at a non-UTF-8 path", path.display()))?;
            tar::check_link(&rel, target)?;
            w.link(&member, target)?;
        } else if kind.is_dir() {
            w.dir(&member)?;
            walk(&path, &rel, at, w)?;
        } else if kind.is_file() {
            if w.len() as u64 + meta.len() > ROOT_TAR_LIMIT {
                bail!("the config root is over {ROOT_TAR_LIMIT} bytes");
            }
            let data = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
            w.file(&member, meta.permissions().mode() & 0o100 != 0, &data)?;
        }
    }
    Ok(())
}

/// Send `offer` through `t`: the root's tar (unless `t` has it), then each
/// repo's bundle (written to a temp file in the cache's parent first);
/// where it all is on the worker whose fleet root is `worker_root`.
pub fn send(offer: &Offer, worker_root: &Path, t: &dyn Transfer) -> Result<Prepared> {
    let haves = t.haves()?;
    if !haves.roots.contains(&offer.hash) {
        let (size, sha256) = transfer::digest(&offer.tar[..])?;
        let header = Header { kind: ROOT_TAR_KIND.into(), name: format!("root-{}.tar", offer.hash), size, sha256, to: None };
        t.send(&header, &mut &offer.tar[..]).context("cannot send the config root")?;
    }
    let tree = worker_root.join("roots").join(&offer.hash);
    let mut folders = Vec::new();
    for r in &offer.repos {
        let dir = r.cache.parent().context("a repo cache has a parent")?;
        let file = TempFile(dir.join(format!(".bundle-{}-{}.part", std::process::id(), next_seq())));
        repo::bundle(&r.cache, &r.branch, &file.0)?;
        let open = || std::fs::File::open(&file.0).with_context(|| format!("cannot read {}", file.0.display()));
        let (size, sha256) = transfer::digest(open()?)?;
        let to = r.at.to_str().context("a folder's name isn't UTF-8")?;
        let header =
            Header { kind: BUNDLE_KIND.into(), name: "repo.bundle".into(), size, sha256, to: Some(format!("{}/{to}", offer.hash)) };
        t.send(&header, &mut open()?).with_context(|| format!("cannot send {}", r.host.display()))?;
        folders.push((r.host.clone(), tree.join(&r.at), r.branch.clone()));
    }
    Ok(Prepared { remote_root: tree.join(&offer.layout.root), folders })
}

/// Put sandbox `sandbox` of `config_root` on the worker whose fleet root is
/// `worker_root`, through `t` ([`offer`], then [`send`]); `repos` is the
/// hub's repo cache dir. The `run` verb's step before it runs there.
pub fn prepare(
    t: &dyn Transfer,
    repos: &Path,
    worker_root: &Path,
    config_root: &Path,
    sandbox: &str,
    branch: Option<&str>,
) -> Result<Prepared, Unprepared> {
    let offer = offer(repos, config_root, sandbox, branch)?;
    Ok(send(&offer, worker_root, t)?)
}

/// `<data>/devsandbox/repos`, the hub's repo caches.
pub fn repos_dir() -> Result<PathBuf> {
    Ok(crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))?.join("repos"))
}

fn next_seq() -> u32 {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// A file removed on drop.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The worker's receivers for code, into fleet root `fleet_root`, added to
/// `receivers`:
///
/// - `root-tar` named `root-<hash>.tar`, `<hash>` its sha256's first 16
///   hex: unpacked ([`tar::extract`]) into a temp dir renamed to
///   `roots/<hash>/`; a hash already there is kept.
/// - `bundle` with `to` = `<hash>/<path>`, `<hash>` a root unpacked here:
///   landed ([`repo::land`]) at `roots/<hash>/<path>`, every dir on the way
///   a real one (no symlink).
///
/// Bodies wait in `roots/.in/`; the unpacking's error is the transfer's
/// answer, so the hub hears it.
pub fn receivers(fleet_root: &Path, receivers: Receivers) -> Receivers {
    let roots = fleet_root.join("roots");
    let (tar_in, tar_roots, bundle_in, bundle_roots) = (roots.clone(), roots.clone(), roots.clone(), roots);
    receivers
        .with(
            ROOT_TAR_KIND,
            ROOT_TAR_LIMIT,
            move |h| {
                let want = format!("root-{}.tar", &h.sha256[..16]);
                if h.name != want || h.to.is_some() {
                    bail!("a root tar is named {want}, not {}", h.name);
                }
                Ok(tar_in.join(".in").join(&h.name))
            },
            |_| {},
        )
        .then(ROOT_TAR_KIND, move |r| {
            let unpacked = unpack_root(&r.path, &tar_roots, &r.header.sha256[..16]);
            let _ = std::fs::remove_file(&r.path);
            unpacked
        })
        .with(
            BUNDLE_KIND,
            BUNDLE_LIMIT,
            move |h| {
                let (hash, _) = bundle_target(h)?;
                if !bundle_in.join(hash).is_dir() {
                    bail!("root {hash} isn't here (its tar goes first)");
                }
                Ok(bundle_in.join(".in").join(format!("{}.bundle", h.sha256)))
            },
            |_| {},
        )
        .then(BUNDLE_KIND, move |r: &Received| {
            let landed = (|| {
                let (hash, rest) = bundle_target(&r.header)?;
                let tree = bundle_roots.join(hash);
                let mut at = tree.clone();
                for part in rest.split('/') {
                    at.push(part);
                    if at.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
                        bail!("{} isn't a directory", at.display());
                    }
                }
                repo::land(&r.path, &at).map(|_| ())
            })();
            let _ = std::fs::remove_file(&r.path);
            landed
        })
}

/// A bundle header's `to`, as `(root hash, path below it)`.
fn bundle_target(h: &Header) -> Result<(&str, &str)> {
    let to = h.to.as_deref().context("a bundle names where it goes (`to`)")?;
    let (hash, rest) = to.split_once('/').with_context(|| format!("a bundle goes below a root, not to {to}"))?;
    if hash.len() != 16 || !hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("`{hash}` isn't a root hash");
    }
    Ok((hash, rest))
}

/// The root tar at `tar` into `roots/<hash>/` (kept if there).
fn unpack_root(tar: &Path, roots: &Path, hash: &str) -> Result<()> {
    let dest = roots.join(hash);
    if dest.is_dir() {
        return Ok(());
    }
    let bytes = std::fs::read(tar).with_context(|| format!("cannot read {}", tar.display()))?;
    if short_hash(&bytes) != hash {
        bail!("the tar isn't root {hash}");
    }
    let temp = roots.join(format!(".{hash}.{}-{}.part", std::process::id(), next_seq()));
    let unpacked = tar::extract(&bytes, &temp).and_then(|()| match std::fs::rename(&temp, &dest) {
        Ok(()) => Ok(()),
        // Another transfer of the same root got there first.
        Err(_) if dest.is_dir() => Ok(()),
        Err(e) => Err(anyhow!(e).context(format!("cannot move the root to {}", dest.display()))),
    });
    let _ = std::fs::remove_dir_all(&temp);
    unpacked
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::repo::tests::{git, origin_and_checkout, scratch};
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn layout_keeps_each_folders_offset() {
        let l = layout(&p("/w/.devsandboxes"), &[p("/w/devsandboxes"), p("/w/stackblitz/bolt")]).unwrap();
        assert_eq!(l, Layout { root: p(".devsandboxes"), folders: vec![p("devsandboxes"), p("stackblitz/bolt")] });
        let l = layout(&p("/home/u/src/cfg"), &[p("/home/u/src/web"), p("/home/u/lib")]).unwrap();
        assert_eq!(l, Layout { root: p("src/cfg"), folders: vec![p("src/web"), p("lib")] });
        // No folder outside: the root under its own name.
        assert_eq!(layout(&p("/w/cfg"), &[]).unwrap(), Layout { root: p("cfg"), folders: vec![] });
        let err = |root: &str, f: &[&str]| layout(&p(root), &f.iter().map(|s| p(s)).collect::<Vec<_>>()).unwrap_err();
        assert!(err("/w/cfg", &["/w/cfg"]).contains("is in the config root"));
        assert!(err("/w/cfg", &["/w/cfg/app"]).contains("is in the config root"));
        assert!(err("/w/cfg", &["/w"]).contains("holds the config root"));
        assert!(err("/w/cfg", &["/w/a", "/w/a/b"]).contains("nest"));
        assert!(err("/", &[]).contains("too shallow"));
        // A folder elsewhere entirely climbs to the top: the root keeps all its names.
        assert_eq!(layout(&p("/w/cfg"), &[p("/x/y")]).unwrap(), Layout { root: p("w/cfg"), folders: vec![p("x/y")] });
    }

    /// `<dir>/cfg` (the config root, sandbox `web` with `folder = "../web"`,
    /// plus what must not travel) and `<dir>/web`, a checkout of
    /// `<dir>/origin.git`.
    fn fixture(dir: &Path) -> PathBuf {
        origin_and_checkout(dir, "web");
        let cfg = dir.join("cfg");
        for d in ["dockerfiles", ".worktrees/web-2", "shared-volumes/h", "shared-files/x", ".git/objects", "features/f/.git"] {
            std::fs::create_dir_all(cfg.join(d)).unwrap();
        }
        std::fs::write(cfg.join("devsandboxes.toml"), "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\n").unwrap();
        std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM x\n").unwrap();
        std::fs::write(cfg.join("features/f/install.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(cfg.join("features/f/install.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(cfg.join(".worktrees/web-2/x"), "no").unwrap();
        std::fs::write(cfg.join(".git/HEAD"), "no").unwrap();
        std::fs::write(cfg.join("features/f/.git/HEAD"), "no").unwrap();
        std::os::unix::fs::symlink("dockerfiles/web.Dockerfile", cfg.join("Dockerfile")).unwrap();
        cfg
    }

    fn member_paths(tar: &[u8]) -> Vec<String> {
        tar::read(tar).unwrap().into_iter().map(|m| m.path).collect()
    }

    #[test]
    fn the_root_tar_skips_state_and_git_and_hashes_by_content() {
        let dir = scratch("pack");
        let cfg = fixture(&dir);
        let tar = pack_root(&cfg, &p("cfg")).unwrap();
        assert_eq!(
            member_paths(&tar),
            [
                "cfg",
                "cfg/Dockerfile",
                "cfg/devsandboxes.toml",
                "cfg/dockerfiles",
                "cfg/dockerfiles/web.Dockerfile",
                "cfg/features",
                "cfg/features/f",
                "cfg/features/f/install.sh",
            ]
        );
        // Stable: same content, same bytes, whatever the mtimes.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM x\n").unwrap();
        assert_eq!(short_hash(&pack_root(&cfg, &p("cfg")).unwrap()), short_hash(&tar));
        // The layout is in the bytes too.
        assert_ne!(short_hash(&pack_root(&cfg, &p("w/cfg")).unwrap()), short_hash(&tar));
        std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM y\n").unwrap();
        assert_ne!(short_hash(&pack_root(&cfg, &p("cfg")).unwrap()), short_hash(&tar));
        // A symlink out of the root refuses it.
        std::os::unix::fs::symlink("../web", cfg.join("out")).unwrap();
        let err = format!("{:#}", pack_root(&cfg, &p("cfg")).unwrap_err());
        assert!(err.contains("symlink out points outside the config root"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `receivers(root)` taking one `send` over a socket pair.
    fn deliver(r: &Arc<Receivers>, header: &Header, body: &[u8]) -> Result<()> {
        transfer::send(r.accept().unwrap(), header, body)
    }

    fn tar_header(tar: &[u8]) -> Header {
        let (size, sha256) = transfer::digest(tar).unwrap();
        Header { kind: ROOT_TAR_KIND.into(), name: format!("root-{}.tar", short_hash(tar)), size, sha256, to: None }
    }

    #[test]
    fn the_worker_unpacks_a_root_once_and_refuses_bad_ones() {
        let dir = scratch("recv-root");
        let cfg = fixture(&dir);
        let fleet = dir.join("fleet");
        let r = Arc::new(receivers(&fleet, Receivers::new(|_| {})));
        let tar = pack_root(&cfg, &p("cfg")).unwrap();
        let h = tar_header(&tar);
        deliver(&r, &h, &tar).unwrap();
        let tree = fleet.join("roots").join(short_hash(&tar));
        assert_eq!(std::fs::read_to_string(tree.join("cfg/devsandboxes.toml")).unwrap(), "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\n");
        assert_eq!(std::fs::read_link(tree.join("cfg/Dockerfile")).unwrap(), p("dockerfiles/web.Dockerfile"));
        assert!(std::fs::metadata(tree.join("cfg/features/f/install.sh")).unwrap().permissions().mode() & 0o100 != 0);
        // Again: kept as is.
        std::fs::write(tree.join("cfg/marker"), "").unwrap();
        deliver(&r, &h, &tar).unwrap();
        assert!(tree.join("cfg/marker").exists());
        let leftovers: Vec<_> = std::fs::read_dir(fleet.join("roots")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 2, "the tree and `.in`: {leftovers:?}");
        assert_eq!(std::fs::read_dir(fleet.join("roots/.in")).unwrap().count(), 0);

        // A name that isn't its hash; a member escaping.
        let err = format!("{:#}", deliver(&r, &Header { name: "root-0000000000000000.tar".into(), ..h.clone() }, &tar).unwrap_err());
        assert!(err.contains("a root tar is named"), "{err}");
        let mut evil = tar::Writer::default();
        evil.dir("cfg").unwrap();
        let mut bytes = evil.finish();
        bytes.truncate(bytes.len() - 1024);
        let mut raw = [0u8; 512];
        raw[..9].copy_from_slice(b"../escape");
        raw[100..108].copy_from_slice(b"0000644\0");
        raw[124..136].copy_from_slice(b"00000000000\0");
        raw[156] = b'0';
        raw[148..156].copy_from_slice(b"        ");
        let sum: u32 = raw.iter().map(|&b| u32::from(b)).sum();
        raw[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        bytes.extend_from_slice(&raw);
        bytes.extend_from_slice(&[0; 1024]);
        let err = format!("{:#}", deliver(&r, &tar_header(&bytes), &bytes).unwrap_err());
        assert!(err.contains("unsafe path \"../escape\""), "{err}");
        assert!(!dir.join("fleet/roots/../escape").exists() && !fleet.join("escape").exists());
        assert!(!fleet.join("roots").join(short_hash(&bytes)).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bundle_needs_its_root_and_a_plain_target() {
        let dir = scratch("recv-bundle");
        let fleet = dir.join("fleet");
        let r = Arc::new(receivers(&fleet, Receivers::new(|_| {})));
        let (size, sha256) = transfer::digest(&b"x"[..]).unwrap();
        let h = Header { kind: BUNDLE_KIND.into(), name: "repo.bundle".into(), size, sha256, to: Some("0123456789abcdef/web".into()) };
        let err = format!("{:#}", deliver(&r, &h, b"x").unwrap_err());
        assert!(err.contains("root 0123456789abcdef isn't here"), "{err}");
        let err = format!("{:#}", deliver(&r, &Header { to: None, ..h.clone() }, b"x").unwrap_err());
        assert!(err.contains("names where it goes"), "{err}");
        let err = format!("{:#}", deliver(&r, &Header { to: Some("web".into()), ..h.clone() }, b"x").unwrap_err());
        assert!(err.contains("below a root"), "{err}");
        // Through a symlink the root's tar made: refused.
        let tree = fleet.join("roots/0123456789abcdef");
        std::fs::create_dir_all(&tree).unwrap();
        std::os::unix::fs::symlink("/tmp", tree.join("web")).unwrap();
        let err = format!("{:#}", deliver(&r, &h, b"x").unwrap_err());
        assert!(err.contains("isn't a directory"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_cant_move_says_why() {
        let dir = scratch("not-movable");
        let cfg = fixture(&dir);
        let repos = dir.join("repos");
        let why = |toml: &str| {
            std::fs::write(cfg.join("devsandboxes.toml"), toml).unwrap();
            match offer(&repos, &cfg, "web", None) {
                Err(Unprepared::NotMovable(why)) => why,
                other => panic!("{other:?}"),
            }
        };
        std::fs::create_dir_all(dir.join("plain")).unwrap();
        assert!(why("[sandbox.web]\nfolder = \"../plain\"\nimage = \"i\"\n").contains("isn't a git checkout"));
        std::fs::create_dir_all(dir.join("local")).unwrap();
        git(&dir.join("local"), &["init", "-q"]);
        assert!(why("[sandbox.web]\nfolder = \"../local\"\nimage = \"i\"\n").contains("has no `origin`"));
        git(&dir.join("local"), &["remote", "add", "origin", dir.join("gone.git").to_str().unwrap()]);
        assert!(why("[sandbox.web]\nfolder = \"../local\"\nimage = \"i\"\n").contains("origin isn't reachable"));
        assert!(why("[sandbox.web]\nfolder = \"dockerfiles\"\nimage = \"i\"\n").contains("is in the config root"));
        git(&dir.join("web"), &["checkout", "-q", "-b", "local-only"]);
        assert!(why("[sandbox.web]\nfolder = \"../web\"\nimage = \"i\"\n").contains("local-only isn't on origin"));
        assert!(why("[sandbox.web]\nfolder = \"../web\"\nimage = \"i\"\nfolders = { \"/w/x\" = \"../${localEnv:X}\" }\n")
            .contains("uses a variable"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hub's `fleet-link` (opener) and the worker's lending end
    /// (answerer) in-process, the worker's receivers [`receivers`] into
    /// `fleet`; the hub's transfer socket.
    fn link(dir: &Path, fleet: &Path) -> PathBuf {
        use crate::devsbd::proto::channel;
        use crate::serve::fleet::link::{Serves, answerer_with, opener_with};
        let hub_dir = dir.join("hub");
        std::fs::create_dir_all(&hub_dir).unwrap();
        let xfer = endpoint::fleet_transfer_socket(&hub_dir, "w");
        let ours = Serves::default().transfers(Arc::new(Receivers::new(|_| {})));
        let theirs = Serves::default().with(channel::API, || None).transfers(Arc::new(receivers(fleet, Receivers::new(|_| {}))));
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        let path = hub_dir.join("fleet-w.sock");
        let sockets = [(channel::API, path), (channel::TRANSFER, xfer.clone())];
        std::thread::spawn(move || {
            let sockets: Vec<_> = sockets.iter().map(|(ch, p)| (*ch, p.as_path())).collect();
            opener_with(&super::super::LINK, o_r, o_w, &sockets, ours, |_| Ok(()))
        });
        std::thread::spawn(move || answerer_with(&super::super::LINK, a_r, a_w, &theirs, None, |_| Ok(()), || {}));
        let start = std::time::Instant::now();
        while UnixStream::connect(&xfer).is_err() {
            assert!(start.elapsed() < Duration::from_secs(5), "the transfer socket accepts");
            std::thread::sleep(Duration::from_millis(10));
        }
        xfer
    }

    #[test]
    fn prepare_moves_a_sandbox_onto_a_worker_over_its_link() {
        let dir = scratch("prepare");
        let cfg = fixture(&dir);
        std::fs::write(
            cfg.join("devsandboxes.toml"),
            "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\nfolders = { \"/workspaces/lib\" = \"../lib\", \"/workspaces/cfg\" = \".\" }\n",
        )
        .unwrap();
        // `../lib`: a second repo, same origin, on `feature`.
        git(&dir, &["clone", "-q", "-b", "feature", dir.join("origin.git").to_str().unwrap(), "lib"]);
        let fleet = dir.join("worker/fleet/hub");
        let t = LinkTransfer { socket: link(&dir, &fleet) };
        let repos = dir.join("hub-data/repos");

        let got = prepare(&t, &repos, &fleet, &cfg, "web", Some("feature")).unwrap();
        let tree = got.remote_root.parent().unwrap().to_path_buf();
        assert_eq!(got.remote_root, tree.join("cfg"));
        assert_eq!(tree.parent().unwrap(), fleet.join("roots"));
        assert_eq!(
            got.folders,
            [(dir.join("web"), tree.join("web"), "feature".to_string()), (dir.join("lib"), tree.join("lib"), "feature".to_string())]
        );
        // `folder = "../web"` resolves from the moved root to the moved checkout.
        let moved = got.remote_root.join("../web").canonicalize().unwrap();
        assert_eq!(moved, tree.join("web").canonicalize().unwrap());
        assert_eq!(std::fs::read_to_string(moved.join("F")).unwrap(), "feature\n");
        assert_eq!(git(&moved, &["symbolic-ref", "--short", "HEAD"]), "feature");
        assert!(got.remote_root.join("dockerfiles/web.Dockerfile").exists());
        assert!(!got.remote_root.join(".worktrees").exists() && !got.remote_root.join(".git").exists());
        // The hub's cache: one bare repo for the one origin.
        let caches: Vec<_> = std::fs::read_dir(&repos).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(caches.len(), 1, "{caches:?}");
        assert!(caches[0].ends_with(".git"));

        // No `--branch`: the hub checkout's own (`main`); same root, kept.
        let again = prepare(&t, &repos, &fleet, &cfg, "web", None).unwrap();
        assert_eq!(again.remote_root, got.remote_root);
        assert_eq!(again.folders[0].2, "main");
        assert_eq!(git(&moved, &["rev-parse", "main"]), git(&dir.join("origin.git"), &["rev-parse", "main"]));
        // A requested branch origin lacks: the checkout's own.
        assert_eq!(prepare(&t, &repos, &fleet, &cfg, "web", Some("new-one")).unwrap().folders[0].2, "main");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_transfer_fails_prepare() {
        struct Refuse(Mutex<Vec<String>>);
        impl Transfer for Refuse {
            fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()> {
                std::io::copy(body, &mut std::io::sink())?;
                self.0.lock().unwrap().push(header.kind.clone());
                bail!("transfer refused: full")
            }
        }
        let dir = scratch("refused");
        let cfg = fixture(&dir);
        let t = Refuse(Mutex::default());
        match prepare(&t, &dir.join("repos"), &dir.join("fleet"), &cfg, "web", None) {
            Err(Unprepared::Failed(e)) => assert!(format!("{e:#}").contains("cannot send the config root: transfer refused: full")),
            other => panic!("{other:?}"),
        }
        assert_eq!(*t.0.lock().unwrap(), ["root-tar"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
