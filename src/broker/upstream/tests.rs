//! Against a local TLS server with the test CA under
//! `tests/fixtures/broker/` (cert for `localhost` and `127.0.0.1`), marked
//! private so the address check lets loopback through.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

use super::*;
use crate::broker::body::BodyReader;
use crate::broker::head::{self, RequestHead};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/broker").join(name)
}

fn roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from_pem_file(fixture("ca.pem")).unwrap()).unwrap();
    roots
}

fn timeouts() -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(5),
        idle: Duration::from_secs(10),
    }
}

fn client(proxy: Option<Proxy>) -> Upstream {
    Upstream::new(roots(), proxy, timeouts()).unwrap()
}

type Conn = BufReader<StreamOwned<ServerConnection, TcpStream>>;

struct Server {
    port: u16,
    listener: TcpListener,
    thread: JoinHandle<()>,
}

impl Server {
    /// Answers one connection with `handle`, then closes it with a
    /// `close_notify`.
    fn start(handle: impl FnOnce(&mut Conn) + Send + 'static) -> Server {
        let chain = CertificateDer::pem_file_iter(fixture("localhost.pem"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
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
        let accept = listener.try_clone().unwrap();
        let thread = thread::spawn(move || {
            let (tcp, _) = accept.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut conn = BufReader::new(StreamOwned::new(ServerConnection::new(config).unwrap(), tcp));
            handle(&mut conn);
            let tls = conn.get_mut();
            tls.conn.send_close_notify();
            let _ = tls.flush();
        });
        Server { port, listener, thread }
    }

    fn url(&self, host: &str, path: &str) -> String {
        format!("https://{host}:{}{path}", self.port)
    }

    /// Joins the handler, then checks nothing else connected.
    fn finish(self) {
        self.thread.join().unwrap();
        assert_no_connection(&self.listener);
    }
}

fn assert_no_connection(listener: &TcpListener) {
    listener.set_nonblocking(true).unwrap();
    let e = listener.accept().map(|_| ()).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
}

fn read_request(conn: &mut Conn) -> (Vec<u8>, RequestHead) {
    let raw = head::read_raw_head(conn).unwrap().unwrap();
    let parsed = head::parse_head(&raw).unwrap();
    (raw, parsed)
}

fn respond(conn: &mut Conn, bytes: &[u8]) {
    let tls = conn.get_mut();
    tls.write_all(bytes).unwrap();
    tls.flush().unwrap();
}

fn get<'a>(url: &'a str, headers: &'a [Header], private: bool) -> Request<'a> {
    Request {
        method: "GET",
        url,
        headers,
        body: Body::Empty,
        private,
    }
}

fn hdr(name: &str, value: &str) -> Header {
    Header {
        name: name.into(),
        value: value.as_bytes().to_vec(),
    }
}

fn body_of(resp: Response) -> Vec<u8> {
    let mut out = Vec::new();
    let mut body = resp.body;
    body.read_to_end(&mut out).unwrap();
    out
}

fn expect_err(r: io::Result<Response>) -> io::Error {
    match r {
        Ok(resp) => panic!("expected an error, got {}", resp.status),
        Err(e) => e,
    }
}

#[test]
fn exact_head_on_the_wire_and_raw_headers_back() {
    let (tx, rx) = mpsc::channel();
    let server = Server::start(move |conn| {
        let (raw, _) = read_request(conn);
        tx.send(raw).unwrap();
        respond(conn, b"HTTP/1.1 200 OK\r\nSet-Cookie: a\r\nX: 1\r\nset-cookie: b\r\nContent-Length: 5\r\n\r\nhello");
    });
    let url = server.url("localhost", "/p?q=1");
    let headers = [hdr("X-A", "1"), hdr("x-dup", "a"), hdr("x-dup", "b"), hdr("Authorization", "Bearer t")];
    let resp = client(None).send(get(&url, &headers, true)).unwrap();
    assert_eq!((resp.status, resp.reason.as_str()), (200, "OK"));
    assert_eq!(resp.headers, vec![hdr("Set-Cookie", "a"), hdr("X", "1"), hdr("set-cookie", "b"), hdr("Content-Length", "5")]);
    assert_eq!((resp.length, resp.decoded), (Length::Exact(5), None));
    assert_eq!(body_of(resp), b"hello");
    let want = format!(
        "GET /p?q=1 HTTP/1.1\r\nHost: localhost:{}\r\nX-A: 1\r\nx-dup: a\r\nx-dup: b\r\nAuthorization: Bearer t\r\n\
         Accept-Encoding: identity\r\nConnection: close\r\n\r\n",
        server.port
    );
    assert_eq!(String::from_utf8(rx.recv().unwrap()).unwrap(), want);
    server.finish();
}

#[test]
fn redirect_passed_back_not_followed() {
    let server = Server::start(|conn| {
        read_request(conn);
        respond(conn, b"HTTP/1.1 302 Found\r\nLocation: https://localhost/elsewhere\r\nContent-Length: 0\r\n\r\n");
    });
    let url = server.url("localhost", "/");
    let resp = client(None).send(get(&url, &[], true)).unwrap();
    assert_eq!(resp.status, 302);
    assert_eq!(head::header(&resp.headers, "location"), Some(&b"https://localhost/elsewhere"[..]));
    assert_eq!(resp.length, Length::Exact(0));
    assert_eq!(body_of(resp), b"");
    server.finish();
}

/// Yields its parts in order, waiting on `gate` before each part after the
/// first.
struct Gated {
    parts: VecDeque<&'static [u8]>,
    gate: Receiver<()>,
    first: bool,
}

impl Read for Gated {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(part) = self.parts.pop_front() else { return Ok(0) };
        if !std::mem::replace(&mut self.first, false) {
            self.gate.recv().unwrap();
        }
        buf[..part.len()].copy_from_slice(part);
        Ok(part.len())
    }
}

#[test]
fn streams_both_ways() {
    let (got_first, gate) = mpsc::channel();
    let (got_one, wait_one) = mpsc::channel();
    let server = Server::start(move |conn| {
        let (raw, req) = read_request(conn);
        assert!(String::from_utf8(raw).unwrap().contains("Transfer-Encoding: chunked\r\n"));
        let mut body = BodyReader::new(&mut *conn, req.framing);
        let mut first = [0u8; 5];
        body.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"first");
        got_first.send(()).unwrap();
        let mut rest = Vec::new();
        body.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"second");
        respond(conn, b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\none\r\n");
        wait_one.recv().unwrap();
        respond(conn, b"3\r\ntwo\r\n0\r\n\r\n");
    });
    let url = server.url("localhost", "/upload");
    let mut reader = Gated {
        parts: VecDeque::from([&b"first"[..], b"second"]),
        gate,
        first: true,
    };
    let resp = client(None)
        .send(Request {
            method: "POST",
            url: &url,
            headers: &[],
            body: Body::Chunked(&mut reader),
            private: true,
        })
        .unwrap();
    assert_eq!(resp.length, Length::Unframed);
    let mut body = resp.body;
    let mut one = [0u8; 3];
    body.read_exact(&mut one).unwrap();
    assert_eq!(&one, b"one");
    got_one.send(()).unwrap();
    let mut rest = Vec::new();
    body.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, b"two");
    server.finish();
}

