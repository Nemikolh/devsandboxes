//! Desktop delivery of a container notification (docs/automations.md,
//! "`devsbd notify`"): `notify-send` on Linux, `osascript` on macOS. Best
//! effort and fully quiet: a missing notifier (no `notify-send`, headless
//! host) is skipped silently, and nothing inherits the TUI's terminal.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::bridge::Notification;
use super::notify::Level;

/// The notifier command for `n` on `os` (`std::env::consts::OS` values), or
/// `None` where there is no known notifier. Pure, so the quoting is testable.
///
/// Linux urgency: info and warn map to `normal`, error to `critical`. Not
/// `low` for info: GNOME (and others) don't pop up low-urgency notifications,
/// and info is `devsbd notify`'s default level.
pub fn desktop_argv(os: &str, n: &Notification) -> Option<Vec<String>> {
    let r = &n.record;
    let body = match &r.link {
        Some(link) => format!("{}\n{link}", r.msg),
        None => r.msg.clone(),
    };
    match os {
        "linux" => {
            let urgency = match r.level {
                Level::Info | Level::Warn => "normal",
                Level::Error => "critical",
            };
            Some(vec![
                "notify-send".into(),
                "--app-name=devsandbox".into(),
                "-u".into(),
                urgency.into(),
                // `--` so a message starting with `-` isn't taken as an option.
                "--".into(),
                format!("devsandbox: {}", n.instance),
                body,
            ])
        }
        "macos" => {
            let script = format!(
                "display notification {} with title \"devsandbox\" subtitle {}",
                applescript_string(&body),
                applescript_string(&n.instance)
            );
            Some(vec!["osascript".into(), "-e".into(), script])
        }
        _ => None,
    }
}

