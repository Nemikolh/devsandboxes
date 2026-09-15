---
name: orchestrator-workflow
description: Run a multi-step feature as orchestrator - plan in markdown, delegate each step to an implementer subagent, review, commit, repeat.
---

# Orchestrator workflow

You orchestrate; subagents implement. You never let a subagent touch git.

## Steps

1. **Recon first.** Read the relevant code (grep/glob, then targeted reads)
   until you can name exact files, functions, and line numbers. Never plan
   from memory.
2. **Write the plan as a markdown file** in `docs/` (e.g. `docs/plan-<topic>.md`):
   one `## Step N` section per commit-sized step, with constraints the
   implementer must obey and code landmarks (file:line). Both the user and
   the subagents read this file.
3. **Present the plan** to the user for approval before touching anything.
   On revision requests, update the plan file and re-present.
4. **Per step, spawn one implementer subagent** (`claude-opus-4-8`):
   - brief = `/implementer` skill + the plan file path + step number
     + code landmarks + anything learned from prior steps' reviews;
   - subagents have no conversation context — brief like a colleague who
     just walked in.
5. **Review every completed step yourself**: read the diff (`git diff --stat`,
   then the important files), re-run build + tests, hunt for cross-cutting
   defects the step brief couldn't foresee (resource leaks, stderr hitting
   owned terminals, cooked-vs-raw mode, blocking the UI thread). Small
   defects: fix directly. Large ones: resume the subagent with the finding.
6. **Commit the step** with a message that says *why*, staging only the
   step's files (never `git add -A`; leave unrelated workspace diffs alone).
7. Track steps with the todo list; mark off as you go. After the final step,
   summarize: what landed, review fixes you made on top, known limitations.

## Invariants

- One step = one review = one commit. No batching.
- Real check outcomes only; report failures, never paper over them.
- User owns scope: new ideas go into the plan (flagged as yours) or a parked
  ideas doc, never silently into code.
