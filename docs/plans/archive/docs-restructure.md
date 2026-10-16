> Archived 2026-10-16. Historical, may be wrong; do not use as a reference.
> Current: [README.md](../../README.md)

# Docs restructure

Turn the flat `docs/` (43 files mixing plans and reference, status lines
that drift, "Known limitations" nobody can trust) into a tree where each
folder holds one kind of document with one maintenance rule.

Audience: contributors and agents, biased to agents. An agent that greps
`docs/` must hit current truth, not history.

## Target layout

```
docs/
  README.md            the map: what devsandbox is in 5 lines, reading order,
                       one line per doc (path: when to read it)
  status.md            the ONLY place for state: In progress / Planned /
                       Parked / Known limitations (by area)
  architecture/        LIVING. true now, updated in the same commit as code
    overview.md        one section per area: 3-8 line summary + links
    config.md
    instances/         run, worktrees, folders, worktreeinclude, locking,
                       rebuild, services, features/entrypoint, vscode, ...
    runtimes.md
    daemon/            role, client-server, verbs, forwards, machines, ...
    helper/            devsbd, frame protocol, bridges, ssh-agent relay
    inbox/
    automations/
    dashboard/
  reference/           LIVING, exhaustive specs: api.md, inbox-cli.md, ...
  decisions/           ADRs still holding true: NNNN-kebab-title.md
  plans/
    active/            approved / in progress / parked plans
    archive/           shipped or superseded plans, banner on top;
                       deleted once outdated
```

Folder names under `architecture/` are a starting point: an area agent may
split or merge files inside its folder; new top-level folders need the
orchestrator.

## Conventions (every agent follows these)

### Living docs (`architecture/`, `reference/`)

- Present tense, describes the code as it is. No "Status:", no "Plan",
  no steps, no dates, no "as built, differs from".
- **Every claim verified against the code.** If an old doc says X and the
  code says Y, write Y and list the discrepancy in your report.
- Start with a 1-3 line paragraph: what this is and when to read it. Then a
  `Code:` line naming files/modules (paths + symbol names, never line
  numbers: they rot).
- Short sections with greppable headings; the term the code uses is the
  term the doc uses.
- No limitations section. Limitations go to `status.md`; a living doc may
  end with `Limitations: [status.md](../status.md#<area>)`.
- Link siblings with relative links. No links into `plans/archive/`.
- Target length: as short as complete. Exhaustive tables belong in
  `reference/`.

### `status.md`

```
## In progress
- **<area>**: one line. → [plan](plans/active/x.md)
## Planned
## Parked
## Known limitations
### <area>
- one line, verified still true against the code. → [doc](architecture/...)
```

Fixing a limitation = deleting its line in the same commit.

### Decisions (`decisions/`)

Only decisions that still hold in the code today. One file each,
20-40 lines:

```
# NNNN: <decision as a statement>
Status: accepted            (or: superseded by NNNN)
## Context
## Decision
## Consequences
Source: <old doc + section, or commit>
```

Numbers are assigned by the decisions lead only (children return drafts).

### Archive (`plans/archive/`)

A shipped or superseded plan moves here only after its true content lives
in `architecture/`/`reference/`/`decisions/`/`status.md`. First line:

```
> Archived <YYYY-MM-DD>. Historical, may be wrong; do not use as a reference.
> Current: [architecture/x.md](../../architecture/x.md)
```

It's deleted once outdated (contradicts the code, or adds nothing over the
living docs). Agents skip `plans/archive/` unless asked about history.

### Out of scope

- `skills/config-toml-spec/SKILL.md` stays the user-facing config reference;
  `architecture/config.md` covers internals (merge, drift, hashing) and
  links to it rather than duplicating.
- No code changes except the link sweep and the link check (step 4).

## Source triage (starting point, leads may correct it)

| area lead | old docs it reads |
|---|---|
| instances + config + runtimes | run-command, rebuild, service-rebuild, folders-worktrees, worktreeinclude, worktree-locking, entrypoint, devcontainer-features, vscode-workspaces-and-persistent-state, config-toml-findings, runtimes, devsandboxes, high-level-architecture (its config/instances/services parts) |
| daemon | serve, client-server, verbs-as-requests, instance-phase, before-machines, machines, port-forwarding, api, windows-support, high-level-architecture (daemon parts) |
| helper | sandbox-helper, ssh-agent, helper-rpc |
| inbox + automations | inbox-redesign, inbox-threads, inbox-cli, automations, automations-guide, dispatcher-branch-holders, cli-proxy |
| dashboard | tui, tui-terminal, tui-selection, tui-prompt-spec, dashboard-client, processes-stop-colors, json-output |

Planned / not started (→ `plans/active/` + `status.md`): helper-rpc,
windows-support, cli-proxy (verify), ideas-agents-skills (parked).

## Step 1: skeleton (orchestrator)

Create the folders, `README.md` and `status.md` skeletons, this plan.
Old docs stay where they are until step 3 so links keep working.

## Step 2: content, four parallel leads

