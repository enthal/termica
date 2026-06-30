//! Persisted window layout (Phase 9F).
//!
//! What we serialize into `window.layout_blob` so a relaunch can
//! rebuild the tile tree and reconnect each pane to its scrollback:
//!
//! - the `egui_tiles::Tree<PaneId>` topology (tabs + splits, with the
//!   app's `PaneId` leaves), and
//! - a map from each app `PaneId` to its durable `pane` row id, so a
//!   restored leaf can find its chunk files (chunks are keyed by the db
//!   pane id, which outlives any single process).
//!
//! Stored as JSON (small, human-inspectable). The format is Termica's;
//! a future change ships a layout-blob migration.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use egui_tiles::Tree;

use crate::pane_slot::PaneId;

/// The OS window's size + position, persisted alongside the layout so a
/// relaunch reopens the window where it was. All values are egui logical
/// **points** (not physical pixels): `inner_*` is the drawable area,
/// `pos_*` is the outer (title-bar-inclusive) top-left in screen
/// coordinates. `pos_*` may be negative on a multi-monitor desktop.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowGeometry {
    pub inner_width: f32,
    pub inner_height: f32,
    pub pos_x: f32,
    pub pos_y: f32,
}

impl WindowGeometry {
    /// Fit this geometry to a monitor of `mon` (width, height) logical
    /// points: cap the size to the monitor and nudge the top-left so the
    /// window lands on-screen. A workspace saved on a large display thus
    /// restores **usably** on a smaller one ("fit to the new screen").
    /// `None` (monitor size unknown) returns the geometry unchanged.
    ///
    /// Pure — the unit of the "fit to screen" behaviour, tested directly.
    pub fn clamp_to_monitor(self, mon: Option<(f32, f32)>) -> WindowGeometry {
        let Some((mon_w, mon_h)) = mon else { return self };
        // A monitor with non-positive reported size tells us nothing; don't
        // collapse the window to zero on a bogus reading.
        if mon_w <= 0.0 || mon_h <= 0.0 {
            return self;
        }
        let inner_width = self.inner_width.min(mon_w);
        let inner_height = self.inner_height.min(mon_h);
        // Keep the window's top-left within [0, monitor - size] so the
        // whole window is visible on the current monitor.
        let pos_x = self.pos_x.clamp(0.0, (mon_w - inner_width).max(0.0));
        let pos_y = self.pos_y.clamp(0.0, (mon_h - inner_height).max(0.0));
        WindowGeometry { inner_width, inner_height, pos_x, pos_y }
    }
}

/// The durable form of a window's layout. Serialized to JSON in
/// `window.layout_blob`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SavedLayout {
    /// Tile topology with `PaneId` leaves — tabs, splits, active leaf.
    pub tree: Tree<PaneId>,
    /// app `PaneId` (`u64`) → durable `pane` row id. A restored leaf
    /// looks itself up here to load its scrollback chunks.
    pub db_pane_by_app: HashMap<u64, i64>,
    /// The OS window's last size + position. `#[serde(default)]` so a
    /// pre-geometry blob (written before this field existed) still parses
    /// — it deserializes to `None` and the window opens at its default
    /// size. Back-compatible: no schema migration.
    #[serde(default)]
    pub window_geometry: Option<WindowGeometry>,
}

