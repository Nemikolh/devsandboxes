//! `devsbd boot`: the container command's start hook (docs/automations.md).
//! The runtime restarting a container brings back only its command, not what
//! the host ran via `exec`; this puts back the daemon and, when the host left
//! a boot file (`bootfile::PATH`), the `postStartCommand` it records.
//!
//! Runs in the background of the container command (`sh -c '… boot & exec
//! sleep infinity'`), as the container's user. The daemon is started as a
//! detached child so it outlives this process; the commands then run in
//! order, stopping at the first failure like the host's lifecycle chain, with
//! their output appended to `bootfile::LOG`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::bootfile::{self, BootSpec};

pub fn run() -> io::Result<()> {
    // Idempotent: a daemon of this build already holding the pidfile makes
    // this one exit at once (the host's `start` races us harmlessly).
    let exe = std::env::current_exe()?;
    let _ = Command::new(&exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();

    let text = match std::fs::read_to_string(bootfile::PATH) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut log = Log::open();
    log.line(&format!("=== {} devsbd boot ===", utc_now()));
    let spec = bootfile::parse(&text).map_err(|e| log.fail(format!("{}: {e}", bootfile::PATH)))?;
    run_spec(&spec, &mut log)
}

fn run_spec(spec: &BootSpec, log: &mut Log) -> io::Result<()> {
    let root = std::fs::metadata("/proc/self").map(|m| m.uid() == 0).unwrap_or(false);
    let account = match (&spec.user, root) {
        (Some(user), true) => {
            let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
            let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
            Some(resolve_user(&passwd, &group, user).ok_or_else(|| log.fail(format!("unknown user `{user}`")))?)
        }
        (Some(user), false) => {
            log.line(&format!("not root: running as the container user, not `{user}`"));
            None
        }
        (None, _) => None,
    };
    for argv in &spec.cmds {
        let Some((program, rest)) = argv.split_first() else { continue };
        let mut cmd = Command::new(program);
        cmd.args(rest).stdin(Stdio::null());
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        if let Some(a) = &account {
            // std drops supplementary groups when switching away from root.
            cmd.uid(a.uid).gid(a.gid).env("HOME", &a.home);
            if let Some(name) = &a.name {
                cmd.env("USER", name);
            }
        }
        cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
        if let Some(f) = &log.file {
            cmd.stdout(f.try_clone()?).stderr(f.try_clone()?);
        }
        let shown = argv.join(" ");
        log.line(&format!("$ {shown}"));
        let status = cmd.status().map_err(|e| log.fail(format!("cannot run `{program}`: {e}")))?;
        if !status.success() {
            let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
            return Err(log.fail(format!("`{shown}` exited with status {code}")));
        }
    }
    Ok(())
}

/// The boot log, or stderr (the container's log) when it can't be opened.
struct Log {
    file: Option<File>,
}

impl Log {
    fn open() -> Log {
        Log { file: OpenOptions::new().create(true).append(true).open(bootfile::LOG).ok() }
    }

    fn line(&mut self, text: &str) {
        match &mut self.file {
            Some(f) => {
                let _ = writeln!(f, "{text}");
            }
            None => eprintln!("devsbd boot: {text}"),
        }
    }

    /// Log `msg` and hand it back as the error `main` exits with.
    fn fail(&mut self, msg: String) -> io::Error {
        self.line(&msg);
        io::Error::other(msg)
    }
}

/// Who to run as: what `docker exec -u` would pick for the same spec.
#[derive(Debug, PartialEq, Eq)]
struct Account {
    name: Option<String>,
    uid: u32,
    gid: u32,
    home: String,
}

/// Resolve `name|uid[:group|gid]` against `/etc/passwd` and `/etc/group`
/// contents. A numeric uid missing from passwd is still valid (gid 0, home
/// `/`, as docker does); an unknown name or group is `None`.
fn resolve_user(passwd: &str, group: &str, spec: &str) -> Option<Account> {
    let (user, grp) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let entry = passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 || (f[0] != user && f[2] != user) {
            return None;
        }
        Some(Account {
            name: Some(f[0].to_string()),
            uid: f[2].parse().ok()?,
            gid: f[3].parse().ok()?,
            home: f[5].to_string(),
        })
    });
    let mut account = match entry {
        Some(a) => a,
        None => Account { name: None, uid: user.parse().ok()?, gid: 0, home: "/".into() },
    };
    if let Some(g) = grp {
        account.gid = match g.parse() {
            Ok(gid) => gid,
            Err(_) => group.lines().find_map(|line| {
                let f: Vec<&str> = line.split(':').collect();
                (f.len() >= 3 && f[0] == g).then(|| f[2].parse().ok()).flatten()
            })?,
        };
    }
    Some(account)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for the log header, without a date dependency.
fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

fn format_utc(secs: u64) -> String {
    let (days, rem) = (secs / 86400, secs % 86400);
    // Howard Hinnant's civil_from_days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\n\
vscode:x:1000:1000:,,,:/home/vscode:/bin/bash\n\
broken:x:notanumber:1:::\n";
    const GROUP: &str = "root:x:0:\ndocker:x:998:vscode\n";

    fn account(name: Option<&str>, uid: u32, gid: u32, home: &str) -> Account {
        Account { name: name.map(String::from), uid, gid, home: home.into() }
    }

    #[test]
    fn resolves_by_name_and_uid() {
        let want = account(Some("vscode"), 1000, 1000, "/home/vscode");
        assert_eq!(resolve_user(PASSWD, GROUP, "vscode"), Some(want));
        let want = account(Some("vscode"), 1000, 1000, "/home/vscode");
        assert_eq!(resolve_user(PASSWD, GROUP, "1000"), Some(want));
        assert_eq!(resolve_user(PASSWD, GROUP, "root"), Some(account(Some("root"), 0, 0, "/root")));
    }

    #[test]
    fn group_override_numeric_or_named() {
        assert_eq!(resolve_user(PASSWD, GROUP, "vscode:5").map(|a| a.gid), Some(5));
        assert_eq!(resolve_user(PASSWD, GROUP, "vscode:docker").map(|a| a.gid), Some(998));
        assert_eq!(resolve_user(PASSWD, GROUP, "vscode:nogroup"), None);
    }

    #[test]
    fn unknown_users() {
        assert_eq!(resolve_user(PASSWD, GROUP, "nobody"), None);
        // Numeric uids need no passwd entry (docker's rule).
        assert_eq!(resolve_user(PASSWD, GROUP, "4242"), Some(account(None, 4242, 0, "/")));
        // A malformed entry doesn't match by name.
        assert_eq!(resolve_user(PASSWD, GROUP, "broken"), None);
    }

    #[test]
    fn formats_utc() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_790_000_000), "2026-09-21T14:13:20Z");
    }

    /// The runner end to end on the host (no user switch): env, cwd,
    /// stop-at-first-failure, output in the log.
    #[test]
    fn runs_commands_in_order_and_stops_on_failure() {
        let dir = std::env::temp_dir().join(format!("devsbd-boot-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("boot.log");
        let mut log = Log { file: Some(File::create(&log_path).unwrap()) };
        let spec = BootSpec {
            user: None,
            cwd: Some(dir.to_string_lossy().into_owned()),
            env: vec![("GREETING".into(), "hi there".into())],
            cmds: vec![
                vec!["sh".into(), "-c".into(), "echo \"$GREETING\" > out; pwd".into()],
                vec![],
                vec!["false".into()],
                vec!["touch".into(), "never".into()],
            ],
        };
        let err = run_spec(&spec, &mut log).unwrap_err();
        assert!(err.to_string().contains("`false` exited with status 1"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.join("out")).unwrap(), "hi there\n");
        assert!(!dir.join("never").exists());
        let logged = std::fs::read_to_string(&log_path).unwrap();
        assert!(logged.contains(&format!("$ sh -c echo \"$GREETING\" > out; pwd\n{}\n", dir.display())), "{logged}");
        assert!(logged.contains("$ false\n"), "{logged}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
