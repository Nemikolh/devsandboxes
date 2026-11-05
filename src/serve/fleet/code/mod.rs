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
//! - **haves** ([`Transfer::haves`]): what the worker has already under its
//!   fleet root for this host, one `code.haves` ([`haves_of`]): the roots
//!   unpacked there and the tips of its cache of each repo.
//! - **send** ([`send`]): the tar unless the worker has it, then each
//!   repo's bundle `--not` its tips (0 bytes when it has everything), one
//!   `TRANSFER` each, through the worker's link ([`LinkTransfer`]).
//! - **receive** ([`receivers`], on the worker): `root-tar` unpacked into
//!   `<fleet root>/roots/<hash>/`; `bundle` into the repo's cache
//!   `<fleet root>/repos/<repo id>.git`, then a checkout below the root cut
//!   from it.
//!
//! The layout keeps every `folder` where the config says, relative to the
//! root: the tar's members sit under the root's last components on the hub
//! (as many as the folders climb with `..`, [`layout`]), so `../web` from
//! `/w/cfg` is `<hash>/cfg` and `<hash>/web` on the worker.
//! [`prepare`] is offer + send.

pub mod repo;
pub mod tar;
pub mod wip;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use self::repo::Repo;
use super::transfer::{self, Header, Received, Receivers};
use crate::serve::api::ApiError;
use crate::config::{Build, Config, MountContext, Placement, ResolvedSandbox};
use crate::worker_policy::{self, Allow, Policy, Refusal};

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

/// The API call a peer asks a worker's haves with (docs/reference/api.md,
/// `code.haves`), answered by [`haves_call`].
pub const HAVES_METHOD: &str = "code.haves";
/// The most repos one `code.haves` asks about.
pub const HAVES_REPOS_MAX: usize = 32;
/// The most roots `code.haves` answers (by name): one past it is only sent
/// again, and kept.
pub const ROOTS_CAP: usize = 1024;
/// How long [`LinkTransfer::haves`] waits for the worker's hello, then for
/// its answer: listing a dir and each cache's tips (at most
/// `repo::HAVES_CAP`) is quick.
pub const HAVES_HELLO_WAIT: Duration = Duration::from_secs(5);
pub const HAVES_WAIT: Duration = Duration::from_secs(30);
/// How long one write of a transfer's body may block: the link's flow
/// control holds it while the worker writes the body out, never this long
/// unless the link is stuck.
pub const SEND_WRITE_WAIT: Duration = Duration::from_secs(5 * 60);
/// How long a sender waits for the worker's answer once the body is sent:
/// the worker takes a bundle into its cache and cuts the checkout before it
/// answers, each git step bounded by `repo::WHOLE`; a landing that takes
/// longer than one such step and some is treated as stuck (the next
/// `prepare` sends again).
pub const ANSWER_WAIT: Duration = Duration::from_secs(35 * 60);

/// What the receiver has already, under its fleet root for the sender: the
/// `code.haves` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Haves {
    /// Root hashes unpacked under `roots/` (at most [`ROOTS_CAP`]).
    #[serde(default)]
    pub roots: BTreeSet<String>,
    /// Per repo identity asked about, its cache's tips (`repo::tips`); a
    /// repo with no cache (or an empty one) is absent.
    #[serde(default)]
    pub tips: BTreeMap<String, Vec<String>>,
}

/// The seam bodies cross: production sends through a worker's link
/// ([`LinkTransfer`]); another implementation (a sync layer) replaces it
/// without changing [`prepare`]'s callers. Fetch-back (`pull`) joins it
/// in fleet step 12e.
pub trait Transfer {
    /// What the receiver has already, with the tips of its cache of each of
    /// `repos` (repo identities). The default: nothing, so everything is
    /// sent.
    fn haves(&self, repos: &[String]) -> Result<Haves> {
        let _ = repos;
        Ok(Haves::default())
    }
    /// One transfer: `header`, then `body`; the receiver's refusal is the
    /// error.
    fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()>;
}

/// [`Transfer`] over a worker's link: each send one `TRANSFER` stream
/// through its link's socket, else its route socket
/// (`fleet::route::connect`).
pub struct LinkTransfer {
    /// The socket dir.
    pub dir: PathBuf,
    pub worker: String,
}

impl LinkTransfer {
    /// `worker`, its sockets in the socket dir `dir`.
    pub fn for_worker(dir: &Path, worker: &str) -> LinkTransfer {
        LinkTransfer { dir: dir.to_path_buf(), worker: worker.to_string() }
    }
}

impl Transfer for LinkTransfer {
    /// `code.haves` over a fresh `API` stream of the same path (hello, then
    /// the call), each step bounded ([`HAVES_HELLO_WAIT`], [`HAVES_WAIT`]).
    fn haves(&self, repos: &[String]) -> Result<Haves> {
        use crate::serve::machines::{link, ssh::Duplex};
        let stream = super::route::connect(&self.dir, &self.worker, crate::devsbd::proto::channel::API)
            .with_context(|| format!("cannot reach the worker `{}`", self.worker))?;
        stream.set_write_timeout(Some(HAVES_WAIT)).context("cannot set a write timeout")?;
        let mut link = link::handshake(&self.worker, Duplex::socket(stream), HAVES_HELLO_WAIT, link::Over::Link)
            .map_err(|e| anyhow!("{e}"))?;
        let r = link.conn.call_within(HAVES_METHOD, json!({ "repos": repos }), HAVES_WAIT)?;
        let r = r.with_context(|| format!("no `{HAVES_METHOD}` answer within {}s", HAVES_WAIT.as_secs()))?;
        if let Some(e) = r.error {
            bail!("{HAVES_METHOD}: {}", e.message);
        }
        serde_json::from_value(r.result.unwrap_or(Value::Null)).with_context(|| format!("bad {HAVES_METHOD} answer"))
    }

    fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()> {
        let stream = super::route::connect(&self.dir, &self.worker, crate::devsbd::proto::channel::TRANSFER)
            .with_context(|| format!("cannot reach the worker `{}`", self.worker))?;
        stream.set_write_timeout(Some(SEND_WRITE_WAIT)).context("cannot set a write timeout")?;
        stream.set_read_timeout(Some(ANSWER_WAIT)).context("cannot set a read timeout")?;
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
    /// The repo's identity (`repo::identity` of the branch): which of the
    /// worker's caches it goes into.
    pub id: String,
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
    /// what it has checked out and gets that branch updated,
    /// `repo::land_from`).
    pub folders: Vec<(PathBuf, PathBuf, String)>,
}

/// What sandbox `sandbox` of `config_root` needs on a worker, its repos
/// fetched into the hub's cache under `repos` (`<data>/devsandbox/repos`).
/// `branch` (a `run --branch`) is the `folder`'s branch when its origin
/// has it; else, and for `folders` entries, the checkout's current branch.
/// `folders` entries inside the config root travel in its tar (their
/// `.git` doesn't); one with a `${…}` variable doesn't move yet; nor does
/// a sandbox with [`ties`].
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
    let tied = ties(&root, &config, &resolved);
    if !tied.is_empty() {
        return not_movable(tied.join(", "));
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
        let id = repo::identity(Repo::Bare(&cache), &format!("refs/heads/{branch}"))?;
        repos_out.push(RepoOffer { host: host.clone(), at: at.clone(), cache, branch, id });
    }
    let tar = pack_root(&root, &layout.root)?;
    let hash = short_hash(&tar);
    Ok(Offer { tar, hash, layout, repos: repos_out })
}

/// [`ties`] of sandbox `sandbox` of the config at `config_root`; a config
/// that doesn't load or resolve is the error (the run reports it itself).
pub fn ties_of(config_root: &Path, sandbox: &str) -> Result<Vec<String>> {
    let root = config_root.canonicalize().with_context(|| format!("config root {} does not exist", config_root.display()))?;
    let config = Config::load(&root)?;
    let resolved = config.resolve_sandbox(sandbox)?;
    Ok(ties(&root, &config, &resolved))
}

/// What ties `sandbox` of `config` (at the canonical config root `root`) to
/// this host, for the user (fleet plan decision 40): first the pin
/// (`placement = "home"`), then the inferred ties, what a
/// worker policy with no allowances would refuse, over the config root and
/// the folders that move with it (`folder`, `folders` outside the root) as
/// the only paths that travel. From the config ([`Policy::config_refusals`]:
/// `initializeCommand`, `caches`, `privileged`, global services,
/// `${localEnv:…}`, …) and each `mounts` bind ([`Policy::check_bind`]:
/// sources outside those paths, `${sharedVolumes}` ones, which never travel
/// ([`ROOT_SKIPS`]), runtime sockets) and each `build` context or
/// dockerfile, the sandbox's or a service's. Any tie keeps the sandbox home,
/// whatever a worker's trust or allowances. Folders that can't move are
/// [`offer`]'s to say (they need git).
pub fn ties(root: &Path, config: &Config, sandbox: &ResolvedSandbox) -> Vec<String> {
    let props = &sandbox.properties;
    let at = |f: &str| {
        let p = root.join(f);
        worker_policy::resolve(&p).unwrap_or(p)
    };
    let main = props.folder.as_deref().map(at);
    let mut moved: Vec<PathBuf> = main.iter().cloned().collect();
    moved.extend(props.folders.iter().flat_map(|f| f.values()).filter(|e| !e.path().contains("${")).map(|e| at(e.path())));
    moved.retain(|f| !f.starts_with(root));
    // The moved folders as `binds`: paths under them travel as the root does.
    let policy = Policy {
        root: root.to_path_buf(),
        data: root.to_path_buf(),
        allow: Allow { binds: moved, ..Allow::default() },
        sockets: worker_policy::host_sockets(),
    };
    let mut out: Vec<String> = Vec::new();
    if props.placement == Some(Placement::Home) {
        out.push("`placement = \"home\"`".into());
    }
    out.extend(policy.config_refusals(root, config, sandbox).into_iter().map(tie));
    let root_s = root.to_string_lossy();
    let shared = root.join("shared-volumes");
    let shared_s = shared.to_string_lossy();
    let folder_s = main.as_deref().map(|m| m.to_string_lossy().into_owned()).unwrap_or_default();
    let basename = main.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ctx = MountContext {
        config_dir: &root_s,
        workspace_folder: &folder_s,
        workspace_folder_basename: &basename,
        shared_volumes: &shared_s,
        instance: "instance",
        container_env: None,
    };
    for mount in props.mounts.iter().flatten() {
        // A bad mount is the run's to report.
        let Ok(m) = mount.resolve(&ctx) else { continue };
        let Some(source) = m.source.as_deref().filter(|_| m.kind == "bind") else { continue };
        // `${localEnv:…}` is counted above; what's still a variable (or
        // relative) isn't a host path to judge.
        if source.contains("${") || !Path::new(source).is_absolute() || reads_local_env(mount) {
            continue;
        }
        if let Err(r) = policy.check_bind(source) {
            out.push(tie(r));
        }
    }
    // Build paths, the sandbox's and its services' (both relative to the
    // config dir, as `services::build_args` takes them); a missing one is
    // the build's to report.
    let services: Vec<Build> = props.services.iter().flatten().filter_map(|n| config.resolve_service(n).ok()?.spec.build).collect();
    for build in props.build.iter().chain(&services) {
        let paths = [("build context", build.context.as_deref().or(Some("."))), ("dockerfile", build.dockerfile.as_deref())];
        for (what, rel) in paths {
            let Some(path) = rel.map(|r| root.join(r)).filter(|p| p.exists()) else { continue };
            if let Err(r) = policy.check_path(&path, what) {
                out.push(tie(r));
            }
        }
    }
    out.dedup();
    out
}

