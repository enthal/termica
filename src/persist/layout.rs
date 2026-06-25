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

/// The durable form of a window's layout. Serialized to JSON in
/// `window.layout_blob`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SavedLayout {
    /// Tile topology with `PaneId` leaves — tabs, splits, active leaf.
    pub tree: Tree<PaneId>,
    /// app `PaneId` (`u64`) → durable `pane` row id. A restored leaf
    /// looks itself up here to load its scrollback chunks.
    pub db_pane_by_app: HashMap<u64, i64>,
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
        SavedLayout { tree, db_pane_by_app }
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
}
