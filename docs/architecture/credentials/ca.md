# The member CA

The certificate authority a container trusts for the hosts the broker
intercepts, and the per-host leaves the broker's TLS presents for them.
Read this before touching the CA's files, its name constraints or leaf
lifetimes. The design is
[credential-broker](../../plans/active/credential-broker.md) (*Shape*:
"The member CA", "In the container"). What isn't built yet:
[status](../../status.md#credentials).

Code: `src/credentials/ca.rs` (`MemberCa`, `Leaf`, `check_host`,
`reconcile`, `ca_params`, `SKEW`, `LEAF_LIFETIME`, `LEAF_RENEW`,
`CA_LIFETIME`, `CA_RENEW`), `src/daemon_config.rs` (`MEMBER_CA_KEY`,
`MEMBER_CA_CERT`, `credential_files`).

## Key and cert

One ECDSA P-256 key and self-signed CA cert per member, beside
`member.key` in the data dir. P-256 rather than the member's ed25519
identity key: browsers and other verifiers refuse Ed25519 certificates.
Certs are made with `rcgen` on ring.

- `member-ca.key`: the key, PKCS#8 PEM. `MemberCa::load_or_create` makes
  it on first use, written to a temp file created 0600 and renamed
  (`serve::locked_file::write_atomic_mode`), and never replaces it. A key
  readable by group or others is refused, as is one that isn't P-256.
- `member-ca.toml`: the current cert and what it was issued for, 0644:

  ```toml
  public_key = "04…"          # hex of the raw public key it was issued over
  hosts = ["api.github.com", "github.com"]
  not_after = 2079043200      # unix seconds
  cert = "-----BEGIN CERTIFICATE-----\n…"
  ```

Both are read and written under `member-ca.lock`. Both, and the lock, are
refused as bind and build sources in every container (`key_guard`,
`daemon_config::credential_files`;
[fleet](../daemon/fleet.md), *Key at rest*): the cert is public, but a
container that could write it would get its own CA installed in the
others.

## Name constraints and the host set

The cert is a CA with path length 0, key usages `keyCertSign` and
`cRLSign`, and name constraints: one permitted DNS subtree per host in its
set, and every IPv4 and IPv6 address excluded, so a leaf with an IP SAN
never verifies. A permitted `github.com` also admits its subdomains in a
verifier; `MemberCa::leaf` mints only for hosts in the set.

Hosts are in `check_host`'s form: lowercase DNS labels, no trailing dot,
no wildcard, not an IP address. The broker case-folds a CONNECT target and
drops its trailing dot before asking.

`run` calls `MemberCa::ensure_hosts` with an instance's intercepted hosts
(both proxy ports': `Intercept::bound_hosts`) before its container exists, and `start` again before it reinstalls
([trust in containers](#trust-in-containers)). The broker calls it too
(`broker::serve::Service::leaf`) when an admitted intercepted connection
names a host the cert doesn't cover yet (a cert removed since the run),
one host at a time, and logs that containers trust the new cert only
once it is reinstalled in them ([broker](broker.md#intercept-mode)). It
loads the CA once per daemon, at the first intercepted connection, so a
host that intercepts nothing never makes a CA key.

`MemberCa::ensure_hosts` unions the hosts it is given with the set on disk
(so a second daemon's hosts survive a handoff) and reissues the cert when
the set grows: same key, same subject, new serial. A leaf minted before
still chains to the new cert, which replaces the old one in place. The set
never shrinks: a host no claim intercepts any more stays covered until
`member-ca.toml` is removed. An empty set has no cert (`cert_pem` is
`None`) and mints nothing: a CA without constraints would vouch for every
name.

`reconcile` also reissues, for the hosts the file has, a cert issued over
another key (a replaced `member-ca.key`) or one with less than `CA_RENEW`
(30 days) of its `CA_LIFETIME` (10 years) left, at load or at the next
`ensure_hosts`.

## Leaves

`MemberCa::leaf(host)` returns a `Leaf`: the chain the server presents
(the leaf alone; containers hold the CA as an anchor) and its key. Each
leaf has a fresh P-256 key, SAN and common name `host`, EKU `serverAuth`,
key usage `digitalSignature`, a random 127-bit serial and an authority key
identifier. It is valid from `SKEW` (1 day) before it is minted, for a
container whose clock lags the host's, to `LEAF_LIFETIME` (7 days) after.

Leaves are cached in memory per host and minted again once less than
`LEAF_RENEW` (1 day) is left; the cache dies with the daemon. The broker
presents a leaf only to a ClientHello whose SNI is its host
([broker](broker.md#intercept-mode)).

## Trust in containers

Code: `src/credentials/trust.rs` (`BUNDLES`, `PROBE_SCRIPT`,
`INSTALL_SCRIPT`, `install_input`, `fingerprint`),
`src/commands/run/claims.rs` (`member_cert`, `install_trust`,
`reinstall_running`, `refresh_trust`).

An instance with a shared proxy port (an `intercept` claim;
[images and features](../instances/images-and-features.md#credential-claims))
trusts the CA through its system bundle, the file the CA variables on its
env name (`Intercept::bundle`, probed from the image at create). The
install is `trust::INSTALL_SCRIPT` exec'd as root with `PATH` pinned, the
bundle as `$1` and on stdin the CA's PEM followed by the Mozilla set
(`webpki-root-certs`). Per distro:

| Found in the container | Anchor written | Then |
| --- | --- | --- |
| `update-ca-trust` (Fedora, RHEL) | `/etc/pki/ca-trust/source/anchors/devsandbox-member.crt` | `update-ca-trust extract` |
| `update-ca-certificates` and `/etc/pki/trust/anchors` (openSUSE) | `/etc/pki/trust/anchors/devsandbox-member.crt` | `update-ca-certificates` |
| `update-ca-certificates` (Debian, Ubuntu, Alpine with `ca-certificates`) | `/usr/local/share/ca-certificates/devsandbox-member.crt` | `update-ca-certificates` |
| no tool, a bundle (Alpine's `ca-certificates-bundle`) | `/usr/local/share/ca-certificates/devsandbox-member.crt` | the CA appended to the bundle between `# devsandbox member CA` markers, an earlier copy replaced |
| no tool, no bundle (`debian:*-slim`) | as above | the bundle written: the Mozilla set plus the CA between the markers, never the CA alone |

The anchor sits where the distro's tool reads it, so a later regeneration
(a package install running the tool) keeps the CA. The bundle path the
env names is the bundle itself, never a snapshot, so a CA added to it
later still counts. A bundle that still doesn't hold the CA's first
base64 line afterwards fails the install.

When it runs:

- **Create** (`materialize`): after the broker file is written, before the
  account fit and lifecycle commands. The cert's `trust::fingerprint` is
  saved as the instance's `Intercept::installed`.
- **A reissue at `run`**: when `ensure_hosts` grew the set,
  `reinstall_running` installs the new cert into every other running
  instance with an `intercept` whose `installed` differs, and records it.
  A stopped one waits for its `start`.
- **`start`** (`refresh_trust`, after the helper, before
  `postStartCommand`): `ensure_hosts` with the instance's hosts, then an
  install when `installed` isn't the current cert (reissued meanwhile, or
  a failed install); a set this grows reaches the running instances too.

Every failure is a warning naming the hosts whose TLS fails there; `run`
and `start` go on.
