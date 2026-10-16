# 0026: The Inbox is one shared, locked file store; records are applied, then acked

Status: accepted

## Context

Several dashboards, the CLI and the daemon all show and change the Inbox.
When each dashboard kept its own in-memory copy and saved it on change,
the last writer won: an older dashboard re-saved records another had
dismissed ("restarts add clutter"). And a host that acked a notify record
before storing it lost the record if it died in between.

## Decision

- One host-wide store, `inbox.json` next to `state.toml` (`inbox::store`).
  Every change is a read-modify-write under an exclusive lock on the
  sibling `inbox.lock`, written to a temp file and renamed; readers take a
  shared lock (best-effort; the rename already makes unlocked reads safe).
  Callers go through `inbox::ops`, which names every read and mutation.
- Delivery is at-least-once: `devsbd notify`/`thread` queue a record as a
  file in the container's durable outbox; the host writes it to the store
  *before* replying `ok` (`notify::REPLY_OK`), and the helper deletes the
  file only after the reply. A store failure replies nothing; the helper
  retries, in order, stopping at the first failure.
- The outbox coalesces thread puts/removes for the same key (puts carry
  full state) and sends/withdraws of the same `(thread, id)`; plain notify
  records are a log and never coalesce.

## Consequences

- Any number of dashboards and CLIs stay consistent; none needs the daemon
  to read (0001).
- Duplicates are possible on retry; thread puts are idempotent (0027).
- A v2 `inbox.toml` is imported once on first write and left in place.

Source: docs/inbox-threads.md, "Decisions" (One shared store; Apply, then ack; The outbox coalesces puts); docs/automations.md, "devsbd notify".
