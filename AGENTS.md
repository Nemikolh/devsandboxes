# devsandbox

Rust CLI (`devsandbox`) that manages devcontainer-style sandboxes on docker /
podman / Apple container. Each sandbox config can spawn many _instances_
(containers named `devsandbox-<instance>`); repeat instances of one repo get
git worktrees so working trees are never shared.

Start with `docs/README.md` (reading order and map) and
`docs/architecture/overview.md`; `docs/status.md` has what doesn't work yet.
Per-module detail lives in `docs/architecture/<area>/` and in each module's
`//!` header, not here.

## Layout

- `src/main.rs` — clap CLI; no subcommand on a TTY opens the TUI dashboard.
  `serve`, `--machine` and the dashboard are unix-only.
- `src/commands/` — one file (or dir) per verb; `run/` is split by concern.
  Shared instance-name resolution: `commands::resolve_instance`.
- `src/config.rs` — the TOML schema (`[template.*]`, `[sandbox.*]`,
  `[services.*]`); `src/state.rs` — global instance state (`state.toml`).
- `src/serve/` — the per-user host daemon and its API
  (`docs/architecture/daemon/`, `docs/reference/api.md`); `machines/` and
  `fleet/` are remote hosts and member identity.
- `src/tui/` — ratatui dashboard, a daemon client for its views (Inbox
  store, settings and prompt history stay local); `app/` is an I/O-free,
  unit-tested state machine (`docs/architecture/dashboard/`).
- `src/runtime/` — the docker/podman/Apple container backend abstraction.
- `src/inbox/` — the shared Inbox store and its ops (`docs/architecture/inbox/`).
- `src/credentials/`, `src/broker/` — the credential broker's data and its
  HTTP core (`docs/architecture/credentials/`).
- `devsbd/` — the static in-container helper (workspace member), built by
  `scripts/build-devsbd.sh` and embedded by `build.rs`; `src/devsbd/` is the
  host side, some files shared with the helper via `#[path]`
  (`docs/architecture/helper/`). `build.rs` never builds the helper: after
  touching `devsbd/` or a shared file, rerun `scripts/build-devsbd.sh` or
  helper tests run a stale binary.
- `src/ask/` — interactive stderr prompts (`docs/architecture/cli-prompts.md`).
- `site/` — Astro + MDX docs site, a standalone pnpm project (see
  `site/README.md`).

## Rules that are easy to miss

- The host runs git only through the `host_git*` constructors
  (`src/commands/run/git.rs`), and fetches `origin` only through
  `fetch_trusted_origin` (`run/origin.rs`, pinned URL). See
  `docs/decisions/0003-host-git-never-runs-repo-controlled-code.md`.
- `src/key_guard.rs` refuses any container bind or build path covering the
  member key or credential files: every new runtime `run`/`build` spawn
  runs the key guard's `check_argv` on its final argv.
- `extends` deep-merges: tables recurse, **arrays concatenate**. Config drift
  is decided in one place, `commands::drift_decision`.
- A config field, merge rule or `${…}` variable change also updates
  `skills/config-toml-spec/SKILL.md` (the site renders it as `/docs/config`).
- A verb or flag change also updates `skills/devsandbox-cli/SKILL.md`
  (installed for users' agents).
- A wire struct change in `src/serve/` also updates the hand-written
  `npm/devsandboxes/index.d.ts` (checked by `serve/dts.rs`).
- The dashboard's `:` command grammar is the `SPECS` table in
  `src/tui/spec.rs`; parsing and completion both derive from it.
- `--in-process` (hidden, global) never lazy-starts a daemon nor fans out to
  machines; the daemon's subprocesses pass it.
- `run --json` stdout is one JSON document: child output goes to stderr
  (`src/json_stdout.rs`).

## Conventions & checks

- Docs (`docs/README.md` has the layout): `architecture/` and `reference/`
  describe the code as it is, present tense, no status lines, no
  limitations sections, code cited by path + symbol (never line numbers).
  A change to behavior updates the area's doc in the same commit, and
  `docs/status.md` (the only place for in progress / planned / parked /
  known limitations) when it adds, fixes or parks something. Decisions
  that still hold are ADRs in `decisions/` (a new one supersedes, old ones
  aren't rewritten). Ideas not agreed yet live in
  `plans/explore/<topic>.md` and move to `plans/active/<topic>.md` once
  agreed (deleted if dropped); once shipped, an active plan's content moves into the living docs and the plan goes to
  `plans/archive/` (banner on top), deleted once outdated. Skip
  `plans/archive/` unless asked about history. Names are topics, no
  `plan-` prefix or `-plan` suffix. `docs_links` (`tests/docs_links.rs`)
  checks every `docs/…` path cited in code and every relative link in docs.
- Doc comments explain _why_; keep them current when moving logic.
- User-facing messages never cite plans, steps, ADRs or docs paths; an
  unbuilt feature says `<feature>: not supported yet! open an issue in
  https://github.com/Nemikolh/devsandboxes` (`NOT_SUPPORTED_YET`).
- Comments say why or state a contract, never what the code does. Before
  committing a change that adds or edits comments, run the
  `simplify-comments` skill (`.agents/skills/simplify-comments/SKILL.md`)
  over it.
- Tests live in `#[cfg(test)]` modules per file; most logic is factored to be
  testable without a container runtime (injectable runners, `sh` as the
  stand-in program).
- Tests that need a container runtime or the embedded helper use
  `#[test_utils::docker_test]` / `#[test_utils::docker_test(helper)]` /
  `#[test_utils::helper_test]` (proc macros in `test-utils/`, a path-only,
  never-published dev-dependency) and return `Result<(), E>`. A missing
  requirement or `Err(why)` skips locally but **fails on CI** (`CI` env var);
  runtime side in `src/test_support.rs` (`gated`, `with_cleanup`).
  "Docker" tests run against the runtime `DEVSANDBOX_RUNTIME` selects (CI's
  `test-podman` job runs them under rootless podman): drive it with
  `test_support::runtime_cmd()` / `runtime::backend()`, never a literal
  `docker`, and mark a docker-only mechanism `docker_test(docker_only)`
  (not applicable elsewhere, so it passes there). A test child with its
  own `XDG_*` dirs calls `test_support::share_runtime_dirs`.
- Checks: `cargo test --workspace` (covers the `devsbd/` helper crate too;
  plain `cargo test` only runs the root package). `cargo clippy --workspace
  --all-targets` is available but not clean yet, so check only that your
  change adds no warnings. The pinned toolchain has no rustfmt: match the
  surrounding formatting by hand. Site: `cd site && pnpm check && pnpm build`;
  `pnpm shot /docs/x` screenshots a built page to `/tmp/site-shots/`.
- Commit each finished, checked change without being asked, staging only
  your own hunks. Subagents never commit unless their brief says to.
- Commits follow Conventional Commits, all lowercase:
  `<type>(<what>): <description>` where `<type>` is `feat`, `fix`, `chore`,
  `docs`, `test`, `refactor`, … and `<what>` is the module or concept touched
  (`tui`, `rm` or any other command, `docker`, `apple`, `config`, …).
- Releases: follow the `release` skill (`.agents/skills/release/SKILL.md`).
  `CHANGELOG.md`'s `## Unreleased` section becomes the GitHub release body.
- Pre-1.0 with very few users: treat everything as unreleased. Break wire
  protocols, file formats, config and CLI freely; no backwards-compat
  shims, migration paths, mixed-version fallbacks or "behavior change"
  changelog notes unless asked. Both ends of a link (helper, daemon,
  machines) are assumed to run the same version.
