# The `:` command line

The dashboard's one-line prompt: how a typed line becomes an action, how
tab completion knows what to offer, and how to add a command or flag.
Read this before touching the prompt grammar.

Code: `src/tui/spec.rs` (`SPECS`, `CommandSpec`, `FlagSpec`, `ArgValue`,
`find`, `parse_args`), `src/tui/prompt.rs` (`Prompt`, `PromptAction`,
`parse_line`, `commands`, `history_path`, `load_history`,
`append_history`), `src/tui/app/command_line.rs` (`on_key_prompt`,
`candidates_for`, `resolve_action`, the prefilled `open_*_prompt`s),
`src/tui/mod.rs` (the event loop executing `PromptAction`s).

## One table: `SPECS`

`spec::SPECS` declares every command: `name`, `aliases` (parse but don't
complete, e.g. `recreate` → `rebuild`), `usage` (the error when required
arguments are missing), `flags` (`FlagSpec`: boolean, or taking a value of
some `ArgValue`), `positionals` (required, in order) and `trailing`
(the rest of the line is verbatim argv, `exec`'s command).

Both parsing and completion read it, so a flag can't parse without
completing or the other way round. Commands today: `run`, `exec`, `code`,
`rm`, `rename`, `stop`, `start`, `rebuild` (alias `recreate`), `port`.

`ArgValue` says what a position holds and what completes there:
`Instance`, `InstanceOrService` (services on the Services tab),
`Service`, `Sandbox`, `Branch`, `Free` (no candidates).

## Parsing

`parse_line` splits on whitespace, looks the command up with `spec::find`
and runs the generic `spec::parse_args`: flags are position-independent,
only `--`-prefixed tokens count as flags (so `exec box ls -la` keeps
`-la`), and once a trailing command's positionals are filled the rest is
captured verbatim. `parse_line` then only maps `ParsedArgs` onto a
`PromptAction` variant. `port` validates its `[host:]port` with
`commands::port::validate_port_spec`, so a bad spec is an inline error.

On `enter`, `on_key_prompt` resolves typed names against the snapshot
(`resolve_action` → `remote::resolve`: a bare name unique across
machines, `box/web`, `@/web`; no exact match passes the name on for the
daemon to resolve). A parse or resolve error stays inline and keeps the
prompt open. Two rewrites happen here, where the tab is known, keeping the
parser pure: `rebuild` on the Services tab becomes
`PromptAction::ServiceRebuild` (`--force` dropped), and `port` becomes a
`pending_port` request (switching to the Ports tab) rather than a
`pending_action`.

`port`'s names resolve together (`resolve_port`): the instance names the
machine and a bare `--service` is that machine's (`port box/web --service
db`), while a qualified `--service box/db` puts a bare instance on its
machine (`port web --service box/db` is `box/web`); a service on another
machine than the instance is an inline error. `port_request` turns the
keys into the `PortRequest`: bare names, plus for a machine its `machine`
and config root there (`remote::instance_route`, or `service_route` for a
service-only one); a machine with no known root yet is a status line.

## Completion

`tab` calls `Prompt::complete` with a closure over `candidates_for`:
command names for the first token; after it, the command's spec decides:
the preceding flag's value kind, unused flags on a `-` stem, the next
unconsumed positional, then the unused flags again. `exec`'s trailing
argv gets nothing. The prompt filters by the token's stem (a stem without
`/` also matches after a candidate's `<machine>/` or `@/`), sorts,
applies the first match and cycles on repeated `tab`; any other edit ends
the cycle.

Candidates come from the snapshot only, never config: instance, service
and sandbox names (`remote::candidates`, remote ones qualified and a
local one `@/<name>` when a machine has the same name, `remote::typed`:
what resolves back without ambiguity), and for
`--branch` the sandbox row's `worktree_branch`, else
`DEFAULT_WORKTREE_BRANCH`, with `${…}` left unsubstituted.

## History

`<data dir>/devsandbox/prompt_history`, next to `state.toml`: loaded on
every prompt open, appended on submit, consecutive duplicates skipped,
capped at 200 lines (`HISTORY_CAP`), best-effort. `↑`/`↓` walk it. This
file I/O runs inside `App`, an exception to its I/O-free rule.

## What each action becomes

The event loop takes `App::pending_action`
([overview.md](overview.md), [tabs-and-actions.md](tabs-and-actions.md)):

- `run`, `rebuild`, `start`, `stop`, `rm`, `ServiceRebuild`: a verb
  request to the daemon (`spawn_verb`), see
  [../daemon/verbs.md](../daemon/verbs.md).
- `exec`: suspends the dashboard and runs `devsandbox -C <root> exec -i
  -t -- <instance> <cmd…>` (over ssh for a machine's row) with inherited
  stdio, then waits for a key ([terminal.md](terminal.md) for the
  in-dashboard terminal instead).
- `code`: VS Code launch on a background thread (`spawn_code`); refused
  for a machine's instance.
- `rename`: `instances.rename` on the daemon worker's connection.
- `port`: `forwards.add` via the daemon worker; a machine's instance or
  service is the same call with `machine` and its root there as `dir`.

One-key shortcuts reuse the prompt: `r` prefills `rename <instance> ` (or
`run <sandbox> ` off an instance; both names as `remote::typed` writes
them, `run @/s ` when a machine has an `s` too), `p` prefills `port <instance> ` or
`port <instance> --service <svc> ` (a machine's row by its key: `port
box/web `, `port box/web --service db `, or for a service nobody uses yet
`port  --service box/db `).

## Adding a command or flag

- A flag: one `FlagSpec` in the command's `SPECS` entry; read it in
  `parse_line` with `args.flag(..)` / `args.value(..)`.
- A command: a `CommandSpec`, a `PromptAction` variant, its arm in
  `parse_line`, its arm in `resolve_action`, and what the event loop does
  with it in `tui::run`.
- A new value kind: an `ArgValue` variant and its arm in
  `candidates_for`.

Limitations: [status.md](../../status.md#dashboard)
