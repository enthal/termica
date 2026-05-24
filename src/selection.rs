//! Mouse selection over the terminal grid.
//!
//! Phase 1E-k: click + drag to select a rectangular-ish range of
//! cells, then copy with Cmd+C (macOS) / Ctrl+Shift+C (Linux/Windows)
//! into the system clipboard. The grid is the source of truth — we
//! never store the selected text, only the two grid points; the
//! current text is materialised on demand from the live grid.
//!
//! This module is intentionally OS-free and egui-free: it deals in
//! alacritty's `Point` (absolute grid lines, signed `Line` for
//! scrollback) and pure pixel arithmetic. The wiring into [`crate::pane`]
//! and the eframe app lives at the call site; everything here is a
//! pure function (or close to it) so we can unit-test the math
//! without rendering anything.
//!
//! ## Coordinate model
//!
//! `Point<Line>` from `alacritty_terminal::index` uses a signed `Line`
//! so scrollback rows naturally fall below 0. The current viewport at
//! a given `display_offset` covers rows
//! `(-display_offset)..(screen_lines - display_offset)`. We store
//! selection endpoints in these absolute coordinates rather than in
//! viewport pixels so that scrolling the display while a selection
//! exists keeps the highlight glued to the right cells.
//!
//! ## What v1 does not do
//!
//! - Smart word / line / rectangular selection modes — Phase 11 polish.
//! - Auto-extend during edge-of-pane drag — same.
//! - Reflow under resize: if the user resizes the window while a
//!   selection is live, the selection may end up pointing at cells
//!   that wrapped differently. We accept that.

#![forbid(unsafe_code)]

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};

/// Granularity at which a selection extends.
///
/// - `Char`: pixel-perfect cell granularity. The default — set by a
///   single click + drag.
/// - `Word`: anchor and head each snap outward to the bounds of the
///   word they sit on. Set by a double-click; subsequent drag motion
///   extends the selection word-by-word.
/// - `Line`: anchor and head each snap to whole-row bounds. Set by a
///   triple-click; subsequent drag motion extends the selection
///   line-by-line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionMode {
    #[default]
    Char,
    Word,
    Line,
}

/// Two grid points + a granularity describing the user's current
/// selection.
///
/// `anchor` is the point the drag started at; `head` follows the
/// pointer. Either may be greater than the other in reading order —
/// `effective_range` returns `(start, end)` normalised, and for the
/// `Word` / `Line` modes also expands the endpoints outward to the
/// surrounding word / line bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
    pub mode: SelectionMode,
}

impl Selection {
    /// Construct a degenerate (single-cell) `Char`-mode selection at
    /// `p`. Set on a single click before the user has dragged.
    pub fn new(p: Point) -> Self {
        Self::with_mode(p, SelectionMode::Char)
    }

    /// Construct a degenerate selection at `p` with the given mode.
    /// Used by the multi-click handler in [`crate::TermicaApp`] to
    /// start a `Word` / `Line` selection at the double/triple-click
    /// point.
    pub fn with_mode(p: Point, mode: SelectionMode) -> Self {
        Self { anchor: p, head: p, mode }
    }

    /// Move the head while keeping the anchor + mode pinned. Called
    /// on every drag-move event.
    pub fn extend_to(&mut self, head: Point) {
        self.head = head;
    }

