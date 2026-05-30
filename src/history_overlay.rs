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
    let now_ms = wall_clock_ms();
    let overlay = slot.ui.history_overlay.as_mut()?;
    paint_overlay(ui, overlay, pane_id, current_cwd.as_deref(), now_ms)
}

/// Wall-clock unix-ms for the production paint path. Tests use
/// `paint_overlay` directly and supply a deterministic `now_ms`.
fn wall_clock_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
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
    now_ms: i64,
) -> Option<OverlayAction> {
    let area_id = ui.id().with(("history-overlay", pane_id));
    let screen_rect = ui.ctx().content_rect();
    let panel_w = (screen_rect.width() * 0.7).clamp(560.0, 1100.0);
    let panel_min_h = (screen_rect.height() * 0.45).clamp(280.0, 520.0);
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
                    .min_scrolled_height(panel_min_h)
                    .max_height(screen_rect.height() * 0.6)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if overlay.ranked.is_empty() {
                            ui.weak("(no matches)");
                            return;
                        }
                        // Snapshot the row data so the result loop
                        // doesn't borrow `overlay` immutably while
                        // we also assign into `action` (mutable).
                        let query = overlay.query.clone();
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
                                    started_at_ms: e.started_at_ms,
                                })
                            })
                            .collect();
                        for r in rows {
                            let is_selected = r.row == overlay.selected;
                            let clicked = paint_clickable_row(
                                ui,
                                &r.text,
                                &query,
                                r.cwd.as_deref(),
                                r.exit_code,
                                &r.source,
                                r.started_at_ms,
                                now_ms,
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
    started_at_ms: i64,
}

fn scope_label(s: OverlayScope) -> &'static str {
    match s {
        OverlayScope::Global => "global*",
        OverlayScope::Pane => "this pane*",
    }
}

/// Render one result row as a clickable area. The row body shows
/// the command (with matching substring highlighted) followed by a
/// meta line: compact age (with the full age string as a hover
/// tooltip), then cwd, exit code, and source tag (`zsh`/`bash`/`fish`)
/// when present. Returns `true` when the user clicked the row.
#[allow(clippy::too_many_arguments)]
fn paint_clickable_row(
    ui: &mut egui::Ui,
    text: &str,
    query: &str,
    cwd: Option<&str>,
    exit_code: Option<i32>,
    source: &str,
    started_at_ms: i64,
    now_ms: i64,
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
            // Command text — highlight the matched substring.
            let job = highlight_layout_job(
                text,
                query,
                command_base_format(ui),
                command_match_format(ui),
            );
            ui.label(job);

            // Meta line: age (with hover for full) · cwd · exit · source.
            let (age_short, age_full) = format_age(now_ms, started_at_ms);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.weak(age_short).on_hover_text(age_full);
                if let Some(c) = cwd {
                    ui.weak("·");
                    ui.weak(c);
                }
                if let Some(code) = exit_code {
                    ui.weak("·");
                    ui.weak(format!("exit {code}"));
                }
                if source != "termica" {
                    ui.weak("·");
                    ui.weak(source);
                }
            });
        });
    });
    let row_id = ui.id().with(("hist-row", text, started_at_ms));
    ui.interact(inner.response.rect, row_id, egui::Sense::click()).clicked()
}

fn command_base_format(ui: &egui::Ui) -> egui::TextFormat {
    egui::TextFormat {
        font_id: egui::FontId::monospace(13.0),
        color: ui.visuals().strong_text_color(),
        ..Default::default()
    }
}

fn command_match_format(ui: &egui::Ui) -> egui::TextFormat {
    egui::TextFormat {
        font_id: egui::FontId::monospace(13.0),
        color: ui.visuals().selection.bg_fill,
        underline: egui::Stroke::new(1.0, ui.visuals().selection.bg_fill),
        ..Default::default()
    }
}

