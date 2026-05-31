//! SQLite-backed `runs` table — Phase 4J's storage layer.
//!
//! Open / migrate / capture / query. No UI dependencies — pure
//! engine code with strict-layer tests against an in-memory
//! database (`:memory:`).

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

/// Schema version. Forward-only. Bumping this and adding the
/// `vN` arm to [`migrate`] is the contract for any schema change.
/// Read on open via `PRAGMA user_version` and compared against
/// the embedded constant; mismatching versions trigger the
/// migration ladder.
const SCHEMA_VERSION: u32 = 2;

/// One row in `runs`. `pane_id` / `app_run_id` / `cwd` are `None`
/// for entries replayed from a shell-history file (those formats
/// don't carry that metadata).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub text: String,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub cwd: Option<String>,
    pub app_run_id: Option<String>,
    pub pane_id: Option<i64>,
    /// Where the row came from: `"termica"` for captured submits,
    /// `"zsh"` / `"bash"` / `"fish"` for shell-history replays.
    pub source: String,
}

/// Query filter for recall + search.
///
/// - `Global` — every row in the table. Default for `^R`.
/// - `Pane { pane_id, app_run_id }` — only this pane in this
///   Termica run. Default for `↑` / `↓`.
#[derive(Debug, Clone)]
pub enum Scope<'a> {
    Global,
    Pane { pane_id: i64, app_run_id: &'a str },
}

/// History store handle. Wraps a `rusqlite::Connection` so the
/// rest of the codebase doesn't need to know we're using SQLite
/// specifically. Cheaply created via [`Self::open`] (file-backed)
/// or [`Self::in_memory`] (test-only).
pub struct HistoryStore {
    conn: Connection,
}

