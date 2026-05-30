//! Visual variants for the focused-editor affordance — the chrome
//! drawn around the chip + editor when this pane is the OS keyboard
//! target. The choice is live-tunable via a second OS window (the
//! "Visual Picker" viewport spawned with `--pick-chrome`) so the
//! comparison happens IN context, not in a mockup.
//!
//! See [spec/04 §"Block visual chrome (shipped)"](../spec/04-prompt-editor.md#block-visual-chrome-shipped).

#![forbid(unsafe_code)]

use eframe::egui;

/// Which chrome to paint. Defaults to [`Self::A`] (the round outline
/// originally picked in `pick_focused_editor_chrome`). New variants
/// are appended; the `id()` / `from_id()` round-trip stays stable so
/// a saved choice in `/tmp` or settings keeps working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChromeVariant {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    /// Default: soft outer glow. Picked via the in-app
    /// `--pick-chrome` viewport — reads as "this area is the focus"
    /// without a hard frame that competes with content.
    #[default]
    I,
    J,
    K,
    L,
}

impl ChromeVariant {
    /// (variant, kebab id, picker label). Drives both the live
    /// picker viewport and any future "remember last pick" round-trip.
    pub const ALL: &'static [(ChromeVariant, &'static str, &'static str)] = &[
        (Self::A, "a-full-outline", "A · Dim grey-white round outline (default)"),
        (Self::B, "b-full-outline-dimmer", "B · Dim outline, dimmer (alpha 0x60)"),
        (Self::C, "c-filled-body", "C · Subtle filled bg, no border"),
        (Self::D, "d-lane", "D · Filled bg + top/bottom rule (lane)"),
        (Self::E, "e-left-bar", "E · Left accent bar only (minimalist)"),
        (Self::F, "f-left-bar-underline", "F · Left bar + bottom underline"),
        (Self::G, "g-bottom-underline", "G · Bottom underline (teal 2px)"),
        (Self::H, "h-corner-brackets", "H · Corner brackets (reticle)"),
        (Self::I, "i-glow", "I · Soft outer glow (no hard line)"),
        (Self::J, "j-accent-outline", "J · Accent (teal) round outline"),
        (Self::K, "k-doubled-hairline", "K · Doubled hairline (print-margin)"),
        (Self::L, "l-chip-tint", "L · Chip-bar tint only"),
    ];

    pub fn id(self) -> &'static str {
        Self::ALL
            .iter()
            .find_map(|(v, id, _)| if *v == self { Some(*id) } else { None })
            .unwrap_or("a-full-outline")
    }

    pub fn label(self) -> &'static str {
        Self::ALL
            .iter()
            .find_map(|(v, _, label)| if *v == self { Some(*label) } else { None })
            .unwrap_or("?")
    }
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x4a, 0xa8, 0xc0);

