//! A wrapping multi-line text input (the Inbox reply box), plus the `:`
//! prompt's horizontal scroll. Editing reuses [`Prompt`]'s primitives; this
//! module adds the layer they lack: wrap by display width, a viewport that
//! follows the cursor, and row-aware cursor movement. Layout is pure fns over
//! `(text, cursor, width)` so it is unit-testable; [`TextArea`] only keeps the
//! viewport's first row and the last wrap width between frames.
//!
//! Wrapping is by character, not by word: every cursor position then has its
//! own cell, so the caret never lands on a cell shared with another position.
//! The cursor needs a cell of its own after the last char of a row and at the
//! end of the text, so those slots take one column like a char: a row filled
//! to the exact width puts the caret at the start of the next row.

use std::cell::Cell;
use std::ops::Range;

use ratatui::text::Span;

use super::prompt::Prompt;

/// Content rows the box grows to before it scrolls vertically.
pub const MAX_ROWS: u16 = 6;

/// Display width of `c` in columns, through ratatui's unicode-width (no direct
/// dependency). A wide glyph is 2, a combining mark 0.
fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

/// `text` wrapped at `width` columns.
#[derive(Debug, PartialEq, Eq)]
pub struct Wrapped {
    /// Char range of each visual row's text (a `\n` belongs to no row).
    pub rows: Vec<Range<usize>>,
    /// `(row, col)` of every cursor position `0..=char_len`.
    pub pos: Vec<(usize, usize)>,
}

impl Wrapped {
    /// The positions on `row`, in order (never empty for a row that exists).
    fn on_row(&self, row: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.pos.iter().enumerate().filter(move |(_, p)| p.0 == row).map(|(k, p)| (k, p.1))
    }
}

/// Wrap `text` by display width. A char that doesn't fit in what's left of a
/// row starts the next one (a wide glyph is never split); a `\n` ends its row;
/// a glyph wider than the whole row still gets a row of its own. The `\n` and
/// end-of-text cursor slots take a column, see the module doc.
pub fn wrap(text: &str, width: usize) -> Wrapped {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut rows = Vec::new();
    let mut pos = Vec::with_capacity(n + 1);
    let (mut row, mut col, mut start) = (0, 0, 0);
    for k in 0..=n {
        let c = chars.get(k).copied();
        let w = match c {
            None | Some('\n') => 1,
            Some(c) => char_width(c),
        };
        if col > 0 && col + w > width {
            rows.push(start..k);
            (row, col, start) = (row + 1, 0, k);
        }
        pos.push((row, col));
        if c == Some('\n') {
            rows.push(start..k);
            (row, col, start) = (row + 1, 0, k + 1);
        } else {
            col += w;
        }
    }
    rows.push(start..n);
    Wrapped { rows, pos }
}

/// Content rows the box shows for `rows` wrapped rows: 1 to [`MAX_ROWS`].
pub fn height(rows: usize) -> u16 {
    (rows.min(MAX_ROWS as usize) as u16).max(1)
}

/// First visible row: the previous one, moved just enough that `cursor_row`
/// is in the `height`-row window and the window doesn't run past `rows`.
pub fn viewport_top(prev: usize, cursor_row: usize, rows: usize, height: usize) -> usize {
    let height = height.max(1);
    let top = prev.min(rows.saturating_sub(height));
    if cursor_row < top {
        cursor_row
    } else if cursor_row >= top + height {
        cursor_row + 1 - height
    } else {
        top
    }
}

/// What a `height`-row window over `text` shows: its first row, the visible
/// rows' text and the caret's `(col, row)` within the window.
#[derive(Debug, PartialEq, Eq)]
pub struct Viewport {
    pub top: usize,
    pub lines: Vec<String>,
    pub caret: (u16, u16),
}

/// The window over `text` wrapped at `width`, `height` rows tall, scrolled
/// from `prev_top` so the cursor is in it.
pub fn viewport(text: &str, cursor: usize, width: usize, height: usize, prev_top: usize) -> Viewport {
    let w = wrap(text, width);
    let chars: Vec<char> = text.chars().collect();
    let (row, col) = w.pos[cursor.min(chars.len())];
    let top = viewport_top(prev_top, row, w.rows.len(), height);
    let lines = w.rows[top..].iter().take(height.max(1)).map(|r| chars[r.clone()].iter().collect()).collect();
    Viewport { top, lines, caret: (col as u16, (row - top) as u16) }
}

