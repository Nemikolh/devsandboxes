//! Markdown for the Inbox: dispatcher text is often LLM output, so the
//! thread pane renders it instead of showing `**`, backticks and `#` raw
//! (docs/inbox-threads.md, *Markdown in threads*). Pure: text and a width in,
//! styled [`Line`]s out, so every rule is unit-testable.
//!
//! Two modes. [`render`] (a thread's `message`, a notify body) renders
//! blocks: headings, lists, quotes, code blocks, rules. [`inline_spans`]
//! (titles, statuses, timeline rows, cards) is one line: only code, emphasis
//! and links as text, newlines folded to spaces, and block syntax at its
//! start (`1. `, `- `, `# `, `> `) kept as the literal text it is in a title.
//! The caller wraps it ([`wrap`], the pane) or truncates it (a card).
//!
//! Wrapping is ours rather than `Paragraph::wrap`'s: the pane bounds its
//! scroll by the rows it renders, so it must know them exactly, and a list
//! item or a quote needs its hanging indent or `│` on every wrapped row.
//! Widths are display widths (ratatui's `Span::width`), so wide glyphs count
//! as the two cells they take. Input is [`sanitize`]d first: what's already
//! in the store predates the apply-boundary pass, and this is cheap.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::ui::ACCENT;
use crate::inbox::sanitize;

/// Inline code and code blocks: a gray tint with an explicit foreground, so
/// it reads on dark and light themes alike (like the selected card's tint,
/// `ui::CARD_TINT` = 236). Two steps lighter than that tint, so a code span
/// still stands out on the selected card.
pub const CODE: Style = Style::new().fg(Color::Indexed(252)).bg(Color::Indexed(238));

const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const HEADING: Style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
const LINK: Style = Style::new().add_modifier(Modifier::UNDERLINED);

/// `md` rendered as blocks for `width` columns. Empty input renders no
/// lines.
pub fn render(md: &str, width: u16) -> Vec<Line<'static>> {
    Blocks::new(width as usize).run(&sanitize(md))
}

/// The source text, unrendered: each line word-wrapped, nothing styled (the
/// pane's `m` raw view).
pub fn raw(text: &str, width: u16) -> Vec<Line<'static>> {
    sanitize(text).lines().flat_map(|l| wrap(&[Span::raw(l.to_string())], width as usize, &[], &[], false)).collect()
}

/// `md` as one line of styled spans, unwrapped: for a single-row context
/// (a card) whose caller truncates.
pub fn inline_spans(md: &str) -> Vec<Span<'static>> {
    let text = escape_block_start(&fold(md));
    let mut inl = Inlines::new(false);
    for event in Parser::new(&text) {
        // One line can still open a block (an HTML one, say): its text is
        // taken as inline text, the block structure ignored.
        if !inl.event(&event) {
            if let Event::Html(t) = &event {
                inl.text(t.trim_end_matches('\n'));
            }
        }
    }
    inl.spans
}

