# 0038: Sandboxes run as the host user, planned per runtime mode

Status: accepted.

## Context

devcontainer.json says *how* to pick a user (`remoteUser`, `containerUser`,
`updateRemoteUserUID`), and the same setting means opposite things on
rootful docker, rootless docker, rootless podman and VM-backed runtimes:
files written into binds came out owned by root or a subuid depending on
the host.

## Decision

- `identity = "host"` is the default: inside, you are a user with the
  host's uid/gid. The config's `remoteUser` (else `containerUser`, else
  the image's `USER`) is the account adopted: name, home and shell kept,
  only its ids change. `identity = "image"` takes users literally;
  `updateRemoteUserUID = false` is a hint for it.
- The runtime's mode is probed per runtime context (metadata first, a
  canary container when ambiguous), cached in `state.toml`, and a pure
  table turns `(mode, identity, account)` into a plan: rootful remaps the
  account to the host ids, rootless docker execs as root (root is you),
  rootless podman uses `keep-id`, VM-backed runtimes need nothing. A
  machine probes for itself.
- The remap happens at create, in the container layer, by the helper
  (`devsbd identity apply`), never in an image layer: one image works on
  every host.
- No idmapped mounts. The adopted account's real ids come from the
  image's `/etc/passwd` (read once per image id); 1000:1000 is assumed
  only when it can't be read.
- With no account to adopt, the sandbox runs as root with a warning
  recommending `common-utils`; devsandbox doesn't add it.
- Services get no identity treatment: they share no mounts with sandboxes.
- Docker and podman are tier 1; Apple `container` and other VM-backed
  runtimes get the no-op plan, best effort.

## Consequences

- Workspace, worktree and cache files come out owned by the host user on
  every tier-1 mode; on rootless docker execs run as root.
- Configs written for root-in-container (`/root/...` mounts) break under
  the default and need the migration (0040).
- An image whose account isn't 1000:1000 and has no readable passwd gets a
  warning, not a fix.

Source: the identity plan, steps 1–6 and 11.
