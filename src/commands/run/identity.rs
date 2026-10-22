//! The identity probe (docs/architecture/runtimes.md, *Mode detection*): the
//! user-namespace mode of the runtime context this host's devsandbox talks
//! to, cached in `state.toml` per context. The cache is consulted first, then
//! the runtime's metadata (`Backend::identity_fast_path`), then the canary
//! (`Backend::identity_canary`) when the metadata can't tell or on
//! `--reprobe`. Probed where the container is created: a machine probes for
//! itself.
//!
//! Also `run`'s side of the plan (docs/architecture/instances/identity.md):
//! [`plan_for`] decides it, [`create_args`] turns it into `run` args, and
//! [`apply_argv`] / [`apply`] run `devsbd identity apply` in the new
//! container.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::config::{parse_shorthand, SandboxProperties};
use crate::runtime::backend;
use crate::runtime::identity::{self, Accounts, Engine, FastPath, Identity, Ids, Mode, Plan, Probe};
use crate::state::State;

use super::IDENTITY_LABEL;

/// The plan for a container from `image`, said once on stderr: its summary,
/// then its warnings and any fallback. Never fails: whatever goes wrong
/// (probe, no helper) falls back to `identity = "image"` ([`decide_plan`]).
/// The image's `USER` is read only when it could be the adopted account.
pub(super) fn plan_for(image: &str, props: &SandboxProperties, reprobe: bool) -> Plan {
    let identity = props.identity();
    let image_user = (identity == Identity::Host && props.container_user.is_none() && props.remote_user.is_none())
        .then(|| backend().image_user(image).ok().flatten())
        .flatten();
    let accounts = Accounts {
        container_user: props.container_user.as_deref(),
        remote_user: props.remote_user.as_deref(),
        image_user: image_user.as_deref(),
    };
    let probe = probe(image, reprobe);
    let (plan, lines) = decide_plan(identity, probe, crate::devsbd::embedded(), accounts, host_ids());
    eprintln!("identity: {}", plan.summary());
    for line in lines {
        eprintln!("{line}");
    }
    plan
}

/// [`identity::plan`] with `run`'s fallbacks, plus the lines to print after
/// the summary (`warning: …` / `note: …`). A failed probe plans an unknown
/// mode (taken literally, nothing relabeled). `identity = "host"` falls back
/// to `"image"` when nothing could remap: no helper in this build, or no
/// host ids where the plan needs them (rootful). Other modes don't read the
/// host ids, so a placeholder stands in for them there.
fn decide_plan(
    identity: Identity,
    probe: Result<Probe>,
    helper: bool,
    accounts: Accounts,
    host: Result<Ids>,
) -> (Plan, Vec<String>) {
    let mut lines = Vec::new();
    let host_wanted = identity == Identity::Host;
    let probe = probe.unwrap_or_else(|e| {
        let then = if host_wanted { "running as identity = \"image\"" } else { "binds are not relabeled" };
        lines.push(format!("warning: identity probe failed: {e:#}; {then}"));
        Probe { mode: Mode::Unknown, selinux: false }
    });
    let mut identity = identity;
    let mut ids = identity::ASSUMED;
    if host_wanted && probe.mode != Mode::Unknown {
        match (&host, helper) {
            (_, false) => {
                lines.push(
                    "note: this build has no devsbd helper (see scripts/build-devsbd.sh), so nothing can remap: \
                     running as identity = \"image\""
                        .into(),
                );
                identity = Identity::Image;
            }
            (Err(e), true) if probe.mode == Mode::Rootful => {
                lines.push(format!("warning: cannot read the host's ids ({e:#}): running as identity = \"image\""));
                identity = Identity::Image;
            }
            (Ok(h), true) => ids = *h,
            (Err(_), true) => {}
        }
    }
    let plan = identity::plan(probe, identity, accounts, ids);
    let warnings = plan.warnings.iter().map(|w| format!("warning: {w}"));
    lines.splice(0..0, warnings);
    (plan, lines)
}

