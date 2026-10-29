# The devsandbox API

Every method and notification `devsandbox serve` answers on its socket,
for the dashboard, the CLI and external clients (editor extensions, GUIs,
the npm package). Read it when calling the API or changing a wire shape;
how clients use it: [client-server.md](../architecture/daemon/client-server.md).

Code: `src/serve/proto.rs` (envelope, `PROTOCOL`, `MAX_LINE`),
`src/serve/daemon.rs` (`handle`, `hello`, `subscribe`, `watch_root`,
`shutdown`, `machine` routing), `src/serve/api.rs` (`call`: every other
method), `src/inbox/wire.rs` (Inbox views, `ApiError`, `decide_*`),
`src/serve/verbs.rs` (verbs), `src/serve/helper.rs` (`helper.call`),
`src/serve/forwards.rs` (`forwards.*`),
`src/serve/machines/api.rs` (`machines.*`), `src/serve/machines/forward.rs`
(`QUICK`, federation), `src/serve/fleet/offer.rs` (`fleet.offer`, `fleet.agent`),
`src/serve/fleet/lender.rs` (`lend.*`), `src/serve/fleet/member.rs` (`member.*`),
`src/serve/relay.rs` (`api --stdio`),
`npm/devsandboxes/index.js` + `index.d.ts` (Node client; `src/serve/dts.rs`
checks the typings against the wire structs).

## Index

| group | methods | `machine`? |
|---|---|---|
| connection | `hello`, `subscribe`, `unsubscribe`, `shutdown`, `link.attach`, `links.inbound` (internal) | no |
| Inbox | `inbox.threads.list`, `inbox.thread.get\|markRead\|act\|reply\|done\|reopen`, `inbox.form.saveDraft\|submit`, `inbox.notify.dismiss\|markRead` | no |
| instances | `instances.list\|resolve\|cancel\|procs\|buildLog\|signal\|rename\|done` | yes |
| | `instances.watch\|unwatch` | no (`machines: true` federates) |
| sessions | `sessions.list\|rm` | yes |
| containers, config | `containers.logs\|inspect\|list\|stats`, `sandboxes.list\|get`, `services.get`, `config.findings\|migrate\|ack` | yes |
| | `roots.list` | no |
| bridges, forwards | `bridges.ensure`, `helper.call`, `forwards.list\|add\|rm` | no |
| verbs | `run`, `rebuild`, `start`, `stop`, `rm`, `services.rebuild`, `gc` | yes (own ssh) |
| machines | `machines.probe\|add\|list\|get\|rm`, `fleet.offer\|agent` (internal), `lend.get\|set`, `member.get\|set`, `capacity.get` | no |

