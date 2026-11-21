//! Renewing an OAuth login's token behind the phantom
//! (docs/architecture/credentials/broker.md, *Renewal*): RFC 6749 §6's
//! `refresh_token` grant, ahead of expiry by [`MARGIN`] or after an upstream
//! 401, one refresh at a time per credential ([`Renewer`]).
//!
//! A refused refresh marks the credential broken in `credentials.toml`
//! (`home::Entry::broken`); a provider that can't be reached marks nothing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use super::{Http, OAuthError, Reply, TokenSet};
use crate::credentials::home::{self, Broken};
use crate::credentials::store::{OAuthToken, Record, Secret, Store};

/// Renewed this long before `expires_at`: longer than a long request (the
/// upstream client idles up to 5 minutes for a first byte), so no token
/// expires mid-call.
pub const MARGIN: u64 = 10 * 60;

/// A 401 this soon after a refresh doesn't refresh again: the token is new,
/// so the 401 is about something else.
pub const RECENT: u64 = 60;

/// Whether `t` is within [`MARGIN`] of its expiry at `now` (unix seconds).
/// No `expires_at`: never.
pub fn due(t: &OAuthToken, now: u64) -> bool {
    t.expires_at.is_some_and(|at| now.saturating_add(MARGIN) >= at)
}

#[derive(Debug)]
pub enum Refreshed {
    Token(OAuthToken),
    /// The provider said no: the refresh token is dead.
    Refused(String),
    /// Unreachable, or an answer that is neither a token nor a refusal.
    Unavailable(String),
}

/// Error codes that say "later", not "never".
fn transient(e: &OAuthError) -> bool {
    matches!(e.code.as_str(), "temporarily_unavailable" | "server_error" | "slow_down")
}

/// Asks `t`'s token endpoint for a new token with its refresh token.
pub fn refresh(http: &Http, t: &OAuthToken, now: u64) -> Refreshed {
    let Some(rt) = &t.refresh_token else {
        return Refreshed::Refused("no refresh token was issued".into());
    };
    let form = [("grant_type", "refresh_token"), ("refresh_token", rt.expose()), ("client_id", t.client_id.as_str())];
    match http.token(&t.token_url, &form) {
        Ok(Reply::Token(set)) => Refreshed::Token(renewed(t, set, now)),
        Ok(Reply::Error(e)) if transient(&e) => Refreshed::Unavailable(format!("the token endpoint answered {e}")),
        Ok(Reply::Error(e)) => Refreshed::Refused(e.to_string()),
        Err(e) => Refreshed::Unavailable(format!("{e:#}")),
    }
}

/// The record after a refresh. A new refresh token (GitHub rotates them)
/// comes with its own expiry; without one the old is kept with its expiry,
/// as RFC 6749 §6 lets a provider reissue none. So is the scope.
pub fn renewed(old: &OAuthToken, set: TokenSet, now: u64) -> OAuthToken {
    let rotated = set.refresh_token.is_some();
    let scope = set.scope.clone().or_else(|| old.scope.clone());
    let mut new = set.into_record(&old.token_url, &old.client_id, now);
    if !rotated {
        new.refresh_token = old.refresh_token.clone();
        new.refresh_expires_at = old.refresh_expires_at;
    }
    new.scope = scope;
    new
}

/// The token to inject.
#[derive(Debug, Clone)]
pub struct Fresh {
    pub token: Secret,
    /// An OAuth token with a refresh token: a 401 may renew it.
    pub renewable: bool,
    /// Renewed for this call.
    pub refreshed: bool,
}

