# Tabs, modals and actions

What the dashboard shows on each tab and how a key becomes a request to
the daemon. Not a keymap (the `?` modal and the help bar are that). Read
this before adding a tab, a modal, or a one-key action.

Code: `src/tui/app/mod.rs` (`Tab`, `App`, `dispatch_key`, `PortRow`,
`PortRequest`), `src/tui/data.rs` (`Node`, `visible_nodes`, `group_of`,
`ORPHANS_NAME`, `totals_line`, `STALE_AFTER_SECS`, `identity_line`), `src/tui/app/tree.rs`,
`src/tui/procs.rs` (`ProcState`, `ProcRow`, `build_forest`, `run_rows`,
`proc_state`, `is_agent`, `AGENT_NAMES`), `src/tui/app/procs.rs`
(`Signal`, `PendingSignal`, `proc_fetch_targets`, `agent_count`),
`src/tui/app/actions.rs` (`busy`, `stop_or_start_instance`,
`remove_instance`, `attach_code`, `verb_answered`, `VerbEnd`,
`PendingDone`), `src/tui/app/view.rs` (`Modal`, `Fetch`, `ConfigView`,
`BuildLogModal`, `ConfirmModal`, `open_config`, `open_build_log`,
`refresh_build_log`), `src/tui/app/identity.rs` (`identity_gate`,
`IdentityReq`, `IdentityOn`, `offer_routes`, `App::identity_offer`,
`App::identity_answered`), `src/tui/ansi.rs`, `src/tui/mod.rs` (`run`,
`spawn_verb`, `verb_call`, `verb_status`), `src/tui/daemon.rs` (`Cmd`,
`DaemonUpdate`), `src/tui/ui.rs`.

## Tabs

`Tab::ALL`: Instances, Services, Ports, Inbox, Machines (`1`-`5`,
`tab`/`shift-tab`, or a click on the title). Machines is listed only while
a machine is registered: `App::tabs` is what the bar (`ui.rs` `tab_spans`,
`tab_hit`), cycling and `5` follow, and `App::set_machines` leaves the tab
for Instances when the last machine goes. Each keeps its own selected row.

- **Instances**: a tree. `visible_nodes` flattens sandboxes (config
  order) → instances → processes into `Node`s: `Sandbox`, `Instance`,
  `Empty` (a sandbox with no instances), `Orphans` (the `(not in config)`
  group for instances whose sandbox is gone) and `Proc`. With machines,
  groups are per (machine, sandbox) (`group_of`). Collapse state and
  expanded process layers survive snapshot refreshes. The Detail pane
  shows the selected row. Done instances render dimmed. A sandbox's
  Detail (`ui.rs` `sandbox_detail`) lists its validation `issues` in red,
  then its identity notes (`SandboxRow.identity`: what uses `/root` and
  breaks under `identity = "host"`) in yellow, dimmed once acked.
