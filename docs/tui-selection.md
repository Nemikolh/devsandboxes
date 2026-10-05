# Mouse text selection in the dashboard

**Status: steps 1-4 implemented; follow-up steps 5-8 planned.** User-facing behaviour: `docs/tui.md`,
*Mouse selection*; terminal specifics: `docs/tui-terminal.md`, *Mouse*.

## Problem

Text selection with the mouse does not work anywhere in `devsandbox` (the TUI dashboard).

- `src/tui/mod.rs:164` (`enter()`) turns on `EnableMouseCapture`, which puts the
  host terminal into mouse reporting (button, drag and SGR modes). From then on
  every press and drag goes to the app as an escape sequence, so the
  emulator never starts its own selection. Its Ctrl+Shift+C has nothing to copy.

- The app does use those events (`App::on_mouse`, `src/tui/app/mod.rs:452`):
  divider drags, tab clicks, card clicks, terminal focus and the wheel. But
  nothing builds a selection from them, and no code reaches the clipboard.

- **Integrated terminal.** `App::terminal_mouse` (`src/tui/app/terminal.rs:83`)
  handles a left press (focus or tab hit) and the wheel only. Presses, drags and
  releases are **never forwarded to the child**. So an app inside the terminal
  that does its own mouse selection (zidane, vim `mouse=a`, tmux) never sees the
  drag. That's why it "does not work" there either. A shell that has no
  mouse tracking gets no local selection either.

Holding Shift (Option on macOS) bypasses capture in most emulators. But the native selection then spans whole screen rows, borders and the next pane included, so it is not a fix.

## Goal (acceptance)

1. A left drag over text in any pane selects it, shown highlighted. The
   selection stays inside the pane it started in: borders, block titles, the tab
   bar, the help/status line, the prompt and other panes are never part of it.

