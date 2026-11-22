# Status

The only place for project state: what's in progress, planned or parked,
and what doesn't work today. Living docs link here instead of keeping
their own limitations. Finishing a plan, parking an idea or fixing a
limitation means editing this file in the same commit.

- A **limitation** is a fact about today's code (verified, with a symbol
  to check). Its fix idea, if any, rides on the same line.
- A **parked** item is a feature nobody is building. **Planned** has a plan
  in `plans/active/`; a parked one may have an idea in `plans/explore/`.

## In progress

- **credentials**: credential-broker steps 1-3 (the broker, `login` by
  paste, device flow and browser OAuth with renewal behind the phantom,
  `access approve|ls|revoke`, the member CA and intercept mode),
  github-and-sentry-access step 1 (sentry-cli through `features/sentry`)
  and git-gateway step 1 (git through the broker with the repo fence, LFS
  and the relay's key filtering) are done, and the fence resolves to repo
  ids and GraphQL mutations are mapped to scopes and placed by their
  input ids; now github-and-sentry-access step 2: `features/gh` and its
  scope table, with the test walking GitHub's GraphQL schema.
  → [plan](plans/active/github-and-sentry-access.md)

## Planned

Order: the orchestrator interface (to plan), helper-rpc
steps 4-5 reporting to it, then the Inbox's removal with fleet step 13
rewritten for the orchestrator
([helper-rpc](plans/active/helper-rpc.md#order-across-plans)). The Inbox
is frozen: no new work on it.

- **machines**: the authority no longer a single point of failure: a
  recovery key made at genesis, a scoped, expiring delegation to the
  control plane (`members`, `configs`); then machine → node.
  → [plan](plans/active/control-plane.md)
- **machines**: fleet-wide `global` services: one container per signed
  root's service, reached from any member over the links.
  → [plan](plans/active/mesh-services.md)
- **machines**: peer links over mutual TLS instead of ssh (ssh kept for
  `machine add`'s install and first contact); routed streams on inner
  TLS. → [plan](plans/active/mtls-links.md)
- **runtimes**: `isolation = "gvisor"` on a sandbox or template (runsc on
  docker/podman, refused where the host can't), Kata after.
  → [plan](plans/active/isolation.md)
- **credentials**: no secret in any sandbox: a broker in `serve` swaps
  per-instance phantoms for credentials kept at their home member, under
  per-sandbox policy; after gh: permission requests, `CRED` over the
  fleet, signed policy, delegation, git push rules and clones.
  → [plan](plans/active/credential-broker.md); per tool:
  [gh and sentry-cli](plans/active/github-and-sentry-access.md),
  [git over smart HTTP, clones replacing worktrees](plans/active/git-gateway.md)

- **helper**: daemon-owned runs (registry, run-state events, a `queued`
  state) with a restart policy, worktree stand-ins, container → host
  `RPC` (helper-rpc steps 4-7; the `RPC` channel and runs and sessions
  over it shipped; run events go to the orchestrator interface, not the
  Inbox), in the order of
  [helper-rpc](plans/active/helper-rpc.md#order-across-plans).
  → [plan](plans/active/helper-rpc.md)
- **machines**: VS Code on remote rows (spike), nameless commands across
  every machine (remote Inbox threads dropped with the Inbox).
  → [plan](plans/active/machines-next.md)
- **machines**: a fleet: every machine a member (one `machine add`,
  links routed over a graph, `worker`/`full` trust per member, a
  swappable control plane), soft-cap placement, devsandbox-owned code transfer
  between hosts, dispatcher children on any machine, checkpoints and
  re-placement: from fleet step 13 on. Steps 1-12 shipped: one kind of
  machine, one peer link per pair (`fleet-link --peer` on the dialed
  end, where the dialer is an inbound machine), every use over the
  link's channels and trust, ssh only for the link and `machine add`'s
  probe; a laptop joins a control plane with `machine add <control-plane>`; a control plane (`control_plane =
  "self"` in `member.toml`, step 9a) places over every connected
  machine, and a link between two hosts naming different control planes is
  refused. `machine add` joins (step 9b): the end in no fleet takes the
  other's control plane, two ends in none make a fleet of two, two control planes are
  refused. A member sees the fleet through its control plane (step 9c: the
  machines the control plane links to show `via` it). `machine rm <m>` takes `m`
  out of the fleet on the control plane (a member's is sent there), `machine rm
  <this host>` leaves, `machine unlink <m>` only drops this host's link
  (step 9d). A member's `run --place` is placed by the control plane (step 9e,
  `fleet.place`); a typed `run` without it stays on its member
  (decision 38). Member identity (step 10a): every member has a key,
  the fleet's membership is a log only its authority (the host that
  made it) signs, every link proves both ends' keys against it, and a
  change typed on another member (`machine rm`, `machine key rotate`,
  an introduction) waits for `machine approve` on the authority. Routing
  (step 10b): every member signs a report of its links and the members
  flood them into one link graph; a member with no link to another
  reaches it over the graph's paths, each routed stream proving its
  origin to its destination, which serves it under its trust for that
  origin; members reached only that way are routed machines, with
  actions, terminals, forwards, placement and fleet calls; `machine
  trust worker` warns when a machine the control plane's side controls dials this
  host. End-to-end sessions (step 10c): each routed stream is encrypted
  between its two daemons, so a relay carries ciphertext only; agent
  access is a per-member grant (`machine set <m> agent on`), carried to
  a granted member over any path. Control-plane swap (step 11): `machine control-plane <m>`
  moves the role by a `control_plane` entry the authority signs (proposed and
  approved there when typed elsewhere), links unchanged. Config
  versions: a `run`/`rebuild`/`start` typed on the authority signs its
  config root's hash as the root's next `config` version when it
  changed (a `machine add` there signs `~/.devsandboxes` and
  `member.toml`'s `roots` the same way, [ADR
  0050](decisions/0050-machine-add-on-the-authority-signs-its-configs.md)),
  and every other member installs that version's files from a bundle
  it stored, else pulls them from the peer that gave it the log, as its own copy under
  `mesh/tree/<name>/`, kept while the authority is offline; that copy
  is one of the member's roots, and a `run`/`rebuild`/`start`/`services
  rebuild` there runs only while it is the signed head (a copy changed in
  place is reinstalled from its stored bundle), from the daemon, a dispatcher's child ops, autostart or the
  CLI alike ([ADR 0049](decisions/0049-the-authority-signs-a-config-every-member-runs.md)). Transfer v1
  (step 12): a placed run's folders move from their own checkouts with
  a WIP snapshot, no remote needed, into per-repo caches on the worker
  (repeat sends carry only the delta) and on the control plane, and `devsandbox
  fetch <machine>/<instance>` (also before `rm` / `done` there) brings
  the work back as hidden refs. Next: step 13, re-scoped to an
  external orchestrator's children and waiting on its interface (see
  the order above).
  → [plan](plans/active/fleet.md); [fleet](architecture/daemon/fleet.md#routed-streams), [ADR 0044](decisions/0044-one-fleet-authority-signs-membership.md), [ADR 0045](decisions/0045-a-routed-stream-proves-its-origin-per-stream.md), [ADR 0046](decisions/0046-routed-streams-are-end-to-end-sessions.md)
- **daemon**: Windows as a remote-only client of a machine.
  → [plan](plans/active/windows-support.md)
- **helper**: drop the host-side `lsof` fallback for a forward's listening
  process (`port::listening_procs`, `ForwardSpec.probe`) once the helper
  can answer for a service reached via an instance and pre-`PORT_INFO`
  helpers are gone; the helper's `PortQuery` is then the only lookup.
  → [forwards](architecture/daemon/forwards.md#routes)
- **helper**: run the ssh-agent relay once on Apple `container` on a Mac
  (built, never tested there).

## Parked

- **runtimes**: GPU CI: a job on a self-hosted GPU runner running a
  sandbox and a service with `hostRequirements.gpu` on docker, rootless
  docker and podman, behind a test gate of its own (the `CI` rule would
  fail every other job). → [ADR 0055](decisions/0055-gpus-follow-the-devcontainer-spec-and-a-required-one-refuses.md)
- **runtimes**: GPU device selection (some GPUs, not every one), other
  vendors, and GPUs under gVisor (`runsc --nvproxy`,
  [isolation](plans/active/isolation.md)).
- **machines**: `machine add` asking what this host lends too when it
  joins the target's fleet or starts one with it (today it only prints a
  `devsandbox machine allow` hint); `machine add`'s `--allow` flags name
  the target's allowances. → [ADR 0056](decisions/0056-a-gpu-is-placed-where-a-worker-lends-one.md);
  `commands::machine::own_allow_hint`
- **machines**: compacting superseded `Op::Build` entries out of the
  membership log; keeping the previous binary and rolling back when a
  pinned build can't link within some minutes; pinning a build made on
  another platform (`devsandbox machine build sign <path>`), for a
  mixed-platform dev fleet.
  → [ADR 0053](decisions/0053-the-authority-pins-the-fleets-build.md)
- **helper**: opening files (`xdg-open report.pdf`): to the VS Code window
  attached to the instance, or the host's opener for a path in a
  bind-mounted workspace. Refused today (exit 3).
  → [in-container](architecture/helper/in-container.md); `devsbd` `open.rs`
- **dashboard**: an OSC 8 link clicked in the embedded terminal opening
  like a container's URL (the row's profile, `profiles.toml`); it never
  reaches the container. → [terminal](architecture/dashboard/terminal.md)
- **helper**: schemes beyond `http(s)` (`mailto:`, `vscode:`) behind a
  confirmation routed to the user (the broker's "ask" route).
  → [bridges](architecture/helper/bridges.md#opening-urls-kind-open); `open::decide`
- **daemon**: opening on every member attached right now instead of the
  last attach only. → [fleet](architecture/daemon/fleet.md#last-attach); `serve::opens::route`
- **helper**: more for `exec` sessions: a detach key (kubectl's
  `--detach-keys`, `ctrl-p,ctrl-q`, as the model), a read-only watcher
  attach (output only, no takeover), a bounded replay of what a session
  printed while detached, an opt-in idle timeout, and printing the
  `exec --attach` line when a client is displaced.
  → [ADR 0054](decisions/0054-interactive-execs-are-helper-sessions-ended-only-on-purpose.md); `devsbd` `sessions.rs`

- **instances**: repeat-instance worktrees in a container volume instead
  of a host bind (ownership stops mattering, faster I/O on VM-backed
  runtimes; the host reaches them through `host_git` and the helper).
- **credentials** (agents): zidane's and other agents' credentials through
  the broker. → [explore](plans/explore/agent-credentials.md)
- **instances**: auto-mount a shared `.agents/skills` folder read-only into
  every instance. → [plan](plans/explore/agents-skills.md)
- **daemon**: `rename`, `profile`, `done`/`undone`, a local `gc`, `service ls` as daemon verbs
  (they run in the CLI process; the API has `instances.rename`/`instances.setProfile`/`instances.done`,
  and `gc` is a verb for `--machine m gc`).
- **npm**: typed verb helpers (`api.run(…)`); `ApiInstances` has only `list`.
- **config**: `runArgs` pass-through, `envFile`, secrets; `devsandbox init`
  scaffolding.
- **runtimes**: an E2B-compatible API served by devsandbox, and
  E2B-compatible clusters (sandbawks) as a backend or fleet member.
  → [explore](plans/explore/e2b-api.md)
- **machines**: k3s-style join tokens as an alternative to ssh first
  contact. → [explore](plans/explore/join-tokens.md)
- **machines**: how resilient fleet state should be (single writer,
  opt-in Raft among always-on members, gossip for observed state).
  → [explore](plans/explore/mesh-state.md)
- **instances**: ideas from sandbawks: process checkpoints, idle pause,
  warm pools, clone, egress policy, not-ready signals for agents.
  → [explore](plans/explore/sandbawks-ideas.md)
- **instances**: `.worktreeinclude.local`, `**`/`[...]` in `worktree-link`,
  read-only store mounts, an unlink/restore command, shared files served by
  `devsbd` from tmpfs.
- **forwards**: auto-detect listening ports (`/proc/net/tcp`); UDP;
  `portsAttributes`/`otherPortsAttributes`/`appPort`.
- **automations**: `devsbd boot` exec'ing into the daemon (it spawns it as a
  child today); a detached lifecycle form for long-running
  `postStartCommand` loops; `npx devsandboxes init dispatcher` scaffold;
  declarative `[automation.*]`; webhook sources; a "was held" branch mark.
- **helper**: `devsandbox agent <instance>` foreground relay; `O_NOFOLLOW`
  hardening of the outbox/runs shared dirs.
- **dashboard**: double-click/triple-click and keyboard selection; clipboard
  fallbacks without OSC 52; copying a selection's markdown source; `--all`
  for `:stop`/`:start`/`:rebuild`; `:logs`/`:inspect`; progress for
  `services.rebuild`.
- **json**: raw-number cpu/mem (schema v2); plain `status` without `--json`.
- **machines**: OS keychain backends for the member key behind `KeyStore`
  (fleet plan decision 52). → [plan](plans/active/fleet.md)
- **machines**: an agent grant counted as exposure (a granted member can
  ssh as the granter wherever its agent reaches; reports carry grants
  since step 10c, `fleet::graph::exposure` doesn't read them).
  → [fleet](architecture/daemon/fleet.md#exposure)
- **machines**: code transfer extras (fleet plan decisions 72-74, 76): a
  first send fetched from a reachable `origin` instead of the link; the
  ignored files a worktree wants (`.worktreeinclude`, `worktree-include`)
  carried with the snapshot; a config root that's a repo moved as one,
  so a `folder` inside it can be placed; fetch-back of `folders`
  entries; fetch-back from the dashboard. → [plan](plans/active/fleet.md)
- **machines**: the dispatcher's code (`dispatcher/`,
  `dispatcher.config.mts`) is reached only through `postStartCommand`,
  so a signed config neither hashes nor ships it: an explicit include
  key, or left to box setup?
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `fleet::mesh::collect`
- **machines**: `start` of an instance whose config drifted only warns,
  on a signed config's copy too: refuse it there?
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `run::warn_on_drift` (from `start::start_instance`)
- **machines**: cutting config versions' bloat: fleet.md's long *Config
  versions* down to rules; `mesh.rs`'s two temp-then-rename writers
  (`mesh::store`, `write_in`) and two "TOML file or empty" loaders
  (`load_roots`, `load_installed`); `Installed.removed`, read by tests
  only. → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `fleet::mesh`
- **machines**: signing on a verb aimed at another machine (the local
  root bound to the remote root's name) stays for now; drop it?
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `daemon::sign_remote_config`
- **machines**: a self-defined canonical encoding for the config bundle's
  hash: it serializes the table with the `toml` crate, so a build with
  another `toml` version may stop matching a signed head.
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `mesh::hash_parts`
- **machines**: a signed root's name is its path's last component as
  typed, not canonicalized, so two symlinks to one root are two names
  (each bound to the same path).
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `changes::sign_config`

## Known limitations

- A command on a machine with several roots and no default (no signed
  copy, no `~/.devsandboxes`) is refused: the CLI has no way to name a
  root on a machine, and the dashboard's remote rows still take the
  first listed root (`tui::remote::machine_root`; the snapshot's machine
  entries don't carry the default).
  → [machines](architecture/daemon/machines.md); `machines::default_root`, `machines::no_default_root`

### instances

- A terminal's reply to a query a program sends just before it exits (or
  one the terminal answers slowly) can arrive after `exec` has restored
  the host tty's echo, and is echoed as `^[[…` text; nothing flushes
  pending input when `exec` returns (`tcflush(0, TCIFLUSH)`), nor between
  a reconnecting remote exec's dials. → `commands::exec::exec_status`,
  `pty_client::dial`
- The host-overlap check on a new service network reads routes only on
  Linux (macOS: nameservers only), is skipped on Apple `container`, and
  never re-checks a network that already exists, e.g. one created before
  the check. → [services](architecture/instances/services.md#networks);
  `services::ensure_network`, `subnet::host_nets`
- `gc` counts only running (or in-flight) instances as live, so it reaps a
  stopped instance's isolated services; the next `start` recreates them
  empty. → [services](architecture/instances/services.md); `services::gc`
- A drifted service is only warned about on `run`/`start`, and the warning
  suggests `gc` + re-run instead of `devsandbox service rebuild`.
  → [drift](architecture/instances/rebuild-and-drift.md); `services::ensure_service`
- Drift misses the build context and OCI feature contents; those need
  `rebuild --force`. → [drift](architecture/instances/rebuild-and-drift.md);
  `config::build_hash`, `sandbox_build_hash`
- Feature cache entries never refresh: a mutable tag (`:1`) stays at its
  first pull. → [images](architecture/instances/images-and-features.md); `features::fetch_into`
- Features: `installsAfter` only (no `dependsOn`,
  `overrideFeatureInstallOrder`); metadata `capAdd`/`init` ignored;
  `_REMOTE_USER` falls back to `root` without reading the image user; no
  BuildKit build contexts. → [images](architecture/instances/images-and-features.md);
  `features::install_order`
- Feature entrypoints race lifecycle commands (nothing waits for e.g.
  dockerd after `run_container`). → [images](architecture/instances/images-and-features.md); `materialize`
- The `cargo` cache replaces the image's `CARGO_HOME`: its `config.toml`
  is not read and `cargo install` binaries land off `PATH` (in the shared
  cache's `bin/`). → [mounts](architecture/instances/mounts-and-state.md);
  `config::known_cache`
- `worktree-include`/`worktree-link` apply to the primary repo only, not to
  `folders` worktrees; a `folders` entry that is a repo subdirectory is
  always bind-mounted; extra `folders` worktrees don't show in `--json` or
  the dashboard. → [worktrees](architecture/instances/worktrees.md);
  `materialize`, `folders::needs_worktree`, `RunRecord`
- Worktrees devsandbox didn't create stay unlocked (only
  `gc.worktreePruneExpire=never` covers them), and the lock file is
  writable from the container. → [worktrees](architecture/instances/worktrees.md);
  `lock_worktree`, `gc_keeps_worktrees_env`
- Host git checks a repo's git dirs and config, then runs: a container
  running at the same time can swap a file (a symlink, `gitdir:`,
  `commondir`, config) in between. Fixed by clones replacing worktrees
  ([git-gateway](plans/active/git-gateway.md)).
  → [mounts](architecture/instances/mounts-and-state.md#host-git); `run::git::check_repo`
- Repeat instances and `folders` worktrees bind-mount the base repo's
  `.git` read-write, so one sandbox can rewrite another's branches and
  shared config, or plant code (`core.fsmonitor`, a `build.rs`) that runs
  in another sandbox on the same checkout. Fixed by clones replacing
  worktrees ([git-gateway](plans/active/git-gateway.md)).
  → [worktrees](architecture/instances/worktrees.md); `run::worktree::git_companion_mount`
- The origin URL is trusted on first use: a repo whose `remote.origin.url`
  a sandbox rewrote before its first host fetch gets the rewritten URL
  pinned.
  → [mounts](architecture/instances/mounts-and-state.md#trusted-origin);
  `run::origin::fetch_trusted_origin`
- Host checkouts run no filter driver, so LFS files in a worktree (or a
  fleet checkout) the host made are pointers until `git lfs pull` in the
  sandbox. A filter defined only under a global `includeIf` whose
  condition holds for some repo isn't found (the names are read outside
  any repo) and still runs there.
  → [mounts](architecture/instances/mounts-and-state.md#host-git);
  `run::git::neutral_filter_args`

### config

- Many devcontainer properties parse but are ignored with a warning at
  `run` (`runArgs`, `appPort`, `portsAttributes`, `workspaceMount`,
  `capAdd`, `securityOpt`, `overrideCommand`, `shutdownAction`,
  `hostRequirements` but `gpu`, `waitFor`, `secrets`, `userEnvProbe`,
  `overrideFeatureInstallOrder`, `name`,
  `customizations.vscode.settings`). → [config](architecture/config.md);
  `SandboxProperties::ignored`
- `extends` can't remove an inherited array entry; a diamond duplicates
  entries. → [config](architecture/config.md); `config::deep_merge`
- A sandbox's `access`: `branches` parses but nothing reads it
  (git-gateway step 2, push rules), and `run` doesn't warn
  about `scopes` or `ask` a claim doesn't have.
  → [plan](plans/active/credential-broker.md); `config::Access`

### credentials

- A claim's device and browser logins send no `scope` (no claim field;
  GitHub Apps take none), and `github-app-manifest` logins are refused
  (github-and-sentry-access step 3). `credentials.toml`'s `members`,
  `approvers` and `delegate` parse and `access ls` lists them, but
  nothing acts on them yet; of `as`'s kinds only `user` works, `app`
  (installation tokens) refusing the run until step 3.
  → [login](architecture/credentials/login.md); `commands::login::choose`
- A pasted token's expiry is known only once a brokered call's response
  reports it (`login` calls no `check` route), so `access ls` shows none
  before; the expiry notice's once-a-day limit lives in the daemon's
  memory (a restart may repeat it); a refreshed OAuth token isn't checked
  against the claim's `tokens.accept` (only `login`'s).
  → [credentials CLI](reference/credentials-cli.md#credentialstoml);
  `credentials::tokens`
- git under a git-host claim: no ssh host alias (`git@github-work:`)
  is rewritten to `https://` (the folder's record keeps the URL, not what
  the alias resolved to), so such a remote's push fails under key
  filtering (the boot check doesn't see it either); the system
  gitconfig copy is written only at create, under `/etc` (gits reading
  another sysconfdir, nix's or conda's, see only the env); when
  `remoteEnv` or `--env` sets git's `GIT_CONFIG_*`, every exec drops
  `gc.worktreePruneExpire`, and an `ensure` replacing the saved `--env`
  leaves `Instance::git_env` as `run` computed it. LFS is tested against
  the broker only, not end to end from a container.
  → [git](architecture/credentials/git.md); `credentials::git::entries`
- The CA variables point at
  the Debian bundle path when the image probe can't run (Apple
  `container`), where the install writes the Mozilla set plus the CA if
  that file is missing. A CA reissued by the broker itself (a host it
  covers on demand, `member-ca.toml` removed since the run) reaches a
  running container only at its next `start`. Claims on an
  instance placed by another member are skipped with a warning until
  credential-broker step 5 (`CRED`).
  → [images and features](architecture/instances/images-and-features.md#credential-claims),
  [ca](architecture/credentials/ca.md#trust-in-containers);
  `credentials::instance::plan`, `commands::run::claims::wire`
- The broker refuses every ask: a scope in `access.<kind>.ask` and a
  route in no scope (`other`) get a 403 saying asks need permission
  requests (credential-broker step 4); `presence` routes pass. The GitHub
  repo fence is enforced on a fenced claim's `{repo}` and
  `/repositories/{id}` routes
  ([fence](architecture/credentials/fence.md#enforcement)) and on
  GraphQL mutations' `fence` inputs
  ([broker](architecture/credentials/broker.md#graphql)); REST body-named
  inputs are refused (github-and-sentry-access step 2). A GraphQL input
  naming a renamed repo by its old `owner/name` is refused, not
  redirected (the body goes as sent). Nothing yet checks that a claim's
  `fence` names every repo- or object-naming input of its mutations (the
  schema-walk test comes with the real `features/gh` claim). A fence entry's pin ([ids](architecture/credentials/fence.md#ids))
  is never dropped, not even by `access forget`: a repo deleted and
  re-created under its name stays outside its entry. A pinned repo
  renamed again is called by its previous name until the cached answer
  expires (1 h). A placed or remote instance's
  fence doesn't travel with it. Pushes in the fence get no branch or tag
  rules and a refused one is a plain `403` at `info/refs`, not a
  `report-status` (git-gateway step 2). LFS transfer handles live in the
  daemon's memory: a daemon restart between a batch and its transfers
  answers them 404, and git-lfs must be run again; an injected batch
  response that isn't JSON or is over the rewrite bound passes with its
  actions' headers (only the injected token masked); transfer adapters
  other than `basic` (tus, ssh) aren't rewritten. Only
  the github test fixture claims a fence: the real `features/gh` claim is
  github-and-sentry-access step 2. No token and registration
  endpoints a claim could mark (no claim field). A credential marked
  broken notifies no approver (credential-broker step 4). Policy reads no
  request body beyond an LFS batch's `operation` and a GraphQL request's
  document, and token-shaped response headers aren't stripped: only the injected value is masked. One
  upstream connection per call, no pooling. `access.log` shows nowhere
  (no dashboard view, nor in `access ls`).
  → [broker](architecture/credentials/broker.md); `broker::policy::decide`,
  `broker::serve::Service`
- The agent relay's key filter under a git-host claim signs a named
  key's login with no confirmation (confirm-on-sign is a permission
  request, credential-broker step 4). A destination key's forwarding hops
  aren't checked, only the login's own bind; an agent certificate isn't
  matched by its underlying key; ecdsa nistp521, `ssh-rsa` (SHA-1),
  certificate and security-key host keys can't bind. The dashboard's `o`
  refuses an instance with a claim and has no override (only
  `devsandbox vscode --allow-credential-leak`).
  → [ssh-agent](architecture/helper/ssh-agent.md#key-filtering);
  `devsbd::agent_filter::Filter`, `commands::vscode::leak_refusal`
- `login` and `access approve|ls|revoke` are local only: no `access.*` /
  `credentials.*` API methods, so no approver answers from another host
  (credential-broker step 4). → [plan](plans/active/credential-broker.md)
- `access revoke` of an OAuth credential leaves its token valid at the
  provider until it expires (no claim field names a revocation
  endpoint), and `access ls` shows no approval time (`credentials.toml`
  records none). → [credentials CLI](reference/credentials-cli.md#access-revoke);
  `commands::access::revoke_at`
- `run`'s host-class warning fires only on a host that brokers a
  credential, not yet on one that is a credential's approver (step 4), and
  `access ls` marks neither it nor a shared writable bind (left out of
  step 2, deferred again by git-gateway step 1); the
  `runArgs` refusals wait for `runArgs` to reach the runtime. In a build
  without the helper, an ssh-agent fallback mount whose agent lives in
  `$XDG_RUNTIME_DIR` reads as a session socket.
  → [mounts](architecture/instances/mounts-and-state.md#what-no-bind-may-reach);
  `commands::run::binds`

### runtimes

- Apple `container`: no `--cache-from` (dropped with a warning), no
  `--privileged`, no restart policy (`autostart = "runtime"` acts as
  `true`), no file binds (feature file mounts skipped, no ssh-agent mount
  fallback), CPU shows `-`. → [runtimes](architecture/runtimes.md);
  `AppleContainer::supports_*`
- GPUs: NVIDIA only, and every GPU (`--gpus all`, `--device
  nvidia.com/gpu=all`): no device selection, and `hostRequirements.gpu`'s
  `cores` / `memory` aren't enforced (warned). `GpuSupport::run_args`
- GPUs on rootless docker: the NVIDIA toolkit needs `no-cgroups = true` in
  the host's nvidia-container-runtime config, which `gpu_support` can't
  see, so without it the create fails at the runtime.
  `runtime::gpu::docker`
- GPUs on podman: CDI specs are looked for in `/etc/cdi` and
  `/var/run/cdi` only; containers.conf's `cdi_spec_dirs` isn't read.
  `runtime::gpu::PODMAN_CDI_DIRS`
- GPUs are checked end to end only by hand, on docker through CDI (a
  required GPU given, refused and an optional one warned without the
  spec); docker's `--gpus` path, podman and placement on a GPU worker
  have never run on a GPU host (no GPU CI). `run::gpu::gpu_decision`
- GPU count in the capacity report: unknown (`gpu.count` `null`) on any
  engine that isn't native Linux (a VM's or a remote engine's devices
  aren't seen here). `capacity::HostFacts::read`
- Podman: `autostart = "runtime"` restarts at boot only with
  `podman-restart.service`, which devsandbox never installs.
  → [runtimes](architecture/runtimes.md); `autostart::restart_decision`
- Identity: only rootful docker (CI's `test` job), rootless podman (CI's
  `test-podman` job) and VM-backed runtimes are exercised; rootless docker,
  rootful podman, SELinux relabeling and `--socket-gid` have never run.
  → [identity](architecture/instances/identity.md)
- Identity: a `workspaceFolder` or `folders` target under `/root` isn't
  rewritten by the migration (`config_identity::Kind::RootPath`, no edit);
  under `identity = "host"` it stays unreachable for the adopted account
  until fixed by hand. → [identity](architecture/instances/identity.md)
- Identity modals (dashboard): the listing doesn't scroll, so one longer
  than the screen is cut (`ui.rs` `draw_confirm_modal`); the offer reads a
  machine's root only when its rows carry un-acked notes, so a machine's
  unused templates show up only in that machine's own dashboard
  (`identity::offer_routes`).
- Identity prompt (CLI): with no daemon, `run` without a sandbox name
  doesn't prompt (the in-process pick comes after it; `run`'s warning
  still shows), and off unix nothing prompts (no verb client).
  → [verbs](architecture/daemon/verbs.md)
- Identity under SELinux on docker: `--mount` binds aren't relabeled (only
  podman has `relabel=shared`), just warned about. `identity::create_args`
- Identity probe: a failed probe isn't cached, so a remote `DOCKER_HOST`
  (always ambiguous, the canary can't see its files) re-runs the canary and
  falls back to `identity = "image"` on every `run`. `run::identity::probe`
- Identity: rootless podman assumes the adopted account is 1000:1000
  (`--userns keep-id:uid=1000,gid=1000`) when the image's `/etc/passwd`
  can't be read (no `sh` in the image) or lacks the account; another uid
  then only gets a warning from `devsbd identity apply --expect`.
  `identity::ASSUMED`
- Apple `container` never reads the image's `/etc/passwd` (no throwaway
  container), so `${containerEnv:HOME}` falls back to `/home/<user>` (or
  `/root`) whatever home the image gives the account.
  `AppleContainer::image_passwd`
- `${containerEnv:…}` in `workspaceFolder` and `folders` stays verbatim:
  they resolve before the image is built. `run::materialize`
- Identity on rootless docker: VS Code attaches as root without the plan's
  `HOME` (`Instance::home` reaches execs only). `vscode::launch`
- Tool env: `PLAYWRIGHT_MCP_SANDBOX=false` and `CHROMIUM_FLAGS=--no-sandbox`
  are unverified knobs (no Playwright MCP or Debian `chromium` wrapper
  was tested). `run::tool_env::tool_env`
- Tool env: the Playwright library keeps its sandbox off (it adds
  `--no-sandbox`) and skips the root plan's browser wrapper unless a script
  launches with `chromiumSandbox: true` and `executablePath:
  $DEVSANDBOX_CHROME`; no env var does it. `run::tool_env::tool_env`
- Browser wrapper: never run under rootless podman (one more userns level);
  its test runs a fake browser under docker. `devsbd/src/browser.rs` `run`
- `rebuild --reprobe` without config drift does nothing unless `--force`
  is given too. `rebuild::rebuild`

### daemon

- Unix-only: the daemon and everything that needs it (dashboard, `api`,
  `machine`, `port`, `-d`, `--machine`, the ssh-agent relay). Off unix,
  verbs run in the CLI process. → [platform support](architecture/daemon/platform-support.md)
- Every live forward is an idle holder, so a running instance with
  `forwardPorts` or a saved ad-hoc forward keeps the daemon up forever.
  → [role](architecture/daemon/role.md); `Shared::holders`
- Each forward spawns its own bridge `exec` instead of sharing the
  instance's. → [forwards](architecture/daemon/forwards.md)
- Foreground `devsandbox port` forwards are invisible to the daemon, the
  Ports tab and the API, and vice versa. TCP only.
  → [forwards](architecture/daemon/forwards.md); `commands::port::port`
- The service injection route needs `/bin/sh`, root `exec` and a writable
  `/run` in the service image. → [forwards](architecture/daemon/forwards.md); `Planned::Inject`
- An adopted verb is tracked by pid only: a reused pid holds the daemon
  until that process exits. → [verbs](architecture/daemon/verbs.md); `InFlight::adopt`
- A CLI that loses its daemon mid-verb follows the build log from the
  start, re-showing streamed lines. → [verbs](architecture/daemon/verbs.md); `verbs_client`
- The startup autostart pass runs in the daemon process (no build log) and
  only walks roots with a recorded instance, so a root with `autostart`
  sandboxes but no instance never autostarts, dashboard open or not.
  → [host poll](architecture/daemon/host-poll.md); `Host::start`, `config_roots`
- The daemon doesn't enforce `hello`. → [api](reference/api.md); `serve::daemon::handle`
- With the event stream live and nothing watched, a host agent appearing
  without a `bridges.ensure` is noticed only within `QUIET_POLL` (60 s).
  → [host poll](architecture/daemon/host-poll.md#the-poll); `host::tick`
- Apple `container` has no event stream, so its daemon lists containers
  every `POLL` (5 s) even when idle.
  → [host poll](architecture/daemon/host-poll.md#the-poll); `Backend::events_argv`

### machines

- A program run by a reconnecting `exec m/x` (or a remote dashboard tab)
  that exits 255 reports 254: 255 is kept for ssh's own failure.
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `commands::exec::reported_code`
- A local daemon exit (handoff, idle, shutdown) cancels a held non-`detach`
  remote verb (every CLI verb): there is no spool to adopt it by.
  → [machines](architecture/daemon/machines.md); `machines::forward::verb`
- Daemon-side ssh (each machine's link, `machine add`'s probe) is
  `BatchMode=yes`: passphrase-prompting keys fail; links over mTLS are
  [planned](plans/active/mtls-links.md).
  → [machines](architecture/daemon/machines.md#ssh); `fleet::transport::Transport::ssh`, `ssh::probe_argv`
- A CLI re-run reports its terminal's agent once (`machines.get
  {agent}`): a daemon that hands over while the ssh session runs leaves
  the machine's `AGENT` streams to the successor's own agent (or the next
  client's report). → [machines](architecture/daemon/machines.md#agent); `machines::cli::info_params`
- Every agent check on a machine a granting host serves `AGENT` to
  (`host_agent`'s liveness connect on `agent-link-<host>.sock` or
  `agent-route-<host>.sock`: each `Bridges::reconcile` of its daemon's
  poll) opens and closes a stream to that host's agent, a routed one's
  opening included. → [machines](architecture/daemon/machines.md#agent);
  `bridge::agent_live`
- Several hosts granting one machine agent each get an
  `agent-link-<host>.sock` or `agent-route-<host>.sock` there; processes
  there use the newest live socket (mtime), not their caller's.
  → [machines](architecture/daemon/machines.md#agent); `endpoint::agent_sockets`
- An introduction's proposal that fails at `machine add` (no path to
  the authority then; the add's answer says why) has no CLI verb to
  retry it: the member stays capped until `membership.propose {name}` is
  called on its introducer.
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes);
  `machines::api::add`, `machines::changes::propose_introduction`
- A member only introduced (capped, not in the log) can't be removed with
  `machine rm`: the log has no key of it to revoke; it goes once its
  introducer is revoked. → [fleet](architecture/daemon/fleet.md); `machines::changes::revoke`
- Link reports (fleet step 10b.2) are versioned by their signer's wall
  clock in ms: which end of a link the graph believes is decided across
  two members' clocks, and a member whose clock went back after a daemon
  restart has its reports refused as older until the clock passes its
  last one (a peer's copy of its own last report moves it past only once
  received). The daemon's route sockets, and so every routed stream,
  follow it.
  → [fleet](architecture/daemon/fleet.md#link-graph); `fleet::graph::next_version`, `fleet::reports`
- [Exposure](architecture/daemon/fleet.md#exposure) takes either end's
  report of a link, so a link gone stays counted until both ends have
  reported since; a Machines tab row's `exposed via` follows a graph
  change at the next push (the next poll pass or machine change).
  → [fleet](architecture/daemon/fleet.md#exposure); `fleet::graph::exposed_workers`
- A routed stream doesn't fail over once open: a hop going down ends it,
  and its client opens it again.
  → [fleet](architecture/daemon/fleet.md#routed-streams); `fleet::routes::first_open`
- A member more than four hops away is unreachable (a routed stream's
  TTL). → [fleet](architecture/daemon/fleet.md#link-graph); `fleet::graph::MAX_HOPS`
- A capped (introduced) member can't rotate its key: the log has no key
  of it to rotate until the authority adds it.
  → [fleet](architecture/daemon/fleet.md#member-identity); `machines::changes::can_rotate`
- Fleets made before member identity (fleet step 10a) have no
  membership log: their links are refused until the fleet is made again
  with `machine add` (fleet plan decision 52).
  → [fleet](architecture/daemon/fleet.md#key-proof); `fleet::proof::Identity::load`
- `machine add` has no `--command`: it sets `command` only when it finds or
  installs devsandbox in `~/.local/bin` off the login `PATH`; a remote devsandbox
  anywhere else off `PATH` is set by hand in `machines.toml`.
  → [machines](architecture/daemon/machines.md#registry-machinestoml)
- `machine add`'s install copies this binary, so only to a machine of the
  same OS and arch (`install::platform_matches`); updating a devsandbox
  that's a symlink replaces the link with a file (`install::INSTALL`'s
  `mv -f`). → [machines](architecture/daemon/machines.md#install)
- Routed machines (no link of their own here) have no rows but the
  control plane's federated view's, one hop only (the machines the control plane itself links
  to), and a member watches its control plane federated only while one of its own
  watchers wants machines; a graph member the view doesn't list is a
  `machine ls` row with no roots, so the federated `sandboxes.list`
  doesn't ask it, and isn't on the dashboard's Machines tab, whose routed
  rows are the view's, `via` the control plane. A routed machine's forward is
  dropped by a sync once it's no routed machine (no path in the graph,
  not in the control plane's view).
  → [machines](architecture/daemon/machines.md#routed-machines); `snapshots::routed`, `snapshots::fleet_view`, `snapshots::merge`
- `machine control-plane` moves the role only (fleet plan decision 70): the new
  control plane's federated view is the machines it links to, and a member watches
  its control plane federated only over a direct link, so a control plane off the routing
  centre (A in `A → K ← B`) shows fewer rows than K would, and B none
  of the fleet's; watches over routes are parked. A member whose only
  links go to a control plane that is down learns of a move only once one of them
  is back. Right after a move a link can drop twice (both ends cycle
  it: the authority after its push, the other end after its answer), so
  a call made then may fail once. Only the authority signs a move, so a
  control plane that is down can be replaced only while the authority is up; a
  fleet made without `--control-plane` has them on one machine, and with it down
  the role can't move (fleet plan step 18); a delegation is
  [planned](plans/active/control-plane.md).
  → [fleet](architecture/daemon/fleet.md#control-plane-mode); `machines::changes::follow`, `leave::After::ControlPlane`
- A grant for an inbound or routed machine takes effect there once this
  host's link report reaches it (pushed by the control plane or flooded by the
  links), then within a probe; while the report lists it, that machine
  probes this host every 2 s, a routed probe being a full routed
  opening, even while this host has no agent to serve.
  → [fleet](architecture/daemon/fleet.md#agent); `fleet::agent_routes`, `fleet::agent::PROBE`
- A verb this host forwards to a machine that reaches its agent through
  `agent-route-<host>.sock` (an inbound or routed machine) doesn't run
  with that socket as `SSH_AUTH_SOCK` (`Peer::agent` is only a dialed
  end's `agent-link-<peer>.sock`), so host git there doesn't use this
  host's agent; the machine's instances do (`bridge::host_agent`).
  → [fleet](architecture/daemon/fleet.md#the-trust-table); `daemon::Shared::fleet_peer`
- Remote rows live in memory except on a control plane (which reloads them from
  `remotes.json`): a fresh non-control-plane daemon shows a machine without rows
  until its link connects (machines seen via the control plane too, until it
  connects). → [machines](architecture/daemon/machines.md), [fleet](architecture/daemon/fleet.md#control-plane-mode); `snapshots::Remotes`, `machines::saved`
- `vscode` and `inbox` are refused for a machine's instances on the CLI;
  the dashboard's `o` on a remote row answers "not supported yet" (its
  `p` and the CLI's `port` work); nameless commands are local only; no
  default machine. → [machines](architecture/daemon/machines.md); `machines::cli::route`, `not_yet_code`
- A forward to a machine's service reached via an instance shows no
  listening process (the header's `helper: false` gives
  `ProcessLookup::None`; locally the `lsof` probe covers it).
  → [forwards](architecture/daemon/forwards.md#remote-bridges); `tunnel::remote_opener`
- A remote forward needs the machine's embedded helper to speak this
  devsandbox's frame protocol; a mismatch is the bridge's version-mismatch
  error ("update devsandbox on <machine>").
  → [forwards](architecture/daemon/forwards.md#remote-bridges); `bridge::mismatch_message`
- A remote forward to a machine whose link is down re-dials its tunnel
  socket after every `RETRY` gap (2 s), with no backoff, until it's
  removed.
  → [forwards](architecture/daemon/forwards.md#the-engine-forward); `forward::retry_gap`
- The npm client's `forwards.list(dir)` can't pass `machines` (the typed
  `call('forwards.list', {machines: true})` can).
  → [api](reference/api.md#forwards); `npm/devsandboxes/index.js`
- `logs --build --follow` on a machine's instance reads the machine's
  `build.log` by path, page by page: a new build rotating it mid-follow
  isn't followed into `build.1.log` as a local follow (which keeps the
  file open) would. → [api](reference/api.md#instancesbuildlog); `build_log::page_in`
- `inspect` on a machine's instance prints no browser sandbox line on
  stderr (it comes from the instance's record, which no API call
  carries).
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::calls::named`
- A piped `exec --session <id>` on a machine's instance without a helper
  fails (its argv is the helper's session client) where a local one runs
  as a plain exec.
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::pipe_mode`
- `--machine <m> ls` lists of a fleet root on `m` only the config roots
  its instances name (placed configs without an instance left don't
  show).
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::calls::config_roots`
- Terminals on a machine's instances without a helper, and service
  terminals there, are a plain `exec -it` under a host pty on the
  machine: not resumable, a dropped link ends them (it kills the
  runtime's exec client; the in-container process may run on, as before
  resumable exec). → [fleet](architecture/daemon/fleet.md#pty); `fleet::pty::decide`, `fleet::pty::run_tty`
- The dashboard opens no terminal in a machine's global service that no
  instance there uses ("no instance on `m` uses service `s`"): a `PTY`
  service dial names the instance whose service it opens, and `exec
  --service` takes an instance name.
  → [terminal](architecture/dashboard/terminal.md#what-a-tab-runs); `tui::remote::service_argv`, `fleet::pty::service_container`
- The capacity report's soft caps (`member.toml` `[caps]`) are edited by
  hand (no `member.set` param), and measured against the whole host's memory and
  disk use, not what placed work takes; `cpus` caps nothing measured. The
  report rides the snapshots only while a root is watched (`capacity.get`
  builds it any time), and an Apple `container` host reports no cpus or
  memory.
  → [fleet](architecture/daemon/fleet.md#capacity); `capacity::over_caps`
- Under the worker policy a placed instance's plain `devsbd notify`
  messages (not threads, which need `inbox`) still reach the worker's
  Inbox and desktop.
  → [fleet](architecture/daemon/fleet.md#worker-policy); `inbox::decide`
- No command sets this host's member name (what the machines it dials
  know it by, `fleet-link --peer <name>`): it's `member.toml`'s `name`,
  set only through `member.set {name}` (API) or by hand, else the
  hostname's first label; a rename counts from each link's next dial.
  And a machine can't stop taking placed work while staying reachable
  (lending's `drain` went with it).
  → [fleet](architecture/daemon/fleet.md); `fleet::member::default_name`, `fleet::dialer::member_name`
- A verb from a linked host (on either end of the link) runs with the
  machine daemon's own process env (a systemd unit's or LaunchAgent's,
  not a login shell's), so `PATH` additions from shell profiles are
  missing (the dialer's verbs on the dialed end get the dialer's agent as
  `SSH_AUTH_SOCK` while its `agent-link-<host>.sock` answers; the dialed
  end's on the dialer get the daemon's own); fix: capture a login env
  once at the daemon's start.
  → [fleet](architecture/daemon/fleet.md#the-trust-table); `fleet::lent::admit`
- A member's download of a published pinned build has not yet run against
  a release built with `DEVSANDBOX_PUBLISHED=1` (the first is the next
  tag), nor has the release workflow's checksum signing run with the
  real `RELEASE_SIGNING_KEY`. A member verifies against the release key
  its binary embeds: a new key reaches members only with a new binary,
  so after a rotation members on an older one refuse every newer
  release until updated by hand. A member with
  no release asset for its platform (an Intel Mac, a source build
  elsewhere) stays behind a published pin. A dev pin (from source,
  `cargo install` included) is taken only with `dev-builds = true` in
  `member.toml` (or trust `full`) and the pin's OS/arch, else the member
  stays behind; a mixed-platform dev fleet is built per platform by hand.
  A member taking dev builds waits for a link to a peer that has the pin:
  with none (the authority unreachable, no member on it yet) it stays on
  its build.
  → [fleet](architecture/daemon/fleet.md#versions); `build.rs` `stamp_build`, `fleet::update::decide`, `fleet::update::update`
- A member waiting for the pinned binary gives up only when the transfer
  hasn't started within 60 s (`PUSH_WAIT`) or fails; one that started and
  stalls while the link stays up waits until the link ends.
  → [fleet](architecture/daemon/fleet.md#versions); `fleet::follow` `Incoming::watch`
- The authority pins every new binary its daemon starts on (each
  `cargo build` run once), so members taking dev builds hand off with
  each, and each pin adds a log entry (synced within `proof::SYNC_LIMIT`);
  a pinned build whose update path is broken strands them until fixed by
  hand. When the authority's kept copy of a dev pin is gone, its push
  fails and a member waiting on it redials every `PUSH_WAIT`; so does a
  host that can't hash its own binary (its `Hello` says `-`), which is
  never on a dev pin.
  → [fleet](architecture/daemon/fleet.md#versions); `machines::changes::sign_build`, `fleet::update::pushes`, `fleet::update::keep_build`
- A member whose update to the pin fails drops the link at each dial: a
  peer that dialed it sees the reason as its row's error (ssh's stderr
  tail), a peer it dialed only sees the inbound machine come and go (the
  reason is in the member's `machine ls` and `serve.log`).
  → [fleet](architecture/daemon/fleet.md#versions); `fleet::dialer`, `commands::fleet_link::run_peer`
- An inbound machine lasts while its link is up: when it goes (a
  laptop asleep, its network down) its rows and its forwards on this
  host go with it, not kept as `unreachable`, and come back (forwards
  not) when it dials again.
  → [machines](architecture/daemon/machines.md#inbound-neighbors); `forwards::Known`
- A member removed while unreachable (`leave` `unreachable`) keeps its
  `control_plane` and its registry entries; its links are refused once the peer
  holds the revoke, and it never takes a log that revokes it except from
  a push (`fleet.sync`). `machine rm` it again on the authority once it's
  reachable, or run `machine rm <its name>` on it.
  → [fleet](architecture/daemon/fleet.md); `machines::leave::rm`
- Members the control plane reaches by no path (no link up, no path in its link
  graph) keep their registry entry for a removed member: `fleet.forget`
  goes once, to the control plane's machines and its routed members. The revoke
  reaches them (a push, or their next link's head comparison), so that
  entry's link is refused.
  → [fleet](architecture/daemon/fleet.md); `machines::leave::remove`
- A member that leaves (`machine rm <itself>`) is forgotten by the other
  members only once the authority approves its proposed revoke; until
  then only the control plane drops its entry.
  → [fleet](architecture/daemon/fleet.md); `machines::leave::rm`
- Placement's `has_image` reads only watched roots' snapshots (this
  host's while watched, a machine's last pushed; a routed machine has
  none, so never has the image), and `has_repo` is never
  filled (no repo locality yet).
  → [fleet](architecture/daemon/fleet.md#in-the-run-verb); `fleet::placed::candidates`
- Only `run --place` is placed, and only by the daemon's `run` verb: a
  `run --place` with no daemon to reach and `--in-process` runs stay
  here, with a note; the dispatcher's children aren't placed yet (fleet step 13). A
  control plane (`control_plane = "self"` in `member.toml`) places its own; a member's is
  placed by the control plane (`fleet.place`, up to 2 s for the dial and 6 s for
  the answer) among the machines both the member and the control plane reach (a
  link or a route), and runs on the member with a note when the control plane
  can't be reached. On a control plane every connected machine is a candidate (ssh
  and inbound), and every routed one a path reaches, each asked a
  `capacity.get` (2 s at most) first.
  The placing host's `state.toml` doesn't record a placed instance (it's the
  machine's, seen as `<machine>/<name>` in federated views; fleet step 13).
  Moving the
  code runs before the forward and isn't cancelled by a hang-up or Ctrl-C
  meanwhile (the run, once forwarded, is). A new `--branch` starts at the
  source checkout's branch tip, not `origin/HEAD` as run in place; a
  `--base` or `worktree-base` naming an `origin/…` ref fails on the
  worker (its checkout has no remote).
  → [fleet](architecture/daemon/fleet.md#in-the-run-verb); `fleet::placed::start_point`
- A placed `run` moves each folder's committed and uncommitted work
  from its checkout on the placing host, no remote needed: the branch
  and a snapshot of the working tree (untracked files included, ignored
  ones not, nor the `.worktreeinclude` ones a worktree wants), or a
  requested branch that isn't checked out as committed. A detached
  `HEAD`, a `folder` inside the config root (the root travels as a tar
  without `.git`), a `folders` entry with a `${…}` variable or outside
  the root but not a checkout doesn't move. `folders` entries inside the
  root travel in its tar without their `.git`. The root's tar skips
  `.git`, `.worktrees`, `shared-volumes` and `shared-files` but not
  git-ignored files, and a symlink pointing out of the root refuses it.
  On the worker, a checkout no instance mounts is recut to what was
  sent, its snapshot applied; one an instance mounts keeps its working
  tree and its branch unless it can fast-forward cleanly, so a new
  worktree there may start from the older tip; two placements of one
  root at once can both see it unmounted (the instance is recorded only
  once it runs), so the second may recut it under the first. A repeat send
  carries only what the worker lacks (`code.haves`: a root it has isn't
  sent again, a bundle goes `--not` its cache's tips), but a first send
  goes whole over the link. A repo with no commit yet gets
  `refs/devsandbox/root` once and keeps it. The worker's cache per repo
  under its fleet root is never pruned, and `roots/<hash>/` trees are
  never removed.
  → [fleet](architecture/daemon/fleet.md#moving-code); `fleet::code::offer`, `fleet::code::send`, `fleet::code::repo::land_from`
- The control plane keeps a copy of each repo a member places on another machine,
  sent after the worker's (so a placement from a laptop uploads twice),
  never pruned and not yet read by anything (re-placement is fleet step
  14). → [fleet](architecture/daemon/fleet.md#moving-code); `fleet::code::mirror`
- Fetch-back (`devsandbox fetch <machine>/<instance>`, and before an
  `rm` or `done` of a placed instance typed on the host that placed it)
  brings back only the `folder` repo's branch and WIP (not `folders`
  entries'); the dashboard has no fetch of its own (its `rm` / done do
  fetch back), and its done blocks its daemon connection, snapshots
  included, while the fetch-back runs. A `placements.json` entry is
  dropped only by an `rm` typed on the placing host: one for an instance
  removed elsewhere (on the worker, or from another host) stays. Entries
  are keyed by the instance's name there: after a rename, an `rm` or
  `done` of the new name (or of its id) doesn't fetch back. A bundle the worker made under `out/` for a host that never
  pulled it is swept only when the next one is made (after 1 h). A
  worker's instance whose history gained another root (an unrelated
  merge) no longer matches the placed repo and is refused.
  → [fleet](architecture/daemon/fleet.md#moving-code); `fleet::code::fetch`
- What ties a sandbox to its host (a bind outside the config root and its
  moved folders, `${localEnv:…}`, `privileged`, …) keeps it
  home whatever a worker's trust or `member.toml` allowances: nothing
  checks whether another host could satisfy it until named host
  resources (fleet plan decision 41). GPUs are the exception: placement
  filters them, so a required GPU is not a tie. X11 has no check of its own beyond
  its bind. A pin names only home (`placement = "home"`): a list of the
  members a sandbox may run on isn't built. Nothing in `shared-volumes/`
  follows a placed instance (logins kept there): on the worker its
  `${sharedVolumes}` dirs, caches and shell history are its fleet root's
  (empty the first time; mesh state, fleet plan decision 43, isn't
  built). `worktree-link` and `worktree-include` are skipped there with a
  warning, so `.env`-style files don't follow either.
  → [fleet](architecture/daemon/fleet.md#moving-code); `fleet::code::ties`
- A push of a signed config blocks the authority's verb until each
  machine has installed it, one machine after another (up to
  `SYNC_WAIT`, 10 min, each). That's a new version's push, and a verb on
  an unchanged root re-sending to the machines the last push missed: one
  of those whose link is half-open blocks each such verb the full wait
  and stalls that link's other calls, and one whose link is down is
  named on every such verb until it's reached.
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `changes::push_config`, `changes::SYNC_WAIT`
- The dashboard drops a verb's config signing notes (all its output), and
  under `-d` the CLI shows only its `note:` lines.
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `tui::spawn_verb`, `verbs_client::held_notes`
- Autostart records the boot before acting, so a signed config's copy
  that is edited in place at boot refuses that pass's starts until the
  next boot.
  → [autostart](architecture/automations/autostart.md); `commands::autostart::autostart`
- A machine offline when a config was signed, which installs it after
  linking to a host that already has the log, sends that host no sync,
  so the host lists the new copy among its roots only at the next sync
  with it or link-up.
  → [machines](architecture/daemon/machines.md#held-links-machines); `daemon::follow_configs`, `Handle::relist`
- A signed config doesn't bundle a `build.context` of `.` (the root) or
  the files a Dockerfile copies from the root, and a `${configDir}` dir
  bind mount's directory isn't walked for files added to the copy (only
  build contexts and local features are).
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `mesh::collect`, `Bundle::dirs`
- Relative folders and signed copies share `mesh/tree/`: a `folder =
  "../x"` and a signed root named `x` land in the same directory.
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `mesh::tree_path`
- A signed copy's folders the authority's `run` lands on a member: not
  `${…}` folders, folders inside the copy (`folder = "."`), nested
  folders, absolute ones, or a checkout with a detached `HEAD`; a
  `landed` checkout's WIP isn't applied again, its checked-out branch
  only fast-forwarded on a clean tree; a run's `--branch` goes to the
  first folder `mesh.folders` answers, which is a `folders` value when
  the sandbox has no `folder`. Parked: a clone from `origin` when the
  authority has no checkout, and fetch-back for these runs.
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `code::folders::classify`, `code::folders::receivers`
- Temp files a crash leaves in `mesh/bundles/` and `mesh/` (a store's,
  an install's unpacking dir, a pull's) are never cleaned up.
  → [fleet](architecture/daemon/fleet.md#approvals-and-pushes); `mesh::store`, `mesh::install`, `mesh::pull_bundle`
- After a worker's downgrade, a dashboard or other long-lived client still
  running the replaced, newer binary hands the new daemon off again at its
  next hello (its version is newer), restarting the older binary each
  time until that client is restarted.
  → [role](architecture/daemon/role.md#version-handoff); `daemon::hello`

### helper

- Builds without an embedded helper (`cargo install` from crates.io) have
  no notifications, control requests, runs, port forwards or boot hook;
  ssh-agent falls back to the bind mount. → [helper](architecture/helper/overview.md); `devsbd::embedded`
- Images without `/bin/sh` can't get the helper, which in relay-first
  builds means no ssh-agent at all. → [ssh-agent](architecture/helper/ssh-agent.md); `devsbd::INSTALL_SCRIPT`
- No ssh-agent forwarding on native Windows hosts. Mount mode (no-helper
  builds) forwards nothing on macOS + podman / Colima-style docker or on
  Apple `container`. Instances created in mount mode keep it until
  recreated. → [ssh-agent](architecture/helper/ssh-agent.md); `relay_mode`
- On `start` of a `"runtime"`-restart instance the boot hook fires before
  the host rewrites the boot file, so that start runs the previously
  recorded `postStartCommand`/env. → [in-container](architecture/helper/in-container.md); `devsbd::sync_boot`
- Images with a tmpfs `/run` lose the helper and boot file on a runtime
  restart: `postStartCommand` doesn't rerun until a host `start`.
  → [in-container](architecture/helper/in-container.md)
- Two CLI builds used on the same containers evict each other's
  in-container daemon on every install, cutting live agent streams.
  → [protocol](architecture/helper/protocol.md); `devsbd` `take_over`
- An older CLI exec'ing a newer helper sends no `Caps`, so that bridge
  never carries agent streams. → [protocol](architecture/helper/protocol.md); `known_host_caps`
- A record the host can't decode (helper newer than the daemon) stalls the
  outbox at that record. → [bridges](architecture/helper/bridges.md); `handle_notify`
- `devsbd boot`'s commands run without the user's supplementary groups
  (`cmd.uid/gid` clears them), unlike `docker exec -u`; fix: switch in
  `pre_exec` with `supplementary_groups`, as `rpc_runs::switch_user` does.
  → [in-container](architecture/helper/in-container.md); `boot::run_spec`
- On a rootless engine whose runc predates 1.4.1, exec'd processes land
  outside the container's cgroup once its root cgroup has controllers
  enabled (docker-in-docker), so a nested dockerd restarted from a shell
  or hook can't start containers. devsandbox only warns (`run`, `start`,
  `inspect`); the fix is the host's runc. The check needs the helper.
  → [lifecycle](architecture/instances/lifecycle.md#the-exec-cgroup-check);
  `run::cgroup::warn_if_outside`
- A session's program exiting 250, 251 or 252 reads as the session's own
  end (`NO_SESSION_EXIT`, `DETACHED_EXIT`, `VERSION_EXIT`): over a machine
  the reconnect loop stops and says the session is gone, was attached
  elsewhere, or is another devsbd's.
  → [in-container](architecture/helper/in-container.md); `serve::machines::cli::Reconnect`
- A session whose client's terminal went keeps listing as `attached`
  (the in-container client docker leaves behind stays connected and
  can't tell) until the next attach displaces it.
  → [in-container](architecture/helper/in-container.md); `commands::exec::choose_session`
- The helper's arch is found by a blind second try, no probe.
  → [helper](architecture/helper/overview.md); `devsbd::candidates`
- A container's URL chooses which free loopback ports the opening host
  binds for it (its own port, an OAuth callback's): a sandbox can hold a
  free well-known port a host tool then connects to, for as long as the
  forward lives. → [forwards](architecture/daemon/forwards.md#open-forwards); `forwards::bind_open`
- Loopback hosts in shorthand (`127.1`), with a zone id or
  percent-encoded aren't recognized: such a URL opens unforwarded, on the
  host's own loopback. → [bridges](architecture/helper/bridges.md#opening-urls-kind-open); `open::loopback_ports`
- Attach records live in the daemon's memory: after a restart or handoff
  on either side, a remote instance's URLs open on its own host (or are
  refused) until the user attaches again. Renaming the instance between
  the attach and an open makes the opening member refuse it.
  → [fleet](architecture/daemon/fleet.md#last-attach); `serve::attaches`
- An open forward into another machine's instance doesn't stop when that
  instance stops, only when idle for 10 min or when the machine goes.
  → [forwards](architecture/daemon/forwards.md#open-forwards); `forwards::Daemon::expire_open`

### inbox

- `devsandbox inbox done|reopen|act` and the API's `inbox.thread.*` change
  the thread only; the child instance's `done` flag flips only from the
  dashboard (`d`/`u` or a `done: true` button). → [model](architecture/inbox/model.md);
  `wire::decide_done`, `mark_thread_child`
- `devsandbox inbox` has no draft-save, notify dismiss or mark-read
  (API-only). → [inbox cli](reference/inbox-cli.md); `InboxCommand`
- The Inbox is per host: each host has its own store; the dashboard shows
  only this host's. → [model](architecture/inbox/model.md)
- An `inbox.json` from a newer schema version is unreadable by an older
  build. → [model](architecture/inbox/model.md#store); `Inbox::from_json`
- `thread ls` fails ("too large to list") past ~512 KiB of JSON.
  → [threads](architecture/inbox/threads-and-feed.md); `EVENTS_BODY_CAP`

### automations

- Autostart never runs off Linux/macOS (no boot id). → [autostart](architecture/automations/autostart.md);
  `read_boot_id`
- After a runtime restart, bridges, forwards, notify delivery and control
  resume only once the daemon runs (`serve install` or the next command).
  → [autostart](architecture/automations/autostart.md)
- `ensure --env` on an existing child changes only the saved env applied
  to `devsandbox exec` (the container's own env changes on rebuild);
  `ensure --branch` is ignored for existing children.
  → [dispatchers](architecture/automations/dispatchers.md); `ensure_action`
- Dispatcher lifecycle ops are serialized host-wide: one long `ensure` (up
  to 30 min) blocks every other dispatcher's `ensure`/`stop`/`rm`/`done`.
  → [dispatchers](architecture/automations/dispatchers.md); `dispatch_control` `LOCK`
- A dispatcher's `run logs --follow` and `run wait` still poll the host with
  short control requests (`devsbd/src/ctl.rs` `wait_once`, `follow`), though
  the host → child leg is one held `run.wait`/`run.logs` stream; fix: a
  streaming control op (like `events-follow`) relaying `run.logs follow`.
  → [dispatchers](architecture/automations/dispatchers.md); `wait_once`

### dashboard

- A thread action waiting on a detached start (`after_start`) never fires
  if the start fails after it was registered.
  → [inbox tab](architecture/dashboard/inbox-tab.md); `start_answered`
- `d` on an instance ignores modifiers: ctrl-d removes it without a prompt.
  → [tabs](architecture/dashboard/tabs-and-actions.md); `dispatch_key`
- Service terminal tabs run `<runtime> exec -i -t` directly, not
  `devsandbox exec` (two terminal paths). `TERM=xterm-256color` reaches
  only the host-side child. → [terminal](architecture/dashboard/terminal.md); `term_argv`
- The Instances tree doesn't show a group's config root: two roots
  defining one sandbox name are two groups that look alike.
  → [tabs](architecture/dashboard/tabs-and-actions.md); `tui::data::GroupKey`
- Kitty keyboard emulation implements only "disambiguate".
  → [terminal](architecture/dashboard/terminal.md); `kitty::DISAMBIGUATE`
- OSC 52 is the only clipboard path: copies are cut at 1 MiB, and terminal
  app copies over the base64 bound are dropped silently.
  → [selection](architecture/dashboard/selection.md); `clipboard::MAX_COPY`
