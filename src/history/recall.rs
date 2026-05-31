//! `↑` / `↓` history recall state machine.
//!
//! Owns the small bit of memory needed to walk a list of past
//! commands and substitute them into the editor: the user's
//! pre-walk buffer, the cached query results, and the current
//! index. Pure logic — no DB, no editor type, just `String` in /
//! `String` out — so tests cover it without spawning anything.
//!
//! The wiring at the call site (`render_pane`) is:
//!   - ArrowUp → `RecallState::step_back(query_fn(), current_text)`
//!   - ArrowDown → `RecallState::step_forward(current_text)`
//!   - Any other edit → `RecallState::abandon()` so the next walk
//!     re-queries and re-saves the buffer.

/// Outcome of one recall step. The caller substitutes `new_text`
/// into the editor (and moves the caret to the end). `Unchanged`
/// means there was no older / newer entry to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallOutcome {
    /// Editor text should now read `new_text`. The caller should
    /// reset the caret to the end of the new text.
    Replace { new_text: String },
    /// We were already at the oldest entry (Up) or returned to the
    /// pre-walk buffer (Down). Editor stays as-is.
    Unchanged,
}

/// State carried across recall steps.
#[derive(Debug, Default, Clone)]
pub struct RecallState {
    /// User's pre-walk editor text. `Some` when a walk is in
    /// progress; restored on `Down` past the most-recent entry.
    saved_buffer: Option<String>,
    /// Cached query results, newest first (index 0 = most recent).
    /// Populated by the caller via the `query_fn` argument the
    /// first time `step_back` is called in a walk.
    cached: Vec<String>,
    /// Position in `cached`. `None` between walks. `Some(0)` =
    /// most-recent entry is currently shown.
    cursor: Option<usize>,
}

impl RecallState {
    /// True if a walk is in progress (the user is currently
    /// looking at a history entry rather than their own buffer).
    pub fn is_walking(&self) -> bool {
        self.cursor.is_some()
    }

    /// Reset the walk. Call from the edit path so the next `↑`
    /// re-saves the current buffer and re-queries. Idempotent.
    pub fn abandon(&mut self) {
        self.saved_buffer = None;
        self.cached.clear();
        self.cursor = None;
    }

    /// `↑`: show the previous (older) history entry. First call in
    /// a walk saves `current_text` as the restore point and queries
    /// history via `query_fn`. Subsequent calls advance the cursor.
    ///
    /// `query_fn` is invoked only on the first step (the cache is
    /// reused thereafter), so the caller pays for at most one DB
    /// query per walk.
    pub fn step_back(
        &mut self,
        query_fn: impl FnOnce() -> Vec<String>,
        current_text: &str,
    ) -> RecallOutcome {
        if self.cursor.is_none() {
            self.saved_buffer = Some(current_text.to_string());
            self.cached = query_fn();
            // Skip entries that match the current buffer — they
            // would render as a no-op. This keeps `↑` from "doing
            // nothing" when the editor already contains the most
            // recent command.
            if self.cached.first().is_some_and(|t| t == current_text) {
                self.cached.remove(0);
            }
            if self.cached.is_empty() {
                self.cursor = None;
                self.saved_buffer = None;
                return RecallOutcome::Unchanged;
            }
            self.cursor = Some(0);
            return RecallOutcome::Replace { new_text: self.cached[0].clone() };
        }
        let next = self.cursor.unwrap() + 1;
        if next >= self.cached.len() {
            return RecallOutcome::Unchanged;
        }
        self.cursor = Some(next);
        RecallOutcome::Replace { new_text: self.cached[next].clone() }
    }

