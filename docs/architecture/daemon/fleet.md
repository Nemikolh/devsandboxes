# Fleet

Machines as one pool of capacity (the plan:
[fleet](../../plans/active/fleet.md)). What exists so far is the link
primitive the fleet's links are built on (the machines'
[agent relay](machines.md#agent) runs on it too) and both sides of a
worker's link: on the hub the offer, the offered registry entry and its
dial; on the worker `lend.toml`, the lending thread, the lent API, the
worker policy and `lend.*` with its CLI, `devsandbox lend` and
`devsandbox fleet join|leave`
([ADR 0041](../../decisions/0041-fleet-workers-dial-their-hub.md)).

Code: `src/serve/fleet/mod.rs` (`LINK`), `src/serve/fleet/link.rs`
(`Spec`, `opener`, `answerer`, `End`, `write_sentinel`,
`match_sentinel`), `src/serve/fleet/offer.rs` (`call`, `Ctx`, `accept`,
`dial`), `src/commands/fleet_link.rs` (`fleet_link`, `run`),
`src/serve/endpoint.rs` (`fleet_socket`), `src/daemon_config.rs`
(`ServeConfig::hub`), `src/devsbd/proto.rs` (`channel::API`, `caps::API`,
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
`enforce_argv`, `Refusal`).

## Link

A link is one byte pipe between two hosts (an ssh's stdin/stdout in
production) carrying the helper's [frame protocol](../helper/protocol.md),
with the roles of a container's link moved onto the two hosts. A connection
to a socket on one host becomes a stream on the mux, answered on the other
host by a connection made there. A `Spec` says what a link carries: its
sentinel line (`\n<sentinel> <version>\n`), its one channel, the cap that
says the far end serves it, and the `Hello` hash (informational).

- **The opener** (`link::opener`, the in-container daemon's role) gets a
  reader, a writer and a socket path. It binds a temp name next to the path
  (0600) and renames it over the path, so the newest opener for a path
  wins; then it writes the sentinel, exchanges `Hello`s (advertising
  nothing) and requires the peer's `Hello` caps to include the spec's
  cap. Its `on_ready` callback runs next, with the peer's `Hello` (its
  `hash` is the far end's spec `hello`, a semver); an error there ends
  the opener with it. Each accepted connection becomes one `Open { channel }` stream
  (`Mux::attach`, ids below 2^31); on a flowing channel
  (`channel::flows`) the stream is flow-controlled. `Open`s from the peer
  are refused. It returns `Ok` when the reader ends or the writer fails,
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
  it with `Close`, as is every other channel). It returns an `End`: the
  handshake failed (with the first line of noise), the keepalive ran
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

## Hub side

A worker dials its hub: its daemon holds `ssh -T -o BatchMode=yes <hub>
devsandbox fleet-link --worker <name>` ([Worker side](#worker-side)). The
hub never dials the worker; everything it does there goes over that
link's `API` streams.

- **`devsandbox fleet-link --worker <name>`** (hidden,
  `commands::fleet_link`) is the opener of `fleet::LINK` on the hub. The
  name must pass `machines::check_name`. It binds
  `endpoint::fleet_socket`: `<socket dir>/fleet-<name>.sock` (0600 in the
  0700 socket dir, the name cut as an agent relay socket's client is), so
  the newest link for a worker wins. Its `on_ready` connects to this host's
  daemon (`client::connect`, which starts one like any client does when
  none runs) and sends `fleet.offer {worker, version, draining}`, `version`
  the worker's `Hello`, `draining` its `--drain` (the worker's `lend` was
  `drain` when it dialed). A refused offer ends the link with `offer refused:
  <why>` (on its stderr, which the worker's ssh keeps). It holds that
  daemon connection for the link's life, so a linked worker is a `clients`
  holder of the hub's daemon. `serve::machines::cli::machine_use` refuses
  `--machine` for it.
- **`fleet.offer`** (`fleet::offer::call`, internal: not in the npm
  typings) is accepted only by a hub: `[serve] hub = true` in
  `daemon.config.toml` (`ServeConfig::hub`, read once at start into
  `daemon::Options::hub`); anything else is `denied`. `accept` (pure)
  adds an offered entry for a new name, keeps an existing offered one, and
  refuses (`denied`) a name an ssh machine has. A new entry reloads the
  machine links (`service::Handle::reload`), which start a link thread for
  it; the offer is logged with the worker's version and whether it's
  draining (nothing on the hub acts on that yet).
- **An offered entry** is `[machine.<name>]` with `offered = true` and no
  `target` in `machines.toml` (`machines::Kind::Offered`,
  [Registry](machines.md#registry-machinestoml)). Its roots are empty.
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

## Worker side

A worker lends itself to one hub, through `lend.toml`; the hub reaches
it only through the link the worker holds, and there only through the
lent API ([ADR 0041](../../decisions/0041-fleet-workers-dial-their-hub.md)).

- **`lend.toml`** (`<data>/devsandbox/lend.toml`, next to `state.toml`,
  `fleet::lend::path`) is the worker's own; the hub never writes it. Keys:
  `state` (`on` | `off` | `drain`; absent is `off`), `hub` (the hub's ssh
  target, as `machine add` takes one), `name` (what this host offers itself
  as; absent is the hostname's first label, `lend::default_name`, which
  must pass `machines::check_name`) and an `[allow]` table, the
  [worker policy](#worker-policy)'s allowances. Unknown keys are ignored. `lend::load`
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
  [--drain]>` (`--drain` while `drain`, read at each dial)
  (through the hub's login shell, `ssh::target_argv`, the builder every
  machine argv uses, here without a registry entry) and runs
  `link::answerer` of `fleet::LINK` on its stdio. A link that ends (the
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
- **`drain`** is recorded, shown by `lend.get` and offered (`--drain`);
  nothing places work yet (fleet plan step 3), so it changes nothing else.

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
  version it says: only a local client's newer binary replaces it.

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
