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

- **Build hash patched into the binary.** The helper can't know its own sha256 at compile time, so
  devsbd holds a fixed-length slot (`BUILD_HASH`: marker `DEVSBD_BUILD_HASH:` + a 64-byte
  placeholder). `build.rs` hashes the unpatched binary, finds the slot (exactly one occurrence, else
  the build fails), overwrites the placeholder with the hex hash, then compresses. `devsbd version`
  reads the slot with a volatile read and prints `devsbd <protocol> <hash>`, which is what the install
  check compares against `devsbd::hash(arch)`. So `hash` identifies the build; it's not the sha256
  of the patched bytes.

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
  1. Write the recorded arch's blob (else the host's), run `devsbd version`.
  2. Any failure → write the other arch's blob to the same path, run `version` again. Don't match on
     `exec format error` — exit code/message differ per runtime (126, 255, OCI error text); one blind
     retry is simpler and only costs anything in the rare case.
  3. Still failing → devsbd unavailable, bind-mount fallback, one-line note
     ("devsbd couldn't run in `<c>`").

  Persist the arch that worked on `Instance` (`devsbd_arch: Option<Arch>`, `None` = unavailable) so
  `start` on an emulated image goes straight to the right blob. Real probing is deferred to
  remote-container support; leave a code comment saying so.

