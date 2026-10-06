# Inbox redesign: conversations, forms, any client

Status: implemented through step 20 (2026-10-06); 13b (remove the v2
put/event compat) pending, timed with the dispatcher's migration. Supersedes
the thread model of
`inbox-threads.md` (single overwritten `message` + field-change timeline,
free-text reply only, TUI-only events, dashboard-only control path, threads
reserved to `dispatcher` instances). The locked store, done instances, host
actions, `vscode-goto` and markdown rendering from that doc stay. Companion
plan for the PR babysitter: `../.devsandboxes/dispatcher-inbox-redesign.md`.

Everything below was agreed on 2026-10-06 unless listed under *Open
questions*.

## Why

First real use of threads (2026-10-02 → 10-06) with the PR babysitter:

- **The pane reads backwards.** The `message` (newest thing) sits at the top,
  then the timeline runs oldest → newest, so the eye jumps top → bottom →
  top. Timeline entries are flattened to one line (`one_line()`,
  `src/tui/app/inbox.rs:279`) and a message change is stored as a copy of the
  whole message, so the history is a wall of truncated duplicates.
- **The only structured input is a button or one free-text line.** Anything
  richer (per-item decisions, editing a draft) was pushed into files in the
  child (`.dispatcher/pr-N-replies.md` + an "Open draft" host action). That
  breaks when the child is stopped, leaves untracked files that make
  `devsandbox rm` refuse eviction, and splits the conversation between the
  Inbox and an editor.
- **The reply box clips.** `draw_reply_input` (`src/tui/ui.rs:1204-1227`)
  renders the input as one `Line` in a fixed box with no wrap and no
  horizontal scroll; the cursor is clamped at the right edge
  (`ui.rs:1223-1224`). Text past the box width is invisible, though it's
  saved and sent.
- **List and pane disagree.** The card chip drops `status` unless the thread
  is `active` (`ui.rs:770-787`); the pane always shows it
  (`inbox.rs:311-320`).
- **Only the TUI can answer.** Events exist only because someone pressed a
  key in a dashboard (`dispatch.rs`, no host CLI path). No `--json` surface
  exposes threads (`docs/json-output.md`), the npm package has none, so no
  other UI can be built.
- **Everything needs an open dashboard.** Every control op (`ensure`, `exec`,
  `events`, …) exits 75 without a bridge, and only the dashboard spawns
  bridges. A dispatcher is dead whenever the TUI is closed.
- **Pull only.** `events --wait` long-polls the store mtime every 500 ms
  (`dispatch.rs:404-430`) inside one control request capped at 300 s. It
  works, but a dispatcher has to structure its whole loop around it.

## Principles

1. **devsandbox is a channel, not an app.** It carries three things between
   an instance and whoever answers: *direct ops* on a dispatcher's children,
   *structured content* in threads, and *events* back. It knows nothing about
   PRs, drafts or emails.
2. **A thread is a conversation.** A small, re-assertable *header* (what it
   is, its state, what you can do) plus an append-only *feed*: *messages*
   from the owner, and the user's *replies*, *actions* and *submissions*.
   Nothing in the feed is a diff of the header.
3. **Structured input is data, not files.** Questions are a form in a
   message; answers are a submission event. Rendering is the client's business.
4. **Every client is equal.** The TUI, the CLI and a custom GUI all talk to
   the same API served by the host daemon (`devsandbox serve`) and produce
   identical events. The owner can't tell, and must not care, where an event
   came from.
5. **The owner's state stays the source of truth.** Every put and send is
   idempotent by id, so an owner re-asserts freely and losing the Inbox loses
   nothing it can't rebuild.
6. **Files stay the source of truth on the host.** The daemon holds only
   what can't live in a file (connections, forwards, subscribers). Killing it
   loses nothing; read-only commands work without it.

## Model

### Ownership

- **Owning threads is its own capability:** a sandbox opts in with
  `inbox = true`. Its instances may `thread put|send|withdraw|rm|ls` and pull or
  follow events for their own threads. Others get the existing `error`
  record ("thread put denied").
- **`dispatcher` only grants direct ops on children** (`ensure`, `exec`,
  `rm`, …). A dispatcher that wants threads declares both. No implicit
  "dispatcher implies inbox".