/// Position of the cursor one visual row up (`up`) or down from `cursor`, at
/// the nearest column not past its own. Past the first row it goes to the
/// start, past the last to the end, like most editors.
pub fn vertical(text: &str, cursor: usize, width: usize, up: bool) -> usize {
    let w = wrap(text, width);
    let last = w.pos.len() - 1;
    let (row, col) = w.pos[cursor.min(last)];
    let target = match (up, row) {
        (true, 0) => return 0,
        (true, r) => r - 1,
        (false, r) if r + 1 >= w.rows.len() => return last,
        (false, r) => r + 1,
    };
    let mut best = None;
    for (k, c) in w.on_row(target) {
        if best.is_none() || c <= col {
            best = Some(k);
        }
    }
    best.unwrap_or(cursor)
}

/// Start (`home`) or end of the cursor's visual row.
pub fn row_edge(text: &str, cursor: usize, width: usize, home: bool) -> usize {
    let w = wrap(text, width);
    let row = w.pos[cursor.min(w.pos.len() - 1)].0;
    let mut on = w.on_row(row).map(|(k, _)| k);
    let edge = if home { on.next() } else { on.last() };
    edge.unwrap_or(cursor)
}

/// The `:` prompt's horizontal scroll over a single line `width` columns
/// wide: the char index the visible part starts at, that part, and the
/// caret's column. Unscrolled while the caret fits; past that the caret sits
/// on the last column. A wide glyph is never cut: one straddling the left
/// edge is dropped whole, one past the right edge isn't drawn.
pub fn hscroll(text: &str, cursor: usize, width: usize) -> (usize, String, u16) {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let caret_col: usize = chars[..cursor].iter().map(|&c| char_width(c)).sum();
    let need = (caret_col + 1).saturating_sub(width);
    let (mut start, mut skipped) = (0, 0);
    while skipped < need && start < cursor {
        skipped += char_width(chars[start]);
        start += 1;
    }
    let mut shown = String::new();
    let mut col = 0;
    for &c in &chars[start..] {
        let w = char_width(c);
        if col + w > width {
            break;
        }
        shown.push(c);
        col += w;
    }
    (start, shown, (caret_col - skipped) as u16)
}

/// The wrapping input: [`Prompt`]'s edits (no history, no completion) over
/// text that may hold `\n`, with the viewport state the renderer updates.
pub struct TextArea {
    edit: Prompt,
    /// First visible row as last drawn, so the window only moves when the
    /// cursor leaves it.
    top: Cell<usize>,
    /// Wrap width as last drawn, for the row-aware keys (`↑`/`↓`/home/end).
    /// `usize::MAX` (never drawn) wraps at newlines only.
    width: Cell<usize>,
}

impl Default for TextArea {
    fn default() -> Self {
        Self::new()
    }
}

impl TextArea {
    pub fn new() -> Self {
        Self { edit: Prompt::new(Vec::new()), top: Cell::new(0), width: Cell::new(usize::MAX) }
    }

    /// Pre-filled with `text`, the cursor at its end: a form's text question
    /// opened on its current answer.
    pub fn with_text(text: &str) -> Self {
        Self { edit: Prompt::with_input(Vec::new(), text.to_string()), ..Self::new() }
    }

    pub fn input(&self) -> &str {
        self.edit.input()
    }

    /// Cursor position as a char index.
    pub fn cursor(&self) -> usize {
        self.edit.cursor()
    }

    /// Content rows the box takes at `width` columns, 1 to [`MAX_ROWS`].
    pub fn height(&self, width: u16) -> u16 {
        height(wrap(self.input(), width as usize).rows.len())
    }

    /// The window to draw at `width` x `height`; records the scroll and width
    /// for the next frame and the row-aware keys.
    pub fn view(&self, width: u16, height: u16) -> Viewport {
        let v = viewport(self.input(), self.cursor(), width as usize, height as usize, self.top.get());
        self.top.set(v.top);
        self.width.set((width as usize).max(1));
        v
    }

    pub fn insert_char(&mut self, c: char) {
        self.edit.insert_char(c);
    }

    pub fn newline(&mut self) {
        self.edit.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        self.edit.backspace();
    }

    pub fn delete(&mut self) {
        self.edit.delete();
    }

    pub fn left(&mut self) {
        self.edit.left();
    }

    pub fn right(&mut self) {
        self.edit.right();
    }

    pub fn clear(&mut self) {
        self.edit.clear();
        self.top.set(0);
    }

    pub fn delete_word(&mut self) {
        self.edit.delete_word();
    }

    pub fn up(&mut self) {
        let at = vertical(self.input(), self.cursor(), self.width.get(), true);
        self.edit.set_cursor(at);
    }

