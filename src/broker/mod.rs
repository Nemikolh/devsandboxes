//! The credential broker's HTTP core (docs/plans/active/credential-broker.md,
//! *Shape*, "The broker"): what `serve` runs over every request an
//! instance's `HTTP_PROXY` stream carries, before policy and injection.
//! All of it but [`upstream`] is pure over `BufRead` / `Write` / byte
//! slices, so it runs on any platform and tests need no sockets.
//!
//! - [`head`]: a request head, parsed with strict framing (one unambiguous
//!   body length or refusal), and case-insensitive header lookup.
//! - [`body`]: a request body by `Content-Length` or chunked, and a
//!   response body re-framed chunked.
//! - [`guard`]: the refusals that keep a browser in the sandbox from
//!   driving the broker (`Origin`, `Sec-Fetch-Site`, `Host`).
//! - [`redact`]: masking the injected value wherever it comes back.
//! - [`upstream`]: the HTTPS client calls go out on, with the address
//!   check on every address it dials.
//!
//! A refusal reaches the caller as an `io::Error` of kind `InvalidData`
//! carrying an [`HttpError`] ([`refusal`]), so one `?` covers transport
//! failures and protocol violations alike while the caller can still answer
//! the latter with a status.

// No caller until the broker is wired into `serve`; its tests are here.
#![cfg_attr(not(test), allow(dead_code))]

pub mod body;
pub mod guard;
pub mod head;
pub mod redact;
pub mod upstream;

use std::fmt;
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    HeadTooLarge,
    TooManyHeaders,
    Truncated,
    BareLineEnding,
    /// Not an HTTP/1.x request httparse accepts (obs-fold, whitespace
    /// before a colon, a bad token, …).
    Malformed,
    UnsupportedVersion,
    BadTarget,
    /// `CONNECT` inside the broker: tunnels are opened by devsbd, never
    /// through a brokered connection.
    Connect,
    /// Both `Content-Length` and `Transfer-Encoding`.
    LengthAndChunked,
    /// A `Content-Length` that isn't one decimal number, or several that
    /// differ.
    BadContentLength,
    /// A `Transfer-Encoding` other than exactly `chunked`, or one on an
    /// HTTP/1.0 request.
    BadTransferEncoding,
    /// `Upgrade`, or `Connection: upgrade`: refused until designed.
    Upgrade,
    Expectation,
    BadChunk,
    Trailers,
    Browser,
    Host,
}

impl HttpError {
    pub fn status(&self) -> u16 {
        match self {
            HttpError::HeadTooLarge | HttpError::TooManyHeaders => 431,
            HttpError::UnsupportedVersion => 505,
            HttpError::Expectation => 417,
            HttpError::Browser | HttpError::Host => 403,
            _ => 400,
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HttpError::HeadTooLarge => "request head too large",
            HttpError::TooManyHeaders => "too many header fields",
            HttpError::Truncated => "connection closed inside a request head",
            HttpError::BareLineEnding => "line ending other than CRLF",
            HttpError::Malformed => "malformed request head",
            HttpError::UnsupportedVersion => "HTTP version other than 1.1 or 1.0",
            HttpError::BadTarget => "request target in neither origin-form nor absolute-form",
            HttpError::Connect => "CONNECT is not brokered",
            HttpError::LengthAndChunked => "both Content-Length and Transfer-Encoding",
            HttpError::BadContentLength => "invalid or conflicting Content-Length",
            HttpError::BadTransferEncoding => "Transfer-Encoding other than chunked",
            HttpError::Upgrade => "protocol upgrades are not brokered",
            HttpError::Expectation => "unsupported Expect",
            HttpError::BadChunk => "malformed chunk",
            HttpError::Trailers => "chunked trailers are not accepted",
            HttpError::Browser => "browser requests (Origin, Sec-Fetch-Site) are refused",
            HttpError::Host => "Host is not this listener's authority",
        })
    }
}

impl std::error::Error for HttpError {}

impl From<HttpError> for io::Error {
    fn from(e: HttpError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

/// The protocol refusal inside `e`, if it is one (else a transport error:
/// drop the connection, no answer).
pub fn refusal(e: &io::Error) -> Option<&HttpError> {
    e.get_ref()?.downcast_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_round_trips_through_io_error() {
        let e: io::Error = HttpError::LengthAndChunked.into();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(refusal(&e), Some(&HttpError::LengthAndChunked));
        assert_eq!(refusal(&io::Error::from(io::ErrorKind::UnexpectedEof)), None);
    }

    #[test]
    fn statuses() {
        assert_eq!(HttpError::BadChunk.status(), 400);
        assert_eq!(HttpError::HeadTooLarge.status(), 431);
        assert_eq!(HttpError::UnsupportedVersion.status(), 505);
        assert_eq!(HttpError::Expectation.status(), 417);
        assert_eq!(HttpError::Host.status(), 403);
    }
}