/// `text` [`sanitize`]d and on one line: each line trimmed, blank ones
/// dropped, the rest joined by a space. The source an inline context shows
/// raw, and what [`inline_spans`] parses.
pub fn fold(text: &str) -> String {
    sanitize(text).lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

/// `line` with block syntax at its start escaped, so a title like `1. fix`
/// or `- wip` stays literal instead of becoming a list with the marker gone.
/// One line only (the caller folds newlines), so only its start matters, and
/// only constructs that one line can open are checked.
fn escape_block_start(line: &str) -> String {
    let b = line.as_bytes();
    let spaced = |i: usize| b.get(i).is_none_or(|c| *c == b' ');
    let escape_at = |i: usize| format!("{}\\{}", &line[..i], &line[i..]);
    match b.first() {
        Some(b'>') => return escape_at(0),
        Some(b'#') => {
            let n = b.iter().take_while(|c| **c == b'#').count();
            if n <= 6 && spaced(n) {
                return escape_at(0);
            }
        }
        Some(b'-' | b'+' | b'*' | b'_') => {
            let c = b[0];
            let rule = c != b'+' && b.iter().all(|x| *x == c || *x == b' ') && b.iter().filter(|x| **x == c).count() >= 3;
            if rule || (c != b'_' && spaced(1)) {
                return escape_at(0);
            }
        }
        Some(b'`') if line.starts_with("```") && !line.trim_start_matches('`').contains('`') => return escape_at(0),
        Some(b'~') if line.starts_with("~~~") => return escape_at(0),
        Some(b'[') if line.find("]:").is_some_and(|i| !line[1..i].contains(']')) => return escape_at(0),
        Some(c) if c.is_ascii_digit() => {
            let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
            if n <= 9 && matches!(b.get(n), Some(b'.' | b')')) && spaced(n + 1) {
                return escape_at(n);
            }
        }
        _ => {}
    }
    line.to_string()
}

/// Inline state shared by both modes: the style stack, the spans so far,
/// and open links.
struct Inlines {
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// Per open link: its URL and where its text starts in `spans`.
    links: Vec<(String, usize)>,
    /// Append ` (url)` after a link's text ([`render`]); a one-line context
    /// has no room for it.
    urls: bool,
}

impl Inlines {
    fn new(urls: bool) -> Self {
        Self { spans: Vec::new(), styles: Vec::new(), links: Vec::new(), urls }
    }

    fn style(&self) -> Style {
        self.styles.iter().fold(Style::default(), |s, x| s.patch(*x))
    }

    fn text(&mut self, t: &str) {
        let style = self.style();
        self.spans.push(Span::styled(t.to_string(), style));
    }

    /// Handle an inline event; false when it isn't one.
    fn event(&mut self, event: &Event) -> bool {
        match event {
            Event::Text(t) | Event::InlineHtml(t) | Event::InlineMath(t) | Event::FootnoteReference(t) => self.text(t),
            Event::Code(t) => {
                let style = self.style().patch(CODE);
                self.spans.push(Span::styled(t.to_string(), style));
            }
            Event::SoftBreak => self.text(" "),
            Event::Start(Tag::Emphasis) => self.styles.push(Style::new().add_modifier(Modifier::ITALIC)),
            Event::Start(Tag::Strong) => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Event::End(TagEnd::Emphasis | TagEnd::Strong) => {
                self.styles.pop();
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.links.push((dest_url.to_string(), self.spans.len()));
                self.styles.push(LINK);
            }
            Event::End(TagEnd::Link) => {
                self.styles.pop();
                if let Some((url, start)) = self.links.pop() {
                    let text: String = self.spans[start..].iter().map(|s| s.content.as_ref()).collect();
                    // An autolink's text is its URL: once is enough.
                    if self.urls && !url.is_empty() && text != url {
                        self.spans.push(Span::styled(format!(" ({url})"), DIM));
                    }
                }
            }
            // An image is its alt text, which arrives as `Text` in between.
            Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => {}
            _ => return false,
        }
        true
    }
}

/// A block container the current line sits in: each adds to the prefix.
enum Container {
    Quote,
    /// A list item: its marker (`• `, `3. `) leads its first row, spaces of
    /// the same width every later one. `fresh` until that first row is out.
    Item { marker: String, fresh: bool },
}

/// [`render`]: walks the events, emitting each finished block's wrapped
/// rows into `out`.
struct Blocks {
    width: usize,
    out: Vec<Line<'static>>,
    inl: Inlines,
    containers: Vec<Container>,
    /// Per open list: the next number (ordered) or `None` (bullets).
    lists: Vec<Option<u64>>,
    /// The text of the code or HTML block being read.
    code: Option<String>,
    html: Option<String>,
    /// Inside a table: its events are skipped, its source is shown.
    table: bool,
    /// A blank row is owed before the next block (blocks are separated, a
    /// tight list's items are not).
    gap: bool,
}

impl Blocks {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            inl: Inlines::new(true),
            containers: Vec::new(),
            lists: Vec::new(),
            code: None,
            html: None,
            table: false,
            gap: false,
        }
    }

    fn run(mut self, md: &str) -> Vec<Line<'static>> {
        // Tables are parsed only so their source can be shown as is (step 16
        // lays them out); without the option their rows would fold into one
        // paragraph.
        let parser = Parser::new_ext(md, Options::ENABLE_TABLES).into_offset_iter();
        for (event, range) in parser {
            if self.table {
                if matches!(event, Event::End(TagEnd::Table)) {
                    self.table = false;
                }
                continue;
            }
            if let Some(code) = self.code.as_mut().filter(|_| !matches!(event, Event::End(_))) {
                if let Event::Text(t) = &event {
                    code.push_str(t);
                }
                continue;
            }
            if let Some(html) = self.html.as_mut().filter(|_| !matches!(event, Event::End(_))) {
                if let Event::Html(t) | Event::Text(t) = &event {
                    html.push_str(t);
                }
                continue;
            }
            if self.inl.event(&event) {
                continue;
            }
            match event {
                Event::Start(Tag::Paragraph) => self.flush(),
                Event::End(TagEnd::Paragraph) => self.end_block(),
                Event::Start(Tag::Heading { .. }) => {
                    self.flush();
                    self.inl.styles.push(HEADING);
                }
                Event::End(TagEnd::Heading(_)) => {
                    self.flush();
                    self.inl.styles.pop();
                    self.gap = true;
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    self.flush();
                    // Owed before the quote, so the blank row has no `│`.
                    self.pay_gap();
                    self.containers.push(Container::Quote);
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    self.flush();
                    self.containers.pop();
                    self.gap = true;
                }
                Event::Start(Tag::CodeBlock(_)) => {
                    self.flush();
                    self.code = Some(String::new());
                }
                Event::End(TagEnd::CodeBlock) => {
                    let code = self.code.take().unwrap_or_default();
                    let code = code.strip_suffix('\n').unwrap_or(&code);
                    for l in code.split('\n') {
                        self.emit(&[Span::styled(l.to_string(), CODE)], true, Some(CODE));
                    }
                    self.gap = true;
                }
                Event::Start(Tag::HtmlBlock) => {
                    self.flush();
                    self.html = Some(String::new());
                }
                Event::End(TagEnd::HtmlBlock) => {
                    let html = self.html.take().unwrap_or_default();
                    self.source_lines(&html);
                }
                Event::Start(Tag::Table(_)) => {
                    self.flush();
                    self.table = true;
                    self.source_lines(&md[range]);
                }
                Event::Start(Tag::List(start)) => {
                    self.flush();
                    self.lists.push(start);
                }
                Event::End(TagEnd::List(_)) => {
                    self.flush();
                    self.lists.pop();
                    if self.lists.is_empty() {
                        self.gap = true;
                    }
                }
                Event::Start(Tag::Item) => {
                    self.flush();
                    let marker = match self.lists.last_mut() {
                        Some(Some(n)) => {
                            *n += 1;
                            format!("{}. ", *n - 1)
                        }
                        _ => "• ".to_string(),
                    };
                    self.containers.push(Container::Item { marker, fresh: true });
                }
                Event::End(TagEnd::Item) => {
                    self.flush();
                    // An empty item still shows its marker.
                    if matches!(self.containers.last(), Some(Container::Item { fresh: true, .. })) {
                        self.emit(&[], false, None);
                    }
                    self.containers.pop();
                }
                Event::HardBreak => self.flush(),
                Event::Rule => {
                    self.flush();
                    let avail = self.width.saturating_sub(width_of(&self.prefix(false))).max(1);
                    self.emit(&[Span::styled("─".repeat(avail), DIM)], true, None);
                    self.gap = true;
                }
                // Not enabled (math, footnotes, …), or nothing to show.
                _ => {}
            }
        }
        self.flush();
        self.out
    }

    /// Emit the inline text gathered so far as one wrapped logical line.
    fn flush(&mut self) {
        if !self.inl.spans.is_empty() {
            let spans = std::mem::take(&mut self.inl.spans);
            self.emit(&spans, false, None);
        }
    }

    fn end_block(&mut self) {
        self.flush();
        self.gap = true;
    }

    /// Raw HTML and (until step 16) tables: their source lines, wrapped.
    fn source_lines(&mut self, text: &str) {
        for l in text.trim_end_matches('\n').split('\n') {
            self.emit(&[Span::raw(l.trim_end().to_string())], false, None);
        }
        self.gap = true;
    }

    /// The prefix for a row: `│ ` per quote, an item's marker on its first
    /// row (`first`), spaces of the marker's width after that.
    fn prefix(&self, first: bool) -> Vec<Span<'static>> {
        self.containers
            .iter()
            .map(|c| match c {
                Container::Quote => Span::styled("│ ", DIM),
                Container::Item { marker, fresh: true } if first => Span::raw(marker.clone()),
                Container::Item { marker, .. } => Span::raw(" ".repeat(width_of(&[Span::raw(marker.as_str())]))),
            })
            .collect()
    }

    /// The owed blank row, if any: only the quote bars of the prefix.
    fn pay_gap(&mut self) {
        if self.gap && !self.out.is_empty() {
            let bars: String = self.prefix(false).iter().map(|s| s.content.as_ref()).collect();
            let bars = bars.trim_end().to_string();
            self.out.push(if bars.is_empty() { Line::default() } else { Line::from(Span::styled(bars, DIM)) });
        }
        self.gap = false;
    }

    /// Wrap `content` under the current prefixes into `out`; `fill` pads each
    /// row to the width (a code block reads as one tinted box).
    fn emit(&mut self, content: &[Span<'static>], hard: bool, fill: Option<Style>) {
        self.pay_gap();
        let lines = wrap(content, self.width, &self.prefix(true), &self.prefix(false), hard);
        for mut line in lines {
            if let Some(fill) = fill {
                let pad = self.width.saturating_sub(line.width());
                if pad > 0 {
                    line.spans.push(Span::styled(" ".repeat(pad), fill));
                }
            }
            self.out.push(line);
        }
        for c in &mut self.containers {
            if let Container::Item { fresh, .. } = c {
                *fresh = false;
            }
        }
    }
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

/// Append `c` to `spans`, extending the last span when it has the same style.
fn push_char(spans: &mut Vec<Span<'static>>, c: char, style: Style) {
    match spans.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push(c),
        _ => spans.push(Span::styled(c.to_string(), style)),
    }
}

