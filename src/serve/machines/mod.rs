//! Machines: remote hosts whose daemons this one talks to over
//! `ssh <target> devsandbox api --stdio` (docs/machines.md).
//!
//! - `ssh`: the ssh argv and quoting, and the injectable [`ssh::Dial`] that
//!   opens a [`ssh::Duplex`] to a machine
//! - `link`: a connection to a machine's daemon (the hello for now)
//!
//! The registry (`machines.toml`) lands here in step 2.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod link;
pub mod ssh;

/// One registered machine, as `machines.toml` will hold it under
/// `[machine.<name>]` (docs/machines.md, F2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The table key, so not a field of its own in the file.
    #[serde(skip)]
    pub name: String,
    /// What ssh takes (`me@host`, `host`, an ssh-config alias) or an
    /// `ssh://` URL ([`ssh::parse_target`]).
    pub target: String,
    /// Config roots on the machine, as the remote canonicalized them.
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    /// The remote devsandbox as shell text (`~/bin/devsandbox`); `None` is
    /// `devsandbox` on the login shell's `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

impl Entry {
    pub fn command(&self) -> &str {
        self.command.as_deref().unwrap_or("devsandbox")
    }
}
