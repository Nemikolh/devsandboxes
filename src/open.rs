//! Opening a container's URL on this host (docs/architecture/helper/bridges.md,
//! "Opening URLs (`kind open`)"): the policy an `open` record passes before
//! the bridge's sink hands it to the opener, and the system opener itself.
//! The policy is pure (clocks and the limiter passed in), and so is the
//! choice of command ([`open_plan`] over this host's `profiles.toml`); only
//! [`open_on_desktop`] touches the environment, the files and spawns.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::state::{Instance, State};

/// An `open` record queued longer ago than this is dropped at delivery: the
/// outbox keeps records while no host is attached, and an OAuth URL opening
/// an hour later is worse than none. Measured from the record's `at`.
pub const OPEN_TTL: Duration = Duration::from_secs(30);

/// URLs one instance may open back to back before [`OPEN_RATE_REFILL`]
/// applies, so a looping script can't flood the desktop with tabs.
pub const OPEN_RATE_BURST: u32 = 5;

/// One more open per instance is allowed every this long.
pub const OPEN_RATE_REFILL: Duration = Duration::from_secs(10);

/// What happens to one `open` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Open,
    /// Not `http`/`https`; the scheme as given (empty when there is none).
    BadScheme(String),
    /// Queued `age` seconds ago, past [`OPEN_TTL`].
    Expired { age: u64 },
    /// The instance's rate limit is spent.
    Limited,
}

impl Verdict {
    /// The `serve.log` line for a refusal or drop, naming `instance` and
    /// `url`; `None` for [`Verdict::Open`].
    pub fn log_line(&self, instance: &str, url: &str) -> Option<String> {
        let why = match self {
            Verdict::Open => return None,
            Verdict::BadScheme(s) if s.is_empty() => "refused, not a URL with a scheme".to_string(),
            Verdict::BadScheme(s) => format!("refused, scheme `{s}` (http and https only)"),
            Verdict::Expired { age } => format!("dropped, queued {age}s ago (over {}s)", OPEN_TTL.as_secs()),
            Verdict::Limited => format!(
                "refused, rate limit ({OPEN_RATE_BURST} per {}s)",
                OPEN_RATE_REFILL.as_secs()
            ),
        };
        Some(format!("{instance}: open {why}: {url}"))
    }
}

/// Decide whether `url`, queued at `at` (unix seconds), opens at `now` (unix
/// seconds). Checked in order: the scheme (`http`/`https`, any case,
/// followed by `://`), then [`OPEN_TTL`], then `limit`, which is only asked
/// (and only spends a token) once the others pass. A record from the future
/// (clock skew between container and host) counts as fresh.
pub fn decide(url: &str, at: u64, now: u64, limit: impl FnOnce() -> bool) -> Verdict {
    let scheme = match url.split_once(':') {
        Some((s, _)) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) => s,
        _ => "",
    };
    let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    if !web || !url[scheme.len()..].starts_with("://") {
        return Verdict::BadScheme(scheme.to_string());
    }
    let age = now.saturating_sub(at);
    if age > OPEN_TTL.as_secs() {
        return Verdict::Expired { age };
    }
    if !limit() {
        return Verdict::Limited;
    }
    Verdict::Open
}

/// The system opener's argv for `url` on `os` (`std::env::consts::OS`
/// values), `None` where there is no known one. `url` is the last entry;
/// [`decide`] already held it to `http(s)://`, so it can't read as an option.
pub fn opener_argv(os: &str, url: &str) -> Option<Vec<String>> {
    match os {
        "linux" => Some(vec!["xdg-open".into(), url.into()]),
        "macos" => Some(vec!["open".into(), url.into()]),
        _ => None,
    }
}

/// What one profile does on this host: a `[<profile>]` table of
/// `profiles.toml` (docs/architecture/instances/profiles.md, *On the opening
/// host*).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProfile {
    /// The command that opens a URL in this profile (a browser and its
    /// profile flag); the URL is appended as the last argument.
    pub open: Vec<String>,
}

/// `profiles.toml`, by profile name. Commands only ever come from this
/// host's own file: only a profile's *name* comes from `state.toml` (and,
/// later, over a fleet link), so no config or machine can make this host run
/// a command.
pub type HostProfiles = BTreeMap<String, HostProfile>;