- **Services**: one row per `ServiceRow` with its backing containers.
- **Ports**: the daemon's forwards for this config root plus every
  forward into a machine (`PortRow`, `configured` for a sandbox's
  `forwardPorts`, `machine` for a remote one, shown in a MACHINE column
  like the Instances tab's). The worker fetches `forwards.list {machines:
  true}` on connect and on `forwards.changed`. `p` on an
  instance or service (a prefilled `port` prompt) adds through
  `forwards.add` and switches here; `d` stops through `forwards.rm`.
  Disconnected, the tab is empty
  ([../daemon/forwards.md](../daemon/forwards.md)).
- **Inbox**: threads from the shared Inbox store
  ([inbox-tab.md](inbox-tab.md)).
- **Machines**: one row per registered machine, in the snapshot's
  `machines` order (`SnapshotMachine`, `ui.rs` `draw_machines`): NAME,
  TARGET, STATE (`LinkState::as_str`, with `: <error>` when set), AGENT
  (`on`/`off`), VERSION (`-` before a link). `a` flips the selected
  machine's `agent`: `App::toggle_selected_agent` queues it
  (`take_pending_machine_set`), the event loop hands it to the daemon
  worker (`Cmd::MachineSet`, `machines.set {name, agent}`), and the
  answer is a status line; the row isn't touched, it shows the new value
  with the next snapshot push
  ([../daemon/machines.md](../daemon/machines.md) *Dashboard*).

The header's totals line marks the snapshot stale with why when the daemon
connection is down, or after `STALE_AFTER_SECS` without a push.

Above the Instances and Services tables (`ui.rs` `draw_status_lines`),
each one row and only while set: `Snapshot.error` in red, then
`identity_line` in yellow, `N sandboxes use /root (U to update)`,
counting the rows (a machine's included) with an un-acked identity note.
`U` opens the offer ([Identity modals](#identity-modals)).

## Process layer

`→`/`space` on an instance expands its processes. `poll.rs` asks the
daemon (`instances.procs`) for the expanded running instances plus the
selected one (for the Detail agent count), on expand, after a change, and
every 5 s (`PROC_TICK`), one request at a time and only on the Instances
tab. `proc_state` turns each answer into a `ps --forest`-style
`ProcState::Rows` (then up to 5 tracked `devsbd run` rows, `run` in the
pid column) or a placeholder `Message`. `is_agent` counts coding-agent
processes by argv basename (`AGENT_NAMES`) for Detail.

On a process row only navigation and signals apply: `t` SIGTERM, `K`
SIGKILL, sent as `instances.signal`, and only when the rows are
container-namespace pids (`signalable`).

## Modals

`Modal`: `Config`, `Help`, `Logs`, `BuildLog`, `Confirm`. A modal takes
every key and the wheel while up. Their data comes from the daemon as
`Fetch`es, one-shots on the daemon worker's connection
([overview.md](overview.md#threads)), answered by a `DaemonUpdate` that
fills the modal if it is still the one open:

- `enter`/`e` → `Config`: original and resolved config
  (`sandboxes.get`/`services.get`) beside the container's
  `containers.inspect`, a draggable divider between them.
- `l` (Instances) → `Logs`: `containers.logs` tail.
- `b` → `BuildLog`: `instances.buildLog` with `ansi`, coloured by
  `ansi::parse_line` (SGR only, other escapes dropped). While the instance
  is in flight, each snapshot triggers a re-read (one outstanding); it
  follows the bottom unless scrolled away.
- `Confirm`: rm's branch question, "cancel the build?", and the identity
  modals (`Confirmed::Identity`/`IdentityOffer`, a listing under the
  question: [Identity modals](#identity-modals)).
- `?` → `Help`: settings toggles, then every key by context.

## One-key actions and the pending queue

`App` never sends anything. A key sets a pending field (`pending_action`,
`pending_signal`, `pending_done`, `pending_fetch`, `pending_cancel`,
`pending_port`/`pending_unport`, `pending_machine_set`, `pending_inbox`,
`pending_open`, `pending_identity`), and the
event loop takes each once per pass and hands it to a thread.

- `s`: stop a running instance, start an exited one; an exited drifted
  one, a missing container or a failed phase gets a `rebuild` instead
  (worktree kept). `stopping`/`starting` sets drop repeat presses until
  the answer.
- `d`: `rm` right away (non-forced, so rm's own refusals and branch
  question still apply); on an in-flight row it asks to cancel the build
  (`instances.cancel`) instead.
- `o`: VS Code (`spawn_code`, reading `state.toml` locally); refused for
  a machine's row.
- `r`/`p`/`:`: the prompt ([command-line.md](command-line.md)); `t`:
  a terminal ([terminal.md](terminal.md)).

`busy` blocks every one-key action on an instance a live `run`/`rebuild`
is materializing.

## Verbs as requests

`run`/`rebuild`/`start`/`stop`/`rm`/`services.rebuild` go to the daemon
from `spawn_verb`, each on a thread and connection of its own (lazy
start), always with `detach: true` and this config root, the caller's env
and, for build verbs, a pty sized to the build-log modal
([../daemon/verbs.md](../daemon/verbs.md)). `run`/`rebuild`/`start` are
answered once the instance is registered and the build runs on; `stop`/
`rm`/`services.rebuild` are held until done. Quitting cancels none. The
answer becomes a status line (`verb_status`), or for an unanswered rm a
`confirm` whose `Confirm` modal resends it with `deleteBranch`. A verb
for a machine's row carries `machine` and its remote root
(`routed_verb_call`). No daemon, no verb: the dashboard never builds in
its own process.

`rename` (`instances.rename`), done flags (`instances.done`, from Inbox
thread actions), signals, cancel and the modal reads share the daemon
worker's connection. Rows change with the next snapshot push, after which
the processes are refetched.

## Identity modals

The dashboard's side of the identity migration
([../config.md](../config.md#identity-findings)), the CLI prompt's
counterpart ([../daemon/verbs.md](../daemon/verbs.md#the-identity-prompt)). Both go through
the same fns, `serve::verbs_client::identity` (`gather`, `listing`,
`answer`), which the daemon worker runs over its connection
(`Cmd::Identity`, `Worker::identity`), a machine's root through that
machine (`machine`, `config.*` being forwarded quick calls).

- **Verb modal.** `take_pending_action` passes every queued action
  through `identity_gate`, so a key and the `:` prompt alike get it: a
  `run` (its sandbox) or `rebuild`/`start` (its instance's group,
  `group_of`) whose sandbox row has un-acked notes isn't returned; a
  `Confirm` modal holds it instead, listing those notes, `[u] update /
  [c] continue as is (don't ask again) / [s] skip this time` (`esc`/`q`
  skip too). Decided from the snapshot alone. `s` releases the verb
  (`cleared_action`, returned next without asking again). `u`/`c` queue
  an `IdentityReq::Answer` holding it, on the notes' ids
  (`IdentityOn::Notes`, `Scope::Ids`: only what the modal listed is
  touched): the worker reads the root's findings and does what the CLI's
  `u`/`c` do (`config.migrate`; `config.ack`, a TOML root user kept by
  `migrate {keep}`). Its `DaemonUpdate::Identity` carries the verb back:
  `App::identity_answered` releases it, the update's line (a YAML
  config's hand edits included) leading the verb's status line; a
  `conflict` drops it, clearing its guards as a failed verb's
  (`verb_answered`).
- **Offer.** On the first snapshot (`identity_offered`, once per
  dashboard) and on `U` (Instances and Services tabs), an
  `IdentityReq::Offer` reads every un-acked finding, an unused template's
  included (`Scope::All`), of the dashboard's root plus each machine root
  whose rows carry un-acked notes (`offer_routes`). The answer
  (`DaemonUpdate::IdentityOffer`, `App::identity_offer`) opens a modal
  listing them per file, `[u] update / [c] continue as is / [l] later`;
  `u`/`c` send `IdentityOn::Groups`, whose file hashes guard the write
  (`conflict` when the file changed since). Nothing to list: `U` says
  "no config uses /root", the first one says nothing; the first one
  doesn't cover a modal already up.

A rewrite changes `config_hash`, so the next push shows the affected
instances drifted and their notes gone.

Limitations: [status.md](../../status.md#dashboard)
