# Plan: `ls` rendering + interactive dashboard

**Status: implemented through step 7 (all steps done).**

Repo: `devsandbox` (Rust, clap CLI managing devcontainer sandboxes).
Key modules: `src/config.rs` (TOML config, templates/`extends`, `resolve_sandbox`),
`src/state.rs` (instances in `~/.local/share/devsandbox/state.toml`),
`src/docker.rs` (docker CLI wrappers), `src/commands/*` (ls, ps, run, rm, exec, services).

Constraints for the implementing agent:
- **Coding only. No git operations whatsoever.** The orchestrator reviews and commits.
- Match existing style: `anyhow::Result`, `Context`, plain modules, tests in `#[cfg(test)]`.
- Run `cargo build` and `cargo test` before reporting a step done.
- New deps only as listed per step (`ratatui`, `crossterm`); everything else stays std.

## Step 1 — `ls`: empty-state message + colored invisible table  ✅ done

`src/commands/ls.rs` currently prints raw `\t`-separated fields.

- No sandboxes in config → print `no sandboxes defined in <dir>/devsandboxes.toml` (stderr? no: stdout, exit 0).
- Otherwise render a borderless ("invisible") table: column widths computed from content,
  two-space gutter, dim uppercase header row.
- Columns: `NAME`, `SOURCE`, `FOLDER`, `SERVICES` (comma list, `-` when none),
  `EXTENDS` (template chain from the raw sandbox table, `-` when none).
- Colors via raw ANSI (no dep): name bold cyan, `image …` green / `dockerfile …` yellow,
  folder default, services magenta, header + `-` dim.
- Color only when stdout `IsTerminal()` and `NO_COLOR` is unset. Helpers live in a new
  `src/render.rs` (small: `style(s, code)`-style functions) so later steps can reuse them.

## Step 2 — Dashboard scaffold (`devsandbox` with no args)  ✅ done

- `main.rs`: `command: Option<Command>`; `None` + stdin/stdout are TTYs → launch dashboard;
  `None` without TTY → print clap help, exit 2.
- Add deps: `ratatui`, `crossterm` (latest, default features).
- New module tree `src/tui/{mod.rs,app.rs,ui.rs}`:
  - terminal setup/teardown (alternate screen, raw mode, panic hook restoring the terminal),
  - event loop: crossterm events + 2s tick for data refresh,
  - `App` state machine: active tab (`Instances` | `Services`), selected row per tab,
  - chrome: tab bar on top, bottom help bar (`q quit · tab switch · ↑↓ select …`).
- Tabs render placeholder content this step; real data lands in steps 3–4.

## Step 3 — Instances view  ✅ done

Data: `State::load()` joined with docker (`docker inspect` / one `docker ps --format json`
call, plus `docker stats --no-stream` for cpu/mem). Collection runs on the tick in a
background thread (mpsc into the event loop) so the UI never blocks on docker.

Table columns: instance name, sandbox config it's based on, container status
(green running / red exited / dim missing), uptime (from `created_unix`), CPU%, MEM,
folder (worktree marker `⎇` when `worktree` is set), services attached.
Detail panel below the table for the selected instance: container name, workspace,
remoteUser, remoteEnv count, base folder vs worktree, config-drift warning (compare
`ResolvedSandbox.config_hash` against the container's label — see `warn_on_drift` in
`src/commands/run.rs`).

## Step 4 — Services view  ✅ done

From `Config::load(dir)` + docker: service name, scope (`global`/`isolated`),
source (image or dockerfile), ports, container status per running instance,
which live instances reference it. Same table/detail-panel pattern as step 3.

## Step 5 — Config explorer modal  ✅ done

- From either tab, `enter`/`e` on a sandbox (or the sandbox behind an instance) opens a
  full-screen modal showing its TOML.
- `tab` toggles **original** (the raw `[sandbox.<name>]` table as written, plus its
  `extends` names) vs **resolved** (the merged table re-serialized with `toml`; use the
  merge result from `resolve_extends` before typed validation so it round-trips).
- Title shows which side is displayed + the config hash. `↑↓`/`pgup`/`pgdn` scroll,
  `esc`/`q` closes.
- Light TOML highlighting (section headers, keys, strings) done manually over lines.

## Step 6 — Command prompt: run / exec / code, history + completion  ✅ done

