//! Restore: rebuild a pane's scrollback from its persisted chunks
//! (Phase 9F). The read side of the writer — `load_chunk` is the
//! inverse of `write_chunk`.
//!
//! `restore_blocks_for_pane` is the pure data path: db pane id → its
//! chunk rows (in logical-line order) → one restored `Sealed` block per
//! chunk, with each block's command / exit / cwd / git rebuilt from the
//! chunk's `chunk_chip` rows (9F-block-meta). The app then wraps these in
//! a no-PTY `Dead` pane. Honoring the `cleared_before_line` Cmd-K
//! watermark is still layered on top later.

#![forbid(unsafe_code)]

use std::path::Path;

use crate::block::{Block, BlockId};
use crate::history::{ChunkRow, HistoryStore};
use crate::persist::chunk::{ChunkError, decode_chunk};
use crate::persist::store::Persistence;
use crate::terminal::StyledLine;

/// Why a chunk couldn't be loaded from disk.
#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Decode(ChunkError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "read chunk: {e}"),
            LoadError::Decode(e) => write!(f, "decode chunk: {e:?}"),
        }
    }
}

/// Load one chunk's logical lines from disk: read the file at
/// `<root>/<row.path>` and decode it. The inverse of `write_chunk`.
pub fn load_chunk(root: &Path, row: &ChunkRow) -> Result<Vec<StyledLine>, LoadError> {
    let bytes = std::fs::read(root.join(&row.path)).map_err(LoadError::Io)?;
    decode_chunk(&bytes).map_err(LoadError::Decode)
}

