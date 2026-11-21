//! What an instance carries for its credential claims (credential-broker,
//! *Phantoms*, *In the container*): per claim its phantom, its loopback
//! listeners (base-URL claims) and its phantom files, plus, with intercept
//! claims, the shared proxy port and git's own ([`Intercept`]), made at `run`, kept by
//! `rebuild`, saved on the `Instance` in `state.toml` and gone with it at
//! `rm`. Pure: `run` (`commands::run::claims`) does the I/O.
//!
//! The broker reads these per call: the bridge names the instance, its
//! claims give the phantom table ([`by_phantom`]) and each listener's
//! upstream ([`by_port`]); the claim's `version` selects the home's approved
//! copy. Which home entry serves a claim isn't kept: profiles and config
//! change, so it's resolved per call (`credentials::home::resolve`).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::claim::{self, Claim, Mode};
use crate::devsbd::brokerfile::{self, BrokerSpec};

/// The container-loopback ports broker listeners take: below the default
/// ephemeral range (`ip_local_port_range` starts at 32768), so no outgoing
/// connection holds one; below Kubernetes' NodePorts (30000-32767), which a
/// cluster in the sandbox binds; above the ports dev servers and databases
/// default to. Per container, so instances never contend for them.
pub const PORTS: RangeInclusive<u16> = 29000..=29999;

/// Claims by the feature ref that made them, as the config names it.
pub type Found = Vec<(String, Claim)>;

/// Random characters in a phantom: base62, ~143 bits.
const PHANTOM_RANDOM: usize = 24;
const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceClaim {
    pub kind: String,
    /// The feature that claimed it, as the config names it.
    pub feature: String,
    /// [`Claim::version`] at `run`: which approved copy the home serves.
    pub version: String,
    /// [`Claim::canonical`], for a home that lacks the version.
    pub claim: String,
    pub phantom: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listeners: Vec<Listener>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<File>,
    /// The claim's `git.hosts`: while any claim has some, the agent relay
    /// filters keys (docs/architecture/helper/ssh-agent.md, *Key filtering*).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_hosts: Vec<String>,
}

/// Every claim's `git.hosts`, in claim order.
pub fn git_hosts(claims: &[InstanceClaim]) -> Vec<String> {
    claims.iter().flat_map(|c| c.git_hosts.iter().cloned()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    /// On the container's `127.0.0.1`.
    pub port: u16,
    pub upstream: String,
}

/// An instance's intercept side: one proxy port handing `hosts` (every
/// intercept claim's upstreams but the ones only git's own port takes,
/// git-gateway) to the broker, git's own port, the proxy env the container
/// had (how devsbd dials every other target), the system bundle the CA
/// variables name, and the member CA cert last installed there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intercept {
    /// On the container's `127.0.0.1`, from [`PORTS`], kept by `rebuild`.
    pub port: u16,
    /// Empty when every intercepted host is a git host.
    pub hosts: Vec<String>,
    /// The claims' `git.hosts` on a port of their own, the only one where
    /// they're handed to the broker: only git's URL-scoped `http.proxy`
    /// points there (docs/architecture/credentials/git.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<ProxyPort>,
    /// [`original_proxy_env`]; never the `HTTPS_PROXY` `run` sets, which
    /// would loop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proxy_env: Vec<(String, String)>,
    pub bundle: String,
    /// [`crate::credentials::trust::fingerprint`] of the cert installed;
    /// `None` until an install succeeds. `start` reinstalls on a mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
}

impl Intercept {
    /// Every host either port hands the broker: what the member CA covers.
    pub fn bound_hosts(&self) -> Vec<String> {
        let git = self.git.iter().flat_map(|g| g.hosts.iter());
        self.hosts.iter().chain(git).cloned().collect()
    }
}

/// A proxy port [`plan`] picks and the hosts it binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyPort {
    pub port: u16,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub claims: Vec<InstanceClaim>,
    /// The shared port: `None` without an intercept claim.
    pub proxy: Option<ProxyPort>,
    /// git's own port: `None` without a claim naming `git.hosts`.
    pub git: Option<ProxyPort>,
}

/// A phantom file as devsbd writes it ([`brokerfile::PhantomFile`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub path: String,
    pub owner: String,
    pub content: String,
}

/// The claim whose phantom is `phantom`, by the table, never by shape.
pub fn by_phantom<'a>(claims: &'a [InstanceClaim], phantom: &str) -> Option<&'a InstanceClaim> {
    claims.iter().find(|c| c.phantom == phantom)
}

/// The claim listening on container port `port`, with that listener's
/// upstream.
pub fn by_port(claims: &[InstanceClaim], port: u16) -> Option<(&InstanceClaim, &str)> {
    claims
        .iter()
        .find_map(|c| c.listeners.iter().find(|l| l.port == port).map(|l| (c, l.upstream.as_str())))
}

