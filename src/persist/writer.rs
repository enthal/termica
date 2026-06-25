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

/// A unit of work for the writer thread. Both arrive on the same channel
/// so they are serialized in submission order — a `Clear` queued after a
/// `Seal` is applied after that block is written (and so deletes it),
/// while a block sealed *after* a clear is written fresh and kept.
enum WriterMsg {
    /// Persist one sealed block.
    Seal(SealedSnapshot),
    /// Discard the pane's entire persisted transcript (Cmd+K / close) and
    /// reset the writer's cursor, so subsequent blocks start a fresh
    /// transcript. The pane's `runs` history is untouched.
    Clear,
    /// Record the pane's current cwd to its durable `pane` row. Sent on
    /// every cwd change so a restored pane lands in the dir it was LAST in
    /// — durable on change, never deferred to quit (the process can
    /// vanish). `None` means "cwd unknown".
    SetCwd(Option<String>),
}

/// Handle owned by `PaneSession`. Dropping it drops the channel
/// `Sender`, the worker's `recv()` returns `Err`, and the thread exits
/// — RAII teardown, no join needed (same as the probe threads).
pub struct ChunkWriter {
    tx: mpsc::Sender<WriterMsg>,
    _worker: JoinHandle<()>,
}

impl ChunkWriter {
    /// Spawn the writer thread for one session. `dir` is the session's
    /// `…/scrollback/session-<id>/pane-<id>/` directory (created by
    /// [`crate::persist::store::Persistence::begin_session`]); `root` is
    /// the data-dir root that chunk paths are relative to (needed to
    /// delete a pane's chunks across *all* its session dirs on `Clear`);
    /// `store` is the shared `termica.sqlite` handle.
    /// `start_line` is the writer's initial cumulative logical-line
    /// cursor — `0` for a fresh pane, the pane's current max `end_line`
    /// for a resumed one (restart), so chunks accumulate across restarts
    /// without overlapping line ranges.
    pub fn spawn(
        root: PathBuf,
        dir: PathBuf,
        store: Arc<Mutex<HistoryStore>>,
        session: SessionId,
        pane_row: PaneRowId,
        start_line: u64,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<WriterMsg>();
        let worker =
            thread::spawn(move || run_worker(root, dir, store, session, pane_row, start_line, rx));
        Self { tx, _worker: worker }
    }

    /// Queue a sealed block for persistence. Best-effort: if the worker
    /// has already exited (pane tearing down), the send is dropped — the
    /// block stays in memory and is simply not persisted, never a crash.
    pub fn submit(&self, snapshot: SealedSnapshot) {
        let _ = self.tx.send(WriterMsg::Seal(snapshot));
    }

    /// Queue a transcript discard (Cmd+K / close): the worker deletes
    /// every chunk file + index row + chips for this pane and resets its
    /// cursor. Serialized behind any already-queued `Seal`s. Best-effort,
    /// like [`Self::submit`].
    pub fn clear(&self) {
        let _ = self.tx.send(WriterMsg::Clear);
    }

    /// Record the pane's current cwd to its durable `pane` row (off the UI
    /// thread). Sent on every cwd change so a restored pane resumes in the
    /// dir it was last in. Best-effort, like [`Self::submit`].
    pub fn set_cwd(&self, cwd: Option<String>) {
        let _ = self.tx.send(WriterMsg::SetCwd(cwd));
    }

    /// Drain and join the worker: drop the sender so the worker's
    /// `recv()` returns `Err` once the queue empties, then block until it
    /// has applied **every** queued message (pending `Seal`s AND a
    /// `Clear`). This is the teardown flush (spec/08 §Teardown): an
    /// explicit Cmd-K / close `Clear` is async, so without draining it
    /// here a quit immediately after Cmd-K would exit before the delete
    /// lands and the "cleared" transcript would resurrect on next launch.
    pub fn finish(self) {
        let ChunkWriter { tx, _worker } = self;
        drop(tx);
        let _ = _worker.join();
    }
}

