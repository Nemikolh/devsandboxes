//! devsbd: container-side half of devsandbox's host<->container relays.
//! argv is hand-parsed (no clap) to keep the static binary small.

mod boot;
mod bridge;
mod ctl;
mod daemon;
mod json;
mod outbox;
mod runs;

// Shared with the host so the two sides can't drift; they live in the root
// crate because devsbd/ isn't packaged (see proto.rs's module doc).
#[path = "../../src/devsbd/bootfile.rs"]
#[allow(dead_code)] // host-only serializer
mod bootfile;
#[path = "../../src/devsbd/control.rs"]
#[allow(dead_code)] // host-only decoder (`decode_request`)
mod control;
#[path = "../../src/devsbd/escape.rs"]
mod escape;
#[path = "../../src/devsbd/mux.rs"]
mod mux;
#[path = "../../src/devsbd/notify.rs"]
mod notify;
#[path = "../../src/devsbd/proto.rs"]
#[allow(dead_code)] // host-only helpers (e.g. Frame::Ping senders)
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
        Some("daemon") => exit_on_err(daemon::run(&build_hash())),
        Some("bridge") => exit_on_err(bridge::run(&build_hash())),
        Some("boot") => exit_on_err(boot::run()),
        Some("notify") => exit_on_err(outbox::run(&args.collect::<Vec<_>>())),
        // `put`/`rm` queue in the outbox (no host needed); `ls` asks the host.
        Some("thread") => {
            let args = args.collect::<Vec<_>>();
            match args.split_first() {
                Some((ls, rest)) if ls == "ls" => std::process::exit(ctl::run("thread-ls", rest)),
                _ => exit_on_err(outbox::thread(&args)),
            }
        }
        Some("events") => {
            let args = args.collect::<Vec<_>>();
            let code = match args.split_first() {
                Some((ack, rest)) if ack == "ack" => ctl::run("events-ack", rest),
                _ => ctl::run("events", &args),
            };
            std::process::exit(code)
        }
        Some(verb @ ("ensure" | "ls" | "branches" | "stop" | "rm" | "done" | "exec")) => {
            std::process::exit(ctl::run(verb, &args.collect::<Vec<_>>()))
        }
        Some("run") => {
            let args = args.collect::<Vec<_>>();
            let code = if runs::is_local(&args) { runs::run(&args) } else { ctl::run_remote(&args) };
            std::process::exit(code)
        }
        _ => {
            eprintln!(
                "usage: devsbd version|daemon|bridge|boot|notify|thread|events|ensure|ls|branches|stop|rm|done|exec|run"
            );
            std::process::exit(2);
        }
    }
}

fn exit_on_err(result: std::io::Result<()>) {
    if let Err(e) = result {
        eprintln!("devsbd: {e}");
        std::process::exit(1);
    }
}
