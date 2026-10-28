# Fleet: machines as one pool of capacity

Status: in progress. Steps 1-4 shipped
([fleet](../../architecture/daemon/fleet.md), ADRs
[0041](../../decisions/0041-fleet-workers-dial-their-hub.md),
[0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md)),
built on two kinds of machine (ssh machines and offered workers, `fleet
join`, `lend`; step 4's channels on the fleet link only); steps 5-9 replace that with the model below (ADR
[0043](../../decisions/0043-every-machine-is-a-fleet-member.md),
proposed). The rest is design. Builds on
[machines](../../architecture/daemon/machines.md);
[machines-next](machines-next.md) items 2 and 3 fold into step 14 here,
its parked auto-update and PTY attach into steps 2 and 4.

## Goal

Add machines to run more containers. A new sandbox goes on any machine
with room; a dispatcher on an always-on server creates, queries and moves
children wherever they run, while laptops lend CPU, memory and disk when
they're awake. Configs live on any machine and are added freely; every
machine sees every machine's sandboxes. Where an instance runs is a
detail the user sees but rarely chooses.

Setting: every machine is the user's own, on one Tailscale tailnet; most
repos have a git remote, not all. A laptop need not accept ssh from
anyone: links go from whichever machine ran `machine add`, and what other
machines may do on a member through devsandbox is that member's own
setting.

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
| P11 | Every path to a machine is a fresh ssh *to* it: each forwarded verb, terminal tabs and CLI re-runs (`tty_argv`, `cli_argv`), forwards (`tunnel_argv`), the agent relay (`agent_argv`). | `machines::ssh` |
| P12 | The API a link reaches is the machine's whole daemon: every root, every instance, `machines.*`. A member must be able to expose less. | `serve::api` |
| P13 | Two ways to add a machine: `machine add` (this host sshes to it, sees its own roots, never places on it) and `fleet join` (it dials the hub, lends a fleet root, its own roots invisible). | `machines::Kind`, `commands::lend` |

## Design decisions

### Members, links and the hub

**Every machine is a fleet member; one member is the hub.** A fleet is
one user's machines with one hub, the director: it holds the member list
and the link graph, the federated view and placement. Being the hub is a
role, not a kind of machine, and has nothing to do with who dials whom.
Every member's daemon stays up while it's in a fleet (it holds links;
`serve install` keeps it up across logins), and the hub saves remote rows
(`remotes.json`) so a restarted hub shows the fleet before members
reconnect.

**A link is an ssh pair.** `machine add X` run on A makes a link A → X:
A's daemon holds `ssh -T -o BatchMode=yes X devsandbox fleet-link`,
always while both are up, retried with the links' backoff. That's the
only way a link forms, whichever end is (or later becomes) the hub. ssh
stays the transport
([ADR 0020](../../decisions/0020-machines-over-plain-ssh-with-protocol-match.md)).
Each host's `machines.toml` lists the links it dials (name, target).

**The transport is a seam.** A link is one byte pipe from a `Transport`
(ssh today, `ssh -T -o BatchMode=yes X devsandbox fleet-link`);
everything between two machines rides it as channels, so another
transport replaces one module. More than one connection between a pair
would be a bandwidth detail behind the seam, never a second path.

**Links form a graph; requests are routed over it.** The expected shape
is a star around one always-on machine (every laptop linked to K), with
some extra links between members. A request to X takes a path through
the graph: each hop's daemon splices the stream onto its next link (an
`Open` naming its destination). The hub learns the graph (each member
reports its links and their states) and publishes it; the requester
tries paths in order (a direct link first, then fewest hops, preferring
hops that stay up) and on a failure tries the next one. With every path
down, X is `unreachable`: placement skips it, re-placement (step 13) may
move its children. Picking the fastest path (measured latency) is later.
If the hub is A in `A → K ← B, K ← C`, K relays everything for A: the
role and the routing centre need not be the same machine.

**One link, many channels.** The link carries the helper's frame
protocol ([protocol](../../architecture/helper/protocol.md)):
flow-controlled streams over one byte pipe, opened from either end. Each
thing that is a fresh ssh to a machine today (P11) becomes a channel:

