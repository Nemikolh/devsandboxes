> Archived 2026-10-16. Historical, may be wrong; do not use as a reference.
> Current: [README.md](../../../README.md)

# Helper lead fragment

## Docs written

- docs/architecture/helper/overview.md: what devsbd is, verbs, build, embedding, install, when it's (re)installed, `relay_mode`, the `cargo install` caveat
- docs/architecture/helper/protocol.md: transport path, frames, caps, handshake/mismatch, keepalive, daemon takeover, mux (legacy vs flow-controlled), line codecs (escape, bootfile, notify, control)
- docs/architecture/helper/bridges.md: host side: serve daemon owns bridges, reconcile/retry, `bridges.ensure`, bridge lifecycle, what a bridge serves (agent, notify → Inbox + desktop, control → dispatch), boot hook host role
- docs/architecture/helper/in-container.md: files/sockets, `devsbd daemon`, `bridge`, `boot`, outbox verbs, control verbs, runs, `vscode-goto`, `features`
- docs/architecture/helper/ssh-agent.md: relay vs mount decided at `run`, per-exec `SSH_AUTH_SOCK`, relay path and agent choice, bridge waits, what-works-where table

Cross-links they depend on: `../daemon/forwards.md` (daemon lead), `../../reference/api.md` (daemon lead), `../../status.md#helper` (needs a `### helper` heading under Known limitations).

## Known limitations (verified true today)

