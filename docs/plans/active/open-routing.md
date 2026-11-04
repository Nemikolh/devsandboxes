# Open routing: a container's `xdg-open` opens on the user's desktop

Status: planned, nothing started.

## Problem

A program in a sandbox that opens a URL (`xdg-open`, `$BROWSER`, Python's
`webbrowser`, Node's `open`, Go's `pkg/browser`) opens nothing: the
container has no desktop. Clicking a link in a TUI agent (grokbot), a CLI's
"press enter to open the browser" step, or an OAuth login (`gh auth
login`, `claude` login, `gcloud auth login`) fails, and the user copies the
URL by hand. An OAuth flow fails even then: its callback goes to a
loopback port in the container, which the host browser can't reach.

Once machines are involved, the host that runs the container isn't the
one the user is sitting at: a sandbox on the always-on server, attached
from the laptop, has to open on the laptop.

And the host's default browser is the wrong one for some sandboxes: work
ones should open in a work Chrome profile, personal ones in Firefox. The
OS can't tell them apart; devsandbox can, once it knows which side of the
user's life an instance belongs to.

## Goals

1. **Shadow the container's openers**: `xdg-open <url>`, `$BROWSER <url>`
   and the Debian alternatives (`sensible-browser`, `x-www-browser`,
   `www-browser`) hand the URL to the host instead of failing.
2. **Open where the user is**: on the host of the instance's most recent
   user attach, so a remote instance opens on the laptop that attached to
   it.
3. **Loopback works**: a URL pointing at the container's loopback, or
   carrying a loopback callback (`redirect_uri=http://localhost:8085/…`),
   gets a forward on the opening host, so the page and the OAuth redirect
   both reach the container.
4. **Profiles**: named collections of sandboxes (`work`, `personal`)
   declared in the config. Every instance is in exactly one, chosen at
   creation and switchable at runtime; the opening host maps the
   profile to its own browser command.

Non-goals: opening files (`xdg-open report.pdf`; the path is the
container's, see Open questions), `inotify` or any other host-side event
routing, opening anything from a service container, Apple `container` or
any instance without the helper (`Instance::devsbd_arch` `None`).

## Profiles

Profiles are bigger than this plan: they are what will later choose an
instance's credentials and its git permissions (which repo operations the
broker allows; the identity stays the same). Opening URLs is their first
use. So the name is `profile`, not `open_profile`, and an instance's
profile is a grant of what that profile holds. Switching it at runtime is
meant to change what the instance is allowed to do; the rules below keep
the switch in the user's hands.

### In the config

```toml
[profiles.work]
[profiles.personal]

[template.work]
profiles = ["work"]

[sandbox.api]
extends = "work"
```

- **`[profiles.<name>]`** declares a profile. Empty tables for now; later
  fields (credentials, git policy) go there. Names follow sandbox-name
  rules.
- **`profiles = [...]`** on a sandbox (or a template it extends: `extends`
  concatenates arrays, so a template is the collection) lists the
  profiles its instances may be in, in preference order. Absent: every
  declared profile, by name order. A name that isn't declared is a config
  error.
- **No `[profiles.*]` at all**: instances have no profile, and opening
  uses the host's default opener. Nothing changes for a config that never
  mentions profiles.
- `profiles` is left out of `sandbox_hash` (`src/config.rs`) like
  `forwardPorts`: membership is host routing and never means a rebuild.
- `skills/config-toml-spec/SKILL.md` documents both.

### Per instance

- **Stored** in `state.toml` on the instance's host: `profile` on the
  instance record, and per `(config root, sandbox)` the last profile
  chosen (`last_profile`), kept when the sandbox's last instance is
  removed.
- **At creation** (`run`): `--profile <p>`, accepted only from the
  sandbox's allowed set. Without it: the sandbox's `last_profile` if it is
  still allowed, else the first allowed. Never a prompt. `run` says which
  one it picked when more than one was allowed (`profile: work (--profile
  to choose)`). A `--profile` choice sets `last_profile`.
- **At runtime**: `instances.setProfile {root, instance, profile}` on the
  instance's host, checked against the allowed set, sets `profile` and
  `last_profile`. Nothing restarts: the container doesn't know its
  profile. It is a forwarded call for a remote instance, like
  `instances.rename`. The dashboard has a one-key action (a picker when
  more than two profiles are allowed) and `:profile <p>`; the CLI has
  `devsandbox profile <instance> [<p>]` (shows it without `<p>`).
- **Only the user switches**: `setProfile` and `--profile` are API/CLI
  only. No control request (`devsbd` `ctl.rs`, `commands::dispatch`) can
  set or name a profile, so a container can never move itself into a
  profile.
- **Dispatcher children** get their parent instance's profile. If the
  child's sandbox doesn't allow it, the spawn is refused (`denied`)
  rather than falling back, so a personal dispatcher can't create a work
  instance.
