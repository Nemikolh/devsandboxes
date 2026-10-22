# Claude Code plugin

Status: proposed
Researched against the Claude Code docs as of 2026-10; nothing is built (no
`claude-plugin/`, no `mcp` or hook subcommand in `src/main.rs`).

Today an agent drives devsandbox through the CLI as described by
[devsandbox-cli](../../../skills/devsandbox-cli/SKILL.md): `run --json`,
`exec <name> …`, `logs`, `rm`. Nothing stops it from running a command on
the host instead, and the dashboard is a human's view, not the agent's. The
idea: a Claude Code plugin under which everything Claude Code does to code,
shell commands and file reads and edits alike, happens in a sandbox
instance, with in-process tools for setting sandboxes up that call
devsandbox under the hood. Claude Code itself stays on the user's machine;
the instance may be on it or on a [machine](../../architecture/daemon/machines.md).

Two facts shape everything below:

- The working tree lives on the host. An instance mounts its host folder
  (the base checkout, or `<config dir>/.worktrees/<instance_id>/` for a
  repeat instance) at `workspaceFolder`, `/workspaces/<basename>` by
  default (`materialize` in `src/commands/run/mod.rs`); the base repo's
  `.git` is mounted at its identical host path (`git_companion_mount`,
  [worktrees](../../architecture/instances/worktrees.md)). `RunRecord`
  and `InstanceRow` (`src/snapshot.rs`) carry both sides: `folder` (host)
  and `workspace` (container).
