//! Mouse text selection (docs/tui-selection.md): the regions each frame makes
//! selectable, the drag state, and reading and highlighting the selected
//! cells. Pure: a frame buffer in, text out; `App` routes the mouse, `ui`
//! registers the regions and patches the highlight into the frame it drew.
//!
//! Positions are in a region's own coordinates ([`Pos`]), not screen cells,
//! so a scrolled document ([`Source::Rows`]; step 3's vt100 scrollback
//! likewise) counts rows in content and keeps a selection across a scroll.
//! A [`Source::Screen`] region is the identity case: row 0 is its rect's
//! top.

use std::rc::Rc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// The pane a region is. One region per id per frame, so a selection can
/// find its region again in the next frame after a redraw moved it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionId {
    InstancesTree,
    Detail,
    ServicesTable,
    ServiceDetail,
    PortsTable,
    InboxList,
    InboxThread,
    Terminal,
    ConfigLeft,
    ConfigRight,
    TextModal,
}

/// Where a region's text comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The cells of the rect in the frame buffer, as drawn.
    Screen,
    /// A scrolled document whose every rendered row the draw had: `rows[scroll]`
    /// is the rect's top. Positions are content rows, so a selection outlives
    /// a scroll and can span rows that are off screen. `Rc`: the region is
    /// cloned per mouse event and frame.
    Rows { scroll: usize, rows: Rc<[RowText]> },
}

/// How a [`RowText`] continues the row before it when copied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Join {
    /// It doesn't: a line of its own.
    #[default]
    Newline,
    /// A word-wrap break, which dropped the space there: one space.
    Space,
    /// A word split mid-way, or a code row broken at the width keeping
    /// every space: nothing.
    Tight,
}

/// What a renderer knows about one row it wrapped (`markdown::wrap_rows`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowMeta {
    pub join: Join,
    /// Display columns of decoration leading the row (a hanging indent, the
    /// `│ ` of a quote): not text a continuation contributes to its line.
    pub prefix: u16,
    /// Trailing columns of padding that aren't text (a code block's tint).
    pub fill: u16,
}

/// One row of a [`Source::Rows`] document: its text as drawn (minus
/// [`RowMeta::fill`]) and how it joins the row before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowText {
    pub text: String,
    pub join: Join,
    pub prefix: u16,
}

impl RowText {
    /// A line of its own (config, inspect, help, logs: one row per line).
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), join: Join::Newline, prefix: 0 }
    }

    pub fn new(line: &Line, meta: RowMeta) -> Self {
        let mut text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // The fill is spaces, a byte a column.
        text.truncate(text.len().saturating_sub(meta.fill as usize));
        Self { text, join: meta.join, prefix: meta.prefix }
    }
}

/// A selectable area of the last frame. `rect` is a pane's *inner* text area,
/// so its borders and title are out of any selection by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub id: RegionId,
    pub rect: Rect,
    pub source: Source,
}

/// A cell in region coordinates. `row` first, so the derived order is
/// reading order; `usize` because a content row (step 2/3 sources) can lie
/// far past the visible rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub row: usize,
    pub col: u16,
}

impl Region {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        self.rect.contains(Position::new(col, row))
    }

    /// The position under screen cell `(col, row)`, clamped to the region so
    /// a drag past its edge never leaves it: above the rect is its first
    /// visible cell, below it its last (the whole rest of the stream), left
    /// or right of it the edge column of that row.
    pub fn pos_at(&self, col: u16, row: u16) -> Pos {
        let r = self.rect;
        let top = self.scroll();
        if r.is_empty() || row < r.y {
            return Pos { row: top, col: 0 };
        }
        if row >= r.bottom() {
            return Pos { row: top + (r.height - 1) as usize, col: r.width - 1 };
        }
        let col = col.clamp(r.x, r.right() - 1) - r.x;
        Pos { row: top + (row - r.y) as usize, col }
    }

    /// The region row at the rect's top.
    pub fn scroll(&self) -> usize {
        match self.source {
            Source::Screen => 0,
            Source::Rows { scroll, .. } => scroll,
        }
    }

    /// Screen row of region row `row`, `None` when it isn't on screen.
    fn screen_row(&self, row: usize) -> Option<u16> {
        let on = row.checked_sub(self.scroll()).filter(|&r| r < self.rect.height as usize);
        on.map(|r| self.rect.y + r as u16)
    }
}

