//! The broker's decision per request (credential-broker, *Policy*), pure
//! over the instance records, the parsed head and two sources read on
//! demand (the home's `credentials.toml`, the instance's config `access`),
//! so a config or grant edit applies to the next call. First refusal
//! answers:
//!
//! 1. the bridge's instance has a record (key and `instance_id`) with claims;
//! 2. the stream's listener is one of its claims' and stands for the
//!    upstream the helper's header names (that claim is "the claim" below);
//! 3. the path is strict ([`route::check_path`]), whatever comes next, so
//!    nothing an upstream would resolve differently is ever forwarded;
//! 4. the phantom, looked for only in inject slots: any other claim's
//!    phantom (this instance's or another's) in any of this instance's
//!    slots is a leak; a slot repeated, or both slots of a two-slot claim
//!    filled, is refused; no phantom of the claim in one of its slots: the
//!    call goes upstream uninjected (a `private` upstream answers a bare
//!    401 instead, never dialed);
//! 5. the credential: `access.<kind>.credential`, else the one granted to
//!    the instance's profile (`home::resolve`);
//! 6. the claim's version is approved for it; from here on the home's copy
//!    rules, never the instance's;
//! 7. the upstream and the phantom's slot are in that copy;
//! 8. the scope: `never` routes refused; a route in no scope is `other`,
//!    refused as an ask; the scope must be granted (`max`, unset: every
//!    scope) and requested (`access.<kind>.scopes`, empty: every scope),
//!    and one in `access.<kind>.ask` is refused as an ask: asks wait for
//!    permission requests (credential-broker step 4).
//!
//! Not here yet: token and registration endpoints a claim marks (no claim
//! field for them), the repo fence (github-and-sentry-access step 2),
//! `presence` (passes until step 4).

use anyhow::Result;

use super::head::{RequestHead, header_indices};
use super::redact;
use super::refusal::Refusal;
use super::route::{self, Scoped};
use crate::config::Access;
use crate::credentials::claim::{self, Pinned, Refusal as Shape, Slot};
use crate::credentials::home::{self, Home};
use crate::credentials::instance::{self, InstanceClaim};
use crate::devsbd::bridge::Caller;
use crate::devsbd::proto::ProxyHeader;
use crate::state::{Instance, State};

const ASKS: &str = "asks need permission requests (credential-broker step 4)";

/// What policy reads on demand, only for a call carrying the phantom.
pub trait Sources {
    fn home(&self) -> Result<Home>;
    /// The instance's config `access.<kind>`, read now.
    fn access(&self, instance: &Instance, kind: &str) -> Result<Option<Access>>;
}

pub struct Call<'a> {
    pub caller: &'a Caller,
    pub header: &'a ProxyHeader,
    pub head: &'a RequestHead,
}

/// Which phantom a call carried, for the log: never its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phantom {
    /// The claim's own, in its slot.
    Own,
    /// Another claim's of the same instance.
    OtherClaim,
    /// Another instance's.
    Foreign,
}

