//! Twelve variants for the focused-editor affordance — the visual
//! that says "this pane will receive your next keypress." Includes
//! the chip bar in every variant so you can see how the candidate
//! treats the prompt as a unit. Wraps top + bottom (the previous
//! variant cut off at the pane bottom).
//!
//! Run:   cargo run --example pick_focused_editor_chrome_v2
//!
//! Picks by writing the variant id to
//! `/tmp/termica-picker-choice.txt`.

#![forbid(unsafe_code)]

use eframe::egui;
use termica::visual_picker::{Variant, run};

const CHIP_TEXT: &str = "~/git/enthal/termica";
const COMMAND: &str = "echo hello world";
const ROW_H: f32 = 30.0;
const CHIP_H: f32 = 20.0;
const PANEL_W: f32 = 480.0;
const PANEL_INNER_PAD: f32 = 6.0;

// All variants paint the same "prompt body" (chip + editor row);
// they differ in the chrome around it.

fn body_size() -> egui::Vec2 {
    egui::vec2(PANEL_W, CHIP_H + ROW_H + 4.0)
}

fn paint_chip(
    painter: &egui::Painter,
    origin: egui::Pos2,
    fonts_id: &egui::FontId,
    ui_color_strong: egui::Color32,
) -> egui::Rect {
    let pad_x = 6.0;
    let pad_y = 2.0;
    // Estimate chip width by char count; close enough for the picker.
    let chip_w = CHIP_TEXT.chars().count() as f32 * 7.5 + 2.0 * pad_x;
    let chip_rect = egui::Rect::from_min_size(origin, egui::vec2(chip_w, CHIP_H));
    painter.rect_filled(chip_rect, 4.0, egui::Color32::from_rgb(0x22, 0x22, 0x22));
    painter.rect_stroke(
        chip_rect,
        4.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(0x44, 0x44, 0x44)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        chip_rect.min + egui::vec2(pad_x, pad_y),
        egui::Align2::LEFT_TOP,
        CHIP_TEXT,
        fonts_id.clone(),
        ui_color_strong,
    );
    chip_rect
}

fn paint_editor_row(painter: &egui::Painter, origin: egui::Pos2, w: f32, fonts_id: &egui::FontId) {
    let row_rect = egui::Rect::from_min_size(origin, egui::vec2(w, ROW_H));
    painter.text(
        row_rect.left_center() + egui::vec2(2.0, 0.0),
        egui::Align2::LEFT_CENTER,
        COMMAND,
        fonts_id.clone(),
        egui::Color32::from_rgb(0x6e, 0xd0, 0xe8),
    );
    // Caret-ish tick after the command so the visual reads as
    // a live editor.
    let glyph_w = 7.5;
    let cx = row_rect.left() + 2.0 + glyph_w * COMMAND.chars().count() as f32 + 1.0;
    painter.line_segment(
        [egui::pos2(cx, row_rect.top() + 6.0), egui::pos2(cx, row_rect.bottom() - 6.0)],
        egui::Stroke::new(1.5, egui::Color32::from_rgba_premultiplied(0x4a, 0xa8, 0xc0, 0xa0)),
    );
}

fn allocate_body(ui: &mut egui::Ui) -> (egui::Rect, egui::Painter, egui::FontId, egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(body_size(), egui::Sense::hover());
    let painter = ui.painter().clone();
    let fonts_id = egui::FontId::monospace(13.0);
    let chip_fg = egui::Color32::from_rgb(0xa0, 0xa0, 0xa0);
    (rect, painter, fonts_id, chip_fg)
}

fn paint_body(
    rect: egui::Rect,
    painter: &egui::Painter,
    fonts_id: &egui::FontId,
    chip_fg: egui::Color32,
) {
    paint_chip(painter, rect.min, fonts_id, chip_fg);
    paint_editor_row(painter, rect.min + egui::vec2(0.0, CHIP_H + 4.0), rect.width(), fonts_id);
}

// --- variants -------------------------------------------------------------

/// A. Full rounded outline, dim grey-white (current production look).
fn v_full_outline(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    painter.rect_stroke(
        rect.expand(3.0),
        6.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0xc0, 0xc0, 0xc0, 0xb0)),
        egui::StrokeKind::Outside,
    );
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

/// B. Full rounded outline, dimmer (alpha 0x60).
fn v_full_outline_dimmer(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    painter.rect_stroke(
        rect.expand(3.0),
        6.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0xc0, 0xc0, 0xc0, 0x60)),
        egui::StrokeKind::Outside,
    );
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

/// C. Filled body bg (slightly lighter than panel), no border.
fn v_filled_body(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    painter.rect_filled(
        rect.expand(3.0),
        6.0,
        egui::Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 0x10),
    );
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

/// D. Filled body + thin top + bottom border (no sides). Reads as
///    a "lane" inside the panel.
fn v_lane(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    let exp = rect.expand2(egui::vec2(0.0, 3.0));
    painter.rect_filled(exp, 0.0, egui::Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 0x08));
    let stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0xc0, 0xc0, 0xc0, 0x40));
    painter.line_segment([exp.left_top(), exp.right_top()], stroke);
    painter.line_segment([exp.left_bottom(), exp.right_bottom()], stroke);
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

