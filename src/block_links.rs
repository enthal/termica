//! Sealed-block URL / file-path detection.
//!
//! The live-grid scanners in [`crate::links`] and [`crate::paths`]
//! operate on alacritty's `Grid<Cell>`. In editor mode the live grid
//! is skipped (the editor footer covers the next-prompt area), so
//! URLs and paths that scroll past into a sealed block snapshot fall
//! out of those scanners' reach. This module re-uses the same pure
//! per-row scanners (`find_urls_in_chars`, `find_paths_in_chars`)
//! but indexes positions in a sealed block's **unified row space**
//! — rows `0..command_lines` come from the command label, the rest
//! from the snapshot — matching the indexing the selection code
//! ([`crate::block_selection`]) already uses.
//!
//! Outputs are translated to [`BlockLinkSpan`]s, a sealed-block-
//! shaped analogue of [`crate::links::LinkSpan`]. The render layer
//! consumes them for hover-underline (with `Cmd` held), open-on-
//! Cmd-click, and "select the whole link" on double-click.

#![forbid(unsafe_code)]

use std::path::Path;

use crate::terminal::StyledLine;

/// One detected URL or existing file path inside a sealed block.
///
/// `row` indexes the block's unified row space (command label rows
/// first, snapshot rows after). `col_start`/`col_end` are inclusive
/// char-column offsets within that row. `url` is the canonical
/// string the OS opener gets — a URL verbatim, or a resolved
/// absolute path stringified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLinkSpan {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
    pub url: String,
}

impl BlockLinkSpan {
    /// True when `(row, col)` falls inside this span.
    pub fn contains(&self, row: usize, col: usize) -> bool {
        row == self.row && col >= self.col_start && col <= self.col_end
    }
}

/// Scan a sealed block (`command` + `snapshot`) for URLs and
/// existing file paths. `cwd` / `home` resolve relative path
/// tokens the same way the live-grid scanner does. The injectable
/// `is_existing` closure mirrors [`crate::paths::scan_visible_paths_with`]
/// so tests don't need a tempdir.
pub fn scan_block_links_with<F>(
    command: &str,
    snapshot: &[StyledLine],
    cwd: Option<&Path>,
    home: Option<&Path>,
    is_existing: F,
) -> Vec<BlockLinkSpan>
where
    F: Fn(&Path) -> bool,
{
    let mut out = Vec::new();
    let cmd_lines: Vec<&str> =
        if command.is_empty() { Vec::new() } else { command.split('\n').collect() };

    for (i, line) in cmd_lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        push_row_links(&chars, i, cwd, home, &is_existing, &mut out);
    }

    let row_offset = cmd_lines.len();
    for (i, sl) in snapshot.iter().enumerate() {
        let chars: Vec<char> = sl.cells.iter().map(|c| c.c).collect();
        push_row_links(&chars, row_offset + i, cwd, home, &is_existing, &mut out);
    }

    out
}

/// Production wrapper that uses `Path::exists` for the path check,
/// matching [`crate::paths::scan_visible_paths`].
pub fn scan_block_links(
    command: &str,
    snapshot: &[StyledLine],
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Vec<BlockLinkSpan> {
    scan_block_links_with(command, snapshot, cwd, home, |p| p.exists())
}

/// True when *any* link in `links` covers `(row, col)`.
pub fn link_at(links: &[BlockLinkSpan], row: usize, col: usize) -> Option<&BlockLinkSpan> {
    links.iter().find(|l| l.contains(row, col))
}

fn push_row_links<F>(
    chars: &[char],
    row: usize,
    cwd: Option<&Path>,
    home: Option<&Path>,
    is_existing: &F,
    out: &mut Vec<BlockLinkSpan>,
) where
    F: Fn(&Path) -> bool,
{
    for (col_start, col_end, url) in crate::links::find_urls_in_chars(chars) {
        out.push(BlockLinkSpan { row, col_start, col_end, url });
    }
    for (col_start, col_end, resolved) in
        crate::paths::find_paths_in_chars(chars, cwd, home, is_existing)
    {
        // URL scan above wins on ties: skip path candidates whose
        // columns overlap an already-recorded URL on this row.
        let overlaps_url =
            out.iter().any(|l| l.row == row && !(col_end < l.col_start || col_start > l.col_end));
        if overlaps_url {
            continue;
        }
        out.push(BlockLinkSpan {
            row,
            col_start,
            col_end,
            url: resolved.to_string_lossy().into_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::StyledCell;
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

    fn never_exists(_: &Path) -> bool {
        false
    }

    #[test]
    fn scans_url_in_snapshot_row() {
        let snapshot = snap(&["see https://example.com for details"]);
        let links = scan_block_links_with("", &snapshot, None, None, never_exists);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].row, 0);
        assert_eq!(links[0].col_start, 4);
        assert_eq!(links[0].url, "https://example.com");
    }

    #[test]
    fn scans_url_in_command_label() {
        let snapshot = snap(&["ok"]);
        let links = scan_block_links_with(
            "curl https://api.example.com",
            &snapshot,
            None,
            None,
            never_exists,
        );
        // Row 0 is the command label; URL starts after "curl " (5 chars).
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].row, 0);
        assert_eq!(links[0].col_start, 5);
        assert_eq!(links[0].url, "https://api.example.com");
    }

    #[test]
    fn command_rows_come_before_snapshot_rows() {
        // Multi-line command + snapshot: command label rows 0..2,
        // snapshot rows 2..3.
        let snapshot = snap(&["output https://x.test/ here"]);
        let links = scan_block_links_with(
            "echo a\necho https://b.test/",
            &snapshot,
            None,
            None,
            never_exists,
        );
        let rows: Vec<usize> = links.iter().map(|l| l.row).collect();
        // Row 1 = command-line "echo https://b.test/"; row 2 =
        // snapshot row 0.
        assert!(rows.contains(&1));
        assert!(rows.contains(&2));
    }

    #[test]
    fn paths_detected_in_snapshot_with_injected_existence_check() {
        let snapshot = snap(&["build/output.txt complete"]);
        let want = Path::new("/work/build/output.txt");
        let cwd = Path::new("/work");
        let links = scan_block_links_with("", &snapshot, Some(cwd), None, |p| p == want);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "/work/build/output.txt");
    }

    #[test]
    fn url_scan_wins_over_overlapping_path() {
        // A URL on the row should not also be recorded as a path
        // candidate. (Paths scanner already skips URL-scheme tokens,
        // but the de-dup here is belt-and-braces.)
        let snapshot = snap(&["https://example.com"]);
        let links = scan_block_links_with("", &snapshot, None, None, never_exists);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://example.com");
    }

    #[test]
    fn empty_command_and_snapshot_yield_no_links() {
        let links = scan_block_links_with("", &[], None, None, never_exists);
        assert!(links.is_empty());
    }

    #[test]
    fn link_at_finds_match_inside_span() {
        let links =
            vec![BlockLinkSpan { row: 2, col_start: 5, col_end: 19, url: "https://x.test".into() }];
        assert!(link_at(&links, 2, 5).is_some());
        assert!(link_at(&links, 2, 19).is_some());
        assert!(link_at(&links, 2, 4).is_none());
        assert!(link_at(&links, 2, 20).is_none());
        assert!(link_at(&links, 1, 5).is_none());
    }
}
