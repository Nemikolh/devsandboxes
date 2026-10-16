# 0007: Record the instance first; failures stay `failed`, only a cancel rolls back

Status: accepted

## Context

`run` is long (image build, features, lifecycle hooks). An instance that
appears only at the end is invisible while it builds and can't be
cancelled by name; an instance that is cleaned up on every failure loses
the build log and the container you'd want to inspect.

## Decision

- `run` inserts a provisional record in `state.toml` before any work, with
  a phase, so the instance shows up (and can be cancelled) while it builds.
- Any failure keeps the instance, marked `failed` at the phase it reached,
  for inspection; the user `rm`s or `rebuild`s it.
- Only a cancel (Ctrl-C, `instances.cancel`, a hang-up, see 0017) rolls a
  fresh `run` back (`run::progress::on_failure`).

## Consequences

- Clients (dashboard, dispatcher `ensure`) must handle in-flight and
  `failed` instances, not just running/stopped.
- Failed instances accumulate until removed; that is the price of being
  able to debug them.

Source: docs/instance-phase.md; docs/verbs-as-requests.md, "Decisions".
