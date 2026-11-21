//! The broker as `serve` runs it: [`Service`] is the [`Broker`] the daemon's
//! bridges hand every `HTTP_PROXY` connection to
//! (docs/architecture/credentials/broker.md). Per connection it reads
//! requests until either side closes; per request it refuses browsers and a
//! foreign `Host`, decides ([`policy::decide`]), then answers the refusal or
//! makes the call: one upstream connection per request, the body streamed
//! both ways, the injected value redacted on the way back, `Location` and the
//! claim's `rewrite` fields pointed at the loopback, an injected call's 401
//! turned into a refusal naming `devsandbox login`. Every request leaves one
//! `access.log` line.
//!
//! Nothing is cached: `state.toml` per request, `credentials.toml`, the
//! store and the instance's config only for a call carrying its phantom,
//! so an edit to any of them applies to the next call.

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use super::audit::{Audit, Line};
use super::body::{BodyReader, ChunkedWriter};
use super::head::{self, Framing, Header, RequestHead, Version};
use super::policy::{self, Action, Call, Context, Injection, Sources};
use super::redact::Secrets;
use super::refusal::{Refusal, reason_phrase};
use super::upstream::{self, Body, Length, Request, Response, Upstream};
use super::{guard, rewrite, route};
use crate::config::{Access, Config};
use crate::credentials::claim::{self, Refusal as Shape};
use crate::credentials::home::{self, Home};
use crate::credentials::instance;
use crate::credentials::store::Store;
use crate::devsbd::bridge::{Broker, Caller};
use crate::devsbd::proto::ProxyHeader;
use crate::state::{Instance, State};

/// Response fields that describe one hop, never passed on.
const HOP_BY_HOP: &[&str] =
    &["connection", "keep-alive", "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade"];

pub struct Service {
    state: PathBuf,
    data: PathBuf,
    /// `Err`: the host had no usable TLS roots; calls answer 502 with it.
    upstream: std::result::Result<Upstream, String>,
    audit: Audit,
    /// Upstream host → the authority dialed instead: tests' local servers.
    authorities: HashMap<String, String>,
}

impl Service {
    /// The data dir is `state`'s.
    pub fn new(state: PathBuf, upstream: std::result::Result<Upstream, String>) -> Result<Service> {
        let data = state.parent().context("state path has no parent")?.to_path_buf();
        Ok(Service { audit: Audit::in_data(&data), state, data, upstream, authorities: HashMap::new() })
    }

    /// The user's `state.toml` and the host's TLS roots and proxy env.
    pub fn from_host(log: fn(&str)) -> Result<Service> {
        let upstream = Upstream::from_host().map_err(|e| {
            let why = format!("{e:#}");
            log(&format!("broker: {why}: brokered calls answer 502"));
            why
        });
        Service::new(State::path()?, upstream)
    }

    #[cfg(test)]
    pub fn with_authority(mut self, host: &str, authority: &str) -> Service {
        self.authorities.insert(host.to_string(), authority.to_string());
        self
    }