/// A mount whose raw source reads `${localEnv:…}`: [`ties`] counts it once,
/// as that variable.
fn reads_local_env(mount: &crate::config::Mount) -> bool {
    let raw = match mount {
        crate::config::Mount::Shorthand(s) => s.as_str(),
        crate::config::Mount::Object(o) => o.source.as_deref().unwrap_or_default(),
    };
    raw.contains("${localEnv:")
}

/// A refusal as a tie: what it names, with the policy's root called what it
/// is here.
fn tie(r: Refusal) -> String {
    r.what.replace("(outside the fleet root)", "(outside the config root)")
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

/// Send `offer` through `t`: first its haves (one ask, every repo's
/// identity), then the root's tar unless the worker has it, then each
/// repo's branch as a bundle `--not` the worker's tips of that repo
/// (written to a temp file in the cache's parent first; a body of 0 bytes
/// when the worker has every commit), its header naming the repo and the
/// branch's commit; where it all is on the worker whose fleet root is
/// `worker_root`.
pub fn send(offer: &Offer, worker_root: &Path, t: &dyn Transfer) -> Result<Prepared> {
    let ids: Vec<String> = offer.repos.iter().map(|r| r.id.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let haves = t.haves(&ids).context("cannot ask the worker what it has")?;
    if !haves.roots.contains(&offer.hash) {
        let (size, sha256) = transfer::digest(&offer.tar[..])?;
        let name = format!("root-{}.tar", offer.hash);
        let header = Header { kind: ROOT_TAR_KIND.into(), name, size, sha256, to: None, repo: None, refs: None };
        t.send(&header, &mut &offer.tar[..]).context("cannot send the config root")?;
    }
    let tree = worker_root.join("roots").join(&offer.hash);
    let mut folders = Vec::new();
    for r in &offer.repos {
        let full = format!("refs/heads/{}", r.branch);
        let commit = repo::commit_at(Repo::Bare(&r.cache), &full)?.with_context(|| format!("{} has no {full}", r.cache.display()))?;
        let dir = r.cache.parent().context("a repo cache has a parent")?;
        let file = TempFile(dir.join(format!(".bundle-{}-{}.part", std::process::id(), next_seq())));
        let tips = haves.tips.get(&r.id).map(Vec::as_slice).unwrap_or_default();
        let new = repo::bundle_refs(Repo::Bare(&r.cache), &[&full], tips, &file.0)?;
        let open = || std::fs::File::open(&file.0).with_context(|| format!("cannot read {}", file.0.display()));
        let mut body: Box<dyn Read> = if new { Box::new(open()?) } else { Box::new(std::io::empty()) };
        let (size, sha256) = if new { transfer::digest(open()?)? } else { transfer::digest(std::io::empty())? };
        let to = r.at.to_str().context("a folder's name isn't UTF-8")?;
        let header = Header {
            kind: BUNDLE_KIND.into(),
            name: "repo.bundle".into(),
            size,
            sha256,
            to: Some(format!("{}/{to}", offer.hash)),
            repo: Some(r.id.clone()),
            refs: Some([(full, commit)].into()),
        };
        t.send(&header, &mut body).with_context(|| format!("cannot send {}", r.host.display()))?;
        folders.push((r.host.clone(), tree.join(&r.at), r.branch.clone()));
    }
    Ok(Prepared { remote_root: tree.join(&offer.layout.root), folders })
}

/// `code.haves {repos}` from a peer whose fleet root here is `fleet_root`
/// (the daemon's `Table::Peer`): [`haves_params`], then [`haves_of`].
pub fn haves_call(fleet_root: &Path, params: &Value) -> Result<Value, ApiError> {
    let repos = haves_params(params)?;
    let haves = haves_of(fleet_root, &repos).map_err(ApiError::internal)?;
    serde_json::to_value(haves).map_err(|e| ApiError::internal(e.into()))
}

/// `code.haves`' `repos`: at most [`HAVES_REPOS_MAX`] repo identities
/// (`repo::check_id`).
pub fn haves_params(params: &Value) -> Result<Vec<String>, ApiError> {
    let Some(list) = params.get("repos").and_then(Value::as_array) else {
        return Err(ApiError::invalid(format!("`{HAVES_METHOD}` needs `repos`, a list of repo identities")));
    };
    if list.len() > HAVES_REPOS_MAX {
        return Err(ApiError::invalid(format!("`{HAVES_METHOD}` asks about at most {HAVES_REPOS_MAX} repos")));
    }
    list.iter()
        .map(|v| {
            let id = v.as_str().ok_or_else(|| ApiError::invalid("`repos` holds repo identities (strings)"))?;
            repo::check_id(id).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
            Ok(id.to_string())
        })
        .collect()
}

/// What fleet root `fleet_root` has: the root hashes unpacked under
/// `roots/` (real dirs named as one, sorted, at most [`ROOTS_CAP`]) and,
/// for each of `repos` with a cache under `repos/`, its tips.
pub fn haves_of(fleet_root: &Path, repos: &[String]) -> Result<Haves> {
    let mut out = Haves::default();
    let roots = fleet_root.join("roots");
    match std::fs::read_dir(&roots) {
        Ok(entries) => {
            let mut names: Vec<String> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| is_hash(n))
                .collect();
            names.sort_unstable();
            names.truncate(ROOTS_CAP);
            out.roots = names.into_iter().collect();
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow!(e).context(format!("cannot list {}", roots.display()))),
    }
    for id in repos {
        repo::check_id(id)?;
        let cache = cache_path(fleet_root, id);
        if cache.symlink_metadata().is_ok_and(|m| m.is_dir()) && cache.join("HEAD").is_file() {
            let tips = repo::tips(Repo::Bare(&cache))?;
            if !tips.is_empty() {
                out.tips.insert(id.clone(), tips);
            }
        }
    }
    Ok(out)
}

/// Fleet root `fleet_root`'s cache of repo `id`: `repos/<id>.git`.
pub fn cache_path(fleet_root: &Path, id: &str) -> PathBuf {
    fleet_root.join("repos").join(format!("{id}.git"))
}

/// [`cache_path`], made a bare repo if it isn't one: `repos/` and the cache
/// real dirs, never through a symlink.
fn make_cache(fleet_root: &Path, id: &str) -> Result<PathBuf> {
    repo::check_id(id)?;
    let cache = cache_path(fleet_root, id);
    real_dir(cache.parent().context("a cache has a parent")?)?;
    real_dir(&cache)?;
    repo::ensure_bare(&cache)?;
    Ok(cache)
}

/// `dir` made if missing (its parent must exist); refused unless it's a
/// real dir (a symlink is refused, whatever it points at).
fn real_dir(dir: &Path) -> Result<()> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(anyhow!(e).context(format!("cannot create {}", dir.display()))),
    }
    if !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        bail!("{} isn't a directory", dir.display());
    }
    Ok(())
}

