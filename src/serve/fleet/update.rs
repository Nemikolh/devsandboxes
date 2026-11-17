//! Every member but the fleet's authority follows the build the authority
//! pinned in the membership log (`membership::Pin`, the latest
//! `Op::Build`; docs/architecture/daemon/fleet.md, *Versions*). Each end of
//! a peer link says its build in its `Hello` ([`hello_of`]: [`HELLO`]'s
//! semver, git hash, `published` or `dev` and platform, then its binary's
//! sha256 prefix and whether it takes dev builds); each end compares
//! itself with the pin ([`decide`], pure) before it serves anything. A
//! published pin this host isn't on is downloaded from the GitHub release
//! and installed over this daemon's binary, which then takes over by a
//! forced handoff ([`update`] over [`Steps`]); the release's `SHA256SUMS`
//! must hash to the pin's and carry a signature by [`RELEASE_KEY`], and the
//! archive's sha256 must be its line (ADR 0047). A dev pin comes over a
//! link instead, from a peer that has it ([`pushes`], [`push`]), and a
//! host taking dev builds on the pin's platform installs it once its
//! sha256 is the pin's ([`install_received`]); a host that can't or won't
//! take the pin is behind ([`Decision::Behind`]) and keeps linking. One
//! update at a time per host ([`lock_update`]).
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

use super::membership::Pin;
use super::transfer;
use crate::commands::fleet_link::Authority;
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
        Ok(sha) => sha_word(&sha),
        Err(_) => "-".into(),
    }
}

/// A sha256 as a `Hello` says it: its first [`SHA_WORD`] hex digits.
pub fn sha_word(sha256: &[u8; 32]) -> String {
    hex::encode(sha256)[..SHA_WORD].to_string()
}

/// The sha256 prefix a peer link's `Hello` hash ([`hello_of`]) says its
/// end's binary has: its seventh word; `None` for `-` or none.
pub fn sha_of(hello: &str) -> Option<&str> {
    hello.split_whitespace().nth(6).filter(|w| *w != "-")
}

/// Whether a peer link's `Hello` hash ([`hello_of`]) says its end takes
/// dev builds: its eighth word is `dev`. A `Hello` without one doesn't.
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

/// The transfer kind of a pinned dev build's binary, and the most a member takes of it.
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

/// What a host does about the fleet's pinned build on one peer link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// On the pin, nothing to follow, or nothing to do on this link: serve
    /// it.
    Same,
    /// The pin is release `semver`, whose `SHA256SUMS` hashes to
    /// `sums_sha256`, and this host runs another build: fetch and install
    /// its release asset ([`update`]), then hand off (an older version too).
    Download { semver: String, sums_sha256: [u8; 32] },
    /// The pin is a dev build this host isn't on, for this platform, this
    /// host takes dev builds and the peer has the pin: take the binary it
    /// pushes ([`pushes`]), install it ([`install_received`]), then hand off.
    Receive,
    /// This host can't or won't take the pin (another platform, no release
    /// target, dev builds not taken): it keeps its build and the link is
    /// served as [`Decision::Same`], with why.
    Behind(String),
}

impl Decision {
    /// Why this host is behind, on [`Decision::Behind`].
    pub fn behind(&self) -> Option<&str> {
        match self {
            Decision::Behind(why) => Some(why),
            _ => None,
        }
    }
}

/// Who is behind the fleet's pin on one peer link, as that link's end sees
/// it from both `Hello`s: this host ([`decide`]'s [`Decision::Behind`]) and
/// the peer ([`peer_behind`]), each with why. A behind link is still up: it
/// is served as on [`Decision::Same`] (docs/architecture/daemon/fleet.md,
/// *Versions*).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Behind {
    pub here: Option<String>,
    pub peer: Option<String>,
}

/// Why the peer saying `theirs` is behind the fleet's `pin` on its link to
/// the end saying `ours` (pure): [`decide`] over the peer's own `Hello`, its
/// dev builds as its eighth word says ([`takes_dev_builds`]), and the
/// mirrored `authority`, as [`pushes`] mirrors it; `None` when it isn't.
pub fn peer_behind(pin: Option<&Pin>, ours: &str, theirs: &str, authority: Authority) -> Option<String> {
    let mirrored = Authority { ours: authority.theirs, theirs: authority.ours };
    match decide(theirs, takes_dev_builds(theirs), mirrored, pin, ours) {
        Decision::Behind(why) => Some(why),
        _ => None,
    }
}

/// Whether a `Hello` hash says release `semver`: published, that semver.
fn on_release(hello: &str, semver: &str) -> bool {
    let build = Build::parse(hello);
    build.published && build.semver == semver
}

