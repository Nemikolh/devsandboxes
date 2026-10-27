# Fleet: machines as one pool of capacity

Status: in progress
Commit 1a (link primitive) shipped: `serve::fleet::link`, `channel::API`;
1b (hub side) shipped: `fleet-link`, `fleet.offer`, the `offered` registry
kind and its dial; 1c (worker side) shipped: `lend.toml` read at start,
the lending thread, the fleet root and the `Worker` table; 1d (CLI)
shipped: `serve::locked_file`, `lend.get|set` applied live, `devsandbox
lend`, `fleet join|leave`; 1e (worker policy) shipped: `worker_policy`,
detected by path (no flag), checked on the final `run` / `build` argv and
the config, allowances in `lend.toml`'s `[allow]`; 1f (hub mode)
shipped: keep-alive, offered links always held, `remotes.json`
(`machines::saved`) restored as `unreachable`
([fleet](../../architecture/daemon/fleet.md), ADR
[0041](../../decisions/0041-fleet-workers-dial-their-hub.md)); the rest
is design. Builds on [machines](../../architecture/daemon/machines.md)
and supersedes nothing yet; [machines-next](machines-next.md) items 2 and 3
fold into step 8 here, its parked auto-update and PTY attach into steps
2 and 4.

## Goal

Add machines to run more containers. A new sandbox goes on any machine
with room; a dispatcher on an always-on server (the **hub**) creates,
queries and moves children wherever they run, while laptops lend CPU,
memory and disk when they're awake. Where an instance runs is a detail
the user sees but rarely chooses.

Setting: every machine is the user's own, on one Tailscale tailnet; most
repos have a git remote, not all. Workers are **not** reachable by ssh
from the hub, by choice: a laptop offers itself through devsandbox only,
and the hub can do on it exactly what the devsandbox API it's offered
allows, nothing more (no shell, no host files).

## Where today stops

