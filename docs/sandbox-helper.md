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

- **`DEVSANDBOX_DEVSBD_REQUIRED=1` → missing helper is a build error.** The release build
  (`release.yml`, step 12) sets it so a broken artifact upload can't silently ship empty blobs; the
  panic names the missing `<arch>/devsbd` path and points at `scripts/build-devsbd.sh`. Unset (the
  default) keeps the empty-blob fallback.

- **No-change builds are a no-op.** A `rerun-if-changed` on a *missing* path is always stale, so
  before the fix cargo reran `build.rs` and recompiled the crate on every build once the helper was
  absent. When the helper file exists `build.rs` tracks it directly; when it's missing it creates the
  empty `<dir>/<arch>/` and tracks that directory, which cargo rescans and so sees the helper appear
  (not an ancestor like `target/`: cargo scans directories recursively and every build modifies
  `target/`). `OUT_DIR` files are rewritten only when their bytes
  change, so a settled tree — helper present or absent — rebuilds without recompiling `devsandbox`.

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
  (`spawn_start`, silently). On docker `/run` is in the container's writable
  layer, so the binary survives a restart (only the daemon doesn't — the bridge
  self-heals that); reinstall on start stays for CLI upgrades and images that
  do mount a tmpfs at `/run`. Reinstall is cheap and the hash check makes it a
  no-op otherwise. `devsbd::ensure_recorded` persists a changed `devsbd_arch`,
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
    only `VERSION` must match). A differing `VERSION` fails the host handshake
    with a typed error carrying both versions, phrased with direction: the
    helper older → `helper in <c> is outdated (protocol N, need M): restart
    the instance`, the helper newer → `this devsandbox is older than the
    helper in <c> (protocol N, need M)`. `exec` prints it as a `note:` after
    the command; the TUI stays silent.
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
    can't parse. The bridge prints its own direction-aware line for that case
    (`daemon in container is older/newer (protocol N) than this helper (M)`),
    which the host surfaces (it prefers the bridge's stderr when its own
    handshake with the bridge hit `UnexpectedEof`, i.e. the bridge died
    before `Hello`). **Exit-code contract:** on a daemon mismatch the bridge
    exits `proto::MISMATCH_EXIT` (3), distinct from any other failure (1) and
    the argv-usage error (2). The host `wait()`s the child after that EOF and,
    seeing code 3 (or a host-side `version_mismatch` on its own `Hello`), flags
    the bridge as a mismatch — so `reconcile` classifies mismatches from the
    exit code, never by string-matching stderr.

- **Bridge:** host runs `exec -i -u root <c> devsbd bridge`. The bridge
  connects to the control socket first (retrying for 2s: `exec -d` returns
  before the daemon listens) and exchanges `Hello` with the daemon, so a
  mismatched daemon shows up as a failed handshake; the host then reports the
  bridge's stderr (e.g. `daemon not running`). **Self-heal:** if that first
  connect finds nothing listening (`NotFound`/`ConnectionRefused`), the bridge
  starts a daemon itself — its own binary, `daemon` subcommand, detached (own
  process group, all stdio null; stdout *must* be null, it's the frame stream
  to the host) — at most once, then keeps retrying for the 2s. This revives a
  container restarted outside devsandbox (restart policy, `docker restart`, VM
  restart): the binary survives in the writable layer but the daemon process
  does not. Concurrent bridges self-starting is safe — the daemon's pidfile
  flock (`take_over`) lets one of this build win and the losers exit. Then it does
  `Hello` with the host and copies bytes verbatim between stdio and the
  control socket; only the daemon parses frames — so `Ping`/`Pong` traverse
  the in-container bridge end to end with no keepalive logic there. The host
  handshake has a 10s timeout (`spawn_with`): a container that accepts the
  `exec` but never completes `Hello` is killed and reported as `helper
  handshake timed out`, so a bridge owner is never blocked forever.
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
  - **Keepalive.** `Mux` records the `Instant` of the last inbound frame (any
    kind) and runs a keepalive thread (`Mux::keepalive`): `Ping` every 15s,
    and after 45s of inbound silence it calls a caller-supplied `on_dead`
    once and stops. It also stops (without firing `on_dead`) once `serve`
    returns, so no thread outlives its bridge. Daemon side: `on_dead` shuts
    the control connection down, so `serve` returns and the wedged bridge
    leaves the routing list (a wedged newest bridge no longer stalls every
    agent request). Host side: `on_dead` kills the child, so `serve` ends,
    `is_done` flips, and `Bridges::reconcile` retries. The dead/alive rule is
    a pure fn tested with injected durations; a real-pipe test drives the
    thread with tiny durations.
    - _Caveat (head-of-line):_ liveness is judged from *inbound* frames, but a
      `serve` blocked writing `Data` to a slow local socket also stops reading,
      so it stops updating the last-inbound timestamp — a slow local peer can
      look dead. Acceptable for agent traffic (small messages); bulk
      http-proxy will need per-stream flow control (see _Later_) so one slow
      stream can't stall the frame reader.

