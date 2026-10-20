# Mouse selection and copy

How text selection and copying work in the dashboard, which captures the
mouse and so has to select and copy itself. Read this before adding a
pane (it must register a region) or touching clipboard output.

Code: `src/tui/select.rs` (`RegionId`, `Source`, `Region`, `Pos`,
`Selection`, `Candidate`, `RowText`/`RowMeta`/`Join`, `extract`,
`highlight`, `terminal_top`), `src/tui/app/selection.rs`
(`selection_mouse`, `selection_press`, `selection_key`, `is_copy_key`,
`copy_key_intercepted`, `Extract`, the renderer hooks `clear_regions` /
`add_region` / `add_rows_region` / `add_terminal_region`),
`src/tui/clipboard.rs` (`osc52`, `osc52_base64`, `cap`, `MAX_COPY`,
`MAX_COPY_BASE64`), `src/tui/settings.rs`, `src/tui/mod.rs` (the event
loop's clipboard writes), `src/tui/ui.rs` (`selectable` and the other
region registrations).

## Why

`enter` turns on `EnableMouseCapture`, so the outer terminal reports every
press and drag to the dashboard and never starts its own selection
(shift+drag bypasses capture in most emulators but spans whole screen rows,
borders included).

## Regions

Each draw registers the selectable panes of that frame (`App::regions`, a
`RefCell` since drawing borrows `App` immutably), one per `RegionId`:
the instances tree, Detail, services table and detail, ports table, machines table, Inbox
list, thread header and feed, terminal body, config modal panes, text
modals. A press is hit-tested against what is on screen. A `Region`'s
`Source` says where its text comes from:

- `Screen`: the frame buffer cells, as drawn.
- `Rows { scroll, rows }`: a scrolled document the draw had in full
  (`RowText`, with `Join` saying how a wrapped row continues the previous
  one), so a selection survives scrolling and can span off-screen rows;
  a drag past the top or bottom autoscrolls one row per event.
- `Terminal { top }`: the integrated terminal, rows counted from the
  oldest scrollback line, text read from vt100 (`extract_terminal`).
  Registered only while the child doesn't track the mouse.

Positions (`Pos`) are region-relative, so a selection finds its region
again after a redraw moved it.

## Routing

`App::on_mouse` gives selection first say (`selection_mouse`): a drag
from a `Candidate` press to another cell starts a `Selection`, further
drags move its head (clamped to the region), and the release ends it.
While no drag-selection is live it consumes nothing, so a press is still a
click everywhere; `selection_press` then runs after the other handlers,
clears the old selection and records a candidate, unless the press grabbed
a divider or went to a mouse-tracking terminal child
([terminal.md](terminal.md#mouse)). `esc` clears a selection (except in a
focused shell, which owns `esc`), as do a press elsewhere, a tab switch, a
modal opening or closing, a terminal tab change, and a thread change.

## Copy

The text is read off the frame just drawn (`take_extract_request` in
`ui::draw`, `select::extract`), so the copy is exactly what the highlight
covers. The release asks for `Extract::Copy` with `copy_on_select` on,
else `Extract::Measure` (a `selected N chars` hint naming a key that
works). The copy key (`is_copy_key`: ctrl+shift+c, or cmd/super+c under
kitty flags) always copies; over a dashboard pane plain ctrl+c copies too
(a legacy terminal sends ctrl+shift+c as it), and quits once the
selection is gone. `copy_key_intercepted` guesses from env vars whether
the outer terminal keeps ctrl+shift+c for itself; it changes wording only.
An all-blank selection copies nothing.

## OSC 52

The event loop writes the text to the outer terminal as OSC 52
(`clipboard::osc52`), cut to 1 MiB on a char boundary (`cap`), wrapped in
tmux's DCS passthrough when `TMUX` is set. OSC 52 has no reply, so the
status line says `copied N chars`. A local dashboard also pipes the text to
the host clipboard command (`clipboard::host_helper`: `pbcopy` on macOS,
else `wl-copy` under `WAYLAND_DISPLAY`, `xclip -selection clipboard` under
`DISPLAY`; none when `SSH_CONNECTION`/`SSH_TTY`/`SSH_CLIENT` is set, the
host there being the remote), spawned by `copy_on_host` in `tui/mod.rs`, so a
copy lands even when the outer terminal ignores OSC 52 (iTerm2's default).

Apps inside the integrated terminal copy with OSC 52 to the vt100
emulator; `KittyState` queues it (reads and payloads over
`MAX_COPY_BASE64` dropped) and the event loop relays the last copy per
frame (`take_terminal_clipboard`) when `terminal_clipboard` is on: as OSC
52, and decoded (`decode_base64`) to the same host clipboard command. Remote
tabs (ssh to a machine, then `docker exec`) carry the sequence unchanged, so
an app in a machine's sandbox copies to this host the same way.

## Settings

Both toggles live in `dashboard.toml` ([overview.md](overview.md#settings)):

- `copy_on_select` (default on): most Linux terminals keep ctrl+shift+c,
  and in the integrated terminal ctrl+c is the shell's, so the release is
  the one copy that always works. Off: only the copy keys copy.
- `terminal_clipboard` (default on): relay terminal apps' OSC 52, as real
  terminals do; off because it lets an app in a container write the host
  clipboard.

Limitations: [status.md](../../status.md#dashboard)
