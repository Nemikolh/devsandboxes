# Dispatchers and child instances

A dispatcher is an instance whose sandbox declares `dispatcher`: from inside
its container it asks the host (over its helper's control stream) to
create, start, stop and remove **child instances** and run commands in
them. Read before changing `src/commands/dispatch.rs`, the child-op side of
`src/devsbd/control.rs`, or anything that treats children specially.
Field syntax: [config-toml-spec](../../../skills/config-toml-spec/SKILL.md).

Code: `src/commands/dispatch.rs` (`handle`, `handle_with`, `authorize`,
`check_fields`, `ensure_action`, `Ensure`, `ensure_args`, `replaced_env`,
`find_child`, `child_name`, `child_key`, `children`, `thread_children`,
`ls`, `branches`, `holder_rows`, `Holder`, `run_op`, `helper_argv`,
`rpc_request`, `rpc_answer`, `exec_timeout`, `Executor`, `Subprocess`,
`log_path`), `src/serve/helper.rs` (`shared`), `src/config.rs`
(`Dispatcher`), `src/devsbd/control.rs` (`Op`, `Status`, `EXIT_*`,
`valid_key`, `denied_env`), `src/devsbd/bridge.rs` (`dispatch_control`,
`writes_state`), `src/commands/run/mod.rs` (`RunExtras`, `orphan_check`,
`DISPATCHER_LABEL`), `src/commands/done.rs`, `src/state.rs`
(`Instance.dispatcher`, `extra_env`, `done`).

## Config

`dispatcher = { spawn = [...], max-instances = N }` (`Dispatcher`):
`spawn` lists sandboxes of the same config root it may instantiate (`"*"`
= any, `may_spawn`); `max-instances` caps its children, default 10
(`DEFAULT_MAX_INSTANCES`, `cap`). Like `autostart`/`inbox`, `dispatcher` is
stripped before `config_hash`: toggling it is never drift. Owning Inbox
threads is the separate `inbox = true`
([threads-and-feed](../inbox/threads-and-feed.md#ownership)).

## Authorization

Every request is checked against the config **as it is now** (`authorize`),
loaded from the config root recorded on the requester's instance (none
recorded → `denied`, `rebuild --force` records it):

- child ops need the requester's sandbox to declare `dispatcher`; a
  requester that is itself a child is `denied` (children can't be
  dispatchers);
- `ensure`/`branches` need the target sandbox in `spawn`, known, and (for
  `ensure`) not itself declaring `dispatcher`, whatever `spawn` says;
- `stop`/`rm`/`done`/`exec`/`run-*` reach only the requester's own children
  (`find_child`: another instance with that name → `denied`; none →
  `failed`; the key matching children of two sandboxes → `usage`, pass
  `--sandbox`);
- `ensure` of a new child is `denied` once `max-instances` children exist
  (all recorded ones, stopped included);
- `check_fields`: each op's allowed fields, key charset (`valid_key`), run
  and event id shapes, branch rules; `branch`/`env` only on `ensure`;
- `env` names in `denied_env` are `denied`: `PATH`, `HOME`, `SHELL`,
  `USER`, `ENV`, `BASH_ENV`, `IFS`, `CDPATH`, `PS4`, `PROMPT_COMMAND`,
  `SSH_AUTH_SOCK`, `TMPDIR`, `GCONV_PATH`, `NODE_OPTIONS`, `RUBYOPT`, and
  prefixes `LD_`, `DYLD_`, `GIT_`, `PYTHON`, `PERL5` (case-insensitive): a
  child's env reaches root execs on its next `start`. `run` re-checks
  both env and branch for `--dispatcher` runs.

`Status` → helper exit code: `ok` 0, `failed` 1, `usage` 2, `no-host` 75
(no host daemon reachable; answered in the container), `denied` 77.

## Children

A child is an ordinary instance named `<sandbox>-<key>` (`child_name`; key
`[a-z0-9][a-z0-9-]*`, ≤ 40), created by the hidden `devsandbox run
--dispatcher <instance_id> [--env K=V]…`. It records the owner's
`instance_id` in `Instance.dispatcher` and on the container label
`devsandbox.dispatcher`; it always gets a git worktree (even as the
folder's first instance), and a `--branch` is taken literally, never as a
pattern. An orphaned container carrying the same dispatcher label and no
worktree left is replaced on `run` (`orphan_check`), so a lost state entry
can't block a key forever.

Children outlive their dispatcher's stop/rm/rebuild; cleanup is the
dispatcher's job. Autostart never starts them, and their existence
suppresses autostart's `run` of their sandbox
([autostart](autostart.md#the-pass)).

## `ensure`

`ensure_action` decides, for `<sandbox>-<key>`:

| child | action | subprocess |
|---|---|---|
| not in state | `Run` (cap checked) | `run <sandbox> --name <name> [--branch B] [--env K=V]… --dispatcher <owner id>` |
| stopped | `Start` | `start <name>` |
| in state, no container | `Rebuild` (keeps its worktree, saved env) | `rebuild <name>` |
| running | `Nothing` | — |
| in flight or failed (`phase_view`) | `failed` with the phase | — |
| another owner's, or another sandbox's under that name | `denied` | — |

For an existing child, a given `env` replaces `extra_env` (replace, not
merge; `replaced_env`), saved before the subprocess, so every later
`devsandbox exec` applies it; the container's own env changes only on
rebuild. `branch` is ignored for existing children. Reusing a `done` child
clears its flag. After a run/start/rebuild, `wait_bridge` waits (bounded)
for the child's ssh-agent relay when execs there would use it. Answer:
`{"name","key","sandbox","state":"running","created","started","note"?}`.

## Ops

| op | does | answer |
|---|---|---|
| `ls` | the requester's children with live state | JSON array `{name, sandbox, key, state: running\|stopped\|missing, branch, done}` |
| `ensure` | above | object line |
| `stop`, `rm` | `devsandbox stop\|rm <name>` | `{name, key, state:"stopped"}` / `{name, key, removed:true}` |
| `done` | set `Instance.done` (idempotent, keeps the original time) | `{name, key, done:true}` |
| `branches` | branch holders of a spawn sandbox's repo, below | JSON array |
| `exec` | `run.start`, or `devsbd run start -- <argv>` in the child | the run id |
| `run-ls`, `run-logs`, `run-wait`, `run-rm`, `run-prune` | `run.ls\|logs\|wait\|rm\|prune`, or `devsbd run …` in the child | what `devsbd run …` prints |
| `events`, `events-ack`, `events-follow`, `thread-ls` | Inbox ops, gated on `inbox = true` | [threads-and-feed](../inbox/threads-and-feed.md#events) |

Lifecycle ops run as `devsandbox --in-process -C <root> <args>` subprocesses
(`Subprocess::devsandbox`): null stdin (prompts take their non-TTY
default), `GIT_TERMINAL_PROMPT=0`, the backend pinned, own process group,
killed after 30 min, output to `<data>/devsandbox/logs/dispatch-<unix>-<op>.log`
(a failure quotes its last 5 lines). Subprocesses, not in-process calls:
those commands print and stream docker/git output to inherited stdio, and
`--in-process` keeps them from lazy-starting a daemon.

Run ops need a running child with a helper, and reach it one of two ways,
chosen per op in `run_op`:

- **RPC**, when the child's daemon lists the op's method
  (`Executor::offers`: `HelperClient::offers`, through the running
  daemon's client, `serve::helper::shared`, as the dispatcher's key):
  `rpc_request` builds the params, `Executor::helper_call` collects the
  reply records, `rpc_answer` turns them into the body the exec path
  prints, and a helper's `error` into `in <child>: devsbd run: <message>`
  as the exec path quotes its stderr. `run.start` carries what the exec
  argv sets (workspace, remote user, `Instance::exec_env` +
  `SSH_AUTH_SOCK`), so `exec` goes this way only for a child with a
  recorded remote user; without one the runtime picks the image's `USER`,
  which the daemon doesn't know. Same caps and timeouts as exec (a timed
  out call is cancelled).
- **Exec** otherwise (an older helper without `RPC`, a non-root daemon for
  `run.start`/`rm`/`prune`, no bridge yet, no daemon): the child's own
  `devsbd run …` (`helper_argv`) like `devsandbox exec` would (remote user,
  workspace, `remoteEnv` + saved env), output captured and capped at
  `MAX_RESPONSE`, timeout 5 min (`run wait`: `MAX_WAIT` + 30 s). A usage
  error from an older helper is reported as "restart the child".

Runs themselves: [in-container](../helper/in-container.md).

`Executor` abstracts every side effect (liveness, subprocess, exec, RPC
calls, the host agent probe, bridge wait, state writes, git, store path), so `handle_with` is tested without a
runtime.

## Serialization

`ensure`, `stop`, `rm`, `done` write `state.toml` (`writes_state`); the
bridge's `dispatch_control` runs them under one host-wide lock for
declared dispatchers. Everything else, including waiting `run-wait` and
`events`, skips it. Transport: [bridges](../helper/bridges.md).

## Branches

`branches <sandbox> [--ahead]`: read-only, `host_git` `worktree list
--porcelain` on the sandbox's base repo (detached and prunable worktrees
skipped). Each branch has a `holder`: `child` (a worktree of this
dispatcher's child, with `instance`), `instance` (any other instance's,
any config root), `base` (the main checkout), `external` (a worktree
devsandbox doesn't know). With `ahead`, each local branch with an `origin`
counterpart gets `ahead` (commits not on `origin/<branch>`), and branches
held nowhere but ahead are listed as `local`. Never a host path. Git calls
are bounded (60 s).

## Done children

`Instance.done` (unix time) means finished but kept: only display and
dispatcher policy read it; `start` and autostart leave it set. Set by the
`done` op, by `devsandbox done|undone <instance>` and the `instances.done`
API (`commands::done::set_saved`), and by the dashboard on the thread's `child`
next to an Inbox thread's done/reopen (`d`/`u`) or a pressed `done: true`
action. `inbox.thread.done`/`reopen`
over the API or `devsandbox inbox` change only the thread, not the child's
flag. `instances.list` carries `thread_children` (owner id → key → child
name, `dispatch::thread_children`) so clients resolve a thread's `child`
without reading state.

Limitations: [status.md](../../status.md#automations)
