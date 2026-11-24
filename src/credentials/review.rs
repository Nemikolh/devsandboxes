//! What an approver reads before approving a claim version: its summary
//! (`devsandbox login`) and its diff against the latest approved one
//! (`devsandbox access approve`; github-and-sentry-access, *The claim
//! contract*). Both read the canonical pinned JSON
//! ([`super::claim::Claim::canonical`]), the form the home keeps and a
//! remote host sends, so a version reads the same whichever way it arrived.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::ask::Node;
use crate::ask::style::Role;

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
    /// `(method, hint)`, a broad login's method as `<method>.broad`.
    pub login: Vec<(String, Option<String>)>,
    /// The other pinned fields, plus `scopes.<name>.permissions`,
    /// `login.<method>.create` and `login.<method>.scope`.
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
                // A claim may have a method twice, broad and not.
                let method = match l.get("broad").and_then(Value::as_bool) {
                    Some(true) => format!("{method}.broad"),
                    _ => method.to_string(),
                };
                for field in ["create", "scope"] {
                    if let Some(v) = l.get(field) {
                        rest.insert(format!("login.{method}.{field}"), v.clone());
                    }
                }
                Ok((method, l.get("hint").and_then(Value::as_str).map(String::from)))
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

    /// What a version asks, as a prompt's tree: scopes and `never` fold to
    /// a preview, each scope opening to its routes.
    pub fn fold(&self, version: &str) -> Vec<Node> {
        let mut tree = vec![self.heading().hint(format!("version {version}"))];
        tree.push(match self.upstreams.len() {
            0 => Node::new("upstreams: none"),
            1 => Node::new(format!("upstreams: {}", self.upstreams.first().expect("one"))),
            n => Node::new("upstreams").hint(format!("{n} hosts")).children(self.upstreams.iter().map(Node::new)).opened(),
        });
        tree.push(if self.scopes.is_empty() {
            Node::new("scopes: none")
        } else {
            Node::new("scopes").hint(plural(self.scopes.len(), "scope")).children(
                self.scopes
                    .iter()
                    .map(|(name, routes)| {
                        Node::new(name).role(Role::Name).hint(count(routes.len())).children(routes.iter().map(Node::new)).brief()
                    }),
            )
        });
        tree.push(if self.never.is_empty() {
            Node::new("never: none")
        } else {
            Node::new("never")
                .role(Role::Danger)
                .hint(format!("{}, no ask or rule opens them", count(self.never.len())))
                .children(self.never.iter().map(Node::new))
        });
        tree
    }

    fn heading(&self) -> Node {
        Node::new(format!("claim `{}` ({})", self.kind, self.mode)).role(Role::Heading)
    }
}

