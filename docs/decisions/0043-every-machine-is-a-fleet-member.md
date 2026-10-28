# 0043: Every machine is a fleet member; how far the hub reaches is the member's setting

Status: proposed (fleet plan step 4b). Accepted when step 4b ships; it
then supersedes 0041 and the direction of 0020 (ssh and the protocol
match stay).

## Context

Fleet steps 1-3 shipped two kinds of machine: ssh machines (`machine
add`: this host sshes to it, sees its own config roots, never places on
it) and offered workers (`fleet join`: it dials the hub, lends a fleet
root, its own roots invisible to the hub; 0041). Two ways to add a
machine, and configs that live on a worker can't be seen or run by the
rest of the fleet. The user wants one way: configs live on any machine,
every machine sees the others', and the hub can be swapped.

## Decision

- **One kind.** Every machine is a member of one fleet; one member is
  the hub, which holds the member list, the federated view and
  placement. `machine add` joins the target to this host's hub, or this
  host to the target's, and makes this host the hub when neither has
  one; two different hubs are refused.
- **Members dial the hub.** Each member holds one link to the hub,
  `ssh <hub> devsandbox fleet-link` (0041's direction, now for every
  member). Nothing dials a member after `machine add`'s one install
  ssh; every stream to a member rides its link.
- **Trust is the member's, set only on it.** `worker` (default): the
  hub runs and drives the member's own configs on it, as those configs
  say (0002), but never edits them or moves them off unless the request
  started on the member; other members' configs placed there run in the
  fleet root under the worker policy; `member.*`, `machines.*`,
  `shutdown` and host paths stay out of reach. `full`: the member's
  whole API. No link carries the setting.
- **The hub is swappable.** `machine hub <m>` repoints every member; the
  new hub rebuilds its view from the members (its state handed over
  later, fleet step 10).
- **One user.** As in 0041.

## Consequences

- One `machine add`; a laptop still never accepts ssh: it can join by
  running `machine add <any member>` itself.
- `worker` bounds a compromised hub to what the member's own configs
  already expose plus contained placed work. It does nothing against an
  ssh into the member, which the tool says where trust is set and shown.
- A member's calls to the hub get the hub's full API: trust protects a
  member from the hub, not the fleet from a member.
- `exec`, terminals, forwards and the agent to other machines need fleet
  step 4's channels before ssh machines can go.

Source: docs/plans/active/fleet.md, "One kind of machine".
