# The devsandbox API

What `devsandbox serve` answers on its socket, for the dashboard, the CLI
and external clients (editor extensions, GUIs). Design and roadmap:
`docs/inbox-redesign.md`, "One API, served by the daemon". Code:
`src/serve/api.rs` (methods), `src/serve/daemon.rs` (connections,
`subscribe`), `src/serve/proto.rs` (envelope), `src/serve/forwards.rs`
(the forwards behind `forwards.*`).

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
| `not-found` | the thread, action, directory, instance or forward named doesn't exist |
| `invalid` | bad params (missing, wrong type, unknown value), or an op that makes no sense for its target |
| `unknown-method` | no such method on this daemon |
| `denied` | the target refuses it (an archived thread is read-only; a thread that takes no replies) |
| `conflict` | reserved (forms) |
| `bind-failed` | a forward's host port couldn't be bound (in use, privileged, an address not on this host) |
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

## Bridges

### `bridges.ensure`

Params: `{"instance": "<name>", "agent": "/abs/path" | null, "wait": <seconds>}`.
`instance` is the instance name (its `state.toml` key); `agent` is the
caller's `$SSH_AUTH_SOCK`, optional. The daemon records `agent` as its
preferred host agent (the most recently reported live one wins; up to 8 are
kept, the daemon's own `$SSH_AUTH_SOCK` last), then reconciles that
instance's bridge at once instead of on its next 5 s poll: one is spawned if
missing, a dead one is respawned. Whether the instance gets a bridge at all
is the daemon's usual rule (running, helper installed; relays the agent in
relay mode). What `exec`, `start`, `run` and the dashboard's terminals send
before they inject `SSH_AUTH_SOCK` (`docs/serve.md`, *Bridges*).

Without `wait`, the result is `{"ok": true}` as soon as that's queued, not
when the bridge is up; ssh in the container waits up to 1 s for it
(`docs/sandbox-helper.md`).

With `wait` (seconds, a number ≥ 0, capped at 15), the answer comes once
that reconcile has run and the instance's bridge has finished its handshake
(it's then routing agent streams), or once `wait` runs out:

```
← {"id":4,"result":{"ok":true,"ready":true,"error":null}}
← {"id":5,"result":{"ok":true,"ready":false,"error":"helper in devsandbox-web is outdated (protocol 3, need 4): restart the instance"}}
```

`error` says why it isn't ready: the handshake failure (a helper version
mismatch, `helper handshake timed out`, …), the bridge `exec` not starting,
no bridge for the instance (not running, no helper), or the wait running out
(the daemon not reaching it, or a handshake still going). `run` and `start`
send `"wait": 12` before their lifecycle commands, which may `git clone` over
ssh right away. An older daemon ignores `wait` and answers `{"ok": true}`.

Errors: `not-found` (no such instance; nothing is recorded), `invalid`
(missing `instance`, relative or empty `agent`, negative or non-numeric
`wait`).

## Forwards

Port forwards (`docs/port-forwarding.md`) live in the daemon: ad-hoc ones
added here, and every running instance's configured `forwardPorts`, which
the daemon starts and stops itself for every config root recorded in
`state.toml`, following its 5 s container poll. They outlive the client
that added them and keep the daemon alive (`docs/serve.md`, *Idle exit*).
A daemon exit (idle or handoff) closes them all; the successor restarts the
configured ones on their saved host ports, ad-hoc ones are gone. The
foreground `devsandbox port` command doesn't use the daemon, and its
forwards aren't listed here.

### Forward row

| field | |
|---|---|
| `id` | daemon-wide id (u64), what `forwards.rm` takes; not reused within a daemon's life |
| `dir` | the canonical config root |
| `local` | the bound host address, e.g. `127.0.0.1:3000` |
| `target` | route label, e.g. `api:3000` or `db:5432 (via instance api)`; empty until the route first resolves |
| `process` | the listening process (`node (pid 412)`), or `null` |
| `state` | `active`, `connecting`, or `error: <reason>` |
| `conns` | open connections |
| `configured` | from a sandbox's `forwardPorts`, not `forwards.add` |

### `forwards.list`

Params: `{"dir": "/abs/config-root"}`, optional. Result: the rows of that
config root (canonicalized), or of every root without `dir`, by `id`.

Errors: `invalid` (relative `dir`).

### `forwards.add`