- **Only the owner listens.** Events for a thread go to its owner and nobody
  else: no listener grants, no subscribing to another instance's threads
  (it would leak the user's answers across sandboxes).
- Identity stays `(owner instance_id, key)`. Two dispatchers in separate
  sandboxes are separate owners and can't see or touch each other's threads,
  children or events, so they can't trip over one another.

### Header (`thread put`)

| field | change |
|---|---|
| `key`, `title`, `link`, `child` | unchanged |
| `state` | unchanged: `needs-you` / `active` / `done` |
| `status` | unchanged (≤60 B), but now always shown next to the state, in the card and the pane alike |
| `actions` | unchanged (host verbs, `done`, `notify`) |
| `message` | **removed.** Explanations are messages in the feed (`thread send`) |
| `reply` | **renamed `compose`**: `{ "placeholder": "…", "hint": "…" }`. `hint` is one dim line under the box saying what sending does *now* ("Starts a comments run with your message"); the owner re-puts it when that changes |

A put only touches the header. A put that changes nothing is still a no-op.
State and status transitions are recorded as compact *markers* in the feed
(below), not as entries per field.

### Terminology

One name per direction, used by the CLI, the API, events and the TUI alike:

| direction | item | made by |
|---|---|---|
| owner → user | **message**: blocks (markdown, fields, forms) | `devsbd thread send` |
| user → owner | **reply**: free text from the composer | `inbox.thread.reply` |
| user → owner | **action**: a header button | `inbox.thread.act` |
| user → owner | **submission**: a form's answers | `inbox.form.submit` |
| either | **marker**: done/reopen, state/status change | host-recorded |

The header (`thread put`) is not a feed item. Earlier drafts called messages
"posts"; that name is gone everywhere.

### Messages (`thread send`)

```json
{
  "thread": "pr-6900",
  "id": "run-1791277117",
  "blocks": [
    { "type": "markdown", "text": "Addressed 3 of 4 comments, 1 needs your call." },
    { "type": "form", "id": "drafts", "submit": "Post replies", "questions": [ … ] }
  ]
}
```

- `id` (`[a-z0-9-]{1,60}`, unique per thread) is the owner's idempotency key.
  Sending the same id with the same content is a no-op; with different
  content it **replaces the message in place** (shown as "edited", no new
  unread/popup unless a form was added). Owners use stable ids derived from
  their own state (a run id), so a restarted dispatcher re-sends for free.
- `thread withdraw <thread> <id>` withdraws a message (rendered as a dim
  "withdrawn" line, kept in the feed so the story stays readable).
- Host-stamped `at` on first insert; order in the feed is first-insert order.
- Blocks, v1: `markdown` (≤16 KiB, rendered with `src/tui/markdown.rs`),
  `form` (below), `fields` (a compact key/value list: `{ "type": "fields",
  "items": [{ "label": "Head", "value": "36b13d41" }] }`, ≤20 items). Unknown
  block types are a schema reject, so a newer owner on an older host fails
  loudly instead of rendering half a message.
- A message is ≤48 KiB serialized (it must fit the 64 KiB outbox record,
  `src/devsbd/notify.rs:42`). At most 8 blocks per message.

### Forms

A form is a block in a message: a list of questions the user answers and
submits once (the *submission*). Labels in the example below are the PR
babysitter's own wording (posting replies on GitHub), not devsandbox terms.

```json
{
  "type": "form",
  "id": "drafts",
  "title": "Replies to post",
  "submit": "Post replies",
  "questions": [
    {
      "id": "c-3726888733",
      "label": "greptile on `src/cost.ts:42`",
      "context": "> Consider batching these writes.\n\nNot changed: writes are already batched in `flush()`.",
      "type": "choice",
      "options": [
        { "id": "post", "label": "Post the reply" },
        { "id": "skip", "label": "Don't reply" }
      ],
      "default": "post"
    },
    {
      "id": "c-3726888733-text",
      "label": "Reply",
      "type": "text",
      "multiline": true,
      "default": "Already batched in `flush()` (src/cost.ts:88), so this would double-buffer.",
      "placeholder": "Reply to post on the comment"
    },
    {
      "id": "notes",
      "label": "Anything else for the agent?",
      "type": "text",
      "multiline": true,
      "required": false,
      "placeholder": "Leave empty to just post"
    }
  ]
}
```

- Question types, v1:
  - `choice`: `options` (1-20, `id` + `label` + optional `description`),
    `multiple` (default false), `default` (an id, or ids when `multiple`).
  - `text`: `placeholder`, `default` (prefilled, editable: this is how a draft
    is offered for editing), `multiline`, `max` (≤8 KiB, default 2 KiB).
  - `confirm`: yes/no with optional `yes`/`no` labels.
- Common: `id` (`[a-z0-9-]{1,60}`, unique in the form), `label` (inline
  markdown, ≤200 B), `context` (markdown shown above the input, ≤4 KiB),
  `required` (default true for `choice`/`confirm`, false for `text`).
- ≤30 questions per form, one form per message. No conditional questions in
  v1: owners split into two messages instead.
- **Lifecycle, kept by the host:** `open` → `submitted` (the submission is
  stored with the message, the form renders read-only with the answers) or
  `withdrawn` (the owner replaced or withdrew the message). A submitted form
  can't be resubmitted; the owner sends a new one if it needs another round.
- **Drafts of answers** (half-filled forms) are stored with the message in the
  host store, not in the TUI, so they survive a dashboard restart and any
  client sees the same partial state.
- Submitting produces one event (`kind: "submit"`, below) with all answers,
  defaults included, so the owner never has to merge defaults itself.
- A form-bearing message entering the feed sets the thread unread and fires the
  needs-you popup if the header is `needs-you`.

### Feed

What the pane shows under the header, **newest first**:

| item | author | rendering |
|---|---|---|
| message | owner | full markdown blocks; forms inline (open ones pinned, see layout) |
| reply | user | the text, full, as markdown (no more `one_line`) |
| action | user | `you: Retry` |
| submission | user | folded summary (`you answered 4 questions`), expandable |
| done / reopen marker | user | one dim line |
| state / status marker | owner | one dim line; consecutive status-only changes collapse (`running comments → review drafts`) |
| message withdrawn / edited | owner | dim tag on the message |

Every user item records *which client* produced it in the store (`tui`,
`cli`, `stdio:<name>`) for the user's own audit, but this is **not** exposed
to the owner in events.

### Events

```json
{"id":"e-…","thread":"pr-6900","kind":"submit","message":"run-1791277117","form":"drafts","answers":{"c-3726888733":"post","c-3726888733-text":"Already batched…","notes":""},"at":"…"}
{"id":"e-…","thread":"pr-6900","kind":"reply","text":"Is this PR still relevant?","at":"…"}
{"id":"e-…","thread":"pr-6900","kind":"action","action":"retry","at":"…"}
{"id":"e-…","thread":"pr-6900","kind":"done","action":"done","at":"…"}
{"id":"e-…","thread":"pr-6900","kind":"reopen","at":"…"}
```

- `key` is renamed `thread` (it was ambiguous next to child keys).
- `kind` gains `submit` (a form submission). Answers: choice → id or `[ids]`, text → string,
  confirm → bool.
- At-least-once with explicit ack stays. Ids, scoping to the owner, the
  100-per-thread cap stay.
- **One live follower per owner.** A second `events --follow` from the same
  owner replaces the first (which exits 0 with a `{"kind":"replaced"}` line),
  so two copies of a dispatcher can't both act on one event.

### Store and limits

- `inbox.toml` → schema `version = 3`, **no migration**: v2 threads are
  dropped on first load (they're projections; a dispatcher re-puts every
  pass) except plain notify records, which are carried over as notify
  threads. No compatibility shim for v2 `thread put` bodies: the helper
  version bump (below) makes the host reject old shapes with an error record
  that says to update the dispatcher. (As built: a temporary shim keeps
  v2 puts and the event `key` working until 13b, *Follow-ups*.)
- The per-owner cap of 200 records is too small once messages exist. New caps:
  200 threads per owner, 300 feed items per thread (oldest markers dropped
  first, then oldest messages without an open form), events unchanged. Retention
  unchanged (done/archived dropped after 14 days).
- TOML is a poor fit for nested blocks/forms. Switch the store to JSON
  (`inbox.json`, same lock + tmp/rename discipline), keeping v2 → v3 load of
  notify records only.

## Channel: devsbd

### Direct ops on children (exists; tidy)

`ensure`, `ls`, `branches`, `stop`, `rm`, `done`, `exec`, `run ls|logs|wait|
rm|prune` stay as they are (`src/commands/dispatch.rs:197-358`). Two changes:

- Every op answers JSON (`ls`/`branches` already do; `ensure`, `done`,
  `stop`, `rm` print a one-line JSON result) so a dispatcher never parses
  prose.
- Host actions on a **stopped** child no longer fail silently or half-work
  (today "Open draft" on a stopped child is broken): `vscode`, `terminal`,
  `forward` start the child first (status line `starting <child>…`), and
  the button shows `(stopped)` next to its label. `logs` doesn't: a stopped
  container's logs are how you see why it stopped.

### Threads and messages

```
devsbd thread put      [--json '<json>' | stdin]   # header
devsbd thread send     [--json '<json>' | stdin]   # message (blocks, forms)
devsbd thread withdraw <thread> <message-id>
devsbd thread rm <key>
devsbd thread ls [--feed]                           # headers; --feed adds messages, form states and submissions
```

`put`, `send`, `withdraw`, `rm` go through the durable outbox (with the same
per-key coalescing for puts; sends coalesce per `(thread, id)`). `thread ls --feed`
lets a dispatcher that lost its state recover which forms were already
answered, so answers are never lost even if an event was acked and the
dispatcher crashed before saving (it can't happen with ack-after-save, but
recovery should not depend on that).

The std-only validator (`devsbd/src/json.rs`) keeps doing syntax + top-level
field extraction (`key` for put, `thread` + `id` for send); schema stays
host-side with serde (`src/inbox/thread.rs`, new `message.rs`, `form.rs`).

### Events: listeners

```
devsbd events [--wait SECS] [--thread KEY]         # pull, as today (+ filter)
devsbd events --follow [--thread KEY]              # push: JSON lines until killed
devsbd events ack <id>...
```

`--follow` is a long-lived stream over the bridge, served by the daemon:
the bridge is already a persistent, bidirectional, multiplexed connection
(`docs/sandbox-helper.md`, *Frame protocol*). Add a `Subscribe` control op
whose response stays open: the daemon writes every pending event for the
owner (optionally one thread),
then each new one as it's enqueued (a store-change notification instead of
the 500 ms mtime poll), and a `{"kind":"ping"}` line every 30 s so a dead
connection is noticed. Acks still go through `events ack` (a separate
request), so delivery stays at-least-once and a reconnecting follower gets
everything unacked first. If the bridge drops or no daemon is running,
`--follow` exits 75 and the caller reconnects; owners wrap it in a retry
loop. A running `inbox = true` instance keeps the daemon alive (below), so
that only happens across daemon restarts or before the first start. Frame kind is
additive (unknown kinds are skipped), so no protocol `VERSION` bump for the
stream itself; the thread v3 schema does bump the helper's advertised
`thread` capability so old dispatchers get a clear error.