impl Phantom {
    pub fn marker(self) -> &'static str {
        match self {
            Phantom::Own => "own",
            Phantom::OtherClaim => "other-claim",
            Phantom::Foreign => "foreign",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub incarnation: Option<String>,
    pub kind: Option<String>,
    pub credential: Option<String>,
    pub scope: Option<String>,
    pub phantom: Option<Phantom>,
    /// Every listener of the instance, (upstream, port): where `Location`
    /// and `rewrite` fields point back to.
    pub listeners: Vec<(String, u16)>,
    /// The approved claim's, once read, else the instance's.
    pub shape: Option<Shape>,
}

#[derive(Debug)]
pub enum Action {
    Refuse(Refusal),
    /// `private`: the upstream is on the LAN; answer a bare 401 instead.
    Uninjected { private: bool },
    Inject(Box<Injection>),
}

#[derive(Debug)]
pub struct Injection {
    pub credential: String,
    pub slot: Slot,
    /// The home's approved copy.
    pub claim: Pinned,
    pub private: bool,
}

#[derive(Debug)]
pub struct Outcome {
    pub context: Context,
    pub action: Action,
}

pub fn decide(call: &Call<'_>, state: &State, sources: &dyn Sources) -> Outcome {
    let mut context = Context::default();
    let action = match steps(call, state, sources, &mut context) {
        Ok(action) => action,
        Err(mut r) => {
            r.credential = context.credential.clone();
            r.scope = context.scope.clone();
            Action::Refuse(r)
        }
    };
    Outcome { context, action }
}

fn internal(why: impl std::fmt::Display) -> Refusal {
    Refusal::new(500, "error", why.to_string())
}

fn steps(call: &Call<'_>, state: &State, sources: &dyn Sources, cx: &mut Context) -> Result<Action, Refusal> {
    let Caller { instance: key, instance_id } = call.caller;
    let inst = state
        .instances
        .get(key)
        .filter(|i| i.instance_id == *instance_id)
        .ok_or_else(|| Refusal::refused(format!("instance `{key}` has no record on this host")))?;
    if inst.claims.is_empty() {
        return Err(Refusal::refused(format!("instance `{key}` claims no credential")));
    }
    cx.incarnation = inst.incarnation.clone();
    cx.listeners =
        inst.claims.iter().flat_map(|c| c.listeners.iter().map(|l| (l.upstream.clone(), l.port))).collect();

    let (own, upstream) = match call.header {
        ProxyHeader::BaseUrl { upstream, listener } => match instance::by_port(&inst.claims, *listener) {
            Some((c, u)) if u == upstream => (c, u),
            _ => {
                return Err(Refusal::refused(format!(
                    "port {listener} is no broker listener for `{upstream}` on instance `{key}`"
                )));
            }
        },
        ProxyHeader::Intercept { .. } => {
            return Err(Refusal::refused("intercept mode isn't built yet (credential-broker step 3)"));
        }
    };
    cx.kind = Some(own.kind.clone());
    let (_, own_claim) = claim::from_canonical(&own.claim).map_err(|e| internal(format!("{e:#}")))?;
    cx.shape = own_claim.refusal.clone();

    let head = call.head;
    let path = head.target.path();
    route::check_path(path).map_err(|why| Refusal::new(400, "refused", why))?;

    let slot = match phantom_slot(head, inst, own, &own_claim, state, cx)? {
        Some(found) => found,
        None => {
            let private = own_claim.upstreams.iter().any(|u| u.host == upstream && u.private);
            return Ok(Action::Uninjected { private });
        }
    };

    let kind = own.kind.as_str();
    let access = sources
        .access(inst, kind)
        .map_err(|e| Refusal::new(503, "error", format!("cannot read the instance's config: {e:#}")))?
        .unwrap_or_default();
    let home = sources.home().map_err(|e| Refusal::new(503, "error", format!("{e:#}")))?;
    let profile = inst.profile.as_deref();
    let (name, entry) = match home::resolve(&home, kind, access.credential.as_deref(), profile) {
        Err(e) => return Err(Refusal::refused(format!("{e:#}"))),
        Ok(None) => {
            let to = match (&access.credential, profile) {
                (Some(named), _) => format!("named `{named}` (`access.{kind}.credential`)"),
                (None, Some(p)) => format!("granted to profile `{p}`"),
                (None, None) => "granted to instances without a profile".to_string(),
            };
            return Err(Refusal::refused(format!("no `{kind}` credential {to} on this host"))
                .todo(format!("devsandbox login {kind}")));
        }
        Ok(Some(found)) => found,
    };
    cx.credential = Some(name.to_string());

    let Some(approved) = entry.approved_claim(&own.version) else {
        let short = &own.version[..own.version.len().min(12)];
        return Err(Refusal::refused(format!(
            "the `{kind}` claim of feature `{}` (version {short}) isn't approved for credential `{name}`",
            own.feature
        ))
        .todo(format!("devsandbox access approve {name}")));
    };
    let (_, approved) = claim::from_canonical(approved).map_err(|e| internal(format!("{e:#}")))?;
    if approved.refusal.is_some() {
        cx.shape = approved.refusal.clone();
    }
    let Some(up) = approved.upstreams.iter().find(|u| u.host == upstream) else {
        return Err(Refusal::refused(format!("`{upstream}` isn't an upstream of the approved `{kind}` claim")));
    };
    if !approved.inject.contains(&slot) {
        return Err(Refusal::refused(format!("the approved `{kind}` claim doesn't inject where the phantom is")));
    }

    let call_route = format!("{} {}", head.method, route::path_only(path));
    let scoped = route::scope_of(&approved, &head.method, path);
    cx.scope = Some(scoped.name().to_string());
    let scope = match scoped {
        Scoped::Never => {
            return Err(Refusal::refused(format!(
                "`{call_route}` is a `never` route of the `{kind}` claim: no ask or rule opens it"
            )));
        }
        Scoped::Other => {
            return Err(Refusal::refused(format!(
                "`{call_route}` is in no scope of the `{kind}` claim (scope `other`): {ASKS}"
            )));
        }
        Scoped::Scope(s) => s,
    };
    if entry.max.as_ref().is_some_and(|max| !max.contains(&scope)) {
        return Err(Refusal::refused(format!("scope `{scope}` isn't granted by credential `{name}` (its `max`)"))
            .todo(format!("add `{scope}` to `max` of [{name}] in credentials.toml")));
    }
    if access.ask.contains(&scope) {
        return Err(Refusal::refused(format!("scope `{scope}` is in `access.{kind}.ask`: {ASKS}")));
    }
    if !access.scopes.is_empty() && !access.scopes.contains(&scope) {
        return Err(Refusal::refused(format!("scope `{scope}` isn't in `access.{kind}.scopes`"))
            .todo(format!("add `{scope}` to `access.{kind}.scopes` in the sandbox's config")));
    }
    Ok(Action::Inject(Box::new(Injection {
        credential: name.to_string(),
        slot,
        private: up.private,
        claim: approved,
    })))
}

/// Step 4: the slot holding the claim's phantom, `None` without one.
fn phantom_slot(
    head: &RequestHead,
    inst: &Instance,
    own: &InstanceClaim,
    own_claim: &Pinned,
    state: &State,
    cx: &mut Context,
) -> Result<Option<Slot>, Refusal> {
    let mut seen: Vec<String> = Vec::new();
    for c in &inst.claims {
        let slots = if c.phantom == own.phantom {
            own_claim.inject.clone()
        } else {
            claim::from_canonical(&c.claim).map(|(_, p)| p.inject).unwrap_or_default()
        };
        for s in &slots {
            seen.extend(slot_values(head, s));
        }
    }
    for v in &seen {
        if *v == own.phantom {
            continue;
        }
        if let Some(other) = instance::by_phantom(&inst.claims, v) {
            cx.phantom = Some(Phantom::OtherClaim);
            return Err(Refusal::new(
                403,
                "leak",
                format!("the `{}` claim's phantom was sent to a `{}` listener", other.kind, own.kind),
            ));
        }
        let foreign = state
            .instances
            .values()
            .filter(|i| i.instance_id != inst.instance_id)
            .any(|i| i.claims.iter().any(|c| c.phantom == *v));
        if foreign {
            cx.phantom = Some(Phantom::Foreign);
            return Err(Refusal::new(403, "leak", "another instance's phantom was sent from this one"));
        }
    }

    let mut filled = Vec::new();
    for s in &own_claim.inject {
        if slot_count(head, s) > 1 {
            return Err(Refusal::new(400, "refused", format!("{} is sent more than once", slot_name(s))));
        }
        if let Some(v) = slot_values(head, s).into_iter().next() {
            filled.push((s.clone(), v));
        }
    }
    if filled.len() > 1 {
        return Err(Refusal::new(400, "refused", format!("both inject slots of the `{}` claim are filled", own.kind)));
    }
    Ok(filled.into_iter().find(|(_, v)| *v == own.phantom).map(|(s, _)| {
        cx.phantom = Some(Phantom::Own);
        s
    }))
}

fn slot_name(s: &Slot) -> String {
    match s {
        Slot::Header { name, .. } => format!("header `{name}`"),
        Slot::Basic { .. } => "header `authorization`".to_string(),
        Slot::Query { param } => format!("query parameter `{param}`"),
    }
}

fn slot_count(head: &RequestHead, s: &Slot) -> usize {
    match s {
        Slot::Header { name, .. } => header_indices(&head.headers, name).count(),
        Slot::Basic { .. } => header_indices(&head.headers, "authorization").count(),
        Slot::Query { param } => query_values(head.target.path(), param).count(),
    }
}

/// Every value `s` holds in the request, in the slot's format (a header
/// that doesn't fit the format holds none).
fn slot_values(head: &RequestHead, s: &Slot) -> Vec<String> {
    let text = |v: &[u8]| std::str::from_utf8(v).ok().map(str::to_string);
    match s {
        Slot::Header { name, format } => header_indices(&head.headers, name)
            .filter_map(|i| text(&head.headers[i].value))
            .filter_map(|v| unformat(&v, format))
            .collect(),
        Slot::Basic { .. } => header_indices(&head.headers, "authorization")
            .filter_map(|i| basic_password(&head.headers[i].value))
            .collect(),
        Slot::Query { param } => query_values(head.target.path(), param).map(str::to_string).collect(),
    }
}

/// The `{token}` part of `value` written in `format`; the text before it
/// (an auth scheme) compares case-insensitively.
fn unformat(value: &str, format: &str) -> Option<String> {
    let (pre, post) = format.split_once(claim::TOKEN)?;
    let head = value.get(..pre.len())?;
    let rest = value.get(pre.len()..)?.strip_suffix(post)?;
    (head.eq_ignore_ascii_case(pre) && !rest.is_empty()).then(|| rest.to_string())
}

fn basic_password(value: &[u8]) -> Option<String> {
    let scheme = value.get(..6)?;
    if !scheme.eq_ignore_ascii_case(b"basic ") {
        return None;
    }
    let plain = redact::base64_decode(value[6..].trim_ascii())?;
    let plain = String::from_utf8(plain).ok()?;
    plain.split_once(':').map(|(_, p)| p.to_string()).filter(|p| !p.is_empty())
}

/// Raw values of `param` in the target's query, compared undecoded.
fn query_values<'a>(target: &'a str, param: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    target
        .split_once('?')
        .map(|(_, q)| q)
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(move |pair| match pair.split_once('=') {
            Some((k, v)) if k == param => Some(v),
            _ => None,
        })
}

