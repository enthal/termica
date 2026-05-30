//! `Ctrl+R` history overlay: a floating popup that shows a fuzzy /
//! substring match over `runs` and lets the user pick one to drop
//! into the editor.
//!
//! State + key handling are pure. Rendering ([`paint`]) is a thin
//! `egui` wrapper that knows nothing the state machine doesn't.
//!
//! Scope toggle (`Tab`): `Global` ↔ `Pane`. Both queries use the
//! same store; ranking lives in [`crate::history::search`].

#![forbid(unsafe_code)]

use eframe::egui;

use crate::history::{Entry, HistoryContext, Scope, rank_entries};
use crate::pane_slot::PaneSlot;

/// Which slice of `runs` to draw from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayScope {
    /// Every row. Default.
    Global,
    /// `(pane_id, app_run_id)` slice — same shape as `↑` / `↓` recall.
    Pane,
}

impl OverlayScope {
    /// The other variant. Used by the `Tab` toggle.
    pub fn flipped(self) -> Self {
        match self {
            Self::Global => Self::Pane,
            Self::Pane => Self::Global,
        }
    }
}

/// One-shot intent returned by [`paint`]. The caller applies it:
/// replace the editor buffer, close the overlay, toggle scope
/// (which also re-fetches the candidate set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayAction {
    /// Replace the editor buffer with `text` and close the overlay.
    Submit(String),
    /// Close the overlay without changing the editor.
    Cancel,
    /// Flip `OverlayScope` and refresh entries. The caller owns
    /// both because refresh needs the `HistoryContext` and `pane_id`
    /// which the overlay state doesn't carry.
    ToggleScope,
}

/// Live state of the overlay.
#[derive(Debug)]
pub struct HistoryOverlay {
    pub query: String,
    pub scope: OverlayScope,
    /// Index into [`Self::ranked`]. `0` is the top of the list.
    pub selected: usize,
    /// Last fetched candidate set under [`Self::scope`]. Reused
    /// across keystrokes; only re-queried when scope changes or
    /// when the overlay opens.
    pub cached_entries: Vec<Entry>,
    /// Indices into `cached_entries`, ordered best-first.
    /// Recomputed on every query change.
    pub ranked: Vec<usize>,
}

impl HistoryOverlay {
    /// Open with an empty query in `Global` scope and a freshly-
    /// fetched candidate set. Returns `None` if the history store
    /// can't be locked or the query fails — the overlay will not
    /// open in that case and the caller should keep the editor as-is.
    pub fn open(history: &HistoryContext, pane_id: u64) -> Option<Self> {
        let mut overlay = Self {
            query: String::new(),
            scope: OverlayScope::Global,
            selected: 0,
            cached_entries: Vec::new(),
            ranked: Vec::new(),
        };
        overlay.refresh_entries(history, pane_id)?;
        overlay.rerank(None);
        Some(overlay)
    }

    /// Refresh `cached_entries` from the store under the current
    /// scope. Called on open and after a scope toggle. Returns
    /// `Some(())` on success; `None` if the lock or query failed.
    pub fn refresh_entries(&mut self, history: &HistoryContext, pane_id: u64) -> Option<()> {
        let store = history.store.lock().ok()?;
        let entries = match self.scope {
            OverlayScope::Global => store.recent(&Scope::Global, 2000).ok()?,
            OverlayScope::Pane => store
                .recent(
                    &Scope::Pane { pane_id: pane_id as i64, app_run_id: &history.app_run_id },
                    2000,
                )
                .ok()?,
        };
        self.cached_entries = dedupe_by_text(entries);
        Some(())
    }

    /// Recompute `ranked` against `query` + `current_cwd`. Resets
    /// `selected` to 0 so the highlight tracks the top result.
    pub fn rerank(&mut self, current_cwd: Option<&str>) {
        self.ranked = rank_entries(&self.cached_entries, &self.query, current_cwd, 200);
        self.selected = 0;
    }

    /// Index into `cached_entries` for the currently highlighted
    /// result, if any.
    pub fn selected_entry_idx(&self) -> Option<usize> {
        self.ranked.get(self.selected).copied()
    }

