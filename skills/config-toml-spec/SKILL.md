---
name: config-toml-spec
description: Reference for devsandbox's `devsandboxes.toml` format — `[template.*]`, `[sandbox.*]`, `[services.*]`, `[profiles.*]`, `extends` merge rules, `${…}` variables, and which devcontainer properties are implemented. Use when writing, editing, reviewing, or debugging a devsandbox config (e.g. porting a devcontainer.json to it).
---

# devsandbox `devsandboxes.toml` spec

`devsandboxes.toml` is devsandbox's single source of truth: one file, outside any project, describing every sandbox of a workspace (separate config dirs split workspaces, e.g. work vs personal, not projects). It replaces per-repo `devcontainer.json` (same property names, TOML syntax, plus devsandbox extras).

## File layout

`devsandboxes.toml` lives in the *config dir* (`-C <dir>`, default `.`). `devsandboxes.yaml` / `devsandboxes.yml` are read instead when present: same schema, merge rules and validation, only the syntax differs (`sandbox: {web: {image: …}}` for `[sandbox.web]`), and a YAML config hashes the same as its TOML equivalent, so switching formats doesn't flag drift. YAML `null` is not a valid value. More than one of the three in a config dir is an error. A legacy `config.toml` (pre-0.5.1 name) is still read when none exist, and is renamed to `devsandboxes.toml` the next time `devsandbox` runs there; one without a `sandbox`, `template` or `services` table is not a devsandbox config and is left alone.

Four top-level tables, all optional:

```toml
[template.<name>]   # reusable, never run directly
[sandbox.<name>]    # runnable; `devsandbox run <name>`
[services.<name>]   # sidecar containers a sandbox lists in `services`
[profiles.<name>]   # profiles a sandbox lists in `profiles`
```

Next to it devsandbox uses: `shared-volumes/` (`${sharedVolumes}`, persistent host state), `.worktrees/<instance-id>/` (repeat-instance git worktrees), `.worktrees/<instance-id>.folders/<basename>-<hash>/` (`folders` worktrees), `shared-files/<folder-basename>-<hash>/` (`worktree-link` store, one per base folder). Relative paths (`folder`, `folders` values, `shell-rc`, `build.dockerfile`, `build.context`) resolve against the config dir.

## Templates and `extends`

- `extends = "name"` or `extends = ["a", "b"]`, on sandboxes *and* templates
  (recursive). Only templates can be extended. Cycles and unknown names error.
- Merge order: listed templates left-to-right, then the table's own body.
- Merge rules:
  - tables merge recursively (`containerEnv`, `build.args`, `customizations`…)
  - **arrays concatenate**, base first (`mounts`, `caches`, `services`,
    `shell-rc`, `customizations.vscode.extensions`…). There is no way to
    remove an inherited entry. Concatenation is what the two main use cases
    need:
    - VS Code extensions are inherited: a template's
      `customizations.vscode.extensions` stay installed when a sandbox adds
      its own.
    - Mounts are inherited: a template's `mounts` (agent binary,
      credentials, shared auth) reach every sandbox without restating them.
      A downstream table overrides an inherited mount by mounting the same
      target again: the last entry for a target wins, the earlier ones are
      dropped. A more specific target *inside* an inherited mount is not an
      override; both apply, in any order in the config: devsandbox mounts
      parents before the mounts nested inside them, on every runtime.
  - scalars: the later value wins.
- Diamond inheritance duplicates concatenated array entries (not deduped).
- After merging, the result must be a valid sandbox (below). Templates are
  only validated through the sandboxes that extend them.

## Sandbox properties

Keys are devcontainer camelCase, except devsandbox extras noted in kebab-case. **Unknown keys are a hard error**, so typos surface immediately.

### devsandbox extras