/// Every broker port: the listeners, then the shared proxy port, then git's.
pub fn ports<'a>(claims: &'a [InstanceClaim], intercept: Option<&'a Intercept>) -> impl Iterator<Item = u16> + 'a {
    let proxies = intercept.into_iter().flat_map(|i| std::iter::once(i.port).chain(i.git.as_ref().map(|g| g.port)));
    claims.iter().flat_map(|c| c.listeners.iter().map(|l| l.port)).chain(proxies)
}

/// `forwards.*`' refusal of a container port that's a broker port: it
/// answers only the container's own clients.
pub fn forward_refusal(claims: &[InstanceClaim], intercept: Option<&Intercept>, key: &str, port: u16) -> Option<String> {
    if intercept.is_some_and(|i| i.port == port) {
        return Some(format!(
            "port {port} of `{key}` is its credential broker proxy port, which only the sandbox itself may reach"
        ));
    }
    if intercept.and_then(|i| i.git.as_ref()).is_some_and(|g| g.port == port) {
        return Some(format!(
            "port {port} of `{key}` is git's credential broker proxy port, which only the sandbox itself may reach"
        ));
    }
    let (claim, _) = by_port(claims, port)?;
    Some(format!(
        "port {port} of `{key}` is its `{}` credential broker listener, which only the sandbox itself may reach",
        claim.kind
    ))
}

/// The helper's broker file for `claims` and the instance's intercept
/// side; `None` without any claim.
pub fn broker_spec(claims: &[InstanceClaim], intercept: Option<&Intercept>) -> Option<BrokerSpec> {
    if claims.is_empty() {
        return None;
    }
    let listeners = claims
        .iter()
        .flat_map(|c| &c.listeners)
        .map(|l| brokerfile::Listener {
            port: l.port,
            mode: brokerfile::Mode::BaseUrl { upstream: l.upstream.clone() },
        })
        .collect();
    let files = claims
        .iter()
        .flat_map(|c| c.files.iter().map(move |f| (c, f)))
        .map(|(c, f)| brokerfile::PhantomFile {
            owner: f.owner.clone(),
            path: f.path.clone(),
            phantom: c.phantom.clone(),
            content: f.content.clone(),
        })
        .collect();
    let (proxy_listeners, proxy_env) = match intercept {
        Some(i) => {
            let shared = brokerfile::ProxyListener { port: i.port, hosts: i.hosts.clone() };
            let git = i.git.iter().map(|g| brokerfile::ProxyListener { port: g.port, hosts: g.hosts.clone() });
            (std::iter::once(shared).chain(git).collect(), i.proxy_env.clone())
        }
        None => (Vec::new(), Vec::new()),
    };
    let credentials = claims
        .iter()
        .flat_map(|c| c.git_hosts.iter().map(move |h| brokerfile::Credential { host: h.clone(), phantom: c.phantom.clone() }))
        .collect();
    Some(BrokerSpec { listeners, proxy_listeners, files, proxy_env, credentials })
}

/// Of the claims a sandbox's features make (feature, claim), the ones `run`
/// wires. One feature per instance claims a credential: a second is
/// refused, naming both; so is a second intercept claim of an upstream (the
/// broker picks a connection's claim by its host), naming both features.
pub fn select(found: Found) -> Result<Found> {
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut intercepted: BTreeMap<String, String> = BTreeMap::new();
    for (feature, claim) in &found {
        if let Some((_, first)) = seen.iter().find(|(kind, _)| *kind == claim.kind) {
            bail!(
                "features `{first}` and `{feature}` both claim the `{}` credential: one feature per sandbox may",
                claim.kind
            );
        }
        seen.push((claim.kind.clone(), feature.clone()));
        if claim.pinned.mode != Mode::Intercept {
            continue;
        }
        for u in &claim.pinned.upstreams {
            if let Some(first) = intercepted.insert(u.host.clone(), feature.clone()) {
                bail!(
                    "features `{first}` and `{feature}` both intercept `{}`: one intercept claim per upstream in a sandbox",
                    u.host
                );
            }
        }
    }
    Ok(found)
}

/// What a claim's phantom starts with: its declared prefix, else
/// `dsbx_<kind>_`.
fn phantom_head(claim: &Claim) -> String {
    match &claim.wiring.phantom.prefix {
        Some(prefix) => prefix.clone(),
        None => format!("dsbx_{}_", claim.kind),
    }
}

/// A fresh phantom for `claim`: its head and [`PHANTOM_RANDOM`] base62
/// characters, safe in a header, a URL and a TOML string.
pub fn mint_phantom(claim: &Claim) -> Result<String> {
    Ok(format!("{}{}", phantom_head(claim), random_base62(PHANTOM_RANDOM)?))
}