/// Build a [`LayoutJob`] for `text` where every case-insensitive
/// occurrence of `query` is rendered with `match_format` and the
/// rest with `base_format`. Empty query → the whole string in
/// `base_format`. Public-in-module for testability; the matching
/// is byte-substring on the lowercase form, with a char-boundary
/// guard so non-ASCII text that happens to change byte length on
/// lowercase falls back to the plain base format instead of
/// panicking.
fn highlight_layout_job(
    text: &str,
    query: &str,
    base: egui::TextFormat,
    hit: egui::TextFormat,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    if query.is_empty() {
        job.append(text, 0.0, base);
        return job;
    }
    let needle = query.to_lowercase();
    let hay = text.to_lowercase();
    if hay.len() != text.len() {
        // Lowercase changed byte length somewhere — slicing the
        // original by lowercase byte offsets is no longer safe.
        // Render plain.
        job.append(text, 0.0, base);
        return job;
    }
    let n_len = needle.len();
    let mut cursor = 0;
    while cursor < text.len() {
        match hay[cursor..].find(&needle) {
            Some(rel) => {
                let abs = cursor + rel;
                let end = abs + n_len;
                if !text.is_char_boundary(abs) || !text.is_char_boundary(end) {
                    job.append(&text[cursor..], 0.0, base);
                    return job;
                }
                if abs > cursor {
                    job.append(&text[cursor..abs], 0.0, base.clone());
                }
                job.append(&text[abs..end], 0.0, hit.clone());
                cursor = end;
            }
            None => {
                job.append(&text[cursor..], 0.0, base);
                break;
            }
        }
    }
    job
}

/// Render a unix-ms timestamp relative to `now_ms` as `(short, full)`.
/// `short` is the compact label that goes in the row meta line
/// (e.g. "4m"); `full` is the long-form tooltip the user gets on
/// hover (e.g. "4 minutes ago"). Determinism rule from spec/09:
/// `now_ms` is injected, not read from the system clock.
pub fn format_age(now_ms: i64, then_ms: i64) -> (String, String) {
    let secs = (now_ms - then_ms).max(0) / 1000;
    if secs < 60 {
        return ("now".to_string(), "just now".to_string());
    }
    let mins = secs / 60;
    if mins < 60 {
        return (format!("{mins}m"), pluralize(mins, "minute"));
    }
    let hours = mins / 60;
    if hours < 24 {
        return (format!("{hours}h"), pluralize(hours, "hour"));
    }
    let days = hours / 24;
    if days == 1 {
        return ("1d".to_string(), "yesterday".to_string());
    }
    if days < 7 {
        return (format!("{days}d"), pluralize(days, "day"));
    }
    if days < 30 {
        let weeks = days / 7;
        return (format!("{weeks}w"), pluralize(weeks, "week"));
    }
    if days < 365 {
        let months = days / 30;
        return (format!("{months}mo"), pluralize(months, "month"));
    }
    let years = days / 365;
    (format!("{years}y"), pluralize(years, "year"))
}

