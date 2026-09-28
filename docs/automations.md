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
| `dispatcher` control API | only sandboxes declaring `dispatcher` | ensure/stop/rm/exec child instances |
| runs | dispatchers (via `exec --detach`) | tracked, logged agent invocations |

## `autostart`

```toml
[sandbox.triage]
autostart = true        # devsandbox-driven
# autostart = "runtime" # container runtime restarts it on boot, when supported
```

### `true` — devsandbox-driven, once per boot

The first devsandbox process after a boot (TUI load, or any CLI command) starts every `autostart` sandbox's instances (as `devsandbox start`: devsbd install, `postStartCommand`, bridges). A sandbox with no instance yet gets one (`run` semantics).

"Once per boot" is keyed on the host boot id, recorded in `state.toml` (`last_autostart_boot`):

- Linux: `/proc/sys/kernel/random/boot_id`
- macOS: `sysctl kern.boottime`

Later invocations compare and skip, so the check is cheap. An instance the user stopped after autostart stays stopped until the next boot.

### `"runtime"` — the runtime restarts it

Created with `--restart unless-stopped` where the backend supports it:

- docker: yes (Docker Desktop / dockerd starts on login/boot).

- podman: only if the user has enabled `podman-restart.service`; devsandbox never installs units. Warn at `run` when it can't tell.

- Apple `container`: tier 2 — falls back to `true` with a warning until it has a restart policy.

The runtime only restarts the *container*. Nothing devsandbox runs via `exec` comes back: not `postStartCommand` (`src/commands/start.rs:61`), not the devsbd daemon (`src/devsbd.rs:108`), not host-side bridges or forwards. So `"runtime"` also needs an in-container boot path:

- The generated entrypoint (`docs/entrypoint.md`) starts `devsbd daemon` itself when the binary is present, before the keep-alive.

- The daemon then runs `postStartCommand` once per container start (as `remoteUser`, `workspaceFolder`, `remoteEnv` — same as `start`), and records that it did so, so a later host `start` doesn't double-run it.

This is what makes a dispatcher's loop survive a reboot with no devsandbox process around. Anything needing the host (control API, forwards, ssh-agent) resumes when one connects (see _No host connected_).

`autostart` should be excluded from `config_hash`: flipping it can be applied in place (`docker update --restart …`) instead of marking the instance drifted.

## `devsbd notify`

Available in every sandbox.

```
devsbd notify [--level info|warn|error] [--link URL] [--key K] "PR 123 needs you"
```

- Delivered to the TUI inbox (badge on the instance, an inbox view listing notifications with source instance, time, link) **and** as a desktop notification: `notify-send` on Linux, `osascript -e 'display notification …'` on macOS. A missing notifier is skipped silently.
- `--key` dedupes: a newer notification with the same key replaces the older one (a dispatcher re-reporting "PR 123 conflicted" every poll doesn't spam).
- **Queued**: written to a durable outbox in the container (`/var/lib/devsandbox/outbox/`, surviving container restarts), drained by the host whenever a bridge is up. One-shot CLI commands may drain pending notifications for desktop delivery; the TUI keeps the history.

## Dispatchers

A dispatcher is an ordinary sandbox that declares `dispatcher`. Only such sandboxes get the control API; for every other sandbox the host refuses control streams.

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

- `spawn`: sandbox configs this dispatcher may instantiate (same config root). can use "*" to allow any config
- `max-instances`: cap on live children, enforced by the host.

### Control API

```
devsbd ensure <sandbox> --key <key> [--branch B] [--env K=V]…
    # idempotent: creates <sandbox>-<key> if missing, starts it if stopped;
    # prints the instance name. --branch sets the worktree branch at creation.
devsbd ls                         # this dispatcher's children + status (JSON)
devsbd stop <key>
devsbd rm <key>
devsbd exec <key> [--detach] -- <cmd>…   # a run; see _Runs_
```

- Children are named `<sandbox>-<key>` (e.g. `web-pr-123`): predictable, so a human can `devsandbox vscode web-pr-123`.

- Children are labelled `devsandbox.dispatcher=<dispatcher instance_id>`, and recorded as such in `state.toml`. `ls`/`stop`/`rm`/`exec` only reach owned children.

- The TUI tree nests children under their dispatcher.

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

The TUI serves control requests for every running dispatcher. Two TUIs don't both serve the same dispatcher: a per-dispatcher lock file (lease) in the data dir picks one. One-shot CLI commands don't serve control.

## Runs

`devsbd exec <key> --detach -- zidane -p "…"` starts a tracked process in a child: run id, start/end time, exit status, captured stdout/stderr (kept in the child, streamed on demand). `devsbd run ls|logs|wait <id>` for the dispatcher; the TUI shows a child's runs next to its processes. Without `--detach`, `exec` streams and returns the exit status.

## Transport

All of the above rides the existing devsbd frame channel (`docs/sandbox-helper.md`), container-initiated `Open` streams on new channels:

- `channel::CONTROL` (request/response JSON over `Data`), `channel::NOTIFY`.
- New caps bits so an old host/helper pair degrades cleanly.

Today the TUI only keeps bridges where there's something to relay: `Bridges::reconcile` (`src/devsbd/bridge.rs:359`) skips everything without a host ssh-agent.
It must also keep a bridge to every running dispatcher (control + notify), and drain notify outboxes from any running instance (a periodic short-lived bridge is enough for non-dispatchers).

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
  `exec`; `unless-stopped` keeps stopped children stopped across reboots.

- Sample dispatcher: a babysit loop polling `gh pr list`, a cheap
  needs-attention check per PR, `ensure` + `exec --detach` only when needed,
  `notify` on anything needing a human.

## Steps

1. `autostart = true`: boot id in `state.toml`, trigger on TUI load and first CLI command.
2. `autostart = "runtime"`: restart policy per backend, entrypoint starts `devsbd daemon`, daemon runs `postStartCommand`; excluded from `config_hash`.
3. `devsbd notify`: outbox, `NOTIFY` channel, TUI inbox + badge, desktop notifier.
4. `dispatcher` config + `CONTROL` channel: `ensure`/`ls`/`stop`/`rm`, ownership label, `max-instances`, lease, fail-fast exit code.
5. Runs: `exec --detach`, `run ls|logs|wait`, TUI view.
6. User docs: patterns above, sample dispatcher; update `skills/config-toml-spec/SKILL.md` for `autostart` and `dispatcher`.

## Open questions

- Can the restart policy be changed in place on podman (`podman update --restart`, version-dependent), or does flipping `autostart` there require a recreate?
- Should `postStartCommand` run by the daemon (runtime mode) and by `start` (host mode) be deduped by container start time, or should runtime-mode sandboxes just never have the host run it?
- Children's `autostart`: inherit the child sandbox's own setting (current assumption), or never autostart and let the dispatcher `ensure` them?
- Notification retention in the TUI (count/age), and whether "dismissed" is persisted in `state.toml`.

## Future

- **`npx devsandboxes init dispatcher`** — scaffold a dispatcher repo: `config.toml` snippet, dispatcher script, git init, `AGENTS.md` + skills. To be explored.
- Declarative `[automation.*]` sugar (source / key / when / run) compiled onto the dispatcher primitives, once a few real dispatchers show the common shape.
- Webhook sources (`gh webhook forward`) instead of polling.
