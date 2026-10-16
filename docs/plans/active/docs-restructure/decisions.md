# Decisions lead (step 2b)

Child spawns all failed (3/3 slots held by other leads), so the clusters
were done by the lead directly. Candidates came from the old docs'
Decisions/Principles/Why sections plus the daemon, helper and instances
leads' "Decision candidates" (their evidence spot-checked against the code,
not re-derived symbol by symbol). Inbox, automations and dashboard
candidates were verified from scratch.

## ADRs written

- 0001 Files are the truth; the daemon is replaceable
- 0002 Config is the security boundary; mount sources come only from config
- 0003 Host git never runs repo-controlled code
- 0004 Two instances never share a working tree (worktrees, start point, locking)
- 0005 `run` always creates; `start` restarts
- 0006 A persistent `instance_id` is separate from the display name
- 0007 Record the instance first; failures stay `failed`, only a cancel rolls back
- 0008 `extends` deep-merges, and arrays concatenate (+ unknown keys are errors)
- 0009 Drift is detected by hash labels on containers, with one lenient rule
- 0010 Services are isolated by default; `global` is opt-in; refcounts derived
- 0011 `service rebuild` recreates the service and rewires sandboxes in place
- 0012 Runtime differences are `Backend` capabilities, not runtime-name checks
- 0013 Devcontainer features are installed natively, entrypoints chained at build time
- 0014 One host daemon per user, on a unix socket, unix-only
- 0015 The newest binary wins: version handoff between daemons (+ restart on-failure)
- 0016 Verbs run as `devsandbox --in-process` subprocesses, one request each
- 0017 Hang-up cancels a verb; `detach` opts out
- 0018 One shared container poll; the daemon pushes snapshots
- 0019 Port forwards tunnel through the helper and are owned by the daemon
- 0020 Remote machines are reached over plain ssh, and must match the protocol
- 0021 The in-container helper is embedded in the CLI and installed through `exec` stdin
- 0022 The helper has no dependencies; shared codecs are line-based and shared via `#[path]`
- 0023 The helper frame protocol grows additively, not by version bumps
- 0024 One in-container helper daemon; a different build takes over (+ daemon owns the bridge)
- 0025 The ssh agent is relayed through the helper, not bind-mounted
- 0026 The Inbox is one shared, locked file store; records are applied, then acked
- 0027 The Inbox is a generic channel; owners hold the meaning and re-assert idempotently
- 0028 Inbox threads belong to their owner instance; owning threads is opt-in
- 0029 `autostart = true` runs once per boot, keyed on the host boot id
- 0030 Dispatcher control is a host API, authorized against the current config on every request
- 0031 The dashboard is a client of the daemon, with no in-process fallback
- 0032 Dashboard logic is an I/O-free state machine; the `:` grammar is one table
- 0033 The integrated terminal is an in-process PTY running the CLI, rendered with vt100
- 0034 The dashboard owns mouse selection and copies through OSC 52
- 0035 `--json` output is one versioned document on a clean stdout

## Dropped

- devsandboxes.md "No daemon" goal: superseded by `devsandbox serve` (`src/serve/`); 0001 is what survived of it.
- devsandboxes.md / run-command.md §4 compose support (`dockerComposeFile`, compose overrides, `rm --services`): compose removed; `SandboxProperties` has no compose fields, services are `[services.*]` + `gc`.
- run-command.md §1 "`run` reuses an existing stopped container": reversed; `run::claim_name` refuses existing names (0005).
- run-command.md §2 "every instance joins the project network": per-instance network for isolated services, global network only when a global service is used (`services::ensure_services`, 0010).
- run-command.md "nameConfig rewritten on every run": written only when extensions/`remoteUser` are set and on every `vscode` launch (`run::materialize`, `vscode::launch`); detail, no ADR.
- rebuild.md "`ensure_services` recreates drifted service containers": it only warns; recreation is `service rebuild` (`services::ensure_service`, 0011).
- vscode-workspaces-and-persistent-state.md "extra folders are plain shared mounts": they get worktrees under `auto`/`always` (`folders::needs_worktree`, 0004).
- high-level-architecture.md "`forwardPorts` parses but warns": implemented by the daemon's forwards (`SandboxProperties::forward_ports`, 0019).
- inbox-threads.md "only `dispatcher` instances own threads": superseded by `inbox = true` (`dispatch::declares_inbox`, "thread put denied: … doesn't declare inbox = true" in `inbox::mod`, 0028).
- inbox-threads.md thread header with `message` / `reply`: replaced by the feed + `compose` (`inbox::Thread.compose`, `inbox::feed`); the redesign's principles are 0027.
- inbox-threads.md v2 `inbox.toml` per-dashboard store: now `inbox.json` v3, `inbox::store` (imports v2 once); the "one shared store" decision itself holds (0026).
- inbox-threads.md / tui.md "the dashboard spawns bridges and reinstalls a stale helper": the daemon's bridges do (`serve::host`, `devsbd::ensure_recorded` before every bridge spawn; 0024, 0031).
- tui.md: dashboard polls the runtime / loads config itself: superseded by dashboard-client (0031); tui/ calls no runtime (`tui::daemon`, `tui::poll` over the API).
- verbs-as-requests.md "the TUI's `rm`/`stop` stay suspended in-process": now held verb requests with `detach` (`tui::spawn_verb`, 0017).
- automations.md "the first TUI load triggers autostart": only the daemon's startup pass (`serve::host` `autostart_all`) and CLI `run`/`rebuild`/`start` (`main.rs`) call `autostart::autostart` (0029).
- json-output.md non-goal "no library bindings; the CLI is the API": superseded by the daemon's JSON-lines API and the npm package's `connect()` over `devsandbox api --stdio` (`serve::relay`, 0014). The envelope decision holds (0035).
- helper-rpc.md, cli-proxy.md, windows-support.md design choices: not implemented; plans, not decisions (plans lead).

## Unsure

- before-machines.md "Colour": verbs get a pty so docker build shows progress, replayed through vt100 in the build-log modal. Appears true (`serve::verbs` `RunParams::tty`) but judged an implementation detail; no ADR. Promote if the orchestrator disagrees.
- tui-terminal.md minor UI choices (50% bottom height, wheel-only scrollback): not verified, too small for an ADR; `ctrl-]`/`F12` folded into 0033.
- 0013's "layout mirrors the official CLI" relies on the instances lead's evidence (`features::download_and_extract`, `run::image`).
