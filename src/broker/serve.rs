//! The broker as `serve` runs it: [`Service`] is the [`Broker`] the daemon's
//! bridges hand every `HTTP_PROXY` connection to
//! (docs/architecture/credentials/broker.md). Per connection it reads
//! requests until either side closes; per request it refuses browsers and a
//! foreign `Host`, decides ([`policy::decide`]), then answers the refusal or
//! makes the call: one upstream connection per request, the body streamed
//! both ways, the injected value redacted on the way back, `Location` and the
//! claim's `rewrite` fields pointed at the loopback (base-URL mode only). An
//! intercepted connection is admitted before its TLS, then served the same
//! way over the TLS the broker terminates ([`super::intercept`]) with the
//! member CA's leaf for its host. An OAuth credential's
//! token is renewed ahead of expiry, and on an upstream 401 renewed and the
//! call replayed when its body was held; any other injected call's 401 is
//! turned into a refusal naming `devsandbox login`. Every request leaves one
//! `access.log` line.
//!
//! Nothing is cached: `state.toml` per request, `credentials.toml`, the
//! store and the instance's config only for a call carrying its phantom,
//! so an edit to any of them applies to the next call.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context as _, Result, bail};
use rustls::{ServerConnection, StreamOwned};

use super::audit::{Audit, Line};
use super::body::{BodyReader, ChunkedWriter};
use super::head::{self, Framing, Header, RequestHead, Version};
use super::policy::{self, Action, Call, Context, Injection, Sources};
use super::redact::Secrets;
use super::refusal::{Refusal, reason_phrase};
use super::upstream::{self, Body, Length, Request, Response, Upstream};
use super::{guard, intercept, rewrite, route};
use crate::config::{Access, Config};
use crate::credentials::ca::{Leaf, MemberCa};
use crate::credentials::claim::{self, Refusal as Shape, Slot};
use crate::credentials::home::{self, Home};
use crate::credentials::instance;
use crate::credentials::oauth::Http;
use crate::credentials::oauth::refresh::{self, Failure, Fresh, Refreshed, Renewer};
use crate::credentials::store::OAuthToken;
use crate::devsbd::bridge::{Broker, Caller};
use crate::devsbd::proto::ProxyHeader;
use crate::state::{Instance, State};

/// Response fields that describe one hop, never passed on.
const HOP_BY_HOP: &[&str] =
    &["connection", "keep-alive", "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade"];

/// A renewable call's body up to this is read before it's sent, so a 401 can
/// be replayed with a renewed token; a longer or chunked one streams.
pub const MAX_REPLAY: u64 = 1024 * 1024;

/// What a `503` for an unreachable token endpoint asks the tool to wait.
const RETRY_AFTER: u16 = 30;

pub struct Service {
    state: PathBuf,
    data: PathBuf,
    /// `Err`: the host had no usable TLS roots; calls answer 502 with it.
    upstream: std::result::Result<Upstream, String>,
    /// The token endpoints' client: `upstream`'s, without the private
    /// override.
    tokens: std::result::Result<Http, String>,
    renewer: Renewer,
    audit: Audit,
    /// Upstream host → the authority dialed instead: tests' local servers.
    authorities: HashMap<String, String>,
    /// Loaded at the first intercepted connection, so a host that
    /// intercepts nothing never makes a CA key; a failed load is retried.
    ca: Mutex<Option<Arc<RwLock<MemberCa>>>>,
    log: fn(&str),
}

/// A connection's bytes: the bridge's stream, or the TLS over it.
enum Wire {
    Plain(UnixStream),
    Tls(Box<StreamOwned<ServerConnection, UnixStream>>),
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Wire::Plain(s) => s.read(buf),
            Wire::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Wire::Plain(s) => s.write(buf),
            Wire::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Wire::Plain(s) => s.flush(),
            Wire::Tls(s) => s.flush(),
        }
    }
}

impl Wire {
    fn close(&mut self) {
        let sock = match self {
            Wire::Plain(s) => s,
            Wire::Tls(s) => {
                s.conn.send_close_notify();
                let _ = s.flush();
                &mut s.sock
            }
        };
        let _ = sock.shutdown(std::net::Shutdown::Both);
    }
}

type Conn = BufReader<Wire>;

