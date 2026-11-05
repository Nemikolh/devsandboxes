//! Fetch-back (docs/architecture/daemon/fleet.md, *Fetch-back*; fleet plan
//! decision 76): a placed instance's branch and WIP come back to the host
//! that placed it, as hidden refs in the checkout the placement came from.
//!
//! - **On the worker** ([`fetch_call`], `instances.fetch` from a link
//!   peer): the instance's `folder` working tree snapshotted onto
//!   `refs/devsandbox/wip/<instance_id>` (kept there: the instance's latest
//!   WIP), its branch and that ref bundled `--not` the asker's haves into
//!   `<the asker's fleet root>/out/<token>.bundle`, for the asker to pull
//!   ([`serves`]: only files under its own `out/`, each removed once pulled;
//!   ones older than [`OUT_STALE`] swept when a new one is made).
//! - **On the placing host** ([`fetch_back`], `placements.fetch` from a
//!   local client): the placement's local checkout from `placements.json`
//!   (never from the worker's answer), its tips as the haves, the worker's
//!   answer pulled on a `TRANSFER` stream this host opened
//!   (`Transfer::pull`), its objects unbundled into the checkout and
//!   [`land`]ed as `refs/devsandbox/<machine>/<instance>/heads/<branch>`
//!   and `…/wip`: the user's branches, index, `HEAD` and working tree are
//!   never touched.
//!
//! Only the sandbox's `folder` repo comes back; `folders` entries' work
//! doesn't.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::repo::{self, QUICK, Repo, run};
use super::{BUNDLE_LIMIT, Transfer, wip};
use crate::commands::run::host_git;
use crate::serve::api::ApiError;
use crate::serve::fleet::placements::Entry;
use crate::serve::fleet::transfer::Receivers;
use crate::state::{Instance, State};

/// The worker's half: a link peer asks it about one of its instances.
pub const FETCH_METHOD: &str = "instances.fetch";
/// The placing host's half: a local client asks its daemon to fetch back.
pub const PLACEMENTS_FETCH: &str = "placements.fetch";
/// The pull kind of an `instances.fetch` bundle.
pub const FETCH_KIND: &str = "fetch-bundle";
/// Where the worker leaves bundles for a peer, under its fleet root.
pub const OUT_DIR: &str = "out";
/// An `out/` file this old was never pulled: swept when a new one is made.
pub const OUT_STALE: Duration = Duration::from_secs(60 * 60);
/// How long the asker waits for `instances.fetch`'s answer: a snapshot
/// (`add -A`) and a bundle there, each git step bounded by `repo::WHOLE`
/// (30 min); longer than both and some is stuck.
pub const FETCH_WAIT: Duration = Duration::from_secs(65 * 60);
/// How long `devsandbox fetch` waits for its daemon's `placements.fetch`:
/// [`FETCH_WAIT`], the pull (each read bounded, a big bundle takes a while)
/// and the unbundle here (`repo::WHOLE`).
pub const CLI_WAIT: Duration = Duration::from_secs(4 * 60 * 60);

/// `instances.fetch`'s answer (`InstancesFetched`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fetched {
    /// The repo's identity over the WIP's history (`repo::identity`).
    pub repo: String,
    /// The instance's branch.
    pub branch: String,
    /// `refs/heads/<branch>` and `refs/devsandbox/wip/<instance_id>`, each at
    /// its commit.
    pub refs: BTreeMap<String, String>,
    /// The bundle's name under `out/` to pull, `None` when the asker has
    /// every commit already.
    pub bundle: Option<String>,
}

/// `placements.fetch`'s answer (`PlacementsFetched`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedBack {
    pub machine: String,
    pub instance: String,
    /// The local checkout the refs are in.
    pub folder: PathBuf,
    /// The refs set there, full name → commit.
    pub refs: BTreeMap<String, String>,
    /// The bundle's size in bytes (0: nothing new came).
    pub fetched: u64,
}

/// What a request with `machine`, typed on this host, does about fetch-back
/// before it's forwarded (fleet plan decision 77).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Before {
    /// Not a placement of this host's, or not a request that fetches back.
    Nothing,
    /// `rm`: fetched back first; one that fails refuses the `rm`.
    Required,
    /// `rm --force` (which already discards uncommitted work): none.
    Skipped,
    /// `done` (not `undone`): fetched back first, a failure a warning.
    BestEffort,
}