/// Drop trailing spaces from a row's content (not its prefix: `content`
/// holds only what came after it).
fn trim_end(content: &mut Vec<Span<'static>>) {
    while let Some(last) = content.last_mut() {
        let trimmed = last.content.trim_end_matches(' ').len();
        if trimmed == 0 {
            content.pop();
        } else {
            last.content.to_mut().truncate(trimmed);
            break;
        }
    }
}

/// Wrap one logical line of styled `content` into rows of at most `width`
/// display columns, `first` leading the first row and `cont` every later
/// one. Word wrap breaks at spaces (dropped at the break) and splits a word
/// wider than the row; `hard` (code) breaks at the width, keeping every
/// space. Always at least one row, so an empty line still takes its row.
pub fn wrap(content: &[Span<'static>], width: usize, first: &[Span<'static>], cont: &[Span<'static>], hard: bool) -> Vec<Line<'static>> {
    let width = width.max(1);
    let cells: Vec<(char, Style)> =
        content.iter().flat_map(|s| s.content.chars().map(move |c| (c, s.style))).collect();
    let mut rows = Rows { out: Vec::new(), row: first.to_vec(), at: first.len(), used: width_of(first), cont, hard };
    let mut wrapped = false;
    let mut i = 0;
    while i < cells.len() {
        let space = cells[i].0 == ' ';
        let end = if hard { i + 1 } else { i + cells[i..].iter().take_while(|(c, _)| (*c == ' ') == space).count() };
        let run = &cells[i..end];
        i = end;
        if space && !hard {
            let w = run.len();
            if rows.empty() && wrapped {
                continue; // a row never starts with the spaces it broke at
            }
            if rows.used + w <= width {
                run.iter().for_each(|(c, s)| rows.push(*c, *s, 1));
            } else if !rows.empty() {
                rows.newline();
                wrapped = true;
            }
            continue;
        }
        let w: usize = run.iter().map(|(c, _)| char_width(*c)).sum();
        if !hard && !rows.empty() && rows.used + w > width {
            rows.newline();
            wrapped = true;
        }
        for (c, s) in run {
            let cw = char_width(*c);
            if !rows.empty() && rows.used + cw > width {
                rows.newline();
                wrapped = true;
            }
            rows.push(*c, *s, cw);
        }
    }
    rows.newline();
    rows.out
}

/// [`wrap`]'s output so far and the row being filled.
struct Rows<'a> {
    out: Vec<Line<'static>>,
    /// The row: its prefix spans, then content from index `at`.
    row: Vec<Span<'static>>,
    at: usize,
    used: usize,
    cont: &'a [Span<'static>],
    hard: bool,
}

