//! egui_tiles `Behavior` impl for Termica.
//!
//! [`TabBehavior`] is a per-frame shim that holds borrows into
//! [`crate::app::TermicaApp`] so the Behavior callbacks can read pane
//! state, draw pane UI, and stage tree mutations (deferred to after
//! `tree.ui()` returns — egui_tiles forbids mutating the tree from
//! inside a callback).

use std::collections::HashMap;
use std::path::Path;

use eframe::egui;
use egui_tiles::{Behavior, ResizeState, Tile, TileId, Tiles, UiResponse};

use crate::pane_slot::{PaneId, PaneSlot};
use crate::render_pane::render_pane;
use crate::tab_title::tab_title_for_with_osc;

/// Per-frame Behavior shim. Holds borrows into [`crate::app::TermicaApp`]
/// so the Behavior callbacks can read pane state, draw pane UI, and
/// stage tree mutations (deferred to after `tree.ui()`).
pub(crate) struct TabBehavior<'a> {
    pub(crate) panes: &'a mut HashMap<PaneId, PaneSlot>,
    pub(crate) home: Option<&'a Path>,
    pub(crate) ctx: &'a egui::Context,
    /// The pane id whose keyboard focus we should advertise via tab
    /// styling. Set by `TermicaApp::update` from the previous frame's
    /// `slot.ui.focused`; one-frame lag is acceptable.
    pub(crate) focused_pane: Option<PaneId>,
    /// Set when the user clicks [+] in a tab strip. Read by
    /// `TermicaApp::update` after `tree.ui()` returns.
    pub(crate) new_tab_requested_in: Option<TileId>,
    /// Tab close requests staged by `on_tab_close`. Drained and
    /// applied by `TermicaApp::update` via [`egui_tiles::Tree::remove_recursively`]
    /// so the parent's `children` and `active` references get
    /// cleaned up properly.
    pub(crate) pending_closes: &'a mut Vec<TileId>,
    /// Same idea, but for tiles whose pane is running a program —
    /// the close goes through the confirmation modal first.
    pub(crate) pending_close_confirm: &'a mut Option<TileId>,
    /// True when ANY modal (close-confirm or quit-confirm) is
    /// currently showing. Forwarded to [`render_pane`] which uses it
    /// to render the pane as inert: no keyboard to the PTY, no
    /// wheel, no mouse selection, no focus grab. Without this, keys
    /// the user thought were going to the modal (Enter, Esc, arrows)
    /// reach the PTY because pane input is read before the modal
    /// renders later in the frame.
    pub(crate) modal_open: bool,
    /// Live-tunable focused-editor chrome variant. Read each frame
    /// from `TermicaApp.chrome_variant`; the second-window picker
    /// viewport writes into the same `Arc<Mutex<…>>` so a click
    /// in the picker updates the chrome on the next paint.
    pub(crate) chrome_variant: crate::focused_chrome::ChromeVariant,
}