/// `profiles.toml`, next to `state.toml`.
pub fn profiles_path() -> Result<PathBuf> {
    Ok(State::path()?.with_file_name("profiles.toml"))
}

/// Parse `profiles.toml`'s contents: `[<profile>]` tables with a non-empty
/// `open` array of strings, nothing else. An empty file is no profiles.
pub fn parse_profiles(contents: &str) -> Result<HostProfiles> {
    let profiles: HostProfiles = toml::from_str(contents)?;
    if let Some(name) = profiles.iter().find(|(_, p)| p.open.is_empty()).map(|(n, _)| n) {
        bail!("[{name}]: `open` is empty (a command and its arguments)");
    }
    Ok(profiles)
}

/// The profiles in `path`; a missing file is none.
pub fn load_profiles(path: &Path) -> Result<HostProfiles> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_profiles(&contents).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HostProfiles::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The argv that opens `url` in `profile` on `os`: the profile's `open`
/// command from `profiles` with `url` appended when it maps one, else the
/// system opener ([`opener_argv`]); also for no profile.
pub fn open_argv(os: &str, profile: Option<&str>, profiles: &HostProfiles, url: &str) -> Option<Vec<String>> {
    match profile.and_then(|p| profiles.get(p)) {
        Some(p) => Some(p.open.iter().cloned().chain([url.to_string()]).collect()),
        None => opener_argv(os, url),
    }
}

/// How one URL opens: the argv ([`open_argv`]; `None`: no known opener on
/// `os`) and a `serve.log` warning to write first, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPlan {
    pub argv: Option<Vec<String>>,
    pub warning: Option<String>,
}

/// Plan opening `url` for `instance` (its `state.toml` entry `inst`, found
/// by id; `None` when it's gone): in the instance's profile, unless
/// `snapshot::profile_disallowed` says its sandbox no longer allows that one
/// (`allowed` is asked only then, it loads the config), in which case the
/// system opener and a warning (config drift is flagged, never followed).
/// No instance or no profile: the system opener.
pub fn open_plan(
    os: &str,
    instance: &str,
    url: &str,
    inst: Option<&Instance>,
    allowed: impl FnOnce(&Instance) -> Option<Vec<String>>,
    profiles: &HostProfiles,
) -> OpenPlan {
    let Some((inst, profile)) = inst.and_then(|i| Some((i, i.profile.as_deref()?))) else {
        return OpenPlan { argv: opener_argv(os, url), warning: None };
    };
    if crate::snapshot::profile_disallowed(Some(profile), allowed(inst).as_deref()) {
        return OpenPlan {
            argv: opener_argv(os, url),
            warning: Some(format!(
                "{instance}: profile `{profile}` no longer allowed by the config; opening {url} with the default opener"
            )),
        };
    }
    OpenPlan { argv: open_argv(os, Some(profile), profiles, url), warning: None }
}

/// The session variables a browser on this host needs: the display
/// (X11 or Wayland) and X11's auth cookie (GNOME's Xwayland puts it under
/// `$XDG_RUNTIME_DIR`, so `~/.Xauthority` isn't it).
const DISPLAY_ENV: [&str; 3] = ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"];

/// Whether this host can show a browser: on Linux, `DISPLAY` or
/// `WAYLAND_DISPLAY` set (non-empty), read through `env`. Other OSes always
/// say yes (macOS's `open` needs no env).
pub fn has_display(os: &str, env: impl Fn(&str) -> Option<OsString>) -> bool {
    os != "linux" || DISPLAY_ENV[..2].iter().any(|k| env(k).is_some_and(|v| !v.is_empty()))
}

