# `devsandbox run` — Plan

`devsandbox run` mimics `devcontainer up` (reuse-or-create semantics, lifecycle hooks,
metadata-driven attach) with three devsandbox-specific twists: deterministic container
names that survive restarts, git-worktree-based multi-instance sandboxes, and services
shared *across* instances instead of owned by one compose project.

## 1. Naming contract (prerequisite for `devsandbox vscode`)

`devsandbox vscode <id>` will use VS Code's **attached-container** flow
(`vscode-remote://attached-container+<hex(container-name)>/<workspace>`). Extensions and
settings for that flow come from
`<globalStorage>/ms-vscode-remote.remote-containers/nameConfigs/<container-name>.json`,
which is keyed **by container name**. Therefore:

- Container names must be **deterministic and stable across restarts**:
  `devsandbox-<instance>` where `<instance>` defaults to the sandbox name for the first
  instance and `<sandbox>-<n>` (first free ordinal) for subsequent ones. No random
  suffixes (the current `suffix()` in `run.rs` must go).
- Restarting an instance (`devsandbox run <sandbox>` with an existing stopped container,
  matched by the `devsandbox.instance` label) does `docker start` on the **same
  container** — same name, so the nameConfig keeps applying. This mirrors
  `findExistingContainer`/`startExistingContainer` in the devcontainer CLI.
- The nameConfig is (re)written on every `run`, not only on create, so config edits
  (extensions list) propagate.
- nameConfig base dir must be resolved per editor and per OS
  (Linux `~/.config/<Product>`, macOS `~/Library/Application Support/<Product>`;
  Product = `Code`, `Cursor`, `VSCodium`, …). Write to every installed product, or make
  it configurable.

## 2. `run` flow (per invocation)

```
resolve sandbox config (extends deep-merge)
resolve instance name (explicit --name | default deterministic)
        │
        ├─ instance exists in state + container exists ──► docker start (if stopped)
        │                                                  run postStartCommand
        │                                                  refresh nameConfig, done
        │
        └─ new instance:
             resolve workspace source folder:
                 first instance for this base folder ──► mount folder directly
                 base folder already mounted directly by
                 another instance (any sandbox, `folders`
                 entries included)                   ──► create git worktree (see §3)
             `folders` entries: same idea, detached worktrees
                 (docs/folders-worktrees.md)
             initializeCommand (host)
             ensure shared services up (see §4)
             build image if build.dockerfile (tag devsandbox-img-<sandbox>)
             docker run -d --name devsandbox-<instance>
                 labels: devsandbox.sandbox, devsandbox.instance,
                         devsandbox.config_hash, devsandbox.base_folder
                 network: project network (see §4)
                 sleep infinity
             write nameConfig
             onCreate/updateContent/postCreate/postStart/postAttach via docker exec
             record instance in state
```

Config drift: label the container with a hash of the resolved config; on reuse, warn
when the hash differs (devcontainer CLI solves this with the `devcontainer.metadata`
image label — we keep our own label but *also* write `devcontainer.metadata` so VS Code
attach picks up user/workspace hints).

## 3. Worktrees for multi-instance sandboxes

When `run` targets a sandbox whose `folder` is already mounted by an existing instance:

- **Delegate to git**: `git -C <folder> worktree add <sandbox-root>/.worktrees/<instance>
  -b sandbox/<instance>` (new branch by default). A `--branch` naming an existing
  branch reuses it: a local one is checked out as is, one only on `origin` becomes a
  local branch tracking `origin/<branch>`. Git refuses a branch already checked out
  elsewhere; `run` reports where (and which instance), and `rm` only offers to delete
  branches `run` created.
- The new instance reuses the **same resolved sandbox config** as the original — only the
  mounted source differs. Same image, same services, same workspace path convention
  (`/workspaces/<basename>`).
- **Git-inside-container problem**: a worktree's `.git` is a *file* pointing at
  `<base>/.git/worktrees/<name>` by absolute host path, and that in turn points back via
  `commondir`/`gitdir`. For git to work inside the container we mount the base repo's
  `.git` directory (read-write; commits from the sandbox must land in the shared object
  store) at the **identical absolute path** inside the container, alongside the worktree
  mount. This is what the devcontainer CLI's `mountGitWorktreeCommonDir` option does.
  Alternative if we want to avoid host-path mirroring: run
  `git worktree repair` / relative gitdir rewriting inside the container — deferred.
- Cleanup: `devsandbox rm <instance>` runs `git worktree remove`, after first checking
  the worktree is clean (a dirty one is refused before anything is torn down). A branch
  `run` created is deleted with `git branch -D` on `--delete-branch`, kept on
  `--keep-branch`; with neither flag `rm` prompts on a TTY and keeps it otherwise.
- State gains `worktree: Option<PathBuf>` and `base_folder: PathBuf` per instance.
- Extra `folders` roots follow the same idea, as detached worktrees with no branch.
  They're recorded in `Instance.folders`, kept across `rebuild`, and removed by `rm`.
  See `docs/folders-worktrees.md`.

## 4. Shared services (the docker-compose divergence)

docker-compose ties service lifetime to one project; we want `[services.*]` shared by
**all instances** that declare them. Design:

- One **docker network per config root**: `devsandbox-net-<project-id>` where
  `project-id` = short hash of the canonicalized config.toml directory. Created
  idempotently; every sandbox instance container joins it.
- Each service runs as a standalone container `devsandbox-svc-<project-id>-<name>`,
  labeled `devsandbox.service=<name>`, with a network **alias = service name** so
  sandboxes reach it as `postgres:5432` etc.
- **Idempotent ensure**: `run` starts a service container only if absent/stopped.
  Existing+running → no-op. Image/env change detection via config-hash label → warn
  (recreate only with an explicit flag; other instances may be using it).
- **Lifecycle / ownership**: services are never stopped implicitly. `devsandbox rm`
  removes the instance; a `devsandbox gc` (or `rm --services`) stops service containers
  when no live instance in the state references them. Refcounting is derived (state +
  `docker ps`), never stored, to avoid state races.
- **Compose-based sandboxes**: they keep their own compose project per instance, but we
  generate an extra override file (`-f`) declaring the project network as
  `external: true` and attaching the main service to it, so compose sandboxes see shared
  services too. We never edit user compose files.
- Host port exposure for services stays opt-in per service config (`ports`), with the
  caveat that two config roots exposing the same port will collide — fail with a clear
  error.

## 5. Out of scope for this iteration

- devcontainer **features** (would require the generated-Dockerfile pipeline; keep
  warning as unsupported).
- remoteUser/UID remapping (`updateRemoteUserUID`).
- Shared cache mounts (`cache-folder`) — separate work item, orthogonal.
- `devcontainer-lock.json`, OCI fetching.

## 6. Implementation steps

1. **Deterministic naming + reuse**: replace `suffix()` with ordinal naming; look up
   existing instance/container by label; `docker start` path with `postStartCommand`;
   config-hash label + drift warning.
2. **nameConfig hardening**: per-OS/per-product globalStorage resolution; write on every
   run.
3. **Worktree path**: detect base-folder-in-use, `git worktree add`, `.git` common-dir
   companion mount, state fields, `rm` cleanup.
4. **Shared services**: project network, `ensure_service()`, network alias wiring,
   `gc`/teardown command, compose override generation.
5. **Tests**: naming determinism, worktree mount args, service ensure idempotence
   (docker mocked behind the `docker.rs` seam).
