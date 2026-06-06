//! Phase 8 — in-pane find (`Cmd/Ctrl+F`).
//!
//! A floating overlay bar at the top of the pane that searches the
//! pane's **sealed** command blocks (command lines + frozen output
//! snapshots) and paints match highlights over the cell grid. See
//! [spec/07 §"Search"](../spec/07-history-and-search.md#search).
//!
//! ## What lives here
//!
//! - The **pure search core**: [`collect_searchable_lines`] flattens a
//!   [`BlockStack`] into [`SearchableLine`]s; [`build_matcher`] +
//!   [`find_matches`] turn a query into char-column [`SearchMatch`]es.
//!   All of it is UI-free and unit-tested without egui.
//! - The **overlay state machine** ([`FindOverlay`]): the query, the
//!   `Aa` / `.*` / filter toggles, the ordered match list + current
//!   selection, and the per-overlay query history walked with `↑`/`↓`.
//!   Every transition is a pure method.
//! - The **render** ([`paint_overlay`]): a thin egui wrapper, split out
//!   like [`crate::history_overlay::paint_overlay`] so snapshot tests
//!   can drive it with a synthetic [`FindOverlay`] and no real pane.
//!
//! ## Scope (v1)
//!
//! Search covers the focused pane's sealed blocks only — the live
//! `Prompt` / `Running` tail is excluded (its output isn't frozen
//! yet). Cross-pane (`CurrentTab`) and `SelectedBlocks` scopes from
//! spec/07 are deferred; `SelectedBlocks` additionally needs block-
//! object selection ([#120](https://github.com/enthal/termica/issues/120)).
//!
//! Match columns are **character columns**, which equal cell columns
//! in the monospaced grid — so a `(row, col_start, col_end)` range
//! maps straight to a highlight rectangle in [`crate::render`].

#![forbid(unsafe_code)]

use eframe::egui;

use crate::block::{Block, BlockId, BlockStack};

/// Which part of a block a line came from. Drives the `All` /
/// `Commands` / `Outputs` filter and which painted widget a match
/// highlights (the teal command label vs the output snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// The user's typed command line (the teal label above output).
    Command,
    /// A row of the frozen output snapshot.
    Output,
}

/// The `All` / `Commands` / `Outputs` filter. Mirrors spec/07's
/// `SearchFilter { Both, CommandOnly, OutputOnly }`; the variant names
/// match the spec, the [`Self::label`] strings match the chip the user
/// sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFilter {
    /// Commands **and** output. Default.
    Both,
    /// Only the command lines.
    CommandOnly,
    /// Only the output snapshots.
    OutputOnly,
}

impl SearchFilter {
    /// Does a line of `kind` participate under this filter?
    pub fn includes(self, kind: LineKind) -> bool {
        match self {
            SearchFilter::Both => true,
            SearchFilter::CommandOnly => kind == LineKind::Command,
            SearchFilter::OutputOnly => kind == LineKind::Output,
        }
    }

    /// The chip label shown in the overlay.
    pub fn label(self) -> &'static str {
        match self {
            SearchFilter::Both => "All",
            SearchFilter::CommandOnly => "Commands",
            SearchFilter::OutputOnly => "Outputs",
        }
    }

    /// Cycle order for the clickable chip: All → Commands → Outputs → All.
    pub fn next(self) -> Self {
        match self {
            SearchFilter::Both => SearchFilter::CommandOnly,
            SearchFilter::CommandOnly => SearchFilter::OutputOnly,
            SearchFilter::OutputOnly => SearchFilter::Both,
        }
    }
}

/// One searchable line pulled out of the block stack: which block it
/// belongs to, whether it's a command or output line, its 0-based row
/// **within its kind** (command rows are indexed against the command's
/// own `split('\n')`; output rows against the snapshot), and the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchableLine {
    pub block_id: BlockId,
    pub kind: LineKind,
    pub row: usize,
    pub text: String,
}

