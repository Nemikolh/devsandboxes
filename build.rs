//! Embeds prebuilt devsbd helpers (docs/sandbox-helper.md). Never invokes
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

use sha2::{Digest, Sha256};
use std::{env, fs, path::Path, path::PathBuf};

fn main() {
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
