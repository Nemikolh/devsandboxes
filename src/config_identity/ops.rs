//! The identity findings as API operations over a config root and a
//! `state.toml` path (docs/reference/api.md, *Config*): `config.findings`,
//! `config.migrate` and `config.ack`. The daemon serves them
//! (`serve::api`); the CLI with no daemon calls them directly, so they're
//! plain fns with the state path injected, and answer the API's errors.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{Finding, Kind, MigrateError, edit_line, file_hash, identity_findings, migrate_file};
use crate::config::{self, Config};
use crate::inbox::wire::ApiError;
use crate::state::State;

/// `config.findings`' answer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFindings {
    /// The config file (as `config::find` names it).
    pub file: PathBuf,
    /// `toml` | `yaml`: a YAML file isn't rewritten (`config.migrate`
    /// answers `unsupported`).
    pub format: &'static str,
    /// What `config.migrate` takes back, to refuse a file changed meanwhile.
    pub file_hash: String,
    pub findings: Vec<IdentityFinding>,
}

/// One [`Finding`] on the wire.
#[derive(Debug, Serialize)]
pub struct IdentityFinding {
    pub id: String,
    /// `template.<name>` | `sandbox.<name>`.
    pub table: String,
    /// `root-target` | `root-env` | `root-path` | `root-user`.
    pub kind: &'static str,
    pub key: String,
    pub value: String,
    /// The update as a line for a person ([`edit_line`]); `null`: fixed by hand.
    pub edit: Option<String>,
    pub sandboxes: Vec<String>,
    /// In `state.toml`'s `identity_acks` for this root.
    pub acked: bool,
}

/// `config.migrate`'s answer.
#[derive(Debug, Serialize)]
pub struct ConfigMigrated {
    /// Findings fixed; 0 wrote nothing.
    pub edits: usize,
    /// This root's instances the rewrite drifted, sorted (see [`drifted`]).
    pub drifted: Vec<String>,
}

/// `config.ack`'s answer: every acked id of the root after the write.
#[derive(Debug, Serialize)]
pub struct ConfigAcked {
    pub acked: Vec<String>,
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::RootTarget => "root-target",
        Kind::RootEnv => "root-env",
        Kind::RootPath => "root-path",
        Kind::RootUser => "root-user",
    }
}

/// Root `root`'s config file, its text and the parsed config. Errors as
/// `serve::api`'s `config_at`: `invalid` (relative root, several files, a
/// config that doesn't parse), `not-found` (no directory, no config file).
fn read(root: &Path) -> Result<(PathBuf, bool, String, Config), ApiError> {
    if !root.is_absolute() {
        return Err(ApiError::invalid(format!("`root` must be absolute, got `{}`", root.display())));
    }
    if !root.is_dir() {
        return Err(ApiError::not_found(format!("no directory {}", root.display())));
    }
    let file = config::find(root)
        .map_err(|e| ApiError::invalid(format!("{e:#}")))?
        .ok_or_else(|| ApiError::not_found(format!("no config file in {}", root.display())))?;
    let text = std::fs::read_to_string(&file)
        .map_err(|e| ApiError::internal(anyhow::Error::new(e).context(format!("cannot read {}", file.display()))))?;
    let yaml = matches!(file.extension().and_then(|e| e.to_str()), Some("yaml" | "yml"));
    let parsed = if yaml { Config::parse_yaml(&text) } else { Config::parse(&text) };
    let config = parsed.map_err(|e| ApiError::invalid(format!("in {}: {e:#}", file.display())))?;
    Ok((file, yaml, text, config))
}

/// The key `root`'s acks live under: its project id, as `autostart_boot`'s.
fn project(root: &Path) -> Result<String, ApiError> {
    crate::commands::services::project_id(root).map_err(ApiError::internal)
}

/// `config.findings {root, sandbox?}`: every finding of `root`'s config, or
/// only those applying to `sandbox` (`not-found` when it has no such
/// sandbox), each with whether it's acked.
pub fn findings(root: &Path, state: &Path, sandbox: Option<&str>) -> Result<ConfigFindings, ApiError> {
    let (file, yaml, text, config) = read(root)?;
    if let Some(name) = sandbox
        && !config.sandboxes.contains_key(name)
    {
        return Err(ApiError::not_found(format!("no sandbox `{name}` in {}", root.display())));
    }
    let state = State::load_at(state).map_err(ApiError::internal)?;
    let acks = state.identity_acks.get(&project(root)?);
    let findings = identity_findings(&config)
        .into_iter()
        .filter(|f| sandbox.is_none_or(|name| f.sandboxes.iter().any(|s| s == name)))
        .map(|f| wire(f, acks))
        .collect();
    Ok(ConfigFindings { file, format: if yaml { "yaml" } else { "toml" }, file_hash: file_hash(&text), findings })
}

fn wire(f: Finding, acks: Option<&BTreeSet<String>>) -> IdentityFinding {
    IdentityFinding {
        acked: acks.is_some_and(|a| a.contains(&f.id)),
        edit: edit_line(&f, false),
        table: f.origin.table(),
        kind: kind_name(f.kind),
        id: f.id,
        key: f.key,
        value: f.value,
        sandboxes: f.sandboxes,
    }
}

