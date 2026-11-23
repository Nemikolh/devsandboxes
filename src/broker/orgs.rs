//! The org fence's ids, learned at the home (github-and-sentry-access,
//! *Sentry's org fence*; docs/architecture/credentials/broker.md#the-org-fence).
//!
//! A fenced org is named by slug in `credentials.toml` and the config, but a
//! tool may name it by its numeric id (the `sentry` CLI reads one from a
//! DSN). The home asks the claim's `orgs.resolve` route for each allowed
//! slug's id, with the credential, on the call's own upstream: never the
//! id the sandbox named, so an id outside the set costs no call of its own.
//!
//! [`place`] is the fence's decision, pure over an id lookup; [`Live`] is
//! the lookup `serve` runs, cached per (credential, upstream, slug)
//! ([`Cache`]) over a [`Transport`] ([`Https`]).

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Read};
use std::sync::Mutex;

use serde_json::Value;

use super::github::{Answer, CACHE_CAP, FAILED_TTL, TTL};
use super::head::Header;
use super::upstream::{Body, Request, Upstream};
use crate::credentials::claim::{self, Slot};
use crate::credentials::store::Secret;

const MAX_BODY: u64 = 1024 * 1024;

/// Whose credential a lookup goes out with, and where.
#[derive(Debug, Clone, Copy)]
pub struct Via<'a> {
    pub credential: &'a str,
    /// The call's upstream host: an org's id is the region's that answers.
    pub upstream: &'a str,
    /// The approved claim marks `upstream` private.
    pub private: bool,
    /// The claim's slot the call carried the phantom in.
    pub slot: &'a Slot,
}

/// What the home asks the upstream. `None`: no answer naming an id.
pub trait Lookup {
    /// `orgs.resolve` for `slug`, its `orgs.id` field.
    fn id_of(&self, via: &Via<'_>, orgs: &claim::Orgs, slug: &str) -> Option<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    In,
    /// The credential's, left out by the config's `access.<kind>.orgs`:
    /// the slug as the credential spells it.
    NotInConfig(String),
    Out,
}

/// Sentry's slugs are never all digits, so such a value is an id.
pub fn is_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// `value` (a path segment or a list element's field) against `allowed`,
/// the credential's `credential` set narrowed by the config: an allowed
/// slug in any case, or the id of one. An id is matched only by resolving
/// the credential's own slugs, allowed ones first.
pub fn place(value: &str, credential: &[String], allowed: &[String], id_of: &dyn Fn(&str) -> Option<String>) -> Place {
    let same = |a: &String| a.eq_ignore_ascii_case(value);
    if allowed.iter().any(same) {
        return Place::In;
    }
    if let Some(slug) = credential.iter().find(|s| same(s)) {
        return Place::NotInConfig(slug.clone());
    }
    if !is_id(value) {
        return Place::Out;
    }
    let rest = credential.iter().filter(|s| !allowed.iter().any(|a| a.eq_ignore_ascii_case(s)));
    for slug in allowed.iter().chain(rest) {
        if id_of(slug).as_deref() == Some(value) {
            return match allowed.contains(slug) {
                true => Place::In,
                false => Place::NotInConfig(slug.clone()),
            };
        }
    }
    Place::Out
}

/// The credential's orgs, narrowed by the config's `access.<kind>.orgs`
/// when it names any.
pub fn allowed(credential: &[String], config: &[String]) -> Vec<String> {
    credential
        .iter()
        .filter(|s| config.is_empty() || config.iter().any(|c| c.eq_ignore_ascii_case(s)))
        .cloned()
        .collect()
}

/// `orgs.resolve`'s path for `slug`. `None` for a slug that isn't one
/// segment of letters, digits, `_` and `-`: a hand-edited
/// `credentials.toml` never steers the credential to another route.
pub fn resolve_path(orgs: &claim::Orgs, slug: &str) -> Option<String> {
    let claim::Route::Http { path, .. } = &orgs.resolve else { return None };
    let safe = !slug.is_empty() && slug.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    safe.then(|| path.replace(&orgs.var, slug))
}

/// `field` of a JSON object: Sentry answers ids as strings, a number is
/// taken too.
pub fn parse_id(body: &[u8], field: &str) -> Option<String> {
    let doc: Value = serde_json::from_slice(body).ok()?;
    let id = match doc.get(field)? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_u64()?.to_string(),
        _ => return None,
    };
    is_id(&id).then_some(id)
}

