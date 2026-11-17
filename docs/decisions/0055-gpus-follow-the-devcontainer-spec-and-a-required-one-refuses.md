# 0055: GPUs follow the devcontainer spec, and a required one refuses

Status: accepted (2026-11-17).

## Context

Sandboxes had no way to use a GPU: `hostRequirements` and `runArgs`
parsed and were ignored. The devcontainer spec asks for one with
`hostRequirements.gpu` (`true` | `"optional"` | `{ cores, memory }`), and
the devcontainers CLI then adds `--gpus all` when docker lists an
`nvidia` runtime, warning and running without one otherwise. Configs
ported from devcontainer.json should keep working.

## Decision

- **The spec's key, nothing of our own**: `hostRequirements.gpu` on a
  sandbox (`config::HostRequirements`, `GpuRequirement`), and the same key
  on a `[services.*]` table as a devsandbox extension. No `gpus = …`
  device selection and no `runArgs` pass-through: GPUs are a backend
  capability ([ADR 0012](0012-runtime-differences-are-backend-capabilities.md),
  `Backend::gpu_support`), docker's `--gpus all` when its `nvidia`
  runtime is registered, else NVIDIA CDI `--device nvidia.com/gpu=all`
  (docker, podman), none on Apple `container`.
- **A required GPU that can't be had refuses the run**
  (`commands::run::gpu_decision`), before anything is built: stricter
  than the devcontainers CLI, because a sandbox that needs a GPU and
  silently runs without one fails later and further from the cause.
  `"optional"` keeps upstream's behavior: a warning, the container runs
  without.
- **Under a worker policy a GPU is an allowance** (`[allow] gpus = true`):
  `check_run` lets exactly those two flags through, any other `--device`
  stays refused.

## Consequences

- A devcontainer.json's `hostRequirements.gpu = true` refuses on a host
  without a usable GPU where VS Code would warn; `"optional"` is the
  portable spelling for "use one if there is one".
- `cores` / `memory` parse and are warned as not enforced; every GPU goes
  to the container.
- Detection reads the runtime's metadata, not a test container, so a host
  whose toolkit is installed but misconfigured (rootless docker without
  `no-cgroups = true`) fails at the runtime's create.
