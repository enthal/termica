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
    /// `true` on a freshly-opened popup so [`paint`] resets the scroll
    /// view to the top on its first frame. egui's `ScrollArea` persists
    /// its offset by id, so without this a reopened popup would inherit
    /// the previous session's scroll position. Cleared after the first
    /// paint; a background refresh (driver result swap) clears it without
    /// scrolling so the view doesn't jump.
    pub scroll_to_top_pending: bool,
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
            scroll_to_top_pending: true,
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
    /// the candidate's `value`, followed by a space so the line is
    /// ready for the next argument. Caller is responsible for
    /// dropping the popup after this returns (it's not self-
    /// destructive so callers can compose with their own popup-
    /// lifecycle logic).
    ///
    /// The trailing space is **suppressed for a directory** (value ends
    /// with `/`): the user keeps completing into the path, exactly as a
    /// shell does. This is full-term acceptance (Tab-commit / Enter /
    /// click / single-candidate auto-accept); extending only to a common
    /// prefix takes a different path (`replace_range` in the renderer)
    /// and correctly gets no space.
    ///
    /// `replace_range` captures the editor's pre-call state (selection =
    /// None) for the undo entry, so Cmd+Z restores the buffer WITHOUT a
    /// phantom selection over the typed-token range.
    pub fn accept(&self, editor: &mut PromptEditor, current_token_len: usize) {
        let end = self.origin_byte.saturating_add(current_token_len).min(editor.len_bytes());
        let value = &self.selected().value;
        let replacement = if value.ends_with('/') { value.clone() } else { format!("{value} ") };
        editor.replace_range(self.origin_byte, end, &replacement);
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
    let mut clicked_row: Option<usize> = None;

    // Column model: shared per-column widths so every row's cells line up
    // into a table, plus the monospace advance to turn char offsets into
    // pixels. The popup width grows to fit the widest row + the tag (capped
    // — a very wide table clips its rightmost columns rather than running
    // off-screen).
    let gap = 2usize;
    let layout = ColumnLayout::compute(&popup.candidates);
    let font = egui::FontId::monospace(13.0);
    // Monospace advance: every glyph is the same width, so one `M` measures
    // the column step. `glyph_width` mutates the font cache, hence
    // `fonts_mut` (the same call render_pane uses for the grid cell width).
    let char_w = ctx.fonts_mut(|f| f.glyph_width(&font, 'M')).max(1.0);
    let max_tag =
        popup.candidates.iter().map(|c| c.source.tag().chars().count()).max().unwrap_or(0);
    let content_w =
        6.0 + (layout.total_chars(gap) as f32) * char_w + ((max_tag + 3) as f32) * char_w + 8.0;
    let panel_w = content_w.clamp(380.0, 820.0);

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
                let top_pending = std::mem::take(&mut popup.scroll_to_top_pending);
                egui::ScrollArea::vertical()
                    .max_height(scroll_h)
                    .id_salt(("completion-popup-scroll", pane_id))
                    .show(ui, |ui| {
                        for (idx, cand) in popup.candidates.iter().enumerate() {
                            let selected = idx == popup.selected_index;
                            let response = paint_row(ui, cand, selected, &layout, char_w, gap);
                            // Click on a row = accept that row.
                            // Hover updates the selected_index so
                            // the row visibly highlights as the
                            // user moves over it.
                            if response.clicked() {
                                clicked_row = Some(idx);
                            } else if response.hovered() {
                                popup.selected_index = idx;
                            }
                            // On a fresh open, snap the view to the top so
                            // a reopened popup doesn't inherit the previous
                            // session's scroll offset (egui persists it by
                            // id). Scroll to row 0's RECT (not the post-row
                            // cursor, which would push row 0 off the top).
                            if idx == 0 && top_pending {
                                ui.scroll_to_rect(response.rect, Some(egui::Align::TOP));
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

/// Paint a small "searching..." affordance with an animated spinner while
/// an asynchronous CLI-native driver result is awaited and no popup is
/// shown yet (the [`crate::completion::CompletionPlan::AwaitDriver`] wait).
/// Same anchor / pivot as [`paint`], so it sits exactly where the popup
/// will appear. `egui::Spinner` requests its own repaints, so it animates
/// without the caller scheduling frames.
pub fn paint_searching(ctx: &egui::Context, anchor: egui::Pos2, pane_id: u64) {
    let area_id = egui::Id::new(("completion-searching", pane_id));
    egui::Area::new(area_id)
        .pivot(egui::Align2::LEFT_BOTTOM)
        .fixed_pos(egui::Pos2::new(anchor.x, anchor.y - 4.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.weak("searching...");
                });
            });
        });
}

/// Bottom-of-popup keybinding strip. The navigate hint uses the drawn
/// up/down arrow glyphs from [`crate::icons`] (Unicode `↑`/`↓` render
/// as tofu on some font setups — CLAUDE.md "no Unicode symbols for
/// icons").
fn paint_keybind_hint(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.weak("Tab / Enter accept");
        ui.weak("·");
        let color = ui.visuals().weak_text_color();
        let h = ui.text_style_height(&egui::TextStyle::Body);
        for down in [false, true] {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(h * 0.7, h), egui::Sense::hover());
            crate::icons::paint_arrow_glyph(ui.painter(), rect, color, down);
        }
        ui.weak("navigate");
        ui.weak("·");
        ui.weak("Esc cancel");
    });
}

