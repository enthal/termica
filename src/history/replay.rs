//! Replay shell-history files into the `runs` table on app start.
//!
//! Wired by `TermicaApp::new` against the on-disk `HistoryStore`;
//! tests inject an in-memory store and a tempdir-backed
//! [`ReplayPaths`] so nothing touches the user's real shell files.
//!
//! Idempotency: backed by the partial UNIQUE INDEX added in
//! [`super::db`] v2 plus `INSERT OR IGNORE` in [`super::db::HistoryStore::record_replayed`].
//! Re-running this on the same files is a no-op for entries that
//! were inserted by a previous run; new entries appended to the
//! file since are picked up.
//!
//! Timestamp policy for entries without one: bash without
//! `HISTTIMEFORMAT` writes no timestamps, so for those entries
//! we synthesize `started_at_ms = -<file_position>`. Negative
//! values keep them sortable below real timestamps (which are
//! always positive unix-ms) while remaining stable across replays
//! of an append-only file. If the file is truncated or rewritten,
//! positions shift and some duplication is possible — accepted
//! as a v1 quirk; the UI's `GROUP BY text` dedup hides it from
//! the user.

use std::path::PathBuf;

use crate::history::HistoryStore;
use crate::history::shell_files::{parse_bash_history, parse_fish_history, parse_zsh_history};

/// Where on disk to look for each shell's history file. The
/// constructor [`Self::from_home`] fills in the conventional
/// defaults; tests construct one explicitly to point at a tempdir.
#[derive(Debug, Clone)]
pub struct ReplayPaths {
    pub zsh: Option<PathBuf>,
    pub bash: Option<PathBuf>,
    pub fish: Option<PathBuf>,
}

impl ReplayPaths {
    /// Conventional defaults: `$HOME/.zsh_history`,
    /// `$HOME/.bash_history`, and `${XDG_DATA_HOME:-$HOME/.local/share}/fish/fish_history`.
    /// Honors `$HISTFILE` for zsh and bash if set.
    pub fn from_home(home: &std::path::Path) -> Self {
        let xdg_data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        let histfile = std::env::var_os("HISTFILE").map(PathBuf::from);
        Self {
            zsh: Some(histfile.clone().unwrap_or_else(|| home.join(".zsh_history"))),
            bash: Some(histfile.unwrap_or_else(|| home.join(".bash_history"))),
            fish: Some(xdg_data.join("fish/fish_history")),
        }
    }
}

/// Summary of one replay pass. Diagnostic-only — printed via the
/// event recorder if `TERMICA_DUMP_EVENTS` is set, otherwise
/// discarded.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplayStats {
    pub zsh_inserted: usize,
    pub bash_inserted: usize,
    pub fish_inserted: usize,
    /// Files we tried to read but could not (missing, permission
    /// denied, …). Not an error — the user may not use that shell.
    pub files_skipped: Vec<PathBuf>,
}

impl ReplayStats {
    pub fn total_inserted(&self) -> usize {
        self.zsh_inserted + self.bash_inserted + self.fish_inserted
    }
}

/// Replay every shell-history file present in `paths` into
/// `store`. Idempotent: replaying the same file twice inserts each
/// row at most once.
pub fn replay_into(store: &HistoryStore, paths: &ReplayPaths) -> ReplayStats {
    let mut stats = ReplayStats::default();
    if let Some(p) = &paths.zsh {
        stats.zsh_inserted = replay_one(store, p, "zsh", parse_zsh_history, &mut stats);
    }
    if let Some(p) = &paths.bash {
        stats.bash_inserted = replay_one(store, p, "bash", parse_bash_history, &mut stats);
    }
    if let Some(p) = &paths.fish {
        stats.fish_inserted = replay_one(store, p, "fish", parse_fish_history, &mut stats);
    }
    stats
}

