//! The broker's decision per request (credential-broker, *Policy*), pure
//! over the instance records, the parsed head and two sources read on
//! demand (the home's `credentials.toml`, the instance's config `access`),
//! so a config or grant edit applies to the next call. First refusal
//! answers:
//!
//! 1. the bridge's instance has a record (key and `instance_id`) with claims;
//! 2. base URL: the stream's listener is one of its base-URL claims' and
//!    stands for the upstream the helper's header names; intercept: the
//!    `CONNECT` target is port 443 of an upstream of one of its
//!    intercept-mode claims ([`intercept_claim`]) (that claim is "the
//!    claim" below);
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
//! 8. the route: `never` routes refused; a route in no scope is `other`,
//!    asked for a claim without a repo fence, refused for a fenced one
//!    (an answer couldn't name a repo); a path past `{repo}` that decodes
//!    badly refused;
//!    `POST /graphql` to a claim with GraphQL routes is its operation's
//!    instead ([`graphql_scopes`]): a query's `graphql query` route,
//!    repo-less, or each top-level mutation field's `graphql mutation
//!    <name>`, every `fence` input it holds placed inside the fence or the
//!    call refused (a write);
//! 9. the repo fence, for a fenced claim ([`Pinned::fenced`]): a route
//!    whose `fence` names an input but `{repo}` and a `/repositories/{id}`
//!    route's `{id}` (a body field) can't be placed yet and is refused; a
//!    `{repo}` or repository id outside the instance's fence
//!    ([`fence_for`], resolved to ids by [`github::Fence`]) goes upstream
//!    with the phantom stripped ([`Action::Outside`]), or is refused naming
//!    `access.<kind>.repos` for a write ([`route::is_write`]; an LFS batch
//!    by its body's `operation`, [`Call::lfs`]); a `{repo}` naming an
//!    entry's old name is sent to its current one; a route without either
//!    is repo-less;
//! 10. the org fence, for a claim with `orgs` ([`check_org`]): a route
//!     holding `orgs.var` names an org of the credential's `orgs`,
//!     narrowed by `access.<kind>.orgs` when it names any, by slug in any
//!     case or by the id one resolves to ([`orgs::place`]), else is
//!     refused, reads too; a credential naming no orgs refuses every such
//!     route. An `orgs.lists` route's answer is filtered to them
//!     ([`Injection::orgs`]);
//! 11. every scope the call needs must be granted (`max`, unset: every
//!     scope) and requested (`access.<kind>.scopes`, empty: every scope);
//!     `other` takes neither. Then one in `access.<kind>.ask` on a fenced
//!     claim's repo-less route is refused (never asked); else, for each
//!     asked one (in `access.<kind>.ask`, or `other`) by its [`AskKey`]
//!     (instance, credential, scope, the repo a fenced claim's call
//!     names): a deny kept for it in `access.json`
//!     ([`Sources::remembered`]) refuses, any asked one's before an ask;
//!     then one allowed by a rule on the credential
//!     ([`home::Entry::rule_for`]), a *this instance* answer kept for it
//!     or [`Sources::granted`] goes through, and the first other one is
//!     [`Action::Ask`]: the caller holds the call for an answer and
//!     decides again ([`super::asks`]). An `upload` LFS batch needs the
//!     claim's `lfs.upload` scope instead of its route's. After the scope
//!     asks, each target of a `presence` route or mutation the call is
//!     ([`presence`]; an id resolved by the home, else refused) is asked
//!     the same way, keyed by route and target too, whatever its scope
//!     allows, and no rule answers it.
//!
//! A git smart-HTTP request ([`route::git_service`]) is refused in git's
//! shape whatever the claim's: git prints a text body.
//!
//! Not here yet: token and registration endpoints a claim marks (no claim
//! field for them), REST body inputs in the fence, `presence` `ref` entries
//! (git pushes).

use anyhow::Result;

use super::asks::{self, Ask, AskKey, PresenceAt};
use super::github::{self, Place};
use super::graphql;
use super::head::{RequestHead, header_indices};
use super::orgs;
use super::presence;
use super::redact;
use super::refusal::Refusal;
use super::route::{self, Scoped};
use crate::config::Access;
use crate::credentials::answers::{Kind, Remembered};
use crate::credentials::claim::{self, Mode, Pinned, Refusal as Shape, Slot};
use crate::credentials::fence;
use crate::credentials::home::{self, Home};
use crate::credentials::instance::{self, InstanceClaim};
use crate::devsbd::bridge::Caller;
use crate::devsbd::proto::ProxyHeader;
use crate::state::{Instance, State};