## Host daemon: `devsandbox serve`

Today the dashboard *is* the host: it spawns bridges (`Bridges::reconcile`,
`src/devsbd/bridge.rs:599`, from `src/tui/mod.rs:261`), so with no TUI open
every control op exits 75 and thread events can't be produced. That's the
root of "the dispatcher stopped responding" whenever the dashboard is
closed. `exec`/`start` meanwhile spawn their own short-lived bridge per
command for the ssh-agent relay (`src/commands/exec.rs:49-51`,
`src/commands/start.rs:105`), so one container can have several bridges and
the daemon picks the newest (`devsbd/src/daemon.rs:120`).

### What it is (and isn't)

A disposable, per-user, per-host process, closer to `ssh-agent` or the
Gradle daemon than to dockerd:

- **Not the state.** `inbox.json`, `state.toml` and the container outboxes
  stay the source of truth with their lock + tmp/rename discipline. The
  daemon holds only connections, forwards, subscribers and the popup
  trigger. Killing it loses nothing; `ps`, `status --json`, `inbox ls` keep
  reading files without it.
- **Not managed by the user.** Started on demand, exits when idle (below),
  optionally installed to start at boot.
- **No UI.** Drawing, keys and interactive terminals stay in the TUI.

### Responsibilities

| | today |
|---|---|
| One bridge per running instance with a helper: spawn, reconcile, reinstall a stale `devsbd` | the TUI |
| Every channel on that bridge: ssh-agent relay, HTTP proxy, notify, control, TCP forwards | TUI bridges, plus per-command bridges in `exec`/`start` |
| Control ops (`ensure`, `exec`, `run …`, `events`, `thread ls`, `Subscribe`) | the TUI, exit 75 without it |
| Draining outboxes into the store (notify, thread put/send/withdraw/rm) | the TUI |
| Change notifications: `--follow` subscribers, API `subscribe` | 500 ms mtime polling per request |
| Port forwards (survive closing the TUI) | the TUI; they die with it |
| Desktop popups (needs-you transitions, notify records) | the TUI |
| The API (below) for every client | none |

