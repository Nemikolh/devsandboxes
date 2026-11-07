//! Opening a container's URL on this host (docs/architecture/helper/bridges.md,
//! "Opening URLs (`kind open`)"): the policy an `open` record passes before
//! the bridge's sink hands it to the opener, and the system opener itself.
//! The policy is pure (clocks and the limiter passed in); only
//! [`open_on_desktop`] touches the environment and spawns.

use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::time::Duration;

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
/// `instance` with the system opener and the [`display_env`], all stdio
/// null. Runs on its own thread (which also reaps the child), so the
/// bridge's handler never waits on it, like `desktop::notify_desktop`. No
/// display, no known opener or a failed spawn is one `log` line naming the
/// instance and the URL.
pub fn open_on_desktop(instance: &str, url: &str, log: fn(&str)) {
    let (instance, url) = (instance.to_string(), url.to_string());
    std::thread::spawn(move || {
        let os = std::env::consts::OS;
        let Some(env) = display_env(os) else {
            return log(&format!("{instance}: no display to open {url} on"));
        };
        let Some(argv) = opener_argv(os, &url) else {
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
