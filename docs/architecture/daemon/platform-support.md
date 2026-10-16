# Platform support of the daemon

The daemon and everything that needs it compile on unix only (Linux,
macOS). The release workflow also builds an `x86_64-pc-windows-msvc`
binary; this page says what that binary does without a daemon. Read it
before touching a `#[cfg(unix)]` gate.

Code: `src/main.rs` (`mod serve`, `mod tui`, the `Serve`/`Api`/`Machine`
subcommands, `detach_needs_daemon`), `src/commands/mod.rs` (`mod machine`),
`src/serve/endpoint.rs` (unix socket only), `src/commands/port.rs`,
`src/commands/exec.rs` (`has_host_agent`), `src/commands/run/cancel.rs`
(`interrupt`), `src/commands/inbox.rs` (`via_daemon`),
`.github/workflows/release.yml`.

## Unix-only modules

`src/serve/` and `src/tui/` are gated in `main.rs`; so are the `serve`,
`api` and `machine` subcommands, which don't exist off unix. In
`src/devsbd.rs`, `bridge`, `desktop` (popups), `forward` (the host
forwarder) and `mux` are unix-only too. `endpoint.rs` is the seam a named-pipe transport would plug
into (`Listener`, `connect`, `Stream`); today it's a `UnixListener`.

## Off unix

| what | behavior |
|---|---|
| no subcommand on a TTY | error: `the dashboard needs devsandbox serve (unix only for now)` |
| `--machine` | error: `--machine needs devsandbox serve (unix only for now)` |
| `run`/`rebuild`/`start`/`stop`/`rm`/`service rebuild` | run in the CLI's process (no verb request, no `ensure_running`) |
| `-d` on `run`/`rebuild`/`start` | error: `` `-d` needs devsandbox serve, which is unix-only for now `` |
| `port` | error: `port forwarding is unix-only for now` |
| ssh-agent relay | `has_host_agent` is false: no `SSH_AUTH_SOCK` injection, no bridge wait |
| `inbox` mutations | always the local `wire::decide_*` path |
| cancelling a recorded pid | `interrupt` answers `not supported on this platform` |

Everything that doesn't need a host process (config, `ls`, `ps`, `exec`
without the relay, `inbox` reads) builds and runs the same.

Limitations: [status.md](../../status.md#daemon)