- **Config drift**: when an instance's profile is no longer allowed (the
  config changed), `ps`/the dashboard flag it and opening uses the host
  default, with a warning in `serve.log`. It isn't moved silently.
  Once profiles carry credentials, such an instance gets none.
- **Shown** in `ps`, `inspect` and the dashboard's instance rows, and in
  `instances.list` / the snapshots (`profile`).

### On the opening host: `profiles.toml`

What a profile does on this host, next to `state.toml`, host-local
because it runs commands here:

```toml
[work]
open = ["google-chrome", "--profile-directory=Profile 2"]

[personal]
open = ["firefox"]
```

- An unmapped profile, an instance with none, or no file → the host's
  opener (`xdg-open` on Linux, `open` on macOS). The URL is the last argv
  entry; stdio null like `desktop::notify_desktop`.
- **Never from the config**: the config lives on the instance's host, and
  the opening host may be another machine. Only the profile *name*
  crosses a link; the command always comes from the opener's own
  `profiles.toml`, so no machine can make another run a command.
- Later per-host profile settings go in the same table.

## Design

### In the container: the shim

- **Multicall name**: `devsbd` invoked as `xdg-open`, `sensible-browser`,
  `x-www-browser` or `www-browser` is the opener (`devsbd/src/open.rs`),
  next to the browser wrapper's argv0 dispatch in `devsbd/src/main.rs`.
  Also `devsbd open <url>` for scripts that want it without the shim.
- **Install**: `INSTALL_SCRIPT` (`src/devsbd.rs`) links the names in
  `BIN_DIR` like the browser names. And it links `/usr/local/bin/xdg-open`
  (ahead of `/usr/bin` on every default `PATH`), best-effort like the
  `devsbd` link, **only when nothing is there or it already points at
  devsbd**, so a user's own wrapper is never clobbered. That covers every
  process, not only `devsandbox exec`'s env (VS Code terminals, lifecycle
  commands, agents spawned by a dispatcher).
- **`BROWSER`**: the exec env (`tool_env::tool_env`) sets
  `BROWSER=/run/devsandbox/bin/xdg-open` on every plan, unless the
  sandbox sets it (`user_keys`). Node's `open` uses its own vendored
  `xdg-open` script, which falls back to `$BROWSER` when it finds no
  desktop, and Python's `webbrowser` reads `$BROWSER` first, so the
  `PATH` link alone misses both.
- **Behaviour**: accepts exactly one argument. A URL with a scheme →
  queued, exit 0. Anything else (a path, no argument) → `xdg-open`'s exit
  codes: 1 for usage, 3 for "devsbd: opening files isn't supported; open
  URLs only". The shim checks syntax and length only (no dependencies,
  [ADR 0022](../../decisions/0022-helper-has-no-dependencies.md)); the
  host decides what's allowed.
- **Record**: a new `kind open` in `src/devsbd/notify.rs` (`at`, `url`)
  queued through the outbox and poked like `devsbd notify`. The outbox
  keeps it when no host is attached, so the host drops `open` records
  older than `OPEN_TTL` (30 s) at delivery: an OAuth URL opening an hour
  later is worse than none. A dropped one is logged to `serve.log` with
  the URL.

### On the instance's host: intake

- **Dispatch**: the bridge's sink (`Bridges::spawn_worker`,
  `src/devsbd/bridge.rs`) routes `kind open` to the opener, not to
  `apply_message` / the Inbox store, so removing the Inbox later doesn't
  touch it. The `ok` reply means "taken", not "opened": opening runs on
  its own thread.
- **Policy** (`src/open.rs`, pure): schemes `http` and `https` only;
  anything else is refused and logged. A per-instance rate limit like
  `desktop::RateLimit` (say 5 per 10 s), so a looping script can't flood
  the desktop with tabs.
- **Profile**: the instance's `profile` from `state.toml` (none when it
  isn't allowed any more, see above) travels with the URL.

### Delivery: which host opens

- **Last attach wins**: every daemon keeps, in memory, per instance, the
  member that opened the latest user attach: a local `exec` / dashboard
  terminal tab / `devsandbox vscode` = this host; a `PTY` stream over a
  link or route = its proven origin (`serve::fleet::pty`,
  [ADR 0045](../../decisions/0045-a-routed-stream-proves-its-origin-per-stream.md)).
  The record goes there. Nothing attached yet, or the latest attach was
  local → open here if this host has a display; otherwise refused and
  logged (`no attached client to open <url> on`).
- **Display**: a Linux host has one when the daemon's env has `DISPLAY` or
  `WAYLAND_DISPLAY`. The systemd unit (`serve::install`) must carry the
  session's display env: check what `import-environment` gives it on
  GNOME/KDE, and if needed add a `PassEnvironment` line or document a
  one-time `systemctl --user import-environment DISPLAY WAYLAND_DISPLAY`.
- **Over the fleet**: a new `open.deliver {instance, root, url, profile}`
  fleet call to that member, which maps `profile` with its own
  `profiles.toml`.
