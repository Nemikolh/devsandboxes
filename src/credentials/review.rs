//! What an approver reads before approving a claim version: its summary
//! (`devsandbox login`) and its diff against the latest approved one
//! (`devsandbox access approve`; github-and-sentry-access, *The claim
//! contract*). Both read the canonical pinned JSON
//! ([`super::claim::Claim::canonical`]), the form the home keeps and a
//! remote host sends, so a version reads the same whichever way it arrived.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

/// Fields read on their own; every other pinned field is compared whole.
const READ: &[&str] = &["upstreams", "mode", "scopes", "never", "login"];

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub kind: String,
    pub mode: String,
    /// `host`, or `host (private)`.
    pub upstreams: BTreeSet<String>,
    /// Scope name → its routes.
    pub scopes: BTreeMap<String, BTreeSet<String>>,
    pub never: BTreeSet<String>,
    /// `(method, hint)`.
    pub login: Vec<(String, Option<String>)>,
    /// The other pinned fields, plus `scopes.<name>.permissions`.
    pub rest: BTreeMap<String, Value>,
}

impl View {
    pub fn parse(canonical: &str) -> Result<View> {
        let doc: Value = serde_json::from_str(canonical).context("a claim version isn't JSON")?;
        let kind = doc.get("kind").and_then(Value::as_str).context("a claim version has no `kind`")?;
        let Some(claim) = doc.get("claim").and_then(Value::as_object) else {
            bail!("a claim version has no `claim` object");
        };
        let mode = claim.get("mode").and_then(Value::as_str).unwrap_or_default().to_string();
        let upstreams = array(claim, "upstreams")?
            .iter()
            .map(|u| {
                let host = u.get("host").and_then(Value::as_str).context("an upstream without `host`")?;
                Ok(match u.get("private").and_then(Value::as_bool) {
                    Some(true) => format!("{host} (private)"),
                    _ => host.to_string(),
                })
            })
            .collect::<Result<_>>()?;
        let mut rest: BTreeMap<String, Value> =
            claim.iter().filter(|(k, _)| !READ.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut scopes = BTreeMap::new();
        for (name, scope) in claim.get("scopes").and_then(Value::as_object).into_iter().flatten() {
            scopes.insert(name.clone(), strings(scope.get("routes"))?);
            if let Some(p) = scope.get("permissions") {
                rest.insert(format!("scopes.{name}.permissions"), p.clone());
            }
        }
        let login = array(claim, "login")?
            .iter()
            .map(|l| {
                let method = l.get("method").and_then(Value::as_str).context("a login without `method`")?;
                Ok((method.to_string(), l.get("hint").and_then(Value::as_str).map(String::from)))
            })
            .collect::<Result<_>>()?;
        Ok(View {
            kind: kind.to_string(),
            mode,
            upstreams,
            scopes,
            never: strings(claim.get("never"))?,
            login,
            rest,
        })
    }

    pub fn summary(&self) -> String {
        let mut out = format!("claim `{}` ({})\n", self.kind, self.mode);
        let up: Vec<&str> = self.upstreams.iter().map(String::as_str).collect();
        let _ = writeln!(out, "  upstreams: {}", up.join(", "));
        if self.scopes.is_empty() {
            let _ = writeln!(out, "  scopes: none");
        } else {
            let _ = writeln!(out, "  scopes:");
            for (name, routes) in &self.scopes {
                let _ = writeln!(out, "    {name} ({})", count(routes.len()));
            }
        }
        if self.never.is_empty() {
            let _ = writeln!(out, "  never: none");
        } else {
            let _ = writeln!(out, "  never (no ask or rule opens them):");
            for route in &self.never {
                let _ = writeln!(out, "    {route}");
            }
        }
        out
    }
}

fn count(n: usize) -> String {
    if n == 1 { "1 route".into() } else { format!("{n} routes") }
}

fn array<'a>(claim: &'a Map<String, Value>, name: &str) -> Result<&'a [Value]> {
    match claim.get(name) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => bail!("a claim version's `{name}` isn't an array"),
    }
}

