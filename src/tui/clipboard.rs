//! Copy to the system clipboard through the outer terminal: OSC 52
//! (docs/architecture/dashboard/selection.md, *OSC 52*). It needs no host clipboard, so it
//! reaches the user's machine over SSH and from inside a container. A local
//! dashboard also pipes each copy to the host's clipboard command
//! ([`host_helper`]), since terminals may ignore OSC 52. Pure: the event loop
//! writes the bytes and spawns the command, since `App` stays I/O-free.

/// Most text one copy sends. Terminals cap the payload (xterm-likes ~100 KB),
/// and a runaway selection shouldn't stall the tty; the caller says when a
/// copy was cut (see [`cap`]).
pub const MAX_COPY: usize = 1 << 20;

/// [`MAX_COPY`] as base64 length: the bound on an OSC 52 payload relayed from
/// a terminal app, which arrives already encoded.
pub const MAX_COPY_BASE64: usize = MAX_COPY.div_ceil(3) * 4;

/// `text` cut to at most [`MAX_COPY`] bytes on a char boundary. The caller
/// compares lengths to report a cut.
pub fn cap(text: &str) -> &str {
    if text.len() <= MAX_COPY {
        return text;
    }
    let mut end = MAX_COPY;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The OSC 52 "set clipboard" sequence for `text`: `ESC ] 52 ; c ; <base64>
/// BEL`. Under tmux (`tmux`), wrapped in its DCS passthrough with every ESC
/// doubled, so tmux hands it to the outer terminal instead of eating it.
pub fn osc52(text: &str, tmux: bool) -> Vec<u8> {
    osc52_base64(base64(text.as_bytes()).as_bytes(), tmux)
}

/// [`osc52`] for a payload that is already base64, as relayed from an app in
/// the integrated terminal: passed through untouched, never re-encoded.
pub fn osc52_base64(b64: &[u8], tmux: bool) -> Vec<u8> {
    let mut seq = b"\x1b]52;c;".to_vec();
    seq.extend_from_slice(b64);
    seq.push(0x07);
    if !tmux {
        return seq;
    }
    let mut out = b"\x1bPtmux;".to_vec();
    for b in seq {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// The host clipboard command a local dashboard also pipes each copy to, so
/// a copy lands even when the outer terminal ignores OSC 52 (iTerm2 does by
/// default). `None` over ssh, where the host clipboard is the remote's and
/// OSC 52 is the only way to the user. `env`: a variable's value, if set.
pub fn host_helper(os: &str, env: impl Fn(&str) -> Option<String>) -> Option<(&'static str, &'static [&'static str])> {
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    if ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"].into_iter().any(set) {
        return None;
    }
    match os {
        "macos" => Some(("pbcopy", &[])),
        _ if set("WAYLAND_DISPLAY") => Some(("wl-copy", &[])),
        _ if set("DISPLAY") => Some(("xclip", &["-selection", "clipboard"])),
        _ => None,
    }
}

/// Decode standard base64 (padding optional), as an app's OSC 52 payload
/// arrives; `None` on anything else.
pub fn decode_base64(b64: &[u8]) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let data = b64.strip_suffix(b"==").or_else(|| b64.strip_suffix(b"=")).unwrap_or(b64);
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 2);
    for chunk in data.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= (value(c)? as u32) << (18 - 6 * i);
        }
        out.extend_from_slice(&n.to_be_bytes()[1..chunk.len()]);
    }
    Some(out)
}

/// Standard (RFC 4648 §4) base64 with `=` padding. Inline: one call site
/// doesn't earn a crate.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_rfc4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input:?}");
        }
        // UTF-8 goes in as its bytes.
        assert_eq!(base64("日本 ✓".as_bytes()), "5pel5pysIOKckw==");
    }

    #[test]
    fn osc52_plain_and_under_tmux() {
        assert_eq!(osc52("foo", false), b"\x1b]52;c;Zm9v\x07");
        assert_eq!(osc52("foo", true), b"\x1bPtmux;\x1b\x1b]52;c;Zm9v\x07\x1b\\");
    }

    #[test]
    fn osc52_base64_passes_payload_through() {
        assert_eq!(osc52_base64(b"aGk=", false), b"\x1b]52;c;aGk=\x07");
        assert_eq!(osc52_base64(b"aGk=", true), b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
        assert_eq!(MAX_COPY_BASE64, base64(&vec![0; MAX_COPY]).len());
    }

    #[test]
    fn base64_decodes_what_it_encodes() {
        for input in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "日本 ✓"] {
            let b64 = base64(input.as_bytes());
            assert_eq!(decode_base64(b64.as_bytes()).as_deref(), Some(input.as_bytes()), "{input:?}");
            let bare = b64.trim_end_matches('=');
            assert_eq!(decode_base64(bare.as_bytes()).as_deref(), Some(input.as_bytes()), "{bare:?}");
        }
        for bad in [&b"Z"[..], b"Zm9v!", b"Zm 9v", b"Zm9vY"] {
            assert_eq!(decode_base64(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn host_helper_by_platform_and_never_over_ssh() {
        fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
            move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        }
        assert_eq!(host_helper("macos", env(&[])), Some(("pbcopy", &[][..])));
        assert_eq!(host_helper("linux", env(&[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")])), Some(("wl-copy", &[][..])));
        assert_eq!(host_helper("linux", env(&[("DISPLAY", ":0")])), Some(("xclip", &["-selection", "clipboard"][..])));
        assert_eq!(host_helper("linux", env(&[("DISPLAY", "")])), None);
        assert_eq!(host_helper("linux", env(&[])), None);
        for ssh in ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"] {
            let vars = [(ssh, "x"), ("DISPLAY", ":0")];
            assert_eq!(host_helper("macos", env(&vars)), None, "{ssh}");
            assert_eq!(host_helper("linux", env(&vars)), None, "{ssh}");
        }
    }

    #[test]
    fn cap_cuts_on_a_char_boundary() {
        assert_eq!(cap("short"), "short");
        // A 3-byte glyph straddling the limit is dropped whole.
        let text = format!("{}日", "a".repeat(MAX_COPY - 1));
        let cut = cap(&text);
        assert_eq!(cut.len(), MAX_COPY - 1);
        assert!(cut.chars().all(|c| c == 'a'));
    }
}
