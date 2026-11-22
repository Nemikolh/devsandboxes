//! Platform-neutral, but for the home's files ([`home`], [`store`], [`ca`]), which
//! take `serve::locked_file`'s unix write discipline, and the logins that
//! fill them ([`oauth`]), which open URLs as `open` does.

#[cfg(unix)]
pub mod ca;
pub mod claim;
pub mod fence;
pub mod git;
#[cfg(unix)]
pub mod home;
pub mod instance;
#[cfg(unix)]
pub mod oauth;
pub mod review;
#[cfg(unix)]
pub mod store;
#[cfg(unix)]
pub mod tokens;
#[cfg(unix)]
pub mod trust;

use anyhow::{Result, bail};

const MAX_NAME: usize = 64;

/// `[a-z0-9][a-z0-9_-]*`: a credential's name in `credentials.toml`
/// (`sentry-work`) and in a config's `access.<kind>.credential`. No `.`, so a
/// name is one bare TOML key.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAME
        || !name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        bail!(
            "credential name `{name}`: lowercase letters, digits, `_` and `-`, starting with a letter or digit, \
             at most {MAX_NAME} bytes"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for good in ["sentry-work", "github", "0", "a_b"] {
            check_name(good).unwrap();
        }
        for bad in ["", "Sentry", "-x", "_x", "a.b", "a b", "a/b", &"x".repeat(65)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }
}
