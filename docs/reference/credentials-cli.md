# `devsandbox login` and `devsandbox access`

Store a credential's secret on this host and approve the claim versions
it serves, and the two files that keep them. Read when changing these
commands or the home's files; the design they implement is
[credential-broker](../plans/active/credential-broker.md) (*Shape*,
"`credentials.toml`") and
[github-and-sentry-access](../plans/active/github-and-sentry-access.md)
(*The claim contract*). Unix only (the files take `serve::locked_file`'s
write discipline). Local only: both run in-process on the host whose
credentials they are, never on a machine (`--machine` refuses them).

Code: `src/main.rs` (`Command::Login`, `AccessCommand`),
`src/commands/login.rs` (`login_at`, `pick`, `pending_one`, `Term`,
`EchoOff`, `approve_or_refuse`), `src/commands/access.rs` (`approve_at`,
`pick_pending`), `src/credentials/home.rs` (`Entry`, `Source`, `Version`,
`record_pending`, `update`), `src/credentials/store.rs` (`Store`,
`Secret`), `src/credentials/review.rs` (`View`, `diff`, `Diff`),
`src/credentials/mod.rs` (`check_name`), `src/broker/audit.rs` (`Audit`,
`Line`).

```
devsandbox login <credential> [--feature <ref>] [--kind <kind>] [--profile <p>]... [--yes]
devsandbox access approve <credential> [--version <v>] [--yes]
```

## Names and kinds

A credential has a name (`sentry-work`, `check_name`:
`[a-z0-9][a-z0-9_-]*`, at most 64 bytes) and serves one claim kind
(`sentry`, `claim::check_kind`). A feature's claim and a config's
`access.<kind>` name the kind; `credentials.toml` and
`access.<kind>.credential` name the credential, so one host can hold one
credential per Sentry org, each granted to its own profiles.

## `login`

The claim to approve (`login_at`):

- `--feature <ref>`: the claims in that feature's metadata
  (`features::resolve`; an OCI ref, or a local folder written as a config
  writes one, `./…`, `../…`, `/…`, resolved against the current dir).
  `pick` takes the claim of `--kind`, else the feature's only one, else
  the one whose kind is the credential's name; else it refuses, listing
  the kinds.
- Without it (`pending_one`): the credential's one pending version.
  Several pending: refused, naming `--feature` and `access approve
  --version`. None pending but one approved: no claim to approve (a
  re-login). No such credential, or one with no version at all: refused,
  naming `--feature`.

An existing credential keeps its kind (another kind is refused) and its
grant: `--profile` is refused on it (edit `profiles` in the file). A
claim version already approved for the credential is not asked about
again.

The login method is the claim's (`View::paste`): `paste` only; a claim
listing only other methods is refused ("not yet (credential-broker step
2)"), and one with no `login` (no `inject` slot) has no secret to log in.

Then, in order: a new version's summary (`View::summary`: kind, mode,
upstreams, each scope with its route count, every `never` route, the
short version) and the question (`approve_or_refuse`: `--yes` approves;
off a terminal without `--yes`, refused); the secret from stdin
(`Stdio::secret`: on a terminal, after the claim's paste `hint`, with echo
off (`EchoOff`, termios `ECHO` cleared, line editing kept); else all of
stdin, so `login x < file` works), trimmed, empty refused; the secret into
the store (first: an approved claim with no secret serves nothing); the
entry (created with `--profile`s as `profiles`, none: unset) and the
approved version into `credentials.toml`. It prints `logged in …` with
the approved version, or `secret replaced`.

## `access approve`

