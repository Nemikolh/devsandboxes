//! A fleet worker follows its control plane's version (docs/architecture/daemon/fleet.md,
//! *Versions*; ADR 0042); the fleet's authority, like the control plane, follows
//! no one (`commands::fleet_link::follows`). Each end of the fleet link says its build in its
//! `Hello` ([`HELLO`]: semver, git hash, `published` or `dev`, platform);
//! the worker compares the control plane's with its own ([`decide`], pure) before it
//! serves anything. A published control-plane build that differs (a downgrade
//! included) is downloaded from the GitHub release and installed over this
//! daemon's binary, which then takes over by a forced handoff ([`update`]
//! over [`Steps`]); the release's `SHA256SUMS` must carry a signature by
//! [`RELEASE_KEY`] and the archive's sha256 first (ADR 0047). A dev build that differs comes over the link instead:
//! the control plane pushes its own binary ([`control_plane_pushes`], [`push`]) and a worker
//! that takes dev builds for its platform installs it the same way
//! ([`install_received`]); else it stops offering, saying why.
//!
//! The steps are injectable ([`Steps`]): [`Release`] is production (curl,
//! tar, `machines::install::INSTALL` run locally, the forced hello, the
//! embedded key), tests fake every one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};

use super::transfer;
use crate::serve::client::{self, Hello, StartReason};
use crate::serve::endpoint;
use crate::serve::machines::install;
use crate::serve::proto::Version;

/// What this build says in the fleet link's `Hello`:
/// `<semver> <git hash> <published|dev> <os>/<arch>`, stamped by `build.rs`
/// (`DEVSANDBOX_BUILD_GIT`, `DEVSANDBOX_BUILD_KIND`,
/// `DEVSANDBOX_BUILD_PLATFORM`).
pub const HELLO: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " ",
    env!("DEVSANDBOX_BUILD_GIT"),
    " ",
    env!("DEVSANDBOX_BUILD_KIND"),
    " ",
    env!("DEVSANDBOX_BUILD_PLATFORM")
);

/// What this build says on a peer link (`fleet-link --peer`,
/// docs/architecture/daemon/fleet.md, *Peer links*): [`HELLO`], then `control_plane`
/// when this host is the fleet's control plane, else `member`, then the fleet's control plane
/// by member name (on the control plane, its own), `-` in no fleet (fleet plan
/// decision 34), then this binary's sha256 prefix ([`own_sha_word`]), then
/// `dev` when this host takes dev builds (`dev_builds`), else `nodev`. Read
/// back by [`is_control_plane`], [`control_plane_of`], [`sha_of`] and
/// [`takes_dev_builds`]. A name that would overflow a `Hello` hash (255
/// bytes) is said as `-`.
///
/// Interned: `link::Spec::hello` is `&'static`, and a process only ever
/// says a handful of these (one per control plane it's had), so each distinct one is
/// leaked once.
pub fn hello_of(control_plane: bool, fleet: Option<&str>, dev_builds: bool) -> &'static str {
    static SAID: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let role = if control_plane { "control_plane" } else { "member" };
    let tail = format!("{} {}", own_sha_word(), if dev_builds { "dev" } else { "nodev" });
    let mut hello = format!("{HELLO} {role} {} {tail}", fleet.unwrap_or("-"));
    if hello.len() > u8::MAX as usize {
        hello = format!("{HELLO} {role} - {tail}");
    }
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = said.iter().find(|s| **s == hello) {
        return s;
    }
    let s: &'static str = Box::leak(hello.into_boxed_str());
    said.push(s);
    s
}

/// [`hello_of`] for this host as `member.toml` says: its `control_plane`
/// ([`super::member::ControlPlane`]), its own member name `own` (what the control plane
/// says as its fleet's) and whether it takes dev builds
/// (`member::Member::takes_dev_builds`).
pub fn hello_for(control_plane: &super::member::ControlPlane, own: Option<&str>, dev_builds: bool) -> &'static str {
    use super::member::ControlPlane;
    match control_plane {
        ControlPlane::None => hello_of(false, None, dev_builds),
        ControlPlane::This => hello_of(true, own, dev_builds),
        ControlPlane::Other(name) => hello_of(false, Some(name), dev_builds),
    }
}

/// The sha256 of this process's own binary (`commands::fleet_link::own_binary`:
/// the inode it runs on Linux, even after the path was replaced), hashed
/// once per process. A dev build is its bytes: a rebuild from uncommitted
/// edits keeps its [`Build`] label.
pub fn own_sha256() -> Result<[u8; 32]> {
    static SHA: std::sync::OnceLock<Result<[u8; 32], String>> = std::sync::OnceLock::new();
    let sha = SHA.get_or_init(|| {
        let binary = crate::commands::fleet_link::own_binary().map_err(|e| format!("cannot locate this binary: {e}"))?;
        sha256_of(&binary).map_err(|e| format!("{e:#}"))
    });
    sha.clone().map_err(|e| anyhow::anyhow!("{e}"))
}

/// The sha256 of the file at `path`.
pub fn sha256_of(path: &Path) -> Result<[u8; 32]> {
    let file = std::fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let (_, hex) = transfer::digest(file).with_context(|| format!("cannot read {}", path.display()))?;
    let mut sha = [0u8; 32];
    hex::decode_to_slice(&hex, &mut sha).expect("digest says 64 hex digits");
    Ok(sha)
}

/// How many hex digits of a binary's sha256 a `Hello` says.
pub const SHA_WORD: usize = 16;

/// [`own_sha256`]'s first [`SHA_WORD`] hex digits, `-` when it can't be
/// read: a `Hello`'s seventh word.
pub fn own_sha_word() -> String {
    match own_sha256() {
        Ok(sha) => hex::encode(sha)[..SHA_WORD].to_string(),
        Err(_) => "-".into(),
    }
}

/// The sha256 prefix a peer link's `Hello` hash ([`hello_of`]) says its
/// end's binary has: its seventh word; `None` for `-` or none.
#[cfg_attr(not(test), allow(dead_code))]
pub fn sha_of(hello: &str) -> Option<&str> {
    hello.split_whitespace().nth(6).filter(|w| *w != "-")
}

/// Whether a peer link's `Hello` hash ([`hello_of`]) says its end takes
/// dev builds: its eighth word is `dev`. A `Hello` without one doesn't.
#[cfg_attr(not(test), allow(dead_code))]
pub fn takes_dev_builds(hello: &str) -> bool {
    hello.split_whitespace().nth(7) == Some("dev")
}

/// Whether a peer link's `Hello` hash ([`hello_of`]) says its end is the
/// control plane: its fifth word is `control_plane`. [`HELLO`] alone (four words) never is.
pub fn is_control_plane(hello: &str) -> bool {
    hello.split_whitespace().nth(4) == Some("control_plane")
}

/// The fleet a peer link's `Hello` hash ([`hello_of`]) says its end is in,
/// by its control plane's member name: its sixth word; `None` for `-` or none.
pub fn control_plane_of(hello: &str) -> Option<&str> {
    hello.split_whitespace().nth(5).filter(|w| *w != "-")
}