/// `s` as an AppleScript string literal: only `\` and `"` need escaping
/// (newlines are allowed inside a literal).
fn applescript_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '\\' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Show `n` as a desktop notification: spawn the notifier with all stdio null
/// and reap it on its own thread, so the caller never waits on it. Spawn
/// failure (missing binary) is ignored.
pub fn notify_desktop(n: &Notification) {
    let Some(argv) = desktop_argv(std::env::consts::OS, n) else { return };
    let spawned = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Popups one instance may show back to back before [`RATE_REFILL`] applies.
const RATE_BURST: u32 = 3;

/// One more popup per instance is allowed every this long.
const RATE_REFILL: Duration = Duration::from_secs(10);

/// A keyed notification repeating the `(instance, key)` of one shown this
/// recently isn't popped again (the inbox row is still updated).
const COALESCE_WINDOW: Duration = Duration::from_secs(60);

/// Which notifications become desktop popups: a token bucket per instance
/// ([`RATE_BURST`], refilled one per [`RATE_REFILL`]) plus coalescing of
/// repeated keys, so a container can't flood the desktop. The clock is passed
/// in, so it's testable; only popups are limited, never inbox delivery.
#[derive(Default)]
pub struct RateLimit {
    // instance → (tokens left, when the last token was credited).
    buckets: HashMap<String, (u32, Instant)>,
    // (instance, key) → when it last popped.
    shown: HashMap<(String, String), Instant>,
}

impl RateLimit {
    /// Whether `n` may pop up at `now`; a `true` consumes a token.
    pub fn allow(&mut self, n: &Notification, now: Instant) -> bool {
        self.shown.retain(|_, at| now.saturating_duration_since(*at) < COALESCE_WINDOW);
        let key = n.record.key.as_ref().map(|k| (n.instance.clone(), k.clone()));
        if key.as_ref().is_some_and(|k| self.shown.contains_key(k)) {
            return false;
        }
        let (tokens, since) = self.buckets.entry(n.instance.clone()).or_insert((RATE_BURST, now));
        let earned = (now.saturating_duration_since(*since).as_nanos() / RATE_REFILL.as_nanos()) as u32;
        if earned > 0 {
            *tokens = (*tokens).saturating_add(earned).min(RATE_BURST);
            *since += RATE_REFILL * earned;
        }
        if *tokens == 0 {
            return false;
        }
        *tokens -= 1;
        if let Some(k) = key {
            self.shown.insert(k, now);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::notify::Record;

    fn from(instance: &str, key: Option<&str>) -> Notification {
        Notification {
            instance: instance.into(),
            record: Record { level: Level::Info, key: key.map(Into::into), link: None, msg: "m".into(), at: 0 },
        }
    }

    #[test]
    fn rate_limit_bursts_then_refills_per_instance() {
        let mut rl = RateLimit::default();
        let t0 = Instant::now();
        let a = from("a", None);
        for _ in 0..RATE_BURST {
            assert!(rl.allow(&a, t0));
        }
        assert!(!rl.allow(&a, t0), "burst spent");
        // Another instance has its own bucket.
        assert!(rl.allow(&from("b", None), t0));
        assert!(!rl.allow(&a, t0 + RATE_REFILL - Duration::from_millis(1)));
        assert!(rl.allow(&a, t0 + RATE_REFILL), "one token refilled");
        assert!(!rl.allow(&a, t0 + RATE_REFILL));
        // A long quiet spell refills to the burst, not beyond.
        let later = t0 + RATE_REFILL * 100;
        for _ in 0..RATE_BURST {
            assert!(rl.allow(&a, later));
        }
        assert!(!rl.allow(&a, later));
    }

    #[test]
    fn rate_limit_coalesces_repeated_keys() {
        let mut rl = RateLimit::default();
        let t0 = Instant::now();
        assert!(rl.allow(&from("a", Some("pr-1")), t0));
        assert!(!rl.allow(&from("a", Some("pr-1")), t0 + Duration::from_secs(1)), "repeat coalesced");
        // Other key, other instance, unkeyed: not coalesced.
        assert!(rl.allow(&from("a", Some("pr-2")), t0 + Duration::from_secs(1)));
        assert!(rl.allow(&from("b", Some("pr-1")), t0 + Duration::from_secs(1)));
        assert!(rl.allow(&from("a", None), t0 + Duration::from_secs(1)));
        // Past the window the key pops again.
        assert!(rl.allow(&from("a", Some("pr-1")), t0 + COALESCE_WINDOW + Duration::from_secs(1)));
    }

    fn n(level: Level, msg: &str, link: Option<&str>) -> Notification {
        Notification {
            instance: "web-pr-1".into(),
            record: Record { level, key: None, link: link.map(Into::into), msg: msg.into(), at: 0 },
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn linux_uses_notify_send_with_urgency() {
        let argv = desktop_argv("linux", &n(Level::Info, "PR 1 needs you", None)).unwrap();
        assert_eq!(
            argv,
            strs(&["notify-send", "--app-name=devsandbox", "-u", "normal", "--", "devsandbox: web-pr-1", "PR 1 needs you"])
        );
        let warn = desktop_argv("linux", &n(Level::Warn, "m", None)).unwrap();
        assert_eq!(warn[3], "normal");
        let error = desktop_argv("linux", &n(Level::Error, "-m", Some("https://x/1"))).unwrap();
        assert_eq!(error[3], "critical");
        // The link goes on its own line; argv needs no shell quoting.
        assert_eq!(error.last().unwrap(), "-m\nhttps://x/1");
    }

    #[test]
    fn macos_uses_osascript_with_escaped_strings() {
        let argv = desktop_argv("macos", &n(Level::Warn, r#"say "hi" \ bye"#, Some("https://x/1"))).unwrap();
        assert_eq!(
            argv,
            strs(&[
                "osascript",
                "-e",
                "display notification \"say \\\"hi\\\" \\\\ bye\nhttps://x/1\" with title \"devsandbox\" subtitle \"web-pr-1\"",
            ])
        );
        assert_eq!(applescript_string(""), "\"\"");
        assert_eq!(applescript_string(r#"a"\"#), r#""a\"\\""#);
    }

    #[test]
    fn other_os_has_no_notifier() {
        assert_eq!(desktop_argv("windows", &n(Level::Info, "m", None)), None);
        assert_eq!(desktop_argv("freebsd", &n(Level::Info, "m", None)), None);
    }
}