/// What the end saying `ours` (a peer link `Hello`, [`hello_of`]) does
/// about the fleet's `pin` on its link to the peer saying `theirs` (pure).
/// The authority (`authority.ours`) follows no one, and with no pin there
/// is nothing to follow: [`Decision::Same`].
///
/// - A published pin: on it when this host runs that release. Else, with
///   no release asset for this platform ([`platform_target`]), behind
///   ([`update`] would fail the same way at every link). Else
///   [`Decision::Download`], but only on a link whose peer is on the pin
///   or is the authority: one link starts the download, not each.
/// - A dev pin: on it when this binary's sha256 prefix ([`sha_of`]) is the
///   pin's (a build is its bytes; the label is for display). Else behind on
///   another platform (checked first: `dev_builds` can't help there) or
///   without `dev_builds` (`member.toml`'s `dev-builds`, or trust `full`:
///   `fleet::member::Member::takes_dev_builds`). Else [`Decision::Receive`]
///   when the peer has the pin (its sha256 prefix is the pin's, or it is
///   the authority, which keeps a copy of what it pinned), else `Same`: a
///   peer that has it pushes it ([`pushes`] is this rule's mirror).
pub fn decide(ours: &str, dev_builds: bool, authority: Authority, pin: Option<&Pin>, theirs: &str) -> Decision {
    let Some(pin) = pin.filter(|_| !authority.ours) else {
        return Decision::Same;
    };
    let platform = platform_of(ours);
    match pin {
        Pin::Published { semver, sums_sha256 } => {
            if on_release(ours, semver) {
                Decision::Same
            } else if platform_target(platform).is_none() {
                Decision::Behind(format!("published build: no devsandbox release is built for {platform} (the fleet's build is {pin})"))
            } else if authority.theirs || on_release(theirs, semver) {
                Decision::Download { semver: semver.clone(), sums_sha256: *sums_sha256 }
            } else {
                Decision::Same
            }
        }
        Pin::Dev { platform: pinned, sha256, .. } => {
            let word = sha_word(sha256);
            if sha_of(ours) == Some(word.as_str()) {
                Decision::Same
            } else if pinned != platform {
                Decision::Behind(format!("dev build: can't run on {platform} (the fleet's build is {pin})"))
            } else if !dev_builds {
                Decision::Behind(format!(
                    "dev build: not taken (`dev-builds = true` in member.toml or `devsandbox machine trust full` takes it; the fleet's build is {pin})"
                ))
            } else if authority.theirs || sha_of(theirs) == Some(word.as_str()) {
                Decision::Receive
            } else {
                Decision::Same
            }
        }
    }
}

/// Which binary an end pushes ([`pushes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Its own (`commands::fleet_link::own_binary`): it is on the pin.
    Own,
    /// The authority's kept copy of the pin, `<builds_dir>/<sha256 hex>`
    /// ([`keep_build`]): the authority runs another build since.
    Kept { sha256: [u8; 32] },
}

impl Source {
    /// The file to push; `builds` is [`builds_dir`].
    pub fn path(&self, builds: &Path) -> std::io::Result<PathBuf> {
        match self {
            Source::Own => crate::commands::fleet_link::own_binary(),
            Source::Kept { sha256 } => Ok(builds.join(hex::encode(sha256))),
        }
    }
}

/// Whether the end saying `ours` pushes the fleet's dev `pin` to the peer
/// saying `theirs` on their link, and which binary (pure): when this end has
/// the pin (its sha256 prefix is the pin's: [`Source::Own`]; or it is the
/// authority: [`Source::Kept`]) and the peer would [`Decision::Receive`] it
/// by [`decide`] over its own `Hello` (its dev builds as its eighth word
/// says, [`takes_dev_builds`]) and the mirrored `authority`. Both ends thus
/// agree from the `Hello`s alone, and nothing is sent to a host that won't
/// take it; a published pin is downloaded, never pushed.
pub fn pushes(pin: Option<&Pin>, ours: &str, theirs: &str, authority: Authority) -> Option<Source> {
    let Some(Pin::Dev { sha256, .. }) = pin else {
        return None;
    };
    let source = if sha_of(ours) == Some(sha_word(sha256).as_str()) {
        Source::Own
    } else if authority.ours {
        Source::Kept { sha256: *sha256 }
    } else {
        return None;
    };
    let mirrored = Authority { ours: authority.theirs, theirs: authority.ours };
    (decide(theirs, takes_dev_builds(theirs), mirrored, pin, ours) == Decision::Receive).then_some(source)
}

