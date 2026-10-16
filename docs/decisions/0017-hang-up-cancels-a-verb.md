# 0017: Hang-up cancels a verb; `detach` opts out

Status: accepted

## Context

A verb request (0016) is held on one connection. When the client goes away
the daemon must decide: keep the verb running or stop it. The CLI's Ctrl-C
must still mean cancel, but the dashboard must be able to quit without
killing a `stop` or `rm` it started.

## Decision

- Closing the connection cancels: the daemon SIGINTs the verb's process
  group; the verb rolls back (0007). A second SIGINT kills.
- CLI Ctrl-C: the first sends `instances.cancel` on a second connection
  and keeps streaming the rollback; the second hangs up.
- `detach: true` waives the hang-up's cancel once the request is answered.
  `run` and single-instance `rebuild` with `detach` answer once the
  instance is registered; `start` and `rebuild --all` right after the
  spawn (`serve::verbs::detach_at`); the dashboard always detaches those and sends its held
  `stop`/`rm`/`service rebuild` with `detach` so they survive a quit.
- A daemon drain (handoff) or a slow reader is not a hang-up: a local verb
  runs on and the next daemon adopts it. A non-detached verb forwarded to
  a machine is the exception: it's never adopted, so a drain cancels it.

## Consequences

- No orphaned half-done verbs from crashed CLIs; the dashboard's actions
  outlive the dashboard.
- Clients must choose `detach` deliberately.

Source: docs/verbs-as-requests.md, "Decisions"; docs/before-machines.md, "Decisions" (Held verbs); docs/dashboard-client.md, "Decisions".