fn strings(v: Option<&Value>) -> Result<BTreeSet<String>> {
    match v {
        None | Some(Value::Null) => Ok(BTreeSet::new()),
        Some(Value::Array(a)) => {
            a.iter().map(|s| s.as_str().map(String::from).context("a route isn't a string")).collect()
        }
        Some(_) => bail!("a claim version's route list isn't an array"),
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Diff {
    pub upstreams: Change,
    /// Only scopes whose routes changed; a new scope's routes all added, a
    /// dropped one's all removed.
    pub scopes: BTreeMap<String, Change>,
    pub never: Change,
    /// Other fields whose value differs (`mode`, `inject`, `refusal`,
    /// `scopes.<name>.permissions`, …), in name order.
    pub changed: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Change {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl Change {
    fn of(old: &BTreeSet<String>, new: &BTreeSet<String>) -> Change {
        Change {
            added: new.difference(old).cloned().collect(),
            removed: old.difference(new).cloned().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    fn render(&self, out: &mut String) {
        for a in &self.added {
            let _ = writeln!(out, "    + {a}");
        }
        for r in &self.removed {
            let _ = writeln!(out, "    - {r}");
        }
    }
}

pub fn diff(old: &View, new: &View) -> Diff {
    let empty = BTreeSet::new();
    let scopes = old
        .scopes
        .keys()
        .chain(new.scopes.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|name| {
            let change = Change::of(old.scopes.get(name).unwrap_or(&empty), new.scopes.get(name).unwrap_or(&empty));
            (!change.is_empty()).then(|| (name.clone(), change))
        })
        .collect();
    let mut changed: BTreeSet<String> = old
        .rest
        .keys()
        .chain(new.rest.keys())
        .filter(|k| old.rest.get(*k) != new.rest.get(*k))
        // A new or dropped scope's permissions go with its routes.
        .filter(|k| match k.strip_prefix("scopes.").and_then(|s| s.strip_suffix(".permissions")) {
            Some(scope) => old.scopes.contains_key(scope) && new.scopes.contains_key(scope),
            None => true,
        })
        .cloned()
        .collect();
    if old.mode != new.mode {
        changed.insert("mode".into());
    }
    if old.login != new.login {
        changed.insert("login".into());
    }
    Diff {
        upstreams: Change::of(&old.upstreams, &new.upstreams),
        scopes,
        never: Change::of(&old.never, &new.never),
        changed: changed.into_iter().collect(),
    }
}

impl Diff {
    pub fn render(&self, old: &View, new: &View) -> String {
        let mut out = String::new();
        if !self.upstreams.is_empty() {
            out.push_str("  upstreams:\n");
            self.upstreams.render(&mut out);
        }
        for (name, change) in &self.scopes {
            let note = match (old.scopes.contains_key(name), new.scopes.contains_key(name)) {
                (false, _) => " (new)",
                (_, false) => " (dropped)",
                _ => "",
            };
            let _ = writeln!(out, "  scope {name}{note}:");
            change.render(&mut out);
        }
        if !self.never.is_empty() {
            out.push_str("  never:\n");
            self.never.render(&mut out);
        }
        if !self.changed.is_empty() {
            let _ = writeln!(out, "  also changed: {}", self.changed.join(", "));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::claim;
    use serde_json::json;

    fn sentry_json() -> Value {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/features/sentry/devcontainer-feature.json"
        ))
        .unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        v["customizations"]["devsandbox"]["credentials"]["sentry"].clone()
    }

    fn view(claim: &Value) -> View {
        View::parse(&claim::parse("sentry", claim).unwrap().canonical()).unwrap()
    }

    #[test]
    fn the_sentry_claim_summarizes() {
        let v = view(&sentry_json());
        assert_eq!(v.kind, "sentry");
        let s = v.summary();
        assert!(s.starts_with("claim `sentry` (base-url)\n  upstreams: de.sentry.io, sentry.io, us.sentry.io\n"), "{s}");
        assert!(s.contains("    issues.read (6 routes)\n"), "{s}");
        assert!(s.contains("  never (no ask or rule opens them):\n    GET /api/0/projects/{org}/{project}/keys/\n"), "{s}");
    }

    #[test]
    fn diff_reports_upstreams_routes_never_and_the_rest() {
        let old_json = sentry_json();
        let mut c = old_json.clone();
        c["upstreams"] = json!(["sentry.io", "us.sentry.io", "eu.sentry.io"]);
        c["scopes"]["releases.read"]["routes"].as_array_mut().unwrap().pop();
        c["scopes"]["org.read"]["routes"].as_array_mut().unwrap().push(json!("GET /api/0/organizations/{org}/members/"));
        c["scopes"]["org.read"]["permissions"] = json!({ "org": "write" });
        c["scopes"].as_object_mut().unwrap().remove("uploads");
        c["scopes"]["debug.read"] = json!({ "routes": ["GET /api/0/projects/{org}/{project}/files/dsyms/"] });
        c["never"].as_array_mut().unwrap().push(json!("POST /api/0/organizations/{org}/integrations/"));
        c["refusal"] = json!({ "shape": "json", "field": "message" });
        c["rewrite"] = json!([]);
        let (old, new) = (view(&old_json), view(&c));
        let d = diff(&old, &new);
        assert_eq!(d.upstreams, Change { added: vec!["eu.sentry.io".into()], removed: vec!["de.sentry.io".into()] });
        assert_eq!(d.scopes.keys().collect::<Vec<_>>(), ["debug.read", "org.read", "releases.read", "uploads"]);
        assert_eq!(d.scopes["releases.read"].removed, ["GET /api/0/projects/{org}/{project}/releases/"]);
        assert_eq!(d.scopes["uploads"].removed.len(), 5);
        assert_eq!(d.never.added, ["POST /api/0/organizations/{org}/integrations/"]);
        assert!(d.never.removed.is_empty());
        assert_eq!(d.changed, ["refusal", "rewrite", "scopes.org.read.permissions"]);
        let text = d.render(&old, &new);
        assert!(text.starts_with("  upstreams:\n    + eu.sentry.io\n    - de.sentry.io\n  scope debug.read (new):\n"), "{text}");
        assert!(text.contains("  scope uploads (dropped):\n    - "), "{text}");
        assert!(text.ends_with("  also changed: refusal, rewrite, scopes.org.read.permissions\n"), "{text}");

        assert_eq!(diff(&old, &old), Diff::default());
        let mut private = old_json.clone();
        private["upstreams"] = json!(["sentry.io", { "host": "us.sentry.io", "private": true }, "de.sentry.io"]);
        let d = diff(&old, &view(&private));
        assert_eq!(d.upstreams.added, ["us.sentry.io (private)"]);
        assert_eq!(d.upstreams.removed, ["us.sentry.io"]);
    }

    #[test]
    fn bad_versions_are_refused() {
        for bad in ["", "[]", r#"{"kind":"x"}"#, r#"{"kind":"x","claim":{"never":"GET /"}}"#] {
            assert!(View::parse(bad).is_err(), "{bad}");
        }
    }
}
