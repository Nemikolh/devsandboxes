# Automations: user guide

Sandboxes that come up on their own after a boot, tell you when they need you, and spawn their own child instances to do work. devsandbox provides the plumbing only; what to watch and when to act lives in a script you write. Design and internals: [`docs/automations.md`](automations.md).

Everything here except `autostart = true` needs the embedded `devsbd` helper (release archives, npm package, local builds after `scripts/build-devsbd.sh`; not `cargo install`). Inside a sandbox it is on `PATH` as `devsbd` (a best-effort `/usr/local/bin/devsbd` symlink; the binary is `/run/devsandbox/bin/devsbd`).

## `autostart`

```toml
[sandbox.triage]
autostart = true         # devsandbox starts it once per boot
# autostart = "runtime"  # the container runtime restarts it on boot
```

**`true`**: once per host boot (per config root), the first dashboard launch, or the first `devsandbox run` / `devsandbox start` (after it finishes), starts every stopped instance of each `autostart` sandbox through the full `start` path (services, helper, `postStartCommand`). A sandbox with no instance yet gets one (`run`). Dispatcher-owned children are skipped (see below). Other commands (`ps`, `stop`, `status --json`, …) never trigger it. An instance you stop afterwards stays stopped until the next boot. If the runtime isn't reachable yet (Docker Desktop still starting, `container system start` not run), the pass is skipped and retried on the next trigger.

**`"runtime"`**: the same pass, plus the container is created with `--restart unless-stopped`, so docker/podman bring it back at boot with no devsandbox process running:

- docker: works as soon as the daemon starts on boot/login.
- podman: only with `podman-restart.service` enabled (`systemctl --user enable podman-restart.service`); devsandbox never installs units and warns at `run`.
- Apple `container`: no restart policy; warns and behaves as `true`.

The runtime restarts only the container command, so new containers carry a boot hook: on every container start it brings back the `devsbd` daemon and re-runs `postStartCommand` (as `remoteUser`, in `workspaceFolder`, with `remoteEnv`), appending its output to `/run/devsandbox/boot.log`. For these instances `devsandbox start` no longer runs `postStartCommand` from the host (`run` still does, at create). Containers created before the hook existed warn on `start`: recreate them with `devsandbox rebuild --force <instance>`. Host-side things (ssh-agent relay, port forwards, notify delivery, the control API) resume when a devsandbox process connects.

Flipping `autostart` is not drift: `start` updates the restart policy of an existing container in place (a podman too old for `update --restart` warns to `rebuild --force`).

## `devsbd notify`

Available in every sandbox, no opt-in:

```
devsbd notify [--level info|warn|error] [--link URL] [--key K] [--] <msg>...
```

- Queued first: each call writes a record to a durable outbox in the container (`/var/lib/devsandbox/outbox/`, survives container restarts) and succeeds even with no host attached.
- Delivered while the **dashboard is open**: it shows up in the Inbox tab (`4`; unread count in the title, a yellow `✉N` on the instance row) and as a desktop notification (`notify-send` on Linux, `osascript` on macOS; skipped silently when missing). One-shot CLI commands don't deliver; the queue waits for the next dashboard.
- `--key` threads per instance: a newer notification with the same key becomes the thread's single row, with the older ones folded under it (`→` to show the trace), so a script re-reporting "PR 123 conflicted" every poll doesn't spam.
- `--link` (http(s) only) opens with `enter` in the Inbox. With several instances sending, the Inbox groups them per instance. `d` dismisses a history row, a thread or an instance's group; `D` clears the inbox. History is saved in `inbox.toml` next to `state.toml` and survives dashboard restarts; dismissed entries are removed from it.
- Limits: desktop popups are rate-limited per instance (a burst of 3, then one per 10 s) and a keyed notification repeating one popped in the last minute doesn't pop again; the Inbox still gets every one. The Inbox keeps 200 notifications per instance (thread history included), so a noisy instance drops its own oldest, not others'.
- Flags are recognized anywhere before `--`; everything after `--` is message.

## Dispatchers

A dispatcher is an ordinary sandbox that declares `dispatcher`. Its instances may create and manage *child* instances; every other sandbox is refused.

