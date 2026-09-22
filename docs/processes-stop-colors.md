# Plan: process layer, stop command, color refresh

Repo: `devsandbox`. Builds on the dashboard in `src/tui/` (see docs/tui.md:
sandbox tree on the Instances tab, background `Snapshot` collection every 2s,
`:` prompt with `run/exec/code/rm`, config/logs/help modals).

Constraints for the implementing agent (unchanged):
- **Coding only. No git operations.** The orchestrator reviews and commits.
- Match existing style; `cargo build` + `cargo test` green before reporting.
- TUI docker calls go through `docker::output_quiet` / `output_merged` only.

## Step 10 — `stop` as a first-class command

`stop` and `rm` stay distinct: `stop` = `docker stop` (container and state entry
survive; `run` already restarts stopped containers), `rm` = full removal. No alias.

- New `src/commands/stop.rs`: `pub fn stop(name: &str) -> Result<()>`.
  Reuse the same instance matching as rm.rs:13-37 (instance name, sandbox name,
  or folder basename; interactive pick on ambiguity) — factor that block into a
  shared helper (e.g. `commands::resolve_instance(state, name)`) used by both
  rm and stop rather than copying it.
  Then: `docker stop <container>`, plus `docker stop` of the instance's isolated
  service containers (same label filters as rm.rs:57-61, but stop instead of
  rm -f; leave networks and global services alone). Print `stopped <key>`.
- main.rs: `Stop { name: String }` subcommand ("Stop a sandbox instance
  (docker stop; `run` restarts it)").
- Prompt: `stop <instance>` action (`PromptAction::Stop`), completion over
  instance names, executed via the suspended-terminal path like rm.
- TUI shortcut: `s` on an instance node (Instances tab) = stop that instance
  WITHOUT suspending the TUI: run it on a background thread (docker stop can
  block ~10s on SIGTERM timeout); reuse the existing status line for
  `stopping <name>…` / result, and force a snapshot refresh when it lands.
  The thread reports back over a channel drained in the event loop (same
  pattern as spawn_collect). `s` on non-instance nodes: no-op.
- Detail panel + help: document `s stop instance`; `stop` in the `?` overlay
  and the prompt help.
- Tests: instance-matching helper (exact/sandbox/folder/ambiguous), prompt parse
  of `stop`, `s` key sets the pending stop on instance nodes only.

## Step 11 — process layer under instances

Third tree level: instance → its running processes, `ps -ef --forest` style.

- Data: `docker top <container> -eo pid,ppid,args` (host-side ps; works even
  when the container image has no `ps`). Parse into
  `Proc { pid, ppid, args }` rows; build the forest ourselves (pure function):
  children ordered by pid under their parent, args prefixed with the usual
  `\_ ` indentation per depth. Unparseable lines skipped.
- Expansion state: `expanded_procs: BTreeSet<String>` (instance names) on App —
  default collapsed. `→`/`space` on an instance node expands (existing sandbox
  fold keys keep working; an instance node with procs expanded shows children;
  `←` on a process row jumps to its instance; `←` on the instance collapses
  procs first, then jumps to the sandbox on the second press).
- Node model: add `Node::Proc { instance: usize, line: usize }` to
  `data::visible_nodes` (new signature takes the proc map + expanded set).
  Proc rows render dim, indented under the instance: `PID` in the TREE column
  gutter, forest-indented args across the remaining width; no status/cpu
  columns.
- Fetch/refresh: processes are fetched ONLY for expanded instances —
  immediately on expand, then every 5s (separate cadence from the 2s snapshot):
  the event loop tracks `last_proc_tick`; on tick it spawns one background
  fetch for all currently-expanded instances (guarded like spawn_collect, one
  in flight) sending `BTreeMap<String, Result-ish ProcList>` back; App stores
  `procs: BTreeMap<String, Vec<ProcRow>>`. Collapse drops nothing (cache kept;
  stale is fine, refresh overwrites). Fetch error for an instance → one dim
  `(processes unavailable: …)` row. A stopped/missing container → skip fetch,
  `(not running)` row.
- Selection: clamping already derives from visible_nodes — verify with procs.
  `enter`/`e`/`l`/`o`/`r`/`s` on a proc row act on its parent instance.
- Tests: forest building (order, indentation, orphan ppid), visible_nodes with
  expanded procs, ←-navigation ladder (proc → instance → sandbox), parse of
  docker top output (header line skipped).

## Step 12 — inspect pane inside the config modal

Debugging aid merged into the existing explorer: config and `docker inspect`
render side by side in one full-screen modal.

- The config modal becomes a split view: LEFT = the existing TOML
  (original/resolved), RIGHT = pretty-printed `docker inspect <container>`
  fetched at open time via `docker::output_quiet` (serde_json parse +
  `to_string_pretty`; on parse failure keep docker's output verbatim with the
  error as the first line; docker down / no container → explanatory text).
- Which container: opened from an instance or proc row → that instance's; from
  a sandbox node → its first running instance (else `(no running instance)`
  placeholder); Services tab → first running backing container of the service.
- Key changes: `t` toggles original/resolved (frees `tab`); `tab` switches
  focused pane (focused pane receives the scroll keys; visible focus marker in
  the pane title). `esc`/`q` close as before.
- Draggable divider: enable crossterm mouse capture (EnableMouseCapture on
  setup, Disable on restore/panic hook); dragging the divider column resizes
  the split, persisted in the modal state as a percentage (clamped ~20–80%).
  Keyboard fallback: `<`/`>` move the divider by 5%.
  Mouse events other than divider drag are ignored (no click-to-select this
  step). Scroll wheel scrolls the hovered pane if cheap to add, else skip.
- Light JSON highlighting line-by-line (keys green, punctuation dim) mirroring
  the TOML highlighter.
- Help bar + `?` overlay updated (`t toggle · tab pane · <> resize`).
- Tests: JSON pretty-print fallback, divider clamp math, `t`/`tab` routing,
  container pick per node kind (instance / sandbox-with-running /
  sandbox-without).

## Step 13 — color refresh

Everything accent-colored is `HIGHLIGHT = Color::Cyan` today (ui.rs:18), used
for BOTH chrome (borders, tab, sandbox names, modal titles) and the selected
row — selection is hard to spot.

- Split into two constants in ui.rs:
  `ACCENT: Color = Color::Rgb(175, 135, 255)` (purple-ish; use Indexed(141) if
  Rgb renders poorly in tmux — pick one, note the choice) for chrome: borders,
  tab highlight, sandbox names, `▸/▾`, modal titles, prompt `:` prefix.
  `SELECTION: Color = Color::Rgb(0, 215, 135)` (spring green, clearly distinct
  from both ACCENT and the plain `Color::Green` used for running status) for
  `row_highlight_style` (+ BOLD).
- Replace every `HIGHLIGHT` use deliberately (chrome → ACCENT, row highlight →
  SELECTION); status/scope/source colors (Green/Red/Yellow/Blue/Magenta dim)
  stay as they are.
- `src/render.rs` (plain `ls`) keeps its ANSI palette — this step is TUI-only.
- Tests: existing style assertions updated (ui.rs:741 expects Cyan).

## Later / parked

`.agents/skills` auto-mount ideas live in docs/ideas-agents-skills.md (not
scheduled; do not implement).
