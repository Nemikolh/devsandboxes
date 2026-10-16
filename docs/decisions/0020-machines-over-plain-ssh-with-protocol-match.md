# 0020: Remote machines are reached over plain ssh, and must match the protocol

Status: accepted

## Context

devsandbox can manage sandboxes on other hosts. That needs transport,
authentication, environment and version-skew rules, and must cost nothing
for users who never add a machine.

## Decision

- Transport is the user's own `ssh` (`serve::machines::ssh`, one argv
  builder), no ControlMaster of devsandbox's own: `~/.ssh/config` decides.
  The remote runs `devsandbox api --stdio` through the login shell, so the
  remote env is what a login gives; relayed verbs get the remote's env.
- A link says hello as `0.0.0` (never hands off the remote daemon, 0015) and
  requires an equal `PROTOCOL`; a mismatch fails `machine add` or marks the
  machine unreachable. Different semvers within one protocol only warn.
- Quick calls share one held connection per machine, connected only while
  wanted plus a 60 s linger (`machines::service`); each forwarded verb gets
  its own ssh.
- The local daemon federates: it watches remote roots on the held link and
  merges their snapshots (push, `snapshots::merge`).
- Bare names resolve across all machines (`<machine>/<name>`, `@/<name>` for
  local); `machines.toml` is written only by the daemon. The CLI reads it
  only to know whether any machine exists, so no machines = no cost.

## Consequences

- No new network service or credential store; ssh auth, jump hosts and
  multiplexing are the user's ssh config.
- Both ends must be upgraded together across protocol bumps.

Source: docs/machines.md, "Decisions (the user's)".
