//! What an instance carries for its credential claims (credential-broker,
//! *Phantoms*, *In the container*): per claim its phantom, its loopback
//! listeners and its phantom files, made at `run`, kept by `rebuild`, saved
//! on the `Instance` in `state.toml` and gone with it at `rm`. Pure: `run`
//! (`commands::run::claims`) does the I/O.
//!
//! The broker reads these per call: the bridge names the instance, its
//! claims give the phantom table ([`by_phantom`]) and each listener's
//! upstream ([`by_port`]); the claim's `version` selects the home's approved
//! copy. Which home entry serves a claim isn't kept: profiles and config
//! change, so it's resolved per call (`credentials::home::resolve`).

use std::collections::BTreeSet;
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    /// On the container's `127.0.0.1`.
    pub port: u16,
    pub upstream: String,
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

pub fn ports(claims: &[InstanceClaim]) -> impl Iterator<Item = u16> + '_ {
    claims.iter().flat_map(|c| c.listeners.iter().map(|l| l.port))
}

/// `forwards.*`' refusal of a container port that's a broker listener: the
/// listener answers only the container's own clients.
pub fn forward_refusal(claims: &[InstanceClaim], key: &str, port: u16) -> Option<String> {
    let (claim, _) = by_port(claims, port)?;
    Some(format!(
        "port {port} of `{key}` is its `{}` credential broker listener, which only the sandbox itself may reach",
        claim.kind
    ))
}

/// The helper's broker file for `claims`; `None` without any.
pub fn broker_spec(claims: &[InstanceClaim]) -> Option<BrokerSpec> {
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
    Some(BrokerSpec { listeners, files, proxy_env: Vec::new() })
}

