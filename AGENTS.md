# devsandbox

Rust CLI (`devsandbox`) that manages devcontainer-style sandboxes on docker /
podman / Apple container. Each sandbox config can spawn many _instances_
(containers named `devsandbox-<instance>`); repeat instances of one repo get
git worktrees so working trees are never shared.

## Module map

- `src/main.rs` — clap CLI; no subcommand on a TTY opens the TUI dashboard
  (`mod tui` is unix-only, like `serve`; off unix it says it needs the daemon).
  `src/json_stdout.rs` points stdout at stderr for `run --json` so child
  output can't corrupt the one JSON document.
- `src/exe.rs` — `devsandbox()`, the binary to spawn (re-resolved per spawn,
  survives an upgrade), and `own_build_stamp()`, this process's own mtime.
- `src/commands/*.rs` — one file per verb (`run`, `start`, `stop`, `rebuild`,
  `rm`, `ps`, `ls`, `exec`, `done`, `inbox`, `logs`, `inspect`, `stats`, `port`, `machine`, `services::{gc,ls,rebuild}`). Shared name
  resolution in `commands/mod.rs` (`resolve_instance`: exact instance name/id
  first, then sandbox | folder basename). `exec.rs` also owns
  `SHELL_FALLBACK_CMD`, the login shell a command-less `exec` and the TUI
  terminal open. `port.rs` resolves a forward route (instance | service via a
  running instance | injection fallback) and hosts the foreground `port` CLI.
  `inbox.rs` is `devsandbox inbox …` (docs/inbox-cli.md): reads from the
  store, mutations via a running daemon's API, else the same `wire::decide_*`
  ops locally. `machine.rs` is `devsandbox machine add|ls|rm`
  (docs/machines.md): a client of `machines.*` only (never in-process);
  `add`'s TTY multi-select of probed roots and the off-TTY one-candidate
  rule.
  `run/` is split by concern: `mod.rs` (`run`/`materialize`, naming,
  `run_container`), `worktree.rs`, `git.rs` (`host_git`: the only way the
  host runs git — hooks/fsmonitor off, repo config allowlist-checked),
  `ssh_agent.rs`, `mounts.rs`, `image.rs`
  (feature image + Dockerfile/entrypoint generation), `lifecycle.rs`,
  `editor.rs` (VS Code name config); `mod.rs` re-exports what other commands use.
- `src/config.rs` — TOML schema: `[template.*]`, `[sandbox.*]`, `[services.*]`.
  `extends` deep-merges (tables recurse, **arrays concatenate** — that's how
  `template.base` services reach every sandbox). Config drift is detected by
  hashing the merged table (`config_hash` label on containers) plus the build
  dockerfile's contents (`build_hash` label); the shared rule is
  `commands::drift_decision`. The full user-facing format reference is the
  `skills/config-toml-spec/SKILL.md` skill: update it whenever a config field,
  merge rule, or `${…}` variable changes (the docs site renders it verbatim as
  `/docs/config`).
- `src/commands/services.rs` — service/network lifecycle. `scope = "isolated"`
  (default): one container per instance on a per-instance network. `scope =
"global"`: one shared container per config root. `gc` reaps unreferenced ones;
  `rebuild` recreates a service's containers and rewires running sandboxes in
  place (no restart, on any runtime).
- Automations (`docs/automations.md` design + steps, `docs/automations-guide.md`
  user guide): `src/commands/autostart.rs` — once-per-boot `autostart` pass
  (boot id per config root in `State.autostart_boot`) and the `"runtime"`
  restart-policy decisions; `src/commands/dispatch.rs` — host side of the
  dispatcher control API: authorization against current config, child naming
  (`<sandbox>-<key>`), ops as logged `devsandbox` subprocesses, run ops exec'd
  in the child's helper (executor injectable for tests).
