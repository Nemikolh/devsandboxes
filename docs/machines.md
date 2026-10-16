# Machines: remote hosts over ssh

Status: built. Steps 1–6 in commits e897bf2..5647d09, step 7 (these docs)
after; what the code ended up doing differently is noted in place, and
what it doesn't do yet is under *Known limitations*.

Rollout step 5 of `docs/client-server.md` (*Machines*, *Exec and terminal*,
*Config roots*). Steps 1–4 and the pre-tasks (`docs/before-machines.md`) are
done. This doc covers `machine add|ls|rm`, the ssh transport, federation
(the local daemon merges each machine's rows), forwarding requests for remote
instances, names that resolve across machines on the CLI, and remote
terminals.

```
devsandbox machine add me@my-server [--name my-server] [--config ~/sandboxes]…
devsandbox machine ls | rm <name>
devsandbox run api                       # `api` is only defined on my-server
devsandbox exec test                     # `test` only exists there: ssh -t … devsandbox exec test
devsandbox stop my-server/test           # qualified: needed only when ambiguous
devsandbox exec @/test                   # the local one
devsandbox ps --machine my-server        # a command without a name: over ssh
```

## Decisions (the user's)

- **Dashboard: merged.** Every dashboard shows its local root plus every
  machine's roots. `instances.watch` takes an opt-in `machines: true` (the
  TUI always sends it), sandboxes are grouped per (machine, sandbox), and a
  machine column appears **only when at least one machine is registered**
  (blank for local rows).
- **Federation: push.** The local daemon watches each machine's roots on
  that machine's held connection, only while a local connection watches with
  `machines: true`, and merges the remote `instances.snapshot`s into its
  cache. `instances.list` gains `machine` and proxies that one machine, with
  no fan-out merge.
- **Verbs: one ssh per forwarded verb**, like local's one connection per
  verb. Quick calls share the held connection. Cancel, hang-up and detach
  pass through unchanged.
- **Plain ssh**, no ControlMaster of our own: the user's `~/.ssh/config`
  decides (a user who wants sharing sets `ControlMaster auto` there).
- **Env: the login shell.** The remote side runs as `ssh -T -A -o
  BatchMode=yes [-p N] -- <host> 'exec "$SHELL" -lc "exec devsandbox api
  --stdio --client machine:<local host> --session-env"'`, and the relay
  replaces each relayed verb request's `env` with its own.
- **Version skew: protocol.** A machine connection says hello as version
  `0.0.0`, so it never hands off the remote daemon, and requires an equal
  `protocol`. A mismatch fails `machine add` with both versions and, on a
  reconnect, marks the machine unreachable with that reason. Different
  semvers within one protocol are allowed: a warning at add, shown in
  `machine ls`. A method the remote lacks surfaces as its `unknown-method`.
- **Names resolve across machines; no `--machine` for named commands.**
  A bare name (a sandbox for `run`, an instance, sandbox or folder name for
  the rest) is looked up locally and on every machine. One match is used,
  wherever it is. Several matches are ambiguous: a pick on a TTY, otherwise
  an error listing the qualified names. The qualified form is
  `<machine>/<name>`, with `@/<name>` for local. Docker container names
  can't hold `/` or `@`, so neither can a working instance name.
- **Default machine name**: the target's host (`me@my-server.com` →
  `my-server.com`; an ssh-config alias stays as is). `--name` overrides it.
- **`-C` stays a local path.** A machine's roots are the ones registered
  with `machine add --config` (or picked at add).
- **Commands without a name** (`ps`, `stats`, `status`, `ls`, `gc`, `stop
  --all`, `rebuild --all`, …) keep a `--machine <m>` flag, which runs them
  over ssh. Without it they're local, as now. Making them apply to every
  linked machine is a step after step 5 (*Later*).
- **CLI: ssh for the rest.** A command that doesn't go through the daemon
  (`logs`, `exec`, `inspect`, `done`, `rename`, …) whose name resolves to a
  machine, or a nameless one with `--machine`, runs as `ssh [-t] <target>
  devsandbox <same args>`. The daemon hands back the target.
- **Service tabs**: `ssh -t <target> <runtime> exec -it …`, the same prefix
  as instance tabs.
- **A machine that's down**: its last rows stay, in memory only, marked
  `unreachable`. A machine not reached since the daemon started shows as one
  `unreachable: <why>` line. Built: a note in the dashboard's header,
  `box: connecting` or `box: unreachable: <why>`, for each machine without
  rows.