impl Service {
    /// The data dir is `state`'s.
    pub fn new(state: PathBuf, upstream: std::result::Result<Upstream, String>) -> Result<Service> {
        let data = state.parent().context("state path has no parent")?.to_path_buf();
        let tokens = upstream.as_ref().map(|u| Http::new(u.clone())).map_err(Clone::clone);
        Ok(Service {
            audit: Audit::in_data(&data),
            renewer: Renewer::new(data.clone()),
            state,
            data,
            upstream,
            tokens,
            authorities: HashMap::new(),
            ca: Mutex::default(),
            log: |_| {},
        })
    }

    /// The user's `state.toml` and the host's TLS roots and proxy env.
    pub fn from_host(log: fn(&str)) -> Result<Service> {
        let upstream = Upstream::from_host().map_err(|e| {
            let why = format!("{e:#}");
            log(&format!("broker: {why}: brokered calls answer 502"));
            why
        });
        Ok(Service { log, ..Service::new(State::path()?, upstream)? })
    }

    #[cfg(test)]
    pub fn with_authority(mut self, host: &str, authority: &str) -> Service {
        self.authorities.insert(host.to_string(), authority.to_string());
        self
    }

    #[cfg(test)]
    pub fn with_tokens(mut self, http: Http) -> Service {
        self.tokens = Ok(http);
        self
    }

