# The folder's remotes and the GitHub repo fence

Which GitHub repos an instance's credential may reach on top of its
config's `access.github.repos`: the remotes its folders held when each
was first run, recorded once and never re-read. Read this before
touching how remotes are read, recorded or narrowed. The design is
[git-gateway](../../plans/active/git-gateway.md) (*The folder's remotes*)
and [github-and-sentry-access](../../plans/active/github-and-sentry-access.md)
(*The repo fence*, *Matching*). What isn't built yet:
[status](../../status.md#credentials).

Code: `src/credentials/fence.rs` (`parse`, `github_repo`, `normalize`,
`plain_host`, `ssh_g_hostname`, `ssh_hostname`, `repo_arg`),
`src/commands/run/remotes.rs` (`record_at`, `read_folder`, `fence_of`,
`instance_folders`, `refresh_at`), `src/commands/run/git.rs`
(`read_remotes`, `read_regular`, `remotes_of`), `src/state.rs`
(`State::folder_remotes`, `FolderRemotes`, `FolderRemote`,
`Instance::github_repos`), `src/commands/access.rs` (`forget_at`,
`fences`, `render_fences`).

## Why once

From its first container on, a folder's `.git/config` is writable from
the sandbox (the folder is bind-mounted; worktree instances share the
base's `.git`). A remote added or rewritten there must never widen what
the sandbox's credential reaches, so the record is taken before the
folder's first container ever and never taken again: `rm`-ing every
instance leaves it, so the next `run` is no first run. A remote the user
adds later counts only once the config names it in `access.github.repos`.

## Recording

`materialize` (every `run` and `rebuild`) calls `remotes::record` with
the base folder and each `folders` entry, canonical, after
`initializeCommand` (it may clone them) and before any service or
container. `record_at` reads each folder with no `State::folder_remotes`
entry outside the state lock, then inserts it under the lock only where
still absent (a racing `run` that recorded first wins):

- No `.git` at all: recorded empty with `unread`, silently; a later
  `git init` in the folder, by the sandbox or the user, counts no more
  than a later remote.
- Else `git::read_remotes`: the git dirs resolved as `host_git` resolves
  them (`git_dirs`: a `.git` symlink, or a `gitdir:` file git didn't
  write for this checkout, refused), then the common dir's `config` read
  as data, never by git: opened `O_NOFOLLOW | O_NONBLOCK` (a symlink or
  FIFO there is refused), a regular file of at most 1 MiB, parsed by the
  same strict `parse_config` as `check_repo`. No key is refused (the
  record runs nothing), and `include.path`/`includeIf` are never
  followed. Every `[remote "<name>"]`'s `url` and `pushurl` values are
  kept verbatim and in order where `fence::parse` keeps them; a remote
  left with none is dropped. `branch.*.remote` URLs, `.gitmodules` and
  `config.worktree` never count.
- A read that fails (an unparsable config, a refused `.git`) is recorded
  empty with `unread` and a warning: never retried.

Reading the file in pure Rust rather than with `git config
--get-regexp` in a one-off container (the plan's first idea) needs no git
in the image, works before any image is built, and is deterministic; it
runs no git on the host either (decision
[0057](../../decisions/0057-host-git-never-follows-what-a-sandbox-can-write.md)).

The run that records a git folder prints one line: the folder's GitHub
fence, or that it has none, or the warning.

## What a record keeps

```toml
[folder_remotes."/home/u/app"]
github = ["org/app", "up/app"]
recorded_at = 1795000000

[[folder_remotes."/home/u/app".remotes]]
name = "origin"
urls = ["git@github-work:Org/App.git"]
pushurls = ["https://github.com/org/app"]
```

`fence::parse` keeps a URL naming a network host: `https://`, `http://`,
`git://`, `ssh://`, `git+ssh://`, `ssh+git://` (any case), or scp form
`[user@]host:path` (a `:` before any `/`, git's rule). Dropped: host
paths, `file://`, `ext::` and every other `<transport>::` helper, other
schemes, URLs with spaces or control characters, and hosts that aren't
`plain_host` (letters, digits, `.`, `-`, `_`, starting with a letter or
digit: no IPv6 literal, no leading `-` ssh would read as an option).

`github` is the fence: the `owner/repo` each kept URL names on GitHub
(`github_repo`), sorted and deduplicated:

- `https://github.com/<owner>/<repo>` (port 443 at most, one trailing
  `/` allowed), `ssh://[user@]github.com[:22]/…` and scp
  `[user@]github.com:[/]…`; `ssh.github.com` (GitHub's ssh on 443) counts
  for the ssh forms. `http://` and `git://` don't count.
- Any other ssh-form host is resolved as an alias with `ssh -G -- <host>`
  on the host (`ssh_hostname`, its `hostname` line read by
  `ssh_g_hostname`), once per host per recording, and counts when it
  resolves to one of those. Only a `plain_host` is handed to ssh:
  `.git/config` wrote it, and ssh substitutes it into a user's `Match exec`
  commands. Resolved when recorded, so a later ssh config edit changes
  nothing.
- `normalize`: owner `[A-Za-z0-9-]+`, repo `[A-Za-z0-9._-]+` (not `.` or
  `..`), lowercased, `.wiki.git` or `.git` dropped; exactly two segments.
  Anything else (an encoded character, a dot-segment, `//`, a query, more
  segments) names no repo: refused, never decoded.

`remotes` keeps every host's URLs, GitHub's included, for the host
fetches of a later clone step.

## Instances' snapshot

`Instance::github_repos` is `fence_of` the instance's folders
(`instance_folders`: its base folder, then each `folders` entry's): the
union of their records' `github`. `materialize` sets it at `run` and
`rebuild`; `start` re-takes it from the records (`remotes::refresh`,
before the container starts) without ever recording, so a folder an
instance mounted before records existed gets one at its next `run` or
`rebuild`. The config's `access.github.repos` isn't in it: the broker
reads that per call.

## `access forget`

`devsandbox access forget <folder> <repo>` (`forget_at`;
[credentials CLI](../../reference/credentials-cli.md#access-forget))
removes `<repo>` (`fence::repo_arg`: `owner/repo` or its GitHub URL,
normalized, no alias resolved) from the folder's `github`. Nothing
re-adds it: the record is never re-read. `remotes` stays verbatim. The
instances on that folder keep their snapshot until their next `start` or
`rebuild`; `access ls` lists each folder's fence and marks those
instances.
