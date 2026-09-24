# ssh agent forwarding

## Goal

Make `git@github.com:…`-style (SSH) git operations work inside a sandbox without copying private keys in, by forwarding the host's ssh-agent — the same thing VS Code's Dev Containers do automatically. Split the socket (a filesystem fact, mounted at `run`) from the activating env var (`SSH_AUTH_SOCK`, injected per `exec`), so the mechanism matches how devsandbox already handles `remoteEnv`.

Rollout is staged by runtime, easiest first:

1. **Linux + docker/podman** — native, ~20 lines. This note's Steps 1–4.

2. **macOS + docker** (OrbStack / Docker Desktop) — same shape, different mount source (a synthesized "magic" socket). See _macOS + docker_.

3. **macOS + Apple `container`** — blocked today; see _Future: Apple container_.

4. **Windows (native)** — disabled; use WSL2. See _Windows (native)_.

## How it splits: mount at `run`, env at `exec`

Two independent pieces:

- **The socket** is bind-mounted when the container is created
  (`run_container`, `src/commands/run.rs:469`), at a **fixed container target**
  (proposed: `/run/devsandbox/ssh-agent.sock`) regardless of the host source.
  The mount _source_ is a devsandbox-owned **symlink**, not the raw
  `$SSH_AUTH_SOCK` path (Step 1): bind mounts re-resolve their source —
  following symlinks — at **every container start**, so `start` can re-point
  the link at the current agent (Step 4) and forwarding survives host agent
  rotation (reboot, re-login) without a rebuild.

- **`SSH_AUTH_SOCK`** is _not_ set via `run -e`. It is injected per `exec`, in
  `exec_argv` (`src/commands/exec.rs:57`), pointing at that fixed target.

This mirrors the existing `remote_env` handling: `exec_argv:75-78` already appends `instance.remote_env` as `-e` flags on every exec rather than baking them into container config. `exec_argv` is the single builder shared by the CLI `exec` (`exec_status`) _and_ the TUI integrated terminal (`src/tui/app.rs:1392`), so both get the var and can't drift.

### Consequence: which terminals see it — and why VS Code is already covered

`docker exec` inherits container-config env (image `ENV` + `run -e`) by default; each caller then adds its own `-e`. Because we inject via `exec -e` and **not** `run -e`:

- ✅ devsandbox's TUI integrated terminal — we build its argv via `exec_argv`.
- ✅ CLI `devsandbox exec …` and lifecycle execs (`onCreate…postAttach`,
  `postStart`, shell-rc wiring) — the CLI/TUI argv builder and the lifecycle
  argv builder consult one shared rule (`exec::ssh_auth_sock_env`), so they
  inject the same `SSH_AUTH_SOCK` and can't drift.
- ➖ **VS Code's integrated terminal** — not ours to serve, and it doesn't need us: VS Code already forwards the agent itself, at the application layer, with no docker involvement (see below).

This feature and VS Code's forwarding are **orthogonal**: two independent relays, each serving only the terminals it spawns. There is no conflict and no trade — VS Code covers its own terminals, devsandbox covers the TUI + CLI terminals VS Code never touches.

### How VS Code forwards (confirmed by inspecting a live container)

VS Code does **not** use a docker mount or container-config env — `docker inspect` shows no socket mount and no `SSH_AUTH_SOCK`, and a plain `docker exec` sees `SSH_AUTH_SOCK=` (empty). Instead:

- `vscode-server` (the node process VS Code injects into the container) creates its own proxy unix socket, e.g. `/tmp/vscode-ssh-auth-<session-uuid>.sock`, and sets `SSH_AUTH_SOCK` to it **in its own process environment** (observed on the extension-host pids).

- It relays ssh-agent protocol bytes over the **client↔server tunnel** VS Code already maintains (multiplexed over the `docker exec` stdio it uses to reach the server) to the host's real agent. The private key never leaves the host.

