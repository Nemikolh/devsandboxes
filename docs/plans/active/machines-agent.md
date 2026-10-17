# Machines: noisy login shells, opt-in agent forwarding

Status: in progress
Items 1 and 2 of [machines-next](machines-next.md). Items 3–5 there (VS
Code spike, remote Inbox, nameless commands) stay unplanned.

Today ([machines](../../architecture/daemon/machines.md),
[ssh-agent](../../architecture/helper/ssh-agent.md)):

- The remote command runs through the login shell (`ssh::remote_command`);
  whatever `.profile`/`.bashrc` prints lands on stdout before the relay's
  first byte and `client::hello` (via `link::handshake`,
  `src/serve/machines/link.rs`) fails on it: the machine is `unreachable`
  with a JSON parse error. Remote forwards already cope
  (`commands::tunnel::read_header` skips lines before its magic).
- Every ssh but a forward's tunnel passes `-A` (`ssh::api_argv`,
  `cli_argv`, `tty_argv`, `tty_argv_with`). The held link
  (`service.rs` `LinkLoop::connect`), `machines.probe|add`
  (`machines/api.rs` `connect`) and each forwarded verb
  (`machines/forward.rs` `verb`, agent from `check_verb_params`) hand ssh
  an `SSH_AUTH_SOCK` through `Dial`'s second argument. On the machine the
  forwarded agent reaches verbs via the relay's `--session-env`
  (`relay::SessionEnv`), which then report it to the remote daemon through
  `bridges.ensure` (`bridge::report_agent` → `AgentCandidates`), and a
  remote daemon lazily started from an ssh session inherits it as its own
  `$SSH_AUTH_SOCK`. Root on the machine can sign with your keys whenever a
  link is up.

Goal: a noisy profile no longer breaks the link; agent forwarding is off by
default per machine, carried over our own frame protocol instead of
`ssh -A`, and switching it off takes effect at once.

## Design decisions (deviations from machines-next flagged *[new]*)

- **Sentinel** before the relay's bytes, read with the tunnel header's
  line skipper; the noise goes to `serve.log`.
- **Agent relay**: a hidden `devsandbox agent-relay --client <c>` on the
  machine, spawned by the local daemon over its own ssh (no `-A`) while the
  machine is wanted and its `agent` is on. Its stdio carries the helper's
  frame protocol (`proto::handshake` + `mux::Mux`): the remote side plays
  the in-container daemon's role (accepts on a unix socket, sends
  `Open { channel: SSH_AGENT }` per connection, `Mux::attach`), the local
  side plays the host bridge's (`Mux::serve` with an `on_open` that checks
  the machine's current `agent` flag, then connects to
  `bridge::host_agent()`).
- **Remote socket**: `<socket_dir>/agent-<client>.sock`, 0600
  (`endpoint::socket_dir`, the client name sanitized to
  `[A-Za-z0-9._-]` and kept short: macOS caps socket paths at 104 bytes).
  One per linked laptop. Unlinked when the relay exits.
- *[new]* **No registration call.** Instead of reporting the socket to the
  remote daemon (`report_agent` needs an instance via `bridges.ensure`),
  `bridge::host_agent()` also considers live `agent-*.sock` files in the
  socket dir, newest mtime first, after the reported candidates and before
  the process's own `$SSH_AUTH_SOCK`. So every process on the machine (the
  daemon's bridges, a `devsandbox exec` run over `ssh -t`, verbs) finds it
  without plumbing, and an exited relay drops out by the liveness check.
  The remote daemon's `Bridges::reconcile` already respawns bridges when
  `has_host_agent()` flips.
