# git in the container under a git-host claim

How git in a sandbox reaches a claim's git hosts (`git.hosts`, e.g.
`github.com`) through the credential broker: its own proxy port, the
member CA, a credential helper handing out the phantom, and the ssh
forms rewritten to `https://`. Read this before touching git's settings,
the helper or the boot check. The design is
[git-gateway](../../plans/active/git-gateway.md) (*The gateway*, "In the
container") and [github-and-sentry-access](../../plans/active/github-and-sentry-access.md)
(*gh*, "git"); what the broker then checks per request is
[broker](broker.md#the-repo-fence). What isn't built yet:
[status](../../status.md#credentials).

Code: `src/credentials/git.rs` (`entries`, `owners`, `config_env`,
`exec_config_env`, `system_config`, `SYSTEM_SCRIPT`, `helper`),
`src/credentials/instance.rs` (`plan`, `Intercept::git`, `Intercept::lfs`,
`ProxyPort`, `broker_spec`), `src/broker/lfs.rs`, `src/commands/run/claims.rs` (`wire`,
`install_git_config`, `git_check_command`, `git_check`, `signs_git`),
`src/commands/run/mod.rs` (`materialize`), `src/commands/start.rs`
(`warn_git`), `src/state.rs` (`Instance::git_env`, `exec_env`),
`devsbd/src/credential.rs`, `devsbd/src/gitcheck.rs`,
`src/devsbd/brokerfile.rs` (`credential`).

## git's proxy port

`instance::plan` gives an instance whose claims name `git.hosts` a second
proxy port next to the shared one (`Intercept::git`, a `ProxyPort` from
`instance::PORTS`, kept by `rebuild` like the shared port). Its bound
hosts are the claims' git hosts, and it is the only port where they are
handed to the broker: the shared port leaves them out, so a `CONNECT
github.com:443` there, from any tool following `HTTPS_PROXY`, is dialed
directly like any unclaimed host. Only git's URL-scoped `http.proxy`
(below) points at git's port; git-lfs reads it too. The broker file
carries it as a second `proxy-listen` line. Like every broker port it is
marked `onAutoForward: ignore` in the VS Code name config and refused by
`forwards.*` (`instance::ports`, `instance::forward_refusal`). The member
CA covers both ports' hosts (`Intercept::bound_hosts`).

## Settings

`credentials::git::entries`, per git host `H`, in this order:

| Key | Value |
| --- | --- |
| `url.https://H/.insteadOf` | `git@H:` |
| `url.https://H/.insteadOf` | `ssh://git@H/` |
| `url.https://H/<Owner>/.insteadOf` (per owner) | `https://H/<Owner>/` |
| `url.https://H/<Owner>/.pushInsteadOf` (per owner) | `https://H/<Owner>/` |
| `http.https://H/.proxy` | `http://127.0.0.1:<git's port>` |
| `http.https://H/.sslCAInfo` | the image's system bundle (`Intercept::bundle`), which carries the member CA |
| `credential.https://H.helper` | empty: resets the helper list |
| `credential.https://H.helper` | `!/run/devsandbox/bin/devsbd credential` |

The owners are `github.com`'s only (the fence knows no other host): of
the folders' recorded remote URLs ([fence](fence.md)), the owner of each
one naming a fenced repo, as that URL spells it (`credentials::git::owners`;
git matches `insteadOf` case-sensitively), so `Org` and `org` both get a
pair. git keeps the longest `insteadOf` match, doesn't chain rewrites, and
matches `pushInsteadOf` against the raw URL in a table of its own, so a
dotfile's `url."git@github.com:".insteadOf = https://github.com/` or
`.pushInsteadOf` loses to the longer identity entry. Ssh host aliases
(`git@github-work:…`) get no rewrite: the record keeps the URL as
written, not what the alias resolved to.

They go on the container's env as git's command scope
(`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>`), read
after every config file, so an equally specific user `http.<url>.*` or
`credential.<url>.*` loses, and only there does the reset drop the
user's global `store`/`cache` helpers (which would otherwise be handed the
phantom on `approve`). `config_env` merges a config's own entries ahead:
those of the image's `ENV`, `containerEnv` and `--env` (each variable's
last value) for the container's `-e`, renumbered from 0, an entry missing
its key or value dropped (git would refuse the whole env), an unparsable
count read as none; then these; then the count. `gc.worktreePruneExpire`
(`gc_keeps_worktrees_env`) extends the count after them. `remoteEnv` and
`--env` are set on every exec over the container's env, so when either
sets one of git's variables, `exec_config_env` merges the same way over
the image's, `containerEnv`'s, `remoteEnv`'s and `--env`'s entries into
`Instance::git_env`, which `Instance::exec_env` applies last.
`CARGO_NET_GIT_FETCH_WITH_CLI=true` goes on the container env too: cargo's
libgit2 fetcher reads only the plain `http.proxy`, then `HTTPS_PROXY`,
the shared port, where git hosts are dialed directly.

### The system copy

For tools that run git with a cleared env, `claims::install_git_config`
writes the same entries as a gitconfig (`git::system_config`, one section
per entry so the order holds) to `/etc/gitconfig.d/devsandbox` and
includes it from `/etc/gitconfig` (`[include] path = …`; git honors
`include` in system scope), both through a temp file and a rename, as
root over exec (`git::SYSTEM_SCRIPT`), at create right after the member
CA's install. An `/etc` that isn't writable, or any failure, is a
warning. In system scope the reset only drops helpers read before it;
the command scope above is what drops the user's.

## The credential helper

`devsbd credential <get|store|erase>` speaks git's credential-helper
protocol on stdin/stdout. `get` with `protocol=https` and a `host` the
broker file lists in a `credential <host> <phantom>` line (a `:443` port
dropped, case-folded) answers `username=x-access-token` and
`password=<phantom>`; anything else, a missing or unparsable broker file
included, answers nothing with exit 0, so git falls through to its next
helper. `store` (after a success) and `erase` (after a failure) are read
and ignored. The phantom comes from the broker file (0644: phantoms are
swapped only for the instance's own calls), never the env, since git's
callers may run under `env -i`. git asks only after the upstream's 401:
a request without the phantom goes upstream uninjected
([broker](broker.md#the-call)).

## The boot check

`devsbd git-check --proxy <url> --ca <path> --host <H>… [--no-signing-key]
-- <dir>…` (`devsbd/src/gitcheck.rs`) prints one warning per line for
what still sends git past the broker; the host prints each as `warning:`.
`claims::git_check` runs it over exec as the exec user with the exec env
(`exec_argv_with`), in the workspace and each `folders` target, only for
an instance with git's port and the helper:

- `run`: after the helper's install and the daemon's start, before the
  lifecycle commands, and again after them (dotfiles land there),
  printing only warnings the first pass didn't.
- `start`: once, after the helper and the CA (cheap: a few git calls).

What it warns of, each warning once:

| Finding | How |
| --- | --- |
| `GIT_SSL_CAINFO`, `GIT_SSL_CAPATH`, `GIT_SSL_NO_VERIFY` set | the process env: they beat every config scope |
| `NO_PROXY`/`no_proxy` covering `H` or `*` | `proxy::bypass`, as devsbd's proxy reads it: git goes direct and the host refuses the phantom |
| a remote's fetch or push URL naming `H` but not `https://H/` (port 443 or none) | `git remote`, then `git remote get-url [--push] --all <remote>`, which apply the rewrites: such a push fails under key filtering with `Permission denied (publickey)` |
| the effective `http.proxy` / `http.sslcainfo` for such a URL isn't ours, or is missing | `git config --get-urlmatch http <url>` with the remote's real URL (a user's `http.https://H/<owner>/.proxy` only shows there); the setting is named from `git config --show-origin --get-regexp '^http\..+\.(proxy|sslcainfo)$'` (its file and key), since `--show-scope` prints `unknown` for `--get-urlmatch` on git 2.39 |
| `commit.gpgsign=true` with `gpg.format=ssh` | only with `--no-signing-key`: the host passes it when the config's `ssh-keys` names no key that signs git (`signing-only`, or one without `destinations`: `claims::signs_git`); `start` without a resolvable config doesn't |

No git in the container, or a folder that isn't a repo, is nothing to
check. A shell rc's env is out of its sight.

## LFS

git-lfs's batch API (`POST /<owner>/<repo>.git/info/lfs/objects/batch`)
rides git's port like any git request, asking the credential helper the
same way. Its answer names where each object goes (`actions.download`,
`upload`, `verify`: an `href` and a `header`), and the header carries
credentials: GitHub's `RemoteAuth` for `lfs.github.com`'s verify, S3
signatures, or, on some hosts, the batch's own `Authorization` echoed,
which after injection holds the real token. The hrefs name storage hosts
(`github-cloud.githubusercontent.com`, S3) that vary per answer and that
no port hands the broker.

A claim pins `lfs` to take part (`claim::Lfs`, in the approved copy, so
it changes the claim's version):

```json
"lfs": { "hosts": ["github.com"], "upload": "git.push" }
```

`hosts` must be among `git.hosts` (so intercept mode only); `upload`
names the scope an `upload` batch needs, since the batch route itself
can't tell an upload from a download. With it, `instance::plan` gives the
instance an LFS listener (`Intercept::lfs`, from `instance::PORTS`, kept
by `rebuild`), a broker port like the others (`instance::ports`,
`forward_refusal`), carried as the broker file's `listen <port> lfs`.

The broker reads a batch's body before policy (`route::lfs_operation`,
at most `route::MAX_LFS_BATCH`; a body that isn't a JSON object naming
`operation` once, as `download` or `upload`, is a 400), so a download is
a read and an upload a write: outside the repo fence a download goes
upstream without a credential (public LFS objects download), an upload
is refused ([broker](broker.md#the-repo-fence)). An upload's scope is
`lfs.upload` instead of the route's. On an injected batch's `200` to a
host `lfs.hosts` names, `lfs::rewrite_batch` takes each action's `href`,
`header` (minus the fields the upstream client writes itself) and expiry
(`expires_in`, else `expires_at`, else `lfs::DEFAULT_TTL`; at most
`lfs::MAX_TTL`) into the broker's `lfs::Store`, under a fresh handle
(32 base62 characters), and rewrites the action to
`http://127.0.0.1:<LFS port>/<handle>` without its `header`, keeping
`expires_*`. An action whose href isn't https only loses its header. A
body that isn't JSON, or over `rewrite::MAX_REWRITE`, passes as it came,
redacted as any response is. A batch sent stripped (outside the fence)
carried no credential and passes untouched.

The store is in memory, per daemon: a handle is the instance's (key,
`instance_id`, incarnation), at most `lfs::MAX_PER_INSTANCE` of them (the
one expiring first makes room). On the LFS listener (`ProxyHeader::Lfs`),
`serve::Service::lfs_request` refuses a browser, a foreign `Host` and a
listener that isn't the instance's, then looks the path's handle up: an
unknown or expired one is a 404, another instance's a 403 logged as a
`leak`, a method other than the action's (`GET` download, `PUT` upload,
`POST` verify) a 403. A found one is sent to its href (address checked
as public, like any upstream) with the stored header replacing the
client's fields of the same name, the client's `Authorization` and
`Cookie` dropped, the body streamed both ways; the response comes back
with the stored header's values (`Accept` and `Content-Type` aside) and
the href's query masked. Each request leaves an `access.log` line with
route `lfs <action>` (`lfs` before the handle resolves), never the href,
the handle or a header value.

git-lfs reaches the loopback href directly: `NO_PROXY` always holds
`127.0.0.1` ([images and features](../instances/images-and-features.md#credential-claims)). `locks` routes are the
claim's own (`GET …/info/lfs/locks` a read, the `POST`s, `locks/verify`
before every push included, writes).

## End to end

`commands::run::tests::git_through_the_broker_end_to_end_with_docker`
runs the whole path: an alpine container with the github test fixture's
claim (its `github.com` marked private, so loopback may be dialed and
the broker answers an uninjected call's 401 itself), whose `github.com`
the broker dials at a local TLS server
(`Service::with_authority`) running the host's `git http-backend` per
request (`http_backend`). The folder's recorded origin puts
`allowed-org/app` in the fence; `other/lib` is outside and anonymous.
git clones and pushes `allowed-org/app` (the broker's 401 challenge, the
helper's phantom, the real token injected), clones `other/lib` stripped
(`devsandbox-fence: outside`), is refused a push to it before the
upstream, and, with `access.github.scopes = ["repo.read"]` written into
the config mid-test, a push to `allowed-org/app`. The upstream never
sees the phantom, and the token only on `allowed-org`; `access.log`
holds neither.