`approve_at`: the credential's pending version (`pick_pending`: `--version`
as a unique prefix, else the only one; none or several refused), shown as
its diff against the latest approved version (`review::diff`, rendered by
`Diff::render`: upstreams `+`/`-`, routes added and removed per scope,
`(new)` and `(dropped)` scopes, `never` routes `+`/`-`, and `also
changed:` naming every other pinned field that differs, `mode`, `inject`,
`login`, `refusal`, a kept scope's `permissions` among them), or its
summary when none is approved. Same question as `login`. Approval moves
the version from `pending` to the end of `approved`; older approved
versions stay (instances pinned to them keep working). The secret is
untouched.

Both commands work on the files directly, in-process; no API method
reaches them.

## `credentials.toml`

Next to `state.toml` (`home::path`, `daemon_config::CREDENTIALS`), read and
written under `credentials.lock` (`home::update` over
`serve::locked_file::update`; a failed update writes nothing). One table
per credential, unknown keys refused:

```toml
[sentry-work]
kind = "sentry"
source = "file"               # the only source; "keyring" and "op://…" refused as not yet
profiles = ["work"]           # unset: every profile
max = ["releases.write"]      # unset: every scope of the claim (the broker's grant check)
members = []                  # these four: parsed, read by nothing
approvers = []
delegate = []
as = []

[[sentry-work.approved]]      # approval order: the last is the latest
version = "3f2a…"             # sha256 of `claim`, as `Claim::version`
claim = '{"kind":"sentry","claim":{…}}'   # `Claim::canonical`

[[sentry-work.pending]]       # `home::record_pending`
version = "…"
claim = '…'
```

`claim` is the canonical pinned claim (no container-side wiring), the copy
the broker enforces. At load (`Entry::check`, `Version::check`) every
version is re-hashed and must match its `version`, and its claim's kind
must be the entry's: a hand edit to a claim can't approve content nobody
was shown. `record_pending` takes canonical JSON (re-hashed here) for an
existing credential, and skips a version already approved or pending;
`run` calls it for a claim whose credential hasn't approved its version,
then refuses
([images and features](../architecture/instances/images-and-features.md#credential-claims)).
`home::resolve` picks the credential a claim takes: the one
`access.<kind>.credential` names, else the one of that kind whose
`profiles` grant the instance's profile.

## The file store

`credentials-store.toml` beside `member.key`
(`daemon_config::CREDENTIAL_STORE`), the secrets by credential name
(`[secret]` table), for `source = "file"`. `Store::put` / `get` /
`remove` read and write it under `credentials-store.lock`, each write to a
temp file created 0600 and renamed (`serve::locked_file::write_atomic_mode`),
so no secret is ever on disk with wider permissions. A store readable by
group or others is refused (read and write), and a parse error names the
file, never its content. `Secret` has no `Display` and a blank `Debug`.

Both files, their locks, and the reserved `access.json` and
`member-ca.key` are refused as bind and build sources in every container,
like `member.key` (`key_guard`, `daemon_config::credential_files`;
[fleet](../architecture/daemon/fleet.md), *Key at rest*).

## `access.log`

The broker's audit log, next to `state.toml` (`broker::audit::Audit`,
0600): one JSON object per line, one line per request a broker listener
carried, rotated to `access.log.1` (replacing it) once the next line would
take the file past 10 MiB. Never a token or a phantom.

```json
{"time":"2026-11-21T10:00:00Z","instance":"web-1","instance_id":"web-1",
 "incarnation":"…","kind":"sentry","credential":"sentry-work","method":"GET",
 "route":"/api/0/organizations/acme/releases/","scope":"releases.read",
 "verdict":"allow","reason":null,"status":200,"phantom":"own"}
```

| field | holds |
|---|---|
| `time` | RFC 3339, UTC, seconds |
| `instance`, `instance_id` | the bridge's instance (state key, persistent id) |
| `incarnation` | the instance's, when its record has one |
| `kind`, `credential`, `scope` | as far as policy got (`null` before) |
| `method`, `route` | the request's, the path before injection, query dropped |
| `verdict` | `allow` (injected), `uninjected`, `refused`, `leak`, `error` |
| `reason` | a refusal's or failure's one line; a skipped `rewrite` |
| `status` | what the tool got (the upstream's, or the broker's own) |
| `phantom` | whose phantom the call carried: `own`, `other-claim`, `foreign`; `null` for none |

How calls are decided: [broker](../architecture/credentials/broker.md).
