# 0003: Host git never runs repo-controlled code

Status: accepted

## Context

Containers can write the repo's `.git`: worktree instances bind-mount the
base repo's `.git`. A sandboxed agent could plant a hook, a
`core.fsmonitor` command, an `ext::` remote or an alternates file, and the
next host-side git call (worktree add, branch listing for a dispatcher)
would execute it on the host, escaping the sandbox.

## Decision

Every host git invocation goes through one wrapper,
`commands/run/git.rs` (`host_git` → `git_command`, guarded by
`check_repo`):

- hooks and `core.fsmonitor` are disabled on the command line;
- the repo's local config files are parsed by devsandbox itself (never via
  `git config`, which follows includes) and checked against an allowlist of
  keys that can't run commands;
- any other key, an `ext::` remote, or an `objects/info/alternates` file
  refuses the operation, naming the file and key; the user reviews and
  removes it.

## Consequences

- Adding a host git call anywhere means using `host_git`; a bare
  `Command::new("git")` is a sandbox escape.
- Some legitimate repo configs are refused until the user cleans them up or
  the allowlist grows; the allowlist only admits keys that can't execute.
- Rejected: trusting `git config` output, or sandboxing host git instead.

Source: docs/high-level-architecture.md, "Principles" ("Host git never runs repo-controlled code").
