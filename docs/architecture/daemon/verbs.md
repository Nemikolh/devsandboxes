# Verbs as requests

`run`, `rebuild`, `start`, `stop`, `rm`, `service rebuild` and `gc` are
API methods: the CLI and the dashboard send them to the daemon, which runs
each as a subprocess and streams it back (`gc` only from the CLI, for an
offered fleet worker: below). Read this before changing how a verb
is launched, cancelled or survives a daemon exit.

Code: `src/serve/verbs.rs` (`is_verb`, `*Params`, `parse`, `argv`,
`serve`, `Runner`, `InFlight`, `Tracker`), `src/serve/verbs_client.rs`
(`from_command`, `run`, `prepare`, `send`, `in_process`, `lost`,
`caller_env`), `src/serve/daemon.rs` (dispatch, `draining`, the start's
`InFlight::adopt`), `src/serve/api.rs` (`instances_cancel`),
`src/commands/run/cancel.rs`, `src/main.rs` (`--in-process`). Wire shapes:
[api.md](../../reference/api.md#verbs).

## Why a subprocess

The daemon turns typed params into an argv (`argv`: `--in-process -C
<root> <verb> --flag=value … -- <target>`; the target always goes in, so
no pick prompt can come up) and spawns this binary, re-resolved per spawn
(`exe::devsandbox`) so verbs run an upgraded binary. A verb that panics or
hangs takes down one request, not the daemon, and a handoff doesn't wait
for a build. `--in-process` makes the child take `main`'s local arms and
never lazy-start a daemon (`client::set_in_process`) or route to a machine.

The child gets:

- the request's `env` and nothing of the daemon's (`${localEnv:…}`,
  `SSH_AUTH_SOCK`, `GIT_*` must be the caller's); `env` is required;
- its own process group (a cancel reaches docker and git under it), null
  stdin, `GIT_TERMINAL_PROMPT=0`;
- stdout/stderr to spool files `<data-dir>/devsandbox/logs/verbs/<pid>.out|.err`
  (0600), not pipes: a daemon exiting mid-verb would EPIPE a pipe;
- for `run`/`rebuild`/`start`, an optional `tty` size its build log tees
  through (coloured output, docker's TTY progress).

## The request loop

`verbs::serve` runs on the connection's thread until the answer: it copies
new spool bytes into `output` notifications (`stream`, `data`; UTF-8 kept
whole across reads), re-reads `state.toml` every 250 ms for `progress`
(`instance`, `phase`: every instance whose `progress.pid` is the child's,
`Tracker`), and reaps the child. Notifications carry the request's id as
`request`. The answer is `{instance, exit: 0}`; a non-zero
exit is error `failed` with `data {exit, instance}` and the message from
the child's last `Error:` line plus its `Caused by:` chain (else its last
non-empty stderr line). `rm` with `ask` answers `confirm` with the
branch question, having removed nothing.

## Detach and hang-up

- **Detached verbs** (`run`, `rebuild`, `start` with `detach`) answer
  early (`{instance, detached: true, record?}`): `run` and a one-instance
  `rebuild` once an instance's `progress.pid` is the child's, `start` and
  `rebuild --all` right after the spawn (`DetachAt`). The verb runs on.
- **Held verbs** (`stop`, `rm`, `services.rebuild`, `gc`) always answer at the
  exit; `detach` only means a hang-up doesn't cancel them.
- **Hang-up** before the answer SIGINTs the group (the verb rolls back),
  unless `detach`, or the client merely stopped reading for the write
  timeout (a paused `run | less`), or the daemon is draining (its read-side
  shutdown looks the same; `Shared::draining` tells them apart).

The dashboard sends `run`/`rebuild`/`start` detached and
`stop`/`rm`/`services.rebuild` with `detach` (a quit mustn't interrupt
them), each on its own connection. The CLI sends `detach` only for `-d`.

## Cancel

`instances.cancel {instance}` SIGINTs the instance's in-flight
`run`/`rebuild` by its recorded `progress.pid` (`cancel::cancel_instance`),
whoever started it. The CLI's Ctrl-C sends it on a second connection so
the rollback still streams; with nothing to name yet (or refused) it hangs
up instead. A second Ctrl-C kills the CLI (a hang-up too). Exit after a
cancel is 130.

## Surviving the daemon: spool and adoption

Verbs in flight don't hold the drain and aren't cancelled by it. A client
streaming one sees the connection end without an answer (`Streamed::Lost`):
the CLI says the verb keeps running and, for a build with a known
instance, follows its build log and exits by its phase (`lost`).

The successor, holding the start lock, sweeps the spool dir before serving
(`InFlight::adopt`): a live pid's files make it *adopted* (a `verbs`
holder, polled with `kill(0)` every accept tick, never waited on), a dead
one's files and dead daemons' temp files go. Adopted verbs can still be
cancelled by `instances.cancel`, but nobody streams their output.

## The CLI side

`verbs_client::from_command` picks the verbs that go to the daemon
(`run` with the dispatcher's hidden `--env`/`--dispatcher` and `rm
--fail-on-ask` stay local). `gc` is one only with `--machine` on an
offered fleet worker (`CliVerb::Gc`: `machines::cli::route` hands only
that one on; an ssh machine's runs `devsandbox gc` there over ssh): one
request per config root its rows name there (`machines::cli::config_roots`,
as its `ls`), each after its `==> m:<root>` line. A local `gc` runs in
the CLI's process. Sent as a verb, `gc` has no TTY: without `force` its
shell-history prompt answers no, so those files stay. Every prompt happens on the user's TTY before
the send, since the child has no stdin: sandbox pick and example config
(`sandboxes.list`), instance pick (`instances.resolve`), the identity
prompt (below), rm's branch question (`ask` → `confirm` → resend). The
request env is the CLI's whole env (`caller_env`, non-UTF-8 vars dropped).

No daemon reachable, or one too old for verbs: one `warning:` line and the
verb runs in the CLI's process as `--in-process` (`Done::InProcess`); `-d`
refuses instead (`detach_needs_daemon` in `main.rs`). A verb aimed at a
machine has no fallback.

### The identity prompt

A config written for root in the container breaks under `identity =
"host"` (`config_identity`, the findings). Once `prepare` knows the
target of `run`/`start`/`rebuild` (the sandbox from `run_target`, an
instance's from `instances.resolve`'s `sandbox`, every sandbox of the root
for `--all`, every root of the machine for `--all --machine`), it calls
`config.findings {root}` where the target lives (with `machine` for a
machine's) and keeps the un-acked findings of those sandboxes
(`verbs_client::identity::prompt`). If any, on a TTY and not under `run
--json`, one prompt lists each one's edit line (or "fix by hand") per
config file and asks `[u]pdate / [c]ontinue as is (don't ask again) /
[s]kip this time` (Enter skips):

- `u`: `config.migrate {root, ids, fileHash}` with the findings that have
  an edit, then `updated devsandboxes.toml (N edits); now drifted: a, b →
  devsandbox rebuild <name>`. A `conflict` (the file changed meanwhile)
  stops the verb, to be re-run; `unsupported` (YAML) prints the edits to
  make by hand; any other error is a `warning:`. A file none of whose
  findings has an edit gets `<file>: nothing to update automatically;
  fix the entries above by hand` instead, and no `config.migrate`;
- `c`: `config.ack`, but a root user in a TOML config, which
  `config.migrate {keep}` keeps in the file as `identity = "image"`;
- `s`: nothing.

The verb is then sent, on the config as it now is. Nothing here blocks a
verb otherwise: a root whose findings can't be read is left out. The
dashboard asks the same through modals, over the same fns
([../dashboard/tabs-and-actions.md](../dashboard/tabs-and-actions.md#identity-modals)).

With no daemon the same prompt runs before the in-process verb, its calls
answered in this process by the API's own table (`identity::local_call`:
`serve::api::call`, `state.toml` at `State::path`). What it asks about is
what's known before the verb's own prompts (`identity::local_asks`): `run`
with a sandbox named, the sandboxes an instance name can mean, the root
for `--all`.

Everywhere else `run` and `rebuild` print one `warning:` line on stderr
from `run::materialize` when un-acked findings apply to the sandbox
(`config_identity::warning`): off a TTY, `run --json` (stdout untouched),
the dispatcher's `--in-process` subprocesses, autostart, and after a
skipped prompt too. Autostart runs in the daemon when it starts (`serve::host`),
before any prompt could be shown: it mostly starts existing containers,
which keep the model they were created with until rebuilt.

## Other `--in-process` callers

The dispatcher's child ops (`commands::dispatch`) run `devsandbox
--in-process -C <root> …` subprocesses, logged, from inside the daemon.

## Verbs on a machine

A verb request carrying `machine` runs over an ssh of its own for the
request's life, ids rewritten, with no spool here; it's a `verbs` holder
while it runs, and a drain cuts it like a hang-up (cancelled there unless
`detach`). → [machines.md](machines.md)

## A placed `run`

A `run` request without `machine` or `here` is placed first
(`daemon.rs` `placed_run`, `fleet::placed`): when an offered fleet worker
takes work and scores above this host, the sandbox's code moves there and
the request goes on as a machine verb would (`remote_verb`, `root`
rewritten to the worker's copy, `here` set), its `progress` and answer
tagged with the worker (`placed::Tagged`, `placed::tag_answer`) so the
CLI prints the name qualified and sends its Ctrl-C there. Otherwise it is
the local verb above, maybe after a `note:` line. Only a hub has offered
workers, so elsewhere this is one check of the machines' state and
nothing more. → [fleet.md](fleet.md#placement)

Limitations: [status.md](../../status.md#daemon)
