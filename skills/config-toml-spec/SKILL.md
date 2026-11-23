---
name: config-toml-spec
description: Reference for devsandbox's `devsandboxes.toml` format — `[template.*]`, `[sandbox.*]`, `[services.*]`, `[profiles.*]`, `[updates]`, `extends` merge rules, `${…}` variables, and which devcontainer properties are implemented. Use when writing, editing, reviewing, or debugging a devsandbox config (e.g. porting a devcontainer.json to it).
---

# devsandbox `devsandboxes.toml` spec

`devsandboxes.toml` is one file, outside any project, describing every sandbox of a workspace (separate config dirs split workspaces, e.g. work vs personal, not projects). It replaces per-repo `devcontainer.json`: same property names, TOML syntax, plus devsandbox extras.

## Files

The *config dir* is `-C <dir>` (default `.`). It holds one of `devsandboxes.toml`, `devsandboxes.yaml` or `devsandboxes.yml`; more than one is an error. YAML has the same schema and rules (`sandbox: {web: {image: …}}` for `[sandbox.web]`) and hashes like its TOML equivalent; `null` isn't a valid value. A legacy `config.toml` is read when none exist and renamed to `devsandboxes.toml` (left alone without a `sandbox`, `template` or `services` table).

Top-level tables, all optional:

```toml
[template.<name>]   # reusable, never run directly
[sandbox.<name>]    # runnable: `devsandbox run <name>`
[services.<name>]   # sidecar containers a sandbox lists in `services`
[profiles.<name>]   # profiles a sandbox lists in `profiles`
[updates]           # how often each feature update runs
```

Any other top-level table is an error.

