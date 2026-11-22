# Images and features

How a sandbox's image is produced: the base image or build, the derived
feature image with its generated Dockerfile and entrypoint chain, and what
features ask of the container. Read before touching image builds or
devcontainer `features` support.

Code: `src/commands/run/image.rs` (`image_for`, `base_image_for`,
`sandbox_image`, `features_image`,
`build_features_image`, `generate_dockerfile`, `install_wrapper`,
`feature_env_file`, `builtin_env`, `entrypoint_wrapper`,
`entrypoint_dockerfile`, `feature_container_opts`, `privileged_decision`),
`src/features.rs` (`FeatureSource`, `resolve`, `fetch_into`,
`install_order`, `feature_issues`, `local_features_fingerprint`,
`build_context_dir`, `build_context_name`, `FeatureMetadata`),
`src/commands/services.rs` (`build_args`, `project_id`), `src/commands/run/mod.rs` (`container_command`),
`src/build_log.rs`, `src/commands/run/claims.rs` (`wire`,
`warn_foreign_files`), `src/credentials/instance.rs`.

## Base image

`base_image_for`: `image` as-is, or `build` (requires `build.dockerfile`)
built with `services::build_args` (the same argv builder services use:
`-f`, `--build-arg`, `--target`, `--cache-from` where the runtime supports
it, `build.options` verbatim, context default `.` relative to the config
dir) and tagged
`devsandbox-img-<project>-<sandbox>` (`sandbox_image`). The build runs on
every `run`/`rebuild`; the runtime's layer cache makes it cheap.

