# Plan: integrated terminal for the TUI

Add an integrated-terminal section to the dashboard: shortcuts open a real
shell (PTY + `docker exec -it`) into the selected instance **or service
container**, rendered inside the TUI so the dashboard stays usable, with a
tab per terminal shared across both top-level tabs.

## Dependencies (new)

- `portable-pty = "0.9"` — PTY open/spawn/resize (wezterm's, cross-platform).
- `vt100 = "0.16"` — VT parser: PTY byte stream → in-memory screen.
- `tui-term = "0.3.4"` — ratatui widget rendering a `vt100::Screen`; built
  against `ratatui-core 0.1` / `ratatui-widgets 0.3`, i.e. exactly our
  ratatui 0.30.2.

## UX spec

### Layout

- No terminals open: layout unchanged (table + full-width Detail, height 9).
- ≥1 terminal: the bottom section grows to 50% of the content area (tables
  shrink) and splits horizontally: **Detail 30% | terminal panel 70%**.
- The terminal panel has a tab strip in its block title:
  `1:web-1  2:api-2  3:postgres-web-1` — active tab accent+bold, exited tabs
  dim with `(exited)`. Titles: instance name for instances, container name
  minus the `devsandbox-` prefix for services.
- The terminal list is global: identical on the Instances and Services tabs.

### Focus model & shortcuts

One new state: `Focus::Dashboard` (today's behavior) vs `Focus::Terminal`.

Dashboard focus (no modal, no prompt):
- `t` — open a terminal for the selected instance / service and focus it;
  if a live terminal for that container already exists, focus that instead.
  Requires a running container (services: first running container; status
  message otherwise). `T` forces a second terminal for the same target.
- `]` / `[` — next / previous terminal tab (changes which is shown).
- `ctrl-]` or `F12` — focus the active terminal.
- `x` — close the active terminal (kills the shell process).

Terminal focus:
- `ctrl-]` or `F12` — leave: focus returns to the dashboard.
- **Every other key is forwarded to the shell** — including `tab`, `q`, `:`,
  digits and F1–F11, so shell completion, htop, vim etc. work unmodified.
- On an exited terminal, keys are swallowed (leave keys still work); the
  help bar shows `exited — x to close`.

Why `ctrl-]` + `F12`: inside-the-shell conflicts only matter for the leave
key (all else is forwarded). `ctrl-]` is the historical telnet escape and is
unused by readline/bash; its one common conflict is vim's jump-to-tag, which
is covered by the `F12` alias. F1/F10/F11 are captured by GUI terminal
emulators (help/menubar/fullscreen) and macOS defaults F-keys to media keys,
which is why F12 is an alias rather than the only binding, and why direct
`F<N>` tab jumping was rejected in favor of `[` / `]`.

Mouse:
- Click on the terminal body → focus it. Click on a tab title → activate it.
- Wheel over the terminal body → vt100 scrollback (any new output snaps back).

### Focus visibility

- Terminal focused: terminal pane border ACCENT+BOLD with `▶ ` title mark
  (same language as the config modal's `pane_block`), table + Detail borders
  DIM, cursor rendered in the terminal.
- Dashboard focused: current styling; terminal pane border DIM, no cursor.
- Help bar swaps per focus: terminal focus shows
  `ctrl-]/F12 back to dashboard · keys go to the shell`.

### Lifecycle

- Shell: `sh -lc 'command -v bash >/dev/null 2>&1 && exec bash -l; exec sh -l'`
  (bash when present, POSIX sh fallback — service images are often minimal).
  `-e TERM=xterm-256color`. Instances reuse the exact `exec` argv the CLI
  builds (workspace, remoteUser, remoteEnv); services get a plain
  `exec -it <container>`.
- Child exit (shell `exit`, container stop) → tab marked `(exited)`, screen
  stays readable, `x` closes.
- Quitting the dashboard kills every terminal child (kill + wait on drop).

## Architecture

`src/tui/term.rs` (new): everything PTY/VT.
- `TermSession`: title, container, `Arc<Mutex<vt100::Parser>>`, PTY master +
  writer, child handle, shared `exited`/`dirty` flags, current size. Reader
  thread: PTY → parser, sets `dirty`, records exit after EOF. `resize()`
  propagates to PTY + parser. Kill+wait on drop. A test constructor takes an
  in-memory writer and no child so tab/key logic stays unit-testable.
- `TermTabs`: pure tab state (sessions, active index, focused flag) with
  open/dedup/next/prev/close — unit-tested.
- `encode_key(KeyEvent, application_cursor: bool) -> Option<Vec<u8>>`: pure
  crossterm→bytes encoding (chars, ctrl/alt, arrows CSI/SS3, home/end,
  pgup/pgdn, F1–F12, backspace 0x7f, delete, enter, esc) — unit-tested.

`src/tui/app.rs`: `focus` + `terms: TermTabs` fields; key routing order
becomes prompt → modal → focused terminal → dashboard. `t` resolves the
selection to a container (pure), then spawns via `term::spawn` inline —
same precedent as `open_logs`/`open_config` doing user-triggered I/O.

`src/tui/ui.rs`: bottom-section split, tab strip, border focus states,
help-bar variants. A pure `term_pane_size(frame, prompt_open) -> (rows, cols)`
shared with the event loop so PTY resize math has one home.

`src/tui/mod.rs`: poll interval drops 250ms → 30ms while any terminal
exists (child echo latency; key events already wake `event::poll`, output
does not). Pre-draw: compare `term_pane_size` against each session and
resize divergent ones. Mouse routing for the panel.

`src/commands/exec.rs`: factor the argv construction out of `exec_status`
into `exec_argv(&state::Instance, tty, command) -> Vec<String>` so the TUI
and CLI can't drift.

## Code landmarks

- `src/tui/app.rs:254` `App` struct; `:560-597` dashboard `on_key` match
  (add `t`/`T`/`[`/`]`/`x`/leave keys; route terminal focus before it);
  `:603` `on_mouse`; `:21-58` `HELP_BODY`.
- `src/tui/ui.rs:23-48` `draw`; `:274-313` `draw_instances` and `:90-115`
  `draw_services` (both hardcode `Constraint::Length(9)` detail areas);
  `:816` `pane_block` (reuse for focus-marking); `:684` `draw_help`.
- `src/tui/mod.rs:37` `POLL_INTERVAL`; `:84-226` event loop; `:98-109`
  poll/dispatch.
- `src/commands/exec.rs:48-67` argv building to extract.
- `src/tui/data.rs:41` `InstanceRow` (`container`, `status`), `:82`
  `ServiceRow` (`containers: Vec<(String, ContainerStatus)>`).

## Steps

### Step 1 — deps + PTY/session core (`term.rs`), exec argv refactor

Add the three dependencies. Create `src/tui/term.rs` with `TermSession`
(spawn/reader-thread/resize/kill-on-drop/test-ctor), `encode_key`, and the
in-container shell command constant. Extract `exec_argv` in
`src/commands/exec.rs` (behavior-preserving; `exec_status` calls it).
No UI wiring yet. Tests: `encode_key` table, argv extraction equivalence,
session state with the test constructor.

### Step 2 — app state: tabs, focus, key routing

`TermTabs` in `term.rs` + `App.focus`/`App.terms`. Key routing (prompt →
modal → terminal → dashboard), `t`/`T` target resolution (instances tab:
selected instance / proc-row parent, must be running; services tab: first
running container), `[`/`]`/`x`, `ctrl-]`+`F12` toggle both directions,
status messages for failures. Forwarded keys write `encode_key` bytes to the
active session (honoring `application_cursor`). Tests: routing, dedup-vs-`T`,
target resolution, exited-session key swallowing.

### Step 3 — rendering

Bottom-section layout (50% height when terminals exist, 30/70 split), tab
strip, `PseudoTerminal` widget rendering the active session's screen,
focused/unfocused border treatment (terminal pane vs table/Detail),
help-bar variants, `HELP_BODY` update, `term_pane_size` helper. Tests: layout
math, tab-strip formatting.

### Step 4 — event loop + mouse + docs

Dynamic poll interval; pre-draw PTY resize via `term_pane_size`; mouse
(click-to-focus, tab click, wheel scrollback); kill-all on quit; update the
module map in `AGENTS.md` and the README key table if one exists. Manual
smoke test against the live config (`-C ../.devsandboxes`) plus `cargo test`.

## Decisions the user may want to adjust

- Bottom-section height when terminals are open: 50% of the content area.
- Leave/focus keys: `ctrl-]` + `F12` (rationale above).
- No direct `F<N>` tab jump (outer-emulator conflicts); `[` / `]` + mouse.
- No scrollback keybinding while focused (wheel only) — shell users keep
  `shift-pgup` for their real terminal.
