//! `status --json`: dump the full [`Snapshot`](crate::snapshot::Snapshot) as
//! machine-readable JSON for an external UI. The snapshot is the same struct
//! the TUI renders, so JSON parity holds by construction. Docker being down is
//! data, not a CLI failure: the rows still come from state and `Snapshot.error`
//! carries the reason, so we print and exit 0.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

/// Current JSON schema version. Bump on any breaking payload change.
const SCHEMA: u32 = 1;

/// Wraps every JSON payload so consumers can gate on the schema version before
/// reading `data`. `pub(crate)` because the read verbs (`ps`/`ls`/`stats`)
/// reuse it for their own `--json` output.
#[derive(Serialize)]
pub(crate) struct Envelope<T: Serialize> {
    schema: u32,
    data: T,
}

impl<T: Serialize> Envelope<T> {
    /// Wrap `data` at the current [`SCHEMA`] version.
    pub(crate) fn new(data: T) -> Self {
        Envelope { schema: SCHEMA, data }
    }
}

pub fn status(dir: &Path) -> Result<()> {
    let snapshot = crate::snapshot::collect(dir);
    let json = serde_json::to_string_pretty(&Envelope::new(snapshot))
        .context("serialize snapshot")?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{ContainerStatus, InstanceRow, Snapshot};
    use std::time::Instant;

    /// Minimal instance row for serialization assertions.
    fn instance(name: &str, status: ContainerStatus) -> InstanceRow {
        InstanceRow {
            name: name.into(),
            sandbox: "web".into(),
            container: format!("devsandbox-{name}"),
            status,
            uptime_secs: 0,
            cpu: None,
            mem: None,
            folder: "/w".into(),
            worktree: false,
            services: Vec::new(),
            workspace: "/w".into(),
            remote_user: None,
            remote_env_len: 0,
            base_folder: "/w".into(),
            drift: false,
            instance_id: String::new(),
            dispatcher: None,
        }
    }

    #[test]
    fn serializes_envelope_status_tags_and_skips_collected_at() {
        let snapshot = Snapshot {
            instances: vec![
                instance("up", ContainerStatus::Running("Up 3 minutes".into())),
                instance("gone", ContainerStatus::Missing),
            ],
            sandboxes: Vec::new(),
            services: Vec::new(),
            sandbox_count: 0,
            runtime_name: "docker",
            runtime_version: None,
            collected_at: Instant::now(),
            stats: true,
            error: None,
        };

        let json = serde_json::to_string(&Envelope::new(snapshot)).unwrap();

        // Envelope shape.
        assert!(json.contains(r#""schema":1"#), "{json}");
        assert!(json.contains(r#""data":"#), "{json}");
        // Running status: internally tagged with its text.
        assert!(
            json.contains(r#""status":{"state":"running","text":"Up 3 minutes"}"#),
            "{json}"
        );
        // Missing status: tagged, no `text` field.
        assert!(json.contains(r#""status":{"state":"missing"}"#), "{json}");
        // `collected_at` is skipped.
        assert!(!json.contains("collected_at"), "{json}");
    }
}
