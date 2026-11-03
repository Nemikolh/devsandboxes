# 0046: Routed streams are end-to-end sessions between their two daemons

Status: accepted. Supersedes 0045's consequence that a relay sees and can
alter a routed stream's bytes after its opening.

## Context

A routed stream (0045) proves its origin and its destination to each
other at its opening, but its bytes after the opening cross every relay
in the clear: a relay can read whatever the stream carries, change it,
inject into it or replay it, and so drive the API calls the origin
makes on the destination. Relaying isn't access (0045), so a relay must
carry a stream without being able to touch it.

## Decision

- **SIGMA over the opening, no extra round trip.** The destination's
  challenge carries a fresh ephemeral x25519 key and the origin's proof
  its own, in place of 0045's nonces (`fleet::session::Ephemeral`,
  `fleet::route::origin_side`, `route::dest_side`). Both ends sign the
  transcript holding both keys with their ed25519 member keys, as
  before, in the domain `devsandbox/route/v2` (`route::DOMAIN`): no
  static DH key and no key conversion. A relay that swaps an ephemeral
  key breaks the signature over it; a DH output that isn't contributory
  (a low-order key) is refused.
- **The crates.** `x25519-dalek`, `chacha20poly1305` and `hkdf`, all on
  the `curve25519-dalek` and `digest` that `ed25519-dalek` uses; no
  Noise crate.
- **Per-direction keys.** HKDF-SHA256 with the transcript's SHA-256 as
  salt and the DH output as input, expanded once per direction with the
  direction in the info (`session::Keys::derive`), so a record reflected
  back to its sender doesn't open.
- **Records.** A `u16` length and a ChaCha20-Poly1305 ciphertext of at
  most 16 KiB of plaintext (`session::MAX_RECORD`), its nonce the
  direction's record counter, its type (data or end) the associated
  data (`session::Sealer`, `session::Opener`). Each direction ends with
  an authenticated end record; a bare EOF, a bad tag or a record out of
  order tears both directions down, never a half-close. Each read is
  sealed and sent at once: `PTY` and `TUNNEL` are interactive.
- **The ends are the daemons.** The origin's `Shared::serve_routed`
  pumps between its client's connection and the stream; the
  destination's `Shared::serve_route` between the stream and a socket
  pair whose other end the inner channel's handler serves
  (`session::pump`). Handlers and relays don't change.
- **Direct links stay on ssh.** A direct link's streams carry no
  session; the ssh transport already secures them.

## Consequences

- A relay carries ciphertext only: it can't read, change, inject into,
  replay, reflect or redirect a routed stream; tampering ends the stream
  at both ends.
- An open routed stream costs both daemons a pump (a thread per
  direction) for its life, and each record 18 bytes of framing and tag.
- The opening's wire format changed with its domain; both ends run the
  same version.
- A relay can still drop a stream or delay it, and sees its timing and
  sizes.

Source: docs/plans/active/fleet.md, step 10c and decisions 61-64.
