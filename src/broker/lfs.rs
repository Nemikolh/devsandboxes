//! LFS transfers through the broker (docs/architecture/credentials/git.md,
//! *LFS*). An injected batch response's actions carry credentials in their
//! `header` (GitHub's `RemoteAuth`, S3 signatures, an echoed
//! `Authorization` holding the real token), and their `href`s name storage
//! hosts no claim intercepts. [`rewrite_batch`] keeps each action's `href`
//! and `header` in the [`Store`] under a fresh handle and points the action
//! at the instance's LFS listener instead, without its `header`; `serve`
//! forwards what reaches a handle with that header added. Pure over `now`
//! (unix seconds).

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::upstream::{self, Target};
use crate::credentials::instance;

/// Handles one instance holds at most; a new one past it evicts the one
/// expiring first.
pub const MAX_PER_INSTANCE: usize = 4096;
/// An action naming no expiry lives this long.
pub const DEFAULT_TTL: u64 = 3600;
/// No handle outlives this, whatever its action says.
pub const MAX_TTL: u64 = 24 * 3600;
/// Base62 characters in a handle: ~190 bits.
const HANDLE_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Download,
    Upload,
    Verify,
}

const ACTIONS: [Action; 3] = [Action::Download, Action::Upload, Action::Verify];

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Download => "download",
            Action::Upload => "upload",
            Action::Verify => "verify",
        }
    }

    /// What git-lfs's `basic` transfer sends to it, the only method its
    /// handle takes.
    pub fn method(self) -> &'static str {
        match self {
            Action::Download => "GET",
            Action::Upload => "PUT",
            Action::Verify => "POST",
        }
    }
}

/// Whose batch made a handle: only that instance, in that incarnation,
/// may use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub instance: String,
    pub instance_id: String,
    pub incarnation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub owner: Owner,
    pub action: Action,
    /// An https URL, as the batch response gave it.
    pub href: String,
    /// The action's `header`, minus fields the upstream client writes
    /// itself ([`upstream::RESERVED`]).
    pub headers: Vec<(String, String)>,
    pub expires: u64,
    /// For the transfer's `access.log` line.
    pub kind: Option<String>,
    pub credential: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// Unknown or expired.
    Missing,
    /// Another instance's (or an earlier incarnation's).
    Foreign,
    Found(&'a Entry),
}

pub struct Store {
    entries: HashMap<String, Entry>,
    cap: usize,
}

impl Store {
    /// `cap` handles per owner.
    pub fn new(cap: usize) -> Store {
        Store { entries: HashMap::new(), cap }
    }

    /// `entry` under a fresh handle, after dropping every expired one;
    /// `None` when the OS random source fails.
    pub fn insert(&mut self, entry: Entry, now: u64) -> Option<String> {
        self.entries.retain(|_, e| e.expires > now);
        let owned: Vec<(&String, u64)> =
            self.entries.iter().filter(|(_, e)| e.owner == entry.owner).map(|(h, e)| (h, e.expires)).collect();
        if owned.len() >= self.cap
            && let Some((first, _)) = owned.iter().min_by_key(|(_, expires)| *expires)
        {
            let first = (*first).clone();
            self.entries.remove(&first);
        }
        let handle = instance::random_base62(HANDLE_LEN).ok()?;
        self.entries.insert(handle.clone(), entry);
        Some(handle)
    }