/// [`DISPLAY_ENV`] out of `systemctl --user show-environment`'s output
/// (`KEY=value` lines). A shell-quoted value (systemd prints one with special
/// characters as `$'...'`) is skipped: display names and the auth path never
/// need quoting.
pub fn session_display_env(show_environment: &str) -> Vec<(String, String)> {
    show_environment
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, v)| DISPLAY_ENV.contains(k) && !v.is_empty() && !v.starts_with("$'"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The display env for the opener on `os`: `Some(empty)` when the daemon's
/// own env has a display (the child inherits it); on Linux otherwise the
/// user manager's current env, which the desktop session imports into at
/// login (GNOME and Plasma do; `systemctl --user import-environment` in
/// other sessions). A `serve install`ed daemon starts with `default.target`,
/// before the session does that, so its own env never has one; reading the
/// manager's at open time also follows a session that came (or went) since.
/// `None`: no display anywhere.
fn display_env(os: &str) -> Option<Vec<(String, String)>> {
    if has_display(os, |k| std::env::var_os(k)) {
        return Some(Vec::new());
    }
    if os != "linux" {
        return None;
    }
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let env = session_display_env(&String::from_utf8_lossy(&out.stdout));
    let lookup = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| OsString::from(v));
    has_display(os, lookup).then_some(env)
}