    fn refresher(&self, now: u64) -> impl Fn(&OAuthToken) -> Refreshed + '_ {
        move |t| match &self.tokens {
            Ok(http) => refresh::refresh(http, t, now),
            Err(why) => Refreshed::Unavailable(why.clone()),
        }
    }

    /// The member CA's leaf for `host`, covering it first when the CA
    /// doesn't yet.
    fn leaf(&self, host: &str) -> Result<Leaf> {
        let ca = {
            let mut slot = self.ca.lock().unwrap_or_else(|e| e.into_inner());
            match &*slot {
                Some(ca) => ca.clone(),
                None => slot.insert(Arc::new(RwLock::new(MemberCa::load_or_create(&self.data)?))).clone(),
            }
        };
        {
            let ca = ca.read().unwrap_or_else(|e| e.into_inner());
            if ca.hosts().contains(host) {
                return ca.leaf(host);
            }
        }
        let mut ca = ca.write().unwrap_or_else(|e| e.into_inner());
        if ca.ensure_hosts(&BTreeSet::from([host.to_string()]))? {
            (self.log)(&format!(
                "broker: the member CA now covers `{host}`: containers trust it once the CA is reinstalled in them"
            ));
        }
        ca.leaf(host)
    }

    /// An intercepted stream admitted (before any TLS: devsbd already
    /// answered the `CONNECT`, so a refusal can only close it) and its TLS
    /// handshake done; `None` when it was closed, with an `access.log` line
    /// unless the client merely went away.
    fn open_intercept(&self, caller: &Caller, host: &str, port: u16, stream: UnixStream) -> Option<Wire> {
        let mut line = self.line(caller);
        let admitted = match State::load_at(&self.state) {
            Ok(state) => policy::admit_intercept(&state, caller, host, port).map(|c| c.kind.clone()),
            Err(e) => Err(Refusal::new(503, "error", format!("{e:#}"))),
        };
        let closed = |line: &mut Line, verdict: &str, reason: String| {
            line.verdict = verdict.into();
            line.reason = Some(reason);
            self.audit.write(line);
            let _ = stream.shutdown(std::net::Shutdown::Both);
        };
        let kind = match admitted {
            Ok(kind) => kind,
            Err(r) => {
                closed(&mut line, r.verdict, r.reason);
                return None;
            }
        };
        line.kind = Some(kind);
        let config = self.leaf(host).and_then(|leaf| intercept::server_config(host, leaf));
        let conn = config.and_then(|c| ServerConnection::new(c).context("the TLS server"));
        let mut tls = match conn {
            Ok(conn) => StreamOwned::new(conn, stream.try_clone().ok()?),
            Err(e) => {
                closed(&mut line, "error", format!("no TLS for `{host}`: {e:#}"));
                return None;
            }
        };
        while tls.conn.is_handshaking() {
            if let Err(e) = tls.conn.complete_io(&mut tls.sock) {
                if e.kind() == io::ErrorKind::InvalidData {
                    closed(&mut line, "refused", format!("TLS handshake for `{host}`: {e}"));
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return None;
            }
        }
        Some(Wire::Tls(Box::new(tls)))
    }

    fn line(&self, caller: &Caller) -> Line {
        Line {
            time: crate::inbox::rfc3339(crate::state::Instance::now()),
            instance: caller.instance.clone(),
            instance_id: caller.instance_id.clone(),
            ..Line::default()
        }
    }

    fn request(&self, caller: &Caller, header: &ProxyHeader, conn: &mut Conn) -> io::Result<bool> {
        let read = head::read_head(conn);
        // After the head: a keep-alive connection may idle for minutes.
        let state = State::load_at(&self.state);
        let shape = state.as_ref().ok().and_then(|s| claim_shape(s, caller, header));
        let mut line = self.line(caller);
        let head = match read {
            Ok(Some(h)) => h,
            Ok(None) => return Ok(false),
            Err(e) => {
                let Some(why) = super::refusal(&e) else { return Err(e) };
                let r = Refusal::new(why.status(), "refused", why.to_string());
                self.refuse(conn.get_mut(), &r, shape.as_ref(), true, &mut line)?;
                return Ok(false);
            }
        };
        line.method = head.method.clone();
        line.route = route::path_only(head.target.path()).to_string();
        let empty_body = matches!(head.framing, Framing::Empty | Framing::Length(0));
        let close_after_refusal = !empty_body || !head.keep_alive();

        let guarded = guard::refuse_browser(&head.headers).and_then(|()| match header {
            ProxyHeader::BaseUrl { listener, .. } => guard::check_host(&head, &format!("127.0.0.1:{listener}"), 80),
            ProxyHeader::Intercept { host, .. } => guard::check_host(&head, host, 443),
        });
        if let Err(why) = guarded {
            let r = Refusal::new(why.status(), "refused", why.to_string());
            self.refuse(conn.get_mut(), &r, shape.as_ref(), close_after_refusal, &mut line)?;
            return Ok(!close_after_refusal);
        }
        let state = match state {
            Ok(s) => s,
            Err(e) => {
                let r = Refusal::new(503, "error", format!("{e:#}"));
                self.refuse(conn.get_mut(), &r, None, close_after_refusal, &mut line)?;
                return Ok(!close_after_refusal);
            }
        };

        let call = Call { caller, header, head: &head };
        let outcome = policy::decide(&call, &state, &Files { data: &self.data });
        let cx = outcome.context;
        line.incarnation = cx.incarnation.clone();
        line.kind = cx.kind.clone();
        line.credential = cx.credential.clone();
        line.scope = cx.scope.clone();
        line.phantom = cx.phantom.map(policy::Phantom::marker);
        match outcome.action {
            Action::Refuse(r) => {
                self.refuse(conn.get_mut(), &r, cx.shape.as_ref(), close_after_refusal, &mut line)?;
                Ok(!close_after_refusal)
            }
            Action::Uninjected { private: true } => {
                let close = close_after_refusal;
                let mut out = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n".to_vec();
                if close {
                    out.extend_from_slice(b"Connection: close\r\n");
                }
                out.extend_from_slice(b"\r\n");
                conn.get_mut().write_all(&out)?;
                line.verdict = "uninjected".into();
                line.reason = Some("a private upstream isn't dialed without the phantom".into());
                line.status = 401;
                self.audit.write(&line);
                Ok(!close)
            }
            Action::Uninjected { private: false } => {
                line.verdict = "uninjected".into();
                self.forward(&head, header, None, &cx, conn, &mut line)
            }
            Action::Inject(inj) => {
                let now = Instance::now();
                match self.renewer.token(&inj.credential, now, &self.refresher(now)) {
                    Ok(fresh) => {
                        line.verdict = "allow".into();
                        line.refreshed = fresh.refreshed;
                        self.forward(&head, header, Some((&inj, fresh)), &cx, conn, &mut line)
                    }
                    Err(f) => {
                        let r = failure_refusal(f, &inj.credential, cx.scope.clone());
                        self.refuse(conn.get_mut(), &r, cx.shape.as_ref(), close_after_refusal, &mut line)?;
                        Ok(!close_after_refusal)
                    }
                }
            }
        }
    }

    fn refuse(
        &self,
        w: &mut dyn Write,
        r: &Refusal,
        shape: Option<&Shape>,
        close: bool,
        line: &mut Line,
    ) -> io::Result<()> {
        line.verdict = r.verdict.to_string();
        line.reason = Some(r.reason.clone());
        line.status = r.status;
        self.audit.write(line);
        w.write_all(&r.response(shape, close))
    }

    fn forward(
        &self,
        head: &RequestHead,
        header: &ProxyHeader,
        inject: Option<(&Injection, Fresh)>,
        cx: &Context,
        conn: &mut Conn,
        line: &mut Line,
    ) -> io::Result<bool> {
        let upstream_host = match header {
            ProxyHeader::BaseUrl { upstream, .. } => upstream.as_str(),
            ProxyHeader::Intercept { host, .. } => host.as_str(),
        };
        let keep = head.keep_alive();
        let upstream = match &self.upstream {
            Ok(u) => u,
            Err(why) => {
                let r = Refusal::new(502, "error", why.clone());
                let close = !keep || !matches!(head.framing, Framing::Empty | Framing::Length(0));
                self.refuse(conn.get_mut(), &r, cx.shape.as_ref(), close, line)?;
                return Ok(!close);
            }
        };
        let authority = self.authorities.get(upstream_host).map_or(upstream_host, String::as_str);
        if head.expect_continue {
            conn.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            conn.get_mut().flush()?;
        }

        let mut body = BodyReader::new(&mut *conn, head.framing);
        let replayable = inject.as_ref().is_some_and(|(_, f)| f.renewable && !f.refreshed);
        let held = match head.framing {
            _ if !replayable => None,
            Framing::Empty => Some(Vec::new()),
            Framing::Length(n) if n <= MAX_REPLAY => {
                let mut buf = Vec::new();
                if let Err(e) = body.read_to_end(&mut buf) {
                    return self.failed(conn.get_mut(), upstream_host, &e, cx, false, line);
                }
                Some(buf)
            }
            _ => None,
        };
        let inj = inject.as_ref().map(|(i, _)| *i);
        let mut token = inject.as_ref().map(|(_, f)| f.token.clone());
        let to = Target { upstream, authority, head };
        let (sent, mut path) = to.send(inj.zip(token.as_ref().map(|t| t.expose())), held.as_deref(), &mut body);
        let keep = keep && body.is_done();
        let w = conn.get_mut();
        let mut resp = match sent {
            Ok(resp) => resp,
            Err(e) => return self.failed(w, upstream_host, &e, cx, keep, line),
        };

        if let Some((inj, fresh)) = &inject
            && resp.status == 401
        {
            let now = Instance::now();
            let renewed = match replayable {
                true => self.renewer.after_401(&inj.credential, &fresh.token, now, &self.refresher(now)),
                false => Ok(None),
            };
            let answered = format!("`{upstream_host}` answered 401 to credential `{}`", inj.credential);
            let (new, held) = match (renewed, &held) {
                (Err(f), _) => {
                    let r = failure_refusal(f, &inj.credential, cx.scope.clone());
                    self.refuse(w, &r, cx.shape.as_ref(), !keep, line)?;
                    return Ok(keep);
                }
                (Ok(None), _) => {
                    let why = match fresh.refreshed {
                        true => format!("{answered}'s token renewed for this call: it's revoked or lacks the permission"),
                        false => format!("{answered}: it's expired, revoked or lacks the permission"),
                    };
                    return self.unauthorized(w, &to, inj, &fresh.token, why, cx, keep, line);
                }
                (Ok(Some(new)), None) => {
                    line.refreshed |= new.refreshed;
                    let why = format!(
                        "{answered} and the request's body streamed past {MAX_REPLAY} bytes, so it couldn't be \
                         replayed with a renewed token: send it again"
                    );
                    return self.unauthorized(w, &to, inj, &new.token, why, cx, keep, line);
                }
                (Ok(Some(new)), Some(held)) => (new, held),
            };
            line.refreshed |= new.refreshed;
            let (sent, p) = to.send(Some((inj, new.token.expose())), Some(held), &mut io::empty());
            path = p;
            resp = match sent {
                Ok(resp) => resp,
                Err(e) => return self.failed(w, upstream_host, &e, cx, keep, line),
            };
            if resp.status == 401 {
                let why = format!("{answered}'s renewed token too: it's revoked or lacks the permission");
                return self.unauthorized(w, &to, inj, &new.token, why, cx, keep, line);
            }
            token = Some(new.token);
        }

        let secrets = inj.zip(token.as_ref()).map(|(inj, token)| policy::secrets(&inj.claim, token.expose()));
        let fields = inj.filter(|_| matches!(header, ProxyHeader::BaseUrl { .. })).and_then(|inj| {
            inj.claim.rewrite.iter().find(|r| route::is(&r.route, &head.method, &path)).map(|r| r.fields.clone())
        });
        line.status = resp.status;
        let respond = Respond {
            http10: head.version == Version::Http10,
            keep,
            secrets: secrets.as_ref(),
            listeners: &cx.listeners,
            fields: fields.as_deref(),
            max_rewrite: rewrite::MAX_REWRITE,
        };
        let result = respond.write(resp, w, line);
        self.audit.write(line);
        result
    }

    /// A call that couldn't be made, answered as `UpstreamError::status`
    /// says (400 for a bad request body).
    fn failed(
        &self,
        w: &mut dyn Write,
        upstream_host: &str,
        e: &io::Error,
        cx: &Context,
        keep: bool,
        line: &mut Line,
    ) -> io::Result<bool> {
        let status = super::refusal(e)
            .map(|h| h.status())
            .or_else(|| upstream::upstream_error(e).map(|u| u.status()))
            .unwrap_or(502);
        let r = Refusal::new(status, "error", format!("calling `{upstream_host}`: {e}"));
        self.refuse(w, &r, cx.shape.as_ref(), !keep, line)?;
        Ok(keep)
    }

    /// An injected call's 401 that stays one: never passed on (the tool
    /// would start its own login), a refusal naming `devsandbox login`. The
    /// broker calls the claim's `check` route itself, and only a 401 there
    /// marks the credential broken: the tool shaped every byte of the call
    /// but the slot.
    #[allow(clippy::too_many_arguments)]
    fn unauthorized(
        &self,
        w: &mut dyn Write,
        to: &Target,
        inj: &Injection,
        token: &crate::credentials::store::Secret,
        why: String,
        cx: &Context,
        keep: bool,
        line: &mut Line,
    ) -> io::Result<bool> {
        if let Some(route) = to.check_401(inj, token.expose()) {
            let reason = format!("its `check` route `{route}` answered 401");
            self.renewer.mark_broken(&inj.credential, reason, Instance::now());
        }
        let r = Refusal {
            credential: Some(inj.credential.clone()),
            scope: cx.scope.clone(),
            ..Refusal::refused(why).todo(format!("devsandbox login {}", inj.credential))
        };
        self.refuse(w, &r, cx.shape.as_ref(), !keep, line)?;
        Ok(keep)
    }
}