/// A link whose two ends name different control planes is refused (fleet plan
/// decision 34; pure): `Err` with why, `peer` the far end's name. One end
/// in no fleet, or both, is fine (`machine add`'s join rule puts them in
/// one).
pub fn same_fleet(peer: &str, ours: &str, theirs: &str) -> Result<(), String> {
    match (control_plane_of(ours), control_plane_of(theirs)) {
        (Some(ours), Some(theirs)) if ours != theirs => {
            Err(format!("`{peer}` is in `{theirs}`'s fleet, this host in `{ours}`'s: a link between two fleets is refused"))
        }
        _ => Ok(()),
    }
}

/// The words of a `Hello` hash that are the build (and platform): the first
/// four, without a peer link's control plane words.
fn build_words(hello: &str) -> impl Iterator<Item = &str> {
    hello.split_whitespace().take(4)
}

/// This binary's platform, `<os>/<arch>` as `std::env::consts` names them
/// (what [`HELLO`] says; tests build other ends' hellos with it).
#[cfg(test)]
pub const PLATFORM: &str = env!("DEVSANDBOX_BUILD_PLATFORM");

/// The transfer kind of a control plane's binary, and the most a member takes of it.
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

/// What a member does about its control plane's build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The same build: serve the link.
    Same,
    /// The control plane runs published `build`, this worker another build: fetch
    /// and install its release asset, then hand off (an older version too).
    Download { build: Build },
    /// The control plane runs a dev build this worker isn't, for this platform, and
    /// this worker takes dev builds: take the binary the control plane pushes, install
    /// it, then hand off.
    Receive,
    /// The control plane runs a build this worker can't or won't take (a dev build,
    /// or a published one with no release asset for this platform): stop
    /// offering, with why.
    Stop(String),
}

/// The worker follows the control plane's exact build (pure): semver, git hash and
/// `published` all equal is the same build. Another dev build is taken
/// only for this worker's platform and with `dev_builds` (`member.toml`'s
/// `dev-builds`, or trust `full`: `fleet::member::Member::takes_dev_builds`);
/// the platform is checked first, since `dev-builds` can't help there.
/// Another published build is downloaded only where a release asset exists
/// for `our_platform` ([`platform_target`]): else [`update`] would fail
/// the same way at every link, so this stops offering instead.
pub fn decide(ours: &Build, control_plane: &Build, our_platform: &str, control_plane_platform: &str, dev_builds: bool) -> Decision {
    if ours == control_plane {
        Decision::Same
    } else if control_plane.published {
        match platform_target(our_platform) {
            Some(_) => Decision::Download { build: control_plane.clone() },
            None => Decision::Stop(format!("published build: no devsandbox release is built for {our_platform} (the control plane runs {})", control_plane.label())),
        }
    } else if control_plane_platform != our_platform {
        Decision::Stop(format!("dev build: can't run on {our_platform} (the control plane runs {} for {control_plane_platform})", control_plane.label()))
    } else if !dev_builds {
        Decision::Stop(format!(
            "dev build: not taken (`dev-builds = true` in member.toml or `devsandbox machine trust full` takes it; the control plane runs {})",
            control_plane.label()
        ))
    } else {
        Decision::Receive
    }
}

/// Whether a control plane saying `control_plane` pushes its binary to a worker saying `worker`
/// (both `Hello` hashes): when that worker, taking dev builds, would
/// [`Decision::Receive`] it. A worker with `dev-builds` off refuses the link
/// before the push opens, so the control plane needn't know. Never to the
/// fleet's authority (`worker_authority`): it follows no one, so it waits for
/// no binary and the push would land on an ordinary `TRANSFER`.
pub fn control_plane_pushes(control_plane: &str, worker: &str, worker_authority: bool) -> bool {
    !worker_authority
        && decide(&Build::parse(worker), &Build::parse(control_plane), platform_of(worker), platform_of(control_plane), true) == Decision::Receive
}

/// The control plane's end of a dev build's update: `binary` (this process's own) as
/// one [`BINARY_KIND`] transfer, a `TRANSFER` stream through the link's
/// socket at `socket` (`endpoint::fleet_socket`), named `devsandbox`.
pub fn push(socket: &Path, binary: &Path) -> Result<()> {
    let open = || std::fs::File::open(binary).with_context(|| format!("cannot read {}", binary.display()));
    let (size, sha256) = transfer::digest(open()?).with_context(|| format!("cannot read {}", binary.display()))?;
    let header = transfer::Header { kind: BINARY_KIND.into(), name: "devsandbox".into(), size, sha256, to: None, repo: None, refs: None };
    let stream = super::link::connect(socket, crate::devsbd::proto::channel::TRANSFER).with_context(|| format!("cannot connect to {}", socket.display()))?;
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

/// [`release_target`] of a `Hello`'s `<os>/<arch>` platform word
/// ([`platform_of`]); `None` for one without a `/` (`unknown`).
pub fn platform_target(platform: &str) -> Option<&'static str> {
    let (os, arch) = platform.split_once('/')?;
    release_target(os, arch)
}

/// The release asset of `version` for `target`: `devsandbox-<v>-<target>.tar.gz`.
pub fn asset_name(version: &str, target: &str) -> String {
    format!("devsandbox-{version}-{target}.tar.gz")
}

/// `https://github.com/<REPO>/releases/download/v<v>/<file>`. `version`
/// comes from the control plane: only a semver's characters go in the URL.
pub fn release_url(version: &str, file: &str) -> Result<String> {
    let ok = !version.is_empty() && version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b));
    if !ok {
        bail!("`{version}` isn't a version");
    }
    Ok(format!("https://github.com/{REPO}/releases/download/v{version}/{file}"))
}

/// The release's checksums, `sha256sum`'s format over every archive, and
/// their signature: raw ed25519 (64 bytes) over the file's bytes by the
/// release key (`.github/workflows/release.yml`'s `release` job).
pub const SUMS: &str = "SHA256SUMS";
pub const SUMS_SIG: &str = "SHA256SUMS.sig";

/// The release signing key's public half (ed25519, raw): the repo's
/// `release-signing.pub`, one line of hex, the file the release workflow
/// checks its secret key against. A release whose [`SUMS`] it didn't sign
/// is never installed.
pub const RELEASE_KEY: [u8; 32] = parse_key(include_str!("../../../release-signing.pub"));

/// 64 hex digits and an optional trailing newline, as 32 bytes; anything
/// else fails the build.
const fn parse_key(text: &str) -> [u8; 32] {
    const fn digit(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("release-signing.pub: not lowercase hex"),
        }
    }
    let bytes = text.as_bytes();
    let len = if !bytes.is_empty() && bytes[bytes.len() - 1] == b'\n' { bytes.len() - 1 } else { bytes.len() };
    if len != 64 {
        panic!("release-signing.pub: not 64 hex digits");
    }
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = digit(bytes[2 * i]) << 4 | digit(bytes[2 * i + 1]);
        i += 1;
    }
    out
}

