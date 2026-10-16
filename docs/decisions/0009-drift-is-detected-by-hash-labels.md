# 0009: Drift is detected by hash labels on containers, with one lenient rule

Status: accepted

## Context

Users edit the config while instances run. devsandbox must tell when a
container no longer matches its config (to warn on `start`, flag it in the
dashboard, and drive `rebuild`) without keeping a copy of the old config
and without flapping on containers made by older versions.

## Decision

- At creation, containers carry a `config_hash` label (hash of the merged
  sandbox table, `config::sandbox_hash`) and a `build_hash` label (hash of
  the build Dockerfile's *contents* and local features' contents,
  `config::build_hash` / `sandbox_build_hash`), not paths.
- Host-side keys that can be applied without recreating the container are
  stripped before hashing (`autostart`, `dispatcher`, `inbox`,
  `forwardPorts`), and default-restating `folders` entries are normalized.
- One rule, `commands::drift_decision`, is shared by every site (`start`'s
  warning, the snapshot's drift flag, `rebuild`). A missing label means no
  drift.

## Consequences

- No stored config history; the label is the record.
- Build context and OCI feature contents aren't hashed: changes there need
  `rebuild --force`.
- Adding a host-only config key means adding it to the strip list, or every
  edit to it will mark instances drifted.

Source: docs/rebuild.md; docs/service-rebuild.md, step 1; docs/high-level-architecture.md, "Drift".
