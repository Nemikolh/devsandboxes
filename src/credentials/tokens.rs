//! A credential's token: the kind an instance takes it as, which secrets
//! `login` takes, and when it stops working (docs/reference/credentials-cli.md,
//! *`login`*, *`access ls`*, *`credentials.toml`*; github-and-sentry-access,
//! *Whose token*). Pure: `run`, `login`, `access ls` and the broker do the I/O.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::claim::{TokenKind, Tokens};
use super::store::Record;

/// `access ls` warns of a credential ending within this many days.
pub const WARN_DAYS: u64 = 14;
/// The broker's desktop notice starts this many days ahead.
pub const NOTICE_DAYS: u64 = 7;
const DAY: u64 = 86_400;

/// The home's `as` list read: each a kind, none twice.
pub fn offered(list: &[String]) -> Result<Vec<TokenKind>> {
    let mut out = Vec::new();
    for s in list {
        let kind = TokenKind::parse(s)?;
        if out.contains(&kind) {
            bail!("`{s}` is listed twice");
        }
        out.push(kind);
    }
    Ok(out)
}

/// The kind an instance takes credential `credential` as, for a claim of
/// kind `kind`: `None` when the claim declares no token kinds (a config's
/// `access.<kind>.as` is then refused: it would change nothing). The home
/// offers its `as` list, `["user"]` when unset; the config's `as` picks
/// within it, else `user` when offered, else `app`. A config only narrows:
/// a kind the home doesn't offer is refused, as is one the claim lacks.
pub fn pick(
    claim: Option<&Tokens>,
    home_as: &[TokenKind],
    wanted: Option<TokenKind>,
    kind: &str,
    credential: &str,
) -> Result<Option<TokenKind>> {
    let declared = claim.map(|t| t.as_.as_slice()).unwrap_or_default();
    if declared.is_empty() {
        if let Some(w) = wanted {
            bail!("`access.{kind}.as = \"{}\"`: the `{kind}` claim declares no token kinds", w.as_str());
        }
        return Ok(None);
    }
    let offered: &[TokenKind] = if home_as.is_empty() { &[TokenKind::User] } else { home_as };
    let picked = wanted.unwrap_or(if offered.contains(&TokenKind::User) { TokenKind::User } else { TokenKind::App });
    let names = |l: &[TokenKind]| l.iter().map(|k| format!("\"{}\"", k.as_str())).collect::<Vec<_>>().join(", ");
    if !declared.contains(&picked) {
        bail!("the `{kind}` claim can't act as `{}`: it declares `as = [{}]`", picked.as_str(), names(declared));
    }
    if !offered.contains(&picked) {
        bail!(
            "credential `{credential}` offers `as = [{}]`, not `{}`: a config only narrows (`as` in credentials.toml)",
            names(offered),
            picked.as_str()
        );
    }
    if picked == TokenKind::App {
        bail!("`as = \"app\"` for credential `{credential}`: installation tokens: {}", crate::NOT_SUPPORTED_YET);
    }
    Ok(Some(picked))
}

/// Refuses a secret none of the claim's `accept` prefixes start, naming the
/// secret's own prefix (up to its first `_`) when it has a short one.
pub fn check_accepted(claim: Option<&Tokens>, secret: &str, kind: &str) -> Result<()> {
    let accept = claim.map(|t| t.accept.as_slice()).unwrap_or_default();
    if accept.is_empty() || accept.iter().any(|p| secret.starts_with(p.as_str())) {
        return Ok(());
    }
    let listed = accept.iter().map(|p| format!("`{p}…`")).collect::<Vec<_>>().join(", ");
    let theirs = secret
        .find('_')
        .filter(|&at| at > 0 && at < 12 && secret[..at].bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(|at| format!(", not `{}…`", &secret[..=at]))
        .unwrap_or_default();
    bail!(
        "the `{kind}` claim takes only {listed} tokens{theirs}: another kind reaches more than the claim can \
         fence (a classic token reaches every repo you can): nothing changed"
    )
}

/// An expiry header's value, unix seconds: `2026-12-01 10:00:00 UTC` or
/// with a numeric offset (`+0500`), as GitHub sends either.
pub fn parse_expiry(value: &str) -> Option<u64> {
    let (date, rest) = value.trim().split_once(' ')?;
    let (time, zone) = rest.split_once(' ')?;
    let offset = match zone {
        "UTC" | "Z" => "Z".to_string(),
        z if z.len() == 5 && matches!(&z[..1], "+" | "-") && z[1..].bytes().all(|b| b.is_ascii_digit()) => {
            format!("{}:{}", &z[..3], &z[3..])
        }
        _ => return None,
    };
    if date.len() != 10 || time.len() != 8 {
        return None;
    }
    crate::broker::lfs::parse_rfc3339(&format!("{date}T{time}{offset}"))
}

/// The expiry to record for a pasted secret from response `headers` (name,
/// value): the claim's `expiry` header's, when it parses and isn't `known`.
pub fn captured<'a>(
    claim: Option<&Tokens>,
    headers: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    known: Option<u64>,
) -> Option<u64> {
    let name = claim?.expiry.as_deref()?;
    let (_, value) = headers.into_iter().find(|(n, _)| n.eq_ignore_ascii_case(name))?;
    let at = parse_expiry(std::str::from_utf8(value).ok()?)?;
    (Some(at) != known).then_some(at)
}