Params: `{"dir": "/abs/config-root", "instance": "<name>", "service":
"<svc>", "address": "<ip>", "spec": "[host:]port"}`, the inputs of
`devsandbox port`: `instance` (resolved like the CLI's instance argument,
but never prompting: a name that matches several instances is
`not-found`), `service` (forward one of its services
instead), or both; `address` defaults to `127.0.0.1`. `spec` `3000` binds
host port 3000 or the next free one up; `8080:3000` exactly 8080.

Result: `{"id": 4, "local": "127.0.0.1:3000", "target": "api:3000"}` once
the listener is bound; the route resolves on (re)connect, so a missing
container shows up later as the row's `state`. `target` this early is
usually the request's own `<instance or service>:<port>`.

Errors: `invalid` (relative `dir`, neither `instance` nor `service`, a bad
`spec` or `address`), `not-found` (no such directory or instance),
`bind-failed` (`"port 8080: address in use"`).

### `forwards.rm`

Params: `{"id": 4}`. Stops the forward (closing its listener and its
connections). Result: `{"ok": true, "local": "127.0.0.1:3000",
"configured": false}`. A configured forward stays stopped until its owner
stops and runs again (an instance; for a `global` service's port, every
running instance that declares it).

Errors: `not-found` (no forward `id`; another client may have stopped it).

## Notifications

### `subscribe` / `unsubscribe`

Params: `{"topics": ["inbox", "instances", "forwards"]}`. Result: `{"ok": true}`.
Unknown topics are `invalid`. Notifications are coarse ("something
changed, re-fetch"): subscribe first, then fetch, so nothing falls in
between.

| notification | topic | params | when |
|---|---|---|---|
| `inbox.changed` | `inbox` | `{"generation": n}` | the store changed: through the daemon at once, through other processes (the dashboard, `devsandbox rm`) within 0.5 s. `n` grows per change the daemon saw; compare within one connection only |
| `inbox.shown` | `inbox` | `{"instance": "<name>", "line": "<name>: <text>"}` | a container message the daemon stored is worth a status line (a `devsbd notify`, a thread put that changed something); `line` is the dashboard's status-line text. Latest only: lines that arrive faster than the client reads are dropped |
| `instances.changed` | `instances` | `{}` | the running containers changed (the daemon polls every 5 s) |
| `forwards.changed` | `forwards` | `{}` | any forward's row changed (added, stopped, state, process, connections; checked every 0.5 s), in any config root |
| `forwards.status` | `forwards` | `{"line": "…"}` | a one-line status: a configured forward started (`forwarding 127.0.0.1:3000 -> api:3000`) or failed, a connection note (`3000: connection refused`). Latest only, like `inbox.shown` |
| `closing` | any | `{"reason": "handoff" \| "idle"}` | the daemon is exiting; reconnect (which starts a new one) |

Notifications coalesce: many changes before one is written are one line.
A client that doesn't read its socket for 5 s is disconnected.

## Holders

A connected client keeps the daemon alive (`docs/serve.md`, *Idle exit*);
hang up when done. So does every forward, ad-hoc or configured, whether or
not its client is still connected: stop ad-hoc ones with `forwards.rm`. A CLI command holding a relay (`bridges.ensure`) stays
connected for its whole run for that reason.

## The dashboard as a client

The dashboard holds one connection for its life (`src/tui/daemon.rs`),
subscribed to `inbox`, `instances` and `forwards`, and reconnects after `closing` or a
lost connection (at once, then 1 s, 2 s, … up to 30 s apart; after a
handoff it waits up to 5 s for the successor before starting a daemon).
Its Inbox writes go through the `inbox.*` mutations while connected, and
straight to the store (the same code) while not. It reads the store file
itself, reloading on `inbox.changed`, and keeps its own instance snapshot
(with processes and stats), refreshed early on `instances.changed`.
`inbox.shown` is its status line. Opening an integrated terminal on a
relay-mode instance sends `bridges.ensure` with the dashboard's
`$SSH_AUTH_SOCK` (skipped while disconnected). Its Ports tab is
`forwards.list` for its config root, re-fetched on `forwards.changed` and
after every reconnect; `p` / `:port` is `forwards.add`, `d` is
`forwards.rm`, `forwards.status` its status line. While disconnected the
tab is empty and an add says the daemon is needed. Two dashboards share one
daemon, and the same forwards.