- Every VS Code terminal is **forked from `vscode-server`**, so it inherits that `SSH_AUTH_SOCK`. That is VS Code's "inject into the terminal environment" step — done by its server, invisible to docker.

That is why the integrated terminal works today with no socket in `docker inspect`, and why our `exec -e` path neither helps nor hinders it. (`remote_env` behaves the same way for devsandbox's own terminals; it too is independent of VS Code's environment injection.)

## Background (code landmarks)

- `src/commands/run.rs:469-540` — `run_container`: assembles `run` args
  (`-v`/`--mount` mounts, `-e` env, labels). The socket bind mount goes here,
  next to `git_companion_mount` (run.rs:380) — same "identical host/container
  path is a filesystem fact" pattern.

- `src/commands/exec.rs:57-82` — `exec_argv`: the per-exec `-e` injection
  point. `remote_env` (exec.rs:75-78) is the precedent to copy.

- `src/runtime/mod.rs:221` — `Backend::supports_file_binds()`: a socket is a
  file bind. docker/podman → `true`, Apple `container` → `false`. This is the
  gate that keeps Apple out of Steps 1–3 for free.

- `src/state.rs` — `Instance`: needs one new persisted field so `exec_argv`
  (which only sees `&Instance`) knows forwarding is active and at what target.

- `src/tui/app.rs:1392` / `src/tui/term.rs:28-33` — the TUI terminal wraps
  `exec_argv`'s output; no change needed, it inherits the new `-e`.

## Step 1 — decide + mount at `run` (Linux)

In `run_container`, before assembling `args`, compute the forward decision:

- Gate on `backend().supports_file_binds()` **and** `SSH_AUTH_SOCK` present in
  the host environment **and** the socket path exists on the host
  (`std::fs::metadata(...).is_ok()`). Any miss → no forwarding, no mount,
  silent (an absent agent is the common case, not an error).

- When forwarding: create a symlink
  `<data-dir>/devsandbox/agent/<instance-id>.sock -> $SSH_AUTH_SOCK`
  (data dir per `State::path`, `src/state.rs:82`) and push
  `-v <symlink>:/run/devsandbox/ssh-agent.sock`.

Why a symlink and not `$SSH_AUTH_SOCK` directly: the bind source is re-resolved (symlinks followed) at every `docker start`, so re-pointing the link between stop and start re-captures the current agent — the raw path would freeze the first session's socket into the container config forever (stale after any re-login). Per-instance links keep refresh races out (each `start` serves only
its own instance) and give cleanup clear ownership: `rm` deletes its own instance's link (next to its isolated-services reaping), and `gc` sweeps orphaned links whose instance id is gone from state — modeled on `gc_shell_history`, but with no confirm prompt: unlike history, a dangling symlink carries no data.

Keep the target constant so Linux and macOS converge on one container path.

## Step 2 — persist the target on `Instance`

`exec_argv` only receives `&Instance`, so it needs to know (a) that forwarding is on and (b) the container target. Add one field, e.g. `ssh_auth_sock: Option<String>` holding the container-side path (`/run/devsandbox/ssh-agent.sock`), set in `materialize` when Step 1 mounted the socket, `None` otherwise. Persisted in `state.toml` like the rest of `Instance`.

## Step 3 — inject at `exec`

In `exec_argv`, after the `remote_env` loop, when `instance.ssh_auth_sock.is_some()` push `-e SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock`. That single change lights up the CLI `exec`, lifecycle execs, and the TUI terminal.

## Step 4 — re-point the symlink at `start`

A host reboot or re-login rotates `$SSH_AUTH_SOCK` (new agent, new path) while the container config keeps the old bind source. Because the source is our symlink and binds re-resolve at every container start:

In `start_instance` (`src/commands/start.rs:42`), before `backend().run_checked(&["start", …])`: when `info.ssh_auth_sock.is_some()` and the host `$SSH_AUTH_SOCK` is present and exists, re-point the instance's symlink at it. Any miss → leave the link untouched, silent (same gate as Step 1): forwarding stays degraded for this run exactly as it would today, and the next `start` from a live session heals it.

