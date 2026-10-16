> Archived 2026-10-16. Historical, may be wrong; do not use as a reference.
> Current: [README.md](../../../README.md)

# Triage lead (step 2d): verdicts for the old flat docs

Read-only pass over the 40 old `docs/*.md` (skipped: docs-site, landing-how-it-works,
examples-rewrite, dashboard-docs, README, status). Done by the lead directly:
the spawn pool was full the whole run. Evidence = my own spot-checks plus the
area leads' verified stale claims (`instances.md`, `daemon.md`, `helper.md`,
`dashboard.md`, `inbox-automations.md`, `decisions.md` in this folder), which I
re-checked where they decide a verdict.

Rule used: `archive` = shipped, still mostly true, carries rationale beyond the
ADRs; `delete` = high staleness, a step plan whose rationale is already in
`decisions/`, or a living doc now duplicated by `architecture/`/`reference/`.
`active` docs already have (or need) a trimmed successor in `plans/active/`;
the old file is then deleted, not archived.

Inbound refs: `git grep --untracked` over tracked + new files, excluding
`site/dist`, `site/.astro`, `.zidane/`, this folder, and `reference/`/
`architecture/` paths of the same basename. "hits/files" then a breakdown:
src (incl. `devsbd/`), old docs, new docs (`architecture|reference|decisions|plans`),
other files named.

## Verdicts

