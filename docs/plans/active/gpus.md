# GPUs

Status: idea, nothing built, direction not agreed.

No sandbox can use a GPU today. `runArgs` (where a devcontainer puts
`--gpus all`) and `hostRequirements` (`gpu`) parse but are ignored with a
warning at `run` (`SandboxProperties::ignored`, test
`unimplemented_properties_are_flagged_ignored` in `src/config.rs`), and no
backend emits `--gpus` or `--device`.

## Goal

A sandbox asks for a GPU in config and gets one on runtimes that can give
it, refused with a clear reason elsewhere.

## Shape

- **Config**: honor `hostRequirements.gpu` (`true` | `"optional"` |
  `{ cores, memory }`) instead of warning, and/or a devsandbox key
  (`gpus = "all" | ["0", "1"]`). Config stays the security boundary
  ([ADR 0002](../../decisions/0002-config-is-the-security-boundary.md)):
  device access comes only from config, never from the repo's
  `.devcontainer.json` unchecked.
- **Runtime capability**: `supports_gpus` on `Backend`, callers branch on it
  ([ADR 0012](../../decisions/0012-runtime-differences-are-backend-capabilities.md)).
  - docker: `--gpus <spec>` (NVIDIA Container Toolkit).
  - podman: CDI, `--device nvidia.com/gpu=all`.
  - Apple `container`: none.
- **Probe**: detect a usable toolkit/CDI spec once per host (like the
  identity probe) so `run` fails early instead of at `docker run`.
- **Drift**: the GPU request is part of the hashed config, so changing it
  is drift (`rebuild`).
- **Fleet**: a GPU is a named host resource (fleet plan decision 41);
  placement must only pick members that have one, and the capacity report
  needs a GPU count. Until then a GPU sandbox stays home.

## Open questions

- `hostRequirements` only, a devsandbox key, or both?
- `runArgs` pass-through (parked in [status](../../status.md)) would
  give GPUs for free on docker but bypasses the capability model; take
  this narrower path instead?
- GPU under gVisor: needs `runsc --nvproxy` and a supported host driver
  version; see [isolation](isolation.md).
- Checkpointing GPU state (`cuda-checkpoint`) is out of scope here; see
  [sandbawks-ideas](sandbawks-ideas.md).
