# The devsandbox API

What `devsandbox serve` answers on its socket, for the dashboard, the CLI
and external clients (editor extensions, GUIs). Design and roadmap:
`docs/inbox-redesign.md`, "One API, served by the daemon". Code:
`src/serve/api.rs` (methods), `src/serve/daemon.rs` (connections,
`subscribe`), `src/serve/proto.rs` (envelope).

## Transport

A unix socket, `serve.sock` in the daemon's socket dir, mode 0600; the
daemon starts on demand. Paths, lazy start, permissions and the version
handoff: `docs/serve.md`. **Whoever can connect can act as the user** on
every sandbox, like `docker.sock`; there is no other access control. A
`devsandbox api --stdio` relay for clients that can only spawn a process is
planned (step 9).

## Framing

JSON lines: one UTF-8 JSON object per `\n`-terminated line, both ways. Lines
longer than 1 MiB close the connection. Three shapes:

```
→ {"id":1,"method":"inbox.threads.list","params":{"view":"needs-you"}}     request
← {"id":1,"result":[…]}                                                    response
← {"id":2,"error":{"code":"not-found","message":"no thread 7"}}            error response
← {"method":"inbox.changed","params":{"generation":3}}                     notification
```

- `id` is a client-chosen unsigned integer, echoed on the response.
  Requests are answered in order, one at a time per connection.
- `params` may be omitted when a method takes none (it counts as `{}`).
  Unknown params fields are ignored.
- A line with a `method` and no `id` is a notification; it can arrive
  between a request and its response.
- A line that isn't a request answers `invalid` with no `id`.

## Hello

Send it first:

```
→ {"id":0,"method":"hello","params":{"version":"0.6.0","build":1767225600,"client":"api:my-ext"}}
← {"id":0,"result":{"version":"0.6.0","build":1767225600,"protocol":1}}
```

| param | |
|---|---|
| `version` | the client's devsandbox semver; external clients send the version they were built against |
| `build` | optional; the devsandbox binary's mtime (unix secs), for dev builds |
| `client` | who's calling: `tui`, `cli`, `api:<name>` (logged; recorded for audit later) |

Result: the daemon's `version` and `build`, the API `protocol` number, and
`"handoff": true` when the client's version is newer than the daemon's: the
daemon is then exiting, and the client should start (or wait for) a new one
(`docs/serve.md`, *Version handoff*). An external client that isn't a
devsandbox binary should send a version no newer than the installed one.

## Stability

`protocol` is `1`. Within one protocol number changes are additive only:
new methods, new optional params, new result fields, new error codes, new
notifications. Clients must ignore fields and notifications they don't
know. A breaking change bumps `protocol`; check it after the hello.

## Errors

`{"id":n,"error":{"code":"…","message":"…"}}`. `message` is for humans;
match on `code`:

| code | |
|---|---|
| `not-found` | the thread, action or directory named doesn't exist |
| `invalid` | bad params (missing, wrong type, unknown value), or an op that makes no sense for its target |
| `unknown-method` | no such method on this daemon |
| `denied` | the target refuses it (an archived thread is read-only; a thread that takes no replies) |
| `conflict` | reserved (forms) |
| `internal` | the daemon failed (store unreadable, …); logged in `serve.log` |

## Threads

Every `inbox.thread.*` method names its thread one of two ways:

- `{"thread": 12}`: the store id, from any summary. Stable for the
  thread's life.
- `{"owner": "bab-disp", "key": "pr-6900"}`: a dispatcher thread by its
  owner, the instance name or `instance_id`, and its key. The id is tried
  first; a name that named several instances over time (removed instances
  keep theirs on archived threads) picks the live thread, then the most
  recently changed. Notification threads aren't addressable this way.

Giving both, or neither, is `invalid`; no match is `not-found`.

### Thread summary

What `inbox.threads.list` returns per thread, and the top of
`inbox.thread.get`:

| field | |
|---|---|
| `id` | store id (u64) |
| `owner` | owner's `instance_id` |
| `owner_name` | owner's instance name when it last wrote |
| `key` | thread key, or `null` (an unkeyed notification) |
| `kind` | `thread` (`devsbd thread put`) or `notify` (`devsbd notify` records) |
| `state` | `needs-you` \| `active` \| `done`; `null` on a notification |
| `status` | the dispatcher's status line, or `null` |
| `title` | a thread's title; a notification's newest message, first line |
| `level` | a notification's newest level (`info` \| `warn` \| `error`), else `null` |
| `unread` | |
| `archived` | the owner was removed: read-only history |
| `needs_you` | in the `needs-you` view (what badges count) |
| `changed_at` | unix secs of the last change |

### `inbox.threads.list`

Params: `{"view": "needs-you" | "active" | "done" | "all"}`, default `all`.
Result: an array of summaries, last change first. The views are the
dashboard's, by the same code (`src/inbox/view.rs`):

- `needs-you`: live threads in state `needs-you`, plus unread notifications.
- `active` / `done`: live threads in that state.
- `all`: everything, archived threads and read notifications included.

