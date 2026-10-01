//! Editor-side config for an instance: the VS Code attached-container name
//! config (extensions, `remoteUser`) that `run` and `vscode` both write.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Register `customizations.vscode.extensions` and `remoteUser` with the
/// Remote-Containers extension by writing its per-container-name config file.
/// Without `remoteUser`, VS Code attaches as the editor's default user and
/// hits EACCES on root-owned files (e.g. rootless docker).
/// `extensions: None` (sandbox config unresolvable) keeps the file's existing
/// list rather than wiping what `run` registered.
pub(crate) fn write_vscode_name_config(
    container: &str,
    extensions: Option<&[String]>,
    remote_user: Option<&str>,
) -> Result<()> {
    let base = editor_config_base()?;
    // Every installed VS Code-family product keys nameConfigs by container name.
    for product in ["Code", "Cursor", "VSCodium"] {
        let product_dir = base.join(product);
        if !product_dir.is_dir() {
            continue; // not installed
        }
        let dir =
            product_dir.join("User/globalStorage/ms-vscode-remote.remote-containers/nameConfigs");
        std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!("{container}.json"));
        let existing = match std::fs::read_to_string(&path) {
            Ok(contents) => Some(contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let contents = merged_name_config(existing.as_deref(), extensions, remote_user)
            .with_context(|| format!("in {}", path.display()))?;
        std::fs::write(&path, contents)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(())
}

/// Per-OS base dir that holds each editor product's config directory:
/// `~/Library/Application Support` on macOS, `$XDG_CONFIG_HOME`/`~/.config`
/// elsewhere.
fn editor_config_base() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        return std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support"))
            .context("cannot determine config dir ($HOME)");
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("cannot determine config dir ($XDG_CONFIG_HOME or $HOME)")
}

/// Existing name-config JSON (if any) with `extensions` and `remoteUser`
/// replaced. `extensions: None` leaves that key untouched; `remoteUser: None`
/// removes the key so the file tracks the config.
fn merged_name_config(
    existing: Option<&str>,
    extensions: Option<&[String]>,
    remote_user: Option<&str>,
) -> Result<String> {
    let mut root = match existing {
        Some(contents) => {
            serde_json::from_str::<serde_json::Value>(contents).context("invalid JSON")?
        }
        None => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .context("existing config is not a JSON object")?;
    if let Some(extensions) = extensions {
        obj.insert("extensions".into(), serde_json::json!(extensions));
    }
    match remote_user {
        Some(user) => {
            obj.insert("remoteUser".into(), serde_json::json!(user));
        }
        None => {
            obj.remove("remoteUser");
        }
    }
    Ok(serde_json::to_string_pretty(&root)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_config_from_scratch() {
        let json = merged_name_config(None, Some(&["a.b".into(), "c.d".into()]), None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["extensions"], serde_json::json!(["a.b", "c.d"]));
        assert!(parsed.get("remoteUser").is_none());
    }

    #[test]
    fn name_config_preserves_other_keys() {
        let existing = r#"{"settings": {"x": 1}, "extensions": ["old.ext"]}"#;
        let json = merged_name_config(Some(existing), Some(&["new.ext".into()]), None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["extensions"], serde_json::json!(["new.ext"]));
        assert_eq!(parsed["settings"]["x"], 1);
    }

    #[test]
    fn name_config_sets_and_clears_remote_user() {
        let json = merged_name_config(None, Some(&[]), Some("root")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["remoteUser"], "root");

        // remote_user gone from config → key removed from an existing file.
        let json = merged_name_config(Some(&json), Some(&[]), None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("remoteUser").is_none());
    }

    #[test]
    fn name_config_unknown_extensions_keeps_existing() {
        // An unresolvable config must not wipe the list `run` registered.
        let existing = r#"{"extensions": ["keep.me"], "remoteUser": "root"}"#;
        let json = merged_name_config(Some(existing), None, Some("root")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["extensions"], serde_json::json!(["keep.me"]));

        let json = merged_name_config(None, None, None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("extensions").is_none());
    }
}