| doc | verdict | staleness | evidence | inbound refs (hits/files; key files) |
|---|---|---|---|---|
| api.md | living → delete | low | Same headings as `reference/api.md` minus 3 (the new copy adds Index, drops "dashboard/CLI as client" which moved to `architecture/daemon/client-server.md`); superseded | 90/48: src 15 (`serve/*`, `tui/daemon.rs`, `inbox/wire.rs`, `build_log.rs`, `runtime/procs.rs`), npm `index.d.ts`/`index.js`/`README.md`, AGENTS, site `host-daemon.mdx` + `serve-lifecycle.mdx`, CHANGELOG, 11 new docs |
| automations-guide.md | living (user guide) → keep | med-low | User-facing guide, not a plan; one stale trigger claim (dashboard launch runs autostart). Linked by **absolute GitHub URL** from the site | 26/14: README, `skills/config-toml-spec/SKILL.md` (2), site `dashboard.mdx`, `examples/babysit-pr.mdx` (URL ×2), AGENTS, CHANGELOG, `inbox/thread.rs` |
| automations.md | archive | med | All steps shipped; rich rationale (once-per-boot, `"runtime"` mode, authorization); stale: TUI-load autostart, `--env` creation-only, `file:line` cites | 58/42: src 22 (`autostart.rs`, `dispatch.rs`, `devsbd/*`, `config.rs`), `devsbd/src/{boot,ctl,outbox,runs}.rs`, decisions 0024/0026/0028/0029/0030, npm `index.d.ts`, site `bridge.mdx`/`host-and-helper.mdx`, AGENTS |
| before-machines.md | delete | low | Pure step plan, all 7 steps shipped (`exe::devsandbox`, verbs `tty`, `instances.watch`, held verbs); content in `daemon/host-poll.md`, `verbs.md`, ADR 0018 | 14/8: old docs 5, new 3 |
| client-server.md | archive | low-med | Umbrella design; Status line correct (steps 1–5 done); steps 6–7 + default machine → status.md | 22/13: `serve/verbs.rs` (2), old docs 7, new 5 |
| cli-proxy.md | active → delete old (successor `plans/active/cli-proxy.md` exists) | high | Nothing built (no `cli-proxy` member in `Cargo.toml`, `config::Service` has no `mounts`); landmarks and premises stale | 5/3: old docs only (`sandbox-helper.md`, `helper-rpc.md`, `automations.md`) |
| config-toml-findings.md | delete | med | Findings log for a `data/config.toml` era; all DONE/OBSOLETE except #7 (`runArgs`, parked, in instances fragment) | 4/2: `devcontainer-features.md`, decision 0008 |
| dashboard-client.md | archive | med-low | Done (steps 1–9); Decisions = rationale for ADR 0031; stale `current_exe()` note + limitation | 17/13: `tui/poll.rs`, `tui/mod.rs`, old docs 8, new 3 |
| devcontainer-features.md | delete | med | Shipped; line refs rot; **file is corrupted** (lines 31–57 duplicate the header and lines 1–26 mid-sentence, since 002dda3); limitations already in `images-and-features.md`, rationale in ADR 0013 | 2/2: `high-level-architecture.md`, decision 0013 |
| devsandboxes.md | delete | high | Original vision: "No daemon", compose, `cache-folder`, docker-compose runtime; all false | 1/1: decision 0002 |
| dispatcher-branch-holders.md | delete | low-med | Implemented; covered by `automations/dispatchers.md` (branch holders); `file:line` cites rot | 2/2: `automations.md`, decision 0030 |
| entrypoint.md | living → delete | low-med | Chain covered by `instances/images-and-features.md#entrypoint-chain`; move its "Differences with the devcontainers CLI" table there first (not in any new doc) | 6/6: `src/features.rs`, `skills/config-toml-spec/SKILL.md`, old docs 3, new 1 |
| folders-worktrees.md | delete | low-med | Implemented; rule in `instances/worktrees.md` + ADR 0004; line refs rot; parked items in instances fragment | 8/6: `src/config.rs` (2), `src/state.rs`, `run/folders.rs`, old 2, new 1 |
| helper-rpc.md | active → delete old (successor `plans/active/helper-rpc.md` exists) | med | Not built (no RPC channel in `devsbd/proto.rs`); ctl path stale | 4/4: old docs only |
| high-level-architecture.md | living → delete | med | Overview to be replaced by `architecture/overview.md`; ~5 stale claims (forwardPorts, drift strip list, feature hashing, remote-row actions, namespacing) | 12/10: decisions 0001–0004, 0009, 0010 + 1 (Source lines, plain text), old 3 |
| ideas-agents-skills.md | active (parked) → delete old (successor `plans/active/agents-skills.md`) | low | Nothing built; successor already trimmed | 1/1: `processes-stop-colors.md` |
| inbox-cli.md | living → delete | low | Accurate but superseded by `reference/inbox-cli.md` (adds `ls` columns, errors, `--machine` refusal) | 14/12: `src/main.rs`, `commands/inbox.rs`, README, AGENTS, `automations-guide.md` (2), plan file |
| inbox-redesign.md | archive | med | Shipped (steps 1–20 + 13b); principles live in ADR 0027; plan voice, dated decisions, `file:line` cites | 42/26: src 15 (`inbox/feed.rs` 3, `inbox/mod.rs`, `tui/app/forms.rs`…), site `serve-lifecycle.mdx`, old 7, new 3 |
| inbox-threads.md | delete | high | Self-declared superseded by inbox-redesign: dispatcher-only owners, `inbox.toml` v2, `message`/`reply` header, dashboard bridges | 33/23: **src 16** (`inbox/thread.rs` 3, `inbox/view.rs`, `tui/app/{inbox,thread_actions}.rs`, `devsbd/notify.rs`, `dispatch.rs`), new 4 |
| instance-phase.md | archive | low-med | Done (steps 1–7); Decisions back ADR 0007; TUI "detached `devsandbox` subprocess" now a verb request | 30/22: src 13 (`rebuild.rs`, `build_log.rs`, run/*), old 8, new 1 |
| json-output.md | delete | low-med | Shipped incl. every "Added after"; envelope in `dashboard/snapshot-and-json.md` + ADR 0035; non-goal "CLI is the API" superseded | 13/8: CHANGELOG (3), old 6, new 1 |
| machines.md | living → archive | low | "Status: built"; limitations verified true (BatchMode, no `--command`, remote `o`/`p` refused); covered by `daemon/machines.md`; *Later* → status | 72/42: **src 25** (`serve/machines/*`, `daemon.rs`, `api.rs`, `tui/app/actions.rs`), AGENTS (2), site `host-daemon.mdx` (URL), `serve-lifecycle.mdx`, CHANGELOG, new 8 |
| port-forwarding.md | archive | low-med | Shipped; Known limitations still true; *Flow control* section describes pre-step-3 code in present tense | 36/19: src 9 (`commands/port.rs` 6, `devsbd/{mux,bridge,forward}.rs`, `serve/forwards.rs`), AGENTS, site `bridge.mdx`/`host-and-helper.mdx` |
| processes-stop-colors.md | delete | high | Steps 10–12 describe dashboard-side `docker stop`/`docker top`/inspect, all now daemon requests | 0 |
| rebuild.md | delete | high | Step plan with dead line refs; both Out-of-scope items exist (`rebuild --all`, `--force`); "`ensure_services` recreates drifted services" false | 4/4: AGENTS, old 2, decision 0009 |
| run-command.md | delete | high | Reuse-on-run reversed, compose, project network, features "out of scope": core claims false; ADRs 0005/0006 hold what survived | 4/4: old 1, decisions 0004/0005/0010 |
| runtimes.md | living → delete | low | Accurate; fully covered by `architecture/runtimes.md` | 6/5: README (2), old 1, new 3 |
| sandbox-helper.md | archive | low-med | Shipped helper design; 4 stale points (bind-mount fallback, size budget, TUI bridges); covered by `architecture/helper/*`, ADRs 0021–0024 | 42/28: src 10 (`devsbd.rs`, `devsbd/{proto,mux}.rs`…), `build.rs`, AGENTS, site `bridge.mdx`/`host-and-helper.mdx`, new 5 |
| serve.md | living → delete | low | Current (edited 2026-10-16) but every section is in `daemon/role.md`, `host-poll.md`, `forwards.md`, `machines.md`; *Future work* trade-offs → status | 56/32: src 9, AGENTS, README, site `host-daemon.mdx` (URL), `serve-lifecycle.mdx`, CHANGELOG, old 16 |
| service-rebuild.md | delete | med | Step plan, shipped; every landmark a dead line ref; covered by `instances/services.md`, ADRs 0009/0011 | 3/3: old 1, decisions 0009/0011 |
| ssh-agent.md | delete | med | Shipped; landmarks dead (`run.rs:469`, `tui/app.rs`), "Apple: blocked today" heading wrong; covered by `helper/ssh-agent.md` + ADR 0025; Windows pipe → status | 29/14: src 5 (`run/ssh_agent.rs` ×8, `state.rs`, `start.rs`), README (2), site `host-and-helper.mdx`, new 4 |
| tui.md | delete | high | Original step plan: 3 tabs, toast, `Node::EmptyMarker`, dashboard polling the runtime, `r` → run; superseded by dashboard-client + `architecture/dashboard/*` | 16/9: old 8, new 1 |
| tui-prompt-spec.md | delete | med | Shipped; `COMMANDS`/`app.rs` names stale; covered by `dashboard/command-line.md` + ADR 0032; parked → status | 1/1: decision 0032 |
| tui-selection.md | archive | low | Steps 1–8 match code; only *Problem* is pre-state; parked → status | 12/10: src 5 (`tui/{select,settings,markdown,clipboard}.rs`…), AGENTS, new 2 |
| tui-terminal.md | delete | med | `dirty`/focused flags gone, `app.rs` landmarks; covered by `dashboard/terminal.md` (kitty, `ctrl-]`/`F12`) + ADR 0033 | 9/5: AGENTS, old 3, new 1 |
| verbs-as-requests.md | archive | low-med | Done (steps 1–6); "five verbs", dashboard `rm`/`stop` suspended stale; rationale behind ADRs 0016/0017 | 12/10: `serve/verbs.rs`, `verbs_client.rs`, `main.rs`, AGENTS, new 3 |
| vscode-workspaces-and-persistent-state.md | delete | med-high | "extra folders: no worktree", "rewritten on every reuse", `${…}` in cache sources: false; covered by `instances/mounts-and-state.md`, ADR 0006 | 1/1: decision 0006 |
| windows-support.md | active | low-med | "Status: plan, not started": true (no Windows client path; `endpoint.rs` unix-only); no `plans/active/` successor yet (plans lead) | 4/3: old docs only (`dashboard-client.md` 2, `serve.md`, `client-server.md`) |
| worktreeinclude.md | living → delete | low | Accurate; covered by `instances/worktrees.md#.worktreeinclude-and-worktree-link`; user semantics in the config spec skill; out-of-scope → status | 4/3: `run/worktree.rs` (2), `src/config.rs`, `skills/config-toml-spec/SKILL.md` |
| worktree-locking.md | living → delete | low | Accurate; covered by `instances/worktrees.md#locking`; Limits → status (instances fragment) | 3/3: CHANGELOG, decision 0004, `helper-rpc.md` |

Totals: archive 10 (automations, client-server, dashboard-client, inbox-redesign,
instance-phase, machines, port-forwarding, sandbox-helper, tui-selection,
verbs-as-requests); delete 25 (incl. 8 `living → delete`); active 4 (cli-proxy,
helper-rpc, ideas-agents-skills, windows-support; the first three already have
successors); keep 1 (automations-guide).

## Stale claims

Mine (checked this pass), then the leads' that I re-checked or that set a verdict.

- devsandboxes.md: "**No daemon**" goal / state store "no daemon" → `devsandbox serve` (`src/serve/`). Compose (`dockerComposeFile`, docker-compose runtime) → removed, no compose fields in `config::SandboxProperties`. `cache-folder` key → `caches`. `devsandbox init` "deferred" → still absent (no `Init` in `main.rs` `Command`): true but moot.
- high-level-architecture.md: `forwardPorts` "not implemented, parse but warn at `run`" → implemented by the daemon (`serve::forwards`, `SandboxProperties::forward_ports`); `runArgs` still warns (`ignored()`). Drift: "`autostart` and `dispatcher` stripped before hashing" → also `inbox`, `forwardPorts`, default-restating `folders` (`config::sandbox_hash`, `normalize_folders`). "devcontainer features aren't hashed" → local-folder features are in `build_hash` (`config::sandbox_build_hash`). Config file `devsandboxes.toml` only → `.yaml`/`.yml` too, legacy `config.toml` migrated (`config::CONFIG_FILES`, `migrate_legacy`). Dashboard "every action and terminal works on machine rows" → `o`/`p` on remote rows answer "isn't supported yet" (`tui/app/actions.rs`). "networks and containers named with a short hash of the config root, so two projects never collide" → image tags and feature build dirs aren't namespaced (`run::image::base_image_for`, `services::service_image`).
- run-command.md: `run` on an existing stopped container `docker start`s it → `run` refuses a taken name, `start` restarts (`run::claim_name`). nameConfig "rewritten on every run" → only when extensions/`remoteUser` set, and on every `vscode` launch (`run::materialize`, `vscode::launch`). "every instance joins the project network" → global net only with a global service, plus per-instance net (`services::ensure_services`). Compose override files, `rm --services` → gone (`gc`). Features "out of scope" → implemented (`src/features.rs`). Container cmd `sleep infinity` → `sh -c '[ -x devsbd ] && devsbd boot & exec sleep infinity'` (`run::container_command`).
- rebuild.md: Out of scope "`--force` rebuild without drift", "`rebuild --all`" → both exist (`main.rs` `Command::Rebuild { all, force }`). "`ensure_services` already recreates drifted service containers" → warns only; `service rebuild` recreates (`services::ensure_service`). TUI `s` on drifted rebuilds via `run_suspended` → verb request (`tui/app/actions.rs` `Exited(_) if row.drift`, `tui/mod.rs::spawn_verb`). Every landmark (`run.rs:16-219`, `tui/app.rs`, `tui/data.rs`) dead.
- service-rebuild.md: "`ServiceRow` has no `drift`", "no `service ls`" → both shipped (`main.rs` `ServiceCommand::{Ls,Rebuild}`); all `file:line` landmarks (`run.rs`, `ui.rs`, `app.rs`, `snapshot.rs:558`) dead.
- cli-proxy.md: "`ensure_services` already recreates a service container when its `config_hash` drifts" → warns only. Landmarks `services.rs:19-25…`, `config.rs:477`, `run.rs:755`, TUI `run_suspended` → dead/gone. "Interim state: `tools` service in the live config" → dropped (per `plans/active/cli-proxy.md`).
- config-toml-findings.md: `${configDir}` = "directory holding `config.toml`" → `devsandboxes.toml` (`config::CONFIG_FILE`). #6 "OCI refs only, no local paths" → local folders supported (`features::FeatureSource::Local`).
- devcontainer-features.md: "flagged as ignored at `src/config.rs:67,103`", `run.rs:471`, `state.rs:63` → dead line refs, `features` is typed. Body corrupted: lines 31–57 repeat the title and lines 1–26 inside the step-4 sentence (committed in 002dda3).
- entrypoint.md: chain ends in `sleep infinity` → the container command also starts `devsbd boot` (`run::container_command`). "Feature metadata isn't part of the config hash" → local features' contents are in `build_hash` (OCI ones still aren't).
- vscode-workspaces-and-persistent-state.md: "Extra folders are plain shared mounts: no worktree treatment" → worktrees under `auto`/`always` (`folders::needs_worktree`). Workspace file "rewritten on every reuse" → no reuse; written on `run`/`rebuild` (`materialize`). "`${sharedVolumes}`/`${instance}` substituted in cache sources" → cache sources fixed (`resolve_caches`). "Apple `container` untested" for nested binds → mounts sorted parents-first on every runtime (`sort_parents_first`).
- folders-worktrees.md / ssh-agent.md / tui-terminal.md / tui-prompt-spec.md / json-output.md / dispatcher-branch-holders.md / inbox-redesign.md / automations.md: `file:line` and pre-split paths (`src/commands/run.rs`, `src/tui/app.rs`, `tui/data.rs:106`, `dispatch.rs:557`, `start.rs:61`, `devsbd.rs:108`) → files split into `run/`, `tui/app/` dirs; lines no longer match.
- automations.md / automations-guide.md: autostart triggered by "the first TUI load / dashboard launch", `run`/`start` → the daemon's startup pass plus CLI `run`/`rebuild`/`start` (`serve::host` `autostart_all`, `main.rs` `Command::Rebuild` arm; `tui/mod.rs` "No autostart pass of our own"). automations.md "`--branch`/`--env` apply at creation only" → `--env` replaces a child's saved env (`dispatch::replaced_env`). automations-guide.md "daemon started by the dashboard, `run` and `start`" → also `rebuild` and any lazy client (`client::connect`).
- inbox-threads.md: only `dispatcher` instances own threads → `inbox = true` (`dispatch::declares_inbox`); `inbox.toml` v2 per-dashboard store → `inbox.json` v3 (`inbox::store`, imports v2 once); `message`/`reply` header → feed + `compose` (`inbox::feed`); dashboard spawns bridges → the daemon (`serve::host`).
- inbox-redesign.md: plan text presenting itself as reference ("Status: implemented through step 20 and 13b"); its *The TUI becomes a client* section predates dashboard-client.
- instance-phase.md: "The TUI … spawns a detached `devsandbox` subprocess" for `run`/`rebuild`/`start` → detached verb requests over the socket (`tui/mod.rs::spawn_verb` → `routed_verb_call`); landmarks `run/mod.rs:640`, `tui/mod.rs:610 run_suspended` dead.
- verbs-as-requests.md: dashboard's `rm`/`stop` "still run in its own process, suspended" → held verb requests (`tui/mod.rs::spawn_verb`); "five verbs", `services *` "come after" → `services.rebuild` is a verb (`serve/verbs.rs` `is_verb`).
- client-server.md: "the five verbs above plus `service rebuild`" correct; Status line correct. Only plan voice ("This doc moves…") is stale.
- before-machines.md: no Status line; all steps shipped (`exe::devsandbox`, `RunParams::tty`, `instances.watch`, `detach`).
- serve.md *Future work*: "The other verbs moving into the daemon" → no plan or code; `rename`/`done` are API methods, `gc`/`service ls` stay in-process.
- api.md *Hello*: "Send it first" → not enforced (`serve/daemon.rs` `handle`); compat notes for older daemons dropped in the new copy.
- port-forwarding.md *Flow control*: "Today `Mux::serve` writes each `Data` straight…" → flow control shipped (`devsbd/mux.rs` `MAX_HOST_STREAMS`, `Window`). Known limitations (v1) all re-verified true: unix-only, one bridge per forward, foreground `port` invisible to the daemon (`commands/port.rs::port` holds its own `Forward`s), `lsof` probe (`port.rs::parse_lsof_f`), no UDP.
- machines.md: limitations re-verified (BatchMode in `machines/ssh.rs::api_argv`; remote `code`/`port` refused, `tui/app/actions.rs`). No stale claim found.
- windows-support.md: "once the verbs run in the daemon the CLI has nothing to run them locally on Windows" → off unix the verbs run in-process (`architecture/daemon/platform-support.md`, `main.rs` `#[cfg(not(unix))]` arms).
- sandbox-helper.md: install failure → "bind-mount fallback" → only in no-helper builds (`run::relay_first`); "CLI upgrade rewrites the binary on next `run`/`start`" → also `port` and every daemon bridge (re)spawn (`Bridges::reconcile` → `ensure_recorded`); size budget < 500 KiB → ~650–720 KiB; Step 10 "TUI bridges off the UI thread" → the TUI keeps no bridges (`serve/host.rs` sole `spawn_worker` caller).
- helper-rpc.md: control socket `/run/devsandbox/ctl` → `/run/devsandbox/devsbd.ctl` (`daemon::CTL`); cites `src/devsbd/bridge.rs:525`.
- ssh-agent.md: "Future: Apple `container` … Blocked today" → the relay covers Apple (only mount mode is blocked); landmarks → `exec::exec_argv_with`, `exec::ssh_auth_sock_env`.
- tui.md: 3 tabs `1/2/3` → 4 incl. Inbox (`Tab::ALL`); error toast → help-bar status line (`App::status`); `Node::EmptyMarker(sandbox)` → `Node::Empty(usize)`; `r` prefills `run` → `rename` (`open_rename_or_run_prompt`); `code` URI via `hex_encode` in `tui/mod.rs` → `commands::vscode::launch`; the dashboard polls the runtime / loads config → it calls no runtime (`tui::daemon`, `tui::poll`).
- tui-terminal.md: `TermSession` `dirty` flag → fixed 30 ms redraw (`term.rs::spawn_reader`); `TermTabs` focused flag → `App::focus`.
- tui-prompt-spec.md: `COMMANDS` const → `prompt::commands()` from `SPECS`; `candidates_for` in `app.rs` → `tui/app/command_line.rs`.
- processes-stop-colors.md: prompt `stop` via suspended terminal, `s` runs `docker stop` → `stop` verb requests; processes via `docker top` → `instances.procs` (`tui/poll.rs`; daemon `exec ps`, `top` fallback); inspect via `docker::output_quiet` → `containers.inspect` (`tui/daemon.rs`).
- dashboard-client.md: terminal exe is `current_exe()` captured at start, upgrades break new tabs → re-resolved per spawn (`ExecCli::argv` → `exe::devsandbox()`); both the note and its known-limitation line obsolete.
- json-output.md: `data.rs:106`/`app.rs:17`, "re-exports `collect`" → `tui/data.rs` re-exports row types + `Snapshot` only; non-goal "no library bindings; the CLI is the API" → JSON-lines API + npm `connect()`.
- tui-selection.md: only *Problem* describes the pre-feature state; Steps 1–8 match code.
- runtimes.md, worktreeinclude.md, worktree-locking.md, inbox-cli.md, serve.md: no wrong claim found in the parts checked (`runtime::RUNTIME_ENV`, `backend_named`, `default_backend_name`, cacheFrom warning in `services.rs`; `copy_worktree_includes`, `link_shared_files`; `lock_worktree`, `unlock_worktree`, `gc_keeps_worktrees_env`; `idle::IDLE_TIMEOUT` 10 min, `daemon::DRAIN_TIMEOUT` 5 s, `LOCK_WAIT` 10 s).

## Notes

- **Supersession chains.** inbox-threads → inbox-redesign → `architecture/inbox/*` + `reference/inbox-cli.md` + ADRs 0026–0028. tui → dashboard-client (+ processes-stop-colors, tui-terminal, tui-selection, tui-prompt-spec) → `architecture/dashboard/*`. run-command → rebuild / folders-worktrees / worktree-locking / worktreeinclude → `instances/*`. client-server (umbrella) → instance-phase, verbs-as-requests, dashboard-client, before-machines, machines → `architecture/daemon/*`. devsandboxes → high-level-architecture → `architecture/overview.md` (step 3). api.md / inbox-cli.md → `reference/` (old copies: delete).
- **Docs whose only remaining value is rationale** (hence archive, not delete): automations, client-server, dashboard-client, inbox-redesign, instance-phase, verbs-as-requests, sandbox-helper, port-forwarding, machines, tui-selection. If the user prefers fewer files, the next to cut are instance-phase, verbs-as-requests, dashboard-client (their decisions are ADRs 0007, 0016/0017, 0031).
- **Before deleting entrypoint.md**: its "Differences with the devcontainers CLI" table exists nowhere else; fold it into `architecture/instances/images-and-features.md`.
- **Absolute GitHub URLs into docs/** (missed by a relative-link check, and published): `site/src/content/docs/host-daemon.mdx` → `docs/serve.md`, `docs/machines.md`, `docs/api.md`; `site/src/content/docs/dashboard.mdx` → `docs/automations-guide.md#inbox-threads`; `site/src/content/examples/babysit-pr.mdx` → `docs/automations-guide.md#sample-pr-babysit-dispatcher` (twice, one inside an agent prompt). Keeping automations-guide.md in place avoids breaking those.
- **Published package text**: `npm/devsandboxes/index.d.ts`, `index.js`, `README.md` cite `docs/api.md`; sweep them to `docs/reference/api.md`.
- **Decisions' `Source:` lines** cite old docs as plain text (not links), incl. deleted ones (run-command, devsandboxes, high-level-architecture…). The link test won't catch them; either leave as history or add the commit sha.
- **CHANGELOG.md** mentions api/serve/machines/json-output/worktree-locking/automations-guide docs in released entries; rewriting those is optional (release bodies already published).
- **Heavy src comment sweeps**: machines (25 src files), automations (22 + 4 in `devsbd/src/`), inbox-threads (16, though superseded: repoint to `architecture/inbox/*`, not to the archived redesign), api (15), inbox-redesign (15), instance-phase (13), sandbox-helper (10 + `build.rs`).
- **automations-guide.md** is the one user guide in `docs/`; neither archive nor architecture. Keep it at its path (site links), fix its autostart-trigger line, and consider moving it to `site/` later. The inbox-automations lead reached the same verdict.
- Surprising: devcontainer-features.md has a corrupted, self-duplicated block since 002dda3; processes-stop-colors.md has zero inbound refs; `devsbd/src/*` module docs cite `docs/automations.md` (archive target), so the sweep must repoint them to `architecture/automations/*` / `architecture/helper/*`.