- `src/inbox/` — the shared Inbox store (`inbox.json` v3, next to `state.toml`):
  `mod.rs` is the pure model (threads keyed by `(owner instance_id, key)`,
  per-owner cap, `Op` mutations, v2 `inbox.toml` notify import), `feed.rs`
  an owner thread's feed (messages, replies, actions, markers; collapse and
  cap as pure fns), `store.rs` the locked
  read-modify-write every dashboard and the bridge's notify sink go through,
  `ops.rs` every read/mutation as a fn over a store path, `view.rs` the
  Inbox views' membership (shared by the TUI and the API), `wire.rs` the
  API's wire views (`ThreadSummary`/`ThreadDetail`), addressing and the user
  ops' `decide_*` checks (platform-neutral: `serve::api` serves them, the
  CLI's local fallback runs them), `clock.rs` how Inbox times read (card
  age, stamp, UTC offset; the dashboard's and `devsandbox inbox`'s).
- `src/serve/` — the per-user host daemon `devsandbox serve` (unix-only, see
  `docs/serve.md`): `endpoint.rs` (socket dir/paths, bind/connect: the
  `local_endpoint` seam), `daemon.rs` (start lock, accept loop, version
  handoff, drain), `host.rs` (the one container poll: each pass lists every
  devsandbox container once and feeds the bridges, the forwards and the
  watched roots' snapshots; every 5 s, 2 s while a root is watched; plus
  the startup autostart pass; the TUI no longer bridges; the sink's status
  lines go out as `inbox.shown`), `snapshots.rs` (the shared poll's per-root
  cache and cadence: stats at most every 7 s, cpu/mem carried over; pure
  over `Instant`s, the join injectable), `client.rs` (`connect`: lazy
  start + hello; `ensure_running` for `run`/`start`; `Conn` sets
  notifications aside), `idle.rs` (pure idle countdown over `Holders`),
  `proto.rs` (JSON-lines wire, `PROTOCOL`), `api.rs` (the API's method table
  and wire views, socket-free, state path and runtime calls injectable;
  `docs/api.md`): the Inbox over `inbox::ops`, and what the dashboard
  reads and edits: `instances.list` (snapshot at a `depth` +
  `thread_children`), `instances.procs`/`signal`/`buildLog`/`rename`/`done`,
  `containers.logs`/`inspect` (`devsandbox-` names only),
  `sandboxes.get`/`services.get`,
  `forwards.rs` (the `serve-forwards` thread owning every Ports-tab `Forward`,
  one registry per config root, daemon-wide ids; reconciles sandboxes'
  `forwardPorts` against `host.rs`'s running poll, with host ports saved in
  `state.toml`; `forwards.*` talk to it over a request/reply channel),
  `forward_store.rs` (`forwards.toml`: the ad-hoc forwards, with their bound
  host ports, that a starting registry restores; locked atomic writes).
  `daemon.rs` also owns `subscribe`: a `serve-watch` thread flags changes on
  each connection's `Outbox`, a per-connection notifier writes them; and
  `instances.watch`/`unwatch` (per-connection watched roots in `Pending`,
  pushed `instances.snapshot`, latest wins per root).
  `relay.rs` is `devsandbox api --stdio` (byte relay stdin/stdout ↔ socket,
  what the npm `connect()` spawns); `dts.rs` (test-only) checks the wire
  structs against the hand-written `npm/devsandboxes/index.d.ts`.
  `install.rs` is `serve install|uninstall` (systemd user unit / LaunchAgent:
  pure render fns, manager calls through an injected runner; `shutdown`
  stops a running daemon first).
  `verbs.rs` is `run`/`rebuild`/`start`/`stop`/`rm`/`services.rebuild` as API methods
  (docs/verbs-as-requests.md): typed params → a pure `argv`, each verb a
  `devsandbox --in-process` subprocess (request env only, own process
  group, stdout/stderr spooled in `logs/verbs/<pid>.out|.err`) streamed as
  `output`/`progress`; hang-up = SIGINT, a drain isn't one (the verb runs
  on; the next daemon's start sweep of the spool adopts live pids as
  holders, `InFlight::adopt`, and drops dead ones' files). The program,
  spool and state path are injectable (`Runner`), so tests run `sh`.
  `verbs_client.rs` is the CLI side: prompts first (`sandboxes.list`,
  `instances.resolve`, rm's `confirm` round), then streams the request;
  Ctrl-C = `instances.cancel` on a second connection; no daemon → the
  in-process fallback.
  `--in-process` (hidden, global) never lazy-starts a daemon
  (`client::set_in_process`) and never fans out to machines; the
  dispatcher's subprocesses pass it too.
  `machines/` is remote hosts over ssh (docs/machines.md; registry
  `machines.toml` next to `state.toml`, written by the daemon only, read by
  the CLI only to know if any exist):
  `mod.rs` (the registry: `Entry`, locked atomic `update`, name rules,
  `parse_name` → `Where` for `m/x` | `@/x` | bare, `default_name`),
  `ssh.rs` (the one argv builder: `api_argv`, `tty_argv`, `cli_argv`,
  login-shell quoting; the injectable `Dial` → `Duplex`, ssh's stderr tail),
  `link.rs` (the hello as version 0.0.0, protocol must match, `LinkError`),
  `service.rs` (`Machines`: one `serve-machine-<name>` thread per machine,
  connected only while wanted + 60 s linger, backoff, state into
  `snapshots::Remotes`, forwarded quick calls one at a time),
  `api.rs` (`machines.probe|add|list|get|rm`, connection-free, dial
  injectable), `forward.rs` (requests with `machine`: the quick-call list,
  verbs over an ssh each with ids rewritten, the federated `instances.resolve`
  / `sandboxes.list` joins as pure fns), `cli.rs` (the CLI side: `plan` where
  a command's name points, `--machine` uses, the ssh re-exec argv; pure, with
  `route` doing the I/O). `snapshots.rs`'s `merge` builds the federated
  snapshot (`instances.watch {machines: true}`).
- `src/runtime/` — backend abstraction over docker/podman/Apple container.
  `procs.rs` parses a container's `ps` listing and its helper's `devsbd run
  ls` (pure; what `instances.procs` answers).
- `devsbd/` — static in-container helper (workspace member), built by
  `scripts/build-devsbd.sh` and embedded by `build.rs`; `src/devsbd.rs` holds
  the embedded blobs + install, `src/devsbd/proto.rs` the frame protocol
  shared with the helper via `#[path]`. See `docs/sandbox-helper.md`.
  `src/devsbd/forward.rs` is the host forwarder engine (unix-only): binds a
  local listener and tunnels each connection through a self-healing bridge, over
  the mux's flow-controlled `Connect` streams. See `docs/port-forwarding.md`.
  Also shared via `#[path]` (line-based, the helper has no serde):
  `bootfile.rs` (boot file for the in-container `postStartCommand` hook),
  `notify.rs` (notification record), `control.rs` (control request/response
  codec + exit codes), `escape.rs` (line escaping for all three). Host-only:
  `desktop.rs` (quiet `notify-send`/`osascript`). Helper-side:
  `devsbd/src/boot.rs` (`devsbd boot`: daemon + boot-file commands on container
  start), `outbox.rs` (`devsbd notify` queue, flushed by the daemon),
  `ctl.rs` (`devsbd ensure|ls|stop|rm|exec|run … <key>` clients over
  `api.sock`), `runs.rs` (`devsbd run start|ls|logs|wait`: tracked runs in a
  container).
- `src/tui/` — ratatui dashboard. `app/` is a deliberately I/O-free state
  machine (unit-tested): `App` + key/mouse dispatch in `app/mod.rs`, with
  `impl App` blocks split into `tree.rs` (selection/navigation), `view.rs`
  (modals, config/inspect view), `command_line.rs` (`:` prompt handling and
  completion), `terminal.rs`, `actions.rs` (one-key actions + pending queue),
  `procs.rs`, `inbox.rs` (Inbox tab: `devsbd notify` history, key dedupe,
  unread badges), `forms.rs` (pinned open forms: rendering, form zone keys); shared test fixtures in `test_support.rs`. `tui/mod.rs` owns the terminal + event loop and runs
  daemon requests on background threads (the dashboard reads no config
  and calls no runtime; still local: the Inbox store, `dashboard.toml`,
  prompt history, VS Code's `state.toml` read); `procs.rs` builds the
  process forest and rows from `instances.procs`' answer; `prompt.rs` is the `:` command line
  (`spec.rs` is its declarative grammar table — parsing _and_ tab completion
  derive from `SPECS`, so a new flag is one table entry);
  `term.rs` holds the integrated terminal's PTY sessions + tab strip
  (`TermSession`/`TermTabs`, a shell rendered via vt100: `devsandbox exec`
  for an instance, `<runtime> exec -it` for a service);
  `kitty.rs` emulates the kitty keyboard protocol for those sessions (vt100
  callbacks + key encoding; see `docs/tui-terminal.md`);
  `remote.rs` is machines in the dashboard (pure): the `machine/name` key
  everything keys rows by, a row's route (`machine` + remote `root`), the
  unreachable rule, the header's `box: connecting` note, and the ssh argv of
  remote terminal tabs.
  `daemon.rs` is the host-daemon worker thread (unix): owns the `serve`
  connection (the dashboard as a holder; reconnect with backoff), watches
  the dashboard's root on every (re)connect (`instances.watch`) and relays
  its `instances.snapshot` pushes (the tree; a lost connection marks it
  stale), `inbox.changed`/`instances.changed`/`inbox.shown`, sends Inbox `Op`s as
  API calls while connected (else the loop applies them locally), and is
  the Ports tab's client (`forwards.list` on `forwards.changed`,
  `forwards.add`/`rm`; the daemon owns the forwards), sends `d`'s
  `instances.cancel`, and carries the one-shot calls (`Cmd::Fetch`: logs,
  inspect, config, build log; `Signal`, `Rename`, `Done`), each answered
  by one `DaemonUpdate`. `poll.rs` is the dashboard's per-request read:
  `instances.procs`, on a thread and connection of its own, never starting
  a daemon (`daemon.rs` does). The dashboard's `run`/`rebuild`/`start` are detached
  verb requests, `stop`/`rm`/`services.rebuild` held ones, each on its own
  connection (`spawn_verb` in `tui/mod.rs`).
  Mouse selection (`docs/tui-selection.md`): `select.rs` is the pure model
  (extraction, highlight), `app/selection.rs` its routing, `clipboard.rs` the
  OSC 52 writer; `ansi.rs` turns the build log's SGR lines into styled
  spans; `settings.rs` the dashboard settings (`dashboard.toml` next
  to `state.toml`, one `SETTINGS` entry per toggle, shown in the `?` modal).
- `site/` — Astro + MDX docs site (standalone pnpm project, not in the Cargo
  workspace; see `site/README.md`). Checks: `cd site && pnpm check && pnpm build`.
  To look at a page after `pnpm build`: `pnpm shot /docs/x [--width 1440,420]`
  (`scripts/shot.ts`, headless Chromium) writes PNGs of its `<figure>`s (or
  the full page) to `/tmp/site-shots/`; read them back to check diagrams and
  layout. One-time setup: `pnpm exec playwright install --with-deps chromium`.

## Conventions & checks

- Docs in `docs/` are named after their topic, no `plan-` prefix or `-plan`
  suffix (e.g. `docs/rebuild.md`, not `docs/plan-rebuild.md`).
- Doc comments explain _why_; keep them current when moving logic.
- Tests live in `#[cfg(test)]` modules per file; most logic is factored to be
  testable without a container runtime.
- Tests that need docker or the embedded helper use
  `#[test_utils::docker_test]` / `#[test_utils::docker_test(helper)]` /
  `#[test_utils::helper_test]` (proc macros in `test-utils/`, a path-only,
  never-published dev-dependency) and return `Result<(), E>`. A missing
  requirement or `Err(why)` skips locally but **fails on CI** (`CI` env var);
  runtime side in `src/test_support.rs` (`gated`, `with_cleanup`).
- Checks: `cargo test --workspace` (covers the `devsbd/` helper crate too;
  plain `cargo test` only runs the root package). clippy/rustfmt are not
  installed in the default toolchain here.
- Commits follow Conventional Commits, all lowercase:
  `<type>(<what>): <description>` where `<type>` is `feat`, `fix`, `chore`,
  `docs`, `test`, `refactor`, … and `<what>` is the module or concept touched
  (`tui`, `rm` or any other command, `docker`, `apple`, `config`, …).
- Releases: follow the `release` skill (`.agents/skills/release/SKILL.md`).
  `CHANGELOG.md`'s `## Unreleased` section becomes the GitHub release body.
