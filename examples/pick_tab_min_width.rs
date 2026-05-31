//! Tab minimum-width variants — the cwd-based titles get tiny when
//! the path collapses to `~`, which is also the *default* start
//! cwd, so the user sees the worst case immediately on launch.
//!
//! Run:   cargo run --example pick_tab_min_width
//!
//! Picks by writing the variant id to
//! `/tmp/termica-picker-choice.txt`.

#![forbid(unsafe_code)]

use eframe::egui;
use termica::visual_picker::{Variant, run};

/// Tab title used in every variant — the worst-case "~" that
/// motivated the change.
const SHORT_TITLE: &str = "~";

/// Approximate measured width of "~" at the egui default font:
/// roughly 16 px including padding. Each variant multiplies this
/// by an integer factor.
const SHORT_BASE_PX: f32 = 16.0;

fn paint_tab(ui: &mut egui::Ui, label: &str, min_w: f32) {
    let height = 26.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(min_w, height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, ui.visuals().widgets.active.bg_fill);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(13.0),
        ui.visuals().strong_text_color(),
    );
}

fn paint_strip(ui: &mut egui::Ui, min_w: f32) {
    // Three tabs in a strip so the user sees how a min-width row
    // looks. The middle one is the worst-case "~".
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        paint_tab(ui, "first-tab", min_w.max(72.0));
        paint_tab(ui, SHORT_TITLE, min_w);
        paint_tab(ui, "another-tab", min_w.max(96.0));
        ui.add_space(6.0);
        ui.label("[+]");
    });
    ui.add_space(4.0);
    ui.weak(format!(
        "min width = {min_w:.0}px ({:.1}× the natural \"~\" width)",
        min_w / SHORT_BASE_PX
    ));
}

fn paint_natural(ui: &mut egui::Ui) {
    // Today's behaviour — no minimum.
    paint_strip(ui, SHORT_BASE_PX);
}

fn paint_2x(ui: &mut egui::Ui) {
    paint_strip(ui, SHORT_BASE_PX * 2.0);
}

fn paint_3x(ui: &mut egui::Ui) {
    paint_strip(ui, SHORT_BASE_PX * 3.0);
}

fn paint_5x(ui: &mut egui::Ui) {
    paint_strip(ui, SHORT_BASE_PX * 5.0);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::path::PathBuf::from("/tmp/termica-picker-choice.txt");
    let _ = std::fs::remove_file(&output);
    let choice = run(
        "Tab minimum width (when title collapses to \"~\")",
        vec![
            Variant::new("natural", "A · Natural width (current; ~16px for \"~\")", paint_natural),
            Variant::new("2x", "B · 2× = ~32px", paint_2x),
            Variant::new("3x", "C · 3× = ~48px (the user's suggestion)", paint_3x),
            Variant::new("5x", "D · 5× = ~80px", paint_5x),
        ],
        &output,
    )?;
    match choice {
        Some(id) => println!("picked: {id} (written to {})", output.display()),
        None => println!("cancelled (no file written)"),
    }
    Ok(())
}
