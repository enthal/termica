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

/// What `key()` decided to do with the keystroke. The caller
/// applies it: substitute, close, or just consume the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayAction {
    /// Replace the editor buffer with `text` and close the overlay.
    Submit(String),
    /// Close the overlay without changing the editor.
    Cancel,
    /// The overlay handled the key; nothing else to do.
    Handled,
    /// The overlay did not consume the key — the caller should
    /// route it elsewhere (in practice: ignore it; the overlay is
    /// modal and swallows everything except its own keys).
    Pass,
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
        self.cached_entries = entries;
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

    /// One key in, one [`OverlayAction`] out. Pure: doesn't query
    /// the store or refresh anything — the caller does any follow-
    /// up work (scope toggle → `refresh_entries` + `rerank`).
    pub fn on_key(&mut self, key: egui::Key, current_cwd: Option<&str>) -> OverlayAction {
        match key {
            egui::Key::Enter => match self.selected_text() {
                Some(t) => OverlayAction::Submit(t.to_string()),
                None => OverlayAction::Cancel,
            },
            egui::Key::Escape => OverlayAction::Cancel,
            egui::Key::ArrowDown => {
                if !self.ranked.is_empty() {
                    self.selected = (self.selected + 1).min(self.ranked.len() - 1);
                }
                OverlayAction::Handled
            }
            egui::Key::ArrowUp => {
                self.selected = self.selected.saturating_sub(1);
                OverlayAction::Handled
            }
            egui::Key::Backspace => {
                self.query.pop();
                self.rerank(current_cwd);
                OverlayAction::Handled
            }
            _ => OverlayAction::Pass,
        }
    }

    /// Apply a printable text event (chars the user typed).
    /// Appends to query and reranks. Bound separately from
    /// `on_key` because egui delivers them as `Event::Text`.
    pub fn on_text(&mut self, s: &str, current_cwd: Option<&str>) -> OverlayAction {
        self.query.push_str(s);
        self.rerank(current_cwd);
        OverlayAction::Handled
    }

    /// Toggle the scope. Caller must follow with `refresh_entries`
    /// + `rerank` to update the candidate set.
    pub fn toggle_scope(&mut self) {
        self.scope = self.scope.flipped();
    }
}

/// Paint the overlay as a centered floating panel over the pane.
/// No-op if the slot doesn't have an open overlay.
///
/// This is the only place that touches `egui::Area` / `egui::Frame`
/// / fonts; the rest of the module is engine-pure. Pragmatic-layer
/// per spec/09 — the logic above is what the strict-layer tests
/// cover.
pub fn paint(ui: &mut egui::Ui, slot: &mut PaneSlot) {
    let Some(overlay) = slot.ui.history_overlay.as_ref() else { return };
    let area_id = ui.id().with(("history-overlay", slot.session.pane_id()));
    let screen_rect = ui.ctx().content_rect();
    let panel_w = (screen_rect.width() * 0.6).clamp(360.0, 720.0);

    egui::Area::new(area_id)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, screen_rect.height() * 0.12))
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(panel_w);
                ui.horizontal(|ui| {
                    ui.strong("Ctrl-R");
                    ui.label(format!("scope: {}", scope_label(overlay.scope)));
                    ui.label("(Tab to toggle)");
                });
                ui.horizontal(|ui| {
                    ui.label("search:");
                    let mut q = overlay.query.clone();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut q)
                            .desired_width(panel_w - 80.0)
                            .interactive(false),
                    );
                    resp.request_focus();
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(screen_rect.height() * 0.5)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if overlay.ranked.is_empty() {
                            ui.weak("(no matches)");
                            return;
                        }
                        for (row, &cand) in overlay.ranked.iter().enumerate() {
                            let Some(entry) = overlay.cached_entries.get(cand) else {
                                continue;
                            };
                            let is_selected = row == overlay.selected;
                            paint_row(ui, entry, is_selected, panel_w);
                        }
                    });
            });
        });
}

fn scope_label(s: OverlayScope) -> &'static str {
    match s {
        OverlayScope::Global => "global*",
        OverlayScope::Pane => "this pane*",
    }
}

fn paint_row(ui: &mut egui::Ui, entry: &Entry, is_selected: bool, panel_w: f32) {
    let bg =
        if is_selected { Some(ui.visuals().selection.bg_fill.linear_multiply(0.4)) } else { None };
    let frame = if let Some(c) = bg { egui::Frame::NONE.fill(c) } else { egui::Frame::NONE };
    frame.show(ui, |ui| {
        ui.set_width(panel_w);
        ui.vertical(|ui| {
            let text = egui::RichText::new(&entry.text).monospace().strong();
            ui.label(text);
            let meta = format_meta(entry);
            if !meta.is_empty() {
                ui.weak(meta);
            }
        });
    });
}

