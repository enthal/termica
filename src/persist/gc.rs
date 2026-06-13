//! Scrollback garbage collection (Phase 9E).
//!
//! `gc()` is the conservative pass that keeps disk + the `runs` table
//! from growing without bound (spec/08 §"Bounding growth"). It runs on
//! startup (off the UI thread) and never deletes a chunk a **live**
//! session could still re-page — liveness is decided by *trying to take
//! the session's ownership lock* (9F): if the lock is free, the session
//! is orphaned and its chunks are gc-eligible; if it's held (by this or
//! another process), the session is live and skipped.
//!
//! Deletions, in order:
//! - **Aged** — chunks past the retention age (default 30 days), of
//!   non-live sessions.
//! - **Total cap** — if all chunk bytes exceed the global cap (default
//!   2 GB), delete the oldest non-live chunks until under it.
//! - **Per-session cap** — each non-live session over its cap (default
//!   200 MB) sheds its oldest chunks.
//! - **`runs` cap** — trim history to its newest N rows (default 50k).
//! - **Torn-write temps** — leftover `.tmp` files (a crash between write
//!   and rename) are always removed.
//!
//! Orphan *chunk* files (a file with no index row) are deliberately
//! **not** deleted here: per the consistency model the chunk-on-disk
//! wins and restore recovers the missing row, so deleting it would lose
//! recoverable data. That reconciliation is restore's job (9F).

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::history::ChunkRow;
use crate::persist::lock::SessionLock;
use crate::persist::store::{PersistError, Persistence};

pub const RETENTION_DAYS: i64 = 30;
pub const RETENTION_MS: i64 = RETENTION_DAYS * 24 * 60 * 60 * 1000;
pub const SESSION_CHUNK_CAP_BYTES: i64 = 200 * 1024 * 1024;
pub const TOTAL_SCROLLBACK_CAP_BYTES: i64 = 2 * 1024 * 1024 * 1024;
pub const RUNS_CAP: i64 = 50_000;

/// The tunable caps `gc()` enforces. [`Default`] is the spec's
/// production policy; tests pass small values to exercise the logic
/// without writing gigabytes.
#[derive(Debug, Clone, Copy)]
pub struct GcCaps {
    pub session_bytes: i64,
    pub total_bytes: i64,
    pub retention_ms: i64,
    pub runs: i64,
}

impl Default for GcCaps {
    fn default() -> Self {
        Self {
            session_bytes: SESSION_CHUNK_CAP_BYTES,
            total_bytes: TOTAL_SCROLLBACK_CAP_BYTES,
            retention_ms: RETENTION_MS,
            runs: RUNS_CAP,
        }
    }
}

/// What a `gc()` pass reclaimed. Returned for logging and asserted by
/// tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub chunks_deleted: usize,
    pub bytes_reclaimed: i64,
    pub tmp_files_deleted: usize,
    pub runs_trimmed: usize,
    pub sessions_skipped_live: usize,
}

