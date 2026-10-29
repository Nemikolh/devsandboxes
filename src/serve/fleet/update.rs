//! A fleet worker follows its hub's version (docs/architecture/daemon/fleet.md,
//! *Versions*; ADR 0042). Each end of the fleet link says its build in its
//! `Hello` ([`HELLO`]: semver, git hash, `published` or `dev`, platform);
//! the worker compares the hub's with its own ([`decide`], pure) before it
//! serves anything. A published hub build that differs (a downgrade
//! included) is downloaded from the GitHub release and installed over this
//! daemon's binary, which then takes over by a forced handoff ([`update`]
//! over [`Steps`]). A dev build that differs comes over the link instead:
//! the hub pushes its own binary ([`hub_pushes`], [`push`]) and a worker
//! that takes dev builds for its platform installs it the same way
//! ([`install_received`]); else it stops offering, saying why.
//!
//! The steps are injectable ([`Steps`]): [`Release`] is production (curl,
//! tar, `machines::install::INSTALL` run locally, the forced hello), tests
//! fake every one.

use std::ffi::OsString;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};

use super::transfer;
use crate::serve::client::{self, Hello, StartReason};
use crate::serve::endpoint;
use crate::serve::machines::install;
use crate::serve::proto::Version;

/// [`HELLO`] followed by its link's `<hub|member>` word, what a peer link
/// (`fleet-link --peer`, docs/architecture/daemon/fleet.md, *Peer links*)
/// says: [`hello_of`], read back by [`is_hub`]. The lending link says
/// [`HELLO`] alone (its worker follows the hub's build whatever).
macro_rules! hello {
    ($flag:literal) => {
        concat!(
            env!("CARGO_PKG_VERSION"),
            " ",
            env!("DEVSANDBOX_BUILD_GIT"),
            " ",
            env!("DEVSANDBOX_BUILD_KIND"),
            " ",
            env!("DEVSANDBOX_BUILD_PLATFORM"),
            $flag
        )
    };
}

/// What this build says in the fleet link's `Hello`:
/// `<semver> <git hash> <published|dev> <os>/<arch>`, stamped by `build.rs`
/// (`DEVSANDBOX_BUILD_GIT`, `DEVSANDBOX_BUILD_KIND`,
/// `DEVSANDBOX_BUILD_PLATFORM`).
pub const HELLO: &str = hello!("");

/// What this build says on a peer link: [`HELLO`] then `hub` when this
/// host is the fleet's hub (`[serve] hub`, `daemon_config::ServeConfig::hub`),
/// else `member`.
pub fn hello_of(hub: bool) -> &'static str {
    if hub { hello!(" hub") } else { hello!(" member") }
}

/// Whether a peer link's `Hello` hash ([`hello_of`]) says its end is the
/// hub: its fifth word is `hub`. A lending link's (four words) never is.
pub fn is_hub(hello: &str) -> bool {
    hello.split_whitespace().nth(4) == Some("hub")
}

/// The words of a `Hello` hash that are the build (and platform): the first
/// four, without a peer link's hub flag.
fn build_words(hello: &str) -> impl Iterator<Item = &str> {
    hello.split_whitespace().take(4)
}

/// This binary's platform, `<os>/<arch>` as `std::env::consts` names them.
pub const PLATFORM: &str = env!("DEVSANDBOX_BUILD_PLATFORM");

/// The transfer kind of a hub's binary, and the most a worker takes of it.
pub const BINARY_KIND: &str = "binary";
pub const BINARY_LIMIT: u64 = 256 << 20;

/// The platform word of a `Hello` hash ([`HELLO`]'s fourth); `unknown`
/// without one.
pub fn platform_of(hello: &str) -> &str {
    hello.split_whitespace().nth(3).unwrap_or("unknown")
}

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

    /// A `Hello` hash, `<semver> <git> <published|dev> …`; a missing word is
    /// `unknown` / dev. The platform isn't part of the build
    /// ([`platform_of`]): one release is built for several.
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
    /// The hub runs a dev build this worker isn't, for this platform, and
    /// this worker takes dev builds: take the binary the hub pushes, install
    /// it, then hand off.
    Receive,
    /// The hub runs a dev build this worker can't or won't take: stop
    /// offering, with why.
    Stop(String),
}