/// One credentialed `GET` to `via.upstream`.
pub trait Transport {
    fn get(&self, via: &Via<'_>, path: &str) -> io::Result<Answer>;
}

/// Answers per (credential, upstream, slug) until their expiry: what a
/// credential sees is its own, and a region answers for its own orgs.
#[derive(Default)]
pub struct Cache {
    map: HashMap<(String, String, String), (Option<String>, u64)>,
}

impl Cache {
    fn get(&self, key: &(String, String, String), now: u64) -> Option<Option<String>> {
        self.map.get(key).filter(|(_, until)| now < *until).map(|(id, _)| id.clone())
    }

    fn put(&mut self, key: (String, String, String), id: Option<String>, until: u64, now: u64) {
        if self.map.len() >= CACHE_CAP {
            self.map.retain(|_, (_, u)| now < *u);
            if self.map.len() >= CACHE_CAP {
                self.map.clear();
            }
        }
        self.map.insert(key, (id, until));
    }
}

/// The lookup `serve` runs: [`Cache`] in front of a [`Transport`].
pub struct Live<'a> {
    pub cache: &'a Mutex<Cache>,
    pub transport: &'a dyn Transport,
    pub now: u64,
}

impl Lookup for Live<'_> {
    fn id_of(&self, via: &Via<'_>, orgs: &claim::Orgs, slug: &str) -> Option<String> {
        let path = resolve_path(orgs, slug)?;
        let key = (via.credential.to_string(), via.upstream.to_string(), slug.to_ascii_lowercase());
        let lock = || self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = lock().get(&key, self.now) {
            return hit;
        }
        let id = match self.transport.get(via, &path) {
            Ok(a) if a.status == 200 => parse_id(&a.body, &orgs.id),
            _ => None,
        };
        let ttl = if id.is_some() { TTL } else { FAILED_TTL };
        lock().put(key, id.clone(), self.now + ttl, self.now);
        id
    }
}

/// [`Transport`] over the broker's upstream client, the token in the
/// claim's slot: `authorities` dialed instead of an upstream (tests' local
/// servers), `private_hosts` dialed despite a private address.
pub struct Https<'a> {
    pub upstream: &'a Upstream,
    pub authorities: &'a HashMap<String, String>,
    pub private_hosts: &'a BTreeSet<String>,
    pub token: &'a dyn Fn(&str) -> Option<Secret>,
}

