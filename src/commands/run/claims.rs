//! `run`'s side of credential claims
//! (docs/architecture/instances/images-and-features.md, *Credential claims*):
//! which of the features' claims apply here, the home's credential and
//! approval each resolves to, and the phantoms, listeners, env and phantom
//! files the instance gets (`credentials::instance`, pure).

use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{parse_shorthand, SandboxProperties};
use crate::credentials::claim::Claim;
use crate::credentials::instance::{self, InstanceClaim};
use crate::runtime::backend;
use crate::state::Instance;

/// What `materialize` puts on the instance and its container.
pub(super) struct Wired {
    pub(super) incarnation: String,
    pub(super) claims: Vec<InstanceClaim>,
    /// `-e` for the container.
    pub(super) env: Vec<(String, String)>,
}

/// Wire `found` (the enabled features' claims) for an instance in
/// `profile`, keeping what `prior` (the instance being rebuilt) had. `home`
/// is the remote user's, `owner` who the phantom files belong to, `targets`
/// every mount and volume target. Refuses a second claim of one kind and a
/// claim version its credential hasn't approved (recorded pending first, for
/// `devsandbox access approve`). A claim with no credential here is wired
/// anyway, so a later `login` works without a rebuild.
#[allow(clippy::too_many_arguments)]
pub(super) fn wire(
    dir: &Path,
    props: &SandboxProperties,
    profile: Option<&str>,
    prior: Option<&Instance>,
    found: instance::Found,
    home: &str,
    owner: &str,
    targets: &[String],
) -> Result<Wired> {
    let incarnation = match prior.and_then(|i| i.incarnation.clone()) {
        Some(kept) => kept,
        None => instance::mint_incarnation()?,
    };
    let mut wired = Wired { incarnation, claims: Vec::new(), env: Vec::new() };
    if found.is_empty() {
        return Ok(wired);
    }
    let (kept, warnings) = instance::select(found)?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    let Some(kept) = brokered_here(dir, kept)? else {
        return Ok(wired);
    };
    check_credentials(props, profile, &kept)?;
    let prior_claims = prior.map(|i| i.claims.as_slice()).unwrap_or_default();
    let mut claims = instance::plan(&kept, prior_claims, instance::mint_phantom)?;
    for ((_, claim), record) in kept.iter().zip(claims.iter_mut()) {
        wired.env.extend(instance::env(claim, record));
        let (files, warnings) = instance::render_files(claim, record, home, owner, targets);
        for w in warnings {
            eprintln!("warning: {w}");
        }
        record.files = files;
    }
    wired.claims = claims;
    Ok(wired)
}

/// `kept`, or `None` (each skipped with a warning) where the broker can't
/// serve it yet: off unix (no daemon), and on an instance placed here by
/// another member, whose claims need `CRED` (credential-broker step 5).
fn brokered_here(dir: &Path, kept: instance::Found) -> Result<Option<instance::Found>> {
    let why = if cfg!(unix) {
        let data = crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))?;
        crate::worker_policy::fleet_root_of(&data, dir)?
            .map(|_| "isn't brokered on a placed instance yet (credential-broker step 5)")
    } else {
        Some("needs the host daemon, which is unix-only")
    };
    let Some(why) = why else { return Ok(Some(kept)) };
    for (feature, claim) in &kept {
        eprintln!("warning: feature `{feature}`: the `{}` claim {why}: skipped", claim.kind);
    }
    Ok(None)
}

/// Each claim's credential here (`home::resolve`): none is a warning naming
/// `devsandbox login`; one that hasn't approved the claim's version records
/// it pending and refuses the run.
#[cfg(unix)]
fn check_credentials(props: &SandboxProperties, profile: Option<&str>, kept: &[(String, Claim)]) -> Result<()> {
    let data = crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))?;
    check_credentials_at(&crate::credentials::home::path(&data), props, profile, kept)
}

