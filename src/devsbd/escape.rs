//! Value escaping for the helper's line-based files (`bootfile.rs`,
//! `notify.rs`): one directive per line, so a value must never contain a raw
//! newline, and NUL is kept free for use as a separator. Shared by both crates
//! (devsbd includes it via `#[path]`), std-only.
//!
//! `\\` → `\`, `\n` → newline, `\0` → NUL; any other escape is an error.

pub fn escape(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
}

pub fn unescape(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('0') => out.push('\0'),
            other => return Err(format!("bad escape `\\{}`", other.map(String::from).unwrap_or_default())),
        }
    }
    Ok(out)
}