/// The worker loop: own the per-session cursor state (next sequence
/// number, cumulative logical-line offset) and apply each message in
/// arrival order. Exits when the channel closes (pane dropped).
fn run_worker(
    root: PathBuf,
    dir: PathBuf,
    store: Arc<Mutex<HistoryStore>>,
    session: SessionId,
    pane_row: PaneRowId,
    start_line: u64,
    rx: mpsc::Receiver<WriterMsg>,
) {
    // Defensive: the directory was created at begin_session, but a
    // racing cleanup or a fresh recovery path may have removed it.
    let _ = std::fs::create_dir_all(&dir);
    // `next_seq` is per session directory (file naming, fresh each
    // session); `cumulative_line` is per pane (continues across restarts).
    let mut next_seq: u64 = 1;
    let mut cumulative_line: u64 = start_line;
    while let Ok(msg) = rx.recv() {
        match msg {
            WriterMsg::Seal(snapshot) => {
                match write_chunk(
                    &dir,
                    &store,
                    session,
                    pane_row,
                    next_seq,
                    cumulative_line,
                    &snapshot,
                ) {
                    Ok(lines_written) => {
                        // Always advance the file sequence (a chunk was
                        // written, even a zero-line one); advance the
                        // logical cursor by the lines this chunk added (0
                        // for a no-output block).
                        cumulative_line += lines_written;
                        next_seq += 1;
                    }
                    Err(e) => eprintln!("termica: scrollback chunk write failed: {e}"),
                }
            }
            WriterMsg::Clear => {
                match clear_pane_chunks(&root, &store, pane_row) {
                    Ok(_) => {}
                    Err(e) => eprintln!("termica: scrollback clear failed: {e}"),
                }
                // The transcript is now empty: subsequent blocks start a
                // fresh sequence at logical line 0.
                next_seq = 1;
                cumulative_line = 0;
            }
            WriterMsg::SetCwd(cwd) => {
                if let Ok(s) = store.lock()
                    && let Err(e) = s.set_pane_cwd(pane_row.0, cwd.as_deref())
                {
                    eprintln!("termica: pane cwd persist failed: {e}");
                }
            }
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
    // Every sealed block is persisted, INCLUDING zero-output ones (a
    // `false`, a `cd`, an `export`): the command label + exit + chips are
    // what make them worth restoring, and a no-output block was visible
    // in the live session, so restore must match it. A zero-line chunk is
    // a tiny valid TMCK (header + empty body) with `start_line ==
    // end_line`; it carries its chips like any other.
    let n_lines = snapshot.lines.len() as u64;
    let end_line = start_line + n_lines;

    // Sealed chunks are always compressed (off the UI thread, so the
    // cost is hidden); live chunks would not be, but we only ever write
    // sealed blocks here.
    let bytes = encode_chunk(&snapshot.lines, true);

    // Atomic publish: write a dotfile temp, then rename onto the final
    // name. A crash leaves either no file or the complete chunk, never a
    // torn one. The temp shares the directory so the rename is on one
    // filesystem.
    //
    // `.chunk.tmck` names OUR container (TMCK = TerMica ChunK), not a
    // bare zstd stream — the 16-byte header sits uncompressed in front,
    // so the file is not directly `zstd -d`-able. Whether the body is
    // compressed is recorded in that header's flags and the row's
    // `compressed` column, not in the extension.
    let file_name = format!("{seq:08}.chunk.tmck");
    let final_path = dir.join(&file_name);
    let tmp_path = dir.join(format!(".{seq:08}.chunk.tmck.tmp"));
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &final_path)?;

    // The stored path is RELATIVE to the data dir, so the index survives
    // the data dir being moved; restore resolves it against the root.
    let rel_path = format!("scrollback/session-{}/pane-{}/{}", session.0, pane_row.0, file_name);

    let store = store.lock().map_err(|_| PersistError::Lock)?;
    let chunk_id = store.insert_scrollback_chunk(
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
    // Block metadata (command / exit / cwd / git) as the chunk's chips,
    // so a restored block shows its label + chips, not just output (9F).
    let chips = snapshot.meta.to_chips();
    if !chips.is_empty() {
        store.insert_chunk_chips(chunk_id, &chips)?;
    }
    Ok(n_lines)
}

/// Delete every persisted chunk for one pane — file, index row, and
/// chips — across all of the pane's sessions (chunks accumulate across
/// restarts, keyed by the durable `pane` row). Returns the count
/// removed.
///
/// This is the TRANSCRIPT half of an explicit discard (Cmd+K / close):
/// the pane's command history (`runs`) is deliberately left intact, so
/// clearing the screen — like the action it mirrors — never wipes
/// `~/.zsh_history`. Files are unlinked first, then their rows (matching
/// `gc`), so a crash mid-clear leaves a recoverable orphan file, never a
/// row pointing at a gone file. The DB lock is **not** held across the
/// filesystem unlinks.
pub fn clear_pane_chunks(
    root: &Path,
    store: &Arc<Mutex<HistoryStore>>,
    pane_row: PaneRowId,
) -> Result<usize, PersistError> {
    // Snapshot the rows under the lock, then release it before touching
    // the filesystem (don't hold the single shared DB mutex across
    // blocking unlinks).
    let rows = {
        let s = store.lock().map_err(|_| PersistError::Lock)?;
        s.pane_scrollback_chunks(pane_row.0)?
    };
    for r in &rows {
        let _ = std::fs::remove_file(root.join(&r.path));
    }
    let s = store.lock().map_err(|_| PersistError::Lock)?;
    for r in &rows {
        s.delete_scrollback_chunk(r.id)?;
    }
    Ok(rows.len())
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
            meta: crate::block::BlockMeta::default(),
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
    fn restart_resumes_the_pane_and_continues_the_logical_cursor() {
        // The 9F restart fix: a resumed session reuses the pane row and
        // continues the logical-line cursor, so a pane's chunks accumulate
        // across restarts in one contiguous, non-overlapping range.
        let (_tmp, persist, rec1) = fixture();
        let store = persist.store_handle();
        write_chunk(
            &rec1.dir,
            &store,
            rec1.session,
            rec1.pane_row,
            1,
            0,
            &snapshot(1, &["a", "b"]),
        )
        .unwrap();

        // Restart: resume the SAME pane under a new session.
        let rec2 = persist.resume_session(rec1.pane_row.0, 2000).unwrap();
        assert_eq!(rec2.pane_row.0, rec1.pane_row.0, "restart reuses the pane row");
        assert_ne!(rec2.session.0, rec1.session.0, "restart opens a new session");
        assert_eq!(rec2.start_line, 2, "cursor resumes at the pane's max end_line");

        // The resumed writer continues past the old chunks (seq restarts at
        // 1 in the new session dir; start_line continues at 2).
        write_chunk(
            &rec2.dir,
            &store,
            rec2.session,
            rec2.pane_row,
            1,
            rec2.start_line,
            &snapshot(2, &["c", "d", "e"]),
        )
        .unwrap();

        // All of the pane's chunks, across both sessions, form one
        // contiguous logical-line range.
        let rows = store.lock().unwrap().pane_scrollback_chunks(rec1.pane_row.0).unwrap();
        let ranges: Vec<(i64, i64)> = rows.iter().map(|r| (r.start_line, r.end_line)).collect();
        assert_eq!(
            ranges,
            vec![(0, 2), (2, 5)],
            "chunks accumulate without overlap across restart"
        );
    }

    #[test]
    fn write_chunk_creates_file_and_row_and_round_trips() {
        let (_tmp, persist, rec) = fixture();
        let snap = snapshot(1, &["hello world", "second line"]);
        let store = persist.store_handle();

        let n = write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 1, 0, &snap).unwrap();
        assert_eq!(n, 2);

        // File exists and decodes back to the exact logical lines.
        let file = rec.dir.join("00000001.chunk.tmck");
        assert!(file.is_file(), "chunk file published at the sequenced name");
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(decode_chunk(&bytes).unwrap(), snap.lines, "round-trips through the file");
        // No temp left behind.
        assert!(!rec.dir.join(".00000001.chunk.tmck.tmp").exists());

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
                "scrollback/session-{}/pane-{}/00000001.chunk.tmck",
                rec.session.0, rec.pane_row.0
            )
        );
        assert_eq!((start, end), (0, 2), "logical-line range [0, 2)");
        assert_eq!(cols, 80);
        assert_eq!(compressed, 1);
        assert_eq!(byte_size, bytes.len() as i64);
    }

    #[test]
    fn write_chunk_persists_empty_block_with_its_chips() {
        // A no-output command (e.g. `false`) still seals a block worth
        // restoring — its command + exit survive as chips, the chunk is a
        // tiny zero-line file, and its logical range is empty so the
        // cursor doesn't advance.
        let (_tmp, persist, rec) = fixture();
        let store = persist.store_handle();
        let mut snap = snapshot(1, &[]);
        snap.meta = crate::block::BlockMeta {
            command: "false".into(),
            exit: Some(1),
            cwd: None,
            git: None,
        };
        let n = write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 1, 4, &snap).unwrap();
        assert_eq!(n, 0, "no output lines");

        let file = rec.dir.join("00000001.chunk.tmck");
        assert!(file.is_file(), "a tiny chunk file is still written");
        assert!(
            decode_chunk(&std::fs::read(&file).unwrap()).unwrap().is_empty(),
            "decodes to zero lines"
        );

        let s = store.lock().unwrap();
        let (id, start, end): (i64, i64, i64) = s
            .conn()
            .query_row("SELECT id, start_line, end_line FROM scrollback_chunk", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((start, end), (4, 4), "empty logical range; cursor doesn't advance");
        let mut chips = s.chunk_chips(id).unwrap();
        chips.sort();
        assert_eq!(
            chips,
            vec![
                ("command".to_string(), "false".to_string()),
                ("exit".to_string(), "1".to_string())
            ]
        );
    }

    #[test]
    fn writer_clear_discards_prior_chunks_and_resets_cursor() {
        // A `Clear` queued after a `Seal` removes that block; a block
        // sealed AFTER the clear is written fresh from logical line 0.
        let (_tmp, persist, rec) = fixture();
        let store = persist.store_handle();
        let writer = ChunkWriter::spawn(
            persist.root().to_path_buf(),
            rec.dir.clone(),
            store.clone(),
            rec.session,
            rec.pane_row,
            0,
        );
        writer.submit(snapshot(1, &["before clear", "second"])); // [0,2)
        writer.clear();
        writer.submit(snapshot(2, &["after clear"])); // fresh: [0,1)
        writer.finish();

        // Only the post-clear chunk survives, and the cursor reset to 0.
        let s = store.lock().unwrap();
        let mut stmt = s
            .conn()
            .prepare("SELECT start_line, end_line FROM scrollback_chunk ORDER BY id")
            .unwrap();
        let ranges: Vec<(i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ranges, vec![(0, 1)], "pre-clear chunk gone; post-clear chunk starts at line 0");
    }

    #[test]
    fn writer_persists_pane_cwd_on_set_cwd() {
        // The pane forwards each cwd change to the writer thread (off the
        // UI thread, durable on change — not at quit). The worker writes
        // it to the durable `pane` row so a restored pane lands in the dir
        // it was LAST in.
        let (_tmp, persist, rec) = fixture();
        let store = persist.store_handle();
        // begin_session recorded the start cwd "/work".
        assert_eq!(
            store.lock().unwrap().pane_cwd(rec.pane_row.0).unwrap().as_deref(),
            Some("/work")
        );
        let writer = ChunkWriter::spawn(
            persist.root().to_path_buf(),
            rec.dir.clone(),
            store.clone(),
            rec.session,
            rec.pane_row,
            0,
        );
        writer.set_cwd(Some("/work/sub/dir".to_string()));
        writer.finish();

        assert_eq!(
            store.lock().unwrap().pane_cwd(rec.pane_row.0).unwrap().as_deref(),
            Some("/work/sub/dir"),
            "the latest cwd is persisted to the pane row"
        );
    }

    #[test]
    fn finish_applies_a_clear_queued_just_before_teardown() {
        // Regression: a Cmd+K (or close) queues a `Clear` on the writer.
        // If the process quits before the writer drains, the delete is
        // lost and the "cleared" transcript resurrects on next launch
        // (observed: clear two panes, quit, relaunch -> blocks came back).
        // `finish()` is the teardown flush — it must apply the queued
        // Clear, leaving the pane's transcript empty on disk.
        let (_tmp, persist, rec) = fixture();
        let store = persist.store_handle();
        let writer = ChunkWriter::spawn(
            persist.root().to_path_buf(),
            rec.dir.clone(),
            store.clone(),
            rec.session,
            rec.pane_row,
            0,
        );
        writer.submit(snapshot(1, &["secret output", "line two"]));
        writer.clear(); // Cmd+K
        writer.finish(); // the flush a quit must perform — no new block in between

        let s = store.lock().unwrap();
        let n: i64 = s
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM scrollback_chunk WHERE pane_id = ?1",
                [rec.pane_row.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "queued Clear applied on teardown — transcript not resurrected");
    }

    #[test]
    fn clear_pane_chunks_deletes_transcript_but_keeps_history() {
        // Cmd+K / close discards a pane's TRANSCRIPT (chunk files + index
        // rows + chips) but NOT its command history (`runs`) — like
        // clearing the screen doesn't wipe `~/.zsh_history`.
        let (_tmp, persist, rec) = fixture();
        let store = persist.store_handle();
        let mut snap = snapshot(1, &["output a", "output b"]);
        snap.meta =
            crate::block::BlockMeta { command: "ls".into(), exit: Some(0), cwd: None, git: None };
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 1, 0, &snap).unwrap();
        write_chunk(&rec.dir, &store, rec.session, rec.pane_row, 2, 2, &snapshot(2, &["more"]))
            .unwrap();
        // A command-history row for this pane — must survive the clear.
        store.lock().unwrap().record_submit("ls", None, rec.pane_row.0, "app-run", 1000).unwrap();

        let file1 = rec.dir.join("00000001.chunk.tmck");
        let file2 = rec.dir.join("00000002.chunk.tmck");
        assert!(file1.is_file() && file2.is_file());

        let deleted = clear_pane_chunks(persist.root(), &store, rec.pane_row).unwrap();
        assert_eq!(deleted, 2, "both chunks removed");

        // Files gone.
        assert!(!file1.exists() && !file2.exists(), "chunk files deleted");
        let s = store.lock().unwrap();
        // Index rows + chips gone.
        let n_chunks: i64 =
            s.conn().query_row("SELECT COUNT(*) FROM scrollback_chunk", [], |r| r.get(0)).unwrap();
        let n_chips: i64 =
            s.conn().query_row("SELECT COUNT(*) FROM chunk_chip", [], |r| r.get(0)).unwrap();
        assert_eq!((n_chunks, n_chips), (0, 0), "no chunk rows or chips remain");
        // History kept.
        assert_eq!(s.count_runs().unwrap(), 1, "command history (runs) is untouched");
    }

    #[test]
    fn writer_thread_sequences_chunks_and_advances_logical_cursor() {
        let (_tmp, persist, rec) = fixture();
        let store = persist.store_handle();
        let writer = ChunkWriter::spawn(
            persist.root().to_path_buf(),
            rec.dir.clone(),
            store.clone(),
            rec.session,
            rec.pane_row,
            0,
        );
        // Three blocks: 2 lines, 0 lines (still persisted), 3 lines.
        writer.submit(snapshot(1, &["a", "b"]));
        writer.submit(snapshot(2, &[]));
        writer.submit(snapshot(3, &["c", "d", "e"]));
        writer.finish();

        // Three files now — the zero-output block is persisted too (seq 2),
        // so its command/exit can be restored.
        assert!(rec.dir.join("00000001.chunk.tmck").is_file());
        assert!(rec.dir.join("00000002.chunk.tmck").is_file());
        assert!(rec.dir.join("00000003.chunk.tmck").is_file());

        // Ranges: [0,2), the empty [2,2), then [2,5). The cursor is
        // unbroken (the empty block has zero width).
        let s = store.lock().unwrap();
        let mut stmt = s
            .conn()
            .prepare(
                "SELECT start_line, end_line FROM scrollback_chunk ORDER BY start_line, end_line",
            )
            .unwrap();
        let ranges: Vec<(i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ranges, vec![(0, 2), (2, 2), (2, 5)], "empty block kept, cursor unbroken");
    }
}