**ssh-agent sessions go through the daemon.** `exec` and `start` stop
spawning their own bridge: they ensure the daemon is up (lazy start) and
the instance's bridge exists, then inject `SSH_AUTH_SOCK` as today. One
bridge per container, no duplicates, and an open `exec` session counts as a
client for the idle rule.

### Transport

- **Unix socket**, mode 0600, in a 0700 per-user dir:
  `$XDG_RUNTIME_DIR/devsandbox/serve.sock`, falling back to
  `~/.local/state/devsandbox/serve.sock`. Keep the path short: macOS caps
  socket paths at 104 bytes, so no config-root hashes in it (one daemon per
  user; config roots are a field in requests).
- Framing: JSON lines, the API below.
- **Access control is file permissions.** Whoever can connect can `exec`
  into every container, like `docker.sock`. Say so in the docs.
- **No loopback TCP** (other local users could connect; it would need a
  token for what the socket permissions give for free).
- **Windows: no daemon yet.** Bridges, forwards, popups and the mux are
  already `#[cfg(unix)]` (`src/devsbd.rs:10-33`), so Windows loses nothing it
  has today. Wrap bind/connect/accept in a small `local_endpoint` module so a
  named pipe (`\\.\pipe\devsandbox-<user>`, Docker's choice) can be added
  when bridging is ported; the `interprocess` crate covers both with a sync
  API, decided then.

### Start

1. **Lazy start, always on, every platform.** A command that needs live
   features (TUI, `run`, `start`, `exec`, `port`, `inbox` mutations,
   `api --stdio`) connects; if nothing answers, it spawns `devsandbox serve`
   detached (`setsid`, stdio to `~/.local/state/devsandbox/serve.log`),
   waits for the socket (up to 5 s), and proceeds. Same trigger set as the
   existing `autostart = true` pass (`docs/automations-guide.md:15`), which
   moves into the daemon's startup.
   - **Races:** an exclusive lock file next to the socket; the loser of two
     concurrent starts connects to the winner.
   - **Version handoff:** a client sends its version on connect; when it's
     newer than the daemon's, the daemon drains (stops accepting, finishes
     in-flight requests, closes subscribers with a reconnect hint), exits,
     and the client starts its own. Same spirit as the helper self-heal.