Notifications: `inbox.changed`, `inbox.shown`, `instances.changed`,
`instances.snapshot`, `forwards.changed`, `forwards.status`, `closing`
(*Notifications*); `output`, `progress` (a verb's, *Verbs*);
`helper.record` (a `helper.call`'s).

## Transport

A unix socket, `serve.sock` in the daemon's socket dir, mode 0600; the
daemon starts on demand. Paths, lazy start, permissions and the version
handoff: [role.md](../architecture/daemon/role.md). **Whoever can connect can act as the user** on
every sandbox, like `docker.sock`; there is no other access control.
Clients that can only spawn a process use the relay instead (*Relay*
below); the npm package does.

## Framing

JSON lines: one UTF-8 JSON object per `\n`-terminated line, both ways. A
line longer than 1 MiB answers `invalid` (`request line too long`, no
`id`) and closes the connection. Blank lines are skipped. Three shapes:

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
| `client` | who's calling: `tui`, `cli`, `api:<name>`. Logged, and recorded in the store on the user's feed items (`tui` and `cli` as is, any other name as `api:<name>`) for the user's audit; never shown to owners |
| `force` | optional, internal (not in the npm typings): `true` hands off whatever the client's version, honored only from a local client of the daemon's own uid; a fleet worker's after installing its hub's version ([fleet](../architecture/daemon/fleet.md#versions)) |

Result: the daemon's `version` and `build`, the API `protocol` number, and
`"handoff": true` when the client's version is newer than the daemon's: the
daemon is then exiting, and the client should start (or wait for) a new one
([role.md](../architecture/daemon/role.md#version-handoff)). The daemon
doesn't enforce the hello: a connection that skips it is served, its user
ops recorded as client `api`. An external client that isn't a
devsandbox binary should send a version no newer than the installed one.

## Stability

`protocol` is `1`. Within one protocol number changes are additive only:
new methods, new optional params, new result fields, new error codes, new
notifications. Clients must ignore fields and notifications they don't
know. A breaking change bumps `protocol`; check it after the hello.

## Errors

`{"id":n,"error":{"code":"…","message":"…"}}`. `message` is for humans;
match on `code`. Some codes add a `data` object (`failed`: *Verbs*); it's
absent otherwise (`confirm` included).

| code | |
|---|---|
| `not-found` | the thread, action, directory, instance, sandbox, service, forward or machine named doesn't exist |
| `invalid` | bad params (missing, wrong type, unknown value), an op that makes no sense for its target, a `machine` on a method that doesn't take one, or a `machine` that isn't a non-empty string |
| `unknown-method` | no such method on this daemon |
| `denied` | the target refuses it (an archived thread is read-only; a thread that takes no replies), or a remote caller's trust doesn't reach it (*Remote callers*) |
| `closed-form` | the form was already submitted, or withdrawn by its owner |
| `conflict` | what the request was based on changed meanwhile: a config file edited since its findings were read (`config.migrate`) |
| `unsupported` | valid, but this target can't do it: a YAML config isn't rewritten (`config.migrate`); the message says what to do instead |
| `bind-failed` | a forward's host port couldn't be bound (in use, privileged, an address not on this host) |
| `failed` | a verb exited non-zero; `data`: `{"exit": n, "instance": name \| null}` (*Verbs*). Also a machine that couldn't be reached: ssh failed, the hello failed or the protocols differ, the link is `unreachable` (*Machines*) |
| `confirm` | an `rm` sent with `ask: true` needs an answer; `message` is the question, nothing was removed (*Verbs*) |
| `internal` | the daemon failed (store unreadable, …); logged in `serve.log` |

### Remote callers

A remote caller's connection is served with this member's trust table
([fleet.md](../architecture/daemon/fleet.md#the-trust-table)), read from
its `member.toml` per request: a fleet hub's over the lending link, and
a link peer's channel streams (after `link.attach`, below). `member.*`, `link.*` and `links.*` are `denied` under
either trust, as is a hello's handoff. Under `trust = "worker"` (the
default) the caller may call `hello`, `subscribe` / `unsubscribe`
(`instances` only), `instances.watch` / `unwatch`, the quick calls (the
`machine`-taking methods in the index but the verbs), `capacity.get`,
`roots.list` and the verbs but `gc` (`all` only while every instance here
is in reach; `gc` is host maintenance, `full` only), never with `machine` or `machines: true`; every `root` / `dir`
must be under this member's fleet root for the caller (a hub's, a fleet member's) or exactly one of its
own roots (`roots.list`'s plus `member.toml`'s `roots`), every named
instance or container one of theirs; `config.migrate` / `config.ack`
only under the fleet root; answers and pushed snapshots are cut to those
roots' instances. Anything else is `denied`, naming `devsandbox machine
trust full` (run on this member) as what opens it. Under `full` the
caller has the API a local client has.

### `link.attach`

Internal (not in the npm typings): the first line of a connection that
carries one of a link's channel streams (a `fleet-link --peer` sends it,
[fleet.md](../architecture/daemon/fleet.md#peer-links)). From then on that
connection is a remote caller's (*Remote callers*); there is no way back.

| params | result | errors |
|---|---|---|
| `{"peer": name, "channel": "api" \| "pty" \| "tunnel" \| "transfer" \| "agent"}`: `peer` 1 to 255 bytes, a machine name (letters, digits, `.`, `_`, `-`, not starting with `.` or `-`), logged; `channel` required | `{"ok": true, "trust": "worker" \| "full"}`, this member's trust at the time | `denied` (a remote caller's connection already, a second attach included, or a connection of another uid), `invalid` (a bad `peer`, a missing or bad `channel`; bytes past the attach line already sent, or a subscribed connection, for a channel other than `api`), `failed` (no data dir for the fleet root) |

The caller is link peer `peer`: its fleet root is `fleet/<peer>/` in the
data dir (created by the attach), and its verbs run with the daemon's own
env, whatever they send, `SSH_AUTH_SOCK` the peer's relayed agent
(`agent-link-<peer>.sock`) while that answers.

- `api`: the connection goes on as an API connection, the trust table's,
  its first byte after the answer the client's (no sentinel on a link
  stream).
- `pty`, `tunnel`, `transfer`, `agent`: once the answer is out the
  connection stops being JSON lines; its bytes are that channel's stream,
  as on a fleet link ([fleet.md](../architecture/daemon/fleet.md)):
  `pty` a session attach or exec (needs the host side's helper client,
  else the stream closes), `tunnel` a forward's bridge, each into an
  instance the trust table reaches; `transfer` one transfer into the fleet
  root; `agent` this host's ssh agent (a `1` byte, then the relay) while
  this host's `machines.toml` has an entry named `peer` with `agent` on,
  else a `0` byte and the close.

The client sends nothing after the attach line until it has read the
answer: bytes the daemon already holds past the line refuse the attach
(`invalid`) rather than being lost.

### `links.inbound`

Internal (`devsandbox fleet-link --peer`'s; not in the npm typings): a
link that machine `peer` dialed to this host is up
([fleet.md](../architecture/daemon/fleet.md#peer-links)), for as long as
this connection is open. While it is, `peer` is an inbound neighbor
(`machines.list`, below); the newest announce of a name wins, so an older
connection closing doesn't remove a newer one's.

| params | result | errors |
|---|---|---|
| `{"peer": name, "version": hello, "hub"?: bool}`: `peer` a machine name; `version` the dialer's link `Hello` (`<semver> <git> <published\|dev> <os>/<arch> <hub\|member>`); `hub` (default `false`) whether it said it's the hub | `{"ok": true}`; then the daemon asks `peer`'s roots over the link (`roots.list` through `fleet-<peer>.sock`) | `denied` (a remote caller's connection or another uid's; or `machines.toml` has an entry named `peer`, any kind: the message names it), `invalid` (a bad name, a missing param), `failed` (no machines registry), `internal` (`machines.toml` doesn't read) |

## Inbox threads

Every `inbox.thread.*` method names its thread one of two ways:

- `{"thread": 12}`: the store id, from any summary. Stable for the
  thread's life.
- `{"owner": "bab-disp", "key": "pr-6900"}`: an owner thread (`devsbd
  thread put`) by its owner, the instance name or `instance_id`, and its key. The id is tried
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
| `status` | the owner's status line, or `null` |
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
| `link` | `http(s)` link (a notification's: its newest record's), or `null` |
| `child` | the key, among the owner's dispatcher children, of the child it's about (host actions target it), or `null`: the thread is about the owner itself |
| `compose` | `{"placeholder": …, "hint": …}` when it takes replies, else `null`; `hint` is one line saying what sending does now |
| `actions` | `[{"id","label","done","host","sends_event"}]`: `host` is the host verb (`vscode`, `terminal`, `logs`, `forward`, `open`, `rm`) or `null`; `sends_event: false` means host-only |
| `feed` | an owner thread's feed, oldest first (first-insert order); see *Feed items* |
| `notes` | a notification's records, newest first: `[{"id","level","msg","link","at"}]` |
| `events_pending` | events the owner hasn't acked yet |

Errors: `invalid`, `not-found`.

#### Feed items

Each item has `type`, `seq` (arrival order across the whole Inbox) and `at`
(unix secs of first insert), plus:

| `type` | fields | |
|---|---|---|
| `message` | `id`, `blocks`, `edited`, `withdrawn` | from the owner (`devsbd thread send`); `blocks`, by `type`: `{"type":"markdown","text"}`, or `{"type":"fields","items":[{"label","value"}]}` (a key/value list, in order; clients show aligned `label  value` rows), or `{"type":"form",…}` (see *Forms*). `edited`: replaced in place since first sent; `withdrawn`: the owner took it back |
| `reply` | `text` | the user's reply |
| `submission` | `message`, `form`, `answers` | the user submitted form `form` of message `message`: every question's answer (see *Forms*). Clients show the answers here, with the outcome that follows, not only under the older message |
| `action` | `action`, `label` | the user pressed an action (its id, its label then) |
| `marker` | `marker`, `from`, `to` | one line: `done` / `reopen` by the user (`from`/`to` `null`), or the owner's `state` / `status` change (`from` → `to`, a status may be `null`). Consecutive status changes collapse into one marker |

A put that changes only other header fields (title, link, actions,
`compose`) adds nothing to the feed. The store also records which client
made each user item; the API and owners' events never carry it.

### Mutations

Each answers `{"ok": true}`, is one locked store write, and enqueues the
event the dashboard would (the owner pulls it with `devsbd events`, or gets
it at once on `devsbd events --follow`).

| method | params | does | errors |
|---|---|---|---|
| `inbox.thread.markRead` | address | mark read | `not-found` |
| `inbox.thread.act` | address + `"action": "<id>"` | press an owner's action: an `action` event (a `done: true` action also sets the thread done) | `not-found` (thread or action), `invalid` (host-only action, notification), `denied` (archived) |
| `inbox.thread.reply` | address + `"text": "…"` | a `reply` event; trimmed, cut at 2000 chars | `not-found`, `invalid` (empty, notification), `denied` (archived, takes no replies) |
| `inbox.thread.done` | address | set done (a `done` event); already done: nothing | `not-found`, `invalid` (notification), `denied` (archived) |
| `inbox.thread.reopen` | address | reopen a done thread (a `reopen` event); not done: nothing | `not-found`, `invalid` (notification), `denied` (archived) |
| `inbox.form.saveDraft` | address + `"message": "<id>"`, `"answers": {…}` | merge `answers` (any subset of the questions) into the form's draft; no event | `not-found` (thread, message, or no form on it), `closed-form`, `invalid` (unknown question, wrong type, bad value; one bad answer refuses all), `denied` (archived) |
| `inbox.form.submit` | address + `"message": "<id>"`, `"answers": {…}` | submit the form: `answers` over the draft over the defaults, one `submit` event with every answer | as `saveDraft`, and `invalid` listing the required questions left unanswered |
| `inbox.notify.dismiss` | `{"thread": id}`, or `{"all": true}` | remove one notification thread, or every one | `not-found`, `invalid` (an owner thread, or `all` with a thread) |
| `inbox.notify.markRead` | `{"thread": id}`, or `{"all": true}` | mark one notification thread read, or every one (owner threads are untouched) | `not-found`, `invalid` (an owner thread, or `all` with a thread) |

### Forms

A message block `{"type":"form", …}`:

| field | |
|---|---|
| `id` | the form's id |
| `title` | or `null` |
| `submit` | the submit button's label |
| `questions` | by `type`, each with `id`, `label` (inline markdown), `context` (markdown or `null`), `required`, `under` and `when` (a follow-up, below; `null` otherwise): `{"type":"choice","options":[{"id","label","description"}],"multiple","default"}` (`default`: an option id, ids when `multiple`, or `null`), `{"type":"text","placeholder","default","multiline","max"}` (`max` in bytes), `{"type":"confirm","yes","no","default"}` (`yes`/`no`: labels or `null`) |
| `state` | `open` \| `submitted` \| `withdrawn` (the owner withdrew the message, or re-sent it without the form) |
| `draft` | the saved answers (`inbox.form.saveDraft`) of a form not submitted (a withdrawn one keeps its draft), `{}` once submitted |
| `answers` | once submitted, every question's answer; `null` before |

Answers are keyed by question id: a choice takes an option id, a
`multiple` choice an array of ids, a text question a string (at most `max`
bytes), a confirm `true`/`false`. A submission's answers have every
question, defaults filled in: unanswered optional text is `""`, an
unanswered `multiple` choice `[]`, an unanswered optional choice or
confirm `null`. A required question needs an answer (text: not blank;
`multiple`: at least one id). A form is submitted once; a submitted form
is frozen, whatever its owner re-sends.

A question with `under` follows up that (earlier, top-level) question:
show it under its parent, unnumbered. With `when` too (the parent is a
choice) it applies only while the parent's answer (given, else draft, else
default) picks that option: otherwise hide it; it is never required then,
and the submission has it `null` whatever was given or drafted for it
(`saveDraft` still keeps its draft). Without `when` it always applies.

Host verbs (`vscode`, `terminal`, …) run in the client that shows the
button; the API doesn't run them, and refuses a host-only action. An action
with both a host verb and an event (`notify: true`, `done: true`) sends only
its event through `act`. Marking a thread done through the API doesn't mark
its child instance done.

## Instances

### `instances.list`

Params: `{"dir": "/abs/path/to/config-root", "depth": "full"}`, `dir`
the directory `-C` would take. Absolute: the daemon's cwd is `/`. Result:
the `status --json` snapshot's `data`, without the
envelope: the API versions itself through `protocol`; plus a top-level
`thread_children`.

`depth` (optional) is how much of the container runtime it asks, cheapest
first:

| `depth` | adds | cost |
|---|---|---|
| `disk` | state + config only; every container status is `{"state": "unknown"}`, no drift, cpu/mem or `runtime_version` | no runtime call |
| `listing` | container statuses, drift, `runtime_version` | tens of ms |
| `full` (absent) | `cpu`/`mem` | about 1.5 s on docker (a stats sample) |

`thread_children` maps each dispatcher's `instance_id` to its children by
thread key, `{"<owner id>": {"<key>": "<instance name>"}}`: the child a
thread's `child` key names, as the control ops resolve it (a key ambiguous
across sandboxes is left out). `{}` when there are none or the state
doesn't read (the snapshot's `error` says why). Not in `status --json`.

Errors: `invalid` (missing or relative `dir`, unknown `depth`), `not-found`
(no such directory).

Each call collects anew (uncached): it's for the CLI and scripts. A client
that shows a root continuously watches it instead (`instances.watch`), so
any number of dashboards cost the runtime one poll.

### `instances.watch` / `instances.unwatch`

Params: `{"dir": "/abs/path/to/config-root"}`. Result: `{"ok": true}`.
Connection-level, like `subscribe`: while this connection is open and
watches `dir`, the daemon pushes `instances.snapshot` for it (see
*Notifications*). Watching needs a `subscribe` to `instances` first, since
the pushes are notifications; unsubscribing from `instances` drops every
watch, and so does hanging up. A client re-watches after a reconnect.

While at least one connection watches a root, the daemon's container poll
runs every 2 s (otherwise on container events and a timer, see
`instances.changed`) and joins every watched root against its
one listing; stats (`cpu`/`mem`) are asked at most every 7 s, and at once
for a root that hasn't had them yet. A snapshot between stats passes
carries each running container's last `cpu`/`mem` over, so every pushed
snapshot is complete. With nothing watched, nothing is collected.

The first snapshot comes at once: the cached one when another watcher
keeps the root fresh, else one at `disk` depth (no runtime call), then a
`listing` one on the poll's next pass and a `full` one after its stats. A
`state.toml` change (a phase, a new instance) re-polls within 0.5 s.
Watching a root twice is one watch; `instances.unwatch` of a root not
watched is fine.

With `"machines": true` every push for `dir` is federated: merged with
every machine's rows (registered or inbound), plus a top-level `machines`, and the
machine links stay up while such a watch lasts (*Machines*, *Federated
snapshots*). Re-watching with another `machines` switches the variant.

Every push from the poll carries this host's `capacity` at the top level
(the same in every root's; the first `disk`-depth push of a new watch has
none), and each `machines` entry its machine's, from that machine's last
push (`null` before one carried it):

#### Capacity

```json
{"cpus": 8, "mem_total": 34359738368, "mem_available": 21474836480,
 "disk_free": 128849018880, "disk_total": 536870912000, "running": 3,
 "lending": "on", "caps": {"cpus": null, "mem": 17179869184, "disk": null, "instances": 4}}
```

Sizes in bytes; `null` is unknown. `cpus` and `mem_total` are the
runtime's (`docker info` `NCPU` / `MemTotal`, podman's `host.cpus` /
`host.memTotal`; a VM-backed engine's VM), asked at most once a minute.
`mem_available` (`/proc/meminfo` `MemAvailable`) and `disk_free` /
`disk_total` (the filesystem of the runtime's data root) only for a
native Linux engine, else `null`. `running`: instances whose container
runs. `lending`: `lend.toml`'s `state` (`null` without one); `caps`
`member.toml`'s `[caps]` soft caps (`null` when none is set): never enforced, over one is
a note ([fleet.md](../architecture/daemon/fleet.md#capacity)).

Errors: `invalid` (missing or relative `dir`; watch without the
`instances` subscription), `not-found` (watch of a missing directory;
unwatch only needs `dir` absolute).

### `instances.resolve`

Params: `{"name": "<name>"}`, as the CLI's `<name>` takes it. Result:
`{"matches": ["web", "web-2"], "found": [{"name": "web", "machine":
null, "root": "/home/me/.devsandboxes", "sandbox": "web"}, …]}`, sorted: an exact instance
name or id if any matches, else every instance of that sandbox or folder
basename. Two or more is the caller's to pick from (the CLI prompts on its
TTY), not an error; then it sends the chosen name to a verb. `found` is
`matches` again, in order, with where each lives: the unqualified name,
its machine (`null`: this host), its config root there (`null` for
instances older than that record) and its sandbox (what the CLI's
identity prompt asks `config.findings` about).

`"machines": true` asks this host and every machine, registered or inbound (*Machines*,
*Federated calls*).

Errors: `not-found` (nothing matches), `invalid` (no `name`).

### `instances.cancel`

Params: `{"instance": "<name>"}`, an exact instance name (resolve first).
SIGINTs the process recorded as building it (its process group, else the
pid alone), whoever started it; `run` then rolls back, `rebuild` records the
instance failed. Read fresh from `state.toml`,
and only while the instance is in flight, so a recycled pid isn't
signalled. Result: `{"ok": true, "pid": n}`, before the verb has stopped:
its own request answers once it has.

Errors: `not-found` (no such instance), `invalid` (not building any more:
ready, failed, or its process is gone), `internal` (the signal failed).

### `instances.procs`

Params: `{"instances": ["web", "api-2"]}`, exact instance names. Result:
an object keyed by each name, its container (from `state.toml`) listed
then:

```json
{
  "web": {
    "processes": [{"pid": "1", "ppid": "0", "args": "sleep infinity"}],
    "signalable": true,
    "runs": [{"id": "1790000000-a1b2", "state": "exited 4", "started": "2026-09-21T14:13:20Z", "argv": "sh -c make"}],
    "sessions": {"running": 2, "detached": 1}
  },
  "api-2": {"error": "not running"}
}
```

`processes` is `ps -eo pid,ppid,args` in the container, strings as printed.
`signalable`: the pids are the container's own; `false` when the image has
no `ps` and the runtime's host-side `top` listed it instead (host pids,
which can't be signalled from inside). `runs` are the helper's tracked runs
(`devsbd run ls`), oldest first: `state` is `running`,
`lost`, `exited <code>` or `killed <signal>`; `[]` without a helper that
tracks runs. `sessions` counts the helper's sessions (`devsbd session
ls`, what `devsandbox exec` runs in): `running`, and `detached`, those of
them with no client attached; absent without a helper that has sessions.
An instance that isn't in `state.toml`, isn't running, or
whose listing failed is an `{"error": "…"}` entry, never a failed call.
Up to three `exec`s per instance (`ps`, `run ls`, `session ls`): seconds
on a busy machine.

Errors: `invalid` (no `instances`), `internal` (`state.toml` doesn't
read).

### `instances.buildLog`

Params: `{"instance": "<name or id>", "lines": 500, "ansi": false}`: an
exact instance name or `instance_id`; `lines` optional (500, at most
10000); `ansi` optional (false). Result: the end of its build log
(`devsandbox logs --build`),
`{"text": "…", "cut": false}`, `cut` when earlier lines were left out;
`null` when it has none yet. A cancelled `run` drops its record, so its
log is then `not-found`.

A log written through a pty (its header's `# tty <cols>x<rows>` line, see
*Verbs* `tty`) is replayed through a terminal emulator at that size, so
progress displays that redraw in place come out as their final state. Its
lines are plain text, or with `ansi` carry SGR escapes (`ESC[…m`: colours,
bold, …) and no other escapes. A log written through pipes is its lines as
they are (a `\r` redraw keeps its last state), whatever `ansi` says.

With `"offset": <n>` (bytes; `lines` and `ansi` then don't apply) the
result is the log's bytes from there instead, what `logs --build
--follow` on an offered worker prints as they come:
`{"text": "…", "next": <n>, "tty": false, "complete": false}`, at most
128 KiB (`build_log::PAGE_BYTES`) of them as UTF-8 text (invalid bytes as
U+FFFD; a character the page or a write in progress cuts is left for the
next page, `build_log::page_text`), `next` the offset after them, `tty`
for a pty log, `complete` once nothing comes after: `next` is the log's
end and the instance wasn't in flight before the read. A follow asks
from `0`, then from each `next` until a page is `complete`; `null` as
above. An offset past the end is an empty page.

Errors: `not-found` (no such instance), `invalid` (no `instance`, a
negative `lines` or `offset`, a non-boolean `ansi`), `internal` (the log or
`state.toml` doesn't read).

### `instances.signal`

Params: `{"instance": "<name>", "pid": "42", "signal": "TERM"}`: an exact
instance name, a pid of its container (from `instances.procs`, when
`signalable`) as a string of digits, `signal` `TERM` or `KILL`. Runs the
runtime's `kill` in the instance's container (from `state.toml`). Result:
`{"ok": true}` once `kill` succeeded, not once the process is gone.

Errors: `invalid` (another signal, a pid that isn't a positive number, a
missing param), `not-found` (no such instance), `internal` (the runtime's
`kill` failed: no such process, the container isn't running).

### `instances.rename`

Params: `{"instance": "<name>", "newName": "<name>"}`, an exact instance
name. Moves its `state.toml` key, as `devsandbox rename`: the container,
worktree and instance id keep theirs. Result: `{"instance": "<new name>"}`,
trimmed.

Errors: `not-found` (no such instance), `invalid` (an empty new name, one
with spaces, the same name, one already taken, or the instance is still
being built by a `run`/`rebuild`), each with the CLI's message;
`internal` (`state.toml` doesn't read or write).

### `instances.done`

Params: `{"instance": "<name>", "done": true}`, an exact instance name.
Sets (`true`) or clears its done flag, as `devsandbox done|undone`;
idempotent (an instance already done keeps its original time). Result:
`{"instance": "<name>", "done": true}`, the flag as it now is.

Errors: `not-found` (no such instance), `invalid` (a missing param),
`internal` (`state.toml` doesn't read or write).

## Sessions

An instance's resumable sessions (`devsandbox exec`'s, and a fleet link's
`PTY` streams', docs/architecture/daemon/fleet.md), through its helper's
`session.*` RPC methods (`serve::api::helper_rpc`). The instance is an
exact name, else an instance id, whose config root is under `root`
(canonicalized), and must have the helper. The helper's `session.*`
methods are root only: an instance whose helper daemon isn't root
(`containerUser`), or an older helper, answers `unsupported`.

Errors of both: `invalid` (a missing param, a relative `root`),
`not-found` (no directory `root`; no such instance in it; no live bridge
to it: not running), `unsupported` (no helper, a helper without RPC, or
one not offering the method), `failed` (the helper's `error`, as
`<method>: <code>: <message>`, or the call broke), `internal`
(`state.toml` doesn't read).

### `sessions.list`

Params: `{"root": "/abs/config/root", "instance": "<name or id>"}`.
Result: `{"sessions": ["<line>", …]}`, the helper's `session ls` lines
(`<id> <state> <attached|detached|-> <started> <argv…>`), oldest first.

### `sessions.rm`

Params: `{"root": …, "instance": …, "id": "<session id>"}`. Ends
session `id` as `devsbd session rm` does (SIGHUP to its command's group,
SIGKILL if it's still there after a grace period). Result: `{"ok":
true}`. Also `invalid` for an id not in a session id's shape
(`<unix secs:010>-<4 hex>`).

## Containers

Only devsandbox's own containers (names starting `devsandbox-`, instances
and services alike): the API is no generic runtime proxy. A runtime error
is `internal` with the runtime's message, a missing container included:
the runtimes' messages don't tell it apart from a runtime that's down
reliably enough for `not-found`.

### `containers.logs`

Params: `{"container": "devsandbox-web", "lines": 50}`, `lines` optional
(50, at most 10000). Result: `{"text": "…"}`, its last lines of stdout
and stderr.

Errors: `invalid` (no `container`, not a `devsandbox-` one, a negative
`lines`), `internal` (the runtime failed).

### `containers.inspect`

Params: `{"container": "devsandbox-web"}`. Result: the runtime's inspect
JSON, parsed (docker and podman: an array of one object; Apple's
`container`: its own shape).

Errors: `invalid` (as `containers.logs`), `internal` (the runtime failed,
or its output isn't JSON).

### `containers.list`

Params: `{"all": false}`, `all` optional (`false`: running ones only).
Result: `{"rows": […]}`, every `devsandbox-` container, instances and
services, sorted by name: `ps --json`'s rows (`commands::ps::rows`), each
the runtime's `name`, `image`, `status`, `state`, `labels`, `host_ports`,
plus `done` (its instance is marked done; `false` for a service) and
`phase` while its instance is being created or failed (absent once
ready). `state.toml` is read best-effort: unreadable, no row is done.
On a fleet hub's connection under `worker` (*Remote callers*) the rows are
cut to what it reaches: those instances' containers and their config
roots' services ([fleet.md](../architecture/daemon/fleet.md#the-trust-table)).

Errors: `invalid` (`all` not a boolean), `internal` (the runtime failed).

### `containers.stats`

Params: `{}`. Result: `{"rows": [{"name": "devsandbox-web", "cpu":
"1.50%", "mem": "20MiB / 1GiB"}, …]}`, the running `devsandbox-`
containers' usage as the runtime renders it, sorted by name: `stats
--json`'s rows (`commands::stats::rows`). Cut on a fleet hub's
connection as `containers.list`'s.

Errors: `internal` (the runtime failed).

## Sandboxes

### `sandboxes.list`

Params: `{"root": "/abs/path/to/config-root"}`. Result: the config's
sandboxes, sorted by name: `[{"name": "web", "root": "/abs/…"}, …]`
(objects, so fields can be added; `root` is the canonical root). A root
without a config file has none: `[]`. `"machines": true` adds every
machine's (*Machines*, *Federated calls*).

Errors: `invalid` (missing or relative `root`, or a config that doesn't
load: the message says why), `not-found` (no such directory).

### `sandboxes.get`

Params: `{"root": "/abs/path/to/config-root", "name": "web"}`. Result:
the sandbox's config as TOML text, what the dashboard's config view shows:

```json
{"original": "extends = [\"base\"]\n…", "resolved": "image = \"alpine\"\n…", "hash": "…"}
```

`original` is `[sandbox.<name>]` as written, `extends` included;
`resolved` the table merged with what it `extends` (the drift check's
input) and `hash` its config hash. A sandbox that doesn't merge (an
`extends` naming no template, a cycle) still answers: `resolved` says why,
`hash` is `""`.

Errors: `invalid` (as `sandboxes.list`, or no `name`), `not-found` (no
such directory, or no such sandbox: a root without a config file has
none).

### `services.get`

Params: `{"root": "/abs/path/to/config-root", "name": "db"}`. Result:
`{"original": "…"}`, `[services.<name>]` as TOML text (services don't
`extends`, so it's also what runs).

Errors: as `sandboxes.get`, `not-found` for no such service.

## Config

A config's identity findings: what breaks under `identity = "host"`
(`/root` mount targets, env values and paths, a root user), the rewrite
that fixes them, and the acks that silence them
([config.md](../architecture/config.md), *Identity findings*). Code:
`src/config_identity/ops.rs`. The daemon writes the user's config file
here ([role.md](../architecture/daemon/role.md), *Files are the truth*).

### `config.findings`

Params: `{"root": "/abs/path/to/config-root", "sandbox"?: "web"}`.
Result:

```json
{"file": "/abs/…/devsandboxes.toml", "format": "toml", "fileHash": "…",
 "findings": [{"id": "…", "table": "sandbox.web", "kind": "root-target",
   "key": "mounts", "value": "source=/a,target=/root/.a",
   "edit": "sandbox.web: mounts entry \"…\" -> \"…\"", "sandboxes": ["web"], "acked": false}]}
```

`format` is `toml` or `yaml`; `fileHash` is what `config.migrate` takes
back. Each finding: `table` (`template.<name>` | `sandbox.<name>`) is
where it's written, `kind` one of `root-target`, `root-env`, `root-path`,
`root-user`, `edit` the update as a line for a person (`null`: fixed by
hand), `sandboxes` those it applies to (empty: a template no sandbox
uses), `acked` whether the user chose to continue as is. With `sandbox`,
only the findings applying to it.

Errors: `invalid` (as `sandboxes.list`), `not-found` (no such directory,
no config file, no such `sandbox`).

### `config.migrate`

Params: `{"root": "/abs/…", "ids"?: ["…"], "keep"?: ["…"], "fileHash": "…"}`.
Rewrites the config file in place, comments and layout kept: the findings
`ids` (absent: every one with an edit, but `keep`'s), and for the
`root-user` findings in `keep`, "continue as is" (`identity = "image"` in
their table). Result: `{"edits": 2, "drifted": ["web", "web-2"]}`, the
findings fixed (0 wrote nothing) and this root's instances whose config
hash the rewrite changed (they drift: `devsandbox rebuild <name>`).

Errors: `conflict` (the file's hash isn't `fileHash`), `unsupported` (a
YAML config; the message lists the edits to make by hand), `invalid` (as
`config.findings`, an id that isn't a current finding, a `keep` that
isn't a `root-user`, an id in both), `not-found` (as `config.findings`),
`internal` (the write failed; the message names a `.bak` left behind).
The hash is checked first, so a YAML config changed since `fileHash` is
`conflict`, not `unsupported`.

### `config.ack`

Params: `{"root": "/abs/…", "ids": ["…"]}`. Records "continue as is" for
those findings in `state.toml` (`identity_acks`, by the root's project
id), and drops acked ids no longer among the root's findings. Result:
`{"acked": ["…"]}`, every acked id of the root, sorted.

Errors: `invalid` (as `config.findings`, or an id that isn't a current
finding), `not-found` (as `config.findings`).

## Bridges

### `bridges.ensure`

Params: `{"instance": "<name>", "agent": "/abs/path" | null, "wait": <seconds>}`.
`instance` is the instance name (its `state.toml` key); `agent` is the
caller's `$SSH_AUTH_SOCK`, optional. The daemon records `agent` as its
preferred host agent (the most recently reported live one wins; up to 8 are
kept, the daemon's own `$SSH_AUTH_SOCK` last), then reconciles that
instance's bridge at once instead of on its next poll: one is spawned if
missing, a dead one is respawned. Whether the instance gets a bridge at all
is the daemon's usual rule (running, helper installed; relays the agent in
relay mode). What `exec`, `start`, `run` and the dashboard's terminals send
before they inject `SSH_AUTH_SOCK`
([host-poll.md](../architecture/daemon/host-poll.md#bridges)).

Without `wait`, the result is `{"ok": true}` as soon as that's queued, not
when the bridge is up; ssh in the container waits up to 1 s for it
([helper/ssh-agent.md](../architecture/helper/ssh-agent.md)).

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
ssh right away.

Errors: `not-found` (no such instance; nothing is recorded), `invalid`
(missing `instance`, relative or empty `agent`, negative or non-numeric
`wait`), `internal` (`state.toml` doesn't read, or a daemon without a
host side).

### `helper.call`

Internal (devsandbox's own clients; not in the npm typings): one RPC call
into an instance's in-container daemon over its bridge
([helper/bridges.md](../architecture/helper/bridges.md#host--container-requests),
[helper/protocol.md](../architecture/helper/protocol.md#rpc)).

Params: `{"instance": "<name>", "method": "<rpc method>", "params": {…}}`.
`instance` is the instance name (its `state.toml` key); `params` is
optional and must fit the RPC JSON subset: integers only, arrays of
strings only, objects nested at most 3 deep. `meta.caller` is the
connection's client (`cli`, `tui`, `api:<name>`).

The request holds its connection like a verb: nothing else is read from it
meanwhile, and a hang-up (queued or mid-call) cancels the call. The call
waits its turn behind the instance's other calls (16 at a time). Meanwhile,
on that connection only:

| notification | params | when |
|---|---|---|
| `helper.record` | `{"request": id, "record": "stdout" \| "stderr" \| "event", "data": …}` | the daemon sent output (`data` a string, UTF-8 lossy) or an event (`data` its object) |

Result: the call's last record, one key:

```
← {"id":6,"result":{"result":{"methods":["rpc.methods@1"]}}}
← {"id":7,"result":{"error":{"code":"unknown-method","message":"no method `x`"}}}
← {"id":8,"result":{"exit":0}}          or {"signal": 9}
```

An RPC `error` record is a result, not an API error: the call went
through. Errors: `invalid` (missing or empty `instance` or `method`,
`params` outside the subset, a `machine`), `not-found` (no such instance,
or no live bridge to it: not running, no helper, handshake not done),
`unsupported` (its helper has no RPC: restart the instance), `failed` (the
stream couldn't open, or ended without a last record: the message says
why), `internal` (`state.toml` doesn't read, or a daemon without a host
side).

## Forwards

Port forwards ([forwards.md](../architecture/daemon/forwards.md)) live in the daemon: ad-hoc ones
added here, and every running instance's configured `forwardPorts`, which
the daemon starts and stops itself for every config root recorded in
`state.toml`, following its container poll. They outlive the client
that added them and keep the daemon alive (*Holders*).
A daemon exit (idle or handoff) closes them all; the successor restarts the
configured ones on their saved host ports (`state.toml`) and the ad-hoc ones
on theirs (`<data-dir>/devsandbox/forwards.toml`, new ids). A forward into
a machine's instance or service (`forwards.add` with `machine`) is bound
here too, ad-hoc only: a machine's `forwardPorts` are never mirrored. The
foreground `devsandbox port` command doesn't use the daemon, and its
forwards aren't listed here.

### Forward row

| field | |
|---|---|
| `id` | daemon-wide id (u64), what `forwards.rm` takes; not reused within a daemon's life |
| `dir` | the canonical config root; with `machine`, the config root on that machine |
| `local` | the bound host address, e.g. `127.0.0.1:3000` |
| `target` | route label, e.g. `api:3000` or `db:5432 (via instance api)`; empty until the route first resolves. A remote forward's is `<machine>/<label>` (`box/web:3000`), the request's own `<machine>/<instance or service>:<port>` until the machine first answers |
| `process` | the listening process (`node (pid 412)`), or `null` |
| `state` | `active`, `connecting`, or `error: <reason>` |
| `conns` | open connections |
| `configured` | from a sandbox's `forwardPorts`, not `forwards.add` |
| `machine` | the machine whose instance or service it reaches; absent for this host's |

### `forwards.list`

Params: `{"dir": "/abs/config-root", "machines": true}`, both optional.
Result: this host's rows of that config root (canonicalized), or of every
root without `dir`, plus with `machines` every forward into a machine
(whatever `dir`), by `id`.

Errors: `invalid` (relative `dir`).

### `forwards.add`

Params: `{"dir": "/abs/config-root", "instance": "<name>", "service":
"<svc>", "address": "<ip>", "spec": "[host:]port"}`, the inputs of
`devsandbox port`: `instance` (resolved like the CLI's instance argument,
but never prompting: a name that matches several instances is
`not-found`), `service` (forward one of its services
instead), or both; `address` defaults to `127.0.0.1`. `spec` `3000` binds
host port 3000 or the next free one up; `8080:3000` exactly 8080.

With `"machine": "<name>"` (a registered machine, or an inbound one
while it's up, its forwards dropped when it goes; empty is none) the
forward reaches that machine's instance or service but still binds here:
the daemon serves the call itself (*The `machine` param*). `dir` is then
the config root on the machine (absolute, taken as is) and `instance` the
name there (the client's `instances.resolve` answer); each bridge is a
`TUNNEL` stream over the machine's link, whose far end resolves both
([forwards.md](../architecture/daemon/forwards.md), *Remote bridges*).

Result: `{"id": 4, "local": "127.0.0.1:3000", "target": "api:3000"}` once
the listener is bound; the route resolves on (re)connect, so a missing
container shows up later as the row's `state`. `target` this early is
usually the request's own `<instance or service>:<port>`.

The forward is saved in `<data-dir>/devsandbox/forwards.toml` with the host
port it bound (an automatic one too), so later daemons recreate it on that
port (on a free one near it if it's taken by then) until `forwards.rm`.

Errors: `invalid` (relative `dir`, neither `instance` nor `service`, a bad
`spec` or `address`), `not-found` (no such directory or instance; with
`machine`: ``no machine named `box` (devsandbox machine ls)``; a remote
instance that doesn't resolve is the row's `state` instead), `bind-failed`
(`"port 8080: address in use"`).

### `forwards.rm`

Params: `{"id": 4}`. Stops the forward (closing its listener and its
connections); an ad-hoc one is also removed from `forwards.toml`, so no
later daemon restores it. Result: `{"ok": true, "local": "127.0.0.1:3000",
"configured": false}`. A configured forward stays stopped until its owner
stops and runs again (an instance; for a `global` service's port, every
running instance that declares it).

Errors: `not-found` (no forward `id`; another client may have stopped it).

## Verbs

`run`, `rebuild`, `start`, `stop` and `rm` are methods taking the CLI
verb's flags as params. The daemon runs each as a `devsandbox --in-process
-C <root> <verb> …` subprocess (`src/serve/verbs.rs`): its own process
group, stdin null (every prompt takes its non-TTY default), and the
request's `env` as its whole environment, plus `GIT_TERMINAL_PROMPT=0`:
`${localEnv:…}`, `SSH_AUTH_SOCK`, `GIT_*`, `DEVSANDBOX_RUNTIME`, `PATH`
and `HOME` are the caller's, never the daemon's.

```
→ {"id":1,"method":"run","params":{"root":"/home/me/.devsandboxes","sandbox":"web","branch":"x","env":{…}}}
← {"method":"progress","params":{"request":1,"instance":"web-2","phase":"preparing"}}
← {"method":"output","params":{"request":1,"stream":"stderr","data":"…"}}
← {"id":1,"result":{"instance":"web-2","exit":0}}
```

| method | params (besides `root`, `env`) |
|---|---|
| `run` | `sandbox`, `name?`, `branch?`, `base?`, `json?`, `reprobe?`, `detach?`, `tty?`, `here?` |
| `rebuild` | `instance` \| `all: true`, `force?`, `reprobe?`, `detach?`, `tty?` |
| `start` | `instance` \| `all: true`, `detach?`, `tty?` |
| `stop` | `instance` \| `all: true`, `detach?` |
| `rm` | `instance`, `deleteBranch?` (`true` deletes, `false` keeps, absent: merged or gone branches go, local work stays), `force?`, `ask?`, `detach?` |
| `services.rebuild` | `name` (a service of the config root), `detach?` |
| `gc` | `force?`, `detach?` |

`reprobe` is the CLI's `--reprobe`: the runtime's identity mode is probed
again on the daemon's host instead of read from its cache
([runtimes](../architecture/runtimes.md#mode-detection)).

**Placement** (`run` without `machine`): on a fleet hub with a worker
taking work, `run` goes where placement scores best, this host or a worker
([fleet](../architecture/daemon/fleet.md#placement)). Placed on a worker,
the sandbox's config root and folders move there first, then the request
is forwarded as with `machine`, in the worker's copy of the root; its
output starts with a `stderr` line `placed on <worker> (<reasons>)` (and
`warning: <worker>: over cap: …` lines), and its `progress`, its result
and its detached answer carry `"machine": "<worker>"`. A sandbox that
can't move runs here, its output starting `note: not placed on <worker>:
<why>; running here` (`<why>` is `draining` when the worker refused the
run); a move that fails is the error `failed` (`data.exit`
`null`), nothing runs. `"here": true` (the CLI's `--here`) always runs
here.

`services.rebuild` runs `devsandbox service rebuild <name>`: no instance
verb, so its answer's (and `failed`'s) `instance` is always `null`, no
`progress` comes, and `ask` doesn't apply.

`gc` runs `devsandbox gc [--force]` at the root: the root's unused
services and networks, orphaned shell history and the host's dangling
agent links. No instance verb either (`instance` always `null`). Nothing
can answer its per-file shell-history prompt, so without `force` those
files are kept. A remote caller needs the member's `full` trust
(*Remote callers*).

`root` is the absolute config root (`-C`); `env` an object of strings,
required. `instance` is resolved as the CLI's `<name>` is, but an
ambiguous one fails (nothing prompts).

Errors before anything runs: `invalid` (bad params, a relative `root`,
`instance` and `all` both or neither, a `tty` side out of range, an `env`
key that is empty or holds `=` or NUL, a value holding NUL), `not-found`
(`root` isn't a directory), `internal` (the subprocess didn't spawn).

The request holds its connection until the verb exits; nothing else is
read from it meanwhile (a pipelined request waits its turn). Meanwhile, on
that connection only (no `subscribe`):

| notification | params | when |
|---|---|---|
| `output` | `{"request": id, "stream": "stdout" \| "stderr", "data": "…"}` | the verb wrote; `data` is UTF-8 (lossy), chunked as read, in order per stream. `run` with `json` writes its document on `stdout` |
| `progress` | `{"request": id, "instance": "<name>", "phase": "…"}` | an instance the verb is materializing changed phase (`state.toml`, read every 0.25 s): `preparing`, `building`, `creating`, a lifecycle step, or `failed` |

Result: `{"instance": name | null, "exit": 0}`. `instance` is the one the
verb recorded its progress on, else (`run`) the one instance it added,
else the one the request named (`null` for `all`, or a name that doesn't
resolve). A non-zero exit is the error `failed`, its `message` the verb's
last `Error:` line (causes joined with `: `), else its last non-empty
stderr line, else `` `devsandbox <verb>` exited with <n> ``; `data.exit`
the exit code (128 + the signal when killed).

**`rm`'s question**: without `deleteBranch`, `rm` asks before deleting a
branch with commits not on its remote, and off a TTY (as here) keeps it.
With `"ask": true` it answers the error `confirm` instead, `message` the
question, having removed nothing; the client asks its user and resends with
`deleteBranch` set. Without `ask`, the non-TTY default applies.

**Detach** (`run`, `rebuild`, `start`): `"detach": true` answers early,
`{"instance": name | null, "detached": true}`, while the verb runs on:
for `run` and a single `rebuild` once an instance records the verb's
process as its builder (plus `"record"`, the `run --json` record without
its envelope, for `run` with `json`), for `start` and `all` right after
the spawn. A verb that exits first answers as usual. No notifications follow
the answer; the client may hang up or send more requests.

The held verbs (`stop`, `rm`, `services.rebuild`, `gc`) take `detach` too, but
never answer early: the answer (`confirm` included) still comes at the
exit. For them `"detach": true` only means a hang-up before the answer
leaves the verb running instead of cancelling it. The dashboard sends it
on every verb, so quitting it never interrupts a `stop` or `rm`; the CLI
never does (Ctrl-C cancels).

**`tty`** (`run`, `rebuild`, `start`): `{"cols": n, "rows": n}`, each
side 1 to 1000 (else `invalid`), runs the verb's commands on a pty of that
size, as on a terminal: coloured output, docker build's TTY progress. The
pty is the verb process's own (its build log's tee, `src/build_log.rs`),
passed to it as `DEVSANDBOX_TTY=<cols>x<rows>` (an `env` entry of that
name is dropped either way), so a daemon exit doesn't hang it up. Both
streams then come as `stdout`, as a terminal shows them (`\r\n` line ends,
escapes); `stderr` carries only what the verb writes outside its build log
(before the log opens, after it closes: `main`'s `Error:`). The
build log records the size (`# tty <cols>x<rows>`). The dashboard sends
its terminal's width (rows capped at 40); the CLI its terminal's size when
stdout and stderr are one, never for `run --json`. Absent: pipes, plain
output.

**Hang-up = cancel**: a client that hangs up before the answer gets its
verb SIGINTed, process group included; `run` and `rebuild` then roll back
or record the instance failed. Not a `detach`
request: its verb runs on whenever the client leaves (cancel it with
`instances.cancel` once it's registered). A client that
stops reading for 5 s is disconnected as usual, but its verb runs on (a
paused pager isn't a cancel). So does a daemon exit (idle, handoff,
`shutdown`): the connection closes without an answer, the verb runs on.

The verb's stdout and stderr are spooled in
`<data-dir>/devsandbox/logs/verbs/<pid>.out|.err` while it runs (removed
once it's reaped); `run`, `rebuild` and `start` also write the instance's
build log, as from the CLI. A verb a daemon left running at its exit is
adopted by the next daemon through those files
([verbs.md](../architecture/daemon/verbs.md#surviving-the-daemon-spool-and-adoption)): it holds that daemon until it exits, unstreamed.

## Machines

Remote hosts whose daemons this one talks to over ssh
([machines.md](../architecture/daemon/machines.md)).
The registry is `<data-dir>/devsandbox/machines.toml`, next to
`state.toml`, written only by `machines.add` / `machines.set` /
`machines.rm` and `fleet.offer`:

```toml
[machine.my-server]
target = "me@my-server"            # what ssh takes, or ssh://[user@]host[:port]
roots = ["/home/me/.devsandboxes"] # config roots there, canonical
command = "~/bin/devsandbox"       # optional: the remote devsandbox, shell text
agent = true                       # optional, default false: this host's ssh-agent is meant for it

[machine.laptop]
offered = true                     # a fleet worker that dialed this host: no target, no ssh to it
```

A machine's `kind` is `ssh` (it has a `target`) or `offered` (a fleet
worker reached over its own link, `fleet.offer`; nothing here sshes to
it: quick calls and verbs go over that link, as do terminals, forwards
and the agent). On either kind the CLI's commands are quick calls, verbs
and `PTY` streams over the machine's link, see
[machines](../architecture/daemon/machines.md#the-cli-clirs).

A machine name is `[A-Za-z0-9._-]+`, not starting with `.` or `-` (never
`/` or `@`, so the CLI's `my-server/web-2` and `@/web-2` are unambiguous).
The API never parses a qualified name: requests carry `machine`.

**Transport.** This daemon holds one link to each ssh machine: `ssh -T -o
BatchMode=yes [-p N] -- <host> 'exec "$SHELL" -lc "exec <command>
fleet-link --peer <this host>"'`, `<this host>` its member name
(`member.toml`'s `name`, else the hostname's first label), in the
user's login shell there. It is a fleet [peer link](../architecture/daemon/fleet.md#peer-links):
`fleet-link` hands every stream to the machine's daemon with
`link.attach {peer, channel}`, so the machine's trust applies to
everything sent over it (*Remote callers*), and this host binds
`fleet-<name>.sock` and its siblings, each connection one stream there.
The link's `API` streams are what this daemon talks to the machine over
(the held connection, a fresh one per forwarded verb); a verb that way
runs with the machine daemon's own env. `BatchMode`: the daemon has no
TTY, so ssh fails instead of prompting. No `-A`: the machine opens
`AGENT` streams over the link, which this daemon answers with its
preferred host agent (*Bridges*) while the machine's `agent` is on
([machines.md](../architecture/daemon/machines.md#agent)). Each hello
says version `0.0.0`, so it never hands off the remote daemon; the
remote's `protocol` must equal ours, a different semver is allowed. Only
`machines.probe` and `machines.add`, before the machine has a link, ssh
to `<command> api --stdio --client machine:<this host> --sentinel` (the
remote's relay, *Relay*: a local client there, under the user's own ssh
login).

**Links.** Every registered machine's link is held while the daemon
runs, an inbound neighbor's while it's up (it is a machine of kind
`inbound` then; gone, its link and rows go): one `serve-machine-<name>`
thread per machine (plus one carrying an
ssh machine's peer link) connects at once and stays connected, whatever
watches. Connected, it subscribes to `instances` and, while a connection
watches with `machines: true`, watches each of the machine's roots. A
peer link that ends is redialed 1 s, 2 s, … 30 s apart; a failed connect
or a lost connection over a live one is retried the same way (a
forwarded call arriving meanwhile retries at once); the rows stay. A
link's `state`:

| `state` | |
|---|---|
| `idle` | not attempted yet (the link's thread is starting) |
| `connecting` | dialing its link and saying hello |
| `connected` | up; `error` may still name a registered root the machine refused |
| `unreachable` | the last attempt failed or the link was lost; `error` says why (ssh's last stderr, the machine's refusal, a protocol mismatch, `its devsandbox serve is exiting`, a hub build being followed: `updated to <build>: handing off`); on a fleet hub also rows restored from `remotes.json` until the link connects, `error` ending `(rows from <time> UTC)` |

### `roots.list`

On every daemon: this host's candidate config roots, what `machines.probe`
asks a machine. Params: `{}`, or `{"paths": ["~/sandboxes", "/srv/x"]}`.
Result: `[{"root": "/abs", "sandboxes": [{"name": "web"}, …], "error":
null}]`. Without `paths`: every distinct `config_dir` in `state.toml` plus
`~/.devsandboxes` if it exists, canonicalized (a root that's gone stays as
recorded), sorted; no disk scan. With `paths`: exactly those, in order,
`~` / `~/…` expanded with this host's home, then canonicalized. A root that
doesn't exist, isn't absolute or whose config doesn't load has `error` set
and `sandboxes: []`; the call itself fails only as `internal`, when
`state.toml` doesn't read (without `paths`). Without `paths`, to a remote
caller with a fleet root here (a fleet link's `API` stream, *Remote
callers*), that fleet root is listed last (`sandboxes: []`, `error:
null`) unless it's an own root already, and flagged `"fleet": true`
either way (absent on every other row): it is reached besides the own
roots, and it is where that caller's placement moves code to.

### `machines.*`

| method | params | result | errors |
|---|---|---|---|
| `machines.probe` | `{"target", "command"?}` (`command`: the machine's devsandbox as shell text, default `devsandbox`) | `{"version", "protocol", "roots": […]}`: the machine's devsandbox and its `roots.list` | `invalid` (bad target, empty `command`), `failed` (ssh, the hello or the protocol) |
| `machines.add` | `{"target", "name"?, "roots": […], "agent"?, "command"?}` (`agent` default `false`; `command` as `probe`'s, saved as the machine's) | `{"name", "roots", "version", "warning"}`: `roots` as saved (the machine's canonical paths, deduplicated), `warning` set when its semver differs from ours | `invalid` (bad target or name, a name taken, no `roots`, empty `command`, a root the machine refuses: each one's reason), `denied` (the name is a live inbound neighbor's), `failed` (as `probe`, or a remote too old to check roots), `internal` (`machines.toml` doesn't read or write) |
| `machines.list` | `{}` | `[{"name", "kind", "target", "roots", "command", "agent", "state", "error", "version", "capacity"}]`, the registry's by name, then the inbound neighbors up now by name; `kind` `ssh` \| `offered` \| `inbound`, `target` `null` for an offered one and an inbound one; `state`, `error` and `version` (from the last hello) its held link's, whatever its kind; an inbound one has `agent` off and `command` `null`, `roots` what its `roots.list` answered over the link (empty until it has); `capacity` the machine's [capacity](#capacity) from its last pushed snapshot (`null` before one carried it) | `internal` (`machines.toml` doesn't parse) |
| `machines.get` | `{"name", "agent"?}` (`agent`: the caller's `SSH_AUTH_SOCK`) | `{"name", "kind", "target", "command", "roots", "agent"}`, a registered machine's or an inbound one's: what an ssh command to it needs (no argv on the wire: build it from `target` and `command`; an offered or inbound one has none) | `not-found`, `invalid` (`agent` not absolute), `internal` (as `list`) |
| `machines.set` | `{"name", "agent"}` | `{"ok": true}`; saves the machine's `agent`, its link restarts if it changed | `not-found`, `denied` (an inbound machine: this host's agent never reaches it; its own entry for this host has the `agent` that counts), `invalid` (no `agent`), `internal` (`machines.toml` doesn't read or write) |
| `machines.rm` | `{"name"}` | `{"ok": true}`; its link stops and its rows go. Nothing on the machine changes | `not-found`, `denied` (an inbound machine: it goes with its link, `machine rm` on it), `internal` (`machines.toml` doesn't read or write) |

`probe` and `add` open a connection of their own (a 20 s hello), checking
ssh, the remote devsandbox and the protocol before anything is saved.
`add` checks every root with `roots.list {paths}` on the machine: a quoted
`~` is the machine's home. `name` defaults to the target's host
(`me@my-server.com` → `my-server.com`). `add`, `set` and `rm` reach the links at
once (`set` of `agent` keeps the live link: the next `AGENT` stream is
answered as it says). `agent` is listed (also in the federated
snapshot's `machines`) and decides whether the daemon answers the
machine's `AGENT` streams with its agent (*Transport*).

`get`'s `agent`, when given, is reported as the caller's agent, like a
verb's `env.SSH_AUTH_SOCK` (*Bridges*): the CLI passes it before running
a command on the machine (its calls and `PTY` streams over the machine's
link), so the agent the machine reaches meanwhile is that terminal's. Adding a machine means trusting it: its socket is as
powerful as `docker.sock`, and with `agent` on your agent is reachable
there.

### `fleet.offer`

Internal (`devsandbox fleet-link`'s; not in the npm typings): a fleet
worker's link to this host is up
([fleet.md](../architecture/daemon/fleet.md#hub-side)). Params:
`{"worker": "<machine name>", "version": "<its semver>", "root":
"<its fleet root>", "draining"?: <bool>}` (`root`: the worker's canonical
fleet root, a path on the worker; `draining`: the worker's lending was
`drain` when it dialed; logged). Result: `{"ok": true}`; a new name is
saved as an offered machine whose one root is `root` (its link starts,
dialed at the worker's `fleet-<worker>.sock`, and watches that root like
an ssh machine's), an offered one is kept, its root replaced (and its link
restarted) when it differs. Errors: `invalid` (a bad name, a missing
param, a `root` that isn't absolute), `denied` (this daemon isn't a
hub: `[serve] hub = true` in `daemon.config.toml`; or the name is an ssh
machine's or a live inbound neighbor's), `internal` (`machines.toml` doesn't read or write).

### `fleet.agent`

Internal (`devsandbox fleet-link`'s; not in the npm typings): the agent
socket the link's `AGENT` streams connect
([fleet.md](../architecture/daemon/fleet.md#agent)), asked every 2 s.
Params: `{"worker": "<machine name>"}`. Result: `{"agent": "<path>"}`,
this daemon's own agent (the agents clients reported first, then the
relayed agents' `agent-*.sock`, then its `SSH_AUTH_SOCK`, as a bridge
picks one) while `worker` is an offered machine with `agent` on;
`{"agent": null}` otherwise (no such machine, an ssh machine's name,
`agent` off, no live agent here). Errors: `invalid` (a bad name, no
`worker`), `internal` (`machines.toml` doesn't read). A fleet worker's
connection under `worker` (*Remote callers*) is `denied` it, as every
`fleet.*`.

### `lend.*`

This host as a fleet worker: `lend.toml` and the link to its hub
([fleet.md](../architecture/daemon/fleet.md#lending-lend)). The daemon is
`lend.toml`'s only writer, and a change applies at once. Both are denied
on a fleet hub's connection under `worker` (*Remote callers*).

| method | params | result | errors |
|---|---|---|---|
| `lend.get` | `{}` | `{"state", "hub", "name", "fleet_root", "link", "error"}` | `internal` (`lend.toml` doesn't parse), `failed` (no data dir) |
| `lend.set` | `{"state"?, "hub"?}`: each one given is changed | as `lend.get`, after the change | `invalid` (an unknown param or `state`, an empty or bad `hub`, lending without a `hub`, or no `name` in `member.toml` and a hostname that isn't a valid one), `failed` (the daemon is exiting, no data dir), `internal` (`lend.toml` doesn't read or write, the fleet root can't be created) |

- `state`: `on` (offer this host, take new work), `off` (no link) or
  `drain` (keep the link, so what's placed here stays reachable; take no
  new work: the hub's `run` is refused `denied`, `this worker is
  draining: no new work`, while `start`, `stop`, `rm` and `rebuild` of
  what's here go on).
- `hub`: the hub's ssh target, as `machines.add` takes one; `null` when
  none was set. Kept when lending goes `off`.
- `name`: the name this host offers itself under: `member.toml`'s
  ([`member.*`](#member)), else the hostname's first label; `null` when
  neither is a valid machine name.
- `fleet_root`: `<data>/devsandbox/fleet/<hub>/`, all the hub reaches
  (canonical while lending); `null` without a `hub`.
- `link`: `off`, `connecting` (the first attempt hasn't ended),
  `connected`, or `retrying` (`error` says why; the next attempt follows
  the backoff). `error` is also set while `off` when the file says lend
  but lending couldn't start.

`lend.set` restarts the link only for a new `hub` (the old ssh killed
first), `member.set` for a new `name`; `on` ↔ `drain` keeps it, and the
next dial offers the new state.

### `member.*`

This host as a fleet member: `member.toml`
([fleet.md](../architecture/daemon/fleet.md#member-member)). The daemon is
its only writer; `[allow]`, `[caps]` and `dev-builds` are edited by hand
and have no param. Local only: both are denied on a remote caller's
connection under either trust (*Remote callers*).

| method | params | result | errors |
|---|---|---|---|
| `member.get` | `{}` | `{"name", "trust", "dev_builds", "roots"}` | `internal` (`member.toml` doesn't parse), `failed` (no data dir) |
| `member.set` | `{"name"?, "trust"?, "roots_add"?, "roots_rm"?}`: each one given is changed | as `member.get`, after the change | `invalid` (an unknown param or `trust`, a bad `name`, a relative root), `failed` (no data dir), `internal` (`member.toml` doesn't read or write) |

- `name`: the name this host goes by in the fleet: the one set, else the
  hostname's first label; `null` when neither is a valid machine name.
  `member.set {name: ""}` clears it. A change restarts a lending link
  under the new name.
- `trust`: `worker` (the default: a remote caller reaches this host's
  own roots and its fleet root, placed work under the worker policy) or
  `full` (the whole API but `member.*`, `link.*` and `links.*`, no worker policy,
  the hub's dev builds taken)
  ([fleet.md](../architecture/daemon/fleet.md#trust)).
- `dev_builds`: `member.toml`'s `dev-builds`, as the file says.
- `roots`: config roots shown to the fleet besides those with instances
  here (absolute paths). `roots_add` adds each one not there yet,
  `roots_rm` drops each one named.

### `capacity.get`

This host's capacity report now, as its snapshots carry it (`Capacity`:
`cpus`, `mem_total`, `mem_available`, `disk_free`, `disk_total`,
`running`, `lending`, `caps`; [fleet.md](../architecture/daemon/fleet.md#capacity)),
built on demand: no watched root needed. Params `{}`. On every link
peer's connection: a host asks each machine before it places a `run`.

### The `machine` param

A request with `"machine": "<name>"` runs on that machine: the daemon drops
`machine` (and `machines`) from the params, forwards the rest, and answers
with the machine's result or error, code and `data` kept, under the
request's own `id`. `dir`, `root` and instance and container names are the
machine's. Quick calls go over the machine's link, one in flight per
machine, each with 15 s to answer:

`instances.list`, `instances.resolve`, `instances.cancel`,
`instances.procs`, `instances.signal`, `instances.rename`,
`instances.done`, `instances.buildLog`, `sessions.list`, `sessions.rm`,
`containers.logs`,
`containers.inspect`, `containers.list`, `containers.stats`, `sandboxes.list`, `sandboxes.get`, `services.get`,
and every verb (*Verbs on a machine*).

`forwards.add` with `machine` isn't forwarded: the daemon serves it, binding
the forward on this host (*Forwards*).

Any other method with `machine` is `invalid` ("can't take a machine
yet"), so a client never takes a local answer for a remote one. Errors
added by the forwarding: `not-found` (no such machine), `failed`
(`<machine>: unreachable: <why>`: the connect the call triggered failed,
answered without waiting out the backoff; or `<machine>: <why>`: the link
broke mid-call), `internal` (`<machine>: timed out`: the link is dropped
and reconnects; `<machine>: the link is stopping`; `<machine>: no
answer`). A method the machine lacks is its `unknown-method`.

### Federated calls

`instances.resolve {name, machines: true}` and `sandboxes.list {root,
machines: true}` ask this host and every machine (registered or inbound) in parallel,
each machine given 5 s over its link (connecting it if needed, so the link
is then up for the next 60 s):

- `instances.resolve`: the union, sorted. A machine's matches are
  qualified in `matches` (`my-server/web-2`), a local one isn't; `found`
  gives each one's unqualified `name`, `machine` and `root`. A machine
  answering `not-found` adds nothing; one that doesn't answer (or fails)
  adds nothing either, and if nothing matched anywhere the `not-found`
  message ends `; unreachable: <machine> (<why>), …`.
- `sandboxes.list`: this root's entries, then each machine's registered
  roots' in registry order, tagged `machine` and `root` (the remote root).
  A machine or root that doesn't answer adds nothing; read `machines.list`
  to say why.

With `machine` instead, only that machine is asked, as above.

### Federated snapshots

A watch with `machines: true` (`instances.watch`) gets every
`instances.snapshot` for its root merged: the local rows as they are, then
each machine's instances, sandboxes and services tagged `"machine":
"<name>"` (sandboxes and services also `root`, the remote root they were
read from; an instance keeps its own `root`). Each remote instance appears
once: from its own root's snapshot when that root is registered, else from
the machine's first root's. `sandbox_count` counts every machine's too;
remote `thread_children` aren't merged. Plus, top-level:

| field | |
|---|---|
| `machines` | every machine (registered, and the inbound neighbors up now), by name: `{"name", "kind", "target", "command", "state", "error", "version", "runtime", "agent", "capacity"}`; `kind` and `target` as `machines.list`'s, `command` the remote devsandbox (`null`: `devsandbox`), `runtime` its `runtime_name` once a snapshot came, `agent` the registry's, `capacity` its [capacity](#capacity) from its last push (`null` before one carried it) |

A machine's rows are last known: they stay while its link is down (in
memory only, so a fresh daemon has none until it connects), and a client
shows them as unreachable while its `state` isn't `connected`. A
reconnect's first remote push, which has no runtime pass yet, keeps the
rows' last status, cpu and mem. A machine's push or state change
re-publishes every federated watcher's root. A watch without `machines`
gets exactly what it got before machines.

### Verbs on a machine

A verb with `machine` (`root` then a path on the machine) goes over an
`API` stream of its own on the machine's link, opened for that request,
so a long build never holds up the link's quick calls. The request minus
`machine` is sent after the hello; the machine runs it with its daemon's
own env (the link's, *Remote callers*), so only `env.SSH_AUTH_SOCK`
matters here: an absolute path is reported to this daemon as an agent,
which the machine's `AGENT` streams are answered with. A machine whose
link is down fails it at once (`failed`, `<machine>: unreachable:
<why>`). The machine's `output` and `progress`
are relayed with `request` rewritten to the local id, and its answer
(`failed`'s `data`, `confirm`, `detached` included) passed through.

- **Hang-up**: the local client leaving closes the stream, which the remote
  daemon sees as its client hanging up: cancelled there unless `detach`.
- **Drain**: a local daemon exit (idle, handoff, `shutdown`) closes it the
  same way, so a non-`detach` remote verb is cancelled, unlike a local verb
  (no spool here to adopt it by). A `detach` verb runs on.
- **Errors**: the stream or its hello failing is `failed` with `data:
  {"exit": null, "instance": null}` and the reason (the link's, when it's
  down); the stream ending before the answer is `failed` (`connection
  lost; the verb may still be running there`).

A forwarded verb holds this daemon while its ssh runs (*Holders*).

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
| `instances.changed` | `instances` | `{}` | the running containers changed (the daemon re-lists on each container event, else every 2 s while a root is watched, 60 s while the runtime's event stream is up, 5 s without one or while a bridge is pending or failed), or `state.toml` did (checked every 0.5 s): a phase moved (a `run`/`rebuild` step, a failure), an instance was created, removed or renamed, marked done or undone |
| `instances.snapshot` | `instances` | `{"dir": "<abs>", "capacity"?, …}`: `instances.list`'s result plus the root and this host's [capacity](#capacity) | a root this connection watches (`instances.watch`) was collected: every poll pass (about every 2 s), changed or not, so its arrival says how fresh it is. Latest only per root |
| `forwards.changed` | `forwards` | `{}` | any forward's row changed (added, stopped, state, process, connections; checked every 0.5 s), in any config root |
| `forwards.status` | `forwards` | `{"line": "…"}` | a one-line status: a configured forward started (`forwarding 127.0.0.1:3000 -> api:3000`) or failed, a connection note (`3000: connection refused`). Latest only, like `inbox.shown` |
| `closing` | any | `{"reason": "handoff" \| "idle" \| "shutdown"}` | the daemon is exiting; reconnect (which starts a new one) |

Notifications coalesce: many changes before one is written are one line.
A client that doesn't read its socket for 5 s is disconnected.

## Daemon

### `shutdown`

Params: `{}`. Result: `{"ok": true}`. The daemon stops accepting and
removes `serve.sock` before it answers, then exits the way a handoff does
([role.md](../architecture/daemon/role.md#drain)): subscribers get `closing` with
`"reason": "shutdown"`, idle connections are closed, in-flight requests
finish (up to 5 s), bridges and forwards stop, the start lock goes last.
The next client that connects starts a new daemon. `devsandbox serve
install` sends it so the service manager's daemon can take over
([role.md](../architecture/daemon/role.md#boot-start-serve-install--uninstall)).

## Holders

A connected client keeps the daemon alive
([role.md](../architecture/daemon/role.md#holders-and-idle-exit));
hang up when done. So does every forward, ad-hoc or configured, whether or
not its client is still connected: stop ad-hoc ones with `forwards.rm`. A CLI command holding a relay (`bridges.ensure`) stays
connected for its whole run for that reason. Every verb holds it until
its subprocess is reaped, a detached one too, and one a previous daemon
left running until it exits. A verb forwarded to a machine holds it while
its ssh runs. A machine link doesn't: the clients that want it do (a
federated watcher, the connection waiting on a forwarded call).

## Relay: `devsandbox api --stdio`

For clients that can only spawn a process (editor extensions, Electron
apps) and shouldn't deal with socket paths or start races. Unix only, like
`serve`. `devsandbox api --stdio [--client <name>]`:

1. connects with a hello of its own (lazy start, version handoff: the
   relay is a devsandbox binary like any CLI command), as `--client`
   (default `api`; it shows in `serve.log`),
2. opens a **fresh** connection and relays it: stdin bytes to the socket,
   socket bytes to stdout, flushed per read. It parses nothing, so the
   relayed connection starts with no hello: the client sends its own as its
   first line, and checks `protocol` itself. stdout carries protocol lines
   only; diagnostics (a failed start) go to stderr.

Exit status:

| | |
|---|---|
| 0 | stdin hit EOF: the relay shuts its write half, so the daemon answers what's in flight and hangs up; waited for up to 5 s. Also when stdout is gone |
| 75 | the daemon closed the connection first (it exited: idle, handoff, killed); run the relay again, which starts a new one |
| 1 | it couldn't connect (the daemon didn't start within 5 s; see `serve.log`) |

The relay is a client like any other: it holds the daemon while it runs,
so hang up (close its stdin) when done.

```
$ devsandbox api --stdio
{"id":0,"method":"hello","params":{"version":"0.0.0","client":"api:my-ext"}}
{"id":0,"result":{"build":1767225600,"protocol":1,"version":"0.6.0"}}
```

A client that isn't a devsandbox binary can send `"version": "0.0.0"`: the
relay's own hello already handed off an older daemon.

`--sentinel` (hidden) is for `machine add`'s probe over ssh
(`machines.probe` / `.add`, *Machines*): once connected to the daemon,
the relay writes a newline and the sentinel line `devsandbox-api 1` to
stdout (`relay::write_sentinel`) before any of the daemon's bytes, so
the probe can skip whatever the remote login shell printed before it.
Nothing else changes: the bytes pass as they do without it. A link's
`API` streams carry no sentinel.

## The npm client

`npm/devsandboxes` (`connect()` in `index.js`, typed in `index.d.ts`)
spawns `devsandbox api --stdio --client npm:<name>`, sends the hello
(`client: "npm:<name>"`, version `0.0.0`) and rejects unless `protocol` is
`1`. `api.call(method, params)` resolves with `result` or rejects with a
`DevsandboxApiError` carrying the daemon's `code`; the client adds its own
codes: `closed` (the relay exited: every pending call rejects, `close` is
emitted with the exit code), `protocol`, and `unsupported` (Windows, no
daemon). Notifications are events (`api.on('inbox.changed', …)`, plus
`notification` for every one), after `api.subscribe(topics)`. Typed
namespaces: `api.inbox.*`, `api.instances.list(dir)`, `api.forwards.*`;
relative `dir`s are resolved against the Node process's cwd. No reconnect:
on `close` (75 after a `closing`), `connect()` again. The interfaces in
`index.d.ts` mirror the serde structs; `src/serve/dts.rs` fails the Rust
tests when a wire struct (or a notification's params) and its interface
disagree.


Limitations: [status.md](../status.md#daemon)