/// Split a completion description into its columns. The driver parsers
/// encode a tabular completion's cells joined by `\t` (with **empty cells
/// preserved** as empty fields, so a row missing a value stays aligned —
/// see [`crate::completion::drivers::parse::parse_fish_complete`]); a plain
/// prose description has no `\t` and is a single cell. We split on `\t`
/// only (keeping empties), so column `k` of every row lines up by index.
fn split_columns(desc: &str) -> Vec<&str> {
    desc.split('\t').collect()
}

/// Per-column maximum widths (in characters) across a popup's candidates,
/// so each row can paint its cells at shared tab-stops. Column 0 is the
/// candidate **name** (`display`); columns 1.. are the description's
/// [`split_columns`] cells. Pure + tested — the paint just reads offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnLayout {
    widths: Vec<usize>,
}

impl ColumnLayout {
    /// Measure every candidate and keep the widest cell per column.
    pub(crate) fn compute(candidates: &[CompletionCandidate]) -> Self {
        let mut widths: Vec<usize> = Vec::new();
        let bump = |i: usize, w: usize, widths: &mut Vec<usize>| {
            if i < widths.len() {
                widths[i] = widths[i].max(w);
            } else {
                widths.push(w);
            }
        };
        for c in candidates {
            bump(0, c.display.chars().count(), &mut widths);
            if let Some(d) = &c.description {
                for (k, cell) in split_columns(d).into_iter().enumerate() {
                    bump(k + 1, cell.chars().count(), &mut widths);
                }
            }
        }
        ColumnLayout { widths }
    }

    /// Number of columns (name + description cells).
    #[cfg(test)]
    pub(crate) fn ncols(&self) -> usize {
        self.widths.len()
    }

    /// Start offset of column `i` in characters, with `gap` blank chars
    /// between adjacent columns. Column 0 starts at 0.
    pub(crate) fn col_start(&self, i: usize, gap: usize) -> usize {
        self.widths.iter().take(i).map(|w| w + gap).sum()
    }

    /// Total width of all columns including the inter-column gaps, in
    /// characters. `0` when there are no columns.
    pub(crate) fn total_chars(&self, gap: usize) -> usize {
        if self.widths.is_empty() {
            return 0;
        }
        self.widths.iter().sum::<usize>() + gap * (self.widths.len() - 1)
    }
}

