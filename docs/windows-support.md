# Windows support

Status: plan, not started.

`devsandbox serve` is unix-only (`docs/serve.md`), and once the verbs run in
the daemon (`docs/client-server.md`) the CLI has nothing to run them
locally on Windows. The plan: **Windows is a remote-only client**. The CLI
and the TUI talk to a machine (`devsandbox machine add ssh://…`), a Linux
box, WSL or a VM, that runs the daemon and the containers.

## What has to work on Windows

- **The CLI as a client**: argument parsing, `resolve`, sending requests,
  streaming `progress`/`output`, Ctrl-C as cancel.
- **The machine connection**: spawning `ssh <target> devsandbox api
  --stdio` with the OpenSSH client that ships with Windows. Without a local
  daemon, the client holds this connection itself instead of a local daemon
  federating it.
- **A default machine**, so `--machine` isn't needed on every command (see
  the open question in `docs/client-server.md`).
- **The TUI**: ratatui/crossterm already support Windows consoles. The
  terminal tabs need `ssh -t` (*Exec and terminal* in the client-server
  doc) instead of a local `docker exec -it` PTY.
- **`exec`**: `ssh -t <machine> devsandbox exec …`.
- **ssh-agent**: Windows' OpenSSH agent is a named pipe. `ssh -A` forwards
  it, so the remote side doesn't change.

## What doesn't

- **Port forwards**: they need a local forwarder (`src/devsbd/forward.rs`,
  unix-only) or plain `ssh -L` per port. Pick one. `ssh -L` is the cheap
  first cut.
- **VS Code `code`**: depends on the remote VS Code spike.
- **Desktop notifications** (`src/desktop.rs`): none on Windows to start with.
- **Local runtime on Windows** (Docker Desktop directly): out of scope.
  WSL as a machine covers it.

## Open questions

- Without a local daemon there's no federation: one machine at a time on
  Windows, or does the client federate in-process?
- Does WSL get first-class treatment (`machine add wsl://<distro>`, no ssh)?
- CI: build and smoke-test the Windows client against a Linux machine over ssh.
