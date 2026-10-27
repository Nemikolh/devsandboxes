//! The browser wrapper: `devsbd` invoked as `chrome`, `chromium`, … (the
//! symlinks the host's install makes in `/run/devsandbox/bin`). Chromium
//! refuses its sandbox at euid 0, and on a root plan every exec runs as
//! container root (docs/architecture/instances/identity.md, *Browser
//! wrapper*). So at euid 0 the wrapper steps into a single-uid user
//! namespace where it is [`userns::WRAPPED_ID`], with container root
//! underneath (files still come out root's), and execs the real browser
//! there; Chrome nests its own namespaces for the renderers. At any other
//! euid it's a plain exec. Fds are inherited as they are: Playwright drives
//! Chrome over `--remote-debugging-pipe`, fds 3 and 4.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::userns;

/// The names the wrapper answers to, in the order the real binary is looked
/// for after the invoked one.
pub const NAMES: [&str; 5] = ["chrome", "chromium", "chromium-browser", "google-chrome", "google-chrome-stable"];

/// Where the wrapper's own links live; never searched (they'd loop).
const OWN_DIR: &str = "/run/devsandbox/bin";

/// The wrapper name `argv0`'s basename is, if any.
pub fn invoked_as(argv0: &OsStr) -> Option<&'static str> {
    let base = Path::new(argv0).file_name()?;
    NAMES.into_iter().find(|n| OsStr::new(n) == base)
}

/// What the search reads from the environment.
#[derive(Debug, Default)]
struct Search<'a> {
    invoked: &'a str,
    path: Option<&'a str>,
    home: Option<&'a str>,
    /// `PLAYWRIGHT_BROWSERS_PATH`: Playwright's download root when set
    /// (`0` means inside `node_modules`, which can't be found from here).
    playwright: Option<&'a str>,
    /// `PUPPETEER_CACHE_DIR`: Puppeteer's, likewise.
    puppeteer: Option<&'a str>,
}

/// Where the real browser may be, best first: the invoked name, then the
/// other [`NAMES`], on `PATH` minus [`OWN_DIR`]; then the newest
/// `chromium-*/chrome-linux*/chrome` of Playwright's downloads; then the
/// newest `chrome/*/chrome-linux64/chrome` of Puppeteer's. `list` names a
/// directory's entries (empty when it can't be read).
fn candidates(s: &Search, list: &dyn Fn(&Path) -> Vec<String>) -> Vec<PathBuf> {
    let names = std::iter::once(s.invoked).chain(NAMES.into_iter().filter(|n| *n != s.invoked));
    let dirs: Vec<&str> = s
        .path
        .unwrap_or_default()
        .split(':')
        .filter(|d| d.starts_with('/') && d.trim_end_matches('/') != OWN_DIR)
        .collect();
    let mut out: Vec<PathBuf> = names.flat_map(|n| dirs.iter().map(move |d| Path::new(d).join(n))).collect();
    let cache = |sub: &str| s.home.map(|h| Path::new(h).join(".cache").join(sub));
    let playwright = match s.playwright {
        Some(p) if !p.is_empty() && p != "0" => Some(PathBuf::from(p)),
        _ => cache("ms-playwright"),
    };
    if let Some(root) = playwright {
        for rev in newest_first(list(&root), "chromium-") {
            let dir = root.join(rev);
            let mut builds: Vec<String> = list(&dir).into_iter().filter(|b| b.starts_with("chrome-linux")).collect();
            builds.sort();
            out.extend(builds.into_iter().map(|b| dir.join(b).join("chrome")));
        }
    }
    let puppeteer = match s.puppeteer {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => cache("puppeteer"),
    };
    if let Some(root) = puppeteer {
        let root = root.join("chrome");
        for build in newest_first(list(&root), "") {
            out.push(root.join(build).join("chrome-linux64").join("chrome"));
        }
    }
    out
}

/// `entries` starting with `prefix`, highest version first (the numbers in
/// the name compared in order: `chromium-1200` before `chromium-999`).
fn newest_first(entries: Vec<String>, prefix: &str) -> Vec<String> {
    let mut picked: Vec<String> = entries.into_iter().filter(|e| e.starts_with(prefix)).collect();
    picked.sort_by_cached_key(|e| std::cmp::Reverse(version_key(e)));
    picked
}

fn version_key(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse().ok()).collect()
}

