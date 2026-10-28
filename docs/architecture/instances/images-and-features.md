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
`src/build_log.rs`.

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
checkouts of one config, a hub's config next to a local one) build into
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

Other metadata (`capAdd`, `init`, `dependsOn`, …) is not applied.

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
