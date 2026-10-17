# ssh-agent forwarding

How the host's ssh-agent reaches processes in a sandbox: the **relay** over
the helper bridge (the default) or, in builds without a helper, a **bind
mount**. Either way the container sees the agent at one fixed path,
`/run/devsandbox/ssh-agent.sock`. Read when an ssh/git call in a sandbox
can't find the agent, or before touching `SSH_AUTH_SOCK` handling.

Code: `src/commands/run/mod.rs` (`relay_first`), `src/commands/run/ssh_agent.rs`
(`SSH_AGENT_TARGET`, `ssh_agent_forward`, `ssh_agent_refresh`,
`MAGIC_SSH_AUTH_SOCK`), `src/devsbd.rs` (`relay_mode`, `boot_spec`),
`src/commands/exec.rs` (`has_host_agent`, `ssh_auth_sock_env`,
`exec_argv_with`, `AgentRelay`), `src/commands/run/lifecycle.rs`,
`src/devsbd/bridge.rs` (`host_agent`, `AgentCandidates`, `report_agent`,
`own_agent`), `devsbd/src/daemon.rs` (`AGENT_SOCK`, `accept_agent_clients`),
`src/serve/machines/ssh.rs`.

## Mode is decided at `run`

`relay_first = cfg!(unix) && devsbd::embedded()`.

- **Relay** (unix host, helper embedded): no mount. `Instance::ssh_auth_sock`
  stays `None`; `relay_mode(info)` is true once the helper is installed
  (`devsbd_arch` set).
- **Mount** (unix host, no helper embedded, e.g. `cargo install`):
  `ssh_agent_forward` binds an agent socket at `SSH_AGENT_TARGET` and records
  that target in `Instance::ssh_auth_sock`. Needs a runtime with file binds,
  `$SSH_AUTH_SOCK` set, and the socket present on the host. The source is a
  symlink `<data-dir>/agent/<instance_id>.sock` → `$SSH_AUTH_SOCK`, re-pointed
  by `start` (`ssh_agent_refresh`) because bind sources re-resolve on every
  container start. On macOS with `docker` the source is Docker Desktop's
  `/run/host-services/ssh-auth.sock` instead, bound directly (no link).

Off unix neither applies: no relay, and `ssh_agent_forward` mounts nothing.
The mode sticks to the instance: one created with a mount keeps it until
recreated.

## `SSH_AUTH_SOCK` is injected per exec

Never set on the container (`run -e`); added as `-e` on each exec by one rule,
`ssh_auth_sock_env`, shared by the CLI/TUI argv builder (`exec_argv_with`)
and lifecycle execs:

- mount mode: always the recorded target;
- relay mode: `SSH_AGENT_TARGET` only when the host has a live agent
  (`has_host_agent`), so an agent-less exec doesn't point ssh at a dead
  socket;
- otherwise unset.

`devsandbox exec` and the dashboard's terminal tabs (which spawn
`devsandbox exec`) get it; so do `run`'s lifecycle chain, `start`'s
`postStartCommand`, and the boot hook's commands (`boot_spec` records it).
A container's own processes not started through these execs don't.

## The relay

```
container process ─▶ /run/devsandbox/ssh-agent.sock ─▶ devsbd daemon ─Open(SSH_AGENT)─▶
   newest agent-capable bridge ─exec stdio─▶ serve daemon ─▶ host agent socket
```

- Container side: the daemon accepts on `ssh-agent.sock` (0666) and routes
  each connection to the newest bridge whose host advertised `SSH_AGENT`,
  holding it up to 1 s for one to appear (a bridge requested alongside a
  command may not be up yet). In mount mode the bind occupies that path and
  the daemon serves no agent.
- Host side: bridges belong to the `devsandbox serve` daemon
  ([bridges.md](bridges.md)). A bridge serves `SSH_AGENT` only while the host
  has a live agent; it is replaced when the agent appears or disappears.
- Which agent: chosen per stream by `host_agent()`: the newest live socket
  among those clients reported (`AgentCandidates`, up to 8), else the
  newest live machine agent relay socket in the socket dir
  (`agent-*.sock`, `endpoint::agent_sockets`;
  [machines.md](../daemon/machines.md#agent)), else the
  daemon's own `$SSH_AUTH_SOCK`. Every client that asks for a bridge
  (`bridges.ensure`) reports its own `$SSH_AUTH_SOCK`, so agent rotation (a
  new `ssh -A` session, re-login) needs no restart: the next command brings
  the live socket. The installed service unit deliberately doesn't capture
  `SSH_AUTH_SOCK` for the same reason.

### Waiting for the bridge

- `exec`: asks for the bridge in the background, doesn't wait; the
  daemon's 1 s hold covers the race.
- `run`'s lifecycle chain, `start`'s `postStartCommand`: wait up to 12 s
  (`AgentRelay::ready`), since they may clone over ssh at once; not ready is
  one `note:`.
- A dispatcher's `ensure` waits for the child's bridge before answering
  (`wait_bridge` in `src/commands/dispatch.rs`).

## What works where

| host / runtime | relay (default builds) | mount (no helper) |
|---|---|---|
| Linux, docker or podman | yes | symlink bind |
| macOS, docker (Docker Desktop, OrbStack) | yes | binds `/run/host-services/ssh-auth.sock` |
| macOS, podman | yes | attempted symlink bind; the host socket can't cross into the podman machine VM |
| Apple `container` | yes | no (`supports_file_binds` false) |
| Windows native | no (relay is unix-only) | no (`ssh_agent_forward` is unix-only) |
| remote machine | yes, on the machine: every ssh to it passes `-A`, and verbs forward the caller's `SSH_AUTH_SOCK` | n/a |

VS Code terminals are not devsandbox's: VS Code forwards its own agent
through its server, independently of both modes.

Limitations: [status.md](../../status.md#helper)
