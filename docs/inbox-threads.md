# Inbox threads: dispatchers that ask, users that answer

Status: implemented (steps 1-10). Where the code differs from the design, the
sections below say so; *Decisions* records what was settled before the build.
User reference: `automations-guide.md` (*Inbox threads*); protocol:
`automations.md` (*Inbox threads*). Companion plan for the PR babysitter:
`../.devsandboxes/dispatcher-threads-plan.md`.

## Why

The PR babysitter (see `automations.md` and `automations-guide.md`) works, but its users can't tell what it is doing or what it wants from them. What we saw on a real run (2026-10-01/02):

- **Noise.** 60 of 105 notifications in a day were one `dispatcher-capacity`
  warning re-sent every pass (5 min). They all used the same key, so the
  thread's single row kept switching between two PRs, and each one was a `warn`
  that could pop up.

- **Events, not state.** The Inbox is a log. A released hold, a finished run
  or a merged PR still leaves its warning in the list. Nothing says "this is
  resolved", so nobody can tell what is still waiting on them.

- **One subject, many threads.** A PR showed up as up to four threads
  (`pr-N-start`, `pr-N`, `pr-N-ci`, `pr-N-retired`), so the user had to piece
  its story together.

- **No next step.** "comments needs-human: <500-char summary>" doesn't say
  what to do, where the agent's draft is (`.dispatcher/pr-N-replies.md` inside
  the child), or how to clear the hold so the slot frees up. Both child slots
  stayed held while other PRs queued behind them.

- **Restarts clutter it.** Restarting the dispatcher re-sends notifications
  its in-memory dedup had suppressed. Restarting the dashboard seems to add
  more: most likely an older dashboard re-saving records dismissed in another
  (see *Decisions*, "One shared store").

- **No way to answer.** `notify` is one-way. The next use case, a triage agent
  asking "send this email or not?", needs the user to answer from the Inbox.

Some of this is the dispatcher's fault (re-sending on every pass, no next step
in the message), and its plan fixes that. The rest is missing from devsandbox:
there is no notion of an open item with a status, no actions, no replies, and
no "this instance is done". Fixing that only in the dispatcher would move the
clutter around, and every future dispatcher (triage, post-landing verify)
would have to solve it again.

## The split

**devsandbox is generic and knows nothing about PRs or emails.** It stores
threads, shows them, runs built-in instance actions instantly, and queues
everything else back to the instance that owns the thread.

**A dispatcher owns the meaning.** It decides which threads exist, their
status, which actions they offer, and what a reply or an action does. Its own
state stays the source of truth: threads are a projection of it, so losing the
Inbox loses nothing.

## Design

### Threads

A thread is identified by `(owner, key)`, where `owner` is the sending
instance's `instance_id` (stable across stop, restart and rebuild; never
reused), not its name. The sending instance owns it, and only instances that
declare `dispatcher` may send threads (see *Decisions*). Fields:

| field | meaning |
|---|---|
| `title`, `link` | what the thread is about ("#6900 feat/agent-run-cost-event", the PR URL) |
| `state` | `needs-you`, `active` or `done`. A fixed set: it drives the Inbox views and the unread badge |
| `status` | free text, the dispatcher's lifecycle stage: `running ci`, `review draft replies`, `merged`, `verified` |
| `child` | optional key of one of the sender's dispatcher children: the instance the thread is about |
| `message` | the current explanation: what happened, what is expected from the user |
| `actions` | buttons (below). Replaced on every put |
| `reply` | optional: allow free-text replies, with a placeholder |

The **timeline** (history shown under the thread) is kept by the host. It
records message changes, `state` and `status` changes, and every user action
and reply.

```bash
devsbd thread put < thread.json      # or: devsbd thread put --json '<json>'
```

```json
{
  "key": "pr-6900",
  "title": "#6900 feat/agent-run-cost-event",
  "link": "https://github.com/stackblitz/bolt/pull/6900",
  "state": "needs-you",
  "status": "review draft replies",
  "child": "pr-6900",
  "message": "4 Greptile comments reviewed, no code change needed. Draft replies are ready; nothing posted.",
  "actions": [
    { "id": "open-draft", "label": "Open draft", "host": { "vscode": { "path": ".dispatcher/pr-6900-replies.md", "line": 1 } } },
    { "id": "post", "label": "Post replies" },
    { "id": "done", "label": "Done", "done": true }
  ],
  "reply": { "placeholder": "Instructions for the next run" }
}
```

- **Idempotent.** A put that changes nothing is dropped by the host: no
  timeline entry, no unread mark, no popup. A restarted dispatcher can
  re-assert all its threads every pass for free, and that is the intended way
  to use it. This is the main fix for clutter.

- **Transport:** the existing durable outbox, like `notify`, so a put works
  with no dashboard open and is delivered later, in order.