Errors: `invalid` (unknown view).

### `inbox.thread.get`

Params: a thread address. Result: the summary's fields plus

| field | |
|---|---|
| `link` | `http(s)` link, or `null` |
| `child` | key of the dispatcher child it's about, or `null` |
| `message` | the dispatcher's message, or `null` |
| `reply` | `{"placeholder": …}` when it takes replies, else `null` |
| `actions` | `[{"id","label","done","host","sends_event"}]`: `host` is the host verb (`vscode`, `terminal`, `logs`, `forward`, `open`, `rm`) or `null`; `sends_event: false` means host-only |
| `entries` | a thread's timeline, oldest first: `[{"seq","at","kind","text"}]`, `kind` one of `message` `state` `status` `action` `reply` `done` `reopen` |
| `notes` | a notification's records, newest first: `[{"id","level","msg","link","at"}]` |
| `events_pending` | events the owner hasn't acked yet |

Errors: `invalid`, `not-found`.

### Mutations

Each answers `{"ok": true}`, is one locked store write, and enqueues the
event the dashboard would (the owner pulls it with `devsbd events`).

| method | params | does | errors |
|---|---|---|---|
| `inbox.thread.markRead` | address | mark read | `not-found` |
| `inbox.thread.act` | address + `"action": "<id>"` | press a dispatcher action: an `action` event (a `done: true` action also sets the thread done) | `not-found` (thread or action), `invalid` (host-only action, notification), `denied` (archived) |
| `inbox.thread.reply` | address + `"text": "…"` | a `reply` event; trimmed, cut at 2000 chars | `invalid` (empty, notification), `denied` (archived, takes no replies) |
| `inbox.thread.done` | address | set done (a `done` event); already done: nothing | `invalid` (notification), `denied` (archived) |
| `inbox.thread.reopen` | address | reopen a done thread (a `reopen` event); not done: nothing | `invalid` (notification), `denied` (archived) |
| `inbox.notify.dismiss` | `{"thread": id}`, or `{"all": true}` | remove one notification thread, or every one | `not-found`, `invalid` (a dispatcher thread, or `all` with a thread) |
| `inbox.notify.markRead` | `{"thread": id}`, or `{"all": true}` | mark one notification thread read, or every one (dispatcher threads are untouched) | `not-found`, `invalid` (a dispatcher thread, or `all` with a thread) |

Host verbs (`vscode`, `terminal`, …) run in the client that shows the
button; the API doesn't run them, and refuses a host-only action. An action
with both a host verb and an event (`notify: true`, `done: true`) sends only
its event through `act`. Marking a thread done through the API doesn't mark
its child instance done.

## Instances

### `instances.list`

Params: `{"dir": "/abs/path/to/config-root"}`, the directory `-C` would
take. Absolute: the daemon's cwd is `/`. Result: the `status --json`
snapshot's `data` (`docs/json-output.md`), without the envelope: the API
versions itself through `protocol`. It queries the container runtime, so it
can take a moment.

Errors: `invalid` (missing or relative `dir`), `not-found` (no such
directory).

## Notifications

### `subscribe` / `unsubscribe`

Params: `{"topics": ["inbox", "instances"]}`. Result: `{"ok": true}`.
Unknown topics are `invalid`. Notifications are coarse ("something
changed, re-fetch"): subscribe first, then fetch, so nothing falls in
between.

| notification | topic | params | when |
|---|---|---|---|
| `inbox.changed` | `inbox` | `{"generation": n}` | the store changed: through the daemon at once, through other processes (the dashboard, `devsandbox rm`) within 0.5 s. `n` grows per change the daemon saw; compare within one connection only |
| `inbox.shown` | `inbox` | `{"instance": "<name>", "line": "<name>: <text>"}` | a container message the daemon stored is worth a status line (a `devsbd notify`, a thread put that changed something); `line` is the dashboard's status-line text. Latest only: lines that arrive faster than the client reads are dropped |
| `instances.changed` | `instances` | `{}` | the running containers changed (the daemon polls every 5 s) |
| `closing` | any | `{"reason": "handoff" \| "idle"}` | the daemon is exiting; reconnect (which starts a new one) |

Notifications coalesce: many changes before one is written are one line.
A client that doesn't read its socket for 5 s is disconnected.

## Holders

A connected client keeps the daemon alive (`docs/serve.md`, *Idle exit*);
hang up when done.

## The dashboard as a client

The dashboard holds one connection for its life (`src/tui/daemon.rs`),
subscribed to `inbox` and `instances`, and reconnects after `closing` or a
lost connection (at once, then 1 s, 2 s, … up to 30 s apart; after a
handoff it waits up to 5 s for the successor before starting a daemon).
Its Inbox writes go through the `inbox.*` mutations while connected, and
straight to the store (the same code) while not. It reads the store file
itself, reloading on `inbox.changed`, and keeps its own instance snapshot
(with processes and stats), refreshed early on `instances.changed`.
`inbox.shown` is its status line. Two dashboards share one daemon.
