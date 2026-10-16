# 0005: `run` always creates; `start` restarts

Status: accepted

## Context

`devcontainer up` reuses an existing container when one matches. For
devsandbox that would mean `run` silently reattaching to a stopped,
possibly drifted container, with names that depend on what happens to
exist.

## Decision

`run` always creates a new instance with a new name and refuses a name
that already exists (`run::claim_name`). Restarting a stopped instance is
`start`'s job, which warns when the container has drifted from the config
(see 0009); recreating it is `rebuild`.

## Consequences

- Names are deterministic and a `run` never inherits surprise state.
- Scripts that want "up if needed" use `start` (or a dispatcher's
  idempotent `ensure`, see 0030), not `run`.
- Diverges from devcontainer CLI semantics on purpose.

Source: docs/run-command.md §1 (the original reuse design, since reversed); code comment on `commands::run::run`.
