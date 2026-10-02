//! Text a container sends is untrusted terminal output: [`sanitize`] makes it
//! safe to store and draw. Applied where the bridge's messages enter the store
//! (`Inbox::apply_sink`) and again by the dashboard's renderer, so text stored
//! before this existed is safe too.
//!
//! ratatui already drops any grapheme holding a control character when it
//! fills a cell, so an escape sequence never reaches the terminal whole; but
//! what's left of it (`[31m`, `]0;title`) is drawn as garbage, a desktop popup
//! gets the raw bytes, and the dashboard's width math would count characters
//! ratatui then skips. Bidi overrides aren't controls to ratatui at all, and
//! reorder whatever follows them on terminals that honour them.

/// Tab stops every this many columns.
const TAB: usize = 4;

/// `s` without control characters: C0 (keeping `\n`), DEL and C1
/// (U+0080–U+009F, e.g. the one-byte CSI `\u{9b}`) are dropped, and so are
/// the bidi embeddings, overrides and isolates (U+202A–U+202E,
/// U+2066–U+2069). `\r\n` and a lone `\r` become `\n`, so a CRLF body keeps
/// its lines; `\t` becomes spaces up to the next multiple of 4 columns
/// (counted in chars since the last newline), so code keeps its alignment.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
                col = 0;
            }
            '\t' => {
                let n = TAB - col % TAB;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            c if c.is_control() || is_bidi_control(c) => {}
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escape_sequences_to_their_printable_rest() {
        // The ESC goes; the rest is ordinary text, harmless once it can't
        // start a sequence.
        assert_eq!(sanitize("a\x1b[31mred\x1b[0m"), "a[31mred[0m");
        assert_eq!(sanitize("\x1b]0;title\x07x"), "]0;titlex");
        assert_eq!(sanitize("\u{9b}31mx"), "31mx", "C1 CSI");
        assert_eq!(sanitize("a\u{85}b\x7fc\x00d"), "abcd", "C1 NEL, DEL, NUL");
    }

    #[test]
    fn keeps_newlines_and_unicode() {
        assert_eq!(sanitize("one\ntwo\n"), "one\ntwo\n");
        assert_eq!(sanitize("crlf\r\nline\rcr"), "crlf\nline\ncr");
        assert_eq!(sanitize("世界 ✓ ünï"), "世界 ✓ ünï");
    }

    #[test]
    fn expands_tabs_to_the_next_stop() {
        assert_eq!(sanitize("\tx"), "    x");
        assert_eq!(sanitize("ab\tx"), "ab  x");
        assert_eq!(sanitize("abcd\tx"), "abcd    x");
        assert_eq!(sanitize("ab\n\tx"), "ab\n    x", "columns restart per line");
    }

    #[test]
    fn drops_bidi_overrides() {
        assert_eq!(sanitize("a\u{202E}cba\u{202C}d"), "acbad");
        assert_eq!(sanitize("\u{2066}x\u{2069}"), "x");
        // Plain right-to-left text is untouched.
        assert_eq!(sanitize("שלום"), "שלום");
    }
}
