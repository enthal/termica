//! Pane-spanning selection across multiple sealed blocks.
//!
//! [`crate::block_selection`] covers selection **inside** a single
//! sealed block; this module covers the cross-block case from
//! [spec/04 §"Cross-block selection"](../spec/04-prompt-editor.md#cross-block-selection).
//!
//! A [`PaneSelection`] is two [`PaneCursor`]s — each cursor carries
//! the [`BlockId`] of its block plus a (row, col) within that block's
//! unified row space (rows `0..command_lines` are the command label;
//! `command_lines..` are the snapshot). Since [`BlockId`]s are
//! allocated monotonically in creation order ([`crate::block`]'s
//! invariant), the natural `(block_id, row, col)` ordering is the
//! same as the visual top-to-bottom reading order of a pane.
//!
//! Pure logic — no egui, no PaneSession reference. The routing
//! / paint hookup in `render_pane.rs` and the text extraction in
//! `PaneSession` consume this module.
//!
//! ## What this module is NOT
//!
//! - It does not own multi-click mode (Word / Line vs. Char). Multi-
//!   click drags currently stay within the source block ([§Mouse in
//!   the editor](../spec/04-prompt-editor.md#mouse-in-the-editor) for
//!   the editor; sealed-block multi-click uses
//!   `BlockSelection`-shaped state via `PaneUiState::sealed_drag_anchor`).
//!   Cross-block drag is always Char-mode.
//! - It does not paint anything. The renderer queries
//!   [`PaneSelection::block_range_for`] per block to compute the per-
//!   block range to overlay, and reuses [`crate::render`]'s existing
//!   per-block highlight helper.

use crate::block::BlockId;
use crate::block_selection::BlockCursor;
use crate::terminal::StyledLine;

/// A cursor into a specific block's unified row space.
///
/// Rows `0..command_lines` of a block come from its command label
/// (split on `\n`); rows `command_lines..` come from its snapshot.
/// `col` indexes a row's cells (or a command line's chars). Both
/// `row` and `col` may be one past the end — same convention as
/// [`BlockCursor`].
///
/// Ordering is `(block_id, row, col)` lexicographic. Since
/// [`BlockId`]s are allocated monotonically in creation order, this
/// IS the visual reading order of a pane: a lower-id block sits
/// above a higher-id block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PaneCursor {
    pub block_id: BlockId,
    pub row: usize,
    pub col: usize,
}

impl PaneCursor {
    pub fn new(block_id: BlockId, row: usize, col: usize) -> Self {
        Self { block_id, row, col }
    }

    /// Helper: build from a [`BlockCursor`] anchored to a known block.
    pub fn in_block(block_id: BlockId, bc: BlockCursor) -> Self {
        Self { block_id, row: bc.row, col: bc.col }
    }
}

/// A selection spanning zero or more sealed blocks.
///
/// `anchor` is the press / first-click end; `head` follows the
/// pointer (or the second click for shift-extend). Either may be in
/// a higher or lower block than the other; [`Self::ordered`] returns
/// them in pane reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSelection {
    pub anchor: PaneCursor,
    pub head: PaneCursor,
}

impl PaneSelection {
    pub fn new(anchor: PaneCursor, head: PaneCursor) -> Self {
        Self { anchor, head }
    }

    /// Endpoints in pane reading order: `(start, end)` with
    /// `start <= end`.
    pub fn ordered(&self) -> (PaneCursor, PaneCursor) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }

    /// True when the selection covers zero cells (`anchor == head`).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// True iff `anchor` and `head` are in the same block — the
    /// within-block case. The renderer can use this to route the
    /// selection to the legacy [`crate::block_selection::BlockSelection`]
    /// path unchanged.
    pub fn is_within_one_block(&self) -> bool {
        self.anchor.block_id == self.head.block_id
    }

    /// Compute the per-block `(start, end)` range that `block_id`
    /// contributes to this selection, in the block's unified row
    /// space (command label rows + snapshot rows).
    ///
    /// `block_total_rows` is the block's `command_lines + snapshot.len()`
    /// — the caller supplies it because this module is pure and
    /// doesn't carry block metadata.
    ///
    /// Returns:
    /// - `None` if `block_id` is outside the selection's block range
    ///   (above the start block or below the end block, or the
    ///   selection is empty).
    /// - `Some((start, end))` with `start <= end` clipped to this
    ///   block's bounds.
    ///
    /// Edge cases:
    /// - If `block_id == start.block_id == end.block_id`, returns
    ///   `(start_block_cursor, end_block_cursor)` — exactly the
    ///   within-block range.
    /// - If `block_id == start.block_id < end.block_id`, returns
    ///   `(start_block_cursor, end-of-block)`.
    /// - If `start.block_id < block_id < end.block_id`, returns
    ///   `(start-of-block, end-of-block)` — the whole block is in
    ///   the selection.
    /// - If `start.block_id < block_id == end.block_id`, returns
    ///   `(start-of-block, end_block_cursor)`.
    pub fn block_range_for(
        &self,
        block_id: BlockId,
        block_total_rows: usize,
    ) -> Option<(BlockCursor, BlockCursor)> {
        if self.is_empty() {
            return None;
        }
        let (start, end) = self.ordered();
        if block_id < start.block_id || block_id > end.block_id {
            return None;
        }
        // End-of-block cursor: one past the last row, col 0 — matches
        // the "cursor past the last char" convention BlockCursor uses
        // for line-select. For paint clipping the only thing that
        // matters is row >= last row, so this works.
        let end_of_block = BlockCursor::new(block_total_rows.saturating_sub(1), usize::MAX);
        let start_of_block = BlockCursor::new(0, 0);
        let start_bc = if block_id == start.block_id {
            BlockCursor::new(start.row, start.col)
        } else {
            start_of_block
        };
        let end_bc = if block_id == end.block_id {
            BlockCursor::new(end.row, end.col)
        } else {
            end_of_block
        };
        Some((start_bc, end_bc))
    }
}