/// The browser's argv: its own path as `argv[0]`, the caller's args, and
/// `--no-sandbox` when no namespace could be made at euid 0.
fn exec_argv(real: &Path, args: Vec<OsString>, no_sandbox: bool) -> Vec<OsString> {
    let mut argv = vec![real.as_os_str().to_owned()];
    argv.extend(args);
    if no_sandbox {
        argv.push("--no-sandbox".into());
    }
    argv
}

/// Find the real browser and exec it (see the module doc); returns only on
/// failure: 127 when none is found, 126 when the exec fails.
pub fn run(invoked: &str, args: Vec<OsString>) -> i32 {
    imp::run(invoked, args)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Search, candidates, exec_argv, userns};
    use std::ffi::{CString, OsString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    mod sys {
        use std::ffi::{c_char, c_int};
        unsafe extern "C" {
            pub fn geteuid() -> u32;
            pub fn execv(path: *const c_char, argv: *const *const c_char) -> c_int;
        }
    }

    pub fn run(invoked: &str, args: Vec<OsString>) -> i32 {
        let Some(real) = find(invoked) else {
            eprintln!(
                "devsbd: {invoked}: no browser found (looked for {} on PATH outside {}, then Playwright's and Puppeteer's downloads)",
                super::NAMES.join("/"),
                super::OWN_DIR
            );
            return 127;
        };
        // SAFETY: plain syscall wrapper.
        let no_sandbox = unsafe { sys::geteuid() } == 0
            && match userns::enter_userns(userns::WRAPPED_ID, userns::WRAPPED_ID) {
                Ok(()) => false,
                Err(f) => {
                    eprintln!(
                        "devsbd: {invoked}: no user namespace for the browser ({}); starting {} with --no-sandbox",
                        userns::describe(f.step, f.errno),
                        real.display()
                    );
                    true
                }
            };
        let err = exec(&real, exec_argv(&real, args, no_sandbox));
        eprintln!("devsbd: {invoked}: exec {}: {err}", real.display());
        126
    }

    /// The first of [`candidates`] that is an executable file and not this
    /// binary under another name (a link back to the wrapper would loop).
    fn find(invoked: &str) -> Option<PathBuf> {
        let var = |k: &str| std::env::var(k).ok();
        let (path, home, playwright, puppeteer) =
            (var("PATH"), var("HOME"), var("PLAYWRIGHT_BROWSERS_PATH"), var("PUPPETEER_CACHE_DIR"));
        let search = Search {
            invoked,
            path: path.as_deref(),
            home: home.as_deref(),
            playwright: playwright.as_deref(),
            puppeteer: puppeteer.as_deref(),
        };
        let list = |dir: &Path| -> Vec<String> {
            let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
            entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect()
        };
        let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
        candidates(&search, &list).into_iter().find(|p| {
            let executable = std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            executable && (me.is_none() || p.canonicalize().ok() != me)
        })
    }

    /// `execv`, fds untouched; returns only the error.
    fn exec(path: &Path, argv: Vec<OsString>) -> std::io::Error {
        let c = |s: &[u8]| CString::new(s).map_err(|_| std::io::Error::from_raw_os_error(22));
        let path = match c(path.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let argv: Vec<CString> = match argv.iter().map(|a| c(a.as_bytes())).collect() {
            Ok(v) => v,
            Err(e) => return e,
        };
        let mut ptrs: Vec<*const c_char> = argv.iter().map(|a| a.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        // SAFETY: `path` and every `argv` entry are NUL-terminated and
        // outlive the call; `ptrs` is NULL-terminated.
        let _: c_int = unsafe { sys::execv(path.as_ptr(), ptrs.as_ptr()) };
        std::io::Error::last_os_error()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::ffi::OsString;

    pub fn run(invoked: &str, _args: Vec<OsString>) -> i32 {
        eprintln!("devsbd: {invoked}: Linux only");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn invoked_as_matches_the_basename() {
        assert_eq!(invoked_as(OsStr::new("/run/devsandbox/bin/chrome")), Some("chrome"));
        assert_eq!(invoked_as(OsStr::new("google-chrome-stable")), Some("google-chrome-stable"));
        assert_eq!(invoked_as(OsStr::new("/usr/bin/chromium-browser")), Some("chromium-browser"));
        assert_eq!(invoked_as(OsStr::new("/run/devsandbox/bin/devsbd")), None);
        assert_eq!(invoked_as(OsStr::new("chrome-wrapper")), None);
        assert_eq!(invoked_as(OsStr::new("")), None);
    }

    fn paths(v: &[PathBuf]) -> Vec<&str> {
        v.iter().map(|p| p.to_str().unwrap()).collect()
    }

    #[test]
    fn path_search_puts_the_invoked_name_first_and_skips_own_dir() {
        let none = |_: &Path| Vec::new();
        let s = Search {
            invoked: "chromium",
            path: Some("/run/devsandbox/bin:/usr/local/bin::rel/bin:/run/devsandbox/bin/:/usr/bin"),
            ..Default::default()
        };
        let got = candidates(&s, &none);
        assert!(got.iter().all(|p| !p.starts_with(OWN_DIR)), "{got:?}");
        assert_eq!(
            paths(&got[..4]),
            ["/usr/local/bin/chromium", "/usr/bin/chromium", "/usr/local/bin/chrome", "/usr/bin/chrome"]
        );
        assert_eq!(got.len(), NAMES.len() * 2);
        // No PATH, no HOME: nothing to look at.
        assert!(candidates(&Search { invoked: "chrome", ..Default::default() }, &none).is_empty());
    }

    #[test]
    fn downloads_newest_first() {
        let tree = BTreeMap::from([
            ("/root/.cache/ms-playwright", vec!["chromium-999", "chromium_headless_shell-1200", "chromium-1200", "ffmpeg-1010"]),
            ("/root/.cache/ms-playwright/chromium-1200", vec!["chrome-linux64", "INSTALLATION_COMPLETE"]),
            ("/root/.cache/ms-playwright/chromium-999", vec!["chrome-linux"]),
            ("/root/.cache/puppeteer/chrome", vec!["linux-131.0.6778.85", "linux-140.0.7339.80", "linux-99.1.2.3"]),
        ]);
        let list = |dir: &Path| -> Vec<String> {
            tree.get(dir.to_str().unwrap()).map(|v| v.iter().map(|s| s.to_string()).collect()).unwrap_or_default()
        };
        let s = Search { invoked: "chrome", home: Some("/root"), ..Default::default() };
        assert_eq!(
            paths(&candidates(&s, &list)),
            [
                "/root/.cache/ms-playwright/chromium-1200/chrome-linux64/chrome",
                "/root/.cache/ms-playwright/chromium-999/chrome-linux/chrome",
                "/root/.cache/puppeteer/chrome/linux-140.0.7339.80/chrome-linux64/chrome",
                "/root/.cache/puppeteer/chrome/linux-131.0.6778.85/chrome-linux64/chrome",
                "/root/.cache/puppeteer/chrome/linux-99.1.2.3/chrome-linux64/chrome",
            ]
        );
        // The tools' own location variables replace the home defaults; `0`
        // (inside node_modules) falls back.
        let tree = BTreeMap::from([
            ("/ms-playwright", vec!["chromium-1"]),
            ("/ms-playwright/chromium-1", vec!["chrome-linux"]),
            ("/pp/chrome", vec!["linux-1"]),
        ]);
        let list = |dir: &Path| -> Vec<String> {
            tree.get(dir.to_str().unwrap()).map(|v| v.iter().map(|s| s.to_string()).collect()).unwrap_or_default()
        };
        let s = Search { invoked: "chrome", home: Some("/root"), playwright: Some("/ms-playwright"), puppeteer: Some("/pp"), ..Default::default() };
        assert_eq!(
            paths(&candidates(&s, &list)),
            ["/ms-playwright/chromium-1/chrome-linux/chrome", "/pp/chrome/linux-1/chrome-linux64/chrome"]
        );
        let s = Search { invoked: "chrome", playwright: Some("0"), ..Default::default() };
        assert!(candidates(&s, &list).is_empty());
    }

    #[test]
    fn exec_argv_appends_no_sandbox_only_on_fallback() {
        let real = Path::new("/usr/bin/chromium");
        let args = vec![OsString::from("--headless"), OsString::from("--remote-debugging-pipe")];
        assert_eq!(exec_argv(real, args.clone(), false), ["/usr/bin/chromium", "--headless", "--remote-debugging-pipe"]);
        assert_eq!(
            exec_argv(real, args, true),
            ["/usr/bin/chromium", "--headless", "--remote-debugging-pipe", "--no-sandbox"]
        );
    }
}
