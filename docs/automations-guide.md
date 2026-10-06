# Automations: user guide

Sandboxes that come up on their own after a boot, tell you when they need you, and spawn their own child instances to do work. devsandbox provides the plumbing only; what to watch and when to act lives in a script you write. Design and internals: [`docs/automations.md`](automations.md), [`docs/inbox-redesign.md`](inbox-redesign.md) and [`docs/serve.md`](serve.md) (the host daemon).

Everything here except `autostart = true` needs the embedded `devsbd` helper (release archives, npm package, local builds after `scripts/build-devsbd.sh`; not `cargo install`). Inside a sandbox it is on `PATH` as `devsbd` (a best-effort `/usr/local/bin/devsbd` symlink; the binary is `/run/devsandbox/bin/devsbd`). The host daemon (`devsandbox serve`, docs/serve.md) updates the helper of every running instance it bridges, so a running dispatcher gets new `devsbd` verbs without a restart once a newer devsandbox's daemon runs (the first command of the new version that reaches the daemon, a dashboard, `run` or `start`, hands it over).

## `autostart`

```toml
[sandbox.triage]
autostart = true         # devsandbox starts it once per boot
# autostart = "runtime"  # the container runtime restarts it on boot
```

**`true`**: once per host boot (per config root), the host daemon's start (`devsandbox serve`, started on demand or at login), the first dashboard launch, or the first `devsandbox run` / `devsandbox start` (after it finishes), whichever comes first, starts every stopped instance of each `autostart` sandbox through the full `start` path (services, helper, `postStartCommand`). A sandbox with no instance yet gets one (`run`). Dispatcher-owned children are skipped (see below). Other commands (`ps`, `stop`, `status --json`, …) never trigger it. An instance you stop afterwards stays stopped until the next boot. If the runtime isn't reachable yet (Docker Desktop still starting, `container system start` not run), the pass is skipped and retried on the next trigger.

**`"runtime"`**: the same pass, plus the container is created with `--restart unless-stopped`, so docker/podman bring it back at boot with no devsandbox process running:

- docker: works as soon as the daemon starts on boot/login.
- podman: only with `podman-restart.service` enabled (`systemctl --user enable podman-restart.service`); devsandbox never installs units and warns at `run`.
- Apple `container`: no restart policy; warns and behaves as `true`.

The runtime restarts only the container command, so new containers carry a boot hook: on every container start it brings back the `devsbd` daemon and re-runs `postStartCommand` (as `remoteUser`, in `workspaceFolder`, with `remoteEnv`), appending its output to `/run/devsandbox/boot.log`. For these instances `devsandbox start` no longer runs `postStartCommand` from the host (`run` still does, at create). Containers created before the hook existed warn on `start`: recreate them with `devsandbox rebuild --force <instance>`. Host-side things (ssh-agent relay, port forwards, notify delivery, the control API) resume once the host daemon runs: the first `devsandbox` command that needs it starts it, or, with `devsandbox serve install`, the user's service manager does at login (docs/serve.md, *Boot start*).

Flipping `autostart` is not drift: `start` updates the restart policy of an existing container in place (a podman too old for `update --restart` warns to `rebuild --force`).

## `devsbd notify`

Available in every sandbox, no opt-in:

```
devsbd notify [--level info|warn|error] [--link URL] [--key K] [--] <msg>...
```

- Queued first: each call writes a record to a durable outbox in the container (`/var/lib/devsandbox/outbox/`, survives container restarts) and succeeds even with no host attached.
- Delivered while the **host daemon** (`devsandbox serve`) runs: it shows up in the Inbox tab (`4`) and as a desktop notification (`notify-send` on Linux, `osascript` on macOS; skipped silently when missing). The dashboard, `run` and `start` start the daemon on demand; it keeps running while a dispatcher or an `inbox = true` instance does, and otherwise exits 10 minutes after its last client (a dashboard, a port forward, …) left. `devsandbox serve install` keeps it running from login instead. Without it the queue waits. A record is acknowledged to the container only after it is saved, so a host that dies mid-delivery loses nothing: the record is resent.
- An unread notification counts in the Inbox title (`Inbox (N)`) and the yellow `✉N` on its instance row, and sits in the **Needs you** view. It is marked read when you open it, or when you leave the Inbox after it was on screen (in Needs you or All).
- `--key` threads per instance: a newer notification with the same key becomes the head of that row, and the older ones are listed under *earlier* in the thread pane beside it, so a script re-reporting "PR 123 conflicted" every poll doesn't spam.
- `--link` (http(s) only) opens with `enter` once the thread has focus. `d` dismisses a notification row with its history; `D` clears every notification (owner threads stay). History is saved in `inbox.json` next to `state.toml`, shared by every open dashboard, the host daemon and `devsandbox inbox`, and kept across restarts; a dismissal in one dashboard is gone from all of them.
- Limits: desktop popups are rate-limited per instance (a burst of 3, then one per 10 s) and a keyed notification repeating one popped in the last minute doesn't pop again; the Inbox still gets every one. The Inbox keeps 200 threads per instance (a keyed notification row counts as one, holding its newest 300 records), so a noisy instance drops its own oldest, not others'.
- Flags are recognized anywhere before `--`; everything after `--` is message.

`notify` is for one-off news from any sandbox: a run finished, a check failed. For an item that stays open until someone deals with it, and that the user may act on from the dashboard, a sandbox declaring `inbox = true` uses a thread (below).

## Dispatchers

A dispatcher is an ordinary sandbox that declares `dispatcher`. Its instances may create and manage *child* instances; every other sandbox is refused.

