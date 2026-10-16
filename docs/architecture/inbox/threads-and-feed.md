# Owner threads and the feed

The owner's side of an Inbox thread: what an instance sends in (`thread
put|send|withdraw|rm`), how the host checks it, how the feed grows and
shrinks, and how the owner reads the user's events back. Read before
changing the put/send schemas, `feed.rs`, or the event control ops.

Code: `src/inbox/thread.rs` (`ThreadPut`, `State`, `Action`, `HostVerb`,
`Compose`, `parse`), `src/inbox/message.rs` (`MessageSend`, `parse`),
`src/inbox/feed.rs` (`FeedItem`, `ItemKind`, `Block`, `Marker`, `send`,
`Sent`, `withdraw`, `push`, `cap`), `src/inbox/mod.rs` (`decide`,
`Inbox::put`, `Inbox::send`, `Event`), `src/commands/dispatch.rs`
(`declares_inbox`, `is_inbox_op`, `events`, `events_body`, `event_json`,
`thread_ls`), `src/commands/dispatch/follow.rs` (`Followers`,
`follow_with`), `src/devsbd/control.rs` (`valid_key`, `valid_message_id`,
`MAX_WAIT`, `FOLLOW_PING`).

## Ownership

Owning threads needs `inbox = true` on the sandbox, checked against the
config as it is now (`declares_inbox`, `is_inbox_owner`); a dispatcher's
child may own threads too. Without it, thread records become an `error`
record in the sender's own Inbox and the event ops answer `denied`. Plain
`devsbd notify` needs nothing. The helper only syntax-checks JSON and
extracts the key/id ([in-container](../helper/in-container.md#outbox-devsbd-notify-devsbd-thread-putsendwithdrawrm));
every schema check runs on the host, and any rejection is one line in an
`error` record keyed `thread:<key>`, the author's only feedback
([model](model.md#owner-writes-the-sink)).

## `thread put`

The thread's whole header, re-asserted every pass (an unchanged put is a
no-op). `deny_unknown_fields`; removed fields (`message`, `reply`) get a
pointed error.

| field | rule |
|---|---|
| `key` | `valid_key`: `[a-z0-9][a-z0-9-]*`, ≤ 40 (`MAX_KEY`) |
| `title` | required, non-blank, ≤ 200 bytes |
| `state` | required: `needs-you` \| `active` \| `done` |
| `status` | ≤ 60 bytes |
| `link` | http(s) URL only (`is_url`), ≤ 2000 bytes |
| `child` | a key (`valid_key`): the dispatcher child the thread is about |
| `actions` | ≤ 9 (`MAX_ACTIONS`), unique ids `[a-z0-9-]{1,40}`, label non-blank ≤ 60 |
| `compose` | present = takes replies; `placeholder`, `hint` ≤ 100 each |

Free text rejects control characters other than `\n`, `\t`, `\r`.

An `Action` is `id`, `label`, `notify`, `done`, optional `host`. `host` is
one verb from a fixed table (`HostVerb`; never an arbitrary command):
`vscode {path?, line?, col?}` (path relative, no `..`, no leading `/`/`~`,
≤ 400), `terminal {}`, `logs {}`, `forward {port}` (1-65535), `open {url}`
(http(s)), `rm {}`. Host verbs run in the dashboard. Pressing an action
enqueues an event (`enqueues_event`) when it has no `host`, or `notify`, or
`done`; `done: true` also sets the thread done (one `done` event carrying
the action id).

## `thread send`

One owner message, `{thread, id, blocks}`, `deny_unknown_fields` down to
block types, so a newer owner on an older host fails loudly instead of
rendering half a message. Body ≤ 48 KiB (`MAX_SEND_BODY`); `thread` a
key; `id` `[a-z0-9-]{1,60}` (`valid_message_id`); 1-8 blocks
(`MAX_BLOCKS`):

- `markdown {text}`: non-empty, ≤ 16 KiB;
- `fields {items: [{label, value}]}`: 1-20 rows, label non-empty ≤ 60,
  value ≤ 200, single line;
- `form`: at most one per message ([forms.md](forms.md)).

The thread must exist (put first), else an `error` record. `id` is the
idempotency key (`feed::send` → `Sent`): a new id appends (`Inserted`,
unread + moves to top); the same content is `Unchanged`; other content
replaces it in place, keeping its position, tagged `edited` (`Edited`:
quiet, unless it opened a form). A withdrawn message sent again comes back
`edited`.

## `thread withdraw` / `thread rm`

`withdraw <thread> <id>` tags the message `withdrawn` (kept; an open form
on it is withdrawn too); unknown ids are a silent no-op. `rm <key>` drops
the owner's thread outright.

## Feed

`FeedItem {seq, at, kind}` in first-insert order. `ItemKind`: `message`
(the owner's: `id`, `blocks`, `edited`, `withdrawn`, `form` record),
`reply`, `action`, `submission` (the user's, each with the `client` that
made it: stored for the user's audit, never on events or the API), and
`marker`: `done`/`reopen` (the user's) or `state`/`status` (an owner put
changing those).

- `push`: a status marker right after another merges into it (earlier
  `from`, new `to` and `at`); a merge back to the start removes it. State
  markers never merge.
- `cap` to `MAX_FEED` (300): oldest markers go first, then oldest
  messages, then oldest user items. A message with an open form is never
  dropped; past those the cap goes soft.

## Events

Each user op on an owner thread enqueues one `Event`
([model](model.md#events)): `action`, `reply`, `done`, `reopen`, `submit`.
The owner pulls them over control ops on its bridge, all scoped to the
requester's own `instance_id`:

- `events` (`devsbd events [--wait N] [--thread K]`): pending events as
  JSON lines, oldest first: `{id, thread, kind, action?, text?, message?,
  form?, answers?, at}` (`at` RFC 3339). All pending ones every call
  (at-least-once) until acked; a body is cut near half of `MAX_RESPONSE`
  and the rest comes after an ack. With a timeout (≤ `MAX_WAIT`, 300 s)
  and nothing pending, waits on the handler thread, waking on this
  process's store writes and re-checking the stamp every 500 ms
  (`EVENTS_POLL`).
- `events-ack`: drop events by id; answers the count. Foreign or unknown
  ids are skipped.
- `events-follow` (`devsbd events --follow`): one held-open stream: every
  pending event, then each new one once per stream; a ping line every 30 s
  (`FOLLOW_PING`) without traffic. One live follower per owner
  (`Followers`): a newer one makes the older write `{"kind":"replaced"}`
  and end. Acks still go through `events-ack`.
- `thread-ls` (`devsbd thread ls [--feed]`): the owner's live threads as
  put bodies (`Thread::to_put`), with `--feed` each owner message (`id`,
  `at`, `blocks`, `edited`, `withdrawn`, `form {id, state, answers?}`):
  what a dispatcher that lost its state reads back.

Codec: [protocol](../helper/protocol.md); bridge handling:
[bridges](../helper/bridges.md).

Limitations: [status.md](../../status.md#inbox)
