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
//! - It does not own multi-click mode (Word / Line vs. Char). The
//!   click count + anchor word/line bounds live on
//!   `PaneUiState::sealed_drag_anchor`. This module's
//!   [`extend_multiclick_selection_endpoints`] takes those bounds
//!   plus the head pointer's word/line bounds and returns the new
//!   `(anchor, head)` PaneCursors — applies the same far-edge rule
//!   for same-block and cross-block drags.
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

/// Compute the new `(anchor, head)` PaneCursors for a multi-click
/// drag-extend, given the original word/line bounds at the anchor
/// block and the word/line bounds under the pointer at the head
/// block.
///
/// **Rule:** each endpoint uses the FAR edge of its word/line
/// within its block — the edge facing AWAY from the other
/// endpoint. The two cases:
///
/// 1. **Same block** (`anchor_block == head_block`): the
///    out-going `(anchor, head)` are `(min(a_start, h_start),
///    max(a_end, h_end))` — the rolling union the within-block
///    drag has always done.
///
/// 2. **Cross block** (`anchor_block != head_block`): direction
///    of the drag (head above or below anchor in pane order)
///    determines which edge each endpoint takes:
///    - Head AFTER anchor (`head_block > anchor_block`): anchor
///      = `a_start` (upper edge of upper block), head = `h_end`
///      (lower edge of lower block).
///    - Head BEFORE anchor (`head_block < anchor_block`): anchor
///      = `a_end` (lower edge of lower block), head = `h_start`
///      (upper edge of upper block).
///
/// This unifies same-block and cross-block multi-click extend
/// behind one rule, and prevents two regressions the cross-block
/// PR shipped with:
///
/// - Forward drag from a multi-click in block A down into block
///   B would degrade to char-mode in B (no word/line snapping).
/// - Backward drag from a multi-click in block A up into block U
///   would "lose" the anchor word's highlight in A because
///   `PaneSelection::ordered()` puts U first and `block_range_for(A)`
///   then runs `(start_of_A, anchor_pos)` — putting the anchor
///   at the word's START makes the range stop BEFORE the word.
///   Using the word's END as the anchor cursor in that direction
///   makes the range include the word.
///
/// Pure function: no egui, no PaneSession. Tested directly.
pub fn extend_multiclick_selection_endpoints(
    anchor_block: BlockId,
    anchor_word_bounds: (BlockCursor, BlockCursor),
    head_block: BlockId,
    head_word_bounds: (BlockCursor, BlockCursor),
) -> (PaneCursor, PaneCursor) {
    let (a_start, a_end) = anchor_word_bounds;
    let (h_start, h_end) = head_word_bounds;

    if anchor_block == head_block {
        // Same block — rolling union. Equivalent to picking the
        // outer pair of endpoints across both bounds, which is
        // what the existing within-block drag has always done.
        let start = a_start.min(h_start);
        let end = a_end.max(h_end);
        return (
            PaneCursor::in_block(anchor_block, start),
            PaneCursor::in_block(anchor_block, end),
        );
    }

    if head_block > anchor_block {
        // Drag DOWN: anchor is the upper block (start of the
        // selection in pane order). Use a_start as the
        // upper/left edge; h_end as the lower/right edge.
        (PaneCursor::in_block(anchor_block, a_start), PaneCursor::in_block(head_block, h_end))
    } else {
        // Drag UP: head is the upper block. anchor is the lower
        // block; use a_end as the lower edge. Head uses h_start
        // as the upper edge.
        (PaneCursor::in_block(anchor_block, a_end), PaneCursor::in_block(head_block, h_start))
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

    // ---- extend_multiclick_selection_endpoints ---------------------
    //
    // Multi-click + drag extend rule (works for both word and line
    // modes). Given the original word/line bounds at the anchor block
    // and the word/line bounds at the head block, produce the new
    // (anchor, head) PaneCursors so that BOTH endpoint blocks have
    // their FULL word/line included.
    //
    // The rule: each endpoint uses the FAR EDGE of its word/line
    // within its block — the edge facing AWAY from the other
    // endpoint. For same-block selections this collapses to the
    // existing rolling-union semantics (start = min, end = max).
    // For cross-block selections it preserves the visual that the
    // user expects: drag from "foo" in block A down to "bar" in
    // block B and the selection covers `foo … bar` with FOO and
    // BAR each fully highlighted, not char-truncated.
    //
    // Per the user-reported regression: pre-fix, the cross-block
    // path silently degraded to char-mode for the head block and
    // (for backward drag) "lost" the original anchor word in the
    // start block because the anchor cursor stopped being on the
    // word's far edge under `ordered()`. The unified far-edge rule
    // fixes both.

    fn bc(row: usize, col: usize) -> BlockCursor {
        BlockCursor::new(row, col)
    }

    #[test]
    fn extend_multiclick_same_block_unions_word_ranges() {
        // Same block, head word later than anchor word: unioned.
        // (anchor_start, head_end) covers both.
        let anchor = BlockId(5);
        let a_bounds = (bc(0, 0), bc(0, 3));
        let h_bounds = (bc(0, 10), bc(0, 15));
        let (anc, head) = extend_multiclick_selection_endpoints(anchor, a_bounds, anchor, h_bounds);
        assert_eq!(anc, PaneCursor::new(BlockId(5), 0, 0));
        assert_eq!(head, PaneCursor::new(BlockId(5), 0, 15));
    }

    #[test]
    fn extend_multiclick_same_block_reverse_drag_unions_outward() {
        // Same block but head word EARLIER than anchor word —
        // still unions to the outer bounds (start = min, end = max).
        let b = BlockId(5);
        let a_bounds = (bc(2, 10), bc(2, 15)); // "anchor"
        let h_bounds = (bc(0, 0), bc(0, 3)); // "head" earlier
        let (anc, head) = extend_multiclick_selection_endpoints(b, a_bounds, b, h_bounds);
        assert_eq!(anc, PaneCursor::new(BlockId(5), 0, 0));
        assert_eq!(head, PaneCursor::new(BlockId(5), 2, 15));
    }

    #[test]
    fn extend_multiclick_cross_block_forward_keeps_both_full_words() {
        // BUG 1 repro: double-click word in block 5, drag down into
        // block 7's word at cols 10..15. Pre-fix, head landed at
        // char-precision in block 7. Fix: head uses h_end (far
        // edge), anchor uses a_start (far edge of upper block).
        let a_bounds = (bc(0, 0), bc(0, 3)); // word at start of block 5
        let h_bounds = (bc(0, 10), bc(0, 15)); // word in block 7
        let (anc, head) =
            extend_multiclick_selection_endpoints(BlockId(5), a_bounds, BlockId(7), h_bounds);
        assert_eq!(anc, PaneCursor::new(BlockId(5), 0, 0));
        assert_eq!(head, PaneCursor::new(BlockId(7), 0, 15));
    }

    #[test]
    fn extend_multiclick_cross_block_backward_keeps_both_full_words() {
        // BUG 2 repro: double-click word in block 7, drag UP into
        // block 5's word. Pre-fix, the anchor word at block 7
        // disappeared because `ordered()` flipped the endpoints
        // and the anchor at a_start became the high end of the
        // selection — putting block 7's range at
        // `(start_of_block_7, a_start)`, which doesn't include
        // the word. Fix: anchor uses a_end (far edge of LOWER
        // block); head uses h_start (far edge of UPPER block).
        let a_bounds = (bc(0, 10), bc(0, 15)); // word in block 7
        let h_bounds = (bc(0, 0), bc(0, 3)); // word in block 5 (upper)
        let (anc, head) =
            extend_multiclick_selection_endpoints(BlockId(7), a_bounds, BlockId(5), h_bounds);
        // Anchor stays in block 7 but at the FAR edge (a_end).
        assert_eq!(anc, PaneCursor::new(BlockId(7), 0, 15));
        // Head in block 5 at the FAR edge (h_start).
        assert_eq!(head, PaneCursor::new(BlockId(5), 0, 0));

        // Sanity: the resulting selection's `ordered()` would put
        // block 5 first; block_range_for(block 7) is the END
        // block, range `(start_of_block, a_end=15)` — includes the
        // original word at cols 10..15. ✓
        let s = PaneSelection::new(anc, head);
        let (start, end) = s.ordered();
        assert_eq!(start.block_id, BlockId(5));
        assert_eq!(end.block_id, BlockId(7));
        assert_eq!(end.col, 15, "block 7 selection ends at the word's right edge");
    }

    #[test]
    fn extend_multiclick_cross_block_with_degenerate_head_bounds_uses_pointer_col() {
        // Pointer is over whitespace in the head block, so the
        // word range there is degenerate `(col, col)`. The
        // far-edge rule still applies — head lands at that col;
        // the helper does NOT misinterpret degenerate bounds.
        let a_bounds = (bc(0, 0), bc(0, 3));
        let h_bounds = (bc(0, 7), bc(0, 7)); // degenerate
        let (anc, head) =
            extend_multiclick_selection_endpoints(BlockId(5), a_bounds, BlockId(7), h_bounds);
        assert_eq!(anc, PaneCursor::new(BlockId(5), 0, 0));
        assert_eq!(head, PaneCursor::new(BlockId(7), 0, 7));
    }

    #[test]
    fn extend_multiclick_line_mode_uses_same_far_edge_rule() {
        // The helper doesn't know "word" vs "line" — it just takes
        // the bounds. Line bounds at row 0 of block 5 (cols 0..20)
        // + bounds at row 2 of block 7 (cols 0..14) → block 5
        // ends at row 0 col 0 (far edge), block 7 ends at row 2
        // col 14 (far edge). Whole lines highlighted in each.
        let a_bounds = (bc(0, 0), bc(0, 20));
        let h_bounds = (bc(2, 0), bc(2, 14));
        let (anc, head) =
            extend_multiclick_selection_endpoints(BlockId(5), a_bounds, BlockId(7), h_bounds);
        assert_eq!(anc, PaneCursor::new(BlockId(5), 0, 0));
        assert_eq!(head, PaneCursor::new(BlockId(7), 2, 14));
    }
}
