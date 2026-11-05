# Fleet: machines as one pool of capacity

Status: in progress. Steps 1-12 shipped
([fleet](../../architecture/daemon/fleet.md), ADRs
[0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md),
[0043](../../decisions/0043-every-machine-is-a-fleet-member.md),
[0044](../../decisions/0044-one-fleet-authority-signs-membership.md),
[0045](../../decisions/0045-a-routed-stream-proves-its-origin-per-stream.md),
[0046](../../decisions/0046-routed-streams-are-end-to-end-sessions.md)):
steps 1-6 on two kinds of machine (ssh machines and lending workers),
steps 7-8 down to one: every machine one peer link per pair, reached
only over its channels, a laptop joining with `machine add <hub>`;
step 9 made it a fleet: the hub in `member.toml`, `machine add`'s join
rule, `machine unlink|rm`, every member seeing the fleet through the
hub and its runs placed there; step 10a gave members identity: a key
each, a membership log only the fleet's authority signs, links that
prove both keys, changes made elsewhere approved on the authority;
step 10b routed streams over the members' signed link graph, each
proving its origin to its destination; step 10c made each routed
stream an end-to-end session and agent access a per-member grant
carried over any path; step 11 moves the hub role with `machine hub`,
an authority-signed log entry; step 12 moves code itself: each folder
from its own checkout with a WIP snapshot (no remote needed), into
per-repo caches, a copy on the hub, and `devsandbox fetch` back. Step
13 is next. The rest is design. Builds on
[machines](../../architecture/daemon/machines.md);
[machines-next](machines-next.md) items 2 and 3 fold into step 15 here,
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
down, X is `unreachable`: placement skips it, re-placement (step 14) may
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

`machine hub <m>` (step 11; a pointer swap for now). Links don't change:
the role moves. It's a `hub` entry the authority signs (decision 68:
typed elsewhere, proposed and approved there), pushed to every
reachable member (each rewrites `hub`); a member unreachable then learns
it from the first member it reconnects to. A dead hub can be replaced
only while the authority is up, so not in a fleet made without `--hub`,
where they're one machine (step 18). `m` starts empty: rows and the
graph come back as members report, the repo cache refills as code moves.
Handing the hub's state over (repo cache, saved rows, dispatcher records,
checkpoints) is step 17, once steps 13-14 make that state matter; the
pointer swap stays for a dead hub.

**A fallback hub** (step 18, later). An ordered list of members that take
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
  repos with a reachable `origin`; from step 12: any, by transfer).
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
  placed and receives periodic checkpoints (step 14), so a repo's latest
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
13 for its daemon-owned runs.

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
   API calls. Step 8 needs
   it: the per-use ssh argvs go there.
   `PTY` is a session attach ([resumable-exec](resumable-exec.md)): the
   channel opens with `{instance, session id, create|attach}` and ends
   with a structured record (`exit <code>` | `no-session` | `detached` | `version` |
   `lost`); a link drop is `lost`, never an exit, and records are
   relayed unchanged by every hop. Session `ls`/`rm` within the caller's
   trust. The member's daemon attaches directly (decision 13).
5. **The rest over the link** (M, shipped on offered workers' links; `gc`
   with step 6). On the fleet link, like step 4, and
   before anything deletes an ssh argv, so an ssh machine never loses a
   command in between: API calls for `ps`, `stats`, `service ls` and
   `logs --follow`; a piped `PTY` mode for a non-tty `exec`; a host-pty
   `<runtime> exec -it` fallback on the member for service terminals and
   helper-less or non-root-helper instances (decision 13's refusals
   lifted); `gc` over the link refused under `worker`, allowed under
   `full` once step 6 has trust. Ssh machines keep their argvs until
   step 8.