/// Of the claims a sandbox's features make (feature, claim), the ones `run`
/// wires, plus a warning per one it skips. One feature per instance claims
/// a credential: a second is refused, naming both.
pub fn select(found: Found) -> Result<(Found, Vec<String>)> {
    let mut kept: Found = Vec::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut warnings = Vec::new();
    for (feature, claim) in found {
        if let Some((_, first)) = seen.iter().find(|(kind, _)| *kind == claim.kind) {
            bail!(
                "features `{first}` and `{feature}` both claim the `{}` credential: one feature per sandbox may",
                claim.kind
            );
        }
        seen.push((claim.kind.clone(), feature.clone()));
        if claim.pinned.mode == Mode::Intercept {
            warnings.push(format!(
                "feature `{feature}`: the `{}` claim is intercept mode, not built yet (credential-broker step 3): skipped",
                claim.kind
            ));
            continue;
        }
        kept.push((feature, claim));
    }
    Ok((kept, warnings))
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
/// [`render_files`]. From `prior` (the instance being rebuilt) each kind
/// keeps its phantom, while it still has the claim's head, and each
/// upstream its port; new upstreams take the lowest free ports of
/// [`PORTS`]. `mint` makes a new phantom.
pub fn plan(
    selected: &[(String, Claim)],
    prior: &[InstanceClaim],
    mut mint: impl FnMut(&Claim) -> Result<String>,
) -> Result<Vec<InstanceClaim>> {
    let mut taken: BTreeSet<u16> = BTreeSet::new();
    let mut out = Vec::new();
    for (feature, claim) in selected {
        let before = prior.iter().find(|p| p.kind == claim.kind);
        let phantom = match before.filter(|p| p.phantom.starts_with(&phantom_head(claim))) {
            Some(p) => p.phantom.clone(),
            None => mint(claim)?,
        };
        let listeners = claim
            .pinned
            .upstreams
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
        });
    }
    for l in out.iter_mut().flat_map(|c| c.listeners.iter_mut()).filter(|l| l.port == 0) {
        let port = PORTS.clone().find(|p| !taken.contains(p)).context("out of credential broker ports")?;
        taken.insert(port);
        l.port = port;
    }
    Ok(out)
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
        let v = serde_json::json!({
            "upstreams": ["api.github.com"],
            "mode": "intercept",
            "inject": [{ "header": "Authorization", "format": "token {token}" }],
            "phantom": { "env": ["GH_TOKEN"] },
            "login": [{ "method": "paste" }],
        });
        claim::parse(kind, &v).unwrap()
    }

    fn counter() -> impl FnMut(&Claim) -> Result<String> {
        let mut n = 0;
        move |c| {
            n += 1;
            Ok(format!("{}{n}", phantom_head(c)))
        }
    }

    #[test]
    fn select_refuses_a_second_claim_and_skips_intercept() {
        let found = vec![("./a".to_string(), sentry()), ("./b".to_string(), sentry())];
        let err = select(found).unwrap_err().to_string();
        assert!(err.contains("features `./a` and `./b` both claim the `sentry` credential"), "{err}");

        let (kept, warnings) = select(vec![("./s".into(), sentry()), ("./gh".into(), intercept("github"))]).unwrap();
        assert_eq!(kept.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(), ["./s"]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("`./gh`") && warnings[0].contains("credential-broker step 3"), "{warnings:?}");
    }

    #[test]
    fn plan_allocates_ports_and_rebuild_keeps_phantoms_and_ports() {
        let selected = vec![("./s".to_string(), sentry())];
        let first = plan(&selected, &[], counter()).unwrap();
        let c = &first[0];
        assert_eq!((c.kind.as_str(), c.phantom.as_str()), ("sentry", "sntrys_1"));
        assert_eq!(c.version, sentry().version());
        // Upstreams sorted at parse; ports from the bottom of the range.
        let got: Vec<(u16, &str)> = c.listeners.iter().map(|l| (l.port, l.upstream.as_str())).collect();
        assert_eq!(got, [(29000, "de.sentry.io"), (29001, "sentry.io"), (29002, "us.sentry.io")]);

        let again = plan(&selected, &first, |_| panic!("kept")).unwrap();
        assert_eq!(again, first);

        // A new version drops one upstream and adds one: the others keep
        // theirs, the new one takes the lowest free port.
        let mut prior = first.clone();
        prior[0].listeners.retain(|l| l.upstream != "de.sentry.io");
        let next = plan(&selected, &prior, counter()).unwrap();
        let got: Vec<(u16, &str)> = next[0].listeners.iter().map(|l| (l.port, l.upstream.as_str())).collect();
        assert_eq!(got, [(29000, "de.sentry.io"), (29001, "sentry.io"), (29002, "us.sentry.io")]);
        assert_eq!(next[0].phantom, "sntrys_1");

        // A phantom without the claim's head is minted again.
        let mut odd = first.clone();
        odd[0].phantom = "other_x".into();
        assert_eq!(plan(&selected, &odd, counter()).unwrap()[0].phantom, "sntrys_1");
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
        let rec = plan(&[("./s".into(), claim.clone())], &[], counter()).unwrap().remove(0);
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
        let mut rec = plan(&[("./s".into(), sentry())], &[], counter()).unwrap();
        let (files, _) = render_files(&sentry(), &rec[0], "/home/dev", "dev", &[]);
        rec[0].files = files;
        assert_eq!(by_phantom(&rec, "sntrys_1").map(|c| c.kind.as_str()), Some("sentry"));
        assert!(by_phantom(&rec, "sntrys_2").is_none());
        assert_eq!(by_port(&rec, 29002).map(|(_, u)| u), Some("us.sentry.io"));
        assert_eq!(ports(&rec).collect::<Vec<_>>(), [29000, 29001, 29002]);
        assert!(forward_refusal(&rec, "web", 29001).unwrap().contains("`sentry` credential broker listener"));
        assert!(forward_refusal(&rec, "web", 3000).is_none());

        assert_eq!(broker_spec(&[]), None);
        let spec = broker_spec(&rec).unwrap();
        let text = brokerfile::serialize(&spec);
        assert_eq!(brokerfile::parse(&text), Ok(spec.clone()));
        assert!(text.starts_with("listen 29000 base-url de.sentry.io\n"), "{text}");
        assert_eq!(spec.files[0].phantom, "sntrys_1");
        assert_eq!(spec.files[0].path, "/home/dev/.sentryclirc");
    }

    #[test]
    fn records_round_trip_through_toml() {
        let mut rec = plan(&[("./s".into(), sentry())], &[], counter()).unwrap();
        rec[0].files = render_files(&sentry(), &rec[0], "/home/dev", "dev", &[]).0;
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            claims: Vec<InstanceClaim>,
        }
        let w = Wrap { claims: rec };
        let text = toml::to_string(&w).unwrap();
        assert_eq!(toml::from_str::<Wrap>(&text).unwrap(), w);
    }
}
