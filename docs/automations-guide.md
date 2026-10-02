# Automations: user guide

Sandboxes that come up on their own after a boot, tell you when they need you, and spawn their own child instances to do work. devsandbox provides the plumbing only; what to watch and when to act lives in a script you write. Design and internals: [`docs/automations.md`](automations.md) and [`docs/inbox-threads.md`](inbox-threads.md).

Everything here except `autostart = true` needs the embedded `devsbd` helper (release archives, npm package, local builds after `scripts/build-devsbd.sh`; not `cargo install`). Inside a sandbox it is on `PATH` as `devsbd` (a best-effort `/usr/local/bin/devsbd` symlink; the binary is `/run/devsandbox/bin/devsbd`). An open dashboard updates the helper of every running instance it bridges, so a running dispatcher gets new `devsbd` verbs without a restart once you open the dashboard of a newer devsandbox.

## `autostart`

```toml
[sandbox.triage]
autostart = true         # devsandbox starts it once per boot
# autostart = "runtime"  # the container runtime restarts it on boot
```

**`true`**: once per host boot (per config root), the first dashboard launch, or the first `devsandbox run` / `devsandbox start` (after it finishes), starts every stopped instance of each `autostart` sandbox through the full `start` path (services, helper, `postStartCommand`). A sandbox with no instance yet gets one (`run`). Dispatcher-owned children are skipped (see below). Other commands (`ps`, `stop`, `status --json`, …) never trigger it. An instance you stop afterwards stays stopped until the next boot. If the runtime isn't reachable yet (Docker Desktop still starting, `container system start` not run), the pass is skipped and retried on the next trigger.

**`"runtime"`**: the same pass, plus the container is created with `--restart unless-stopped`, so docker/podman bring it back at boot with no devsandbox process running:

- docker: works as soon as the daemon starts on boot/login.
- podman: only with `podman-restart.service` enabled (`systemctl --user enable podman-restart.service`); devsandbox never installs units and warns at `run`.
- Apple `container`: no restart policy; warns and behaves as `true`.

The runtime restarts only the container command, so new containers carry a boot hook: on every container start it brings back the `devsbd` daemon and re-runs `postStartCommand` (as `remoteUser`, in `workspaceFolder`, with `remoteEnv`), appending its output to `/run/devsandbox/boot.log`. For these instances `devsandbox start` no longer runs `postStartCommand` from the host (`run` still does, at create). Containers created before the hook existed warn on `start`: recreate them with `devsandbox rebuild --force <instance>`. Host-side things (ssh-agent relay, port forwards, notify delivery, the control API) resume when a devsandbox process connects.

Flipping `autostart` is not drift: `start` updates the restart policy of an existing container in place (a podman too old for `update --restart` warns to `rebuild --force`).

## `devsbd notify`

Available in every sandbox, no opt-in:

```
devsbd notify [--level info|warn|error] [--link URL] [--key K] [--] <msg>...
```

- Queued first: each call writes a record to a durable outbox in the container (`/var/lib/devsandbox/outbox/`, survives container restarts) and succeeds even with no host attached.
- Delivered while the **dashboard is open**: it shows up in the Inbox tab (`4`) and as a desktop notification (`notify-send` on Linux, `osascript` on macOS; skipped silently when missing). One-shot CLI commands don't deliver; the queue waits for the next dashboard. A record is acknowledged to the container only after it is saved, so a dashboard that dies mid-delivery loses nothing: the record is resent.
- An unread notification counts in the Inbox title (`Inbox (N)`) and the yellow `✉N` on its instance row, and sits in the **Needs you** view. It is marked read when you open it, or when you leave the Inbox after it was on screen (in Needs you or All).
- `--key` threads per instance: a newer notification with the same key becomes the head of that row, and the older ones are listed under *earlier* in the thread pane beside it, so a script re-reporting "PR 123 conflicted" every poll doesn't spam.
- `--link` (http(s) only) opens with `enter` once the thread has focus. `d` dismisses a notification row with its history; `D` clears every notification (dispatcher threads stay). History is saved in `inbox.toml` next to `state.toml`, shared by every open dashboard and kept across restarts; a dismissal in one dashboard is gone from all of them.
- Limits: desktop popups are rate-limited per instance (a burst of 3, then one per 10 s) and a keyed notification repeating one popped in the last minute doesn't pop again; the Inbox still gets every one. The Inbox keeps 200 items per instance (notifications, thread timelines and pending events together), so a noisy instance drops its own oldest, not others'.
- Flags are recognized anywhere before `--`; everything after `--` is message.