6. **Member file and trust** (M, shipped). [Trust](#trust-worker-or-full):
   `member.toml` takes `name`, `trust`, `[caps]`, `[allow]`, `dev-builds`
   and the picked `roots` (decision 17); `member.get` / `member.set`,
   daemon-written (decision 3); `machine trust worker|full`. The confined
   table (`fleet::lent`) becomes the trust table: `worker` admits the
   member's own roots plus the fleet root, placed work under the worker
   policy (decision 9); `full` skips the table and the policy. Applied to
   every remote connection, ssh machines' (`api --stdio`) too. Lending
   state (`state`, `hub`) stays in `lend.toml` until step 8. As built:
   an ssh machine's relay attaches its connection with `link.attach
   {peer}`; `machine add` records the picked roots on the member
   (`machine roots add` there over its add-time ssh); `gc` is a daemon
   verb, `full` only; the per-use ssh argvs still bypass trust until
   step 8.
7. **One link per machine pair** (L, shipped as 7a-7e).
   [Members, links and the hub](#members-links-and-the-hub): a
   `Transport` seam (dial → `Duplex`), ssh its only implementation;
   several connections would only ever be a bandwidth detail behind it,
   never a second path in the code. Every registry entry is held by its
   dialer, always (decision 7), as `fleet-link --peer <dialer>` on the
   far end (decision 22). Both ends run both halves (decision 23): OUT
   binds one `fleet-<other>.sock` for local clients and opens the
   channel each accept names in its first byte; IN hands each of the other
   end's streams to this daemon under this host's trust table. On the
   dialed end that's `link.attach {channel, peer}` as the stream's first
   line on the daemon socket; on the dialer, the daemon in-process. The
   dialed end knows the link as an inbound neighbor; the dialer's agent
   reaches the member as decision 18 says.

   ```
    L (dialer)                                   K (dialed)
   ┌─────────────────────────┐ Transport::dial ┌─────────────────────────────┐
   │ L's daemon                │ (ssh -T -o Batch│ devsandbox fleet-link --peer L│
   │  entry K, held always     │  Mode=yes K     │  (ssh's child on K; its stdio │
   │  reads the sentinel       │  devsandbox     │   is the link; writes the     │
   │                           │  fleet-link …)  │   sentinel)                   │
   │ OUT: fleet-K.sock         │ ═══════════════►│ OUT: fleet-L.sock             │
   │  each accept: first byte  │ one byte pipe,  │      agent-link-L.sock        │
   │  names ch → Open(ch)      │ frame mux:      │  each accept: first byte      │
   │                           │ API PTY TUNNEL  │  names ch → Open(ch)          │
   │                           │ TRANSFER AGENT, │                               │
   │                           │ either end opens│ IN: Open(ch) from L → K's     │
   │ IN: Open(ch) from K → L's │                 │  daemon socket, first line    │
   │  daemon in-process, peer K│                 │  link.attach{ch, peer: L}     │
   │  under L's trust table    │                 │  → K's trust table for L      │
   └─────────────────────────┘                 └─────────────────────────────┘
   ```

   `AGENT` opens only member → dialer: L answers it while L's entry for K
   has `agent` on. The hello carries the hub flag and versions follow it
   (decision 25). Ssh machines move onto the link here; offered workers
   (lending) keep theirs until step 8, as do the per-use ssh argvs.
   - **7a Transport seam + symmetric link body** (refactor).
     `fleet/transport.rs`: `Transport::dial(target, args) -> Duplex`,
     ssh its only impl (`worker::link_argv`, lending's transport,
     `ssh::target_argv` behind it). `fleet/link.rs`: `opener_with` and
     `answerer_with` share one post-handshake body (an `Endpoint`: what
     to bind as OUT, the `Serves` table as IN).
   - **7b `link.attach {channel, peer}` for every channel** (daemon).
     `api`, `pty`, `tunnel`, `transfer` (per-peer receivers), `agent`
     (the host agent iff this host's entry for the peer has `agent` on);
     the peer is `{name, fleet_root: fleet/<peer>/, env: the daemon's}`
     (decision 24). One `serve_channel(channel, peer, stream)` behind
     both the socket and the in-process paths.
   - **7c `fleet-link --peer <dialer>`**. IN through `link.attach`, OUT
     binding `fleet-<peer>.sock` + `agent-link-<peer>.sock`; the
     inbound neighbor announced (`links.inbound`) for the link's life
     and shown by `machines.list`, a name clash with an outbound entry
     refused. `--worker/--root` stays until step 8.
   - **7d ssh machines on the link**. `Machines` holds every ssh entry
     through the `Transport`, always (`Want`, linger and `machines.hold`
     go); `Dial` is the entry's `fleet_socket`; the agent over `AGENT`
     (the `agent-relay` thread no longer started); no `api_argv` /
     `--session-env` for machines.
   - **7e docs close**.

   As built: `links.inbound` and `link.attach` are local only (the
   trust table refuses `link.*` and `links.*`); an inbound neighbor is a
   `machines.list` row of kind `inbound`, its roots asked once at the
   announce; a peer's verbs run with the daemon's env, `SSH_AUTH_SOCK`
   its relayed `agent-link-<peer>.sock` while that answers
   (`lent::Peer::agent`); `machines.hold` went, a CLI re-run reports its
   terminal's agent with `machines.get {agent}`; the dialer's IN names
   the peer by its registry entry, so its fleet root there is
   `fleet/<entry name>/`. A daemon with machines still idles out (a link
   isn't a holder).
8. **One kind of machine** (L, shipped as 8a-8f). Every entry reached through
   its channel forms; inbound neighbors are machines (decision 28);
   lending removed (decision 26): a laptop joins with `machine add <hub>`.

   ```
    laptop ──machine add hub──► hub      (the laptop dials; the hub lists
      fleet-hub.sock ◄═link═► fleet-lap.sock         "inbound lap")
      every client on either end: API / PTY / TUNNEL / TRANSFER there;
      the hub places on lap through fleet-lap.sock; AGENT only lap → hub
   ```

   Nothing is deleted before its replacement is used:
   - **8a CLI on channels for every machine.** `machines/cli.rs`
     `plan`/`route`: the offered forms (quick calls, verbs, `PTY` exec
     with reconnect, piped exec, `logs --follow`, `service ls`, `gc`)
     the only path, `offered.rs` merged in; `ssh::cli_argv` left only
     for `machine add`'s `machine roots add`.
   - **8b Dashboard terminals, forwards and `port` on channels.**
     `tui/remote.rs` tabs via `fleet::pty_client` for every machine;
     `serve/forwards.rs` `via()` and `commands/port.rs` always
     `Via::Fleet`.
   - **8c Delete the per-use ssh paths.** `tty_argv*`, `cli_argv` but
     add-time, `tunnel_argv`, `api_argv`, `agent_argv`, `devsandbox
     tunnel`'s ssh half, `agent-relay`, `machines/agent.rs`, the relay's
     `--session-env` / `SessionEnv` / `attach`, `link.attach` without a
     `channel`, `offered_refusal`, the sentinel inside link `API` streams
     (decision 27). `api --stdio` stays for npm.
   - **8d Inbound neighbors are machines.** `Machines` runs a link over
     `fleet-<n>.sock` per announced inbound neighbor (its carrier is the
     `fleet-link` process): rows, capacity, quick calls, verbs,
     dashboard rows; placement candidates are every connected machine
     (`fleet/placed.rs` `workers()`), offered ones' `drain` honored
     until 8e.
   - **8e Delete lending.** `fleet join|leave`, `devsandbox lend`,
     `lend.*`, `lend.toml`, `fleet.offer` / `fleet.agent`, `fleet-link
     --worker`, `fleet/worker.rs` `Lending`, `fleet/lender.rs` (the
     `member.toml` path into `Shared`), capacity's `lending` and LEND,
     `place`'s `drain`, `machines::Kind` and the `is_offered()` branches
     (decision 29).
   - **8f Docs close.**

   As built: a hub only places (`[serve] hub`; a `run` elsewhere stays
   there until step 9), over every connected machine; an inbound
   machine that goes takes its rows and forwards with it; each link a
   host dials holds its daemon up (`idle::Holders::links`); `machine
   add`'s probe is `api --stdio --sentinel` over ssh, the only ssh but
   the link's; `roots.list` marks the caller's fleet root (`fleet:
   true`), which placement moves code into; no command sets the member
   name (`member.set {name}` only), and `drain` went with lending.
9. **Membership and the hub** (M).
   [Joining and leaving](#joining-and-leaving): `hub` in `member.toml`
   (decision 1), `[serve] hub` gone; `machine add`'s join rule (a name
   already in the fleet refused, decision 2; two hubs refused, decision
   19), `machine unlink|rm`. The hub holds the member list; members watch
   it with `machines: true` (the asking machine's rows never
   re-exported), so every member's snapshot is federated. A `run` typed
   on a member is placed by the hub (decision 20; with the hub
   unreachable it runs where it was typed, with a note); `remotes.json`
   saved rows on the hub; versions follow the hub as `member.toml` names
   it (step 7 reads `[serve] hub`).
   Decisions 31-35 settle what the design above left open. Sub-steps,
   each a commit:
   - **9a `hub` in `member.toml`.** `hub = "self" | "<name>"` (absent:
     no fleet), `member.get`/`member.set {hub}`; `[serve] hub` goes and
     every reader reads the member file at its use (hello, keep-alive,
     placement, `remotes.json`); the hello names the hub (decision 34);
     a hub change ends this host's links so both ends redial.
   - **9b `machine add`'s join rule.** The table in
     [Joining and leaving](#joining-and-leaving) over both ends'
     `member.get` (the target's over the add-time probe), the joiner's
     `hub` written; names (decision 35) and two hubs refused.
   - **9c Members see the fleet through the hub.** A member watches the
     hub with `machines: true`, the asking member's rows never
     re-exported; machines seen only that way are view-only (decision
     33).
   - **9d `machine unlink|rm`, leaving.** Today's `rm` becomes
     `unlink`; `rm` per decision 32; `rm` of this host leaves.
   - **9e A member's `run` placed by the hub** (decision 31).
   - **9f Docs close.**

   As built: the daemon keeps `hub` in memory (`Shared::hub`, read at
   start, set by `member.set`); a change cycles the links it holds
   (`service::Handle::cycle`) and sends `links.cycle` to each
   `fleet-link` it's dialed on. The hello is `<build> <hub|member>
   <hub name|->` (`update::same_fleet`). `machine add` pins both
   joiners' member names and reserves `self`; `--name` renames the
   target only while it joins. A member watches the hub's fleet root
   for it with `machines: true` (a peer's federated watch is served on
   the hub only); machines seen that way are `via` the hub, one hop,
   and the federated `instances.resolve` / `sandboxes.list` don't ask
   them. `machine unlink` is the old `rm`; `machine rm` runs
   `fleet.leave` / `fleet.forget` from the hub (`machines::leave`,
   admitted by `lent::admit_fleet`), a member's sent there as
   `fleet.rm`; the links a call ends go after its answer is written.
   `fleet.place` answers `{on: requester|hub|machine, machine,
   lines}`; a member asks it on every unpinned `run`.
10. **Identity, then routing over the graph** (L). Identity first
    (decision 44). Sub-steps:
    - **10a Member identity.** Each member holds a private key; a link
      proves both ends' keys in its handshake, so no device acts as
      another member, or as the hub, without that member's key.
      - **Signed statements:** signer, payload hash, a version that only
        goes up per signer; every verifier keeps the highest version it
        has seen and refuses older (rollback). What's signed: membership,
        keys, revocations, the hub, config snapshots (the whole config
        root's hash plus each moved repo's commit ids, checked by the
        worker against what arrives; decision 39, steps 12-13) and
        delegated config edits.
      - **The authority** (decision 45) signs membership, key changes,
        revocations and hub changes. The hub stores and serves them,
        vouching for nothing: a key the hub (or a relay) hands over with
        no authority's or introducer's signature is refused.
      - **Membership is a hash-chained log** whose head members compare
        with each other directly on every link, so a hub showing two
        members different lists is seen.
      - **Joining:** `machine add` shows the joining key's fingerprint
        to check (ssh's first-use trust is the root). A member added by
        a non-authority member is signed by that introducer and capped
        by it (decision 46).
      - **Revocation and rotation:** `machine rm` revokes the key, signed,
        pushed to every member and fetched again on reconnect; a member
        can rotate its key.
      - **Key at rest:** never reachable from a container (a bind whose
        source covers the key path is refused on every host, not only
        under the worker policy) nor through an ssh-agent that is
        relayed (decision 30); the OS keychain where there is one.
      - **No fallback:** a link that doesn't prove its keys is refused.
        Decision 32's hub-only calls check the hub's key, not the name
        the link announced.

      Sub-steps, each a commit (decisions 48-52 settle what the above
      left open):
      - **10a.1 Identity primitives** (`fleet/identity.rs`,
        `ed25519-dalek`). A `KeyStore` seam, its file implementation
        `member.key` next to `member.toml` (0600, created under a lock,
        read by the daemon and `fleet-link`); public keys as
        `ed25519:<hex>` with a fingerprint; sign/verify with domain
        separation; `Statement {signer, key, kind, payload_sha256,
        version, sig}` and the pure rollback check (the highest version
        seen per signer and kind, older refused). `member.get` answers
        `key` and `fingerprint`; `devsandbox machine key` prints them.
      - **10a.2 The membership log** (`fleet/membership.rs`, pure;
        `membership.json` next to `state.toml`, daemon-written). A hash
        chain of authority-signed entries: `Genesis {authority, key,
        hub}`, `Add {name, key}`, `Revoke {name, key}`, `Rotate {name,
        old, new}` (the new key countersigned by the old) and `Hub
        {name}`. `apply` verifies every entry into the members, the hub,
        the revoked keys and the head; `compare` says same, behind, ahead
        or fork. Introductions sit outside the chain (decision 50).
        `member.toml`'s `hub` is written only from an accepted log.
      - **10a.3 `machine add` writes the log.** A fleet-creating add
        signs `Genesis` and the target's `Add` here (decision 48); an add
        by the authority signs `Add`; an add by another member signs an
        introduction and proposes it to the authority (decision 49); a
        host joining the target's fleet gets its introduction (or `Add`,
        the target being the authority) signed there over the add-time
        probe and takes the log from it. The joining key's fingerprint is
        shown and confirmed on a TTY (`--yes` off one).
      - **10a.4 The link proves both keys.** After the `Hello`s
        (`link::opener_with`, `link::answerer_with`) each end sends a
        nonce, then signs the transcript (both nonces, both `Hello`s,
        both names); the peer's name must resolve to its key in the log
        or a capped introduction. Unknown names, revoked keys and links
        that don't prove are refused; `fleet::LINK`'s sentinel version
        goes up. Heads are compared on every link: the end behind fetches
        the entries it lacks (`fleet.membership`, self-verifying, from any
        member); a fork refuses the link and is logged.
      - **10a.5 Trust by the proven key.** `lent::Peer` carries the
        proven key and its introducer (`link.attach` from `fleet-link`,
        the dialer's in-process peer); `lent::admit_fleet` admits the
        hub-only calls by the log's hub key; a capped member introduces
        no one (decision 51).
      - **10a.6 Revocation, rotation, approvals.** `machine rm` is a
        `Revoke`, `machine key rotate` a `Rotate`; typed on the
        authority they're signed there, elsewhere proposed to it
        (`fleet.propose`) and approved there (decision 49). A signed
        change is pushed to the hub and the authority's linked members,
        the rest catch up on their next link's head comparison; step 9's
        `fleet.leave`/`fleet.forget` follow a valid `Revoke`.
      - **10a.7 Key at rest.** A bind whose source covers `member.key` is
        refused on every host (mount assembly, not the worker policy);
        the key is never loaded into an ssh-agent; OS keychains behind
        `KeyStore` are parked (decision 52).
      - **10a.8 Docs close.** An ADR for decision 45; `fleet.md`
        (architecture), `docs/status.md`, this step's as-built note.

      As built: there is no `fleet.membership` call; the entries an end
      lacks travel in the key proof's `sync` round (`fleet::proof`),
      after a `hello` round (names, nonces, heads), and a fourth
      `confirm` round has the opener's verdict on the answerer's proof.
      The proof sits on the raw pipe before the mux, `fleet::LINK`'s
      sentinel at version 2. A signed change is pushed to every machine
      the authority holds and flooded on by each host whose log it made
      newer (`fleet.sync`, `membership_api::sync`). `member.set` takes no
      `hub`; `machines.add` takes a mandatory `key` (the fingerprint the
      user confirmed, checked against the target's `member.get`) and
      `hub`, and `membership.sign` answers unchanged for a name already in
      under that key, so a failed add can be run again. `membership.json`
      and `proposals.json` sit next to `member.toml`. A rotation makes
      `member.key.next`, promoted over `member.key` once a log the host
      takes lists it (`membership_api::settle`), after which the host's
      links cycle and redial (`Shared::key_rotated`). Introductions made
      by a non-authority `machine add` are proposed as `fleet.propose
      {kind: add}` after the add. No command signs a `hub` entry yet
      (step 11). The key guard (`key_guard`) also refuses a build whose
      context, dockerfile, `--build-context`, `--secret` or `--ssh` path
      covers the key.
    - **10b Routing.** Members report their links and the hub publishes
      the graph; routed streams are spliced hop by hop toward a
      destination (an `Open` naming it), each naming the member it
      started on, proven by that member's key (10a), so a relay can't
      speak for another member and each destination applies its trust
      for the stream's origin; requesters try paths in order (direct,
      then fewest hops, preferring hops that stay up) and fail over
      (decision 21). `machine trust worker`'s ssh warning comes from the
      graph, shown in `machine ls` and the Machines tab. ADR 0043
      accepted (0041 removed); `machines.md` and `fleet.md`
      (architecture) rewritten. Until 10c a relay still sees and can
      alter a routed stream's bytes after its opening.

      Sub-steps, each a commit (decisions 53-60 settle what the above
      left open):
      - **10b.1 The graph model** (`fleet/graph.rs`, pure). A member's
        link report (decision 59) as an `identity::Statement` of kind
        `links`, its version `max(now_ms, last + 1)`, older refused
        through `Seen`; `Graph::apply` takes a report only from a log
        member or a held introduction, never a revoked key;
        `Graph::paths(from, to)` in order: direct, fewest hops, hops
        through always-on members, the oldest `since`; an edge is up when
        the newer of its two ends' reports says so.
      - **10b.2 Reports and the published graph.** Each member sends
        `fleet.report` to the hub on a link-state change (debounced) and
        when its hub link comes up; the hub collects them and pushes the
        graph to its linked members as `changes::push` does the log.
        Every member keeps the graph it has and answers `fleet.graph`
        with it (the reports verify themselves, so a member with no link
        to the hub gets it from a neighbor); `graph.get` for local
        clients. `lent::admit_fleet`: `fleet.report` on the hub only,
        `fleet.graph` from any log member or held introduction.
      - **10b.3 The routed opening** (`fleet/route.rs`). `channel::ROUTE`
        and `caps::ROUTE` (decision 53; the helper never sees it),
        `lent::Channel::Route`; the header, the challenge-response both
        ways (decision 54) on `proof`'s framing (`Chan`, `write_raw`,
        `read_raw`, `resolve` shared), refusals for a forged or revoked
        origin, a replay, a relay posing as the destination, a TTL of 0,
        a loop, an oversized header.
      - **10b.4 Relays and destinations.** `Shared::serve_channel`
        serves `ROUTE` on both link ends (`Dialer::answer`,
        `fleet_link::run_peer`): a relay checks its next hop is a direct
        link that's up and splices (decision 56); the destination
        verifies the opening and serves the inner channel under its trust
        for the origin (decision 57; `AGENT` refused, decision 58). A
        three-daemon fixture next to `Pair`.
      - **10b.5 Paths and failover.** A per-member `route-<name>.sock`
        the daemon binds for every member in its graph; each accept
        tries `Graph::paths` in order within the handshake budget and
        moves on at a refusal, a timeout or a failed connect.
        `route::connect(dir, name, channel)`: `fleet-<name>.sock` first,
        else `route-<name>.sock`; every link-socket caller uses it.
      - **10b.6 Routed machines get actions** (decision 60). Routed
        entries in `Machines` (no link of their own; rows still the hub's
        view), quick calls, verbs, terminals and forwards over
        `route::connect`; decision 33's `seen_via` refusals go, except
        the agent toggle's (decision 58).
      - **10b.7 Fleet calls over any path.** `changes::propose` (lifting
        decision 49's direct-link limit) and an add's introduction,
        `leave::remove`'s `fleet.forget`, `ask_hub`; `run --place`
        candidates and code moves to routed machines
        (`placed::reachable_workers`, `member_workers`).
      - **10b.8 Exposure from the graph.** `graph::exposure`: the hub is
        exposed; so is every member an exposed one dials (it has a shell
        there) and every `full` member an exposed one reaches, to a fixed
        point, each with its chain. `member.get`'s `exposed_by`, `machine
        trust worker`'s warning naming it, `machine ls` and the Machines
        tab ([What `worker` protects](#trust-worker-or-full)).
      - **10b.9 Docs close.** ADR 0045 (a routed stream's origin proof),
        ADR 0043 accepted and 0041 removed (links to it fixed),
        `machines.md` and `fleet.md` rewritten, `docs/status.md`, this
        step's as-built note.

      As built: `fleet.graph {reports}` is both the link-up exchange
      and the push, and every member, not only the hub, floods a graph
      change to its connected links (not back to the one peer it came
      from). A member's report is due at once on its hub link coming up
      and on a `member.set` that changes `trust`. `route-<name>.sock` is
      bound only for members with no connected link here, and
      `route::connect` tries `fleet-<name>.sock` first; the held link's
      own dial (`dialer::dial`) stays direct-only, since it is the link
      state this host reports. `relay_header` checks the next hop and
      destination against the relay's stored log, and every hop checks
      the header came from the hop before it (`route::came_from`). Each
      read of an opening waits at most `route::OPENING` (10 s) at a hop,
      each path's attempt `routes::ATTEMPT` (5 s), all within the hello
      wait. Routed machines are the graph's members with a path and no
      link here plus the hub view's (`snapshots::routed`), `via` the
      first hop that's up (the hub for one only the view lists); the
      dashboard's Machines tab shows only the view's, `via` the hub.
      Exposure counts a link that dials whatever its state, and a
      reach over links in any state.
    - **10c End-to-end sessions** (its own step, decision 47). Each
      routed stream is an authenticated, encrypted session between the
      member it started on and its destination (a Noise-style handshake
      over both identity keys, 10a), naming its destination and with
      replay protection, so a relay carries ciphertext only: it can't
      read, change, inject into, replay or redirect a stream. `AGENT`
      becomes routed behind a per-member grant (decision 58).

      Sub-steps, each a commit (decisions 61-67 settle what the above
      left open):
      - **10c.1 The session layer** (`fleet/session.rs`, pure over
        `Read`/`Write`). Per-direction keys from the DH output and the
        transcript (decision 62), sealed records with an authenticated
        end per direction (decision 63) and the pump between a plaintext
        stream and a sealed one. Tests: a flipped byte, a dropped,
        reordered, replayed or reflected record, a truncation, an
        oversized length.
      - **10c.2 Encrypted routed streams.** The opening carries both
        ephemeral keys (decision 61) and yields the session;
        `Shared::serve_routed` and `Shared::serve_route` pump through it
        (decision 64). Tampering-relay tests in `route.rs`. ADR 0046
        (supersedes 0045's "a relay sees and can alter bytes").
      - **10c.3 The agent grant** (decisions 65, 67). `member.toml`'s
        grants, `agent_for` by proven key, `machines.toml`'s `agent`
        flag gone, `machines.set {agent}` on any machine, the grantees in
        each member's report (decision 66).
      - **10c.4 Routed `AGENT`** (decision 66). `Header::check` allows
        it; the destination answers by its grant; the instance's host
        probes the members whose report lists it and binds
        `agent-route-<member>.sock` while one serves.
      - **10c.5 Docs close.** `docs/status.md`, this step's as-built
        note, `CHANGELOG.md`.

      As built: `session::pump` runs one direction on a `route-session`
      thread and the other on the caller's (the counted `serve-routed` or
      `serve-route` thread); a record's type follows from its length (an
      END is the only empty one) and is authenticated as its associated
      data. Grants live in `fleet::grants` over `member.toml`'s
      `[[agent]]`; `machines.toml` has no `agent` flag. A host probes a
      granting member through `route::connect` (its link socket when it
      dialed that member, else its route socket) from
      `fleet::agent_routes`, one `serve-agent-route` thread per member,
      skipping inbound neighbors (their `fleet-link --peer` binds
      `agent-link-<member>.sock` as before); the dialed end's
      `fleet-link --peer` now serves `AGENT` too, so a member that was
      dialed can grant its dialer. `AGENT` openings and probes aren't
      logged per stream. A forwarded verb's `SSH_AUTH_SOCK` is still only
      a dialed end's `agent-link-<peer>.sock` (`Shared::fleet_peer`), and
      an agent grant isn't counted as exposure (both in
      `docs/status.md`).
11. **Hub swap, pointer** (S–M, shipped as 11a-11c). `machine hub <m>`: the role moves, links
    stay; every reachable member rewrites `hub`, a late one learns it on
    reconnect.

    Most of it is there from 10a: the log's `hub` entry (`Op::Hub`,
    `membership::Membership` checks its target is a member), `hub`
    written from an adopted log (`membership_api::adopt`, `hub_for`), a
    live hub change cycling every link (`daemon::Shared::set_hub`), a
    late member taking the newer log at its next link's proof and
    redialing (`proof::hub_moved`), and every role reader asking the hub
    at its use. What's missing is a way to sign the entry, and an order:
    `changes::sign` follows the hub (cycling links) before the push, and
    `changes::sync` before its answer is written. Sub-steps, each a
    commit (decisions 68-70 settle what the above left open):
    - **11a `machine hub <m>`** (decisions 68, 69).
      `proposals::Change::Hub {name}` and its `check` (a member of the
      log, not already the hub; an introduced or unknown name `invalid`);
      `membership.hub {name}` in `machines::changes`, modeled on
      `membership.rotate`: on the authority the `hub` entry signed, stored,
      pushed, then followed here; elsewhere proposed (`propose`, a pending
      one of the same name answering its id), answered `{"type":
      "signed"|"proposed", …}`; `membership.approve` signs a `hub`
      proposal (`sign_change`). Decision 69's order: a host follows a hub
      change only after its push (the authority) or after its flood and
      written answer (a `fleet.sync` that made its log newer: a
      `leave::After` variant, as `After::Rotated` does for a key).
      `devsandbox machine hub <m>` (`commands::machine`, `main.rs`)
      prints what the push reached or the proposal's id and the
      `machine approve` to run on the authority. The refusals that point
      at step 11 name `machine hub` (`leave::hub_named`, `proposals::check`'s revoke of the hub, `join.rs`'s
      `--hub` refusal). Docs: `docs/reference/api.md` (`membership.hub`,
      `fleet.propose`'s `hub` kind, `membership.approve`),
      `npm/devsandboxes/index.d.ts` (the `dts` check),
      `docs/architecture/daemon/fleet.md` (*Membership log*, *Approvals and
      pushes*, *Hub mode*), `docs/architecture/daemon/machines.md` (the
      `machine` CLI section).
    - **11b The role follows on every host.** A member the push missed
      (down while the authority signed) learns at its next link's proof
      and redials saying the new hub; everything that reads the role
      follows a live change with no restart: the federated watch moves to
      the new hub (`service::Hooks::hub`), link reports go to it
      (`fleet::reports`), the hub-only `fleet.*` calls are admitted by the
      new hub's key (`lent::admit_fleet`), the old hub stops saving
      `remotes.json` and the new one starts from none
      (`service::save_loop`), members linked to the new hub follow its
      build (`fleet_link::follows`). A test for each that has none, a fix
      for any reader that takes the role once.
    - **11c Docs close.** `docs/status.md` (step 11 shipped, next 12;
      decision 70's limitation), this step's as-built note,
      `CHANGELOG.md`.

    As built: `membership.hub {name}` answers `{"type": "signed"|
    "proposed", …}` (`changes::HubMove`) over the pure `changes::plan_hub`,
    which reuses `RotatePlan`; naming the hub, an introduced or an unknown
    name is `invalid`, a capped asker `denied`. Every authority-signed
    change now goes `changes::sign`, push, `changes::follow` (hub, then a
    rotated key). A `fleet.sync` that moved this host's hub (read from
    `member.toml` before the sync) floods before answering and returns
    `leave::After::Hub`, which also stands for a rotate that came with it
    (one cycle). Both ends of a link cycle it after a move, so it can
    drop twice (`docs/status.md`). 11b found every role reader already
    live: the federated watch (`service::Hooks::hub` each tick), link
    reports (`reports::run` each round), the hub-only `fleet.*` calls
    (`lent::admit_fleet` over the log read per call), `remotes.json`
    (`save_loop`), the peer watch and keep-alive (`Shared::is_hub`) and
    versions (each dial's hello); it added tests only.
12. **Transfer v1** (L, shipped as 12a-12g). Repo identity, WIP snapshot commits, bundles
    routed over the graph, repo caches, the hub mirror, fetch-back.
    Repos without a remote and unpushed work can be placed.

    Today (step 3, decision 12) the placing host fetches each folder's
    `origin` into a cache keyed by URL (`code::repo::cache_dir`) and
    bundles one pushed branch from there; the worker lands it straight
    into a checkout (`code::repo::land`); `Transfer::haves` asks nothing,
    so everything is sent every time. Bundles already go over any path
    (`LinkTransfer` through `route::connect`). Sub-steps, each a commit
    (decisions 71-77 settle what the above left open):
    - **12a Git plumbing** (`fleet/code/repo.rs`, a `fleet/code/wip.rs`
      if it reads better; pure git over temp repos, no wire). Every git
      child bounded (`runtime::bounded::run` with a timeout, the existing
      `repo::run` / `optional` and the tests' `git` helper included).
      Repo identity (decision 71): `identity(checkout, rev)`, the
      unborn repo's `refs/devsandbox/root`. The snapshot (decision 73):
      a WIP commit over `HEAD` through a temp index (`GIT_INDEX_FILE`;
      `read-tree`, `add -A`, `write-tree`, `commit-tree` with a fixed
      devsandbox identity, `update-ref`), the user's index, branches,
      stash and working tree untouched; applying one to a checkout
      (working tree to its tree, index at `HEAD`: modified, deleted and
      untracked files come back as uncommitted). Bundles of given refs
      `--not` a list of haves (unknown ones dropped, none new: no
      bundle), the tips a repo has (capped). All through the `host_git`
      family (ADR 0003).
    - **12b Repo caches and haves on the receiver.** A worker keeps one
      bare cache per repo identity and sender, `<fleet root>/repos/<repo
      id>.git` (`host_git_bare`); a `bundle` transfer names its repo
      and refs in its header and goes into that cache first (a body of
      0 bytes: no new objects), its refs set from the header (each
      checked to be a commit there), then the checkout at `to` is cut
      from the cache (new: the branch checked out, the WIP applied;
      existing: refs as `land` moves them today). `code.haves {repo}`,
      an API call any peer may make about its own fleet root
      (`lent`'s table, `docs/reference/api.md`), answers the roots
      unpacked there and the cache's tips; `Transfer` becomes `haves`,
      `send` and (12e) `pull`, `LinkTransfer::haves` asking it over
      `route::connect`'s `API`, so a root the worker has isn't sent
      again and a bundle carries only what it lacks. The sender is still
      the origin cache here.
    - **12c Send from the source checkout** (decisions 72, 73). `offer`
      bundles from the folder's own checkout: its branch (the requested
      one when the checkout has it, else its current one; a detached
      `HEAD` doesn't move) and a fresh WIP snapshot, on a transient ref
      removed once bundled, `--not` the worker's haves. No `origin`
      anywhere: `repo::fetch`, `cache_dir` by URL and the hub's
      `Options::repos` go, and the `NotMovable` reasons for a missing,
      unreachable or behind origin with them. `place::Spec::repo` is the
      identity. `start_point` keeps a new `--branch` starting at the
      bundled branch.
    - **12d The hub mirror** (decision 75). A placing host that isn't the
      hub, placing on a worker that isn't the hub, sends the same refs
      to the hub's cache for it too (no checkout), best effort.
    - **12e Fetch-back** (decision 76). A pull on `TRANSFER`
      (`transfer.rs`: the opener names what it wants, the other end
      answers a `Header` and the body, or `error`); `instances.fetch
      {instance, haves}` on the worker (the instance's worktree
      snapshotted onto `refs/devsandbox/wip/<instance_id>`, its branch
      and that ref bundled for the asker under its fleet root);
      `placements.json` on the placing host (written when a placed run's
      answer names its instance); the bundle landed in the source
      checkout as `refs/devsandbox/<machine>/<instance>/heads/<branch>`
      and `…/wip`; `devsandbox fetch <machine>/<instance>`
      (`commands/fetch.rs`, `main.rs`), the API call on this host's
      daemon, `npm/devsandboxes/index.d.ts`.
    - **12f Fetch-back at `rm` and `done`** (decision 77). A placed
      instance's `rm` or `done` typed on the host that placed it fetches
      back first.
    - **12g Docs close.** `docs/status.md` (steps 1-12 shipped, next
      13; the parked items below), this step's as-built note,
      `CHANGELOG.md`.

    As built: every host git child is bounded (`repo::QUICK` 1 min,
    `repo::WHOLE` 30 min) and its own process group, killed whole at
    its timeout. `Transfer` is `haves`, `send` and `pull`;
    `LinkTransfer` asks `code.haves` (peer-only: the caller's own fleet
    root's roots and each cache's tips) over `route::connect`'s `API`.
    A `bundle` header names its `repo` and `refs`; a 0-byte body means
    nothing new, and one with no `to` is cache-only (the hub mirror,
    `code::mirror`, after the worker's transfer, failures a note line).
    The source snapshots right before bundling, on a transient
    `refs/devsandbox/wip/send-<nonce>` deleted before anything is sent;
    a requested branch that isn't checked out moves without WIP, an
    unborn branch as the devsandbox root. Decision 73 was amended
    after 12d (a checkout no instance mounts is recut, a mounted one
    left). Fetch-back: the worker's `instances.fetch` reuses the last
    WIP commit when nothing changed, bundles into `<fleet root>/out/`
    (swept after an hour), served by a pull on `TRANSFER`
    (`Receivers::serves`) and removed once pulled; the asker's
    `placements.fetch` is local-only, finds the entry (a bare instance
    name when unique) and checks the local checkout's identity; machine
    names `wip` and `root` can't name hidden refs. An `rm` fetches back
    on every attempt, the `ask` round's included; a `placements.json`
    that doesn't load refuses a placed-looking `rm` (`--force` gets
    past). `place::Spec::repo` stays unfilled (nothing reads it). Not
    built: fetch-back from the dashboard.
13. **Dispatcher children on any machine** (M). Instances carry
    `origin_host` and repo identity; `ls`/the dashboard show where they
    run. The dispatcher (on the hub) keeps authorizing on its host
    ([ADR 0030](../../decisions/0030-dispatcher-control-is-authorized-on-the-host.md));
    `dispatch::Executor`'s subprocess becomes a placed, forwarded verb;
    the hub's `state.toml` records child → machine; `exec`/`run-*` ops go
    through forwarded calls; `branches` reads the hub's repo cache.
    `max-instances` stays per dispatcher, fleet-wide.
14. **Checkpoints and re-placement** (M–L). Placed instances checkpoint
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
15. **The rest of the fleet view** (M). The Inbox behind the daemon on
    every member, with a federated thread list (machines-next §2);
    nameless commands across the fleet (machines-next §3).
16. **Sync layer** (L, later). A second `Transfer` for state git doesn't
    carry: ignored outputs, volumes, caches, live editing. Then build-cache
    sharing (a registry on the hub, `--cache-from`) and services that span
    machines (P10) can be revisited.
17. **Hub handoff** (M). `machine hub <m>` with the old hub alive ships
    its state to `m` over `TRANSFER` (repo cache, saved rows, dispatcher
    and placement records, checkpoints) before the role moves; the
    pointer swap (step 11) stays for a dead hub.
18. **Fallback hub** (M, later). [A fallback hub](#swapping-the-hub):
    settle who decides and the old hub's return first.

## Decisions

Numbered as code cites them; 13–16 as step 4 makes them, 3, 9 and 17 as
step 6 does, 7, 18, 22-25 as step 7 does, 26-30 as step 8 does, 1, 2, 19
and 20 as step 9 does, 21 as step 10 does, 36 and 37 as a fix to
step 9's `machine add` does, 38 as a fix to step 9's placement does, 39-43 as steps 13, 14
and 16 do, 44-47 as step 10 does, 48-52 as step 10a does, 53-60 as
step 10b does, 61-67 as step 10c does, 68-70 as step 11 does, 71-77 as
step 12 does.

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
    credentials. Pulls a slice of step 12 forward (bundle of committed
    refs only).
13. `PTY` is a direct attach: the member's daemon calls the helper's
    `session.attach` over the bridge's RPC (`HelperClient::call_with_input`),
    no `exec -it` and no in-container client; service terminals,
    helper-less instances and instances whose helper daemon isn't root
    (`session.attach` is `needs_root`, e.g. `containerUser`) were refused
    over the link until step 5, which runs them as a non-resumable
    `exec -it` under a host pty on the member.
14. `AGENT` on the link keeps the agent relay's semantics: the linking
    host's agent, while its registry entry has `agent = true`, reaches
    every instance on the machine (not only placed ones as
    [Credentials](#credentials) first said).
15. CLI re-runs to a member are API calls within its trust (quick calls,
    snapshots, verbs), never a `devsandbox …` run there.
16. Step 4 builds the channels on `fleet::LINK` (offered workers); ssh
    machines keep their per-use argvs until step 8 (step 7 puts them on
    the fleet link, step 8 moves every use onto its channels).
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
21. Routing is the full graph form (step 10), not hub-relay only.
22. Link shape: the dialer's daemon is the answerer and binds the
    per-link sockets; the far end's `fleet-link --peer <dialer>` hands
    each stream to its daemon with `link.attach`; the peer's name is
    authenticated only by ssh login (decision 2). Who binds: decision 23.
23. Both ends of a link run both halves: OUT binds one
    `fleet-<other>.sock` (and, on the dialed end,
    `agent-link-<dialer>.sock`, since ssh-agent clients can't send a
    channel byte) and opens the channel each accept names in its first
    byte; IN hands
    the other end's streams to this daemon under this host's trust
    table. The handshake roles (the dialed end writes the sentinel, the
    dialer reads it) say nothing about who binds, so every client reaches
    every machine the same way (`fleet-<name>.sock`).
24. A peer's fleet root is `fleet/<peer member name>/` under the data
    dir, replacing `lend::fleet_dir_name(<hub target>)`; no migration.
25. Versions follow the hello's hub flag (`[serve] hub` until step 9):
    the end that isn't the hub follows a hub peer, whichever end dialed
    (an ssh machine of a hub self-updates too; a dev build only with
    `dev-builds` or `full`); two non-hubs need a protocol match.
26. `lend.toml` goes with lending (step 8); `[serve] hub` stays the hub
    flag until step 9 puts `hub` in `member.toml`.
27. A link `API` stream carries no relay sentinel: inside a mux stream
    there's no login shell to skip; `machines::link::handshake` reads a
    plain hello.
28. Inbound neighbors are machines from step 8 (rows, forwarding,
    placement), so lending can go without a laptop losing its way to
    lend; the join rule and hub membership stay step 9.
29. `MachineRow.kind` after `machines::Kind`: `ssh` (this host dials
    it) or `inbound` (it dials this host).
30. The agent keeps decision 18's direction: a laptop that joined with
    `machine add <hub>` is the dialer, so its agent reaches the hub's
    instances (while its entry has `agent` on) and the hub's never
    reaches the laptop's.
31. A `run` typed on a member is placed by the hub's decision only:
    the member asks `fleet.place` (its spec, capacity and the machines
    it reaches), the hub answers a machine with its fleet-wide view,
    and the member moves the code and forwards over its own links. Until
    step 10 the candidates are the machines the member links to
    directly. With the hub unreachable the member runs here, with a
    note.
32. `machine rm <m>` runs on the hub (a member's is sent there as
    `fleet.rm`): the hub sends `fleet.leave` to `m` (it clears `hub` and
    drops its own entries) and `fleet.forget {m}` to every member it
    links (each drops its entry for `m`, refuses `m`'s inbound link). A
    member admits `fleet.leave` / `fleet.forget` only from the peer its
    `member.toml` names as hub; members the hub has no link to learn it
    in step 10.
33. Machines a member sees only through the hub's federated view are
    view-only until step 10: their rows and state show, actions,
    terminals and forwards on them are refused naming the hub. (Spent:
    10b.6 makes them routed machines, decision 60.)
34. A link's hello names its end's hub (not only a hub flag); a link
    whose two ends name different hubs is refused.
35. A machine's registry name defaults to its member name (`member.get`
    over the add-time probe), so the name `machine add` checks against
    the fleet is the one the fleet knows it by.
36. `machine add --hub` makes the target the hub: `machines.add` takes a
    mandatory `hub: boolean`. With neither end in a fleet, `true` makes a
    fleet of two with the target as hub (the target's `member.set {hub:
    "self"}`, this host's `hub` the target's name), `false` with this
    host as hub, as before. With either end in a fleet the join table
    decides as before ([Joining and leaving](#joining-and-leaving)); `true`
    there is refused unless the target already is the hub (moving the
    role is step 11's `machine hub`). So a laptop holding the configs
    makes an always-on server the hub with no ssh into the laptop.
37. A member may have no picked roots (amends decision 17): `machines.add`
    takes `roots: []`. When the probe finds no candidate roots, `machine
    add` takes none without asking; with candidates, the picker accepts an
    empty pick, and off a TTY one candidate is taken, several are an
    error as before. No roots picked, nothing is recorded on the machine
    (no `machine roots add` ssh). A hub or worker that only runs other
    members' configs (in their fleet roots) is added that way.
38. A run typed by the user (CLI, dashboard) runs on the member whose
    config it names, never placed (amends decision 20, narrows decision
    31): the user's own sandboxes stay on their machine and nothing waits
    on the hub. Placement is for dispatcher children (step 13) and an
    explicit `run --place`, which asks the hub as decision 31 says.
    `--machine` and qualified names still pick the machine. `run --here`
    is dropped: with nothing placed by default there is nothing to opt
    out of.
39. A dispatcher runs on any member and has no notion of where its
    config lives: it reads its home member's config from the mesh. While
    the home member is up, an edit there reaches it at once; while it is
    down, the last copy the mesh holds is the current config
    ([ADR 0030](../../decisions/0030-dispatcher-control-is-authorized-on-the-host.md)'s
    check runs against it). When the member running a dispatcher goes
    down, a per-dispatcher setting says what the hub does: move it to
    another member, or notify that it is down.
40. What ties a sandbox to a host is known before anything moves, two
    ways, both before step 13:
    - **Inferred** from the merged config: bind sources outside the
      config root, `${localEnv:…}`, `shared-volumes/` paths, runtime
      sockets, X11, `folder`/`folders` that can't move (`code::offer`'s
      reasons). Placement skips a host that can't satisfy them and says
      why; `code::offer` refuses up front what the worker policy would
      only refuse on the worker after the code moved.
    - **Pinned** in the config (`placement = "home"`, or the members it
      may run on): explicit and self-documenting, even where inference
      would let it move.

    As built (`code::ties`): any tie keeps the sandbox home,
    whatever a worker's trust or allowances (per-host satisfaction waits
    on decision 41), and a tied `run --place` runs here with a note
    before the hub is asked. The pin is `placement = "home"` only (no
    member lists), reported as the first tie.
41. The target model is named host resources: a config names what it
    needs (a mount of resource `gh-auth`, say), each member maps names to
    its own paths, and a sandbox is placeable on members that provide
    all of its names. Secrets never move: each host supplies its own (a
    worker's own scoped token, never the home member's). The worker
    policy's `binds` / `local-env` allowances become which resources
    another member's configs may use there. Decision 40 comes first;
    this waits on the open question below.
42. A dispatcher's state (leader, log, its records) is mesh state, not a
    bind on the host it runs on, so it moves with the dispatcher
    (decision 39). A dispatcher is its own sandbox config, not part of a
    development sandbox whose binds tie it to one host.
43. `shared-volumes` that aren't secrets (skills, agent state) are mesh
    state, carried by step 16's sync layer; until then each host has its
    own.
44. Member identity (step 10a) comes before routing: ssh vouches only
    for the end of a direct link, and a routed stream reaches its
    destination through other members, so without keys a relay could
    claim to carry another member's stream and get that member's trust.
    Steps 11-13 lean on the same proof (the hub's calls, signed config
    snapshots), so they build on keys rather than names.
45. The fleet's authority is one member, H (the user's trusted machine),
    for now: its key alone signs membership, key changes, revocations and
    hub changes, step 11's included (a hub can't hand its role over by
    itself). The hub, even always-on, is availability, not authority.
    Adding or revoking a member waits until H is reachable, except
    through an introducer (decision 46).
46. A member that a non-authority member adds (B joining through
    `machine add A`) is signed by its introducer, A, and every host
    trusts it no more than it trusts A, until the user confirms it on
    that host (its fingerprint). A's own trust table is A's: A may use B
    as A trusts it; B can't gain more on H than A has.
47. End-to-end sessions between members are their own step (10c), after
    routing (10b): proving who opened a stream doesn't stop a relay from
    altering it afterward.
48. The authority (decision 45) is the host that ran the fleet-creating
    `machine add`; with `--hub` (decision 36) that's the laptop, the
    server it adds the hub.
49. A membership change typed on another member is forwarded to the
    authority (`fleet.propose`) and waits there for the user's approval
    (`devsandbox machine pending|approve|deny` on the authority, with a
    desktop notification); the authority's key never signs on a remote
    request alone. Until routing (10b) a proposal reaches the authority
    only over a direct link to it, else it's refused naming it.

    As built (10b.7): a proposal goes over any path (`leave::call`: the
    direct link while up, else a route), failing only when none reaches
    the authority, naming it and why.
50. The membership log has one writer, the authority. An introduction
    (decision 46) is a statement signed by its introducer, outside the
    chain, held by every member as capped until the authority adds that
    member to the log.
51. While trust is one setting per host, decision 46's cap is recorded
    and shown, and a capped member can't introduce others; it narrows
    trust once per-peer grants exist (not decided).
52. The member key lives behind a `KeyStore` seam, a 0600 file for now;
    OS keychain backends are parked. Fleets made before step 10a have no
    membership log: their links are refused until the fleet is made
    again with `machine add` (no migration, pre-1.0).
53. A routed stream is a link stream on its own channel, `ROUTE`, that
    starts with a header naming its destination, origin, inner channel,
    path and TTL; `Frame::Open`, which the in-container helper shares,
    stays one channel byte.
54. A routed stream's origin proves itself per stream, challenge and
    response both ways: the destination sends a nonce, the origin signs
    the fleet's genesis, origin, destination, channel, path and both
    nonces with its member key (its own domain, apart from the link
    proof's), then the destination proves itself back. A fresh nonce
    stops replays and a relay can't pose as the destination; 10c's
    handshake takes this place.
55. Source routing: the requester puts the path in the header as a list
    of member names; a relay checks only that its next hop is a direct
    link that's up. TTL 4; a path with a loop is refused. The path stays
    a list so a later "an empty path lets the relay decide" mode is a
    small change.
56. Relaying isn't access: a relay carries a stream from any proven
    link peer to any member in its log or held introductions, whatever
    its own trust setting, and refuses revoked keys and names it doesn't
    know.
57. A destination treats a routed origin as a direct peer: the key it
    proved, its fleet root `fleet/<origin>/`, its introduction's cap
    (decision 46). No agent comes with it.
58. Routed channels are `API`, `PTY`, `TUNNEL` and `TRANSFER`; `AGENT`
    stays direct-only until 10c (decisions 18, 30). In 10c `AGENT` is
    routed behind a per-member agent grant: consent per destination
    member, keyed by that member's key (10a), off by default. My agent
    reaches instances on B only if I turned agent access on for B; it
    never comes along from a relay's (K's) own `agent` flag.
59. The graph is the members' own signed link reports (each link's
    peer, who dials, its state and since when, plus the member's trust
    and whether it's always on), one `identity::Statement` per member
    whose version only goes up. The hub collects and pushes them; every
    member keeps them and serves them to any log member, and a report
    verifies by its signer's key whoever carried it. As built (10b.7):
    every member floods what made its copy newer to its direct links,
    not only the hub, so a member with no link to the hub stays current.
60. Machines reached only over a route are rows of their own
    (`machine ls`, `machines.list`), their instances still from the
    hub's federated view.
61. A routed stream's session is SIGMA-style over the opening of
    decision 54, with no extra round trip: the destination's challenge
    carries a fresh ephemeral x25519 key and the origin's proof its own,
    in place of the nonces; both ends sign the transcript holding both
    with their ed25519 member keys (no static DH key, no key
    conversion), in the domain `devsandbox/route/v2`. A DH output that
    isn't contributory (a low-order key) is refused. Crates:
    `x25519-dalek` 3, `chacha20poly1305` 0.11, `hkdf` 0.13, all on
    `curve25519-dalek` 5 and `digest` 0.11 like `ed25519-dalek` 3; no
    Noise crate.
62. Session keys: HKDF-SHA256 with the transcript's SHA-256 as salt
    and the DH output as input, expanded once per direction with its
    role in the info, so a record can't be reflected to its sender.
63. A session's bytes are records: a `u16` length and a
    ChaCha20-Poly1305 ciphertext of at most 16 KiB of plaintext, its
    nonce a per-direction counter, its type (data or end) in the
    associated data. Each direction ends with an authenticated end
    record; a bare EOF, a bad tag or a record out of order is an error
    that tears both directions down, never a half-close. Each read is
    sealed and sent at once (`PTY` and `TUNNEL` are interactive).
64. A session's ends are the two daemons: the origin's
    `serve_routed` and the destination's `serve_route`, a socket pair
    between the session and the inner channel's handler, so handlers
    and relays don't change. Direct link streams stay as they are, on
    ssh.
65. An agent grant is this host's consent, per member, kept in
    `member.toml` (written by the daemon only, decision 3) as the
    member's name and key. It replaces `machines.toml`'s `agent` flag,
    so a direct link's `AGENT` and a routed one follow one rule: the
    stream's proven key is granted. A grant follows an authority-signed
    rotation of that key and ends with its revocation. Off by default;
    never inherited from a relay's grant.
66. A member's signed link report (decision 59) lists the members it
    grants agent to. A host probes, over a routed `AGENT` stream, only
    the members whose report lists it, and binds
    `agent-route-<member>.sock` while one answers serving; the granting
    member checks its grant on every stream regardless.
67. `machines.set {agent}` grants or withdraws agent for any machine:
    registered, inbound or routed. Decisions 18 and 30 hold as "never
    implicitly": an agent goes where the user granted it, whichever end
    dialed.
68. `machine hub <m>` is a `hub` entry in the membership log, signed by
    the authority only (decision 45): typed there it's signed at once
    (the user typing it is the approval), typed on another member it's a
    `fleet.propose {kind: hub}` approved there with `machine approve`.
    Its target is a member of the log (an introduced member can't be the
    hub); it needn't be reachable, so a dead hub can be replaced while the
    authority is up (not when the dead hub is the authority, the default
    for a fleet made without `--hub`). Naming the current hub is refused
    as `invalid` (nothing to sign).
69. A host follows a hub change (its `hub` rewritten, every link cycled
    so both ends redial saying it) only once the change has left it: the
    authority after its push, a host a `fleet.sync` made newer after
    sending the log on and writing its answer. Cycling first would cut
    the links carrying the change; a host it misses still learns it at
    its next link's proof (`proof::hub_moved`).
70. The swap moves the role only, not links nor what the hub's view
    reaches: the new hub's federated view is the machines it links to
    (one hop, as before), and members watch it federated only over a
    direct link. A hub off the routing centre (A in `A → K ← B`) shows
    fewer rows than K would until watches go over routes, which is
    parked, not step 11.
71. A repo's identity is the first 16 hex of the sha256 over the sorted
    root commit ids (hex, one per line) of the moved branch's history.
    A repo with no commit yet first gets `refs/devsandbox/root`, one
    empty-tree commit devsandbox makes once (with its own timestamp, so
    two empty repos don't share an id), and its snapshots start there.
    The identity names caches; it isn't a trust boundary (a sender can
    name any id, and a receiver's caches are per sender, under its fleet
    root).
72. v1 sends from the checkout of the host the run was placed from (the
    one whose config it is), never from an `origin`: decision 12's origin
    fetch on the placing host goes, and no receiver fetches an origin (a
    placed member never needs credentials, as decision 12 said). A repeat
    send carries only what the receiver lacks, by its haves (its cache's
    tips, the roots it has). A first send of a big repo goes whole over
    the link; fetching it from a reachable `origin` first is parked.
73. A placement's snapshot is the folder's working tree as one commit
    over its branch tip (tracked changes plus untracked, non-ignored
    files), made through a temp index on a transient ref in the source
    repo that is removed once bundled. The receiver's cache keeps it as
    `refs/devsandbox/wip/<branch>` (the latest one sent for that branch).
    It is applied only to a checkout the transfer creates; an existing
    checkout gets its refs moved as `land` does today and its working
    tree is otherwise left alone, since an instance on the worker may be
    using it. A detached `HEAD` still doesn't move. Ignored files a
    worktree wants (`.worktreeinclude`, `worktree-include`) don't travel
    in v1 (parked).

    Amended (after 12d): with every new checkout dirty from its WIP, "as
    `land` does today" failed a repeat placement whose fast-forward hit
    the WIP's files. An existing checkout that no instance in the
    worker's `state.toml` mounts is recut like a new one (the sent branch,
    `reset --hard`, untracked non-ignored files removed, the WIP
    applied); one an instance mounts keeps its working tree, its branch
    fast-forwarded only when clean, else left (the sent tip stays in the
    cache), and the transfer doesn't fail.
74. A config root that's a git repo keeps moving as a tar (its `.git`
    left out), so a `folder` inside the config root still doesn't move;
    moving such a root as a repo is parked.
75. The hub mirror: a placing host that isn't the hub, placing on a
    worker that isn't the hub, also sends the same refs into the hub's
    cache for it (`fleet/<source>/repos/<id>.git` there, no checkout),
    after the worker's transfer. Best effort: a failure is a note line in
    the run's output and the log, and the run goes on. Nothing prunes it
    yet (the retention question stays open).
76. Fetch-back is a pull by the host that placed the instance:
    `devsandbox fetch <machine>/<instance>` asks the worker
    (`instances.fetch`, admitted for an instance in the caller's fleet
    root under `worker`, any it reaches under `full`), which snapshots the
    instance's `folder` working tree onto `refs/devsandbox/wip/<instance_id>`
    in its repo and bundles that ref and the instance's branch `--not` the
    asker's haves into a file under the asker's fleet root; the asker
    pulls it on a `TRANSFER` stream it opened (the worker serves only
    files it made for that peer) and lands it in the local checkout the
    placement came from as `refs/devsandbox/<machine>/<instance>/heads/<branch>`
    and `refs/devsandbox/<machine>/<instance>/wip`: never the user's
    branches, index or working tree. That checkout comes from the placing
    host's own record (`placements.json` next to `state.toml`, written by
    the daemon when a placed run's answer names the instance), never from
    what the worker says. Only the `folder` repo comes back; `folders`
    entries' work is parked.
77. A placed instance's `rm` typed on the host that placed it fetches
    back first: a fetch-back that fails refuses the `rm`, naming why, and
    `rm --force` (which already discards uncommitted work) skips it. A
    `done` there fetches back best effort (a warning on failure).

## Open questions

- Withholding (parked): a hub that hides a member's newer signed
  snapshot can't be told from "there is none" while that member is
  offline, and decision 39 runs the last copy meanwhile. Pushing heads
  directly to every linked member, or a snapshot lifetime, are the
  candidates if it matters later.
- Fencing in step 14: is "the member stops the stale copy when it
  reconnects" enough, or must a placed instance hold a lease it renews
  with the hub and stop itself when the lease lapses (laptop offline but
  awake, e.g. off the tailnet)? Either way it ends that copy's sessions,
  which a user may still mean to reattach to (step 14).
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
- A dispatcher's down-host setting (decision 39): in its sandbox config
  or the hub's `member.toml`, and after how long a member counts as
  down? And the worker policy denies dispatcher sockets (decision 9): a
  dispatcher moved onto another member runs under that member's trust
  for its home member, so does each member that may run it need the
  `dispatcher` allowance (or `full`) for that home member, set ahead?
- **Blocks decision 41.** Named host resources need: an opt-in way to
  move a config to resource names (a config keeps its host paths until
  its user moves it), and a way for the user to see and set a member's
  resources, since `member.toml` is written by the daemon only
  (decision 3) and not something a user opens.
