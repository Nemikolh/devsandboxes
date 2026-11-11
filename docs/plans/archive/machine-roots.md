> Archived 2026-11-11. Historical, may be wrong; do not use as a reference.
> Current: [architecture/daemon/machines.md](../../architecture/daemon/machines.md)

# Machine roots: what a machine lists, nothing picked at add

Status: shipped.

## Goal

`machine add` stops asking for or recording config roots: that's the
pre-fleet model (this host chooses which of a box's roots it shows). A
machine's roots are what its daemon answers in `roots.list` over the
link: its own configs (state.toml `config_dir`s, `~/.devsandboxes`,
`member.toml` `roots`), its signed copies and this host's fleet root
there. `devsandbox machine roots add|rm` stays: it is each member's own
setting, read where it lives.

## Decisions

1. Removed: the add's root picker, `--config`, `record_roots` (the
   add-time ssh running `machine roots add` there), `machines.add`'s
   `roots` and its `roots.list {paths}` check, `machines.probe`'s roots,
   the registry's `roots` in `machines.toml` (an old file's key is
   ignored on load, dropped on the next write).
2. `roots.list` includes `member.toml` `roots` (today only `Reach::of`
   adds them), so a member's picked roots stay visible.
3. Default root for a command on a machine with no root given: the
   signed copy (one copy; with several, the one named `.devsandboxes`),
   else the machine's `~/.devsandboxes`, else its only root, else an
   error naming the roots and the `m:<root>` form. *(Orchestrator's
   call: the several-copies rule and routed machines below.)* A routed
   machine (roots from the hub's view, no flags) uses "only root, else
   error".
4. No compat shims (pre-1.0).

## Step 1: `machine add` takes no roots

- CLI: `src/main.rs` `MachineCommand::Add` (`configs`, `--config` help and
  example); `src/commands/machine.rs` `add`'s pick/record path,
  `added_roots`, `record_roots`, `record_roots_argv`, `roots_warning`,
  `label`, `pick_roots`, `Selection`, `picked`, `only_root`, module doc;
  `ssh::add_cli_argv` (dead after) and its tests.
- API: `machines/api.rs` `Probed.roots` and the probe's `roots.list`
  call, `AddParams.roots` and the `roots.list {paths}` check,
  `Added.roots`; `serve::api` `RootsParams`/`roots_check` and the
  `paths` dispatch (`daemon.rs` condition, `lent` gating tests).
- `Entry.roots` stays this step (written empty).
- Tests, `npm/devsandboxes/index.d.ts` (probe, add, `roots.list`),
  docs: `machines.md` `machine add`, `api.md` probe/add/`roots.list`,
  `status.md` (the "machine added before `machine add` recorded its
  roots" entry), `site/` `--config` lines (`host-daemon.mdx`,
  `going-further.mdx`), `AGENTS.md` `add_cli_argv` mention.

## Step 2: a machine's roots are what it lists

- `machines/mod.rs` `Entry.roots`/`RawEntry` removed; `service.rs`
  `live_roots`, `registered`, `same_but_roots`, `Cmd::Roots`,
  `apply_roots` simplified to the listing (inbound neighbors' roots,
  `daemon::inbound_roots`, may go too if the link's own listing covers
  them).
- `machines.list` / `machines.get` answer the live roots (today
  registry ones via `registered(ctx)`).
- `serve::api::roots_list` adds `member.toml` `roots`.
- Hub restart: rows restored from `remotes.json` aren't pruned before
  the machine's link lists (`snapshots::Remotes::sync`; or save roots in
  `remotes.json`).
- Order now `roots.list`'s: check `snapshots::merge` (index 0 claims
  rootless rows) and `tui/remote.rs` `machine_root`.
- Tests (`service`, `forward`, `dialer`, `saved`, `snapshots`, `mod`),
  docs (`machines.md` Registry, Live roots, inbound; `fleet.md`
  `remotes.json`, trust table; `api.md` list/get/`roots.list`).

## Step 3: the default root on a machine

- `RootCandidate` (`serve/api.rs`) gains `copy: Option<String>` (from
  `mesh::copy_of`) and `home: bool`; carried to `MachineInfo`
  (`machines/api.rs`) as a `default_root` computed by a pure fn.
- Used where a machine's first root is taken today: `machines/cli.rs`
  (`port`, `resolve_and_route`, `root_on`'s error), `verbs_client.rs`
  (resolved name with no root).
- Tests; `api.md`, `index.d.ts` (`RootCandidate`, `MachineInfo`),
  `machines.md` CLI section.

## Step 4: close

Docs verified, `CHANGELOG.md` Unreleased, status.md, plan archived.