- **`devsbd thread rm <key>`** drops a thread (e.g. on state loss cleanup),
  pending events included. As built, `done` and archived threads are
  *dropped* 14 days after their last change (a done thread still holding
  unacked events is kept until they're acked); past the per-instance cap of
  200 (records, timeline entries and pending events together), archived
  threads go first, then done ones without pending events, then the oldest
  single items. Events are never evicted by the cap; each thread keeps at
  most 100, oldest dropped.

- A put changing `message`, `state` or `status` adds one timeline entry each
  and marks the thread unread; a change to `title`, `link`, `child`,
  `actions` or `reply` is applied silently. The put always wins over the
  user: a thread the user marked done comes back if the dispatcher puts it
  as `needs-you` again, so a dispatcher must record the `done` event.

- **`notify` stays** for one-off alerts. Internally it can become a thread
  with no `state` and no actions, so both share one store and one UI.

### Actions

Two kinds, which can be combined on one button:

- **Host actions** run in the dashboard immediately, with no dispatcher round
  trip. This matters because the most common thing a user does on a thread is
  look at the work (open VS Code on the child at the right file). It must not
  wait up to a pass interval. v1 verbs are the ones the dashboard already
  has:

  | verb | args | same as |
  |---|---|---|
  | `vscode` | `path?`, `line?`, `col?` (relative to the instance workspace) | `o` (see *VS Code at a line*) |
  | `terminal` | | `t` |
  | `logs` | | `l` |
  | `forward` | `port` | `p` |
  | `open` | `url` (http/https) | `enter` on a link |
  | `rm` | | `devsandbox rm`, after a confirm prompt |

  The target is the thread's `child`, or the sender itself when there is no
  `child`. A host action can never target any other instance. Paths are
  validated to stay inside the workspace. There is no arbitrary command on
  the host, ever.

- **Dispatcher actions** (any action without `host`, or with `notify: true`)
  enqueue an event for the owning instance. That's how "Post replies",
  "Send / Skip", or "Retry" are wired: devsandbox doesn't know what they do.

- **`done: true`** is a built-in: it sets the thread to `done`, marks its
  `child` done (below), and enqueues one `done` event whose `action` is the
  button's id (not an `action` event too), so a dispatcher handles "done" in
  one place and still knows which button it was.

Host verbs are written `"host": { "<verb>": { …args } }`, `{}` for a verb
with no args, exactly one verb per action.

The thread pane also has fixed keys that need no declaration: the host verbs
on the thread's child (`o`, `t`, `l`, and `p` for the forward prompt; `o`
opens no file), `r` to reply (when `reply` is set), `d` to mark done (a
`done` event without `action`), `u` to reopen (the thread goes back to
`active`, a `reopen` event). `d` and `u` only act on a real transition.

### Events: pull, not push

```bash
devsbd events [--wait SECS]        # JSON lines, oldest first
devsbd events ack <id>...
```

```json
{"id": "e-1790900001-3f2a", "key": "pr-6900", "kind": "action", "action": "post", "at": "2026-10-02T12:00:01Z"}
{"id": "e-1790900042-77c1", "key": "pr-6900", "kind": "reply", "text": "Also rename the event to agent_run.cost", "at": "2026-10-02T12:00:42Z"}
{"id": "e-1790900050-0b9e", "key": "pr-6900", "kind": "done", "action": "done", "at": "2026-10-02T12:00:50Z"}
```

As built: ids are `e-<unix secs:010>-<4 hex>`; `kind` is
`action|reply|done|reopen`; `action` and `text` are left out when absent;
`at` is RFC 3339 UTC from the dashboard host's clock (the store keeps unix
seconds). `events ack` prints how many it dropped and ignores unknown ids.
One `events` answer stops at about half of the 1 MiB response cap; the rest
comes after an ack.

- **Held by the host** in `inbox.toml`, with the thread, until acked.
  Delivery is at least once: the dispatcher acks after it has saved its own
  state, so its handlers must be idempotent. With two dashboards open, the
  event id stops an event being handled twice.

- **A control op**, so it needs an open dashboard (exit 75 otherwise), like
  `ensure`. In practice that's fine: an event only exists because a user
  clicked in a dashboard.

- **`--wait`** long-polls, capped at 300 s like `run wait`. A dispatcher can
  replace its sleep between passes with `devsbd events --wait <interval>` and
  react to a click within seconds. It doesn't need a persistent connection
  for that.

- Events are scoped to the owning instance: an instance only sees events for
  its own threads.

Why pull: today messages only go from container to host (`notify`, control
ops). A pull queue reuses the control path, survives restarts on both sides,
and fits a dispatcher that already loops. Instant responses aren't needed yet.

**Later: a bidirectional connection.** The bridge is already a persistent
container↔host connection. A `devsbd events --follow` stream over it (or a
socket the dispatcher subscribes to) would give push delivery and allow live
interactions such as a dispatcher updating a thread while the user watches it.
Not needed now. The pull API stays the base, and a stream would just deliver
the same events sooner.

### Done instances

An instance can be marked **done**: kept as is (container, worktree, runs),
shown dimmed in `ps` and in the dashboard, and removed whenever the user wants.