/// One match. The column range is in **character columns** (== cell
/// columns), half-open `[col_start, col_end)`, so it maps directly to
/// a highlight rectangle. `row` / `kind` / `block_id` locate which
/// painted line the highlight goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub block_id: BlockId,
    pub kind: LineKind,
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
}

/// Flatten a pane's [`BlockStack`] into searchable lines. **Sealed
/// blocks only** (v1 scope): the live `Prompt` / `Running` tail is
/// skipped because its output isn't a frozen snapshot yet. Output
/// rows are right-trimmed of the cell-grid's space padding so a query
/// can't match trailing blanks (trimming only the end keeps every
/// earlier column's index — and therefore every highlight rect —
/// exact).
pub fn collect_searchable_lines(blocks: &BlockStack) -> Vec<SearchableLine> {
    let mut out = Vec::new();
    for block in blocks.iter() {
        let Block::Sealed { id, command, snapshot, .. } = block else { continue };
        if !command.is_empty() {
            for (row, line) in command.split('\n').enumerate() {
                out.push(SearchableLine {
                    block_id: *id,
                    kind: LineKind::Command,
                    row,
                    text: line.to_string(),
                });
            }
        }
        for (row, line) in snapshot.iter().enumerate() {
            let text: String = line.text_chars().collect();
            out.push(SearchableLine {
                block_id: *id,
                kind: LineKind::Output,
                row,
                text: text.trim_end_matches(' ').to_string(),
            });
        }
    }
    out
}

/// The two independent match toggles the overlay exposes (`Aa` and
/// `.*`). Combined they cover the spec/07 modes: case-sensitive
/// literal, case-insensitive literal, and regex (which itself honours
/// the case toggle via `RegexBuilder::case_insensitive`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindParams {
    /// `Aa` on → exact case. Off (default) → case-insensitive.
    pub case_sensitive: bool,
    /// `.*` on → treat the query as a regular expression.
    pub regex: bool,
}

/// A compiled query, ready to scan lines. Substring matching is done
/// in char space (so columns line up with cells); regex defers to the
/// `regex` crate and we convert its byte offsets back to char columns.
pub enum Matcher {
    /// Literal substring. `needle` is the raw query chars; `ci` selects
    /// ASCII-case-insensitive comparison.
    Substring { needle: Vec<char>, ci: bool },
    /// A compiled regular expression.
    Regex(regex::Regex),
}

/// Build a [`Matcher`] for `query` under `params`. Returns `Err` only
/// when `params.regex` is set and the pattern doesn't compile — the
/// overlay surfaces that as a `(bad regex)` hint rather than matches.
pub fn build_matcher(query: &str, params: FindParams) -> Result<Matcher, regex::Error> {
    if params.regex {
        regex::RegexBuilder::new(query)
            .case_insensitive(!params.case_sensitive)
            .build()
            .map(Matcher::Regex)
    } else {
        Ok(Matcher::Substring { needle: query.chars().collect(), ci: !params.case_sensitive })
    }
}

/// Char-column ranges of every non-overlapping match of `matcher` in
/// `text`, left-to-right. Empty for an empty needle / empty regex
/// match (a zero-width match would highlight nothing and loop forever).
fn line_match_columns(text: &str, matcher: &Matcher) -> Vec<(usize, usize)> {
    match matcher {
        Matcher::Substring { needle, ci } => substring_char_ranges(text, needle, *ci),
        Matcher::Regex(re) => {
            let mut ranges = Vec::new();
            for m in re.find_iter(text) {
                if m.start() == m.end() {
                    continue;
                }
                // Byte offset → char column.
                let cs = text[..m.start()].chars().count();
                let ce = text[..m.end()].chars().count();
                ranges.push((cs, ce));
            }
            ranges
        }
    }
}

