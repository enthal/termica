//! Variants for the "this pane will receive your next keypress"
//! affordance. Separate from the caret, which gates on real focus
//! AND OS foreground; this visual just says "the keyboard belongs
//! to this editor right now."
//!
//! Run:   cargo run --example pick_focused_editor_chrome
//!
//! Picks by writing the variant id to
//! `/tmp/termica-picker-choice.txt`.

#![forbid(unsafe_code)]

use eframe::egui;
use termica::visual_picker::{Variant, run};

/// Match the real editor surface as closely as the picker can: a
/// monospace command line on a dark background, with a faint caret
/// to anchor the visual. The variants differ ONLY in the chrome
/// drawn around it.
const COMMAND_LINE: &str = "echo hello world";

fn editor_size() -> egui::Vec2 {
    egui::vec2(440.0, 36.0)
}

fn paint_editor_body(ui: &mut egui::Ui, rect: egui::Rect) {
    let painter = ui.painter();
    // Body fill (slightly lighter than the panel bg, like a real
    // input field).
    painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(0x10, 0x12, 0x16));
    // Command text.
    let font_id = egui::FontId::monospace(14.0);
    let text_pos = egui::pos2(rect.left() + 10.0, rect.center().y);
    painter.text(
        text_pos,
        egui::Align2::LEFT_CENTER,
        COMMAND_LINE,
        font_id.clone(),
        egui::Color32::from_rgb(0xe6, 0xe6, 0xe6),
    );
    // Faint caret after the text so each variant feels "live".
    let glyph_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let caret_x = rect.left() + 10.0 + glyph_w * COMMAND_LINE.chars().count() as f32 + 1.0;
    let caret_top = rect.top() + 6.0;
    let caret_bot = rect.bottom() - 6.0;
    painter.line_segment(
        [egui::pos2(caret_x, caret_top), egui::pos2(caret_x, caret_bot)],
        egui::Stroke::new(1.5, egui::Color32::from_rgba_premultiplied(0x4a, 0xa8, 0xc0, 0xa0)),
    );
}

fn paint_white_round_rect(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(editor_size(), egui::Sense::hover());
    // Soft white round rect around the editor body.
    ui.painter().rect_stroke(
        rect.expand(2.0),
        6.0,
        egui::Stroke::new(1.5, egui::Color32::from_rgb(0xe6, 0xe6, 0xe6)),
        egui::StrokeKind::Outside,
    );
    paint_editor_body(ui, rect);
}

fn paint_dim_white_round_rect(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(editor_size(), egui::Sense::hover());
    ui.painter().rect_stroke(
        rect.expand(2.0),
        6.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgba_premultiplied(0xa0, 0xa0, 0xa0, 0xb0)),
        egui::StrokeKind::Outside,
    );
    paint_editor_body(ui, rect);
}

fn paint_accent_round_rect(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(editor_size(), egui::Sense::hover());
    ui.painter().rect_stroke(
        rect.expand(2.0),
        6.0,
        egui::Stroke::new(1.5, egui::Color32::from_rgb(0x4a, 0xa8, 0xc0)),
        egui::StrokeKind::Outside,
    );
    paint_editor_body(ui, rect);
}

fn paint_left_accent_bar(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(editor_size(), egui::Sense::hover());
    paint_editor_body(ui, rect);
    // Thick vertical bar to the LEFT of the body, accent color.
    let bar = egui::Rect::from_min_max(
        egui::pos2(rect.left() - 5.0, rect.top()),
        egui::pos2(rect.left() - 2.0, rect.bottom()),
    );
    ui.painter().rect_filled(bar, 1.5, egui::Color32::from_rgb(0x4a, 0xa8, 0xc0));
}

fn paint_full_glow(ui: &mut egui::Ui) {
    // Soft outer glow + faint white outline.
    let (rect, _) = ui.allocate_exact_size(editor_size(), egui::Sense::hover());
    let painter = ui.painter();
    for (offset, alpha) in [(4.0, 30u8), (3.0, 50), (2.0, 80)] {
        painter.rect_stroke(
            rect.expand(offset),
            6.0 + offset,
            egui::Stroke::new(1.0, egui::Color32::from_rgba_premultiplied(0xff, 0xff, 0xff, alpha)),
            egui::StrokeKind::Outside,
        );
    }
    painter.rect_stroke(
        rect.expand(2.0),
        6.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgba_premultiplied(0xff, 0xff, 0xff, 0xc0)),
        egui::StrokeKind::Outside,
    );
    paint_editor_body(ui, rect);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::path::PathBuf::from("/tmp/termica-picker-choice.txt");
    let _ = std::fs::remove_file(&output);
    let choice = run(
        "Focused-editor chrome (when this pane will receive your next keypress)",
        vec![
            Variant::new("white-round-rect", "A · White round rect", paint_white_round_rect),
            Variant::new(
                "dim-white-round-rect",
                "B · Dim grey-white round rect",
                paint_dim_white_round_rect,
            ),
            Variant::new(
                "accent-round-rect",
                "C · Accent (teal) round rect",
                paint_accent_round_rect,
            ),
            Variant::new("left-accent-bar", "D · Left accent bar only", paint_left_accent_bar),
            Variant::new("white-glow", "E · Soft white outer glow", paint_full_glow),
        ],
        &output,
    )?;
    match choice {
        Some(id) => println!("picked: {id} (written to {})", output.display()),
        None => println!("cancelled (no file written)"),
    }
    Ok(())
}
