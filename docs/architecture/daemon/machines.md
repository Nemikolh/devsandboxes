# Machines: remote hosts over ssh

A machine is a remote host running devsandbox, registered with `devsandbox
machine add`. The local daemon links to each machine's daemon over
`ssh <target> devsandbox api --stdio`, merges its rows into watched
snapshots, and forwards requests for its instances. Read this before
touching anything that takes a `machine` param or a `m/name`.

Code: `src/serve/machines/mod.rs` (`Entry`, `path`, `parse`, `update`,
`check_name`, `Where`, `parse_name`, `default_name`), `ssh.rs`
(`api_argv`, `tty_argv`, `tty_argv_with`, `cli_argv`, `parse_target`,
`Dial`, `Duplex`, `ssh_dial`), `link.rs` (`LinkError`, `client_name`),
`service.rs` (`Machines`, `Handle`, `Timing`, `Hooks`), `api.rs`
(`machines.*`, `Ctx`), `forward.rs` (`QUICK`, `FANOUT_WAIT`, `verb`, the
federated joins), `cli.rs` (`plan`, `Plan`, `machine_use`, `route`,
`localize`), `src/serve/snapshots.rs` (`Remotes`, `LinkState`, `merge`),
`src/serve/relay.rs` (`--session-env`), `src/commands/machine.rs`,
`src/tui/remote.rs`. Params: [api.md](../../reference/api.md#machines).

## Registry: `machines.toml`

`<data-dir>/devsandbox/machines.toml`, `[machine.<name>]` with `target`
(`me@host`, an ssh-config alias, or `ssh://[user@]host[:port]`), `roots`
(remote config roots, as the remote canonicalized them) and an optional
`command` (remote devsandbox as shell text, default `devsandbox`; set by
hand, `machine add` has no flag for it). Only the daemon writes it
(`machines.add`/`rm`): locked read-modify-write on `machines.lock`, temp
file + rename, since two daemons overlap during a handoff. A file that
doesn't parse is logged and changes nothing. The CLI reads it only to know
whether any machine exists, so with none every command resolves exactly as
without machines and never starts a daemon it didn't need.

Names: `[A-Za-z0-9._-]+`, not starting with `.` or `-` (`check_name`).
Default: the target's host (`default_name`). `parse_name` splits a CLI
`<name>`: `m/x` → `Where::Machine`, `@/x` → `Where::Local`, bare →
`Where::Any`. Container names can't hold `/` or `@`, so this never
collides with an instance name.

## ssh

One argv builder for the daemon, the CLI and the dashboard. Plain ssh, no
ControlMaster of its own: `~/.ssh/config` decides. The remote command runs
through the login shell (`exec "$SHELL" -lc '…'`, each arg quoted for it),
so the remote `PATH` is the user's.

- `api_argv`: `ssh -T -A -o BatchMode=yes [-p N] -- <host> … api --stdio
  --client <c> --session-env` (the daemon has no TTY: ssh must fail, not
  prompt).
- `tty_argv` / `tty_argv_with`: `-t -A`, no `BatchMode` (terminal tabs,
  interactive re-execs; `tty_argv_with` runs `<runtime> exec -it` for
  service tabs).
- `cli_argv`: `-t` on a terminal, else `-T` (piped bytes stay clean).
- `tunnel_argv`: `ssh -T [-o BatchMode=yes] [-p N] -- <host> … -C <root>
  tunnel [--instance=<name>] [--service=<svc>] --port <n>`, a remote
  forward's bridge ([forwards.md](forwards.md#remote-bridges)). No `-A`
  (a forward's bridge serves no agent); `BatchMode` when `batch` (the
  daemon). The `=` forms keep a name starting with `-` a value.

`Tail` (a bounded ring of a stream's last bytes, `Tail::collect`) keeps
ssh's stderr for `spawn`'s `Peer` and the remote forward opener alike.

`Dial` (`Fn(&Entry, agent) -> Duplex`) is injectable; a `Duplex` is
always a unix socket pumped to the ssh child's pipes, so `client::Conn`
works on it unchanged and tests dial a second test daemon's socket.
`--session-env` makes the remote relay replace each verb request's
`params.env` with its own login-shell env.

## Link handshake

`link.rs`: the hello says version `0.0.0`, build 0, so a link never hands
off the remote daemon (only that host's own clients upgrade it). The
`protocol` must match (`LinkError`); differing semvers within one protocol
are a warning at add and shown in `machine ls`. A remote login shell that
prints to stdout breaks the hello (the error carries ssh's stderr tail).

## Held links: `Machines`

The daemon runs a `Machines` service for its whole life: one
`serve-machine-<name>` thread per registered machine, started, stopped or
restarted when `add`/`rm` change the registry. A link connects only while
wanted (a local connection watching with `machines: true`, or a forwarded
call queued) and for `Timing::linger` (60 s) after. Connected, it
subscribes to `instances`, watches each of the machine's roots while a
local watcher wants machines (not for a link lingering on calls), and
stores pushed snapshots in `Remotes` (memory only, kept while the link is
down).
`LinkState`: `idle`, `connecting`, `connected`, `unreachable` (with the
reason); retries back off from 1 s doubling to 30 s while wanted.
Handshake timeout 20 s. A link isn't an idle holder of the local daemon;
the clients that want it are. On the machine it's a client like any other.

## Forwarded requests

A request carrying `machine` (not registered: `not-found`) has it stripped
and goes to that machine:

- **Quick calls** (`forward::QUICK`: `instances.list|cancel|procs|signal|
  rename|done|buildLog|resolve`, `containers.logs|inspect`,
  `sandboxes.list|get`, `services.get`) go over the held link, one in
  flight per machine, `Timing::call` (15 s) each. A timeout drops the link
  (its late answer would be read as the next call's); a call arriving
  during the backoff retries the connect at once, and calls queued when a
  connect fails get `failed` (`<machine>: unreachable: …`). The remote's answer, error code and data
  included, is the answer.
- **Verbs** get a fresh dial per request, so a build never holds up the
  link. `output`/`progress` are relayed with `request` rewritten to the
  local id. While its ssh runs it's a `verbs` holder here. A hang-up, or a
  local drain, closes the ssh, which the remote sees as a hang-up: cancelled
  there unless `detach`.
- **Federated** `instances.resolve` / `sandboxes.list` with `machines:
  true` ask this host and every machine in parallel, each machine given
  `FANOUT_WAIT` (5 s). Remote matches come back qualified (`box/web-2`)
  with an additive `found: [{name, machine, root}]`; a machine that doesn't
  answer adds nothing, and an empty result names the unreachable ones.

## Federated snapshots

`instances.watch {dir, machines: true}` (the dashboard always sends it)
gets every push merged with each machine's rows (`snapshots::merge`): rows
gain `machine` and keep their remote `root`, and a top-level `machines`
array carries each machine's `name`, `target`, `state`, `error`, `version`,
`command`, `runtime`. A machine's push or state change re-publishes those
watchers' roots from the last local params. Details:
[host-poll.md](host-poll.md#snapshots).

## The CLI: `cli.rs`

`route` (I/O) runs `plan` (pure) in `main` before a verb goes to
`verbs_client` or any command runs locally:

- Under `--in-process`: nothing fans out; `--machine` is an error.
- **Verbs** (`run`, `rebuild`, `start`, `stop`, `rm`) keep their name;
  `verbs_client` resolves it across machines (`instances.resolve` /
  `sandboxes.list` with `machines: true`), picking on a TTY when ambiguous,
  and sends the request with the match's `machine` and root.
- **Other named commands** (`exec`, `logs`, `inspect`, `done`, `undone`,
  `rename`, …) are resolved through the daemon; one on a machine is re-run
  as `ssh [-t] <target> devsandbox <args>` (`exec` of ssh). `logs` /
  `inspect` with a bare `devsandbox-…` container name stay local. `vscode`
  and `port` on a machine's instance are refused.
- **`--machine m`** is for nameless commands (`machine_use`): `ps`,
  `stats` run once over ssh; root-bound ones (`ls`, `status`, `gc`,
  `service ls`) run once per registered root with `-C <root>` and a
  `==> m:<root>` line (`--json` with several roots is an error); `stop
  --all`, `rebuild --all`, `start --all`, `service rebuild` go as verbs.
  On a named command it's an error (qualify the name instead); refused for
  `inbox`, `vscode`, `port`, `serve`, `api`, `machine`, the dashboard.
- Without a local daemon, a qualified remote name and `--machine` fail
  ("needs devsandbox serve"); a bare name resolves locally.

## `devsandbox machine add|ls|rm`

A client of `machines.*` only, never in-process (`src/commands/machine.rs`).
`add` without `--config` calls `machines.probe` (the remote's
`roots.list`: every `config_dir` in its `state.toml` plus
`~/.devsandboxes`) and lets the user multi-select on a TTY; off a TTY it
takes the one usable candidate, else errors listing them. `machines.add`
then checks every given root on the machine (`roots.list {paths}`: `~`
expanded with the remote home, canonicalized, config loaded).

## Dashboard

`src/tui/remote.rs`: rows are keyed `machine/name` (bare for local),
routed back to a request's `machine` + remote `root`; terminal tabs use
`tty_argv`; a machine without rows shows `box: connecting` or `box:
unreachable: <why>` in the header. VS Code and port forwarding on a remote
row answer "isn't supported yet".

Limitations: [status.md](../../status.md#daemon)
