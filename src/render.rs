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