/// Why a fenced claim's unattributable call is refused, not asked: an
/// answer couldn't name a repo.
const FENCED_OTHER: &str = "and a repo-fenced claim never asks for one";

/// What policy reads on demand, only for a call carrying the phantom.
pub trait Sources {
    fn home(&self) -> Result<Home>;
    /// The instance's config `access.<kind>`, read now.
    fn access(&self, instance: &Instance, kind: &str) -> Result<Option<Access>>;
    /// What resolves the fence to GitHub's ids; `None`: entries match by
    /// name only.
    fn github(&self) -> Option<&dyn github::Lookup> {
        None
    }
    /// What resolves an org's slug to its id; `None`: orgs match by slug
    /// only.
    fn orgs(&self) -> Option<&dyn orgs::Lookup> {
        None
    }
    /// Whether an ask with `key` is answered for this call: it goes
    /// through instead of asking. May take a one-shot grant, so it's
    /// asked only once every refusal is ruled out.
    fn granted(&self, _key: &AskKey) -> bool {
        false
    }
    /// The answer kept in `access.json` for `key`, read now.
    fn remembered(&self, _key: &AskKey) -> Result<Option<Remembered>> {
        Ok(None)
    }
}

pub struct Call<'a> {
    pub caller: &'a Caller,
    pub header: &'a ProxyHeader,
    pub head: &'a RequestHead,
    /// An LFS batch's `operation`, read from its body before policy
    /// ([`route::lfs_operation`]); `None` for any other call.
    pub lfs: Option<route::LfsOp>,
    /// A GraphQL request's operation, read from its body before policy
    /// ([`graphql::parse`]) when the claim has GraphQL routes.
    pub graphql: Option<&'a graphql::Operation>,
    /// A presence route's JSON body, read before policy when
    /// [`presence::reads_body`].
    pub body: Option<&'a serde_json::Value>,
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
    /// and `rewrite` fields point back to. Empty in intercept mode: the
    /// tool already talks to the real host.
    pub listeners: Vec<(String, u16)>,
    /// The approved claim's, once read, else the instance's; git's for a
    /// smart-HTTP request.
    pub shape: Option<Shape>,
    /// The `owner/repo` of an [`Action::Outside`] call.
    pub outside: Option<String>,
}

#[derive(Debug)]
pub enum Action {
    Refuse(Refusal),
    /// `private`: the upstream is on the LAN; answer a bare 401 instead.
    Uninjected { private: bool },
    Inject(Box<Injection>),
    /// Outside the repo fence: upstream with the phantom's slot removed,
    /// no credential (`private`: dialed as the upstream's injected calls
    /// are).
    Outside { slot: Slot, private: bool },
    /// Held for a permission request; decided again once it's answered.
    Ask(Box<Ask>),
}

/// A scope a call needs, for the last step.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Need {
    scope: String,
    /// The repo a fenced claim's call names.
    repo: Option<String>,
    /// A fenced claim's call naming no repo: never asked.
    repo_less: bool,
    /// Scope `other`: always asked, past `max` and `access.<kind>.scopes`,
    /// which name claim scopes only.
    other: bool,
}

#[derive(Debug)]
pub struct Injection {
    pub credential: String,
    pub slot: Slot,
    /// The home's approved copy.
    pub claim: Pinned,
    pub private: bool,
    /// The instance's LFS listener, for an LFS batch to a host the approved
    /// claim's `lfs` names: its response's transfers are pointed there.
    pub lfs: Option<u16>,
    /// The entry's recorded `expires_at` (unix seconds).
    pub expires_at: Option<u64>,
    /// The path to send instead of the tool's: the repo it named renamed to
    /// the current name of the repository that name was pinned to, so a
    /// later owner of the old name never gets the credential.
    pub path: Option<String>,
    /// An `orgs.lists` route's: its answer keeps only the fence's orgs.
    pub orgs: Option<OrgList>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgList {
    /// The list elements' top-level fields naming an org.
    pub fields: Vec<String>,
    /// The allowed slugs ([`orgs::allowed`]).
    pub allowed: Vec<String>,
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

/// Step 1: the caller's record, carrying claims.
fn known<'a>(state: &'a State, caller: &Caller) -> Result<&'a Instance, Refusal> {
    let Caller { instance: key, instance_id } = caller;
    let inst = state
        .instances
        .get(key)
        .filter(|i| i.instance_id == *instance_id)
        .ok_or_else(|| Refusal::refused(format!("instance `{key}` has no record on this host")))?;
    if inst.claims.is_empty() {
        return Err(Refusal::refused(format!("instance `{key}` claims no credential")));
    }
    Ok(inst)
}

