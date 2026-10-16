# Inbox views, wire and clients

The user's side of the Inbox: which threads a view shows, the wire views
clients get, how a thread is addressed, the checks every user op passes,
and which client serves what. Read before adding an Inbox client or API
method, or changing a `decide_*` check. Method and JSON details:
[api.md § Inbox threads](../../reference/api.md#inbox-threads); CLI:
[inbox-cli.md](../../reference/inbox-cli.md).

Code: `src/inbox/view.rs` (`View`), `src/inbox/wire.rs` (`ThreadSummary`,
`ThreadDetail`, `FeedItemView`, `NoteView`, `summaries`, `Addr`,
`resolve`, `user_target`, `decide_*`, `ApiError`), `src/inbox/clock.rs`,
`src/serve/api.rs` (`call`, `mutate`, `dismiss`, `notify_mark_read`,
`audit_client`), `src/serve/daemon.rs` (`watch`, `inbox.changed`,
`inbox.shown`), `src/commands/inbox.rs`, `src/tui/daemon.rs` (`op_call`,
`DaemonWorker::apply`).

## Views

`View` is shared by the dashboard's view strip and `inbox.threads.list`
(and `devsandbox inbox ls`), so they never disagree:

| view | shows |
|---|---|
| `needs-you` | `Thread::needs_you`: live owner threads in state `needs-you`, unread live notify threads. What badges count |
| `active` | live owner threads in `active` |
| `done` | live owner threads in `done` |
| `all` | everything, archived and read notify threads included |

`View`'s own default is `needs-you`; the API and the CLI default to `all`.
`summaries` sorts by `changed_at`, newest first, stable on ties.

## Wire views

The wire is explicit structs, never the store model: the store's JSON is
internal and carries what the wire must not (the client behind each user
item). `ThreadSummary` carries both addresses (`id`, and `owner` /
`owner_name` + `key`), `kind`, `state`, `title` (a notify thread's newest
message, first line), `level`, `unread`, `archived`, `needs_you`,
`changed_at`. `ThreadDetail` adds `link`, `child`, `compose`, `actions`
(each with `host` and `sends_event`), the feed oldest first
(`FeedItemView`, forms with state/draft/answers), a notify thread's `notes`
newest first, and `events_pending`.

## Addressing

`Addr` is `{thread: id}` or `{owner, key}` (both together, or neither, is
`invalid`). `resolve` matches `owner` as an instance id first
(`ops::thread_id`), then as `owner_name` among owner threads (a removed
instance's archived threads keep their name; a live thread wins, then the
most recently changed). `user_target` (every user op) additionally refuses
a notify thread (`invalid`: no owner to answer it) and an archived one
(`denied`: its owner is gone).

## Checks: `decide_*`

The user ops' checks, platform-neutral so the daemon and the CLI's local
fallback share them. Each runs under the store lock inside
`ops::apply_if` and returns the `Op`s to apply or an `ApiError`
(`not-found`, `invalid`, `denied`, `closed-form`), because
`Inbox::apply` itself treats stale input as a silent no-op.

| fn | method | refuses |
|---|---|---|
| `decide_done` | `inbox.thread.done` | `user_target` |
| `decide_reopen` | `inbox.thread.reopen` | `user_target` |
| `decide_act` | `inbox.thread.act` | unknown action `not-found`; host-only action `invalid` (the API never runs host verbs) |
| `decide_reply` | `inbox.thread.reply` | blank `invalid`; no `compose` `denied` |
| `decide_save_draft` | `inbox.form.saveDraft` | see [forms](forms.md#answers) |
| `decide_submit` | `inbox.form.submit` | see [forms](forms.md#answers) |

`decide_done` on a done thread passes, but `apply` then adds nothing (no
event). The API's bookkeeping methods (`inbox.thread.markRead`,
`inbox.notify.dismiss`, `inbox.notify.markRead`, each with `all`) check in
`src/serve/api.rs`: dismiss and notify markRead refuse owner threads.

## Clients

- **Daemon** (`serve::api`, unix): serves every Inbox method over
  `ops`. A connection's hello `client` becomes the audit tag
  (`audit_client`: `tui`, `cli`, else `api:<name>`). The `serve-watch`
  thread flags `inbox.changed` (one pending per connection, coalesced) on
  its own writes at once and others' via the store stamp; the host's sink
  posts `inbox.shown` (a container message's status line, latest only).
- **CLI** (`devsandbox inbox`): reads the store directly; mutations go to a
  running daemon (never lazy-started), else `apply_if` locally with tag
  `cli`.
- **Dashboard**: reads stay local (the store is the truth; `inbox.changed`
  only triggers a reload). While connected, its `Op`s go out as API calls
  (`op_call`); otherwise it applies them through `ops::apply` with tag
  `tui`. A call that broke the connection is retried locally only if it
  enqueues no event; an event-enqueuing one is reported, never sent twice.

All three produce identical events because every path ends in
`Inbox::apply`.

## Time display

`clock.rs` (`short_age`, `stamp`, `local_utc_offset`) formats Inbox times
for both the dashboard and the CLI; it lives outside `src/tui/` because the
dashboard is unix-only and the CLI isn't.

Limitations: [status.md](../../status.md#inbox)
