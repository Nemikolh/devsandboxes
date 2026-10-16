# 0016: Verbs run as `devsandbox --in-process` subprocesses, one request each

Status: accepted

## Context

`run`, `rebuild`, `start`, `stop`, `rm` and `service rebuild` are served by
the daemon so the dashboard, CLI and npm clients can start, watch and
cancel them. Running them on daemon threads would let one panic or hang
take down the daemon, block version handoff, and give verbs the daemon's
environment instead of the caller's.

## Decision

- Each verb request spawns `devsandbox --in-process <verb> …`
  (`serve::verbs`, a pure `argv` from typed params) in its own process
  group, with `env_clear()` + the caller's whole env from the request +
  `GIT_TERMINAL_PROMPT=0`, and null stdin.
- Output is spooled to `logs/verbs/<pid>.out|.err` and streamed as
  `output`/`progress`. A new daemon's start sweep adopts live verb pids
  (`InFlight::adopt`) and drops dead ones' files.
- One connection per verb request; no job/task concept is user-facing.
- Every prompt happens client-side before the request is sent
  (`serve::verbs_client`: `sandboxes.list`, `instances.resolve`, rm's
  `confirm` round).
- `--in-process` never lazy-starts a daemon and never fans out to machines.
- No daemon reachable → the CLI warns and runs the verb in-process.

## Consequences

- A crashing verb kills one request; handoff doesn't wait for verbs.
- `${localEnv:…}`, `SSH_AUTH_SOCK`, `GIT_*` behave exactly as from a shell.
- A verb that needs input must get it as a parameter, never from stdin.

Source: docs/verbs-as-requests.md, "Decisions"; docs/client-server.md, "Execution, internally".
