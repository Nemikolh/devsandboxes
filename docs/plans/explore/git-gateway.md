# Git gateway

Status: explored, not agreed. Builds on
[credential-broker](credential-broker.md) (base-URL mode, phantoms, the
repo fence). Design and diagrams:
[Credentials and access](../../../site/src/content/architecture/secrets.mdx),
*Git*.

## Goal

Git in a sandbox pushes and fetches through the broker over smart HTTP,
which shows what ssh hides: the repo, the service (`upload-pack` or
`receive-pack`) and the ref names a push updates. That gives:

- no credential in the sandbox for git either;
- read-only sandboxes, and branch rules where a sandbox asks for them
  (none by default: one sandbox often opens many PRs);
- every repeat instance its own clone, served fast from a mirror the host
  keeps, replacing worktrees and the shared `.git` they need.

## What's wrong with worktrees today

From [worktrees](../../architecture/instances/worktrees.md) and
[status](../../status.md):

1. **A shared, writable git directory.** A worktree's `.git` file points at
   `<base>/.git/worktrees/<id>` by absolute path, so the base repo's whole
   `.git` is bind-mounted read-write into every repeat instance
   (`git_companion_mount`): any of them can rewrite another's branch, the
   shared config, packed refs, the other worktrees' records.
2. **Records that look orphaned.** A container sees the base `.git` but not
   the other checkouts, so `git worktree prune` or `git gc` there would
   delete their records: worktree locks (`lock_worktree`, the lock file
   writable from the container), `gc.worktreePruneExpire=never` per exec
   (`gc_keeps_worktrees_env`), and planned stand-in `.git` files
   (`worktrees.sync`, [helper-rpc](../active/helper-rpc.md) step 6) all
   exist to paper over it.
3. **Host-identical paths.** The absolute pointers force the base repo and
   worktrees to be mounted at their host paths.
4. **Ignored files need a host-side store.** `.worktreeinclude` copies and
   `worktree-link` symlinks go through a store the container can write, so
   they're skipped under a worker policy, apply to the primary repo only,
   not to `folders` worktrees.
5. **Two models of a checkout.** A local repeat instance gets a worktree;
   a placed one gets a bundle plus a working-tree snapshot landed in the
   worker's cache, recut when unmounted (fleet code transfer).
6. **Asymmetry.** The first instance mounts your own checkout (any sandbox,
   any root), the next ones get worktrees; a branch checked out elsewhere
   is refused.
7. **Bind costs.** On VM-backed runtimes a bind is slow for `git status` on
   big trees, and ownership needs the identity machinery.

## Shape

### The gateway

- **In the container**, system gitconfig written by the feature (or
  devsandbox for any claimed git host):
  `url."http://127.0.0.1:<port>/github.com/".insteadOf` for
  `https://github.com/`, `git@github.com:` and `ssh://git@github.com/`.
  ssh remotes go through the gateway too, so GitHub needs no agent.
- **The broker** recognizes `GET …/<owner>/<repo>.git/info/refs?service=…`
  and `POST …/git-upload-pack|git-receive-pack`, passes `Git-Protocol`
  (v2 fetches) through, and checks:
  - the repo fence (as gh's: the sandbox's remotes plus
    `access.github.repos`);
  - `git.read` for upload-pack, `git.push` for receive-pack, refused at
    `info/refs` before git sends anything;
  - for pushes, the pkt-line commands at the head of the request body
    (`<old> <new> <ref>`): `branches` patterns, creation and deletion
    (`new` all zeros) rules. The objects stream through unread.
- **Refusing nicely**: a refused push answers a proper `report-status`
  (`ng <ref> <reason>`, in side-band when negotiated) so git prints a
  rejected ref with the reason, rather than a bare `403`. Atomic: one
  refused ref refuses the push.
- **LFS**: `info/lfs/objects/batch` is brokered like any route; the object
  URLs it returns are the LFS server's own short-lived, per-object links
  and go direct.

### The host mirror

One bare mirror per upstream repo on the instance's host
(`<data-dir>/devsandbox/git/<host>/<owner>/<repo>.git`), fetched with the
home's credential through the broker:

- **Refs from upstream, objects from the mirror**: `info/refs` / `ls-refs`
  are proxied to upstream, so the sandbox always sees the truth; the pack
  for `upload-pack` is served by `git upload-pack --stateless-rpc` on the
  mirror (through `host_git`: hooks off) after fetching any wants it
  lacks. A clone of a repo the host already has is local disk speed.
