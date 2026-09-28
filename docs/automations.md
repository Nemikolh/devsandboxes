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

### Step 5 — notify: host side

- `src/devsbd/bridge.rs`: bridges advertise `caps::NOTIFY` when given a
  notification sink; `on_open(NOTIFY)` reads the record, replies `ok`, hands a
  `Notification { instance, level, key, link, msg, at }` to the sink (a
  `UnixStream::pair` + thread, since the mux speaks `Conn`).
- `Bridges::reconcile` (`src/devsbd/bridge.rs:359`): keep a bridge per running
  helper-capable instance even without a host agent (today it clears
  everything when there's no agent at `:362`); agent routing unchanged.
- Desktop notifier: `notify-send` / `osascript`, spawned quietly, missing
  binary ignored; the argv builder is pure and tested.

### Step 6 — notify: TUI inbox

- `src/tui/app/`: notifications list in `App` (dedupe by `key`, capped at 200,
  in memory), unread badge on the instance row, an inbox view listing
  instance, time, level, message, link; key/command to open and to clear.
  I/O-free, unit-tested with `test_support.rs` fixtures.
- `src/tui/mod.rs`: drain the sink each loop turn into `App`; fire the desktop
  notifier from the worker, not the UI thread.

### Step 7 — `dispatcher` config + control handler (host, pure)

- `src/config.rs`: `dispatcher: Option<Dispatcher { spawn: Vec<String>,
  max_instances: Option<u32> }>` (`"*"` = any sandbox of this root), validated
  at `ls` (unknown sandbox names error, except `"*"`).
- `src/state.rs` `Instance`: `dispatcher: Option<String>` (owner
  `instance_id`); `run_container` adds label `devsandbox.dispatcher=<id>`.
- New `src/commands/dispatch.rs`: request/response types (JSON) for `ensure`,
  `ls`, `stop`, `rm`; pure authorization (`spawn` list, ownership, cap) and
  naming (`<sandbox>-<key>`, key charset validated); execution reusing
  `run::materialize` / `start_instance` / `stop` / `rm` in quiet mode.
  `ensure` = create with `--branch`/`--env` if missing, start if stopped,
  return the name.
- Tests: authorization matrix, naming, cap, `ls` filters to owned children.

### Step 8 — control channel end to end

- Protocol: `channel::CONTROL = 4`, `caps::CONTROL = 1 << 3`. Helper:
  `devsbd ensure|ls|stop|rm` send a request over `api.sock`; the daemon opens
  a `CONTROL` stream to the newest bridge whose host advertises `CONTROL`, or
  answers "no host connected" → the CLI exits 75.
- Host: only bridges of instances whose sandbox declares `dispatcher`
  advertise `CONTROL`; the handler re-checks on every request (config may have
  changed). Runs on the bridge worker thread, quiet, never the UI thread.
- TUI tree nests children under their dispatcher; orphans marked.
- Tests: exit 75 with no bridge; `#[test_utils::docker_test(helper)]`
  dispatcher `ensure`s a child and `ls` lists it.

### Step 9 — runs

- `devsbd exec <key> [--detach] -- cmd…` via `CONTROL`: host execs in the
  child through the child's own helper (`devsbd run start` there), which
  records run id, times, exit status, and stdout/stderr under
  `/var/lib/devsandbox/runs/<id>/`. `devsbd run ls|logs|wait <id>` for the
  dispatcher.
- TUI: a child's runs next to its processes.

### Step 10 — Apple `container` pass

- Re-check every step on Apple `container`: runtime mode falls back with a
  warning, notify + control work over the same bridges.

### Step 11 — docs

- `skills/config-toml-spec/SKILL.md`: `autostart`, `dispatcher`.
- User doc (patterns above, sample babysit dispatcher, backgrounding the loop,
  `devsbd` commands, exit 75); `docs/high-level-architecture.md` and the
  `AGENTS.md` module map for the new modules.

## Open questions

- Can the restart policy be changed in place on podman (`podman update --restart`, version-dependent), or does flipping `autostart` there require a recreate?
- Should `postStartCommand` run by the daemon (runtime mode) and by `start` (host mode) be deduped by container start time, or should runtime-mode sandboxes just never have the host run it?
- Children's `autostart`: inherit the child sandbox's own setting (current assumption), or never autostart and let the dispatcher `ensure` them?
- Notification retention in the TUI (count/age), and whether "dismissed" is persisted in `state.toml`.

## Future

- **`npx devsandboxes init dispatcher`** — scaffold a dispatcher repo: `config.toml` snippet, dispatcher script, git init, `AGENTS.md` + skills. To be explored.
- Declarative `[automation.*]` sugar (source / key / when / run) compiled onto the dispatcher primitives, once a few real dispatchers show the common shape.
- Webhook sources (`gh webhook forward`) instead of polling.