- Set by a `done: true` action on a thread with a `child`, by `d` on an
  instance row, by `devsandbox done <instance>`, or by the dispatcher via
  `devsbd done <key>` (e.g. the PR merged). Cleared by `devsandbox undone`,
  `u`, or a `devsbd ensure` that reuses it.

- `devsbd ls` reports `"done": true`. A done child still counts toward
  `max-instances` (it's a real container), but a dispatcher may evict it
  without asking. Whether it does is the dispatcher's policy, not devsandbox's.

- This replaces "remove the child to release the hold" as the way a user says
  "I've looked at it, carry on".

### VS Code at a line

`devsandbox vscode <instance> [--goto PATH[:LINE[:COL]]]`, also used by the
`vscode` host action. `PATH` is relative to the instance's workspace folder.

Today `launch` (`src/commands/vscode.rs`) runs
`code --file-uri|--folder-uri vscode-remote://attached-container+<hex>/<path>`.
A deep link is out: VS Code's URL handler only opens folders for
`vscode://vscode-remote/` URIs.

**Use the remote CLI inside the container**, the `code` you get in a VS Code
integrated terminal. It's a script shipped with the VS Code server
(`~/.vscode-server/bin/<commit>/bin/remote-cli/code`) that talks to the
attached window over a unix socket named by `VSCODE_IPC_HOOK_CLI`
(`/tmp/vscode-ipc-<uuid>.sock`), and it supports `-g file:line:col`. Checked
in a devsandbox instance (VS Code 1.139.1, server `04c0d99f`): with a clean
environment containing only that variable, i.e. what an exec from the host would give,

```bash
env -i PATH=/usr/bin:/bin VSCODE_IPC_HOOK_CLI=/tmp/vscode-ipc-….sock \
  ~/.vscode-server/bin/<commit>/bin/remote-cli/code -g <abs path>:46:3
```

exits 0 and opens the file at the line in the attached window (confirmed).

**Which socket.** The `/tmp/vscode-ipc-*.sock` files are not equivalent. In
the instance above, all six accepted a connection:

| owner process | sockets | what it is |
|---|---|---|
| `out/server-main.js` | 4 | one per integrated terminal (the terminal's own `VSCODE_IPC_HOOK_CLI`) |
| `bootstrap-fork --type=agentHost` | 1 | the agent host |
| `bootstrap-fork --type=extensionHost` | 1 | the window: one extension host per connected window |

So "newest live socket" is wrong: it would usually pick a terminal's. The rule
is **the socket owned by an `extensionHost` process**. Find it by matching
the socket's inode in `/proc/net/unix` to `/proc/<pid>/fd`, then check the
owner's cmdline for `--type=extensionHost`.

**Liveness isn't attachment.** The extension host runs with
`VSCODE_RECONNECTION_GRACE_TIME=10800000`: it outlives a closed window by 3 h,
socket included. A connectable socket doesn't prove a window is there.

`launch` with `--goto` (as built: no snapshot step; the host opens the
window, then execs `devsbd vscode-goto <abs path>[:line[:col]] --wait 30` as
root, and the helper polls for an extension host; the steps below otherwise
hold):

1. **Snapshot** the extension-host sockets in the container (step 3's
   helper). Not built: polling after step 2 covers both cases.

2. **Open or focus the window from the host, always**, as `launch` does today
   (`code --file-uri <workspace file>`). VS Code focuses an existing window
   for the same workspace instead of opening a second one. That covers both
   "no window" and "window closed but its extension host is still in grace".

3. **Pick the extension host:**
   - The snapshot is empty: poll for one to appear, up to 30 s (the window
     connects and may install its server on first attach). On timeout, report
     `opened VS Code; --goto dropped (window not ready)`.
   - One or more: take the most recently started process (`/proc/<pid>/stat`
     start time), i.e. the last window opened or reloaded. Several windows on
     one instance is rare (e.g. the workspace plus a subfolder); the newest
     one wins, and that rule is documented.

4. **Run the CLI from that process's own server**, which settles the server
   version and the editor variant with no probing. Its cmdline starts with
   `<server dir>/bin/<commit>/node` (`.vscode-server`,
   `.vscode-server-insiders`, `.cursor-server`, … all have this shape), so
   the CLI is the single script in the sibling `bin/remote-cli/` (`code`,
   `code-insiders`, `cursor`). Exec it as the extension host's owner, with
   `VSCODE_IPC_HOOK_CLI=<socket>`, `-g <workspace>/<path>:<line>:<col>`.

Steps 1, 3 and 4 run inside the container as one helper op
(`devsbd vscode-goto <ABS_PATH>[:LINE[:COL]] [--wait SECS]`, `--wait` at most
60, internal, called by the host through exec). The `/proc` scan stays in Rust
next to the rest of `devsbd`, not in a shell snippet. It prints
`socket=… pid=… cli=…` and exits 3 when no window appeared.

**Root can't see the socket, so the helper re-execs.** docker drops
`CAP_SYS_PTRACE`, so root in the container can't `readlink` another user's
`/proc/<pid>/fd` entries. The helper finds extension hosts from
world-readable `cmdline`/`stat`/`status`; when the newest one's fds are off
limits, it re-runs `vscode-goto` as that process's uid/gid with the remaining
wait, and the owner reads its own fds and runs the CLI. Run as a non-root user
other than the owner, it refuses.

This doesn't depend on the host `code` CLI or the authority format, so it's
the same on docker, podman and Apple `container`. Build-time test cases:
- the window already open;
- no window at all;
- the window closed less than 3 h ago (grace);
- two windows on one instance;
- Insiders or Cursor, if available.

Fallback when no extension host or remote CLI is found: the window still
opens, and the status line says the line was dropped.

### Inbox UI

Gmail-like rather than a log:

- **List:** one row per thread, sorted by last change. Columns: state marker,
  sender instance, title, status chip, age. Views: **Needs you** (default),
  **Active**, **Done**, **All** (one-off `notify` records included).

- **Badge:** the title bar count and the yellow `✉N` on instance rows count
  `needs-you` threads, not unread records. Noise can no longer inflate it.

- **Thread pane:** title, link, status, the child with its run state, the
  `message`, the timeline, the count of events the owner hasn't pulled yet,
  then numbered actions (`1`–`9`) and a reply box (`r`). A thread is read
  when opened, and a thread that changes while open marks itself read.
  Plain notify records are also marked read when the user leaves the Inbox
  from **Needs you** or **All**, where they were on screen: they have no
  state to resolve them.

- Desktop popups only fire when a thread enters `needs-you`, or for `notify`
  records as today.

## Out of scope / later

- The bidirectional stream (above).
- **Shared threads across users.** Assumed: each user runs their own
  devsandbox and dispatcher, so the host-local store is enough. Several people
  acting on one dispatcher's threads would need a store off the host.
- Declarative dispatchers (see `automations.md`, *Future*).

## Decisions

Settled before implementation (2026-10-02); they override the design above
where the two disagree.

- **One shared store, any dashboard.** Today each dashboard keeps its own
  in-memory inbox, loads `inbox.toml` once and overwrites it whenever it
  changes (`src/tui/mod.rs:197`, `:364`, `:610`): the last writer wins. And
  the daemon routes notify/control only to the **newest** attached bridge
  (`devsbd/src/daemon.rs:120`). So a click in an older dashboard would create
  an event the newer one (serving `devsbd events`) never sees. `inbox.toml`
  becomes a host store under a lock, read-modify-write on every change (std
  `File::lock` on a sibling `inbox.lock`; the write stays tmp + rename), that
  every dashboard and CLI (`rm`) goes through. Dashboards reload it when its
  mtime changes. Likely the main cause of the "restarts add clutter" bug: an
  older dashboard re-saving records dismissed in another.

- **Apply, then ack.** The bridge writes a record into the store *before*
  replying `ok` (today it replies first, `src/devsbd/bridge.rs:317`, so a
  dashboard dying between the two loses the record). A store failure replies
  nothing, and the daemon retries.

- **Threads belong to the owner's `instance_id`**, and only `dispatcher`
  instances may send them (others get an `error` record: "thread put denied").
  A stopped, restarted or rebuilt dispatcher keeps its threads, and can read
  them back with `devsbd thread ls` (e.g. after losing its own state). No
  other instance can read or change them. `devsandbox rm` of the owner
  **archives** its threads (instance ids are never reused): they're read-only,
  shown only in **All**, and dropped by retention.

- **Helper: syntax check only.** `devsbd thread put` checks JSON syntax with
  a small std-only validator (the helper has no dependencies on purpose) and
  pulls out `key`. Bad syntax or a missing `key` exits 2. The record carries
  the JSON opaque, and the host checks the schema with serde. A schema reject
  becomes an `error` notify record from that instance in the Inbox, so the
  author sees it.

- **The outbox coalesces puts:** queuing a `thread put|rm` for a key deletes
  any older pending `put|rm` for the same key. Puts carry full state, so only
  the newest matters, and a dispatcher that re-asserts every pass with no
  dashboard open doesn't pile up files.

- **Thread pane is a focused view.** `enter` on a thread opens it and `esc`
  closes it. While it's open it shadows the dashboard keys (like the terminal
  and modals do: `src/tui/app/mod.rs:317`): `1`–`9` run actions (the global
  `1`–`4` tab keys are shadowed), `o`/`t`/`l`/`p` target the thread's child,
  `r` replies, `d` marks done, `u` reopens, and `enter` opens the link. On
  the list, `d` marks a thread done, while plain notify records keep
  `d` = dismiss and `D` = clear (notify records only). `v` cycles the views.

- **`rm` host action** reuses the `:rm` path (`PromptAction::Rm`, suspended,
  CLI confirm). No new confirm widget.

- **Needs-you view and badge** also count unread plain notify records (my
  proposal, not in the design above). Otherwise a non-dispatcher's `notify`
  would only show up under **All**.

- **Helper self-heal.** A running instance only gets a new `devsbd` on
  `run`/`start`/`port` (`src/commands/start.rs:54`), never from an open
  dashboard. So a running dispatcher wouldn't get `thread`/`events` until
  `devsandbox start`. The dashboard now reinstalls a stale helper when it
  (re)spawns a bridge.

- **Done flag.** Shown in `status`/TUI/npm (`snapshot::InstanceRow`) and in
  `ps` (new state join: `ps` is runtime-only today). `u` on a thread clears
  its child's done flag. `start`/autostart of a done instance leaves it done.

## Implementation steps

One step = one commit. Checks after every step: `cargo test --workspace`.
Every new behavior gets unit tests in the touched file's `#[cfg(test)]`
module, in the style of its neighbors. Container paths use
`#[test_utils::docker_test(helper)]`. No step touches the dispatcher repo
(`../.devsandboxes/dispatcher`): its plan follows this one.

### Step 1: helper self-heal in the dashboard

- `Bridges::reconcile` (`src/devsbd/bridge.rs:575`): before (re)spawning a
  bridge for an instance with a helper, when the previous bridge reported
  `is_mismatch()` or this is the first spawn this session, run
  `crate::devsbd::ensure_recorded(key, info, true)` (quiet; a no-op hash check
  when current, `src/devsbd.rs:309`). It runs on the bridge worker thread,
  never the UI thread.
- Test: decision factored as a pure fn (when to reinstall), unit-tested next
  to `retry_decision`.
- As built: the check runs before *every* (re)spawn, not only after a
  mismatch, since a stale helper with the same protocol `VERSION` bridges
  fine and only lacks verbs. Spawns are rare, so it's one extra `exec` per
  spawn.

### Step 2: shared inbox store (format v2), no UI change

- New host module `src/inbox/` (`mod.rs` model, `store.rs` file I/O), out of
  `src/tui/` because the bridge, `dispatch` and `rm` use it too. Move the
  persistence half of `src/tui/app/inbox.rs` (`SavedInbox`… at `:296`,
  `to_toml`/`from_toml` at `:240`/`:268`, the cap at `:23`) there. The TUI
  keeps only view state (selection, folds).
- Schema `version = 2`: `[[thread]]` with `owner` (instance_id),
  `owner_name` (last known name, for display and archive), `key?`,
  `kind = notify|thread`, `archived`, `unread`, thread fields
  (title/link/state/status/child/message/actions/reply, unused by
  notify), `[[thread.entry]]` timeline (`seq`, `at`, `kind` = note | message
  | state | status | action | reply | done | reopen, payload), and
  `[[thread.event]]` (pending events, step 6). v1 files migrate on load: each
  thread's `instance` is resolved to `instance_id` through `State`, and a
  name that doesn't resolve makes the thread archived. Notes become `note`
  entries.
- `store::update(|inbox| …)`: lock, load, mutate, save if changed, unlock.
  `store::load()` for readers. Every write bumps the file's mtime.
- Wiring: the notify sink (`Bridges::spawn_worker`, `bridge.rs:619`) applies
  the record through `store::update` in `handle_notify` **before** writing
  `REPLY_OK` (`bridge.rs:317`), then pokes the TUI (the existing mpsc
  carries "reload" instead of the record). The TUI loads at startup, reloads
  when the mtime changes (checked each tick, cheap), and sends `d`/`D`/
  mark-read through `store::update`. It drops `save_inbox`/`take_dirty`
  (`src/tui/mod.rs:364`, `:610`).
- Behavior is unchanged otherwise (same rows, same keys). Tests: v1→v2
  migration, concurrent `update` from two threads loses nothing, apply-then-ack
  ordering (fake sink failure → no `ok`).

### Step 3: `devsbd thread put|rm` transport + host apply

- `src/devsbd/notify.rs`: optional `kind thread-put|thread-rm` (absent =
  plain notify, so the format stays compatible) and `body <escaped JSON>`
  (put) / `key` (rm). Update the module doc's format block. Both crates'
  fixture test cover it.
- Helper: `devsbd/src/json.rs`, a std-only JSON syntax validator + top-level
  string field extractor (for `key`), with unit tests (nested values, escapes,
  unicode, trailing garbage, depth cap). `devsbd/src/outbox.rs` gets the
  `thread put [--json '<json>' | stdin]` / `thread rm <key>` verbs and the
  same-key coalescing (scan pending files for `kind thread-*` + same `key`,
  delete them, then queue). Wire into `devsbd/src/main.rs:43` usage + match.
- Host: `src/inbox/thread.rs`, the serde schema (`deny_unknown_fields`).
  Rules: `state` must be one of the three; at most 9 actions; action `id`s
  unique and `[a-z0-9-]{1,40}`; host verbs from the fixed table; `vscode.path`
  relative with no `..` (normalized, must stay inside the workspace); `open.url`
  passes `is_url` (`src/tui/app/inbox.rs:329`); `forward.port` is 1–65535;
  `child` passes `dispatch::valid_key` (`src/commands/dispatch.rs:432`); key
  as `valid_key`. Authorization: `dispatch::declares_dispatcher(owner)`
  (`dispatch.rs:144`).
- Apply (pure, in `src/inbox/mod.rs`): a put that changes nothing is a no-op
  (no entry, no unread, no mtime bump). Otherwise one timeline entry per
  changed field (message/state/status), `unread` set, and `needs_popup`
  returned when `state` *enters* `needs-you`. `rm` deletes the thread. A
  reject becomes an `error` notify record from that owner.
- Popups (`desktop.rs`, called from the sink): threads only on the
  needs-you transition; notify records as today.
- Archive: `devsandbox rm` (`src/commands/rm.rs`) marks the removed
  instance's threads (notify ones too) `archived` through `store::update`,
  after the state entry is gone. Retention drops `done`/archived threads
  14 days after their last change; the per-owner cap evicts archived, then
  done, then the oldest.
- Tests: apply idempotence and timeline, every validation rule, the deny path,
  coalescing, and a docker helper test: `thread put` with no host, then
  attach, and exactly one record arrives.

### Step 4: Inbox UI v2: list, views, focused thread pane (read-only)

- `src/tui/app/inbox.rs` + `src/tui/ui.rs` (`draw_inbox` `:538`, `inbox_row`
  `:604`, `draw_inbox_detail` `:674`): one row per thread sorted by last
  change. Columns: state marker, owner name, title (notify: message's first
  line), status chip, age (`b25563f`'s formatter). Views Needs you (default)
  / Active / Done / All (archived only in All), cycled with `v`, current view
  in the tab title. Drop per-instance grouping and inline history folding:
  the timeline replaces them. Notify-record history shows in the pane.
- Badges: tab title and the `✉N` on instance rows (`ui.rs:963`, `:1079`)
  become needs-you threads + unread notify records per owner (mapping owner
  id to row name through the snapshot).
- Focused pane: `enter` opens it, `esc` closes it. It shows title, link,
  status, child (resolved to an instance + its run state; expose a
  `dispatch::find_child`-based resolver, `dispatch.rs:547`, as a pub fn),
  message, timeline, numbered actions (rendered, not runnable yet). It marks
  itself read when the thread changes while open. Key shadowing: a new tier in
  `App::on_key` after the terminal (`src/tui/app/mod.rs:329`). `d`/`D` per
  *Decisions*. Update `?` help.
- Tests: view filtering, sort, badge counts, pane key shadowing (`1` doesn't
  switch tabs while the pane is open), read-on-change.

### Step 5: host actions

- New `src/tui/app/thread_actions.rs`: an action with `host` resolves its
  target (the child, else the owner; never another instance; refused with a
  status line if the child isn't found) and maps to the existing plumbing:
  `vscode` → `PromptAction::Code` (`actions.rs:34`; path/line dropped until
  step 9), `terminal` → `open_terminal` for that instance (generalize it to
  take a name), `logs` → `open_logs` (same), `forward` → `pending_port`,
  `open` → `pending_open`, `rm` → `PromptAction::Rm` (suspended CLI confirm,
  `src/tui/mod.rs:529`). Fixed pane keys `o`/`t`/`l`/`p` use the same
  resolver. Actions without `host` are shown greyed ("needs events", step 6).
- Tests: target resolution (child, owner, foreign child refused), each verb's
  pending effect.

### Step 6: events, replies, `devsbd events` / `thread ls`

- Store: `[[thread.event]]` (`id` `e-<secs>-<4hex>`, `kind` action | reply |
  done | reopen, `action?`, `text?`, `at`). The pane enqueues through
  `store::update`: dispatcher actions (no `host`, or `notify: true`), `r`
  reply (single-line input reusing `Prompt`'s edit primitives,
  `src/tui/prompt.rs`; placeholder from `reply.placeholder`), `d`/`done: true`
  (thread → `done`, event; the child's done flag comes in step 7), `u`
  (reopen event). Every one also adds a timeline entry.
- Control ops in `src/devsbd/control.rs` (`Op`, `ALL` `:80`, `as_str`, a new
  repeatable `ack <id>` field): `events` (`timeout` ≤ `dispatch::MAX_WAIT`),
  `events-ack`, `thread-ls`. Handled in `dispatch::handle_with`
  (`dispatch.rs:177`) with `check_fields` rows (`:306`): only the owner's
  events/threads, keyed by `instance_id`. `events` long-polls on the handler
  thread (store mtime checked every 500 ms) and doesn't take the state lock
  (`bridge.rs:221`; not in `writes_state`). Body: JSON lines, oldest first.
- Helper `devsbd/src/ctl.rs`: `events [--wait SECS]`, `events ack <id>...`,
  `thread ls` (usage `:117`/`:168` tables, `main.rs` verbs).
- Tests: codec round trip, authorization (another dispatcher sees nothing),
  ack idempotence, wait wakes on an enqueue from another thread, reply/action
  enqueue from the pane.

### Step 7: done instances

- `src/state.rs` `Instance` (`:10`): `done: Option<u64>` (since, unix secs;
  `#[serde(default, skip_serializing_if = "Option::is_none")]`). Update the
  literal fixtures.
- CLI: `devsandbox done|undone <instance>` (`src/main.rs:31`, modelled on
  `Vscode`; `resolve_instance`), new `src/commands/done.rs`.
  `snapshot::InstanceRow` (`src/snapshot.rs:53`, `:629`) `done`; `ps` joins
  state for a DONE marker in table and JSON; npm `index.js`/`index.d.ts`
  (`done()`/`undone()` like `stop`, `InstanceRow.done`).
- Dispatch: `Op::Done` (`devsbd done <key>`, in `writes_state`), `ls` rows
  `"done": true` (`dispatch.rs:586`), `Ensure` reuse clears it
  (`dispatch.rs:211`, next to `replaced_env`). It goes through a new
  `Executor` method so tests stay fake.
- TUI: done rows dimmed (`ui.rs:1058`, like `status_style` `:1149`). On
  instance rows, `d` marks done and `u` clears it (background `OpDone`
  pattern, `src/tui/mod.rs:687`). The thread `done`/`d` sets the child's flag
  and `u` clears it (*Decisions*).
- Not changed: `start`/autostart keep the flag. A done child still counts
  toward `max-instances`.

### Step 8: `devsbd vscode-goto` (helper, local verb)

- `devsbd/src/vscode.rs`, a local inline verb (`main.rs:43`, no host round
  trip). Pure parsers with fixture tests (model: `runs.rs:626`
  `parse_stat`): `/proc/net/unix` listening sockets matching
  `/tmp/vscode-ipc-*.sock` → inodes; `/proc/<pid>/fd` → owner pid; cmdline
  has `--type=extensionHost`; start time from `/proc/<pid>/stat`; server dir
  from cmdline argv0 `<dir>/bin/<commit>/node` → the single script in
  `bin/<commit>/bin/remote-cli/`. Pick the newest; with `--wait SECS`, poll
  for one to appear. Exec the CLI as the owner's uid/gid (`/proc/<pid>/status`
  `Uid:`/`Gid:`, `CommandExt::uid/gid` like `boot.rs:64`) with a clean env
  (`PATH`, `HOME` from `/etc/passwd`, `VSCODE_IPC_HOOK_CLI`) and
  `-g <abs>:<line>:<col>`. Print `socket=… pid=…`. Exit 3 when none is found.
- Docker test: a fake extension host (a process with that cmdline listening
  on a `/tmp/vscode-ipc-x.sock`) and a fake remote-cli script that records
  its argv/env, run with a non-root owner.

### Step 9: `devsandbox vscode --goto` + host action line

- `src/commands/vscode.rs:23` `launch` gets `goto: Option<Goto>` (parsed
  `PATH[:LINE[:COL]]`, relative and normalized inside the workspace). Flow
  per *VS Code at a line*: open/focus from the host as today, then exec
  `devsbd vscode-goto … --wait 30` as root in the container. Status: `opened
  VS Code at <path>:<line>`, or `…; --goto dropped (<reason>)`.
- CLI flag `--goto` (`src/main.rs:162`). TUI: `PromptAction::Code` gains the
  goto and runs in the background (`OpDone`, `src/tui/mod.rs:687`) instead of
  on the UI thread (`:283`). The thread `vscode` host action passes
  path/line/col.
- Tests: goto parsing/validation, status strings; the docker test reuses
  step 8's fakes.

### Step 10: docs

- `docs/automations-guide.md` (`thread put|rm|ls`, `events`, `done`, thread
  JSON reference), `docs/automations.md` (protocol: record kinds, control
  ops), `docs/sandbox-helper.md` (verbs, `vscode-goto`), `docs/tui.md`
  (views, pane keys, done rows), the `CHANGELOG.md` Unreleased section. Mark
  this doc's status as implemented.

## Notes for the dispatcher plan

- Its "needs an instance restart" for a missing verb is really "needs
  `devsandbox start`"; after step 1, an open dashboard is enough.
- Thread puts are dropped unless the sender declares `dispatcher`.

## Inbox layout v2 (feedback after the first build)

Feedback on the step 4 UI: the list + full-screen pane layout is clumsy, rows
read like an old-school table, and `v` is a poor view switcher. Agreed shape:

```
 ‹ Needs you 3 │ Active 5 │ Done │ All ›
╭─ Inbox ──────────────────╮╭─ #6900 feat/agent-run-cost ──────────────╮
│▌#6900 feat/agent-run…  2m││ review draft replies · pr-6900 (running) │
│▌● needs you   bab-disp   ││ 4 Greptile comments reviewed…             │
│                          ││ ── timeline ──                            │
│ #7414 fix/login     1h   ││ 12:00 state → needs-you                   │
│ ○ running ci  bab-disp   ││ [1] Open draft  [2] Post replies  [3] Done│
│                          ││╭─────────────────────────────────────────╮│
│ ci failed on main   3d   │││ Instructions for the next run…          ││
│ ▲ warn        builder    ││╰─────────────────────────────────────────╯│
╰──────────────────────────╯╰───────────────────────────────────────────╯
```

Decisions:
- **Side by side:** the list on the left, the selected thread on the right,
  always shown (no more open/close pane). Split 40:60, and the divider can be
  dragged with the mouse like the config modal's (`view.rs` `divider_pct`/
  `col_near`, `app/mod.rs:437`). The split isn't saved. With terminals
  open, the bottom terminal panel stays as it is on other tabs
  (`ui.rs` `content_areas`), and the split uses the area above it.
- **Two focus zones, plus the input.** The **list** has focus by default:
  `↑`/`↓` select, `←`/`→` switch views (replacing `v`), `d`/`u`/`o`/`t`/`l`/`p`
  act on the selected thread, and `1`–`4` keep switching tabs. `enter` moves
  focus to the **thread**: `1`–`9` run actions, `↑`/`↓`/PgUp/PgDn scroll, and
  `enter` opens the link. `r` or `i` (from either zone) focus the **input**.
  `esc` steps back one zone: input → thread → list. The focused zone's border
  is highlighted.
- **Input at the bottom of the thread pane**, replacing the bottom-bar reply
  box (`ui.rs:41`, `draw_reply` `:1462`). It's shown only when the thread
  sets `reply` (placeholder dim inside it). Other threads get a one-line dim
  hint in its place, and notifications get nothing. `enter` sends (the same
  `Op::Reply`), and the box stays focused and empty for the next message.
- **Cards instead of table rows:** two lines plus a blank spacer, no border.
  Line 1: title (bold while unread) left, age right. Line 2: the state or
  level chip left (`● needs you`, `○ <status or active>`, `✓ done`, `▲ warn`,
  `✖ error`, `· info`), the sender right. When selected: an accent bar `▌` in
  the first column and a subtle background tint over both lines. Archived
  cards are dimmed. The view switcher is a one-line strip above the list,
  with `‹`/`›` and the current view highlighted.

### Step 11: side-by-side layout, focus zones, arrows, input in the pane

- `src/tui/app/inbox.rs`: replace `InboxView::open` (open/esc pane) with a
  focus enum `List | Thread | Input`. The right pane follows the selection,
  and keeps its scroll per selected thread id, reset on change. Re-target
  the pane key tier in `App::on_key` (`app/mod.rs`, after the terminal
  check) so it applies only when focus is `Thread`/`Input`. List-zone keys
  go in the normal Inbox arms: `←`/`→` views (remove `v`), `d`/`u`/
  `o`/`t`/`l`/`p` on the selected thread (reusing `thread_actions.rs`), `enter`
  → Thread focus, `r`/`i` → Input when `reply` is set (hint otherwise).
  Leaving the Inbox tab resets focus to List. "Mark read when opened" becomes
  "when selected", and also "when it changes while selected".
- `ReplyBox`'s submit keeps focus in the input and clears it. Its edit keys
  stay as they are.
- `src/tui/ui.rs`: `draw_inbox` lays out `[list | thread]` horizontally in
  the top area (terminal panel unchanged). The thread pane is a vertical
  `[content | input(3 rows) or hint(1 row)]`. Remove the bottom-bar reply
  drawing and `draw_inbox_detail`. Focus-highlighted borders.
- Footer hints and `?` help per zone.
- Tests: rewrite the pane tests for the zones (`enter`/`esc` steps; `1` in
  List switches tabs, in Thread runs an action; `←`/`→` views; `r` focus
  and multi-send; read-on-select).

### Step 12: card rendering

- `src/tui/ui.rs`: `draw_inbox_list` renders cards (a `List`, or manual
  `Paragraph`s with offset handling, so the selection stays visible: 3 rows
  per card). Age right-aligned on line 1, sender right-aligned on line 2,
  truncating the left text with `…` to fit. Selection: `▌` accent plus a
  background tint (pick a subtle `Color::Rgb`/indexed value that also reads
  on light terminals, falling back to `REVERSED`-free styling). The view strip
  sits above the list. Keep the pure layout bits (truncate-to-fit, chip text)
  as tested fns.

### Step 13: draggable divider

- Mouse press near the list/thread divider then drag sets `split_pct`
  (clamped like `clamp_split`). It lives on `InboxView`, defaults to 40, and
  isn't persisted. It reuses `divider_pct`/`col_near`, and the hit-test uses
  the same area function as `draw_inbox` so they can't drift. Tests mirror
  the config modal's drag tests.

### Step 14: docs

- `docs/tui.md` Inbox section, `site/src/content/docs/dashboard.mdx` key
  tables, the CHANGELOG `## Unreleased` "The Inbox shows…" entry (`←`/`→`,
  side-by-side, input in the pane).
