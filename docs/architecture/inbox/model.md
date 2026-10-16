# Inbox model and store

The Inbox's persisted half: the two kinds of thread, what each mutation
does, caps and retention, pending events, and the host-wide `inbox.json`
every client shares. Read before changing `src/inbox/mod.rs`, `store.rs` or
`ops.rs`, or adding a client that reads or writes the Inbox.

Code: `src/inbox/mod.rs` (`Inbox`, `Thread`, `Kind`, `Note`, `Event`,
`EventKind`, `Op`, `push`, `put`, `send`, `withdraw`, `thread_rm`,
`apply`, `user_op`, `apply_sink`, `decide`, `archive_owner`, `prune`,
`enforce_cap`, `events_for`, `ack`, `threads_for`, `to_json`, `from_json`,
`import_v2`), `src/inbox/store.rs` (`path`, `load_at`, `update_at`,
`stamp_at`, `generation`, `wait_changed`), `src/inbox/ops.rs`,
`src/inbox/sanitize.rs`, `src/commands/rm.rs` (`archive_inbox`).

## Threads

Every `Thread` has a store `id` (persisted, from a counter that never goes
back, so ids survive reloads and are never reused), an `owner` (the sending
instance's `instance_id`, stable across stop/start/rebuild/rename) and an
`owner_name` (the instance name at the last write: display only). `id`s,
note ids, feed `seq`s and event `seq`s share one counter, so arrival order
is total across the store. Two kinds share the list:

- `Kind::Notify`: `devsbd notify` records (`Record`: `level`
  info/warn/error, optional `key`, `link`, `msg`, `at`). A keyed record
  joins the notify thread with the same `(owner, key)` as its new head
  (newest first, `MAX_NOTES` kept), moves it to the top and marks it
  unread; an unkeyed record is a thread of its own. Keys are per owner:
  two sandboxes can't collide.
- `Kind::Thread`: an owner thread from `devsbd thread put`, identified by
  `(owner, key)` (`ops::thread_id`). A header the owner re-asserts with each
  put (`title`, `link`, `state`, `status`, `child`, `actions`, `compose`),
  a `feed` (messages, the user's replies/actions/submissions, markers;
  [threads-and-feed.md](threads-and-feed.md)) and pending `events`. A
  notify record never joins an owner thread with the same key.

`state` is `needs-you` | `active` (default) | `done`. `needs_you()` is a live
owner thread in `needs-you`, or an unread notify thread: what views and
badges count ([views-and-wire.md](views-and-wire.md)). `unread` is the
"news" flag; `changed_at()` is the later of the last put and the head
record.

## Owner writes (the sink)

The bridge's sink decodes each container record and calls `decide`
(pure): a notify record → `SinkAction::Push`; a thread record from an
instance whose sandbox lacks `inbox = true`, or whose body fails the
schema (or names another key/id than the one it was queued under), → a
`Push` of an `error` record keyed `thread:<key>` carrying the reason, so
the author sees it. `ops::sink` then runs `apply_sink` under the store
lock: `prune` first (so a store nobody else touches still ages out), then
`sanitize` every container string, then:

- `Put` → `Inbox::put`: creates the thread (unread) or replaces the header
  wholesale. A state or status change adds a `Marker` to the feed and
  marks unread; a cosmetic change (title, label, rename) doesn't. A put
  that changes nothing is `PutOutcome::Unchanged`: no field touched, so no
  write, since owners re-assert every thread every pass. A put
  un-archives.
- `Send` → `Inbox::send` (no such thread → an `error` record "put it
  first"); `Withdraw`; `Rm` (`thread_rm` drops the owner thread).

It returns `Shown`: the status line, and a desktop popup only for a notify
record, a put entering `needs-you`, or a new message (or a form) on a
`needs-you` thread. Transport: [bridges](../helper/bridges.md).

## User ops

`Op` names targets by store id, never row index. Bookkeeping ops:
`MarkRead`, `MarkNotifyRead` (owner threads are only read by opening
them), `RemoveThread`, `ClearNotify` (owner threads can't be dismissed:
they'd come back on the next put). User ops (`Op::enqueues_event`): `Act`,
`Reply`, `MarkDone`, `Reopen`, `Submit` add feed items tagged with the
client (`tui`, `cli`, `api:<name>`) and enqueue one `Event`; `SaveDraft`
only updates a form's draft ([forms.md](forms.md)). `apply` is
deliberately forgiving: a notify or archived target, an unknown action, a
host-only action, an empty reply, a done on a done thread or a reopen on a
live one is a silent no-op. Clients that must report errors go through
`wire::decide_*` first ([views-and-wire.md](views-and-wire.md)).

`MarkDone`/`Reopen` set `state` to `done`/`active` (the owner's next put
may change it again). Neither touches the child instance's `done` flag in
`state.toml`; only the dashboard flips that alongside the op
([dispatchers](../automations/dispatchers.md)).

## Events

An `Event` is delivery state, not history: `id` (`e-<unix:010>-<4 hex>`,
unique in the store, minted under the lock), `seq`, `kind` (`action`,
`reply`, `done`, `reopen`, `submit`), payload (`action`, `text`, or
`message`/`form`/`answers`), `at`. It never says which client made it.
Owners read them (`events_for`, oldest first, at-least-once until acked)
and ack by id (`ack`: unknown or foreign ids are skipped). Owner side:
[threads-and-feed.md](threads-and-feed.md#events).

## Caps and retention

| bound | value | what goes |
|---|---|---|
| `THREADS_PER_OWNER` | 200 | per owner (`enforce_cap`, never the thread just written): archived first, then `done` without events, then least recently changed without events. Threads with unacked events stay (the cap goes soft), unless archived |
| `MAX_NOTES` | 300 (`MAX_FEED`) | oldest notes of a notify thread |
| `MAX_FEED` | 300 | see [threads-and-feed.md](threads-and-feed.md#feed) |
| `MAX_EVENTS` | 100 | oldest events of a thread |
| `MAX_REPLY` | 2000 chars | the rest of a reply |
| `RETENTION` | 14 days | `prune`: archived threads, and `done` ones without events, unchanged that long. Live threads never age out |

## Archive

`devsandbox rm` calls `ops::archive`: every thread of the instance's
`instance_id` becomes `archived` (read-only, hidden outside the `all` view,
refused by user ops), then `prune` runs. Instance ids are never reused, so
archived threads are history until retention drops them.

## Sanitize

`sanitize` drops C0 (except `\n`), DEL, C1 and bidi embedding/override/
isolate characters from container text (untrusted terminal output),
normalizes `\r\n`/`\r` to `\n` and expands tabs to 4-column stops. Applied in `apply_sink` before
storing, and again by the dashboard's renderer for older stored text.

## Store

`inbox.json` beside `state.toml` (`store::path`), one per host, shared by
every dashboard, the daemon and the CLI.

- Writes: `update_at` takes an exclusive lock on the sibling `inbox.lock`,
  reads, applies, and writes only if the serialized content changed
  (temp `.inbox.json.<pid>.tmp` + rename), so no-op updates don't move
  the mtime pollers watch.
- Reads: `load_at` takes a shared lock, best-effort (the atomic rename
  already makes unlocked reads safe). A missing file is an empty Inbox.
- Change detection: `stamp_at` (mtime + length) for other processes;
  `generation`/`wait_changed`, an in-process counter bumped by every write
  this process makes, wake long-polls like `events --wait` at once. Other
  processes' writes don't bump it.
- Version: `VERSION` 3, JSON (`SavedInbox`). Any other version is an
  error: a store written by a newer build isn't this one's to rewrite.
- Import: with no `inbox.json` but an `inbox.toml` (v1/v2) beside it,
  `import_v2` carries over **notify threads only** (owner threads are
  re-put by their owners). v1 names map to instance ids through state; an
  unresolved one keeps its thread archived under owner `name:<instance>`.
  The first write saves `inbox.json`; `inbox.toml` is left in place.

## `ops`

Every read and mutation goes through `src/inbox/ops.rs` with an explicit
store path (tests and the daemon pass their own): `load`, `stamp`,
`apply`, `apply_if` (a check and its ops in one locked write), `sink`,
`archive`, `events`, `ack`, `threads`, `thread_id`. Times are minted inside
the lock, so stamps follow write order. One code path per verb is why
every client produces identical events.

Limitations: [status.md](../../status.md#inbox)
