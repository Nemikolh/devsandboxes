# Isolation levels (gVisor)

Status: idea, nothing built, direction not agreed.

Every sandbox today gets the runtime's default isolation: runc on
docker/podman (shared host kernel), a lightweight VM per container on Apple
`container`. A sandbox running untrusted code or an unsupervised agent gets
the same boundary as one you're editing by hand.

## Goal

A sandbox declares how much it's trusted, and devsandbox picks the
strongest runtime the host offers for that level, or refuses.

## Levels (strawman)

| Level | Meaning | docker / podman | Apple `container` |
| --- | --- | --- | --- |
| `host` | today's default | runc | VM (already stronger) |
| `untrusted` | user-space kernel | `--runtime=runsc` (gVisor) | VM |
| `vm` | hardware boundary | Kata (`--runtime=kata`) | VM |

Config: `isolation = "untrusted"` on a sandbox or template. A level the host
can't satisfy refuses the `run`, never falls back silently.

## gVisor on docker/podman

- Install `runsc` and register it as a runtime (`/etc/docker/daemon.json`
  `runtimes`, or podman's `--runtime` path); detected by a probe and
  exposed as a `Backend` capability
  ([ADR 0012](../../decisions/0012-runtime-differences-are-backend-capabilities.md)).
- Cost: CPU-bound work is near native; syscall-heavy work (`git status`
  on big trees, `npm install`, many forks) is noticeably slower; each
  sandbox carries a fixed memory overhead.
- Known friction: `privileged` and docker-in-docker features, some
  `/proc`/`/sys` and `ioctl` gaps, bind-mount performance (the workspace
  is a bind), the helper's `exec` path and ssh-agent relay must be
  re-verified under `runsc`.
- GPUs need `--nvproxy` and a supported host driver ([gpus](gpus.md)).

## How it fits what exists

- `member.toml` `[allow]` (e.g. `privileged`) already gates what a worker
  accepts; a level is a natural allowance: a worker can require
  `untrusted` or stronger for placed work.
- `privileged = true` and `isolation = "untrusted"` conflict: refuse.
- The level is hashed config: changing it is drift (`rebuild`).

## Open questions

- Levels by name, or name the runtime (`runtime = "runsc"`)?
- Per sandbox, or per instance (a dispatcher's children stricter than
  the dispatcher)?
- Does `untrusted` also mean default-deny egress
  ([sandbawks-ideas](sandbawks-ideas.md))?
- Bonus of gVisor: memory checkpoint/restore (`runsc checkpoint`), which
  fleet step 14 can't do with runc ([fleet](fleet.md)).
