# Mesh root groups

The dashboard's Instances tree groups by (machine, sandbox name), so a
fleet whose members all run one signed config shows every sandbox once
per host (`s`, then `box/s`, …). The mesh has one config, made of
signed config roots (`devsandboxes.toml` dirs on `<machine>/<path>`,
signed by the authority: [ADR 0049](../../decisions/0049-the-authority-signs-a-config-every-member-runs.md)),
and every instance came from one of them. What got lost is the root's
identity: rows carry only `root`, a path on their own host, so nothing
says the authority's `~/.devsandboxes` and a member's
`mesh/tree/.devsandboxes` copy are the same root.

## Decisions

1. **A mesh root's id is the name the membership log signs it under**
   (`Op::Config {root}`, `fleet::membership::check_root_name`): on the
   authority the name `mesh/roots.toml` binds to the row's root path
   (`fleet::mesh::load_roots`, `roots_path`), on a member the copy the
   root is exactly (`fleet::mesh::copy_of(data, root)` → `(name, true)`).
   An unsigned root has none: it stays its host's.
2. **Each host tags its own rows**: it is the only one that knows its
   bindings and copies. `mesh_root` rides on `InstanceRow`, `SandboxRow`
   and `ServiceRow`; `snapshots::merge` passes it through.
3. **The tree groups by root, not machine**: a group is
   `(mesh root, sandbox)` for a mesh root, else
   `(machine, root path, sandbox)` (one host's two roots defining `s` are
   two groups, as two mesh roots defining `s` are). The root isn't
   rendered yet; the group key carries it.
4. **Backfill**: an instance with no recorded `root` (older than
   `state::Instance::config_dir`) groups under the first mesh-root group,
   in tree order, defining its sandbox; else today's same-machine
   fallback. Nothing is rewritten on disk.
5. **A merged group's header is one representative row**: this host's
   when it has the root, else the first machine's in tree order. Actions
   on the header (`:run`, config view, `Empty`'s hint) keep that row's
   route (`remote::sandbox_route`).

## Step 1: rows carry `mesh_root`

- `src/snapshot.rs`: `pub mesh_root: Option<String>` on `InstanceRow`
  (near `root`, ~line 131), `SandboxRow` (~line 191), `ServiceRow`;
  `#[serde(default, skip_serializing_if = "Option::is_none")]`, doc
  comment pointing at this decision (the signed name, set by the row's
  own host). Every struct literal gets `mesh_root: None`
  (`sandbox_rows`, `build_service_rows`, the instance join, tests).
- `src/serve/fleet/mesh.rs`: a pure-ish
  `pub fn root_id(bindings: &BTreeMap<String, PathBuf>, data: &Path, root: &Path) -> Option<String>`:
  the binding whose path equals `root` (canonicalized, as `bind` stores
  canonical paths), else `copy_of(data, root)` when exact. Unit tests
  beside `copy_of`'s.
- Tag in `serve::api::InstancesList::of` (`src/serve/api.rs` ~line 569):
  data dir is `state.parent()` (as `roots.list`'s no-host branch,
  ~line 164); load `load_roots(&roots_path(data))` once (an error is
  none), then set every row's `mesh_root` from its `root`. Keep it a
  separate pure fn (`tag_mesh_roots(snapshot, bindings, data)`) so it's
  testable. This covers both `instances.snapshot` pushes (the daemon's
  `default_join`, `daemon.rs` ~line 226) and `instances.list`.
- Wire types: `npm/devsandboxes/index.d.ts` (`InstanceRow`,
  `SandboxRow`, `ServiceRow`: `mesh_root?: string`, `@experimental`);
  the `dts` test checks them. `docs/reference/api.md`: the row fields.
- Docs: `docs/architecture/daemon/machines.md` *Federated snapshots*
  (rows carry `mesh_root`, tagged where they're made); `fleet.md`
  *Config versions* if it lists where a root's name is used.

## Step 2: the tree groups by root

- `src/tui/data.rs`: a `GroupKey` (`Mesh { root, sandbox }` /
  `Host { machine, root, sandbox }`) with a stable string form for
  `collapsed` (distinct from `ORPHANS_NAME`); `group_key(&SandboxRow)`.
  `sandbox_order` yields one representative index per group (decision
  5; local rows in config order first, then machines by machine and
  name, as today). `group_of(sandboxes, inst)` returns the group's
  representative: `inst.mesh_root` → that mesh group; else its root's
  host group (today's rules, `data.rs` ~line 79); else the backfill
  (decision 4). `visible_nodes` collapses by the group key.
- `src/tui/remote.rs`: header doc; `instance_sandbox` (the row an
  instance's Detail shows) unchanged unless it disagrees with
  `group_of` for mesh rows; a `group_key` helper if the UI needs it.
- `src/tui/app/tree.rs` `collapse_key` → the group key;
  `src/tui/ui.rs` `sandbox_tree_row` (~line 2030) the same;
  `node_machine` (~line 1945) blank for a mesh group spanning hosts.
- Tests: rewrite `tree.rs`
  `same_names_on_two_machines_are_separate_groups` into: mesh rows of
  one root merge across hosts (instances of both under one header, local
  header), unsigned roots stay split per host, two roots on one host
  split, the backfill, collapse by group key. `data.rs` unit tests for
  `group_of` / `sandbox_order`.
- Docs: `docs/architecture/dashboard/tabs-and-actions.md` *Tabs*
  ("groups are per (machine, sandbox)"), `docs/architecture/daemon/machines.md`
  *Dashboard*, `data.rs` doc comments (`group_of`, `visible_nodes`).
  `CHANGELOG.md` `## Unreleased`: one line.