/// Non-overlapping char-column ranges where `needle` occurs in
/// `haystack`. Case-insensitivity is ASCII-only (`eq_ignore_ascii_case`)
/// so the comparison stays 1 char ↔ 1 column — full Unicode case
/// folding can change char counts and would desync highlight columns
/// from the cell grid. Non-ASCII text still matches when the case
/// already agrees.
fn substring_char_ranges(haystack: &str, needle: &[char], ci: bool) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    if needle.is_empty() {
        return ranges;
    }
    let hay: Vec<char> = haystack.chars().collect();
    if needle.len() > hay.len() {
        return ranges;
    }
    let eq = |a: char, b: char| if ci { a.eq_ignore_ascii_case(&b) } else { a == b };
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if (0..needle.len()).all(|k| eq(hay[i + k], needle[k])) {
            ranges.push((i, i + needle.len()));
            i += needle.len(); // non-overlapping
        } else {
            i += 1;
        }
    }
    ranges
}

/// All matches across `lines`, in document order (the order
/// [`collect_searchable_lines`] produced: oldest block first, command
/// before output, left-to-right within a line). `filter` drops lines
/// whose kind is excluded.
pub fn find_matches(
    lines: &[SearchableLine],
    matcher: &Matcher,
    filter: SearchFilter,
) -> Vec<SearchMatch> {
    let mut out = Vec::new();
    for line in lines {
        if !filter.includes(line.kind) {
            continue;
        }
        for (col_start, col_end) in line_match_columns(&line.text, matcher) {
            out.push(SearchMatch {
                block_id: line.block_id,
                kind: line.kind,
                row: line.row,
                col_start,
                col_end,
            });
        }
    }
    out
}

/// How many query strings the per-pane find history keeps.
const HISTORY_CAP: usize = 100;

/// Live state of the find overlay. Created by [`Self::open`] on
/// `Cmd/Ctrl+F` and held in `PaneUiState::find_overlay` while open.
#[derive(Debug, Clone)]
pub struct FindOverlay {
    /// The query the user is editing.
    pub query: String,
    /// `Aa` toggle — exact case when `true`.
    pub case_sensitive: bool,
    /// `.*` toggle — regex when `true`.
    pub regex: bool,
    /// `All` / `Commands` / `Outputs` filter.
    pub filter: SearchFilter,
    /// Matches for the current query, in document order. Recomputed by
    /// [`Self::recompute`] each frame.
    pub matches: Vec<SearchMatch>,
    /// Index into [`Self::matches`] of the "current" match (the bright
    /// highlight + the one `prev`/`next` and scroll-to track).
    pub selected: usize,
    /// `true` when `regex` is on and the pattern failed to compile.
    pub regex_error: bool,
    /// Previously-submitted queries, newest first. Walked with `↑`/`↓`
    /// and shown in the `▾` dropdown. Per-pane, in-memory for the
    /// session (it survives close/reopen of the overlay in this pane).
    pub history: Vec<String>,
    /// Position in [`Self::history`] while walking it, or `None` when
    /// the user is editing the live query.
    pub history_pos: Option<usize>,
    /// The live query saved when the user started walking history, so
    /// `↓` past the newest entry restores what they were typing.
    pub draft: String,
    /// Whether the `▾` history dropdown list is showing.
    pub dropdown_open: bool,
}

impl FindOverlay {
    /// Open a fresh overlay, seeded with this pane's prior query
    /// `history` (so reopening keeps the dropdown populated).
    pub fn open(history: Vec<String>) -> Self {
        Self {
            query: String::new(),
            case_sensitive: false,
            regex: false,
            filter: SearchFilter::Both,
            matches: Vec::new(),
            selected: 0,
            regex_error: false,
            history,
            history_pos: None,
            draft: String::new(),
            dropdown_open: false,
        }
    }

    /// Recompute [`Self::matches`] against `lines` and the current
    /// query / toggles. Clamps `selected` into range; an empty query
    /// clears matches (no highlight, no `(bad regex)`).
    pub fn recompute(&mut self, lines: &[SearchableLine]) {
        if self.query.is_empty() {
            self.matches.clear();
            self.regex_error = false;
            self.selected = 0;
            return;
        }
        let params = FindParams { case_sensitive: self.case_sensitive, regex: self.regex };
        match build_matcher(&self.query, params) {
            Ok(matcher) => {
                self.matches = find_matches(lines, &matcher, self.filter);
                self.regex_error = false;
                if self.selected >= self.matches.len() {
                    self.selected = 0;
                }
            }
            Err(_) => {
                self.matches.clear();
                self.regex_error = true;
                self.selected = 0;
            }
        }
    }

