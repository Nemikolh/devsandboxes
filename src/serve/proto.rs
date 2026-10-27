//! The daemon's wire format: JSON lines, one request, response or
//! notification per line (docs/reference/api.md; the methods are in
//! `api.rs`).
//!
//! ```text
//! → {"id":1,"method":"hello","params":{"version":"0.6.0","build":1767225600,"client":"tui"}}
//! ← {"id":1,"result":{"version":"0.6.0","build":1767225600,"protocol":1}}
//! ← {"id":1,"result":{"version":"0.6.0","build":1767225600,"protocol":1,"handoff":true}}   (client is newer)
//! ← {"id":2,"error":{"code":"unknown-method","message":"…"}}
//! ← {"method":"inbox.changed","params":{"generation":3}}                  (no id: a notification)
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Longest request line the daemon reads; longer ones close the connection.
pub const MAX_LINE: u64 = 1 << 20;

/// The API's protocol number, sent in every hello reply. Changes within one
/// number are additive only (new methods, new fields, new error codes); a
/// breaking change bumps it.
pub const PROTOCOL: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    /// Echoed back on the response. Absent on notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Stable machine code: `unknown-method`, `invalid`, …
    pub code: String,
    pub message: String,
    /// Machine-readable details some codes carry (`failed`: the verb's exit
    /// and instance). Additive: absent unless a code documents it.
    #[serde(default, skip_serializing_if = "no_data")]
    pub data: Option<Value>,
}

fn no_data(data: &Option<Value>) -> bool {
    matches!(data, None | Some(Value::Null))
}

impl Response {
    pub fn ok(id: Option<u64>, result: Value) -> Self {
        Self { id, result: Some(result), error: None }
    }

    pub fn err(id: Option<u64>, code: &str, message: impl Into<String>) -> Self {
        Self::err_with(id, code, message, None)
    }