    /// `↓`: walk newer. Returns to the saved buffer when stepping
    /// past index 0; abandons the walk. No-op when not walking.
    pub fn step_forward(&mut self) -> RecallOutcome {
        let Some(cursor) = self.cursor else {
            return RecallOutcome::Unchanged;
        };
        if cursor == 0 {
            // Back to the user's pre-walk buffer.
            let restored = self.saved_buffer.take().unwrap_or_default();
            self.cached.clear();
            self.cursor = None;
            return RecallOutcome::Replace { new_text: restored };
        }
        let prev = cursor - 1;
        self.cursor = Some(prev);
        RecallOutcome::Replace { new_text: self.cached[prev].clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn first_up_saves_buffer_and_shows_most_recent() {
        let mut s = RecallState::default();
        let out = s.step_back(|| entries(&["ls", "cd", "echo"]), "my-draft");
        assert_eq!(out, RecallOutcome::Replace { new_text: "ls".to_string() });
        assert!(s.is_walking());
        assert_eq!(s.saved_buffer.as_deref(), Some("my-draft"));
    }

    #[test]
    fn second_up_walks_to_older_entry() {
        let mut s = RecallState::default();
        let _ = s.step_back(|| entries(&["ls", "cd"]), "");
        let out = s.step_back(|| panic!("query_fn must not be called on subsequent steps"), "");
        assert_eq!(out, RecallOutcome::Replace { new_text: "cd".to_string() });
    }

    #[test]
    fn up_at_oldest_returns_unchanged() {
        let mut s = RecallState::default();
        s.step_back(|| entries(&["only"]), "");
        let out = s.step_back(|| panic!("query_fn not called"), "");
        assert_eq!(out, RecallOutcome::Unchanged);
        // State is preserved — we're still walking, still at the
        // oldest entry.
        assert!(s.is_walking());
    }

    #[test]
    fn down_at_most_recent_restores_saved_buffer_and_abandons() {
        let mut s = RecallState::default();
        s.step_back(|| entries(&["ls"]), "my-draft");
        let out = s.step_forward();
        assert_eq!(out, RecallOutcome::Replace { new_text: "my-draft".to_string() });
        assert!(!s.is_walking());
    }

    #[test]
    fn down_when_not_walking_is_unchanged() {
        let mut s = RecallState::default();
        let out = s.step_forward();
        assert_eq!(out, RecallOutcome::Unchanged);
        assert!(!s.is_walking());
    }

    #[test]
    fn down_walks_back_toward_newer_entries() {
        let mut s = RecallState::default();
        s.step_back(|| entries(&["a", "b", "c"]), "draft");
        s.step_back(|| entries(&[]), "draft"); // → b
        s.step_back(|| entries(&[]), "draft"); // → c
        let out = s.step_forward(); // → b
        assert_eq!(out, RecallOutcome::Replace { new_text: "b".to_string() });
        let out = s.step_forward(); // → a
        assert_eq!(out, RecallOutcome::Replace { new_text: "a".to_string() });
        let out = s.step_forward(); // → restore "draft"
        assert_eq!(out, RecallOutcome::Replace { new_text: "draft".to_string() });
    }

    #[test]
    fn abandon_resets_state() {
        let mut s = RecallState::default();
        s.step_back(|| entries(&["ls"]), "draft");
        assert!(s.is_walking());
        s.abandon();
        assert!(!s.is_walking());
        assert!(s.saved_buffer.is_none());
        // After abandon, the next up re-queries (and re-saves the
        // buffer, which may now differ from the original "draft").
        let out = s.step_back(|| entries(&["new"]), "different-draft");
        assert_eq!(out, RecallOutcome::Replace { new_text: "new".to_string() });
        assert_eq!(s.saved_buffer.as_deref(), Some("different-draft"));
    }

    #[test]
    fn up_with_empty_history_is_unchanged_and_does_not_save_buffer() {
        // If history is empty, the walk shouldn't start — the user's
        // draft must remain editable as-is.
        let mut s = RecallState::default();
        let out = s.step_back(|| entries(&[]), "draft");
        assert_eq!(out, RecallOutcome::Unchanged);
        assert!(!s.is_walking());
        assert!(s.saved_buffer.is_none());
    }

    #[test]
    fn up_skips_entry_that_already_matches_editor() {
        // Edge case: editor already contains "ls" (e.g. the user
        // just submitted it, the cwd changed, they reused). The
        // first `↑` should jump straight to the entry BEFORE that,
        // not redisplay the same string.
        let mut s = RecallState::default();
        let out = s.step_back(|| entries(&["ls", "cd", "echo"]), "ls");
        assert_eq!(out, RecallOutcome::Replace { new_text: "cd".to_string() });
    }
}
