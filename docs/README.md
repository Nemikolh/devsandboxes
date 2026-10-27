# devsandbox docs

Contributor and agent docs: how the code works today, why, and what's left.
User-facing docs live in `site/` (and the config reference in
`skills/config-toml-spec/SKILL.md`).

## Reading order

1. [architecture/overview.md](architecture/overview.md): the whole system in
   one page, with a link per area.
2. The area doc for what you're touching (below). Each starts with when to
   read it and a `Code:` line.
3. [status.md](status.md): what doesn't work yet, what's planned or parked.
4. [decisions/](decisions/): before changing something that looks
   deliberate.

## Kinds of doc

| folder | holds | rule |
|---|---|---|
| `architecture/` | how each area works | true now; changes in the same commit as the code |
| `reference/` | exhaustive specs | true now; same commit as the code |
| `decisions/` | decisions that still hold (ADRs) | append-only; a new ADR supersedes an old one |
| `status.md` | in progress, planned, parked, known limitations | the only place for state |
| `plans/active/` | work not done yet | one per topic; leaves once shipped |
| `plans/archive/` | shipped plans kept for history | may be wrong; skip unless asked about history; deleted once outdated |

## Map

- **Config**: [architecture/config.md](architecture/config.md)
- **Instances**: [lifecycle](architecture/instances/lifecycle.md),
  [worktrees](architecture/instances/worktrees.md),
  [rebuild and drift](architecture/instances/rebuild-and-drift.md),
  [services](architecture/instances/services.md),
  [images and features](architecture/instances/images-and-features.md),
  [mounts and state](architecture/instances/mounts-and-state.md),
  [identity](architecture/instances/identity.md)
- **Runtimes**: [architecture/runtimes.md](architecture/runtimes.md)
- **Daemon**: [role](architecture/daemon/role.md),
  [client-server](architecture/daemon/client-server.md),
  [verbs](architecture/daemon/verbs.md),
  [host poll](architecture/daemon/host-poll.md),
  [forwards](architecture/daemon/forwards.md),
  [machines](architecture/daemon/machines.md),
  [fleet](architecture/daemon/fleet.md),
  [platform support](architecture/daemon/platform-support.md)
- **Helper**: [overview](architecture/helper/overview.md),
  [protocol](architecture/helper/protocol.md),
  [bridges](architecture/helper/bridges.md),
  [in-container](architecture/helper/in-container.md),
  [ssh-agent](architecture/helper/ssh-agent.md)
- **Inbox**: [model](architecture/inbox/model.md),
  [threads and feed](architecture/inbox/threads-and-feed.md),
  [forms](architecture/inbox/forms.md),
  [views and wire](architecture/inbox/views-and-wire.md)
- **Automations**: [autostart](architecture/automations/autostart.md),
  [dispatchers](architecture/automations/dispatchers.md)
- **Dashboard**: [overview](architecture/dashboard/overview.md),
  [tabs and actions](architecture/dashboard/tabs-and-actions.md),
  [command line](architecture/dashboard/command-line.md),
  [terminal](architecture/dashboard/terminal.md),
  [selection](architecture/dashboard/selection.md),
  [inbox tab](architecture/dashboard/inbox-tab.md),
  [snapshot and json](architecture/dashboard/snapshot-and-json.md)
- **Reference**: [daemon API](reference/api.md),
  [`devsandbox inbox` CLI](reference/inbox-cli.md)
