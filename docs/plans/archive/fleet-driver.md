> Archived 2026-11-11. Historical, may be wrong; do not use as a reference.
> Current: [architecture/daemon/fleet.md](../../architecture/daemon/fleet.md), [ADR 0050](../../decisions/0050-machine-add-on-the-authority-signs-its-configs.md)

# Fleet driver: a server runs it, a laptop drives it

Status: shipped. Built on [fleet](../active/fleet.md) steps 1-12
([fleet](../../architecture/daemon/fleet.md),
[machines](../../architecture/daemon/machines.md), ADRs
[0044](../../decisions/0044-one-fleet-authority-signs-membership.md),
[0049](../../decisions/0049-the-authority-signs-a-config-every-member-runs.md)).

## Goal

The first real fleet setup works end to end:

```bash
# on the host (the authority), juchu becomes the hub
devsandbox machine add juchu --hub
# on the laptop (no container runtime): joins as a member
devsandbox machine add juchu
# on the host: approve the laptop's join
devsandbox machine approve <id>
# next day, on the laptop
devsandbox run juchu/bolt-restate
```

- juchu gets the host's signed config as part of the `machine add`.
- The laptop's join reaches the authority as a proposal; once approved,
  it's a full member (no longer capped).
- A `run` from the host onto juchu's signed copy brings the sandbox's
  repos over `TRANSFER` from the host's checkouts; a run from a host
  without them, with the folder missing on juchu, says the files are
  missing and how to get them there.
- A laptop with no container runtime is a clean driver: no runtime
  errors, no placement onto it, no runnable-looking copies of its own.

## Decisions

1. `machine add` on the authority signs `~/.devsandboxes` and
   `member.toml`'s `roots`, never other `state.toml` dirs (a binding in
   `mesh/roots.toml` is permanent). The new member installs them at its
   first link-up (the existing `daemon::follow_configs`); no new transport.
2. The laptop's introduction is proposed by its signer at the laptop's
   request and approved by hand on the authority (`machine approve`):
   the authority still never signs on a remote request alone (0044, 0049).
3. Repo sync: a folder missing on the member is landed from the
   authority's checkout; one devsandbox landed before gets the delta and
   its refs moved, its working tree never recut; a checkout the user made
   by hand is never touched (a marker tells them apart).
4. Parked: clone from `origin` as a fallback (origin as derived state in
   the bundle); fetch-back (`placements.json`) for these runs.

## Step 1: the signer proposes a joining host's introduction

Today: `machines::api::add` (`src/serve/machines/api.rs`, `Logs::Signed`
branch ~:334) calls `membership.sign` on the machine over the add-time
probe; the machine's `membership_api::sign` stores an introduction and
nothing proposes it (`introduced_here` ~:386 requires `introducer ==
me`); the authority doesn't even hold it until its link redials.
`docs/status.md` ~:360 documents the gap.

- New local-only `membership.propose {name}` in the `changes::call`
  group (`src/serve/machines/changes.rs` `call` ~:158; daemon dispatch
  `src/serve/daemon.rs` ~:2208, which has `shared.machines`): finds the
  held introduction of `name` this host signed, `changes::propose`s it
  (`Change::Add {introduction}`, ~:693) to the log's authority over any
  path, answers `Proposed`. Refused on every link like other
  `membership.*` (`lent::admit`).
- The machine also pushes its stored log right after the introduction is
  signed (`changes::push`, best effort, off the request thread), so the
  authority holds the introduction (graph reports, routes to the joiner)
  before approval.
- `add`, `Logs::Signed` branch, after this host's adopt: when `here`
  holds an introduction of this host by the machine, call
  `membership.propose {name: this_name}` over the still-open probe (keep
  the probe open until then) and fill `Added.proposed`; an error is the
  answer's `proposed.error`, never a failed add. The CLI's existing
  `proposed_line` (`src/commands/machine.rs` ~:603) then prints `approve
  there with devsandbox machine approve <id>`.
- Constraints: no new field on `membership.sign` (`SignParams` is
  `deny_unknown_fields`); propose only after the sign's lock is released.
- Tests: extend `this_host_is_introduced_by_the_machine`
  (`machines/api.rs` ~:1147) to assert `proposed`; an end-to-end in
  `changes.rs` modelled on `a_sync_floods_to_a_third_host` (~:1657):
  authority A dials K, L joins K, the proposal is pending on A,
  `membership.approve` makes L a member of the log on all three.
- Docs: `fleet.md` join section ("H joins X's fleet" ~:85), Proposals
  (~:1897), `membership.*` list; `machines.md` `machine add` paragraph
  (~:789); `docs/reference/api.md` (new method); drop the status.md
  entry; ADR 0044 is unchanged in substance (no new ADR).

## Step 2: `machine add` on the authority signs its configs

- `machines::api::Ctx` gets the roots to sign (a hook built in
  `daemon.rs` ~:356-419 from `shared.home` and `member.toml`'s `roots`:
  `~/.devsandboxes` when it's a dir, plus picked roots; never
  `mesh::roots`).
- `changes::sign_configs(ctx, roots) -> Vec<ConfigSigned>` maps
  `sign_config` (`changes.rs` ~:490), dropping `None` and `NotAuthority`.