`<project>` is the config root's `services::project_id` (8 hex chars of
the canonical root path's hash, the same key that scopes networks and
service containers), so two roots with a same-named sandbox (two
checkouts of one config, a control plane's config next to a local one) build into
tags and build contexts of their own.

## Feature image

When the sandbox has enabled `features`, `image_for` builds a derived image
`devsandbox-feat-<project>-<sandbox>` (`features_image`) on top of the
base. Its own `feat-` head keeps it apart from every sandbox's
`img-<project>-` build tag and every service's `img-svc-` one
(`sandbox_image` has the argument), where a `-feat` suffix would give
sandbox `foo-feat`'s build and sandbox `foo`'s feature image one tag. Layout mirrors the
official devcontainers CLI.

1. Each entry: options from the config value (`true`, a version string, or a
   scalar table; `false` skips it), `FeatureSource::parse`, `resolve`.
2. `install_order`: topological sort on `installsAfter` (matched by ref
   without tag, else bare id; absent deps ignored; cycles error),
   alphabetical tie-break.
3. A fresh build context at
   `<cache>/devsandbox/features/build-<project>-<sandbox>/`
   (`build_context_name`): the builtin env
   (`_CONTAINER_USER`, `_REMOTE_USER`; `_*_HOME` appended in the image), and
   per feature `<idx>-<id>/` with its files, `devcontainer-features.env`
   (metadata defaults overridden by config values, names via `safe_id`) and
   `devcontainer-features-install.sh` (banner, sources both env files,
   runs `install.sh`).
4. `generate_dockerfile`: `FROM <base>`, `USER root`, per feature its
   `containerEnv` as `ENV`, `COPY` and `RUN` the wrapper; `USER
   <containerUser>` last if set. `_REMOTE_USER` is `remoteUser`, else
   `containerUser`, else `root` (no image-user introspection).

## Feature sources and cache

- **OCI** (`ghcr.io/…/node:1`): fetched with `curl` (anonymous bearer
  token, manifest, the `application/vnd.devcontainers.layer.v1+tar` layer),
  extracted via a temp dir renamed into
  `<cache>/devsandbox/features/<registry>/<path>/<tag>/`
  (`$XDG_CACHE_HOME`, else `~/.cache`). A populated cache entry is reused
  as-is: mutable tags don't refresh until the dir is cleared.
- **Local** (ref starting `./`, `../` or `/`): a folder with the same
  layout, resolved against the config dir, read in place, no cache.

`feature_issues` reports bad refs and missing local folders without network
(for `ls` and the dashboard).

## Entrypoint chain

Feature `entrypoint`s are baked at build time. If any enabled feature has
one, a second metadata-only build on the same tag reads the image's own
`ENTRYPOINT` (`Backend::image_entrypoint`, since a Dockerfile can't reference
its parent's), copies `devsandbox-entrypoint.sh` to
`/usr/local/share/devsandbox-entrypoint.sh`, and sets `ENTRYPOINT
["/bin/sh", <wrapper>, <image entrypoint…>]`. The wrapper runs each feature
entrypoint in install order (a failure prints a warning to the container log
and continues), then `exec "$@"`: the image's own entrypoint, then the
container command.

The container command is always
`sh -c '[ -x <devsbd> ] && <devsbd> boot & exec sleep infinity'`
(`container_command`): it keeps the container alive and, once the helper is
installed, runs its boot hook on every start. The `devsbd` helper is not
baked into any image; `materialize` installs it by `exec` after the
container is created ([helper](../helper/)). The image's entrypoint must
`exec "$@"` or `sleep infinity` never runs.

Differences from the devcontainers CLI, which builds the chain at run time
(`docker run --entrypoint /bin/sh -c '<script>'`): the image's own
`ENTRYPOINT` always runs after the feature entrypoints (the CLI drops it
unless `overrideCommand: false`; devsandbox ignores `overrideCommand`), and
a failing feature entrypoint logs a warning instead of continuing silently.
Order (install order) and the lack of arguments are the same.

## What features ask of the container

`feature_container_opts`, at `run`/`rebuild`:

- `mounts`: `${…}` substituted (`${devcontainerId}` = instance id, so a
  volume named with it is per-instance and `rm` deletes it). A sandbox mount
  on the same target wins. A missing bind source under `${sharedVolumes}` is
  created like a config mount's (`creatable_feature_source`: devsandbox's own
  state, e.g. a feature's per-instance dirs). Skipped with a warning: unknown
  variable, any other missing bind source, a file bind on a runtime without
  file binds.
- `privileged`: OR-ed with the sandbox's `privileged` (`false` can't veto a
  feature). Skipped with a warning where the runtime has no `--privileged`.

- `customizations.devsandbox.credentials`: the feature's credential
  claims (`FeatureMetadata::credentials`), collected for `run`
  ([below](#credential-claims)).
- `customizations.devsandbox.updates`: the feature's updates
  (`FeatureMetadata::updates`, `updates::from_customizations`), a list of
  `{ "name", "mount", "command", "every" }`. `name` is
  `[a-z0-9][a-z0-9-]*`, at most 32 bytes, the key the config's `updates`
  table and the CLI use, so one namespace across a sandbox's features
  (`updates::check_unique`). `mount` is the target of one of the feature's
  own `mounts`, which must be a read-only bind of a dir under
  `${sharedVolumes}/` (no `..`): the one dir the update writes, read-only
  in every sandbox. `command` is an argv run in the feature's image,
  `command[0]` an absolute path. `every` is `<n>h`, `<n>d` or `<n>w`
  (`updates::parse_duration`), default `7d` (`updates::DEFAULT_EVERY`).
  Unknown fields, a bad entry or a name declared twice fail the feature's
  parse.

Other metadata (`capAdd`, `init`, `dependsOn`, …) is not applied.

## Credential claims

What a feature's claim (`credentials::claim`; the contract is
[github-and-sentry-access](../../plans/active/github-and-sentry-access.md#the-claim-contract))
turns into on the instance, in `materialize` once the identity plan is
known (`claims::wire`, the pure parts in `credentials::instance`):

- **Which apply** (`instance::select`): a second feature claiming a kind
  another feature claims is refused, naming both, and so is a second
  `intercept` claim of an upstream another intercept claim has (the broker
  picks a connection's claim by its host), naming both features. On an
  instance placed here by another member
  (its config root under the fleet dir, `worker_policy::fleet_root_of`)
  every claim is skipped with a warning, and off unix too (no daemon).
- **The credential** (`home::resolve` over `credentials.toml`): the one
  `access.<kind>.credential` names, else the one of that kind granted to
  the instance's profile; several granted is refused, naming them. None
  is a warning naming `devsandbox login`: the claim is wired anyway, so a
  later login needs no rebuild. One that hasn't approved the claim's
  version gets it recorded pending (`home::record_pending`) and the run is
  refused, naming `devsandbox access approve <credential>`. Which
  credential serves a claim isn't kept: the broker resolves it per call.
- **Phantoms and ports** (`instance::plan`): per claim a phantom, the
  claim's `phantom.prefix` (else `dsbx_<kind>_`) and 24 random base62
  characters (`mint_phantom`), and container-loopback ports from
  `instance::PORTS` (29000-29999: below the ephemeral range and
  Kubernetes' NodePorts, above dev-server defaults; per container, so
  instances never contend), lowest free first: one per upstream of each
  `base-url` claim, and one shared proxy port (`instance::ProxyPort`,
  saved as the instance's `intercept`, `instance::Intercept`) whose hosts
  are every `intercept` claim's upstreams but the ones its `git` field
  names, which go on git's own proxy port (`Intercept::git`, one more
  `ProxyPort`; [git](../credentials/git.md)). With only git hosts
  intercepted, the shared port binds none. Intercept claims get no
  listener, but a claim pinning `lfs` gives the instance the LFS
  transfers' listener (`Intercept::lfs`, broker file `listen <port> lfs`;
  [git](../credentials/git.md#lfs)). A `rebuild` keeps each kind's phantom (while it has the
  claim's head), each upstream's port, both proxy ports and the LFS listener; a new one
  takes a free port. The helper's broker file gets each proxy port as
  `proxy-listen` and each git host's phantom as `credential`
  (`instance::broker_spec`). The instance's `incarnation`
  (`mint_incarnation`) is minted at create and kept by `rebuild`.
- **Env** (`instance::env`, `-e` on the container): each `phantom.env`
  variable is the phantom, `baseUrl`'s is
  `http://127.0.0.1:<the first upstream's port>`. With a shared proxy port
  (`instance::intercept_env`):

  | Variable | Value |
  | --- | --- |
  | `HTTPS_PROXY`, `https_proxy` | `http://127.0.0.1:<shared port>` |
  | `NO_PROXY`, `no_proxy` | `localhost,127.0.0.1,::1`, the instance's services' aliases, then the container's own `NO_PROXY` and `no_proxy` entries, minus every entry that would exempt a bound host (`instance::no_proxy`) |
  | `NODE_USE_ENV_PROXY` | `1` |
  | `NODE_EXTRA_CA_CERTS`, `DENO_CERT`, `REQUESTS_CA_BUNDLE`, `AWS_CA_BUNDLE` (`instance::CA_VARS`) | the image's system bundle |
  | each intercept claim's `ca.env` | the image's system bundle |
  | `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>` (with git hosts) | the image's, `containerEnv`'s and `--env`'s own entries, renumbered from 0, then git's settings for each git host ([git](../credentials/git.md#settings)) |
  | `CARGO_NET_GIT_FETCH_WITH_CLI` (with git hosts) | `true` |

  No `HTTP_PROXY` (plain HTTP is never intercepted) and no
  `SSL_CERT_FILE` (OpenSSL, libcurl, Go and Bun read the system bundle).
  The system bundle is probed from the built image before create
  (`claims::probe_bundle`, `credentials::trust::PROBE_SCRIPT`, a
  throwaway container like `image_passwd`'s):
  `/etc/ssl/certs/ca-certificates.crt` (Debian, Ubuntu, Alpine),
  `/etc/pki/tls/certs/ca-bundle.crt` (Fedora, RHEL),
  `/etc/ssl/ca-bundle.pem` (openSUSE); none, or a probe that can't run,
  is the Debian path, where the install writes one. The proxy env the
  container would have had (`instance::original_proxy_env`: the image's
  `ENV`, then `containerEnv`, then `--env`, each variable's last value)
  is saved on the `intercept` and written to the broker file as its
  `proxy` lines, how devsbd dials the targets it doesn't hand the broker;
  never the `HTTPS_PROXY` set here, which would loop. A `remoteEnv` or
  `--env` setting `HTTPS_PROXY`, `https_proxy`, `NO_PROXY` or `no_proxy`
  is warned of (`instance::proxy_overrides`): set on every exec, it sends
  that exec around the shared port.
- **The member CA** (`claims::member_cert`): before create,
  `MemberCa::ensure_hosts` covers both proxy ports' hosts
  (`Intercept::bound_hosts`), reissuing the
  cert when they're new to it; once the container exists and before
  lifecycle commands, `claims::install_trust` installs it
  ([ca](../credentials/ca.md#trust-in-containers)) and records its
  fingerprint as the `intercept`'s `installed`. When this run reissued the
  cert, `claims::reinstall_running` puts it into every other running
  instance with an `intercept` that lacks it. Failures are warnings: the
  run goes on, TLS to the claimed hosts failing.
- **git** (with git hosts): the system gitconfig copy right after the CA
  install, and the boot check before and after the lifecycle commands
  ([git](../credentials/git.md)).
- **Phantom files** (`instance::render_files`): each template with
  `{phantom}` and `{baseUrl}` filled, at its path under the remote user's
  home (`facts::home`), owned by the exec user. One on or under a mount or
  volume target (`claims::mount_targets`: the workspace, the volumes, the
  `--mount`s) is skipped with a warning: a login shared through a mount
  would be overwritten. After the daemon starts, `claims::warn_foreign_files`
  warns of each one devsbd leaves alone because something else is at its
  path.

The records (`credentials::instance::InstanceClaim`: kind, feature,
`version`, the canonical pinned `claim`, `phantom`, `listeners`,
`files`) go on `Instance::claims`; `instance::broker_spec` turns them
into the helper's broker file, written after the helper's install and
before its daemon starts (`devsbd::prepare`, then `devsbd::start_daemon`
after `devsbd identity apply`, so the files get the account's final ids;
at `start`, a bridge's heal and a forward's resolve through
`devsbd::ensure_recorded`, as `/run` may be a tmpfs:
[in-container](../helper/in-container.md#broker-listeners)). The VS Code
name config marks the listener and proxy ports `onAutoForward: ignore`,
and `forwards.*` refuses them (`instance::ports`, `forward_refusal`;
[forwards](../daemon/forwards.md)).

## Drift and logs

The `build_hash` label covers the build dockerfile's contents plus enabled
local features' files; OCI feature contents and the build context are not
hashed ([rebuild and drift](rebuild-and-drift.md)).

Build output, like everything `run`/`rebuild`/`start` prints, is teed into
`<data-dir>/devsandbox/logs/<instance_id>/build.log` (previous one rotated to
`build.1.log`), readable with `devsandbox logs --build`. The tee is only
enabled by `main` for those verbs (`build_log::enable`), which includes the
daemon's `--in-process` verb subprocesses; calls made inside the `serve`
process itself (its autostart pass) don't tee.

Limitations: [status.md](../../status.md#instances)
