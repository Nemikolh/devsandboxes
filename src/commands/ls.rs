use std::path::Path;

use anyhow::{Context, Result};
use toml::Value;

use crate::commands::status::Envelope;
use crate::config::Config;
use crate::render::{bold_cyan, dim, green, magenta, yellow};

const HEADERS: [&str; 5] = ["NAME", "SOURCE", "FOLDER", "SERVICES", "EXTENDS"];

pub fn ls(dir: &Path, json: bool) -> Result<()> {
    let config = Config::load(dir)?;

    // JSON path shares the snapshot's `SandboxRow` (structured services/extends,
    // config_hash, issues) so a UI gets the same data the TUI does. No docker,
    // like the table path; resolve failures become `?` rows rather than an error,
    // and an empty config emits `data: []`, not the human "no sandboxes" prose.
    // The identity notes' acks come from state; a state that won't load (or a
    // root without a project id) reads as none acked, not an error.
    if json {
        let acked = crate::commands::services::project_id(dir)
            .ok()
            .and_then(|p| crate::state::State::load().ok()?.identity_acks.remove(&p))
            .unwrap_or_default();
        let (rows, _errors) = crate::snapshot::sandbox_rows(dir, &config, &acked);
        println!("{}", render_json(&rows)?);
        return Ok(());
    }

    let sandboxes = config.resolve_all()?;
    if sandboxes.is_empty() {
        println!("{}", none_defined(&crate::config::display_path(dir)));
        return Ok(());
    }

    let mut lines = Vec::with_capacity(sandboxes.len());
    for sandbox in &sandboxes {
        // Feature problems a build would hit (a missing local feature folder),
        // surfaced here rather than only at `run`.
        for issue in crate::features::feature_issues(dir, sandbox.properties.features.as_ref()) {
            eprintln!("warning: sandbox `{}`: {issue}", sandbox.name);
        }
        lines.push(Line {
            name: sandbox.name.clone(),
            source: sandbox.source(),
            folder: sandbox.folder().map(str::to_string),
            services: sandbox
                .properties
                .services
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|s| s.join(", ")),
            extends: config
                .sandboxes
                .get(&sandbox.name)
                .and_then(|table| table.get("extends"))
                .and_then(extends_names),
        });
    }
    print!("{}", table(&lines));
    Ok(())
}

/// `ls --json`'s document for `rows` (one `println` of it), shared with the
/// re-run on an offered machine (`machines::cli::offered`).
pub(crate) fn render_json(rows: &[crate::snapshot::SandboxRow]) -> Result<String> {
    serde_json::to_string_pretty(&Envelope::new(rows)).context("serialize sandboxes")
}

/// `ls`'s line for a config root without sandboxes.
pub(crate) fn none_defined(dir: &Path) -> String {
    format!("no sandboxes defined in {}", dir.display())
}

/// One sandbox of the `ls` table, its cells before colouring (`None`: `-`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub name: String,
    pub source: String,
    pub folder: Option<String>,
    pub services: Option<String>,
    pub extends: Option<String>,
}

impl Line {
    /// The line of a snapshot's sandbox row: what the re-run on an offered
    /// machine has instead of the config (`source`, the `, `-joined
    /// services and `extends`, as [`ls`] builds them).
    pub(crate) fn of_row(row: &crate::snapshot::SandboxRow) -> Line {
        let joined = |v: &[String]| (!v.is_empty()).then(|| v.join(", "));
        Line {
            name: row.name.clone(),
            source: row.source.clone(),
            folder: row.folder.clone(),
            services: joined(&row.services),
            extends: joined(&row.extends),
        }
    }
}

/// The `ls` table, header first, each line ending in `\n`.
pub(crate) fn table(lines: &[Line]) -> String {
    // Plain cells (for width math) and colored cells (for display) are kept in
    // lockstep so column widths derive from the visible text, not the escapes.
    let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".to_string());
    let plain: Vec<[String; 5]> = lines
        .iter()
        .map(|l| [l.name.clone(), l.source.clone(), dash(&l.folder), dash(&l.services), dash(&l.extends)])
        .collect();
    let colored: Vec<[String; 5]> = lines
        .iter()
        .map(|l| {
            [
                bold_cyan(&l.name),
                color_source(&l.source),
                l.folder.clone().unwrap_or_else(|| dim("-")),
                l.services.as_deref().map(magenta).unwrap_or_else(|| dim("-")),
                l.extends.clone().unwrap_or_else(|| dim("-")),
            ]
        })
        .collect();
    let widths = column_widths(&HEADERS, &plain);
    let mut out = format_row(&HEADERS.map(dim), &plain_headers(), &widths);
    for (colored_row, plain_row) in colored.iter().zip(&plain) {
        out.push_str(&format_row(colored_row, plain_row, &widths));
    }
    out
}

