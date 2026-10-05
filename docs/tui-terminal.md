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
- Wheel over the terminal body, routed like xterm:
  - the child enabled mouse tracking (`?1000h` and friends: zidane, vim
    `mouse=a`, htop) → a wheel report (button 64/65 plus modifiers) at the
    pane-relative cell, in the child's encoding (SGR, X10 or UTF-8);
  - the child is on the alternate screen without mouse tracking (less, man)
    → 3 ↑/↓ per notch, honoring DECCKM, because the alternate screen has
    no scrollback;
  - otherwise (a shell on the main screen, or an exited session) → vt100
    scrollback.
- Buttons (docs/tui-selection.md, step 3), also routed by whether the child
  tracks the mouse. A click on the body still focuses the terminal either way.
  - **Tracking child** (zidane, vim `mouse=a`, tmux): a press on the body,
    any button, is forwarded at its pane-relative cell, and from then on that
    button's drags and release, in the child's mode and encoding: `?1000`
    gets presses and releases, `?1002` drags too, `?1003` hover as well (see
    *Hover* below). A drag
    or release outside the panel is clamped to the body's edge, so a child
    that saw the press always sees the release. Only after a press on the
    body: a drag that started elsewhere (a pane, the border, the tab strip)
    never reaches the child. The child does its own selection and copy; the
    dashboard forms none (its copy reaches the clipboard through the OSC 52
    relay below).
  - **Otherwise** (a shell prompt, `less` without mouse, an exited session):
    a left drag is a local selection over the vt100 screen, copied like
    every other pane's (docs/tui.md, *Mouse selection*). It
    reaches into the scrollback: drag past the body's top or bottom to
    scroll a line per drag event, or use the wheel; the selection stays.
    Wrapped lines copy as one line. Limits: positions count rows from the
    oldest scrollback line, so new output doesn't move the selection, until
    the 5000-line scrollback is full; from then on each new line drops the
    oldest and the text under the selection shifts a row. A program that
    redraws in place (a progress bar, `top`) changes the text under the
    selection, and the copy is what's there at the time of the copy.
    Switching terminal tab, or closing one, clears the selection.
  - `ctrl-shift-c` with a selection copies it instead of reaching the
    shell; without one it goes to the shell as before.
- Hover (docs/tui-selection.md, step 7): crossterm's `EnableMouseCapture`
  already turns on `?1003` in the outer terminal, so button-less motion
  arrives as `Moved`. Over the body of the active live session whose child
  is in `AnyMotion` (`?1003`: hover highlights, tooltips), it is forwarded
  as button 35 (3 + 32, plus modifier bits) at the pane-relative cell, once
  per cell: crossterm reports a move on every cell change, and a repeat of
  the last forwarded cell for the same session is dropped (a press, or
  leaving the body, resets that, so the next move reports). No modal may be
  open. `ButtonMotion`
  and the other modes never get it. Nothing is sent off the body, and the
  protocol has no leave event, so a child keeps its last hover when the
  pointer leaves. Hover never touches the local selection.
- Clipboard (docs/tui-selection.md, step 5): an app copies by sending OSC 52
  to *its* terminal, our vt100 emulator. `KittyState::copy_to_clipboard`
  (`src/tui/kitty.rs`, active whether or not the kitty emulation is) queues
  the base64 payload as is, newest wins; an empty payload, or one over the
  dashboard's own 1 MiB copy cap, is dropped whole (cut base64 could decode
  to broken UTF-8). The selector is ignored: the relay always writes `c`.
  The event loop drains every session after each draw and writes the last
  copy to the outer terminal (tmux-wrapped like our own copies, docs/tui.md,
  *Clipboard: OSC 52*); the status line says `copied from <tab title>`.
  OSC 52 *reads* are never answered: an app in a container must not read the
  host clipboard. The `terminal_clipboard` setting (on by default,
  docs/tui.md, *Settings*) turns the relay off; queues are drained anyway,
  so a copy made while it's off is not sent later.

### Keyboard: kitty protocol

Legacy key encoding conflates chords (ctrl+m and Enter are both `\r`,
ctrl+i is Tab, ctrl+[ is Esc), so apps like zidane bind them through the
[kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/).
For that to work through the dashboard, both hops must speak it:

- **Outer terminal → devsandbox** (`src/tui/mod.rs`): at startup crossterm's
  `supports_keyboard_enhancement()` sends `CSI ? u` followed by DA1. If the
  terminal answers the first, devsandbox pushes `DISAMBIGUATE_ESCAPE_CODES`
  after entering the alternate screen (kitty keeps one stack per screen)
  and pops it on every exit path: quit, error, panic hook, and around
  suspended prompt commands. Only that flag: event types would add
  release/repeat events the dashboard filters anyway.
- **devsandbox → inner app** (`src/tui/kitty.rs`): vt100 ignores the
  protocol, so `KittyState` rides along as the parser's `Callbacks` and
  handles the `CSI > / < / = / ? … u` sequences vt100 passes to
  `unhandled_csi`. It keeps one flag stack per screen, masks flags to the
  disambiguate bit, and queues `CSI ? flags u` replies, which the reader
  thread writes back to the PTY after each chunk. It also answers XTVERSION
  (`CSI > 0 q` → `DCS > | devsandbox(<version>) ST`): OpenTUI apps such as
  zidane ignore the `? u` reply unless a terminal identified itself first. The alternate screen's
  stack is dropped whenever the child is back on the main screen, so an app
  that exits (or crashes) without popping can't leave the shell in kitty
  mode. Keys go through `kitty::encode_key` (Esc, and ctrl/alt/super chords
  of text keys, become `CSI code;mods u`; modified Enter/Tab/Backspace too)
  and fall back to the legacy `term::encode_key` otherwise.

When the outer terminal doesn't support the protocol, the emulation stays
off: queries go unanswered and pushes are ignored, so inner apps see what
they'd see from a legacy terminal. Supported: kitty, Alacritty ≥ 0.13,
Ghostty, WezTerm, foot, iTerm2 ≥ 3.5. GNOME Terminal (VTE) implements
neither kitty nor modifyOtherKeys, so ctrl+m reaches devsandbox as `\r` and
no layer can recover it; the same goes for tmux, which doesn't answer the
query.

### Focus visibility

- Terminal focused: terminal pane border ACCENT+BOLD with `▶ ` title mark
  (same language as the config modal's `pane_block`), table + Detail borders
  DIM, cursor rendered in the terminal.
- Dashboard focused: current styling; terminal pane border DIM, no cursor.
- Help bar swaps per focus: terminal focus shows
  `ctrl-]/F12 back to dashboard · keys go to the shell`.

### Lifecycle

- Shell: `SHELL_FALLBACK_CMD` (`commands/exec.rs`, shared with a command-less
  `devsandbox exec <name>`) probes zsh, then bash, then POSIX `sh`, all as
  login shells (service images are often minimal). `TERM=xterm-256color` is
  set on the runtime client only; `exec` doesn't forward it, so the
  container shell gets the runtime's default (`xterm` on docker) unless the
  image or `remoteEnv` sets one. Instances reuse the exact `exec` argv the CLI
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