/// Run one gc pass. `now_ms` is injected (never `SystemTime::now()`) so
/// the age policy is deterministic in tests.
pub fn gc(persist: &Persistence, now_ms: i64, caps: &GcCaps) -> Result<GcReport, PersistError> {
    let store = persist.store_handle();
    let rows = {
        let s = store.lock().map_err(|_| PersistError::Lock)?;
        s.all_scrollback_chunks()?
    };

    // Liveness per session: try to take the lock. Acquired -> orphaned
    // (release immediately by dropping the guard); held -> live; error
    // -> assume live (never delete what we can't prove is dead).
    let mut live_by_session: HashMap<i64, bool> = HashMap::new();
    for row in &rows {
        live_by_session.entry(row.session_id).or_insert_with(|| {
            match SessionLock::try_acquire(&persist.session_dir(row.session_id)) {
                Ok(Some(_guard)) => false,
                Ok(None) => true,
                Err(_) => true,
            }
        });
    }
    let is_live = |sid: i64| live_by_session.get(&sid).copied().unwrap_or(true);

    // Decide which rows die. `deleted` is the single source of truth;
    // each phase only adds to it.
    let mut deleted: HashSet<i64> = HashSet::new();

    // (a) Aged chunks of non-live sessions.
    let aged_cutoff = now_ms - caps.retention_ms;
    for r in &rows {
        if !is_live(r.session_id) && r.end_time < aged_cutoff {
            deleted.insert(r.id);
        }
    }

    // (b) Global byte cap: oldest non-live chunks first, until under.
    let live_or_deleted =
        |r: &ChunkRow, deleted: &HashSet<i64>| is_live(r.session_id) || deleted.contains(&r.id);
    let mut total: i64 =
        rows.iter().filter(|r| !deleted.contains(&r.id)).map(|r| r.byte_size).sum();
    if total > caps.total_bytes {
        let mut candidates: Vec<&ChunkRow> =
            rows.iter().filter(|r| !live_or_deleted(r, &deleted)).collect();
        candidates.sort_by_key(|r| (r.end_time, r.id));
        for r in candidates {
            if total <= caps.total_bytes {
                break;
            }
            deleted.insert(r.id);
            total -= r.byte_size;
        }
    }

    // (c) Per-session byte cap.
    let mut by_session: HashMap<i64, Vec<&ChunkRow>> = HashMap::new();
    for r in rows.iter().filter(|r| !live_or_deleted(r, &deleted)) {
        by_session.entry(r.session_id).or_default().push(r);
    }
    for (_sid, mut chunks) in by_session {
        let mut session_total: i64 = chunks.iter().map(|r| r.byte_size).sum();
        if session_total > caps.session_bytes {
            chunks.sort_by_key(|r| (r.end_time, r.id));
            for r in chunks {
                if session_total <= caps.session_bytes {
                    break;
                }
                deleted.insert(r.id);
                session_total -= r.byte_size;
            }
        }
    }

    // Apply: delete the file (missing is fine — chunk-on-disk may be gone
    // already), then the row. File first so a crash mid-gc leaves an
    // orphan file (recoverable) rather than a row pointing at nothing.
    let root = persist.root().to_path_buf();
    let mut report = GcReport::default();
    {
        let s = store.lock().map_err(|_| PersistError::Lock)?;
        for r in rows.iter().filter(|r| deleted.contains(&r.id)) {
            let _ = std::fs::remove_file(root.join(&r.path));
            s.delete_scrollback_chunk(r.id)?;
            report.chunks_deleted += 1;
            report.bytes_reclaimed += r.byte_size;
        }
        if s.count_runs()? > caps.runs {
            report.runs_trimmed = s.trim_runs_to_newest(caps.runs)?;
        }
    }

    report.tmp_files_deleted = cleanup_tmp_files(&root.join("scrollback"));
    report.sessions_skipped_live = live_by_session.values().filter(|&&live| live).count();
    Ok(report)
}