/// The plan's part of `run`'s args: the [`IDENTITY_LABEL`] label, `--user`,
/// `--userns`, then `volumes` as `-v` and `mounts` as `--mount`, relabeled
/// when SELinux asks for it (`plan.relabel`): `-v` binds get `z`; a `--mount`
/// bind gets `relabel=shared` on podman, while docker's `--mount` has no
/// relabel option, so its binds are named in the returned warning instead.
pub(super) fn create_args(plan: &Plan, podman: bool, volumes: &[String], mounts: &[String]) -> (Vec<String>, Option<String>) {
    let mut args = vec!["--label".to_string(), format!("{IDENTITY_LABEL}={}", plan.label())];
    if let Some(user) = &plan.create_user {
        args.extend(["--user".into(), user.clone()]);
    }
    if let Some(userns) = &plan.userns {
        args.extend(["--userns".into(), userns.clone()]);
    }
    for volume in volumes {
        let value = if plan.relabel { relabel_volume(volume) } else { volume.clone() };
        args.extend(["-v".into(), value]);
    }
    let mut unlabeled = Vec::new();
    for mount in mounts {
        let bind = parse_shorthand(mount).ok().filter(|(kind, ..)| kind.as_deref().unwrap_or("bind") == "bind");
        let value = match bind {
            Some(_) if plan.relabel && podman => format!("{mount},relabel=shared"),
            Some((_, _, target, _)) if plan.relabel => {
                unlabeled.push(target);
                mount.clone()
            }
            _ => mount.clone(),
        };
        args.extend(["--mount".into(), value]);
    }
    let warning = (!unlabeled.is_empty()).then(|| {
        format!(
            "SELinux is enforcing but docker's `--mount` can't relabel binds, so these may be unreadable inside: {}; \
             relabel their host paths (`chcon -Rt container_file_t <path>`) or use podman",
            unlabeled.join(", ")
        )
    });
    (args, warning)
}

/// A `-v src:dst[:opts]` with the shared SELinux label (`z`) added.
fn relabel_volume(volume: &str) -> String {
    if volume.matches(':').count() >= 2 {
        format!("{volume},z")
    } else {
        format!("{volume}:z")
    }
}

/// The docker socket paths a bind may hand the container.
const DOCKER_SOCKETS: [&str; 2] = ["/var/run/docker.sock", "/run/docker.sock"];

/// The host path of the docker socket among `mounts` (`--mount` values): a
/// bind whose source is a usual socket path or `DOCKER_HOST`'s unix path
/// (`docker_host`). Only on rootful plans: under a user namespace or a VM's
/// share the host gid means nothing inside, so there's no group to add.
fn docker_socket_source(mode: Mode, mounts: &[String], docker_host: Option<&str>) -> Option<String> {
    if mode != Mode::Rootful {
        return None;
    }
    let from_env = docker_host.and_then(|h| h.strip_prefix("unix://"));
    mounts.iter().find_map(|mount| {
        let (kind, source, ..) = parse_shorthand(mount).ok()?;
        let source = source.filter(|_| kind.as_deref().unwrap_or("bind") == "bind")?;
        (DOCKER_SOCKETS.contains(&source.as_str()) || from_env == Some(source.as_str())).then_some(source)
    })
}

/// The targets of the named volumes among `mounts` (`--mount` values),
/// feature mounts included: `devsbd identity apply --volume` hands the empty
/// root-owned ones to the account.
pub(super) fn volume_targets(mounts: &[String]) -> Vec<String> {
    mounts
        .iter()
        .filter_map(|mount| parse_shorthand(mount).ok())
        .filter(|(kind, ..)| kind.as_deref() == Some("volume"))
        .map(|(_, _, target, _)| target)
        .collect()
}

/// The docker socket's gid on the host, when `mounts` binds it
/// ([`docker_socket_source`]); `None` too when it can't be read.
pub(super) fn socket_gid(mode: Mode, mounts: &[String]) -> Option<u32> {
    let docker_host = std::env::var("DOCKER_HOST").ok();
    let source = docker_socket_source(mode, mounts, docker_host.as_deref())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(source).ok().map(|m| m.gid())
    }
    #[cfg(not(unix))]
    {
        let _ = source;
        None
    }
}