- Builds without an embedded helper (`cargo install` from crates.io) have no notifications, control requests, runs, port forwards or boot hook; ssh-agent falls back to the bind mount → [overview.md](../../../architecture/helper/overview.md); `Cargo.toml` `exclude` has `/devsbd/` + `/scripts/`, `build.rs` empty blob, `devsbd::embedded`
- Images without `/bin/sh` (distroless) can't get the helper; in default (relay-first) builds that means no ssh-agent at all, no mount fallback → [ssh-agent.md](../../../architecture/helper/ssh-agent.md); `devsbd::INSTALL_SCRIPT` runs via `runtime::SH`, `run/mod.rs` "ssh-agent forwarding off" note
- Windows native hosts get no ssh-agent forwarding (relay and mount are both unix-only) → [ssh-agent.md](../../../architecture/helper/ssh-agent.md); `relay_mode` `cfg!(unix)`, `ssh_agent_forward` `!cfg!(unix)` early return
- Mount mode (no-helper builds) forwards nothing on macOS + podman or Colima-style docker (no magic socket, host socket can't cross the VM) and nothing on Apple `container` (no file binds) → [ssh-agent.md](../../../architecture/helper/ssh-agent.md); `ssh_agent.rs` `magic_socket`, `apple.rs` `supports_file_binds` false
- Instances created in mount mode keep it until recreated → [ssh-agent.md](../../../architecture/helper/ssh-agent.md); `relay_mode` requires `ssh_auth_sock.is_none()`
- On a host `start` of a runtime-restart instance the boot hook fires before the host rewrites the boot file, so that start runs the previously recorded `postStartCommand` → [in-container.md](../../../architecture/helper/in-container.md); `devsbd::sync_boot` doc "Known gap"
- Images that mount a tmpfs at `/run` lose the helper and boot file on a runtime restart: the boot hook is a no-op (`[ -x BIN ]`), so `postStartCommand` doesn't rerun until a host `start`; the serve daemon reinstalls the helper on its next bridge spawn → [in-container.md](../../../architecture/helper/in-container.md); `run::container_command`, `devsbd::BIN` doc, `Bridges::reconcile`
- Two CLI builds used against the same containers evict each other's in-container daemon on every install (takeover keys on build-hash inequality), cutting live agent streams → [protocol.md](../../../architecture/helper/protocol.md); `devsbd/src/daemon.rs` `take_over`
- An older CLI exec'ing a newer helper sends no `Caps` behind a capable bridge, so that bridge never carries agent streams → [protocol.md](../../../architecture/helper/protocol.md); `daemon.rs` `known_host_caps` doc "Accepted gap"
- A record the host can't decode (helper newer than the host daemon) stalls the outbox at that record: the host replies `bad record`, the flusher keeps the file and stops in order → [bridges.md](../../../architecture/helper/bridges.md); `bridge.rs` `handle_notify`/`NOTIFY_REPLY_BAD`, `daemon.rs` `flush` breaks on `send_record` error
- Helper arch is found by a blind second try, no probe → [overview.md](../../../architecture/helper/overview.md); `devsbd::candidates` doc "Real probing is deferred"

## Status entries

- Planned (plans lead owns): helper RPC, host → in-container daemon requests (`docs/helper-rpc.md`, not implemented: no host-initiated channel exists; every host op is a fresh `exec`).
- Planned/parked: API proxying over the helper (`proto::channel::HTTP_PROXY` reserved, unused; `docs/sandbox-helper.md` "Later").
- Planned/parked: Windows ssh-agent relay over a named pipe (`docs/ssh-agent.md` "Windows (native)"; `mux.rs` doc "the Windows host will need a named-pipe stream").

## Stale claims in old docs

- sandbox-helper.md, Installing step 3: install failure → "bind-mount fallback" → only in no-helper builds; with a helper embedded `run` never mounts, a failed install means ssh-agent off. Evidence: `run/mod.rs` `relay_first`, note "ssh-agent forwarding off in …".
- sandbox-helper.md, Installing/Idempotence: "A CLI upgrade therefore rewrites the binary on next `run`/`start`" → also `port`, and the serve daemon before every bridge (re)spawn. Evidence: `Bridges::reconcile` → `ensure_recorded`; `commands/port.rs`.
- sandbox-helper.md, Binary: "Target size budget: < 500 KiB uncompressed" → current helpers are ~650 KiB (aarch64) / ~720 KiB (x86_64). Evidence: `wc -c target/devsbd/*/devsbd`.
- sandbox-helper.md, Step 10 "TUI bridges off the UI thread" → the TUI keeps no bridges; the only `Bridges::spawn_worker` caller is `serve/host.rs` `poll`. Also stale code comment: `devsbd/src/daemon.rs` `Bridges` doc says fallback to "a long-lived one (the TUI's)".
- helper-rpc.md, "The persistent link": control socket `/run/devsandbox/ctl` → `/run/devsandbox/devsbd.ctl` (`daemon::CTL`). Also cites `src/devsbd/bridge.rs:525` by line.
- ssh-agent.md, code landmarks: `src/commands/run.rs:469`, `src/commands/exec.rs:57`, `src/tui/app.rs:1392` → `run/` and `tui/app/` are directories now; builder is `exec::exec_argv_with`, rule `exec::ssh_auth_sock_env`.
- ssh-agent.md, "Future: Apple container" heading/"Blocked today" → the relay covers Apple `container` (no file binds needed); only mount mode is blocked there.
- Stale code comment (not a doc): `src/devsbd/bridge.rs` `Bridge.mismatch` says a mismatched bridge "is not retried while its container stays running" → retried after `MISMATCH_RETRY` (300 s), `retry_decision`.
- Stale code comments: `src/devsbd/bridge.rs` module doc and `devsbd/src/daemon.rs` module doc list control requests as `ensure|ls|stop|rm` → 16 ops (`control::Op::ALL`).

## Decision candidates

- Embed both Linux helpers in the CLI and install through `exec -i` stdin; why: offline, version-locked to the host, works on Apple `container` (no file binds) and needs no `docker cp`; source: sandbox-helper.md "Why embed"; still true because `src/devsbd.rs` `INSTALL_SCRIPT` + `run_with_stdin`, `build.rs`.
- `build.rs` never invokes cargo; the helper is a separate build step whose output build.rs only reads; why: nested cargo deadlocks on the target lock, leaks env (Windows `+crt-static`), reruns on every check, can't work from the crates.io tarball; source: sandbox-helper.md "Build + embedding"; still true because `build.rs` reads `DEVSANDBOX_DEVSBD_DIR` only, `scripts/build-devsbd.sh`.
- Relay-first ssh-agent: when a helper is embedded, never bind-mount the agent; why: one mechanism on every runtime, survives agent rotation while running, fixes uid-mismatch; source: sandbox-helper.md Step 6 / "Precedence"; still true because `run/mod.rs` `relay_first`.
- `SSH_AUTH_SOCK` injected per exec, never via `run -e`, at one fixed container path; mount source is a re-pointable symlink; why: rotation without rebuild, one rule for CLI/TUI/lifecycle; source: ssh-agent.md "How it splits"; still true because `exec::ssh_auth_sock_env`, `ssh_agent::ssh_agent_link`/`ssh_agent_refresh`.
- Agent chosen per stream from client-reported sockets; the service unit doesn't capture `SSH_AUTH_SOCK`; why: rotating agents; source: code (`bridge.rs` `AgentCandidates`, `serve/install.rs` `CAPTURED_ENV` doc); still true because both exist.
- One in-container daemon per build; a different build takes over ("last start wins"), keyed on hash inequality not on newer; why: the binary on disk always matches the running daemon; source: sandbox-helper.md "Last start wins"; still true because `daemon.rs` `take_over`.
- Daemon runs as root and serves the agent socket 0666; why: fixes uid mismatch, blast radius one container, key never enters; source: sandbox-helper.md "0666 agent socket"; still true because `daemon.rs` `listen(AGENT_SOCK, 0o666)`, `start_daemon` `-u root`.
- Protocol grows additively (unknown frame kinds skipped, caps bits, trailing `Hello` bytes) instead of `VERSION` bumps; CLI capabilities via `devsbd features`, separate from `version` and `caps`; why: a bump makes every running helper a mismatch until restart; source: sandbox-helper.md "Frame protocol", Binary/`features`; still true because `proto.rs` module doc + `read_frame` → `Ok(None)` for unknown kinds, `VERSION = 1`, `main.rs` `FEATURES`.
- Notify delivery is at-least-once and durable: host acks only after the Inbox store write; outbox kept in order, stops at first failure; source: notify.rs/daemon.rs docs; still true because `handle_notify`, `daemon::flush`.
- The serve daemon owns the one bridge per container; CLI commands ask for it with `bridges.ensure` instead of spawning their own; why: one place for notify/control, holders, agent reports; source: code (`bridge.rs` module doc, `serve/host.rs`); still true because `AgentRelay` → `ensure_bridge`.

## Uncovered

- API proxying / TLS interception ("Later" in sandbox-helper.md) and its open questions: not implemented; plans lead.
- helper-rpc.md entirely: not implemented design; plans lead.
- Windows named-pipe relay design (ssh-agent.md "Windows (native)"): not implemented; plans lead / windows-support.
- ssh-agent.md appendix (how git picks `SSH_AUTH_SOCK` vs `GIT_ASKPASS`) and the VS Code forwarding investigation: background about external tools, no devsandbox code; one line about VS Code kept.
- Port-forward engine (`src/devsbd/forward.rs`) beyond the `Connect`/`Window` wire: daemon lead (`../daemon/forwards.md`).
- Dispatcher op semantics (`commands::dispatch`), Inbox record semantics (`inbox::decide`/`ops::sink`), events: automations/inbox leads; my docs name the modules.
- Steps / review history in sandbox-helper.md: history only.