/// Where one request goes: the upstream client, the authority dialed.
struct Target<'a> {
    upstream: &'a Upstream,
    authority: &'a str,
    head: &'a RequestHead,
}

impl Target<'_> {
    /// The tool's request with `token` in the slot: the body `held`, else
    /// streamed from `body`. Also the path as sent.
    fn send(
        &self,
        inject: Option<(&Injection, &str)>,
        held: Option<&[u8]>,
        body: &mut dyn Read,
    ) -> (io::Result<Response>, String) {
        let head = self.head;
        let mut headers = upstream_headers(&head.headers);
        let mut path = head.target.path().to_string();
        if let Some((inj, token)) = inject {
            policy::inject(&mut headers, &mut path, &inj.slot, token);
        }
        let url = format!("https://{}{path}", self.authority);
        let mut held_body = held.unwrap_or_default();
        let req_body = match (held, head.framing) {
            (_, Framing::Empty) => Body::Empty,
            (Some(b), _) => Body::Length(b.len() as u64, &mut held_body),
            (None, Framing::Length(n)) => Body::Length(n, body),
            (None, Framing::Chunked) => Body::Chunked(body),
        };
        let private = inject.is_some_and(|(i, _)| i.private);
        let sent = self.upstream.send(Request { method: &head.method, url: &url, headers: &headers, body: req_body, private });
        (sent, path)
    }

    /// Calls the claim's `check` route (a `GET` or `HEAD` with no sandbox
    /// input) with `token`: the route when it answered 401.
    fn check_401<'c>(&self, inj: &'c Injection, token: &str) -> Option<&'c claim::Route> {
        let route = inj.claim.check.as_ref()?;
        let claim::Route::Http { method, path } = route else { return None };
        let mut path = path.clone();
        let mut headers = Vec::new();
        match &inj.slot {
            Slot::Header { name, .. } => headers.push(Header { name: name.clone(), value: Vec::new() }),
            Slot::Basic { .. } => headers.push(Header { name: "Authorization".into(), value: Vec::new() }),
            Slot::Query { param } => {
                path.push(if path.contains('?') { '&' } else { '?' });
                path.push_str(param);
                path.push('=');
            }
        }
        policy::inject(&mut headers, &mut path, &inj.slot, token);
        let url = format!("https://{}{path}", self.authority);
        let req = Request { method: method.as_str(), url: &url, headers: &headers, body: Body::Empty, private: inj.private };
        self.upstream.send(req).is_ok_and(|r| r.status == 401).then_some(route)
    }
}

