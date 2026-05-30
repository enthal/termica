//! Dimness variants for the sealed-block background wash on
//! commands that exit non-zero. Current production color is too
//! bright — exercise a spread of dimmer alternatives.
//!
//! Run:   cargo run --example pick_failed_block_bg
//!
//! Picks by writing the variant id to
//! `/tmp/termica-picker-choice.txt`.

#![forbid(unsafe_code)]

use eframe::egui;
use termica::visual_picker::{Variant, run};

/// Sample failed block — same content as `pick_block_separator`'s
/// `cargo check` row so the visuals are comparable.
fn paint_sample(ui: &mut egui::Ui, bg: egui::Color32) {
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    // Chip row.
    ui.horizontal(|ui| {
        for (label, fg) in [
            ("~/git/enthal/termica", egui::Color32::from_rgb(0xa0, 0xa0, 0xa0)),
            ("exit 1", egui::Color32::from_rgb(0xff, 0x6e, 0x6e)),
        ] {
            let mono = egui::FontId::monospace(13.0);
            let galley = ui.fonts_mut(|f| f.layout_no_wrap(label.to_string(), mono.clone(), fg));
            let pad_x = 8.0;
            let pad_y = 3.0;
            let chip_w = galley.size().x + 2.0 * pad_x;
            let chip_h = galley.size().y + 2.0 * pad_y;
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(chip_w, chip_h), egui::Sense::hover());
            ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(0x22, 0x22, 0x22));
            ui.painter().rect_stroke(
                rect,
                4.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(0x44, 0x44, 0x44)),
                egui::StrokeKind::Inside,
            );
            ui.painter().galley(rect.min + egui::vec2(pad_x, pad_y), galley, fg);
        }
    });
    ui.label(
        egui::RichText::new("cargo check")
            .monospace()
            .color(egui::Color32::from_rgb(0x6e, 0xd0, 0xe8)),
    );
    ui.label(
        egui::RichText::new("error: cannot find macro `foo` in this scope")
            .monospace()
            .color(egui::Color32::from_rgb(0xd8, 0xd8, 0xd8)),
    );
    ui.label(
        egui::RichText::new("    --> src/lib.rs:42:5")
            .monospace()
            .color(egui::Color32::from_rgb(0xa0, 0xa0, 0xa0)),
    );
    // Wash slot — sits behind everything paint after .add(Noop).
    let r = ui.min_rect();
    ui.painter().set(bg_idx, egui::Shape::rect_filled(r.expand2(egui::vec2(4.0, 2.0)), 4.0, bg));
}

/// Helper to keep the alpha math in one place. `r` is the
/// unmultiplied red (out of 255); we use unmultiplied so the
/// alpha actually attenuates instead of silently clamping.
fn red(r: u8, alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(r, r / 4, r / 4, alpha)
}

fn paint_current(ui: &mut egui::Ui) {
    paint_sample(ui, red(0x80, 0x40));
}
fn paint_28(ui: &mut egui::Ui) {
    paint_sample(ui, red(0x80, 0x28));
}
fn paint_20(ui: &mut egui::Ui) {
    paint_sample(ui, red(0x80, 0x20));
}
fn paint_18(ui: &mut egui::Ui) {
    paint_sample(ui, red(0x80, 0x18));
}
fn paint_10(ui: &mut egui::Ui) {
    paint_sample(ui, red(0x80, 0x10));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::path::PathBuf::from("/tmp/termica-picker-choice.txt");
    let _ = std::fs::remove_file(&output);
    let choice = run(
        "Failed-block bg dimness (warm dark red wash)",
        vec![
            Variant::new("a40", "A · current (alpha 0x40 ≈ 25%)", paint_current),
            Variant::new("a28", "B · alpha 0x28 (≈16%)", paint_28),
            Variant::new("a20", "C · alpha 0x20 (≈12%)", paint_20),
            Variant::new("a18", "D · alpha 0x18 (≈9%)", paint_18),
            Variant::new("a10", "E · alpha 0x10 (≈6%) — just a tint", paint_10),
        ],
        &output,
    )?;
    match choice {
        Some(id) => println!("picked: {id} (written to {})", output.display()),
        None => println!("cancelled (no file written)"),
    }
    Ok(())
}
