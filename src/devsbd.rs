//! Embedded devsbd helper binaries (see docs/sandbox-helper.md and build.rs).
//! Both Linux arches are always embedded: the non-host one is the fallback
//! for emulated images. An empty blob means the helper wasn't built, and
//! callers fall back to the bind-mount path.

use std::borrow::Cow;
use std::io::Read;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// Arch a local runtime runs natively (Windows x86_64 → linux x86_64,
    /// macOS arm64 → linux aarch64).
    pub fn host() -> Arch {
        if cfg!(target_arch = "aarch64") { Arch::Aarch64 } else { Arch::X86_64 }
    }

    pub fn other(self) -> Arch {
        match self {
            Arch::X86_64 => Arch::Aarch64,
            Arch::Aarch64 => Arch::X86_64,
        }
    }
}

const X86_64_ZST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/devsbd-x86_64.zst"));
const AARCH64_ZST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/devsbd-aarch64.zst"));
const X86_64_HASH: &str = include_str!(concat!(env!("OUT_DIR"), "/devsbd-x86_64.sha256"));
const AARCH64_HASH: &str = include_str!(concat!(env!("OUT_DIR"), "/devsbd-aarch64.sha256"));

fn compressed(arch: Arch) -> &'static [u8] {
    match arch {
        Arch::X86_64 => X86_64_ZST,
        Arch::Aarch64 => AARCH64_ZST,
    }
}

/// Decompressed helper for `arch`, or `None` when it wasn't embedded.
pub fn blob(arch: Arch) -> Option<Cow<'static, [u8]>> {
    decompress(compressed(arch)).map(Cow::Owned)
}

/// sha256 (hex) of the uncompressed helper, or `None` when not embedded.
pub fn hash(arch: Arch) -> Option<&'static str> {
    let h = match arch {
        Arch::X86_64 => X86_64_HASH,
        Arch::Aarch64 => AARCH64_HASH,
    };
    (!h.is_empty()).then_some(h)
}

fn decompress(zst: &[u8]) -> Option<Vec<u8>> {
    if zst.is_empty() {
        return None;
    }
    let mut src = zst;
    let mut dec = ruzstd::decoding::StreamingDecoder::new(&mut src)
        .expect("embedded devsbd blob is valid zstd (written by build.rs)");
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("decompress embedded devsbd");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_blob_means_unavailable() {
        assert_eq!(decompress(&[]), None);
    }

    #[test]
    fn decompress_roundtrips_zstd() {
        let data = b"devsbd payload".repeat(100);
        let zst = zstd::encode_all(&data[..], 19).unwrap();
        assert_eq!(decompress(&zst).as_deref(), Some(&data[..]));
    }

    #[test]
    fn blob_and_hash_agree_on_availability() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            assert_eq!(blob(arch).is_some(), hash(arch).is_some(), "{arch:?}");
        }
    }

    #[test]
    fn embedded_blob_carries_its_hash() {
        // build.rs patches the hash into the helper's BUILD_HASH slot, which is
        // what `devsbd version` reports.
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let (Some(b), Some(h)) = (blob(arch), hash(arch)) else { continue };
            assert_eq!(h.len(), 64);
            let slot = [&b"DEVSBD_BUILD_HASH:"[..], h.as_bytes()].concat();
            assert!(b.windows(slot.len()).any(|w| w == slot), "{arch:?}");
        }
    }

    #[test]
    fn other_arch_flips() {
        assert_eq!(Arch::host().other().other(), Arch::host());
        assert_ne!(Arch::host().other(), Arch::host());
    }
}
