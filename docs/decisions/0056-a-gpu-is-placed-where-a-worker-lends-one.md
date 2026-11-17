# 0056: A GPU is placed where a worker lends one

Status: accepted (2026-11-17).

## Context

What a sandbox needs of its host beyond the worker policy's defaults was
a tie: it kept the sandbox home whatever a worker could give, until
named host resources (fleet plan decision 41) let members say what they
provide. That design waits on how a config moves to resource names and
how a user sets a member's resources. A GPU needs no host path, so none
of that blocks it.

## Decision

- **A worker says what it can give and whether it lends it**: the
  capacity report's `gpu` (`capacity::GpuReport`: a usable GPU per
  `Backend::gpu_support`, its `/dev/nvidia<N>` count on a native engine,
  `lent` when `[allow] gpus = true` or `trust = "full"`).
- **Placement filters on it** (`place::gpu_check`): a required GPU (the
  sandbox's or a listed service's, `placed::gpu_need`) goes only to a
  candidate whose report has one it lends, or to the requester, whose
  own run is under no worker policy; a pin elsewhere is
  `NoPlace::PinNoGpu`. An optional GPU is a score bonus (`place::GPU`).
  A required GPU is no longer a tie (`code::ties`); the worker's
  `check_run` still enforces it.
- **Allowances stay the worker's own grant**: `member.set {allow}` is
  local-only, so a peer never sets them; `machine allow` sets them on
  this host and `machine add` asks (or takes `--allow …`) and applies them
  on the target over the add's own ssh session.

Fleet plan decision 78 records the same choice ahead of decision 41.

## Consequences

- A GPU is the first resource placement reasons about per worker; decision
  41's named resources can fold it in rather than replace it.
- A worker whose report is missing or stale counts as having no GPU, so a
  required-GPU run may stay home (with a note) when a GPU machine is just
  slow to report.
- Lending is all-or-nothing per host: every GPU, to every member's placed
  work.
