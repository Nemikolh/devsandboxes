//! `devsandbox access approve <credential>` (docs/reference/credentials-cli.md):
//! approve a claim version pending for one of this home's credentials,
//! after its diff against the latest approved one (github-and-sentry-access,
//! *The claim contract*). The secret is left alone. Local only.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::login::{Stdio, Term, approve_or_refuse, data_dir};
use crate::credentials::home::{self, Version};
use crate::credentials::review;

pub fn approve(credential: &str, version: Option<&str>, yes: bool) -> Result<()> {
    let done = approve_at(&data_dir()?, credential, version, yes, &mut Stdio)?;
    println!("{done}");
    Ok(())
}

/// Returns the line to print.
pub(crate) fn approve_at(data: &Path, name: &str, version: Option<&str>, yes: bool, term: &mut dyn Term) -> Result<String> {
    let path = home::path(data);
    let entry = home::load(&path)?
        .remove(name)
        .with_context(|| format!("no credential `{name}` in {}", path.display()))?;
    let pending = pick_pending(name, &entry.pending, version)?;
    let new = pending.view()?;
    let text = match entry.latest_approved() {
        Some(latest) => {
            let old = latest.view()?;
            let diff = review::diff(&old, &new).render(&old, &new);
            format!("claim `{}` version {}, against approved {}:\n{diff}", new.kind, pending.short(), latest.short())
        }
        None => format!("{}  version {}\n", new.summary(), pending.short()),
    };
    term.show(&text);
    approve_or_refuse(term, yes, &format!("Approve this claim version for `{name}`?"))?;
    home::update(&path, |h| {
        let entry = h.get_mut(name).with_context(|| format!("credential `{name}` is gone"))?;
        if !entry.pending.iter().any(|p| p.version == pending.version) {
            bail!("claim version {} of `{name}` is no longer pending", pending.short());
        }
        Ok(entry.approve(pending.clone()))
    })?;
    Ok(format!("approved claim version {} for `{name}`", pending.short()))
}

/// `version` (a prefix of one), else the only one.
fn pick_pending<'a>(name: &str, pending: &'a [Version], version: Option<&str>) -> Result<&'a Version> {
    let list = || pending.iter().map(Version::short).collect::<Vec<_>>().join(", ");
    match version {
        Some(v) => {
            let found: Vec<&Version> = pending.iter().filter(|p| !v.is_empty() && p.version.starts_with(v)).collect();
            match found.as_slice() {
                [one] => Ok(one),
                [] => bail!("no pending claim version `{v}` for `{name}` (pending: {})", or_none(&list())),
                _ => bail!("`{v}` names several pending claim versions of `{name}`: give more of it"),
            }
        }
        None => match pending {
            [] => bail!("nothing pending for `{name}`"),
            [one] => Ok(one),
            _ => bail!("`{name}` has {} pending claim versions ({}): pass --version", pending.len(), list()),
        },
    }
}

fn or_none(list: &str) -> &str {
    if list.is_empty() { "none" } else { list }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::login::tests::{Script, temp};
    use crate::credentials::home::Entry;
    use crate::credentials::home::tests::sentry;
    use crate::credentials::store::{Secret, Store};

    #[test]
    fn approves_the_pending_version_after_its_diff() {
        let data = temp("approve");
        let path = home::path(&data);
        assert!(approve_at(&data, "s", None, true, &mut Script::new(true, &[], None)).is_err());
        Store::in_data(&data).put("s", &Secret::new("keep".into())).unwrap();
        let mut newer = sentry();
        newer.pinned.never.pop();
        home::update(&path, |h| {
            let mut e = Entry::new("sentry", None);
            e.approve(Version::of(&sentry()));
            h.insert("s".into(), e);
            Ok(())
        })
        .unwrap();
        let err = approve_at(&data, "s", None, true, &mut Script::new(true, &[], None)).unwrap_err();
        assert!(err.to_string().contains("nothing pending"), "{err}");
        home::update(&path, |h| home::record_pending(h, "s", &newer.canonical())).unwrap();

        let err = approve_at(&data, "s", None, false, &mut Script::new(false, &[], None)).unwrap_err();
        assert!(err.to_string().contains("pass --yes"), "{err}");
        let mut term = Script::new(true, &[false], None);
        assert!(approve_at(&data, "s", None, false, &mut term).is_err());
        assert!(term.shown.contains("  never:\n    - "), "{}", term.shown);
        assert_eq!(home::load(&path).unwrap()["s"].pending.len(), 1);

        let done = approve_at(&data, "s", Some(&newer.version()[..8]), false, &mut Script::new(true, &[true], None)).unwrap();
        assert!(done.starts_with("approved claim version"), "{done}");
        let e = &home::load(&path).unwrap()["s"];
        assert!(e.pending.is_empty());
        assert_eq!(e.latest_approved().unwrap().version, newer.version());
        assert_eq!(e.approved.len(), 2, "the older version stays for instances pinned to it");
        assert_eq!(Store::in_data(&data).get("s").unwrap().unwrap().expose(), "keep");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn picks_a_pending_version() {
        let v = |c: &str| Version { version: c.into(), claim: String::new() };
        let two = [v("abc1"), v("abd2")];
        assert_eq!(pick_pending("s", &two[..1], None).unwrap().version, "abc1");
        assert!(pick_pending("s", &two, None).unwrap_err().to_string().contains("pass --version"));
        assert_eq!(pick_pending("s", &two, Some("abd")).unwrap().version, "abd2");
        assert!(pick_pending("s", &two, Some("ab")).unwrap_err().to_string().contains("several"));
        assert!(pick_pending("s", &two, Some("")).is_err());
        assert!(pick_pending("s", &[], Some("x")).unwrap_err().to_string().contains("(pending: none)"));
    }
}