    /// The current match, if any.
    pub fn selected_match(&self) -> Option<&SearchMatch> {
        self.matches.get(self.selected)
    }

    /// Move to the next match, wrapping. No-op with zero matches.
    pub fn next_match(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    /// Move to the previous match, wrapping. No-op with zero matches.
    pub fn prev_match(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// Cycle the `All` / `Commands` / `Outputs` filter and reset the
    /// selection to the top of the (about-to-be-recomputed) list.
    pub fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.selected = 0;
    }

    /// Flip the `Aa` (case) toggle; selection resets on recompute.
    pub fn toggle_case(&mut self) {
        self.case_sensitive = !self.case_sensitive;
        self.selected = 0;
    }

    /// Flip the `.*` (regex) toggle.
    pub fn toggle_regex(&mut self) {
        self.regex = !self.regex;
        self.selected = 0;
    }

    /// Walk to an **older** history entry (`↑`). The first press saves
    /// the live query as the draft; further presses move toward the
    /// oldest, clamping there.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_pos {
            None => {
                self.draft = self.query.clone();
                self.history_pos = Some(0);
            }
            Some(i) if i + 1 < self.history.len() => self.history_pos = Some(i + 1),
            Some(_) => {}
        }
        if let Some(i) = self.history_pos {
            self.query = self.history[i].clone();
        }
    }

    /// Walk to a **newer** history entry (`↓`). Stepping past the
    /// newest restores the saved draft and returns to live editing.
    pub fn history_next(&mut self) {
        match self.history_pos {
            Some(i) if i > 0 => {
                self.history_pos = Some(i - 1);
                self.query = self.history[i - 1].clone();
            }
            Some(_) => {
                self.history_pos = None;
                self.query = self.draft.clone();
            }
            None => {}
        }
    }

    /// Fill the query from a clicked dropdown entry and stop walking.
    pub fn pick_history(&mut self, idx: usize) {
        if let Some(q) = self.history.get(idx) {
            self.query = q.clone();
            self.history_pos = None;
            self.dropdown_open = false;
        }
    }

    /// Commit the current query to the front of the history (delete any
    /// earlier duplicate, cap the length). Called when the user submits
    /// a search (Enter). Blank queries are ignored.
    pub fn commit_history(&mut self) {
        let q = self.query.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.history.retain(|h| h != &q);
        self.history.insert(0, q);
        self.history.truncate(HISTORY_CAP);
        self.history_pos = None;
    }
}

/// What [`paint_overlay`] decided this frame. The caller (`render_pane`)
/// applies it: closes the overlay, and/or schedules a scroll so the
/// current match comes into view.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FindOutcome {
    /// Esc / close button — drop the overlay.
    pub close: bool,
    /// The selection (or query) changed — scroll the current match
    /// into view on the next frame.
    pub scroll_to_selected: bool,
}

/// Slot-aware entry point: pull the overlay out of `slot.ui` and paint
/// it. Returns `FindOutcome::default()` (no-op) when no overlay is open.
pub fn paint(
    ui: &mut egui::Ui,
    slot: &mut crate::pane_slot::PaneSlot,
    pane_rect: egui::Rect,
) -> FindOutcome {
    let pane_id = slot.session.pane_id();
    let Some(overlay) = slot.ui.find_overlay.as_mut() else { return FindOutcome::default() };
    paint_overlay(ui, overlay, pane_id, pane_rect)
}

/// Bright, near-opaque accents for the count text + current match.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(255, 196, 0);

