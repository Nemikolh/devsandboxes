# Scheduled feature updates

The daemon keeps every feature update current
([images-and-features](../instances/images-and-features.md#running-an-update)):
each one a config root declares runs as `devsandbox update <name>` once
its interval has passed. Read this before changing when updates run, how
they're claimed or how a failure is reported; the update itself is
[update-cli.md](../../reference/update-cli.md).

Code: `src/serve/updates.rs` (`pass`, `due`, `claim`, `interval`, `Env`,
`Live`, `CHECK_EVERY`, `RETRY`, `TIMEOUT`, `INBOX_OWNER`),
`src/serve/host.rs` (`updates_loop`, `Host::updating`),
`src/commands/update.rs` (`declared`), `src/state.rs`
(`State.updates`, `UpdateRecord`).

## When

A `serve-updates` thread (`host::updates_loop`) runs `updates::pass` when
the daemon starts, then every `CHECK_EVERY` (1 h). A pass walks the
config roots recorded on instances (`host::config_roots`). For each root
(`Live::declared`): the config loads (else a `serve.log` line and the root
is skipped), its sandboxes' enabled features are resolved
(`commands::update::declared`, the verb's own discovery; a sandbox that
doesn't resolve is a log line), and of the updates they declare only
those of a sandbox with an instance in that root count: an instance means
its features image was built, which the update runs from.

An update's interval is the config's `updates.<name>` (`"never"`: not
run), else the feature's `every` (`updates::interval`). It's due
(`updates::due`) once the interval has passed since its last success and,
while its last attempt failed, `RETRY` (6 h) since that attempt.

## The record and the claim

`State.updates[<project id>][<name>]` is an `UpdateRecord`: `ok`, the
last success (written by the verb, whoever ran it), and `tried`, the
daemon's last claim. An attempt is failing while `tried` is newer than
`ok`; a manual `devsandbox update` that succeeds ends that.

Before running, the pass claims the update under the state lock
(`Live::claim` → `updates::claim` in `State::update`): still due, `tried`
is set to now, so of two daemons racing on one `state.toml` (one handing
off to its successor) only one runs it. A claim
that never finishes (the process killed) reads as a failure and is
retried after `RETRY`.

Before the first claim of a pass the runtime must answer
(`Live::ready`: a container list); otherwise the pass ends with a log
line and nothing is claimed. Apple `container` is refused there, like by
the verb.

## The run

One at a time, each due update runs as
`devsandbox --in-process -C <root> update <name>` (`Live::run`): its own
process group, null stdin, stdout and stderr appended to
`logs/update-<unix>-<name>.log` next to `state.toml`, pinned to the
daemon's runtime (`RUNTIME_ENV`), killed with its group after `TIMEOUT`
(15 min, `bounded::wait_until`, `bounded::kill_group`).

A failure (non-zero exit, timeout) is a `serve.log` line with the log's
last line and path. The first failure of a streak (the record before the
claim wasn't failing) is also an Inbox notice: an error record keyed
`update-<name>` from the owner `INBOX_OWNER` (`devsandbox:updates`, never
an instance id, so no `rm` archives it), through `inbox::ops::sink`.

## Holding the daemon

`Host::updating` is 1 while a pass runs (from before the thread starts),
the `updates` holder (`idle::Holders`), so an idle exit never kills an
update. Dropping the `Host` (shutdown, handoff) wakes the thread's sleep
and kills a running update (`Live::stopped` is the `wait_until` stop);
the pass then ends without a notice, the claim kept.