/// An instance's incarnation (credential-broker, *Identity is the bridge*).
pub fn mint_incarnation() -> Result<String> {
    random_base62(PHANTOM_RANDOM)
}

fn random_base62(len: usize) -> Result<String> {
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];
    while out.len() < len {
        getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("cannot read the OS random source: {e}"))?;
        // 248 = 4 * 62: rejecting the rest keeps every character equally likely.
        for b in buf.iter().filter(|&&b| b < 248) {
            if out.len() == len {
                break;
            }
            out.push(BASE62[(b % 62) as usize] as char);
        }
    }
    Ok(out)
}

/// The records for `selected` (run's kept claims), files left to
/// [`render_files`], and the shared proxy port. Base-URL claims get a
/// listener per upstream; intercept claims none: their upstreams go on the
/// shared port, but for the hosts their `git` field names, which go on
/// git's own port (git-gateway). From `prior` (the instance being rebuilt)
/// each kind keeps its phantom, while it still has the claim's head, each
/// upstream its port, the shared port `prior_port` and git's
/// `prior_git_port`; new ones take the lowest free ports of [`PORTS`].
/// `mint` makes a new phantom.
pub fn plan(
    selected: &[(String, Claim)],
    prior: &[InstanceClaim],
    prior_port: Option<u16>,
    prior_git_port: Option<u16>,
    mut mint: impl FnMut(&Claim) -> Result<String>,
) -> Result<Planned> {
    let mut taken: BTreeSet<u16> = BTreeSet::new();
    let mut out = Vec::new();
    for (feature, claim) in selected {
        let before = prior.iter().find(|p| p.kind == claim.kind);
        let phantom = match before.filter(|p| p.phantom.starts_with(&phantom_head(claim))) {
            Some(p) => p.phantom.clone(),
            None => mint(claim)?,
        };
        let listened = if claim.pinned.mode == Mode::BaseUrl { claim.pinned.upstreams.as_slice() } else { &[] };
        let listeners = listened
            .iter()
            .map(|u| {
                let kept = before.and_then(|p| p.listeners.iter().find(|l| l.upstream == u.host)).map(|l| l.port);
                let port = kept.filter(|p| taken.insert(*p)).unwrap_or(0);
                Listener { port, upstream: u.host.clone() }
            })
            .collect();
        out.push(InstanceClaim {
            kind: claim.kind.clone(),
            feature: feature.clone(),
            version: claim.version(),
            claim: claim.canonical(),
            phantom,
            listeners,
            files: Vec::new(),
            git_hosts: claim.wiring.git.as_ref().map(|g| g.hosts.clone()).unwrap_or_default(),
        });
    }
    let intercepting: Vec<&Claim> =
        selected.iter().map(|(_, c)| c).filter(|c| c.pinned.mode == Mode::Intercept).collect();
    let git_of = |c: &Claim| c.wiring.git.as_ref().map(|g| g.hosts.clone()).unwrap_or_default();
    let hosts: BTreeSet<String> = intercepting
        .iter()
        .flat_map(|c| {
            let git = git_of(c);
            c.pinned.upstreams.iter().filter(move |u| !git.contains(&u.host)).map(|u| u.host.clone())
        })
        .collect();
    let git_hosts: BTreeSet<String> = intercepting.iter().flat_map(|c| git_of(c)).collect();
    let kept_port = prior_port.filter(|p| !intercepting.is_empty() && PORTS.contains(p) && taken.insert(*p));
    let kept_git = prior_git_port.filter(|p| !git_hosts.is_empty() && PORTS.contains(p) && taken.insert(*p));
    let mut free = || -> Result<u16> {
        let port = PORTS.clone().find(|p| !taken.contains(p)).context("out of credential broker ports")?;
        taken.insert(port);
        Ok(port)
    };
    for l in out.iter_mut().flat_map(|c| c.listeners.iter_mut()).filter(|l| l.port == 0) {
        l.port = free()?;
    }
    let proxy = match (intercepting.is_empty(), kept_port) {
        (true, _) => None,
        (false, Some(port)) => Some(ProxyPort { port, hosts: hosts.into_iter().collect() }),
        (false, None) => Some(ProxyPort { port: free()?, hosts: hosts.into_iter().collect() }),
    };
    let git = match (git_hosts.is_empty(), kept_git) {
        (true, _) => None,
        (false, Some(port)) => Some(ProxyPort { port, hosts: git_hosts.into_iter().collect() }),
        (false, None) => Some(ProxyPort { port: free()?, hosts: git_hosts.into_iter().collect() }),
    };
    Ok(Planned { claims: out, proxy, git })
}

/// The proxy variables `run` reads from the container's env.
pub const PROXY_VARS: [&str; 8] =
    ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy", "NO_PROXY", "no_proxy"];

