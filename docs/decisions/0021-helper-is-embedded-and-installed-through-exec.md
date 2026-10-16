# 0021: The in-container helper is embedded in the CLI and installed through `exec` stdin

Status: accepted

## Context

Bridges, ssh-agent relay, notify, the control API, runs and port forwards
all need a process inside each container (`devsbd`). It could be downloaded
in the image, bind-mounted from the host, copied with `docker cp`, or
shipped inside the CLI.

## Decision

- Static musl `devsbd` binaries for x86_64 and aarch64 Linux are built
  separately (`scripts/build-devsbd.sh` or CI) and embedded in the
  `devsandbox` binary by `build.rs` from `DEVSANDBOX_DEVSBD_DIR`.
  `build.rs` never invokes cargo; a missing helper embeds an empty blob so
  plain `cargo build` works (`DEVSANDBOX_DEVSBD_REQUIRED=1` makes it fatal).
- Installation writes the binary through `exec -i -u root … /bin/sh -c`
  stdin (`src/devsbd.rs`, `INSTALL_SCRIPT`), checked by build hash so an
  up-to-date helper is a no-op (`devsbd::ensure_recorded`).

## Consequences

- Works offline, on any distro image (static), on Apple `container` (which
  can't file-bind), and needs no `docker cp`.
- Helper and CLI are version-locked by construction.
- Nested cargo from `build.rs` was rejected: it deadlocks on the target
  lock, leaks env into the inner build, reruns on every check, and can't
  work from the crates.io tarball.
- Images need `/bin/sh` and root `exec`; distroless images get no helper.

Source: docs/sandbox-helper.md, "Why embed instead of install-from-network / mount", "Build + embedding".
