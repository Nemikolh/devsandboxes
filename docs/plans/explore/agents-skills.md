# Agents skills auto-mount

Status: parked
Raw idea, never scheduled; nothing is built (no `.agents/skills` handling in `src/` or `devsbd/`).

Today a skills folder reaches instances only as an explicit bind in a
sandbox's or template's `mounts` (arrays concatenate through `extends`, so
one entry in `template.base` covers every sandbox). See
[mounts-and-state](../../architecture/instances/mounts-and-state.md) and
[config](../../architecture/config.md).

## Idea

A `.agents/skills/` folder (per config root, maybe also a user-global
`~/.config/devsandbox/skills`) mounted read-only into every instance at a
well-known path, so coding agents inside sandboxes share skills/playbooks
without baking them into images.

## Sketch

- Mount: added by `resolve_mounts` (`src/commands/run/mounts.rs`) when the
  folder exists, skipped otherwise. Global and per-root as two mounts
  (`/opt/agents/skills/global`, `/opt/agents/skills/project`) to avoid
  overlay tricks.
- Read-only bind: host edits reach every running instance; instances can't
  mutate shared state.
- Worktree instances mount from the base folder, not the worktree.
- `AGENTS_SKILLS_DIR=/opt/agents/skills` in the container env so agent
  tooling finds it without guessing.
- Config: an opt-out per sandbox (`skills = false`), maybe a subdir
  allowlist (`skills = ["only", "these"]`, individual mounts) later. A new
  field means updating `skills/config-toml-spec/SKILL.md`.
- Dashboard: sandbox detail shows whether skills are mounted and how many.
- Later: `devsandbox skills ls` listing skill files with their front-matter
  title.

## Open questions

- Precedence when global and project define the same skill name.
- Whether this is worth a feature over a documented `mounts` entry in
  `template.base`.
