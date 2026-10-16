# 0030: Dispatcher control is a host API, authorized against the current config on every request

Status: accepted

## Context

A dispatcher is a sandbox that spawns and manages other sandboxes
(children). That gives code inside a container power over the host. The
grant must come from the user's config, survive edits to it, and never let
a container escalate (spawn arbitrary sandboxes, recurse, reach others'
children, read host paths).

## Decision

- Only sandboxes declaring `dispatcher` get control ops, over the helper's
  `CONTROL` stream to the host (`commands::dispatch`). Every request is
  checked against the config as it is now: `ensure` only for sandboxes in
  `spawn`, `max-instances` enforced, stop/rm/done/exec only on owned
  children, children can't be dispatchers, denied `--env` names refused.
- Children are named `<sandbox>-<key>`, labelled with the dispatcher's
  `instance_id`, and kept when the dispatcher stops or is removed (cleanup
  is the dispatcher's job; orphans are removable by hand).
- Ops answer JSON; exit codes are fixed (0, 1, 2 usage, 75 no host, 77
  denied). With no host connected, control fails fast with 75; there is no
  durable command queue (notify, by contrast, is queued).
- Git questions (`devsbd branches`) are answered by the host through
  `host_git` (0003): a worktree's `.git` points at a host path the
  container can't see, and host paths never reach the response.

## Consequences

- Editing the config revokes or narrows a dispatcher's power immediately.
- Dispatcher scripts must retry on 75 and reconcile children via `devsbd ls`.

Source: docs/automations.md, "Dispatchers", "No host connected"; docs/dispatcher-branch-holders.md, "Why not have the dispatcher read git itself".
