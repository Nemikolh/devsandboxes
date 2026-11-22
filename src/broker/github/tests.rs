use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use super::*;
use crate::broker::serve::tests::upstream_server;
use crate::broker::upstream::Timeouts;

pub(crate) fn repo(id: u64, full_name: &str) -> RepoRef {
    RepoRef { id, node_id: format!("R_{id}"), full_name: full_name.into() }
}

/// GitHub as a table: names (a renamed one answering its new name), ids,
/// non-repository nodes; every lookup and pin recorded.
#[derive(Default)]
pub(crate) struct Fake {
    pub names: BTreeMap<String, RepoRef>,
    pub ids: BTreeMap<u64, RepoRef>,
    pub nodes: BTreeMap<String, RepoRef>,
    pub calls: RefCell<Vec<String>>,
    pub pins: RefCell<Vec<(String, u64)>>,
}

impl Fake {
    /// `repo` answering `names` (its current name and older ones).
    pub fn with(mut self, r: RepoRef, names: &[&str]) -> Fake {
        for n in names {
            self.names.insert(n.to_string(), r.clone());
        }
        self.ids.insert(r.id, r);
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Lookup for Fake {
    fn by_name(&self, _: &Via<'_>, repo: &str) -> Option<RepoRef> {
        self.calls.borrow_mut().push(format!("name {repo}"));
        self.names.get(repo).cloned()
    }
    fn by_id(&self, _: &Via<'_>, id: u64) -> Option<RepoRef> {
        self.calls.borrow_mut().push(format!("id {id}"));
        self.ids.get(&id).cloned()
    }
    fn node(&self, _: &Via<'_>, node_id: &str) -> Option<RepoRef> {
        self.calls.borrow_mut().push(format!("node {node_id}"));
        self.nodes.get(node_id).cloned().or_else(|| self.ids.values().find(|r| r.node_id == node_id).cloned())
    }
    fn pin(&self, repo: &str, to: &RepoRef) {
        self.pins.borrow_mut().push((repo.to_string(), to.id));
    }
}

const VIA: Via<'static> = Via { credential: "github-work", private: false };

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn pinned(list: &[(&str, u64)]) -> BTreeMap<String, GithubPin> {
    list.iter().map(|(n, id)| (n.to_string(), GithubPin { id: *id, node_id: format!("R_{id}"), pinned_at: 1 })).collect()
}

#[test]
fn an_entry_is_pinned_at_its_first_resolution() {
    let fake = Fake::default().with(repo(42, "o/r"), &["o/r"]);
    let mut fence = Fence::new(names(&["o/r"]), &BTreeMap::new(), Some((&fake, VIA)));
    assert_eq!(fence.place_name("o/r"), Place::In { current: None });
    assert_eq!(*fake.pins.borrow(), [("o/r".to_string(), 42)]);
    assert!(fence.place_id(42));
    assert!(!fence.place_id(7));
    assert_eq!(fake.calls(), ["name o/r", "id 42"], "pinned once, then no name lookup");
}

#[test]
fn a_renamed_repo_matches_by_both_names_and_goes_to_the_current_one() {
    // `o/r` was renamed to `N/R`: GitHub's 301 answers the old name with it.
    let fake = Fake::default().with(repo(42, "N/R"), &["o/r", "n/r"]);
    let mut fence = Fence::new(names(&["o/r"]), &BTreeMap::new(), Some((&fake, VIA)));
    assert_eq!(fence.place_name("o/r"), Place::In { current: Some("N/R".into()) });
    assert_eq!(fence.place_name("n/r"), Place::In { current: None });
    assert_eq!(fence.place_name("x/y"), Place::Out);
    assert!(!fake.calls().iter().any(|c| c.contains("x/y") || c.contains("n/r")), "{:?}", fake.calls());
}

#[test]
fn whoever_re_registers_an_old_name_gets_nothing() {
    // `o/r` was pinned to 42, since renamed to `n/r`; someone else now owns
    // `o/r` (99).
    let fake = Fake::default().with(repo(42, "n/r"), &["n/r"]).with(repo(99, "o/r"), &["o/r"]);
    let mut fence = Fence::new(names(&["o/r"]), &pinned(&[("o/r", 42)]), Some((&fake, VIA)));
    assert_eq!(fence.place_name("o/r"), Place::In { current: Some("n/r".into()) }, "sent to the pinned repo");
    assert!(fence.place_id(42));
    assert!(!fence.place_id(99));
    assert_eq!(fence.place_node("R_99"), Err(Some(repo(99, "o/r"))));
    assert!(fake.pins.borrow().is_empty());
    assert!(!fake.calls().contains(&"name o/r".to_string()), "a pinned entry's name is never looked up");
}

#[test]
fn unresolvable_entries_match_by_name_only() {
    let fake = Fake::default();
    let mut fence = Fence::new(names(&["o/r"]), &BTreeMap::new(), Some((&fake, VIA)));
    assert_eq!(fence.place_name("o/r"), Place::In { current: None });
    assert_eq!(fence.place_name("x/y"), Place::Out);
    assert!(!fence.place_id(1));
    assert!(fake.pins.borrow().is_empty());

    let mut bare = Fence::new(names(&["o/r"]), &pinned(&[("o/r", 42)]), None);
    assert_eq!(bare.place_name("o/r"), Place::In { current: None });
    assert_eq!(bare.place_name("n/r"), Place::Out);
    assert!(bare.place_id(42), "pins hold without a lookup");
    assert_eq!(bare.name_of(42), None);
}

#[test]
fn nodes_match_pins_without_a_call_and_resolve_otherwise() {
    let mut fake = Fake::default().with(repo(42, "o/r"), &["o/r"]).with(repo(7, "x/y"), &["x/y"]);
    fake.nodes.insert("I_issue".into(), repo(42, "o/r"));
    fake.nodes.insert("PR_out".into(), repo(7, "x/y"));
    let mut fence = Fence::new(names(&["o/r"]), &pinned(&[("o/r", 42)]), Some((&fake, VIA)));
    assert_eq!(fence.place_node("R_42"), Ok(repo(42, "o/r")));
    assert!(fake.calls().is_empty());
    assert_eq!(fence.place_node("I_issue"), Ok(repo(42, "o/r")));
    assert_eq!(fence.place_node("PR_out"), Err(Some(repo(7, "x/y"))));
    assert_eq!(fence.place_node("nothing"), Err(None));
    assert_eq!(fence.name_of(7).as_deref(), Some("x/y"));
}

#[test]
fn rest_and_graphql_answers() {
    let ok = br#"{"id":42,"node_id":"R_kgDOabc","full_name":"Org/Repo.js","private":true}"#;
    assert_eq!(parse_repo(ok), Some(RepoRef { id: 42, node_id: "R_kgDOabc".into(), full_name: "Org/Repo.js".into() }));
    for bad in [
        &br#"{"id":42,"node_id":"R_1","full_name":"o/r/x"}"#[..],
        br#"{"id":42,"node_id":"R_1","full_name":"o/../r"}"#,
        br#"{"id":42,"node_id":"R 1","full_name":"o/r"}"#,
        br#"{"id":-1,"node_id":"R_1","full_name":"o/r"}"#,
        br#"{"node_id":"R_1","full_name":"o/r"}"#,
        b"nope",
    ] {
        assert_eq!(parse_repo(bad), None, "{}", String::from_utf8_lossy(bad));
    }

    let repo_node = br#"{"data":{"node":{"__typename":"Repository","databaseId":42,"id":"R_42","nameWithOwner":"o/r"}}}"#;
    assert_eq!(parse_node(repo_node), (Fetched::Found(repo(42, "o/r")), true));
    let issue = br#"{"data":{"node":{"__typename":"Issue","repository":{"databaseId":42,"id":"R_42","nameWithOwner":"o/r"}}}}"#;
    assert_eq!(parse_node(issue), (Fetched::Found(repo(42, "o/r")), false));
    let user = br#"{"data":{"node":{"__typename":"User"}}}"#;
    assert_eq!(parse_node(user), (Fetched::Missing, false));
    let none = br#"{"data":{"node":null},"errors":[{"type":"NOT_FOUND"}]}"#;
    assert_eq!(parse_node(none), (Fetched::Missing, false));
    assert_eq!(parse_node(b"<html>"), (Fetched::Failed, false));
}

#[test]
fn redirects_are_followed_only_on_api_github_com() {
    let p = |l: &str| redirect_path(l.as_bytes());
    assert_eq!(p("https://api.github.com/repositories/42").as_deref(), Some("/repositories/42"));
    assert_eq!(p("https://API.github.com:443/repositories/42?x=1").as_deref(), Some("/repositories/42?x=1"));
    for off in [
        "https://evil.example/repositories/42",
        "http://api.github.com/repositories/42",
        "https://api.github.com:8443/repositories/42",
        "https://api.github.com.evil.example/repositories/42",
        "https://user@api.github.com/repositories/42",
        "/repositories/42",
        "https://api.github.com/a/../repositories/42",
        "https://api.github.com//repositories/42",
    ] {
        assert_eq!(p(off), None, "{off}");
    }
}

// Over the broker's upstream client, against a local TLS server.

const TOKEN: &str = "REALtoken-gh";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/broker").join(name)
}

fn client() -> Upstream {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from_pem_file(fixture("ca.pem")).unwrap()).unwrap();
    Upstream::new(roots, None, Timeouts { connect: Duration::from_secs(5), idle: Duration::from_secs(10) }).unwrap()
}

