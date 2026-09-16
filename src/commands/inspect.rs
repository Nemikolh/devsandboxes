use anyhow::Result;

use super::resolve_container;
use crate::render::{classify_json_line, dim, green, JsonLine};
use crate::runtime::backend;
use crate::state::State;

/// Pretty-print the runtime's `inspect` JSON for an instance or service
/// container, with the same light key highlighting as the TUI inspect pane
/// (color only on a TTY; see `render::paint`).
pub fn inspect(name: &str) -> Result<()> {
    let state = State::load()?;
    let container = resolve_container(&state, name)?;
    let raw = backend().inspect_json(&container)?;
    for line in pretty_inspect(&raw).lines() {
        println!("{}", paint_json_line(line));
    }
    Ok(())
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
