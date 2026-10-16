# Dashboard lead: fragment

## Docs written

- docs/architecture/dashboard/overview.md: entry (TTY, unix-only), pure client of serve and what's still local, threads + event loop, `App` I/O-free (and its exceptions), machines in the dashboard (`remote.rs`), settings (`dashboard.toml`, `SETTINGS`)
- docs/architecture/dashboard/tabs-and-actions.md: the four tabs, tree `Node`s, process layer, modals + `Fetch`, one-key actions + pending queue, verbs as detached/held requests
- docs/architecture/dashboard/command-line.md: `:` prompt, `SPECS` driving parsing + completion, name resolution, history, how to add a command/flag
- docs/architecture/dashboard/terminal.md: PTY sessions, what a tab runs, focus/keys, kitty keyboard emulation, mouse, OSC 52 relay, layout
- docs/architecture/dashboard/selection.md: regions/`Source`, routing, copy keys, OSC 52 (+ tmux), the two settings
- docs/architecture/dashboard/inbox-tab.md: Inbox UI only: data flow (local reads, ops via API or local), views, read semantics, pane, markdown, replies/forms, thread actions
- docs/architecture/dashboard/snapshot-and-json.md: `Snapshot` rows, `Depth`/`RuntimeView`, who collects, the dashboard's copy (`InstancesList`), `--json` verbs + `Envelope`, `run --json` + `JsonStdout`

## Known limitations (verified true today)

### dashboard
- The dashboard runs no autostart pass and the daemon's startup pass only walks roots recorded in `state.toml`, so opening the dashboard on a root with no recorded instances autostarts nothing → architecture/dashboard/overview.md; evidence: tui/mod.rs `dashboard` (comment "No autostart pass of our own"), serve/host.rs roots from `state.instances[*].config_dir`
- A thread host action waiting on a detached start (`after_start`) never fires if the start fails after it was registered; only a failed request or a newer action drops it → architecture/dashboard/inbox-tab.md; evidence: tui/app/thread_actions.rs `start_answered`, `run_after_start` (doc names F14)
- VS Code (`o`, `:code`, thread `vscode`) and port forwards are refused for a machine's rows → architecture/dashboard/overview.md#machines; evidence: tui/app/actions.rs `not_yet_code`, `not_yet_port`; tui/mod.rs `PromptAction::Code` arm
- The Inbox tab shows only this host's store; machines' Inbox threads aren't federated → architecture/dashboard/inbox-tab.md; evidence: tui/mod.rs loads `inbox::store::path()` only
- `d` on an instance ignores modifiers: ctrl-d removes it without a prompt → architecture/dashboard/tabs-and-actions.md; evidence: tui/app/mod.rs `dispatch_key` (`KeyCode::Char('d') if self.tab == Tab::Instances && !on_proc`, no ctrl guard)
- Service terminal tabs run `<runtime> exec -i -t` directly, not `devsandbox exec` (two terminal paths) → architecture/dashboard/terminal.md; evidence: tui/app/terminal.rs `term_argv`
- `TERM=xterm-256color` reaches only the host-side child; the container shell gets the runtime's default (`xterm` on docker) unless the image/`remoteEnv` sets one → architecture/dashboard/terminal.md; evidence: tui/term.rs `TermSession::spawn`
- Kitty keyboard emulation implements only "disambiguate"; apps asking for release/repeat/alternate keys/text don't get them → architecture/dashboard/terminal.md; evidence: tui/kitty.rs `DISAMBIGUATE` doc
- OSC 52 is the only clipboard path: a terminal without OSC 52 can't copy; copies are cut at 1 MiB, and terminal-app copies over the base64 bound are dropped silently → architecture/dashboard/selection.md; evidence: tui/clipboard.rs `MAX_COPY`, tui/kitty.rs `copy_to_clipboard`

## Status entries

### Parked
- **dashboard**: `--all` for `stop`/`start`/`rebuild` in the `:` prompt; prompt commands for `logs`/`inspect` (not in `spec::SPECS`). Source: docs/tui-prompt-spec.md *Parked*
- **dashboard**: selection extras: double-click word / triple-click line, keyboard (visual-mode) selection, host clipboard fallbacks (`arboard`/`wl-copy`/`xclip`), copying markdown source from rendered mode (raw `m` exists). None in code. Source: docs/tui-selection.md *Parked*
- **dashboard**: one terminal path for instances and services (service tabs via `devsandbox exec`-like CLI). Source: docs/dashboard-client.md known limitations ("for later, user's note")