/// 16 lowercase hex: a root hash's (and a repo identity's) shape.
fn is_hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
/// - `bundle` with `to` = `<hash>/<path>`, `<hash>` a root unpacked here,
///   `repo` its repo identity and `refs` one `refs/heads/<branch>` and at
///   most its WIP ref (`refs/devsandbox/wip/<branch>`, [`bundle_refs_of`]):
///   its objects into the cache `repos/<repo>.git` (made, bare, if missing;
///   a 0-byte body brings none), the refs set there (each a commit the
///   cache now has, `repo::set_refs`), then the checkout at
///   `roots/<hash>/<path>` cut from the cache ([`repo::land_from`]: a new
///   one gets the WIP applied), every dir on the way a real one (no
///   symlink).
///
/// Bodies wait in `roots/.in/`; the unpacking's error is the transfer's
/// answer, so the hub hears it.
pub fn receivers(fleet_root: &Path, receivers: Receivers) -> Receivers {
    let roots = fleet_root.join("roots");
    let (tar_in, tar_roots, bundle_in, bundle_roots) = (roots.clone(), roots.clone(), roots.clone(), roots);
    let fleet_root = fleet_root.to_path_buf();
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
                bundle_refs_of(h)?;
                if !bundle_in.join(hash).is_dir() {
                    bail!("root {hash} isn't here (its tar goes first)");
                }
                // Unique: two empty bodies share their sha256.
                Ok(bundle_in.join(".in").join(format!("{}-{}.bundle", h.sha256, repo::nonce())))
            },
            |_| {},
        )
        .then(BUNDLE_KIND, move |r: &Received| {
            let landed = (|| {
                let (hash, rest) = bundle_target(&r.header)?;
                let (id, branch, wip) = bundle_refs_of(&r.header)?;
                let tree = bundle_roots.join(hash);
                let mut at = tree.clone();
                for part in rest.split('/') {
                    at.push(part);
                    if at.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
                        bail!("{} isn't a directory", at.display());
                    }
                }
                let cache = make_cache(&fleet_root, id)?;
                if r.header.size > 0 {
                    repo::unbundle(&cache, &r.path)?;
                }
                repo::set_refs(&cache, r.header.refs.as_ref().context("a bundle names its refs")?)?;
                repo::land_from(&cache, &at, branch, wip)
            })();
            let _ = std::fs::remove_file(&r.path);
            landed
        })
}

