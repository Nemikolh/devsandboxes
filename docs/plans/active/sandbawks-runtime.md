# sandbawks as a runtime

Status: idea, nothing built, direction not agreed.

[sandbawks](https://github.com/hypotheosis/Sandbawks) is an E2B-compatible
sandbox control plane on Kubernetes: gVisor-isolated Pods (Kata optional),
warm pools, and pause/resume that checkpoints memory, processes and files to
S3 (`runsc checkpoint` + a containerd shim that restores at start). Its
in-sandbox agent `envd` serves exec, filesystem and PTY over gRPC; clients
use the E2B SDKs.

## Goal

`DEVSANDBOX_RUNTIME=sandbawks` (or a per-sandbox runtime) runs instances on
a sandbawks cluster: capacity beyond the user's own machines, stronger
isolation, and instances that pause and resume with their processes alive.

## Fit with `Backend`

| devsandbox needs | sandbawks has | Gap |
| --- | --- | --- |
| build an image | templates (`SandboxTemplate`) from an image | no build; push to a registry first |
| bind-mount the worktree | none; files via `envd` or PVCs | code must move in, as fleet step 12 moves it ([fleet](fleet.md)) |
| `exec -it` | `envd` process/PTY RPCs | a client in Rust, or the helper over an `envd` exec |
| `devsbd` bridges | none | the helper runs inside; its frames need a stream (`envd` PTY or a port) |
| services, networks | none (one Pod per sandbox) | services unsupported, or as extra containers in the Pod |
| `stop`/`start` | `pause`/`connect` (memory kept) | better than today |
| events, `list`, `stats` | the `Sandbox` CR status | poll the gateway |

Much of `Backend` is docker-shaped argv (`run`, `build`, `exec`); this
backend is an API client, so it's a different kind of backend, closer to a
fleet member than to podman.

## Alternative: a fleet member

Instead of a `Backend`, a daemon that fronts a sandbawks cluster joins the
fleet as a member with capacity, and placement sends work there. Reuses
code transfer, routed streams and fetch-back; the member translates verbs to
sandbawks calls.

## Open questions

- Backend or fleet member?
- Is losing services/bind mounts acceptable for the sandboxes that would run
  there (agent children of a dispatcher, not hand-edited ones)?
- Auth: a `TeamAPIKey` per user, stored where?
- One client for zidane: `devsandboxes-zidane-extension` and sandbawks'
  connection contract (`docs/sandbox-connection-ux-contract.md` there) are
  two session environments for the same harness.