Each lead spawns one child per target document (or per small cluster),
briefs it with this file + its sources, reviews its output against the
code, then reports. Leads write **only inside their own folder** and into
their own fragment file `docs/plans/active/docs-restructure/<lead>.md`
(limitations, status entries, discrepancies found, decision candidates).
Nobody edits `status.md`, `README.md`, `overview.md` or old docs in step 2.

- **2a. Architecture leads** (one per area row above, run in parallel):
  write `architecture/<area>/…` (and `reference/…` for daemon: `api.md`;
  inbox: `inbox-cli.md`). Fragment: limitations verified true today,
  stale claims found in old docs, decision candidates.
- **2b. Decisions lead**: reads every old doc's Decisions / rationale
  sections, children verify each against the code, keeps only those still
  true, writes `decisions/NNNN-*.md`. Fragment: decisions dropped and why.
- **2c. Plans lead**: for each not-started/parked/in-progress doc, verify
  status against code and git log, write `plans/active/<topic>.md`
  (trimmed, current), and a fragment with status.md entries.
- **2d. Triage lead** (read-only): for every old doc, a verdict table:
  `archive` (shipped, still mostly true, keep for history) |
  `delete` (already outdated or fully superseded) | `active`; with one line
  of evidence each. Deletes and moves nothing.

## Step 3: merge (orchestrator)

From the fragments: write `status.md`, `architecture/overview.md` (from the
area docs), `README.md` map. Cross-check that every `archive`/`delete`
verdict's true content landed somewhere. Move `archive` docs to
`plans/archive/` with the banner; list the `delete` set for the user to
confirm (nothing deleted without confirmation).

## Step 4: links + guard

### Old → new map (for the sweep)

Pick the new doc by the TOPIC of the referencing comment, not just the old
file name; open the target and use its real heading names for any
section reference (`*Section*`). Never point at `plans/archive/` from
code or living docs.

| old `docs/…` | new home (by topic) |
|---|---|
| api.md | reference/api.md |
| serve.md | architecture/daemon/role.md; forwards → daemon/forwards.md; poll → daemon/host-poll.md; install/idle/handoff → daemon/role.md |
| client-server.md | architecture/daemon/client-server.md; machines bits → daemon/machines.md |
| verbs-as-requests.md | architecture/daemon/verbs.md |
| before-machines.md | daemon/host-poll.md (shared poll), daemon/verbs.md (held verbs), daemon/role.md (upgrades), instances/images-and-features.md (build log) |
| instance-phase.md | architecture/instances/lifecycle.md; build log → instances/images-and-features.md |
| machines.md | architecture/daemon/machines.md |
| port-forwarding.md | architecture/daemon/forwards.md |
| windows-support.md | plans/active/windows-support.md (future) or daemon/platform-support.md (today) |
| sandbox-helper.md | architecture/helper/{overview,protocol,bridges,in-container}.md by topic |
| ssh-agent.md | architecture/helper/ssh-agent.md |
| helper-rpc.md | plans/active/helper-rpc.md |
| inbox-redesign.md, inbox-threads.md | architecture/inbox/{model,threads-and-feed,forms,views-and-wire}.md by topic |
| inbox-cli.md | reference/inbox-cli.md |
| automations.md, dispatcher-branch-holders.md | architecture/automations/{autostart,dispatchers}.md; boot hook → helper/in-container.md |
| automations-guide.md | unchanged (stays at docs/automations-guide.md) |
| cli-proxy.md | plans/active/cli-proxy.md |
| ideas-agents-skills.md | plans/active/agents-skills.md |
| tui.md, dashboard-client.md, processes-stop-colors.md | architecture/dashboard/{overview,tabs-and-actions}.md |
| tui-terminal.md | architecture/dashboard/terminal.md |
| tui-selection.md | architecture/dashboard/selection.md |
| tui-prompt-spec.md | architecture/dashboard/command-line.md |
| json-output.md | architecture/dashboard/snapshot-and-json.md |
| run-command.md, devsandboxes.md | architecture/instances/lifecycle.md (mounts → mounts-and-state.md, services → services.md) |
| rebuild.md, service-rebuild.md | architecture/instances/rebuild-and-drift.md |
| folders-worktrees.md, worktreeinclude.md, worktree-locking.md | architecture/instances/worktrees.md |
| entrypoint.md, devcontainer-features.md | architecture/instances/images-and-features.md |
| vscode-workspaces-and-persistent-state.md | architecture/instances/mounts-and-state.md |
| config-toml-findings.md | architecture/config.md |
| high-level-architecture.md | architecture/overview.md |
| runtimes.md | architecture/runtimes.md |

- Rewrite every `docs/<old>.md` reference (about 262 in `src/`,
  `AGENTS.md`, `skills/`, `npm/devsandboxes/`) to its new home.
- `AGENTS.md`: module map pointers updated; a *Docs* rule: behavior change
  in area X updates `architecture/X` (and `status.md`) in the same commit;
  skip `plans/archive/` unless asked about history.
- A `#[test]` that every relative link in `docs/**/*.md` resolves and no
  living doc links into `plans/archive/`.
- `cargo test --workspace`.

## Commits

One commit per step after the orchestrator's review:
`docs(<area>): …` for step 2's outputs (one per lead), `docs: …` for
3 and 4.
