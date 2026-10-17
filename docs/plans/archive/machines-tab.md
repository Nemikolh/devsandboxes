> Archived 2026-10-17. Historical, may be wrong; do not use as a reference.
> Current: [architecture/dashboard/tabs-and-actions.md](../../architecture/dashboard/tabs-and-actions.md#tabs),
> [architecture/daemon/machines.md](../../architecture/daemon/machines.md#dashboard)

# Machines tab in the dashboard

Status: shipped

Each machine has an `agent` flag (`machines.toml`, off by default; see
[machines](../../architecture/daemon/machines.md) *Agent* and ADR
[0036](../../decisions/0036-machines-get-the-agent-over-our-relay-never-ssh-a.md)).
Today only the CLI flips it (`devsandbox machine set <name> --agent on|off`,
API `machines.set {name, agent}`, `src/serve/machines/api.rs` `set`). The
dashboard never calls a `machines.*` method and its tabs are fixed.

Goal: a **Machines** tab, listed only while **at least one machine is
registered** (`!app.machines.is_empty()`), whose `a` key flips the selected
machine's `agent`.

Decisions (user, 2026-10-17):

- Visibility rule: *registered*, not *connected*. A down machine's
  `agent` can be flipped too (a registry write needs no link).
- No confirmation when turning `agent` on (or off).
- Scope: only the `a` toggle. No `:machine set` command, no `rm`, no
  terminal from this tab.

## What the dashboard already has

- Every registered machine arrives with every snapshot push:
  `DaemonUpdate::Snapshot(list, machines)` (`src/tui/daemon.rs`) →
  `app.machines = machines` (`src/tui/mod.rs`, the `Snapshot` arm in the
  event loop), a `Vec<SnapshotMachine>` (`src/serve/snapshots.rs`
  `SnapshotMachine`: `name`, `target`, `state: LinkState`, `error`,
  `version`, `runtime`, `command`, `agent`). The dashboard always watches
  with `machines: true` (`daemon.rs` `watch_call`).
- After `machines.set`, `Handle::reload` (`src/serve/machines/service.rs`)
  runs `Remotes::sync` (copies `agent`) and then `hooks.changed`, which
  re-publishes snapshots, so the new value reaches `app.machines` with the
  next push; no extra fetch.

## Constraints (every step)

- `src/tui/app/` stays I/O-free (a pure state machine, unit-tested; shared
  fixtures in `src/tui/app/test_support.rs`). Never call the daemon from the
  UI thread: requests go `App` pending field → event loop
  (`src/tui/mod.rs`) → worker `Cmd` (`src/tui/daemon.rs`).
- Tab order: Instances, Services, Ports, Inbox, Machines. `1`-`4` keep their
  meaning; `5` selects Machines only while it's listed.
- Mimic surrounding code; the Ports tab is the model throughout.
- Docs: present tense, code cited by path + symbol, never line numbers.
- Checks: `cargo test --workspace` (includes `tests/docs_links.rs`). Site:
  `cd site && pnpm check`.
- No git: the orchestrator commits.

## Step 1: the tab and its rows (read-only)

Code landmarks:

- `src/tui/app/mod.rs`: `enum Tab` / `Tab::ALL` / `Tab::title` /
  `Tab::index` (add `Machines`, index 4; `ALL` gets 5 entries), `App.selected:
  [usize; 4]` and its init `[0; 4]` (→ 5), `clamp_selection` (iterates
  `Tab::ALL`), `row_count_for` (add `Tab::Machines => self.machines.len()`),
  `next_tab`/`prev_tab` (cycle over the *listed* tabs, not `Tab::ALL`, and
  by position in that list, not `Tab::index`), `dispatch_key` (`'5'`, only
  when listed). Add `App::tabs() -> Vec<Tab>` (or similar) returning
  `Tab::ALL` minus `Machines` when `self.machines.is_empty()`.
- `app.machines` is assigned directly in `src/tui/mod.rs` (the `Snapshot`
  arm). Replace with an `App::set_machines(Vec<SnapshotMachine>)` that stores
  them, clamps selection, and, when the tab is open and the list became
  empty, switches to Instances via `set_tab` (`src/tui/app/inbox.rs`).
  Update the `machines` field's doc comment (`mod.rs`, "Empty without
  any: …").
- Every exhaustive `match` on `Tab` needs the new arm: `src/tui/app/view.rs`
  `open_config` (no config: `return`, like Ports/Inbox, both matches),
  `src/tui/app/terminal.rs` (`"terminal: not available on the Machines
  tab"`), `src/tui/ui.rs` `draw_content` and the hint line (~`Tab::Ports
  => "q quit · tab switch · ↑↓ select · d stop forward …"`). Grep for
  others (`Tab::Inbox =>`). `l` (`open_logs`) and other non-tab-gated keys
  must be no-ops on this tab; check them.
- `src/tui/ui.rs` `tab_spans` iterates `Tab::ALL`: use `app.tabs()` so the
  bar and `tab_hit` clicks follow visibility.
- Rows: new `draw_machines` in `ui.rs`, copying `draw_ports` /
  `draw_ports_empty` / `draw_ports_table`. Columns NAME, TARGET, STATE
  (`LinkState::as_str`, plus `: <error>` when set), AGENT (`on`/`off`),
  VERSION (`-` when none). List every registered machine in the order
  they arrive.
- Help: `HELP_BODY` in `src/tui/app/view.rs`: `1-4  jump to tab` → mention
  `5` (Machines, when listed); add a `Machines tab` section listing ↑/k ↓/j
  (step 2 adds `a`).

Tests (`#[cfg(test)]` in the touched files, using `test_support.rs`
fixtures; there are existing tab tests in `src/tui/app/mod.rs` around the
`'3'`/tab-cycling/`tab_spans` click tests and in `src/tui/ui.rs`):

- tab hidden with no machines (`tabs()`, `tab_spans`, `5` is a no-op,
  cycling skips it); listed with one machine of any state (e.g.
  `Unreachable`);
- `5`, `tab`/`shift-tab` (wrap from Machines to Instances and back), and a
  tab-bar click reach it;
- `set_machines(vec![])` while on Machines → Instances; selection clamps when
  the list shrinks;
- the rows render (NAME/STATE with error/AGENT on|off/VERSION) in a
  `TestBackend` like the existing Ports render tests.

Docs:

- `docs/architecture/dashboard/tabs-and-actions.md` *Tabs*: the tab list
  (`Tab::ALL` … `1`-`4`) → five tabs, Machines listed only while a machine
  is registered (`App::tabs`, `5`), and a **Machines** bullet (rows from the
  snapshot's `machines`, columns).
- `docs/architecture/daemon/machines.md` *Dashboard*: the Machines tab lists
  every registered machine with its link state and `agent`.
- `site/src/content/docs/dashboard.mdx`: where it describes tabs / machines
  (the "With machines registered…" paragraph), mention the Machines tab.

## Step 2: `a` flips the selected machine's `agent`

Code landmarks (model: Ports' `d`):

- `src/tui/app/actions.rs` `stop_selected_forward` / `take_pending_unport`,
  field `pending_unport` in `src/tui/app/mod.rs` `App`: add a
  `pending_machine_set: Option<(String, bool)>` (or a small struct), set by
  a `toggle_selected_agent` on `a` when `self.tab == Tab::Machines` (in
  `dispatch_key`, next to `KeyCode::Char('d') if self.tab == Tab::Ports`),
  with the status line `box: agent on…`/`off…`, and a `take_…` accessor.
  No-op with no row.
- `src/tui/mod.rs` event loop, next to `take_pending_unport` →
  `daemon.forward_rm(id)`: take it and call a new
  `DaemonWorker::machine_set(name, agent)`.
- `src/tui/daemon.rs`: `Cmd::MachineSet { name, agent }`, its arm in `run`'s
  loop, a pure `machine_set_call(name, agent) -> ("machines.set", {name,
  agent})` next to `rm_call`, and `Worker::machine_set` modelled on
  `forward_rm`: success → `DaemonUpdate::Status("<name>: agent on|off")`;
  API error → `Status(e.message)`; disconnected → a `Status` saying it needs
  the daemon (see `cancel`'s wording with `self.down`); transport error →
  `self.lost(false)`. Update the module doc's list of calls (top of file,
  near the `forwards.rm` mention). The new `agent` value shows with the next
  snapshot push; don't touch `app.machines` optimistically.
- Verify, don't assume, that `machines.set` → `Handle::reload` →
  `Remotes::sync` → `hooks.changed` puts the new `agent` into the next
  federated snapshot: find the existing tests in
  `src/serve/machines/service.rs` / `src/serve/snapshots.rs`; if none covers
  "agent change alone → `Remotes` has it and `changed` fires", add one.
- Help (`HELP_BODY`) `Machines tab`: `a  turn the selected machine's agent
  on/off`. Hint line for `Tab::Machines` in `ui.rs`: add `a agent on/off`.

Tests:

- `a` on Machines queues `(name, !agent)` for the selected row; `a` on
  other tabs doesn't; no row → nothing.
- `machine_set_call` builds `machines.set {name, agent}`; the `Cmd`
  round-trip like the existing `ForwardRm(4)` test in `daemon.rs`.
- The reload/snapshot test above.

Docs:

- `docs/architecture/dashboard/tabs-and-actions.md` Machines bullet: `a`
  sends `machines.set`; the row updates with the next push.
- `docs/architecture/daemon/machines.md` *Agent* paragraph ("set by
  `machine add --agent` or `machine set …`"): add the dashboard's `a` on the
  Machines tab; *Dashboard*: same.
- `site/src/content/docs/dashboard.mdx` and
  `site/src/content/docs/host-daemon.mdx` (the sentence listing how to turn
  `agent` on): mention `a` on the dashboard's Machines tab.
