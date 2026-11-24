//! Small ANSI styling helpers, gated on stdout being a terminal with `NO_COLOR`
//! unset. When color is disabled every helper returns its input unchanged, so
//! callers can wrap text unconditionally.

use std::io::IsTerminal;

/// Whether colored output is enabled: stdout is a TTY and `NO_COLOR` is unset.
pub fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Wrap `text` in the SGR `code` (e.g. `"1;36"`) when color is enabled.
pub fn paint(text: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold_cyan(text: &str) -> String {
    paint(text, "1;36")
}

pub fn green(text: &str) -> String {
    paint(text, "32")
}

pub fn yellow(text: &str) -> String {
    paint(text, "33")
}

pub fn magenta(text: &str) -> String {
    paint(text, "35")
}

pub fn dim(text: &str) -> String {
    paint(text, "2")
}

/// Style-agnostic classification of one pretty-printed JSON line, shared by the
/// TUI inspect pane and the CLI `inspect` command so both highlight identically.
#[derive(Debug, PartialEq, Eq)]
pub enum JsonLine {
    /// Punctuation-only structural line (`{`, `}`, `[`, `],`): render dim.
    Structural,
    /// `"key": value` — the key half ends at the given byte index (just past
    /// the colon): render it green, the rest plain.
    KeyValue(usize),
    /// Anything else: render verbatim.
    Plain,
}

/// Classify a pretty-printed JSON line for light highlighting: keys scannable,
/// values plain — no real parser.
pub fn classify_json_line(line: &str) -> JsonLine {
    let trimmed = line.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| matches!(c, '{' | '}' | '[' | ']' | ',')) {
        return JsonLine::Structural;
    }
    if line.trim_start().starts_with('"') {
        if let Some(colon) = line.find(':') {
            return JsonLine::KeyValue(colon + 1);
        }
    }
    JsonLine::Plain
}

/// Humanize an elapsed-seconds duration: `3d4h`, `2h05m`, `12m`, `40s`.
/// Shared by the dashboard's UPTIME column and `ps`'.
pub fn humanize_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

/// Fixed-width plain-text table: header row first, two-space gutter, no
/// trailing padding on the last column.
/// Every row has `headers.len()` cells.
pub fn table<R: AsRef<[String]>>(headers: &[&str], rows: &[R]) -> String {
    let n = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.as_ref().iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let header_row: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let mut out = String::new();
    for row in std::iter::once(header_row.as_slice()).chain(rows.iter().map(AsRef::as_ref)) {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(cell);
            if i + 1 < n {
                out.extend(std::iter::repeat(' ').take(widths[i] - cell.len()));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_json_lines_by_kind() {
        // `"key": value` splits just past the colon.
        assert_eq!(classify_json_line("    \"Id\": \"abc\","), JsonLine::KeyValue(9));
        assert_eq!(classify_json_line("  {"), JsonLine::Structural);
        assert_eq!(classify_json_line("],"), JsonLine::Structural);
        assert_eq!(classify_json_line("    \"abc\""), JsonLine::Plain);
        assert_eq!(classify_json_line(""), JsonLine::Plain);
        assert_eq!(classify_json_line("not json"), JsonLine::Plain);
    }

    #[test]
    fn table_aligns_and_never_pads_last_column() {
        let rows = vec![
            ["devsandbox-repo".to_string(), "node:22".to_string(), "Up 3 minutes".to_string()],
            ["devsandbox-x".to_string(), "alpine".to_string(), "running".to_string()],
        ];
        assert_eq!(
            table(&["NAME", "IMAGE", "STATUS"], &rows),
            "NAME             IMAGE    STATUS\n\
             devsandbox-repo  node:22  Up 3 minutes\n\
             devsandbox-x     alpine   running\n"
        );
    }
}
