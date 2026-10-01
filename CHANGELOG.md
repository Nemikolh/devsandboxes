# Changelog

## Unreleased

### Added

- **`devsbd branches <sandbox>` tells a dispatcher which branches are already checked out.** It prints a JSON list of every branch in a worktree of the sandbox's repo and who holds it: one of the dispatcher's own children, another instance, the base checkout, or a worktree devsandbox doesn't know. It reads git on the host each time, so a `git switch` inside an instance is seen. With `--ahead`, each row also gets the number of commits `origin` doesn't have yet, and local branches that are ahead but not checked out are listed too. A dispatcher can use it to leave alone PRs that someone is working on locally. The sandbox must be in the dispatcher's `spawn`. Running dispatchers pick up the new helper when restarted.

### Changed

- **`folders` entries get their own git worktree when the checkout belongs to someone else.** If an extra folder is a git repo and it's another sandbox's `folder`, or another instance already mounts it, the instance now gets a worktree of it, detached at that checkout's `HEAD`. Before, the instance mounted the live checkout, so the owner switching branches changed what the instance saw. The worktree is kept across `rebuild` and removed by `rm`. Dispatcher children always get one. To keep the old live view for an entry, write it as a table with `worktree = "never"`, or use `"always"` to force a worktree:

```toml
[sandbox.web]
folder = "../web"
folders = { "/workspaces/api" = "../api", "/workspaces/.shared" = { path = "../.shared", worktree = "never" } }
```

- A sandbox's own `folder` now also gets a worktree when another instance mounts that folder directly as a `folders` entry.

- **The Inbox keeps its history.** Notifications are saved in `inbox.toml` next to `state.toml`, so they survive closing the dashboard; `d`/`D` remove them from the file too. A notification with the same `--key` no longer replaces the earlier one: it becomes the head of a thread, with the older ones folded under it (`→` to show them). With more than one instance sending, the Inbox groups them per instance, foldable like the Instances tree. The limit is now 200 notifications per instance (was 50, and 200 overall).

- **`worktree-link` no longer migrates old `shared-files/<sandbox>/` stores.** The automatic move only relinked the instance being run, so every other instance on the same repo was left with dangling links. A link into an old store is now skipped with a warning that shows where the file should go. Move it there, delete the old link, and `rebuild`.

## 0.5.0

