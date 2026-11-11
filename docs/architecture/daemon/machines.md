# Machines: remote hosts over ssh

A machine is a remote host running devsandbox: registered with `devsandbox
machine add`, one that registered this host from its end (an
[inbound neighbor](#inbound-neighbors)), or a fleet member this host has
no link to, reached through others (a [routed machine](#routed-machines)).
The local daemon holds one link to each registered or inbound machine
(for an ssh machine `ssh <target> devsandbox fleet-link --peer <this
host>`, a [peer link](fleet.md#peer-links); for an inbound one the peer
link it dialed), talks to the machine's daemon over its `API` streams (a
routed machine's over [routed streams](fleet.md#routed-streams)), merges
its rows into watched snapshots, and forwards requests for its
instances. Read this before touching anything that takes a `machine`
param or a `m/name`.

Code: `src/serve/machines/mod.rs` (`Entry`, `Kind`, `MachineKind`,
`RawEntry`, `path`, `parse`, `update`, `inbound.rs` (`Neighbors`, `Guard`),
`check_name`, `Where`, `parse_name`), `join.rs` (`plan`, `Plan`, `Fleet`,
`Joined`, `logs`, `Logs`), `leave.rs` (`plan`, `Plan`, `Here`, `rm`, `fleet_call`,
`After`, `apply`, `RmAnswer`, `Removed`, `Left`), `changes.rs`
(`revoke`, `rotate`, `propose`, `push`, `call`, `Signed`, `Proposed`,
`Rotation`), `ssh.rs`
(`probe_argv`, `script_argv`, `target_argv`,
`parse_target`, `Dial`, `RouteDial`, `Duplex`, `probe_dial`), `link.rs`
(`handshake`, `Over`, `LinkError`, `client_name`), `service.rs` (`Machines`, `Handle`, `Hold`,
`Held`, `Timing`, `Hooks`, `Hub`), `saved.rs` (`remotes.json`), `api.rs`
(`machines.*`, `Ctx`, `introducer_of`), `forward.rs` (`QUICK`, `FANOUT_WAIT`, `verb`, the
federated joins), `cli.rs` (`machine_names`, `plan`, `Plan`, `machine_use`, `route`,
`localize`, `Reconnect`, `dials`, `pipe_mode`, `attach_mode`), `cli/calls.rs` (`use_of`, `Use`,
`Refused`, `refusal`, `named`, `per_root`, `config_roots`), `install.rs` (`CHECK`, `INSTALL`, `check`, `install`,
`parse_check`, `decide`, `platform_matches`), `src/serve/snapshots.rs` (`Remotes`, `LinkState`, `merge`,
`fleet_view`, `routed`, `Routed`), `src/serve/fleet/grants.rs` (`granted`,
`granted_names`, `grant`, `withdraw`),
`src/serve/fleet/dialer.rs` (`Dialer`, `dial`, `routed`, `link_args`),
`src/serve/fleet/route.rs` (`connect`), `src/serve/fleet/routes.rs` (`reach`),
`src/serve/relay.rs` (`--sentinel`), `src/commands/machine.rs`,
`src/serve/endpoint.rs` (`fleet_socket`, `link_agent_socket`, `agent_sockets`),
`src/tui/remote.rs`. Params: [api.md](../../reference/api.md#machines).

## Registry: `machines.toml`

`<data-dir>/devsandbox/machines.toml`, `[machine.<name>]` (read and
written through `RawEntry`): `target` (`me@host`, an ssh-config alias, or
`ssh://[user@]host[:port]`), the machine this host sshes to
(`Kind::Ssh`); an entry an older build wrote with `offered = true` fails
the file, the error naming it (`machines::parse`: delete it by hand).
`Kind::Inbound` is the other kind, never in the file: the service's
entry for an [inbound neighbor](#inbound-neighbors) (`Entry::inbound`).
No roots: a machine's are what its own daemon lists
([Live roots](#held-links-machines); `Entry::roots` holds them in
memory, never written, and an older build's `roots` key is ignored and
dropped by the next write). Then an optional
`command` (remote devsandbox as shell text, default `devsandbox`; no
`machine add` flag: `machine add` sets it to `~/.local/bin/devsandbox`
when it installs there off the login `PATH` (see
[`machine add`](#devsandbox-machine-addlssetunlinkrmhubpendingapprovedenykeytrustroots)) through `machines.add`'s
`command` param, any other location is set by hand; `machines.probe`
dials with the same param). An older build's `agent` key is ignored:
whom this host's ssh-agent reaches is `member.toml`'s grants
([Agent](#agent)). Only the daemon writes it
(`machines.add`/`set`/`unlink`/`rm`, and `fleet.leave`/`fleet.forget`
from the hub, [fleet.md](fleet.md), *How a machine leaves*): locked read-modify-write on `machines.lock`, temp
file + rename (`serve::locked_file::update`), since two daemons overlap during a handoff. A file that
doesn't parse is logged and changes nothing. The CLI reads it only to know
which machines exist (`cli::machine_names`), adding the inbound ones a
running daemon lists (`machines.list`, without a lazy start) only when the
socket dir holds a `fleet-*.sock` none of the registered machines' links
binds (`cli::foreign_link_sockets`), so with no machine of either kind every
command resolves exactly as without machines and never contacts a daemon it
didn't need.

Names: `[A-Za-z0-9._-]+`, not starting with `.` or `-` (`check_name`).
A machine's registry name is its member name (fleet plan decision 35):
without `--name`, `machines.add` takes it from the machine's `member.get`;
`--name` renames the machine only while it joins a fleet by that add
([`machine add`](#devsandbox-machine-addlssetunlinkrmhubpendingapprovedenykeytrustroots)). `parse_name` splits a CLI
`<name>`: `m/x` → `Where::Machine`, `@/x` → `Where::Local`, bare →
`Where::Any`. Container names can't hold `/` or `@`, so this never
collides with an instance name.

## Inbound neighbors

A machine whose link to this host was dialed from its end (`machine add
<this host>` run there, a [peer link](fleet.md#peer-links)) is an inbound
neighbor here while that link is up: never in `machines.toml`, memory
only (`machines::inbound::Neighbors`, shared by `machines::api::Ctx`, the
`Machines` service and the forwards registry). Its `fleet-link --peer
<name>` announces it (`links.inbound`) on a connection it holds; the
neighbor lasts while that connection does (`Guard`, the newest announce
of a name winning, so an older connection's close never removes a newer
one's). Its roots are its link's listing, as any machine's (its own
config roots and this host's fleet root there, the `roots.list` the held
link asks at each link-up; empty until then).

An inbound neighbor is a machine like a registered one (fleet plan
decision 28), of kind `inbound` (`Kind::Inbound`, `MachineKind::Inbound`,
`Entry::inbound`): the service holds a link to it
([Held links](#held-links-machines)), so it has rows in federated
snapshots and the dashboard, quick calls and verbs with `machine: <name>`
and qualified names `<name>/x` reach it ([Forwarded
requests](#forwarded-requests)), the CLI's re-runs, terminal tabs and
forwards go over its `fleet-<name>.sock` (which its `fleet-link --peer`
binds here), and placement may pick it
([fleet](fleet.md#placement)). Every stream this host opens on it is
served by its daemon under its trust table for this host (its registry
entry's name for this host is the peer there, its fleet root
`fleet/<that name>/`). `machines.list` lists the neighbors after the
registry's entries, `machines.get` answers them: no `target` or
`command`, `agent` this host's grant for it ([Agent](#agent)), `state`,
`error`, `version` and `capacity` its held link's. `machines.set
{agent}` records a grant for it by its key like any machine's (fleet
plan decision 67); on its link this host is the dialed end, so the
neighbor's daemon probes this host over its own `fleet-<this host>.sock`
and binds `agent-route-<this host>.sock` while it finds the grant in
this host's link report ([Agent](#agent)). `machines.unlink` refuses it (it goes
with its link: `devsandbox machine unlink <this host>` run on it).
`machines.rm` takes it out of the fleet like any machine
([fleet.md](fleet.md), *How a machine leaves*): there, or once its `fleet.leave`
or `fleet.forget` came, this host ends its link (`Ctx::end_inbound`,
`daemon::Shared::end_inbound`: `links.cycle {why}` on its `fleet-link`'s
held `links.inbound` connection, which hangs up). When the neighbor goes, so does the
machine: its link is stopped and its rows dropped (not kept as
`unreachable`: nothing here could reach it again), and its forwards go
(`forwards::Known`). `devsandbox machine ls` shows `inbound` as the
TARGET. A name is a registry entry's or a neighbor's, never both:
`links.inbound` refuses a name `machines.toml` has (any kind),
`machines.add` a live neighbor's (`inbound::taken`);
should one race the other, the registry's entry is the machine
(`service::with_inbound`).

## ssh

ssh is the link's transport and the add-time path, nothing else. The
machine's link is dialed by the daemon through
`fleet::transport::Transport::ssh` (`ssh -T -o BatchMode=yes [-p N] --
<host> <command> fleet-link --peer <member>`, the entry's `command` as the
program: `Transport::dial_with`, over `ssh::target_argv`;
[Held links](#held-links-machines)). Every use of a machine after that
(the CLI's commands, the dashboard's terminals, forwards and `port`) goes
over the link's channels. Before a link exists, `machine add` reaches the
target over ssh with the argvs in `ssh.rs`:

- `probe_argv`: `ssh -T -o BatchMode=yes [-p N] -- <host> … api --stdio
  --client <c> --sentinel` (the daemon has no TTY: ssh must fail, not
  prompt), what `machines.probe` and `machines.add` dial (`probe_dial`)
  for the hello and `member.get`. The relay there is a local client of
  that daemon, under the user's own ssh login; `--sentinel` makes it write
  its sentinel line first ([Link handshake](#link-handshake)).
- `script_argv`: `ssh -T [-p N] -- <host> sh -c <script> sh <args…>`, a
  POSIX script run for a person at the CLI ([Install](#install)); no
  `BatchMode`. A script is one line: the login shell may be csh, which
  refuses a newline inside quotes.

Each goes through `argv_with`, which takes `Entry::ssh_target`, so none
is built for an inbound machine. Plain ssh, no ControlMaster of its own:
`~/.ssh/config` decides. The remote command runs through the login shell
(`exec "$SHELL" -lc '…'`, each arg quoted for it), so the remote `PATH` is
the user's. No argv passes `-A`: this host's ssh-agent reaches a machine
only over `AGENT` streams ([Agent](#agent)), only while this host grants
it.

`Tail` (a bounded ring of a stream's last bytes, `Tail::collect`) keeps
ssh's stderr for `spawn`'s `Peer`.

`Dial` (`Fn(&Entry) -> Duplex`) is injectable. The daemon's is
`fleet::dialer::dial`: a connection to the machine's `fleet-<name>.sock`
in the socket dir, one `API` stream on its link, ssh or inbound alike,
never a route (it is the link's state, which this host's link report
says); `machines::api::Ctx::dial` (the held link, forwarded verbs,
placement's capacity asks) is that one, `Ctx::probe` the add-time
`probe_dial` (`probe_argv`, `link::client_name` as the client).
`RouteDial` (`Fn(&str) -> Duplex`, the daemon's `fleet::dialer::routed`)
is a member's `API` stream through `fleet::route::connect`, what a
[routed machine](#routed-machines) is dialed with. A `Duplex` is always a
unix socket (pumped to the ssh child's pipes for `probe_dial`), so
`client::Conn` works on it unchanged and tests dial a second test
daemon's socket.

Every stream of a machine's link reaches the machine's daemon through
`link.attach {peer: <this host's member name>, channel, key,
introduced_by}` (the key and introducer this host proved on the link),
which its
`fleet-link --peer` sends first ([fleet](fleet.md#peer-links)); a
[routed stream](fleet.md#routed-streams)'s destination serves it for
its proven origin, this host, the same way but with no agent. Either way
the remote daemon serves it with the machine's trust table
([fleet](fleet.md#the-trust-table)): under its `trust = "worker"` (the
default) this host reaches the machine's own roots and its fleet root
for this host (`fleet/<this host>/` there) only (its own roots being
those its instances name, plus its `member.toml` `roots`, which `machine
roots add` run there sets), never `machines.*`, `member.*`, `shutdown` or an
edit of those configs (`fleet.*` goes by who the caller is, whatever the
trust: [fleet](fleet.md#the-trust-table)); under `full`, everything but `member.*`,
`membership.*`, `graph.*`, `link.*` and `links.*`. A verb this way runs with the machine daemon's
own env (the peer's `env`, `fleet::lent::Peer`), not a login shell's,
over a direct link its `SSH_AUTH_SOCK` this host's agent relayed there
(`agent-link-<this host>.sock`, the peer's `agent`) while that socket
answers, so host git on the machine uses the agent this host serves.
No use of a machine bypasses the trust table: the only ssh shells on it
are the add-time ones (`probe_argv`'s probe, `script_argv`'s install),
each the user's own login there.

## Install

`src/serve/machines/install.rs` checks which devsandbox a machine runs and
copies this binary there, for `machine add`
([`devsandbox machine …`](#devsandbox-machine-addlssetunlinkrmhubpendingapprovedenykeytrustroots)). The
CLI runs it, not the daemon: the daemon's ssh
is `BatchMode` with nobody to ask, and the bytes are the CLI's own binary.
Both scripts go over `script_argv` and print their report after a marker
line, past whatever the login shell printed first.

- `CHECK` (`check`): `$1` is the program (empty: `devsandbox`), resolved
  with `command -v`; empty and not on the login `PATH`, it falls back to
  `$HOME/.local/bin/devsandbox` (`Remote::off_path`), since a login shell
  often misses that dir (added in `.bashrc`). It reports `uname -s`,
  `uname -m`, the path and the output of its `--version`. `parse_check` makes it a `Remote`, whose
  `version` is `None` when `--version` fails (a build from before it
  existed).
- `platform_matches`: `uname` against this binary's
  `std::env::consts::{OS, ARCH}` (`Linux`↔`linux`, `Darwin`↔`macos`;
  `x86_64`|`amd64`↔`x86_64`, `aarch64`|`arm64`↔`aarch64`); copying
  itself can't work anywhere else.
- `decide` (pure): the same semver is `Plan::Nothing`; a devsandbox found
  with another or no version is updated in place (`Dest::Found`); a
  missing one goes to `$HOME/.local/bin/devsandbox` (`Dest::Default`).
- `INSTALL` (`install`): `$1` is the destination (empty: the default).
  ssh's stdin, the binary, goes into a `mktemp` file next to it,
  `chmod 755`, then its `--version` must run (a binary linked against a
  libc the machine lacks fails here), then `mv -f` over the destination:
  atomic, no "text file busy" over a running binary. Any failure removes
  the temp file and leaves the destination as it was. It prints the
  installed path, expanded.

A failed ssh or script is an error carrying the tail of its stderr.

## Link handshake

`link.rs`: the hello says version `0.0.0`, build 0, so a link never hands
off the remote daemon (only that host's own clients upgrade it). The
`protocol` must match (`LinkError`); differing semvers within one protocol
are a warning at add and shown in `machine ls`. Who the machine is was
settled below this, by the peer link's [key proof](fleet.md#key-proof);
the add-time probe has no link and checks the confirmed key instead.

`handshake` takes what the stream is (`Over`). A link's `API` stream
(`Over::Link`: the held link, forwarded verbs, capacity asks) is the
remote daemon from its first byte: no
login shell in a mux stream, so no sentinel (fleet plan decision 27).
The add-time probe (`Over::Ssh`) reads up to the relay's sentinel first:
`api --stdio --sentinel` writes `\n` + `devsandbox-api 1\n`
(`relay::write_sentinel`) once it reaches the remote daemon, and the
probe skips every line before it (`ssh::skip_lines`, byte at a time, so
nothing past the sentinel is consumed), bounded at 64 KiB, with the
hello's read timeout. The leading newline ends a motd printed without a
final one, so the sentinel always starts a line and is matched as a whole
line (a trailing `\r` tolerated); another version is an error naming the
older side. The first skipped line (cut at 200 chars) is kept on
`Link::noise`. EOF before the sentinel (or the hello's reply) is
`LinkError::Closed` with ssh's stderr tail; the bound or a timeout is
`LinkError::Other` with that first line.

## Held links: `Machines`

The daemon runs a `Machines` service for its whole life: one
`serve-machine-<name>` thread per registered machine and per inbound
neighbor up now, plus, for an ssh machine, a `serve-machine-<name>-link`
thread carrying its link, started, stopped or restarted when `add`/`set`/`rm`
change the registry or a neighbor comes or goes
(`Handle::reload`, which the neighbors' hook, `Neighbors::on_change`,
calls; one reload at a time; a change to a machine's entry restarts its
link, and a reload leaves a kept machine's roots and rows in `Remotes`
as its link left them, `Remotes::sync`).
Agent grants aren't the registry's: a grant change reloads nothing.
`Handle::entries` is both sources (`service::with_inbound`). Every link is
held from the start, wanted or not (fleet plan decision 7), an inbound
one's while its neighbor is up.

- **The carrier.** An ssh machine's link thread runs the service's `Hold`
  again and again, the backoff (1 s doubling to 30 s, reset once a carrier
  came up) between: `fleet::dialer::Dialer::hold`, which dials
  `fleet-link --peer <this host's member name>` there (`member.toml`'s
  `name`, else the hostname's first label; `fleet::dialer::link_args`)
  through the daemon's transport and runs the answerer of the fleet link
  ([fleet](fleet.md#peer-links)). It binds `fleet-<name>.sock` and its
  `.pty`, `.tunnel`, `.transfer` siblings in the socket dir (newest binder
  wins, so a successor daemon takes them over), each accept one stream
  to the machine; each stream the machine opens is this daemon's
  (`daemon::machine_dialer`: `Shared::open_lent`, peer `<name>`, its
  fleet root `fleet/<name>/` under the data dir). It reports through
  `Held`: `up` once the `Hello`s are through (the link's sentinel and
  the `Hello`s within `Timing::handshake`), `note` for a link up but serving
  nothing yet (waiting for the hub's build), and returns why it ended,
  with ssh's stderr tail (where the far end's refusal is, such as a name
  clash: [inbound neighbors](#inbound-neighbors) there). A hub machine on
  another build is followed first ([fleet](fleet.md#versions)). An
  inbound machine's carrier is the `fleet-link --peer <name>` that
  announced it: no thread of its own (`service::carried`).
- **The link.** Once the carrier is up (always, for an inbound machine)
  the machine thread dials `fleet-<name>.sock` (`Dial`) and says hello,
  subscribes to `instances`, asks its `roots.list`, watches each of the
  machine's roots while a
  local watcher wants machines (`Hooks::watching`), and stores pushed
  snapshots in `Remotes` (kept while the link is down; in memory only,
  but on a hub, which also saves them to `remotes.json`). The machine
  that is this host's hub (`Hooks::hub`: `member.toml`'s `hub` names
  another member, `daemon::Shared::hub_of`; asked at each look at the
  watchers, and a hub change redials every link anyway) is also watched
  federated meanwhile: the link's last `roots.list` gives its fleet root
  for this host (the `fleet` row, there whether or not the hub has a
  config root of its own), watched with `machines: true`
  (`LinkLoop::watch`, `Want`/`Watched`; a refusal is the machine's
  `error`, asked again at the next change). That push is split
  (`snapshots::fleet_view`): its rows tagged `machine` and its `machines`
  array, this host's own name left out (guarded: the hub leaves it out
  already) and entries the hub itself sees `via` another dropped (the
  machines the hub links to), are the hub's `Machine::fleet`, whose
  machines are [routed](#routed-machines) here; the rest is that root's
  own rows when it is one of the entry's roots. A machine that stops being
  the hub loses its view. A carrier going
  down ends the link at once and is the machine's `error`; a carrier
  coming up is dialed at once; a connect that fails or a link lost over a
  live carrier is retried with the backoff.
- **Live roots.** A machine's roots are what its last `roots.list`
  over the link answered, in that order (sorted: `serve::api::roots_list`;
  its state.toml `config_dir`s, `~/.devsandboxes`, its signed copies,
  its `member.toml` `roots` and this host's fleet root there), nothing
  this host picked. A listed row with an `error` (a root recorded there
  whose directory is gone) is left out unless it's the `fleet` row, so
  it's never watched and refused. A listing goes into `Remotes`
  (`Machine::roots`; a root no longer listed loses its rows), with its
  default root (`Machine::default_root`, below) and `Machine::listed`
  set, under the
  links lock that `Handle::reload` syncs `Remotes` under
  (`LinkLoop::apply_roots`). Before a machine's link first lists, its
  roots are none, or on a hub the ones `remotes.json` restored with its
  rows and default (`saved::SavedMachine::roots`, `default_root`), kept
  until it lists.
- **Default root.** The root a command on a machine with no root given
  takes (`machines::default_root`, pure over the listing's rows): the
  row `roots.list` flags `copy` (a signed config's copy, `mesh::copy_of`)
  when there's one, of several the one named `.devsandboxes`; else the
  row flagged `home` (its `~/.devsandboxes`); else its only root; else
  none. A routed machine has no listing here: its only root as the hub's
  view has it, else none. `machines.get` answers it as `default_root`,
  with `listed`; `snapshots::merge` takes an instance whose root isn't
  listed from the default root's snapshot (none: the first root's).
  `Handle::entries`, `machines.list` and `machines.get` answer them, so
  `forward::sandboxes_everywhere`, `forward::resolve_everywhere`,
  `verbs_client::run_target`'s `sandboxes.list` and the watch above all
  see a root the machine got later (a [signed config copy](fleet.md) it
  installed since, under `mesh/tree/`) with nothing to do here, and
  `Remotes` stores its pushes. The link lists at each link-up and again on `Handle::relist`:
  after each `fleet.sync` this host's `changes::push` got through to the
  machine, and each one that came from it (`changes::sync`), since a
  member installs a moved config before answering a sync and sends the
  log on only after installing. They are names to ask about only: the
  machine's trust table decides every call on them (a `sandboxes.list`
  or watch of a root this host may not reach answers as the table says),
  so listing a root reaches nothing the table didn't already admit.

`LinkState`: `idle` (before the first attempt), `connecting`,
`connected`, `unreachable` (with the reason). A failure is logged when it
differs from the last one logged since the link (or carrier) was up, so
a machine that's away logs once. Each registered machine's link holds
the local daemon up (`idle::Holders::links`). On the machine it's an [inbound neighbor](#inbound-neighbors)
(`links.inbound`), its streams clients like any other, and the machine
holds a link back over it to this host.

The socket dir holds one `fleet-<name>.sock` per link up (or coming
up), bound by the link's local end, and the daemon's own
`route-<member>.sock` for each member of its link graph it has no link
up to, which opens routed streams to that member over the graph's paths
([fleet](fleet.md#routed-streams), *The requester*). A link coming up
unbinds that member's route socket, a link going down binds it again.
`route::connect` tries the link's socket first, then the route socket;
the held link's own dial (`Dial`) only ever uses the link's socket. The
CLI's look for inbound and routed machines (`cli::foreign_link_sockets`)
asks the daemon's `machines.list` when a `fleet-*.sock` no registered
machine binds, or any `route-*.sock`, is there.

## Routed machines

A routed machine (`MachineKind::Routed`, wire `routed`; fleet plan
decision 60) is a fleet member this host has no link of its own to,
reached through other members (`snapshots::routed`, pure, over the
daemon's `Shared::routed_reach`, the service's `Hooks::routed`): each
member it binds a route socket for that its link graph has a path to,
plus each machine its hub's federated view lists, neither a machine
here. It has no registry entry, no `serve-machine-*` thread and no watch:
its rows are the hub's view's. `Handle::routed` lists them
(`snapshots::Routed`): `via` the first hop of the first graph path whose
first hop is a link up here (`fleet::routes::reach`), else the hub whose
view lists it; `state` `connected` while there is such a path, else
`unreachable` with why (a path that's there may still fail past its
first hop: the call says so); `roots` the `root`s of its sandboxes rows
in the hub's view, `version` and `capacity` the view's. `machines.list`
and `machine ls` list them all; the federated snapshot (and so the
dashboard) has only those the hub's view lists, `via` the hub
([Federated snapshots](#federated-snapshots)).

Everything that acts on a machine works on a routed one the same way,
over `RouteDial` where a direct machine uses its link: forwarded quick
calls (`Handle::call_routed`: a stream of its own per call, hello within
`Timing::handshake`, the call within `Timing::call`), verbs
(`Handle::dial`), the federated calls, placement and code moves
([fleet](fleet.md#in-the-run-verb)), terminals and forwards (every kind
of machine's on `route::connect`). Its daemon serves them under its trust for this host
([fleet](fleet.md#routed-streams), *The destination*). `machines.set
{agent}` and the dashboard's agent toggle record and withdraw a grant
for it by its key in this host's membership log, as for any machine
([Agent](#agent)); the grant is in this host's link report, and the
machine's routed `AGENT` streams carry this host's agent there.

## Forwarded requests

A request carrying `machine` (neither registered, an inbound neighbor
up now nor [routed](#routed-machines): `not-found`, `Handle::unknown`)
has it stripped and goes to that machine, but for the ones served here:

- **Quick calls** (`forward::QUICK`: `instances.list|cancel|procs|signal|
  rename|setProfile|done|buildLog|resolve`, `containers.logs|inspect`,
  `sandboxes.list|get`, `services.get`) go over the held link, one in
  flight per machine, `Timing::call` (15 s) each. A timeout drops the link
  (its late answer would be read as the next call's), not its carrier; a
  call arriving during the backoff retries the connect at once, and calls
  queued when a connect fails or while the carrier is down get `failed`
  (`<machine>: unreachable: …`; during the carrier's first attempt they
  wait for it). A routed machine's go over a stream of their own each
  ([Routed machines](#routed-machines)). The remote's answer, error code
  and data included, is the answer. A `done` (`instances.done` with
  `done: true`) of an instance this host placed there is fetched back
  first, best effort, a failure the answer's `warning`
  ([fleet](fleet.md#moving-code), *Fetch-back*: `serve::daemon`'s
  `remote_done`).
- **Verbs** get a fresh `API` stream per request (`Handle::dial`: the
  link's `Dial`, a routed machine's `RouteDial`), so a build
  never holds up the link. The requester's `SSH_AUTH_SOCK` (an absolute
  path in `env`) is reported to this daemon (`bridge::report_agent`), so
  the machine's `AGENT` streams reach the live agent of whoever asked even
  when the daemon started without one; the rest of `env` gives way to the
  machine daemon's own. A link that's down, or no route that opens, fails
  it at once (`<machine>: unreachable: <why>`). `output`/`progress` are relayed with
  `request` rewritten to the local id. While its stream is open it's a
  `verbs` holder here. A hang-up, or a local drain, closes the stream,
  which the remote sees as a hang-up: cancelled there unless `detach`.
  An `rm` of an instance this host placed there is fetched back first,
  or refused unforwarded when that fails; `force` skips it
  ([fleet](fleet.md#moving-code), *Fetch-back*: `serve::daemon`'s
  `remote_rm`).
- **Federated** `instances.resolve` / `sandboxes.list` with `machines:
  true` ask this host and every machine (routed ones too, at their roots
  as the hub's view has them) in parallel, each machine given
  `FANOUT_WAIT` (5 s). Remote matches come back qualified (`box/web-2`)
  with an additive `found: [{name, machine, root}]`; a machine that doesn't
  answer adds nothing, and an empty result names the unreachable ones.
- **Served here** (`forward::HERE`: `forwards.add`, `placements.fetch`;
  the latter's `machine` names the placement, see
  [fleet](fleet.md#moving-code)): for `forwards.add`, `machine` names where
  the forward reaches, not where it runs. The daemon keeps the param and
  answers itself, binding the forward on this host with one `TUNNEL`
  stream per bridge over the machine's link, or a route, whatever its
  kind ([forwards.md](forwards.md#ad-hoc-forwards-and-forwardstoml)).

- **`machines.get {name, agent?}`** takes the caller's absolute
  `SSH_AUTH_SOCK` as `agent` and reports it like a verb's: the CLI's
  re-runs pass it ([The CLI](#the-cli-clirs)). A routed machine answers
  kind `routed`, no `target`, its view roots (the agent reported as for
  any machine); the CLI takes it like an inbound one
  (`Kind::from_wire`: no ssh target, reached over the sockets). A tab's
  `--reserve-255` resolve takes `denied` as final, not as an outage
  (`cli::resolve_lost`).

## Federated snapshots

`instances.watch {dir, machines: true}` (the dashboard always sends it)
gets every push merged with each machine's rows (`snapshots::merge`): rows
gain `machine` and keep their remote `root`, and a top-level `machines`
array carries each machine's `name`, `kind` (`ssh` |
`inbound` | `routed`, fleet plan decisions 29, 60), `target` (`null` for an inbound one), `state`,
`error`, `version`, `command`,
`runtime`, `agent` (this host's grant, [Agent](#agent)), `capacity` (its host's
[capacity report](fleet.md#capacity), from its last push), `via`,
`exposed_by` ([exposure](fleet.md#exposure)). A machine's push or state change re-publishes those
watchers' roots from the last local params. Details:
[host-poll.md](host-poll.md#snapshots).

On a fleet member, `merge` also lifts the hub's `Machine::fleet` view:
each machine it lists becomes an entry of its own after the direct ones,
kind `routed`, `via` the hub, with no `target` or `command` (no link of
its own here reaches it), its `agent` this host's grant (not the hub's),
its rows as the hub tagged them; while the hub's link isn't
connected it takes the hub's state, `error` `through <hub>: <why>`, rows
last known. A machine this host links to wins over the same name seen
via the hub. The view lives in `Remotes` only (never in `remotes.json`:
`saved::of` takes rows and versions), and isn't a machine of the
service's: its machines are [routed](#routed-machines)
(`Remotes::routed`), which is how `machines.list`, placement, the
federated calls and forwarded requests reach them.

On a hub, a link peer's federated watch (a member watching it,
[fleet](fleet.md#hub-mode)) is merged with `merge`'s `except` the peer's
name: its own rows and entry never go back to it. `publish_snapshot` and
`publish_machines` merge once per root and asking peer
(`Pending::except`: `None` for local clients, which share one merge).

## Agent

This host's ssh-agent reaches a machine of any kind as `AGENT` streams
the machine opens (fleet plan decisions 14, 18, 66 and 67; no ssh of its
own, no `-A`), probing this host every `fleet::agent::PROBE` (2 s) and
binding a socket in its socket dir only while this host serves
([fleet](fleet.md#agent)); each connection there becomes one `AGENT`
stream:

- a machine this host dials (registered here): its `fleet-link --peer
  <this host>` binds `agent-link-<this host>.sock`
  (`endpoint::link_agent_socket`) over the link;
- an inbound or routed machine: its daemon binds
  `agent-route-<this host>.sock` (`endpoint::route_agent_socket`,
  `fleet::agent_routes`), probing only while this host's link report
  lists it among the grantees, over its link to this host
  (`fleet-<this host>.sock`, which this host's `fleet-link --peer`
  serves `AGENT` on) or a routed stream, end to end with this host.

This daemon answers each stream (`daemon::agent_for`,
`fleet::agent::answer`) with its agent (`bridge::host_agent`: the agents
clients reported first) while `member.toml` grants it to the key the
machine proved (on the link, or as the routed stream's origin; never a
relay's), read per stream, else with `NOT_SERVING` and the close. So
withdrawing the grant refuses the next stream at once, and the machine
unbinds its socket at its next probe; the link stays up either way.

On the machine nothing registers the socket: `bridge::host_agent` tries
the live `agent-*.sock` files in the socket dir (`endpoint::agent_sockets`,
newest mtime first) after the agents clients reported and before the
process's own `$SSH_AUTH_SOCK`, so the daemon's bridges and any command
run there (a verb, an `exec`'s `PTY` stream, a terminal tab) find it, and a link that's gone
drops out by the liveness check.

**Grants** (fleet plan decisions 65-67) are this host's consent, per
member, kept in `member.toml` as `[[agent]]` tables of `name` and `key`
(`fleet::member::Grant`, none by default, one per name), written by the
daemon only (`fleet::grants::grant` / `withdraw`, locked and atomic like
every `member.toml` write). A grant holds for the key it was made with
and every key the membership log's `rotate` entries move it to under
that name, while that key is the name's current one (the log's, or a
held introduction's); a `revoke` ends it, and a member added again under
the name with a new key isn't granted. `machine add --agent` grants the
key the user confirmed once the add succeeds (`machines.add {agent}`);
`machine set <name> --agent on|off` and `a` on the dashboard's Machines
tab (`machines.set {agent}`) grant `name`'s current key in this host's
membership log, or withdraw the grant, for any kind of machine:
registered, inbound or routed. A grant change makes this host's link
report due at once (it lists the grantees, [fleet](fleet.md#link-graph))
and re-publishes the federated snapshots. Whether this host grants a
member now (`grants::granted_names`) is `agent` in `machines.list` /
`get` and the federated snapshot's `machines`, shown in the Machines
tab's AGENT column and `machine ls`, and marked `(agent)` in the
dashboard's header note.

## The CLI: `cli.rs`

`route` (I/O) runs `plan` (pure) in `main` before a verb goes to
`verbs_client` or any command runs locally. A command on a machine goes
through this host's daemon (API calls with `machine`) or, for `PTY` and
`TUNNEL` dials from this process, `fleet::route::connect` (the
machine's `fleet-<name>.sock`, else its `route-<name>.sock`: the same
for every kind of machine), and so through the machine's trust table (fleet plan decision 15:
API calls and `PTY` and `TUNNEL` streams within its trust, never a
`devsandbox …` run there).

- Under `--in-process`: nothing fans out; `--machine` is an error.
- **A command's root on a machine**: the match's own root there (the
  instance's `instances.resolve` row), else the machine's [default
  root](#held-links-machines) (`cli::Info::root_or_default`, from
  `machines.get`'s `default_root`). With none the command fails before
  any call (`machines::no_default_root`): the machine's link hasn't
  listed its roots yet, it has none, or its roots, named as `m:<root>`,
  hold none that is the default. `stop --all` / `start --all` take the
  machine's first root (any root reaches its whole `state.toml`);
  `rebuild --all`, `service rebuild`, `gc`, `ls` and `service ls` go by
  each root.
- **Verbs** (`run`, `rebuild`, `start`, `stop`, `rm`) keep their name;
  `verbs_client` resolves it across machines (`instances.resolve` /
  `sandboxes.list` with `machines: true`), picking on a TTY when ambiguous,
  and sends the request with the match's `machine` and root. On a host
  with no container runtime (`Ui::runs_here`, `runtime::installed`),
  `run`'s choice (`verbs_client::choose_sandbox`) leaves this host's rows
  out whenever a machine's remain, so a bare `run x` defined here and on
  one machine goes there without a pick; this host's rows alone are kept,
  and `commands::run::run` refuses them before any work (`this host has
  no container runtime …`).
- **Other named commands** (`exec`, `logs`, `inspect`, `done`, `undone`,
  `rename`, …) are resolved through the daemon; one on a machine is API
  calls through this host's daemon with `machine` set, or `PTY` dials from
  this process for an `exec` (below). `logs` / `inspect` with a bare
  `devsandbox-…` container name stay local. `port` on a machine's instance
  runs here: `route` returns `Routed::Port` (the machine's `Entry`, the
  match's root or else the machine's default, the instance as the machine
  names it) and `commands::port::port_remote` forwards over one `TUNNEL`
  stream per bridge through `route::connect`
  (`tunnel::remote_opener`; [forwards](forwards.md),
  [fleet](fleet.md#tunnel)). `vscode` on a machine's instance is refused
  (`calls::refusal`: not available over its link yet).
- **`fetch <machine>/<instance>`** (`commands::fetch`) runs here: one
  `placements.fetch` to this host's daemon, which fetches the instance
  this host placed back into the local checkout it came from
  ([fleet](fleet.md#moving-code), *Fetch-back*); a bare `<instance>`
  resolves among this host's placements, `@/<name>` is refused.
  `--machine` refuses it (`machine_use`).
- **`--machine m`** is for nameless commands (`machine_use`): `ps`,
  `stats`, `ls`, `status`, `service ls` are API calls
  (`MachineUse::Calls`, below); `stop --all`, `rebuild --all`, `start
  --all`, `service rebuild` and `gc` go as verbs (`verbs_client`; `gc` is
  `CliVerb::Gc`, one `gc` request per config root, each after its `==>
  m:<root>` line, the roots as `ls` takes them); `port --service <svc>`
  naming no instance (`MachineUse::Port`) forwards the machine's global
  service from its default root, as above. On a named command
  it's an error (qualify the name instead); refused for `inbox`, `vscode`,
  `serve`, `api`, `machine`, the dashboard.
- **Trust**: every call and stream is served by the machine's daemon
  under its trust for this host ([ssh](#ssh)). Under `worker` (the
  default) a command reaches the machine's own roots (those its
  instances name, plus its `member.toml` `roots`, which `machine add`
  records) and its fleet root for this host only, and `gc` is refused;
  `devsandbox machine trust full` on the machine opens the rest. A
  refusal (`denied`) is the command's error, named after the machine
  (`calls::answer_error`: ``machine `m`: …``, the machine's advice after
  it).
- Without a local daemon, a qualified remote name and `--machine` fail
  ("needs devsandbox serve"); a bare name resolves locally.
- **The agent**: the `machines.get` lookup before a command on a machine
  passes this process's `SSH_AUTH_SOCK` (if absolute) as `agent`
  (`info_params`), so while this host grants the machine agent the agent the daemon
  answers the machine's `AGENT` streams with, which the command there
  finds as `agent-link-<this host>.sock` or `agent-route-<this
  host>.sock` ([Agent](#agent)), is this
  terminal's. Nothing is held for the run: the link is always up.
- **API calls** (`cli::calls`): printed by the local command's own render
  fns (`commands::logs::render_logs`, `inspect::render`, `ls::table` /
  `ls::render_json`, `status::render`, `done::status_line`,
  `rename::renamed_line`, `profile::profile_line` / `show_lines`, `ps::render`, `stats::render`,
  `services::ls_render`), so the output reads the same; `calls::use_of`
  (pure) picks the call, `calls::named` / `calls::per_root` /
  `calls::listing` make them over an injected call; each exits 0, an
  error answer is the command's error. `logs [-n N]` is `containers.logs`
  of the instance's container (its row's, from an `instances.list` at
  `depth: disk`: a rename keeps the container; a name that is no instance
  is taken as a container name); `logs --build` is `instances.buildLog`
  (up to `api::MAX_LINES` lines, a note on stderr when cut; `ansi` when
  stdout is a terminal); `logs --build --follow` is `instances.buildLog`
  pages with `offset` (`calls::follow_build_log`, by the row's
  `instance_id`, so a rename doesn't lose it): from the log's start, the
  next page at once while pages bring something, after
  `logs::FOLLOW_POLL` when one doesn't (`calls::follow_next`), until one
  is `complete`; printed as the local follow prints (as they are on a
  terminal, a pty log through `build_log::Strip` elsewhere), and the
  record gone meanwhile ends it, as locally; `inspect [--json]` is
  `containers.inspect` (without the local one's browser sandbox line: that
  comes from this host's record); `done` / `undone` are `instances.done`,
  worded from the row's `done` before, the answer's `warning` (a placed
  instance's failed fetch-back) on stderr first; `rename` is `instances.rename`;
  `profile <m>/<x> <p>` is `instances.setProfile`, and without `<p>` the
  row's `profile` and `profile_disallowed` (no allowed set: the row has
  none).
  `--machine m ls [--json]` and `status --json` take each registered root
  for the config roots it stands for (`calls::config_roots`: itself, and
  the config roots under it its listing's instance rows name; a root
  without a config of its own, a fleet root, only those, when it has
  any), each an `instances.list` after its `==> m:<root>` line
  (`root_banners`: `--json` with several roots is an error); `ls` renders
  the snapshot's sandbox rows (a config that doesn't load is the error, as
  locally). `--machine m service ls [--json]` takes the same roots and
  rules, each an `instances.list` at `depth: listing` (container
  statuses), and renders its service rows (`used_by` cut to what the
  trust admits); the snapshot's `error` (the runtime's, resolve failures)
  goes to stderr, a config that doesn't load is the error. The empty line
  names `<root>/devsandboxes.toml`. `--machine m ps [--all] [--json]` is
  one `containers.list`, `stats [--json]` one `containers.stats`
  (`calls::listing`): the machine answers its whole runtime's rows cut to
  what its trust reaches (those instances' containers and their config
  roots' services, [fleet](fleet.md#the-trust-table)).
- **An `exec`** is `PTY` streams to the machine from this process
  (`fleet::pty_client::dial`, over `route::connect`; [fleet](fleet.md#pty)), at the
  instance's root there (the match's, else the machine's default); a dial's
  end becomes this process's exit code (`pty_client::exit_of`: the
  program's, 255 only for a lost link, the helper's session codes).
- **A tty `exec` reconnects** (`reconnect_session`: no `--attach` /
  `--sessions` / `--service`, and a tty by a local `exec`'s rule,
  `shell_flags`, with this process on a terminal standing for its stdin).
  The id is this process's (`--session` as given, else
  `exec::mint_session_id`); the first dial creates the session (or
  attaches) running the command (none: the login shell), each re-dial
  attaches only (a reconnect never starts a second program; `dials`),
  both `--reserve-255` (a program's 255 reported as 254), in
  `reconnect_loop`. `Reconnect` (pure, shared with the dashboard's remote
  tabs) decides after each dial: after a dial that ran `CONNECTED_AFTER`
  (5 s) a 255 is an outage: `connection lost, reconnecting… (Ctrl-C to
  stop)` on stderr, then `reconnecting… (attempt N)` per further try,
  re-dialed after `backoff` (1, 2, 4, 8, then 10 s, no limit on tries). A
  255 before any dial got that far ends the loop with it. Any other code
  ends the loop with it; `NO_SESSION_EXIT` (250) also prints that the
  session is gone (`gone_note`); the helper's 251 (attached elsewhere) and
  252 (`VERSION_EXIT`: started by another devsbd, still running) end it
  too, the helper's own note on stderr. A fallback terminal
  ([fleet](fleet.md#pty), *Tty fallback*: no helper, a non-root one) is
  dialed as a session, so a lost link re-dials its attach, which the
  machine answers with `no-session`: the loop ends with 250 and the
  machine's not-resumable message. Ctrl-C during a wait ends the process.
- **Sessions**: `--sessions` is a `sessions.list` quick call with
  `machine` (the lines printed, or `no sessions in \`m/x\`` on stderr);
  `--attach [ID]` is one dial attaching only (no id:
  `exec::choose_detached` over that list), no loop, as a local
  `--attach` (`session_exec`): on a terminal a session dial, else the
  helper's session client piped (`attach_mode`:
  `exec::session_attach_command`, stdin kept), as a local `exec --attach`
  without a tty runs it. Under `--reserve-255` (a dashboard tab's dials)
  a qualified name whose `instances.resolve` fails but as `not-found` (the
  machine's link is down) exits 255, the error on stderr
  (`cli::resolve_lost`), so the tab waits the outage out as for a dial
  that lost its link.
- **An `exec` without a tty** is one piped `PTY` dial ([fleet](fleet.md#pty),
  *Pipe mode*), never re-dialed: `cli::pipe_mode` (pure) gives its argv
  and `-i` as a local `exec` decides them (the command as typed with `-i`
  as given; none, the login shell with `exec::shell_flags`' `-i`); with
  `--session <id>` that argv in the helper's session `id`
  (`exec::session_new_command`), as a local `exec --session` without a tty
  runs it. `exec --service <name>` is one service `PTY` dial
  (`cli::service_mode`: the command as typed, none the machine's login
  shell; [fleet](fleet.md#pty), *Service terminals*), never re-dialed (no
  session), refused without a terminal.

## `devsandbox machine add|ls|set|unlink|rm|hub|pending|approve|deny|key|trust|roots`

A client of `machines.*` (and, for `trust`, `roots` and `key`,
`member.*`; for `key rotate`, `hub`, `pending`, `approve` and `deny`,
`membership.*`) only, never in-process (`src/commands/machine.rs`).

`add` first checks the machine's devsandbox (`ensure_devsandbox`, over
[Install](#install)): `install::check` through the login `PATH`, then
`install::decide` against this binary's version. The same version goes on
to the probe. Otherwise, a machine whose `uname` fails
`install::platform_matches` gets a warning and goes on to the probe when
it has no devsandbox, and is left as it is when it has another version
(the daemon's version warning still applies). A devsandbox found only
through the `~/.local/bin` fallback (`off_path`) makes the machine's
`command` `LOCAL_COMMAND`, installed over or not. Then `consent`: `--auto-install` installs, a TTY asks `[y/N]` on
stderr, no TTY is an error pointing at `--auto-install` (`refusal`).
Declined, a missing devsandbox is an error and nothing is added; another
version carries on as it is. `install::install` copies `exe::devsandbox()`
there and a second `install::check` decides (`rechecked`): `devsandbox` on
the login `PATH` is now this version, done; installed to
`~/.local/bin` but the login `PATH` doesn't reach it first (macOS by
default, another devsandbox earlier on it), the installed path is checked
directly and the machine's `command` becomes `LOCAL_COMMAND`
(`~/.local/bin/devsandbox`); updated in place but still not what the
login `PATH` resolves, an error naming what it resolves to. That
`command` goes to both `machines.probe` and `machines.add`.

`add` then always calls `machines.probe` (the remote's version and its
`member.get`'s `key` and `fingerprint`). It prints `<target>'s key
fingerprint is SHA256:…` on stderr and has it confirmed (`confirm_key`,
over `consent`): `--yes` takes it, a TTY asks `[y/N]` (to compare with
`devsandbox machine key` run there), no TTY without `--yes` is an error
naming the fingerprint and the flag (`key_refusal`). The confirmed key
goes to `machines.add` as `key`. `add` picks no config roots: a
machine's are the ones its own daemon lists (its `roots.list`), and a
member names more of its own with `devsandbox machine roots add` run
there. `add` prints `added <name> (<target>)`.

`machines.add` also applies the join rule
([fleet.md](fleet.md), *How a machine joins*): on the same probe it asks
the machine's `member.get` (refusing, `denied` and naming both
fingerprints, a `key` that isn't the one there) and `machines.list`,
reads this host's `member.toml` (`Ctx::member`, the name
`Member::name_or` the hostname), and `join::plan` decides from both
ends' hubs who joins and the registry name, refusing two hubs (`denied`)
and a joining name the fleet already has. `add --hub` (`machines.add`'s
`hub`, always sent) makes the machine the hub of the new fleet when
neither end is in one, and is refused (`denied`) when either end is in a
fleet whose hub the machine isn't. Then `join::logs` plans both ends'
membership logs over this host's `membership.json`, its key, and the
machine's `membership.get`. Every check (names, the registry and
the inbound neighbors, both logs) comes before any write; then the
machine's `member.set {name}` when it joins, its membership over the
still-open probe (`membership.adopt` of the log made here, or of this
host's on an extra link; `membership.sign` of this host when it joins
the machine's fleet), this host's registry entry, this host's
membership (`membership_api::adopt` of the same log, the machine's, or
the machine's signed answer: it writes `hub` and the name the log knows
this host by, then `Ctx::hub_changed`, which is
`daemon::Shared::set_hub` as after a `membership.adopt` call), on the
authority from a local client this host's configs signed
(`changes::sign_configs` of `changes::add_roots`: `~/.devsandboxes` and
`member.toml`'s `roots`; [fleet.md](fleet.md#approvals-and-pushes),
*Config versions*), and the links reload, so the machine's first link-up
carries the log with them and it installs them. Each sign's notes are
the answer's `configs` (``note: signed config `<name>` v<N>``, or why
not), printed by `machine add` on stderr after its other lines; a
failed sign never fails the add. A write that fails after an earlier one
says what was already written. Each end's name is pinned as it adopts, so a name that
came from the hostname stays the one the fleet knows it by.
The answer's `fleet` is the
fleet both ends are in and who joined; the CLI prints it on a line after
`added …` (`fleet_line`). When this host, not the fleet's authority,
introduced the machine, `machines.add` then proposes that introduction
to the authority over any path (`fleet.propose`, best effort; the
answer's `proposed`), and the CLI prints `proposed its add to
<authority>: approve there with devsandbox machine approve <id>`, or why
not (`proposed_line`). When the machine, not the authority, introduced
this host (`membership.sign` there), `machines.add` keeps the probe open
until this host's writes are done and asks the machine to propose it
(`membership.propose {name}`, best effort; an error, an older machine's
`unknown-method` included, is the answer's `proposed.error`, never a
failed add), and the CLI prints `proposed this host's join to
<authority>: …` (`added_proposal_line`). `add --agent` grants the machine agent by its
confirmed key ([Agent](#agent)); a name the registry or an inbound
neighbor has is refused. `set <name> --agent on|off` (`machines.set`;
`--agent` is required) grants or withdraws it for a machine of any kind
and prints `<name>: agent on|off`. `ls` shows the grant in an `AGENT`
column, and `inbound` as an inbound machine's `TARGET`, `routed via
<hop>` as a [routed](#routed-machines) one's; `CAPACITY` is the
machine's last [capacity report](fleet.md#capacity)
(`machines.list`'s `capacity`, `-` before one came), the over-cap note in
parentheses; a machine this host's membership holds as an introduction
(`machines.list`'s `introduced_by`, `api::introducer_of`: capped, it
introduces no one, fleet plan decision 51) is named `<name> (introduced
by <introducer>)`, and one `worker` and exposed through another machine
(`machines.list`'s `exposed_by`, [exposure](fleet.md#exposure)) `<name>
(exposed via <dialer>)`, the machine that dials it (`graph::exposed_note`;
both notes joined by a comma) (`commands::machine::cells`). `unlink <name>` (`machines.unlink`) drops this host's link to
a registered machine, which stays a fleet member; refused for an inbound
one, prints `unlinked <name>`. `rm <name>` (`machines.rm`) takes the
machine out of the fleet, and `rm` of this host's own member name leaves
it ([fleet.md](fleet.md), *How a machine leaves*); it prints one line per
outcome (`rm_lines`): on the authority the revoke signed and whom it was
sent to (`signed_lines`), then who was removed from whose fleet, whether
the machine was told or not reached (and why), each member that forgot
it or wasn't reached; on another member `proposed removing <name> to
<authority>: approve there with devsandbox machine approve <id>`; on a
leave its own proposal's outcome and the entries this host dropped.
`hub <name>` (`commands::machine::hub`, `membership.hub`) moves the
fleet's hub role to member `name` ([fleet.md](fleet.md#approvals-and-pushes)):
on the authority it prints `<name> is the fleet's hub` then
`signed_lines` (whom the push reached); on another member `proposed
making <name> the fleet's hub to <authority>: approve there with
devsandbox machine approve <id>` (`hub_lines`).
`pending`, `approve <id>` and `deny <id>` are the authority's side of a
proposal ([fleet.md](fleet.md#approvals-and-pushes)), local only:
`pending` (`membership.pending`) prints an `ID FROM WHAT WHEN` table
(`pending_text`, the age as the Inbox shows one) or `no pending
proposals`; `approve` (`membership.approve`) signs it here and prints
`signed_lines`; `deny` (`membership.deny`) drops it and prints `denied
proposal <id>`.

`trust [worker|full]` is about this host, not a registered machine: a
client of `member.*` (this host's `member.toml`;
[fleet.md](fleet.md#trust)); while the answer's `exposed_by` is set it
prints `warning: <dialer> dials this host over ssh, so it has a shell
here …` on stderr, naming that machine and the chain
(`exposure_warning`, [exposure](fleet.md#exposure)). So is `roots [add|rm] [<path>…]`
(`commands::machine::roots`), this host's picked roots (`member.toml`'s
`roots`): without a subcommand `member.get`, printing them one per line
(or a note when there are none); `add` resolves each path here
(`resolve_root`: a leading `~` / `~/` is `$HOME`, a relative path is
against the current directory), canonicalizes it and requires a directory
(`existing_dir`), then `member.set {roots_add}`; `rm` resolves the same
way without requiring it to exist and sends both the canonical form (when
it exists) and the literal one as `member.set {roots_rm}`, so a removed
directory can be dropped. Both print the roots after the change. So is
`key` (`commands::machine::key`): `member.get`, printing `<name> <key>`
(`-` for a host with no valid name) and the key's fingerprint on the next
line (`key_text`), then a pending rotation's next key (`rotating to
<fingerprint> (<key>): …`, `member.get`'s `next`); the key is made on
that first ask ([fleet.md](fleet.md#member-identity)). `key rotate`
(`commands::machine::rotate_key`, `membership.rotate`) rotates it: on the
authority `this host's key is now <fingerprint> (<key>)` then
`signed_lines`; on another member `proposed rotating this host's key to
<fingerprint> to <authority>: approve there with devsandbox machine
approve <id>` and that this host keeps its key until the signed change
reaches it (`rotation_lines`).
`--machine` and qualified names refuse `machine` as a whole
(`serve::machines::cli::machine_use`).

## Dashboard

`src/tui/remote.rs`: rows are keyed `machine/name` (bare for local),
routed back to a request's `machine` + remote `root`; on every machine,
whatever its kind, an instance tab runs this host's `devsandbox exec …
-- <m>/<name>` (`on_machine`, the executable `ExecCli::exe`), whose tty
`exec` dials the machine ([The CLI](#the-cli-clirs)), and a
service tab runs `devsandbox exec --service=<name> -- <m>/<instance>`
through an instance using it (`service_argv`, `service_instance`; a
global service no instance uses has none, and its tab is refused); a
remote instance tab runs
its shell in a helper session it names (`session_exec_argv`) and, when its
dial exits 255 (a lost link, or the link down when it resolves the name),
re-dials an attach to it into the same tab (`attach_argv`,
`tui::app::terminal::Redial`, the CLI's `Reconnect` rule;
[terminal.md](../dashboard/terminal.md) *Remote tabs reconnect*); a machine without rows shows `box: connecting` or `box:
unreachable: <why>` in the header (`pending_note`), the name as `box
(agent)` while this host grants it agent. `p` / `:port` on a remote row sends
`forwards.add` with `machine` and the row's root (the forward binds here,
listed with the rest on the Ports tab: `forwards.list {machines: true}`);
VS Code on a remote row answers "isn't supported yet". The Machines tab,
listed while there is a machine, shows every machine from the snapshot's
`machines` with its target (dimmed `inbound` for an inbound one, dimmed
`via <hub>` for a routed one, which the snapshot has only from the hub's
view), link state (and error), `agent` (this host's grant, any kind),
and its
CAPACITY ([capacity](fleet.md#capacity)); a machine `worker` and exposed
through another one (the entry's `exposed_by`) has `exposed via
<dialer>` after its NAME, in yellow (`ui::machine_name_text`,
[exposure](fleet.md#exposure));
`a` flips the selected one's grant with `machines.set` (no prompt; a
down machine's too, it's a `member.toml` write; any kind,
`App::toggle_selected_agent`), and the row follows with the next push,
which the grant change triggers
([tabs-and-actions.md](../dashboard/tabs-and-actions.md) *Tabs*). A routed machine's
rows are actionable like any other's: its tabs run the same `exec` on the
qualified name (`on_machine`), dialed over a route, and actions on its
rows carry its name as `machine`, which the daemon forwards to it over a
route ([Routed machines](#routed-machines)).

Limitations: [status.md](../../status.md#daemon)
