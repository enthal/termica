//! Row-separator variants for the Ctrl+R history overlay.
//!
//! Run:   cargo run --example pick_history_row_separator
//!
//! Picks the winner by writing its kebab-case id to
//! `/tmp/termica-picker-choice.txt`.

#![forbid(unsafe_code)]

use eframe::egui;
use termica::visual_picker::{Variant, run};

/// Fake row data shared across every variant so the user is
/// comparing the SEPARATOR, not the row content.
const FAKE_ROWS: &[(&str, &str)] = &[
    ("cargo test --workspace", "2m · ~/git/enthal/termica · exit 0"),
    ("git status", "8m · ~/git/enthal/termica · exit 0"),
    ("ls -la", "3h · zsh"),
    ("cd src", "1d · zsh"),
    ("vim CLAUDE.md", "3d · bash"),
];

const ROW_W: f32 = 480.0;

fn paint_row(ui: &mut egui::Ui, text: &str, meta: &str) {
    ui.vertical(|ui| {
        ui.set_width(ROW_W);
        ui.label(egui::RichText::new(text).monospace().strong());
        ui.weak(meta);
    });
}

fn paint_more_spacing(ui: &mut egui::Ui) {
    for (text, meta) in FAKE_ROWS {
        paint_row(ui, text, meta);
        ui.add_space(8.0);
    }
}

fn paint_hairline(ui: &mut egui::Ui) {
    let n = FAKE_ROWS.len();
    for (i, (text, meta)) in FAKE_ROWS.iter().enumerate() {
        paint_row(ui, text, meta);
        if i + 1 < n {
            let rect = ui.allocate_space(egui::vec2(ROW_W, 1.0)).1;
            let stroke = egui::Stroke::new(1.0, ui.visuals().text_color().gamma_multiply(0.18));
            ui.painter().line_segment(
                [
                    egui::pos2(rect.left(), rect.center().y),
                    egui::pos2(rect.right(), rect.center().y),
                ],
                stroke,
            );
        }
    }
}

fn paint_alternating_bg(ui: &mut egui::Ui) {
    let alt = ui.visuals().widgets.noninteractive.bg_fill.gamma_multiply(1.4);
    for (i, (text, meta)) in FAKE_ROWS.iter().enumerate() {
        let frame = if i % 2 == 1 { egui::Frame::NONE.fill(alt) } else { egui::Frame::NONE };
        frame.show(ui, |ui| paint_row(ui, text, meta));
    }
}

fn paint_dotted_rule(ui: &mut egui::Ui) {
    let n = FAKE_ROWS.len();
    for (i, (text, meta)) in FAKE_ROWS.iter().enumerate() {
        paint_row(ui, text, meta);
        if i + 1 < n {
            ui.add_space(2.0);
            ui.weak(
                egui::RichText::new("·  ·  ·  ·  ·  ·  ·  ·  ·  ·  ·  ·  ·  ·  ·  ·  ·")
                    .monospace(),
            );
            ui.add_space(2.0);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::path::PathBuf::from("/tmp/termica-picker-choice.txt");
    // Wipe a stale choice from a previous run so absence means
    // "cancelled" for the caller.
    let _ = std::fs::remove_file(&output);
    let choice = run(
        "Ctrl+R row separator",
        vec![
            Variant::new("more-spacing", "A · More vertical space", paint_more_spacing),
            Variant::new("hairline", "B · Faint hairline between rows", paint_hairline),
            Variant::new("alternating-bg", "C · Alternating row backgrounds", paint_alternating_bg),
            Variant::new("dotted-rule", "D · Dotted rule between rows", paint_dotted_rule),
        ],
        &output,
    )?;
    match choice {
        Some(id) => println!("picked: {id} (written to {})", output.display()),
        None => println!("cancelled (no file written)"),
    }
    Ok(())
}
