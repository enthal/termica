//! `↑` / `↓` history recall state machine.
//!
//! Owns the small bit of memory needed to walk a list of past
//! commands and substitute them into the editor: the user's
//! pre-walk buffer (text + caret), the cached query results, and
//! the current index. Pure logic — no DB, no editor type, just
//! `String` + `usize` in / `String` + `usize` out — so tests cover
//! it without spawning anything.
//!
//! The wiring at the call site (`render_pane`) is:
//!   - ArrowUp → `RecallState::step_back(query_fn(), current_text,
//!     current_cursor)`
//!   - ArrowDown → `RecallState::step_forward()`
//!   - Any other edit → `RecallState::abandon()` so the next walk
//!     re-queries and re-saves the buffer.
//!
//! Per [spec/04 §"History walk (Up/Down)"](../../spec/04-prompt-editor.md#history-walk-updown),
//! the saved buffer carries BOTH text and caret byte index so a
//! `↑ → ↓` round-trip restores the editor exactly — caret included.

/// Outcome of one recall step. The caller substitutes `new_text`
/// into the editor and places the caret at `new_cursor`.
///
/// `new_cursor` is `Some(byte_index)` when restoring the
/// in-progress buffer at the head of the walk (the user's caret
/// position is preserved) and `None` when showing a history entry
/// (the caller defaults to end-of-text — same convention as zsh /
/// bash / fish).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallOutcome {
    /// Editor text should now read `new_text`. The caller places
    /// the caret at `new_cursor` if `Some`, otherwise at the end of
    /// the new text.
    Replace { new_text: String, new_cursor: Option<usize> },
    /// We were already at the oldest entry (Up) or returned to the
    /// pre-walk buffer (Down) with no saved state. Editor stays
    /// as-is.
    Unchanged,
}

/// The user's pre-walk editor state. Stored on the first `↑` and
/// restored on `↓` past the most-recent entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedBuffer {
    text: String,
    cursor: usize,
}

