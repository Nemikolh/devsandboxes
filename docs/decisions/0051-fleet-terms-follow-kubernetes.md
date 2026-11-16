# 0051: The fleet's terms follow Kubernetes

Status: accepted (2026-11-16)

## Context

devsandbox's fleet grew roles that Kubernetes already names: a per-host
agent, a coordinating role that places work, a signing key that decides
membership. Ours had names of their own, the coordinating one `hub`,
so a user who knows Kubernetes had to learn a second vocabulary for the
same shapes ([kubernetes](../reference/kubernetes.md) maps the rest).

The obvious candidate for `hub` was `scheduler`. It is wrong:
kube-scheduler only binds pods to nodes; it relays nothing and keeps no
state. Our role federates every member's view (`snapshots::merge`), is
the build members follow (ADR 0042), saves the fleet's rows
(`machines::saved`), never idles out, and places runs.

## Decision

- **The role is the control plane.** `member.toml`'s `control_plane =
  "self" | "<name>"` (`fleet::member::ControlPlane`), `devsandbox machine
  control-plane <m>` to move it, `machine add --control-plane` to pick it
  for a new fleet, `control_plane` in the API, the membership log and the
  links' `Hello`. Its placement (`place`) is its scheduler function, not
  a role of its own.
- **The authority keeps its name.** It is the cluster CA's analog, a role
  of its own that may sit on another member (ADR 0044).
- **No compatibility.** Pre-1.0: an older `hub` key, log entry or `Hello`
  isn't read; a fleet made before is made again.
- **Earlier ADRs keep saying hub.** They are append-only; read `hub`
  there as the control plane.

## Consequences

- Kubernetes vocabulary is the default for new fleet concepts;
  `machine` → `node` is the likely next rename
  ([control-plane](../plans/active/control-plane.md)).
- Existing fleets must be made again after upgrading.

Source: docs/reference/kubernetes.md, docs/plans/active/control-plane.md.
