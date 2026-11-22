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
//! turned into a refusal naming `devsandbox login`. An LFS batch's body is
//! read before policy (its `operation` makes it a read or a write), and an
//! injected batch's response has its transfers put behind handles on the
//! instance's LFS listener ([`super::lfs`]), whose connections this serves
//! too: each request's handle forwarded to its stored `href` with its
//! stored header. Every request leaves one `access.log` line.
//!
//! Nothing is cached but GitHub's answers about the repo fence's names
//! ([`github::Cache`]): `state.toml` per request, `credentials.toml`, the
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
use super::upstream::{self, Body, Length, Request, Response, Upstream, UpstreamError};
use super::{github, graphql, guard, intercept, lfs, rewrite, route};
use crate::config::{Access, Config};
use crate::credentials::ca::{Leaf, MemberCa};
use crate::credentials::claim::{self, Refusal as Shape, Slot};
use crate::credentials::home::{self, Home};
use crate::credentials::instance;
use crate::credentials::oauth::Http;
use crate::credentials::oauth::refresh::{self, Failure, Fresh, Refreshed, Renewer};
use crate::credentials::tokens::{self, Notices};
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

/// Marks a response to a call sent outside the repo fence; an upstream's
/// own is dropped.
const FENCE_HEADER: &str = "devsandbox-fence";

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
    /// Hosts dialed despite a private address: tests' local servers.
    private_hosts: BTreeSet<String>,
    lfs: Mutex<lfs::Store>,
    /// What GitHub answered for the fence's names and ids, per credential.
    github: Mutex<github::Cache>,
    /// Loaded at the first intercepted connection, so a host that
    /// intercepts nothing never makes a CA key; a failed load is retried.
    ca: Mutex<Option<Arc<RwLock<MemberCa>>>>,
    log: fn(&str),
    notices: Mutex<Notices>,
    /// Shows a notice's text: the desktop's notifier, a test's sink.
    notify: Box<dyn Fn(&str) + Send + Sync>,
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
            private_hosts: BTreeSet::new(),
            lfs: Mutex::new(lfs::Store::new(lfs::MAX_PER_INSTANCE)),
            ca: Mutex::default(),
            github: Mutex::default(),
            log: |_| {},
            notices: Mutex::default(),
            notify: Box::new(|body| {
                let popup = crate::inbox::ShownPopup {
                    key: None,
                    level: crate::devsbd::notify::Level::Warn,
                    body: body.to_string(),
                };
                crate::devsbd::desktop::notify_desktop("credentials", &popup);
            }),
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

    /// `host` dialed at `authority`, its private address allowed: a test's
    /// LFS storage host, or a claim's host past the one it may mark private.
    #[cfg(test)]
    pub fn with_private_host(mut self, host: &str, authority: &str) -> Service {
        self.private_hosts.insert(host.to_string());
        self.with_authority(host, authority)
    }

    #[cfg(test)]
    pub fn with_notify(mut self, notify: impl Fn(&str) + Send + Sync + 'static) -> Service {
        self.notify = Box::new(notify);
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
            ProxyHeader::Lfs { listener } => guard::check_host(&head, &format!("127.0.0.1:{listener}"), 80),
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

        let mut batch = None;
        if route::lfs_batch(&head.method, head.target.path()) {
            match batch_body(&head, conn)? {
                Ok(read) => batch = Some(read),
                Err((r, close)) => {
                    self.refuse(conn.get_mut(), &r, shape.as_ref(), close, &mut line)?;
                    return Ok(!close);
                }
            }
        }
        let mut gql = None;
        if route::graphql(&head.method, head.target.path()) && instance_claim(&state, caller, header).is_some_and(|c| c.graphql()) {
            match graphql_body(&head, conn)? {
                Ok(read) => gql = Some(read),
                Err((r, close)) => {
                    self.refuse(conn.get_mut(), &r, shape.as_ref(), close, &mut line)?;
                    return Ok(!close);
                }
            }
        }
        let call = Call {
            caller,
            header,
            head: &head,
            lfs: batch.as_ref().map(|(_, op)| *op),
            graphql: gql.as_ref().map(|(_, op)| op),
        };
        let token = |credential: &str| {
            let now = Instance::now();
            self.renewer.token(credential, now, &self.refresher(now)).ok().map(|f| f.token)
        };
        let https = self.upstream.as_ref().ok().map(|upstream| github::Https {
            upstream,
            authority: self.authorities.get(github::API).map_or(github::API, String::as_str),
            token: &token,
        });
        let live = https.as_ref().map(|t| github::Live {
            cache: &self.github,
            transport: t,
            state: &self.state,
            now: Instance::now(),
        });
        let outcome = policy::decide(&call, &state, &Files { data: &self.data, github: live });
        let pre = batch.map(|(body, _)| body).or(gql.map(|(body, _)| body));
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
                // git's curl sends the helper's credential only in answer to
                // a challenge.
                if matches!(cx.shape, Some(Shape::Git {})) {
                    out.extend_from_slice(b"WWW-Authenticate: Basic realm=\"devsandbox\"\r\n");
                }
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
                self.forward(&head, header, Via::Bare, &cx, pre, conn, &mut line)
            }
            Action::Outside { slot, private } => {
                line.verdict = "outside".into();
                self.forward(&head, header, Via::Stripped(&slot, private), &cx, pre, conn, &mut line)
            }
            Action::Inject(inj) => {
                let now = Instance::now();
                match self.renewer.token(&inj.credential, now, &self.refresher(now)) {
                    Ok(fresh) => {
                        line.verdict = "allow".into();
                        line.refreshed = fresh.refreshed;
                        self.forward(&head, header, Via::Inject(&inj, fresh), &cx, pre, conn, &mut line)
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

    /// `pre`: the body, already read (an LFS batch's).
    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        head: &RequestHead,
        header: &ProxyHeader,
        via: Via<'_>,
        cx: &Context,
        pre: Option<Vec<u8>>,
        conn: &mut Conn,
        line: &mut Line,
    ) -> io::Result<bool> {
        let (inject, strip) = match via {
            Via::Bare => (None, None),
            Via::Inject(inj, fresh) => (Some((inj, fresh)), None),
            Via::Stripped(slot, private) => (None, Some((slot, private))),
        };
        let upstream_host = match header {
            ProxyHeader::BaseUrl { upstream, .. } => upstream.as_str(),
            ProxyHeader::Intercept { host, .. } => host.as_str(),
            ProxyHeader::Lfs { .. } => unreachable!("policy refuses an LFS listener's request"),
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
        let read_ahead = pre.is_some();
        if head.expect_continue && !read_ahead {
            conn.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            conn.get_mut().flush()?;
        }

        let mut body = BodyReader::new(&mut *conn, head.framing);
        let replayable = inject.as_ref().is_some_and(|(_, f)| f.renewable && !f.refreshed);
        let held = match head.framing {
            _ if read_ahead => pre,
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
        let private = self.private_hosts.contains(upstream_host);
        let to = Target { upstream, authority, head, strip, private };
        let (sent, mut path) = to.send(inj.zip(token.as_ref().map(|t| t.expose())), held.as_deref(), &mut body);
        let keep = keep && (read_ahead || body.is_done());
        let w = conn.get_mut();
        let mut resp = match sent {
            Ok(resp) => resp,
            Err(e) => return self.failed(w, upstream_host, &e, cx, keep, line),
        };

        if strip.is_some() && resp.status == 401 {
            let repo = cx.outside.as_deref().unwrap_or_default();
            let kind = cx.kind.as_deref().unwrap_or_default();
            let r = Refusal {
                credential: cx.credential.clone(),
                scope: cx.scope.clone(),
                ..Refusal::refused(format!(
                    "`{repo}` is outside the instance's repo fence, and `{upstream_host}` answered 401 to the call \
                     sent without a credential"
                ))
                .todo(policy::repos_todo(repo, kind))
            };
            self.refuse(w, &r, cx.shape.as_ref(), !keep, line)?;
            return Ok(keep);
        }

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

        if let Some((inj, fresh)) = &inject {
            self.track_expiry(inj, fresh, &resp.headers);
        }
        let secrets = inj.zip(token.as_ref()).map(|(inj, token)| policy::secrets(&inj.claim, token.expose()));
        let fields = inj.filter(|_| matches!(header, ProxyHeader::BaseUrl { .. })).and_then(|inj| {
            inj.claim.rewrite.iter().find(|r| route::is(&r.route, &head.method, &path)).map(|r| r.fields.clone())
        });
        let lfs = inj.and_then(|i| i.lfs).filter(|_| resp.status == 200).map(|port| LfsRewrite {
            store: &self.lfs,
            owner: lfs::Owner {
                instance: line.instance.clone(),
                instance_id: line.instance_id.clone(),
                incarnation: cx.incarnation.clone(),
            },
            port,
            kind: cx.kind.as_deref(),
            credential: cx.credential.as_deref(),
        });
        line.status = resp.status;
        let respond = Respond {
            http10: head.version == Version::Http10,
            keep,
            outside: strip.is_some(),
            secrets: secrets.as_ref(),
            listeners: &cx.listeners,
            fields: fields.as_deref(),
            lfs,
            max_rewrite: rewrite::MAX_REWRITE,
        };
        let result = respond.write(resp, w, line);
        self.audit.write(line);
        result
    }

    /// Records a pasted token's expiry as the response reports it (the
    /// claim's `tokens.expiry` header), and sends the desktop notice of a
    /// credential ending soon (`tokens::Notices`): the daemon sees every
    /// injected call, so a credential in use is noticed whatever runs it.
    fn track_expiry(&self, inj: &Injection, fresh: &Fresh, headers: &[Header]) {
        let mut end = if fresh.pasted { inj.expires_at } else { fresh.ends };
        if fresh.pasted {
            let pairs = headers.iter().map(|h| (h.name.as_str(), h.value.as_slice()));
            if let Some(at) = tokens::captured(inj.claim.tokens.as_ref(), pairs, inj.expires_at) {
                if let Err(e) = home::set_expires_at(&home::path(&self.data), &inj.credential, at) {
                    (self.log)(&format!("broker: cannot record credential `{}`'s expiry: {e:#}", inj.credential));
                }
                end = Some(at);
            }
        }
        let Some(end) = end else { return };
        let due = self.notices.lock().unwrap_or_else(|e| e.into_inner()).due(&inj.credential, end, Instance::now());
        if let Some(body) = due {
            (self.notify)(&body);
        }
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

impl Service {
    /// One request on the LFS listener: its path's handle, looked up for
    /// the caller ([`lfs::Store::get`]), forwarded to the stored `href`.
    fn lfs_request(&self, caller: &Caller, listener: u16, conn: &mut Conn) -> io::Result<bool> {
        let read = head::read_head(conn);
        let mut line = Line { route: "lfs".into(), ..self.line(caller) };
        let head = match read {
            Ok(Some(h)) => h,
            Ok(None) => return Ok(false),
            Err(e) => {
                let Some(why) = super::refusal(&e) else { return Err(e) };
                let r = Refusal::new(why.status(), "refused", why.to_string());
                self.refuse(conn.get_mut(), &r, None, true, &mut line)?;
                return Ok(false);
            }
        };
        line.method = head.method.clone();
        let close = !matches!(head.framing, Framing::Empty | Framing::Length(0)) || !head.keep_alive();
        match self.lfs_entry(caller, listener, &head, &mut line) {
            Ok(entry) => {
                line.verdict = "allow".into();
                self.transfer(&head, &entry, conn, &mut line)
            }
            Err(r) => {
                self.refuse(conn.get_mut(), &r, None, close, &mut line)?;
                Ok(!close)
            }
        }
    }

    fn lfs_entry(&self, caller: &Caller, listener: u16, head: &RequestHead, line: &mut Line) -> Result<lfs::Entry, Refusal> {
        guard::refuse_browser(&head.headers)
            .and_then(|()| guard::check_host(head, &format!("127.0.0.1:{listener}"), 80))
            .map_err(|why| Refusal::new(why.status(), "refused", why.to_string()))?;
        let state = State::load_at(&self.state).map_err(|e| Refusal::new(503, "error", format!("{e:#}")))?;
        let key = &caller.instance;
        let inst = state
            .instances
            .get(key)
            .filter(|i| i.instance_id == caller.instance_id)
            .ok_or_else(|| Refusal::refused(format!("instance `{key}` has no record on this host")))?;
        line.incarnation = inst.incarnation.clone();
        if inst.intercept.as_ref().and_then(|i| i.lfs) != Some(listener) {
            return Err(Refusal::refused(format!("port {listener} is no LFS listener of instance `{key}`")));
        }
        let owner = lfs::Owner {
            instance: key.clone(),
            instance_id: caller.instance_id.clone(),
            incarnation: inst.incarnation.clone(),
        };
        let handle = head.target.path().strip_prefix('/').filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_alphanumeric()));
        let store = self.lfs.lock().unwrap_or_else(|e| e.into_inner());
        match handle.map_or(lfs::Lookup::Missing, |h| store.get(h, &owner, Instance::now())) {
            lfs::Lookup::Missing => Err(Refusal::new(
                404,
                "refused",
                "no such LFS transfer: its handle is unknown or expired (run the git lfs command again)",
            )),
            lfs::Lookup::Foreign => {
                Err(Refusal::new(403, "leak", "another instance's LFS transfer handle was sent from this one"))
            }
            lfs::Lookup::Found(e) => {
                let action = e.action;
                line.route = format!("lfs {}", action.name());
                line.kind = e.kind.clone();
                line.credential = e.credential.clone();
                if head.method != action.method() {
                    return Err(Refusal::refused(format!(
                        "`{}` to an LFS {} transfer, which takes `{}`",
                        head.method,
                        action.name(),
                        action.method()
                    )));
                }
                Ok(e.clone())
            }
        }
    }

    /// The client's request to `e.href` with `e`'s header and none of the
    /// client's credentials, the body streamed both ways. The log never
    /// gets the href (its query is a signature) nor a header value.
    fn transfer(&self, head: &RequestHead, e: &lfs::Entry, conn: &mut Conn, line: &mut Line) -> io::Result<bool> {
        let keep = head.keep_alive();
        let target = upstream::Target::parse(&e.href);
        let (upstream, target) = match (&self.upstream, target) {
            (Ok(u), Ok(t)) => (u, t),
            (Err(why), _) => {
                let close = !keep || !matches!(head.framing, Framing::Empty | Framing::Length(0));
                self.refuse(conn.get_mut(), &Refusal::new(502, "error", why.clone()), None, close, line)?;
                return Ok(!close);
            }
            (Ok(_), Err(_)) => {
                let r = Refusal::new(500, "error", "the stored LFS transfer has no https href");
                self.refuse(conn.get_mut(), &r, None, true, line)?;
                return Ok(false);
            }
        };
        let authority = self.authorities.get(&target.host).cloned().unwrap_or_else(|| target.authority());
        let url = format!("https://{authority}{}", target.path);
        let headers = transfer_headers(&head.headers, &e.headers);
        if head.expect_continue {
            conn.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            conn.get_mut().flush()?;
        }
        let mut body = BodyReader::new(&mut *conn, head.framing);
        let req_body = match head.framing {
            Framing::Empty => Body::Empty,
            Framing::Length(n) => Body::Length(n, &mut body),
            Framing::Chunked => Body::Chunked(&mut body),
        };
        let private = self.private_hosts.contains(&target.host);
        let sent = upstream.send(Request { method: &head.method, url: &url, headers: &headers, body: req_body, private });
        let keep = keep && body.is_done();
        let w = conn.get_mut();
        let resp = match sent {
            Ok(resp) => resp,
            Err(err) => {
                let status = super::refusal(&err)
                    .map(|h| h.status())
                    .or_else(|| upstream::upstream_error(&err).map(|u| u.status()))
                    .unwrap_or(502);
                // An invalid request's error quotes it: the stored header.
                let why = match upstream::upstream_error(&err) {
                    Some(UpstreamError::Invalid(_)) => "the stored LFS transfer isn't a valid request".to_string(),
                    _ => err.to_string(),
                };
                let r = Refusal::new(status, "error", format!("calling `{}`: {why}", target.host));
                self.refuse(w, &r, None, !keep, line)?;
                return Ok(keep);
            }
        };
        line.status = resp.status;
        let secrets = transfer_secrets(e);
        let respond = Respond {
            http10: head.version == Version::Http10,
            keep,
            outside: false,
            secrets: Some(&secrets),
            listeners: &[],
            fields: None,
            lfs: None,
            max_rewrite: 0,
        };
        let result = respond.write(resp, w, line);
        self.audit.write(line);
        result
    }
}

/// Stored header fields that carry no credential: left unmasked, as a
/// response may well repeat them.
const PLAIN_FIELDS: &[&str] = &["accept", "content-type"];

/// A transfer's fields: the client's, minus its credentials and any the
/// stored header names, then the stored header.
fn transfer_headers(client: &[Header], stored: &[(String, String)]) -> Vec<Header> {
    let mut out: Vec<Header> = upstream_headers(client)
        .into_iter()
        .filter(|h| {
            !["authorization", "cookie"].iter().any(|n| h.name.eq_ignore_ascii_case(n))
                && !stored.iter().any(|(name, _)| name.eq_ignore_ascii_case(&h.name))
        })
        .collect();
    out.extend(stored.iter().map(|(name, value)| Header { name: name.clone(), value: value.as_bytes().to_vec() }));
    out
}

/// What a transfer's response must not show the container: the stored
/// header's values and the href's query (a signed URL's signature).
fn transfer_secrets(e: &lfs::Entry) -> Secrets {
    let values = e.headers.iter().filter(|(name, _)| !PLAIN_FIELDS.iter().any(|p| name.eq_ignore_ascii_case(p)));
    let query = e.href.split_once('?').map(|(_, q)| q.as_bytes().to_vec());
    Secrets::new(values.map(|(_, v)| v.as_bytes().to_vec()).chain(query))
}

enum Via<'a> {
    /// As the tool sent it.
    Bare,
    Inject(&'a Injection, Fresh),
    /// Without the phantom's slot (`true`: a private upstream).
    Stripped(&'a Slot, bool),
}

/// Where one request goes: the upstream client, the authority dialed.
struct Target<'a> {
    upstream: &'a Upstream,
    authority: &'a str,
    head: &'a RequestHead,
    /// The slot removed and whether the upstream is private.
    strip: Option<(&'a Slot, bool)>,
    /// A test's private host, whatever the claim says.
    private: bool,
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
        let renamed = inject.and_then(|(inj, _)| inj.path.clone());
        let mut path = renamed.unwrap_or_else(|| head.target.path().to_string());
        if let Some((inj, token)) = inject {
            policy::inject(&mut headers, &mut path, &inj.slot, token);
        }
        if let Some((slot, _)) = self.strip {
            policy::strip(&mut headers, &mut path, slot);
        }
        let url = format!("https://{}{path}", self.authority);
        let mut held_body = held.unwrap_or_default();
        let req_body = match (held, head.framing) {
            (_, Framing::Empty) => Body::Empty,
            (Some(b), _) => Body::Length(b.len() as u64, &mut held_body),
            (None, Framing::Length(n)) => Body::Length(n, body),
            (None, Framing::Chunked) => Body::Chunked(body),
        };
        let private = self.private || inject.is_some_and(|(i, _)| i.private) || self.strip.is_some_and(|(_, p)| p);
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
            let keep = match header {
                ProxyHeader::Lfs { listener } => self.lfs_request(caller, listener, &mut conn),
                _ => self.request(caller, &header, &mut conn),
            };
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
    instance_claim(state, caller, header)?.refusal
}

/// The instance's claim for the listener or intercepted host. Its version
/// names the approved copy policy uses, so the two agree on what to read
/// before policy.
fn instance_claim(state: &State, caller: &Caller, header: &ProxyHeader) -> Option<claim::Pinned> {
    let inst = state.instances.get(&caller.instance).filter(|i| i.instance_id == caller.instance_id)?;
    let c = match header {
        ProxyHeader::BaseUrl { listener, .. } => instance::by_port(&inst.claims, *listener)?.0,
        ProxyHeader::Intercept { host, port } => policy::intercept_claim(inst, host, *port)?,
        ProxyHeader::Lfs { .. } => return None,
    };
    Some(claim::from_canonical(&c.claim).ok()?.1)
}

/// A batch's body and its `operation`, else the refusal and whether the
/// connection closes after it.
type BatchBody = Result<(Vec<u8>, route::LfsOp), (Refusal, bool)>;

/// An LFS batch's body, read whole before policy (its `operation` makes the
/// call a read or a write), after the `100 Continue` it asked for.
fn batch_body(head: &RequestHead, conn: &mut Conn) -> io::Result<BatchBody> {
    let too_large = || Refusal::new(413, "refused", format!("an LFS batch body over {} bytes", route::MAX_LFS_BATCH));
    if head::header(&head.headers, "content-encoding").is_some() {
        return Ok(Err((Refusal::new(400, "refused", "an LFS batch body with a `Content-Encoding`"), true)));
    }
    if matches!(head.framing, Framing::Length(n) if n > route::MAX_LFS_BATCH) {
        return Ok(Err((too_large(), true)));
    }
    if head.expect_continue {
        conn.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        conn.get_mut().flush()?;
    }
    let mut body = Vec::new();
    let mut reader = BodyReader::new(&mut *conn, head.framing);
    if let Err(e) = (&mut reader).take(route::MAX_LFS_BATCH + 1).read_to_end(&mut body) {
        let Some(why) = super::refusal(&e) else { return Err(e) };
        return Ok(Err((Refusal::new(why.status(), "refused", why.to_string()), true)));
    }
    if body.len() as u64 > route::MAX_LFS_BATCH {
        return Ok(Err((too_large(), true)));
    }
    match route::lfs_operation(&body) {
        Ok(op) => Ok(Ok((body, op))),
        Err(why) => Ok(Err((Refusal::new(400, "refused", why), !head.keep_alive()))),
    }
}

/// A GraphQL request's body as sent and its operation, else the refusal and
/// whether the connection closes after it.
type GraphqlBody = Result<(Vec<u8>, graphql::Operation), (Refusal, bool)>;

/// A GraphQL request's body, read whole before policy (its operation is the
/// call's route), after the `100 Continue` it asked for: within
/// [`graphql::MAX_BODY`] as sent and decoded, then [`graphql::parse`]d.
/// The bytes as sent go upstream.
fn graphql_body(head: &RequestHead, conn: &mut Conn) -> io::Result<GraphqlBody> {
    let too_large = || Refusal::new(413, "refused", format!("a GraphQL body over {} bytes", graphql::MAX_BODY));
    if matches!(head.framing, Framing::Length(n) if n > graphql::MAX_BODY) {
        return Ok(Err((too_large(), true)));
    }
    if head.expect_continue {
        conn.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        conn.get_mut().flush()?;
    }
    let mut body = Vec::new();
    let mut reader = BodyReader::new(&mut *conn, head.framing);
    if let Err(e) = (&mut reader).take(graphql::MAX_BODY + 1).read_to_end(&mut body) {
        let Some(why) = super::refusal(&e) else { return Err(e) };
        return Ok(Err((Refusal::new(why.status(), "refused", why.to_string()), true)));
    }
    if body.len() as u64 > graphql::MAX_BODY {
        return Ok(Err((too_large(), true)));
    }
    let close = !head.keep_alive();
    let read = graphql::decode(&body, head::header(&head.headers, "content-encoding")).and_then(|d| graphql::parse(&d));
    match read {
        Ok(op) => Ok(Ok((body, op))),
        Err(bad) => Ok(Err((Refusal::new(bad.status, "refused", bad.why), close))),
    }
}

/// An injected LFS batch's response rewrite ([`lfs::rewrite_batch`]).
struct LfsRewrite<'a> {
    store: &'a Mutex<lfs::Store>,
    owner: lfs::Owner,
    port: u16,
    kind: Option<&'a str>,
    credential: Option<&'a str>,
}

impl LfsRewrite<'_> {
    fn rewrite(&self, body: &[u8]) -> Option<Vec<u8>> {
        let to = lfs::Batch {
            owner: &self.owner,
            kind: self.kind,
            credential: self.credential,
            port: self.port,
            now: Instance::now(),
        };
        lfs::rewrite_batch(body, &to, &mut self.store.lock().unwrap_or_else(|e| e.into_inner()))
    }
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
    /// Sent stripped, outside the repo fence: marked `devsandbox-fence: outside`.
    outside: bool,
    secrets: Option<&'a Secrets>,
    listeners: &'a [(String, u16)],
    /// The claim's `rewrite` fields for this route.
    fields: Option<&'a [String]>,
    /// An injected LFS batch's 200: its actions go behind handles.
    lfs: Option<LfsRewrite<'a>>,
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
            if (name == "content-encoding" && resp.decoded.is_some()) || name == FENCE_HEADER {
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
        if self.outside {
            headers.push(Header { name: FENCE_HEADER.into(), value: b"outside".to_vec() });
        }
        let mut reason = resp.reason.clone().into_bytes();
        if let Some(s) = self.secrets {
            s.mask(&mut reason);
        }

        let mut body = resp.body;
        let mut prefix = Vec::new();
        if (self.fields.is_some() || self.lfs.is_some()) && resp.length != Length::Empty {
            (&mut body).take(self.max_rewrite as u64 + 1).read_to_end(&mut prefix)?;
            if prefix.len() <= self.max_rewrite {
                // Before masking: the stored header is what goes upstream.
                let mut out = match &self.lfs {
                    Some(lfs) => lfs.rewrite(&prefix).unwrap_or(prefix),
                    None => prefix,
                };
                if let Some(s) = self.secrets {
                    s.mask(&mut out);
                }
                let out = match self.fields {
                    Some(fields) => rewrite::json_fields(&out, fields, self.listeners).unwrap_or(out),
                    None => out,
                };
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
    github: Option<github::Live<'a>>,
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

    fn github(&self) -> Option<&dyn github::Lookup> {
        self.github.as_ref().map(|l| l as &dyn github::Lookup)
    }
}

#[cfg(test)]
pub(crate) mod tests;