/// State carried across recall steps.
#[derive(Debug, Default, Clone)]
pub struct RecallState {
    /// User's pre-walk editor state (text + caret). `Some` when a
    /// walk is in progress; restored on `↓` past the most-recent
    /// entry per spec/04 §"History walk (Up/Down)".
    saved: Option<SavedBuffer>,
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
        self.saved = None;
        self.cached.clear();
        self.cursor = None;
    }

    /// `↑`: show the previous (older) history entry. First call in
    /// a walk saves `current_text` + `current_cursor` as the
    /// restore point and queries history via `query_fn`. Subsequent
    /// calls advance the cursor.
    ///
    /// `query_fn` is invoked only on the first step (the cache is
    /// reused thereafter), so the caller pays for at most one DB
    /// query per walk.
    pub fn step_back(
        &mut self,
        query_fn: impl FnOnce() -> Vec<String>,
        current_text: &str,
        current_cursor: usize,
    ) -> RecallOutcome {
        if self.cursor.is_none() {
            self.saved =
                Some(SavedBuffer { text: current_text.to_string(), cursor: current_cursor });
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
                self.saved = None;
                return RecallOutcome::Unchanged;
            }
            self.cursor = Some(0);
            return RecallOutcome::Replace { new_text: self.cached[0].clone(), new_cursor: None };
        }
        let next = self.cursor.unwrap() + 1;
        if next >= self.cached.len() {
            return RecallOutcome::Unchanged;
        }
        self.cursor = Some(next);
        RecallOutcome::Replace { new_text: self.cached[next].clone(), new_cursor: None }
    }

    /// `↓`: walk newer. Returns to the saved buffer (text + caret)
    /// when stepping past index 0; abandons the walk. No-op when
    /// not walking.
    pub fn step_forward(&mut self) -> RecallOutcome {
        let Some(cursor) = self.cursor else {
            return RecallOutcome::Unchanged;
        };
        if cursor == 0 {
            // Back to the user's pre-walk buffer — text AND caret.
            // Per spec/04 §"History walk (Up/Down)": the round-trip
            // through `↑ → ↓` puts the caret right where it was
            // when `↑` first stepped away.
            let restored =
                self.saved.take().unwrap_or(SavedBuffer { text: String::new(), cursor: 0 });
            self.cached.clear();
            self.cursor = None;
            return RecallOutcome::Replace {
                new_text: restored.text,
                new_cursor: Some(restored.cursor),
            };
        }
        let prev = cursor - 1;
        self.cursor = Some(prev);
        RecallOutcome::Replace { new_text: self.cached[prev].clone(), new_cursor: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn replace_text(new_text: &str) -> RecallOutcome {
        RecallOutcome::Replace { new_text: new_text.to_string(), new_cursor: None }
    }

    fn replace_with_cursor(new_text: &str, new_cursor: usize) -> RecallOutcome {
        RecallOutcome::Replace { new_text: new_text.to_string(), new_cursor: Some(new_cursor) }
    }

    #[test]
    fn first_up_saves_buffer_and_shows_most_recent() {
        let mut s = RecallState::default();
        let out = s.step_back(|| entries(&["ls", "cd", "echo"]), "my-draft", 3);
        assert_eq!(out, replace_text("ls"));
        assert!(s.is_walking());
        assert_eq!(s.saved.as_ref().map(|b| b.text.as_str()), Some("my-draft"));
        assert_eq!(s.saved.as_ref().map(|b| b.cursor), Some(3));
    }

    #[test]
    fn second_up_walks_to_older_entry() {
        let mut s = RecallState::default();
        let _ = s.step_back(|| entries(&["ls", "cd"]), "", 0);
        let out = s.step_back(|| panic!("query_fn must not be called on subsequent steps"), "", 0);
        assert_eq!(out, replace_text("cd"));
    }

    #[test]
    fn up_at_oldest_returns_unchanged() {
        let mut s = RecallState::default();
        s.step_back(|| entries(&["only"]), "", 0);
        let out = s.step_back(|| panic!("query_fn not called"), "", 0);
        assert_eq!(out, RecallOutcome::Unchanged);
        assert!(s.is_walking());
    }

    #[test]
    fn down_at_most_recent_restores_saved_buffer_text_and_cursor() {
        // The headline of the spec/04 caret-restore rule: ↑ → ↓
        // restores BOTH text and caret byte index.
        let mut s = RecallState::default();
        s.step_back(|| entries(&["ls"]), "my-draft", 4);
        let out = s.step_forward();
        assert_eq!(out, replace_with_cursor("my-draft", 4));
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
        s.step_back(|| entries(&["a", "b", "c"]), "draft", 2);
        s.step_back(|| entries(&[]), "draft", 2); // → b
        s.step_back(|| entries(&[]), "draft", 2); // → c
        let out = s.step_forward(); // → b
        assert_eq!(out, replace_text("b"));
        let out = s.step_forward(); // → a
        assert_eq!(out, replace_text("a"));
        let out = s.step_forward(); // → restore "draft" with cursor 2
        assert_eq!(out, replace_with_cursor("draft", 2));
    }

    #[test]
    fn abandon_resets_state() {
        let mut s = RecallState::default();
        s.step_back(|| entries(&["ls"]), "draft", 0);
        assert!(s.is_walking());
        s.abandon();
        assert!(!s.is_walking());
        assert!(s.saved.is_none());
        // After abandon, the next up re-queries (and re-saves the
        // buffer + cursor, which may now differ from the original).
        let out = s.step_back(|| entries(&["new"]), "different-draft", 7);
        assert_eq!(out, replace_text("new"));
        assert_eq!(s.saved.as_ref().map(|b| b.cursor), Some(7));
    }

    #[test]
    fn up_with_empty_history_is_unchanged_and_does_not_save_buffer() {
        let mut s = RecallState::default();
        let out = s.step_back(|| entries(&[]), "draft", 2);
        assert_eq!(out, RecallOutcome::Unchanged);
        assert!(!s.is_walking());
        assert!(s.saved.is_none());
    }

    #[test]
    fn up_skips_entry_that_already_matches_editor() {
        let mut s = RecallState::default();
        let out = s.step_back(|| entries(&["ls", "cd", "echo"]), "ls", 2);
        assert_eq!(out, replace_text("cd"));
    }

    #[test]
    fn round_trip_preserves_cursor_at_arbitrary_position() {
        // The user types "git push origin", caret somewhere mid-
        // word at byte 4 (between "git " and "push"). They ↑ once,
        // glance, ↓ back. The caret returns to byte 4 — NOT to
        // end-of-text.
        let mut s = RecallState::default();
        s.step_back(|| entries(&["ls"]), "git push origin", 4);
        let out = s.step_forward();
        assert_eq!(out, replace_with_cursor("git push origin", 4));
    }
}
