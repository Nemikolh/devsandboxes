# 0027: The Inbox is a generic channel; owners hold the meaning and re-assert idempotently

Status: accepted

## Context

The first Inbox users (a PR babysitter, triage agents) wanted open items
with a status, actions and replies. Teaching devsandbox about PRs or emails
would make every new automation a devsandbox change; and clients built on
top (TUI, CLI, npm GUIs) must not diverge in what they can do.

## Decision

- devsandbox stores threads, shows them, runs built-in instance actions,
  and queues everything else back to the owning instance as events. It
  knows nothing about the domain; the owner decides which threads exist
  and what they say.
- Structured input is data: questions are forms in a message; answers are
  submission events. Rendering is each client's business.
- Every client is equal in what reaches the owner: the TUI, `devsandbox
  inbox` and API clients send the same `inbox::Op`s, checked by
  `inbox::wire` `decide_*` (served by `serve::api`, run locally by the CLI
  fallback) and applied by `Inbox::apply`, so they produce identical
  events. Only the dashboard runs an action's host verb and sets a
  thread child's done flag; the API refuses host-only actions.
- The owner's state is the source of truth: a `thread put` that changes
  nothing is a no-op that doesn't even rewrite the file
  (`Inbox::put` → `PutOutcome::Unchanged`), so an owner re-asserts every
  thread on every pass, and a lost Inbox can be rebuilt.

## Consequences

- New automations need no devsandbox changes, only threads, messages,
  forms and events.
- Owners must keep their own state and tolerate re-delivered events (ack
  by id).

Source: docs/inbox-redesign.md, "Principles"; docs/inbox-threads.md, "The split".