- **Receiver's check**: a member only accepts `open.deliver` for an
  instance it attached to itself within the last `OPEN_ATTACH_WINDOW`
  (attach still live, or ended < 10 min ago). It knows this without
  asking anyone (it originated the attach), so no machine can drive
  another's desktop by naming an instance that member never touched,
  whatever its trust. On the trust table under both `worker` and `full`.
- **Feedback**: the opening daemon pushes `opens.done {instance, url,
  profile, outcome}` to subscribed connections; the dashboard shows it as
  a status line (`x: opened https://… (work)` / `x: open refused: scheme
  ssh`). A new notification, not `inbox.shown`.

### Loopback forwards

Run by the opening host before it opens (pure parse in `src/open.rs`,
forwards through the existing registry):

- **Direct**: a URL whose host is `localhost`, `127.0.0.1`, `[::1]` or
  `0.0.0.0` → a forward to that instance port, `HostPort::Prefer(port)`,
  and the URL is rewritten to the bound port if it isn't the same one.
- **Callback**: a loopback URL in the query values `redirect_uri`,
  `redirect_url` or `callback` → a forward with `HostPort::Fixed(port)`:
  the provider redirects to exactly that port, so a rewrite can't help.
  If the port is taken, the URL still opens and `serve.log` / the
  dashboard status line say which port blocked the callback.
- **Reuse**: an existing forward of the same instance port (configured
  `forwardPorts` or ad-hoc) is used as it is.
- **Lifetime**: opener forwards are a third kind next to ad-hoc and
  configured (`serve::forwards`): shown in the Ports tab marked `open`,
  never written to `forwards.toml`, stopped after `OPEN_FORWARD_IDLE`
  (10 min) with no connection, or when the instance stops. On a remote
  instance they are remote forwards (`forwards.add` with `machine`,
  `tunnel::remote_opener`) owned by the opening host, as today's remote
  forwards are.

## Steps

1. **Local open**: the shim and its links, `BROWSER`, the `open` record
   kind with `OPEN_TTL`, sink dispatch, `src/open.rs` policy, opening on
   this host with the system opener. Docker test: `xdg-open
   https://example.test` in a container reaches the opener (opener
   injectable, like `Runner` in `serve::verbs`).
2. **Profiles in the config and state**: `[profiles.*]` and `profiles`
   (parse, merge, validation, out of `sandbox_hash`, the spec skill),
   `profile` / `last_profile` in `state.toml`, `run --profile` (CLI and
   `serve::verbs`' `run` params) with the default rule, dispatcher
   children inheriting, `profile` in `ps` / `inspect` / `instances.list`.
3. **Switching and the host map**: `instances.setProfile` (local and
   forwarded), `devsandbox profile`, the dashboard action and
   `:profile`, `profiles.toml` and opening with the profile's command.
4. **Loopback forwards**: the parse, the `open` forward kind, Ports tab
   marker, idle stop.
5. **Delivery over the fleet**: last-attach tracking, `open.deliver`, the
   receiver's check, remote opener forwards.
6. **Dashboard feedback**: `opens.done` and the status line.

Steps 1-4 are useful without machines; 5 is what makes it work for a
sandbox on the server.

## Docs on shipping

- `docs/architecture/helper/in-container.md`: the opener names, the
  install links, `devsbd open`.
- `docs/architecture/helper/protocol.md` (`notify`): the `open` kind.
- `docs/architecture/helper/bridges.md`: sink dispatch, TTL, policy.
- `docs/architecture/daemon/forwards.md`: the `open` forward kind.
- `docs/architecture/daemon/fleet.md`: `open.deliver`, last attach, the
  receiver's check.
- A new `docs/architecture/instances/profiles.md`: profiles, the
  per-instance rules, `profiles.toml`; `docs/reference/api.md` for
  `instances.setProfile` and `opens.done`.
- `skills/config-toml-spec/SKILL.md`: `[profiles.*]`, `profiles`.
- An ADR: an instance's profile is the user's choice (never the
  container's), and only profile names cross machines.
- `CHANGELOG.md` Unreleased.

## Open questions

- **Files**: `xdg-open path` could go to the VS Code window attached to
  the instance (`devsbd vscode-goto`), or to the host's opener when the
  path is in a bind-mounted workspace (mapped to its host path). Left out
  until there's a use case.
- **Terminal hyperlinks**: an OSC 8 link clicked in the dashboard's
  embedded terminal (`tui::term`) never reaches the container, so this
  plan doesn't cover it. The dashboard could open it the same way (the
  row's profile, `profiles.toml`); a dashboard question.
- **Confirmation** for schemes beyond `http(s)` (`mailto:`, `vscode:`)
  needs a prompt routed to the user; it fits the broker's "ask" route
  later better than an ad-hoc one here.
- **Several attached members**: last attach wins. Opening on every member
  attached right now may be what users expect with two screens; wait for
  a real case.
- **Profile switch with credentials in play**: once profiles carry
  credentials, what a switch does to credentials already handed out
  (revoked at once, or left to expire) belongs to the broker's plan.
