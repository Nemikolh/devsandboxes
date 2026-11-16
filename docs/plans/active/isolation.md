# Isolation (gVisor)

Status: planned, nothing built.

Every sandbox today gets the runtime's default isolation: runc on
docker/podman (shared host kernel), a lightweight VM per container on Apple
`container`. A sandbox running untrusted code or an unsupervised agent gets
the same boundary as one you're editing by hand.

## Goal

A sandbox or template names the OCI runtime that isolates it,
`isolation = "gvisor"` (Kubernetes' RuntimeClass analog,
[kubernetes.md](../../reference/kubernetes.md)). gVisor first; Kata
(`isolation = "kata"`) is a later follow-up. Absent means the runtime's
own default (runc, or Apple `container`'s VM). A host that can't satisfy
it refuses the `run`, never falls back silently.

## Shape

- **gVisor is an OCI runtime** (`runsc`): docker runs it with
  `--runtime=runsc` once registered under `runtimes` in
  `/etc/docker/daemon.json`, podman with `--runtime <path to runsc>`.
- **Linux only** (x86_64, arm64). Platforms: systrap (the default, best
  inside VMs), KVM (bare metal or nested virtualization), ptrace. On macOS
  only by installing `runsc` inside Docker Desktop's Linux VM
  (community-reported, not officially documented).
- **Apple `container`** already runs a VM per container; `gvisor` there is
  refused, not treated as satisfied (open question below).
- **A probe, a `Backend` capability**
  ([ADR 0012](../../decisions/0012-runtime-differences-are-backend-capabilities.md)):
  whether the runtime has `runsc` registered (docker: `docker info`'s
  runtimes; podman: a `runsc` it can find). Callers branch on the
  capability, never on the runtime name; the `run` argv gains the
  runtime flag in one place.
- **Hashed config**: `isolation` is part of the merged table, so changing
  it is drift (`rebuild`).
- **`privileged = true` with `isolation = "gvisor"`** conflicts: refused
  at config load.
- **Fleet**: `member.toml`'s `[allow]` already gates what a worker accepts;
  the worker policy, which refuses flags it doesn't know, has to let the
  runtime flag through (it only adds isolation), and a worker could later
  require it for placed work.
- **What runsc changes**: networking is its own netstack by default
  (`--network=host` is the opt-out); GPUs need `--nvproxy` and a supported
  host driver ([gpus](gpus.md)); `runsc checkpoint|restore` exists, which
  runc can't do for fleet step 14 ([fleet](fleet.md)).
- **Cost**: CPU-bound work is near native; syscall-heavy work (`git
  status` on big trees, `npm install`, many forks) is noticeably slower;
  each sandbox carries a fixed memory overhead.
- **Friction to verify under runsc**: the helper's `exec` path (install
  and `devsbd bridge`), the ssh-agent relay, sessions, bind-mount
  performance (the workspace is a bind), some `/proc`/`/sys` and `ioctl`
  gaps, docker-in-docker features (and `privileged`, refused above).

## Rollout

1. **Probe**: the `Backend` capability for gVisor on docker and podman,
   with its parser unit-tested; Apple `container` answers no.
2. **Config key and refusal**: `isolation` on `[sandbox.*]` and
   `[template.*]` (`"gvisor"` only for now), the run argv's runtime flag,
   the refusals (host without it, Apple `container`, `privileged`), drift;
   the config spec skill updated.
3. **Verify under runsc**: a `docker_test` gated on the capability that
   runs a sandbox with `isolation = "gvisor"` and checks the helper
   installs and bridges, a session attaches, a forward carries bytes and
   the ssh-agent relay answers; fix what fails or record it in
   `status.md`.
4. **Docs**: runtimes.md and the config spec for the key, the site's
   user page (installing runsc, the macOS route).

Kata (`isolation = "kata"`) follows the same steps once gVisor ships.

## Open questions

- Apple `container`: refuse `gvisor` (proposed: the user asked for a
  specific runtime), or accept its VM as at least as strong?
- Per sandbox only, or per instance (a dispatcher's children stricter
  than the dispatcher)?
- Does an isolated sandbox also mean default-deny egress
  ([sandbawks-ideas](../explore/sandbawks-ideas.md))?
- Pass runsc flags through (platform, `--nvproxy`), or pick them for the
  user?
