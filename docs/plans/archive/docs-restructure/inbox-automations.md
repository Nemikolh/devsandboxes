> Archived 2026-10-16. Historical, may be wrong; do not use as a reference.
> Current: [README.md](../../../README.md)

# Inbox + automations lead fragment

Written by the lead directly (the spawn pool was full for the whole run;
no child subagents). Every claim checked against the code named in each
doc's `Code:` line.

## Docs written

- docs/architecture/inbox/model.md: thread kinds, ids/owner, sink decision, user `Op`s, events, caps + retention, archive, sanitize, store (`inbox.json` v3, lock, import), `ops` layer
- docs/architecture/inbox/threads-and-feed.md: `inbox = true` gate, `thread put` / `send` / `withdraw` / `rm` schemas and bounds, feed rules (`push` collapse, `cap`), event control ops (`events`, `events-ack`, `events-follow`, `thread-ls`)
- docs/architecture/inbox/forms.md: form schema, follow-ups (`under`/`when`), `FormRecord` lifecycle + `resend` table, draft/submit checks, submit event
- docs/architecture/inbox/views-and-wire.md: `View`s, wire views, `Addr`/`resolve`/`user_target`, `decide_*` table, which client serves what, `clock.rs`
- docs/architecture/automations/autostart.md: modes, the once-per-boot pass, triggers, boot id, `"runtime"` restart policy + boot hook
- docs/architecture/automations/dispatchers.md: config, authorization, children, `ensure` table, ops table, subprocess/run-op execution, state-write lock, branch holders, done children
- docs/reference/inbox-cli.md: exhaustive `devsandbox inbox` (verified against `InboxCommand` and `src/commands/inbox.rs`)

Removed `docs/architecture/{inbox,automations}/.gitkeep`. All docs end with
`Limitations: [status.md](../../status.md#inbox|#automations)` (reference:
`../status.md#inbox`): status.md needs `### inbox` and `### automations`
headings.

## Known limitations (verified true today)

