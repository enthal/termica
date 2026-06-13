//! The persistence orchestrator (Phase 9D): allocates the durable
//! `pane` + `session` rows that anchor a shell session's scrollback,
//! and owns the on-disk directory layout under `<data-dir>/scrollback/`.
//!
//! This slice (9D-i) is the foundation the background writer thread
//! (9D-ii) builds on: a session must have its rows and its directory
//! before any chunk file can be written into it. It is pure of UI and
//! does not spawn threads — `begin_session` is a synchronous DB
//! transaction plus a `create_dir_all`, exercised directly by unit
//! tests against a file-backed DB in a tempdir.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::history::HistoryStore;

/// SQLite-assigned id of a `session` row — one PTY spawn. Unique
/// across every process sharing the database (allocated under the WAL
/// writer lock), which is what makes the `session-<id>/` directory a
/// safe per-process partition. See [spec/08 §Concurrent processes].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub i64);

/// SQLite-assigned id of a `pane` row — the durable identity of a
/// pane, distinct from the app's in-process `PaneId` counter. A pane
/// outlives any single session (restart-from-`Dead` opens a new
/// session under the same pane row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneRowId(pub i64);

/// What [`Persistence::begin_session`] hands back: the freshly
/// allocated row ids, the directory their chunk files go in, and the
/// held session-ownership lock.
///
/// Not `Clone`: it owns the [`SessionLock`], whose lifetime *is* the
/// session's ownership. The caller (the pane) must keep this alive for
/// as long as the session is live — dropping it releases ownership.
#[derive(Debug)]
pub struct SessionRecord {
    pub pane_row: PaneRowId,
    pub session: SessionId,
    /// `<data-dir>/scrollback/session-<session>/pane-<pane_row>/`,
    /// created on disk by the time this is returned.
    pub dir: PathBuf,
    /// The exclusive advisory lock on this session's directory, held so
    /// no other process adopts it while we're running ([spec/08]
    /// §"Concurrent processes"). Released on drop / process death.
    pub lock: crate::persist::lock::SessionLock,
}

/// Failure modes of a persistence operation. Kept coarse: callers
/// treat persistence as best-effort-but-loud — a failed session start
/// degrades to "no scrollback persistence for this pane", never a
/// crash (the same posture as [`crate::history`]).
#[derive(Debug)]
pub enum PersistError {
    Db(rusqlite::Error),
    Io(std::io::Error),
    /// The shared store mutex was poisoned by a panic in another
    /// thread. Unrecoverable for this operation.
    Lock,
    /// A freshly-begun session's directory was already locked — should
    /// be impossible (session ids are unique), so it signals a logic
    /// bug rather than normal contention.
    SessionLockHeld,
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Db(e) => write!(f, "persistence db error: {e}"),
            PersistError::Io(e) => write!(f, "persistence io error: {e}"),
            PersistError::Lock => write!(f, "persistence store mutex poisoned"),
            PersistError::SessionLockHeld => {
                write!(f, "new session directory was already locked (unexpected)")
            }
        }
    }
}

impl std::error::Error for PersistError {}

impl From<rusqlite::Error> for PersistError {
    fn from(e: rusqlite::Error) -> Self {
        PersistError::Db(e)
    }
}

impl From<std::io::Error> for PersistError {
    fn from(e: std::io::Error) -> Self {
        PersistError::Io(e)
    }
}

/// Owns the persistence root (`<data-dir>`) and a handle to the shared
/// SQLite store. Cheap to clone the handle out of; the store itself is
/// behind an `Arc<Mutex<…>>` because a `rusqlite::Connection` is
/// `Send` but not `Sync`.
pub struct Persistence {
    root: PathBuf,
    store: Arc<Mutex<HistoryStore>>,
}

impl Persistence {
    /// `root` is the resolved data dir (`<data-dir>`); chunk files live
    /// under `root/scrollback/…`. `store` is the shared `termica.sqlite`
    /// handle (same one the history layer writes `runs` through).
    pub fn new(root: PathBuf, store: Arc<Mutex<HistoryStore>>) -> Self {
        Self { root, store }
    }

