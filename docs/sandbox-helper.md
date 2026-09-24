# sandbox-helper: an in-container binary shipped inside devsandbox

## Goal

Ship a tiny static Linux binary (working name `devsbd`) **embedded in the `devsandbox` executable**, written into every sandbox container by devsandbox, and used as the container-side half of host↔container relays:

1. **SSH agent forwarding over exec stdio** — the relay `docs/ssh-agent.md`
   keeps deferring (_stale-socket caveat_, _Future: Apple container_,
   _Windows (native)_). One mechanism, every runtime, no socket bind mounts,
   no staleness.

2. **API proxying** (later, separate plan) — forward requests (Sentry,
   api.github.com, …) to a global devsandbox-owned service container that holds
   the credentials, with TLS interception via an injected CA.

This plan covers **the binary itself, its build + embedding, installation into containers, and the SSH relay**. API proxying is only sketched (_Later_) so the binary's shape doesn't block it.

## Why embed instead of install-from-network / mount

- Works offline and on air-gapped images; no curl/wget in the image needed.

- Version-locked to the CLI: the helper and the host side speak one protocol version by construction, no negotiation matrix.

- Apple `container` can't file-bind (`Backend::supports_file_binds()`, `src/runtime/mod.rs:221`), so "mount the binary in" isn't universal; writing it through `exec` stdin is.

## Binary: `devsbd/` crate

Turn the repo into a cargo workspace with a second member `devsbd/` (`devsbd`). Root stays the `devsandbox` package.

Size profile. `lto`, `strip`, `panic`, `codegen-units` can't be set per package, so use a custom profile that only the helper build uses:

```toml
[profile.devsbd]
inherits = "release"
opt-level = "z"
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

Targets: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` (static; runs on any distro image, glibc or not).

Dependencies — keep minimal, size is the product:

- No tokio for the SSH relay: blocking std I/O + a thread per stream is fine
  at agent-protocol volumes. Revisit if API proxying needs it.

- rustls: **not needed for SSH forwarding**. It only enters if the helper
  itself terminates or originates TLS (see _Open questions_). Gate it behind a
  cargo feature so the SSH-only build stays small.

- Target size budget: < 500 KiB uncompressed, ~200 KiB zstd per arch.

Subcommands (argv[1], no clap — hand-parse to save size):

- `daemon` — in-container long-lived process; owns the unix listeners.

- `bridge` — speaks the framed stdio protocol on stdin/stdout to the host; attaches to the daemon.

- `version` — prints protocol version + build hash (install check).

## Build + embedding

