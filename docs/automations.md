# Automations: sandboxes that start themselves and spawn their own work

## Goal

Let a user declare sandboxes that come up on their own after a boot and do work unattended, with devsandbox providing only the infrastructure. Motivating use cases:

1. **PR babysit** — track open PRs; when one needs it (conflicts, stale base, bot comments), start an agent that fixes it. No agent runs while nothing needs doing.

2. **Post-landing verify** — once a tracked PR lands, check logs/spans for the feature and open a PR with fixes for anything found.

3. **Triage** — read emails/Slack, build a "things to watch" list, serve it as a UI on a port.

Two shapes fall out:

- **Resident** (3): one long-lived instance. Almost a normal sandbox plus autostart.

- **Fleet** (1, 2): a varying number of instances, one per *item* (or a pool of workers over items), created, woken, and retired over time. 2 is 1's item moving to its next stage, on the same instance.

devsandbox does **not** grow a workflow DSL. The logic (what to watch, when to act) lives in a user-written **dispatcher script** running in a sandbox; devsandbox gives it a small, capability-scoped control API. A declarative layer may emerge later from real dispatchers (_Future_).

Non-goals: launchd/systemd units or any always-on host daemon; credential isolation (see `docs/cli-proxy.md`, orthogonal); webhooks/inbound events.

## Building blocks

| block | who can use it | purpose |
|---|---|---|
| `autostart` | any sandbox | come up after a boot |
| `devsbd notify` | every sandbox, no opt-in | tell the human something (TUI + desktop) |
| `dispatcher` control API | only sandboxes declaring `dispatcher` | ensure/stop/rm/done/exec child instances |
| runs | dispatchers (via `exec --detach`) | tracked, logged agent invocations |
| Inbox threads + events | only sandboxes declaring `inbox = true` (a dispatcher wanting threads declares both) | open items the user can act on; their clicks and replies come back as events (_Inbox threads_, `docs/inbox-threads.md`) |

## `autostart`

```toml
[sandbox.triage]
autostart = true        # devsandbox-driven
# autostart = "runtime" # container runtime restarts it on boot, when supported
```

### `true` — devsandbox-driven, once per boot

The first TUI load, or the user's own `devsandbox run` / `start` (after it completes), after a boot starts every `autostart` sandbox's stopped instances (as `devsandbox start`: devsbd install, `postStartCommand`, bridges). A sandbox with no instance yet gets one (`run` semantics). Other CLI commands never trigger it.

"Once per boot" is keyed on the host boot id, recorded per config root in `state.toml` (`autostart_boot`: project id → boot id):

- Linux: `/proc/sys/kernel/random/boot_id`
- macOS: `sysctl kern.boottime`

Later invocations compare and skip, so the check is cheap. An instance the user stopped after autostart stays stopped until the next boot.

### `"runtime"` — the runtime restarts it

Created with `--restart unless-stopped` where the backend supports it:

- docker: yes (Docker Desktop / dockerd starts on login/boot).

- podman: only if the user has enabled `podman-restart.service`; devsandbox never installs units. Warns at `run`.

- Apple `container`: tier 2 — falls back to `true` with a warning until it has a restart policy.

The runtime only restarts the *container*. Nothing devsandbox runs via `exec` comes back: not `postStartCommand` (`src/commands/start.rs:61`), not the devsbd daemon (`src/devsbd.rs:108`), not host-side bridges or forwards. So `"runtime"` also needs an in-container boot path:

- The container command runs `devsbd boot` in the background when the binary is present, before the keep-alive (`sh -c '[ -x …/devsbd ] && …/devsbd boot & exec sleep infinity'`); it starts the daemon.

- `devsbd boot` then runs `postStartCommand` on every container start (as `remoteUser`, `workspaceFolder`, `remoteEnv` — same as `start`), from a boot file the host writes at `/run/devsandbox/boot`, output appended to `/run/devsandbox/boot.log`. The host `start` never runs it for these instances (`run` still does at create, before the helper exists).

This is what makes a dispatcher's loop survive a reboot with no devsandbox process around. Anything needing the host (control API, forwards, ssh-agent) resumes when one connects (see _No host connected_).

`autostart` should be excluded from `config_hash`: flipping it can be applied in place (`docker update --restart …`) instead of marking the instance drifted.

## `devsbd notify`

Available in every sandbox.

```
devsbd notify [--level info|warn|error] [--link URL] [--key K] "PR 123 needs you"
```

- Delivered to the TUI inbox (badge on the instance, an inbox view listing notifications with source instance, time, link) **and** as a desktop notification: `notify-send` on Linux, `osascript -e 'display notification …'` on macOS. A missing notifier is skipped silently.
- `--key` dedupes: a newer notification with the same key replaces the older one (a dispatcher re-reporting "PR 123 conflicted" every poll doesn't spam).
- **Queued**: written to a durable outbox in the container (`/var/lib/devsandbox/outbox/`, surviving container restarts), drained by the host whenever a notify-capable bridge is up. Only the host daemon's (`devsandbox serve`) bridges are (one-shot CLI commands don't drain). The history lives in the shared Inbox store (`inbox.toml`, _Inbox threads_ below), not in a dashboard.

## Dispatchers

A dispatcher is an ordinary sandbox that declares `dispatcher`. Only such sandboxes get the control API: every host daemon bridge advertises `CONTROL`, and the host re-checks the config on each request and denies (exit 77) every other sandbox.

```toml
[sandbox.pr-dispatcher]
extends = "base"
folder = "../pr-dispatcher"            # its own repo: script, AGENTS.md, skills
autostart = "runtime"
dispatcher = { spawn = ["web"], max-instances = 10 }
postStartCommand = "./babysit-loop.sh"

[sandbox.web]                          # children are ordinary instances of it
extends = "base"
folder = "../web"
```

- `spawn`: sandbox configs this dispatcher may instantiate (same config root); `"*"` = every non-dispatcher sandbox (children can never be dispatchers).
- `max-instances`: cap on owned children (stopped ones count), default 10, enforced by the host.

### Control API

