//! Selection over a sealed block's `Vec<StyledLine>` snapshot.
//!
//! Independent of egui — pure logic. The live-grid [`crate::selection`]
//! module covers selection over alacritty's `Grid<Cell>`; this module
//! covers the parallel case for a frozen [`Block::Sealed`](crate::block::Block)
//! snapshot, which has no grid display offset and no scrollback.
//!
//! Cross-block selection (per
//! [spec/04 §"Cross-block selection"](../spec/04-prompt-editor.md#cross-block-selection))
//! lands in a follow-up; for now a `BlockSelection` is confined to a
//! single block identified by [`BlockId`].

use crate::block::BlockId;
use crate::prompt_editor::is_word_char;
use crate::terminal::{StyledCell, StyledLine};

/// `(row, col)` within a sealed block's snapshot. `row` indexes the
/// `Vec<StyledLine>`; `col` indexes the row's `cells` vec. Both are
/// permitted to point one past the end (i.e. equal to `lines.len()`
/// or `cells.len()`) to represent a cursor *after* the last
/// character, in the same way text editors place a caret past the
/// final glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockCursor {
    pub row: usize,
    pub col: usize,
}

impl BlockCursor {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// A selection inside one sealed block.
///
/// `anchor` is the fixed end (where the press / double-click /
/// triple-click landed); `head` is the moving end (where the
/// pointer is now). The two compare on `(row, col)` lexicographic
/// order so [`Self::ordered`] returns them in reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSelection {
    pub block_id: BlockId,
    pub anchor: BlockCursor,
    pub head: BlockCursor,
}

impl BlockSelection {
    pub fn new(block_id: BlockId, anchor: BlockCursor, head: BlockCursor) -> Self {
        Self { block_id, anchor, head }
    }

    /// Endpoints in reading order: `(start, end)` with `start <= end`.
    /// Both endpoints are returned unchanged (no canonicalisation
    /// beyond the swap) so callers that want to track which end was
    /// the original anchor can still recover it from `self.anchor`.
    pub fn ordered(&self) -> (BlockCursor, BlockCursor) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }

    /// True when the selection covers zero cells (anchor == head).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// Inclusive-start, exclusive-end column range of the word at `col`
/// in `line.cells`. If `col` is past the end or lands on a
/// non-word cell, returns the degenerate range `(col, col)`. Same
/// word predicate as the editor's
/// [`crate::prompt_editor::word_range_at`].
pub fn cell_word_range(line: &[StyledCell], col: usize) -> (usize, usize) {
    if col >= line.len() {
        return (col, col);
    }
    if !is_word_char(line[col].c) {
        return (col, col);
    }
    let mut start = col;
    while start > 0 && is_word_char(line[start - 1].c) {
        start -= 1;
    }
    let mut end = col;
    while end < line.len() && is_word_char(line[end].c) {
        end += 1;
    }
    (start, end)
}

/// Full-width range of the row, used by triple-click and
/// triple-click-drag. Trailing whitespace is included; the renderer
/// pads each row to the original cell width with space cells and we
/// preserve that on overlay, then [`block_selection_text`] trims
/// trailing spaces per row for the copy-to-clipboard payload.
pub fn cell_line_range(line: &[StyledCell]) -> (usize, usize) {
    (0, line.len())
}

