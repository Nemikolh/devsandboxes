# 0008: `extends` deep-merges, and arrays concatenate

Status: accepted

## Context

Templates (`[template.*]`) carry shared settings: mounts, extensions,
services. A sandbox extending a template usually wants to add to those
lists, not restate them. JSON-merge-style "arrays replace" would force every
sandbox to copy the template's mounts to add one.

## Decision

`extends = "a"` or `["a", "b"]` merges left to right with cycles detected
(`config::deep_merge`): tables recurse, **arrays concatenate**, scalars
override. Mounts are then collapsed to one per target, last entry wins, so
a downstream table overrides an inherited mount by re-mounting its target.
This is how `template.base`'s services reach every sandbox. Unknown keys
are hard errors (`deny_unknown_fields`), so a typo can't silently drop a
setting.

## Consequences

- There is no way to remove an inherited array entry; a diamond
  (`extends` reaching one template twice) duplicates entries.
- The merged table is what gets hashed for drift (0009).
- Any change here must update `skills/config-toml-spec/SKILL.md`, the
  user-facing format reference.

Source: docs/config-toml-findings.md #4; docs/high-level-architecture.md, "devsandboxes.toml".