    pub fn get(&self, handle: &str, owner: &Owner, now: u64) -> Lookup<'_> {
        match self.entries.get(handle) {
            Some(e) if e.expires <= now => Lookup::Missing,
            Some(e) if e.owner != *owner => Lookup::Foreign,
            Some(e) => Lookup::Found(e),
            None => Lookup::Missing,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// One batch response's context: whose it is and where its transfers go.
pub struct Batch<'a> {
    pub owner: &'a Owner,
    pub kind: Option<&'a str>,
    pub credential: Option<&'a str>,
    /// The instance's LFS listener.
    pub port: u16,
    pub now: u64,
}

/// `body` (a batch response) with every `objects[].actions.<action>`
/// stored and pointed at `http://127.0.0.1:<port>/<handle>`, its `header`
/// dropped and its `expires_*` kept. An action whose `href` isn't an
/// https URL only loses its `header`. `None` when the body isn't JSON or
/// holds no action: the caller sends it as it came.
pub fn rewrite_batch(body: &[u8], to: &Batch<'_>, store: &mut Store) -> Option<Vec<u8>> {
    let mut doc: Value = serde_json::from_slice(body).ok()?;
    let objects = doc.get_mut("objects")?.as_array_mut()?;
    let mut changed = false;
    for object in objects {
        let Some(actions) = object.get_mut("actions").and_then(Value::as_object_mut) else { continue };
        for kind in ACTIONS {
            if let Some(Value::Object(action)) = actions.get_mut(kind.name()) {
                changed |= rewrite_action(action, kind, to, store);
            }
        }
    }
    changed.then(|| serde_json::to_vec(&doc).expect("a parsed JSON value serializes"))
}

fn rewrite_action(action: &mut Map<String, Value>, kind: Action, to: &Batch<'_>, store: &mut Store) -> bool {
    let header = action.remove("header");
    let Some(href) = action.get("href").and_then(Value::as_str).filter(|h| Target::parse(h).is_ok()) else {
        return header.is_some();
    };
    let headers = match &header {
        Some(Value::Object(fields)) => fields
            .iter()
            .filter(|(name, _)| !upstream::RESERVED.iter().any(|r| name.eq_ignore_ascii_case(r)))
            .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
            .collect(),
        _ => Vec::new(),
    };
    let entry = Entry {
        owner: to.owner.clone(),
        action: kind,
        href: href.to_string(),
        headers,
        expires: expiry(action, to.now),
        kind: to.kind.map(str::to_string),
        credential: to.credential.map(str::to_string),
    };
    if let Some(handle) = store.insert(entry, to.now) {
        action.insert("href".into(), Value::String(format!("http://127.0.0.1:{}/{handle}", to.port)));
    }
    true
}

/// `expires_in` (preferred, as the LFS API says), else `expires_at`, else
/// [`DEFAULT_TTL`]; at most [`MAX_TTL`] away.
fn expiry(action: &Map<String, Value>, now: u64) -> u64 {
    let given = match action.get("expires_in").and_then(Value::as_i64) {
        Some(secs) => Some(now.saturating_add_signed(secs)),
        None => action.get("expires_at").and_then(Value::as_str).and_then(parse_rfc3339),
    };
    given.unwrap_or(now + DEFAULT_TTL).min(now + MAX_TTL)
}

/// Unix seconds of an RFC 3339 date-time (`2026-11-21T10:00:00Z`, a
/// fraction and an offset allowed); `None` for anything else.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let num = |at: usize, len: usize| -> Option<i64> {
        let t = s.get(at..at + len)?;
        t.bytes().all(|b| b.is_ascii_digit()).then(|| t.parse().ok())?
    };
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || !rest.is_ascii() || rest.as_bytes()[3] != b':' {
                return None;
            }
            let (h, m) = (rest[1..3].parse::<i64>().ok()?, rest[4..6].parse::<i64>().ok()?);
            if !rest[1..3].bytes().chain(rest[4..6].bytes()).all(|b| b.is_ascii_digit()) || h > 23 || m > 59 {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
    };
    let t = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(t).ok()
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_800_000_000;

    fn owner(instance: &str) -> Owner {
        Owner { instance: instance.into(), instance_id: format!("{instance}-id"), incarnation: Some("inc".into()) }
    }

    fn batch(o: &Owner) -> Batch<'_> {
        Batch { owner: o, kind: Some("github"), credential: Some("github-work"), port: 29005, now: NOW }
    }

    fn entry(o: &Owner, expires: u64) -> Entry {
        Entry {
            owner: o.clone(),
            action: Action::Download,
            href: "https://s.example/x".into(),
            headers: Vec::new(),
            expires,
            kind: None,
            credential: None,
        }
    }

    /// The handle a rewritten href names.
    fn handle(href: &Value) -> String {
        href.as_str().unwrap().strip_prefix("http://127.0.0.1:29005/").unwrap().to_string()
    }

    #[test]
    fn actions_are_stored_and_pointed_at_the_listener() {
        let web = owner("web-1");
        let mut store = Store::new(MAX_PER_INSTANCE);
        let body = json!({
            "transfer": "basic",
            "objects": [
                { "oid": "a", "size": 1, "actions": {
                    "download": {
                        "href": "https://github-cloud.githubusercontent.com/a?sig=SECRETSIG",
                        "header": { "Authorization": "RemoteAuth SECRET", "Content-Length": "1" },
                        "expires_in": 600,
                        "expires_at": "2027-01-01T00:00:00Z",
                    },
                } },
                { "oid": "b", "size": 2, "actions": {
                    "upload": { "href": "https://s3.example/b", "header": { "x-amz-date": "now", "n": 3 }, "expires_at": "2027-01-15T10:00:00+01:00" },
                    "verify": { "href": "https://lfs.github.com/o/r/objects/verify", "header": { "Authorization": "RemoteAuth V" } },
                } },
                { "oid": "c", "size": 3, "error": { "code": 404, "message": "not found" } },
                { "oid": "d", "size": 4, "actions": { "download": { "href": "http://plain.example/d", "header": { "Authorization": "x" } } } },
            ],
        });
        let out = rewrite_batch(&serde_json::to_vec(&body).unwrap(), &batch(&web), &mut store).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let text = String::from_utf8(out).unwrap();
        for secret in ["SECRET", "github-cloud", "s3.example", "lfs.github.com", "x-amz-date"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }

        let download = &v["objects"][0]["actions"]["download"];
        assert_eq!((download["expires_in"].as_i64(), download["expires_at"].as_str()), (Some(600), Some("2027-01-01T00:00:00Z")));
        let Lookup::Found(e) = store.get(&handle(&download["href"]), &web, NOW) else { panic!("stored") };
        assert_eq!(e.action, Action::Download);
        assert_eq!(e.href, "https://github-cloud.githubusercontent.com/a?sig=SECRETSIG");
        assert_eq!(e.headers, [("Authorization".to_string(), "RemoteAuth SECRET".to_string())], "reserved fields dropped");
        assert_eq!(e.expires, NOW + 600, "expires_in wins");
        assert_eq!((e.kind.as_deref(), e.credential.as_deref()), (Some("github"), Some("github-work")));

        let upload = &v["objects"][1]["actions"]["upload"];
        let Lookup::Found(e) = store.get(&handle(&upload["href"]), &web, NOW) else { panic!("stored") };
        assert_eq!((e.action, e.headers.clone()), (Action::Upload, vec![("x-amz-date".to_string(), "now".to_string())]));
        assert_eq!(e.expires, NOW + 3600, "expires_at, its offset applied");
        let verify = &v["objects"][1]["actions"]["verify"];
        let Lookup::Found(e) = store.get(&handle(&verify["href"]), &web, NOW) else { panic!("stored") };
        assert_eq!((e.action, e.expires), (Action::Verify, NOW + DEFAULT_TTL));

        assert_eq!(v["objects"][2]["error"]["code"], 404);
        // Not https: left as is, its header dropped.
        assert_eq!(v["objects"][3]["actions"]["download"], json!({ "href": "http://plain.example/d" }));
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn bodies_left_alone() {
        let web = owner("web-1");
        let mut store = Store::new(MAX_PER_INSTANCE);
        for body in [&b"not json"[..], br#"{"objects":"x"}"#, br#"{"message":"x"}"#, br#"{"objects":[{"oid":"a","actions":{}}]}"#]
        {
            assert_eq!(rewrite_batch(body, &batch(&web), &mut store), None, "{}", String::from_utf8_lossy(body));
        }
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn handles_expire_belong_to_their_owner_and_are_capped() {
        let (web, other) = (owner("web-1"), owner("other"));
        let mut store = Store::new(2);
        let h = store.insert(entry(&web, NOW + 10), NOW).unwrap();
        assert!(h.len() == HANDLE_LEN && h.bytes().all(|b| b.is_ascii_alphanumeric()), "{h}");
        assert!(matches!(store.get(&h, &web, NOW), Lookup::Found(_)));
        assert_eq!(store.get(&h, &other, NOW), Lookup::Foreign);
        let rebuilt = Owner { incarnation: Some("new".into()), ..web.clone() };
        assert_eq!(store.get(&h, &rebuilt, NOW), Lookup::Foreign, "another incarnation");
        assert_eq!(store.get(&h, &web, NOW + 10), Lookup::Missing, "expired");
        assert_eq!(store.get("nope", &web, NOW), Lookup::Missing);

        // At the cap the one expiring first goes; other owners keep theirs.
        let theirs = store.insert(entry(&other, NOW + 5), NOW).unwrap();
        let later = store.insert(entry(&web, NOW + 20), NOW).unwrap();
        let third = store.insert(entry(&web, NOW + 30), NOW).unwrap();
        assert_eq!(store.get(&h, &web, NOW), Lookup::Missing, "evicted");
        assert!(matches!(store.get(&later, &web, NOW), Lookup::Found(_)));
        assert!(matches!(store.get(&third, &web, NOW), Lookup::Found(_)));
        assert!(matches!(store.get(&theirs, &other, NOW), Lookup::Found(_)));
        // An insert drops what has expired.
        store.insert(entry(&other, NOW + 100), NOW + 6).unwrap();
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn expiry_is_bounded() {
        let at = |v: Value| expiry(v.as_object().unwrap(), NOW);
        assert_eq!(at(json!({})), NOW + DEFAULT_TTL);
        assert_eq!(at(json!({ "expires_in": 30 })), NOW + 30);
        assert_eq!(at(json!({ "expires_in": -30 })), NOW - 30);
        assert_eq!(at(json!({ "expires_in": 10_000_000 })), NOW + MAX_TTL);
        assert_eq!(at(json!({ "expires_at": crate::inbox::rfc3339(NOW + 60) })), NOW + 60);
        assert_eq!(at(json!({ "expires_at": "soon" })), NOW + DEFAULT_TTL);
    }

    #[test]
    fn rfc3339_dates() {
        for t in [0, 951_782_400, NOW, 4_102_444_800] {
            assert_eq!(parse_rfc3339(&crate::inbox::rfc3339(t)), Some(t));
        }
        assert_eq!(parse_rfc3339("2026-11-21T10:00:00.123456Z"), parse_rfc3339("2026-11-21T10:00:00Z"));
        assert_eq!(parse_rfc3339("2026-11-21T11:30:00+01:30"), parse_rfc3339("2026-11-21T10:00:00Z"));
        assert_eq!(parse_rfc3339("2026-11-21T08:00:00-02:00"), parse_rfc3339("2026-11-21T10:00:00Z"));
        for bad in [
            "",
            "2026-11-21",
            "2026-11-21 10:00:00Z",
            "2026-13-21T10:00:00Z",
            "2026-11-21T24:00:00Z",
            "2026-11-21T10:00:00",
            "2026-11-21T10:00:00.Z",
            "2026-11-21T10:00:00+0100",
            "2026-11-21T10:00:00+01:6x",
            "+026-11-21T10:00:00Z",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }
}
