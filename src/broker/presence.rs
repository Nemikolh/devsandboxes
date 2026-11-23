//! `presence` on REST routes and GraphQL mutations (credential-broker,
//! *Shape*, "`presence`"; github-and-sentry-access, *gh*'s "presence"):
//! which of a claim's `presence` routes a call is, and the target each
//! names, pure over the request and its body. An id target is named as the
//! call holds it ([`Named::Release`], [`Named::RefNode`]) and resolved by
//! the home with the credential (`policy`, over [`super::github::Lookup`]),
//! never taken from the sandbox. A ref target (a `git/refs` route's, a
//! `createRef`/`updateRef`/`deleteRef` input) is presence only when the
//! claim's `ref` entries name its full ref for its operation
//! ([`ref_asked`]): tags and ref deletions, whichever route moves them.

use serde_json::Value as Json;

use super::graphql::{self, Leaf};
use super::route;
use crate::credentials::claim::{self, Method, Pinned, Presence, RefOp, Route};

/// The bound on a presence route's body, read whole before policy.
pub const MAX_BODY: u64 = 1024 * 1024;

/// GitHub's release id: the release's `tag_name` is what publishing it acts
/// on.
const RELEASE_ID: &str = "{release_id}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    /// As the call holds it.
    Target(String),
    /// The release at the call's own path: its `tag_name`, looked up.
    Release,
    /// A `Ref`'s node id: its full name, looked up.
    RefNode(String),
}

/// One presence target of a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The claim's `target` field, for a refusal.
    pub field: String,
    pub named: Named,
    /// A ref target's operation: asked only when [`ref_asked`].
    pub ref_op: Option<RefOp>,
}

/// Whether a ref operation on full ref `name` is presence: a `ref` entry's
/// pattern holds it and its `on` the operation (empty: every one).
pub fn ref_asked(claim: &Pinned, name: &str, op: RefOp) -> bool {
    claim.presence.iter().any(|p| match p {
        Presence::Ref { pattern, on } => claim::ref_pattern_matches(pattern, name) && (on.is_empty() || on.contains(&op)),
        Presence::Route { .. } => false,
    })
}

/// The `git/refs` segment a ref route's path holds the ref past.
const GIT_REFS: &str = "/git/refs/";

fn ref_op_of_method(method: Method) -> Option<RefOp> {
    match method {
        Method::Post => Some(RefOp::Create),
        Method::Patch => Some(RefOp::Update),
        Method::Delete => Some(RefOp::Delete),
        _ => None,
    }
}

/// The ref mutations' operations.
fn ref_op_of_mutation(name: &str) -> Option<RefOp> {
    match name {
        "createRef" => Some(RefOp::Create),
        "updateRef" => Some(RefOp::Update),
        "deleteRef" => Some(RefOp::Delete),
        _ => None,
    }
}

/// A `git/refs` route's full ref from the request: `refs/` and the route's
/// segments past `git/refs/`, each `{var}` the segment the path gave it
/// (`heads/{prefix}/{branch}` → `refs/heads/a/x`).
fn path_ref(route_path: &str, path: &str) -> Option<String> {
    let (_, rest) = route_path.split_once(GIT_REFS)?;
    let segs: Option<Vec<String>> = rest
        .split('/')
        .map(|seg| match seg.starts_with('{') {
            true => route::var_value(route_path, path, seg),
            false => Some(seg.to_string()),
        })
        .collect();
    Some(format!("refs/{}", segs?.join("/")))
}

/// Why a presence call's target can't be named: its `target` field and the
/// reason. The call is refused, never passed unasked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unnamed {
    pub field: String,
    pub why: String,
}

fn http_routes<'a>(claim: &'a Pinned, method: &'a str, path: &'a str) -> impl Iterator<Item = (&'a Route, &'a str)> {
    claim.presence.iter().filter_map(move |p| match p {
        Presence::Route { route, target } if route::is(route, method, path) => Some((route, target.as_str())),
        _ => None,
    })
}

/// A target that isn't a `{var}` of the path is a body field.
fn body_field(target: &str) -> bool {
    !target.starts_with('{')
}

/// A release's create or edit route: whether it's presence turns on its
/// body's `draft`.
fn release(route: &Route) -> Option<Method> {
    match route {
        Route::Http { method, path } if path.ends_with("/releases") || path.ends_with("/releases/{release_id}") => {
            Some(*method)
        }
        _ => None,
    }
}