impl<'a> Behavior<PaneId> for TabBehavior<'a> {
    fn tab_title_for_pane(&mut self, pane_id: &PaneId) -> egui::WidgetText {
        let slot = self.panes.get(pane_id);
        let term = slot.map(|s| s.session.terminal());
        let osc = term.and_then(|t| t.osc_title());
        let cwd = term.and_then(|t| t.cwd());
        let running = running_command_for(slot);
        pad_to_min_chars(
            &tab_title_for_with_osc(*pane_id, osc.as_deref(), cwd, self.home, running.as_deref()),
            MIN_TAB_TITLE_CHARS,
        )
        .into()
    }

    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile_id: TileId, pane_id: &mut PaneId) -> UiResponse {
        let Some(slot) = self.panes.get_mut(pane_id) else {
            ui.label(format!("(pane {} gone)", pane_id.0));
            return UiResponse::None;
        };
        // Salt every widget id under this pane by `PaneId` so a
        // pane that ends up referenced from multiple tree slots —
        // egui_tiles is permissive about this; we don't think it
        // happens in our flow but the cost of being safe is zero —
        // never collides on auto-generated ids. CLAUDE.md spells
        // out the rule: assume any UI element can render twice in
        // a frame and salt accordingly.
        let modal_open = self.modal_open;
        let chrome_variant = self.chrome_variant;
        ui.push_id(("pane", pane_id.0), |ui| {
            render_pane(ui, self.ctx, slot, self.home, modal_open, chrome_variant);
        });
        UiResponse::None
    }

    fn is_tab_closable(&self, tiles: &Tiles<PaneId>, _tile_id: TileId) -> bool {
        // Don't allow closing the very last tab — that would leave
        // the app with no pane and no obvious way for the user to
        // recover. A future PR can change this to either auto-spawn
        // a new pane or quit the app on last-close.
        tiles.iter().filter(|(_, t)| matches!(t, Tile::Pane(_))).count() > 1
    }

    fn on_tab_close(&mut self, tiles: &mut Tiles<PaneId>, tile_id: TileId) -> bool {
        // **Don't** let egui_tiles call `tiles.remove(tile_id)` —
        // that path doesn't clean up the parent Tabs container's
        // `children` / `active` references, which leaves a stale
        // pointer that blanks the pane area. Either route into the
        // modal (if a program is running) or defer the removal to
        // `TermicaApp::update` which uses
        // [`egui_tiles::Tree::remove_recursively`] (the cleanup-correct path).
        let running = matches!(tiles.get(tile_id), Some(Tile::Pane(pid))
            if self.panes.get(pid).is_some_and(|s| s.session.terminal().is_alternate_screen()));
        if running {
            *self.pending_close_confirm = Some(tile_id);
        } else {
            self.pending_closes.push(tile_id);
        }
        false
    }

    fn gap_width(&self, _style: &egui::Style) -> f32 {
        // egui_tiles' default is 1.0 px — too tight for the
        // focused-editor chrome to render its outer expand without
        // bleeding into a neighboring pane on splits. 16 px gives
        // the glow's 5–6 px outer radius room to land in the gap
        // (the chrome's horizontal clip in `render_pane` extends
        // halfway into this gap). Also reads as a clean tile
        // separation visually.
        16.0
    }

    fn resize_stroke(&self, style: &egui::Style, resize_state: ResizeState) -> egui::Stroke {
        // egui_tiles' default `Idle` stroke uses `tab_bar_color`,
        // which in dark mode is `extreme_bg_color` — near-black.
        // With our widened 16 px gap, that paints a wide black
        // stripe between panes that reads as a bug. Blend the
        // idle gap with `panel_fill` so it becomes invisible;
        // keep the bright hover / drag strokes egui's defaults so
        // the splitter still affords interaction on hover.
        match resize_state {
            ResizeState::Idle => egui::Stroke::new(self.gap_width(style), style.visuals.panel_fill),
            ResizeState::Hovering => style.visuals.widgets.hovered.fg_stroke,
            ResizeState::Dragging => style.visuals.widgets.active.fg_stroke,
        }
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        // By default egui_tiles prunes Tabs containers with only one
        // child, which silently removes our tab strip whenever there's
        // exactly one pane. We want the tab strip to ALWAYS be visible
        // (so the `[+]` button is reachable and the user has a clear
        // affordance for tabs even from the empty state) — hence
        // `all_panes_must_have_tabs = true`, which the docs say wins
        // over `prune_single_child_tabs`.
        egui_tiles::SimplificationOptions { all_panes_must_have_tabs: true, ..Default::default() }
    }

    fn top_bar_right_ui(
        &mut self,
        _tiles: &Tiles<PaneId>,
        ui: &mut egui::Ui,
        tile_id: TileId,
        _tabs: &egui_tiles::Tabs,
        _scroll_offset: &mut f32,
    ) {
        // Stage the "new tab in this Tabs container" intent. The
        // app applies it after `tree.ui()` so we don't mutate the
        // tree from inside a Behavior callback.
        if ui.button("+").on_hover_text("New tab").clicked() {
            self.new_tab_requested_in = Some(tile_id);
        }
    }

    // ---- Knauty-style tab styling --------------------------------
    //
    // Stay close to egui_tiles' defaults so the chrome doesn't
    // shout. The only departures:
    //
    //   - Inactive tabs render their title in italic. Mirrors
    //     Knauty's convention and makes "this is not the active
    //     tab in this region" immediately legible.
    //   - The active tab WHOSE PANE HOLDS KEYBOARD FOCUS renders
    //     its title in bold. Same idea — subtle, no extra paint,
    //     just font weight. With multiple split regions, the bold
    //     tab tells you where your typing will land.
    //
    // No accent-colored outline / background tint. The previous
    // version was too loud per UX review.

    fn tab_title_for_tile(&mut self, tiles: &Tiles<PaneId>, tile_id: TileId) -> egui::WidgetText {
        let Some(Tile::Pane(pane_id)) = tiles.get(tile_id) else {
            return "?".into();
        };
        let slot = self.panes.get(pane_id);
        let term = slot.map(|s| s.session.terminal());
        let osc = term.and_then(|t| t.osc_title());
        let cwd = term.and_then(|t| t.cwd());
        let running = running_command_for(slot);
        // No styling on the text itself any more. Active-in-container
        // is already differentiated by egui_tiles' default `tab_ui`
        // (brighter bg + connecting hline). The focused-for-the-whole-
        // app indicator is the blue bottom border painted in
        // [`on_tab_button`] below.
        egui::RichText::new(pad_to_min_chars(
            &tab_title_for_with_osc(*pane_id, osc.as_deref(), cwd, self.home, running.as_deref()),
            MIN_TAB_TITLE_CHARS,
        ))
        .into()
    }

    fn on_tab_button(
        &mut self,
        tiles: &Tiles<PaneId>,
        tile_id: TileId,
        response: egui::Response,
    ) -> egui::Response {
        // Tab click → focus its pane. egui_tiles changes the parent
        // Tabs.active only when the clicked tab is a *different*
        // tile, so clicking the already-active tab would otherwise
        // be a no-op — but the user expects clicking any tab to
        // bring keyboard focus to its pane, exactly like clicking
        // the pane's visible body does. We can't rely on the
        // `now_active.difference(prev_active)` diff in `update`
        // for this case because active didn't change.
        if response.clicked()
            && let Some(Tile::Pane(pane_id)) = tiles.get(tile_id)
            && let Some(slot) = self.panes.get_mut(pane_id)
        {
            slot.ui.needs_focus = true;
        }

        // Paint a blue underline on the tab that owns app-wide
        // keyboard focus. With multiple split regions each showing
        // their own active tab, this is what tells the user "your
        // typing lands HERE" at a glance. We paint into the same
        // layer the tab was drawn on (via `layer_painter`) and on
        // top of egui_tiles' own painted geometry — `on_tab_button`
        // runs after the default `tab_ui` returns.
        let is_focused = matches!(tiles.get(tile_id), Some(Tile::Pane(pid))
            if self.focused_pane == Some(*pid));
        if is_focused {
            paint_focused_tab_underline(&response);
        }
        response
    }
}

