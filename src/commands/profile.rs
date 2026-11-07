//! `devsandbox profile` and the daemon's `instances.setProfile`: show or
//! switch a running instance's profile (docs/architecture/instances/profiles.md,
//! *Switching*). Only the user switches: nothing a container can send
//! reaches this (no control request names a profile).

use std::path::Path;

use anyhow::{Result, bail};

use crate::config::Config;
use crate::state::{Instance, State};

/// Why a switch changed nothing (I/O errors aside), typed so the daemon's
/// `instances.setProfile` can answer `not-found` vs `invalid`; the message
/// is the CLI's error either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// No instance by that exact name.
    Unknown(String),
    /// A profile its sandbox doesn't allow, an allowed set that can't be
    /// told (no recorded root, unreadable config, sandbox gone), or the
    /// instance is mid-`run`/`rebuild`.
    Invalid(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Unknown(m) | Refused::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Refused {}

fn unknown(name: &str) -> Refused {
    Refused::Unknown(format!("no instance named `{name}` (see `devsandbox ps -a`)"))
}

/// `devsandbox profile <name> [<profile>]`: without `profile`, print the
/// instance's profile and what its sandbox allows now; with it, switch.
/// `name` is the exact instance key, as `rename` takes it.
pub fn profile(name: &str, profile: Option<&str>) -> Result<()> {
    let state_path = State::path()?;
    match profile {
        Some(p) => {
            let p = set_profile_at(&state_path, name, p)?;
            println!("{}", profile_line(name, Some(&p), false));
        }
        None => {
            let state = State::load_at(&state_path)?;
            let Some(inst) = state.instances.get(name) else { bail!(unknown(name)) };
            let allowed = allowed_for(&mut None, inst);
            let disallowed = crate::snapshot::profile_disallowed(inst.profile.as_deref(), allowed.as_deref().ok());
            for line in show_lines(name, inst.profile.as_deref(), disallowed, Some(&allowed)) {
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// The line naming `name`'s profile: what a switch prints, and the show
/// form's first line (also the re-run on a machine, `machines::cli::calls`).
pub(crate) fn profile_line(name: &str, profile: Option<&str>, disallowed: bool) -> String {
    let p = profile.unwrap_or("(none)");
    let flag = if disallowed { " (its sandbox doesn't allow it any more)" } else { "" };
    format!("profile {name}: {p}{flag}")
}

/// The show form: [`profile_line`], then the allowed set when known
/// (`allowed`: `None` on a machine, whose rows carry the profile and its
/// flag but not the set).
pub(crate) fn show_lines(
    name: &str,
    profile: Option<&str>,
    disallowed: bool,
    allowed: Option<&Result<Vec<String>, String>>,
) -> Vec<String> {
    let mut lines = vec![profile_line(name, profile, disallowed)];
    match allowed {
        Some(Ok(a)) if a.is_empty() => lines.push("allowed: (none: its sandbox has no profiles)".into()),
        Some(Ok(a)) => lines.push(format!("allowed: {}", a.join(", "))),
        Some(Err(why)) => lines.push(format!("allowed: unknown ({why})")),
        None => {}
    }
    lines
}

/// The profiles `inst`'s sandbox allows in its own config root's config as
/// it is now (`Config::allowed_profiles`), or why that can't be told. Like
/// `snapshot::allowed_now`, with the reason kept for the user. `loaded`
/// lets a caller pass a config it already read for that root.
fn allowed_for(loaded: &mut Option<Config>, inst: &Instance) -> Result<Vec<String>, String> {
    let Some(dir) = inst.config_dir.as_ref() else {
        return Err("it has no recorded config root (`devsandbox rebuild` records one)".into());
    };
    let config = match loaded {
        Some(c) => c,
        None => loaded.insert(Config::load(dir).map_err(|e| format!("can't read the config in {}: {e:#}", dir.display()))?),
    };
    let resolved = config
        .resolve_sandbox(&inst.sandbox)
        .map_err(|_| format!("sandbox `{}` isn't in the config in {} any more", inst.sandbox, dir.display()))?;
    Ok(config.allowed_profiles(&resolved))
}

/// Whether `sandbox`'s `allowed` set takes `profile`, in `run --profile`'s
/// words (`run::choose_profile`).
fn check(sandbox: &str, allowed: &[String], profile: &str) -> Result<(), Refused> {
    if allowed.is_empty() {
        return Err(Refused::Invalid(format!("sandbox `{sandbox}` has no profiles")));
    }
    if !allowed.iter().any(|a| a == profile) {
        return Err(Refused::Invalid(format!(
            "sandbox `{sandbox}` doesn't allow profile `{profile}`; allowed: {}",
            allowed.join(", ")
        )));
    }
    Ok(())
}

/// Silent core shared with the daemon's `instances.setProfile`, so it never
/// touches stdout/stderr or prompts. Switches instance `name` (exact key)
/// to `profile` and makes it its sandbox's `last_profile` (`run`'s
/// default), in one locked update of the state file at `state`. The check
/// reads the record inside that update, so it judges the record it writes;
/// the config is read there too (a TOML parse). Nothing restarts: the
/// container doesn't know its profile. A refusal is a [`Refused`] (downcast
/// it). Returns the profile set.
pub fn set_profile_at(state: &Path, name: &str, profile: &str) -> Result<String> {
    State::update_at(state, |state| {
        let mut config = None;
        set_in_state(state, name, profile, &mut config)
    })
}

/// [`set_profile_at`]'s change to `state`; `config` as [`allowed_for`]'s,
/// so tests pass one in without a config file.
fn set_in_state(state: &mut State, name: &str, profile: &str, config: &mut Option<Config>) -> Result<String> {
    let Some(info) = state.instances.get(name) else { return Err(unknown(name).into()) };
    // A `run`/`rebuild` mid-flight saves the record it loaded before the
    // build, which would put the old profile back.
    if let Some(why) = crate::commands::in_flight(name, info.progress.as_ref(), info.phase_view().as_ref()) {
        return Err(Refused::Invalid(why).into());
    }
    let allowed = allowed_for(config, info).map_err(|why| Refused::Invalid(format!("can't switch `{name}`'s profile: {why}")))?;
    check(&info.sandbox, &allowed, profile)?;
    // `Instance::project` is `services::project_id` of the root `run` read,
    // the key `run` keeps `last_profile` under.
    let project = match &info.project {
        p if !p.is_empty() => p.clone(),
        _ => crate::commands::services::project_id(info.config_dir.as_deref().expect("allowed_for checked it"))?,
    };
    let sandbox = info.sandbox.clone();
    state.instances.get_mut(name).expect("checked above").profile = Some(profile.to_string());
    state.last_profile.entry(project).or_default().insert(sandbox, profile.to_string());
    Ok(profile.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "[profiles.work]\n[profiles.personal]\n[sandbox.web]\nimage = \"x\"\nprofiles = [\"work\", \"personal\"]\n\
                          [sandbox.bare]\nimage = \"x\"\nprofiles = []\n";

    fn state(root: Option<&str>, sandbox: &str) -> State {
        let root = root.map(|r| format!("config_dir = \"{r}\"\n")).unwrap_or_default();
        toml::from_str(&format!(
            "[instance.a]\nsandbox = \"{sandbox}\"\nproject = \"abc12345\"\ncontainer = \"c\"\nfolder = \"/f\"\n\
             workspace = \"/w\"\ncreated_unix = 0\nprofile = \"work\"\n{root}"
        ))
        .unwrap()
    }

    fn config() -> Option<Config> {
        Some(Config::parse(CONFIG).unwrap())
    }

    #[test]
    fn sets_the_profile_and_last_profile() {
        let mut s = state(Some("/cfg"), "web");
        assert_eq!(set_in_state(&mut s, "a", "personal", &mut config()).unwrap(), "personal");
        assert_eq!(s.instances["a"].profile.as_deref(), Some("personal"));
        assert_eq!(s.last_profile["abc12345"]["web"], "personal");
    }

    #[test]
    fn refusals_change_nothing() {
        let refused = |s: &mut State, name: &str, p: &str, c: &mut Option<Config>| {
            set_in_state(s, name, p, c).unwrap_err().downcast::<Refused>().unwrap()
        };
        let mut s = state(Some("/cfg"), "web");
        assert_eq!(
            refused(&mut s, "nope", "work", &mut config()),
            Refused::Unknown("no instance named `nope` (see `devsandbox ps -a`)".into())
        );
        assert_eq!(
            refused(&mut s, "a", "other", &mut config()),
            Refused::Invalid("sandbox `web` doesn't allow profile `other`; allowed: work, personal".into())
        );
        let mut bare = state(Some("/cfg"), "bare");
        assert_eq!(refused(&mut bare, "a", "work", &mut config()), Refused::Invalid("sandbox `bare` has no profiles".into()));
        let mut gone = state(Some("/cfg"), "gone");
        let Refused::Invalid(m) = refused(&mut gone, "a", "work", &mut config()) else { panic!() };
        assert_eq!(m, "can't switch `a`'s profile: sandbox `gone` isn't in the config in /cfg any more");
        let mut rootless = state(None, "web");
        let Refused::Invalid(m) = refused(&mut rootless, "a", "work", &mut config()) else { panic!() };
        assert!(m.contains("no recorded config root"), "{m}");
        let mut unreadable = state(Some("/nonexistent/devsandbox-profile-test"), "web");
        let Refused::Invalid(m) = refused(&mut unreadable, "a", "work", &mut None) else { panic!() };
        assert!(m.contains("can't read the config in /nonexistent/devsandbox-profile-test"), "{m}");
        for s in [&s, &bare, &gone, &rootless, &unreadable] {
            assert_eq!(s.instances["a"].profile.as_deref(), Some("work"));
            assert!(s.last_profile.is_empty());
        }
    }

    #[test]
    fn in_flight_is_refused() {
        let mut s = state(Some("/cfg"), "web");
        let me = std::process::id();
        s.instances.get_mut("a").unwrap().progress =
            Some(toml::from_str(&format!("phase = \"building\"\npid = {me}\n")).unwrap());
        let e = set_in_state(&mut s, "a", "personal", &mut config()).unwrap_err().downcast::<Refused>().unwrap();
        assert!(matches!(e, Refused::Invalid(m) if m.contains("still building")));
    }

    #[test]
    fn show_lines_read_as_the_switch() {
        assert_eq!(profile_line("web-1", Some("work"), false), "profile web-1: work");
        let two = Ok(vec!["work".to_string(), "personal".to_string()]);
        assert_eq!(show_lines("web-1", Some("work"), false, Some(&two)), ["profile web-1: work", "allowed: work, personal"]);
        assert_eq!(
            show_lines("web-1", Some("gone"), true, None),
            ["profile web-1: gone (its sandbox doesn't allow it any more)"]
        );
        assert_eq!(
            show_lines("web-1", None, false, Some(&Ok(Vec::new()))),
            ["profile web-1: (none)", "allowed: (none: its sandbox has no profiles)"]
        );
        assert_eq!(show_lines("web-1", None, false, Some(&Err("why".into())))[1], "allowed: unknown (why)");
    }
}
