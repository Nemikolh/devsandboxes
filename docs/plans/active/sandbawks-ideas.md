# Ideas from sandbawks

Status: ideas, none agreed. Each would get its own plan if picked up.

Things [sandbawks](https://github.com/hypotheosis/Sandbawks) does that
devsandbox could borrow, independent of running on it
([sandbawks-runtime](sandbawks-runtime.md)).

- **Process checkpoints.** sandbawks pauses a sandbox with
  `runsc checkpoint` (memory, processes, files) and resumes it elsewhere.
  Fleet step 14 checkpoints WIP only: "Process state is lost"
  ([fleet](fleet.md)). Options: gVisor sandboxes ([isolation](isolation.md)),
  or CRIU via `podman container checkpoint --export` (fragile with open TCP
  connections; nothing on Apple `container`). Would make `devsandbox move`
  keep a running agent.
- **Idle pause.** sandbawks pauses after an idle timeout (activity = calls
  through its gateway) and resumes on the next connect. devsandbox could
  stop (or checkpoint) idle instances to free a laptop's memory, with
  activity from `exec` sessions, forwards and the helper.
- **Warm pools.** `SandboxWarmPool` keeps N ready sandboxes; a claim is a
  reassignment, not a create. For devsandbox: pre-built, pre-started
  instances per sandbox so `run` (and a dispatcher's `ensure`) only cuts a
  worktree and binds it. Bind mounts are fixed at create, so this needs the
  worktree in a volume (parked in [status](../../status.md)) or a late
  mount.
- **Clone.** `restoreFrom` starts a new sandbox from another's snapshot,
  to fork an agent's environment. devsandbox analogue: `run --from
  <instance>` = new worktree at the source's WIP plus a copy of its
  volumes.
- **Egress policy.** Every sandbawks sandbox starts deny-all, with an
  `egressAllow` list per template/request. devsandbox has no egress
  control; a natural part of an `untrusted` level
  ([isolation](isolation.md)), e.g. an internal network plus an allowlisting
  proxy service.
- **"Not ready" signals for agents.** sandbawks' gateway answers a call to a
  sandbox that's still starting with an in-band `NOTICE` (exit 75 transient,
  distinct code for terminal) instead of a hang or opaque error, and honors
  a leading `sleep`. devsandbox's `exec` against a building or stopped
  instance could give agents the same actionable answer.
- **Usage records.** Per-instance CPU/memory-time, for knowing what
  dispatchers cost across the fleet.