```
devsbd ensure <sandbox> --key <key> [--branch B] [--env K=V]...
    # idempotent: creates <sandbox>-<key> if missing, starts it if stopped,
    # rebuilds it if its container is gone; prints the instance name.
    # --branch/--env apply at creation only.
devsbd ls                                   # this dispatcher's children + state (JSON)
devsbd branches <sandbox> [--ahead]         # who holds each branch of <sandbox>'s repo (JSON);
                                            # read-only, see docs/dispatcher-branch-holders.md
devsbd stop <key> [--sandbox S]
devsbd rm <key> [--sandbox S]
devsbd done <key> [--sandbox S]             # mark a child done; `ensure` reusing it clears it
devsbd exec <key> [--sandbox S] [--detach] -- <cmd>...   # a run; see _Runs_
devsbd events [--wait SECS] [--thread KEY]  # pending Inbox events (JSON lines); see _Inbox threads_
devsbd events --follow [--thread KEY]       # the same, pushed over one held-open stream
devsbd events ack <id>...
devsbd thread ls                            # this instance's live threads (JSON; needs inbox = true)
```

Exit codes: 0 ok, 1 failed, 2 usage (or a key shared by two sandboxes without `--sandbox`), 75 no host connected, 77 denied.

- Children are named `<sandbox>-<key>` (e.g. `web-pr-123`): predictable, so a human can `devsandbox vscode web-pr-123`.

- Children are labelled `devsandbox.dispatcher=<dispatcher instance_id>`, and recorded as such in `state.toml`. `ls`/`stop`/`rm`/`done`/`exec` only reach owned children.

- The TUI tree keeps children under their own sandbox group, marked with a dim `⇠ <dispatcher>` suffix.

### Lifetime

- Children are **kept** when the dispatcher is stopped, removed, or rebuilt.
  Cleaning up is the dispatcher script's job (`devsbd rm`); it owns its own
  memory of which keys it manages (a volume, its repo, …) and can reconcile
  against `devsbd ls` on start.

- A child whose dispatcher no longer exists is shown as orphaned in the TUI and removable by hand (`devsandbox rm`).

### No host connected

Control needs the host (worktrees, `state.toml`, runtime calls, bridges), and after a `"runtime"` restart none may be attached yet. Rule:

- `notify` is queued (above) — never lost.
- Control operations **fail fast** with a dedicated exit code (e.g. 75, `EX_TEMPFAIL`) and a "no host connected" message. The script retries/waits. No durable command queue.

The TUI serves control requests for every running dispatcher. Two TUIs don't both serve a request: the daemon routes each one to exactly one bridge, the newest whose host advertises `CONTROL`. One-shot CLI commands don't serve control.

## Inbox threads

Design and rationale: `docs/inbox-threads.md`; user reference: `docs/automations-guide.md`. A sandbox declaring `inbox = true` (typically a dispatcher) puts **threads** (one item each: state, status, message, actions, reply box) into the Inbox and pulls **events** (what the user clicked or typed) back.

### Outbox records

`devsbd thread put|send|withdraw|rm` use the `notify` outbox and stream. The record format (`src/devsbd/notify.rs`) gained an optional `kind` line:

- absent: a plain notify record (`level`, `key?`, `link?`, `msg`), byte-for-byte what older helpers write;
- `kind thread-put`: `key` plus `body`, the thread JSON, escaped and opaque to the helper;
- `kind thread-rm`: `key` only;
- `kind thread-send`: `key` (the thread), `id` (the message id) plus `body`, the message JSON (docs/inbox-redesign.md, *Messages*);
- `kind thread-withdraw`: `key` and `id`.

The helper checks JSON syntax with a std-only validator (`devsbd/src/json.rs`, nesting capped at 32) and pulls out the routing fields: `key` for a put, `thread` + `id` for a send. Keys are checked with `control::valid_key`, message ids with `control::valid_message_id` (`[a-z0-9-]{1,60}`), and a send body is capped at 48 KiB (`notify::MAX_SEND_BODY`, leaving room in the 64 KiB record); any of these fails with exit 2 and nothing queued. The schema belongs to the host (`src/inbox/thread.rs` for puts, `src/inbox/message.rs` for sends; serde, `deny_unknown_fields`, unknown block types rejected).

**Coalescing** (`outbox::enqueue_thread`, rule in `Message::supersedes`): queuing a record drops the pending ones it makes pointless. A put replaces a queued put of its thread; an rm replaces a queued put or rm of its thread and every queued send/withdraw in it; a send or withdraw replaces a queued send or withdraw of the same `(thread, id)`, either way round. A put never replaces a queued rm (rm then put is a fresh thread, not the old one re-headed). Notify records are never coalesced. The new record takes the **position** of the oldest record it replaces (written as `<that name's base>+<n:06>`, which sorts right after it), not the tail of the queue: the host appends a message to the feed when it first sees its id, so moving `send a` behind a later `send b` would swap them, and moving a put behind the sends that need its thread would get them rejected.

### Apply, then ack

The bridge's notify handler applies a record to the store *before* replying `ok` (`bridge::handle_notify` → `apply_message` → `inbox::ops::sink`). A store failure sends no reply, the daemon keeps the file and resends it, so a dashboard dying mid-delivery loses nothing. `inbox::decide` turns each message into one store action:

- notify → push the record;
- thread message from an instance whose sandbox doesn't declare `inbox = true` (`dispatch::declares_inbox`, evaluated only for thread messages) → an `error` notify record from that instance, key `thread:<key>`, `thread put|rm|send|withdraw denied: …`;
- put whose body fails the schema, or whose body `key` differs from the record's → the same, `thread put rejected: <why>`; a send likewise (`thread send rejected: <why>`, also when its body `thread`/`id` differ from the record's);
- valid put → `Inbox::put`; rm → drop the thread;
- valid send → `Inbox::send`: no such thread of this owner → `thread send rejected: no thread `<key>`; put it first`; a new id is appended to the feed (`feed::send`), marks the thread unread and returns a popup when the header is `needs-you`; the same id with other blocks replaces the message in place and tags it `edited` (no unread, no popup); the same blocks again is a no-op; a withdrawn id sent again comes back in place, `edited`;
- withdraw → `Inbox::withdraw`: tags the message `withdrawn` and keeps it; an unknown thread or id is a silent no-op.

A put that changes nothing leaves the store byte-identical, so no write, no mtime bump, no popup, no status line. A change to `message`/`state`/`status` adds one timeline entry each and marks the thread unread; entering `needs-you` returns a desktop popup (key `thread:<key>`, through the per-instance rate limit). Retention (`Inbox::prune`) runs on this path: archived threads, and done threads with no pending events, go 14 days after their last change.