/// E. Left accent bar (teal, 3px) — minimalist, just a margin marker.
fn v_left_bar(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    let bar = egui::Rect::from_min_max(
        egui::pos2(rect.left() - 6.0, rect.top() - 2.0),
        egui::pos2(rect.left() - 3.0, rect.bottom() + 2.0),
    );
    painter.rect_filled(bar, 1.5, egui::Color32::from_rgb(0x4a, 0xa8, 0xc0));
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

/// F. Left accent bar + thin bottom underline (the bar marks
///    "current" + the underline scopes the line).
fn v_left_bar_underline(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    let bar = egui::Rect::from_min_max(
        egui::pos2(rect.left() - 5.0, rect.top() - 2.0),
        egui::pos2(rect.left() - 2.0, rect.bottom() + 2.0),
    );
    painter.rect_filled(bar, 1.5, egui::Color32::from_rgb(0x4a, 0xa8, 0xc0));
    paint_body(rect, &painter, &fonts_id, chip_fg);
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.bottom() + 4.0),
            egui::pos2(rect.right(), rect.bottom() + 4.0),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0x4a, 0xa8, 0xc0, 0x80)),
    );
}

/// G. Bottom-only underline (teal, 2px). Reads as "this line owns
///    the keyboard" without any framing.
fn v_bottom_underline(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    paint_body(rect, &painter, &fonts_id, chip_fg);
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.bottom() + 4.0),
            egui::pos2(rect.right(), rect.bottom() + 4.0),
        ],
        egui::Stroke::new(2.0, egui::Color32::from_rgba_unmultiplied(0x4a, 0xa8, 0xc0, 0xc0)),
    );
}

/// H. Corner brackets only (top-left + bottom-right). Sci-fi targeting
///    reticle vibe, very minimal.
fn v_corner_brackets(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    let r = rect.expand(3.0);
    let bracket_len = 12.0;
    let stroke =
        egui::Stroke::new(1.5, egui::Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 0xa0));
    painter.line_segment([r.left_top(), r.left_top() + egui::vec2(bracket_len, 0.0)], stroke);
    painter.line_segment([r.left_top(), r.left_top() + egui::vec2(0.0, bracket_len)], stroke);
    painter
        .line_segment([r.right_bottom(), r.right_bottom() - egui::vec2(bracket_len, 0.0)], stroke);
    painter
        .line_segment([r.right_bottom(), r.right_bottom() - egui::vec2(0.0, bracket_len)], stroke);
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

/// I. Soft outer glow only (no hard line). Reads as "this area
///    is the focus."
fn v_glow(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    for (offset, alpha) in [(5.0, 0x18u8), (3.5, 0x30), (2.0, 0x60)] {
        painter.rect_stroke(
            rect.expand(offset),
            6.0 + offset,
            egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, alpha)),
            egui::StrokeKind::Outside,
        );
    }
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

/// J. Accent (teal) full rounded outline.
fn v_accent_outline(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    painter.rect_stroke(
        rect.expand(3.0),
        6.0,
        egui::Stroke::new(1.5, egui::Color32::from_rgb(0x4a, 0xa8, 0xc0)),
        egui::StrokeKind::Outside,
    );
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

/// K. Doubled hairline (two 1px stripes, 2px apart). Print-margin look.
fn v_doubled_hairline(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    let stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0xc0, 0xc0, 0xc0, 0x80));
    painter.rect_stroke(rect.expand(2.0), 5.0, stroke, egui::StrokeKind::Outside);
    painter.rect_stroke(rect.expand(5.0), 7.0, stroke, egui::StrokeKind::Outside);
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

/// L. Chip-bar tint only (the cwd row gets a subtle accent tint;
///    the editor body stays plain). Inverse of "outline the whole".
fn v_chip_tint(ui: &mut egui::Ui) {
    let (rect, painter, fonts_id, chip_fg) = allocate_body(ui);
    // Background tint behind the chip area only.
    let chip_area = egui::Rect::from_min_size(
        rect.min + egui::vec2(-PANEL_INNER_PAD, -2.0),
        egui::vec2(rect.width() + 2.0 * PANEL_INNER_PAD, CHIP_H + 4.0),
    );
    painter.rect_filled(
        chip_area,
        4.0,
        egui::Color32::from_rgba_unmultiplied(0x4a, 0xa8, 0xc0, 0x18),
    );
    paint_body(rect, &painter, &fonts_id, chip_fg);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::path::PathBuf::from("/tmp/termica-picker-choice.txt");
    let _ = std::fs::remove_file(&output);
    let choice = run(
        "Focused-editor affordance — pick the visual that says \"keyboard lands here\"",
        vec![
            Variant::new(
                "a-full-outline",
                "A · Dim grey-white round outline (current)",
                v_full_outline,
            ),
            Variant::new(
                "b-full-outline-dimmer",
                "B · Dim grey-white round outline, dimmer",
                v_full_outline_dimmer,
            ),
            Variant::new("c-filled-body", "C · Subtle filled bg, no border", v_filled_body),
            Variant::new("d-lane", "D · Filled bg + top/bottom rule (lane)", v_lane),
            Variant::new("e-left-bar", "E · Left accent bar only (minimalist)", v_left_bar),
            Variant::new(
                "f-left-bar-underline",
                "F · Left bar + bottom underline",
                v_left_bar_underline,
            ),
            Variant::new(
                "g-bottom-underline",
                "G · Bottom underline (teal, 2px)",
                v_bottom_underline,
            ),
            Variant::new("h-corner-brackets", "H · Corner brackets (reticle)", v_corner_brackets),
            Variant::new("i-glow", "I · Soft outer glow (no hard line)", v_glow),
            Variant::new("j-accent-outline", "J · Accent (teal) round outline", v_accent_outline),
            Variant::new(
                "k-doubled-hairline",
                "K · Doubled hairline (print-margin)",
                v_doubled_hairline,
            ),
            Variant::new("l-chip-tint", "L · Chip-bar tint only", v_chip_tint),
        ],
        &output,
    )?;
    match choice {
        Some(id) => println!("picked: {id} (written to {})", output.display()),
        None => println!("cancelled (no file written)"),
    }
    Ok(())
}