/// Build the restored `Sealed` blocks for one persisted pane, oldest
/// first — one block per chunk. A chunk whose file is missing or
/// corrupt is **skipped with a warning** (the consistency model: a row
/// pointing at a gone file degrades to a gap, never a crash); the rest
/// still restore. Returns an empty vec if the pane has no chunks.
pub fn restore_blocks_for_pane(persist: &Persistence, pane_db_id: i64) -> Vec<Block> {
    let store = persist.store_handle();
    // Pull the chunk rows and each chunk's chips up front (one lock
    // window), so the decode loop below touches only the filesystem.
    let rows_and_meta: Vec<(ChunkRow, crate::block::BlockMeta)> = match store.lock() {
        Ok(s) => s
            .pane_scrollback_chunks(pane_db_id)
            .unwrap_or_default()
            .into_iter()
            .map(|row| {
                let chips = s.chunk_chips(row.id).unwrap_or_default();
                let meta = crate::block::BlockMeta::from_chips(&chips);
                (row, meta)
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    let root = persist.root().to_path_buf();
    let mut blocks = Vec::with_capacity(rows_and_meta.len());
    for (row, meta) in rows_and_meta.iter() {
        match load_chunk(&root, row) {
            Ok(lines) => {
                // Id from the *surviving* count, not the row index: a
                // skipped chunk must not leave a gap, or the live Prompt
                // tail (id = sealed.len()) would collide with a kept
                // block. Contiguous 0..n is the BlockId contract here.
                blocks.push(Block::restored_sealed(
                    BlockId(blocks.len() as u64),
                    lines,
                    row.start_time,
                    row.end_time,
                    meta.clone(),
                ));
            }
            Err(e) => {
                eprintln!("termica: skipping unreadable scrollback chunk {}: {e}", row.path);
            }
        }
    }
    blocks
}

/// Helper used by both this module's tests and `HistoryStore` callers:
/// the pane db id is the one chunks are keyed by.
pub fn pane_has_chunks(store: &HistoryStore, pane_db_id: i64) -> bool {
    store.pane_scrollback_chunks(pane_db_id).map(|v| !v.is_empty()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockId, SealedSnapshot};
    use crate::persist::store::Persistence;
    use crate::persist::writer::write_chunk;
    use crate::terminal::{StyledCell, StyledLine};
    use std::sync::{Arc, Mutex};

    fn line(s: &str) -> StyledLine {
        use alacritty_terminal::term::cell::Flags;
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        StyledLine {
            cells: s
                .chars()
                .map(|c| StyledCell {
                    c,
                    fg: Color::Named(NamedColor::Foreground),
                    bg: Color::Named(NamedColor::Background),
                    flags: Flags::empty(),
                })
                .collect(),
        }
    }

    fn snap(lines: &[&str]) -> SealedSnapshot {
        SealedSnapshot {
            block_id: BlockId(0),
            lines: lines.iter().map(|s| line(s)).collect(),
            emit_cols: 80,
            start_time_ms: 1000,
            end_time_ms: 2000,
            meta: crate::block::BlockMeta::default(),
        }
    }

    fn persistence() -> (tempfile::TempDir, Persistence) {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::history::HistoryStore::open(&tmp.path().join("termica.sqlite")).unwrap();
        let p = Persistence::new(tmp.path().to_path_buf(), Arc::new(Mutex::new(store)));
        (tmp, p)
    }

    #[test]
    fn load_chunk_is_the_inverse_of_write_chunk() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(None, "zsh", 1).unwrap();
        let s = snap(&["alpha line", "bravo line"]);
        write_chunk(&rec.dir, &p.store_handle(), rec.session, rec.pane_row, 1, 0, &s).unwrap();

        let rows = p.store_handle().lock().unwrap().pane_scrollback_chunks(rec.pane_row.0).unwrap();
        assert_eq!(rows.len(), 1);
        let lines = load_chunk(p.root(), &rows[0]).unwrap();
        assert_eq!(lines, s.lines, "round-trips through the file");
    }

    #[test]
    fn restore_blocks_rebuilds_one_sealed_block_per_chunk_in_order() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(None, "zsh", 1).unwrap();
        let store = p.store_handle();
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 1, 0, &snap(&["first cmd out"]))
            .unwrap();
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 2, 1, &snap(&["second", "lines"]))
            .unwrap();

        let blocks = restore_blocks_for_pane(&p, rec.pane_row.0);
        assert_eq!(blocks.len(), 2, "one Sealed block per chunk");
        // Oldest first, ids 0..n, output preserved.
        match &blocks[0] {
            Block::Sealed { id, snapshot, command, exit, .. } => {
                assert_eq!(*id, BlockId(0));
                assert_eq!(snapshot.len(), 1);
                let text: String = snapshot[0].text_chars().collect();
                assert_eq!(text, "first cmd out");
                // This chunk was written with default (empty) meta.
                assert!(command.is_empty());
                assert_eq!(*exit, None);
            }
            other => panic!("expected Sealed, got {other:?}"),
        }
        match &blocks[1] {
            Block::Sealed { id, snapshot, .. } => {
                assert_eq!(*id, BlockId(1));
                assert_eq!(snapshot.len(), 2);
            }
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    #[test]
    fn restored_block_carries_command_exit_and_git_from_chips() {
        use crate::block::BlockMeta;
        use crate::git_context::{DirtySummary, GitContext};
        let (_tmp, p) = persistence();
        let rec = p.begin_session(None, "zsh", 1).unwrap();
        let git = GitContext {
            branch: Some("main".into()),
            ahead: 1,
            behind: 0,
            dirty: DirtySummary { files_changed: 2, lines_added: 9, lines_removed: 3 },
        };
        let mut s = snap(&["cargo build output"]);
        s.meta = BlockMeta {
            command: "cargo build".into(),
            exit: Some(101),
            cwd: Some("/work/proj".into()),
            git: Some(git.clone()),
        };
        write_chunk(&rec.dir, &p.store_handle(), rec.session, rec.pane_row, 1, 0, &s).unwrap();

        let blocks = restore_blocks_for_pane(&p, rec.pane_row.0);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Sealed { command, exit, header, .. } => {
                assert_eq!(command, "cargo build", "command label restores from chips");
                assert_eq!(*exit, Some(101), "exit code restores");
                assert_eq!(header.cwd.as_deref(), Some(std::path::Path::new("/work/proj")));
                assert_eq!(
                    header.git.as_ref(),
                    Some(&git),
                    "full git context restores (JSON chip)"
                );
            }
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    #[test]
    fn skipped_middle_chunk_leaves_contiguous_block_ids() {
        // A missing chunk in the *middle* must not leave a gap in the
        // assigned BlockIds: ids must be contiguous 0..n over the
        // surviving blocks. Otherwise the live Prompt tail (whose id is
        // `next_id = sealed.len()`) collides with a surviving sealed
        // block — a duplicate-BlockId, the documented critical-bug class.
        let (_tmp, p) = persistence();
        let rec = p.begin_session(None, "zsh", 1).unwrap();
        let store = p.store_handle();
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 1, 0, &snap(&["first"])).unwrap();
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 2, 1, &snap(&["middle"])).unwrap();
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 3, 2, &snap(&["last"])).unwrap();
        // Delete the *middle* chunk's file (row survives).
        std::fs::remove_file(rec.dir.join("00000002.chunk.tmck")).unwrap();

        let blocks = restore_blocks_for_pane(&p, rec.pane_row.0);
        assert_eq!(blocks.len(), 2, "the two readable chunks restore");
        let ids: Vec<u64> = blocks
            .iter()
            .map(|b| match b {
                Block::Sealed { id, .. } => id.0,
                other => panic!("expected Sealed, got {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec![0, 1], "ids are contiguous 0..n, not [0, 2]");
    }

    #[test]
    fn missing_chunk_file_is_skipped_not_fatal() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(None, "zsh", 1).unwrap();
        let store = p.store_handle();
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 1, 0, &snap(&["kept"])).unwrap();
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 2, 1, &snap(&["gone"])).unwrap();
        // Delete the second chunk's file but leave its row (a "chunk
        // missing" situation the consistency model tolerates).
        std::fs::remove_file(rec.dir.join("00000002.chunk.tmck")).unwrap();

        let blocks = restore_blocks_for_pane(&p, rec.pane_row.0);
        assert_eq!(
            blocks.len(),
            1,
            "the readable chunk still restores; the missing one is skipped"
        );
    }

    #[test]
    fn restore_blocks_empty_for_pane_with_no_chunks() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(None, "zsh", 1).unwrap();
        assert!(restore_blocks_for_pane(&p, rec.pane_row.0).is_empty());
    }

    #[test]
    fn pre_command_output_block_persists_and_restores_without_confusing_the_index() {
        // The "before first command" output-only block (empty command, no
        // chips) must persist as a normal chunk and restore as bare output
        // — and the FIRST command's chunk must continue the logical-line
        // cursor right after it, contiguous and non-overlapping. This is
        // the storage/recovery correctness the feature must not break.
        let (_tmp, p) = persistence();
        let rec = p.begin_session(None, "zsh", 1).unwrap();
        let store = p.store_handle();

        // Output-only init block at [0, 2): default meta = empty command.
        let init = snap(&["init line 1", "init line 2"]);
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 1, 0, &init).unwrap();
        // First command's block continues at line 2: [2, 3), with a command.
        let mut cmd = snap(&["hello"]);
        cmd.meta = crate::block::BlockMeta {
            command: "echo hello".into(),
            exit: Some(0),
            cwd: None,
            git: None,
        };
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 2, 2, &cmd).unwrap();

        // The chunk index is contiguous and non-overlapping.
        let rows = store.lock().unwrap().pane_scrollback_chunks(rec.pane_row.0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].start_line, rows[0].end_line), (0, 2), "init block covers [0,2)");
        assert_eq!(
            (rows[1].start_line, rows[1].end_line),
            (2, 3),
            "the command block continues right after, no gap/overlap"
        );

        // Restore: first an output-only block, then the command block.
        let blocks = restore_blocks_for_pane(&p, rec.pane_row.0);
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            Block::Sealed { id, command, exit, snapshot, .. } => {
                assert_eq!(*id, BlockId(0));
                assert!(command.is_empty(), "the init block restores with NO command label");
                assert_eq!(*exit, None, "and no exit chip");
                assert_eq!(snapshot.len(), 2, "its two init lines survive");
            }
            other => panic!("expected an output-only Sealed block first, got {other:?}"),
        }
        match &blocks[1] {
            Block::Sealed { id, command, exit, .. } => {
                assert_eq!(*id, BlockId(1), "ids stay contiguous 0..n");
                assert_eq!(command, "echo hello");
                assert_eq!(*exit, Some(0));
            }
            other => panic!("expected the command block second, got {other:?}"),
        }
    }
}
