# devsandboxes — High-Level Plan

A Rust CLI that layers sandbox management on top of devcontainers: shared caches across projects, and lifecycle commands that delegate to docker / docker-compose without requiring the devcontainer CLI.

## Goals

- Stay **compatible with devcontainer.json semantics** (features, image, build.dockerfile, dockerComposeFile) while adding a config layer (`config.toml`) with templates, deep-merge `extends`, shared services, and shared caching between all devcontainers.
- **No daemon**: the CLI defers to docker for runtime; host-side state stored in a user location (e.g. `~/.local/share/devsandboxes`).
- **Security boundary**: an orchestrator/agent driving the CLI must not be able to mutate the host — no arbitrary mounts, only start/exec inside sandboxes.

## Architecture

```
.devsandbox/            versioned sandbox definitions (git repo)
  .worktrees/           git worktrees to avoid recloning projects
  config.toml           services, templates, sandboxes
  dockerfiles/          supporting build files
repository-1/           sibling project folders referenced by sandboxes (but they could be wherever)
```

Core components:

1. **Config layer** — parse `config.toml`: `[services.*]`, `[template.*]`, `[sandbox.*]`. Resolve `extends` as a deep merge (sandbox overrides template). Validate at the boundary; sandbox entries accept devcontainer.json properties plus `extends`, `folder`, `services`, `cache-folder`.
2. **Devcontainer derivation** — translate a resolved sandbox config into an effective devcontainer configuration (image / Dockerfile / compose), including shared cache mounts (e.g. `.pnpm-store`) managed by the CLI, never user-specified arbitrary mounts.
3. **Runtime layer** — shell out to docker / docker-compose. Containers named with a `devsandbox-` style prefix so they can be discovered via `docker ps` filtering. Linked services (e.g. postgres) started alongside and networked to the sandbox.
4. **State store** — record created sandboxes (name, image/config used, folder, containers) in a user-level location; no daemon.
5. **Editor integration** — launch VS Code (or a fork like Cursor, swappable) attached to a running sandbox in devcontainer mode, doing whatever setup vscode needs.

## CLI surface (v1)

| Command                                  | Behavior                                                                                                                                                                           |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `devsandbox ls`                          | List sandbox configs ("images") defined in the current folder's config.                                                                                                            |
| `devsandbox ps`                          | List running sandbox instances (proxy to `docker ps` filtered by naming prefix).                                                                                                   |
| `devsandbox run`                         | Start a new sandbox from a config; defaults to background; named (or name returned); interactive image picker when on a TTY; offers to generate an example config when none exist. |
| `devsandbox exec [-it] <name> <command>` | docker-exec-like; `<name>` may be a repository or sandbox image name — interactive disambiguation on a TTY, error when ambiguous otherwise.                                        |
| `devsandbox vscode <sandboxid>`          | Open VS Code (or fork) attached to the sandbox in devcontainer mode.                                                                                                               |
| `devsandbox init`                        | Config scaffolding — **deferred, not in v1**.                                                                                                                                      |

## Milestones

1. **M1 — Config core**: TOML parsing, template/sandbox model, `extends` deep merge, `ls`.
2. **M2 — Runtime basics**: derive docker run/compose invocations, container naming scheme, `run`, `ps`, `exec`, host-side state store.
3. **M3 — Devcontainer compatibility**: honor devcontainer.json properties and features; shared cache mounts; linked `[services]`.
4. **M4 — Editor integration**: `vscode` command with swappable editor binary.
5. **M5 — Polish**: interactive TTY menus, example-config generation, worktree support.

## Open questions

- Should `[services]` ports ever be exposed on the host, or container-to-container only?
- Exact user location and format for runtime state.
- How much of the devcontainer **features** spec to implement natively vs. reuse.