    pub fn err_with(id: Option<u64>, code: &str, message: impl Into<String>, data: Option<Value>) -> Self {
        Self { id, result: None, error: Some(ErrorBody { code: code.into(), message: message.into(), data }) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloParams {
    pub version: String,
    #[serde(default)]
    pub build: u64,
    /// Who's calling (`tui`, `cli`, `api:<name>`); recorded for audit later.
    #[serde(default)]
    pub client: String,
    /// Hand off to this client's binary whatever its version, an older one
    /// included: a fleet worker that installed its hub's version
    /// (`fleet::update`, ADR 0042). Honored only on a local connection from
    /// this daemon's own uid (`daemon::hello`), never on a `Worker` one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub version: String,
    pub build: u64,
    /// [`PROTOCOL`]; 0 from a daemon that predates it (no API).
    #[serde(default)]
    pub protocol: u32,
    /// The client is newer: the daemon is draining and will exit; the client
    /// should start its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub handoff: bool,
}

/// A daemon-to-client message with no `id`: `inbox.changed`,
/// `inbox.shown`, `instances.changed`, `closing`. A line with a `method` is one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A daemon or client version: the semver plus the binary's mtime, so a
/// rebuilt dev binary with the same semver counts as newer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub semver: String,
    pub build: u64,
}

impl Version {
    /// This binary: `CARGO_PKG_VERSION` + the mtime (unix seconds) of the
    /// binary this process runs ([`crate::exe::own_build_stamp`], not the
    /// file now at its path), 0 when unknown.
    pub fn current() -> Self {
        Self { semver: env!("CARGO_PKG_VERSION").into(), build: crate::exe::own_build_stamp().unwrap_or(0) }
    }

    /// Ordering key: `(major, minor, patch, build)`, compared lexicographically.
    fn key(&self) -> (u64, u64, u64, u64) {
        let (a, b, c) = semver_triple(&self.semver);
        (a, b, c, self.build)
    }

    pub fn is_newer_than(&self, other: &Version) -> bool {
        self.key() > other.key()
    }
}

/// `"1.2.3"` → `(1, 2, 3)`. Each component's leading digits count (so
/// `"0.7.0-rc1"` is `(0, 7, 0)`); missing or non-numeric parts are 0.
pub fn semver_triple(s: &str) -> (u64, u64, u64) {
    let mut parts = s.split('.').map(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().unwrap_or(0)
    });
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(semver: &str, build: u64) -> Version {
        Version { semver: semver.into(), build }
    }

    #[test]
    fn semver_parses_numeric_prefixes() {
        assert_eq!(semver_triple("0.6.0"), (0, 6, 0));
        assert_eq!(semver_triple("1.10.2"), (1, 10, 2));
        assert_eq!(semver_triple("0.7.0-rc1"), (0, 7, 0));
        assert_eq!(semver_triple("2"), (2, 0, 0));
        assert_eq!(semver_triple("x.y"), (0, 0, 0));
    }

    #[test]
    fn newer_compares_semver_numerically_then_build() {
        assert!(v("0.6.0", 2).is_newer_than(&v("0.6.0", 1)));
        assert!(!v("0.6.0", 1).is_newer_than(&v("0.6.0", 1)));
        assert!(!v("0.6.0", 0).is_newer_than(&v("0.6.0", 1)));
        // Numeric, not string: 0.10 > 0.9.
        assert!(v("0.10.0", 0).is_newer_than(&v("0.9.0", 99)));
        // Semver wins over build.
        assert!(!v("0.5.9", 999).is_newer_than(&v("0.6.0", 1)));
    }

    #[test]
    fn wire_shapes() {
        let req: Request = serde_json::from_str(r#"{"method":"hello","params":{"version":"0.6.0","build":2,"client":"cli"}}"#).unwrap();
        assert_eq!((req.id, req.method.as_str()), (None, "hello"));
        let hello: HelloParams = serde_json::from_value(req.params).unwrap();
        assert_eq!((hello.version.as_str(), hello.build, hello.client.as_str(), hello.force), ("0.6.0", 2, "cli", false));
        let forced = HelloParams { force: true, ..hello };
        assert_eq!(
            serde_json::to_value(&forced).unwrap(),
            serde_json::json!({"version":"0.6.0","build":2,"client":"cli","force":true})
        );

        let quiet = HelloResult { version: "0.6.0".into(), build: 1, protocol: PROTOCOL, handoff: false };
        let ok = Response::ok(Some(1), serde_json::to_value(&quiet).unwrap());
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            serde_json::json!({"id":1,"result":{"version":"0.6.0","build":1,"protocol":1}})
        );
        let handoff = HelloResult { handoff: true, ..quiet };
        assert_eq!(serde_json::to_string(&handoff).unwrap(), r#"{"version":"0.6.0","build":1,"protocol":1,"handoff":true}"#);
        // A pre-API daemon's reply still parses.
        let old: HelloResult = serde_json::from_str(r#"{"version":"0.6.0","build":1}"#).unwrap();
        assert_eq!(old.protocol, 0);

        let note = Notification { method: "inbox.changed".into(), params: serde_json::json!({"generation":2}) };
        assert_eq!(serde_json::to_string(&note).unwrap(), r#"{"method":"inbox.changed","params":{"generation":2}}"#);

        let err = Response::err(Some(3), "unknown-method", "nope");
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"id":3,"error":{"code":"unknown-method","message":"nope"}}"#
        );
        let null = Response::err_with(Some(3), "x", "m", Some(Value::Null));
        assert_eq!(serde_json::to_string(&null).unwrap(), r#"{"id":3,"error":{"code":"x","message":"m"}}"#);
        let failed = Response::err_with(Some(4), "failed", "boom", Some(serde_json::json!({"exit": 1})));
        assert_eq!(
            serde_json::to_string(&failed).unwrap(),
            r#"{"id":4,"error":{"code":"failed","message":"boom","data":{"exit":1}}}"#
        );
    }
}
