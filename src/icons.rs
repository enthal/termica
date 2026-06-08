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
    let (tip_y, tail_y) = if down { (c.y + s, c.y - s) } else { (c.y - s, c.y + s) };
    // Stem.
    painter.line_segment([egui::pos2(c.x, tail_y), egui::pos2(c.x, tip_y)], stroke);
    // Chevron head at the tip — slightly longer arms for a cleaner
    // arrow than a stubby caret.
    let arm = s * 0.85;
    let arm_y = if down { c.y + s - arm } else { c.y - s + arm };
    painter.line_segment([egui::pos2(c.x - arm, arm_y), egui::pos2(c.x, tip_y)], stroke);
    painter.line_segment([egui::pos2(c.x + arm, arm_y), egui::pos2(c.x, tip_y)], stroke);
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

/// Draw the macOS Command (⌘) "looped square" — the four edges of a
/// central square (a `#`) capped by a circular loop tangent at each
/// corner. Geometry follows the standard glyph (tabler / Susan Kare
/// proportions): loop centres at ±`a`, radius `r`, straight edges at
/// ±`(a − r)` spanning the full ±`a` so each is tangent to two loops.
/// Drawn rather than using the Unicode ⌘ (CLAUDE.md "no Unicode symbols
/// for icons").
pub fn paint_command_symbol(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let ext = rect.height() * 0.30; // glyph half-extent (loop far edge = a + r)
    let r = ext * 0.30; // loop radius
    let a = ext - r; // loop-centre offset → outer edge at `ext`
    let inner = a - r; // straight edges, tangent to the loops
    let stroke = egui::Stroke::new(1.3, color);

    // Four straight edges forming a `#`: horizontals at y = ±inner and
    // verticals at x = ±inner, each spanning the full ±a so its ends
    // land on the tangent points of the two loops it joins.
    painter
        .line_segment([egui::pos2(c.x - a, c.y - inner), egui::pos2(c.x + a, c.y - inner)], stroke);
    painter
        .line_segment([egui::pos2(c.x - a, c.y + inner), egui::pos2(c.x + a, c.y + inner)], stroke);
    painter
        .line_segment([egui::pos2(c.x - inner, c.y - a), egui::pos2(c.x - inner, c.y + a)], stroke);
    painter
        .line_segment([egui::pos2(c.x + inner, c.y - a), egui::pos2(c.x + inner, c.y + a)], stroke);
    // A loop ring at each corner, tangent to the two edges meeting there.
    for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
        painter.circle_stroke(egui::pos2(c.x + sx * a, c.y + sy * a), r, stroke);
    }
}

/// Draw the macOS Option (⌥) key glyph: a short horizontal bar at the
/// bottom-left and a longer one at the top-right, joined by a diagonal
/// (bottom-left bar's right end up to the top-right bar's left end).
pub fn paint_option_symbol(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let e = rect.height() * 0.26;
    let stroke = egui::Stroke::new(1.3, color);
    // Bottom-left short bar.
    let bl_l = egui::pos2(c.x - e, c.y + e);
    let bl_r = egui::pos2(c.x - e * 0.3, c.y + e);
    painter.line_segment([bl_l, bl_r], stroke);
    // Top-right long bar.
    let tr_l = egui::pos2(c.x + e * 0.1, c.y - e);
    let tr_r = egui::pos2(c.x + e, c.y - e);
    painter.line_segment([tr_l, tr_r], stroke);
    // Diagonal joining them.
    painter.line_segment([bl_r, tr_l], stroke);
}

/// Draw the Control (⌃) key glyph: an upward chevron.
pub fn paint_control_symbol(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let w = rect.height() * 0.24;
    let h = rect.height() * 0.16;
    let stroke = egui::Stroke::new(1.4, color);
    let tip = egui::pos2(c.x, c.y - h);
    painter.line_segment([egui::pos2(c.x - w, c.y + h), tip], stroke);
    painter.line_segment([egui::pos2(c.x + w, c.y + h), tip], stroke);
}