/// [`check_credentials`] against the `credentials.toml` at `path`.
#[cfg(unix)]
fn check_credentials_at(
    path: &Path,
    props: &SandboxProperties,
    profile: Option<&str>,
    kept: &[(String, Claim)],
) -> Result<()> {
    use crate::credentials::home;
    let creds = home::load(path)?;
    for (feature, claim) in kept {
        let kind = claim.kind.as_str();
        let named = props.access.as_ref().and_then(|a| a.get(kind)).and_then(|a| a.credential.as_deref());
        let Some((name, entry)) = home::resolve(&creds, kind, named, profile)? else {
            eprintln!(
                "warning: feature `{feature}`: no `{kind}` credential here: `devsandbox login {}` stores one; \
                 the sandbox is wired for it, no rebuild needed",
                named.unwrap_or(kind)
            );
            continue;
        };
        let version = claim.version();
        if entry.approved_claim(&version).is_none() {
            let canonical = claim.canonical();
            home::update(path, |h| home::record_pending(h, name, &canonical))?;
            bail!(
                "credential `{name}` hasn't approved this version of feature `{feature}`'s `{kind}` claim ({}): \
                 run `devsandbox access approve {name}`, then try again",
                &version[..12]
            );
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_credentials(_: &SandboxProperties, _: Option<&str>, _: &[(String, Claim)]) -> Result<()> {
    Ok(())
}

/// Every container path a phantom file may not land on: the workspace, the
/// `src:dst` volumes and the `--mount` targets.
pub(super) fn mount_targets(workspace: &str, volumes: &[String], mounts: &[String]) -> Vec<String> {
    let volume_targets = volumes.iter().filter_map(|v| v.split(':').nth(1).map(str::to_string));
    let mount_targets = mounts.iter().filter_map(|m| parse_shorthand(m).ok().map(|(_, _, target, _)| target));
    std::iter::once(workspace.to_string()).chain(volume_targets).chain(mount_targets).collect()
}

/// Warn of each phantom file devsbd will leave alone because something else
/// is at its path (from the image, say). Checked as the file's owner; the
/// daemon's own writes are atomic, so a check racing one never misfires.
pub(super) fn warn_foreign_files(container: &str, claims: &[InstanceClaim]) {
    const SCRIPT: &str = "if [ -L \"$1\" ] || { [ -e \"$1\" ] && ! { [ -f \"$1\" ] && grep -qF -- \"$2\" \"$1\"; }; }; \
                          then echo foreign; fi";
    for (claim, file) in claims.iter().flat_map(|c| c.files.iter().map(move |f| (c, f))) {
        let script = crate::runtime::fixed_path(SCRIPT);
        let args =
            ["exec", "-u", &file.owner, container, crate::runtime::SH, "-c", &script, "sh", &file.path, &claim.phantom];
        if backend().output_quiet(&args).is_ok_and(|out| out.trim() == "foreign") {
            eprintln!(
                "warning: `{}` is already in the image and not a phantom file: left alone, so the `{}` claim's \
                 tools there won't use the broker",
                file.path, claim.kind
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_targets_cover_workspace_volumes_and_mounts() {
        let got = mount_targets(
            "/workspaces/repo",
            &["/host/.git:/host/.git".into()],
            &["type=volume,source=v,target=/home/dev/.cache".into(), "type=bind,source=/a,target=/b".into()],
        );
        assert_eq!(got, ["/workspaces/repo", "/host/.git", "/home/dev/.cache", "/b"]);
    }

    #[cfg(unix)]
    #[test]
    fn an_unapproved_version_is_recorded_pending_and_refused() {
        use crate::credentials::home::{self, Entry, Home};
        let dir = std::env::temp_dir().join(format!("dsv-run-claims-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = home::path(&dir);
        let claim = crate::credentials::home::tests::sentry();
        let kept = vec![("./s".to_string(), claim.clone())];
        let props = |toml: &str| {
            let config = crate::config::Config::parse(&format!("[sandbox.s]\nimage = \"x\"\n{toml}")).unwrap();
            config.resolve_sandbox("s").unwrap().properties
        };
        // No credential: wired anyway.
        check_credentials_at(&path, &props(""), None, &kept).unwrap();

        home::update(&path, |h: &mut Home| Ok(h.insert("sentry-work".into(), Entry::new("sentry", None)))).unwrap();
        let err = check_credentials_at(&path, &props(""), None, &kept).unwrap_err().to_string();
        assert!(err.contains("run `devsandbox access approve sentry-work`"), "{err}");
        assert_eq!(home::load(&path).unwrap()["sentry-work"].pending[0].version, claim.version());

        home::update(&path, |h: &mut Home| {
            let e = h.get_mut("sentry-work").unwrap();
            let v = e.pending[0].clone();
            Ok(e.approve(v))
        })
        .unwrap();
        check_credentials_at(&path, &props(""), None, &kept).unwrap();
        home::update(&path, |h: &mut Home| Ok(h.insert("gh".into(), Entry::new("github", None)))).unwrap();
        let err = check_credentials_at(&path, &props("[sandbox.s.access.sentry]\ncredential = \"gh\"\n"), None, &kept)
            .unwrap_err()
            .to_string();
        assert!(err.contains("a `github` credential"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