/// The worker follows the hub's exact build (pure): semver, git hash and
/// `published` all equal is the same build. Another dev build is taken
/// only for this worker's platform and with `dev_builds` (`member.toml`'s
/// `dev-builds`, or trust `full`: `fleet::member::Member::takes_dev_builds`);
/// the platform is checked first, since `dev-builds` can't help there.
pub fn decide(ours: &Build, hub: &Build, our_platform: &str, hub_platform: &str, dev_builds: bool) -> Decision {
    if ours == hub {
        Decision::Same
    } else if hub.published {
        Decision::Download { version: hub.semver.clone() }
    } else if hub_platform != our_platform {
        Decision::Stop(format!("dev build: can't run on {our_platform} (the hub runs {} for {hub_platform})", hub.label()))
    } else if !dev_builds {
        Decision::Stop(format!(
            "dev build: not taken (`dev-builds = true` in member.toml or `devsandbox machine trust full` takes it; the hub runs {})",
            hub.label()
        ))
    } else {
        Decision::Receive
    }
}

/// Whether a hub saying `hub` pushes its binary to a worker saying `worker`
/// (both `Hello` hashes): when that worker, taking dev builds, would
/// [`Decision::Receive`] it. A worker with `dev-builds` off refuses the link
/// before the push opens, so the hub needn't know.
pub fn hub_pushes(hub: &str, worker: &str) -> bool {
    decide(&Build::parse(worker), &Build::parse(hub), platform_of(worker), platform_of(hub), true) == Decision::Receive
}

/// The hub's end of a dev build's update: `binary` (this process's own) as
/// one [`BINARY_KIND`] transfer through the link's transfer socket at
/// `socket` (`endpoint::fleet_transfer_socket`), named `devsandbox`.
pub fn push(socket: &Path, binary: &Path) -> Result<()> {
    let open = || std::fs::File::open(binary).with_context(|| format!("cannot read {}", binary.display()));
    let (size, sha256) = transfer::digest(open()?).with_context(|| format!("cannot read {}", binary.display()))?;
    let header = transfer::Header { kind: BINARY_KIND.into(), name: "devsandbox".into(), size, sha256, to: None };
    let stream = UnixStream::connect(socket).with_context(|| format!("cannot connect to {}", socket.display()))?;
    transfer::send(stream, &header, open()?)
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
    /// The build `binary fleet-hello` reports: its [`HELLO`].
    fn build(&self, binary: &Path) -> Result<String>;
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

/// Install the hub's dev build, received at `binary`, over this daemon's
/// binary and hand off to it: make it executable, check that it reports
/// `hub` (the hub's `Hello` hash, every word), install it at
/// [`Steps::dest`], check the installed binary reports `hub` too, then
/// [`Steps::handoff`]. A peer link's hub flag in `hub` isn't part of the
/// build (`fleet-hello` prints [`HELLO`]). Removing `binary`'s scratch dir
/// is the caller's.
pub fn install_received(steps: &dyn Steps, binary: &Path, hub: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("cannot make {} executable", binary.display()))?;
    check_build(steps, binary, hub, "the hub's binary")?;
    let dest = steps.dest()?;
    steps.install(binary, &dest)?;
    check_build(steps, &dest, hub, "the installed binary")?;
    steps.handoff().context("cannot hand this daemon off to it")
}

fn check_build(steps: &dyn Steps, binary: &Path, want: &str, what: &str) -> Result<()> {
    let got = steps.build(binary).with_context(|| format!("{what} ({}) doesn't run", binary.display()))?;
    if !build_words(&got).eq(build_words(want)) {
        bail!("{what} ({}) reports {:?}, not {want:?}", binary.display(), got.trim());
    }
    Ok(())
}

fn check_version(steps: &dyn Steps, binary: &Path, want: &str, what: &str) -> Result<()> {
    let got = steps.version(binary).with_context(|| format!("{what} ({}) doesn't run", binary.display()))?;
    if got != want {
        bail!("{what} ({}) reports {got}, not {want}", binary.display());
    }
    Ok(())
}

