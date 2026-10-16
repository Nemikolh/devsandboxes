---
name: orchestrator-workflow
description: Run a multi-step feature as orchestrator - plan in markdown, delegate each step to an implementer subagent, review, commit, repeat.
metadata:
  internal: true
---

# Orchestrator workflow

You orchestrate; subagents implement. You never let a subagent touch git.

## Steps

1. **Recon first.** Read the relevant code (grep/glob, then targeted reads)
   until you can name exact files, functions, and line numbers. Never plan
   from memory.
2. **Write the plan as a markdown file** at `docs/plans/active/<topic>.md`
   (topic name, no `plan-` prefix; layout in `docs/README.md`): one
   `## Step N` section per commit-sized step, with constraints the
   implementer must obey, code landmarks (file:line), and the
   `docs/architecture/` docs each step must update. Both the user and the
   subagents read this file. Add an *In progress* line for it to
   `docs/status.md`.
3. **Present the plan** to the user for approval before touching anything.
   On revision requests, update the plan file and re-present.
4. **Per step, spawn one implementer subagent**:
   - "task" = `/implementer` skill + the plan file path + step number
     - code landmarks + anything learned from prior steps' reviews;
   - subagents have no conversation context — brief like a colleague who
     just walked in.
5. **Review every completed step yourself**: read the diff (`git diff --stat`,
   then the important files), re-run build + tests, hunt for cross-cutting
   defects the step brief couldn't foresee (resource leaks, stderr hitting
   owned terminals, cooked-vs-raw mode, blocking the UI thread). Check the
   step updated the area docs (`docs/architecture/`, `docs/reference/`) and
   `docs/status.md` for any behavior it changed, and that what they now say
   matches the code. Small defects: fix directly. Large ones: resume the
   subagent with the finding.
6. **Commit the step** with a message that says _why_, staging only the
   step's files (never `git add -A`; leave unrelated workspace diffs alone).
7. Track steps with the todo list; mark off as you go.
8. **Close the plan** in a final commit, once every step has landed:
   - what's now true lives in `docs/architecture/` / `docs/reference/`
     (verify it against the code, not the plan);
   - decisions that will hold become ADRs in `docs/decisions/` (a new one
     supersedes, old ones aren't rewritten);
   - `docs/status.md`: drop the *In progress* line; add remaining work
     (*Planned* / *Parked*) and known limitations, one verified line each;
   - move the plan to `docs/plans/archive/` with the banner (see an
     existing archived plan); nothing outside the archive may link to it.

   Then summarize: what landed, review fixes you made on top, known
   limitations.

## Invariants

- One step = one review = one commit. No batching.
- Real check outcomes only; report failures, never paper over them.
- User owns scope: new ideas go into the plan (flagged as yours) or a
  *Parked* line in `docs/status.md`, never silently into code.
- `cargo test --workspace` (not plain `cargo test`: it skips `devsbd/`);
  it includes `tests/docs_links.rs`, which fails on broken doc links.
