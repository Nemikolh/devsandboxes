---
name: devsandbox-cli
description: |
  Use the devsandbox CLI to create, run, exec into, and tear down devcontainer-based sandboxes from a devsandboxes.toml file.
  Covers every subcommand, ordered by how useful each is to an agent, plus a devsandboxes.toml overview (full format: the `config-toml-spec` skill).
---

# devsandbox CLI

`devsandbox` manages devcontainer-style sandboxes directly on a container runtime
(docker / podman / Apple `container`) — no devcontainer CLI, no daemon to set
up (its own host daemon starts on demand, see below). One
`devsandboxes.toml`, outside of any project, holds every sandbox of a
workspace: add a sandbox to it rather than a new config per project (separate
configs split workspaces, e.g. work vs personal).

It defines templates, sandboxes, and shared services; the CLI derives containers, networks,
mounts, caches, and VS Code wiring from it.

Everything it creates is name-prefixed `devsandbox-`, so it is discoverable with a
plain `docker ps`. Docker is the source of truth for liveness; host state is a
single TOML file (see [State](#state)).

## Install

Binary name is `devsandbox`. From the repo root:

```bash
cargo install --path .        # puts `devsandbox` on PATH (~/.cargo/bin)
# or, without installing:
cargo build --release         # -> target/release/devsandbox
```

Requires a container runtime on PATH and `git` (worktrees) and `curl` (fetching
`features`). Select the runtime with `DEVSANDBOX_RUNTIME=docker|podman|container`;
default is `container` on macOS, `docker` elsewhere. An unknown value warns and
falls back to the default.

`-C <dir>` (global flag) points any command at a different config root; default is
the current directory.

`run`, `rebuild`, `start`, `stop` and `rm` do their work in the per-user host
daemon (`devsandbox serve`, started on demand; Linux and macOS) with the
calling shell's environment, and stream their output and exit code back as if
they ran in place. Killing the terminal doesn't stop them; Ctrl-C still
cancels (exit 130). With no daemon reachable they print one `warning:` line and
run in-process. `-d` / `--detach` on `run`, `rebuild` and `start` returns once
the instance is registered, printing only its name (`run -d --json`: the run
record); follow the rest with `logs --build <name> --follow`.

## devsandboxes.toml

TOML, not JSON (`devsandboxes.yaml` / `.yml` work too, same schema). Three top-level table families:

- `[template.<name>]` — reusable property sets, referenced via `extends`.
- `[sandbox.<name>]` — a devcontainer definition plus devsandbox extras.
- `[services.<name>]` — sidecar containers (databases, etc.).

Sandbox/template keys use devcontainer **camelCase** (`postCreateCommand`, `containerEnv`, …);
devsandbox extras (`folder`, `services`, `caches`, `persist-shell-history`, `shell-rc`, …) are
additions on top. Unknown keys are a **hard error**; valid devcontainer properties that aren't
implemented parse but warn at `run`. `extends` deep-merges templates: tables recurse, **arrays
concatenate**, scalars are replaced.

**For the full format** — every key, merge rules, `${…}` variables, services, what's implemented —
use the `config-toml-spec` skill.

### Minimal example

```toml
[services.database]
image = "postgres:16"
scope = "global"

[template.base]
caches = ["pnpm"]
persist-shell-history = true
[template.base.features]
"ghcr.io/devcontainers/features/node:1" = { version = "lts" }

[sandbox.web]
extends = "base"
folder = "../web"
services = ["database"]
image = "mcr.microsoft.com/devcontainers/base:ubuntu"
postCreateCommand = "pnpm install"
customizations.vscode.extensions = ["dbaeumer.vscode-eslint"]
```

## Commands (most to least useful for an agent)

`<name>` accepts an **instance name**, a **sandbox config name**, or a **repository folder basename**. An exact
instance name (or instance id) always wins: with instances `web` and `web-2`, `web` is the instance `web`. Only when
nothing matches exactly do sandbox/folder names count, and ambiguity there errors on a non-TTY (prompts on a TTY). Most commands are non-interactive-safe; anything needing a choice bails with the options
listed when stdin is not a TTY. With [machines](#machines) registered, a bare `<name>` is also looked up on every
machine; `<machine>/<name>` and `@/<name>` (this host) qualify it.

1. **`exec [-i] [-t] <name> [cmd…]`** — run a command inside the instance. Runs in
   the workspace dir and honors the recorded `remoteEnv` / `remoteUser`. Exits
   with the command's own status. This is the agent's main way to do work inside a
   sandbox. `--` is only needed when the command's first word is a devsandbox
   flag (`-i`, `-t`, `-h`/`--help`, `-C`/`--dir`): those are parsed as flags
   anywhere before the command (`exec web -- -t`). Non-interactive commands
   need neither `-i` nor `-t`.

   Without `cmd` it opens a login shell (zsh, else bash, else sh). With neither
   flag given that shell gets `-i`, plus `-t` when stdin is a TTY; any explicit
   flag is used as given instead.

   ```bash
   devsandbox exec web pnpm test
   devsandbox exec web          # interactive login shell on a TTY
   ```

2. **`run [sandbox] [--name <n>] [-d]`** — create/start an instance in the **background**.
   Prints the instance name on stdout, but child output (image builds,
   `docker run`, git, hooks) may precede it and an autostart pass may follow
   it with its own lines. **`--json`** sends all of
   that to stderr and prints only
   `{"schema":1,"data":{"name","instance_id","sandbox","container","workspace","folder","base_folder","worktree","branch"}}`
   (`worktree`/`branch` are `null` without a worktree) — prefer it when scripting.
   On a non-TTY the sandbox arg is **required** (it lists available names on
   error). Behavior worth knowing:

   - Never reuses an instance: a taken name is an error (`start <name>` restarts
     a stopped instance, `rebuild <name>` recreates a drifted one).

   - A **second `run` against a folder already live** creates a git **worktree**
     under `.worktrees/<instance>` on a fresh `sandbox/<instance>` branch, and a
     `<sandbox>-<n>` instance name — the two containers never share a checkout.
     `--branch <name>` naming an existing branch checks it out instead (a local
     one as is, a remote-only one as a local branch tracking `origin/<name>`);
     a branch checked out elsewhere is an error naming where.

   - **Config drift**: reusing a container whose recorded config hash differs from
     the current config only _warns_; `rebuild <name>` applies the changes without
     losing the worktree.

   - A failed lifecycle command aborts `run` but **keeps the container** so you can
     `exec` in to debug.

3. **`logs <name> [-n N]`** — last N lines (default 50) of the container's
   stdout+stderr, verbatim; prints `(no log output)` when empty. `<name>` also
   accepts a raw devsandbox container name (with or without the `devsandbox-`
   prefix), so service containers work: `devsandbox logs devsandbox-svc-<proj>-database`.
   First stop when a lifecycle command or service misbehaves.

   **`logs --build <name> [-f|--follow]`** prints the instance's build log
   instead: everything its last `run`/`rebuild`/`start` printed (image build,
   lifecycle commands, git, the final error), from
   `<data>/devsandbox/logs/<instance_id>/build.log` (the one before is
   `build.1.log`; `rm` deletes both, as does a rolled-back `run`). `--follow`
   keeps printing while the instance is still being built and exits once it's
   up, failed, or gone. Only the daemon's own autostart pass writes no build log.
   A build that ran on a terminal logged one's output (colours, docker's
   redrawn progress): a terminal gets it as is; piped, it's plain text, the
   redraws collapsed to their final state (with `--follow`, escapes stripped
   as they come, so redrawn lines repeat).

4. **`inspect <name>`** — pretty-printed runtime `inspect` JSON (config, mounts,
   networks, env, exit code). Same name resolution as `logs`, service containers
   included. Keys are colour-highlighted on a TTY only; piped output is plain
   JSON, safe to parse.

5. **`ps [-a]`** — list instances (joined from state + runtime). `-a` includes
   stopped ones. Columns: NAME, IMAGE, STATUS.

6. **`stats`** — one-shot CPU/memory usage of every running `devsandbox-*`
   container (instances and services). Columns: NAME, CPU, MEM.

7. **`ls`** — list sandbox _configs_ from `devsandboxes.toml` (NAME, SOURCE, FOLDER,
   SERVICES, EXTENDS). Use to discover what can be `run` and to catch config
   parse/validation errors.

8. **`stop <name>`** — stop the instance container + its isolated service
   containers. Idempotent (already-stopped/missing is fine). State survives; a
   later `run` restarts it.

9. **`rebuild <name>`** / **`rebuild --all`** (alias `recreate`; `-d` as for `run`) — recreate the
   instance's container from the current config when it has drifted; the
   worktree, branch, instance name, and per-instance state (shell history,
   `${instance}` mounts) are kept and the lifecycle commands re-run. No drift →
   no-op (safe to run speculatively); `--force` recreates anyway (picks up
   devsandbox-side behavior changes the drift hashes cannot see). `--all`
   rebuilds every drifted instance (every instance with `--force`), skipping
   ones from other config roots. The fix for the config-drift warning.

10. **`rm <name> [--delete-branch | --keep-branch] [--force]`** — remove container, its
   worktree, isolated services, per-instance network, and the state entry.
   Managed shell history is **kept** so a rebuilt instance inherits it. Global
   services are left for `gc`. A worktree with uncommitted or untracked changes
   makes `rm` refuse up front, removing nothing: commit, stash or discard them
   first, or pass `--force` (`-f`) to discard them. `--force` also warns and
   moves on when a teardown step fails, so the instance always leaves state. Branch handling applies to the worktree's current branch (the
   recorded one when detached), never the remote's default branch:
   `--delete-branch` deletes it (`git branch -D`, unmerged commits too),
   `--keep-branch` keeps it. With neither, `rm` runs `git fetch --prune origin`,
   then deletes it without asking when all its commits are on its remote branch
   or its upstream is gone (merged PR). Otherwise it prompts on a TTY and keeps it
   off one. The recorded branch is deleted along with it when it is an ancestor.
   Scripts that need a local-only branch gone must pass `--delete-branch`.

11. **`gc [--force]`** — reap shared services no live instance references, orphaned
   networks, and orphaned shell-history files (`--force` skips the per-file
   confirm). Housekeeping, not part of a normal task loop.

12. **`devsandbox`** (no subcommand) — opens the interactive **TUI dashboard** on a
    TTY; prints help and exits `2` otherwise. **Not for agents** — it takes over
    the terminal.

All commands colour output only when stdout is a TTY and `NO_COLOR` is unset;
piped/captured output is always plain text.

## Machines

Sandboxes on other hosts, over ssh (Linux and macOS; the remote needs
devsandbox on its login shell's `PATH`). The local host daemon keeps the list
and talks to each machine's daemon; nothing else changes on the machine.

```bash
devsandbox machine add me@my-server            # probe its config roots; pick on a TTY
devsandbox machine add ssh://me@box:2200 --name box --config '~/sandboxes'
devsandbox machine add me@my-server --agent    # with `agent` on
devsandbox machine ls [--json]                 # NAME TARGET ROOTS AGENT STATE VERSION
devsandbox machine set box --agent on|off      # change a machine's `agent`
devsandbox machine rm box                      # unregister only
devsandbox machine trust [worker|full]         # this host's fleet trust (show / set)
```

- `add` without `--config` offers the machine's known config roots (a
  numbered multi-select on a TTY); off a TTY it takes the only one, and zero
  or several is an error listing them: pass `--config <path on the machine>`
  (repeatable). Quote a `~` so your local shell doesn't expand it. The name
  defaults to the target's host; a name already registered is refused.
  Adding a machine means trusting it. `--agent` / `machine set <name>
  --agent on|off` (off by default, shown in `machine ls`) decide whether
  the local daemon relays your ssh-agent there: for `run`/`start`/… (e.g.
  git clones over ssh on the machine), for `exec`, `logs` and the other
  commands re-run on the machine over ssh, and for dashboard terminals;
  off takes effect at once. ssh to a machine never uses `ssh -A`. The
  daemon's ssh runs with `BatchMode=yes`: use a key in
  your agent (or a `ControlMaster` in `~/.ssh/config`).
- **Names resolve across machines.** `devsandbox run api` runs `api` where
  it's defined; `exec test`, `logs test`, `stop test` find the instance
  wherever it is. Ambiguous: a pick on a TTY, otherwise an error listing
  `@/test` and `box/test`; rerun with the qualified name. A remote `run`
  prints the instance qualified (`box/api`). Container names in `logs` /
  `inspect` stay local unless qualified.
- **`--machine <m>`** is only for commands without a name: `ps`, `stats`
  (once), `ls`, `status --json`, `gc`, `service ls` (once per root of `m`,
  each after a `==> m:<root>` line on stderr; `--json` with several roots is
  refused), `stop --all`, `start --all`, `rebuild --all`, `service rebuild
  <svc>`, `port --service <global-svc> <port>` (from `m`'s first root).
  Without it these are local only.
- `port box/api 3000` forwards a machine's instance port to this host (one
  ssh per port, until Ctrl-C), same specs and output as a local `port`.
  The dashboard's `p` on a machine's row does the same through the daemon
  (listed on its Ports tab).
- Not on machines yet: `vscode`, the Inbox. A machine needs the
  daemon: with none answering, a qualified name or `--machine` is an error.
  With no machine registered, nothing of this costs anything.

## State

- Instances: `~/.local/share/devsandbox/state.toml` (`$XDG_DATA_HOME` honored).
- Machines: `machines.toml` next to it (`[machine.<name>]`: `target`, `roots`,
  optional `command`, the remote devsandbox path, set by hand).
- Persistent per-config-root data (caches, shell history, `${sharedVolumes}`):
  `<config dir>/shared-volumes/`.
- Networks/containers are namespaced by a short hash of the config-root path, so
  two projects never collide.

## Browsers in a sandbox

When `DEVSANDBOX_BROWSER_SANDBOX=1` is set in an instance (`devsandbox exec
<instance> env`), Chromium's sandbox works there. Playwright turns it off by
default, so launch with `chromiumSandbox: true`, plus `executablePath:
process.env.DEVSANDBOX_CHROME` when that's set (instances whose execs run as
root: `/run/devsandbox/bin/chrome` starts the browser at a non-root uid):