Only `start` needs this; `run` creates the link fresh (Step 1) and `rebuild` goes through `run`.

### Caveats (Linux)

- **Socket ownership.** The forwarded socket is owned by the **host uid**. It
  only works if the container process runs as that uid — root, or a
  `remoteUser`/`containerUser` whose uid matches (rootful docker shares the
  host uid namespace). Host uid 1000 + container `vscode` uid 1000: fine.
  macOS uid 501, or a remapped rootless podman: the socket appears owned by a
  foreign uid and `ssh-add -l` fails with a permission error. Document, don't
  block.

- **Stale socket while running.** Steps 1+4 cover rotation across a
  stop/start. What they cannot fix: the agent rotating while the container
  **keeps running** — the kernel resolved the symlink at start, and re-pointing
  it on the host is invisible to a live mount. This mainly bites hosts reached
  via SSH agent forwarding, where sshd mints a new `/tmp/ssh-XXXX/agent.N` per
  connection. Accepted; workaround is `stop`/`start`. Agents at stable paths
  (systemd user agent, gnome-keyring, 1Password) never rotate and hit none of
  this. Alternatives that would close the gap, considered and deferred:
  - **Directory mount** — bind a devsandbox-owned _directory_ and keep the
    socket inside it; directory binds track contents live, so a recreated
    socket appears in running containers. Needs a host-side keeper for the
    socket (agent pinned via `ssh-agent -a`, or a relay process) — a
    daemon-shaped liability for a run-and-exit CLI.

  - **exec-stdio relay** (VS Code-style) — bridge the agent protocol over each
    exec session's stdio to a per-session in-container socket. Zero staleness
    by construction and needs no file binds. **This is now built and default**
    on unix whenever the embedded `devsbd` helper is present: the host side
    connects to its *current* `$SSH_AUTH_SOCK` per stream, so
    rotation-while-running no longer bites (`docs/sandbox-helper.md`). The mount path below is the
    fallback for `cargo install` builds (no helper) and non-unix hosts.

- **Not part of `config_hash`.** Forwarding is a runtime/environment fact, not
  config; it must not enter the drift hash or every login would look like
  drift.

## macOS + docker (Step, later)

The Mac host's real `$SSH_AUTH_SOCK` is a launchd socket under `/private/tmp/com.apple.launchd.*/Listeners` that **cannot** cross into the runtime's VM — sockets don't transmit across the hypervisor boundary. Docker Desktop and OrbStack work around this by synthesizing a **magic socket** the host filesystem doesn't actually contain: mount `/run/host-services/ssh-auth.sock` and set `SSH_AUTH_SOCK` to that same path, and the host agent is forwarded.

Implication for the split above: only the **mount source** in Step 1 changes — macOS+docker binds the literal `/run/host-services/ssh-auth.sock` directly, with **no symlink**: the magic path is stable by construction, so there is nothing to rotate and Step 4 is a no-op on this runtime. The fixed container target and the Step 3 `exec -e` injection are unchanged. Detection: `cfg!(target_os = "macos")` && backend is docker.

Extra wrinkles to handle when this lands:

- The magic path does **not** exist on the host, so the Step 1
  "socket exists" probe can't gate it. Mount unconditionally on macOS+docker
  (or gate some other way).

- It forwards whichever agent was in the environment when the runtime launched;
  no per-container control.

- It does not cover `docker build` (only `run`/`exec`), which devsandbox
  doesn't need here.

## Future: Apple `container`

Blocked today, two independent reasons:

1. `supports_file_binds()` is `false` — it binds directories only, not a
   socket. Step 1's gate already excludes it.

2. Same VM-boundary problem as Docker Desktop, with **no** documented
   magic-socket equivalent.