/// Extract text covered by `sel` from `snapshot`. Multi-row
/// selections concatenate rows with `\n`. Trailing whitespace on
/// each row is trimmed so that copy-to-clipboard payloads don't
/// carry the grid's space-padding all the way to the right margin.
///
/// `sel.block_id` is **not** validated against the snapshot — the
/// caller is expected to have looked the matching `Block::Sealed`
/// up by id already. Returns an empty string for empty selections
/// and for ranges that fall entirely outside the snapshot.
pub fn block_selection_text(snapshot: &[StyledLine], sel: &BlockSelection) -> String {
    if sel.is_empty() {
        return String::new();
    }
    let (start, end) = sel.ordered();
    let mut out = String::new();
    let last_row = end.row.min(snapshot.len().saturating_sub(1));
    if start.row > last_row {
        return out;
    }

    for row in start.row..=last_row {
        let line = match snapshot.get(row) {
            Some(l) => l,
            None => continue,
        };
        let (col_lo, col_hi) = if start.row == end.row {
            (start.col, end.col)
        } else if row == start.row {
            (start.col, line.cells.len())
        } else if row == end.row {
            (0, end.col)
        } else {
            (0, line.cells.len())
        };
        let col_lo = col_lo.min(line.cells.len());
        let col_hi = col_hi.min(line.cells.len()).max(col_lo);
        let slice: String =
            line.cells[col_lo..col_hi].iter().map(|c| c.c).collect::<String>().trim_end().into();
        if row > start.row {
            out.push('\n');
        }
        out.push_str(&slice);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockId;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::Color;

    fn cell(c: char) -> StyledCell {
        StyledCell {
            c,
            fg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
            bg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            flags: Flags::empty(),
        }
    }

    fn line(s: &str) -> StyledLine {
        StyledLine { cells: s.chars().map(cell).collect() }
    }

    fn snap(rows: &[&str]) -> Vec<StyledLine> {
        rows.iter().map(|r| line(r)).collect()
    }

    fn sel(anchor: (usize, usize), head: (usize, usize)) -> BlockSelection {
        BlockSelection::new(
            BlockId(7),
            BlockCursor::new(anchor.0, anchor.1),
            BlockCursor::new(head.0, head.1),
        )
    }

    // ---- BlockCursor ordering ---------------------------------------

    #[test]
    fn block_cursor_orders_by_row_then_col() {
        assert!(BlockCursor::new(0, 5) < BlockCursor::new(1, 0));
        assert!(BlockCursor::new(2, 3) < BlockCursor::new(2, 4));
        assert_eq!(BlockCursor::new(0, 0), BlockCursor::new(0, 0));
    }

    // ---- BlockSelection::ordered ------------------------------------

    #[test]
    fn ordered_returns_endpoints_in_reading_order() {
        let s = sel((2, 5), (1, 3));
        let (start, end) = s.ordered();
        assert_eq!(start, BlockCursor::new(1, 3));
        assert_eq!(end, BlockCursor::new(2, 5));
    }

    #[test]
    fn ordered_passes_through_when_already_ordered() {
        let s = sel((0, 1), (3, 7));
        let (start, end) = s.ordered();
        assert_eq!(start, BlockCursor::new(0, 1));
        assert_eq!(end, BlockCursor::new(3, 7));
    }

    #[test]
    fn empty_when_anchor_equals_head() {
        let s = sel((1, 4), (1, 4));
        assert!(s.is_empty());
    }

    // ---- cell_word_range --------------------------------------------

    #[test]
    fn cell_word_range_at_word_returns_word_bounds() {
        let l = line("foo bar baz").cells;
        assert_eq!(cell_word_range(&l, 0), (0, 3));
        assert_eq!(cell_word_range(&l, 1), (0, 3));
        assert_eq!(cell_word_range(&l, 4), (4, 7));
    }

    #[test]
    fn cell_word_range_at_whitespace_is_degenerate() {
        let l = line("foo bar").cells;
        assert_eq!(cell_word_range(&l, 3), (3, 3));
    }

    #[test]
    fn cell_word_range_at_underscore_includes_underscore() {
        let l = line("my_var rest").cells;
        assert_eq!(cell_word_range(&l, 0), (0, 6));
        assert_eq!(cell_word_range(&l, 3), (0, 6));
    }

    #[test]
    fn cell_word_range_past_end_is_degenerate() {
        let l = line("hi").cells;
        assert_eq!(cell_word_range(&l, 10), (10, 10));
    }

    // ---- cell_line_range --------------------------------------------

    #[test]
    fn cell_line_range_is_full_row() {
        let l = line("foo bar").cells;
        assert_eq!(cell_line_range(&l), (0, 7));
    }

    #[test]
    fn cell_line_range_empty_row() {
        let l = line("").cells;
        assert_eq!(cell_line_range(&l), (0, 0));
    }

    // ---- block_selection_text ---------------------------------------

    #[test]
    fn selection_text_single_row_partial() {
        let s = snap(&["hello world"]);
        let text = block_selection_text(&s, &sel((0, 6), (0, 11)));
        assert_eq!(text, "world");
    }

    #[test]
    fn selection_text_single_row_handles_reversed_endpoints() {
        let s = snap(&["hello world"]);
        let text = block_selection_text(&s, &sel((0, 11), (0, 6)));
        assert_eq!(text, "world");
    }

    #[test]
    fn selection_text_multi_row_joins_with_newlines() {
        let s = snap(&["line one", "line two", "line three"]);
        let text = block_selection_text(&s, &sel((0, 5), (2, 4)));
        assert_eq!(text, "one\nline two\nline");
    }

    #[test]
    fn selection_text_full_rows_trims_trailing_whitespace() {
        // Grid rows are padded to width with spaces; copy should
        // omit that padding.
        let s = snap(&["hi      ", "there   "]);
        let text = block_selection_text(&s, &sel((0, 0), (1, 8)));
        assert_eq!(text, "hi\nthere");
    }

    #[test]
    fn selection_text_empty_when_anchor_equals_head() {
        let s = snap(&["abc"]);
        assert_eq!(block_selection_text(&s, &sel((0, 1), (0, 1))), "");
    }

    #[test]
    fn selection_text_clamps_cols_past_row_end() {
        let s = snap(&["abc"]);
        let text = block_selection_text(&s, &sel((0, 0), (0, 99)));
        assert_eq!(text, "abc");
    }

    #[test]
    fn selection_text_returns_empty_when_start_row_past_snapshot() {
        let s = snap(&["abc"]);
        assert_eq!(block_selection_text(&s, &sel((5, 0), (6, 2))), "");
    }
}
