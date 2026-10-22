# Identity: one user model across runtimes

Status: planned
Nothing is built. Today `run_container` (`src/commands/run/mod.rs`) passes
`containerUser` as `--user`, `generate_dockerfile`
(`src/commands/run/image.rs`) bakes it as `USER`, `exec` runs as
`remoteUser`, and `updateRemoteUserUID` parses but is ignored
(`SandboxProperties::ignored`). No code reasons about user namespaces,
SELinux labels or the docker socket's gid.

## Problem

devcontainer.json says *how* to pick a user (`remoteUser`, `containerUser`,
`updateRemoteUserUID`, uids baked into images), not *what* the user wants.
The same config is right on one host and broken on the next, because the
same mechanism means opposite things per runtime mode:

| Mode | uid 0 inside is | uid 1000 inside is | What works |
|---|---|---|---|
| rootful docker (Linux) | host root | host 1000 | non-root user, uid remapped to the host uid |
| rootless docker | the host user | a subuid (e.g. 100999) | run as root (root *is* you); no non-root uid can map to you |
| rootless podman | the host user (default userns) | a subuid | `--userns=keep-id:uid=U,gid=G` maps you onto the image user |
| VM-backed (Docker Desktop, OrbStack, colima, podman machine, Apple `container`) | depends on the share | depends on the share | mostly anything; the file share translates ownership |
| any of the above + SELinux enforcing | | | binds need `:z` or every access is EACCES |

Cross-cutting breakage:
- **cache binds**: `resolve_caches` creates their sources on the host
  (`ensure_bind_source`); under a userns the container user can't write
  them, or they end up owned by a subuid.
- **docker socket**: the socket's gid differs per host, so
  docker-outside-of-docker's group setup only works by luck.
- **ssh-agent socket and named volumes**: same ownership problem. Named
  volumes start root-owned unless the image path already exists.

## Goal

The intent, in every mode:
1. Files the sandbox writes into host binds (workspace, worktrees,
   `folders`, caches) are owned by the host user on the host.
2. The dev user inside can write its workspace, `$HOME`, caches and the
   sockets it's given (docker, ssh-agent).
3. The same image works on every host (no per-uid rebuilds; matters for
   `machines/`, where one config runs on several hosts).

## Scope

- **Tier 1, the best experience**: docker (rootful and rootless) and
  podman (rootful and rootless). Covered by tests and fixed when broken.
- **Tier 2, best effort**: Apple `container`, plus other VM-backed
  runtimes. They get the no-op plan and nothing else. Breakage there is a
  status line, not a blocker.
- **Best effort on ids**: no idmapped mounts. The adopted account is
  assumed to be uid/gid 1000 (what devcontainer base images and
  common-utils' `automatic` user use). Hosts and images that stray from
  that get a diagnostic, not a guarantee (see *Assumed ids*).
- **Services are out of scope**: see *Services*.

## Design

### 1. Identity contract (config)

A sandbox field `identity`:
- `"host"` (default): inside you are a user with the host uid/gid. The
  image's `remoteUser` (else `containerUser`, else the image's `USER`)
  is the account adopted: its name, home and shell are kept, only its
  uid/gid change. Paths under `/home/vscode` etc. keep working.
- `"image"`: today's behavior, users taken literally, no remapping.

`updateRemoteUserUID` stops being ignored and turns into a hint: `false`
means `identity = "image"`. A new field means updating
`skills/config-toml-spec/SKILL.md`. Hash `identity` for drift, since it
changes how the container is created.

### 2. Probe (per runtime context, cached)

Mode detection is empirical, with metadata as the fast path:
- **fast path**: `docker info` security options contain `name=rootless`;
  `podman info` `.Host.Security.Rootless`; `docker context`/`DOCKER_HOST`
  pointing at a VM socket; Apple `container` is always VM-backed.