The main crate must build for every release target (incl. macOS, Windows) while embedding *Linux* helper binaries. `build.rs` never invokes cargo itself: a nested `cargo build` deadlocks on the outer build's `target/` lock unless given its own target dir (second cache, cold rebuilds), inherits the outer build's env (`CARGO_ENCODED_RUSTFLAGS`, `TARGET`, … — e.g. Windows' `+crt-static` leaking into the musl build), runs on every `cargo check`/rust-analyzer pass, needs hand-maintained `rerun-if-changed` for devsbd sources, and can't work from the crates.io tarball anyway. A workspace doesn't remove the ordering problem — binary artifact dependencies (`-Z bindeps`) are nightly-only — so devsbd is built as a separate step and the main build only *reads* its output:

- **Prebuilt helpers, located by env var.** `build.rs` reads `DEVSANDBOX_DEVSBD_DIR` 
  (containing `x86_64/devsbd`, `aarch64/devsbd`), zstd-compresses each into `OUT_DIR`, and emits
  `cargo:rerun-if-changed`/`rerun-if-env-changed`. `src/devsbd.rs` does `include_bytes!(concat!(env!("OUT_DIR"), "/devsbd-x86_64.zst"))`.

- **Missing helpers → empty blobs, feature off.** `cargo build`/`cargo test` with no env var still
  works (dev loop, CI `test` job, `cargo install` from crates.io). At runtime an empty blob means 
  "helper unavailable": SSH forwarding falls back to the existing bind-mount path (`docs/ssh-agent.md`) and a
  one-line note is logged. `cargo install` users therefore get today's behavior, not a broken one — call that out in the README.

- Compression: `zstd` crate as a **build-dependency** (level 19, C is fine at build time); decompression at
  runtime with `ruzstd` (pure Rust, no C on the Windows/macOS host builds). Confirm neither is in the tree
  yet — both are new deps.

- Expose `devsbd::blob(arch) -> Option<Cow<[u8]>>` and `devsbd::hash(arch)` (sha256 of each arch's
  uncompressed bytes, computed in `build.rs`) for install checks. Both arches are always embedded:
  the non-host one is the fallback for emulated images (see _Installing_) and later serves remote
  containers.

- `release.yml`: new `devsbd` job matrix (x86_64 on `ubuntu-latest`, aarch64 on
  `ubuntu-24.04-arm`, both with `musl-tools`) runs
  `cargo build -p devsbd --profile devsbd --target …`, uploads
  artifacts; the `build` job downloads both into a dir and sets
  `DEVSANDBOX_DEVSBD_DIR`, overriding the local default below.

- **Local default:** `.cargo/config.toml` sets
  `[env] DEVSANDBOX_DEVSBD_DIR = { value = "target/devsbd", relative = true }`, and
  `scripts/build-devsbd.sh` builds both arches into `target/devsbd/{x86_64,aarch64}/devsbd`. Plain
  `cargo run` then embeds whatever was last built; rerun the script only when devsbd changes. Nothing
  built yet → empty blobs, fallback as above.

- **Cross-building via `rust-lld`:** `.cargo/config.toml` sets `linker = "rust-lld"` for both musl
  targets, so `rustup target add {x86_64,aarch64}-unknown-linux-musl` is the only prerequisite —
  the musl targets ship their own CRT objects. Same script on Linux (either arch) and macOS, no
  Docker, no zig/`cross`. To verify on a Mac in Step 1.

- **Rule: devsbd has no C dependencies** (no `cc`/build scripts compiling C or asm). That's what
  keeps the `rust-lld` cross-build working. Note `ring` and `aws-lc-rs` (rustls' crypto backends)
  both break it — another input to the TLS open question. If a C dep ever becomes unavoidable, build
  devsbd inside a container on macOS (a runtime is present on any host using the CLI); deferred until
  then.

## Installing into a container

- **Arch: host arch first, other arch on failure.** No `uname`/`inspect` probe: a local runtime runs
  host-arch containers (`cfg!(target_arch)`; Windows x86_64 → linux x86_64, macOS arm64 → linux
  aarch64). Except emulated images — Docker Desktop/OrbStack/podman silently run amd64-only images
  under Rosetta/qemu on arm64 hosts (and qemu binfmt on arm64 Linux), where the host-arch binary fails
  with `exec format error`. So:
  1. Write the host-arch blob, run `devsbd version`.
  2. Any failure → write the other arch's blob to the same path, run `version` again. Don't match on
     `exec format error` — exit code/message differ per runtime (126, 255, OCI error text); one blind
     retry is simpler and only costs anything in the rare case.
  3. Still failing → devsbd unavailable, bind-mount fallback, one-line note
     ("devsbd couldn't run in `<c>`").

  Persist the arch that worked on `Instance` (`devsbd_arch: Option<Arch>`, `None` = unavailable) so
  `start` on an emulated image goes straight to the right blob. Real probing is deferred to
  remote-container support; leave a code comment saying so.

- **Transport:** stream the decompressed bytes into
  `exec -i -u 0 <c> sh -c 'mkdir -p /run/devsandbox/bin && cat > …tmp && chmod 755 …tmp && mv …tmp /run/devsandbox/bin/devsbd'`.
  Works on docker, podman and Apple `container` alike (no `docker cp`, no file binds). Requires `/bin/sh` in the image — distroless images
  get the fallback.

- **Idempotence:** first run `devsbd version`; skip the write when
  it reports `devsbd::hash(devsbd_arch)`. A CLI upgrade therefore rewrites the binary
  on next `run`/`start`.

- **When:** in `run` after create (next to lifecycle commands) and in
  `start_instance` (`src/commands/start.rs:42`) after the container starts —
  `/run` is often tmpfs, so the binary may not survive a restart; reinstall is
  cheap and the hash check makes it a no-op otherwise.

- Not part of `config_hash`/`build_hash` (runtime fact, same rule as ssh-agent).

## SSH forwarding over stdio

```
ssh (in container) ──unix──▶ devsbd daemon ◀──frames over exec stdio──▶ devsandbox (host) ──▶ $SSH_AUTH_SOCK
                    /run/devsandbox/ssh-agent.sock      `exec -i <c> devsbd bridge`
```

- **Daemon:** started with `exec -d <c> devsbd daemon` after install.
  Pidfile/lock at `/run/devsandbox/devsbd.pid`; a second `daemon` exits 0 if
  one is alive. Listens on `/run/devsandbox/ssh-agent.sock` (mode 0666 or
  chowned to `remoteUser` — fixes the uid-mismatch caveat of the bind mount)
  and on a control socket `/run/devsandbox/devsbd.ctl` for bridges.

- **Bridge:** host runs `exec -i <c> devsbd bridge`; the bridge
  connects to the control socket and relays frames between it and its stdio.
  Each agent client connection accepted by the daemon becomes a stream routed
  to *one* live bridge (most recent); no live bridge → accept and close
  immediately (ssh reports "agent refused", same as no agent).

- **Host side:** for each `Open` the host connects to the *current*
  `$SSH_AUTH_SOCK` (so rotation is a non-issue) — or, later, the Windows
  named pipe. Owner of the host side:
  - TUI: one bridge per running instance while the dashboard is open
    (background thread in `src/tui/mod.rs`).
  - CLI `exec`: spawn a bridge thread for the lifetime of the exec.
  - Everything else (VS Code terminals, plain `docker exec`): covered whenever
    a devsandbox TUI/exec is alive; otherwise not. VS Code keeps its own
    forwarding. Document the gap; a `devsandbox agent <instance>` foreground
    command is a cheap follow-up if needed.

- **Env:** `exec_argv` keeps injecting `SSH_AUTH_SOCK` (`src/commands/exec.rs:81`)
  — the target path is identical to the bind-mount design, so the two modes
  are interchangeable per instance. `Instance.ssh_auth_sock` stays the switch;
  add `ssh_mode: Mount | Relay` (or infer from `devsbd_arch.is_some()`).

- **Precedence:** helper available → relay (every runtime, including Apple
  `container` and Windows once the pipe client exists); else → existing bind
  mount on docker/podman Linux.

### Frame protocol

Length-prefixed, little-endian, one byte stream in each direction:

```
u32 stream_id | u8 kind | u32 len | payload[len]
kind: 0 Hello(version, hash)  1 Open(channel: u8)  2 Data  3 Close  4 Ping/Pong
```

- `Hello` both ways first; mismatched version → bridge exits non-zero with a
  message the host surfaces ("helper out of date, restart instance").
- `channel` byte reserves room for API proxy streams (`1 = ssh-agent`,
  `2 = http-proxy`, …) so the protocol doesn't change later.
- Protocol code lives in a small `helper-proto` module shared by both crates
  (a third workspace member, or `#[path]` include) so host and helper can't
  drift; unit-tested with in-memory pipes.

## Steps

1. Workspace + `devsbd/` crate skeleton (`version`), `[profile.devsbd]`,
   `scripts/build-devsbd.sh`; check size.
2. `build.rs` + `src/devsbd.rs` embedding with empty-blob fallback; tests for
   blob/hash plumbing (`cargo test` must pass with no helper built).
3. Install into container (host arch → other-arch retry, stream write, hash check) in `run` +
   `start`; `Instance.devsbd_arch`.
4. Shared frame protocol + tests.
5. Helper `daemon` + `bridge`; host-side bridge driver (`src/commands/agent.rs`
   or similar), wired into CLI `exec` and the TUI.
6. Switch ssh-agent to relay when available; update `docs/ssh-agent.md`
   (stale-socket, Apple, Windows sections point here).
7. `release.yml` helper job (after confirmation).

## Later: API proxying (separate plan)

- Helper grows a `http-proxy` channel: listens on `127.0.0.1:<port>`, sandbox
  gets `HTTPS_PROXY`/`HTTP_PROXY` via `exec_argv`; streams go to the global
  credentials service container (network alias, cf. `docs/cli-proxy.md`)
  rather than to the host.

- TLS interception: devsandbox generates a CA per config root, installs the
  cert in the container (`update-ca-certificates` when present, plus
  `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`, `SSL_CERT_FILE`,
  `GIT_SSL_CAINFO`, `CURL_CA_BUNDLE`) — the install path from _Installing_
  handles the file write.

- Service container: build from this repo vs. embed it too (offline mode) —
  decide in that plan. Overlaps heavily with `docs/cli-proxy.md`'s `tools`
  service; reconcile the two there.

## Open questions

- **Where does TLS terminate?** If the helper only tunnels `CONNECT` bytes and
  the service container terminates TLS with the CA key, the helper needs **no
  rustls** and the CA private key never enters the sandbox (a sandbox with the
  key could mint certs for anything, though only its own trust store trusts
  them). Leaning: service terminates, helper stays a dumb tunnel; rustls only
  if helper↔service traffic must be encrypted on the shared network.

- Should the daemon run as root (`-u 0`) and chown the socket to `remoteUser`,
  or run as `remoteUser` directly? Root is simpler and fixes the uid caveat.

- Images without `/bin/sh` or with read-only `/run`: fall back silently, or
  pick another dir (`/tmp/devsandbox`)?
