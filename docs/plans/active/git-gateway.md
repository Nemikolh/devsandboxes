# Git gateway

Status: agreed; building step 1, the rest after. Builds on
[credential-broker](credential-broker.md) (intercept mode, phantoms, the
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
- every repeat instance its own clone, seeded from your checkout and
  upstream, replacing worktrees and the shared `.git` they need.

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

- **In the container** git reaches `github.com` through credential-broker's
  intercept (the member CA) on a proxy port of its own, the only one
  where `github.com` is claimed, so remotes stay
  `https://github.com/` URLs: gh reads its repo from `git remote -v`,
  which applies `insteadOf`, and drops remotes on hosts it doesn't know,
  so a loopback URL would break it
  ([github-and-sentry-access](github-and-sentry-access.md)). The claim's
  `git` field rewrites only the ssh forms (`git@github.com:`,
  `ssh://git@github.com/`, resolved host aliases) to
  `https://github.com/`, so GitHub needs no agent, and per fenced owner,
  spelled as each recorded URL spells it (the match is case-sensitive),
  maps `https://github.com/<owner>/` to itself with both `insteadOf` and
  `pushInsteadOf` (git keeps the longest match, doesn't chain rewrites,
  and matches `pushInsteadOf` against the raw URL in a table of its
  own), so a dotfile's `url."git@github.com:".insteadOf` or
  `.pushInsteadOf = https://github.com/` loses to the longer identity
  entry; an equally long one wins if read first (global scope), which
  only the boot check catches. Alongside, scoped to `https://github.com/`:
  `http.<url>.proxy` set to that port (git's `http.proxy`
  beats `HTTPS_PROXY`, so a dotfile's corporate proxy would send git past
  the broker), `http.<url>.sslCAInfo` set to the system bundle file that
  carries the member CA (credential-broker, *The member CA*; nix and
  conda git carry their own bundle path, and a dotfile's `sslCAInfo`
  replaces the system one where libcurl has no compiled-in CA path:
  Debian's keeps trusting `/etc/ssl/certs` beside it unless `sslCAPath`
  replaces that too), and the
  `credential.<url>.helper` list reset (empty `helper =`) then set to
  devsbd, answering `x-access-token` and the phantom after git's first
  401 (it sees only protocol and host, `useHttpPath` unset, and ignores
  the `store` git sends after a success and the `erase` after a
  failure): the broker injects only on a request carrying the phantom, and
  git-lfs asks `git credential fill` the same way. The settings go on
  the container env as `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/
  `GIT_CONFIG_VALUE_n`: command scope is read last, so equally specific
  user `http.<url>.*` and `credential.<url>.*` settings lose, only there does the reset drop the user's global
  `store`/`cache` helpers (which `approve` would otherwise feed the
  phantom, into a `~/.git-credentials` that is often a host bind), and
  gits reading another system gitconfig (nix, conda) or none get them
  too; the system gitconfig carries a copy for tools that clear the env.
  A config's own `GIT_CONFIG_*` in `containerEnv`/`remoteEnv` is merged
  ahead of these entries, never replacing them.
  `CARGO_NET_GIT_FETCH_WITH_CLI=true` goes there too (env beats a
  `.cargo/config`): cargo's libgit2 fetcher reads only the plain
  `http.proxy`, then `HTTPS_PROXY`, the shared port where `github.com`
  is dialed directly, so a private git dep would carry the phantom
  straight to GitHub. The config reference names the other clients that
  skip URL-scoped proxies (JGit, go-git, dulwich).
  gh's own `-c credential.helper=…gh auth git-credential` replaces the
  helper and hands over the same phantom. At boot, and again after the
  create-time lifecycle commands (dotfiles land there), devsbd checks
  each remote's resulting fetch and push URLs (`git remote get-url
  [--push] --all`; `--get-urlmatch` doesn't evaluate rewrites), the
  effective proxy and CA for each of those URLs (`git config
  --get-urlmatch http <url>`, with the remote's real URL, as a user's
  `http.https://github.com/<owner>/.proxy` shows only there), and the
  env `--get-urlmatch` doesn't reflect: `GIT_SSL_CAINFO`,
  `GIT_SSL_CAPATH` and `GIT_SSL_NO_VERIFY` beat every config scope,
  command scope included, and a `NO_PROXY` covering `github.com` (or
  `*`) bypasses `http.<url>.proxy`, so git goes direct and the helper
  hands the phantom to GitHub (fails closed: "Invalid username or
  token"; a shell rc's `NO_PROXY` the check can't see). It warns when a
  user's setting still wins (a more specific `http.<url>.proxy`, one of
  those env vars, a URL off `https://github.com/`: that push fails under
  key filtering with `Permission denied (publickey)`), naming the setting
  to drop or narrow; `--show-scope` prints `unknown` for
  `--get-urlmatch` on git 2.39, so the file comes from `git config
  --show-origin --get-regexp` over `http.*.proxy` and `http.*.sslCAInfo`.
- **The broker** recognizes `GET …/<owner>/<repo>.git/info/refs?service=…`
  and `POST …/git-upload-pack|git-receive-pack` (de-chunked, gzip
  bodies decoded), passes `Git-Protocol` (v2 fetches) through, and
  checks, with the rule table gh's REST and GraphQL routes share:
  - the repo fence (as gh's: the folder's recorded remotes plus
    `access.github.repos`, below), normalized as credential-broker's
    *Policy* says and forwarded exactly as checked. The fence resolves to
    repo ids at the home as gh's does, so a renamed repo's old and new
    names both match and the request goes to the current name; an
    upstream redirect is never followed with the credential. Outside
    the fence, as credential-broker's policy says, `upload-pack` goes
    upstream with no credential (public git deps and submodules work),
    `receive-pack` is refused, and an upstream 401 becomes the broker's
    403 naming `access.github.repos`, so git never prompts;
  - `repo.read` for upload-pack, `git.push` for receive-pack, refused at
    `info/refs` before git sends anything;
  - for pushes, the receive-pack request parsed to its full grammar
    (`*shallow (command-list | push-cert)`, 40- or 64-hex ids, the empty
    `0000` probe git sends before a large push allowed): `branches` and
    `tags` patterns, creation and deletion (`new` all zeros) rules; with
    either set, a ref outside `refs/heads/` and `refs/tags/`
    (`refs/notes/*`, custom namespaces) is refused. The claim's
    `presence` ref forms (`refs/tags/**`, deletions) match the parsed
    commands too: the push is held before anything goes upstream (the
    pack waits on window credit), the ask keyed on the ref
    (credential-broker, *`presence`*), and a hold that runs out answers
    `ng`. Anything not fully parsed is refused, `push-cert` included until
    supported. The objects stream through unread.
- **Refusing nicely**: a refused push drains the pack, then answers a
  `report-status` (`ng <ref> <reason>`, `report-status-v2` and side-band
  as the client negotiated) so git prints a rejected ref with the reason,
  rather than a bare `403` or "remote end hung up". Atomic: one refused
  ref refuses the push.
- **Branch rules bind every ref writer**: `branches` and `tags` apply to
  every REST route and GraphQL mutation that moves refs, and the claim's
  scope table marks each write route as a ref writer, with the field
  naming its ref, or as moving none (ref writers include `git/refs`,
  `contents` writes and deletes with `branch`, `merges`,
  `pulls/{n}/merge`, `pulls/{n}/update-branch`, `merge-upstream`,
  `branches/{b}/rename`, `releases` create and update (a tag from
  `tag_name`/`target_commitish`), code-scanning autofix commits;
  `createRef`, `updateRef(s)`, `deleteRef`, `createCommitOnBranch`,
  `createLinkedBranch`, `mergeBranch`, `mergePullRequest`,
  `enablePullRequestAutoMerge`, `enqueuePullRequest`,
  `updatePullRequestBranch`, `revertPullRequest`). With `branches` or
  `tags` set, an unmarked write route is denied (a glob like `POST
  /repos/{repo}/**` marks nothing), and a ref writer must name its ref
  by path or body field, or by a node id the home resolves (`node(id)`)
  before deciding; anything else is denied, never `other`/ask. A test
  checks the marks against gh's commands.
- **Workflows**: a pushed `.github/workflows/*` runs with the repo's
  Actions secrets, one hop from the sandbox. Installation tokens are
  minted without the `workflows` permission (GitHub then refuses such
  pushes) unless the sandbox has `git.push.workflows`; under a user
  token (`as = "user"`) the token's own Workflows permission decides,
  which devsandbox can't read for a fine-grained PAT. That stops new or edited
  workflows only: an existing `on: push` workflow runs whatever repo
  code the pushed branch carries (`scripts/ci.sh`, `build.rs`), so
  `git.push` is code execution in CI, and what keeps secrets from it is
  GitHub's side (environments with required reviewers, secrets only on
  protected branches). The cost: GitHub checks the commits a push adds,
  so a merge of `origin/main` that brings in main's own workflow changes
  is refused too, as are `merges` and `update-branch` calls then; the
  sandbox rebases instead, or gets `git.push.workflows`, and the config
  reference says so.