- Claude Code's own file tools (Read, Edit, MultiEdit, Write, NotebookEdit,
  Grep, Glob) run in the Claude Code process on the host; hooks and stdio
  MCP servers do too, with the user's full access
  ([sandboxing](https://code.claude.com/docs/en/sandboxing), *What runs
  outside the sandbox*). So the plugin never lets them run: pointed at the
  host they read whatever folder the agent guesses (the base checkout
  instead of the worktree, `~/.ssh`), and for an instance on a machine the
  tree isn't on this host at all.

## Goals

- Every shell command Claude Code runs (Bash, and any other
  command-running tool) runs in the instance owning the session, or is
  refused. Fail closed.
- Claude can list sandboxes, create, stop, rebuild and remove instances,
  read logs and forward ports through typed tools, without the CLI's
  TTY prompts.
- No host file access: Claude Code's file tools are denied, and reading,
  editing and searching files are tools that run in the instance.
- The same session drives a local instance or one on a machine, with no
  difference to Claude: every call goes through devsandbox's routing
  (`exec` and the daemon's `machine` param).
- One Claude Code session (or worktree-isolated subagent) = one devsandbox
  instance.
- The plugin is thin: logic lives in the `devsandbox` binary, so plugin
  and binary can't drift apart in behavior.

## Non-goals

- Replacing the dashboard; it stays the human's view of what agents do.
- Running Claude Code itself inside the container (the
  [dev container](https://code.claude.com/docs/en/devcontainer) route):
  that's possible today with no plugin, and loses the host-side
  orchestration this plan is about.
- Confining Claude Code's own process (WebFetch, Read of `~/.ssh`): that's
  the sandbox runtime's job
  ([sandbox environments](https://code.claude.com/docs/en/sandbox-environments)).

## Which instance a session uses

The session's cwd can't decide: with the instance on a machine, nothing
of it is under the host cwd. The binding is explicit: the `run` and `use`
tools bind the hook input's `session_id` (plus `agent_id` for a subagent)
to a qualified instance name (`my-server/web-2`, `@/web-2`), kept in a
small plugin state file next to `state.toml` (locked, like
`machines.toml`). An unbound session gets every command and file tool
denied with a reason that tells Claude to call `run` or `use`. A stale
binding (instance removed) is dropped on the next call, same reason.

`WorktreeCreate` replaces Claude Code's own git worktree creation for
`--worktree`, `isolation: "worktree"` subagents and background sessions;
a command hook prints the worktree path on stdout, and failure fails the
creation ([hooks](https://code.claude.com/docs/en/hooks), *WorktreeCreate*).
The plugin's hook runs `devsandbox run --json <sandbox>` and binds the
new instance to the subagent; `WorktreeRemove` runs `devsandbox rm`. So
subagents with worktree isolation each get their own container. The path
it must print is meaningless here (no host file tool ever reads it): an
empty host dir per instance would do, if Claude Code accepts one (open
question).

## Forcing commands into the sandbox

### A. `PreToolUse` rewrite (recommended)

A `PreToolUse` hook may return `updatedInput`, which replaces the tool's
whole input before it runs, together with `permissionDecision`
`allow` | `deny` | `ask` | `defer`; permission rules are evaluated against
the rewritten input, and deny > defer > ask > allow across hooks
([hooks](https://code.claude.com/docs/en/hooks), *PreToolUse decision
control*). Plugin hooks fire inside subagents too, with `agent_id` set.

The hook (matcher `*`, `onFailure: "block"`) rewrites a Bash call's
`command` to

```
devsandbox exec --no-agent <instance> -- bash -lc '<command>'
```

keeping `description`, `timeout` and `run_in_background`, and answers
`allow` (the container is the boundary; `ask` per `userConfig`). Every
other tool that runs commands (`PowerShell`, `Monitor`, any unknown one)
is denied, and so is every host file tool (*Files* below). The plugin's
MCP tools and the rest pass. `devsandbox exec` already routes a
qualified name to its machine (over ssh), so the rewrite is the same for
local and remote instances.

- Pros: Bash keeps everything Claude knows (background runs, timeouts,
  output handling); no new tool to learn; one hook covers subagents.
- Fail-open by default: a hook that crashes or prints bad JSON is a
  non-blocking error and the action goes ahead, unless the hook sets
  `onFailure: "block"` (Claude Code v2.1.295+). The plugin requires that
  version; the hook also exits 2 on anything it can't parse.
- User deny rules like `Bash(git push *)` no longer match: they're checked
  against `devsandbox exec …`. The hook can re-check the original command
  against the user's Bash deny rules, but it has to read settings itself.
- Shell state doesn't carry: a `cd` or `export` in one call is gone in
  the next (each call is a fresh `exec`). Claude Code's tracked cwd is a
  host path that means nothing in the container, so the hook ignores it:
  every command starts in the instance's `workspace`.
- On a machine every call is an ssh round trip; the link's reuse
  (`serve::machines::service`) decides how bad that is. Measure in the
  spike.
- Uncertain: whether Claude Code wraps `command` before `PreToolUse` sees
  it (cwd tracking, env snapshot); the docs show the bare command in the
  hook input. Needs a spike.
- Uncertain: a killed or timed-out `devsandbox exec` (no `-t`) may leave
  the process running in the container. The helper's tracked runs
  (`devsbd run`, [in-container](../../architecture/helper/in-container.md))
  could own long commands instead.

### B. Deny Bash, MCP `exec` tool

Strip Bash and expose `exec` as an MCP tool. Plugins can't ship
permission rules (a plugin's settings only honor `agent` and
`subagentStatusLine`,
[plugins reference](https://code.claude.com/docs/en/plugins-reference)),
so "no Bash" means either the hook denying it or the plugin's
`settings.json` setting `agent` to a plugin agent whose `tools` omits Bash
(`agent` starts every session as that agent, its prompt replacing the
default; [settings](https://code.claude.com/docs/en/settings-reference),
[subagents](https://code.claude.com/docs/en/sub-agents)). Plugin agents
ignore `hooks`, `mcpServers` and `permissionMode`.

- Pros: no rewriting, nothing to escape.
- Cons: MCP output is capped (warning past 10k tokens, 25k by default) and
  a stdio call idles out after 30 min without progress
  ([MCP](https://code.claude.com/docs/en/mcp)); no background runs;
  Claude fights a missing Bash; the `agent` route replaces the system
  prompt for every session the plugin is enabled in.

### C. `CLAUDE_CODE_SHELL_PREFIX`

A wrapper around every shell command Claude Code spawns: Bash calls, but
also hook commands and stdio MCP server startup
([env vars](https://code.claude.com/docs/en/env-vars)). A user env var, not
something a plugin sets, and the wrapper would have to tell Bash calls from
its own hooks. Rejected; noted as the escape hatch for users without
plugins.

**Recommendation**: A, with B's deny as its fallback (no instance, any
command tool it can't rewrite). Revisit the `agent` route only if A's
spike fails.

### Claude Code's built-in sandbox

Complementary, not a substitute: it wraps shell commands only, and docker
is incompatible with it (it needs `excludedCommands`)
([sandboxing](https://code.claude.com/docs/en/sandboxing)). Users with it
on add `excludedCommands: ["devsandbox exec *"]`; the plugin can't set it.
With A every command already runs in a container, so the built-in sandbox
adds nothing for Bash.

## Files

Read, Edit, MultiEdit, Write, NotebookEdit, Grep and Glob are always
denied, whatever the path: the deny reason names the plugin's file tools.
Those run in the bound instance, through the helper, so there's one view
of the tree (the container's), one set of paths (container paths, the same
ones Bash output shows), and nothing to translate. They work unchanged on
a machine's instance. The host-side copy of a local instance's tree is
never read by Claude, so a confused agent can't edit the base checkout
while its instance sits on a worktree.

The tools mirror Claude Code's built-ins closely enough that its habits
carry over: `read` (path, offset, limit; line-numbered), `edit`
(exact-string replace, unique unless `replace_all`), `multi_edit`,
`write`, `grep`, `glob`. Each is a `devsbd` request in the container
(new ops beside `devsbd run`, [helper](../../architecture/helper/in-container.md)),
not `exec` of shell tools the image may lack. Edit's read-before-write
guard is the server's to keep (it tracks what was read per session).

## Tool surface

A stdio MCP server, `devsandbox mcp` (hidden subcommand), declared in the
plugin's `.mcp.json`. Plugin MCP tools are named
`mcp__plugin_<plugin>_<server>__<tool>`
([hooks](https://code.claude.com/docs/en/hooks), *Match MCP tools*). It's
a daemon client ([client-server](../../architecture/daemon/client-server.md),
`serve::client::connect`), with the CLI's in-process fallback. The config
root comes from the plugin's `userConfig` (`directory`), passed as `-C`.

| tool | maps to | notes |
|---|---|---|
| `sandboxes` | `sandboxes.list` | what can be run |
| `instances` | `instances.list` | name (qualified), status, drift, `workspace`, which one this session is bound to |
| `run` | `run` verb, `json: true` | `sandbox`, `name?`, `branch?`; verb `progress` → MCP progress (keeps the idle timer alive); answers the run record |
| `start` / `stop` / `rebuild` | verbs | `instance`; `rebuild` without `all` |
| `rm` | `rm` verb, `ask: true` | `confirm` comes back as a question for the user; no `force` |
| `use` | binding | `instance` (qualified): bind this session to an existing instance |
| `logs` | `containers.logs`, `instances.buildLog` | `build?`, `lines?` |
| `port` | `forwards.add` | daemon-owned; answers the host port |
| `read` / `edit` / `multi_edit` / `write` / `grep` / `glob` | helper file ops in the bound instance | *Files* |

Every tool takes qualified names and passes `machine` through, so a
machine's sandboxes are listed, run and bound like local ones. Not
exposed: `exec` (Bash is the exec, A), `inspect` (it carries the
container env, secrets included), `gc`, `machine`, `services rebuild`,
`inbox`, `vscode`. MCP calls prompt per Claude Code's permission rules; the
plugin's skill says which are safe to allow
(`mcp__plugin_devsandbox_devsandbox__instances` and friends).

Verb params and errors: [api.md](../../reference/api.md) *Verbs*. `env`
is the MCP server's own (inherited from Claude Code), as the CLI sends its
shell's.

## Secrets and the ssh-agent

- `devsandbox exec` holds an agent relay while it runs (`AgentRelay` in
  `src/commands/exec.rs`): processes in the container can sign with the
  user's keys. Unsetting `SSH_AUTH_SOCK` doesn't stop it (the daemon falls
  back to reported agents and relay sockets, `bridge::host_agent`). Agent
  sessions get `exec --no-agent` by default, `userConfig.ssh_agent` turns
  it on ([ssh-agent](../../architecture/helper/ssh-agent.md)).
- An exec'd command gets the instance's recorded `remoteEnv` and extra
  env (`exec_argv_with`), never Claude Code's env.
- The hook and the MCP server run with the user's full access, as every
  hook and MCP server does: they're the trusted base, so both are the
  `devsandbox` binary, not scripts.
- Credentials in containers stay a config matter
  ([cli-proxy](cli-proxy.md)).

## Packaging

```
.claude-plugin/marketplace.json   name "devsandboxes", one entry, source "./claude-plugin"
claude-plugin/
  .claude-plugin/plugin.json      name "devsandbox", version, userConfig
  hooks/hooks.json                PreToolUse, SessionStart, WorktreeCreate/Remove → devsandbox claude-hook <event>
  .mcp.json                       devsandbox mcp -C ${user_config.config_root}
  skills/devsandbox/SKILL.md      how to work under the plugin
```

Layout and fields per the
[plugins reference](https://code.claude.com/docs/en/plugins-reference);
users run `/plugin marketplace add Nemikolh/devsandboxes` then
`/plugin install devsandbox@devsandboxes`
([marketplaces](https://code.claude.com/docs/en/plugin-marketplaces)).
`devsandbox` must be on `PATH` (cargo, npm); the `SessionStart` hook
checks it and the binary's version and says so in `additionalContext`.
`plugin.json`'s `version` pins users until it changes, so `release.sh`
bumps it with the crate and npm pins. Component paths can't escape the
plugin root, so `skills/devsandbox-cli` can't be referenced from there.
Besides how to work under the plugin, the skill carries instance facts
an agent would otherwise rediscover the hard way, e.g. the browser note
from [identity](identity.md) (*Browser wrapper*): with
`DEVSANDBOX_BROWSER_SANDBOX=1`, launch Playwright with
`chromiumSandbox: true`, plus `executablePath: $DEVSANDBOX_CHROME` when
that's set.

## Steps

1. **`exec --no-agent`**: the hook needs it; `exec_argv_with` skips
   `AgentRelay`, on machines too.
2. **Helper file ops**: `read`/`edit`/`write`/`grep`/`glob` as `devsbd`
   requests, pure over a root dir and unit-tested in `devsbd/`, reached
   from the host like `devsbd run` (local and over a machine's link).
3. **Session bindings**: the state file, `run`/`use` binding, stale
   pruning; pure model plus a locked store.
4. **`devsandbox claude-hook <event>`** (hidden): `PreToolUse` as a pure
   fn over the hook input and the binding (rewrite, deny, ask, quoting),
   unit-tested.
5. **`devsandbox mcp`** (hidden): stdio MCP over the daemon client, the
   table above; tests with an injected API like `serve::api`'s.
6. **`claude-plugin/` + marketplace**: manifest, hooks, `.mcp.json`,
   skill; `claude plugin validate` in CI.
7. **`WorktreeCreate`/`WorktreeRemove`**: `run --json` + bind / `rm`.
8. **Release and docs**: `release.sh` bumps `plugin.json`; an
   `architecture/` doc for the plugin; a site page; CHANGELOG.

## Open questions

- Does `PreToolUse` see Claude Code's own wrapping of the Bash command,
  and does a rewritten command keep cwd tracking sane? (Spike before 4.)
- Auto-`allow` rewritten commands, or `ask`? Lean `allow`, deny rules
  re-checked by the hook against the original command.
- Does Claude Code use its own file tools well enough without them, or
  does it fall back to `cat`/`sed` in Bash? Instructions in the plugin's
  skill and the deny reasons, measured in the spike.
- Does `WorktreeCreate` accept a path that isn't a git worktree (an
  empty per-instance dir)? Else skip it and bind subagents through `run`.
- Latency on a machine: one ssh round trip per tool call acceptable, or
  does the link need a persistent exec channel?
- Which sandbox does `WorktreeCreate` run when several share the repo's
  folder? A `userConfig` default, else the only one, else fail.
- Orphaned container processes after a timed-out `exec`: tracked runs, or
  accept it?
- `bash -lc` vs the `SHELL_FALLBACK_CMD` probe for images without bash.
- Plugin skill: a copy of `skills/devsandbox-cli` trimmed for the plugin,
  or a symlink (does plugin install keep symlinks?).
- Should `rm` be callable by Claude at all, or only via `WorktreeRemove`?