- **canary** (ground truth, run when the fast path is ambiguous or on
  `--reprobe`): a throwaway container from a tiny image (or the helper's
  static binary on `scratch`) with a temp host dir bound in. Inside, write
  one file as uid 0 and one as uid 1000. On the host, `stat` both. The
  four outcomes (host-uid / root / subuid / squashed) classify the mode.
  Also detects SELinux: an EACCES on the unlabeled bind that disappears
  with `:z`.

The result goes into `state.toml`, keyed by runtime and context
(`DOCKER_HOST`/context name, podman connection, machine). A changed key
means a fresh probe. Lives in `src/runtime/` as a pure classifier over the
two stats, plus a backend method running the canary.

### 3. Strategy table (pure)

`(mode, identity, adopted account) → Plan`, a pure function in
`src/runtime/` that gets a table test:

| Mode | Plan |
|---|---|
| rootful | create as root, then remap the adopted account to host uid/gid (step 4); exec as it |
| rootless docker | exec as root, `HOME` = the adopted account's home, chowned to 0 (root is you) |
| rootless podman | `--userns=keep-id:uid=1000,gid=1000` (the assumed adopted ids); no remap |
| VM-backed (Tier 2) | no remap; exec as the adopted account |
| `identity = "image"` | today's behavior |

Plus modifiers on every plan: `:z` on binds when SELinux is detected, and
the docker socket's gid added to the dev user (step 4) when the sandbox
mounts it.

The plan is recorded on the instance (a label, like `config_hash`) so
`exec`, the TUI terminal and lifecycle commands read the user from it
rather than recomputing it.

### 4. Remap at create, by the helper

Not a build-time `USER`/`usermod` layer: that bakes the host uid into the
image, which breaks requirement 3 and the image cache.

After create, during the existing helper install and before
`onCreateCommand`, a root exec runs a new `devsbd identity apply
<user> <uid> <gid> [--socket-gid N]`:
- rewrite the account's uid/gid in `/etc/passwd` / `/etc/group`
  (moving any clashing account aside), with no dependency on `usermod`
  being in the image;
- chown the parts of `$HOME` the image's old uid owns (not a blind
  `chown -R`: large homes make that slow, fixuid's known problem);
- add a group with the socket's gid and put the user in it.

It runs once per container: the edits live in the container layer, so a
restart keeps them and `devsbd boot` has nothing to redo. Idempotent, so
a repeat run is harmless. Note that the container command
(`container_command`) runs as `containerUser`, so this has to be a root
exec from the host, not part of `devsbd boot`.

### Assumed ids

The plan is built before the container exists, so it can't read the
image's `/etc/passwd`. It assumes the adopted account is 1000:1000.
`devsbd identity apply` sees the real ids, and on a mismatch it warns,
naming the account, its real ids and what will be wrong. Under keep-id,
that means workspace files come out owned by a subuid. There is no
retry with corrected ids. If this turns out to bite, the fix is to read
the image's `/etc/passwd` once per image (`<runtime> run --rm`, cached
by image id), not idmapped mounts.

On rootful docker the host uid is free to be anything: the remap moves
the account to it, whatever its starting uid.

### No account to adopt

An image with no non-root user (no `remoteUser`/`containerUser`, image
`USER` root, no uid-1000 account) has nothing to remap. Two options,
undecided (see *Open questions*):
- add the `common-utils` feature implicitly (`username = "automatic"`,
  which creates a non-root user and passwordless sudo);
- run as root, then warn and recommend adding `common-utils` to the
  config.

Under rootless docker neither option changes the outcome: non-root
inside is a subuid there, so root is still the plan.

### Tool environment

Chromium's sandbox won't start as root, and often not in a container at
all (no unprivileged user namespaces under the default seccomp/AppArmor).
Each tool's own knob is how it gets turned off. Agents shouldn't have to
work that out, so devsandbox sets the knobs in the container env, from the
plan, next to `builtin_env`:

| Tool | Knob | Status |
|---|---|---|
| Puppeteer | `PUPPETEER_DANGEROUS_NO_SANDBOX=1` | merged upstream 2026-03 (puppeteer#14756); older Puppeteer ignores it |
| Electron (VS Code, Electron tests) | `ELECTRON_DISABLE_SANDBOX=1` | since Electron 5 |
| QtWebEngine | `QTWEBENGINE_DISABLE_SANDBOX=1` | long-standing |
| Playwright library | none needed | `chromiumSandbox` defaults to false |
| Playwright MCP | `PLAYWRIGHT_MCP_SANDBOX=false` | seen in the wild, unverified |
| Debian/Ubuntu `chromium` wrapper | `CHROMIUM_FLAGS=--no-sandbox` | from memory, unverified |
| pip, bundler (root warnings only) | `PIP_ROOT_USER_ACTION=ignore`, `BUNDLE_SILENCE_ROOT_WARNING=1` | silencers, root plans only |

What no env var covers (a raw `google-chrome`, chrome-launcher/Lighthouse,
Karma, Selenium): a shim in `/run/devsandbox/bin` that execs the real
binary with `--no-sandbox`, with `CHROME_PATH`/`CHROME_BIN` pointed at
it. That's a later step, and only when a browser binary exists in the
image. Shadowing binaries has its own risks.

Homebrew refuses to run as root and has no override: documented, not fixed.

#### Keeping the browser sandbox where it can work

The browser sandbox is what keeps a renderer exploit away from
everything in the container: the workspace, the ssh-agent socket, the
docker socket. Agents browsing untrusted pages are exactly that case.
So the knobs are a fallback, not the default.

Chromium's layer-1 sandbox is either the namespace sandbox or the
deprecated setuid helper (`sandbox/linux/README.md` in Chromium).
In a container it needs exactly two things:
- **a non-root euid**: the zygote aborts at euid 0 unless given
  `--no-sandbox`, and no flag or capability gets past that;
- **unprivileged user-namespace creation**:
  - podman's default seccomp profile (containers/common) allows
    `clone`/`unshare` with `CLONE_NEWUSER`;
  - docker's default profile (moby/profiles) blocks it unless
    CAP_SYS_ADMIN is held (moby#42441, still open). Docker's docs table
    claims the opposite;
  - Chrome's zygote also calls `clone(CLONE_NEWPID)` from inside its own
    userns, which docker's profile blocks too, so allowing
    `CLONE_NEWUSER` alone isn't enough (tested, below);
  - on Ubuntu 23.10+ hosts, `kernel.apparmor_restrict_unprivileged_userns=1`
    doesn't block it under docker's default AppArmor profile, but does
    once AppArmor is turned off: an unconfined task's new userns loses
    its capabilities and the `uid_map` write fails. **Never pass
    `apparmor=unconfined`** for this.

Nested userns is fine under rootless runtimes: Chrome maps only its own
uid.

Tested on Ubuntu 24.04.5 (kernel 6.8, restriction sysctl = 1), rootful
Docker 29.9.1, `ubuntu:24.04` and `mcr.microsoft.com/playwright:v1.64.1-noble`
(uid 1000 / `pwuser`):

| seccomp / AppArmor | `unshare -Ur true` | headless Chrome, no `--no-sandbox` |
|---|---|---|
| default / default | EPERM | `No usable sandbox!` |
| + `NEWUSER` rule / default | ok | hangs: `Zygote could not fork` (`clone(CLONE_NEWPID)` EPERM) |
| + `NEWUSER` + `NEWPID` rules / default | ok | **ok**: renderers in a new user and pid ns, chrooted, 2 seccomp filters |
| same / `unconfined` | `uid_map` write EPERM | `No usable sandbox!` |
| Playwright's `seccomp_profile.json` / default | ok | ok (much looser: unconditional `clone`/`unshare`/`setns`, last changed 2021) |
| `unconfined` / default | ok | ok |

Per mode:

| Mode | Browser sandbox | How |
|---|---|---|
| podman rootful, rootless (keep-id) | on | nothing extra; the dev user is non-root |
| docker rootful (remapped user) | on (tested) | devsandbox passes `--security-opt seccomp=<profile>`, default AppArmor kept |
| docker rootless (root plan) | on, via the browser wrapper (below) | same profile; the wrapper puts the browser at a non-root euid |
| no account to adopt (root, any runtime) | on, via the wrapper | the wrapper needs no account in the image |

The docker profile is moby's default (`moby/profiles` `seccomp/v0.2.4`,
what Docker 29.9.1 vendors) plus two rules, both excluded when
`CAP_SYS_ADMIN` is held, the same shape as moby's own rules:

```json
{"names":["clone","unshare"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":268435456,"valueTwo":268435456,"op":"SCMP_CMP_MASKED_EQ"}],"excludes":{"caps":["CAP_SYS_ADMIN"],"arches":["s390","s390x"]}}
{"names":["clone"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":536870912,"valueTwo":536870912,"op":"SCMP_CMP_MASKED_EQ"}],"excludes":{"caps":["CAP_SYS_ADMIN"],"arches":["s390","s390x"]}}
```

The second rule is `CLONE_NEWPID`. The kernel still requires
`CAP_SYS_ADMIN` in the owning userns, so it only takes effect inside a
new userns. `clone3` stays ENOSYS (glibc falls back to `clone`). The
profile is embedded and pinned to a moby version, regenerated
deliberately, and passed by path (`--security-opt seccomp=<file>`,
written next to `state.toml`).
| VM-backed (Tier 2) | probably on | one secondary source reports Apple `container` restores it. Not verified |

Cost of the docker profile: unprivileged userns opens the kernel attack
surface that docker's default closes (a steady source of kernel CVEs).
That's the same trade podman's default already makes. It's a sandbox
field, `userns = true` by default on docker, and `false` keeps moby's
profile and falls back to the knobs. It changes create args, so it's
hashed for drift.

#### Browser wrapper (root plans)

Under rootless docker the dev process is container root, which is you on
the host. The same holds when there's no account to adopt. Chrome refuses
its sandbox at euid 0, so a wrapper starts it at a non-root euid:
- **where:** `/run/devsandbox/bin/chrome`, the target of
  `PUPPETEER_EXECUTABLE_PATH`/`CHROME_PATH`/`CHROME_BIN` and first on
  `PATH` under the usual names (`chrome`, `chromium`, `google-chrome`).
  It finds the real binary on the rest of `PATH`, else Playwright's
  `~/.cache/ms-playwright`;
- **how:** `devsbd` does the uid drop itself (static, no dependency on
  the image's util-linux) and execs the real binary, keeping fds open
  (Playwright drives Chrome over `--remote-debugging-pipe`, fds 3/4);
- **what uid: a single-uid userns** (`unshare --user --map-user=1001
  --map-group=1001` equivalent: `unshare(CLONE_NEWUSER)`, `setgroups`
  deny, one-line `uid_map`/`gid_map`, exec). The euid is non-root
  inside, with container root underneath, so it writes whatever root can
  and files come out host-owned. No account is needed in the image, and
  no HOME or profile-dir fixups. Chrome nests its own userns for the
  renderers. The browser process is as confined as any other root
  process in the container, the same as Chrome under a desktop user.
  Tested: headless Chrome and Playwright 1.64.1 (`chromiumSandbox: true`,
  `--remote-debugging-pipe`, `newPage`/`goto`) both work, with the
  renderer sandbox on;
- **rejected: a separate browser uid** (`setpriv --reuid`). It works
  only with a writable HOME and every `--user-data-dir` the driver made
  as root (0700) chowned first: Playwright's fails with `Failed to
  create a ProcessSingleton`, and plain `HOME=/root` fails earlier. The
  files it writes end up owned by that uid. An unmapped `unshare -U`
  (euid 65534) fails outright with `No usable sandbox!`;
- **Gap:** the Playwright library doesn't go through the wrapper unless
  told to (`executablePath`, and `chromiumSandbox: true` so it stops
  adding `--no-sandbox`). No env var for it is known. Until then its
  sandbox stays off on root plans. It still runs, because Playwright
  adds `--no-sandbox` by default. Since devsandbox can't fix this
  silently, agents are told:
  - **discoverable:** two env vars. `DEVSANDBOX_BROWSER_SANDBOX=1` is set
    when the probe passed. Playwright's `chromiumSandbox` defaults to
    false on *every* plan, so this matters on non-root plans too. On root
    plans, `DEVSANDBOX_CHROME=/run/devsandbox/bin/chrome` is set as
    well. A script then does `chromium.launch({ chromiumSandbox: true,
    executablePath: process.env.DEVSANDBOX_CHROME })` when the first is
    set, and keeps Playwright's defaults otherwise;
  - **said where agents read it:** a short note in
    `skills/devsandbox-cli/SKILL.md` (host agents exec'ing into
    instances), the Claude Code plugin's skill
    ([claude-code-plugin](claude-code-plugin.md)), and for agents running
    inside an instance, the auto-mounted skills
    ([agents-skills](agents-skills.md), parked). The note: with
    `DEVSANDBOX_BROWSER_SANDBOX=1`, launch Playwright with
    `chromiumSandbox: true`, plus `executablePath: $DEVSANDBOX_CHROME`
    when that's set; never add `--no-sandbox` by hand. Written
    when the wrapper ships, not before: skills describe what exists;
- **Scope:** every root plan. On other plans the dev user is already
  non-root and needs no wrapper.

Picked by the probe, not assumed: the canary also runs `unshare -Urp
--fork true` (user and pid ns, Chrome's zygote sequence) as a non-root uid
inside the container (with the planned seccomp
profile). Reading sysctls inside the container isn't enough: one report
has `ubuntu:24.04` with every sysctl looking fine, yet docker's seccomp
still blocks it. Success means the knobs stay unset. Failure means they're
set, and `run` says why ("no user namespaces: seccomp" or "…: AppArmor
restriction on the host"). The result is recorded on the instance and
shown by `inspect`. The knobs are env only, applied on `exec`.

### 5. Caches and volumes

Today cache binds are created host-side. Under a plan that maps the dev
user to the host uid (rootful + remap, keep-id, VM), the existing binds
are correct as they are. Under rootless docker root is you, so they're
correct too. What's left is named volumes and feature mounts:
`devsbd identity apply` chowns their mount points when they're root-owned
and empty.

### 6. Diagnostics

`run` prints one line naming the mode and the plan ("rootless docker:
exec as root, root maps to you"), and `inspect` shows the recorded
plan. Ignored or overridden devcontainer user fields are warned about
once, the same way as other ignored properties.

### 7. Migrating existing configs

Configs written for root-in-container break under `identity = "host"`:
`target=/root/...` mounts land in a 0700 dir the dev user can't reach.
The user asked for a migration (an exception to the pre-1.0
no-migrations rule): offered when the TUI opens, and asked before a verb
acts on an affected sandbox. The per-verb prompt is the preferred surface.
A design pass (a subagent reading the code) found it's one mechanism:
findings, a rewrite and an ack.

**`${containerEnv:HOME}`** (devcontainer's variable; `config::resolve_var`
knows only `localEnv:` today). The general form `${containerEnv:NAME}`
reads the sandbox's `containerEnv`, then the image's `Config.Env`. `HOME`
is rarely in either, so it falls back to the adopted account's home.
That home comes from the image's `/etc/passwd`, read once per image id.
The same read gives the real uid/gid, so it replaces the 1000 guess in
*Assumed ids*. The fallback is `/home/<user>`, or `/root` for root.

It's resolved at create time through a new field on
`config::MountContext`. The devcontainer spec doesn't allow
`containerEnv:` in `mounts`; devsandbox does, and the spec skill says so.
It's also substituted in `containerEnv`/`remoteEnv`, which the spec says
aren't substituted today, so the spec changes. A self-reference
(`containerEnv.HOME = "${containerEnv:HOME}"`) is an error. With no
runtime (`snapshot::validate_sandbox`, `ls`), it stays verbatim and isn't
an error. On the rootless-docker root plan `HOME` is still the adopted
home, so a rewritten config doesn't depend on the host's plan.

**Findings**: a pure `identity_findings(&Config)` in a sibling module
(`src/config_identity.rs`). It scans each template's and sandbox's *own*
table, so every finding names the table that defines it, and walks
`extends` (as `Config::resolve_extends` does) for the sandboxes it
reaches. There's one config file per dir (`config::find`), so the origin
is a table, not a file. Kinds:
- `RootTarget`: a `mounts` target equal to or under `/root`, in both
  forms (`config::parse_shorthand` `target=`/`dst=`/`destination=`, and
  `MountObject::target`). Rewritten to `${containerEnv:HOME}/...`;
- `RootEnv`: `containerEnv`/`remoteEnv` values under `/root` (e.g. the
  site examples' `CLAUDE_CONFIG_DIR`). Rewritten the same way;
- `RootPath`: `workspaceFolder`/`folders` targets under `/root`. Fixed by
  hand;
- `RootUser`: `remoteUser`/`containerUser = "root"`, counted only where
  it wins the merge (`Config::resolved_table`). "Update" removes the key
  (the image's account gets adopted). "Continue as is" isn't an ack: it
  writes `identity = "image"` into that table, so keeping root is
  explicit and the finding is gone for good.

Not findings: caches (already under `/var/cache/devsandbox/`,
`config::known_cache`), shell history (`/commandhistory`), feature mounts
(not the user's file), `updateRemoteUserUID` (a hint, not a migration).

A finding's id is `short_hash(table | key | current value)`, without the
array index (indices shift), so a new or edited `/root` mount warns again.

**Rewrite**: add `toml_edit`. It shares `toml`'s `toml_parser`/`toml_writer`/
`winnow`, so it's about one new crate, and it keeps comments and layout.
The rewrite is a pure `rewrite(text, &[Finding])` that reparses its output
and checks the findings are gone. YAML configs (`Config::parse_yaml`) are
refused, with the edits printed instead. Writing:
- `devsandboxes.toml.bak` is copied before the write and deleted once the
  new file is in place and reparses with the findings gone. It only
  guards the write itself, and is left behind (and reported) only when
  something fails;
- tmp + rename, keeping the mode, through a symlink to its target;
- `conflict` when the file's hash changed since the findings were read.

The rewrite changes `config_hash`, so affected instances show drift.
That's the right outcome (pre-identity containers need recreating), and
it's reported with the next step: `updated devsandboxes.toml (5 edits);
now drifted: web, web-2 → devsandbox rebuild <name>`. The verb that asked
then continues on the new config. An instance created before identity
has no plan label: it keeps its recorded `remote_user` (`state::Instance`)
until it's rebuilt.

**Ack ("continue as is")**: `identity_acks` in `state.toml`, project id
to finding ids, next to `autostart_boot`, written through `State::update`
and pruned of stale ids on every write. Not `dashboard.toml` (the CLI
wouldn't see it), and not the config (noise, and it would change the
hash). An acked finding leaves the header and the prompts, and stays dimmed
in Detail.

**Surfaces**:
- **verb prompt** (CLI): a pre-send quick call in
  `serve::verbs_client::prepare`, like `sandboxes.list`, once the target
  sandbox is known: `[u]pdate / [c]ontinue as is (don't ask again) /
  [s]kip this time`. It covers `run`/`start`/`rebuild`, with one combined
  prompt for `--all`. `exec` creates nothing and doesn't prompt. It is not
  modelled on rm's confirm round, which is an error coming back from an
  already-spawned subprocess;
- **verb prompt** (TUI): before `spawn_verb` sends run/start/rebuild, a
  three-choice modal (on `Modal::Confirm`, `tui/app/actions.rs`). The `:`
  prompt goes through the same path;
- **header**: a yellow line under the red `Snapshot.error` line
  (`tui/ui.rs`), e.g. `2 sandboxes use /root (U to update)`, for
  un-acked findings only;
- **Detail**: `SandboxRow.identity`, filled in `snapshot::sandbox_rows`
  (shared with `ls`), rendered in `tui/ui.rs::sandbox_detail` next to the
  red `issues`, yellow (dim when acked);
- **TUI open**: on the first `instances.snapshot`
  (`tui/daemon.rs::snapshot_update`), a modal listing every un-acked
  finding's edit: `update / continue as is / later`. Ack and migrate go
  out as daemon-worker `Cmd`s, like `Rename`.

**API**: `config.findings {root, sandbox?}` → `{file, format, fileHash,
findings}`, `config.migrate {root, ids?, fileHash}` → `{edits, drifted}`
(errors `conflict`, `unsupported`, `invalid`), `config.ack {root, ids}`.
All three are in `machines/forward.rs`'s `QUICK`, so a machine's config is
checked, rewritten and acked by that machine's daemon. Machines' configs
are rewritten in place, the same way as local ones. This makes the
daemon write a user file, against the rule in `run::offer_example_config`,
because the TUI and machines have no other way to write. The doc says so.
Update `npm/devsandboxes/index.d.ts` (`serve/dts.rs`) and
`docs/reference/api.md`.

**Edge cases**:
- off a TTY, and under `--json`: one `warning:` line on stderr, never
  blocking, with stdout left alone (`json_stdout`);
- the dispatcher's `--in-process` subprocesses and autostart: no
  prompt, the warning only, printed by `run` the way `ignored()` is;
- no daemon: the same prompt, with the pure fns called locally;
- a template shared through `extends`: edited once, fixed for every
  sandbox using it. An unused template's finding shows up in the
  TUI-open offer only;
- **autostart**: "before anything boots" can't fully hold. The first
  connect starts the daemon, and `serve::host::Host::new`'s autostart
  pass runs straight away. It mostly starts existing containers, which
  keep their old model until rebuilt anyway. Accepted (user's call); the
  architecture doc says so.

Also update the spec skill's example, the site examples and
`site/src/lib/agents.test.ts`'s matcher (`${containerEnv:HOME}/.claude`).

## Services

No plan. `[services.*]` has no mount, user or userns knobs (`config::Service`,
`deny_unknown_fields`; `services::service_run_args` emits no `-v`,
`--mount`, `--user` or `--userns`). Services attach to sandboxes only
through bridge networks (`--network`/`network connect`; no pods, no
`container:` namespaces), so:
- no host-owned files;
- no volumes shared with a sandbox;
- no userns coupling when the sandbox gets keep-id on rootless podman.

The service image's own user is left as is, the TUI service terminal
included. Apple's `wire_service_dns` already execs as root. Guard: a
test asserting `service_run_args` emits no `--userns`/`--user`, so that
identity code never leaks into services through a shared args builder.
Revisit if services ever gain mounts (seed data, a volume shared with a
sandbox): then they need `:z` and a uid story.

## Machines

The plan is computed where the container is created, by that host's
devsandbox: the probe and its cache belong to the host running the
runtime. A machine probes for itself, the same way as local. This fits
the longer-term goal of any machine exposing any sandbox: wherever a
sandbox is defined, the plan follows the runtime that runs it, not the
config. The cache key includes the context, never the machine that
asked.

## Steps

1. Pure classifier + strategy table + tests (`src/runtime/`).
2. Probe: fast path, then canary; cache in `state.toml`; `--reprobe`.
3. `devsbd identity apply` in the helper, with unit tests on passwd/group
   rewriting (pure over file contents).
4. Wire into `run`: plan → create args (`--userns`, `:z`) → apply exec →
   label; `exec`/TUI/lifecycle read the label.
5. `identity` config field, `updateRemoteUserUID` as its hint, spec skill,
   drift hash.
5b. Tool environment: the userns probe in the canary, the docker seccomp
   profile and the `userns` field (spec skill), the knobs as a fallback.
   Then the browser wrapper and `DEVSANDBOX_CHROME`, as their own step,
   with the agent note in `skills/devsandbox-cli/SKILL.md` (and the
   plugin's skill, if it exists by then).
6. Docs: `architecture/instances/` (a new `identity.md`), `runtimes.md`,
   status line for anything left.

Migration (section 7, after step 4):

7. `${containerEnv:NAME}` / `${containerEnv:HOME}`: `MountContext`,
   `resolve_var`, env substitution, the image passwd and `Config.Env` read
   cached by image id (replacing the 1000 guess), spec skill.
8. `identity_findings` with origins and ids; table tests (templates,
   `extends` chains, scalar overrides).
9. `rewrite` on `toml_edit`: round-trip tests, YAML refusal, the atomic
   write with a transient `.bak`, the `conflict` guard, and `RootUser`'s
   `identity = "image"` write.
10. `identity_acks` in `state.toml`; `config.findings`/`migrate`/`ack`
    (`QUICK`, d.ts, api.md).
11. `SandboxRow.identity`, the header line, the Detail lines.
12. The CLI prompt in `verbs_client::prepare`, the in-process fallback,
    the off-TTY warning.
13. The TUI verb modal and the TUI-open offer; docs and a status line.

Docker tests: one `#[test_utils::docker_test(helper)]` per Tier 1 mode
the CI host can run (rootful at least), asserting a file written in the
workspace is host-uid-owned.

## Decisions

- No idmapped mounts. Best effort on ids, with 1000 assumed (above).
- Docker and podman are Tier 1; Apple `container` is Tier 2.
- Services get no identity treatment.
- The probe runs per runtime host (a machine probes for itself).
- Existing configs get a migration (user's request, despite pre-1.0):
  asked per verb (preferred), offered when the TUI opens, flagged in the
  header and Detail. "Continue as is" is remembered per finding,
  except for root users, where it writes `identity = "image"`.
- `${containerEnv:HOME}` is the home variable, allowed in mounts
  (beyond the devcontainer spec).
- The `.bak` exists only during the write. It's deleted on success.
- Machines' configs are rewritten remotely, by their own daemon.
- Autostart may run before the TUI-open offer is answered. Accepted.

## Experiment (separate, after the above)

**Volume-backed repeat-instance worktrees.** Worktrees devsandbox creates
for repeat instances could live in a container volume instead of a host
bind. The host reaches their content through `host_git` (branches) and the
helper. Ownership then stops mattering for them, and file I/O on VM-backed
runtimes gets much faster. The primary instance stays a bind. Needs its
own plan if the identity work leaves gaps.

## Open questions

- No account to adopt: implicit `common-utils`, or warn and recommend it?
- Rootless docker and podman: the wrapper and the profile are tested on
  rootful docker only. Retest under both rootless runtimes (one more
  userns level) before calling them "on".
- Rootless docker runs everything as root. That's rarely a problem in
  devcontainer base images. The known frictions are tools that refuse or
  degrade as root: Homebrew refuses it; Chromium/Puppeteer/Playwright need
  `--no-sandbox`; pip and bundler only warn. Document it, and point users
  who hit it to rootless podman?