fn pluralize(n: i64, unit: &str) -> String {
    if n == 1 { format!("1 {unit} ago") } else { format!("{n} {unit}s ago") }
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

    // ---- format_age ---------------------------------------------------

    fn ms(secs: i64) -> i64 {
        secs * 1000
    }

    #[test]
    fn age_under_a_minute_renders_now() {
        assert_eq!(format_age(ms(100), ms(70)), ("now".into(), "just now".into()));
        assert_eq!(format_age(ms(100), ms(41)), ("now".into(), "just now".into()));
    }

    #[test]
    fn age_minutes_singular_and_plural() {
        assert_eq!(format_age(ms(120), ms(60)), ("1m".into(), "1 minute ago".into()));
        assert_eq!(format_age(ms(60 * 4), ms(0)), ("4m".into(), "4 minutes ago".into()));
        assert_eq!(format_age(ms(60 * 59), ms(0)), ("59m".into(), "59 minutes ago".into()));
    }

    #[test]
    fn age_hours_singular_and_plural() {
        assert_eq!(format_age(ms(60 * 60), ms(0)), ("1h".into(), "1 hour ago".into()));
        assert_eq!(format_age(ms(60 * 60 * 3), ms(0)), ("3h".into(), "3 hours ago".into()));
    }

    #[test]
    fn age_one_day_renders_yesterday() {
        let one_day = 60 * 60 * 24;
        assert_eq!(format_age(ms(one_day), ms(0)), ("1d".into(), "yesterday".into()));
    }

    #[test]
    fn age_two_to_six_days_render_as_days() {
        let day = 60 * 60 * 24;
        assert_eq!(format_age(ms(day * 3), ms(0)), ("3d".into(), "3 days ago".into()));
        assert_eq!(format_age(ms(day * 6), ms(0)), ("6d".into(), "6 days ago".into()));
    }

    #[test]
    fn age_weeks_singular_and_plural() {
        let day = 60 * 60 * 24;
        assert_eq!(format_age(ms(day * 7), ms(0)), ("1w".into(), "1 week ago".into()));
        assert_eq!(format_age(ms(day * 14), ms(0)), ("2w".into(), "2 weeks ago".into()));
    }

    #[test]
    fn age_months_and_years() {
        let day = 60 * 60 * 24;
        assert_eq!(format_age(ms(day * 30), ms(0)), ("1mo".into(), "1 month ago".into()));
        assert_eq!(format_age(ms(day * 90), ms(0)), ("3mo".into(), "3 months ago".into()));
        assert_eq!(format_age(ms(day * 365), ms(0)), ("1y".into(), "1 year ago".into()));
        assert_eq!(format_age(ms(day * 365 * 2), ms(0)), ("2y".into(), "2 years ago".into()));
    }

    #[test]
    fn age_future_timestamps_clamp_to_now() {
        // Defensive: if `then_ms > now_ms` (clock skew), render
        // "now" rather than a negative duration.
        assert_eq!(format_age(ms(100), ms(500)), ("now".into(), "just now".into()));
    }

    // ---- highlight_layout_job -----------------------------------------

    fn base_fmt() -> egui::TextFormat {
        egui::TextFormat::default()
    }

    fn hit_fmt() -> egui::TextFormat {
        egui::TextFormat {
            color: egui::Color32::RED,
            underline: egui::Stroke::new(1.0, egui::Color32::RED),
            ..Default::default()
        }
    }

    /// Extract the (text, color) of each section so the test can
    /// assert which runs got the match treatment without touching
    /// egui font metrics.
    fn job_sections(job: &egui::text::LayoutJob) -> Vec<(String, bool)> {
        job.sections
            .iter()
            .map(|s| {
                let text = job.text[s.byte_range.clone()].to_string();
                let highlighted = s.format.color == egui::Color32::RED;
                (text, highlighted)
            })
            .collect()
    }

    #[test]
    fn highlight_empty_query_is_one_base_section() {
        let job = highlight_layout_job("echo hello", "", base_fmt(), hit_fmt());
        assert_eq!(job_sections(&job), vec![("echo hello".into(), false)]);
    }

    #[test]
    fn highlight_matches_case_insensitively() {
        let job = highlight_layout_job("echo $PWD", "pwd", base_fmt(), hit_fmt());
        assert_eq!(job_sections(&job), vec![("echo $".into(), false), ("PWD".into(), true)]);
    }

    #[test]
    fn highlight_matches_multiple_occurrences() {
        let job = highlight_layout_job("foo bar foo baz foo", "foo", base_fmt(), hit_fmt());
        assert_eq!(
            job_sections(&job),
            vec![
                ("foo".into(), true),
                (" bar ".into(), false),
                ("foo".into(), true),
                (" baz ".into(), false),
                ("foo".into(), true),
            ]
        );
    }

    #[test]
    fn highlight_no_match_is_one_base_section() {
        let job = highlight_layout_job("ls -la", "xyz", base_fmt(), hit_fmt());
        assert_eq!(job_sections(&job), vec![("ls -la".into(), false)]);
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