#[test]
fn length_body_sent_and_short_body_refused() {
    let server = Server::start(|conn| {
        let (raw, req) = read_request(conn);
        assert!(String::from_utf8(raw).unwrap().contains("Content-Length: 4\r\n"));
        let mut body = Vec::new();
        BodyReader::new(&mut *conn, req.framing).read_to_end(&mut body).unwrap();
        assert_eq!(body, b"data");
        respond(conn, b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n");
    });
    let url = server.url("localhost", "/");
    let mut data = &b"data and more"[..];
    let resp = client(None)
        .send(Request {
            method: "PUT",
            url: &url,
            headers: &[],
            body: Body::Length(4, &mut data),
            private: true,
        })
        .unwrap();
    assert_eq!(resp.status, 201);
    server.finish();

    let server = Server::start(|conn| {
        read_request(conn);
        let _ = conn.read_to_end(&mut Vec::new());
    });
    let url = server.url("localhost", "/");
    let mut short = &b"abc"[..];
    let e = expect_err(client(None).send(Request {
        method: "PUT",
        url: &url,
        headers: &[],
        body: Body::Length(10, &mut short),
        private: true,
    }));
    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    server.finish();
}

#[test]
fn gzip_answer_decoded_br_refused() {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(b"plain text").unwrap();
    let gz = gz.finish().unwrap();
    let server = Server::start(move |conn| {
        read_request(conn);
        let mut resp = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n", gz.len()).into_bytes();
        resp.extend(&gz);
        respond(conn, &resp);
    });
    let url = server.url("localhost", "/");
    let resp = client(None).send(get(&url, &[], true)).unwrap();
    assert_eq!((resp.length, resp.decoded), (Length::Unframed, Some(Encoding::Gzip)));
    assert_eq!(body_of(resp), b"plain text");
    server.finish();

    let server = Server::start(|conn| {
        read_request(conn);
        respond(conn, b"HTTP/1.1 200 OK\r\nContent-Encoding: br\r\nContent-Length: 1\r\n\r\nx");
    });
    let url = server.url("localhost", "/");
    let e = expect_err(client(None).send(get(&url, &[], true)));
    assert_eq!(upstream_error(&e), Some(&UpstreamError::Encoding("br".into())));
    server.finish();
}

#[test]
fn body_to_the_connection_end() {
    let server = Server::start(|conn| {
        read_request(conn);
        respond(conn, b"HTTP/1.1 200 OK\r\n\r\nuntil close");
    });
    let url = server.url("localhost", "/");
    let resp = client(None).send(get(&url, &[], true)).unwrap();
    assert_eq!(resp.length, Length::Unframed);
    assert_eq!(body_of(resp), b"until close");
    server.finish();
}

#[test]
fn head_request_has_no_body() {
    let server = Server::start(|conn| {
        read_request(conn);
        respond(conn, b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 42\r\n\r\n");
    });
    let url = server.url("localhost", "/");
    let resp = client(None)
        .send(Request {
            method: "HEAD",
            ..get(&url, &[], true)
        })
        .unwrap();
    assert_eq!((resp.length, resp.decoded), (Length::Empty, None));
    assert_eq!(body_of(resp), b"");
    server.finish();
}

#[test]
fn untrusted_certificate_fails() {
    let server = Server::start(|conn| {
        let _ = conn.fill_buf();
    });
    let url = server.url("localhost", "/");
    let upstream = Upstream::new(RootCertStore::empty(), None, timeouts()).unwrap();
    let e = expect_err(upstream.send(get(&url, &[], true)));
    assert!(e.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()), "{e}");
    server.finish();
}

#[test]
fn loopback_refused_unless_private() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let proxy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = Proxy::parse(&format!("http://{}", proxy_listener.local_addr().unwrap()), "").unwrap();
    for upstream in [client(None), client(Some(proxy))] {
        for host in ["localhost", "127.0.0.1", "[::1]"] {
            let url = format!("https://{host}:{port}/");
            let e = expect_err(upstream.send(get(&url, &[], false)));
            match upstream_error(&e) {
                Some(UpstreamError::Refused(ip)) => assert!(ip.is_loopback(), "{ip}"),
                other => panic!("{host}: {other:?} ({e})"),
            }
        }
    }
    assert_no_connection(&listener);
    assert_no_connection(&proxy_listener);
}