    /// The text of the currently highlighted entry, if any.
    pub fn selected_text(&self) -> Option<&str> {
        self.selected_entry_idx().and_then(|i| self.cached_entries.get(i)).map(|e| e.text.as_str())
    }

    /// Advance the selection down (toward older results), clamped
    /// at the last row. Exposed for testing the bound; also used
    /// by `paint` via [`egui::Key::ArrowDown`].
    pub fn move_down(&mut self) {
        if !self.ranked.is_empty() {
            self.selected = (self.selected + 1).min(self.ranked.len() - 1);
        }
    }

    /// Walk the selection up (toward newer results), clamped at 0.
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Toggle the scope. Caller must follow with `refresh_entries`
    /// + `rerank` to update the candidate set.
    pub fn toggle_scope(&mut self) {
        self.scope = self.scope.flipped();
    }
}

/// Dedupe by command text, keeping the FIRST occurrence (which is
/// the most recent because `recent()` returns newest-first). Without
/// this, replayed shell history files turn a user's "ran `ls` 200
/// times" into 200 list rows.
fn dedupe_by_text(entries: Vec<Entry>) -> Vec<Entry> {
    let mut seen = std::collections::HashSet::new();
    entries.into_iter().filter(|e| seen.insert(e.text.clone())).collect()
}

/// Paint the overlay as a centered floating panel over the pane
/// and return an [`OverlayAction`] when the user submitted /
/// cancelled / asked to toggle scope this frame. Returns `None`
/// when the overlay is closed or stayed open without an event.
///
/// Owns its own input via:
///   - A real interactive `TextEdit::singleline` bound directly to
///     `overlay.query`. egui handles caret + typing + selection.
///   - `ctx.input` checks for navigation keys (`Enter` / `Esc` /
///     `Tab` / `ArrowUp` / `ArrowDown`) — `lock_focus(true)` on the
///     TextEdit keeps `Tab` from triggering focus navigation so we
///     can repurpose it for scope toggle.
///   - Clickable rows in the results list (mouse → Submit).
pub fn paint(ui: &mut egui::Ui, slot: &mut PaneSlot) -> Option<OverlayAction> {
    let pane_id = slot.session.pane_id();
    let current_cwd = slot.session.terminal().cwd().map(|p| p.display().to_string());
    let overlay = slot.ui.history_overlay.as_mut()?;
    paint_overlay(ui, overlay, pane_id, current_cwd.as_deref())
}

