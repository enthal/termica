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
    /// `true` when the selected row should be scrolled into view
    /// on the next paint. Set by [`Self::move_selection`]; cleared
    /// by [`paint`] after scrolling. Lets the user manually
    /// scroll without the popup yanking back to the selection
    /// every frame — we only scroll on a real selection change.
    pub scroll_to_selected_pending: bool,
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
            scroll_to_selected_pending: false,
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
        // Schedule a scroll on the next paint so the newly-
        // selected row is visible even when it would otherwise
        // be below / above the scroll viewport (e.g. after
        // wrap-around with `↑` from the first row to the last).
        self.scroll_to_selected_pending = true;
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

    /// Compute the readline-style smart-Tab extension.
    ///
    /// Starting from the selected candidate's value at byte
    /// `current_token_len`, walk forward one char at a time and
    /// include each char as long as at least one OTHER candidate
    /// also starts with the extended prefix. Stop at the first
    /// char where the selected diverges from every other
    /// candidate. The returned `&str` is the new full prefix the
    /// editor should land at after Tab (NOT just the appended
    /// suffix — the caller replaces the typed token with it
    /// wholesale via `set_selection` + `insert_str`).
    ///
    /// Examples (current_token = "ab"):
    /// - candidates `[abcx, abcy, abz]`, selected = `abcx`:
    ///   pos 2 char 'c' → abcy matches "abc" → include. pos 3
    ///   char 'x' → no other matches "abcx" → stop. Result:
    ///   "abc". User can press Tab again with the now-filtered
    ///   `[abcx, abcy]`; that pass finds no extension beyond
    ///   "abc" (the candidates diverge at pos 3) so Tab is a
    ///   no-op and the user picks via Up/Down + Enter.
    /// - **Single candidate**: Tab is a full accept — there's
    ///   nothing to disambiguate, so the user's clearly trying
    ///   to commit.
    /// - **No extension possible** (all other candidates diverge
    ///   from selected immediately past the token): returns the
    ///   current token unchanged; the caller treats that as
    ///   "Tab can't extend, do nothing".
    pub fn tab_extend(&self, current_token_len: usize) -> &str {
        if self.candidates.len() == 1 {
            return &self.selected().value;
        }
        let selected = self.selected().value.as_str();
        if current_token_len >= selected.len() {
            return selected;
        }
        let mut byte_pos = current_token_len;
        while byte_pos < selected.len() {
            let Some(next_char) = selected.get(byte_pos..).and_then(|s| s.chars().next()) else {
                break;
            };
            let next_pos = byte_pos + next_char.len_utf8();
            let Some(prefix_inc) = selected.get(..next_pos) else { break };
            let any_other = self
                .candidates
                .iter()
                .enumerate()
                .any(|(i, c)| i != self.selected_index && c.value.starts_with(prefix_inc));
            if !any_other {
                break;
            }
            byte_pos = next_pos;
        }
        selected.get(..byte_pos).unwrap_or(selected)
    }
}

/// Paint the popup as an `egui::Area` anchored at `anchor`.
///
/// `anchor` is the TOP-LEFT of the editor; the popup's BOTTOM-
/// LEFT corner aligns with it (via `Area::pivot(LEFT_BOTTOM)`),
/// so the popup grows upward and the editor stays unoccluded —
/// regardless of how tall the candidate list ends up being. No
/// pre-estimated height math, which means no risk of getting the
/// estimate wrong and clipping into the editor.
///
/// Up to `max_visible_rows` candidates render before the list
/// scrolls.
///
/// Returns `Some(row_idx)` when the user clicked a row — the
/// caller treats this as an `Accept` action (replaces the typed
/// token with the clicked candidate's value, closes the popup).
/// `None` when no row was clicked this frame.
pub fn paint(
    ctx: &egui::Context,
    popup: &mut CompletionPopup,
    anchor: egui::Pos2,
    pane_id: u64,
    max_visible_rows: usize,
) -> Option<usize> {
    let area_id = egui::Id::new(("completion-popup", pane_id));
    let row_h = 18.0;
    let panel_w = 380.0;
    let mut clicked_row: Option<usize> = None;

    egui::Area::new(area_id)
        // Pivot at LEFT_BOTTOM means the bottom-left corner of
        // the area sits at `anchor` — i.e. the popup's bottom
        // edge aligns with the editor's top edge and the popup
        // grows upward.
        .pivot(egui::Align2::LEFT_BOTTOM)
        .fixed_pos(egui::Pos2::new(anchor.x, anchor.y - 4.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(panel_w);
                let scroll_h = (max_visible_rows as f32) * row_h;
                let scroll_pending = std::mem::take(&mut popup.scroll_to_selected_pending);
                egui::ScrollArea::vertical()
                    .max_height(scroll_h)
                    .id_salt(("completion-popup-scroll", pane_id))
                    .show(ui, |ui| {
                        for (idx, cand) in popup.candidates.iter().enumerate() {
                            let selected = idx == popup.selected_index;
                            let response = paint_row(ui, cand, selected);
                            // Click on a row = accept that row.
                            // Hover updates the selected_index so
                            // the row visibly highlights as the
                            // user moves over it.
                            if response.clicked() {
                                clicked_row = Some(idx);
                            } else if response.hovered() {
                                popup.selected_index = idx;
                            }
                            // After painting the selected row, if
                            // a scroll was scheduled this frame
                            // (move_selection ran), align the
                            // row's center with the viewport
                            // center so wrap-around at the ends
                            // visibly jumps to the new position.
                            if selected && scroll_pending {
                                ui.scroll_to_cursor(Some(egui::Align::Center));
                            }
                        }
                    });
                ui.add_space(2.0);
                paint_keybind_hint(ui);
            });
        });
    clicked_row
}

