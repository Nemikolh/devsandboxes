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
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};
use serde_json::Value;

use super::*;
use crate::broker::policy::tests::{FOREIGN, OWN, caller, home_with, sentry, state_with};
use crate::broker::upstream::Timeouts;
use crate::credentials::store::Secret;

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
        let dir = std::env::temp_dir().join(format!("dsv-broker-serve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = dir.join("data");
        let cfg = dir.join("cfg");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("devsandboxes.toml"), "[sandbox.web]\nfolder = \".\"\n").unwrap();

        let mut claim = sentry();
        for u in &mut claim.pinned.upstreams {
            u.private = true;
        }
        let mut state = state_with(claim.clone());
        for inst in state.instances.values_mut() {
            inst.config_dir = Some(cfg.clone());
        }
        std::fs::write(data.join("state.toml"), toml::to_string(&state).unwrap()).unwrap();
        std::fs::write(home::path(&data), home::serialize(&home_with(&claim)).unwrap()).unwrap();
        Store::in_data(&data).put("sentry-work", &Secret::new(TOKEN.into())).unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from_pem_file(fixture("ca.pem")).unwrap()).unwrap();
        let timeouts = Timeouts { connect: Duration::from_secs(5), idle: Duration::from_secs(10) };
        let upstream = Upstream::new(roots, None, timeouts).unwrap();
        let mut service = Service::new(data.join("state.toml"), Ok(upstream)).unwrap();
        for u in &claim.pinned.upstreams {
            service = service.with_authority(&u.host, &format!("localhost:{upstream_port}"));
        }
        Rig { dir, service: Arc::new(service), state }
    }

    fn port(&self, upstream: &str) -> u16 {
        self.state.instances["web-1"].claims[0].listeners.iter().find(|l| l.upstream == upstream).unwrap().port
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
        Client { port: self.port(upstream), reader: BufReader::new(ours.try_clone().unwrap()), w: ours }
    }

    fn connect(&self, upstream: &str) -> Client {
        self.connect_as(caller(), upstream)
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
        assert!(!text.contains(TOKEN) && !text.contains(OWN) && !text.contains(FOREIGN), "{text}");
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Client {
    port: u16,
    reader: BufReader<UnixStream>,
    w: UnixStream,
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
    /// `extra` header lines, CRLF-terminated; `Host` is the listener's.
    fn send(&mut self, method: &str, path: &str, extra: &str, body: &[u8]) {
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{extra}", self.port);
        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        self.w.write_all(req.as_bytes()).unwrap();
        self.w.write_all(body).unwrap();
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
    up.w.write_all(req.as_bytes()).unwrap();
    assert_eq!(up.read().status, 100);
    up.w.write_all(body).unwrap();
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
    assert!(v["detail"].as_str().unwrap().contains("asks need permission requests"), "{v}");
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
    c.w.write_all(b"GET /api/0/ HTTP/1.1\r\nHost: evil.example\r\n\r\n").unwrap();
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

#[test]
fn upstream_401_on_an_injected_call_is_a_login_refusal() {
    let (port, _) = upstream_server(|_, _| {
        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 2\r\n\r\nno".to_vec()
    });
    let rig = Rig::new("401", port);
    let a = rig.connect("sentry.io").call("GET", "/api/0/", &bearer(OWN), b"");
    assert_eq!(a.status, 403);
    assert!(!a.head.to_ascii_lowercase().contains("www-authenticate"), "{}", a.head);
    assert_eq!(a.json()["devsandbox"]["do"], "devsandbox login sentry-work");
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
    Respond { http10, keep: true, secrets: None, listeners, fields, max_rewrite: max }
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
