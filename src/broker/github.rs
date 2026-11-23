//! The GitHub repo fence resolved to repository ids and node ids at the home
//! (github-and-sentry-access, *Matching* and *Routes the broker can
//! attribute*; git-gateway, *The gateway*, "The broker";
//! docs/architecture/credentials/fence.md#ids).
//!
//! Each fence entry is pinned to the repository it named the first time
//! the home resolved it (`State::github_ids`): `GET /repos/{owner}/{repo}`
//! with the credential, a rename's 301 followed once and only on
//! `api.github.com`. From then on the entry stands for that id: a renamed
//! repo matches by its old and its current name, a call naming the old one
//! goes to the current one, `/repositories/{id}` and repository node ids
//! match the pins, and whoever re-registers the old name gets nothing.
//!
//! [`Fence`] is the placement, pure over a [`Lookup`]; [`Live`] is the
//! lookup `serve` runs, cached per credential ([`Cache`]) over a
//! [`Transport`] ([`Https`]: the broker's upstream client, redirects off).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Mutex;

use serde::Deserialize;

use super::head::Header;
use super::route;
use super::upstream::{self, Body, Request, Upstream};
use crate::credentials::fence;
use crate::credentials::store::Secret;
use crate::state::{GithubPin, State};

/// The host the home resolves against.
pub const API: &str = "api.github.com";

/// How long a resolved repository is trusted before it's asked again.
pub const TTL: u64 = 3600;
/// A 404 (no access, or no such repo).
pub const MISSING_TTL: u64 = 300;
/// Any other answer, or none: short, so GitHub being down costs one call a
/// minute per name, not one per request.
pub const FAILED_TTL: u64 = 60;
/// Entries past this drop the expired ones, then all.
pub const CACHE_CAP: usize = 4096;
const MAX_BODY: u64 = 1024 * 1024;

/// The `node(id:)` query: a repository, or the repository of an object
/// that has one.
const NODE_QUERY: &str = "query($id:ID!){node(id:$id){__typename \
    ...on Repository{databaseId id nameWithOwner} \
    ...on RepositoryNode{repository{databaseId id nameWithOwner}} \
    ...on Ref{repository{databaseId id nameWithOwner}} \
    ...on Release{repository{databaseId id nameWithOwner}} \
    ...on Milestone{repository{databaseId id nameWithOwner}} \
    ...on Label{repository{databaseId id nameWithOwner}} \
    ...on Commit{repository{databaseId id nameWithOwner}}}}";

/// A repository as GitHub answered for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub id: u64,
    pub node_id: String,
    /// As GitHub spells it, checked to be one [`fence::normalize`] reads.
    pub full_name: String,
}

impl RepoRef {
    fn new(id: u64, node_id: String, full_name: String) -> Option<RepoRef> {
        let (owner, repo) = full_name.split_once('/')?;
        fence::normalize(owner, repo)?;
        (!node_id.is_empty() && node_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'=')))
            .then_some(RepoRef { id, node_id, full_name })
    }

    /// `full_name` normalized, as fence entries are.
    pub fn name(&self) -> String {
        self.full_name.to_ascii_lowercase()
    }
}

/// Whose credential a lookup goes out with.
#[derive(Debug, Clone, Copy)]
pub struct Via<'a> {
    pub credential: &'a str,
    /// The approved claim marks `api.github.com` private.
    pub private: bool,
}