/// The request with the phantom replaced by `token` in `slot` only: one
/// header value, or one query parameter of `path`.
pub fn inject(headers: &mut [super::head::Header], path: &mut String, slot: &Slot, token: &str) {
    match slot {
        Slot::Header { name, format } => {
            let first = header_indices(headers, name).next();
            if let Some(i) = first {
                headers[i].value = format.replace(claim::TOKEN, token).into_bytes();
            }
        }
        Slot::Basic { username } => {
            let first = header_indices(headers, "authorization").next();
            if let Some(i) = first {
                let mut v = b"Basic ".to_vec();
                v.extend(redact::basic_auth(username.as_bytes(), token.as_bytes()));
                headers[i].value = v;
            }
        }
        Slot::Query { param } => {
            let Some((p, q)) = path.split_once('?') else { return };
            let encoded = percent_encode(token);
            let q: Vec<String> = q
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((k, _)) if k == param => format!("{k}={encoded}"),
                    _ => pair.to_string(),
                })
                .collect();
            *path = format!("{p}?{}", q.join("&"));
        }
    }
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// What redaction masks for an injection of `token`: the value, each
/// basic-auth form the claim's slots would carry it in, and its query form.
pub fn secrets(claim: &Pinned, token: &str) -> redact::Secrets {
    let mut values = vec![token.as_bytes().to_vec(), percent_encode(token).into_bytes()];
    for s in &claim.inject {
        if let Slot::Basic { username } = s {
            values.push(redact::basic_auth(username.as_bytes(), token.as_bytes()));
        }
    }
    redact::Secrets::new(values)
}

#[cfg(test)]
pub(crate) mod tests;