impl Transport for Https<'_> {
    fn get(&self, via: &Via<'_>, path: &str) -> io::Result<Answer> {
        let token = (self.token)(via.credential)
            .ok_or_else(|| io::Error::other(format!("no token for credential `{}`", via.credential)))?;
        let field = |name: &str, value: &str| Header { name: name.into(), value: value.as_bytes().to_vec() };
        let mut path = path.to_string();
        let mut headers = vec![field("Accept", "application/json"), field("User-Agent", "devsandbox-broker")];
        match via.slot {
            Slot::Header { name, .. } => headers.push(field(name, "")),
            Slot::Basic { .. } => headers.push(field("Authorization", "")),
            Slot::Query { param } => {
                path.push(if path.contains('?') { '&' } else { '?' });
                path.push_str(param);
                path.push('=');
            }
        }
        super::policy::inject(&mut headers, &mut path, via.slot, token.expose());
        let authority = self.authorities.get(via.upstream).map_or(via.upstream, String::as_str);
        let url = format!("https://{authority}{path}");
        let private = via.private || self.private_hosts.contains(via.upstream);
        let resp = self.upstream.send(Request { method: "GET", url: &url, headers: &headers, body: Body::Empty, private })?;
        let mut out = Vec::new();
        resp.body.take(MAX_BODY).read_to_end(&mut out)?;
        Ok(Answer { status: resp.status, location: None, body: out })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn fence() -> claim::Orgs {
        claim::Orgs {
            var: "{org}".into(),
            resolve: claim::Route::Http { method: claim::Method::Get, path: "/api/0/organizations/{org}/".into() },
            id: "id".into(),
            lists: Default::default(),
        }
    }

    fn ids(slug: &str) -> Option<String> {
        match slug {
            "acme" => Some("11".into()),
            "beta" => Some("22".into()),
            _ => None,
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn places_slugs_and_ids() {
        let cred = s(&["acme", "Beta"]);
        let narrowed = allowed(&cred, &s(&["ACME"]));
        assert_eq!(narrowed, s(&["acme"]));
        assert_eq!(allowed(&cred, &[]), cred);
        assert_eq!(place("AcMe", &cred, &narrowed, &ids), Place::In);
        assert_eq!(place("beta", &cred, &narrowed, &ids), Place::NotInConfig("Beta".into()));
        assert_eq!(place("gamma", &cred, &narrowed, &ids), Place::Out);
        assert_eq!(place("11", &cred, &narrowed, &ids), Place::In);
        assert_eq!(place("33", &cred, &narrowed, &ids), Place::Out);
        assert_eq!(place("11", &cred, &narrowed, &|_| None), Place::Out, "a failed lookup is no match");
        // `Beta` resolved as the credential spells it.
        let spelled = |slug: &str| (slug == "Beta").then(|| "22".to_string());
        assert_eq!(place("22", &cred, &narrowed, &spelled), Place::NotInConfig("Beta".into()));
        assert!(!is_id("") && !is_id("1a") && is_id("007"));
    }

    #[test]
    fn an_id_lookup_asks_only_the_credentials_slugs() {
        let asked = RefCell::new(Vec::new());
        let id_of = |slug: &str| {
            asked.borrow_mut().push(slug.to_string());
            None
        };
        assert_eq!(place("99", &s(&["acme"]), &s(&["acme"]), &id_of), Place::Out);
        assert_eq!(*asked.borrow(), s(&["acme"]));
        asked.borrow_mut().clear();
        assert_eq!(place("not-an-id", &s(&["acme"]), &s(&["acme"]), &id_of), Place::Out);
        assert!(asked.borrow().is_empty(), "a slug outside the set is never looked up");
    }

    #[test]
    fn resolve_paths_and_ids() {
        let f = fence();
        assert_eq!(resolve_path(&f, "acme-2").as_deref(), Some("/api/0/organizations/acme-2/"));
        for bad in ["", "a/b", "a%2f", "..", "a?b"] {
            assert_eq!(resolve_path(&f, bad), None, "{bad}");
        }
        assert_eq!(parse_id(br#"{"id":"11","slug":"acme"}"#, "id").as_deref(), Some("11"));
        assert_eq!(parse_id(br#"{"id":11}"#, "id").as_deref(), Some("11"));
        for bad in [&br#"{"id":"x1"}"#[..], br#"{"id":-1}"#, br#"{"uuid":"1"}"#, br#"[{"id":"1"}]"#, b"nope"] {
            assert_eq!(parse_id(bad, "id"), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    struct Fake {
        calls: RefCell<Vec<(String, String)>>,
        answer: Answer,
    }

    impl Transport for Fake {
        fn get(&self, via: &Via<'_>, path: &str) -> io::Result<Answer> {
            self.calls.borrow_mut().push((via.upstream.to_string(), path.to_string()));
            Ok(self.answer.clone())
        }
    }

    #[test]
    fn live_caches_per_credential_upstream_and_slug() {
        let slot = Slot::Header { name: "authorization".into(), format: "Bearer {token}".into() };
        let via = |credential, upstream| Via { credential, upstream, private: false, slot: &slot };
        let fake = Fake { calls: RefCell::default(), answer: Answer { status: 200, location: None, body: br#"{"id":"11"}"#.to_vec() } };
        let cache = Mutex::default();
        let live = |now| Live { cache: &cache, transport: &fake, now };
        let f = fence();
        assert_eq!(live(0).id_of(&via("a", "us.sentry.io"), &f, "acme").as_deref(), Some("11"));
        assert_eq!(live(1).id_of(&via("a", "us.sentry.io"), &f, "ACME").as_deref(), Some("11"));
        assert_eq!(fake.calls.borrow().len(), 1, "cached, case-folded");
        live(1).id_of(&via("b", "us.sentry.io"), &f, "acme");
        live(1).id_of(&via("a", "de.sentry.io"), &f, "acme");
        assert_eq!(fake.calls.borrow().len(), 3, "per credential and upstream");
        assert_eq!(fake.calls.borrow()[2], ("de.sentry.io".to_string(), "/api/0/organizations/acme/".to_string()));
        live(TTL).id_of(&via("a", "us.sentry.io"), &f, "acme");
        assert_eq!(fake.calls.borrow().len(), 4, "expired");
        assert_eq!(live(0).id_of(&via("a", "us.sentry.io"), &f, "a/b"), None);
        assert_eq!(fake.calls.borrow().len(), 4, "an unsafe slug is never sent");

        let failing = Fake { calls: RefCell::default(), answer: Answer { status: 500, location: None, body: Vec::new() } };
        let cache = Mutex::default();
        let live = |now| Live { cache: &cache, transport: &failing, now };
        assert_eq!(live(0).id_of(&via("a", "us.sentry.io"), &f, "acme"), None);
        live(FAILED_TTL - 1).id_of(&via("a", "us.sentry.io"), &f, "acme");
        assert_eq!(failing.calls.borrow().len(), 1, "a failure is cached briefly");
        live(FAILED_TTL).id_of(&via("a", "us.sentry.io"), &f, "acme");
        assert_eq!(failing.calls.borrow().len(), 2);
    }
}