/// The production opener (`devsbd::bridge::Opener`): open `url` from
/// `instance` (`instance_id`, so a rename doesn't lose it) with the command
/// [`open_plan`] picks and the [`display_env`], all stdio null. `state.toml`
/// and `profiles.toml` are read on every open, so an edit applies without a
/// daemon restart; either one unreadable is logged and opening goes on with
/// the system opener. Runs on its own thread (which also reaps the child),
/// so the bridge's handler never waits on it, like
/// `desktop::notify_desktop`. No display, no known opener or a failed spawn
/// is one `log` line naming the instance and the URL.
pub fn open_on_desktop(instance: &str, instance_id: &str, url: &str, log: fn(&str)) {
    let (instance, instance_id, url) = (instance.to_string(), instance_id.to_string(), url.to_string());
    std::thread::spawn(move || {
        let os = std::env::consts::OS;
        let Some(env) = display_env(os) else {
            return log(&format!("{instance}: no display to open {url} on"));
        };
        let profiles = profiles_path().and_then(|p| load_profiles(&p)).unwrap_or_else(|e| {
            log(&format!("{instance}: {e:#}; opening {url} with the default opener"));
            HostProfiles::new()
        });
        let state = State::load().unwrap_or_else(|e| {
            log(&format!("{instance}: {e:#}; opening {url} with the default opener"));
            State::default()
        });
        let inst = state.instances.values().find(|i| i.instance_id == instance_id);
        let allowed = |i: &Instance| crate::snapshot::allowed_now(&mut BTreeMap::new(), i);
        let plan = open_plan(os, &instance, &url, inst, allowed, &profiles);
        if let Some(warning) = &plan.warning {
            log(warning);
        }
        let Some(argv) = plan.argv else {
            return log(&format!("{instance}: no opener on {os} for {url}"));
        };
        let spawned = Command::new(&argv[0])
            .args(&argv[1..])
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                let _ = child.wait();
            }
            Err(e) => log(&format!("{instance}: cannot run `{}` to open {url}: {e}", argv[0])),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::desktop::RateLimit;
    use std::time::Instant;

    const AT: u64 = 1_790_000_000;

    fn open_now(url: &str) -> Verdict {
        decide(url, AT, AT, || true)
    }

    #[test]
    fn decide_allows_http_and_https_only() {
        for url in ["https://example.com/a?b=c", "http://localhost:8080/", "HTTPS://EXAMPLE.COM"] {
            assert_eq!(open_now(url), Verdict::Open, "{url}");
        }
        let bad = |url: &str, scheme: &str| assert_eq!(open_now(url), Verdict::BadScheme(scheme.into()), "{url}");
        bad("ssh://host", "ssh");
        bad("file:///etc/passwd", "file");
        bad("javascript:alert(1)", "javascript");
        bad("mailto:a@b.c", "mailto");
        bad("vscode://file/x", "vscode");
        bad("https:example.com", "https");
        bad("/etc/passwd", "");
        bad("report.pdf", "");
        bad("", "");
        bad("-https://x", "-https");
    }

    #[test]
    fn decide_drops_records_past_the_ttl() {
        let ttl = OPEN_TTL.as_secs();
        assert_eq!(decide("https://x", AT, AT + ttl, || true), Verdict::Open, "exactly the TTL still opens");
        assert_eq!(decide("https://x", AT, AT + ttl + 1, || true), Verdict::Expired { age: ttl + 1 });
        assert_eq!(decide("https://x", AT + 5, AT, || true), Verdict::Open, "skewed into the future");
    }

    #[test]
    fn decide_spends_the_limit_only_on_otherwise_good_opens() {
        let mut rl = RateLimit::new(OPEN_RATE_BURST, OPEN_RATE_REFILL);
        let t0 = Instant::now();
        let mut try_open = |url: &str, at: u64, t: Instant| decide(url, at, AT, || rl.allow("web-1", None, t));
        // Refused and expired records don't spend tokens.
        for _ in 0..10 {
            assert_eq!(try_open("ssh://x", AT, t0), Verdict::BadScheme("ssh".into()));
            assert!(matches!(try_open("https://x", AT - 60, t0), Verdict::Expired { .. }));
        }
        for _ in 0..OPEN_RATE_BURST {
            assert_eq!(try_open("https://x", AT, t0), Verdict::Open);
        }
        assert_eq!(try_open("https://x", AT, t0), Verdict::Limited);
        assert_eq!(try_open("https://x", AT, t0 + OPEN_RATE_REFILL), Verdict::Open, "one refilled");
        // Another instance has its own bucket.
        assert!(rl.allow("web-2", None, t0));
    }

    #[test]
    fn refusals_log_the_instance_and_url() {
        assert_eq!(Verdict::Open.log_line("web-1", "https://x"), None);
        let line = |v: Verdict| v.log_line("web-1", "ssh://x").unwrap();
        assert_eq!(line(Verdict::BadScheme("ssh".into())), "web-1: open refused, scheme `ssh` (http and https only): ssh://x");
        assert_eq!(line(Verdict::BadScheme(String::new())), "web-1: open refused, not a URL with a scheme: ssh://x");
        assert_eq!(line(Verdict::Expired { age: 45 }), "web-1: open dropped, queued 45s ago (over 30s): ssh://x");
        assert_eq!(line(Verdict::Limited), "web-1: open refused, rate limit (5 per 10s): ssh://x");
    }

    #[test]
    fn opener_argv_per_os() {
        assert_eq!(opener_argv("linux", "https://x/?a=b c"), Some(vec!["xdg-open".into(), "https://x/?a=b c".into()]));
        assert_eq!(opener_argv("macos", "https://x"), Some(vec!["open".into(), "https://x".into()]));
        assert_eq!(opener_argv("windows", "https://x"), None);
    }

    #[test]
    fn profiles_toml_parses_open_commands() {
        let profiles = parse_profiles(
            "[work]\nopen = [\"google-chrome\", \"--profile-directory=Profile 2\"]\n\n[personal]\nopen = [\"firefox\"]\n",
        )
        .unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles["work"].open, vec!["google-chrome", "--profile-directory=Profile 2"]);
        assert_eq!(profiles["personal"].open, vec!["firefox"]);
        assert!(parse_profiles("").unwrap().is_empty(), "an empty file is no profiles");
        assert!(parse_profiles("# nothing yet\n").unwrap().is_empty());
    }

    #[test]
    fn profiles_toml_refuses_bad_entries() {
        let err = |s: &str| format!("{:#}", parse_profiles(s).unwrap_err());
        assert!(err("[work]\nopen = []\n").contains("[work]: `open` is empty"));
        err("[work]\nopen = [\"chrome\", 2]\n");
        err("[work]\nopen = \"chrome\"\n");
        err("[work]\n");
        assert!(err("[work]\nopen = [\"chrome\"]\nicon = \"x\"\n").contains("icon"), "unknown keys refused");
        err("open = [\"chrome\"]\n");
    }

    #[test]
    fn a_missing_profiles_toml_is_none() {
        let dir = std::env::temp_dir().join(format!("devsandbox-open-test-{}", std::process::id()));
        assert!(load_profiles(&dir.join("profiles.toml")).unwrap().is_empty());
    }

    fn profiles() -> HostProfiles {
        parse_profiles("[work]\nopen = [\"chrome\", \"--profile-directory=Work\"]\n").unwrap()
    }

    #[test]
    fn open_argv_uses_a_mapped_profile_else_the_system_opener() {
        let url = "https://x/?a=b";
        let argv = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(open_argv("linux", Some("work"), &profiles(), url), argv(&["chrome", "--profile-directory=Work", url]));
        assert_eq!(open_argv("macos", Some("work"), &profiles(), url), argv(&["chrome", "--profile-directory=Work", url]));
        assert_eq!(open_argv("linux", Some("personal"), &profiles(), url), argv(&["xdg-open", url]), "unmapped");
        assert_eq!(open_argv("macos", None, &profiles(), url), argv(&["open", url]), "no profile");
        assert_eq!(open_argv("linux", None, &HostProfiles::new(), url), argv(&["xdg-open", url]));
    }

    /// One state entry, as `state.toml` holds it, with `profile`.
    fn instance(profile: Option<&str>) -> Instance {
        let profile = profile.map_or(String::new(), |p| format!("profile = \"{p}\"\n"));
        let state: State = toml::from_str(&format!(
            "[instance.web-1]\nsandbox = \"web\"\ninstance_id = \"web-1\"\ncontainer = \"devsandbox-web-1\"\n\
             folder = \"/src/web\"\nworkspace = \"/workspaces/web\"\ncreated_unix = 1\n{profile}"
        ))
        .unwrap();
        state.instances.into_values().next().unwrap()
    }

    #[test]
    fn open_plan_picks_the_instance_profile_command() {
        let url = "https://x";
        let allowed = |set: &'static [&'static str]| {
            move |_: &Instance| Some(set.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        let plan = |inst: Option<&Instance>, set: &'static [&'static str]| {
            open_plan("linux", "web-1", url, inst, allowed(set), &profiles())
        };
        let argv = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let default = OpenPlan { argv: argv(&["xdg-open", url]), warning: None };

        let work = instance(Some("work"));
        assert_eq!(
            plan(Some(&work), &["work", "personal"]),
            OpenPlan { argv: argv(&["chrome", "--profile-directory=Work", url]), warning: None },
            "mapped"
        );
        assert_eq!(plan(Some(&instance(Some("personal"))), &["work", "personal"]), default, "unmapped");
        assert_eq!(plan(Some(&instance(None)), &["work"]), default, "no profile");
        assert_eq!(plan(None, &["work"]), default, "no instance");
        assert_eq!(
            open_plan("linux", "web-1", url, Some(&work), |_| None, &profiles()).argv,
            argv(&["chrome", "--profile-directory=Work", url]),
            "an allowed set that can't be told doesn't flag"
        );
        assert_eq!(
            plan(Some(&work), &["personal"]),
            OpenPlan {
                argv: argv(&["xdg-open", url]),
                warning: Some(
                    "web-1: profile `work` no longer allowed by the config; opening https://x with the default opener"
                        .into()
                ),
            },
            "disallowed"
        );
        let unasked = |_: &Instance| -> Option<Vec<String>> { panic!("no profile: the config isn't loaded") };
        open_plan("linux", "web-1", url, Some(&instance(None)), unasked, &profiles());
    }

    #[test]
    fn display_comes_from_the_env_on_linux() {
        let env = |set: &'static [(&'static str, &'static str)]| {
            move |k: &str| set.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
        };
        assert!(!has_display("linux", env(&[])));
        assert!(!has_display("linux", env(&[("DISPLAY", "")])));
        assert!(has_display("linux", env(&[("DISPLAY", ":0")])));
        assert!(has_display("linux", env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        assert!(!has_display("linux", env(&[("XAUTHORITY", "/run/user/1000/a")])), "auth alone isn't a display");
        assert!(has_display("macos", env(&[])));
    }

    #[test]
    fn session_display_env_picks_the_display_vars() {
        let out = "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\n\
                   DISPLAY=:0\n\
                   HOME=/home/u\n\
                   WAYLAND_DISPLAY=wayland-0\n\
                   XAUTHORITY=/run/user/1000/.mutter-Xwaylandauth.AB12\n\
                   XDG_CURRENT_DESKTOP=GNOME\n";
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            session_display_env(out),
            vec![
                pair("DISPLAY", ":0"),
                pair("WAYLAND_DISPLAY", "wayland-0"),
                pair("XAUTHORITY", "/run/user/1000/.mutter-Xwaylandauth.AB12"),
            ]
        );
        // A headless manager: nothing; quoted and empty values are skipped.
        assert!(session_display_env("HOME=/home/u\nPATH=/usr/bin\n").is_empty());
        assert!(session_display_env("DISPLAY=\nXAUTHORITY=$'/tmp/a b'\n").is_empty());
    }
}