impl Broker for Service {
    fn serve(&self, caller: &Caller, header: ProxyHeader, stream: UnixStream) {
        let (header, wire) = match header {
            ProxyHeader::Intercept { host, port } => {
                let host = intercept::fold(&host);
                let Some(wire) = self.open_intercept(caller, &host, port, stream) else { return };
                (ProxyHeader::Intercept { host, port }, wire)
            }
            base => (base, Wire::Plain(stream)),
        };
        let mut conn = BufReader::new(wire);
        loop {
            let keep = self.request(caller, &header, &mut conn);
            if conn.get_mut().flush().is_err() || !matches!(keep, Ok(true)) {
                break;
            }
        }
        conn.get_mut().close();
    }
}

/// The instance's claim shape for the listener or intercepted host, for
/// refusals before policy.
fn claim_shape(state: &State, caller: &Caller, header: &ProxyHeader) -> Option<Shape> {
    let inst = state.instances.get(&caller.instance).filter(|i| i.instance_id == caller.instance_id)?;
    let c = match header {
        ProxyHeader::BaseUrl { listener, .. } => instance::by_port(&inst.claims, *listener)?.0,
        ProxyHeader::Intercept { host, port } => policy::intercept_claim(inst, host, *port)?,
    };
    claim::from_canonical(&c.claim).ok()?.1.refusal
}