/// Whether `serve` reads the body before policy: a presence route whose
/// target is a body field, or a release's.
pub fn reads_body(claim: &Pinned, method: &str, path: &str) -> bool {
    http_routes(claim, method, path).any(|(r, t)| body_field(t) || release(r).is_some())
}

/// Draft-then-publish is one ask: creating a draft isn't presence, and an
/// edit is only when it publishes (`"draft": false`).
fn draft_exempt(route: &Route, body: Option<&Json>) -> bool {
    let draft = body.and_then(|b| b.get("draft"));
    match release(route) {
        Some(Method::Post) => draft == Some(&Json::Bool(true)),
        Some(Method::Patch) => draft != Some(&Json::Bool(false)),
        _ => false,
    }
}

/// The presence targets of an HTTP call; `body` is its JSON when
/// [`reads_body`] (`None` there is the caller's bug, refused).
pub fn http(claim: &Pinned, method: &str, path: &str, body: Option<&Json>) -> Result<Vec<Found>, Unnamed> {
    let mut out = Vec::new();
    for (r, target) in http_routes(claim, method, path) {
        if draft_exempt(r, body) {
            continue;
        }
        let unnamed = |why: &str| Unnamed { field: target.to_string(), why: why.to_string() };
        let Route::Http { method: rm, path: rp } = r else { unreachable!("an HTTP presence route") };
        let ref_op = rp.contains(GIT_REFS.trim_end_matches('/')).then(|| ref_op_of_method(*rm)).flatten();
        let named = if target == RELEASE_ID {
            vec![Named::Release]
        } else if !body_field(target) {
            let value = match ref_op {
                Some(_) => path_ref(rp, path),
                None => route::var_value(rp, path, target),
            };
            vec![Named::Target(value.ok_or_else(|| unnamed("the path holds none"))?)]
        } else {
            let body = body.ok_or_else(|| unnamed("its body wasn't read"))?;
            strings(graphql::json_inputs(body, target), &unnamed)?.into_iter().map(Named::Target).collect()
        };
        for named in named {
            out.push(full_ref(Found { field: target.to_string(), named, ref_op })?);
        }
    }
    Ok(out)
}

/// The presence targets of one mutation field: a `…Id` input is a `Ref`'s
/// node id, any other the ref's name as sent.
pub fn mutation(claim: &Pinned, field: &graphql::Field) -> Result<Vec<Found>, Unnamed> {
    let mut out = Vec::new();
    for p in &claim.presence {
        let Presence::Route { route: Route::GraphqlMutation(name), target } = p else { continue };
        if *name != field.name {
            continue;
        }
        let unnamed = |why: &str| Unnamed { field: target.clone(), why: why.to_string() };
        let last = target.rsplit('.').next().unwrap_or(target).trim_end_matches("[]");
        let ref_op = ref_op_of_mutation(name);
        for s in strings(graphql::inputs(field, target), &unnamed)? {
            let named = if last.ends_with("Id") { Named::RefNode(s) } else { Named::Target(s) };
            out.push(full_ref(Found { field: target.clone(), named, ref_op })?);
        }
    }
    Ok(out)
}

/// A ref target named as sent must be a full ref: GitHub takes no other,
/// and a short one would match no `ref` entry and pass unasked.
fn full_ref(f: Found) -> Result<Found, Unnamed> {
    match (&f.named, f.ref_op) {
        (Named::Target(s), Some(_)) if !s.starts_with("refs/") => {
            Err(Unnamed { field: f.field, why: format!("`{s}` isn't a full ref (`refs/…`)") })
        }
        _ => Ok(f),
    }
}

/// The non-empty strings an input read holds, at least one.
fn strings(read: Result<Vec<Leaf>, String>, unnamed: &dyn Fn(&str) -> Unnamed) -> Result<Vec<String>, Unnamed> {
    let leaves = read.map_err(|why| unnamed(&why))?;
    if leaves.is_empty() {
        return Err(unnamed("the call holds none"));
    }
    leaves
        .into_iter()
        .map(|l| match l {
            Leaf::Str(s) if !s.is_empty() => Ok(s),
            Leaf::Str(_) => Err(unnamed("it's empty")),
            Leaf::Int(i) => Err(unnamed(&format!("it holds {i}, not a name"))),
        })
        .collect()
}

#[cfg(test)]
mod tests;
