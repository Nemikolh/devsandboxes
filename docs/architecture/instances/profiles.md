# Profiles

Named collections of sandboxes: a config declares profiles under
`[profiles.*]`, and each sandbox lists the ones its instances may be in.
Read before touching profile declaration or membership. Field reference:
[config-toml-spec](../../../skills/config-toml-spec/SKILL.md).

Code: `src/config.rs` (`Config::profiles`, `Profile`, `check_profile_name`,
`Config::validate`, `SandboxProperties::profiles`, `Config::resolve_sandbox`,
`Config::allowed_profiles`, `sandbox_hash`).

## Declaration

`Config::profiles` maps each `[profiles.<name>]` table to a `Profile`. The
struct has no fields and `deny_unknown_fields`, so any key inside a
profile table is a parse error. `Config::validate`, run by both
`Config::parse` and `Config::parse_yaml`, checks every name with
`check_profile_name`: `[a-z0-9][a-z0-9_.-]*`. A bad name or key fails the
whole file, so every command that loads the config reports it.

## Membership

`SandboxProperties::profiles` is the sandbox's `profiles` array, in
preference order. It goes through `extends` like any array
(`deep_merge` concatenates, base first), so a template that sets it acts as
the collection and a sandbox can add to it. `resolve_sandbox` then drops
repeated names, keeping each one's first position, and fails on a name with
no `[profiles.<name>]` (including when the config declares none). Like
`forwardPorts`, this check runs whenever a sandbox resolves, not at load.

`Config::allowed_profiles` is a resolved sandbox's set: its `profiles` if
set, else every declared profile in name order (the `BTreeMap`'s), empty
when the config declares none.

## Drift

`sandbox_hash` strips `profiles`: membership never changes the container,
so editing it is not drift ([config.md](../config.md#hashing)).