fn failure_refusal(f: Failure, credential: &str, scope: Option<String>) -> Refusal {
    let login = format!("devsandbox login {credential}");
    let r = match f {
        Failure::Missing => Refusal::refused(format!("no secret is stored for credential `{credential}`")).todo(login),
        Failure::Store(why) => Refusal::new(503, "error", why),
        Failure::Refused(why) => {
            Refusal::refused(format!("renewing credential `{credential}`'s token was refused: {why}")).todo(login)
        }
        Failure::Unavailable(why) => Refusal {
            retry_after: Some(RETRY_AFTER),
            ..Refusal::new(503, "error", format!("cannot renew credential `{credential}`'s token: {why}"))
        },
    };
    Refusal { credential: Some(credential.to_string()), scope, ..r }
}

/// The request's fields minus what the upstream client writes itself and
/// what the request's `Connection` names.
fn upstream_headers(headers: &[Header]) -> Vec<Header> {
    let named: Vec<String> = head::list_elements(headers, "connection")
        .map(|t| String::from_utf8_lossy(t).to_ascii_lowercase())
        .collect();
    headers
        .iter()
        .filter(|h| {
            let name = h.name.to_ascii_lowercase();
            !upstream::RESERVED.contains(&name.as_str()) && !named.contains(&name)
        })
        .cloned()
        .collect()
}

/// How one upstream response goes back to the tool.
struct Respond<'a> {
    http10: bool,
    keep: bool,
    secrets: Option<&'a Secrets>,
    listeners: &'a [(String, u16)],
    /// The claim's `rewrite` fields for this route.
    fields: Option<&'a [String]>,
    max_rewrite: usize,
}