- **Transport:** stream the decompressed bytes into
  `exec -i -u root <c> sh -c 'mkdir -p /run/devsandbox/bin && cat > …tmp && chmod 755 …tmp && mv …tmp /run/devsandbox/bin/devsbd'`
  (`Backend::run_with_stdin`: stdout discarded, stderr captured, so it's TUI-safe). `-u root`
  rather than `-u 0`, matching the existing Apple backend exec (`src/runtime/apple.rs`). Works on
  docker, podman and Apple `container` alike (no `docker cp`, no file binds). Requires `/bin/sh` in
  the image — distroless images get the fallback.

- **Idempotence:** first run `devsbd version`; skip the write when
  it reports either embedded arch's hash (the reporting one is the arch that works). A CLI upgrade
  therefore rewrites the binary on next `run`/`start`.

- **When:** in `run` after create (next to lifecycle commands) and in
  `start_instance` (`src/commands/start.rs:42`, both the resolved and bare
  branches) after the container starts, plus the TUI's bare `s` start
  (`spawn_start`, silently) — `/run` is often tmpfs, so the binary may not
  survive a restart; reinstall is cheap and the hash check makes it a no-op
  otherwise. `devsbd::ensure_recorded` persists a changed `devsbd_arch`,
  reloading state before saving.

- Failure never fails the command: a one-line `note:` on stderr (suppressed in
  the TUI). Builds without embedded helpers (`cargo install`) print
  `devsbd not embedded in this build` on each `run`/`start`.

- Not part of `config_hash`/`build_hash` (runtime fact, same rule as ssh-agent).

## SSH forwarding over stdio

```
ssh (in container) ──unix──▶ devsbd daemon ◀──frames over exec stdio──▶ devsandbox (host) ──▶ $SSH_AUTH_SOCK
                    /run/devsandbox/ssh-agent.sock      `exec -i <c> devsbd bridge`
```

- **Daemon:** started with `exec -d -u root <c> devsbd daemon` after every
  successful install (`devsbd::ensure`). Pidfile at `/run/devsandbox/devsbd.pid`
  (`<pid> <build hash>`) held with an exclusive `File::try_lock` for the
  daemon's lifetime. The kernel drops the lock with the process, so a stale
  pidfile never blocks. Listens on `/run/devsandbox/ssh-agent.sock` (mode
  0666, daemon runs as root — fixes the uid-mismatch caveat of the bind mount)
  and on a control socket `/run/devsandbox/devsbd.ctl` (0600) for bridges. If
  the agent path is taken (the bind mount), it logs and serves only the
  control socket.

- **Version mismatch / takeover.** A CLI upgrade while a container runs
  leaves the old binary and old daemon in it. Outcomes:
  - `exec`/TUI before any reinstall: the host runs the *old* `bridge`. A
    differing build hash is fine (bridge and daemon are the same old build;
    only `VERSION` must match). A differing `VERSION` fails the host
    handshake: `exec` prints `note: ssh-agent relay unavailable in <c>:
    protocol version N, expected M: helper out of date, restart the instance`
    after the command; the TUI stays silent.
  - `stop` + `start` / `rebuild`: clean, no process survives.
  - `start` on a still-running container (`start.rs` accepts it): `ensure`
    rewrites the binary (hash differs) and starts a new daemon. The lock is
    held by a daemon whose pidfile hash differs, so the new one sends `Quit`
    as the first frame on the control socket. The old one exits (its bridges
    and streams end), and the new one takes the lock (waits up to 3s) and
    rebinds the sockets. Same hash → exits 0. No hash in the pidfile yet (its
    owner is mid-startup) → treated as same build, so two concurrent starts
    never evict each other.
  - The bridge also exchanges `Hello` with the daemon, so a bridge/daemon
    mismatch fails the handshake rather than passing frames the daemon
    can't parse.

- **Bridge:** host runs `exec -i -u root <c> devsbd bridge`. The bridge
  connects to the control socket first (retrying for 2s: `exec -d` returns
  before the daemon listens) and exchanges `Hello` with the daemon, so a
  missing or mismatched daemon shows up as a failed handshake; the host then
  reports the bridge's stderr (e.g. `daemon not running`). Then it does
  `Hello` with the host and copies bytes verbatim between stdio and the
  control socket; only the daemon parses frames.
  Each agent client connection accepted by the daemon becomes a stream routed
  to *one* live bridge (most recent). No live bridge → the client is held up
  to 1s for one to attach, then closed (ssh reports "agent refused", same as
  no agent). The hold is what makes the optimistic `exec` below safe; its cost
  is a 1s delay before "refused" when no devsandbox process is alive. The daemon
  keeps every connected bridge, so when the newest ends (a CLI `exec`),
  routing falls back to the next newest (the TUI's) instead of to nothing.

- **Stream routing** (`src/devsbd/mux.rs`, shared via `#[path]` like the
  protocol): the same `Mux` runs in the daemon and on the host. Whoever sees
  a local stream end first sends `Close`; receiving `Close` shuts the local
  socket without echoing. A refused `Open` (unknown channel, no host agent)
  is answered with `Close`. Losing the frame connection shuts every stream.
  No half-close: a client's EOF closes the stream both ways (ssh clients
  don't half-close agent connections). Unix-only for now (unix sockets on
  both ends).

- **Host side:** for each `Open` the host connects to the *current*
  `$SSH_AUTH_SOCK` (so rotation is a non-issue) — or, later, the Windows
  named pipe. Owner of the host side:
  - Each host process owns its bridges; they're never shared across processes.
    Several bridges per container are normal (the daemon routes to the newest
    live one, see _Bridge_).
  - TUI: one bridge per running instance while the dashboard is open, used by
    every integrated terminal tab on that instance
    (`devsbd::bridge::Bridges`, reconciled on each snapshot in
    `src/tui/mod.rs`; a dead bridge is retried at most every 10s).
  - CLI `exec` (including the TUI's suspended `:exec`, which goes through
    `exec_status`): its own bridge for the lifetime of the exec, started
    optimistically alongside the command, with no handshake wait and so no added
    latency. The daemon's hold covers a command that reaches the agent
    first (a `docker exec` startup is ~60–100 ms). A failed handshake is
    reported after the command exits, so it can't interleave with its output.
  - Until step 6, bridges only run for instances with the helper and *no*
    bind mount (`bridge::wanted`): the mount occupies the daemon's socket path.
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
kind: 0 Hello(u32 version, hash utf-8)  1 Open(channel: u8)  2 Data  3 Close  4 Ping  5 Pong  6 Quit
```

- **Frozen forever:** the header layout, `Hello` and `Quit` (pinned by a
  byte-level test). An outdated daemon must still understand the `Quit` that
  replaces it, and either side's `Hello` must decode far enough to report a
  version mismatch. The daemon answers a bridge's `Hello` with its own even
  on mismatch, for the same reason.

- Control frames (`Hello`, `Ping`, `Pong`, `Quit`) use stream 0; the daemon allocates
  stream ids from 1 for the connections it accepts. `Pong` echoes the `Ping`
  payload (separate kinds so a reply can't be mistaken for a new ping).
- Payload capped at 1 MiB (`MAX_PAYLOAD`): stray bytes on the stream (a shell
  banner on stdout) become an `InvalidData` error, not a huge allocation.
  Unknown kinds and malformed `Hello`/`Open` payloads are errors too; EOF at a
  frame boundary is a clean end, inside a frame `UnexpectedEof`.
- Each frame is encoded into one buffer and written with a single
  `write_all` + flush, so writers sharing a stream behind a mutex never
  interleave frames.

- `Hello` both ways first; mismatched version → the host's handshake fails
  with "helper out of date, restart the instance", which CLI `exec` prints
  (see _Version mismatch / takeover_).
- `channel` byte reserves room for API proxy streams (`1 = ssh-agent`,
  `2 = http-proxy`, …) so the protocol doesn't change later.
- `Hello`'s hash is informational (the sender's build hash); only `version`
  must match (`proto::handshake`). `proto::VERSION` is also what
  `devsbd version` prints.
- Protocol code lives in `src/devsbd/proto.rs` (std-only), included by devsbd
  via `#[path]` so host and helper can't drift. Not a third workspace crate: it
  would have to be published for `cargo install devsandbox` to build, and
  `devsbd/` is excluded from the package, hence the file sits in the root
  crate. Unit-tested with in-memory readers and real OS pipes; runs under both
  `cargo test` and `cargo test -p devsbd`.

## Steps

1. [x] Workspace + `devsbd/` crate skeleton (`version`), `[profile.devsbd]`,
   `scripts/build-devsbd.sh`; check size.
2. [x] `build.rs` + `src/devsbd.rs` embedding with empty-blob fallback; tests for
   blob/hash plumbing (`cargo test` must pass with no helper built).
3. [x] Install into container (host arch → other-arch retry, stream write, hash check) in `run` +
   `start`; `Instance.devsbd_arch`.
4. [x] Shared frame protocol + tests.
5. [x] Helper `daemon` + `bridge`; host-side bridge driver (`src/commands/agent.rs`
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

- Should the daemon run as root (`-u root`) and chown the socket to `remoteUser`,
  or run as `remoteUser` directly? Root is simpler and fixes the uid caveat.

- Images without `/bin/sh` or with read-only `/run`: fall back silently, or
  pick another dir (`/tmp/devsandbox`)?