/// The renderable inner of [`paint`], split out so snapshot tests can
/// drive it with a synthetic [`FindOverlay`] and a plain rect instead
/// of standing up a real pane. Mirrors
/// [`crate::history_overlay::paint_overlay`]'s shape.
pub fn paint_overlay(
    ui: &mut egui::Ui,
    overlay: &mut FindOverlay,
    pane_id: u64,
    pane_rect: egui::Rect,
) -> FindOutcome {
    let area_id = ui.id().with(("find-overlay", pane_id));
    let margin = 8.0;
    let panel_w = (pane_rect.width() - 2.0 * margin).clamp(280.0, 920.0);
    let mut outcome = FindOutcome::default();

    // Read navigation keys from the raw input layer (the TextEdit
    // doesn't consume Esc / Enter / arrows). `↑`/`↓` walk the query
    // history; Enter / Shift+Enter step matches; Esc closes.
    let (enter, escape, up, down, shift) = ui.ctx().input(|i| {
        (
            i.key_pressed(egui::Key::Enter),
            i.key_pressed(egui::Key::Escape),
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::ArrowDown),
            i.modifiers.shift,
        )
    });

    egui::Area::new(area_id)
        .order(egui::Order::Foreground)
        .fixed_pos(pane_rect.left_top() + egui::vec2(margin, margin))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(panel_w);
                ui.horizontal_wrapped(|ui| {
                    ui.strong("find");

                    let prev_query = overlay.query.clone();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut overlay.query)
                            .desired_width(220.0)
                            .id_salt(("find-query", pane_id))
                            .hint_text("search transcript…")
                            .lock_focus(true),
                    );
                    if !resp.has_focus() {
                        resp.request_focus();
                    }
                    // A real edit (not a history walk) returns to live
                    // editing and re-homes the selection to the first match.
                    if overlay.query != prev_query {
                        overlay.history_pos = None;
                        overlay.selected = 0;
                        outcome.scroll_to_selected = true;
                    }

                    // `▾` history dropdown toggle, drawn with the painter
                    // (no Unicode glyphs per CLAUDE.md).
                    if triangle_button(ui, ("find-dropdown", pane_id)).clicked() {
                        overlay.dropdown_open = !overlay.dropdown_open;
                    }

                    // Match count / status.
                    if overlay.regex_error {
                        ui.colored_label(ui.visuals().warn_fg_color, "(bad regex)");
                    } else if overlay.query.is_empty() {
                        ui.weak("type to search");
                    } else if overlay.matches.is_empty() {
                        ui.weak("no results");
                    } else {
                        ui.colored_label(
                            ACCENT,
                            format!("{} of {}", overlay.selected + 1, overlay.matches.len()),
                        );
                    }

                    // Toggles.
                    if ui
                        .selectable_label(overlay.case_sensitive, "Aa")
                        .on_hover_text("Match case")
                        .clicked()
                    {
                        overlay.toggle_case();
                        outcome.scroll_to_selected = true;
                    }
                    if ui
                        .selectable_label(overlay.regex, ".*")
                        .on_hover_text("Regular expression")
                        .clicked()
                    {
                        overlay.toggle_regex();
                        outcome.scroll_to_selected = true;
                    }
                    if ui
                        .button(overlay.filter.label())
                        .on_hover_text("Search commands, output, or both")
                        .clicked()
                    {
                        overlay.cycle_filter();
                        outcome.scroll_to_selected = true;
                    }

                    // Prev / Next (text, not glyphs).
                    let nav_enabled = !overlay.matches.is_empty();
                    if ui.add_enabled(nav_enabled, egui::Button::new("Prev")).clicked() {
                        overlay.prev_match();
                        outcome.scroll_to_selected = true;
                    }
                    if ui.add_enabled(nav_enabled, egui::Button::new("Next")).clicked() {
                        overlay.next_match();
                        outcome.scroll_to_selected = true;
                    }
                    if ui.button("Done").clicked() {
                        outcome.close = true;
                    }
                });

                // History dropdown list.
                if overlay.dropdown_open && !overlay.history.is_empty() {
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt(("find-history", pane_id))
                        .max_height(160.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            let entries = overlay.history.clone();
                            for (i, h) in entries.iter().enumerate() {
                                if ui.selectable_label(false, h).clicked() {
                                    overlay.pick_history(i);
                                    outcome.scroll_to_selected = true;
                                }
                            }
                        });
                }
            });
        });

    // Keyboard, after layout so toggles/clicks above are already applied.
    if escape {
        outcome.close = true;
    } else if enter {
        if shift {
            overlay.prev_match();
        } else {
            overlay.next_match();
        }
        overlay.commit_history();
        outcome.scroll_to_selected = true;
    } else if up {
        overlay.history_prev();
        outcome.scroll_to_selected = true;
    } else if down {
        overlay.history_next();
        outcome.scroll_to_selected = true;
    }

    outcome
}

