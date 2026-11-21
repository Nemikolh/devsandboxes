//! A request's path against a claim's routes: the strict path check every
//! call passes before policy reads anything else, and the match that maps a
//! method and path to a scope, a `never` route or `other`
//! (docs/plans/active/github-and-sentry-access.md, *Matching*).
//!
//! The query string takes part only where a route constrains it
//! (`?service=git-receive-pack`: that parameter, once, with that raw
//! value). Segments compare raw, percent-encoding and case included: a
//! route's literal segments are unreserved characters only
//! (`claim::check_path`), so an encoded form of one never matches it and
//! lands in `other`, never in a wider scope. The one exception is the dot
//! check: `%2e` in any case is refused like `.`, so no encoded dot-segment
//! reaches an upstream that would resolve it.
//!
//! `{repo}` takes two segments, `owner/repo`, strict as
//! `fence::normalize` reads them (case-folded, `.git`/`.wiki.git`
//! dropped, no encoded character). Past it, `%2F` is decoded once and
//! the decoded path matched (gh sends `git/ref/tags%2Fv1`); a double
//! encoding (`%25XX`) or a dot-segment or `//` after decoding is refused.

use crate::credentials::claim::{self, Method, Pinned, Route};
use crate::credentials::fence;

/// The variable that names a repo: two segments, attributed by the fence.
pub const REPO: &str = "{repo}";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    No,
    /// With the normalized `owner/repo` its `{repo}` took.
    Yes(Option<String>),
    /// Past `{repo}` the path decodes to one an upstream may resolve
    /// differently.
    Refused(&'static str),
}

/// `{var}` takes one non-empty segment, [`REPO`] two; a literal one equals
/// it byte for byte; a trailing `/` must match too.
pub fn matches(route_path: &str, path: &str) -> Match {
    let (want_path, want_query) = route_path.split_once('?').map_or((route_path, None), |(p, q)| (p, Some(q)));
    let want: Vec<&str> = want_path.split('/').collect();
    let got: Vec<&str> = path_only(path).split('/').collect();
    let query_ok = || want_query.is_none_or(|q| query_holds(q, path));
    let Some(at) = want.iter().position(|w| *w == REPO) else {
        return match segments_match(&want, &got) && query_ok() {
            true => Match::Yes(None),
            false => Match::No,
        };
    };
    if got.len() < at + 2 || !segments_match(&want[..at], &got[..at]) {
        return Match::No;
    }
    let Some(repo) = fence::normalize(got[at], got[at + 1]) else { return Match::No };
    let rest = match decoded_rest(&got[at + 2..]) {
        Ok(rest) => rest,
        Err(why) => return Match::Refused(why),
    };
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    match segments_match(&want[at + 1..], &rest) && query_ok() {
        true => Match::Yes(Some(repo)),
        false => Match::No,
    }
}

fn segments_match(want: &[&str], got: &[&str]) -> bool {
    want.len() == got.len()
        && want.iter().zip(got).all(|(w, g)| match w.strip_prefix('{').and_then(|v| v.strip_suffix('}')) {
            Some(_) => !g.is_empty(),
            None => w == g,
        })
}

/// The segments past the repo with `%2F` decoded once.
fn decoded_rest(raw: &[&str]) -> Result<Vec<String>, &'static str> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let joined = raw.join("/");
    let b = joined.as_bytes();
    let double = (0..b.len().saturating_sub(4))
        .any(|i| b[i] == b'%' && b[i + 1..i + 3] == *b"25" && b[i + 3..i + 5].iter().all(u8::is_ascii_hexdigit));
    if double {
        return Err("a double-encoded character (`%25XX`) past the repo");
    }
    let decoded = joined.replace("%2F", "/").replace("%2f", "/");
    let segments: Vec<String> = decoded.split('/').map(str::to_string).collect();
    for (i, seg) in segments.iter().enumerate() {
        match seg.as_str() {
            "." | ".." => return Err("a dot-segment past the repo once `%2F` is decoded"),
            "" if i + 1 < segments.len() => return Err("an empty segment past the repo once `%2F` is decoded"),
            _ => {}
        }
    }
    Ok(segments)
}