```toml
[sandbox.pr-dispatcher]
extends = "base"
folder = "../pr-dispatcher"      # its own repo: loop script, AGENTS.md, skills
autostart = "runtime"
dispatcher = { spawn = ["web"], max-instances = 10 }
postStartCommand = "nohup ./babysit-loop.sh >babysit.log 2>&1 &"

[sandbox.web]                    # children are ordinary instances of it
extends = "base"
folder = "../web"
```

- `spawn`: sandboxes of this config root the dispatcher may instantiate; `"*"` allows every non-dispatcher sandbox of the root, which hands the dispatcher their mounts, docker socket and other privileges: list names unless you mean that. Omitted = nothing. Children can't be dispatchers: a sandbox that declares `dispatcher` is never spawnable, even when named.
- `max-instances` (optional, default 10): cap on the dispatcher's children, stopped ones included.
- Both are re-read on every request, and neither marks instances drifted.

**Background the loop.** Lifecycle commands block the `run`/`start` that runs them, so a long-running loop in `postStartCommand` must detach itself: `nohup ./loop.sh >loop.log 2>&1 &`. The output lands in the workspace (gitignore it).

### Control commands

Run from inside a dispatcher instance:

```
devsbd ensure <sandbox> --key <key> [--branch B] [--env K=V]...
devsbd ls
devsbd stop <key> [--sandbox S]
devsbd rm <key> [--sandbox S]
devsbd exec <key> [--sandbox S] [--detach] -- <cmd>...
devsbd run ls <key> [--sandbox S]
devsbd run logs <key> <id> [--sandbox S] [--follow]
devsbd run wait <key> <id> [--sandbox S] [--timeout SECS]
devsbd run rm <key> <id> [--sandbox S] [--force]
devsbd run prune <key> [--sandbox S] [--keep N]
```

- `ensure` is idempotent: creates `<sandbox>-<key>` if missing, starts it if stopped, recreates it if its container is gone, and prints the instance name. `--branch` applies only at creation (ignored for an existing child). `--env` values are recorded and kept when the child is rebuilt or its container recreated; on an existing child, `--env` replaces the recorded set with exactly the given one (a variable left out is dropped; no `--env` keeps it). Every command devsandbox runs in the child uses the recorded values from then on (`devsandbox exec`, the dashboard's terminals, `devsbd exec` runs, lifecycle commands), overriding a `remoteEnv` entry of the same name. The container's own environment is only set when it's created, so processes already running, PID 1, and a hand-run `docker exec` keep the old values until `devsandbox rebuild`. `--branch` names the branch of the child's worktree: an existing local branch is checked out as is, one only on `origin` (e.g. a PR head) is fetched and checked out as a local branch tracking `origin/<branch>` (so a plain `git push` updates the PR), anything else is created from the repo's default base. A branch already checked out elsewhere (the base checkout, another instance) is an error naming where. `rm` only offers to delete a branch the child created. The name is taken literally (no `${…}` substitution, unlike the sandbox's `worktree-branch` pattern) and must be 1–200 chars of `[A-Za-z0-9._/-]`, not start with `-`, `/` or `.`, not end with `/` or `.`, and contain no `..`, `//`, component starting with `.` or ending in `.lock`; anything else is a usage error (exit 2). Without `--branch` the child's branch comes from `worktree-branch` (default `sandbox/${instance}`). `--env` may not set variables that steer what runs (denied, exit 77): `PATH`, `HOME`, `SHELL`, `USER`, `ENV`, `BASH_ENV`, `IFS`, `CDPATH`, `PS4`, `PROMPT_COMMAND`, `SSH_AUTH_SOCK`, `TMPDIR`, `GCONV_PATH`, `NODE_OPTIONS`, `RUBYOPT`, and anything starting with `LD_`, `DYLD_`, `GIT_`, `PYTHON` or `PERL5` (matched case-insensitively).
- `ls` prints a JSON array of this dispatcher's children: `name`, `sandbox`, `key`, `state` (`running` | `stopped` | `missing`), `branch`.
- `stop` / `rm` / `exec` / `run …` take the key; `--sandbox` disambiguates a key used under two sandboxes.
- `exec` starts a tracked *run* in a running child (`ensure` it first), as the child's `remoteUser` in its workspace with its `remoteEnv`. With `--detach` it prints the run id and returns; without, it prints `devsbd: run <id>` to stderr, streams the output, and exits with the run's code (`killed N` → 128+N, `lost` → 1).
- `run ls` prints one line per run, oldest first, for the newest 50 runs only: `<id> <state> <started, UTC> <argv…>`, argv cut at 200 characters (ending in `…`). Runs whose files are oversized or not regular files are left out. `run logs` prints the output so far (`--follow`: until the run ends). `run wait` prints the final state, or `running` once `--timeout` expires; it exits 0 either way.
- `run rm` deletes one run (its output included). A `running` run is refused (exit 1) unless `--force`, which first kills the run's command with everything it started (SIGKILL to its process group); the run ends as `killed 9`. `run prune` deletes the child's finished (`exited`/`killed`) and `lost` runs except the newest `--keep N` of them (default 0), never a running one, and prints `removed <count>`. "Newest" is by start time to the second; runs started in the same second are ordered arbitrarily.
- Run states: `running`, `exited N`, `killed N` (signal), `lost` (its supervisor died without recording an end, e.g. the child restarted). Runs are kept in the child under `/var/lib/devsandbox/runs/<id>/` until `run rm`/`run prune` deletes them (or the child is removed or rebuilt). The dashboard shows a child's recent runs as dim `run` rows under its processes.

