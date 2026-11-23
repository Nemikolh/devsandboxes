//! The broker's own answers, in the upstream's error shape the claim
//! declares (credential-broker, *Refusals*), so a tool shows the reason the
//! way it shows the upstream's errors. Never `WWW-Authenticate`: that starts
//! a client's own login.

use serde_json::{Map, Value, json};

use crate::credentials::claim::Refusal as Shape;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    /// `refused`, `leak` (a phantom where it doesn't belong), `pending`
    /// (a permission request unanswered), `denied` (answered no) or
    /// `error` (the broker or the upstream failed).
    pub verdict: &'static str,
    /// One line.
    pub reason: String,
    pub credential: Option<String>,
    pub scope: Option<String>,
    /// The permission request the call was held on (`R-…`).
    pub request: Option<String>,
    /// What would let the call through (`devsandbox login sentry`).
    pub todo: Option<String>,
    /// Seconds, sent as `Retry-After`.
    pub retry_after: Option<u16>,
}

impl Refusal {
    pub fn new(status: u16, verdict: &'static str, reason: impl Into<String>) -> Refusal {
        Refusal { status, verdict, reason: reason.into(), credential: None, scope: None, request: None, todo: None, retry_after: None }
    }

    pub fn refused(reason: impl Into<String>) -> Refusal {
        Refusal::new(403, "refused", reason)
    }

    pub fn todo(mut self, todo: impl Into<String>) -> Refusal {
        self.todo = Some(todo.into());
        self
    }

    fn devsandbox(&self) -> Value {
        json!({
            "verdict": self.verdict,
            "credential": self.credential,
            "scope": self.scope,
            "request": self.request,
            "do": self.todo,
        })
    }

    fn message(&self) -> String {
        let line = |s: &str| s.replace(['\r', '\n'], " ");
        match &self.todo {
            Some(todo) => format!("devsandbox: {} ({})", line(&self.reason), line(todo)),
            None => format!("devsandbox: {}", line(&self.reason)),
        }
    }

    /// (content type, body). No shape: `json` with `message`.
    pub fn body(&self, shape: Option<&Shape>) -> (&'static str, Vec<u8>) {
        let message = self.message();
        let doc = match shape {
            None => nested("message", message, self.devsandbox()),
            Some(Shape::Json { field }) => nested(field, message, self.devsandbox()),
            Some(Shape::Anthropic {}) => json!({
                "type": "error",
                "error": { "type": "permission_error", "message": message },
                "devsandbox": self.devsandbox(),
            }),
            Some(Shape::Mcp {}) => json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32001, "message": message },
                "devsandbox": self.devsandbox(),
            }),
            // git prints a failed request's text body as the error.
            Some(Shape::Git {}) => return ("text/plain; charset=utf-8", format!("{message}\n").into_bytes()),
        };
        ("application/json", serde_json::to_vec(&doc).expect("a JSON value serializes"))
    }

    /// The whole response; `close` adds `Connection: close`.
    pub fn response(&self, shape: Option<&Shape>, close: bool) -> Vec<u8> {
        let (content_type, body) = self.body(shape);
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
            self.status,
            reason_phrase(self.status),
            body.len()
        )
        .into_bytes();
        if let Some(s) = self.retry_after {
            out.extend_from_slice(format!("Retry-After: {s}\r\n").as_bytes());
        }
        if close {
            out.extend_from_slice(b"Connection: close\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&body);
        out
    }
}

/// `field` is a field path (`detail`, `error.message`): nested objects.
fn nested(field: &str, message: String, devsandbox: Value) -> Value {
    let mut value = Value::String(message);
    for name in field.rsplit('.') {
        let mut obj = Map::new();
        obj.insert(name.to_string(), value);
        value = Value::Object(obj);
    }
    if let Value::Object(obj) = &mut value {
        obj.entry("devsandbox").or_insert(devsandbox);
    }
    value
}

pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> (String, Value) {
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        (head.to_string(), serde_json::from_str(body).unwrap_or(Value::String(body.to_string())))
    }

    fn sample() -> Refusal {
        Refusal {
            credential: Some("sentry-work".into()),
            scope: Some("releases.write".into()),
            ..Refusal::refused("scope `releases.write` isn't in `access.sentry.scopes`").todo("add it to the config")
        }
    }

    #[test]
    fn json_shape_with_the_devsandbox_object() {
        let r = sample();
        let (head, body) = parse(&r.response(Some(&Shape::Json { field: "detail".into() }), false));
        assert!(head.starts_with("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: "), "{head}");
        assert!(!head.to_ascii_lowercase().contains("www-authenticate") && !head.contains("Connection"), "{head}");
        assert_eq!(
            body["detail"],
            "devsandbox: scope `releases.write` isn't in `access.sentry.scopes` (add it to the config)"
        );
        assert_eq!(
            body["devsandbox"],
            json!({ "verdict": "refused", "credential": "sentry-work", "scope": "releases.write", "request": null, "do": "add it to the config" })
        );
    }

    #[test]
    fn other_shapes() {
        let r = Refusal::new(400, "refused", "bad\r\nrequest");
        let (head, body) = parse(&r.response(None, true));
        assert!(head.starts_with("HTTP/1.1 400 Bad Request\r\n") && head.ends_with("Connection: close"), "{head}");
        assert_eq!(body["message"], "devsandbox: bad  request");
        assert_eq!(body["devsandbox"]["credential"], Value::Null);

        let (_, body) = parse(&r.response(Some(&Shape::Json { field: "error.message".into() }), false));
        assert_eq!(body["error"]["message"], "devsandbox: bad  request");
        assert_eq!(body["devsandbox"]["verdict"], "refused");
        let (_, body) = parse(&r.response(Some(&Shape::Anthropic {}), false));
        assert_eq!((body["type"].as_str(), body["error"]["type"].as_str()), (Some("error"), Some("permission_error")));
        let (_, body) = parse(&r.response(Some(&Shape::Mcp {}), false));
        assert_eq!(body["error"]["code"], -32001);
        let (head, body) = parse(&r.response(Some(&Shape::Git {}), false));
        assert!(head.contains("text/plain"), "{head}");
        assert_eq!(body, Value::String("devsandbox: bad  request\n".into()));
    }
}