## Stale claims in old docs

- dashboard-client.md: instance terminal exe is `current_exe()` captured at dashboard start; an upgrade breaks new tabs → re-resolved per spawn (`ExecCli.exe: None` → `crate::exe::devsandbox()`), evidence tui/app/terminal.rs `ExecCli::argv`, tui/mod.rs `run` comment. Both the "as built" note and the known-limitation line are obsolete.
- dashboard-client.md: "The Inbox is still read from the local store until federation (rollout step 5)" → still true (keep as limitation), but "rollout step" framing is plan history.
- tui-terminal.md: `TermSession` has shared `exited`/`dirty` flags, reader sets `dirty` → no `dirty` flag; fixed redraw cadence (30 ms) instead; evidence tui/term.rs `spawn_reader` doc.
- tui-terminal.md: `TermTabs` holds a focused flag → focus is `App::focus`; evidence tui/term.rs `TermTabs`, tui/app/mod.rs `Focus`.
- tui-terminal.md: code landmarks reference `src/tui/app.rs` and line numbers → `app/` is a directory of modules.
- tui-prompt-spec.md: `COMMANDS` const in prompt.rs, `candidates_for` in `app.rs` → `prompt::commands()` derived from `SPECS`; `candidates_for` in tui/app/command_line.rs.
- tui.md Step 6: `code` builds the vscode-remote URI with `hex_encode` in `src/tui/mod.rs` → `commands::vscode::launch` (tui/mod.rs `launch_code`).
- tui.md Step 7: "error toast (bottom-right, auto-dismiss)" → a status line in the help bar (`App::status`); no toast in code.
- tui.md Phase 3: three tabs, `1/2/3` → four tabs incl. Inbox, `1`-`4` (tui/app/mod.rs `Tab::ALL`).
- tui.md Step 8: `Node::EmptyMarker(sandbox)` → `Node::Empty(usize)`; Step 9: `r` on an instance prefills `run <sandbox>` → prefills `rename <instance>` (tui/app/command_line.rs `open_rename_or_run_prompt`).
- processes-stop-colors.md Step 10: prompt `stop` runs via the suspended-terminal path; `s` runs `docker stop` on a dashboard thread → both are `stop` verb requests to the daemon (tui/mod.rs `spawn_verb`).
- processes-stop-colors.md Step 11: processes fetched with `docker top` by the dashboard → `instances.procs` via tui/poll.rs; daemon does `exec ps` with a `top` fallback (`signalable` false then), plus `devsbd run` rows (tui/procs.rs `proc_state`, `run_rows`).
- processes-stop-colors.md Step 12: inspect fetched at modal open via `docker::output_quiet` → `containers.inspect` through the daemon worker (tui/daemon.rs `fetch_call`).
- json-output.md Step 1/3 line refs (`data.rs:106`, `app.rs:17`) and "re-exports `collect`" → tui/data.rs re-exports only the row types and `Snapshot`; `collect` isn't re-exported.
- json-output.md Step 4: README gets a "JSON output" section → README has one table row only (`status --json` + list of verbs). Envelope/schema promise is now in architecture/dashboard/snapshot-and-json.md.
- json-output.md verdict: shipped (steps 1-4 and every "Added after"), no unbuilt parts besides the explicit non-goal (raw cpu/mem numbers, "schema-v2 idea").
- tui-selection.md *Problem*: describes the pre-selection state (no selection, no forwarding to mouse-tracking children) → all built; Steps 1-8 match code.
- Code-comment rot noticed (not fixed, outside scope): tui/app/mod.rs `pending_done` doc says "`d`/`u` on an instance row" (only Inbox thread actions queue it; `u` does nothing on an instance); `PortRow`/`pending_port`/`pending_unport`/`set_ports` docs say "forwarder worker (step 10)" (it's the daemon worker); `App::on_key` doc says "the modal open path reads config/fs" (it queues `Fetch`es); `pending_signal` doc says "a `kill` the event loop should spawn" (it's `instances.signal`); snapshot.rs `Depth` doc says "The TUI walks all three on startup" (the daemon does).

## Decision candidates

