//! A fleet worker follows its hub's version (docs/architecture/daemon/fleet.md,
//! *Versions*; ADR 0042). Each end of the fleet link says its build in its
//! `Hello` ([`HELLO`]: semver, git hash, `published` or `dev`); the worker
//! compares the hub's with its own ([`decide`], pure) before it serves
//! anything. A published hub build that differs (a downgrade included) is
//! downloaded from the GitHub release and installed over this daemon's
//! binary, which then takes over by a forced handoff ([`update`] over
//! [`Steps`]); a dev build that differs is fleet step 2c's (the worker stops
//! offering, saying so).
//!
//! The steps are injectable ([`Steps`]): [`Release`] is production (curl,
//! tar, `machines::install::INSTALL` run locally, the forced hello), tests
//! fake every one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};

use crate::serve::client::{self, Hello, StartReason};
use crate::serve::endpoint;
use crate::serve::machines::install;
use crate::serve::proto::Version;

/// What this build says in the fleet link's `Hello`:
/// `<semver> <git hash> <published|dev>`, stamped by `build.rs`
/// (`DEVSANDBOX_BUILD_GIT`, `DEVSANDBOX_BUILD_KIND`).
pub const HELLO: &str =
    concat!(env!("CARGO_PKG_VERSION"), " ", env!("DEVSANDBOX_BUILD_GIT"), " ", env!("DEVSANDBOX_BUILD_KIND"));

/// The GitHub repository the releases are published in.
pub const REPO: &str = "Nemikolh/devsandboxes";

/// One build, as a link `Hello` says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub semver: String,
    /// The short git hash it was built from; `unknown` without one.
    pub git: String,
    /// The release workflow's build of a tagged version (its asset exists).
    pub published: bool,
}

impl Build {
    /// This binary's.
    pub fn current() -> Build {
        Build::parse(HELLO)
    }

    /// A `Hello` hash, `<semver> <git> <published|dev>`; a missing word is
    /// `unknown` / dev.
    pub fn parse(hello: &str) -> Build {
        let mut words = hello.split_whitespace();
        let semver = words.next().unwrap_or("").to_string();
        let git = words.next().unwrap_or("unknown").to_string();
        let published = words.next() == Some("published");
        Build { semver, git, published }
    }

    /// `0.7.0 (published, 1a2b3c4d5e6f)`, for messages.
    pub fn label(&self) -> String {
        format!("{} ({}, {})", self.semver, if self.published { "published" } else { "dev" }, self.git)
    }
}

/// What a worker does about its hub's build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The same build: lend.
    Same,
    /// The hub runs published `version`, this worker another build: fetch
    /// and install it, then hand off (an older version too).
    Download { version: String },
    /// The hub runs a dev build this worker isn't: its binary would have to
    /// come over the link (fleet step 2c).
    DevBuild,
}

/// The worker follows the hub's exact build (pure): semver, git hash and
/// `published` all equal is the same build.
pub fn decide(ours: &Build, hub: &Build) -> Decision {
    if ours == hub {
        Decision::Same
    } else if hub.published {
        Decision::Download { version: hub.semver.clone() }
    } else {
        Decision::DevBuild
    }
}

/// Why a worker stops offering to a hub running a dev build.
pub fn dev_build_reason(hub: &Build) -> String {
    format!("dev build: needs fleet step 2c (the hub runs {}, this worker {})", hub.label(), Build::current().label())
}

/// The release target of a host (`std::env::consts` `OS`, `ARCH`), as
/// `.github/workflows/release.yml` names its `tar.gz` assets; `None` where no
/// release is built (an Intel Mac) or the daemon doesn't run (Windows).
pub fn release_target(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// `https://github.com/<REPO>/releases/download/v<v>/devsandbox-<v>-<target>.tar.gz`.
/// `version` comes from the hub: only a semver's characters go in the URL.
pub fn asset_url(version: &str, target: &str) -> Result<String> {
    let ok = !version.is_empty() && version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b));
    if !ok {
        bail!("`{version}` isn't a version");
    }
    Ok(format!("https://github.com/{REPO}/releases/download/v{version}/devsandbox-{version}-{target}.tar.gz"))
}

/// The steps of an update, injectable.
pub trait Steps: Send + Sync {
    /// Download `url` and unpack it into `into` (an empty dir): the binary.
    fn fetch(&self, url: &str, into: &Path) -> Result<PathBuf>;
    /// The semver `binary --version` reports.
    fn version(&self, binary: &Path) -> Result<String>;
    /// Where this daemon's binary is (what to install over).
    fn dest(&self) -> Result<PathBuf>;
    /// Install `binary` at `dest` (atomically).
    fn install(&self, binary: &Path, dest: &Path) -> Result<()>;
    /// Make this daemon hand off to the binary now at `dest`, whatever its
    /// version.
    fn handoff(&self) -> Result<()>;
}

