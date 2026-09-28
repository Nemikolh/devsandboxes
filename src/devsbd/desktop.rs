//! Desktop delivery of a container notification (docs/automations.md,
//! "`devsbd notify`"): `notify-send` on Linux, `osascript` on macOS. Best
//! effort and fully quiet: a missing notifier (no `notify-send`, headless
//! host) is skipped silently, and nothing inherits the TUI's terminal.

use std::process::{Command, Stdio};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::notify::Record;

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
