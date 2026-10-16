# 0023: The helper frame protocol grows additively, not by version bumps

Status: accepted

## Context

Running containers keep the helper they were started with while the host
binary upgrades. If every protocol change bumped a version, every running
helper would mismatch the new host until its container restarted.

## Decision

- `proto::VERSION` stays 1. New frame kinds are added; a reader skips
  unknown kinds (payload consumed, `read_frame` returns `Ok(None)`) instead
  of failing. Capabilities are advertised as `caps` bits in `Hello`
  (e.g. `NOTIFY`, `CONTROL`), and `Hello` may carry trailing bytes old
  readers ignore. Port forwarding landed as new kinds with no bump.
- In-container CLI capabilities are reported separately by `devsbd features`,
  distinct from `version` (build hash) and `caps`.

## Consequences

- An old helper/host pair degrades cleanly: a missing capability disables a
  feature instead of breaking the bridge.
- Every protocol addition must be optional and gated on a cap or feature;
  a truly incompatible change would still need `VERSION`.

Source: docs/sandbox-helper.md, "Frame protocol"; docs/port-forwarding.md, "Why tunnel through devsbd".
