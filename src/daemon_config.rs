//! The per-user global config, `<data>/devsandbox/daemon.config.toml`: settings
//! that belong to the user rather than to one `-C` config root (the daemon is
//! per user). Read by `devsandbox serve`; top-level so other per-user code can
//! read it too.
// Only `serve` (unix-only) reads it so far.
#![cfg_attr(not(unix), allow(dead_code))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Unknown keys are ignored (no `deny_unknown_fields`) so a file written for a
/// newer binary still loads in an older one.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct DaemonConfig {
    pub serve: ServeConfig,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ServeConfig {
    /// No idle exit, as `serve --keep-alive`.
    pub keep_alive: bool,
}

/// [`path_from`] over the process env.
pub fn path() -> Result<PathBuf> {
    path_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// `<data>/devsandbox/daemon.config.toml`, next to `state.toml`: `State::path`'s
/// base-dir logic over the given `$XDG_DATA_HOME` / `$HOME`.
fn path_from(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    let base = xdg_data_home
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".local/share")))
        .context("cannot determine user data dir ($XDG_DATA_HOME or $HOME)")?;
    Ok(base.join("devsandbox/daemon.config.toml"))
}

/// The global config; a missing file is the defaults.
pub fn load() -> Result<DaemonConfig> {
    load_from(&path()?)
}

fn load_from(path: &Path) -> Result<DaemonConfig> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse(&contents).with_context(|| format!("invalid daemon config {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DaemonConfig::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn parse(contents: &str) -> Result<DaemonConfig> {
    Ok(toml::from_str(contents)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_prefers_xdg_data_home_then_home() {
        let p = path_from(Some("/xdg".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/xdg/devsandbox/daemon.config.toml"));
        let p = path_from(None, Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.local/share/devsandbox/daemon.config.toml"));
        assert!(path_from(None, None).is_err());
    }

    #[test]
    fn defaults() {
        let c = DaemonConfig::default();
        assert!(!c.serve.keep_alive);
        assert_eq!(parse("").unwrap(), c, "an empty file is the defaults");
        assert_eq!(parse("[serve]\n").unwrap(), c, "an empty table too");
    }

    #[test]
    fn keep_alive_parses() {
        let c = parse("[serve]\nkeep-alive = true\n").unwrap();
        assert!(c.serve.keep_alive);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let c = parse("from-the-future = 1\n[serve]\nkeep-alive = true\nlater = \"x\"\n[other]\na = 1\n").unwrap();
        assert!(c.serve.keep_alive);
    }

    #[test]
    fn an_invalid_value_is_an_error() {
        assert!(parse("[serve]\nkeep-alive = \"yes\"").is_err());
        assert!(parse("not toml at all [").is_err());
    }

    #[test]
    fn loads_from_a_file() {
        let dir = std::env::temp_dir().join(format!("devsandbox-daemon-config-{}", std::process::id()));
        let path = dir.join("daemon.config.toml");
        assert_eq!(load_from(&path).unwrap(), DaemonConfig::default(), "missing: defaults");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "[serve]\nkeep-alive = true\n").unwrap();
        assert!(load_from(&path).unwrap().serve.keep_alive);
        std::fs::write(&path, "[serve]\nkeep-alive = 1\n").unwrap();
        let err = load_from(&path).unwrap_err();
        assert!(format!("{err:#}").contains("invalid daemon config"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
