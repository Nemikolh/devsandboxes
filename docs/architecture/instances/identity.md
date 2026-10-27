# Identity

Who an instance runs as. The devcontainer user fields (`containerUser`,
`remoteUser`, a uid baked into the image) say how to pick a user, and the
same mechanism means opposite things on rootful docker, rootless docker,
rootless podman and VM-backed runtimes. So `run` decides the user from the
runtime's probed mode and the sandbox's `identity`, once per container, and
records the outcome. Read before touching `--user`, exec users, `HOME` or
bind labels.

Code: `src/runtime/identity.rs` (`Identity`, `Accounts`, `plan`, `Plan`,
`Plan::summary`, `Plan::label`), `src/commands/run/identity.rs`
(`plan_for`, `decide_plan`, `create_args`, `apply_argv`, `apply`,
`apply_outcome`, `socket_gid`, `volume_targets`), `src/commands/run/mod.rs`
(`materialize`, `run_container`, `IDENTITY_LABEL`), `src/state.rs`
(`Instance::remote_user`, `Instance::home`, `Instance::exec_env`),
`src/config.rs` (`SandboxProperties::identity`, `SandboxProperties::userns`),
`src/runtime/seccomp.rs` (`profile`, `ensure_file`), `devsbd/src/identity.rs`.

## The model

The sandbox's `identity` (`SandboxProperties::identity`):

- `"host"` (default): inside you are a user with the host's uid/gid. The
  **adopted account** (`Accounts::adopted`) is `remoteUser`, else
  `containerUser`, else the image's `USER` (`Backend::image_user`, read
  only when neither field is set), as a name (a `user:group` spec loses its
  group). It keeps its name, home and shell. root, `0` or nothing means
  there's no account to adopt: the plan runs as root and warns,
  recommending the `common-utils` feature (not under rootless docker,
  where root is the plan anyway).
- `"image"`: users taken literally. Created as `containerUser`, exec'd as
  `remoteUser` (else `containerUser`), nothing remapped.

Unset, `updateRemoteUserUID = false` means `"image"`. The image build
(`generate_dockerfile`, `builtin_env`) still bakes `USER containerUser`;
the create-time `--user` overrides it.

## The plan

