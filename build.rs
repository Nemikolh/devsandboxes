//! Embeds prebuilt devsbd helpers (docs/architecture/helper/overview.md). Never invokes
//! cargo itself: helpers come from `DEVSANDBOX_DEVSBD_DIR`
//! (`<arch>/devsbd`), built separately by `scripts/build-devsbd.sh` or CI.
//! A missing helper yields an empty blob so plain `cargo build`/`cargo
//! install` still work; the runtime treats that as "helper unavailable".
//! `DEVSANDBOX_DEVSBD_REQUIRED=1` turns a missing/empty helper into a build
//! error (CI, where a release must ship the helper).
//!
//! The helper can't know its own hash, so the sha256 of the unpatched binary
//! is written into its `BUILD_HASH` slot (devsbd/src/main.rs) before
//! compression; `devsbd version` then reports exactly `devsbd::hash(arch)`.
//!
//! Also stamps this build for the fleet link's `Hello`
//! (`serve::fleet::update::HELLO`): `DEVSANDBOX_BUILD_GIT`, the short git
//! hash of `HEAD` (`unknown` without git or a repo: never a build failure),
//! `DEVSANDBOX_BUILD_KIND`, `published` when `DEVSANDBOX_PUBLISHED=1`
//! (the release workflow's build of a tagged version) else `dev`, and
//! `DEVSANDBOX_BUILD_PLATFORM`, the target's `<os>/<arch>` as
//! `std::env::consts` names them (`linux/x86_64`).

use sha2::{Digest, Sha256};
use std::process::Command;
use std::{env, fs, path::Path, path::PathBuf};

fn main() {
    stamp_build();
    println!("cargo:rerun-if-env-changed=DEVSANDBOX_DEVSBD_DIR");
    println!("cargo:rerun-if-env-changed=DEVSANDBOX_DEVSBD_REQUIRED");
    let required = env::var_os("DEVSANDBOX_DEVSBD_REQUIRED").is_some_and(|v| v == "1");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let dir = env::var_os("DEVSANDBOX_DEVSBD_DIR").map(PathBuf::from);
    for arch in ["x86_64", "aarch64"] {
        let bin = dir.as_ref().map(|d| d.join(arch).join("devsbd"));
        let raw = match &bin {
            Some(p) => {
                // A `rerun-if-changed` on a missing path is *always* stale, so
                // cargo would rerun this script and recompile the crate on
                // every build. Watch the (created, empty) arch dir instead:
                // cargo rescans it and sees the helper appear. Not an
                // ancestor like `target/`, which cargo scans recursively and
                // which every build modifies.
                let arch_dir = p.parent().expect("<dir>/<arch>/devsbd has a parent");
                if p.exists() {
                    println!("cargo:rerun-if-changed={}", p.display());
                } else if fs::create_dir_all(arch_dir).is_ok() {
                    println!("cargo:rerun-if-changed={}", arch_dir.display());
                } else {
                    // Unwritable dir: fall back to always-stale on the file.
                    println!("cargo:rerun-if-changed={}", p.display());
                }
                fs::read(p).unwrap_or_default()
            }
            None => Vec::new(),
        };
        if raw.is_empty() && required {
            panic!(
                "DEVSANDBOX_DEVSBD_REQUIRED=1 but no devsbd helper for {arch}: {} is missing or \
                 empty; run scripts/build-devsbd.sh",
                bin.as_ref().map_or_else(
                    || "DEVSANDBOX_DEVSBD_DIR (unset)".to_string(),
                    |p| p.display().to_string()
                )
            );
        }
        let (zst, hash) = if raw.is_empty() {
            (Vec::new(), String::new())
        } else {
            let hash: String = Sha256::digest(&raw).iter().map(|b| format!("{b:02x}")).collect();
            let patched = patch_hash(raw, &hash, arch);
            let zst = zstd::encode_all(&patched[..], 19).expect("zstd compress devsbd");
            (zst, hash)
        };
        // Only rewrite when content changed: an unconditional write bumps the
        // file mtime, which would make `include_bytes!`'s fingerprint stale and
        // recompile the crate every build.
        write_if_changed(&out.join(format!("devsbd-{arch}.zst")), &zst);
        write_if_changed(&out.join(format!("devsbd-{arch}.sha256")), hash.as_bytes());
    }
}

/// The fleet link's build stamp (module doc). The git hash is watched through
/// `HEAD` and the ref it names (worktree-aware: `git rev-parse --git-path`),
/// plus `packed-refs`, so a commit or checkout reruns this script; a path
/// that doesn't exist isn't watched (it would be always stale).
fn stamp_build() {
    println!("cargo:rerun-if-env-changed=DEVSANDBOX_PUBLISHED");
    let published = env::var_os("DEVSANDBOX_PUBLISHED").is_some_and(|v| v == "1");
    println!("cargo:rustc-env=DEVSANDBOX_BUILD_KIND={}", if published { "published" } else { "dev" });
    let cfg = |key: &str| env::var(key).unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=DEVSANDBOX_BUILD_PLATFORM={}/{}", cfg("CARGO_CFG_TARGET_OS"), cfg("CARGO_CFG_TARGET_ARCH"));
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(env::var_os("CARGO_MANIFEST_DIR")?)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        out.status.success().then(|| text.trim().to_string()).filter(|t| !t.is_empty())
    };
    let hash = git(&["rev-parse", "--short=12", "HEAD"])
        .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=DEVSANDBOX_BUILD_GIT={hash}");
    let mut watched = vec![git(&["rev-parse", "--git-path", "HEAD"]), git(&["rev-parse", "--git-path", "packed-refs"])];
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(git(&["rev-parse", "--git-path", &head_ref]));
    }
    let root = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from).unwrap_or_default();
    for path in watched.into_iter().flatten() {
        let path = root.join(path);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// Write only when `bytes` differ from the file's current contents, so a
/// no-change build leaves the mtime (and the crate's fingerprint) untouched.
fn write_if_changed(path: &Path, bytes: &[u8]) {
    if fs::read(path).ok().as_deref() != Some(bytes) {
        fs::write(path, bytes).unwrap();
    }
}

/// Must match `BUILD_HASH` in devsbd/src/main.rs byte for byte.
const MARKER: &[u8] = b"DEVSBD_BUILD_HASH:";
const PLACEHOLDER: &[u8] = b"unpatched-devsbd-build-hash-placeholder-------------------------";

fn patch_hash(mut bin: Vec<u8>, hash: &str, arch: &str) -> Vec<u8> {
    let slot = [MARKER, PLACEHOLDER].concat();
    let hits: Vec<usize> = bin
        .windows(slot.len())
        .enumerate()
        .filter(|(_, w)| *w == slot)
        .map(|(i, _)| i)
        .collect();
    // Exactly one: zero means a stale/foreign binary, several means we can't
    // tell which copy the helper reads.
    let [at] = hits[..] else {
        panic!(
            "devsbd {arch}: expected 1 BUILD_HASH slot, found {}; rerun scripts/build-devsbd.sh",
            hits.len()
        );
    };
    let start = at + MARKER.len();
    bin[start..start + PLACEHOLDER.len()].copy_from_slice(hash.as_bytes());
    bin
}
