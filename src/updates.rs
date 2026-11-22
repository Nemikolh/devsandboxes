//! Feature updates: a feature's `customizations.devsandbox.updates` (a
//! command baked in its image that refreshes a `${sharedVolumes}` dir its
//! sandboxes mount read-only) and the config's per-update intervals.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{parse_shorthand, Mount};

/// A feature's `every` when it sets none.
pub const DEFAULT_EVERY: Duration = Duration::from_secs(7 * DAY);

const HOUR: u64 = 60 * 60;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;

const MAX_NAME: usize = 32;

const SHARED_VOLUMES: &str = "${sharedVolumes}/";

/// One declared update, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))] // `every`: read by the daemon, unix-only
pub struct Update {
    pub name: String,
    /// The target of the feature's `${sharedVolumes}` bind the command writes.
    pub mount: String,
    pub command: Vec<String>,
    pub every: Duration,
}

/// A config `updates` value: `"never"` or a duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))] // read by the daemon, unix-only
pub enum Interval {
    Never,
    Every(Duration),
}

impl Interval {
    pub fn parse(s: &str) -> Result<Self> {
        if s == "never" {
            return Ok(Interval::Never);
        }
        parse_duration(s).map(Interval::Every)
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Interval::parse(&s).map_err(|e| serde::de::Error::custom(format!("{e:#}")))
    }
}

/// `<n>h`, `<n>d` or `<n>w`, `n` ≥ 1.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let bad = || anyhow::anyhow!("`{}` isn't a duration (`<n>h`, `<n>d` or `<n>w`, e.g. `7d`)", s.escape_debug());
    let unit = match s.chars().last() {
        Some('h') => HOUR,
        Some('d') => DAY,
        Some('w') => WEEK,
        _ => return Err(bad()),
    };
    let digits = &s[..s.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = digits.parse().map_err(|_| bad())?;
    if n == 0 {
        return Err(bad());
    }
    n.checked_mul(unit).map(Duration::from_secs).ok_or_else(bad)
}

