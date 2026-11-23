//! A feature's credential claims: `customizations.devsandbox.credentials`
//! in its `devcontainer-feature.json`, an object keyed by credential kind
//! (`sentry`, `github`), the kind a config's `access.<credential>` and the
//! home's credentials name. The contract is
//! docs/plans/active/github-and-sentry-access.md, *The claim contract*.
//! Data only: parsed and validated here, acted on elsewhere.
//!
//! A claim splits in two. [`Pinned`] is what the home approves and binds
//! the secret to; its canonical JSON hashes into [`Claim::version`].
//! [`Wiring`] (`phantom`, `baseUrl`, `ca`, `git`) is what devsandbox sets up
//! in the container, left out of the version: the container could ignore
//! it anyway. Lists whose order means nothing are sorted at parse, so the
//! version changes with the claim's meaning, never with its layout.
//!
//! Strict on purpose (unknown fields refused): a field the home couldn't
//! see in the pinned part would be one no approval covered.

// The broker, `run` and `login` steps read these (credential-broker rollout 1).
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const TOKEN: &str = "{token}";
pub const PHANTOM: &str = "{phantom}";
/// `base-url` only.
pub const BASE_URL: &str = "{baseUrl}";

const MAX_INJECT: usize = 2;
const MAX_HOLD_SECS: u32 = 3600;
const MAX_TEMPLATE: usize = 64 * 1024;
const MAX_KIND: usize = 32;
const MAX_PREFIX: usize = 32;
/// Unknown routes' scope for a credential without a fence; no claim names it.
pub const OTHER_SCOPE: &str = "other";
/// Where a claim's GraphQL routes are called: `POST` with a JSON body.
pub const GRAPHQL_PATH: &str = "/graphql";

const FIELDS: &[&str] = &[
    "upstreams", "mode", "inject", "phantom", "git", "baseUrl", "rewrite", "login", "check",
    "checkShows", "mint", "scopes", "never", "presence", "hold", "refusal", "fence", "ca", "lfs", "tokens",
    "orgs",
];

const MAX_ACCEPT: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub kind: String,
    pub pinned: Pinned,
    pub wiring: Wiring,
}

impl Claim {
    /// Keys in declaration or sorted order, sets sorted.
    pub fn canonical(&self) -> String {
        #[derive(Serialize)]
        struct Canonical<'a> {
            kind: &'a str,
            claim: &'a Pinned,
        }
        serde_json::to_string(&Canonical { kind: &self.kind, claim: &self.pinned })
            .expect("a claim always serializes: every map key is a string")
    }

    /// Not the feature's digest, so a release that leaves the claim alone
    /// keeps it.
    pub fn version(&self) -> String {
        hex::encode(&Sha256::digest(self.canonical().as_bytes())[..])
    }
}

/// The pinned claim back from [`Claim::canonical`]'s JSON (the home's
/// approved copy), with its kind. Trusts the content: the home re-hashed it
/// at load and it was checked when first parsed from a feature.
pub fn from_canonical(canonical: &str) -> Result<(String, Pinned)> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Canonical {
        kind: String,
        claim: Pinned,
    }
    let c: Canonical = serde_json::from_str(canonical).context("an approved claim version doesn't read back")?;
    Ok((c.kind, c.claim))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Pinned {
    /// As declared, not sorted: the first is where `login` checks a token
    /// (and `baseUrl`'s).
    pub upstreams: Vec<Upstream>,
    pub mode: Mode,
    pub inject: Vec<Slot>,
    pub rewrite: Vec<Rewrite>,
    pub login: Vec<Login>,
    pub check: Option<Route>,
    /// A top-level string field of `check`'s response naming whose the
    /// token is (GitHub's `login`): `login` shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_shows: Option<String>,
    pub mint: Option<Mint>,
    pub scopes: BTreeMap<String, Scope>,
    pub never: Vec<Route>,
    pub presence: Vec<Presence>,
    /// `None` takes the broker's default.
    pub hold: Option<u32>,
    pub refusal: Option<Refusal>,
    /// `{var}` from the path, else a body field path. Non-empty: the
    /// credential is repo-fenced ([`Pinned::fenced`]).
    pub fence: BTreeMap<Route, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orgs: Option<Orgs>,
    /// Pinned, not wiring: it sends LFS transfers, with the credentials a
    /// batch response hands out, through the broker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lfs: Option<Lfs>,
    /// Pinned: which secrets the home takes for it and what it may act as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

/// What the credential's secret may be (docs/reference/credentials-cli.md;
/// github-and-sentry-access, *Whose token*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tokens {
    /// The kinds a credential of this claim can be, sorted: a claim
    /// declaring any is one the home's `as` and a config's
    /// `access.<kind>.as` apply to (`credentials::tokens`).
    #[serde(default, rename = "as", skip_serializing_if = "Vec::is_empty")]
    pub as_: Vec<TokenKind>,
    /// Prefixes `login` takes a secret by, sorted; empty: any secret.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept: Vec<String>,
    /// Prefixes `login` takes only for a broad credential (`login --broad`),
    /// sorted, none of `accept`'s: tokens reaching every repo the account
    /// can, more than the claim fences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub broad: Vec<String>,
    /// A response header, lowercased, in which the upstream reports a
    /// pasted token's expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    /// Minted per instance for the app's installation (github-and-sentry-access
    /// step 3).
    App,
    /// The user's own: a pasted token or an OAuth login's.
    User,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::App => "app",
            TokenKind::User => "user",
        }
    }

    pub fn parse(s: &str) -> Result<TokenKind> {
        match s {
            "app" => Ok(TokenKind::App),
            "user" => Ok(TokenKind::User),
            _ => bail!("`{s}` isn't a token kind: `user` or `app`"),
        }
    }
}

/// The org fence: one path variable naming the account's organization
/// (github-and-sentry-access, *Sentry's org fence*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Orgs {
    /// A whole `{name}`, as routes write it.
    pub var: String,
    /// Answers an org's slug with its `id`: an org may be named by either.
    pub resolve: Route,
    /// The top-level field of `resolve`'s answer holding the id.
    pub id: String,
    /// Org-less routes whose JSON array answer lists orgs: each element is
    /// kept by the named top-level fields (sorted) holding a slug or id.
    #[serde(default)]
    pub lists: BTreeMap<Route, Vec<String>>,
}

/// git-lfs through the broker (docs/architecture/credentials/git.md, *LFS*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lfs {
    /// Sorted; each one of `git.hosts`.
    pub hosts: Vec<String>,
    /// The scope an `upload` batch needs, whatever scope the batch route is
    /// in: the route can't tell an upload from a download.
    pub upload: String,
}

impl Pinned {
    /// A claim declaring any `fence` input is fenced: its `{repo}` routes
    /// are checked against the instance's repo fence (credential-broker,
    /// *Policy*).
    pub fn fenced(&self) -> bool {
        !self.fence.is_empty()
    }