| # | Pain | Code |
|---|---|---|
| P1 | A sandbox is a path on one host: `folder` is relative to a config root there; each machine has its own `roots`. Running `web` elsewhere needs its config and checkout mirrored by hand. | `config.rs`, `machines::Entry` |
| P2 | No placement: `run` resolves a name and, matched on several machines, asks on a TTY. No capacity is reported. | `verbs_client`, `snapshots::merge` |
| P3 | Dispatcher children live on the dispatcher's host: child ops are `--in-process` subprocesses that never reach a machine; ownership, `max-instances` and `state.toml` are per host. | `commands::dispatch` `Subprocess` |
| P4 | Links are held only while a client wants them (+ 60 s linger); remote rows are memory only. Nothing makes a daemon a standing hub. | `service.rs` `Want`, `snapshots::Remotes` |
| P5 | Federation is one hop: a laptop linked to the hub doesn't see the hub's machines. | `snapshots::merge` |
| P6 | A protocol mismatch makes a machine unreachable; nothing updates it. | `link.rs`, `install` |
| P7 | Credentials: the agent relay flows from the linking host to the machine; a hub-placed child has no obvious agent. | [Agent](../../architecture/daemon/machines.md#agent) |
| P8 | The Inbox is a per-host store. | `inbox::store` |
| P9 | Images are built per machine, no shared cache; image tags aren't namespaced by root, so shared configs clobber. | `run::image::base_image_for` |
| P10 | `scope = "global"` services are per host: children on two machines see two postgres. | `commands::services` |
| P11 | Every path to a machine is an ssh *to* it: the link (`api_argv`), each forwarded verb (a fresh dial), terminal tabs and CLI re-runs (`tty_argv`, `cli_argv`), forwards (`tunnel_argv`), the agent relay (`agent_argv`), install (`script_argv`). None works when only the worker may dial. | `machines::ssh` |
| P12 | The API a link reaches is the machine's whole daemon: every root, every instance, `machines.*`. A worker must expose only what it lends. | `serve::api` |

## Design decisions

### Topology: workers dial the hub (the CI-runner model)

A worker offers itself; the hub never dials it. The worker's daemon holds
one outbound ssh to the hub (`ssh -T -o BatchMode=yes <hub> devsandbox
fleet-link --worker <name>`, a hidden command like `agent-relay` that
connects to the hub's daemon), retried with the link's backoff while
lending is on. ssh stays the transport
([ADR 0020](../../decisions/0020-machines-over-plain-ssh-with-protocol-match.md)),
only the direction flips: the laptop needs ssh to the hub (it has it),
the hub needs nothing on the laptop: no sshd exposure, no key of the
hub's in the laptop's `authorized_keys`.

**One link, many channels.** The link carries the helper's frame
protocol ([protocol](../../architecture/helper/protocol.md)), which
already runs between two hosts for the machines' agent relay
(`commands::agent_relay`): flow-controlled streams over one byte pipe.
Each thing that is an ssh *to* a machine today (P11) becomes a channel
the hub opens on the held link:

| today (ssh to the machine) | fleet worker (channel on its link) |
|---|---|
| `api_argv`: the link | `API`: the hub as a client of the worker's lent API |
| a fresh dial per forwarded verb | one `API` channel per verb (a hang-up is the channel's close) |
| `tty_argv`, `cli_argv`: terminals, CLI re-runs | `PTY`: exec with resize (machines-next's parked "PTY attach over the protocol", now needed) |
| `tunnel_argv`: forwards | `CONNECT`: the forwarder's existing streams |
| `agent_argv` | `SSH_AGENT` from hub to worker, below |
| — | `TRANSFER`: bundles and tars, below |

On the hub, a worker's `Dial` opens a channel on its held link and
returns it as a `Duplex`, so `client::Conn`, `forward::verb`, the quick
calls and `Machines` above the `Dial` seam stay as they are. A worker is
a second kind of `machines.toml` entry (`offered`, no `target`), written
when its first offer arrives; ssh machines keep working for hosts you do
ssh into.

**The lent API is not the daemon's API (P12).** An offered link is
served on the worker by a restricted method table (`fleet::WORKER`), not
`serve::api`'s:

- it reaches only the worker's **fleet root** for that hub
  (`<data>/devsandbox/fleet/<hub>/`): instances the hub placed, their
  repo caches and config roots. The laptop's own roots and instances
  don't exist for the hub;
- verbs, quick calls, snapshots, `PTY`/`CONNECT` into those instances,
  `TRANSFER` into and out of that root; never `machines.*`, roots,
  `serve` handoff, host paths;
- a **worker policy** in `lend.toml` checks every resolved config before
  it runs: bind mounts only inside the fleet root (named volumes fine),
  no `privileged`, no runtime socket mount, no host network, unless
  `lend.toml` allows them (e.g. docker-in-docker features). The hub is
  trusted; the policy bounds what a compromised hub could do to the
  laptop to "run containers that see only what was lent".

**The worker owns its lending** (`lend.toml` next to `state.toml`,
edited by `devsandbox lend …`, never by the hub): `on|off|drain`, the hub
it offers to, soft caps (`cpus`, `mem`, `disk`, `instances`), the policy
above. `devsandbox fleet join <hub>` writes it and starts offering;
`fleet leave` drains, then stops. Its capacity report rides the link's
snapshot. A sleeping laptop is a dropped link (`unreachable`): placement
skips it, re-placement (step 7) may move its children.

Hub mode is a daemon setting (`hub = true` under `[serve]` in the existing
`daemon.config.toml`, `src/daemon_config.rs`; implies `keep-alive`): the daemon never idles out, accepts offers, and
persists remote rows (`remotes.json`) so a restarted hub shows the fleet
before workers reconnect. `serve install` on the server keeps it up.

Trust: an ADR records that fleet workers dial their hub and that a hub
reaches a worker only through the lent API, inside the worker policy;
config authority for placed instances sits with the hub
([ADR 0002](../../decisions/0002-config-is-the-security-boundary.md)
stays as is for a host's own roots). Sharing a fleet between users is out
of scope.

### Placement: soft caps, a pure `place`

Today nothing is limited, so v0 is mostly the seam: a capacity report,
a pure `place(spec, &[Candidate]) -> Placement`, and soft caps.

- **Report**: each daemon adds to its snapshot the host's cpus, memory
  total/available, disk free under the runtime's data root, running
  instance count and its `lend.toml`. Cheap, from the existing poll.
- **Candidates**: the hub and every reachable worker lending `on` that
  can get the code (step 3: repos with a reachable `origin`; from step 5:
  any, by transfer).
- **Score**: headroom against soft caps, plus locality (repo already
  cloned, image already built, which `config_hash`/`build_hash` labels
  tell), plus a small bias for the requester's own host.
- **Soft caps never refuse.** Over a cap a machine scores low; if every
  candidate is over, the best one is used and the verb's output, the
  dashboard header and the Machines tab say `over cap: mem 18G/16G`.
  `--machine m` or a qualified name always wins over the score.
  `drain` is the only hard no (for new work).

### Moving code: devsandbox owns the transfer

Forcing every repo through a third-party remote is wrong: it pushes
unfinished work, and a repo started locally has no remote yet. devsandbox
moves the code itself, host to host, over the links it already has. A
git remote, when there is one, is only a shortcut for the bulk.

- **Repo identity**: the sorted root commit ids (stable across clones, no
  remote needed); a repo with no commit yet gets one devsandbox commit on
  a hidden ref before it can move.
- **Snapshot**: a source host captures a worktree as a commit on
  `refs/devsandbox/wip/<instance_id>`: tracked changes plus untracked,
  non-ignored files, written through a temporary index (`write-tree`,
  `commit-tree`), never touching the user's index, branch or stash, and
  through `host_git`
  ([ADR 0003](../../decisions/0003-host-git-never-runs-repo-controlled-code.md)).
  Ignored files the worktree wants (`.worktreeinclude`,
  `worktree-include`) go alongside as a tar.
- **Wire**: the receiver sends the commits it has for that repo identity;
  the sender streams `git bundle create - <wip ref> <branch> --not <haves>`
  on a `TRANSFER` channel; the receiver `git fetch`es the bundle into its
  repo cache (`<data>/devsandbox/repos/<repo id>.git`, bare; under the
  fleet root on a worker) and cuts the worktree from it. With a reachable
  `origin`, the receiver fetches from it first so the bundle carries only
  the delta.
- **Route**: workers never talk to each other, so laptop A's code
  placed on laptop B flows A → hub → B. The hub keeps what passes
  through (below), which is what makes re-placement possible.
- **Back**: the placed instance's branch and WIP are fetched back into the
  source repo as `refs/devsandbox/<machine>/<instance>/…` on demand
  (`devsandbox fetch <instance>`, the dashboard) and at `done`/`rm`;
  never a silent rewrite of the user's branches.
- **Hub mirror**: the hub keeps the repo cache for every repo it has
  placed and receives periodic checkpoints (step 7), so a repo's latest
  WIP survives its laptop going to sleep.
- **Config roots** move the same way (a config root that's a git repo
  as a repo, any other as a content-hashed tar of the root), so a worker
  runs the hub's config as the hub resolved it, dockerfiles and local
  features included.

All of it sits behind one seam, `Transfer` (`offer`, `haves`, `send`,
`receive`), so the bundle implementation can be replaced.

**Short term git, long term a sync layer.** Git bundles are right for
starting and moving work: content-addressed, incremental, already on
every host. They don't cover state outside git: ignored build outputs,
`node_modules`, named volumes, caches, a database's data, or live
two-way editing of a placed instance's files from the laptop. Those need
a real sync layer (content-defined chunks, rsync-style deltas or a
mutagen-like live session), which `Transfer` must allow without changing
its callers: the plan keeps the seam, the git implementation is v1.

### Versions: the worker updates itself

The hub can't install anything on a worker, so the worker updates
itself: at its link's hello it learns the hub's version, and when it
differs it fetches that version, installs it and lets its own version
handoff restart its daemon
([ADR 0015](../../decisions/0015-newest-binary-wins-version-handoff.md)),
which re-offers:

- **Published version**: the release asset for its own platform from
  GitHub (`devsandbox-<v>-<target>.tar.gz`, as
  `.github/workflows/release.yml` names them), verified, installed
  atomically (the existing `install::INSTALL` steps).
- **Local build** (not a published version): the hub's own binary over a
  `TRANSFER` channel when the OS and arch match
  (`install::platform_matches`) and the worker's `lend.toml` accepts dev
  builds (it's the hub choosing code the laptop runs outside a
  container); else the worker stops offering with "dev build: can't run
  on <os>/<arch>". Accepted: a dev fleet of mixed platforms runs
  published builds or is built per platform by hand.
- Release targets today: linux x86_64/aarch64 (musl), macOS aarch64,
  windows x86_64. An Intel Mac can't auto-update until a target is added.

### Credentials

A hub-placed child gets the hub's agent over the link's `SSH_AGENT`
channel, served on the worker only to that hub's placed instances (the
worker's bridges pick it for fleet-root instances, never for the
laptop's own). The hub's agent is whatever the hub has: a dedicated key
on the server (deploy or user key), not a laptop's. The laptop's own
agent never reaches the hub.

## Rollout

Each step is usable on its own. Order with the other plans is in
[helper-rpc](helper-rpc.md#order-across-plans): steps 1-3 run alongside
helper-rpc's 1-3; step 4 waits for helper-rpc's `session.attach`; step 6
for its daemon-owned runs.

1. **Fleet link and hub mode** (L). `fleet-link`, the frame-mux link with
   `API` channels, the `offered` registry kind and its channel `Dial`,
   `fleet::WORKER` over the fleet root, the worker policy, `lend.toml`,
   `fleet join|leave`, `devsandbox lend`, `hub = true`, persisted remote
   rows, capacity in the snapshot, the Machines tab showing lending and
   capacity. Quick calls and verbs work on placed instances from here.
2. **Worker self-update** (S–M). Download or the hub's binary at the
   hello; stop offering with the reason otherwise. Before the fleet grows
   past two workers.
3. **Placement v0 + minimal transfer** (M). `place`, soft caps and
   warnings; `run <sandbox>` places on the hub or a worker; a placed run
   gets the config root as a tar and clones the repo from `origin`
   (committed, pushed work only; a repo without a reachable `origin` runs
   where it is). `--here` forces local. Namespace image tags and feature
   build dirs by config root (P9's clobber), since shared configs make it
   bite.
4. **Interactive over the link** (M). `PTY` (exec, terminal tabs, the
   reconnecting `exec`), `CONNECT` forwards, `SSH_AGENT` from the hub.
   Placed instances become as usable as local ones.
   `PTY` is a session attach ([resumable-exec](resumable-exec.md)): the
   channel opens with `{instance, session id, create|attach}` and ends
   with a structured record (`exit <code>` | `gone` | `detached` |
   `lost`); a link drop is `lost`, never an exit. The 255 /
   `--reserve-255` convention (`serve::machines::cli::Reconnect`) stays
   ssh-machine-only. `fleet::WORKER` gains session `ls`/`rm` for
   fleet-root instances. Decide first whether the worker daemon attaches
   to the session's `sock` directly (proposed: drops the in-container
   client and the `exec -it` hop, settles helper-rpc's `run.attach`) or
   goes through `<runtime> exec -it` as the ssh path does.
5. **Transfer v1** (L). Repo identity, WIP snapshot commits, bundles
   routed through the hub, repo caches, the hub mirror, fetch-back.
   Repos without a remote and unpushed work can be placed.
6. **Dispatcher children on any machine** (M). Instances carry
   `origin_host` and repo identity; `ls`/the dashboard show where they
   run. The dispatcher (on the
   hub) keeps authorizing on its host
   ([ADR 0030](../../decisions/0030-dispatcher-control-is-authorized-on-the-host.md));
   `dispatch::Executor`'s subprocess becomes a placed, forwarded verb;
   the hub's `state.toml` records child → machine; `exec`/`run-*` ops go
   through forwarded calls; `branches` reads the hub's repo cache.
   `max-instances` stays per dispatcher, fleet-wide.
7. **Checkpoints and re-placement** (M–L). Placed instances checkpoint
   WIP to the hub's mirror (on idle, every few minutes while dirty, at
   stop). A child whose worker is unreachable past a grace period, or on a
   worker set to `drain`, is re-ensured on another machine from its last
   checkpoint: same name and `instance_id`, the old copy fenced (its
   worker, when back, is told the instance moved: stop it, keep its WIP
   ref, no auto-delete). Process state is lost, committed and
   checkpointed work isn't. `devsandbox move <instance> [--to m]` is the
   manual form. Fencing a copy ends its sessions: live or detached
   sessions extend the grace period (or at least warn), and an attach to
   a moved instance says `moved to <machine>`, not `no session`.
8. **Laptop as a client of the hub** (M). A laptop links the hub as an
   ordinary ssh machine (laptop → hub, fine) or, when it's also a worker,
   over its offered link's `API` channel in the other direction. Federated
   snapshots through the hub (a link's watch asks for `machines: true`;
   rows of the asking machine never re-exported: no cycles), `PTY` and
   `CONNECT` to another worker's instances relayed by the hub (`PTY` end
   records relayed unchanged: a worker hop dropping reaches the client
   as `lost`), the Inbox behind the daemon on
   both ends (machines-next §2), nameless commands across the fleet
   (machines-next §3).
9. **Sync layer** (L, later). A second `Transfer` for state git doesn't
   carry: ignored outputs, volumes, caches, live editing. Then build-cache
   sharing (a registry on the hub, `--cache-from`) and services that span
   machines (P10) can be revisited.

## Steps 1-3 in detail

Decided with the user before building (2026-10-27):

1. `hub = true` in `daemon.config.toml` `[serve]`, no `serve.toml`.
2. An offer is accepted only by a hub; ssh login to the hub is the
   authentication (the user's own account). A worker name colliding with
   an ssh machine is refused.
3. `lend.toml` is written by the daemon only, through `lend.*` API calls
   (the `machines.toml` rule); `devsandbox lend` / `fleet` are clients.
4. A worker's fleet root is one pseudo-root on the hub; `fleet::WORKER`
   lists the config roots under it.
5. Step 1's acceptance test: a config copied by hand under
   `<worker data>/devsandbox/fleet/<hub>/`, `run --machine <worker>`.
6. Until step 4, paths that need ssh to the machine (`exec`, terminal
   tabs, forwards, CLI re-runs) refuse on an offered machine with
   `needs fleet step 4` wording; `logs` / `inspect` use the
   `containers.*` quick calls.
7. The hub always holds offered links (no `Want` / linger for them).
8. Capacity: cpus / memory total from `docker info` (`NCPU`,
   `MemTotal`, cached 60 s); free memory and free disk only for a native
   Linux runtime (`/proc/meminfo`, `statvfs` of the runtime's root), else
   unknown. VM-backed engines report the VM.
9. Worker policy denies by default: binds outside the fleet root,
   `privileged`, runtime socket mounts, host network, `caches`,
   `shared-volumes`, `worktree-link`, `global` services, inbox /
   dispatcher sockets. Allowances per worker in `lend.toml`.
10. Self-update: the hello carries git hash + `published`; the worker
    follows the hub's exact version (a downgrade is a forced handoff);
    installs in place at `exe::devsandbox()`. Verified by TLS + running
    `--version`; no checksum asset (a `release.yml` change not approved).
11. Placement runs in the daemon.
12. Step 3's code: the hub clones `origin` with its own credentials and
    sends a bundle over `TRANSFER`; a worker never needs credentials.
    Pulls a slice of step 5 forward (bundle of committed refs only).

Commits (landmarks from recon; line numbers drift, trust the symbol):

- **1a link primitive.** `channel::API` / `caps::API`
  (`src/devsbd/proto.rs`) on helper-rpc step 1's flow-controlled `Open`.
  One `serve::fleet::link` module generalizing the opener of
  `src/commands/agent_relay.rs` `run` (bind a socket, temp name +
  rename, each accepted connection → `Mux::attach(.., Some(channel))`)
  and the answerer of `src/serve/machines/agent.rs` `serve` (each `Open`
  → a local socket), sentinel and channel as parameters. Test: a
  `client::Conn` through the mux reaches a test daemon.
- **1b hub side.** Hidden `fleet-link --worker <name>` (`src/main.rs`,
  like `AgentRelay`; `serve::machines::cli` `machine_use` refuses it):
  binds `fleet-<name>.sock` in the socket dir, mux on stdio, sends
  `fleet.offer {worker, version}` to its daemon. `machines::Entry` gets
  a kind (`Ssh { target }` | `Offered`); every ssh argv builder refuses
  `Offered`; the offered `Dial` is `UnixStream::connect(fleet sock)` +
  `Duplex::socket`, so `link::handshake` (the API channel writes the
  `devsandbox-api 1` sentinel, `relay::write_sentinel`), `forward::verb`
  and the quick calls work unchanged. `machine ls`, the Machines tab and
  `snapshots::SnapshotMachine` show the kind.
- **1c worker side.** A lending thread in the worker daemon holding `ssh
  -T -o BatchMode=yes <hub> devsandbox fleet-link --worker <name>` with
  `service.rs`'s backoff while lending is on. `Open{API}` → socketpair →
  `Shared::spawn_conn` (`src/serve/daemon.rs`) with a per-connection
  table `Full | Worker { hub, root }` checked at the top of `handle`:
  hello, subscribe, `instances.watch`/`unwatch`, the `QUICK` set
  (`machines/forward.rs`) and verbs only; no `machine` param; every
  `root` / `dir` under the fleet root; instance rows filtered by
  `config_dir`; verbs' `env` replaced by the worker's own login env (as
  `relay::SessionEnv` does for ssh). ADR 0041: workers dial the hub; a
  hub reaches a worker only through the lent API and its policy.
- **1d CLI.** `lend.toml` model + store (third locked atomic writer after
  `machines::update` and `forward_store`: extract a shared helper);
  `lend.get|set` API; `devsandbox lend on|off|drain|show`, `fleet join
  <hub>` / `fleet leave` (drain, then off); the daemon reacts to `lend.set`
  directly (no file watching).
- **1e worker policy.** The verb subprocess learns its root is a fleet
  root (hidden flag from `verbs::argv`); a pure checker over the resolved
  config, called at `run`'s privileged decision (`commands/run/mod.rs`
  `privileged_decision`) and before `identity::create_args`'s binds;
  allowances from `lend.toml`; refusal names the field and the
  allowance.
- **1f hub mode.** `hub = true` (`daemon_config.rs`, read in
  `daemon.rs`): never idles (`idle::decide` keep-alive), accepts offers
  (else `fleet.offer` errors), keeps offered links wanted, saves / loads
  `remotes.json` around `snapshots::Remotes` (rows shown `unreachable`
  until the worker reconnects).
- **1g capacity.** `docker info` parse gains `NCPU` / `MemTotal`
  (`runtime/identity.rs` parser, fixture there), cached; native-Linux
  free memory / disk; running count (`host.rs` `live_instances`) and
  lending state into each root's snapshot and `SnapshotMachine`;
  Machines tab columns, `over cap` note; `docs/reference/api.md`,
  `index.d.ts`.
- **2a `TRANSFER`.** A flow-controlled channel carrying a header line
  (kind, name, size, sha256 of the body) then raw bytes; receiver writes
  to a temp file and checks size + hash before rename.
- **2b self-update.** Hello gains `git` + `published`
  (`src/serve/proto.rs`); on a mismatch the worker fetches
  `https://github.com/<repo>/releases/download/v<v>/devsandbox-<v>-<target>.tar.gz`
  (curl / tar shell-out like `commands/features.rs`), installs with
  `machines::install::INSTALL`'s steps run locally, runs `--version`,
  then hellos its own daemon to trigger the handoff (forced, also for a
  downgrade), and re-offers.
- **2c dev builds.** Unpublished hub version: the hub's binary over
  `TRANSFER` when `install::platform_matches` and `lend.toml`
  `dev-builds = true`; else stop offering with `dev build: can't run on
  <os>/<arch>`.
- **3a image namespacing (P9).** `run::image::base_image_for`
  (`devsandbox-img-<sandbox>`), the `-feat` tag, `features::build_context_dir`,
  services' `img-svc-<service>` gain a short hash of the config root;
  check rebuild / gc / image facts.
- **3b `place`.** Pure `place(spec, &[Candidate]) -> Placement`,
  soft-cap score + warnings, from 1g's capacity; `drain` the only hard no.
- **3c placed run.** In the daemon's `run` verb: `place` when the user
  didn't pin a machine (`--here` forces local); config root as a tar over
  `TRANSFER` into `fleet/<hub>/roots/<hash>/`; the hub clones `origin`
  into its repo cache and sends a bundle of the branch; the worker cuts
  the worktree at the folder's offset relative to the config root; no
  reachable `origin` → runs where it is, with a note.

## Open questions

- Fencing in step 7: is "the worker stops the stale copy when it
  reconnects" enough, or must a placed instance hold a lease it renews
  with the hub and stop itself when the lease lapses (laptop offline but
  awake, e.g. off the tailnet)? Either way it ends that copy's sessions,
  which a user may still mean to reattach to (step 7).
- Checkpoint cadence and size: big untracked files in WIP commits; a size
  cap with a warning, or `.devsandboxignore`?
- Hub mirror retention: when are WIP refs and repo caches of removed
  instances pruned?
- Should `run` place by default once a fleet exists, or only with
  `--anywhere` until placement has earned trust?
- Services: does a placed child's `global` service move with it, or must
  `global` services pin their dependents to one machine?
- Worker policy defaults: per-worker allowances for now (decision 9);
  per-sandbox allowances if docker-in-docker features make that too
  coarse.
- One hub per worker, or may a laptop lend to several hubs (one fleet
  root each)?