- `api::add` calls it after this host's adopt and `hub_changed` and
  before `links.reload()` (~:362-363): at fleet creation the log exists
  only after the adopt, and before the reload the new entry isn't in
  `links.entries()` yet (not a push miss); its first dial carries the
  newer log and the member pulls the bundle. A `Failed` never fails the
  add (it's committed): `Added.configs` (notes, `#[serde(default)]`),
  printed by `commands::machine::add`; `npm/devsandboxes/index.d.ts`
  `MachineAdded` updated (`npm_typings_match`).
- Tests: `machines/api.rs` `Pair` (`the_authority_adds_the_machine`,
  `add_hub_makes_the_machine_the_hub`): the log has `config` v1, the
  bundle stored, `mesh/roots.toml` bound; an end-to-end install on the
  member over a real link (modelled on `a_member_installs_each_signed_version`,
  `changes.rs` ~:1283) if the harness allows.
- Docs: `fleet.md` Config versions (who signs: verbs *and* `machine
  add`), `machines.md` `machine add`, `api.md` `machines.add`, ADR 0049's
  "its own verbs only" (a new short ADR superseding that sentence, or
  amend per `docs/README.md` rules: new ADR), site doc if it states the
  "run a verb first" step (`site/`, grep).

## Step 3: a run from the authority brings the repos

Landmarks: `daemon::verb` (`daemon.rs` ~:2599-2609, `sign_remote_config`
~:2759, `remote_verb`); placed transfer `code::prepare`/`offer`/`send`/
`send_repo` (`src/serve/fleet/code/mod.rs` ~:365-716), receivers
`code::receivers` (~:856, mounted `daemon.rs` ~:1127-1148),
`repo::land_from` (`code/repo.rs` ~:188, recuts unmounted checkouts:
never on a hand clone), `transfer.rs` `check_header` (no `..` in `to`).

- Member side, new API call `mesh.folders {root, sandbox}` (peer-callable,
  gated like `code.haves` in `lent.rs`): `root` must be exactly an
  installed copy; answers each `folder`/`folders` entry: its config
  string, its path relative to `mesh/tree/`, `missing` | `landed`
  (devsandbox marker present) | `user` (exists, no marker).
- Authority side, in `verb()` after `sign_remote_config`, for `run` from
  a local client whose root's name is bound here (`mesh::bound_root`):
  ask `mesh.folders`, map each entry to this host's checkout
  (`bound_root.join(<config string>)`), and send `missing` and `landed`
  ones over `TRANSFER` with a new kind (e.g. `mesh-folder`, `to` relative
  to `mesh/tree/`), reusing the per-repo part of `offer` (factored out,
  no `ties` check: the signature is the consent) and `send_repo`, with
  the member's `code.haves` for deltas. `user` entries are left alone.
  Failures stop the run with a clear error naming the folder.
- Member receiver (new, chained next to `code::receivers`): only from the
  log's authority by its proven key; `to` under `mesh/tree/`, real dirs,
  outside every installed copy's dir, at most one `..` out of a copy;
  objects into the authority's repo cache
  (`<data>/fleet/<authority>/repos/<id>.git`); `missing` → fresh checkout
  via `land_from` + marker; `landed` → refs moved only (as `land_from`
  with `in_use = true`), never a working-tree recut.
- Out of scope (clear error): `${…}` folders, folders inside the copy
  (`folder = "."`), nested folders, a detached `HEAD` on the authority.
- Missing-files error, for any host: `commands/run/mod.rs` ~:135-138 and
  `run/mounts.rs` ~:146-149, when `dir` is a signed copy
  (`mesh::copy_of`): name the folder and absolute path, say the signed
  config carries no code, and to run it from the authority `<name>`
  (which syncs it) or clone it there.
- Tests: `code/mod.rs` link helpers (`prepare_moves_a_sandbox_onto_a_worker_over_its_link`
  ~:1438, `a_wip_snapshot_lands_in_a_new_checkout_only` ~:1823); the
  receiver's refusals (non-authority key, `..`, a copy's dir, `user`
  checkout untouched); `tests/mesh_copy.rs` for the error.
- Docs: `fleet.md` Config versions ("cloned there by the user" ~:2032)
  and Transfer (kinds, receivers); `api.md` (`mesh.folders`); status.md
  (parked: origin fallback, fetch-back for these runs; the existing
  `../x` vs copy name clash line ~:671 stays or is updated).

## Step 4: a host with no container runtime is a driver

- Runtime absence: a spawn failure because the binary isn't found
  becomes a distinct "not installed" outcome (`src/runtime/mod.rs`,
  `dockerlike.rs`, `apple.rs`), probed once and re-checked rarely
  (`ENGINE_TTL`-like).
- Daemon: while not installed, `host.rs` skips the listing/`server_version`
  calls and stats, `events.rs`'s `EventWatcher` isn't started, the idle
  tick isn't held at 5 s; the snapshot carries no `error` for it, and the
  dashboard header names no runtime (`tui/data.rs` ~:253, `tui/ui.rs`
  ~:324).
- Capacity: a `runtime` flag in `Capacity` (`fleet/capacity.rs`);
  `place()` (`fleet/place.rs` ~:203) skips a member without one ("no
  container runtime"), and a pin to it is refused clearly.
- Copies: a host without a runtime doesn't advertise its mesh copies as
  own roots (`lent::own_roots` / `roots.list`), so `laptop/bolt-restate`
  rows don't appear and a bare `run bolt-restate` isn't ambiguous; its
  own CLI picks drop local rows it can't run.
- Tests: a daemon test with `PATH=/nonexistent` (model
  `run_under_a_fleet_root_is_refused_before_the_runtime`,
  `commands/run/mod.rs` ~:1677); `place` unit tests; `capacity.rs`.
- Docs: `machines.md`/`fleet.md` (Capacity, Placement), the dashboard
  doc for the header, `api.md` capacity shape, `index.d.ts`.

## Step 5: close

Living docs verified against code, `CHANGELOG.md` Unreleased, status.md
(drop the in-progress line; parked items), this plan to
`plans/archive/`.
