# 0048: A profile is the user's choice, and only its name crosses machines

Status: accepted (2026-11-10)

## Context

An instance's profile (`work`, `personal`) picks how its URLs open on
the user's desktop, and later its credentials and git permissions: it is
a grant. A container opening a URL is untrusted input, and once machines
are involved the host that runs the instance isn't the one the user sits
at: a sandbox on the always-on server, attached from the laptop, has to
open on the laptop. So a URL, and what to open it with, travels between
fleet members, and a member is as trusted as `member.toml` says, which
may be not much (`worker`). Neither the container nor any member may be
able to run a command on another host, move an instance into a profile,
or put pages on a desktop whose user never reached that instance.

## Decision

- **The user chooses the profile.** At creation (`run --profile`, else
  the sandbox's `last_profile`, else its first allowed) and at runtime
  (`instances.setProfile`, `devsandbox profile`, the dashboard), checked
  against the sandbox's allowed set. No control request (`devsbd`'s
  `ctl.rs`, `commands::dispatch`) sets or names one, so a container can
  never move itself into a profile.
- **Only the name travels.** The instance's host decides which name an
  open carries (`open::profile_to_send`: none once the config no longer
  allows it) and sends that name alone (`open.deliver`'s `profile`). The
  command always comes from the opening host's own `profiles.toml`
  (`open::open_argv`); the receiver checks the name is a profile name and
  maps it, or opens with its system opener. No config and no member ever
  names a command for another host.
- **The opening host opens only what its user reached.** A member takes
  `open.deliver` only for an instance it attached to itself on the
  sender, live or ended less than `open::OPEN_ATTACH_WINDOW` (10 min)
  ago (`Attaches::attached_out`), under `worker` and `full` trust alike;
  it re-applies the URL policy (`http(s)` only, a rate limit per sender
  and instance). It knows its attaches without asking anyone, so no
  member can drive another's desktop by naming an instance.
- **The last attach decides, with no fallback.** The instance's host
  opens where the latest attach came from (`serve::opens::route`). When
  delivery to that member fails, the open is logged and dropped, not
  opened on the instance's host: the user is at that member.

## Consequences

- A profile means the same thing everywhere only by name: each host maps
  it in its own `profiles.toml`, and an unmapped one opens with the
  system opener there.
- An attach is what lets a member's desktop be used, so the window is
  the exposure: for 10 min after its user's last attach, the sender can
  open (rate-limited) `http(s)` URLs there for that instance.
- An open whose member is unreachable is lost rather than opened where
  nobody looks; the user sees it in `serve.log` (and, later, the
  dashboard's status line).
- The receiver checks the instance by the name the sender gives now
  against the name its user attached by, so a rename in between makes it
  refuse until the user attaches again.
