//! `run`'s side of credential claims
//! (docs/architecture/instances/images-and-features.md, *Credential claims*):
//! which of the features' claims apply here, the home's credential and
//! approval each resolves to, and the phantoms, listeners, env and phantom
//! files the instance gets (`credentials::instance`, pure); for intercept
//! claims the shared proxy port, its env, and the member CA's trust in the
//! container (`credentials::trust`); for git-host claims git's own port,
//! its settings and the boot check (`credentials::git`).

use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{parse_shorthand, SandboxProperties};
use crate::credentials::claim::Claim;
use crate::credentials::instance::{self, Intercept, InstanceClaim};
use crate::runtime::backend;
use crate::state::Instance;

/// What `run` does with a claim version its credential hasn't approved,
/// once it's recorded pending.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Unapproved {
    /// Refuse, naming `devsandbox access approve`: off a TTY, `--json`, the
    /// dashboard, `rebuild`.
    #[default]
    Refuse,
    /// Show its review and ask on this process's terminal (`run` in-process
    /// on a TTY): a yes approves it and the run goes on.
    Ask,
    /// Fail with [`NeedsApproval`] (`run --fail-on-ask`, the daemon's `run`
    /// with `ask`): the verb client asks on its own terminal and resends.
    FailOnAsk,
}

/// [`Unapproved::FailOnAsk`]'s failure; its message is [`unapproved`]'s,
/// which [`parse_unapproved`] reads back.
#[derive(Debug)]
pub struct NeedsApproval(pub String);

impl std::fmt::Display for NeedsApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NeedsApproval {}

/// What hasn't been approved, short `version` included.
#[cfg_attr(not(unix), allow(dead_code))]
fn unapproved(credential: &str, feature: &str, kind: &str, version: &str) -> String {
    let short = &version[..version.len().min(12)];
    format!("credential `{credential}` hasn't approved this version of feature `{feature}`'s `{kind}` claim ({short})")
}

/// The credential and short version an [`unapproved`] message names.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn parse_unapproved(message: &str) -> Option<(String, String)> {
    let (credential, rest) = message.strip_prefix("credential `")?.split_once('`')?;
    let version = rest.rsplit_once('(')?.1.strip_suffix(')')?;
    Some((credential.to_string(), version.to_string()))
}

/// The refusal: [`unapproved`]'s `message` and what to run.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn refusal(message: &str, credential: &str) -> String {
    format!("{message}: run `devsandbox access approve {credential}`, then try again")
}

/// What `materialize` puts on the instance and its container.
pub(super) struct Wired {
    pub(super) incarnation: String,
    pub(super) claims: Vec<InstanceClaim>,
    pub(super) intercept: Option<Intercept>,
    /// What to install once the container exists ([`install_trust`]).
    pub(super) ca: Option<MemberCert>,
    /// `-e` for the container.
    pub(super) env: Vec<(String, String)>,
    /// git's settings for the git-host claims (`credentials::git::entries`),
    /// merged into the command-scope env behind a config's own by the caller.
    pub(super) git: Vec<(String, String)>,
}

pub(super) struct MemberCert {
    pub(super) pem: String,
    /// This run grew the CA's host set: running instances get the reissued
    /// cert too ([`reinstall_running`]).
    pub(super) grew: bool,
}

/// What the intercept side reads from the image and the run's env.
pub(super) struct Proxying<'a> {
    /// Built, local: probed for its system bundle.
    pub(super) image: &'a str,
    /// `instance::original_proxy_env` of the image's `ENV`, `containerEnv`
    /// and `--env`.
    pub(super) proxy_env: Vec<(String, String)>,
    /// The services' aliases, for `NO_PROXY`.
    pub(super) aliases: Vec<String>,
    /// `--env`'s keys: set on every exec, like `remoteEnv`'s.
    pub(super) run_env_keys: Vec<String>,
    /// The fenced owners as the folders' recorded URLs spell them
    /// (`credentials::git::owners`).
    pub(super) owners: Vec<String>,
}