/// Bottom-of-popup keybinding strip. Per CLAUDE.md
/// "no Unicode pictographic icons in widget code" — Unicode
/// arrows (`↑`/`↓`) render as tofu on some font setups. Plain
/// text "Up/Down" is the v1 stand-in until an `icons.rs` module
/// (planned in [spec/06 §Iconography](../../spec/06-workspace-and-tiles.md#iconography))
/// supplies real triangle glyphs via `egui::Painter`.
fn paint_keybind_hint(ui: &mut egui::Ui) {
    ui.weak("[Tab / Enter] accept   [Up / Down] navigate   [Esc] cancel");
}

fn paint_row(ui: &mut egui::Ui, cand: &CompletionCandidate, selected: bool) -> egui::Response {
    let visuals = ui.visuals();
    let bg = if selected { visuals.selection.bg_fill } else { egui::Color32::TRANSPARENT };
    let fg = if selected { visuals.selection.stroke.color } else { visuals.text_color() };
    let dim = visuals.weak_text_color();

    // `allocate_response` returns an interactive widget covering
    // the row rect; this is what surfaces clicks + hover state
    // to the caller. (`allocate_space` is the read-only variant
    // and was the source of "popup rows don't respond to
    // clicks.")
    let desired = egui::Vec2::new(ui.available_width(), 18.0);
    let response = ui.allocate_response(desired, egui::Sense::click());
    let rect = response.rect;
    if selected {
        ui.painter().rect_filled(rect, 2.0, bg);
    }
    // Display label, source tag, description — laid out left-to-right
    // with the tag pinned to the right edge.
    let painter = ui.painter();
    let display = &cand.display;
    let tag = cand.source.tag();
    let font = egui::FontId::monospace(13.0);
    painter.text(
        egui::Pos2::new(rect.min.x + 6.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        display,
        font.clone(),
        fg,
    );
    if let Some(desc) = cand.description.as_deref() {
        // Description center-right of the row.
        painter.text(
            egui::Pos2::new(rect.max.x - 60.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            desc,
            font.clone(),
            dim,
        );
    }
    painter.text(
        egui::Pos2::new(rect.max.x - 6.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        tag,
        font,
        dim,
    );
    response
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

    // ---- tab_extend (smart-Tab) -----------------------------------

    fn cands(values: &[&str]) -> Vec<CompletionCandidate> {
        values.iter().map(|v| cand(v)).collect()
    }

    #[test]
    fn tab_extend_extends_to_shared_prefix_when_multiple_match() {
        // Selected "abcx" against [abcx, abcy, abz]; current token
        // "ab" (len 2). 'c' is shared by abcy → include. 'x' is
        // unique → stop. Result: "abc".
        let p = CompletionPopup::new(0, "ab", cands(&["abcx", "abcy", "abz"])).unwrap();
        assert_eq!(p.tab_extend(2), "abc");
    }

    #[test]
    fn tab_extend_unique_candidate_extends_to_full_value() {
        // Single candidate → Tab is a full accept (nothing to
        // disambiguate).
        let p = CompletionPopup::new(0, "ab", cands(&["abcdef"])).unwrap();
        assert_eq!(p.tab_extend(2), "abcdef");
    }

    #[test]
    fn tab_extend_no_shared_extension_returns_current_token_prefix() {
        // Selected "abcx" against [abcx, abdy, abez]; at pos 2
        // selected's 'c' has no peer. Result: stays at "ab".
        let p = CompletionPopup::new(0, "ab", cands(&["abcx", "abdy", "abez"])).unwrap();
        assert_eq!(p.tab_extend(2), "ab");
    }

    #[test]
    fn tab_extend_token_already_at_max_returns_full_value() {
        // current_token_len equals selected.len() — nothing to
        // walk past. Just return selected.
        let p = CompletionPopup::new(0, "abc", cands(&["abc", "abcd"])).unwrap();
        assert_eq!(p.tab_extend(3), "abc");
    }

    #[test]
    fn tab_extend_handles_multibyte_chars_correctly() {
        // Selected "café" (4 bytes including 2-byte é). Other
        // candidate "calf". Current token "ca" (len 2). 'f' (in
        // "calf") vs 'f' (no — selected has é at pos 2). At pos
        // 2 selected's char is 'é'; 'calf' has 'l' there — diverge.
        // Result: "ca".
        let p = CompletionPopup::new(0, "ca", cands(&["café", "calf"])).unwrap();
        assert_eq!(p.tab_extend(2), "ca");
    }

    #[test]
    fn tab_extend_iterative_use_converges() {
        // Apply tab_extend repeatedly with filtered candidates —
        // each pass narrows the list, eventually no further
        // extension is possible and the user must pick.
        let p1 = CompletionPopup::new(0, "ab", cands(&["abcx", "abcy", "abz"])).unwrap();
        assert_eq!(p1.tab_extend(2), "abc");
        // After applying "abc", the filtered list is [abcx, abcy].
        let p2 = CompletionPopup::new(0, "abc", cands(&["abcx", "abcy"])).unwrap();
        // Selected = abcx; abcy has 'y' at pos 3 vs 'x' → diverge.
        // No extension; "abc" stays.
        assert_eq!(p2.tab_extend(3), "abc");
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