```toml
[sandbox.pr-dispatcher]
extends = "base"
folder = "../pr-dispatcher"      # its own repo: loop script, AGENTS.md, skills
autostart = "runtime"
dispatcher = { spawn = ["web"], max-instances = 10 }
postStartCommand = "nohup ./babysit-loop.sh >babysit.log 2>&1 &"

[sandbox.web]                    # children are ordinary instances of it
extends = "base"
folder = "../web"
```

- `spawn`: sandboxes of this config root the dispatcher may instantiate; `"*"` allows every non-dispatcher sandbox of the root, which hands the dispatcher their mounts, docker socket and other privileges: list names unless you mean that. Omitted = nothing. Children can't be dispatchers: a sandbox that declares `dispatcher` is never spawnable, even when named.
- `max-instances` (optional, default 10): cap on the dispatcher's children, stopped ones included.
- Both are re-read on every request, and neither marks instances drifted.

**Background the loop.** Lifecycle commands block the `run`/`start` that runs them, so a long-running loop in `postStartCommand` must detach itself: `nohup ./loop.sh >loop.log 2>&1 &`. The output lands in the workspace (gitignore it).

### Control commands

Run from inside a dispatcher instance:

```
devsbd ensure <sandbox> --key <key> [--branch B] [--env K=V]...
devsbd ls
devsbd branches <sandbox> [--ahead]
devsbd stop <key> [--sandbox S]
devsbd rm <key> [--sandbox S]
devsbd done <key> [--sandbox S]
devsbd exec <key> [--sandbox S] [--detach] -- <cmd>...
devsbd run ls <key> [--sandbox S]
devsbd run logs <key> <id> [--sandbox S] [--follow]
devsbd run wait <key> <id> [--sandbox S] [--timeout SECS]
devsbd run rm <key> <id> [--sandbox S] [--force]
devsbd run prune <key> [--sandbox S] [--keep N]
devsbd events [--wait SECS] [--thread KEY]
devsbd events --follow [--thread KEY]
devsbd events ack <id>...
devsbd thread ls [--feed]
```

`events`, `events ack` and `thread ls` need `inbox = true` rather than `dispatcher`; they're covered under [Inbox threads](#inbox-threads).