/// The root exec of `devsbd identity apply` that fits the adopted account to
/// `plan` in `container`; `None` when there's nothing to fit (`identity =
/// "image"`, or no account to adopt). The mode flag: `--remap` to the host
/// ids (rootful), `--expect` the assumed ids (keep-id), `--root-home`
/// (rootless docker), none (VM-backed: only the home, socket and volumes).
pub(super) fn apply_argv(container: &str, plan: &Plan, socket_gid: Option<u32>, volumes: &[String]) -> Option<Vec<String>> {
    let account = plan.account.as_deref().filter(|_| plan.identity == Identity::Host)?;
    let mut args: Vec<String> =
        ["exec", "-u", "root", container, crate::devsbd::BIN, "identity", "apply", account].map(String::from).into();
    if let Some(remap) = &plan.remap {
        args.extend(["--remap".into(), format!("{}:{}", remap.uid, remap.gid)]);
    } else if plan.userns.is_some() {
        args.extend(["--expect".into(), format!("{}:{}", identity::ASSUMED.uid, identity::ASSUMED.gid)]);
    } else if plan.root_home {
        args.push("--root-home".into());
    }
    if let Some(gid) = socket_gid {
        args.extend(["--socket-gid".into(), gid.to_string()]);
    }
    for volume in volumes {
        args.extend(["--volume".into(), volume.clone()]);
    }
    Some(args)
}

/// Run [`apply_argv`]'s exec; the account's home on success, its warnings
/// passed through to stderr.
pub(super) fn apply(argv: &[String]) -> Result<String> {
    let out = Command::new(backend().bin())
        .args(argv)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {} (is it installed?)", backend().bin()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (home, warnings) = apply_outcome(out.status.success(), &stdout, &stderr)?;
    for warning in warnings {
        eprintln!("{warning}");
    }
    Ok(home)
}

const APPLY_PREFIX: &str = "devsbd: identity: ";

/// `devsbd identity apply`'s outcome (contract in
/// docs/architecture/helper/in-container.md): the home from its one stdout
/// line `home=<path>`, and its stderr lines as `run` prints them
/// (`devsbd: identity: warning: x` → `warning: identity: x`). A failure is
/// an error carrying the helper's why.
fn apply_outcome(success: bool, stdout: &str, stderr: &str) -> Result<(String, Vec<String>)> {
    let lines = stderr.lines().map(str::trim).filter(|l| !l.is_empty());
    if !success {
        let why: Vec<&str> = lines.map(|l| l.strip_prefix(APPLY_PREFIX).unwrap_or(l)).collect();
        bail!("devsbd identity apply failed: {}", if why.is_empty() { "no reason given".into() } else { why.join("; ") });
    }
    let warnings = lines
        .map(|l| match l.strip_prefix(APPLY_PREFIX).and_then(|w| w.strip_prefix("warning: ")) {
            Some(w) => format!("warning: identity: {w}"),
            None => l.to_string(),
        })
        .collect();
    let mut out = stdout.lines().filter(|l| !l.trim().is_empty());
    match (out.next().and_then(|l| l.strip_prefix("home=")), out.next()) {
        (Some(home), None) if !home.trim().is_empty() => Ok((home.trim().to_string(), warnings)),
        _ => bail!("devsbd identity apply: unexpected output `{}`", stdout.trim()),
    }
}

/// The probe for the current runtime context. `image` is the sandbox's
/// (already built or pulled) image, which the canary runs from; `reprobe`
/// skips the cache and runs the canary even when the metadata is clear. An
/// image without `/bin/sh` fails the canary: the error is the caller's to
/// handle.
pub(crate) fn probe(image: &str, reprobe: bool) -> Result<Probe> {
    let b = backend();
    // Apple `container`: every container is its own VM; nothing to run.
    if b.name() == "container" {
        return Ok(Probe { mode: Mode::VmBacked, selinux: false });
    }
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let key = identity::context_key(b.name(), env, docker_current_context);
    let cached = State::load()?.identity_probes.get(&key).copied();
    let engine = if b.name() == "podman" { Engine::Podman } else { Engine::Docker };
    let (probe, fresh) = decide(cached, reprobe, || b.identity_fast_path(), |relabel| {
        let dir = State::path()?.with_file_name(format!("identity-probe-{}", std::process::id()));
        let canary = with_canary_dir(&dir, |dir| b.identity_canary(image, dir, relabel))?;
        let host = host_ids()?;
        identity::classify(host, canary.as_root, canary.as_1000, engine).with_context(|| {
            format!(
                "identity canary: unrecognized ownership (host {}:{}, uid 0 wrote as {}:{}, uid {} as {}:{})",
                host.uid,
                host.gid,
                canary.as_root.uid,
                canary.as_root.gid,
                identity::ASSUMED.uid,
                canary.as_1000.uid,
                canary.as_1000.gid
            )
        })
    })?;
    if fresh {
        State::update(|state| {
            state.identity_probes.insert(key, probe);
            Ok(())
        })?;
    }
    Ok(probe)
}

