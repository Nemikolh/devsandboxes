---
name: devsandbox-cli
description: |
  Use the devsandbox CLI to create, run, exec into, and tear down devcontainer-based sandboxes from a config.toml file.
  Covers config.toml (devcontainer properties plus devsandbox extras) and every subcommand, ordered by how useful each is to an agent.
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

Sandbox/template keys use devcontainer **camelCase** (`postCreateCommand`, `containerEnv`, …).
Unknown keys are a **hard error**. Valid devcontainer properties that aren't implemented yet
parse but emit a `warning: ignoring unsupported properties: …` at `run` (see [what's implemented](#devcontainer-properties)).

### devsandbox extras (what differs from devcontainer.json)

These keys do not exist in devcontainer.json — they are devsandbox additions:

| Key                     | Where             | Meaning                                                                                                                                                                                                                                              |
| ----------------------- | ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `extends`               | sandbox, template | Template name (`"base"`) or list (`["a","b"]`) to deep-merge **under** this table. Merged left-to-right; templates may extend templates; cycles are detected and error.                                                                              |
| `folder`                | sandbox           | **Required to `run`.** Host project folder, relative to the config dir, mounted as the workspace. Must exist.                                                                                                                                        |
| `folders`               | sandbox           | Extra VS Code roots: `{ "/abs/container/path" = "../host/folder" }`. Each is bind-mounted and listed in a generated `.code-workspace`. Container path must be absolute and differ from the primary workspace.                                        |
| `services`              | sandbox           | List of `[services.<name>]` to start and network alongside the instance.                                                                                                                                                                             |
| `caches`                | sandbox, template | Package-manager caches to persist + share across the config root. Supported: `pnpm`, `cargo`, `npm`, `yarn`, `go`, `pip`. Each becomes a shared bind mount under `shared-volumes/` plus the env var(s) pointing the tool at it. Unknown name errors. |
| `persist-shell-history` | sandbox, template | `true` provisions a per-instance `.zsh_history` on the host, bind-mounted at `/root/.zsh_history`. Survives `rm` + `run`; never shared between concurrent instances. (Kebab-case, unlike the camelCase devcontainer keys.)                           |

`[services.<name>]` fields (also devsandbox-specific): `image` **xor** `build`, plus `env` (map), `ports` (list, `"8080:80"`), `command` (string or list),
and `scope = "isolated"` (default; one container per instance) or `"global"` (one per config root, shared). Reached from the sandbox by service name as
the DNS alias.

### Deep-merge semantics for `extends`

Nested tables merge recursively; **arrays concatenate** (base first, then the overriding table) so a sandbox can add to a template's `mounts` / `extensions`
without restating them; scalars are replaced.

### devcontainer properties

**Implemented** (behave as in devcontainer.json): `image`, `build` (`dockerfile`, `context`, `args`, `target`, `cacheFrom`, `options`), `workspaceFolder`,
`containerEnv`, `remoteEnv`, `containerUser`, `remoteUser`, `init`, `mounts`, `features`, `customizations.vscode.extensions`, and the lifecycle commands
`initializeCommand` (runs on the **host**), `onCreateCommand`, `updateContentCommand`, `postCreateCommand`, `postStartCommand`, `postAttachCommand`.

**Parsed but ignored (warn at `run`)**: `name`, `forwardPorts`, `appPort`, `portsAttributes`, `otherPortsAttributes`, `runArgs`, `workspaceMount`,
`overrideFeatureInstallOrder`, `updateRemoteUserUID`, `userEnvProbe`, `overrideCommand`, `shutdownAction`, `privileged`, `capAdd`, `securityOpt`,
`hostRequirements`, `waitFor`, `secrets`, `customizations.vscode.settings`, and any non-vscode `customizations.<tool>`.

`features` are fetched as OCI artifacts, cached per user, ordered by `installsAfter`, and baked into a derived image at `run`. Options accept the
devcontainer forms: `= true`, a bare version string, or an options table; `= false` skips the feature.

### Mount / cache variables

Usable in `workspaceFolder`, `mounts` sources, and `folders`:

  `${configDir}`, `${localWorkspaceFolder}`, `${localWorkspaceFolderBasename}`, `${localEnv:VAR}` (unset → empty),
  plus devsandbox's `${sharedVolumes}` (the config root's `shared-volumes/` dir) and `${instance}` (the running instance
  name — use it to anchor per-instance state in a mount source). Unknown `${…}` is left verbatim. Missing bind sources
  are auto-created (a final component with a dot → file, else directory).

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

   - Reuses an existing instance's container: starts it if stopped, refreshes
     service DNS + VS Code wiring, re-runs `postStartCommand`.

   - A **second `run` against a folder already live** creates a git **worktree**
     under `.worktrees/<instance>` on a fresh `sandbox/<instance>` branch, and a
     `<sandbox>-<n>` instance name — the two containers never share a checkout.

   - **Config drift**: reusing a container whose recorded config hash differs from
     the current config only _warns_; you must `rm` then `run` to apply changes.

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

9. **`rm <name>`** — remove container, its worktree (prompts to delete the
   `sandbox/<instance>` branch on a TTY), isolated services, per-instance network,
   and the state entry. Managed shell history is **kept** so a rebuilt instance
   inherits it. Global services are left for `gc`.

10. **`gc [--force]`** — reap shared services no live instance references, orphaned
   networks, and orphaned shell-history files (`--force` skips the per-file
   confirm). Housekeeping, not part of a normal task loop.

11. **`devsandbox`** (no subcommand) — opens the interactive **TUI dashboard** on a
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
