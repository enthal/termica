//! Snapshot tests for tile-layout chrome across split topologies.
//!
//! These are the Phase 2 acceptance test from
//! [`spec/10-roadmap.md`]: render a `Tree<PaneId>` with the same
//! `egui_tiles` behaviors the app uses (tab titles, focus
//! underline, tab strip layout) and snapshot the result. Pane
//! bodies are colored placeholder rects rather than real PTY
//! grids — we don't bring up real bash sessions here because the
//! goal is to lock in the *chrome* (tabs, splits, focus indicator)
//! across layouts, not assert anything about shell output.
//!
//! Production code paths that ARE exercised:
//! - [`termica::tab_title_for`] for tab titles (the same fn
//!   `TabBehavior::tab_title_for_pane` calls).
//! - [`termica::paint_focused_tab_underline`] for the focused-tab
//!   blue underline (the same fn `TabBehavior::on_tab_button`
//!   calls, extracted as a free fn for exactly this reuse).
//!
//! Update baselines with:
//!
//!     UPDATE_SNAPSHOTS=true cargo test --workspace
//!
//! and inspect every changed `.png` per
//! [CLAUDE.md](../CLAUDE.md#snapshot-review).

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;
use egui_kittest::Harness;
use egui_tiles::{Behavior, SimplificationOptions, Tile, TileId, Tiles, Tree, UiResponse};
use termica::{PaneId, paint_focused_tab_underline, tab_title_for};

/// Test-only `Behavior` that mirrors the visual contract of the
/// real `TabBehavior` but draws colored placeholder rects for
/// pane bodies. The chrome-relevant methods (`tab_title_for_pane`,
/// `on_tab_button`, `simplification_options`) call into the same
/// production helpers the app uses, so any regression in the
/// title rule or focus underline shows up in these snapshots too.
struct StubBehavior {
    cwd_by_pane: HashMap<PaneId, PathBuf>,
    home: PathBuf,
    /// Pane whose tab gets the blue focus underline. `None` →
    /// no tab is marked focused, useful as a control case.
    focused_pane: Option<PaneId>,
}

impl Behavior<PaneId> for StubBehavior {
    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile_id: TileId, pane: &mut PaneId) -> UiResponse {
        let rect = ui.available_rect_before_wrap();
        // Distinct fill per pane id so each placeholder is
        // visually unambiguous in the snapshot.
        let bg = match pane.0 % 4 {
            0 => egui::Color32::from_rgb(40, 40, 60),
            1 => egui::Color32::from_rgb(60, 40, 40),
            2 => egui::Color32::from_rgb(40, 60, 40),
            _ => egui::Color32::from_rgb(60, 60, 40),
        };
        ui.painter().rect_filled(rect, 0.0, bg);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("pane {}", pane.0),
            egui::FontId::proportional(16.0),
            egui::Color32::LIGHT_GRAY,
        );
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &PaneId) -> egui::WidgetText {
        let cwd = self.cwd_by_pane.get(pane).map(|p| p.as_path());
        tab_title_for(*pane, cwd, Some(self.home.as_path())).into()
    }

    fn on_tab_button(
        &mut self,
        tiles: &Tiles<PaneId>,
        tile_id: TileId,
        response: egui::Response,
    ) -> egui::Response {
        let is_focused = matches!(tiles.get(tile_id), Some(Tile::Pane(p))
            if self.focused_pane == Some(*p));
        if is_focused {
            paint_focused_tab_underline(&response);
        }
        response
    }

    fn simplification_options(&self) -> SimplificationOptions {
        // Same setting as the real `TabBehavior`. Without this,
        // `egui_tiles` prunes single-child Tabs containers, which
        // would change the tab-strip layout in the snapshots and
        // not match what the app renders.
        SimplificationOptions { all_panes_must_have_tabs: true, ..SimplificationOptions::default() }
    }
}

