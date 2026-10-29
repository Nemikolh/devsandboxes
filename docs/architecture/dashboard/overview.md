# Dashboard overview

The TUI dashboard (`devsandbox` with no subcommand on a TTY) is a client
of `devsandbox serve`: it renders the daemon's snapshot pushes and sends
its actions as API requests. Read this first for anything under
`src/tui/`; the sibling docs go deeper.

Code: `src/main.rs` (no-subcommand branch), `src/tui/mod.rs` (`dashboard`,
`setup`, `enter`, `restore`, `install_panic_hook`, `run`, `spawn_verb`,
`spawn_code`, `run_suspended`, `open_link`), `src/tui/daemon.rs`
(`DaemonWorker`, `Cmd`, `DaemonUpdate`, `Backoff`, `open`, `connect`,
`watch_call`, `op_call`), `src/tui/poll.rs` (`PollWorker`, `PollUpdate`),
`src/tui/app/mod.rs` (`App`, `Tab`, `Focus`, `on_key`, `on_mouse`),
`src/tui/ui.rs` (`draw`), `src/tui/data.rs`, `src/tui/remote.rs`,
`src/tui/settings.rs` (`Settings`, `SETTINGS`, `load`, `save`).

## Entry

`main` opens the dashboard when there is no subcommand and both stdin and
stdout are TTYs; otherwise it prints help and exits 2. `--machine` is
refused. `mod tui` is unix-only: off unix the no-subcommand TTY case says
the dashboard needs `devsandbox serve`
([../daemon/platform-support.md](../daemon/platform-support.md)).
`dashboard` installs a panic hook that restores the terminal, enables raw
mode, probes the kitty keyboard protocol, enters the alternate screen with
mouse capture, runs the loop, and restores on every exit path.

## A client of the daemon

The dashboard reads no config and runs no runtime command for its views:
the tree is the daemon's `instances.snapshot` push for this config root,
processes come from `instances.procs`, modal contents from one-shot reads,
the Ports tab from `forwards.*`, verbs are verb requests
([tabs-and-actions.md](tabs-and-actions.md)). It runs no autostart pass
either; the daemon does.

Still local:

- the Inbox store (`inbox.json`): loaded at start, reloaded when its
  mtime/size stamp moves or on `inbox.changed`; written locally only when
  no daemon is connected ([inbox-tab.md](inbox-tab.md));
- `dashboard.toml` (settings) and `prompt_history`, next to `state.toml`;
- `state.toml`, read by the VS Code launch (`spawn_code` →
  `commands::vscode::launch`);
- child processes it starts itself: terminal tabs (`devsandbox exec`, or
  `<runtime> exec` for a service, [terminal.md](terminal.md)), `:exec`
  suspended in the foreground, the desktop opener for Inbox links.

Without a daemon the tree says why (or keeps the last snapshot, marked
stale), verbs fail with "needs devsandbox serve", and the Inbox
works on the store alone.

## Threads

- **Main thread**: the event loop in `tui::run`. Each pass: resize PTYs,
  `ui::draw`, write pending clipboard output, poll one crossterm event
  (250 ms, 30 ms while a terminal is open) into `App::on_key` /
  `App::on_mouse`, then drain `App`'s pending requests to the workers and
  the workers' answers back into `App`.
- **Daemon worker** (`DaemonWorker`): owns the long-lived connection
  (client `tui`), so the dashboard is a holder. Connects lazily (starting
  a daemon if none answers; after a version handoff it waits for the
  successor first), subscribes to `inbox`, `instances`, `forwards`, and
  watches this root (`instances.watch` with `machines: true`) on every
  (re)connect. Reconnects on the `Backoff` schedule (at once, then 1 s
  doubling to 30 s). Relays pushes as `DaemonUpdate`s
  (`Snapshot`, `Down`, `InboxChanged`, `InstancesChanged`, `Shown`,
  `Ports`, …) and carries the one-shot `Cmd`s: `Fetch` (logs, config,
  inspect), `BuildLog`, `Signal`, `Rename`, `Done`, `Cancel`, `Inbox`
  ops, `ForwardAdd`/`ForwardRm`. Disconnected, each answers with a
  "needs devsandbox serve" line.
- **Poll worker** (`PollWorker`): `instances.procs` on a connection of its
  own, so a slow `docker exec` never delays notifications; at most one
  request outstanding; never starts a daemon.
- **One thread per verb request** (`spawn_verb`, own connection) and per
  VS Code launch (`spawn_code`).
- **One reader thread per terminal tab** (`term::spawn_reader`).

Nothing is written to stderr while the dashboard runs; failures become
status lines.

## `App`: the state machine

`App` holds all view state (tab, selections, snapshot, modal, prompt,
terminal tabs, Inbox view, settings) and turns key and mouse events into
state changes plus pending requests (`pending_action`, `pending_fetch`,
`pending_inbox`, …) that the event loop takes with `take_*`. It does no
socket or runtime I/O, so key handling is unit-tested without a terminal
or daemon (`app/test_support.rs`). Accepted exceptions: spawning a
terminal tab's PTY and reading/appending prompt history happen inline.
`ui::draw` renders from `&App` and registers selection regions through
`RefCell`s; layout helpers (`terminal_panel_rect`, `tab_hit`, …) are
shared with `App`'s mouse hit-tests so drawing and hit-testing can't
drift.

## Machines

With machines registered the snapshot is federated: remote rows carry
`machine`, and the snapshot's `machines` give each link's state
([../daemon/machines.md](../daemon/machines.md)). Everything the
dashboard keys by name (tree groups, process cache, in-flight sets,
pending actions, terminal tabs) uses `remote::qualify`: the bare name
locally, `box/web-2` on machine `box` (names can't hold `/`). `App` queues
keys; the I/O edges split them back (`remote::split`) into the request's
`machine` param and, from the row, its remote config `root`
(`remote::Route`, `instance_route`/`sandbox_route`/`service_route`).

- Rows of a machine whose link isn't `connected` show `unreachable`
  (`remote::unreachable`).
- The header notes machines with no rows yet: `box: connecting` /
  `box: unreachable: <why>` (`pending_note`).
- Typed names resolve against the snapshot: bare when unique across
  machines, else `box/web` or `@/web` for this host (`remote::resolve`).
- Remote terminals and `:exec` run this host's `devsandbox exec` on the
  qualified name, which reaches the machine over its link's `PTY` streams
  (`remote::exec_argv`, `service_argv`). Port forwards to a machine's rows bind here
  (`forwards.add` with `machine`); VS Code is refused for them.

## Settings

`Settings` persists per-user toggles in
`<data dir>/devsandbox/dashboard.toml`; every field defaults on its own,
unknown keys are ignored. The `?` modal's rows, toggling and hit-testing
derive from `SETTINGS`, so a new setting is a field plus one table entry.
The event loop loads at start (an invalid file runs on defaults, with a
status line) and saves after a toggle. The two toggles are described in
[selection.md](selection.md#settings).

Further reading: [tabs-and-actions.md](tabs-and-actions.md),
[command-line.md](command-line.md), [terminal.md](terminal.md),
[selection.md](selection.md), [inbox-tab.md](inbox-tab.md),
[snapshot-and-json.md](snapshot-and-json.md),
[../daemon/client-server.md](../daemon/client-server.md),
[../../reference/api.md](../../reference/api.md).

Limitations: [status.md](../../status.md#dashboard)
