//! Painter-drawn icons.
//!
//! Termica never uses Unicode glyphs for UI icons — they render as
//! tofu on systems without the right font (CLAUDE.md "Code style").
//! Every icon here is drawn with egui's [`egui::Painter`], mirroring
//! the knauty `icons.rs` pattern: a small [`icon_button`] helper that
//! allocates an interact-sized square, picks a hover/normal colour,
//! and hands a painter + rect to a closure.
//!
//! Used by the find bar, the completion popup, and the keybindings
//! cheat-sheet. (Phase 5's status-header `icons.rs` folds in here.)

#![forbid(unsafe_code)]

use eframe::egui;

/// Allocate an interact-sized square button and paint an icon into it.
/// Returns the [`egui::Response`] so callers can `.clicked()` /
/// `.on_hover_text()`. The icon colour follows hover state.
pub fn icon_button(
    ui: &mut egui::Ui,
    hover_text: &str,
    paint: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
) -> egui::Response {
    let size = egui::Vec2::splat(ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let color = if response.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    paint(ui.painter(), rect, color);
    response.on_hover_text(hover_text)
}

/// Same as [`icon_button`] but greyed + non-interactive when `enabled`
/// is false (egui's disabled visuals), for Prev/Next with no matches.
pub fn icon_button_enabled(
    ui: &mut egui::Ui,
    enabled: bool,
    hover_text: &str,
    paint: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
) -> egui::Response {
    let size = egui::Vec2::splat(ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(
        size,
        if enabled { egui::Sense::click() } else { egui::Sense::hover() },
    );
    let color = if !enabled {
        ui.visuals().weak_text_color()
    } else if response.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    paint(ui.painter(), rect, color);
    if enabled { response.on_hover_text(hover_text) } else { response }
}

/// Draw a thin up- or down-pointing arrow (stem + chevron head) into
/// `rect`. `down == true` points down. Used for find next/prev and the
/// completion-popup navigate hint — the clean line-arrow look of a
/// modern find bar rather than a fat triangle.
pub fn paint_arrow_glyph(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    down: bool,
) {
    let c = rect.center();
    let s = rect.height() * 0.26;
    let stroke = egui::Stroke::new(1.6, color);
    let (tip_y, tail_y, head_y) =
        if down { (c.y + s, c.y - s, c.y + s * 0.35) } else { (c.y - s, c.y + s, c.y - s * 0.35) };
    // Stem.
    painter.line_segment([egui::pos2(c.x, tail_y), egui::pos2(c.x, tip_y)], stroke);
    // Chevron head at the tip.
    painter.line_segment([egui::pos2(c.x - s * 0.6, head_y), egui::pos2(c.x, tip_y)], stroke);
    painter.line_segment([egui::pos2(c.x + s * 0.6, head_y), egui::pos2(c.x, tip_y)], stroke);
}

/// Up-arrow button — "previous match" (find searches from the bottom,
/// so previous = up = older).
pub fn arrow_up_button(ui: &mut egui::Ui, enabled: bool, hover: &str) -> egui::Response {
    icon_button_enabled(ui, enabled, hover, |p, r, c| paint_arrow_glyph(p, r, c, false))
}

/// Down-arrow button — "next match".
pub fn arrow_down_button(ui: &mut egui::Ui, enabled: bool, hover: &str) -> egui::Response {
    icon_button_enabled(ui, enabled, hover, |p, r, c| paint_arrow_glyph(p, r, c, true))
}

/// Close (✕) button — two diagonal strokes. Mirrors knauty's
/// `ui_close_button` shape.
pub fn close_button(ui: &mut egui::Ui, hover: &str) -> egui::Response {
    icon_button(ui, hover, |painter, rect, color| {
        let r = rect.shrink(rect.height() * 0.32);
        let stroke = egui::Stroke::new(1.6, color);
        painter.line_segment([r.left_top(), r.right_bottom()], stroke);
        painter.line_segment([r.right_top(), r.left_bottom()], stroke);
    })
}

/// Paint a small filled down-pointing triangle — the "this opens a
/// dropdown" indicator. Raw paint helper; the caller owns layout.
pub fn paint_dropdown_triangle(
    painter: &egui::Painter,
    center: egui::Pos2,
    size: f32,
    color: egui::Color32,
) {
    let pts = vec![
        egui::pos2(center.x - size, center.y - size * 0.5),
        egui::pos2(center.x + size, center.y - size * 0.5),
        egui::pos2(center.x, center.y + size * 0.5),
    ];
    painter.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
}

/// A standalone dropdown-arrow button (the find-history `▾`).
pub fn dropdown_button(ui: &mut egui::Ui, hover: &str) -> egui::Response {
    icon_button(ui, hover, |painter, rect, color| {
        paint_dropdown_triangle(painter, rect.center(), rect.height() * 0.18, color);
    })
}

/// White when a block section is included in the search, dark grey when
/// excluded.
const SECTION_ON: egui::Color32 = egui::Color32::from_rgb(0xe0, 0xe0, 0xe0);
const SECTION_OFF: egui::Color32 = egui::Color32::from_rgb(0x55, 0x55, 0x55);

/// Draw the block-section filter glyph into `rect`: a horizontal line
/// (the command line of a block) above a rectangle (its output area).
/// Each is white when its section is included in the search and dark
/// grey when excluded — so "All" is both white, "Commands" is a white
/// line over a grey box, "Outputs" a grey line over a white box.
pub fn paint_block_filter_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    command_on: bool,
    output_on: bool,
) {
    let c = rect.center();
    let w = rect.height() * 0.34;
    let cmd_color = if command_on { SECTION_ON } else { SECTION_OFF };
    let out_color = if output_on { SECTION_ON } else { SECTION_OFF };
    // Command line: a short horizontal bar near the top.
    let line_y = c.y - w * 0.95;
    painter.line_segment(
        [egui::pos2(c.x - w, line_y), egui::pos2(c.x + w, line_y)],
        egui::Stroke::new(1.8, cmd_color),
    );
    // Output area: a filled rectangle below it.
    let body = egui::Rect::from_min_max(
        egui::pos2(c.x - w, c.y - w * 0.35),
        egui::pos2(c.x + w, c.y + w * 1.05),
    );
    painter.rect_filled(body, 1.0, out_color);
}

/// A clickable block-section filter chip: the [`paint_block_filter_icon`]
/// glyph in an interact-sized square. Returns the response.
pub fn block_filter_button(
    ui: &mut egui::Ui,
    command_on: bool,
    output_on: bool,
    hover: &str,
) -> egui::Response {
    let size = egui::Vec2::splat(ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    paint_block_filter_icon(ui.painter(), rect, command_on, output_on);
    response.on_hover_text(hover)
}

/// Draw the macOS Command (⌘) symbol — the "looped square": a small
/// square with an open loop at each corner. Used in the keybindings
/// cheat-sheet so the Cmd modifier reads natively without a Unicode
/// glyph. `color` and `rect` set the ink and bounds.
pub fn paint_command_symbol(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let s = rect.height() * 0.22; // half-side of the central square
    let r = s * 0.6; // loop radius
    let stroke = egui::Stroke::new(1.5, color);
    // Central square joining the four loop centres.
    let corners = [
        egui::pos2(c.x - s, c.y - s),
        egui::pos2(c.x + s, c.y - s),
        egui::pos2(c.x + s, c.y + s),
        egui::pos2(c.x - s, c.y + s),
    ];
    for i in 0..4 {
        painter.line_segment([corners[i], corners[(i + 1) % 4]], stroke);
        painter.circle_stroke(corners[i], r, stroke);
    }
}