`runtime::identity::plan(probe, identity, accounts, host_ids)` is pure and
table-tested. The probe is the runtime context's mode and SELinux flag
([runtimes](../runtimes.md#mode-detection)). Per mode, with
`identity = "host"` and an account `A` (ids assumed 1000:1000,
`identity::ASSUMED`):

| Mode | Create (`run`) | `devsbd identity apply A` | Exec as | `HOME` |
|---|---|---|---|---|
| rootful | `--user root` | `--remap <host uid>:<host gid>` | `A` | `A`'s own |
| rootless docker | `--user root` | `--root-home` (home chowned to 0) | root | `A`'s home |
| rootless podman | `--userns keep-id:uid=1000,gid=1000` | `--expect 1000:1000` (warns when `A` isn't) | `A` | `A`'s own |
| VM-backed | `--user containerUser` when set | no mode flag | `A` | `A`'s own |
| any, `identity = "image"` | `--user containerUser` when set | not run | `remoteUser`, else `containerUser` | the image's |

On every plan, SELinux enforcing (`Plan::relabel`) relabels binds
(`create_args`): each `-v` (workspace, git companion, `worktree-link`
store, an ssh-agent bind) gets `z`; a `--mount` bind gets `relabel=shared`
on podman. docker's `--mount` has no relabel option, so docker binds given
as `--mount` are named in one warning instead.

`run` prints `Plan::summary` once (`identity: rootful: exec as vscode,
remapped to 1000:1000`), then the plan's warnings and any fallback.

## Tool environment

Chromium's sandbox (and Electron's) needs a non-root euid and the right
to create a user namespace. Docker's default seccomp profile blocks
`clone`/`unshare` with `CLONE_NEWUSER` unless `CAP_SYS_ADMIN` is held, and
Chrome's zygote also calls `clone(CLONE_NEWPID)` from inside its new
userns, which the profile blocks too. Podman's default profile allows
both.

So on docker `run` creates the container with devsandbox's own profile
(`runtime::seccomp::profile`): moby's default, vendored verbatim and
pinned by file name (`src/runtime/seccomp/moby-default-v0.2.4.json`,
`moby/profiles` `seccomp/v0.2.4`), plus two rules (`RULES`) shaped like
moby's own `clone` rule: `clone`/`unshare` with `CLONE_NEWUSER`, and
`clone` with `CLONE_NEWPID`, both excluded when `CAP_SYS_ADMIN` is held
(moby's rule allows everything then) and on s390. The kernel still wants
`CAP_SYS_ADMIN` in the owning userns for a new pid namespace, so the second
rule only takes effect inside a new userns. `clone3` stays ENOSYS (glibc
falls back to `clone`). Docker's default AppArmor profile is kept: with
AppArmor off, Ubuntu's `kernel.apparmor_restrict_unprivileged_userns`
strips the new userns of its capabilities.

`seccomp::ensure_file` writes the profile next to `state.toml` as
`seccomp-<short_hash of the profile>.json` when missing (temp file +
rename), and `create_args` passes `--security-opt seccomp=<path>` after
`--userns`. The docker CLI reads the file itself, so a remote daemon needs
nothing. `plan_for` decides it (`wants_seccomp`): docker only, the
sandbox's `userns` on (`SandboxProperties::userns`, default `true`), and
not `--privileged`, which drops seccomp anyway. The decision is
`Plan::seccomp`, recorded as the bare `seccomp` field of the label and
said in the summary (`, user namespaces allowed (seccomp)`). A profile
that can't be written is a warning, and the container gets docker's
default.

The cost: unprivileged user namespaces open kernel attack surface that
docker's default closes, the same trade podman's default makes. `userns =
false` keeps docker's default profile. The field changes create args, so
it's hashed for drift.

## Applying it

In `materialize`, after the helper is installed and before shell-rc and the
lifecycle chain, a root exec runs `devsbd identity apply` (`apply_argv`,
contract in
[in-container](../helper/in-container.md#devsbd-identity-apply)) when the
plan adopts an account under `identity = "host"`. Besides the mode flag:

- `--socket-gid <gid>` when a `--mount` bind's source is the docker
  socket (`/var/run/docker.sock`, `/run/docker.sock`, or `DOCKER_HOST`'s
  `unix://` path), with the gid the host `stat`s. Rootful only: under a
  user namespace or a VM's share the host gid means nothing inside.
- `--volume <target>` for each named-volume `--mount`, feature mounts
  included: empty root-owned mount points go to the account.

`apply_outcome` reads the one stdout line `home=<path>` and turns the
helper's `devsbd: identity: warning: …` lines into `warning: identity: …`.
A failed apply fails the create: the instance is kept as `failed`
([lifecycle](lifecycle.md#phases-and-failure)). The edits live in the
container layer, so `start` doesn't rerun it; `rebuild` does, on the new
container.

## What's recorded

- The container label `devsandbox.identity` (`IDENTITY_LABEL`):
  `Plan::label`, `;`-separated `key=value` fields
  (`mode=rootful;identity=host;account=vscode;create=root;exec=vscode;remap=vscode:1000:1000`).
  `inspect` shows it in the runtime's JSON.
- `Instance::remote_user`: the plan's exec user (`Plan::exec_user`), the
  `-u` of every exec: `exec_argv_with` (CLI, dashboard terminal, dispatcher
  run ops), the lifecycle chain (`exec_lifecycle`), the boot file
  (`boot_spec_for`), `start`'s `postStartCommand`, and the VS Code name
  config (`write_vscode_name_config`, from `run` and `vscode::launch`).
- `Instance::home`: the home `apply` reported, on root-home plans only.
  `Instance::exec_env` puts it first as `HOME`, so `remoteEnv` and
  `run --env` override it.

An instance created before identity has no label and keeps the
`remote_user` its `run` recorded (the config's `remoteUser`) until
rebuilt.

## Fallbacks

`decide_plan` never fails a `run`:

- the probe fails (an image without `/bin/sh` on an ambiguous engine, a
  remote `DOCKER_HOST`): a warning, the mode is `Unknown`, and the plan is
  `identity = "image"` with nothing relabeled;
- the build has no embedded helper (`cargo install`): a note, and
  `"image"` (nothing could remap);
- the host ids can't be read on a rootful plan: a warning, and `"image"`;
- the helper is embedded but fails to install in the container: the plan
  stands (the container was already created with it), a warning says the
  account couldn't be fitted, and the create goes on.

## Services

Services get no identity treatment: `service_run_args` emits no `--user`,
`--userns` or mounts (guarded by
`service_run_args_have_no_identity_flags`). They reach sandboxes only over
bridge networks, so they share no files or user namespace with them.
