//! UI snapshot tests via `egui_kittest`.
//!
//! Update baselines with:
//!
//!     UPDATE_SNAPSHOTS=true cargo test --workspace
//!
//! After regenerating, inspect every changed `.png` and any
//! `*.diff.png` per [CLAUDE.md](../CLAUDE.md#snapshot-review).
//!
//! Snapshot files live at `tests/snapshots/<name>.png`. The kittest
//! threshold lives in [`kittest.toml`](../kittest.toml) and is
//! tighter on macOS than on Linux because GPU drivers differ.
//!
//! The fixtures are deterministic plain structs — `PaneView` carries
//! no OS handles, so we don't spawn a real PTY here.

#![forbid(unsafe_code)]

use eframe::egui;
use egui_kittest::Harness;
use termica::pane::PaneView;

fn empty_view() -> PaneView {
    PaneView::default()
}

fn typical_view() -> PaneView {
    PaneView {
        bytes_received: 84,
        alt_screen: false,
        screen_text: [
            "$ echo hello                            ",
            "hello                                   ",
            "$ ls                                    ",
            "Cargo.toml  README.md  src/             ",
            "$                                       ",
        ]
        .join("\n"),
    }
}

#[test]
fn snapshot_central_panel_empty() {
    let view = empty_view();
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(800.0, 200.0))
        .build_ui(move |ui| termica::central_panel(ui, &view));
    harness.snapshot("central_panel_empty");
}

#[test]
fn snapshot_central_panel_with_typical_output() {
    let view = typical_view();
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(800.0, 260.0))
        .build_ui(move |ui| termica::central_panel(ui, &view));
    harness.snapshot("central_panel_typical");
}
