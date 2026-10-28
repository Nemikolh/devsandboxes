# Integrated terminal

The dashboard's terminal panel: shells in instances and service
containers, each on a host PTY rendered through a vt100 parser, with key,
mouse and clipboard plumbing. Read this before changing what a tab runs,
how keys reach it, or the kitty emulation.

Code: `src/tui/term.rs` (`TermSession`, `TermTabs`, `TermParser`,
`spawn_reader`, `process_output`, `encode_key`, `encode_mouse`,
`encode_wheel`), `src/tui/kitty.rs` (`KittyState`, `encode_key`,
`DISAMBIGUATE`), `src/tui/app/terminal.rs` (`ExecCli`, `on_key_terminal`,
`terminal_mouse`, `open_terminal`, `term_argv`, `exec_argv`, `Redial`,
`redial_terminals`), `src/tui/remote.rs` (`exec_argv`,
`session_exec_argv`, `attach_argv`, `service_argv`), `src/tui/ui.rs`
(`open_bottom_split`, `terminal_panel_rect`, `term_pane_size`,
`terminal_tab_hit`), `src/tui/mod.rs` (`setup`/`enter`/`restore`,
`TERM_POLL_INTERVAL`), `src/commands/exec.rs` (`SHELL_FALLBACK_CMD`).

## Sessions