/// The probe's decision, I/O injected: the cached probe unless `reprobe`,
/// else the metadata, else (ambiguous metadata, or `reprobe`) the canary,
/// told whether to relabel its bind. SELinux always comes from the metadata.
/// The `bool` is whether the result is new (to be cached).
fn decide(
    cached: Option<Probe>,
    reprobe: bool,
    fast_path: impl FnOnce() -> Result<FastPath>,
    canary: impl FnOnce(bool) -> Result<Mode>,
) -> Result<(Probe, bool)> {
    if let (Some(probe), false) = (cached, reprobe) {
        return Ok((probe, false));
    }
    let fast = fast_path().context("cannot read the runtime's info")?;
    let mode = match fast.mode {
        Ok(mode) if !reprobe => mode,
        Ok(_) => canary(fast.selinux)?,
        Err(why) => canary(fast.selinux).with_context(|| format!("{why}, so the identity canary decides"))?,
    };
    Ok((Probe { mode, selinux: fast.selinux }, true))
}

/// Run `f` over a fresh, empty `dir` any uid can write (the canary's
/// writers are uid 0 and 1000, mapped who knows where), removed after. It
/// sits next to `state.toml`, under `$HOME`: the dir VM file shares (colima,
/// lima) cover by default, which the system temp dir may not be.
fn with_canary_dir<T>(dir: &Path, f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777))
            .with_context(|| format!("cannot open up {}", dir.display()))?;
    }
    let out = f(dir);
    // The dir is ours, so its foreign-owned files unlink fine.
    let _ = std::fs::remove_dir_all(dir);
    out
}

#[cfg(unix)]
fn host_ids() -> Result<Ids> {
    // SAFETY: getuid/getgid have no preconditions and can't fail.
    Ok(Ids { uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() } })
}

#[cfg(not(unix))]
fn host_ids() -> Result<Ids> {
    anyhow::bail!("the identity canary needs a unix host")
}

