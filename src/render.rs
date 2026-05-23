//! Custom egui cell renderer for the terminal grid.
//!
//! Phase 1E-b: walk the [`TerminalState`] grid and paint every cell
//! into an [`egui::Painter`]. No reliance on egui's text widgets —
//! cells get an explicit background rectangle (when non-default)
//! plus an explicit glyph draw, so future styled-run batching and
//! per-cell decorations have a clean place to grow.
//!
//! Out of scope for this PR (deliberately): bold / italic /
//! underline rendering, cursor shape / blink, selection / search
//! highlights, styled-run batching as an optimization. Those land
//! across later 1E-* and Phase 2 sub-PRs.
//!
//! See [`spec/02-terminal-engine.md`](../spec/02-terminal-engine.md#rendering)
//! for the rendering contract this implements.

#![forbid(unsafe_code)]

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Vec2};

use crate::terminal::TerminalState;

/// Font size in egui points. Tuned by eye for legibility on the
/// terminals we tested against; configurable later.
pub const DEFAULT_FONT_SIZE: f32 = 14.0;

/// Pane background when no cell-level background applies. Slightly
/// off-black so a cursor block (later) reads as a tile, not a hole.
pub const DEFAULT_BG: Color32 = Color32::from_rgb(0x12, 0x12, 0x12);

/// Default foreground. Off-white; matches alacritty's "light"
/// foreground convention for dark themes.
pub const DEFAULT_FG: Color32 = Color32::from_rgb(0xd8, 0xd8, 0xd8);

/// Paint `term`'s visible grid into the current cursor position.
///
/// Allocates exactly `cols × rows` of monospace metrics from `ui` so
/// the surrounding layout knows we drew there. Caller decides where
/// the grid lives (typically directly inside a `CentralPanel` after
/// the status line).
pub fn paint_terminal(ui: &mut egui::Ui, term: &TerminalState) {
    let font_id = FontId::monospace(DEFAULT_FONT_SIZE);
    // `glyph_width` / `row_height` mutate the font cache as they go,
    // so the `_mut` access is the correct one — `fonts()` (read-only)
    // refuses to call them.
    let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let row_h = ui.fonts_mut(|f| f.row_height(&font_id));

    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();

    let size = Vec2::new(cols as f32 * cell_w, rows as f32 * row_h);
    let (rect, _response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    // One full-pane background draw is cheaper than `rows * cols`
    // default-color fills, so we only paint per-cell backgrounds for
    // cells that actually differ from the pane default.
    painter.rect_filled(rect, 0.0, DEFAULT_BG);

    for row in 0..rows {
        for col in 0..cols {
            let pt = Point::new(Line(row as i32), Column(col));
            let cell = &grid[pt];

            let x = rect.min.x + col as f32 * cell_w;
            let y = rect.min.y + row as f32 * row_h;

            let bg = ansi_to_egui(cell.bg);
            if let Some(c) = bg {
                let cell_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, row_h));
                painter.rect_filled(cell_rect, 0.0, c);
            }

            // Spaces don't paint a glyph (default cells are already
            // covered by the pane background). Non-space glyphs do.
            if cell.c != ' ' {
                let fg = ansi_to_egui(cell.fg).unwrap_or(DEFAULT_FG);
                painter.text(
                    Pos2::new(x, y),
                    egui::Align2::LEFT_TOP,
                    cell.c.to_string(),
                    font_id.clone(),
                    fg,
                );
            }
        }
    }
}

/// Map an alacritty `Color` to an egui `Color32`. Returns `None`
/// when the color is the terminal's logical default (foreground or
/// background) so the caller can substitute its own default — this
/// keeps the "default never paints a per-cell background rect"
/// optimization honest.
fn ansi_to_egui(color: Color) -> Option<Color32> {
    match color {
        Color::Named(n) => named_to_egui(n),
        Color::Spec(Rgb { r, g, b }) => Some(Color32::from_rgb(r, g, b)),
        Color::Indexed(i) => indexed_to_egui(i),
    }
}

