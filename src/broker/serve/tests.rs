//! End to end over a socket pair: the broker in front of a local TLS
//! server with the test CA (`tests/fixtures/broker/`), the claim's
//! upstreams pointed at it ([`Service::with_authority`]) and marked private
//! so the address check lets loopback through.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned};
use serde_json::Value;

use super::*;
use crate::broker::policy::tests::{
    FOREIGN, GH, LFS_PORT, OWN, caller, git_basic, github, home_with, intercept_home, intercept_state, sentry, state_with,
    with_lfs,
};
use crate::broker::upstream::Timeouts;
use crate::credentials::oauth::tests::{GITHUB_TOKEN, field, form, json, path};
use crate::credentials::store::{Record, Secret, Store};

const TOKEN: &str = "REALtoken-0123456789";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/broker").join(name)
}

/// What the fake upstream received: the raw head and the body.
pub(crate) type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// Answers every connection with `answer(head, body)`, then closes it.
pub(crate) fn upstream_server(answer: impl Fn(&str, &[u8]) -> Vec<u8> + Send + Sync + 'static) -> (u16, Seen) {
    let chain = CertificateDer::pem_file_iter(fixture("localhost.pem")).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("localhost.key")).unwrap();
    let config = Arc::new(
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let answer = Arc::new(answer);
    thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(tcp) = tcp else { return };
            let (config, log, answer) = (config.clone(), log.clone(), answer.clone());
            thread::spawn(move || {
                tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                let mut conn = BufReader::new(StreamOwned::new(ServerConnection::new(config).unwrap(), tcp));
                let Ok(Some(raw)) = head::read_raw_head(&mut conn) else { return };
                let parsed = head::parse_head(&raw).unwrap();
                let mut body = Vec::new();
                BodyReader::new(&mut conn, parsed.framing).read_to_end(&mut body).unwrap();
                let raw = String::from_utf8(raw).unwrap();
                let reply = answer(&raw, &body);
                log.lock().unwrap().push((raw, body));
                let tls = conn.get_mut();
                tls.write_all(&reply).unwrap();
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    (port, seen)
}

struct Rig {
    dir: PathBuf,
    service: Arc<Service>,
    state: State,
}

impl Rig {
    /// `state_with(sentry)` with every upstream private, the home granting
    /// `sentry-work` (token [`TOKEN`]) and a config root, each upstream
    /// dialed at `localhost:<upstream port>`.
    fn new(tag: &str, upstream_port: u16) -> Rig {
        let claim = private(sentry());
        Rig::with(tag, upstream_port, state_with(claim.clone()), home_with(&claim), "sentry-work", &claim)
    }

    /// [`intercept_state`] with github's upstreams private, `github-work`
    /// holding [`TOKEN`].
    fn intercept(tag: &str, upstream_port: u16) -> Rig {
        let claim = private(github());
        let mut state = intercept_state(claim.clone());
        for c in state.instances.values_mut().flat_map(|i| &mut i.claims).filter(|c| c.kind == "github") {
            c.listeners.clear();
        }
        Rig::with(tag, upstream_port, state, intercept_home(&claim), "github-work", &claim)
    }

    /// [`Rig::intercept`] with the repo-fenced github fixture, `web-1`'s
    /// folders recording `o/r`.
    fn fenced(tag: &str, upstream_port: u16) -> Rig {
        let claim = private(crate::credentials::home::tests::github());
        let mut state = intercept_state(claim.clone());
        state.instances.get_mut("web-1").unwrap().github_repos = vec!["o/r".into()];
        Rig::with(tag, upstream_port, state, intercept_home(&claim), "github-work", &claim)
    }

    /// [`Rig::fenced`] with an LFS listener on `web-1` and `other`, LFS
    /// storage host `storage.test` dialed at `localhost:<storage_port>`.
    fn lfs(tag: &str, upstream_port: u16, storage_port: u16) -> Rig {
        let claim = private(crate::credentials::home::tests::github());
        let mut state = intercept_state(claim.clone());
        state.instances.get_mut("web-1").unwrap().github_repos = vec!["o/r".into()];
        state.instances.values_mut().for_each(with_lfs);
        let storage = format!("localhost:{storage_port}");
        let home = intercept_home(&claim);
        Rig::with_service(tag, upstream_port, state, home, "github-work", &claim, |s| s.with_storage("storage.test", &storage))
    }

    fn with(tag: &str, upstream_port: u16, state: State, home: Home, credential: &str, claim: &claim::Claim) -> Rig {
        Rig::with_service(tag, upstream_port, state, home, credential, claim, |s| s)
    }

    fn with_service(
        tag: &str,
        upstream_port: u16,
        mut state: State,
        home: Home,
        credential: &str,
        claim: &claim::Claim,
        extra: impl FnOnce(Service) -> Service,
    ) -> Rig {
        let dir = std::env::temp_dir().join(format!("dsv-broker-serve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = dir.join("data");
        let cfg = dir.join("cfg");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("devsandboxes.toml"), "[sandbox.web]\nfolder = \".\"\n").unwrap();

        for inst in state.instances.values_mut() {
            inst.config_dir = Some(cfg.clone());
        }
        std::fs::write(data.join("state.toml"), toml::to_string(&state).unwrap()).unwrap();
        std::fs::write(home::path(&data), home::serialize(&home).unwrap()).unwrap();
        Store::in_data(&data).put(credential, &Record::token(Secret::new(TOKEN.into()))).unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from_pem_file(fixture("ca.pem")).unwrap()).unwrap();
        let timeouts = Timeouts { connect: Duration::from_secs(5), idle: Duration::from_secs(10) };
        let upstream = Upstream::new(roots, None, timeouts).unwrap();
        let mut service =
            Service::new(data.join("state.toml"), Ok(upstream)).unwrap().with_tokens(crate::credentials::oauth::tests::http());
        for u in &claim.pinned.upstreams {
            service = service.with_authority(&u.host, &format!("localhost:{upstream_port}"));
        }
        Rig { dir, service: Arc::new(extra(service)), state }
    }

    fn port(&self, upstream: &str) -> u16 {
        self.state.instances["web-1"].claims[0].listeners.iter().find(|l| l.upstream == upstream).unwrap().port
    }

    /// `sentry-work` as an OAuth login: [`OLD`] with refresh token
    /// `refresh`, renewed at `https://127.0.0.1:<port>/token`.
    fn oauth(&self, refresh: &str, expires_at: u64, port: u16) {
        let rec = OAuthToken {
            access_token: Secret::new(OLD.into()),
            refresh_token: Some(Secret::new(refresh.into())),
            expires_at: Some(expires_at),
            refresh_expires_at: None,
            token_url: format!("https://127.0.0.1:{port}/token"),
            client_id: "Iv1.c".into(),
            scope: None,
        };
        Store::in_data(&self.dir.join("data")).put("sentry-work", &Record::OAuth(rec)).unwrap();
    }

    fn stored(&self) -> String {
        let token = Store::in_data(&self.dir.join("data")).token_now("sentry-work").unwrap().unwrap();
        token.expose().to_string()
    }

    fn broken(&self, credential: &str) -> Option<home::Broken> {
        home::load(&home::path(&self.dir.join("data"))).unwrap()[credential].broken.clone()
    }

    fn config(&self, text: &str) {
        std::fs::write(self.dir.join("cfg/devsandboxes.toml"), text).unwrap();
    }

    /// A connection to `upstream`'s listener, as `caller` on its bridge.
    fn connect_as(&self, caller: Caller, upstream: &str) -> Client {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let header = ProxyHeader::BaseUrl { upstream: upstream.into(), listener: self.port(upstream) };
        let service = self.service.clone();
        thread::spawn(move || service.serve(&caller, header, theirs));
        Client { host: format!("127.0.0.1:{}", self.port(upstream)), reader: BufReader::new(Box::new(ours)) }
    }

    /// The member CA's cert as the only root, as a container would trust it.
    fn member_roots(&self) -> RootCertStore {
        let ca = MemberCa::load_or_create(&self.dir.join("data")).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(ca.cert_pem().unwrap().as_bytes()).unwrap()).unwrap();
        roots
    }

    /// `CONNECT host:port` as devsbd hands it over, then TLS to `sni`
    /// (`None`: no SNI) offering `alpn`, trusting only the member CA: the
    /// client, or why its handshake failed.
    fn intercept_to(&self, host: &str, port: u16, sni: Option<&str>, alpn: &[&[u8]]) -> Result<Client, String> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let header = ProxyHeader::Intercept { host: host.into(), port };
        let service = self.service.clone();
        thread::spawn(move || service.serve(&caller(), header, theirs));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(self.member_roots())
            .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        config.enable_sni = sni.is_some();
        let name = ServerName::try_from(sni.unwrap_or(host).to_string()).unwrap();
        let mut tls = StreamOwned::new(ClientConnection::new(Arc::new(config), name).unwrap(), ours);
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock).map_err(|e| e.to_string())?;
        }
        if let Some(p) = tls.conn.alpn_protocol() {
            assert_eq!(p, b"http/1.1");
        }
        Ok(Client { host: host.to_string(), reader: BufReader::new(Box::new(tls)) })
    }

    fn connect(&self, upstream: &str) -> Client {
        self.connect_as(caller(), upstream)
    }

    /// A connection to the LFS listener, as `caller` on its bridge.
    fn lfs_listener(&self, caller: Caller) -> Client {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let service = self.service.clone();
        thread::spawn(move || service.serve(&caller, ProxyHeader::Lfs { listener: LFS_PORT }, theirs));
        Client { host: format!("127.0.0.1:{LFS_PORT}"), reader: BufReader::new(Box::new(ours)) }
    }

    /// `access.log` once it has `n` lines: a line lands after its answer.
    fn log(&self, n: usize) -> Vec<Value> {
        let mut text = String::new();
        for _ in 0..100 {
            text = std::fs::read_to_string(self.dir.join("data/access.log")).unwrap_or_default();
            if text.lines().count() >= n {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!text.contains(TOKEN) && !text.contains(OWN) && !text.contains(FOREIGN) && !text.contains(GH), "{text}");
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn private(mut claim: claim::Claim) -> claim::Claim {
    for u in &mut claim.pinned.upstreams {
        u.private = true;
    }
    claim
}

trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send> Duplex for T {}

struct Client {
    /// What `Host` names.
    host: String,
    /// Writes go through `get_mut`.
    reader: BufReader<Box<dyn Duplex>>,
}

#[derive(Debug)]
struct Answer {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&self.body)))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|l| {
            let (n, v) = l.split_once(": ")?;
            n.eq_ignore_ascii_case(name).then_some(v)
        })
    }
}

