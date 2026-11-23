//! Absolute upstream URLs in a response turned into the instance's loopback
//! listeners (base-URL mode): a `Location` header, and the JSON fields a
//! claim's `rewrite` names (sentry-cli's chunk-upload `url`, Sentry's
//! regions' `regions[].url`), so the tool's next call comes back through the broker
//! instead of going to the upstream with the phantom.
//!
//! Only `https://<host>[:443]` URLs whose host has a listener on the
//! instance are touched; anything else (relative, another host, another
//! port or scheme) passes as is.
//!
//! Also the org fence's lists ([`org_list`]): an `orgs.lists` route's
//! answer cut to the orgs the call may reach, in either mode.

use serde_json::Value;

/// A JSON body past this passes through unrewritten (and is logged so).
pub const MAX_REWRITE: usize = 16 * 1024 * 1024;

/// `listeners`: (upstream host, container port) of every listener the
/// instance has. `None` when `url` isn't one to rewrite.
pub fn loopback_url(url: &str, listeners: &[(String, u16)]) -> Option<String> {
    let rest = url.get(..8).filter(|s| s.eq_ignore_ascii_case("https://")).map(|_| &url[8..])?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return None;
    }
    let host = match authority.rsplit_once(':') {
        Some((h, "443")) => h,
        Some(_) => return None,
        None => authority,
    };
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let port = listeners.iter().find(|(h, _)| *h == host)?.1;
    let tail = if tail.is_empty() || tail.starts_with(['?', '#']) { format!("/{tail}") } else { tail.to_string() };
    Some(format!("http://127.0.0.1:{port}{tail}"))
}

/// A `Location` value rewritten, if it's an absolute URL to a bound
/// upstream.
pub fn location(value: &[u8], listeners: &[(String, u16)]) -> Option<Vec<u8>> {
    loopback_url(std::str::from_utf8(value).ok()?, listeners).map(String::into_bytes)
}

/// `body` with each of `fields` (`url`, `regions[].url`, `a.b`) holding a
/// URL to a bound upstream replaced by its loopback one. `None` when the
/// body isn't JSON or nothing changed: the caller sends it as it came.
pub fn json_fields(body: &[u8], fields: &[String], listeners: &[(String, u16)]) -> Option<Vec<u8>> {
    let mut doc: Value = serde_json::from_slice(body).ok()?;
    let mut changed = false;
    for f in fields {
        let path: Vec<&str> = f.split('.').collect();
        changed |= rewrite_at(&mut doc, &path, listeners);
    }
    changed.then(|| serde_json::to_vec(&doc).expect("a parsed JSON value serializes"))
}

/// `body`, a JSON array, keeping only the elements one of whose top-level
/// `fields` holds an `allowed` slug (any case) or the id of one: `id_of`
/// resolves the slugs once, and only for an element whose fields hold all
/// digits and no slug. `None` when the body isn't a JSON array: the
/// caller refuses it rather than pass a list it couldn't cut.
pub fn org_list(body: &[u8], fields: &[String], allowed: &[String], id_of: &dyn Fn(&str) -> Option<String>) -> Option<Vec<u8>> {
    let Value::Array(items) = serde_json::from_slice(body).ok()? else { return None };
    let mut ids: Option<Vec<String>> = None;
    let kept: Vec<Value> = items
        .into_iter()
        .filter(|item| {
            let values: Vec<String> = fields
                .iter()
                .filter_map(|f| match item.get(f)? {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => n.as_u64().map(|n| n.to_string()),
                    _ => None,
                })
                .collect();
            if values.iter().any(|v| allowed.iter().any(|a| a.eq_ignore_ascii_case(v))) {
                return true;
            }
            let digits: Vec<&String> =
                values.iter().filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())).collect();
            if digits.is_empty() {
                return false;
            }
            let ids = ids.get_or_insert_with(|| allowed.iter().filter_map(|s| id_of(s)).collect());
            digits.iter().any(|d| ids.contains(d))
        })
        .collect();
    Some(serde_json::to_vec(&Value::Array(kept)).expect("a parsed JSON value serializes"))
}

