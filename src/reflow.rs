//! The keystone of sealed-block reflow (Phase 9B): a [`ReflowMap`] that
//! re-wraps a block's width-independent **logical lines** into the
//! **visual rows** painted at the current pane width, and translates
//! coordinates between the two spaces in both directions.
//!
//! Sealed blocks are stored as logical lines (see
//! [spec/08 §"Logical lines"](../spec/08-persistence.md#logical-lines-not-grid-rows)
//! and [`crate::persist::chunk`]); the renderer paints visual rows. A
//! selection or a search match is anchored in **logical** coordinates
//! so it survives a resize, but hit-testing starts from a pixel (→ a
//! visual row/col) and painting a highlight needs visual rects. This
//! module is the pure bridge:
//!
//! - [`ReflowMap::visual_rows`] — the rows to paint at this width.
//! - [`ReflowMap::visual_to_logical`] — pixel hit → logical cursor.
//! - [`ReflowMap::logical_to_visual`] — logical endpoint → visual cell,
//!   for painting selection / match highlights.
//!
//! Pure logic, no egui. Built on [`crate::persist::chunk::wrap_logical_line`],
//! whose inverse ([`crate::persist::chunk::unwrap_rows`]) produced the
//! logical lines in the first place; the three compose so that
//! `visual_to_logical` and `logical_to_visual` are inverses up to the
//! usual wide-char / past-the-end clamping.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use alacritty_terminal::term::cell::Flags;

use crate::block::BlockId;
use crate::block_selection::BlockCursor;
use crate::persist::chunk::wrap_logical_line;
use crate::terminal::{StyledCell, StyledLine};

/// A logical position within a block's output: which logical line, and
/// which logical cell within it (wide chars count as one cell; there
/// are no spacer cells in logical space). Either field may sit one past
/// the end, matching the cursor-after-last-char convention used by
/// [`crate::block_selection::BlockCursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogicalPos {
    pub line: usize,
    pub col: usize,
}

impl LogicalPos {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// A visual position within the painted rows: a wrapped row index and a
/// column in *visual* cells (wide chars span two visual columns; spacer
/// cells occupy the second).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VisualPos {
    pub row: usize,
    pub col: usize,
}

impl VisualPos {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// True for a wide-char spacer cell — present in visual rows, absent
/// from logical lines, so it never advances a logical column.
fn is_spacer(cell: &StyledCell) -> bool {
    cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
}

/// The re-wrapping of one block's logical lines at a fixed width, plus
/// the coordinate map between logical and visual space. Rebuild when
/// the pane width changes; cache otherwise.
#[derive(Debug, Clone)]
pub struct ReflowMap {
    /// The wrapped rows to paint, top to bottom.
    rows: Vec<StyledLine>,
    /// Per visual row: `(logical_line, logical_start_col)` — the logical
    /// line it belongs to and the logical column at which this row
    /// begins. Parallel to `rows`.
    row_origin: Vec<(usize, usize)>,
    /// Per logical line: the half-open range of visual-row indices it
    /// occupies. Parallel to the input logical lines.
    visual_span: Vec<(usize, usize)>,
}

impl ReflowMap {
    /// Wrap `logical` lines for a terminal `cols` columns wide and build
    /// the coordinate map. An empty input yields an empty map.
    pub fn build(logical: &[StyledLine], cols: usize) -> Self {
        let mut rows = Vec::new();
        let mut row_origin = Vec::new();
        let mut visual_span = Vec::with_capacity(logical.len());

        for line in logical {
            let start_row = rows.len();
            let wrapped = wrap_logical_line(line, cols);
            let mut col_off = 0usize;
            for vrow in wrapped {
                let logical_cells = vrow.cells.iter().filter(|c| !is_spacer(c)).count();
                row_origin.push((visual_span.len(), col_off));
                rows.push(vrow);
                col_off += logical_cells;
            }
            visual_span.push((start_row, rows.len()));
        }

        ReflowMap { rows, row_origin, visual_span }
    }

