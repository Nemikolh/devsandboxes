# Mesh config: a signed config every member runs

Status: in progress. Step 1 shipped (1a-1d: signed from the
authority's verbs, local or aimed at a machine, over the config's
structure and referenced files, the bundle kept, names bound to one
path); step 2 in progress (2a: receive and install).

## Goal

A config belongs to the fleet, not to the host it was written on. The
authority signs a version of it; every member accepts the latest signed
version it holds as valid even while the authority is offline, never an
older one (no rollback), and runs requests only against it. The use
case that drives it: an always-on box runs the sandboxes of a config
written on another host, driven from a laptop, while that host is
offline for days.

Builds on the membership log
([fleet](../../architecture/daemon/fleet.md#membership-log)): the chain,
fork detection, `changes::sync` / `flood` and "the end behind takes the
later entries" already carry any `Op` to every member without its
signer. Replaces, for mesh roots, the fleet plan's "config authority for
a member's own roots stays with that member"
([fleet](fleet.md#members-links-and-the-hub), ADR 0002); an ADR records
it when this plan closes.

## Decisions

1. **Signed on the authority's own verbs**, no CLI: a `run` / `rebuild`
   / `start` typed on the authority (a local client, `Table::Full`)
   signs its root's next version when the hash changed. Linked peers'
   verbs and the dispatcher's subprocesses never sign.
2. **What's hashed is the config's structure and what it references**,
   not its text and not the whole root: the parsed `devsandboxes.toml`
   table in a canonical encoding (comments and formatting don't count),
   plus each file under the root the config references, by relative
   path and content. Notes, logs and junk in the root are neither
   hashed nor shipped. Baby step: a richer notion (git-tracked files,
   explicit includes) can come later.
3. **A root is named by its directory's name** in the log. The
   authority binds a name to the first path it signs it from and
   refuses another path for that name. May move to a full path or
   another identity later.
4. **The signature overrides a `worker` member's policy.** A member's
   own policy is its `member.toml` `trust` (`worker` | `full`) and the
   `[allow]` allowances of the worker policy (`src/worker_policy.rs`),
   which today apply only to configs under `<data>/devsandbox/fleet/`.
   A mesh copy lives outside that, so no policy applies to it: the
   authority's signature is the consent. Accepted cost: the authority's
   key is worth root on every member, and a privileged sandbox with the
   runtime socket on the authority can read it (`key_guard` covers
   binds and builds only). `key_guard`'s checks keep running everywhere.
5. **No new request fields.** A verb already can't carry config: the
   receiver resolves `root` from its own disk, `env` is the receiver
   daemon's own (`lent::peer_env`), `profile` is checked against the
   sandbox. The receiver checks its copy against its own log head.

## Rollout

### Step 1: signing, fixed

Shipped first cut: `Op::Config { root, version, hash }`
(`src/serve/fleet/membership.rs`), `changes::sign_config`
(`src/serve/machines/changes.rs`), called from `serve::daemon` `verb()`
and `sign_config`. Its gaps, each a sub-step and a commit:

- **1a. Drop the `config.signed` notification.** It only announces a
  side effect. `sign_config` logs (`serve.log`) instead; remove
  `ConfigSigned`'s wire use, `verbs_client::config_signed_lines`, the
  dashboard's status line (`tui/mod.rs`), `npm/devsandboxes/index.d.ts`
  and `docs/reference/api.md`'s entry, and the dts check.
- **1b. Hash the structure and keep the bundle.** A config root's
  bundle: `devsandboxes.toml` plus every file under the root the config
  references (sandboxes' and services' `build` dockerfile and context
  when under the root, `mounts` sources that resolve under the root
  such as `${configDir}/mcps/*.json`, local features under the root;
  never `ROOT_SKIPS`, never `.git`). References outside the root are
  host paths, resolved per host like `folder`. The hash: sha256 over the
  parsed table's canonical encoding then the sorted
  `(relative path, sha256 of contents)` list, a fn the receiver (step
  2) recomputes over what it unpacked. The config must load
  (`config::Config::load`) or nothing is signed (logged). The bundle
  (a tar via `code::tar`) is stored at signing by hash, under
  `<data>/devsandbox/mesh/` outside `fleet/`, so the authority can
  serve the signed version after the root is edited again.
- **1c. Bind a name to one path.** The authority records name → path
  when it first signs a name and refuses another path for it (logged).
- **1d. Sign on remote verbs.** A `run`/`rebuild`/`start` with
  `machine` from a local client on the authority signs the local root
  bound (1c) to the last component of the remote `root`; no binding,
  nothing signed. Today `verb()` returns through `remote_verb` first.

### Step 2: distribution, pulled on adoption

One mechanism, no authority push code: every host that adopts a log
moving a `config` head pulls the missing bundle from the peer that gave
it the log; every host keeps the bundles it has and serves pulls of
them. Not the placement receiver: that writes into the sender's fleet
root (`fleet/<authority>/roots/<hash>/`, under the worker policy,
refused by `Reach::admits`) and one dir per version.

- **2a. Receive and install.** `mesh::install` unpacks into a temp dir,
  re-collects, and refuses unless the hash equals the log head's for
  that root (transfer checks only the sender's own header: this is the
  rollback guard); stores the bundle in `bundles/`; updates the copy at
  `<data>/devsandbox/mesh/tree/<name>/` (never under `fleet/`, named as
  the log names it, not `CONFIG_AT`) in place: the bundle's files
  written, the previous install's no longer listed removed,
  `ROOT_SKIPS` and local files (`.leader`, `dispatch.log`) left. One
  path per root, so worktrees, `shared-volumes`, the services project
  id and instances' recorded roots survive a new version. The
  authority never unpacks a name it has bound. Pulls: transfer kind
  `mesh-bundle` (`mesh::serves`, `mesh::pull_bundle`).
- **2b. On adoption.** Wire the pull + install into log adoption, once
  per moved head, from the peer that gave the log. No "who has hash X"
  discovery, no `code.haves`.

### Step 3: admission on the running member

- `Reach::admits` (`src/serve/fleet/lent.rs`) admits
  `mesh/tree/<root>` only while the copy matches the signed head: the
  structural hash recomputed (or a marker written at verified update),
  else `not the signed config`. Needed because the copy can be edited
  in place: the `devsandboxes` sandbox mounts its own config root
  read-write into a privileged container.
- `roots.list` lists mesh roots, so a laptop's
  `forward::sandboxes_everywhere` finds them.
- Open (the user's call): `start` of a container whose config drifted
  only warns (`start::warn_on_drift`); refuse it on mesh roots?

### Step 4: trust

No code (decision 4). With the copy outside `fleet/`, `Policy::detect`
answers none for it.

### Step 5: host-local values

No new keys. `${localEnv:…}` already resolves from the running daemon's
env; `${configDir}`, `${sharedVolumes}`, `.worktrees` and
`shared-files` hang off the config dir, which is the per-host copy.
Relative `folder`s (`../x`) land at `<data>/devsandbox/mesh/tree/x`:
the user clones there once (or
symlinks; folders are canonicalized). Box setup the user does: clones,
`~/.zidane`, rootless docker with `XDG_RUNTIME_DIR` in the daemon's
env, bolt-restate's `packages/*/.env`.

## Known gaps

- Files a config reaches only through a command (the dispatcher's code
  in `dispatcher/`, started by `postStartCommand`) aren't referenced
  structurally, so they're neither hashed nor shipped (decision 2).
  Nor is what a Dockerfile `COPY`s from a `build.context` of `.` (the
  root itself is never bundled), nor a reference written as an absolute
  path or through another variable (`mesh::collect`).
- For step 3: `run`'s `ensure_bind_source` creates a missing
  `${configDir}/…` source, and a read-write `${configDir}` mount lets a
  container write one; either changes the recomputed hash of a copy, so
  the admission check must recompute over the bundle's own file list
  (or a marker), not re-collect blindly.

## Not in this plan

Pulling from any peer; edits signed elsewhere (proposals exist);
moving the authority; `repo =` keys or a per-host workspace base;
placement (`ties_of`, separate work).