    pub fn down(&mut self) {
        let at = vertical(self.input(), self.cursor(), self.width.get(), false);
        self.edit.set_cursor(at);
    }

    /// Start of the cursor's visual row.
    pub fn home(&mut self) {
        let at = row_edge(self.input(), self.cursor(), self.width.get(), true);
        self.edit.set_cursor(at);
    }

    /// End of the cursor's visual row.
    pub fn end(&mut self) {
        let at = row_edge(self.input(), self.cursor(), self.width.get(), false);
        self.edit.set_cursor(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> TextArea {
        let mut t = TextArea::new();
        for c in s.chars() {
            if c == '\n' { t.newline() } else { t.insert_char(c) }
        }
        t
    }

    fn rows(text: &str, width: usize) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        wrap(text, width).rows.into_iter().map(|r| chars[r].iter().collect()).collect()
    }

    #[test]
    fn short_text_is_one_row() {
        assert_eq!(rows("", 10), [""]);
        assert_eq!(rows("hello", 10), ["hello"]);
        let w = wrap("hi", 10);
        assert_eq!(w.pos, [(0, 0), (0, 1), (0, 2)]);
    }

    #[test]
    fn long_input_wraps_by_char() {
        assert_eq!(rows("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        let w = wrap("abcdefghij", 4);
        // A row boundary: after `d` is the start of the next row.
        assert_eq!(w.pos[3], (0, 3));
        assert_eq!(w.pos[4], (1, 0));
        assert_eq!(w.pos[10], (2, 2), "end of text");
    }

    #[test]
    fn exact_width_row_puts_the_end_caret_on_the_next_row() {
        assert_eq!(rows("abcd", 4), ["abcd", ""]);
        assert_eq!(wrap("abcd", 4).pos[4], (1, 0));
        // Before a newline, the same.
        assert_eq!(rows("abcd\nx", 4), ["abcd", "", "x"]);
        let w = wrap("abcd\nx", 4);
        assert_eq!(w.pos[4], (1, 0), "the newline's slot");
        assert_eq!(w.pos[5], (2, 0));
    }

    #[test]
    fn wide_glyphs_take_two_columns_and_never_split() {
        // 漢 is 2 columns: three fit in 6, the fourth wraps.
        assert_eq!(rows("漢漢漢漢", 6), ["漢漢漢", "漢"]);
        // In 5 columns two fit; the third would straddle, so it wraps.
        assert_eq!(rows("漢漢漢", 5), ["漢漢", "漢"]);
        let w = wrap("a漢b", 3);
        assert_eq!(w.pos, [(0, 0), (0, 1), (1, 0), (1, 1)]);
        // Wider than the whole row: alone on its row, never looping.
        assert_eq!(rows("漢x", 1), ["漢", "x", ""]);
    }

    #[test]
    fn newlines_break_rows() {
        assert_eq!(rows("a\nb", 10), ["a", "b"]);
        assert_eq!(rows("a\n", 10), ["a", ""]);
        assert_eq!(rows("\n\n", 10), ["", "", ""]);
        let w = wrap("a\nb", 10);
        assert_eq!(w.pos, [(0, 0), (0, 1), (1, 0), (1, 1)]);
    }

    #[test]
    fn height_grows_to_the_cap() {
        assert_eq!(height(0), 1);
        assert_eq!(height(1), 1);
        assert_eq!(height(4), 4);
        assert_eq!(height(6), 6);
        assert_eq!(height(40), MAX_ROWS);
        assert_eq!(typed(&"x".repeat(100)).height(10), MAX_ROWS);
        assert_eq!(typed("a\nb\nc").height(10), 3);
    }

    #[test]
    fn viewport_follows_the_cursor() {
        // Stays while the cursor is inside.
        assert_eq!(viewport_top(2, 4, 10, 3), 2);
        // Moves down just enough.
        assert_eq!(viewport_top(0, 7, 10, 3), 5);
        // And up.
        assert_eq!(viewport_top(5, 1, 10, 3), 1);
        // Never past the end (text got shorter).
        assert_eq!(viewport_top(8, 2, 4, 3), 1);
    }

    #[test]
    fn caret_at_every_edge_is_visible() {
        let text = "abcdefghijklmnopqrstuvwxyz"; // 26 chars at 4 cols: 7 rows
        for cursor in 0..=26 {
            let v = viewport(text, cursor, 4, 3, 0);
            assert!(v.caret.0 < 4 && v.caret.1 < 3, "cursor {cursor}: {v:?}");
            assert!(v.lines.len() <= 3);
        }
        // Start.
        assert_eq!(viewport(text, 0, 4, 3, 0), Viewport { top: 0, lines: vec!["abcd".into(), "efgh".into(), "ijkl".into()], caret: (0, 0) });
        // End: the last row holds `yz`.
        let v = viewport(text, 26, 4, 3, 0);
        assert_eq!((v.top, v.caret), (4, (2, 2)));
        assert_eq!(v.lines, ["qrst", "uvwx", "yz"]);
        // Exact width: 8 chars at 4 cols, end caret on the empty third row.
        let v = viewport("abcdefgh", 8, 4, 6, 0);
        assert_eq!((v.lines.len(), v.caret), (3, (0, 2)));
    }

    #[test]
    fn resize_rewraps_and_keeps_the_cursor_visible() {
        let text = "x".repeat(60);
        let wide = viewport(&text, 60, 30, 6, 0);
        assert_eq!((wide.top, wide.caret), (0, (0, 2)));
        // Narrower: 60 chars at 8 cols is 8 rows; the window scrolls to the cursor.
        let narrow = viewport(&text, 60, 8, 6, wide.top);
        assert_eq!((narrow.top, narrow.caret), (2, (4, 5)));
        // Wider again: everything fits, the stale top is clamped back.
        let back = viewport(&text, 60, 80, 6, narrow.top);
        assert_eq!((back.top, back.caret), (0, (60, 0)));
    }

    #[test]
    fn up_and_down_move_by_visual_row() {
        let text = "abcdefghij"; // abcd / efgh / ij
        assert_eq!(vertical(text, 6, 4, true), 2);
        assert_eq!(vertical(text, 2, 4, false), 6);
        // Into a shorter row: its end.
        assert_eq!(vertical(text, 7, 4, false), 10);
        // Past the edges.
        assert_eq!(vertical(text, 2, 4, true), 0);
        assert_eq!(vertical(text, 9, 4, false), 10);
        // Across a newline.
        assert_eq!(vertical("abc\nd", 2, 10, false), 5);
        assert_eq!(vertical("abc\nd", 5, 10, true), 1);
    }

    #[test]
    fn home_and_end_stay_on_the_row() {
        let text = "abcdefghij";
        assert_eq!(row_edge(text, 6, 4, true), 4);
        // A soft-wrapped row ends before its last char: the next position is the next row's.
        assert_eq!(row_edge(text, 5, 4, false), 7);
        assert_eq!(row_edge(text, 8, 4, false), 10);
        assert_eq!(row_edge("ab\ncd", 1, 10, false), 2, "before the newline");
    }

    #[test]
    fn textarea_edits_and_scrolls() {
        let mut t = typed("hello\nworld");
        assert_eq!(t.input(), "hello\nworld");
        t.view(20, 6);
        t.up();
        assert_eq!(t.cursor(), 5);
        t.home();
        assert_eq!(t.cursor(), 0);
        t.down();
        t.end();
        assert_eq!(t.cursor(), 11);
        t.backspace();
        assert_eq!(t.input(), "hello\nworl");
        t.clear();
        assert_eq!((t.input(), t.cursor()), ("", 0));
        // A long line scrolls, then follows the cursor back to the top.
        let mut t = typed(&"y".repeat(50));
        assert_eq!(t.view(5, 6).top, 5);
        for _ in 0..10 {
            t.up();
            t.view(5, 6);
        }
        assert_eq!((t.cursor(), t.view(5, 6).top), (0, 0));
    }

    #[test]
    fn hscroll_keeps_the_caret_in_view() {
        // Fits: unscrolled.
        assert_eq!(hscroll("hello", 5, 10), (0, "hello".into(), 5));
        // Caret at the very end of an exact fit needs one more column.
        assert_eq!(hscroll("hello", 5, 5), (1, "ello".into(), 4));
        assert_eq!(hscroll("hello", 0, 5), (0, "hello".into(), 0));
        // Long line, caret mid-way.
        assert_eq!(hscroll("abcdefghijklmnop", 12, 5), (8, "ijklm".into(), 4));
        // Wide glyphs: columns, not chars; a straddler is dropped whole.
        assert_eq!(hscroll("漢漢漢", 3, 4), (2, "漢".into(), 2));
        assert_eq!(hscroll("a漢漢", 3, 4), (2, "漢".into(), 2));
        assert_eq!(hscroll("漢漢漢", 1, 4), (0, "漢漢".into(), 2));
        for cursor in 0..=16 {
            let (_, _, caret) = hscroll("abcdefghijklmnop", cursor, 5);
            assert!(caret < 5, "cursor {cursor}");
        }
    }
}