`TermSession::spawn` opens a `portable_pty` PTY, runs the tab's argv on it
with `TERM=xterm-256color` (reaches only the host-side child; the runtime's
`exec` doesn't forward it), and starts a reader thread that feeds output
into a shared `vt100::Parser<KittyState>` (5000 lines of scrollback) and
writes back the replies the callbacks queued. EOF sets `exited`: the tab
shows `(exited)`, keeps its screen, swallows keys, and `x` closes it.
Dropping a session kills and waits the child, so quitting the dashboard
leaks no `exec` client. An instance tab's shell lives on in its helper
session (see *What a tab runs*), there for `devsandbox exec --attach`.

`TermTabs` is the pure tab strip (open, `find`, next/prev, close; the
active index). One strip serves every dashboard tab. `t` focuses a live
tab for the same container (`find` skips exited ones), `T` always opens a
new one. The spawn happens inline in `App::open_terminal`, an accepted
exception to `App`'s I/O-free rule.

The event loop sizes every PTY to the panel before each draw
(`term_pane_size`, `resize_all`; a no-op when unchanged) and polls every
30 ms instead of 250 ms while any terminal exists, since child output
arrives on reader threads and only shows on the next draw.

## What a tab runs

- An instance (`t` on a running instance or one of its process rows, or a
  thread's child from the Inbox): `devsandbox -C <root> exec -i -t --
  <instance>` (`ExecCli::argv`), the executable re-resolved per spawn
  (`exe::devsandbox`) so tabs opened after an upgrade run the new binary.
  The CLI owns the runtime argv, the in-flight guard and the ssh-agent
  relay; with no command it runs `SHELL_FALLBACK_CMD` (zsh, then bash, then
  `sh`, as login shells), in a new helper session when the instance has
  the helper (`devsbd session new`, `commands::exec::session_for`; see
  [../helper/in-container.md](../helper/in-container.md)). Its errors
  print in the tab.
- A service (Services tab, first running container): `<runtime> exec -i
  -t <container>` + `SHELL_FALLBACK_CMD`, the binary named by the
  snapshot's `runtime_name`. The one place the dashboard runs the runtime
  itself.
- A machine's row (`box/…` key): the same over `ssh -t` on that machine
  (`remote::session_exec_argv` against the instance's root there,
  `remote::service_argv`), see [../daemon/machines.md](../daemon/machines.md).
  An instance's adds `--reserve-255 --session=<id>`, the id minted by
  the tab (`exec::mint_session_id`), so it can reconnect (below).
- An offered fleet worker's instance row: this host's `devsandbox exec
  -i -t --reserve-255 --session=<id> -- <machine>/<instance>` (no ssh to
  it: `remote::on_machine`, the executable `ExecCli::exe`), whose tty
  `exec` dials the worker over its fleet link
  ([../daemon/fleet.md](../daemon/fleet.md) *PTY*) and reconnects as the
  CLI does; the tab re-dials it the same way. A service row's tab runs
  this host's `devsandbox exec --service=<name> -- <machine>/<instance>`
  (`remote::service_argv`), the instance the one whose container it is
  (`remote::service_instance`: an isolated service's `used_by` entry at
  the container's index, a global one's first user); the worker derives
  the container itself. It isn't a session: no `Redial`, it just ends.

## Remote tabs reconnect

A remote instance tab carries a `Redial` (`TermSession::redial`; local
and service tabs have none and just end). The event loop calls
`App::redial_terminals` each pass: once the tab's dial (ssh, or the local
CLI for an offered worker) has ended
(`TermSession::exit_code`, the PTY's EOF then a reaped child), `Redial`
applies the CLI's rule (`serve::machines::cli::Reconnect`: a 255 after a
dial that ran 5 s is a lost connection, `--reserve-255` keeping a
program's own 255 out of it; any other code ends). A lost tab waits out
the backoff (1, 2, 4, 8, then 10 s), labelled `(reconnecting…)` in the
tab strip (`(reconnecting… N)` from its second try), then `TermSession::respawn` runs
`remote::attach_argv` (`exec --reserve-255 --attach=<id>`) on a fresh PTY
feeding the same parser: same tab, title, screen and scrollback; the
program redraws over them. While it waits the tab is still `find`'s live
tab for its container, swallows keys, and `x` closes it, which ends the
loop. A tab whose session is gone, whose first dial never connected, or
whose re-dial can't spawn (the reason written into its screen) shows
`(exited)`.

`:exec <instance> <cmd…>` is different: it suspends the dashboard and runs
the same `devsandbox exec` with inherited stdio
([command-line.md](command-line.md)).

## Focus and keys

`App::focus` is `Dashboard` or `Terminal`. Key order in
`App::dispatch_key`: selection `esc`/copy keys, the prompt, a modal, a
focused terminal, then dashboard bindings. Dashboard focus: `t`/`T` open,
`[`/`]` switch tabs, `x` closes, `ctrl-]` or `F12` focuses. Terminal
focus: `ctrl-]`/`F12` leave, everything else is encoded and written to the
PTY (`on_key_terminal`): `kitty::encode_key` when the app on that screen
pushed kitty flags, else `term::encode_key` honoring DECCKM (application
cursor). `ctrl-]` is the telnet escape readline leaves unbound; `F12`
covers vim's jump-to-tag.

## Kitty keyboard protocol

vt100 ignores the protocol, so an inner app asking for it would fall back
to legacy encoding, where ctrl+m and Enter are both `\r`. `KittyState` is
the parser's `Callbacks`: `unhandled_csi` tracks one flag stack per screen
(capped at 16), answers `CSI ? u` queries and XTVERSION, and the reader
writes the replies to the PTY. Only `DISAMBIGUATE` is implemented; the
other flags need input the dashboard never receives. Emulation is on only
when the outer terminal speaks the protocol, probed once in `setup`
(`supports_keyboard_enhancement`); the dashboard then pushes
`DISAMBIGUATE_ESCAPE_CODES` on the alternate screen and pops it exactly
once in `restore` (also run from the panic hook and around `:exec`).

## Mouse

`terminal_mouse` (no modal open): a left click in the panel focuses it,
on the title row activates the clicked tab; a click outside unfocuses.
Buttons on the body go to a child that enabled mouse tracking, in its
mode and encoding, with drags and the release clamped to the body;
otherwise the body is a local selection region
([selection.md](selection.md)). Hover goes only to an `AnyMotion` child,
once per cell. The wheel follows xterm: a tracking child gets wheel
reports, an alternate-screen app without tracking gets 3 arrow keys per
notch, otherwise the vt100 scrollback scrolls.

## Clipboard relay

`KittyState::copy_to_clipboard` also queues the child's OSC 52 copies
(newest wins; reads and payloads over `clipboard::MAX_COPY_BASE64` are
dropped). The event loop relays the last one per frame to the outer
terminal when the `terminal_clipboard` setting is on; queues drain either
way ([selection.md](selection.md#osc-52)).

## Layout

With terminals open the content area's bottom half splits into the Detail
pane (30 %) and the terminal panel (`open_bottom_split`); the tab strip is
the panel's block title, and the focused panel gets the accent border and
`▶ ` mark. `terminal_panel_rect` is the one layout definition shared by
drawing, PTY sizing and mouse hit-tests.

Limitations: [status.md](../../status.md#dashboard)