    fn request(
        &self,
        caller: &Caller,
        header: &ProxyHeader,
        reader: &mut BufReader<UnixStream>,
        w: &mut UnixStream,
    ) -> io::Result<bool> {
        let read = head::read_head(reader);
        // After the head: a keep-alive connection may idle for minutes.
        let state = State::load_at(&self.state);
        let shape = state.as_ref().ok().and_then(|s| listener_shape(s, caller, header));
        let mut line = Line {
            time: crate::inbox::rfc3339(crate::state::Instance::now()),
            instance: caller.instance.clone(),
            instance_id: caller.instance_id.clone(),
            ..Line::default()
        };
        let head = match read {
            Ok(Some(h)) => h,
            Ok(None) => return Ok(false),
            Err(e) => {
                let Some(why) = super::refusal(&e) else { return Err(e) };
                let r = Refusal::new(why.status(), "refused", why.to_string());
                self.refuse(w, &r, shape.as_ref(), true, &mut line)?;
                return Ok(false);
            }
        };
        line.method = head.method.clone();
        line.route = route::path_only(head.target.path()).to_string();
        let empty_body = matches!(head.framing, Framing::Empty | Framing::Length(0));
        let close_after_refusal = !empty_body || !head.keep_alive();

        let guarded = guard::refuse_browser(&head.headers).and_then(|()| match header {
            ProxyHeader::BaseUrl { listener, .. } => guard::check_host(&head, &format!("127.0.0.1:{listener}"), 80),
            ProxyHeader::Intercept { .. } => Ok(()),
        });
        if let Err(why) = guarded {
            let r = Refusal::new(why.status(), "refused", why.to_string());
            self.refuse(w, &r, shape.as_ref(), close_after_refusal, &mut line)?;
            return Ok(!close_after_refusal);
        }
        let state = match state {
            Ok(s) => s,
            Err(e) => {
                let r = Refusal::new(503, "error", format!("{e:#}"));
                self.refuse(w, &r, None, close_after_refusal, &mut line)?;
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
        let upstream_host = match header {
            ProxyHeader::BaseUrl { upstream, .. } => upstream.as_str(),
            ProxyHeader::Intercept { host, .. } => host.as_str(),
        };
        match outcome.action {
            Action::Refuse(r) => {
                self.refuse(w, &r, cx.shape.as_ref(), close_after_refusal, &mut line)?;
                Ok(!close_after_refusal)
            }
            Action::Uninjected { private: true } => {
                let close = close_after_refusal;
                let mut out = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n".to_vec();
                if close {
                    out.extend_from_slice(b"Connection: close\r\n");
                }
                out.extend_from_slice(b"\r\n");
                w.write_all(&out)?;
                line.verdict = "uninjected".into();
                line.reason = Some("a private upstream isn't dialed without the phantom".into());
                line.status = 401;
                self.audit.write(&line);
                Ok(!close)
            }
            Action::Uninjected { private: false } => {
                line.verdict = "uninjected".into();
                self.forward(&head, upstream_host, None, &cx, reader, w, &mut line)
            }
            Action::Inject(inj) => {
                let token = match Store::in_data(&self.data).token_now(&inj.credential) {
                    Ok(Some(t)) => t,
                    Ok(None) => {
                        let r = Refusal {
                            credential: Some(inj.credential.clone()),
                            ..Refusal::refused(format!("no secret is stored for credential `{}`", inj.credential))
                                .todo(format!("devsandbox login {}", inj.credential))
                        };
                        self.refuse(w, &r, cx.shape.as_ref(), close_after_refusal, &mut line)?;
                        return Ok(!close_after_refusal);
                    }
                    Err(e) => {
                        let r = Refusal::new(503, "error", format!("{e:#}"));
                        self.refuse(w, &r, cx.shape.as_ref(), close_after_refusal, &mut line)?;
                        return Ok(!close_after_refusal);
                    }
                };
                line.verdict = "allow".into();
                self.forward(&head, upstream_host, Some((&inj, token.expose())), &cx, reader, w, &mut line)
            }
        }
    }

    fn refuse(
        &self,
        w: &mut UnixStream,
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

    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        head: &RequestHead,
        upstream_host: &str,
        inject: Option<(&Injection, &str)>,
        cx: &Context,
        reader: &mut BufReader<UnixStream>,
        w: &mut UnixStream,
        line: &mut Line,
    ) -> io::Result<bool> {
        let keep = head.keep_alive();
        let upstream = match &self.upstream {
            Ok(u) => u,
            Err(why) => {
                let r = Refusal::new(502, "error", why.clone());
                let close = !keep || !matches!(head.framing, Framing::Empty | Framing::Length(0));
                self.refuse(w, &r, cx.shape.as_ref(), close, line)?;
                return Ok(!close);
            }
        };
        let mut headers = upstream_headers(&head.headers);
        let mut path = head.target.path().to_string();
        if let Some((inj, token)) = inject {
            policy::inject(&mut headers, &mut path, &inj.slot, token);
        }
        let authority = self.authorities.get(upstream_host).map_or(upstream_host, String::as_str);
        let url = format!("https://{authority}{path}");
        if head.expect_continue {
            w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        }

        let mut body = BodyReader::new(&mut *reader, head.framing);
        let sent = {
            let req_body = match head.framing {
                Framing::Empty => Body::Empty,
                Framing::Length(n) => Body::Length(n, &mut body),
                Framing::Chunked => Body::Chunked(&mut body),
            };
            let private = inject.is_some_and(|(i, _)| i.private);
            upstream.send(Request { method: &head.method, url: &url, headers: &headers, body: req_body, private })
        };
        let body_done = body.is_done();
        let keep = keep && body_done;
        let resp = match sent {
            Ok(resp) => resp,
            Err(e) => {
                let status = super::refusal(&e)
                    .map(|h| h.status())
                    .or_else(|| upstream::upstream_error(&e).map(|u| u.status()))
                    .unwrap_or(502);
                let r = Refusal::new(status, "error", format!("calling `{upstream_host}`: {e}"));
                self.refuse(w, &r, cx.shape.as_ref(), !keep, line)?;
                return Ok(keep);
            }
        };

        if let Some((inj, _)) = inject.filter(|_| resp.status == 401) {
            let r = Refusal {
                credential: Some(inj.credential.clone()),
                scope: cx.scope.clone(),
                ..Refusal::refused(format!(
                    "`{upstream_host}` answered 401 to credential `{}`: it's expired, revoked or lacks the permission",
                    inj.credential
                ))
                .todo(format!("devsandbox login {}", inj.credential))
            };
            self.refuse(w, &r, cx.shape.as_ref(), !keep, line)?;
            return Ok(keep);
        }

        let secrets = inject.map(|(inj, token)| policy::secrets(&inj.claim, token));
        let fields = inject.and_then(|(inj, _)| {
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
}

impl Broker for Service {
    fn serve(&self, caller: &Caller, header: ProxyHeader, stream: UnixStream) {
        let Ok(read) = stream.try_clone() else { return };
        let mut reader = BufReader::new(read);
        let mut w = stream;
        while let Ok(true) = self.request(caller, &header, &mut reader, &mut w) {}
        let _ = w.shutdown(std::net::Shutdown::Both);
    }
}

/// The instance's claim shape for the listener, for refusals before policy.
fn listener_shape(state: &State, caller: &Caller, header: &ProxyHeader) -> Option<Shape> {
    let ProxyHeader::BaseUrl { listener, .. } = header else { return None };
    let inst = state.instances.get(&caller.instance).filter(|i| i.instance_id == caller.instance_id)?;
    let (c, _) = instance::by_port(&inst.claims, *listener)?;
    claim::from_canonical(&c.claim).ok()?.1.refusal
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