/// The pushing end of a dev pin's update: `binary` as one [`BINARY_KIND`]
/// transfer, a `TRANSFER` stream through the link's socket at `socket`
/// (`endpoint::fleet_socket`), named `devsandbox`.
pub fn push(socket: &Path, binary: &Path) -> Result<()> {
    let open = || std::fs::File::open(binary).with_context(|| format!("cannot read {}", binary.display()));
    let (size, sha256) = transfer::digest(open()?).with_context(|| format!("cannot read {}", binary.display()))?;
    let header = transfer::Header { kind: BINARY_KIND.into(), name: "devsandbox".into(), size, sha256, to: None, repo: None, refs: None };
    let stream = super::link::connect(socket, crate::devsbd::proto::channel::TRANSFER).with_context(|| format!("cannot connect to {}", socket.display()))?;
    transfer::send(stream, &header, open()?)
}

/// This host's update lock (`endpoint::update_lock_path` in the socket dir
/// `dir`, `flock`ed): held from a link's [`Decision::Download`] or
/// [`Decision::Receive`] to the handoff, so one update runs at a time
/// whichever links decided one. `None` while another holds it.
pub fn lock_update(dir: &Path) -> Result<Option<std::fs::File>> {
    let path = endpoint::update_lock_path(dir);
    let file = std::fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("cannot lock {}", path.display())),
    }
}