impl Client {
    fn write(&mut self, bytes: &[u8]) {
        let w = self.reader.get_mut();
        w.write_all(bytes).unwrap();
        w.flush().unwrap();
    }

    /// `extra` header lines, CRLF-terminated; `Host` is [`Client::host`].
    fn send(&mut self, method: &str, path: &str, extra: &str, body: &[u8]) {
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\n{extra}", self.host);
        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        self.write(req.as_bytes());
        self.write(body);
    }

    fn read(&mut self) -> Answer {
        let raw = head::read_raw_head(&mut self.reader).unwrap().expect("an answer");
        let head = String::from_utf8(raw).unwrap();
        let status = head[9..12].parse().unwrap();
        let mut a = Answer { status, head, body: Vec::new() };
        let framing = match (a.header("content-length"), a.header("transfer-encoding")) {
            (Some(n), None) => Framing::Length(n.parse().unwrap()),
            (None, Some("chunked")) => Framing::Chunked,
            (None, None) if status == 100 => Framing::Empty,
            (None, None) => {
                self.reader.read_to_end(&mut a.body).unwrap();
                return a;
            }
            other => panic!("framing {other:?}"),
        };
        BodyReader::new(&mut self.reader, framing).read_to_end(&mut a.body).unwrap();
        a
    }

    fn call(&mut self, method: &str, path: &str, extra: &str, body: &[u8]) -> Answer {
        self.send(method, path, extra, body);
        self.read()
    }

    /// The broker closed its side.
    fn closed(&mut self) -> bool {
        matches!(self.reader.fill_buf(), Ok(b) if b.is_empty())
    }
}

fn bearer(p: &str) -> String {
    format!("Authorization: Bearer {p}\r\n")
}