- The dashboard is a pure client of `devsandbox serve` and never runs a verb in its own process; no daemon = the verb fails. Why: a build must outlive the dashboard and log itself; one poll serves every dashboard. Source: docs/dashboard-client.md *Decisions*. Still true: tui/mod.rs `verb_status` (`Err` → "needs devsandbox serve", doc "no fallback"), `dashboard` (no autostart).
- `App` is an I/O-free state machine; the event loop does I/O by draining `pending_*`/`take_*` requests. Why: key handling unit-testable without a terminal or daemon. Source: docs/tui.md, AGENTS.md. Still true: tui/app/mod.rs module doc + `pending_*` fields (exceptions: PTY spawn, prompt history).
- Prompt grammar is one local declarative table (`SPECS`) read by parser and completer, not derived from clap. Why: parse/complete drift (`rebuild --force` parsed, never completed); the prompt grammar is a small, tab-dependent subset. Source: docs/tui-prompt-spec.md *Problem*/*Design*. Still true: tui/spec.rs, tui/prompt.rs `parse_line`, tui/app/command_line.rs `candidates_for`.
- Terminal focus leaves with `ctrl-]` or `F12`; no `F<N>` tab jump; scrollback by wheel only. Why: everything else must reach the shell; `ctrl-]` unbound in readline, F12 covers vim; F-keys taken by emulators/macOS. Source: docs/tui-terminal.md *Focus model* + *Decisions*. Still true: tui/app/terminal.rs `on_key_terminal`.
- Emulate the kitty keyboard protocol (disambiguate only), only when the outer terminal speaks it. Why: vt100 ignores it, so inner apps lose ctrl+m vs Enter; advertising it without outer support would be a lie. Source: docs/tui-terminal.md *Keyboard: kitty protocol*. Still true: tui/kitty.rs module doc, tui/mod.rs `setup`/`enter`.
- Instance terminal tabs and `:exec` run `devsandbox exec`, not a runtime argv. Why: the CLI owns argv, in-flight guard, ssh-agent relay; the dashboard reads no state; remote is a prefix. Source: docs/dashboard-client.md step 8. Still true: tui/app/terminal.rs `ExecCli`.
- The dashboard selects text itself and copies only via OSC 52; copy on select defaults on; terminal-app OSC 52 relay defaults on but is switchable. Why: mouse capture disables the emulator's selection; OSC 52 works over ssh/tmux; the release is the one copy gesture that always reaches the app; relay lets a container write the host clipboard. Source: docs/tui-selection.md *Design*, steps 5-6. Still true: tui/select.rs, tui/clipboard.rs, tui/settings.rs `Settings` docs.
- Dashboard verbs are always sent with `detach` (quitting cancels none). Why: a quit used to SIGINT a held `stop`/`rm`. Source: docs/dashboard-client.md known limitations (fixed via before-machines step 2). Still true: tui/mod.rs `verb_call`, `verb_held` doc.
- One `Snapshot` struct for the dashboard and `status --json`, `{schema, data}` envelope, additive fields keep `schema: 1`. Why: parity by construction. Source: docs/json-output.md goal + "Added after" sections. Still true: snapshot.rs (`Serialize + Deserialize`), commands/status.rs `Envelope`, serve/api.rs `InstancesList`.
- `run --json` points the process's stdout at stderr up front instead of threading a flag through spawn sites. Why: the next spawn site added would be missed. Source: docs/json-output.md *Added after: run --json*. Still true: json_stdout.rs module doc.
- Inbox reads stay local (store file is truth); writes go through the API when connected, else the same `inbox::ops` locally. Why: dashboards and CLI share one file; same ops code either way. Source: docs/dashboard-client.md. Still true: tui/daemon.rs module doc, tui/mod.rs inbox block.
- The dashboard keys every row by `machine/name` (`remote::qualify`); names can't hold `/`. Why: one key for tree, caches, in-flight sets, tabs; local keys unchanged. Source: docs/machines.md step 6. Still true: tui/remote.rs module doc.

## Uncovered

- Exhaustive keymap: lives in the `?` modal (`view::help_body`) and help bar (`ui.rs`); a doc copy would rot.
- Colours/styling and exact layout (processes-stop-colors.md Step 13, tui.md layout bits, `ui.rs` palette): presentation, not architecture.
- Inbox model/store/ops/wire and `devsandbox inbox`: inbox lead's area; inbox-tab.md points at `src/inbox/` code instead of linking `architecture/inbox/` (not written yet; orchestrator may add links in step 3/4).
- Forwards engine, verbs runner, host poll, machines link: daemon docs, linked.
- npm `execArgv()` and `index.d.ts` (json-output.md *Added after*): daemon/client-server.md covers npm.
- Plan history (steps, F-numbers, "as built" deltas) of every source doc.
- `snapshot-and-json.md` sits under dashboard/ because the Snapshot is the dashboard's model; the CLI `--json` contract might deserve a `reference/` home in step 3.