/// Whether `sig` is `key`'s ed25519 signature over `sums` (pure; strict:
/// canonical signatures, no small-order key).
pub fn verify_sums(sums: &[u8], sig: &[u8], key: &[u8; 32]) -> Result<()> {
    let Ok(sig) = <[u8; 64]>::try_from(sig) else {
        bail!("{SUMS_SIG} is {} bytes, not a 64-byte ed25519 signature", sig.len());
    };
    let key = ed25519_dalek::VerifyingKey::from_bytes(key).map_err(|_| anyhow::anyhow!("the release key isn't an ed25519 public key"))?;
    key.verify_strict(sums, &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| anyhow::anyhow!("{SUMS}'s signature doesn't verify against the release key"))
}

/// The sha256 [`SUMS`] gives `name` (pure): its one `<64 hex>  <name>`
/// line (`sha256sum`'s text mode; ` *<name>`, binary mode, too). None or
/// several is an error, and so is a line that isn't one.
pub fn sum_of(sums: &[u8], name: &str) -> Result<[u8; 32]> {
    let sums = std::str::from_utf8(sums).map_err(|_| anyhow::anyhow!("{SUMS} isn't text"))?;
    let mut found = None;
    for (n, line) in sums.lines().enumerate().filter(|(_, l)| !l.is_empty()) {
        let bad = || anyhow::anyhow!("{SUMS} line {} isn't `<sha256>  <name>`", n + 1);
        let (hex, file) = line.split_once(' ').ok_or_else(bad)?;
        let file = file.strip_prefix(' ').or_else(|| file.strip_prefix('*')).ok_or_else(bad)?;
        let mut sum = [0u8; 32];
        if hex.len() != 64 || hex::decode_to_slice(hex, &mut sum).is_err() {
            return Err(bad());
        }
        if file == name && found.replace(sum).is_some() {
            bail!("{SUMS} lists {name} more than once");
        }
    }
    found.with_context(|| format!("{SUMS} doesn't list {name}"))
}

/// `archive`'s sha256 is `want`, [`SUMS`]'s for `name`.
fn check_sum(archive: &Path, name: &str, want: &[u8; 32]) -> Result<()> {
    let file = std::fs::File::open(archive).with_context(|| format!("cannot read {}", archive.display()))?;
    let (_, got) = transfer::digest(file).with_context(|| format!("cannot read {}", archive.display()))?;
    if got != hex::encode(want) {
        bail!("{name}'s sha256 is {got}, not {SUMS}'s {}", hex::encode(want));
    }
    Ok(())
}

/// The steps of an update, injectable.
pub trait Steps: Send + Sync {
    /// Download `url` into the file `out`.
    fn download(&self, url: &str, out: &Path) -> Result<()>;
    /// Unpack the binary of the release archive `archive` into `into`.
    fn unpack(&self, archive: &Path, into: &Path) -> Result<PathBuf>;
    /// The key a release's [`SUMS`] must be signed by ([`RELEASE_KEY`]).
    fn release_key(&self) -> [u8; 32];
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

/// Install the control plane's published `control_plane` build over this daemon's binary and
/// hand off to it: fetch its release asset for `os`/`arch` into a scratch
/// dir under `scratch` ([`fetch_verified`]: signed checksums first), check
/// that its `fleet-hello` is `control_plane` (semver, git
/// and kind: a release asset that isn't the control plane's exact build would be
/// downloaded again at every link), install it at [`Steps::dest`], check
/// the installed binary is `control_plane` too, then [`Steps::handoff`]. The scratch
/// dir is removed whatever happens. [`decide`] never says `Download` where
/// `os`/`arch` has no release; the check here stands in case it does.
pub fn update(steps: &dyn Steps, control_plane: &Build, os: &str, arch: &str, scratch: &Path) -> Result<()> {
    let target = release_target(os, arch).with_context(|| format!("no devsandbox release is built for {os}/{arch}"))?;
    let name = asset_name(&control_plane.semver, target);
    let dir = scratch_dir(scratch)?;
    let installed = (|| {
        let binary = fetch_verified(steps, &control_plane.semver, &name, &dir)?;
        check_release(steps, &binary, control_plane, "the downloaded binary")?;
        let dest = steps.dest()?;
        steps.install(&binary, &dest)?;
        check_release(steps, &dest, control_plane, "the installed binary")
    })();
    let _ = std::fs::remove_dir_all(&dir);
    installed?;
    steps.handoff().context("cannot hand this daemon off to it")
}

/// Install the control plane's dev build, received at `binary`, over this daemon's
/// binary and hand off to it: make it executable, check that it reports
/// `control_plane` (the control plane's `Hello` hash, every word), install it at
/// [`Steps::dest`], check the installed binary reports `control_plane` too, then
/// [`Steps::handoff`]. A peer link's control plane words in `control_plane` aren't part of the
/// build (`fleet-hello` prints [`HELLO`]). Removing `binary`'s scratch dir
/// is the caller's.
pub fn install_received(steps: &dyn Steps, binary: &Path, control_plane: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("cannot make {} executable", binary.display()))?;
    check_build(steps, binary, control_plane, "the control plane's binary")?;
    let dest = steps.dest()?;
    steps.install(binary, &dest)?;
    check_build(steps, &dest, control_plane, "the installed binary")?;
    steps.handoff().context("cannot hand this daemon off to it")
}

/// Release `version`'s file `file`, downloaded into `dir`.
fn fetch(steps: &dyn Steps, version: &str, file: &str, dir: &Path) -> Result<PathBuf> {
    let url = release_url(version, file)?;
    let out = dir.join(file);
    steps.download(&url, &out).with_context(|| format!("cannot download {url}"))?;
    Ok(out)
}

/// Release `version`'s [`SUMS`], downloaded into `dir` with [`SUMS_SIG`]
/// and only returned once the signature checks against
/// [`Steps::release_key`] ([`verify_sums`]).
fn fetch_sums(steps: &dyn Steps, version: &str, dir: &Path) -> Result<Vec<u8>> {
    let read = |path: &Path| std::fs::read(path).with_context(|| format!("cannot read {}", path.display()));
    let sums = read(&fetch(steps, version, SUMS, dir)?)?;
    let sig = read(&fetch(steps, version, SUMS_SIG, dir)?)?;
    verify_sums(&sums, &sig, &steps.release_key()).with_context(|| format!("release v{version} isn't signed by the release key"))?;
    Ok(sums)
}