| key | type | meaning |
|---|---|---|
| `folder` | string | Host project dir (relative to config dir, must exist). Required by `run`. A second instance of the same folder gets a git worktree, as does an instance whose folder another instance already mounts as a direct `folders` entry. Refused when it covers a config root (`folder = "."`, or a config dir inside the repo): keep the config beside the repo. |
| `services` | [string] | Names from `[services.*]` to start and attach. |
| `caches` | [string] | Shared package caches: `pnpm`, `cargo`, `npm`, `yarn`, `go`, `pip`. Each = bind mount under `shared-volumes/<sub>` + env vars pointing the tool at it (targets are `/var/cache/devsandbox/<sub>`, outside any home, so they work for any `remoteUser`/`containerUser`; `cargo` sets `CARGO_HOME`, so `cargo install` lands in the shared cache's `bin/`, not on the image's `PATH`). Unknown name errors. |
| `persist-shell-history` | bool | Per-instance `shared-volumes/history/<id>/.zsh_history`, dir mounted at `/commandhistory`, `HISTFILE` set (env + rc line). Kept on `rm`. |
| `worktree-branch` | string | Branch pattern for worktree instances, `${…}` vars allowed. Default `sandbox/${instance}`. `run --branch` overrides. An existing local branch is checked out as is (`worktree-base` ignored); one only on `origin` becomes a local branch tracking it; else a new branch is created. A branch checked out in another worktree is an error. |
| `worktree-base` | string | Commit-ish start point (e.g. `origin/develop`). Default: remote's default branch. `run --base` overrides. |
| `worktree-include` | [string] | Gitignore-syntax patterns: gitignored, untracked files copied from the base repo into each worktree on `run` and again on every `rebuild` (never overwriting, so a copy deleted on purpose comes back on the next `rebuild`; `start` doesn't copy). Added after the repo's own `.worktreeinclude` (always honored), so `!pat` can negate it. Skipped, with a warning, for a config placed on a fleet worker. See `docs/architecture/instances/worktrees.md`. |
| `worktree-link` | [string] | Gitignored paths from the repo root (`.env` = root only; leading `./` ok) shared live by all instances. `*`/`?` glob within one segment (`packages/*/.env`), matched against existing paths in the base repo and store; no `**`/`[...]`. For each: the base repo's file is moved to `shared-files/<folder-basename>-<hash>/<path>` (one store per base folder, shared by every sandbox on it; links into old `shared-files/<sandbox>/` stores are skipped with a warning, migrate them by hand) and each tree gets an absolute symlink; that dir is mounted at its host path (works on Apple `container`). Tracked/non-ignored paths and existing conflicting files are skipped with a warning; `..`/absolute/`.git` entries error. Store kept on `rm`. Skipped, with a warning (no store made or mounted), for a config placed on a fleet worker. |
| `shell-rc` | [string] | Host shell snippets sourced by `~/.zshrc`/`~/.bashrc`. Must exist and be files (no auto-create). Parent dir mounted read-only at `/devsandbox/rc/<i>`, so a snippet directly in the config dir is refused (its parent is a config root): put it in a subdir. |
| `folders` | {container path = host dir \| `{ path, worktree = "auto"\|"always"\|"never" }`} | Extra VS Code workspace roots. Key must be absolute and differ from `workspaceFolder`; `path` (or the plain string) must exist, `${…}` vars allowed. `worktree` (default `"auto"`; unknown keys/values error): `"auto"` gives the instance its own git worktree when the dir is a git repo root and it's owned (some sandbox's `folder` in this config, or the base folder of any existing instance, from any config root), already directly mounted by another instance, or the instance is a dispatcher child; else a direct bind. `"always"`: worktree, error if not a git repo root. `"never"`: always the live host dir. Worktrees live in `.worktrees/<instance-id>.folders/`, are detached at the base checkout's `HEAD` (no branch created), and survive `rebuild` while the entry's key and path stay the same (under `"auto"` a rebuild keeps whichever mount the instance had; switching to `"always"`/`"never"` applies on the next `rebuild`); `worktree-include`/`worktree-link` don't apply to them. A host dir is refused like a `mounts` bind (a config root, …). |
| `autostart` | `true` \| `false` \| `"runtime"` | Once per host boot (per config root), the host daemon's start (`devsandbox serve`), the first `devsandbox run` / `start` or TUI launch, whichever comes first, starts this sandbox's stopped instances (full `start` path), or `run`s one if it has none; dispatcher children are never started and count as existing (a sandbox with only children gets no `run`); containerless instances are skipped with a note; while the runtime is unreachable the pass is skipped and retried on the next trigger. Any other value errors. `"runtime"` does the same and also creates the container with `--restart unless-stopped` on docker/podman (podman restarts at boot only with `podman-restart.service` enabled; devsandbox never installs it), so the runtime restarts the container itself. An in-container boot hook then brings back the helper daemon and re-runs `postStartCommand` (as `remoteUser`, in `workspaceFolder`, with `remoteEnv`; output appended to `/run/devsandbox/boot.log`) on every container start, so `devsandbox start` doesn't run it from the host; `run` still does at create. Host-side bridges, forwards and control resume once the host daemon runs: the first devsandbox command that needs it starts it, or the service manager does at login after `devsandbox serve install`. Containers created before the hook keep the old behavior and warn on `start`: recreate with `devsandbox rebuild --force <instance>`. Apple `container` has no restart policy: warning, behaves as `true`. Not hashed for drift: `start` updates an existing container's restart policy in place. |
| `dispatcher` | `{ spawn = [string], max-instances = int }` | Makes this sandbox a dispatcher: its instances may create/start/stop/remove *child* instances through the in-container control API (`devsbd ensure\|ls\|branches\|stop\|rm\|done\|exec\|run …`, served by the host daemon `devsandbox serve`, started on demand; see docs/automations-guide.md). `spawn`: sandboxes of this config root it may instantiate, `"*"` = every non-dispatcher sandbox of this root (granting the dispatcher their mounts, docker socket and other privileges); a sandbox that declares `dispatcher` is never spawnable, even when named (children can't be dispatchers; a request from a child is denied too); each other entry must name an existing sandbox (`devsandbox ls` errors otherwise); omitted = empty (nothing may be spawned). Arrays concatenate under `extends` like any other. `max-instances` (optional, default 10): cap on the dispatcher's children recorded in state (stopped ones count). A child's `--env K=V` (control API) may not name `PATH`, `HOME`, `SHELL`, `USER`, `ENV`, `BASH_ENV`, `IFS`, `CDPATH`, `PS4`, `PROMPT_COMMAND`, `SSH_AUTH_SOCK`, `TMPDIR`, `GCONV_PATH`, `NODE_OPTIONS`, `RUBYOPT` or anything starting `LD_`, `DYLD_`, `GIT_`, `PYTHON`, `PERL5` (case-insensitive; denied). Unknown keys error (e.g. `max_instances`). Children are ordinary instances named `<sandbox>-<key>` (key `[a-z0-9][a-z0-9-]{0,39}`), always on their own git worktree, labelled `devsandbox.dispatcher=<dispatcher instance id>` and recorded with that owner in `state.toml`; a dispatcher only reaches its own children. Children are kept when the dispatcher is stopped/removed/rebuilt. Re-read from config on every request and not hashed for drift. Grants only the child ops: Inbox threads and events need `inbox` (below). |
| `inbox` | bool | `true`: this sandbox's instances may own Inbox threads: `devsbd thread put\|send\|withdraw\|rm\|ls` and `devsbd events` / `events --follow` / `events ack` for their own threads (see docs/automations-guide.md, _Inbox threads_). Without it a thread put/send/withdraw/rm becomes an `error` notification (`thread put denied: …`, `thread send denied: …`, …) and `thread ls`/`events` are denied (exit 77); plain `devsbd notify` never needs it. Independent of `dispatcher`, which grants only child ops: a dispatcher that wants threads declares both, and a dispatcher's child may declare `inbox` to own threads of its own. A running instance with `inbox = true` keeps the host daemon up, like a dispatcher. Default unset (= `false`); a scalar, so under `extends` the most-derived value wins. Re-read from config on every message and request and not hashed for drift. |
| `profiles` | [string] | Names from `[profiles.*]` this sandbox's instances may be in, in preference order. Unset: every declared profile, in name order; none declared: no profiles. A name with no `[profiles.<name>]` errors. Arrays concatenate under `extends`, so a template listing `profiles` makes its sandboxes a collection; repeated names (after concatenation) are dropped, the first position kept. Not hashed for drift. |
| `access` | {kind = `{ scopes, ask, credential, repos, branches, as }`} | What this sandbox asks of this host's credential of each claim kind (`[sandbox.web.access.sentry]`; the kind a feature's claim names). `scopes` = [string]: the claim's scopes the sandbox's calls may use (unset or empty: every scope of the claim, within the credential's `max` grant); `ask` = [string]: scopes held for an answer instead of allowed (until permission requests exist, a scope listed here is refused, as is a route in no scope); `credential` = string: the credential (its name in `credentials.toml`, e.g. `"sentry-work"`, `[a-z0-9][a-z0-9_-]*`) to take over the one granted to the instance's profile; `repos` = [string] (under `github`; `owner/repo` or a GitHub URL of it): repos the sandbox's GitHub repo fence adds to the remotes its folders held at their first `run` (recorded once and never re-read, so a remote added later counts only once listed here; `devsandbox access forget <folder> <repo>` drops a recorded one); `branches` = [string], `as` = string: parsed, for GitHub. Kinds are lowercase letters, digits and `-`, starting with a letter. Unknown keys error. Arrays concatenate under `extends`, so a template's `scopes` reach every sandbox extending it and a sandbox can only add to them; repeats are dropped. `run` reads `credential` (a credential of another kind is refused); the broker reads `credential`, `scopes`, `ask` and `repos` on every call, so an edit applies to the next call without a rebuild (outside the fence a call goes without the credential, and a write is refused naming `repos`); `branches` and `as` aren't enforced yet. Not hashed for drift. |
| `ssh-keys` | [`{ key, destinations, signing-only }`] | The host agent's keys an instance with a git-host claim (a feature claim whose `git.hosts` names hosts, e.g. GitHub's) may use over the agent relay; without such a claim every key is relayed as before and this is unused. Unset: the sandbox sees no keys. `key` = string: the key as `ssh-add -l` prints it (`"SHA256:…"`) or its public key line (`"ssh-ed25519 AAAA… me@laptop"`, the comment ignored); exact match, so a certificate is named by its own line. `destinations` = [string]: the hosts it may log in to, as `known_hosts` names them (`"gitlab.example.com"`, `"[git.corp]:2222"`); it signs only an ssh login on a connection the client bound (`session-bind@openssh.com`, OpenSSH 8.9+) to a host key the destination has in **this host's** `known_hosts` (`ssh-keygen -F`, read per connection; never the container's), carrying that session's id. A destination missing from `known_hosts` refuses, naming it. Empty list errors. **A key named without `destinations`, or with a claimed git host among them, reopens GitHub over ssh**: anything in the sandbox can then push with it, past the repo fence (read-only fences don't apply over ssh). `signing-only` = bool (default false): used only for ssh signatures in namespace `git` (`gpg.format = ssh` commit signing), never a login; takes no `destinations`. Name a key twice to have both. Refusals show as "agent refused operation" in the sandbox, the reason in the instance's Inbox and `serve.log`. Unknown keys error. Arrays concatenate under `extends`. Read per agent connection, so an edit needs no rebuild. An instance with a git-host claim never gets the agent by bind mount (builds without the helper): `run` drops it with a warning. Not hashed for drift. |
| `placement` | `"home"` | **Experimental (fleet).** Pins the sandbox to the host whose config names it: `run --place` never moves it to another machine; it runs here, with ``note: not placed: `placement = "home"`; running here``. A run without `--place` never moves a sandbox anyway (`--machine` / `m/x` run that machine's own config there). Only `"home"` exists; any other value errors. A scalar, so under `extends` the most-derived value wins. Not hashed for drift. |
| `identity` | `"host"` \| `"image"` | Who you are inside. `"host"` (default): a user with the host uid/gid; the config's `remoteUser` (else `containerUser`, else the image's `USER`) is the account adopted, keeping its name, home and shell, only its uid/gid change. Nothing to adopt (root, `0` or no user): the sandbox runs as root, with a warning recommending the `common-utils` feature (no warning on rootless docker, where execs run as root anyway). Config entries that assume root in the container (paths under `/root`, a root `remoteUser`/`containerUser`) get a one-line warning from `run`, and on a terminal `run`, `start` and `rebuild` offer to fix them: update the config / continue as is (not asked again) / skip this time. `"image"`: users taken literally (`containerUser`/`remoteUser` as written), no remapping. Unset: `updateRemoteUserUID = false` means `"image"`; an explicit `identity` wins over it. Any other value errors. A scalar, so under `extends` the most-derived value wins. Hashed for drift. |
| `userns` | bool | Let unprivileged processes create user namespaces, which a browser's own sandbox (Chromium, Electron) needs. `true` (default): on docker, the container is created with devsandbox's seccomp profile (moby's default plus `clone`/`unshare` with `CLONE_NEWUSER`, and `clone` with `CLONE_NEWPID`, for processes without `CAP_SYS_ADMIN`); not with `privileged`, which drops seccomp. Podman's default already allows it; nothing on Apple `container`. `run` then checks in the container whether the browser sandbox works and sets `DEVSANDBOX_BROWSER_SANDBOX=1` on every exec if so, else the tools' sandbox-off knobs (see the `devsandbox-cli` skill, *Browsers in a sandbox*). The cost: unprivileged user namespaces open kernel attack surface docker's default closes. `false`: docker's default profile. A non-bool errors. A scalar, so under `extends` the most-derived value wins. Hashed for drift. |

### Implemented devcontainer properties

| key | notes |
|---|---|
| `image` | Base image. Exactly one of `image` / `build` is needed. |
| `build` | `{ dockerfile (required), context = ".", args = {}, target, cacheFrom = str\|[str], options = [str] }`. Tag `devsandbox-img-<root>-<sandbox>` (`<root>`: 8 hex chars of the config dir's hash). `cacheFrom` ignored with a warning on runtimes without support. |
| `features` | `{ "<ref>" = { opts } \| "version" \| true \| false }` (`false` skips it, handy to disable an inherited feature). `<ref>` is an OCI ref (`ghcr.io/devcontainers/features/node:1`) or a local folder starting with `./`, `../` or `/`, resolved against the config dir, holding `devcontainer-feature.json` + `install.sh` (e.g. `[template.base.features."./features/toolkit"]`); a missing folder or metadata file is flagged by `ls`. No https/tarball refs. Ordered by `installsAfter`, baked into `devsandbox-feat-<root>-<sandbox>`. Feature-declared `mounts` are applied, but a sandbox `mounts` entry on the same target replaces them, and one the host or runtime can't satisfy (missing bind source, except one under `${sharedVolumes}`, which is created; single-file bind on Apple `container`) is skipped with a warning, as is one still holding an unsupported `${…}` variable. A feature declaring `privileged: true` runs the container `--privileged` (skipped with a warning on Apple `container`, which has no such flag). Feature `entrypoint`s run at every container start, in install order, before the image's own `ENTRYPOINT` (a failing one warns in the logs and the rest continue; see `docs/architecture/instances/images-and-features.md`). Feature `capAdd` is ignored. A feature's credential claim (`customizations.devsandbox.credentials`) gets the sandbox a phantom token (on the env and in the files the claim names under the remote user's home, a file under a mount target skipped with a warning) and a loopback base URL standing for the service, both kept until `rm`, `rebuild` included; it takes this host's credential `access.<kind>.credential` names, else the one granted to the instance's profile (none: a warning naming `devsandbox login`, wired anyway). `run` refuses two features claiming one kind, and a claim version its credential hasn't approved (`devsandbox access approve <credential>`). Intercept-mode claims, and claims on instances placed by another fleet member, are skipped with a warning. |
| `workspaceFolder` | Container path of the working tree. Default `/workspaces/<folder basename>`. `${…}` vars allowed. |
| `mounts` | Docker shorthand `"source=…,target=…,type=bind[,readonly]"` or `{ type = "bind", source, target, readonly }`. Default type `bind` (needs `source`). One mount per target: the last entry wins (see merge rules). Missing bind sources are auto-created: a basename with a dot after any leading dots (`creds.json`) → file, otherwise → dir (`.zshrc` becomes a *dir*). A `volume` whose source uses `${instance}` / `${devcontainerId}` is per-instance: `rm` deletes it (`rebuild` keeps it); other named volumes are shared and never deleted. A bind (here, a feature's, `folder`, `folders`) or build context/dockerfile whose host path is the data dir's member key (`<data>/devsandbox/member.key`, `.next`, `.lock`) or any dir above it (`~`, `~/.local/share`, the data dir, `/`; symlinks followed) is refused on every config and trust level: bind a narrower path. So is a bind (read-only too) that covers a config root (this one, or any an existing instance came from: bind the files you need read-only, one bind each), a whole `shared-volumes/` dir (a subdir is fine), or covers or lies in another instance's worktree or per-instance bind source; and a `volume` that is another instance's per-instance volume. |
| `containerEnv` | `docker run -e`. Values `${…}`-substituted at create (beyond the devcontainer spec, which doesn't); there `${containerEnv:X}` reads only the image's env and `HOME`, never this `containerEnv`'s other keys, so `PATH = "${containerEnv:PATH}:/opt/bin"` extends the image's `PATH`. A key set here wins over devsandbox's own exec env (the browser knobs, a root plan's `PATH`). In a sandbox with a git-host claim, its own `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>` are kept, renumbered from 0, ahead of the claim's git settings, which apply after them (an entry missing its key or value is dropped). |
| `remoteEnv` | Applied on every `exec` and lifecycle command. Values `${…}`-substituted at create, `${containerEnv:X}` included. A key set here wins over devsandbox's own exec env (the browser knobs, a root plan's `PATH`). In a sandbox with an intercept credential claim, `HTTPS_PROXY`, `https_proxy`, `NO_PROXY` or `no_proxy` here (or in `run --env`) is warned of: every exec would go around the credential broker's proxy port. With a git-host claim, `GIT_CONFIG_*` here (or in `run --env`) is merged ahead of the claim's git settings as in `containerEnv`, never replacing them. Those settings (`url.*.insteadOf` for the ssh forms, `http.https://<host>/.proxy`, `.sslCAInfo`, `credential.https://<host>.helper`) are URL-scoped: git clients that skip URL-scoped proxies (JGit, go-git, dulwich) don't reach the git host through the broker; cargo is pointed at the git CLI (`CARGO_NET_GIT_FETCH_WITH_CLI=true`). |
| `containerUser` | Under `identity = "image"`: `docker run --user`. Under `"host"`: the account adopted when `remoteUser` is unset; the create user follows the runtime's mode (root on rootful and rootless docker, keep-id's user on rootless podman, `containerUser` on VM-backed runtimes). |
| `remoteUser` | Under `identity = "image"`: `exec -u` for `exec`, lifecycle commands, VS Code attach. Under `"host"`: the account adopted (its uid/gid fitted to the host's); every exec runs as it, except on rootless docker, where execs run as root (root is the host user) with `HOME` set to its home. `remoteEnv.HOME` overrides that `HOME`. |
| `updateRemoteUserUID` | bool; a hint for `identity` when that's unset: `false` means `identity = "image"`, `true` changes nothing. A non-bool errors. Hashed for drift. |
| `init` | `true` → `docker run --init`. |
| `privileged` | bool; `true` → `docker run --privileged`. OR-ed with any enabled feature's `privileged`, so `false` can't veto a feature that needs it. Skipped with a warning on Apple `container` (no such flag). |
| `hostRequirements.gpu` | `true` (required), `false` (none), `"optional"`, or a table `{ cores = int, memory = "8gb" }` (counts as required; `cores` / `memory` warned as not enforced, nothing else allowed in it). Every NVIDIA GPU, no device selection: docker `--gpus all` when its `nvidia` runtime is registered, else `--device nvidia.com/gpu=all` with an NVIDIA CDI spec (docker's `CDISpecDirs`; podman's `/etc/cdi`, `/var/run/cdi`); none on Apple `container`. Decided before anything is built: required with no usable GPU fails `run` / `rebuild` naming why; `"optional"` warns and runs without. On a fleet worker it needs `[allow] gpus = true` in its `member.toml` (required without it is refused; optional runs without). The other `hostRequirements` keys are warned (below). Hashed for drift. |
| `initializeCommand` | Runs on the **host**, cwd = config dir, before anything is created. |
| `onCreateCommand`, `updateContentCommand`, `postCreateCommand`, `postStartCommand`, `postAttachCommand` | Run in that order in the container after create (as the exec user `remoteUser` describes, in `workspaceFolder`, with `remoteEnv`). `start` re-runs only `postStartCommand`. |
| `customizations.vscode.extensions` | [string]; installed via VS Code (workspace recommendations on Apple `container`). |
| `forwardPorts` | [int \| string]; forwarded to `127.0.0.1` on the host by the host daemon (`devsandbox serve`) for each running instance, dashboard open or not (tunneled like `devsandbox port`, no `-p`). Entries `[host:][service:]port`: `3000` / `"3000"` (the instance's port), `"db:5432"` (a service's port; must be in the sandbox's `services`), and the devsandbox extensions `"8080:3000"` / `"15432:db:5432"` (host port to start from). A leading number is always the host port. The host port is the target port (or the given one) if free, else the next free one up (100 tried), skipping ports held by other instances' forwards; the choice is saved per instance in `state.toml` (`forwarded_ports`; global services once per config root) and reused next time. A `global` service's entry is forwarded once per config root however many instances declare it. Duplicate entries (after `extends` concatenation) are dropped. Anything else errors. Not hashed for drift. |

Lifecycle command forms: `"shell string"` (→ `sh -c`), `["argv", "..."]`, or a table `{ name = "cmd" | [argv] }` — the named form runs **sequentially**, in key order, not in parallel.

### Accepted but ignored (warned at `run`)

`name`, `appPort`, `portsAttributes`, `otherPortsAttributes`, `runArgs`, `workspaceMount`, `overrideFeatureInstallOrder`, `userEnvProbe`, `overrideCommand`, `shutdownAction`, `capAdd`, `securityOpt`, `hostRequirements` but `gpu` (each set key warned by name: `hostRequirements.gpu.cores`, `.gpu.memory`, `.cpus`, `.memory`, `.storage`), `waitFor`, `secrets`, `customizations.vscode.settings`, any other `customizations.vscode.*` or `customizations.<tool>`.

`hostRequirements` is typed: unknown keys error, `gpu` takes the forms above, `cpus` / `memory` / `storage` take any value. `extends` merges it like any table (a sandbox's `hostRequirements.gpu` overrides a template's, the template's other keys stay). It is hashed for drift.

Not accepted at all (hard error): compose keys (`dockerComposeFile`, `service`, `runServices`) — compose support was removed.

## `${…}` variables

Substituted in `mounts` (source/target), `workspaceFolder`, `worktree-branch`, `shell-rc`, `folders` (keys and values), `containerEnv` and `remoteEnv` (values). Unknown expressions are left verbatim. `${containerEnv:NAME}` only resolves at create, once the image is built: in `mounts` (a devsandbox extension; the devcontainer spec doesn't allow it there), `shell-rc`, feature mounts, `containerEnv` and `remoteEnv`; anywhere else, and in `ls`, it stays verbatim.

| var | value |
|---|---|
| `${configDir}` | absolute config dir |
| `${sharedVolumes}` | `${configDir}/shared-volumes` |
| `${localWorkspaceFolder}` | absolute host `folder` (the base repo, even for worktree instances) |
| `${localWorkspaceFolderBasename}` | its basename |
| `${instance}` | persistent instance id (name at creation; stable across renames) |
| `${devcontainerId}` | same as `${instance}` (devcontainer's stable per-container id; features name volumes with it) |
| `${localEnv:VAR}` | host env var, empty if unset |
| `${localEnv:VAR:default}` | host env var if set (even to empty), else `default`: everything after the `:` following `VAR`, `:`s included |
| `${containerEnv:NAME}` | the container's env: the sandbox's `containerEnv`, else the image's `ENV`, else for `HOME` the home of the account the container runs as (`remoteUser`, else `containerUser`, else the image's `USER`, as the image's `/etc/passwd` has it; else `/home/<user>`; `/root` for root or no user), the same under every runtime's plan; empty if unset (`${containerEnv:NAME:default}`: `default` if unset, as for `localEnv`). `target=${containerEnv:HOME}/.claude` lands in the user's home whoever that is |

## Services (`[services.<name>]`)

Unknown keys error. No `extends` for services.

| key | type | meaning |
|---|---|---|
| `scope` | `"isolated"` (default) \| `"global"` | isolated: one container per instance on a per-instance network; global: one container per config root. |
| `image` / `build` | as sandbox | exactly one required. |
| `env` | {string = string} | `-e` vars. |
| `ports` | [string] | `docker run -p` specs (`"5432"`, `"5432:5432"`, `"127.0.0.1:5432:5432"`). A bare container port (`"5432"`, `"53/udp"`, `"127.0.0.1::5432"`) gets the same host port when it's free, else one picked by the runtime. An explicit host port already published by another container errors. |
| `command` | string \| [string] | Appended after the image. A string is passed as **one** argv item, not shell-split. |
| `hostRequirements` | table | The sandbox key and forms (devsandbox extension: the spec has none for services). `gpu` acts as on a sandbox, decided before the service's image is built or pulled: required with no usable GPU fails the run naming the service, `"optional"` warns and runs without; on a fleet worker a required one needs `[allow] gpus = true`. The other keys are ignored, and not warned. Hashed for drift. |

Sandboxes reach a service by its name as hostname (network alias, or `/etc/hosts` on runtimes without aliases). `devsandbox gc` reaps unreferenced containers; `devsandbox service rebuild <name>` recreates and rewires in place, without restarting the sandboxes.

## Profiles (`[profiles.<name>]`)

Declares a profile: a named collection of sandboxes, whose members are the sandboxes listing it in `profiles` (or every sandbox that sets no `profiles`). The table takes no keys yet; any key errors. Names are `[a-z0-9][a-z0-9_.-]*`; anything else errors when the file loads. What a profile does on a host isn't set here.

Each instance is in one of its sandbox's allowed profiles, picked at `devsandbox run`: `--profile <name>` (refused unless allowed), else the one last chosen with `--profile` (or `devsandbox profile`) for that sandbox while it's still allowed, else the first allowed. A dispatcher's children are in the dispatcher's profile. `devsandbox profile <instance> <name>` switches an existing instance to another allowed profile without restarting it.

```toml
[profiles.work]
[profiles.personal]

[template.work]
profiles = ["work"]

[sandbox.api]
extends = "work"
```

## Drift

Each container is labelled with `config_hash` (hash of the merged sandbox table, or the service table) and `build_hash` (hash of the dockerfile *contents*). Any change to the merged config — including via a template — marks existing instances as drifted (`devsandbox rebuild <instance>`), except `autostart`, `dispatcher`, `inbox`, `placement`, `forwardPorts`, `profiles`, `access` and `ssh-keys`, which are stripped before hashing. A `folders` table that only restates the default (`{ path = "x" }`, `worktree = "auto"`) hashes like the plain string `"x"`; `"always"`/`"never"` count as drift. For a sandbox, `build_hash` also covers the contents of its enabled local-folder features (every file, by relative path), so editing a local feature's `install.sh` is drift too. The build context and OCI feature contents are not hashed.

## Checking a config

- `devsandbox -C <dir> ls` resolves and validates every sandbox (merge +
  types) without touching docker. It does not check that `folder`, `shell-rc`
  or `folders` paths exist, or that `services` names are defined — `run` does.
- The TUI config explorer shows both the original and the resolved (merged)
  table for a sandbox.

## Example

```toml
[template.base]
init = true
worktree-branch = "me/${instance}"
caches = ["pnpm", "cargo"]
persist-shell-history = true
mounts = ["source=${sharedVolumes}/gh,target=${containerEnv:HOME}/.config/gh,type=bind"]

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