/// `decision` with this host's update lock ([`lock_update`]) when it is an
/// update: the lock, to hold until the handoff; when another link holds it
/// (or it can't be taken), [`Decision::Same`] and why, for the log.
pub fn under_lock(decision: Decision, dir: &Path) -> (Decision, Option<std::fs::File>, Option<String>) {
    if !matches!(decision, Decision::Download { .. } | Decision::Receive) {
        return (decision, None, None);
    }
    match lock_update(dir) {
        Ok(Some(lock)) => (decision, Some(lock), None),
        Ok(None) => (Decision::Same, None, Some("an update is already under way".into())),
        Err(e) => (Decision::Same, None, Some(format!("no update: {e:#}"))),
    }
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
/// comes from the fleet's pin: only a semver's characters go in the URL.
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

/// Install pinned release `semver` over this daemon's binary and hand off
/// to it: fetch its release asset for `os`/`arch` into a scratch dir under
/// `scratch` ([`fetch_verified`]: checksums hashing to the pin's
/// `sums_sha256` and signed first), check that its `fleet-hello` is that
/// release (published, `semver`: else the new daemon would download it
/// again at every link), install it at [`Steps::dest`], check the installed
/// binary is that release too, then [`Steps::handoff`]. The scratch dir is
/// removed whatever happens. [`decide`] never says `Download` where
/// `os`/`arch` has no release; the check here stands in case it does.
pub fn update(steps: &dyn Steps, semver: &str, sums_sha256: &[u8; 32], os: &str, arch: &str, scratch: &Path) -> Result<()> {
    let target = release_target(os, arch).with_context(|| format!("no devsandbox release is built for {os}/{arch}"))?;
    let name = asset_name(semver, target);
    let dir = scratch_dir(scratch)?;
    let installed = (|| {
        let binary = fetch_verified(steps, semver, sums_sha256, &name, &dir)?;
        check_release(steps, &binary, semver, "the downloaded binary")?;
        let dest = steps.dest()?;
        steps.install(&binary, &dest)?;
        check_release(steps, &dest, semver, "the installed binary")
    })();
    let _ = std::fs::remove_dir_all(&dir);
    installed?;
    steps.handoff().context("cannot hand this daemon off to it")
}

/// Install the fleet's dev `pin`, received at `binary`, over this daemon's
/// binary and hand off to it: its sha256 must be the pin's before anything
/// else (a binary from a peer is never run otherwise), then it is made
/// executable, its `fleet-hello` must say the pin's build (semver, git,
/// `dev`, platform: [`HELLO`]'s four words), it is installed at
/// [`Steps::dest`], the installed binary must say it too, then
/// [`Steps::handoff`]. Removing `binary`'s scratch dir is the caller's.
pub fn install_received(steps: &dyn Steps, binary: &Path, pin: &Pin) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let Pin::Dev { semver, git, platform, sha256 } = pin else {
        bail!("the fleet's build is {pin}: a release is downloaded, not received");
    };
    let got = sha256_of(binary)?;
    if got != *sha256 {
        bail!("the received binary's sha256 is {}, not the fleet's build's {}", hex::encode(got), hex::encode(sha256));
    }
    std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("cannot make {} executable", binary.display()))?;
    let want = format!("{semver} {git} dev {platform}");
    check_build(steps, binary, &want, "the received binary")?;
    let dest = steps.dest()?;
    steps.install(binary, &dest)?;
    check_build(steps, &dest, &want, "the installed binary")?;
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
/// [`Steps::release_key`] ([`fetch_sums`]), [`SUMS`]'s sha256 checked
/// against the pin's `sums_sha256`, `name`'s line found ([`sum_of`]), only
/// then the archive downloaded, its sha256 checked, and unpacked. No
/// signature, no install: there is no unsigned path.
fn fetch_verified(steps: &dyn Steps, version: &str, sums_sha256: &[u8; 32], name: &str, dir: &Path) -> Result<PathBuf> {
    use sha2::Digest;
    let sums = fetch_sums(steps, version, dir)?;
    let got: [u8; 32] = sha2::Sha256::digest(&sums).into();
    if got != *sums_sha256 {
        bail!("release v{version}'s {SUMS} hashes to {}, not the fleet's build's {}", hex::encode(got), hex::encode(sums_sha256));
    }
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

/// `binary`'s `fleet-hello` is release `semver`: published, that semver
/// (its platform this host's; the pin binds every platform's asset).
fn check_release(steps: &dyn Steps, binary: &Path, semver: &str, what: &str) -> Result<()> {
    let got = steps.build(binary).with_context(|| format!("{what} ({}) doesn't run", binary.display()))?;
    if !on_release(&got, semver) {
        bail!("{what} ({}) is devsandbox {}, not the fleet's release {semver}", binary.display(), Build::parse(&got).label());
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

    const LX: &str = "linux/x86_64";

    /// A peer link `Hello` of `build` (`<semver> <git> <kind>`) on
    /// `platform`, its binary's sha256 prefix `sha` and dev builds `dev`.
    fn hello(build: &str, platform: &str, sha: &str, dev: bool) -> String {
        format!("{build} {platform} member k {sha} {}", if dev { "dev" } else { "nodev" })
    }

    fn dev_pin(h: u8) -> Pin {
        Pin::Dev { semver: "0.8.0".into(), git: "dddd".into(), platform: LX.into(), sha256: [h; 32] }
    }

    fn published_pin(semver: &str) -> Pin {
        Pin::Published { semver: semver.into(), sums_sha256: [5; 32] }
    }

    const MEMBER: Authority = Authority { ours: false, theirs: false };
    const TO_AUTHORITY: Authority = Authority { ours: false, theirs: true };
    const AUTHORITY: Authority = Authority { ours: true, theirs: false };

    #[test]
    fn the_authority_and_a_fleet_without_a_pin_follow_nothing() {
        let ours = hello("0.1.0 aaaa dev", LX, "1111111111111111", true);
        let peer = hello("0.9.0 bbbb dev", LX, &sha_word(&[7; 32]), true);
        let elsewhere = Pin::Dev { semver: "0.8.0".into(), git: "dddd".into(), platform: "other/os".into(), sha256: [7; 32] };
        for pin in [dev_pin(7), published_pin("0.9.0"), elsewhere] {
            for dev in [false, true] {
                assert_eq!(decide(&ours, dev, AUTHORITY, Some(&pin), &peer), Decision::Same, "{pin}, dev builds {dev}");
            }
        }
        assert_eq!(decide(&ours, true, MEMBER, None, &peer), Decision::Same, "no pin");
        assert_eq!(decide(&ours, true, TO_AUTHORITY, None, &peer), Decision::Same, "no pin, the authority's peer");
    }

    #[test]
    fn a_published_pin_is_downloaded_from_a_link_to_a_peer_on_it() {
        let pin = published_pin("0.9.0");
        let sums = [5; 32];
        let download = Decision::Download { semver: "0.9.0".into(), sums_sha256: sums };
        let on_it = hello("0.9.0 aaaa published", "macos/aarch64", "-", false);
        let off_it = hello("0.8.0 aaaa published", LX, "-", false);
        // On it: this host runs that release, whatever its platform or commit.
        assert_eq!(decide(&hello("0.9.0 ffff published", LX, "-", false), false, MEMBER, Some(&pin), &off_it), Decision::Same);
        // Not on it: a dev build of the same semver, another release, an older one.
        for ours in ["0.9.0 aaaa dev", "0.8.0 aaaa published", "1.0.0 aaaa published"] {
            let ours = hello(ours, LX, "-", false);
            assert_eq!(decide(&ours, false, MEMBER, Some(&pin), &on_it), download, "{ours}: from a peer on it");
            assert_eq!(decide(&ours, false, TO_AUTHORITY, Some(&pin), &off_it), download, "{ours}: from the authority");
            assert_eq!(decide(&ours, false, MEMBER, Some(&pin), &off_it), Decision::Same, "{ours}: not this link");
            assert_eq!(decide(&ours, false, MEMBER, Some(&pin), &hello("0.9.0 aaaa dev", LX, "-", false)), Decision::Same, "a dev 0.9.0 isn't on it");
        }
        // No release for this platform: behind, on any link.
        let ours = hello("0.8.0 aaaa published", "macos/x86_64", "-", false);
        let behind = Decision::Behind(format!("published build: no devsandbox release is built for macos/x86_64 (the fleet's build is {pin})"));
        assert_eq!(decide(&ours, true, TO_AUTHORITY, Some(&pin), &on_it), behind);
        assert!(matches!(decide("0.8.0 aaaa published", true, MEMBER, Some(&pin), &on_it), Decision::Behind(_)), "no platform word");
        assert_eq!(decide(&hello("0.9.0 aaaa published", "macos/x86_64", "-", false), true, MEMBER, Some(&pin), &on_it), Decision::Same, "on it needs none");
    }

    #[test]
    fn a_dev_pin_is_received_from_a_peer_that_has_it() {
        let pin = dev_pin(7);
        let has = sha_word(&[7; 32]);
        let ours = hello("0.8.0 dddd dev", LX, "1111111111111111", true);
        let peer_on_it = hello("0.8.0 dddd dev", "macos/aarch64", &has, false);
        let peer_off_it = hello("0.8.0 dddd dev", LX, "2222222222222222", true);
        // On it: the bytes, not the label.
        assert_eq!(decide(&hello("0.1.0 x published", LX, &has, false), false, MEMBER, Some(&pin), &peer_off_it), Decision::Same);
        assert_eq!(decide(&ours, true, MEMBER, Some(&pin), &peer_on_it), Decision::Receive, "the same label, other bytes");
        assert_eq!(decide(&ours, true, TO_AUTHORITY, Some(&pin), &peer_off_it), Decision::Receive, "the authority keeps a copy");
        assert_eq!(decide(&ours, true, MEMBER, Some(&pin), &peer_off_it), Decision::Same, "a peer without it: wait for one that has it");
        assert_eq!(decide(&ours, true, MEMBER, Some(&pin), "0.8.0 dddd dev linux/x86_64"), Decision::Same, "a hello without the words");
        // Dev builds not taken: behind, saying how to opt in.
        let not_taken = format!(
            "dev build: not taken (`dev-builds = true` in member.toml or `devsandbox machine trust full` takes it; the fleet's build is {pin})"
        );
        assert_eq!(decide(&ours, false, TO_AUTHORITY, Some(&pin), &peer_on_it), Decision::Behind(not_taken));
        // Another platform: behind, checked first.
        let mac = hello("0.8.0 dddd dev", "macos/aarch64", "1111111111111111", true);
        let elsewhere = Decision::Behind(format!("dev build: can't run on macos/aarch64 (the fleet's build is {pin})"));
        assert_eq!(decide(&mac, true, TO_AUTHORITY, Some(&pin), &peer_on_it), elsewhere);
        assert_eq!(decide(&mac, false, TO_AUTHORITY, Some(&pin), &peer_on_it), elsewhere, "dev-builds can't help");
        assert!(matches!(decide("0.8.0 dddd dev", true, MEMBER, Some(&pin), &peer_on_it), Decision::Behind(_)), "no platform word");
    }

    /// The end that has the pin pushes it exactly when the other would
    /// receive it: both from the `Hello`s alone.
    #[test]
    fn a_peer_that_has_the_pin_pushes_it_to_one_that_would_receive_it() {
        let pin = dev_pin(7);
        let has = sha_word(&[7; 32]);
        let on_it = hello("0.8.0 dddd dev", LX, &has, false);
        let taker = hello("0.7.0 eeee dev", LX, "1111111111111111", true);
        assert_eq!(pushes(Some(&pin), &on_it, &taker, MEMBER), Some(Source::Own));
        assert_eq!(pushes(Some(&pin), &taker, &on_it, MEMBER), None, "the other way");
        assert_eq!(pushes(Some(&pin), &on_it, &hello("0.7.0 eeee dev", LX, "1111111111111111", false), MEMBER), None, "takes no dev builds");
        assert_eq!(pushes(Some(&pin), &on_it, &hello("0.7.0 eeee dev", "macos/aarch64", "1111111111111111", true), MEMBER), None, "another platform");
        assert_eq!(pushes(Some(&pin), &on_it, &hello("0.8.0 dddd dev", LX, &has, true), MEMBER), None, "on it already");
        assert_eq!(pushes(Some(&pin), &on_it, &taker, TO_AUTHORITY), None, "never to the authority");
        // The authority on another build since: its kept copy.
        let moved_on = hello("0.9.0 ffff dev", LX, "3333333333333333", false);
        assert_eq!(pushes(Some(&pin), &moved_on, &taker, AUTHORITY), Some(Source::Kept { sha256: [7; 32] }));
        assert_eq!(pushes(Some(&pin), &on_it, &taker, AUTHORITY), Some(Source::Own), "on it: its own");
        assert_eq!(pushes(Some(&pin), &moved_on, &taker, MEMBER), None, "a member off the pin has nothing to push");
        assert_eq!(pushes(Some(&published_pin("0.9.0")), &on_it, &taker, MEMBER), None, "a release is downloaded");
        assert_eq!(pushes(None, &on_it, &taker, MEMBER), None);
        let dir = Path::new("/kept");
        assert_eq!(Source::Kept { sha256: [7; 32] }.path(dir).unwrap(), dir.join(hex::encode([7; 32])));
        assert_eq!(Source::Own.path(dir).unwrap(), crate::commands::fleet_link::own_binary().unwrap());

        // Symmetry: every pair of hellos, every authority, both pins.
        let shas = ["1111111111111111".to_string(), has.clone(), "-".into()];
        let mut hellos = Vec::new();
        for build in ["0.8.0 dddd dev", "0.9.0 aaaa published"] {
            for platform in [LX, "macos/aarch64"] {
                for sha in &shas {
                    for dev in [false, true] {
                        hellos.push(hello(build, platform, sha, dev));
                    }
                }
            }
        }
        let flip = |a: Authority| Authority { ours: a.theirs, theirs: a.ours };
        for pin in [dev_pin(7), published_pin("0.9.0")] {
            for a in [MEMBER, TO_AUTHORITY, AUTHORITY] {
                for x in &hellos {
                    for y in &hellos {
                        let receives = decide(y, takes_dev_builds(y), flip(a), Some(&pin), x) == Decision::Receive;
                        assert_eq!(pushes(Some(&pin), x, y, a).is_some(), receives, "{pin} {a:?}: {x} → {y}");
                    }
                }
            }
        }
    }

    /// Each end says the other behind exactly when the other's own
    /// decision does: both from the `Hello`s alone.
    #[test]
    fn each_end_knows_whether_the_peer_is_behind() {
        let pin = dev_pin(7);
        let has = sha_word(&[7; 32]);
        let on_it = hello("0.8.0 dddd dev", LX, &has, false);
        let mac = hello("0.8.0 dddd dev", "macos/aarch64", "1111111111111111", true);
        let elsewhere = format!("dev build: can't run on macos/aarch64 (the fleet's build is {pin})");
        assert_eq!(peer_behind(Some(&pin), &on_it, &mac, MEMBER), Some(elsewhere.clone()));
        assert_eq!(peer_behind(Some(&pin), &mac, &on_it, MEMBER), None, "the other way: on it");
        assert_eq!(decide(&mac, true, MEMBER, Some(&pin), &on_it).behind(), Some(elsewhere.as_str()), "the behind end's own word");
        // Its dev builds as its `Hello` says.
        let nodev = hello("0.7.0 eeee dev", LX, "1111111111111111", false);
        assert!(peer_behind(Some(&pin), &on_it, &nodev, MEMBER).unwrap().starts_with("dev build: not taken"));
        assert_eq!(peer_behind(Some(&pin), &on_it, &hello("0.7.0 eeee dev", LX, "1111111111111111", true), MEMBER), None, "it takes it");
        assert_eq!(peer_behind(Some(&pin), &on_it, &mac, TO_AUTHORITY), None, "the authority is never behind");
        assert_eq!(peer_behind(None, &on_it, &mac, MEMBER), None, "no pin");
        assert_eq!(Decision::Same.behind(), None);

        // Symmetry with `decide`, as `pushes`' test.
        let mut hellos = Vec::new();
        for build in ["0.8.0 dddd dev", "0.9.0 aaaa published"] {
            for platform in [LX, "macos/aarch64", "other/os"] {
                for sha in ["1111111111111111", has.as_str()] {
                    for dev in [false, true] {
                        hellos.push(hello(build, platform, sha, dev));
                    }
                }
            }
        }
        let flip = |a: Authority| Authority { ours: a.theirs, theirs: a.ours };
        for pin in [dev_pin(7), published_pin("0.9.0")] {
            for a in [MEMBER, TO_AUTHORITY, AUTHORITY] {
                for x in &hellos {
                    for y in &hellos {
                        let theirs = decide(y, takes_dev_builds(y), flip(a), Some(&pin), x);
                        assert_eq!(peer_behind(Some(&pin), x, y, a).as_deref(), theirs.behind(), "{pin} {a:?}: {x} sees {y}");
                        let ours = decide(x, takes_dev_builds(x), a, Some(&pin), y);
                        assert_eq!(peer_behind(Some(&pin), y, x, flip(a)).as_deref(), ours.behind(), "{pin} {a:?}: {y} sees {x}");
                    }
                }
            }
        }
    }

    #[test]
    fn one_update_at_a_time() {
        let dir = scratch("lock");
        let first = lock_update(&dir).unwrap().expect("free");
        assert!(lock_update(&dir).unwrap().is_none(), "held");
        let (d, lock, why) = under_lock(Decision::Receive, &dir);
        assert_eq!((d, lock.is_none(), why.as_deref()), (Decision::Same, true, Some("an update is already under way")));
        let (d, lock, why) = under_lock(Decision::Behind("x".into()), &dir);
        assert_eq!((d, lock.is_none(), why), (Decision::Behind("x".into()), true, None), "no update, no lock");
        drop(first);
        let (d, lock, why) = under_lock(Decision::Receive, &dir);
        assert_eq!((d, lock.is_some(), why), (Decision::Receive, true, None));
        assert!(lock_update(&dir).unwrap().is_none(), "held until dropped");
        drop(lock);
        let download = Decision::Download { semver: "0.9.0".into(), sums_sha256: [0; 32] };
        assert!(under_lock(download, &dir).1.is_some());
        let _ = std::fs::remove_dir_all(&dir);
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
    pub(crate) fn honest_sums(version: &str) -> Vec<u8> {
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

    /// The sha256 of [`honest_sums`] of `version`: what its pin says.
    pub(crate) fn sums_sha256(version: &str) -> [u8; 32] {
        <sha2::Sha256 as sha2::Digest>::digest(honest_sums(version)).into()
    }

    /// [`update`] to the pinned release 0.7.0.
    fn update_to(fake: &Fake, os: &str, arch: &str, dir: &Path) -> Result<()> {
        update(fake, "0.7.0", &sums_sha256("0.7.0"), os, arch, dir)
    }

    /// What release 0.7.0's asset for this host's platform says.
    const ASSET: &str = "0.7.0 aaaa published linux/x86_64";

    #[test]
    fn an_update_installs_checks_and_hands_off() {
        let dir = scratch("ok");
        let fake = Fake::building(ASSET);
        update_to(&fake, "linux", "x86_64", &dir).unwrap();
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
        let err = update_to(&fake, "linux", "aarch64", &dir).unwrap_err();
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
            let err = update_to(&fake, "linux", "x86_64", &dir).unwrap_err();
            assert_eq!(format!("{err:#}"), want, "{tamper}");
            assert_eq!(fake.calls().len(), fetches, "{tamper}: the archive isn't even downloaded: {:?}", fake.calls());
            assert!(empty(&dir));
        }
        let fake = Fake { tamper: Some("archive"), ..Fake::building(ASSET) };
        let err = format!("{:#}", update_to(&fake, "linux", "x86_64", &dir).unwrap_err());
        assert!(err.starts_with("devsandbox-0.7.0-x86_64-unknown-linux-musl.tar.gz's sha256 is "), "{err}");
        assert!(err.contains(", not SHA256SUMS's "), "{err}");
        assert!(!fake.calls().iter().any(|c| c.starts_with("unpack")), "{:?}", fake.calls());
        assert!(empty(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Signed checksums that aren't the ones the fleet pinned (another
    /// build of that release, re-signed): refused before the archive.
    #[test]
    fn sums_other_than_the_pins_are_refused() {
        let dir = scratch("pinned-sums");
        let fake = Fake::building(ASSET);
        let err = format!("{:#}", update(&fake, "0.7.0", &[9; 32], "linux", "x86_64", &dir).unwrap_err());
        let got = hex::encode(sums_sha256("0.7.0"));
        assert_eq!(err, format!("release v0.7.0's SHA256SUMS hashes to {got}, not the fleet's build's {}", hex::encode([9; 32])));
        assert_eq!(fake.calls().len(), 2, "the archive isn't even downloaded: {:?}", fake.calls());
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
        // The asset isn't the pinned release: never installed.
        for asset in ["0.6.9 aaaa published linux/x86_64", "0.7.0 aaaa dev linux/x86_64"] {
            let fake = Fake::building(asset);
            let err = format!("{:#}", update_to(&fake, "linux", "x86_64", &dir).unwrap_err());
            assert!(err.ends_with("not the fleet's release 0.7.0"), "{err}");
            assert!(err.starts_with("the downloaded binary ("), "{err}");
            assert!(!fake.calls().iter().any(|c| c.starts_with("install")), "{asset}");
        }
        // The installed binary isn't (dest resolved elsewhere): no handoff.
        let fake = Fake { installed_build: "0.8.0 cccc published linux/x86_64".into(), ..Fake::building(ASSET) };
        let err = update_to(&fake, "linux", "x86_64", &dir).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "the installed binary (/dest/devsandbox) is devsandbox 0.8.0 (published, cccc), not the fleet's release 0.7.0"
        );
        assert!(!fake.calls().iter().any(|c| c.starts_with("handoff")));
        // One that doesn't run.
        let fake = Fake { fail: Some("build"), ..Fake::building(ASSET) };
        let err = format!("{:#}", update_to(&fake, "linux", "x86_64", &dir).unwrap_err());
        assert!(err.starts_with("the downloaded binary (") && err.ends_with("doesn't run: build broke"), "{err}");
        // A failed handoff says so.
        let fake = Fake { fail: Some("handoff"), ..Fake::building(ASSET) };
        let err = update_to(&fake, "linux", "x86_64", &dir).unwrap_err();
        assert_eq!(format!("{err:#}"), "cannot hand this daemon off to it: handoff broke");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_received_build_is_hashed_then_made_executable_checked_installed_and_handed_off_to() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("received");
        let binary = dir.join("devsandbox");
        let received = |bytes: &str| {
            std::fs::write(&binary, bytes).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        let mode = || std::fs::metadata(&binary).unwrap().permissions().mode() & 0o777;
        let pin = Pin::Dev { semver: "0.8.0".into(), git: "dddd".into(), platform: LX.into(), sha256: <sha2::Sha256 as sha2::Digest>::digest(b"new").into() };
        let says = "0.8.0 dddd dev linux/x86_64";
        received("new");
        let fake = Fake::building(says);
        install_received(&fake, &binary, &pin).unwrap();
        assert_eq!(mode(), 0o755);
        assert_eq!(fake.calls(), ["build received", "install /dest/devsandbox", "build dest", "handoff "]);
        // Other bytes than the pin's: refused before anything runs, not even made executable.
        received("evil");
        let fake = Fake::building(says);
        let err = format!("{:#}", install_received(&fake, &binary, &pin).unwrap_err());
        assert!(err.starts_with("the received binary's sha256 is ") && err.contains(", not the fleet's build's "), "{err}");
        assert!(fake.calls().is_empty(), "never run: {:?}", fake.calls());
        assert_eq!(mode(), 0o600);
        // The pin's bytes saying another build: never installed.
        received("new");
        let fake = Fake { received_build: "0.8.0 eeee dev linux/x86_64".into(), ..Fake::building(says) };
        let err = format!("{:#}", install_received(&fake, &binary, &pin).unwrap_err());
        assert!(err.contains("reports \"0.8.0 eeee dev linux/x86_64\", not \"0.8.0 dddd dev linux/x86_64\""), "{err}");
        assert_eq!(fake.calls(), ["build received"]);
        // One that doesn't run.
        let fake = Fake { fail: Some("build"), ..Fake::building(says) };
        let err = format!("{:#}", install_received(&fake, &binary, &pin).unwrap_err());
        assert!(err.starts_with("the received binary (") && err.ends_with("doesn't run: build broke"), "{err}");
        // The installed one isn't it: no handoff.
        let fake = Fake { installed_build: "0.8.0 dddd dev".into(), ..Fake::building(says) };
        let err = format!("{:#}", install_received(&fake, &binary, &pin).unwrap_err());
        assert!(err.starts_with("the installed binary (/dest/devsandbox) reports"), "{err}");
        assert!(!fake.calls().iter().any(|c| c.starts_with("handoff")));
        // A published pin is never received.
        let fake = Fake::building(says);
        assert!(install_received(&fake, &binary, &published_pin("0.9.0")).is_err());
        assert!(fake.calls().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_release_no_fetch() {
        let dir = scratch("target");
        let fake = Fake::building(ASSET);
        let err = update_to(&fake, "macos", "x86_64", &dir).unwrap_err();
        assert_eq!(format!("{err:#}"), "no devsandbox release is built for macos/x86_64");
        assert!(fake.calls().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