`notify` is for one-off news from any sandbox: a run finished, a check failed. For an item that stays open until someone deals with it, and that the user may act on from the dashboard, a dispatcher uses a thread (below).

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
devsbd events [--wait SECS]
devsbd events ack <id>...
devsbd thread ls
```

`events`, `events ack` and `thread ls` are covered under [Inbox threads](#inbox-threads).

- `ensure` is idempotent: creates `<sandbox>-<key>` if missing, starts it if stopped, recreates it if its container is gone, and prints the instance name. `--branch` applies only at creation (ignored for an existing child). `--env` values are recorded and kept when the child is rebuilt or its container recreated; on an existing child, `--env` replaces the recorded set with exactly the given one (a variable left out is dropped; no `--env` keeps it). Every command devsandbox runs in the child uses the recorded values from then on (`devsandbox exec`, the dashboard's terminals, `devsbd exec` runs, lifecycle commands), overriding a `remoteEnv` entry of the same name. The container's own environment is only set when it's created, so processes already running, PID 1, and a hand-run `docker exec` keep the old values until `devsandbox rebuild`. `--branch` names the branch of the child's worktree: an existing local branch is checked out as is, one only on `origin` (e.g. a PR head) is fetched and checked out as a local branch tracking `origin/<branch>` (so a plain `git push` updates the PR), anything else is created from the repo's default base. A branch already checked out elsewhere (the base checkout, another instance) is an error naming where. `rm` only offers to delete a branch the child created. The name is taken literally (no `${…}` substitution, unlike the sandbox's `worktree-branch` pattern) and must be 1–200 chars of `[A-Za-z0-9._/-]`, not start with `-`, `/` or `.`, not end with `/` or `.`, and contain no `..`, `//`, component starting with `.` or ending in `.lock`; anything else is a usage error (exit 2). Without `--branch` the child's branch comes from `worktree-branch` (default `sandbox/${instance}`). `--env` may not set variables that steer what runs (denied, exit 77): `PATH`, `HOME`, `SHELL`, `USER`, `ENV`, `BASH_ENV`, `IFS`, `CDPATH`, `PS4`, `PROMPT_COMMAND`, `SSH_AUTH_SOCK`, `TMPDIR`, `GCONV_PATH`, `NODE_OPTIONS`, `RUBYOPT`, and anything starting with `LD_`, `DYLD_`, `GIT_`, `PYTHON` or `PERL5` (matched case-insensitively).
- `ls` prints a JSON array of this dispatcher's children: `name`, `sandbox`, `key`, `state` (`running` | `stopped` | `missing`), `branch`, `done`.
- `done` marks a child done (see [Done children](#done-children)) and prints its name. Idempotent: a child already done keeps the time it was first marked.
- `branches` prints a JSON array, sorted by `branch`, of the branches checked out in any worktree of `<sandbox>`'s repo (its `folder`), read live from git on the host, so a `git switch` inside an instance or a worktree made by hand shows up (`ls`'s `branch` is the one at creation). `holder` says who has it: `child` (one of this dispatcher's children), `instance` (any other instance, from any config root, including one on the folder itself), `base` (the folder's own checkout), `external` (a worktree devsandbox doesn't know); `instance` names the instance for the first two. Stopped instances still hold their branch; detached worktrees and ones whose directory is gone are left out. `--ahead` adds `ahead`, the commits on the local branch that `origin/<branch>` doesn't have (left out when `origin/<branch>` doesn't exist), and lists local branches in no worktree with `ahead > 0` as `holder: "local"`. `<sandbox>` must be in `spawn` (exit 77), but may be a dispatcher itself, since nothing is created; no `folder`, or a missing one, fails (exit 1). Read-only, so it never waits behind another dispatcher's `ensure`.

  ```json
  [
    {"branch": "fix/login", "holder": "instance", "instance": "web-2", "ahead": 2},
    {"branch": "main", "holder": "base", "ahead": 0},
    {"branch": "wip/unpushed", "holder": "local", "ahead": 3}
  ]
  ```

- `stop` / `rm` / `exec` / `run …` take the key; `--sandbox` disambiguates a key used under two sandboxes.
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
| 75 | no host connected: the dashboard isn't open (or the helper daemon isn't running). Retry later. |
| 77 | denied: not a dispatcher (or a dispatcher's child), sandbox not in `spawn` (or, for `ensure`, itself a dispatcher), not this dispatcher's child, `max-instances` reached, a denied `--env` name |

Control is served only while the devsandbox **dashboard** is open (two open dashboards are fine: each request goes to one of them). Nothing is queued: a script must retry on 75.

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

A dispatcher can put **threads** in the Inbox: one row per item it tracks (a PR, an email to answer), with a state, a status, a message, buttons and an optional reply box. The dispatcher owns what they mean; devsandbox stores them, shows them, runs the built-in buttons and hands everything else back as **events** the dispatcher pulls. Its own state stays the source of truth: threads are a projection of it, so losing the Inbox loses nothing.

Use `notify` for one-off news, from any sandbox. Use a thread for anything still open that the user may act on: it stays in one row, changes state instead of piling up messages, and leaves **Needs you** once it's resolved. Only instances whose sandbox declares `dispatcher` may put threads.

### `devsbd thread put`

```
devsbd thread put < thread.json
devsbd thread put --json '<json>'
devsbd thread rm <key>
devsbd thread ls
```

`put` queues the whole thread in the same durable outbox as `notify`, so it works with no dashboard open and is delivered later, in order. The helper only checks JSON syntax, that `key` is a valid key and that the record fits in 64 KiB (exit 2 otherwise); the dashboard checks the rest when it applies the put. A rejected put (or any thread message from a sandbox that doesn't declare `dispatcher`) becomes an `error` notification from your own instance in the Inbox, keyed `thread:<key>`, reading `thread put rejected: <why>` or `thread put denied: <why>`: watch for those while writing a dispatcher.

```json
{
  "key": "pr-6900",
  "title": "#6900 feat/agent-run-cost-event",
  "link": "https://github.com/owner/web/pull/6900",
  "state": "needs-you",
  "status": "review draft replies",
  "child": "pr-6900",
  "message": "4 review comments checked, no code change needed. Draft replies are ready; nothing posted.",
  "actions": [
    { "id": "open-draft", "label": "Open draft", "host": { "vscode": { "path": ".dispatcher/pr-6900-replies.md", "line": 1 } } },
    { "id": "post", "label": "Post replies" },
    { "id": "done", "label": "Done", "done": true }
  ],
  "reply": { "placeholder": "Instructions for the next run" }
}
```

| field | required | meaning | limit |
|---|---|---|---|
| `key` | yes | the thread's id, per sending instance | key rules: lowercase letters, digits, `-`, starting with a letter or digit, at most 40 chars |
| `title` | yes | one line: what the thread is about | non-empty, 200 bytes |
| `state` | yes | `needs-you`, `active` or `done`: drives the Inbox views and the badges | one of the three |
| `link` | no | opened with `enter` once the thread has focus | http(s) only, 2000 bytes |
| `status` | no | free text, your lifecycle stage (`running ci`, `merged`), shown as a chip on the row | 60 bytes |
| `child` | no | key of one of your children: the instance the thread is about, which host actions and the pane's `o`/`t`/`l`/`p` target | key rules |
| `message` | no | the current explanation: what happened, what you expect from the user | 4000 bytes |
| `actions` | no | buttons, numbered `1`–`9` in the pane | at most 9 |
| `reply` | no | `{ "placeholder": "…" }` (placeholder optional): allow free-text replies | placeholder 100 bytes |

The `message` (and a `notify` body) is rendered as markdown in the thread pane; `title` and `status` take inline markdown (code, emphasis, links). Control characters in anything a container sends are stripped before it's stored.

Each action:

| field | meaning | limit |
|---|---|---|
| `id` | what your event carries back | lowercase letters, digits, `-`, 1–40 chars, unique in the thread |
| `label` | the button text | non-empty, 60 bytes |
| `host` | a built-in verb the dashboard runs at once (table below). Absent: the button only sends you an `action` event | exactly one verb |
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

The target is the thread's `child`, or your own instance when there is no `child`; a host action can't reach any other instance, and a `child` that isn't one of yours (or is gone) makes the button fail with a status line. There is no arbitrary host command. A `vscode.path` must be relative, without `..`, empty components or a leading `~`, at most 400 bytes.

Every text field refuses control characters other than newline, and unknown fields are rejected.

**Re-assert every pass.** A put that changes nothing is dropped by the dashboard: no write, no unread mark, no timeline entry, no popup. So the intended use is to put every open thread on every pass, from your own state, rather than tracking what you already sent. What a put does when it does change something:

- `message`, `state` or `status` changed: one timeline entry each, and the thread is unread again.
- `state` entered `needs-you` (or a new thread starts there): a desktop popup, rate-limited like `notify`.
- `title`, `link`, `child`, `actions`, `reply` changed: updated silently. `actions` and `reply` are replaced wholesale.

Your put always wins: if the user marked a thread done and your next pass puts it as `needs-you` again, it's back in Needs you. Handle the `done` event (below) and put the thread as `done` from then on.

While no dashboard is open, a queued `put` or `rm` for a key replaces any older queued `put`/`rm` for the same key (notifications are never coalesced), so a dispatcher re-asserting every few minutes overnight doesn't pile up files.

`thread rm <key>` drops one of your threads, its pending events with it. `thread ls` prints a JSON array of your live threads in `put` shape (the `state` reflects the user's done/reopen too), for a dispatcher that lost its own state; it needs an open dashboard (exit 75 otherwise). Threads are tied to your instance's id, not its name: they survive stop, restart and rebuild. `devsandbox rm` of the dispatcher archives them (read-only, shown only in **All**). Done and archived threads are dropped 14 days after their last change, unless a done thread still has events you haven't acked; past the 200-per-instance cap, archived threads go first, then done ones.

### Events

What the user does on a thread comes back as an event, held by the dashboard host until you ack it:

```
devsbd events [--wait SECS]     # JSON lines, oldest first
devsbd events ack <id>...
```

```json
{"id":"e-1790900001-3f2a","key":"pr-6900","kind":"action","action":"post","at":"2026-10-02T12:00:01Z"}
{"id":"e-1790900042-77c1","key":"pr-6900","kind":"reply","text":"Also rename the event to agent_run.cost","at":"2026-10-02T12:00:42Z"}
{"id":"e-1790900050-0b9e","key":"pr-6900","kind":"done","action":"done","at":"2026-10-02T12:00:50Z"}
```

| `kind` | sent when | extra field |
|---|---|---|
| `action` | `1`–`9` on a button without `host`, or with `notify: true` | `action`: the button's `id` |
| `reply` | `r` in the pane, then `enter` (trimmed, at most 2000 chars) | `text` |
| `done` | `d` on the thread (list or pane), or a `done: true` button | `action`: the button's `id`, only from a button |
| `reopen` | `u` in the pane on a done thread (it goes back to `active`) | |

`at` is RFC 3339 UTC, by the dashboard host's clock. `d` and `u` only send an event when they change something (`d` on a done thread, `u` on a live one, do nothing). Each event also adds a timeline entry, and the pane shows how many are still waiting for you. Archived threads take no events.

- **At least once.** `events` prints every pending event, not just new ones, until they're acked. Ack after you've saved what the event changed in your own state, and make handlers idempotent: a crash between the two replays the event. The `id` tells two deliveries of one event apart.
- `events ack` prints how many it dropped; an id that's unknown, already acked or another instance's is ignored, so a retried ack is harmless. A malformed id is a usage error (exit 2).
- `--wait SECS` (at most 300) returns as soon as anything is pending, or prints nothing after `SECS`. Use it in place of your loop's sleep and a click gets handled within seconds. Without `--wait`, `events` answers at once.
- Events are a control command: they need an open dashboard (exit 75 otherwise). An event only exists because someone clicked in a dashboard, so there's nothing to miss meanwhile; they wait in `inbox.toml`.
- Each thread keeps at most 100 unacked events (the oldest is dropped beyond that), and one `events` answer stops at about 512 KiB: ack what you got and call again for the rest.
- You only ever see and ack events of your own threads.

### Sample: a thread loop

One file per item in `.items/` holds its state; the loop re-puts every thread, then waits for clicks. POSIX sh, with `jq`.

```sh
#!/bin/sh
set -u
ITEMS=.items; TODO=.todo      # this dispatcher's own state; gitignored
mkdir -p "$ITEMS" "$TODO"

put() {  # <key> <state> <message>
  jq -cn --arg k "$1" --arg s "$2" --arg m "$3" '{
    key: $k, title: "Item \($k)", state: $s, child: $k, message: $m,
    actions: [
      {id: "open", label: "Open the draft", host: {vscode: {path: "DRAFT.md", line: 1}}},
      {id: "send", label: "Send it"},
      {id: "skip", label: "Skip", done: true}
    ],
    reply: {placeholder: "What should change?"}
  }' | devsbd thread put
}

while :; do
  # ... checks and runs update $ITEMS/<key> and work off $TODO/<key> here ...

  # Re-assert every thread; unchanged ones cost nothing.
  for f in "$ITEMS"/*; do
    [ -e "$f" ] || continue
    put "${f##*/}" "$(cat "$f")" "Draft ready in DRAFT.md."
  done

  # Sleep up to 5 min, or until the user clicks.
  out=$(devsbd events --wait 300); rc=$?
  if [ "$rc" -eq 75 ]; then sleep 60; continue; fi   # no dashboard open
  [ "$rc" -eq 0 ] && [ -n "$out" ] || continue
  printf '%s\n' "$out" | while IFS= read -r ev; do
    key=$(printf '%s' "$ev" | jq -r .key)
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

# Run a control command, retrying while no dashboard is open (exit 75).
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
- **Host-side limits**: a dashboard serves at most 8 notify/control requests per instance at once (more are refused and retried, or fail with "host disconnected"). A dispatched `ensure`/`stop`/`rm` is killed after 30 min (with its docker/git children) and answers `timed out after 30 min`; a run command (`exec`, `run ls|logs|rm|prune`) after 5 min, `run wait` after its 300 s cap plus 30 s; run-command output over 1 MiB is an error, not a cut-off answer.
- **The dashboard must be open** for notifications to be delivered and for control commands to succeed (exit 75 otherwise). Notifications queue; control requests don't.
- **Host git refuses repos with command-running config**: every sandbox can write the repo's `.git` (worktree instances share the base repo's), so host git — `worktree add`/`remove`, `fetch`, `branch -D`, which `ensure` and `rm` trigger — runs with hooks and `core.fsmonitor` disabled and first checks the repo's local config files (`.git/config`, `config.worktree`s) against an allowlist of keys that can't run commands. Anything else (`core.sshCommand`, `filter.*`, `include.path`, `credential.*`, an `ext::` remote URL, …) or an `objects/info/alternates` file makes the command fail with `refusing to run git on <repo>: … sets <key>`. A sandbox may have written it: review the key and remove it (`git config --unset <key>` after checking it is yours); put personal settings in `~/.gitconfig`, which isn't checked.
