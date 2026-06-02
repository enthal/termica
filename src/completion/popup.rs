//! Completion-popup UI state.
//!
//! Owns the candidate list, the currently-selected row, and the
//! byte range in the editor that gets replaced on accept. The
//! actual paint lives in [`crate::render`]; this module is data +
//! transitions, testable without egui.

use crate::prompt_editor::PromptEditor;

use super::CompletionCandidate;

/// One in-flight completion popup. Created when the user presses
/// `Tab` and a non-empty candidate list comes back; cleared on
/// Esc, accept, or any edit that breaks the typed-token prefix.
///
/// `origin_byte` is the editor byte index where the typed token
/// starts; `original_token` is what the user had typed at popup-
/// open time. On accept, the renderer replaces
/// `editor.text[origin_byte..origin_byte + current_token_len]`
/// with the accepted candidate's `value`.
///
/// `selected_index` is the cursor in the candidate list. Always
/// in range `[0, candidates.len())`.
#[derive(Debug, Clone)]
pub struct CompletionPopup {
    pub origin_byte: usize,
    pub original_token: String,
    pub candidates: Vec<CompletionCandidate>,
    pub selected_index: usize,
}

impl CompletionPopup {
    /// Open a popup with the given candidates at `origin_byte`.
    /// Returns `None` if `candidates` is empty — the caller
    /// suppresses the popup in that case rather than rendering an
    /// empty one.
    pub fn new(
        origin_byte: usize,
        original_token: impl Into<String>,
        candidates: Vec<CompletionCandidate>,
    ) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        Some(Self {
            origin_byte,
            original_token: original_token.into(),
            candidates,
            selected_index: 0,
        })
    }

    /// Currently-highlighted candidate.
    pub fn selected(&self) -> &CompletionCandidate {
        &self.candidates[self.selected_index]
    }

    /// Move the selection by `delta` rows, wrapping at the ends.
    /// `delta` of `+1` is "next candidate"; `-1` is "previous".
    pub fn move_selection(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }
        let len = self.candidates.len() as isize;
        let new_idx = ((self.selected_index as isize + delta).rem_euclid(len)) as usize;
        self.selected_index = new_idx;
    }

    /// Accept the highlighted candidate: replace the editor's
    /// `origin_byte..origin_byte + len(current_token)` range with
    /// the candidate's `value`. Caller is responsible for
    /// dropping the popup after this returns (it's not self-
    /// destructive so callers can compose with their own popup-
    /// lifecycle logic).
    ///
    /// Uses [`PromptEditor::set_selection`] + [`PromptEditor::insert_str`]
    /// to keep undo coherence — the replace lands as one
    /// `OpKind::Other` entry.
    pub fn accept(&self, editor: &mut PromptEditor, current_token_len: usize) {
        let end = self.origin_byte.saturating_add(current_token_len).min(editor.len_bytes());
        editor.set_selection(self.origin_byte, end);
        editor.insert_str(&self.selected().value);
    }
}

#[cfg(test)]
mod tests {
    use super::super::CompletionSource;
    use super::*;

    fn cand(value: &str) -> CompletionCandidate {
        CompletionCandidate::simple(value, CompletionSource::Path)
    }

    #[test]
    fn new_with_no_candidates_returns_none() {
        assert!(CompletionPopup::new(0, "", vec![]).is_none());
    }

    #[test]
    fn new_with_candidates_returns_some_and_selects_zero() {
        let p = CompletionPopup::new(2, "Ca", vec![cand("Cargo.toml")]).unwrap();
        assert_eq!(p.selected_index, 0);
        assert_eq!(p.selected().value, "Cargo.toml");
        assert_eq!(p.original_token, "Ca");
        assert_eq!(p.origin_byte, 2);
    }

    #[test]
    fn move_selection_advances_and_wraps() {
        let mut p = CompletionPopup::new(0, "", vec![cand("a"), cand("b"), cand("c")]).unwrap();
        p.move_selection(1);
        assert_eq!(p.selected().value, "b");
        p.move_selection(1);
        assert_eq!(p.selected().value, "c");
        p.move_selection(1); // wraps
        assert_eq!(p.selected().value, "a");
        p.move_selection(-1); // wraps backward
        assert_eq!(p.selected().value, "c");
    }

    #[test]
    fn move_selection_large_delta_uses_modular_arithmetic() {
        let mut p = CompletionPopup::new(0, "", vec![cand("a"), cand("b"), cand("c")]).unwrap();
        p.move_selection(10);
        // (0 + 10).rem_euclid(3) == 1 → "b"
        assert_eq!(p.selected().value, "b");
        p.move_selection(-7);
        // (1 + (-7)).rem_euclid(3) == (-6).rem_euclid(3) == 0 → "a"
        assert_eq!(p.selected().value, "a");
    }

    #[test]
    fn accept_replaces_typed_token_with_candidate_value() {
        let mut e = PromptEditor::new();
        e.insert_str("ls Ca");
        // Token "Ca" starts at byte 3, length 2.
        let p = CompletionPopup::new(3, "Ca", vec![cand("Cargo.toml")]).unwrap();
        p.accept(&mut e, 2);
        assert_eq!(e.text(), "ls Cargo.toml");
        // Caret lands at the end of the inserted text.
        assert_eq!(e.cursor(), e.len_bytes());
    }

    #[test]
    fn accept_clamps_to_buffer_end_when_token_len_overshoots() {
        // Defensive: if some stale state passes a too-large
        // current_token_len, the accept still doesn't panic
        // (selection set clamps; insert_str degrades to "replace
        // up to end").
        let mut e = PromptEditor::new();
        e.insert_str("ab");
        let p = CompletionPopup::new(0, "ab", vec![cand("hello")]).unwrap();
        p.accept(&mut e, 999);
        assert_eq!(e.text(), "hello");
    }
}
