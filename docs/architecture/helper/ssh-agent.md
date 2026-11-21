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
`src/serve/fleet/agent.rs` (a machine's and a fleet worker's relayed agent),
`src/devsbd/agent_filter.rs` (the key filter under a git-host claim).

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
  A mount hands over the whole agent, so an instance with a git-host claim
  gets none: `run` drops it with a warning (`ssh_agent::mount_refusal`)
  and the instance has no agent.

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
A tty exec's command runs in a helper session (`devsbd session new`,
[in-container.md](in-container.md)), whose supervisor takes its env from
the exec that created it: the command keeps that exec's `SSH_AUTH_SOCK`
(or its absence) for good, and a later `exec --attach` changes neither.
Both hold an `AgentRelay` while they run, as any `exec` does.

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
  newest live relayed agent socket in the socket dir
  (`agent-*.sock`, `endpoint::agent_sockets`: on a machine a granting
  host's `agent-link-<host>.sock` or `agent-route-<host>.sock`,
  [machines.md](../daemon/machines.md#agent);
  on a fleet worker its link's `agent-fleet-<control-plane>.sock`, bound only while the control plane serves its agent, [fleet.md](../daemon/fleet.md#agent)), else the
  daemon's own `$SSH_AUTH_SOCK`. Every client that asks for a bridge
  (`bridges.ensure`) reports its own `$SSH_AUTH_SOCK`, as does every verb
  forwarded to a machine (`machines::forward::verb`), so agent rotation (a
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

## Key filtering

While a git-host claim applies (a claim whose `git.hosts` names hosts,
recorded as `InstanceClaim::git_hosts`), the relay filters the agent, or
anything in the sandbox could `ssh git@github.com git-receive-pack …` with
the user's key past the gateway's repo fence. The daemon's bridge for such
an instance holds an `agent_filter::Gate` (`Gate::for_instance`); each agent
stream then runs through `agent_filter::relay` on its own thread instead of
being spliced to the agent socket (`open_stream`). Forward bridges of such
an instance serve no agent (`forward::Route::agent`), so every agent
stream goes through the daemon's.

Per connection, the gate reads the sandbox's `ssh-keys` from its config
(like the broker's `access`: an edit applies to the next connection, no
rebuild; a config that doesn't load offers no keys) and runs a fresh
`agent_filter::Filter`:

- `REQUEST_IDENTITIES`: the agent's answer keeps only the named keys
  (`KeyRef::matches`: the `SHA256:` fingerprint or the public key line's
  blob, exact; a certificate is its own key). None by default.
- `SIGN_REQUEST` with an unnamed key: refused.
- A named key without `destinations` signs anything, as without a claim:
  it reopens GitHub over ssh (the config reference says so).
- A named key with `destinations` signs only a publickey
  `SSH_MSG_USERAUTH_REQUEST` (`publickey` or
  `publickey-hostbound-v00@openssh.com`) that names that key and carries
  the session id of the connection's last `session-bind@openssh.com`,
  which must be a login bind (not forwarding) to a host key that one of
  the destinations has in this host's `known_hosts` (OpenSSH's
  destination constraints, `ssh-add -h`). Intermediate forwarding hops
  aren't checked: the login's own bind is. A client without session-bind
  (OpenSSH before 8.9, Go's x/crypto/ssh, libgit2) loses named keys only.
- A `signing-only` key signs only an SSHSIG blob (the `SSHSIG` preamble,
  `ssh-keygen -Y sign`) in namespace `git`: ssh commit signing, never a
  login. The same key can be named twice, once per use.
- `session-bind@openssh.com`: verified before it counts
  (`agent_filter::verify`: the host key's signature over the session id;
  ed25519, ecdsa nistp256/384, rsa-sha2-256/512; nistp521, `ssh-rsa`'s
  SHA-1, certificate and security-key host keys are refused, naming the
  type), then passed on to the agent. As in OpenSSH's agent, a connection
  bound for a login takes no further bind, and at most 16.
- Anything else (adding, removing or locking keys, other extensions):
  refused.

A destination's host keys come from the filtering host's own `known_hosts`
(`agent_filter::SshKeygen`: `ssh-keygen -F <host> -f <file>` over
`~/.ssh/known_hosts` and `/etc/ssh/ssh_known_hosts`, run per connection so
a rotation is followed; `@revoked` keys dropped, `@cert-authority` lines
skipped), never the container's, where the sandbox could plant its own key
under the destination's name. A destination with no entry refuses, naming
the host and that it must be in `known_hosts`.

A refusal answers `SSH_AGENT_FAILURE`, which the client prints as "agent
refused operation", so its reason goes to `serve.log` and to the
instance's Inbox as a warning keyed `ssh-agent` (`refusal_report` in
`src/devsbd/bridge.rs`).

The filter sits where the relay ends, on the instance's host: a remote
instance's agent arrives there over `AGENT` streams and is filtered by
that host's bridge, against that host's `known_hosts` and config.

VS Code's attach brings its own credential helper, agent and askpass, so
`devsandbox vscode` refuses an instance with any credential claim unless
`--allow-credential-leak` (then warns), and the dashboard's `o` refuses it
(`commands::vscode::leak_refusal`).

## What works where

| host / runtime | relay (default builds) | mount (no helper) |
|---|---|---|
| Linux, docker or podman | yes | symlink bind |
| macOS, docker (Docker Desktop, OrbStack) | yes | binds `/run/host-services/ssh-auth.sock` |
| macOS, podman | yes | attempted symlink bind; the host socket can't cross into the podman machine VM |
| Apple `container` | yes | no (`supports_file_binds` false) |
| Windows native | no (relay is unix-only) | no (`ssh_agent_forward` is unix-only) |
| remote machine | while this host grants it agent (`member.toml`'s `[[agent]]`, by its member key): the local daemon answers the `AGENT` streams the machine opens (over its link, or routed) with it, as `agent-link-<host>.sock` or `agent-route-<host>.sock` there ([machines.md](../daemon/machines.md#agent)); no ssh to a machine passes `-A` (a CLI re-run reports its terminal's agent through `machines.get`); with no grant nothing of this host's agent reaches it; an offered fleet worker gets it over its fleet link's `AGENT` streams instead ([fleet.md](../daemon/fleet.md#agent)) | n/a |

VS Code terminals are not devsandbox's: VS Code forwards its own agent
through its server, independently of both modes.

Limitations: [status.md](../../status.md#helper)