    /// The visual rows to paint.
    pub fn visual_rows(&self) -> &[StyledLine] {
        &self.rows
    }

    /// Number of visual rows.
    pub fn visual_row_count(&self) -> usize {
        self.rows.len()
    }

    /// Number of logical lines this map was built from.
    pub fn logical_line_count(&self) -> usize {
        self.visual_span.len()
    }

    /// Translate a visual position (from a pixel hit) into the logical
    /// position it addresses. A wide char's two visual columns both map
    /// to its single logical column. `row` past the end clamps to the
    /// last row; `col` past a row's content clamps to the row's logical
    /// length, so a click in the right margin lands after the last char.
    pub fn visual_to_logical(&self, v: VisualPos) -> LogicalPos {
        if self.rows.is_empty() {
            return LogicalPos::new(0, 0);
        }
        let row = v.row.min(self.rows.len() - 1);
        let (line, start_col) = self.row_origin[row];
        let cells = &self.rows[row].cells;
        let vcol = v.col.min(cells.len());
        // Logical cells (non-spacers) strictly before the clicked cell.
        let mut offset = cells[..vcol].iter().filter(|c| !is_spacer(c)).count();
        // A click on a wide char's spacer (its right half) belongs to the
        // wide char itself, not the cell after it.
        if vcol < cells.len() && is_spacer(&cells[vcol]) {
            offset = offset.saturating_sub(1);
        }
        LogicalPos::new(line, start_col + offset)
    }

    /// Translate a logical position into the visual cell that renders
    /// it, for painting a selection or match highlight. A logical column
    /// past a line's content maps to the visual column just past its
    /// last cell. A logical line index past the end clamps to the last
    /// visual row.
    pub fn logical_to_visual(&self, l: LogicalPos) -> VisualPos {
        if self.rows.is_empty() {
            return VisualPos::new(0, 0);
        }
        if l.line >= self.visual_span.len() {
            return VisualPos::new(self.rows.len(), 0);
        }
        let (start_row, end_row) = self.visual_span[l.line];
        // Find the visual row of this logical line that contains column
        // `l.col`: the last row whose start_col <= l.col. Default to the
        // final row of the line so an over-the-end column lands there.
        let mut target = end_row.saturating_sub(1).max(start_row);
        for row in start_row..end_row {
            let (_, start_col) = self.row_origin[row];
            let next_start =
                if row + 1 < end_row { self.row_origin[row + 1].1 } else { usize::MAX };
            if l.col >= start_col && l.col < next_start {
                target = row;
                break;
            }
        }
        let start_col = self.row_origin[target].1;
        let want = l.col.saturating_sub(start_col);
        let cells = &self.rows[target].cells;
        // Advance past `want` logical cells.
        let mut passed = 0usize;
        let mut vcol = 0usize;
        while vcol < cells.len() && passed < want {
            if !is_spacer(&cells[vcol]) {
                passed += 1;
            }
            vcol += 1;
        }
        // If we landed on a wide char's spacer, step past it so the
        // column sits after the full glyph (a cursor "before" the next
        // logical cell, not inside the wide char).
        if vcol < cells.len() && is_spacer(&cells[vcol]) {
            vcol += 1;
        }
        VisualPos::new(target, vcol)
    }
}

/// Per-pane cache of [`ReflowMap`]s — one per sealed block, all built
/// for a single pane width. Sealed blocks are immutable once sealed, so
/// the cache only needs rebuilding when the width changes or the set of
/// sealed blocks changes (a new command sealed, or Cmd-K cleared them).
#[derive(Debug, Default)]
pub struct ReflowCache {
    cols: usize,
    block_count: usize,
    maps_by_block: HashMap<BlockId, ReflowMap>,
}

impl ReflowCache {
    /// True when the cache already reflects this width and sealed-block
    /// count, so a rebuild can be skipped.
    pub fn is_current(&self, cols: usize, block_count: usize) -> bool {
        self.cols == cols && self.block_count == block_count
    }

