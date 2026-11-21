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
`LEAF_RENEW` (1 day) is left; the cache dies with the daemon.
