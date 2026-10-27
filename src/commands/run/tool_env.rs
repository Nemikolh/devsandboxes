//! The tool environment (docs/architecture/instances/identity.md, *Tool
//! environment*): whether Chromium's sandbox can work in the container, as
//! `devsbd userns check` finds it after create ([`check_argv`], [`check`],
//! [`browser_sandbox`]), and the env every exec then gets from it
//! ([`tool_env`]), recorded on the instance (`state::Instance::tool_env`).

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::runtime::backend;

/// The userns check's verdict, recorded on the instance. Stored as
/// `browser_sandbox = "on"` or `browser_sandbox = { off = "<why>" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserSandbox {
    /// Every namespace Chromium's sandbox makes could be made.
    On,
    /// One couldn't: a readable why ([`browser_sandbox`]).
    Off(String),
}

/// The knobs that turn the browser sandbox off in each tool that has one
/// (the plan's *Tool environment* table). `PLAYWRIGHT_MCP_SANDBOX` and
/// `CHROMIUM_FLAGS` are unverified.
const SANDBOX_OFF_KNOBS: [(&str, &str); 5] = [
    ("PUPPETEER_DANGEROUS_NO_SANDBOX", "1"),
    ("ELECTRON_DISABLE_SANDBOX", "1"),
    ("QTWEBENGINE_DISABLE_SANDBOX", "1"),
    ("PLAYWRIGHT_MCP_SANDBOX", "false"),
    ("CHROMIUM_FLAGS", "--no-sandbox"),
];

/// Silencers for tools that only warn when run as root.
const ROOT_SILENCERS: [(&str, &str); 2] = [("PIP_ROOT_USER_ACTION", "ignore"), ("BUNDLE_SILENCE_ROOT_WARNING", "1")];

/// Set when the browser sandbox works for the exec user, so a script can
/// opt in where its tool defaults to off (Playwright's `chromiumSandbox`).
const SANDBOX_ON: (&str, &str) = ("DEVSANDBOX_BROWSER_SANDBOX", "1");

/// The exec of `devsbd userns check` in `container` as the plan's exec user
/// (`exec_user`; the container's own user when `None`), `--as-root` on a
/// root plan.
pub(super) fn check_argv(container: &str, exec_user: Option<&str>) -> Vec<String> {
    let mut args = vec!["exec".to_string()];
    if let Some(user) = exec_user {
        args.extend(["-u".into(), user.into()]);
    }
    args.extend([container, crate::devsbd::BIN, "userns", "check"].map(String::from));
    if root_plan(exec_user) {
        args.push("--as-root".into());
    }
    args
}

/// A plan whose execs run as root (rootless docker, no account to adopt).
pub(crate) fn root_plan(exec_user: Option<&str>) -> bool {
    exec_user == Some("root")
}

/// Run [`check_argv`]'s exec; `None` (unknown) when it couldn't run.
pub(super) fn check(argv: &[String]) -> Option<BrowserSandbox> {
    let out = Command::new(backend().bin()).args(argv).stdin(Stdio::null()).output().ok()?;
    browser_sandbox(out.status.code(), &String::from_utf8_lossy(&out.stderr))
}

/// `devsbd userns check`'s outcome (contract in
/// docs/architecture/helper/in-container.md): exit 0 is `On`; exit 1 is
/// `Off`, its `devsbd: userns: <step>: <errno>` line read as a why (EPERM
/// creating a namespace is seccomp; EPERM setting one up is the host's
/// AppArmor userns restriction), else the line as is. Anything else (2: a
/// helper too old to know `userns`; the exec's own failures; a signal) is
/// unknown.
pub(super) fn browser_sandbox(code: Option<i32>, stderr: &str) -> Option<BrowserSandbox> {
    match code? {
        0 => Some(BrowserSandbox::On),
        1 => {
            let line = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("devsbd userns check failed");
            let parsed = line.strip_prefix("devsbd: userns: ").and_then(|rest| rest.rsplit_once(": "));
            let why = match parsed {
                Some(("unshare(CLONE_NEWUSER)" | "clone(CLONE_NEWPID)", "EPERM")) => "no user namespaces: seccomp",
                Some(("uid_map" | "gid_map" | "setgroups", "EPERM")) => {
                    "no user namespaces: AppArmor restriction on the host"
                }
                Some(("euid", "root")) => "execs run as root",
                _ => line,
            };
            Some(BrowserSandbox::Off(why.into()))
        }
        _ => None,
    }
}