/// The renderable inner of [`paint`], split out so snapshot tests
/// can drive it with a synthetic `HistoryOverlay` instead of
/// standing up a real `PaneSlot` (which needs a PTY). Same
/// signature shape — input keys, return one [`OverlayAction`].
pub fn paint_overlay(
    ui: &mut egui::Ui,
    overlay: &mut HistoryOverlay,
    pane_id: u64,
    current_cwd: Option<&str>,
) -> Option<OverlayAction> {
    let area_id = ui.id().with(("history-overlay", pane_id));
    let screen_rect = ui.ctx().content_rect();
    let panel_w = (screen_rect.width() * 0.6).clamp(360.0, 720.0);
    let scope_text = scope_label(overlay.scope).to_string();
    let mut action: Option<OverlayAction> = None;

    egui::Area::new(area_id)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, screen_rect.height() * 0.12))
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(panel_w);
                ui.horizontal(|ui| {
                    ui.strong("Ctrl-R");
                    ui.label(format!("scope: {scope_text}"));
                    ui.weak("(Tab toggles scope · Enter submits · Esc cancels)");
                });
                let prev_query = overlay.query.clone();
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut overlay.query)
                        .desired_width(panel_w - 16.0)
                        .id_salt(("history-overlay-query", pane_id))
                        .hint_text("search…")
                        .lock_focus(true),
                );
                if !resp.has_focus() {
                    resp.request_focus();
                }
                if overlay.query != prev_query {
                    overlay.rerank(current_cwd);
                }

                // Navigation keys. `ctx.input` is the source of
                // truth for "was this key pressed this frame";
                // checking through the TextEdit response would
                // miss Esc / Tab because the TextEdit doesn't
                // consume those.
                let (enter, escape, tab, arrow_down, arrow_up) = ui.ctx().input(|i| {
                    (
                        i.key_pressed(egui::Key::Enter),
                        i.key_pressed(egui::Key::Escape),
                        i.key_pressed(egui::Key::Tab),
                        i.key_pressed(egui::Key::ArrowDown),
                        i.key_pressed(egui::Key::ArrowUp),
                    )
                });
                if enter {
                    action = Some(match overlay.selected_text() {
                        Some(t) => OverlayAction::Submit(t.to_string()),
                        None => OverlayAction::Cancel,
                    });
                } else if escape {
                    action = Some(OverlayAction::Cancel);
                } else if tab {
                    action = Some(OverlayAction::ToggleScope);
                } else {
                    if arrow_down {
                        overlay.move_down();
                    }
                    if arrow_up {
                        overlay.move_up();
                    }
                }

                ui.separator();

                egui::ScrollArea::vertical()
                    .id_salt(("history-overlay-results", pane_id))
                    .max_height(screen_rect.height() * 0.5)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if overlay.ranked.is_empty() {
                            ui.weak("(no matches)");
                            return;
                        }
                        // Snapshot the row data so the result loop
                        // doesn't borrow `overlay` immutably while
                        // we also assign into `action` (mutable).
                        let rows: Vec<RowView> = overlay
                            .ranked
                            .iter()
                            .enumerate()
                            .filter_map(|(row, &cand)| {
                                let e = overlay.cached_entries.get(cand)?;
                                Some(RowView {
                                    row,
                                    text: e.text.clone(),
                                    cwd: e.cwd.clone(),
                                    exit_code: e.exit_code,
                                    source: e.source.clone(),
                                })
                            })
                            .collect();
                        for r in rows {
                            let is_selected = r.row == overlay.selected;
                            let clicked = paint_clickable_row(
                                ui,
                                &r.text,
                                r.cwd.as_deref(),
                                r.exit_code,
                                &r.source,
                                is_selected,
                                panel_w,
                            );
                            if clicked {
                                action = Some(OverlayAction::Submit(r.text));
                                break;
                            }
                        }
                    });
            });
        });

    action
}

/// Snapshot of one row pulled out of `cached_entries` so the
/// click loop doesn't borrow `overlay` immutably while it also
/// writes the click result back into `action`.
struct RowView {
    row: usize,
    text: String,
    cwd: Option<String>,
    exit_code: Option<i32>,
    source: String,
}

fn scope_label(s: OverlayScope) -> &'static str {
    match s {
        OverlayScope::Global => "global*",
        OverlayScope::Pane => "this pane*",
    }
}

/// Render one result row as a clickable area. Returns `true`
/// when the user clicked it.
fn paint_clickable_row(
    ui: &mut egui::Ui,
    text: &str,
    cwd: Option<&str>,
    exit_code: Option<i32>,
    source: &str,
    is_selected: bool,
    panel_w: f32,
) -> bool {
    let frame = if is_selected {
        egui::Frame::NONE.fill(ui.visuals().selection.bg_fill.linear_multiply(0.35))
    } else {
        egui::Frame::NONE
    };
    let inner = frame.show(ui, |ui| {
        ui.set_width(panel_w);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(text).monospace().strong());
            let meta = format_meta(cwd, exit_code, source);
            if !meta.is_empty() {
                ui.weak(meta);
            }
        });
    });
    let row_id = ui.id().with(("hist-row", text));
    ui.interact(inner.response.rect, row_id, egui::Sense::click()).clicked()
}