- **LFS**: `info/lfs/objects/batch` is brokered, and its response
  rewritten: action `header`s carry credentials (GitHub's `RemoteAuth` on
  `lfs.github.com`'s verify, AWS signatures on S3 uploads, other hosts
  echo the client's `Authorization`), and transfers go straight to the
  storage hosts (`github-cloud.githubusercontent.com`, S3), which the
  `github.com`-scoped proxy doesn't cover, so each action's `href`
  becomes `http://127.0.0.1:<port>/<handle>` on a loopback port for LFS
  transfers (base-URL mode: storage hosts vary, so none joins the CA's
  names), the handle mapping, per instance, to the href and header the
  broker adds on the way out. The same
  response rewrite as sentry's chunk uploads
  ([github-and-sentry-access](github-and-sentry-access.md)).
  `info/lfs/locks*` maps to `git.push` for writes (`locks/verify`, sent
  before every push) and `repo.read` for listing.

### Host git

The host keeps no copy of a repo: the sandbox's fetches and pushes pass
through the gateway. Every host fetch this plan adds from a claimed host runs on behalf of
one instance, with its claim and credential, never as a
member-wide grant: host git fetches with a one-shot credential helper
(a `devsandbox` subprocess asking the daemon, bound to the instance) and
none of the user's git config (`GIT_CONFIG_GLOBAL=/dev/null`,
`GIT_CONFIG_NOSYSTEM=1`, `credential.helper=` before the one-shot,
`protocol.allow=never` with `protocol.https.allow=always`; only the
bundle seed and step 4's `rm` bundle fetch add `protocol.file.allow=always`, each
naming its exact source path; the user's `http.proxy` and
`http.sslCAInfo`, read by devsandbox, passed back as `-c`): a dotfile
`insteadOf` to ssh, `core.sshCommand`, `http.extraHeader` or a global
helper would otherwise fetch with the member's own keys and tokens,
unfenced and unlogged. `host_git`'s `safe_env` covers neither, so
every host git this plan adds runs in this env (*Who clones*).

