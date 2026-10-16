# Architecture overview

What devsandbox is made of and how the parts fit. Read this first, then
follow the link for the area you're touching. Each section is a summary;
the linked docs are the reference. What doesn't work yet is in
[status.md](../status.md), and why things are the way they are is in
[decisions/](../decisions/).

devsandbox is a Rust CLI that runs devcontainer-style sandboxes on docker,
podman or Apple `container`. A config root's `devsandboxes.toml` declares
sandboxes; each sandbox can spawn many **instances** (containers named
`devsandbox-<instance>`), and repeat instances of one repo get their own git
worktree.

```
 CLI ─┐                                         ┌─ machines (ssh → their daemon)
 TUI ─┼─ unix socket ─► devsandbox serve ───────┤
 npm ─┘   (JSON lines)  verbs, poll, forwards,  └─ runtime (docker | podman | container)
                        bridges, Inbox, API                    │
                              │ exec -i … devsbd bridge         ▼
                              └────────────────────► devsbd in each instance
```

## Principles

- Files are the truth, the daemon is replaceable: liveness comes from the
  runtime, host state is files, stopping the daemon loses nothing.
  [0001](../decisions/0001-files-are-the-truth-the-daemon-is-replaceable.md)
- Config is the security boundary: mount sources come only from config.
  [0002](../decisions/0002-config-is-the-security-boundary.md)
- Host git never runs repo-controlled code.
  [0003](../decisions/0003-host-git-never-runs-repo-controlled-code.md)
- Two instances never share a working tree.
  [0004](../decisions/0004-two-instances-never-share-a-working-tree.md)

## Config

`[template.*]`, `[sandbox.*]` and `[services.*]` tables; `extends`
deep-merges (tables recurse, arrays concatenate). The merged table is
hashed for drift detection. Internals: [config.md](config.md). User-facing
field reference: [config-toml-spec](../../skills/config-toml-spec/SKILL.md).

## Instances

`run` always creates an instance (`materialize`), `start` restarts one,
`rebuild` recreates a drifted container and keeps the worktree and state.
Images are the base image or build plus a derived feature image. Services
are per-instance (`isolated`, the default) or shared per root (`global`).

- [lifecycle.md](instances/lifecycle.md): `run`, the instance record,
  `start`/`stop`/`rename`/`rm`.
- [worktrees.md](instances/worktrees.md): repeat instances, `folders`
  worktrees, `.worktreeinclude`/`worktree-link`, locking.
- [rebuild-and-drift.md](instances/rebuild-and-drift.md): `config_hash`,
  `build_hash`, `drift_decision`, `rebuild`, `service rebuild`.
- [services.md](instances/services.md): scopes, networks, `gc`.
- [images-and-features.md](instances/images-and-features.md): image builds,
  devcontainer features, entrypoint chain.
- [mounts-and-state.md](instances/mounts-and-state.md): mounts, caches,
  host-side data files, VS Code integration, the host git wrapper.

## Runtimes

One `Backend` trait over docker, podman and Apple `container`; runtime
differences are capabilities, not branches at call sites.
[runtimes.md](runtimes.md)

## The daemon

`devsandbox serve`, one per user and host, unix-only, started on demand.
It runs the verbs (`run`, `rebuild`, `start`, `stop`, `rm`,
`service rebuild`) as subprocess requests, owns the one container poll,
the bridges to every helper, the port forwards, the API and the links to
other machines.

- [role.md](daemon/role.md): what it owns, start, idle, version handoff,
  install. Start here.
- [client-server.md](daemon/client-server.md): how clients connect, the
  in-process fallback.
- [verbs.md](daemon/verbs.md): verbs as requests, cancel, adoption.
- [host-poll.md](daemon/host-poll.md): the shared poll and snapshot pushes.
- [forwards.md](daemon/forwards.md): port forwarding.
- [machines.md](daemon/machines.md): remote hosts over ssh.
- [platform-support.md](daemon/platform-support.md): what runs off unix.
- API reference: [reference/api.md](../reference/api.md).

## The helper (`devsbd`)

A static Linux binary embedded in `devsandbox` and copied into every
instance: the container half of the ssh-agent relay, notifications,
dispatcher control requests, port forwards, tracked runs and the restart
boot hook. The daemon keeps one bridge (`exec -i … devsbd bridge`) per
running instance.

- [overview.md](helper/overview.md): build, embed, install. Start here.
- [protocol.md](helper/protocol.md): frames, mux, line codecs.
- [bridges.md](helper/bridges.md): the host side.
- [in-container.md](helper/in-container.md): `devsbd daemon`, boot hook, verbs.
- [ssh-agent.md](helper/ssh-agent.md): agent forwarding.

## The Inbox

A host-wide store (`inbox.json`) of threads, keyed by owner instance and
key, that instances write into (`devsbd thread …`, `devsbd notify`) and the
user answers from the dashboard, the CLI or the API.

- [model.md](inbox/model.md): threads, mutations, caps, the store.
- [threads-and-feed.md](inbox/threads-and-feed.md): the owner's side.
- [forms.md](inbox/forms.md): forms.
- [views-and-wire.md](inbox/views-and-wire.md): the user's side, wire views,
  `decide_*` checks.
- CLI reference: [reference/inbox-cli.md](../reference/inbox-cli.md).

## Automations

Sandboxes that start themselves after a boot (`autostart`) and dispatcher
instances that create and drive child instances through the helper's
control channel, authorized against the current config on every request.

- [autostart.md](automations/autostart.md)
- [dispatchers.md](automations/dispatchers.md)

## The dashboard

`devsandbox` with no subcommand on a TTY: a ratatui client of the daemon.
It renders the daemon's snapshot pushes and sends every action as an API
request. Its state machine (`App`) does no I/O, so it's unit-tested.

- [overview.md](dashboard/overview.md): threads, what's local. Start here.
- [tabs-and-actions.md](dashboard/tabs-and-actions.md)
- [command-line.md](dashboard/command-line.md): the `:` prompt grammar.
- [terminal.md](dashboard/terminal.md): integrated terminal, kitty keys.
- [selection.md](dashboard/selection.md): mouse selection, OSC 52.
- [inbox-tab.md](dashboard/inbox-tab.md)
- [snapshot-and-json.md](dashboard/snapshot-and-json.md): the `Snapshot`
  model and every `--json` output.

## Repository layout

```
src/
  main.rs        CLI (clap); bare TTY invocation → dashboard
  config.rs      devsandboxes.toml model, extends, hashing
  features.rs    devcontainer features
  state.rs       host-side instance store
  snapshot.rs    rows shared by --json and the dashboard
  commands/      one file per verb
  serve/         the daemon; machines/
  inbox/         the Inbox store and wire views
  runtime/       Backend: docker/podman, Apple container
  devsbd*.rs     embedded helper blobs, shared protocol files
  tui/           the dashboard
devsbd/          the in-container helper crate
test-utils/      docker_test / helper_test proc macros
site/            docs site (Astro), user-facing
skills/          config-toml-spec: user-facing config reference
```

AGENTS.md has the module-by-module map.
