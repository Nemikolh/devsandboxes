# Fleet

Machines as one pool of capacity (the plan:
[fleet](../../plans/active/fleet.md)). What exists so far is the link
primitive the fleet's links are built on (the machines'
[agent relay](machines.md#agent) runs on it too) and both sides of a
worker's link: on the hub the offer, the offered registry entry and its
dial, and [hub mode](#hub-mode); on the worker `lend.toml`, the lending thread, the lent API, the
worker policy and `lend.*` with its CLI, `devsandbox lend` and
`devsandbox fleet join|leave`
([ADR 0041](../../decisions/0041-fleet-workers-dial-their-hub.md)); and
every daemon's [capacity report](#capacity), carried by its snapshots;
and the link's [`TRANSFER`](#transfer) streams, both ways, with no
kind received yet; and [versions](#versions): a worker follows its hub's
published build
([ADR 0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md)).

Code: `src/serve/fleet/mod.rs` (`LINK`), `src/serve/fleet/link.rs`
(`Spec`, `opener`, `opener_with`, `answerer`, `answerer_with`,
`OpenerTransfer`, `AnswererTransfer`, `Outlet`, `End`, `write_sentinel`,
`match_sentinel`), `src/serve/fleet/transfer.rs` (`Header`, `Received`,
`send`, `receive`, `Receivers`, `parse_header`, `check_header`,
`check_size`, `encode_reply`, `parse_reply`, `digest`),
`src/serve/fleet/offer.rs` (`call`, `Ctx`, `accept`,
`dial`), `src/commands/fleet_link.rs` (`fleet_link`, `run`),
`src/serve/endpoint.rs` (`fleet_socket`, `fleet_transfer_socket`,
`accept_any`), `src/daemon_config.rs`
(`ServeConfig::hub`), `src/serve/machines/service.rs` (`Hub`,
`save_loop`, `log_failure`), `src/serve/machines/saved.rs` (`Saved`,
`of`, `machines`, `load`, `save`, `save_wait`), `src/serve/snapshots.rs`
(`Machine::set_link`, `Remotes::restore`), `src/devsbd/proto.rs` (`channel::API`, `caps::API`,
`channel::flows`); the worker side: `src/serve/fleet/lend.rs` (`Lend`,
`load`, `check`, `update`, `fleet_root`, `fleet_dir_name`, `default_name`),
`src/serve/fleet/worker.rs` (`Lending`, `Status`, `link_argv`, `Transport`),
`src/serve/fleet/lender.rs` (`Lender`, `call`, `merge`, `transition`,
`LendInfo`), `src/serve/locked_file.rs` (`update`),
`src/commands/lend.rs` (`show`, `set_state`, `join`, `leave`, `busy`),
`src/serve/fleet/lent.rs` (`Scope`, `decide`, `admit`, `confine_path`,
`filter_answer`, `filter_snapshot`), `src/serve/daemon.rs`
(`start_lending`, `Table`, `Shared::open_lent`), `src/worker_policy.rs`
(`Allow`, `Policy`, `fleet_root_of`, `load_allow`, `enforce`,
`enforce_argv`, `Refusal`); capacity: `src/serve/capacity.rs`
(`Capacity`, `Caps`, `HostFacts`, `EngineCache`, `parse_meminfo`,
`parse_size`, `over_caps`, `over_note`, `summary`, `lend_label`),
`src/runtime/identity.rs` (`EngineInfo`, `docker_engine_info`,
`podman_engine_info`), `Backend::engine_info`, `src/serve/host.rs`
(`poll`, `running_instances`), `src/serve/snapshots.rs`
(`Snapshots::set_capacity`, `Machine::capacity`,
`SnapshotMachine::capacity`), `src/serve/fleet/lend.rs` (`SoftCaps`);
versions: `src/serve/fleet/update.rs` (`HELLO`, `Build`, `decide`,
`Decision`, `release_target`, `asset_url`, `curl_argv`, `update`,
`Steps`, `Release`), `build.rs` (`stamp_build`),
`src/serve/machines/install.rs` (`install_local`, `version_of`),
`src/serve/proto.rs` (`HelloParams::force`), `src/serve/client.rs`
(`hello_with`), `src/serve/endpoint.rs` (`peer_is_own_user`),
`src/serve/daemon.rs` (`hello`).

## Link

A link is one byte pipe between two hosts (an ssh's stdin/stdout in
production) carrying the helper's [frame protocol](../helper/protocol.md),
with the roles of a container's link moved onto the two hosts. A connection
to a socket on one host becomes a stream on the mux, answered on the other
host by a connection made there. A `Spec` says what a link carries: its
sentinel line (`\n<sentinel> <version>\n`), its one channel, the cap that
says the far end serves it, and the `Hello` hash (the agent relay's is
informational; on the fleet link it is the build a worker
[follows](#versions)).

- **The opener** (`link::opener`, the in-container daemon's role) gets a
  reader, a writer and a socket path. It binds a temp name next to the path
  (0600) and renames it over the path, so the newest opener for a path
  wins; then it writes the sentinel, exchanges `Hello`s (advertising
  nothing) and requires the peer's `Hello` caps to include the spec's
  cap. Its `on_ready` callback runs next, with the peer's `Hello` (its
  `hash` is the far end's spec `hello`); an error there ends
  the opener with it. Each accepted connection becomes one `Open { channel }` stream
  (`Mux::attach`, ids below 2^31); on a flowing channel
  (`channel::flows`) the stream is flow-controlled. `Open`s from the peer
  are refused, but for [`TRANSFER`](#transfer). It returns `Ok` when the reader ends or the writer fails,
  an error when the keepalive (`mux::KEEPALIVE_INTERVAL` /
  `KEEPALIVE_TIMEOUT`) hears nothing or the listener fails. Exiting
  unlinks the socket only while it is still its own (same device and
  inode), never a newer opener's.
- **The answerer** (`link::answerer`, the host bridge's role) gets a
  reader, a writer and an `open` callback. It skips what the login shell
  printed up to the sentinel (`ssh::skip_lines`, at most `SENTINEL_LIMIT`
  bytes; another sentinel version is a handshake failure saying which side
  to update), exchanges `Hello`s advertising the cap, and answers each
  `Open` on the spec's channel with `open()`'s connection (`None` refuses
  it with `Close`, as is every other channel). `answerer_with`'s
  `on_ready` gets the opener's `Hello`; its `Err` ends the link before
  anything is served. It returns an `End`: the
  handshake failed (with the first line of noise), `on_ready` refused
  (`Refused`, with why), the keepalive ran
  `on_dead` (which must end the reader, e.g. kill the ssh), or the stream
  closed.

Two specs exist:

| spec | sentinel | channel | cap | opener / answerer |
|---|---|---|---|---|
| `machines::agent::SPEC` | `devsandbox-agent 1` | `SSH_AGENT` (legacy) | `caps::SSH_AGENT` | `commands::agent_relay::run` / `machines::agent::serve` |
| `fleet::LINK` | `devsandbox-fleet 1` | `API` (flowing) | `caps::API` | `commands::fleet_link::run` (on the hub) / the worker's lending thread (`fleet::worker`) |

On `fleet::LINK` each connection to the opener's socket is one connection
to whatever daemon socket the answerer's `open` connects, so a
`client::Conn` on the opener's host says its hello to, and calls, a daemon
on the answerer's host.

**The sentinel rule.** An `API` stream starts with the API relay's own
sentinel line, `\n` + `devsandbox-api 1\n` (`relay::write_sentinel`): the
answerer's `open` hands back a daemon connection that writes that line
before the daemon's bytes. So a stream reads exactly like an ssh machine's
relay, and `machines::link::handshake` (which skips to that line, then
says its hello) works on it unchanged.

## Transfer

A link may also carry bulk transfers on flowing `TRANSFER` streams, in
both directions; `fleet::LINK`'s two ends do (`link::opener_with` with an
`OpenerTransfer`, `link::answerer_with` with an `AnswererTransfer`), the
agent relay's don't. Both ends then advertise `caps::TRANSFER` in their
`Hello`s; the opener requires it of the peer, the answerer opens toward
the opener only when the opener advertised it.

**One transfer per stream** (`serve::fleet::transfer`):

1. The sender writes a header line, JSON (`Header`, `encode_header`):
   `{"kind":"root-tar","name":"cfg.tar","size":3,"sha256":"ba78…"}`.
   `kind` (`[a-z0-9-]`, at most 32 bytes: `binary`, `root-tar`, `bundle`)
   picks the receiver; `name` is a file name (no `/`, not `.`/`..`, at most
   255 bytes); `size` the body's length; `sha256` its lowercase hex hash
   (`check_header`). At most `HEADER_MAX` (4 KiB) with its newline.
2. The receiver (`receive`) checks the header and `size` against its
   limit (`check_size`), asks its `decide` where the body goes, creates a
   temp file next to there (`.<file>.<pid>-<n>.part`, 0600, its dir
   created) and answers `ready` (`READY`); any refusal answers
   `error <why>` instead and the sender sends no body.
3. The sender writes exactly `size` raw bytes, then half-closes (`Eof`).
4. The receiver writes the body into the temp file and requires it to
   end at `size` (fewer bytes before the `Eof`, or any byte after, is an
   error) with the header's sha256; then renames it into place.
5. The receiver answers one line, `ok` or `error <why>` (`encode_reply`),
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
\`<kind>\` transfers"). No end registers a kind yet: the worker daemon's
`start_lending` and `commands::fleet_link::run` pass empty receivers.

**Hub → worker.** `fleet-link` binds a second socket next to the link's,
`endpoint::fleet_transfer_socket` (`fleet-<name>.transfer.sock`, bound and
unlinked like `fleet-<name>.sock`, one accept loop over both:
`endpoint::accept_any`); each connection there is one `Open { TRANSFER }`
(an id below 2^31) to the worker, so a hub process connects and calls
`transfer::send` on it. The worker's answerer hands it to its receivers.

**Worker → hub.** The answerer opens with high-bit ids
(`Mux::open_host`, through its `Outlet`, set from the `Hello`s until the
link ends; `Lending::open_transfer` on the worker), and the opener admits
them through `Mux::on_host_open` (`TRANSFER` only) into its receivers. So
the two directions never share an id space.

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
  `version` the worker's `Hello` hash (`<semver> <git> <published|dev>`),
  `root` its `--root`, `draining` its
  `--drain` (the worker's `lend` was `drain` when it dialed). A refused offer ends the link with `offer refused:
  <why>` (on its stderr, which the worker's ssh keeps). It holds that
  daemon connection for the link's life, so a linked worker is a `clients`
  holder of the hub's daemon. `serve::machines::cli::machine_use` refuses
  `--machine` for it.
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
  draining (nothing on the hub acts on that yet).
- **An offered entry** is `[machine.<name>]` with `offered = true`, no
  `target`, and `roots` the worker's fleet root in `machines.toml`
  (`machines::Kind::Offered`,
  [Registry](machines.md#registry-machinestoml)). Its link watches that
  root while a federated watch wants it, as an ssh machine's watches its
  roots (`service.rs` `LinkLoop::watch`), and its rows merge into
  federated snapshots the same way (`snapshots::merge`); the
  [Worker table](#the-worker-table) confines the watch to the fleet root.
- **The dial** (`fleet::offer::dial`, wrapped around whatever reaches ssh
  machines when `daemon::run` builds its `Dial`): for an offered entry, a
  connection to its `fleet-<name>.sock` in the daemon's socket dir as a
  plain `ssh::Duplex::socket`; down, `its fleet link isn't up (…)`. Each
  connection is one `API` stream to the worker's daemon, so the held link
  (`service::Machines`), the quick calls and `forward::verb` (one stream
  per verb, a hang-up the stream's close) run on it as on an ssh machine.
- **What needs ssh** to the machine is refused for an offered one: every
  `machines::ssh` argv goes through `argv_with`, which takes
  `Entry::ssh_target`, an error for an offered entry
  (`machines::offered_refusal`: exec, terminals, forwards and CLI re-runs
  there need fleet step 4). So the CLI's re-runs (`exec`, `logs`,
  `inspect`, `done`, …, `--machine` per-root commands), the dashboard's
  terminal tabs, remote forwards and the agent relay all fail with that
  message; `machines.set` refuses `agent` on for one (`invalid`). The
  dashboard's logs and inspect views go through the `containers.*` quick
  calls and work.

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
placement (plan step 3b) will score against. The host poll
(`host.rs` `poll`) builds one `capacity::Capacity` per tick while a root
is watched (`Capacity::build`, pure) and hands it to the snapshot cache
(`Snapshots::set_capacity`), which puts it top-level in every root's
`instances.snapshot` params it collects next. Nothing is built while
nothing is watched; a new watch's first, disk-depth push has none.

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
  poll's listing has running (`host.rs` `running_instances`; services
  and strays don't count), every instance on the host, the fleet root's
  or not.
- **`lending` and `caps`**: `lend.toml`'s `state` (none without a
  `lend.toml`) and its `[caps]` in bytes (`Caps::of`; none when no cap is
  set), read through the daemon's `Lender::file` on each tick, so a hand
  edit shows on the next poll.

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
([Hub side](#hub-side)); the Worker table's snapshot filter
(`lent::filter_snapshot`) leaves it as is. The dashboard's Machines tab
and `devsandbox machine ls` show it as LEND (`capacity::lend_label`:
`on`, `drain`, `off`, `-`) and CAPACITY (`capacity::summary`: `8 cpu ·
12/32G · 120G free · 3 run`, memory in use / total, then the over-cap
note).

## Worker side

A worker lends itself to one hub, through `lend.toml`; the hub reaches
it only through the link the worker holds, and there only through the
lent API ([ADR 0041](../../decisions/0041-fleet-workers-dial-their-hub.md)).

- **`lend.toml`** (`<data>/devsandbox/lend.toml`, next to `state.toml`,
  `fleet::lend::path`) is the worker's own; the hub never writes it. Keys:
  `state` (`on` | `off` | `drain`; absent is `off`), `hub` (the hub's ssh
  target, as `machine add` takes one), `name` (what this host offers itself
  as; absent is the hostname's first label, `lend::default_name`, which
  must pass `machines::check_name`), an `[allow]` table, the
  [worker policy](#worker-policy)'s allowances, and a `[caps]` table, the
  soft caps of the [capacity report](#capacity) (`lend::SoftCaps`:
  `cpus`, `mem`, `disk`, `instances`, each optional; `mem` and `disk`
  sizes like `16G`, checked by `capacity::parse_size`). Unknown keys are
  ignored. `[allow]` and `[caps]` are edited by hand: `lend.set` changes
  neither. `lend::load`
  reads it (missing is not lending); `lend::check`: lending (`on`/`drain`)
  requires a `hub`, and a `hub` or `name` that's set must be valid. The
  daemon reads it at start (`daemon::start_lending`, `Lender::start`;
  `Options::lend` in tests; a bad file is logged and nothing lends) and is
  its only writer ([Lending: `lend.*`](#lending-lend)): `lend::update`, a
  read-modify-write under the sibling `lend.lock`, written to a temp file
  and renamed (`locked_file::update`, which `machines.toml` and
  `forwards.toml` are written with too).
- **The fleet root** for a hub is `<data>/devsandbox/fleet/<hub>/`
  (`lend::fleet_root`), `<hub>` the target made one plain path component
  (`lend::fleet_dir_name`: the host, `user@host`, plus `_<port>` for a URL's
  port; bytes other than ASCII alphanumerics, `.`, `-`, `_` and `@` become
  `_`, and so does a leading `.`). The daemon creates it when lending starts
  and holds its canonical path. All the hub reaches is under it.
- **The lending thread** (`fleet::worker::Lending`, `serve-lend`) runs
  while the file says `on` or `drain`. It spawns `link_argv`'s `ssh -T -o
  BatchMode=yes [-p N] -- <hub> <devsandbox fleet-link --worker <name>
  --root <fleet root> [--drain]>` (the canonical fleet root; `--drain`
  while `drain`, read at each dial)
  (through the hub's login shell, `ssh::target_argv`, the builder every
  machine argv uses, here without a registry entry) and runs
  `link::answerer_with` of `fleet::LINK` on its stdio, serving nothing
  until its hub's build checks out ([Versions](#versions)). A link that ends (the
  handshake failed, the keepalive heard nothing, ssh exited) is logged
  with ssh's stderr tail and retried after the machine links' backoff
  (`service::backoff`: 1 s doubling to 30 s), reset once a link came up.
  Its `Status` is `Connecting` until the first attempt ends, `Connected`
  once the `Hello`s are through, `Retrying(why)` after a failure (why with
  ssh's stderr tail). Dropping the thread (`lend.set` off or to another
  hub or name, the daemon exiting after its drain: `Lender::stop`) shuts
  the link's socket and kills the ssh. The transport is
  `fleet::worker::Transport` (`ssh::spawn` in production;
  `Options::lend_transport` in tests). A lending daemon never idles out
  (`Lender::lends`, checked each tick): it is what holds the link.
- **Each `API` stream** the hub opens is answered by `Shared::open_lent`: a
  socket pair whose daemon end writes the API relay's sentinel line first
  ([the sentinel rule](#link)) and is then served like any connection
  (`spawn_conn_as`), with the `Worker` table. Its connection id comes from
  the same counter as the listener's.

### Versions

A worker follows its hub's exact build, an older one included
([ADR 0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md)):

- **The build in the `Hello`.** `fleet::LINK`'s `hello` is
  `fleet::update::HELLO`, `<semver> <git> <published|dev>` (`0.8.0
  1a2b3c4d5e6f dev`), from `build.rs`'s `stamp_build`: `DEVSANDBOX_BUILD_GIT`
  is `git rev-parse --short=12 HEAD` of the source tree (`unknown` when git
  or the repo is missing; the build never fails for it), watched through
  `HEAD`, the ref it names and `packed-refs`; `DEVSANDBOX_BUILD_KIND` is
  `published` when the build ran with `DEVSANDBOX_PUBLISHED=1` (meant for
  the release workflow), else `dev`. `Build::parse` reads one back (a
  missing word is `unknown` / dev).
- **The decision** (`update::decide`, pure, over this build and the hub's
  from the answerer's `on_ready`): the same semver, git and kind is
  `Same`, and the link serves; another build that is published is
  `Download { version }`; another dev build is `DevBuild`. Only `Same`
  serves: the others refuse the link (`End::Refused`) before the first
  stream, so the hub's offer registers the worker but its link finds it
  gone.
- **`DevBuild`** stops offering: `lend.get`'s error is `dev build: needs
  fleet step 2c (the hub runs <hub build>, this worker <ours>)`
  (`update::dev_build_reason`), retried at the next attempt.
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
  elsewhere); a `Worker` connection never hands off, forced or not, and a
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
at once, without watching the file. Never on a `Worker` connection
(`lent::decide` denies them). Wire: [`lend.*`](../../reference/api.md#lend).

- **`lend.get`** answers `LendInfo`: the file's `state` and `hub`, the
  `name` offered (the file's, else `lend::default_name` of the hostname;
  `null` when neither is valid), the `fleet_root` (canonical while
  lending), and the link: `off`, `connecting`, `connected` or `retrying`,
  with `error` the `Retrying` reason, or why lending didn't start while
  the file says lend.
- **`lend.set {state?, hub?, name?}`** merges what's given into the file
  (`lender::merge`, pure: `hub` non-empty and a valid target; `name` a
  `machines::check_name`, `""` clears it; lending needs a `hub`) and, when
  it lends, checks the name it would offer, all before writing; then
  applies it under the `Lender`'s lock (`lender::transition`, pure, over
  the running thread's and the wanted `(hub, name)`): nothing running and
  lending starts one (its fleet root created); off stops it; another hub
  or name stops the old one (its ssh killed) and starts a new one; the same
  pair keeps the thread and its link and sets what the next dial offers,
  so `on` ↔ `drain` never drops what's placed here. It answers the
  `LendInfo` after the change. A daemon that's exiting answers `failed`.
- **`drain`** is recorded, shown by `lend.get`, offered (`--drain`) and
  reported as the [capacity](#capacity)'s `lending`; nothing places work yet (fleet plan step 3), so it changes nothing else.

The CLI (`commands::lend`) is a client of these two, through the daemon
(started like any client does), never in-process; `--machine` refuses
both commands (`serve::machines::cli::machine_use`):

- `devsandbox lend` / `lend show`: `lend.get` as a one-row table (`STATE`,
  `HUB`, `NAME`, `LINK`, `FLEET ROOT`), then the error if any.
  `lend on|off|drain`: `lend.set {state}`.
- `devsandbox fleet join <hub> [--name N]`: `lend.set {hub, name?, state:
  on}`.
- `devsandbox fleet leave`: `lend.set {state: drain}`, then
  `instances.list {dir: <fleet root>, depth: listing}` every 2 s until
  `commands::lend::busy` (pure, over its rows) finds no instance whose
  config root is under the fleet root running or being built (a note on
  stderr names them when that set changes), then `lend.set {state: off}`.
  Ctrl-C ends the wait and leaves lending at `drain`. Not lending: it says
  so and changes nothing.

### The Worker table

Every connection has a table (`daemon::Table`): `Full` for local clients,
`Worker(Scope)` for the hub's streams. A `Scope` (`fleet::lent::Scope`) is
the hub, the canonical fleet root and the env its verbs get. On a `Worker`
connection each request goes through `lent::admit` at the top of
`handle`, before any dispatch:

- **`decide`** (pure, over method and params) lets through `hello`,
  `subscribe` / `unsubscribe` with only the `instances` topic,
  `instances.watch` / `unwatch`, the quick calls
  (`machines::forward::QUICK`) and the verbs (`verbs::is_verb`). It refuses
  (`denied`) any request with a `machine` param or `machines: true`
  (nothing federates past the worker), a verb with `all` (`--all` is every
  instance in `state.toml`, not the root's), and every other method:
  `machines.*`, `fleet.*`, `lend.*`, `shutdown`, `helper.call`, `forwards.*`,
  `bridges.ensure`, `roots.list`, the Inbox. Refusals are logged.
- **Paths**: a `root` or `dir` param must be absolute, have no `..`, and
  canonicalize (symlinks resolved) under the fleet root
  (`lent::confine_path`); it is rewritten to the canonical path. A path that
  doesn't exist is `not-found`, except an unwatch's, checked as written.
  `instances.list`, the watches, `sandboxes.*`, `services.get`, `config.*`
  and the verbs must carry theirs.
- **Instances**: an `instance` (exact name or id), `instances.procs`'
  `instances` and the `containers.*` `container` must be an instance whose
  `config_dir` is under the fleet root (`lent::lent_instances`, from
  `state.toml`), else `not-found`. A verb's `instance` is resolved as the
  CLI resolves it (`commands::instance_matches`), and every match must be
  lent; `rebuild` / `start` / `stop` / `rm` must name one.
- **Env**: a verb's `env` is replaced by the scope's, the daemon's own
  process env (`verbs_client::caller_env`, `Options::lend_env` in tests),
  never what the hub sent: the worker-side counterpart of
  `relay::SessionEnv` on an ssh machine.
- **Answers and pushes**: a snapshot lists every instance in `state.toml`,
  whatever its `dir`, so `instances.list` answers and every
  `instances.snapshot` pushed to the connection (`Pending::confine`) are
  cut by `lent::filter_snapshot`: instance rows whose `root` is under the
  fleet root, their names alone in services' `used_by` and in
  `thread_children`. `instances.resolve`'s matches are cut the same way
  (`lent::filter_answer`); none left is `not-found`.
- **Hello** on a `Worker` connection never hands the daemon off, whatever
  version it says and even with `force`: only a local client's newer
  binary (or this user's forced hello, [Versions](#versions)) replaces it.

### Worker policy

What a config under a fleet root may ask of the host beyond containers
that see only what was lent (`src/worker_policy.rs`). Denied by default;
every item has an allowance in `lend.toml`'s `[allow]`.

- **Under the policy by path.** A config dir whose canonical path is
  under `<data>/devsandbox/fleet/` (`fleet_root_of`: symlinks count where
  they point) is under the policy of its fleet root, the first component
  below `fleet/`, whatever process runs it: a lending daemon's verb
  subprocess or a local CLI pointed at the dir. No flag carries it: each
  check point calls `Policy::for_dir` (the data dir from `$XDG_DATA_HOME` /
  `$HOME`, as `state.toml`'s) itself, and reads `[allow]` off `lend.toml`
  next to `state.toml` (`load_allow`: that table only, so it is read
  without `serve`; one that doesn't parse fails the verb).
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
  privileged = true under [allow] in lend.toml)`; an item no allowance
  covers ends `(no lend.toml allowance permits it)`.

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