- **Not now**: a default machine (an open question for the Windows plan),
  remote Inbox threads (after step 5, with the Inbox behind the daemon),
  remote forwards (step 6), remote VS Code (step 7).
- **Later, separate from step 5**: auto-update a machine's devsandbox to
  match the newest one linked to it (*Later*).

## Flagged (mine; say if you disagree)

- **F1 Layout**: `src/serve/machines/` with `mod.rs` (the registry,
  `machines.toml`), `ssh.rs` (the argv builder and the injectable `Dial`),
  `link.rs` (the held connection per machine) and `forward.rs` (requests
  forwarded to a machine). `src/commands/machine.rs` is the CLI.
- **F2 Registry**: `<data-dir>/devsandbox/machines.toml`, next to
  `state.toml`, holding `[machine.<name>]` with `target` (what ssh takes:
  `me@host`, `host`, an ssh-config alias; an `ssh://` URL is turned into
  that, a port included as `-p`), `roots = ["/abs", …]` (remote paths, as
  the remote canonicalized them) and an optional `command` (the remote
  devsandbox, default `devsandbox`). Locked read-modify-write and an atomic
  rename, like `src/serve/forward_store.rs:94`. A name is
  `[A-Za-z0-9._-]+` (never `@`, never `/`), so `my-server/web-2` splits at
  the first `/` and `@/web-2` is local.
