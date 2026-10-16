# Clients of the daemon

How the CLI, the dashboard and external programs talk to `devsandbox
serve`, and what each does when no daemon answers. Read this before making
a command use (or stop using) the daemon.

Code: `src/serve/client.rs` (`Conn`, `connect`, `connect_running`,
`ensure_running`, `ensure_bridge`, `set_in_process`), `src/serve/proto.rs`,
`src/serve/verbs_client.rs`, `src/serve/relay.rs` (`stdio`, `GRACE`),
`src/serve/dts.rs`, `src/main.rs` (`--in-process`, `--machine`, routing),
`src/commands/inbox.rs` (`via_daemon`), `src/commands/machine.rs`,
`src/commands/exec.rs` (`AgentRelay`), `src/tui/daemon.rs`,
`src/tui/poll.rs`, `npm/devsandboxes/index.js` + `index.d.ts`.

## The connection

JSON lines over `serve.sock`; the first request is `hello` (`version`,
`build`, `client`: `tui`, `cli`, or another name such as the npm
package's `npm:<name>`, audited as `api:<name>`). The reply carries the
daemon's version and `protocol` (`proto::PROTOCOL`), or `handoff: true`
when the client is newer ([role.md](role.md#version-handoff)). Lines are
capped at `proto::MAX_LINE` (1 MiB). Wire details:
[api.md](../../reference/api.md).

`Conn` is sync and single-threaded: `call` sets aside notifications that
arrive before its response (`next_notification` hands them out),
`call_within` bounds the wait, `call_streaming` hands a verb's
notifications to a callback that also polls a flag each tick (returning
`false` hangs up). An open `Conn` makes its process a holder.

Two ways in:

- `connect`: lazy start (spawn, or the service manager) when nothing
  answers; restarts after a handoff.
- `connect_running`: never starts one; `None` when nothing answers or the
  daemon is exiting.

## `--in-process`

A hidden global flag: this process is a verb run by the daemon or the
dispatcher. `main` calls `client::set_in_process` for it, and also when a
CLI verb falls back in-process (the fallback doesn't pass the flag), after
which nothing in the process lazy-starts a daemon (a draining daemon's
subprocess would race its successor for `serve.lock`; a fallback already
knows nothing answers). Under the flag no command routes to a machine and
the verbs run locally instead of as requests. `ensure_bridge` then needs a
running daemon (`connect_running`), else the caller prints a `note:`.

## Who uses what

| client | daemon use | without a daemon |
|---|---|---|
| `run`/`rebuild`/`start`/`stop`/`rm`/`service rebuild` | verb request via `connect` ([verbs.md](verbs.md)) | `warning:` + in-process; `-d` refuses |
| `run`/`rebuild`/`start` run in this process, after success, neither `--in-process` nor a fallback (in practice `run --env`/`--dispatcher`, which `verbs_client::from_command` keeps local) | `ensure_running` (a dispatcher/inbox instance needs a host) | one `warning:` line |
| `exec` (and lifecycle chains) into a relay-mode instance | `ensure_bridge` with the caller's `SSH_AUTH_SOCK`, held while it runs | `note:` line, no relay |
| `inbox` mutations | `connect_running` + the method | local `wire::decide_*` ops on the store |
| `inbox` reads | none: reads `inbox.json` | same |
| `machine add\|ls\|rm` | `machines.*` via `connect`, never in-process | error |
| a command naming a machine's instance, `--machine` | routed by `machines::cli::route` ([machines.md](machines.md)) | error |
| `port` | none: forwards in its own process ([forwards.md](forwards.md)) | same |
| `ls`, `ps`, `logs`, `inspect`, `stats`, … | none: read `state.toml` and the runtime | same |

## The dashboard

A pure client: it reads no config and calls no runtime (still local: the
Inbox store, `dashboard.toml`, prompt history). `tui/daemon.rs` is one
worker thread owning the long-lived `Conn` (lazy start, reconnect with
backoff; after a handoff it waits for the successor rather than starting
one): it subscribes to `inbox`, `instances`, `forwards`, watches its root
on every (re)connect (`instances.watch`) and relays `instances.snapshot`
pushes; a lost connection marks the tree stale. Inbox `Op`s go out as API
calls while connected, else the loop applies them locally through the same
`inbox::ops`. Quick one-shot calls (logs, inspect, config, build log,
signal, rename, done, cancel) share that connection. `tui/poll.rs` asks
`instances.procs` on a connection of its own and never starts a daemon.
Each verb is its own connection (`tui::spawn_verb`). Detail: the
dashboard docs.

## External clients: `devsandbox api --stdio`

`relay.rs` is a byte relay between stdin/stdout and the socket for
programs that can only spawn a process (the npm package, editor
extensions). It parses nothing: framing, `hello` and the protocol check
stay with the client; diagnostics go to stderr. It lazy-starts the daemon
and holds it while it runs. Exit 0 on stdin EOF (after up to `GRACE`, 5 s,
for answers in flight) or when stdout is gone; 75 (`EXIT_NO_HOST`) when the
daemon closed first (reconnect to start a new one). `--session-env`, used
only by a machine link's remote relay, rewrites each verb request's
`params.env` to the relay's own login-shell env.

The npm package `devsandboxes` spawns this relay (`DEVSANDBOX_BINARY`
overrides the binary). Its typings `index.d.ts` are hand-written;
`serve/dts.rs` (test-only) serializes each wire struct and fails the build
when its keys differ from the matching `interface`.

Limitations: [status.md](../../status.md#daemon)