    /// Begin a new shell session: allocate its `pane` + `session` rows
    /// (one transaction) and create its on-disk directory. Returns the
    /// ids and directory the writer thread will append chunks into.
    ///
    /// Row allocation commits before the directory is created, so a
    /// failure to `mkdir` leaves committed rows with no directory yet —
    /// harmless, since the writer (9D-ii) `create_dir_all`s defensively
    /// before its first write and an empty-directoryless session simply
    /// has no chunks.
    pub fn begin_session(
        &self,
        cwd: Option<&str>,
        shell_kind: &str,
        now_ms: i64,
    ) -> Result<SessionRecord, PersistError> {
        let (pane_id, session_id) = {
            let store = self.store.lock().map_err(|_| PersistError::Lock)?;
            store.begin_session_rows(cwd, shell_kind, now_ms)?
        };
        let dir = self.session_pane_dir(session_id, pane_id);
        std::fs::create_dir_all(&dir)?;
        // Take this session's ownership lock. The session id is freshly
        // allocated and unique, so no other process could be holding it —
        // a `None` here would mean a same-process double-begin, which we
        // surface rather than silently run unlocked.
        let lock = crate::persist::lock::SessionLock::try_acquire(&dir)?
            .ok_or(PersistError::SessionLockHeld)?;
        Ok(SessionRecord {
            pane_row: PaneRowId(pane_id),
            session: SessionId(session_id),
            dir,
            lock,
        })
    }

    /// The directory a session's chunk files live in:
    /// `<root>/scrollback/session-<session>/pane-<pane>/`. Pure path
    /// arithmetic — does not touch the filesystem.
    fn session_pane_dir(&self, session_id: i64, pane_id: i64) -> PathBuf {
        self.root
            .join("scrollback")
            .join(format!("session-{session_id}"))
            .join(format!("pane-{pane_id}"))
    }

    /// The persistence root (`<data-dir>`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A clone of the shared store handle, for the background writer to
    /// insert `scrollback_chunk` rows through.
    pub fn store_handle(&self) -> Arc<Mutex<HistoryStore>> {
        Arc::clone(&self.store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persistence() -> (tempfile::TempDir, Persistence) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store =
            HistoryStore::open(&tmp.path().join("termica.sqlite")).expect("store opens on disk");
        let p = Persistence::new(tmp.path().to_path_buf(), Arc::new(Mutex::new(store)));
        (tmp, p)
    }

    #[test]
    fn begin_session_creates_rows_and_directory() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(Some("/work/proj"), "zsh", 1_700_000_000_000).unwrap();

        // The directory exists, under the documented layout.
        assert!(rec.dir.is_dir(), "session/pane directory is created on disk");
        let expected_tail = Path::new("scrollback")
            .join(format!("session-{}", rec.session.0))
            .join(format!("pane-{}", rec.pane_row.0));
        assert!(
            rec.dir.ends_with(&expected_tail),
            "dir {:?} ends with {:?}",
            rec.dir,
            expected_tail
        );

        // Both rows exist with the id we were handed.
        let store = p.store.lock().unwrap();
        let conn = store.conn();
        let session_pane: i64 = conn
            .query_row("SELECT pane_id FROM session WHERE id = ?1", [rec.session.0], |r| r.get(0))
            .expect("session row exists");
        assert_eq!(session_pane, rec.pane_row.0, "session points at the pane we created");
    }

    #[test]
    fn pane_row_has_null_tab_and_recorded_metadata() {
        let (_tmp, p) = persistence();
        let rec = p.begin_session(Some("/work/proj"), "fish", 4242).unwrap();

        let store = p.store.lock().unwrap();
        let conn = store.conn();
        let (tab_id, cwd, shell_kind, created_at, last_open): (
            Option<i64>,
            Option<String>,
            String,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT tab_id, cwd, shell_kind, created_at, last_open FROM pane WHERE id = ?1",
                [rec.pane_row.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert!(tab_id.is_none(), "tab_id is NULL until layout is saved (9F)");
        assert_eq!(cwd.as_deref(), Some("/work/proj"));
        assert_eq!(shell_kind, "fish");
        assert_eq!(created_at, 4242);
        assert_eq!(last_open, 4242, "last_open seeds to created_at");

        // session.started_at carries the same clock.
        let started: i64 = conn
            .query_row("SELECT started_at FROM session WHERE id = ?1", [rec.session.0], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(started, 4242);
    }

    #[test]
    fn session_ids_are_unique_across_calls() {
        let (_tmp, p) = persistence();
        let a = p.begin_session(None, "zsh", 1).unwrap();
        let b = p.begin_session(None, "zsh", 2).unwrap();
        assert_ne!(a.session.0, b.session.0, "each spawn gets a fresh session id");
        assert_ne!(a.pane_row.0, b.pane_row.0, "each spawn gets a fresh pane row");
        assert_ne!(a.dir, b.dir, "distinct sessions get distinct directories");
        assert!(a.dir.is_dir() && b.dir.is_dir());
    }
}