fn count(n: usize) -> String {
    plural(n, "route")
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
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

    fn nodes(&self) -> impl Iterator<Item = Node> + '_ {
        let added = self.added.iter().map(|a| Node::new(format!("+ {a}")).role(Role::Added));
        added.chain(self.removed.iter().map(|r| Node::new(format!("- {r}")).role(Role::Removed)))
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
    /// The diff as a prompt's tree, `new`'s `version` against `old`'s
    /// `approved`. Edited scopes, upstreams and `never` start open;
    /// a new or dropped scope folds, its lines all one sign.
    pub fn fold(&self, old: &View, new: &View, version: &str, approved: &str) -> Vec<Node> {
        let mut tree = vec![new.heading().hint(format!("version {version}, against approved {approved}"))];
        if !self.upstreams.is_empty() {
            tree.push(Node::new("upstreams").children(self.upstreams.nodes()).opened());
        }
        for (name, change) in &self.scopes {
            let node = Node::new(format!("scope {name}")).children(change.nodes());
            tree.push(match (old.scopes.contains_key(name), new.scopes.contains_key(name)) {
                (false, _) => node.hint("new"),
                (_, false) => node.hint("dropped"),
                _ => node.opened(),
            });
        }
        if !self.never.is_empty() {
            tree.push(Node::new("never").role(Role::Danger).children(self.never.nodes()).opened());
        }
        if !self.changed.is_empty() {
            tree.push(Node::new(format!("also changed: {}", self.changed.join(", "))));
        }
        tree
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::claim;
    use serde_json::json;

    fn sentry_json() -> Value {
        fixture_json("sentry")
    }

    fn fixture_json(dir: &str) -> Value {
        let path = format!("{}/tests/fixtures/features/{dir}/devcontainer-feature.json", env!("CARGO_MANIFEST_DIR"));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        v["customizations"]["devsandbox"]["credentials"]["sentry"].clone()
    }

    fn view(claim: &Value) -> View {
        View::parse(&claim::parse("sentry", claim).unwrap().canonical()).unwrap()
    }

    /// The tree as the plain fallback prints it, one string.
    fn plain(tree: Vec<Node>) -> String {
        crate::ask::fold::Fold::new(tree).plain_lines(crate::ask::Style::plain()).iter().map(|l| format!("{l}\n")).collect()
    }

    fn tree_open(tree: &[Node], label: &str) -> bool {
        tree.iter().find(|n| n.label == label).expect("a node so labeled").open
    }

    #[test]
    fn the_sentry_claim_summarizes() {
        let v = view(&sentry_json());
        assert_eq!(v.kind, "sentry");
        let tree = v.fold("abc");
        let scopes = &tree[2];
        let s = plain(tree.clone());
        assert!(
            s.starts_with("claim `sentry` (intercept) (version abc)\n▾ upstreams (3 hosts)\n  de.sentry.io\n  sentry.io\n  us.sentry.io\n▾ scopes (14 scopes)\n  ▸ alerts.read (5 routes)\n  ▸ alerts.write (6 routes)\n"),
            "plain: each scope its head alone: {s}"
        );
        let hint = |name: &str| scopes.children.iter().find(|c| c.label == name).and_then(|c| c.hint.clone());
        assert_eq!(hint("issues.read").as_deref(), Some("13 routes"));
        assert_eq!(hint("issues.write").as_deref(), Some("5 routes"));
        assert_eq!(hint("seer").as_deref(), Some("1 route"));
        assert_eq!(scopes.children.iter().find(|c| c.label == "seer").unwrap().children.len(), 1);
        assert!(s.contains("\n▾ never (13 routes, no ask or rule opens them)\n  DELETE /api/0/organizations/{org}/\n"), "{s}");
        assert_eq!(s.lines().skip_while(|l| !l.starts_with("▾ never")).count(), 14, "plain: every never route: {s}");
        assert_eq!(tree[3].role, Role::Danger);
        assert!(!scopes.open && !tree[3].open, "scopes and never fold");

        let mut bare = sentry_json();
        bare["upstreams"] = json!(["sentry.io"]);
        bare["scopes"] = json!({});
        bare["never"] = json!([]);
        bare.as_object_mut().unwrap().remove("orgs");
        assert_eq!(plain(view(&bare).fold("v")), "claim `sentry` (intercept) (version v)\nupstreams: sentry.io\nscopes: none\nnever: none\n");
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
        let (old, new) = (view(&old_json), view(&c));
        let d = diff(&old, &new);
        assert_eq!(d.upstreams, Change { added: vec!["eu.sentry.io".into()], removed: vec!["de.sentry.io".into()] });
        assert_eq!(d.scopes.keys().collect::<Vec<_>>(), ["debug.read", "org.read", "releases.read", "uploads"]);
        assert_eq!(
            d.scopes["releases.read"].removed,
            ["GET /api/0/organizations/{org}/preprodartifacts/{artifact_id}/install-details/"]
        );
        assert_eq!(d.scopes["uploads"].removed.len(), 5);
        assert_eq!(d.never.added, ["POST /api/0/organizations/{org}/integrations/"]);
        assert!(d.never.removed.is_empty());
        assert_eq!(d.changed, ["refusal", "scopes.org.read.permissions"]);
        let tree = d.fold(&old, &new, "v2", "v1");
        assert_eq!((tree[1].children[0].role, tree[1].children[1].role), (Role::Added, Role::Removed));
        let text = plain(tree);
        assert!(
            text.starts_with(
                "claim `sentry` (intercept) (version v2, against approved v1)\n▾ upstreams\n  + eu.sentry.io\n  - de.sentry.io\n\
                 ▾ scope debug.read (new)\n  + GET /api/0/projects/{org}/{project}/files/dsyms/\n\
                 ▾ scope org.read\n  + GET /api/0/organizations/{org}/members/\n"
            ),
            "{text}"
        );
        assert!(text.contains("\n▾ scope uploads (dropped)\n  - "), "{text}");
        assert!(!tree_open(&d.fold(&old, &new, "v2", "v1"), "scope uploads"), "a dropped scope folds on a terminal");
        assert!(text.contains("\n▾ never\n  + POST /api/0/organizations/{org}/integrations/\n"), "{text}");
        assert!(text.ends_with("\nalso changed: refusal, scopes.org.read.permissions\n"), "{text}");

        assert_eq!(diff(&old, &old), Diff::default());
        let mut private = old_json.clone();
        private["upstreams"] = json!(["sentry.io", { "host": "us.sentry.io", "private": true }, "de.sentry.io"]);
        let d = diff(&old, &view(&private));
        assert_eq!(d.upstreams.added, ["us.sentry.io (private)"]);
        assert_eq!(d.upstreams.removed, ["us.sentry.io"]);

        let cli = fixture_json("sentry-cli");
        let mut unrewritten = cli.clone();
        unrewritten["rewrite"] = json!([]);
        assert_eq!(diff(&view(&cli), &view(&unrewritten)).changed, ["rewrite"]);
    }

    #[test]
    fn a_paste_login_s_token_page_is_a_change() {
        let mut old_json = sentry_json();
        old_json["login"][0].as_object_mut().unwrap().remove("create");
        let mut c = old_json.clone();
        c["login"][0]["create"] = json!({ "url": "https://sentry.io/settings/account/api/auth-tokens/new-token/" });
        let (old, new) = (view(&old_json), view(&c));
        assert_eq!(diff(&old, &new).changed, ["login.paste.create"]);
        let mut d = c.clone();
        d["login"][0]["create"]["permissions"] = json!(true);
        assert_eq!(diff(&new, &view(&d)).changed, ["login.paste.create"]);
    }

    #[test]
    fn a_broad_login_its_scope_and_broad_tokens_are_changes() {
        let mut old_json = sentry_json();
        old_json["login"].as_array_mut().unwrap().retain(|l| l["method"] == "paste");
        let device = |scope: &str| {
            json!({ "method": "device", "clientId": "c", "scope": scope, "broad": true,
                    "deviceUrl": "https://sentry.io/d", "tokenUrl": "https://sentry.io/t" })
        };
        let mut c = old_json.clone();
        c["tokens"] = json!({ "broad": ["sntryu_"] });
        c["login"].as_array_mut().unwrap().push(device("org:read"));
        let (old, new) = (view(&old_json), view(&c));
        assert_eq!(diff(&old, &new).changed, ["login", "login.device.broad.scope", "tokens"]);
        let mut d = c.clone();
        d["login"][1] = device("org:read org:write");
        assert_eq!(diff(&new, &view(&d)).changed, ["login.device.broad.scope"]);
        let mut narrow = c.clone();
        narrow["login"][1]["broad"] = json!(false);
        let changed = diff(&new, &view(&narrow)).changed;
        assert!(changed.contains(&"login".to_string()) && changed.contains(&"login.device.scope".to_string()), "{changed:?}");
    }

    #[test]
    fn the_check_s_shown_field_is_a_change() {
        let old_json = sentry_json();
        let mut c = old_json.clone();
        c["checkShows"] = json!("user");
        assert_eq!(diff(&view(&old_json), &view(&c)).changed, ["checkShows"]);
    }

    #[test]
    fn the_org_fence_is_a_change() {
        let mut old_json = sentry_json();
        old_json.as_object_mut().unwrap().remove("orgs");
        let mut c = old_json.clone();
        c["orgs"] = json!({
            "var": "{org}",
            "resolve": "GET /api/0/organizations/{org}/",
            "id": "id",
            "lists": { "GET /api/0/organizations/": ["slug", "id"] },
        });
        let new = view(&c);
        assert_eq!(diff(&view(&old_json), &new).changed, ["orgs"]);
        let mut d = c.clone();
        d["orgs"]["lists"] = json!({});
        assert_eq!(diff(&new, &view(&d)).changed, ["orgs"]);
    }

    #[test]
    fn bad_versions_are_refused() {
        for bad in ["", "[]", r#"{"kind":"x"}"#, r#"{"kind":"x","claim":{"never":"GET /"}}"#] {
            assert!(View::parse(bad).is_err(), "{bad}");
        }
    }
}