    /// Any scope or `never` route is a GraphQL one: the broker reads
    /// `POST /graphql`'s document to decide it (`broker::graphql`).
    pub fn graphql(&self) -> bool {
        self.scopes.values().flat_map(|s| &s.routes).chain(&self.never).any(Route::is_graphql)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Upstream {
    pub host: String,
    /// On the LAN: the broker dials it despite its private address.
    pub private: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    BaseUrl,
    Intercept,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Slot {
    /// `name` lowercased; `format` holds [`TOKEN`] once.
    Header { name: String, format: String },
    /// The token as the password.
    Basic { username: String },
    Query { param: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Rewrite {
    pub route: Route,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(
    tag = "method",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Login {
    Paste {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create: Option<Create>,
    },
    /// No `client_id`: the home names its own OAuth app at login. `scope`:
    /// sent as the request's OAuth `scope`. `broad`: it yields one of
    /// `tokens.broad`, so only `login --broad` uses it.
    Device {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        device_url: String,
        token_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        broad: bool,
    },
    Browser {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        authorize_url: String,
        token_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        broad: bool,
    },
    /// Empty, not unit: see [`Refusal`].
    GithubAppManifest {},
}

impl Login {
    pub fn broad(&self) -> bool {
        matches!(self, Login::Device { broad: true, .. } | Login::Browser { broad: true, .. })
    }
}

/// The page a paste login opens to create the token on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    /// https, no query: login appends its own.
    pub url: String,
    /// Prefill the granted scopes' `permissions` as `<permission>=<level>`
    /// query params (GitHub's fine-grained PAT page reads them).
    #[serde(default)]
    pub permissions: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mint {
    GithubApp,
    OauthRefresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub routes: Vec<Route>,
    pub permissions: BTreeMap<String, Level>,
}

/// Ordered: a higher level includes the lower ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Read,
    Write,
    Admin,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Read => "read",
            Level::Write => "write",
            Level::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Presence {
    /// `{var}` from the route's path, else a body field path.
    Route { route: Route, target: String },
    /// Sorted.
    Ref { pattern: String, on: Vec<RefOp> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefOp {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Refusal {
    /// GitHub's `message`, Sentry's `detail`.
    Json { field: String },
    // Empty struct variants, not unit ones: serde's `deny_unknown_fields`
    // skips unit variants of an internally tagged enum.
    Anthropic {},
    Git {},
    Mcp {},
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Wiring {
    pub phantom: Phantom,
    pub base_url: Option<BaseUrl>,
    pub ca: Option<Ca>,
    pub git: Option<Git>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Phantom {
    /// For a client that reads its auth mode from the key's prefix.
    pub prefix: Option<String>,
    pub env: Vec<String>,
    pub files: Vec<PhantomFile>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhantomFile {
    /// Relative to the remote user's home, no `.` or `..`.
    pub path: String,
    /// Holds [`PHANTOM`]; [`BASE_URL`] too in `base-url` mode.
    pub template: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BaseUrl {
    pub env: String,
    /// The first upstream as written.
    pub upstream: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ca {
    pub env: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Git {
    /// Whose ssh remote forms become https and whose credential helper is
    /// devsbd's.
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
}

const METHODS: &[(Method, &str)] = &[
    (Method::Get, "GET"),
    (Method::Head, "HEAD"),
    (Method::Post, "POST"),
    (Method::Put, "PUT"),
    (Method::Patch, "PATCH"),
    (Method::Delete, "DELETE"),
    (Method::Options, "OPTIONS"),
];

impl Method {
    pub fn as_str(self) -> &'static str {
        METHODS.iter().find(|(m, _)| *m == self).map(|(_, s)| *s).expect("every method is listed")
    }

    pub fn parse(s: &str) -> Option<Method> {
        METHODS.iter().find(|(_, name)| *name == s).map(|(m, _)| *m)
    }
}

/// `METHOD /path/{var}/…[?name=value]`, `graphql query` (every query) or
/// `graphql mutation <name>` (a top-level mutation field). Path segments
/// are literal or a whole `{var}`; `{repo}` takes two, `owner/repo`
/// (`broker::route`); a trailing `/` is part of the route.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Route {
    Http { method: Method, path: String },
    GraphqlQuery,
    GraphqlMutation(String),
}

impl Route {
    pub fn parse(s: &str) -> Result<Route> {
        if s == "graphql query" {
            return Ok(Route::GraphqlQuery);
        }
        if let Some(name) = s.strip_prefix("graphql mutation ") {
            if !is_ident(name, false) {
                bail!("`{s}`: `{name}` isn't a GraphQL name");
            }
            return Ok(Route::GraphqlMutation(name.to_string()));
        }
        if s.starts_with("graphql ") {
            bail!("`{s}`: write `graphql query` or `graphql mutation <name>`");
        }
        let Some((m, path)) = s.split_once(' ') else {
            bail!("`{s}`: write `METHOD /path` or `graphql mutation <name>`");
        };
        let Some(method) = Method::parse(m) else {
            let all: Vec<&str> = METHODS.iter().map(|(_, s)| *s).collect();
            bail!("`{s}`: `{m}` isn't one of {}", all.join(", "));
        };
        check_path(path).with_context(|| format!("`{s}`"))?;
        Ok(Route::Http { method, path: path.to_string() })
    }

    pub fn vars(&self) -> Vec<&str> {
        match self {
            Route::Http { path, .. } => {
                path.split_once('?').map_or(path.as_str(), |(p, _)| p).split('/').filter_map(path_var).collect()
            }
            Route::GraphqlQuery | Route::GraphqlMutation(_) => Vec::new(),
        }
    }

    pub fn is_graphql(&self) -> bool {
        !matches!(self, Route::Http { .. })
    }
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Route::Http { method, path } => write!(f, "{} {path}", method.as_str()),
            Route::GraphqlQuery => write!(f, "graphql query"),
            Route::GraphqlMutation(name) => write!(f, "graphql mutation {name}"),
        }
    }
}

impl Serialize for Route {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Route {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Route, D::Error> {
        let s = String::deserialize(d)?;
        Route::parse(&s).map_err(|e| serde::de::Error::custom(format!("{e:#}")))
    }
}

/// `feature` names it in errors.
pub fn from_customizations(feature: &str, customizations: Option<&Value>) -> Result<Vec<Claim>> {
    match customizations.and_then(|c| c.pointer("/devsandbox/credentials")) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(credentials) => parse_all(feature, credentials),
    }
}

fn parse_all(feature: &str, credentials: &Value) -> Result<Vec<Claim>> {
    let Some(map) = credentials.as_object() else {
        bail!(
            "feature `{feature}`: `customizations.devsandbox.credentials` must be an object keyed by credential kind"
        );
    };
    map.iter()
        .map(|(kind, claim)| {
            parse(kind, claim).with_context(|| format!("feature `{feature}`: credential `{kind}`"))
        })
        .collect()
}

pub fn check_kind(kind: &str) -> Result<()> {
    if kind.is_empty()
        || kind.len() > MAX_KIND
        || !kind.starts_with(|c: char| c.is_ascii_lowercase())
        || !kind.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("the kind must be lowercase letters, digits and `-`, starting with a letter");
    }
    Ok(())
}

pub fn parse(kind: &str, value: &Value) -> Result<Claim> {
    check_kind(kind)?;
    let Some(obj) = value.as_object() else {
        bail!("must be an object");
    };
    if let Some(unknown) = obj.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        bail!("unknown field `{unknown}`");
    }

    let mode: Mode = required(obj, "mode")?;
    let raw_upstreams: Vec<RawUpstream> = required(obj, "upstreams")?;
    let upstreams = upstreams(raw_upstreams)?;
    let first_upstream = upstreams[0].host.clone();
    let hosts: BTreeSet<&str> = upstreams.iter().map(|u| u.host.as_str()).collect();

    let inject = inject(field(obj, "inject")?.unwrap_or_default())?;

    let mut env_names = BTreeSet::new();
    let base_url = base_url(field(obj, "baseUrl")?, mode, first_upstream, &mut env_names)?;
    let phantom = phantom(field(obj, "phantom")?, mode, &inject, &mut env_names)?;
    let ca = ca(field(obj, "ca")?, mode, &mut env_names)?;
    let git = git(field(obj, "git")?, mode, &hosts)?;

    let rewrite = rewrite(field(obj, "rewrite")?.unwrap_or_default(), mode)?;
    let tokens = tokens(field(obj, "tokens")?, &inject)?;
    let login = login(field(obj, "login")?, &inject, tokens.as_ref())?;
    let check = match field::<String>(obj, "check")? {
        None => None,
        Some(raw) => Some(check_route(&raw).context("`check`")?),
    };
    let check_shows: Option<String> = field(obj, "checkShows")?;
    if let Some(name) = &check_shows {
        if check.is_none() {
            bail!("`checkShows` reads `check`'s response: the claim has no `check`");
        }
        if !is_ident(name, true) {
            bail!("`checkShows`: `{name}` isn't a field name (letters, digits, `_` and `-`)");
        }
    }
    let mint: Option<Mint> = field(obj, "mint")?;
    let scopes = scopes(field(obj, "scopes")?.unwrap_or_default())?;
    let never = never(field(obj, "never")?.unwrap_or_default(), &scopes)?;
    check_graphql_path(&scopes, &never)?;
    let presence = presence(field(obj, "presence")?.unwrap_or_default())?;
    let hold: Option<u32> = field(obj, "hold")?;
    if let Some(secs) = hold
        && !(1..=MAX_HOLD_SECS).contains(&secs)
    {
        bail!("`hold`: {secs} isn't between 1 and {MAX_HOLD_SECS} seconds");
    }
    let refusal: Option<Refusal> = field(obj, "refusal")?;
    if let Some(Refusal::Json { field }) = &refusal {
        check_field_path(field).context("`refusal.field`")?;
    }
    let fence = fence(field(obj, "fence")?.unwrap_or_default())?;
    let lfs = lfs(field(obj, "lfs")?, mode, git.as_ref(), &scopes)?;
    let orgs = orgs(field(obj, "orgs")?, &scopes)?;

    Ok(Claim {
        kind: kind.to_string(),
        pinned: Pinned {
            upstreams,
            mode,
            inject,
            rewrite,
            login,
            check,
            check_shows,
            mint,
            scopes,
            never,
            presence,
            hold,
            refusal,
            fence,
            orgs,
            lfs,
            tokens,
        },
        wiring: Wiring { phantom, base_url, ca, git },
    })
}

fn field<T: DeserializeOwned>(obj: &Map<String, Value>, name: &str) -> Result<Option<T>> {
    obj.get(name)
        .map(|v| serde_json::from_value(v.clone()).with_context(|| format!("`{name}`")))
        .transpose()
}

fn required<T: DeserializeOwned>(obj: &Map<String, Value>, name: &str) -> Result<T> {
    field(obj, name)?.with_context(|| format!("`{name}` is required"))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawUpstream {
    Host(String),
    Entry(RawUpstreamEntry),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUpstreamEntry {
    host: String,
    #[serde(default)]
    private: bool,
}

/// The first one is `baseUrl`'s.
fn upstreams(raw: Vec<RawUpstream>) -> Result<Vec<Upstream>> {
    if raw.is_empty() {
        bail!("`upstreams` must name at least one host");
    }
    let mut out: Vec<Upstream> = Vec::new();
    for (i, entry) in raw.into_iter().enumerate() {
        let (host, private) = match entry {
            RawUpstream::Host(host) => (host, false),
            RawUpstream::Entry(e) => (e.host, e.private),
        };
        check_host(&host).with_context(|| format!("`upstreams[{i}]`"))?;
        if out.iter().any(|u| u.host == host) {
            bail!("`upstreams[{i}]`: `{host}` is listed twice");
        }
        out.push(Upstream { host, private });
    }
    if out.iter().filter(|u| u.private).count() > 1 {
        bail!("`upstreams`: at most one upstream is `private`");
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlot {
    header: Option<String>,
    format: Option<String>,
    basic: Option<String>,
    query: Option<String>,
}

const RESERVED_HEADERS: &[&str] = &["host", "content-length", "transfer-encoding", "connection"];

fn inject(raw: Vec<RawSlot>) -> Result<Vec<Slot>> {
    if raw.len() > MAX_INJECT {
        bail!("`inject`: at most {MAX_INJECT} slots");
    }
    let mut out = Vec::new();
    for (i, slot) in raw.into_iter().enumerate() {
        let slot = one_slot(slot).with_context(|| format!("`inject[{i}]`"))?;
        if out.contains(&slot) {
            bail!("`inject[{i}]`: the same slot twice");
        }
        out.push(slot);
    }
    out.sort();
    Ok(out)
}

fn one_slot(raw: RawSlot) -> Result<Slot> {
    match raw {
        RawSlot { header: Some(name), format: Some(format), basic: None, query: None } => {
            if !is_http_token(&name) {
                bail!("`{name}` isn't a header name");
            }
            let name = name.to_ascii_lowercase();
            if RESERVED_HEADERS.contains(&name.as_str()) {
                bail!("`{name}` can't take a token");
            }
            if format.matches(TOKEN).count() != 1 {
                bail!("`format` must hold `{TOKEN}` once, e.g. `Bearer {TOKEN}`");
            }
            if format.chars().any(char::is_control) {
                bail!("`format` holds a control character");
            }
            Ok(Slot::Header { name, format })
        }
        RawSlot { header: Some(_), format: None, basic: None, query: None } => {
            bail!("a `header` slot needs a `format`, e.g. `Bearer {TOKEN}`")
        }
        RawSlot { header: None, format: Some(_), .. } => bail!("`format` goes with `header`"),
        RawSlot { header: None, format: None, basic: Some(username), query: None } => {
            if username.is_empty() || username.contains(':') || username.chars().any(char::is_control) {
                bail!("`basic` takes the username: non-empty, no `:` or control characters");
            }
            Ok(Slot::Basic { username })
        }
        RawSlot { header: None, format: None, basic: None, query: Some(param) } => {
            if param.is_empty() || !param.bytes().all(is_unreserved) {
                bail!("`query`: `{param}` isn't a plain query parameter name");
            }
            Ok(Slot::Query { param })
        }
        _ => bail!("name exactly one of `header` (with `format`), `basic` or `query`"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPhantom {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    files: Vec<RawPhantomFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPhantomFile {
    path: String,
    template: String,
}

fn phantom(
    raw: Option<RawPhantom>,
    mode: Mode,
    inject: &[Slot],
    env_names: &mut BTreeSet<String>,
) -> Result<Phantom> {
    let raw = match (raw, inject.is_empty()) {
        (None, true) => return Ok(Phantom::default()),
        (Some(_), true) => bail!("`phantom` without an `inject` slot: nothing would replace it"),
        (raw, false) => raw.unwrap_or(RawPhantom { prefix: None, env: Vec::new(), files: Vec::new() }),
    };
    if raw.env.is_empty() && raw.files.is_empty() {
        bail!("`phantom` must name an env var or a file: every injected call carries the phantom");
    }
    if let Some(prefix) = &raw.prefix
        && (prefix.is_empty()
            || prefix.len() > MAX_PREFIX
            || !prefix.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    {
        bail!("`phantom.prefix`: up to {MAX_PREFIX} letters, digits, `_` and `-`");
    }
    for (i, name) in raw.env.iter().enumerate() {
        add_env(name, env_names).with_context(|| format!("`phantom.env[{i}]`"))?;
    }
    let mut files: Vec<PhantomFile> = Vec::new();
    for (i, file) in raw.files.into_iter().enumerate() {
        let at = || format!("`phantom.files[{i}]`");
        check_home_path(&file.path).with_context(at)?;
        if files.iter().any(|f| f.path == file.path) {
            bail!("{}: `{}` is listed twice", at(), file.path);
        }
        if file.template.len() > MAX_TEMPLATE {
            bail!("{}: the template is over {MAX_TEMPLATE} bytes", at());
        }
        if !file.template.contains(PHANTOM) {
            bail!("{}: the template must hold `{PHANTOM}`", at());
        }
        if mode == Mode::Intercept && file.template.contains(BASE_URL) {
            bail!("{}: `{BASE_URL}` is only for `base-url` mode", at());
        }
        files.push(PhantomFile { path: file.path, template: file.template });
    }
    Ok(Phantom { prefix: raw.prefix, env: raw.env, files })
}

fn base_url(
    raw: Option<String>,
    mode: Mode,
    first_upstream: String,
    env_names: &mut BTreeSet<String>,
) -> Result<Option<BaseUrl>> {
    match (mode, raw) {
        (Mode::BaseUrl, None) => bail!("`base-url` mode needs `baseUrl`, the env var that takes the loopback URL"),
        (Mode::Intercept, Some(_)) => bail!("`baseUrl` is only for `base-url` mode"),
        (Mode::Intercept, None) => Ok(None),
        (Mode::BaseUrl, Some(env)) => {
            add_env(&env, env_names).context("`baseUrl`")?;
            Ok(Some(BaseUrl { env, upstream: first_upstream }))
        }
    }
}

fn ca(raw: Option<Ca>, mode: Mode, env_names: &mut BTreeSet<String>) -> Result<Option<Ca>> {
    let Some(ca) = raw else { return Ok(None) };
    if mode != Mode::Intercept {
        bail!("`ca` is only for `intercept` mode: `base-url` is plain HTTP");
    }
    add_env(&ca.env, env_names).context("`ca.env`")?;
    Ok(Some(ca))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGit {
    hosts: Vec<String>,
}

fn git(raw: Option<RawGit>, mode: Mode, upstreams: &BTreeSet<&str>) -> Result<Option<Git>> {
    let Some(raw) = raw else { return Ok(None) };
    if mode != Mode::Intercept {
        bail!("`git` is only for `intercept` mode");
    }
    if raw.hosts.is_empty() {
        bail!("`git.hosts` must name at least one host");
    }
    for (i, host) in raw.hosts.iter().enumerate() {
        if !upstreams.contains(host.as_str()) {
            bail!("`git.hosts[{i}]`: `{host}` isn't one of the claim's `upstreams`");
        }
        if raw.hosts[..i].contains(host) {
            bail!("`git.hosts[{i}]`: `{host}` is listed twice");
        }
    }
    Ok(Some(Git { hosts: raw.hosts }))
}

fn lfs(raw: Option<Lfs>, mode: Mode, git: Option<&Git>, scopes: &BTreeMap<String, Scope>) -> Result<Option<Lfs>> {
    let Some(mut raw) = raw else { return Ok(None) };
    if mode != Mode::Intercept {
        bail!("`lfs` is only for `intercept` mode");
    }
    let Some(git) = git else {
        bail!("`lfs` needs `git`: LFS batches go to the git hosts");
    };
    if raw.hosts.is_empty() {
        bail!("`lfs.hosts` must name at least one host");
    }
    for (i, host) in raw.hosts.iter().enumerate() {
        if !git.hosts.contains(host) {
            bail!("`lfs.hosts[{i}]`: `{host}` isn't one of `git.hosts`");
        }
        if raw.hosts[..i].contains(host) {
            bail!("`lfs.hosts[{i}]`: `{host}` is listed twice");
        }
    }
    if !scopes.contains_key(&raw.upload) {
        bail!("`lfs.upload`: `{}` isn't one of the claim's `scopes`", raw.upload);
    }
    raw.hosts.sort();
    Ok(Some(raw))
}

fn orgs(raw: Option<Orgs>, scopes: &BTreeMap<String, Scope>) -> Result<Option<Orgs>> {
    let Some(mut raw) = raw else { return Ok(None) };
    let var = match path_var(&raw.var) {
        Some(name) if is_ident(name, false) => name.to_string(),
        _ => bail!("`orgs.var`: `{}` isn't a whole `{{name}}` variable", raw.var),
    };
    if var == "repo" {
        bail!("`orgs.var`: `{{repo}}` is the repo fence's");
    }
    let holds = |r: &Route| r.vars().contains(&var.as_str());
    if !scopes.values().flat_map(|s| &s.routes).any(holds) {
        bail!("`orgs.var`: no scoped route holds `{}`", raw.var);
    }
    if !matches!(raw.resolve, Route::Http { method: Method::Get, .. }) {
        bail!("`orgs.resolve`: `{}` isn't a `GET` route: its answer names the id", raw.resolve);
    }
    if !holds(&raw.resolve) {
        bail!("`orgs.resolve`: `{}` has no `{}`", raw.resolve, raw.var);
    }
    if !is_ident(&raw.id, true) {
        bail!("`orgs.id`: `{}` isn't a field name (letters, digits, `_` and `-`)", raw.id);
    }
    for (route, fields) in &mut raw.lists {
        let at = || format!("`orgs.lists.{route}`");
        if route.is_graphql() {
            bail!("{}: lists are HTTP routes", at());
        }
        if !scopes.values().any(|s| s.routes.contains(route)) {
            bail!("{}: not in any of the claim's `scopes`", at());
        }
        if holds(route) {
            bail!("{}: it holds `{}`, so the fence checks it already", at(), raw.var);
        }
        if fields.is_empty() {
            bail!("{}: name at least one field", at());
        }
        for (i, f) in fields.iter().enumerate() {
            if !is_ident(f, true) {
                bail!("{}: `{f}` isn't a field name (letters, digits, `_` and `-`)", at());
            }
            if fields[..i].contains(f) {
                bail!("{}: `{f}` is listed twice", at());
            }
        }
        fields.sort();
    }
    Ok(Some(raw))
}

fn tokens(raw: Option<Tokens>, inject: &[Slot]) -> Result<Option<Tokens>> {
    let Some(mut raw) = raw else { return Ok(None) };
    if inject.is_empty() {
        bail!("`tokens` without an `inject` slot: there is no secret");
    }
    if raw.as_.is_empty() && raw.accept.is_empty() && raw.broad.is_empty() && raw.expiry.is_none() {
        bail!("`tokens` names nothing: `as`, `accept`, `broad` or `expiry`");
    }
    raw.as_.sort();
    if raw.as_.windows(2).any(|w| w[0] == w[1]) {
        bail!("`tokens.as`: a kind is listed twice");
    }
    for (name, list) in [("accept", &mut raw.accept), ("broad", &mut raw.broad)] {
        if list.len() > MAX_ACCEPT {
            bail!("`tokens.{name}`: at most {MAX_ACCEPT} prefixes");
        }
        for (i, p) in list.iter().enumerate() {
            if p.is_empty() || p.len() > MAX_PREFIX || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                bail!("`tokens.{name}[{i}]`: up to {MAX_PREFIX} letters, digits, `_` and `-`");
            }
            if list[..i].contains(p) {
                bail!("`tokens.{name}[{i}]`: `{p}` is listed twice");
            }
        }
        list.sort();
    }
    // Disjoint as prefixes, not only as strings: else one secret would be
    // both accepted and broad.
    for b in &raw.broad {
        if let Some(a) = raw.accept.iter().find(|a| a.starts_with(b.as_str()) || b.starts_with(a.as_str())) {
            bail!("`tokens.broad`: `{b}` overlaps `tokens.accept`'s `{a}`: a prefix is accepted or broad, not both");
        }
    }
    if let Some(h) = &mut raw.expiry {
        if !is_http_token(h) {
            bail!("`tokens.expiry`: `{h}` isn't a header name");
        }
        *h = h.to_ascii_lowercase();
    }
    Ok(Some(raw))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRewrite {
    route: String,
    fields: Vec<String>,
}

fn rewrite(raw: Vec<RawRewrite>, mode: Mode) -> Result<Vec<Rewrite>> {
    if !raw.is_empty() && mode != Mode::BaseUrl {
        bail!("`rewrite` is only for `base-url` mode");
    }
    let mut out: Vec<Rewrite> = Vec::new();
    for (i, entry) in raw.into_iter().enumerate() {
        let at = || format!("`rewrite[{i}]`");
        let route = Route::parse(&entry.route).with_context(at)?;
        if route.is_graphql() {
            bail!("{}: rewrites apply to HTTP routes", at());
        }
        if out.iter().any(|r| r.route == route) {
            bail!("{}: `{route}` is listed twice", at());
        }
        if entry.fields.is_empty() {
            bail!("{}: `fields` must name at least one field", at());
        }
        let mut fields = BTreeSet::new();
        for f in entry.fields {
            check_field_path(&f).with_context(at)?;
            if !fields.insert(f.clone()) {
                bail!("{}: `{f}` is listed twice", at());
            }
        }
        out.push(Rewrite { route, fields: fields.into_iter().collect() });
    }
    out.sort();
    Ok(out)
}

fn login(raw: Option<Vec<Login>>, inject: &[Slot], tokens: Option<&Tokens>) -> Result<Vec<Login>> {
    let mut raw = match (raw, inject.is_empty()) {
        (None, true) => return Ok(Vec::new()),
        (Some(_), true) => bail!("`login` without an `inject` slot: there is no secret to get"),
        (None, false) => bail!("`login` is required: how `devsandbox login` gets the secret"),
        (Some(raw), false) => raw,
    };
    if raw.is_empty() {
        bail!("`login` must name at least one method");
    }
    for (i, l) in raw.iter().enumerate() {
        let at = || format!("`login[{i}]`");
        // A method twice only as a broad and a narrow one: `--broad` picks
        // between them.
        if raw[..i].iter().any(|o| std::mem::discriminant(o) == std::mem::discriminant(l) && o.broad() == l.broad()) {
            bail!("{}: the same method twice", at());
        }
        if l.broad() && tokens.is_none_or(|t| t.broad.is_empty()) {
            bail!("{}: a `broad` login needs `tokens.broad`: the prefixes of the tokens it yields", at());
        }
        match l {
            Login::Paste { hint, create } => {
                if hint.as_deref().is_some_and(str::is_empty) {
                    bail!("{}: `hint` is empty", at());
                }
                if let Some(c) = create {
                    check_https_url(&c.url).with_context(|| format!("{}: `create.url`", at()))?;
                    if c.url.contains(['?', '#']) {
                        bail!("{}: `create.url` `{}` has a query or fragment: login adds its own", at(), c.url);
                    }
                }
            }
            Login::Device { client_id, device_url, token_url, scope, .. } => {
                client_id.as_deref().map(check_client_id).transpose().with_context(at)?;
                scope.as_deref().map(check_scope).transpose().with_context(at)?;
                check_https_url(device_url).with_context(|| format!("{}: `deviceUrl`", at()))?;
                check_https_url(token_url).with_context(|| format!("{}: `tokenUrl`", at()))?;
            }
            Login::Browser { client_id, authorize_url, token_url, scope, .. } => {
                client_id.as_deref().map(check_client_id).transpose().with_context(at)?;
                scope.as_deref().map(check_scope).transpose().with_context(at)?;
                check_https_url(authorize_url).with_context(|| format!("{}: `authorizeUrl`", at()))?;
                check_https_url(token_url).with_context(|| format!("{}: `tokenUrl`", at()))?;
            }
            Login::GithubAppManifest {} => {}
        }
    }
    raw.sort();
    Ok(raw)
}

fn check_route(raw: &str) -> Result<Route> {
    let route = Route::parse(raw)?;
    match &route {
        Route::Http { method: Method::Get | Method::Head, .. } if route.vars().is_empty() => Ok(route),
        _ => bail!("`{route}`: the broker calls it with no sandbox input: a `GET` or `HEAD` with no `{{var}}`"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScope {
    routes: Vec<String>,
    #[serde(default)]
    permissions: BTreeMap<String, Level>,
}

/// A scope is its routes (`"pr.write": ["POST /repos/{repo}/pulls", …]`)
/// or `{routes, permissions}`.
fn scopes(raw: BTreeMap<String, Value>) -> Result<BTreeMap<String, Scope>> {
    let mut out = BTreeMap::new();
    let mut seen: BTreeMap<Route, String> = BTreeMap::new();
    for (name, value) in raw {
        let at = format!("`scopes.{name}`");
        if !is_scope_name(&name) {
            bail!("{at}: scope names are lowercase dot-separated words (`pr.write`)");
        }
        if name == OTHER_SCOPE {
            bail!("{at}: `{OTHER_SCOPE}` is the scope of unknown routes; no claim names it");
        }
        let RawScope { routes, permissions } = match value {
            Value::Array(_) => RawScope {
                routes: serde_json::from_value(value).with_context(|| at.clone())?,
                permissions: BTreeMap::new(),
            },
            other => serde_json::from_value(other).with_context(|| at.clone())?,
        };
        if routes.is_empty() {
            bail!("{at}: a scope needs at least one route");
        }
        let mut parsed = Vec::new();
        for raw in &routes {
            let route = Route::parse(raw).with_context(|| at.clone())?;
            if let Some(other) = seen.insert(route.clone(), name.clone()) {
                if other == name {
                    bail!("{at}: `{route}` is listed twice");
                }
                bail!("{at}: `{route}` is in scope `{other}` too: a route has one scope");
            }
            parsed.push(route);
        }
        for key in permissions.keys() {
            if !key.starts_with(|c: char| c.is_ascii_lowercase())
                || !key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_:-".contains(&b))
            {
                bail!("{at}: `{key}` isn't a permission name");
            }
        }
        parsed.sort();
        out.insert(name, Scope { routes: parsed, permissions });
    }
    Ok(out)
}

/// A claim with GraphQL routes has `POST /graphql` decided by its document,
/// so an HTTP route on that path could only mislead.
fn check_graphql_path(scopes: &BTreeMap<String, Scope>, never: &[Route]) -> Result<()> {
    let routes = || scopes.values().flat_map(|s| &s.routes).chain(never);
    if !routes().any(Route::is_graphql) {
        return Ok(());
    }
    match routes().find(|r| matches!(r, Route::Http { path, .. } if path.split('?').next() == Some(GRAPHQL_PATH))) {
        Some(r) => bail!("`{r}`: a claim with GraphQL routes decides `{GRAPHQL_PATH}` by its document"),
        None => Ok(()),
    }
}

/// A route is scoped or `never`, not both: no ask or rule opens a `never`
/// route, so a scope naming one could only mislead.
fn never(raw: Vec<String>, scopes: &BTreeMap<String, Scope>) -> Result<Vec<Route>> {
    let mut out = Vec::new();
    for (i, raw) in raw.iter().enumerate() {
        let at = || format!("`never[{i}]`");
        let route = Route::parse(raw).with_context(at)?;
        if out.contains(&route) {
            bail!("{}: `{route}` is listed twice", at());
        }
        if let Some((name, _)) = scopes.iter().find(|(_, s)| s.routes.contains(&route)) {
            bail!("{}: `{route}` is in scope `{name}` too: a route is scoped or `never`, not both", at());
        }
        out.push(route);
    }
    out.sort();
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPresence {
    route: Option<String>,
    target: Option<String>,
    #[serde(rename = "ref")]
    pattern: Option<String>,
    on: Option<Vec<RefOp>>,
}

fn presence(raw: Vec<RawPresence>) -> Result<Vec<Presence>> {
    let mut out = Vec::new();
    for (i, entry) in raw.into_iter().enumerate() {
        let at = || format!("`presence[{i}]`");
        let p = match entry {
            RawPresence { route: Some(route), target: Some(target), pattern: None, on: None } => {
                let route = Route::parse(&route).with_context(at)?;
                check_input(&route, &target).with_context(|| format!("{}: `target`", at()))?;
                Presence::Route { route, target }
            }
            RawPresence { route: Some(_), target: None, .. } => {
                bail!("{}: a `route` needs the `target` field naming what it acts on", at())
            }
            RawPresence { route: None, target: None, pattern: Some(pattern), on } => {
                check_ref_pattern(&pattern).with_context(at)?;
                let on: BTreeSet<RefOp> = match on {
                    None => [RefOp::Create, RefOp::Update, RefOp::Delete].into(),
                    Some(on) if on.is_empty() => bail!("{}: `on` is empty", at()),
                    Some(on) => on.into_iter().collect(),
                };
                Presence::Ref { pattern, on: on.into_iter().collect() }
            }
            _ => bail!("{}: name a `route` with its `target`, or a `ref` (with `on`)", at()),
        };
        if out.contains(&p) {
            bail!("{}: listed twice", at());
        }
        out.push(p);
    }
    out.sort();
    Ok(out)
}

fn fence(raw: BTreeMap<String, Vec<String>>) -> Result<BTreeMap<Route, Vec<String>>> {
    let mut out = BTreeMap::new();
    for (raw_route, inputs) in raw {
        let at = || format!("`fence.{raw_route}`");
        let route = Route::parse(&raw_route).with_context(at)?;
        if route == Route::GraphqlQuery {
            bail!("{}: queries pass under the token's own reach, never fenced", at());
        }
        if inputs.is_empty() {
            bail!("{}: name at least one input", at());
        }
        let mut set = BTreeSet::new();
        for input in inputs {
            check_input(&route, &input).with_context(at)?;
            if !set.insert(input.clone()) {
                bail!("{}: `{input}` is listed twice", at());
            }
        }
        if out.insert(route.clone(), set.into_iter().collect()).is_some() {
            bail!("{}: `{route}` is listed twice", at());
        }
    }
    Ok(out)
}

fn check_input(route: &Route, input: &str) -> Result<()> {
    if let Some(var) = path_var(input) {
        if !route.vars().contains(&var) {
            bail!("`{input}`: `{route}` has no `{{{var}}}`");
        }
        return Ok(());
    }
    check_field_path(input)
}

fn add_env(name: &str, seen: &mut BTreeSet<String>) -> Result<()> {
    if !is_ident(name, false) {
        bail!("`{name}` isn't an env var name");
    }
    if !seen.insert(name.to_string()) {
        bail!("`{name}` is already set by this claim");
    }
    Ok(())
}

fn check_host(h: &str) -> Result<()> {
    if h.contains(['/', ':', '@', '?', '#']) {
        bail!("`{h}` isn't a hostname: write the bare host, without scheme, port or path");
    }
    if h.ends_with('.') {
        bail!("`{h}`: drop the trailing dot");
    }
    if h.bytes().any(|b| b.is_ascii_uppercase()) {
        bail!("`{h}`: write hosts in lowercase");
    }
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if h.is_empty() || h.len() > 253 || !h.split('.').all(label_ok) {
        bail!("`{h}` isn't a hostname");
    }
    Ok(())
}

fn check_https_url(url: &str) -> Result<()> {
    let Some(rest) = url.strip_prefix("https://") else {
        bail!("`{url}` isn't an https URL");
    };
    let host = rest.split('/').next().unwrap_or_default();
    check_host(host)?;
    if rest.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("`{url}` holds whitespace or a control character");
    }
    Ok(())
}

fn check_client_id(id: &str) -> Result<()> {
    if id.is_empty() || id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("`clientId` must be non-empty, no whitespace");
    }
    Ok(())
}

/// RFC 6749 §3.3: space-separated tokens of printable ASCII, no `"` or `\`.
fn check_scope(scope: &str) -> Result<()> {
    let token = |t: &str| !t.is_empty() && t.bytes().all(|b| matches!(b, 0x21 | 0x23..=0x5b | 0x5d..=0x7e));
    if !scope.split(' ').all(token) {
        bail!("`scope` `{}`: OAuth scopes separated by single spaces", scope.escape_debug());
    }
    Ok(())
}

fn check_home_path(path: &str) -> Result<()> {
    if path.starts_with('~') {
        bail!("`{path}`: write it relative to the home, without `~/`");
    }
    if path.starts_with('/') {
        bail!("`{path}`: must be relative to the remote user's home, not absolute");
    }
    if path.is_empty()
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.split('/').any(|c| c.is_empty() || c == "." || c == "..")
    {
        bail!("`{path}`: must be a relative path under the home, without `.`, `..` or empty parts");
    }
    Ok(())
}

/// A route's path, then at most one `?name=value` the request's query must
/// hold exactly once ([`query_constraint`]).
fn check_path(path: &str) -> Result<()> {
    let (path, query) = path.split_once('?').map_or((path, None), |(p, q)| (p, Some(q)));
    let Some(rest) = path.strip_prefix('/') else {
        bail!("the path must start with `/`");
    };
    if let Some(q) = query {
        let ok = q.split_once('=').is_some_and(|(k, v)| {
            !k.is_empty() && !v.is_empty() && k.bytes().all(is_unreserved) && v.bytes().all(is_unreserved)
        });
        if !ok {
            bail!("`?{q}`: a route's query is one `?name=value`, unreserved characters only");
        }
    }
    if path.contains('#') {
        bail!("no fragment in a route");
    }
    let segments: Vec<&str> = rest.split('/').collect();
    let mut vars = BTreeSet::new();
    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            if i + 1 == segments.len() {
                continue;
            }
            bail!("an empty segment (`//`)");
        }
        if let Some(var) = path_var(seg) {
            if !is_ident(var, false) {
                bail!("`{seg}`: a variable is `{{name}}`, letters, digits and `_`");
            }
            if !vars.insert(var) {
                bail!("`{{{var}}}` twice");
            }
            continue;
        }
        if *seg == "." || *seg == ".." {
            bail!("a dot-segment (`{seg}`)");
        }
        if !seg.bytes().all(|b| is_unreserved(b) || b"!$&'()*+,;=:@".contains(&b)) {
            bail!("`{seg}`: a segment is literal (no `%`, `{{` or `}}` but a whole `{{var}}`)");
        }
    }
    Ok(())
}

fn check_field_path(p: &str) -> Result<()> {
    if p.is_empty() || !p.split('.').all(|seg| is_ident(seg.strip_suffix("[]").unwrap_or(seg), true)) {
        bail!("`{p}` isn't a field path (`name`, `a.b`, `list[].name`)");
    }
    Ok(())
}

fn check_ref_pattern(p: &str) -> Result<()> {
    let Some(rest) = p.strip_prefix("refs/") else {
        bail!("`{p}`: a ref pattern starts with `refs/`");
    };
    let segments: Vec<&str> = rest.split('/').collect();
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        let ok = if *seg == "*" {
            last
        } else {
            !seg.is_empty()
                && *seg != "."
                && *seg != ".."
                && !seg.chars().any(|c| c.is_control() || c.is_whitespace() || "~^:?[\\*".contains(c))
        };
        if !ok {
            bail!("`{p}` isn't a ref pattern (`refs/tags/*`, `refs/heads/main`; `*` only as the last part)");
        }
    }
    Ok(())
}

/// Whether full ref `name` is one [`check_ref_pattern`]'s `pattern` names:
/// equal, or under a trailing `*`'s prefix by one or more segments
/// (`refs/heads/*` holds `refs/heads/a/x`), as git's refspec globs read.
pub fn ref_pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/')),
        None => pattern == name,
    }
}

fn path_var(seg: &str) -> Option<&str> {
    seg.strip_prefix('{')?.strip_suffix('}')
}

fn is_ident(s: &str, dash: bool) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || (dash && b == b'-'))
}

fn is_scope_name(s: &str) -> bool {
    s.split('.').all(|w| {
        w.starts_with(|c: char| c.is_ascii_lowercase())
            && w.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    })
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~".contains(&b)
}

fn is_http_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SENTRY: &str = include_str!("../../tests/fixtures/features/sentry/devcontainer-feature.json");
    const SENTRY_CLI: &str = include_str!("../../tests/fixtures/features/sentry-cli/devcontainer-feature.json");

    fn claim_of(feature: &str) -> Value {
        let feature: Value = serde_json::from_str(feature).unwrap();
        feature.pointer("/customizations/devsandbox/credentials/sentry").unwrap().clone()
    }

    /// The base-url claim most error cases start from: `baseUrl`, `rewrite`,
    /// a phantom prefix and file all set.
    fn cli_value() -> Value {
        claim_of(SENTRY_CLI)
    }

    /// The sentry-cli claim with `field` set (`Null` removes it).
    fn cli_with(field: &str, value: Value) -> Value {
        let mut claim = cli_value();
        let obj = claim.as_object_mut().unwrap();
        if value.is_null() {
            obj.remove(field);
        } else {
            obj.insert(field.to_string(), value);
        }
        claim
    }

    fn cli() -> Claim {
        parse("sentry", &cli_value()).unwrap()
    }

    fn version_of(claim: Value) -> String {
        parse("sentry", &claim).unwrap().version()
    }

    /// The full error chain, as through a feature named `sentry`.
    fn err(claim: Value) -> String {
        let all = json!({ "sentry": claim });
        format!("{:#}", parse_all("sentry", &all).unwrap_err())
    }

    fn assert_err(claim: Value, wants: &[&str]) {
        let e = err(claim);
        assert!(e.starts_with("feature `sentry`: credential `sentry`: "), "{e}");
        for want in wants {
            assert!(e.contains(want), "{want:?} not in {e:?}");
        }
    }

    #[test]
    fn sentry_fixture_parses() {
        let c = parse("sentry", &claim_of(SENTRY)).unwrap();
        let p = &c.pinned;
        assert_eq!(p.mode, Mode::Intercept);
        let hosts: Vec<&str> = p.upstreams.iter().map(|u| u.host.as_str()).collect();
        assert_eq!(hosts, ["sentry.io", "us.sentry.io", "de.sentry.io"]);
        assert_eq!(
            p.inject,
            [Slot::Header { name: "authorization".into(), format: "Bearer {token}".into() }]
        );
        assert!(p.rewrite.is_empty());
        assert_eq!(p.check, Some(Route::parse("GET /api/0/users/me/").unwrap()));
        assert_eq!(p.check_shows.as_deref(), Some("email"));
        assert!(matches!(
            p.login[..],
            [Login::Paste { create: Some(_), .. }, Login::Device { client_id: Some(_), broad: false, .. }]
        ));
        assert!(p.tokens.is_none());
        let org_read = &p.scopes["org.read"].routes;
        assert!(org_read.contains(&Route::parse("GET /api/0/users/me/").unwrap()));
        assert!(p.never.contains(&Route::parse("DELETE /api/0/projects/{org}/{project}/").unwrap()));

        let w = &c.wiring;
        assert_eq!(w.base_url, None);
        assert_eq!(w.phantom.prefix, None);
        assert_eq!(w.phantom.env, ["SENTRY_AUTH_TOKEN"]);
        assert!(w.phantom.files.is_empty());
        assert_eq!((&w.ca, &w.git), (&None, &None));
    }

    #[test]
    fn sentry_cli_fixture_parses() {
        let c = cli();
        assert_eq!(c.kind, "sentry");
        let p = &c.pinned;
        assert_eq!(p.mode, Mode::BaseUrl);
        let hosts: Vec<&str> = p.upstreams.iter().map(|u| u.host.as_str()).collect();
        assert_eq!(hosts, ["sentry.io", "us.sentry.io", "de.sentry.io"]);
        assert!(p.upstreams.iter().all(|u| !u.private));
        assert_eq!(
            p.inject,
            [Slot::Header { name: "authorization".into(), format: "Bearer {token}".into() }]
        );
        assert_eq!(p.check, Some(Route::parse("GET /api/0/").unwrap()));
        assert_eq!(p.refusal, Some(Refusal::Json { field: "detail".into() }));
        assert!(matches!(p.login[..], [Login::Paste { hint: Some(_), create: None }]));
        assert_eq!(p.rewrite.len(), 2);
        let chunk = p.rewrite.iter().find(|r| r.route.to_string().contains("chunk-upload")).unwrap();
        assert_eq!(chunk.fields, ["url"]);
        let regions = p.rewrite.iter().find(|r| r.route.to_string().contains("regions")).unwrap();
        assert_eq!(regions.fields, ["regions[].url"]);
        assert_eq!(
            p.scopes.keys().map(String::as_str).collect::<Vec<_>>(),
            ["issues.read", "org.read", "releases.read", "releases.write", "uploads"]
        );
        assert_eq!(p.scopes["uploads"].permissions["release"], Level::Admin);
        assert!(p.never.contains(&Route::parse("POST /api/0/organizations/{org}/org-auth-tokens/").unwrap()));

        let w = &c.wiring;
        assert_eq!(
            w.base_url,
            Some(BaseUrl { env: "SENTRY_URL".into(), upstream: "sentry.io".into() })
        );
        assert_eq!(w.phantom.prefix.as_deref(), Some("sntrys_"));
        assert_eq!(w.phantom.env, ["SENTRY_AUTH_TOKEN"]);
        let rc = &w.phantom.files[0];
        assert_eq!(rc.path, ".sentryclirc");
        assert!(rc.template.contains("[defaults]\nurl = {baseUrl}"), "{}", rc.template);
        assert!(rc.template.contains("token = {phantom}"), "{}", rc.template);
        assert_eq!(w.ca, None);
        assert_eq!(w.git, None);
    }

    #[test]
    fn routes_parse_and_print_back() {
        for s in [
            "GET /",
            "GET /api/0/",
            "POST /repos/{repo}/pulls",
            "DELETE /api/0/projects/{org}/{project}/keys/{key}/",
            "PUT /a/b:c@d",
            "GET /{repo}/info/refs?service=git-receive-pack",
            "graphql mutation createPullRequest",
            "graphql query",
        ] {
            assert_eq!(Route::parse(s).unwrap().to_string(), s);
        }
        let r = Route::parse("GET /api/0/projects/{org}/{project}/").unwrap();
        assert_eq!(r.vars(), ["org", "project"]);
        assert!(Route::parse("graphql mutation x").unwrap().vars().is_empty());
    }

    #[test]
    fn bad_routes_are_refused() {
        for (s, want) in [
            ("get /api/", "isn't one of GET"),
            ("FETCH /api/", "isn't one of GET"),
            ("GET", "write `METHOD /path`"),
            ("GET api/0/", "must start with `/`"),
            ("GET /api//0/", "empty segment"),
            ("GET /api/../0/", "dot-segment"),
            ("GET /api/./0/", "dot-segment"),
            ("GET /repos/%2e%2e/x", "a segment is literal"),
            ("GET /a{b}/", "a segment is literal"),
            ("GET /{a}/{a}/", "`{a}` twice"),
            ("GET /{a-b}/", "a variable is"),
            ("GET /api/?x", "one `?name=value`"),
            ("GET /api/?x=1&y=2", "one `?name=value`"),
            ("GET /api/?x=%41", "one `?name=value`"),
            ("GET /api/?=1", "one `?name=value`"),
            ("GET /api/#x", "no fragment"),
            ("GET  /api/", "must start with `/`"),
            ("graphql mutation create-pr", "isn't a GraphQL name"),
            ("graphql query viewer", "write `graphql query` or `graphql mutation <name>`"),
            ("graphql subscription x", "write `graphql query`"),
        ] {
            let e = format!("{:#}", Route::parse(s).unwrap_err());
            assert!(e.contains(want), "{s:?}: {want:?} not in {e:?}");
        }
    }

    #[test]
    fn credentials_must_be_an_object() {
        let e = from_customizations(
            "f",
            Some(&json!({ "devsandbox": { "credentials": ["sentry"] } })),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("feature `f`: `customizations.devsandbox.credentials`"));
        assert!(from_customizations("f", None).unwrap().is_empty());
        assert!(from_customizations("f", Some(&json!({ "vscode": {} }))).unwrap().is_empty());
    }

    #[test]
    fn kind_and_shape_errors() {
        let e = format!("{:#}", parse_all("f", &json!({ "Sentry": cli_value() })).unwrap_err());
        assert!(e.contains("credential `Sentry`: the kind must be lowercase"), "{e}");
        assert_err(json!([]), &["must be an object"]);
        assert_err(cli_with("upstream", json!(["sentry.io"])), &["unknown field `upstream`"]);
        assert_err(cli_with("mode", Value::Null), &["`mode` is required"]);
        assert_err(cli_with("mode", json!("proxy")), &["`mode`: unknown variant `proxy`"]);
        assert_err(cli_with("hold", json!(0)), &["`hold`: 0 isn't between 1 and 3600"]);
        assert_err(cli_with("hold", json!("2m")), &["`hold`: invalid type"]);
        assert_err(cli_with("mint", json!("magic")), &["`mint`: unknown variant `magic`"]);
        assert_err(
            cli_with("refusal", json!({ "shape": "json", "field": "a..b" })),
            &["`refusal.field`", "isn't a field path"],
        );
        assert_eq!(
            parse("sentry", &cli_with("refusal", json!({ "shape": "git" }))).unwrap().pinned.refusal,
            Some(Refusal::Git {})
        );
        assert_err(
            cli_with("refusal", json!({ "shape": "git", "field": "x" })),
            &["`refusal`", "unknown field `field`"],
        );
    }

    #[test]
    fn upstream_errors() {
        for (upstreams, want) in [
            (json!([]), "at least one host"),
            (json!(["https://sentry.io"]), "`upstreams[0]`: `https://sentry.io` isn't a hostname: write the bare host"),
            (json!(["sentry.io:443"]), "without scheme, port or path"),
            (json!(["sentry.io/api"]), "without scheme, port or path"),
            (json!(["Sentry.io"]), "write hosts in lowercase"),
            (json!(["sentry.io."]), "drop the trailing dot"),
            (json!(["*.sentry.io"]), "isn't a hostname"),
            (json!(["-a.io"]), "isn't a hostname"),
            (json!(["sentry.io", "sentry.io"]), "`upstreams[1]`: `sentry.io` is listed twice"),
            (
                json!([{ "host": "a.lan", "private": true }, { "host": "b.lan", "private": true }]),
                "at most one upstream is `private`",
            ),
            (json!([{ "host": "a.lan", "lan": true }]), "`upstreams`"),
        ] {
            assert_err(cli_with("upstreams", upstreams), &[want]);
        }
        let c = parse("sentry", &cli_with("upstreams", json!(["sentry.io", { "host": "sentry.lan", "private": true }]))).unwrap();
        assert!(c.pinned.upstreams.iter().any(|u| u.host == "sentry.lan" && u.private));
    }

    #[test]
    fn mode_and_base_url_errors() {
        assert_err(cli_with("baseUrl", Value::Null), &["`base-url` mode needs `baseUrl`"]);
        assert_err(cli_with("baseUrl", json!("SENTRY-URL")), &["`baseUrl`: `SENTRY-URL` isn't an env var name"]);
        assert_err(
            cli_with("baseUrl", json!("SENTRY_AUTH_TOKEN")),
            &["`phantom.env[0]`: `SENTRY_AUTH_TOKEN` is already set by this claim"],
        );
        let mut intercept = cli_with("mode", json!("intercept"));
        assert_err(intercept.clone(), &["`baseUrl` is only for `base-url` mode"]);
        intercept.as_object_mut().unwrap().remove("baseUrl");
        // the fixture's rc template holds {baseUrl}
        assert_err(intercept.clone(), &["`phantom.files[0]`: `{baseUrl}` is only for `base-url` mode"]);
        let obj = intercept.as_object_mut().unwrap();
        obj.insert("phantom".into(), json!({ "env": ["SENTRY_AUTH_TOKEN"] }));
        assert_err(intercept.clone(), &["`rewrite` is only for `base-url` mode"]);
        intercept.as_object_mut().unwrap().remove("rewrite");
        let c = parse("sentry", &intercept).unwrap();
        assert_eq!(c.pinned.mode, Mode::Intercept);
        assert_eq!(c.wiring.base_url, None);
    }

    #[test]
    fn inject_errors() {
        let bearer = json!({ "header": "Authorization", "format": "Bearer {token}" });
        for (inject, want) in [
            (json!([bearer, { "header": "x-api-key", "format": "{token}" }, { "query": "t" }]), "at most 2 slots"),
            (json!([{ "header": "Authorization" }]), "`inject[0]`: a `header` slot needs a `format`"),
            (json!([{ "format": "Bearer {token}" }]), "`format` goes with `header`"),
            (json!([{ "header": "Authorization", "format": "Bearer" }]), "must hold `{token}` once"),
            (json!([{ "header": "Authorization", "format": "{token} {token}" }]), "must hold `{token}` once"),
            (json!([{ "header": "Bad Header", "format": "{token}" }]), "isn't a header name"),
            (json!([{ "header": "Host", "format": "{token}" }]), "`host` can't take a token"),
            (json!([{ "header": "a", "format": "{token}\r\nx: y" }]), "control character"),
            (json!([{ "header": "a", "format": "{token}", "basic": "u" }]), "exactly one of"),
            (json!([{}]), "exactly one of"),
            (json!([{ "basic": "x:y" }]), "`basic` takes the username"),
            (json!([{ "query": "a b" }]), "isn't a plain query parameter name"),
            (json!([{ "cookie": "a" }]), "unknown field `cookie`"),
            (json!([bearer, { "header": "authorization", "format": "Bearer {token}" }]), "`inject[1]`: the same slot twice"),
        ] {
            assert_err(cli_with("inject", inject), &[want]);
        }
        let two = cli_with("inject", json!([bearer, { "header": "X-Api-Key", "format": "{token}" }]));
        assert_eq!(parse("sentry", &two).unwrap().pinned.inject.len(), 2);
    }

    #[test]
    fn guard_only_claim_has_no_phantom_or_login() {
        let mut claim = cli_value();
        let obj = claim.as_object_mut().unwrap();
        obj.remove("inject");
        assert_err(claim.clone(), &["`phantom` without an `inject` slot"]);
        claim.as_object_mut().unwrap().remove("phantom");
        assert_err(claim.clone(), &["`login` without an `inject` slot"]);
        claim.as_object_mut().unwrap().remove("login");
        let c = parse("sentry", &claim).unwrap();
        assert!(c.pinned.inject.is_empty());
        assert_eq!(c.wiring.phantom, Phantom::default());
    }

    #[test]
    fn phantom_errors() {
        let rc = |path: &str, template: &str| json!({ "env": [], "files": [{ "path": path, "template": template }] });
        for (phantom, want) in [
            (Value::Null, "`phantom` must name an env var or a file"),
            (json!({}), "`phantom` must name an env var or a file"),
            (json!({ "env": ["1TOKEN"] }), "`phantom.env[0]`: `1TOKEN` isn't an env var name"),
            (json!({ "env": ["A", "A"] }), "`phantom.env[1]`: `A` is already set"),
            (json!({ "env": ["A"], "prefix": "sk ant" }), "`phantom.prefix`"),
            (json!({ "env": ["A"], "secret": true }), "unknown field `secret`"),
            (rc("/root/.sentryclirc", "{phantom}"), "`phantom.files[0]`: `/root/.sentryclirc`: must be relative"),
            (rc("~/.sentryclirc", "{phantom}"), "without `~/`"),
            (rc("../.sentryclirc", "{phantom}"), "without `.`, `..`"),
            (rc(".config//gh/hosts.yml", "{phantom}"), "without `.`, `..` or empty parts"),
            (rc(".sentryclirc", "[auth]\ntoken = x\n"), "the template must hold `{phantom}`"),
        ] {
            assert_err(cli_with("phantom", phantom), &[want]);
        }
        let dup = json!({ "files": [
            { "path": ".a", "template": "{phantom}" },
            { "path": ".a", "template": "{phantom}" },
        ] });
        assert_err(cli_with("phantom", dup), &["`phantom.files[1]`: `.a` is listed twice"]);
    }

    #[test]
    fn scope_and_never_errors() {
        let scopes = |v: Value| cli_with("scopes", v);
        assert_err(scopes(json!({ "a": ["GET /x"], "b": ["GET /x"] })), &["`scopes.b`: `GET /x` is in scope `a` too"]);
        assert_err(scopes(json!({ "a": ["GET /x", "GET /x"] })), &["`scopes.a`: `GET /x` is listed twice"]);
        assert_err(scopes(json!({ "other": ["GET /x"] })), &["`other` is the scope of unknown routes"]);
        assert_err(scopes(json!({ "Pr.Write": ["GET /x"] })), &["scope names are lowercase"]);
        assert_err(scopes(json!({ "a": [] })), &["`scopes.a`: a scope needs at least one route"]);
        assert_err(scopes(json!({ "a": ["get /x"] })), &["`scopes.a`: `get /x`"]);
        assert_err(scopes(json!({ "a": { "routes": ["GET /x"], "permissions": { "x": "owner" } } })), &["`scopes.a`", "unknown variant `owner`"]);
        assert_err(scopes(json!({ "a": { "routes": ["GET /x"], "permissions": { "X Y": "read" } } })), &["`X Y` isn't a permission name"]);
        assert_err(scopes(json!({ "a": { "routes": ["GET /x"], "ask": true } })), &["unknown field `ask`"]);
        let pr = parse("sentry", &scopes(json!({ "pr.write": ["POST /repos/{repo}/pulls", "graphql mutation createPullRequest"] }))).unwrap();
        assert_eq!(pr.pinned.scopes["pr.write"].routes.len(), 2);
        assert!(pr.pinned.scopes["pr.write"].permissions.is_empty());

        assert_err(
            cli_with("never", json!(["GET /api/0/"])),
            &["`never[0]`: `GET /api/0/` is in scope `org.read` too: a route is scoped or `never`, not both"],
        );
        assert_err(cli_with("never", json!(["POST /x", "POST /x"])), &["`never[1]`: `POST /x` is listed twice"]);
    }

    #[test]
    fn rewrite_errors() {
        let rewrite = |v: Value| cli_with("rewrite", v);
        assert_err(rewrite(json!([{ "route": "GET /x", "fields": [] }])), &["`rewrite[0]`: `fields` must name"]);
        assert_err(rewrite(json!([{ "route": "GET /x", "fields": ["regions[]url"] }])), &["isn't a field path"]);
        assert_err(rewrite(json!([{ "route": "GET /x", "fields": ["a", "a"] }])), &["`a` is listed twice"]);
        assert_err(rewrite(json!([{ "route": "graphql mutation x", "fields": ["a"] }])), &["rewrites apply to HTTP routes"]);
        assert_err(
            rewrite(json!([{ "route": "GET /x", "fields": ["a"] }, { "route": "GET /x", "fields": ["b"] }])),
            &["`rewrite[1]`: `GET /x` is listed twice"],
        );
        assert_err(rewrite(json!([{ "route": "GET /x", "field": "a" }])), &["unknown field `field`"]);
    }

    #[test]
    fn login_and_check_errors() {
        assert_err(cli_with("login", Value::Null), &["`login` is required"]);
        assert_err(cli_with("login", json!([])), &["at least one method"]);
        assert_err(cli_with("login", json!([{ "method": "paste" }, { "method": "paste", "hint": "x" }])), &["`login[1]`: the same method twice"]);
        assert_err(cli_with("login", json!([{ "method": "sms" }])), &["`login`", "unknown variant `sms`"]);
        assert_err(cli_with("login", json!([{ "method": "paste", "url": "x" }])), &["unknown field `url`"]);
        assert_err(cli_with("login", json!([{ "method": "github-app-manifest", "owner": "x" }])), &["unknown field `owner`"]);
        assert_err(
            cli_with("login", json!([{ "method": "device", "clientId": "abc", "deviceUrl": "http://github.com/login/device/code", "tokenUrl": "https://github.com/login/oauth/access_token" }])),
            &["`login[0]`: `deviceUrl`", "isn't an https URL"],
        );
        let all = json!([
            { "method": "github-app-manifest" },
            { "method": "browser", "clientId": "abc", "authorizeUrl": "https://github.com/login/oauth/authorize", "tokenUrl": "https://github.com/login/oauth/access_token" },
            { "method": "device", "clientId": "abc", "deviceUrl": "https://github.com/login/device/code", "tokenUrl": "https://github.com/login/oauth/access_token" },
            { "method": "paste" },
        ]);
        assert_eq!(parse("sentry", &cli_with("login", all)).unwrap().pinned.login.len(), 4);
        assert_err(
            cli_with("login", json!([{ "method": "device", "clientId": "", "deviceUrl": "https://github.com/login/device/code", "tokenUrl": "https://github.com/login/oauth/access_token" }])),
            &["`login[0]`", "`clientId` must be non-empty"],
        );
        let home_app = json!([
            { "method": "browser", "authorizeUrl": "https://github.com/login/oauth/authorize", "tokenUrl": "https://github.com/login/oauth/access_token" },
            { "method": "device", "deviceUrl": "https://github.com/login/device/code", "tokenUrl": "https://github.com/login/oauth/access_token" },
        ]);
        let c = parse("sentry", &cli_with("login", home_app)).unwrap();
        assert!(c.pinned.login.iter().all(|l| matches!(l, Login::Device { client_id: None, .. } | Login::Browser { client_id: None, .. })));
        assert!(!c.canonical().contains("clientId"), "an absent client id stays absent in the pinned claim");
        assert_eq!(from_canonical(&c.canonical()).unwrap().1, c.pinned);

        let device = |extra: Value| {
            let mut d = json!({ "method": "device", "clientId": "c", "deviceUrl": "https://github.com/login/device/code", "tokenUrl": "https://github.com/login/oauth/access_token" });
            d.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            d
        };
        let broad = |logins: Value, tokens: Value| {
            let mut c = cli_with("login", logins);
            if !tokens.is_null() {
                c["tokens"] = tokens;
            }
            c
        };
        let two = json!([device(json!({})), device(json!({ "broad": true, "scope": "repo read:org" }))]);
        let c = parse("sentry", &broad(two.clone(), json!({ "accept": ["sntryu_"], "broad": ["gho_", "ghb_"] }))).unwrap();
        let t = c.pinned.tokens.as_ref().unwrap();
        assert_eq!(t.broad, ["ghb_", "gho_"], "sorted");
        assert_eq!(c.pinned.login.iter().filter(|l| l.broad()).count(), 1);
        assert!(c.pinned.login.iter().any(|l| matches!(l, Login::Device { scope: Some(s), broad: true, .. } if s == "repo read:org")));
        assert!(!c.canonical().contains("\"broad\":false"), "a narrow login stays without it");
        assert_eq!(from_canonical(&c.canonical()).unwrap().1, c.pinned);
        assert_err(broad(two.clone(), json!({ "accept": ["sntryu_"] })), &["`login[1]`: a `broad` login needs `tokens.broad`"]);
        assert_err(broad(two.clone(), Value::Null), &["`login[1]`: a `broad` login needs `tokens.broad`"]);
        let twice = json!([device(json!({ "broad": true })), device(json!({ "broad": true }))]);
        assert_err(broad(twice, json!({ "broad": ["gho_"] })), &["`login[1]`: the same method twice"]);
        for (tokens, wants) in [
            (json!({ "accept": ["gh"], "broad": ["gho_"] }), "`tokens.broad`: `gho_` overlaps `tokens.accept`'s `gh`"),
            (json!({ "accept": ["gho_x"], "broad": ["gho_"] }), "overlaps `tokens.accept`'s `gho_x`"),
            (json!({ "accept": ["gho_"], "broad": ["gho_"] }), "overlaps"),
            (json!({ "broad": ["gho_", "gho_"] }), "`tokens.broad[1]`: `gho_` is listed twice"),
            (json!({ "broad": ["g o"] }), "`tokens.broad[0]`: up to 32 letters"),
            (json!({ "broad": vec!["a"; 9] }), "`tokens.broad`: at most 8 prefixes"),
        ] {
            assert_err(broad(two.clone(), tokens), &[wants]);
        }
        for bad in ["", "repo  read", " repo", "a\"b", "a\\b", "r\u{e9}po", "a\tb"] {
            assert_err(broad(json!([device(json!({ "scope": bad }))]), Value::Null), &["`login[0]`", "OAuth scopes separated by single spaces"]);
        }
        let browser = json!([{ "method": "browser", "authorizeUrl": "https://github.com/login/oauth/authorize", "tokenUrl": "https://github.com/login/oauth/access_token", "scope": "repo", "broad": true }]);
        let c = parse("sentry", &broad(browser, json!({ "broad": ["gho_"] }))).unwrap();
        assert!(matches!(&c.pinned.login[..], [Login::Browser { client_id: None, broad: true, .. }]), "the client id may be the home's");

        let paste = |create: Value| cli_with("login", json!([{ "method": "paste", "create": create }]));
        let c = parse("sentry", &paste(json!({ "url": "https://github.com/settings/personal-access-tokens/new" }))).unwrap();
        let want = Create { url: "https://github.com/settings/personal-access-tokens/new".into(), permissions: false };
        assert_eq!(c.pinned.login, [Login::Paste { hint: None, create: Some(want) }]);
        assert_eq!(from_canonical(&c.canonical()).unwrap().1, c.pinned);
        assert_err(paste(json!({ "url": "http://github.com/new" })), &["`login[0]`: `create.url`", "isn't an https URL"]);
        assert_err(paste(json!({ "url": "https://github.com/new?x=1" })), &["`login[0]`: `create.url`", "a query or fragment"]);
        assert_err(paste(json!({ "url": "https://github.com/new#x" })), &["a query or fragment"]);
        assert_err(paste(json!({ "url": "https://github.com/new", "name": "x" })), &["unknown field `name`"]);
        assert_err(paste(json!({ "permissions": true })), &["missing field `url`"]);

        assert_err(cli_with("check", json!("GET /api/0/organizations/{org}/")), &["`check`", "no `{var}`"]);
        assert_err(cli_with("check", json!("POST /api/0/")), &["`check`", "a `GET` or `HEAD`"]);
        assert_err(cli_with("check", json!("graphql mutation x")), &["`check`"]);

        let c = parse("sentry", &cli_with("checkShows", json!("user_name"))).unwrap();
        assert_eq!(c.pinned.check_shows.as_deref(), Some("user_name"));
        assert!(c.canonical().contains(r#""checkShows":"user_name""#), "{}", c.canonical());
        assert_eq!(from_canonical(&c.canonical()).unwrap().1, c.pinned);
        assert!(!cli().canonical().contains("checkShows"), "absent stays absent");
        assert_err(cli_with("checkShows", json!("user.name")), &["`checkShows`", "isn't a field name"]);
        assert_err(cli_with("checkShows", json!("")), &["isn't a field name"]);
        let mut unchecked = cli_with("checkShows", json!("login"));
        unchecked.as_object_mut().unwrap().remove("check");
        assert_err(unchecked, &["the claim has no `check`"]);
    }

    #[test]
    fn presence_and_fence() {
        let ok = cli_with(
            "presence",
            json!([
                { "route": "POST /repos/{repo}/releases", "target": "tag_name" },
                { "route": "DELETE /repos/{repo}/git/refs/{ref}", "target": "{ref}" },
                { "route": "graphql mutation updateRef", "target": "input.refId" },
                { "ref": "refs/tags/*" },
                { "ref": "refs/heads/*", "on": ["delete"] },
            ]),
        );
        let p = parse("sentry", &ok).unwrap().pinned.presence;
        assert!(p.contains(&Presence::Ref { pattern: "refs/tags/*".into(), on: vec![RefOp::Create, RefOp::Update, RefOp::Delete] }));
        assert!(p.contains(&Presence::Ref { pattern: "refs/heads/*".into(), on: vec![RefOp::Delete] }));

        let presence = |v: Value| cli_with("presence", v);
        assert_err(presence(json!([{ "route": "DELETE /a/{x}", "target": "{y}" }])), &["`presence[0]`: `target`", "has no `{y}`"]);
        assert_err(presence(json!([{ "route": "POST /a" }])), &["needs the `target` field"]);
        assert_err(presence(json!([{ "ref": "tags/*" }])), &["starts with `refs/`"]);
        assert_err(presence(json!([{ "ref": "refs/*/x" }])), &["`*` only as the last part"]);
        assert_err(presence(json!([{ "ref": "refs/tags/*", "on": [] }])), &["`on` is empty"]);
        assert_err(presence(json!([{ "ref": "refs/tags/*", "route": "POST /a", "target": "x" }])), &["name a `route` with its `target`, or a `ref`"]);
        assert_err(presence(json!([{ "ref": "refs/tags/*" }, { "ref": "refs/tags/*", "on": ["delete", "create", "update"] }])), &["`presence[1]`: listed twice"]);

        let fence = cli_with(
            "fence",
            json!({
                "POST /repos/{repo}/forks": ["{repo}", "organization"],
                "graphql mutation createPullRequest": ["input.repositoryId", "input.headRepositoryId"],
                "graphql mutation createCommitOnBranch": ["input.branch.repositoryNameWithOwner", "input.ids[].id"],
            }),
        );
        let f = parse("sentry", &fence).unwrap().pinned.fence;
        assert_eq!(f[&Route::parse("graphql mutation createPullRequest").unwrap()], ["input.headRepositoryId", "input.repositoryId"]);
        assert_err(cli_with("fence", json!({ "graphql query": ["owner"] })), &["`fence.graphql query`", "never fenced"]);
        assert_err(cli_with("fence", json!({ "graphql mutation m": ["input..x"] })), &["isn't a field path"]);
        assert_err(cli_with("fence", json!({ "GET /a/{x}": ["{y}"] })), &["`fence.GET /a/{x}`", "has no `{y}`"]);
        assert_err(cli_with("fence", json!({ "graphql mutation m": ["{repo}"] })), &["has no `{repo}`"]);
        assert_err(cli_with("fence", json!({ "GET /a": [] })), &["name at least one input"]);
        assert_err(cli_with("fence", json!({ "GET /a": ["x", "x"] })), &["`x` is listed twice"]);
    }

    fn orgs_value() -> Value {
        json!({
            "var": "{org}",
            "resolve": "GET /api/0/organizations/{org}/",
            "id": "id",
            "lists": { "GET /api/0/organizations/": ["slug", "id"] },
        })
    }

    fn orgs_with(field: &str, value: Value) -> Value {
        let mut orgs = orgs_value();
        orgs[field] = value;
        cli_with("orgs", orgs)
    }

    #[test]
    fn orgs_fence_one_scoped_variable() {
        let c = parse("sentry", &cli_with("orgs", orgs_value())).unwrap();
        let o = c.pinned.orgs.as_ref().unwrap();
        assert_eq!(o.var, "{org}");
        assert_eq!(o.resolve, Route::parse("GET /api/0/organizations/{org}/").unwrap());
        assert_eq!(o.id, "id");
        assert_eq!(o.lists[&Route::parse("GET /api/0/organizations/").unwrap()], ["id", "slug"], "sorted");
        assert_eq!(from_canonical(&c.canonical()).unwrap().1, c.pinned);
        assert!(!cli().canonical().contains("\"orgs\""), "absent: the canonical form doesn't change");
        let mut no_lists = orgs_value();
        no_lists.as_object_mut().unwrap().remove("lists");
        assert!(parse("sentry", &cli_with("orgs", no_lists)).unwrap().pinned.orgs.unwrap().lists.is_empty());

        assert_err(orgs_with("extra", json!(1)), &["`orgs`", "unknown field `extra`"]);
        assert_err(orgs_with("var", json!("org")), &["`orgs.var`: `org` isn't a whole `{name}` variable"]);
        assert_err(orgs_with("var", json!("{a-b}")), &["isn't a whole `{name}` variable"]);
        assert_err(orgs_with("var", json!("{repo}")), &["`orgs.var`: `{repo}` is the repo fence's"]);
        assert_err(orgs_with("var", json!("{team}")), &["`orgs.var`: no scoped route holds `{team}`"]);
        assert_err(orgs_with("resolve", json!("graphql query")), &["`orgs.resolve`: `graphql query` isn't a `GET` route"]);
        assert_err(orgs_with("resolve", json!("PUT /api/0/organizations/{org}/")), &["isn't a `GET` route"]);
        assert_err(orgs_with("resolve", json!("GET /api/0/organizations/")), &["`orgs.resolve`: `GET /api/0/organizations/` has no `{org}`"]);
        assert_err(orgs_with("resolve", json!("GET /a//")), &["`orgs`", "empty segment"]);
        assert_err(orgs_with("id", json!("")), &["`orgs.id`: `` isn't a field name"]);
        assert_err(orgs_with("id", json!("a.b")), &["`orgs.id`: `a.b` isn't a field name"]);
        let lists = |l: Value| orgs_with("lists", l);
        assert_err(lists(json!({ "graphql query": ["slug"] })), &["`orgs.lists.graphql query`: lists are HTTP routes"]);
        assert_err(lists(json!({ "GET /api/0/teams/": ["slug"] })), &["`orgs.lists.GET /api/0/teams/`: not in any of the claim's `scopes`"]);
        assert_err(
            lists(json!({ "GET /api/0/organizations/{org}/projects/": ["slug"] })),
            &["`orgs.lists.GET /api/0/organizations/{org}/projects/`: it holds `{org}`"],
        );
        assert_err(lists(json!({ "GET /api/0/organizations/": [] })), &["name at least one field"]);
        assert_err(lists(json!({ "GET /api/0/organizations/": ["a[]"] })), &["`a[]` isn't a field name"]);
        assert_err(lists(json!({ "GET /api/0/organizations/": ["slug", "slug"] })), &["`slug` is listed twice"]);
    }

    #[test]
    fn graphql_routes_own_the_graphql_path() {
        let gql = parse("sentry", &cli_with("scopes", json!({ "read": ["graphql query", "GET /api/0/"] }))).unwrap();
        assert!(gql.pinned.graphql());
        assert!(!parse("sentry", &cli_with("scopes", json!({ "read": ["POST /graphql"] }))).unwrap().pinned.graphql());
        assert_err(
            cli_with("scopes", json!({ "read": ["graphql query"], "w": ["POST /graphql"] })),
            &["`POST /graphql`: a claim with GraphQL routes decides `/graphql` by its document"],
        );
    }

    #[test]
    fn git_and_ca_are_intercept_only() {
        assert_err(cli_with("git", json!({ "hosts": ["sentry.io"] })), &["`git` is only for `intercept` mode"]);
        assert_err(cli_with("ca", json!({ "env": "X" })), &["`ca` is only for `intercept` mode"]);

        let gh = json!({
            "upstreams": ["api.github.com", "uploads.github.com", "github.com"],
            "mode": "intercept",
            "inject": [{ "header": "Authorization", "format": "Bearer {token}" }],
            "phantom": { "env": ["GH_TOKEN"], "files": [{ "path": ".config/gh/hosts.yml", "template": "github.com:\n  oauth_token: {phantom}\n" }] },
            "login": [{ "method": "paste" }],
            "git": { "hosts": ["github.com"] },
            "ca": { "env": "SOME_CA" },
        });
        let c = parse("github", &gh).unwrap();
        assert_eq!(c.wiring.git, Some(Git { hosts: vec!["github.com".into()] }));
        assert_eq!(c.wiring.ca, Some(Ca { env: "SOME_CA".into() }));

        let with = |field: &str, v: Value| {
            let mut c = gh.clone();
            c.as_object_mut().unwrap().insert(field.into(), v);
            format!("{:#}", parse("github", &c).unwrap_err())
        };
        assert!(with("git", json!({ "hosts": ["gitlab.com"] })).contains("`git.hosts[0]`: `gitlab.com` isn't one of the claim's `upstreams`"));
        assert!(with("git", json!({ "hosts": [] })).contains("at least one host"));
        assert!(with("git", json!({ "hosts": ["github.com", "github.com"] })).contains("listed twice"));
        assert!(with("ca", json!({ "env": "GH_TOKEN" })).contains("`ca.env`: `GH_TOKEN` is already set"));
    }

    #[test]
    fn lfs_is_pinned_and_rides_on_git_hosts() {
        let gh = json!({
            "upstreams": ["api.github.com", "github.com"],
            "mode": "intercept",
            "inject": [{ "basic": "x-access-token" }],
            "phantom": { "env": ["GH_TOKEN"] },
            "login": [{ "method": "paste" }],
            "git": { "hosts": ["github.com"] },
            "scopes": { "repo.read": ["POST /{repo}/info/lfs/objects/batch"], "git.push": ["POST /{repo}/git-receive-pack"] },
        });
        let with = |lfs: Value| {
            let mut c = gh.clone();
            c.as_object_mut().unwrap().insert("lfs".into(), lfs);
            c
        };
        let ok = with(json!({ "hosts": ["github.com"], "upload": "git.push" }));
        let c = parse("github", &ok).unwrap();
        assert_eq!(c.pinned.lfs, Some(Lfs { hosts: vec!["github.com".into()], upload: "git.push".into() }));
        assert_eq!(from_canonical(&c.canonical()).unwrap().1, c.pinned);
        let without = parse("github", &gh).unwrap();
        assert_ne!(c.version(), without.version(), "lfs is pinned");
        assert!(!without.canonical().contains("\"lfs\""), "absent: the canonical form doesn't change");
        let other_scope = with(json!({ "hosts": ["github.com"], "upload": "repo.read" }));
        assert_ne!(parse("github", &other_scope).unwrap().version(), c.version());

        let err = |v: Value| format!("{:#}", parse("github", &v).unwrap_err());
        assert!(err(with(json!({ "hosts": ["api.github.com"], "upload": "git.push" }))).contains("`lfs.hosts[0]`: `api.github.com` isn't one of `git.hosts`"));
        assert!(err(with(json!({ "hosts": [], "upload": "git.push" }))).contains("at least one host"));
        assert!(err(with(json!({ "hosts": ["github.com", "github.com"], "upload": "git.push" }))).contains("listed twice"));
        assert!(err(with(json!({ "hosts": ["github.com"], "upload": "lfs.write" }))).contains("`lfs.upload`: `lfs.write` isn't one of the claim's `scopes`"));
        assert!(err(with(json!({ "hosts": ["github.com"] }))).contains("`lfs`"));
        assert!(err(with(json!({ "hosts": ["github.com"], "upload": "git.push", "verify": true }))).contains("unknown field `verify`"));
        let mut no_git = ok.clone();
        no_git.as_object_mut().unwrap().remove("git");
        assert!(err(no_git).contains("`lfs` needs `git`"));
        assert_err(cli_with("lfs", json!({ "hosts": ["sentry.io"], "upload": "uploads" })), &["`lfs` is only for `intercept` mode"]);
    }

    #[test]
    fn canonical_reads_back() {
        let mut c = cli();
        c.pinned.upstreams[0].private = true;
        c.pinned.inject.push(Slot::Basic { username: "u".into() });
        c.pinned.inject.push(Slot::Query { param: "key".into() });
        c.pinned.presence.push(Presence::Ref { pattern: "refs/tags/*".into(), on: vec![RefOp::Create] });
        c.pinned.fence.insert(Route::parse("GET /a/{x}/").unwrap(), vec!["{x}".into()]);
        c.pinned.hold = Some(5);
        let (kind, pinned) = from_canonical(&c.canonical()).unwrap();
        assert_eq!((kind.as_str(), &pinned), ("sentry", &c.pinned));
        assert!(from_canonical(r#"{"kind":"x","claim":{}}"#).is_err());
    }

    #[test]
    fn version_is_sha256_hex_of_the_canonical_pinned_part() {
        let c = cli();
        let v = c.version();
        assert_eq!(v.len(), 64);
        assert!(v.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(v, hex::encode(&Sha256::digest(c.canonical().as_bytes())[..]));
        let canonical = c.canonical();
        assert!(canonical.starts_with(r#"{"kind":"sentry","claim":{"upstreams":"#), "{canonical}");
        for wiring in ["SENTRY_AUTH_TOKEN", "SENTRY_URL", "sntrys_", ".sentryclirc"] {
            assert!(!canonical.contains(wiring), "{wiring} in {canonical}");
        }
    }

    #[test]
    fn reordering_keeps_the_version() {
        let base = cli().version();
        let mut claim = cli_value();
        let obj = claim.as_object_mut().unwrap();
        for list in ["never", "rewrite"] {
            obj.get_mut(list).unwrap().as_array_mut().unwrap().reverse();
        }
        for scope in obj["scopes"].as_object_mut().unwrap().values_mut() {
            scope["routes"].as_array_mut().unwrap().reverse();
        }
        assert_eq!(parse("sentry", &claim).unwrap().version(), base);
        // The first upstream is pinned: the host `login` checks a token on.
        claim["upstreams"].as_array_mut().unwrap().reverse();
        let reordered = parse("sentry", &claim).unwrap();
        assert_ne!(reordered.version(), base);
        assert_eq!(reordered.wiring.base_url.unwrap().upstream, "de.sentry.io");

        let two = |a: Value, b: Value| version_of(cli_with("inject", json!([a, b])));
        let bearer = json!({ "header": "Authorization", "format": "Bearer {token}" });
        let key = json!({ "header": "x-api-key", "format": "{token}" });
        assert_eq!(two(bearer.clone(), key.clone()), two(key, bearer));
        // header names compare case-insensitively
        assert_eq!(
            version_of(cli_with("inject", json!([{ "header": "AUTHORIZATION", "format": "Bearer {token}" }]))),
            base
        );
        let rewrite = |fields: Value| {
            version_of(cli_with("rewrite", json!([{ "route": "GET /x", "fields": fields }])))
        };
        assert_eq!(rewrite(json!(["a", "b"])), rewrite(json!(["b", "a"])));
    }

    #[test]
    fn wiring_changes_keep_the_version() {
        let base = cli().version();
        let rc = "[defaults]\nurl = {baseUrl}\n\n[auth]\ntoken = {phantom}\n";
        for (field, value) in [
            ("baseUrl", json!("SENTRY_BASE")),
            ("phantom", json!({ "env": ["SENTRY_TOKEN"], "files": [{ "path": ".sentryclirc", "template": rc }] })),
            ("phantom", json!({ "prefix": "x_", "env": ["SENTRY_AUTH_TOKEN"] })),
            ("phantom", json!({ "files": [{ "path": ".config/sentry/rc", "template": "token={phantom}" }] })),
        ] {
            assert_eq!(version_of(cli_with(field, value.clone())), base, "{field} = {value}");
        }
    }

    #[test]
    fn semantic_changes_move_the_version() {
        let base = cli().version();
        let mut seen = BTreeSet::from([base.clone()]);
        let mut changed = |claim: Value, what: &str| {
            let v = version_of(claim);
            assert!(seen.insert(v), "{what}: version unchanged or same as another change");
        };
        changed(cli_with("upstreams", json!(["sentry.io", "us.sentry.io", "de.sentry.io", "eu.sentry.io"])), "upstream added");
        changed(cli_with("upstreams", json!(["sentry.io", "us.sentry.io"])), "upstream dropped");
        changed(cli_with("upstreams", json!(["sentry.io", "us.sentry.io", { "host": "de.sentry.io", "private": true }])), "private");
        changed(cli_with("inject", json!([{ "header": "Authorization", "format": "Token {token}" }])), "format");
        changed(cli_with("inject", json!([{ "query": "token" }])), "slot");
        changed(cli_with("check", json!("HEAD /api/0/")), "check");
        changed(cli_with("check", Value::Null), "no check");
        changed(cli_with("checkShows", json!("user")), "checkShows");
        changed(cli_with("hold", json!(30)), "hold");
        changed(cli_with("mint", json!("oauth-refresh")), "mint");
        changed(cli_with("refusal", json!({ "shape": "json", "field": "message" })), "refusal");
        changed(cli_with("login", json!([{ "method": "paste" }])), "login hint");
        changed(cli_with("rewrite", json!([{ "route": "GET /api/0/organizations/{org}/chunk-upload/", "fields": ["url"] }])), "rewrite dropped");
        changed(cli_with("presence", json!([{ "ref": "refs/tags/*" }])), "presence");
        changed(cli_with("fence", json!({ "GET /api/0/organizations/{org}/": ["{org}"] })), "fence");
        changed(cli_with("orgs", orgs_value()), "orgs");
        changed(orgs_with("id", json!("uuid")), "orgs.id");
        changed(orgs_with("lists", json!({})), "orgs.lists");

        let mut scopes = cli_value()["scopes"].clone();
        scopes["issues.read"]["routes"].as_array_mut().unwrap().push(json!("GET /api/0/issues/{id}/hashes/"));
        changed(cli_with("scopes", scopes), "route added to a scope");

        let mut scopes = cli_value()["scopes"].clone();
        let moved = scopes["issues.read"]["routes"].as_array_mut().unwrap().pop().unwrap();
        scopes["org.read"]["routes"].as_array_mut().unwrap().push(moved);
        changed(cli_with("scopes", scopes), "route moved between scopes");

        let mut scopes = cli_value()["scopes"].clone();
        scopes["issues.read"]["permissions"]["event"] = json!("write");
        changed(cli_with("scopes", scopes), "permission level");

        let mut never = cli_value()["never"].clone();
        never.as_array_mut().unwrap().pop();
        changed(cli_with("never", never), "never dropped");

        let other_kind = parse("sentry-alt", &cli_value()).unwrap().version();
        assert_ne!(other_kind, base, "kind");
    }
}
