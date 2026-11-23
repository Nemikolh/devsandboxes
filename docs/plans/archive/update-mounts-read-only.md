> Archived 2026-11-23. Historical, may be wrong; do not use as a reference.
> Current: [images and features](../../architecture/instances/images-and-features.md),
> [daemon/updates.md](../../architecture/daemon/updates.md)

# Update mounts are read-only by declaration

Status: agreed; building.

## Goal

The features repo's CI (`devcontainers/action` validate, in both `test`
and `release`) refuses gh, zidane and sentry: the dev container feature
schema's `Mount` (`type`, `source`, `target`, nothing else) has no
`readonly`, and devsandbox requires the update's mount to say
`readonly: true` (`updates::from_customizations`, `src/updates.rs`).

Instead, declaring an update makes its mount read-only: devsandbox mounts
the bind an update names read-only in every sandbox, whatever the mount
says. The feature's `mounts` stay spec-valid.

## Step 1: devsandbox forces an update's mount read-only

- `src/updates.rs`: the update's `mount` must still be one of the
  feature's own binds under `${sharedVolumes}/` (no `..`, no
  `${instance}`), but no longer has to say `readonly`.
- `src/commands/run/image.rs` `feature_container_opts` (the feature
  mounts loop): a feature mount whose target an update of the same
  feature names is resolved with `readonly = true`.
- Tests: the "must be mounted read-only" refusal goes; a test that the
  update's mount reaches the container argv read-only though the feature
  didn't say so; fixtures (`tests/fixtures/features/{github,sentry}`, the
  inline ones in `updates.rs` and `image.rs` tests) drop `readonly`.
- Docs: `docs/architecture/instances/images-and-features.md`,
  `docs/architecture/daemon/updates.md`.

## Step 2: the features drop `readonly`

github.com/devsandboxes/features: `src/{gh,zidane,sentry}/devcontainer-feature.json`
lose `"readonly": true` (versions bumped: gh 1.3.1, zidane 2.1.1, sentry
1.2.1); READMEs that say the feature mounts it read-only say devsandbox
does. Validate with the devcontainer CLI's feature schema check if
available (`npx @devcontainers/cli features …`), else the schema JSON;
commit, push, watch CI green.

## Closing

Plan archived.