- **F3 ssh argv** (`ssh.rs`, one fn used by the daemon, the CLI and the
  TUI): `ssh -T -A -o BatchMode=yes [-p N] -- <host> <remote>` for the API
  (`-t` instead of `-T`, and no `BatchMode`, for terminals and interactive
  CLI commands; a CLI re-exec off a terminal keeps `-T`). `<remote>` is
  `exec "$SHELL" -lc '<cmd>'`, with `<cmd>` `exec <command> '<arg>'…`, each
  arg quoted for the login shell and `<cmd>` again for the outer one;
  `command` is shell text, never quoted (`~/bin/devsandbox` expands). `BatchMode`: the daemon has no TTY, so ssh must fail
  instead of prompting. `SSH_AUTH_SOCK` for `-A` is the requester's (a verb
  request's `env`). For the held link it's the daemon's preferred agent,
  the one the bridges already track (`bridges.ensure`'s `agent`).
- **F4 Relay `--session-env`** (hidden; `src/serve/relay.rs`): the
  upstream copy becomes line-aware only under that flag. A request whose
  `method` is a verb (`verbs::is_verb`) gets `params.env` replaced by the
  relay's own env (`verbs_client::caller_env`), and any other line passes
  through as is. The downstream copy stays a byte copy.
- **F5 `roots.list`**: a new method on every daemon that answers this
  host's candidate roots, `[{"root": "/abs", "sandboxes": [{"name"}…],
  "error": null}]`: every distinct `config_dir` in its `state.toml`, plus
  `~/.devsandboxes` if it exists, sorted, with no disk scan. This is what
  `machines.probe` asks a remote. Local methods: `machines.probe {target}`,
  `machines.add {target, name, roots}`, `machines.list`, `machines.rm
  {name}`. Built: `roots.list {paths}` checks exactly those paths instead
  (`~` expanded with that host's home, canonicalized, config loaded), which
  is how `machines.add` checks *every* given root on the machine, picked or
  typed.
- **F6 Routing param**: methods that name an instance, root or container
  take an optional `machine`. The local daemon strips it and forwards the
  request. The API never parses `my-server/web-2`: the CLI does, and the
  TUI's rows carry `machine`. A `machine` that isn't registered is
  `not-found`.
- **F7 Row fields** (additive): `InstanceRow` gains `root` (its
  `config_dir`, `null` for old instances) and `machine` (absent when local,
  `skip_serializing_if`). `SandboxRow` and `ServiceRow` gain `machine` and
  `root` too. `status --json` gains `root`. A watch with `machines: true`
  adds a top-level `machines: [{"name", "target", "state":
  "connected"|"connecting"|"unreachable", "error", "version"}]`, one per
  registered machine. A row of a machine that isn't `connected` is
  `unreachable` to the TUI. Built: `state` also has `idle` (not wanted, so
  not connected), entries add `command` and `runtime` (the remote's
  `runtime_name`), and `sandbox_count` counts every machine's sandboxes
  too. A remote instance row keeps the `root` its machine gave it; remote
  sandbox and service rows get the remote root they were read from.
- **F8 Dedup**: `snapshot::join` lists every instance in `state.toml`,
  whatever its root (the local "(not in config)" orphans). For a machine,
  each instance is taken once: from the root it was made in (`root`), else
  from the machine's first root. A remote root's sandboxes and services are
  shown, its orphans aren't.
- **F9 Link lifetime**: a machine's held connection opens when wanted (a
  watch with machines, or a forwarded quick call) and closes 60 s after the
  last want. Backoff is 1 s, 2 s, … 30 s, like the dashboard's
  (`src/tui/daemon.rs:256`). A held link is not an idle holder of the local
  daemon: it's the clients that want it that hold. On the remote end it is
  a client like any other.
- **F10 Quick calls on the link** are serialized, one in flight per
  machine, with a 15 s timeout (the call answers `internal: <machine>:
  timed out`). Built: a timeout drops the link (its late answer would be
  read as the next call's) and it reconnects; a call queued while the
  machine is unreachable answers `failed` at once. `instances.procs` (a
  `docker exec` per instance) uses the link too: the TUI asks for one
  machine per request.
- **F11 Federated resolve**: `instances.resolve {name, machines: true}`
  asks this host and every registered machine in parallel. Each machine
  gets 5 s over its link, so a link already up costs nothing, and links
  stay up 60 s after use (F9). The matches are the union; a remote one is
  qualified (`my-server/web-2`), a local one isn't. An additive `found:
  [{"name", "machine", "root"}]` (machine `null` = local) gives the CLI
  each match's machine and root. A machine that doesn't answer adds
  nothing, and if nothing matched at all, the `not-found` message names the
  unreachable machines. A qualified name asks only its machine (`@/` only
  this host). Without `machines`, `instances.resolve` is unchanged.
  `sandboxes.list {root, machines: true}` does the same for `run`: entries
  gain `machine` and `root`, from the local root plus every machine's
  roots.
- **F12 A machine's root**: `run a/web` (or a bare `web` found only on
  `a`) uses the machine root whose config has `web` (two of a machine's
  roots defining it is ambiguous like any other match). An instance verb
  uses the instance's `root` from `found`. A machine's roots are only the
  registered ones.
- **F13 No local daemon**: a qualified remote name and `--machine` need
  it, since the registry and the links are the daemon's. The in-process
  fallback refuses them with "needs devsandbox serve". A bare name then
  resolves locally, as today.
- **F15 Zero cost without machines**: before fanning out, the CLI checks
  whether `machines.toml` has any entry, reading the file, not the daemon.
  With none, every command resolves exactly as today, so local `exec`,
  `logs`, … never start a daemon they didn't need before. This is the one
  place the CLI reads the daemon's registry itself.
- **F16 Nameless commands over ssh** with `--machine m`: `ps`, `stats`
  and the other root-less ones run once. A root-bound one (`ls`, `status`,
  `gc`, `service ls`, `stop --all`, `rebuild --all`, `start --all`) runs
  once per registered root of `m` with `-C <root>`, each preceded by a
  `==> m:<root>` line on stderr. `--json` with several roots is an error
  that lists them (concatenated documents don't parse). `--machine` on a
  named command is an error that says to qualify the name instead.
- **F17 Container names** (`logs` and `inspect` also take
  `devsandbox-…` names) stay local unless qualified (`a/devsandbox-web`).
- **F14 Tests**: the transport is a `Dial` (`Fn(&Entry, Purpose) ->
  Result<Duplex>`). Production spawns ssh. Tests connect straight to a
  second test daemon's socket (`daemon::tests::spawn_daemon`). The ssh argv
  and quoting are pure fns with unit tests. Smoke tests use a fake `ssh` on
  `PATH` that drops its options and runs the remote command with another
  `XDG_*` set: a second "machine" on the same docker (unique instance
  names, since container names are per docker).

## Step 1: transport and the remote side

The pieces a remote needs, plus the dialer. No registry yet.

- `src/serve/machines/ssh.rs` (F1, F3): `api_argv(entry, agent)`,
  `tty_argv(entry, remote_args)`, `remote_command(command, args)`
  (`$SHELL -lc` plus POSIX single-quote quoting), and
  `parse_target` (`ssh://user@host:port` → `user@host` + `-p port`; plain
  targets pass through). `Dial` and `Duplex`: a reader, a writer and a
  closer (kill and reap the ssh child). The production dial spawns ssh with
  piped stdin and stdout, and keeps the last ~4 KiB of stderr so a failure
  says why (`Permission denied`, `devsandbox: command not found`).