/// [`Before`] for `method` with `params` (its `force` or `done`) when the
/// instance it names is one this host placed (`placed`).
pub fn before(method: &str, params: &Value, placed: bool) -> Before {
    let flag = |k: &str| params.get(k).and_then(Value::as_bool);
    match method {
        _ if !placed => Before::Nothing,
        "rm" if flag("force") == Some(true) => Before::Skipped,
        "rm" => Before::Required,
        "instances.done" if flag("done") == Some(true) => Before::BestEffort,
        _ => Before::Nothing,
    }
}

/// The instance `name` names in `state`: a state key, else an
/// `instance_id` (one backfilled to its key when empty).
pub fn find_instance<'a>(state: &'a State, name: &str) -> Option<(&'a String, &'a Instance)> {
    state.instances.get_key_value(name).or_else(|| state.instances.iter().find(|(k, i)| id_of(k, i) == name))
}

fn id_of<'a>(key: &'a str, i: &'a Instance) -> &'a str {
    if i.instance_id.is_empty() { key } else { &i.instance_id }
}

/// `instances.fetch {instance, haves}` from a peer whose fleet root here is
/// `fleet_root`, over this host's `state`: [`fetch_of`] the instance.
/// `not-found` for an instance `state` lacks, `invalid` for bad params,
/// `failed` for what git or the checkout refused (a detached `HEAD`).
pub fn fetch_call(state: &State, fleet_root: &Path, params: &Value) -> Result<Value, ApiError> {
    let instance = params.get("instance").and_then(Value::as_str).ok_or_else(|| ApiError::invalid(format!("`{FETCH_METHOD}` needs `instance`")))?;
    let haves = match params.get("haves") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) if list.len() <= repo::HAVES_CAP => list
            .iter()
            .map(|v| {
                let id = v.as_str().ok_or_else(|| ApiError::invalid("`haves` holds commit ids"))?;
                repo::check_oid(id).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
                Ok(id.to_string())
            })
            .collect::<Result<_, ApiError>>()?,
        Some(_) => return Err(ApiError::invalid(format!("`haves` is a list of at most {} commit ids", repo::HAVES_CAP))),
    };
    let (key, inst) = find_instance(state, instance).ok_or_else(|| ApiError::not_found(format!("no instance `{instance}`")))?;
    let fetched = fetch_of(id_of(key, inst), inst, fleet_root, &haves).map_err(|e| ApiError::failed(format!("{e:#}")))?;
    serde_json::to_value(fetched).map_err(|e| ApiError::internal(e.into()))
}

/// One instance's branch and WIP bundled for the peer whose fleet root is
/// `fleet_root`. The working dir is `inst.folder` (a worktree instance's
/// worktree); the branch a worktree instance's recorded one, else the
/// checkout's current branch (a detached `HEAD` is refused). The snapshot
/// goes onto `refs/devsandbox/wip/<id>` and stays (the previous one kept
/// when its tree and base are the same); the bundle of the branch
/// and that ref `--not` `haves` into `out/<token>.bundle` (none when
/// nothing is new), stale `out/` files swept first.
pub fn fetch_of(id: &str, inst: &Instance, fleet_root: &Path, haves: &[String]) -> Result<Fetched> {
    let folder = &inst.folder;
    let wip_ref = format!("refs/devsandbox/wip/{id}");
    wip::check_wip_ref(&wip_ref).with_context(|| format!("instance id `{id}` can't name a ref"))?;
    let Some(checkout) = repo::probe(folder)? else { bail!("{} isn't a git checkout", folder.display()) };
    let branch = match (&inst.worktree, &inst.branch, checkout.branch) {
        (Some(_), Some(b), _) => b.clone(),
        (_, _, Some(b)) => b,
        _ => bail!("{} has a detached HEAD (check out a branch there first)", folder.display()),
    };
    repo::check_branch(&branch)?;
    let heads = format!("refs/heads/{branch}");
    let old = repo::commit_at(Repo::Checkout(folder), &wip_ref)?;
    let mut snap = wip::snapshot(folder, &wip_ref)?;
    // The same tree over the same base as last time: the old commit, so a
    // fetch with nothing new carries nothing (a new one differs only by
    // its time).
    if let Some(old) = old.filter(|o| *o != snap.wip)
        && same_snapshot(folder, old.as_str(), &snap.wip)?
    {
        run(host_git(folder)?.args(["update-ref", &wip_ref, &old]), QUICK).with_context(|| format!("cannot set {wip_ref}"))?;
        snap.wip = old;
    }
    let tip = repo::commit_at(Repo::Checkout(folder), &heads)?.with_context(|| format!("{} has no commit on {branch} yet", folder.display()))?;
    let id = repo::identity(Repo::Checkout(folder), &wip_ref)?;
    let out = fleet_root.join(OUT_DIR);
    super::real_dir(&out)?;
    sweep(&out, OUT_STALE);
    let name = format!("{}.bundle", repo::nonce());
    let file = out.join(&name);
    let made = repo::bundle_refs(Repo::Checkout(folder), &[&heads, &wip_ref], haves, &file).inspect_err(|_| {
        let _ = std::fs::remove_file(&file);
    })?;
    let refs = [(heads, tip), (wip_ref, snap.wip)].into();
    Ok(Fetched { repo: id, branch, refs, bundle: made.then_some(name) })
}