/// Variables naming a CA bundle that add to a tool's roots
/// (`NODE_EXTRA_CA_CERTS`, `DENO_CERT`) or replace certifi's
/// (`REQUESTS_CA_BUNDLE`, `AWS_CA_BUNDLE`), each pointed at the system
/// bundle itself so a CA added to it later still counts. No
/// `SSL_CERT_FILE`: OpenSSL, libcurl, Go and Bun read the system bundle.
pub const CA_VARS: [&str; 4] = ["NODE_EXTRA_CA_CERTS", "DENO_CERT", "REQUESTS_CA_BUNDLE", "AWS_CA_BUNDLE"];

const LOOPBACK: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// The proxy env the container has without `run`'s: of `layers` (key,
/// value) in the order they apply (the image's `ENV`, then `containerEnv`,
/// then `--env`), each of [`PROXY_VARS`]' last value.
pub fn original_proxy_env<'a>(layers: impl IntoIterator<Item = (&'a String, &'a String)>) -> Vec<(String, String)> {
    let mut last: BTreeMap<&str, &str> = BTreeMap::new();
    for (k, v) in layers {
        if let Some(var) = PROXY_VARS.iter().find(|var| **var == k.as_str()) {
            last.insert(var, v);
        }
    }
    PROXY_VARS.iter().filter_map(|var| last.get(var).map(|v| (var.to_string(), v.to_string()))).collect()
}

/// `NO_PROXY`'s value: loopback, the services' `aliases`, then the
/// container's own `NO_PROXY` and `no_proxy` entries, each but the ones
/// that would exempt a bound host (it would skip the broker).
pub fn no_proxy(original: &[(String, String)], bound: &[String], aliases: &[String]) -> String {
    let theirs = original
        .iter()
        .filter(|(k, _)| k == "NO_PROXY" || k == "no_proxy")
        .flat_map(|(_, v)| v.split(','))
        .map(str::trim);
    let mut out: Vec<&str> = Vec::new();
    for entry in LOOPBACK.into_iter().chain(aliases.iter().map(String::as_str)).chain(theirs) {
        if !entry.is_empty() && !out.contains(&entry) && !bound.iter().any(|h| exempts(entry, h)) {
            out.push(entry);
        }
    }
    out.join(",")
}

/// Whether `NO_PROXY` entry `entry` (`*`, a name, `.name`, `*.name`, with
/// or without `:port`) exempts `host`, as curl and Go read it.
fn exempts(entry: &str, host: &str) -> bool {
    let entry = entry.to_ascii_lowercase();
    let name = match entry.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => entry.as_str(),
    };
    let name = name.trim_start_matches('*').trim_start_matches('.').trim_end_matches('.');
    name.is_empty() || host == name || host.strip_suffix(name).is_some_and(|rest| rest.ends_with('.'))
}

/// The container env an intercept side sets: both cases of `HTTPS_PROXY`
/// and `NO_PROXY` ([`no_proxy`], with the services' `aliases`),
/// `NODE_USE_ENV_PROXY`, and [`CA_VARS`] at the system bundle.
pub fn intercept_env(intercept: &Intercept, aliases: &[String]) -> Vec<(String, String)> {
    let proxy = format!("http://127.0.0.1:{}", intercept.port);
    let no_proxy = no_proxy(&intercept.proxy_env, &intercept.hosts, aliases);
    let mut out = vec![
        ("HTTPS_PROXY".to_string(), proxy.clone()),
        ("https_proxy".to_string(), proxy),
        ("NO_PROXY".to_string(), no_proxy.clone()),
        ("no_proxy".to_string(), no_proxy),
        ("NODE_USE_ENV_PROXY".to_string(), "1".to_string()),
    ];
    out.extend(CA_VARS.iter().map(|v| (v.to_string(), intercept.bundle.clone())));
    out
}

/// The proxy variables of `keys` (remoteEnv's, `--env`'s): set on every
/// exec, they'd send it around the shared port.
pub fn proxy_overrides<'a>(keys: impl IntoIterator<Item = &'a String>) -> Vec<&'a str> {
    let routed = ["HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy"];
    keys.into_iter().map(String::as_str).filter(|k| routed.contains(k)).collect()
}

/// `baseUrl`'s value: the listener standing for the claim's first upstream.
pub fn base_url(claim: &Claim, record: &InstanceClaim) -> Option<String> {
    let wanted = &claim.wiring.base_url.as_ref()?.upstream;
    let l = record.listeners.iter().find(|l| l.upstream == *wanted)?;
    Some(format!("http://127.0.0.1:{}", l.port))
}

