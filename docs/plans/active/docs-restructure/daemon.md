# Daemon lead: fragment

## Docs written

- docs/architecture/daemon/role.md: what the daemon owns, files-are-truth, socket/lock/log, lazy start, handoff, holders + idle, drain, `serve install|uninstall`
- docs/architecture/daemon/platform-support.md: unix-only gates and what each affected command does off unix
- docs/architecture/daemon/client-server.md: `Conn`, `connect` vs `connect_running`, `--in-process`, who uses the daemon (table), dashboard as client, `api --stdio` relay, npm + dts
- docs/architecture/daemon/verbs.md: verbs as subprocesses, request loop, detach/hang-up, cancel, spool + adoption, CLI side, fallback
- docs/architecture/daemon/host-poll.md: the one container poll, bridges/`bridges.ensure`, snapshots + watch, notifications/subscribe, autostart
- docs/architecture/daemon/forwards.md: `Forward` engine, routes, host ports, `Registry`, `forwards.toml`, configured `forwardPorts`, foreground `port`
- docs/architecture/daemon/machines.md: registry, ssh argv, link handshake, held links, forwarded requests, federation, CLI routing, `machine add`
- docs/reference/api.md: every method + notification (index table, envelope, errors, Inbox, instances, containers, sandboxes, bridges, forwards, verbs, machines, notifications, shutdown, holders, relay, npm)

## Known limitations (verified true today)

### daemon
- Daemon, dashboard, `api`, `machine`, `port`, `-d`, `--machine`, ssh-agent relay are unix-only; off unix verbs run in the CLI process → architecture/daemon/platform-support.md; evidence: src/main.rs `#[cfg(unix)]` gates, `detach_needs_daemon`, `exec::has_host_agent`
- Every live forward is an idle holder, so a running instance with `forwardPorts` or any saved ad-hoc forward keeps the daemon up forever → architecture/daemon/role.md; evidence: serve/daemon.rs `Shared::holders` (`Registry::active`)
- Each forward spawns its own bridge `exec` instead of riding the instance's daemon bridge → architecture/daemon/forwards.md; evidence: serve/forwards.rs module doc, devsbd/forward.rs supervisor
- Foreground `devsandbox port` forwards are invisible to the daemon, Ports tab and API (and vice versa) → architecture/daemon/forwards.md; evidence: main.rs `Command::Port` → `commands::port::port`, no client
- TCP only, no UDP forwarding → architecture/daemon/forwards.md; evidence: devsbd/forward.rs `TcpListener`
- Service injection route needs `/bin/sh`, root `exec`, writable `/run` in the service image → architecture/daemon/forwards.md; evidence: port.rs `Planned::Inject` (helper install via `devsbd::ensure`)
- An adopted verb is tracked by pid only: a reused pid holds the daemon until that process exits → architecture/daemon/verbs.md; evidence: verbs.rs `InFlight::adopt`/`check_adopted` (`state::pid_alive`)
- A CLI that loses its daemon mid-verb follows the build log from the start, re-showing streamed lines → architecture/daemon/verbs.md; evidence: verbs_client.rs `lost` → `logs::build_log(instance, true)`
- The daemon's startup autostart pass runs in-process in the daemon (`serve-autostart` thread), not as a verb subprocess → architecture/daemon/host-poll.md; evidence: serve/host.rs `Host::start` (`autostart_all`)
- npm client has no typed verb helpers (`api.call('run', …)` only) → reference/api.md; evidence: npm/devsandboxes/index.d.ts `Api`
- The daemon doesn't enforce `hello` (a connection may skip it) → reference/api.md; evidence: serve/daemon.rs `handle`

