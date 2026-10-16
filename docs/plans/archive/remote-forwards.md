> Archived 2026-10-16. Historical, may be wrong; do not use as a reference.
> Current: [architecture/daemon/forwards.md](../../architecture/daemon/forwards.md#remote-bridges),
> [architecture/daemon/machines.md](../../architecture/daemon/machines.md)

# Remote forwards

Status: shipped
Item 1 of [machines-next](../active/machines-next.md): `devsandbox port`, the
dashboard's `p` / `:port` and the Ports tab work on a machine's instances.

Scope: remote forwards are always asked for from this side (CLI or
dashboard) and owned here; a machine's `forwardPorts` are never mirrored
locally (a port exposed on the remote doesn't mean you want it here). Like
local forwards, a dead ssh is respawned by the engine until the forward is
stopped; daemon-held ones persist in `forwards.toml`.

## Design

**Transport: one ssh per forward, carrying the existing bridge mux.** A
local forward today keeps one `<runtime> exec -i -u root <c> devsbd bridge`
alive and opens a flow-controlled `Connect` stream on its mux per accepted
connection (`src/devsbd/forward.rs`, `src/devsbd/bridge.rs`). For a remote
instance the same byte stream comes from

    ssh -T [-o BatchMode=yes] -- <host> <remote devsandbox> tunnel -C <root> [--instance <key>] [--service <svc>] --port <n>

where the remote `tunnel` (hidden) resolves the route on its own host
(`port::resolver`: its config, state, runtime), writes one header line
(route label, dial host/port, helper hash) to stdout, then `exec`s
`<runtime> exec -i -u root <container> devsbd bridge`, so stdio passes
straight through. The local side reads the header, then runs the usual
handshake + mux on the ssh child's pipes. Everything the engine does
(many connections over one bridge, half-close, flow control, respawn with
re-resolve on death) works unchanged; ssh's ControlMaster, if the user has
one, makes each spawn cheap. Not over the held machine link: it's
JSON-lines, one quick call at a time; tunnelling bytes there means a new
multiplexing layer in the API for no gain.

The listening-process column comes from the helper itself (step 1), so it
works over ssh too.

Consequences (go to `docs/status.md` when shipped):
- A remote service reached via an instance shows no listening process (the
  instance's helper can't see the service container; locally the host
  `lsof` fallback still covers it).
- The helper frame protocol must match between this devsandbox and the
  remote's embedded helper; a mismatch is the bridge's existing
  version-mismatch error, phrased for a machine ("update devsandbox on
  <machine>" rather than "restart the instance").
- A remote devsandbox without `tunnel` fails with a clear "update
  devsandbox on <machine>" (detected from its stderr / exit).

## Step 1 — the helper finds the listening process

Today the host `exec`s `lsof -F` in the container every `PROBE_REFRESH`
(`commands/port.rs:178` `listening_procs`, called through
`ForwardSpec.probe`, `forward.rs:69`, `refresh_process` `:525`): an extra
`exec` per forward per 10 s, and nothing in images without `lsof`. Move it
into the helper daemon, asked over the forward's own bridge.

- Protocol (`src/devsbd/proto.rs`, shared with `devsbd/` via `#[path]`):
  additive, `VERSION` unchanged: a new cap (e.g. `caps::PORT_INFO`) and a
  host-initiated request/answer for "who listens on TCP port N" (new frame
  pair or a stream on a new channel: implementer's call, consistent with
  `docs/architecture/helper/protocol.md`, which documents it). The
  in-container bridge relays it (`devsbd/src/bridge.rs:16` `OWN_CAPS`:
  caps are `own & daemon`), the daemon (`devsbd/src/daemon.rs:353` Hello
  caps) answers it.
- Helper lookup: `/proc/net/tcp` + `tcp6` LISTEN sockets on the port →
  inodes → `/proc/*/fd` `socket:[inode]` → `/proc/<pid>/comm`. No `lsof`.
  Same output as today (`format_procs`, `port.rs:166`: `node (pid 412)`,
  deduped, order kept); pure parsing fns unit-tested in `devsbd/`.
- Host: `Bridge` gets a `listening(port, timeout) -> Option<String>`
  (`src/devsbd/bridge.rs`); the engine's refresh uses it when the bridge has
  the cap and the probe target is the dialed container (instance and
  injection routes); else it falls back to `ForwardSpec.probe` (today's
  `lsof` exec, kept for via-instance routes and older helpers). Still on
  the supervisor thread, still `PROBE_REFRESH`.
- Tests: proc parsing (pure, `devsbd`), frame codec round-trip, a
  `docker_test(helper)` that a forward to `nc -l` shows its process
  without `lsof` in the image (alpine has none by default; check), model on
  `listening_procs_with_docker` (`port.rs:1082`).
- Docs: protocol.md, forwards.md (*Routes* `listening_procs` paragraph).

## Step 2 — engine: pluggable bridge opener

No behavior change.

- `src/devsbd/bridge.rs`: split `spawn_observed` (`:513`) into building the
  child and attaching to it, so a caller can hand in an already-spawned
  child whose stdout it has partly read (the header). New
  `pub fn attach(child: Child, stdin, stdout, stderr, hash, label) -> Bridge`
  (agent-less, no services, no observer); `spawn_for` (`:481`) unchanged in
  behavior. `container` in `Bridge` is only for messages: generalize to a
  label.
- `src/devsbd/forward.rs`: `ForwardSpec.resolve` (`:63`) +
  `bridge::spawn_for` in `try_spawn` (`:424`) become one opener:
  `open: Box<dyn Fn(&AtomicBool /*stop*/) -> Result<(Route, Bridge), String> + Send + Sync>`
  (exact shape implementer's call). Keep a helper
  `local_opener(resolve)` that does today's resolve + `spawn_for`, so the
  callers (`commands/port.rs:521`, `serve/forwards.rs:602`, `:704`, and
  the tests) change one line each. `Route.container` stays for the local
  opener; the engine itself must no longer need it.
- Constraint: the opener runs on the supervisor thread; a `Drop` must still
  stop within a `HANDSHAKE_SLICE`-ish bound (the remote opener in step 2
  blocks on ssh, so it must honor `stop`). The probe fallback from step 1
  stays per opener (`None` for remote).
- Tests: existing forward/bridge tests pass unchanged; one pure-ish test of
  an opener that fails (`Error` + note), like
  `failing_resolve_surfaces_error_and_drops_cleanly`.

## Step 3 — `devsandbox tunnel` and the remote opener

- Header codec, shared by both ends (new `src/devsbd/tunnel.rs` or in
  `commands/port.rs`): one line `devsandbox-tunnel 1 <json>\n`, json
  `{label, host, port, hash}` or `{error}`. Reader skips lines before the
  magic (a chatty login shell), bounded (e.g. 64 KiB / 50 lines).
- Hidden `Command::Tunnel { instance, service, port }` in `src/main.rs`
  (unix-only, `#[command(hide = true)]`), uses the global `-C`: resolves
  `instance` with `resolve_instance_noninteractive`, runs
  `port::resolver(dir, key, service, port)()`, writes the header, then
  `CommandExt::exec` of `<backend().bin()> exec -i -u root <container>
  devsbd bridge`. On a resolve error: an `{error}` header, exit non-zero.
  Writes nothing else to stdout. Put the body in `commands/port.rs` (`pub fn
  tunnel`).
- `src/serve/machines/ssh.rs`: `tunnel_argv(entry, batch: bool, root,
  instance, service, port)` through `argv` (`:158`): `-T`, no `-A`,
  `BatchMode=yes` when `batch` (daemon).
- Remote opener (in `commands/port.rs`, `pub fn remote_opener(argv_fn,
  machine)`): spawn the argv (stdin/stdout piped, stderr captured as a
  tail), read the header with a deadline (`HANDSHAKE_WAIT`), honoring
  `stop` (reader thread + channel; kill the child on stop/timeout), then
  `bridge::attach`. The argv is rebuilt per spawn from a closure (the
  daemon re-reads `machines.toml` so a changed target heals). Errors: the
  header's `{error}`; ssh's stderr tail when no header; "update devsandbox
  on <machine>" when the tail says the subcommand is unknown. Route label
  shown as `<machine>/<label>`.
- Tests: header codec round-trip + noise skipping (pure); remote opener
  against a fake argv (`sh -c 'echo motd; printf <header>; exec cat'` /
  one that prints an error header / one that dies) for the error paths;
  a `docker_test(helper)` where the argv is `sh -c '<echo header>; exec
  docker exec -i -u root <c> devsbd bridge'` and an echo server round-trips
  (model on `forwards_to_a_loopback_echo_server_with_docker`,
  `forward.rs:732`). Plus one test that runs the real `tunnel` subcommand
  is optional (needs a state with an instance): skip unless cheap.

## Step 4 — `devsandbox port` on a machine (CLI)

- `src/serve/machines/cli.rs:473`: instead of bailing for `Port`, return a
  new `Routed::PortOn { entry, root, name }` (machine `Entry` from
  `machine_info`, `:479`); `main.rs:512` passes it to a foreground
  `port::port_remote` that is `port::port` (`commands/port.rs:481`) with
  the remote opener (`batch = false`), fallback probe `None`. Share the wait/print
  loop with `port`, don't copy it.
- `--machine m` with `port --service <svc>` (no name): allowed, root = the
  machine's only root (several → error naming `-C`? keep: first root, like
  `cli.rs:481`). Update `machine_use` (`:85`) and `plan` (`:162`-`:187`).
- `main.rs:267` doc string ("machines aren't supported yet") updated.
- Tests: `plan`/`machine_use` table tests (`cli.rs:569`, `:593`).
- Docs: machines.md *The CLI*, forwards.md *Foreground*, the
  `config-toml-spec` skill only if it mentions this (likely not).

## Step 5 — daemon: ad-hoc remote forwards

- API: `forwards.add` with `machine` is handled **locally** (it binds
  here), before the generic refusal at `src/serve/daemon.rs:917`;
  `dir` is the remote root (absolute, not canonicalized locally). Unknown
  machine → `not-found`. `forwards.list` gains `machines: bool` (default
  false: today's behavior, local only); `forwards.rm` unchanged (ids are
  daemon-wide). `ForwardRow` (`serve/forwards.rs:64`) and `AddRequest`
  (`:86`) gain `machine: Option<String>`.
- Registry (`serve/forwards.rs`): remote forwards live outside `Root`
  (which loads local config): e.g. `remote: BTreeMap<(String, PathBuf),
  RemoteRoot>`, ad-hoc only (no configured remote forwards, ever: see
  *Scope*). The instance name is resolved on
  the remote by `tunnel` (store the name the user resolved through
  `instances.resolve`, i.e. the remote state key the client sends). The
  registry needs the machines' entries: give `Registry::spawn` a lookup
  (`Fn(&str) -> Option<Entry>`, the daemon passes the `machines.toml` path
  or `links.entries()`); a sync drops forwards (and store entries) of a
  machine no longer registered.
- `forwards.toml` (`forward_store.rs:29`): `Entry.machine:
  Option<String>`, skip-if-none, version stays 1 (additive). `restorable`
  (`forwards.rs:886`) keeps remote entries without the local state check.
- `docs/reference/api.md` *Forwards*, `npm/devsandboxes/index.d.ts`
  (`dts.rs` test checks it), forwards.md, machines.md *Forwarded requests*.
- Tests: `api.rs` forwards tests (`:2203`) for `machine` params; registry
  tests with a fake opener for add/list/rm/restore/drop-on-unregister.

## Step 6 — dashboard

- `src/tui/app/command_line.rs:107`, `:129`, `:190`: send the forward
  instead of `not_yet_port`; `PortRequest` (`tui/app/mod.rs:116`) gains
  the machine + remote root (from the row's route, `tui/remote.rs`).
  Delete `not_yet_port` (`actions.rs:30`).
- `tui/daemon.rs:504` `list_call` sends `machines: true`; `:510`
  `add_call` sends `machine`; `PortRow` gains `machine`, the Ports tab
  shows it like other remote rows (`box/…` in target, or a column: match
  how the Instances tab shows remote rows).
- Tests: the app's `p` tests and `daemon.rs` call-shape tests (`:1022`).
- Docs: dashboard docs (`docs/architecture/dashboard/`), machines.md
  *Dashboard*.

## Step 7 — wrap-up

`docs/status.md` (the limitations above), `CHANGELOG.md` Unreleased,
machines-next.md item 1 removed (note there that a machine's
`forwardPorts` are deliberately not mirrored), this plan to
`plans/archive/` with its content in the living docs.