fn replay_one(
    store: &HistoryStore,
    path: &std::path::Path,
    source: &str,
    parser: fn(&[u8]) -> Vec<crate::history::shell_files::ParsedEntry>,
    stats: &mut ReplayStats,
) -> usize {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            stats.files_skipped.push(path.to_path_buf());
            return 0;
        }
    };
    let entries = parser(&bytes);
    let mut inserted = 0usize;
    for (i, e) in entries.iter().enumerate() {
        // For entries without a timestamp, synthesize a stable
        // negative value derived from file position. See the
        // module-level note.
        let ts = e.started_at_ms.unwrap_or(-(i as i64 + 1));
        match store.record_replayed(&e.text, ts, source) {
            Ok(Some(_)) => inserted += 1,
            Ok(None) => {} // already present
            Err(_) => {}   // soft-fail; one bad row shouldn't abort the whole replay
        }
    }
    inserted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Scope;

    fn store() -> HistoryStore {
        HistoryStore::in_memory().unwrap()
    }

    fn write(path: &std::path::Path, bytes: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn replay_inserts_zsh_entries_into_runs() {
        let s = store();
        let tmp = tempfile::tempdir().unwrap();
        let zsh = tmp.path().join(".zsh_history");
        write(&zsh, b": 1700000000:0;ls\n: 1700000005:0;cd ..\n");
        let stats = replay_into(&s, &ReplayPaths { zsh: Some(zsh), bash: None, fish: None });
        assert_eq!(stats.zsh_inserted, 2);
        let rows = s.recent(&Scope::Global, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.source == "zsh"));
    }

    #[test]
    fn replay_is_idempotent_on_re_run() {
        let s = store();
        let tmp = tempfile::tempdir().unwrap();
        let zsh = tmp.path().join(".zsh_history");
        write(&zsh, b": 1700000000:0;ls\n: 1700000005:0;cd ..\n");
        let paths = ReplayPaths { zsh: Some(zsh), bash: None, fish: None };

        let first = replay_into(&s, &paths);
        assert_eq!(first.zsh_inserted, 2);

        let second = replay_into(&s, &paths);
        assert_eq!(second.zsh_inserted, 0, "re-running inserts nothing new");

        let rows = s.recent(&Scope::Global, 10).unwrap();
        assert_eq!(rows.len(), 2, "no duplicate rows");
    }

    #[test]
    fn replay_picks_up_newly_appended_entries() {
        let s = store();
        let tmp = tempfile::tempdir().unwrap();
        let zsh = tmp.path().join(".zsh_history");
        write(&zsh, b": 1700000000:0;ls\n");
        let paths = ReplayPaths { zsh: Some(zsh.clone()), bash: None, fish: None };

        let first = replay_into(&s, &paths);
        assert_eq!(first.zsh_inserted, 1);

        // Append a new entry as a fresh shell session would.
        let mut existing = std::fs::read(&zsh).unwrap();
        existing.extend_from_slice(b": 1700000010:0;echo new\n");
        std::fs::write(&zsh, &existing).unwrap();

        let second = replay_into(&s, &paths);
        assert_eq!(second.zsh_inserted, 1, "only the new entry is inserted");
        let rows = s.recent(&Scope::Global, 10).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn replay_handles_bash_without_timestamps_via_position() {
        let s = store();
        let tmp = tempfile::tempdir().unwrap();
        let bash = tmp.path().join(".bash_history");
        write(&bash, b"ls\ncd ..\necho hi\n");
        let stats =
            replay_into(&s, &ReplayPaths { zsh: None, bash: Some(bash.clone()), fish: None });
        assert_eq!(stats.bash_inserted, 3);

        // Re-run produces no new rows even though no real
        // timestamps are involved.
        let second = replay_into(&s, &ReplayPaths { zsh: None, bash: Some(bash), fish: None });
        assert_eq!(second.bash_inserted, 0);
        assert_eq!(s.recent(&Scope::Global, 10).unwrap().len(), 3);
    }

    #[test]
    fn replay_separates_sources_for_same_text() {
        // The same `ls` line in zsh AND bash at the same timestamp
        // produces TWO rows (different source), not one. The unique
        // index is (source, text, started_at), not (text, started_at).
        let s = store();
        let tmp = tempfile::tempdir().unwrap();
        let zsh = tmp.path().join(".zsh_history");
        let bash = tmp.path().join(".bash_history");
        write(&zsh, b": 1700000000:0;ls\n");
        write(&bash, b"#1700000000\nls\n");
        let stats = replay_into(&s, &ReplayPaths { zsh: Some(zsh), bash: Some(bash), fish: None });
        assert_eq!(stats.zsh_inserted, 1);
        assert_eq!(stats.bash_inserted, 1);
        assert_eq!(s.recent(&Scope::Global, 10).unwrap().len(), 2);
    }

    #[test]
    fn replay_skips_missing_files_without_erroring() {
        let s = store();
        let tmp = tempfile::tempdir().unwrap();
        let zsh = tmp.path().join("does-not-exist");
        let stats =
            replay_into(&s, &ReplayPaths { zsh: Some(zsh.clone()), bash: None, fish: None });
        assert_eq!(stats.zsh_inserted, 0);
        assert_eq!(stats.files_skipped, vec![zsh]);
    }

    #[test]
    fn replay_all_three_shells_in_one_pass() {
        let s = store();
        let tmp = tempfile::tempdir().unwrap();
        let zsh = tmp.path().join(".zsh_history");
        let bash = tmp.path().join(".bash_history");
        let fish = tmp.path().join("fish/fish_history");
        write(&zsh, b": 1700000000:0;zsh-cmd\n");
        write(&bash, b"#1700000000\nbash-cmd\n");
        write(&fish, b"- cmd: fish-cmd\n  when: 1700000000\n");
        let stats =
            replay_into(&s, &ReplayPaths { zsh: Some(zsh), bash: Some(bash), fish: Some(fish) });
        assert_eq!(stats.zsh_inserted, 1);
        assert_eq!(stats.bash_inserted, 1);
        assert_eq!(stats.fish_inserted, 1);
        assert_eq!(stats.total_inserted(), 3);
    }
}