Beside the file devsandbox keeps `shared-volumes/` (`${sharedVolumes}`, persistent host state), `.worktrees/` (instances' git worktrees) and `shared-files/` (`worktree-link` stores). Relative paths (`folder`, `folders`, `shell-rc`, `build.dockerfile`, `build.context`, local features) resolve against the config dir.

## Templates and `extends`

- `extends = "name"` or `extends = ["a", "b"]`, on sandboxes and templates (recursive). Only templates can be extended; cycles and unknown names error.
- Merge order: the listed templates left to right, then the table's own body.
- Tables merge recursively (`containerEnv`, `build.args`, `features`, `customizations`…); scalars: the later value wins.
- **Arrays concatenate**, base first (`mounts`, `caches`, `services`, `shell-rc`, `customizations.vscode.extensions`…), so a template's mounts and extensions reach every sandbox. An inherited entry can't be removed, and diamond inheritance duplicates entries.
- `mounts`: the last entry for a target wins, so mounting the same target again overrides an inherited mount. A target nested inside an inherited one is not an override: both apply, parents first.
- The merged result must be a valid sandbox. Templates are only validated through the sandboxes extending them.

## Sandbox properties

Devcontainer keys are camelCase; devsandbox extras are kebab-case. **Unknown keys are a hard error**, so typos surface immediately.

### Project and worktrees

- **`folder`** (string): host project dir, relative to the config dir; must exist. Required by `run`. A second instance of the same folder gets its own git worktree. Refused when it contains a config dir (`folder = "."`, a config inside the repo): keep the config beside the repo.
- **`worktree-branch`** (string): the branch of a worktree instance, `${…}` allowed. Default `sandbox/${instance}`; `run --branch` overrides. An existing local branch is checked out as is, one only on `origin` becomes a local branch tracking it, else it's created. A branch checked out in another worktree errors.
- **`worktree-base`** (string): commit-ish a new branch starts from (e.g. `origin/develop`). Default: the remote's default branch; `run --base` overrides.
- **`worktree-include`** ([string]): gitignore-syntax patterns of ignored, untracked files copied from the base repo into each worktree on `run` and `rebuild`, never overwriting. Applied after the repo's own `.worktreeinclude`, so `!pat` negates it.
- **`worktree-link`** ([string]): gitignored paths from the repo root (`.env`, `packages/*/.env`; `*`/`?` within one segment, no `**`) shared live by every instance: the file moves to `shared-files/<folder-basename>-<hash>/` and each tree gets a symlink to it. Tracked paths and conflicting files are skipped with a warning; `..`, absolute and `.git` paths error. The store is kept on `rm`.
- **`folders`** ({container path = host dir | `{ path, worktree }`}): extra workspace roots. Keys are absolute and differ from `workspaceFolder`; paths must exist, `${…}` allowed. `worktree`: `"auto"` (default: a worktree when the dir is a git repo root that some sandbox's `folder` or another instance already uses, or for a dispatcher child; else a direct bind), `"always"` (a worktree; errors if not a repo root) or `"never"` (the live host dir). These worktrees are detached at the base checkout's `HEAD`. Unknown keys and values error.

### Environment

- **`services`** ([string]): names from `[services.*]` to start and attach.
- **`caches`** ([string]): shared package caches, any of `pnpm`, `cargo`, `npm`, `yarn`, `go`, `pip` (other names error). Each mounts a dir of `shared-volumes/` at `/var/cache/devsandbox/<name>` and points the tool at it. `cargo` sets `CARGO_HOME`, so `cargo install` lands in the cache's `bin/`, not on `PATH`. See [Shared package caches](https://devsandboxes.com/examples/caches).
- **`persist-shell-history`** (bool): per-instance shell history in `shared-volumes/history/<id>/`, mounted at `/commandhistory` with `HISTFILE` set. Kept on `rm`.
- **`shell-rc`** ([string]): host shell snippets sourced by `~/.zshrc` and `~/.bashrc`; must be existing files. Each one's parent dir is mounted read-only, so a snippet can't sit directly in the config dir: use a subdir.
- **`identity`** (`"host"` | `"image"`): `"host"` (default) gives the account `remoteUser` names (else `containerUser`, else the image's `USER`) the host's uid/gid, keeping its name, home and shell; with no account to adopt the sandbox runs as root, with a warning. `"image"`: users as written, no remapping. Unset with `updateRemoteUserUID = false` means `"image"`. `run`, `start` and `rebuild` offer to fix entries assuming root (`/root` paths). See [Who you are inside](https://devsandboxes.com/docs/going-further#who-you-are-inside).
- **`userns`** (bool): default `true`, which lets unprivileged processes create user namespaces, as a browser's own sandbox (Chromium, Electron) needs: a seccomp profile on docker, podman's default, nothing on Apple `container`. `run` checks the browser sandbox works and sets `DEVSANDBOX_BROWSER_SANDBOX=1`, else the tools' sandbox-off knobs. `false`: docker's default profile, less kernel attack surface. See [Browsers with their sandbox](https://devsandboxes.com/examples/browsers).
- **`placement`** (`"home"`): experimental (fleet). `run --place` never moves this sandbox to another machine; it runs on the host whose config names it. The only value.

### Automations

- **`autostart`** (`true` | `false` | `"runtime"`): `true` starts this sandbox's stopped instances once per host boot, or runs one if it has none (dispatcher children aside), at the first of: the host daemon's start, a `run`, a `start` or the dashboard. `"runtime"` also creates the container with `--restart unless-stopped` on docker/podman, and an in-container hook re-runs `postStartCommand` on every container start; podman restarts at boot only with `podman-restart.service` enabled. Apple `container` has no restart policy and behaves as `true`.
- **`dispatcher`** (`{ spawn = [string], max-instances = int }`): its instances may create, start, stop and remove *child* instances through the in-container control API (`devsbd ensure|ls|branches|stop|rm|done|exec|run …`). `spawn` names the sandboxes of this config it may instantiate; `"*"` is every non-dispatcher one, privileges included; omitted: none. A dispatcher is never spawnable. `max-instances` caps its children, stopped ones included (default 10). Children are `<sandbox>-<key>`, each on its own worktree, and outlive the dispatcher. A child's `--env` may not set loader or shell variables (`PATH`, `HOME`, `LD_*`, `GIT_*`, …). Unknown keys error.
- **`inbox`** (bool): `true` lets instances own Inbox threads (`devsbd thread put|send|withdraw|rm|ls`, `devsbd events`); without it those are denied. Plain `devsbd notify` never needs it. Separate from `dispatcher`: a dispatcher wanting threads sets both.

### Credentials and keys

- **`profiles`** ([string]): the `[profiles.*]` this sandbox's instances may be in, preferred first. Unset: every declared profile, in name order. An undeclared name errors; repeats are dropped.
- **`access`** ({kind = table}): what the sandbox takes of this host's credential for a feature's claim kind (`[sandbox.web.access.sentry]`; kinds are lowercase letters, digits and `-`). All keys optional, unknown ones error. See [gh and sentry without a token](https://devsandboxes.com/examples/credentials).
  - `scopes` ([string]): the claim's scopes calls may use; unset or empty: all of them, within the credential's grant.
  - `ask` ([string]): scopes whose calls are held for a permission request (answered in the dashboard, the desktop popup or `devsandbox access approve|deny`), within the claim's `hold`; on a repo-fenced claim only routes naming a repo can be asked.
  - `credential` (string): a credential from `credentials.toml` (`"sentry-work"`) to take instead of the profile's.
  - `repos` ([string], `github`): repos (`owner/repo` or a URL) the repo fence allows beyond the remotes recorded at the folder's first `run`.
  - `orgs` ([string], claims with an org fence, e.g. `sentry`): the org slugs calls may name, within the credential's `orgs` in `credentials.toml`; unset or empty: all of the credential's.
  - `as` (`"user"` | `"app"`): the token kind, for claims declaring kinds (github); default `user`. `app` is refused as not supported yet.
  - `branches` ([string]): parsed, not enforced yet.

  The broker reads `credential`, `scopes`, `ask` and `repos` on every call, so edits apply without a rebuild.
- **`ssh-keys`** ([`{ key, destinations, signing-only }`]): for a sandbox with a git-host claim (e.g. the `gh` feature), the host agent's keys the agent relay offers; unset: none. `key`: `"SHA256:…"` as `ssh-add -l` prints it, or the public key line. `destinations`: hosts as `known_hosts` names them (`"[git.corp]:2222"`); a login is signed only to a host whose key is in **this host's** `known_hosts`. An empty list errors. **A key without `destinations`, or listing a claimed git host, can push over ssh past the repo fence.** `signing-only = true`: git commit signatures only, no `destinations`. Unknown keys error.

### Implemented devcontainer properties

- **`image`**: base image. Exactly one of `image` and `build`.
- **`build`**: `{ dockerfile (required), context = ".", args = {}, target, cacheFrom = str|[str], options = [str] }`. `cacheFrom` is ignored with a warning where the runtime lacks it.
- **`features`**: `{ "<ref>" = { opts } | "version" | true | false }`; `false` disables an inherited one. `<ref>` is an OCI ref (`ghcr.io/devcontainers/features/node:1`) or a local folder starting with `./`, `../` or `/` holding `devcontainer-feature.json` and `install.sh`; no https or tarball refs. Ordered by `installsAfter`. Feature `mounts` (a sandbox mount on the same target wins), `privileged` and `entrypoint` apply; `capAdd` is ignored. A feature's credential claim gives the sandbox a stand-in token while the real one stays on the host; `run` refuses a claim version not yet approved (`devsandbox access approve`) and two features claiming one kind.
- **`workspaceFolder`**: container path of the working tree. Default `/workspaces/<folder basename>`; `${…}` allowed.
- **`mounts`**: `"source=…,target=…,type=bind[,readonly]"` or `{ type, source, target, readonly }`; `type` defaults to `bind`, which needs `source`. A missing bind source is created: a file when the basename has a dot after its leading dots (`creds.json`), else a dir (`.zshrc` becomes a dir). A `volume` named with `${instance}` or `${devcontainerId}` is per instance and deleted on `rm`; others are shared. Refused: a bind covering a config dir, a whole `shared-volumes/`, the member key or a dir above it (`~`, the data dir, `/`), or another instance's worktree or volume.
- **`containerEnv`**: the container's env. Values are `${…}`-substituted (beyond the spec); `${containerEnv:X}` there reads the image's env, so `PATH = "${containerEnv:PATH}:/opt/bin"` extends it. Wins over devsandbox's own exec env.
- **`remoteEnv`**: env of every `exec` and lifecycle command, `${…}`-substituted; wins over devsandbox's own exec env. With an intercept credential claim, setting `HTTPS_PROXY` or `NO_PROXY` warns: execs would bypass the broker. With a git-host claim, `GIT_CONFIG_*` entries here or in `containerEnv` go before the claim's git settings, never replacing them.
- **`containerUser`**, **`remoteUser`**: under `identity = "image"`, `docker run --user` and the user of execs, lifecycle commands and VS Code. Under `"host"`, the account adopted (`remoteUser` first). On rootless docker execs run as root, with `HOME` set to that account's home (`remoteEnv.HOME` overrides).
- **`updateRemoteUserUID`** (bool): `false` with `identity` unset means `identity = "image"`; `true` changes nothing.
- **`init`**: `true` runs the container with `--init`.
- **`privileged`** (bool): `--privileged`, OR-ed with enabled features' `privileged`, so `false` can't veto one. Skipped with a warning on Apple `container`.
- **`hostRequirements`**: only `gpu` is acted on: `true` (required), `false`, `"optional"`, or `{ cores, memory }` (required; both warned as not enforced). It means every NVIDIA GPU (`--gpus all` or CDI); none on Apple `container`. Required with no usable GPU fails `run`; optional warns and runs without. A fleet worker needs `[allow] gpus = true`. `cpus`, `memory` and `storage` take any value and are warned; other keys error. See [GPUs](https://devsandboxes.com/docs/going-further#gpus).
- **`initializeCommand`**: runs on the **host**, in the config dir, before anything is created.
- **`onCreateCommand`**, **`updateContentCommand`**, **`postCreateCommand`**, **`postStartCommand`**, **`postAttachCommand`**: run in that order in the container after create, as the exec user, in `workspaceFolder`, with `remoteEnv`. `start` re-runs only `postStartCommand`.
- **`customizations.vscode.extensions`** ([string]): installed via VS Code (as workspace recommendations on Apple `container`).
- **`forwardPorts`** ([int | string]): forwarded to `127.0.0.1` on the host by the host daemon while an instance runs. `3000` is the instance's port, `"db:5432"` a port of a service in `services`; `"8080:3000"` and `"15432:db:5432"` pick the host port to start from (a leading number is always the host port). A taken port moves to the next free one, remembered per instance. Duplicates are dropped; other forms error. See [Port forwarding](https://devsandboxes.com/examples/port-forwarding).

Lifecycle commands take `"shell string"` (run with `sh -c`), `["argv", "..."]`, or a table `{ name = "cmd" | [argv] }`, whose entries run **one after another**, in key order, not in parallel.

### Accepted but ignored (warned at `run`)

`name`, `appPort`, `portsAttributes`, `otherPortsAttributes`, `runArgs`, `workspaceMount`, `overrideFeatureInstallOrder`, `userEnvProbe`, `overrideCommand`, `shutdownAction`, `capAdd`, `securityOpt`, `hostRequirements` but `gpu`, `waitFor`, `secrets`, `customizations.vscode.settings`, any other `customizations.vscode.*` or `customizations.<tool>`.

Not accepted (hard error): the compose keys `dockerComposeFile`, `service`, `runServices`. Use `[services.*]` instead.

## `${…}` variables

Substituted in `mounts` (source and target), `workspaceFolder`, `worktree-branch`, `shell-rc`, `folders` (keys and values), and `containerEnv` and `remoteEnv` values. Unknown expressions stay verbatim. `${containerEnv:NAME}` resolves only at create, once the image exists: in `mounts`, `shell-rc`, feature mounts, `containerEnv` and `remoteEnv`; elsewhere, and in `ls`, it stays verbatim.

| var | value |
|---|---|
| `${configDir}` | absolute config dir |
| `${sharedVolumes}` | `${configDir}/shared-volumes` |
| `${localWorkspaceFolder}` | absolute host `folder` (the base repo, even for worktree instances) |
| `${localWorkspaceFolderBasename}` | its basename |
| `${instance}` | persistent instance id (the name at creation; stable across renames) |
| `${devcontainerId}` | same as `${instance}` |
| `${localEnv:VAR}` | host env var, empty if unset |
| `${localEnv:VAR:default}` | host env var if set (even to empty), else `default` (everything after the `:`) |
| `${containerEnv:NAME}` | the sandbox's `containerEnv`, else the image's `ENV`, else for `HOME` the home of the user the container runs as; empty if unset. `${containerEnv:NAME:default}` as for `localEnv`. `target=${containerEnv:HOME}/.claude` lands in that user's home |

## Services (`[services.<name>]`)

Unknown keys error. Services take no `extends`. See [Private or shared services](https://devsandboxes.com/examples/services).

| key | type | meaning |
|---|---|---|
| `scope` | `"isolated"` (default) \| `"global"` | isolated: one container per instance, on its own network; global: one container per config dir. |
| `image` / `build` | as for a sandbox | exactly one required. |
| `env` | {string = string} | env vars. |
| `ports` | [string] | `docker run -p` specs (`"5432"`, `"5432:5432"`, `"127.0.0.1:5432:5432"`). A bare container port gets the same host port when free, else one the runtime picks. A host port already published elsewhere errors. |
| `command` | string \| [string] | appended after the image. A string is **one** argument, not shell-split. |
| `hostRequirements` | table | as for a sandbox; `gpu` is checked before the image is built or pulled, other keys are ignored silently. |

Sandboxes reach a service by its name as hostname. `devsandbox gc` reaps unreferenced service containers; `devsandbox service rebuild <name>` recreates one and rewires running sandboxes without restarting them.

## Profiles (`[profiles.<name>]`)

A profile is a named collection of sandboxes: those listing it in `profiles`, plus every sandbox that sets no `profiles`. The table takes no keys yet (any key errors). Names are `[a-z0-9][a-z0-9_.-]*`. `devsandbox run --profile <name>` picks an instance's profile (else the last one picked for that sandbox, else its first allowed); `devsandbox profile <instance> <name>` switches it. A dispatcher's children share its profile.

```toml
[profiles.work]
[profiles.personal]

[template.work]
profiles = ["work"]

[sandbox.api]
extends = "work"
```

## Updates (`[updates]`)

A feature can declare updates: a command refreshing a shared dir its sandboxes mount read-only, every `7d` unless it sets another interval. `[updates]` overrides that by update name for the whole config dir: `"<n>h"`, `"<n>d"`, `"<n>w"` (`n` ≥ 1) or `"never"`. Names are `[a-z0-9][a-z0-9-]*`, at most 32 characters; a bad name or value errors. The host daemon runs due updates; `devsandbox update <name>` runs one now.

```toml
[updates]
gh = "1d"
node = "never"
```

## Drift

Containers carry a hash of their merged sandbox (or service) table and of their dockerfile's contents, plus, for a sandbox, its local features' files. Any change, including through a template, marks existing instances as drifted until `devsandbox rebuild <instance>`. Exceptions, applied without a rebuild: `autostart`, `dispatcher`, `inbox`, `placement`, `forwardPorts`, `profiles`, `access`, `ssh-keys` and `[updates]`. A `folders` entry restating the default (`{ path = "x" }`) hashes like `"x"`. The build context and OCI feature contents aren't hashed.

## Checking a config

- `devsandbox -C <dir> ls` resolves and validates every sandbox (merge and types) without touching the container runtime. It doesn't check that `folder`, `shell-rc` or `folders` paths exist, or that `services` names are defined: `run` does.
- The dashboard's config view shows a sandbox's table as written and as resolved.

## Example

```toml
[template.base]
init = true
worktree-branch = "me/${instance}"
caches = ["pnpm", "cargo"]
persist-shell-history = true
mounts = ["source=${sharedVolumes}/claude,target=${containerEnv:HOME}/.claude,type=bind"]

[services.postgres]
image = "postgres:16"
env = { POSTGRES_PASSWORD = "postgres" }

[sandbox.web]
extends = "base"
folder = "../web"
build = { dockerfile = "dockerfiles/web.Dockerfile" }
services = ["postgres"]
containerEnv = { DATABASE_URL = "postgres://postgres:postgres@postgres/postgres" }
postCreateCommand = "pnpm install"
customizations.vscode.extensions = ["dbaeumer.vscode-eslint"]
```