/// `name=value` is in the target's query exactly once for `name`, raw.
fn query_holds(constraint: &str, target: &str) -> bool {
    let Some((name, value)) = constraint.split_once('=') else { return false };
    let mut values = target
        .split_once('?')
        .map(|(_, q)| q)
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter(|pair| pair.split_once('=').map_or(*pair, |(k, _)| k) == name)
        .map(|pair| pair.split_once('=').map_or("", |(_, v)| v));
    values.next() == Some(value) && values.next().is_none()
}

pub fn match_route(route: &Route, m: &str, path: &str) -> Match {
    match route {
        Route::Http { method: rm, path: rp } if Method::parse(m) == Some(*rm) => matches(rp, path),
        _ => Match::No,
    }
}

/// Whether `route` is this HTTP request.
pub fn is(route: &Route, m: &str, path: &str) -> bool {
    matches!(match_route(route, m, path), Match::Yes(_))
}

/// The git smart-HTTP service a request is part of: `…/info/refs` with one
/// `service=git-upload-pack|git-receive-pack`, or a `POST` to
/// `…/git-upload-pack|git-receive-pack`.
pub fn git_service(m: &str, target: &str) -> Option<&'static str> {
    const SERVICES: [&str; 2] = ["git-upload-pack", "git-receive-pack"];
    let path = path_only(target);
    if path.ends_with("/info/refs") && matches!(m, "GET" | "HEAD") {
        return SERVICES.into_iter().find(|s| query_holds(&format!("service={s}"), target));
    }
    let last = path.rsplit('/').next()?;
    SERVICES.into_iter().find(|s| m == "POST" && *s == last)
}

