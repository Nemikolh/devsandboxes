use super::*;
use crate::broker::head::parse_head;
use crate::credentials::home::{Entry, Version};
use crate::credentials::instance::plan;

pub(crate) const OWN: &str = "sntrys_own";
pub(crate) const FOREIGN: &str = "sntrys_foreign";
pub(crate) const OTHER_CLAIM: &str = "dsbx_anth_x";
pub(crate) const GH: &str = "dsbx_github_own";

pub(crate) fn sentry() -> claim::Claim {
    crate::credentials::home::tests::sentry()
}

/// A second claim on the instance, its slot `x-api-key`.
fn anth() -> claim::Claim {
    let v = serde_json::json!({
        "upstreams": ["api.anthropic.com"],
        "mode": "base-url",
        "baseUrl": "ANTHROPIC_BASE_URL",
        "inject": [{ "header": "x-api-key", "format": "{token}" }],
        "phantom": { "env": ["ANTHROPIC_API_KEY"] },
        "login": [{ "method": "paste" }],
    });
    claim::parse("anth", &v).unwrap()
}

/// An intercept-mode claim on two hosts, its slot `Authorization: token`.
pub(crate) fn github() -> claim::Claim {
    let v = serde_json::json!({
        "upstreams": ["api.github.com", "github.com"],
        "mode": "intercept",
        "inject": [{ "header": "Authorization", "format": "token {token}" }],
        "phantom": { "env": ["GH_TOKEN"] },
        "login": [{ "method": "paste" }],
        "scopes": { "user.read": { "routes": ["GET /user"] } },
    });
    claim::parse("github", &v).unwrap()
}

/// `web-1` with sentry ([`OWN`]) and `github` ([`GH`]), `other` with
/// github ([`FOREIGN`]).
pub(crate) fn intercept_state(github: claim::Claim) -> State {
    let mut state = State::default();
    state.instances.insert("web-1".into(), instance("web-1", claims(&[sentry(), github.clone()], &[OWN, GH])));
    state.instances.insert("other".into(), instance("other", claims(&[github], &[FOREIGN])));
    state
}

/// [`home_with`] plus `github-work`, granted to `work`, with `claim` approved.
pub(crate) fn intercept_home(claim: &claim::Claim) -> Home {
    let mut home = home_with(&sentry());
    let mut e = Entry::new("github", Some(vec!["work".into()]));
    e.approve(Version::of(claim));
    home.insert("github-work".into(), e);
    home
}

/// `key` (id `<key>-id`) in profile `work` with `claims`, as `state.toml` holds it.
pub(crate) fn instance(key: &str, claims: Vec<InstanceClaim>) -> Instance {
    let state: State = toml::from_str(&format!(
        "[instance.{key}]\nsandbox = \"web\"\ninstance_id = \"{key}-id\"\ncontainer = \"devsandbox-{key}\"\n\
         folder = \"/src/web\"\nworkspace = \"/workspaces/web\"\ncreated_unix = 1\nprofile = \"work\"\n\
         incarnation = \"inc-{key}\"\n"
    ))
    .unwrap();
    let mut inst = state.instances.into_values().next().unwrap();
    inst.claims = claims;
    inst
}

/// Claims for `selected` with phantoms in order.
pub(crate) fn claims(selected: &[claim::Claim], phantoms: &[&str]) -> Vec<InstanceClaim> {
    let selected: Vec<(String, claim::Claim)> = selected.iter().map(|c| (format!("./{}", c.kind), c.clone())).collect();
    let mut next = phantoms.iter();
    plan(&selected, &[], None, None, None, |_| Ok(next.next().unwrap().to_string())).unwrap().claims
}

/// `web-1` with sentry (+ anth) and `other` with sentry.
pub(crate) fn state_with(claim: claim::Claim) -> State {
    let mut state = State::default();
    state.instances.insert("web-1".into(), instance("web-1", claims(&[claim.clone(), anth()], &[OWN, OTHER_CLAIM])));
    state.instances.insert("other".into(), instance("other", claims(&[claim], &[FOREIGN])));
    state
}

pub(crate) fn caller() -> Caller {
    Caller { instance: "web-1".into(), instance_id: "web-1-id".into() }
}

/// `sentry-work`, granted to `work`, with `claim` approved.
pub(crate) fn home_with(claim: &claim::Claim) -> Home {
    let mut e = Entry::new("sentry", Some(vec!["work".into()]));
    e.approve(Version::of(claim));
    Home::from([("sentry-work".to_string(), e)])
}

pub(crate) struct Src {
    pub home: Home,
    pub access: Option<Access>,
}

impl Sources for Src {
    fn home(&self) -> Result<Home> {
        Ok(self.home.clone())
    }
    fn access(&self, _: &Instance, kind: &str) -> Result<Option<Access>> {
        assert!(["sentry", "github"].contains(&kind), "{kind}");
        Ok(self.access.clone())
    }
}

fn port(state: &State, upstream: &str) -> u16 {
    state.instances["web-1"].claims[0].listeners.iter().find(|l| l.upstream == upstream).unwrap().port
}

struct Case {
    state: State,
    src: Src,
    upstream: String,
    port: u16,
}