/// A small square button with a downward triangle painted in it — the
/// history dropdown affordance. Hand-drawn rather than a Unicode `▾`
/// per CLAUDE.md's "no Unicode symbols for icons" rule.
fn triangle_button(ui: &mut egui::Ui, salt: impl std::hash::Hash) -> egui::Response {
    let size = egui::vec2(18.0, 18.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let resp = resp.on_hover_text("Recent searches");
    let visuals = ui.style().interact(&resp);
    ui.painter().rect(rect, 2.0, visuals.bg_fill, visuals.bg_stroke, egui::StrokeKind::Inside);
    let c = rect.center();
    let w = 4.5;
    let h = 2.5;
    let tri =
        vec![egui::pos2(c.x - w, c.y - h), egui::pos2(c.x + w, c.y - h), egui::pos2(c.x, c.y + h)];
    ui.painter().add(egui::Shape::convex_polygon(tri, visuals.fg_stroke.color, egui::Stroke::NONE));
    let _ = salt; // id_salt reserved for callers that nest these in a loop
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::LifecycleEvent;
    use crate::terminal::TerminalState;

    // ---- pure matcher --------------------------------------------------

    fn cols(text: &str, query: &str, case_sensitive: bool, regex: bool) -> Vec<(usize, usize)> {
        let m = build_matcher(query, FindParams { case_sensitive, regex }).expect("compiles");
        line_match_columns(text, &m)
    }

    #[test]
    fn literal_case_insensitive_by_default() {
        assert_eq!(cols("echo $PWD", "pwd", false, false), vec![(6, 9)]);
    }

    #[test]
    fn literal_case_sensitive_respects_case() {
        assert_eq!(cols("echo $PWD", "pwd", true, false), vec![]);
        assert_eq!(cols("echo $PWD", "PWD", true, false), vec![(6, 9)]);
    }

    #[test]
    fn multiple_non_overlapping_matches() {
        assert_eq!(cols("aXaXa", "a", false, false), vec![(0, 1), (2, 3), (4, 5)]);
    }

    #[test]
    fn overlapping_needle_does_not_double_count() {
        // "aa" in "aaaa" → matches at 0 and 2, not 0/1/2.
        assert_eq!(cols("aaaa", "aa", false, false), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn columns_are_char_not_byte_offsets() {
        // The "é" before the match is 2 bytes but 1 column; the match
        // must report column 2, not byte 3.
        assert_eq!(cols("aébc", "bc", false, false), vec![(2, 4)]);
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert_eq!(cols("anything", "", false, false), vec![]);
    }

    #[test]
    fn regex_mode_matches_pattern() {
        assert_eq!(cols("err 12 err 34", r"\d+", false, true), vec![(4, 6), (11, 13)]);
    }

    #[test]
    fn regex_honours_case_toggle() {
        assert_eq!(cols("ERROR", "error", false, true), vec![(0, 5)]); // ci default
        assert_eq!(cols("ERROR", "error", true, true), vec![]); // case-sensitive
    }

    #[test]
    fn regex_zero_width_match_is_skipped() {
        // `a*` matches empty at every position; we must not emit zero-
        // width highlights (and must not loop forever).
        assert_eq!(cols("bbb", "a*", false, true), vec![]);
    }

    #[test]
    fn bad_regex_is_an_error() {
        assert!(
            build_matcher("(unclosed", FindParams { case_sensitive: false, regex: true }).is_err()
        );
    }

    // ---- collect + find over a block stack -----------------------------

    /// Run one command to completion, sealing a block whose output is
    /// `output_bytes`.
    fn seal(stack: &mut BlockStack, term: &mut TerminalState, command: &str, output_bytes: &[u8]) {
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: command.to_string() },
            term,
            1,
        );
        term.feed(output_bytes);
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, term, 2);
    }

    #[test]
    fn collect_skips_the_live_prompt_tail() {
        let stack = BlockStack::new(0); // just the live Prompt
        assert!(collect_searchable_lines(&stack).is_empty());
    }

    #[test]
    fn collect_yields_command_then_output_rows() {
        let mut stack = BlockStack::new(0);
        let mut term = TerminalState::new(4, 20);
        seal(&mut stack, &mut term, "echo hi", b"hi\r\n");
        let lines = collect_searchable_lines(&stack);
        // First line is the command, then the output rows.
        assert_eq!(lines[0].kind, LineKind::Command);
        assert_eq!(lines[0].text, "echo hi");
        assert!(lines.iter().any(|l| l.kind == LineKind::Output && l.text == "hi"));
    }

    #[test]
    fn output_rows_are_right_trimmed_of_padding() {
        let mut stack = BlockStack::new(0);
        let mut term = TerminalState::new(4, 20);
        seal(&mut stack, &mut term, "x", b"hi\r\n");
        let lines = collect_searchable_lines(&stack);
        let hi =
            lines.iter().find(|l| l.kind == LineKind::Output && l.text.contains("hi")).unwrap();
        assert_eq!(hi.text, "hi", "grid padding spaces must be trimmed");
    }

    #[test]
    fn filter_restricts_to_commands_or_outputs() {
        let mut stack = BlockStack::new(0);
        let mut term = TerminalState::new(4, 20);
        seal(&mut stack, &mut term, "grep needle", b"needle found\r\n");
        let lines = collect_searchable_lines(&stack);
        let m =
            build_matcher("needle", FindParams { case_sensitive: false, regex: false }).unwrap();

        let both = find_matches(&lines, &m, SearchFilter::Both);
        assert_eq!(both.len(), 2, "command + output");

        let cmd = find_matches(&lines, &m, SearchFilter::CommandOnly);
        assert_eq!(cmd.len(), 1);
        assert!(cmd.iter().all(|x| x.kind == LineKind::Command));

        let out = find_matches(&lines, &m, SearchFilter::OutputOnly);
        assert_eq!(out.len(), 1);
        assert!(out.iter().all(|x| x.kind == LineKind::Output));
    }

    #[test]
    fn matches_are_in_document_order() {
        let mut stack = BlockStack::new(0);
        let mut term = TerminalState::new(4, 20);
        seal(&mut stack, &mut term, "one z", b"z\r\n");
        seal(&mut stack, &mut term, "two z", b"z\r\n");
        let lines = collect_searchable_lines(&stack);
        let m = build_matcher("z", FindParams { case_sensitive: false, regex: false }).unwrap();
        let matches = find_matches(&lines, &m, SearchFilter::Both);
        // Block 0's command + output come before block 1's.
        let ids: Vec<u64> = matches.iter().map(|x| x.block_id.0).collect();
        assert_eq!(ids, vec![0, 0, 1, 1]);
    }

    // ---- overlay state machine -----------------------------------------

    fn overlay_with_matches(n: usize) -> FindOverlay {
        let mut o = FindOverlay::open(vec![]);
        o.query = "q".to_string();
        o.matches = (0..n)
            .map(|i| SearchMatch {
                block_id: BlockId(0),
                kind: LineKind::Output,
                row: i,
                col_start: 0,
                col_end: 1,
            })
            .collect();
        o
    }

    #[test]
    fn next_and_prev_wrap_around() {
        let mut o = overlay_with_matches(3);
        assert_eq!(o.selected, 0);
        o.next_match();
        o.next_match();
        assert_eq!(o.selected, 2);
        o.next_match();
        assert_eq!(o.selected, 0, "next wraps");
        o.prev_match();
        assert_eq!(o.selected, 2, "prev wraps");
    }

    #[test]
    fn nav_on_empty_matches_is_noop() {
        let mut o = overlay_with_matches(0);
        o.next_match();
        o.prev_match();
        assert_eq!(o.selected, 0);
    }

    #[test]
    fn recompute_clamps_selection_when_matches_shrink() {
        let mut stack = BlockStack::new(0);
        let mut term = TerminalState::new(4, 20);
        seal(&mut stack, &mut term, "aaa", b"\r\n");
        let lines = collect_searchable_lines(&stack);
        let mut o = FindOverlay::open(vec![]);
        o.query = "a".to_string();
        o.recompute(&lines);
        assert_eq!(o.matches.len(), 3);
        o.selected = 2;
        // Narrow the query so there's only one match — selection must
        // clamp back into range rather than dangle past the end.
        o.query = "aaa".to_string();
        o.recompute(&lines);
        assert_eq!(o.matches.len(), 1);
        assert_eq!(o.selected, 0);
    }

    #[test]
    fn empty_query_clears_matches_and_error() {
        let mut o = FindOverlay::open(vec![]);
        o.regex = true;
        o.query = String::new();
        o.recompute(&[]);
        assert!(o.matches.is_empty());
        assert!(!o.regex_error);
    }

    #[test]
    fn recompute_flags_bad_regex() {
        let mut o = FindOverlay::open(vec![]);
        o.regex = true;
        o.query = "(".to_string();
        o.recompute(&[]);
        assert!(o.regex_error);
        assert!(o.matches.is_empty());
    }

    #[test]
    fn filter_cycle_order() {
        let mut o = FindOverlay::open(vec![]);
        assert_eq!(o.filter, SearchFilter::Both);
        o.cycle_filter();
        assert_eq!(o.filter, SearchFilter::CommandOnly);
        o.cycle_filter();
        assert_eq!(o.filter, SearchFilter::OutputOnly);
        o.cycle_filter();
        assert_eq!(o.filter, SearchFilter::Both);
    }

    // ---- query history walk --------------------------------------------

    #[test]
    fn history_up_down_walks_and_restores_draft() {
        let mut o = FindOverlay::open(vec!["newest".into(), "older".into()]);
        o.query = "typing".to_string();
        o.history_prev(); // save draft, go to newest
        assert_eq!(o.query, "newest");
        o.history_prev(); // older
        assert_eq!(o.query, "older");
        o.history_prev(); // clamp at oldest
        assert_eq!(o.query, "older");
        o.history_next(); // back to newest
        assert_eq!(o.query, "newest");
        o.history_next(); // back to the live draft
        assert_eq!(o.query, "typing");
        o.history_next(); // no-op at draft
        assert_eq!(o.query, "typing");
    }

    #[test]
    fn history_walk_on_empty_history_is_noop() {
        let mut o = FindOverlay::open(vec![]);
        o.query = "x".to_string();
        o.history_prev();
        assert_eq!(o.query, "x");
    }

    #[test]
    fn commit_dedupes_and_moves_to_front() {
        let mut o = FindOverlay::open(vec!["a".into(), "b".into()]);
        o.query = "b".to_string();
        o.commit_history();
        assert_eq!(o.history, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn commit_ignores_blank_query() {
        let mut o = FindOverlay::open(vec!["a".into()]);
        o.query = "   ".to_string();
        o.commit_history();
        assert_eq!(o.history, vec!["a".to_string()]);
    }

    #[test]
    fn pick_history_fills_query() {
        let mut o = FindOverlay::open(vec!["first".into(), "second".into()]);
        o.dropdown_open = true;
        o.pick_history(1);
        assert_eq!(o.query, "second");
        assert!(!o.dropdown_open);
    }
}
