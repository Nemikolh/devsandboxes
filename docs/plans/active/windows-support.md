# Windows support

Status: planned
Direction agreed (Windows as a remote-only client of a machine), nothing of
it built: off unix there is no `serve`, `api` or `machine` subcommand and
`--machine` is refused.

Today the release workflow ships an `x86_64-pc-windows-msvc` binary that
runs config, `ls`, `ps`, `exec` and the verbs in its own process against a
local runtime; no dashboard, `-d`, `port`, ssh-agent relay or machines
([platform-support](../../architecture/daemon/platform-support.md)).
Machines over ssh exist on unix
([machines](../../architecture/daemon/machines.md)); WSL2 running the Linux
build is the supported path, with the Windows agent bridged in by `socat` +
`npiperelay.exe` ([ssh-agent](../../architecture/helper/ssh-agent.md)).

## Plan

**Windows is a remote-only client.** The CLI and the TUI talk to a machine
(a Linux box, WSL or a VM, added with `devsandbox machine add ssh://…`)
that runs the daemon and the containers. Native Docker Desktop as the
runtime stays unsupported territory: WSL as a machine covers it.

## What has to work

- **CLI as a client**: argument parsing, `resolve`, requests, streaming
  `progress`/`output`, Ctrl-C as cancel; the client half of
  `src/serve/machines/` compiled off unix.
- **The machine connection**: `ssh <target> devsandbox api --stdio` with
  Windows' bundled OpenSSH. With no local daemon, the client holds the
  connection itself instead of a daemon federating it.
- **A default machine**, so `--machine` isn't needed on every command.
- **The TUI**: gated to unix in `src/main.rs`; it comes back once it can
  reach a machine (ratatui/crossterm support Windows consoles), with
  terminal tabs over `ssh -t` (as remote rows already do,
  `src/tui/remote.rs`) instead of a local PTY exec.
- **`exec`**: `ssh -t <machine> devsandbox exec …`.
- **ssh-agent**: Windows' agent is a named pipe; `ssh -A` forwards it, so
  the remote side doesn't change.

## What doesn't (first cut)

- **Port forwards**: need a local forwarder (`src/devsbd/forward.rs`,
  unix-only) or `ssh -L` per port. `ssh -L` is the cheap first cut; on
  unix, forwards into a machine run through that same local forwarder
  ([forwards](../../architecture/daemon/forwards.md#remote-bridges)).
- **VS Code `code`**: waits for the remote VS Code spike
  ([machines-next](machines-next.md)).
- **Desktop notifications** (`src/desktop.rs`): none on Windows.

## Later: native ssh-agent relay

Only if the local, in-process path should keep working on Windows. The
container side is the built `devsbd` relay; Windows lacks the host side: a
named-pipe client (`\\.\pipe\openssh-ssh-agent`, needs
`tokio::net::windows::named_pipe` or a winapi crate, neither a dependency)
feeding a bridge's stdio, since `host_agent()` reads a unix
`$SSH_AUTH_SOCK`. Bridges are owned by `serve`, so this also means a
Windows daemon: `src/serve/endpoint.rs` (`Listener`, `connect`, `Stream`)
is the named-pipe seam, `src/devsbd/mux.rs` notes the stream it needs.
Symlink-based mounting is no answer: a pipe can't be bind-mounted as a unix
socket, and Docker Desktop's magic socket doesn't forward on the WSL2
backend.

## Open questions

- Without a local daemon there's no federation: one machine at a time, or
  does the client federate in-process?
- Default machine: configured where, and does it make `--machine`
  implicit only off unix, or everywhere?
- WSL first-class (`machine add wsl://<distro>`, no ssh)?
- What happens to the existing in-process local verbs on Windows: keep
  them as a fallback, or refuse without a machine?
- CI: build and smoke-test the Windows client against a Linux machine
  over ssh.