/// `config.migrate {root, ids?, keep?, fileHash}`: [`migrate_file`] on
/// `root`'s config file, then the instances it drifted. Errors: `conflict`
/// (the file changed since `file_hash`), `unsupported` (YAML; the message
/// lists the edits), `invalid` (unknown ids, a kept non-root-user, a rewrite
/// that didn't come out right), `internal` (the write; names a `.bak` left).
pub fn migrate(
    root: &Path,
    state: &Path,
    ids: Option<&[String]>,
    keep: &[String],
    expected_hash: &str,
) -> Result<ConfigMigrated, ApiError> {
    let (file, _, _, before) = read(root)?;
    let migrated = migrate_file(&file, ids, keep, expected_hash).map_err(|e| match e {
        MigrateError::Conflict => ApiError::conflict(e.to_string()),
        MigrateError::Unsupported { .. } => ApiError::unsupported(e.to_string()),
        MigrateError::Invalid(why) => ApiError::invalid(why),
        MigrateError::Io(e) => ApiError::internal(e),
    })?;
    if migrated.edits == 0 {
        return Ok(ConfigMigrated { edits: 0, drifted: Vec::new() });
    }
    let (_, _, _, after) = read(root)?;
    let state = State::load_at(state).map_err(ApiError::internal)?;
    let drifted = drifted(&state, &project(root)?, &before, &after);
    Ok(ConfigMigrated { edits: migrated.edits, drifted })
}

/// The instances of project `project` whose sandbox's config hash
/// (`Config::resolved_table`, what the `config_hash` label records) differs
/// between `before` and `after`: their containers now drift
/// (`commands::drift_decision`), without asking the runtime. A container
/// that already drifted still does; one with no label (pre-upgrade) is
/// counted though `drift_decision` would let it be. A sandbox that doesn't
/// merge on both sides isn't counted.
pub fn drifted(state: &State, project: &str, before: &Config, after: &Config) -> Vec<String> {
    let hash = |config: &Config, name: &str| config.resolved_table(name).ok().map(|(_, h)| h);
    // A BTreeMap: names already sorted.
    state
        .instances
        .iter()
        .filter(|(_, i)| i.project == project)
        .filter(|(_, i)| hash(before, &i.sandbox) != hash(after, &i.sandbox))
        .map(|(name, _)| name.clone())
        .collect()
}

/// `config.ack {root, ids}`: add `ids` (each a current finding of `root`,
/// else `invalid`) to the root's acks, dropping every acked id no longer
/// among its findings; no ids left removes the root's entry.
pub fn ack(root: &Path, state: &Path, ids: &[String]) -> Result<ConfigAcked, ApiError> {
    let (_, _, _, config) = read(root)?;
    let current: BTreeSet<String> = identity_findings(&config).into_iter().map(|f| f.id).collect();
    if let Some(id) = ids.iter().find(|id| !current.contains(*id)) {
        return Err(ApiError::invalid(format!("no finding {id} in the config (reread its findings)")));
    }
    let project = project(root)?;
    let acked = State::update_at(state, |s| {
        let mut set = s.identity_acks.remove(&project).unwrap_or_default();
        set.extend(ids.iter().cloned());
        set.retain(|id| current.contains(id));
        let acked: Vec<String> = set.iter().cloned().collect();
        if !set.is_empty() {
            s.identity_acks.insert(project.clone(), set);
        }
        Ok(acked)
    })
    .map_err(ApiError::internal)?;
    Ok(ConfigAcked { acked })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("devsandbox-identity-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("root")).unwrap();
        (dir.join("root"), dir.join("state/state.toml"))
    }

    const CONFIG: &str = "[sandbox.web]\nimage = \"img\"\nremoteUser = \"root\"\nmounts = [\"source=/a,target=/root/.a\"]\n\
                          [sandbox.api]\nimage = \"img\"\nworkspaceFolder = \"/root/app\"\n[sandbox.ok]\nimage = \"img\"\n";

    fn instance(sandbox: &str, project: &str) -> crate::state::Instance {
        let toml = format!(
            "sandbox = \"{sandbox}\"\nproject = \"{project}\"\ncontainer = \"c\"\nfolder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\n"
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn drifted_is_the_instances_whose_sandbox_hash_moved() {
        let before = Config::parse(CONFIG).unwrap();
        let after = Config::parse(&CONFIG.replace("target=/root/.a", "target=/opt/a")).unwrap();
        let mut state = State::default();
        for (name, sandbox, project) in
            [("web", "web", "p1"), ("web-2", "web", "p1"), ("api", "api", "p1"), ("other", "web", "p2"), ("gone", "nope", "p1")]
        {
            state.instances.insert(name.into(), instance(sandbox, project));
        }
        assert_eq!(drifted(&state, "p1", &before, &after), vec!["web", "web-2"]);
        assert_eq!(drifted(&state, "p1", &before, &before), Vec::<String>::new());
    }

    #[test]
    fn ack_adds_prunes_and_drops_an_empty_root() {
        let (root, state) = scratch("ack");
        std::fs::write(root.join("devsandboxes.toml"), CONFIG).unwrap();
        let all: Vec<String> = findings(&root, &state, None).unwrap().findings.into_iter().map(|f| f.id).collect();
        assert_eq!(all.len(), 3);
        let got = ack(&root, &state, &all[..2]).unwrap();
        let mut want = all[..2].to_vec();
        want.sort();
        assert_eq!(got.acked, want);
        let project = crate::commands::services::project_id(&root).unwrap();
        // A stale id from an earlier config is dropped on the next write.
        State::update_at(&state, |s| {
            s.identity_acks.get_mut(&project).unwrap().insert("stale".into());
            Ok(())
        })
        .unwrap();
        let got = ack(&root, &state, &[]).unwrap();
        assert_eq!(got.acked, want);
        assert_eq!(code(ack(&root, &state, &["nope".into()])), "invalid");
        // Every finding fixed: the root's entry goes.
        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.ok]\nimage = \"img\"\n").unwrap();
        assert_eq!(ack(&root, &state, &[]).unwrap().acked, Vec::<String>::new());
        assert!(State::load_at(&state).unwrap().identity_acks.is_empty());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    fn code<T: std::fmt::Debug>(r: Result<T, ApiError>) -> &'static str {
        r.unwrap_err().code
    }
}