### machines
- A local daemon exit (handoff, idle, shutdown) cancels a held non-`detach` remote verb (every CLI verb): no spool to adopt it by → architecture/daemon/machines.md; evidence: serve/daemon.rs module doc, machines/forward.rs `verb`
- Daemon-side ssh is `BatchMode=yes` and one handshake per forwarded verb: passphrase-prompt keys fail → architecture/daemon/machines.md; evidence: machines/ssh.rs `api_argv`, forward.rs `verb` (fresh dial)
- A remote login shell that prints to stdout breaks the hello → architecture/daemon/machines.md; evidence: machines/ssh.rs `Duplex` (raw stdout), link.rs
- `machine add` has no `--command`; a remote devsandbox off `PATH` is set by hand in `machines.toml` → architecture/daemon/machines.md; evidence: main.rs `MachineCommand::Add` (target, name, configs)
- Remote rows are memory only: a fresh daemon shows a machine without rows until its link connects → architecture/daemon/machines.md; evidence: snapshots.rs `Remotes`
- `vscode` and `port` on a machine's instance are refused; dashboard `o`/`p` on a remote row say "isn't supported yet"; `inbox` not for `--machine` → architecture/daemon/machines.md; evidence: machines/cli.rs `route`, `machine_use`; tui/app/actions.rs
- Nameless commands without `--machine` are local only → architecture/daemon/machines.md; evidence: machines/cli.rs `plan` (`Plan::Here`)
- No remote Inbox threads (the local Inbox reads only this host's store) → architecture/daemon/machines.md; evidence: inbox store is local, no `inbox.*` in `forward::QUICK`
- No default machine → architecture/daemon/machines.md; evidence: main.rs routing needs `--machine` or a qualified name

## Status entries

Planned / not started:
- **machines**: remote port forwards (client-server rollout step 6); `port`/`p` refused on machines today.
- **machines**: remote VS Code (rollout step 7, needs a spike).
- **machines**: default machine (open question, tied to Windows plan).
- **machines**: nameless commands across every machine without `--machine` (docs/machines.md *Later*).
- **machines**: auto-update a machine's devsandbox to the newest linked (docs/machines.md *Later*, sketch only).
- **daemon**: Windows as a remote-only client (docs/windows-support.md, plans lead owns it); `endpoint.rs` is the named-pipe seam.
Parked / noted, not planned:
- **forwards**: share one bridge per container across forwards; narrow forwards-as-holders (only with a connection, or a grace); auto-detect listening ports via `/proc/net/tcp`; persist CLI forwards so the TUI lists them (docs/port-forwarding.md, docs/serve.md *Future work*).
- **verbs**: adopted-verb pid-reuse check (`getpgid` or start time) (docs/verbs-as-requests.md).
- **forwardPorts**: `portsAttributes`, `otherPortsAttributes`, `appPort` not implemented (warned as ignored; port-forwarding.md; not re-verified, instances lead may own config).

## Stale claims in old docs

- verbs-as-requests.md: "The dashboard's `rm`/`stop` still run in its own process, suspended" → they're held verb requests with `detach` (tui `spawn_verb`; verbs.rs `StopParams` doc)
- verbs-as-requests.md: Status/step text calls verbs "five" and "services * … come after" → `services.rebuild` is a verb (verbs.rs `is_verb`)
- client-server.md: Status "design" → rollout steps 1–5 shipped; only steps 6–7 (remote forwards, remote VS Code) remain
- serve.md *Future work*: "The other verbs moving into the daemon" → no plan/code for more verbs; `rename`/`done` are API methods, `gc`/`service ls` in-process
- high-level-architecture.md (Dashboard): "every action and terminal works on [machine rows]" → VS Code and port forwarding on remote rows answer "isn't supported yet" (tui/app/actions.rs)
- api.md *Hello*: "Send it first" → not enforced (daemon.rs `handle`); also dropped compat notes ("older daemon has no `found`", "ignores `wait`", "older than this method")
- port-forwarding.md *Flow control*: "Today `Mux::serve` writes each `Data` straight…" → flow control shipped (mux `MAX_HOST_STREAMS`, `Window`); present tense describes pre-step-3 code
- before-machines.md: no Status line; all 7 steps shipped (`exe::devsandbox` re-resolved per spawn, verbs `tty`, `instances.watch`, held verbs with `detach`)
- instance-phase.md: Status done; belongs to instances lead (phase, build log, cancel/rollback); daemon-relevant bits covered in verbs.md
- Code comments (not edited, src out of scope): serve/idle.rs `Holders` says "`inbox = true` joins in step 11" (it does: host.rs `holds_host`) and "`followers` stays 0 until step 18" (never set; follow streams count as `control`); serve/mod.rs lists verbs as "those five" (six with `services.rebuild`)

## Decision candidates

- Files are the truth, the daemon is replaceable; why: any command works without it, a crash/handoff loses nothing; source: high-level-architecture.md principles; still true because verbs fall back in-process (verbs_client `in_process`), Inbox ops run locally (`wire::decide_*`), forwards/machines persist in files.
- One daemon per user, unix socket only, access = file perms, no TCP listener; why: short socket path (macOS 104 B), docker.sock model; source: serve.md *Paths*; still true because endpoint.rs `socket_dir_from`, `Listener::bind` 0600 + 0700 dir.
- Newest binary wins: version = semver + own mtime, newer hello triggers handoff; machine links say 0.0.0 to never hand off a remote; source: serve.md *Version handoff*, machines.md; still true because proto.rs `Version::key`, link.rs.
- Verbs run as `devsandbox --in-process` subprocesses with the caller's whole env, output spooled to files; why: a panic/hang kills one request, handoff doesn't wait, no EPIPE on daemon exit; source: verbs-as-requests.md, client-server.md *Execution, internally*; still true because verbs.rs `argv`, `Spool`, `Runner`.
- Hang-up = cancel, `detach` opts out, a drain or slow reader is not a hang-up; source: verbs-as-requests.md Decisions; still true because verbs.rs `Left`, `Shared::draining`.
- All prompts happen client-side before the verb is sent (child has null stdin); source: verbs-as-requests.md; still true because verbs_client.rs `prepare` (`ask` → `confirm`).
- Service unit restart policy on-failure, not always; why: handoff exits 0 and must not respawn the stale binary; source: serve.md *Boot start*; still true because install.rs `render_systemd`/`render_launchd`.
- One shared container poll, dashboards watch and the daemon pushes snapshots; why: N dashboards cost one listing + one stats per 7 s; source: before-machines.md *Shared poll*; still true because host.rs `poll`, snapshots.rs.
- Notifications are coalesced flags written by a per-connection notifier; slow clients cut after 5 s; source: api.md *Notifications*; still true because daemon.rs `Pending`, `WRITE_TIMEOUT`.
- Port forwards tunnel through the helper's bridge/mux, not `-p` or a sidecar; source: port-forwarding.md *Why tunnel*; still true because devsbd/forward.rs + mux `Connect`.
- Machines: plain ssh (no own ControlMaster), login-shell env, one ssh per forwarded verb, links only while wanted; source: machines.md *Decisions*; still true because ssh.rs, service.rs `Timing`, forward.rs `verb`.
- Zero cost without machines: the CLI reads `machines.toml` itself only to know if any exist; source: machines.md F15; still true because main.rs `registered_names`, cli.rs `plan`.

## Uncovered

- Old dashboard-as-client / CLI-as-client prose from api.md: condensed into client-server.md; TUI key-level detail (`b`, `d`, `:rename`, MACHINE column) left to the dashboard lead.
- Inbox semantics beyond the wire (threads model, events, forms authoring): inbox lead; api.md keeps only the wire shapes.
- Bridge/mux/frame-protocol internals (caps, flow-control windows, agent routing): helper lead.
- `forwardPorts` config syntax and `config_hash` exclusion details beyond one line: config/instances lead (and the config-toml-spec skill).
- Old docs' steps, smoke-test recipes and "working rules" (before-machines.md, machines.md, verbs-as-requests.md): history, not living truth.
- BUGS.md items cited in before-machines.md ("lazily started daemon seems to hold inherited fds"): untracked file, not verified.
- reference/api.md was carried from the old api.md and spot-checked section by section (envelope, errors, Inbox wire structs, instances params/defaults, bridges.ensure, forwards rows, verbs params/answers, machines structs, routing, relay, npm), not re-derived field by field; every child spawn failed (3/3 slots held by other leads), so no independent second pass.
