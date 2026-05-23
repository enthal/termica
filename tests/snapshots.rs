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

#![forbid(unsafe_code)]

use eframe::egui;
use egui_kittest::Harness;

/// The empty central panel renders the Phase-1B heading + status line.
/// Regenerating this snapshot is expected on any intentional change to
/// `termica::central_panel` and only there.
#[test]
fn snapshot_central_panel_empty() {
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(800.0, 200.0))
        .build_ui(termica::central_panel);
    harness.snapshot("central_panel_empty");
}
