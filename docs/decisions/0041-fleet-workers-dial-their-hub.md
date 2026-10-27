# 0041: Fleet workers dial their hub; a hub reaches a worker only through the lent API

Status: accepted

## Context

A fleet pools the user's machines: an always-on hub places work on
laptops that lend capacity while awake. Machines (0020) are reached by an
ssh *to* them, which a laptop shouldn't need to allow (no sshd exposure, no
hub key in its `authorized_keys`), and a machine link reaches its daemon's
whole API: every root, every instance, `machines.*`. A worker must expose
only what it lends.

## Decision

- **Workers dial the hub.** A worker's daemon holds `ssh -T -o
  BatchMode=yes <hub> devsandbox fleet-link --worker <name>`
  (`serve::fleet::worker`), retried with the machine links' backoff while
  `lend.toml` says `on` or `drain`. ssh stays the transport (0020); only the
  direction flips. The hub never dials a worker: every stream it opens
  rides that link (`fleet::LINK`).
- **The lent API, not the daemon's.** Each stream the hub opens is a
  connection with the `Worker` table (`serve::fleet::lent`): hello (never a
  version handoff), `instances` subscriptions and watches, the quick calls
  and the verbs; never a `machine` param, federation, `machines.*`,
  `fleet.*`, `shutdown`, `helper.call`, forwards or the Inbox. Every path
  is under the worker's fleet root for that hub
  (`<data>/devsandbox/fleet/<hub>/`), canonicalized; every instance named
  or listed is one whose `config_dir` is under it; a verb runs with the
  worker's own env, never the hub's. The worker policy (fleet plan commit
  1e) adds what a placed config may ask of the host.
- **The worker owns its lending.** `lend.toml` is the worker's; the hub
  never writes it.
- **Config authority for placed instances sits with the hub.** A config
  root under the fleet root is the hub's, run as the hub resolved it;
  [0002](0002-config-is-the-security-boundary.md) stays as is for a host's
  own roots, which the hub never reaches.
- **One user.** A fleet is one user's machines; sharing a fleet, or a
  worker, between users is out of scope.

## Consequences

- A laptop needs only ssh to the hub, which it has; the hub needs nothing
  on the laptop.
- What a compromised hub can do to a worker is bounded by the lent API and
  the worker policy: run containers that see only what was lent.
- Things that are an ssh *to* a machine today (terminals, forwards, CLI
  re-runs, the agent) need channels on the worker's link (fleet plan step
  4) before they work on an offered machine.

Source: docs/plans/active/fleet.md, "Topology: workers dial the hub" and
"The lent API is not the daemon's API".