/// The intercept-mode claim of `inst` with `host` among its upstreams, for a
/// `CONNECT` to `host:port`; only port 443 is intercepted.
pub fn intercept_claim<'a>(inst: &'a Instance, host: &str, port: u16) -> Option<&'a InstanceClaim> {
    if port != 443 {
        return None;
    }
    inst.claims.iter().find(|c| {
        claim::from_canonical(&c.claim)
            .is_ok_and(|(_, p)| p.mode == Mode::Intercept && p.upstreams.iter().any(|u| u.host == host))
    })
}

/// Steps 1 and 2 for an intercepted connection, before its TLS: whether the
/// caller's instance intercepts `host:port`, and the claim that does.
pub fn admit_intercept<'a>(state: &'a State, caller: &Caller, host: &str, port: u16) -> Result<&'a InstanceClaim, Refusal> {
    let inst = known(state, caller)?;
    intercept_claim(inst, host, port).ok_or_else(|| {
        Refusal::refused(format!("`{host}:{port}` is no intercepted upstream of instance `{}`", caller.instance))
    })
}

fn steps(call: &Call<'_>, state: &State, sources: &dyn Sources, cx: &mut Context) -> Result<Action, Refusal> {
    let key = &call.caller.instance;
    let inst = known(state, call.caller)?;
    cx.incarnation = inst.incarnation.clone();

    let no_listener = |listener: &u16, upstream: &str| {
        Refusal::refused(format!("port {listener} is no broker listener for `{upstream}` on instance `{key}`"))
    };
    let (own, upstream, to) = match call.header {
        ProxyHeader::BaseUrl { upstream, listener } => match instance::by_port(&inst.claims, *listener) {
            Some((c, u)) if u == upstream => {
                cx.listeners = inst
                    .claims
                    .iter()
                    .flat_map(|c| c.listeners.iter().map(|l| (l.upstream.clone(), l.port)))
                    .collect();
                (c, u, format!("a `{}` listener", c.kind))
            }
            _ => return Err(no_listener(listener, upstream)),
        },
        ProxyHeader::Intercept { host, port } => {
            let c = admit_intercept(state, call.caller, host, *port)?;
            (c, host.as_str(), format!("the `{}` claim's `{host}`", c.kind))
        }
        ProxyHeader::Lfs { listener } => {
            return Err(Refusal::refused(format!("port {listener} is an LFS listener, not a brokered upstream")));
        }
    };
    cx.kind = Some(own.kind.clone());
    let (_, own_claim) = claim::from_canonical(&own.claim).map_err(|e| internal(format!("{e:#}")))?;
    cx.shape = own_claim.refusal.clone();
    if let ProxyHeader::BaseUrl { listener, .. } = call.header
        && own_claim.mode != Mode::BaseUrl
    {
        return Err(no_listener(listener, upstream));
    }

    let head = call.head;
    let path = head.target.path();
    let git = route::git_service(&head.method, path).is_some();
    if git {
        cx.shape = Some(Shape::Git {});
    }
    route::check_path(path).map_err(|why| Refusal::new(400, "refused", why))?;

    let slot = match phantom_slot(head, inst, own, &own_claim, state, &to, cx)? {
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
    if approved.refusal.is_some() && !git {
        cx.shape = approved.refusal.clone();
    }
    let Some(up) = approved.upstreams.iter().find(|u| u.host == upstream) else {
        return Err(Refusal::refused(format!("`{upstream}` isn't an upstream of the approved `{kind}` claim")));
    };
    if !approved.inject.contains(&slot) {
        return Err(Refusal::refused(format!("the approved `{kind}` claim doesn't inject where the phantom is")));
    }

    let call_route = format!("{} {}", head.method, route::path_only(path));
    let mut renamed = None;
    let mut org_list = None;
    let mut presence_needs = Vec::new();
    let needed = if approved.graphql() && route::graphql(&head.method, path) {
        let Some(op) = call.graphql else {
            return Err(internal("a GraphQL request reached policy with its document unread"));
        };
        let mut fence = approved.fenced().then(|| fence_for(inst, &access, state, sources, &approved, name));
        graphql_scopes(&approved, op, fence.as_mut(), kind, cx, &mut presence_needs)?
    } else {
        if call.body.is_none() && presence::reads_body(&approved, &head.method, path) {
            return Err(internal("a presence call reached policy with its body unread"));
        }
        let matched = route::scope_of(&approved, &head.method, path).map_err(|why| Refusal::new(400, "refused", why))?;
        cx.scope = Some(matched.scoped.name().to_string());
        let lfs_upload = approved.lfs.as_ref().filter(|_| call.lfs == Some(route::LfsOp::Upload));
        let (scope, other) = match matched.scoped.clone() {
            Scoped::Never => {
                return Err(Refusal::refused(format!(
                    "`{call_route}` is a `never` route of the `{kind}` claim: no ask or rule opens it"
                )));
            }
            Scoped::Other if approved.fenced() => {
                return Err(Refusal::refused(format!(
                    "`{call_route}` is in no scope of the `{kind}` claim (scope `other`), {FENCED_OTHER}"
                )));
            }
            Scoped::Other => (claim::OTHER_SCOPE.to_string(), true),
            Scoped::Scope(s) => match lfs_upload {
                Some(lfs) => {
                    cx.scope = Some(lfs.upload.clone());
                    (lfs.upload.clone(), false)
                }
                None => (s, false),
            },
        };
        let route_path = match &matched.route {
            Some(claim::Route::Http { path, .. }) => Some(path.as_str()),
            _ => None,
        };
        let mut repo_less = false;
        let mut repo = None;
        if approved.fenced() {
            let mut fence = fence_for(inst, &access, state, sources, &approved, name);
            let outside = |repo: String, named: bool, cx: &mut Context| {
                let write = match call.lfs {
                    Some(op) => op == route::LfsOp::Upload,
                    None => route::is_write(&head.method, path),
                };
                if write {
                    let r = Refusal::refused(format!("`{repo}` is outside the instance's repo fence: no credential writes to it"));
                    return Err(if named { r.todo(repos_todo(&repo, kind)) } else { r });
                }
                cx.outside = Some(repo);
                Ok(Action::Outside { slot: slot.clone(), private: up.private })
            };
            match attribute(&approved, &matched, path) {
                Placed::Unplaced(inputs) => {
                    return Err(Refusal::refused(format!(
                        "`{call_route}` names its repo by {inputs}, which the broker can't resolve yet"
                    )));
                }
                Placed::BadId => {
                    return Err(Refusal::new(400, "refused", format!("`{call_route}` names no repository id")));
                }
                Placed::RepoLess => repo_less = true,
                Placed::Repo(named) => match fence.place_name(&named) {
                    Place::In { current } => {
                        renamed = current.zip(route_path).and_then(|(c, rp)| route::with_repo(rp, path, &c));
                        repo = Some(named);
                    }
                    Place::Out => return outside(named, true, cx),
                },
                Placed::Id(id) => match fence.entry_with_id(id) {
                    Some(entry) => repo = Some(entry),
                    None => {
                        return match fence.name_of(id) {
                            Some(repo) => outside(repo, true, cx),
                            None => outside(format!("repository {id}"), false, cx),
                        };
                    }
                },
            }
        }
        if let (Some(org_fence), Some(rp)) = (&approved.orgs, route_path) {
            let allowed = orgs::allowed(&entry.orgs, &access.orgs);
            match route::var_value(rp, path, &org_fence.var) {
                Some(value) => {
                    let via = orgs::Via { credential: name, upstream, private: up.private, slot: &slot };
                    let id_of = |slug: &str| sources.orgs().and_then(|l| l.id_of(&via, org_fence, slug));
                    check_org(&value, &entry.orgs, &allowed, &id_of, name, kind, &call_route)?;
                }
                None => {
                    org_list = matched
                        .route
                        .as_ref()
                        .and_then(|r| org_fence.lists.get(r))
                        .map(|fields| OrgList { fields: fields.clone(), allowed });
                }
            }
        }
        let found = presence::http(&approved, &head.method, path, call.body).map_err(|u| unnamed(&call_route, u))?;
        presence_needs.extend(found.into_iter().map(|found| PresenceNeed {
            scope: scope.clone(),
            repo: repo.clone(),
            route: call_route.clone(),
            found,
        }));
        vec![Need { scope, repo, repo_less, other }]
    };
    // Every refusal before any ask, so an answer is never followed by one.
    for Need { scope, .. } in needed.iter().filter(|n| !n.other) {
        if entry.max.as_ref().is_some_and(|max| !max.contains(scope)) {
            return Err(Refusal::refused(format!("scope `{scope}` isn't granted by credential `{name}` (its `max`)"))
                .todo(format!("add `{scope}` to `max` of [{name}] in credentials.toml")));
        }
        if !access.scopes.is_empty() && !access.scopes.contains(scope) {
            return Err(Refusal::refused(format!("scope `{scope}` isn't in `access.{kind}.scopes`"))
                .todo(format!("add `{scope}` to `access.{kind}.scopes` in the sandbox's config")));
        }
    }
    let asked = |n: &Need| n.other || access.ask.contains(&n.scope);
    if let Some(Need { scope, .. }) = needed.iter().find(|n| asked(n) && n.repo_less && !n.other) {
        return Err(Refusal::refused(format!(
            "scope `{scope}` is in `access.{kind}.ask`, but `{call_route}` names no repo, so it's never asked"
        )));
    }
    let key_of = |scope: &str, repo: &Option<String>, presence: Option<PresenceAt>| AskKey {
        instance_id: inst.instance_id.clone(),
        credential: name.to_string(),
        scope: scope.to_string(),
        repo: repo.clone(),
        presence,
    };
    let mut keys: Vec<AskKey> = needed.iter().filter(|n| asked(n)).map(|n| key_of(&n.scope, &n.repo, None)).collect();
    // Presence after the scope asks, each target resolved by the home now:
    // one that can't be named is refused, before any ask.
    let api = approved.upstreams.iter().find(|u| u.host == github::API);
    let lookup = sources.github().zip(api).map(|(l, u)| (l, github::Via { credential: name, private: u.private }));
    let own_path = route::path_only(renamed.as_deref().unwrap_or(path));
    for p in presence_needs {
        let target = match &p.found.named {
            presence::Named::Target(s) => Ok(s.clone()),
            presence::Named::Release => {
                lookup.and_then(|(l, via)| l.release_tag(&via, own_path)).ok_or("the release doesn't resolve to its `tag_name`")
            }
            presence::Named::RefNode(id) => {
                lookup.and_then(|(l, via)| l.ref_name(&via, id)).ok_or("the ref doesn't resolve to its name")
            }
        };
        let target = target.map_err(|why| unnamed(&p.route, presence::Unnamed { field: p.found.field, why: why.into() }))?;
        // A ref the claim's `ref` entries don't name for this operation
        // isn't presence: the call is any other of its scope.
        if p.found.ref_op.is_some_and(|op| !presence::ref_asked(&approved, &target, op)) {
            continue;
        }
        let key = key_of(&p.scope, &p.repo, Some(PresenceAt { route: p.route, target }));
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    let mut open = Vec::new();
    for ask_key in keys {
        let kept = sources
            .remembered(&ask_key)
            .map_err(|e| Refusal::new(503, "error", format!("cannot read the kept answers: {e:#}")))?;
        let ruled = || {
            ask_key.presence.is_none()
                && entry.rule_for(&ask_key.scope, ask_key.repo.as_deref(), &inst.sandbox, profile).is_some()
        };
        match kept {
            Some(r) if r.answer == Kind::Deny => return Err(asks::denied(&r.id)),
            Some(_) => {}
            None if ruled() => {}
            None => open.push(ask_key),
        }
    }
    for ask_key in open {
        if !sources.granted(&ask_key) {
            return Ok(Action::Ask(Box::new(Ask {
                key: ask_key,
                instance: key.clone(),
                sandbox: inst.sandbox.clone(),
                kind: kind.to_string(),
                method: head.method.clone(),
                route: route::path_only(path).to_string(),
                hold: approved.hold.map_or(asks::DEFAULT_HOLD, |s| std::time::Duration::from_secs(s.into())),
            })));
        }
    }
    let lfs = match (call.header, &approved.lfs, inst.intercept.as_ref().and_then(|i| i.lfs)) {
        (ProxyHeader::Intercept { host, .. }, Some(lfs), Some(port)) if call.lfs.is_some() && lfs.hosts.contains(host) => {
            Some(port)
        }
        _ => None,
    };
    Ok(Action::Inject(Box::new(Injection {
        credential: name.to_string(),
        slot,
        private: up.private,
        claim: approved,
        lfs,
        expires_at: entry.expires(),
        path: renamed,
        orgs: org_list,
    })))
}

/// A presence target of a call, before the home resolves it.
struct PresenceNeed {
    scope: String,
    repo: Option<String>,
    /// What the key names: `<METHOD> <path>` or `graphql mutation <name>`.
    route: String,
    found: presence::Found,
}

fn unnamed(route: &str, u: presence::Unnamed) -> Refusal {
    Refusal::refused(format!(
        "`{route}` is a presence call (always asked), and the broker can't name its target (`{}`: {})",
        u.field, u.why
    ))
}

/// `value`, the org a route's `orgs.var` took, against the fence.
fn check_org(
    value: &str,
    credential: &[String],
    allowed: &[String],
    id_of: &dyn Fn(&str) -> Option<String>,
    name: &str,
    kind: &str,
    call_route: &str,
) -> Result<(), Refusal> {
    if credential.is_empty() {
        return Err(Refusal::refused(format!("`{call_route}` names an org, and credential `{name}` names no orgs"))
            .todo(format!("add the orgs it may reach to `orgs` of [{name}] in credentials.toml")));
    }
    match orgs::place(value, credential, allowed, id_of) {
        orgs::Place::In => Ok(()),
        orgs::Place::NotInConfig(org) => Err(Refusal::refused(format!("org `{org}` isn't in `access.{kind}.orgs`"))
            .todo(format!("add `{org}` to `access.{kind}.orgs` in the sandbox's config"))),
        orgs::Place::Out if orgs::is_id(value) => Err(Refusal::refused(format!(
            "org id `{value}` is none of the orgs credential `{name}` may reach"
        ))
        .todo(format!("add the org's slug to `orgs` of [{name}] in credentials.toml"))),
        orgs::Place::Out => {
            Err(Refusal::refused(format!("org `{value}` is outside the orgs credential `{name}` may reach"))
                .todo(format!("add `{value}` to `orgs` of [{name}] in credentials.toml")))
        }
    }
}

/// A GraphQL call's scopes: a query's `graphql query` scope (repo-less:
/// never asked), else each top-level mutation field's, once per repo its
/// `fence` inputs name, placed in the fence first (a fenced claim) or
/// refused: a mutation is a write. A route in no scope is `other`,
/// refused for a fenced claim.
fn graphql_scopes(
    claim: &Pinned,
    op: &graphql::Operation,
    mut fence: Option<&mut github::Fence<'_>>,
    kind: &str,
    cx: &mut Context,
    presence_needs: &mut Vec<PresenceNeed>,
) -> Result<Vec<Need>, Refusal> {
    let scope_of = |route: &claim::Route, cx: &mut Context| {
        if claim.never.contains(route) {
            cx.scope = Some(Scoped::Never.name().to_string());
            return Err(Refusal::refused(format!(
                "`{route}` is a `never` route of the `{kind}` claim: no ask or rule opens it"
            )));
        }
        match claim.scopes.iter().find(|(_, s)| s.routes.contains(route)) {
            Some((name, _)) => {
                cx.scope = Some(name.clone());
                Ok((name.clone(), false))
            }
            None if claim.fenced() => {
                cx.scope = Some(claim::OTHER_SCOPE.to_string());
                Err(Refusal::refused(format!(
                    "`{route}` is in no scope of the `{kind}` claim (scope `other`), {FENCED_OTHER}"
                )))
            }
            None => {
                cx.scope = Some(claim::OTHER_SCOPE.to_string());
                Ok((claim::OTHER_SCOPE.to_string(), true))
            }
        }
    };
    let fields = match op {
        graphql::Operation::Query => {
            let (scope, other) = scope_of(&claim::Route::GraphqlQuery, cx)?;
            return Ok(vec![Need { scope, repo: None, repo_less: true, other }]);
        }
        graphql::Operation::Mutation(fields) => fields,
    };
    let mut needed: Vec<Need> = Vec::new();
    for f in fields {
        let route = claim::Route::GraphqlMutation(f.name.clone());
        let (scope, other) = scope_of(&route, cx)?;
        let mut repos: Vec<Option<String>> = Vec::new();
        let mut repo_less = false;
        if let Some(fence) = fence.as_deref_mut() {
            for input in claim.fence.get(&route).into_iter().flatten() {
                let leaves = graphql::inputs(f, input).map_err(|why| Refusal::new(400, "refused", why))?;
                for leaf in leaves {
                    repos.push(Some(place_input(fence, &f.name, input, leaf, kind, cx)?));
                }
            }
            repo_less = repos.is_empty();
        }
        if repos.is_empty() {
            repos.push(None);
        }
        let found = presence::mutation(claim, f).map_err(|u| unnamed(&route.to_string(), u))?;
        for (found, repo) in found.iter().flat_map(|fd| repos.iter().map(move |r| (fd, r))) {
            let route = route.to_string();
            presence_needs.push(PresenceNeed { scope: scope.clone(), repo: repo.clone(), route, found: found.clone() });
        }
        for repo in repos {
            match needed.iter_mut().find(|n| n.scope == scope && n.repo == repo) {
                Some(n) => n.repo_less |= repo_less,
                None => needed.push(Need { scope: scope.clone(), repo, repo_less, other }),
            }
        }
    }
    if needed.is_empty() {
        return Err(Refusal::new(400, "refused", "a GraphQL mutation without a field"));
    }
    let mut scopes: Vec<&str> = Vec::new();
    for n in &needed {
        if !scopes.contains(&n.scope.as_str()) {
            scopes.push(&n.scope);
        }
    }
    cx.scope = Some(scopes.join(","));
    Ok(needed)
}

/// One value of a mutation's `fence` input, inside the fence (the repo it
/// names) or refused: an input named `…NameWithOwner` holds `owner/name`,
/// any other a global id (a repository's matched against the pins, another
/// object's resolved to its repository) or a repository's database id.
fn place_input(
    fence: &mut github::Fence<'_>,
    field: &str,
    input: &str,
    leaf: graphql::Leaf,
    kind: &str,
    cx: &mut Context,
) -> Result<String, Refusal> {
    let at = format!("`{field}`'s `{input}`");
    let outside = |repo: String, named: bool| {
        let r = Refusal::refused(format!("{at} names `{repo}`, outside the instance's repo fence: no credential writes to it"));
        if named { r.todo(repos_todo(&repo, kind)) } else { r }
    };
    let last = input.rsplit('.').next().unwrap_or(input).trim_end_matches("[]");
    let by_name = last.to_ascii_lowercase().ends_with("namewithowner");
    match (leaf, by_name) {
        (graphql::Leaf::Str(s), true) => {
            let Some(repo) = s.split_once('/').and_then(|(o, r)| fence::normalize(o, r)) else {
                return Err(Refusal::new(400, "refused", format!("{at} holds `{s}`, not an `owner/name`")));
            };
            match fence.place_name(&repo) {
                Place::In { current: None } => Ok(repo),
                // The body goes upstream as sent, so the old name can't be
                // swapped for the current one as a REST path is.
                Place::In { current: Some(c) } => {
                    Err(Refusal::refused(format!("{at} names `{repo}`, which is `{c}` now: name it by its current name")))
                }
                Place::Out => Err(outside(repo, true)),
            }
        }
        (graphql::Leaf::Str(id), false) => match fence.place_node(&id) {
            Ok(r) => Ok(r.name()),
            Err(Some(r)) => Err(outside(r.name(), true)),
            Err(None) => {
                cx.scope = Some(claim::OTHER_SCOPE.to_string());
                Err(Refusal::refused(format!(
                    "{at} holds `{id}`, which doesn't resolve to a repository (scope `other`), {FENCED_OTHER}"
                )))
            }
        },
        (graphql::Leaf::Int(id), false) => match fence.entry_with_id(id) {
            Some(entry) => Ok(entry),
            None => Err(match fence.name_of(id) {
                Some(repo) => outside(repo, true),
                None => outside(format!("repository {id}"), false),
            }),
        },
        (graphql::Leaf::Int(id), true) => Err(Refusal::new(400, "refused", format!("{at} holds {id}, not an `owner/name`"))),
    }
}

/// What a matched route of a fenced claim names.
#[derive(Debug, PartialEq, Eq)]
enum Placed {
    Repo(String),
    RepoLess,
    /// A `/repositories/{id}` route's id.
    Id(u64),
    /// A `/repositories/{id}` route whose `{id}` isn't ASCII digits.
    BadId,
    /// The `fence` inputs beyond `{repo}`, for the reason.
    Unplaced(String),
}

/// A route naming its repository by id.
const REPOSITORY_ID: &str = "/repositories/{id}";

fn attribute(claim: &Pinned, matched: &route::Matched, path: &str) -> Placed {
    let inputs: Vec<&String> = matched.route.as_ref().and_then(|r| claim.fence.get(r)).into_iter().flatten().collect();
    let by_id = match &matched.route {
        Some(claim::Route::Http { path: rp, .. }) => {
            let rest = route::path_only(rp).strip_prefix(REPOSITORY_ID);
            rest.is_some_and(|r| r.is_empty() || r.starts_with('/')) && inputs.iter().any(|i| *i == "{id}")
        }
        _ => false,
    };
    let unplaced: Vec<String> = inputs
        .iter()
        .filter(|i| **i != route::REPO && !(by_id && **i == "{id}"))
        .map(|i| format!("`{i}`"))
        .collect();
    if !unplaced.is_empty() {
        return Placed::Unplaced(unplaced.join(", "));
    }
    if by_id {
        let id = route::path_only(path).split('/').nth(2).unwrap_or_default();
        return match id.bytes().all(|b| b.is_ascii_digit()) {
            true => id.parse().map_or(Placed::BadId, Placed::Id),
            false => Placed::BadId,
        };
    }
    match &matched.repo {
        Some(repo) => Placed::Repo(repo.clone()),
        None => Placed::RepoLess,
    }
}

/// The instance's repo fence: its folders' recorded GitHub repos and the
/// config's `access.<kind>.repos`, normalized (an entry naming no repo
/// adds none).
pub fn fence_of(inst: &Instance, access: &Access) -> std::collections::BTreeSet<String> {
    let config = access.repos.iter().filter_map(|r| fence::repo_arg(r).ok());
    inst.github_repos.iter().cloned().chain(config).collect()
}

/// The instance's fence as a [`github::Fence`]: [`fence_of`]'s entries,
/// their pins in `state`, and `sources`' lookup with `credential` when the
/// approved `claim` has `api.github.com` among its upstreams.
pub fn fence_for<'a>(
    inst: &Instance,
    access: &Access,
    state: &State,
    sources: &'a dyn Sources,
    claim: &Pinned,
    credential: &'a str,
) -> github::Fence<'a> {
    let api = claim.upstreams.iter().find(|u| u.host == github::API);
    let lookup = sources
        .github()
        .zip(api)
        .map(|(l, u)| (l, github::Via { credential, private: u.private }));
    github::Fence::new(fence_of(inst, access), &state.github_ids, lookup)
}