    /// Replace the cache contents wholesale (after a width or block-set
    /// change). The caller builds the maps from the current blocks.
    pub fn replace(
        &mut self,
        cols: usize,
        block_count: usize,
        maps_by_block: HashMap<BlockId, ReflowMap>,
    ) {
        self.cols = cols;
        self.block_count = block_count;
        self.maps_by_block = maps_by_block;
    }

    /// The cached map for a sealed block, if present.
    pub fn get(&self, block_id: BlockId) -> Option<&ReflowMap> {
        self.maps_by_block.get(&block_id)
    }
}

/// Convert a unified-space cursor from LOGICAL to VISUAL coordinates,
/// for painting a selection. A sealed block's unified row space is
/// `command_lines` rows of command label (never reflowed — 1:1) followed
/// by the output's *logical* lines. Command rows pass through; output
/// rows (`row >= cmd_lines`, i.e. logical line `row - cmd_lines`) map
/// through `map`.
pub fn unified_logical_to_visual(c: BlockCursor, cmd_lines: usize, map: &ReflowMap) -> BlockCursor {
    if c.row < cmd_lines {
        c
    } else {
        let v = map.logical_to_visual(LogicalPos::new(c.row - cmd_lines, c.col));
        BlockCursor::new(cmd_lines + v.row, v.col)
    }
}

/// Convert a unified-space `(visual_row, col)` from a pixel hit into a
/// LOGICAL cursor, for storing a selection. Inverse of
/// [`unified_logical_to_visual`]. Command rows pass through; visual
/// output rows map back to logical via `map`.
pub fn unified_visual_to_logical(
    visual_row: usize,
    col: usize,
    cmd_lines: usize,
    map: &ReflowMap,
) -> BlockCursor {
    if visual_row < cmd_lines {
        BlockCursor::new(visual_row, col)
    } else {
        let l = map.visual_to_logical(VisualPos::new(visual_row - cmd_lines, col));
        BlockCursor::new(cmd_lines + l.line, l.col)
    }
}

/// Total visual rows the output occupies after reflow — for clamping a
/// pixel hit to the painted height.
pub fn visual_output_rows(map: &ReflowMap) -> usize {
    map.visual_row_count()
}

/// Translate a find match on output logical line `logical_row`, columns
/// `[c0, c1)`, into the visual highlight segments to paint (one per
/// visual row the match spans after reflow). Each segment is
/// `(visual_row, col_start, col_end, is_current)` in the snapshot's
/// visual-row space, matching `render::paint_match_highlights`.
pub fn output_match_visual_segments(
    map: &ReflowMap,
    logical_row: usize,
    c0: usize,
    c1: usize,
    is_current: bool,
) -> Vec<(usize, usize, usize, bool)> {
    if c1 <= c0 {
        return Vec::new();
    }
    let start = map.logical_to_visual(LogicalPos::new(logical_row, c0));
    let end = map.logical_to_visual(LogicalPos::new(logical_row, c1));
    let row_len = |r: usize| map.visual_rows().get(r).map(|row| row.cells.len()).unwrap_or(0);

    if start.row == end.row {
        return vec![(start.row, start.col, end.col, is_current)];
    }
    let mut out = Vec::new();
    out.push((start.row, start.col, row_len(start.row), is_current));
    for r in (start.row + 1)..end.row {
        out.push((r, 0, row_len(r), is_current));
    }
    out.push((end.row, 0, end.col, is_current));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    fn cell(c: char) -> StyledCell {
        StyledCell {
            c,
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }

    fn wide(c: char) -> StyledCell {
        StyledCell {
            c,
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::WIDE_CHAR,
        }
    }

    fn line(s: &str) -> StyledLine {
        StyledLine { cells: s.chars().map(cell).collect() }
    }

    #[test]
    fn empty_input_is_empty_map() {
        let m = ReflowMap::build(&[], 80);
        assert_eq!(m.visual_row_count(), 0);
        assert_eq!(m.logical_line_count(), 0);
        // Degenerate lookups don't panic.
        assert_eq!(m.visual_to_logical(VisualPos::new(0, 0)), LogicalPos::new(0, 0));
        assert_eq!(m.logical_to_visual(LogicalPos::new(0, 0)), VisualPos::new(0, 0));
    }

    #[test]
    fn no_wrap_when_lines_fit_is_identity() {
        let logical = vec![line("abc"), line("de")];
        let m = ReflowMap::build(&logical, 80);
        assert_eq!(m.visual_row_count(), 2);
        assert_eq!(m.logical_line_count(), 2);
        // Visual rows equal the logical lines unchanged.
        assert_eq!(m.visual_rows(), &logical[..]);
        // Round-trip a few positions.
        for (li, lcol) in [(0usize, 0usize), (0, 2), (1, 1)] {
            let lp = LogicalPos::new(li, lcol);
            let vp = m.logical_to_visual(lp);
            assert_eq!(m.visual_to_logical(vp), lp);
        }
    }

    #[test]
    fn wrapped_line_maps_second_row_to_continued_logical_cols() {
        // "abcde" at width 3 -> rows ["abc","de"], one logical line.
        let m = ReflowMap::build(&[line("abcde")], 3);
        assert_eq!(m.visual_row_count(), 2);
        assert_eq!(m.logical_line_count(), 1);
        // Visual (row 1, col 0) is logical col 3 ('d').
        assert_eq!(m.visual_to_logical(VisualPos::new(1, 0)), LogicalPos::new(0, 3));
        // Visual (row 1, col 1) is logical col 4 ('e').
        assert_eq!(m.visual_to_logical(VisualPos::new(1, 1)), LogicalPos::new(0, 4));
        // And back: logical col 4 -> visual row 1 col 1.
        assert_eq!(m.logical_to_visual(LogicalPos::new(0, 4)), VisualPos::new(1, 1));
        // Logical col 2 ('c') is on the first row.
        assert_eq!(m.logical_to_visual(LogicalPos::new(0, 2)), VisualPos::new(0, 2));
    }

    #[test]
    fn second_logical_line_offsets_visual_rows() {
        // Two logical lines, the first wraps. "abcd"(w3)->2 rows; "xy"->1.
        let m = ReflowMap::build(&[line("abcd"), line("xy")], 3);
        assert_eq!(m.visual_row_count(), 3);
        // Visual row 2 is logical line 1, col 0.
        assert_eq!(m.visual_to_logical(VisualPos::new(2, 0)), LogicalPos::new(1, 0));
        assert_eq!(m.logical_to_visual(LogicalPos::new(1, 1)), VisualPos::new(2, 1));
    }

    #[test]
    fn click_past_row_end_clamps_to_logical_length() {
        // "abc" width 80: click at col 50 -> logical col 3 (after 'c').
        let m = ReflowMap::build(&[line("abc")], 80);
        assert_eq!(m.visual_to_logical(VisualPos::new(0, 50)), LogicalPos::new(0, 3));
        // Click below the last row clamps to the last row.
        assert_eq!(m.visual_to_logical(VisualPos::new(99, 0)), LogicalPos::new(0, 0));
    }

    #[test]
    fn wide_char_two_visual_cols_map_to_one_logical_col() {
        // [a, 日(wide), b] at width 80 -> one row: a,日,spacer,b.
        let logical = vec![StyledLine { cells: vec![cell('a'), wide('日'), cell('b')] }];
        let m = ReflowMap::build(&logical, 80);
        assert_eq!(m.visual_row_count(), 1);
        // Visual col 1 (日) and col 2 (its spacer) both -> logical col 1.
        assert_eq!(m.visual_to_logical(VisualPos::new(0, 1)), LogicalPos::new(0, 1));
        assert_eq!(m.visual_to_logical(VisualPos::new(0, 2)), LogicalPos::new(0, 1));
        // Visual col 3 ('b') -> logical col 2.
        assert_eq!(m.visual_to_logical(VisualPos::new(0, 3)), LogicalPos::new(0, 2));
        // Logical col 2 ('b') -> visual col 3 (past the wide char + spacer).
        assert_eq!(m.logical_to_visual(LogicalPos::new(0, 2)), VisualPos::new(0, 3));
    }

    #[test]
    fn unified_conversions_pass_command_rows_and_map_output_rows() {
        // "abcde" at width 3 -> 2 visual output rows. One command line.
        let map = ReflowMap::build(&[line("abcde")], 3);
        let cmd_lines = 1;
        // A command-row cursor (row 0) is unchanged in both directions.
        let cmd_cur = BlockCursor::new(0, 2);
        assert_eq!(unified_logical_to_visual(cmd_cur, cmd_lines, &map), cmd_cur);
        assert_eq!(unified_visual_to_logical(0, 2, cmd_lines, &map), cmd_cur);
        // Output logical line 0 col 3 ('d') is unified row `cmd_lines`,
        // and maps to visual unified row 2 (cmd row + 2nd output visual
        // row), col 0.
        let logical = BlockCursor::new(cmd_lines, 3);
        let visual = unified_logical_to_visual(logical, cmd_lines, &map);
        assert_eq!(visual, BlockCursor::new(2, 0));
        // And back.
        assert_eq!(unified_visual_to_logical(2, 0, cmd_lines, &map), logical);
    }

    #[test]
    fn output_match_spanning_a_wrap_splits_into_per_row_segments() {
        // "abcde" at width 3 -> rows ["abc","de"]. A match over the whole
        // logical line [0,5) paints as two segments: row 0 cols 0..3 and
        // row 1 cols 0..2.
        let map = ReflowMap::build(&[line("abcde")], 3);
        let segs = output_match_visual_segments(&map, 0, 0, 5, true);
        assert_eq!(segs, vec![(0, 0, 3, true), (1, 0, 2, true)]);
        // A match within one visual row stays a single segment.
        let segs = output_match_visual_segments(&map, 0, 0, 2, false);
        assert_eq!(segs, vec![(0, 0, 2, false)]);
        // Empty range yields nothing.
        assert!(output_match_visual_segments(&map, 0, 3, 3, false).is_empty());
    }

    #[test]
    fn reflow_cache_tracks_width_and_block_count() {
        let mut cache = ReflowCache::default();
        assert!(!cache.is_current(80, 1));
        let mut maps = HashMap::new();
        maps.insert(BlockId(3), ReflowMap::build(&[line("hi")], 80));
        cache.replace(80, 1, maps);
        assert!(cache.is_current(80, 1));
        assert!(!cache.is_current(40, 1), "width change invalidates");
        assert!(!cache.is_current(80, 2), "block-count change invalidates");
        assert!(cache.get(BlockId(3)).is_some());
        assert!(cache.get(BlockId(9)).is_none());
    }

    #[test]
    fn logical_to_visual_roundtrips_over_wrapped_wide_content() {
        // A line mixing wide chars, wrapped narrow, exercises both maps.
        let logical = vec![StyledLine {
            cells: vec![cell('a'), wide('日'), cell('b'), wide('本'), cell('c')],
        }];
        for cols in [2usize, 3, 4, 5, 80] {
            let m = ReflowMap::build(&logical, cols);
            for lcol in 0..=5 {
                let lp = LogicalPos::new(0, lcol);
                let vp = m.logical_to_visual(lp);
                // logical -> visual -> logical is identity for in-range cols.
                if lcol < 5 {
                    assert_eq!(
                        m.visual_to_logical(vp),
                        lp,
                        "cols={cols} lcol={lcol}: {vp:?} did not map back"
                    );
                }
            }
        }
    }
}