/// Whether commits `a` and `b` of `checkout` have the same tree and the
/// same first parent.
fn same_snapshot(checkout: &Path, a: &str, b: &str) -> Result<bool> {
    repo::check_oid(a)?;
    repo::check_oid(b)?;
    let out = run(host_git(checkout)?.args(["rev-parse", &format!("{a}^{{tree}}"), &format!("{a}^"), &format!("{b}^{{tree}}"), &format!("{b}^")]), QUICK)?;
    let ids: Vec<&str> = out.lines().collect();
    Ok(ids.len() == 4 && ids[0] == ids[2] && ids[1] == ids[3])
}

/// Remove `out`'s regular files last modified over `age` ago.
fn sweep(out: &Path, age: Duration) {
    let Ok(entries) = std::fs::read_dir(out) else { return };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let Ok(meta) = e.path().symlink_metadata() else { continue };
        let old = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|d| d > age);
        if meta.is_file() && old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// A name [`fetch_of`] makes: `[0-9a-f-]` then `.bundle`.
fn is_out_name(name: &str) -> bool {
    name.strip_suffix(".bundle").is_some_and(|t| !t.is_empty() && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'-')))
}

/// `receivers` serving [`FETCH_KIND`] pulls out of `<fleet_root>/out/`:
/// only a name [`fetch_of`] makes, a regular file (no symlink, `out/` a
/// real dir), removed once the peer answered `ok`.
pub fn serves(fleet_root: &Path, receivers: Receivers) -> Receivers {
    let out = fleet_root.join(OUT_DIR);
    receivers.serves(
        FETCH_KIND,
        move |name| {
            if !is_out_name(name) {
                bail!("`{name}` isn't a bundle this host makes");
            }
            if !out.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                bail!("no {name} to pull here");
            }
            let path = out.join(name);
            match path.symlink_metadata() {
                Ok(m) if m.is_file() => Ok(path),
                Ok(_) => bail!("{name} isn't a file"),
                Err(_) => bail!("no {name} to pull here"),
            }
        },
        |path| {
            let _ = std::fs::remove_file(path);
        },
    )
}

/// The refs fetch-back sets for `machine/instance`: under
/// `refs/devsandbox/<machine>/<instance>`, each a plain ref component
/// (`repo::check_branch`, no `/`); `wip` and `root` are refused as a
/// machine name (devsandbox's own `refs/devsandbox/wip/…` and
/// `refs/devsandbox/root`).
pub fn hidden_prefix(machine: &str, instance: &str) -> Result<String> {
    for (what, c) in [("machine", machine), ("instance", instance)] {
        if c.contains('/') || repo::check_branch(c).is_err() {
            bail!("{what} name `{c}` can't name a ref");
        }
    }
    if matches!(machine, "wip" | "root") {
        bail!("machine name `{machine}` can't name a ref (devsandbox keeps refs/devsandbox/{machine} for itself)");
    }
    Ok(format!("refs/devsandbox/{machine}/{instance}"))
}