- `inbox.thread.done`/`reopen` over the API and `devsandbox inbox done|reopen` change only the thread; the child instance's `done` flag flips only from the dashboard → [dispatchers.md](../../../architecture/automations/dispatchers.md#done-children); `wire::decide_done` yields only `Op::MarkDone`, `tui/app/inbox.rs` `mark_thread_done` → `mark_thread_child`
- `devsandbox inbox` has no draft-save, notify dismiss or mark-read commands (API-only: `inbox.form.saveDraft`, `inbox.notify.*`) → [inbox-cli.md](../../../reference/inbox-cli.md); `InboxCommand` variants
- The Inbox doesn't span machines: `devsandbox inbox --machine` is refused, each host has its own store → [inbox-cli.md](../../../reference/inbox-cli.md#how-it-runs); `machines/cli.rs` `MachineUse::Refused("inbox")`
- An `inbox.json` written by a newer schema version is unreadable by an older build (no downgrade) → [model.md](../../../architecture/inbox/model.md#store); `Inbox::from_json` rejects any version ≠ `VERSION`
- `thread ls` fails ("too large to list") past ~512 KiB of JSON → [threads-and-feed.md](../../../architecture/inbox/threads-and-feed.md#events); `dispatch::thread_ls`, `EVENTS_BODY_CAP`
- Autostart never runs off Linux/macOS (no boot id) → [autostart.md](../../../architecture/automations/autostart.md#boot-id); `read_boot_id` returns `None`
- `autostart = "runtime"` on podman restarts at boot only with `podman-restart.service`, which devsandbox never installs → [autostart.md](../../../architecture/automations/autostart.md#runtime-restart-policy); `restart_decision` warning
- `autostart = "runtime"` on Apple `container` behaves as `true` (no restart policy) → same; `AppleContainer::supports_restart_policy` false
- After a runtime restart, host-side bridges, forwards, notify delivery and control resume only once the daemon runs (`serve install` or the next command) → [autostart.md](../../../architecture/automations/autostart.md#runtime-boot-hook); bridges live in `serve::host`
- On `start` of a `"runtime"` container, the boot hook fires before the host rewrites the boot file, so that one start runs the previously recorded `postStartCommand`/env → [autostart.md](../../../architecture/automations/autostart.md#runtime-boot-hook); `devsbd::sync_boot` doc ("Known gap"), `run::boot_spec_for` doc
- `ensure --env` on an existing child changes only the saved env applied to `devsandbox exec`; the container's own env (PID 1, running processes) changes only on rebuild; `ensure --branch` is ignored for existing children → [dispatchers.md](../../../architecture/automations/dispatchers.md#ensure); `ensure_action` doc, `replaced_env`
- A dispatcher's lifecycle ops are serialized host-wide: one long `ensure` (up to 30 min) blocks every other dispatcher's `ensure`/`stop`/`rm`/`done` → [dispatchers.md](../../../architecture/automations/dispatchers.md#serialization); `bridge::dispatch_control` `LOCK`, `DEVSANDBOX_TIMEOUT`

## Status entries

- Planned/not started: **cli-proxy** (credential-isolated `gh`/`sentry-cli` via the `tools` service): nothing exists (no `cli-proxy/` member, no shim/policy code; only the reserved `proto::channel::HTTP_PROXY` id, which is for API proxying). Plans lead owns it.
- Shipped (archive candidates, true content now in living docs): `docs/dispatcher-branch-holders.md` ("Status: implemented"; `branches` op, `Holder`), `docs/inbox-redesign.md` (all steps + 13b done), `docs/automations.md` (all steps [x]; step 16 "no zombie from the boot hook" is "proposed, not decided": plans lead may want to check it), `docs/inbox-cli.md` (→ reference/inbox-cli.md). `docs/inbox-threads.md`: superseded by inbox-redesign (delete candidate). `docs/automations-guide.md` is a user guide, not a plan: keep or move to the site, not archive.

## Stale claims in old docs

- skills/config-toml-spec/SKILL.md `autostart` row: triggers are "the host daemon's start, the first `run` / `start` or TUI launch" → `rebuild` triggers it too, and the dashboard runs no pass of its own (it lazily starts the daemon, which does); evidence `main.rs` `Command::Rebuild` arm calls `autostart::autostart`, `tui/mod.rs` "No autostart pass of our own"
- docs/automations.md § `true` and docs/automations-guide.md § autostart: "the first TUI load / dashboard launch" triggers the pass, `run`/`start` only → same truth as above (mechanism is the daemon; `rebuild` missing)
- docs/automations.md § Control API: "`--branch`/`--env` apply at creation only" → `--env` on an existing child replaces its saved `extra_env` (`dispatch::replaced_env`, `Executor::save_env`); only `--branch` is creation-only
- docs/automations.md § `"runtime"`, docs/dispatcher-branch-holders.md, docs/inbox-redesign.md § Why: cite `file:line` locations (`start.rs:61`, `devsbd.rs:108`, `dispatch.rs:557`, `dispatch.rs:404-430`) that no longer point at what they describe
- docs/inbox-redesign.md header: "Status: implemented through step 20 and 13b" with dated decisions → it's a plan doc presenting itself as reference; current truth in architecture/inbox/
- Code comments (not docs, report only, out of scope to edit): `autostart.rs` `autostart` doc says "Called after `run`/`start` and on TUI load" (dashboard no longer calls it; `rebuild` does); `dispatch.rs` module doc and `Subprocess` comments say the handler runs "on the TUI's bridge worker" (it runs on the host daemon's bridges, `devsbd::bridge::dispatch_control`); `inbox/view.rs` and `inbox/thread.rs` module docs point to superseded `docs/inbox-threads.md` (link sweep will repoint)
- docs/inbox-cli.md: verified accurate; superseded by reference/inbox-cli.md (adds `ls` columns, error codes per command, `closed-form`, `--machine` refusal)

## Decision candidates

- Owning Inbox threads is its own capability (`inbox = true`), separate from `dispatcher`; a child may own threads; why: least privilege, children report on their own work; source inbox-redesign.md § Ownership; still true because `dispatch::authorize` gates inbox ops on `is_inbox_owner` and child ops on `dispatcher`, and `is_inbox_op` lets a child through
- Only the owner reads a thread's events, and events never say which client made them; why: no cross-sandbox leak of the user's answers, every client equal; source inbox-redesign.md § Principles 4 + § Ownership; still true because `Inbox::events_for`/`ack` filter by owner and `Event` has no client field (the client lives only on feed items)
- Threads are keyed by `(owner instance_id, key)`, never by instance name, and `rm` archives instead of deleting; why: renames/rebuilds keep threads, ids are never reused; source inbox-redesign.md § Ownership; still true because `Inbox::put`/`ops::thread_id` match `owner`, `ops::archive` from `rm::archive_inbox`
- Put and send are idempotent by key/id so owners re-assert every pass and the Inbox is rebuildable from owner state; an unchanged put doesn't even write; source inbox-redesign.md § Principles 5; still true because `PutOutcome::Unchanged`, `feed::Sent::Unchanged`, `store::update_at` writes only on content change
- The helper only syntax-checks thread JSON; every schema check runs on the host and rejections come back as an `error` record; why: one schema owner, newer owner on older host fails loudly; source inbox-threads.md § Decisions; still true because `devsbd/src/json.rs` + `inbox::decide`/`refused`, `deny_unknown_fields` on `ThreadPut`/`message::Wire`
- Host actions are a fixed verb table, never an arbitrary command, and the API refuses host-only actions; why: a container can't make the host run things; source inbox-threads.md § Actions; still true because `thread::HostVerb` (6 variants) and `wire::decide_act`
- One code path per Inbox mutation (`inbox::ops`, user checks in `wire::decide_*` run under the store lock), shared by daemon, CLI fallback and dashboard fallback; why: identical events whatever the client or daemon availability; source inbox-redesign.md § One API; still true because `ops::apply_if` in `serve::api::mutate` and `commands::inbox::apply_local`
- A submitted form is frozen against re-sends; why: user answers stay attached to the questions they answered and re-asserting the original message stays a no-op; source inbox-redesign.md § Forms; still true because `form::resend` Submitted arm
- Autostart records the boot before acting, and only when there's something to do and the runtime is reachable; why: a failing start can't retrigger forever, and a late `autostart = true` or slow Docker Desktop still gets this boot's pass; source automations.md § `true`; still true because `autostart::autostart` order (empty → return unrecorded; `backend().list` error → return unrecorded; `State::update` claim before `actions`)
- `"runtime"` mode uses `unless-stopped` and an in-container boot hook rather than host exec; why: a user-stopped container stays stopped, and `postStartCommand` must run with no host attached; source automations.md § `"runtime"`; still true because `RUNTIME_RESTART`, `boot_spec_for`, `post_start_runner`
- Dispatcher requests are authorized against the config as it is now, never cached; children can't be dispatchers; `max-instances` defaults to 10 and counts stopped children; why: no recursion, bounded fleet, config edits take effect immediately; source automations.md § Dispatchers + Step 19; still true because `dispatch::authorize`, `ensure_action`, `Dispatcher::DEFAULT_MAX_INSTANCES`
- Dispatcher lifecycle ops run as `devsandbox --in-process` subprocesses, not in-process calls; why: those commands stream to inherited stdio, and `--in-process` stops a nested lazy daemon start; source dispatch.rs module doc / automations.md Step 8; still true because `Subprocess::devsandbox`
- Dispatcher env/branch values are data: a denylist of loader/shell env names, branches never pattern-substituted; why: a child's env reaches root execs on its next start; source automations.md Step 18/19; still true because `control::denied_env`, `run::worktree_branch` (`Some(branch) if dispatched`)
- `branches` never returns host paths; source dispatcher-branch-holders.md; still true because `BranchRow` has no path field

## Uncovered

- `devsbd` in-container verbs (outbox, coalescing, `events --follow` client, runs store): helper lead's `in-container.md`/`protocol.md`; linked, not repeated
- Bridge transport of notify/control (apply-then-ack, `MAX_HANDLERS`, follow write timeouts): helper lead's `bridges.md`
- Exact API JSON for Inbox methods: daemon lead's `reference/api.md`; linked
- Dashboard Inbox UI (keys, view strip, unread dedupe, popups' rate limit `desktop::RateLimit`): dashboard lead
- automations-guide.md's user patterns (resident vs dispatcher, scripts): user-facing, belongs to the site, not contributor docs
- inbox-redesign.md's daemon/transport sections (`devsandbox serve`, `api --stdio`): daemon lead
- Form rendering details (`QuestionView` shapes): api.md § Forms