/// A press inside a region that may become a selection: the first drag to
/// another cell turns it into one; released in place it was a click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub region: RegionId,
    pub anchor: Pos,
    /// Screen cell pressed, to tell a drag from a click.
    pub at: (u16, u16),
}

/// A stream selection from `anchor` (where the drag began) to `head` (where
/// the pointer is), both inclusive, in `region`'s coordinates. `dragging`
/// while the button is held; it stays shown after release until cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub region: RegionId,
    pub anchor: Pos,
    pub head: Pos,
    pub dragging: bool,
}

impl Selection {
    /// `(from, to)` in reading order, whichever way the drag went.
    pub fn range(&self) -> (Pos, Pos) {
        ordered(self.anchor, self.head)
    }
}

pub fn ordered(a: Pos, b: Pos) -> (Pos, Pos) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The inclusive column span the stream `from..=to` covers on `row` of a
/// `width`-column region: a partial first row, full middle rows, a partial
/// last row; `None` off the range.
pub fn row_span(from: Pos, to: Pos, row: usize, width: u16) -> Option<(u16, u16)> {
    if row < from.row || row > to.row || width == 0 {
        return None;
    }
    let start = if row == from.row { from.col } else { 0 };
    let end = if row == to.row { to.col } else { width - 1 };
    (start <= end).then_some((start, end.min(width - 1)))
}

/// The glyphs of screen row `y` within `rect` as `(column from rect.x,
/// width, symbol)`. A wide glyph's continuation cells (ratatui resets them
/// to `" "`; some widgets leave `""`) are skipped, so they neither
/// duplicate text nor add a space.
fn glyphs(buf: &Buffer, rect: Rect, y: u16) -> Vec<(u16, u16, &str)> {
    let mut out = Vec::new();
    let mut x = rect.x;
    while x < rect.right() {
        let Some(cell) = buf.cell((x, y)) else { break };
        let sym = cell.symbol();
        let w = (Span::raw(sym).width() as u16).max(1);
        out.push((x - rect.x, w, sym));
        x = x.saturating_add(w);
    }
    out
}

/// The text of the stream `from..=to` of `rect` in `buf`: one line per row,
/// trailing spaces trimmed, joined with `\n`. A wide glyph is taken whole
/// when any of its cells is selected.
pub fn extract_screen(buf: &Buffer, rect: Rect, from: Pos, to: Pos) -> String {
    let rect = rect.intersection(buf.area);
    let region = Region { id: RegionId::Detail, rect, source: Source::Screen };
    let mut lines = Vec::new();
    for row in from.row..=to.row {
        let (Some(y), Some((c0, c1))) = (region.screen_row(row), row_span(from, to, row, rect.width)) else {
            break;
        };
        let line: String = glyphs(buf, rect, y)
            .into_iter()
            .filter(|&(x, w, _)| x + w > c0 && x <= c1)
            .map(|(_, _, s)| s)
            .collect();
        lines.push(line.trim_end_matches(' ').to_string());
    }
    lines.join("\n")
}

