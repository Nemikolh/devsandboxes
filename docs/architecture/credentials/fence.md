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
`instance_folders`, `refresh_at`), `src/broker/policy.rs` (`fence_of`,
`fence_for`), `src/broker/github.rs` (`Fence`, `Lookup`, `Live`,
`Cache`, `Https`, `fetch_name`, `redirect_path`, `fetch_node`),
`src/commands/run/git.rs`
(`read_remotes`, `read_regular`, `remotes_of`), `src/state.rs`
(`State::folder_remotes`, `FolderRemotes`, `FolderRemote`,
`Instance::github_repos`, `State::github_ids`, `GithubPin`), `src/commands/access.rs` (`forget_at`,
`fences`, `render_fences`).

Sentry's org fence, a claim's `orgs`, is the broker's:
[broker](broker.md#the-org-fence).

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

## Enforcement

The broker enforces the fence for a fenced claim (one whose `fence` names
a route, as github's does): `policy::fence_of` is the instance's
`github_repos` plus the config's `access.<kind>.repos` (each through
`fence::repo_arg`), read per call, and a route's `{repo}` (normalized
as `normalize` does) inside it gets the credential, subject to its scope.
Outside it a read goes upstream with the phantom stripped, marked
`devsandbox-fence: outside`, an upstream `401` turned into a `403` naming
`access.<kind>.repos`; a write, a git push included, is refused at
`info/refs`. Entries stand for the repositories they were pinned to
([Ids](#ids)), so `/repositories/{id}` routes are placed too. The rules:
[broker](broker.md#the-repo-fence).

## Ids

A name can change hands: a repo is renamed (GitHub redirects the old
name until someone takes it) or its owner renames their account and
someone registers the old login. So the broker pins each fence entry to
the repository it named the first time the home resolved it, and from
then on the entry stands for that repository's id, whatever the names
do.

```toml
[github_ids."org/app"]
id = 1296269
node_id = "R_kgDOABC"
pinned_at = 1795000000
```

`State::github_ids` maps an entry (normalized, from a folder's record or
`access.github.repos`) to its `GithubPin`, host-wide: an id is GitHub's,
not a folder's. A pin is written once (`Live::pin`, under the state
lock, only where absent) and never changed.

`policy::fence_for` builds a `github::Fence` per call from `fence_of`'s
entries, their pins and, when the approved claim has `api.github.com`
among its upstreams, the lookup with the call's credential
(`github::Via`). Placement (`Fence`):

- An entry without a pin is resolved: `GET /repos/{owner}/{repo}` on
  `api.github.com` with the credential (`Authorization: Bearer`,
  `Accept: application/vnd.github+json`, `User-Agent: devsandbox-broker`).
  A 301/302/307/308 is followed once, and only to a `Location` on
  `https://api.github.com` itself (`redirect_path`): GitHub answers a
  renamed repo's old name with a 301 to `/repositories/{id}`. A second
  redirect, or one to any other host, is never followed. The answer's
  `id`, `node_id` and `full_name` (checked as `normalize` reads names)
  pin the entry. A 404 (no access, no such repo) pins nothing: the entry
  matches by name only, as does every entry without a lookup.
- A `{repo}` that is an entry: in, and when its pinned repository's
  current name (`GET /repositories/{id}`) differs, the call is sent to
  that name (`route::with_repo` rewrites the two segments, a `.git` or
  `.wiki.git` suffix kept; `Injection::path`). The old name is never
  dialed with the credential, so whoever registers it gets nothing.
- A `{repo}` that isn't: in when it is the current name of an entry's
  pinned repository. Only the entries are looked up, never the name the
  sandbox sent, so what the sandbox names can't spend the credential's
  rate limit.
- A repository id (`/repositories/{id}`): in when an entry is pinned to
  it. Outside, its name (looked up by id) names it in the refusal.
- A node id (`Fence::place_node`, for GraphQL mutation inputs): in when
  an entry is pinned to that repository node id, with no call; else
  GitHub's `node(id:)` (`fetch_node`: a repository, or the `repository`
  of a `RepositoryNode`, `Ref`, `Release`, `Milestone`, `Label` or
  `Commit`) and placed by that repository's id.

`Live` is the lookup `serve` runs: answers cached in the daemon's
memory per (credential, name | id | node id), since what a credential
sees is its own: a repository for `TTL` (1 h), a 404 for `MISSING_TTL`
(5 min), any other answer or none for `FAILED_TTL` (1 min), at most
`CACHE_CAP` entries. A node that isn't a repository is never cached (a
transferred issue keeps its id), nor is a node that resolves to nothing.
The calls go through the broker's upstream client (`Https`), redirects
off, the token the credential's (`Renewer::token`, renewed when due).

## `access forget`

`devsandbox access forget <folder> <repo>` (`forget_at`;
[credentials CLI](../../reference/credentials-cli.md#access-forget))
removes `<repo>` (`fence::repo_arg`: `owner/repo` or its GitHub URL,
normalized, no alias resolved) from the folder's `github`. Nothing
re-adds it: the record is never re-read. `remotes` stays verbatim. The
instances on that folder keep their snapshot until their next `start` or
`rebuild`; `access ls` lists each folder's fence and marks those
instances.
