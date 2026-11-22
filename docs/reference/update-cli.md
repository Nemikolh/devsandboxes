# `devsandbox update`

Run a feature's update now: the command a feature bakes in its image to
refresh a `${sharedVolumes}` dir its sandboxes mount read-only (a tool's
binary, e.g. gh's). Read when scripting updates or changing the verb; what
a feature declares is in
[images-and-features](../architecture/instances/images-and-features.md#running-an-update).

Code: `src/main.rs` (`Command::Update`), `src/commands/update.rs`
(`update`, `images`, `choose`, `refusal`, `declares`, `newest`,
`host_dir`, `update_argv`), `src/updates.rs` (`from_label`).

```
devsandbox [-C <dir>] update <name> [args…]
```

`<name>` is the update's name as its feature declares it
(`updates::check_name`: `[a-z0-9][a-z0-9-]*`, at most 32 bytes). `args`
are appended to the update's `command` (a version, to pin or roll back:
`devsandbox update gh 2.60.0`); anything after `<name>` goes there,
flags included.

## Scheduled runs

The host daemon runs the same verb on its own, with no args: when it
starts and then hourly, each update the features image of a sandbox with
an instance lists,
once its interval has passed since its last success. The interval is the
config's top-level `updates` table (`gh = "1d"`, or `"never"` to never
run it on schedule), else the `every` the image was built with (default
`7d`). A
failure is retried after 6 h; its first in a row posts an Inbox notice,
and each run's output is in `logs/update-<unix>-<name>.log` next to
`state.toml`. Detail:
[daemon/updates.md](../architecture/daemon/updates.md). Running the
verb by hand works whatever the schedule says, and a success counts for
it.

## Which image

An image runs the updates it was built with: each features image carries
them in its `devsandbox.updates` label
([images-and-features](../architecture/instances/images-and-features.md#feature-image)).
For the config at `-C` (`update` → `images`), each sandbox's local
features image (`run::features_image`) is inspected
(`Backend::image_labels`) and its label checked like a feature's
declaration (`updates::from_label`); a label that doesn't pass is a
``warning: sandbox `<name>`: …`` line and lists nothing. Among the images
whose label lists `<name>`, the newest by `Backend::image_created` runs
it (`choose` → `newest`; an unknown date ranks below any known one, a tie
goes to the first sandbox in name order), with the `mount`, `source` and
`command` of that label, whatever the feature declares now.

No image lists it (`refusal`): the sandboxes' enabled `features` are
resolved (fetched into the cache like at `run`, `declares`) to say why.
Some declare `<name>` and have a built image: ``update `<name>`: the
image of sandbox `a` was built without it; `devsandbox rebuild a` adds
it`` (several: `devsandbox rebuild <sandbox>`). Some declare it, none
built: ``update `<name>`: no image of sandbox `a`, `b` is built yet;
`devsandbox run <sandbox>` builds it``. None declares it: ``no sandbox of
this config has a feature declaring update `<name>` ``.

## The container

`host_dir` resolves the label's `source` (the source of the feature's
mount at the update's `mount` when the image was built) with `${sharedVolumes}` = `<config root>/shared-volumes` and no
instance (a source naming `${instance}` / `${devcontainerId}` is refused:
every instance shares the dir), checks it lies under `shared-volumes/`
(`run::creatable_feature_source`), and creates it when missing, past a
worker policy's check (`run::guard_create`, `run::provision_bind_source`).

`update_argv`: `run --rm --pull=never`, the sandbox's identity plan
(`run::plan_for` with that image's facts, never privileged, then
`run::create_args`: the identity label, `--user`, `--userns`, the seccomp
profile, the dir's `--mount` read-write and relabeled per the plan), the
default network, `--entrypoint <command[0]>` (so the image's entrypoint
chain doesn't run), the image, `command[1..]`, then `args`. The argv
passes the guards a sandbox's `run` does (`key_guard::check_argv`,
`run::check_bind_argv` with no instance of its own,
`worker_policy::enforce_argv`). Its output goes to the terminal.

On exit 0 the time goes to `state.toml` as
`State.updates[<project id>][<name>].ok` (unix seconds, `State::update`);
any other exit is ``update `<name>` failed (exit N)`` and records
nothing. The config's `updates` table isn't read here: an interval is
the schedule's, not the verb's.

`update` runs against this host's config only: `--machine` refuses it
(`machines::cli::machine_use`), and so does Apple `container`.