fn format_meta(cwd: Option<&str>, exit_code: Option<i32>, source: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(c) = cwd {
        parts.push(c.to_string());
    }
    if let Some(code) = exit_code {
        parts.push(format!("exit {code}"));
    }
    if source != "termica" {
        parts.push(source.to_string());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(text: &str, ts: i64) -> Entry {
        Entry {
            id: ts,
            text: text.to_string(),
            started_at_ms: ts,
            finished_at_ms: None,
            exit_code: None,
            cwd: None,
            app_run_id: None,
            pane_id: None,
            source: "termica".to_string(),
        }
    }

    fn overlay_with(entries: Vec<Entry>) -> HistoryOverlay {
        let mut o = HistoryOverlay {
            query: String::new(),
            scope: OverlayScope::Global,
            selected: 0,
            cached_entries: entries,
            ranked: Vec::new(),
        };
        o.rerank(None);
        o
    }

    #[test]
    fn empty_query_lists_all_entries_newest_first() {
        let entries =
            vec![make_entry("third", 300), make_entry("second", 200), make_entry("first", 100)];
        let o = overlay_with(entries);
        assert_eq!(o.ranked, vec![0, 1, 2]);
        assert_eq!(o.selected_text(), Some("third"));
    }

    #[test]
    fn rerank_filters_via_substring() {
        let entries = vec![
            make_entry("cargo run", 300),
            make_entry("ls", 200),
            make_entry("cargo test", 100),
        ];
        let mut o = overlay_with(entries);
        o.query = "cargo".to_string();
        o.rerank(None);
        let texts: Vec<&str> =
            o.ranked.iter().map(|i| o.cached_entries[*i].text.as_str()).collect();
        assert_eq!(texts, vec!["cargo run", "cargo test"]);
    }

    #[test]
    fn move_down_clamps_to_last_row() {
        let entries = vec![make_entry("a", 200), make_entry("b", 100)];
        let mut o = overlay_with(entries);
        o.move_down();
        assert_eq!(o.selected, 1);
        o.move_down();
        assert_eq!(o.selected, 1, "clamped to last index");
    }

    #[test]
    fn move_up_clamps_to_zero() {
        let entries = vec![make_entry("a", 200), make_entry("b", 100)];
        let mut o = overlay_with(entries);
        o.move_down();
        o.move_up();
        assert_eq!(o.selected, 0);
        o.move_up();
        assert_eq!(o.selected, 0, "clamped at 0");
    }

    #[test]
    fn move_down_on_empty_results_is_noop() {
        let mut o = overlay_with(vec![make_entry("ls", 100)]);
        o.query = "no-match".to_string();
        o.rerank(None);
        assert!(o.ranked.is_empty());
        o.move_down();
        assert_eq!(o.selected, 0);
    }

    #[test]
    fn rerank_resets_selection_to_top() {
        let entries = vec![make_entry("a", 300), make_entry("b", 200), make_entry("c", 100)];
        let mut o = overlay_with(entries);
        o.move_down();
        o.move_down();
        assert_eq!(o.selected, 2);
        o.query = "a".to_string();
        o.rerank(None);
        assert_eq!(o.selected, 0);
    }

    #[test]
    fn toggle_scope_flips_the_field() {
        let mut o = overlay_with(vec![]);
        assert_eq!(o.scope, OverlayScope::Global);
        o.toggle_scope();
        assert_eq!(o.scope, OverlayScope::Pane);
        o.toggle_scope();
        assert_eq!(o.scope, OverlayScope::Global);
    }

    #[test]
    fn dedupe_keeps_first_occurrence_of_each_text() {
        // `recent()` returns newest-first, so the first occurrence
        // IS the most recent — keep it. Replayed shell-history
        // files with `ls` ten times must collapse to one row.
        let entries = vec![
            make_entry("ls", 500),
            make_entry("cd", 400),
            make_entry("ls", 300),
            make_entry("ls", 200),
            make_entry("cd", 100),
        ];
        let deduped = dedupe_by_text(entries);
        let texts: Vec<&str> = deduped.iter().map(|e| e.text.as_str()).collect();
        let ts: Vec<i64> = deduped.iter().map(|e| e.started_at_ms).collect();
        assert_eq!(texts, vec!["ls", "cd"]);
        assert_eq!(ts, vec![500, 400], "kept the newest occurrence of each");
    }
}