### The shared store

`inbox.toml` next to `state.toml` (`src/inbox/store.rs`, format v2; v1 files migrate on load, names resolved to instance ids through `state.toml`, unresolvable ones archived). Every writer (bridges' notify sinks, every dashboard's `d`/`D`/mark-read and pane ops, the control handlers, `devsandbox rm`) goes through `src/inbox/ops.rs`, one `store::update_at` per op: exclusive `File::lock` on the sibling `inbox.lock`, load, mutate, write through a temp file + rename only when the content changed. Readers take a shared lock, best-effort. Dashboards reload when the file's mtime or length changes (checked each tick), so a dismissal in one is a dismissal in all. Before this, each dashboard kept its own copy and overwrote the file: an older dashboard brought back records dismissed in a newer one.

Threads are keyed by `(owner instance_id, key)` and kind (a notify key and a thread key can coincide without meeting). Events live on their thread (`[[thread.event]]`) until acked; user ops (`Act`, `Reply`, `MarkDone`, `Reopen`) mint the event id (`e-<unix secs:010>-<4 hex>`) and time under the store lock, so a dashboard never applies them to its own copy first. The per-owner cap (200) counts notes, timeline entries and pending events; it evicts archived threads first, then done threads without pending events, then single oldest items, and never drops an event (each thread keeps at most 100, oldest dropped).

### Control ops

`events`, `events-ack`, `thread-ls` and `done` join the control codec (`src/devsbd/control.rs`, whose module doc has the line format and the `events` JSON). `events` takes `timeout` (capped at `MAX_WAIT`, 300 s, by the helper and the host), `events-ack` a repeatable `ack <id>` field; neither takes `sandbox`, since they always address the requester's own threads. `events` takes an optional `key`: the thread filter (`devsbd events --thread`), reusing the codec's existing field rather than adding one, since a thread key has the child key's charset (`valid_key`); `events-ack` and `thread-ls` take no `key`.

- `events`: the requester's pending events, oldest first across threads, one JSON object per line (`id`, `thread`, `key`, `kind` = action|reply|done|reopen|submit, `action?`, `text?`, `message?`, `form?`, `answers?`, `at` RFC 3339 UTC). `key` is the deprecated v2 name of `thread`, same value, removed with the other v2 compat (step 13b of `docs/inbox-redesign.md`). All of them until acked: delivery is at least once. With a `key` filter, only that thread's events are answered and waited for; an unknown thread answers none (a dispatcher may filter before its first put lands). With a `timeout` and nothing pending, the handler thread waits, re-reading the store only when its stamp moves (every 500 ms). It holds no lock beyond the store's shared one and skips the host control lock, so a waiting `events` never delays a click being written or another dispatcher's `ensure`. The body stops at about half of `MAX_RESPONSE`; the rest follows after an ack.
- `events-ack`: drops the requester's events with those ids, answers the count; unknown ids are skipped.
- `events-follow` (`devsbd events --follow`, `Op::Subscribe`): the requester's events pushed over a held-open response; see _Following events_ below.
- `thread-ls`: the requester's live (non-archived) threads as a JSON array in `thread put` shape.
- `done`: sets `done = <unix secs>` on an owned child in `state.toml` (through an `Executor` method, so tests stay fake), under the host control lock like `ensure`/`stop`/`rm`. `ensure` reusing a done child clears it before its subprocess runs.

### Following events

`events-follow` is the one control op whose response isn't one `status`/`body` pair. **Framing:** the host answers the usual encoded response first; only when it's `status ok` (empty body) the stream stays open and carries raw JSON lines after it, not escaped, one per line, until either end closes:

```text
status ok
body 
{"id":"e-1790900001-3f2a","thread":"pr-6900","key":"pr-6900","kind":"reply","text":"…","at":"…"}
{"kind":"ping"}
{"kind":"replaced"}
```