fn rewrite_at(v: &mut Value, path: &[&str], listeners: &[(String, u16)]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        let Value::String(s) = v else { return false };
        return match loopback_url(s, listeners) {
            Some(new) => {
                *s = new;
                true
            }
            None => false,
        };
    };
    let (name, each) = match first.strip_suffix("[]") {
        Some(n) => (n, true),
        None => (*first, false),
    };
    let Some(child) = v.as_object_mut().and_then(|o| o.get_mut(name)) else { return false };
    if !each {
        return rewrite_at(child, rest, listeners);
    }
    let Value::Array(items) = child else { return false };
    let mut changed = false;
    for item in items {
        changed |= rewrite_at(item, rest, listeners);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn listeners() -> Vec<(String, u16)> {
        vec![("sentry.io".into(), 29001), ("us.sentry.io".into(), 29002)]
    }

    #[test]
    fn urls() {
        let l = listeners();
        let cases = [
            ("https://us.sentry.io/api/0/x/?a=1", Some("http://127.0.0.1:29002/api/0/x/?a=1")),
            ("HTTPS://US.Sentry.IO:443/p", Some("http://127.0.0.1:29002/p")),
            ("https://sentry.io.", Some("http://127.0.0.1:29001/")),
            ("https://sentry.io?q", Some("http://127.0.0.1:29001/?q")),
            ("https://sentry.io:8443/p", None),
            ("http://sentry.io/p", None),
            ("https://evil.example/p", None),
            ("https://u@sentry.io/p", None),
            ("/relative", None),
        ];
        for (url, want) in cases {
            assert_eq!(loopback_url(url, &l).as_deref(), want, "{url}");
        }
        assert_eq!(location(b"https://sentry.io/next", &l), Some(b"http://127.0.0.1:29001/next".to_vec()));
        assert_eq!(location(b"\xff", &l), None);
    }

    #[test]
    fn chunk_upload_and_regions() {
        let l = listeners();
        let body = json!({ "url": "https://us.sentry.io/api/0/organizations/acme/chunk-upload/", "chunkSize": 8388608 });
        let out = json_fields(&serde_json::to_vec(&body).unwrap(), &["url".into()], &l).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["url"], "http://127.0.0.1:29002/api/0/organizations/acme/chunk-upload/");
        assert_eq!(v["chunkSize"], 8388608);

        let body = json!({ "regions": [
            { "name": "us", "url": "https://us.sentry.io" },
            { "name": "de", "url": "https://de.sentry.io" },
            { "name": "x", "url": 3 },
        ] });
        let out = json_fields(&serde_json::to_vec(&body).unwrap(), &["regions[].url".into()], &l).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["regions"][0]["url"], "http://127.0.0.1:29002/");
        assert_eq!(v["regions"][1]["url"], "https://de.sentry.io", "no listener: untouched");
        assert_eq!(v["regions"][2]["url"], 3);
    }

    #[test]
    fn org_lists_keep_the_fences_orgs() {
        let fields = vec!["slug".to_string(), "id".to_string()];
        let allowed = vec!["acme".to_string(), "beta".to_string()];
        let asked = std::cell::Cell::new(0);
        let id_of = |slug: &str| {
            asked.set(asked.get() + 1);
            (slug == "beta").then(|| "22".to_string())
        };
        let filter = |v: Value| {
            let out = org_list(&serde_json::to_vec(&v).unwrap(), &fields, &allowed, &id_of)?;
            Some(serde_json::from_slice::<Value>(&out).unwrap())
        };
        let list = json!([
            { "slug": "ACME", "id": "11", "name": "Acme" },
            { "slug": "other", "id": "33" },
            { "name": "no fields" },
            "not an object",
        ]);
        assert_eq!(filter(list), Some(json!([{ "slug": "ACME", "id": "11", "name": "Acme" }])));
        assert_eq!(asked.get(), 2, "`other`'s id resolved the allowed slugs once");
        asked.set(0);
        let by_id = json!([{ "id": "22" }, { "id": 22 }, { "id": "11" }, { "slug": "x" }, { "slug": "acme" }]);
        assert_eq!(filter(by_id), Some(json!([{ "id": "22" }, { "id": 22 }, { "slug": "acme" }])));
        assert_eq!(asked.get(), 2, "once per body");
        asked.set(0);
        assert_eq!(filter(json!([{ "slug": "acme" }, { "slug": "x" }])), Some(json!([{ "slug": "acme" }])));
        assert_eq!(asked.get(), 0, "no digits, no lookup");
        assert_eq!(filter(json!([])), Some(json!([])));
        assert_eq!(filter(json!({ "detail": "x" })), None, "not an array");
        assert_eq!(org_list(b"nope", &fields, &allowed, &id_of), None);
        let none: Vec<String> = Vec::new();
        assert_eq!(org_list(br#"[{"slug":"acme"}]"#, &fields, &none, &id_of).as_deref(), Some(&b"[]"[..]));
    }

    #[test]
    fn untouched_bodies() {
        let l = listeners();
        let other = serde_json::to_vec(&json!({ "url": "https://evil.example/x" })).unwrap();
        assert_eq!(json_fields(&other, &["url".into()], &l), None);
        assert_eq!(json_fields(b"not json", &["url".into()], &l), None);
        let relative = serde_json::to_vec(&json!({ "url": "/api/0/x/" })).unwrap();
        assert_eq!(json_fields(&relative, &["url".into()], &l), None);
        let missing = serde_json::to_vec(&json!({ "regions": "nope" })).unwrap();
        assert_eq!(json_fields(&missing, &["regions[].url".into(), "a.b".into()], &l), None);
    }
}