2. **Boot start, opt-in.** `devsandbox serve install|uninstall` writes a
   systemd user unit (Linux) or a LaunchAgent (macOS). Needed only for
   dispatcher/inbox containers restarted by the runtime at boot
   (`autostart = "runtime"`): a container can't start a host process, so
   without it they wait (exit 75, retrying) until the first `devsandbox`
   command. Installing implies keep-alive.

### Idle exit

The daemon exits after 10 minutes with no *holder*; a new holder cancels
the countdown.

| holds it | why |
|---|---|
| a connected client: TUI, `api --stdio`, an `inbox` command, an `exec` session using the ssh-agent relay | in use |
| an active port forward | lives on the bridge mux |
| a running instance declaring `dispatcher` or `inbox = true` | expects a live host: control ops, `--follow`, answers |
| an in-flight control request or a `--follow` subscriber | in use |

Not holders: plain running containers (their outbox is durable and drains at
the next start; only their popups are late) and stopped instances. With a
dispatcher running the daemon never idles, by design.
`serve.keep-alive = true` (global config,
`<data-dir>/devsandbox/daemon.config.toml`, step 10a) disables the idle exit. If plain
`inbox = true` posters holding it turns out wasteful, narrow that row to
"has a follower or an open thread"; start simple.

### The TUI becomes a client

Instance list, inbox and live state come from the API; control-dependent
features go through the daemon. Two dashboards open no longer race over
the newest bridge or the store. Terminals (`docker exec -it` via vt100) stay
in the TUI process; their ssh-agent relay rides the daemon's bridge.

## Clients

### One API, served by the daemon

JSON lines over the socket, request/response plus notifications. Namespaces:

- `inbox.*`: `threads.list` (view filter), `thread.get` (header + feed),
  `thread.markRead`, `thread.act`, `thread.reply`, `form.saveDraft`,
  `form.submit`, `thread.done`, `thread.reopen`, `notify.dismiss`.
- `instances.*`: `list` (the `status --json` snapshot), `done`/`undone`,
  `stop`/`start` (later, as needed).
- `forwards.*`: `list`, `add`, `rm`.
- `subscribe` / `unsubscribe` with topics (`inbox`, `instances`,
  `forwards`) → `{"method":"inbox.changed","params":{…}}` notifications.

```
→ {"id":1,"method":"inbox.threads.list","params":{"view":"needs-you"}}
← {"id":1,"result":[{ "owner":"bab-disp","key":"pr-6900","state":"needs-you", … }]}
→ {"id":2,"method":"subscribe","params":{"topics":["inbox"]}}
← {"id":2,"result":{"ok":true}}
← {"method":"inbox.changed","params":{"owner":"…","key":"pr-6900"}}
→ {"id":3,"method":"inbox.form.submit","params":{"owner":"…","key":"pr-6900","message":"run-…","answers":{…}}}
```

Errors: `{"id":n,"error":{"code":"…","message":"…"}}`, stable codes
(`not-found`, `invalid`, `closed-form`, `conflict`, `denied`). Every
handler is a thin call into a host module (`src/inbox/ops.rs` for the
inbox: every mutation and read is a function over the store), so the TUI,
the CLI and external GUIs produce identical events by construction. Each
user item records its client (`tui`, `cli`, `api:<name>` from a `hello`
handshake) in the store for the user's audit; owners never see it.
Schema: `docs/api.md`, TypeScript types generated from the serde structs.

### `devsandbox api --stdio`