That in-container relay is now built: the embedded `devsbd` helper serves the agent over exec stdio, which needs no file binds and so covers Apple `container` too (`docs/sandbox-helper.md`). Not yet exercised on a Mac; the mechanism is runtime-agnostic (`exec -i`/`exec -d`), so it should work once tested. It also eliminates the rotation-while-running staleness on every runtime and replaces the mount path wherever the helper is embedded.

## Windows (native): disabled, relay deferred

**Status:** forwarding is compiled out on non-unix hosts. `ssh_agent_link` builds its symlink via a `#[cfg(unix)]` helper; `ssh_agent_forward` and
`ssh_agent_refresh` return early under `!cfg!(unix)`, so no mount, no `Instance.ssh_auth_sock`, and no warning. The two symlink tests are
`#[cfg(unix)]`. (Before this, `std::os::unix::fs::symlink` broke the Windows release build.)

**Why not just a Windows symlink** (`std::os::windows::fs::symlink_file`): it would compile, but nothing useful would be on the other end.

- Windows' own OpenSSH agent listens on a **named pipe**
  (`\\.\pipe\openssh-ssh-agent`), not a unix socket, and normally leaves
  `SSH_AUTH_SOCK` unset. So the Step 1 gate would already skip it.

- A pipe (or a Git-Bash/MSYS agent socket) can't be bind-mounted into a
  Linux container as a working unix socket.

- Docker Desktop's magic `/run/host-services/ssh-auth.sock` (see _macOS +
  docker_) doesn't forward on the Windows/WSL2 backend: `ssh-add -l` in the
  container comes back empty (still open as of 2026-09).
  1Password adds a hop (1Password → Windows pipe → WSL2 → container) that
  doesn't resolve out of the box either.

- Windows symlinks also need Developer Mode or admin.

**Supported workaround today:** run devsandbox **inside WSL2** (the Linux build). Bridge the Windows agent into WSL with `socat` + `npiperelay.exe`
(`socat UNIX-LISTEN:$SSH_AUTH_SOCK,fork EXEC:"npiperelay.exe -ei -s //./pipe/openssh-ssh-agent"`). From devsandbox's point of view that's the plain Linux case: Steps 1–4 apply unchanged.

**Future native path (not started):** an agent relay owned by devsandbox:

1. Host side: open the named pipe. That needs a Windows pipe client
   (`tokio::net::windows::named_pipe` or a small winapi crate), and neither
   is a dependency today.

2. Transport: move agent-protocol bytes over a `docker exec -i` stdio stream
   into the container.

3. Container side: a tiny listener (`socat` or an injected helper) serves a
   unix socket at `SSH_AGENT_TARGET`. Step 3's `exec -e` injection stays the
   same.

The container side (2–3) is the built `devsbd` relay (`docs/sandbox-helper.md`); Windows only lacks the **host side** — a named-pipe client feeding the bridge's stdio, since `host_agent()` reads a unix `$SSH_AUTH_SOCK` today. Add that pipe client and relay mode lights up natively (no mount, no `start` re-point, no link cleanup). Until then, the WSL2 workaround above is the supported path.

## Appendix — how git chooses `SSH_AUTH_SOCK` vs `GIT_ASKPASS`

They never compete; the **remote URL scheme** selects the transport, and each transport uses at most one:

- `git@host:…` / `ssh://…` → SSH transport → git execs `ssh`, which uses
  `SSH_AUTH_SOCK`. `GIT_ASKPASS` is never consulted.

- `https://…` → HTTPS transport → git walks the `credential.helper` chain;
  `GIT_ASKPASS` is only the fallback prompt when no helper returns a
  credential. `SSH_AUTH_SOCK` is never consulted.

Having both set in the container is harmless. To see which a repo will use:

```
git -C <repo> remote -v                              # scheme picks the mechanism
git config --show-origin --get-all credential.helper # HTTPS helper chain
GIT_TRACE=1 GIT_SSH_COMMAND='ssh -v' git fetch       # watch the choice
```