/// What the home asks GitHub. `None`: no such repository for this
/// credential, or no answer.
pub trait Lookup {
    /// `GET /repos/{repo}`, a rename's 301 followed once.
    fn by_name(&self, via: &Via<'_>, repo: &str) -> Option<RepoRef>;
    /// `GET /repositories/{id}`.
    fn by_id(&self, via: &Via<'_>, id: u64) -> Option<RepoRef>;
    /// `node(id:)`: the repository a global id names or belongs to. Only a
    /// repository's answer may be cached: any other object can move (a
    /// transferred issue keeps its id).
    fn node(&self, via: &Via<'_>, node_id: &str) -> Option<RepoRef>;
    /// Records `repo`'s pin unless it has one.
    fn pin(&self, repo: &str, to: &RepoRef);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// `current`: the repository's current name when the call named an
    /// older one, the call to be sent there.
    In { current: Option<String> },
    Out,
}

/// The instance's fence for one call: its entries, their pins, and the
/// lookup for what isn't pinned yet. Without a lookup (a claim without
/// `api.github.com`) entries match by name only.
pub struct Fence<'a> {
    names: BTreeSet<String>,
    pins: BTreeMap<String, GithubPin>,
    lookup: Option<(&'a dyn Lookup, Via<'a>)>,
}

impl<'a> Fence<'a> {
    pub fn new(
        names: BTreeSet<String>,
        pins: &BTreeMap<String, GithubPin>,
        lookup: Option<(&'a dyn Lookup, Via<'a>)>,
    ) -> Fence<'a> {
        let pins = pins.iter().filter(|(k, _)| names.contains(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        Fence { names, pins, lookup }
    }

    /// `entry`'s pin, resolving and pinning it the first time.
    fn pin_of(&mut self, entry: &str) -> Option<GithubPin> {
        if let Some(p) = self.pins.get(entry) {
            return Some(p.clone());
        }
        let (lookup, via) = self.lookup?;
        let r = lookup.by_name(&via, entry)?;
        lookup.pin(entry, &r);
        let pin = GithubPin { id: r.id, node_id: r.node_id, pinned_at: crate::state::Instance::now() };
        self.pins.insert(entry.to_string(), pin.clone());
        Some(pin)
    }

    /// The repository `entry` is pinned to, as GitHub names it now.
    fn current(&mut self, entry: &str) -> Option<RepoRef> {
        let pin = self.pin_of(entry)?;
        let (lookup, via) = self.lookup?;
        lookup.by_id(&via, pin.id).filter(|r| r.id == pin.id)
    }

    /// `repo` (normalized): an entry, matched by name and sent to its
    /// pinned repository's current name; else the current name of an
    /// entry's pinned repository. Every call made is one per entry,
    /// whatever the sandbox names: a name outside the fence is never
    /// looked up itself.
    pub fn place_name(&mut self, repo: &str) -> Place {
        if self.names.contains(repo) {
            let current = self.current(repo).map(|r| r.full_name).filter(|c| c.to_ascii_lowercase() != repo);
            return Place::In { current };
        }
        let entries: Vec<String> = self.names.iter().cloned().collect();
        for e in entries {
            if self.current(&e).is_some_and(|r| r.name() == repo) {
                return Place::In { current: None };
            }
        }
        Place::Out
    }

    /// A repository id (`/repositories/{id}`, a mutation's `databaseId`):
    /// in when an entry is pinned to it.
    pub fn place_id(&mut self, id: u64) -> bool {
        self.entry_with_id(id).is_some()
    }

    pub fn entry_with_id(&mut self, id: u64) -> Option<String> {
        let entries: Vec<String> = self.names.iter().cloned().collect();
        entries.into_iter().find(|e| self.pin_of(e).is_some_and(|p| p.id == id))
    }

    /// A global node id: a repository node id an entry is pinned to, with
    /// no call; else the repository `node(id:)` resolves it to, placed by
    /// its id. `Ok`: in, with the repository (`full_name` the entry's for
    /// a pinned one); `Err`: out, with the repository when it resolved.
    pub fn place_node(&mut self, node_id: &str) -> Result<RepoRef, Option<RepoRef>> {
        let entries: Vec<String> = self.names.iter().cloned().collect();
        for e in &entries {
            if let Some(p) = self.pin_of(e).filter(|p| p.node_id == node_id) {
                return Ok(RepoRef { id: p.id, node_id: p.node_id, full_name: e.clone() });
            }
        }
        let (lookup, via) = self.lookup.ok_or(None)?;
        let r = lookup.node(&via, node_id).ok_or(None)?;
        match self.place_id(r.id) {
            true => Ok(r),
            false => Err(Some(r)),
        }
    }

    /// What a repository id outside the fence is called, for a refusal.
    pub fn name_of(&self, id: u64) -> Option<String> {
        let (lookup, via) = self.lookup?;
        lookup.by_id(&via, id).filter(|r| r.id == id).map(|r| r.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub location: Option<Vec<u8>>,
    pub body: Vec<u8>,
}

/// One credentialed call to [`API`].
pub trait Transport {
    fn call(&self, via: &Via<'_>, method: &str, path: &str, body: Option<&[u8]>) -> io::Result<Answer>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Fetched {
    Found(RepoRef),
    /// 404, or a `node(id:)` that names nothing with a repository.
    Missing,
    Failed,
}

#[derive(Deserialize)]
struct RestRepo {
    id: u64,
    node_id: String,
    full_name: String,
}

pub fn parse_repo(body: &[u8]) -> Option<RepoRef> {
    let r: RestRepo = serde_json::from_slice(body).ok()?;
    RepoRef::new(r.id, r.node_id, r.full_name)
}

/// The path a redirect's `Location` names on [`API`] itself; any other
/// host is never followed with the credential.
pub fn redirect_path(location: &[u8]) -> Option<String> {
    let t = upstream::Target::parse(std::str::from_utf8(location).ok()?.trim()).ok()?;
    (t.host == API && t.port == 443 && route::check_path(&t.path).is_ok()).then_some(t.path)
}

fn fetch_repo(t: &dyn Transport, via: &Via<'_>, path: &str) -> Fetched {
    let answer = |path: &str| t.call(via, "GET", path, None);
    let mut a = match answer(path) {
        Ok(a) => a,
        Err(_) => return Fetched::Failed,
    };
    if matches!(a.status, 301 | 302 | 307 | 308) {
        let Some(next) = a.location.as_deref().and_then(redirect_path) else { return Fetched::Failed };
        a = match answer(&next) {
            Ok(a) => a,
            Err(_) => return Fetched::Failed,
        };
    }
    match a.status {
        200 => parse_repo(&a.body).map_or(Fetched::Failed, Fetched::Found),
        404 => Fetched::Missing,
        _ => Fetched::Failed,
    }
}

/// `GET /repos/{repo}`, a rename's 301 followed once.
pub fn fetch_name(t: &dyn Transport, via: &Via<'_>, repo: &str) -> Fetched {
    let Some((owner, name)) = repo.split_once('/').filter(|(o, r)| fence::normalize(o, r).is_some()) else {
        return Fetched::Missing;
    };
    fetch_repo(t, via, &format!("/repos/{owner}/{name}"))
}

pub fn fetch_id(t: &dyn Transport, via: &Via<'_>, id: u64) -> Fetched {
    fetch_repo(t, via, &format!("/repositories/{id}"))
}

/// `node(id:)` over GraphQL, and whether the node is the repository itself.
pub fn fetch_node(t: &dyn Transport, via: &Via<'_>, node_id: &str) -> (Fetched, bool) {
    let body = serde_json::json!({ "query": NODE_QUERY, "variables": { "id": node_id } }).to_string();
    let a = match t.call(via, "POST", "/graphql", Some(body.as_bytes())) {
        Ok(a) if a.status == 200 => a,
        _ => return (Fetched::Failed, false),
    };
    parse_node(&a.body)
}

pub fn parse_node(body: &[u8]) -> (Fetched, bool) {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct GqlRepo {
        database_id: u64,
        id: String,
        name_with_owner: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Node {
        #[serde(rename = "__typename")]
        typename: String,
        database_id: Option<u64>,
        id: Option<String>,
        name_with_owner: Option<String>,
        repository: Option<GqlRepo>,
    }
    #[derive(Deserialize)]
    struct Data {
        node: Option<Node>,
    }
    #[derive(Deserialize)]
    struct Reply {
        data: Option<Data>,
    }
    let Ok(reply) = serde_json::from_slice::<Reply>(body) else { return (Fetched::Failed, false) };
    let Some(node) = reply.data.and_then(|d| d.node) else { return (Fetched::Missing, false) };
    let repo = node.typename == "Repository";
    let r = match (repo, node.repository) {
        (true, _) => node
            .database_id
            .zip(node.id)
            .zip(node.name_with_owner)
            .and_then(|((id, node_id), name)| RepoRef::new(id, node_id, name)),
        (false, Some(g)) => RepoRef::new(g.database_id, g.id, g.name_with_owner),
        (false, None) => None,
    };
    (r.map_or(Fetched::Missing, Fetched::Found), repo)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Name(String),
    Id(u64),
    Node(String),
}

/// Answers per (credential, key) until their expiry: what a credential sees
/// is its own.
#[derive(Default)]
pub struct Cache {
    map: HashMap<(String, Key), (Option<RepoRef>, u64)>,
}

impl Cache {
    fn get(&self, credential: &str, key: &Key, now: u64) -> Option<Option<RepoRef>> {
        self.map.get(&(credential.to_string(), key.clone())).filter(|(_, until)| now < *until).map(|(r, _)| r.clone())
    }

    fn put(&mut self, credential: &str, key: Key, r: Option<RepoRef>, until: u64, now: u64) {
        if self.map.len() >= CACHE_CAP {
            self.map.retain(|_, (_, u)| now < *u);
            if self.map.len() >= CACHE_CAP {
                self.map.clear();
            }
        }
        self.map.insert((credential.to_string(), key), (r, until));
    }

    /// A found repository under its id and node id too.
    fn found(&mut self, credential: &str, r: &RepoRef, now: u64) {
        let until = now + TTL;
        self.put(credential, Key::Id(r.id), Some(r.clone()), until, now);
        self.put(credential, Key::Node(r.node_id.clone()), Some(r.clone()), until, now);
    }
}

/// The lookup `serve` runs per call: [`Cache`] in front of a [`Transport`],
/// pins written to `state.toml`.
pub struct Live<'a> {
    pub cache: &'a Mutex<Cache>,
    pub transport: &'a dyn Transport,
    pub state: &'a Path,
    pub now: u64,
}

impl Live<'_> {
    fn cached(&self, via: &Via<'_>, key: Key, fetch: impl FnOnce() -> Fetched) -> Option<RepoRef> {
        let lock = || self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = lock().get(via.credential, &key, self.now) {
            return hit;
        }
        let fetched = fetch();
        let mut cache = lock();
        let (r, ttl) = match fetched {
            Fetched::Found(r) => {
                cache.found(via.credential, &r, self.now);
                (Some(r), TTL)
            }
            Fetched::Missing => (None, MISSING_TTL),
            Fetched::Failed => (None, FAILED_TTL),
        };
        cache.put(via.credential, key, r.clone(), self.now + ttl, self.now);
        r
    }
}

impl Lookup for Live<'_> {
    fn by_name(&self, via: &Via<'_>, repo: &str) -> Option<RepoRef> {
        self.cached(via, Key::Name(repo.to_ascii_lowercase()), || fetch_name(self.transport, via, repo))
    }

    fn by_id(&self, via: &Via<'_>, id: u64) -> Option<RepoRef> {
        self.cached(via, Key::Id(id), || fetch_id(self.transport, via, id))
    }

    fn node(&self, via: &Via<'_>, node_id: &str) -> Option<RepoRef> {
        let key = Key::Node(node_id.to_string());
        if let Some(Some(hit)) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(via.credential, &key, self.now) {
            return Some(hit);
        }
        match fetch_node(self.transport, via, node_id) {
            (Fetched::Found(r), repo) => {
                let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                cache.found(via.credential, &r, self.now);
                if repo {
                    cache.put(via.credential, key, Some(r.clone()), self.now + TTL, self.now);
                }
                Some(r)
            }
            _ => None,
        }
    }

    fn pin(&self, repo: &str, to: &RepoRef) {
        let pin = GithubPin { id: to.id, node_id: to.node_id.clone(), pinned_at: self.now };
        // Unwritten, the pin still holds for this call and is taken again
        // at the next.
        let _ = State::update_at(self.state, |s| {
            s.github_ids.entry(repo.to_string()).or_insert(pin);
            Ok(())
        });
    }
}

/// [`Transport`] over the broker's upstream client: `authority` dialed for
/// [`API`] (tests' local server), the credential's token from `token`.
pub struct Https<'a> {
    pub upstream: &'a Upstream,
    pub authority: &'a str,
    pub token: &'a dyn Fn(&str) -> Option<Secret>,
}

impl Transport for Https<'_> {
    fn call(&self, via: &Via<'_>, method: &str, path: &str, body: Option<&[u8]>) -> io::Result<Answer> {
        let token = (self.token)(via.credential)
            .ok_or_else(|| io::Error::other(format!("no token for credential `{}`", via.credential)))?;
        let field = |name: &str, value: &str| Header { name: name.into(), value: value.as_bytes().to_vec() };
        let mut headers = vec![
            field("Authorization", &format!("Bearer {}", token.expose())),
            field("Accept", "application/vnd.github+json"),
            field("User-Agent", "devsandbox-broker"),
            field("X-GitHub-Api-Version", "2022-11-28"),
        ];
        let mut sent = body.unwrap_or_default();
        let req_body = match body {
            Some(b) => {
                headers.push(field("Content-Type", "application/json"));
                Body::Length(b.len() as u64, &mut sent)
            }
            None => Body::Empty,
        };
        let url = format!("https://{}{path}", self.authority);
        let resp =
            self.upstream.send(Request { method, url: &url, headers: &headers, body: req_body, private: via.private })?;
        let location = super::head::header(&resp.headers, "location").map(<[u8]>::to_vec);
        let mut out = Vec::new();
        resp.body.take(MAX_BODY).read_to_end(&mut out)?;
        Ok(Answer { status: resp.status, location, body: out })
    }
}

#[cfg(test)]
pub(crate) mod tests;