impl HistoryStore {
    /// Open the history database at `path`, applying any pending
    /// schema migrations. Creates the file (and the parent
    /// directory) if needed.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                    Some(format!("create parent dir {}: {e}", parent.display())),
                )
            })?;
        }
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Open a transient in-memory database. Used by tests so we
    /// don't litter `tmp/` and get deterministic isolation per
    /// test. Migrates immediately so the schema is ready to query.
    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Read the schema version stored in the database (0 for a
    /// fresh DB) and step through any pending migrations up to
    /// [`SCHEMA_VERSION`]. The migration set is forward-only;
    /// downgrades are not supported.
    fn migrate(&self) -> rusqlite::Result<()> {
        let current: u32 =
            self.conn.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
        for v in (current + 1)..=SCHEMA_VERSION {
            match v {
                1 => self.apply_v1()?,
                2 => self.apply_v2()?,
                _ => unreachable!("no migration for v{v}"),
            }
        }
        self.conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        Ok(())
    }

    fn apply_v2(&self) -> rusqlite::Result<()> {
        // Idempotent replay: a partial UNIQUE INDEX over the natural
        // dedup key for replayed shell-history-file rows.
        // `source = 'termica'` rows are excluded because each
        // invocation IS a distinct row (running `ls` ten times
        // produces ten captured runs). For replayed rows, the same
        // file line replayed twice MUST collapse to one row —
        // `INSERT OR IGNORE` in [`Self::record_replayed`] relies on
        // this index.
        self.conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_replay_unique
                 ON runs(source, text, started_at)
                 WHERE source != 'termica';",
        )
    }

    fn apply_v1(&self) -> rusqlite::Result<()> {
        // Single-row-per-invocation log. The UI layer dedupes by
        // text via `GROUP BY` at query time. `pane_id` is a plain
        // INTEGER (no FK yet) per the module-level note.
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS runs (
                id           INTEGER PRIMARY KEY,
                text         TEXT NOT NULL,
                started_at   INTEGER NOT NULL,
                finished_at  INTEGER,
                exit_code    INTEGER,
                cwd          TEXT,
                app_run_id   TEXT,
                pane_id      INTEGER,
                source       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_runs_started_at
                ON runs(started_at DESC);
            CREATE INDEX IF NOT EXISTS idx_runs_pane_started
                ON runs(pane_id, app_run_id, started_at DESC);
            CREATE INDEX IF NOT EXISTS idx_runs_text
                ON runs(text);
            ",
        )
    }

    /// Record a fresh submit. Returns the row id so the caller
    /// can later call [`Self::record_finish`] with it once
    /// `CommandFinished` arrives. `cwd` is the shell's current
    /// directory at submit time (`None` if unknown).
    pub fn record_submit(
        &self,
        text: &str,
        cwd: Option<&str>,
        pane_id: i64,
        app_run_id: &str,
        started_at_ms: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (text, started_at, cwd, app_run_id, pane_id, source)
             VALUES (?1, ?2, ?3, ?4, ?5, 'termica')",
            params![text, started_at_ms, cwd, app_run_id, pane_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Stamp the finish of a previously-recorded run. No-op if
    /// `id` doesn't exist (the run might have been pruned).
    pub fn record_finish(
        &self,
        id: i64,
        finished_at_ms: i64,
        exit_code: Option<i32>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE runs SET finished_at = ?1, exit_code = ?2 WHERE id = ?3",
            params![finished_at_ms, exit_code, id],
        )?;
        Ok(())
    }

    /// Record a replayed shell-history-file entry. `source` is
    /// e.g. `"zsh"`, `"bash"`, `"fish"`. `started_at_ms` is the
    /// shell-recorded timestamp when available, or any stable
    /// value when not (bash without `HISTTIMEFORMAT` has no
    /// per-entry timestamps; the replay loop substitutes a
    /// file-position-derived value).
    ///
    /// Returns the newly-inserted row id, or `None` if a row with
    /// the same `(source, text, started_at)` already existed and
    /// the insert was a no-op. The unique index that makes this
    /// idempotent lives in [`Self::apply_v2`].
    pub fn record_replayed(
        &self,
        text: &str,
        started_at_ms: i64,
        source: &str,
    ) -> rusqlite::Result<Option<i64>> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO runs (text, started_at, source) VALUES (?1, ?2, ?3)",
            params![text, started_at_ms, source],
        )?;
        Ok(if n == 0 { None } else { Some(self.conn.last_insert_rowid()) })
    }

    /// Pull the most recent `limit` rows under `scope`, newest
    /// first. Used by the `↑` / `↓` arrow recall and as the
    /// starting set the `^R` overlay narrows down.
    pub fn recent(&self, scope: &Scope<'_>, limit: usize) -> rusqlite::Result<Vec<Entry>> {
        let (sql, params): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match scope {
            Scope::Global => (
                "SELECT id, text, started_at, finished_at, exit_code, cwd, app_run_id,
                        pane_id, source
                 FROM runs
                 ORDER BY started_at DESC
                 LIMIT ?1",
                vec![Box::new(limit as i64)],
            ),
            Scope::Pane { pane_id, app_run_id } => (
                "SELECT id, text, started_at, finished_at, exit_code, cwd, app_run_id,
                        pane_id, source
                 FROM runs
                 WHERE pane_id = ?1 AND app_run_id = ?2
                 ORDER BY started_at DESC
                 LIMIT ?3",
                vec![Box::new(*pane_id), Box::new(app_run_id.to_string()), Box::new(limit as i64)],
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(param_refs), row_to_entry)?;
        rows.collect()
    }

    /// Look up an entry by id. Used in tests; trivial in
    /// production but useful for asserting `record_finish` round-
    /// trips.
    pub fn get(&self, id: i64) -> rusqlite::Result<Option<Entry>> {
        self.conn
            .query_row(
                "SELECT id, text, started_at, finished_at, exit_code, cwd, app_run_id,
                        pane_id, source
                 FROM runs WHERE id = ?1",
                params![id],
                row_to_entry,
            )
            .optional()
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: row.get(0)?,
        text: row.get(1)?,
        started_at_ms: row.get(2)?,
        finished_at_ms: row.get(3)?,
        exit_code: row.get(4)?,
        cwd: row.get(5)?,
        app_run_id: row.get(6)?,
        pane_id: row.get(7)?,
        source: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> HistoryStore {
        HistoryStore::in_memory().expect("in-memory store opens cleanly")
    }

    #[test]
    fn open_in_memory_creates_runs_table() {
        let s = store();
        // The simplest "did the migration run" probe: a select
        // against the table doesn't error.
        let count: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
            .expect("runs table exists after migrate");
        assert_eq!(count, 0);
    }

    #[test]
    fn schema_version_is_recorded_after_migrate() {
        let s = store();
        let v: u32 = s
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("user_version is readable");
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn migrate_is_idempotent_on_second_open() {
        // Apply the v1 migration twice on the same in-memory db
        // — the `IF NOT EXISTS` guards mean it must not error.
        let s = store();
        s.migrate().expect("re-migrate is a no-op");
        let v: u32 = s.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn record_submit_persists_text_and_metadata() {
        let s = store();
        let id = s
            .record_submit("cargo run", Some("/tmp/proj"), 7, "app-uuid-abc", 1_700_000_000_000)
            .expect("submit recorded");
        let got = s.get(id).expect("get").expect("row exists");
        assert_eq!(got.text, "cargo run");
        assert_eq!(got.cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(got.pane_id, Some(7));
        assert_eq!(got.app_run_id.as_deref(), Some("app-uuid-abc"));
        assert_eq!(got.started_at_ms, 1_700_000_000_000);
        assert_eq!(got.finished_at_ms, None);
        assert_eq!(got.exit_code, None);
        assert_eq!(got.source, "termica");
    }

    #[test]
    fn record_finish_updates_in_place() {
        let s = store();
        let id = s.record_submit("ls", None, 1, "app", 100).unwrap();
        s.record_finish(id, 200, Some(0)).unwrap();
        let got = s.get(id).unwrap().unwrap();
        assert_eq!(got.finished_at_ms, Some(200));
        assert_eq!(got.exit_code, Some(0));
    }

    #[test]
    fn record_finish_on_missing_id_is_a_silent_no_op() {
        let s = store();
        // No row exists; should not error.
        s.record_finish(9999, 1, Some(1)).unwrap();
    }

    #[test]
    fn record_replayed_entries_have_no_pane_or_app_run_metadata() {
        let s = store();
        let id = s.record_replayed("git status", 500, "zsh").unwrap().expect("inserted");
        let got = s.get(id).unwrap().unwrap();
        assert_eq!(got.pane_id, None);
        assert_eq!(got.app_run_id, None);
        assert_eq!(got.cwd, None);
        assert_eq!(got.source, "zsh");
    }

    #[test]
    fn record_replayed_is_idempotent_on_same_key() {
        let s = store();
        let first = s.record_replayed("ls", 100, "zsh").unwrap();
        let second = s.record_replayed("ls", 100, "zsh").unwrap();
        assert!(first.is_some(), "first insert returns the new row id");
        assert!(second.is_none(), "second insert is a no-op (no new row)");
        let entries = s.recent(&Scope::Global, 10).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn record_replayed_distinguishes_source_and_timestamp() {
        // Same text, different source OR different timestamp →
        // distinct rows. Only (source, text, started_at) collisions
        // collapse.
        let s = store();
        s.record_replayed("ls", 100, "zsh").unwrap();
        s.record_replayed("ls", 100, "bash").unwrap(); // different source
        s.record_replayed("ls", 200, "zsh").unwrap(); // different ts
        let entries = s.recent(&Scope::Global, 10).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn record_submit_is_not_dedup_constrained() {
        // Termica-captured rows must NOT collide on the unique
        // index — running `ls` ten times produces ten rows.
        let s = store();
        for _ in 0..3 {
            s.record_submit("ls", None, 1, "app", 100).unwrap();
        }
        let entries = s.recent(&Scope::Global, 10).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn recent_global_returns_newest_first_across_panes() {
        let s = store();
        s.record_submit("first", None, 1, "app", 100).unwrap();
        s.record_submit("second", None, 2, "app", 200).unwrap();
        s.record_submit("third", None, 1, "app", 300).unwrap();
        let entries = s.recent(&Scope::Global, 10).unwrap();
        let texts: Vec<_> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["third", "second", "first"]);
    }

    #[test]
    fn recent_pane_filters_by_pane_and_app_run() {
        let s = store();
        // Two panes, two different app runs — only one combo
        // matches the requested scope.
        s.record_submit("a", None, 1, "run-A", 100).unwrap();
        s.record_submit("b", None, 1, "run-B", 200).unwrap();
        s.record_submit("c", None, 2, "run-A", 300).unwrap();
        s.record_submit("d", None, 1, "run-A", 400).unwrap();

        let entries = s.recent(&Scope::Pane { pane_id: 1, app_run_id: "run-A" }, 10).unwrap();
        let texts: Vec<_> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["d", "a"]);
    }

    #[test]
    fn recent_global_respects_limit() {
        let s = store();
        for i in 0..5 {
            s.record_submit(&format!("cmd {i}"), None, 1, "app", 100 + i).unwrap();
        }
        let entries = s.recent(&Scope::Global, 3).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn open_writes_to_disk_and_persists_across_handles() {
        // Open a file-backed store, write a row, drop the
        // handle, reopen, see the row. Catches the "file-backed
        // path forgot to commit" class of bug.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("history.sqlite");
        {
            let s = HistoryStore::open(&path).expect("open creates file");
            s.record_submit("first run", None, 1, "app", 100).unwrap();
        }
        let s = HistoryStore::open(&path).expect("reopen succeeds");
        let entries = s.recent(&Scope::Global, 10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "first run");
    }

    #[test]
    fn open_creates_parent_directory_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("h.sqlite");
        assert!(!path.parent().unwrap().exists());
        let s = HistoryStore::open(&path).expect("creates nested dirs");
        // Sanity: the schema is there.
        assert!(s.get(1).unwrap().is_none());
    }
}