- Link handshake (`link.rs`, connect only for now): over a `Duplex`,
  `hello {version: "0.0.0", client: "machine:<local hostname>"}`. Return the
  remote's `HelloResult`, or a typed error for a protocol mismatch that
  names both versions (`src/serve/proto.rs:21`). Reuse `client::Conn`
  (`src/serve/client.rs:29`) over a generic stream if that's simple.
  Otherwise give `Conn` a constructor over `Box<dyn Read/Write>`. Don't
  fork the line reader.
- Relay `--session-env` (F4): `src/main.rs:297` (`Api`) gets a hidden
  flag, and `src/serve/relay.rs` `stdio`/`pump` (:55/:83) a mode that
  rewrites verb requests' `env` line by line (`MAX_LINE` still applies).
  Tests: a verb line has its env replaced, other lines arrive
  byte-identical, a partial line is held until its newline.
- `roots.list` (F5) in `src/serve/api.rs` `call` (:85), with the state
  path and home injectable like the other handlers. Reuse `sandboxes_list`'s
  config read (`config_at`, :602) per root, and put an unreadable config in
  the entry's `error`.
- `index.d.ts`: `RootCandidate`. Tests in `api.rs`'s dts cases.
- Tests: the argv and quoting (a `'` inside a path, a target with a port),
  the hello against a test daemon over a socket `Duplex` (protocol
  equal → ok; a fake peer answering protocol 2 → the mismatch error), and
  `roots.list` on a scratch state.

## Step 2: registry, `machines.*` and `machine add|ls|rm`

- `src/serve/machines/mod.rs` (F2): `Entry`, `path()` (next to
  `State::path()`, `src/state.rs:352`), `load`, `update` (locked, atomic),
  and name checks.
- Methods (connection-free, in `api.rs` or a `machines::call`, wired from
  `daemon.rs` `handle` :769 with the `Dial` from `Options`, which is
  injectable like `Options.join` :105):
  - `machines.probe {target}` dials, says hello and calls `roots.list`.
    Result: `{"version", "protocol", "roots": [RootCandidate…]}`. Errors:
    `invalid` (bad target), `failed` (ssh failed or the hello failed:
    `message` is ssh's stderr tail or the mismatch).
  - `machines.add {target, name, roots}` checks a root that isn't among
    the probe's candidates on the remote (`sandboxes.list {root}`: it
    exists and its config parses), then saves the entry. A name already
    taken is `invalid`. Answers `{"name", "warning": null | "…"}`, where
    the warning is a semver differing from ours. Built: every root is
    checked with `roots.list {paths}` (F5) and saved as the machine
    canonicalized it; `name` is optional (the default name), `roots` must
    not be empty, and the answer also has `roots` and `version`.
  - `machines.list`: `[{"name", "target", "roots", "state", "error",
    "version"}]`. `state` is `idle` until step 3 adds links.
  - `machines.rm {name}`: `not-found` if absent.
- CLI: `src/commands/machine.rs` and `Command::Machine` in `src/main.rs`.
  `add <target> [--name <n>] [--config <remote path>]…` (default name:
  the target's host, see *Decisions*; a taken name is an error that says
  to pass `--name`), whose long help says
  adding a machine means trusting it: whoever controls it can act as you
  in its containers, and its socket is as powerful as `docker.sock`. Also
  `ls [--json]` and `rm <name>`. Without `--config`, `add` probes. On a TTY
  it shows a numbered multi-select (`1,3`; model it on `commands::pick`,
  `src/commands/mod.rs:241`) of the candidates, each with its sandboxes,
  plus "other path…". Off a TTY it takes exactly one candidate, and zero or
  several is an error that lists them and points at `--config`. With
  `--config`, there's no picker. The CLI talks to the daemon only (lazy
  start); no daemon is an error (F13).
- `index.d.ts`: `MachineRow`, the probe result. Tests: registry
  round-trip and lock, name rules, the add flow against a test daemon
  standing in as the remote (the F14 dial), the non-TTY rules as a pure fn.
- Smoke: the fake-ssh second machine. `machine add` without `--config`,
  then with a typed bad path (refused), `ls`, `rm`.

