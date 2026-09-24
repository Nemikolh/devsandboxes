//! devsbd: container-side half of devsandbox's host<->container relays.
//! argv is hand-parsed (no clap) to keep the static binary small.

// Shared with the host so the two sides can't drift; lives in the root crate
// because devsbd/ isn't packaged (see the module doc).
#[path = "../../src/devsbd/proto.rs"]
#[allow(dead_code)] // daemon/bridge land in step 5
mod proto;

/// Build hash slot. The helper can't know its own sha256 at compile time, so
/// devsandbox's build.rs finds this marker in the binary and overwrites the 64
/// bytes after it with the sha256 (hex) of the unpatched binary — the value
/// `devsandbox` compares against on install. Keep in sync with build.rs.
#[used]
static BUILD_HASH: [u8; 82] = *b"DEVSBD_BUILD_HASH:unpatched-devsbd-build-hash-placeholder-------------------------";

fn build_hash() -> String {
    // Volatile read: without it the compiler may fold the placeholder into the
    // print and the patched bytes would never be read.
    let slot = unsafe { std::ptr::read_volatile(&BUILD_HASH) };
    String::from_utf8_lossy(&slot[18..]).into_owned()
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("version") => println!("devsbd {} {}", proto::VERSION, build_hash()),
        _ => {
            eprintln!("usage: devsbd version");
            std::process::exit(2);
        }
    }
}