fn named_to_egui(n: NamedColor) -> Option<Color32> {
    // Palette tuned for legibility on a dark theme. Bright variants
    // are simply slightly lighter than their base; dim variants
    // slightly darker. The values aren't sacred — they will be
    // replaced by a configurable theme in Phase 10.
    let c = match n {
        NamedColor::Black => Color32::from_rgb(0x12, 0x12, 0x12),
        NamedColor::Red => Color32::from_rgb(0xcd, 0x31, 0x31),
        NamedColor::Green => Color32::from_rgb(0x52, 0xc4, 0x1c),
        NamedColor::Yellow => Color32::from_rgb(0xd6, 0xa6, 0x12),
        NamedColor::Blue => Color32::from_rgb(0x42, 0x7d, 0xd5),
        NamedColor::Magenta => Color32::from_rgb(0xb6, 0x46, 0xc0),
        NamedColor::Cyan => Color32::from_rgb(0x39, 0xa8, 0xb2),
        NamedColor::White => Color32::from_rgb(0xc0, 0xc0, 0xc0),
        NamedColor::BrightBlack => Color32::from_rgb(0x55, 0x55, 0x55),
        NamedColor::BrightRed => Color32::from_rgb(0xff, 0x6b, 0x6b),
        NamedColor::BrightGreen => Color32::from_rgb(0x86, 0xf2, 0x5d),
        NamedColor::BrightYellow => Color32::from_rgb(0xff, 0xd1, 0x46),
        NamedColor::BrightBlue => Color32::from_rgb(0x7c, 0xa9, 0xff),
        NamedColor::BrightMagenta => Color32::from_rgb(0xe2, 0x7b, 0xf1),
        NamedColor::BrightCyan => Color32::from_rgb(0x6c, 0xd9, 0xe6),
        NamedColor::BrightWhite => Color32::from_rgb(0xff, 0xff, 0xff),
        NamedColor::DimBlack => Color32::from_rgb(0x0a, 0x0a, 0x0a),
        NamedColor::DimRed => Color32::from_rgb(0x96, 0x24, 0x24),
        NamedColor::DimGreen => Color32::from_rgb(0x3d, 0x8f, 0x15),
        NamedColor::DimYellow => Color32::from_rgb(0xa0, 0x7c, 0x0e),
        NamedColor::DimBlue => Color32::from_rgb(0x31, 0x5e, 0x9f),
        NamedColor::DimMagenta => Color32::from_rgb(0x86, 0x34, 0x90),
        NamedColor::DimCyan => Color32::from_rgb(0x2a, 0x7e, 0x85),
        NamedColor::DimWhite => Color32::from_rgb(0x8a, 0x8a, 0x8a),
        // Logical defaults: caller supplies its own.
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor
        | NamedColor::BrightForeground
        | NamedColor::DimForeground => return None,
    };
    Some(c)
}

/// Map an xterm 256-color palette index to an egui color.
///
/// Layout (per the de facto xterm convention):
/// - `0..=15`: the same 16 named colors as [`named_to_egui`].
/// - `16..=231`: a 6×6×6 cube where each axis steps through
///   `{0, 0x5f, 0x87, 0xaf, 0xd7, 0xff}`.
/// - `232..=255`: a 24-step grayscale ramp.
fn indexed_to_egui(i: u8) -> Option<Color32> {
    if i <= 15 {
        // Map low indices through the named-color path so the look
        // matches the rest of the palette without duplicating constants.
        let named = match i {
            0 => NamedColor::Black,
            1 => NamedColor::Red,
            2 => NamedColor::Green,
            3 => NamedColor::Yellow,
            4 => NamedColor::Blue,
            5 => NamedColor::Magenta,
            6 => NamedColor::Cyan,
            7 => NamedColor::White,
            8 => NamedColor::BrightBlack,
            9 => NamedColor::BrightRed,
            10 => NamedColor::BrightGreen,
            11 => NamedColor::BrightYellow,
            12 => NamedColor::BrightBlue,
            13 => NamedColor::BrightMagenta,
            14 => NamedColor::BrightCyan,
            _ => NamedColor::BrightWhite,
        };
        return named_to_egui(named);
    }
    if (16..=231).contains(&i) {
        let cube = i - 16;
        let r = cube / 36;
        let g = (cube % 36) / 6;
        let b = cube % 6;
        const STEPS: [u8; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
        return Some(Color32::from_rgb(STEPS[r as usize], STEPS[g as usize], STEPS[b as usize]));
    }
    // 232..=255 grayscale: 8 + 10 * (i - 232).
    let n = 8u16 + 10u16 * (i as u16 - 232);
    let v = n.min(255) as u8;
    Some(Color32::from_rgb(v, v, v))
}

#[cfg(test)]
mod tests {
    //! Pure-color-mapping tests. The render path itself is exercised
    //! by snapshot tests in `tests/snapshots.rs`.

    use super::*;

    #[test]
    fn named_default_returns_none() {
        assert!(named_to_egui(NamedColor::Foreground).is_none());
        assert!(named_to_egui(NamedColor::Background).is_none());
        assert!(named_to_egui(NamedColor::Cursor).is_none());
    }

    #[test]
    fn named_red_is_reddish() {
        let c = named_to_egui(NamedColor::Red).expect("red maps");
        assert!(c.r() > c.g() && c.r() > c.b(), "red channel should dominate: {c:?}");
    }

    #[test]
    fn ansi_spec_passes_through_rgb() {
        let c = ansi_to_egui(Color::Spec(Rgb { r: 1, g: 2, b: 3 })).expect("rgb maps");
        assert_eq!((c.r(), c.g(), c.b()), (1, 2, 3));
    }

    #[test]
    fn indexed_0_through_15_match_named_palette() {
        for (idx, named) in [
            (0, NamedColor::Black),
            (1, NamedColor::Red),
            (7, NamedColor::White),
            (8, NamedColor::BrightBlack),
            (15, NamedColor::BrightWhite),
        ] {
            assert_eq!(indexed_to_egui(idx), named_to_egui(named), "idx {idx}");
        }
    }

    #[test]
    fn indexed_cube_index_16_is_pure_black() {
        let c = indexed_to_egui(16).expect("cube black");
        assert_eq!((c.r(), c.g(), c.b()), (0, 0, 0));
    }

    #[test]
    fn indexed_cube_index_231_is_pure_white() {
        let c = indexed_to_egui(231).expect("cube white");
        assert_eq!((c.r(), c.g(), c.b()), (255, 255, 255));
    }

    #[test]
    fn indexed_grayscale_is_grayscale() {
        for i in 232..=255 {
            let c = indexed_to_egui(i).expect("gray");
            assert_eq!(c.r(), c.g());
            assert_eq!(c.g(), c.b());
        }
    }
}