Sandboxes can now start themselves after a reboot and run unattended **dispatcher** scripts that create, drive and clean up their own child instances. Messages from inside containers show up in the dashboard and as desktop notifications. See `docs/automations-guide.md`. `run --json`, a command-less `exec` and exact-name matching make devsandbox easier to drive from scripts and other programs, and the docs now live at [devsandboxes.com](https://devsandboxes.com).

### Added

- **`autostart`**: a sandbox's instances come up once per boot, when the dashboard opens or after your first `devsandbox run` / `start`. A sandbox with no instance yet gets one; dispatcher children are left to their dispatcher.
- **`autostart = "runtime"`**: docker and podman restart the containers themselves at boot (`--restart unless-stopped`), with no devsandbox process needed. Podman also needs `podman-restart.service` enabled. On Apple `container` it behaves like `true`. Changing `autostart` never marks an instance as drifted; the next `start` applies it in place.

```toml
[sandbox.triage]
folder = "../triage"
autostart = "runtime"   # or true
```

- **Boot hook**: new containers run `devsbd boot` on every start, which brings back the helper daemon and, in runtime mode, `postStartCommand` (output in `/run/devsandbox/boot.log`). Containers created before this need `devsandbox rebuild --force` to get it.
- **`devsbd notify`**: any sandbox can send the user a message. It is queued inside the container until a dashboard is open, then shown in the new **Inbox** tab (unread badges on instance rows, `enter` opens an `http(s)` link, `d`/`D` dismiss) and as a desktop notification (`notify-send` / `osascript`).

```bash
devsbd notify --level warn --key pr-123 --link https://github.com/o/r/pull/123 "PR 123 needs you"
```

- **Dispatchers**: a sandbox that declares `dispatcher` can manage child instances through `devsbd`, while the dashboard is open. Children are named `<sandbox>-<key>`, always get their own worktree, are marked `⇠ <dispatcher>` in the TUI, and outlive the dispatcher. Host-side operations log to `<data>/devsandbox/logs/dispatch-*.log`. Exit codes: 75 when no dashboard is connected, 77 when denied.
  - `spawn = ["*"]` means every sandbox in the config root except other dispatchers, and it grants their mounts, docker socket and privileges. Children can never be dispatchers themselves.
  - `max-instances` defaults to 10 and counts stopped children too.
  - `--branch` accepts plain branch names only (`[A-Za-z0-9._/-]`) and is used literally, with no `${…}` expansion.
  - `--env` refuses names that could take over processes in the child: `PATH`, `HOME`, `SHELL`, `LD_*`, `DYLD_*`, `GIT_*`, `NODE_OPTIONS`, `PYTHON*` and similar. Its values are kept across a `devsandbox rebuild` of the child, including when `ensure` recreates a missing container. On an existing child, `ensure --env` replaces the saved values, and every command devsandbox runs in the child (`exec`, dashboard terminals, `devsbd exec` runs, lifecycle commands) uses them from then on, over a same-named `remoteEnv`; already-running processes keep the old ones until a rebuild.

```toml
[sandbox.pr-dispatcher]
folder = "../pr-dispatcher"
autostart = "runtime"
dispatcher = { spawn = ["web"], max-instances = 10 }   # "*" = any non-dispatcher sandbox
postStartCommand = "nohup ./loop.sh >loop.log 2>&1 &"
```

```bash
devsbd ensure web --key pr-123 --branch feat/x   # create or start web-pr-123
devsbd exec pr-123 --detach -- zidane -p "babysit PR 123"
devsbd run logs pr-123 <id> --follow
devsbd ls; devsbd stop pr-123; devsbd rm pr-123
```

- **Runs**: commands started with `devsbd exec` are tracked in the child (id, log, exit status). Follow them with `devsbd run ls|logs|wait`, clean them up with `devsbd run rm` (`--force` kills a running one) and `devsbd run prune --keep N`; the TUI shows a child's last runs under its processes.
- `devsbd` is on `PATH` in containers (`/usr/local/bin/devsbd`).
- **`forwardPorts`**: while the dashboard is open it forwards each running instance's configured ports to `127.0.0.1`, the same way `devsandbox port` does (no container restart). Each instance keeps its host ports across dashboard restarts, and a port already taken moves to the next free one up. A global service's port is forwarded once, however many instances list it. In the Ports tab these rows are marked `(config)`; `d` stops one until the dashboard reopens.

```toml
[sandbox.api]
services = ["db"]
# 3000, "db:5432" as in devcontainer.json; "8080:3000" / "15432:db:5432" pick the host port
forwardPorts = [3000, "8080:3000", "db:5432"]
```

- **`run --json`**: prints one `{"schema": 1, "data": {...}}` document for the new instance (`name`, `instance_id`, `sandbox`, `container`, `workspace`, `folder`, `base_folder`, `worktree`, `branch`; the last two `null` without a worktree). Everything child processes print (image builds, `docker run`, git, lifecycle hooks, autostart) goes to stderr, so stdout parses as-is for programmatic and GUI callers.
- **`exec <name>` without a command** opens a login shell (zsh, else bash, else sh), the same one the dashboard's terminal opens. With neither `-i` nor `-t` given it runs with `-i`, plus `-t` when stdin is a TTY, so it works as is from a terminal or a pty (node-pty); explicit flags are used as given.
- **npm: `execArgv(name, cmd?, { tty, interactive })`** returns `{ file, args }` for spawning `devsandbox exec` yourself (node-pty, `child_process.spawn`) without shell quoting.
- **`rm --delete-branch` / `rm --keep-branch`** answer the "delete branch?" question without prompting, so scripts and other callers without a TTY can clean up the branch `run` created. `--delete-branch` deletes it like answering yes (`git branch -D`, so unmerged commits go too); `--keep-branch` keeps it. Branches `run` reused rather than created are always kept. With neither flag, `rm` still asks on a TTY and keeps the branch otherwise. npm: `rm(name, { deleteBranch })`, where `true` / `false` pass `--delete-branch` / `--keep-branch`.
- **Docs site at [devsandboxes.com](https://devsandboxes.com)**: a quick start, a playable tour of the dashboard, the full `config.toml` reference, the Node API reference, ten worked examples (parallel coding agents, shared and per-instance services, package caches, ssh-agent and `git push`, babysitting pull requests, porting a `devcontainer.json`, …), full-text search (⌘K / Ctrl+K) and this changelog.

### Changed

- **Worktrees reuse existing branches.** `run --branch X`, and a dispatcher's `devsbd ensure --branch X`, no longer fail when `X` exists:
  - a local `X` is checked out as it is;
  - an `X` that only exists on `origin` (a PR head, for example) becomes a local branch tracking `origin/X`, so a plain `git push` updates it;
  - a branch that's already checked out somewhere else gives an error saying where.

  `rm` only offers to delete branches devsandbox created itself.
- New containers use `sh -c '… devsbd boot & exec sleep infinity'` as their command instead of `sleep infinity`, so images need `/bin/sh`.
- `start` sets the container's restart policy from `autostart`, overwriting a policy set by hand.
- While the dashboard is open it keeps a helper connection to every running instance that has the helper, not only when an ssh-agent is being relayed.
- `devsandbox status --json` instance rows include `instance_id` and, for children, `dispatcher`.
- **`rebuild` applies `worktree-include`.** Before, adding a pattern marked instances as drifted, but the rebuild copied nothing. Now `rebuild` copies any missing matching files into the worktree; existing files are never overwritten. A copied file you delete on purpose comes back on the next rebuild, so drop the pattern instead.
- `devsandbox port 3000` (and the Ports tab) moves to 3001, 3002, … when 3000 is taken on the host, instead of a random port.
- A service `ports` entry that gives only the container port (`"5432"`) now gets the same port on the host when it's free, instead of a random one. If the port is taken, the runtime still picks one.
- **An exact instance name wins.** With instances `web` and `web-2`, `devsandbox exec web` (and `rm`, `stop`, `logs`, …) now means the instance `web` instead of an ambiguity that failed off a TTY and prompted on one, including inside a pty. An exact instance id counts too; sandbox and folder names only apply when nothing matches exactly. `exec` now uses the same resolution as the other verbs, so it also accepts an instance's id after a rename.
- **npm (breaking): `run()` resolves with the new instance's record** (`RunRecord`) instead of its name; use `(await run(...)).name` for the old value. It calls `run --json`, so build and autostart output can no longer end up as the "name".
- In the generated VS Code workspace, the main folder is labelled with its directory name (`webcontainer`) instead of the instance name (`webcontainer-sqlite`), like the extra `folders` roots. If an extra root has the same directory name, the instance name is kept. Existing instances pick this up on `rebuild --force`.

### Fixed

- **`rm` on a dirty worktree no longer leaves an instance without its container.** `rm` removed the container, volumes and services first, then `git worktree remove` refused the uncommitted or untracked changes, stranding the instance. It now checks the worktree first and refuses with the list of changes, removing nothing.
- **Keyboard chords like ctrl+m reach apps in dashboard terminals.** In terminals that support the kitty keyboard protocol (kitty, Alacritty ≥ 0.13, Ghostty, WezTerm, foot, iTerm2 ≥ 3.5), the dashboard now turns the protocol on and passes it through to apps inside its terminals that ask for it. zidane's ctrl+m, for example, no longer arrives as Enter. Shells and other apps that don't ask for it get the same keys as before. GNOME Terminal and tmux don't support the protocol, so there ctrl+m and Enter still can't be told apart.
- **The mouse wheel scrolls full-screen apps in dashboard terminals.** It used to scroll only the terminal's own scrollback, which full-screen apps don't have. Now apps that track the mouse (zidane, vim with `mouse=a`, htop) get the wheel directly, and pagers like `less` and `man` get ↑/↓. Shells still scroll their history.
- **Forwarded ports reach servers listening only on IPv6 loopback.** Dev servers told `localhost` (webpack-dev-server, Vite, Node 17+) often bind only `[::1]`, while forwards dialed `127.0.0.1`, so every connection was refused. A loopback dial now also tries the other family (`127.0.0.1` ↔ `::1`). Running instances pick up the new helper when restarted.
- **`worktree-link` works for several sandboxes on the same folder.** The shared copy used to live in one store per sandbox. The base checkout can only link to one of them, so the second sandbox's worktrees got nothing. The store is now per repo: `shared-files/<folder name>-<hash>/`, shared by every sandbox on that folder. The next `run` or `rebuild` moves files from the old `shared-files/<sandbox>/` stores and relinks the base checkout and worktrees. Instances still running on an old store need a `rebuild`. If two old stores held different copies of a file, the one the base checkout linked to wins, and the other stays where it was with a warning.
- **ssh-agent forwarding no longer breaks `run` on macOS + docker in builds without the helper** (`cargo install`). The fallback bind-mounted the Mac's launchd agent socket, which can't cross into the runtime VM, so `docker run` failed with `mkdir …/agent/<instance>.sock: file exists`. It now mounts Docker Desktop / OrbStack's `/run/host-services/ssh-auth.sock` instead. Builds that embed the helper were never affected.
- **`devsandbox port` and the Ports tab work on macOS.** Accepted connections inherited non-blocking mode from the listener there, so every relay read failed with `os error 35` and no forwarded connection got through.
- **0.4.0 never reached crates.io** (the publish step failed on the release archives in the checkout); `cargo install devsandbox` goes straight from 0.3.4 to this release.

### Security

- **Git on the host can no longer run code planted by a sandbox.** Worktree instances mount the base repo's `.git` read-write, so a container could plant hooks or command-running config (`core.fsmonitor`, `core.sshCommand`, filter drivers, `include.path`, ...) that the host's `git worktree add` / `fetch` / `worktree remove` would then run as you.
  - Every git command devsandbox runs on the host now has hooks and fsmonitor turned off.
  - It first checks the repo's local config against an allowlist. If a key could run a command, it refuses and names the key: review it and remove it. `ext::` remotes and object alternates are refused as well.
  - This affects every worktree instance, not only dispatcher children.
- The commands devsandbox runs as root inside containers (helper install, boot file, Apple `/etc/hosts` update, workspace file) use `/bin/sh` with a fixed `PATH`, so they don't depend on the container's `PATH`.
- **Limits on what a container can make the host do:**
  - at most 8 concurrent notify/control handlers per container;
  - output from host-run commands is capped and timed out (5 min for in-child commands, 30 min for dispatch operations, which are killed with their whole process group);
  - desktop popups are rate-limited per instance (3 at once, then one every 10 s), and repeats of the same key within 60 s don't pop up again;
  - the inbox keeps at most 50 entries per instance (200 overall);
  - `devsbd run ls` lists only the newest 50 runs and shortens long command lines.

<details><summary>Commits</summary>

- c0af00e feat(site): add a changelog page rendered from CHANGELOG.md
- d70eb16 fix(npm): keep execArgv names from parsing as flags, correct exec/run docs
- d5e0884 fix(rm): refuse a dirty worktree before tearing anything down, add branch flags
- 623710f feat(cli): make run, exec and name resolution usable from programs
- 88a16b9 docs(site): add a dashboard tour page that walks through each feature beside a sticky, playable mini dashboard, so readers learn the tui before installing it
- e3d2b28 feat(site): fake the command prompt, port forwards, logs, config explorer and run/stop in the mini dashboard, so the docs can teach every key hands-on
- ef295b7 docs(tui): document kitty keyboard pass-through and wheel routing for integrated terminals
- 47a6a19 docs(tui): correct the pty TERM comment, docker exec does not forward it into the container
- ba99a4f fix(tui): answer xtversion in integrated terminals, since opentui apps like zidane ignore the kitty flags reply until the terminal identifies itself
- 52d9194 fix(tui): send the mouse wheel to integrated-terminal apps that track the mouse or use the alternate screen, which has no scrollback to scroll
- fb9e02d feat(tui): send kitty-encoded keys to integrated-terminal apps that enabled the protocol, so ctrl+m no longer arrives as enter
- 95c6236 feat(tui): emulate the kitty keyboard protocol's flag stacks and queries in the integrated terminal, so apps inside can opt into it
- 0ac6963 refactor(site): extract the mini dashboard into a mountable component, so the docs can host a second one driven from outside
- dd3ad31 feat(tui): enable the kitty keyboard protocol on terminals that support it, so chords like ctrl+m can reach apps in the integrated terminal
- 721b5ab feat(site): make t open a terminal in the landing mini dashboard, split and tabbed like the real one, so the preview shows the feature it advertises
- 7ff786f fix(site): tighten the examples that need no setup and make the pr babysitter readable at a glance
- b126a11 docs(site): trim the pr babysitter example to the new example shape and give its loop a codex variant, so it works with the reader's agent
- f5cb4c1 docs(site): trim the features and devcontainer porting examples to the new example shape, keeping the porting reference tables readers come for
- a1ee4f4 docs(site): trim the port forwarding and vs code examples to the new example shape, and stop calling forwardPorts ignored now the dashboard honours it
- c0d24e9 docs(site): trim the caches and ssh push examples to the new example shape, with the agent picked by the reader on the push one
- 2b6c5ae docs(site): trim the shared and isolated services examples to the new example shape, with the agent picked by the reader on the isolated one
- 06fb312 feat(site): let readers pick their coding agent once and trim the parallel agents example, so examples read short and match the agent they use
- 0e41f55 feat(site): animate the how-it-works section and make the mini dashboard playable, so pressing o shows vs code attaching to a sandbox
- ab6d22b feat(site): how-it-works section with a host/sandboxes/agents diagram next to a mini dashboard, so visitors see how the tool is used
- 59e5404 feat(site): position the landing page as devcontainers for ai agents and drop the devcontainer comparison the audience doesn't need
- b0b5a9b fix: add github icon to link
- edda15a ci(site): move pages deploy into a dispatchable workflow that release calls, so docs-only changes can ship between releases
- 9e23dc4 ci(site): build the docs site on every pr and deploy it to pages on release, so the docs always match what users can install
- 83bd81b fix(site): load sidebar nav styles on every page, so the mobile drawer outside /docs has no bullets or stray chevrons
- 5c0961c docs(config): escape the pipes that truncated the dispatcher row and name the real service/gc commands
- 9400e75 feat(site): 404, og image, one reading order with prev/next and edit links, a link check in the build, and a warning-free build
- 874d74b feat(site): pagefind full-text search behind a ⌘k dialog, so any field, flag or example is one keystroke away
- 6dd530f feat(site): remaining examples, port forwarding, vscode, features, the pr babysit dispatcher and porting a devcontainer.json
- f140d5f feat(site): examples section with one recipe per use case and a copyable agent prompt, starting with worktrees, services, caches and ssh
- 2fc8c4b feat(site): node api reference generated from the npm package's index.d.ts, so the docs follow the published typings
- d127052 feat(site): config reference rendered from the config-toml-spec skill, so the site and the agent skill never drift
- ce29397 feat(site): quick start page, from install to a second worktree instance and the dashboard
- fc10522 feat(site): landing page that pitches devsandboxes as devcontainers multiplied, with a hero that mirrors real run/ps output
- 7454a57 feat(site): scaffold the astro docs site with the prototype's design system, so docs pages can be written against a real shell
- cd577ab feat(port): forward sandboxes' forwardPorts from the tui, keeping each instance's host ports in state so they stay the same across restarts
- 70553ce feat(services): give a bare service port the same host port when it's free, instead of letting the runtime pick a random one
- 043c1de fix(port): clear o_nonblock on accepted sockets, since macos inherits it from the listener and every relay read failed with os error 35
- a0061d2 fix(port): try the other loopback family when dialing, so dev servers bound only on ::1 (node's localhost) are reachable
- 44f357a fix(ssh-agent): mount docker's magic agent socket on macos, since the launchd socket can't cross into the vm and failed docker run
- 5607ad3 fix(worktree): keep worktree-link files in one store per repo, so several sandboxes on the same folder all get the shared files
- fb94fab feat(dispatch): ensure --env replaces a child's saved env and every exec applies it, so new values take effect without a rebuild
- 37fc704 docs(changelog): list the step 13-15 commits
- 9d7efc8 feat(dispatch): devsbd run rm and run prune, so dispatchers can clear runs instead of letting them pile up
- 2608a23 fix(autostart): never start or create dispatcher children at boot, so idle children the dispatcher parked stay stopped
- 54193b6 feat(run): keep --env values in state so rebuilds, including a dispatcher recreating a child, don't drop them
- 57237dc fix(rebuild): copy worktree-include files on rebuild so the drift a new pattern raises is actually fixed by rebuilding
- 2feb352 docs(changelog): unreleased notes for the security hardening and existing-branch worktrees
- d022e5c feat(run): reuse an existing local or origin branch for a worktree instead of refusing, so instances and dispatcher children can work on PR branches
- 36f532a fix(port): look up lsof on the container's PATH again, the --env deny-list already closes the PATH injection and the fixed PATH hid lsof in /usr/local/bin
- e5404ac fix(devsbd): cap run ls to the newest 50 runs and bounded argv/meta reads, so a container user can't make run listings unbounded
- aac1539 fix(dispatch): bound handler threads, child output and runtimes, and rate-limit desktop popups, so a container can't exhaust the host or flood the desktop
- 4c1549f fix(dispatch): deny dangerous --env names, keep children from being dispatchers, cap children at 10 by default and run host root execs with a fixed PATH, so a dispatcher can't gain root in a child or self-replicate
- 11b9f7a fix(dispatch): validate dispatcher branch names and never template-expand them, so a container can't read host env through ${localEnv:…}
- a5948da fix(run): host git runs with hooks and fsmonitor disabled and refuses repos whose local config can run commands, so a sandbox can't execute code on the host through the shared .git
- c59d627 docs(automations): plan security fixes for host git, branch values, dispatcher authz and resource limits
- f44f329 docs(changelog): unreleased notes for automations, and plan steps for the follow-up decisions
- 5c2e49a docs(automations): user guide, architecture and module map for autostart, notify and dispatchers
- b080bf1 fix(autostart): keep the boot's pass when the runtime is unreachable and pin dispatch subprocesses to the parent's backend, found auditing apple container
- 7868cb5 feat(dispatch): tracked runs in child instances (devsbd exec, run ls/logs/wait), so dispatchers can start agents and follow their outcome
- d499764 feat(dispatch): devsbd ensure/ls/stop/rm over a control channel served by the dashboard, failing fast with 75 when no host is attached
- 32a030d feat(dispatch): dispatcher config and host-side control handler that runs ops as logged subprocesses, so a sandbox can safely own child instances
- d5a1f6f feat(tui): inbox tab with per-instance unread badges, so notifications stay reviewable after the status line moves on
- cec26b4 feat(tui): serve devsbd notify streams from dashboard bridges and raise desktop notifications, so container messages reach the user
- 35f2e7c feat(devsbd): notify queues records in a durable outbox and the daemon flushes them to notify-capable hosts, so no message is lost while no host is attached
- c3ae6ce feat(devsbd): boot hook in the container command so runtime restarts bring back the helper daemon and postStartCommand without a host
- 75345d6 feat(autostart): runtime mode sets --restart unless-stopped so docker/podman bring sandboxes back on boot without devsandbox, flipped in place on start
- 9075dec feat(autostart): bring autostart sandboxes up once per boot from the tui or the first run/start, so automations survive a reboot
- 74ff407 docs(automations): step plan, with triggers, per-root boot ids and runtime-mode poststart settled
- 6bb660e docs(automations): design for autostarted sandboxes, notify and dispatcher-driven child instances
- 0442d37 fix(ci): download release archives outside the checkout so cargo publish sees a clean tree

</details>

## 0.4.0

Worktree instances can now bring along the gitignored files a repo needs to run, like `.env` files, instead of starting without them.

### Added

- **Copy gitignored files into new worktrees.** A repo's `.worktreeinclude` file is always honored, and `worktree-include` adds more patterns (gitignore syntax). Matching untracked files are copied from the base checkout into each **new** worktree and never overwrite existing files.
- **Share gitignored files across all instances.** Each `worktree-link` path is moved to one shared copy under `shared-files/<sandbox>/`, and every worktree gets a symlink to it. An edit in one instance shows up in all of them. This also works on Apple `container`, which can't bind single files. Globs work within one path segment (`*` / `?`), and a leading `./` is accepted.

```toml
[sandbox.web]
folder = "../web"
worktree-include = ["fixtures/*.local.json"]   # copied once, per worktree
worktree-link = [".env", "packages/*/.env"]    # one live copy shared by every instance
```

- A `CHANGELOG.md`, which is also used as the GitHub release notes.

### Fixed

- TUI: pressing `s` to start an instance refreshes the list shortly afterwards, so the new state appears right away.

<details><summary>Commits</summary>

- 52da316 docs(changelog): readable release notes with examples, written ahead of release via a release skill
- 9dff820 docs(changelog): backfill changelog and generate it on release so github releases carry notes
- c6925a0 feat(run): one-segment globs and leading ./ in worktree-link
- ebd7200 feat(run): carry gitignored files into worktrees via copy or shared link
- d836a5a fix(tui): when start is pressed, refresh instance list after 500ms
- 7005680 chore(ci): fix warnings reported

</details>

## 0.3.4

On-demand port forwarding, devcontainer features that need privileges or entrypoints (docker-in-docker works now), and a full `config.toml` reference.

### Added

- **`devsandbox port`**: makes a port inside an instance or one of its services reachable on the host, without publishing it when the container is created and without a restart. Forwards reconnect by themselves after the target restarts or is rebuilt. When the image has `lsof`, the command shows which process is listening. Unix hosts only.

```bash
devsandbox port api 3000                      # localhost:3000 -> api's :3000
devsandbox port api 8080:3000                 # bind host :8080 instead
devsandbox port api --service postgres 5432   # api's postgres, via the api container
devsandbox port --service redis 6379          # a global service, no instance named
```

- **TUI Ports tab**: lists active forwards; `p` adds one and `d` removes one. Quitting never hangs on a forward that is still connecting.
- **Privileged features and feature entrypoints.** A feature that declares `privileged: true` now runs the container `--privileged`, and feature entrypoints run at every container start, so features such as docker-in-docker work out of the box. `privileged` can also be set directly on a sandbox. Both are skipped with a warning on Apple `container`.

```toml
[sandbox.infra]
image = "mcr.microsoft.com/devcontainers/base:ubuntu"
[sandbox.infra.features]
"ghcr.io/devcontainers/features/docker-in-docker:2" = {}
```

- **`config-toml-spec` skill**: the complete reference for `config.toml` (every key, merge rules, `${…}` variables), written for both people and agents.

### Fixed

- `${devcontainerId}` in feature mounts resolves to the instance id. Per-instance volumes are deleted on `rm` and kept on `rebuild`.
- A sandbox mount on the same target now replaces a mount inherited from a template, instead of Docker rejecting the duplicate.
- Nested mounts are applied parent-first on every runtime, not only on Docker.

### Internal

- `run.rs` and the TUI `app.rs` are split into per-concern modules. Docker-dependent tests are now required to pass on CI.

<details><summary>Commits</summary>

- ff124fb fix: don't specify the model on the skill
- e45afbd docs: fix broken rustdoc intra-doc links
- 75e1732 refactor(tui): split app.rs into per-concern modules
- 704bbaf refactor(run): split run.rs into per-concern modules
- 155b3de fix(features): resolve ${devcontainerId} and reap per-instance volumes on rm
- 4f1512c feat(features): run feature entrypoints at container start
- 7fe0e4f feat(config): honor sandbox-level `privileged` instead of ignoring it
- feb8ec7 feat(features): honor feature `privileged` so docker-in-docker style features work
- facce5f fix(run): order mounts parent-first before handing them to the runtime, since only docker sorts nested mounts itself and that is undocumented
- c060d7c docs(skills): add a config-toml-spec skill as the full config.toml reference, so users get one accurate source and devsandbox-cli stops carrying a drifting copy
- a71ea70 fix(run): let a downstream mount on the same target replace an inherited template mount, since only feature mounts could be overridden and docker rejected the duplicate target
- aa37f71 test(ci): build the devsbd helper and run the whole workspace on ci, and gate docker/helper tests behind #[docker_test] so they skip locally but fail on ci instead of silently passing
- 2d1bb66 docs(repo): drop references to internal project names from docs and test fixtures
- e7c0d57 test(devsbd): retry the prefer-port test with a fresh port when a parallel test grabs the released one first
- 6df4d8b test(devsbd): retry the embedded helper exec on etxtbsy, since parallel tests' forks can briefly inherit the just-written binary's write fd
- 8bee843 docs(port-forwarding): document the forwarding protocol, flow control and caps routing, plus the port command and ports tab, so docs match what shipped
- fdcf7e7 feat(tui): run ports-tab forwards on a worker thread that owns them, and make forward teardown prompt so quitting never hangs on a handshake
- 5ef2538 feat(tui): add a ports tab with a port prompt and p/d shortcuts so forwards can be requested and listed from the dashboard
- cea5d40 feat(port): show which process listens on a forwarded port via lsof when the image has it
- dfe7777 feat(port): add a foreground devsandbox port command that forwards instance or service ports until ctrl-c
- f1e6783 feat(port): resolve forward routes to an instance, a service via a running instance, or the service container itself, re-evaluated on every reconnect so restarts and rebuilds heal
- 07e5a0f feat(devsbd): add the host forwarder engine that listens locally and tunnels each connection through a self-healing bridge
- 81de9a5 feat(devsbd): dial forwarded connects in the daemon and negotiate caps end to end so forwards only run against capable helpers and agent-less bridges never take ssh-agent routing
- 9328728 feat(devsbd): add per-stream credit flow control, half-close and host-initiated streams to the mux so bulk forwarded traffic can't stall other streams or the keepalive
- 2cc5523 refactor(devsbd): let mux streams be tcp as well as unix sockets so forwarded ports can reuse the stream routing
- 95a1904 feat(devsbd): add connect/window/eof/caps frames and close reasons so port forwarding can land without a protocol version bump
- c038810 docs(port-forwarding): plan on-demand port forwarding over devsbd so dev servers bound to localhost and services are reachable without recreating containers
- a7a3b7b chore(release): keep the readme's pinned download links on the released version
- dc734b1 docs(readme): recommend npm and release archives, which embed devsbd, over the limited cargo install; document the relay as the main ssh-agent path

</details>

## 0.3.3

Release pipeline only: npm packages are now published with npm trusted publishing (OIDC) instead of a long-lived token. No changes to the CLI.

<details><summary>Commits</summary>

- cff095b ci(release): publish to npm with trusted publishing instead of a long-lived token

</details>

## 0.3.2

The npm root package is renamed to **`devsandboxes`**, because npm rejected `devsandbox` as too similar to an existing package. The installed binary is still called `devsandbox`.

```bash
npm i -g devsandboxes
npx devsandboxes --help
```

<details><summary>Commits</summary>

- 53cedc3 fix(npm): rename the root package to devsandboxes since npm rejects devsandbox as too similar to dev-sandbox

</details>

## 0.3.1

The CLI is now published to npm, so a Rust toolchain is no longer needed to install it. Prebuilt binaries ship as per-platform packages (linux x64/arm64, macOS arm64, windows x64), and the package also has a typed Node API.

> The root package was renamed to `devsandboxes` in 0.3.2; install that version or later.

<details><summary>Commits</summary>

- 86df202 feat(npm): publish the cli to npm with per-platform binary packages and a typed node api, so npx devsandbox works without a rust toolchain

</details>

## 0.3.0

ssh-agent forwarding no longer needs a socket bind mount. A small static helper (`devsbd`) is embedded in the release binaries, installed into each container, and relays the host agent over `exec` stdio. This works on every runtime, including Apple `container`, and keeps working when the host agent socket changes.

### Added

- **Relayed ssh-agent forwarding** through the embedded `devsbd` helper, used by default. Relays detect wedged connections with keepalives and handshake timeouts, and restart the daemon if the container was restarted outside devsandbox. `git` over ssh works inside `postCreateCommand` and the other lifecycle commands.
- **`devsandbox vscode <name>`** attaches VS Code to an instance from the CLI, using the same launch path as the TUI's `o` key.

```bash
devsandbox vscode web
```

### Changed

- Release archives (and later the npm packages) embed the helper. `cargo install` builds don't, and fall back to the bind mount, which works only on Linux docker/podman.
- The TUI manages ssh-agent bridges on a worker thread, so the dashboard never blocks on them.

<details><summary>Commits</summary>

- 3401365 docs(readme): say release binaries embed the helper now that the release job builds it
- 0529cde fix(run): give lifecycle commands the forwarded ssh-agent so ssh git operations in setup commands work
- 45e7b2b chore(release): build and embed the devsbd helpers in release binaries, and stop excluding src/devsbd from the published crate
- afc6ddc docs(devsbd): plan the release helper job and agent access for lifecycle commands
- 8ec0e6b chore(devsbd): fail ci builds that lack the helper, keep helper-less builds incremental, and catch a stale embedded helper in tests
- 66be187 refactor(tui): own ssh-agent bridges on a worker thread so reconciling never blocks the dashboard, and back off on version mismatch
- a8071fc feat(devsbd): let the bridge start a missing daemon so containers restarted outside devsandbox keep their relay
- bc882e8 feat(devsbd): detect wedged relays with keepalive and a handshake timeout so agent requests can't hang forever
- 304bef7 feat(devsbd): leave room in the wire protocol for additive changes and bound what a container can make the host do
- 8012eb8 feat(devsbd): make the relay the default ssh-agent path so forwarding survives agent rotation and needs no socket mount
- 56e49d8 docs(devsbd): plan relay-first forwarding and the helper hardening found in review
- 7e5a8b0 feat(devsbd): relay the host ssh-agent through a helper daemon and stdio bridges so forwarding needs no socket mount
- 4d6f29d docs(agents): run workspace tests so the devsbd helper crate is checked too
- 9e89b74 feat(devsbd): add the shared host<->helper frame protocol so both sides decode one wire format
- 95ec425 feat(devsbd): install the helper on run/start so containers carry the build matching the cli
- 3681527 feat(devsbd): embed a static in-container helper so relays work on every runtime without network or file binds
- 2a522c5 docs(devsbd): plan an embedded in-container helper for host<->container relays
- 8840709 feat(vscode): add `devsandbox vscode` command to attach vs code from the cli, sharing the tui's launch path

</details>

## 0.2.4

Fixes the Windows build, which broke the 0.2.3 release: 0.2.3 has no published binaries, so this is the first release that ships its changes. Also clarifies the docs.

<details><summary>Commits</summary>

- b7449fc fix: clarify docs
- fffd63e fix: windows build

</details>

## 0.2.3

ssh-agent forwarding, worktrees that start from the remote's default branch, and process signalling from the dashboard.

> This tag never got a GitHub release (the Windows build failed); its changes shipped in 0.2.4.

### Added

- **ssh-agent forwarding.** The host `SSH_AUTH_SOCK` reaches every instance through a per-instance symlink mount, and `start` re-points it before the container starts. Forwarding therefore survives reboots and re-logins without a rebuild. `exec` sets `SSH_AUTH_SOCK` on every session. `rm` removes an instance's link and `gc` removes orphaned ones.
- **Worktrees start from the remote's default branch.** Instead of branching from whatever is checked out in the base repo, `run` fetches `origin` and branches from `origin/HEAD` (or `main`/`master`), without touching the base checkout. Use `run --base` or `worktree-base` to pick a different start point.

```toml
[sandbox.api]
folder = "../api"
worktree-base = "origin/develop"
```

- **TUI process rows can be signalled**: `t` sends SIGTERM and `K` sends SIGKILL to the selected pid. `x` closes an exited terminal where it is.

### Fixed

- Process listings use pids from the container's namespace, so signals reach the right process. Images without `ps` fall back to `docker top` and are shown as not signalable. The listing no longer includes its own `ps` process.

<details><summary>Commits</summary>

- 1c56b2f feat(run): start worktrees from a fetched origin default branch
- 5d339f3 fix(runtime): drop the self ps row from process listings
- f4a3737 feat(docker): fall back to docker top when the image lacks ps
- d6a6896 fix(docker): list container-namespace pids so signals hit the right process
- b72d8ff feat(tui): make process rows signal-only (SIGTERM/SIGKILL)
- 140977f feat(tui): close exited terminal with x in place
- 300e420 feat(gc): reap ssh-agent links; rm drops its own
- a9a8cd2 feat(start): re-point ssh-agent symlink before container start
- d28302d feat(exec): inject SSH_AUTH_SOCK on every exec
- 7255429 feat(run): forward host ssh-agent via per-instance symlink mount
- 9d77713 docs(ssh-agent): plan agent forwarding via re-pointable symlink mount

</details>

## 0.2.2

Renames no longer lose per-instance state, feature mounts are applied, and `rebuild --force` is added.

### Added

- **`rebuild --force`** recreates an instance even when no drift is detected. Use it to pick up devsandbox-side changes that the config hashes can't see.

```bash
devsandbox rebuild web --force
devsandbox rebuild --all --force
```

- **Feature-declared mounts are applied**, such as the host docker socket from docker-outside-of-docker. A sandbox mount on the same target overrides the feature's, and mounts the host can't satisfy are skipped with a warning.
- **Persistent instance ids.** Everything tied to an instance (`${instance}` mounts, worktree dir, shell history, isolated services) is keyed by an id that never changes, so renaming an instance is purely cosmetic.
- TUI prompt completion is driven by the same spec table as parsing, so every flag completes (including `rebuild --force`).

### Fixed

- Shell history survives in VS Code terminals: `HISTFILE` is pinned in the rc files, not only in the container env.
- `rename` requires the exact instance name and no longer hangs the TUI.
- Missing instances can be rebuilt from the TUI.

<details><summary>Commits</summary>

- ad8c40b fix: make sure the subagent has a model specified
- 947a512 docs(agents): note spec.rs in the tui module map
- 60676c6 feat(tui): spec-driven tab completion; rebuild --force completes
- 8fc0f0f refactor(tui): drive prompt parsing from the spec table
- b1aa1d9 feat(tui): declarative command spec table for the prompt
- 65a5377 docs(naming): drop plan- prefixes/suffixes, add tui prompt spec plan
- a12f65c feat: add a --force flag to rebuild to be able to account from devsandbox changes
- a9c8efa fix(tui): missing instances can now be rebuilt
- d85edd5 feat(state): persistent instance_id so rename never moves per-instance state
- 73d8025 feat(features): honor feature-declared mounts
- 4fa1783 fix(run): pin HISTFILE in the rc files, not just container env
- 8d62bbd fix(rename): require exact instance name and never write stdout in the tui
- f9db061 refactor(features): drop unused cache_dir method
- d0ff413 chore(cargo): exclude release.sh, .agents, .github from crate

</details>

## 0.2.1

Packaging only: the crate gains a description and the MIT license so it can be published on crates.io, and docs are excluded from it.

```bash
cargo install devsandbox
```

<details><summary>Commits</summary>

- 9c03b41 chore(cargo): add description, mit license, exclude docs from crate

</details>

## 0.2.0

Instance lifecycle commands (`start`, `stop`, `rebuild`, `rename`), an integrated terminal in the dashboard, JSON output for scripts, drift detection for dockerfiles and services, and Apple `container` support.

### Added

- **Separate lifecycle verbs.** `run` always creates a fresh instance. `start` / `stop` resume and suspend existing ones (`--all` for every instance), and `rebuild` (alias `recreate`) applies config changes in place while keeping the worktree, branch and per-instance state.

```bash
devsandbox run web --branch me/login-fix
devsandbox stop --all
devsandbox start web
devsandbox rebuild --all        # recreate every drifted instance
```

- **Integrated terminal in the TUI**: `t` opens a shell in the selected instance (zsh when available, otherwise bash) in its workspace folder. Terminals open as tabs; `[`/`]` cycle between them, `x` closes one, `ctrl-]`/`F12` leaves the terminal, and mouse scrollback works.
- **Machine-readable output.** `status --json` prints the full dashboard snapshot in a versioned envelope, and `ps`, `ls`, `stats`, `inspect` accept `--json` (contract in `docs/json-output.md`).

```bash
devsandbox status --json | jq '.data'
devsandbox ps --json
```

- **Service drift and rebuild.** Editing a dockerfile now counts as drift, for sandboxes and services alike. `service rebuild <name>` recreates a service and rewires running sandboxes without restarting them, and `service ls` lists services like `ls` lists sandboxes. Drifted services are marked in the TUI and can be rebuilt from there.

```bash
devsandbox service ls
devsandbox service rebuild postgres
```

- **`rename`** (CLI and `r` in the TUI), plus **`logs`**, **`inspect`**, **`stats`** subcommands that work the same on every runtime.
- **`shell-rc`**: host shell snippets sourced by the container's `~/.zshrc`/`~/.bashrc`.

```toml
[template.base]
shell-rc = ["rc/aliases.sh"]
```

- **Configurable worktree branch** via `worktree-branch` (supports `${instance}`) or `run --branch`.
- **Apple `container`** is supported as a runtime.
- TUI: coding-agent processes are highlighted in the process tree, and the detail panel shows an agent count.
- A `devsandbox-cli` skill that teaches coding agents to drive the CLI.

### Fixed

- Worktrees are created under the config dir instead of the base repo, and a branch name that already exists is rejected with a clear error.
- The base folder is reused correctly once its direct-mount instance is removed.

<details><summary>Commits</summary>

- 7c5beb0 ci: release on crates.io
- 5bd48a2 docs: document build-hash drift, service rebuild and service ls
- 781f636 feat(tui): surface service drift and rebuild services from the dashboard
- f1bf8b4 feat(services): service ls mirroring ls, sharing the tui's row builder
- e45c31e feat(services): rebuild a service without restarting its sandboxes
- 536dc85 feat(config): hash dockerfile contents so edits register as drift
- 736dfea feat(tui): probe zsh before bash for integrated terminal
- c08fc3d feat(rename): rename instances via r in the tui and a cli verb
- 08cae4e feat: add shell-rc
- 1e3de35 feat: set integrated terminal cwd for workspaces
- 7d6afef docs(json): document the json output contract
- dd7da88 feat(json): --json output for ps, ls, stats, inspect
- 1274c13 feat(status): machine-readable snapshot via status --json
- 5647ba3 refactor(snapshot): extract snapshot collection from tui
- 27dd37e docs(json): plan for machine-readable output
- 9e8386f chore: add support for apple container as fallback
- 33b65e1 docs: commit implemented plan
- e1d0c0e fix(tui): help bar shows t term and the real s verb
- 1781881 feat(tui): wire the terminal into the event loop and mouse
- c5587cf feat(tui): render the integrated terminal panel
- 904856e feat(tui): terminal tabs, focus model and key routing
- f86f54a feat(prompt): flag and branch-value completion for run
- b78ae47 feat(tui): pty session core for the integrated terminal
- 144130c docs(agents): record the conventional-commit format
- 0cb8abb feat(rebuild): add --all for batch drift fixes
- 6777b44 docs(skill): run never reuses an instance
- cdf458f Point drift docs at rebuild instead of rm + run
- 58e1d07 TUI: s on a stopped drifted instance rebuilds instead of bare-starting
- 7e7e3c0 Add rebuild (alias: recreate) to fix config drift in place
- 635ed25 Extract materialize() from run()
- cb9c929 fix: bug with base folder never reused
- 05194eb feat: separate instance lifecycle: run creates, start/stop restart
- f670ac6 Reject a worktree branch that already exists
- ac72c27 Make worktree branch name configurable
- d2678a8 Fix worktree created under base repo instead of config dir
- 44d8c48 cli: add logs, inspect, and stats subcommands
- a3fca98 feat: add skills/devsandbox-cli/SKILL.md skill to teach an agent how to use the tool
- 49facee tui: show on-demand agent count in the instance Detail panel
- 4cc7764 tui: highlight coding-agent processes blue in the process forest
- 04aaf9e docs: add README.md

</details>

## 0.1.1

First release. devsandbox runs devcontainer-style sandboxes directly on docker/podman from one `config.toml`, with no devcontainer CLI and no daemon.

### Highlights

- **devcontainer semantics in TOML.** Sandboxes use devcontainer property names (`image`, `build`, `mounts`, `containerEnv`, `remoteUser`, the lifecycle commands, VS Code extensions…). Unknown keys are a hard error. `extends` composes reusable templates: tables merge, arrays concatenate.
- **Native devcontainer features.** Features are fetched from their OCI registry and baked into a derived image, without the devcontainer CLI.
- **Concurrent instances of one repo.** A second `run` of the same folder gets its own git worktree and branch, so instances never share a checkout. Instance names are deterministic (`web`, `web-2`, …).
- **Services.** Sidecar containers are reachable by name. `scope = "isolated"` (the default) gives each instance its own copy; `scope = "global"` shares one per project. `gc` removes the ones no instance uses.
- **Shared caches and state.** `caches` shares package caches across instances, `persist-shell-history` keeps a per-instance history, and `${sharedVolumes}` / `${instance}` place other state per instance.
- **Config drift detection.** Each container records a hash of its merged config, so an edited config shows up as drift.
- **VS Code.** Attach to an instance as a named workspace, with extra roots from `folders` and `remoteUser` honored.
- **TUI dashboard** (run `devsandbox` with no arguments): a tree of sandboxes and instances with live CPU/memory, a process tree for each instance, a services view, a config explorer (original vs resolved, next to `docker inspect`), logs, and a `:` command prompt with history and completion.
- Static release builds for Linux (musl), macOS and Windows.

```toml
[services.postgres]
image = "postgres:16"

[template.base]
caches = ["pnpm", "cargo"]
persist-shell-history = true
[template.base.features]
"ghcr.io/devcontainers/features/node:1" = { version = "lts" }

[sandbox.web]
extends = "base"
folder = "../web"
image = "mcr.microsoft.com/devcontainers/base:ubuntu"
services = ["postgres"]
postCreateCommand = "pnpm install"
folders = { "/workspaces/shared-lib" = "../shared-lib" }
```

```bash
devsandbox run web             # first instance mounts ../web directly
devsandbox run web             # second instance: web-2, on its own worktree
devsandbox exec -it web-2 zsh
devsandbox rm web-2
```

<details><summary>Commits</summary>

- afb786a fix: remove unused file
- 362f295 gc: reap orphaned shell-history files
- b834316 rm: keep the managed shell-history file
- 415763a fix: now using devsandboxes, this file is no longer needed :tada:
- 020ff66 feat: ${sharedVolumes} and ${instance} mount variables
- 342e928 feat: open sandboxes as named VS Code workspaces via a folders property
- 377c54b fix(test): add connection timeout
- 00eaf4d fix: pin the rust version to the container version
- 4114307 feat: flag sandboxes with no image or build in the dashboard
- 9831fc8 feat: validate config entries in the dashboard and log run errors
- 392d028 docs: switch example sandboxes to native features
- a134871 run: bake devcontainer features into a derived image
- e326c19 features: fetch OCI feature artifacts natively
- cf318d4 config: type the devcontainer features property
- 1b2a410 docs: plan for native devcontainer features support
- 9260620 fix: failing test on local
- a4861d7 Add CI workflow to catch build/test breakage before release tags
- f90232f chore: fix CI release logic
- 155e30b agents: orchestrator and implementer skills
- fab60a2 docs: add the first plans
- f70d803 feat: add basic support for macOS
- ccc958f tui: split accent purple from selection green
- a225787 tui: config modal splits into config | docker inspect panes
- 3569eff tui: process forest under expanded instances
- f259245 stop: first-class command distinct from rm
- f50ec78 vscode attach: honor remoteUser in the name config
- 6b88849 tui: one-key run prefill and VS Code attach from the tree
- 303cae6 tui: instances tab as a sandbox tree
- 7bc7e89 tui: drop plan-step references from UI text and comments
- 6e6b1d3 tui: help overlay, instance logs, header totals, staleness marker
- 5da536e tui: command prompt with history and tab completion
- 012860a tui: config explorer modal with original vs resolved toggle
- 59d3139 tui: services view joined from config, state and docker
- 5b4f6fa tui: live instances view with background docker collection
- 7b48117 tui: dashboard scaffold behind bare invocation
- 6201081 ls: readable table with colors and an empty-state message
- 19acc23 Recursive/list extends, remote+container user, init, workspace vars, caches
- 7469421 Add managed per-instance shell history
- f80ee7c Implement mounts, additive merge, and global/isolated services
- 0a0df68 services: shared per-project network + service containers, add gc
- e415d9a run: deterministic naming, reuse, worktrees, config-hash drift; add rm
- 8f56f9c Add release CI: static builds for linux (musl), macos, windows on version bump
- a1ee668 Type sandbox config to the devcontainer schema and expand runtime support
- 722fe21 Add runtime basics: run, ps, exec, host-side state store
- cad57f9 Add config core: TOML model, extends deep merge, ls command
- 1fd3f93 initial commit

</details>
