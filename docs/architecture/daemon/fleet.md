# Fleet

Machines as one pool of capacity (the plan:
[fleet](../../plans/active/fleet.md)). What exists so far is the link
primitive the fleet's links are built on (the machines'
[agent relay](machines.md#agent) runs on it too) and both sides of a
worker's link: on the hub the offer, the offered registry entry and its
dial, and [hub mode](#hub-mode); on the worker `lend.toml`,
`member.toml`, the lending thread, the lent API, the worker policy,
`lend.*` and `member.*` with their CLI, `devsandbox lend`,
`devsandbox fleet join|leave` and `devsandbox machine trust`
([ADR 0041](../../decisions/0041-fleet-workers-dial-their-hub.md)); and
every daemon's [capacity report](#capacity), carried by its snapshots,
and [placement](#placement), which scores it and which the hub's `run`
verb follows;
and the link's [`TRANSFER`](#transfer) streams, both ways, with
[moving code](#moving-code) on them (a config root and its repos onto a
worker, for a placed `run`); and
[versions](#versions): a worker follows its hub's build, a published one
downloaded, a dev one received over `TRANSFER`
([ADR 0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md)).

Code: `src/serve/fleet/mod.rs` (`LINK`), `src/serve/fleet/link.rs`
(`Spec`, `cap_of`, `Handler`, `Serves`, `Endpoint`, `opener`,
`opener_with`, `answerer`, `answerer_with`, `run`, `Out`, `Outlet`,
`End`, `splice`, `write_sentinel`, `match_sentinel`),
`src/serve/fleet/transport.rs` (`Transport`, `check_target`),
`src/serve/fleet/transfer.rs` (`Header`, `Received`,
`send`, `receive`, `Receivers`, `parse_header`, `check_header`,
`receive_then`, `check_size`, `encode_reply`, `parse_reply`, `digest`),
`src/serve/fleet/code/mod.rs` (`Transfer`, `LinkTransfer`, `Haves`,
`offer`, `Offer`, `send`, `prepare`, `Prepared`, `Unprepared`, `layout`,
`pack_root`, `receivers`, `repos_dir`), `src/serve/fleet/code/tar.rs`
(`Writer`, `read`, `extract`, `check_path`, `check_link`),
`src/serve/fleet/code/repo.rs` (`probe`, `cache_dir`, `fetch`,
`has_branch`, `bundle`, `land`, `check_branch`),
`src/serve/fleet/offer.rs` (`call`, `Ctx`, `accept`,
`dial`), `src/commands/fleet_link.rs` (`fleet_link`, `run`),
`src/serve/endpoint.rs` (`fleet_socket`, `fleet_transfer_socket`,
`fleet_pty_socket`, `fleet_tunnel_socket`, `fleet_channel_socket`,
`accept_any`),
`src/serve/fleet/pty_client.rs` (`Request`, `header`, `End`, `exit_of`,
`Terminal`, `attach`, `dial`), `src/serve/machines/cli.rs` (`Dials`,
`dials`, `run_dials`, `offered_session_exec`), `src/daemon_config.rs`
(`ServeConfig::hub`), `src/serve/machines/service.rs` (`Hub`,
`save_loop`, `log_failure`), `src/serve/machines/saved.rs` (`Saved`,
`of`, `machines`, `load`, `save`, `save_wait`), `src/serve/snapshots.rs`
(`Machine::set_link`, `Remotes::restore`), `src/devsbd/proto.rs` (`channel::API`, `channel::PTY`,
`channel::TUNNEL`, `channel::AGENT` and their `caps`, `channel::flows`); the worker side: `src/serve/fleet/lend.rs` (`Lend`,
`load`, `check`, `update`, `fleet_root`, `fleet_dir_name`),
`src/serve/fleet/member.rs` (`Member`, `SoftCaps`, `load`, `check`,
`update`, `default_name`, `merge`, `call`, `MemberInfo`),
`src/serve/fleet/worker.rs` (`Lending`, `Status`, `link_args`),
`src/serve/fleet/lender.rs` (`Lender`, `Lender::reapply`, `call`, `merge`,
`transition`, `LendInfo`), `src/serve/locked_file.rs` (`update`),
`src/commands/lend.rs` (`show`, `set_state`, `join`, `leave`, `busy`),
`src/commands/machine.rs` (`trust`),
`src/serve/fleet/lent.rs` (`Peer`, `Files`, `Reach`, `own_roots`,
`decide`, `admit`, `confine_path`, `lent_instances`, `filter_answer`,
`filter_snapshot`), `src/serve/daemon.rs`
(`start_lending`, `Table`, `Shared::reach_files`, `Shared::open_lent`,
`Shared::open_lent_pty`, `Shared::open_lent_tunnel`),
`src/serve/fleet/worker.rs` (`Open`, `Opens`), `src/serve/fleet/agent.rs`
(`SERVING`, `NOT_SERVING`, `Status`, `status_of`, `Action`, `decide`,
`socket`, `serve`, `PROBE`, `Cached`, `Gate`, `answer`, `refresh`,
`REFRESH`, `MAX_AGE`), `src/serve/fleet/offer.rs` (`fleet.agent`),
`src/commands/fleet_link.rs` (`agent_of`), `src/serve/daemon.rs`
(`Options::host_agent`),
`src/serve/fleet/link.rs` (`Socket`, `Outlet::is_up`),
`src/serve/fleet/pty.rs`
(`METHOD`, `code`, `Header`, `parse_header`, `gate`, `offered`,
`attach_params`, `serve`), `src/serve/fleet/tunnel.rs` (`read_request`,
`scoped`, `confine`, `check_service`, `serve`), `src/commands/tunnel.rs`
(`Request`, `resolve`, `Header::of`, `Via`, `remote_opener`),
`src/commands/port.rs` (`resolver_in`, `LoadState`, `uses_service`),
`src/devsbd/bridge.rs` (`Transport`, `attach`), `src/commands/exec.rs` (`ExecContext`,
`exec_context`), `src/serve/helper.rs` (`HelperClient::call_with_input`,
`Input`), `src/serve/api.rs` (`sessions_list`, `sessions_rm`,
`session_container`, `helper_rpc`, `SessionsList`), `src/worker_policy.rs`
(`Trust`, `Allow`, `Policy`, `fleet_root_of`, `load_member`, `enforce`,
`enforce_argv`, `Refusal`); capacity: `src/serve/capacity.rs`
(`Capacity`, `Caps`, `HostFacts`, `EngineCache`, `parse_meminfo`,
`parse_size`, `over_caps`, `over_note`, `summary`, `lend_label`),
`src/runtime/identity.rs` (`EngineInfo`, `docker_engine_info`,
`podman_engine_info`), `Backend::engine_info`, `src/serve/host.rs`
(`poll`, `running_instances`), `src/serve/snapshots.rs`
(`Snapshots::set_capacity`, `Machine::capacity`,
`SnapshotMachine::capacity`), `src/serve/fleet/lend.rs` (`SoftCaps`);
placement: `src/serve/fleet/place.rs` (`place`, `score`, `Spec`,
`Candidate`, `MachineRef`, `Placement`, `NoPlace`, `has_image`,
`reachable`), `src/serve/fleet/placed.rs` (`worth_placing`, `workers`,
`reachable_workers`, `CAPACITY_WAIT`, `with_fresh`, `spec`,
`candidates`, `Ask`, `decide`, `Placed`, `start_point`, `lines`,
`forwarded`, `after_forward`, `Forwarded`, `tag_answer`, `Tagged`),
`src/serve/daemon.rs` (`placed_run`, `fresh_capacities`,
`remote_verb`, `Shared::capacity`, `Options::local_capacity`,
`Options::repos`), `src/serve/capacity.rs` (`HostCapacity`,
`Capacity::with_own`), `src/serve/fleet/lent.rs` (`refuse_new_work`,
`DRAINING`), `src/serve/fleet/lender.rs` (`Lender::draining`),
`src/serve/verbs.rs` (`RunParams::here`), `src/serve/verbs_client.rs`
(`run_target`, `Streaming`), `src/serve/machines/cli.rs` (`plan`);
versions: `src/serve/fleet/update.rs` (`HELLO`, `PLATFORM`,
`platform_of`, `Build`, `decide`, `Decision`, `hub_pushes`, `push`,
`install_received`, `release_target`, `asset_url`, `curl_argv`, `update`,
`Steps`, `Release`), `src/serve/fleet/worker.rs` (`Follow`, `Incoming`),
`src/commands/fleet_link.rs` (`push_build`), `src/main.rs`
(`Command::FleetHello`), `build.rs` (`stamp_build`),
`src/serve/machines/install.rs` (`install_local`, `version_of`),
`src/serve/proto.rs` (`HelloParams::force`), `src/serve/client.rs`
(`hello_with`), `src/serve/endpoint.rs` (`peer_is_own_user`),
`src/serve/daemon.rs` (`hello`).

## Link

A link is one byte pipe between two hosts (a [transport](#transport)'s
connection: an ssh's stdin/stdout in production) carrying the helper's [frame protocol](../helper/protocol.md),
with the roles of a container's link moved onto the two hosts. A connection
to a socket on one host becomes a stream on the mux, answered on the other
host by a connection made there. A `Spec` says what a link is: its
sentinel line (`\n<sentinel> <version>\n`), the channel it stands for
(the opener requires the peer to serve it), and the `Hello` hash (the
agent relay's is informational; on the fleet link it is the build a
worker [follows](#versions)).

**Channels, from either end.** Both ends are a `link::Endpoint`: OUT,
the sockets it binds (a channel and a path each) and an `Outlet` its own
code opens streams through; IN, a table of the channels it serves
(`link::Serves`: channel → `Handler`, a callback handing back the local
end of a stream the peer opens, `None` to refuse it). Its `Hello`
advertises the table's caps (`link::cap_of`: `SSH_AGENT`, `API`,
`TRANSFER`, `PTY`, `TUNNEL`, `AGENT`). An `Open` on a channel missing
from the table is refused with `Close` and the link stays up. Either end
opens streams on a channel the other advertised, and only those. Each
socket is bound at a temp name next to its path (0600) and renamed over
the path, before the end's `Hello`, so the newest end binding a path
wins; each accepted connection (one accept loop over all its sockets,
`endpoint::accept_any`) becomes one `Open { channel }` stream on its
socket's channel, or is dropped unopened when the peer didn't advertise
that channel. The end unlinks each socket when the link ends, only while
it is still its own (same device and inode), never a newer end's. The
outlet is set to the link from the `Hello`s until it ends:
`Outlet::open(channel)` opens a stream (flowing channels only), an error
when no link is up or the peer didn't advertise the channel. Handlers run
on the mux's read thread, so they return at once: a local connect, or a
socket pair whose other end a thread of its own takes
(`transfer::Receivers::accept`).

Past the `Hello`s both roles run one body (`link::run`): the outlet set,
the keepalive (`mux::KEEPALIVE_INTERVAL` / `KEEPALIVE_TIMEOUT`), the
accept loop on a thread of its own, the peer's frames served, and the
first ending (the stream closed, a write failed, the keepalive heard
nothing, a listener failed) stopping the accepts and closing every
stream. The roles differ in the handshake, the stream id space
(`link::Out`) and how an ending ends them:

- **The opener** (`link::opener_with`, the end ssh runs: the hub's
  `fleet-link`, the agent relay's far end) binds its sockets, writes the
  sentinel, exchanges `Hello`s and requires the peer to advertise the
  spec's channel. Its `on_ready` callback runs next, with the peer's
  `Hello` (its `hash` is the far end's spec `hello`); an error there ends
  the opener with it. It opens ids below 2^31 (`Mux::attach`: a socket's
  connection attached as it is, flow-controlled on a flowing channel,
  `channel::flows`, legacy otherwise; an outlet's a socket pair). The
  peer's `Open`s carry high-bit ids, admitted through `Mux::on_host_open`
  and answered from its table. Its reader runs on a thread of its own, so
  any ending ends it: `Ok` when the reader ends or the writer fails, an
  error when the keepalive hears nothing or a listener fails.
  `link::opener` is the one-socket case: the spec's channel, nothing
  served.
- **The answerer** (`link::answerer_with`, the end that dials: the
  worker's lending thread, a machine's agent relay) skips what the login
  shell printed up to the sentinel (`ssh::skip_lines`, at most
  `SENTINEL_LIMIT` bytes; another sentinel version is a handshake failure
  saying which side to update), binds its sockets, exchanges `Hello`s,
  and answers each low-bit `Open` from its table. Its `on_ready` gets the
  opener's `Hello`; its `Err` ends the link before anything is served. It
  opens high-bit ids (`Mux::open_host`: flowing channels only, which every
  link channel is; a socket's connection spliced to the stream,
  `link::splice`). Its reader runs on the caller's thread until the
  stream ends; a failed listener ends only the accepts. It returns an
  `End`: the handshake failed (with the first line of noise, or a socket
  not bound), `on_ready` refused (`Refused`, with why), the keepalive ran
  `on_dead` (which must end the reader, e.g. kill the ssh), or the stream
  closed. `link::answerer` is the one-channel case: the spec's channel
  served by an `open` callback, no outlet.

Two specs exist:

| spec | sentinel | channel | cap | opener / answerer |
|---|---|---|---|---|
| `machines::agent::SPEC` | `devsandbox-agent 1` | `SSH_AGENT` (legacy) | `caps::SSH_AGENT` | `commands::agent_relay::run` / `machines::agent::serve` |
| `fleet::LINK` | `devsandbox-fleet 1` | `API` (flowing) | `caps::API` | `commands::fleet_link::run` (on the hub) / the worker's lending thread (`fleet::worker`) |

The agent relay uses the one-channel cases (`link::opener`,
`link::answerer`). On `fleet::LINK` the hub's opener binds
`endpoint::fleet_socket` for `API` and `endpoint::fleet_transfer_socket`
for `TRANSFER` (a further channel's socket is
`endpoint::fleet_channel_socket`, `fleet-<name>.<suffix>.sock`) and
serves `TRANSFER`; the worker's answerer serves `API`, `TRANSFER`,
`PTY` ([PTY](#pty)) and `TUNNEL` ([TUNNEL](#tunnel)), and binds no
socket; the hub's opener also serves `AGENT` ([AGENT](#agent)), which
the worker opens through its outlet. Each
connection to the API
socket is one connection to whatever daemon socket the answerer's `API`
handler connects, so a
`client::Conn` on the opener's host says its hello to, and calls, a daemon
on the answerer's host.

**The sentinel rule.** An `API` stream starts with the API relay's own
sentinel line, `\n` + `devsandbox-api 1\n` (`relay::write_sentinel`): the
answerer's `API` handler hands back a daemon connection that writes that line
before the daemon's bytes. So a stream reads exactly like an ssh machine's
relay, and `machines::link::handshake` (which skips to that line, then
says its hello) works on it unchanged.

## Transport

What carries a link's bytes is one seam, `fleet::transport::Transport`:
`Transport::dial(target, args)` reaches `devsandbox <args>` on `target`
(`[user@]host`, an ssh-config alias or `ssh://[user@]host[:port]`) and
hands back a `machines::ssh::Duplex`, one byte pipe to that command's
stdio. ssh is its only implementation (`Transport::ssh`: `ssh -T -o
BatchMode=yes [-p N] -- <host> <devsandbox <args>>` through the target's
login shell, built by `ssh::target_argv`, the builder every machine argv
uses, and spawned by `ssh::spawn`; `BatchMode`: the daemon has no TTY to
prompt on). Swapping ssh, or spreading a link over several connections
for bandwidth, changes that module and nothing that dials. Callers name
the target and the devsandbox args only (the worker's link:
`worker::link_args`); tests build a `Transport::new` whose far end runs
in-process and see both.

## Transfer

A link may also carry bulk transfers on flowing `TRANSFER` streams, in
both directions; `fleet::LINK`'s two ends do (each serving `TRANSFER`
through `link::Serves::transfers`), the agent relay's don't. Both ends
then advertise `caps::TRANSFER` in their `Hello`s, and each opens
`TRANSFER` streams only toward a peer that advertised it.

**One transfer per stream** (`serve::fleet::transfer`):

1. The sender writes a header line, JSON (`Header`, `encode_header`):
   `{"kind":"root-tar","name":"cfg.tar","size":3,"sha256":"ba78…"}`.
   `kind` (`[a-z0-9-]`, at most 32 bytes: `binary`, `root-tar`, `bundle`)
   picks the receiver; `name` is a file name (no `/`, not `.`/`..`, at most
   255 bytes); `size` the body's length; `sha256` its lowercase hex hash;
   an optional `to`, where below the receiver's tree the body belongs (a
   relative path of plain components, at most 1024 bytes; left out when
   unset), for a kind that places more than its file (`check_header`).
   At most `HEADER_MAX` (4 KiB) with its newline.
2. The receiver (`receive`) checks the header and `size` against its
   limit (`check_size`), asks its `decide` where the body goes, creates a
   temp file next to there (`.<file>.<pid>-<n>.part`, 0600, its dir
   created) and answers `ready` (`READY`); any refusal answers
   `error <why>` instead and the sender sends no body.
3. The sender writes exactly `size` raw bytes, then half-closes (`Eof`).
4. The receiver writes the body into the temp file and requires it to
   end at `size` (fewer bytes before the `Eof`, or any byte after, is an
   error) with the header's sha256; then renames it into place.
5. A kind that unpacks its body does so now, on the placed file
   (`receive_then`, `Receivers::then`): its error is the answer and the
   file is removed, so the sender hears an unpacking that failed.
6. The receiver answers one line, `ok` or `error <why>` (`encode_reply`),
   and both close. On any error the temp file is removed; a bad body is
   first read to its end and dropped (at most `size` plus `DRAIN_EXTRA`,
   64 KiB): a receiver that closes while the sender is still writing
   loses its answer on the mux, the sender seeing only a broken pipe.
   `send` returns the receiver's why as `transfer refused: <why>`.

**Receivers.** Each end hands the `TRANSFER` streams the peer opens to
its `Receivers`: one entry per kind (`Receivers::with`: a size limit,
`decide`, and a `done` run with each `Received` after its `ok`). The
link's callback answers the `Open` with one end of a socket pair
(`Receivers::accept`) and receives on a `fleet-transfer` thread of its
own; a kind with no entry is refused ("nothing here receives
\`<kind>\` transfers"). `Receivers::on_failure` adds a kind's hook for a
transfer that failed after its `decide` took it. The kinds received so
far are the worker's: `binary` (`update::BINARY_KIND`, at most
`BINARY_LIMIT`, 256 MiB: the hub's dev build, [Versions](#versions)),
which the lending thread registers itself (`Incoming::receiver`), and
`root-tar` / `bundle` ([Moving code](#moving-code)), which
`start_lending` registers (`code::receivers` over the lending's fleet
root); `commands::fleet_link::run`'s receivers are empty.

**Hub → worker.** `fleet-link` binds a second socket next to the link's,
`endpoint::fleet_transfer_socket` (`fleet-<name>.transfer.sock`, bound and
unlinked like `fleet-<name>.sock`, one accept loop over both:
`endpoint::accept_any`); each connection there is one `Open { TRANSFER }`
(an id below 2^31) to the worker, so a hub process connects and calls
`transfer::send` on it. The worker's `TRANSFER` handler hands it to its
receivers.

**Worker → hub.** The answerer opens with high-bit ids
(`Outlet::open(TRANSFER)`; `Lending::open_transfer` on the worker), and
the opener answers them from its table, its `TRANSFER` handler handing
them to its receivers. So
the two directions never share an id space.

## Moving code

A sandbox goes to a worker as its config root and its folders' repos, so
the worker runs the config as the hub has it (dockerfiles, local
features) on a checkout of the branch (fleet plan decision 12: the hub
reaches `origin` with its own credentials; a worker never needs any). It
sits behind one seam, `serve::fleet::code`'s `Transfer` (`haves`, `send`;
`offer` builds what goes, `receivers` takes it), so a sync layer can
replace the git + tar implementation; `LinkTransfer` sends each body as
one `TRANSFER` through the worker's link (a connection to
`endpoint::fleet_transfer_socket`). `prepare(transfer, repos, worker
root, config root, sandbox, branch)` is `offer` then `send`, answering
`Prepared` (the config root on the worker, `remote_root`, and each moved
folder with its branch) or `Unprepared`: `NotMovable(why)` (it runs
where it is, the why for the user) or `Failed`. The placed `run` calls
it ([In the `run` verb](#in-the-run-verb)).

**Offer** (`offer`, on the hub). The sandbox resolves from the config
root (canonical) as `run` resolves it. Its `folder` and every `folders`
entry outside the root must each be a checkout's root (`repo::probe`: a
`.git` there) with an `origin`; a `folders` entry inside the root
travels in its tar instead (its `.git` doesn't), one with a `${…}`
variable doesn't move, nor does a `folder` in the root. Each origin is
fetched (`repo::fetch`: every branch, `+refs/heads/*:refs/heads/*`,
pruned, no tags, `protocol.ext.allow=never`) into the hub's repo cache,
`<data>/devsandbox/repos/<sha256(url) prefix>.git` (bare, made by `git
init --bare`; `repo::cache_dir`, `repos_dir`). The branch is the
requested one (`run --branch`, for `folder` only) when origin has it,
else the checkout's current branch when origin has it; neither is
`NotMovable` (push it first), as is an origin the fetch can't reach.

**Layout** (`layout`, pure). Paths stay as the config says: each folder
relative to the root climbs `n` times (`..`) then goes down, and the
root's members sit under its last `max(n, 1)` path components on the hub,
so every folder lands at its own relative offset with the hub's names:
root `/w/.devsandboxes` with `folder = "../web"` is `.devsandboxes/` and
`web/` in the tree. A root too shallow, a folder holding the root, and
two folders where one holds the other are `NotMovable`.

**The root's tar** (`pack_root`, `code::tar`). Written in-process, never
by a `tar` program: ustar with pax `path` / `linkpath` records past 100
bytes, members sorted, mtime 0, owner 0, modes 0644 / 0755 by the owner's
exec bit, so the same content (and layout) is the same bytes on every
hub. Left out: `.worktrees`, `shared-volumes` and `shared-files` at the
top (`ROOT_SKIPS`), `.git` at any depth, and anything but dirs, regular
files and symlinks; ignore files aren't read. Symlinks are stored, never
followed; one whose target isn't relative with its `..`s first and no
more of them than it has parents in the root (`tar::check_link`), a
non-UTF-8 name, or a tar over `ROOT_TAR_LIMIT` (256 MiB) refuses the
root. Its name is `root-<hash>.tar`, `<hash>` the sha256's first 16 hex.

**Send** (`send`). The tar, unless `haves` lists its hash (nothing asks
yet: `Transfer::haves` is empty), then per repo `git bundle create` of
`refs/heads/<branch>` from the cache (a temp file next to it), sent as
`bundle` with `to` = `<hash>/<folder's offset>`. A refusal is the
receiver's why (`Failed`).

**Receive** (`receivers`, on the worker, into its fleet root). Bodies
wait in `roots/.in/`.

- `root-tar`: the name must be `root-<its sha256's first 16 hex>.tar`.
  `tar::read` refuses an absolute path, a `.` / `..` / empty component, a
  symlink outside `check_link`'s shape, any other member type (hard
  links, devices, global pax headers), a pax key other than `path` /
  `linkpath`, a bad checksum and more than `MEMBERS_MAX` members.
  `tar::extract` writes into a fresh temp dir, every parent a real
  directory it made (never through a symlink), never over a member,
  then renames it to `roots/<hash>/`; a hash already there is kept.
- `bundle`: `to` must be `<hash>/<path>` with `roots/<hash>/` there (the
  tar goes first) and no symlink on the way. `repo::land` makes the
  checkout if missing (`git init`, HEAD to the bundle's one
  `refs/heads/<branch>`, fetch, `reset --hard`) with no remote; an
  existing checkout gets the branch fast-forwarded when it's the one
  checked out, else moved to the bundle's. A dir that exists, isn't
  empty and has no `.git` is refused.

All of it is host git through `run::git`'s family
([ADR 0003](../../decisions/0003-host-git-never-runs-repo-controlled-code.md)):
`host_git` on checkouts, `host_git_bare` on the cache, `host_git_new`
for `git init`; hooks and fsmonitor off, repo config checked. Branch
names are `repo::check_branch`'s (plain characters, no `..`, no
component starting `.` or `-`). Under the fleet root the moved root and
checkouts fall under the [worker policy](#worker-policy) like any other.

## Hub side

A worker dials its hub: its daemon holds `ssh -T -o BatchMode=yes <hub>
devsandbox fleet-link --worker <name> --root <fleet root>` ([Worker side](#worker-side)). The
hub never dials the worker; everything it does there goes over that
link's `API` streams.

- **`devsandbox fleet-link --worker <name> --root <path>`** (hidden,
  `commands::fleet_link`) is the opener of `fleet::LINK` on the hub. The
  name must pass `machines::check_name`, the root (the worker's canonical
  fleet root, a path on the worker) must be absolute. It binds
  `endpoint::fleet_socket`: `<socket dir>/fleet-<name>.sock` (0600 in the
  0700 socket dir, the name cut as an agent relay socket's client is), so
  the newest link for a worker wins. Its `on_ready` connects to this host's
  daemon (`client::connect`, which starts one like any client does when
  none runs) and sends `fleet.offer {worker, version, root, draining}`,
  `version` the worker's `Hello` hash (`<semver> <git> <published|dev> <os>/<arch>`),
  `root` its `--root`, `draining` its
  `--drain` (the worker's `lend` was `drain` when it dialed). A refused offer ends the link with `offer refused:
  <why>` (on its stderr, which the worker's ssh keeps). It holds that
  daemon connection for the link's life, so a linked worker is a `clients`
  holder of the hub's daemon. `serve::machines::cli::machine_use` refuses
  `--machine` for it. Next to its socket it binds
  `endpoint::fleet_transfer_socket` (`fleet-<name>.transfer.sock`,
  `TRANSFER` streams), `endpoint::fleet_pty_socket`
  (`fleet-<name>.pty.sock`, `PTY` streams: [PTY](#pty)) and
  `endpoint::fleet_tunnel_socket` (`fleet-<name>.tunnel.sock`, `TUNNEL`
  streams: [TUNNEL](#tunnel)), unlinked with it. It serves the `AGENT`
  streams the worker opens ([AGENT](#agent), *The hub's end*).
- **`fleet.offer`** (`fleet::offer::call`, internal: not in the npm
  typings) is accepted only by a hub: `[serve] hub = true` in
  `daemon.config.toml` (`ServeConfig::hub`, read once at start into
  `daemon::Options::hub`); anything else is `denied`. `accept` (pure)
  adds an offered entry for a new name with the offer's `root` as its one
  root, keeps an existing offered one (its root replaced when the offer
  names another: `Offered::Updated`), and refuses (`denied`) a name an ssh
  machine has. A new or changed entry reloads the machine links
  (`service::Handle::reload`), which start (or restart) its link thread;
  the offer is logged with the worker's version, root and whether it's
  draining (only logged: placement reads `drain` from the worker's
  capacity report).
- **An offered entry** is `[machine.<name>]` with `offered = true`, no
  `target`, and `roots` the worker's fleet root in `machines.toml`
  (`machines::Kind::Offered`,
  [Registry](machines.md#registry-machinestoml)). Its link watches that
  root while a federated watch wants it, as an ssh machine's watches its
  roots (`service.rs` `LinkLoop::watch`), and its rows merge into
  federated snapshots the same way (`snapshots::merge`); the
  [trust table](#the-trust-table) confines the watch to the fleet root
  (or an own root of the worker's), and its pushes carry the instances of
  every root the worker's trust reaches: under `worker`, its own roots'
  rows as well as the placed ones, which `snapshots::merge` takes from
  the fleet root's snapshot as it takes an ssh machine's unregistered
  roots' rows from its first root's.
- **The dial** (`fleet::offer::dial`, wrapped around whatever reaches ssh
  machines when `daemon::run` builds its `Dial`): for an offered entry, a
  connection to its `fleet-<name>.sock` in the daemon's socket dir as a
  plain `ssh::Duplex::socket`; down, `its fleet link isn't up (…)`. Each
  connection is one `API` stream to the worker's daemon, so the held link
  (`service::Machines`), the quick calls and `forward::verb` (one stream
  per verb, a hang-up the stream's close) run on it as on an ssh machine.
- **Terminals** go over the link: a tty `exec` on an offered machine
  (and its `--attach`, `--sessions`) and the dashboard's instance tabs,
  which run that `exec` locally, are `PTY` streams from the hub
  ([PTY](#pty), *The hub's end*); so is an `exec` without a tty, as a
  piped `PTY` stream ([PTY](#pty), *Pipe mode*).
- **Port forwards** go over the link: the daemon's forwards into an
  offered machine (`forwards.add` with `machine`) and a `devsandbox port`
  on one are `TUNNEL` streams from the hub ([TUNNEL](#tunnel), *The hub's
  end*).
- **The agent** goes over the link: with `machine set <name> agent on`
  (`machines.set`, taken for an offered entry as for an ssh one) the
  hub's agent reaches every instance on the worker as `AGENT` streams
  ([AGENT](#agent)). The machine links run no agent relay for an offered
  entry (`service.rs` `spawn_link` starts no agent thread for it, and
  `Want::agent_ready` is `false` at once), and `forward::verb` waits for
  none.
- **The CLI's other re-runs** are API calls with `machine` (fleet plan
  decision 15): `logs` (`containers.logs`), `logs --build`
  (`instances.buildLog`; `--follow`, its pages by `offset`), `inspect` (`containers.inspect`), `done` /
  `undone` (`instances.done`), `rename` (`instances.rename`), `--machine
  m ls` / `status --json` (`instances.list` per config root under the
  fleet root; `service ls` too, its service rows), `--machine m ps` (`containers.list`) and `stats`
  (`containers.stats`), printed as the local commands print them
  ([machines](machines.md#the-cli-clirs), `machines::cli::offered`).
  `--machine m gc [--force]` is the `gc` verb per config root
  (`verbs_client`'s `CliVerb::Gc`), which the worker runs under `full`
  trust only: under `worker` its refusal is the error. The
  dashboard's logs and inspect views go through the same quick calls.
- **What has no link form yet** is refused, each in its own words:
  `vscode` (`cli::offered::refusal`), a session `exec` without a tty
  (`--attach` off a terminal, `--session` without `-t`: a session over
  the link needs one), and the dashboard's service tabs
  (`tui::remote::service_argv`: service terminals aren't supported over
  the fleet link). No ssh argv is built for an offered entry:
  `machines::ssh`'s `argv_with` takes `Entry::ssh_target`, an error for one
  (`machines::offered_refusal`, a backstop no path reaches first).

### Hub mode

`[serve] hub = true` in `daemon.config.toml` (`ServeConfig::hub`, read
once at start into `daemon::Options::hub`; `serve install` on the server
keeps it running) makes the daemon a hub:

- **It never idles out**: `hub` implies `keep-alive` (`daemon::run`
  passes `keep_alive` to `idle::decide` for a hub).
- **It accepts offers** (`fleet.offer`, above); any other daemon refuses
  them.
- **It holds every offered link** (fleet plan decision 7): the daemon
  passes `service::Hub` to `Machines::spawn`, and `Inner::wanted` is true
  for an offered entry whatever holds, watchers or linger say. Its link
  thread dials at once and, while the worker's `fleet-link` socket is
  absent, retries with the usual backoff (1 s doubling to 30 s), its
  `state` `unreachable`. An offered link's failure is logged only when it
  differs from the last one logged since the link was up
  (`service::log_failure`), so an absent worker is one line in
  `serve.log`, not one per retry. Ssh machines are held only while
  wanted, as anywhere.
- **It saves the machines' rows**, `remotes.json` next to `state.toml`
  (`machines::saved::path`; `Options::remotes` in tests), so a restarted
  hub shows the fleet before its machines reconnect. The
  `serve-remotes` thread (`service::save_loop`) saves when the rows
  change (a pushed snapshot, a hello's version, a registry reload), at
  most every `saved::SAVE_EVERY` (5 s, `saved::save_wait`), and once more
  after the links stop; through `locked_file::update` (under
  `remotes.lock`, temp file + rename, unchanged content not rewritten).
  The file:

  ```json
  {
    "version": 1,
    "machines": {
      "lap": {
        "at": 1761566400,
        "version": "0.6.0",
        "rows": { "/remote/root": { "dir": "/remote/root", "instances": [] } }
      }
    }
  }
  ```

  `at` is when the rows were pushed (unix seconds), `version` the
  machine's devsandbox from its last hello, `rows` each remote root's last
  `instances.snapshot` params, as `Remotes` holds them; a machine with
  neither rows nor a version isn't saved. Kind, target, roots and the rest
  come from `machines.toml`. A file of another `version` or one that
  doesn't parse is ignored and overwritten by the next save.
- **At start** `Machines::spawn` loads the file (`saved::load`,
  `saved::machines`) into `Remotes` (`Remotes::restore`) before the first
  registry sync, which drops the machines and roots no longer registered
  (and the next save drops them from the file). Restored machines are
  `restored`: `Machine::set_link` shows them `unreachable`, `error` saying
  why with the rows' time (`not reconnected since this daemon started
  (rows from Oct 27 12:00 UTC)`, or the link's failure with the same
  suffix), and ignores `connecting` meanwhile, until their link connects.

## Capacity

Every daemon reports its host's capacity in its snapshots (fleet plan
decision 8; wire: [Capacity](../../reference/api.md#capacity)), what
[placement](#placement) scores against. The host poll
(`host.rs` `poll`) builds one `capacity::Capacity` per tick while a root
is watched (`HostCapacity::report`, over `Capacity::build`, pure) and
hands it to the snapshot cache (`Snapshots::set_capacity`), which puts
it top-level in every root's `instances.snapshot` params it collects
next; a new watch's first, disk-depth push has none. On demand,
`capacity.get` answers the same report watched or not
([api](../../reference/api.md#capacityget)): `capacity::HostCapacity`
holds the one `EngineCache` and the running count the poll sets from
every listing (watched or not), and the daemon adds `lend.toml`'s state
and `member.toml`'s caps (`Shared::capacity`, `Capacity::with_own`); tests replace the
rest with `Options::local_capacity`. It is on the trust table under either trust, so
a hub asks its workers before it places.

- **`cpus`, `mem_total`**: the runtime's `info`
  (`Backend::engine_info`: `docker info`'s `NCPU` / `MemTotal`,
  `identity::docker_engine_info`; podman's `host.cpus` / `host.memTotal`,
  `identity::podman_engine_info`; unknown on Apple `container`), the same
  output the identity probe's fast path reads. Asked at most every
  `capacity::ENGINE_TTL` (60 s, `EngineCache::get`, pure over `Instant`s;
  a failed call keeps the last answer and waits out the same interval). A
  VM-backed engine reports its VM.
- **Native Linux** (`EngineInfo::native`): the engine runs on this host's
  own kernel, by the fast path's own rules: Linux, no remote endpoint
  (`tcp://`, `ssh://`), no VM marker (Docker Desktop, OrbStack, Rancher
  Desktop, colima/lima names or contexts, a VM's socket path; for podman,
  no remote service off a local `unix://` socket). Only then does
  `HostFacts::read` add `mem_available` (`/proc/meminfo` `MemAvailable`,
  `capacity::parse_meminfo`) and `disk_free` / `disk_total` (`statvfs` of
  the runtime's data root, `DockerRootDir` / podman's `store.graphRoot`,
  when it exists here); otherwise they are unknown.
- **`running`**: the instances in `state.toml` whose container the
  poll's last listing has running (`host.rs` `running_instances`; services
  and strays don't count), every instance on the host, the fleet root's
  or not.
- **`lending` and `caps`**: `lend.toml`'s `state` (none without a
  `lend.toml`) and `member.toml`'s `[caps]` in bytes (`Caps::of`; none
  when no cap is set), read through the daemon's `Lender::own`
  (`capacity::Own`) each time, so a hand edit shows on the next report.

**Soft caps never refuse.** `capacity::over_caps` (pure) lists what
exceeds one: memory in use (`mem_total - mem_available`) over `mem`,
disk in use (`disk_total - disk_free`) over `disk`, `running` over
`instances`; `cpus` caps no use (none is reported), and an item whose use
is unknown is never over. `over_note` makes the text, `over cap: mem
18G/16G`.

**A machine's capacity** is the one its own snapshots carry: the link
stores each pushed root's params (`service.rs` `LinkLoop::store`), and
`Machine::capacity` takes the first of its roots' that has one, into
`SnapshotMachine::capacity` (federated snapshots) and `machines.list`'s
`capacity`; last known while the link is down, and restored with a hub's
`remotes.json` rows. A worker's comes through its offered root
([Hub side](#hub-side)); the trust table's snapshot filter
(`lent::filter_snapshot`) leaves it as is. The dashboard's Machines tab
and `devsandbox machine ls` show it as LEND (`capacity::lend_label`:
`on`, `drain`, `off`, `-`) and CAPACITY (`capacity::summary`: `8 cpu ·
12/32G · 120G free · 3 run`, memory in use / total, then the over-cap
note).

## Placement

Where new work goes (fleet plan decision 11: placement runs in the
daemon). `place::place` is pure over a `Spec` (the sandbox, its
`config_hash` / `build_hash`, the repo, the requester, an optional pin)
and the `Candidate`s, each a `MachineRef` (this host or a machine name)
with `reachable`, `lending`, its `Capacity` (or none yet), `has_image`
and `has_repo`. It answers a `Placement` (the machine, `warnings`, the
`score` and its `reasons` as text) or a `NoPlace`.

- **A pin wins** (`Spec::pinned`: `--machine m` or a qualified name).
  It is refused only when the machine isn't a candidate
  (`NoPlace::UnknownPin`) or is unreachable (`NoPlace::PinUnreachable`);
  over a cap, draining or not lending, it is used with a warning.
- **New work never goes** to an unreachable machine or one lending `off`
  or `drain`. None left: `NoPlace::NoCandidate`, each skipped one with
  why.
- **Soft caps never refuse.** Among the rest, the highest `place::score`
  wins; ties go to the lowest `MachineRef` (this host first, then
  machines by name). Every exceeded cap (`capacity::over_caps`)
  subtracts `OVER` (2000), more than an in-cap machine can score, so a
  machine over a cap ranks below every one that isn't; when all are
  over, the best is still used and its `warnings` carry
  `<machine>: over cap: mem 18G/16G` (`capacity::over_note`).
- **Score**: headroom, each item `0..=100` points and an unknown one
  `NEUTRAL` (50), so a machine with no report is neither best nor worst:
  memory ×3 (`mem_available / mem_total`, or the room left under the
  `mem` cap when smaller), disk ×2 (`disk_free / disk_total`, or under
  `disk`), slots ×2 (room left under `instances`; unknown without the
  cap), cpu ×1 (`running` per cpu, the `cpus` cap lowering the count;
  `0` at one instance per cpu). Then locality: `IMAGE` (300) when
  `has_image`, `REPO` (150) when `has_repo`; and `REQUESTER` (50) when
  the candidate is the requester's own host.

Candidates come from what the daemon already keeps:
`Candidate::local` (this host: always reachable and lending `on`) and
`Candidate::from_machine` over a `snapshots::Machine` (its
`Machine::capacity`; `lending` from that report; without one an ssh
machine is `off`, so one that never ran `devsandbox lend` takes only
pinned work, and an offered one `on`, since it links only while lending
(a draining one refuses the `run` itself, below);
`place::reachable`: an ssh machine unless its link state is
`unreachable`, an offered one only while `connected`).
`place::has_image` reads roots' `instances.snapshot` params: a sandbox
row of the spec's name at its `config_hash` (and `build_hash`, when the
spec has one) with a ready, non-drifted instance of that sandbox from
the same root, since image tags are per config root. No snapshot says
which repos a host has, so `has_repo` is the caller's to fill.

### In the `run` verb

The daemon's `run` verb places a request that names no `machine` and
isn't `here` (`daemon.rs` `placed_run`, the decisions pure in
`fleet::placed`). Placement runs on a hub in practice: the candidates are
this host and its offered workers (`placed::workers`; code moves only
over a worker's link, so an ssh machine is never placed on, only pinned
with `--machine` or a qualified name, which are forwarded as before), and
only a hub has offered workers.

- **Worth it?** `placed::worth_placing`: some worker reachable (an
  offered link `connected`). If none, the run is local with nothing else
  read or asked.
- **Fresh reports** (`daemon.rs` `fresh_capacities`): each reachable
  worker is asked `capacity.get`, in parallel, each on a fresh `API`
  stream over its offered dial (`machines::link::handshake`, then the
  call), each bounded by `placed::CAPACITY_WAIT` (2 s) for the dial and
  hello and again for the answer. An answer replaces the worker's saved
  report and lending state (`placed::with_fresh`, pure); a timeout or an
  error leaves the saved one (its last push, or `remotes.json`'s).
- **Candidates** (`placed::candidates`): `Candidate::local` with this
  host's capacity built now (`Shared::capacity`, as `capacity.get`
  answers it) and every watched root's snapshot, and `from_machine` for
  each worker with its fresh report. The `Spec` (`placed::spec`) takes the sandbox's
  `config_hash` / `build_hash` from the request root's snapshot when it
  is watched, and the requester is this host.
- **Decide** (`placed::decide`): this host wins (the `REQUESTER` bias
  breaks a tie) → the local verb, unchanged. A worker wins →
  `code::prepare` for it: the worker's fleet root is its registry entry's
  one root, the transfer its link's `LinkTransfer`, the repo cache
  `Options::repos` (default `code::repos_dir`), the config root the
  request's, the branch its `branch`. `NotMovable(why)` runs it here with
  `note: not placed on <worker>: <why>; running here` first in the
  output; `Failed` is the verb's `failed` error (`cannot place on
  <worker>: …`), nothing runs elsewhere.
- **Forward**: the request as a `machine` one (`placed::forwarded`:
  `machine` the worker, `root` its `Prepared::remote_root`, `here` so the
  worker never places again), through `remote_verb` as any machine verb
  ([machines](machines.md#forwarded-requests)). Its output starts with
  `placed on <worker> (<reasons>)` and a `warning: <worker>: over cap: …`
  line per warning (`placed::lines`); its `progress`, result and detached
  answer carry `machine` (`placed::Tagged`, `placed::tag_answer`), so the
  CLI prints `<worker>/<instance>`, cancels there on Ctrl-C, and the
  dashboard's status line names it so.
- **`drain` is a hard no at the worker**: on a Worker connection a `run`
  is refused `denied` with `lent::DRAINING` (`this worker is draining: no
  new work`) while the lending thread drains (`Lender::draining`, the
  live state, not the file; `lent::refuse_new_work`); `start`, `stop`,
  `rm` and `rebuild` of what's placed there go on. A fresh report already
  keeps a draining worker out of the candidates; if the hub picked it
  anyway (no report in time, a drain since), the code has moved but the
  refusal runs the run here, `note: not placed on <worker>: draining;
  running here` first (`placed::after_forward` → `Forwarded::Here`).
- **A new branch** (`placed::start_point`): a `--branch` origin doesn't
  have was bundled as the hub checkout's own branch (`code::offer`), and
  the moved checkout has no `origin` for `run` to start a branch from
  (`worktree_start_point` falls back to `HEAD`, whichever branch first
  landed there). So without the user's `--base` the worker gets
  `--base <the bundled branch>`: the new branch starts at the hub
  checkout's branch tip, where on the hub it would start at
  `origin/HEAD`.

The moving runs on the request's connection thread before the forward
(a fetch of each origin, the transfers), so a hang-up meanwhile is only
seen once the forward starts. `run --here` (`RunParams::here`) skips all
of it; the CLI then resolves the sandbox among this host's only
(`verbs_client::run_target`) and refuses a qualified one, and `--here`
with `--machine` is refused (`machines::cli::plan`). The dashboard's `run`
is the same verb request, so it is placed too.

## Worker side

A worker lends itself to one hub, through `lend.toml`; the hub reaches
it only through the link the worker holds, and there only through the
lent API ([ADR 0041](../../decisions/0041-fleet-workers-dial-their-hub.md)).
What the worker is as a fleet member (its name, trust and the rest) is
`member.toml`'s.

- **`lend.toml`** (`<data>/devsandbox/lend.toml`, next to `state.toml`,
  `fleet::lend::path`) is the worker's own; the hub never writes it. Keys:
  `state` (`on` | `off` | `drain`; absent is `off`) and `hub` (the hub's
  ssh target, as `machine add` takes one). Unknown keys are ignored.
  `lend::load` reads it (missing is not lending); `lend::check`: lending
  (`on`/`drain`) requires a `hub`, and a `hub` that's set must be valid. The
  daemon reads it at start (`daemon::start_lending`, `Lender::start`;
  `Options::lend` in tests; a bad file is logged and nothing lends) and is
  its only writer ([Lending: `lend.*`](#lending-lend)): `lend::update`, a
  read-modify-write under the sibling `lend.lock`, written to a temp file
  and renamed (`locked_file::update`, which `machines.toml`,
  `forwards.toml` and `member.toml` are written with too).
- **`member.toml`** (`<data>/devsandbox/member.toml`, next to
  `state.toml`, `fleet::member::path`; beside `Options::lend` in tests) is
  this host as a member. Keys: `name` (what this host offers itself as;
  absent is the hostname's first label, `member::default_name`, which
  must pass `machines::check_name`), `trust` (`worker` | `full`, absent is
  `worker`: [Trust](#trust)), `dev-builds` (take the hub's own binary
  when it runs a dev build, [Versions](#versions); absent is `false`, left
  out of the file while `false`), `roots` (config roots this host shows
  the fleet besides those its `state.toml` has instances in, absolute;
  `devsandbox machine roots add|rm` on this host sets them, and
  `machine add` run elsewhere records the roots it picks here that way:
  [machines.md](machines.md#devsandbox-machine-addlssetrmtrustroots)),
  an `[allow]` table, the [worker policy](#worker-policy)'s allowances,
  and a `[caps]` table, the soft caps of the
  [capacity report](#capacity) (`member::SoftCaps`: `cpus`, `mem`,
  `disk`, `instances`, each optional; `mem` and `disk` sizes like `16G`,
  checked by `capacity::parse_size`). Unknown keys are ignored.
  `member::check`: a `name` that's set is valid, `roots` are absolute,
  `[caps]` and `[allow]` well-formed. The daemon is its only writer
  ([Member: `member.*`](#member-member)), with `member::update` (the
  `lend.toml` discipline, under `member.lock`); `dev-builds`, `[allow]`
  and `[caps]` are edited by hand (no call changes them) and read at each
  use.
- **The fleet root** for a hub is `<data>/devsandbox/fleet/<hub>/`
  (`lend::fleet_root`), `<hub>` the target made one plain path component
  (`lend::fleet_dir_name`: the host, `user@host`, plus `_<port>` for a URL's
  port; bytes other than ASCII alphanumerics, `.`, `-`, `_` and `@` become
  `_`, and so does a leading `.`). The daemon creates it when lending starts
  and holds its canonical path. All the hub reaches is under it.
- **The lending thread** (`fleet::worker::Lending`, `serve-lend`) runs
  while the file says `on` or `drain`. It dials the hub through the
  [transport](#transport) with `link_args`' `fleet-link --worker <name>
  --root <fleet root> [--drain]` (the canonical fleet root; `--drain`
  while `drain`, read at each dial) and runs `link::answerer_with` of
  `fleet::LINK` on the connection, serving nothing
  until its hub's build checks out ([Versions](#versions)). A link that ends (the
  handshake failed, the keepalive heard nothing, ssh exited) is logged
  with ssh's stderr tail and retried after the machine links' backoff
  (`service::backoff`: 1 s doubling to 30 s), reset once a link came up.
  Its `Status` is `Connecting` until the first attempt ends, `Connected`
  once the `Hello`s are through, `Retrying(why)` after a failure (why with
  ssh's stderr tail). Dropping the thread (`lend.set` off or to another
  hub, `member.set` to another name, the daemon exiting after its drain: `Lender::stop`) shuts
  the link's socket and kills the ssh. The transport is
  `Transport::ssh` in production, `Options::lend_transport` in tests; the
  hub target is checked when lending starts (`transport::check_target`).
  A lending daemon never idles out
  (`Lender::lends`, checked each tick): it is what holds the link.
- **Each `API` stream** the hub opens is answered by `Shared::open_lent`: a
  socket pair whose daemon end writes the API relay's sentinel line first
  ([the sentinel rule](#link)) and is then served like any connection
  (`spawn_conn_as`), with the `Worker` table. Its connection id comes from
  the same counter as the listener's.
- **Each `PTY` stream** is answered by `Shared::open_lent_pty`: a socket
  pair whose daemon end `fleet::pty::serve` takes on a `serve-pty` thread
  ([PTY](#pty)); refused without a host side (no helper client) or once
  the daemon stops. The lending thread gets both handlers as its `Opens`
  (channel → `Open`), and refuses both while it waits for the hub's
  binary ([Versions](#versions)).
- **The hub's agent** is a socket the lending thread binds in the
  daemon's socket dir while a link serves, the hub advertised
  `caps::AGENT` and its probes say the hub serves it (never while it
  waits for the hub's binary): each connection there is one `AGENT`
  stream to the hub ([AGENT](#agent)).
  `Lending::spawn` takes the dir (`daemon::start_lending` passes its
  own).

### Versions

A worker follows its hub's exact build, an older one included
([ADR 0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md)):

- **The build in the `Hello`.** `fleet::LINK`'s `hello` is
  `fleet::update::HELLO`, `<semver> <git> <published|dev> <os>/<arch>`
  (`0.8.0 1a2b3c4d5e6f dev linux/x86_64`), from `build.rs`'s
  `stamp_build`: `DEVSANDBOX_BUILD_PLATFORM` is the target's
  `CARGO_CFG_TARGET_OS` / `CARGO_CFG_TARGET_ARCH` (what `std::env::consts`
  says, `update::PLATFORM`); `DEVSANDBOX_BUILD_GIT`
  is `git rev-parse --short=12 HEAD` of the source tree (`unknown` when git
  or the repo is missing; the build never fails for it), watched through
  `HEAD`, the ref it names and `packed-refs`; `DEVSANDBOX_BUILD_KIND` is
  `published` when the build ran with `DEVSANDBOX_PUBLISHED=1` (meant for
  the release workflow), else `dev`. `Build::parse` reads the first three
  words back (a missing one is `unknown` / dev), `update::platform_of` the
  fourth (`unknown` without one): the platform isn't part of the build,
  one release being built for several. `devsandbox fleet-hello` (hidden)
  prints `HELLO`.
- **The decision** (`update::decide`, pure, over this build and platform,
  the hub's from the answerer's `on_ready`, and whether this host takes
  dev builds: `member.toml`'s `dev-builds`, or trust `full`
  (`member::Member::takes_dev_builds`), read at each link's `Hello`s
  through `worker::Follow::dev_builds`): the
  same semver, git and kind is `Same`, and the link serves; another build
  that is published is `Download { version }`; another dev build is
  `Receive` when the platforms are equal and dev builds are taken, else
  `Stop(why)`. `Download` and `Stop` refuse the link (`End::Refused`)
  before the first stream, so the hub's offer registers the worker but
  its link finds it gone; `Receive` keeps the link up serving only the
  hub's binary.
- **`Stop`** stops offering, retried at the next attempt. `lend.get`'s
  error is `dev build: can't run on <our os>/<arch> (the hub runs <hub
  build> for <its os>/<arch>)`, checked first since `dev-builds` can't
  help there, else `` dev build: not taken (`dev-builds = true` in
  member.toml or `devsandbox machine trust full` takes it; the hub runs
  <hub build>) ``.
- **`Receive`**, the hub's side: `fleet-link`, once its offer is made,
  pushes its own binary when `update::hub_pushes` (over both `Hello`s:
  the worker, taking dev builds, would `Receive`), from a thread of its
  own (`commands::fleet_link::push_build`): `update::push`, one `binary`
  transfer named `devsandbox` through its own transfer socket. The
  binary is the `fleet-link` process's (`/proc/self/exe` on Linux, the
  inode it runs even if the path was replaced since; `current_exe()`
  elsewhere), not the hub daemon's: it is the one that said the link's
  `Hello`, so the build the worker decided on. The hub pushes rather
  than waiting to be asked: the worker's decision stands either way,
  since it refuses the header (no body sent) unless it waits for a
  binary, and a worker taking no dev builds has refused the link before
  the push opens; no request kind or round trip is needed. A failed push
  is on `fleet-link`'s stderr.
- **`Receive`**, the worker's side: the link's `API` streams are refused
  (the `Worker` table is out of reach until the update is done) and a
  wait is set up (`worker::Incoming`: a private scratch dir under the temp
  dir, `update::scratch_dir`); `lend.get`'s error is `receiving the hub's
  build <hub build>`. The `binary` receiver takes one transfer while a
  wait is on (else `this worker isn't waiting for a binary`) into
  `<scratch>/devsandbox` (0600, as every transfer lands), then shuts the
  link. No transfer started within `PUSH_WAIT` (60 s: `Incoming::watch`)
  or one that failed shuts it too, with why. With the link down (its ssh
  killed) `update::install_received` over the worker's `Steps`: the file
  made 0755, its `fleet-hello` (`Steps::build`) must report the hub's
  `Hello` word for word (semver, git, kind, platform), installed by
  `install::install_local` as a download is, the installed binary's
  `fleet-hello` checked again, then the forced handoff, below. The
  scratch dir is removed either way. The status follows `Download`'s:
  `updating to <hub build>`, `updated to <hub build>: handing off`, or
  `update to <hub build> failed: <why>`, retried after the backoff (a
  `Receive` link never counts as up, so the backoff isn't reset).
- **`Download`**: the ssh is killed, then `update::update` over the
  worker's `Steps` (`Release` in production; `Options::lend_update` in
  tests): the asset `devsandbox-<v>-<target>.tar.gz` of release `v<v>`
  (`asset_url`: `https://github.com/Nemikolh/devsandboxes/releases/download/…`,
  `<v>` limited to a semver's characters; `<target>` by `release_target`
  from `std::env::consts`: `x86_64-unknown-linux-musl`,
  `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`, none elsewhere) is
  fetched into a private scratch dir under the temp dir with curl
  (`curl_argv`: `--fail --location --proto =https --proto-redir =https
  --tlsv1.2`, a stall of 30 s gives up) and unpacked with `tar`. Its
  `--version` must report `<v>` (`install::version_of`); it is installed
  over `exe::devsandbox()` (refused while that path is gone) by
  `install::install_local`, `machines::install::INSTALL` run locally as
  `sh -c INSTALL sh <dest>` (temp file next to it, its own `--version`
  check, rename); the installed binary must report `<v>` too. The scratch
  dir is removed either way. Then the handoff, below.
- **Any failure** stops offering: `lend.get`'s `retrying` error says
  `update to <v> failed: <why>`, and the next link attempt, after the
  usual backoff (`service::backoff`, doubling to 30 s), decides again.
  While it runs the error is `updating to <v>`; after it, `updated to <v>:
  handing off`.
- **The forced handoff** (`Release::handoff`): a hello on the daemon's own
  socket with `force` (`client::hello_with`, `HelloParams::force`) as
  client `fleet-update`. `daemon::hello` hands off on a forced hello only
  from a `Full` connection whose peer runs as the daemon's uid
  (`endpoint::peer_is_own_user`: `SO_PEERCRED` on Linux, `getpeereid`
  elsewhere); a `Peer` connection never hands off, forced or not, and a
  refused force is logged. Then the successor is started as a newer
  client's handoff starts it (`install::lazy_start` with
  `StartReason::Handoff`: the service manager's unit, rewritten to this
  path, when installed; else detached), and waits for the lock while the
  old daemon drains ([role](role.md#version-handoff)). The new daemon
  lends as `lend.toml` says, and its hello matches the hub's.

### Lending: `lend.*`

`lend.toml` is the daemon's (the `machines.toml` rule): `lend.get` and
`lend.set` (`fleet::lender::call`, over the daemon's `Lender`) are the
only way to read its live state or change it, and every change is applied
at once, without watching the file. Never on a `Peer` connection under
`worker` (`lent::decide` denies them). Wire: [`lend.*`](../../reference/api.md#lend).

- **`lend.get`** answers `LendInfo`: the file's `state` and `hub`, the
  `name` offered (`member.toml`'s, else `member::default_name` of the
  hostname; `null` when neither is valid), the `fleet_root` (canonical while
  lending), and the link: `off`, `connecting`, `connected` or `retrying`,
  with `error` the `Retrying` reason, or why lending didn't start while
  the file says lend.
- **`lend.set {state?, hub?}`** merges what's given into the file
  (`lender::merge`, pure: `hub` non-empty and a valid target; lending
  needs a `hub`) and, when it lends, checks the name it would offer
  (`member.toml`'s), all before writing; then
  applies it under the `Lender`'s lock (`lender::transition`, pure, over
  the running thread's and the wanted `(hub, name)`): nothing running and
  lending starts one (its fleet root created); off stops it; another hub
  or name stops the old one (its ssh killed) and starts a new one; the same
  pair keeps the thread and its link and sets what the next dial offers,
  so `on` ↔ `drain` never drops what's placed here. It answers the
  `LendInfo` after the change. A daemon that's exiting answers `failed`.
- **`drain`** is recorded, shown by `lend.get`, offered (`--drain`) and
  reported as the [capacity](#capacity)'s `lending`, which the hub's
  [placement](#in-the-run-verb) reads, and a hub's `run` is refused
  while the lending thread drains (`lent::refuse_new_work`).

The CLI (`commands::lend`) is a client of these two, through the daemon
(started like any client does), never in-process; `--machine` refuses
both commands (`serve::machines::cli::machine_use`):

- `devsandbox lend` / `lend show`: `lend.get` as a one-row table (`STATE`,
  `HUB`, `NAME`, `LINK`, `FLEET ROOT`), then the error if any.
  `lend on|off|drain`: `lend.set {state}`.
- `devsandbox fleet join <hub> [--name N]`: `member.set {name}` when
  `--name` is given, then `lend.set {hub, state: on}`.
- `devsandbox fleet leave`: `lend.set {state: drain}`, then
  `instances.list {dir: <fleet root>, depth: listing}` every 2 s until
  `commands::lend::busy` (pure, over its rows) finds no instance whose
  config root is under the fleet root running or being built (a note on
  stderr names them when that set changes), then `lend.set {state: off}`.
  Ctrl-C ends the wait and leaves lending at `drain`. Not lending: it says
  so and changes nothing.

### Member: `member.*`

`member.toml` is the daemon's too (decision 3 of the fleet plan):
`member.get` and `member.set` (`fleet::member::call`, over the
`Lender`'s `member.toml` path and hostname) are the only way to change
it. Local only: never on a `Peer` connection (`lent::admit` refuses
them under either trust). Wire: [`member.*`](../../reference/api.md#member).

- **`member.get`** answers `MemberInfo`: the `name` this host goes by
  (`Member::name_or`: the file's, else the hostname's first label; `null`
  when neither is valid), `trust`, `dev_builds` as the file says, and
  `roots`.
- **`member.set {name?, trust?, roots_add?, roots_rm?}`** merges what's
  given into the file (`member::merge`, pure: `name` a
  `machines::check_name`, `""` clears it; `roots_add` absolute, one
  already there kept once; `roots_rm` drops what it names), checks it and
  writes it; a change of the resolved name re-applies lending at once
  (`Lender::reapply`), so a lending thread restarts under the new name as
  a new hub restarts it. It answers the `MemberInfo` after the change.

### Trust

`member.toml`'s `trust` is this member's own setting, changed only on it:
`devsandbox machine trust [worker|full]` (`commands::machine::trust`) is
`member.set {trust}`, or `member.get` without an argument, printing the
trust (under `full`, with a note that remote callers get this host's
whole API); `--machine` refuses it with the rest of `machine`
(`serve::machines::cli::machine_use`). It is read for every request a
remote caller sends (`lent::Reach::load`), so a change applies to the
next one. It applies to every remote caller alike: a hub over the
lending link, and a host that linked this one as an ssh machine (its
relay connections, after `link.attach`). `worker` is the default: a
remote caller reaches this host's own roots and (a hub) its fleet root
through the [trust table](#the-trust-table), placed
work under the [worker policy](#worker-policy). `full` gives it the API a
local client has (but `member.*` and `link.*`), lifts the worker policy
(`worker_policy::Policy::detect` answers none) and takes the hub's dev
builds ([Versions](#versions)).

### The trust table

Every connection has a table (`daemon::Table`): `Full` for local clients,
`Peer(Peer)` for a remote caller's: the hub's `API` streams over the
lending link (`Shared::open_lent`), and an ssh machine's relay connection
once it is attached. A `Peer` (`fleet::lent::Peer`) is who it is (for the log),
its fleet root (canonical; `Some` on the lending link, `None` for an ssh
machine's peer) and the env its verbs get (`Some` on the lending link:
this host's own; `None` keeps a verb's as sent). For each request on a `Peer` connection the daemon reads
a `lent::Reach` from this host's files (`Reach::load` over
`Shared::reach_files`: `member.toml` and `state.toml`, the home):

- **`trust`**, `member.toml`'s; no file (or no data dir) is `worker`. A
  `member.toml` that doesn't load fails closed: `worker`, no picked roots,
  the error logged.
- **Own roots** (`Reach::own`): `lent::own_roots` (every `config_dir` in
  `state.toml` plus `~/.devsandboxes`, what `roots.list` answers) and
  `member.toml`'s `roots`, canonicalized where they exist (decision 17 of
  the [plan](../../plans/active/fleet.md#decisions)); those are written
  by `devsandbox machine roots add` (`commands::machine::roots`), which
  `machine add` runs here over ssh for every root it picks
  (`commands::machine::record_roots`). Placed instances'
  config roots are in `state.toml` too.
- **The fleet root**, the `Peer`'s (none for an ssh machine's peer).

`Reach::admits` takes a canonical path that is under the fleet root or
exactly one of the own roots (a dir nested in an own root isn't one);
`Reach::admits_recorded` the same for a recorded `config_dir` (canonical
when it exists, never with a `..`). Every gate below uses it.

Each request goes through `lent::refuse_new_work` (a `run` from the
lending link's peer, the one with a fleet root, while the lending thread
drains is `denied`, `lent::DRAINING`, under either trust; `drain` is
about the lending link, so an ssh machine's peer isn't refused) and
`lent::admit` at the top of `handle`, before any dispatch. Under
either trust `member.*` and `link.*` are `denied`: this member's own
settings and links are its local clients' only.

Under **`full`** that's all: the request runs as a local client's would,
any root, any instance, no filter on its answer or its pushes, except
that a verb's `env` is still replaced when the `Peer` has one, and a hello
never hands the daemon off (below). Under **`worker`**:

- **`decide`** (pure, over method and params) lets through `hello`,
  `subscribe` / `unsubscribe` with only the `instances` topic,
  `instances.watch` / `unwatch`, the quick calls
  (`machines::forward::QUICK`), `capacity.get`, `roots.list` (with or
  without `paths`: read-only; `machines.add` checks typed roots through
  it) and the verbs (`verbs::is_verb`) but `gc`. It refuses
  (`denied`) any request with a `machine` param or `machines: true`
  (nothing federates past the member), `gc` (host maintenance: it reaps
  services and the host's agent links, not one root's instances), and
  every other method:
  `machines.*`, `fleet.*`, `lend.*`, `shutdown`, `helper.call`, `forwards.*`,
  `bridges.ensure`, the Inbox, each refusal naming `devsandbox machine
  trust full` as what opens it. Refusals are logged.
- **Paths**: a `root` or `dir` param must be absolute, have no `..`, and
  canonicalize (symlinks resolved) to a path the reach admits
  (`lent::confine_path`); it is rewritten to the canonical path. A path that
  doesn't exist is `not-found`, except an unwatch's, checked as written.
  `instances.list`, the watches, `sandboxes.*`, `services.get`, `config.*`,
  `sessions.*` and the verbs must carry theirs. `config.migrate` and
  `config.ack` edit a config, so their `root` must be under the fleet
  root: an own root's is `denied`, naming `machine trust full`.
- **Instances**: an `instance` (exact name or id, `sessions.*`' included), `instances.procs`'
  `instances` and the `containers.*` `container` must be an instance whose
  `config_dir` the reach admits (`lent::lent_instances`, from
  `state.toml`), else `not-found`; a `containers.*` `container` may also
  be a service container of those roots (its `devsandbox-svc-<project>-`
  name). A verb's `instance` is resolved as the
  CLI resolves it (`commands::instance_matches`), and every match must be
  lent; `rebuild` / `start` / `stop` / `rm` must name one, or carry
  `all`: `--all` is every instance in `state.toml`, not the root's, so
  it is admitted only while the reach admits every one of them (an
  instance without a recorded `config_dir` stops it, `denied`).
- **Env**: a verb's `env` is replaced by the `Peer`'s when it has one: on
  the lending link the daemon's own process env (`verbs_client::caller_env`,
  `Options::lend_env` in tests), never what the hub sent: the worker-side
  counterpart of `relay::SessionEnv` on an ssh machine.
- **Answers and pushes**: a snapshot lists every instance in `state.toml`,
  whatever its `dir`, so `instances.list` answers and every
  `instances.snapshot` pushed to the connection (`Pending::reach`: the
  notifier reads the reach per push, outside the pending lock) are
  cut by `lent::filter_snapshot`: instance rows whose `root` the reach
  admits, their names alone in services' `used_by` and in
  `thread_children`, their ids alone in `isolated` services' `containers`
  (whose names carry the instance id). `instances.resolve`'s matches are cut the same way
  (`lent::filter_answer`); none left is `not-found`. `containers.list` /
  `containers.stats` list the whole host's runtime, so their `rows` are
  cut (`lent::cut_containers`) to the lent instances' containers and the
  service containers of their config roots: a listing row by its
  `devsandbox.project` label being one of those roots'
  `services::project_id` (no label: dropped), a stats row, which has no
  labels, by its `devsandbox-svc-<project>-` name.

**Hello** on a `Peer` connection never hands the daemon off, whatever
version it says and even with `force`, under either trust: only a local
client's newer binary (or this user's forced hello, [Versions](#versions))
replaces it.

**`link.attach {peer}`** turns a `Full` connection into a `Peer` one: an
ssh machine's relay (`api --stdio --session-env`) sends it first, before
relaying a byte ([machines](machines.md#ssh), `relay::attach`). The
daemon (`daemon::link_attach`) takes it only from a `Full` connection of
its own user (`endpoint::peer_is_own_user`), `peer` a non-empty name
(`lent::attach_peer`: at most 255 bytes, no control characters; the
linking host's name, shown only in the log), and switches the
connection to `Peer { name: peer, fleet_root: None, env: None }`, its
pushed snapshots filtered from then on (`Pending::reach`, as
`Shared::spawn_conn_as` sets it for the lending link's). `env` is
`None`: the relay's `SessionEnv` already replaced each verb's with the
machine's login env and the linking host's agent relay, which the
daemon must keep. It is one-way: a second `link.attach` is `link.*`,
which the table refuses. It logs `connection N: link from <peer>, trust
<t>` and answers `{ok, trust}`.

### PTY

A terminal into a lent instance over the link: each `PTY` stream the hub
opens is one session attach (or a piped exec: *Pipe mode*; a tty exec
where the helper can't attach: *Tty fallback*; a service container's
terminal: *Service terminals*, all below), the worker's daemon calling the instance
helper's `session.attach` itself over its bridge
(`HelperClient::call_with_input`; decision 13 of the
[plan](../../plans/active/fleet.md#decisions)): no `exec -it`, no client
in the container. The stream carries `rpc` records
(`src/devsbd/rpc.rs`, [protocol](../helper/protocol.md)), and
`fleet::pty::serve` relays them; it parses none past the header.

- **The header** is the hub's first record (within 30 s): a `call` of
  `session.attach` (`pty::METHOD`) whose params are `{"root": <abs
  config root>, "instance": <exact name or instance id>, "session":
  <id>, "create"?: {"argv"?: [...]}}` (`pty::parse_header`). The session
  id must pass `control::valid_run_id`. No `create` (or `null`) attaches
  only; `create` creates the session unless it exists, then attaches,
  running `argv`, or the login shell (`exec::SHELL_FALLBACK_CMD`) when
  `argv` is absent or empty.
- **The gate** (`pty::gate`, pure but for canonicalizing `root`):
  `root` confined as an `API` request's is (`lent::confine_path` over
  the link's `lent::Reach`, read per stream: under the fleet root or an
  own root under `worker`, any root under `full`); the instance found by
  exact name, else by id, among the lent ones (`lent::lent_instances`)
  whose config root is under `root`. Then `pty::decide` (pure) picks the terminal from whether the
  instance has the helper (`Instance::devsbd_arch`) and whether it offers
  `session.attach` (`HelperClient::offers`): offered, the attach below; no
  bridge up, `not-running`; no helper, or a helper whose daemon isn't root
  (`session.attach` is `needs_root`: a `containerUser` instance, or an
  older helper), the *Tty fallback* for a header with `create`, and for an
  attach-only one `no-session` (nothing resumable exists), its message
  saying the instance's terminals over the link aren't resumable.
- **The attach**: `session.attach {id, create?: {argv, cwd, env, user?}}`
  (`pty::attach_params`), `create` as an exec into the instance runs
  (`exec::exec_context`, which `exec_argv_with` builds its argv from too:
  the workspace, the remote user when set, `Instance::exec_env` then
  `SSH_AUTH_SOCK`), caller `fleet hub <hub>`. It queues behind the
  container's `helper::MAX_CALLS` like any call, abandoned if the hub
  hangs up meanwhile.
- **Records**: hub → worker `stdin`, `resize`, `stdin-eof` (any other is
  ignored, as the helper ignores it), into the call's `Input` on a thread
  of their own; worker → hub the helper's records unchanged: `stdout`…,
  then exactly one end record, `exit` (`128 + n` for a signal) or
  `error {code, message}` with the helper's code (`no-session`,
  `detached`, `version`, `bad-request`, `failed`). After it the worker
  closes the stream.
- **Hang-up**: the hub closing its end cancels the call; the session
  lives on in the container (detached), for a later attach. Nothing more
  is written.
- **Refusals and losses** are one `error` record and the stream's close:
  `bad-request` (no or a malformed header, a relative `root`), `denied`
  (`root` outside the fleet root; logged), `not-found` (no such root, no
  such lent instance in it, a service it doesn't use), `not-running` (no
  live bridge to its helper), `no-session` (attach only, without a helper
  that attaches), `unsupported` (the helper stopped offering
  `session.attach` between the check and the call), `failed`
  (`state.toml` or the config doesn't read, the helper couldn't be asked,
  a terminal couldn't be spawned), and `lost`: the attach ended without an end record
  (the bridge or its call broke), so the hub can't tell how the session
  is, only that the attach is gone.

**Pipe mode** is a `PTY` stream for an `exec` without a tty: no session,
no helper, a `<runtime> exec` on the worker with its stdio piped.

- **The header** has `"pipe": {"argv": [...], "interactive"?: bool}`
  instead of `session` / `create` (`pty::Mode::Pipe`; with either of
  them, or without a command in `argv`, it is `bad-request`: the hub
  sends the login shell itself). `interactive` is `exec -i`: whether the
  hub's stdin is forwarded.
- **The gate** is `pty::gate`'s, without the helper: the root confined,
  the instance lent and under it; no `offered` (nothing is asked of a
  helper; an instance that isn't running is the runtime exec's own
  error, on `stderr`).
- **The exec** is `pty::runtime_exec` of a `pty::Target::Instance`: the
  runtime's `exec_argv_with(instance, interactive, false, argv,
  host_agent)`, the same argv a local `exec` without `-t` runs (`-i` when
  interactive), its stdin the hub's or empty, its stdout and stderr
  piped. The daemon passes `runtime_exec` to `pty::serve` as a
  `pty::ExecCommand` (a pipe's and a tty's argv alike); tests pass a
  program of their own.
- **Records** (`pty::run_pipe`): hub → worker `stdin` into the exec's
  stdin, `stdin-eof` closing it (anything else ignored); worker → hub
  its stdout as `stdout` and its stderr as `stderr` records (up to 64
  KiB each), then, once it exited and both pipes are drained, one `exit`
  (`pty::exit_record`: its code, or `Exit::Signal` for the signal that
  killed it); a spawn or wait failure is `failed`. Then the worker
  closes the stream.
- **Hang-up**: the hub closing its end before the exit kills and reaps
  the exec; nothing more is written. The hub's end closes when its own
  stdout's reader went away (`| head`): `pty_client::attach` then ends
  as `SIGPIPE` (exit 141, no note), as a local exec would.

**Tty fallback** is a session header's terminal for an instance whose
helper can't attach (`pty::Terminal::Tty`): `<runtime> exec -it` on the
worker under a host pty, not resumable.

- **The exec** is `pty::runtime_exec` of `exec_argv_with(instance, true,
  true, argv, host_agent)` (the local tty exec's argv; `argv` the
  header's `create.argv`, else the login shell), spawned on a fresh host
  pty (`pty::spawn_tty`: portable-pty, 80×24 until the first `resize`, as
  the dashboard's tabs spawn theirs). The session id is unused: nothing
  is resumable. An instance that isn't running is the runtime exec's own
  error, on the terminal.
- **Records** (`pty::run_tty`): first one `stderr` record,
  `this terminal isn't resumable (no helper | helper not root): a dropped
  link ends it` (`pty::NotResumable::note`, ending `\r\n`: the hub's
  terminal is raw by then), then the pty's output as `stdout` records;
  hub → worker `stdin` into the pty, `resize` onto it (`MasterPty::resize`;
  `stdin-eof` and anything else ignored); once the child exited and the
  pty's output is drained, one `exit` (`pty::tty_exit_record`: its code,
  or its signal, mapped back from portable-pty's `strsignal` name). Then
  the worker closes the stream.
- **Hang-up**: the hub closing its end first kills the child
  (portable-pty's `kill`: SIGHUP, then SIGKILL after a grace) and reaps
  it; nothing more is written. A lost link is that hang-up too, so the
  hub's reconnect re-dials an attach, answered `no-session`.

**Service terminals** are a `PTY` stream into the container of a service
a lent instance uses (`exec --service`, the dashboard's service tabs):
the same tty exec, never a session.

- **The header** has `"service": <name>` and a `create` (`{"argv"?}`,
  required: there are no service sessions) alongside `root` and
  `instance`, without `session` or `pipe` (`pty::Mode::Service`; else
  `bad-request`).
- **The gate** is `pty::gate`'s for the instance (it scopes the
  service), then `pty::service_container`: the config read from the
  instance's own config root, the service one its sandbox uses, and the
  container named on the worker by the service's scope
  (`port::service_container_for`, as the forwards name it:
  `isolated_service_container` by the instance's id, or the global
  `service_container`), never by the hub. Not used by it, or no such
  service: `not-found`.
- **The exec** is `pty::runtime_exec` of a `pty::Target::Service`:
  `<runtime> exec -i -t <container>` and the command, else the login
  shell (the dashboard's local service tab's argv), under a host pty;
  records and hang-up as the *Tty fallback*'s, without the note.

`sessions.list` / `sessions.rm` ([api](../../reference/api.md#sessions))
are quick calls, admitted on the `Worker` table with the instance rule
above; the session list and removal go through the helper's
`session.ls` / `session.rm`.

**The hub's end** (`fleet::pty_client`) is a CLI process on the hub, not
its daemon: a tty `exec` on the offered machine
([machines](machines.md#the-cli-clirs), `cli::Dials::Pty`), or the
dashboard tab that runs one. Each dial (`pty_client::dial`) connects to
the worker's `fleet-<name>.pty.sock` in this user's socket dir (absent:
the link is down), sends the header (`pty_client::header`: the root and
exact name resolution found there, the session id, `create` on the first
dial only), puts the terminal in raw mode (crossterm's, restored by a
guard on every return and an unwinding panic) and relays
(`pty_client::attach`, the terminal injected): the size at once and
whenever it changes, looked at every `pty_client::SIZE_POLL` (200 ms) as
the helper's own session client does rather than on `SIGWINCH`; stdin as
`stdin` records (polled, so the relay stops without a byte to read),
its EOF `stdin-eof`; `stdout` records to stdout until the end record,
then the stream is closed. The end becomes the exit code an ssh dial
would give (`pty_client::exit_of`), so the CLI's `Reconnect` and the
dashboard's `Redial` apply unchanged: `exit` its code (`128 + n` for a
signal; 255 as 254 under `--reserve-255`); `no-session` 250, `detached`
251, `version` 252 (`cli::NO_SESSION_EXIT`, `DETACHED_EXIT`,
`VERSION_EXIT`) with the helper's note after `devsbd session: `;
`lost`, a stream that ends without an end record, and an absent socket
255; the other refusals (`bad-request`, `denied`, `not-found`,
`not-running`, `unsupported`, `failed`) `pty_client::REFUSED_EXIT` (1)
with `error: <message> (<code>)`, which no loop retries.

A service dial (`pty_client::Mode::Service`, `exec --service` on the
machine: `cli::service_mode`) is a session dial's terminal (raw mode,
size) with the service header, never re-dialed: its 255 is reported
once. A fallback terminal's note comes as a `stderr` record, written to
stderr as any is.

A pipe-mode dial (`pty_client::Mode::Pipe`, a non-tty `exec`:
`cli::pipe_mode`) is the same dial without the terminal: no raw mode, no
size; stdin read and sent (its EOF `stdin-eof`) only when interactive,
else not read at all (`pty_client::Terminal`'s `input` `None`);
`stdout` records to stdout, `stderr` records to stderr; the same
`exit_of`. It never goes through the reconnect loop: its 255 (a lost
link, or `lost`) is reported once, with its line.

### TUNNEL

A port forward's bridge into a lent instance over the link: each
`TUNNEL` stream the hub opens is what an ssh machine's `devsandbox
tunnel` is at the far end of a forward's ssh
([forwards](forwards.md#remote-bridges)), served by
`fleet::tunnel::serve` on a thread of its own
(`Shared::open_lent_tunnel`).

- **The request** is the hub's first line (within 30 s, at most 64 KiB,
  read byte by byte: `tunnel::read_request`), `tunnel::Request` as JSON:
  `{"root": <abs config root>, "instance"?: <name>, "service"?: <name>,
  "port": <n>}`, the arguments `devsandbox tunnel` takes and with its
  rules: an instance, a service or both; the instance as
  `resolve_instance` takes a name (exact name or id, then sandbox or
  folder basename; ambiguous refuses).
- **The gate**: `root` confined as an `API` request's is
  (`tunnel::confine`, `lent::confine_path` over the link's `lent::Reach`,
  read per stream: under the fleet root or an own root under `worker`, any
  root under `full`; it must exist). The route is resolved over
  `tunnel::scoped` state only: the lent instances whose config root is
  under `root`, so neither the
  instance name nor a service's via-instance pick (`port::plan`) can land
  on any other instance. A service must be one such an instance uses, the
  named one when there is one (`tunnel::check_service`,
  `port::uses_service`), so the injection fallback only reaches a service
  container of that root's project (a `global` one too, which the
  [worker policy](#worker-policy) denies unless allowed); `denied`
  refusals are logged.
- **The answer** is the route resolved as `devsandbox tunnel` resolves it
  (`commands::tunnel::resolve` over `port::resolver_in`, the state
  loader injected), written as one `commands::tunnel::Header` line
  (`Header::of`): the route, or `{"error"}` for any refusal, after which
  the stream closes. After a route the worker runs `bridge::bridge_command`
  into its container with the stream itself as stdin and stdout (its
  fds, `Stdio::from`), its stderr line by line in the daemon's log, so
  the bridge's handshake and mux follow the header as over ssh. The
  bridge is killed and reaped once the hub closes the stream (watched
  with `daemon::hung_up`), or the stream closes when the bridge exits.

**The hub's end** is the forward engine's opener
(`commands::tunnel::remote_opener` with `tunnel::Via::Fleet`), in the
hub's daemon (`forwards::via`, chosen per spawn from the machine's
registry entry) or a foreground `devsandbox port` (`port::port_remote`).
Each open connects to the worker's `fleet-<name>.tunnel.sock` in the
socket dir (absent: `its fleet link isn't up (…)`), writes the request,
reads the header (`read_header`, up to 10 s, `stop` checked meanwhile)
and attaches the bridge to the stream (`bridge::attach` over a
`bridge::Transport::Stream`: killing it shuts the stream down, and a
stream that ends is a bridge that's done, as a dead child is), so the
engine's self-healing re-opens a new stream as it respawns an ssh. Each
TCP connection is then a `Connect` on that bridge's mux, inside the
`TUNNEL` stream.

### AGENT

The hub's ssh agent on the worker, over the link: the agent relay's
semantics ([machines](machines.md#agent)) for an offered machine. While
the hub's registry entry for the worker has `agent = true`, every
instance there reaches the hub's agent, not only placed ones. Streams go
worker → hub (`channel::AGENT`, flowing), so the worker needs nothing of
the hub but the link it holds.

**The status byte.** Every `AGENT` stream starts with one byte from the
hub: `agent::SERVING` (1), then the relay to the hub's agent, or
`agent::NOT_SERVING` (0), then the close (`agent::status_of`; no byte,
another one or a wait over 5 s is a failure).

**The worker's end** (`fleet::agent::serve`, on a `serve-lend-agent`
thread of the link's `run_link`): once the link's `Hello`s are through
with a hub on this build that advertised `caps::AGENT`, and the outlet
is up, it probes the hub every `agent::PROBE` (2 s): an `AGENT` stream
(`link::Outlet::open`) whose status byte it reads, then closes. While
the hub serves it keeps `fleet::agent::socket` bound,
`endpoint::agent_socket` of client `fleet-<hub>` (`agent-fleet-<hub>.sock`,
the hub its ssh target), as an opener binds its sockets (`link::Socket`:
a temp name renamed over the path, 0600, the newest wins); on
`NOT_SERVING` or a failure it unbinds it (`agent::decide`, pure: bind
when serving and unbound, unbind when bound and anything else). Each
connection to the socket becomes one `AGENT` stream on a thread of its
own: past a `SERVING` byte relayed both ways on two threads, each
direction's end passed on as a half-close; any other answer closes the
client and unbinds the socket at once. When the link ends (the outlet
goes down) or `run_link`'s answerer returns, the socket is unlinked if
it is still its own. Named `agent-*.sock`, it is one of the agent relay
sockets `bridge::host_agent` tries (`endpoint::agent_sockets`, newest
first, each probed with a connect), so the worker's bridges reach the
hub's agent as they reach a machine's relayed one, and only while it
leads somewhere: with `agent` off on the hub, or the link gone, there is
no socket to pick, and the worker's own agent is the fallback.

**The hub's end** (`commands::fleet_link`): its table serves `AGENT`
(so `caps::AGENT` is always advertised) with `fleet::agent::Gate::open`,
on the mux's read thread: always a socket pair at once, whose far end
`agent::answer` takes on a thread of its own. It writes `NOT_SERVING`
without a fresh agent path; else `SERVING`, and once the worker's first
bytes come (a probe sends none and closes, so it never dials the agent)
a connection to that path, spliced. The path is the hub daemon's
`fleet.agent {worker}` answer ([api.md](../../reference/api.md#fleetagent)):
the daemon's own `bridge::host_agent` (the agents clients reported
included, as for the machines' agent relays; `daemon::Options::host_agent`
in tests) while the worker's entry is offered with `agent = true`, else
`null`. `agent::refresh` asks it on a thread of its own, started once the
offer is through, on a fresh connection to the hub's daemon
(`client::connect`, `fleet_link::agent_of`) every `agent::REFRESH` (2 s),
and caches it (`agent::Cached`, counted while younger than
`agent::MAX_AGE`, 10 s), so `machine set <w> agent on|off` binds or
unbinds the worker's socket within about `REFRESH` + `PROBE`, on the live
link. A failed read keeps the last answer, which ages out; the first of a
run of failures is on `fleet-link`'s stderr.

### Worker policy

What a config under a fleet root may ask of the host beyond containers
that see only what was lent (`src/worker_policy.rs`). Denied by default;
every item has an allowance in `member.toml`'s `[allow]`, and none of it
applies under `trust = "full"` ([Trust](#trust)).

- **Under the policy by path.** A config dir whose canonical path is
  under `<data>/devsandbox/fleet/` (`fleet_root_of`: symlinks count where
  they point) is under the policy of its fleet root, the first component
  below `fleet/`, whatever process runs it: a lending daemon's verb
  subprocess or a local CLI pointed at the dir. No flag carries it: each
  check point calls `Policy::for_dir` (the data dir from `$XDG_DATA_HOME` /
  `$HOME`, as `state.toml`'s) itself, and reads `trust` and `[allow]` off
  `member.toml` next to `state.toml` (`load_member`: those only, so it is
  read without `serve`; one that doesn't parse fails the verb). Under
  `full`, `Policy::detect` answers no policy.
- **The final argv is the authority.** `Policy::check_run` checks the
  `run` argv of every container right before it reaches the runtime: the
  sandbox's in `commands::run::run_container` (after features, identity,
  caches and mounts are merged in) and every service's in
  `commands::services::ensure_service`, which `run`, `rebuild`, `start` and
  `services rebuild` all create service containers through. Only the flags
  devsandbox writes pass (`-d`, `--name`, `--label`, `-w`, `--init`,
  `--restart`, `-e`, `--user`, `-p`, `--network-alias`, `-v`, `--mount`,
  `--network`, `--userns`, `--security-opt`, `--privileged`), so anything
  else (`--cap-add`, `--device`, `--pid`, `--ipc`, `--volumes-from`, a
  combined `-it`) is refused with no allowance, as are a `-p` whose host
  side isn't bound to loopback (`127.0.0.1:` / `[::1]:`; `publish`
  permits it), `--userns` other than `keep-id…`, `--security-opt` other than devsandbox's own seccomp
  profile (`seccomp-*.json` in the data dir) and networks other than
  devsandbox's (`devsandbox-net-…`; `host` needs `host-network`). The
  first positional is the image; what follows is its command.
  `Policy::check_build` checks a sandbox's or service's `build` argv
  (`commands::run::image::base_image_for`, `services::service_image`):
  the dockerfile and the context resolve under the fleet root, `--network
  host` needs `host-network`, and flags `build.options` could add beyond
  `--pull`, `--no-cache`, `-q`, `--platform`, `--progress` (`--secret`,
  `--ssh`, `--build-context`, …) are refused.
- **Binds** (`check_bind`, `-v` with a path source and `--mount
  type=bind`): the source is resolved as the host would (its deepest
  existing ancestor canonicalized, a `..` in the rest refused) and must be
  under the fleet root or a `binds` entry. A runtime socket (named
  `docker.sock`, `podman.sock`, `containerd.sock`, one of
  `runtime_sockets`' paths, `DOCKER_HOST`'s and `CONTAINER_HOST`'s
  included, or a dir holding one) needs `runtime-socket`. Under the fleet
  root, a path through `shared-volumes/` needs `shared-volumes` (or
  `caches` for a cache's dir), one through `shared-files/` `worktree-link`.
  Named and anonymous volumes and `tmpfs` pass; `--mount` keys beyond the
  ones devsandbox writes (`volume-opt`, which can bind a host path) and
  other mount types are refused.
- **Before anything is done**: `Policy::check_config`, from `run` (right
  after the sandbox resolves, before the runtime is called or the
  instance recorded) and again at the top of `materialize` (so `rebuild`
  too), refuses `initializeCommand` (it runs on the host), `dispatcher`,
  `inbox`, `caches`, `persist-shell-history` (`shared-volumes`),
  `worktree-link`, `privileged`, `global` services, and local features
  whose folder, or any file in it, resolves outside the fleet root (they
  are copied into the image's build context). `folder` and `folders`
  hosts are checked (`check_path`) before their worktrees are made, and
  `resolve_mounts` checks the config's `mounts` before it creates any
  bind source. Any `${localEnv:NAME}` (or `${localEnv:NAME:default}`) in
  the merged sandbox table (`Config::resolved_table`: templates, keys
  included) or its services' tables is refused unless `local-env` lists
  that exact name.
- **`${localEnv:…}` never expands unchecked.** `config::resolve_var`, the
  one place `${…}` variables are expanded, asks `local_env_allowed` over
  the context's `configDir`: under a fleet root a name `local-env` doesn't
  list (or a policy that can't be read) stays verbatim, so what no config
  check saw (a hub's `run --branch`) gets nothing of this host's env.
- **Services' `ports` publish on loopback.** Under the policy
  `services::ensure_service` writes a spec without a host address
  (`5432`, `8080:80`) as `127.0.0.1::5432` / `127.0.0.1:8080:80`
  (`loopback_publish`) before choosing host ports, so a placed service
  with ports works by default; one the config binds elsewhere
  (`0.0.0.0:…`) needs `publish`. `dispatcher` and `inbox` are checked on every control
  request too (`commands::dispatch::policy_gate`), since a config may gain
  them after its instance exists.
- **The refusal** (`Refusal`) names the item and its allowance:
  `refused by this machine's lending policy: privileged (allow with
  privileged = true under [allow] in member.toml)`; an item no allowance
  covers ends `(no member.toml allowance permits it)`.

| `[allow]` key | permits |
|---|---|
| `privileged` | `--privileged` (the sandbox's or a feature's) |
| `runtime-socket` | a bind of a runtime socket or a dir holding one |
| `host-network` | `--network host` on `run` or `build` |
| `caches` | `caches` and their dirs' binds |
| `shared-volumes` | binds under `shared-volumes/`, `persist-shell-history` |
| `worktree-link` | `worktree-link` and its store's bind |
| `global-services` | `scope = "global"` services |
| `dispatcher` | `dispatcher` and its control ops |
| `inbox` | `inbox = true` and its control ops |
| `initialize-command` | `initializeCommand` |
| `publish` | a published port (`-p`) beyond loopback |
| `local-env = ["NAME", …]` | `${localEnv:NAME}` for exactly these names |
| `binds = ["/abs", …]` | host paths outside the fleet root (and under them) for binds, build files and contexts, `folder`s and local features |

The booleans default to `false`, the lists to empty; an unset allowance is
left out of the file `lend::update` writes. `binds` entries must be
absolute (`Allow::check`, part of `lend::check`).