| today (ssh to the machine) | channel on a link |
|---|---|
| `api_argv`: the link | `API`: a client of the member's API, within its trust |
| a fresh dial per forwarded verb | one `API` channel per verb (a hang-up is the channel's close) |
| `tty_argv`: terminals | `PTY`: a session attach with resize (step 4) |
| `cli_argv`: CLI re-runs | `API` calls within the member's trust (decision 15) |
| `tunnel_argv`: forwards | `TUNNEL`: a forward's bridge, its `Connect` streams inside |
| `agent_argv` | `AGENT`, [Credentials](#credentials) |
| — | `TRANSFER`: binaries, bundles and tars |

A routed channel's `Dial` returns a `Duplex`, so `client::Conn`,
`forward::verb`, the quick calls and `Machines` above the `Dial` seam
stay as they are. The ssh at `machine add` time (install check,
`script_argv`) stays.

**Configs live on any member.** A member's own config roots (those it
has instances in, plus those picked at `machine add`, as probed roots are
picked today) are part of the fleet: every member sees every member's
sandboxes and instances (a member's daemon watches the hub with
`machines: true`; rows of the asking machine are never re-exported, so no
cycles), and can run a member's config on that member, or elsewhere as
[Trust](#trust-worker-or-full) allows. Config authority for a member's
own roots stays with that member
([ADR 0002](../../decisions/0002-config-is-the-security-boundary.md));
a config placed elsewhere runs as its home member resolved it.

### Joining and leaving

**`machine add <target>`**, run on host H, makes the link H → X (the
install check first, as today) and asks both for their hub:

| H | X | result |
|---|---|---|
| in a fleet (hub K) | in none | X joins K's fleet |
| in none | in a fleet (hub K) | H joins K's fleet: the laptop case, `machine add <any member>` run on the laptop |
| in none | in none | a fleet of two, H the hub |
| hub K | hub K | an extra link in the same fleet |
| hub K | hub L | refused, naming both |

The link is H → X in every row; joining only writes `hub` in the
joiner's member file. ssh login is the authentication (the user's own
account). `machine unlink <m>` drops this host's link to `m` (`m` stays a
member while another link reaches it); `machine rm <m>` takes `m` out of
the fleet (every link to it closed, the dialers' entries dropped);
`machine rm` of the host it runs on leaves the fleet.

**The member file** (`member.toml` next to `state.toml`): `hub` (the
hub's member name, or `self` on the hub), `name`, `trust`, `[caps]`,
`[allow]`, `dev-builds`. Written by the daemon only, through `member.*`
calls; `machine` is its client. There's no lending toggle: a member takes
work while it's reachable, and leaving is `machine rm`.

### Trust: `worker` or `full`

A member's own setting, `worker` by default, changed only on that
machine (`devsandbox machine trust worker|full`, a local `member.set`; no
link's table carries `member.*`, and a `--machine` or a qualified name
refuses it). It applies to every remote caller, the hub included: the
hub's role adds the member list, the graph and placement, not access.

- `worker`: a remote caller reaches
  - **the member's own roots**: snapshots, quick calls, verbs (`run`,
    `start`, `stop`, `rebuild`, `rm`), `PTY` and `TUNNEL` into those
    instances, which run as the member's config says. Never an edit of
    those configs or files (`config.*`, a `TRANSFER` into them), and
    never a move of them off the member unless the request started on the
    member itself: a `run` typed on the laptop may send the laptop's
    config elsewhere, another machine asking may not;
  - **placed work** (another member's config), in the member's **fleet
    root** (`<data>/devsandbox/fleet/<hub>/`: placed config roots and
    their repo caches), under the **worker policy**: every resolved config
    is checked before it runs (decision 9's denials), allowances in
    `member.toml`'s `[allow]`;
  - never `member.*`, `machines.*`, `shutdown`, `helper.call`, a version
    handoff other than the fleet's own update ([Versions](#versions)),
    or a host path outside the above.
- `full`: the member's whole API, as a local client has it: any root,
  any path, its configs moved freely, no worker policy, dev builds
  accepted.
- **What `worker` protects**: the member from the other machines, inside
  devsandbox. Nothing against an ssh into it: a machine holding a link
  *into* B has a shell on B and could rewrite B's `member.toml`. The
  graph says exactly when that bites: B's `worker` buys little when the
  hub, or a `full` member the hub reaches, dials B. `machine trust
  worker` says so then, naming the machine, and `machine ls` and the
  Machines tab show it on B's row.

### Swapping the hub

`machine hub <m>` (step 10; a pointer swap for now). Links don't change:
the role moves. Run on any member, it tells every reachable member that
`m` is the hub (each rewrites `hub`); a member unreachable then learns it
from the first member it reconnects to. `m` starts empty: rows and the
graph come back as members report, the repo cache refills as code moves.
Handing the hub's state over (repo cache, saved rows, dispatcher records,
checkpoints) is step 16, once steps 12-13 make that state matter; the
pointer swap stays for a dead hub.

**A fallback hub** (step 17, later). An ordered list of members that take
the role when the hub has been unreachable for a while (minutes to hours,
a setting), so a fleet whose hub is a laptop keeps placing. Open: who
decides the hub is gone (the fallback itself, or a majority of the
members that can see each other), and what happens when the old hub
comes back (it rejoins as a member, or takes the role back) without two
hubs placing at once.

### Placement: soft caps, a pure `place`

Runs on the hub: a `run` typed on any member goes to the hub, which
places it; with the hub unreachable it runs where it was typed, with a
note. Today nothing is limited, so v0 is mostly the seam: a capacity
report, a pure `place(spec, &[Candidate]) -> Placement`, and soft caps.

- **Report**: each daemon adds to its snapshot the host's cpus, memory
  total/available, disk free under the runtime's data root, running
  instance count and its `[caps]`. Cheap, from the existing poll.
- **Candidates**: every reachable member that can get the code (step 3:
  repos with a reachable `origin`; from step 11: any, by transfer).
- **Score**: headroom against soft caps, plus locality (repo already
  cloned, image already built, which `config_hash`/`build_hash` labels
  tell), plus a small bias for the member the request came from.
- **Soft caps never refuse.** Over a cap a machine scores low; if every
  candidate is over, the best one is used and the verb's output, the
  dashboard header and the Machines tab say `over cap: mem 18G/16G`.
  `--machine m` or a qualified name always wins over the score; `--here`
  forces where it was typed.

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
  fleet root for placed work) and cuts the worktree from it. With a
  reachable `origin`, the receiver fetches from it first so the bundle
  carries only the delta.
- **Route**: along the graph's path (A → K → B in a star); the hub also
  keeps a copy (below), which is what makes re-placement possible.
- **Back**: the placed instance's branch and WIP are fetched back into the
  source repo as `refs/devsandbox/<machine>/<instance>/…` on demand
  (`devsandbox fetch <instance>`, the dashboard) and at `done`/`rm`;
  never a silent rewrite of the user's branches.
- **Hub mirror**: the hub keeps the repo cache for every repo it has
  placed and receives periodic checkpoints (step 13), so a repo's latest
  WIP survives its laptop going to sleep.
- **Config roots** move the same way (a config root that's a git repo
  as a repo, any other as a content-hashed tar of the root), so a placed
  config runs as its home member resolved it, dockerfiles and local
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

### Versions

Every member runs the hub's exact build, and updates itself: it learns
the hub's version from the link hellos, and when it differs fetches that
version, installs it and lets its own version handoff restart its daemon
([ADR 0015](../../decisions/0015-newest-binary-wins-version-handoff.md)):

- **Published version**: the release asset for its own platform from
  GitHub (`devsandbox-<v>-<target>.tar.gz`, as
  `.github/workflows/release.yml` names them), verified, installed
  atomically (the existing `install::INSTALL` steps).
- **Local build** (not a published version): the hub's own binary over
  `TRANSFER` (along the path) when the OS and arch match
  (`install::platform_matches`) and the member accepts dev builds
  (`dev-builds`, or `full`: it's another machine choosing code this one
  runs outside a container); else the member says "dev build: can't run
  on <os>/<arch>" and takes no work. Accepted: a dev fleet of mixed
  platforms runs published builds or is built per platform by hand.
- Release targets today: linux x86_64/aarch64 (musl), macOS aarch64,
  windows x86_64. An Intel Mac can't auto-update until a target is added.

### Credentials

The hub's agent reaches the member's instances over `AGENT` while
the hub's registry entry for the member has `agent = true`, as the agent
relay does for a machine today: every instance on the member, not only
placed ones (decision 14). The hub's agent is whatever the hub has: a
dedicated key on the server (deploy or user key), not a laptop's. A
member's own agent never leaves it. With one link per pair (step 7)
that holds for the dialer: its agent reaches every instance on the
member while its entry has `agent` on, relayed sockets picked before the
member's own, as today (decision 18).

## Rollout

Each step is usable on its own. Order with the other plans is in
[helper-rpc](helper-rpc.md#order-across-plans): steps 1-3 run alongside
helper-rpc's 1-3; step 4 waits for helper-rpc's `session.attach`; step
12 for its daemon-owned runs.

1. **Fleet link and hub mode** (L, shipped). The frame-mux link with
   `API` channels, the confined table over a fleet root, the worker
   policy, persisted remote rows, capacity in the snapshot.
2. **Self-update** (S–M, shipped). A published hub build downloaded, a
   dev build pushed when accepted.
3. **Placement v0 + minimal transfer** (M, shipped). `place`, soft caps
   and warnings; a placed run gets the config root as a tar and a bundle
   the hub cloned from `origin` (committed, pushed work only); `--here`;
   image tags and feature build dirs namespaced by config root (P9).
4. **Interactive over the link** (M, shipped on offered workers' links). `PTY` (exec, terminal tabs, the
   reconnecting `exec`), `TUNNEL` forwards, `AGENT`, CLI re-runs as
   API calls. Step 7 needs
   it: the per-use ssh argvs go there.
   `PTY` is a session attach ([resumable-exec](resumable-exec.md)): the
   channel opens with `{instance, session id, create|attach}` and ends
   with a structured record (`exit <code>` | `no-session` | `detached` | `version` |
   `lost`); a link drop is `lost`, never an exit, and records are
   relayed unchanged by every hop. Session `ls`/`rm` within the caller's
   trust. The member's daemon attaches directly (decision 13).
5. **The rest over the link** (M). On the fleet link, like step 4, and
   before anything deletes an ssh argv, so an ssh machine never loses a
   command in between: API calls for `ps`, `stats`, `service ls` and
   `logs --follow`; a piped `PTY` mode for a non-tty `exec`; a host-pty
   `<runtime> exec -it` fallback on the member for service terminals and
   helper-less or non-root-helper instances (decision 13's refusals
   lifted); `gc` over the link refused under `worker`, allowed under
   `full` once step 6 has trust. Ssh machines keep their argvs until
   step 7.
6. **Member file and trust** (M). [Trust](#trust-worker-or-full):
   `member.toml` takes `name`, `trust`, `[caps]`, `[allow]`, `dev-builds`
   and the picked `roots` (decision 17); `member.get` / `member.set`,
   daemon-written (decision 3); `machine trust worker|full`. The confined
   table (`fleet::lent`) becomes the trust table: `worker` admits the
   member's own roots plus the fleet root, placed work under the worker
   policy (decision 9); `full` skips the table and the policy. Applied to
   every remote connection, ssh machines' (`api --stdio`) too. Lending
   state (`state`, `hub`) stays in `lend.toml` until step 8.
7. **One link per machine pair** (L).
   [Members, links and the hub](#members-links-and-the-hub): a
   `Transport` trait (dial → `Duplex`), ssh its only implementation;
   several connections would only ever be a bandwidth detail behind it,
   never a second path in the code. Every registry entry is held by its
   dialer (decision 7) as `fleet-link --peer <dialer>` (decision 22): the
   dialer's daemon is the answerer and binds
   `fleet-<X>.{sock,pty,tunnel,transfer}.sock` locally; on X,
   `fleet-link` is the opener and hands each stream to X's daemon with a
   `link.attach {channel, peer}` first line, and X's daemon applies the
   peer's trust. X's daemon knows inbound links as neighbors, so both
   ends see each other and either opens any channel; the dialer's agent
   reaches the member as decision 18 says. Removed: `machines::Kind`
   (entries are this host's links), the per-use ssh argvs, the
   `api --stdio` / `tunnel` / `agent-relay` relays for machines,
   `offered_refusal` and the `is_offered()` branches. `fleet join|leave`,
   `devsandbox lend`, `lend.*`, `fleet.offer` and the lending thread give
   way to `machine add`; placement candidates are every neighbor;
   `place`'s `drain` and capacity's `lending` go. `lend.toml` goes once
   step 8 has `hub`.
8. **Membership and the hub** (M).
   [Joining and leaving](#joining-and-leaving): `hub` in `member.toml`
   (decision 1), `[serve] hub` gone; `machine add`'s join rule (a name
   already in the fleet refused, decision 2; two hubs refused, decision
   19), `machine unlink|rm`. The hub holds the member list; members watch
   it with `machines: true` (the asking machine's rows never
   re-exported), so every member's snapshot is federated. A `run` typed
   on a member is placed by the hub (decision 20; with the hub
   unreachable it runs where it was typed, with a note); `remotes.json`
   saved rows on the hub; versions follow the hub, keyed on who is hub
   rather than on the link's direction.
9. **Routing over the graph** (M–L). Members report their links and the
   hub publishes the graph; routed streams are spliced hop by hop toward
   a destination (an `Open` naming it); requesters try paths in order
   (direct, then fewest hops, preferring hops that stay up) and fail over
   (decision 21). `machine trust worker`'s ssh warning comes from the
   graph, shown in `machine ls` and the Machines tab. ADR 0043 accepted
   (0041 removed); `machines.md` and `fleet.md` (architecture)
   rewritten.
10. **Hub swap, pointer** (S–M). `machine hub <m>`: the role moves, links
    stay; every reachable member rewrites `hub`, a late one learns it on
    reconnect.
11. **Transfer v1** (L). Repo identity, WIP snapshot commits, bundles
    routed over the graph, repo caches, the hub mirror, fetch-back.
    Repos without a remote and unpushed work can be placed.
12. **Dispatcher children on any machine** (M). Instances carry
    `origin_host` and repo identity; `ls`/the dashboard show where they
    run. The dispatcher (on the hub) keeps authorizing on its host
    ([ADR 0030](../../decisions/0030-dispatcher-control-is-authorized-on-the-host.md));
    `dispatch::Executor`'s subprocess becomes a placed, forwarded verb;
    the hub's `state.toml` records child → machine; `exec`/`run-*` ops go
    through forwarded calls; `branches` reads the hub's repo cache.
    `max-instances` stays per dispatcher, fleet-wide.
13. **Checkpoints and re-placement** (M–L). Placed instances checkpoint
    WIP to the hub's mirror (on idle, every few minutes while dirty, at
    stop). A child whose member is unreachable past a grace period, or
    `machine rm`'d, is re-ensured on another machine from its last
    checkpoint: same name and `instance_id`, the old copy fenced (its
    member, when back, is told the instance moved: stop it, keep its WIP
    ref, no auto-delete). Process state is lost, committed and
    checkpointed work isn't. `devsandbox move <instance> [--to m]` is the
    manual form. Fencing a copy ends its sessions: live or detached
    sessions extend the grace period (or at least warn), and an attach to
    a moved instance says `moved to <machine>`, not `no session`.
14. **The rest of the fleet view** (M). The Inbox behind the daemon on
    every member, with a federated thread list (machines-next §2);
    nameless commands across the fleet (machines-next §3).
15. **Sync layer** (L, later). A second `Transfer` for state git doesn't
    carry: ignored outputs, volumes, caches, live editing. Then build-cache
    sharing (a registry on the hub, `--cache-from`) and services that span
    machines (P10) can be revisited.
16. **Hub handoff** (M). `machine hub <m>` with the old hub alive ships
    its state to `m` over `TRANSFER` (repo cache, saved rows, dispatcher
    and placement records, checkpoints) before the role moves; the
    pointer swap (step 10) stays for a dead hub.
17. **Fallback hub** (M, later). [A fallback hub](#swapping-the-hub):
    settle who decides and the old hub's return first.

## Decisions

Numbered as code cites them; 13–16 as step 4 makes them, 3, 9 and 17 as
step 6 does, 7, 18 and 22 as step 7 does, 1, 2, 19 and 20 as step 8
does, 21 as step 9 does.

1. The hub is `hub = "self"` in `member.toml`; no separate daemon
   setting.
2. ssh login is the authentication (the user's own account); `machine
   add` refuses a name already in the fleet.
3. `member.toml` is written by the daemon only, through `member.*` calls
   (the `machines.toml` rule); `machine` is a client.
4. A fleet root is one pseudo-root per member; the `worker` table lists
   the config roots under it.
5. Step 1's acceptance test: a config copied by hand under
   `<data>/devsandbox/fleet/<hub>/`, `run --machine <member>`.
6. Until step 4, paths that need ssh to the machine (`exec`, terminal
   tabs, forwards, CLI re-runs) refuse on an offered machine with
   `needs fleet step 4` wording; `logs` / `inspect` use the
   `containers.*` quick calls. (Spent: step 4 shipped; what an offered
   machine still refuses is in `docs/status.md`.)
7. Every link is always held (no `Want` / linger).
8. Capacity: cpus / memory total from `docker info` (`NCPU`,
   `MemTotal`, cached 60 s); free memory and free disk only for a native
   Linux runtime (`/proc/meminfo`, `statvfs` of the runtime's root), else
   unknown. VM-backed engines report the VM.
9. The worker policy denies by default: binds outside the fleet root,
   `privileged`, runtime socket mounts, host network, `caches`,
   `shared-volumes`, `worktree-link`, `global` services, inbox /
   dispatcher sockets. Allowances per member in `member.toml`; none under
   `full`.
10. Self-update: the hello carries git hash + `published`; a member
    follows the hub's exact version (a downgrade is a forced handoff);
    installs in place at `exe::devsandbox()`. Verified by TLS + running
    `--version`; no checksum asset (a `release.yml` change not approved).
11. Placement runs in the daemon.
12. Step 3's code: the hub clones `origin` with its own credentials and
    sends a bundle over `TRANSFER`; a placed member never needs
    credentials. Pulls a slice of step 11 forward (bundle of committed
    refs only).
13. `PTY` is a direct attach: the member's daemon calls the helper's
    `session.attach` over the bridge's RPC (`HelperClient::call_with_input`),
    no `exec -it` and no in-container client; service terminals,
    helper-less instances and instances whose helper daemon isn't root
    (`session.attach` is `needs_root`, e.g. `containerUser`) refuse over
    the link until step 5's `exec -it` fallback.
14. `AGENT` on the link keeps the agent relay's semantics: the linking
    host's agent, while its registry entry has `agent = true`, reaches
    every instance on the machine (not only placed ones as
    [Credentials](#credentials) first said).
15. CLI re-runs to a member are API calls within its trust (quick calls,
    snapshots, verbs), never a `devsandbox …` run there.
16. Step 4 builds the channels on `fleet::LINK` (offered workers); ssh
    machines keep their per-use argvs until step 7 makes every link a
    fleet link.
17. A member's fleet-visible roots are those its `state.toml` has
    instances in, plus those picked at `machine add` (kept in the
    member's own `member.toml` `roots`).
18. Decision 14 holds under the one-link shape: the dialer's agent
    reaches every instance on the member while the dialer's entry has
    `agent` on (relayed sockets picked before the member's own, as
    today).
19. One fleet per member: `machine add` refuses two hubs.
20. Once a fleet exists, `run` places by default; `--here` /
    `--machine` / a qualified name override.
21. Routing is the full graph form (step 9), not hub-relay only.
22. Link shape: the dialer's daemon is the answerer and binds the
    per-link sockets; the far end's `fleet-link --peer <dialer>` hands
    each stream to its daemon with `link.attach`; the peer's name is
    authenticated only by ssh login (decision 2).

## Open questions

- Fencing in step 13: is "the member stops the stale copy when it
  reconnects" enough, or must a placed instance hold a lease it renews
  with the hub and stop itself when the lease lapses (laptop offline but
  awake, e.g. off the tailnet)? Either way it ends that copy's sessions,
  which a user may still mean to reattach to (step 13).
- Checkpoint cadence and size: big untracked files in WIP commits; a size
  cap with a warning, or `.devsandboxignore`?
- Hub mirror retention: when are WIP refs and repo caches of removed
  instances pruned?
- Services: does a placed child's `global` service move with it, or must
  `global` services pin their dependents to one machine?
- Worker policy defaults: per-member allowances for now (decision 9);
  per-sandbox allowances if docker-in-docker features make that too
  coarse.
- Authenticating peers without ssh (later): a transport over the
  tailnet, say, where no ssh login vouches for `--peer` (decision 22).