A stateless relay: stdin/stdout ↔ the socket, starting the daemon if
needed, exiting when stdin closes. For clients that can only spawn a
process (editor extensions, Electron apps) and shouldn't deal with socket
paths or start races. Clients that can open a unix socket talk to the
daemon directly. The npm package wraps it (`inbox`, `instances`,
`forwards`, typed).

### CLI

```
devsandbox inbox ls [--view needs-you|active|done|all] [--json]
devsandbox inbox show <owner>/<key> [--json]
devsandbox inbox reply <owner>/<key> <text|->       # - reads stdin
devsandbox inbox act <owner>/<key> <action-id>
devsandbox inbox submit <owner>/<key> <message-id> --json '<answers>'   # or stdin
devsandbox inbox done|reopen <owner>/<key>
```

`<owner>/<key>` may also be a numeric store id (as `ls` shows; notify
threads have no key). `<owner>` is the instance name (resolved to its id); `--json` uses the
`{"schema":1,"data":…}` envelope of `docs/json-output.md` around the API's
view structs (`src/inbox/wire.rs`). `ls`/`show` read the store directly
(work without the daemon); mutations go through a running daemon's API
(never lazy-started) so subscribers and owners are notified at once, else
apply locally through the same checks. See `docs/inbox-cli.md`.

### TUI Inbox

Keep the side-by-side layout and cards (`inbox-threads.md`, *Inbox layout
v2*). Change the thread pane:

```
╭─ #6900 feat/agent-run-cost-event ↗ ───────────────────╮
│ ● needs you · review drafts          bab-disp · pr-6900 │  header (pinned)
│ [1] Retry  [2] Done  [o] VS Code  [l] Logs              │
├─────────────────────────────────────────────────────────┤
│ ╭ Replies to post ────────────────────────── open form ╮│  open forms (pinned
│ │ 1/3 greptile on src/cost.ts:42                       ││   while open)
│ │   > Consider batching these writes.                  ││
│ │   (•) Post the reply   ( ) Don't reply               ││
│ │   Reply: Already batched in `flush()` (src/cos…      ││
│ │ [Tab] next  [Space] pick  [e] edit  [Enter] Confirm  ││
│ ╰──────────────────────────────────────────────────────╯│
│ 2m  bab-disp                                            │  feed, newest first
│     Addressed 3 of 4 comments, 1 needs your call. …     │
│ 9m  you                                                 │
│     Is this PR still relevant? Include a snippet.       │
│ 9m  · active → running comments                         │
├─────────────────────────────────────────────────────────┤
│ ╭──────────────────────────────────────────────────────╮│  composer (grows
│ │ Instructions for this PR…                            ││   to 6 rows)
│ ╰──────────────────────────────────────────────────────╯│
│  Starts an agent with your message                      │  compose.hint
╰─────────────────────────────────────────────────────────╯
```

- **Header pinned** (never scrolls): title + link marker, one chip line
  `<state> · <status>` (the same text as the card's chip, from one function),
  owner and child (with `(stopped)`), then the action row: numbered owner
  actions plus the always-available child keys.
- **Open forms pinned under the header** while open, newest first, then the
  feed newest first. Scroll starts at the top, which is now the newest
  thing. Submitted forms move into the feed as answered messages, with the
  submission folded under them.
- **Form editing** is its own focus zone (list → thread → form/composer):
  `Tab`/`Shift-Tab` move between questions, `↑`/`↓` move within options,
  `Space` picks, `e` on a text question opens it in the textarea (`Enter`
  or `Esc` keeps the edit, `Alt-Enter` inserts a newline). `Enter` outside a
  text edit is **Confirm**: it shows a one-line summary (`submit 4 answers?`,
  missing required answers named) and a second `Enter` submits, `Esc`
  cancels. Every change saves the draft through `form.saveDraft`.
- **Composer**: a real wrapping textarea, shared by text questions. Wraps by
  display width, grows from 1 to 6 rows then scrolls vertically, keeps the
  cursor visible (fixes the clipping bug at `ui.rs:1204-1227`); `Enter`
  sends, `Alt-Enter` / `Shift-Enter` (when the terminal reports it) inserts a
  newline. Built on `Prompt`'s edit primitives (`src/tui/prompt.rs`) with a
  wrap/viewport layer, unit-tested on its own. The `:` prompt gets the same
  horizontal-scroll fix (`ui.rs:1912-1928`).
- **Card chip = pane chip**: `chip()` always shows `<state marker> <status
  or state name>`; one function used by both (`ui.rs:770-787`,
  `inbox.rs:311-320` today).
- `?` help and footer hints per zone; `docs/tui.md` updated.

## Implementation steps

One step = one commit; `cargo test --workspace` after each; every behavior
gets unit tests in its file's `#[cfg(test)]` module (pure layout fns,
store ops, schema rules), TestBackend renders for the pane and form.

