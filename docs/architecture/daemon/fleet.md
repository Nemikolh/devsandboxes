# Fleet

Machines as one pool of capacity (the plan:
[fleet](../../plans/active/fleet.md)). What exists is the link primitive
the fleet's links are built on and the [peer links](#peer-links) built on
it, which carry every machine; what goes over a machine's link
([over a link](#over-a-link)); [hub mode](#hub-mode); every daemon's
[capacity report](#capacity), carried by its snapshots, and
[placement](#placement), which scores it and which the hub's `run` verb
follows; the link's [`TRANSFER`](#transfer) streams, both ways, with
[moving code](#moving-code) on them (a config root and its repos onto
the machine a `run` is placed on); [versions](#versions): a member
follows its hub's build, a published one downloaded, a dev one received
over `TRANSFER`
([ADR 0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md));
and the [member side](#member-side): `member.toml`, `member.*` with
`devsandbox machine trust|roots`, the trust table and the worker policy
that every remote caller goes through.

**How a machine joins** (decisions 28 and 30 of the plan): on a laptop
L, `devsandbox machine add <hub>` registers the hub K in L's
`machines.toml`; L's daemon then dials K (`ssh <hub> devsandbox
fleet-link --peer <L's member name>`, `fleet::dialer`) and holds that
link. K's `fleet-link --peer` announces L to K's daemon as an inbound
neighbor (`links.inbound`), so L is a machine on K, of kind `inbound`:
its rows, quick calls, verbs, terminals, forwards and placement reach
it over the link L holds. L's agent reaches K's instances while L's
entry for K has `agent` on; K's agent never reaches L.

Code: `src/serve/fleet/mod.rs` (`LINK`), `src/serve/fleet/link.rs`
(`Spec`, `cap_of`, `Handler`, `Serves`, `Endpoint`, `Socket`,
`opener_with`, `answerer_with`, `run`, `Out`, `Outlet`, `Outlet::is_up`,
`Outlet::hang_up`, `End`, `splice`, `write_sentinel`, `match_sentinel`),
`src/serve/fleet/transport.rs` (`Transport`),
`src/serve/fleet/transfer.rs` (`Header`, `Received`,
`send`, `receive`, `Receivers`, `parse_header`, `check_header`,
`receive_then`, `check_size`, `encode_reply`, `parse_reply`, `digest`),
`src/serve/fleet/code/mod.rs` (`Transfer`, `LinkTransfer`, `Haves`,
`offer`, `Offer`, `send`, `prepare`, `Prepared`, `Unprepared`, `layout`,
`pack_root`, `receivers`, `repos_dir`), `src/serve/fleet/code/tar.rs`
(`Writer`, `read`, `extract`, `check_path`, `check_link`),
`src/serve/fleet/code/repo.rs` (`probe`, `cache_dir`, `fetch`,
`has_branch`, `bundle`, `land`, `check_branch`); peer links:
`src/commands/fleet_link.rs` (`fleet_link_peer`, `run_peer`,
`PeerOptions`, `Announce`, `REANNOUNCE`, `attach_stream`),
`src/serve/fleet/dialer.rs` (`Dialer`, `Dialer::hold`, `link_args`,
`member_name`, `dial`, `Open`), `src/serve/endpoint.rs` (`fleet_socket`,
`fleet_transfer_socket`, `fleet_pty_socket`, `fleet_tunnel_socket`,
`fleet_channel_socket`, `link_agent_socket`, `accept_any`),
`src/serve/machines/inbound.rs` (`Neighbors`, `Neighbor`, `Guard`,
`taken`), `src/serve/daemon.rs` (`machine_dialer`, `links_inbound`,
`inbound_roots`), `src/serve/fleet/lent.rs` (`with_fleet_root`); over a
link: `src/serve/fleet/pty_client.rs` (`Request`, `header`, `End`,
`exit_of`, `Terminal`, `attach`, `dial`), `src/serve/machines/cli.rs`
(`dials`, `reconnect_loop`, `session_exec`, `pipe_mode`); hub mode:
`src/daemon_config.rs` (`ServeConfig::hub`),
`src/serve/machines/service.rs` (`Hub`, `save_loop`, `log_failure`),
`src/serve/machines/saved.rs` (`Saved`, `of`, `machines`, `load`,
`save`, `save_wait`), `src/serve/snapshots.rs` (`Machine::set_link`,
`Remotes::restore`); channels: `src/devsbd/proto.rs` (`channel::API`,
`channel::PTY`, `channel::TUNNEL`, `channel::AGENT` and their `caps`,
`channel::flows`); capacity: `src/serve/capacity.rs` (`Capacity`,
`Caps`, `Own`, `HostFacts`, `EngineCache`, `HostCapacity`,
`Capacity::with_own`, `parse_meminfo`, `parse_size`, `over_caps`,
`over_note`, `summary`), `src/runtime/identity.rs` (`EngineInfo`,
`docker_engine_info`, `podman_engine_info`), `Backend::engine_info`,
`src/serve/host.rs` (`poll`, `running_instances`),
`src/serve/snapshots.rs` (`Snapshots::set_capacity`,
`Machine::capacity`, `SnapshotMachine::capacity`); placement:
`src/serve/fleet/place.rs` (`place`, `score`, `Spec`, `Candidate`,
`MachineRef`, `Placement`, `NoPlace`, `has_image`, `reachable`),
`src/serve/fleet/placed.rs` (`worth_placing`, `workers`,
`reachable_workers`, `fleet_root`, `CAPACITY_WAIT`, `with_fresh`,
`spec`, `candidates`, `Ask`, `decide`, `Placed`, `start_point`, `lines`,
`forwarded`, `tag_answer`, `Tagged`), `src/serve/daemon.rs`
(`placed_run`, `fresh_capacities`, `fleet_root_on`, `remote_verb`,
`Shared::capacity`, `Shared::own`, `Options::local_capacity`,
`Options::repos`), `src/serve/verbs.rs` (`RunParams::here`),
`src/serve/verbs_client.rs` (`run_target`, `Streaming`),
`src/serve/machines/cli.rs` (`plan`); versions:
`src/serve/fleet/update.rs` (`HELLO`, `hello_of`, `is_hub`,
`platform_of`, `Build`, `decide`, `Decision`, `hub_pushes`, `push`,
`install_received`, `scratch_dir`, `release_target`, `asset_url`,
`curl_argv`, `update`, `Steps`, `Release`), `src/serve/fleet/follow.rs`
(`Follow`, `Incoming`, `PUSH_WAIT`, `EndLink`, `shutting`),
`src/commands/fleet_link.rs` (`follows`, `push_build`, `own_binary`),
`src/serve/daemon.rs` (`follow_hub`, `Options::update_steps`),
`src/main.rs` (`Command::FleetHello`), `build.rs` (`stamp_build`),
`src/serve/machines/install.rs` (`install_local`, `version_of`),
`src/serve/proto.rs` (`HelloParams::force`), `src/serve/client.rs`
(`hello_with`), `src/serve/endpoint.rs` (`peer_is_own_user`),
`src/serve/daemon.rs` (`hello`); the member side:
`src/serve/fleet/member.rs` (`Member`, `SoftCaps`, `load`, `check`,
`update`, `default_name`, `merge`, `call`, `MemberInfo`),
`src/serve/locked_file.rs` (`update`), `src/commands/machine.rs`
(`trust`, `roots`, `record_roots`), `src/serve/fleet/lent.rs` (`Peer`,
`Channel`, `attach_params`, `peer_fleet_root`, `Files`, `Reach`,
`own_roots`, `decide`, `admit`, `confine_path`, `lent_instances`,
`filter_answer`, `filter_snapshot`, `cut_containers`),
`src/serve/daemon.rs` (`Options::member`, `Options::peer_env`,
`Options::host_agent`, `member_path`, `Table`, `Shared::reach_files`,
`Shared::open_lent`, `Shared::serve_channel`, `link_attach`, `Handoff`,
`agent_for`), `src/serve/fleet/agent.rs` (`SERVING`, `NOT_SERVING`,
`Status`, `status_of`, `Action`, `decide`, `serve`, `PROBE`, `answer`),
`src/serve/fleet/pty.rs` (`METHOD`, `code`, `Header`, `parse_header`,
`gate`, `attach_params`, `serve`), `src/serve/fleet/tunnel.rs`
(`read_request`, `scoped`, `confine`, `check_service`, `serve`),
`src/commands/tunnel.rs` (`Request`, `resolve`, `Header::of`, `Via`,
`remote_opener`), `src/commands/port.rs` (`resolver_in`, `LoadState`,
`uses_service`), `src/devsbd/bridge.rs` (`Transport`, `attach`),
`src/commands/exec.rs` (`ExecContext`, `exec_context`),
`src/serve/helper.rs` (`HelperClient::call_with_input`, `Input`),
`src/serve/api.rs` (`sessions_list`, `sessions_rm`,
`session_container`, `helper_rpc`, `SessionsList`),
`src/worker_policy.rs` (`Trust`, `Allow`, `Policy`, `fleet_root_of`,
`load_member`, `enforce`, `enforce_argv`, `Refusal`).

## Link

A link is one byte pipe between two hosts (a [transport](#transport)'s
connection: an ssh's stdin/stdout in production) carrying the helper's [frame protocol](../helper/protocol.md),
with the roles of a container's link moved onto the two hosts. A connection
to a socket on one host becomes a stream on the mux, answered on the other
host by a connection made there. A `Spec` says what a link is: its
sentinel line (`\n<sentinel> <version>\n`), the channel it stands for
(the opener requires the peer to serve it), and the `Hello` hash (on the
fleet link the build, which a member [follows](#versions)).

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
wins (`link::Socket`); each accepted connection (one accept loop over all its sockets,
`endpoint::accept_any`) becomes one `Open { channel }` stream on its
socket's channel, or is dropped unopened when the peer didn't advertise
that channel. The end unlinks each socket when the link ends, only while
it is still its own (same device and inode), never a newer end's. The
outlet is set to the link from the `Hello`s until it ends:
`Outlet::open(channel)` opens a stream (flowing channels only), an error
when no link is up or the peer didn't advertise the channel;
`Outlet::hang_up` ends the live link from that end's own code, as if the
peer's stream had ended. Handlers run
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

- **The opener** (`link::opener_with`, the end ssh runs: a
  [peer link](#peer-links)'s dialed end) binds its sockets, writes the
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
- **The answerer** (`link::answerer_with`, the end that dials: a
  [peer link](#peer-links)'s dialer) skips what the login
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
  closed.

One spec exists:

| spec | sentinel | channel | cap | opener / answerer |
|---|---|---|---|---|
| `fleet::LINK` | `devsandbox-fleet 1` | `API` (flowing) | `caps::API` | `commands::fleet_link::run_peer` (a [peer link](#peer-links)'s dialed end) / `fleet::dialer::Dialer::hold` (the dialer); each end's `hello` `update::hello_of` |

Each end binds `fleet-<peer>.sock` for `API` (`endpoint::fleet_socket`)
and its `.pty`, `.tunnel` and `.transfer` siblings
(`endpoint::fleet_pty_socket`, `fleet_tunnel_socket`,
`fleet_transfer_socket`; a further channel's socket is
`endpoint::fleet_channel_socket`, `fleet-<name>.<suffix>.sock`), and
serves `API`, `TRANSFER`, `PTY` ([PTY](#pty)) and `TUNNEL`
([TUNNEL](#tunnel)) with its daemon; the dialer also serves `AGENT`
([AGENT](#agent)), which the dialed end opens. Each connection to an
end's API socket is one connection to the far host's daemon, so a
`client::Conn` on one host says its hello to, and calls, the daemon on
the other.

**No sentinel inside a stream.** An `API` stream is the daemon from its
first byte: a mux stream has no login shell in it to skip (fleet plan
decision 27), so `machines::link::handshake` says its hello at once
(`Over::Link`). Only the link itself, and `machine add`'s probe over ssh
(`api --stdio --sentinel`, [machines.md](machines.md#link-handshake)),
start with a sentinel line.

## Transport

What carries a link's bytes is one seam, `fleet::transport::Transport`:
`Transport::dial(target, program, args)` reaches `<program> <args>` on
`target` (`[user@]host`, an ssh-config alias or `ssh://[user@]host[:port]`)
and hands back a `machines::ssh::Duplex`, one byte pipe to that
command's stdio. ssh is its only implementation (`Transport::ssh`: `ssh
-T -o BatchMode=yes [-p N] -- <host> <program> <args>` through the
target's login shell, built by `ssh::target_argv`, the builder every
machine argv uses, and spawned by `ssh::spawn`; `BatchMode`: the daemon
has no TTY to prompt on). Swapping ssh, or spreading a link over several
connections for bandwidth, changes that module and nothing that dials.
Callers name the target, the program (a machine entry's `command`) and
the devsandbox args only (`dialer::link_args`); the daemon's is
`Transport::ssh` in production, `Options::link_transport` in tests,
which build a `Transport::new` whose far end runs in-process and see
both.

## Peer links

`machine add K` run on L makes a link L → K (decisions 7, 22, 23 and 28
of the plan): L dials `devsandbox fleet-link --peer L` on K through the
[transport](#transport), `L` being L's member name
(`dialer::member_name`: `member.toml`'s `name`, else the hostname's,
read at each dial), and both ends run both halves of a link. K's end
is `commands::fleet_link::run_peer` (`fleet_link_peer` from the CLI:
the required `--peer` passes `machines::check_name`), ssh's child
there, its stdio the link: the opener of `fleet::LINK` (it writes the
sentinel), saying `update::hello_of(hub)` (below, *Versions*), `hub`
read from `daemon.config.toml` as the daemon reads it
(`daemon_config::load`).

- **IN.** Each stream L opens on `API`, `PTY`, `TUNNEL` or `TRANSFER` is
  answered by `attach_stream`: a socket pair whose far end a thread takes,
  connecting K's daemon socket and sending `link.attach {peer: L,
  channel}` as the connection's first line, reading the answer byte by
  byte (nothing past it), then splicing the stream to the connection
  (`link::splice`); an `API` stream is K's daemon from its first byte
  ([no sentinel inside a stream](#link)). Every
  stream is therefore served by K's daemon under K's trust table for L,
  with L's fleet root `fleet/L/` (`Shared::serve_channel`); a refused
  attach closes the stream, with why on stderr. `AGENT` isn't served: it
  opens only member → dialer (decision 18). Per-stream connects reach
  whichever daemon is up, a successor after a version handoff included.
- **OUT.** K binds `fleet-L.sock`, `fleet-L.pty.sock`,
  `fleet-L.tunnel.sock` and `fleet-L.transfer.sock`
  (`endpoint::fleet_socket` and its siblings): each accepted connection
  is a stream to L, which L's daemon answers. While L serves `AGENT`,
  `agent::serve` binds `agent-link-L.sock` (`endpoint::link_agent_socket`,
  through K's outlet): an `agent-*.sock`, so K's bridges pick it as a
  relayed agent (`endpoint::agent_sockets`).
- **The inbound neighbor.** Once the `Hello`s are through (and this end
  follows nobody), K's end announces L on a connection of its own
  (`links.inbound {peer, version, hub}`, the version L's `Hello` hash, the
  hub flag `update::is_hub` of it) and holds that connection for the
  link's life (`Announce`): K's daemon keeps L in its inbound neighbors
  (`machines::inbound::Neighbors`) while the connection lives, the newest
  announce of a name winning (`Guard`), and asks L's roots over
  `fleet-L.sock` on a thread (`daemon::inbound_roots`: the hello as
  `0.0.0`, then `roots.list`, which L answers with its own roots and
  K's fleet root there, flagged `fleet`, `lent::with_fleet_root`). L is
  then a machine on K like a registered one, of kind `inbound`: K's
  `Machines` service holds a link to it over `fleet-L.sock` while the
  neighbor is up, so its rows, quick calls, verbs, the CLI's re-runs,
  terminal tabs, forwards and placement reach L through the sockets this
  end binds ([over a link](#over-a-link)), each stream served by L's
  daemon under L's trust table for K
  ([machines](machines.md#inbound-neighbors)). A name that
  `machines.toml` has is refused, and the refusal ends the
  link with why on stderr (L sees ssh's stderr tail); `machines.add`
  refuses a live neighbor's name in turn. When the held
  connection drops (K's daemon handed off or restarted), `Announce`
  connects and announces again, backing off from `REANNOUNCE` (100 ms) to
  5 s; the link stays up meanwhile. A refusal then ends the link.

L's end is the daemon's: each ssh machine `K` in L's `machines.toml` is
held by its `serve-machine-K-link` thread running
`fleet::dialer::Dialer::hold` (the service's `Hold`, built by
`daemon::machine_dialer`; [machines](machines.md#held-links-machines)),
redialed after the backoff (`service::backoff`: 1 s doubling to 30 s)
whenever it ends, the far end's refusal (ssh's stderr tail) its error.
It is the answerer of `fleet::LINK` (it reads the sentinel), saying
`update::hello_of(hub)` for L, with the sentinel and the `Hello`s within
`machines::service::Timing::handshake`.

- **OUT.** L binds `fleet-K.sock` and its `.pty`, `.tunnel`,
  `.transfer` siblings in its socket dir (newest binder wins: a
  successor daemon's link takes them over, the old daemon's ssh dying
  with it); each accepted connection a stream to K. Every client here
  reaches K through them ([over a link](#over-a-link)).
- **IN.** Each stream K opens on `API`, `PTY`, `TUNNEL`, `TRANSFER` or
  `AGENT` goes to L's daemon in-process (`dialer::Open`: `Shared::open_lent`
  with peer `{name: K, fleet_root: fleet/K/, env: the daemon's}`), the
  `serve_channel` a `link.attach` connection gets (decision 24); `AGENT`
  is answered with L's agent while L's entry for K has `agent` on
  (`daemon::agent_for`, read per stream: decision 18). While the hub's
  binary is on its way every stream but that `binary` transfer is
  refused.

## Over a link

Every client reaches a machine, ssh or inbound, through the sockets its
link's local end binds, `fleet-<name>.*.sock` in the daemon's socket dir.

- **The dial** (`fleet::dialer::dial`, the daemon's `Dial` for every
  machine): a connection to its `fleet-<name>.sock` as a plain
  `ssh::Duplex::socket`; down, `its link isn't up (…)`. Each connection
  is one `API` stream to the machine's daemon, so the held link
  (`service::Machines`), the quick calls and `forward::verb` (one stream
  per verb, a hang-up the stream's close) run on it. A link's failure is
  logged only when it differs from the last one logged since the link
  was up (`service::log_failure`), so an absent machine is one line in
  `serve.log`, not one per retry.
- **Terminals**: a tty `exec` on a machine (and its `--attach`,
  `--sessions`) and the dashboard's instance and service tabs, which run
  that `exec` locally, are `PTY` streams ([PTY](#pty), *The caller's
  end*); so is an `exec` without a tty, as a piped `PTY` stream
  ([PTY](#pty), *Pipe mode*).
- **Port forwards**: the daemon's forwards into a machine (`forwards.add`
  with `machine`) and a `devsandbox port` on one are `TUNNEL` streams
  ([TUNNEL](#tunnel), *The caller's end*).
- **The agent**: with `machine set <name> agent on` this host's agent
  reaches every instance on that machine as `AGENT` streams
  ([AGENT](#agent), [machines](machines.md#agent)); only on a machine
  this host dials (its entry's `agent`), never on an inbound one.
- **The CLI's other commands on a machine** are API calls with `machine`
  (fleet plan decision 15), within the machine's trust: `logs`
  (`containers.logs`), `logs --build` (`instances.buildLog`; `--follow`,
  its pages by `offset`), `inspect` (`containers.inspect`), `done` /
  `undone` (`instances.done`), `rename` (`instances.rename`), `--machine
  m ls` / `status --json` (`instances.list` per config root under the
  fleet root; `service ls` too, its service rows), `--machine m ps`
  (`containers.list`) and `stats` (`containers.stats`), printed as the
  local commands print them ([machines](machines.md#the-cli-clirs),
  `machines::cli::calls`). `--machine m gc [--force]` is the `gc` verb
  per config root (`verbs_client`'s `CliVerb::Gc`), which the machine
  runs under `full` trust only: under `worker` its refusal is the error.
  The dashboard's logs and inspect views go through the same quick calls.
- **What has no link form yet** is refused, each in its own words:
  `vscode` (`cli::calls::refusal`, on any machine) and the dashboard's
  tab on a global service no instance uses (`tui::remote::service_argv`:
  a service dial goes through an instance using it). No ssh argv is
  built for an inbound machine: `machines::ssh`'s `argv_with` takes
  `Entry::ssh_target`, an error for one.

## Transfer

A link may also carry bulk transfers on flowing `TRANSFER` streams, in
both directions; `fleet::LINK`'s two ends do (each serving `TRANSFER`).
Both ends then advertise `caps::TRANSFER` in their `Hello`s, and each
opens `TRANSFER` streams only toward a peer that advertised it.

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

**Receivers.** A `TRANSFER` stream is handed to a `Receivers`: one entry
per kind (`Receivers::with`: a size limit, `decide`, and a `done` run
with each `Received` after its `ok`). The handler answers the `Open`
with one end of a socket pair (`Receivers::accept`) and receives on a
`fleet-transfer` thread of its own; a kind with no entry is refused
("nothing here receives \`<kind>\` transfers").
`Receivers::on_failure` adds a kind's hook for a transfer that failed
after its `decide` took it. Each end's streams reach its daemon's
`Shared::serve_channel` (the dialed end's through `link.attach`, the
dialer's in-process), which receives `root-tar` / `bundle`
([Moving code](#moving-code)) with `code::receivers` over the peer's
fleet root. While an end waits for the hub's build, its link's own
`binary` receiver takes the one transfer instead (`update::BINARY_KIND`,
at most `BINARY_LIMIT`, 256 MiB: the hub's dev build,
`follow::Incoming::receiver`, [Versions](#versions)).

**Directions.** The opener's streams carry ids below 2^31, the
answerer's high-bit ids (`Outlet::open(TRANSFER)`), so the two
directions never share an id space. A process on either host connects
to `fleet-<peer>.transfer.sock` and calls `transfer::send` on it.

## Moving code

A sandbox goes to the machine it's placed on (the worker) as its config
root and its folders' repos, so the worker runs the config as the hub
has it (dockerfiles, local features) on a checkout of the branch (fleet
plan decision 12: the hub reaches `origin` with its own credentials; a
worker never needs any). It sits behind one seam, `serve::fleet::code`'s
`Transfer` (`haves`, `send`; `offer` builds what goes, `receivers` takes
it), so a sync layer can replace the git + tar implementation;
`LinkTransfer` sends each body as one `TRANSFER` through the machine's
link (a connection to `endpoint::fleet_transfer_socket`).
`prepare(transfer, repos, worker root, config root, sandbox, branch)` is
`offer` then `send`, answering `Prepared` (the config root on the
worker, `remote_root`, and each moved folder with its branch) or
`Unprepared`: `NotMovable(why)` (it runs where it is, the why for the
user) or `Failed`. The placed `run` calls it
([In the `run` verb](#in-the-run-verb)).

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

**Receive** (`receivers`, on the worker, into the hub's fleet root
there). Bodies wait in `roots/.in/`.

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

## Hub mode

`[serve] hub = true` in `daemon.config.toml` (`ServeConfig::hub`, read
once at start into `daemon::Options::hub`; `serve install` on the server
keeps it running) makes the daemon a hub:

- **It never idles out**: `hub` implies `keep-alive` (`daemon::run`
  passes `keep_alive` to `idle::decide` for a hub). Nothing else keeps a
  daemon alive for the fleet.
- **Its links say `hub`**: the `Hello` of every peer link it holds or is
  dialed on ends `hub` (`update::hello_of`), so the member at the other
  end follows its build ([Versions](#versions)), and pushes its dev build
  to a member that takes one.
- **It places runs**: its `run` verb places a request that names no
  machine ([In the `run` verb](#in-the-run-verb)).
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
every listing (watched or not), and the daemon adds `member.toml`'s caps
(`Shared::capacity`, `Capacity::with_own`); tests replace the
rest with `Options::local_capacity`. It is on the trust table under either trust, so
a host asks its machines before it places.

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
- **`caps`**: `member.toml`'s `[caps]` in bytes (`Caps::of`; none when
  no cap is set), read through `Shared::own` (`capacity::Own`) each
  time, so a hand edit shows on the next report.

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
`remotes.json` rows. The trust table's snapshot filter
(`lent::filter_snapshot`) leaves it as is. The dashboard's Machines tab
and `devsandbox machine ls` show it as CAPACITY (`capacity::summary`:
`8 cpu · 12/32G · 120G free · 3 run`, memory in use / total, then the
over-cap note).

## Placement

Where new work goes (fleet plan decision 11: placement runs in the
daemon). `place::place` is pure over a `Spec` (the sandbox, its
`config_hash` / `build_hash`, the repo, the requester, an optional pin)
and the `Candidate`s, each a `MachineRef` (this host or a machine name)
with `reachable`, its `Capacity` (or none yet), `has_image` and
`has_repo`. It answers a `Placement` (the machine, `warnings`, the
`score` and its `reasons` as text) or a `NoPlace`.

- **A pin wins** (`Spec::pinned`: `--machine m` or a qualified name).
  It is refused only when the machine isn't a candidate
  (`NoPlace::UnknownPin`) or is unreachable (`NoPlace::PinUnreachable`);
  over a cap, it is used with a warning.
- **New work never goes** to an unreachable machine. None left:
  `NoPlace::NoCandidate`, each skipped one with why.
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
`Candidate::local` (this host: always reachable) and
`Candidate::from_machine` over a `snapshots::Machine` of either kind (its
`Machine::capacity`; `place::reachable`: while its link is `connected`).
`place::has_image` reads roots' `instances.snapshot` params: a sandbox
row of the spec's name at its `config_hash` (and `build_hash`, when the
spec has one) with a ready, non-drifted instance of that sandbox from
the same root, since image tags are per config root. No snapshot says
which repos a host has, so `has_repo` is the caller's to fill.

### In the `run` verb

A hub's (`[serve] hub`) `run` verb places a request that names no
`machine` and isn't `here` (`daemon.rs` `placed_run`, the decisions pure
in `fleet::placed`); any other host runs it here. The candidates are
this host and every machine, ssh or inbound (`placed::workers`:
placement candidates are every neighbor), the ones whose link is up
taking new work; code moves over the machine's link into this host's
fleet root there, which every peer link's `TRANSFER` receives into
(`Shared::serve_channel`), so a placed run needs no ssh of its own. A
`--machine` or qualified run is forwarded as before.

- **Worth it?** `placed::worth_placing`: some machine reachable (its
  link `connected`, read from the links' last states, no I/O). If none,
  the run is local with nothing else read or asked.
- **Fresh reports** (`daemon.rs` `fresh_capacities`): each reachable
  machine is asked `capacity.get`, in parallel, each on a fresh `API`
  stream over its link's dial (`machines::link::handshake`, then the
  call), each bounded by `placed::CAPACITY_WAIT` (2 s) for the dial and
  hello and again for the answer. An answer replaces the machine's saved
  report (`placed::with_fresh`, pure); a timeout or an error leaves the
  saved one (its last push, or `remotes.json`'s).
- **Candidates** (`placed::candidates`): `Candidate::local` with this
  host's capacity built now (`Shared::capacity`, as `capacity.get`
  answers it) and every watched root's snapshot, and `from_machine` for
  each machine with its fresh report. The `Spec` (`placed::spec`) takes the sandbox's
  `config_hash` / `build_hash` from the request root's snapshot when it
  is watched, and the requester is this host.
- **Decide** (`placed::decide`): this host wins (the `REQUESTER` bias
  breaks a tie) → the local verb, unchanged. A machine wins →
  `code::prepare` for it: its fleet root for this host is the row its
  `roots.list` flags `fleet`, asked over a fresh `API` stream
  (`daemon::fleet_root_on`, `placed::fleet_root`; none, or no answer
  within `CAPACITY_WAIT`, is a `NotMovable`), the transfer its link's
  `LinkTransfer` (`fleet-<name>.transfer.sock`), the repo cache
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

## Member side

What a host is to the machines linked with it: its name, trust, roots,
soft caps and allowances in `member.toml`, and how it serves a link
peer's streams. A peer reaches this host only through its link, and
there only through the trust table.

- **`member.toml`** (`<data>/devsandbox/member.toml`, next to
  `state.toml`, `fleet::member::path`; `Options::member` in tests) is
  this host as a member. Keys: `name` (what this host goes by on the
  links it dials; absent is the hostname's first label,
  `member::default_name`, which must pass `machines::check_name`),
  `trust` (`worker` | `full`, absent is `worker`: [Trust](#trust)),
  `dev-builds` (take a hub's own binary when it runs a dev build,
  [Versions](#versions); absent is `false`, left out of the file while
  `false`), `roots` (config roots this host shows the fleet besides
  those its `state.toml` has instances in, absolute; `devsandbox machine
  roots add|rm` on this host sets them, and `machine add` run elsewhere
  records the roots it picks here that way:
  [machines.md](machines.md#devsandbox-machine-addlssetrmtrustroots)),
  an `[allow]` table, the [worker policy](#worker-policy)'s allowances,
  and a `[caps]` table, the soft caps of the
  [capacity report](#capacity) (`member::SoftCaps`: `cpus`, `mem`,
  `disk`, `instances`, each optional; `mem` and `disk` sizes like `16G`,
  checked by `capacity::parse_size`). Unknown keys are ignored.
  `member::check`: a `name` that's set is valid, `roots` are absolute,
  `[caps]` and `[allow]` well-formed. The daemon is its only writer
  ([Member: `member.*`](#member-member)), with `member::update`, a
  read-modify-write under the sibling `member.lock`, written to a temp
  file and renamed (`locked_file::update`, which `machines.toml` and
  `forwards.toml` are written with too); `dev-builds`, `[allow]` and
  `[caps]` are edited by hand (no call changes them) and read at each
  use.
- **A peer's fleet root** is `<data>/devsandbox/fleet/<peer>/`
  (`lent::peer_fleet_root`, `<peer>` the link peer's machine name),
  created when the peer's first stream is served. What the peer places
  here goes under it ([moving code](#moving-code)).
- **Each `API`, `PTY`, `TUNNEL`, `TRANSFER` and `AGENT` stream** a peer
  opens reaches `Shared::serve_channel`, the one server of a peer's
  channel streams: through a `link.attach {channel}` connection on the
  dialed end, through `Shared::open_lent` (a socket pair whose daemon end
  goes there) on the dialer ([the trust table](#the-trust-table)). An
  `API` end is served like any connection from its first byte ([no
  sentinel inside a stream](#link)) (`spawn_conn_as`), with the `Peer`
  table; its connection id comes from the same counter as the
  listener's. A `PTY` end is `fleet::pty::serve`'s on a `serve-pty`
  thread ([PTY](#pty)), refused without a host side (no helper client);
  a `TUNNEL` end `fleet::tunnel::serve`'s ([TUNNEL](#tunnel)). All are
  refused once the daemon stops.

### Versions

A member follows its hub's exact build, an older one included
([ADR 0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md)),
on a [peer link](#peer-links), whichever end dialed (decision 25):

- **The build in the `Hello`.** `fleet::LINK`'s `hello` is
  `update::hello_of(hub)`: `fleet::update::HELLO`, `<semver> <git>
  <published|dev> <os>/<arch>` (`0.8.0 1a2b3c4d5e6f dev linux/x86_64`),
  and a fifth word, `hub` when that end's host is the hub (`[serve]
  hub`), else `member` (`update::is_hub` reads it). `HELLO` is from
  `build.rs`'s `stamp_build`: `DEVSANDBOX_BUILD_PLATFORM` is the target's
  `CARGO_CFG_TARGET_OS` / `CARGO_CFG_TARGET_ARCH` (what
  `std::env::consts` says); `DEVSANDBOX_BUILD_GIT` is `git rev-parse
  --short=12 HEAD` of the source tree (`unknown` when git or the repo is
  missing; the build never fails for it), watched through `HEAD`, the
  ref it names and `packed-refs`; `DEVSANDBOX_BUILD_KIND` is `published`
  when the build ran with `DEVSANDBOX_PUBLISHED=1` (meant for the release
  workflow), else `dev`. `Build::parse` reads the first three words back
  (a missing one is `unknown` / dev), `update::platform_of` the fourth
  (`unknown` without one): the platform isn't part of the build, one
  release being built for several; both ignore the fifth.
  `devsandbox fleet-hello` (hidden) prints `HELLO`.
- **Who follows** (`commands::fleet_link::follows`, pure, over both
  `Hello`s): the end that isn't the hub follows a hub peer through
  `update::decide`; two non-hubs only need the frame protocol's match; a
  hub never follows. The dialed end runs it in `run_peer`'s `on_ready`,
  the dialer (`fleet::dialer`) in the answerer's.
- **The decision** (`update::decide`, pure, over this build and
  platform, the hub's, and whether this host takes dev builds:
  `member.toml`'s `dev-builds`, or trust `full`
  (`member::Member::takes_dev_builds`), read at each link's `Hello`s
  through `follow::Follow::dev_builds`): the same semver, git and kind
  is `Same`, and the link serves; another build that is published is
  `Download { version }`; another dev build is `Receive` when the
  platforms are equal and dev builds are taken, else `Stop(why)`:
  `dev build: can't run on <our os>/<arch> (the hub runs <hub build>
  for <its os>/<arch>)`, checked first since `dev-builds` can't help
  there, else `` dev build: not taken (`dev-builds = true` in
  member.toml or `devsandbox machine trust full` takes it; the hub runs
  <hub build>) ``. `Download` and `Stop` end the link before the first
  stream; `Receive` keeps it up serving only the hub's binary.
- **The push**: the hub's end, once the `Hello`s are through, pushes its
  own binary when `update::hub_pushes` (over both `Hello`s: the member,
  taking dev builds, would `Receive`), from a thread of its own
  (`commands::fleet_link::push_build`): `update::push`, one `binary`
  transfer named `devsandbox` through `fleet-<member>.transfer.sock`.
  On a dialed hub the binary is the `fleet-link` process's
  (`own_binary`: `/proc/self/exe` on Linux, the inode it runs even if
  the path was replaced since; `current_exe()` elsewhere), not the hub
  daemon's: it is the one that said the link's `Hello`, so the build the
  member decided on. The hub pushes rather than waiting to be asked: the
  member's decision stands either way, since it refuses the header (no
  body sent) unless it waits for a binary, and a member taking no dev
  builds has ended the link before the push opens; no request kind or
  round trip is needed.
- **`Receive`**, the member's side: every stream but the `binary`
  transfer is refused and a wait is set up (`follow::Incoming`: a
  private scratch dir under the temp dir, `update::scratch_dir`). The
  `binary` receiver takes one transfer while a wait is on (else `this
  member isn't waiting for a binary`) into `<scratch>/devsandbox` (0600,
  as every transfer lands), then ends the link. No transfer started
  within `PUSH_WAIT` (60 s: `Incoming::watch`) or one that failed ends
  it too, with why. With the link down, `update::install_received` over
  `Follow::steps`: the file made 0755, its `fleet-hello` (`Steps::build`)
  must report the hub's `Hello` word for word (semver, git, kind,
  platform: the first four), installed by `install::install_local` as a
  download is, the installed binary's `fleet-hello` checked again, then
  the forced handoff, below. The scratch dir is removed either way.
- **`Download`**: `update::update` over `Follow::steps` (`update::Release`
  in production; `Options::update_steps` in tests): the asset
  `devsandbox-<v>-<target>.tar.gz` of release `v<v>` (`asset_url`:
  `https://github.com/Nemikolh/devsandboxes/releases/download/…`, `<v>`
  limited to a semver's characters; `<target>` by `release_target` from
  `std::env::consts`: `x86_64-unknown-linux-musl`,
  `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`, none elsewhere)
  is fetched into a private scratch dir under the temp dir with curl
  (`curl_argv`: `--fail --location --proto =https --proto-redir =https
  --tlsv1.2`, a stall of 30 s gives up) and unpacked with `tar`. Its
  `--version` must report `<v>` (`install::version_of`); it is installed
  over `exe::devsandbox()` (refused while that path is gone) by
  `install::install_local`, `machines::install::INSTALL` run locally as
  `sh -c INSTALL sh <dest>` (temp file next to it, its own `--version`
  check, rename); the installed binary must report `<v>` too. The
  scratch dir is removed either way. Then the handoff, below.
- **The forced handoff** (`Release::handoff`): a hello on the daemon's
  own socket with `force` (`client::hello_with`, `HelloParams::force`)
  as client `fleet-update`. `daemon::hello` hands off on a forced hello
  only from a `Full` connection whose peer runs as the daemon's uid
  (`endpoint::peer_is_own_user`: `SO_PEERCRED` on Linux, `getpeereid`
  elsewhere); a `Peer` connection never hands off, forced or not, and a
  refused force is logged. Then the successor is started as a newer
  client's handoff starts it (`install::lazy_start` with
  `StartReason::Handoff`: the service manager's unit, rewritten to this
  path, when installed; else detached), and waits for the lock while the
  old daemon drains ([role](role.md#version-handoff)). The new daemon's
  hello matches the hub's.
- **On the dialed end** (`run_peer`): `Download` and `Stop` end the link
  from `on_ready`, then `Download` installs and hands off; `Receive`
  ends the link with `Outlet::hang_up` once the binary is in or the wait
  is over, then `install_received`. Either way `fleet-link` then exits
  with what happened (`updated to devsandbox <build>, handing off`, or
  the failure), so the dialer redials and reaches the new binary.
- **On the dialer** (`fleet::dialer`, its `Follow` from
  `daemon::follow_hub`): `Download` and `Stop` refuse the link
  (`End::Refused`), then `Download` kills the ssh, installs and hands
  off; `Receive` ends the link once the binary is in or the wait is
  over, then `install_received` and the handoff. The outcome (`updating
  to <build>`, `updated to <build>: handing off`, `update to <build>
  failed: <why>`) is the machine's `error` until the next attempt, after
  the backoff, which decides again.

### Member: `member.*`

`member.toml` is the daemon's (decision 3 of the fleet plan):
`member.get` and `member.set` (`fleet::member::call(method, params,
path, hostname)`, over the daemon's `Shared::member` path and
`Shared::hostname`) are the only way to change it. Local only: never on
a `Peer` connection (`lent::admit` refuses them under either trust).
Wire: [`member.*`](../../reference/api.md#member).

- **`member.get`** answers `MemberInfo`: the `name` this host goes by
  (`Member::name_or`: the file's, else the hostname's first label; `null`
  when neither is valid), `trust`, `dev_builds` as the file says, and
  `roots`.
- **`member.set {name?, trust?, roots_add?, roots_rm?}`** merges what's
  given into the file (`member::merge`, pure: `name` a
  `machines::check_name`, `""` clears it; `roots_add` absolute, one
  already there kept once; `roots_rm` drops what it names), checks it and
  writes it. It answers the `MemberInfo` after the change. A changed
  name is what the next dial of each link says (`dialer::member_name`
  reads it per dial).

### Trust

`member.toml`'s `trust` is this member's own setting, changed only on it:
`devsandbox machine trust [worker|full]` (`commands::machine::trust`) is
`member.set {trust}`, or `member.get` without an argument, printing the
trust (under `full`, with a note that remote callers get this host's
whole API); `--machine` refuses it with the rest of `machine`
(`serve::machines::cli::machine_use`). It is read for every request a
remote caller sends (`lent::Reach::load`), so a change applies to the
next one. It applies to every link peer alike, whichever end dialed.
`worker` is the default: a remote caller reaches this host's own roots
and its fleet root
through the [trust table](#the-trust-table), placed
work under the [worker policy](#worker-policy). `full` gives it the API a
local client has (but `member.*`, `link.*` and `links.*`), lifts the worker policy
(`worker_policy::Policy::detect` answers none) and takes a hub's dev
builds ([Versions](#versions)).

### The trust table

Every connection has a table (`daemon::Table`): `Full` for local clients,
`Peer(Peer)` for a link peer's `API` stream: a `link.attach {channel:
"api"}` connection on the dialed end, an `API` stream `Shared::open_lent`
hands the daemon on the dialer. A `Peer` (`fleet::lent::Peer`) is who it
is (the peer's machine name, for the log), its fleet root (canonical)
and the env its verbs get (this host's own, the daemon's
`Shared::peer_env`; a `Peer` without one, only in tests, keeps a verb's
as sent). The
`PTY` and `TUNNEL` streams have no table: `fleet::pty::serve` and
`fleet::tunnel::serve` read the same `Reach` for their `Peer` and gate
the root and instance against it ([PTY](#pty), [TUNNEL](#tunnel)). For each request on a `Peer` connection the daemon reads
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
- **The fleet root**, the `Peer`'s: `fleet/<peer>/` in the data dir
  (`lent::peer_fleet_root`, decision 24 of the
  [plan](../../plans/active/fleet.md#decisions)).

`Reach::admits` takes a canonical path that is under the fleet root or
exactly one of the own roots (a dir nested in an own root isn't one);
`Reach::admits_recorded` the same for a recorded `config_dir` (canonical
when it exists, never with a `..`). Every gate below uses it.

Each request goes through `lent::admit` at the top of `handle`, before
any dispatch. Under either trust `member.*`, `link.*` and `links.*` are
`denied`: this member's own settings and links are its local clients'
only.

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
  `machines.*`, `shutdown`, `helper.call`, `forwards.*`,
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
- **Env**: a verb's `env` is replaced by the `Peer`'s when it has one:
  the daemon's own process env (`verbs_client::caller_env`,
  `Options::peer_env` in tests), never what the peer sent, its
  `SSH_AUTH_SOCK` the peer's relayed agent (`Peer::agent`:
  `agent-link-<peer>.sock`) while that socket answers.
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

**`link.attach {peer, channel}`** turns a `Full` connection into one
stream of a link peer's: a `fleet-link --peer` sends it first on each
stream it hands its daemon ([peer links](#peer-links)). The daemon
(`daemon::link_attach`) takes it only from a `Full` connection of its own
user (`endpoint::peer_is_own_user`); `lent::attach_params` wants `peer`
(at most 255 bytes, no control characters, and a machine name,
`machines::check_name`, since it names a directory) and `channel`, one
of `api`, `pty`, `tunnel`, `transfer`, `agent` (`lent::Channel`;
required). The peer is `Peer { name: peer, fleet_root:
Some(<data>/fleet/<peer>/), env: Some(<the daemon's env>), agent:
Some(agent-link-<peer>.sock) }`. It is one-way: a second `link.attach`
is `link.*`, which the table refuses. It logs `connection N: <channel>
stream from <peer>, trust <t>` and answers `{ok, trust}`. `api` switches
the connection in place to `Table::Peer`, its pushed snapshots filtered
from then on (`Pending::reach`, as `Shared::spawn_conn_as` sets it for
the dialer's `API` streams). Any other channel sets `ConnState::handoff`: once the
answer is out, `serve_conn` ends its loop, the connection leaves `conns`
(so the drain no longer cuts it) and its stream, `BufReader::into_inner`
with nothing read past the line and its write timeout cleared, goes to
`Shared::serve_channel`:

- `pty` → `fleet::pty::serve`, `tunnel` → `fleet::tunnel::serve`, each
  gating the root and instance on the peer's reach;
- `transfer` → one `fleet::transfer::Receivers::receive` over
  `fleet::code::receivers` of the peer's fleet root;
- `agent` → `fleet::agent::answer` with `Options::host_agent`'s agent
  while this host's `machines.toml` has an entry named the peer with
  `agent` on (`daemon::agent_for`), else none (`NOT_SERVING`, then the
  close): decision 18 of the
  [plan](../../plans/active/fleet.md#decisions), the dialer's agent
  gated by the dialer's own entry.

Each runs on a thread of its own, counted in the `clients` holders
(`Shared::channels`) until it returns. The wait-for-answer rule: the
client sends nothing after the attach line before it read the answer, so
the daemon refuses (`invalid`) an attach whose connection reader already
holds bytes past the line (`ConnState::buffered`), and one on a
subscribed connection (its notifier could still write past the answer).

### PTY

A terminal into a lent instance over the link: each `PTY` stream a link
peer (the caller) opens is one session attach (or a piped exec: *Pipe mode*; a tty exec
where the helper can't attach: *Tty fallback*; a service container's
terminal: *Service terminals*, all below), the serving host's daemon calling the instance
helper's `session.attach` itself over its bridge
(`HelperClient::call_with_input`; decision 13 of the
[plan](../../plans/active/fleet.md#decisions)): no `exec -it`, no client
in the container. The stream carries `rpc` records
(`src/devsbd/rpc.rs`, [protocol](../helper/protocol.md)), and
`fleet::pty::serve` relays them; it parses none past the header.

- **The header** is the caller's first record (within 30 s): a `call` of
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
  `SSH_AUTH_SOCK`), caller `fleet hub <peer>`. It queues behind the
  container's `helper::MAX_CALLS` like any call, abandoned if the caller
  hangs up meanwhile.
- **Records**: caller → server `stdin`, `resize`, `stdin-eof` (any other is
  ignored, as the helper ignores it), into the call's `Input` on a thread
  of their own; server → caller the helper's records unchanged: `stdout`…,
  then exactly one end record, `exit` (`128 + n` for a signal) or
  `error {code, message}` with the helper's code (`no-session`,
  `detached`, `version`, `bad-request`, `failed`). After it the server
  closes the stream.
- **Hang-up**: the caller closing its end cancels the call; the session
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
  (the bridge or its call broke), so the caller can't tell how the session
  is, only that the attach is gone.

**Pipe mode** is a `PTY` stream for an `exec` without a tty: no session,
no helper, a `<runtime> exec` on the serving host with its stdio piped.

- **The header** has `"pipe": {"argv": [...], "interactive"?: bool}`
  instead of `session` / `create` (`pty::Mode::Pipe`; with either of
  them, or without a command in `argv`, it is `bad-request`: the caller
  sends the login shell itself). `interactive` is `exec -i`: whether the
  caller's stdin is forwarded.
- **The gate** is `pty::gate`'s, without the helper: the root confined,
  the instance lent and under it; nothing is asked of a helper (an instance that isn't running is the runtime exec's own
  error, on `stderr`).
- **The exec** is `pty::runtime_exec` of a `pty::Target::Instance`: the
  runtime's `exec_argv_with(instance, interactive, false, argv,
  host_agent)`, the same argv a local `exec` without `-t` runs (`-i` when
  interactive), its stdin the caller's or empty, its stdout and stderr
  piped. The daemon passes `runtime_exec` to `pty::serve` as a
  `pty::ExecCommand` (a pipe's and a tty's argv alike); tests pass a
  program of their own.
- **Records** (`pty::run_pipe`): caller → server `stdin` into the exec's
  stdin, `stdin-eof` closing it (anything else ignored); server → caller
  its stdout as `stdout` and its stderr as `stderr` records (up to 64
  KiB each), then, once it exited and both pipes are drained, one `exit`
  (`pty::exit_record`: its code, or `Exit::Signal` for the signal that
  killed it); a spawn or wait failure is `failed`. Then the server
  closes the stream.
- **Hang-up**: the caller closing its end before the exit kills and reaps
  the exec; nothing more is written. The caller's end closes when its own
  stdout's reader went away (`| head`): `pty_client::attach` then ends
  as `SIGPIPE` (exit 141, no note), as a local exec would.

**Tty fallback** is a session header's terminal for an instance whose
helper can't attach (`pty::Terminal::Tty`): `<runtime> exec -it` on the
serving host under a host pty, not resumable.

- **The exec** is `pty::runtime_exec` of `exec_argv_with(instance, true,
  true, argv, host_agent)` (the local tty exec's argv; `argv` the
  header's `create.argv`, else the login shell), spawned on a fresh host
  pty (`pty::spawn_tty`: portable-pty, 80×24 until the first `resize`, as
  the dashboard's tabs spawn theirs). The session id is unused: nothing
  is resumable. An instance that isn't running is the runtime exec's own
  error, on the terminal.
- **Records** (`pty::run_tty`): first one `stderr` record,
  `this terminal isn't resumable (no helper | helper not root): a dropped
  link ends it` (`pty::NotResumable::note`, ending `\r\n`: the caller's
  terminal is raw by then), then the pty's output as `stdout` records;
  caller → server `stdin` into the pty, `resize` onto it (`MasterPty::resize`;
  `stdin-eof` and anything else ignored); once the child exited and the
  pty's output is drained, one `exit` (`pty::tty_exit_record`: its code,
  or its signal, mapped back from portable-pty's `strsignal` name). Then
  the server closes the stream.
- **Hang-up**: the caller closing its end first kills the child
  (portable-pty's `kill`: SIGHUP, then SIGKILL after a grace) and reaps
  it; nothing more is written. A lost link is that hang-up too, so the
  caller's reconnect re-dials an attach, answered `no-session`.

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
  container named on the serving host by the service's scope
  (`port::service_container_for`, as the forwards name it:
  `isolated_service_container` by the instance's id, or the global
  `service_container`), never by the caller. Not used by it, or no such
  service: `not-found`.
- **The exec** is `pty::runtime_exec` of a `pty::Target::Service`:
  `<runtime> exec -i -t <container>` and the command, else the login
  shell (the dashboard's local service tab's argv), under a host pty;
  records and hang-up as the *Tty fallback*'s, without the note.

`sessions.list` / `sessions.rm` ([api](../../reference/api.md#sessions))
are quick calls, admitted on the `Peer` table with the instance rule
above; the session list and removal go through the helper's
`session.ls` / `session.rm`.

**The caller's end** (`fleet::pty_client`) is a CLI process on the
calling host, not its daemon: a tty `exec` on the machine
([machines](machines.md#the-cli-clirs), `cli::dials`), or the
dashboard tab that runs one. Each dial (`pty_client::dial`) connects to
the machine's `fleet-<name>.pty.sock` in this user's socket dir (absent:
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

A port forward's bridge into a lent instance over the link, every remote
forward's ([forwards](forwards.md#remote-bridges)): each `TUNNEL` stream
a link peer (the caller) opens is served by
`fleet::tunnel::serve` on a thread of its own
(`Shared::serve_channel`).

- **The request** is the caller's first line (within 30 s, at most 64 KiB,
  read byte by byte: `tunnel::read_request`), `tunnel::Request` as JSON:
  `{"root": <abs config root>, "instance"?: <name>, "service"?: <name>,
  "port": <n>}`: an instance, a service or both; the instance as
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
- **The answer** is the route (`commands::tunnel::resolve` over `port::resolver_in`, the state
  loader injected), written as one `commands::tunnel::Header` line
  (`Header::of`): the route, or `{"error"}` for any refusal, after which
  the stream closes. After a route the server runs `bridge::bridge_command`
  into its container with the stream itself as stdin and stdout (its
  fds, `Stdio::from`), its stderr line by line in the daemon's log, so
  the bridge's handshake and mux follow the header on the stream. The
  bridge is killed and reaped once the caller closes the stream (watched
  with `daemon::hung_up`), or the stream closes when the bridge exits.

**The caller's end** is the forward engine's opener
(`commands::tunnel::remote_opener` with `tunnel::Via::Fleet`), in the
calling host's daemon (`forwards::via`, per spawn, for every registered machine)
or a foreground `devsandbox port` (`port::port_remote`).
Each open connects to the machine's `fleet-<name>.tunnel.sock` in the
socket dir (absent: `its fleet link isn't up (…)`), writes the request,
reads the header (`read_header`, up to 10 s, `stop` checked meanwhile)
and attaches the bridge to the stream (`bridge::attach` over a
`bridge::Transport::Stream`: killing it shuts the stream down, and a
stream that ends is a bridge that's done, as a dead child is), so the
engine's self-healing re-opens a new stream as it respawns any bridge. Each
TCP connection is then a `Connect` on that bridge's mux, inside the
`TUNNEL` stream.

### AGENT

The dialer's ssh agent on the machine it dials, over the
[peer link](#peer-links): an ssh machine's `AGENT` semantics
([machines](machines.md#agent)). While the dialer L's registry entry
for K has `agent = true`, every instance on K reaches L's agent, not
only placed ones. Streams go dialed end → dialer (`channel::AGENT`,
flowing), so K needs nothing of L but the link L holds; K's agent never
reaches L (decision 18).

**The status byte.** Every `AGENT` stream starts with one byte from the
dialer: `agent::SERVING` (1), then the relay to its agent, or
`agent::NOT_SERVING` (0), then the close (`agent::status_of`; no byte,
another one or a wait over 5 s is a failure).

**The dialed end** (`fleet::agent::serve`, on a thread of
`commands::fleet_link::run_peer`): once the link's `Hello`s are through
with a dialer that advertised `caps::AGENT`, and the outlet is up, it
probes the dialer every `agent::PROBE` (2 s): an `AGENT` stream
(`link::Outlet::open`) whose status byte it reads, then closes. While
the dialer serves it keeps `agent-link-<dialer>.sock` bound
(`endpoint::link_agent_socket`), as an opener binds its sockets
(`link::Socket`: a temp name renamed over the path, 0600, the newest
wins); on `NOT_SERVING` or a failure it unbinds it (`agent::decide`,
pure: bind when serving and unbound, unbind when bound and anything
else). Each connection to the socket becomes one `AGENT` stream on a
thread of its own: past a `SERVING` byte relayed both ways on two
threads, each direction's end passed on as a half-close; any other
answer closes the client and unbinds the socket at once. When the link
ends (the outlet goes down) the socket is unlinked if it is still its
own. Named `agent-*.sock`, it is one of the relayed agents' sockets
`bridge::host_agent` tries (`endpoint::agent_sockets`, newest first,
each probed with a connect), so K's bridges reach L's agent as they
reach a machine's relayed one, and only while it leads somewhere: with
`agent` off on L, or the link gone, there is no socket to pick, and K's
own agent is the fallback. A verb a `Peer` connection runs gets that
socket as `SSH_AUTH_SOCK` while it answers (`Peer::agent`,
[the trust table](#the-trust-table)).

**The dialer's end**: its table serves `AGENT` (so `caps::AGENT` is
always advertised); each stream goes to `Shared::serve_channel`
(`dialer::Open`), which answers it with `fleet::agent::answer` on a
thread of its own. `answer` writes `NOT_SERVING` without an agent path;
else `SERVING`, and once the far end's first bytes come (a probe sends
none and closes, so it never dials the agent) a connection to that
path, spliced. The path is `daemon::agent_for`, read per stream: the
daemon's own `bridge::host_agent` (the agents clients reported
included; `daemon::Options::host_agent` in tests) while this host's
`machines.toml` has an entry named the peer with `agent` on, else
none. So `machine set <K> agent on|off` binds or unbinds K's socket
within about `PROBE`, on the live link.

### Worker policy

What a config under a fleet root may ask of the host beyond containers
that see only what was lent (`src/worker_policy.rs`). Denied by default;
every item has an allowance in `member.toml`'s `[allow]`, and none of it
applies under `trust = "full"` ([Trust](#trust)).

- **Under the policy by path.** A config dir whose canonical path is
  under `<data>/devsandbox/fleet/` (`fleet_root_of`: symlinks count where
  they point) is under the policy of its fleet root, the first component
  below `fleet/`, whatever process runs it: a peer's verb
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
  `refused by this machine's worker policy: privileged (allow with
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
left out of the file `member::update` writes. `binds` entries must be
absolute (`Allow::check`, part of `member::check`).
