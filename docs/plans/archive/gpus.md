> Archived 2026-11-17. Historical, may be wrong; do not use as a reference.
> Current: [architecture/runtimes.md#gpus](../../architecture/runtimes.md#gpus),
> [architecture/daemon/fleet.md#placement](../../architecture/daemon/fleet.md#placement),
> [ADR 0055](../../decisions/0055-gpus-follow-the-devcontainer-spec-and-a-required-one-refuses.md),
> [ADR 0056](../../decisions/0056-a-gpu-is-placed-where-a-worker-lends-one.md)

# GPUs

Status: shipped (c121bc0, 06bf906, 6951613, 91aebf5, 7663a16, 58f8e67,
2c25b8b, 855d759, c308945).

No sandbox can use a GPU today. `runArgs` (where a devcontainer puts
`--gpus all`) and `hostRequirements` (`gpu`) parse but are ignored with a
warning at `run` (`SandboxProperties::ignored`, test
`unimplemented_properties_are_flagged_ignored` in `src/config.rs`), and no
backend emits `--gpus` or `--device`.

## Goal

A sandbox (or a service) asks for a GPU in config with the devcontainer
spec's `hostRequirements.gpu` and gets one on runtimes that can give it,
refused with a clear reason elsewhere. Placed runs go only where a worker
has a GPU and lends it.

## Decisions

1. Config is the spec's `hostRequirements.gpu` only, compatible with
   devcontainer.json: `true` | `false` | `"optional"` |
   `{ cores, memory }` (a table counts as required; `cores` / `memory`
   parse and are warned as not enforced). `hostRequirements.cpus`,
   `memory`, `storage` keep the "ignored" warning. No devsandbox key, no
   `runArgs` pass-through: GPUs go through the backend capability model
   ([ADR 0012](../../decisions/0012-runtime-differences-are-backend-capabilities.md)).
2. Required and no usable GPU: `run` refuses, naming why (stricter than
   the devcontainers CLI, which warns and runs). `"optional"`: a warning,
   the container runs without one.
3. Services take the same `hostRequirements.gpu` key under
   `[services.*]` (a devsandbox extension: the spec has none for
   services). A required GPU a service can't get fails the run.
4. NVIDIA only, every GPU: docker `--gpus all` when its `nvidia` runtime
   is registered, else CDI `--device nvidia.com/gpu=all`; podman CDI;
   Apple `container` none. No device selection.
5. Worker policy: `[allow] gpus = true` lets exactly those two flags
   through `check_run`; any other `--device` stays refused.
6. Placement: a required GPU goes only to a candidate whose capacity
   report has a usable GPU **and** lends it (`gpus = true` allowed, or
   `trust = "full"`); the requester's own host needs only the GPU (its
   own config is under no worker policy). A GPU is handled ahead of fleet
   decision 41's general named resources (fleet plan decision 78).
7. Allowances are the worker's to grant: no peer or control plane sets
   them over a link. `devsandbox machine allow` acts on its own host only;
   `machine add` asks and applies them on the target through its own ssh
   session.

Out of scope: gVisor (`runsc --nvproxy`, see [isolation](../active/isolation.md)),
GPU checkpointing (see [sandbawks-ideas](../explore/sandbawks-ideas.md)),
GPU CI (a self-hosted runner, later).

## Steps

Phase 1, local GPUs:

1. **config**: a typed `HostRequirements` / `GpuRequirement` on
   `SandboxProperties` and on `Service` (`src/config.rs`). Still listed by
   `ignored()`: nothing acts on it yet. Docs: `config-toml-spec` skill,
   `docs/architecture/config.md`.
2. **runtime**: `GpuSupport` (`Gpus` | `Cdi` | `None(reason)`) in a pure
   `src/runtime/gpu.rs`, read with the backend's existing engine info
   (docker: `Runtimes.nvidia`, else an `nvidia.com/gpu` CDI spec in
   `CDISpecDirs` on a native engine; podman: a CDI spec in `/etc/cdi`,
   `/var/run/cdi`; Apple: none). Docs: `docs/architecture/runtimes.md`.
3. **run**: a pure decision (flags | refusal | warning), the flags next
   to `--privileged` in `run_container`; `gpu` leaves `ignored()`.
   Worker policy `Allow.gpus` in `check_run` and `config_refusals` (so a
   GPU sandbox is a placement tie until step 7). Docs:
   `instances/lifecycle.md`, fleet.md *Worker policy*, `status.md`,
   CHANGELOG.
4. **services**: the same decision in `ensure_services` /
   `service_run_args`; `config_refusals` covers services. Docs:
   `instances/services.md`.

Phase 3, placement:

5. **capacity**: `Capacity` gains `gpu: Option<GpuReport { count, lent }>`; wire
   (`api.md`, `index.d.ts`), `capacity::summary`. Docs: fleet.md
   *Capacity*.
6. **place**: `Spec` / `PlaceSpec` carry the GPU need; `place::place`
   filters (required; a pin without one is `NoPlace::PinNoGpu`) or
   scores (optional). Docs: fleet.md *Placement*.
7. **ties**: the GPU stops being a tie in `code::ties`; the worker's
   `check_run` still enforces it. Docs: fleet.md *In the run verb*.

Phase 4, allowances at add and join:

8. **`devsandbox machine allow`**: `member.set {allow}` (local clients
   only, as all of `member.*`), writing `[allow]` through
   `member::update`, and `member.get`'s `allow`; one CLI flag per
   allowance. Docs: machines.md, api.md.
9. **prompts**: `machine add` asks which allowances the target lends (or
   takes the same flags off a TTY) and applies them on the target over
   its ssh session; an inbound join asks on the joining host.
