# devsbd: the in-container helper

`devsbd` is a small static Linux binary embedded in `devsandbox` and copied
into every sandbox container. It is the container-side half of every
host↔container relay: ssh-agent, notifications, dispatcher control requests,
port forwards, tracked runs, the restart boot hook. Read this first for how it
gets built, embedded and installed; the siblings cover what it does.

Code: `devsbd/src/main.rs` (verbs), `build.rs`, `scripts/build-devsbd.sh`,
`src/devsbd.rs` (`blob`, `hash`, `embedded`, `install`, `ensure*`, `BIN`,
`relay_mode`), `[profile.devsbd]` in `Cargo.toml`, `.cargo/config.toml`.

## Verbs

argv is hand-parsed (no clap) to keep the binary small.

| verb | what | doc |
|---|---|---|
| `version` | `devsbd <proto::VERSION> <build hash>`: the host's install check | here |
| `features` | append-only list of CLI capabilities a dispatcher may probe (`thread-send`, …) | here |
| `daemon` | the long-lived in-container daemon | [in-container.md](in-container.md) |
| `bridge` | stdio ↔ daemon relay the host runs via `exec -i` | [protocol.md](protocol.md) |
| `boot` | restart hook: starts the daemon, reruns `postStartCommand` | [in-container.md](in-container.md) |
| `notify`, `thread put\|send\|withdraw\|rm` | queue Inbox records in the outbox | [in-container.md](in-container.md) |
| `thread ls`, `events [ack]`, `ensure`, `ls`, `branches`, `stop`, `rm`, `done`, `exec` | control requests to the host | [bridges.md](bridges.md) |
| `run start\|ls\|logs\|wait\|rm\|prune` | tracked runs (local, or on a child via the host) | [in-container.md](in-container.md) |
| `vscode-goto` | open a file in the attached VS Code window | [in-container.md](in-container.md) |

`features` is separate from `version` (whose one-line output the host
parses) and from the frame protocol's `caps` (which describe the host link,
not the CLI). A helper without `features` exits 2, read as "none".

## Build

`scripts/build-devsbd.sh` builds `-p devsbd --profile devsbd` for
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` into
`target/devsbd/<arch>/devsbd`. The `devsbd` profile is size-tuned
(`opt-level = "z"`, lto, strip, `panic = "abort"`); musl targets link with
`rust-lld` from any host (`.cargo/config.toml`). Rerun it after changing
`devsbd/`; cargo never builds the helper itself.

## Embedding

`build.rs` reads `$DEVSANDBOX_DEVSBD_DIR/<arch>/devsbd`
(`.cargo/config.toml` points it at `target/devsbd`; CI at its downloaded
artifacts). Per arch it:

- computes the sha256 of the unpatched binary and writes it into the
  helper's `BUILD_HASH` slot (marker `DEVSBD_BUILD_HASH:` + 64-byte
  placeholder; exactly one occurrence or the build panics), so
  `devsbd version` reports the same hash `devsbd::hash(arch)` returns;
- zstd-compresses it (level 19) into `OUT_DIR`, decompressed at install time
  with `ruzstd`.

Both arches are always embedded: the non-host one is the fallback for
emulated images. A missing helper yields an empty blob, which the runtime
treats as "helper unavailable" (`embedded()` false).
`DEVSANDBOX_DEVSBD_REQUIRED=1` makes a missing helper a build error; CI tests
and release builds set it.

### `cargo install` builds have no helper

The crates.io package excludes `/devsbd/` and `/scripts/`, and
`.cargo/config.toml` doesn't apply to `cargo install`, so such builds embed
empty blobs. Everything helper-based is then off: the ssh-agent falls back to
the bind mount ([ssh-agent.md](ssh-agent.md)), and notifications, control
requests, runs, port forwards and the boot hook are unavailable. Release
archives and the npm package carry the helper.

## Install

`install(container, recorded)` writes the blob through `exec -i -u root …
/bin/sh -c INSTALL_SCRIPT` stdin (no `docker cp`, no file binds: works on
every runtime, Apple `container` included), to a temp file renamed to
`BIN` = `/run/devsandbox/bin/devsbd` so a concurrent `version` never sees half
a binary. The script also creates `/var/lib/devsandbox/outbox` and
`/var/lib/devsandbox/runs` (mode 1777: the sandbox user writes them) and
symlinks `/usr/local/bin/devsbd`; those three are best-effort.

- Skip: the installed helper's `version` hash already equals an embedded one.
- Arch order (`candidates`): the arch recorded on the instance
  (`Instance::devsbd_arch`), else the host's, then the other. Each candidate
  is written and kept if its `version` then reports the expected hash: one
  blind retry, no `uname` probe or `exec format error` matching.
- `ensure_or_reason` = `install` + `start_daemon` (`exec -d -u root … devsbd
  daemon`, idempotent via the daemon's pidfile lock, failures silent).
  `ensure` prints the reason as a note; `ensure_recorded` also persists a
  changed `devsbd_arch` in `state.toml`. The helper is optional: no failure
  here fails a command.

## When it is (re)installed

- `run` (`materialize`): `ensure_or_reason` after the container is created,
  before lifecycle commands; the arch is saved on the `Instance`.
- `start`: `ensure_recorded` after `start` (on `/run` in the writable layer
  the binary survives a restart but the daemon does not; images with a tmpfs
  `/run` lose both).
- `port`: `ensure_recorded` for the instance, `ensure` for a service
  container it forwards into.
- The serve daemon: `Bridges::reconcile` calls `ensure_recorded` before every
  bridge (re)spawn, so a CLI upgrade reaches running instances without a
  restart ([bridges.md](bridges.md)).

A hash change (any rebuild of the helper) reinstalls; the protocol
`VERSION` alone doesn't decide it.

## `relay_mode`

`relay_mode(info)`: unix host, helper installed (`devsbd_arch` set), and no
ssh-agent bind mount recorded (`ssh_auth_sock` none). Instances created with
the mount stay in mount mode until recreated. See [ssh-agent.md](ssh-agent.md).

Limitations: [status.md](../../status.md#helper)