fn token(credential: &str) -> Option<Secret> {
    (credential != "none").then(|| Secret::new(format!("{TOKEN}-{credential}")))
}

fn reply(status: u16, extra: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

const REPO_42: &str = r#"{"id":42,"node_id":"R_42","full_name":"N/R"}"#;

/// GitHub's answers by request line.
fn github(head: &str, body: &[u8]) -> Vec<u8> {
    let line = head.lines().next().unwrap();
    let path = line.split(' ').nth(1).unwrap();
    match path {
        "/repos/o/r" => reply(301, "Location: https://api.github.com/repositories/42\r\n", ""),
        "/repositories/42" => reply(200, "", REPO_42),
        "/repos/off/host" => reply(301, "Location: https://evil.example/repositories/42\r\n", ""),
        "/repos/twice/r" => reply(301, "Location: https://api.github.com/repos/o/r\r\n", ""),
        "/repos/n/r" => reply(200, "", REPO_42),
        "/repos/down/r" => reply(502, "", ""),
        "/graphql" => {
            let q: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert!(q["query"].as_str().unwrap().contains("node(id:$id)"));
            match q["variables"]["id"].as_str().unwrap() {
                "I_1" => reply(
                    200,
                    "",
                    r#"{"data":{"node":{"__typename":"Issue","repository":{"databaseId":42,"id":"R_42","nameWithOwner":"N/R"}}}}"#,
                ),
                "R_42" => reply(
                    200,
                    "",
                    r#"{"data":{"node":{"__typename":"Repository","databaseId":42,"id":"R_42","nameWithOwner":"N/R"}}}"#,
                ),
                _ => reply(200, "", r#"{"data":{"node":null}}"#),
            }
        }
        _ => reply(404, "", r#"{"message":"Not Found"}"#),
    }
}

struct Rig {
    dir: PathBuf,
    upstream: Upstream,
    authority: String,
    cache: Mutex<Cache>,
}

impl Rig {
    fn new(tag: &str, port: u16) -> Rig {
        let dir = std::env::temp_dir().join(format!("dsv-broker-github-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Rig { dir, upstream: client(), authority: format!("localhost:{port}"), cache: Mutex::default() }
    }

    /// `f` with the live lookup at `now`.
    fn at<T>(&self, now: u64, f: impl FnOnce(&Live<'_>) -> T) -> T {
        let https = Https { upstream: &self.upstream, authority: &self.authority, token: &token };
        let state = self.dir.join("state.toml");
        f(&Live { cache: &self.cache, transport: &https, state: &state, now })
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const LOCAL: Via<'static> = Via { credential: "work", private: true };

fn paths(seen: &crate::broker::serve::tests::Seen) -> Vec<String> {
    seen.lock().unwrap().iter().map(|(h, _)| h.lines().next().unwrap().to_string()).collect()
}

#[test]
fn a_rename_is_followed_once_with_the_credential() {
    let (port, seen) = upstream_server(github);
    let rig = Rig::new("rename", port);
    assert_eq!(rig.at(1000, |l| l.by_name(&LOCAL, "o/r")), Some(repo(42, "N/R")));
    assert_eq!(paths(&seen), ["GET /repos/o/r HTTP/1.1", "GET /repositories/42 HTTP/1.1"]);
    for (head, _) in seen.lock().unwrap().iter() {
        assert!(head.contains(&format!("\r\nAuthorization: Bearer {TOKEN}-work\r\n")), "{head}");
        assert!(head.contains("\r\nAccept: application/vnd.github+json\r\n"), "{head}");
        assert!(head.contains("\r\nUser-Agent: devsandbox-broker\r\n"), "{head}");
    }
    // Cached under its name, id and node id.
    assert_eq!(rig.at(1001, |l| (l.by_id(&LOCAL, 42), l.node(&LOCAL, "R_42"))), (Some(repo(42, "N/R")), Some(repo(42, "N/R"))));
    assert_eq!(seen.lock().unwrap().len(), 2);

    assert_eq!(rig.at(1000, |l| l.by_name(&LOCAL, "off/host")), None, "never followed off api.github.com");
    assert_eq!(rig.at(1000, |l| l.by_name(&LOCAL, "twice/r")), None, "a second redirect isn't followed");
    assert_eq!(paths(&seen)[2..], ["GET /repos/off/host HTTP/1.1", "GET /repos/twice/r HTTP/1.1", "GET /repos/o/r HTTP/1.1"]);
}

#[test]
fn answers_are_cached_per_credential_until_their_ttl() {
    let (port, seen) = upstream_server(github);
    let rig = Rig::new("ttl", port);
    let n = || seen.lock().unwrap().len();
    assert_eq!(rig.at(1000, |l| l.by_name(&LOCAL, "n/r")), Some(repo(42, "N/R")));
    assert_eq!(rig.at(1000 + TTL - 1, |l| l.by_name(&LOCAL, "N/R")), Some(repo(42, "N/R")));
    assert_eq!(n(), 1, "cached, case-folded");
    let other = Via { credential: "home", ..LOCAL };
    rig.at(1000, |l| l.by_name(&other, "n/r"));
    assert_eq!(n(), 2, "another credential asks itself");
    rig.at(1000 + TTL, |l| l.by_name(&LOCAL, "n/r"));
    assert_eq!(n(), 3, "expired");

    assert_eq!(rig.at(1000, |l| l.by_name(&LOCAL, "gone/r")), None);
    rig.at(1000 + MISSING_TTL - 1, |l| l.by_name(&LOCAL, "gone/r"));
    assert_eq!(n(), 4, "a 404 is cached");
    rig.at(1000 + MISSING_TTL, |l| l.by_name(&LOCAL, "gone/r"));
    assert_eq!(n(), 5);

    assert_eq!(rig.at(1000, |l| l.by_name(&LOCAL, "down/r")), None);
    rig.at(1000 + FAILED_TTL - 1, |l| l.by_name(&LOCAL, "down/r"));
    assert_eq!(n(), 6, "a failure too, briefly");
    rig.at(1000 + FAILED_TTL, |l| l.by_name(&LOCAL, "down/r"));
    assert_eq!(n(), 7);

    let none = Via { credential: "none", ..LOCAL };
    assert_eq!(rig.at(1000, |l| l.by_name(&none, "n/r")), None, "no token, no call");
    assert_eq!(n(), 7);
}

#[test]
fn only_repository_nodes_are_cached() {
    let (port, seen) = upstream_server(github);
    let rig = Rig::new("nodes", port);
    for _ in 0..2 {
        assert_eq!(rig.at(1000, |l| l.node(&LOCAL, "I_1")), Some(repo(42, "N/R")));
    }
    assert_eq!(seen.lock().unwrap().len(), 2, "an issue is re-resolved each time");
    assert!(seen.lock().unwrap()[0].0.starts_with("POST /graphql HTTP/1.1\r\n"));
    assert_eq!(rig.at(1000, |l| l.node(&LOCAL, "R_42")), Some(repo(42, "N/R")), "the issue's repository, cached");
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(rig.at(1000, |l| l.node(&LOCAL, "X_0")), None);
    assert_eq!(rig.at(1000, |l| l.node(&LOCAL, "X_0")), None);
    assert_eq!(seen.lock().unwrap().len(), 4, "nor a miss");
}

#[test]
fn pins_land_in_state_once() {
    let (port, _) = upstream_server(github);
    let rig = Rig::new("pins", port);
    rig.at(1000, |l| {
        l.pin("o/r", &repo(42, "N/R"));
        l.pin("o/r", &repo(99, "o/r"));
    });
    let state = State::load_at(&rig.dir.join("state.toml")).unwrap();
    assert_eq!(state.github_ids["o/r"], GithubPin { id: 42, node_id: "R_42".into(), pinned_at: 1000 });
}