/// Echoes the request's `Authorization` in a header and the body.
fn echo(head: &str, _: &[u8]) -> Vec<u8> {
    let auth = head.lines().find_map(|l| l.strip_prefix("Authorization: ")).unwrap_or("").to_string();
    let body = format!("{{\"you sent\":\"{auth}\"}}");
    format!("HTTP/1.1 200 OK\r\nX-Echo: {auth}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

#[test]
fn injects_only_the_slot_redacts_echoes_and_keeps_alive() {
    let (port, seen) = upstream_server(echo);
    let rig = Rig::new("inject", port);
    let mut c = rig.connect("sentry.io");
    for path in ["/api/0/", "/api/0/organizations/acme/releases/?q=1"] {
        let a = c.call("GET", path, &format!("{}X-Note: {OWN}\r\n", bearer(OWN)), b"");
        assert_eq!(a.status, 200, "{a:?}");
        let masked = "*".repeat(TOKEN.len());
        assert_eq!(a.header("x-echo"), Some(format!("Bearer {masked}").as_str()), "{}", a.head);
        assert_eq!(a.json()["you sent"], format!("Bearer {masked}"));
        assert!(!a.head.contains("Connection: close"), "{}", a.head);
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one upstream connection per request");
    for (head, _) in seen.iter() {
        assert!(head.contains(&format!("\r\nAuthorization: Bearer {TOKEN}\r\n")), "{head}");
        assert!(head.contains(&format!("\r\nHost: localhost:{port}\r\n")), "{head}");
        // Outside the slot the phantom goes as is.
        assert!(head.contains(&format!("\r\nX-Note: {OWN}\r\n")), "{head}");
        assert_eq!(head.matches(OWN).count(), 1, "{head}");
    }
    assert!(seen[1].0.starts_with("GET /api/0/organizations/acme/releases/?q=1 HTTP/1.1\r\n"));

    let log = rig.log(2);
    assert_eq!(log.len(), 2);
    let l = &log[1];
    assert_eq!(l["verdict"], "allow");
    assert_eq!(l["credential"], "sentry-work");
    assert_eq!(l["scope"], "releases.read");
    assert_eq!(l["route"], "/api/0/organizations/acme/releases/");
    assert_eq!(l["phantom"], "own");
    assert_eq!(l["incarnation"], "inc-web-1");
    assert_eq!(l["status"], 200);
}

#[test]
fn chunk_upload_url_is_rewritten_and_the_post_comes_back_through_the_broker() {
    let (port, seen) = upstream_server(|head, body| {
        if head.starts_with("GET /api/0/organizations/acme/chunk-upload/ ") {
            let json = r#"{"url":"https://us.sentry.io/api/0/organizations/acme/chunk-upload/","chunkSize":8388608}"#;
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{json}\r\n0\r\n\r\n", json.len())
                .into_bytes()
        } else {
            let reply = format!("got {} bytes", body.len());
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{reply}", reply.len()).into_bytes()
        }
    });
    let rig = Rig::new("chunk", port);
    let mut c = rig.connect("sentry.io");
    let a = c.call("GET", "/api/0/organizations/acme/chunk-upload/", &bearer(OWN), b"");
    assert_eq!(a.status, 200, "{a:?}");
    assert!(a.header("transfer-encoding").is_none(), "re-framed with a length: {}", a.head);
    let us = rig.port("us.sentry.io");
    let url = a.json()["url"].as_str().unwrap().to_string();
    assert_eq!(url, format!("http://127.0.0.1:{us}/api/0/organizations/acme/chunk-upload/"));

    let path = url.split_once(&format!(":{us}")).unwrap().1;
    // The body only once the broker said 100.
    let body = b"chunk-bytes";
    let mut req = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{us}\r\n{}", bearer(OWN));
    req.push_str(&format!("Expect: 100-continue\r\nContent-Length: {}\r\n\r\n", body.len()));
    let mut up = rig.connect("us.sentry.io");
    up.write(req.as_bytes());
    assert_eq!(up.read().status, 100);
    up.write(body);
    let a = up.read();
    assert_eq!((a.status, a.body.as_slice()), (200, &b"got 11 bytes"[..]));

    let seen = seen.lock().unwrap();
    let post = seen.iter().find(|(h, _)| h.starts_with("POST ")).unwrap();
    assert!(post.0.contains(&format!("Authorization: Bearer {TOKEN}\r\n")) && !post.0.contains("Expect"), "{}", post.0);
    assert_eq!(post.1, body);
}

#[test]
fn location_points_back_at_the_loopback() {
    let (port, _) = upstream_server(|_, _| {
        b"HTTP/1.1 302 Found\r\nLocation: https://us.sentry.io/api/0/next/\r\nContent-Length: 0\r\n\r\n".to_vec()
    });
    let rig = Rig::new("location", port);
    let a = rig.connect("sentry.io").call("GET", "/api/0/", &bearer(OWN), b"");
    assert_eq!(a.status, 302);
    assert_eq!(a.header("location"), Some(format!("http://127.0.0.1:{}/api/0/next/", rig.port("us.sentry.io")).as_str()));
}

#[test]
fn refusals_take_the_claims_shape_and_never_reach_upstream() {
    let (port, seen) = upstream_server(echo);
    let rig = Rig::new("refuse", port);
    let mut c = rig.connect("sentry.io");
    let a = c.call("GET", "/api/0/unknown/", &bearer(OWN), b"");
    assert_eq!(a.status, 403, "{a:?}");
    assert!(!a.head.to_ascii_lowercase().contains("www-authenticate"));
    let v = a.json();
    assert!(v["detail"].as_str().unwrap().contains("permission requests: not supported yet!"), "{v}");
    assert_eq!(v["devsandbox"]["verdict"], "refused");
    assert_eq!(v["devsandbox"]["scope"], "other");
    assert_eq!(v["devsandbox"]["credential"], "sentry-work");
    // A refusal with no body keeps the connection.
    let a = c.call("GET", "/api/0/organizations/acme/../x/", "", b"");
    assert_eq!(a.status, 400);
    assert!(a.json()["detail"].as_str().unwrap().contains("dot-segment"));
    // With an unread body it closes.
    let a = c.call("POST", "/api/0/projects/a/b/keys/", &bearer(OWN), b"{}");
    assert_eq!(a.status, 403);
    assert!(a.json()["detail"].as_str().unwrap().contains("`never` route"));
    assert!(a.head.contains("Connection: close"), "{}", a.head);
    assert!(c.closed());

    let a = rig.connect("sentry.io").call("GET", "/api/0/", &bearer(FOREIGN), b"");
    assert_eq!((a.status, a.json()["devsandbox"]["verdict"].as_str()), (403, Some("leak")));

    let mut c = rig.connect("sentry.io");
    c.write(b"GET /api/0/ HTTP/1.1\r\nHost: evil.example\r\n\r\n");
    let a = c.read();
    assert_eq!(a.status, 403);
    assert!(a.json()["detail"].as_str().unwrap().contains("Host"));

    let a = rig.connect_as(Caller { instance: "web-1".into(), instance_id: "old".into() }, "sentry.io").call(
        "GET",
        "/api/0/",
        &bearer(OWN),
        b"",
    );
    assert_eq!(a.status, 403);

    assert!(seen.lock().unwrap().is_empty(), "nothing was dialed");
    let verdicts: Vec<String> = rig.log(6).iter().map(|l| l["verdict"].as_str().unwrap().to_string()).collect();
    assert_eq!(verdicts, ["refused", "refused", "refused", "leak", "refused", "refused"]);
    assert_eq!(rig.log(6)[3]["phantom"], "foreign");
}

#[test]
fn config_edits_apply_to_the_next_call() {
    let (port, _) = upstream_server(echo);
    let rig = Rig::new("config", port);
    let mut c = rig.connect("sentry.io");
    assert_eq!(c.call("GET", "/api/0/issues/1/", &bearer(OWN), b"").status, 200);
    rig.config("[sandbox.web]\nfolder = \".\"\n[sandbox.web.access.sentry]\nscopes = [\"org.read\"]\n");
    let a = c.call("GET", "/api/0/issues/1/", &bearer(OWN), b"");
    assert_eq!(a.status, 403);
    assert!(a.json()["detail"].as_str().unwrap().contains("access.sentry.scopes"));
    assert_eq!(c.call("GET", "/api/0/", &bearer(OWN), b"").status, 200);
}

const UNAUTHORIZED: &[u8] = b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 2\r\n\r\nno";

#[test]
fn upstream_401_on_an_injected_call_is_a_login_refusal() {
    let (port, seen) = upstream_server(|_, _| UNAUTHORIZED.to_vec());
    let rig = Rig::new("401", port);
    let a = rig.connect("sentry.io").call("GET", "/api/0/issues/1/", &bearer(OWN), b"");
    assert_eq!(a.status, 403);
    assert!(!a.head.to_ascii_lowercase().contains("www-authenticate"), "{}", a.head);
    assert_eq!(a.json()["devsandbox"]["do"], "devsandbox login sentry-work");
    // The broker's own call to the claim's `check` route got a 401 too.
    let seen = seen.lock().unwrap();
    assert!(seen[1].0.starts_with("GET /api/0/ HTTP/1.1\r\n"), "{}", seen[1].0);
    assert!(seen[1].0.to_ascii_lowercase().contains(&format!("\r\nauthorization: bearer {}\r\n", TOKEN.to_ascii_lowercase())));
    assert_eq!(rig.broken("sentry-work").unwrap().reason, "its `check` route `GET /api/0/` answered 401");
    assert_eq!(rig.log(1).len(), 1, "the check leaves no line");
}

#[test]
fn a_401_the_check_route_doesnt_repeat_marks_nothing() {
    let (port, _) = upstream_server(|head, _| match path(head) {
        "/api/0/" => json(200, "{}"),
        _ => UNAUTHORIZED.to_vec(),
    });
    let rig = Rig::new("401-check-ok", port);
    let a = rig.connect("sentry.io").call("GET", "/api/0/issues/1/", &bearer(OWN), b"");
    assert_eq!(a.status, 403);
    assert_eq!(rig.broken("sentry-work"), None);
}

const OLD: &str = "OLDtoken-9876543210";

/// The provider and the API on one server: `/token` renews `ghr_old` to
/// GitHub's `ghu_live` and refuses any other refresh token; the API answers
/// 401 to anything but `Bearer ghu_live`, else how many body bytes it got.
fn renewing(head: &str, body: &[u8]) -> Vec<u8> {
    if path(head) == "/token" {
        return match field(&form(body), "refresh_token") {
            Some("ghr_old") => json(200, GITHUB_TOKEN),
            _ => json(400, r#"{"error":"invalid_grant"}"#),
        };
    }
    // The broker's own `check` call sends the claim's lowercase field name.
    if !head.to_ascii_lowercase().contains("\r\nauthorization: bearer ghu_live\r\n") {
        return UNAUTHORIZED.to_vec();
    }
    let reply = format!("got {} bytes", body.len());
    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{reply}", reply.len()).into_bytes()
}

fn no_tokens_in(log: &[Value]) {
    let text = serde_json::to_string(log).unwrap();
    assert!(!text.contains(OLD) && !text.contains("ghu_live") && !text.contains("ghr_"), "{text}");
}

const RELEASES: &str = "/api/0/organizations/acme/releases/";

#[test]
fn a_401_renews_and_replays_a_held_body() {
    let (port, seen) = upstream_server(renewing);
    let rig = Rig::new("replay", port);
    rig.oauth("ghr_old", Instance::now() + 86_400, port);
    let mut c = rig.connect("sentry.io");
    let a = c.call("POST", RELEASES, &bearer(OWN), br#"{"version":"1"}"#);
    assert_eq!((a.status, a.body.as_slice()), (200, &b"got 15 bytes"[..]), "{a:?}");
    {
        let seen = seen.lock().unwrap();
        let paths: Vec<&str> = seen.iter().map(|(h, _)| path(h)).collect();
        assert_eq!(paths, [RELEASES, "/token", RELEASES]);
        assert!(seen[0].0.contains(&format!("Bearer {OLD}\r\n")), "{}", seen[0].0);
        assert_eq!(seen[0].1, seen[2].1, "the same body, replayed");
    }
    assert_eq!(rig.stored(), "ghu_live");

    // The renewed token serves the next call as is.
    assert_eq!(c.call("GET", "/api/0/", &bearer(OWN), b"").status, 200);
    assert_eq!(seen.lock().unwrap().len(), 4);
    let log = rig.log(2);
    no_tokens_in(&log);
    assert_eq!((log[0]["refreshed"].as_bool(), log[0]["status"].as_u64()), (Some(true), Some(200)));
    assert!(log[1].get("refreshed").is_none(), "{}", log[1]);
}

#[test]
fn a_streamed_body_past_the_bound_isnt_replayed() {
    let (port, seen) = upstream_server(renewing);
    let rig = Rig::new("no-replay", port);
    rig.oauth("ghr_old", Instance::now() + 86_400, port);
    let body = vec![b'x'; MAX_REPLAY as usize + 1];
    let a = rig.connect("sentry.io").call("POST", RELEASES, &bearer(OWN), &body);
    assert_eq!(a.status, 403, "{a:?}");
    let v = a.json();
    assert!(v["detail"].as_str().unwrap().contains("streamed past 1048576 bytes"), "{v}");
    assert_eq!(v["devsandbox"]["do"], "devsandbox login sentry-work");
    let paths: Vec<String> = seen.lock().unwrap().iter().map(|(h, _)| path(h).to_string()).collect();
    assert_eq!(paths, [RELEASES, "/token", "/api/0/"], "renewed and checked, not replayed");
    assert_eq!(rig.stored(), "ghu_live");
    assert_eq!(rig.broken("sentry-work"), None, "the check passed with the renewed token");
    assert_eq!(rig.log(1)[0]["refreshed"], true);
}

#[test]
fn renewed_ahead_of_expiry() {
    let (port, seen) = upstream_server(renewing);
    let rig = Rig::new("ahead", port);
    rig.oauth("ghr_old", Instance::now() + 60, port);
    let a = rig.connect("sentry.io").call("GET", "/api/0/", &bearer(OWN), b"");
    assert_eq!(a.status, 200, "{a:?}");
    let paths: Vec<String> = seen.lock().unwrap().iter().map(|(h, _)| path(h).to_string()).collect();
    assert_eq!(paths, ["/token", "/api/0/"]);
    let log = rig.log(1);
    no_tokens_in(&log);
    assert_eq!(log[0]["refreshed"], true);
}

#[test]
fn a_refused_renewal_is_a_login_refusal_and_an_unreachable_one_a_503() {
    let (port, _) = upstream_server(renewing);
    let rig = Rig::new("renew-fail", port);
    let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    rig.oauth("ghr_old", Instance::now(), closed);
    let a = rig.connect("sentry.io").call("GET", "/api/0/", &bearer(OWN), b"");
    assert_eq!((a.status, a.header("retry-after")), (503, Some("30")), "{a:?}");
    assert!(a.json()["detail"].as_str().unwrap().contains("cannot renew credential `sentry-work`'s token"));
    assert_eq!(rig.broken("sentry-work"), None);

    rig.oauth("ghr_dead", Instance::now(), port);
    let a = rig.connect("sentry.io").call("GET", "/api/0/", &bearer(OWN), b"");
    assert_eq!(a.status, 403, "{a:?}");
    let v = a.json();
    assert!(v["detail"].as_str().unwrap().contains("was refused: invalid_grant"), "{v}");
    assert_eq!(v["devsandbox"]["do"], "devsandbox login sentry-work");
    assert_eq!(rig.broken("sentry-work").unwrap().reason, "its refresh was refused: invalid_grant");
    assert_eq!(rig.stored(), OLD);
}

#[test]
fn uninjected_calls() {
    let (port, seen) = upstream_server(echo);
    let rig = Rig::new("uninjected", port);
    // A private upstream without the phantom: a bare 401, never dialed.
    let a = rig.connect("sentry.io").call("GET", "/api/0/", "", b"");
    assert_eq!((a.status, a.body.len()), (401, 0));
    assert!(a.header("www-authenticate").is_none());
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(rig.log(1)[0]["verdict"], "uninjected");
}

/// [`echo`] with a `Location` to the real host.
fn echo_located(head: &str, body: &[u8]) -> Vec<u8> {
    let reply = String::from_utf8(echo(head, body)).unwrap();
    reply.replacen("\r\n", "\r\nLocation: https://api.github.com/next\r\n", 1).into_bytes()
}

/// The member CA covering `api.github.com` before the broker starts, as
/// `run` will have made it.
fn covered(rig: &Rig) {
    let mut ca = MemberCa::load_or_create(&rig.dir.join("data")).unwrap();
    ca.ensure_hosts(&BTreeSet::from(["api.github.com".to_string()])).unwrap();
}

#[test]
fn intercept_terminates_tls_injects_and_keeps_alive() {
    let (port, seen) = upstream_server(echo_located);
    let rig = Rig::intercept("intercept", port);
    covered(&rig);
    // h2 is offered too: the broker settles on HTTP/1.1.
    let mut c = rig.intercept_to("API.GitHub.com.", 443, Some("api.github.com"), &[b"h2", b"http/1.1"]).unwrap();
    c.host = "api.github.com".into();
    for _ in 0..2 {
        let a = c.call("GET", "/user", &format!("Authorization: token {GH}\r\n"), b"");
        assert_eq!(a.status, 200, "{a:?}");
        let masked = format!("token {}", "*".repeat(TOKEN.len()));
        assert_eq!(a.header("x-echo"), Some(masked.as_str()), "{}", a.head);
        // Passed as is: the tool already talks to the real host.
        assert_eq!(a.header("location"), Some("https://api.github.com/next"));
    }
    // `Host` must be the `CONNECT` target: refused over the TLS.
    c.host = "evil.example".into();
    let a = c.call("GET", "/user", &format!("Authorization: token {GH}\r\n"), b"");
    assert_eq!(a.status, 403, "{a:?}");
    assert!(a.json()["message"].as_str().unwrap().contains("Host"), "{a:?}");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (head, _) in seen.iter() {
        assert!(head.contains(&format!("\r\nAuthorization: token {TOKEN}\r\n")), "{head}");
        assert!(head.starts_with("GET /user HTTP/1.1\r\n"), "{head}");
    }
    let log = rig.log(3);
    let verdicts: Vec<&str> = log.iter().map(|l| l["verdict"].as_str().unwrap()).collect();
    assert_eq!(verdicts, ["allow", "allow", "refused"]);
    assert_eq!((log[0]["credential"].as_str(), log[0]["scope"].as_str()), (Some("github-work"), Some("user.read")));
}

#[test]
fn intercept_refusals_close_the_stream() {
    let (port, seen) = upstream_server(echo);
    let rig = Rig::intercept("intercept-refuse", port);
    covered(&rig);
    // Not intercepted: closed before any TLS.
    for (host, p) in [("sentry.io", 443), ("api.github.com", 8443)] {
        let err = rig.intercept_to(host, p, Some(host), &[b"http/1.1"]).err().expect("closed");
        assert!(!err.is_empty());
    }
    // SNI absent or another host, ALPN h2 only: the handshake fails.
    assert!(rig.intercept_to("api.github.com", 443, None, &[]).is_err());
    assert!(rig.intercept_to("api.github.com", 443, Some("github.com"), &[]).is_err());
    let err = rig.intercept_to("api.github.com", 443, Some("api.github.com"), &[b"h2"]).err().expect("no h2");
    assert!(err.contains("NoApplicationProtocol"), "{err}");
    // No ALPN at all is plain HTTP/1.1.
    let mut c = rig.intercept_to("api.github.com", 443, Some("api.github.com"), &[]).unwrap();
    assert_eq!(c.call("GET", "/user", &format!("Authorization: token {GH}\r\n"), b"").status, 200);
    assert_eq!(seen.lock().unwrap().len(), 1);

    // A failed handshake's line lands after its alert went out: unordered.
    let log = rig.log(6);
    let lines: Vec<String> = log.iter().map(|l| format!("{} {}", l["verdict"], l["reason"])).collect();
    let count = |s: &str| lines.iter().filter(|l| l.contains(s)).count();
    assert_eq!(count("\"refused\" \"`sentry.io:443` is no intercepted upstream of instance `web-1`\""), 1, "{lines:?}");
    assert_eq!(count("`api.github.com:8443` is no intercepted upstream"), 1, "{lines:?}");
    assert_eq!(count("TLS handshake for `api.github.com`"), 3, "{lines:?}");
    assert_eq!(count("\"allow\""), 1, "{lines:?}");
}

#[test]
fn an_uncovered_host_is_covered_on_demand() {
    let (port, _) = upstream_server(echo);
    let rig = Rig::intercept("intercept-ensure", port);
    covered(&rig);
    rig.service.leaf("github.com").unwrap();
    let hosts = MemberCa::load_or_create(&rig.dir.join("data")).unwrap().hosts();
    assert_eq!(hosts, BTreeSet::from(["api.github.com".to_string(), "github.com".to_string()]));
    let mut c = rig.intercept_to("github.com", 443, Some("github.com"), &[b"http/1.1"]).unwrap();
    assert_eq!(c.call("GET", "/user", &format!("Authorization: token {GH}\r\n"), b"").status, 200);
}

/// A git client on `github.com` through the broker.
fn git_client(rig: &Rig) -> Client {
    let mut ca = MemberCa::load_or_create(&rig.dir.join("data")).unwrap();
    ca.ensure_hosts(&BTreeSet::from(["github.com".to_string()])).unwrap();
    let mut c = rig.intercept_to("github.com", 443, Some("github.com"), &[b"http/1.1"]).unwrap();
    c.host = "github.com".into();
    c
}

/// What reached the upstream from the tool: the fence's own lookups
/// (`broker::github`) left out.
fn tool_calls(seen: &Seen) -> Vec<(String, Vec<u8>)> {
    seen.lock().unwrap().iter().filter(|(h, _)| !h.contains("\r\nUser-Agent: devsandbox-broker\r\n")).cloned().collect()
}

fn git_headers() -> String {
    format!("{}Git-Protocol: version=2\r\n", git_basic(GH))
}

#[test]
fn git_fetch_in_the_fence_injects_into_the_basic_slot() {
    let (port, seen) = upstream_server(echo);
    let rig = Rig::fenced("git-in", port);
    let a = git_client(&rig).call("GET", "/o/r.git/info/refs?service=git-upload-pack", &git_headers(), b"");
    assert_eq!(a.status, 200, "{a:?}");
    assert_eq!(a.header("devsandbox-fence"), None);
    let seen = tool_calls(&seen);
    let head = &seen[0].0;
    let real = String::from_utf8(crate::broker::redact::basic_auth(b"x-access-token", TOKEN.as_bytes())).unwrap();
    assert!(head.contains(&format!("\r\nAuthorization: Basic {real}\r\n")), "{head}");
    assert!(head.contains("\r\nGit-Protocol: version=2\r\n"), "{head}");
    assert!(head.starts_with("GET /o/r.git/info/refs?service=git-upload-pack HTTP/1.1\r\n"), "{head}");
    assert_eq!(rig.log(1)[0]["scope"], "repo.read");
}

#[test]
fn a_pasted_tokens_reported_expiry_is_recorded_and_noticed_once_a_day() {
    let end = Instance::now() + 3 * 86_400;
    let reported = crate::inbox::rfc3339(end).replace('T', " ").replace('Z', " UTC");
    let (port, _) = upstream_server(move |_, _| {
        format!("HTTP/1.1 200 OK\r\nGitHub-Authentication-Token-Expiration: {reported}\r\nContent-Length: 0\r\n\r\n")
            .into_bytes()
    });
    let claim = private(crate::credentials::home::tests::github());
    let mut state = intercept_state(claim.clone());
    state.instances.get_mut("web-1").unwrap().github_repos = vec!["o/r".into()];
    let shown = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = shown.clone();
    let rig = Rig::with_service("expiry", port, state, intercept_home(&claim), "github-work", &claim, |s| {
        s.with_notify(move |body| sink.lock().unwrap().push(body.to_string()))
    });
    let mut c = git_client(&rig);
    for _ in 0..2 {
        let a = c.call("GET", "/o/r.git/info/refs?service=git-upload-pack", &git_headers(), b"");
        assert_eq!(a.status, 200, "{a:?}");
    }
    let entry = home::load(&home::path(&rig.dir.join("data"))).unwrap()["github-work"].clone();
    assert_eq!(entry.expires(), Some(end));
    let shown = shown.lock().unwrap();
    assert_eq!(shown.len(), 1, "{shown:?}");
    let rfc = crate::inbox::rfc3339(end);
    assert!(shown[0].starts_with("credential `github-work` expires in ") && shown[0].contains(&rfc), "{}", shown[0]);
}

#[test]
fn git_without_the_phantom_on_a_private_upstream_is_challenged() {
    let (port, seen) = upstream_server(echo);
    let rig = Rig::fenced("git-challenge", port);
    let a = git_client(&rig).call("GET", "/o/r.git/info/refs?service=git-upload-pack", "Git-Protocol: version=2\r\n", b"");
    assert_eq!((a.status, a.header("www-authenticate")), (401, Some("Basic realm=\"devsandbox\"")), "{a:?}");
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(rig.log(1)[0]["verdict"], "uninjected");
}

#[test]
fn git_fetch_outside_the_fence_goes_stripped_and_marked() {
    let (port, seen) = upstream_server(echo);
    let rig = Rig::fenced("git-out", port);
    let a = git_client(&rig).call("GET", "/x/y.git/info/refs?service=git-upload-pack", &git_headers(), b"");
    assert_eq!(a.status, 200, "{a:?}");
    assert_eq!(a.header("devsandbox-fence"), Some("outside"), "{}", a.head);
    let seen = tool_calls(&seen);
    let head = &seen[0].0;
    assert!(!head.to_ascii_lowercase().contains("authorization"), "{head}");
    assert!(head.contains("\r\nGit-Protocol: version=2\r\n"), "{head}");
    let log = rig.log(1);
    assert_eq!((log[0]["verdict"].as_str(), log[0]["status"].as_u64()), (Some("outside"), Some(200)));
}

#[test]
fn an_outside_401_is_the_brokers_403() {
    let (port, _) = upstream_server(|_, _| UNAUTHORIZED.to_vec());
    let rig = Rig::fenced("git-out-401", port);
    let a = git_client(&rig).call("GET", "/x/y.git/info/refs?service=git-upload-pack", &git_headers(), b"");
    assert_eq!(a.status, 403, "{a:?}");
    assert!(!a.head.to_ascii_lowercase().contains("www-authenticate"), "{}", a.head);
    assert_eq!(a.header("content-type"), Some("text/plain; charset=utf-8"));
    let text = String::from_utf8(a.body).unwrap();
    assert!(text.contains("`x/y` is outside the instance's repo fence"), "{text}");
    assert!(text.contains("add `x/y` to `access.github.repos`"), "{text}");
    assert_eq!(rig.broken("github-work"), None, "no credential was sent: nothing to mark");
}

#[test]
fn pushes_are_refused_at_info_refs_before_upstream() {
    let (port, seen) = upstream_server(echo);
    let rig = Rig::fenced("git-push", port);
    let mut c = git_client(&rig);
    let a = c.call("GET", "/x/y.git/info/refs?service=git-receive-pack", &git_headers(), b"");
    assert_eq!(a.status, 403, "{a:?}");
    let text = String::from_utf8(a.body).unwrap();
    assert!(text.starts_with("devsandbox: `x/y` is outside the instance's repo fence"), "{text}");

    rig.config("[sandbox.web]\nfolder = \".\"\n[sandbox.web.access.github]\nscopes = [\"repo.read\"]\n");
    let a = c.call("GET", "/o/r.git/info/refs?service=git-receive-pack", &git_headers(), b"");
    assert_eq!(a.status, 403, "{a:?}");
    assert!(String::from_utf8(a.body).unwrap().contains("scope `git.push` isn't in `access.github.scopes`"));
    assert_eq!(c.call("GET", "/o/r.git/info/refs?service=git-upload-pack", &git_headers(), b"").status, 200);
    assert_eq!(tool_calls(&seen).len(), 1, "only the fetch went upstream");
}

/// GitHub after `o/r` became `N/R` (id 42): a 301 for the old name.
fn renamed(head: &str, body: &[u8]) -> Vec<u8> {
    let repo = r#"{"id":42,"node_id":"R_42","full_name":"N/R"}"#;
    match head.split(' ').nth(1).unwrap() {
        "/repos/o/r" => b"HTTP/1.1 301 Moved\r\nLocation: https://api.github.com/repositories/42\r\nContent-Length: 0\r\n\r\n".to_vec(),
        "/repositories/42" if head.contains("User-Agent: devsandbox-broker") => {
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{repo}", repo.len()).into_bytes()
        }
        _ => echo(head, body),
    }
}

#[test]
fn a_renamed_repo_is_pinned_and_called_by_its_current_name() {
    let (port, seen) = upstream_server(renamed);
    let rig = Rig::fenced("renamed", port);
    let mut ca = MemberCa::load_or_create(&rig.dir.join("data")).unwrap();
    ca.ensure_hosts(&BTreeSet::from(["api.github.com".to_string()])).unwrap();
    let mut c = rig.intercept_to("api.github.com", 443, Some("api.github.com"), &[b"http/1.1"]).unwrap();
    let auth = format!("Authorization: token {GH}\r\n");
    assert_eq!(c.call("GET", "/repos/o/r/pulls", &auth, b"").status, 200);
    assert_eq!(c.call("GET", "/repositories/42", &auth, b"").status, 200);
    let tool = tool_calls(&seen);
    assert!(tool[0].0.starts_with("GET /repos/N/R/pulls HTTP/1.1\r\n"), "{}", tool[0].0);
    assert!(tool[0].0.contains(&format!("\r\nAuthorization: token {TOKEN}\r\n")), "{}", tool[0].0);
    assert!(tool[1].0.starts_with("GET /repositories/42 HTTP/1.1\r\n"), "{}", tool[1].0);
    assert_eq!(seen.lock().unwrap().len(), 4, "the old name and the id looked up once each");
    let state = State::load_at(&rig.dir.join("data/state.toml")).unwrap();
    assert_eq!((state.github_ids["o/r"].id, state.github_ids["o/r"].node_id.as_str()), (42, "R_42"));
}

/// GitHub for the fence's own lookups (`o/r` is id 42, node `R_7` is
/// `x/y`); the tool's calls echoed.
fn github_api(head: &str, body: &[u8]) -> Vec<u8> {
    let answer = |s: &str| format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{s}", s.len()).into_bytes();
    let broker = head.contains("User-Agent: devsandbox-broker");
    match head.split(' ').nth(1).unwrap() {
        "/repos/o/r" if broker => answer(r#"{"id":42,"node_id":"R_42","full_name":"o/r"}"#),
        "/graphql" if broker => {
            answer(r#"{"data":{"node":{"__typename":"Repository","databaseId":7,"id":"R_7","nameWithOwner":"x/y"}}}"#)
        }
        _ => echo(head, body),
    }
}

#[test]
fn graphql_goes_upstream_byte_identical_once_decided() {
    let (port, seen) = upstream_server(github_api);
    let rig = Rig::fenced("graphql", port);
    let mut ca = MemberCa::load_or_create(&rig.dir.join("data")).unwrap();
    ca.ensure_hosts(&BTreeSet::from(["api.github.com".to_string()])).unwrap();
    let mut c = rig.intercept_to("api.github.com", 443, Some("api.github.com"), &[b"http/1.1"]).unwrap();
    let auth = format!("Authorization: token {GH}\r\n");

    let viewer = br#"{"query":"query { viewer { login } }"}"#;
    assert_eq!(c.call("POST", "/graphql", &auth, viewer).status, 200);
    let pr = br#"{ "query": "mutation($input: CreatePullRequestInput!) { createPullRequest(input: $input) { pullRequest { url } } }",
        "variables": { "input": { "repositoryId": "R_42", "baseRefName": "main", "headRefName": "x", "title": "t" } } }"#;
    let a = c.call("POST", "/graphql", &auth, pr);
    assert_eq!(a.status, 200, "{a:?}");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(viewer).unwrap();
    let gz = gz.finish().unwrap();
    assert_eq!(c.call("POST", "/graphql", &format!("{auth}Content-Encoding: gzip\r\n"), &gz).status, 200);

    let tool = tool_calls(&seen);
    assert_eq!(tool.len(), 3, "{tool:?}");
    for ((head, body), sent) in tool.iter().zip([&viewer[..], &pr[..], &gz[..]]) {
        assert_eq!(body, sent, "{head}");
        assert!(head.starts_with("POST /graphql HTTP/1.1\r\n") && head.contains(&format!("\r\nAuthorization: token {TOKEN}\r\n")), "{head}");
    }
    assert!(tool[2].0.contains("\r\nContent-Encoding: gzip\r\n"), "{}", tool[2].0);

    // Outside the fence, a batch, a body that doesn't decode: refused before
    // the tool's call goes anywhere. A refusal with a body closes.
    let refused = |extra: &str, body: &[u8]| {
        let mut c = rig.intercept_to("api.github.com", 443, Some("api.github.com"), &[b"http/1.1"]).unwrap();
        c.call("POST", "/graphql", &format!("{auth}{extra}"), body)
    };
    let out = br#"{"query":"mutation { createPullRequest(input: {repositoryId: \"R_42\", headRepositoryId: \"R_7\"}) { clientMutationId } }"}"#;
    let a = refused("", out);
    assert_eq!(a.status, 403, "{a:?}");
    assert!(a.json()["message"].as_str().unwrap().contains("names `x/y`, outside the instance's repo fence"), "{:?}", a.json());
    let a = refused("", br#"[{"query":"{ a }"}]"#);
    assert_eq!(a.status, 400, "{a:?}");
    assert!(a.json()["message"].as_str().unwrap().contains("a batch of GraphQL requests"));
    let a = refused("Content-Encoding: gzip\r\n", viewer);
    assert_eq!(a.status, 400, "{a:?}");
    assert_eq!(tool_calls(&seen).len(), 3);

    let log = rig.log(6);
    let scopes: Vec<_> = log.iter().map(|l| (l["verdict"].as_str().unwrap(), l["scope"].as_str())).collect();
    assert_eq!(
        scopes,
        [
            ("allow", Some("api.read")),
            ("allow", Some("pr.write")),
            ("allow", Some("api.read")),
            ("refused", Some("pr.write")),
            ("refused", None),
            ("refused", None)
        ]
    );
}

#[test]
fn a_graphql_body_past_the_bound_is_refused() {
    let (port, seen) = upstream_server(github_api);
    let rig = Rig::fenced("graphql-big", port);
    let mut ca = MemberCa::load_or_create(&rig.dir.join("data")).unwrap();
    ca.ensure_hosts(&BTreeSet::from(["api.github.com".to_string()])).unwrap();
    let mut c = rig.intercept_to("api.github.com", 443, Some("api.github.com"), &[b"http/1.1"]).unwrap();
    let head = format!(
        "POST /graphql HTTP/1.1\r\nHost: api.github.com\r\nAuthorization: token {GH}\r\nContent-Length: {}\r\n\r\n",
        graphql::MAX_BODY + 1
    );
    c.write(head.as_bytes());
    let a = c.read();
    assert_eq!(a.status, 413, "{a:?}");
    assert!(c.closed());
    assert!(seen.lock().unwrap().is_empty());
}

const STORAGE_AUTH: &str = "RemoteAuth STORAGE-secret-42";
const SIG: &str = "X-Amz-Signature=SIG0123456789";
const BATCH: &str = "/o/r.git/info/lfs/objects/batch";
const DOWNLOAD: &[u8] = br#"{"operation":"download","transfers":["basic"],"objects":[{"oid":"a","size":5}]}"#;
const UPLOAD: &[u8] = br#"{"operation":"upload","transfers":["basic"],"objects":[{"oid":"a","size":5}]}"#;

/// github.com's LFS batch API: one object whose actions live on
/// `storage.test`, each with a storage credential in its header (the
/// download's also echoes the batch's `Authorization`, as some hosts do).
fn lfs_api(head: &str, body: &[u8]) -> Vec<u8> {
    let auth = head.lines().find_map(|l| l.strip_prefix("Authorization: ")).unwrap_or("").to_string();
    let actions = match route::lfs_operation(body).unwrap() {
        route::LfsOp::Download => serde_json::json!({ "download": {
            "href": format!("https://storage.test/obj/a?{SIG}"),
            "header": { "Authorization": STORAGE_AUTH, "X-Batch-Auth": auth },
            "expires_in": 600,
        } }),
        route::LfsOp::Upload => serde_json::json!({
            "upload": { "href": "https://storage.test/up/a", "header": { "Authorization": STORAGE_AUTH, "Content-Type": "application/octet-stream" } },
            "verify": { "href": "https://storage.test/verify", "header": { "Authorization": STORAGE_AUTH } },
        }),
    };
    let body = serde_json::json!({ "transfer": "basic", "objects": [{ "oid": "a", "size": 5, "actions": actions }] }).to_string();
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/vnd.git-lfs+json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        .into_bytes()
}

/// A storage host repeating what it got, credentials included.
fn storage_echo(head: &str, body: &[u8]) -> Vec<u8> {
    let first = head.lines().next().unwrap_or("").to_string();
    let auth = head.lines().find_map(|l| l.strip_prefix("Authorization: ")).unwrap_or("").to_string();
    let reply = format!("{first} with {auth}: {} bytes", body.len());
    format!("HTTP/1.1 200 OK\r\nX-Seen-Auth: {auth}\r\nContent-Length: {}\r\n\r\n{reply}", reply.len()).into_bytes()
}

fn lfs_headers() -> String {
    format!("{}Content-Type: application/vnd.git-lfs+json\r\n", git_basic(GH))
}

/// The action's href as a path on the LFS listener.
fn handle_path(action: &Value) -> String {
    assert!(action.get("header").is_none(), "{action}");
    let href = action["href"].as_str().unwrap();
    href.strip_prefix(&format!("http://127.0.0.1:{LFS_PORT}")).unwrap_or_else(|| panic!("{href}")).to_string()
}

fn hides_storage(text: &str) {
    for hidden in ["STORAGE-secret", "SIG0123456789", "storage.test", TOKEN, GH] {
        assert!(!text.contains(hidden), "{hidden} in {text}");
    }
}

#[test]
fn lfs_transfers_go_through_handles_with_the_stored_header() {
    let (api, api_seen) = upstream_server(lfs_api);
    let (storage, storage_seen) = upstream_server(storage_echo);
    let rig = Rig::lfs("lfs", api, storage);

    let a = git_client(&rig).call("POST", BATCH, &lfs_headers(), DOWNLOAD);
    assert_eq!(a.status, 200, "{a:?}");
    hides_storage(&String::from_utf8_lossy(&a.body));
    let action = &a.json()["objects"][0]["actions"]["download"];
    assert_eq!(action["expires_in"], 600);
    let download = handle_path(action);
    let real = String::from_utf8(crate::broker::redact::basic_auth(b"x-access-token", TOKEN.as_bytes())).unwrap();
    {
        let seen = api_seen.lock().unwrap();
        assert!(seen[0].0.contains(&format!("\r\nAuthorization: Basic {real}\r\n")), "{}", seen[0].0);
        assert_eq!(seen[0].1, DOWNLOAD, "the read-ahead body went upstream as sent");
    }

    // The client's own credential stays behind; the stored header goes out,
    // masked wherever the storage host repeats it.
    let mut t = rig.lfs_listener(caller());
    for _ in 0..2 {
        let got = t.call("GET", &download, &git_basic(GH), b"");
        assert_eq!(got.status, 200, "{got:?}");
        hides_storage(&format!("{}{}", got.head, String::from_utf8_lossy(&got.body)));
    }
    {
        let seen = storage_seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one connection per transfer, the client's kept alive");
        let head = &seen[0].0;
        assert!(head.starts_with(&format!("GET /obj/a?{SIG} HTTP/1.1\r\n")), "{head}");
        assert!(head.contains(&format!("\r\nAuthorization: {STORAGE_AUTH}\r\n")), "{head}");
        assert!(head.contains(&format!("\r\nX-Batch-Auth: Basic {real}\r\n")), "an echoed header is used, not shown: {head}");
        assert!(!head.contains(GH) && head.matches("Authorization:").count() == 1, "{head}");
    }
    let a = rig.lfs_listener(caller()).call("PUT", &download, "", b"bytes");
    assert_eq!(a.status, 403, "{a:?}");
    assert!(a.json()["message"].as_str().unwrap().contains("`PUT` to an LFS download transfer, which takes `GET`"));

    let a = git_client(&rig).call("POST", BATCH, &lfs_headers(), UPLOAD);
    assert_eq!(a.status, 200, "{a:?}");
    hides_storage(&String::from_utf8_lossy(&a.body));
    let actions = &a.json()["objects"][0]["actions"];
    let (upload, verify) = (handle_path(&actions["upload"]), handle_path(&actions["verify"]));
    let got = rig.lfs_listener(caller()).call("PUT", &upload, "Content-Type: text/plain\r\n", b"hello");
    assert_eq!((got.status, String::from_utf8_lossy(&got.body).ends_with(": 5 bytes")), (200, true), "{got:?}");
    assert_eq!(rig.lfs_listener(caller()).call("POST", &verify, "", br#"{"oid":"a","size":5}"#).status, 200);
    {
        let seen = storage_seen.lock().unwrap();
        let put = seen.iter().find(|(h, _)| h.starts_with("PUT /up/a ")).unwrap();
        assert_eq!(put.1, b"hello");
        assert!(put.0.contains("\r\nContent-Type: application/octet-stream\r\n") && !put.0.contains("text/plain"), "{}", put.0);
        assert!(seen.iter().any(|(h, _)| h.starts_with("POST /verify ")));
    }

    // Unknown, another instance's, or on the wrong listener: refused, never dialed.
    let before = storage_seen.lock().unwrap().len();
    let a = rig.lfs_listener(caller()).call("GET", "/0123456789abcdefABCDEF0123456789", "", b"");
    assert_eq!(a.status, 404, "{a:?}");
    assert_eq!(rig.lfs_listener(caller()).call("GET", &format!("{download}?x=1"), "", b"").status, 404);
    let other = Caller { instance: "other".into(), instance_id: "other-id".into() };
    let a = rig.lfs_listener(other).call("GET", &download, "", b"");
    assert_eq!((a.status, a.json()["devsandbox"]["verdict"].as_str()), (403, Some("leak")), "{a:?}");
    let mut wrong = rig.lfs_listener(caller());
    wrong.host = "127.0.0.1:1".into();
    assert_eq!(wrong.call("GET", &download, "", b"").status, 403);
    assert_eq!(storage_seen.lock().unwrap().len(), before);

    let log = rig.log(12);
    let text = serde_json::to_string(&log).unwrap();
    hides_storage(&text);
    assert!(!text.contains(download.trim_start_matches('/')), "no handle in the log: {text}");
    let routes = |route: &str, verdict: &str| log.iter().filter(|l| l["route"] == route && l["verdict"] == verdict).count();
    assert_eq!(routes("lfs download", "allow"), 2, "{text}");
    assert_eq!(routes("lfs download", "refused"), 1, "{text}");
    assert_eq!((routes("lfs upload", "allow"), routes("lfs verify", "allow")), (1, 1), "{text}");
    assert_eq!((routes("lfs", "refused"), routes("lfs", "leak")), (3, 1), "{text}");
    let l = log.iter().find(|l| l["route"] == "lfs upload").unwrap();
    assert_eq!((l["credential"].as_str(), l["kind"].as_str(), l["incarnation"].as_str()), (Some("github-work"), Some("github"), Some("inc-web-1")));
    let batches: Vec<&Value> = log.iter().filter(|l| l["route"] == "/o/r.git/info/lfs/objects/batch").collect();
    let scopes: Vec<&str> = batches.iter().map(|l| l["scope"].as_str().unwrap()).collect();
    assert_eq!(scopes, ["repo.read", "git.push"]);
}

#[test]
fn lfs_batches_outside_the_fence_or_malformed() {
    let (api, seen) = upstream_server(lfs_api);
    let rig = Rig::lfs("lfs-out", api, 1);
    let outside = "/x/y.git/info/lfs/objects/batch";
    let a = git_client(&rig).call("POST", outside, &lfs_headers(), DOWNLOAD);
    assert_eq!((a.status, a.header("devsandbox-fence")), (200, Some("outside")), "{a:?}");
    // Sent with no credential: nothing to hide, passed as is.
    assert!(a.json()["objects"][0]["actions"]["download"]["href"].as_str().unwrap().starts_with("https://storage.test/"));
    assert!(!seen.lock().unwrap()[0].0.to_ascii_lowercase().contains("authorization"));

    let a = git_client(&rig).call("POST", outside, &lfs_headers(), UPLOAD);
    assert_eq!(a.status, 403, "{a:?}");
    assert!(a.json()["message"].as_str().unwrap().contains("`x/y` is outside the instance's repo fence"));
    for (body, status) in [(&br#"{"operation":"verify"}"#[..], 400), (b"{}", 400), (b"", 400)] {
        let a = git_client(&rig).call("POST", BATCH, &lfs_headers(), body);
        assert_eq!(a.status, status, "{a:?}");
    }
    let mut c = git_client(&rig);
    c.send("POST", BATCH, &format!("{}Content-Length: {}\r\n", lfs_headers(), route::MAX_LFS_BATCH + 1), b"");
    assert_eq!(c.read().status, 413);
    assert_eq!(seen.lock().unwrap().len(), 1, "only the outside download went upstream");
}

#[test]
fn an_lfs_batch_past_the_bound_or_not_json_passes() {
    let store = Mutex::new(lfs::Store::new(10));
    let owner = lfs::Owner { instance: "web-1".into(), instance_id: "web-1-id".into(), incarnation: None };
    let body: &'static [u8] =
        br#"{"objects":[{"actions":{"download":{"href":"https://s.example/a","header":{"Authorization":"x"}}}}]}"#;
    let write = |body: &'static [u8], max: usize, line: &mut Line| {
        let lfs = LfsRewrite { store: &store, owner: owner.clone(), port: LFS_PORT, kind: None, credential: None };
        let respond = Respond { lfs: Some(lfs), ..respond(&[], None, max, false) };
        let mut out = Vec::new();
        respond.write(response(Length::Exact(body.len() as u64), &[], body), &mut out, line).unwrap();
        String::from_utf8(out).unwrap()
    };
    let mut line = Line::default();
    let text = write(body, 1024, &mut line);
    assert!(text.contains(&format!("http://127.0.0.1:{LFS_PORT}/")) && !text.contains("Authorization"), "{text}");
    assert_eq!(line.reason, None);
    let text = write(body, 8, &mut line);
    assert!(text.ends_with(std::str::from_utf8(body).unwrap()), "{text}");
    assert!(line.reason.as_deref().unwrap().contains("not rewritten"));
    let mut line = Line::default();
    assert!(write(b"<html>", 1024, &mut line).ends_with("\r\n\r\n<html>"));
    assert_eq!(store.lock().unwrap().len(), 1);
}

fn response(length: Length, headers: &[(&str, &str)], body: &'static [u8]) -> Response {
    Response {
        status: 200,
        reason: "OK".into(),
        headers: headers.iter().map(|(n, v)| Header { name: n.to_string(), value: v.as_bytes().to_vec() }).collect(),
        length,
        decoded: None,
        body: Box::new(body),
    }
}

fn respond<'a>(listeners: &'a [(String, u16)], fields: Option<&'a [String]>, max: usize, http10: bool) -> Respond<'a> {
    Respond { http10, keep: true, outside: false, secrets: None, listeners, fields, lfs: None, max_rewrite: max }
}

#[test]
fn rewrite_past_its_bound_passes_through() {
    let listeners = vec![("us.sentry.io".to_string(), 29002)];
    let fields = vec!["url".to_string()];
    let body: &'static [u8] = br#"{"url":"https://us.sentry.io/x"}"#;
    let len = body.len().to_string();
    let headers = [("Content-Length", len.as_str()), ("Connection", "x-hop"), ("X-Hop", "1"), ("Keep-Alive", "t")];

    let mut out = Vec::new();
    let mut line = Line::default();
    let keep = respond(&listeners, Some(&fields), 1024, false)
        .write(response(Length::Exact(body.len() as u64), &headers, body), &mut out, &mut line)
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(keep);
    assert!(text.ends_with("\r\n\r\n{\"url\":\"http://127.0.0.1:29002/x\"}"), "{text}");
    assert!(text.contains("Content-Length: 34\r\n"), "{text}");
    assert!(!text.contains("X-Hop") && !text.contains("Keep-Alive") && !text.contains("Connection"), "{text}");
    assert_eq!(line.reason, None);

    let mut out = Vec::new();
    respond(&listeners, Some(&fields), 8, false)
        .write(response(Length::Exact(body.len() as u64), &headers, body), &mut out, &mut line)
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.ends_with(&format!("Content-Length: {len}\r\n\r\n{}", String::from_utf8_lossy(body))), "{text}");
    assert!(line.reason.as_deref().unwrap().contains("not rewritten"));

    // Unframed past the bound: chunked, the read prefix first.
    let mut out = Vec::new();
    respond(&listeners, Some(&fields), 8, false).write(response(Length::Unframed, &[], body), &mut out, &mut line).unwrap();
    let mut r = BufReader::new(&out[..]);
    let raw = head::read_raw_head(&mut r).unwrap().unwrap();
    assert!(String::from_utf8(raw).unwrap().contains("Transfer-Encoding: chunked\r\n"));
    let mut got = Vec::new();
    BodyReader::new(r, Framing::Chunked).read_to_end(&mut got).unwrap();
    assert_eq!(got, body);
}

#[test]
fn unframed_to_http10_closes() {
    let mut out = Vec::new();
    let keep = respond(&[], None, 0, true).write(response(Length::Unframed, &[], b"to the end"), &mut out, &mut Line::default());
    let text = String::from_utf8(out).unwrap();
    assert!(!keep.unwrap());
    assert!(text.contains("Connection: close\r\n\r\nto the end"), "{text}");
}