/// Wire `found` (the enabled features' claims) for an instance in
/// `profile`, keeping what `prior` (the instance being rebuilt) had. `home`
/// is the remote user's, `owner` who the phantom files belong to, `targets`
/// every mount and volume target. Refuses a second claim of one kind and a
/// claim version its credential hasn't approved (recorded pending first, for
/// `devsandbox access approve`), unless `unapproved` asks and it's approved.
/// A claim with no credential here is wired
/// anyway, so a later `login` works without a rebuild. With intercept
/// claims, the member CA covers their hosts before the container exists.
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
    proxying: Proxying,
    unapproved: Unapproved,
) -> Result<Wired> {
    let incarnation = match prior.and_then(|i| i.incarnation.clone()) {
        Some(kept) => kept,
        None => instance::mint_incarnation()?,
    };
    let mut wired =
        Wired { incarnation, claims: Vec::new(), intercept: None, ca: None, env: Vec::new(), git: Vec::new() };
    if found.is_empty() {
        return Ok(wired);
    }
    let kept = instance::select(found)?;
    let Some(kept) = brokered_here(dir, kept)? else {
        return Ok(wired);
    };
    check_credentials(props, profile, &kept, unapproved)?;
    let prior_claims = prior.map(|i| i.claims.as_slice()).unwrap_or_default();
    let prior_port = prior.and_then(|i| i.intercept.as_ref()).map(|i| i.port);
    let prior_git_port = prior.and_then(|i| i.intercept.as_ref()).and_then(|i| i.git.as_ref()).map(|g| g.port);
    let prior_lfs_port = prior.and_then(|i| i.intercept.as_ref()).and_then(|i| i.lfs);
    let planned =
        instance::plan(&kept, prior_claims, prior_port, prior_git_port, prior_lfs_port, instance::mint_phantom)?;
    let mut claims = planned.claims;
    for ((_, claim), record) in kept.iter().zip(claims.iter_mut()) {
        wired.env.extend(instance::env(claim, record));
        let (files, warnings) = instance::render_files(claim, record, home, owner, targets);
        for w in warnings {
            eprintln!("warning: {w}");
        }
        record.files = files;
    }
    wired.claims = claims;
    if let Some(shared) = planned.proxy {
        let exec_keys = props.remote_env.iter().flat_map(|e| e.keys()).chain(&proxying.run_env_keys);
        let overrides = instance::proxy_overrides(exec_keys);
        if !overrides.is_empty() {
            eprintln!(
                "warning: `remoteEnv` or `--env` sets {}: every exec then goes around the credential broker's proxy port",
                overrides.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
            );
        }
        let intercept = Intercept {
            port: shared.port,
            hosts: shared.hosts,
            git: planned.git,
            lfs: planned.lfs,
            proxy_env: proxying.proxy_env,
            bundle: probe_bundle(proxying.image),
            installed: None,
        };
        wired.env.extend(instance::intercept_env(&intercept, &proxying.aliases));
        let knobs = kept.iter().filter_map(|(_, c)| c.wiring.ca.as_ref());
        wired.env.extend(knobs.map(|ca| (ca.env.clone(), intercept.bundle.clone())));
        if let Some(git) = &intercept.git {
            wired.git = crate::credentials::git::entries(&git.hosts, &proxying.owners, git.port, &intercept.bundle);
            // cargo's libgit2 fetcher skips URL-scoped proxies: it'd take the
            // shared port, where git hosts are dialed directly.
            wired.env.push(("CARGO_NET_GIT_FETCH_WITH_CLI".into(), "true".into()));
        }
        wired.ca = member_cert(&intercept.bound_hosts());
        wired.intercept = Some(intercept);
    }
    Ok(wired)
}