impl SavedLayout {
    /// Serialize to the bytes stored in `window.layout_blob`.
    pub fn to_blob(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Parse a `layout_blob` back into a `SavedLayout`.
    pub fn from_blob(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// The app `PaneId`s that are leaves of the saved tree, in tree
    /// order. Restore creates one Dead pane per id.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.tree
            .tiles
            .tiles()
            .filter_map(|tile| match tile {
                egui_tiles::Tile::Pane(p) => Some(*p),
                egui_tiles::Tile::Container(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SavedLayout {
        // A two-pane tab tile.
        let mut tiles = egui_tiles::Tiles::default();
        let a = tiles.insert_pane(PaneId(3));
        let b = tiles.insert_pane(PaneId(7));
        let root = tiles.insert_tab_tile(vec![a, b]);
        let tree = Tree::new("restored", root, tiles);
        let db_pane_by_app = HashMap::from([(3u64, 100i64), (7u64, 101i64)]);
        SavedLayout { tree, db_pane_by_app, window_geometry: None }
    }

    #[test]
    fn blob_round_trips() {
        let layout = sample();
        let blob = layout.to_blob().unwrap();
        let back = SavedLayout::from_blob(&blob).unwrap();
        // The map survives verbatim.
        assert_eq!(back.db_pane_by_app, layout.db_pane_by_app);
        // The topology survives: same leaves, and the root is still a
        // Tabs container over both panes. (Tree has no PartialEq and its
        // tiles live in a HashMap, so serialized byte order isn't stable
        // — assert structure, not bytes.)
        let mut ids = back.pane_ids();
        ids.sort_by_key(|p| p.0);
        assert_eq!(ids, vec![PaneId(3), PaneId(7)]);
        let root = back.tree.root().expect("root tile");
        match back.tree.tiles.get(root) {
            Some(egui_tiles::Tile::Container(c)) => {
                assert_eq!(c.kind(), egui_tiles::ContainerKind::Tabs);
                assert_eq!(c.num_children(), 2);
            }
            other => panic!("expected a Tabs container at root, got {other:?}"),
        }
    }

    #[test]
    fn pane_ids_lists_every_leaf() {
        let layout = sample();
        let mut ids = layout.pane_ids();
        ids.sort_by_key(|p| p.0);
        assert_eq!(ids, vec![PaneId(3), PaneId(7)]);
    }

    #[test]
    fn parsed_layout_resolves_db_pane_ids() {
        let blob = sample().to_blob().unwrap();
        let back = SavedLayout::from_blob(&blob).unwrap();
        for p in back.pane_ids() {
            assert!(back.db_pane_by_app.contains_key(&p.0), "every leaf maps to a db pane id");
        }
    }

    // ---- window geometry (spec/08) --------------------------------

    #[test]
    fn window_geometry_round_trips_through_the_blob() {
        let mut layout = sample();
        let geom =
            WindowGeometry { inner_width: 1440.0, inner_height: 900.0, pos_x: 120.0, pos_y: 64.0 };
        layout.window_geometry = Some(geom);
        let back = SavedLayout::from_blob(&layout.to_blob().unwrap()).unwrap();
        assert_eq!(back.window_geometry, Some(geom), "geometry survives the JSON blob verbatim");
    }

    #[test]
    fn pre_geometry_blob_parses_with_none() {
        // A blob written before `window_geometry` existed has no such key.
        // `#[serde(default)]` must let it parse (back-compat, no migration).
        // Build it by stripping the field from a real blob, so the rest of
        // the (egui_tiles) shape is exactly what older code emitted.
        let json = sample().to_blob().unwrap();
        let mut v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        v.as_object_mut().unwrap().remove("window_geometry");
        let old_blob = serde_json::to_vec(&v).unwrap();
        let back =
            SavedLayout::from_blob(&old_blob).expect("old blob without geometry still parses");
        assert_eq!(back.window_geometry, None, "absent geometry deserializes to None");
    }

    #[test]
    fn clamp_to_monitor_unknown_monitor_is_identity() {
        let g =
            WindowGeometry { inner_width: 3000.0, inner_height: 2000.0, pos_x: -50.0, pos_y: 10.0 };
        assert_eq!(g.clamp_to_monitor(None), g, "unknown monitor → unchanged");
    }

    #[test]
    fn clamp_to_monitor_caps_oversize_window_and_pulls_it_onscreen() {
        // Saved on a 3840×2160 display at (200, 100), restored onto 1920×1080.
        let g = WindowGeometry {
            inner_width: 3000.0,
            inner_height: 2000.0,
            pos_x: 2500.0,
            pos_y: 1500.0,
        };
        let c = g.clamp_to_monitor(Some((1920.0, 1080.0)));
        assert_eq!(c.inner_width, 1920.0, "width capped to monitor");
        assert_eq!(c.inner_height, 1080.0, "height capped to monitor");
        assert_eq!(c.pos_x, 0.0, "top-left pulled fully on-screen (monitor - size == 0)");
        assert_eq!(c.pos_y, 0.0);
    }

    #[test]
    fn clamp_to_monitor_leaves_a_fitting_window_alone() {
        let g =
            WindowGeometry { inner_width: 1200.0, inner_height: 800.0, pos_x: 100.0, pos_y: 50.0 };
        assert_eq!(
            g.clamp_to_monitor(Some((1920.0, 1080.0))),
            g,
            "a window that fits is unchanged"
        );
    }

    #[test]
    fn clamp_to_monitor_lifts_a_negative_offscreen_origin() {
        let g =
            WindowGeometry { inner_width: 800.0, inner_height: 600.0, pos_x: -300.0, pos_y: -20.0 };
        let c = g.clamp_to_monitor(Some((1920.0, 1080.0)));
        assert_eq!(
            (c.pos_x, c.pos_y),
            (0.0, 0.0),
            "a negative (off-screen) origin is lifted to the corner"
        );
        assert_eq!((c.inner_width, c.inner_height), (800.0, 600.0), "a fitting size is preserved");
    }
}