**Standalone fixes (land first, no dependency):**

1. **Reply box fix.** Wrapping textarea widget (`src/tui/textarea.rs`: wrap
   by display width, viewport follows cursor, grows to N rows), used by the
   Inbox input; horizontal scroll for the `:` prompt. Tests: long input,
   wide glyphs, cursor at every edge, resize.
2. **One chip function** for card and pane. Feed rendering unflattened (full
   reply text as markdown).

**Daemon and API (the base everything else is built on):**

3. **`inbox::ops`.** Move every TUI mutation behind store functions
   (`src/inbox/ops.rs`); the TUI calls only these. No behavior change.
4. **`local_endpoint` + `devsandbox serve` skeleton.** Socket bind with
   permissions, lock file, lazy start from clients (`setsid`, wait for the
   socket), version handoff (drain + exit), idle exit with the holder table
   (holders stubbed), `serve.keep-alive`, `serve.log`. Unix only; the
   command is absent on Windows. Tests: concurrent starts (one winner),
   handoff, idle countdown as a pure fn.
5. **Bridges move into the daemon.** `Bridges` (reconcile, helper reinstall,
   notify sink, control handler) runs in `serve`; the outbox drain and
   popups with it; the `autostart = true` pass runs at daemon start.
   Holders: running `dispatcher`/`inbox` instances, in-flight control.
   Store-change notifications (a channel fed by `store::update`) replace the
   500 ms poll for `events --wait`.
6. **The API.** JSON-lines protocol (`hello`, request/response, errors,
   `subscribe` topics), `inbox.*` over `inbox::ops`, `instances.list`,
   `docs/api.md`. Clients count as holders.
7. **TUI as a client.** The TUI stops spawning bridges and uses the API for
   inbox, instances and control-dependent features; two dashboards share
   one daemon.
8. **ssh-agent and forwards through the daemon.** `exec`/`start` drop their
   per-command bridge and ensure the daemon + the instance's bridge
   (`exec` sessions are holders); `forwards.*` API, forwards survive closing
   the TUI and are holders. Remove the per-command bridge code.
9. **`devsandbox api --stdio`** relay, npm wrapper (`inbox`, `instances`,
   `forwards`) with hand-written types guarded by a Rust drift test
   (`src/serve/dts.rs`; no codegen dependency).
10. **`devsandbox serve install|uninstall`** (systemd user unit,
    LaunchAgent), implies keep-alive.

**Follow-ups agreed after step 10 (2026-10-06):**

10a. **Per-user global config** `<data-dir>/devsandbox/daemon.config.toml`
     (next to `state.toml`; `$XDG_DATA_HOME` or `~/.local/share`) with
     `[serve] keep-alive = true`, read by `devsandbox serve`.
10b. **Upgrades keep the installed unit.** A newer client that triggers a
     handoff while a unit is installed rewrites the unit to point at its own
     binary (same captured env as `serve install`) and restarts it through
     the manager, instead of spawning an unmanaged daemon. Newest binary
     always wins, dev builds included (re-run `serve install` from the binary
     you want to keep).
10c. **Ad-hoc forwards persist** in `<data-dir>/devsandbox/forwards.toml`, so
     a successor daemon (handoff, restart, boot) recreates them on their host
     ports; `forwards.rm` removes them.
10d. **Docs: future work.** The TUI still collects its own instance snapshot
     (not `instances.list`); instances with `forwardPorts` hold the daemon
     forever. Both recorded in `docs/serve.md` as things to revisit.

**Content model:**

11. **Ownership.** `inbox = true` sandbox setting (config schema, docs
    site config page); thread verbs check it instead of `dispatcher`;
    `dispatcher` keeps child ops only.
12. **Store v3 (JSON) + feed model.** Header without `message`, `compose`,
    feed items (message, reply, action, submission, markers), caps and
    retention, v2 notify carry-over, v2 threads dropped. Rewrite the apply
    logic in `src/inbox/mod.rs` (markers instead of per-field entries; marker
    collapsing as a pure fn). Each user item records its client (`tui`,
    `cli`, `api:<name>` from the hello; deferred here from step 6), never
    exposed to owners. The API view structs (`src/serve/api.rs`) and the
    npm typings move to the v3 shapes in the same commit (the drift guard in
    `src/serve/dts.rs` enforces it); the current pane is adapted just enough
    to render v3 until step 14 rewrites it.