/// `[a-z0-9][a-z0-9-]*`, at most 32 bytes: an update's name, the config's
/// `updates` key and the CLI's argument.
pub fn check_name(name: &str) -> Result<()> {
    let lower_alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let ok = name.len() <= MAX_NAME
        && name.bytes().next().is_some_and(lower_alnum)
        && name.bytes().all(|b| lower_alnum(b) || b == b'-');
    if !ok {
        bail!(
            "update name `{}`: lowercase letters, digits and `-`, starting with a letter or digit, at most {MAX_NAME}",
            name.escape_debug()
        );
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUpdate {
    name: String,
    mount: String,
    command: Vec<String>,
    every: Option<String>,
}

/// The feature's `customizations.devsandbox.updates`, each checked against
/// the feature's own `mounts`. `feature` names it in errors.
pub fn from_customizations(feature: &str, customizations: Option<&Value>, mounts: &[Mount]) -> Result<Vec<Update>> {
    let updates = match customizations.and_then(|c| c.pointer("/devsandbox/updates")) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(updates) => updates,
    };
    let Some(list) = updates.as_array() else {
        bail!("feature `{feature}`: `customizations.devsandbox.updates` must be a list");
    };
    let mut parsed: Vec<Update> = Vec::new();
    for (i, value) in list.iter().enumerate() {
        let label = match value.get("name").and_then(Value::as_str) {
            Some(name) => format!("update `{name}`"),
            None => format!("update #{}", i + 1),
        };
        let update = parse(value, mounts).with_context(|| format!("feature `{feature}`: {label}"))?;
        if parsed.iter().any(|u| u.name == update.name) {
            bail!("feature `{feature}`: update `{}` is declared twice", update.name);
        }
        parsed.push(update);
    }
    Ok(parsed)
}

fn parse(value: &Value, mounts: &[Mount]) -> Result<Update> {
    let raw: RawUpdate = serde_json::from_value(value.clone())?;
    check_name(&raw.name)?;
    check_mount(&raw.mount, mounts).context("`mount`")?;
    match raw.command.first() {
        None => bail!("`command` is empty"),
        Some(program) if !program.starts_with('/') => {
            bail!("`command`: `{}` must be an absolute path", program.escape_debug())
        }
        Some(_) => {}
    }
    let every = match &raw.every {
        None => DEFAULT_EVERY,
        Some(s) => parse_duration(s).context("`every`")?,
    };
    Ok(Update { name: raw.name, mount: raw.mount, command: raw.command, every })
}

/// `target` must be one of the feature's `mounts`, a read-only bind of a dir
/// under `${sharedVolumes}`: the only dir the update writes, and one no
/// sandbox can write.
fn check_mount(target: &str, mounts: &[Mount]) -> Result<()> {
    let found = mounts.iter().find_map(|m| {
        let (kind, source, t, readonly) = match m {
            Mount::Shorthand(s) => parse_shorthand(s).ok()?,
            Mount::Object(o) => (o.kind.clone(), o.source.clone(), o.target.clone(), o.readonly.unwrap_or(false)),
        };
        (t == target).then_some((kind, source, readonly))
    });
    let Some((kind, source, readonly)) = found else {
        bail!("`{}` isn't the target of one of the feature's `mounts`", target.escape_debug());
    };
    let shared = source.as_deref().and_then(|s| s.strip_prefix(SHARED_VOLUMES)).is_some_and(|rest| {
        !rest.is_empty() && rest.split('/').all(|seg| seg != "..")
    });
    if kind.as_deref().unwrap_or("bind") != "bind" || !shared {
        bail!("`{}` must be a bind of a dir under `${{sharedVolumes}}`", target.escape_debug());
    }
    if !readonly {
        bail!("`{}` must be mounted read-only", target.escape_debug());
    }
    Ok(())
}

/// Update names are one namespace across a sandbox's features (the config
/// and the CLI name an update alone): refuse a name two features declare.
pub fn check_unique(features: &[(&str, &[Update])]) -> Result<()> {
    let mut seen: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for (feature, updates) in features {
        for update in *updates {
            if let Some(other) = seen.insert(&update.name, feature)
                && other != *feature
            {
                bail!("update `{}` is declared by both `{other}` and `{feature}`", update.name);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mounts() -> Vec<Mount> {
        serde_json::from_value(json!([
            {"type": "bind", "source": "${sharedVolumes}/gh-bin", "target": "/var/lib/gh/bin", "readonly": true},
            "type=bind,source=${sharedVolumes}/tool,target=/opt/tool,readonly",
            {"type": "bind", "source": "${sharedVolumes}/rw", "target": "/rw"},
            {"type": "bind", "source": "/etc/hosts", "target": "/host"},
            {"type": "volume", "source": "${sharedVolumes}/v", "target": "/vol", "readonly": true},
            {"type": "bind", "source": "${sharedVolumes}/../x", "target": "/up", "readonly": true},
        ]))
        .unwrap()
    }

    fn custom(updates: Value) -> Value {
        json!({"devsandbox": {"updates": updates}})
    }

    fn parse_one(update: Value) -> Result<Vec<Update>> {
        from_customizations("gh", Some(&custom(json!([update]))), &mounts())
    }

    fn err(update: Value) -> String {
        format!("{:#}", parse_one(update).unwrap_err())
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(2 * 86400));
        assert_eq!(parse_duration("1w").unwrap(), Duration::from_secs(7 * 86400));
        for bad in ["", "h", "0d", "1", "1m", "1.5d", "+1d", "-1d", " 1d", "1d ", "99999999999999999999w", "1H"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Interval::parse("never").unwrap(), Interval::Never);
        assert_eq!(Interval::parse("3h").unwrap(), Interval::Every(Duration::from_secs(3 * 3600)));
        assert!(Interval::parse("Never").is_err());
    }

    #[test]
    fn names() {
        for ok in ["gh", "0", "a-b-1", &"a".repeat(32)] {
            check_name(ok).unwrap();
        }
        for bad in ["", "-a", "A", "a_b", "a.b", &"a".repeat(33)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parses_with_default_every() {
        let updates = from_customizations(
            "gh",
            Some(&custom(json!([
                {"name": "gh", "mount": "/var/lib/gh/bin", "command": ["/usr/local/lib/gh/gh-update", "/var/lib/gh/bin"]},
                {"name": "tool", "mount": "/opt/tool", "command": ["/bin/up"], "every": "1d"},
            ]))),
            &mounts(),
        )
        .unwrap();
        assert_eq!(
            updates,
            [
                Update {
                    name: "gh".into(),
                    mount: "/var/lib/gh/bin".into(),
                    command: vec!["/usr/local/lib/gh/gh-update".into(), "/var/lib/gh/bin".into()],
                    every: DEFAULT_EVERY,
                },
                Update {
                    name: "tool".into(),
                    mount: "/opt/tool".into(),
                    command: vec!["/bin/up".into()],
                    every: Duration::from_secs(86400),
                },
            ]
        );
    }

    #[test]
    fn absent_is_empty() {
        assert!(from_customizations("x", None, &[]).unwrap().is_empty());
        assert!(from_customizations("x", Some(&json!({"vscode": {}})), &[]).unwrap().is_empty());
        assert!(from_customizations("x", Some(&json!({"devsandbox": {"updates": null}})), &[]).unwrap().is_empty());
    }

    #[test]
    fn refusals() {
        let e = format!("{:#}", from_customizations("gh", Some(&custom(json!({}))), &[]).unwrap_err());
        assert!(e.contains("must be a list"), "{e}");

        let ok = |extra: Value| {
            let mut v = json!({"name": "gh", "mount": "/var/lib/gh/bin", "command": ["/bin/up"]});
            v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            v
        };
        let cases = [
            (ok(json!({"extra": 1})), "unknown field `extra`"),
            (ok(json!({"name": "Gh"})), "update name `Gh`"),
            (json!({"mount": "/var/lib/gh/bin", "command": ["/bin/up"]}), "update #1: missing field `name`"),
            (ok(json!({"command": []})), "`command` is empty"),
            (ok(json!({"command": ["up"]})), "must be an absolute path"),
            (ok(json!({"every": "0d"})), "`every`"),
            (ok(json!({"every": "never"})), "`every`"),
            (ok(json!({"mount": "/nope"})), "isn't the target of one of the feature's `mounts`"),
            (ok(json!({"mount": "/rw"})), "must be mounted read-only"),
            (ok(json!({"mount": "/host"})), "must be a bind of a dir under `${sharedVolumes}`"),
            (ok(json!({"mount": "/vol"})), "must be a bind of a dir under `${sharedVolumes}`"),
            (ok(json!({"mount": "/up"})), "must be a bind of a dir under `${sharedVolumes}`"),
        ];
        for (update, want) in cases {
            let e = err(update.clone());
            assert!(e.starts_with("feature `gh`: update"), "{e}");
            assert!(e.contains(want), "{update}: {e}");
        }

        let twice = json!([
            {"name": "gh", "mount": "/var/lib/gh/bin", "command": ["/bin/up"]},
            {"name": "gh", "mount": "/opt/tool", "command": ["/bin/up"]},
        ]);
        let e = format!("{:#}", from_customizations("gh", Some(&custom(twice)), &mounts()).unwrap_err());
        assert!(e.contains("update `gh` is declared twice"), "{e}");
    }

    #[test]
    fn unique_across_features() {
        let one = |name: &str| Update {
            name: name.into(),
            mount: "/m".into(),
            command: vec!["/bin/up".into()],
            every: DEFAULT_EVERY,
        };
        let (a, b) = ([one("gh"), one("x")], [one("y")]);
        check_unique(&[("a", &a), ("b", &b), ("a", &a)]).unwrap();
        let c = [one("x")];
        let e = check_unique(&[("a", &a), ("c", &c)]).unwrap_err().to_string();
        assert_eq!(e, "update `x` is declared by both `a` and `c`");
    }
}