/// Install published `version` over this daemon's binary and hand off to
/// it: fetch the asset for `os`/`arch` into a scratch dir under `scratch`,
/// check that it reports `version`, install it at [`Steps::dest`], check the
/// installed binary reports `version`, then [`Steps::handoff`]. The scratch
/// dir is removed whatever happens.
pub fn update(steps: &dyn Steps, version: &str, os: &str, arch: &str, scratch: &Path) -> Result<()> {
    let target = release_target(os, arch).with_context(|| format!("no devsandbox release is built for {os}/{arch}"))?;
    let url = asset_url(version, target)?;
    let dir = scratch_dir(scratch)?;
    let installed = (|| {
        let binary = steps.fetch(&url, &dir).with_context(|| format!("cannot download {url}"))?;
        check_version(steps, &binary, version, "the downloaded binary")?;
        let dest = steps.dest()?;
        steps.install(&binary, &dest)?;
        check_version(steps, &dest, version, "the installed binary")
    })();
    let _ = std::fs::remove_dir_all(&dir);
    installed?;
    steps.handoff().context("cannot hand this daemon off to it")
}

fn check_version(steps: &dyn Steps, binary: &Path, want: &str, what: &str) -> Result<()> {
    let got = steps.version(binary).with_context(|| format!("{what} ({}) doesn't run", binary.display()))?;
    if got != want {
        bail!("{what} ({}) reports {got}, not {want}", binary.display());
    }
    Ok(())
}