- **Host side:** for each `Open` the host connects to the *current*
  `$SSH_AUTH_SOCK` (so rotation is a non-issue) — or, later, the Windows
  named pipe. Owner of the host side:
  - Each host process owns its bridges; they're never shared across processes.
    Several bridges per container are normal (the daemon routes to the newest
    live one, see _Bridge_).
  - TUI: one bridge per running instance while the dashboard is open, used by
    every integrated terminal tab on that instance (`devsbd::bridge::Bridges`).
    Owned by a worker thread (`Bridges::spawn_worker` → `BridgeWorker`), not the
    UI thread: the snapshot arm in `src/tui/mod.rs` just `send`s the worker the
    owned list of running containers, and the worker does `State::load` + the
    per-instance `exec` off-thread, coalescing a burst of snapshots to the
    newest list before reconciling. A dead bridge is retried at most every 10s —
    except a protocol **version mismatch** (host- or daemon-side, see _Bridge_),
    which is retried only every 5 min, since only a helper rewrite fixes it
    (`start`, which may not take the container out of the running set; a real
    restart drops the entry and retries at once). On TUI exit `BridgeWorker`'s `Drop` closes the channel and joins the
    worker (any exit path, including `?` early returns), so every bridge is
    killed before the terminal is restored.
  - CLI `exec` (including the TUI's suspended `:exec`, which goes through
    `exec_status`): its own bridge for the lifetime of the exec, started
    optimistically alongside the command, with no handshake wait and so no added
    latency. The daemon's hold covers a command that reaches the agent
    first (a `docker exec` startup is ~60–100 ms). A failed handshake is
    reported after the command exits, so it can't interleave with its output.
  - Bridges run for instances in relay mode (`devsbd::relay_mode`: helper
    installed, no bind mount) when the host has a live agent. A mounted
    instance stays on the mount (the mount occupies the daemon's socket path).
  - Everything else (VS Code terminals, plain `docker exec`): covered whenever
    a devsandbox TUI/exec is alive; otherwise not. VS Code keeps its own
    forwarding. Document the gap; a `devsandbox agent <instance>` foreground
    command is a cheap follow-up if needed.

- **Env:** `exec_argv` (`src/commands/exec.rs`) injects `SSH_AUTH_SOCK` in both
  modes at the same target. Mount mode carries the target on
  `Instance.ssh_auth_sock` and always injects. Relay mode is inferred
  (`devsbd::relay_mode` = `devsbd_arch.is_some() && ssh_auth_sock.is_none()`,
  `false` off unix) and injects the fixed `SSH_AGENT_TARGET` **only when the
  host has a live agent** (`$SSH_AUTH_SOCK` set and the path exists), so an
  agent-less exec doesn't point ssh at a dead socket. Same probe gates bridge
  spawning (`bridge::has_host_agent`, from `exec_status` and
  `Bridges::reconcile`): no host agent → no extra `exec` that can't help. No
  `ssh_mode` field: the two `Option`s already encode it.

- **Precedence:** relay-first. On unix, whenever a helper is embedded
  (`devsbd::embedded()`), `run` never mounts the socket — the daemon serves it
  on every runtime (docker/podman now; Apple `container` and Windows once the
  pipe client exists). Only builds with no embedded helper (`cargo install`)
  and non-unix hosts fall back to the bind mount. An instance created under one
  path keeps it until recreated (`rm` + `run` / `rebuild`); the two `Option`s
  on `Instance` record which.

- **Last start wins (takeover).** Takeover keys on build-hash inequality,
  not on which build is newer: any `start` by a CLI of another build
  (upgrade, downgrade, or a second install) rewrites the binary and evicts
  the running daemon, ending its bridges and in-flight agent streams. Two
  CLI builds used against the same containers therefore evict each other on
  every `start`. Same build is a no-op. Chosen over a version gate so the
  binary on disk always matches the running daemon.

- **0666 agent socket.** The daemon runs as root and chmods
  `/run/devsandbox/ssh-agent.sock` to 0666, so **any uid inside the container**
  can reach the host agent (same effective exposure as the bind mount when the
  container process runs as the socket owner — it fixes that path's
  uid-mismatch failure). The blast radius is one container's processes; the
  private key never enters the sandbox regardless.

### Frame protocol

Length-prefixed, little-endian, one byte stream in each direction:

```
u32 stream_id | u8 kind | u32 len | payload[len]
kind: 0 Hello(u32 version, u8 hash_len, hash utf-8, u32 caps)  1 Open(channel: u8)  2 Data  3 Close  4 Ping  5 Pong  6 Quit
```

- **Frozen forever:** the header layout, `Hello`'s leading `u32 version`, and
  `Quit` (pinned by a byte-level test). An outdated daemon must still
  understand the `Quit` that replaces it, and either side's `Hello` must
  decode its `version` far enough to report a mismatch — even when the rest of
  the payload is a future layout it can't parse (so on version mismatch,
  decoding keeps only `version` and doesn't fail on the rest). The daemon
  answers a bridge's `Hello` with its own even on mismatch, for the same
  reason. `Hello`'s trailing `caps: u32` is a capability bitset (0 today,
  unknown bits ignored) and bytes after it are ignored, leaving room for
  later fields without a `VERSION` bump.

