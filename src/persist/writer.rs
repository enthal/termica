//! The per-pane background scrollback writer (Phase 9D-ii).
//!
//! When a block seals, [`crate::block::BlockStack::observe_lifecycle_event`]
//! returns the block's [`SealedSnapshot`] (logical lines, already
//! un-wrapped). The pane forwards it here, to a background thread that
//! encodes it into a chunk file (atomic temp-then-rename, zstd) and
//! inserts the matching `scrollback_chunk` index row — off the UI
//! thread, mirroring the git/gh probe pattern ([`crate::git_probe`]).
//!
//! **A chunk is one sealed block.** The writer keeps a per-pane running
//! cursor over the pane's cumulative *logical* lines, so chunk N covers
//! `[cursor, cursor + N_lines)` and the index is stable across resize
//! (logical lines are width-independent). Crash durability is therefore
//! per-finished-command: a command's output becomes durable when it
//! seals; the in-flight running block is not yet persisted (a future
//! `current.chunk` path can tighten that — see spec/08 §Consistency).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::block::SealedSnapshot;
use crate::history::HistoryStore;
use crate::persist::chunk::encode_chunk;
use crate::persist::store::{PaneRowId, PersistError, SessionId};

/// Handle owned by `PaneSession`. Dropping it drops the channel
/// `Sender`, the worker's `recv()` returns `Err`, and the thread exits
/// — RAII teardown, no join needed (same as the probe threads).
pub struct ChunkWriter {
    tx: mpsc::Sender<SealedSnapshot>,
    _worker: JoinHandle<()>,
}

impl ChunkWriter {
    /// Spawn the writer thread for one session. `dir` is the session's
    /// `…/scrollback/session-<id>/pane-<id>/` directory (created by
    /// [`crate::persist::store::Persistence::begin_session`]); `store`
    /// is the shared `termica.sqlite` handle.
    pub fn spawn(
        dir: PathBuf,
        store: Arc<Mutex<HistoryStore>>,
        session: SessionId,
        pane_row: PaneRowId,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<SealedSnapshot>();
        let worker = thread::spawn(move || run_worker(dir, store, session, pane_row, rx));
        Self { tx, _worker: worker }
    }

    /// Queue a sealed block for persistence. Best-effort: if the worker
    /// has already exited (pane tearing down), the send is dropped — the
    /// block stays in memory and is simply not persisted, never a crash.
    pub fn submit(&self, snapshot: SealedSnapshot) {
        let _ = self.tx.send(snapshot);
    }

    /// Drop the sender and join the worker, so every queued snapshot is
    /// flushed before the assertions run. Test-only: production teardown
    /// is the non-blocking RAII drop above.
    #[cfg(test)]
    pub(crate) fn join_and_finish(self) {
        let ChunkWriter { tx, _worker } = self;
        drop(tx);
        let _ = _worker.join();
    }
}

/// The worker loop: own the per-session cursor state (next sequence
/// number, cumulative logical-line offset) and persist each snapshot in
/// arrival order. Exits when the channel closes (pane dropped).
fn run_worker(
    dir: PathBuf,
    store: Arc<Mutex<HistoryStore>>,
    session: SessionId,
    pane_row: PaneRowId,
    rx: mpsc::Receiver<SealedSnapshot>,
) {
    // Defensive: the directory was created at begin_session, but a
    // racing cleanup or a fresh recovery path may have removed it.
    let _ = std::fs::create_dir_all(&dir);
    let mut next_seq: u64 = 1;
    let mut cumulative_line: u64 = 0;
    while let Ok(snapshot) = rx.recv() {
        match write_chunk(&dir, &store, session, pane_row, next_seq, cumulative_line, &snapshot) {
            Ok(lines_written) if lines_written > 0 => {
                cumulative_line += lines_written;
                next_seq += 1;
            }
            // Empty snapshot (a command with no output): nothing written,
            // cursor and sequence unchanged.
            Ok(_) => {}
            Err(e) => eprintln!("termica: scrollback chunk write failed: {e}"),
        }
    }
}

