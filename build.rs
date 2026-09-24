//! Embeds prebuilt devsbd helpers (docs/sandbox-helper.md). Never invokes
//! cargo itself: helpers come from `DEVSANDBOX_DEVSBD_DIR`
//! (`<arch>/devsbd`), built separately by `scripts/build-devsbd.sh` or CI.
//! A missing helper yields an empty blob so plain `cargo build`/`cargo
//! install` still work; the runtime treats that as "helper unavailable".
//!
//! The helper can't know its own hash, so the sha256 of the unpatched binary
//! is written into its `BUILD_HASH` slot (devsbd/src/main.rs) before
//! compression; `devsbd version` then reports exactly `devsbd::hash(arch)`.

use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=DEVSANDBOX_DEVSBD_DIR");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let dir = env::var_os("DEVSANDBOX_DEVSBD_DIR").map(PathBuf::from);
    for arch in ["x86_64", "aarch64"] {
        let bin = dir.as_ref().map(|d| d.join(arch).join("devsbd"));
        let raw = match &bin {
            Some(p) => {
                println!("cargo:rerun-if-changed={}", p.display());
                fs::read(p).unwrap_or_default()
            }
            None => Vec::new(),
        };
        let (zst, hash) = if raw.is_empty() {
            (Vec::new(), String::new())
        } else {
            let hash: String = Sha256::digest(&raw).iter().map(|b| format!("{b:02x}")).collect();
            let patched = patch_hash(raw, &hash, arch);
            let zst = zstd::encode_all(&patched[..], 19).expect("zstd compress devsbd");
            (zst, hash)
        };
        fs::write(out.join(format!("devsbd-{arch}.zst")), zst).unwrap();
        fs::write(out.join(format!("devsbd-{arch}.sha256")), hash).unwrap();
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
