//! The `Dead`-pane "Restart shell" affordance is pinned to the BOTTOM of
//! the pane (where the live editor footer sits), not a strip at the top.
//!
//! A positional test (not a snapshot): render a real `Dead` pane via
//! `render_pane` and assert the "Restart shell" button lands in the
//! bottom region of the pane. Robust across platforms/DPI — it asserts
//! relative placement, not pixels.

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use termica::block::BlockStack;
use termica::focused_chrome::ChromeVariant;
use termica::pane::PaneSession;
use termica::watermark::WatermarkSettings;
use termica::{PaneSlot, PaneUiState, render_pane};

#[test]
fn restart_shell_affordance_is_pinned_to_the_pane_bottom() {
    const PANE_H: f32 = 240.0;

    // A restored pane with no transcript is `Dead` and shows the
    // "Restart shell" affordance.
    let session = PaneSession::restored(
        24,
        80,
        BlockStack::with_restored_sealed(Vec::new()),
        1,    // app pane id
        1,    // durable db pane row
        None, // no last-known cwd
    );
    let mut slot = PaneSlot { session, ui: PaneUiState::default() };
    assert!(slot.session.is_dead(), "a restored pane with no live shell is Dead");

    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, PANE_H)).build_ui(move |ui| {
            let ctx = ui.ctx().clone();
            render_pane(
                ui,
                &ctx,
                &mut slot,
                None,
                false,
                ChromeVariant::default(),
                WatermarkSettings::default(),
            );
        });
    harness.run();

    let button = harness.get_by_label("Restart shell");
    let y = button.rect().center().y;
    assert!(
        y > PANE_H / 2.0,
        "the Restart-shell affordance must sit in the BOTTOM half of the pane \
         (got center y={y}, pane height={PANE_H})",
    );
}
