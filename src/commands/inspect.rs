use anyhow::Result;

use super::resolve_container;
use crate::render::{classify_json_line, dim, green, JsonLine};
use crate::runtime::backend;
use crate::state::{Instance, State};

/// Pretty-print the runtime's `inspect` JSON for an instance or service
/// container, with the same light key highlighting as the TUI inspect pane
/// (color only on a TTY; see `render::paint`). For an instance, its browser
/// sandbox line goes first, on stderr, from the record ([`browser_line`]).
pub fn inspect(name: &str, json: bool) -> Result<()> {
    let state = State::load()?;
    let container = resolve_container(&state, name)?;
    let raw = backend().inspect_json(&container)?;
    if let Some(info) = state.instances.values().find(|i| i.container == container) {
        eprintln!("{}", browser_line(info));
    }
    println!("{}", render(&raw, json));
    Ok(())
}

/// What `inspect` prints of raw inspect output (one `println` of it), shared
/// with the re-run on a machine (`machines::cli::calls`). JSON
/// mode is the runtime's own inspect document verbatim (no color, no
/// envelope): the payload is the backend's schema, not ours.
pub(crate) fn render(raw: &str, json: bool) -> String {
    if json {
        return pretty_inspect(raw);
    }
    pretty_inspect(raw).lines().map(paint_json_line).collect::<Vec<_>>().join("\n")
}

/// `run`'s browser sandbox line for a recorded instance
/// (`run::tool_env::summary`); an instance from before the check says so.
fn browser_line(info: &Instance) -> String {
    if info.browser_sandbox.is_none() && info.tool_env.is_empty() {
        return "browser sandbox: not recorded (created before the userns check; `devsandbox rebuild` checks it)".into();
    }
    use crate::commands::run::tool_env;
    tool_env::summary(tool_env::root_plan(info.remote_user.as_deref()), info.browser_sandbox.as_ref())
}

/// Pretty-print raw `inspect` output. On JSON parse failure the raw output is
/// kept with the parse error as the first line. Shared with the TUI's inspect
/// pane so both render identically.
pub fn pretty_inspect(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => match serde_json::to_string_pretty(&v) {
            Ok(pretty) => pretty,
            Err(e) => format!("cannot format inspect JSON: {e}\n{raw}"),
        },
        Err(e) => format!("could not parse inspect JSON: {e}\n{raw}"),
    }
}

/// ANSI rendering of one pretty-printed JSON line: structural lines dim,
/// `"key":` prefixes green, everything else verbatim.
fn paint_json_line(line: &str) -> String {
    match classify_json_line(line) {
        JsonLine::Structural => dim(line),
        JsonLine::KeyValue(split) => {
            let (key, value) = line.split_at(split);
            format!("{}{value}", green(key))
        }
        JsonLine::Plain => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_inspect_formats_valid_json() {
        assert_eq!(
            pretty_inspect(r#"[{"Id":"abc"}]"#),
            "[\n  {\n    \"Id\": \"abc\"\n  }\n]"
        );
    }

    #[test]
    fn pretty_inspect_keeps_raw_on_parse_failure() {
        let out = pretty_inspect("not json");
        assert!(out.starts_with("could not parse inspect JSON:"), "{out}");
        assert!(out.ends_with("\nnot json"), "{out}");
    }
}
