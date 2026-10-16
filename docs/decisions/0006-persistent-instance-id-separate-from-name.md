# 0006: A persistent `instance_id` is separate from the display name

Status: accepted

## Context

Users rename instances. If the name were the identity, a rename would have
to move the container, its mounts, its worktree, its shell history and its
Inbox threads, or orphan them.

## Decision

Each instance gets an `instance_id` at creation (`run::unique_instance_id`)
that never changes and is never reused. Containers, mounts, worktrees,
per-instance state, Inbox thread ownership (0028) and dispatcher ownership
(`devsandbox.dispatcher=<instance_id>` labels) key on the id. The name is a
display handle: `instances.rename` / `rename_exact` change only the name.

## Consequences

- Rename is cheap and safe; nothing on disk or in the runtime moves.
- Because ids are never reused, a removed instance's Inbox threads can be
  archived rather than deleted (0028).
- Resolution (`commands::resolve_instance`) accepts either name or id.

Source: `state::Instance` docs; docs/vscode-workspaces-and-persistent-state.md.
