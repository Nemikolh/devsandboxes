# 0033: The integrated terminal is an in-process PTY running the CLI, rendered with vt100

Status: accepted

## Context

The dashboard opens shells in instances and services without leaving the
TUI. Options: suspend the TUI and hand over the terminal, require tmux, or
emulate a terminal inside the dashboard. A hand-built `docker exec -it` per
tab would also duplicate what `devsandbox exec` already does.

## Decision

- Terminal tabs are PTY sessions inside the dashboard (`tui::term`,
  `TermSession`/`TermTabs`), parsed and rendered with the `vt100` crate.
- An instance tab runs this binary's `exec -i -t -- <instance>`
  (`app/terminal.rs`, `ExecCli`), so the CLI builds the argv (workspace,
  `remoteUser`, `remoteEnv`), guards in-flight instances and holds the
  ssh-agent bridge; remote rows prefix `ssh -t`. Service tabs run
  `<runtime> exec -it`. The shell is `SHELL_FALLBACK_CMD` (zsh, bash, `sh`,
  as login shells).
- Because vt100 ignores the kitty keyboard protocol, `tui::kitty` emulates
  its "disambiguate" level (and XTVERSION) for inner apps, only when the
  outer terminal speaks it too.
- Leave/focus keys are `ctrl-]` and `F12`; no `F<N>` tab jumps.

## Consequences

- Shells, TUIs and agents run side by side with the dashboard, no tmux.
- Terminal emulation gaps are ours to fix (vt100 coverage, kitty levels
  beyond disambiguate need input the dashboard doesn't receive).

Source: docs/tui-terminal.md, "Lifecycle", "Decisions the user may want to adjust"; docs/dashboard-client.md, "Decisions" (Terminal tabs).