13. **`thread send` / `thread withdraw` end to end.** Schema
    (`src/inbox/message.rs`, blocks `markdown`/`fields`), outbox kinds
    `thread-send` / `thread-withdraw` + coalescing per `(thread, id)`
    (`devsbd/src/outbox.rs`, `src/devsbd/notify.rs`), validator extraction
    of `thread`/`id`, host apply (insert / replace / withdraw, edited tag),
    `thread ls --feed`. Bump the helper's thread capability; old-shape puts
    become an error record.
14. **Pane layout v3.** Pinned header + action row, feed newest first,
    composer + `compose.hint`, stopped-child labels. Rewrite the pane tests.
15. **Forms: schema and store.** `src/inbox/form.rs` (types, limits,
    validation), lifecycle (open/submitted/withdrawn), drafts, `submit` event
    with defaults filled in, `inbox.form.saveDraft`/`inbox.form.submit`
    (API, `docs/api.md`, npm typings).
16. **Forms: TUI.** Pinned open forms, form focus zone, choice/text/confirm
    widgets (text uses step 1's textarea), `Enter` Confirm, submissions
    folded in the feed.
17. **Events v3.** `thread` instead of `key`, `submit` kind, `--thread`
    filter.
18. **`--follow`.** `Subscribe` control op with a held-open response fed by
    step 5's notifications, pings, one follower per owner (`replaced`),
    exit 75 on drop. Followers are holders. Docker helper test: follow,
    enqueue from the host, line arrives; second follower replaces the
    first; kill the daemon, exit 75.
19. **CLI** `devsandbox inbox …` (reads from the store, mutations via the
    API with the TUI's fallback to `inbox::ops` when no daemon answers, so
    events are identical either way), `--json` envelope.
20. **Host actions on stopped children** (start first, `(stopped)` label);
    JSON answers for `ensure`/`done`/`stop`/`rm`.
21. **Docs.** `automations-guide.md` (`inbox = true`, threads, messages,
    forms, events, follow), `automations.md` (protocol), `sandbox-helper.md`
    (verbs, `Subscribe`, bridges owned by the daemon), `ssh-agent.md`,
    `port-forwarding.md`, `tui.md`, `json-output.md`, `api.md`, site pages
    (daemon, config, key tables), CHANGELOG. Mark `inbox-threads.md`
    superseded where this doc replaces it.

The dispatcher plan can start right after step 5 (it works with the TUI
closed), switch to messages after 13, forms after 17, and `--follow` after 18.

## Follow-ups

### 13b

Step 13 landed as 13a (`thread send`/`withdraw`, `thread ls --feed`) with
the v2 shapes still accepted, so the running PR babysitter keeps working.
13b removes that compat once the dispatcher has moved to messages, `compose`
and `thread`. Every site is tagged `v2 put compat` or `v2 event compat` in
`src/`:

- **Puts.** `ThreadPut::message` and `ThreadPut::reply`, the `Reply` type,
  `ThreadPut::compose()`'s fallback to `reply` and the `reply.placeholder`
  check (`src/inbox/thread.rs`); the `header-message` feed item a put's
  `message` becomes, `feed::HEADER_MESSAGE`, `header_message`,
  `HeaderChange`, `header_change`, `apply_header_change`
  (`src/inbox/feed.rs`), `Inbox::apply_header` and its calls in
  `Inbox::put`, the popup body quoting `message`, the sanitizing of both
  fields (`src/inbox/mod.rs`); the reserved `header-message` id in
  `src/inbox/message.rs`. With `deny_unknown_fields`, an old-shape put then
  becomes a `thread put rejected` error record.
- **`thread ls`.** `Thread::to_put` stops echoing `message`/`reply`
  (`src/inbox/mod.rs`), and `message_rows` stops skipping the header message
  (`src/commands/dispatch.rs`).
- **Events.** `EventLine::key`, the deprecated alias of `thread`
  (`src/commands/dispatch.rs`), and its mention in `src/devsbd/control.rs`'s
  module doc.
- **Tests and docs.** The tagged tests in `src/inbox/mod.rs`,
  `src/commands/dispatch.rs` and `src/tui/app/inbox.rs`; the compat notes in
  `docs/automations-guide.md` (the put's old fields, `key` on events) and
  `docs/automations.md` (`key` in the `events` line, the `header-message`
  item).

## Open questions

- ~~Feed order in the TUI~~: decided 2026-10-06, **newest first**.
- **Multiple open forms per thread**: allowed (one per message). Should a new
  form from the same owner auto-withdraw older open ones? Leaning no: the
  owner withdraws explicitly.
- **Windows daemon**: named pipe via `local_endpoint` once bridging is
  ported; dependency (`interprocess` or hand-rolled) decided then.
- **Shared threads across users** stay out of scope (`inbox-threads.md`).
- **Listener grants** (letting a thread's child follow its events): not now;
  only the owner listens.
