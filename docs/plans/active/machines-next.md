# Machines: what's left

Status: planned
Machines shipped through rollout step 5, remote forwards (step 6) after
it; the remote VS Code
spike (step 7) was agreed and never started, the rest is noted for later.
A machine's `forwardPorts` are deliberately not mirrored here: forwards
into a machine are always asked for from this side.

Today: `machine add|ls|rm`, the ssh link, the federated snapshot, verbs and
quick calls forwarded to a machine, names resolved across machines,
`ssh -t` exec and terminal tabs, and port forwards bound here
([machines](../../architecture/daemon/machines.md),
[client-server](../../architecture/daemon/client-server.md),
[forwards](../../architecture/daemon/forwards.md#remote-bridges)). On a
remote name the CLI refuses `vscode` and `inbox` (`MachineUse::Refused` in
`src/serve/machines/cli.rs`); the dashboard answers `o` on a remote row
with a "not yet" line (`not_yet_code` in `src/tui/app/actions.rs`).

Items 1 and 2 are being built: [machines-agent](machines-agent.md).

## 1. A noisy login shell doesn't break the link

The remote command runs through the login shell (`ssh::remote_command`), so
anything `.profile`/`.bashrc` prints to stdout lands before the relay's
first byte, and `client::hello` (byte-by-byte read of one reply line) fails
on it: the machine is `unreachable` with a JSON parse error. Remote
forwards already cope (`commands::tunnel::read_header` skips lines before
its magic); the API link should too.

- `devsandbox api --stdio --session-env` (`serve::relay::stdio`; only a
  machine link passes `--session-env`, so plain API clients are unchanged)
  writes `\n` + a sentinel line (e.g. `devsandbox-api 1\n`) to stdout
  before relaying. The leading newline ends a motd without a final one.
- The machine side of the hello (`machines::link`, used by the held link,
  forwarded verbs and `machines.probe|add`) reads lines until the sentinel,
  bounded (64 KiB, like `tunnel::HEADER_LIMIT`), then does the usual
  `client::hello` on the rest. Factor the line skipping out of
  `tunnel::read_header` and share it.
- The noise itself is dropped, except into `serve.log` (first line,
  truncated) so a broken profile can be found.
- `docs/status.md`'s *A remote login shell that prints to stdout breaks
  the hello* goes; machines.md *Link handshake* describes the sentinel.
- Tests: `link.rs` fake peers with noise before the sentinel, noise
  without a newline, no sentinel (old remote), noise past the bound.

## 2. Agent forwarding is opt-in per machine, switchable live

Today every ssh but a forward's tunnel passes `-A` (`ssh::api_argv`,
`cli_argv`, `tty_argv`), and the held link keeps the agent forwarded
while anything wants the machine. Root on the machine, and likely its
sandboxes (the relay's `--session-env` hands verbs the forwarded
`SSH_AUTH_SOCK`; verify how far it reaches the remote daemon's
in-container relay), can sign with your keys meanwhile. Sandboxes there
should be able to use git like local ones, but only when you say so.

Goal: `agent = false` by default in `machines.toml`; when on, git works on
the machine and in its sandboxes as it does locally; turning it off takes
effect at once, without reconnecting.

Design sketch: stop using ssh's `-A` entirely and carry agent streams over
our own infra, where the local daemon decides per stream.

- A hidden `devsandbox agent-relay` (like `tunnel`) on the machine,
  spawned over its own ssh (no `-A`) by the local daemon's `Machines`
  service while the machine is wanted and `agent` is on. Its stdio carries
  the helper's frame protocol (`src/devsbd/mux.rs`), the remote side
  opening `Open { channel: SSH_AGENT }` streams like an in-container
  daemon does.
- On the machine it serves a unix socket (`<runtime-dir>/devsandbox/
  agent-<client>.sock`, 0600) and registers it with the remote daemon as
  an agent candidate (`bridge::report_agent`, as `bridges.ensure` does), so
  host git there and the remote daemon's in-container relay use it. The
  relay's `--session-env` sets verbs' `SSH_AUTH_SOCK` to it (or removes
  it when off).
- Locally each `Open` is checked against the machine's current `agent`
  setting, then connected to `bridge::host_agent()`. Off: refused, and the
  relay ssh is closed; the remote socket then fails fast (no agent rather
  than a hang), and the candidate is dropped there.
- `devsandbox machine add --agent` and a new `machine set <name> --agent
  on|off` (`machines.set`, registry write by the daemon like `add`/`rm`);
  `machine ls` and the dashboard header show it. Re-`add` of an existing
  name stays refused.
- Drop `-A` from `api_argv`, `cli_argv`, `tty_argv`. Terminal tabs and CLI
  re-runs are `devsandbox exec`/… into containers, which get the agent
  from the remote daemon's relay, i.e. this one.
- Open: several laptops linked to one machine (one socket each; whose
  agent a sandbox gets: the newest, as `AgentCandidates` does today);
  `ssh-add -c`-style confirmation is the agent's business, not ours.
- Docs: machines.md (*ssh*, a new *Agent* section), the helper's
  ssh-agent.md for the remote leg, api.md (`machines.set`), the CLI
  reference/skill.

## 3. VS Code on a remote machine (rollout step 7, spike)

`code` attaches VS Code to a local container. Candidates: Remote-SSH to the
machine plus attach-to-container through its docker context, or Remote-SSH
straight into the container through the helper. Low priority; nothing
depends on it.

## 4. Remote Inbox threads

The Inbox is a local store (`inbox::store`); the dashboard reads it
directly and `inbox.*` isn't in the forwarded quick-call list
(`forward::QUICK`). Remote threads need the Inbox behind the daemon on both
ends and a federated thread list, like the snapshot's `merge`.

## 5. Nameless commands across every machine

`ps`, `stats`, `status`, `ls`, `gc`, `stop --all`, … without `--machine`
apply to this host and every linked machine, merged through the daemon
with a machine column. Today they're local unless `--machine` is given
(`Plan::Here` in `src/serve/machines/cli.rs`).

## Parked

- **Default machine**: making `--machine` implicit; an open question owned
  by [windows-support](windows-support.md).
- **Auto-update a machine** to the newest devsandbox linked to it. Sketch:
  at the hello, if the local binary is newer and the platform matches,
  push it (scp to the remote's `command` path, atomically) and reconnect;
  the remote's version handoff restarts its daemon. Open: platform/arch
  mismatch (download the release instead), dev builds, a remote shared by
  several laptops (newest wins), opting out. Today a protocol mismatch
  marks the machine unreachable, a semver difference is a warning.
- **PTY attach over the protocol** (an exec stream with resize), for
  clients that can't spawn ssh.
- **Following a build log remotely** (`instances.follow`); today only
  `instances.buildLog`, a tail.