/// The docker CLI's `currentContext` (`$DOCKER_CONFIG/config.json`, else
/// `~/.docker/config.json`), as the CLI itself resolves it.
fn docker_current_context() -> Option<String> {
    let dir = std::env::var_os("DOCKER_CONFIG")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".docker")))?;
    identity::docker_current_context(&std::fs::read_to_string(dir.join("config.json")).ok()?)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use anyhow::bail;

    use super::*;

    const ROOTFUL: Probe = Probe { mode: Mode::Rootful, selinux: false };

    fn fast(mode: Result<Mode, &'static str>, selinux: bool) -> impl FnOnce() -> Result<FastPath> {
        move || Ok(FastPath { mode, selinux })
    }

    #[test]
    fn cache_hit_runs_nothing() {
        let out = decide(Some(ROOTFUL), false, || panic!("fast path ran"), |_| panic!("canary ran")).unwrap();
        assert_eq!(out, (ROOTFUL, false));
    }

    #[test]
    fn clear_metadata_skips_the_canary() {
        let out = decide(None, false, fast(Ok(Mode::RootlessDocker), true), |_| panic!("canary ran")).unwrap();
        assert_eq!(out, (Probe { mode: Mode::RootlessDocker, selinux: true }, true));
    }

    #[test]
    fn ambiguous_metadata_runs_the_canary_with_its_selinux() {
        let relabeled = Cell::new(None);
        let canary = |relabel| {
            relabeled.set(Some(relabel));
            Ok(Mode::VmBacked)
        };
        let out = decide(None, false, fast(Err("remote"), true), canary).unwrap();
        assert_eq!(out, (Probe { mode: Mode::VmBacked, selinux: true }, true));
        assert_eq!(relabeled.get(), Some(true));
    }

    #[test]
    fn reprobe_skips_the_cache_and_runs_the_canary() {
        let ran = Cell::new(false);
        let canary = |_| {
            ran.set(true);
            Ok(Mode::RootlessPodman)
        };
        let out = decide(Some(ROOTFUL), true, fast(Ok(Mode::Rootful), false), canary).unwrap();
        assert_eq!(out, (Probe { mode: Mode::RootlessPodman, selinux: false }, true));
        assert!(ran.get());
    }

    #[test]
    fn failures_surface_with_why() {
        let err = decide(None, false, fast(Err("the docker daemon remaps users"), false), |_| bail!("no sh"))
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("remaps users, so the identity canary decides") && text.contains("no sh"), "{text}");
        let err = decide(None, false, || bail!("daemon down"), |_| panic!("canary ran")).unwrap_err();
        assert!(format!("{err:#}").contains("daemon down"));
    }

    const HOST: Ids = Ids { uid: 501, gid: 20 };

    fn vscode() -> Accounts<'static> {
        Accounts { container_user: None, remote_user: Some("vscode"), image_user: None }
    }

    fn probe_of(mode: Mode) -> Result<Probe> {
        Ok(Probe { mode, selinux: false })
    }

    #[test]
    fn decide_plan_host_with_helper() {
        let (p, lines) = decide_plan(Identity::Host, probe_of(Mode::Rootful), true, vscode(), Ok(HOST));
        assert_eq!((p.identity, p.remap.map(|r| (r.uid, r.gid))), (Identity::Host, Some((501, 20))));
        assert!(lines.is_empty(), "{lines:?}");
        // Non-rootful plans don't read the host ids.
        let (p, lines) = decide_plan(Identity::Host, probe_of(Mode::VmBacked), true, vscode(), Err(anyhow::anyhow!("x")));
        assert_eq!((p.identity, p.exec_user.as_deref()), (Identity::Host, Some("vscode")));
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn decide_plan_falls_back_to_image() {
        // No helper: nothing can remap.
        let (p, lines) = decide_plan(Identity::Host, probe_of(Mode::Rootful), false, vscode(), Ok(HOST));
        assert_eq!((p.identity, p.mode, p.remap), (Identity::Image, Mode::Rootful, None));
        assert!(lines.len() == 1 && lines[0].starts_with("note: this build has no devsbd helper"), "{lines:?}");
        // Probe failure: unknown mode, said why.
        let (p, lines) = decide_plan(Identity::Host, Err(anyhow::anyhow!("no sh")), true, vscode(), Ok(HOST));
        assert_eq!((p.identity, p.mode), (Identity::Image, Mode::Unknown));
        assert_eq!(p.exec_user.as_deref(), Some("vscode"));
        assert_eq!(lines, ["warning: identity probe failed: no sh; running as identity = \"image\""]);
        // Rootful without host ids.
        let (p, lines) = decide_plan(Identity::Host, probe_of(Mode::Rootful), true, vscode(), Err(anyhow::anyhow!("x")));
        assert_eq!(p.identity, Identity::Image);
        assert!(lines[0].starts_with("warning: cannot read the host's ids"), "{lines:?}");
    }

    #[test]
    fn decide_plan_image_and_plan_warnings() {
        let (p, lines) = decide_plan(Identity::Image, Err(anyhow::anyhow!("down")), false, vscode(), Ok(HOST));
        assert_eq!((p.identity, p.mode), (Identity::Image, Mode::Unknown));
        assert_eq!(lines, ["warning: identity probe failed: down; binds are not relabeled"]);
        // The plan's own warnings come first, as `warning:` lines.
        let (_, lines) = decide_plan(Identity::Host, probe_of(Mode::Rootful), true, Accounts::default(), Ok(HOST));
        assert_eq!(lines, [format!("warning: {}", identity::NO_ACCOUNT_WARNING)]);
    }

    fn host_plan(mode: Mode, selinux: bool) -> Plan {
        identity::plan(Probe { mode, selinux }, Identity::Host, vscode(), HOST)
    }

    #[test]
    fn create_args_per_mode() {
        let label = |p: &Plan| format!("{IDENTITY_LABEL}={}", p.label());
        let p = host_plan(Mode::Rootful, false);
        let (args, warning) = create_args(&p, false, &[], &[]);
        assert_eq!(args, ["--label".into(), label(&p), "--user".into(), "root".into()]);
        assert_eq!(warning, None);
        let p = host_plan(Mode::RootlessPodman, false);
        let (args, _) = create_args(&p, true, &[], &[]);
        assert_eq!(args, ["--label".into(), label(&p), "--userns".into(), "keep-id:uid=1000,gid=1000".into()]);
        let p = host_plan(Mode::VmBacked, false);
        let (args, _) = create_args(&p, false, &[], &[]);
        assert_eq!(args, ["--label".to_string(), label(&p)]);
        // Image identity: `containerUser` as is.
        let accounts = Accounts { container_user: Some("app"), ..Default::default() };
        let p = identity::plan(Probe { mode: Mode::Rootful, selinux: false }, Identity::Image, accounts, HOST);
        let (args, _) = create_args(&p, false, &[], &[]);
        assert_eq!(args[2..], ["--user", "app"]);
    }

    #[test]
    fn create_args_relabel_binds() {
        let volumes = ["/src:/workspaces/x".to_string(), "/a:/b:ro".to_string()];
        let mounts = [
            "type=bind,source=/c,target=/var/cache/devsandbox/cargo".to_string(),
            "type=volume,source=v,target=/v".to_string(),
            "source=/d,target=/d,type=bind,readonly".to_string(),
        ];
        let tail = |args: Vec<String>| args[4..].to_vec();
        // Podman: every bind relabeled, volumes left alone.
        let (args, warning) = create_args(&host_plan(Mode::Rootful, true), true, &volumes, &mounts);
        assert_eq!(
            tail(args),
            [
                "-v", "/src:/workspaces/x:z", "-v", "/a:/b:ro,z",
                "--mount", "type=bind,source=/c,target=/var/cache/devsandbox/cargo,relabel=shared",
                "--mount", "type=volume,source=v,target=/v",
                "--mount", "source=/d,target=/d,type=bind,readonly,relabel=shared",
            ]
        );
        assert_eq!(warning, None);
        // Docker: `-v` relabeled, `--mount` binds named in one warning.
        let (args, warning) = create_args(&host_plan(Mode::Rootful, true), false, &volumes, &mounts);
        let args = tail(args);
        assert_eq!(args[1], "/src:/workspaces/x:z");
        assert_eq!(args[4..], mounts.iter().flat_map(|m| ["--mount".to_string(), m.clone()]).collect::<Vec<_>>()[..]);
        let warning = warning.unwrap();
        assert!(warning.contains("/var/cache/devsandbox/cargo, /d") && !warning.contains("/v,"), "{warning}");
        // No SELinux: untouched.
        let (args, warning) = create_args(&host_plan(Mode::Rootful, false), true, &volumes, &mounts);
        assert_eq!(args[5], "/src:/workspaces/x");
        assert_eq!(args[9], mounts[0]);
        assert_eq!(warning, None);
    }

    #[test]
    fn apply_argv_per_mode() {
        let base = ["exec", "-u", "root", "c", crate::devsbd::BIN, "identity", "apply", "vscode"];
        let argv = |p: &Plan, gid, vols: &[String]| apply_argv("c", p, gid, vols).unwrap();
        assert_eq!(argv(&host_plan(Mode::Rootful, false), None, &[])[8..], ["--remap", "501:20"]);
        assert_eq!(argv(&host_plan(Mode::Rootful, false), None, &[])[..8], base);
        assert_eq!(argv(&host_plan(Mode::RootlessPodman, false), None, &[])[8..], ["--expect", "1000:1000"]);
        assert_eq!(argv(&host_plan(Mode::RootlessDocker, false), None, &[])[8..], ["--root-home"]);
        assert!(argv(&host_plan(Mode::VmBacked, false), None, &[])[8..].is_empty());
        let vols = ["/v".to_string(), "/w".to_string()];
        assert_eq!(
            argv(&host_plan(Mode::Rootful, false), Some(998), &vols)[10..],
            ["--socket-gid", "998", "--volume", "/v", "--volume", "/w"]
        );
        // Nothing to fit: image identity, or no account.
        let image = identity::plan(Probe { mode: Mode::Rootful, selinux: false }, Identity::Image, vscode(), HOST);
        assert_eq!(apply_argv("c", &image, None, &[]), None);
        let root = identity::plan(Probe { mode: Mode::Rootful, selinux: false }, Identity::Host, Accounts::default(), HOST);
        assert_eq!(apply_argv("c", &root, None, &[]), None);
    }

    #[test]
    fn socket_source_and_volume_targets() {
        let mounts = [
            "type=bind,source=/var/run/docker.sock,target=/var/run/docker-host.sock".to_string(),
            "type=volume,source=dind-${instance},target=/var/lib/docker".to_string(),
            "type=volume,source=x,target=/home/vscode/.cache".to_string(),
        ];
        assert_eq!(docker_socket_source(Mode::Rootful, &mounts, None).as_deref(), Some("/var/run/docker.sock"));
        for mode in [Mode::RootlessDocker, Mode::RootlessPodman, Mode::VmBacked, Mode::Unknown] {
            assert_eq!(docker_socket_source(mode, &mounts, None), None, "{mode:?}");
        }
        let custom = ["type=bind,source=/srv/d.sock,target=/var/run/docker.sock".to_string()];
        assert_eq!(docker_socket_source(Mode::Rootful, &custom, None), None);
        assert_eq!(
            docker_socket_source(Mode::Rootful, &custom, Some("unix:///srv/d.sock")).as_deref(),
            Some("/srv/d.sock")
        );
        // A volume named like the socket isn't it.
        let vol = ["type=volume,source=/run/docker.sock,target=/x".to_string()];
        assert_eq!(docker_socket_source(Mode::Rootful, &vol, None), None);
        assert_eq!(volume_targets(&mounts), ["/var/lib/docker", "/home/vscode/.cache"]);
    }

    #[test]
    fn apply_outcome_parses_home_and_warnings() {
        let (home, warnings) = apply_outcome(true, "home=/home/vscode\n", "").unwrap();
        assert_eq!((home.as_str(), warnings.len()), ("/home/vscode", 0));
        let stderr = "devsbd: identity: warning: volume /v does not exist\nsomething else\n";
        let (_, warnings) = apply_outcome(true, "home=/root\n", stderr).unwrap();
        assert_eq!(warnings, ["warning: identity: volume /v does not exist", "something else"]);
        for bad in ["", "home=\n", "x=1\n", "home=/a\nhome=/b\n"] {
            assert!(apply_outcome(true, bad, "").is_err(), "{bad:?}");
        }
        let err = apply_outcome(false, "", "devsbd: identity: no account `vscode`\n").unwrap_err();
        assert_eq!(err.to_string(), "devsbd identity apply failed: no account `vscode`");
        let err = apply_outcome(false, "", "").unwrap_err();
        assert!(err.to_string().ends_with("no reason given"));
    }

    #[test]
    fn canary_dir_is_world_writable_and_removed() {
        let dir = std::env::temp_dir().join(format!("devsandbox-canary-dir-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("stale")).unwrap();
        let seen = with_canary_dir(&dir, |d| {
            let entries = std::fs::read_dir(d)?.count();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(d)?.permissions().mode() & 0o777, 0o777);
            }
            std::fs::write(d.join("0"), "")?;
            Ok(entries)
        })
        .unwrap();
        assert_eq!(seen, 0, "a leftover dir is emptied first");
        assert!(!dir.exists());
        assert!(with_canary_dir(&dir, |_| -> Result<()> { bail!("x") }).is_err());
        assert!(!dir.exists());
    }
}
