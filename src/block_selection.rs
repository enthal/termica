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

/// Index past the last non-`' '` cell in `line`. The terminal grid
/// pads every row to the column width with space cells; this
/// function returns the position the user would think of as
/// "end of typed content" so selections, line-select, and clamping
/// don't extend across the imaginary trailing space cells. Returns
/// `0` for an all-space (or empty) row.
pub fn effective_row_len(line: &[StyledCell]) -> usize {
    line.iter().rposition(|c| c.c != ' ').map(|i| i + 1).unwrap_or(0)
}

/// Inclusive-start, exclusive-end column range of a row's typed
/// content, used by triple-click and triple-click-drag. Stops at
/// the last non-space cell so the selection doesn't span the
/// grid's trailing space padding (the same padding that
/// [`block_selection_text`] already trims for the clipboard).
pub fn cell_line_range(line: &[StyledCell]) -> (usize, usize) {
    (0, effective_row_len(line))
}

/// Extract text covered by `sel` from a sealed block's unified
/// row space: rows `0..command_lines` come from `command` (split
/// on `\n`); rows `command_lines..` come from `snapshot`. Multi-
/// row selections concatenate with `\n`; trailing whitespace on
/// each row is trimmed so copy-to-clipboard payloads don't carry
/// the grid's space-padding to the right margin.
///
/// `sel.block_id` is **not** validated against the inputs — the
/// caller is expected to have looked the matching `Block::Sealed`
/// up by id already. Returns an empty string for empty selections
/// and for ranges that fall entirely outside the block.
pub fn block_selection_text(
    command: &str,
    snapshot: &[StyledLine],
    sel: &BlockSelection,
) -> String {
    if sel.is_empty() {
        return String::new();
    }
    let (start, end) = sel.ordered();
    let cmd_lines: Vec<&str> =
        if command.is_empty() { Vec::new() } else { command.split('\n').collect() };
    let total_rows = cmd_lines.len() + snapshot.len();
    if total_rows == 0 {
        return String::new();
    }
    let last_row = end.row.min(total_rows - 1);
    if start.row > last_row {
        return String::new();
    }

    let mut out = String::new();
    for row in start.row..=last_row {
        let row_len_chars = if row < cmd_lines.len() {
            cmd_lines[row].chars().count()
        } else {
            snapshot.get(row - cmd_lines.len()).map(|l| l.cells.len()).unwrap_or(0)
        };
        let (col_lo, col_hi) = if start.row == end.row {
            (start.col, end.col)
        } else if row == start.row {
            (start.col, row_len_chars)
        } else if row == end.row {
            (0, end.col)
        } else {
            (0, row_len_chars)
        };
        let col_lo = col_lo.min(row_len_chars);
        let col_hi = col_hi.min(row_len_chars).max(col_lo);

        let slice: String = if row < cmd_lines.len() {
            // Command lines are stored as `&str` with `char`-level
            // semantics; col_lo / col_hi count chars. Walk
            // char_indices to find the byte range.
            let line = cmd_lines[row];
            let chars: Vec<(usize, char)> = line.char_indices().collect();
            let start_b = chars.get(col_lo).map(|(b, _)| *b).unwrap_or(line.len());
            let end_b = chars.get(col_hi).map(|(b, _)| *b).unwrap_or(line.len());
            line[start_b..end_b].trim_end().to_string()
        } else {
            let line = &snapshot[row - cmd_lines.len()];
            line.cells[col_lo..col_hi]
                .iter()
                .map(|c| c.c)
                .collect::<String>()
                .trim_end()
                .to_string()
        };

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

    #[test]
    fn cell_line_range_stops_at_last_non_space_cell() {
        // Snapshot rows are padded to grid width with `' '` cells.
        // Triple-click line-select must stop at the last typed char
        // so the user does not "select imaginary characters" that
        // exist only as grid padding.
        let l = line("hi      ").cells;
        assert_eq!(cell_line_range(&l), (0, 2));
    }

    #[test]
    fn cell_line_range_blank_row_is_degenerate() {
        // An all-space row has no typed content; the range collapses
        // to (0, 0) rather than (0, width).
        let l = line("        ").cells;
        assert_eq!(cell_line_range(&l), (0, 0));
    }

    // ---- effective_row_len ------------------------------------------

    #[test]
    fn effective_row_len_strips_trailing_spaces() {
        let l = line("abc   ").cells;
        assert_eq!(effective_row_len(&l), 3);
    }

    #[test]
    fn effective_row_len_keeps_inner_spaces() {
        let l = line("a   b   ").cells;
        assert_eq!(effective_row_len(&l), 5);
    }

    #[test]
    fn effective_row_len_zero_for_all_space_row() {
        let l = line("     ").cells;
        assert_eq!(effective_row_len(&l), 0);
    }

    #[test]
    fn effective_row_len_full_when_no_trailing_space() {
        let l = line("abc").cells;
        assert_eq!(effective_row_len(&l), 3);
    }

    // ---- block_selection_text ---------------------------------------

    #[test]
    fn selection_text_single_row_partial() {
        let s = snap(&["hello world"]);
        let text = block_selection_text("", &s, &sel((0, 6), (0, 11)));
        assert_eq!(text, "world");
    }

    #[test]
    fn selection_text_single_row_handles_reversed_endpoints() {
        let s = snap(&["hello world"]);
        let text = block_selection_text("", &s, &sel((0, 11), (0, 6)));
        assert_eq!(text, "world");
    }

    #[test]
    fn selection_text_multi_row_joins_with_newlines() {
        let s = snap(&["line one", "line two", "line three"]);
        let text = block_selection_text("", &s, &sel((0, 5), (2, 4)));
        assert_eq!(text, "one\nline two\nline");
    }

    #[test]
    fn selection_text_full_rows_trims_trailing_whitespace() {
        // Grid rows are padded to width with spaces; copy should
        // omit that padding.
        let s = snap(&["hi      ", "there   "]);
        let text = block_selection_text("", &s, &sel((0, 0), (1, 8)));
        assert_eq!(text, "hi\nthere");
    }

    #[test]
    fn selection_text_empty_when_anchor_equals_head() {
        let s = snap(&["abc"]);
        assert_eq!(block_selection_text("", &s, &sel((0, 1), (0, 1))), "");
    }

    #[test]
    fn selection_text_clamps_cols_past_row_end() {
        let s = snap(&["abc"]);
        let text = block_selection_text("", &s, &sel((0, 0), (0, 99)));
        assert_eq!(text, "abc");
    }

    #[test]
    fn selection_text_returns_empty_when_start_row_past_snapshot() {
        let s = snap(&["abc"]);
        assert_eq!(block_selection_text("", &s, &sel((5, 0), (6, 2))), "");
    }

    // ---- command-label region (Phase 4F polish) ---------------------

    #[test]
    fn selection_text_inside_single_line_command() {
        // Block: command = "echo hello", snapshot = ["hello"]. Row 0
        // is the command line; row 1 is the output.
        let s = snap(&["hello"]);
        let text = block_selection_text("echo hello", &s, &sel((0, 5), (0, 10)));
        assert_eq!(text, "hello");
    }

    #[test]
    fn selection_text_inside_multi_line_command() {
        // Multi-line command (Shift+Enter in the editor):
        // row 0 = "echo a", row 1 = "echo b", row 2 = "out".
        let s = snap(&["out"]);
        let text = block_selection_text("echo a\necho b", &s, &sel((0, 5), (1, 6)));
        assert_eq!(text, "a\necho b");
    }

    #[test]
    fn selection_text_spans_command_and_snapshot() {
        // Selection starts in the command, ends in the output.
        let s = snap(&["hello"]);
        let text = block_selection_text("echo hello", &s, &sel((0, 5), (1, 5)));
        assert_eq!(text, "hello\nhello");
    }

    #[test]
    fn selection_text_empty_command_falls_back_to_snapshot_indexing() {
        // When the command is empty, row 0 is the first snapshot row.
        let s = snap(&["abc", "def"]);
        let text = block_selection_text("", &s, &sel((0, 0), (1, 3)));
        assert_eq!(text, "abc\ndef");
    }
}
