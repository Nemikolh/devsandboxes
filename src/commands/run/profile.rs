//! Which profile a new instance is in (docs/architecture/instances/profiles.md,
//! *Per instance*).

use anyhow::{bail, Result};

/// The profile `run` gives a new instance of `sandbox`, from its `allowed`
/// set ([`crate::config::Config::allowed_profiles`], preference order): the
/// `requested` one (`--profile`), which must be allowed; else `last`
/// (`State::last_profile`) while it's still allowed; else the first allowed.
/// `None` only when nothing is allowed and nothing was requested. Never a
/// prompt.
pub(crate) fn choose_profile(
    sandbox: &str,
    allowed: &[String],
    requested: Option<&str>,
    last: Option<&str>,
) -> Result<Option<String>> {
    if let Some(p) = requested {
        if allowed.is_empty() {
            bail!("sandbox `{sandbox}` has no profiles; drop --profile `{p}`");
        }
        if !allowed.iter().any(|a| a == p) {
            bail!("sandbox `{sandbox}` doesn't allow profile `{p}`; allowed: {}", allowed.join(", "));
        }
        return Ok(Some(p.to_string()));
    }
    Ok(last
        .filter(|l| allowed.iter().any(|a| a == l))
        .map(str::to_string)
        .or_else(|| allowed.first().cloned()))
}

/// What `run` says about the profile it picked: only when there was a
/// choice to make, so a one-profile (or profile-less) sandbox stays quiet.
pub(crate) fn profile_note(allowed: &[String], chosen: Option<&str>, requested: bool) -> Option<String> {
    let p = chosen.filter(|_| allowed.len() > 1)?;
    Some(if requested { format!("profile: {p}") } else { format!("profile: {p} (--profile to choose)") })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn requested_must_be_allowed() {
        let allowed = names(&["work", "personal"]);
        assert_eq!(choose_profile("web", &allowed, Some("personal"), Some("work")).unwrap().as_deref(), Some("personal"));
        let err = choose_profile("web", &allowed, Some("other"), None).unwrap_err().to_string();
        assert_eq!(err, "sandbox `web` doesn't allow profile `other`; allowed: work, personal");
        let err = choose_profile("web", &[], Some("work"), None).unwrap_err().to_string();
        assert_eq!(err, "sandbox `web` has no profiles; drop --profile `work`");
    }

    #[test]
    fn last_while_still_allowed_else_first() {
        let allowed = names(&["work", "personal"]);
        assert_eq!(choose_profile("web", &allowed, None, Some("personal")).unwrap().as_deref(), Some("personal"));
        assert_eq!(choose_profile("web", &allowed, None, Some("gone")).unwrap().as_deref(), Some("work"));
        assert_eq!(choose_profile("web", &allowed, None, None).unwrap().as_deref(), Some("work"));
    }

    #[test]
    fn nothing_allowed_is_no_profile() {
        assert_eq!(choose_profile("web", &[], None, Some("work")).unwrap(), None);
        assert_eq!(choose_profile("web", &[], None, None).unwrap(), None);
    }

    #[test]
    fn note_only_when_there_was_a_choice() {
        let two = names(&["work", "personal"]);
        assert_eq!(profile_note(&two, Some("work"), false).as_deref(), Some("profile: work (--profile to choose)"));
        assert_eq!(profile_note(&two, Some("work"), true).as_deref(), Some("profile: work"));
        assert_eq!(profile_note(&names(&["work"]), Some("work"), false), None);
        assert_eq!(profile_note(&[], None, false), None);
    }
}
