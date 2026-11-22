# 0058: A broad token is the home's opt-in

Status: accepted (2026-11-22).

## Context

Users in orgs where they can't install a GitHub App have two ways to give
the broker a GitHub credential: a fine-grained PAT made by hand, or gh's
own OAuth login (GitHub CLI's OAuth App), whose `gho_` token reaches
every repo the account can. The repo fence refuses writes outside it,
but GraphQL queries aren't fenced, so a broad token lets a sandbox read
every private repo the account can.

## Decision

- **The claim declares, the home opts in.** A feature's claim lists the
  prefixes it accepts only from a broad credential (`tokens.broad`) and
  marks the logins that yield one (`broad: true`, with their `scope`);
  only `devsandbox login --broad` makes a credential broad (`Entry.broad`
  in `credentials.toml`), after a confirmation naming its reach. A
  feature can't widen what the home accepts.
- **Broadness is the credential's, fixed at its first login**: changing
  it is a file edit, like `profiles`.
- **gh's public client id lives in the claim**, not in devsandbox.

## Consequences

- The narrow paths stay the default: the paste login opens a prefilled
  fine-grained PAT page, and every login checks whose token it is.
- A broad credential shares GitHub CLI's per-user token limit with the
  user's `gh auth login`s, which can revoke each other's oldest tokens.
