//! SGR-escaped text (a replayed pty build log, `instances.buildLog` with
//! `ansi`) as ratatui lines, for the build-log modal's colours. Only SGR
//! (`ESC[…m`) styles; every other escape is dropped, so the plain text a
//! line parses to is what's shown, selected and counted.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// `line` (one line, no `\n`) as its plain text and its styled [`Line`].
pub fn parse_line(line: &str) -> (String, Line<'static>) {
    let mut plain = String::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut style = Style::default();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            if !c.is_control() || c == '\t' {
                run.push(c);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                let mut params = String::new();
                let mut fin = None;
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        fin = Some(c);
                        break;
                    }
                    params.push(c);
                }
                if fin == Some('m') {
                    let next = sgr(style, &params);
                    if next != style && !run.is_empty() {
                        plain.push_str(&run);
                        spans.push(Span::styled(std::mem::take(&mut run), style));
                    }
                    style = next;
                }
            }
            // OSC and the other string sequences: up to BEL or `ESC \`.
            Some(']' | 'P' | 'X' | '^' | '_') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            // An intermediate takes one more char (`ESC ( B`).
            Some('\x20'..='\x2f') => {
                chars.next();
            }
            _ => {}
        }
    }
    if !run.is_empty() {
        plain.push_str(&run);
        spans.push(Span::styled(run, style));
    }
    (plain, Line::from(spans))
}

/// `style` after SGR `params` (`;`- or `:`-separated; empty = reset).
fn sgr(mut style: Style, params: &str) -> Style {
    let mut codes = params.split([';', ':']).map(|p| if p.is_empty() { Some(0) } else { p.parse::<u16>().ok() });
    while let Some(code) = codes.next() {
        let Some(code) = code else { continue };
        style = match code {
            0 => Style::default(),
            1 => style.add_modifier(Modifier::BOLD),
            2 => style.add_modifier(Modifier::DIM),
            3 => style.add_modifier(Modifier::ITALIC),
            4 => style.add_modifier(Modifier::UNDERLINED),
            7 => style.add_modifier(Modifier::REVERSED),
            22 => style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style.remove_modifier(Modifier::ITALIC),
            24 => style.remove_modifier(Modifier::UNDERLINED),
            25 => style.remove_modifier(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK),
            27 => style.remove_modifier(Modifier::REVERSED),
            30..=37 => style.fg(indexed((code - 30) as u8)),
            90..=97 => style.fg(indexed((code - 90 + 8) as u8)),
            40..=47 => style.bg(indexed((code - 40) as u8)),
            100..=107 => style.bg(indexed((code - 100 + 8) as u8)),
            39 => Style { fg: None, ..style },
            49 => Style { bg: None, ..style },
            38 | 48 => {
                let color = match codes.next().flatten() {
                    Some(5) => codes.next().flatten().and_then(|n| u8::try_from(n).ok()).map(indexed),
                    Some(2) => {
                        let mut c = || codes.next().flatten().and_then(|n| u8::try_from(n).ok());
                        match (c(), c(), c()) {
                            (Some(r), Some(g), Some(b)) => Some(Color::Rgb(r, g, b)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                match color {
                    Some(color) if code == 38 => style.fg(color),
                    Some(color) => style.bg(color),
                    None => style,
                }
            }
            _ => style,
        };
    }
    style
}

/// Palette colour `n`, the first 16 by name (the terminal's own).
fn indexed(n: u8) -> Color {
    const NAMED: [Color; 16] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    NAMED.get(usize::from(n)).copied().unwrap_or(Color::Indexed(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(line: &Line) -> Vec<(String, Style)> {
        line.spans.iter().map(|s| (s.content.to_string(), s.style)).collect()
    }

    #[test]
    fn sgr_styles_runs_and_the_plain_text_drops_every_escape() {
        let (plain, line) = parse_line("a\x1b[0;1;34mb\x1b[22mc\x1b[0md");
        assert_eq!(plain, "abcd");
        let blue = Style::default().fg(Color::Blue);
        assert_eq!(
            spans(&line),
            [
                ("a".into(), Style::default()),
                ("b".into(), blue.add_modifier(Modifier::BOLD)),
                ("c".into(), blue.remove_modifier(Modifier::BOLD | Modifier::DIM)),
                ("d".into(), Style::default()),
            ]
        );
        // Non-SGR escapes and controls go, styles don't change.
        let (plain, line) = parse_line("x\x1b[1A\x1b[?25ly\x1b]0;t\x07z\x1b]8;;u\x1b\\w\x1b(Bv\x07");
        assert_eq!(plain, "xyzwv");
        assert_eq!(spans(&line), [("xyzwv".into(), Style::default())]);
        assert_eq!(parse_line("").0, "");
    }

    #[test]
    fn sgr_codes_map_to_styles() {
        let s = |params: &str| sgr(Style::default(), params);
        assert_eq!(s("31"), Style::default().fg(Color::Red));
        assert_eq!(s("97;100"), Style::default().fg(Color::White).bg(Color::DarkGray));
        assert_eq!(s("38;5;200;48;5;3"), Style::default().fg(Color::Indexed(200)).bg(Color::Yellow));
        assert_eq!(s("38;2;1;2;3"), Style::default().fg(Color::Rgb(1, 2, 3)));
        assert_eq!(s("38:2:1:2:3"), Style::default().fg(Color::Rgb(1, 2, 3)));
        assert_eq!(s("2;3;4;7"), Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::REVERSED));
        let off = sgr(s("1;3;4;7;31;41"), "23;24;27;39;49");
        assert_eq!((off.fg, off.bg, off.add_modifier), (None, None, Modifier::BOLD));
        assert_eq!(sgr(s("1;31"), ""), Style::default(), "ESC[m resets");
        assert_eq!(s("38;5"), Style::default(), "incomplete: ignored");
        assert_eq!(s("5;53;31"), Style::default().fg(Color::Red), "unknown codes ignored");
    }
}
