# Plan: cli-proxy — credential-isolated gh / sentry-cli via the `tools` service

## Goal

Sandboxes can run `gh` and `sentry-cli` without ever holding the credentials.
Tokens live only in the shared `tools` service (`scope = "global"`); each
sandbox gets shim CLIs at `/usr/local/bin/{gh,sentry-cli}` that forward the
invocation to a server in `tools` and stream the result back. This is the
ssh-agent pattern applied to CLIs: forward *operations*, not secrets — plus
one thing ssh can't give us: a server-side policy that filters subcommands
(`gh pr *` yes, `gh auth token` no).

Best-effort by design:

- A sandbox can still *do* whatever the proxied CLI allows (confused deputy);
  it just can't exfiltrate the token. The policy table narrows the blast
  radius.
- Remote CLI file access only works for paths inside workspaces (mounted into
  `tools` at identical paths). Anything else (`/tmp/foo.patch`) fails with a
  clear "path not shared with tools" error. Accepted limitation.
- In-sandbox coding agents are explicitly out of scope: their CLIs must keep
  running *inside* the sandbox, so their credential isolation needs a different
  mechanism (separate plan).

Interim state (already live): `template.base` in the real config mounts shared
auth rw into every sandbox (`${sharedVolumes}/gh` → `/root/.config/gh`,
`${sharedVolumes}/sentry/sentryclirc.ini` → `/root/.sentryclirc`). Convenience
only, zero isolation. Remove those mounts when this plan ships.

## Background (code landmarks)

- `src/commands/services.rs:19-25,49-112` — global services run on the shared
  per-project network; instance containers join it; alias = service name, so
  sandboxes reach the server as `tools:<port>`.
- `src/commands/services.rs:126-141` — `ensure_services` already recreates a
  service container when its `config_hash` drifts; mounts becoming part of the
  service spec makes "new sandbox folder ⇒ recreate tools" fall out of this.
- `src/config.rs:477-488` — `Service` has no `mounts` today; that's the main
  CLI gap.
- `src/config.rs` `VarCtx` (`${sharedVolumes}`, `${configDir}`, …) — service
  mounts must reuse this substitution (no `${instance}` for global scope).
- `src/commands/run.rs:755-778` — `ensure_bind_source`: leading-dot-only
  basenames become directories; service mount sources must follow the same
  rules.
- `Cargo.toml:10,16` — `portable-pty` + `vt100` already in the tree (TUI
  terminal); the server reuses `portable-pty`.

## Design

### Binary

One new static binary (musl), a workspace member `cli-proxy/`, sharing nothing
with the `devsandbox` crate except conventions. Dispatch on `argv[0]`:

- `cli-proxy --serve --policy <file>` — server mode, runs in `tools`.
- symlinked as `gh` / `sentry-cli` — client mode; tool = basename.

Distribution into sandboxes: bind-mount the binary + symlinks from
`${sharedVolumes}` via `template.base.mounts` (upgrade without image
rebuilds). v2 sugar: a `proxy-clis = ["gh", "sentry-cli"]` sandbox key that
generates the mounts.

### Protocol

One TCP connection per invocation, `tools:7070` on the shared project
network. Handshake: JSON header
`{ tool, args, cwd, env: {allowlisted}, tty: Option<{cols, rows}> }`,
then length-prefixed multiplexed frames: `stdin`, `stdout`, `stderr`,
`resize`, `signal`, `exit(code)`.

- PTY on the server iff the client's stdin is a TTY (`gh pr create` prompts,
  pagers, colors); plain pipes otherwise so `gh api | jq` behaves.
- Env is an allowlist (`TERM`, `NO_COLOR`, `PAGER`, `GH_*`/`SENTRY_*` minus
  token/auth vars) — never the full sandbox env.
- Ctrl-C: client forwards `SIGINT/SIGTERM` as `signal` frames; client exits
  with the remote exit code.
- AuthZ: network isolation is the boundary (only project members join the
  network). Optional hardening later: shared-secret file mounted into both
  sides.

### Policy (the reason this isn't sshd)

TOML policy file in `tools`, checked server-side before exec:

```toml
[gh]
allow = ["pr *", "issue *", "repo view", "api *", "release *", "run *"]
deny  = ["auth *", "api /user/keys*"]   # deny wins

[sentry-cli]
allow = ["*"]
```

Deny by default for unknown tools.

### File access: share, don't intercept

Interception is a dead end: `gh` is a static Go binary (no libc ⇒ no
LD_PRELOAD), seccomp-notify/ptrace is a syscall-level network FS project, and
reverse-FUSE needs `/dev/fuse` + extra caps and makes `git status` crawl.

Instead the `tools` container mounts every sandbox's `folder` parent and the
config root's `.worktrees/` at **identical absolute paths**. Server behavior:
`chdir(header.cwd)` when it exists, else run from `$HOME` and print a warning.
Covers `gh pr create` (needs the git repo — install git in the tools image),
`sentry-cli sourcemaps upload ./dist`, `gh release upload <artifact>`.

Worktree caveat: a worktree's `.git` file points at the main checkout's
`.git/worktrees/<x>`, so *both* the main repo and `.worktrees/` must be
mounted, both at host-identical paths. devsandbox creates the worktrees, so it
knows both paths.

### New sandbox folder ⇒ recreate `tools`

Mounts are fixed at container creation; a sandbox added later isn't visible in
`tools`. With service mounts hashed into `config_hash`, `ensure_services`
already detects the drift — but recreating a *global* service other running
instances depend on must not be silent:

- CLI: prompt `service tools is outdated and shared; recreate? [y/N]`
  (skippable with `--yes`); on decline, keep the stale container and warn.
- TUI: reuse the pending-prompt pattern (`src/tui/mod.rs` run_suspended path).
- Logins survive recreation because auth state lives in a mounted volume, not
  the container FS.

## Steps

1. **`Service.mounts` in devsandbox** — add `mounts: Option<Vec<Mount>>` to
   `Service` (`src/config.rs:477`), substituted with the config-root `VarCtx`
   (reject `${instance}` in global scope), provisioned via
   `ensure_bind_source`, passed in `service_run_args`, and included in the
   service `config_hash`. Unit tests alongside the existing services tests.
2. **Recreate confirmation** — gate `ensure_services`' recreate-on-drift for
   `global` services behind a confirm callback (CLI prompt / TUI pending
   prompt / `--yes`).
3. **`cli-proxy` crate** — workspace member; client + server + protocol +
   policy; PTY via `portable-pty`; musl release build. Testable without a
   runtime (loopback socket in unit tests).
4. **Wire the live config** — tools Dockerfile gains git + `cli-proxy
   --serve`; `[services.tools]` gains auth-state + workspace mounts; shim
   binary + symlink mounts in `template.base`; drop the interim shared-auth
   mounts.

## Open questions

- Workspace mounts in `tools`: ro would stop the trusted container writing
  into sandbox trees, but breaks `gh pr checkout` / anything that writes.
  Lean rw for v1?
- `git push` from sandboxes: git talks HTTPS itself, so it bypasses the shim.
  A git `credential.helper` shim that RPCs to `tools` reintroduces a
  transient in-memory token. Out of scope v1.
- Policy defaults: ship a starter policy in the repo or leave it entirely to
  the config root?