fn unmul(r: u8, g: u8, b: u8, a: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// Paint the chosen chrome around the editor area. `body_rect` is
/// the combined chip-bar + editor footprint. `chip_rect` is `Some`
/// when the chip bar is showing — used by variant L (chip-bar tint)
/// which decorates only the chip portion.
pub fn paint(
    painter: &egui::Painter,
    chip_rect: Option<egui::Rect>,
    body_rect: egui::Rect,
    variant: ChromeVariant,
) {
    use ChromeVariant::*;
    match variant {
        A => stroke_round(painter, body_rect, 1.0, unmul(0xc0, 0xc0, 0xc0, 0xb0)),
        B => stroke_round(painter, body_rect, 1.0, unmul(0xc0, 0xc0, 0xc0, 0x60)),
        C => fill_round(painter, body_rect, unmul(0xff, 0xff, 0xff, 0x10)),
        D => paint_lane(painter, body_rect),
        E => paint_left_bar(painter, body_rect),
        F => {
            paint_left_bar(painter, body_rect);
            paint_bottom_rule(
                painter,
                body_rect,
                1.0,
                unmul(ACCENT.r(), ACCENT.g(), ACCENT.b(), 0x80),
            );
        }
        G => paint_bottom_rule(
            painter,
            body_rect,
            2.0,
            unmul(ACCENT.r(), ACCENT.g(), ACCENT.b(), 0xc0),
        ),
        H => paint_corner_brackets(painter, body_rect),
        I => paint_glow(painter, body_rect),
        J => stroke_round(painter, body_rect, 1.5, ACCENT),
        K => paint_doubled_hairline(painter, body_rect),
        L => paint_chip_tint(painter, chip_rect),
    }
}

fn stroke_round(painter: &egui::Painter, rect: egui::Rect, width: f32, color: egui::Color32) {
    painter.rect_stroke(
        rect.expand(3.0),
        6.0,
        egui::Stroke::new(width, color),
        egui::StrokeKind::Outside,
    );
}

fn fill_round(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    painter.rect_filled(rect.expand(3.0), 6.0, color);
}

fn paint_lane(painter: &egui::Painter, rect: egui::Rect) {
    let exp = rect.expand2(egui::vec2(0.0, 3.0));
    painter.rect_filled(exp, 0.0, unmul(0xff, 0xff, 0xff, 0x08));
    let stroke = egui::Stroke::new(1.0, unmul(0xc0, 0xc0, 0xc0, 0x40));
    painter.line_segment([exp.left_top(), exp.right_top()], stroke);
    painter.line_segment([exp.left_bottom(), exp.right_bottom()], stroke);
}

fn paint_left_bar(painter: &egui::Painter, rect: egui::Rect) {
    let bar = egui::Rect::from_min_max(
        egui::pos2(rect.left() - 6.0, rect.top() - 2.0),
        egui::pos2(rect.left() - 3.0, rect.bottom() + 2.0),
    );
    painter.rect_filled(bar, 1.5, ACCENT);
}

fn paint_bottom_rule(painter: &egui::Painter, rect: egui::Rect, width: f32, color: egui::Color32) {
    let y = rect.bottom() + 4.0;
    painter.line_segment(
        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
        egui::Stroke::new(width, color),
    );
}

fn paint_corner_brackets(painter: &egui::Painter, rect: egui::Rect) {
    let r = rect.expand(3.0);
    let len = 12.0;
    let stroke = egui::Stroke::new(1.5, unmul(0xff, 0xff, 0xff, 0xa0));
    painter.line_segment([r.left_top(), r.left_top() + egui::vec2(len, 0.0)], stroke);
    painter.line_segment([r.left_top(), r.left_top() + egui::vec2(0.0, len)], stroke);
    painter.line_segment([r.right_bottom(), r.right_bottom() - egui::vec2(len, 0.0)], stroke);
    painter.line_segment([r.right_bottom(), r.right_bottom() - egui::vec2(0.0, len)], stroke);
}

fn paint_glow(painter: &egui::Painter, rect: egui::Rect) {
    for (offset, alpha) in [(5.0f32, 0x18u8), (3.5, 0x30), (2.0, 0x60)] {
        painter.rect_stroke(
            rect.expand(offset),
            6.0 + offset,
            egui::Stroke::new(1.0, unmul(0xff, 0xff, 0xff, alpha)),
            egui::StrokeKind::Outside,
        );
    }
}

fn paint_doubled_hairline(painter: &egui::Painter, rect: egui::Rect) {
    let stroke = egui::Stroke::new(1.0, unmul(0xc0, 0xc0, 0xc0, 0x80));
    painter.rect_stroke(rect.expand(2.0), 5.0, stroke, egui::StrokeKind::Outside);
    painter.rect_stroke(rect.expand(5.0), 7.0, stroke, egui::StrokeKind::Outside);
}

fn paint_chip_tint(painter: &egui::Painter, chip_rect: Option<egui::Rect>) {
    let Some(rect) = chip_rect else { return };
    let area = rect.expand2(egui::vec2(2.0, 2.0));
    painter.rect_filled(area, 4.0, unmul(ACCENT.r(), ACCENT.g(), ACCENT.b(), 0x18));
}