/// The text of the stream `from..=to` of a [`Source::Rows`] document shown
/// `width` columns wide, from its rows rather than the screen, so rows
/// scrolled out of view are copied too. Rows join as their [`Join`] says,
/// a continuation without its `prefix` decoration; trailing spaces are
/// trimmed per line, not at a [`Join::Tight`] break (they're code).
///
/// A row reaching the last visible column (always a middle row; the last
/// one when the drag got to the right edge) is taken to its end: a line
/// wider than the pane (config, inspect: `Paragraph` cuts them at the edge)
/// is copied whole instead of cut where the pane happens to end.
pub fn extract_rows(rows: &[RowText], width: u16, from: Pos, to: Pos) -> String {
    let mut out = String::new();
    let last = to.row.min(rows.len().saturating_sub(1));
    for (i, r) in rows.iter().enumerate().take(last + 1).skip(from.row) {
        let Some((c0, c1)) = row_span(from, to, i, width) else { continue };
        let mut start = c0 as usize;
        let end = if c1 + 1 >= width { usize::MAX } else { c1 as usize };
        if i > from.row {
            match r.join {
                Join::Newline => {
                    trim_line_end(&mut out);
                    out.push('\n');
                }
                Join::Space => {
                    trim_line_end(&mut out);
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push(' ');
                    }
                    start = start.max(r.prefix as usize);
                }
                Join::Tight => start = start.max(r.prefix as usize),
            }
        }
        out.push_str(&cols(&r.text, start, end));
    }
    trim_line_end(&mut out);
    out
}

fn trim_line_end(s: &mut String) {
    s.truncate(s.trim_end_matches(' ').len());
}

/// The glyphs of `text` covering display columns `start..=end`, a wide one
/// whole when any of its cells is in (as [`extract_screen`] does); a
/// zero-width one goes with the glyph before it.
fn cols(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let (mut x, mut taken) = (0usize, false);
    let mut buf = [0u8; 4];
    for c in text.chars() {
        let w = Span::raw(&*c.encode_utf8(&mut buf)).width();
        if w > 0 {
            taken = x + w > start && x <= end;
        }
        if taken {
            out.push(c);
        }
        x += w;
        if x > end {
            break;
        }
    }
    out
}

/// The selected text of `region` as drawn in `buf`.
pub fn extract(buf: &Buffer, region: &Region, sel: &Selection) -> String {
    let (from, to) = sel.range();
    match &region.source {
        Source::Screen => extract_screen(buf, region.rect, from, to),
        Source::Rows { rows, .. } => extract_rows(rows, region.rect.width, from, to),
    }
}

