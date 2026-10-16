# Machines: what's left

Status: planned
Machines shipped through rollout step 5; remote forwards (step 6) and the
remote VS Code spike (step 7) were agreed and never started, the rest is
noted for later.

Today: `machine add|ls|rm`, the ssh link, the federated snapshot, verbs and
quick calls forwarded to a machine, names resolved across machines, and
`ssh -t` exec and terminal tabs
([machines](../../architecture/daemon/machines.md),
[client-server](../../architecture/daemon/client-server.md)). On a remote
name the CLI refuses `port`, `vscode` and `inbox`
(`MachineUse::Refused` in `src/serve/machines/cli.rs`); the dashboard
answers `o`/`p` on a remote row with a "not yet" line (`not_yet_code`,
`not_yet_port` in `src/tui/app/actions.rs`).

## 1. Remote forwards (rollout step 6)

- A forward to a remote instance binds on the local host and tunnels over
  the machine connection to the remote daemon's forwarder
  ([forwards](../../architecture/daemon/forwards.md)), so `port` and the
  Ports tab work on remote rows.
- `forwardPorts` of remote instances are **opt-in per machine**, so a
  remote box doesn't grab local ports by surprise.
- Open: how a tunnelled stream rides the machine link (today a held
  JSON-lines connection, quick calls one at a time) versus an ssh of its
  own per forward, like verbs.

## 2. VS Code on a remote machine (rollout step 7, spike)

`code` attaches VS Code to a local container. Candidates: Remote-SSH to the
machine plus attach-to-container through its docker context, or Remote-SSH
straight into the container through the helper. Low priority; nothing
depends on it.

## 3. Remote Inbox threads

The Inbox is a local store (`inbox::store`); the dashboard reads it
directly and `inbox.*` isn't in the forwarded quick-call list
(`forward::QUICK`). Remote threads need the Inbox behind the daemon on both
ends and a federated thread list, like the snapshot's `merge`.

## 4. Nameless commands across every machine

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