### One clone per instance

A repeat instance (and every dispatcher child, and a `folders` entry
another sandbox owns: whatever gets a worktree today) gets its own full
clone instead of a worktree. Where the clone lives:

| option | host sees files | shared state | cost |
|---|---|---|---|
| a container volume, cloned by devsbd at create | no (through the helper) | none | VS Code must attach; work comes back only by a fetch-back through the helper (none exists: `devsandbox fetch` is placements only) |
| a host dir under `.worktrees/<id>/` with its own `.git` | yes | none | a bind, as today, without the companion mount |

Leaning: a host dir with its own `.git`, so nothing changes for host
editors while the shared `.git` disappears; a volume as an option for
VM-backed runtimes. Full clones, not partial: a promisor clone writes
`remote.*.promisor`/`partialclonefilter`, which `refusal` in
`src/commands/run/git.rs` refuses, lazily fetches with host credentials,
and breaks `log -p` or `checkout` mid-task when the daemon or the home is
unreachable. The first instance keeps binding your checkout unless the sandbox sets
`checkout = "clone"` (default `"bind"`: repeats get worktrees until
step 4 makes them clones), which clones it too; its pushes still go
through the gateway.

- **Who clones**: a host dir must exist before the container, so before
  any bridge or instance identity. Host git inits the clone and fetches
  (`--update-head-ok`: the unborn `HEAD` branch counts as checked out)
  from a bundle of the base checkout `+refs/heads/*:refs/heads/*`,
  `+refs/remotes/*:refs/remotes/*`, tags and `HEAD:refs/devsandbox/base-head`
  (what remote-less repos start from), then fetches each recorded URL's
  heads into `refs/remotes/<remote>/*` and its tags (`--prune`: the bundle
  carries branches upstream deleted) and runs `remote set-head <remote>
  --auto`, before the container starts, so the start point is as fresh as today's
  `fetch_origin` makes it (the bundle makes that fetch small). A claimed
  host's fetch and `set-head` use the instance-bound helper and the
  repo's https form, `https://github.com/<owner>/<repo>` at its
  resolved current name (the isolated env refuses ssh and scp forms;
  the clone's remote URLs keep the recorded spelling); an unclaimed one's (GitLab,
  ssh remotes, any sandbox without the claim) runs through `host_git` with
  the user's config and keys as `fetch_origin` does today: the URL is the
  record's and the clone isn't container-written yet. A failed fetch
  leaves the bundle's refs, with a warning. Host git never opens a container-writable git dir
  (the base checkout's, a clone's once its container has started): a
  `gitdir:`, a `commondir` or a symlinked `refs/`/`objects/` there
  points host git at any host repo, past a path check and racing the
  container. The bundle is `git -c safe.directory='*' bundle create -
  --all` (the image's root on a host-owned checkout is otherwise
  "dubious ownership") in a one-off `--rm` container of the sandbox's
  image (entrypoint overridden) binding only the
  checkout, read-only, so those pointers resolve in the container's
  filesystem; its stdout goes to a host-private file. When the bundle
  fails (no git in the image, a `.git` file pointing outside the
  checkout as a linked worktree's or submodule's does, a repo format the
  image's git can't read) a repo with a remote starts from the upstream
  fetch alone, with a warning; a remote-less one makes `run` refuse,
  saying why. Every one-off container in this plan (the bundle, the
  includes copy, `rm`'s check, step 4's fetch-back) reads sandbox-written
  config, so it runs `--network none`, its binds labeled as `create_args`
  labels them, and its git with `core.fsmonitor=false` and
  `core.hooksPath=/dev/null`. Each clone is a full copy: N × repo size on disk,
  and a full `pack-objects` plus a host `index-pack` per create, minutes
  for a multi-GB repo against a worktree's seconds. Each `folders` repo gets the same (its
  own bundle, clone and seed), detached at its `refs/devsandbox/base-head`
  (its checkout's `HEAD`) as today. The clone gets every recorded remote
  (your fork and the org's upstream), by name, with their upstream URLs.
  With the home offline at create, the clone comes from the base
  checkout's bundle (or the placed one) only, with a warning. The gateway
  serves only the instance's later fetches and pushes. Every other host git
  here (init, the bundle and claimed fetches, the checkout, step 4's fetch from `rm`'s
  bundle) runs in *Host git*'s isolated env: the checked-out tree is
  sandbox-written, and its `.gitattributes` could select a driver the
  user's global config defines (`filter.lfs.process`, with a `.lfsconfig`
  `lfs.url` naming another private repo or a LAN address), run on the
  host with the user's helpers. Without that config LFS pointers stay
  pointers; `git lfs pull` in the container fetches through the gateway.
- **Host fetches use recorded URLs**: today `worktree_start_point`'s
  `fetch_origin`, `fetch_origin_branch` and `rm`'s `fetch_prune` run
  `fetch origin` in the base checkout with the user's credentials, and
  `refusal` in `src/commands/run/git.rs` allows any `remote.*.url` but
  `ext::`: a sandbox that points `origin` at an out-of-fence private repo
  or a host path (`/home/u/other-repo`) gets it fetched into a checkout
  it reads at the next `run` or `rm`. Every host fetch for a folder names
  a recorded URL instead (claimed hosts with the
  instance-bound helper), never a host path, and lands in the new
  clone, not the base checkout.
- **Start point and branch** stay as `worktree_start_point` /
  `worktree_branch` decide them today (never the base checkout's branch),
  resolved in the clone over the refs fetched above: the base's `HEAD`
  fallback reads `refs/devsandbox/base-head` (the clone's own `HEAD` is
  unborn), and `refs/remotes/<remote>/HEAD` is the symref `set-head`
  wrote, else the bundle's plain copy, read as a ref.
- **Ignored files** (`.worktreeinclude`) are copied once into the clone
  at create, not linked, by a second such container binding the clone
  rw, run after host git's last step in it and as the identity plan's
  user (`src/commands/run/identity.rs`), never by host code
  (`copy_entry` follows a parent directory swapped for a symlink:
  `worker_skip_warning`'s reason); `worktree-link`'s live sharing needs
  an answer (a read-only bind of the store, or dropped).
- **`rm`'s check** runs `git status` and looks for commits on no remote
  in a one-off container (`-c safe.directory`) binding the clone
  read-only (host dir) or
  through the helper (volume), never host git: unlike a worktree's, a
  clone's commits aren't in your checkout, so `rm` asks before deleting
  them (step 4: the container bundles them to a host-private file, which
  host git fetches into the local remote first).
- **Placed and remote runs converge**: a worker or machine holding a
  delegated credential (decision 3's installation tokens), or the home
  itself, fetches upstream into its clone the same way, others start
  from the bundle alone (a static credential
  never leaves the home); the code transfer keeps carrying only what upstream doesn't have
  (unpushed commits, the working-tree snapshot).
- **The folder's remotes** are the per-folder record in `state.toml`
  (keyed by canonical host path, kept past `rm`, snapshotted on each
  `Instance`, as in github-and-sentry-access): every remote's name and
  `url`/`pushurl` verbatim, any host, a host path, `file://` or `ext::`
  URL dropped, for every sandbox, claimed or not; the GitHub fence is
  its `github.com` entries. Read at the folder's first `run` ever (built:
  the common dir's `config` read as data by devsandbox's strict parser,
  opened without following symlinks, size-capped, includes never
  followed, never host git; no image or one-off container needed, so it
  works before any build and in images without git:
  [fence](../../architecture/credentials/fence.md)), never re-read, so `rm`-ing every instance
  doesn't make the next `run` a first one; your checkout and the clones
  are writable from the container, so a `git remote add` or a rewritten
  `remote.*.url` there never changes it; widening is
  `access.github.repos`.
- **Config**: a `folders` entry's `worktree` mode becomes its clone or
  bind choice (`always` clones, `never` binds, `auto` as today's rule);
  `worktree-branch`, `worktree-base` and `worktree-include` keep their
  meaning for clones, `worktree-link` waits on *Ignored files*; the
  `config-toml-spec` skill renames them with step 4.
- **Gone**: `git_companion_mount`, worktree locks, the gc env, the
  host-identical path rule, and `worktrees.sync` from helper-rpc.
  **Replaced**: dispatchers' `branches` op (`src/commands/dispatch.rs`,
  today `worktree list --porcelain` on the base repo) lists children's
  branches from the local remote's `refs/devsandbox/<instance>/*`.

### Other ways around the gateway

`insteadOf` is advice: anything in the sandbox can `ssh git@github.com
git-receive-pack …` through the agent relay (on by default, `relay_first`
in `src/commands/run/mod.rs`) with the user's key. Refusing sessions
bound (`session-bind@openssh.com`) to GitHub's host keys can't fence
that: the sandbox's own client can bind to a host key it made up, then
have a plain `publickey` request signed, which names no host key; and
clients without session-bind (Go's x/crypto/ssh, libgit2, OpenSSH
before 8.9) would break everywhere. So from step 1 the relay filters
keys: when a git-host claim applies, `REQUEST_IDENTITIES` answers only
the keys the sandbox's config names (none by default) and signing with
others is refused. A named key is usually also on GitHub, so it comes
with its destinations, OpenSSH's destination-constraint model
(`ssh-add -h`): the relay signs with it only on a connection bound by
`session-bind@openssh.com` to one of those hosts' keys, and only data
carrying that bound session id. A forger can't bind GitHub's session to
a host key it doesn't hold, so an allowlist holds where a denylist of
GitHub's keys can't; clients without session-bind lose named keys only.
The destinations' host keys come from the filtering host's own
`known_hosts` (`ssh-keygen -F`, read per connection, so rotations
follow), never the container's, where the sandbox could plant its own
key under the destination's name; a miss refuses, naming the host and
that it must be in the host's `known_hosts`. A key named without
destinations, or with a claimed git host among them, reopens GitHub over ssh, past the fence and read-only, and
the config reference says so. ssh commit signing (`gpg.format = ssh`)
has the agent sign an SSHSIG blob, which carries no session id, so a
named key can also be signing-only: listed, and used only for data
that parses as SSHSIG (its magic preamble) in namespace `git`, never
for userauth, so it can't log in to GitHub. Without one, the boot check
warns when `commit.gpgsign` is on with an ssh key, before `git commit`
fails mid-task. A remote
instance's agent arrives over `AGENT` streams ([ssh-agent](../../architecture/helper/ssh-agent.md)), so the
filter sits where the relay ends, on the instance's host. Mount mode
(`ssh_agent_forward`) can't filter and is refused for an instance with
a claim.

VS Code's attach (`devsandbox vscode`) is outside the guarantee.
Dev Containers installs a git credential helper calling back through
`vscode-remote-containers-ipc-*.sock`, forwards the host's agent
(`vscode-ssh-auth-*.sock`), and the git extension's askpass
(`VSCODE_GIT_IPC_HANDLE`) hands over the user's GitHub login. Its
switches (`dev.containers.gitCredentialHelperConfigLocation`,
`copyGitConfig`, `dockerCredentialHelper`) are host-side user settings,
not the remote `settings` of the `nameConfigs` file
(`src/commands/run/editor.rs`) devsandbox writes, and reports have the
IPC socket answering credential requests with them off. So
`devsandbox vscode` warns on an instance with a claim and needs a flag
to go on.

The relay stays for ssh to named destinations and, with no claim, for
everything, gaining
confirm-on-sign as a permission request. Repo rules over ssh remain
impossible: rewrite to the gateway where the host speaks HTTPS.

## Ideas weighed

| idea | from | U | N | verdict |
|---|---|---|---|---|
| broker reads receive-pack commands | smart HTTP + egress proxies | 9 | 8 | chosen |
| refs upstream, packs from a host mirror | CI runners' git caches, Gerrit replicas | 8 | 6 | later, if GitHub bandwidth matters: needs a per-want fence gate and keeps tips other credentials fetched |
| partial clones through the gateway | git's promisor remotes | 7 | 6 | no: host git refuses them, offline local ops fail mid-task |
| `clone --shared` against a read-only mirror bind | git alternates | 6 | 4 | fastest, but a shared object store again, read-only |
| reflink / overlay snapshots of a base checkout | btrfs, overlayfs | 6 | 6 | not a checkout model; a reflinked copy (`FICLONE`, `clonefile`: own inodes) is a later speedup of the clone's copy where the filesystem has it |
| a local remote: an instance pushes `refs/devsandbox/<instance>/*` to a host repo, not upstream, and your checkout fetches an agent's work from it | inverting "push means upstream" | 8 | 9 | worth a step: review an agent's branch without it ever reaching GitHub |
| repo rules over ssh via the agent | the relay | — | — | fails: the agent never sees the repo |

## Rollout

1. **Pass-through**, on credential-broker step 3 (intercept): the ssh
   forms rewritten to `https://github.com/`, the scoped proxy, CA and
   credential helper settings, inject the credential, the per-folder
   repo fence, `repo.read` / `git.push`, LFS rewrite, the relay's key
   filtering under a git-host claim (named keys with destinations from
   the host's `known_hosts`, signing-only keys, mount mode refused),
   `devsandbox vscode`'s warning. The `config-toml-spec` skill gains the
   named keys. The claim applies to an
   instance only where it can be brokered: locally, and on placed or
   remote instances once credential-broker step 5 carries `CRED`; until
   then they keep today's agent and `[[agent]]` grants. Enough for
   [github-and-sentry-access](github-and-sentry-access.md) step 2.
   Built: the remotes record and fence
   ([fence](../../architecture/credentials/fence.md)), the broker's fence
   and smart HTTP ([broker](../../architecture/credentials/broker.md#the-repo-fence)),
   git's own proxy port, settings, credential helper and boot check, LFS
   handles ([git](../../architecture/credentials/git.md)), the relay's
   key filtering ([ssh-agent](../../architecture/helper/ssh-agent.md#key-filtering)),
   `devsandbox vscode`'s refusal, `ssh-keys` in the `config-toml-spec`
   skill; tested end to end with a clone and push through the broker
   (git's 401 challenge on a private upstream came with it). Deferred, each a
   [status](../../status.md#credentials) limitation: rewrites for a
   recorded ssh host alias (`git@github-work:`), the fence resolved to
   repo ids (shared with github-and-sentry-access step 2), `access ls`
   marking a host-class home or shared writable bind.
2. **Push rules**: receive-pack parsing, `branches` and `tags` (git and
   REST), create and delete rules, `report-status` refusals,
   `git.push.workflows`, `presence` ref forms (passing as in step 1
   until credential-broker step 4b asks).
3. **Clones for repeat instances**: `checkout = "clone"` opt-in, the
   recorded-URL host fetches, `rm` asking before unpushed commits go.
   Dispatcher children stay on worktrees. The `config-toml-spec` skill
   gains `checkout`.
4. **Local remote**: `refs/devsandbox/<instance>/*` in a per-instance
   host repo (`receive.fsckObjects`, a size cap), pushed
   to over a `devsandbox` remote at the broker's loopback base URL (gh
   drops it as an unknown host), fetched into your checkout after each
   push to it, into `refs/remotes/devsandbox/<instance>/*` (a rename to
   `origin` can't clobber your tracking refs), by a one-off container
   of the sandbox's image
   (as the identity plan's user for host-owned binds, `runtime::identity::plan`,
   so new objects stay yours on the host, and `-c safe.directory`)
   binding the checkout rw and the instance's repo read-only, never host
   git (*Who clones*: your checkout's git dir is
   container-writable). A failed fetch-back (no git in the image, a repo
   format its git can't read) warns, naming the instance repo's path;
   the work stays there. Only what an instance pushes there reaches you,
   and dispatchers' `branches` lists only a child's pushed branches.
   Then dispatcher children move to clones with `branches` relisted over
   it, and clones become the default for repeat instances (today their
   commits land in your checkout's refs at once; without this they
   wouldn't reach it unless pushed upstream); worktree code and its docs
   removed.
5. Agent relay confirm-on-sign.

## Open questions

- Force-push detection needs ancestry (`old` vs `new`), which the broker
  holding no copy of the repo doesn't know: refuse late, allow
  force-push only where `branches` says, or receive into a host
  quarantine (ancestry checked by host code, no hooks) and push upstream
  from the host?
- Push options, `--atomic`, signed pushes (`push-cert`) through the
  parser.
- VS Code attach for real: a devsbd reaper deleting the IPC and
  ssh-auth sockets as they appear (racy, what others ship), or an attach
  path that installs no bridges?
- Non-GitHub hosts (GitLab, Gitea, self-hosted): the same gateway with a
  claim per host; what's GitHub-specific is only minting.
