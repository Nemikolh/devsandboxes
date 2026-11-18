# CLI proxy

Status: parked
Agreed design, never started: no `cli-proxy` crate, `Service` has no
`mounts`, and the live config dropped its `tools` service; shared auth is
still bind-mounted into every sandbox. The transport choice is open (below).

Builds on: global services on the shared project network, reached by
service-name alias, with drift warned on and fixed by `services rebuild`
(rewires running sandboxes in place), see
[services](../../architecture/instances/services.md); the helper's frame
protocol and its reserved `channel::HTTP_PROXY`, see
[protocol](../../architecture/helper/protocol.md); and possibly
[helper-rpc](helper-rpc.md).

## Goal

Sandboxes run `gh` and `sentry-cli` without holding their credentials.
Tokens live only in a `tools` service (`scope = "global"`); each sandbox
gets shim CLIs at `/usr/local/bin/{gh,sentry-cli}` that forward the
invocation to a server in `tools` and stream the result back. The
ssh-agent pattern applied to CLIs (forward operations, not secrets), plus
a server-side policy filtering subcommands (`gh pr *` yes, `gh auth token`
no).

Best-effort by design:

- A sandbox can still do whatever the proxied CLI allows (confused
  deputy); it just can't exfiltrate the token. The policy narrows the
  blast radius.
- Remote file access works only for paths inside workspaces (mounted into
  `tools` at identical paths); anything else fails with "path not shared
  with tools".
- In-sandbox coding agents are out of scope: their CLIs must run inside the
  sandbox, so their credential isolation needs another mechanism.

## Decisions

- **Shim + server, not sshd**: the policy table is the point.
- **Share files, don't intercept**: `gh` is static Go (no `LD_PRELOAD`);
  seccomp-notify/ptrace is a network-FS project; reverse-FUSE needs
  `/dev/fuse` and extra caps and makes `git status` crawl. Instead `tools`
  mounts every sandbox's `folder` parent and the config root's
  `.worktrees/` at host-identical paths (both needed: a worktree's `.git`
  file points into the main checkout). Server `chdir`s to the caller's cwd
  when it exists, else runs from `$HOME` with a warning. The tools image
  needs git.
- **PTY iff the client's stdin is a TTY** (prompts, pagers, colors); plain
  pipes otherwise so `gh api | jq` behaves.
- **Env is an allowlist** (`TERM`, `NO_COLOR`, `PAGER`, `GH_*`/`SENTRY_*`
  minus token/auth vars), never the sandbox env.
- **Ctrl-C** forwards `SIGINT`/`SIGTERM`; the client exits with the remote
  exit code.
- **Policy**: TOML in `tools`, checked before exec, deny wins, unknown
  tools denied:

  ```toml
  [gh]
  allow = ["pr *", "issue *", "repo view", "api *", "release *", "run *"]
  deny  = ["auth *", "api /user/keys*"]

  [sentry-cli]
  allow = ["*"]
  ```

- **Distribution**: one static (musl) binary, dispatching on `argv[0]`
  (`cli-proxy --serve --policy <file>` in `tools`; symlinked as `gh` /
  `sentry-cli` in sandboxes), bind-mounted from `${sharedVolumes}` via
  `template.base.mounts` so upgrades need no image rebuild. Later sugar: a
  `proxy-clis = ["gh", "sentry-cli"]` sandbox key generating the mounts.
- **New sandbox folder means a stale `tools`**: service mounts go into the
  service's `config_hash`, so `ensure_service` warns on drift and the user
  runs `devsandbox services rebuild tools`. Logins survive because auth
  state lives in a mounted volume.

## Transport (open)

1. **Own TCP**: `tools:7070` on the shared network, one connection per
   invocation, JSON header `{ tool, args, cwd, env, tty: {cols, rows}? }`
   then length-prefixed `stdin`/`stdout`/`stderr`/`resize`/`signal`/`exit`
   frames (the [helper-rpc](helper-rpc.md) envelope, if that lands first).
   AuthZ is network membership; optional shared-secret file later.
2. **Through devsbd and the host** ([helper-rpc](helper-rpc.md) option 2):
   no listener in `tools`, authorization per caller instance, policy on the
   host; needs container → host RPC and `serve` running.
3. **HTTP proxying over the helper** (the reserved `channel::HTTP_PROXY`):
   the helper listens on `127.0.0.1:<port>`, sandboxes get
   `HTTPS_PROXY`/`HTTP_PROXY`, streams go to a credentials service that
   terminates TLS with a per-config-root CA (cert installed in the
   container via `update-ca-certificates`, `NODE_EXTRA_CA_CERTS`,
   `REQUESTS_CA_BUNDLE`, `SSL_CERT_FILE`, `GIT_SSL_CAINFO`,
   `CURL_CA_BUNDLE`) and injects credentials. Helper stays a dumb tunnel
   (no rustls, CA key never in the sandbox); flow control reuses
   `Connect`'s `Window` credit, no `VERSION` bump. Covers `git push` over
   HTTPS too, which the shims can't.

## Steps

1. **`Service.mounts`**: `mounts` on `Service` (`src/config.rs`),
   substituted via `config::substitute` with a config-root
   `MountContext` (reject `${instance}` for
   global scope), sources provisioned like `ensure_bind_source`
   (`src/commands/run/mounts.rs`), passed in `service_run_args`
   (`src/commands/services.rs`), part of the service `config_hash`. Update
   `skills/config-toml-spec/SKILL.md`.
2. **`cli-proxy` crate**: workspace member; client, server, protocol,
   policy; PTY via `portable-pty`; musl build; tests over a loopback
   socket.
3. **Live config**: tools Dockerfile gains git and `cli-proxy --serve`;
   `[services.tools]` with auth-state and workspace mounts; shim mounts in
   `template.base`; drop the interim shared-auth mounts.

## Open questions

- Transport: 1, 2 or 3 above.
- Workspace mounts in `tools` ro (safer) or rw (`gh pr checkout` writes)?
  Lean rw.
- `git push` over HTTPS bypasses the shim; a credential-helper shim
  reintroduces a transient token. Out of scope unless option 3.
- Ship a starter policy in the repo or leave it to the config root?