/// The container env `claim` sets: each phantom variable, and `baseUrl`'s.
pub fn env(claim: &Claim, record: &InstanceClaim) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        claim.wiring.phantom.env.iter().map(|name| (name.clone(), record.phantom.clone())).collect();
    if let (Some(b), Some(url)) = (&claim.wiring.base_url, base_url(claim, record)) {
        out.push((b.env.clone(), url));
    }
    out
}

/// `claim`'s phantom files under `home` (the remote user's), owned by
/// `owner`, plus a warning per file skipped because it lands on or under a
/// mount or volume target in `targets`: a shared login mounted there would
/// be overwritten (and every other instance's calls flagged as leaks).
pub fn render_files(
    claim: &Claim,
    record: &InstanceClaim,
    home: &str,
    owner: &str,
    targets: &[String],
) -> (Vec<File>, Vec<String>) {
    let base_url = base_url(claim, record).unwrap_or_default();
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    for f in &claim.wiring.phantom.files {
        let path = format!("{}/{}", home.trim_end_matches('/'), f.path);
        if let Some(target) = targets.iter().find(|t| covers(t, &path)) {
            warnings.push(format!(
                "feature `{}`: `{path}` is under mount `{target}`: its `{}` phantom file is skipped",
                record.feature, claim.kind
            ));
            continue;
        }
        let content = f.template.replace(claim::BASE_URL, &base_url).replace(claim::PHANTOM, &record.phantom);
        files.push(File { path, owner: owner.to_string(), content });
    }
    (files, warnings)
}

