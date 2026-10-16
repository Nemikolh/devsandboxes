# 0013: Devcontainer features are installed natively, entrypoints chained at build time

Status: accepted

## Context

Sandboxes use devcontainer features. The official path is the
`devcontainer` CLI (Node), or an OCI registry client. Both are heavy
dependencies for a single static Rust binary. Features may also ship
entrypoints (e.g. dockerd) that must run when the container starts,
alongside the image's own `ENTRYPOINT`.

## Decision

- Features are fetched with plain `curl` + `tar` (`features::download_and_extract`)
  from OCI refs or local folders, cached, and installed in a generated
  feature image (`run::image`) whose layout mirrors the official CLI's, so
  feature scripts see what they expect.
- Feature entrypoints are chained at build time into one wrapper
  (`run::image::entrypoint_wrapper`), and the image's own `ENTRYPOINT` is
  always kept at the end of the chain.

## Consequences

- No Node or registry crate dependency; feature support follows the spec as
  far as implemented (`installsAfter` only; no `dependsOn`).
- Cached features never refresh on their own (mutable tags stay at the
  first pull).
- Lifecycle commands may start while feature entrypoints are still coming
  up; there is no readiness wait.

Source: docs/devcontainer-features.md, "Design"; docs/entrypoint.md.