fn fresh(rec: &Record, refreshed: bool) -> Fresh {
    Fresh {
        token: rec.access_token().clone(),
        renewable: matches!(rec, Record::OAuth(t) if t.refresh_token.is_some()),
        refreshed,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// No secret stored.
    Missing,
    /// The store can't be read or written; marked broken.
    Store(String),
    /// The provider refused the refresh; marked broken.
    Refused(String),
    /// The provider can't be reached now; not broken.
    Unavailable(String),
}

/// The broker's renewals, shared by every connection thread of the daemon.
pub struct Renewer {
    data: PathBuf,
    /// Per credential, held across its refresh so the connection threads
    /// wait for one refresh instead of each spending the refresh token;
    /// holds when this process last refreshed it (unix seconds). The
    /// store's file lock, taken inside, covers a login and a second daemon.
    flights: Mutex<HashMap<String, Arc<Mutex<Option<u64>>>>>,
}

impl Renewer {
    /// The store and `credentials.toml` in the data dir `data`.
    pub fn new(data: PathBuf) -> Renewer {
        Renewer { data, flights: Mutex::default() }
    }

    /// `name`'s token at `now`, renewed first when [`due`]. Only a due
    /// token takes a lock.
    pub fn token(&self, name: &str, now: u64, refresh: &dyn Fn(&OAuthToken) -> Refreshed) -> Result<Fresh, Failure> {
        match Store::in_data(&self.data).get(name) {
            Err(e) => Err(self.store_failure(name, &e, now)),
            Ok(None) => Err(Failure::Missing),
            Ok(Some(Record::OAuth(t))) if due(&t, now) && t.refresh_token.is_some() => {
                self.renew(name, now, refresh, None).and_then(|f| f.ok_or(Failure::Missing))
            }
            Ok(Some(r)) => Ok(fresh(&r, false)),
        }
    }

    /// After an upstream 401 to `used`: the token to replay with (renewed,
    /// or one another thread or a login stored meanwhile), or `None` when
    /// there's nothing newer to try (no refresh token, or refreshed within
    /// [`RECENT`]).
    pub fn after_401(
        &self,
        name: &str,
        used: &Secret,
        now: u64,
        refresh: &dyn Fn(&OAuthToken) -> Refreshed,
    ) -> Result<Option<Fresh>, Failure> {
        self.renew(name, now, refresh, Some(used))
    }

    fn renew(
        &self,
        name: &str,
        now: u64,
        refresh: &dyn Fn(&OAuthToken) -> Refreshed,
        after_401: Option<&Secret>,
    ) -> Result<Option<Fresh>, Failure> {
        let flight = self.flight(name);
        let mut last = flight.lock().unwrap_or_else(PoisonError::into_inner);
        let recent = last.is_some_and(|at| now < at.saturating_add(RECENT));
        // Re-read under both locks: whoever held them may have renewed it.
        let out = Store::in_data(&self.data).renew(name, |rec| {
            let Some(rec) = rec else { return (None, Err(Failure::Missing)) };
            let current = fresh(rec, false);
            let keep = |current: Fresh| (None, Ok(after_401.is_none().then_some(current)));
            let t = match rec {
                Record::OAuth(t) if t.refresh_token.is_some() => t,
                _ => return keep(current),
            };
            let wanted = match after_401 {
                None => due(t, now),
                Some(used) if t.access_token != *used => return (None, Ok(Some(current))),
                Some(_) => !recent,
            };
            if !wanted {
                return keep(current);
            }
            match refresh(t) {
                Refreshed::Token(new) => {
                    let f = Fresh {
                        token: new.access_token.clone(),
                        renewable: new.refresh_token.is_some(),
                        refreshed: true,
                    };
                    (Some(Record::OAuth(new)), Ok(Some(f)))
                }
                Refreshed::Refused(why) => (None, Err(Failure::Refused(why))),
                Refreshed::Unavailable(why) => (None, Err(Failure::Unavailable(why))),
            }
        });
        match out {
            Err(e) => Err(self.store_failure(name, &e, now)),
            Ok(Ok(Some(f))) if f.refreshed => {
                *last = Some(now);
                let _ = home::clear_broken(&home::path(&self.data), name);
                Ok(Some(f))
            }
            Ok(Err(Failure::Refused(why))) => {
                self.mark_broken(name, format!("its refresh was refused: {why}"), now);
                Err(Failure::Refused(why))
            }
            Ok(other) => other,
        }
    }

    fn flight(&self, name: &str) -> Arc<Mutex<Option<u64>>> {
        let mut map = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
        map.entry(name.to_string()).or_default().clone()
    }

    fn store_failure(&self, name: &str, e: &anyhow::Error, now: u64) -> Failure {
        let why = format!("{e:#}");
        self.mark_broken(name, format!("the credential store can't be used: {why}"), now);
        Failure::Store(why)
    }

    /// Best effort: the call is answered either way.
    pub fn mark_broken(&self, name: &str, reason: String, now: u64) {
        let broken = Broken { at: crate::inbox::rfc3339(now), reason };
        let _ = home::mark_broken(&home::path(&self.data), name, broken);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::credentials::home::{Entry, Home};
    use crate::credentials::oauth::parse_token_reply;
    use crate::credentials::oauth::tests::{GITHUB_TOKEN, UNIX, field, form, http, json, path};

    fn token(access: &str, refresh: Option<&str>, expires_at: Option<u64>, token_url: &str) -> OAuthToken {
        OAuthToken {
            access_token: Secret::new(access.into()),
            refresh_token: refresh.map(|r| Secret::new(r.into())),
            expires_at,
            refresh_expires_at: refresh.map(|_| UNIX + 1000),
            token_url: token_url.into(),
            client_id: "Iv1.c".into(),
            scope: Some("repo".into()),
        }
    }

    #[test]
    fn due_within_the_margin() {
        let t = token("a", Some("r"), Some(UNIX + MARGIN + 1), "https://x/t");
        assert!(!due(&t, UNIX));
        assert!(due(&t, UNIX + 1));
        assert!(due(&t, UNIX + MARGIN + 100), "expired");
        assert!(!due(&token("a", Some("r"), None, "https://x/t"), UNIX), "no lifetime given");
    }

    #[test]
    fn renewed_rotates_or_keeps_the_refresh_token() {
        let old = token("a", Some("ghr_old"), Some(UNIX), "https://x/t");
        let set = |body: &str| match parse_token_reply(200, Some("application/json"), body.as_bytes()).unwrap() {
            Reply::Token(t) => t,
            Reply::Error(_) => unreachable!(),
        };
        let new = renewed(&old, set(GITHUB_TOKEN), UNIX + 5);
        assert_eq!((new.access_token.expose(), new.refresh_token.as_ref().unwrap().expose()), ("ghu_live", "ghr_live"));
        assert_eq!((new.expires_at, new.refresh_expires_at), (Some(UNIX + 5 + 28800), Some(UNIX + 5 + 15897600)));
        assert_eq!((new.token_url.as_str(), new.client_id.as_str(), new.scope.as_deref()), ("https://x/t", "Iv1.c", Some("repo")));

        let new = renewed(&old, set(r#"{"access_token":"b","expires_in":60,"scope":"gist"}"#), UNIX);
        assert_eq!(new.refresh_token.as_ref().unwrap().expose(), "ghr_old");
        assert_eq!((new.refresh_expires_at, new.expires_at, new.scope.as_deref()), (old.refresh_expires_at, Some(UNIX + 60), Some("gist")));
    }

    /// A token endpoint taking `ghr_old` (rotating it), refusing `ghr_dead`,
    /// and saying "later" to `ghr_busy` and 500 to `ghr_500`.
    fn provider() -> (u16, crate::broker::serve::tests::Seen) {
        crate::broker::serve::tests::upstream_server(|head, body| {
            let f = form(body);
            assert_eq!(path(head), "/token");
            assert_eq!((field(&f, "grant_type"), field(&f, "client_id")), (Some("refresh_token"), Some("Iv1.c")));
            match field(&f, "refresh_token") {
                Some("ghr_old") => json(200, GITHUB_TOKEN),
                Some("ghr_busy") => json(400, r#"{"error":"temporarily_unavailable"}"#),
                Some("ghr_500") => json(500, "{}"),
                _ => json(400, r#"{"error":"invalid_grant","error_description":"The refresh token is bad"}"#),
            }
        })
    }

    #[test]
    fn refresh_against_the_token_endpoint() {
        let (port, seen) = provider();
        let url = format!("https://127.0.0.1:{port}/token");
        let http = http();
        let Refreshed::Token(t) = refresh(&http, &token("a", Some("ghr_old"), Some(UNIX), &url), UNIX) else { panic!() };
        assert_eq!((t.access_token.expose(), t.refresh_token.unwrap().expose()), ("ghu_live", "ghr_live"));
        assert!(matches!(
            refresh(&http, &token("a", Some("ghr_dead"), None, &url), UNIX),
            Refreshed::Refused(why) if why == "invalid_grant (The refresh token is bad)"
        ));
        assert!(matches!(refresh(&http, &token("a", Some("ghr_busy"), None, &url), UNIX), Refreshed::Unavailable(_)));
        let Refreshed::Unavailable(why) = refresh(&http, &token("a", Some("ghr_500"), None, &url), UNIX) else { panic!() };
        assert!(why.contains("answered 500"), "{why}");
        assert_eq!(seen.lock().unwrap().len(), 4);

        // Nothing listens there any more.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let Refreshed::Unavailable(why) =
            refresh(&http, &token("a", Some("ghr_old"), None, &format!("https://127.0.0.1:{closed}/token")), UNIX)
        else {
            panic!()
        };
        assert!(why.contains("cannot reach"), "{why}");
        assert!(matches!(refresh(&http, &token("a", None, None, &url), UNIX), Refreshed::Refused(_)));
    }

    struct Rig {
        dir: PathBuf,
        renewer: Renewer,
    }

    impl Rig {
        fn new(tag: &str, rec: OAuthToken) -> Rig {
            let dir = std::env::temp_dir().join(format!("dsv-refresh-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let home = Home::from([("gh".to_string(), Entry::new("github", None))]);
            std::fs::write(home::path(&dir), home::serialize(&home).unwrap()).unwrap();
            Store::in_data(&dir).put("gh", &Record::OAuth(rec)).unwrap();
            Rig { renewer: Renewer::new(dir.clone()), dir }
        }

        fn stored(&self) -> OAuthToken {
            match Store::in_data(&self.dir).get("gh").unwrap() {
                Some(Record::OAuth(t)) => t,
                other => panic!("{other:?}"),
            }
        }

        fn broken(&self) -> Option<Broken> {
            home::load(&home::path(&self.dir)).unwrap()["gh"].broken.clone()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Hands out `new-<n>` tokens, counting calls.
    fn counting(calls: &AtomicUsize) -> impl Fn(&OAuthToken) -> Refreshed + '_ {
        move |old| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            let mut t = old.clone();
            t.access_token = Secret::new(format!("new-{n}"));
            t.refresh_token = Some(Secret::new(format!("ghr-{n}")));
            t.expires_at = Some(UNIX + 28800);
            Refreshed::Token(t)
        }
    }

    #[test]
    fn renewed_only_when_due_and_the_rotation_is_stored() {
        let (port, _) = provider();
        let url = format!("https://127.0.0.1:{port}/token");
        let rig = Rig::new("due", token("old", Some("ghr_old"), Some(UNIX + MARGIN + 10), &url));
        let http = http();
        let calls = AtomicUsize::new(0);
        let call = |t: &OAuthToken| {
            calls.fetch_add(1, Ordering::SeqCst);
            refresh(&http, t, UNIX + 20)
        };
        let f = rig.renewer.token("gh", UNIX, &call).unwrap();
        assert_eq!((f.token.expose(), f.renewable, f.refreshed, calls.load(Ordering::SeqCst)), ("old", true, false, 0));

        let f = rig.renewer.token("gh", UNIX + 20, &call).unwrap();
        assert_eq!((f.token.expose(), f.refreshed), ("ghu_live", true));
        let t = rig.stored();
        assert_eq!((t.access_token.expose(), t.refresh_token.as_ref().unwrap().expose()), ("ghu_live", "ghr_live"));
        assert_eq!((t.expires_at, t.refresh_expires_at), (Some(UNIX + 20 + 28800), Some(UNIX + 20 + 15897600)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!rig.renewer.token("gh", UNIX + 30, &call).unwrap().refreshed);
        assert_eq!(rig.renewer.token("nope", UNIX, &call).unwrap_err(), Failure::Missing);
    }

    #[test]
    fn threads_share_one_refresh() {
        let rig = Rig::new("flight", token("old", Some("ghr_old"), Some(UNIX), "https://x/t"));
        let calls = AtomicUsize::new(0);
        let slow = |t: &OAuthToken| {
            std::thread::sleep(Duration::from_millis(100));
            counting(&calls)(t)
        };
        let barrier = Barrier::new(8);
        let tokens: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        rig.renewer.token("gh", UNIX, &slow).unwrap().token.expose().to_string()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(tokens.iter().all(|t| t == "new-0"), "{tokens:?}");
    }

    #[test]
    fn a_refused_refresh_marks_broken_and_an_unreachable_one_doesnt() {
        let rig = Rig::new("refused", token("old", Some("ghr_dead"), Some(UNIX), "https://x/t"));
        let down = |_: &OAuthToken| Refreshed::Unavailable("cannot reach https://x/t".into());
        assert!(matches!(rig.renewer.token("gh", UNIX, &down), Err(Failure::Unavailable(_))));
        assert_eq!(rig.broken(), None);

        let refused = |_: &OAuthToken| Refreshed::Refused("invalid_grant".into());
        assert_eq!(rig.renewer.token("gh", UNIX, &refused).unwrap_err(), Failure::Refused("invalid_grant".into()));
        let b = rig.broken().unwrap();
        assert_eq!((b.at.as_str(), b.reason.as_str()), ("2023-11-14T22:13:20Z", "its refresh was refused: invalid_grant"));
        assert_eq!(rig.stored().access_token.expose(), "old", "the store is left alone");
        // The first reason stays.
        rig.renewer.mark_broken("gh", "later".into(), UNIX + 1);
        assert_eq!(rig.broken(), Some(b));

        let calls = AtomicUsize::new(0);
        assert!(rig.renewer.token("gh", UNIX, &counting(&calls)).unwrap().refreshed);
        assert_eq!(rig.broken(), None, "a refresh that works clears it");
    }

    #[test]
    fn an_unreadable_store_marks_broken() {
        use std::os::unix::fs::PermissionsExt;
        let rig = Rig::new("store", token("old", Some("r"), None, "https://x/t"));
        let store = Store::in_data(&rig.dir);
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let calls = AtomicUsize::new(0);
        let Err(Failure::Store(why)) = rig.renewer.token("gh", UNIX, &counting(&calls)) else { panic!() };
        assert!(why.contains("must be 0600"), "{why}");
        assert!(rig.broken().unwrap().reason.starts_with("the credential store can't be used: "));
    }

    #[test]
    fn a_401_renews_once_then_not_within_recent() {
        let rig = Rig::new("401", token("old", Some("r"), Some(UNIX + 28800), "https://x/t"));
        let calls = AtomicUsize::new(0);
        let call = counting(&calls);
        let old = Secret::new("old".into());
        let f = rig.renewer.after_401("gh", &old, UNIX, &call).unwrap().unwrap();
        assert_eq!((f.token.expose(), f.refreshed), ("new-0", true));
        // Another thread's 401 on the token just replaced: replay with the new one.
        let f = rig.renewer.after_401("gh", &old, UNIX + 1, &call).unwrap().unwrap();
        assert_eq!((f.token.expose(), f.refreshed), ("new-0", false));
        // A 401 on the new token so soon: nothing newer to try.
        let new = Secret::new("new-0".into());
        assert!(rig.renewer.after_401("gh", &new, UNIX + RECENT - 1, &call).unwrap().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(rig.renewer.after_401("gh", &new, UNIX + RECENT, &call).unwrap().unwrap().refreshed);

        let mut no_refresh = token("a", None, None, "https://x/t");
        no_refresh.refresh_expires_at = None;
        Store::in_data(&rig.dir).put("gh", &Record::OAuth(no_refresh)).unwrap();
        assert!(rig.renewer.after_401("gh", &Secret::new("a".into()), UNIX + 999, &call).unwrap().is_none());
    }
}
