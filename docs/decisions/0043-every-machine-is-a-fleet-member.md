# 0043: Every machine is a fleet member; how far others reach it is the member's setting

Status: proposed (fleet plan step 9). Accepted when step 9 ships; it
then replaces 0041, removed with it (never released), and 0020's
one-hop machine links (ssh and the protocol match stay).

## Context

Machines were two kinds: ssh machines (`machine add`: this host sshes
to it, sees its own config roots, never places on it) and offered
workers (`fleet join`: it dials the hub, lends a fleet root, its own
roots invisible to the hub). Two ways to add a machine, and configs on a
worker can't be seen or run by the rest of the fleet. Wanted: one way,
configs on any machine, every machine seeing the others', a swappable
hub.

## Decision

- **One kind.** Every machine is a member of one fleet; one member is
  the hub, a role (member list, link graph, placement), not a kind of
  machine. `machine add` joins the target to this host's fleet, or this
  host to the target's, and makes a fleet of two with this host as hub
  when neither has one; two different hubs are refused.
- **A link is an ssh pair.** `machine add X` on A makes the link A → X,
  held by A's daemon (`ssh X devsandbox fleet-link`), whichever of them
  is the hub. Either end opens streams on it; every stream between
  machines rides a link.
- **Routed over the graph.** A stream to a machine with no direct link
  is spliced hop by hop along a path; the requester tries paths in turn
  and fails over; with every path down the machine is unreachable.
- **Trust is the member's, set only on it.** `worker` (default): a
  remote caller, the hub included, runs and drives the member's own
  configs on it, as those configs say (0002), but never edits them or
  moves them off unless the request started on the member; other
  members' configs placed there run in the fleet root under the worker
  policy; `member.*`, `machines.*`, `shutdown` and host paths stay out of
  reach. `full`: the member's whole API. No link carries the setting.
- **The hub is swappable.** `machine hub <m>` moves the role; links stay
  and `m` rebuilds its view from the members (its state handed over
  later, fleet step 16).
- **One user.** A fleet is one user's machines; sharing one between
  users is out of scope.

## Consequences

- One `machine add`; a laptop still never has to accept ssh: it joins by
  running `machine add <any member>` itself.
- `worker` bounds what another machine can do through devsandbox to what
  the member's own configs already expose plus contained placed work.
  Not through ssh: a machine with a link into the member has a shell
  there. The graph shows when the hub (or a `full` member it reaches)
  holds one, and the tool says so where trust is set and shown.
- A relaying machine sees and carries other members' streams; a laptop
  hop drops paths when it sleeps, so always-on hops are preferred.
- `exec`, terminals, forwards and the agent to other machines need fleet
  step 4's channels before the per-use ssh argvs can go.

Source: docs/plans/active/fleet.md, "Members, links and the hub"
through "Trust", rollout steps 5-9.
