//! One line of a runtime's container event stream ([`super::Backend::events_argv`])
//! and whether it can change what the daemon's container poll would list.
//!
//! Docker (`--format {{json .}}`) says `Action` and `Actor.Attributes.name`;
//! podman (`--format json`) says `Status` and `Name`. Both are accepted.

use serde::Deserialize;

use super::NAME_PREFIX;

/// A container event: the container's name and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub action: String,
}

/// Docker's and podman's line shapes, merged: every field optional.
#[derive(Debug, Deserialize)]
struct Line {
    #[serde(rename = "Type", default)]
    kind: Option<String>,
    /// Docker.
    #[serde(rename = "Action", default)]
    action: Option<String>,
    /// Docker.
    #[serde(rename = "Actor", default)]
    actor: Option<Actor>,
    /// Podman.
    #[serde(rename = "Status", default)]
    status: Option<String>,
    /// Podman.
    #[serde(rename = "Name", default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Actor {
    #[serde(rename = "Attributes", default)]
    attributes: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

/// One event line, `None` for anything that isn't a container event with a
/// name and an action (garbage, an empty line, another object type).
pub fn parse(line: &str) -> Option<Event> {
    let line: Line = serde_json::from_str(line.trim()).ok()?;
    if line.kind.as_deref().is_some_and(|k| k != "container") {
        return None;
    }
    let docker_name = || {
        line.actor.as_ref()?.attributes.as_ref()?.get("name")?.as_str().map(str::to_string)
    };
    let name = docker_name().or(line.name.clone()).filter(|n| !n.is_empty())?;
    let action = line.action.clone().or(line.status.clone()).filter(|a| !a.is_empty())?;
    Some(Event { name, action })
}

/// The actions that change a container's existence, name or running state,
/// i.e. what a re-list could see differently. Docker says `die` and
/// `destroy`, podman `died` and `remove`.
const RELEVANT: &[&str] = &[
    "create", "start", "restart", "die", "died", "stop", "kill", "destroy", "remove", "rename", "pause", "unpause",
];

/// Whether `e` should wake the poll: a devsandbox container ([`NAME_PREFIX`])
/// and an action in [`RELEVANT`]. A whitelist, not a blacklist: the daemon's
/// own bridges `exec` into containers, and each exec emits `exec_create: …`,
/// `exec_start: …` and `exec_die` (podman: `exec`, `exec_died`); accepting
/// those would make every poll pass wake the next one, a loop. Health checks
/// (`health_status: …`), `attach`, `top` and the like change nothing listed
/// either.
pub fn relevant(e: &Event) -> bool {
    e.name.starts_with(NAME_PREFIX) && RELEVANT.contains(&e.action.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker(action: &str, name: &str) -> String {
        format!(
            r#"{{"status":"{action}","id":"d04ae961","from":"alpine:3.20","Type":"container","Action":"{action}","Actor":{{"ID":"d04ae961475bf93f","Attributes":{{"execDuration":"0","exitCode":"0","image":"alpine:3.20","name":"{name}"}}}},"scope":"local","time":1792236569,"timeNano":1792236569481842333}}"#
        )
    }

    fn podman(status: &str, name: &str) -> String {
        format!(
            r#"{{"ID":"7f3c0b1e9a2d","Image":"docker.io/library/alpine:3.20","Name":"{name}","Status":"{status}","Time":"2026-10-17T10:12:03.123456789+02:00","Type":"container","Attributes":{{"image":"docker.io/library/alpine:3.20","name":"{name}"}}}}"#
        )
    }

    fn accepted(line: &str) -> bool {
        parse(line).is_some_and(|e| relevant(&e))
    }

    #[test]
    fn docker_lines_parse() {
        let e = parse(&docker("die", "devsandbox-web")).unwrap();
        assert_eq!(e, Event { name: "devsandbox-web".into(), action: "die".into() });
        // Docker 28 drops the legacy lowercase fields.
        let modern = r#"{"Type":"container","Action":"start","Actor":{"ID":"d04a","Attributes":{"image":"alpine:3.20","name":"devsandbox-x"}},"scope":"local","time":1,"timeNano":1}"#;
        assert_eq!(parse(modern).unwrap(), Event { name: "devsandbox-x".into(), action: "start".into() });
        for action in ["create", "start", "restart", "die", "stop", "kill", "destroy", "rename", "pause", "unpause"] {
            assert!(accepted(&docker(action, "devsandbox-web")), "{action}");
        }
    }

    #[test]
    fn podman_lines_parse() {
        let e = parse(&podman("died", "devsandbox-web")).unwrap();
        assert_eq!(e, Event { name: "devsandbox-web".into(), action: "died".into() });
        for status in ["create", "start", "restart", "died", "stop", "kill", "remove", "rename", "pause", "unpause"] {
            assert!(accepted(&podman(status, "devsandbox-web")), "{status}");
        }
    }

    #[test]
    fn noise_is_rejected() {
        // The bridges' own execs: these must never wake the poll.
        assert!(!accepted(&docker("exec_create: sh -c devsbd bridge", "devsandbox-web")));
        assert!(!accepted(&docker("exec_start: sh -c devsbd bridge", "devsandbox-web")));
        assert!(!accepted(&docker("exec_die", "devsandbox-web")));
        assert!(!accepted(&podman("exec", "devsandbox-web")));
        assert!(!accepted(&podman("exec_died", "devsandbox-web")));
        assert!(!accepted(&docker("health_status: healthy", "devsandbox-web")));
        assert!(!accepted(&podman("health_status", "devsandbox-web")));
        assert!(!accepted(&docker("attach", "devsandbox-web")));
        assert!(!accepted(&docker("top", "devsandbox-web")));
        assert!(!accepted(&podman("cleanup", "devsandbox-web")));
        // Not ours.
        assert!(!accepted(&docker("start", "postgres")));
        assert!(!accepted(&podman("start", "my-devsandbox-x")));
        // Not an event.
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
        assert_eq!(parse("not json"), None);
        assert_eq!(parse("{}"), None);
        assert_eq!(parse(r#"{"Type":"network","Action":"connect","Actor":{"Attributes":{"name":"devsandbox-net"}}}"#), None);
    }
}