/// A new, private dir under `parent`.
fn scratch_dir(parent: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = parent.join(format!("devsandbox-update-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new().mode(0o700).create(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

/// `curl` fetching `url` into `out`: HTTPS only (redirects included), TLS
/// 1.2 at least, an HTTP error a failure; a stalled transfer (<1 B/s for
/// 30 s) gives up.
pub fn curl_argv(url: &str, out: &Path) -> Vec<OsString> {
    let mut argv: Vec<OsString> = [
        "curl",
        "--fail",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--tlsv1.2",
        "--silent",
        "--show-error",
        "--connect-timeout",
        "15",
        "--speed-limit",
        "1",
        "--speed-time",
        "30",
        "-o",
    ]
    .map(OsString::from)
    .into();
    argv.push(out.into());
    argv.push(url.into());
    argv
}

/// The production [`Steps`]: the release asset from GitHub, installed over
/// `exe::devsandbox()`, then a forced hello to the daemon in `socket_dir`.
pub struct Release {
    pub socket_dir: PathBuf,
}

impl Steps for Release {
    fn fetch(&self, url: &str, into: &Path) -> Result<PathBuf> {
        let archive = into.join("devsandbox.tar.gz");
        run(&curl_argv(url, &archive))?;
        let unpack: Vec<OsString> = vec!["tar".into(), "-xzf".into(), archive.into(), "-C".into(), into.into(), "devsandbox".into()];
        run(&unpack)?;
        let binary = into.join("devsandbox");
        if !binary.is_file() {
            bail!("the archive has no devsandbox binary");
        }
        Ok(binary)
    }

    fn version(&self, binary: &Path) -> Result<String> {
        let out = run(&[binary.into(), "--version".into()])?;
        install::version_of(&out).with_context(|| format!("`--version` said {:?}", out.trim()))
    }

    fn dest(&self) -> Result<PathBuf> {
        let exe = crate::exe::devsandbox().context("cannot locate this devsandbox binary")?;
        if exe.to_string_lossy().ends_with(" (deleted)") {
            bail!("this daemon's binary {} is gone", exe.display());
        }
        Ok(exe)
    }

    fn install(&self, binary: &Path, dest: &Path) -> Result<()> {
        install::install_local(dest, binary).map(drop)
    }

    /// A forced hello on the daemon's own socket (`HelloParams::force`: this
    /// user, a local connection), then the successor's start as a newer
    /// client's handoff starts it (`install::lazy_start`: the service
    /// manager's unit when one is installed, else detached). The successor
    /// waits for this daemon's lock while it drains.
    fn handoff(&self) -> Result<()> {
        let mut stream = endpoint::connect(&self.socket_dir).context("cannot connect to this daemon")?;
        stream.set_read_timeout(Some(client::START_TIMEOUT)).context("cannot set a read timeout")?;
        match client::hello_with(&mut stream, &Version::current(), "fleet-update", true)? {
            Hello::Handoff => crate::serve::install::lazy_start(&self.socket_dir, StartReason::Handoff),
            Hello::Ready(_) => bail!("the daemon refused the forced handoff"),
            Hello::Closed => bail!("the daemon closed the connection before answering"),
        }
    }
}

/// Run `argv` to its end: its stdout, or an error with its stderr's last line.
fn run(argv: &[OsString]) -> Result<String> {
    let (program, args) = argv.split_first().context("empty argv")?;
    let name = program.to_string_lossy();
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {name}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        match stderr.lines().rev().find(|l| !l.trim().is_empty()) {
            Some(line) => bail!("{name}: {}", line.trim()),
            None => bail!("{name} exited with {}", out.status),
        }
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    fn build(semver: &str, git: &str, published: bool) -> Build {
        Build { semver: semver.into(), git: git.into(), published }
    }

    #[test]
    fn hellos_parse() {
        assert_eq!(Build::parse("0.7.0 1a2b3c published"), build("0.7.0", "1a2b3c", true));
        assert_eq!(Build::parse("0.7.0 1a2b3c dev"), build("0.7.0", "1a2b3c", false));
        assert_eq!(Build::parse("0.7.0"), build("0.7.0", "unknown", false));
        assert_eq!(Build::parse(""), build("", "unknown", false));
        let ours = Build::current();
        assert_eq!(ours.semver, env!("CARGO_PKG_VERSION"));
        assert_eq!(Build::parse(HELLO), ours);
        assert!(HELLO.len() <= u8::MAX as usize, "fits a `Hello` hash");
        assert_eq!(build("0.7.0", "1a2b", true).label(), "0.7.0 (published, 1a2b)");
    }

    #[test]
    fn the_worker_follows_its_hub_exactly() {
        let ours = build("0.8.0", "aaaa", true);
        assert_eq!(decide(&ours, &ours.clone()), Decision::Same);
        let newer = build("0.9.0", "bbbb", true);
        assert_eq!(decide(&ours, &newer), Decision::Download { version: "0.9.0".into() });
        let older = build("0.7.0", "cccc", true);
        assert_eq!(decide(&ours, &older), Decision::Download { version: "0.7.0".into() }, "a downgrade too");
        // A dev worker of a published hub takes the release, same semver or not.
        let dev = build("0.8.0", "dddd", false);
        assert_eq!(decide(&dev, &ours), Decision::Download { version: "0.8.0".into() });
        assert_eq!(decide(&dev, &dev.clone()), Decision::Same);
        assert_eq!(decide(&ours, &dev), Decision::DevBuild);
        assert_eq!(decide(&dev, &build("0.8.0", "eeee", false)), Decision::DevBuild);
        assert!(dev_build_reason(&dev).starts_with("dev build: needs fleet step 2c (the hub runs 0.8.0 (dev, dddd)"));
    }

    #[test]
    fn targets_follow_the_release_assets() {
        assert_eq!(release_target("linux", "x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(release_target("linux", "aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(release_target("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(release_target("macos", "x86_64"), None, "no Intel Mac release");
        assert_eq!(release_target("windows", "x86_64"), None);
        assert_eq!(release_target("linux", "arm"), None);
    }

    #[test]
    fn asset_urls() {
        assert_eq!(
            asset_url("0.7.0", "aarch64-apple-darwin").unwrap(),
            "https://github.com/Nemikolh/devsandboxes/releases/download/v0.7.0/devsandbox-0.7.0-aarch64-apple-darwin.tar.gz"
        );
        assert!(asset_url("0.7.0-rc.1", "x").is_ok());
        for bad in ["", "0.7.0/../../x", "0.7 0", "0.7.0?x"] {
            assert!(asset_url(bad, "x").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_curl_argv_is_https_only() {
        let argv = curl_argv("https://x/y", Path::new("/t/a.tar.gz"));
        let words: Vec<_> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        for want in [["--proto", "=https"], ["--proto-redir", "=https"]] {
            assert!(words.windows(2).any(|w| w == want), "{words:?}");
        }
        assert!(words.contains(&"--fail".into()) && words.contains(&"--tlsv1.2".into()) && words.contains(&"--location".into()));
        assert_eq!(words[words.len() - 3..], ["-o", "/t/a.tar.gz", "https://x/y"]);
    }

    /// Steps that touch nothing but a scratch dir: what each one was asked,
    /// what `version` reports per path, and a step to fail.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub calls: Mutex<Vec<String>>,
        /// `--version` of the fetched binary and of the installed one.
        pub fetched_version: String,
        pub installed_version: String,
        pub fail: Option<&'static str>,
    }

    impl Fake {
        pub(crate) fn reporting(version: &str) -> Fake {
            Fake { fetched_version: version.into(), installed_version: version.into(), ..Fake::default() }
        }

        fn step(&self, name: &str, detail: String) -> Result<()> {
            self.calls.lock().unwrap().push(format!("{name} {detail}"));
            if self.fail == Some(name) {
                bail!("{name} broke");
            }
            Ok(())
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Steps for Fake {
        fn fetch(&self, url: &str, into: &Path) -> Result<PathBuf> {
            self.step("fetch", url.into())?;
            let binary = into.join("devsandbox");
            std::fs::write(&binary, "new").unwrap();
            Ok(binary)
        }
        fn version(&self, binary: &Path) -> Result<String> {
            let installed = binary == Path::new("/dest/devsandbox");
            self.step("version", if installed { "dest".into() } else { "fetched".into() })?;
            Ok(if installed { self.installed_version.clone() } else { self.fetched_version.clone() })
        }
        fn dest(&self) -> Result<PathBuf> {
            Ok("/dest/devsandbox".into())
        }
        fn install(&self, binary: &Path, dest: &Path) -> Result<()> {
            assert!(binary.is_file(), "installs what was fetched");
            self.step("install", dest.display().to_string())
        }
        fn handoff(&self) -> Result<()> {
            self.step("handoff", String::new())
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn empty(dir: &Path) -> bool {
        std::fs::read_dir(dir).unwrap().next().is_none()
    }

    #[test]
    fn an_update_installs_checks_and_hands_off() {
        let dir = scratch("ok");
        let fake = Fake::reporting("0.7.0");
        update(&fake, "0.7.0", "linux", "x86_64", &dir).unwrap();
        let url = "https://github.com/Nemikolh/devsandboxes/releases/download/v0.7.0/devsandbox-0.7.0-x86_64-unknown-linux-musl.tar.gz";
        assert_eq!(
            fake.calls(),
            [format!("fetch {url}"), "version fetched".into(), "install /dest/devsandbox".into(), "version dest".into(), "handoff ".into()]
        );
        assert!(empty(&dir), "the scratch dir is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_download_installs_nothing() {
        let dir = scratch("fetch");
        let fake = Fake { fail: Some("fetch"), ..Fake::reporting("0.7.0") };
        let err = update(&fake, "0.7.0", "linux", "aarch64", &dir).unwrap_err();
        assert!(format!("{err:#}").starts_with("cannot download https://github.com/"), "{err:#}");
        assert!(format!("{err:#}").ends_with("fetch broke"), "{err:#}");
        assert_eq!(fake.calls().len(), 1);
        assert!(empty(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_version_is_a_failure_and_no_handoff() {
        let dir = scratch("version");
        // The asset isn't what it says: never installed.
        let fake = Fake { fetched_version: "0.6.9".into(), ..Fake::reporting("0.7.0") };
        let err = update(&fake, "0.7.0", "linux", "x86_64", &dir).unwrap_err();
        assert!(format!("{err:#}").contains("reports 0.6.9, not 0.7.0"), "{err:#}");
        assert!(!fake.calls().iter().any(|c| c.starts_with("install")));
        // The installed binary isn't (dest resolved elsewhere): no handoff.
        let fake = Fake { installed_version: "0.8.0".into(), ..Fake::reporting("0.7.0") };
        let err = update(&fake, "0.7.0", "linux", "x86_64", &dir).unwrap_err();
        assert!(format!("{err:#}").starts_with("the installed binary (/dest/devsandbox) reports 0.8.0, not 0.7.0"), "{err:#}");
        assert!(!fake.calls().iter().any(|c| c.starts_with("handoff")));
        // A failed handoff says so.
        let fake = Fake { fail: Some("handoff"), ..Fake::reporting("0.7.0") };
        let err = update(&fake, "0.7.0", "linux", "x86_64", &dir).unwrap_err();
        assert_eq!(format!("{err:#}"), "cannot hand this daemon off to it: handoff broke");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_release_no_fetch() {
        let dir = scratch("target");
        let fake = Fake::reporting("0.7.0");
        let err = update(&fake, "0.7.0", "macos", "x86_64", &dir).unwrap_err();
        assert_eq!(format!("{err:#}"), "no devsandbox release is built for macos/x86_64");
        assert!(fake.calls().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