/// The env every exec gets from the check (`sandbox`, `None` = unknown) and
/// the plan: `DEVSANDBOX_BROWSER_SANDBOX=1` when the sandbox works for a
/// non-root exec user, else the sandbox-off knobs (a root plan's browser
/// runs at euid 0, where Chromium refuses its sandbox); the root silencers on
/// root plans. Keys the sandbox sets itself (`user_keys`: its
/// `containerEnv`/`remoteEnv`) are left out.
pub(super) fn tool_env(
    root_plan: bool,
    sandbox: Option<&BrowserSandbox>,
    user_keys: &BTreeSet<&str>,
) -> BTreeMap<String, String> {
    let mut env: Vec<(&str, &str)> = match (root_plan, sandbox) {
        (false, Some(BrowserSandbox::On)) => vec![SANDBOX_ON],
        _ => SANDBOX_OFF_KNOBS.to_vec(),
    };
    if root_plan {
        env.extend(ROOT_SILENCERS);
    }
    env.into_iter()
        .filter(|(k, _)| !user_keys.contains(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The line `run` and `inspect` say about the browser sandbox: on, or off
/// with why and that the knobs are set ([`tool_env`]'s rule).
pub(crate) fn summary(root_plan: bool, sandbox: Option<&BrowserSandbox>) -> String {
    let why = match (root_plan, sandbox) {
        (false, Some(BrowserSandbox::On)) => return "browser sandbox: on".into(),
        (_, Some(BrowserSandbox::Off(why))) => why.as_str(),
        (true, Some(BrowserSandbox::On)) => "execs run as root",
        (_, None) => "unknown: the userns check didn't run",
    };
    format!("browser sandbox: off ({why}); sandbox-off knobs set")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn off(why: &str) -> Option<BrowserSandbox> {
        Some(BrowserSandbox::Off(why.into()))
    }

    #[test]
    fn check_argv_per_plan() {
        let tail = ["c", crate::devsbd::BIN, "userns", "check"];
        assert_eq!(check_argv("c", Some("vscode"))[..3], ["exec", "-u", "vscode"]);
        assert_eq!(check_argv("c", Some("vscode"))[3..], tail);
        let root = check_argv("c", Some("root"));
        assert_eq!(root[3..7], tail);
        assert_eq!(root[7..], ["--as-root"]);
        assert_eq!(check_argv("c", None)[1..], tail);
    }

    #[test]
    fn browser_sandbox_reads_the_outcome() {
        assert_eq!(browser_sandbox(Some(0), ""), Some(BrowserSandbox::On));
        let seccomp = off("no user namespaces: seccomp");
        let apparmor = off("no user namespaces: AppArmor restriction on the host");
        for (line, want) in [
            ("devsbd: userns: unshare(CLONE_NEWUSER): EPERM\n", &seccomp),
            ("devsbd: userns: clone(CLONE_NEWPID): EPERM\n", &seccomp),
            ("devsbd: userns: euid: root\n", &off("execs run as root")),
            ("devsbd: userns: uid_map: EPERM\n", &apparmor),
            ("devsbd: userns: gid_map: EPERM\n", &apparmor),
            ("devsbd: userns: setgroups: EPERM\n", &apparmor),
        ] {
            assert_eq!(&browser_sandbox(Some(1), line), want, "{line}");
        }
        // Other errnos and steps, or no line: raw.
        assert_eq!(
            browser_sandbox(Some(1), "devsbd: userns: unshare(CLONE_NEWUSER): ENOSPC\n"),
            off("devsbd: userns: unshare(CLONE_NEWUSER): ENOSPC")
        );
        assert_eq!(browser_sandbox(Some(1), "devsbd: userns: fork: 11\n"), off("devsbd: userns: fork: 11"));
        assert_eq!(browser_sandbox(Some(1), "\n"), off("devsbd userns check failed"));
        // Unknown: an old helper's usage error, exec's own codes, a signal.
        for code in [Some(2), Some(126), Some(127), None] {
            assert_eq!(browser_sandbox(code, "usage: devsbd …"), None, "{code:?}");
        }
    }

    fn keys(env: &BTreeMap<String, String>) -> Vec<&str> {
        env.keys().map(String::as_str).collect()
    }

    const KNOBS: [&str; 5] = [
        "CHROMIUM_FLAGS",
        "ELECTRON_DISABLE_SANDBOX",
        "PLAYWRIGHT_MCP_SANDBOX",
        "PUPPETEER_DANGEROUS_NO_SANDBOX",
        "QTWEBENGINE_DISABLE_SANDBOX",
    ];

    #[test]
    fn tool_env_per_plan_and_verdict() {
        let none = BTreeSet::new();
        let on = Some(BrowserSandbox::On);
        let seccomp = off("no user namespaces: seccomp");
        // Non-root, on: the opt-in only.
        let env = tool_env(false, on.as_ref(), &none);
        assert_eq!(env, BTreeMap::from([("DEVSANDBOX_BROWSER_SANDBOX".into(), "1".into())]));
        // Non-root, off or unknown: the knobs.
        for sandbox in [seccomp.as_ref(), None] {
            let env = tool_env(false, sandbox, &none);
            assert_eq!(keys(&env), KNOBS, "{sandbox:?}");
            assert_eq!((env["PLAYWRIGHT_MCP_SANDBOX"].as_str(), env["CHROMIUM_FLAGS"].as_str()), ("false", "--no-sandbox"));
        }
        // Root, whatever the verdict: the knobs and the silencers.
        let mut root = KNOBS.to_vec();
        root.extend(["PIP_ROOT_USER_ACTION", "BUNDLE_SILENCE_ROOT_WARNING"]);
        root.sort();
        for sandbox in [on.as_ref(), seccomp.as_ref(), None] {
            let env = tool_env(true, sandbox, &none);
            assert_eq!(keys(&env), root, "{sandbox:?}");
            assert_eq!(env["PIP_ROOT_USER_ACTION"], "ignore");
        }
        // The sandbox's own keys win by absence.
        let user = BTreeSet::from(["CHROMIUM_FLAGS", "PIP_ROOT_USER_ACTION", "DEVSANDBOX_BROWSER_SANDBOX"]);
        let env = tool_env(true, None, &user);
        assert!(!env.contains_key("CHROMIUM_FLAGS") && !env.contains_key("PIP_ROOT_USER_ACTION"), "{env:?}");
        assert!(tool_env(false, on.as_ref(), &user).is_empty());
    }

    #[test]
    fn summary_lines() {
        assert_eq!(summary(false, Some(&BrowserSandbox::On)), "browser sandbox: on");
        assert_eq!(
            summary(false, off("no user namespaces: seccomp").as_ref()),
            "browser sandbox: off (no user namespaces: seccomp); sandbox-off knobs set"
        );
        assert_eq!(
            summary(true, Some(&BrowserSandbox::On)),
            "browser sandbox: off (execs run as root); sandbox-off knobs set"
        );
        assert_eq!(
            summary(false, None),
            "browser sandbox: off (unknown: the userns check didn't run); sandbox-off knobs set"
        );
    }

    #[test]
    fn serde_shape() {
        #[derive(Serialize, Deserialize)]
        struct T {
            browser_sandbox: BrowserSandbox,
        }
        let on = toml::to_string(&T { browser_sandbox: BrowserSandbox::On }).unwrap();
        assert_eq!(on.trim(), "browser_sandbox = \"on\"");
        let text = toml::to_string(&T { browser_sandbox: BrowserSandbox::Off("x y".into()) }).unwrap();
        let back: T = toml::from_str(&text).unwrap();
        assert_eq!(back.browser_sandbox, BrowserSandbox::Off("x y".into()));
        let inline: T = toml::from_str("browser_sandbox = { off = \"why\" }").unwrap();
        assert_eq!(inline.browser_sandbox, BrowserSandbox::Off("why".into()));
    }
}