- `ensure` is idempotent: creates `<sandbox>-<key>` if missing, starts it if stopped, recreates it if its container is gone, and prints one JSON line: `{"name":"web-pr-1","key":"pr-1","sandbox":"web","state":"running","created":false,"started":true}` (`created`: the child didn't exist; `started`: it existed but wasn't up; both false: it was already running). `--branch` applies only at creation (ignored for an existing child). `--env` values are recorded and kept when the child is rebuilt or its container recreated; on an existing child, `--env` replaces the recorded set with exactly the given one (a variable left out is dropped; no `--env` keeps it). Every command devsandbox runs in the child uses the recorded values from then on (`devsandbox exec`, the dashboard's terminals, `devsbd exec` runs, lifecycle commands), overriding a `remoteEnv` entry of the same name. The container's own environment is only set when it's created, so processes already running, PID 1, and a hand-run `docker exec` keep the old values until `devsandbox rebuild`. `--branch` names the branch of the child's worktree: an existing local branch is checked out as is, one only on `origin` (e.g. a PR head) is fetched and checked out as a local branch tracking `origin/<branch>` (so a plain `git push` updates the PR), anything else is created from the repo's default base. A branch already checked out elsewhere (the base checkout, another instance) is an error naming where. `rm` only offers to delete a branch the child created. The name is taken literally (no `${…}` substitution, unlike the sandbox's `worktree-branch` pattern) and must be 1–200 chars of `[A-Za-z0-9._/-]`, not start with `-`, `/` or `.`, not end with `/` or `.`, and contain no `..`, `//`, component starting with `.` or ending in `.lock`; anything else is a usage error (exit 2). Without `--branch` the child's branch comes from `worktree-branch` (default `sandbox/${instance}`). `--env` may not set variables that steer what runs (denied, exit 77): `PATH`, `HOME`, `SHELL`, `USER`, `ENV`, `BASH_ENV`, `IFS`, `CDPATH`, `PS4`, `PROMPT_COMMAND`, `SSH_AUTH_SOCK`, `TMPDIR`, `GCONV_PATH`, `NODE_OPTIONS`, `RUBYOPT`, and anything starting with `LD_`, `DYLD_`, `GIT_`, `PYTHON` or `PERL5` (matched case-insensitively).
- `ls` prints a JSON array of this dispatcher's children: `name`, `sandbox`, `key`, `state` (`running` | `stopped` | `missing`), `branch`, `done`.
- `done` marks a child done (see [Done children](#done-children)) and prints `{"name","key","done":true}` as one JSON line. Idempotent: a child already done keeps the time it was first marked.
- `branches` prints a JSON array, sorted by `branch`, of the branches checked out in any worktree of `<sandbox>`'s repo (its `folder`), read live from git on the host, so a `git switch` inside an instance or a worktree made by hand shows up (`ls`'s `branch` is the one at creation). `holder` says who has it: `child` (one of this dispatcher's children), `instance` (any other instance, from any config root, including one on the folder itself), `base` (the folder's own checkout), `external` (a worktree devsandbox doesn't know); `instance` names the instance for the first two. Stopped instances still hold their branch; detached worktrees and ones whose directory is gone are left out. `--ahead` adds `ahead`, the commits on the local branch that `origin/<branch>` doesn't have (left out when `origin/<branch>` doesn't exist), and lists local branches in no worktree with `ahead > 0` as `holder: "local"`. `<sandbox>` must be in `spawn` (exit 77), but may be a dispatcher itself, since nothing is created; no `folder`, or a missing one, fails (exit 1). Read-only, so it never waits behind another dispatcher's `ensure`.

  ```json
  [
    {"branch": "fix/login", "holder": "instance", "instance": "web-2", "ahead": 2},
    {"branch": "main", "holder": "base", "ahead": 0},
    {"branch": "wip/unpushed", "holder": "local", "ahead": 3}
  ]
  ```

- `stop` / `rm` / `exec` / `run …` take the key; `--sandbox` disambiguates a key used under two sandboxes. `stop` prints `{"name","key","state":"stopped"}`, `rm` `{"name","key","removed":true}`, one JSON line each. A failing op prints no JSON: its message goes to stderr with a non-zero exit.
- `exec` starts a tracked *run* in a running child (`ensure` it first), as the child's `remoteUser` in its workspace with its `remoteEnv`. With `--detach` it prints the run id and returns; without, it prints `devsbd: run <id>` to stderr, streams the output, and exits with the run's code (`killed N` → 128+N, `lost` → 1).
- `run ls` prints one line per run, oldest first, for the newest 50 runs only: `<id> <state> <started, UTC> <argv…>`, argv cut at 200 characters (ending in `…`). Runs whose files are oversized or not regular files are left out. `run logs` prints the output so far (`--follow`: until the run ends). `run wait` prints the final state, or `running` once `--timeout` expires; it exits 0 either way.
- `run rm` deletes one run (its output included). A `running` run is refused (exit 1) unless `--force`, which first kills the run's command with everything it started (SIGKILL to its process group); the run ends as `killed 9`. `run prune` deletes the child's finished (`exited`/`killed`) and `lost` runs except the newest `--keep N` of them (default 0), never a running one, and prints `removed <count>`. "Newest" is by start time to the second; runs started in the same second are ordered arbitrarily.
- Run states: `running`, `exited N`, `killed N` (signal), `lost` (its supervisor died without recording an end, e.g. the child restarted). Runs are kept in the child under `/var/lib/devsandbox/runs/<id>/` until `run rm`/`run prune` deletes them (or the child is removed or rebuilt). The dashboard shows a child's recent runs as dim `run` rows under its processes.

Exit codes:

| code | meaning |
|---|---|
| 0 | ok |
| 1 | failed (the message says why; host-side details in the log below) |
| 2 | usage error, or an ambiguous key (pass `--sandbox`) |
| 75 | no host connected: no host daemon (`devsandbox serve`) is reachable (or the helper daemon in the container isn't running). Retry later. |
| 77 | denied: not a dispatcher (or a dispatcher's child; for `events` and `thread ls`: no `inbox = true`), sandbox not in `spawn` (or, for `ensure`, itself a dispatcher), not this dispatcher's child, `max-instances` reached, a denied `--env` name |

Control is served by the host daemon (`devsandbox serve`), which the dashboard, `run` and `start` start and which stays up while a dispatcher runs, dashboard closed or not. It can still be missing, e.g. after a reboot when the runtime restarted the dispatcher before any `devsandbox` command ran (`devsandbox serve install` closes that gap). Nothing is queued: a script must retry on 75.

### Children

- Named `<sandbox>-<key>` (key: lowercase letters, digits, `-`, starting with a letter or digit, at most 40 chars), e.g. `web-pr-123`, so you can `devsandbox exec -it web-pr-123 zsh` or attach VS Code to it.
- Always on their own git worktree, even when the folder isn't otherwise in use.
- Labelled with their owner and recorded in `state.toml`; a dispatcher only reaches its own children. The dashboard marks them with a dim `⇠ <dispatcher>` suffix, or `(orphan)` once the dispatcher is gone.
- **Kept** when the dispatcher is stopped, removed, or rebuilt. Cleaning up is the script's job (`devsbd rm`); it should remember which keys it manages and can reconcile against `devsbd ls`. Orphans are removed by hand with `devsandbox rm`.
- An `ensure` that reuses a child marked done clears the mark: the dispatcher is putting it back to work.
- Each host-side operation runs as a `devsandbox` subprocess logging to `<data-dir>/devsandbox/logs/dispatch-<unix>-<op>.log` (next to `state.toml`, e.g. `~/.local/share/devsandbox/logs/`).
- The boot pass never starts or creates children: the dispatcher `ensure`s them when it needs them. A sandbox whose only instances are children gets no extra instance either. Exception: children of an `autostart = "runtime"` sandbox carry `--restart unless-stopped`, so docker/podman bring back the ones that were running at shutdown (the runtime's rule, not devsandbox's).

### Done children

A child marked **done** is kept as is (container, worktree, runs), shown dimmed with a `✓` in the dashboard and as `Up … (done)` in `devsandbox ps`, and removed whenever someone decides to. It's how a user says "I've looked at it, carry on" without removing anything.

- Set by `devsbd done <key>` (e.g. the PR merged), a thread's `done` (below), `d` on the instance row, or `devsandbox done <instance>`. Cleared by `devsandbox undone`, `u` on the row or on the thread, or an `ensure` that reuses the child.
- `devsbd ls` reports `"done": true`. A done child is still a real container: it counts toward `max-instances`. Whether to evict done children to make room is the dispatcher's call; devsandbox never removes them.
- `start` and the boot pass leave the mark alone.

## Inbox threads

A sandbox that declares `inbox = true` (typically a dispatcher) can put **threads** in the Inbox: one row per item it tracks (a PR, an email to answer). A thread is a small **header** (title, state, status, buttons, an optional reply box) the owner re-asserts, and a **feed** under it: the owner's **messages** (markdown, key/value fields, forms), and the user's **replies**, **actions** (button presses) and **submissions** (answered forms), with one-line **markers** for done/reopen and state or status changes. The owner decides what they mean; devsandbox stores them, shows them, runs the built-in buttons and hands everything else back as **events** the owner pulls. Its own state stays the source of truth: threads are a projection of it, so losing the Inbox loses nothing.

Use `notify` for one-off news, from any sandbox. Use a thread for anything still open that the user may act on: it stays in one row, changes state instead of piling up messages, and leaves **Needs you** once it's resolved. Only instances whose sandbox declares `inbox = true` may put, remove or list threads and read their events:

```toml
[sandbox.pr-dispatcher]
dispatcher = { spawn = ["web"] }   # child ops: ensure, exec, rm, …
inbox = true                        # threads and events
```

`dispatcher` only grants the child ops; a dispatcher that wants threads declares both. Any other sandbox, a dispatcher's child included, may declare `inbox = true` to own threads of its own. Like `dispatcher`, it's re-read on every message and request and never marks instances drifted.

### `devsbd thread put`

```
devsbd thread put < thread.json
devsbd thread put --json '<json>'
devsbd thread rm <key>
devsbd thread ls
```

`put` queues the whole header in the same durable outbox as `notify`, so it works with no host daemon running and is delivered later, in order. The helper only checks JSON syntax, that `key` is a valid key and that the record fits in 64 KiB (exit 2 otherwise); the host checks the rest when it applies the put. A rejected put (or any thread message from a sandbox that doesn't declare `inbox = true`) becomes an `error` notification from your own instance in the Inbox, keyed `thread:<key>`, reading `thread put rejected: <why>` or `thread put denied: <why>`: watch for those while writing a dispatcher.

```json
{
  "key": "pr-6900",
  "title": "#6900 feat/agent-run-cost-event",
  "link": "https://github.com/owner/web/pull/6900",
  "state": "needs-you",
  "status": "review draft replies",
  "child": "pr-6900",
  "actions": [
    { "id": "code", "label": "VS Code", "host": { "vscode": { "path": "src/cost.ts", "line": 42 } } },
    { "id": "retry", "label": "Retry" },
    { "id": "done", "label": "Done", "done": true }
  ],
  "compose": { "placeholder": "Instructions for the next run", "hint": "Starts a comments run with your message" }
}
```

| field | required | meaning | limit |
|---|---|---|---|
| `key` | yes | the thread's id, per sending instance | key rules: lowercase letters, digits, `-`, starting with a letter or digit, at most 40 chars |
| `title` | yes | one line: what the thread is about | non-empty, 200 bytes |
| `state` | yes | `needs-you`, `active` or `done`: drives the Inbox views and the badges | one of the three |
| `link` | no | opened with `enter` once the thread has focus | http(s) only, 2000 bytes |
| `status` | no | free text, your lifecycle stage (`running ci`, `merged`), shown as a chip on the row | 60 bytes |
| `child` | no | for a dispatcher: the `--key` of one of its children, the instance the thread is about, which host actions and the pane's `o`/`t`/`l`/`p` target. Left out (always, for an owner without children): they target your own instance | key rules |
| `actions` | no | buttons, numbered `1`–`9` in the pane | at most 9 |
| `compose` | no | `{ "placeholder": "…", "hint": "…" }` (both optional): allow free-text replies. `hint` is one dim line under the reply box saying what sending does now; re-put it when that changes | 100 bytes each |

`title` and `status` take inline markdown (code, emphasis, links). Control characters in anything a container sends are stripped before it's stored.

Each action:

| field | meaning | limit |
|---|---|---|
| `id` | what your event carries back | lowercase letters, digits, `-`, 1–40 chars, unique in the thread |
| `label` | the button text | non-empty, 60 bytes |
| `host` | a built-in verb the dashboard runs at once (table below; `devsandbox inbox` and API clients refuse host-only buttons). Absent: the button only sends you an `action` event | exactly one verb |
| `notify` | `true`: also send an `action` event when the button has a `host` verb | |
| `done` | `true`: set the thread done, mark its `child` done, and send one `done` event carrying this `id` (instead of an `action` event) | |

Host verbs, written `"host": { "<verb>": { …args } }` (`{}` for none):

| verb | args | does (same as) |
|---|---|---|
| `vscode` | `path?`, `line?`, `col?` | VS Code attached to the target; with `path` (relative to the target's workspace folder), opens that file at the line (`o`, `devsandbox vscode --goto`) |
| `terminal` | | a dashboard terminal in the target (`t`) |
| `logs` | | the target's container log (`l`) |
| `forward` | `port` (1–65535) | forwards that port of the target to the host (`p`, Ports tab) |
| `open` | `url` (http/https) | opens the URL on the host |
| `rm` | | `devsandbox rm` of the target, with the CLI's own confirm |

The target is the thread's `child`, or your own instance when there is no `child`; a host action can't reach any other instance, and a `child` that isn't one of yours (or is gone) makes the button fail with a status line. A stopped target is started first for `vscode`, `terminal` and `forward` (the button shows `(stopped)`, the status line `starting <name>…`); `logs` reads a stopped container's log as is. There is no arbitrary host command. A `vscode.path` must be relative, without `..`, empty components or a leading `~`, at most 400 bytes.

Every text field refuses control characters other than newline, and unknown fields are rejected.

**Re-assert every pass.** A put that changes nothing is dropped by the host: no write, no unread mark, no marker, no popup. So the intended use is to put every open thread on every pass, from your own state, rather than tracking what you already sent. What a put does when it does change something:

- `state` or `status` changed: a marker in the feed (consecutive status-only changes collapse into one, `running comments → review drafts`), and the thread is unread again.
- `state` entered `needs-you` (or a new thread starts there): a desktop popup, rate-limited like `notify`.
- `title`, `link`, `child`, `actions`, `compose` changed: updated silently. `actions` and `compose` are replaced wholesale.

Your put always wins: if the user marked a thread done and your next pass puts it as `needs-you` again, it's back in Needs you. Handle the `done` event (below) and put the thread as `done` from then on.

While no host daemon runs, a queued `put` replaces any older queued `put` for the same key, and an `rm` replaces queued `put`s, `rm`s, `send`s and `withdraw`s for its key (notifications are never coalesced), so a dispatcher re-asserting every few minutes overnight doesn't pile up files. A replacing record keeps the queue position of the one it replaced, so the order you sent things in is the order they arrive.

`thread rm <key>` drops one of your threads, its pending events with it. `thread ls` prints a JSON array of your live threads in `put` shape (the `state` reflects the user's done/reopen too), for an owner that lost its own state; `thread ls --feed` adds each thread's `messages` (below). Both need the host daemon (exit 75 otherwise). Threads are tied to your instance's id, not its name: they survive stop, restart and rebuild. `devsandbox rm` of the owner archives them (read-only, shown only in **All**). Done and archived threads are dropped 14 days after their last change, unless a done thread still has events you haven't acked; past the 200-per-instance cap, archived threads go first, then done ones. Each thread's feed keeps 300 items: past that, the oldest markers go first, then the oldest messages without an open form.

### Messages (`devsbd thread send`)

The header (`put`) says where a thread stands; **messages** tell the story under it: what a run did, what it needs from the user. They replace the header's old `message` field, which a put now rejects.

```sh
devsbd thread send < message.json
devsbd thread send --json '<json>'
devsbd thread withdraw <thread> <id>
```

```json
{
  "thread": "pr-6900",
  "id": "run-1791277117",
  "blocks": [
    { "type": "markdown", "text": "Addressed 3 of 4 comments, **1 needs your call**." },
    { "type": "fields", "items": [{ "label": "Head", "value": "36b13d41" }, { "label": "CI", "value": "green" }] }
  ]
}
```

- **Put the thread first.** A send to a thread you haven't put is rejected (`thread send rejected: no thread `<key>`; put it first`).
- **`id` is your idempotency key** within the thread: derive it from your own state (a run id), so a restarted dispatcher can re-send everything for free. The same id with the same blocks is a no-op; with other blocks it **replaces the message in place**, tagged "edited", without marking the thread unread or popping up. A new id is appended to the feed (the feed keeps first-send order), marks the thread unread, and pops up when the thread is `needs-you`.
- **`withdraw`** takes a message back: it stays in the feed as a dim "withdrawn" line so the story stays readable. Withdrawing an id the host never saw is a silent no-op. Sending a withdrawn id again brings it back in place, tagged "edited".
- Like puts, sends and withdraws queue in the outbox with no host daemon running; a newer send or withdraw of the same `(thread, id)` replaces the queued one.

| field | meaning | limit |
|---|---|---|
| `thread` | the key of one of your threads | key rules |
| `id` | the message's id, unique in the thread | lowercase letters, digits, `-`, 1–60 chars |
| `blocks` | what the message shows, in order | 1–8 blocks; the whole body at most 48 KiB |

Blocks, by `type`:

| type | fields | renders as | limit |
|---|---|---|---|
| `markdown` | `text` | markdown, like a `notify` body | non-empty, 16 KiB |
| `fields` | `items`: `[{ "label", "value" }]` | aligned `label  value` rows | 1–20 items; one-line `label` (non-empty, 60 bytes) and `value` (200 bytes) |
| `form` | see [Forms](#forms) | questions the user answers and submits | one per message |

Unknown fields and unknown block types are rejected, as are control characters other than newline (and newlines in a field). The helper checks the JSON syntax, `thread`, `id` and the 48 KiB budget before queuing (exit 2, nothing queued); the host checks the rest and reports a reject as an `error` notification, `thread send rejected: <why>`, like a bad put. Markdown blocks render like a `notify` body (headings, lists, code, quotes, links, tables).

`thread ls --feed` lists each thread's messages as `"messages": [{ "id", "at", "blocks", "edited", "withdrawn", "form"? }]` (your messages only, not the user's replies), `blocks` in the shape you sent them, so a dispatcher that lost its state can see what it already said. On an older helper `thread send` fails with `unknown verb`; check with `devsbd features | grep -qx thread-send` (an older helper has no `features` either and prints nothing to stdout).

### Forms

A `form` block asks the user questions they answer and submit once; you get one `submit` event with every answer. Labels below are the PR babysitter's own wording:

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

| field | meaning | limit |
|---|---|---|
| `id` | the form's id | lowercase letters, digits, `-`, 1–60 chars |
| `title` | optional heading | one line, 200 bytes |
| `submit` | the submit button's label, default `Submit` | one line, 40 bytes |
| `questions` | what to ask, in order | 1–30 |

Every question has an `id` (unique in the form, same rule as the form's), a `label` (inline markdown, one line, 200 bytes), an optional `context` (markdown shown above the input, 4 KiB) and `required` (default `true` for `choice` and `confirm`, `false` for `text`). By `type`:

| type | fields | answer |
|---|---|---|
| `choice` | `options`: 1–20 `{ "id", "label", "description"? }` (one-line `label` 200 bytes, `description` 4 KiB); `multiple` (default `false`); `default`: an option id, or an array of ids when `multiple` | the option id; an array of ids when `multiple` |
| `text` | `placeholder` (200 bytes), `default` (prefilled and editable: how you offer a draft for editing), `multiline` (default `false`), `max` (1–8192 bytes, default 2048) | the text |
| `confirm` | `yes` / `no` labels (one line, 40 bytes), `default` (`true`/`false`) | `true` or `false` |

One form per message. Defaults must be valid answers (known option ids, an array exactly when `multiple`, text within `max`, no newline in a single-line default). Unknown fields and question types are rejected like any other bad send.

**Lifecycle**, kept by the host:

- A form is **open** from the send that brings it. A message bringing a form into the feed (a new message, or a re-send adding one) marks the thread unread and pops up when the thread is `needs-you`.
- While open, the user's half-filled answers are a **draft** stored with the message in the host, so any dashboard or API client sees the same one. Re-sending the message (same `id`) with a changed form replaces the form and keeps only the draft answers that still fit (same question id, same type, a valid value). Re-sending it without the form, or withdrawing the message, **withdraws** the form.
- **Submitted** once, it is **frozen**: re-sending its message can still change the other blocks (tagged "edited"), but the form block you send is ignored and the stored form keeps the user's answers. Re-sending your original message stays a no-op, so a re-asserting dispatcher changes nothing. A form can't be submitted twice: send a new message for another round.
- Like the rest of the feed, the 300-items-per-thread cap never drops a message whose form is still open.

The submission arrives as one event (below), `answers` keyed by question id with **every** question present and defaults filled in: you never merge defaults yourself. An optional text left empty is `""`, an optional `multiple` choice with no pick `[]`, an optional choice or confirm left unanswered `null`.

`thread ls --feed` gives each message that has carried a form a `"form": { "id", "state", "answers"? }`: `state` is `open`, `submitted` or `withdrawn`, and `answers` (only when submitted) is what the `submit` event carried, so a dispatcher that lost its state, or acked nothing yet, can recover the answers.

The dashboard pins open forms under the thread's header and answers them in place (docs/tui.md, *Forms*); `devsandbox inbox submit` answers from a shell (docs/inbox-cli.md), API clients with `inbox.form.saveDraft` and `inbox.form.submit` (docs/api.md).

### Events

What the user does on a thread comes back as an event, held by the host until you ack it:

```
devsbd events [--wait SECS] [--thread KEY]   # JSON lines, oldest first
devsbd events --follow [--thread KEY]        # the same lines, pushed until the stream ends
devsbd events ack <id>...
```

```json
{"id":"e-1790900001-3f2a","thread":"pr-6900","kind":"action","action":"post","at":"2026-10-02T12:00:01Z"}
{"id":"e-1790900042-77c1","thread":"pr-6900","kind":"reply","text":"Also rename the event to agent_run.cost","at":"2026-10-02T12:00:42Z"}
{"id":"e-1790900050-0b9e","thread":"pr-6900","kind":"done","action":"done","at":"2026-10-02T12:00:50Z"}
{"id":"e-1790900061-5d10","thread":"pr-6900","kind":"submit","message":"run-1791277117","form":"drafts","answers":{"c-3726888733":"post","c-3726888733-text":"Already batched…","notes":""},"at":"2026-10-02T12:01:01Z"}
```

| `kind` | sent when | extra field |
|---|---|---|
| `action` | `1`–`9` on a button without `host`, or with `notify: true` | `action`: the button's `id` |
| `reply` | `r` in the pane, then `enter` (trimmed, at most 2000 chars) | `text` |
| `done` | `d` on the thread (list or pane), or a `done: true` button | `action`: the button's `id`, only from a button |
| `reopen` | `u` in the pane on a done thread (it goes back to `active`) | |
| `submit` | a form was submitted ([Forms](#forms)) | `message`, `form`: the message's and the form's ids; `answers`: every question's answer |

The keys are the dashboard's; `devsandbox inbox reply|act|submit|done|reopen` (docs/inbox-cli.md) and API clients (docs/api.md) produce the same events, and you can't tell which one the user used.

`thread` is the key of the thread it happened on (the `key` you put it with). `at` is RFC 3339 UTC, by the host's clock. `d` and `u` only send an event when they change something (`d` on a done thread, `u` on a live one, do nothing). Each event also shows in the feed (the reply, the action, the folded submission, a done/reopen marker), and the pane shows how many are still waiting for you. Archived threads take no events.

- **At least once.** `events` prints every pending event, not just new ones, until they're acked. Ack after you've saved what the event changed in your own state, and make handlers idempotent: a crash between the two replays the event. The `id` tells two deliveries of one event apart.
- `events ack` prints how many it dropped; an id that's unknown, already acked or another instance's is ignored, so a retried ack is harmless. A malformed id is a usage error (exit 2).
- `--wait SECS` (at most 300) returns as soon as anything is pending, or prints nothing after `SECS`. Use it in place of your loop's sleep and a click gets handled within seconds. Without `--wait`, `events` answers at once.
- `--thread KEY` answers only that thread's events, and with `--wait` waits for one there (an event on another of your threads doesn't end the wait). A thread you haven't put (yet) just has none: an empty answer, not an error. The others stay pending for an unfiltered `events`.
- Events are a control command: they need the host daemon (exit 75 otherwise). An event only exists because the user answered (in a dashboard, `devsandbox inbox` or an API client), so there's nothing to miss meanwhile; they wait in `inbox.json`.
- Each thread keeps at most 100 unacked events (the oldest is dropped beyond that), and one `events` answer stops at about 512 KiB: ack what you got and call again for the rest.
- You only ever see and ack events of your own threads.

### Following events (`--follow`)

`devsbd events --follow` keeps one stream open to the host instead of polling: it prints every pending event first, then each new one the moment it's enqueued, as JSON lines on stdout, until the stream ends. Each event comes at most once per stream; acks still go through `devsbd events ack`, so a follower that restarts gets everything unacked again (at least once, as above). `--thread KEY` filters as for `events`; `--follow` with `--wait` is a usage error (exit 2).

Besides events, the stream carries two lines of its own:

- `{"kind":"ping"}` after 30 s without a line. Ignore it; it keeps the stream warm and lets both ends notice a dead one (90 s without a byte ends it).
- `{"kind":"replaced"}` when another `--follow` from the same instance starts: only the newest follower gets events, so two copies of a dispatcher can't both act on one click. It's the last line, and the old follower exits 0. Stop that copy; don't restart it.

Exit codes: 0 after `replaced`; 75 when the stream ends otherwise (no host daemon, the bridge dropped, the daemon in the container died or restarted): reconnect, with a backoff; 2 for a usage error, or a host too old to know `--follow` (fall back to `events --wait`); 77 without `inbox = true`. `devsbd features` lists `events-follow` on helpers that have it. A follower counts as a host daemon client, so it keeps the daemon from idling out.

The recommended loop: follow, apply each event, save, ack; restart on 75 with a backoff; fall back to `--wait` on 2; stop on `replaced`. POSIX sh can't read the exit code of a pipeline's first command, so the stream goes through a FIFO:

```sh
handle() {  # one event line on stdin: apply it, save, then ack
  ev=$(cat)
  # ... apply $ev to your own state and save it ...
  devsbd events ack "$(printf '%s' "$ev" | jq -r .id)" >/dev/null
}

fifo=$(mktemp -u); mkfifo "$fifo"
backoff=1
while :; do
  devsbd events --follow >"$fifo" & pid=$!
  while IFS= read -r ev; do
    case $(printf '%s' "$ev" | jq -r .kind) in
      ping|replaced) ;;
      *) printf '%s' "$ev" | handle; backoff=1 ;;
    esac
  done <"$fifo"
  wait "$pid"; rc=$?
  case $rc in
    0)  exit 0 ;;                                 # replaced: a newer copy runs
    2)  exec ./poll-loop.sh ;;                    # old host: use `events --wait`
    75) sleep "$backoff"; [ "$backoff" -lt 60 ] && backoff=$((backoff * 2)) ;;
    *)  sleep 60 ;;
  esac
done
```

### Sample: a thread loop

For a sandbox with `inbox = true`. One file per item in `.items/` holds its state; the loop re-puts every thread and re-sends its message (both no-ops when unchanged), then waits for clicks. POSIX sh, with `jq`.

```sh
#!/bin/sh
set -u
ITEMS=.items; TODO=.todo      # this dispatcher's own state; gitignored
mkdir -p "$ITEMS" "$TODO"

put() {  # <key> <state>
  jq -cn --arg k "$1" --arg s "$2" '{
    key: $k, title: "Item \($k)", state: $s, child: $k,
    actions: [
      {id: "open", label: "Open the draft", host: {vscode: {path: "DRAFT.md", line: 1}}},
      {id: "send", label: "Send it"},
      {id: "skip", label: "Skip", done: true}
    ],
    compose: {placeholder: "What should change?", hint: "Queues your note for the next draft"}
  }' | devsbd thread put
}

say() {  # <key> <message id> <markdown>
  jq -cn --arg k "$1" --arg i "$2" --arg m "$3" \
    '{thread: $k, id: $i, blocks: [{type: "markdown", text: $m}]}' | devsbd thread send
}

while :; do
  # ... checks and runs update $ITEMS/<key> and work off $TODO/<key> here ...

  # Re-assert every thread; unchanged ones cost nothing.
  for f in "$ITEMS"/*; do
    [ -e "$f" ] || continue
    put "${f##*/}" "$(cat "$f")"
    say "${f##*/}" draft "Draft ready in \`DRAFT.md\`."
  done

  # Sleep up to 5 min, or until the user clicks.
  out=$(devsbd events --wait 300); rc=$?
  if [ "$rc" -eq 75 ]; then sleep 60; continue; fi   # no host daemon
  [ "$rc" -eq 0 ] && [ -n "$out" ] || continue
  printf '%s\n' "$out" | while IFS= read -r ev; do
    key=$(printf '%s' "$ev" | jq -r .thread)
    case $(printf '%s' "$ev" | jq -r '.kind + ":" + (.action // "")') in
      action:send) echo send >>"$TODO/$key"; echo active >"$ITEMS/$key" ;;
      reply:)      printf '%s' "$ev" | jq -r .text >>"$TODO/$key" ;;
      done:*)      echo done >"$ITEMS/$key" ;;
      reopen:)     echo active >"$ITEMS/$key" ;;
    esac
  done
  # Saved above, so acking is safe; a crash before this replays the events.
  devsbd events ack $(printf '%s\n' "$out" | jq -r .id) >/dev/null
done
```

Writing `done` into `.items/` matters: the next pass puts the thread as `done`, so the user's click sticks. The `done: true` button has already marked the child done; call `devsbd done <key>` yourself for items you close on your own (e.g. the PR merged).

## Patterns

devsandbox takes no stand; pick per use case.

- **One instance per item**: `--key pr-123`. Easy to inspect (the PR has its own container, worktree, agent session). A later stage calls `ensure` with the same key and finds the same instance. Retire with `stop` until the item's last stage is done, then `rm`.
- **Worker pool**: keys `worker-1`, `worker-2`, …; the script assigns items to workers. Resources stay bounded regardless of item count; per-item memory is the script's concern.
- **`stop` vs `rm`**: `stop` keeps the container, worktree and any uncommitted work; `rm` deletes container, worktree and state entry.
- **Idle children**: `stop` a child between runs and `ensure` it before the next `exec`, so nothing runs while nothing needs doing.
- **Skip items whose branch is checked out elsewhere**: before acting on a PR (pushing, `gh pr update-branch`, starting an agent), look its head branch up in `devsbd branches <sandbox> --ahead` and skip it when anything but its own child holds it, `local` included. Otherwise a checkout left behind `origin` can later force-push over the dispatcher's work. Call it once per repo per pass, not per item.

## Sample: PR babysit dispatcher

Polls open PRs, runs a cheap check per PR, and starts an agent in the PR's own child only when needed; asks for a human with `devsbd notify`. POSIX sh, `gh` authenticated in the dispatcher.

```sh
#!/bin/sh
# babysit-loop.sh, started from postStartCommand:
#   nohup ./babysit-loop.sh >babysit.log 2>&1 &
set -u
REPO=owner/web
STATE=.babysit            # key -> run id; gitignored
mkdir -p "$STATE"

# Run a control command, retrying while no host daemon runs (exit 75).
ctl() {
  while :; do
    "$@"; rc=$?
    [ "$rc" -eq 75 ] || return "$rc"
    sleep 60
  done
}

# Cheap check, no agent: conflicted or behind its base.
needs_attention() {
  case $(gh pr view "$1" -R "$REPO" --json mergeStateStatus -q .mergeStateStatus </dev/null) in
    DIRTY|BEHIND) return 0 ;;
    *) return 1 ;;
  esac
}

while :; do
  gh pr list -R "$REPO" --json number,headRefName -q '.[] | "\(.number) \(.headRefName)"' >"$STATE/open"

  while read -r num branch; do
    key="pr-$num"
    # An agent already on it?
    if [ -f "$STATE/$key" ] &&
       [ "$(ctl devsbd run wait "$key" "$(cat "$STATE/$key")" --timeout 0)" = running ]; then
      continue
    fi
    needs_attention "$num" || continue
    ctl devsbd ensure web --key "$key" --branch "$branch" --env PR_NUMBER="$num" >/dev/null || {
      devsbd notify --level error --key "$key" "PR $num: cannot start a child (exit $?)"
      continue
    }
    ctl devsbd run prune "$key" --keep 5 >/dev/null   # old runs' logs
    id=$(ctl devsbd exec "$key" --detach -- sh -c \
      'zidane -p "Rebase this PR on its base, fix conflicts and failing checks, push. If a human decision is needed, run: devsbd notify --level warn --key pr-$PR_NUMBER <why>"') || continue
    echo "$id" >"$STATE/$key"
  done <"$STATE/open"

  # Retire children whose PR is no longer open.
  for f in "$STATE"/pr-*; do
    [ -e "$f" ] || continue
    key=${f##*/}
    grep -q "^${key#pr-} " "$STATE/open" && continue
    ctl devsbd rm "$key" && rm -f "$f"
  done

  sleep 300
done
```

Notes: `--branch "$branch"` puts the child's worktree on the PR head, tracking `origin/<branch>`, so the agent's `git push` updates the PR. That only works for PRs whose head is on `origin`: a fork's branch isn't, and would come out as a fresh branch of that name off the default base. `--env` values are set when the child is created and kept across its rebuilds; a later `ensure` with different values replaces them for everything devsandbox runs in the child from then on (already-running processes keep the old ones until a rebuild). `devsbd notify` is also available inside the children, so the agent can ask for help directly.

## Known limitations

- **Non-root `containerUser` and the boot hook**: the hook runs as the container's user, so it can't start the daemon (or switch to `remoteUser`) when that user isn't root. The daemon comes back on the next host `start`.
- **Supplementary groups**: when the boot hook switches from root to `remoteUser`, the user's supplementary groups are dropped.
- **Zombies**: one zombie process per container start unless the sandbox sets `init = true`.
- **Runs are kept until deleted**: `/var/lib/devsandbox/runs/` grows until the child is removed or rebuilt; clean up with `devsbd run prune <key> [--keep N]` (or `run rm` for one run).
- **Host-side limits**: the host serves at most 8 notify/control requests per instance at once (more are refused and retried, or fail with "host disconnected"). A dispatched `ensure`/`stop`/`rm` is killed after 30 min (with its docker/git children) and answers `timed out after 30 min`; a run command (`exec`, `run ls|logs|rm|prune`) after 5 min, `run wait` after its 300 s cap plus 30 s; run-command output over 1 MiB is an error, not a cut-off answer.
- **The host daemon must be running** (`devsandbox serve`, started by the dashboard, `run` and `start`) for notifications to be delivered and for control commands to succeed (exit 75 otherwise). Notifications queue; control requests don't.
- **Host git refuses repos with command-running config**: every sandbox can write the repo's `.git` (worktree instances share the base repo's), so host git — `worktree add`/`remove`, `fetch`, `branch -D`, which `ensure` and `rm` trigger — runs with hooks and `core.fsmonitor` disabled and first checks the repo's local config files (`.git/config`, `config.worktree`s) against an allowlist of keys that can't run commands. Anything else (`core.sshCommand`, `filter.*`, `include.path`, `credential.*`, an `ext::` remote URL, …) or an `objects/info/alternates` file makes the command fail with `refusing to run git on <repo>: … sets <key>`. A sandbox may have written it: review the key and remove it (`git config --unset <key>` after checking it is yours); put personal settings in `~/.gitconfig`, which isn't checked.