2. Releasing the mouse copies the selected text to the system clipboard. So the
   copy is already done when the user reaches for the usual shortcut
   (Ctrl+Shift+C / Cmd+C), and pasting gives the text. If the shortcut does
   reach the app (an emulator that doesn't bind it), it copies the
   selection too.

3. **Markdown (Inbox thread pane):** the copy holds what the rendered content
   shows: list bullets and numbers, quote bars, table borders/columns, code
   block text, link text plus ` (url)`. Rows that were soft-wrapped by our own
   wrap are joined back into one line. The selection can extend beyond the visible
   rows (drag past the pane edge autoscrolls, the wheel scrolls while selecting),
   so a long message can be copied whole.

4. **Integrated terminal:** if the child has turned on mouse tracking, press,
   drag and release are forwarded to it in its protocol mode and encoding, and it
   does its own selection. Otherwise (a shell's prompt, `less` without mouse), the
   dashboard selects locally on the vt100 screen, honoring the scrollback
   offset, and copies the same way.

5. Existing clicks keep working: a press-and-release without movement is still a
   click (card select, tab switch, terminal focus, Inbox strip). Divider drags
   (config modal, Inbox split) still resize and never start a selection.

## Design

### Clipboard: OSC 52

The copy is `ESC ] 52 ; c ; <base64(utf8)> BEL`, written to stdout. It
needs no host clipboard, so it works over SSH, in a devcontainer and inside the
container. Supported by VS Code's terminal (since 1.93), kitty, WezTerm,
Alacritty, foot, iTerm2, Windows Terminal, Ghostty, and by tmux with
`set-clipboard`. Under tmux (`$TMUX` set) wrap it in the DCS passthrough
`ESC P tmux ; <seq with ESC doubled> ESC \`. Base64 is a small inline encoder
(no new crate: `Cargo.toml` has none, and the standard alphabet is ~20 lines).
Some terminals cap the payload (xterm-likes ~100 KB). Copy at most 1 MiB of
text and say so in the status line when cut. Linux-only `arboard`/`xclip`
fallbacks are out of scope (they don't reach the user's machine from a
container).

`App` stays I/O-free. A copy sets `App::clipboard: Option<String>`, and the event
loop in `src/tui/mod.rs` (after `on_mouse`/`on_key`, ~line 290) takes it and
writes the sequence. A status hint (`copied N chars`) confirms the copy, since
OSC 52 has no reply.

### Selection model: `src/tui/select.rs` (new, pure)

- **Regions.** Each draw records the selectable regions on screen: `Region { id:
  RegionId, rect: Rect, source: Source }`. `rect` is the **inner** text area, so
  borders and titles are outside by construction. `RegionId` is an enum naming
  the pane (`InstancesTree`, `Detail`, `ServicesTable`, `ServiceDetail`,
  `PortsTable`, `InboxList`, `InboxThread`, `Terminal`, `ConfigLeft`,
  `ConfigRight`, `TextModal`). The registry lives on `App` behind a `RefCell`,
  cleared at the start of `ui::draw`. This is the same interior-mutability
  pattern as `InboxView::set_pane_max` / `set_list_offset`
  (`src/tui/app/inbox.rs:136-218`), so the hit-test always matches what was drawn.
- **`Source`** says where the text comes from:
  - `Screen`: the cells of the rect in the frame buffer (tables, detail panes,
    cards). Selection coordinates are screen cells.
  - `Rows { scroll, rows: Vec<RowText> }`: a scrollable document whose full
    rendered rows the draw path already has (the thread pane, the config/inspect
    panes, help/logs). `RowText { text: String, soft_wrapped: bool }`, where
    `soft_wrapped` means "this row continues the previous logical line". Selection
    coordinates are `(content row, col)`, independent of scroll.
  - `Terminal`: the active vt100 screen. Text comes from
    `vt100::Screen::contents_between`, which already joins wrapped rows.
- **Selection** `{ region, anchor, head, dragging }`, where positions are
  `(row, col)` in the region's coordinates. A `Down(Left)` inside a region
  records a *candidate* anchor and changes nothing else. The first `Drag(Left)`
  to a different cell turns it into a selection. Until then the press is a click
  for the existing handlers. The head is clamped to the anchor's region, so the
  selection never crosses panes or borders. `Up(Left)` ends the drag and copies.
  A click elsewhere, `Esc`, a tab switch or a modal open/close clears it.
- **Extraction.** Linear (stream) selection, like a terminal: from anchor to
  head in reading order. Full rows in between, a partial first row and a partial
  last row. Trailing spaces are trimmed per row. A `Screen` row that a
  wide glyph continues skips the continuation cell (ratatui stores the spacer as
  `""`/skip). For `Rows`, a row with `soft_wrapped` joins the previous one with
  a single space instead of a `\n`. This matches our wrap, which drops the
  space at the break (`markdown::wrap`, `src/tui/markdown.rs:585`). Hanging
  indents and quote bars on continuation rows are part of the cont prefix
  (`cont` spans), so drop the prefix's width when joining: `RowText` carries
  `prefix: u16` (columns of `first`/`cont` decoration) for that.
- **Highlight.** After everything else is drawn, `ui::draw` patches the
  selected cells' style with `Modifier::REVERSED` in the frame buffer, using the
  region's rect and, for `Rows`, the scroll. It is drawn last so modals and
  panes can't hide it.

### Routing in `App::on_mouse`

Selection runs **first** in `on_mouse`, before the per-feature handlers. It
only consumes events while a drag-selection is live (`Drag`/`Up` after it
started) and lets the candidate press through. So a click on a card still
selects it, and a drag that starts on a card selects text instead of
re-selecting cards. Exclusions: when `dragging_divider` (config modal) or
`inbox.dragging` (Inbox split) is set, or the press is on a divider column, no
candidate is recorded. Over the terminal region with a mouse-tracking child,
the event is forwarded instead (step 3).

## Steps

### Step 1: selection core + clipboard, `Screen` regions everywhere

- New `src/tui/select.rs`: `RegionId`, `Region`, `Source::Screen` only (the enum
  can grow later), `Selection`, `extract_screen(&Buffer, Rect, from, to) ->
  String`, `highlight(&mut Buffer, …)`, ordered-range helpers. Unit tests:
  forward and backward drags, single row, multi-row partial ends, trailing-space
  trim, wide glyphs (`"日本"`), clamping to the region.
- New `src/tui/clipboard.rs`: `osc52(text: &str, tmux: bool) -> Vec<u8>` and the
  inline base64 encoder. Test vectors are RFC 4648 (`""`, `"f"` … `"foobar"`),
  a UTF-8 string, and the tmux wrapping.
- `App` (`src/tui/app/mod.rs`): `selection: Option<Selection>`,
  `regions: RefCell<Vec<Region>>`, `clipboard: Option<String>`. Selection
  handling goes at the top of `on_mouse` as described above (both the modal and
  non-modal paths). Clearing: on `Esc` in `on_key` when a selection exists
  (consume it, don't also close things), on `set_tab`, and on modal open/close.
- `ui.rs`: `draw` clears the registry, and every pane registers its inner rect:
  Instances tree + detail (`draw_tree`/`draw_detail`, ~1347/1623), Services
  table + detail (~313/436), Ports table (~527), Inbox list inner (~1010), thread
  pane `content` (~1125), terminal panel inner (~1237), config left/right
  (~1925/1939), text modal (~1980). Register the panes on the modal *only*
  while a modal is open, so a region hidden by a modal is never hit. At the end of
  `draw`, highlight the selection, and if a copy is pending, extract from
  `frame.buffer_mut()` into the `App` (a `Cell`/`RefCell` slot that `on_mouse`
  `Up` requests and the loop drains). That keeps extraction on the very buffer
  that was drawn, with no buffer clone per frame.
- `src/tui/mod.rs`: after event handling and the next draw, take the
  extracted text and write `clipboard::osc52(text, env TMUX set)` to stdout,
  then flush. Status: `copied N chars`.
- In this step every region is `Screen`, including the thread pane, config,
  text modal and terminal: they're refined in steps 2 and 3.
- Tests (in `app/` via `test_support`, and `ui.rs` with `TestBackend`):
  - drag on the Inbox thread pane then release yields the visible text, no `│`
    border or title;
  - a click without movement still selects a card / switches tab;
  - a divider drag doesn't select;
  - `Esc` clears.

### Step 2: `Rows` regions: markdown thread pane, config/inspect, help/logs

- `markdown::wrap` (`src/tui/markdown.rs:585`) and `markdown::render` need to
  say which output rows continue a logical line, and how wide the decoration
  prefix is. Pick the least invasive shape: e.g. a sibling
  `render_rows(md, width) -> Vec<(Line, RowMeta)>` with `render` mapping over it,
  so existing callers and tests are untouched. Code blocks (`hard` wrap) are
  also soft-wrapped continuations, but join with **no** space (the hard wrap
  keeps every space). Tables laid out as columns are one row per row, never
  joined.
- `ui::pane_rows` (`src/tui/ui.rs:1084`) / `draw_inbox_pane` (~1110): register
  the thread content as `Source::Rows` with the full row list and
  `app.inbox.scroll`. Raw mode (`m`) rows use the same metadata from
  `markdown::raw`.
- Config and inspect panes and the text modal register `Rows` built from their
  body lines (no wrap there: `Paragraph` without `wrap`, one row per line, cut
  at the pane edge). Copying a row gives the **whole line**, including the part
  past the right edge, when the selection's head is at or beyond the
  last visible column. Document this choice in the doc comment.
- Autoscroll: a `Drag` above or below a `Rows` region's rect scrolls it one row
  per event toward the pointer and moves the head. The wheel over the region
  during a selection scrolls and keeps the selection (positions are content
  rows). Scroll writes go through the existing state (`inbox.scroll`,
  `ConfigView::scroll`/`inspect_scroll`, `TextModal::scroll`) with their
  clamps.
- Tests: the copy of a rendered markdown doc holds bullets, `1.`, quote
  bar, table borders and code text. A soft-wrapped paragraph comes back as one
  line. A selection across a scroll returns rows that were off-screen. The raw
  mode copy equals the source lines.

### Step 3: integrated terminal: forward to mouse-tracking children, local select otherwise

- `src/tui/term.rs`: generalize `encode_wheel` (~402) into
  `encode_mouse(kind, button, modifiers, col, row, encoding)` that covers press,
  release (SGR `m`, legacy button 3) and motion (+32), keeping `encode_wheel`'s
  tests green, either as a thin wrapper or by updating them. Respect
  `vt100::MouseProtocolMode`:
  - `Press`: presses only;
  - `PressRelease`: presses and releases;
  - `ButtonMotion`: those plus drag;
  - `AnyMotion`: also plain motion (crossterm only reports `Moved` if we enable
    `?1003`; we don't, so treat it as `ButtonMotion`).
- `App::terminal_mouse` (`src/tui/app/terminal.rs:83`): inside the body (not the
  title row) with a live session, if `mouse_protocol_mode() != None`, forward
  Down, Drag and Up for all buttons at the pane-relative cell, and do **no**
  local selection. The existing focus-on-click stays. Otherwise the terminal
  region is `Source::Terminal`: local selection over the screen, text via
  `contents_between` (joins wrapped lines). The `scrollback()` offset is part of
  the coordinates, so the wheel during a selection works as for `Rows`.
- The child sees releases even when the drag ends outside the panel: clamp
  the forwarded coordinates to the body.
- Tests: SGR, legacy and UTF-8 encodings for press, release and drag. Routing: a
  tracking child receives the bytes (`TermSession::take_written`) and no
  selection forms. A non-tracking shell gets a local selection whose copy equals
  the screen text.

### Step 4: keyboard copy, help and docs

- Copy shortcut: in `on_key` (and `on_key_terminal` *before* encoding to the
  PTY), Ctrl+Shift+C (crossterm reports `Char('C')` or `Char('c')` with
  `CONTROL|SHIFT`, depending on kitty flags) and `super+c` copy the live
  selection again. Without a selection, the terminal key goes to the PTY as
  today.
- `?` help (`src/tui/app/view.rs:~57-94`): one line, `drag  select & copy ·
  shift+drag  terminal's own selection`.
- Docs: `docs/tui.md` (selection section), `docs/tui-terminal.md`
  (forwarding/local selection), the site page if the dashboard docs cover
  mouse (`site/`, check `docs/dashboard-docs.md`). `CHANGELOG.md` `## Unreleased`
  → one "Added" entry. **The worktree already has unrelated `CHANGELOG.md`
  edits: stage only this hunk** (`git add -p`).

## Follow-up (feedback after steps 1-4)

Feedback from using it:

1. In the integrated terminal, selecting in a mouse-tracking app (zidane) works,
   but its copy never reaches the clipboard. In a login shell, where our local
   selection is used, it does. **Root cause:** a terminal app copies by sending
   OSC 52 to *its* terminal, which here is our vt100 emulator. `KittyState`, our
   `vt100::Callbacks` impl (`src/tui/kitty.rs:173`), doesn't implement
   `copy_to_clipboard`, so vt100 parses the sequence (`perform.rs`, `[b"52",
   ty, data]`) and it is dropped. Ctrl+Shift+C does reach the app: with kitty
   flags it's encoded as `CSI 99;6u` (`kitty::encode_key`).
2. Copy on release should be an **option, off by default**. Copy then happens
   with Ctrl+Shift+C / Cmd+C (or Ctrl+C over a dashboard selection, for legacy
   terminals). The dashboard has no settings yet: add them, shown in the `?`
   modal, which becomes **Settings & help**.
3. The review fixes go into `CHANGELOG.md`.
4. Forward hover (button-less motion) to apps that ask for it.

### Step 5: relay terminal apps' OSC 52 to the outer clipboard

- `KittyState` (it's the session's only `vt100::Callbacks`) implements
  `copy_to_clipboard(screen, ty, data)`. It queues `data`, which is already
  base64 (vt100 checks the alphabet), in a bounded field next to `replies`,
  e.g. `clipboard: Option<Vec<u8>>`. The newest copy wins, capped at the same
  1 MiB as `clipboard::MAX_COPY` (base64 length ≈ 4/3 of that). The selector
  (`ty`) is ignored and always written as `c`. This works whether or not the
  kitty emulation is enabled: clipboard relay is not a kitty feature, so
  `enabled` must not gate it. `paste_from_clipboard` (OSC 52 *reads*) stays
  unanswered: a container app must not read the host clipboard.
- `TermSession` exposes `take_clipboard() -> Option<Vec<u8>>` (locks the
  parser, takes the field). The event loop (`src/tui/mod.rs`, next to the
  existing `app.take_clipboard()` after `terminal.draw`) drains every session.
  It writes the payload through a new `clipboard::osc52_base64(b64, tmux)`
  that shares the framing and tmux wrapping with `osc52`. The status line says
  `copied from <tab title>`. All of this stays I/O-free in `App` (go through
  `app.terms`).
- Gate: a setting `terminal_clipboard` (default **on**, since that's what
  real terminals do and what the feedback expects). Step 5 lands before the
  settings exist (step 6), so it hard-codes on; step 6 wires the setting.
  *(Orchestrator's suggestion, not in the feedback: an app in a container
  writing the host clipboard is worth an off switch.)*
- Tests: feeding `\x1b]52;c;aGk=\x07` to a test session queues `aGk=`. A
  `?` read queues nothing. The newest copy wins. Oversized payloads are
  dropped. `osc52_base64` framing works with and without tmux.

### Step 6: dashboard settings + copy-on-select option (off by default)

- New `src/tui/settings.rs`: `Settings { copy_on_select: bool (false),
  terminal_clipboard: bool (true) }`, serde with `#[serde(default)]` per field
  so an older or partial file loads. It's persisted per user at
  `<data>/devsandbox/dashboard.toml`, using `State::path`'s base-dir logic, like
  `prompt::history_path` (`src/tui/prompt.rs:449`). Load once at startup;
  a missing or invalid file means defaults, plus a status-line warning if it's
  invalid. Save best-effort from the event loop when `App` marks the settings
  changed, so `App` stays I/O-free.
- `?` modal → **Settings & help** (`src/tui/app/view.rs:366` `open_help`,
  `HELP_BODY` at `:16`, drawn by `draw_text_modal` in `src/tui/ui.rs`).
  - A settings section on top: one row per setting, `[x] copy on select —
    releasing a mouse selection copies it`, a cursor row, `space`/`enter`
    toggles and saves. Below it, the existing help text, scrolling as today.
  - Keep the help body selectable (its `Rows` region).
  - Model it as data (a `SETTINGS` table of `(key, label, get, set)`) so a new
    setting is one entry.
  - Update the bottom help line for the modal (`ui.rs` ~1846).
- Selection: on release, copy only if `copy_on_select`. Otherwise keep the
  selection and show `selected N chars — ctrl-shift-c copies` (count from the
  extraction, i.e. extract without sending). Step 5's relay checks
  `terminal_clipboard`.
- Tests: defaults, loading a partial or invalid file, a toggle marks the
  settings for saving, release with the option off copies nothing (with the
  hint), and with it on copies. Update the existing selection tests, which
  assume copy on release: they set the option on, or use the key.

### Step 7: hover forwarding to `AnyMotion` children

- crossterm's `EnableMouseCapture` already enables `?1003`, so
  `MouseEventKind::Moved` arrives. Today it is dropped.
- `term::encode_mouse` (`src/tui/term.rs` ~440): motion with no button is
  button 3 + 32 (= 35) plus the modifier bits; SGR final `M`. Sent only when
  the mode is `AnyMotion`. `ButtonMotion` keeps dropping it.
- `App::terminal_mouse`: forward `Moved` over the body of the active live
  session to an `AnyMotion` child. Skip it when it's the same cell as the last
  forwarded motion (crossterm reports every pixel-cell change; this avoids
  duplicate reports). Nothing outside the body: there's no leave event in the
  protocol.
- Tests: encodings for motion, mode filtering, routing, duplicate suppression.

### Step 8: CHANGELOG and docs pass

- Update the selection bullet in `## Unreleased` → *Added*: copy on select is an
  option (off by default) in the new Settings & help screen (`?`); otherwise
  Ctrl+Shift+C / Cmd+C copies (Ctrl+C over a dashboard selection in terminals
  that send it plain). Terminal apps can set the clipboard. Hover reaches apps
  that ask for it.
- *Fixed* entries for pre-existing bugs found in review:
  - A press on the Inbox thread pane's first text column grabbed the
    list/thread divider. It now grabs only on the borders.
  - With help or logs open, clicks and the wheel reached the hidden integrated
    terminal underneath.
  - Apps in the integrated terminal never received clicks or drags, and their
    clipboard copies (OSC 52) were dropped.
- The tab-switch clearing and the Ctrl+C copy fixes are part of the new feature,
  so they're covered by the *Added* bullet, not *Fixed*.
- Docs: `docs/tui.md` (*Mouse selection*, new *Settings* section),
  `docs/tui-terminal.md` (*Mouse*: hover, OSC 52 relay), and the site
  (`site/src/content/docs/dashboard.mdx`: the `?` row, the drag row).
  Run `cd site && pnpm check && pnpm build`.

## Parked (not in scope)

- Double-click word / triple-click line selection.
- Keyboard-driven selection (vi-style visual mode).
- Host clipboard fallbacks (`arboard`, `wl-copy`, `xclip`) for terminals without
  OSC 52.
- Copying the markdown *source* for a selection in rendered mode (raw mode `m`
  already gives source).
