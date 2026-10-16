# 0028: Inbox threads belong to their owner instance; owning threads is opt-in

Status: accepted

## Context

Threads carry the user's answers. If any instance could read or answer
another's threads, a compromised sandbox could read what the user told a
different one. Threads also need to outlive restarts and rebuilds of their
owner, and to stay findable after it's removed.

## Decision

- A thread's identity is `(owner instance_id, key)` (0006). Only the owner
  sees, changes or receives events for its threads; there are no listener
  grants. Host actions on a thread target only its `child` (among the
  owner's own children) or the owner.
- Owning threads is its own capability: the sandbox declares `inbox = true`.
  `dispatcher` grants child ops only; a dispatcher wanting threads declares
  both. Every message and every `events`/`thread-ls` request is checked
  against the config as it is now; others get an `error` record ("thread
  put denied: …").
- Because ids are never reused, `devsandbox rm` of the owner archives its
  threads (read-only, shown only in the All view, dropped by retention)
  instead of deleting them; a put from a live owner un-archives.
- A per-owner cap bounds how many threads one owner can hold.

## Consequences

- Two dispatchers can't see or trip over each other.
- Removing an instance doesn't erase the user's history with it.

Source: docs/inbox-redesign.md, "Model" (Ownership); docs/inbox-threads.md, "Decisions"; docs/automations.md, "Authorization".