- Control frames (`Hello`, `Ping`, `Pong`, `Quit`) use stream 0. **Stream-id
  spaces:** the daemon (container side) allocates ids with the high bit clear
  (1..2^31, wrapping back to 1, never 0); the host allocates them with the
  high bit set (none yet). The host refuses (`Close`) an `Open` whose id has
  the high bit set, whose id is already live, or once 64 streams are live
  (`MAX_STREAMS`). `Pong` echoes the `Ping` payload (separate kinds so a reply
  can't be mistaken for a new ping).
- Payload capped at 1 MiB (`MAX_PAYLOAD`): stray bytes on the stream (a shell
  banner on stdout) become an `InvalidData` error, not a huge allocation.
  Malformed `Hello`/`Open` payloads are errors too. **Unknown kinds are
  skipped** (their payload consumed), not errors, so additive frames don't
  need a `VERSION` bump. EOF at a frame boundary is a clean end, inside a
  frame `UnexpectedEof`.
- Each frame is encoded into one buffer and written with a single
  `write_all` + flush, so writers sharing a stream behind a mutex never
  interleave frames.

- `Hello` both ways first; a mismatched version makes `proto::handshake`
  return a typed `VersionMismatch { peer, ours }` (wrapped in an `InvalidData`
  `io::Error`, recovered with `proto::version_mismatch`). Each side phrases
  its own direction-aware message, which CLI `exec` prints (see _Version
  mismatch / takeover_).
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
5. [x] Helper `daemon` + `bridge`; host-side bridge driver (`src/devsbd/bridge.rs`),
   wired into CLI `exec` and the TUI.
6. [x] Switch ssh-agent to relay when available (details below).
7. [x] Protocol revision before first release (details below).
8. [x] Keepalive + timeouts (details below).
9. [x] Daemon self-heal (details below).
10. [x] TUI bridge ownership off the UI thread (details below).
11. [x] Build hygiene (details below).
12. `release.yml` helper job (after confirmation).

Decisions taken for 6–11 (user, 2026-09-24): **relay-first** (never mount when
a helper is embedded), **last start wins** on takeover (hash differs → take
over; document it), agent socket stays **0666** (document the consequence).

### Step 6 — relay-first ssh-agent

- `run` (`src/commands/run.rs` `materialize`, ~l.252): on unix, when
  `devsbd::embedded()` (new: any arch has a hash), skip `ssh_agent_forward`
  entirely — no mount, no symlink, `ssh_auth_sock = None`. If `ensure` then
  fails, the instance has no forwarding (distroless / read-only `/run`); the
  note says so: `note: ssh-agent forwarding off in <c>: <reason>`.
  Non-unix hosts and builds without helpers keep today's path unchanged.
- Mode is inferred, no new field: relay = `devsbd_arch.is_some() &&
  ssh_auth_sock.is_none()` (`bridge::wanted`, now cfg-independent so
  `exec_argv` can call it; `false` on non-unix). Instances created with a
  mount keep mount mode until recreated.
- `exec_argv` (`src/commands/exec.rs:94`): mount mode unchanged; relay mode
  injects `-e SSH_AUTH_SOCK=<SSH_AGENT_TARGET>` **only when the host has a
  usable agent** (`$SSH_AUTH_SOCK` set and the path exists). Keep the builder
  testable: the env probe is one small fn, the argv logic takes its result
  (e.g. an inner `exec_argv_with(.., host_agent: bool)`); no env mutation in
  tests.
- Same probe gates bridge spawning: `exec_status` and `Bridges::reconcile`
  spawn nothing when the host has no agent (removes the extra `exec` per
  command where it can't help). `host_agent()` (bridge.rs:55) also checks the
  path exists.
- Docs: `docs/ssh-agent.md` stale-socket caveat, _Future: Apple container_ and
  _Windows_ sections point here; README feature table: Apple `container` via
  relay (note: not yet exercised on a Mac), Linux/macOS-docker relay when
  helper embedded; README note that `cargo install` builds have no helper and
  use the mount path. Document in this file: last-start-wins takeover
  (two CLI builds on one machine evict each other's daemon on `start`), and
  that 0666 means any uid in the container can use the agent (same as the
  bind mount when running as the socket owner).

### Step 7 — protocol revision (unreleased, `VERSION` stays 1)

- `Hello` payload becomes `u32 version | u8 hash_len | hash | u32 caps`;
  `caps` = 0 for now, unknown bits ignored, bytes after `caps` ignored (room
  for later fields). Only `u32 version` at offset 0 is frozen for mismatch
  reporting; update the frozen-bytes test and _Frame protocol_ above.
- Unknown frame kinds are skipped (payload consumed), not errors, so
  additive frames don't need a `VERSION` bump.
- Stream id spaces: container-allocated ids have the high bit clear
  (daemon allocates 1..2^31, wrapping back to 1, never 0), host-allocated
  ids have it set (none yet). The host refuses (`Close`) an `Open` with the
  high bit set or an id already live.
- Host caps concurrent streams per bridge (64); further `Open`s are refused.
- `Mux::serve` (`src/devsbd/mux.rs` `Data` arm) must not hold the `streams`
  mutex across the blocking `write_all`: keep a cloned handle (e.g.
  `Arc<UnixStream>`), drop the guard, then write.
- Mismatch direction: `proto::handshake` returns a typed error carrying
  `peer`/`ours` versions (downcastable from `io::Error`). Host message:
  peer older → `helper in <c> is outdated (protocol N, need M): run
  devsandbox start <instance>`; peer newer → `this devsandbox is older than
  the helper in <c> (protocol N, need M)`. The bridge reports daemon mismatch
  the same way, from its own side.
- Flow control stays out (agent volumes); record in _Later_ that
  http-proxy needs credit/window frames (a new kind, now additive).

### Step 8 — keepalive + timeouts

- `Mux` gains liveness: record `Instant` of the last inbound frame in
  `serve`; a keepalive thread sends `Ping` every 15s and, after 45s with no
  inbound frame, calls a caller-supplied `on_dead` and stops.
- Daemon: per bridge, `on_dead` shuts down the control connection, so
  `serve` returns and the bridge leaves the routing list (a wedged newest
  bridge no longer stalls every agent request).
- Host: per bridge, `on_dead` kills the child (`is_done` flips, TUI retries).
- Host handshake timeout: 10s, then kill the child and report
  `Err("helper handshake timed out")`.
- Pure logic (timeout decision) unit-tested with injected clock/durations;
  intervals are consts.

### Step 9 — daemon self-heal [x]

- `devsbd bridge`: when the first control-socket connect fails with
  `NotFound`/`ConnectionRefused`, spawn `current_exe() daemon` detached
  (`process_group(0)`, stdio null, not waited), then retry the connect for
  the existing 2s. The daemon's pidfile lock already makes racing starts
  safe. Covers containers restarted outside devsandbox (restart policy,
  `docker restart`, VM restart).
- Fix the doc claim "`/run` is often tmpfs": on docker it's in the writable
  layer, so the binary survives a restart but the daemon doesn't.

### Step 10 — TUI bridges off the UI thread [x]

- `Bridges` (`src/devsbd/bridge.rs`) moves to a worker thread fed by an mpsc
  of running-container lists (sent from the snapshot arm, `src/tui/mod.rs`
  ~l.240). `State::load` and spawns happen there. On TUI exit the sender is
  dropped and the thread joined, so bridges are killed before returning.
- Version mismatch (typed error from step 7, or `devsbd bridge` exiting
  `proto::MISMATCH_EXIT`) is retried on a 5 min gap instead of 10s (review
  change: "until the container leaves the running set" would never retry after
  a `start` on a running container, which is what fixes it).

### Step 11 — build hygiene [x]

- `build.rs`: `DEVSANDBOX_DEVSBD_REQUIRED=1` turns a missing/empty helper
  into a build error (for CI, step 12).
- Check whether a `rerun-if-changed` on a missing path reruns `build.rs` (and
  recompiles the crate via rewritten `OUT_DIR` files) on every build; if so,
  fix (e.g. only write `OUT_DIR` files when content changed).
- Test (linux, host arch embedded): write the host-arch blob to a temp file,
  run `version`, assert it reports `proto::VERSION` and `hash(host)` — catches
  a stale `target/devsbd` after a protocol change.

## Later: API proxying (separate plan)

- Helper grows a `http-proxy` channel: listens on `127.0.0.1:<port>`, sandbox
  gets `HTTPS_PROXY`/`HTTP_PROXY` via `exec_argv`; streams go to the global
  credentials service container (network alias, cf. `docs/cli-proxy.md`)
  rather than to the host. Unlike ssh-agent's small messages, bulk HTTP needs
  flow control, so this adds credit/window frames — a new frame kind gated on a
  `Hello` `caps` bit (a peer that skips the frames must not be sent them), so
  it needs no `VERSION` bump.

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