/// Persist one sealed block: encode → atomic write → index row. Returns
/// the number of logical lines written (0 = empty snapshot, skipped:
/// no file, no row). Synchronous and pure of threads, so the encode /
/// path / range / cursor logic is unit-testable without timing.
///
/// `start_line` is the pane's cumulative logical-line offset for this
/// chunk; the row records `[start_line, start_line + lines)`.
pub fn write_chunk(
    dir: &Path,
    store: &Arc<Mutex<HistoryStore>>,
    session: SessionId,
    pane_row: PaneRowId,
    seq: u64,
    start_line: u64,
    snapshot: &SealedSnapshot,
) -> Result<u64, PersistError> {
    let n_lines = snapshot.lines.len() as u64;
    if n_lines == 0 {
        return Ok(0);
    }
    let end_line = start_line + n_lines;

    // Sealed chunks are always compressed (off the UI thread, so the
    // cost is hidden); live chunks would not be, but we only ever write
    // sealed blocks here.
    let bytes = encode_chunk(&snapshot.lines, true);

    // Atomic publish: write a dotfile temp, then rename onto the final
    // name. A crash leaves either no file or the complete chunk, never a
    // torn one. The temp shares the directory so the rename is on one
    // filesystem.
    let file_name = format!("{seq:08}.chunk.zst");
    let final_path = dir.join(&file_name);
    let tmp_path = dir.join(format!(".{seq:08}.chunk.zst.tmp"));
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &final_path)?;

    // The stored path is RELATIVE to the data dir, so the index survives
    // the data dir being moved; restore resolves it against the root.
    let rel_path = format!("scrollback/session-{}/pane-{}/{}", session.0, pane_row.0, file_name);

    let store = store.lock().map_err(|_| PersistError::Lock)?;
    store.insert_scrollback_chunk(
        session.0,
        pane_row.0,
        &rel_path,
        start_line as i64,
        end_line as i64,
        snapshot.emit_cols as i64,
        snapshot.start_time_ms,
        snapshot.end_time_ms,
        true,
        bytes.len() as i64,
    )?;
    Ok(n_lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockId;
    use crate::persist::chunk::decode_chunk;
    use crate::persist::store::Persistence;
    use crate::terminal::{StyledCell, StyledLine};

    fn line(s: &str) -> StyledLine {
        use alacritty_terminal::term::cell::Flags;
        use alacritty_terminal::vte::ansi::Color;
        StyledLine {
            cells: s
                .chars()
                .map(|c| StyledCell {
                    c,
                    fg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
                    bg: Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
                    flags: Flags::empty(),
                })
                .collect(),
        }
    }

    fn snapshot(id: u64, lines: &[&str]) -> SealedSnapshot {
        SealedSnapshot {
            block_id: BlockId(id),
            lines: lines.iter().map(|s| line(s)).collect(),
            emit_cols: 80,
            start_time_ms: 1000,
            end_time_ms: 2000,
        }
    }

    /// `(tempdir, persistence, session record)` — a real on-disk DB +
    /// scrollback directory the writer can publish into.
    fn fixture() -> (tempfile::TempDir, Persistence, crate::persist::store::SessionRecord) {
        let tmp = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(&tmp.path().join("termica.sqlite")).unwrap();
        let persist = Persistence::new(tmp.path().to_path_buf(), Arc::new(Mutex::new(store)));
        let rec = persist.begin_session(Some("/work"), "zsh", 1000).unwrap();
        (tmp, persist, rec)
    }

    #[test]
    fn write_chunk_creates_file_and_row_and_round_trips() {
        let (_tmp, persist, rec) = fixture();
        let snap = snapshot(1, &["hello world", "second line"]);
        let store = persist.store_handle();

        let n = write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 1, 0, &snap).unwrap();
        assert_eq!(n, 2);

        // File exists and decodes back to the exact logical lines.
        let file = rec.dir.join("00000001.chunk.zst");
        assert!(file.is_file(), "chunk file published at the sequenced name");
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(decode_chunk(&bytes).unwrap(), snap.lines, "round-trips through the file");
        // No temp left behind.
        assert!(!rec.dir.join(".00000001.chunk.zst.tmp").exists());

        // Index row matches.
        let s = store.lock().unwrap();
        let (path, start, end, cols, compressed, byte_size): (String, i64, i64, i64, i64, i64) = s
            .conn()
            .query_row(
                "SELECT path, start_line, end_line, emit_cols, compressed, byte_size
                 FROM scrollback_chunk WHERE session_id = ?1",
                [rec.session.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(
            path,
            format!(
                "scrollback/session-{}/pane-{}/00000001.chunk.zst",
                rec.session.0, rec.pane_row.0
            )
        );
        assert_eq!((start, end), (0, 2), "logical-line range [0, 2)");
        assert_eq!(cols, 80);
        assert_eq!(compressed, 1);
        assert_eq!(byte_size, bytes.len() as i64);
    }

    #[test]
    fn write_chunk_skips_empty_snapshot() {
        let (_tmp, persist, rec) = fixture();
        let store = persist.store_handle();
        let n = write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 1, 0, &snapshot(1, &[]))
            .unwrap();
        assert_eq!(n, 0, "empty snapshot writes nothing");
        assert!(!rec.dir.join("00000001.chunk.zst").exists());
        let count: i64 = store
            .lock()
            .unwrap()
            .conn()
            .query_row("SELECT COUNT(*) FROM scrollback_chunk", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "no index row for an empty block");
    }

    #[test]
    fn writer_thread_sequences_chunks_and_advances_logical_cursor() {
        let (_tmp, persist, rec) = fixture();
        let store = persist.store_handle();
        let writer = ChunkWriter::spawn(rec.dir.clone(), store.clone(), rec.session, rec.pane_row);
        // Three blocks: 2 lines, 0 lines (skipped), 3 lines.
        writer.submit(snapshot(1, &["a", "b"]));
        writer.submit(snapshot(2, &[]));
        writer.submit(snapshot(3, &["c", "d", "e"]));
        writer.join_and_finish();

        // Two files (the empty one is skipped), sequenced 1 and 2.
        assert!(rec.dir.join("00000001.chunk.zst").is_file());
        assert!(rec.dir.join("00000002.chunk.zst").is_file());
        assert!(!rec.dir.join("00000003.chunk.zst").exists());

        // Rows: the cursor advances past the skipped empty block, so the
        // second chunk starts where the first ended (no gap from the
        // empty one).
        let s = store.lock().unwrap();
        let mut stmt = s
            .conn()
            .prepare("SELECT start_line, end_line FROM scrollback_chunk ORDER BY start_line")
            .unwrap();
        let ranges: Vec<(i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ranges, vec![(0, 2), (2, 5)], "contiguous logical-line ranges, empty skipped");
    }
}
