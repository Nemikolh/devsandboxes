# 0047: Release downloads are verified by signed checksums

Status: accepted. Supersedes 0042's "There is no checksum asset: TLS and
the version check are the verification".

## Context

A member following a published hub (0042) downloads that release's
archive from GitHub and installs it over its own binary. Release builds
are now published (`DEVSANDBOX_PUBLISHED=1` in the release workflow), so
that download path is live on every member of a fleet whose hub runs a
release. TLS only says the bytes came from GitHub, and `fleet-hello`
only says what the binary claims to be. A checksum file published in
the same release would share the archive's trust root: whoever can
replace an asset can replace the checksum file too. What a member needs
is proof that the project's release process made the archive, and that
proof has to come from something the member already holds.

## Decision

- **Signed `SHA256SUMS`.** The release workflow's `release` job writes
  `SHA256SUMS` (`sha256sum`'s format, every archive, sorted basenames)
  and signs it with an ed25519 key: `SHA256SUMS.sig`, the raw 64-byte
  pure-ed25519 signature over the file's bytes (`openssl pkeyutl -sign
  -rawin`). Both are release assets.
- **The key is embedded.** Its public half is `release-signing.pub` at
  the repo root (one line of hex), compiled into the binary
  (`fleet::update::RELEASE_KEY`, parsed at build time) and read by CI,
  which refuses to sign with a secret (`RELEASE_SIGNING_KEY`) whose
  public key differs, and verifies the signature against the committed
  key before publishing.
- **Verify before unpacking.** A member downloads `SHA256SUMS` and its
  signature first, verifies it strictly (`update::verify_sums`), finds
  the asset's one line by exact name (`update::sum_of`), downloads the
  archive and checks its sha256, and only then unpacks it
  (`update::fetch_verified`). Any failure stops the update, saying which
  check failed. There is no path for an unsigned release.
- The `fleet-hello` checks of 0042 stay: the signature says who built
  the archive, the hello says it is the hub's build.

## Consequences

- Replacing a release asset (or the checksums beside it) no longer gets
  code onto members; forging one needs the signing key.
- A member verifies against the key its binary was built with: losing
  or rotating the key means binaries with the old key refuse every
  release signed by the new one, and those members are updated by hand
  (or by a dev-build push) once.
- The signing key is a repository secret. CI writes it to a private temp
  file only for the signing step and removes it; whoever can run the
  release workflow with secrets can sign.
- Releases made before this change have no signature and can't be
  followed by a member that has it.
