# gh: the github claim and the tests that hold it

What `features/gh` claims and how devsandbox checks that claim against the
gh it installs and against GitHub's GraphQL schema. Read this before
changing the claim's scope table, `fence` or `never` list, or moving to
another gh release. The design is
[github-and-sentry-access](../../plans/active/github-and-sentry-access.md)
(*The claim contract*, *gh*, *The installer itself*); how the broker
applies a claim is [broker](broker.md), the fence it checks is
[fence](fence.md), git's side is [git](git.md). What isn't built yet:
[status](../../status.md#credentials).

Code and data: the claim in `github.com/devsandboxes/features`'
`src/gh/devcontainer-feature.json`, mirrored by
`tests/fixtures/features/github/devcontainer-feature.json` (same metadata
and claim, a no-op `install.sh`; keep them in step by hand),
`tests/fixtures/gh/routes.json`, `tests/fixtures/gh/schema-extract.json`,
`scripts/gh-schema-extract.py`, `src/broker/gh_claim.rs` (test-only),
`src/commands/run/mod.rs` (`gh_e2e_child`, `fake_github_api`,
`GH_VERSION`).

## The claim

Kind `github`, intercept mode on `api.github.com`, `uploads.github.com`
and `github.com` (git's own proxy port, `git.hosts`), injected as
`Authorization: token …` or git's basic `x-access-token`. The phantom is
`GH_TOKEN` and `~/.config/gh/hosts.yml`; `GITHUB_TOKEN` isn't set, so
tools that only read it stay unauthenticated rather than pick up a
phantom the plan never named. `tokens` takes `github_pat_` and `ghu_`,
acting as `user` or `app`; `check` is `GET /user`; refusals are GitHub's
JSON `message`. `login` is paste only: a `device` method needs a client id
in the claim, and no app ships with the feature (a client id given only
at `login --client-id` has nothing to override).

Scopes, each with the app permissions it needs:

| scope | covers |
|---|---|
| `api.read` | every GraphQL query, `GET /`, `/rate_limit`, `/meta` |
| `user.read` | `GET /user` |
| `search.read` | repo-less reads: `/search/*`, `/notifications`, received events, `GET /gists/{id}`, licenses, gitignore templates |
| `repo.read` | git fetch and LFS downloads, `/repositories/{id}`, REST reads under `/repos/{repo}/…` (contents, refs, tags, trees, issues, pulls, comments, releases and assets, labels, milestones, collaborators, rulesets, attestations) |
| `actions.read` | runs, jobs, logs, artifacts, workflows, caches, check runs, statuses |
| `actions.write` | rerun, cancel, delete runs; dispatch, enable, disable workflows; delete caches |
| `pr.write` | REST and GraphQL pull request writes, reviews and review requests |
| `pr.merge` | `PUT …/merge`, `mergePullRequest`, auto-merge |
| `issues.write` | issue, comment, label and assignee writes (shared with PRs: those are issues too) |
| `discussions.write` | discussion mutations |
| `git.push` | receive-pack, LFS locks, ref writes (REST `git/refs`, `createRef`/`updateRef`/`deleteRef`, `createCommitOnBranch`, `updatePullRequestBranch`, `createLinkedBranch`), `merge-upstream`, forks |
| `releases.write` | release and asset writes, uploads included |

A route variable takes one path segment, so paths past the repo that gh
sends with `%2F` (`contents/.github%2Fworkflows%2Fci.yml`,
`git/refs/heads%2Fuser%2Fx`) are listed at each depth gh needs: contents
up to four segments, branch names with up to one `/`.

Repo settings (`PATCH /repos/{repo}`, topics, archive, delete,
autolinks) take the `administration` permission, which no scope maps
to, so they are `other`; so are repo creation, Actions secrets reads and
variables, gist writes, account keys and Projects. `never` holds deploy
keys, account ssh/GPG/signing keys, repo and org webhooks, collaborator
and org-member invitations, runner registration and JIT tokens, Actions,
Dependabot, Codespaces and environment secret writes, and the
installation, OAuth-token and authorization routes. `presence` names tag
refs, ref deletions, REST `git/refs`, releases (by `tag_name` or id) and
`createRef`/`updateRef`/`deleteRef`.

`fence`: every mapped mutation lists each input naming a repository or
an object in one (`input.repositoryId`, `input.pullRequestId`,
`input.labelIds[]`, `input.agentAssignment.targetRepositoryId`, …);
`GET /repositories/{id}` its `{id}`; `POST /repos/{repo}/forks` its body's
`organization` (refused until REST body inputs are placed). Inputs naming
an actor (`assigneeIds`, `userIds`, `teamIds`, `botIds`, `actorIds`) or
an owner's issue metadata (`issueTypeId`, `statusId`, issue field ids)
aren't fenced: no repository holds them (`NOT_IN_A_REPO` in
`src/broker/gh_claim.rs`).

## The route table

`tests/fixtures/gh/routes.json` lists, per gh command of the release
`gh` names, the calls it makes: REST request lines with sample values
(`acme/app`, gh's own `%2F` encoding), `graphql query` and `graphql
mutation <name>`. `commands` must each land in a scope
(`every_gh_call_is_placed`, through `broker::route::scope_of`), with
`fence` inputs for each mutation; `outside` lists what stays out of the
table on purpose, with why, and must land in `other` or `never`; `never`
must land in `never`. `every_never_route_is_unreachable` instantiates
each `never` route, and
`every_scoped_route_matches_itself_and_presence_and_fence_name_scoped_routes`
checks no scope's route is shadowed by another's.

It is derived from gh's source at that tag (`github.com/cli/cli`): REST
paths from every `safeurl.JoinPath` call under `api/`, `pkg/` and
`internal/` with the HTTP method of the request that follows it, plus
the URLs gh follows from responses (release `url`/`upload_url`, artifact
`archive_download_url`, logs); mutations from every `graphql:"<name>(input…"`
tag and `<name>(input: $input)` document. To move to another release:
take its source, re-run that extraction (a `grep`/script over
`safeurl.JoinPath(` and `(input`), diff against the table, place each new
call in the claim or under `outside` with a reason, bump `gh` here and
`GH_VERSION` with the release's checksums (`gh_<version>_checksums.txt`)
in `src/commands/run/mod.rs`.

## The schema walk

`tests/fixtures/gh/schema-extract.json` is GitHub's public schema
(`https://docs.github.com/public/fpt/schema.docs.graphql`, from
`github/docs`, CC BY 4.0) trimmed to each mapped mutation's arguments
and every input object reachable from them. Regenerate it whenever the
claim maps another mutation (the test fails until then):

```sh
curl -fsSLO https://docs.github.com/public/fpt/schema.docs.graphql
scripts/gh-schema-extract.py schema.docs.graphql \
  tests/fixtures/features/github/devcontainer-feature.json > tests/fixtures/gh/schema-extract.json
```

`every_mapped_mutation_fences_each_input_naming_a_repo` walks each
mutation's inputs, nested input objects and lists included, and fails on
an input typed `ID`, named `…RepositoryId`, `ownerId` or
`…NameWithOwner`, that its `fence` doesn't name (actors and issue
metadata aside), on an input object holding both `owner` and `name` (no
`fence` input can place that pair), and on a `fence` input the schema
doesn't have or that isn't an `ID` or `String`.

## End to end

`gh_through_the_broker_end_to_end_with_docker` runs the pinned gh
(`GH_VERSION`, its linux tarball checksummed at image build) in a debian
image under the github fixture's claim, with `api.github.com` marked
private and dialed at a loopback mock (`fake_github_api`), the folder's
origin `allowed-org/app` its fence: `gh auth status` exits 0, `gh api`
reads `allowed-org/app` with the token and `other/lib` stripped
(`devsandbox-fence: outside`), a `createPullRequest` naming the fenced
repo's node id passes and one naming `other/lib`'s is refused with the
broker's reason before reaching the mock. The mock sees the token only on
calls inside the fence (gh's `token …`, the home's id lookups' `Bearer
…`), never the phantom.

## The installer

`src/gh/install.sh` bakes gh into `/usr/local/lib/gh` through
`gh-update.sh --bake` (release tarball, checked against the release's
checksums) and installs `gh-launcher.sh` as `/usr/local/bin/gh`. The
launcher runs `<dir>/<current>/gh` from `/var/lib/devsandbox/gh/bin` when
a sandbox mounts that dir and can't write it, else the baked gh.
`gh-update.sh <dir> [version]` runs on the host: the dir is a
`shared-volumes/` bind the config mounts read-only (the devcontainer
feature schema has no read-only mount, so the feature declares none).