/// Remove leftover `.tmp` files (torn writes — a crash between
/// `write` and `rename`) anywhere under the scrollback tree. These are
/// never valid chunks, so they're always safe to delete.
fn cleanup_tmp_files(scrollback_root: &Path) -> usize {
    fn walk(dir: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, count);
            } else if path.extension().and_then(|e| e.to_str()) == Some("tmp")
                && std::fs::remove_file(&path).is_ok()
            {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(scrollback_root, &mut count);
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockId, SealedSnapshot};
    use crate::history::HistoryStore;
    use crate::persist::store::{Persistence, SessionRecord};
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

    fn snap(lines: &[&str], end_time_ms: i64) -> SealedSnapshot {
        SealedSnapshot {
            block_id: BlockId(1),
            lines: lines.iter().map(|s| line(s)).collect(),
            emit_cols: 80,
            start_time_ms: end_time_ms - 1,
            end_time_ms,
        }
    }

    fn persistence() -> (tempfile::TempDir, Persistence) {
        let tmp = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(&tmp.path().join("termica.sqlite")).unwrap();
        let p = Persistence::new(tmp.path().to_path_buf(), Arc::new(Mutex::new(store)));
        (tmp, p)
    }

    /// Write one chunk into a session and return its db byte_size.
    fn write_one(
        p: &Persistence,
        rec: &SessionRecord,
        seq: u64,
        start: u64,
        snap: &SealedSnapshot,
    ) {
        write_chunk(&rec.dir, &p.store_handle(), rec.session, rec.pane_row, seq, start, snap)
            .unwrap();
    }

    #[test]
    fn deletes_aged_chunks_of_dead_sessions() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(Some("/w"), "zsh", 1000).unwrap();
        write_one(&p, &rec, 1, 0, &snap(&["old output"], 1000));
        let chunk_path = rec.dir.join("00000001.chunk.tmck");
        assert!(chunk_path.is_file());
        drop(rec); // release the lock -> session is now dead/orphaned

        // 40 days later, retention is 30 days -> aged out.
        let now = 1000 + 40 * 24 * 60 * 60 * 1000;
        let report = gc(&p, now, &GcCaps::default()).unwrap();

        assert_eq!(report.chunks_deleted, 1);
        assert!(!chunk_path.exists(), "aged chunk file deleted");
        let n: i64 = p
            .store_handle()
            .lock()
            .unwrap()
            .conn()
            .query_row("SELECT COUNT(*) FROM scrollback_chunk", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "aged chunk row deleted");
    }

    #[test]
    fn never_deletes_a_live_sessions_chunks() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(Some("/w"), "zsh", 1000).unwrap();
        write_one(&p, &rec, 1, 0, &snap(&["live output"], 1000));
        // rec stays alive -> lock held -> session is LIVE.
        let now = 1000 + 999 * 24 * 60 * 60 * 1000; // ancient, but live
        let report = gc(&p, now, &GcCaps::default()).unwrap();
        assert_eq!(report.chunks_deleted, 0, "a live session is never gc'd");
        assert_eq!(report.sessions_skipped_live, 1);
        assert!(rec.dir.join("00000001.chunk.tmck").is_file());
    }

    #[test]
    fn enforces_total_byte_cap_oldest_first() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(None, "zsh", 1).unwrap();
        // Three chunks, increasing end_time. Distinct content so sizes are
        // real; oldest = end_time 10.
        write_one(&p, &rec, 1, 0, &snap(&["chunk one alpha"], 10));
        write_one(&p, &rec, 2, 1, &snap(&["chunk two bravo"], 20));
        write_one(&p, &rec, 3, 2, &snap(&["chunk three charlie"], 30));
        drop(rec);

        // Read the three byte sizes; set the total cap so the single
        // oldest chunk must be evicted to get under it.
        let sizes: Vec<(i64, i64)> = {
            let s = p.store_handle();
            let g = s.lock().unwrap();
            let mut stmt = g
                .conn()
                .prepare("SELECT end_time, byte_size FROM scrollback_chunk ORDER BY end_time")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        let total: i64 = sizes.iter().map(|(_, b)| b).sum();
        let cap = total - 1; // just over: forces eviction of exactly the oldest
        let caps = GcCaps { total_bytes: cap, retention_ms: i64::MAX, ..Default::default() };

        let report = gc(&p, 1_000_000, &caps).unwrap();
        assert_eq!(report.chunks_deleted, 1, "exactly the oldest chunk evicted");
        // The survivor end_times are 20 and 30 (oldest=10 gone).
        let survivors: Vec<i64> = {
            let s = p.store_handle();
            let g = s.lock().unwrap();
            let mut stmt = g
                .conn()
                .prepare("SELECT end_time FROM scrollback_chunk ORDER BY end_time")
                .unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        assert_eq!(survivors, vec![20, 30]);
    }

    #[test]
    fn trims_runs_to_cap_keeping_newest() {
        let (_tmp, p) = persistence();
        {
            let s = p.store_handle();
            let g = s.lock().unwrap();
            for i in 0..5 {
                g.record_submit(&format!("cmd {i}"), None, 1, "app", 100 + i).unwrap();
            }
        }
        let caps = GcCaps { runs: 2, ..Default::default() };
        let report = gc(&p, 0, &caps).unwrap();
        assert_eq!(report.runs_trimmed, 3, "5 rows trimmed to the newest 2");
        let s = p.store_handle();
        let g = s.lock().unwrap();
        assert_eq!(g.count_runs().unwrap(), 2);
        // The survivors are the two newest (started_at 103, 104).
        let newest: i64 =
            g.conn().query_row("SELECT MIN(started_at) FROM runs", [], |r| r.get(0)).unwrap();
        assert_eq!(newest, 103);
    }

    #[test]
    fn removes_torn_write_temp_files() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(None, "zsh", 1).unwrap();
        let tmp_file = rec.dir.join(".00000007.chunk.tmck.tmp");
        std::fs::write(&tmp_file, b"torn").unwrap();
        drop(rec);
        let report = gc(&p, 0, &GcCaps::default()).unwrap();
        assert_eq!(report.tmp_files_deleted, 1);
        assert!(!tmp_file.exists());
    }
}
