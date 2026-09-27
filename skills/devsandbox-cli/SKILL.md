---
name: devsandbox-cli
description: |
  Use the devsandbox CLI to create, run, exec into, and tear down devcontainer-based sandboxes from a config.toml file.
  Covers every subcommand, ordered by how useful each is to an agent, plus a config.toml overview (full format: the `config-toml-spec` skill).
---

# devsandbox CLI

`devsandbox` manages devcontainer-style sandboxes directly on a container runtime
(docker / podman / Apple `container`) — no devcontainer CLI, no daemon. One
`config.toml` that lives outside of any project, unique per user.

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

## config.toml

TOML, not JSON. Three top-level table families:

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

`<name>` accepts an **instance name**, a **sandbox config name**, or a **repository folder basename**. Ambiguity errors
on a non-TTY (prompts on a TTY). Most commands are non-interactive-safe; anything needing a choice bails with the options
listed when stdin is not a TTY.

1. **`exec [-i] [-t] <name> <cmd…>`** — run a command inside the instance. Runs in
   the workspace dir and honors the recorded `remoteEnv` / `remoteUser`. Exits
   with the command's own status. This is the agent's main way to do work inside a
   sandbox. `--` is not needed (`allow_hyphen_values`), but `-i`/`-t` are
   devsandbox flags and must precede `<name>`. Non-interactive commands need
   neither `-i` nor `-t`; use `-it` only for a shell.

   ```bash
   devsandbox exec web pnpm test
   devsandbox exec -it web zsh
   ```

2. **`run [sandbox] [--name <n>]`** — create/start an instance in the **background**.
   Prints only the instance name to stdout (warnings go to stderr) — capture it.
   On a non-TTY the sandbox arg is **required** (it lists available names on
   error). Behavior worth knowing:

   - Never reuses an instance: a taken name is an error (`start <name>` restarts
     a stopped instance, `rebuild <name>` recreates a drifted one).

   - A **second `run` against a folder already live** creates a git **worktree**
     under `.worktrees/<instance>` on a fresh `sandbox/<instance>` branch, and a
     `<sandbox>-<n>` instance name — the two containers never share a checkout.

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

4. **`inspect <name>`** — pretty-printed runtime `inspect` JSON (config, mounts,
   networks, env, exit code). Same name resolution as `logs`, service containers
   included. Keys are colour-highlighted on a TTY only; piped output is plain
   JSON, safe to parse.

5. **`ps [-a]`** — list instances (joined from state + runtime). `-a` includes
   stopped ones. Columns: NAME, IMAGE, STATUS.

6. **`stats`** — one-shot CPU/memory usage of every running `devsandbox-*`
   container (instances and services). Columns: NAME, CPU, MEM.

7. **`ls`** — list sandbox _configs_ from `config.toml` (NAME, SOURCE, FOLDER,
   SERVICES, EXTENDS). Use to discover what can be `run` and to catch config
   parse/validation errors.

8. **`stop <name>`** — stop the instance container + its isolated service
   containers. Idempotent (already-stopped/missing is fine). State survives; a
   later `run` restarts it.

9. **`rebuild <name>`** / **`rebuild --all`** (alias `recreate`) — recreate the
   instance's container from the current config when it has drifted; the
   worktree, branch, instance name, and per-instance state (shell history,
   `${instance}` mounts) are kept and the lifecycle commands re-run. No drift →
   no-op (safe to run speculatively); `--force` recreates anyway (picks up
   devsandbox-side behavior changes the drift hashes cannot see). `--all`
   rebuilds every drifted instance (every instance with `--force`), skipping
   ones from other config roots. The fix for the config-drift warning.

10. **`rm <name>`** — remove container, its worktree (prompts to delete the
   `sandbox/<instance>` branch on a TTY), isolated services, per-instance network,
   and the state entry. Managed shell history is **kept** so a rebuilt instance
   inherits it. Global services are left for `gc`.

11. **`gc [--force]`** — reap shared services no live instance references, orphaned
   networks, and orphaned shell-history files (`--force` skips the per-file
   confirm). Housekeeping, not part of a normal task loop.

12. **`devsandbox`** (no subcommand) — opens the interactive **TUI dashboard** on a
    TTY; prints help and exits `2` otherwise. **Not for agents** — it takes over
    the terminal.

All commands colour output only when stdout is a TTY and `NO_COLOR` is unset;
piped/captured output is always plain text.

## State

- Instances: `~/.local/share/devsandbox/state.toml` (`$XDG_DATA_HOME` honored).
- Persistent per-config-root data (caches, shell history, `${sharedVolumes}`):
  `<config dir>/shared-volumes/`.
- Networks/containers are namespaced by a short hash of the config-root path, so
  two projects never collide.

## Debugging a sandbox

Container name for an instance is `devsandbox-<instance>`; service containers are
`devsandbox-svc-<project>-…`. `devsandbox ps -a` shows the names. Typical loop:

```bash
devsandbox ps -a                  # what exists, what's stopped
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