fn fixed_home() -> PathBuf {
    // Hard-coded so the `~`-substitution in `tab_title_for` is
    // independent of the test runner's actual `$HOME`. Without
    // this, the snapshot would render different tab titles on
    // CI vs a contributor's laptop.
    PathBuf::from("/Users/tim")
}

/// Tabs + 2-pane horizontal split: a horizontal `Linear` root
/// containing two single-tab `Tabs` children. Mirrors a user who
/// drag-split one tab to the right and now has two side-by-side
/// regions, each with its own tab strip.
fn build_two_pane_horizontal() -> (Tree<PaneId>, StubBehavior) {
    let mut tree = Tree::<PaneId>::empty("two-pane");
    let p1 = tree.tiles.insert_pane(PaneId(1));
    let p2 = tree.tiles.insert_pane(PaneId(2));
    let tabs_a = tree.tiles.insert_tab_tile(vec![p1]);
    let tabs_b = tree.tiles.insert_tab_tile(vec![p2]);
    let root = tree.tiles.insert_horizontal_tile(vec![tabs_a, tabs_b]);
    tree.root = Some(root);

    let mut cwds = HashMap::new();
    cwds.insert(PaneId(1), PathBuf::from("/Users/tim/git/enthal/termica"));
    cwds.insert(PaneId(2), PathBuf::from("/Users/tim/notes"));

    (
        tree,
        StubBehavior {
            cwd_by_pane: cwds,
            home: fixed_home(),
            // Right pane focused so the snapshot documents the
            // blue underline landing on its tab and NOT on the
            // left container's tab.
            focused_pane: Some(PaneId(2)),
        },
    )
}

/// Tabs + 3-pane T-split: horizontal `Linear` root with a single-
/// tab `Tabs` on the left and a vertical `Linear` on the right
/// containing two single-tab `Tabs` stacked. The shape a user
/// gets by drag-splitting the right pane horizontally after a
/// vertical split.
fn build_three_pane_t_split() -> (Tree<PaneId>, StubBehavior) {
    let mut tree = Tree::<PaneId>::empty("three-pane-t");
    let p1 = tree.tiles.insert_pane(PaneId(1));
    let p2 = tree.tiles.insert_pane(PaneId(2));
    let p3 = tree.tiles.insert_pane(PaneId(3));
    let tabs_a = tree.tiles.insert_tab_tile(vec![p1]);
    let tabs_b = tree.tiles.insert_tab_tile(vec![p2]);
    let tabs_c = tree.tiles.insert_tab_tile(vec![p3]);
    let right = tree.tiles.insert_vertical_tile(vec![tabs_b, tabs_c]);
    let root = tree.tiles.insert_horizontal_tile(vec![tabs_a, right]);
    tree.root = Some(root);

    let mut cwds = HashMap::new();
    cwds.insert(PaneId(1), PathBuf::from("/Users/tim"));
    cwds.insert(PaneId(2), PathBuf::from("/Users/tim/git/enthal/termica"));
    cwds.insert(PaneId(3), PathBuf::from("/tmp"));

    (
        tree,
        StubBehavior {
            cwd_by_pane: cwds,
            home: fixed_home(),
            // Top-right tab. Documents that the focus indicator
            // works in nested-Linear container topologies too.
            focused_pane: Some(PaneId(2)),
        },
    )
}

#[test]
fn snapshot_two_pane_horizontal_split() {
    let (mut tree, mut behavior) = build_two_pane_horizontal();
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(800.0, 300.0)).build_ui(move |ui| {
            tree.ui(&mut behavior, ui);
        });
    harness.snapshot("two_pane_horizontal_split");
}

#[test]
fn snapshot_three_pane_t_split() {
    let (mut tree, mut behavior) = build_three_pane_t_split();
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(800.0, 400.0)).build_ui(move |ui| {
            tree.ui(&mut behavior, ui);
        });
    harness.snapshot("three_pane_t_split");
}