```js
chromium.launch({ chromiumSandbox: !!process.env.DEVSANDBOX_BROWSER_SANDBOX, executablePath: process.env.DEVSANDBOX_CHROME })
```

Never add `--no-sandbox` by hand. Without `DEVSANDBOX_BROWSER_SANDBOX`,
keep the tool's defaults; `devsandbox inspect <instance>` prints a
`browser sandbox:` line saying why it's off and whether devsandbox set its
sandbox-off knobs.

## Debugging a sandbox

Container name for an instance is `devsandbox-<instance>`; service containers are
`devsandbox-svc-<project>-…`. `devsandbox ps -a` shows the names. Typical loop:

```bash
devsandbox ps -a                  # what exists, what's stopped
devsandbox logs --build web       # what the last run/rebuild/start printed
devsandbox logs web               # lifecycle / container output (-n 200 for more)
devsandbox logs devsandbox-svc-<proj>-database   # a service's output
devsandbox inspect web            # mounts, networks, env, exit code
devsandbox stats                  # anything pegging CPU or leaking memory?
devsandbox exec web sh -c 'env; ls -la'          # poke around inside
```

All of these go through the runtime backend, so they work unchanged on docker,
podman, and Apple `container` — no need to call `docker` directly. Remember a
failed lifecycle command keeps the container around precisely so `logs` + `exec`
can diagnose it.
