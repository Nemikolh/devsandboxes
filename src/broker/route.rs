//! A request's path against a claim's routes: the strict path check every
//! call passes before policy reads anything else, and the match that maps a
//! method and path to a scope, a `never` route or `other`
//! (docs/plans/active/github-and-sentry-access.md, *Matching*).
//!
//! The query string never takes part. Segments compare raw, percent-encoding
//! and case included: a route's literal segments are unreserved characters
//! only (`claim::check_path`), so an encoded form of one never matches it
//! and lands in `other`, never in a wider scope. The one exception is the
//! dot check: `%2e` in any case is refused like `.`, so no encoded
//! dot-segment reaches an upstream that would resolve it.

use crate::credentials::claim::{self, Method, Pinned, Route};

/// Why a path is refused before policy: dot-segments, encoded dots and
/// empty segments (`//`) anywhere, which an upstream may resolve to a
/// path the route match never saw.
pub fn check_path(path: &str) -> Result<(), &'static str> {
    let path = path_only(path);
    if !path.starts_with('/') {
        return Err("a path that isn't absolute");
    }
    if path.to_ascii_lowercase().contains("%2e") {
        return Err("an encoded dot (`%2e`) in the path");
    }
    let segments: Vec<&str> = path[1..].split('/').collect();
    for (i, seg) in segments.iter().enumerate() {
        match *seg {
            "." | ".." => return Err("a dot-segment (`.` or `..`) in the path"),
            "" if i + 1 < segments.len() => return Err("an empty segment (`//`) in the path"),
            _ => {}
        }
    }
    Ok(())
}

/// The target's path without its query.
pub fn path_only(target: &str) -> &str {
    target.split_once('?').map_or(target, |(p, _)| p)
}

/// `{var}` takes one non-empty segment; a literal one equals it byte for
/// byte; a trailing `/` must match too.
pub fn matches(route_path: &str, path: &str) -> bool {
    let want: Vec<&str> = route_path.split('/').collect();
    let got: Vec<&str> = path_only(path).split('/').collect();
    want.len() == got.len()
        && want.iter().zip(&got).all(|(w, g)| match w.strip_prefix('{').and_then(|v| v.strip_suffix('}')) {
            Some(_) => !g.is_empty(),
            None => w == g,
        })
}

/// Whether `route` is this HTTP request.
pub fn is(route: &Route, m: &str, path: &str) -> bool {
    match route {
        Route::Http { method: rm, path: rp } => Method::parse(m) == Some(*rm) && matches(rp, path),
        Route::GraphqlMutation(_) => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scoped {
    Never,
    Scope(String),
    /// No route of the claim: [`claim::OTHER_SCOPE`].
    Other,
}

impl Scoped {
    pub fn name(&self) -> &str {
        match self {
            Scoped::Never => "never",
            Scoped::Scope(s) => s,
            Scoped::Other => claim::OTHER_SCOPE,
        }
    }
}

/// `never` first: a route is in one scope or `never` (`claim::parse`), but
/// two patterns may both match one path, and `never` must win.
pub fn scope_of(claim: &Pinned, m: &str, path: &str) -> Scoped {
    if claim.never.iter().any(|r| is(r, m, path)) {
        return Scoped::Never;
    }
    claim
        .scopes
        .iter()
        .find(|(_, s)| s.routes.iter().any(|r| is(r, m, path)))
        .map_or(Scoped::Other, |(name, _)| Scoped::Scope(name.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentry() -> Pinned {
        crate::credentials::home::tests::sentry().pinned
    }

    #[test]
    fn strict_paths() {
        for ok in ["/", "/api/0/", "/api/0/projects/a/b/?x=..&y=//", "/a.b/..c/c../%41", "/x?"] {
            assert_eq!(check_path(ok), Ok(()), "{ok}");
        }
        for bad in [
            "/api/0/../x",
            "/api/0/./x",
            "/..",
            "/.",
            "/api/0/%2e%2e/x",
            "/api/0/%2E./x",
            "/api/0/a%2eb/",
            "//api/0/",
            "/api//0/",
            "*",
        ] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn route_matching() {
        assert!(matches("/api/0/organizations/{org}/", "/api/0/organizations/acme/"));
        assert!(matches("/api/0/organizations/{org}/", "/api/0/organizations/acme/?cursor=1"));
        assert!(!matches("/api/0/organizations/{org}/", "/api/0/organizations/acme"), "trailing slash");
        assert!(!matches("/api/0/organizations/{org}/", "/api/0/organizations//"), "empty var");
        assert!(!matches("/api/0/organizations/{org}/", "/api/0/organizations/a/b/"));
        assert!(!matches("/api/0/", "/API/0/"), "case-sensitive");
        assert!(!matches("/api/0/", "/%61pi/0/"), "raw, not decoded");
    }

    #[test]
    fn scopes_never_and_other() {
        let c = sentry();
        assert_eq!(scope_of(&c, "GET", "/api/0/"), Scoped::Scope("org.read".into()));
        assert_eq!(scope_of(&c, "GET", "/api/0/organizations/acme/chunk-upload/"), Scoped::Scope("uploads".into()));
        assert_eq!(scope_of(&c, "POST", "/api/0/projects/a/b/keys/"), Scoped::Never);
        assert_eq!(scope_of(&c, "POST", "/api/0/"), Scoped::Other);
        assert_eq!(scope_of(&c, "get", "/api/0/"), Scoped::Other, "methods are case-sensitive");
        assert_eq!(scope_of(&c, "TRACE", "/api/0/"), Scoped::Other);
        assert_eq!(Scoped::Other.name(), "other");
    }
}
