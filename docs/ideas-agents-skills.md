# Ideas: .agents/skills auto-mount (parked, not scheduled)

Raw ideas for a future phase; nothing here is committed work.

## Core idea
A `.agents/skills/` folder (per config root, maybe also `~/.config/devsandbox/skills`
for user-global ones) auto-mounted read-only into every instance at a well-known
path (e.g. `/opt/agents/skills`), so coding agents inside sandboxes share a
library of skills/playbooks without baking them into images.

## Sketch
- Mount: `-v <dir>/.agents/skills:/opt/agents/skills:ro` added in build_run_args
  when the folder exists; skip otherwise. Global + per-root merged via two mounts
  (`/opt/agents/skills/global`, `/opt/agents/skills/project`) to avoid overlay
  tricks.
- Config surface: `[defaults] skills = false` opt-out, or per-sandbox
  `skills = ["only", "these"]` (subdir allowlist via individual file mounts —
  maybe v2).
- Env hint inside the container: `AGENTS_SKILLS_DIR=/opt/agents/skills` so agent
  tooling can discover it without convention-guessing.
- TUI: sandbox detail panel shows whether skills are mounted + count; config
  explorer already shows the resolved mounts.
- `devsandbox skills ls` (later): list skill files with their front-matter title.
- Live-editing works for free (bind mount) — editing a skill on the host updates
  every running instance; read-only keeps instances from mutating shared state.
- Open questions: precedence when global and project define the same skill name;
  whether worktree instances should see the base folder's skills (probably yes —
  mount from base_folder, not the worktree).