#[test]
fn invalid_requests_refused_before_dialing() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("https://localhost:{}/", listener.local_addr().unwrap().port());
    let upstream = client(None);
    let cases: [(&str, Vec<Header>); 6] = [
        ("GET", vec![hdr("Host", "evil")]),
        ("GET", vec![hdr("content-length", "0")]),
        ("GET", vec![hdr("X", "a\r\nInjected: 1")]),
        ("GET", vec![hdr("Bad Name", "a")]),
        ("G T", vec![]),
        ("CONNECT", vec![]),
    ];
    for (method, headers) in &cases {
        let e = expect_err(upstream.send(Request {
            method,
            ..get(&url, headers, true)
        }));
        assert!(matches!(upstream_error(&e), Some(UpstreamError::Invalid(_))), "{method} {headers:?}: {e}");
    }
    let e = expect_err(upstream.send(get("http://localhost/", &[], true)));
    assert!(matches!(upstream_error(&e), Some(UpstreamError::Invalid(_))));
    assert_no_connection(&listener);
}

/// An HTTP proxy that records each `CONNECT` head, dials its target and
/// splices; a target it can't dial gets a 502.
fn start_proxy() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    thread::spawn(move || {
        for client in listener.incoming() {
            let mut client = client.unwrap();
            let raw = head::read_raw_head(&mut BufReader::with_capacity(1, &mut client)).unwrap().unwrap();
            let raw = String::from_utf8(raw).unwrap();
            let target = raw.split(' ').nth(1).unwrap().to_string();
            log.lock().unwrap().push(raw);
            let Ok(upstream) = TcpStream::connect(&target) else {
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
                continue;
            };
            client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").unwrap();
            for (mut from, mut to) in [
                (client.try_clone().unwrap(), upstream.try_clone().unwrap()),
                (upstream, client),
            ] {
                thread::spawn(move || {
                    let _ = io::copy(&mut from, &mut to);
                    let _ = to.shutdown(Shutdown::Write);
                });
            }
        }
    });
    (port, seen)
}