/// Fetch back `entry`'s instance (the placing host's end): the local
/// checkout must still be the repo sent ([`check_local`]), its tips are the
/// haves `ask` sends the worker (`instances.fetch`), the answer's repo must
/// be the entry's, its bundle (if any) is pulled through `t` into a temp
/// file in the checkout's git dir (removed after), then [`land`]ed.
pub fn fetch_back(entry: &Entry, ask: impl FnOnce(&[String]) -> Result<Fetched>, t: &dyn Transfer) -> Result<FetchedBack> {
    let prefix = hidden_prefix(&entry.machine, &entry.instance)?;
    let local = &entry.folder;
    check_local(local, entry)?;
    let haves = repo::tips(Repo::Checkout(local))?;
    let fetched = ask(&haves)?;
    if fetched.repo != entry.repo {
        bail!("{} on {} is repo {}, not {} as placed from {}", entry.instance, entry.machine, fetched.repo, entry.repo, local.display());
    }
    let file = repo::TempPath::new(&Repo::Checkout(local).git_dir()?, "fetch");
    let (bundle, size) = match &fetched.bundle {
        Some(name) => {
            let size = t.pull(FETCH_KIND, name, BUNDLE_LIMIT, &file.0).with_context(|| format!("cannot pull {name} from {}", entry.machine))?;
            (Some(file.0.as_path()), size)
        }
        None => (None, 0),
    };
    let refs = land(local, &prefix, &fetched, bundle)?;
    Ok(FetchedBack { machine: entry.machine.clone(), instance: entry.instance.clone(), folder: local.clone(), refs, fetched: size })
}

/// The local checkout at `local` is still the repo `entry` sent: one of its
/// placed branch, its devsandbox root (an unborn repo's) or `HEAD` has
/// `entry.repo` as its identity.
pub fn check_local(local: &Path, entry: &Entry) -> Result<()> {
    if repo::probe(local)?.is_none() {
        bail!("{} (where {}/{} was placed from) isn't a git checkout any more", local.display(), entry.machine, entry.instance);
    }
    let heads = format!("refs/heads/{}", entry.branch);
    for rev in [heads.as_str(), repo::ROOT_REF, "HEAD"] {
        if repo::commit_at(Repo::Checkout(local), rev)?.is_some() && repo::identity(Repo::Checkout(local), rev)? == entry.repo {
            return Ok(());
        }
    }
    bail!("{} isn't repo {} any more (where {}/{} was placed from)", local.display(), entry.repo, entry.machine, entry.instance)
}