## Step 3: links and the federated snapshot

- `src/serve/machines/link.rs`: one thread per machine, owned by a
  `Machines` service the daemon starts with the host side
  (`daemon.rs:211`). It connects when wanted (F9), then `subscribe
  {instances}` and `instances.watch {dir}` for each of the entry's roots.
  Built: the threads and the service are `service.rs` (`link.rs` is the
  hello), and the roots are watched only while a local watcher wants
  machines, not for a link a call opened (the remote polls its runtime for
  a watch).
  Each pushed `instances.snapshot` is stored under (machine, root). State
  (`connecting`, `connected`, `unreachable` + why) goes into
  `machines.list`. Reconnect follows F9. A registry change (`add`/`rm`)
  reaches it at once (the method tells the service).
- `src/serve/snapshots.rs` (built for this, F8 in `before-machines.md`):
  a machine dimension next to `Root`. Remote rows are kept per (machine,
  root), last known, tagged `machine` (F7), de-duplicated (F8). A local
  root's push for a `machines: true` watcher is the local snapshot plus
  every machine's rows plus `machines`. A remote push re-publishes every
  watched local root that wants machines. Keep the pure-fn style: the merge
  is a fn over values, tested without a daemon.
- `daemon.rs`: `Pending.watched` (:551) records each root's `machines`
  flag. `watch_root` (:920) parses it (`api::watch_dir` :190 grows a
  sibling). `publish_snapshot` (:458) and the host poll's `on_snapshot`
  (`daemon.rs:221`) push the right variant per connection, and
  `watched_roots` (:472) also says whether any watcher wants machines (the
  link's want).
- `src/snapshot.rs`: `root` and `machine` on the rows (F7), filled for
  local rows from `Instance::config_dir` (`src/state.rs:99`). Update
  `docs/json-output.md` (`root`).
- `instances.list {dir, machine}` proxies that machine on its link; this
  is the first forwarded quick call. The forward plumbing (a request/reply
  channel into the link thread, F10) is built here and reused in step 4.
- `index.d.ts`: the row fields, `machines`, the watch param. Update
  `npm_typings_match_every_notification` if the snapshot's params change
  (`daemon.rs:1585`).
- Tests: merge and dedup, unreachable rows kept, a never-reached machine,
  a watch without `machines` unchanged byte for byte. Plus the link against
  a test daemon: rows appear, the remote daemon stops (the machine goes
  unreachable with the rows kept), it comes back (they refresh).

## Step 4: forwarding requests to a machine

- `src/serve/machines/forward.rs`. The quick calls with `machine` (F6) go
  on the link: `instances.cancel`, `.procs`, `.signal`, `.rename`, `.done`,
  `.buildLog`, `.resolve`, `containers.logs`, `.inspect`, `sandboxes.list`,
  `.get`, `services.get`. The remote's error comes back as is, its code
  kept. In `daemon.rs` `handle` (:769), a request with `machine` goes to the
  forwarder before `api::call`.
- Verbs with `machine` (`run`, `rebuild`, `start`, `stop`, `rm`,
  `services.rebuild`): a fresh `Dial` per request, using the request
  `env`'s `SSH_AUTH_SOCK` for `-A` (F3), then the hello and the request
  minus `machine` (the remote relay swaps `env`, F4). The remote's `output`
  and `progress` are relayed onto the local connection with `request`
  rewritten to the local id, and the answer is passed through (`failed`'s
  `data`, `confirm`, `detached` included). A local hang-up (`ConnPeer::
  hung_up`, `daemon.rs:838`) closes the `Duplex`, so the remote relay sees
  EOF, which is the remote's hang-up: cancel unless `detach`. A drain
  (`draining`) also closes it, which matches the local rule (a daemon exit
  leaves verbs running) only for `detach` requests. For held non-detach
  verbs, decide: either keep the ssh child running past the drain
  (`setsid`, adopted like a spooled verb), or accept that a local handoff
  cancels a remote CLI verb. Say which in the review notes.
- Is a forwarded verb a local idle holder? Yes, while its ssh runs. Make
  it count in `Shared::holders` (:384).
- `instances.resolve` and `sandboxes.list` with `machines: true`
  (F11), and `machines.get {name}` → `{"target", "command", "roots",
  "ssh": [argv prefix]}` (for the CLI's re-exec and for the TUI). Also
  `ssh` in each `machines` snapshot entry, so the TUI needs no call.
  Built: no argv on the wire. `machines.get` answers `{"name", "target",
  "command", "roots"}` and snapshot entries carry `target` and `command`;
  Rust callers build the argv with `ssh::tty_argv` / `cli_argv`, other
  clients from those two fields. The drain question above: a local daemon
  exit cancels a held non-`detach` remote verb (*Known limitations*).
- `index.d.ts`: `machine` on the params, `found`. Tests (test daemon as
  remote): a forwarded quick call and its error code, a streamed verb with
  ids rewritten, a hang-up that cancels the remote verb (a `sh` `Runner`
  on the remote test daemon, as `daemon.rs:1321` does), detach that
  survives a hang-up, an unknown machine.

## Step 5: the CLI

- A shared name parser (`commands/mod.rs` or `serve/machines/`):
  `<machine>/<name>`, `@/<name>` or bare, as a `Where` enum (`Local`,
  `Machine(m)`, `Any`). Pure, with tests.
- `src/serve/verbs_client.rs`: names go through the federated resolve
  (F11) unless no machine exists (F15). `run`'s sandbox goes through
  `sandboxes.list {machines: true}` (`prepare`, :246): one match runs where
  it is (F12), and the TTY picker without a sandbox lists them all, remote
  ones qualified. Instance verbs go through `instances.resolve` (`resolve`,
  :333): a TTY pick among qualified names, else the error listing them.
  The chosen match sets `machine` and `root` on the verb (:428) and on
  Ctrl-C's `instances.cancel` (`cancel_by_name`, :496). The
  lost-connection follow (`lost`, :558) can't read a remote build log
  locally: for a remote verb it says so and exits 1.
- `src/main.rs` `Cli` (:30): a global `--machine <name>`, for nameless
  commands only (F16).
- Other named commands (`main.rs:454`: `logs`, `inspect`, `exec`, `done`,
  `undone`, `rename`): the `<name>` is resolved the same way. A local match
  runs in-process as today, with the resolved name. A remote one goes
  through `machines.get`, then `exec`s (unix `CommandExt::exec`) `ssh` with
  the same args: the name unqualified, `-C` dropped and replaced by the
  instance's root. `-t` is used when stdin and stdout are terminals.
  `rename` stays exact (no sandbox/folder matching), but a qualified or
  remote-only name works. `vscode` and `port` on a remote instance: "not
  yet" (steps 7 and 6). Nameless commands with `--machine`: F16. Keep the
  argv rewrite a pure fn, with tests.
- `--machine` on `machine …`, `serve`, `api` or `inbox` is an error.
- Tests: the name parser, argv rewriting, the root choice (pure over
  `sandboxes.list` answers), ambiguity across local and a machine, F15's
  short-circuit, and `verbs_client` against a test daemon with a machine
  (as in step 4).
- Smoke (fake-ssh machine): `run <remote-only sandbox>`, `run` of a
  sandbox defined on both (the error, then `a/x`), `stop x` and `rm x` for
  a remote-only instance, Ctrl-C during a remote `run` (rolled back on the
  remote), `run -d`, `exec x -- uname -a`, `exec @/y`, `ps --machine m`,
  and a local `exec` with no machines registered (no daemon started).

## Step 6: the dashboard

- `src/tui/daemon.rs` `watch_call` (:555): `machines: true`.
  `DaemonUpdate::Snapshot` carries `machines`.
- Tree: sandboxes keyed by (machine, name) (`data.rs:83` `visible_nodes`,
  the collapse key `tree.rs:29`, the process-expand key `data.rs:143`, the
  `stopping`/`starting` sets in `actions.rs:147`), so that `web` on two
  machines is two groups. The machine column (`ui.rs:1691` titles,
  `instance_tree_row` :1850, `sandbox_tree_row` :1800) is only drawn when
  `machines` isn't empty. A machine that isn't `connected` turns its rows'
  status into `unreachable` (styled like `missing`), and a machine with no
  rows yet gets one `unreachable: <why>` / `connecting` line.
- Actions carry the row's `machine` and `root`: `verb_call`
  (`src/tui/mod.rs:651`) and `spawn_verb` (:774). Every `Cmd` that names an
  instance or container (`daemon.rs:120`), procs (one request per machine,
  `poll.rs:195`), the build log, signal, rename and cancel.
- Terminals: `ExecCli::argv` (`terminal.rs:44`) and `term_argv` (:456)
  prefix the snapshot entry's `ssh` argv (`-t`) for a remote row or
  service. The tab dedup key (`terms.find(&container)`, :405) becomes
  (machine, container).
- Not yet, each a status line: `o`/`:code` on a remote row (VS Code, step
  7), `p`/`:port` (forwards, step 6), Inbox threads of remote instances
  (none arrive yet).
- Tests in `app/` (`test_support.rs` fixtures with a machine): grouping,
  the column only with machines, unreachable labels, the argv prefix.
- Smoke: a dashboard with the fake-ssh machine. Rows show under their
  machine. Run `s`/`d`/`b`/`t` on a remote row and a terminal on a remote
  service, kill the remote daemon (the rows show unreachable, then come
  back), and check the screen through vt100 with and without a machine (no
  column).

## Step 7: docs

- `docs/client-server.md` (*Machines*, *Exec and terminal*, *Rollout*,
  *Open questions*), `docs/api.md` (*Machines*, the `machine` param,
  `roots.list`, `machines.*`, `instances.watch` `machines`, `instances.resolve`
  `found`, the relay's `--session-env`), `docs/serve.md`,
  `docs/high-level-architecture.md`, the CLI skill
  (`skills/devsandbox-cli/`) for `machine …` and `--machine`, the site's
  architecture and dashboard pages (`site/src/content/docs/`), `AGENTS.md`'s
  module map (`serve/machines/`, `commands/machine.rs`), `CHANGELOG.md`
  `## Unreleased`.
- Checks: `cd site && pnpm check && pnpm build && pnpm test`.

## Known limitations

- **A local daemon exit cancels a held remote verb.** A drain (handoff,
  idle, `shutdown`) cuts the verb's ssh like a hang-up, so a non-`detach`
  verb on a machine (every CLI verb) is cancelled there, unlike a local
  verb, which runs on and is adopted. `detach` verbs (the dashboard's, `-d`)
  run on.
- **A hang-up during the dial isn't noticed.** The dial and hello take up to
  20 s; a client that leaves meanwhile has its verb started, then cancelled
  at the first check.
- **The remote login shell must keep stdout clean.** A `.profile` or rc file
  that prints to stdout breaks the hello (the error names ssh's stderr when
  there is any).
- **One ssh handshake per verb**, and `BatchMode` on every daemon-side ssh:
  a key that needs a passphrase prompt fails. Use an agent, or a
  `ControlMaster auto` in `~/.ssh/config` to share one connection.
- **No `--command` on `machine add`.** A remote devsandbox that isn't on the
  login shell's `PATH` is set by hand: `command = "~/bin/devsandbox"` under
  `[machine.<name>]` in `machines.toml`.
- **Remote rows are memory only.** A fresh daemon shows a machine without
  rows (`box: connecting`) until its link connects.
- **Dashboard**: the Services tab has no machine column; two of a machine's
  roots defining one sandbox name show it from the first root; `o`/`:code`
  and `p`/`:port` on a remote row are "not yet" status lines.
- **CLI messages**: a failed remote build's hint names the bare instance
  (`rebuild web-4`, not `box/web-4`); `stop box/x` matching nothing
  doesn't name the machine; a remote `run`'s stdout comes one line late (so
  its printed name can be qualified).
- **Nameless commands without `--machine` are local only** (*Later*).
- **Not there yet**: remote Inbox threads, remote forwards and `port`
  (rollout step 6), remote VS Code and `vscode` (step 7), a default machine.

## Later (noted, not in step 5)

- **Auto-update a machine** (the user's idea): a machine's devsandbox
  follows the newest version linked to it. A sketch: at the link's hello,
  if the local binary is newer and the remote's platform matches, push the
  binary (scp to the remote's `command` path, atomically) and reconnect; the
  remote's own version handoff then restarts its daemon. Open questions:
  platform/arch mismatch (a macOS laptop, a Linux server: download the
  release instead), dev builds, a remote shared by several laptops (the
  newest wins), and opting out.
- **Nameless commands across every machine** (the user's, the step after
  step 5): `ps`, `stats`, `status`, `ls`, `gc`, `stop --all`, … without
  `--machine` apply to this host and every linked machine, merged (a
  machine column) through the daemon.
- Remote forwards (step 6), remote VS Code (step 7), remote Inbox threads,
  a default machine (Windows).