/// The sha256 of release `version`'s [`SUMS`], fetched into a scratch dir
/// under `scratch` and signature-checked ([`fetch_sums`]): what a published
/// build's pin (`membership::Pin::Published`) binds. The scratch dir is
/// removed whatever happens.
pub fn release_sums_sha256(steps: &dyn Steps, version: &str, scratch: &Path) -> Result<[u8; 32]> {
    use sha2::Digest;
    let dir = scratch_dir(scratch)?;
    let sums = fetch_sums(steps, version, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(sha2::Sha256::digest(sums?).into())
}

/// Where the authority keeps the binaries it pinned, under data dir `data`
/// (`member.toml`'s): `fleet/.builds/`, a dot name no machine's fleet root
/// (`fleet/<name>/`) can have.
pub fn builds_dir(data: &Path) -> PathBuf {
    data.join("fleet").join(".builds")
}

/// How many kept builds [`keep_build`] leaves, the new one included.
pub const KEPT_BUILDS: usize = 3;

/// `binary`, whose sha256 is `sha256`, copied to
/// `<builds_dir>/<sha256 hex>` (dir 0700, file 0755; through a temp file
/// whose sha256 must be `sha256`, then renamed), unless an intact copy is
/// there already. The oldest other copies are removed past
/// [`KEPT_BUILDS`]. A later `cargo build` replaces the file a running
/// binary's path names: this copy is what the pin's bytes stay as.
pub fn keep_build(data: &Path, binary: &Path, sha256: &[u8; 32]) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = builds_dir(data);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).with_context(|| format!("cannot restrict {}", dir.display()))?;
    let name = hex::encode(sha256);
    let dest = dir.join(&name);
    if sha256_of(&dest).ok() != Some(*sha256) {
        let tmp = dir.join(format!(".{name}.{}", std::process::id()));
        let copied = (|| {
            std::fs::copy(binary, &tmp).with_context(|| format!("cannot copy {} to {}", binary.display(), tmp.display()))?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).with_context(|| format!("cannot chmod {}", tmp.display()))?;
            let got = sha256_of(&tmp)?;
            if got != *sha256 {
                bail!("the copy of {} hashes to {}, not {name}: it changed meanwhile", binary.display(), hex::encode(got));
            }
            std::fs::rename(&tmp, &dest).with_context(|| format!("cannot rename {} to {}", tmp.display(), dest.display()))
        })();
        if copied.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        copied?;
    }
    prune_builds(&dir, &name);
    Ok(dest)
}

/// The kept builds in `dir` but `keep` and the newest others, oldest
/// first removed until [`KEPT_BUILDS`] are left. Only `<64 hex>` names.
fn prune_builds(dir: &Path, keep: &str) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut others: Vec<(std::time::SystemTime, PathBuf)> = read
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n != keep && n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    others.sort();
    let extra = others.len().saturating_sub(KEPT_BUILDS - 1);
    for (_, path) in others.into_iter().take(extra) {
        let _ = std::fs::remove_file(path);
    }
}

/// Release `version`'s asset `name`, unpacked in `dir`: [`SUMS`] and
/// [`SUMS_SIG`] downloaded and the signature checked against
/// [`Steps::release_key`] ([`fetch_sums`]), `name`'s line found
/// ([`sum_of`]), only then the archive downloaded, its sha256 checked, and
/// unpacked. No signature, no install: there is no unsigned path.
fn fetch_verified(steps: &dyn Steps, version: &str, name: &str, dir: &Path) -> Result<PathBuf> {
    let sums = fetch_sums(steps, version, dir)?;
    let want = sum_of(&sums, name).with_context(|| format!("release v{version}'s checksums"))?;
    let archive = fetch(steps, version, name, dir)?;
    check_sum(&archive, name, &want)?;
    steps.unpack(&archive, dir)
}

fn check_build(steps: &dyn Steps, binary: &Path, want: &str, what: &str) -> Result<()> {
    let got = steps.build(binary).with_context(|| format!("{what} ({}) doesn't run", binary.display()))?;
    if !build_words(&got).eq(build_words(want)) {
        bail!("{what} ({}) reports {:?}, not {want:?}", binary.display(), got.trim());
    }
    Ok(())
}