/// One block's contribution to a [`PaneSelection`] text extraction:
/// just enough state to slice the block without coupling this module
/// to [`crate::pane::PaneSession`].
///
/// The renderer / `PaneSession` constructs these for the blocks
/// touched by the selection; this module owns the multi-block
/// stitching.
pub struct BlockSlice<'a> {
    pub block_id: BlockId,
    pub command: &'a str,
    pub snapshot: &'a [StyledLine],
}

impl<'a> BlockSlice<'a> {
    pub fn new(block_id: BlockId, command: &'a str, snapshot: &'a [StyledLine]) -> Self {
        Self { block_id, command, snapshot }
    }

    /// `(command_lines, snapshot.len())` — matches `PaneSession::sealed_block_rows`.
    pub fn rows(&self) -> (usize, usize) {
        let cmd_lines = if self.command.is_empty() { 0 } else { self.command.split('\n').count() };
        (cmd_lines, self.snapshot.len())
    }

    pub fn total_rows(&self) -> usize {
        let (c, s) = self.rows();
        c + s
    }
}

/// Materialise the text covered by `sel` across `blocks`.
///
/// `blocks` are supplied **in pane reading order** (top to bottom);
/// the caller iterates the [`crate::block::BlockStack`] and filters
/// to sealed blocks. Per-block text comes from
/// [`crate::block_selection::block_selection_text`] applied to that
/// block's clipped range.
///
/// Multi-block selections concatenate per-block payloads with a
/// single `\n`. Empty selections return `""`.
pub fn pane_selection_text(blocks: &[BlockSlice<'_>], sel: &PaneSelection) -> String {
    if sel.is_empty() {
        return String::new();
    }
    let (start, end) = sel.ordered();
    let mut out = String::new();
    let mut first = true;
    for b in blocks {
        if b.block_id < start.block_id || b.block_id > end.block_id {
            continue;
        }
        let total = b.total_rows();
        let Some((start_bc, end_bc)) = sel.block_range_for(b.block_id, total) else {
            continue;
        };
        // Build a BlockSelection for block_selection_text; the block_id
        // field is unused by that function but required by the type.
        let block_sel = crate::block_selection::BlockSelection::new(b.block_id, start_bc, end_bc);
        let text = crate::block_selection::block_selection_text(b.command, b.snapshot, &block_sel);
        if text.is_empty() {
            continue;
        }
        if !first {
            out.push('\n');
        }
        out.push_str(&text);
        first = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockId;
    use crate::terminal::{StyledCell, StyledLine};
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

    fn pc(block_id: u64, row: usize, col: usize) -> PaneCursor {
        PaneCursor::new(BlockId(block_id), row, col)
    }

    fn sel(anchor: PaneCursor, head: PaneCursor) -> PaneSelection {
        PaneSelection::new(anchor, head)
    }

    // ---- PaneCursor ordering ----------------------------------------

    #[test]
    fn pane_cursor_orders_by_block_id_first() {
        assert!(pc(1, 100, 100) < pc(2, 0, 0));
    }

    #[test]
    fn pane_cursor_within_same_block_orders_by_row_then_col() {
        assert!(pc(5, 0, 99) < pc(5, 1, 0));
        assert!(pc(5, 1, 3) < pc(5, 1, 4));
    }

    // ---- PaneSelection::ordered -------------------------------------

    #[test]
    fn pane_selection_ordered_returns_endpoints_in_pane_order() {
        let s = sel(pc(2, 0, 0), pc(1, 0, 0));
        let (start, end) = s.ordered();
        assert_eq!(start, pc(1, 0, 0));
        assert_eq!(end, pc(2, 0, 0));
    }

    #[test]
    fn pane_selection_already_ordered_passes_through() {
        let s = sel(pc(1, 0, 0), pc(2, 3, 4));
        let (start, end) = s.ordered();
        assert_eq!(start, pc(1, 0, 0));
        assert_eq!(end, pc(2, 3, 4));
    }

    #[test]
    fn pane_selection_is_empty_when_anchor_equals_head() {
        let s = sel(pc(1, 2, 3), pc(1, 2, 3));
        assert!(s.is_empty());
    }

    #[test]
    fn pane_selection_is_within_one_block_iff_same_block_id() {
        assert!(sel(pc(1, 0, 0), pc(1, 5, 9)).is_within_one_block());
        assert!(!sel(pc(1, 0, 0), pc(2, 0, 0)).is_within_one_block());
    }

    // ---- PaneSelection::block_range_for -----------------------------

    #[test]
    fn block_range_for_above_selection_returns_none() {
        let s = sel(pc(5, 0, 0), pc(7, 3, 4));
        assert!(s.block_range_for(BlockId(4), 10).is_none());
    }

    #[test]
    fn block_range_for_below_selection_returns_none() {
        let s = sel(pc(5, 0, 0), pc(7, 3, 4));
        assert!(s.block_range_for(BlockId(8), 10).is_none());
    }

    #[test]
    fn block_range_for_within_one_block_returns_exact_range() {
        let s = sel(pc(5, 1, 3), pc(5, 4, 7));
        let (start, end) = s.block_range_for(BlockId(5), 10).unwrap();
        assert_eq!(start, BlockCursor::new(1, 3));
        assert_eq!(end, BlockCursor::new(4, 7));
    }

    #[test]
    fn block_range_for_start_block_runs_from_anchor_to_end_of_block() {
        let s = sel(pc(5, 2, 3), pc(7, 1, 0));
        let (start, end) = s.block_range_for(BlockId(5), 8).unwrap();
        assert_eq!(start, BlockCursor::new(2, 3));
        // End-of-block: row 7 (last), col MAX (will get clamped by
        // block_selection_text to row len).
        assert_eq!(end.row, 7);
        assert_eq!(end.col, usize::MAX);
    }

    #[test]
    fn block_range_for_middle_block_runs_full_block() {
        let s = sel(pc(5, 0, 0), pc(7, 0, 0));
        let (start, end) = s.block_range_for(BlockId(6), 4).unwrap();
        assert_eq!(start, BlockCursor::new(0, 0));
        assert_eq!(end.row, 3);
        assert_eq!(end.col, usize::MAX);
    }

    #[test]
    fn block_range_for_end_block_runs_from_block_start_to_head() {
        let s = sel(pc(5, 0, 0), pc(7, 2, 5));
        let (start, end) = s.block_range_for(BlockId(7), 10).unwrap();
        assert_eq!(start, BlockCursor::new(0, 0));
        assert_eq!(end, BlockCursor::new(2, 5));
    }

    #[test]
    fn block_range_for_empty_selection_returns_none() {
        let s = sel(pc(5, 1, 2), pc(5, 1, 2));
        assert!(s.block_range_for(BlockId(5), 10).is_none());
    }

    #[test]
    fn block_range_for_reverse_selection_normalises_via_ordered() {
        // Anchor in block 7, head dragged up into block 5. Querying
        // block 6 still produces a full-block range.
        let s = sel(pc(7, 2, 0), pc(5, 1, 4));
        let (start, end) = s.block_range_for(BlockId(6), 5).unwrap();
        assert_eq!(start, BlockCursor::new(0, 0));
        assert_eq!(end.row, 4);
    }

    // ---- pane_selection_text ----------------------------------------

    #[test]
    fn pane_selection_text_empty_for_degenerate_selection() {
        let s = sel(pc(1, 0, 0), pc(1, 0, 0));
        let block = snap(&["hello"]);
        let blocks = vec![BlockSlice::new(BlockId(1), "", &block)];
        assert_eq!(pane_selection_text(&blocks, &s), "");
    }

    #[test]
    fn pane_selection_text_within_one_block_matches_block_selection() {
        // Single-block parity with the existing block_selection_text.
        let snapshot = snap(&["hello world", "second line"]);
        let s = sel(pc(1, 0, 6), pc(1, 1, 6));
        let blocks = vec![BlockSlice::new(BlockId(1), "", &snapshot)];
        assert_eq!(pane_selection_text(&blocks, &s), "world\nsecond");
    }

    #[test]
    fn pane_selection_text_spans_two_blocks() {
        // Block 1: snapshot "abc / def" (2 rows). Block 2: snapshot
        // "ghi" (1 row). Selection from block 1 row 0 col 2 ('c') to
        // block 2 row 0 col 2 ('h' end). Expected: "c\ndef\ngh".
        let snap1 = snap(&["abc", "def"]);
        let snap2 = snap(&["ghi"]);
        let blocks =
            vec![BlockSlice::new(BlockId(1), "", &snap1), BlockSlice::new(BlockId(2), "", &snap2)];
        let s = sel(pc(1, 0, 2), pc(2, 0, 2));
        assert_eq!(pane_selection_text(&blocks, &s), "c\ndef\ngh");
    }

    #[test]
    fn pane_selection_text_spans_three_blocks_with_middle_full() {
        let snap1 = snap(&["alpha", "beta"]);
        let snap2 = snap(&["gamma"]);
        let snap3 = snap(&["delta"]);
        let blocks = vec![
            BlockSlice::new(BlockId(1), "", &snap1),
            BlockSlice::new(BlockId(2), "", &snap2),
            BlockSlice::new(BlockId(3), "", &snap3),
        ];
        // From "ha" in block 1 row 0 to "lt" in block 3 row 0:
        // expect "ha\nbeta\ngamma\ndelt".
        let s = sel(pc(1, 0, 3), pc(3, 0, 4));
        assert_eq!(pane_selection_text(&blocks, &s), "ha\nbeta\ngamma\ndelt");
    }

    #[test]
    fn pane_selection_text_reverse_drag_yields_same_payload() {
        // Same expected payload regardless of which end is anchor.
        let snap1 = snap(&["abc"]);
        let snap2 = snap(&["def"]);
        let blocks =
            vec![BlockSlice::new(BlockId(1), "", &snap1), BlockSlice::new(BlockId(2), "", &snap2)];
        let forward = sel(pc(1, 0, 0), pc(2, 0, 3));
        let backward = sel(pc(2, 0, 3), pc(1, 0, 0));
        assert_eq!(pane_selection_text(&blocks, &forward), pane_selection_text(&blocks, &backward));
        assert_eq!(pane_selection_text(&blocks, &forward), "abc\ndef");
    }

    #[test]
    fn pane_selection_text_handles_command_lines_in_start_block() {
        // Block 1 has a multi-line command "echo a\necho b" plus
        // snapshot ["out"]. Selecting from row 0 col 5 (after "echo ")
        // through block 2 row 0 col 3 should yield "a\necho b\nout\nxyz".
        let snap1 = snap(&["out"]);
        let snap2 = snap(&["xyz"]);
        let blocks = vec![
            BlockSlice::new(BlockId(1), "echo a\necho b", &snap1),
            BlockSlice::new(BlockId(2), "", &snap2),
        ];
        let s = sel(pc(1, 0, 5), pc(2, 0, 3));
        assert_eq!(pane_selection_text(&blocks, &s), "a\necho b\nout\nxyz");
    }

    #[test]
    fn pane_selection_text_skips_unselected_blocks() {
        // Selection between blocks 2 and 3; block 1 and block 5
        // contribute nothing even though they're in the list.
        let snap1 = snap(&["one"]);
        let snap2 = snap(&["two"]);
        let snap3 = snap(&["three"]);
        let snap5 = snap(&["five"]);
        let blocks = vec![
            BlockSlice::new(BlockId(1), "", &snap1),
            BlockSlice::new(BlockId(2), "", &snap2),
            BlockSlice::new(BlockId(3), "", &snap3),
            BlockSlice::new(BlockId(5), "", &snap5),
        ];
        let s = sel(pc(2, 0, 0), pc(3, 0, 5));
        assert_eq!(pane_selection_text(&blocks, &s), "two\nthree");
    }

    #[test]
    fn pane_selection_text_single_block_falls_through_to_within_block_helper() {
        // The single-block path uses the within-block helper. We don't
        // re-test all of block_selection_text's edge cases here — its
        // own tests cover that — but we confirm the integration.
        let snap1 = snap(&["hello   "]);
        let blocks = vec![BlockSlice::new(BlockId(7), "", &snap1)];
        let s = sel(pc(7, 0, 0), pc(7, 0, 8));
        // Trailing spaces are trimmed per the block_selection_text rule.
        assert_eq!(pane_selection_text(&blocks, &s), "hello");
    }
}