#[test]
fn proxy_tunnels_to_the_checked_address_with_sni_by_name() {
    let (tx, rx) = mpsc::channel();
    let server = Server::start(move |conn| {
        let (raw, _) = read_request(conn);
        let sni = conn.get_ref().conn.server_name().map(str::to_string);
        tx.send((raw, sni)).unwrap();
        respond(conn, b"HTTP/1.1 204 No Content\r\n\r\n");
    });
    let (proxy_port, seen) = start_proxy();
    let proxy = Proxy::parse(&format!("http://us%3Aer:pw@127.0.0.1:{proxy_port}"), "").unwrap();
    let url = server.url("localhost", "/via");
    let resp = client(Some(proxy)).send(get(&url, &[], true)).unwrap();
    assert_eq!(resp.status, 204);
    let (raw, sni) = rx.recv().unwrap();
    assert!(String::from_utf8(raw).unwrap().starts_with(&format!("GET /via HTTP/1.1\r\nHost: localhost:{}\r\n", server.port)));
    assert_eq!(sni.as_deref(), Some("localhost"));
    let port = server.port;
    server.finish();

    let seen = seen.lock().unwrap();
    // `localhost` may resolve to ::1 first, which the server doesn't listen on.
    let last = seen.last().unwrap();
    let want = format!("CONNECT 127.0.0.1:{0} HTTP/1.1\r\nHost: 127.0.0.1:{0}\r\nProxy-Authorization: Basic ", port);
    assert!(last.starts_with(&want), "{last:?}");
    let mut auth = b"Basic ".to_vec();
    auth.extend(crate::broker::redact::basic_auth(b"us:er", b"pw"));
    assert!(last.contains(&format!("Proxy-Authorization: {}\r\n", String::from_utf8(auth).unwrap())));
    for head in seen.iter() {
        assert!(!head.contains("localhost"), "CONNECT by name: {head:?}");
    }
}

#[test]
fn no_proxy_goes_direct() {
    let server = Server::start(|conn| {
        read_request(conn);
        respond(conn, b"HTTP/1.1 204 No Content\r\n\r\n");
    });
    let proxy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = Proxy::parse(&format!("http://{}", proxy_listener.local_addr().unwrap()), "localhost").unwrap();
    let url = server.url("localhost", "/");
    assert_eq!(client(Some(proxy)).send(get(&url, &[], true)).unwrap().status, 204);
    server.finish();
    assert_no_connection(&proxy_listener);
}