- **Pushes go upstream**, then the mirror fetches the pushed refs.
- **The fence comes first**: the mirror serves an instance only what the
  broker allowed it to read.

### One clone per instance

A repeat instance (and every dispatcher child) clones over the gateway
instead of getting a worktree. Where the clone lives:

| option | host sees files | shared state | cost |
|---|---|---|---|
| a container volume, cloned by devsbd at create | no (through the helper) | none | VS Code must attach; `devsandbox fetch` to get work back |
| a host dir under `.worktrees/<id>/` with its own `.git` | yes | none | a bind, as today, without the companion mount |
| `--filter=blob:none` partial clone, either place | as above | none | lazy blob fetches through the gateway; fast to create |

Leaning: a host dir with its own `.git`, partial clone, so nothing changes
for host editors while the shared `.git` disappears; a volume as an option
for VM-backed runtimes. The first instance keeps binding your checkout
(`checkout = "bind" | "clone"` per sandbox, defaulting to today's split);
its pushes still go through the gateway.

- **Start point and branch** stay as `worktree_start_point` /
  `worktree_branch` decide them today (never the base checkout's branch).
- **Ignored files** (`.worktreeinclude`) are copied once into the clone
  by the host at create, not linked; `worktree-link`'s live sharing needs
  an answer (a read-only bind of the store, or dropped).
- **`rm`'s dirty check** runs `git status` in the clone (host dir) or
  through the helper (volume).
- **Placed runs converge**: a worker clones from its own mirror the same
  way; the code transfer keeps carrying only what upstream doesn't have
  (unpushed commits, the working-tree snapshot).
- **Gone**: `git_companion_mount`, worktree locks, the gc env, the
  host-identical path rule, and `worktrees.sync` from helper-rpc.

### The agent relay

Stays for ssh itself and git hosts no claim covers, gaining what a relay
can see: per-sandbox key filtering (`REQUEST_IDENTITIES` answered with the
sandbox's keys only, signing with others refused) and confirm-on-sign as a
permission request. Repo rules over ssh remain impossible: rewrite to the
gateway where the host speaks HTTPS.

## Ideas weighed

| idea | from | U | N | verdict |
|---|---|---|---|---|
| broker reads receive-pack commands | smart HTTP + egress proxies | 9 | 8 | chosen |
| refs upstream, packs from a host mirror | CI runners' git caches, Gerrit replicas | 8 | 6 | chosen |
| partial clones through the gateway | git's promisor remotes | 7 | 6 | chosen for repeat instances |
| `clone --shared` against a read-only mirror bind | git alternates | 6 | 4 | fastest, but a shared object store again, read-only |
| reflink / overlay snapshots of a base checkout | btrfs, overlayfs | 6 | 6 | filesystem-specific; no help on VM runtimes |
| the host mirror as a local remote: an instance pushes `refs/devsandbox/<instance>/*` to the mirror, not upstream, and your checkout fetches an agent's work from it | inverting "push means upstream" | 8 | 9 | worth a step: review an agent's branch without it ever reaching GitHub |
| repo rules over ssh via the agent | the relay | — | — | fails: the agent never sees the repo |

## Rollout

1. **Pass-through**: `insteadOf` to the gateway, inject the credential,
   the repo fence, `git.read` / `git.push`. Enough for
   [github-and-sentry-access](github-and-sentry-access.md) step 2.
2. **Push rules**: receive-pack command parsing, `branches`, create and
   delete rules, `report-status` refusals.
3. **The host mirror**: proxied refs, local packs, fetch on push.
4. **Clones for repeat instances**: `checkout = "clone"`, dispatcher
   children first; then the default for repeat instances; worktree code
   and its docs removed.
5. **Local remote**: `refs/devsandbox/<instance>/*` on the mirror, `fetch`
   from it.
6. Agent relay key filtering and confirm-on-sign.

## Open questions

- Force-push detection needs ancestry (`old` vs `new`), which the broker
  only knows from the mirror, after the pack: refuse late, or allow
  force-push only where `branches` says?
- Push options, `--atomic`, signed pushes through the parser.
- Non-GitHub hosts (GitLab, Gitea, self-hosted): the same gateway with a
  claim per host; what's GitHub-specific is only minting.
- Mirror pruning and disk: per host, LRU by last use?