/// Reverse the selected cells of `region` in `buf`, whole glyphs like
/// [`extract_screen`], so what's highlighted is what gets copied.
pub fn highlight(buf: &mut Buffer, region: &Region, sel: &Selection) {
    let (from, to) = sel.range();
    let rect = region.rect.intersection(buf.area);
    let reversed = Style::default().add_modifier(Modifier::REVERSED);
    // Only the rows on screen: a scrolled selection may start above it.
    let top = from.row.max(region.scroll());
    let bottom = to.row.min(region.scroll() + rect.height as usize);
    for row in top..=bottom {
        let (Some(y), Some((c0, c1))) = (region.screen_row(row), row_span(from, to, row, rect.width)) else {
            continue;
        };
        let cells: Vec<(u16, u16)> = glyphs(buf, rect, y)
            .into_iter()
            .filter(|&(x, w, _)| x + w > c0 && x <= c1)
            .map(|(x, w, _)| (x, w))
            .collect();
        for (x, w) in cells {
            for dx in 0..w.min(rect.width - x) {
                if let Some(cell) = buf.cell_mut((rect.x + x + dx, y)) {
                    cell.set_style(reversed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(row: usize, col: u16) -> Pos {
        Pos { row, col }
    }

    /// A buffer with `rows` drawn at `(1, 1)`, inside a 1-cell frame of
    /// `#`, and the inner rect they fill.
    fn framed(rows: &[&str], width: u16) -> (Buffer, Rect) {
        let inner = Rect::new(1, 1, width, rows.len() as u16);
        let mut buf = Buffer::filled(Rect::new(0, 0, width + 2, inner.height + 2), ratatui::buffer::Cell::new("#"));
        for y in inner.y..inner.bottom() {
            for x in inner.x..inner.right() {
                buf[(x, y)].set_symbol(" ");
            }
        }
        for (i, r) in rows.iter().enumerate() {
            buf.set_string(inner.x, inner.y + i as u16, r, Style::default());
        }
        (buf, inner)
    }

    fn sel(anchor: Pos, head: Pos) -> Selection {
        Selection { region: RegionId::Detail, anchor, head, dragging: false }
    }

    #[test]
    fn single_row_forward_and_backward() {
        let (buf, r) = framed(&["hello world"], 12);
        assert_eq!(extract_screen(&buf, r, pos(0, 0), pos(0, 4)), "hello");
        let (from, to) = sel(pos(0, 10), pos(0, 6)).range();
        assert_eq!(extract_screen(&buf, r, from, to), "world");
    }

    #[test]
    fn multi_row_partial_ends_and_trim() {
        let (buf, r) = framed(&["first line", "middle", "last line"], 12);
        // From "line" on row 0 to "la" on row 2; the middle row is whole,
        // and nothing of the `#` frame gets in.
        let (from, to) = sel(pos(2, 1), pos(0, 6)).range();
        assert_eq!(extract_screen(&buf, r, from, to), "line\nmiddle\nla");
        // Trailing blanks of a full row are trimmed, a selected run of
        // blanks alone is an empty line.
        assert_eq!(extract_screen(&buf, r, pos(1, 0), pos(2, 11)), "middle\nlast line");
        assert_eq!(extract_screen(&buf, r, pos(1, 7), pos(1, 11)), "");
    }

    #[test]
    fn wide_glyphs_are_taken_whole_once() {
        let (buf, r) = framed(&["a日本b"], 8);
        assert_eq!(extract_screen(&buf, r, pos(0, 0), pos(0, 7)), "a日本b");
        // Starting on 日's continuation cell or ending on 本's first one
        // still takes the glyph, once.
        assert_eq!(extract_screen(&buf, r, pos(0, 2), pos(0, 3)), "日本");
        assert_eq!(extract_screen(&buf, r, pos(0, 4), pos(0, 4)), "本");
    }

    #[test]
    fn pos_at_clamps_to_the_region() {
        let region = Region { id: RegionId::Detail, rect: Rect::new(5, 3, 10, 4), source: Source::Screen };
        assert_eq!(region.pos_at(7, 4), pos(1, 2));
        // Left / right of it: the edge column of that row.
        assert_eq!(region.pos_at(0, 4), pos(1, 0));
        assert_eq!(region.pos_at(50, 4), pos(1, 9));
        // Above: the start; below: the end.
        assert_eq!(region.pos_at(9, 0), pos(0, 0));
        assert_eq!(region.pos_at(0, 30), pos(3, 9));
        assert!(region.contains(5, 3) && !region.contains(15, 3) && !region.contains(5, 7));
    }

    #[test]
    fn row_span_streams() {
        let (from, to) = (pos(1, 3), pos(3, 2));
        assert_eq!(row_span(from, to, 0, 10), None);
        assert_eq!(row_span(from, to, 1, 10), Some((3, 9)));
        assert_eq!(row_span(from, to, 2, 10), Some((0, 9)));
        assert_eq!(row_span(from, to, 3, 10), Some((0, 2)));
        assert_eq!(row_span(from, to, 4, 10), None);
        assert_eq!(row_span(pos(0, 4), pos(0, 4), 0, 10), Some((4, 4)));
    }

    fn row(text: &str, join: Join, prefix: u16) -> RowText {
        RowText { text: text.into(), join, prefix }
    }

    #[test]
    fn rows_join_as_they_were_wrapped() {
        let rows = [
            row("• one two", Join::Newline, 2),
            row("  three", Join::Space, 2),
            row("│ ab ", Join::Newline, 2),
            row("│  cd", Join::Tight, 2),
            row("plain", Join::Newline, 0),
        ];
        // Whole: a continuation drops its prefix, a soft break gets one
        // space, a hard one keeps the code's spaces at the break.
        assert_eq!(extract_rows(&rows, 9, pos(0, 0), pos(4, 8)), "• one two three\n│ ab  cd\nplain");
        // Partial ends cut by display column; the joined row's cut counts
        // its prefix columns (dropped) like the screen shows them.
        assert_eq!(extract_rows(&rows, 9, pos(0, 6), pos(1, 3)), "two th");
        // Starting on a continuation, there's nothing to join: as shown.
        assert_eq!(extract_rows(&rows, 9, pos(1, 0), pos(1, 6)), "  three");
        // Past the content: up to its end.
        assert_eq!(extract_rows(&rows, 9, pos(4, 0), pos(9, 8)), "plain");
        assert_eq!(extract_rows(&[], 9, pos(0, 0), pos(3, 3)), "");
    }

    #[test]
    fn rows_wider_than_the_region_are_taken_whole_from_the_edge() {
        let rows = [RowText::plain("key = \"a very long value\""), RowText::plain("x = 日本語")];
        assert_eq!(extract_rows(&rows, 8, pos(0, 0), pos(0, 7)), "key = \"a very long value\"");
        assert_eq!(extract_rows(&rows, 8, pos(0, 0), pos(0, 2)), "key");
        // A middle row is whole; wide glyphs count two columns, taken whole.
        assert_eq!(extract_rows(&rows, 8, pos(0, 6), pos(1, 4)), "\"a very long value\"\nx = 日");
        assert_eq!(extract_rows(&rows, 8, pos(1, 5), pos(1, 6)), "日本");
    }

    #[test]
    fn row_text_drops_the_fill() {
        let line = Line::from(vec![Span::raw("  ab "), Span::raw("   ")]);
        let meta = RowMeta { join: Join::Tight, prefix: 2, fill: 3 };
        assert_eq!(RowText::new(&line, meta), row("  ab ", Join::Tight, 2));
    }

    #[test]
    fn a_scrolled_region_maps_content_rows() {
        let rows: Rc<[RowText]> = (0..20).map(|i| RowText::plain(format!("r{i}"))).collect();
        let region = Region { id: RegionId::Detail, rect: Rect::new(1, 1, 4, 3), source: Source::Rows { scroll: 5, rows } };
        assert_eq!(region.pos_at(2, 2), pos(6, 1));
        assert_eq!(region.pos_at(2, 0), pos(5, 0), "above: the first visible row");
        assert_eq!(region.pos_at(2, 9), pos(7, 3), "below: the last visible row");
        // Highlighted only where on screen: rows 5-7 of a 2..=9 selection.
        let (mut buf, _) = framed(&["r5", "r6", "r7"], 4);
        highlight(&mut buf, &region, &sel(pos(2, 0), pos(9, 1)));
        let rev = |x, y| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        assert!((1..5).all(|x| (1..4).all(|y| rev(x, y))));
        assert!(!rev(0, 1) && !rev(1, 4) && !rev(1, 0));
    }

    #[test]
    fn highlight_reverses_exactly_the_selection() {
        let (mut buf, r) = framed(&["abc", "de日"], 4);
        let region = Region { id: RegionId::Detail, rect: r, source: Source::Screen };
        highlight(&mut buf, &region, &sel(pos(1, 3), pos(0, 2)));
        let rev = |x, y| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        // Row 0 from col 2 to the end, row 1 up to col 3: 日 (cols 2-3) whole.
        let got: Vec<(u16, u16)> =
            (0..buf.area.height).flat_map(|y| (0..buf.area.width).map(move |x| (x, y))).filter(|&(x, y)| rev(x, y)).collect();
        assert_eq!(got, [(3, 1), (4, 1), (1, 2), (2, 2), (3, 2), (4, 2)]);
    }
}
