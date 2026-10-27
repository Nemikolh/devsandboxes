# Helper protocol: frames, mux, line codecs

The wire between the host and `devsbd`: one framed byte stream over the stdio
of `exec -i <container> devsbd bridge`, multiplexed into streams, plus the
line-based record formats that ride on it (or sit in files). Read before
changing either side of the link; both sides compile the same source files.

Code: `src/devsbd/proto.rs` (`Frame`, `VERSION`, `caps`, `channel`,
`handshake`), `src/devsbd/mux.rs` (`Mux`, `Conn`), `devsbd/src/bridge.rs`,
`devsbd/src/daemon.rs` (`serve_bridge`, `Bridges::route`),
`devsbd/src/listeners.rs` (`lookup`), `src/devsbd/rpc.rs`,
`devsbd/src/rpc_server.rs`,
`src/devsbd/{bootfile,notify,control,escape}.rs`, `devsbd/src/json.rs`.
All shared files are included by the helper via `#[path]`; they are std-only
because `devsbd` has no dependencies.

## Transport path

```
host owner ──exec -i -u root <c> /run/devsandbox/bin/devsbd bridge──▶ devsbd bridge
                  (stdin/stdout = frames)                                  │ unix socket
                                                        /run/devsandbox/devsbd.ctl (0600)
                                                                           ▼
                                                                    devsbd daemon
```

- `devsbd bridge` connects to the daemon first (retrying ~2 s; if nothing
  listens it spawns `devsbd daemon` once itself: the self-heal for containers
  restarted outside devsandbox), does `Hello` with the daemon, then with the
  host, then copies bytes verbatim both ways. The daemon parses the frames;
  the bridge never does after the handshake.
- The bridge advertises to the host `(TCP_FORWARD | PORT_INFO | RPC) &
  <daemon's caps>` (`OWN_CAPS`), so the host sees what the whole path can
  serve.
- The daemon routes each new stream to the **newest** attached bridge whose
  host serves that channel; older bridges stay as fallbacks.

Who owns host bridges and what they serve: [bridges.md](bridges.md).

## Frames

Little-endian header `u32 stream | u8 kind | u32 len`, then `len` payload
bytes (`MAX_PAYLOAD` = 1 MiB: stray bytes such as a shell banner become an
error, not a huge allocation). Stream 0 carries control frames.

| kind | frame | payload / use |
|---|---|---|
| 0 | `Hello` | `u32 version \| u8 hash_len \| hash \| u32 caps`; first frame each way; trailing bytes ignored |
| 1 | `Open` | one channel byte; daemon opens a stream toward the host, or the host one toward the daemon (`RPC`) |
| 2 | `Data` | stream bytes |
| 3 | `Close` | optional UTF-8 reason (e.g. `connection refused (127.0.0.1:3000)`); empty = no payload |
| 4/5 | `Ping`/`Pong` | keepalive, payload echoed |
| 6 | `Quit` | first frame on `devsbd.ctl` from a daemon of another build taking over |
| 7 | `Connect` | host asks the daemon to dial `host:port` in the container (port forwards) |
| 8 | `Window` | `u32` credit grant on a flow-controlled stream |
| 9 | `Eof` | half-close |
| 10 | `Caps` | `u32` host capabilities, sent once right after the handshake |
| 11 | `PortQuery` | `u32 id \| u16 port`; host asks who listens on a TCP port in the container |
| 12 | `PortInfo` | `u32 id`, then per process `u32 pid \| u8 comm_len \| comm`; the daemon's answer |

**Frozen across versions:** the header, `Hello`'s leading `u32` version, and
`Quit`. Unknown kinds are skipped, so frames are added without bumping
`VERSION` (currently 1). `Hello`'s `hash` is informational; only `version`
must match.

### Channels (`Open`)

