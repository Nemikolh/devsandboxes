use std::path::Path;

use anyhow::{Context, Result};

use super::resolve_instance;
use crate::config::Config;
use crate::runtime::backend;
use crate::state::{Instance, State};

/// CLI entry point: open VS Code attached to the instance `name` resolves to
/// (same as `o` in the dashboard).
pub fn vscode(dir: &Path, name: &str) -> Result<()> {
    let state = State::load()?;
    let key = resolve_instance(&state, name)?;
    let info = &state.instances[&key];
    println!("{}", launch(dir, &key, info)?);
    Ok(())
}

/// Launch VS Code attached to `info`'s container, detached. Writes the
/// extensions name-config (best-effort), then spawns `code`. Returns a one-line
/// status. Shared with the TUI's `o` so both open identically.
pub fn launch(dir: &Path, instance: &str, info: &Instance) -> Result<String> {
    // Extensions and remoteUser come from the resolved sandbox; failure to
    // resolve is non-fatal — fall back to the remote user recorded in state at
    // run time so the attach still opens with write access.
    let resolved = Config::load(dir)
        .ok()
        .and_then(|cfg| cfg.resolve_sandbox(&info.sandbox).ok());
    let extensions: Vec<String> = resolved
        .as_ref()
        .and_then(|sb| sb.properties.vscode_extensions().map(<[String]>::to_vec))
        .unwrap_or_default();
    let remote_user = resolved
        .as_ref()
        .and_then(|sb| sb.properties.remote_user.clone())
        .or_else(|| info.remote_user.clone());
    let _ = super::run::write_vscode_name_config(
        &info.container,
        &extensions,
        remote_user.as_deref(),
    );

    // Prefer the generated `.code-workspace` (window named after the instance,
    // carries the extra `folders` roots); instances created before it existed
    // fall back to a plain folder open.
    let (flag, path) = match &info.workspace_file {
        Some(file) => ("--file-uri", file.as_str()),
        None => ("--folder-uri", info.workspace.as_str()),
    };
    // The Remote-Containers extension resolves a different authority per runtime.
    // Docker/podman use `attached-container` (hex of the bare container name).
    // Apple `container` uses `apple-container` (hex of a JSON `{id, image}`
    // payload) and requires the user's opt-in
    // `dev.containers.experimentalAppleContainerSupport` setting.
    let (authority, hint) = if backend().name() == "container" {
        let image = apple_image_reference(&info.container).unwrap_or_default();
        let payload = serde_json::json!({ "id": info.container, "image": image }).to_string();
        (
            format!("apple-container+{}", hex_encode(&payload)),
            " (needs dev.containers.experimentalAppleContainerSupport=true)",
        )
    } else {
        (format!("attached-container+{}", hex_encode(&info.container)), "")
    };
    let uri = format!("vscode-remote://{authority}/{path}");
    std::process::Command::new("code")
        .args([flag, &uri])
        .spawn()
        .context("code: failed to launch (`code` on PATH?)")?;
    Ok(format!("opening VS Code → {instance}{hint}"))
}

/// Apple `container` image reference (`configuration.image.reference`) for
/// `container`, needed in the `apple-container` attach URI payload. Best-effort:
/// `None` when inspect fails or the field is absent, in which case the caller
/// sends an empty image (the resolver only requires `id`).
fn apple_image_reference(container: &str) -> Option<String> {
    let json = backend().inspect_json(container).ok()?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let obj = v.get(0).unwrap_or(&v);
    obj.get("configuration")?
        .get("image")?
        .get("reference")?
        .as_str()
        .map(str::to_string)
}

/// Lowercase hex of a string's UTF-8 bytes, as the Remote-Containers URI wants.
fn hex_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::hex_encode;

    #[test]
    fn hex_encodes_container_name() {
        // Lowercase hex of the UTF-8 bytes, matching the attach URI format.
        assert_eq!(hex_encode("devsandbox-web"), "64657673616e64626f782d776562");
        assert_eq!(hex_encode(""), "");
        assert_eq!(hex_encode("A/z"), "412f7a");
    }

    #[test]
    fn apple_authority_payload_roundtrips() {
        // The `apple-container` authority carries hex of a JSON `{id, image}`
        // payload; VS Code hex-decodes and `JSON.parse`s it. Verify the encoding
        // devsandbox emits decodes back to that object.
        let payload = serde_json::json!({ "id": "devsandbox-web", "image": "img:latest" })
            .to_string();
        let hex = hex_encode(&payload);
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let decoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded["id"], "devsandbox-web");
        assert_eq!(decoded["image"], "img:latest");
    }
}