/// `binary`'s `fleet-hello` is the build `want` (its platform aside: the
/// asset is this host's, the control plane's may be another).
fn check_release(steps: &dyn Steps, binary: &Path, want: &Build, what: &str) -> Result<()> {
    let got = steps.build(binary).with_context(|| format!("{what} ({}) doesn't run", binary.display()))?;
    let got = Build::parse(&got);
    if got != *want {
        bail!("{what} ({}) is devsandbox {}, not the control plane's {}", binary.display(), got.label(), want.label());
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

/// The most a release download may take, in seconds (`curl --max-time`):
/// the stall bound alone lets a trickle hold the member's update, and its
/// link, for ever.
pub const DOWNLOAD_MAX_TIME: &str = "600";

/// `curl` fetching `url` into `out`: HTTPS only (redirects included), TLS
/// 1.2 at least, an HTTP error a failure; a stalled transfer (<1 B/s for
/// 30 s) gives up, and so does one still running after
/// [`DOWNLOAD_MAX_TIME`].
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
        "--max-time",
        DOWNLOAD_MAX_TIME,
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
    fn download(&self, url: &str, out: &Path) -> Result<()> {
        run(&curl_argv(url, out)).map(drop)
    }

    fn unpack(&self, archive: &Path, into: &Path) -> Result<PathBuf> {
        let unpack: Vec<OsString> = vec!["tar".into(), "-xzf".into(), archive.into(), "-C".into(), into.into(), "devsandbox".into()];
        run(&unpack)?;
        let binary = into.join("devsandbox");
        if !binary.is_file() {
            bail!("the archive has no devsandbox binary");
        }
        Ok(binary)
    }

    fn release_key(&self) -> [u8; 32] {
        RELEASE_KEY
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
        let ours = Build::parse(HELLO);
        assert_eq!(ours.semver, env!("CARGO_PKG_VERSION"));
        assert_eq!(Build::parse(HELLO), ours);
        assert!(HELLO.len() <= u8::MAX as usize, "fits a `Hello` hash");
        assert_eq!(platform_of(HELLO), PLATFORM);
        assert_eq!(PLATFORM, format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH));
        assert_eq!(platform_of("0.7.0 1a2b dev"), "unknown");
        assert_eq!(Build::parse("0.7.0 1a2b dev linux/x86_64"), build("0.7.0", "1a2b", false), "the platform isn't the build");
        assert_eq!(build("0.7.0", "1a2b", true).label(), "0.7.0 (published, 1a2b)");
        // A peer link's control plane words: the role, then the control plane, not part of the build;
        // then this binary's sha256 prefix and whether this end takes dev builds.
        let sha = own_sha_word();
        assert_eq!(sha.len(), SHA_WORD);
        assert_eq!(sha, hex::encode(own_sha256().unwrap())[..SHA_WORD]);
        let own = sha256_of(&crate::commands::fleet_link::own_binary().unwrap()).unwrap();
        assert_eq!(own_sha256().unwrap(), own, "this binary's bytes");
        assert_eq!(hello_of(true, Some("box"), false), format!("{HELLO} control_plane box {sha} nodev"));
        assert_eq!(hello_of(false, Some("box"), true), format!("{HELLO} member box {sha} dev"));
        assert_eq!(hello_of(false, None, false), format!("{HELLO} member - {sha} nodev"));
        assert!(std::ptr::eq(hello_of(false, Some("box"), true), hello_of(false, Some("box"), true)), "leaked once");
        assert!(hello_of(true, Some(&"x".repeat(63)), true).len() <= u8::MAX as usize, "fits a `Hello` hash");
        assert_eq!(hello_of(true, Some(&"x".repeat(250)), true), format!("{HELLO} control_plane - {sha} dev"), "too long a name isn't said");
        let longest = 255 - format!("{HELLO} control_plane  {sha} nodev").len();
        assert_eq!(hello_of(true, Some(&"x".repeat(longest)), false).len(), 255, "a name that just fits is said");
        assert!(hello_of(true, Some(&"x".repeat(longest + 1)), false).contains(" - "));
        assert!(is_control_plane(hello_of(true, None, true)) && !is_control_plane(hello_of(false, Some("control_plane"), true)));
        assert!(!is_control_plane(HELLO), "a hello without the flag never says control plane");
        assert!(!is_control_plane("0.7.0 1a2b dev control_plane"), "the fifth word, not any");
        assert_eq!((Build::parse(hello_of(true, None, true)), platform_of(hello_of(false, None, false))), (ours, PLATFORM));
        assert_eq!((control_plane_of(hello_of(true, Some("k"), true)), control_plane_of(hello_of(false, Some("k"), false))), (Some("k"), Some("k")));
        assert_eq!((control_plane_of(hello_of(false, None, true)), control_plane_of(HELLO), control_plane_of("0.7.0 a dev x/y control_plane")), (None, None, None));
        // The two new words.
        assert_eq!((sha_of(hello_of(false, None, true)), takes_dev_builds(hello_of(false, None, true))), (Some(sha.as_str()), true));
        assert!(!takes_dev_builds(hello_of(true, Some("k"), false)));
        assert_eq!(sha_of(hello_of(true, Some(&"x".repeat(250)), true)), Some(sha.as_str()), "a dropped name keeps the words");
        assert_eq!((sha_of("0.7.0 a dev x/y member k - dev"), sha_of("0.7.0 a dev x/y member k"), sha_of(HELLO)), (None, None, None));
        assert!(!takes_dev_builds("0.7.0 a dev x/y member k 0123456789abcdef") && !takes_dev_builds("0.7.0 a dev x/y member k dev"), "the eighth word, not any");
        use crate::serve::fleet::member::ControlPlane;
        assert_eq!(hello_for(&ControlPlane::This, Some("k"), true), hello_of(true, Some("k"), true));
        assert_eq!(hello_for(&ControlPlane::Other("k".into()), Some("lap"), false), hello_of(false, Some("k"), false));
        assert_eq!(hello_for(&ControlPlane::None, Some("lap"), true), hello_of(false, None, true));
    }

    #[test]
    fn a_release_s_sums_are_hashed_once_signed() {
        use sha2::Digest;
        let dir = scratch("sums");
        let fake = Fake::default();
        let want: [u8; 32] = sha2::Sha256::digest(honest_sums("0.9.0")).into();
        assert_eq!(release_sums_sha256(&fake, "0.9.0", &dir).unwrap(), want);
        let base = "https://github.com/Nemikolh/devsandboxes/releases/download/v0.9.0";
        assert_eq!(fake.calls(), [format!("fetch {base}/SHA256SUMS"), format!("fetch {base}/SHA256SUMS.sig")]);
        assert!(empty(&dir), "the scratch dir is gone");
        for tamper in ["sums", "sig", "key"] {
            let fake = Fake { tamper: Some(tamper), ..Fake::default() };
            let e = format!("{:#}", release_sums_sha256(&fake, "0.9.0", &dir).unwrap_err());
            assert!(e.contains("release v0.9.0 isn't signed by the release key"), "{tamper}: {e}");
        }
        let offline = Fake { fail: Some("fetch"), ..Fake::default() };
        assert!(format!("{:#}", release_sums_sha256(&offline, "0.9.0", &dir).unwrap_err()).contains("cannot download"));
        assert!(empty(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_kept_build_is_an_exact_private_copy_and_old_ones_go() {
        use std::os::unix::fs::PermissionsExt;
        let data = scratch("keep");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let mut kept = Vec::new();
        for i in 0..5u8 {
            let binary = data.join(format!("bin{i}"));
            std::fs::write(&binary, [i; 10]).unwrap();
            let sha = sha256_of(&binary).unwrap();
            let at = keep_build(&data, &binary, &sha).unwrap();
            assert_eq!(at, builds_dir(&data).join(hex::encode(sha)));
            assert_eq!((std::fs::read(&at).unwrap(), mode(&at), mode(&builds_dir(&data))), ([i; 10].to_vec(), 0o755, 0o700));
            // Again: the intact copy stays.
            assert_eq!(keep_build(&data, &binary, &sha).unwrap(), at);
            kept.push(at);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let left: Vec<bool> = kept.iter().map(|p| p.exists()).collect();
        assert_eq!(left, [false, false, true, true, true], "the newest {KEPT_BUILDS}");
        // A binary that isn't what the pin says: nothing kept.
        let binary = data.join("bin0");
        let e = format!("{:#}", keep_build(&data, &binary, &[9; 32]).unwrap_err());
        assert!(e.contains("it changed meanwhile"), "{e}");
        assert!(!builds_dir(&data).join(hex::encode([9; 32])).exists());
        assert_eq!(std::fs::read_dir(builds_dir(&data)).unwrap().count(), KEPT_BUILDS, "no temp file left");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn two_fleets_never_link() {
        let (k, l, none) = (hello_of(true, Some("k"), false), hello_of(false, Some("l"), false), hello_of(false, None, false));
        assert_eq!(same_fleet("box", k, l), Err("`box` is in `l`'s fleet, this host in `k`'s: a link between two fleets is refused".into()));
        assert!(same_fleet("box", l, k).unwrap_err().starts_with("`box` is in `k`'s fleet, this host in `l`'s"));
        assert_eq!(same_fleet("box", k, hello_of(false, Some("k"), false)), Ok(()), "the same control plane");
        assert_eq!(same_fleet("box", k, none), Ok(()), "one end in no fleet");
        assert_eq!(same_fleet("box", none, l), Ok(()));
        assert_eq!(same_fleet("box", none, none), Ok(()));
        assert_eq!(same_fleet("box", k, HELLO), Ok(()), "a hello without the words");
    }

    #[test]
    fn the_worker_follows_its_control_plane_exactly() {
        let (lx, mac) = ("linux/x86_64", "macos/aarch64");
        let d = |ours: &Build, control_plane: &Build| decide(ours, control_plane, lx, lx, false);
        let ours = build("0.8.0", "aaaa", true);
        assert_eq!(d(&ours, &ours.clone()), Decision::Same);
        assert_eq!(decide(&ours, &ours.clone(), lx, mac, false), Decision::Same, "one release, many platforms");
        let newer = build("0.9.0", "bbbb", true);
        assert_eq!(d(&ours, &newer), Decision::Download { build: newer.clone() });
        assert_eq!(decide(&ours, &newer, lx, mac, false), Decision::Download { build: newer.clone() }, "each its asset");
        let older = build("0.7.0", "cccc", true);
        assert_eq!(d(&ours, &older), Decision::Download { build: older.clone() }, "a downgrade too");
        // A dev worker of a published control plane takes the release, same semver or not.
        let dev = build("0.8.0", "dddd", false);
        assert_eq!(d(&dev, &ours), Decision::Download { build: ours.clone() });
        // A published worker of a published control plane of the same semver but another
        // commit (a moved tag): that release, not a `Same`.
        assert_eq!(d(&ours, &build("0.8.0", "ffff", true)), Decision::Download { build: build("0.8.0", "ffff", true) });
        assert_eq!(d(&dev, &dev.clone()), Decision::Same);
        assert_eq!(decide(&dev, &dev.clone(), lx, mac, false), Decision::Same);
    }

    #[test]
    fn a_published_control_plane_build_needs_a_release_for_our_platform() {
        let ours = build("0.8.0", "aaaa", true);
        let newer = build("0.9.0", "bbbb", true);
        for platform in ["linux/x86_64", "linux/aarch64", "macos/aarch64"] {
            assert_eq!(decide(&ours, &newer, platform, "linux/x86_64", false), Decision::Download { build: newer.clone() }, "{platform}");
        }
        assert_eq!(
            decide(&ours, &newer, "macos/x86_64", "macos/aarch64", true),
            Decision::Stop("published build: no devsandbox release is built for macos/x86_64 (the control plane runs 0.9.0 (published, bbbb))".into())
        );
        assert!(matches!(decide(&ours, &newer, "unknown", "linux/x86_64", true), Decision::Stop(_)), "no platform word");
        assert_eq!(decide(&ours, &ours.clone(), "macos/x86_64", "linux/x86_64", false), Decision::Same, "the same build needs none");
        assert!(!control_plane_pushes("0.9.0 bbbb published linux/x86_64", "0.8.0 aaaa published macos/x86_64", false), "nothing to push");
        assert_eq!(platform_target("linux/aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!((platform_target("macos/x86_64"), platform_target("unknown"), platform_target("")), (None, None, None));
    }

    #[test]
    fn a_dev_control_plane_build_needs_the_platform_then_dev_builds() {
        let (lx, mac) = ("linux/x86_64", "macos/aarch64");
        let ours = build("0.8.0", "aaaa", true);
        let dev = build("0.8.0", "dddd", false);
        assert_eq!(decide(&ours, &dev, lx, lx, true), Decision::Receive);
        assert_eq!(decide(&build("0.8.0", "eeee", false), &dev, lx, lx, true), Decision::Receive, "another dev build");
        assert_eq!(
            decide(&ours, &dev, lx, lx, false),
            Decision::Stop(
                "dev build: not taken (`dev-builds = true` in member.toml or `devsandbox machine trust full` takes it; the control plane runs 0.8.0 (dev, dddd))"
                    .into()
            )
        );
        let mismatch = Decision::Stop("dev build: can't run on linux/x86_64 (the control plane runs 0.8.0 (dev, dddd) for macos/aarch64)".into());
        assert_eq!(decide(&ours, &dev, lx, mac, true), mismatch);
        assert_eq!(decide(&ours, &dev, lx, mac, false), mismatch, "the platform first: dev-builds can't help");
        assert!(matches!(decide(&ours, &dev, lx, "unknown", true), Decision::Stop(_)), "no platform word");
    }

    #[test]
    fn the_control_plane_pushes_to_a_worker_that_would_receive() {
        let control_plane = "0.8.0 dddd dev linux/x86_64";
        assert!(control_plane_pushes(control_plane, "0.8.0 aaaa published linux/x86_64", false));
        assert!(control_plane_pushes(control_plane, "0.7.0 eeee dev linux/x86_64", false));
        assert!(!control_plane_pushes(control_plane, control_plane, false), "the same build");
        assert!(!control_plane_pushes(control_plane, "0.8.0 aaaa published macos/aarch64", false), "another platform");
        assert!(!control_plane_pushes("0.8.0 dddd published linux/x86_64", "0.7.0 eeee dev linux/x86_64", false), "a release is downloaded");
        // The authority follows no one: nothing is pushed to it, older or newer.
        assert!(!control_plane_pushes(control_plane, "0.7.0 eeee dev linux/x86_64", true), "an older authority");
        assert!(!control_plane_pushes(control_plane, "0.9.0 ffff dev linux/x86_64", true), "a newer authority");
    }

    #[test]
    fn a_push_is_one_binary_transfer() {
        let dir = scratch("push");
        let socket = dir.join("t.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let binary = dir.join("exe");
        std::fs::write(&binary, b"the control plane's build").unwrap();
        let into = dir.join("in");
        let receiver = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut ch = [0u8; 1];
            std::io::Read::read_exact(&mut stream, &mut ch).unwrap();
            assert_eq!(ch[0], crate::devsbd::proto::channel::TRANSFER, "a `TRANSFER` stream");
            transfer::receive(stream, BINARY_LIMIT, |h| Ok(into.join(&h.name)))
        });
        push(&socket, &binary).unwrap();
        let got = receiver.join().unwrap().unwrap();
        assert_eq!((got.header.kind.as_str(), got.header.name.as_str(), got.header.size), (BINARY_KIND, "devsandbox", 25));
        assert_eq!(std::fs::read(&got.path).unwrap(), b"the control plane's build");
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
            release_url("0.7.0", &asset_name("0.7.0", "aarch64-apple-darwin")).unwrap(),
            "https://github.com/Nemikolh/devsandboxes/releases/download/v0.7.0/devsandbox-0.7.0-aarch64-apple-darwin.tar.gz"
        );
        assert!(release_url("0.7.0-rc.1", SUMS).is_ok());
        for bad in ["", "0.7.0/../../x", "0.7 0", "0.7.0?x"] {
            assert!(release_url(bad, SUMS).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_curl_argv_is_https_only() {
        let argv = curl_argv("https://x/y", Path::new("/t/a.tar.gz"));
        let words: Vec<_> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        for want in [["--proto", "=https"], ["--proto-redir", "=https"], ["--max-time", "600"], ["--speed-time", "30"]] {
            assert!(words.windows(2).any(|w| w == want), "{words:?}");
        }
        assert!(words.contains(&"--fail".into()) && words.contains(&"--tlsv1.2".into()) && words.contains(&"--location".into()));
        assert_eq!(words[words.len() - 3..], ["-o", "/t/a.tar.gz", "https://x/y"]);
    }

    /// The test release key (never the real one's secret).
    fn test_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[7; 32])
    }

    /// What a fake release archive holds.
    const ARCHIVE: &[u8] = b"a release archive";

    /// A release's honest [`SUMS`] for `version`: every target's archive
    /// is [`ARCHIVE`].
    fn honest_sums(version: &str) -> Vec<u8> {
        let sum = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(ARCHIVE));
        let targets = ["aarch64-apple-darwin", "aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl"];
        targets.iter().map(|t| format!("{sum}  {}\n", asset_name(version, t))).collect::<String>().into_bytes()
    }

    /// Steps that touch nothing but a scratch dir: what each one was asked,
    /// what `build` reports per path, a step to fail, and a part of the
    /// release to tamper with (`sums`: a byte flipped after signing; `sig`:
    /// a signature one byte short; `key`: another release key; `archive`:
    /// other bytes than the sums say).
    #[derive(Default)]
    pub(crate) struct Fake {
        pub calls: Mutex<Vec<String>>,
        /// `fleet-hello` of a received (or fetched) binary and of the
        /// installed one.
        pub received_build: String,
        pub installed_build: String,
        pub fail: Option<&'static str>,
        pub tamper: Option<&'static str>,
    }

    impl Fake {
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
        /// Recorded as `fetch <url>`; the release's files by the URL's last
        /// two words, `v<version>/<file>`.
        fn download(&self, url: &str, out: &Path) -> Result<()> {
            use ed25519_dalek::Signer as _;
            self.step("fetch", url.into())?;
            let mut words = url.rsplit('/');
            let (file, version) = (words.next().unwrap(), words.next().unwrap().trim_start_matches('v'));
            let tamper = |what| self.tamper == Some(what);
            let body = match file {
                SUMS => {
                    let mut sums = honest_sums(version);
                    if tamper("sums") {
                        sums[0] ^= 1;
                    }
                    sums
                }
                SUMS_SIG => {
                    let sig = test_key().sign(&honest_sums(version)).to_bytes().to_vec();
                    if tamper("sig") { sig[..63].to_vec() } else { sig }
                }
                _ if tamper("archive") => b"another archive".to_vec(),
                _ => ARCHIVE.to_vec(),
            };
            std::fs::write(out, body).unwrap();
            Ok(())
        }
        fn unpack(&self, archive: &Path, into: &Path) -> Result<PathBuf> {
            self.step("unpack", archive.file_name().unwrap().to_string_lossy().into_owned())?;
            assert_eq!(std::fs::read(archive).unwrap(), ARCHIVE, "unpacks only what was checked");
            let binary = into.join("devsandbox");
            std::fs::write(&binary, "new").unwrap();
            Ok(binary)
        }
        fn release_key(&self) -> [u8; 32] {
            let key = if self.tamper == Some("key") { ed25519_dalek::SigningKey::from_bytes(&[8; 32]) } else { test_key() };
            key.verifying_key().to_bytes()
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

    /// The control plane's published 0.7.0, and what its release asset for this
    /// host's platform says.
    fn control_plane() -> Build {
        build("0.7.0", "aaaa", true)
    }
    const ASSET: &str = "0.7.0 aaaa published linux/x86_64";

    #[test]
    fn an_update_installs_checks_and_hands_off() {
        let dir = scratch("ok");
        // The control plane on another platform: the asset says this one.
        let fake = Fake::building(ASSET);
        update(&fake, &Build::parse("0.7.0 aaaa published macos/aarch64 control_plane k"), "linux", "x86_64", &dir).unwrap();
        let base = "https://github.com/Nemikolh/devsandboxes/releases/download/v0.7.0";
        let asset = "devsandbox-0.7.0-x86_64-unknown-linux-musl.tar.gz";
        assert_eq!(
            fake.calls(),
            [
                format!("fetch {base}/SHA256SUMS"),
                format!("fetch {base}/SHA256SUMS.sig"),
                format!("fetch {base}/{asset}"),
                format!("unpack {asset}"),
                "build received".into(),
                "install /dest/devsandbox".into(),
                "build dest".into(),
                "handoff ".into()
            ]
        );
        assert!(empty(&dir), "the scratch dir is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_download_installs_nothing() {
        let dir = scratch("fetch");
        let fake = Fake { fail: Some("fetch"), ..Fake::building(ASSET) };
        let err = update(&fake, &control_plane(), "linux", "aarch64", &dir).unwrap_err();
        assert!(format!("{err:#}").starts_with("cannot download https://github.com/"), "{err:#}");
        assert!(format!("{err:#}").ends_with("/SHA256SUMS: fetch broke"), "{err:#}");
        assert_eq!(fake.calls().len(), 1);
        assert!(empty(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unverified_release_is_never_unpacked() {
        let dir = scratch("verify");
        let cases = [
            ("sums", "release v0.7.0 isn't signed by the release key: SHA256SUMS's signature doesn't verify against the release key", 2),
            ("sig", "release v0.7.0 isn't signed by the release key: SHA256SUMS.sig is 63 bytes, not a 64-byte ed25519 signature", 2),
            ("key", "release v0.7.0 isn't signed by the release key: SHA256SUMS's signature doesn't verify against the release key", 2),
        ];
        for (tamper, want, fetches) in cases {
            let fake = Fake { tamper: Some(tamper), ..Fake::building(ASSET) };
            let err = update(&fake, &control_plane(), "linux", "x86_64", &dir).unwrap_err();
            assert_eq!(format!("{err:#}"), want, "{tamper}");
            assert_eq!(fake.calls().len(), fetches, "{tamper}: the archive isn't even downloaded: {:?}", fake.calls());
            assert!(empty(&dir));
        }
        let fake = Fake { tamper: Some("archive"), ..Fake::building(ASSET) };
        let err = format!("{:#}", update(&fake, &control_plane(), "linux", "x86_64", &dir).unwrap_err());
        assert!(err.starts_with("devsandbox-0.7.0-x86_64-unknown-linux-musl.tar.gz's sha256 is "), "{err}");
        assert!(err.contains(", not SHA256SUMS's "), "{err}");
        assert!(!fake.calls().iter().any(|c| c.starts_with("unpack")), "{:?}", fake.calls());
        assert!(empty(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sums_verify_only_with_their_key_and_bytes() {
        use ed25519_dalek::Signer as _;
        let sums = honest_sums("0.7.0");
        let sig = test_key().sign(&sums).to_bytes();
        let key = test_key().verifying_key().to_bytes();
        verify_sums(&sums, &sig, &key).unwrap();
        let mut flipped = sums.clone();
        flipped[70] ^= 0x20;
        assert!(verify_sums(&flipped, &sig, &key).is_err(), "a flipped byte");
        let mut bad_sig = sig;
        bad_sig[0] ^= 1;
        assert!(verify_sums(&sums, &bad_sig, &key).is_err(), "a flipped signature byte");
        assert_eq!(format!("{:#}", verify_sums(&sums, &sig[..63], &key).unwrap_err()), "SHA256SUMS.sig is 63 bytes, not a 64-byte ed25519 signature");
        assert!(verify_sums(&sums, &[sig.as_slice(), &[0]].concat(), &key).is_err(), "65 bytes");
        assert!(verify_sums(&sums, &[], &key).is_err(), "empty");
        let other = ed25519_dalek::SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes();
        assert_eq!(
            format!("{:#}", verify_sums(&sums, &sig, &other).unwrap_err()),
            "SHA256SUMS's signature doesn't verify against the release key"
        );
        assert!(verify_sums(&sums, &sig, &RELEASE_KEY).is_err(), "not the real key's");
    }

    /// A signature `openssl pkeyutl -sign -rawin` made (the release
    /// workflow's command), over `OPENSSL_SUMS`, with a throwaway key
    /// (`openssl genpkey -algorithm ed25519`) whose public half is
    /// `OPENSSL_PUB`: what CI makes, the member verifies.
    #[test]
    fn openssl_signatures_verify() {
        let key: [u8; 32] = hex::decode(OPENSSL_PUB).unwrap().try_into().unwrap();
        let sig = hex::decode(OPENSSL_SIG).unwrap();
        verify_sums(OPENSSL_SUMS.as_bytes(), &sig, &key).unwrap();
        let name = "devsandbox-0.9.0-aarch64-apple-darwin.tar.gz";
        assert_eq!(hex::encode(sum_of(OPENSSL_SUMS.as_bytes(), name).unwrap()), OPENSSL_SUMS[..64]);
        assert!(verify_sums(OPENSSL_SUMS.replace("0.9.0", "0.9.1").as_bytes(), &sig, &key).is_err());
    }
    const OPENSSL_PUB: &str = "b33161fae07cbbe9dfd5454999d2367ac3a1a0cf20cb17fbdfdfa886f1525066";
    const OPENSSL_SIG: &str = "111bff9864e46e0890e1066f87a384820be8909fbd554eb57cb3596dcced7b5bd18c91824dd224282cb117342971cd02f69240264734152b58d88a7b28f3160b";
    const OPENSSL_SUMS: &str = "19afaf54e1c73ed8d742a22e5ad23c0fc9875c0697df22365652435250dd2496  devsandbox-0.9.0-aarch64-apple-darwin.tar.gz\nf984e7480c12bb81a839eaad8d5036fbb7f0621371ec7186eeec726ada4eaf6f  devsandbox-0.9.0-x86_64-pc-windows-msvc.zip\n45e66ed30407ff38e59c2f40bb7bccf71fd854a42bb0c37ec6b380852d2778cf  devsandbox-0.9.0-x86_64-unknown-linux-musl.tar.gz\n";

    #[test]
    fn the_release_key_is_an_ed25519_key() {
        assert!(ed25519_dalek::VerifyingKey::from_bytes(&RELEASE_KEY).is_ok());
        assert_eq!(hex::encode(RELEASE_KEY), include_str!("../../../release-signing.pub").trim_end());
        assert_eq!(parse_key("00".repeat(32).as_str()), [0; 32], "no newline is fine");
    }

    #[test]
    fn a_sums_line_is_found_once_by_exact_name() {
        let (a, b) = ("ab".repeat(32), "cd".repeat(32));
        let sums = format!("{a}  devsandbox-0.7.0-x.tar.gz\n{b} *devsandbox-0.7.0-y.tar.gz\n");
        assert_eq!(sum_of(sums.as_bytes(), "devsandbox-0.7.0-x.tar.gz").unwrap(), [0xab; 32]);
        assert_eq!(sum_of(sums.as_bytes(), "devsandbox-0.7.0-y.tar.gz").unwrap(), [0xcd; 32], "binary mode");
        let err = |sums: &str, name| format!("{:#}", sum_of(sums.as_bytes(), name).unwrap_err());
        assert_eq!(err(&sums, "devsandbox-0.7.0-x.tar"), "SHA256SUMS doesn't list devsandbox-0.7.0-x.tar", "exact names only");
        assert_eq!(err(&sums, "x.tar.gz"), "SHA256SUMS doesn't list x.tar.gz");
        assert_eq!(err("", "x"), "SHA256SUMS doesn't list x");
        let twice = format!("{sums}{b}  devsandbox-0.7.0-x.tar.gz\n");
        assert_eq!(err(&twice, "devsandbox-0.7.0-x.tar.gz"), "SHA256SUMS lists devsandbox-0.7.0-x.tar.gz more than once");
        for bad in [format!("{a} x"), format!("{} x", &a[1..]), format!("{}  x", "zz".repeat(32)), "nonsense".into()] {
            assert!(err(&bad, "x").contains("line 1 isn't"), "{bad}");
        }
        assert_eq!(format!("{:#}", sum_of(&[0xff], "x").unwrap_err()), "SHA256SUMS isn't text");
    }

    #[test]
    fn a_wrong_build_is_a_failure_and_no_handoff() {
        let dir = scratch("version");
        // The asset isn't the control plane's build: never installed, whichever word differs.
        for asset in ["0.6.9 aaaa published linux/x86_64", "0.7.0 bbbb published linux/x86_64", "0.7.0 aaaa dev linux/x86_64"] {
            let fake = Fake::building(asset);
            let err = format!("{:#}", update(&fake, &control_plane(), "linux", "x86_64", &dir).unwrap_err());
            assert!(err.ends_with("not the control plane's 0.7.0 (published, aaaa)"), "{err}");
            assert!(err.starts_with("the downloaded binary ("), "{err}");
            assert!(!fake.calls().iter().any(|c| c.starts_with("install")), "{asset}");
        }
        // The installed binary isn't (dest resolved elsewhere): no handoff.
        let fake = Fake { installed_build: "0.8.0 cccc published linux/x86_64".into(), ..Fake::building(ASSET) };
        let err = update(&fake, &control_plane(), "linux", "x86_64", &dir).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "the installed binary (/dest/devsandbox) is devsandbox 0.8.0 (published, cccc), not the control plane's 0.7.0 (published, aaaa)"
        );
        assert!(!fake.calls().iter().any(|c| c.starts_with("handoff")));
        // One that doesn't run.
        let fake = Fake { fail: Some("build"), ..Fake::building(ASSET) };
        let err = format!("{:#}", update(&fake, &control_plane(), "linux", "x86_64", &dir).unwrap_err());
        assert!(err.starts_with("the downloaded binary (") && err.ends_with("doesn't run: build broke"), "{err}");
        // A failed handoff says so.
        let fake = Fake { fail: Some("handoff"), ..Fake::building(ASSET) };
        let err = update(&fake, &control_plane(), "linux", "x86_64", &dir).unwrap_err();
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
        let control_plane = "0.8.0 dddd dev linux/x86_64";
        received();
        let fake = Fake::building(control_plane);
        install_received(&fake, &binary, control_plane).unwrap();
        assert_eq!(std::fs::metadata(&binary).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fake.calls(), ["build received", "install /dest/devsandbox", "build dest", "handoff "]);
        // A peer link's control plane said its control-plane flag too: not part of the build.
        received();
        let fake = Fake::building(control_plane);
        install_received(&fake, &binary, &format!("{control_plane} control plane")).unwrap();
        assert_eq!(fake.calls().len(), 4);
        // Not the control plane's build: never installed.
        received();
        let fake = Fake { received_build: "0.8.0 eeee dev linux/x86_64".into(), ..Fake::building(control_plane) };
        let err = format!("{:#}", install_received(&fake, &binary, control_plane).unwrap_err());
        assert!(err.contains("reports \"0.8.0 eeee dev linux/x86_64\", not \"0.8.0 dddd dev linux/x86_64\""), "{err}");
        assert_eq!(fake.calls(), ["build received"]);
        // One that doesn't run.
        let fake = Fake { fail: Some("build"), ..Fake::building(control_plane) };
        let err = format!("{:#}", install_received(&fake, &binary, control_plane).unwrap_err());
        assert!(err.starts_with("the control plane's binary (") && err.ends_with("doesn't run: build broke"), "{err}");
        // The installed one isn't it: no handoff.
        let fake = Fake { installed_build: "0.8.0 dddd dev".into(), ..Fake::building(control_plane) };
        let err = format!("{:#}", install_received(&fake, &binary, control_plane).unwrap_err());
        assert!(err.starts_with("the installed binary (/dest/devsandbox) reports"), "{err}");
        assert!(!fake.calls().iter().any(|c| c.starts_with("handoff")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_release_no_fetch() {
        let dir = scratch("target");
        let fake = Fake::building(ASSET);
        let err = update(&fake, &control_plane(), "macos", "x86_64", &dir).unwrap_err();
        assert_eq!(format!("{err:#}"), "no devsandbox release is built for macos/x86_64");
        assert!(fake.calls().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