/// A bundle header's `to`, as `(root hash, path below it)`.
fn bundle_target(h: &Header) -> Result<(&str, &str)> {
    let to = h.to.as_deref().context("a bundle names where it goes (`to`)")?;
    let (hash, rest) = to.split_once('/').with_context(|| format!("a bundle goes below a root, not to {to}"))?;
    if !is_hash(hash) {
        bail!("`{hash}` isn't a root hash");
    }
    Ok((hash, rest))
}

/// A bundle header's `repo` and `refs` (both checked by
/// `transfer::check_header`), as `(repo id, branch, its WIP commit)`:
/// exactly one `refs/heads/<branch>`, besides it at most
/// `refs/devsandbox/wip/<branch>` (the same branch's), nothing else.
fn bundle_refs_of(h: &Header) -> Result<(&str, &str, Option<&str>)> {
    let id = h.repo.as_deref().context("a bundle names its repo (`repo`)")?;
    let refs = h.refs.as_ref().context("a bundle names its refs (`refs`)")?;
    let mut branches = refs.keys().filter_map(|n| n.strip_prefix("refs/heads/"));
    let (Some(branch), None) = (branches.next(), branches.next()) else {
        bail!("a bundle carries one branch (`refs/heads/…`), not {}", refs.keys().cloned().collect::<Vec<_>>().join(", "));
    };
    let wip = repo::wip_ref(branch);
    if let Some(other) = refs.keys().find(|n| !n.starts_with("refs/heads/") && **n != wip) {
        bail!("a bundle carries its branch and that branch's WIP ref ({wip}), not {other}");
    }
    Ok((id, branch, refs.get(&wip).map(String::as_str)))
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
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::repo::tests::{git, origin_and_checkout, scratch};
    use super::*;
    use crate::serve::endpoint;

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
        Header { kind: ROOT_TAR_KIND.into(), name: format!("root-{}.tar", short_hash(tar)), size, sha256, to: None, repo: None, refs: None }
    }

    /// A `bundle` header for `body` into `to`, of repo `repo` with `refs`.
    fn bundle_header(body: &[u8], to: &str, repo: &str, refs: &[(&str, &str)]) -> Header {
        let (size, sha256) = transfer::digest(body).unwrap();
        Header {
            kind: BUNDLE_KIND.into(),
            name: "repo.bundle".into(),
            size,
            sha256,
            to: Some(to.into()),
            repo: Some(repo.into()),
            refs: Some(refs.iter().map(|(n, id)| (n.to_string(), id.to_string())).collect()),
        }
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
        let oid = "a".repeat(40);
        let h = bundle_header(b"x", "0123456789abcdef/web", "fedcba9876543210", &[("refs/heads/main", &oid)]);
        let err = format!("{:#}", deliver(&r, &h, b"x").unwrap_err());
        assert!(err.contains("root 0123456789abcdef isn't here"), "{err}");
        // Its repo and refs: one branch, at most that branch's WIP ref.
        let err = format!("{:#}", deliver(&r, &Header { repo: None, ..h.clone() }, b"x").unwrap_err());
        assert!(err.contains("names its repo"), "{err}");
        let err = format!("{:#}", deliver(&r, &Header { refs: None, ..h.clone() }, b"x").unwrap_err());
        assert!(err.contains("names its refs"), "{err}");
        for refs in [
            vec![("refs/heads/main", oid.as_str()), ("refs/heads/b", oid.as_str())],
            vec![("refs/devsandbox/wip/main", oid.as_str())],
        ] {
            let bad = bundle_header(b"x", "0123456789abcdef/web", "fedcba9876543210", &refs);
            let err = format!("{:#}", deliver(&r, &bad, b"x").unwrap_err());
            assert!(err.contains("carries one branch"), "{refs:?}: {err}");
        }
        for other in ["refs/devsandbox/wip/b", "refs/tags/v1", "refs/devsandbox/root"] {
            let bad = bundle_header(b"x", "0123456789abcdef/web", "fedcba9876543210", &[("refs/heads/main", &oid), (other, &oid)]);
            let err = format!("{:#}", deliver(&r, &bad, b"x").unwrap_err());
            assert!(err.contains(&format!("not {other}")), "{other}: {err}");
        }
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

    #[test]
    fn ties_name_what_keeps_a_sandbox_home() {
        let dir = scratch("ties");
        let cfg = dir.join("cfg");
        for d in ["cfg/data", "web/sub", "lib", "elsewhere"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let ties_of_toml = |extra: &str| {
            let text = format!(
                "[services.g]\nimage = \"redis\"\nscope = \"global\"\n[services.i]\nimage = \"redis\"\n\
                 [sandbox.s]\nfolder = \"../web\"\nimage = \"alpine\"\nfolders = {{ \"/w/lib\" = \"../lib\" }}\n{extra}\n"
            );
            let config = Config::parse(&text).unwrap();
            let s = config.resolve_sandbox("s").unwrap();
            ties(&cfg, &config, &s)
        };
        // What travels (the root, the moved folders) and what isn't a bind.
        let web = dir.join("web").display().to_string();
        let lib = dir.join("lib").display().to_string();
        let clean = format!(
            "services = [\"i\"]\nmounts = [\"source=${{configDir}}/data,target=/d,type=bind\", \
             \"source=${{localWorkspaceFolder}}/sub,target=/s\", \"source={web}/sub,target=/w\", \
             \"source={lib},target=/l\", \"source=v-${{instance}},target=/v,type=volume\", \
             {{ type = \"tmpfs\", target = \"/t\" }}]"
        );
        assert_eq!(ties_of_toml(&clean), Vec::<String>::new());
        // A build inside the root isn't a tie; one whose context climbs out is.
        std::fs::write(cfg.join("Dockerfile"), "FROM alpine\n").unwrap();
        assert_eq!(ties_of_toml("build = { dockerfile = \"Dockerfile\" }"), Vec::<String>::new());
        assert_eq!(
            ties_of_toml("build = { dockerfile = \"Dockerfile\", context = \"../elsewhere\" }"),
            [format!("build context `{}` (outside the config root)", cfg.join("../elsewhere").display())]
        );
        // The pin: a tie on its own, and first among others.
        assert_eq!(ties_of_toml("placement = \"home\""), ["`placement = \"home\"`"]);
        assert_eq!(ties_of_toml("privileged = true\nplacement = \"home\""), ["`placement = \"home\"`", "privileged"]);
        assert_eq!(ties_of_toml(""), Vec::<String>::new());
        let out = dir.join("elsewhere").display().to_string();
        for (extra, want) in [
            (format!("mounts = [\"source={out},target=/o\"]"), format!("bind mount of `{out}` (outside the config root)")),
            (
                "mounts = [{ source = \"${sharedVolumes}/x\", target = \"/x\" }]".into(),
                format!("shared-volumes mount `{}`", cfg.join("shared-volumes/x").display()),
            ),
            (
                "mounts = [\"source=/var/run/docker.sock,target=/s\"]".into(),
                "runtime socket mount `/var/run/docker.sock`".into(),
            ),
            ("containerEnv = { T = \"${localEnv:X}\" }".into(), "`${localEnv:X}` (this host's env)".into()),
            ("caches = [\"cargo\"]".into(), "`caches`".into()),
            ("privileged = true".into(), "privileged".into()),
            ("initializeCommand = \"true\"".into(), "`initializeCommand`".into()),
            ("services = [\"i\", \"g\"]".into(), "global service `g`".into()),
        ] {
            assert_eq!(ties_of_toml(&extra), [want], "{extra}");
        }
        // All of them, a `${localEnv:…}` mount counted once, as its variable.
        let all = ties_of_toml(&format!(
            "privileged = true\ncaches = [\"cargo\"]\nmounts = [\"source=${{localEnv:HOME}}/.gh,target=/g\", \"source={out},target=/o\"]"
        ));
        assert_eq!(
            all,
            [
                "`caches`".to_string(),
                "privileged".into(),
                "`${localEnv:HOME}` (this host's env)".into(),
                format!("bind mount of `{out}` (outside the config root)"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offer_refuses_a_tied_sandbox() {
        let dir = scratch("offer-tied");
        let cfg = fixture(&dir);
        std::fs::write(cfg.join("devsandboxes.toml"), "[sandbox.web]\nfolder = \"../web\"\nimage = \"i\"\nprivileged = true\ncaches = [\"npm\"]\n")
            .unwrap();
        match offer(&dir.join("repos"), &cfg, "web", None) {
            Err(Unprepared::NotMovable(why)) => assert_eq!(why, "`caches`, privileged"),
            other => panic!("{other:?}"),
        }
        assert_eq!(ties_of(&cfg, "web").unwrap(), ["`caches`", "privileged"]);
        assert!(ties_of(&cfg, "nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The machine's end of one `API` stream, as the daemon answers a peer
    /// whose fleet root is `fleet`: a hello, then `code.haves`
    /// ([`haves_call`]); anything else unknown. Reads bounded.
    fn serve_api(stream: UnixStream, fleet: &Path) {
        use std::io::{BufRead, BufReader, Write};

        use crate::serve::proto::{HelloResult, PROTOCOL, Request, Response};
        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut out = stream;
        let mut line = String::new();
        while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
            let req: Request = serde_json::from_str(&line).unwrap();
            let resp = match req.method.as_str() {
                "hello" => {
                    let hello = HelloResult { version: "0.0.0".into(), build: 0, protocol: PROTOCOL, handoff: false };
                    Response::ok(req.id, serde_json::to_value(hello).unwrap())
                }
                HAVES_METHOD => match haves_call(fleet, &req.params) {
                    Ok(v) => Response::ok(req.id, v),
                    Err(e) => Response::err(req.id, e.code, e.message),
                },
                m => Response::err(req.id, "unknown-method", m.to_string()),
            };
            if writeln!(out, "{}", serde_json::to_string(&resp).unwrap()).is_err() {
                return;
            }
            line.clear();
        }
    }

    /// A link's two ends in-process (this host the opener, the machine the
    /// answerer), the machine's receivers [`receivers`] into `fleet` and its
    /// `API` [`serve_api`]; this host's link socket.
    fn link(dir: &Path, fleet: &Path) -> PathBuf {
        use crate::devsbd::proto::channel;
        use crate::serve::fleet::link::{Endpoint, Serves, answerer_with, opener_with};
        let hub_dir = dir.join("hub");
        std::fs::create_dir_all(&hub_dir).unwrap();
        let socket = endpoint::fleet_socket(&hub_dir, "w");
        let ours = Serves::default().transfers(Arc::new(Receivers::new(|_| {})));
        let api_fleet = fleet.to_path_buf();
        let api = move || {
            let (mine, theirs) = UnixStream::pair().ok()?;
            let fleet = api_fleet.clone();
            std::thread::spawn(move || serve_api(mine, &fleet));
            Some(theirs.into())
        };
        let theirs = Serves::default().with(channel::API, api).transfers(Arc::new(receivers(fleet, Receivers::new(|_| {}))));
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        let path = socket.clone();
        std::thread::spawn(move || {
            opener_with(&super::super::LINK, o_r, o_w, Endpoint { socket: Some(&path), serves: ours, outlet: None, auth: None }, |_| Ok(()))
        });
        std::thread::spawn(move || {
            answerer_with(&super::super::LINK, a_r, a_w, Endpoint { serves: theirs, ..Endpoint::default() }, |_| Ok(()), || {})
        });
        let start = std::time::Instant::now();
        while UnixStream::connect(&socket).is_err() {
            assert!(start.elapsed() < Duration::from_secs(5), "the link socket accepts");
            std::thread::sleep(Duration::from_millis(10));
        }
        socket
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
        let t = LinkTransfer::for_worker(link(&dir, &fleet).parent().unwrap(), "w");
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

    /// A [`Transfer`] recording each send's kind and size, passing it on.
    struct Recorded<'a> {
        inner: &'a dyn Transfer,
        sent: Mutex<Vec<(String, u64)>>,
    }

    impl Transfer for Recorded<'_> {
        fn haves(&self, repos: &[String]) -> Result<Haves> {
            self.inner.haves(repos)
        }
        fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()> {
            self.sent.lock().unwrap().push((header.kind.clone(), header.size));
            self.inner.send(header, body)
        }
    }

    impl Recorded<'_> {
        fn take(&self) -> Vec<(String, u64)> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }

    #[test]
    fn repeat_sends_carry_only_what_the_worker_lacks() {
        let dir = scratch("repeat");
        let cfg = fixture(&dir);
        let fleet = dir.join("worker/fleet/hub");
        let link = LinkTransfer::for_worker(link(&dir, &fleet).parent().unwrap(), "w");
        let t = Recorded { inner: &link, sent: Mutex::default() };
        let repos = dir.join("hub-data/repos");
        let origin = dir.join("origin.git");
        let id = repo::identity(Repo::Bare(&origin), "refs/heads/main").unwrap();
        // Nothing there yet: no roots, no tips.
        assert_eq!(link.haves(std::slice::from_ref(&id)).unwrap(), Haves::default());

        let got = prepare(&t, &repos, &fleet, &cfg, "web", Some("feature")).unwrap();
        let first = t.take();
        assert_eq!(first.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["root-tar", "bundle"]);
        let checkout = got.folders[0].1.clone();
        assert_eq!(git(&checkout, &["symbolic-ref", "--short", "HEAD"]), "feature");
        assert_eq!(git(&checkout, &["remote"]), "", "no remote on the worker");
        let cache = cache_path(&fleet, &id);
        assert_eq!(git(&cache, &["rev-parse", "refs/heads/feature"]), git(&origin, &["rev-parse", "feature"]));
        // The worker answers its root and the cache's tips, over the link.
        let haves = link.haves(std::slice::from_ref(&id)).unwrap();
        let hash = got.remote_root.parent().unwrap().file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(haves.roots, [hash].into());
        assert_eq!(haves.tips[&id], [git(&origin, &["rev-parse", "feature"])]);

        // A new commit on origin: only the bundle goes, and only the delta.
        let seed = dir.join("seed");
        std::fs::write(seed.join("F"), "feature 2\n").unwrap();
        git(&seed, &["commit", "-q", "-am", "two"]);
        git(&seed, &["push", "-q", origin.to_str().unwrap(), "feature"]);
        prepare(&t, &repos, &fleet, &cfg, "web", Some("feature")).unwrap();
        let second = t.take();
        assert_eq!(second.len(), 1, "the root isn't sent again: {second:?}");
        assert!(second[0].1 > 0 && second[0].1 < first[1].1, "a delta: {second:?} after {first:?}");
        assert_eq!(std::fs::read_to_string(checkout.join("F")).unwrap(), "feature 2\n", "fast-forwarded");

        // Nothing new: an empty bundle, still answered `ok`.
        prepare(&t, &repos, &fleet, &cfg, "web", Some("feature")).unwrap();
        assert_eq!(t.take(), [("bundle".to_string(), 0)]);
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), git(&origin, &["rev-parse", "feature"]));
        assert_eq!(std::fs::read_dir(fleet.join("roots/.in")).unwrap().count(), 0, "no body left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wip_snapshot_lands_in_a_new_checkout_only() {
        let dir = scratch("recv-wip");
        let fleet = dir.join("fleet");
        std::fs::create_dir_all(fleet.join("roots/0123456789abcdef")).unwrap();
        let r = Arc::new(receivers(&fleet, Receivers::new(|_| {})));
        // The sender's checkout: a commit, then uncommitted work.
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("f"), "one\n").unwrap();
        git(&src, &["add", "f"]);
        git(&src, &["commit", "-q", "-m", "one"]);
        std::fs::write(src.join("f"), "changed\n").unwrap();
        std::fs::write(src.join("u"), "untracked\n").unwrap();
        let id = repo::identity(Repo::Checkout(&src), "HEAD").unwrap();
        let wip_ref = repo::wip_ref("main");
        let send = |name: &str, haves: &[String]| {
            let snap = wip::snapshot(&src, &wip_ref).unwrap();
            let file = dir.join(name);
            let body = if repo::bundle_refs(Repo::Checkout(&src), &["refs/heads/main", &wip_ref], haves, &file).unwrap() {
                std::fs::read(&file).unwrap()
            } else {
                Vec::new()
            };
            let h = bundle_header(&body, "0123456789abcdef/a", &id, &[("refs/heads/main", &snap.base), (&wip_ref, &snap.wip)]);
            deliver(&r, &h, &body).unwrap();
            snap
        };
        let first = send("1.bundle", &[]);
        let at = fleet.join("roots/0123456789abcdef/a");
        assert_eq!(git(&at, &["rev-parse", "HEAD"]), first.base);
        assert_eq!(git(&at, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(std::fs::read_to_string(at.join("f")).unwrap(), "changed\n");
        assert_eq!(git(&at, &["status", "--porcelain=v1", "-uall"]), "M f\n?? u");
        assert_eq!(git(&at, &["for-each-ref", "refs/devsandbox"]), "", "the WIP ref isn't kept in the checkout");
        let cache = cache_path(&fleet, &id);
        assert_eq!(git(&cache, &["rev-parse", &wip_ref]), first.wip);

        // Again, more work: the cache gets the new snapshot, the existing
        // checkout's working tree is left alone.
        std::fs::write(src.join("f"), "changed again\n").unwrap();
        let tips = repo::tips(Repo::Bare(&cache)).unwrap();
        let second = send("2.bundle", &tips);
        assert_ne!(second.wip, first.wip);
        assert_eq!(git(&cache, &["rev-parse", &wip_ref]), second.wip);
        assert_eq!(std::fs::read_to_string(at.join("f")).unwrap(), "changed\n");

        // A ref naming a commit neither the body nor the cache has.
        let missing = "1".repeat(40);
        let h = bundle_header(b"", "0123456789abcdef/a", &id, &[("refs/heads/main", &missing)]);
        let err = format!("{:#}", deliver(&r, &h, b"").unwrap_err());
        assert!(err.contains("is neither in the bundle nor already here"), "{err}");
        assert_eq!(git(&cache, &["rev-parse", "refs/heads/main"]), second.base, "no ref moved");
        // The cache is never reached through a symlink.
        let other = dir.join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(&other, cache_path(&fleet, "fedcba9876543210")).unwrap();
        let h = bundle_header(b"", "0123456789abcdef/b", "fedcba9876543210", &[("refs/heads/main", &second.base)]);
        let err = format!("{:#}", deliver(&r, &h, b"").unwrap_err());
        assert!(err.contains("isn't a directory"), "{err}");
        assert_eq!(std::fs::read_dir(&other).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn haves_answer_the_fleet_roots_roots_and_caches() {
        let dir = scratch("haves");
        let fleet = dir.join("fleet");
        assert_eq!(haves_of(&fleet, &[]).unwrap(), Haves::default(), "no fleet root yet");
        for d in ["roots/0123456789abcdef", "roots/.in", "roots/.0123456789abcdef.1-2.part", "roots/not-a-hash"] {
            std::fs::create_dir_all(fleet.join(d)).unwrap();
        }
        std::fs::write(fleet.join("roots/fedcba9876543210"), "a file").unwrap();
        let cache = cache_path(&fleet, "00000000000000aa");
        std::fs::create_dir_all(&cache).unwrap();
        git(&cache, &["init", "-q", "--bare"]);
        let ids = ["00000000000000aa".to_string(), "00000000000000bb".to_string()];
        let got = haves_of(&fleet, &ids).unwrap();
        assert_eq!(got, Haves { roots: ["0123456789abcdef".to_string()].into(), tips: BTreeMap::new() }, "an empty cache has no tips");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        git(&src, &["commit", "-q", "--allow-empty", "-m", "x"]);
        git(&src, &["push", "-q", cache.to_str().unwrap(), "main"]);
        let got = haves_call(&fleet, &json!({ "repos": ids })).unwrap();
        assert_eq!(got, json!({"roots": ["0123456789abcdef"], "tips": {"00000000000000aa": [git(&src, &["rev-parse", "HEAD"])]}}));
        crate::serve::dts::assert_matches("CodeHaves", &got);

        let code = |params: Value| haves_call(&fleet, &params).unwrap_err().code;
        assert_eq!(code(json!({})), "invalid");
        assert_eq!(code(json!({"repos": "x"})), "invalid");
        assert_eq!(code(json!({"repos": [1]})), "invalid");
        assert_eq!(code(json!({"repos": ["../../etc"]})), "invalid");
        assert_eq!(code(json!({"repos": vec!["0123456789abcdef"; HAVES_REPOS_MAX + 1]})), "invalid");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