Exit codes:

| code | meaning |
|---|---|
| 0 | ok |
| 1 | failed (the message says why; host-side details in the log below) |
| 2 | usage error, or an ambiguous key (pass `--sandbox`) |
| 75 | no host connected: the dashboard isn't open (or the helper daemon isn't running). Retry later. |
| 77 | denied: not a dispatcher (or a dispatcher's child), sandbox not in `spawn` or itself a dispatcher, not this dispatcher's child, `max-instances` reached, a denied `--env` name |

Control is served only while the devsandbox **dashboard** is open (two open dashboards are fine: each request goes to one of them). Nothing is queued: a script must retry on 75.

### Children

- Named `<sandbox>-<key>` (key: lowercase letters, digits, `-`, starting with a letter or digit, at most 40 chars), e.g. `web-pr-123`, so you can `devsandbox exec -it web-pr-123 zsh` or attach VS Code to it.
- Always on their own git worktree, even when the folder isn't otherwise in use.
- Labelled with their owner and recorded in `state.toml`; a dispatcher only reaches its own children. The dashboard marks them with a dim `⇠ <dispatcher>` suffix, or `(orphan)` once the dispatcher is gone.
- **Kept** when the dispatcher is stopped, removed, or rebuilt. Cleaning up is the script's job (`devsbd rm`); it should remember which keys it manages and can reconcile against `devsbd ls`. Orphans are removed by hand with `devsandbox rm`.
- Each host-side operation runs as a `devsandbox` subprocess logging to `<data-dir>/devsandbox/logs/dispatch-<unix>-<op>.log` (next to `state.toml`, e.g. `~/.local/share/devsandbox/logs/`).
- The boot pass never starts or creates children: the dispatcher `ensure`s them when it needs them. A sandbox whose only instances are children gets no extra instance either. Exception: children of an `autostart = "runtime"` sandbox carry `--restart unless-stopped`, so docker/podman bring back the ones that were running at shutdown (the runtime's rule, not devsandbox's).

## Patterns

devsandbox takes no stand; pick per use case.

- **One instance per item**: `--key pr-123`. Easy to inspect (the PR has its own container, worktree, agent session). A later stage calls `ensure` with the same key and finds the same instance. Retire with `stop` until the item's last stage is done, then `rm`.
- **Worker pool**: keys `worker-1`, `worker-2`, …; the script assigns items to workers. Resources stay bounded regardless of item count; per-item memory is the script's concern.
- **`stop` vs `rm`**: `stop` keeps the container, worktree and any uncommitted work; `rm` deletes container, worktree and state entry.
- **Idle children**: `stop` a child between runs and `ensure` it before the next `exec`, so nothing runs while nothing needs doing.

## Sample: PR babysit dispatcher

Polls open PRs, runs a cheap check per PR, and starts an agent in the PR's own child only when needed; asks for a human with `devsbd notify`. POSIX sh, `gh` authenticated in the dispatcher.

```sh
#!/bin/sh
# babysit-loop.sh, started from postStartCommand:
#   nohup ./babysit-loop.sh >babysit.log 2>&1 &
set -u
REPO=owner/web
STATE=.babysit            # key -> run id; gitignored
mkdir -p "$STATE"

# Run a control command, retrying while no dashboard is open (exit 75).
ctl() {
  while :; do
    "$@"; rc=$?
    [ "$rc" -eq 75 ] || return "$rc"
    sleep 60
  done
}

# Cheap check, no agent: conflicted or behind its base.
needs_attention() {
  case $(gh pr view "$1" -R "$REPO" --json mergeStateStatus -q .mergeStateStatus </dev/null) in
    DIRTY|BEHIND) return 0 ;;
    *) return 1 ;;
  esac
}

while :; do
  gh pr list -R "$REPO" --json number,headRefName -q '.[] | "\(.number) \(.headRefName)"' >"$STATE/open"

  while read -r num branch; do
    key="pr-$num"
    # An agent already on it?
    if [ -f "$STATE/$key" ] &&
       [ "$(ctl devsbd run wait "$key" "$(cat "$STATE/$key")" --timeout 0)" = running ]; then
      continue
    fi
    needs_attention "$num" || continue
    ctl devsbd ensure web --key "$key" --branch "$branch" --env PR_NUMBER="$num" >/dev/null || {
      devsbd notify --level error --key "$key" "PR $num: cannot start a child (exit $?)"
      continue
    }
    ctl devsbd run prune "$key" --keep 5 >/dev/null   # old runs' logs
    id=$(ctl devsbd exec "$key" --detach -- sh -c \
      'zidane -p "Rebase this PR on its base, fix conflicts and failing checks, push. If a human decision is needed, run: devsbd notify --level warn --key pr-$PR_NUMBER <why>"') || continue
    echo "$id" >"$STATE/$key"
  done <"$STATE/open"

  # Retire children whose PR is no longer open.
  for f in "$STATE"/pr-*; do
    [ -e "$f" ] || continue
    key=${f##*/}
    grep -q "^${key#pr-} " "$STATE/open" && continue
    ctl devsbd rm "$key" && rm -f "$f"
  done

  sleep 300
done
```

Notes: `--branch "$branch"` puts the child's worktree on the PR head, tracking `origin/<branch>`, so the agent's `git push` updates the PR. That only works for PRs whose head is on `origin`: a fork's branch isn't, and would come out as a fresh branch of that name off the default base. `--env` values are set when the child is created and kept across its rebuilds; a later `ensure` with different values replaces them for everything devsandbox runs in the child from then on (already-running processes keep the old ones until a rebuild). `devsbd notify` is also available inside the children, so the agent can ask for help directly.

## Known limitations

- **Non-root `containerUser` and the boot hook**: the hook runs as the container's user, so it can't start the daemon (or switch to `remoteUser`) when that user isn't root. The daemon comes back on the next host `start`.
- **Supplementary groups**: when the boot hook switches from root to `remoteUser`, the user's supplementary groups are dropped.
- **Zombies**: one zombie process per container start unless the sandbox sets `init = true`.
- **Runs are kept until deleted**: `/var/lib/devsandbox/runs/` grows until the child is removed or rebuilt; clean up with `devsbd run prune <key> [--keep N]` (or `run rm` for one run).
- **Host-side limits**: a dashboard serves at most 8 notify/control requests per instance at once (more are refused and retried, or fail with "host disconnected"). A dispatched `ensure`/`stop`/`rm` is killed after 30 min (with its docker/git children) and answers `timed out after 30 min`; a run command (`exec`, `run ls|logs|rm|prune`) after 5 min, `run wait` after its 300 s cap plus 30 s; run-command output over 1 MiB is an error, not a cut-off answer.
- **The dashboard must be open** for notifications to be delivered and for control commands to succeed (exit 75 otherwise). Notifications queue; control requests don't.
- **Host git refuses repos with command-running config**: every sandbox can write the repo's `.git` (worktree instances share the base repo's), so host git — `worktree add`/`remove`, `fetch`, `branch -D`, which `ensure` and `rm` trigger — runs with hooks and `core.fsmonitor` disabled and first checks the repo's local config files (`.git/config`, `config.worktree`s) against an allowlist of keys that can't run commands. Anything else (`core.sshCommand`, `filter.*`, `include.path`, `credential.*`, an `ext::` remote URL, …) or an `objects/info/alternates` file makes the command fail with `refusing to run git on <repo>: … sets <key>`. A sandbox may have written it: review the key and remove it (`git config --unset <key>` after checking it is yours); put personal settings in `~/.gitconfig`, which isn't checked.