/// When the secret stops working, as far as the home knows: an OAuth
/// record's refresh token's expiry (its access token renews until then),
/// else a pasted one's recorded `expires_at`. A short-lived OAuth token with
/// nothing to renew it is left out: `access ls` shows its own expiry.
pub fn ends_at(record: Option<&Record>, recorded: Option<u64>) -> Option<u64> {
    match record? {
        Record::OAuth(t) if t.refresh_token.is_some() => t.refresh_expires_at,
        Record::OAuth(_) => None,
        Record::Static { .. } => recorded,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Left {
    /// Whole days left; 0 under a day or past.
    pub days: u64,
    pub expired: bool,
    /// Within [`WARN_DAYS`] and not past.
    pub warn: bool,
}

pub fn left(end: u64, now: u64) -> Left {
    let rest = end.saturating_sub(now);
    Left { days: rest / DAY, expired: end <= now, warn: end > now && rest <= WARN_DAYS * DAY }
}

/// "in 9 days", "in less than a day", "expired".
pub fn in_days(l: Left) -> String {
    match (l.expired, l.days) {
        (true, _) => "expired".into(),
        (false, 0) => "in less than a day".into(),
        (false, 1) => "in 1 day".into(),
        (false, n) => format!("in {n} days"),
    }
}

/// The broker's desktop notices of a credential ending soon: within
/// [`NOTICE_DAYS`], before it ends, at most once per credential per UTC day.
/// In memory: a restarted daemon may notify again the same day.
#[derive(Debug, Default)]
pub struct Notices {
    last: HashMap<String, u64>,
}

impl Notices {
    /// The notice's text, when one is due now (and then counted as sent).
    pub fn due(&mut self, credential: &str, end: u64, now: u64) -> Option<String> {
        if end <= now || end - now > NOTICE_DAYS * DAY {
            return None;
        }
        let today = now / DAY;
        if self.last.get(credential) == Some(&today) {
            return None;
        }
        self.last.insert(credential.to_string(), today);
        Some(format!(
            "credential `{credential}` expires {} ({}): `devsandbox login {credential}` replaces it",
            in_days(left(end, now)),
            crate::inbox::rfc3339(end)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::store::{OAuthToken, Secret};

    fn tokens(as_: &[TokenKind]) -> Tokens {
        Tokens {
            as_: as_.to_vec(),
            accept: vec!["ghu_".into(), "github_pat_".into()],
            expiry: Some("github-authentication-token-expiration".into()),
        }
    }

    #[test]
    fn user_by_default_app_refused_and_a_config_only_narrows() {
        use TokenKind::{App, User};
        let both = tokens(&[App, User]);
        let pick = |home: &[TokenKind], wanted| pick(Some(&both), home, wanted, "github", "gh");
        assert_eq!(pick(&[], None).unwrap(), Some(User), "the home offers user by default");
        assert_eq!(pick(&[User], Some(User)).unwrap(), Some(User));
        assert_eq!(pick(&[App, User], None).unwrap(), Some(User), "user when offered");
        for (home, wanted, wants) in [
            (&[App, User][..], Some(App), "installation tokens: not supported yet! open an issue in https://github.com/Nemikolh/devsandboxes"),
            (&[App][..], None, "installation tokens"),
            (&[][..], Some(App), "credential `gh` offers `as = [\"user\"]`, not `app`: a config only narrows"),
            (&[App][..], Some(User), "offers `as = [\"app\"]`, not `user`"),
        ] {
            let err = pick(home, wanted).unwrap_err().to_string();
            assert!(err.contains(wants), "{home:?} {wanted:?}: {err}");
        }
        let err = super::pick(Some(&tokens(&[App])), &[], None, "github", "gh").unwrap_err().to_string();
        assert!(err.contains("can't act as `user`: it declares `as = [\"app\"]`"), "{err}");

        assert_eq!(super::pick(None, &[App], None, "sentry", "s").unwrap(), None);
        assert_eq!(super::pick(Some(&tokens(&[])), &[], None, "sentry", "s").unwrap(), None);
        let err = super::pick(None, &[], Some(User), "sentry", "s").unwrap_err().to_string();
        assert!(err.contains("`access.sentry.as = \"user\"`: the `sentry` claim declares no token kinds"), "{err}");

        assert_eq!(offered(&["user".into(), "app".into()]).unwrap(), [User, App]);
        assert!(offered(&["user".into(), "user".into()]).unwrap_err().to_string().contains("twice"));
        assert!(offered(&["bot".into()]).unwrap_err().to_string().contains("`user` or `app`"));
    }

    #[test]
    fn only_the_accepted_prefixes_are_taken() {
        let t = tokens(&[]);
        check_accepted(Some(&t), "github_pat_11ABC", "github").unwrap();
        check_accepted(Some(&t), "ghu_abc", "github").unwrap();
        check_accepted(None, "anything", "sentry").unwrap();
        check_accepted(Some(&Tokens { accept: vec![], ..t.clone() }), "ghp_x", "github").unwrap();
        for (secret, theirs) in [("ghp_abc", ", not `ghp_…`"), ("gho_abc", ", not `gho_…`"), ("plainhex", ""), ("_x", "")] {
            let err = check_accepted(Some(&t), secret, "github").unwrap_err().to_string();
            assert!(err.contains(&format!("takes only `ghu_…`, `github_pat_…` tokens{theirs}:")), "{secret}: {err}");
            assert!(err.contains("nothing changed"), "{err}");
        }
        let err = check_accepted(Some(&t), "averyveryverylongprefix_x", "github").unwrap_err().to_string();
        assert!(!err.contains("averyvery"), "a long head may be the secret itself: {err}");
    }

    #[test]
    fn the_expiry_header_is_read_in_either_form() {
        assert_eq!(parse_expiry("2026-12-01 10:00:00 UTC"), crate::broker::lfs::parse_rfc3339("2026-12-01T10:00:00Z"));
        assert_eq!(parse_expiry("2026-12-01 15:00:00 +0500"), parse_expiry("2026-12-01 10:00:00 UTC"));
        assert_eq!(parse_expiry("2026-12-01 03:00:00 -0700"), parse_expiry("2026-12-01 10:00:00 UTC"));
        for bad in ["", "2026-12-01", "2026-12-01T10:00:00Z", "2026-12-01 10:00:00 PST", "2026-12-01 10:00 UTC", "2026-13-01 10:00:00 UTC"] {
            assert_eq!(parse_expiry(bad), None, "{bad}");
        }

        let t = tokens(&[]);
        let at = parse_expiry("2026-12-01 10:00:00 UTC");
        let headers = [("Content-Type", &b"application/json"[..]), ("GitHub-Authentication-Token-Expiration", b"2026-12-01 10:00:00 UTC")];
        assert_eq!(captured(Some(&t), headers, None), at);
        assert_eq!(captured(Some(&t), headers, at), None, "already recorded");
        assert_eq!(captured(Some(&Tokens { expiry: None, ..t.clone() }), headers, None), None);
        assert_eq!(captured(None, headers, None), None);
        assert_eq!(captured(Some(&t), [("github-authentication-token-expiration", &b"soon"[..])], None), None);
    }

    #[test]
    fn the_end_is_the_refresh_tokens_else_the_recorded_one() {
        let oauth = |refresh: bool| {
            Record::OAuth(OAuthToken {
                access_token: Secret::new("ghu_a".into()),
                refresh_token: refresh.then(|| Secret::new("ghr_a".into())),
                expires_at: Some(100),
                refresh_expires_at: Some(5000),
                token_url: "https://github.com/login/oauth/access_token".into(),
                client_id: "Iv1.c".into(),
                scope: None,
            })
        };
        assert_eq!(ends_at(Some(&oauth(true)), Some(9)), Some(5000));
        assert_eq!(ends_at(Some(&oauth(false)), Some(9)), None);
        assert_eq!(ends_at(Some(&Record::token(Secret::new("github_pat_x".into()))), Some(9)), Some(9));
        assert_eq!(ends_at(None, Some(9)), None);
    }

    #[test]
    fn warnings_start_fourteen_days_ahead() {
        let now = 1_800_000_000;
        assert_eq!(left(now + 15 * DAY, now), Left { days: 15, expired: false, warn: false });
        assert_eq!(left(now + 14 * DAY, now), Left { days: 14, expired: false, warn: true });
        assert_eq!(left(now + 3600, now), Left { days: 0, expired: false, warn: true });
        assert_eq!(left(now, now), Left { days: 0, expired: true, warn: false });
        assert_eq!(in_days(left(now + 14 * DAY + 5, now)), "in 14 days");
        assert_eq!(in_days(left(now + DAY, now)), "in 1 day");
        assert_eq!(in_days(left(now + 60, now)), "in less than a day");
        assert_eq!(in_days(left(now - 60, now)), "expired");
    }

    #[test]
    fn a_notice_goes_once_a_day_within_seven_days() {
        let day0 = 1_800_000_000 / DAY * DAY;
        let end = day0 + 6 * DAY;
        let mut n = Notices::default();
        assert_eq!(n.due("gh", day0 + 30 * DAY, day0), None, "too far ahead");
        let first = n.due("gh", end, day0 + 10).unwrap();
        assert!(first.contains("credential `gh` expires in 5 days") && first.contains("`devsandbox login gh`"), "{first}");
        assert_eq!(n.due("gh", end, day0 + 3600), None, "once that day");
        assert!(n.due("other", end, day0 + 3600).is_some(), "per credential");
        assert!(n.due("gh", end, day0 + DAY).is_some(), "again the next day");
        assert_eq!(n.due("gh", end, end), None, "not once it ended");
        let mut n = Notices::default();
        assert!(n.due("gh", day0 + 7 * DAY, day0).is_some(), "seven days ahead");
        assert!(n.due("x", day0 + 7 * DAY + 1, day0).is_none());
    }
}
