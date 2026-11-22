# 0060: An image runs the updates it was built with

Status: accepted (2026-11-22). Refines
[0059](0059-feature-updates-run-in-the-features-image.md).

## Context

0059 ran an update's command from the feature's current metadata inside
the newest local features image. A feature release that moves its updater
(gh 1.3.0, zidane 2.1.0) made that command missing from every image built
before it, and devsandbox can't tell such an image is stale: a remote
feature's contents aren't part of the build hash.

## Decision

- The features image carries a `devsandbox.updates` label: every update
  its features declared at build, with the mount's unresolved source
  (`commands::run::image`, `updates::to_label`).
- `devsandbox update` and the daemon read only that label
  (`updates::from_label`, re-validated); the current metadata decides
  nothing.

## Consequences

- An older image runs its own older updater, or none; a feature release
  reaches an image at its next rebuild.
- An image built before the label has no updates: the daemon skips it, a
  manual `update` names `devsandbox rebuild`.