- **`--session-env`** sets a verb's `SSH_AUTH_SOCK` to the relay socket
  when it's live at the time of the request line (host git there, e.g.
  worktree clones). *[new]* When it isn't, the login env's own
  `SSH_AUTH_SOCK` (the machine's own agent, if any; no longer ours since
  there's no `-A`) is left as is rather than removed.
- *[new]* **Wanted** gains holders beyond watchers and queued calls: an
  in-flight forwarded verb, and a CLI command re-run on the machine over
  ssh. Without them a `devsandbox exec m/x` with the dashboard closed would
  have no link, so no relay, so no agent.
- **Off** (`machine set m --agent off`): the relay's ssh is killed at once;
  in-flight agent streams cut; later `Open`s refused.

## Step 1 — a noisy login shell doesn't break the link

- `relay::stdio` (`src/serve/relay.rs`): with `session_env`, after `open`
  succeeds and before `pump`, write `\n` + `devsandbox-api 1\n` to stdout
  and flush. The leading newline ends a motd without a final one. Without
  `--session-env` nothing changes (plain API clients, npm).
- Factor the byte-by-byte bounded line skipper out of
  `tunnel::read_header` (`src/commands/tunnel.rs`) into a shared fn (in
  `src/serve/machines/ssh.rs`, which tunnel already imports `Tail` from):
  reads one byte at a time (nothing past the matched newline is consumed),
  bounded at 64 KiB total, hands each line to a matcher, keeps the first
  skipped line for diagnostics. `read_header` keeps its behavior and tests.
- `link::handshake`: read up to the sentinel (same read timeout as the
  hello), then `client::hello` as today. EOF before it → `LinkError::Closed`
  with ssh's stderr tail, as today. Past the bound → `LinkError::Other`
  saying no `devsandbox-api` line came, with the first noise line
  (truncated to ~200 chars). `Link` gains the first skipped line (if any);
  the held link (`service.rs` `LinkLoop::connect`) logs it once per
  connect through `inner.log` (`machine <name>: login shell printed: …`).
- Tests (`link.rs` fake peers): noise lines before the sentinel; noise
  without a trailing newline (the sentinel's leading `\n` ends it); no
  sentinel then EOF; noise past the bound. Update existing fake peers
  (`fake_peer`, `a_spawned_peer_links`) to send the sentinel. A relay test
  that `--session-env` writes it and plain `--stdio` doesn't (factor the
  write so it's testable without a process).
- Docs: machines.md *Link handshake* describes the sentinel and where noise
  goes; reference/api.md's relay section says `--session-env` writes it;
  `docs/status.md` drops *A remote login shell that prints to stdout
  breaks the hello*.

## Step 2 — the agent relay transport (not wired yet)

- Hidden `agent-relay` subcommand in `src/main.rs` (model on `Tunnel`,
  `src/main.rs` `Command::Tunnel`), `--client <name>`, unix-only, in a new
  `src/commands/agent_relay.rs` (module doc says it is the far end of a
  machine's agent relay, docs/architecture/daemon/machines.md *Agent*).
  It: computes the socket path (shared fn, see below), binds it 0600
  (remove a stale file first only if nothing accepts on it; a live one
  means another relay for this client: replace it, newest wins), does
  `proto::handshake` on stdin/stdout, then accepts connections and opens
  one `SSH_AGENT` stream each on a `Mux` over stdout, served by
  `Mux::serve` on stdin. Exits (unlinking its socket) on stdin EOF, a
  write error, or a failed keepalive (`Mux::keepalive`), so a partitioned
  link can't leave a socket that accepts and hangs. Model the accept +
  `attach` loop on the in-container daemon's (`devsbd/src/daemon.rs`
  `accept_agent_clients`), without its 1 s hold-for-a-bridge.
- The local end: `src/serve/machines/agent.rs`, a fn that takes a `Duplex`
  (or any read/write pair) and an `allowed: Fn() -> bool`, does the
  handshake advertising `caps::SSH_AGENT`, and serves `Open`s:
  `SSH_AGENT` and `allowed()` → connect `bridge::host_agent()`; else
  refuse. Returns when the stream ends. Model on the host side of a bridge
  (`src/devsbd/bridge.rs` `open_stream`, the `SSH_AGENT` arm).
- Shared socket path fn, e.g. `endpoint::agent_socket(dir, client)`
  (`src/serve/endpoint.rs`), with the sanitizing and length bound, tested.
- `bridge::host_agent()` (`src/devsbd/bridge.rs`): candidates are the
  reported ones, then live `agent-*.sock` in `endpoint::socket_dir()`
  newest mtime first, then `own_agent()`. Keep `AgentCandidates::pick`
  pure (pass the scanned list in); update its doc and `has_host_agent`'s.
- `ssh::agent_argv(entry, client)` (`src/serve/machines/ssh.rs`): `ssh -T
  -o BatchMode=yes [-p N] -- <host> <devsandbox agent-relay --client c>`,
  no `-A`; test it next to `tunnel_argvs`.
- Tests: an end-to-end over a socket pair: the remote half in a thread
  (`agent-relay`'s core, factored to take its stdio and socket dir), the
  local half with a fake agent (a unix listener echoing), a client
  connecting to the remote socket gets the echo; `allowed` false → the
  client's connection closes promptly; the local half dropping → the
  remote socket file is gone. `host_agent` ordering with a scratch socket
  dir.
- Docs: machines.md gains an *Agent* section describing the relay (wired
  in step 4; say here only what exists); ssh-agent.md's *Which agent*
  bullet gains the socket-dir candidates.

## Step 3 — `agent` per machine in the registry, CLI and API

- `Entry` (`src/serve/machines/mod.rs`) gains `agent: bool`,
  `#[serde(default, skip_serializing_if = …false)]`. Fix every `Entry {…}`
  literal in tests.
- `machines.add` takes `agent` (default false); new `machines.set {name,
  agent}` (`src/serve/machines/api.rs`, registry write through
  `mod.rs`'s locked `update`, then `service` reload, like `add`/`rm`).
  `machines.list`/`get` views carry `agent`. Update
  `npm/devsandboxes/index.d.ts` (the `npm_typings_match` test checks it)
  and docs/reference/api.md.
- CLI (`src/commands/machine.rs`): `machine add … --agent`, new
  `machine set <name> --agent on|off`, `machine ls` shows an agent column.
  Re-`add` of an existing name stays refused.
- Dashboard: where the header lists machines/their link state
  (`src/tui/remote.rs`, the header note), show `agent` for machines that
  have it on. Keep it to a short marker.
- Nothing uses the flag for forwarding yet: `-A` is still on everywhere.
- Docs: machines.md *Registry* and *`devsandbox machine add|ls|rm`* (now
  `…|set`), reference/api.md, the CLI reference/skill if it lists machine
  commands.

## Step 4 — the daemon carries the agent; ssh `-A` dropped from daemon-side ssh

- `service.rs`: per link, an agent-relay ssh (`ssh::agent_argv` through an
  injectable spawner, like `Dial`, so tests run the remote half in-process
  or as `sh`) runs while the link is wanted and `entry.agent` is on,
  served by step 2's local end on its own thread, with the same backoff
  rules as the link (no tight respawn when ssh fails). Its `allowed` reads
  an `Arc<AtomicBool>` the service updates on `reload`; `reload` turning
  it off kills the relay's ssh at once (don't wait for the tick). `rm` and
  daemon exit kill it too.
- Wanted gains holders: a forwarded verb (`forward.rs` `verb`) holds the
  machine wanted for its duration (a guard from `Handle`). With `agent`
  on, the verb waits (bounded, ~5 s) for the relay's handshake before
  sending, so a `run` that clones over ssh doesn't race it.
- Drop `-A` from `api_argv`; drop `Dial`'s agent argument and every caller
  that computes one (`LinkLoop::connect`, `machines/api.rs` `connect`,
  `forward.rs` `verb`/`check_verb_params` keep checking the env but stop
  extracting an agent). `ssh::spawn` loses `agent`.
- `relay::SessionEnv`: per verb line, `SSH_AUTH_SOCK` = the relay socket
  for this relay's `--client` when it accepts a connection now; otherwise
  the login env as is. Keep the rewrite pure (inject the probe).
- Tests: service test with agent on → relay spawned while wanted, gone
  after linger; `set` off → killed promptly; forwarded verb holds the
  machine wanted. `api_argv` test asserts no `-A`.
- Docs: machines.md *ssh* (no `-A` on the API link), *Held links*
  (holders, the relay), *Agent*; ssh-agent.md's *remote machine* row;
  `docs/status.md`: anything not done yet is listed as a limitation
  (CLI re-runs still use `-A` until step 5).

## Step 5 — CLI re-runs and terminal tabs without `-A`

- `cli::route` (`src/serve/machines/cli.rs`): `exec_ssh` replaces the
  process, so it can't hold a daemon connection (and OpenSSH closes
  inherited fds above 2). For a machine with `agent` on, spawn ssh and
  wait instead (`spawn_ssh` already exists for `PerRoot`), holding a
  daemon connection that marks the machine wanted (new `machines.hold
  {name}`: per-connection, released when the connection closes; model on
  `instances.watch`'s per-connection state in `serve::daemon`). The parent
  must ignore SIGINT/SIGQUIT while ssh runs (ssh `-t` owns the terminal)
  and exit with ssh's status. With `agent` off, keep `exec_ssh`.
- Drop `-A` from `cli_argv`, `tty_argv`, `tty_argv_with`. Dashboard
  terminal tabs need no hold: the dashboard watches machines, which makes
  them wanted.
- Tests: `plan`/argv tests updated (no `-A` anywhere); `machines.hold`
  counts as wanted and stops when the connection closes.
- Docs: machines.md *The CLI* and *Agent*; reference/api.md
  (`machines.hold`); ssh-agent.md; `docs/status.md` updated.

## Close

Per the orchestrator workflow: machines.md / ssh-agent.md / api.md checked
against the code, an ADR for "agent forwarding over our own relay, never
`ssh -A`" in `docs/decisions/`, `docs/status.md` limitations (e.g. several
laptops on one machine: the newest relay socket wins), this plan archived,
machines-next.md items 1–2 dropped.