impl Case {
    fn new() -> Case {
        let state = state_with(sentry());
        let port = port(&state, "sentry.io");
        Case { state, src: Src { home: home_with(&sentry()), access: None }, upstream: "sentry.io".into(), port }
    }

    /// `extra` header lines, CRLF-terminated.
    fn run(&self, method: &str, path: &str, extra: &str) -> Outcome {
        let head =
            parse_head(format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{extra}\r\n", self.port).as_bytes())
                .unwrap();
        let header = ProxyHeader::BaseUrl { upstream: self.upstream.clone(), listener: self.port };
        decide(&Call { caller: &caller(), header: &header, head: &head, lfs: None }, &self.state, &self.src)
    }

    fn get(&self, path: &str, extra: &str) -> Outcome {
        self.run("GET", path, extra)
    }
}

fn bearer(p: &str) -> String {
    format!("Authorization: Bearer {p}\r\n")
}

#[track_caller]
fn refused(o: &Outcome, wants: &str) -> Refusal {
    match &o.action {
        Action::Refuse(r) => {
            let text = format!("{} {}", r.reason, r.todo.as_deref().unwrap_or_default());
            assert!(text.contains(wants), "{wants:?} not in {text:?}");
            r.clone()
        }
        other => panic!("want a refusal with {wants:?}, got {other:?}"),
    }
}

#[track_caller]
fn injected(o: &Outcome) -> &Injection {
    match &o.action {
        Action::Inject(i) => i,
        other => panic!("want an injection, got {other:?}"),
    }
}

#[test]
fn allowed_call_injects_into_the_slot_with_the_homes_copy() {
    let c = Case::new();
    let o = c.get("/api/0/organizations/acme/releases/?cursor=x", &bearer(OWN));
    let i = injected(&o);
    assert_eq!(i.credential, "sentry-work");
    assert_eq!(i.slot, Slot::Header { name: "authorization".into(), format: "Bearer {token}".into() });
    assert!(!i.private);
    assert_eq!(o.context.scope.as_deref(), Some("releases.read"));
    assert_eq!(o.context.kind.as_deref(), Some("sentry"));
    assert_eq!(o.context.phantom, Some(Phantom::Own));
    assert_eq!(o.context.incarnation.as_deref(), Some("inc-web-1"));
    assert_eq!(o.context.shape, Some(Shape::Json { field: "detail".into() }));
    assert!(o.context.listeners.contains(&("us.sentry.io".to_string(), port(&c.state, "us.sentry.io"))));
    // The scheme is case-insensitive.
    injected(&c.get("/api/0/", &format!("Authorization: bearer {OWN}\r\n")));
}

#[test]
fn unknown_instance_or_listener() {
    let mut c = Case::new();
    c.state.instances.get_mut("web-1").unwrap().instance_id = "reused".into();
    refused(&c.get("/api/0/", &bearer(OWN)), "has no record");
    let mut c = Case::new();
    c.state.instances.get_mut("web-1").unwrap().claims.clear();
    refused(&c.get("/api/0/", &bearer(OWN)), "claims no credential");

    let mut c = Case::new();
    c.upstream = "us.sentry.io".into();
    refused(&c.get("/api/0/", &bearer(OWN)), "no broker listener for `us.sentry.io`");
    let mut c = Case::new();
    c.port = 3000;
    refused(&c.get("/api/0/", &bearer(OWN)), "port 3000");
}

#[test]
fn strict_paths_refused_before_anything() {
    let c = Case::new();
    for path in ["/api/0/organizations/acme/../other/", "/api/0/%2e%2e/", "/api//0/", "/api/0/./"] {
        let r = refused(&c.get(path, &bearer(OWN)), "");
        assert_eq!(r.status, 400, "{path}");
        // Without a phantom too: nothing upstream would resolve differently.
        assert!(matches!(c.get(path, "").action, Action::Refuse(_)), "{path}");
    }
}

#[test]
fn no_phantom_goes_uninjected() {
    let c = Case::new();
    for extra in ["".to_string(), bearer("sntrys_someone_elses_real_token"), "X-Api-Key: nope\r\n".into()] {
        let o = c.get("/api/0/whatever", &extra);
        assert!(matches!(o.action, Action::Uninjected { private: false }), "{extra}: {:?}", o.action);
        assert_eq!(o.context.phantom, None);
    }
    // The phantom outside its slot goes as is, never swapped.
    let o = c.get(&format!("/api/0/?token={OWN}"), &format!("X-Note: {OWN}\r\n"));
    assert!(matches!(o.action, Action::Uninjected { .. }), "{:?}", o.action);
    // The wrong scheme isn't the slot's format.
    assert!(matches!(c.get("/api/0/", &format!("Authorization: token {OWN}\r\n")).action, Action::Uninjected { .. }));
}

#[test]
fn private_upstream_without_phantom_is_a_bare_401() {
    let mut claim = sentry();
    claim.pinned.upstreams.iter_mut().find(|u| u.host == "sentry.io").unwrap().private = true;
    let mut c = Case::new();
    c.state = state_with(claim.clone());
    c.src.home = home_with(&claim);
    assert!(matches!(c.get("/api/0/", "").action, Action::Uninjected { private: true }));
    assert!(injected(&c.get("/api/0/", &bearer(OWN))).private);
}

#[test]
fn leaks_are_refused_and_marked() {
    let c = Case::new();
    let o = c.get("/api/0/", &bearer(FOREIGN));
    let r = refused(&o, "another instance's phantom");
    assert_eq!((r.status, r.verdict), (403, "leak"));
    assert_eq!(o.context.phantom, Some(Phantom::Foreign));

    // Another claim's phantom in its own slot, sent to this listener.
    let o = c.get("/api/0/", &format!("X-Api-Key: {OTHER_CLAIM}\r\n"));
    let r = refused(&o, "the `anth` claim's phantom was sent to a `sentry` listener");
    assert_eq!(r.verdict, "leak");
    assert_eq!(o.context.phantom, Some(Phantom::OtherClaim));
    refused(&c.get("/api/0/", &bearer(OTHER_CLAIM)), "`anth` claim's phantom");
}

#[test]
fn repeated_or_doubly_filled_slots() {
    let c = Case::new();
    let r = refused(&c.get("/api/0/", &format!("{}{}", bearer(OWN), bearer(OWN))), "sent more than once");
    assert_eq!(r.status, 400);

    let mut two = sentry();
    two.pinned.inject.push(Slot::Header { name: "x-api-key".into(), format: "{token}".into() });
    let mut c = Case::new();
    c.state = state_with(two.clone());
    c.src.home = home_with(&two);
    refused(&c.get("/api/0/", &format!("{}X-Api-Key: {OWN}\r\n", bearer(OWN))), "both inject slots");
    let o = c.get("/api/0/", &format!("X-Api-Key: {OWN}\r\n"));
    assert_eq!(injected(&o).slot, Slot::Header { name: "x-api-key".into(), format: "{token}".into() });
}

#[test]
fn basic_and_query_slots() {
    let mut claim = sentry();
    claim.pinned.inject = vec![Slot::Basic { username: "x".into() }, Slot::Query { param: "key".into() }];
    let mut c = Case::new();
    c.state = state_with(claim.clone());
    c.src.home = home_with(&claim);
    let basic = String::from_utf8(redact::basic_auth(b"anyone", OWN.as_bytes())).unwrap();
    let o = c.get("/api/0/", &format!("Authorization: Basic {basic}\r\n"));
    assert_eq!(injected(&o).slot, Slot::Basic { username: "x".into() });
    let o = c.get(&format!("/api/0/?a=1&key={OWN}"), "");
    assert_eq!(injected(&o).slot, Slot::Query { param: "key".into() });
    refused(&c.get(&format!("/api/0/?key={OWN}&key=x"), ""), "query parameter `key` is sent more than once");
}

#[test]
fn credential_resolution() {
    let mut c = Case::new();
    c.src.home = Home::new();
    let r = refused(&c.get("/api/0/", &bearer(OWN)), "no `sentry` credential granted to profile `work`");
    assert_eq!(r.todo.as_deref(), Some("devsandbox login sentry"));

    let mut c = Case::new();
    c.src.access = Some(Access { credential: Some("sentry-oss".into()), ..Access::default() });
    refused(&c.get("/api/0/", &bearer(OWN)), "named `sentry-oss`");

    let mut c = Case::new();
    c.src.home.insert("sentry-2".into(), Entry::new("sentry", None));
    refused(&c.get("/api/0/", &bearer(OWN)), "name one in `access.sentry.credential`");

    let mut c = Case::new();
    c.src.home.get_mut("sentry-work").unwrap().profiles = Some(vec!["personal".into()]);
    refused(&c.get("/api/0/", &bearer(OWN)), "granted to profile `work`");
}

#[test]
fn version_must_be_approved_and_the_homes_copy_rules() {
    let mut c = Case::new();
    c.src.home.get_mut("sentry-work").unwrap().approved.clear();
    let o = c.get("/api/0/", &bearer(OWN));
    let r = refused(&o, "isn't approved for credential `sentry-work`");
    assert_eq!(r.todo.as_deref(), Some("devsandbox access approve sentry-work"));
    assert_eq!(r.credential.as_deref(), Some("sentry-work"));

    // The instance's claim carries a route the approved copy doesn't: the
    // copy decides. Same version on purpose (a tampered record).
    let mut c = Case::new();
    let mut narrower = sentry();
    narrower.pinned.scopes.remove("releases.read");
    let entry = c.src.home.get_mut("sentry-work").unwrap();
    entry.approved[0].claim = narrower.canonical();
    let o = c.get("/api/0/organizations/acme/releases/", &bearer(OWN));
    refused(&o, "scope `other`");

    let mut c = Case::new();
    let mut moved = sentry();
    moved.pinned.upstreams.retain(|u| u.host != "sentry.io");
    c.src.home.get_mut("sentry-work").unwrap().approved[0].claim = moved.canonical();
    refused(&c.get("/api/0/", &bearer(OWN)), "`sentry.io` isn't an upstream of the approved");

    let mut c = Case::new();
    let mut other_slot = sentry();
    other_slot.pinned.inject = vec![Slot::Header { name: "authorization".into(), format: "token {token}".into() }];
    c.src.home.get_mut("sentry-work").unwrap().approved[0].claim = other_slot.canonical();
    refused(&c.get("/api/0/", &bearer(OWN)), "doesn't inject where the phantom is");
}

#[test]
fn scopes_never_other_ask_and_defaults() {
    let c = Case::new();
    let o = c.run("POST", "/api/0/projects/acme/web/keys/", &bearer(OWN));
    refused(&o, "a `never` route");
    assert_eq!(o.context.scope.as_deref(), Some("never"));
    let o = c.get("/api/0/unknown/", &bearer(OWN));
    let r = refused(&o, "(scope `other`): asks need permission requests (credential-broker step 4)");
    assert_eq!(r.scope.as_deref(), Some("other"));

    // Unset scopes and max: every scope.
    for (method, path) in
        [("GET", "/api/0/"), ("POST", "/api/0/organizations/acme/releases/"), ("GET", "/api/0/issues/42/")]
    {
        injected(&c.run(method, path, &bearer(OWN)));
    }

    let mut c = Case::new();
    c.src.access = Some(Access { scopes: vec!["org.read".into()], ..Access::default() });
    injected(&c.get("/api/0/", &bearer(OWN)));
    let r = refused(&c.get("/api/0/issues/1/", &bearer(OWN)), "scope `issues.read` isn't in `access.sentry.scopes`");
    assert!(r.todo.unwrap().contains("access.sentry.scopes"));

    let mut c = Case::new();
    c.src.access = Some(Access { ask: vec!["issues.read".into()], ..Access::default() });
    refused(&c.get("/api/0/issues/1/", &bearer(OWN)), "is in `access.sentry.ask`: asks need permission requests");

    let mut c = Case::new();
    c.src.home.get_mut("sentry-work").unwrap().max = Some(vec!["org.read".into()]);
    injected(&c.get("/api/0/", &bearer(OWN)));
    let r = refused(&c.get("/api/0/issues/1/", &bearer(OWN)), "isn't granted by credential `sentry-work` (its `max`)");
    assert_eq!(r.scope.as_deref(), Some("issues.read"));
}

/// A call through intercept to `host:port`.
fn through(state: &State, src: &Src, host: &str, port: u16, path: &str, extra: &str) -> Outcome {
    let head = parse_head(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n").as_bytes()).unwrap();
    let header = ProxyHeader::Intercept { host: host.into(), port };
    decide(&Call { caller: &caller(), header: &header, head: &head, lfs: None }, state, src)
}

#[test]
fn intercept_matches_the_claim_by_host() {
    let state = intercept_state(github());
    let src = Src { home: intercept_home(&github()), access: None };
    let token = |p: &str| format!("Authorization: token {p}\r\n");
    for host in ["api.github.com", "github.com"] {
        let o = through(&state, &src, host, 443, "/user", &token(GH));
        let i = injected(&o);
        assert_eq!((i.credential.as_str(), o.context.kind.as_deref()), ("github-work", Some("github")));
        assert_eq!(o.context.phantom, Some(Phantom::Own));
        assert!(o.context.listeners.is_empty());
    }
    assert!(matches!(through(&state, &src, "github.com", 443, "/user", "").action, Action::Uninjected { .. }));
    refused(&through(&state, &src, "api.github.com", 443, "/repos/a/b", &token(GH)), "scope `other`");

    // A base-URL claim's upstream, an unbound host, another port.
    for (host, port) in [("sentry.io", 443), ("example.com", 443), ("api.github.com", 8443)] {
        let o = through(&state, &src, host, port, "/user", &token(GH));
        refused(&o, &format!("`{host}:{port}` is no intercepted upstream of instance `web-1`"));
    }
    let r = admit_intercept(&state, &Caller { instance: "web-1".into(), instance_id: "x".into() }, "github.com", 443);
    assert!(r.unwrap_err().reason.contains("has no record"));
    assert_eq!(admit_intercept(&state, &caller(), "github.com", 443).unwrap().kind, "github");
}

#[test]
fn intercept_leaks_and_mode_mismatches() {
    let state = intercept_state(github());
    let src = Src { home: intercept_home(&github()), access: None };
    // The base-URL claim's phantom in its own slot, sent to the intercepted host.
    let o = through(&state, &src, "api.github.com", 443, "/user", &bearer(OWN));
    let r = refused(&o, "the `sentry` claim's phantom was sent to the `github` claim's `api.github.com`");
    assert_eq!((r.verdict, o.context.phantom), ("leak", Some(Phantom::OtherClaim)));
    let o = through(&state, &src, "api.github.com", 443, "/user", &format!("Authorization: token {FOREIGN}\r\n"));
    assert_eq!(o.context.phantom, Some(Phantom::Foreign));

    // An intercept claim's upstream reached as a base-URL listener (`run`
    // gives intercept claims none: a hand-edited record).
    let mut state = state;
    let listener = 29900;
    let gh = &mut state.instances.get_mut("web-1").unwrap().claims[1];
    gh.listeners.push(crate::credentials::instance::Listener { port: listener, upstream: "api.github.com".into() });
    let head = parse_head(format!("GET /user HTTP/1.1\r\nHost: 127.0.0.1:{listener}\r\nAuthorization: token {GH}\r\n\r\n").as_bytes())
        .unwrap();
    let header = ProxyHeader::BaseUrl { upstream: "api.github.com".into(), listener };
    let o = decide(&Call { caller: &caller(), header: &header, head: &head, lfs: None }, &state, &src);
    refused(&o, &format!("port {listener} is no broker listener for `api.github.com`"));
}

/// [`intercept_state`] with the repo-fenced github fixture, `web-1`'s
/// folders recording `o/r`.
pub(crate) fn fenced_state() -> State {
    let mut state = intercept_state(crate::credentials::home::tests::github());
    state.instances.get_mut("web-1").unwrap().github_repos = vec!["o/r".into()];
    state
}

pub(crate) fn fenced_src(access: Option<Access>) -> Src {
    Src { home: intercept_home(&crate::credentials::home::tests::github()), access }
}

/// git's `Authorization: Basic` with `p` as the password.
pub(crate) fn git_basic(p: &str) -> String {
    let b = String::from_utf8(redact::basic_auth(b"x-access-token", p.as_bytes())).unwrap();
    format!("Authorization: Basic {b}\r\n")
}

fn fenced(src: &Src, method: &str, host: &str, path: &str, extra: &str) -> Outcome {
    let head = parse_head(format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n").as_bytes()).unwrap();
    let header = ProxyHeader::Intercept { host: host.into(), port: 443 };
    decide(&Call { caller: &caller(), header: &header, head: &head, lfs: None }, &fenced_state(), src)
}

const UPLOAD_REFS: &str = "/o/r.git/info/refs?service=git-upload-pack";

#[test]
fn fence_admits_its_repos_with_either_slot() {
    let src = fenced_src(None);
    let o = fenced(&src, "GET", "github.com", UPLOAD_REFS, &git_basic(GH));
    let i = injected(&o);
    assert_eq!(i.slot, Slot::Basic { username: "x-access-token".into() });
    assert_eq!((o.context.scope.as_deref(), o.context.outside.as_deref()), (Some("repo.read"), None));
    assert_eq!(o.context.shape, Some(Shape::Git {}), "git's shape for smart HTTP");
    // Case-folded, `.git`/`.wiki.git` reduced, the REST form.
    injected(&fenced(&src, "GET", "github.com", "/O/R.wiki.git/info/refs?service=git-upload-pack", &git_basic(GH)));
    injected(&fenced(&src, "POST", "github.com", "/o/r.git/git-upload-pack", &git_basic(GH)));
    let o = fenced(&src, "POST", "api.github.com", "/repos/o/R/pulls", &format!("Authorization: token {GH}\r\n"));
    assert_eq!(injected(&o).slot, Slot::Header { name: "authorization".into(), format: "token {token}".into() });
    assert_eq!(o.context.shape, Some(Shape::Json { field: "message".into() }));
}

#[test]
fn outside_the_fence_reads_go_stripped_and_writes_are_refused() {
    let src = fenced_src(None);
    let o = fenced(&src, "GET", "github.com", "/x/y.git/info/refs?service=git-upload-pack", &git_basic(GH));
    assert!(
        matches!(&o.action, Action::Outside { slot: Slot::Basic { .. }, private: false }),
        "{:?}",
        o.action
    );
    assert_eq!(o.context.outside.as_deref(), Some("x/y"));
    assert!(matches!(fenced(&src, "POST", "github.com", "/x/y/git-upload-pack", &git_basic(GH)).action, Action::Outside { .. }));
    let token = format!("Authorization: token {GH}\r\n");
    assert!(matches!(fenced(&src, "GET", "api.github.com", "/repos/x/y", &token).action, Action::Outside { .. }));

    for (method, host, path) in [
        ("POST", "api.github.com", "/repos/x/y/pulls"),
        ("GET", "github.com", "/x/y.git/info/refs?service=git-receive-pack"),
        ("POST", "github.com", "/x/y.git/git-receive-pack"),
    ] {
        let auth = if host == "github.com" { git_basic(GH) } else { token.clone() };
        let o = fenced(&src, method, host, path, &auth);
        let r = refused(&o, "`x/y` is outside the instance's repo fence: no credential writes to it");
        assert_eq!(r.todo.as_deref(), Some("add `x/y` to `access.github.repos` in the sandbox's config"), "{path}");
    }
    // `never` wins over the fence.
    refused(&fenced(&src, "GET", "api.github.com", "/repos/x/y/keys", &token), "a `never` route");
}

#[test]
fn config_repos_widen_the_fence() {
    let src = fenced_src(Some(Access { repos: vec!["X/Y.git".into(), "https://github.com/a/b".into(), "junk".into()], ..Access::default() }));
    injected(&fenced(&src, "POST", "github.com", "/x/y.git/git-receive-pack", &git_basic(GH)));
    injected(&fenced(&src, "GET", "github.com", "/a/b/info/refs?service=git-upload-pack", &git_basic(GH)));
    injected(&fenced(&src, "GET", "github.com", UPLOAD_REFS, &git_basic(GH)));
    assert_eq!(
        fence_of(&fenced_state().instances["web-1"], &src.access.clone().unwrap()),
        ["a/b", "o/r", "x/y"].map(String::from).into()
    );
}

#[test]
fn a_push_in_the_fence_still_needs_its_scope() {
    let src = fenced_src(Some(Access { scopes: vec!["repo.read".into()], ..Access::default() }));
    injected(&fenced(&src, "GET", "github.com", UPLOAD_REFS, &git_basic(GH)));
    let o = fenced(&src, "GET", "github.com", "/o/r.git/info/refs?service=git-receive-pack", &git_basic(GH));
    let r = refused(&o, "scope `git.push` isn't in `access.github.scopes`");
    assert_eq!(r.scope.as_deref(), Some("git.push"));
    assert_eq!(o.context.shape, Some(Shape::Git {}));
}

/// The LFS listener port [`with_lfs`] gives an instance.
pub(crate) const LFS_PORT: u16 = 29005;

/// `inst`'s intercept side with the LFS listener on [`LFS_PORT`].
pub(crate) fn with_lfs(inst: &mut Instance) {
    inst.intercept = Some(crate::credentials::instance::Intercept {
        port: 29003,
        hosts: vec!["api.github.com".into()],
        git: None,
        lfs: Some(LFS_PORT),
        proxy_env: Vec::new(),
        bundle: "/b".into(),
        installed: None,
    });
}

fn batch(state: &State, src: &Src, path: &str, op: Option<route::LfsOp>) -> Outcome {
    let head = parse_head(format!("POST {path} HTTP/1.1\r\nHost: github.com\r\n{}Content-Length: 2\r\n\r\n", git_basic(GH)).as_bytes())
        .unwrap();
    let header = ProxyHeader::Intercept { host: "github.com".into(), port: 443 };
    decide(&Call { caller: &caller(), header: &header, head: &head, lfs: op }, state, src)
}

#[test]
fn lfs_batches_read_or_write_by_their_operation() {
    use route::LfsOp::{Download, Upload};
    let mut state = fenced_state();
    with_lfs(state.instances.get_mut("web-1").unwrap());
    let src = fenced_src(None);
    let inside = "/o/r.git/info/lfs/objects/batch";
    let o = batch(&state, &src, inside, Some(Download));
    assert_eq!((injected(&o).lfs, o.context.scope.as_deref()), (Some(LFS_PORT), Some("repo.read")));
    // An upload needs the claim's `lfs.upload` scope, not the route's.
    let o = batch(&state, &src, inside, Some(Upload));
    assert_eq!((injected(&o).lfs, o.context.scope.as_deref()), (Some(LFS_PORT), Some("git.push")));
    let read_only = fenced_src(Some(Access { scopes: vec!["repo.read".into()], ..Access::default() }));
    injected(&batch(&state, &read_only, inside, Some(Download)));
    let r = refused(&batch(&state, &read_only, inside, Some(Upload)), "scope `git.push` isn't in `access.github.scopes`");
    assert_eq!(r.scope.as_deref(), Some("git.push"));

    // Outside the fence a download goes stripped, an upload is refused.
    let outside = "/x/y.git/info/lfs/objects/batch";
    let o = batch(&state, &src, outside, Some(Download));
    assert!(matches!(o.action, Action::Outside { .. }), "{:?}", o.action);
    refused(&batch(&state, &src, outside, Some(Upload)), "`x/y` is outside the instance's repo fence: no credential writes");
    refused(&batch(&state, &src, outside, None), "no credential writes");

    // No LFS listener on the instance: nothing to point transfers at.
    let o = batch(&fenced_state(), &src, inside, Some(Download));
    assert_eq!(injected(&o).lfs, None);
    // Nor for a claim approved without `lfs`.
    let mut plain = crate::credentials::home::tests::github();
    plain.pinned.lfs = None;
    let mut home = fenced_src(None).home;
    home.get_mut("github-work").unwrap().approved[0].claim = plain.canonical();
    let o = batch(&state, &Src { home, access: None }, inside, Some(Upload));
    assert_eq!((injected(&o).lfs, o.context.scope.as_deref()), (None, Some("repo.read")));
}

#[test]
fn lfs_locks_list_as_reads_and_verify_as_writes() {
    let src = fenced_src(None);
    let o = fenced(&src, "GET", "github.com", "/o/r.git/info/lfs/locks?path=a", &git_basic(GH));
    assert_eq!((injected(&o).lfs, o.context.scope.as_deref()), (None, Some("repo.read")));
    for path in ["/o/r.git/info/lfs/locks/verify", "/o/r.git/info/lfs/locks", "/o/r.git/info/lfs/locks/12/unlock"] {
        assert_eq!(fenced(&src, "POST", "github.com", path, &git_basic(GH)).context.scope.as_deref(), Some("git.push"), "{path}");
    }
    assert!(matches!(fenced(&src, "GET", "github.com", "/x/y.git/info/lfs/locks", &git_basic(GH)).action, Action::Outside { .. }));
    refused(&fenced(&src, "POST", "github.com", "/x/y.git/info/lfs/locks/verify", &git_basic(GH)), "no credential writes");
}

#[test]
fn repo_less_and_unplaced_routes() {
    let token = format!("Authorization: token {GH}\r\n");
    let o = fenced(&fenced_src(None), "GET", "api.github.com", "/user", &token);
    assert_eq!(o.context.outside, None);
    injected(&o);
    let ask = fenced_src(Some(Access { ask: vec!["user.read".into(), "repo.read".into()], ..Access::default() }));
    refused(&fenced(&ask, "GET", "api.github.com", "/user", &token), "names no repo, so it's never asked");
    refused(&fenced(&ask, "GET", "api.github.com", "/repos/o/r", &token), "asks need permission requests");

    // A body input isn't placed.
    let mut body = crate::credentials::home::tests::github();
    let pulls = claim::Route::parse("POST /repos/{repo}/pulls").unwrap();
    body.pinned.fence.insert(pulls, vec!["{repo}".into(), "head".into()]);
    let mut src = fenced_src(None);
    src.home.get_mut("github-work").unwrap().approved[0].claim = body.canonical();
    let o = fenced(&src, "POST", "api.github.com", "/repos/o/r/pulls", &token);
    refused(&o, "`POST /repos/o/r/pulls` names its repo by `head`, which the broker can't resolve yet");
    // Unknown routes stay `other`.
    refused(&fenced(&fenced_src(None), "GET", "api.github.com", "/repos/o/r/hooks", &token), "scope `other`");
    // A path past the repo that decodes badly.
    let o = fenced(&fenced_src(None), "GET", "api.github.com", "/repos/o/r/contents/a%2F..%2Fb", &token);
    assert_eq!(refused(&o, "dot-segment past the repo").status, 400);
}

#[test]
fn stripping_removes_only_the_slot() {
    let mut headers =
        parse_head(format!("GET / HTTP/1.1\r\nAuthorization: token {GH}\r\nGit-Protocol: version=2\r\n\r\n").as_bytes())
            .unwrap()
            .headers;
    let mut path = format!("/x?a=1&key={GH}");
    strip(&mut headers, &mut path, &Slot::Basic { username: "u".into() });
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].name, "Git-Protocol");
    strip(&mut headers, &mut path, &Slot::Query { param: "key".into() });
    assert_eq!(path, "/x?a=1");
    strip(&mut headers, &mut path, &Slot::Query { param: "a".into() });
    assert_eq!(path, "/x");
}

#[test]
fn injection_rewrites_only_the_slot() {
    let mut headers = parse_head(
        format!("GET /x?key={OWN}&b=2 HTTP/1.1\r\nAuthorization: Bearer {OWN}\r\nX-Note: {OWN}\r\n\r\n").as_bytes(),
    )
    .unwrap()
    .headers;
    let mut path = format!("/x?key={OWN}&b=2");
    let bearer = Slot::Header { name: "authorization".into(), format: "Bearer {token}".into() };
    inject(&mut headers, &mut path, &bearer, "real");
    assert_eq!(headers[0].value, b"Bearer real");
    assert_eq!(headers[1].value, format!("X-Note: {OWN}").split_once(": ").unwrap().1.as_bytes());
    assert_eq!(path, format!("/x?key={OWN}&b=2"));

    inject(&mut headers, &mut path, &Slot::Query { param: "key".into() }, "a b/c");
    assert_eq!(path, "/x?key=a%20b%2Fc&b=2");
    inject(&mut headers, &mut path, &Slot::Basic { username: "u".into() }, "pw");
    assert_eq!(headers[0].value, b"Basic dTpwdw==");
}

#[test]
fn secrets_cover_basic_and_query_forms() {
    let mut claim = sentry().pinned;
    claim.inject.push(Slot::Basic { username: "x-access-token".into() });
    let s = secrets(&claim, "t/k");
    let mut body = b"t/k t%2Fk ".to_vec();
    body.extend(redact::basic_auth(b"x-access-token", b"t/k"));
    assert!(s.mask(&mut body));
    assert!(body.iter().all(|&b| b == b'*' || b == b' '), "{}", String::from_utf8_lossy(&body));
}

/// [`fenced_src`] with GitHub answered by `fake`.
struct Resolving {
    src: Src,
    fake: github::tests::Fake,
}

impl Sources for Resolving {
    fn home(&self) -> Result<Home> {
        self.src.home()
    }
    fn access(&self, inst: &Instance, kind: &str) -> Result<Option<Access>> {
        self.src.access(inst, kind)
    }
    fn github(&self) -> Option<&dyn github::Lookup> {
        Some(&self.fake)
    }
}

fn resolving(state: &State, src: &Resolving, method: &str, host: &str, path: &str) -> Outcome {
    let auth = if host == "github.com" { git_basic(GH) } else { format!("Authorization: token {GH}\r\n") };
    let head = parse_head(format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n{auth}\r\n").as_bytes()).unwrap();
    let header = ProxyHeader::Intercept { host: host.into(), port: 443 };
    decide(&Call { caller: &caller(), header: &header, head: &head, lfs: None }, state, src)
}

#[test]
fn repository_ids_are_placed_by_the_fences_pins() {
    use github::tests::{Fake, repo};
    let src = Resolving {
        src: fenced_src(None),
        fake: Fake::default().with(repo(42, "o/r"), &["o/r"]).with(repo(7, "x/y"), &["x/y"]),
    };
    let state = fenced_state();
    let o = resolving(&state, &src, "GET", "api.github.com", "/repositories/42");
    assert_eq!((injected(&o).path.as_deref(), o.context.scope.as_deref()), (None, Some("repo.read")));
    assert_eq!(*src.fake.pins.borrow(), [("o/r".to_string(), 42)], "pinned at its first resolution");

    let o = resolving(&state, &src, "GET", "api.github.com", "/repositories/7");
    assert!(matches!(o.action, Action::Outside { .. }), "{:?}", o.action);
    assert_eq!(o.context.outside.as_deref(), Some("x/y"));
    let o = resolving(&state, &src, "GET", "api.github.com", "/repositories/8");
    assert_eq!(o.context.outside.as_deref(), Some("repository 8"));
    for bad in ["/repositories/4x2", "/repositories/99999999999999999999999"] {
        assert_eq!(refused(&resolving(&state, &src, "GET", "api.github.com", bad), "names no repository id").status, 400);
    }

    // Without a lookup only a pin places an id.
    let o = fenced(&fenced_src(None), "GET", "api.github.com", "/repositories/42", &format!("Authorization: token {GH}\r\n"));
    assert_eq!(o.context.outside.as_deref(), Some("repository 42"));
    let mut pinned = fenced_state();
    pinned.github_ids.insert("o/r".into(), crate::state::GithubPin { id: 42, node_id: "R_42".into(), pinned_at: 1 });
    let o = decide(
        &Call {
            caller: &caller(),
            header: &ProxyHeader::Intercept { host: "api.github.com".into(), port: 443 },
            head: &parse_head(format!("GET /repositories/42 HTTP/1.1\r\nHost: api.github.com\r\nAuthorization: token {GH}\r\n\r\n").as_bytes())
                .unwrap(),
            lfs: None,
        },
        &pinned,
        &fenced_src(None),
    );
    injected(&o);
}

#[test]
fn a_renamed_repo_is_reached_by_either_name_at_its_current_one() {
    use github::tests::{Fake, repo};
    // `o/r` (recorded) is now `N/R`.
    let src = Resolving { src: fenced_src(None), fake: Fake::default().with(repo(42, "N/R"), &["o/r", "n/r"]) };
    let state = fenced_state();
    let o = resolving(&state, &src, "POST", "api.github.com", "/repos/o/r/pulls");
    assert_eq!(injected(&o).path.as_deref(), Some("/repos/N/R/pulls"));
    let o = resolving(&state, &src, "GET", "github.com", UPLOAD_REFS);
    assert_eq!(injected(&o).path.as_deref(), Some("/N/R.git/info/refs?service=git-upload-pack"));
    let o = resolving(&state, &src, "POST", "api.github.com", "/repos/n/r/pulls");
    assert_eq!((injected(&o).path.as_deref(), o.context.outside.as_deref()), (None, None));
    let r = refused(&resolving(&state, &src, "POST", "api.github.com", "/repos/x/y/pulls"), "outside the instance's repo fence");
    assert_eq!(r.todo.as_deref(), Some("add `x/y` to `access.github.repos` in the sandbox's config"));
}

#[test]
fn a_re_registered_old_name_never_gets_the_credential() {
    use github::tests::{Fake, repo};
    // `o/r` was pinned to 42, renamed `n/r`; someone else registered `o/r`.
    let fake = Fake::default().with(repo(42, "n/r"), &["n/r"]).with(repo(99, "o/r"), &["o/r"]);
    let src = Resolving { src: fenced_src(None), fake };
    let mut state = fenced_state();
    state.github_ids.insert("o/r".into(), crate::state::GithubPin { id: 42, node_id: "R_42".into(), pinned_at: 1 });
    let o = resolving(&state, &src, "POST", "api.github.com", "/repos/o/r/pulls");
    assert_eq!(injected(&o).path.as_deref(), Some("/repos/n/r/pulls"), "sent to the pinned repository");
    assert!(matches!(resolving(&state, &src, "GET", "api.github.com", "/repositories/99").action, Action::Outside { .. }));
    injected(&resolving(&state, &src, "GET", "api.github.com", "/repositories/42"));
    assert!(src.fake.pins.borrow().is_empty());
}

#[test]
fn a_claim_without_api_github_com_matches_names_only() {
    use github::tests::{Fake, repo};
    let mut plain = crate::credentials::home::tests::github();
    plain.pinned.upstreams.retain(|u| u.host != github::API);
    let mut src = Resolving { src: fenced_src(None), fake: Fake::default().with(repo(42, "N/R"), &["o/r"]) };
    src.src.home.get_mut("github-work").unwrap().approved[0].claim = plain.canonical();
    let o = resolving(&fenced_state(), &src, "GET", "github.com", UPLOAD_REFS);
    assert_eq!(injected(&o).path, None);
    assert!(src.fake.calls.borrow().is_empty());
}
