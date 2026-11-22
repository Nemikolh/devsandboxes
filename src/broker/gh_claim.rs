//! Test-only: the `features/gh` claim (its mirror,
//! `tests/fixtures/features/github`) held to the gh it installs and to
//! GitHub's GraphQL schema (docs/architecture/credentials/gh.md).
//!
//! - `tests/fixtures/gh/routes.json`: the calls each gh command of the
//!   pinned release makes, from gh's source. Every `commands` call lands in
//!   a scope, every `outside` call in `other` or `never`, every `never`
//!   call in `never`; each mapped mutation has `fence` inputs.
//! - `tests/fixtures/gh/schema-extract.json`: the mapped mutations'
//!   arguments and the input objects they reach, trimmed from GitHub's
//!   public schema by `scripts/gh-schema-extract.py`. Every input that can
//!   name a repo or an object in one must be in the mutation's `fence`
//!   (github-and-sentry-access, *Routes the broker can attribute*), and
//!   every `fence` input must exist.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::route::{self, Scoped};
use crate::credentials::claim::{Pinned, Route};

fn claim() -> Pinned {
    crate::credentials::home::tests::github().pinned
}

#[derive(Deserialize)]
struct Routes {
    commands: BTreeMap<String, Vec<String>>,
    outside: BTreeMap<String, Outside>,
    never: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct Outside {
    calls: Vec<String>,
}

fn routes() -> Routes {
    serde_json::from_str(include_str!("../../tests/fixtures/gh/routes.json")).unwrap()
}

/// Where the claim puts one call: `graphql …` by its route, an HTTP
/// request line by [`route::scope_of`].
fn placed(c: &Pinned, call: &str) -> Scoped {
    if let Some(rest) = call.strip_prefix("graphql ") {
        let r = Route::parse(&format!("graphql {rest}")).unwrap();
        if c.never.contains(&r) {
            return Scoped::Never;
        }
        return match c.scopes.iter().find(|(_, s)| s.routes.contains(&r)) {
            Some((name, _)) => Scoped::Scope(name.clone()),
            None => Scoped::Other,
        };
    }
    let (m, path) = call.split_once(' ').unwrap();
    route::check_path(path).unwrap_or_else(|why| panic!("`{call}`: {why}"));
    route::scope_of(c, m, path).unwrap_or_else(|why| panic!("`{call}`: {why}")).scoped
}

#[test]
fn every_gh_call_is_placed() {
    let c = claim();
    let r = routes();
    let mut misplaced = Vec::new();
    for (cmd, calls) in &r.commands {
        for call in calls {
            if !matches!(placed(&c, call), Scoped::Scope(_)) {
                misplaced.push(format!("gh {cmd}: `{call}` is {}", placed(&c, call).name()));
            }
            if let Some(name) = call.strip_prefix("graphql mutation ") {
                let fence = c.fence.get(&Route::GraphqlMutation(name.into()));
                if fence.is_none_or(Vec::is_empty) {
                    misplaced.push(format!("gh {cmd}: `{call}` has no `fence` inputs"));
                }
            }
        }
    }
    for (cmd, o) in &r.outside {
        for call in &o.calls {
            if matches!(placed(&c, call), Scoped::Scope(_)) {
                misplaced.push(format!("gh {cmd}: `{call}` is meant to stay out of the table"));
            }
        }
    }
    for (cmd, calls) in &r.never {
        for call in calls {
            if placed(&c, call) != Scoped::Never {
                misplaced.push(format!("gh {cmd}: `{call}` isn't `never`"));
            }
        }
    }
    assert!(misplaced.is_empty(), "{misplaced:#?}");
}

/// `{repo}` as `acme/app`, any other variable as one segment.
fn instance(path: &str) -> String {
    path.replace(route::REPO, "acme/app")
        .split('/')
        .map(|seg| match seg.starts_with('{') && seg.ends_with('}') {
            true => "x1",
            false => seg,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[test]
fn every_never_route_is_unreachable() {
    let c = claim();
    for r in &c.never {
        let call = match r {
            Route::Http { method, path } => format!("{} {}", method.as_str(), instance(path)),
            other => other.to_string(),
        };
        assert_eq!(placed(&c, &call), Scoped::Never, "{call}");
    }
}

#[test]
fn every_scoped_route_matches_itself_and_presence_and_fence_name_scoped_routes() {
    let c = claim();
    let scoped: BTreeSet<&Route> = c.scopes.values().flat_map(|s| &s.routes).collect();
    for (name, s) in &c.scopes {
        for r in &s.routes {
            let Route::Http { method, path } = r else { continue };
            let call = format!("{} {}", method.as_str(), instance(path));
            assert_eq!(placed(&c, &call), Scoped::Scope(name.clone()), "{call}: shadowed by another scope's route");
        }
    }
    for p in &c.presence {
        if let crate::credentials::claim::Presence::Route { route, .. } = p {
            assert!(scoped.contains(route), "presence `{route}` is in no scope");
        }
    }
    for r in c.fence.keys() {
        assert!(scoped.contains(r), "fence `{r}` is in no scope");
    }
}

#[derive(Deserialize)]
struct Schema {
    source: String,
    license: String,
    mutations: BTreeMap<String, BTreeMap<String, String>>,
    inputs: BTreeMap<String, BTreeMap<String, String>>,
}

fn schema() -> Schema {
    serde_json::from_str(include_str!("../../tests/fixtures/gh/schema-extract.json")).unwrap()
}

fn named(t: &str) -> &str {
    t.trim_matches(|c| matches!(c, '[' | ']' | '!'))
}

/// Inputs typed `ID` that name an actor (a user, team or bot) or an owner's
/// issue metadata (types, fields, statuses): no repository's object, so
/// nothing a fence could place them in.
const NOT_IN_A_REPO: &[&str] = &[
    "actorId",
    "actorIds",
    "assigneeIds",
    "botIds",
    "teamIds",
    "userIds",
    "fieldId",
    "issueTypeId",
    "multiSelectOptionIds",
    "singleSelectOptionId",
    "statusId",
];

/// Every leaf under `t` as `(fence path, type)`, `[]` after each list step;
/// with each input object holding both `owner` and `name`.
fn leaves(s: &Schema, path: &str, t: &str, seen: &mut Vec<String>, out: &mut Vec<(String, String)>, pairs: &mut Vec<String>) {
    let path = match t.trim_end_matches('!').starts_with('[') {
        true => format!("{path}[]"),
        false => path.to_string(),
    };
    let Some(fields) = s.inputs.get(named(t)) else {
        out.push((path, t.to_string()));
        return;
    };
    if seen.iter().any(|n| n == named(t)) {
        return;
    }
    seen.push(named(t).to_string());
    if fields.contains_key("owner") && fields.contains_key("name") {
        pairs.push(path.clone());
    }
    for (name, ft) in fields {
        leaves(s, &format!("{path}.{name}"), ft, seen, out, pairs);
    }
    seen.pop();
}

/// What the plan's schema walk flags: an `ID`, a `…RepositoryId`/`ownerId`
/// or a `…NameWithOwner`.
fn names_a_repo(path: &str, t: &str) -> bool {
    let last = path.rsplit('.').next().unwrap_or(path).trim_end_matches("[]");
    let lower = last.to_ascii_lowercase();
    named(t) == "ID" || lower.ends_with("repositoryid") || lower == "ownerid" || lower.ends_with("namewithowner")
}

#[test]
fn the_schema_extract_says_where_it_comes_from() {
    let s = schema();
    assert_eq!(s.source, "https://docs.github.com/public/fpt/schema.docs.graphql");
    assert!(s.license.contains("CC BY 4.0"), "{}", s.license);
}

#[test]
fn every_mapped_mutation_fences_each_input_naming_a_repo() {
    let c = claim();
    let s = schema();
    let mapped: BTreeSet<&str> = c
        .scopes
        .values()
        .flat_map(|sc| &sc.routes)
        .chain(&c.never)
        .filter_map(|r| match r {
            Route::GraphqlMutation(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    let extracted: BTreeSet<&str> = s.mutations.keys().map(String::as_str).collect();
    assert_eq!(mapped, extracted, "regenerate tests/fixtures/gh/schema-extract.json (scripts/gh-schema-extract.py)");

    let mut problems = Vec::new();
    for (name, args) in &s.mutations {
        let fence: BTreeSet<&str> = c
            .fence
            .get(&Route::GraphqlMutation(name.clone()))
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        let (mut out, mut pairs) = (Vec::new(), Vec::new());
        for (arg, t) in args {
            leaves(&s, arg, t, &mut Vec::new(), &mut out, &mut pairs);
        }
        for p in pairs {
            problems.push(format!("{name}: `{p}` names a repo by `owner` and `name`, which no `fence` input can place"));
        }
        for (path, t) in &out {
            let last = path.rsplit('.').next().unwrap_or(path).trim_end_matches("[]");
            if names_a_repo(path, t) && !fence.contains(path.as_str()) && !NOT_IN_A_REPO.contains(&last) {
                problems.push(format!("{name}: `{path}` ({t}) isn't in its `fence`"));
            }
        }
        for f in &fence {
            match out.iter().find(|(p, _)| p == f) {
                Some((_, t)) if matches!(named(t), "ID" | "String") => {}
                Some((_, t)) => problems.push(format!("{name}: `fence` input `{f}` is a {t}, not an id or a name")),
                None => problems.push(format!("{name}: `fence` input `{f}` isn't in the schema")),
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn the_walk_flags_what_the_plan_names() {
    assert!(names_a_repo("input.subjectId", "ID!"));
    assert!(names_a_repo("input.headRepositoryId", "String"));
    assert!(names_a_repo("input.ownerId", "String"));
    assert!(names_a_repo("input.branch.repositoryNameWithOwner", "String"));
    assert!(names_a_repo("input.labelIds[]", "[ID!]"));
    assert!(!names_a_repo("input.title", "String!"));
    assert!(!names_a_repo("input.name", "String"));

    let s: Schema = serde_json::from_value(serde_json::json!({
        "source": "", "license": "",
        "mutations": { "m": { "input": "In!" } },
        "inputs": {
            "In": { "items": "[Item!]", "self": "In", "target": "Target" },
            "Item": { "id": "ID!" },
            "Target": { "owner": "String!", "name": "String!" }
        }
    }))
    .unwrap();
    let (mut out, mut pairs) = (Vec::new(), Vec::new());
    leaves(&s, "input", "In!", &mut Vec::new(), &mut out, &mut pairs);
    assert!(out.contains(&("input.items[].id".into(), "ID!".into())), "{out:?}");
    assert_eq!(pairs, ["input.target"]);
}