fn plain_headers() -> [String; 5] {
    HEADERS.map(|h| h.to_string())
}

fn color_source(source: &str) -> String {
    if source.starts_with("image ") {
        green(source)
    } else if source.starts_with("dockerfile ") {
        yellow(source)
    } else {
        source.to_string()
    }
}

/// `extends` as a `, `-joined string; `None` when absent or not a string/array.
fn extends_names(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let names: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if names.is_empty() {
                None
            } else {
                Some(names.join(", "))
            }
        }
        _ => None,
    }
}

/// Widest plain cell per column, header included.
fn column_widths(headers: &[&str; 5], rows: &[[String; 5]]) -> [usize; 5] {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    widths
}

/// Render one row: pad each colored cell to its column width using the plain
/// cell's length, join with a two-space gutter, no trailing padding on the last
/// column. Returns a line ending in `\n`.
fn format_row(colored: &[String; 5], plain: &[String; 5], widths: &[usize; 5]) -> String {
    let mut out = String::new();
    for i in 0..5 {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&colored[i]);
        if i < 4 {
            let pad = widths[i] - plain[i].len();
            out.extend(std::iter::repeat(' ').take(pad));
        }
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: [&str; 5]) -> [String; 5] {
        cells.map(str::to_string)
    }

    #[test]
    fn widths_take_widest_cell_including_header() {
        let rows = [
            row(["alpha", "image x", "-", "-", "-"]),
            row(["a", "dockerfile very/long/path", "web", "db", "base"]),
        ];
        let widths = column_widths(&HEADERS, &rows);
        assert_eq!(widths[0], "alpha".len()); // wider than "NAME"
        assert_eq!(widths[1], "dockerfile very/long/path".len());
        assert_eq!(widths[2], "FOLDER".len()); // header wins over "web"/"-"
        assert_eq!(widths[3], "SERVICES".len());
        assert_eq!(widths[4], "EXTENDS".len());
    }

    #[test]
    fn row_padding_uses_two_space_gutter_and_no_trailing_pad() {
        let widths = [5usize, 7, 6, 8, 7];
        let cells = row(["a", "image x", "-", "-", "base"]);
        let line = format_row(&cells, &cells, &widths);
        assert_eq!(line, "a      image x  -       -         base\n");
        // last column is not padded: line ends right after "base".
        assert!(!line.trim_end_matches('\n').ends_with(' '));
    }

    #[test]
    fn padding_uses_plain_length_not_colored_length() {
        let widths = column_widths(&HEADERS, &[row(["name", "image x", "f", "svc", "ext"])]);
        let plain = row(["name", "image x", "f", "svc", "ext"]);
        let colored = [
            "\x1b[1;36mname\x1b[0m".to_string(),
            "\x1b[32mimage x\x1b[0m".to_string(),
            "f".to_string(),
            "\x1b[35msvc\x1b[0m".to_string(),
            "ext".to_string(),
        ];
        let colored_line = format_row(&colored, &plain, &widths);
        let plain_line = format_row(&plain, &plain, &widths);
        // Stripping escapes from the colored render yields the plain render:
        // padding was computed from plain lengths, unaffected by escapes.
        let stripped = strip_ansi(&colored_line);
        assert_eq!(stripped, plain_line);
    }

    #[test]
    fn extends_string_and_array() {
        assert_eq!(
            extends_names(&Value::String("base".into())),
            Some("base".to_string())
        );
        let arr = Value::Array(vec![Value::String("a".into()), Value::String("b".into())]);
        assert_eq!(extends_names(&arr), Some("a, b".to_string()));
        assert_eq!(extends_names(&Value::Array(vec![])), None);
        assert_eq!(extends_names(&Value::Integer(1)), None);
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