Any other status (`denied`, `usage`, `failed`, the daemon's own `no-host`) is the whole answer, as for every op. That's the smallest additive change: the request is an ordinary one with a new op name, so an older host fails to decode it (`bad op`) and answers `usage`, which the client reports as exit 2 (the dispatcher falls back to `events --wait`); an older helper has no `--follow` flag (also exit 2). No frame-protocol `VERSION` bump: it's an ordinary `CONTROL` stream that simply stays open, and mux streams already carry data in both directions for as long as they live.

- **Lines.** Every pending event of the requester (only `key`'s thread with a filter; `timeout` is a usage error), then each new one as it's enqueued, each at most once per stream: the handler keeps the ids it sent (forgetting acked ones), so a re-read of the store doesn't repeat them. Acks still go through `events-ack`, so a new stream starts with everything unacked again: at least once across reconnects. `{"kind":"ping"}` after `control::FOLLOW_PING` (30 s) without a line.
- **One follower per owner.** A process-wide registry maps owner `instance_id` → the current follower's generation (`dispatch::Followers`); a new follower bumps it, and an older one that sees the bump writes `{"kind":"replaced"}` and closes (the client exits 0). So two copies of a dispatcher can't both act on one event.
- **Host side.** Served on the bridge's control handler thread (`bridge::handle_control` hands a decoded `Subscribe` to the bridge's follow handler, `dispatch::follow`), holding one of the bridge's `MAX_HANDLERS` slots for its life; counted in `CONTROL_IN_FLIGHT` like a waiting `events`, so a follower is a daemon holder (`docs/serve.md`). It reads only: no host control lock. It wakes on in-process store writes (`inbox::ops::wait_changed`), in slices of at most 500 ms with the stamp check for other processes' writes, and on the ping timer. `CONTROL_READ_TIMEOUT` only bounds reading the request; writes time out after 60 s (`FOLLOW_WRITE_TIMEOUT`, twice the ping), so an idle but healthy stream is never cut. A write to a closed stream ends it: a follower whose client left is gone by the next ping.
- **Helper daemon.** `daemon::control_follow` relays the stream to the API-socket client as bytes arrive: no reply timeout, no `MAX_RESPONSE` cap, no buffering to EOF. Reads from the mux stream go through a 64-chunk queue drained by a writer thread, so a client that stops reading can't back bytes up into the mux (whose frame reader writes these legacy streams inline); a full queue drops the follower instead (it reconnects). Either end closing closes the other. `control::FOLLOW_SILENCE` (90 s, three pings) without a byte ends it too.
- **Client.** `devsbd events --follow` prints each complete line as it arrives and flushes; after `replaced` it exits 0. The stream ending any other way (the bridge dropped, so the daemon's mux closed the stream; the daemon died; 90 s of silence; a line cut short) exits 75.

### Authorization

Only instances whose sandbox declares `inbox = true` put threads or read events (`declares_inbox` on every message, the same check in `dispatch::handle_with` on every `events`/`events-ack`/`events-follow`/`thread-ls` request, against the config as it is now). `dispatcher` grants only the child ops; a dispatcher's child may declare `inbox = true` and own threads of its own. Everything is scoped to the requester's `instance_id`: another instance never sees, acks or changes an owner's threads, and host actions on a thread target only its `child` (resolved among the owner's own children) or the owner itself. Ids survive stop/restart/rebuild and are never reused, so `devsandbox rm` **archives** the removed instance's threads (notify ones too) instead of deleting them: read-only, shown only in **All**, dropped by retention. A put from a live owner un-archives (only `rm` archives).

### Helper self-heal

New verbs need a new `devsbd` in the running dispatcher, and only `run`/`start`/`port` used to reinstall it. Now `Bridges::reconcile` runs `devsbd::ensure_recorded` (a no-op hash check when current) before every bridge (re)spawn, on the bridge worker thread; the daemon of the old build is taken over by the new one. Opening a newer dashboard is enough.

## Runs

`devsbd exec <key> [--sandbox S] --detach -- zidane -p "…"` starts a tracked process in a child: run id, start/end time, exit status, captured stdout/stderr (kept in the child under `/var/lib/devsandbox/runs/<id>/`, streamed on demand). Runs live in the child, so the dispatcher names it:

```
devsbd run ls <key> [--sandbox S]                      # id, state, start (UTC), argv; one line per run
devsbd run logs <key> <id> [--sandbox S] [--follow]    # output so far; --follow until the run ends
devsbd run wait <key> <id> [--sandbox S] [--timeout SECS] # prints `exited N` | `killed N` | `lost`,
                                                       # or `running` after --timeout; exit 0
devsbd run rm <key> <id> [--sandbox S] [--force]       # delete a run; --force kills a running one first
devsbd run prune <key> [--sandbox S] [--keep N]        # delete ended/lost runs but the newest N (default 0)
```

States: `running`, `exited N`, `killed N` (signal), `lost` (its supervisor died without recording an end, e.g. the child restarted). Without `--detach`, `exec` prints the id to stderr (`devsbd: run <id>`), streams the output, and exits with the run's code (`killed N` → 128+N, `lost` → 1). The child must be a running, owned child (`ensure` it first). Inside the child the same store is `devsbd run start [--cwd D] -- cmd…` / `run ls` / `run logs <id> [--offset N]` / `run wait <id> [--timeout S]` / `run rm <id> [--force]` / `run prune [--keep N]` — what the host execs there. The TUI shows a child's last runs (dim `run` rows) after its processes.

## Transport

All of the above rides the existing devsbd frame channel (`docs/sandbox-helper.md`), container-initiated `Open` streams on new channels:

- `channel::CONTROL` (line-based request/response over `Data`, see `src/devsbd/control.rs`), `channel::NOTIFY`.
- New caps bits (`caps::NOTIFY`, `caps::CONTROL`) so an old host/helper pair degrades cleanly.

In-container clients (`devsbd notify`'s poke, the control commands) talk to the daemon over `/run/devsandbox/api.sock`; the daemon sends each stream to the newest bridge whose host advertises the cap. The TUI keeps a bridge per running helper-capable instance (with or without a host ssh-agent) and only those advertise `NOTIFY` + `CONTROL`; short-lived CLI bridges (`exec`, lifecycle, forwards) advertise neither.

## Patterns (for the user docs)

devsandbox takes no stand; both are supported and documented:

- **One instance per item** — `key = "pr-123"`. Easy to inspect (open VS Code
  in the PR's own instance). Memory is free: pr-verify later calls `ensure`
  with the same key and finds the same container, worktree, agent session. So
  retire with `stop`, not `rm`, until the item's last stage is done.

- **Worker pool** — `key = "worker-1"`, `worker-2`; the script assigns items to
  workers. Bounded resources regardless of item count; harder to inspect one
  item; per-item memory is the script's concern.

- **Idle children** — stop a child between runs and `ensure` it before the next
  `exec`. The boot pass never starts children, so idle children stay stopped
  across reboots (runtime-mode ones that were running come back via
  `unless-stopped`).

- Sample dispatcher: a babysit loop polling `gh pr list`, a cheap
  needs-attention check per PR, `ensure` + `exec --detach` only when needed,
  `notify` on anything needing a human.

## Steps

One step = one commit. Each implementer re-reads the landmarks it's given before editing (line numbers drift as steps land). After any change under `devsbd/` or to `src/devsbd/{proto,mux}.rs`, rebuild the helper with `scripts/build-devsbd.sh` before `cargo test --workspace` (a stale embedded blob fails the helper tests).

Decisions taken while planning (differ from or sharpen the design above):

- **Triggers**: autostart fires on TUI load (before the alternate screen, so
  `run` output is readable) and after the user's own `devsandbox run` /
  `devsandbox start` — not on every CLI command (`ps`, `status --json`,
  `stop --all` shouldn't start things). After, not before, so `run web` for an
  autostart `web` doesn't create two instances.

- **Boot id is per config root**: `state.toml` is global but autostart is a
  config property, so the record is keyed by project id
  (`services::project_id`), not a single `last_autostart_boot`.

- **No lease**: the daemon already routes each client stream to exactly one
  bridge, the newest capable one (`devsbd/src/daemon.rs:73`, `agent_route`), so
  two TUIs never both serve a request. The lock-file lease is dropped.

- **Runtime-mode `postStartCommand`** (open question above): run only by the
  in-container boot hook on every container start, never by the host `start`;
  `run` (create) still runs it from the host after `postCreateCommand` because
  the hook can't (helper not installed yet at create).

- **Long-running loops**: lifecycle commands block the host
  (`src/commands/run/lifecycle.rs:29`), so a dispatcher loop in
  `postStartCommand` must background itself (`nohup ./loop.sh >loop.log 2>&1 &`).
  Documented in step 11; a detached lifecycle form is a possible later step.

- **`devsbd` on `PATH`**: the helper lives at `/run/devsandbox/bin/devsbd`
  (`src/devsbd.rs:111`); install also symlinks `/usr/local/bin/devsbd`
  (best-effort) so scripts call `devsbd notify`.

### Step 1 — `autostart = true` [x]

- `src/config.rs:30` `SandboxProperties`: `autostart: Option<Autostart>`,
  `enum Autostart { Off, Devsandbox, Runtime }` deserialized from `true` /
  `false` / `"runtime"` (anything else errors, naming the accepted values).
  `"runtime"` behaves like `true` in this step. Excluded from `config_hash`
  (`src/config.rs:689`): strip the `autostart` key before hashing, in
  `resolve_sandbox` and `resolved_table` alike.
- `src/state.rs:92` `State`: `autostart_boot: BTreeMap<String, String>`
  (project id → boot id), `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]`.
- New `src/commands/autostart.rs`: `pub fn autostart(dir: &Path)` — never
  fails the caller (warnings to stderr). No config / no boot id → no-op.
  Same boot id recorded for this project → no-op. Otherwise **record first,
  save**, then act, so a failing start can't retrigger on every invocation.
  Per `autostart` sandbox: its instances in state with matching
  `sandbox` + `project`: start the stopped ones through the full
  `start_instance` path (`src/commands/start.rs:43`, make it `pub(crate)`),
  skip running ones, skip containerless ones with a note; none at all →
  `run::run(dir, Some(name), None, None, None)`.
- Boot id: Linux `/proc/sys/kernel/random/boot_id` (trimmed); macOS
  `sysctl -n kern.boottime` → the `{ sec = …, usec = … }` part. Parsing is a
  pure fn with tests.
- The decision is a pure fn (recorded boot, current boot, autostart sandboxes,
  instance rows with running state) → `Vec<Action>` (`Start(key)` /
  `Run(sandbox)`), unit-tested: already-booted no-op, stopped started, running
  skipped, missing → run, other project's instances ignored.
- Wiring: `src/main.rs:225`/`:234` call it after `Run`/`Start` (whatever their
  result, then return that result); `src/tui/mod.rs:88` `dashboard` calls it
  before `setup()`.
- Tests: config parse (bool, `"runtime"`, bad string), hash unaffected by
  `autostart`, state round-trip with the new map.

### Step 2 — `autostart = "runtime"`: restart policy [x]

- `src/runtime/mod.rs` `Backend`: `fn supports_restart_policy(&self) -> bool`
  (docker/podman true, Apple `container` false — confirm in its backend file)
  and `fn set_restart_policy(&self, container, policy: &str) -> Result<()>`
  (`update --restart <policy>`; Apple: no-op).
- `run_container` (`src/commands/run/mod.rs:520`): `--restart unless-stopped`
  when `Runtime` and supported; Apple → warning, behaves as `true`. Podman:
  warn at `run` that boot restarts need `podman-restart.service` enabled.
- Because `autostart` isn't hashed, apply flips in place: `start_instance`
  and `autostart` set the policy (`unless-stopped` / `no`) when the sandbox
  resolves; on failure (old podman without `update --restart`) warn "recreate
  with `devsandbox rebuild --force`".
- Tests: run-args builder includes/omits `--restart` per mode + backend.

### Step 3 — in-container boot hook [x]

Landed limitations: the hook runs as the container's user, so a non-root `containerUser` can't start the daemon or switch users (daemon returns on the next host `start`); switching from root drops supplementary groups (no `setgroups` without libc); one zombie per container start unless `init = true`.

- Container command becomes a hook + keep-alive for **new** containers:
  `sh -c '[ -x /run/devsandbox/bin/devsbd ] && /run/devsandbox/bin/devsbd boot & exec sleep infinity'`
  (replaces `sleep infinity` at `src/commands/run/mod.rs:585`). No binary →
  no-op, so first create is unaffected. Existing containers keep `sleep
  infinity`; `start` of a runtime-mode instance without the hook warns to
  `rebuild --force`.
- `devsbd boot` (`devsbd/src/main.rs:31`): start the daemon detached
  (idempotent, same as `start_daemon`, `src/devsbd.rs:180`), then, if
  `/run/devsandbox/boot` exists, run its `postStartCommand` as the recorded
  user/cwd/env, output appended to `/run/devsandbox/boot.log`. File format is
  line-based and hand-parsed (no serde in the helper; size budget): `user`,
  `cwd`, `env K=V`, one `cmd` per sequential command with NUL-separated argv.
  User switch via `CommandExt::uid/gid` after an `/etc/passwd` lookup.
- Host writes/removes `/run/devsandbox/boot` (via `exec -u root … sh -c 'cat >'`,
  like `INSTALL_SCRIPT`, `src/devsbd.rs:115`) on `run`, `start`, `rebuild` for
  runtime-mode instances with a `postStartCommand`; removes it otherwise.
  Host `start` skips `postStartCommand` for runtime-mode instances (decision
  above).
- Also here: the best-effort `/usr/local/bin/devsbd` symlink in the install.
- Tests: boot-file serializer (host) and parser (helper) round-trip on a
  shared fixture; `#[test_utils::docker_test(helper)]`: a runtime-mode
  container restarted with `docker restart` runs `postStartCommand` again.

### Step 4 — `devsbd notify`: helper side [x]

- `devsbd notify [--level info|warn|error] [--link URL] [--key K] <msg>`:
  writes one record to `/var/lib/devsandbox/outbox/<secs>-<nanos>-<pid>` (atomic
  rename), then pokes the daemon over a new client socket
  (`/run/devsandbox/api.sock`, like `AGENT_SOCK`, `devsbd/src/daemon.rs:145`).
  Succeeds even with no daemon (the record is queued).
- Protocol (`src/devsbd/proto.rs`): `channel::NOTIFY = 3`,
  `caps::NOTIFY = 1 << 2` (host serves notify). Daemon: on poke and on each
  bridge attach whose host advertises `NOTIFY`, flush the outbox oldest first:
  one `Open(NOTIFY)` stream per record, record bytes, `Eof`; delete the file
  only after the host's `ok` reply.
- Tests: record encode/decode, outbox ordering, flush-on-attach with an
  in-process mux pair (see existing daemon tests).

### Step 5 — notify: host side [x]

- `src/devsbd/bridge.rs`: bridges advertise `caps::NOTIFY` when given a
  notification sink; `on_open(NOTIFY)` reads the record, replies `ok`, hands a
  `Notification { instance, level, key, link, msg, at }` to the sink (a
  `UnixStream::pair` + thread, since the mux speaks `Conn`).
- `Bridges::reconcile` (`src/devsbd/bridge.rs:359`): keep a bridge per running
  helper-capable instance even without a host agent (today it clears
  everything when there's no agent at `:362`); agent routing unchanged.
- Desktop notifier: `notify-send` / `osascript`, spawned quietly, missing
  binary ignored; the argv builder is pure and tested.

### Step 6 — notify: TUI inbox [x]

- `src/tui/app/`: notifications list in `App` (dedupe by `key`, capped at 200,
  in memory), unread badge on the instance row, an inbox view listing
  instance, time, level, message, link; key/command to open and to clear.
  I/O-free, unit-tested with `test_support.rs` fixtures.
- `src/tui/mod.rs`: drain the sink each loop turn into `App`; fire the desktop
  notifier from the worker, not the UI thread.

### Step 7 — `dispatcher` config + control handler (host, pure) [x]

- `src/config.rs`: `dispatcher: Option<Dispatcher { spawn: Vec<String>,
  max_instances: Option<u32> }>` (`"*"` = any sandbox of this root), validated
  at `ls` (unknown sandbox names error, except `"*"`).
- `src/state.rs` `Instance`: `dispatcher: Option<String>` (owner
  `instance_id`); `run_container` adds label `devsandbox.dispatcher=<id>`.
- `Instance.config_dir: Option<PathBuf>` recorded at creation (state holds
  only the project id today); the handler resolves a dispatcher's config root
  from it.
- Execution by **subprocess**, not in-process: the handler runs on the TUI's
  bridge worker, and `run`/`start` print and stream docker/git output through
  inherited stdio (`run_checked`, `src/runtime/mod.rs:165`), which would hit
  the alternate screen. So each op spawns `current_exe() -C <config_dir> run …
  | start … | stop … | rm …`, stdio to `<data>/devsandbox/logs/dispatch-*.log`,
  and maps the exit status. New hidden `run` flags: `--env K=V` (extra
  containerEnv for this instance) and `--dispatcher <instance_id>` (owner).
- Wire format shared with the helper (`src/devsbd/control.rs`, `#[path]`,
  line-based like `bootfile`/`notify` — the helper has no serde): request
  `op`/`sandbox`/`key`/`branch`/`env` lines (later `force`, and a bare `ahead`
  for `branches`); response status + body (`ls` and `branches` bodies are JSON
  built on the host).
- New `src/commands/dispatch.rs`: pure authorization (`spawn` list, ownership,
  cap) and naming (`<sandbox>-<key>`, key charset validated), `ensure` = create
  if missing, start if stopped, return the name; executor injectable for tests.
- Tests: codec round-trip, authorization matrix, naming, cap, `ls` filters to
  owned children, `ensure` decision per child state.

### Step 8 — control channel end to end [x]

- Protocol: `channel::CONTROL = 4`, `caps::CONTROL = 1 << 3`. Helper:
  `devsbd ensure|ls|stop|rm` send a request over `api.sock`; the daemon opens
  a `CONTROL` stream to the newest bridge whose host advertises `CONTROL`, or
  answers "no host connected" → the CLI exits 75.
- Host: only bridges of instances whose sandbox declares `dispatcher`
  advertise `CONTROL`; the handler re-checks on every request (config may have
  changed). (Landed differently: every TUI bridge advertises `CONTROL` and the
  handler denies non-dispatchers.) Runs on the bridge worker thread, quiet, never the UI thread.
- TUI tree marks children with a dim `⇠ <dispatcher>` suffix under their own
  sandbox group (no re-nesting); orphans marked `⇠ <id> (orphan)`.
- Tests: exit 75 with no bridge; `#[test_utils::docker_test(helper)]`
  dispatcher `ensure`s a child and `ls` lists it.

### Step 9 — runs [x]

- `devsbd exec <key> [--detach] -- cmd…` via `CONTROL`: host execs in the
  child through the child's own helper (`devsbd run start` there), which
  records run id, times, exit status, and stdout/stderr under
  `/var/lib/devsandbox/runs/<id>/`. `devsbd run ls|logs|wait <id>` for the
  dispatcher.
- TUI: a child's runs next to its processes.

### Step 10 — Apple `container` pass [x]

- Re-check every step on Apple `container`: runtime mode falls back with a
  warning, notify + control work over the same bridges.

Apple `container` (code audit on Linux, not run on a Mac; flags checked
against apple/container's `docs/command-reference.md`):

- **Verified by code.** `run` passes `--label` (`devsandbox.boot_hook`,
  `devsandbox.dispatcher`) and the `sh -c '<hook>'` command after the image as
  Apple's init-process arguments; `--restart` is never emitted there. `label`
  reads `configuration.labels` from `inspect` JSON. Every exec argv the steps
  added (`sync_boot`, bridges, `exec_argv` for runs, `devsbd run ls`) uses only
  `-i`/`-d`/`-u`/`-w`/`-e`, which Apple's `exec` spells the same; runs already
  go through `commands::exec::exec_argv`. Dispatch subprocesses get
  `DEVSANDBOX_RUNTIME` pinned to the parent's backend.
- **Degrades.** `autostart = "runtime"`: warning at `run`, then behaves as
  `true` (once-per-boot pass from the TUI / first `run`/`start`); no restart
  policy is touched, no boot file is written (`sync_boot` removes any), and
  host `start` keeps running `postStartCommand` without reading the label.
  Autostart skips (without recording the boot) when the runtime doesn't answer
  `ls`, e.g. before `container system start`.
- **Unverified on Apple.** `exec -i` carrying the bridge's binary mux stream
  and the boot-file stdin byte-exact; `--` and dash-leading args after the
  container id reaching the process unparsed (`devsbd run start -- …`,
  `--offset`/`--timeout`); whether `/run/devsandbox` survives `container
  stop`/`start` (host `start` reinstalls the helper anyway, so only the
  in-container `devsbd boot` daemon restart would be lost); the macOS boot id
  (`sysctl kern.boottime`) path.

### Step 11 — docs [x]

Landed as `docs/automations-guide.md` (user guide, linked from the README's
_Automations_ section), plus the config spec, `docs/high-level-architecture.md`,
`docs/sandbox-helper.md` and the `AGENTS.md` module map.

- `skills/config-toml-spec/SKILL.md`: `autostart`, `dispatcher`.
- User doc (patterns above, sample babysit dispatcher, backgrounding the loop,
  `devsbd` commands, exit 75); `docs/high-level-architecture.md` and the
  `AGENTS.md` module map for the new modules.

### Step 12 — worktrees on existing branches [x]

Decision (user): creating an instance on an existing branch must work, for
`run --branch` and therefore `devsbd ensure --branch`.

- `create_worktree` (`src/commands/run/worktree.rs:22`): today it bails when
  `refs/heads/<branch>` exists (~:56-74). New rule: local branch exists →
  `git worktree add <path> <branch>` (no `-b`, no start point, `--base`
  ignored with a note); only `origin/<branch>` exists (a PR head fetched from
  the remote; `git fetch origin <branch>` first, quietly, best-effort) →
  `git worktree add --track -b <branch> <path> origin/<branch>` so a plain
  `git push` updates the PR; neither → today's `-b … --no-track <start>`.
  A branch already checked out in another worktree (or the base checkout) →
  git refuses; surface a message naming where it's checked out.
- `rm` must not delete a branch it didn't create: record in state whether
  `run` created the branch (`Instance.branch` is only set for created ones —
  keep that invariant, or add a flag) so `rm`'s delete-branch prompt only
  covers branches devsandbox made.
- Tests: the three branch cases against a temp git repo with a bare remote
  (look for existing git fixtures in `worktree.rs` tests), plus the
  checked-out-elsewhere message.

### Step 13 — `--env` persisted [x]

Decision (user): `run --env` values live in `state.toml` and survive
`rebuild` (including `ensure` recreating a containerless child).

- `Instance.extra_env: BTreeMap<String, String>` (serde default, skip if
  empty), written by `materialize`, reused by `rebuild` when `RunExtras.env`
  is empty (same fallback shape as `dispatcher`/`config_dir`).
- `ensure` on an existing child with different `--env`: still ignored
  (documented), not merged.
- Follow-up (user decision): `ensure --env` on an existing child (running,
  stopped, or containerless) *replaces* its `extra_env` with exactly the
  given set (no `--env` keeps it), written by the host handler under the
  control lock before the `start`/`rebuild` subprocess, which reads it back.
  Every devsandbox exec uses `Instance::exec_env` = `remoteEnv` overlaid by
  `extra_env` (instance-specific wins, as `docker run` puts `--env` after
  `containerEnv`): `exec_argv` (CLI, TUI terminals, dispatcher run ops),
  lifecycle execs, the boot file. The container's own env changes only on
  `rebuild`.

### Step 14 — autostart skips dispatcher-owned instances [x]

Decision (user): the once-per-boot pass never starts or creates children.

- `commands/autostart.rs` `actions`: filter rows with `Instance.dispatcher`
  set; a sandbox whose only instances are children still gets no `Run` (the
  dispatcher owns that sandbox's fleet — decide: count children as "existing"
  so no stray instance is created; test it).
- `"runtime"` children still come back via the runtime's `unless-stopped`
  when they were running — that's the runtime's rule, documented.

### Step 15 — clearing runs [x]

Decision (user): dispatchers clear their runs through `devsbd`.

- Helper (`devsbd/src/runs.rs`): `run rm <id>` (refuses a `running` run
  unless `--force`, which kills its process group first) and `run prune
  [--keep N]` (deletes finished/lost runs, newest N kept, default 0).
- Control ops `RunRm` / `RunPrune` (`src/devsbd/control.rs`,
  `src/commands/dispatch.rs`), CLI `devsbd run rm <key> <id> [--force]` and
  `devsbd run prune <key> [--keep N]` (`devsbd/src/ctl.rs`). Read-only
  ops skip the host lock; these don't touch `state.toml` either, so they skip
  it too.
- Docs: guide + known-limitations entry replaced.

### Step 16 — no zombie from the boot hook (proposed by orchestrator, not decided)

`devsbd boot` exits after `postStartCommand`, leaving a zombie under a
`sleep` PID 1 when `init` is off. `boot` could instead `exec` into `devsbd
daemon` once done (the daemon is long-lived anyway), removing that zombie.
Run supervisors still need `init = true`. Skip unless approved.

### Security fixes (steps 17–21)

From a security review of the automations work. Threat model: a container
(and any user in it) is untrusted relative to the host. Low findings are
deferred.

### Step 17 — host git never runs repo-controlled code (Critical) [x]

Every worktree instance bind-mounts the base repo's `.git` read-write
(`git_companion_mount`, `src/commands/run/worktree.rs:10`), and the host runs
git on that repo (`worktree add` / `fetch` / `worktree remove` / `branch -D`,
`worktree.rs` + `src/commands/rm.rs`). A container can plant hooks or
command-running config (`core.fsmonitor`, `core.sshCommand`, filter drivers,
`include.path`, …) that the host then runs as the user. Dispatchers let a
container trigger those host git calls itself (`ensure`, `rm`).

- One wrapper for **every** host git invocation (all `Command::new("git")`
  sites in `worktree.rs` and `rm.rs`): always prepend
  `-c core.hooksPath=/dev/null -c core.fsmonitor=false`, stdin null, and
  `GIT_TERMINAL_PROMPT=0`.
- Before running, a pure check of the repo's local config: read
  `<git-common-dir>/config` plus every `worktrees/*/config.worktree` **as
  files** (parse the git config syntax — don't run `git config`, which could
  resolve includes). Keys are checked against an **allowlist** (section names
  compared case-insensitively):
  - `core.{repositoryformatversion,filemode,bare,logallrefupdates,ignorecase,precomposeunicode,symlinks,autocrlf,eol,safecrlf}`
  - `extensions.*`
  - `remote.<n>.{url,pushurl,fetch,tagopt,prune,mirror}`
  - `branch.<n>.{remote,merge,rebase,pushremote,description}`
  - `user.{name,email}`
  - `init.defaultbranch`, `pull.{rebase,ff}`, `push.{default,autosetupremote}`
  - `fetch.prune`
  - `gc.*` except `gc.*hook*`
  - `lfs.*`, `submodule.<n>.{url,active,update}` where `update` is not a `!command`
  - `worktree.*`
  - `rerere.*`
  - `color.*`
  - `advice.*`

  Anything else, notably `include*`, `filter.*`, `diff.*`, `merge.*`, `core.sshCommand`, `core.gitProxy`, `core.askPass`, `credential.*`, `url.*`, `protocol.*`, `http.*`, `uploadpack.*`, `receive.*`, `remote.*.uploadpack|receivepack|vcs|proxy`, and `core.alternateRefsCommand`, makes the op **refuse** with
  `refusing to run git on <repo>: .git/config sets <key>, which can run
  commands on the host; a sandbox may have written it — review and remove it`.
  Remote URLs using `ext::` are refused the same way. Also check
  `objects/info/alternates` exists → refuse (points host git elsewhere).
- Tests: parser (sections, subsections with quotes, continuation lines,
  comments, case), allowlist accept/reject tables, wrapper args; a test that
  a planted `post-checkout` hook and `core.fsmonitor` do not run during
  `create_worktree`/removal in a temp repo (the reviewer's PoC).
- Document in the guide + `docs/high-level-architecture.md`.

### Step 18 — dispatcher branch values are data, not templates (High) [x]

`req.branch` reaches `run --branch`, where `substitute` expands
`${localEnv:…}` (host env) and path vars (`src/commands/run/mod.rs:~145`).

- `control.rs`: `valid_branch` — `[A-Za-z0-9._/-]`, 1–200 chars, no leading
  `-`/`/`/`.`, no `..`, `//`, `@{`, trailing `/`, `.lock` suffix (git
  check-ref-format rules, implemented purely); checked in `check_fields`
  (`dispatch.rs`) → `Usage`.
- `run`: when `--dispatcher` is set, the `--branch` value is used verbatim
  (no `substitute`); the sandbox's `worktree-branch` pattern (config, trusted)
  still substitutes.
- Tests: valid/invalid branch table; dispatcher branch with `${localEnv:X}` is
  rejected at the boundary, and never expanded in `run`.

### Step 19 — dispatcher authorization hardening (Medium ×2) [x]

- `--env` names: deny-list at the control boundary (`control::parse_env` or
  `check_fields`) and in `run --env`: `PATH`, `HOME`, `SHELL`, `USER`, `ENV`,
  `BASH_ENV`, `IFS`, `CDPATH`, `PS4`, `PROMPT_COMMAND`, `SSH_AUTH_SOCK`,
  `LD_*`, `DYLD_*`, `GCONV_PATH`, `GIT_*`, `NODE_OPTIONS`, `PYTHON*`,
  `PERL5*`, `RUBYOPT`, `TMPDIR` → `Denied` naming the var.
- Root execs devsandbox runs in containers (`INSTALL_SCRIPT`, `sync_boot`,
  `start_daemon`, bridge spawn — `src/devsbd.rs`, `src/devsbd/bridge.rs`)
  invoke `/bin/sh` by absolute path.
- Children can't be dispatchers: `ensure` of a sandbox that declares
  `dispatcher` → `Denied` (even if named in `spawn`); a control request from an
  instance with `Instance.dispatcher` set → `Denied`. `"*"` therefore never
  matches dispatcher sandboxes.
- `max-instances` defaults to 10 when unset.
- Docs: config-spec skill + guide: `"*"` grants every non-dispatcher sandbox
  of the root (their mounts, docker socket, privileges); the env deny-list.
- Tests: env deny table; dispatcher-sandbox spawn denied; child-as-dispatcher
  denied; default cap.

### Step 20 — host-side limits and timeouts (Medium) [x]

- Per-bridge semaphore for notify/control handler threads (e.g. 8), held for
  the handler's whole life (not the mux entry): over the limit → the `Open`
  is refused (closed) without spawning (`src/devsbd/bridge.rs` `open_stream`).
- Bounded, timed child I/O: `dispatch.rs` `exec_in` and the TUI's `run ls`
  exec (`src/tui/mod.rs:~647`) read stdout through `take(MAX_RESPONSE + 1)`
  (TUI: a smaller cap) with a wall-clock timeout that kills the child
  (`exec_in`: 5 min except `run-wait`, which is already capped at 300 s + slack;
  TUI `run ls`: 5 s). The dispatch `devsandbox` subprocess gets a timeout
  (30 min, kill) so the host lock can't be held forever.
- Desktop notifications rate-limited per instance (token bucket: burst 3, one
  per 10 s), repeats of the same `(instance, key)` within the window coalesced;
  inbox entries still arrive.
- Inbox: per-instance cap of 200 records, thread history included; no global
  cap (`src/tui/app/inbox.rs`).
- Tests: semaphore refusal, bounded reader truncation + timeout kill (pure
  helper over a spawned `sh -c`), token bucket, per-instance cap.

### Step 21 — helper-side hardening of shared dirs (Medium, DoS part) [x]

- `run ls` (`devsbd/src/runs.rs`): newest 50 runs only, argv truncated
  (200 chars), entries that aren't real dirs / regular files skipped.
- `argv`/`meta` read with a size cap (`take`), so one huge file can't blow up
  the output. FIFO/symlink hardening of the shared dirs is the deferred Low
  finding (the host-side timeouts of step 20 bound its effect on the TUI).
- Tests: cap on count and argv length; non-regular entries skipped.

## Open questions

- Can the restart policy be changed in place on podman (`podman update --restart`, version-dependent), or does flipping `autostart` there require a recreate?
- Should `postStartCommand` run by the daemon (runtime mode) and by `start` (host mode) be deduped by container start time, or should runtime-mode sandboxes just never have the host run it?
- Notification retention in the TUI (count/age), and whether "dismissed" is persisted in `state.toml`.

## Future

- **`npx devsandboxes init dispatcher`** — scaffold a dispatcher repo: `devsandboxes.toml` snippet, dispatcher script, git init, `AGENTS.md` + skills. To be explored.
- Declarative `[automation.*]` sugar (source / key / when / run) compiled onto the dispatcher primitives, once a few real dispatchers show the common shape.
- Webhook sources (`gh webhook forward`) instead of polling.