/// Paint the blue underline that marks the app-wide focused tab.
/// Public + free-standing so snapshot tests can apply the same
/// paint to a synthetic `egui_tiles::Tree` without standing up a
/// full [`crate::app::TermicaApp`]; the production caller is
/// [`TabBehavior::on_tab_button`]. Single source of truth for the
/// focus visual.
pub fn paint_focused_tab_underline(response: &egui::Response) {
    let painter = response.ctx.layer_painter(response.layer_id);
    let rect = response.rect;
    let color = response.ctx.style().visuals.selection.bg_fill;
    let stroke = egui::Stroke::new(2.5, color);
    let y = rect.bottom() - 1.25;
    painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], stroke);
}

/// Minimum tab title length in characters. Chosen via the
/// `pick_tab_min_width` visual picker — variant `3x` (≈48 px,
/// 3× the natural width of the worst-case `~` title that the
/// default home-directory pane shows on startup). egui_tiles has
/// no min-tab-width knob, so we get the width via the title text:
/// shorter titles are padded with spaces on each side. Symmetric
/// padding keeps the text centered (`tab_ui` paints it left-aligned
/// within the tab rect; the extra leading space shifts it
/// rightward so the visual center matches the tab center).
const MIN_TAB_TITLE_CHARS: usize = 7;

/// Pad `text` with leading + trailing spaces until it reaches
/// `min_chars`. Strings already `>= min_chars` are returned
/// unchanged. Odd-shortfalls split the extra space toward the
/// right so the result is consistently positioned across renders.
fn pad_to_min_chars(text: &str, min_chars: usize) -> String {
    let len = text.chars().count();
    if len >= min_chars {
        return text.to_string();
    }
    let shortfall = min_chars - len;
    let left = shortfall / 2;
    let right = shortfall - left;
    let left_pad: String = std::iter::repeat_n(' ', left).collect();
    let right_pad: String = std::iter::repeat_n(' ', right).collect();
    format!("{left_pad}{text}{right_pad}")
}

/// Extract the currently-running command string for a pane (if
/// any). When the pane's tail block is `Running`, this surfaces
/// the recorded command line so [`tab_title_for_with_osc`] can use
/// its first word as the tab title (`less`, `vim`, `htop`, …).
/// Returns `None` for `Prompt` / `Sealed` tails — the cwd or OSC
/// title wins in those states.
pub(crate) fn running_command_for(slot: Option<&PaneSlot>) -> Option<String> {
    let slot = slot?;
    match slot.session.blocks().last()? {
        crate::block::Block::Running { command, .. } => Some(command.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_to_min_chars_leaves_long_titles_alone() {
        assert_eq!(pad_to_min_chars("very-long-title", 7), "very-long-title");
    }

    #[test]
    fn pad_to_min_chars_centers_short_title() {
        // "~" → "   ~   " (3 spaces each side; 1 + 6 = 7).
        assert_eq!(pad_to_min_chars("~", 7), "   ~   ");
    }

    #[test]
    fn pad_to_min_chars_handles_odd_shortfall_with_extra_on_right() {
        // "ab" + 5 short → split 2/3 (extra on right so the
        // visual center sits to the right of the geometric one,
        // matching egui_tiles' LEFT_CENTER paint origin).
        assert_eq!(pad_to_min_chars("ab", 7), "  ab   ");
    }

    #[test]
    fn pad_to_min_chars_is_noop_when_already_at_min() {
        assert_eq!(pad_to_min_chars("abcdefg", 7), "abcdefg");
    }

    #[test]
    fn pad_to_min_chars_handles_empty_string() {
        // Defensive: a Behavior callback that returned an empty
        // title shouldn't panic; the helper should pad it to a
        // 7-space placeholder so the tab still has a clickable area.
        assert_eq!(pad_to_min_chars("", 7), "       ");
    }
}