#[cfg(unix)]
fn data_dir() -> Result<std::path::PathBuf> {
    crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// The image's system bundle (`credentials::trust::PROBE_SCRIPT`, from a
/// throwaway container like `Backend::image_passwd`'s); none, or a probe
/// that can't run, reads as the default path, where the install writes one.
#[cfg(unix)]
fn probe_bundle(image: &str) -> String {
    use crate::credentials::trust;
    let script = crate::runtime::fixed_path(trust::PROBE_SCRIPT);
    let args =
        ["run", "--rm", "--pull=never", "--network=none", "--user", "0:0", "--entrypoint", crate::runtime::SH, image, "-c", &script];
    let found = backend().output_quiet(&args).ok().and_then(|out| trust::parse_probe(&out));
    found.unwrap_or(trust::DEFAULT_BUNDLE).to_string()
}

#[cfg(not(unix))]
fn probe_bundle(_: &str) -> String {
    unreachable!("claims aren't wired off unix (`brokered_here`)")
}

/// The member CA cert covering `hosts`, reissued when they're new to it.
/// A failure is a warning: the run goes on, its TLS to them failing.
#[cfg(unix)]
fn member_cert(hosts: &[String]) -> Option<MemberCert> {
    let covered = || -> Result<(Option<String>, bool)> {
        let mut ca = crate::credentials::ca::MemberCa::load_or_create(&data_dir()?)?;
        let grew = ca.ensure_hosts(&hosts.iter().cloned().collect())?;
        Ok((ca.cert_pem().map(str::to_string), grew))
    };
    match covered() {
        Ok((Some(pem), grew)) => Some(MemberCert { pem, grew }),
        Ok((None, _)) => None,
        Err(e) => {
            eprintln!(
                "warning: the member CA can't cover {}: {e:#}: TLS to them will fail in the sandbox",
                hosts.join(", ")
            );
            None
        }
    }
}

#[cfg(not(unix))]
fn member_cert(_: &[String]) -> Option<MemberCert> {
    None
}

/// Put `pem` into `container`'s trust ([`install_in`]), recording it on
/// `intercept`; a failure is a warning naming what breaks.
#[cfg(unix)]
pub(super) fn install_trust(container: &str, intercept: &mut Intercept, pem: &str) {
    match install_in(container, &intercept.bundle, pem) {
        Ok(()) => intercept.installed = Some(crate::credentials::trust::fingerprint(pem)),
        Err(e) => eprintln!("{}", install_warning(container, &intercept.bound_hosts(), &e)),
    }
}

#[cfg(unix)]
fn install_warning(container: &str, hosts: &[String], why: &str) -> String {
    format!(
        "warning: couldn't install the member CA in `{container}` ({why}): TLS to {} fails there until it is",
        hosts.join(", ")
    )
}

/// `credentials::trust::INSTALL_SCRIPT` as root, `bundle` the file the CA
/// variables name.
#[cfg(unix)]
fn install_in(container: &str, bundle: &str, pem: &str) -> Result<(), String> {
    use crate::credentials::trust;
    let script = crate::runtime::fixed_path(trust::INSTALL_SCRIPT);
    let args = ["exec", "-i", "-u", "root", container, crate::runtime::SH, "-c", &script, "sh", bundle];
    backend().run_with_stdin(&args, trust::install_input(pem).as_bytes()).map_err(|e| format!("{e:#}"))
}

/// The reissued `pem` into every other running instance with intercept
/// claims that doesn't have it yet; a stopped one gets it at its next
/// `start` ([`refresh_trust`]).
#[cfg(unix)]
pub(super) fn reinstall_running(except: &str, pem: &str) {
    let Ok(state) = crate::state::State::load() else { return };
    let fingerprint = crate::credentials::trust::fingerprint(pem);
    for (key, info) in &state.instances {
        let Some(intercept) = &info.intercept else { continue };
        if key == except || intercept.installed.as_deref() == Some(fingerprint.as_str()) {
            continue;
        }
        if backend().is_running(&info.container).ok().flatten() == Some(true) {
            reinstall(key, &info.container, intercept, pem, &fingerprint);
        }
    }
}

#[cfg(unix)]
fn reinstall(key: &str, container: &str, intercept: &Intercept, pem: &str, fingerprint: &str) {
    if let Err(e) = install_in(container, &intercept.bundle, pem) {
        eprintln!("{}", install_warning(container, &intercept.bound_hosts(), &e));
        return;
    }
    let saved = crate::state::State::update(|state| {
        if let Some(i) = state.instances.get_mut(key).and_then(|i| i.intercept.as_mut()) {
            i.installed = Some(fingerprint.to_string());
        }
        Ok(())
    });
    if let Err(e) = saved {
        eprintln!("note: couldn't record the member CA installed in `{container}`: {e:#}");
    }
}

/// `start`'s side: the member CA cert covering the instance's hosts, into
/// its container when it isn't the one installed there (reissued for
/// another instance's host while this one was stopped, or a failed
/// install); a set this grows reaches the running instances too.
#[cfg(unix)]
pub(crate) fn refresh_trust(key: &str, info: &Instance) {
    let Some(intercept) = &info.intercept else { return };
    let Some(cert) = member_cert(&intercept.bound_hosts()) else { return };
    let fingerprint = crate::credentials::trust::fingerprint(&cert.pem);
    if intercept.installed.as_deref() != Some(fingerprint.as_str()) {
        reinstall(key, &info.container, intercept, &cert.pem, &fingerprint);
    }
    if cert.grew {
        reinstall_running(key, &cert.pem);
    }
}

/// git's settings into the container's system gitconfig too
/// (`credentials::git::SYSTEM_SCRIPT`, as root), for tools that run git with
/// a cleared env; a failure is a warning.
#[cfg(unix)]
pub(super) fn install_git_config(container: &str, entries: &[(String, String)]) {
    use crate::credentials::git;
    if entries.is_empty() {
        return;
    }
    let script = crate::runtime::fixed_path(git::SYSTEM_SCRIPT);
    let args = ["exec", "-i", "-u", "root", container, crate::runtime::SH, "-c", &script];
    if let Err(e) = backend().run_with_stdin(&args, git::system_config(entries).as_bytes()) {
        eprintln!(
            "warning: couldn't write `{}` in `{container}` ({e:#}): git run with a cleared env there misses \
             the git-host claim's settings",
            git::SYSTEM_FILE
        );
    }
}

/// Whether `props`' `ssh-keys` names a key the agent relay lets sign git
/// commits under a git-host claim: `signing-only`, or one with no
/// `destinations` (docs/architecture/helper/ssh-agent.md, *Key filtering*).
pub(crate) fn signs_git(props: &SandboxProperties) -> bool {
    props.ssh_keys.iter().flatten().any(|k| k.signing_only || k.destinations.is_none())
}

/// `devsbd git-check`'s command for `info`: `None` without git's port.
/// `no_signing_key`: check ssh commit signing too (the config is known and
/// names no key that signs git: [`signs_git`]).
pub(crate) fn git_check_command(info: &Instance, no_signing_key: bool) -> Option<Vec<String>> {
    let intercept = info.intercept.as_ref()?;
    let git = intercept.git.as_ref()?;
    let mut cmd: Vec<String> = vec![crate::devsbd::BIN.into(), "git-check".into()];
    cmd.extend(["--proxy".into(), format!("http://127.0.0.1:{}", git.port), "--ca".into(), intercept.bundle.clone()]);
    for host in &git.hosts {
        cmd.extend(["--host".into(), host.clone()]);
    }
    if no_signing_key {
        cmd.push("--no-signing-key".into());
    }
    cmd.push("--".into());
    cmd.push(info.workspace.clone());
    cmd.extend(info.folders.iter().map(|f| f.target.clone()));
    Some(cmd)
}

/// The boot check's warnings (docs/architecture/credentials/git.md, *The
/// boot check*), run as the exec user with the exec env; none when it can't
/// run (no helper, no git-host claim).
#[cfg(unix)]
pub(crate) fn git_check(info: &Instance, no_signing_key: bool) -> Vec<String> {
    let Some(cmd) = git_check_command(info, no_signing_key).filter(|_| info.devsbd_arch.is_some()) else {
        return Vec::new();
    };
    let argv = crate::commands::exec::exec_argv_with(info, false, false, &cmd, false);
    let out = backend().output_quiet(&argv.iter().map(String::as_str).collect::<Vec<_>>()).unwrap_or_default();
    out.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect()
}

#[cfg(not(unix))]
pub(crate) fn git_check(_: &Instance, _: bool) -> Vec<String> {
    Vec::new()
}

/// `kept`, or `None` (each skipped with a warning) where the broker can't
/// serve it yet: off unix (no daemon), and on an instance placed here by
/// another member, whose claims need `CRED` (credential-broker step 5).
fn brokered_here(dir: &Path, kept: instance::Found) -> Result<Option<instance::Found>> {
    let why = if cfg!(unix) {
        let data = crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))?;
        crate::worker_policy::fleet_root_of(&data, dir)?
            .map(|_| format!("isn't brokered on a placed instance ({})", crate::NOT_SUPPORTED_YET))
    } else {
        Some("needs the host daemon, which is unix-only".to_string())
    };
    let Some(why) = why else { return Ok(Some(kept)) };
    for (feature, claim) in &kept {
        eprintln!("warning: feature `{feature}`: the `{}` claim {why}: skipped", claim.kind);
    }
    Ok(None)
}

