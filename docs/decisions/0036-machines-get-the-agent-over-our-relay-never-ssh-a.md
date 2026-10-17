# 0036: Machines get the agent over our own relay, never `ssh -A`, and only when asked

Status: accepted

## Context

Every ssh to a machine passed `-A`, so while any link was up, root on the
machine (and whatever its sandboxes reached) could sign with your keys,
whether you needed git there or not. `ssh -A` can't be switched off for a
live session, and which agent a long-held link carried was decided at
connect time.

## Decision

- No ssh to a machine passes `-A` (`serve::machines::ssh`). A machine's
  `agent` setting in `machines.toml` (off by default; `machine add
  --agent`, `machine set <name> --agent on|off`) decides whether it gets
  this host's agent at all.
- With `agent` on, the local daemon runs a relay per machine while the
  machine is wanted: the hidden `devsandbox agent-relay` there, over its
  own ssh, speaking the helper's frame protocol (0019, 0025) with the
  machine in the in-container daemon's role. This side checks the
  machine's current setting on every agent stream, so switching it off
  takes effect at once (the relay is killed, no reconnect).
- On the machine the relay's socket is found, not registered: processes
  there try live `agent-*.sock` files in the socket dir, so the remote
  daemon, its bridges and commands run there all pick it up; an exited
  relay drops out by the liveness check.
- What keeps a machine wanted, and so its relay up: a dashboard, a
  forwarded verb, a CLI command re-run there (`machines.hold`, held for
  the ssh's life).
- Remote output before our first bytes (a login shell's motd) is skipped
  up to a sentinel line, on the API link and the agent relay alike.

## Consequences

- Forwarding keys to a host is an explicit, per-host, revocable choice.
- A CLI re-run on a machine with `agent` on is a child of devsandbox, not
  an `exec`: the parent holds the daemon connection and ignores
  SIGINT/SIGQUIT while ssh runs.
- An agent check on the machine costs a stream through the relay (a
  round trip to this host's agent).

Source: docs/architecture/daemon/machines.md, *Agent*.