/// Land `fetched` (its `bundle` already here, if any) in the checkout at
/// `checkout`: the bundle's objects into its object store
/// (`repo::unbundle_into`, no ref touched), then `<prefix>/heads/<branch>`
/// at the branch's commit and `<prefix>/wip` at the WIP's, each checked to
/// be a commit there first. Nothing else changes: not the user's branches,
/// index, `HEAD` or working tree. The refs set.
pub fn land(checkout: &Path, prefix: &str, fetched: &Fetched, bundle: Option<&Path>) -> Result<BTreeMap<String, String>> {
    repo::check_branch(&fetched.branch)?;
    let heads = format!("refs/heads/{}", fetched.branch);
    let tip = fetched.refs.get(&heads).with_context(|| format!("the answer has no {heads}"))?;
    let wips: Vec<&String> = fetched.refs.iter().filter(|(n, _)| **n != heads).map(|(_, id)| id).collect();
    let ([wip_id], true) = (wips.as_slice(), fetched.refs.keys().all(|n| *n == heads || n.starts_with("refs/devsandbox/wip/"))) else {
        bail!("the answer carries {heads} and one WIP ref, not {}", fetched.refs.keys().cloned().collect::<Vec<_>>().join(", "));
    };
    let set: BTreeMap<String, String> = [(format!("{prefix}/heads/{}", fetched.branch), tip.clone()), (format!("{prefix}/wip"), (*wip_id).clone())].into();
    for (name, id) in &set {
        repo::check_ref(name)?;
        repo::check_oid(id)?;
    }
    if let Some(b) = bundle {
        repo::unbundle_into(Repo::Checkout(checkout), b)?;
    }
    for id in set.values() {
        if repo::commit_at(Repo::Checkout(checkout), id)?.as_deref() != Some(id.as_str()) {
            bail!("commit {id} is neither in the bundle nor already in {}", checkout.display());
        }
    }
    for (name, id) in &set {
        run(host_git(checkout)?.args(["update-ref", name, id]), QUICK).with_context(|| format!("cannot set {name}"))?;
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    use serde_json::json;

    use super::super::repo::tests::{git, repo_with_branches, scratch};
    use super::*;

    fn state(body: &str) -> State {
        toml::from_str(body).unwrap()
    }

    /// A socket pair, each end's reads and writes bounded.
    fn pair() -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        for s in [&a, &b] {
            s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
        }
        (a, b)
    }

    fn instance_toml(name: &str, id: &str, folder: &Path, extra: &str) -> String {
        format!(
            "[instance.{name}]\nsandbox = \"web\"\ninstance_id = \"{id}\"\ncontainer = \"c-{name}\"\nworkspace = \"/w\"\ncreated_unix = 0\nfolder = \"{}\"\n{extra}",
            folder.display()
        )
    }

    /// What fetch-back must leave alone in a checkout.
    fn user_state(repo: &Path) -> Vec<String> {
        vec![
            std::fs::read(repo.join(".git/index")).map(|b| super::super::short_hash(&b)).unwrap_or_default(),
            git(repo, &["status", "--porcelain=v1", "-uall"]),
            git(repo, &["symbolic-ref", "HEAD"]),
            git(repo, &["for-each-ref", "refs/heads"]),
            git(repo, &["stash", "list"]),
        ]
    }

    fn out_files(fleet: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(fleet.join(OUT_DIR)).map(|d| d.flatten().map(|e| e.file_name().into_string().unwrap()).collect()).unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn the_worker_bundles_a_direct_instances_branch_and_wip() {
        let dir = scratch("fetch-direct");
        let web = repo_with_branches(&dir, "web");
        let fleet = dir.join("fleet");
        std::fs::create_dir_all(&fleet).unwrap();
        std::fs::write(web.join("README"), "dirty\n").unwrap();
        let s = state(&instance_toml("web", "web-id", &web, ""));
        let got = fetch_call(&s, &fleet, &json!({"instance": "web"})).unwrap();
        crate::serve::dts::assert_matches("InstancesFetched", &got);
        let f: Fetched = serde_json::from_value(got).unwrap();
        assert_eq!(f.branch, "main");
        let main = git(&web, &["rev-parse", "main"]);
        let wip = git(&web, &["rev-parse", "refs/devsandbox/wip/web-id"]);
        assert_eq!(f.refs, [("refs/heads/main".to_string(), main.clone()), ("refs/devsandbox/wip/web-id".to_string(), wip.clone())].into());
        assert_eq!(git(&web, &["show", &format!("{wip}:README")]), "dirty", "the WIP ref stays on the worker");
        assert_eq!(f.repo, repo::identity(Repo::Checkout(&web), "HEAD").unwrap());
        let name = f.bundle.clone().unwrap();
        assert_eq!(out_files(&fleet), [name.clone()], "only under the peer's out/");
        assert_eq!(git(&web, &["status", "--porcelain=v1"]), "M README", "the worker's checkout untouched");
        // By its id too; with every commit as a have: nothing new, no bundle.
        let again: Fetched =
            serde_json::from_value(fetch_call(&s, &fleet, &json!({"instance": "web-id", "haves": [main, wip]})).unwrap()).unwrap();
        assert_eq!(again.bundle, None);
        assert_eq!(out_files(&fleet), [name]);
        // Errors.
        let code = |p: Value| fetch_call(&s, &fleet, &p).unwrap_err().code;
        assert_eq!(code(json!({})), "invalid");
        assert_eq!(code(json!({"instance": "nope"})), "not-found");
        assert_eq!(code(json!({"instance": "web", "haves": ["HEAD"]})), "invalid");
        git(&web, &["checkout", "-q", "--detach"]);
        let err = fetch_call(&s, &fleet, &json!({"instance": "web"})).unwrap_err();
        assert!(err.code == "failed" && err.message.contains("detached HEAD"), "{err:?}");
        let bad = state(&instance_toml("web", "a..b", &web, ""));
        assert!(fetch_call(&bad, &fleet, &json!({"instance": "web"})).unwrap_err().message.contains("can't name a ref"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_worker_bundles_a_worktree_instance_from_its_worktree() {
        let dir = scratch("fetch-worktree");
        let web = repo_with_branches(&dir, "web");
        let wt = dir.join("wt");
        git(&web, &["worktree", "add", "-q", "-b", "joan/web-2", wt.to_str().unwrap()]);
        std::fs::write(wt.join("new"), "n\n").unwrap();
        git(&wt, &["add", "new"]);
        git(&wt, &["commit", "-q", "-m", "on the worktree"]);
        std::fs::write(wt.join("untracked"), "u\n").unwrap();
        let fleet = dir.join("fleet");
        std::fs::create_dir_all(&fleet).unwrap();
        let extra = format!("base_folder = \"{}\"\nworktree = \"{}\"\nbranch = \"joan/web-2\"\n", web.display(), wt.display());
        let s = state(&instance_toml("web-2", "web-2", &wt, &extra));
        let f: Fetched = serde_json::from_value(fetch_call(&s, &fleet, &json!({"instance": "web-2"})).unwrap()).unwrap();
        assert_eq!(f.branch, "joan/web-2");
        assert_eq!(f.refs["refs/heads/joan/web-2"], git(&wt, &["rev-parse", "HEAD"]));
        let wip = &f.refs["refs/devsandbox/wip/web-2"];
        assert_eq!(git(&web, &["show", &format!("{wip}:untracked")]), "u");
        // The bundle carries both refs; it verifies in a clone of the base.
        let bundle = fleet.join(OUT_DIR).join(f.bundle.unwrap());
        let listed = git(&web, &["bundle", "list-heads", bundle.to_str().unwrap()]);
        assert!(listed.contains("refs/heads/joan/web-2") && listed.contains("refs/devsandbox/wip/web-2"), "{listed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_out_files_are_swept() {
        let dir = scratch("fetch-sweep");
        let out = dir.join("out");
        std::fs::create_dir_all(out.join("sub")).unwrap();
        std::fs::write(out.join("old.bundle"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        sweep(&out, Duration::from_millis(10));
        assert!(!out.join("old.bundle").exists());
        assert!(out.join("sub").exists(), "only files");
        std::fs::write(out.join("new.bundle"), "x").unwrap();
        sweep(&out, OUT_STALE);
        assert!(out.join("new.bundle").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_peers_own_out_files_are_served() {
        let dir = scratch("fetch-serves");
        let fleet = dir.join("fleet");
        std::fs::create_dir_all(fleet.join(OUT_DIR)).unwrap();
        std::fs::write(fleet.join("secret.bundle"), "s").unwrap();
        std::fs::write(fleet.join(OUT_DIR).join("1a-2.bundle"), "body").unwrap();
        std::os::unix::fs::symlink(fleet.join("secret.bundle"), fleet.join(OUT_DIR).join("2b.bundle")).unwrap();
        let r = Arc::new(serves(&fleet, Receivers::new(|_| {})));
        let pull = |name: &str| {
            let dest = dir.join("got");
            let _ = std::fs::remove_file(&dest);
            let (a, b) = pair();
            let r = Arc::clone(&r);
            let server = std::thread::spawn(move || r.receive(b));
            let got = crate::serve::fleet::transfer::pull(a, FETCH_KIND, name, 100, &dest);
            let _ = server.join().unwrap();
            got.map(|n| (n, std::fs::read_to_string(&dest).unwrap()))
        };
        assert_eq!(pull("1a-2.bundle").unwrap(), (4, "body".to_string()));
        assert!(!fleet.join(OUT_DIR).join("1a-2.bundle").exists(), "removed once pulled");
        for (name, why) in [("1a-2.bundle", "no 1a-2.bundle"), ("2b.bundle", "isn't a file"), ("secret.txt", "isn't a bundle"), ("..", "bad transfer name")] {
            let err = format!("{:#}", pull(name).unwrap_err());
            assert!(err.contains(why), "{name}: {err}");
        }
        assert!(fleet.join("secret.bundle").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rm_fetches_back_first_unless_forced_and_done_best_effort() {
        assert_eq!(before("rm", &json!({"instance": "web-2"}), true), Before::Required);
        assert_eq!(before("rm", &json!({"instance": "web-2", "force": false, "ask": true}), true), Before::Required);
        assert_eq!(before("rm", &json!({"instance": "web-2", "force": true}), true), Before::Skipped);
        assert_eq!(before("rm", &json!({"instance": "web-2"}), false), Before::Nothing, "not placed from here");
        assert_eq!(before("rm", &json!({"instance": "web-2", "force": true}), false), Before::Nothing);
        assert_eq!(before("instances.done", &json!({"instance": "web-2", "done": true}), true), Before::BestEffort);
        assert_eq!(before("instances.done", &json!({"instance": "web-2", "done": false}), true), Before::Nothing, "undone");
        assert_eq!(before("instances.done", &json!({"instance": "web-2", "done": true}), false), Before::Nothing);
        assert_eq!(before("stop", &json!({"instance": "web-2"}), true), Before::Nothing);
    }

    #[test]
    fn hidden_refs_take_plain_names_only() {
        assert_eq!(hidden_prefix("lap", "web-2").unwrap(), "refs/devsandbox/lap/web-2");
        for (m, i) in [("a/b", "x"), ("lap", "a/b"), ("..", "x"), ("lap", "-x"), ("lap", ""), ("wip", "x"), ("root", "x"), ("lap", "x.lock")] {
            assert!(hidden_prefix(m, i).is_err(), "{m}/{i}");
        }
    }

    /// `web` (the source, placed from) and a clone `there` standing for the
    /// worker's checkout, with a commit and uncommitted work on it; the
    /// placement's entry.
    fn placed(tag: &str) -> (PathBuf, PathBuf, PathBuf, Entry) {
        let dir = scratch(tag);
        let web = repo_with_branches(&dir, "web");
        let there = dir.join("there");
        git(&dir, &["clone", "-q", web.to_str().unwrap(), there.to_str().unwrap()]);
        std::fs::write(there.join("README"), "worker\n").unwrap();
        git(&there, &["commit", "-q", "-am", "on the worker"]);
        std::fs::write(there.join("wip"), "uncommitted\n").unwrap();
        let entry = Entry {
            machine: "lap".into(),
            instance: "web-2".into(),
            sandbox: "web".into(),
            root: dir.join("cfg"),
            folder: web.clone(),
            remote: there.clone(),
            repo: repo::identity(Repo::Checkout(&web), "HEAD").unwrap(),
            branch: "main".into(),
        };
        (dir, web, there, entry)
    }

    /// A [`Transfer`] pulling straight out of a fleet root's `out/`.
    struct OutDir(PathBuf);
    impl Transfer for OutDir {
        fn send(&self, _: &crate::serve::fleet::transfer::Header, _: &mut dyn std::io::Read) -> Result<()> {
            unreachable!()
        }
        fn pull(&self, kind: &str, name: &str, limit: u64, dest: &Path) -> Result<u64> {
            let (a, b) = pair();
            let r = serves(&self.0, Receivers::new(|_| {}));
            let server = std::thread::spawn(move || r.receive(b).map(|_| ()));
            let got = crate::serve::fleet::transfer::pull(a, kind, name, limit, dest);
            server.join().unwrap()?;
            got
        }
    }

    #[test]
    fn fetch_back_lands_hidden_refs_only() {
        let (dir, web, there, entry) = placed("fetch-back");
        std::fs::write(web.join("README"), "local dirty\n").unwrap();
        git(&web, &["stash", "-q"]);
        std::fs::write(web.join("F2"), "untracked here\n").unwrap();
        let before = user_state(&web);
        let fleet = dir.join("fleet");
        std::fs::create_dir_all(&fleet).unwrap();
        let s = state(&instance_toml("web-2", "web-2", &there, ""));
        let ask = |haves: &[String]| -> Result<Fetched> {
            let v = fetch_call(&s, &fleet, &json!({"instance": "web-2", "haves": haves})).map_err(|e| anyhow::anyhow!(e.message))?;
            Ok(serde_json::from_value(v)?)
        };
        let got = fetch_back(&entry, ask, &OutDir(fleet.clone())).unwrap();
        crate::serve::dts::assert_matches("PlacementsFetched", &serde_json::to_value(&got).unwrap());
        assert!(got.fetched > 0);
        let heads = "refs/devsandbox/lap/web-2/heads/main";
        assert_eq!(got.refs.keys().collect::<Vec<_>>(), [heads, "refs/devsandbox/lap/web-2/wip"]);
        assert_eq!(git(&web, &["rev-parse", heads]), git(&there, &["rev-parse", "main"]));
        assert_eq!(git(&web, &["show", "refs/devsandbox/lap/web-2/wip:wip"]), "uncommitted");
        assert_eq!(git(&web, &["show", &format!("{heads}:README")]), "worker");
        assert_eq!(user_state(&web), before, "the user's branches, index, HEAD, tree and stash are untouched");
        assert_eq!(out_files(&fleet), Vec::<String>::new(), "pulled, so removed");
        let temps: Vec<_> = std::fs::read_dir(web.join(".git")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("devsandbox-")).collect();
        assert!(temps.is_empty(), "{temps:?}");

        // Again with nothing new: no bundle, the same refs.
        let again = fetch_back(&entry, ask, &OutDir(fleet.clone())).unwrap();
        assert_eq!((again.fetched, &again.refs), (0, &got.refs));

        // A placement whose names can't be refs, or whose repo isn't the checkout's.
        let err = |e: &Entry| format!("{:#}", fetch_back(e, ask, &OutDir(fleet.clone())).unwrap_err());
        assert!(err(&Entry { machine: "a/b".into(), ..entry.clone() }).contains("can't name a ref"));
        assert!(err(&Entry { repo: "0000000000000000".into(), ..entry.clone() }).contains("isn't repo 0000000000000000 any more"));
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        git(&other, &["init", "-q", "-b", "main"]);
        git(&other, &["commit", "-q", "--allow-empty", "-m", "x"]);
        let other_s = state(&instance_toml("web-2", "web-2", &other, ""));
        let lie = |haves: &[String]| -> Result<Fetched> {
            let v = fetch_call(&other_s, &fleet, &json!({"instance": "web-2", "haves": haves})).map_err(|e| anyhow::anyhow!(e.message))?;
            Ok(serde_json::from_value(v)?)
        };
        let e = format!("{:#}", fetch_back(&entry, lie, &OutDir(fleet.clone())).unwrap_err());
        assert!(e.contains(&format!("not {} as placed from", entry.repo)), "{e}");
        assert_eq!(user_state(&web), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn landing_refuses_an_answer_it_cant_check() {
        let (dir, web, _, _) = placed("fetch-land");
        let main = git(&web, &["rev-parse", "main"]);
        let f = |refs: &[(&str, &str)]| Fetched {
            repo: "0".repeat(16),
            branch: "main".into(),
            refs: refs.iter().map(|(n, id)| (n.to_string(), id.to_string())).collect(),
            bundle: None,
        };
        let prefix = "refs/devsandbox/lap/web-2";
        let err = |fetched: &Fetched| format!("{:#}", land(&web, prefix, fetched, None).unwrap_err());
        assert!(err(&f(&[("refs/devsandbox/wip/x", &main)])).contains("no refs/heads/main"));
        assert!(err(&f(&[("refs/heads/main", &main), ("refs/heads/other", &main)])).contains("one WIP ref"));
        assert!(err(&f(&[("refs/heads/main", &main), ("refs/devsandbox/wip/x", &"1".repeat(40))])).contains("neither in the bundle"));
        assert_eq!(git(&web, &["for-each-ref", "refs/devsandbox"]), "", "nothing set");
        let ok = land(&web, prefix, &f(&[("refs/heads/main", &main), ("refs/devsandbox/wip/x", &main)]), None).unwrap();
        assert_eq!(ok.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