/// Each claim's credential here (`home::resolve`): none is a warning naming
/// `devsandbox login`; one that hasn't approved the claim's version records
/// it pending, then does as `unapproved` says; a token kind it can't be
/// taken as (`credentials::tokens::pick`) refuses the run.
#[cfg(unix)]
fn check_credentials(
    props: &SandboxProperties,
    profile: Option<&str>,
    kept: &[(String, Claim)],
    unapproved: Unapproved,
) -> Result<()> {
    let data = crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))?;
    let path = crate::credentials::home::path(&data);
    check_credentials_at(&path, props, profile, kept, unapproved, &mut crate::commands::login::Stdio)
}

/// [`check_credentials`] against the `credentials.toml` at `path`, asking
/// on `term`.
#[cfg(unix)]
fn check_credentials_at(
    path: &Path,
    props: &SandboxProperties,
    profile: Option<&str>,
    kept: &[(String, Claim)],
    mode: Unapproved,
    term: &mut dyn crate::commands::login::Term,
) -> Result<()> {
    use crate::credentials::home;
    let creds = home::load(path)?;
    use crate::credentials::{claim::TokenKind, tokens};
    for (feature, claim) in kept {
        let kind = claim.kind.as_str();
        let access = props.access.as_ref().and_then(|a| a.get(kind));
        let named = access.and_then(|a| a.credential.as_deref());
        let wanted = access.and_then(|a| a.as_.as_deref()).map(TokenKind::parse).transpose()?;
        let Some((name, entry)) = home::resolve(&creds, kind, named, profile)? else {
            // No home `as` to narrow by yet: the claim's and step 3's refusals only.
            let any = [TokenKind::App, TokenKind::User];
            tokens::pick(claim.pinned.tokens.as_ref(), &any, wanted, kind, named.unwrap_or(kind))?;
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
            let what = unapproved(name, feature, kind, &version);
            match mode {
                Unapproved::FailOnAsk => return Err(NeedsApproval(what).into()),
                Unapproved::Ask if term.tty() => {
                    if !crate::commands::access::ask_approve(path, name, Some(&version), term)? {
                        bail!("{}", refusal(&what, name));
                    }
                }
                Unapproved::Ask | Unapproved::Refuse => bail!("{}", refusal(&what, name)),
            }
        }
        tokens::pick(claim.pinned.tokens.as_ref(), &tokens::offered(&entry.as_)?, wanted, kind, name)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_credentials(_: &SandboxProperties, _: Option<&str>, _: &[(String, Claim)], _: Unapproved) -> Result<()> {
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
        check_credentials_at(&path, &props(""), None, &kept, Unapproved::Refuse, &mut no_term()).unwrap();

        home::update(&path, |h: &mut Home| Ok(h.insert("sentry-work".into(), Entry::new("sentry", None)))).unwrap();
        let err = check_credentials_at(&path, &props(""), None, &kept, Unapproved::Refuse, &mut no_term()).unwrap_err().to_string();
        assert!(err.contains("run `devsandbox access approve sentry-work`"), "{err}");
        assert_eq!(home::load(&path).unwrap()["sentry-work"].pending[0].version, claim.version());

        home::update(&path, |h: &mut Home| {
            let e = h.get_mut("sentry-work").unwrap();
            let v = e.pending[0].clone();
            Ok(e.approve(v))
        })
        .unwrap();
        check_credentials_at(&path, &props(""), None, &kept, Unapproved::Refuse, &mut no_term()).unwrap();
        home::update(&path, |h: &mut Home| Ok(h.insert("gh".into(), Entry::new("github", None)))).unwrap();
        let err = check_credentials_at(&path, &props("[sandbox.s.access.sentry]\ncredential = \"gh\"\n"), None, &kept, Unapproved::Refuse, &mut no_term())
            .unwrap_err()
            .to_string();
        assert!(err.contains("a `github` credential"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn token_kinds_are_checked_against_the_claim_and_the_home() {
        use crate::credentials::home::{self, Entry, Home, Version};
        let dir = std::env::temp_dir().join(format!("dsv-run-claims-as-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = home::path(&dir);
        let gh = crate::credentials::home::tests::github();
        let kept = vec![("./gh".to_string(), gh.clone())];
        let props = |toml: &str| {
            let config = crate::config::Config::parse(&format!("[sandbox.s]\nimage = \"x\"\n{toml}")).unwrap();
            config.resolve_sandbox("s").unwrap().properties
        };
        let check = |toml: &str| check_credentials_at(&path, &props(toml), None, &kept, Unapproved::Refuse, &mut no_term()).map_err(|e| e.to_string());
        let app = "[sandbox.s.access.github]\nas = \"app\"\n";
        let user = "[sandbox.s.access.github]\nas = \"user\"\n";
        // No credential yet: only the claim's and step 3's refusals.
        check("").unwrap();
        assert!(check(app).unwrap_err().contains("installation tokens: not supported yet!"));

        let mut entry = Entry::new("github", None);
        entry.approve(Version::of(&gh));
        home::update(&path, |h: &mut Home| Ok(h.insert("gh".into(), entry))).unwrap();
        check("").unwrap();
        check(user).unwrap();
        let err = check(app).unwrap_err();
        assert!(err.contains("credential `gh` offers `as = [\"user\"]`, not `app`"), "{err}");
        home::update(&path, |h: &mut Home| {
            h.get_mut("gh").unwrap().as_ = vec!["app".into()];
            Ok(())
        })
        .unwrap();
        assert!(check("").unwrap_err().contains("installation tokens"), "app when user isn't offered");
        assert!(check(user).unwrap_err().contains("offers `as = [\"app\"]`, not `user`"));

        let sentry = vec![("./s".to_string(), crate::credentials::home::tests::sentry())];
        let err = check_credentials_at(&path, &props("[sandbox.s.access.sentry]\nas = \"user\"\n"), None, &sentry, Unapproved::Refuse, &mut no_term())
            .unwrap_err()
            .to_string();
        assert!(err.contains("the `sentry` claim declares no token kinds"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn no_term() -> crate::commands::login::tests::Script {
        crate::commands::login::tests::Script::new(false, &[], None)
    }

    #[cfg(unix)]
    #[test]
    fn an_unapproved_version_is_asked_about_or_failed_for_the_client() {
        use crate::commands::login::tests::Script;
        use crate::credentials::home::{self, Entry, Home, Version};
        let dir = std::env::temp_dir().join(format!("dsv-run-claims-ask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = home::path(&dir);
        let old = crate::credentials::home::tests::sentry();
        let mut new = old.clone();
        new.pinned.never.pop();
        let kept = vec![("./s".to_string(), new.clone())];
        let props = crate::config::Config::parse("[sandbox.s]\nimage = \"x\"\n").unwrap().resolve_sandbox("s").unwrap().properties;
        let mut entry = Entry::new("sentry", None);
        entry.approve(Version::of(&old));
        home::update(&path, |h: &mut Home| Ok(h.insert("sentry-work".into(), entry))).unwrap();
        let approved = || home::load(&path).unwrap()["sentry-work"].approved_claim(&new.version()).is_some();
        let check = |mode, term: &mut Script| check_credentials_at(&path, &props, None, &kept, mode, term);

        // The client's round: recorded pending, its message read back.
        let err = check(Unapproved::FailOnAsk, &mut Script::new(true, &[], None)).unwrap_err();
        let message = err.downcast_ref::<NeedsApproval>().expect("NeedsApproval").0.clone();
        assert_eq!(parse_unapproved(&message), Some(("sentry-work".into(), new.version()[..12].to_string())));
        assert!(!message.contains("access approve"), "the client adds it on a no: {message}");
        assert!(refusal(&message, "sentry-work").ends_with("run `devsandbox access approve sentry-work`, then try again"));

        // Off a terminal, asking refuses as `Refuse` does.
        let err = check(Unapproved::Ask, &mut no_term()).unwrap_err().to_string();
        assert!(err.contains("run `devsandbox access approve sentry-work`"), "{err}");

        let mut term = Script::new(true, &[false], None);
        let err = check(Unapproved::Ask, &mut term).unwrap_err().to_string();
        assert!(err.contains("run `devsandbox access approve sentry-work`"), "{err}");
        assert!(term.shown.contains("against approved"), "{}", term.shown);
        assert!(term.shown.ends_with("Approve this claim for `sentry-work`?"), "{}", term.shown);
        assert!(!approved());
        assert_eq!(home::load(&path).unwrap()["sentry-work"].pending.len(), 1);

        check(Unapproved::Ask, &mut Script::new(true, &[true], None)).unwrap();
        assert!(approved());
        assert!(home::load(&path).unwrap()["sentry-work"].pending.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
