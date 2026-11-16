# Join tokens: joining a fleet without ssh

Status: explored, not agreed. An alternative to ssh first contact for
[mtls-links](../active/mtls-links.md), which keeps ssh for `machine add`.

Today a host joins by `machine add`, which needs an ssh login on the
target: the install check and copy (`machines/install.rs`), the add-time
probe where the target's fingerprint is confirmed by eye and the logs are
signed ([machines](../../architecture/daemon/machines.md),
[fleet](../../architecture/daemon/fleet.md), *How a machine joins*).
ssh's first-use trust of the target is the root of the check.

## Idea

k3s joins an agent with a server URL and a token; kubeadm's token comes
with `--discovery-token-ca-cert-hash`, so the joiner pins the cluster CA
before trusting anything it's told. Here:

```
# on the authority (or any member, as an introduction)
devsandbox machine token create [--ttl 1h] [--name box]
# on the new host
devsandbox machine join <url> --token <token>
```

## Token shape

`dst1.<id>.<secret>.<fp>`: an id the authority stores, 32 random bytes
of secret (stored hashed), and the fingerprint of the key the joiner must
find at `<url>` (`PublicKey::fingerprint` of the authority's, or the
listening member's, member key: the `discovery-token-ca-cert-hash`
analogue). Single use, a TTL (default 1 h), optionally bound to a member
name. Stored with the authority's pending state (as `proposals.json`),
never in the log: a token isn't membership.

## Flow

1. `join` dials `<url>` over TLS ([mtls-links](../active/mtls-links.md)),
   checking the server's key against the token's fingerprint before
   anything else.
2. It sends its name, its new member key and a proof of the secret (an
   HMAC over both keys and the name, so a token seen on the wire can't be
   used for another key).
3. The authority signs `add {name, key}` and answers the log; a member
   that isn't the authority signs an introduction and proposes it
   (`membership.propose`), exactly the [approvals
   flow](../../architecture/daemon/fleet.md#approvals-and-pushes) a
   `machine add` on a non-authority runs today. A valid token can stand
   for the approval, or still queue it (`machine pending`): a choice.
4. The joiner adopts the log and holds a link to `<url>` as a dialer: a
   laptop needs no listener.

## Install without ssh

`join` runs on a host that already has the binary. Getting it there
without ssh: the release download members already make when they follow
the control plane's version, verified against the embedded release key
([ADR 0047](../../decisions/0047-release-downloads-are-verified-by-signed-checksums.md),
`fleet::update::fetch_verified`). A one-liner install script would carry
the same check, or be the weak link; a dev build has no such path (it
comes over `TRANSFER` once linked).

## Questions

- Worth it next to ssh, which every target here already has? The case is
  hosts with no sshd (a cloud VM image, a container host, Windows).
- Token stands for approval, or approval still asked?
- Who may mint: the authority only, or any member (as an introducer)?
- Revoking an unused token; listing them.