pub fn repos_todo(repo: &str, kind: &str) -> String {
    format!("add `{repo}` to `access.{kind}.repos` in the sandbox's config")
}

/// Step 4: the slot holding the claim's phantom, `None` without one. `to`
/// names where the call went, for a leak's reason.
fn phantom_slot(
    head: &RequestHead,
    inst: &Instance,
    own: &InstanceClaim,
    own_claim: &Pinned,
    state: &State,
    to: &str,
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
                format!("the `{}` claim's phantom was sent to {to}", other.kind),
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

/// The request without `slot`: its header, or its query parameter.
pub fn strip(headers: &mut Vec<super::head::Header>, path: &mut String, slot: &Slot) {
    match slot {
        Slot::Header { name, .. } => headers.retain(|h| !h.name.eq_ignore_ascii_case(name)),
        Slot::Basic { .. } => headers.retain(|h| !h.name.eq_ignore_ascii_case("authorization")),
        Slot::Query { param } => {
            let Some((p, q)) = path.split_once('?') else { return };
            let kept: Vec<&str> =
                q.split('&').filter(|pair| pair.split_once('=').map_or(*pair, |(k, _)| k) != param).collect();
            *path = match kept.is_empty() {
                true => p.to_string(),
                false => format!("{p}?{}", kept.join("&")),
            };
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