/// A new, private dir under `parent`.
pub fn scratch_dir(parent: &Path) -> Result<PathBuf> {
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

    fn build(&self, binary: &Path) -> Result<String> {
        Ok(run(&[binary.into(), "fleet-hello".into()])?.trim().to_string())
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
        assert_eq!(platform_of(HELLO), PLATFORM);
        assert_eq!(PLATFORM, format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH));
        assert_eq!(platform_of("0.7.0 1a2b dev"), "unknown");
        assert_eq!(Build::parse("0.7.0 1a2b dev linux/x86_64"), build("0.7.0", "1a2b", false), "the platform isn't the build");
        assert_eq!(build("0.7.0", "1a2b", true).label(), "0.7.0 (published, 1a2b)");
        // A peer link's hub flag: a fifth word, not part of the build.
        assert_eq!(hello_of(true), format!("{HELLO} hub"));
        assert_eq!(hello_of(false), format!("{HELLO} member"));
        assert!(hello_of(true).len() <= u8::MAX as usize, "fits a `Hello` hash");
        assert!(is_hub(hello_of(true)) && !is_hub(hello_of(false)));
        assert!(!is_hub(HELLO), "a lending link's hello never says hub");
        assert!(!is_hub("0.7.0 1a2b dev hub"), "the fifth word, not any");
        assert_eq!((Build::parse(hello_of(true)), platform_of(hello_of(false))), (ours, PLATFORM));
    }

    #[test]
    fn the_worker_follows_its_hub_exactly() {
        let (lx, mac) = ("linux/x86_64", "macos/aarch64");
        let d = |ours: &Build, hub: &Build| decide(ours, hub, lx, lx, false);
        let ours = build("0.8.0", "aaaa", true);
        assert_eq!(d(&ours, &ours.clone()), Decision::Same);
        assert_eq!(decide(&ours, &ours.clone(), lx, mac, false), Decision::Same, "one release, many platforms");
        let newer = build("0.9.0", "bbbb", true);
        assert_eq!(d(&ours, &newer), Decision::Download { version: "0.9.0".into() });
        assert_eq!(decide(&ours, &newer, lx, mac, false), Decision::Download { version: "0.9.0".into() }, "each its asset");
        let older = build("0.7.0", "cccc", true);
        assert_eq!(d(&ours, &older), Decision::Download { version: "0.7.0".into() }, "a downgrade too");
        // A dev worker of a published hub takes the release, same semver or not.
        let dev = build("0.8.0", "dddd", false);
        assert_eq!(d(&dev, &ours), Decision::Download { version: "0.8.0".into() });
        assert_eq!(d(&dev, &dev.clone()), Decision::Same);
        assert_eq!(decide(&dev, &dev.clone(), lx, mac, false), Decision::Same);
    }

    #[test]
    fn a_dev_hub_build_needs_the_platform_then_dev_builds() {
        let (lx, mac) = ("linux/x86_64", "macos/aarch64");
        let ours = build("0.8.0", "aaaa", true);
        let dev = build("0.8.0", "dddd", false);
        assert_eq!(decide(&ours, &dev, lx, lx, true), Decision::Receive);
        assert_eq!(decide(&build("0.8.0", "eeee", false), &dev, lx, lx, true), Decision::Receive, "another dev build");
        assert_eq!(
            decide(&ours, &dev, lx, lx, false),
            Decision::Stop(
                "dev build: not taken (`dev-builds = true` in member.toml or `devsandbox machine trust full` takes it; the hub runs 0.8.0 (dev, dddd))"
                    .into()
            )
        );
        let mismatch = Decision::Stop("dev build: can't run on linux/x86_64 (the hub runs 0.8.0 (dev, dddd) for macos/aarch64)".into());
        assert_eq!(decide(&ours, &dev, lx, mac, true), mismatch);
        assert_eq!(decide(&ours, &dev, lx, mac, false), mismatch, "the platform first: dev-builds can't help");
        assert!(matches!(decide(&ours, &dev, lx, "unknown", true), Decision::Stop(_)), "no platform word");
    }

    #[test]
    fn the_hub_pushes_to_a_worker_that_would_receive() {
        let hub = "0.8.0 dddd dev linux/x86_64";
        assert!(hub_pushes(hub, "0.8.0 aaaa published linux/x86_64"));
        assert!(hub_pushes(hub, "0.7.0 eeee dev linux/x86_64"));
        assert!(!hub_pushes(hub, hub), "the same build");
        assert!(!hub_pushes(hub, "0.8.0 aaaa published macos/aarch64"), "another platform");
        assert!(!hub_pushes("0.8.0 dddd published linux/x86_64", "0.7.0 eeee dev linux/x86_64"), "a release is downloaded");
    }

    #[test]
    fn a_push_is_one_binary_transfer() {
        let dir = scratch("push");
        let socket = dir.join("t.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let binary = dir.join("exe");
        std::fs::write(&binary, b"the hub's build").unwrap();
        let into = dir.join("in");
        let receiver = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            transfer::receive(stream, BINARY_LIMIT, |h| Ok(into.join(&h.name)))
        });
        push(&socket, &binary).unwrap();
        let got = receiver.join().unwrap().unwrap();
        assert_eq!((got.header.kind.as_str(), got.header.name.as_str(), got.header.size), (BINARY_KIND, "devsandbox", 15));
        assert_eq!(std::fs::read(&got.path).unwrap(), b"the hub's build");
        assert!(format!("{:#}", push(&socket, &dir.join("nope")).unwrap_err()).starts_with("cannot read"));
        let _ = std::fs::remove_dir_all(&dir);
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
        /// `fleet-hello` of a received binary and of the installed one.
        pub received_build: String,
        pub installed_build: String,
        pub fail: Option<&'static str>,
    }

    impl Fake {
        pub(crate) fn reporting(version: &str) -> Fake {
            Fake { fetched_version: version.into(), installed_version: version.into(), ..Fake::default() }
        }

        pub(crate) fn building(hello: &str) -> Fake {
            Fake { received_build: hello.into(), installed_build: hello.into(), ..Fake::default() }
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
        fn build(&self, binary: &Path) -> Result<String> {
            let installed = binary == Path::new("/dest/devsandbox");
            self.step("build", if installed { "dest".into() } else { "received".into() })?;
            Ok(if installed { self.installed_build.clone() } else { self.received_build.clone() })
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
    fn a_received_build_is_made_executable_checked_installed_and_handed_off_to() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("received");
        let binary = dir.join("devsandbox");
        let received = || {
            std::fs::write(&binary, "new").unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        let hub = "0.8.0 dddd dev linux/x86_64";
        received();
        let fake = Fake::building(hub);
        install_received(&fake, &binary, hub).unwrap();
        assert_eq!(std::fs::metadata(&binary).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fake.calls(), ["build received", "install /dest/devsandbox", "build dest", "handoff "]);
        // A peer link's hub said its hub flag too: not part of the build.
        received();
        let fake = Fake::building(hub);
        install_received(&fake, &binary, &format!("{hub} hub")).unwrap();
        assert_eq!(fake.calls().len(), 4);
        // Not the hub's build: never installed.
        received();
        let fake = Fake { received_build: "0.8.0 eeee dev linux/x86_64".into(), ..Fake::building(hub) };
        let err = format!("{:#}", install_received(&fake, &binary, hub).unwrap_err());
        assert!(err.contains("reports \"0.8.0 eeee dev linux/x86_64\", not \"0.8.0 dddd dev linux/x86_64\""), "{err}");
        assert_eq!(fake.calls(), ["build received"]);
        // One that doesn't run.
        let fake = Fake { fail: Some("build"), ..Fake::building(hub) };
        let err = format!("{:#}", install_received(&fake, &binary, hub).unwrap_err());
        assert!(err.starts_with("the hub's binary (") && err.ends_with("doesn't run: build broke"), "{err}");
        // The installed one isn't it: no handoff.
        let fake = Fake { installed_build: "0.8.0 dddd dev".into(), ..Fake::building(hub) };
        let err = format!("{:#}", install_received(&fake, &binary, hub).unwrap_err());
        assert!(err.starts_with("the installed binary (/dest/devsandbox) reports"), "{err}");
        assert!(!fake.calls().iter().any(|c| c.starts_with("handoff")));
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