    /// Normalise the *raw* anchor / head to `(start, end)` in reading
    /// order. For `Char` selections this is also the effective range;
    /// for `Word` / `Line` selections, use [`Self::effective_range`]
    /// instead — it expands the endpoints outward to whole-word /
    /// whole-line bounds.
    pub fn range(&self) -> (Point, Point) {
        if (self.anchor.line, self.anchor.column) <= (self.head.line, self.head.column) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// True when anchor == head AND we're in `Char` mode — i.e. a
    /// single-click that hasn't yet become a drag. We don't paint or
    /// copy anything in this state. Word/Line selections are NEVER
    /// considered empty (a double-click on a word produces a real
    /// highlight even with zero drag motion).
    pub fn is_empty(&self) -> bool {
        self.mode == SelectionMode::Char && self.anchor == self.head
    }

    /// Compute the effective `(start, end)` range to highlight / copy.
    ///
    /// For `Char` selections this is the same as [`Self::range`].
    /// For `Word` / `Line` selections, each endpoint expands outward
    /// to its surrounding word / line bounds, then the two are
    /// unioned in reading order.
    pub fn effective_range(&self, grid: &Grid<Cell>) -> (Point, Point) {
        let (a, b) = self.range();
        match self.mode {
            SelectionMode::Char => (a, b),
            SelectionMode::Word => {
                let (a_start, _) = word_bounds_at(grid, a);
                let (_, b_end) = word_bounds_at(grid, b);
                (a_start, b_end)
            }
            SelectionMode::Line => {
                let (a_start, _) = line_bounds_at(grid, a);
                let (_, b_end) = line_bounds_at(grid, b);
                (a_start, b_end)
            }
        }
    }
}

/// Bounds of the word containing `p`, as `(start, end)` inclusive.
///
/// A "word" is a maximal run of non-whitespace cells on the same
/// line. If `p` lies on a whitespace cell, returns `(p, p)` — there
/// is no word to expand to, just the single cell. The first/last
/// returned column is clamped to the grid's column range.
///
/// We deliberately keep the definition simple (whitespace ↔ non-
/// whitespace, no Unicode word breaking, no `.`/`/`/`-` smart-split).
/// Terminal selection is for grabbing arbitrary stretches of text;
/// the smart variants (URL grabbing etc.) live in the link engine
/// that follows in the next PR.
pub fn word_bounds_at(grid: &Grid<Cell>, p: Point) -> (Point, Point) {
    let cols = grid.columns();
    if cols == 0 {
        return (p, p);
    }
    let line = p.line;
    let col = p.column.0.min(cols.saturating_sub(1));

    let here = grid[Point::new(line, Column(col))].c;
    if here.is_whitespace() {
        let pt = Point::new(line, Column(col));
        return (pt, pt);
    }

    // Walk left while non-whitespace; same right.
    let mut start = col;
    while start > 0 {
        let c = grid[Point::new(line, Column(start - 1))].c;
        if c.is_whitespace() {
            break;
        }
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols {
        let c = grid[Point::new(line, Column(end + 1))].c;
        if c.is_whitespace() {
            break;
        }
        end += 1;
    }
    (Point::new(line, Column(start)), Point::new(line, Column(end)))
}

/// Bounds of the entire line containing `p`, as `(start, end)`
/// inclusive. Always `(col 0, col cols-1)` of the same line.
pub fn line_bounds_at(grid: &Grid<Cell>, p: Point) -> (Point, Point) {
    let cols = grid.columns();
    let last = cols.saturating_sub(1);
    (Point::new(p.line, Column(0)), Point::new(p.line, Column(last)))
}

/// Geometry of the painted grid, in screen pixels + cell counts.
///
/// Bundled together so [`pixel_to_grid_point`] and the overlay painter
/// in [`crate::render`] take a single argument instead of seven or
/// eight loose parameters. Constructed by the renderer once per frame
/// and passed to whichever caller needs to map between pixels and
/// grid cells.
#[derive(Debug, Clone, Copy)]
pub struct GridGeometry {
    /// Top-left of the painted grid, in screen pixels.
    pub origin_x: f32,
    pub origin_y: f32,
    /// Width of one cell, in screen pixels.
    pub cell_w: f32,
    /// Height of one row, in screen pixels.
    pub row_h: f32,
    /// Current scrollback offset, in rows from the live bottom.
    pub display_offset: i32,
    /// Number of rows in the viewport.
    pub screen_lines: usize,
    /// Number of columns in the viewport.
    pub cols: usize,
}

/// Map a pixel pointer position to an absolute grid `Point`.
///
/// The returned point is clamped to a valid cell: viewport row into
/// `0..screen_lines`, column into `0..cols`. Negative input pixels
/// (above / left of the grid) clamp to the top-left; out-of-range
/// input clamps to the bottom-right. Pure function — no egui, no OS.
pub fn pixel_to_grid_point(pixel_x: f32, pixel_y: f32, geom: GridGeometry) -> Point {
    // Guard against pathological font metrics — division by zero would
    // hand back NaN that then clamps to wherever as_i32 wants.
    let cell_w = if geom.cell_w > 0.0 { geom.cell_w } else { 1.0 };
    let row_h = if geom.row_h > 0.0 { geom.row_h } else { 1.0 };

    let rel_x = (pixel_x - geom.origin_x).max(0.0);
    let rel_y = (pixel_y - geom.origin_y).max(0.0);

    let max_col = geom.cols.saturating_sub(1);
    let max_row = geom.screen_lines.saturating_sub(1);

    let col = (rel_x / cell_w).floor() as i64;
    let viewport_row = (rel_y / row_h).floor() as i64;

    let col = col.clamp(0, max_col as i64) as usize;
    let viewport_row = viewport_row.clamp(0, max_row as i64) as i32;

    // viewport_row in 0..screen_lines maps to absolute Line via
    //     line = viewport_row - display_offset
    // (the same translation alacritty's `viewport_to_point` uses).
    let line = viewport_row - geom.display_offset;
    Point::new(Line(line), Column(col))
}

/// Extract the text currently under `selection` from `grid`.
///
/// Multi-line selections join rows with `\n`. Trailing whitespace on
/// each row is preserved up to (and including) the selected columns —
/// we don't try to be clever about "logical end of line" yet. Wide
/// (double-width) glyphs have their spacer cell skipped so we don't
/// duplicate the leading char.
///
/// Empty selections (`anchor == head` on the same cell) still return
/// the single cell's character, which is useful for "copy whatever
/// the user pointed at" but harmless if the caller chooses to gate on
/// [`Selection::is_empty`] first.
pub fn selection_text(grid: &Grid<Cell>, selection: &Selection) -> String {
    let (start, end) = selection.effective_range(grid);
    let cols = grid.columns();
    let screen_lines = grid.screen_lines() as i32;
    let display_offset = grid.display_offset() as i32;

    // Bound the line range we'll touch to lines that the grid will
    // actually return when indexed by `Point`: the live viewport
    // plus whatever sits in scrollback up to history_size.
    let history = grid.history_size() as i32;
    let min_line = -(history + display_offset);
    let max_line = screen_lines - 1 - display_offset;

    let mut out = String::new();

    let start_line = start.line.0.clamp(min_line, max_line);
    let end_line = end.line.0.clamp(min_line, max_line);

    for line_idx in start_line..=end_line {
        let line_start = if line_idx == start.line.0 { start.column.0 } else { 0 };
        let line_end = if line_idx == end.line.0 {
            end.column.0.min(cols.saturating_sub(1))
        } else {
            cols - 1
        };
        let line_start = line_start.min(cols.saturating_sub(1));

        for col_idx in line_start..=line_end {
            let pt = Point::new(Line(line_idx), Column(col_idx));
            let cell = &grid[pt];
            // Wide-spacer cells are the right half of a double-width
            // glyph; the actual character already came out of the
            // left half. Skipping them avoids "héllo" → "héello".
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            out.push(cell.c);
        }

        if line_idx < end_line {
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    //! Pure tests — no egui context, no PTY, no clipboard.

    use super::*;
    use alacritty_terminal::Term;
    use alacritty_terminal::event::{Event, EventListener};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    #[derive(Default)]
    struct NopListener;
    impl EventListener for NopListener {
        fn send_event(&self, _e: Event) {}
    }

    fn term_with(text: &[u8], rows: u16, cols: u16) -> Term<NopListener> {
        let size = TermSize::new(cols as usize, rows as usize);
        let mut term = Term::new(Config::default(), &size, NopListener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        for b in text {
            parser.advance(&mut term, &[*b]);
        }
        term
    }

    // --- Selection ---------------------------------------------------

    #[test]
    fn selection_new_is_empty() {
        let p = Point::new(Line(0), Column(0));
        let s = Selection::new(p);
        assert!(s.is_empty());
        let (a, b) = s.range();
        assert_eq!(a, b);
    }

    #[test]
    fn selection_range_normalises_reversed_drag() {
        let a = Point::new(Line(3), Column(5));
        let b = Point::new(Line(1), Column(2));
        let mut s = Selection::new(a);
        s.extend_to(b);
        let (start, end) = s.range();
        assert_eq!(start, b, "earlier line should win");
        assert_eq!(end, a);
    }

    #[test]
    fn selection_range_normalises_same_line_reversed_columns() {
        let a = Point::new(Line(2), Column(10));
        let b = Point::new(Line(2), Column(3));
        let mut s = Selection::new(a);
        s.extend_to(b);
        let (start, end) = s.range();
        assert_eq!(start.column.0, 3);
        assert_eq!(end.column.0, 10);
    }

    // --- pixel_to_grid_point ----------------------------------------

    fn geom_at(origin_x: f32, origin_y: f32, display_offset: i32) -> GridGeometry {
        GridGeometry {
            origin_x,
            origin_y,
            cell_w: 10.0,
            row_h: 20.0,
            display_offset,
            screen_lines: 24,
            cols: 80,
        }
    }

    #[test]
    fn pixel_to_grid_point_maps_origin_to_top_left() {
        let p = pixel_to_grid_point(0.0, 0.0, geom_at(0.0, 0.0, 0));
        assert_eq!(p, Point::new(Line(0), Column(0)));
    }

    #[test]
    fn pixel_to_grid_point_floors_within_a_cell() {
        // (15.9, 39.9) sits inside cell (1, 1) of a 10×20 cell.
        let p = pixel_to_grid_point(15.9, 39.9, geom_at(0.0, 0.0, 0));
        assert_eq!(p, Point::new(Line(1), Column(1)));
    }

    #[test]
    fn pixel_to_grid_point_subtracts_rect_origin() {
        // Same cell as the previous test, but with the rect shifted
        // 100 px right / 50 px down.
        let p = pixel_to_grid_point(115.9, 89.9, geom_at(100.0, 50.0, 0));
        assert_eq!(p, Point::new(Line(1), Column(1)));
    }

    #[test]
    fn pixel_to_grid_point_clamps_negative_input_to_origin() {
        let p = pixel_to_grid_point(-50.0, -50.0, geom_at(0.0, 0.0, 0));
        assert_eq!(p, Point::new(Line(0), Column(0)));
    }

    #[test]
    fn pixel_to_grid_point_clamps_overshoot_to_bottom_right() {
        // A pointer way past the right/bottom edge should clamp to the
        // last cell — not produce a wild row/col.
        let geom = GridGeometry { screen_lines: 5, cols: 8, ..geom_at(0.0, 0.0, 0) };
        let p = pixel_to_grid_point(10_000.0, 10_000.0, geom);
        assert_eq!(p, Point::new(Line(4), Column(7)));
    }

    #[test]
    fn pixel_to_grid_point_applies_display_offset_for_scrollback() {
        // viewport_row = 0, display_offset = 3 ⇒ absolute Line(-3).
        // That's the row 3 lines back into history that's currently
        // painted at the top of the viewport.
        let p = pixel_to_grid_point(0.0, 0.0, geom_at(0.0, 0.0, 3));
        assert_eq!(p, Point::new(Line(-3), Column(0)));
    }

    #[test]
    fn pixel_to_grid_point_survives_zero_cell_metrics() {
        // Defensive: bad font setup shouldn't NaN us into Line::MAX.
        let geom = GridGeometry { cell_w: 0.0, row_h: 0.0, ..geom_at(0.0, 0.0, 0) };
        let p = pixel_to_grid_point(50.0, 50.0, geom);
        // Just verify it's a valid bounded cell.
        assert!((0..24).contains(&(p.line.0 as usize)));
        assert!((0..80).contains(&p.column.0));
    }

    // --- selection_text --------------------------------------------

    /// Build a `Char`-mode selection between two raw points.
    fn char_sel(a: Point, b: Point) -> Selection {
        Selection { anchor: a, head: b, mode: SelectionMode::Char }
    }

    #[test]
    fn selection_text_single_row_returns_substring() {
        let term = term_with(b"hello world", 5, 20);
        let sel = char_sel(Point::new(Line(0), Column(0)), Point::new(Line(0), Column(4)));
        let t = selection_text(term.grid(), &sel);
        assert_eq!(t, "hello");
    }

    #[test]
    fn selection_text_multi_row_joins_with_newline() {
        // "one\r\ntwo\r\nthree" puts three labels on three rows.
        // Selecting the full content of rows 0..2 yields "one  …\ntwo  …\nthree".
        // The full-row width is 20 cols (spaces fill the rest).
        let term = term_with(b"one\r\ntwo\r\nthree", 5, 20);
        let sel = char_sel(Point::new(Line(0), Column(0)), Point::new(Line(2), Column(4)));
        let t = selection_text(term.grid(), &sel);
        // First row: "one" + 17 trailing spaces (to col 19 — full row).
        // Middle row: full 20 cells of "two…".
        // Last row: "three" (cols 0..=4).
        let lines: Vec<&str> = t.split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("one"));
        assert!(lines[1].starts_with("two"));
        assert_eq!(lines[2], "three");
    }

    #[test]
    fn selection_text_reverse_drag_returns_same_string() {
        let term = term_with(b"hello world", 5, 20);
        let forward = char_sel(Point::new(Line(0), Column(0)), Point::new(Line(0), Column(4)));
        let reverse = char_sel(Point::new(Line(0), Column(4)), Point::new(Line(0), Column(0)));
        assert_eq!(selection_text(term.grid(), &forward), selection_text(term.grid(), &reverse));
    }

    #[test]
    fn selection_text_clamps_overshoot_to_grid_bounds() {
        // Asking for a column far past the grid's right edge should
        // just stop at the last cell, not panic with OOB.
        let term = term_with(b"abc", 5, 20);
        let sel = char_sel(Point::new(Line(0), Column(0)), Point::new(Line(0), Column(999)));
        let t = selection_text(term.grid(), &sel);
        assert!(t.starts_with("abc"));
        // ...followed by the row's trailing spaces, up to the last col.
        assert_eq!(t.chars().count(), 20);
    }

    #[test]
    fn selection_text_empty_selection_returns_single_cell() {
        let term = term_with(b"X", 5, 20);
        let sel = Selection::new(Point::new(Line(0), Column(0)));
        let t = selection_text(term.grid(), &sel);
        assert_eq!(t, "X");
    }

    // --- word / line bounds + effective range ----------------------

    #[test]
    fn word_bounds_expands_in_both_directions() {
        // "  hello world  " — clicking on 'l' in "hello" should select
        // the whole word.
        let term = term_with(b"  hello world  ", 5, 20);
        let (start, end) = word_bounds_at(term.grid(), Point::new(Line(0), Column(4))); // 'l' in hello (col 4)
        // "hello" sits at cols 2..=6.
        assert_eq!(start.column.0, 2);
        assert_eq!(end.column.0, 6);
    }

    #[test]
    fn word_bounds_on_whitespace_is_degenerate() {
        // Clicking on a space cell yields a single-cell range — there
        // is no word to expand to.
        let term = term_with(b"  hello world  ", 5, 20);
        let p = Point::new(Line(0), Column(0)); // leading space
        let (start, end) = word_bounds_at(term.grid(), p);
        assert_eq!(start, p);
        assert_eq!(end, p);
    }

    #[test]
    fn word_bounds_clamps_to_left_edge() {
        // A word that starts at column 0 should not walk past it.
        let term = term_with(b"abc def", 5, 20);
        let (start, end) = word_bounds_at(term.grid(), Point::new(Line(0), Column(0)));
        assert_eq!(start.column.0, 0);
        assert_eq!(end.column.0, 2);
    }

    #[test]
    fn line_bounds_returns_full_row() {
        let term = term_with(b"hi", 5, 20);
        let (start, end) = line_bounds_at(term.grid(), Point::new(Line(0), Column(0)));
        assert_eq!(start, Point::new(Line(0), Column(0)));
        assert_eq!(end, Point::new(Line(0), Column(19)));
    }

    #[test]
    fn effective_range_word_mode_expands_both_endpoints() {
        // Anchor clicked inside "hello"; head clicked inside "world".
        // Effective range should cover "hello world" — the union of
        // the two snapped words.
        let term = term_with(b"  hello world  ", 5, 20);
        let sel = Selection {
            anchor: Point::new(Line(0), Column(3)), // inside "hello"
            head: Point::new(Line(0), Column(9)),   // inside "world"
            mode: SelectionMode::Word,
        };
        let (start, end) = sel.effective_range(term.grid());
        assert_eq!(start.column.0, 2); // start of "hello"
        assert_eq!(end.column.0, 12); // end of "world"
    }

    #[test]
    fn effective_range_word_mode_reverse_drag_still_unions() {
        // Anchor in "world", head in "hello" (drag right-to-left).
        // Effective range should still cover "hello world".
        let term = term_with(b"  hello world  ", 5, 20);
        let sel = Selection {
            anchor: Point::new(Line(0), Column(9)), // inside "world"
            head: Point::new(Line(0), Column(3)),   // inside "hello"
            mode: SelectionMode::Word,
        };
        let (start, end) = sel.effective_range(term.grid());
        assert_eq!(start.column.0, 2);
        assert_eq!(end.column.0, 12);
    }

    #[test]
    fn effective_range_line_mode_covers_whole_rows() {
        // Anchor on row 0 col 5, head on row 2 col 1. Effective range
        // covers rows 0..=2 entirely.
        let term = term_with(b"one\r\ntwo\r\nthree", 5, 20);
        let sel = Selection {
            anchor: Point::new(Line(0), Column(5)),
            head: Point::new(Line(2), Column(1)),
            mode: SelectionMode::Line,
        };
        let (start, end) = sel.effective_range(term.grid());
        assert_eq!(start, Point::new(Line(0), Column(0)));
        assert_eq!(end, Point::new(Line(2), Column(19)));
    }

    #[test]
    fn selection_is_empty_treats_word_mode_as_non_empty() {
        // Even with anchor == head, a Word selection is "real" — the
        // user just double-clicked and we should highlight the word.
        let p = Point::new(Line(0), Column(3));
        let s = Selection::with_mode(p, SelectionMode::Word);
        assert!(!s.is_empty());
        let line_s = Selection::with_mode(p, SelectionMode::Line);
        assert!(!line_s.is_empty());
        // But a single-click Char selection IS empty (nothing to paint).
        let char_s = Selection::with_mode(p, SelectionMode::Char);
        assert!(char_s.is_empty());
    }

    #[test]
    fn selection_text_word_mode_yields_just_the_word() {
        let term = term_with(b"  hello world  ", 5, 20);
        let sel = Selection::with_mode(Point::new(Line(0), Column(4)), SelectionMode::Word);
        let t = selection_text(term.grid(), &sel);
        assert_eq!(t, "hello");
    }
}