- `:` opens a one-line prompt in the bottom bar. Commands:
  - `run <sandbox> [--name n]` — start instance,
  - `exec <instance> <cmd…>` — run inside instance,
  - `code <instance>` — open VS Code attached to the container:
    reuse `write_vscode_name_config` (run.rs) then
    `code --folder-uri vscode-remote://attached-container+<hex(container)>/<workspace>`,
    where `hex(container)` is the lowercase hex of the container name's UTF-8
    bytes (see `hex_encode` in `src/tui/mod.rs`). `code` is spawned detached (no
    TUI suspend); its outcome shows as a one-line status in the help bar.
  - `rm <instance>`.
- Execution suspends the TUI (leave alt screen + raw mode), runs the existing command fns
  (`commands::run::run`, `commands::exec::exec`, …) with inherited stdio, waits for a
  keypress, re-enters the TUI and forces a data refresh.
- `tab` completion: first token from the command list; argument token from sandbox names
  (`run`) or instance names (`exec`, `code`, `rm`). Cycle on repeated tab.
- History: `↑↓` in the prompt; persisted to `<data dir>/devsandbox/prompt_history`
  (same base-dir logic as `State::path`), capped at 200 entries, deduped consecutive.

## Step 7 — Polish + extras  ✅ done

- `?` help overlay listing all keys (now *Settings & help*, see *Settings* below).
- Error toast (bottom-right, auto-dismiss on next key) instead of crashing on docker/config
  errors; dashboard must start fine with no devsandboxes.toml, no docker, or empty state.
- Logs preview: `l` on an instance shows last ~50 container log lines in the modal.
- Header line with totals: N sandboxes, N running / N stopped instances, N service
  containers, docker version (cached).

## Phase 2 — Instances tab becomes a sandbox tree

### Step 8 — Tree data model + rendering

The Instances tab currently lists instances flat. It becomes a tree keyed on
sandboxes from config, instances nested below:

- `Snapshot` gains `sandboxes: Vec<SandboxRow { name, source, folder, services,
  extends, config_hash }>` filled from `Config::load` + `resolve_all` (every
  configured sandbox appears, even with zero instances). Config load failure →
  existing error path, empty sandbox list, instances still shown (see orphans).
- App holds `collapsed: BTreeSet<String>` (sandbox names; default expanded),
  preserved across snapshot refreshes. A pure function flattens
  (sandboxes, instances, collapsed) into visible nodes:
  `Node::Sandbox(i) | Node::Instance(i) | Node::EmptyMarker(sandbox)`.
  Instances whose sandbox is not in config group under a synthetic dim
  `(not in config)` sandbox node at the bottom.
