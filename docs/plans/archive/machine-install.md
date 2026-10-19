> Archived 2026-10-19. Historical, may be wrong; do not use as a reference.
> Current: [architecture/daemon/machines.md](../../architecture/daemon/machines.md)

# `machine add` installs devsandbox on the machine

## Goal

`devsandbox machine add <target>` today assumes the machine already runs
the same devsandbox: a missing one fails in `machines.probe` with ssh's
stderr (`sh: devsandbox: not found`), a different semver is only a
warning (`machines::api::add`). Instead, before probing, the CLI checks the
machine's devsandbox and, when it's missing or another version, offers to
copy *this* binary there.

- On a TTY: ask (`[y/N]`). Declined: missing → error; another version →
  carry on as today (the existing semver warning).
- `--auto-install`: install without asking (TTY or not).
- No TTY, no flag: error, pointing at `--auto-install`.

A machine without a `devsandboxes.toml` then goes through the same
probe/pick/add flow as today (the picker's "other path…", the off-TTY
error). Initializing a config there is a later command.

## Design

The CLI does it, not the daemon: the daemon's ssh is `BatchMode` and has no
user to ask, and the bytes to copy are the CLI's own binary. It runs its own
ssh (`-T`, no `BatchMode`) through the same login-shell quoting as every
other remote command (`ssh::remote_command`), so `PATH` is the user's.

1. **Check**: one `sh -c` script: `command -v devsandbox`, `uname -sm`,
   `devsandbox --version`. Parsed into missing | found at `<path>` with a
   version (`None` when `--version` fails: a build from before it existed).
2. **Decide** (pure): same semver as `CARGO_PKG_VERSION` → nothing to do;
   else install, ask or fail as above.
3. **Platform**: `uname -sm` must match this binary's
   `std::env::consts::{OS, ARCH}` (`Linux`↔`linux`, `Darwin`↔`macos`;
   `x86_64`, `aarch64`|`arm64`↔`aarch64`), else refuse: copying itself
   can't work there.
4. **Install**: stream `exe::devsandbox()` over ssh's stdin into a
   `mktemp` file in the destination's directory, `chmod 755`, run
   `"$t" --version` (catches a binary that can't run there, e.g. a
   glibc-linked dev build), `mv -f` over the destination (atomic, no "text
   file busy" on a running binary). Destination: the found path when
   updating; `$HOME/.local/bin/devsandbox` when missing.
5. **Re-check** through the login shell: when `devsandbox` there now
   resolves to this version, done. When installed to `~/.local/bin` and
   it doesn't (not on the login `PATH`, as on macOS by default; another
   devsandbox earlier on it), the machine's `command` becomes
   `~/.local/bin/devsandbox` (shell text, the registry's `command` in
   `machines.toml`), checked once more through it. An update in place
   that still doesn't resolve is an error.
6. Probe/pick/add as before, passing that `command` to `machines.probe`
   and `machines.add` (new optional param), which dial with it and save it. A remote daemon still running the old binary
   is replaced by the existing version handoff: the relay's own probe hello
   (`relay::open`) is newer.

## Step 1: check, install and decide (no CLI change yet)

- `src/main.rs`: `#[command(version)]` on the top-level `Cli`, so
  `devsandbox --version` prints `devsandbox <semver>`.
- `src/serve/machines/ssh.rs`: `script_argv(entry, script, args)`: `ssh -T
  [-p N] -- <host> <remote_command("sh", ["-c", script, "sh", args…])>`
  (no `BatchMode`: a person is there). Test like `api_and_tty_argvs`.
- New `src/serve/machines/install.rs` (unix, `pub mod install` in
  `machines/mod.rs`): the check and install scripts as consts, `Remote`
  (parsed check output), `parse_check`, `platform_matches`, `Plan`
  (`Nothing | Install { dest, from: Option<…> }`), `decide`, and the I/O
  `check(entry)` / `install(entry, dest)` via `std::process::Command`
  (stderr tail in errors). Tests: parse/decide/platform as unit tests;
  the scripts run through real `/bin/sh` with a temp `HOME` and `PATH`
  (install into a missing `~/.local/bin`, then the check finds it; update
  in place over an existing file).
- Docs: `docs/architecture/daemon/machines.md` (*ssh*: `script_argv`).

## Step 2: `command` on `machines.probe` / `machines.add`

- `src/serve/machines/api.rs`: optional `command: Option<String>` on
  `ProbeParams` and `AddParams`, put on the dialed `Entry` and saved on
  the added one (`Entry.command`, already in `machines.toml`). An empty
  string is `invalid`. Tests: extend `add_checks_the_roots_on_the_machine_then_saves`
  (saved `command`).
- Docs: `docs/reference/api.md` (*machines* table), `npm/devsandboxes/index.d.ts`
  (`ApiMethods` params), `machines.md` *Registry* (`command` is set by
  `machine add` when it installs off `PATH`; still no flag).

## Step 3: wire it into `machine add`

- `src/main.rs`: `--auto-install` on `MachineCommand::Add`, documented in
  its help.
- `src/commands/machine.rs` `add`: check → decide → (prompt via `prompt`) →
  install → re-check, before `machines.probe`; messages on stderr.
- Docs: `machines.md` *`devsandbox machine add|ls|set|rm`*, `docs/status.md`
  (the *no `--command`* limitation line: still no flag, but an install
  sets it),
  `CHANGELOG.md` *Unreleased*, `AGENTS.md` module map (`install.rs`).