fn format_meta(entry: &Entry) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(cwd) = entry.cwd.as_deref() {
        parts.push(cwd.to_string());
    }
    if let Some(code) = entry.exit_code {
        parts.push(format!("exit {code}"));
    }
    if entry.source != "termica" {
        parts.push(format!("· {}", entry.source));
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
        // Cached entries are already newest-first (DB returns them
        // that way) so rerank with "" preserves that order.
        let entries =
            vec![make_entry("third", 300), make_entry("second", 200), make_entry("first", 100)];
        let o = overlay_with(entries);
        assert_eq!(o.ranked, vec![0, 1, 2]);
        assert_eq!(o.selected_text(), Some("third"));
    }

    #[test]
    fn typing_filters_via_substring() {
        let entries = vec![
            make_entry("cargo run", 300),
            make_entry("ls", 200),
            make_entry("cargo test", 100),
        ];
        let mut o = overlay_with(entries);
        o.on_text("cargo", None);
        let texts: Vec<&str> =
            o.ranked.iter().map(|i| o.cached_entries[*i].text.as_str()).collect();
        assert_eq!(texts, vec!["cargo run", "cargo test"]);
    }

    #[test]
    fn backspace_widens_filter() {
        let entries = vec![make_entry("alpha", 200), make_entry("beta", 100)];
        let mut o = overlay_with(entries);
        o.on_text("alph", None);
        assert_eq!(o.ranked.len(), 1);
        o.on_key(egui::Key::Backspace, None);
        assert_eq!(o.query, "alp");
        assert_eq!(o.ranked.len(), 1);
        for _ in 0..3 {
            o.on_key(egui::Key::Backspace, None);
        }
        assert_eq!(o.query, "");
        assert_eq!(o.ranked.len(), 2);
    }

    #[test]
    fn arrow_down_advances_selection_clamped_to_last_row() {
        let entries = vec![make_entry("a", 200), make_entry("b", 100)];
        let mut o = overlay_with(entries);
        assert_eq!(o.selected, 0);
        o.on_key(egui::Key::ArrowDown, None);
        assert_eq!(o.selected, 1);
        o.on_key(egui::Key::ArrowDown, None);
        assert_eq!(o.selected, 1, "clamped to last index");
    }

    #[test]
    fn arrow_up_decreases_selection_clamped_to_zero() {
        let entries = vec![make_entry("a", 200), make_entry("b", 100)];
        let mut o = overlay_with(entries);
        o.on_key(egui::Key::ArrowDown, None);
        o.on_key(egui::Key::ArrowUp, None);
        assert_eq!(o.selected, 0);
        o.on_key(egui::Key::ArrowUp, None);
        assert_eq!(o.selected, 0);
    }

    #[test]
    fn enter_submits_selected_entry() {
        let entries = vec![make_entry("only", 100)];
        let mut o = overlay_with(entries);
        let action = o.on_key(egui::Key::Enter, None);
        assert_eq!(action, OverlayAction::Submit("only".to_string()));
    }

    #[test]
    fn enter_on_empty_results_cancels() {
        let mut o = overlay_with(vec![make_entry("ls", 100)]);
        o.on_text("xyz", None);
        let action = o.on_key(egui::Key::Enter, None);
        assert_eq!(action, OverlayAction::Cancel);
    }

    #[test]
    fn escape_cancels() {
        let mut o = overlay_with(vec![make_entry("ls", 100)]);
        assert_eq!(o.on_key(egui::Key::Escape, None), OverlayAction::Cancel);
    }

    #[test]
    fn rerank_resets_selection_to_top() {
        let entries = vec![make_entry("a", 300), make_entry("b", 200), make_entry("c", 100)];
        let mut o = overlay_with(entries);
        o.on_key(egui::Key::ArrowDown, None);
        o.on_key(egui::Key::ArrowDown, None);
        assert_eq!(o.selected, 2);
        o.on_text("a", None);
        // After rerank, selection is back to 0 (the top result).
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
    fn unknown_key_returns_pass() {
        let mut o = overlay_with(vec![make_entry("ls", 100)]);
        let action = o.on_key(egui::Key::Tab, None);
        assert_eq!(action, OverlayAction::Pass);
    }
}