`SSH_AGENT` = 1, `HTTP_PROXY` = 2 (reserved, unused), `NOTIFY` = 3,
`CONTROL` = 4: opened by the daemon. The daemon opens `NOTIFY`/`CONTROL` only
toward a host that advertised the matching cap. `RPC` = 5: opened by the
host, only toward a daemon advertising `caps::RPC`. `API` = 6: never on a
container's link; between two hosts' `serve::fleet::link` ends
([fleet](../daemon/fleet.md)), opened by the end with the socket toward a
peer advertising `caps::API`, each stream one connection to a host
daemon's API. `TRANSFER` = 7: never on a container's link either; one bulk
transfer between two hosts' link ends (a header line, the body, a one-line
answer: [fleet](../daemon/fleet.md#transfer)), opened by either end toward
a peer advertising `caps::TRANSFER`: the end with the socket with ids below
2^31, the other end with high-bit ids, which the first admits through
`Mux::on_host_open`.

Whether a channel's streams are legacy or flow-controlled (see
[Mux](#mux)) is a property of the channel, `proto::channel::flows`, so both
ends agree without a flag on the wire: `RPC`, `API` and `TRANSFER` flow,
the rest are legacy.

### Caps

Bitset, unknown bits ignored: `TCP_FORWARD` (daemon serves `Connect`),
`PORT_INFO` (daemon answers `PortQuery`), `RPC` (daemon accepts host
`Open(RPC)`; sent by every daemon, `DAEMON_CAPS` in
`devsbd/src/daemon.rs`), `SSH_AGENT`, `NOTIFY`, `CONTROL` (host serves
those channels), `API` (a fleet link's answerer serves `channel::API`;
host-to-host only), `TRANSFER` (the peer receives `channel::TRANSFER`;
both ends of a fleet link, host-to-host only). The host sends
`Caps` after the handshake (`SSH_AGENT` only when it has a live agent;
`NOTIFY | CONTROL` only on the serve daemon's bridges). Until a bridge's
`Caps` arrives the daemon treats it as capable of nothing, except a bridge
whose own `Hello` caps were 0 (a pre-`Caps` host), treated as `SSH_AGENT`
only (`known_host_caps`).

### Handshake and version mismatch

`proto::handshake` writes our `Hello`, reads the peer's; a different
`version` is a typed `VersionMismatch`. The daemon answers a mismatched
bridge's `Hello` anyway so the bridge can report it. A bridge whose daemon
mismatches prints a direction-aware line and exits `MISMATCH_EXIT` (3); the
host classifies that by exit code, not text. The host kills a bridge whose
handshake takes over 10 s (`HANDSHAKE_TIMEOUT`).

### Port queries

The forward engine's listening-process lookup
([forwards.md](../daemon/forwards.md#routes)). A request/answer pair on
stream 0 rather than a stream: no local socket on either end, one small
answer. The host sends `PortQuery` only to a daemon advertising
`PORT_INFO` (older helpers lack the bit and never see the frame), with an
id of its choosing, and waits for the `PortInfo` carrying that id
(`Mux::port_query`, bounded by the caller's timeout; queued through the
control writer, so a wedged pipe can't block the caller past it). The
daemon runs each lookup on its own thread (`Mux::on_port_query`, at most
`MAX_PORT_LOOKUPS` = 4 at once per bridge; past that a query goes
unanswered) and answers `(pid, comm)` per process in pid order, at most
`MAX_PORT_PROCS` (64), an empty list when nothing listens. A late answer
finds no waiter and is dropped; the connection ending fails every waiting
query at once.

`listeners::lookup` reads `/proc`, no `lsof`: LISTEN (`0A`) rows on the
port in `/proc/net/tcp` and `tcp6` (any local address) give socket inodes,
`/proc/<pid>/fd` links `socket:[<inode>]` give the pids (at most
`MAX_PIDS`, 4096, scanned), `/proc/<pid>/comm` the names. A process that
vanishes mid-scan is skipped. The daemon's network namespace is the
container's, so this sees what the forward dials.

### Keepalive

Both ends of an established link run `Mux::keepalive`: `Ping` every 15 s,
peer declared dead after 45 s of inbound silence. The host then kills the
`exec`; the daemon shuts the bridge's socket and drops it from its routing
list.

### Daemon takeover

One daemon per container per build: `/run/devsandbox/devsbd.pid` holds a
flock plus `<pid> <hash>`. A daemon of a different build sends `Quit` on
`devsbd.ctl`, waits up to 3 s for the lock, and takes over; a second daemon
of the same build exits 0.

## Mux

`Mux` routes frames to local `Conn`s (unix or TCP sockets) on both ends with
the same code, so close semantics match: whoever sees its local end close
first sends `Close`; receiving `Close` shuts the local socket without echoing.

Stream ids: the daemon allocates `1..2^31` (high bit clear), the host
allocates with the high bit set (`Connect`, `Open(RPC)`), monotonically,
never reusing a just-closed id at once. Each side refuses ids from the wrong
space: the host every high-bit `Open`; the daemon a low-bit `Connect`, and a
high-bit `Open` unless it registered `Mux::on_host_open` (it does, and that
hook accepts `RPC` only). Caps: 64 live daemon-opened streams, 256 live
host-opened streams (`Connect` and `Open` together, `connect_allowed`) per
bridge.

Two stream kinds share a connection:

- **Legacy** (`Open` on ssh-agent, notify, control). `Data` is written into the
  local socket on the frame-reader thread. A peer `Eof` `shutdown(Write)`s the
  local socket, which is how request/response streams work: the daemon sends
  the request then `Eof`, the host replies and closes.
- **Flow-controlled** (`Connect`: forwarded TCP; `Open` on a flowing
  channel: `RPC`, `API`, `TRANSFER`). Both ends register the stream as flowing: the opener in
  `Mux::connect` / `Mux::open_host` (the host's `Open`, handing the caller
  one end of a socket pair), the acceptor when it admits the `Open`. Per-direction credit
  starting at `INITIAL_WINDOW` (256 KiB), `Window` granted once half is
  consumed, a per-stream write queue drained by its own thread (a slow reader
  never blocks the frame reader or the keepalive), half-close via `Eof` on
  both sides with no `Close`. Overrunning credit or data after `Eof` closes
  the stream with a reason. The daemon dials each `Connect` on its own thread
  (5 s per address, loopback falls back to the other IP family). The engine on
  top: [../daemon/forwards.md](../daemon/forwards.md).

## RPC

One host → daemon request per `Open(RPC)` stream (the daemon's side:
[in-container.md](in-container.md#devsbd-daemon); the host's:
[bridges.md](bridges.md#host--container-requests)): concurrent requests
don't block each other, and cancelling one is closing its stream
(`Mux::close_stream`: `Close` with a reason, the local end shut, the
stream's `on_close` run). Inside the
stream's bytes (`Data` boundaries mean nothing) ride length-prefixed records
`u8 tag | u32 len | payload`, little endian, payload ≤ `MAX_RECORD` (1 MiB),
`src/devsbd/rpc.rs` (`Record`, `write_record`, `read_record`):

| tag | record | dir | payload |
|---|---|---|---|
| 1 | `call` | → | first record: JSON `{ "method", "params", "meta" }` (`params`/`meta` objects, optional) |
| 2 | `stdin` | → | raw bytes |
| 3 | `stdin-eof` | → | empty |
| 4 | `resize` | → | `u16 cols \| u16 rows` |
| 5 | `signal` | → | `u8 signo` |
| 6 / 7 | `stdout` / `stderr` | ← | raw bytes |
| 8 | `event` | ← | JSON object |
| 9 | `result` | ← | JSON object, last record on success |
| 10 | `error` | ← | JSON `{ "code", "message" }`, last record on failure |
| 11 | `exit` | ← | `u8 kind (0 code, 1 signal) \| i32 value`, last record of a process |

An unknown tag or a malformed payload is an error (both ends ship together).
Error codes every method can answer: `unknown-method` (no such method, or
not offered by this daemon), `bad-request` (the stream didn't start with
a well-formed `call`, or its params are wrong for the method) and `failed`
(the method ran and failed; `message` says why). `session.attach` adds
`no-session` (nothing to attach to), `detached` (another client took the
session over) and `version` (the session speaks another session wire);
`rpc::code` names them all. The daemon half-closes after its last record and
reads the caller's input until its `Eof`, so a caller sends `Eof` once it
has nothing more to send and the stream ends with both `Eof`s.

JSON payloads use a subset with its own encoder and decoder in `rpc.rs`
(`Json`, `Obj`, `encode_json`, `decode_json`), since the helper has no
serde: objects (keys unique, nested at most `MAX_DEPTH` = 4), strings,
`i64` integers, bools, `null`, and arrays of strings only. String maps
(`env`) are objects of strings (`Obj::map`). No floats, no arrays of
anything else; bigger data travels as byte records.

Discovery: `rpc.methods` answers `{"methods": ["rpc.methods@1", …]}`, the
methods this daemon offers with their versions. The `run.*` methods and
their records: [in-container.md](in-container.md#runs-over-rpc); the
`session.*` ones: [in-container.md](in-container.md#sessions-over-rpc).

## Line codecs

The helper has no serde, so records are hand-parsed lines `<key> <value>`,
one directive per line. Unknown keys, bad repeats or bad escapes are errors:
host and helper ship together, so a mismatch is a bug.

### `escape`

Values escape `\` → `\\`, newline → `\n`, NUL → `\0`; any other escape is an
error. Keeps one directive per line and NUL free as a separator.

### `bootfile`

`/run/devsandbox/boot`, written by the host, read by `devsbd boot`: `user`,
`cwd` (each at most once), `env K=V` (repeatable, ordered), `cmd` (one per
command, argv split on NUL). Output goes to `/run/devsandbox/boot.log`.

### `notify`

One record per file in `/var/lib/devsandbox/outbox` and per `NOTIFY` stream
(≤ `MAX_RECORD` 64 KiB): optional `kind` (`thread-put|thread-rm|thread-send|
thread-withdraw`; absent = plain notify), `at` (unix seconds), and per kind
`level`, `key`, `id`, `link`, `msg`, `body` (JSON, ≤ `MAX_SEND_BODY` 48 KiB
for sends). Thread records repeat the `key`/`id` pulled from `body` so the
outbox can coalesce (`Message::supersedes`) without parsing JSON. The host
replies `ok` (`REPLY_OK`) only after storing the record; anything else keeps
it queued. `devsbd/src/json.rs` is only a syntax checker that extracts those
top-level fields; the host owns the schema.

### `control`

Request: `op` (one of `ensure|ls|stop|rm|done|exec|run-ls|run-logs|run-wait|
run-rm|run-prune|branches|events|events-ack|events-follow|thread-ls`) plus
optional `sandbox`, `key`, `branch`, `env` (repeatable), `arg` (repeatable),
`id`, `offset`, `timeout`, `force`, `keep`, `ahead`, `ack` (repeatable),
`feed`; ≤ `MAX_REQUEST` 64 KiB. Which fields an op needs is the host handler's
call, not the codec's. Response: `status`
(`ok|denied|failed|no-host|usage`) and `body`; ≤ `MAX_RESPONSE` 1 MiB.
`events-follow` is the one held-open response: after `status ok` the host
streams raw JSON lines, a `{"kind":"ping"}` every 30 s idle, and
`{"kind":"replaced"}` when a newer follower for the same owner starts.
Long polls cap at `MAX_WAIT` (300 s).

Exit codes of the `devsbd` control verbs: 0 ok, 1 failed, 2 usage,
75 no host (`EX_TEMPFAIL`), 77 denied (`EX_NOPERM`).

Limitations: [status.md](../../status.md#helper)