/// Whether container path `path` is `target` or below it.
fn covers(target: &str, path: &str) -> bool {
    let t = target.trim_end_matches('/');
    path == t || path.strip_prefix(t).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentry() -> Claim {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/features/sentry/devcontainer-feature.json"
        ))
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        claim::parse("sentry", &v["customizations"]["devsandbox"]["credentials"]["sentry"]).unwrap()
    }

    fn intercept(kind: &str) -> Claim {
        intercept_of(kind, serde_json::json!(["api.github.com"]), None)
    }

    fn intercept_of(kind: &str, upstreams: serde_json::Value, git: Option<serde_json::Value>) -> Claim {
        let mut v = serde_json::json!({
            "upstreams": upstreams,
            "mode": "intercept",
            "inject": [{ "header": "Authorization", "format": "token {token}" }],
            "phantom": { "env": [format!("{}_TOKEN", kind.to_uppercase())] },
            "login": [{ "method": "paste" }],
        });
        if let Some(git) = git {
            v["git"] = git;
        }
        claim::parse(kind, &v).unwrap()
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn counter() -> impl FnMut(&Claim) -> Result<String> {
        let mut n = 0;
        move |c| {
            n += 1;
            Ok(format!("{}{n}", phantom_head(c)))
        }
    }

    #[test]
    fn select_refuses_a_second_claim_and_a_shared_intercepted_upstream() {
        let found = vec![("./a".to_string(), sentry()), ("./b".to_string(), sentry())];
        let err = select(found).unwrap_err().to_string();
        assert!(err.contains("features `./a` and `./b` both claim the `sentry` credential"), "{err}");

        let kept = select(vec![("./s".into(), sentry()), ("./gh".into(), intercept("github"))]).unwrap();
        assert_eq!(kept.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(), ["./s", "./gh"]);

        let other = intercept_of("ghe", serde_json::json!(["example.com", "api.github.com"]), None);
        let err = select(vec![("./gh".into(), intercept("github")), ("./ghe".into(), other)]).unwrap_err().to_string();
        assert!(err.contains("features `./gh` and `./ghe` both intercept `api.github.com`"), "{err}");
        // A base-URL claim of the same host is no conflict: it has its own listener.
        let base = sentry();
        let shared = intercept_of("x", serde_json::json!([base.pinned.upstreams[0].host.clone()]), None);
        select(vec![("./s".into(), base), ("./x".into(), shared)]).unwrap();
    }

    #[test]
    fn plan_allocates_ports_and_rebuild_keeps_phantoms_and_ports() {
        let selected = vec![("./s".to_string(), sentry())];
        let planned = plan(&selected, &[], None, None, counter()).unwrap();
        assert_eq!(planned.proxy, None);
        let first = planned.claims;
        let c = &first[0];
        assert_eq!((c.kind.as_str(), c.phantom.as_str()), ("sentry", "sntrys_1"));
        assert_eq!(c.version, sentry().version());
        // Upstreams sorted at parse; ports from the bottom of the range.
        let got: Vec<(u16, &str)> = c.listeners.iter().map(|l| (l.port, l.upstream.as_str())).collect();
        assert_eq!(got, [(29000, "de.sentry.io"), (29001, "sentry.io"), (29002, "us.sentry.io")]);

        let again = plan(&selected, &first, None, None, |_| panic!("kept")).unwrap().claims;
        assert_eq!(again, first);

        // A new version drops one upstream and adds one: the others keep
        // theirs, the new one takes the lowest free port.
        let mut prior = first.clone();
        prior[0].listeners.retain(|l| l.upstream != "de.sentry.io");
        let next = plan(&selected, &prior, None, None, counter()).unwrap().claims;
        let got: Vec<(u16, &str)> = next[0].listeners.iter().map(|l| (l.port, l.upstream.as_str())).collect();
        assert_eq!(got, [(29000, "de.sentry.io"), (29001, "sentry.io"), (29002, "us.sentry.io")]);
        assert_eq!(next[0].phantom, "sntrys_1");

        // A phantom without the claim's head is minted again.
        let mut odd = first.clone();
        odd[0].phantom = "other_x".into();
        assert_eq!(plan(&selected, &odd, None, None, counter()).unwrap().claims[0].phantom, "sntrys_1");
    }

    #[test]
    fn intercept_claims_share_one_proxy_port_kept_across_rebuilds() {
        let gh = intercept_of(
            "github",
            serde_json::json!(["api.github.com", "uploads.github.com", "github.com"]),
            Some(serde_json::json!({ "hosts": ["github.com"] })),
        );
        let other = intercept_of("x", serde_json::json!(["api.x.io"]), None);
        let selected = vec![("./s".to_string(), sentry()), ("./gh".to_string(), gh.clone()), ("./x".to_string(), other)];
        let first = plan(&selected, &[], None, None, counter()).unwrap();
        assert!(first.claims[1].listeners.is_empty() && first.claims[2].listeners.is_empty());
        assert_eq!(git_hosts(&first.claims), ["github.com"]);
        assert!(first.claims[0].git_hosts.is_empty());
        // git's host stays off the shared port (git-gateway gives git its own).
        assert_eq!(
            first.proxy,
            Some(ProxyPort {
                port: 29003,
                hosts: vec!["api.github.com".into(), "api.x.io".into(), "uploads.github.com".into()]
            })
        );
        assert_eq!(first.git, Some(ProxyPort { port: 29004, hosts: vec!["github.com".into()] }));

        // A rebuild whose sentry claim gained an upstream: both proxy ports
        // keep their numbers, the new listener takes the next free one.
        let mut prior = first.claims.clone();
        prior[0].listeners.retain(|l| l.upstream != "us.sentry.io");
        let next = plan(&selected, &prior, Some(29002), Some(29004), counter()).unwrap();
        assert_eq!(next.proxy.as_ref().map(|p| p.port), Some(29002));
        assert_eq!(next.git.as_ref().map(|p| p.port), Some(29004));
        let us = next.claims[0].listeners.iter().find(|l| l.upstream == "us.sentry.io").unwrap();
        assert_eq!(us.port, 29003);
        let again = plan(&selected, &first.claims, Some(29003), Some(29004), counter()).unwrap();
        assert_eq!((again.proxy, again.git), (first.proxy.clone(), first.git.clone()));
        // An instance from before git's port gets one, the lowest free.
        let upgraded = plan(&selected, &first.claims, Some(29003), None, counter()).unwrap();
        assert_eq!(upgraded.git.map(|g| g.port), Some(29004));
        // A kept git port out of the range is replaced.
        let moved = plan(&selected, &first.claims, Some(29003), Some(80), counter()).unwrap();
        assert_eq!(moved.git.map(|g| g.port), Some(29004));

        let git_only = intercept_of("g", serde_json::json!(["github.com"]), Some(serde_json::json!({ "hosts": ["github.com"] })));
        let planned = plan(&[("./g".into(), git_only)], &[], Some(29000), Some(29001), counter()).unwrap();
        assert_eq!(planned.proxy, Some(ProxyPort { port: 29000, hosts: vec![] }));
        assert_eq!(planned.git, Some(ProxyPort { port: 29001, hosts: vec!["github.com".into()] }));
        // No git field: no git port, whatever was kept.
        assert_eq!(plan(&selected[2..], &[], None, Some(29001), counter()).unwrap().git, None);
    }

    #[test]
    fn original_proxy_env_takes_each_variables_last_layer() {
        let image = vars(&[("HTTPS_PROXY", "http://img:1"), ("PATH", "/bin"), ("no_proxy", "corp"), ("HTTPS_PROXY", "http://img:2")]);
        let config: BTreeMap<String, String> =
            [("HTTP_PROXY".to_string(), "http://cfg:3".to_string()), ("https_proxy".into(), "http://cfg:4".into())].into();
        let layers = image.iter().map(|(k, v)| (k, v)).chain(config.iter());
        assert_eq!(
            original_proxy_env(layers),
            vars(&[("HTTPS_PROXY", "http://img:2"), ("https_proxy", "http://cfg:4"), ("HTTP_PROXY", "http://cfg:3"), ("no_proxy", "corp")])
        );
        assert!(original_proxy_env(std::iter::empty()).is_empty());
    }

    #[test]
    fn no_proxy_is_loopback_aliases_and_theirs_minus_bound_hosts() {
        let bound = vec!["api.github.com".to_string(), "uploads.github.com".to_string()];
        let original = vars(&[
            ("HTTPS_PROXY", "http://corp:3128"),
            ("NO_PROXY", "corp.lan, .github.com,db,api.github.com:443"),
            ("no_proxy", "*,10.0.0.0/8,localhost,github.com,xapi.github.com,*.GitHub.com"),
        ]);
        assert_eq!(
            no_proxy(&original, &bound, &["db".into(), "cache".into()]),
            "localhost,127.0.0.1,::1,db,cache,corp.lan,10.0.0.0/8,xapi.github.com"
        );
        assert_eq!(no_proxy(&[], &bound, &[]), "localhost,127.0.0.1,::1");
        assert_eq!(no_proxy(&[], &["localhost".into()], &["api".into()]), "127.0.0.1,::1,api");
    }

    #[test]
    fn intercept_env_routes_https_and_names_the_bundle() {
        let i = Intercept {
            port: 29004,
            hosts: vec!["api.github.com".into()],
            git: None,
            proxy_env: vars(&[("https_proxy", "http://corp:3128"), ("NO_PROXY", "corp")]),
            bundle: "/etc/pki/tls/certs/ca-bundle.crt".into(),
            installed: None,
        };
        let env = intercept_env(&i, &["db".into()]);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("HTTPS_PROXY"), Some("http://127.0.0.1:29004"));
        assert_eq!(get("https_proxy"), Some("http://127.0.0.1:29004"));
        assert_eq!(get("NO_PROXY"), Some("localhost,127.0.0.1,::1,db,corp"));
        assert_eq!(get("no_proxy"), get("NO_PROXY"));
        assert_eq!(get("NODE_USE_ENV_PROXY"), Some("1"));
        for var in CA_VARS {
            assert_eq!(get(var), Some("/etc/pki/tls/certs/ca-bundle.crt"), "{var}");
        }
        assert!(get("SSL_CERT_FILE").is_none() && get("HTTP_PROXY").is_none());

        let keys = ["HTTPS_PROXY".to_string(), "FOO".into(), "no_proxy".into(), "HTTP_PROXY".into()];
        assert_eq!(proxy_overrides(&keys), ["HTTPS_PROXY", "no_proxy"]);
    }

    #[test]
    fn minted_values_are_long_random_and_plain() {
        let p = mint_phantom(&sentry()).unwrap();
        let rest = p.strip_prefix("sntrys_").unwrap();
        assert_eq!(rest.len(), PHANTOM_RANDOM);
        assert!(rest.bytes().all(|b| b.is_ascii_alphanumeric()), "{p}");
        assert_ne!(p, mint_phantom(&sentry()).unwrap());
        let gh = mint_phantom(&intercept("github")).unwrap();
        assert!(gh.starts_with("dsbx_github_"), "{gh}");
        assert_eq!(mint_incarnation().unwrap().len(), PHANTOM_RANDOM);
    }

    #[test]
    fn env_and_files_render_and_mount_targets_skip() {
        let claim = sentry();
        let rec = plan(&[("./s".into(), claim.clone())], &[], None, None, counter()).unwrap().claims.remove(0);
        assert_eq!(
            env(&claim, &rec),
            [
                ("SENTRY_AUTH_TOKEN".to_string(), "sntrys_1".to_string()),
                ("SENTRY_URL".to_string(), "http://127.0.0.1:29001".to_string())
            ]
        );
        let (files, warnings) = render_files(&claim, &rec, "/home/dev/", "dev", &["/workspaces/x".into()]);
        assert!(warnings.is_empty());
        assert_eq!(
            files,
            [File {
                path: "/home/dev/.sentryclirc".into(),
                owner: "dev".into(),
                content: "[defaults]\nurl = http://127.0.0.1:29001\n\n[auth]\ntoken = sntrys_1\n".into(),
            }]
        );
        for target in ["/home/dev", "/home/dev/", "/", "/home/dev/.sentryclirc"] {
            let (files, warnings) = render_files(&claim, &rec, "/home/dev", "dev", &[target.into()]);
            assert!(files.is_empty(), "{target}");
            assert!(warnings[0].contains("phantom file is skipped"), "{warnings:?}");
        }
        let (files, _) = render_files(&claim, &rec, "/home/dev", "dev", &["/home/devx".into()]);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn lookups_and_broker_spec() {
        let selected = vec![("./s".to_string(), sentry()), ("./gh".to_string(), intercept("github"))];
        let planned = plan(&selected, &[], None, None, counter()).unwrap();
        let mut rec = planned.claims;
        let (files, _) = render_files(&sentry(), &rec[0], "/home/dev", "dev", &[]);
        rec[0].files = files;
        assert_eq!(by_phantom(&rec, "sntrys_1").map(|c| c.kind.as_str()), Some("sentry"));
        assert!(by_phantom(&rec, "sntrys_2").is_none());
        assert_eq!(by_port(&rec, 29002).map(|(_, u)| u), Some("us.sentry.io"));
        let shared = planned.proxy.unwrap();
        let i = Intercept {
            port: shared.port,
            hosts: shared.hosts,
            git: None,
            proxy_env: vars(&[("HTTPS_PROXY", "http://corp:3128")]),
            bundle: "/b".into(),
            installed: None,
        };
        assert_eq!(ports(&rec, None).collect::<Vec<_>>(), [29000, 29001, 29002]);
        assert_eq!(ports(&rec, Some(&i)).collect::<Vec<_>>(), [29000, 29001, 29002, 29003]);
        assert!(forward_refusal(&rec, None, "web", 29001).unwrap().contains("`sentry` credential broker listener"));
        assert!(forward_refusal(&rec, Some(&i), "web", 29003).unwrap().contains("credential broker proxy port"));
        assert!(forward_refusal(&rec, None, "web", 29003).is_none());
        assert!(forward_refusal(&rec, Some(&i), "web", 3000).is_none());

        assert_eq!(broker_spec(&[], None), None);
        let spec = broker_spec(&rec, None).unwrap();
        let text = brokerfile::serialize(&spec);
        assert_eq!(brokerfile::parse(&text), Ok(spec.clone()));
        assert!(text.starts_with("listen 29000 base-url de.sentry.io\n"), "{text}");
        assert_eq!(spec.files[0].phantom, "sntrys_1");
        assert_eq!(spec.files[0].path, "/home/dev/.sentryclirc");

        let text = brokerfile::serialize(&broker_spec(&rec, Some(&i)).unwrap());
        assert!(text.contains("\nproxy-listen 29003 api.github.com\n"), "{text}");
        assert!(text.ends_with("\nproxy HTTPS_PROXY=http://corp:3128\n"), "{text}");
        assert!(brokerfile::parse(&text).is_ok());
        assert!(!text.contains("credential "), "no git hosts: {text}");
    }

    #[test]
    fn git_port_is_a_broker_port_with_the_phantom_for_the_helper() {
        let gh = intercept_of(
            "github",
            serde_json::json!(["api.github.com", "github.com"]),
            Some(serde_json::json!({ "hosts": ["github.com"] })),
        );
        let planned = plan(&[("./gh".into(), gh)], &[], None, None, counter()).unwrap();
        let (shared, git) = (planned.proxy.unwrap(), planned.git.unwrap());
        let i = Intercept {
            port: shared.port,
            hosts: shared.hosts,
            git: Some(git),
            proxy_env: Vec::new(),
            bundle: "/b".into(),
            installed: None,
        };
        assert_eq!(i.bound_hosts(), ["api.github.com", "github.com"]);
        assert_eq!(ports(&planned.claims, Some(&i)).collect::<Vec<_>>(), [29000, 29001]);
        assert!(forward_refusal(&planned.claims, Some(&i), "web", 29001).unwrap().contains("git's credential broker proxy port"));
        let text = brokerfile::serialize(&broker_spec(&planned.claims, Some(&i)).unwrap());
        assert_eq!(
            text,
            "proxy-listen 29000 api.github.com\nproxy-listen 29001 github.com\ncredential github.com dsbx_github_1\n"
        );
        assert!(brokerfile::parse(&text).is_ok());
    }

    #[test]
    fn records_round_trip_through_toml() {
        let mut rec = plan(&[("./s".into(), sentry())], &[], None, None, counter()).unwrap().claims;
        rec[0].files = render_files(&sentry(), &rec[0], "/home/dev", "dev", &[]).0;
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            claims: Vec<InstanceClaim>,
            intercept: Option<Intercept>,
        }
        let intercept = Intercept {
            port: 29003,
            hosts: vec!["api.github.com".into()],
            git: Some(ProxyPort { port: 29004, hosts: vec!["github.com".into()] }),
            proxy_env: vars(&[("HTTPS_PROXY", "http://corp:3128"), ("NO_PROXY", "")]),
            bundle: "/etc/ssl/certs/ca-certificates.crt".into(),
            installed: Some("ab".repeat(32)),
        };
        let w = Wrap { claims: rec, intercept: Some(intercept) };
        let text = toml::to_string(&w).unwrap();
        assert_eq!(toml::from_str::<Wrap>(&text).unwrap(), w);
    }
}
