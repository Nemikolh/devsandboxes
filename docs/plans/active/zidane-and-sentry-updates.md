# zidane and sentry updates

Status: agreed; building.

## Goal

The zidane and sentry features update like gh
([ADR 0059](../../decisions/0059-feature-updates-run-in-the-features-image.md)):
the feature declares a read-only `${sharedVolumes}` bin dir and an update
command baked in its image; devsandbox's daemon runs it on schedule in a
throwaway container; no sandbox can write the binary the others run.

Today:

- zidane mounts `${sharedVolumes}/zidane-bin` read-write: any sandbox's
  `zidane-update` (or `zidane upgrade`) changes the binary every sandbox
  runs, and the launcher seeds the dir from the image on first run.
- sentry has no shared dir: sentry-cli is the image's, changed only by a
  rebuild.

All the work is in github.com/devsandboxes/features; the devsandbox repo
only mirrors the sentry fixture. gh's files (`src/gh/gh-update.sh`,
`gh-launcher.sh`, `install.sh`, `devcontainer-feature.json`, README's
*Updating*) are the model.

## Step 1: zidane

- `devcontainer-feature.json`: the `zidane-bin` mount `readonly: true`;
  `customizations.devsandbox.updates: [{ "name": "zidane", "mount":
  "/var/lib/devsandbox/zidane/bin", "command": [<updater>, "/var/lib/devsandbox/zidane/bin"] }]`;
  version 2.1.0 (pre-1.0 rule: break freely, so `zidane:2` users get it).
- `zidane-update.sh`: takes the target dir as its argument like
  `gh-update` (install into it, `current`, keep new + previous), still
  `--bake` for the image; installed where `install.sh` doesn't wipe it.
- `zidane-launcher.sh`: no seeding (the dir is read-only); a writable dir
  runs the image's copy and says so (as gh's launcher); `zidane upgrade` in
  a sandbox says updates are devsandbox's (`devsandbox update zidane` on
  the host) instead of running `zidane-update`; `--check` still runs
  zidane's own. No `zidane-update` on `PATH` in the sandbox.
- README: *Updating* like gh's; the "writable by every sandbox" paragraph
  goes (skills and extensions dirs stay shared and writable: they're
  config, not binaries; say so in one line).
- `test/zidane/test.sh` adjusted.

## Step 2: sentry

- New `sentry-update.sh` (fetch from Sentry's release registry with the
  sha256 check `install.sh` does today, into `<dir>/<version>/`,
  `current`, keep new + previous; `--bake` for the image) and
  `sentry-launcher.sh` as `/usr/local/bin/sentry-cli` (shared `current`
  when the dir is read-only, else the image's copy).
- `devcontainer-feature.json`: read-only `${sharedVolumes}/sentry-bin` →
  `/var/lib/devsandbox/sentry/bin`, `updates: [{ "name": "sentry", … }]`,
  version 1.1.0. `SENTRY_DISABLE_UPDATE_CHECK` stays.
- README: *Updating* like gh's.
- `test/sentry/test.sh` adjusted.
- devsandbox repo: `tests/fixtures/features/sentry/devcontainer-feature.json`
  mirrors the metadata (customizations, mounts, version).

## Step 3: publish

Top-level README's table rows; shellcheck if available,
`scripts/test-feature.sh` for both; commit and push the features repo.

## Closing

devsandbox docs naming zidane's in-sandbox update checked; plan archived.