fn paint_row(
    ui: &mut egui::Ui,
    cand: &CompletionCandidate,
    selected: bool,
    layout: &ColumnLayout,
    char_w: f32,
    gap: usize,
) -> egui::Response {
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
    // Layout: the candidate name is column 0; the description's cells are
    // columns 1.., each painted at a shared tab-stop (`layout`) so rows
    // line up into a table. The source tag stays pinned at the right edge.
    let painter = ui.painter();
    let tag = cand.source.tag();
    let font = egui::FontId::monospace(13.0);
    let mid_y = rect.center().y;
    let left = rect.min.x + 6.0;

    let tag_rect = painter.text(
        egui::Pos2::new(rect.max.x - 6.0, mid_y),
        egui::Align2::RIGHT_CENTER,
        tag,
        font.clone(),
        dim,
    );
    // Clip every cell to the area before the tag, so a too-wide table can
    // never paint under the tag (it scrolls/clips instead of overlapping).
    let cells_right = tag_rect.min.x - 8.0;
    let cell_clip = egui::Rect::from_min_max(
        egui::Pos2::new(rect.min.x, rect.min.y),
        egui::Pos2::new(cells_right.max(rect.min.x), rect.max.y),
    );
    let cell_painter = painter.with_clip_rect(cell_clip);

    // Column 0: the name (insertable value), in the normal text color.
    cell_painter.text(
        egui::Pos2::new(left, mid_y),
        egui::Align2::LEFT_CENTER,
        &cand.display,
        font.clone(),
        fg,
    );
    // Columns 1..: the description cells, dim, at their tab-stops.
    if let Some(desc) = cand.description.as_deref() {
        for (k, cell) in split_columns(desc).into_iter().enumerate() {
            let x = left + (layout.col_start(k + 1, gap) as f32) * char_w;
            cell_painter.text(
                egui::Pos2::new(x, mid_y),
                egui::Align2::LEFT_CENTER,
                cell,
                font.clone(),
                dim,
            );
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::super::CompletionSource;
    use super::*;

    fn cand(value: &str) -> CompletionCandidate {
        CompletionCandidate::simple(value, CompletionSource::Path)
    }

    fn cand_desc(value: &str, desc: &str) -> CompletionCandidate {
        CompletionCandidate::with_description(value, desc, CompletionSource::Path)
    }

    // ---- column layout (tabular completions) ------------------------

    #[test]
    fn split_columns_splits_on_tabs_preserving_empties() {
        // `\t` = column boundary; empty fields are KEPT so columns align by
        // index. A prose description (no `\t`) is one cell.
        assert_eq!(
            split_columns("ds\tapps/v1\ttrue\tDaemonSet"),
            ["ds", "apps/v1", "true", "DaemonSet"]
        );
        assert_eq!(
            split_columns("\tresource.k8s.io/v1\tfalse\tDeviceClass"),
            ["", "resource.k8s.io/v1", "false", "DeviceClass"],
            "a leading empty cell is preserved (the row's missing short-name column)"
        );
        assert_eq!(split_columns("Switch branches"), ["Switch branches"]);
    }

    #[test]
    fn column_layout_keeps_widest_cell_per_column_with_empty_cells() {
        // Column 0 is the name; columns 1.. are the `\t` cells. The third
        // row has an EMPTY short-name cell, so its apiversion stays in the
        // apiversion column (not shifted into the short-name column).
        let cands = vec![
            cand_desc("daemonsets", "ds\tapps/v1\ttrue\tDaemonSet"),
            cand_desc("deployments", "deploy\tapps/v1\ttrue\tDeployment"),
            cand_desc("deviceclasses", "\tresource.k8s.io/v1\tfalse\tDeviceClass"),
        ];
        let layout = ColumnLayout::compute(&cands);
        // col0 = max name = "deviceclasses" (13). col1 (short) = max(ds,
        // deploy, "") = 6. col2 (apiversion) = max(apps/v1, resource.k8s.io/v1) = 18.
        assert_eq!(layout.ncols(), 5);
        assert_eq!(layout.col_start(0, 2), 0);
        assert_eq!(
            layout.col_start(1, 2),
            13 + 2,
            "descriptions start after the widest name + gap"
        );
        assert_eq!(layout.col_start(2, 2), 13 + 2 + 6 + 2, "apiversion column after name + short");
    }

    #[test]
    fn column_layout_single_description_is_two_columns() {
        // A normal (non-tabular) completion: name + one-cell description.
        let cands = vec![cand_desc("checkout", "Switch branches")];
        let layout = ColumnLayout::compute(&cands);
        assert_eq!(layout.ncols(), 2);
        assert_eq!(layout.total_chars(2), 8 + 2 + "Switch branches".len());
    }

    #[test]
    fn column_layout_no_descriptions_is_one_column() {
        let cands = vec![cand("alpha"), cand("longer-name")];
        let layout = ColumnLayout::compute(&cands);
        assert_eq!(layout.ncols(), 1);
        assert_eq!(
            layout.total_chars(2),
            "longer-name".len(),
            "no inter-column gap for one column"
        );
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
    fn accept_replaces_typed_token_and_appends_trailing_space() {
        let mut e = PromptEditor::new();
        e.insert_str("ls Ca");
        // Token "Ca" starts at byte 3, length 2.
        let p = CompletionPopup::new(3, "Ca", vec![cand("Cargo.toml")]).unwrap();
        p.accept(&mut e, 2);
        // Trailing space so the line is ready for the next argument.
        assert_eq!(e.text(), "ls Cargo.toml ");
        // Caret lands at the end (after the space).
        assert_eq!(e.cursor(), e.len_bytes());
    }

    #[test]
    fn accept_directory_value_keeps_no_trailing_space() {
        // A directory (value ends with `/`) is NOT space-suffixed — the
        // user keeps completing into the path.
        let mut e = PromptEditor::new();
        e.insert_str("cd sr");
        let p = CompletionPopup::new(3, "sr", vec![cand("src/")]).unwrap();
        p.accept(&mut e, 2);
        assert_eq!(e.text(), "cd src/");
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
        assert_eq!(e.text(), "hello ");
    }
}