impl Rows<'_> {
    fn empty(&self) -> bool {
        self.row.len() == self.at
    }

    fn push(&mut self, c: char, style: Style, width: usize) {
        if self.empty() {
            self.row.push(Span::styled(c.to_string(), style));
        } else {
            push_char(&mut self.row, c, style);
        }
        self.used += width;
    }

    fn newline(&mut self) {
        let mut content = self.row.split_off(self.at);
        if !self.hard {
            trim_end(&mut content);
        }
        let mut row = std::mem::replace(&mut self.row, self.cont.to_vec());
        row.append(&mut content);
        self.out.push(Line::from(row));
        self.at = self.cont.len();
        self.used = width_of(self.cont);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(text).collect()
    }

    fn full(md: &str, width: u16) -> Vec<String> {
        texts(&render(md, width))
    }

    /// The style of the first span containing `needle`.
    fn style_of(lines: &[Line], needle: &str) -> Style {
        lines.iter().flat_map(|l| &l.spans).find(|s| s.content.contains(needle)).unwrap().style
    }

    #[test]
    fn empty_input_renders_nothing() {
        assert!(render("", 20).is_empty());
        assert!(inline_spans("").is_empty());
        assert!(raw("", 20).is_empty());
    }

    #[test]
    fn paragraphs_wrap_at_spaces_and_are_separated() {
        assert_eq!(full("one two three four\n\nnext", 9), ["one two", "three", "four", "", "next"]);
        // Soft breaks join; a hard break (two spaces) doesn't.
        assert_eq!(full("a\nb  \nc", 20), ["a b", "c"]);
    }

    #[test]
    fn a_word_wider_than_the_row_is_split() {
        assert_eq!(full("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(full("ab abcdefgh", 4), ["ab", "abcd", "efgh"]);
    }

    #[test]
    fn wide_glyphs_count_two_columns() {
        // 世界 is 4 columns: two fit in 5, the third goes to the next row.
        assert_eq!(full("世界世", 5), ["世界", "世"]);
        assert_eq!(full("a 世界", 5), ["a", "世界"]);
        for l in render("世界世界世界 世界", 5) {
            assert!(l.width() <= 5, "{l:?}");
        }
    }

    #[test]
    fn headings_are_bold_and_accented() {
        let lines = render("# Title\ntext", 20);
        assert_eq!(texts(&lines), ["Title", "", "text"]);
        assert_eq!(style_of(&lines, "Title"), HEADING);
    }

    #[test]
    fn emphasis_strong_and_inline_code() {
        let lines = render("*it* **bold** `x = 1`", 40);
        assert_eq!(texts(&lines), ["it bold x = 1"]);
        assert!(style_of(&lines, "it").add_modifier.contains(Modifier::ITALIC));
        assert!(style_of(&lines, "bold").add_modifier.contains(Modifier::BOLD));
        assert_eq!(style_of(&lines, "x = 1"), CODE);
    }

    #[test]
    fn code_blocks_are_tinted_padded_and_wrapped_hard() {
        let lines = render("```rust\nfn main() {}\n\n  let x = 1;\n```\nafter", 10);
        assert_eq!(texts(&lines), ["fn main() ", "{}        ", "          ", "  let x = ", "1;        ", "", "after"]);
        for l in &lines[..5] {
            assert!(l.spans.iter().all(|s| s.style == CODE), "{l:?}");
        }
        // No highlighting, no markdown inside.
        assert_eq!(full("    *x*", 10), ["*x*       "]);
    }

    #[test]
    fn bullet_lists_hang_their_wrapped_rows() {
        assert_eq!(full("- one two three\n- four", 9), ["• one two", "  three", "• four"]);
    }

    #[test]
    fn numbered_lists_count_from_their_start() {
        assert_eq!(full("3. a\n4. b b b", 7), ["3. a", "4. b b", "   b"]);
        assert_eq!(full("9. a\n10. b", 20), ["9. a", "10. b"]);
    }

    #[test]
    fn nested_lists_indent_under_their_item() {
        assert_eq!(full("- a\n  - b c d\n  - e\n- f", 7), ["• a", "  • b c", "    d", "  • e", "• f"]);
        assert_eq!(full("1. a\n   - b", 20), ["1. a", "   • b"]);
    }

    #[test]
    fn an_item_with_paragraphs_keeps_them_under_its_marker() {
        assert_eq!(full("- first\n\n  second\n- next", 20), ["• first", "", "  second", "", "• next"]);
        // A code block in an item sits in the hanging indent.
        let lines = full("- run:\n\n  ```\n  make\n  ```", 10);
        assert_eq!(lines, ["• run:", "", "  make    "]);
    }

    #[test]
    fn quotes_bar_every_row() {
        let lines = render("> one two three\n>\n> four", 9);
        assert_eq!(texts(&lines), ["│ one two", "│ three", "│", "│ four"]);
        assert_eq!(lines[1].spans[0].style, DIM);
        // The gap before a quote has no bar.
        assert_eq!(full("a\n\n> b", 9), ["a", "", "│ b"]);
        assert_eq!(full("> - a b c", 7), ["│ • a b", "│   c"]);
    }

    #[test]
    fn rules_span_the_width() {
        let lines = render("a\n\n---\n\nb", 5);
        assert_eq!(texts(&lines), ["a", "", "─────", "", "b"]);
        assert_eq!(style_of(&lines, "─"), DIM);
    }

    #[test]
    fn links_are_underlined_with_a_dim_url() {
        let lines = render("see [the PR](https://x/1) or <https://y>", 80);
        assert_eq!(texts(&lines), ["see the PR (https://x/1) or https://y"]);
        assert_eq!(style_of(&lines, "the PR"), LINK);
        assert_eq!(style_of(&lines, "(https://x/1)"), DIM);
        // Inline: the text only.
        assert_eq!(text(&Line::from(inline_spans("[the PR](https://x/1)"))), "the PR");
    }

    #[test]
    fn html_is_literal_and_images_are_their_alt_text() {
        assert_eq!(full("a <b>bold</b> c", 40), ["a <b>bold</b> c"]);
        assert_eq!(full("<div>\nhi\n</div>", 40), ["<div>", "hi", "</div>"]);
        assert_eq!(full("![a chart](https://x/c.png)", 40), ["a chart"]);
    }

    #[test]
    fn tables_stay_as_their_source() {
        assert_eq!(full("| a | b |\n|---|---|\n| 1 | 2 |\n\nafter", 40), ["| a | b |", "|---|---|", "| 1 | 2 |", "", "after"]);
    }

    #[test]
    fn inline_mode_is_one_line() {
        let spans = inline_spans("fix `parse`\nfor **all**\n\ninputs");
        let line = Line::from(spans.clone());
        assert_eq!(text(&line), "fix parse for all inputs");
        assert_eq!(style_of(&[line], "parse"), CODE);
        // Block syntax at the start stays literal.
        for title in ["1. fix", "- wip", "# 42", "> quoted", "* star", "---", "```x", "[a]: b"] {
            assert_eq!(text(&Line::from(inline_spans(title))), title, "{title}");
        }
        // Emphasis at the start still renders.
        assert_eq!(text(&Line::from(inline_spans("*wip* x"))), "wip x");
        assert_eq!(text(&Line::from(inline_spans("#7001 docs"))), "#7001 docs");
        // Wrapped by the caller.
        assert_eq!(texts(&wrap(&inline_spans("a b\nc"), 3, &[], &[], false)), ["a b", "c"]);
    }

    #[test]
    fn raw_wraps_the_source_unstyled() {
        let lines = raw("# T\n**b** and more", 8);
        assert_eq!(texts(&lines), ["# T", "**b**", "and more"]);
        assert!(lines.iter().flat_map(|l| &l.spans).all(|s| s.style == Style::default()));
    }

    #[test]
    fn control_characters_never_reach_a_span() {
        let lines = render("a\x1b[31mb\u{9b}c\t`d\x07`", 40);
        // The tab goes to the next stop: column 8, once ESC and CSI are gone.
        assert_eq!(texts(&lines), ["a[31mbc d"]);
        assert_eq!(text(&Line::from(inline_spans("x\x1b]0;t\x07"))), "x]0;t");
    }

    #[test]
    fn wrap_prefixes_and_trailing_spaces() {
        let first = [Span::raw("> ")];
        let cont = [Span::raw("  ")];
        let lines = wrap(&[Span::raw("aa bb cc")], 6, &first, &cont, false);
        assert_eq!(texts(&lines), ["> aa", "  bb", "  cc"]);
        // Leading spaces of the first row are content (indentation); hard
        // mode keeps every space.
        assert_eq!(texts(&wrap(&[Span::raw("  x")], 6, &[], &[], false)), ["  x"]);
        assert_eq!(texts(&wrap(&[Span::raw("ab  cd")], 3, &[], &[], true)), ["ab ", " cd"]);
        assert_eq!(texts(&wrap(&[], 6, &first, &cont, false)), ["> "]);
    }
}