/// A call that would change something upstream: git's receive-pack, and
/// any method but `GET`/`HEAD` other than upload-pack (a fetch's `POST`).
pub fn is_write(m: &str, target: &str) -> bool {
    match git_service(m, target) {
        Some(s) => s == "git-receive-pack",
        None => !matches!(m, "GET" | "HEAD"),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    pub scoped: Scoped,
    /// `None` for `other`.
    pub route: Option<Route>,
    /// What the route's `{repo}` took.
    pub repo: Option<String>,
}

/// `never` first: a route is in one scope or `never` (`claim::parse`), but
/// two patterns may both match one path, and `never` must win; then a
/// route constraining the query over one that doesn't (`info/refs`'s
/// `?service=`). `Err`: a refused path.
pub fn scope_of(claim: &Pinned, m: &str, path: &str) -> Result<Matched, &'static str> {
    let first = |routes: &mut dyn Iterator<Item = &Route>| -> Result<Option<(Route, Option<String>)>, &'static str> {
        for r in routes {
            match match_route(r, m, path) {
                Match::No => {}
                Match::Yes(repo) => return Ok(Some((r.clone(), repo))),
                Match::Refused(why) => return Err(why),
            }
        }
        Ok(None)
    };
    if let Some((route, repo)) = first(&mut claim.never.iter())? {
        return Ok(Matched { scoped: Scoped::Never, route: Some(route), repo });
    }
    let constrained = |r: &Route| matches!(r, Route::Http { path, .. } if path.contains('?'));
    for pass in [true, false] {
        for (name, s) in &claim.scopes {
            if let Some((route, repo)) = first(&mut s.routes.iter().filter(|r| constrained(r) == pass))? {
                return Ok(Matched { scoped: Scoped::Scope(name.clone()), route: Some(route), repo });
            }
        }
    }
    Ok(Matched { scoped: Scoped::Other, route: None, repo: None })
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

    fn yes(route: &str, path: &str) -> bool {
        matches!(matches(route, path), Match::Yes(_))
    }

    #[test]
    fn route_matching() {
        assert!(yes("/api/0/organizations/{org}/", "/api/0/organizations/acme/"));
        assert!(yes("/api/0/organizations/{org}/", "/api/0/organizations/acme/?cursor=1"));
        assert!(!yes("/api/0/organizations/{org}/", "/api/0/organizations/acme"), "trailing slash");
        assert!(!yes("/api/0/organizations/{org}/", "/api/0/organizations//"), "empty var");
        assert!(!yes("/api/0/organizations/{org}/", "/api/0/organizations/a/b/"));
        assert!(!yes("/api/0/", "/API/0/"), "case-sensitive");
        assert!(!yes("/api/0/", "/%61pi/0/"), "raw, not decoded");
        assert_eq!(matches("/api/{x}/", "/api/a%2Fb/"), Match::Yes(None), "no decoding without `{{repo}}`");
    }

    #[test]
    fn repo_takes_two_strict_segments() {
        let repo = |route: &str, path: &str| matches(route, path);
        let r = |s: &str| Match::Yes(Some(s.to_string()));
        assert_eq!(repo("/repos/{repo}/pulls", "/repos/Owner/Repo/pulls"), r("owner/repo"));
        assert_eq!(repo("/{repo}/info/refs", "/o/r.git/info/refs?service=x"), r("o/r"));
        assert_eq!(repo("/{repo}/info/refs", "/o/r.wiki.git/info/refs"), r("o/r"));
        assert_eq!(repo("/{repo}/info/refs", "/O/R/info/refs"), r("o/r"));
        assert_eq!(repo("/repos/{repo}", "/repos/o/r"), r("o/r"));
        assert_eq!(repo("/repos/{repo}", "/repos/o/r/"), Match::No, "trailing slash");
        assert_eq!(repo("/repos/{repo}", "/repos/o"), Match::No);
        assert_eq!(repo("/repos/{repo}/pulls", "/repos/o/r/x/pulls"), Match::No, "exactly two");
        for encoded in ["/repos/o%2Dx/r/pulls", "/repos/o/r%41/pulls", "/repos/o%2Fr/x/pulls", "/repos/o_x/r/pulls"] {
            assert_eq!(repo("/repos/{repo}/pulls", encoded), Match::No, "{encoded}");
        }
    }

    #[test]
    fn past_the_repo_slashes_decode_once() {
        let tags = "/repos/{repo}/git/ref/tags/{tag}";
        assert_eq!(matches(tags, "/repos/o/r/git/ref/tags%2Fv1"), Match::Yes(Some("o/r".into())));
        assert_eq!(matches(tags, "/repos/o/r/git/ref/tags%2fv1"), Match::Yes(Some("o/r".into())));
        assert_eq!(matches(tags, "/repos/o/r/git/ref/tags/v1"), Match::Yes(Some("o/r".into())));
        assert_eq!(matches(tags, "/repos/o/r/git/ref/tags%2Fv1%2Fx"), Match::No);
        let contents = "/repos/{repo}/contents/{path}";
        for bad in [
            "/repos/o/r/contents/tags%252Fv1",
            "/repos/o/r/contents/a%2F..%2Fb",
            "/repos/o/r/contents/..%2Fx",
            "/repos/o/r/contents/a%2F.%2Fb",
            "/repos/o/r/contents/a%2F%2Fb",
        ] {
            assert!(matches!(matches(contents, bad), Match::Refused(_)), "{bad}");
        }
        assert_eq!(matches(contents, "/repos/o/r/contents/a%25b"), Match::Yes(Some("o/r".into())), "a lone `%25`");
    }

    #[test]
    fn query_constraints() {
        let push = "/{repo}/info/refs?service=git-receive-pack";
        assert!(yes(push, "/o/r.git/info/refs?service=git-receive-pack"));
        assert!(yes(push, "/o/r.git/info/refs?x=1&service=git-receive-pack&y"), "others ignored");
        assert!(!yes(push, "/o/r.git/info/refs?service=git-upload-pack"));
        assert!(!yes(push, "/o/r.git/info/refs"));
        assert!(!yes(push, "/o/r.git/info/refs?service=git-receive-pack&service=git-upload-pack"), "once");
        assert!(!yes(push, "/o/r.git/info/refs?service=git%2Dreceive-pack"), "raw");
        assert!(!yes(push, "/o/r.git/info/refs?Service=git-receive-pack"));
    }

    #[test]
    fn git_services_and_writes() {
        assert_eq!(git_service("GET", "/o/r.git/info/refs?service=git-upload-pack"), Some("git-upload-pack"));
        assert_eq!(git_service("GET", "/o/r/info/refs?service=git-receive-pack"), Some("git-receive-pack"));
        assert_eq!(git_service("GET", "/o/r/info/refs"), None);
        assert_eq!(git_service("POST", "/o/r.git/git-upload-pack"), Some("git-upload-pack"));
        assert_eq!(git_service("GET", "/o/r.git/git-upload-pack"), None);
        assert!(!is_write("POST", "/o/r.git/git-upload-pack"));
        assert!(is_write("POST", "/o/r.git/git-receive-pack"));
        assert!(is_write("GET", "/o/r.git/info/refs?service=git-receive-pack"));
        assert!(!is_write("GET", "/repos/o/r"));
        assert!(!is_write("HEAD", "/repos/o/r"));
        assert!(is_write("POST", "/repos/o/r/pulls"));
        assert!(is_write("POST", "/o/r/info/lfs/objects/batch"));
    }

    #[test]
    fn scopes_never_and_other() {
        let c = sentry();
        let scope = |m: &str, p: &str| scope_of(&c, m, p).unwrap().scoped;
        assert_eq!(scope("GET", "/api/0/"), Scoped::Scope("org.read".into()));
        assert_eq!(scope("GET", "/api/0/organizations/acme/chunk-upload/"), Scoped::Scope("uploads".into()));
        assert_eq!(scope("POST", "/api/0/projects/a/b/keys/"), Scoped::Never);
        assert_eq!(scope("POST", "/api/0/"), Scoped::Other);
        assert_eq!(scope("get", "/api/0/"), Scoped::Other, "methods are case-sensitive");
        assert_eq!(scope("TRACE", "/api/0/"), Scoped::Other);
        assert_eq!(Scoped::Other.name(), "other");
    }

    #[test]
    fn github_scopes_by_service_and_repo() {
        let mut c = crate::credentials::home::tests::github().pinned;
        let m = |c: &Pinned, m: &str, p: &str| scope_of(c, m, p).unwrap();
        let fetch = m(&c, "GET", "/o/r.git/info/refs?service=git-upload-pack");
        assert_eq!((fetch.scoped, fetch.repo.as_deref()), (Scoped::Scope("repo.read".into()), Some("o/r")));
        assert_eq!(m(&c, "GET", "/o/r.git/info/refs?service=git-receive-pack").scoped, Scoped::Scope("git.push".into()));
        assert_eq!(m(&c, "GET", "/o/r.git/info/refs").scoped, Scoped::Other);
        assert_eq!(m(&c, "POST", "/o/r.git/git-receive-pack").scoped, Scoped::Scope("git.push".into()));
        assert_eq!(m(&c, "POST", "/repos/o/r/keys").scoped, Scoped::Never);
        let user = m(&c, "GET", "/user");
        assert_eq!((user.scoped, user.repo), (Scoped::Scope("user.read".into()), None));
        assert!(scope_of(&c, "GET", "/repos/o/r/contents/a%2F..%2Fb").is_err());

        // A constrained route wins over an unconstrained one in another scope.
        c.scopes.get_mut("user.read").unwrap().routes.push(Route::parse("GET /{repo}/info/refs").unwrap());
        assert_eq!(m(&c, "GET", "/o/r/info/refs?service=git-receive-pack").scoped, Scoped::Scope("git.push".into()));
        assert_eq!(m(&c, "GET", "/o/r/info/refs?service=x").scoped, Scoped::Scope("user.read".into()));
    }
}