- Dispatcher children (docs/automations.md) stay under their own sandbox
  group; the row gets a dim `⇠ <dispatcher instance>` suffix, or
  `⇠ <id> (orphan)` once the owner id is gone from state
  (`data::dispatcher_label`, fed by the snapshot's `instance_id`/`dispatcher`).
- Selection indexes the visible-node list; clamps on refresh AND on
  collapse/expand. Row count for the Instances tab = visible nodes.
- Keys: `→`/`space` expand, `←` collapse (on an instance: jump to its sandbox),
  `space` toggles.
- Sandbox line: `▸/▾ <name>` + stats: `N/M running`, source (colored like ls),
  folder. Expanded sandbox with zero instances shows one dim child line
  `no instances — : run <name>`.
- Instance line, indented: existing columns (status, uptime, cpu/mem, folder,
  worktree marker).
- Detail panel: sandbox selected → sandbox summary (source, folder, services,
  extends, config hash, instance count); instance selected → existing detail.

### Step 9 — Tree interactions

- `enter`/`e` on a sandbox node → config modal for that sandbox (existing
  original/resolved split, `build_sandbox_view` already takes a name).
  On an instance node → sandbox config as before. On the `(not in config)`
  group or its children → no config; instance nodes there still get `l` logs.
- `r` on a sandbox node opens the prompt prefilled `run <name> ` (cursor at
  end); `r` on an instance prefills `run <its sandbox> `.
- `o` on an instance node = VS Code attach (same path as the `code` prompt
  command). `l` logs stays instance-only.
- Help overlay + help bar updated for the tree keys.

## Phase 3 — Ports tab (on-demand port forwarding)

See `docs/port-forwarding.md` for the engine. The dashboard gains a third tab.

- `Tab::Ports` sits next to Instances and Services: `1/2/3` jump to a tab and
  `tab`/`S-tab` cycle over all three (no longer a two-tab toggle).
- Table columns: `LOCAL` (`127.0.0.1:3000`), `TARGET` (route label, already
  carrying any `(via instance …)` suffix — no separate VIA column), `PROCESS`
  (`node (pid 412)`, `-` when unknown), `STATE` (active green / connecting
  yellow / error red), `CONNS`. Empty state points at `p` / `:port`.
- Running instances' `forwardPorts` are forwarded automatically, on the host
  port saved for them in `state.toml` (docs/port-forwarding.md, _Configured
  forwards_); their `TARGET` carries a dim `(config)`.
- Keys:
  - `p` on an Instances row (not a process row) opens the prompt prefilled
    `port <instance> `; `p` on a Services row prefills
    `port <first used_by instance> --service <svc> ` (blank instance slot when
    the service has no user).
  - `d` on the Ports tab stops the selected forward (a configured one stays
    stopped until the dashboard reopens).
- Prompt command
  `port <instance> [--service s] [--address a] <[host:]port>`: an instance is
  required (a global service is reached by naming any instance that references
  it); `--service` completes service names. The port spec is validated on submit
  (`p` or `h:p`, non-zero u16s) via `commands::port::validate_port_spec`; a bad
  spec is an inline prompt error. Submitting switches to the Ports tab and hands
  the request to the forwarder worker (it never suspends the TUI).

## Phase 4 — Inbox tab (notifications and dispatcher threads)

Transport: `docs/automations.md` ("`devsbd notify`", _Inbox threads_);
threads, events and their design: `docs/inbox-threads.md`. The Inbox shows the
shared store (`inbox.toml` next to `state.toml`, `src/inbox/`), which every
dashboard reads and writes under a lock: the bridge worker applies each
delivered record to it, then pokes the event loop, which reloads (also
whenever the file's mtime or length moves, e.g. another dashboard dismissed
something) and shows the record briefly on the status line. `App`
(`src/tui/app/inbox.rs`) only holds view state: the view, the selection, the
open thread. `d`/`D`/mark-read and the pane's actions are `inbox::Op`s the
event loop applies through `inbox::ops::apply`. Desktop popups fire from the
worker, never the UI thread.

- `Tab::Inbox` is the fourth tab: `4` jumps to it, `tab`/`S-tab` cycle over
  all four. Its title carries the **needs-you** count (`Inbox (3)`), and an
  Instances row shows the same count for that instance as a yellow `✉N`
  after its name (matched by `instance_id`, so a renamed instance keeps it).
  Needs-you = dispatcher threads in state `needs-you` plus unread notify
  records; archived threads never count. Unread alone doesn't, so a
  dispatcher re-asserting threads can't inflate it.
- **Layout: side by side, always.** The list is on the left, the selected
  thread's pane on the right — no open/close, the pane always shows whatever
  the cursor is on (`src/tui/ui.rs` `draw_inbox`/`inbox_areas`). The divider
  can be dragged with the mouse (`InboxView::split_pct`, `src/tui/app/inbox.rs`
  `inbox_mouse`), clamped 20-80%, default 40%, not saved. With terminals open,
  the split sits in the area above the terminal panel, which stays where it is
  on every tab.
- **Mouse.** A press off the divider is a click (`inbox_click`, hit-tested by
  `ui::inbox_hit` from the draw path's own layout fns and last list offset):
  a card selects its thread and focuses the list (the spacer row hits
  nothing), a view name in the strip switches to it (`‹`/`›` step), the pane
  focuses the thread, the input box the input (left like `esc` by a click on
  the pane), the hint row the thread. The wheel moves the selection one card
  over the list and scrolls the pane over it. A click on a tab title (any
  tab, `ui::tab_hit` over `ui::tab_spans`, which the tab bar is drawn from)
  switches tabs like `1`–`4`. None of it under a modal or the prompt; a
  click outside the terminal panel still unfocuses a terminal.
- **Cards**, one per thread, last change first, two lines plus a blank
  spacer (`draw_inbox_list`, `card_lines`): line 1 is the title (bold while
  unread, `↗` when there's a link, `· archived` when the sender is gone) with
  a compact age on the right (`now`, `5m`, `3h`, `2d`, then a date like
  `Oct 12` past a week, `short_age`); line 2 is the state or level chip
  (`●`/`○`/`✓` then the status, or without one `needs you`/`active`/`done`;
  `▲ warn`, `✖ error`, `· info`; one function, `tui::app::inbox::chip`, shared with
  the pane head) with the sender on the right. The selected card gets an accent
  bar in the first column and a background tint over both lines; archived
  cards are dimmed throughout.
- **Views**, stepped with `←`/`→` (clamped at the ends, no wraparound; `v` no
  longer cycles them), shown as a one-line strip above the list:
  `‹ Needs you 3 │ Active 5 │ Done │ All ›`, zero counts left out, the
  current view accented. **Needs you** (default), **Active**, **Done**,
  **All**. Archived threads and read notify records only show in All.
- **Read.** A thread is read when it becomes the selected one (it's on
  screen in the pane), and again whenever it changes while selected. Notify
  records are also marked read when you leave the Inbox from Needs you or
  All, since they were on screen: a notify record has no state to resolve
  it, so it would otherwise sit in Needs you until selected. The selected
  thread stays listed until the cursor leaves it, so the next row doesn't
  slide under the cursor and get read in turn.
- List keys: `↑`/`↓` select; `←`/`→` step the view; `enter` focuses the
  thread pane; `r`/`i` focus its reply input (when the thread takes
  replies); `o`/`t`/`l`/`p` target the selected thread's child (or its
  sender with none); `d` dismisses a notify thread with its history, or
  marks a dispatcher thread done (with an event, and its child done); `u`
  reopens a done thread; `D` clears every notify thread (dispatcher threads
  are state their dispatcher re-asserts, so they stay); `1`–`4` still switch
  tabs.

Keys go to one of three focus zones (`InboxFocus`): **List** (above), the
**thread pane**, and its reply **input**. `enter` on the list moves focus to
the thread; `esc` steps back one zone at a time (input → thread → list). The
focused zone's border is highlighted. While the thread or its input has
focus it shadows the dashboard keys (a tier in `App::on_key` after the
terminal, like a modal): the `1`–`4` tab keys and the rest act on the
thread, not on rows the user can't see. `q`, `:` and `?` stay reachable. The
pane shows the title; the card's chip and the time; `from` (sender,
`(archived: instance removed)`); `link`; `child` (resolved among the
sender's own children, with its run state, or `(no such child)`); the
message; the timeline (put changes, actions, replies, done/reopen; for a
notify thread, its earlier records); `N events waiting for <sender>` until
the dispatcher acks them; the numbered actions (`⌂ host` runs in the
dashboard, `→ <sender>` sends an event, `✓ done`).

**Markdown.** Dispatcher text is often LLM output, so the pane renders it
(`src/tui/markdown.rs`, `pulldown-cmark`): the message and notify bodies as
blocks (bold accented headings, emphasis, tinted inline code and code
blocks, bullet/numbered lists with hanging indents, `│` quotes, dim rules,
links as underlined text plus a dim `(url)`, raw HTML literal, images as
their alt text, tables as aligned columns under a bold header and a dim
rule when they fit, widest columns cut to 8 with `…` first, else their
source lines), and a reply's full text the same way, under its timeline
row; the title, status, other timeline rows and the cards' title/status
inline only (code, emphasis, links as text,
one line). Every pane line is word-wrapped by display width by the renderer
itself, so the scroll bound is the exact row count. `m` shows the source
instead (pane title `Thread · raw`), for every thread, this session only.
Container text has its control characters stripped when stored
(`inbox::sanitize`, in `Inbox::apply_sink`) and again when drawn.

| key | does |
|---|---|
| `esc` | back to the list |
| `↑`/`k` `↓`/`j`, `pgup`/`pgdn`, `g`/`G` | scroll |
| `enter` | open the link (`xdg-open`, `open` on macOS; only `http(s)://`: the link comes from the container) |
| `1`–`9` | run that action: a host verb (`vscode`, `terminal`, `logs`, `forward`, `open`, `rm`) runs at once on the thread's child, else on its sender; the rest is an event for the sender (`src/tui/app/thread_actions.rs`) |
| `o` `t` `l` `p` | VS Code / terminal / logs / forward prompt on the child, or the sender without one |
| `r`/`i` | focus the reply input, when the thread takes replies; otherwise a status hint and focus stays |
| `d` | mark done: an event, and the child marked done |
| `u` | reopen a done thread: back to `active`, an event, and the child's done mark cleared |
| `m` | toggle rendered markdown / raw source (all threads, not saved) |

**The reply input** sits at the bottom of the thread pane and is only drawn
when the thread sets `reply` (`draw_inbox_pane`/`draw_reply_input`,
`src/tui/ui.rs`); a thread without one gets a one-line dim hint in its place,
a notify thread gets nothing. It wraps what's typed by display width
(`src/tui/textarea.rs`) and grows from one to six rows, then scrolls to keep
the cursor in view. `enter` sends a non-empty reply (the same `Op::Reply`)
and keeps the input focused and empty for the next message; `alt-enter` (or
`shift-enter`, where the terminal reports it) inserts a newline; `esc` steps
back to the thread. Its editing keys are the `:` prompt's, with `↑`/`↓` and
`home`/`end` moving on the wrapped rows. The `:` prompt itself scrolls
sideways once the line outgrows the bar.

Archived threads refuse actions, replies, `d` and `u` with a status line. A
host action whose child isn't found is refused rather than run on the sender,
and sends no event. The `rm` verb goes through the `:rm` path (the CLI's own
confirm on the suspended screen).

## Phase 5 — done instances, VS Code at a line

- An instance marked done (`state::Instance::done`) stays in the tree,
  dimmed, with a `✓` after its name. `d` / `u` on an Instances row mark it
  done / clear it, written off the UI thread (`commands::done::set_saved`);
  the same flag is set by `devsandbox done|undone`, `devsbd done`, and a
  thread's done/reopen on its child.
- `:code <instance> [--goto path[:line[:col]]]` opens VS Code and then the
  file (relative to the instance's workspace folder) at the line, via
  `devsbd vscode-goto` in the container (`docs/sandbox-helper.md`). It runs
  in the background, since the in-container half waits up to 30 s for the
  window to attach; the outcome is the status line (`opened VS Code at
  <path>:<line>`, or `opened VS Code; --goto dropped (<reason>)`). The
  thread `vscode` action passes its `path`/`line`/`col` the same way; `o`
  opens no file.

## Mouse selection

Design and steps: `docs/tui-selection.md`. Code: `src/tui/select.rs` (model,
extraction, highlight), `src/tui/app/selection.rs` (routing),
`src/tui/clipboard.rs` (OSC 52), `src/tui/settings.rs` (*copy on select*).

- **Select.** A left drag in any pane selects text inside that pane only:
  its inner text area, so borders, block titles, the tab bar, the help/status
  line, the prompt and other panes are never part of it. The selection is
  shown reversed. A press and release without movement is still a click
  (card, tab, terminal focus); a divider drag still resizes.
- **Release.** By default (*copy on select* on, *Settings*) releasing copies
  and the status line says `copied N chars`. With the setting off, releasing
  copies nothing, so a stray drag never clobbers the clipboard: the selection
  stays and the status line names a copy key, `selected N chars —
  ctrl-shift-c copies` (see *Copy-key wording* below for the other forms).
  The selection stays shown until `esc` (except in a focused terminal: the
  shell owns its `esc`), a click elsewhere, a tab switch, a
  modal opening or closing, another Inbox thread, `m` (rendered ⇄ raw) or
  another terminal tab clears it.
- **Copy keys.** `ctrl-shift-c`, or `cmd-c` / `super-c`, copies the live
  selection whatever the setting, everywhere (prompt, modals, a focused
  terminal included). Crossterm sees `ctrl-shift-c` and `super-c` only from
  a terminal speaking the kitty keyboard protocol (docs/tui-terminal.md,
  *Keyboard*), and many emulators bind them to their own copy, which under
  mouse capture has nothing to copy; there, *copy on select* is the way. A
  legacy terminal that doesn't bind it sends plain `ctrl-c`, so over a
  dashboard selection `ctrl-c` copies too instead of quitting (a focused
  shell or the prompt keeps its `ctrl-c`; with no selection it quits as
  before). That fallback is also the key that works in terminals binding
  `ctrl-shift-c` themselves.
- **Copy-key wording.** At startup the dashboard guesses whether the outer
  terminal binds `ctrl-shift-c` to its own copy by default
  (`copy_key_intercepted` in `src/tui/app/selection.rs`), from the env vars
  each one sets: Alacritty (`ALACRITTY_WINDOW_ID`, `ALACRITTY_SOCKET`,
  `TERM=alacritty`), kitty (`KITTY_WINDOW_ID`, `TERM=xterm-kitty`), WezTerm
  (`WEZTERM_PANE`, `WEZTERM_EXECUTABLE`), VTE terminals such as GNOME
  Terminal (`VTE_VERSION`), Konsole (`KONSOLE_VERSION`), foot
  (`TERM=foot`/`foot-extra`), Ghostty (`TERM_PROGRAM=ghostty`,
  `GHOSTTY_RESOURCES_DIR`), Windows Terminal (`WT_SESSION`), VS Code
  (`TERM_PROGRAM=vscode`). If so, the release hint, the help's copy line and
  the *copy on select* help name `ctrl-c` instead; over the integrated
  terminal's local selection, where `ctrl-c` is the shell's and
  `ctrl-shift-c` never arrives, the hint says `selected N chars — turn on
  copy on select (?) to copy here`. Under tmux or ssh the vars may be missing
  or stale, so the guess can be wrong; it only changes wording, every copy
  key behaves the same.
- **Documents** (the Inbox thread pane, the config/inspect modal, the help
  and logs modals): the copy is the rendered text, as shown: list bullets and
  numbers, quote bars, table rules and columns, code text, links as
  `text (url)`. Rows our own wrap split are joined back into one line (code
  rows without adding a space). Dragging past the pane's top or bottom
  scrolls it a row per drag event, and the wheel scrolls during a selection
  and keeps it, so a long document copies whole. Config and inspect lines
  wider than the pane are cut at its edge; a selection whose end reaches the
  last visible column copies such a line whole.
- **Other panes** (tables, detail, cards): the cells on screen, trailing
  spaces trimmed per row.
- **Integrated terminal**: a child that tracks the mouse gets the drag
  instead (and hover, if it asked for any motion), and its own OSC 52
  copies are relayed to the outer clipboard (status `copied from <tab
  title>`; the *terminal clipboard* setting turns it off); otherwise a local
  selection over the screen and its scrollback. Details and limits in
  docs/tui-terminal.md, *Mouse*.
- **The terminal's own selection.** `shift`-drag (`option`-drag on macOS)
  bypasses mouse capture in most emulators and gives their native selection:
  whole screen rows, borders and the next pane included.

### Clipboard: OSC 52

The copy is `ESC ] 52 ; c ; <base64> BEL` written to the outer terminal, so
it needs no host clipboard and works over SSH and from inside a container.
At most 1 MiB is sent (some terminals cap the payload lower); a cut copy
says `(cut at 1 MiB)`. OSC 52 has no reply: a terminal that ignores it
leaves the clipboard unchanged, with the status line still saying `copied`.

Supported by VS Code's terminal (1.93 and later), kitty, WezTerm,
Alacritty, foot, iTerm2, Windows Terminal and Ghostty; some ask before
letting a program write the clipboard, or have it as a setting.
GNOME Terminal and other VTE terminals: support depends on the VTE
version; check yours.

Under tmux (`$TMUX` set) the sequence goes through tmux's DCS passthrough
(`ESC P tmux ; … ESC \`), which hands it unchanged to the terminal tmux
runs in. tmux 3.3 and later drop passthrough unless enabled:
`set -g allow-passthrough on`. `set-clipboard` plays no part in this path,
and the terminal outside tmux must support OSC 52 itself.

## Settings

Per-user dashboard toggles (`src/tui/settings.rs`), saved at
`<data dir>/devsandbox/dashboard.toml` (`$XDG_DATA_HOME`, else
`~/.local/share`; the same base dir as `state.toml`):

```toml
copy_on_select = true
terminal_clipboard = true
```

| key | default | on means |
|---|---|---|
| `copy_on_select` | on | releasing a mouse selection copies it (else the copy keys do; *Mouse selection*) |
| `terminal_clipboard` | on | apps in the integrated terminal may set the clipboard through OSC 52, relayed to the outer terminal (docs/tui-terminal.md, *Mouse*) |

Every field defaults on its own, so a partial or older file loads and
unknown keys are ignored. A missing file is the defaults; an invalid one too,
with a `settings not loaded, using defaults: …` status line. The file is
loaded once at startup and written by the event loop after each toggle
(`App` only marks it for saving, staying I/O-free); a failed write says
`settings not saved: …`.

They are shown and changed in the `?` modal, now **Settings & help**: the
settings as `[x]` rows on top, the key reference below (selectable like any
document).

| key | does |
|---|---|
| `tab` / `shift-tab` | move the setting cursor |
| `space`, `enter`, click on a row | toggle that setting (and save) |
| `↑`/`k` `↓`/`j`, `pgup`/`pgdn`, `g`/`G` | scroll the help |
| `esc`, `q`, `?` | close |

A new setting is a field on `Settings` (with its default) plus one entry in
the `SETTINGS` table: its key, label and one-line help, a getter and a
setter. The modal's rows, toggling and hit-testing all derive from that table.

## Step ordering / commits

Each step = one review + one commit by the orchestrator. Steps 3 and 4 may share
data-collection plumbing; build it in step 3.