impl Respond<'_> {
    fn write(&self, resp: Response, w: &mut dyn Write, line: &mut Line) -> io::Result<bool> {
        let mut keep = self.keep;
        let named: Vec<String> = head::list_elements(&resp.headers, "connection")
            .map(|t| String::from_utf8_lossy(t).to_ascii_lowercase())
            .collect();
        let mut headers = Vec::new();
        let mut lengths = Vec::new();
        for h in &resp.headers {
            let name = h.name.to_ascii_lowercase();
            if HOP_BY_HOP.contains(&name.as_str()) || named.contains(&name) {
                continue;
            }
            if name == "content-encoding" && resp.decoded.is_some() {
                continue;
            }
            let mut value = h.value.clone();
            if let Some(s) = self.secrets {
                s.mask(&mut value);
            }
            if name == "location"
                && let Some(v) = rewrite::location(&value, self.listeners)
            {
                value = v;
            }
            if name == "content-length" {
                lengths.push(Header { name: h.name.clone(), value });
            } else {
                headers.push(Header { name: h.name.clone(), value });
            }
        }
        let mut reason = resp.reason.clone().into_bytes();
        if let Some(s) = self.secrets {
            s.mask(&mut reason);
        }

        let mut body = resp.body;
        let mut prefix = Vec::new();
        if let Some(fields) = self.fields.filter(|_| resp.length != Length::Empty) {
            (&mut body).take(self.max_rewrite as u64 + 1).read_to_end(&mut prefix)?;
            if prefix.len() <= self.max_rewrite {
                if let Some(s) = self.secrets {
                    s.mask(&mut prefix);
                }
                let out = rewrite::json_fields(&prefix, fields, self.listeners).unwrap_or(prefix);
                headers.push(Header { name: "Content-Length".into(), value: out.len().to_string().into_bytes() });
                w.write_all(&status_head(resp.status, &reason, &headers, keep, self.http10))?;
                w.write_all(&out)?;
                return Ok(keep);
            }
            line.reason = Some(format!("not rewritten: the body is over {} bytes", self.max_rewrite));
        }

        let redactor = self.secrets.map(Secrets::stream);
        match resp.length {
            Length::Empty => {
                headers.extend(lengths);
                w.write_all(&status_head(resp.status, &reason, &headers, keep, self.http10))?;
            }
            Length::Exact(n) => {
                headers.push(Header { name: "Content-Length".into(), value: n.to_string().into_bytes() });
                w.write_all(&status_head(resp.status, &reason, &headers, keep, self.http10))?;
                pump(&prefix, &mut body, redactor, w)?;
            }
            Length::Unframed if self.http10 => {
                keep = false;
                w.write_all(&status_head(resp.status, &reason, &headers, keep, self.http10))?;
                pump(&prefix, &mut body, redactor, w)?;
            }
            Length::Unframed => {
                headers.push(Header { name: "Transfer-Encoding".into(), value: b"chunked".to_vec() });
                w.write_all(&status_head(resp.status, &reason, &headers, keep, self.http10))?;
                let mut chunked = ChunkedWriter::new(&mut *w);
                pump(&prefix, &mut body, redactor, &mut chunked)?;
                chunked.finish()?;
            }
        }
        Ok(keep)
    }
}

fn status_head(status: u16, reason: &[u8], headers: &[Header], keep: bool, http10: bool) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} ").into_bytes();
    if reason.is_empty() {
        out.extend_from_slice(reason_phrase(status).as_bytes());
    } else {
        out.extend_from_slice(reason);
    }
    out.extend_from_slice(b"\r\n");
    for h in headers {
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(&h.value);
        out.extend_from_slice(b"\r\n");
    }
    match (keep, http10) {
        (false, _) => out.extend_from_slice(b"Connection: close\r\n"),
        (true, true) => out.extend_from_slice(b"Connection: keep-alive\r\n"),
        (true, false) => {}
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// `prefix` then the rest of `body`, through the redactor, flushed per
/// read so a stream (SSE) goes out as it comes.
fn pump(
    prefix: &[u8],
    body: &mut dyn Read,
    mut redactor: Option<super::redact::Redactor>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut send = |chunk: &[u8], out: &mut dyn Write| -> io::Result<()> {
        match &mut redactor {
            Some(r) => out.write_all(&r.feed(chunk))?,
            None => out.write_all(chunk)?,
        }
        out.flush()
    };
    if !prefix.is_empty() {
        send(prefix, out)?;
    }
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        send(&buf[..n], out)?;
    }
    if let Some(r) = redactor {
        out.write_all(&r.finish())?;
    }
    out.flush()
}

/// The files policy reads, from the data dir and the instance's config root.
struct Files<'a> {
    data: &'a Path,
}

impl Sources for Files<'_> {
    fn home(&self) -> Result<Home> {
        home::load(&home::path(self.data))
    }

    fn access(&self, inst: &Instance, kind: &str) -> Result<Option<Access>> {
        let Some(dir) = &inst.config_dir else {
            bail!("the instance was recorded without its config root: rebuild it");
        };
        let resolved = Config::load(dir)?.resolve_sandbox(&inst.sandbox)?;
        Ok(resolved.properties.access.and_then(|mut a| a.remove(kind)))
    }
}

#[cfg(test)]
pub(crate) mod tests;
