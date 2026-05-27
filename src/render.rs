//! Custom egui cell renderer for the terminal grid.
//!
//! Phase 1E-b: walk the [`TerminalState`] grid and paint every cell
//! into an [`egui::Painter`]. No reliance on egui's text widgets —
//! cells get an explicit background rectangle (when non-default)
//! plus an explicit glyph draw, so future styled-run batching and
//! per-cell decorations have a clean place to grow.
//!
//! Phase 1E-f also covers the basic cell-attribute decorations:
//! bold (brighten), dim (darken), inverse (swap fg/bg), hidden
//! (skip glyph), underline / strikethrough (extra line). Italic
//! needs an italic monospace font and is deferred to Phase 10;
//! every underline variant currently renders as the same single
//! line, with the spec calling out doubled / curly / dotted /
//! dashed as future polish.
//!
//! Still out of scope for this PR (deliberately): cursor shape /
//! blink, selection / search highlights, styled-run batching as
//! an optimization, mouse selection. Those land across later
//! 1E-* and Phase 2 sub-PRs.
//!
//! See [`spec/02-terminal-engine.md`](../spec/02-terminal-engine.md#rendering)
//! for the rendering contract this implements.

#![forbid(unsafe_code)]

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Response, Stroke, Vec2};

use crate::links::LinkSpan;
use crate::prompt_editor::PromptEditor;
use crate::selection::{GridGeometry, Selection};
use crate::terminal::{StyledLine, TerminalState};

/// Font size in egui points. Tuned by eye for legibility on the
/// terminals we tested against; configurable later.
pub const DEFAULT_FONT_SIZE: f32 = 14.0;

/// Pane background when no cell-level background applies. Slightly
/// off-black so a cursor block reads as a tile, not a hole.
pub const DEFAULT_BG: Color32 = Color32::from_rgb(0x12, 0x12, 0x12);

/// Default foreground. Off-white; matches alacritty's "light"
/// foreground convention for dark themes.
pub const DEFAULT_FG: Color32 = Color32::from_rgb(0xd8, 0xd8, 0xd8);

/// Cursor overlay color. Semi-transparent white so the character
/// underneath stays legible (block cursor over text reads as
/// inverted-ish, not as a hole). Phase 10 will make this themable.
pub const CURSOR_COLOR: Color32 = Color32::from_rgba_premultiplied(0x70, 0x70, 0x70, 0x70);

/// Selection overlay color. Semi-transparent blue, painted *over* the
/// cell backgrounds and *under* the glyphs, so selected text remains
/// readable. Tuned to match what macOS Terminal / iTerm2 settle on by
/// default — saturated enough to be unmistakable, light enough not to
/// swallow dark text.
pub const SELECTION_COLOR: Color32 = Color32::from_rgba_premultiplied(0x3a, 0x60, 0xa0, 0x80);

/// Color of the underline drawn beneath a Cmd/Ctrl-hovered URL.
/// Same colour family as the selection overlay so the two feel like
/// siblings in the UI, but fully opaque so a thin underline is
/// readable. Phase 10 will make this themable.
pub const LINK_UNDERLINE_COLOR: Color32 = Color32::from_rgb(0x7c, 0xa9, 0xff);

/// Foreground for user-entered text inside the [`PromptEditor`].
/// Visually distinct from `DEFAULT_FG` (which paints shell output)
/// so a screenshot makes immediately clear which characters the user
/// typed vs. which the shell wrote back. A bright teal — the same
/// hue family as `ALT_SCREEN_BORDER_COLOR` so the "Termica owns
/// this region" cue is consistent. 4G's prompt chrome will likely
/// retune all of these together; the precise value is intentionally
/// not sacred.
pub const EDITOR_FG: Color32 = Color32::from_rgb(0x6e, 0xd0, 0xe8);

/// Cursor block colour inside the editor. Saturated teal at higher
/// alpha than the live-grid `CURSOR_COLOR` so the user's input
/// caret reads as the *active* cursor whenever the editor is on
/// screen. (The live grid's cursor is suppressed by
/// `paint_terminal`'s `hide_cursor` flag in editor mode, so there
/// can never be two cursors painted at once.)
pub const EDITOR_CURSOR_COLOR: Color32 = Color32::from_rgba_premultiplied(0x4a, 0xa8, 0xc0, 0xa0);

/// Result of one paint pass over the terminal grid.
///
/// The caller (the eframe app) uses this to do hit-testing for the
/// mouse: which cell did the pointer land on, was it inside the grid,
/// did a drag begin? The `geometry` field is what pure helpers in
/// [`crate::selection`] consume.
#[derive(Debug)]
pub struct TerminalRender {
    /// egui response over the painted rect (sensed `click_and_drag`).
    pub response: Response,
    /// Geometry of the painted grid, suitable for passing to
    /// [`crate::selection::pixel_to_grid_point`].
    pub geometry: GridGeometry,
}

/// Paint `term`'s visible grid into the current cursor position.
///
/// Allocates exactly `cols × rows` of monospace metrics from `ui` so
/// the surrounding layout knows we drew there. Caller decides where
/// the grid lives (typically directly inside a `CentralPanel` after
/// the status line).
///
/// `selection`, if `Some`, is painted as a semi-transparent overlay
/// over the cells covered by the selection range. The hit-testing /
/// drag tracking lives in the caller — this function only renders.
///
/// Returns a [`TerminalGeometry`] describing what was painted plus
/// the egui `Response` (sensed `click_and_drag`) so the caller can
/// turn pointer events into grid coordinates.
pub fn paint_terminal(
    ui: &mut egui::Ui,
    term: &TerminalState,
    selection: Option<&Selection>,
    hover_link: Option<&LinkSpan>,
    hide_cursor: bool,
) -> TerminalRender {
    let font_id = FontId::monospace(DEFAULT_FONT_SIZE);
    // `glyph_width` / `row_height` mutate the font cache as they go,
    // so the `_mut` access is the correct one — `fonts()` (read-only)
    // refuses to call them.
    let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let row_h = ui.fonts_mut(|f| f.row_height(&font_id));

    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    // Translate viewport rows (`0..rows`) to grid `Line` indices.
    // When the user scrolls into the scrollback, the visible top
    // moves UP into negative `Line` territory.
    //
    // See `alacritty_terminal::term::viewport_to_point`:
    //     grid_line = viewport_line - display_offset
    let display_offset = grid.display_offset() as i32;

    let size = Vec2::new(cols as f32 * cell_w, rows as f32 * row_h);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    // One full-pane background draw is cheaper than `rows * cols`
    // default-color fills, so we only paint per-cell backgrounds for
    // cells that actually differ from the pane default.
    painter.rect_filled(rect, 0.0, DEFAULT_BG);

    for row in 0..rows {
        for col in 0..cols {
            let grid_line = (row as i32) - display_offset;
            let pt = Point::new(Line(grid_line), Column(col));
            let cell = &grid[pt];

            let x = rect.min.x + col as f32 * cell_w;
            let y = rect.min.y + row as f32 * row_h;
            let cell_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, row_h));

            let (fg, bg, paint_glyph) = cell_colors(cell);

            if let Some(c) = bg {
                painter.rect_filled(cell_rect, 0.0, c);
            }

            // Spaces don't paint a glyph (default cells are already
            // covered by the pane background). Non-space glyphs do —
            // unless the HIDDEN flag is set, in which case we paint
            // the bg (above) but not the glyph itself.
            if paint_glyph && cell.c != ' ' {
                painter.text(
                    Pos2::new(x, y),
                    egui::Align2::LEFT_TOP,
                    cell.c.to_string(),
                    font_id.clone(),
                    fg,
                );
            }

            // Underline decorations. alacritty distinguishes plain /
            // double / curly / dotted / dashed underlines; for v1 we
            // collapse all variants to one line under the cell. Phase
            // 10 polish can break them apart with proper rendering.
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                let underline_y = y + row_h - 1.5;
                painter.line_segment(
                    [Pos2::new(x, underline_y), Pos2::new(x + cell_w, underline_y)],
                    Stroke::new(1.0, fg),
                );
            }

            // Strikeout: a single line through the vertical middle.
            if cell.flags.contains(Flags::STRIKEOUT) {
                let strike_y = y + row_h * 0.5;
                painter.line_segment(
                    [Pos2::new(x, strike_y), Pos2::new(x + cell_w, strike_y)],
                    Stroke::new(1.0, fg),
                );
            }
        }
    }

    let geometry = GridGeometry {
        origin_x: rect.min.x,
        origin_y: rect.min.y,
        cell_w,
        row_h,
        display_offset,
        screen_lines: rows,
        cols,
    };

    // Selection overlay. Painted on top of the cells (so the
    // background color of selected cells is tinted blue) and below
    // the cursor (so the cursor block remains visible inside a
    // selection). We only paint when there's a real range — a
    // degenerate `anchor == head` selection is a click that hasn't
    // become a drag (Char mode), and we don't tint a single cell for
    // that. Word/Line selections are NEVER considered empty: a
    // double-click on a word produces a real highlight even with no
    // drag motion.
    if let Some(sel) = selection
        && !sel.is_empty()
    {
        let (start, end) = sel.effective_range(grid);
        paint_selection_overlay(&painter, start, end, geometry);
    }

    // Link hover underline. The caller has already gated this on
    // "Cmd/Ctrl held" — if `hover_link` is `Some` we draw the
    // underline unconditionally, so a future "always show URL
    // affordance" toggle would just pass the link in without the
    // modifier gate.
    if let Some(link) = hover_link {
        paint_link_underline(&painter, link, geometry);
    }

    // Cursor overlay. Block shape for now (vim / less / htop all
    // expect a visible cursor while in their alt-screen UIs). Phase 4
    // will move ownership of cursor visibility to the prompt-editor
    // when we're at a trusted shell prompt; until then the renderer
    // simply mirrors what the terminal mode flags say.
    if !hide_cursor
        && term.is_cursor_visible()
        && let Some((row, col)) = term.cursor_position()
    {
        let x = rect.min.x + col as f32 * cell_w;
        let y = rect.min.y + row as f32 * row_h;
        let cursor_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, row_h));
        painter.rect_filled(cursor_rect, 0.0, CURSOR_COLOR);
    }

    TerminalRender { response, geometry }
}

/// Paint a frozen sealed-block snapshot into `ui` at its current cursor.
/// Allocates the exact space the snapshot needs (rows × widest-line
/// columns at monospace metrics) and paints cell-by-cell, mirroring
/// the [`paint_terminal`] loop minus the live-only concerns (selection,
/// cursor, link hover, alt-screen border). Phase 4F adds cross-block
/// selection; until then sealed blocks are static.
///
/// Returns the `Response` over the painted rect so the caller can
/// chain interactions (Phase 4G adds click-to-collapse) once they
/// exist; for 4A-render the response is unused.
pub fn paint_styled_lines(ui: &mut egui::Ui, lines: &[StyledLine]) -> Response {
    let font_id = FontId::monospace(DEFAULT_FONT_SIZE);
    let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let row_h = ui.fonts_mut(|f| f.row_height(&font_id));
    let cols = lines.iter().map(|l| l.cells.len()).max().unwrap_or(0);
    let rows = lines.len();
    let size = Vec2::new(cols as f32 * cell_w, rows as f32 * row_h);

    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    if size.x > 0.0 && size.y > 0.0 {
        painter.rect_filled(rect, 0.0, DEFAULT_BG);
    }

    for (row_idx, line) in lines.iter().enumerate() {
        for (col, cell) in line.cells.iter().enumerate() {
            let x = rect.min.x + col as f32 * cell_w;
            let y = rect.min.y + row_idx as f32 * row_h;
            let cell_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, row_h));

            let (fg, bg, paint_glyph) = cell_colors_for(cell.fg, cell.bg, cell.flags);
            if let Some(c) = bg {
                painter.rect_filled(cell_rect, 0.0, c);
            }
            if paint_glyph && cell.c != ' ' {
                painter.text(
                    Pos2::new(x, y),
                    egui::Align2::LEFT_TOP,
                    cell.c.to_string(),
                    font_id.clone(),
                    fg,
                );
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                let underline_y = y + row_h - 1.5;
                painter.line_segment(
                    [Pos2::new(x, underline_y), Pos2::new(x + cell_w, underline_y)],
                    Stroke::new(1.0, fg),
                );
            }
            if cell.flags.contains(Flags::STRIKEOUT) {
                let strike_y = y + row_h * 0.5;
                painter.line_segment(
                    [Pos2::new(x, strike_y), Pos2::new(x + cell_w, strike_y)],
                    Stroke::new(1.0, fg),
                );
            }
        }
    }
    response
}

/// Paint the [`PromptEditor`] inside its [`Block::Prompt`](crate::block::Block::Prompt).
///
/// Allocates space in `ui`'s current flow and paints there. The
/// snapshot tests drive this overload because kittest wants a rect
/// to claim for hit-testing. The live `render_pane` path uses
/// [`paint_prompt_editor_at`] instead, with an absolute origin
/// derived from the live `Term`'s cursor row — that's how the
/// editor visually continues the shell's prompt line in Phase 4C.
///
/// 4G adds the prompt chrome (the `❯` glyph, the cwd / branch /
/// dirty chips); 4F adds selection rendering; 4H adds syntax
/// highlighting. For now the editor paints unstyled `DEFAULT_FG`
/// text on `DEFAULT_BG`.
pub fn paint_prompt_editor(ui: &mut egui::Ui, editor: &PromptEditor) -> Response {
    let font_id = FontId::monospace(DEFAULT_FONT_SIZE);
    let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let row_h = ui.fonts_mut(|f| f.row_height(&font_id));
    let lines = editor.lines_with_cursor();
    let rows = lines.len().max(1);
    let widest_chars = lines.iter().map(|l| l.text.chars().count()).max().unwrap_or(0);
    let cols = widest_chars + 1;
    let size = Vec2::new((cols as f32 * cell_w).max(cell_w), rows as f32 * row_h);

    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, DEFAULT_BG);
    paint_prompt_editor_at(&painter, editor, rect.min, cell_w, row_h, &font_id);
    response
}

/// Paint the [`PromptEditor`] at an explicit origin using
/// pre-computed monospace metrics. No `ui` allocation — the caller
/// owns the layout. Used by the live `render_pane` path to overlay
/// the editor on top of the live `Term`'s painted area starting at
/// `(term_rect.min.x, term_rect.min.y + (cursor_row + 1) * row_h)`
/// so the editor's first line sits immediately below the row holding
/// the shell's prompt + cursor.
pub fn paint_prompt_editor_at(
    painter: &egui::Painter,
    editor: &PromptEditor,
    origin: Pos2,
    cell_w: f32,
    row_h: f32,
    font_id: &FontId,
) {
    for (row_idx, line) in editor.lines_with_cursor().iter().enumerate() {
        let y = origin.y + row_idx as f32 * row_h;
        if !line.text.is_empty() {
            painter.text(
                Pos2::new(origin.x, y),
                egui::Align2::LEFT_TOP,
                line.text,
                font_id.clone(),
                EDITOR_FG,
            );
        }
        if line.cursor_on_line {
            let cursor_x = origin.x + line.cursor_col_chars as f32 * cell_w;
            let cursor_rect = Rect::from_min_size(Pos2::new(cursor_x, y), Vec2::new(cell_w, row_h));
            painter.rect_filled(cursor_rect, 0.0, EDITOR_CURSOR_COLOR);
        }
    }
}

/// Paint the semi-transparent selection rectangles.
///
/// Takes the `(start, end)` *effective* range already in reading
/// order — the caller resolves Char vs Word vs Line mode before
/// calling this. Multi-line selections are drawn as three pieces:
/// the partial first row, any full middle rows, and the partial last
/// row. Single-row selections collapse to one rectangle. Selections
/// that fall partly outside the current viewport (because the user
/// has scrolled) are clipped to the visible rows.
fn paint_selection_overlay(painter: &egui::Painter, start: Point, end: Point, g: GridGeometry) {
    // Translate absolute Lines to viewport row indices, clamping each
    // endpoint to the visible range. If both endpoints land outside
    // the viewport on the same side, the clamp collapses them and we
    // paint nothing (the for-loop range becomes empty).
    let start_viewport = (start.line.0 + g.display_offset).max(0);
    let end_viewport = (end.line.0 + g.display_offset).min(g.screen_lines as i32 - 1);
    if start_viewport > end_viewport {
        return;
    }

    for vrow in start_viewport..=end_viewport {
        let (col_lo, col_hi) = if start.line.0 == end.line.0 {
            // Single-line selection.
            (start.column.0, end.column.0)
        } else if vrow == start.line.0 + g.display_offset {
            // First row of a multi-line selection.
            (start.column.0, g.cols.saturating_sub(1))
        } else if vrow == end.line.0 + g.display_offset {
            // Last row of a multi-line selection.
            (0, end.column.0)
        } else {
            // Middle row.
            (0, g.cols.saturating_sub(1))
        };

        let col_hi = col_hi.min(g.cols.saturating_sub(1));
        let col_lo = col_lo.min(col_hi);

        let x0 = g.origin_x + col_lo as f32 * g.cell_w;
        // `+ 1` so the highlight visually includes the cell under the
        // pointer at the right edge.
        let x1 = g.origin_x + (col_hi as f32 + 1.0) * g.cell_w;
        let y0 = g.origin_y + vrow as f32 * g.row_h;
        let y1 = y0 + g.row_h;

        painter.rect_filled(
            Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1)),
            0.0,
            SELECTION_COLOR,
        );
    }
}

/// Underline the cells covered by `link` in the link-hover colour.
///
/// Drawn 1.5 px above the cell bottom (matches the regular underline
/// decoration in [`paint_terminal`]), at 1.5 px thickness so it
/// reads as "affordance" rather than as a textual underline that
/// the program itself emitted. Single-row only — multi-row URL
/// stitching is deferred (see [`crate::links`]).
fn paint_link_underline(painter: &egui::Painter, link: &LinkSpan, g: GridGeometry) {
    let viewport_row = link.start.line.0 + g.display_offset;
    if viewport_row < 0 || viewport_row >= g.screen_lines as i32 {
        return;
    }

    let col_lo = link.start.column.0.min(g.cols.saturating_sub(1));
    let col_hi = link.end.column.0.min(g.cols.saturating_sub(1));

    let x0 = g.origin_x + col_lo as f32 * g.cell_w;
    let x1 = g.origin_x + (col_hi as f32 + 1.0) * g.cell_w;
    let y = g.origin_y + (viewport_row as f32 + 1.0) * g.row_h - 1.5;

    painter
        .line_segment([Pos2::new(x0, y), Pos2::new(x1, y)], Stroke::new(1.5, LINK_UNDERLINE_COLOR));
}

/// Resolve a cell to its `(fg, bg, paint_glyph)` triple for this
/// frame, applying the per-cell attribute flags:
///
/// - `INVERSE`: swap fg / bg (with logical-default substitution).
/// - `BOLD`: brighten the fg toward white. Cheap stand-in for a
///   real bold font; Phase 10 can swap in a proper bold glyph
///   render. Bold is intentionally **not** applied to bg.
/// - `DIM`: darken the fg toward black. Same caveat.
/// - `HIDDEN`: keep colors but signal the caller to skip the glyph.
///
/// `bg` is `None` when the cell wants the pane default — that lets
/// `paint_terminal` skip per-cell background fills for default
/// cells (the pane-wide fill already covered them).
fn cell_colors(cell: &alacritty_terminal::term::cell::Cell) -> (Color32, Option<Color32>, bool) {
    cell_colors_for(cell.fg, cell.bg, cell.flags)
}

/// Compute the (fg, bg, paint_glyph) triple for a cell from its raw
/// styling fields. Used by both the live-grid path
/// ([`cell_colors`]) and the sealed-block snapshot path
/// ([`paint_styled_lines`]) so both layers paint identically.
fn cell_colors_for(fg: Color, bg: Color, flags: Flags) -> (Color32, Option<Color32>, bool) {
    let mut fg = ansi_to_egui(fg).unwrap_or(DEFAULT_FG);
    // The default-bg case keeps `bg_opt = None` so the per-cell fill
    // is skipped; only solid-color backgrounds paint a rectangle.
    let mut bg_opt = ansi_to_egui(bg);

    if flags.contains(Flags::INVERSE) {
        // After inversion, "default bg" becomes the actual fg color
        // and vice versa — neither side can be `None` anymore, so
        // when one was logical-default we substitute its visible
        // counterpart.
        let prev_fg = fg;
        let prev_bg = bg_opt.unwrap_or(DEFAULT_BG);
        fg = prev_bg;
        bg_opt = Some(prev_fg);
    }

    if flags.contains(Flags::DIM) {
        fg = scale_brightness(fg, 0.5);
    }
    if flags.contains(Flags::BOLD) {
        fg = brighten_toward_white(fg, 0.3);
    }

    let paint_glyph = !flags.contains(Flags::HIDDEN);
    (fg, bg_opt, paint_glyph)
}

/// Move each RGB channel `f` of the distance toward `255`.
fn brighten_toward_white(c: Color32, f: f32) -> Color32 {
    let f = f.clamp(0.0, 1.0);
    let r = c.r() as f32 + (255.0 - c.r() as f32) * f;
    let g = c.g() as f32 + (255.0 - c.g() as f32) * f;
    let b = c.b() as f32 + (255.0 - c.b() as f32) * f;
    Color32::from_rgb(r as u8, g as u8, b as u8)
}

/// Multiply each RGB channel by `f` (0 = black, 1 = unchanged).
fn scale_brightness(c: Color32, f: f32) -> Color32 {
    let f = f.clamp(0.0, 1.0);
    let r = (c.r() as f32 * f) as u8;
    let g = (c.g() as f32 * f) as u8;
    let b = (c.b() as f32 * f) as u8;
    Color32::from_rgb(r, g, b)
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

    // --- cell-attribute helpers ----------------------------------------

    #[test]
    fn brighten_toward_white_moves_each_channel_toward_max() {
        let base = Color32::from_rgb(100, 100, 100);
        let brightened = brighten_toward_white(base, 0.5);
        // r = 100 + (255-100)*0.5 = 177.5 -> truncated to 177
        assert!(
            brightened.r() > base.r() && brightened.g() > base.g() && brightened.b() > base.b(),
            "expected all channels to brighten; got {brightened:?}"
        );
        // f=0 must be a no-op.
        assert_eq!(brighten_toward_white(base, 0.0), base);
        // f=1 must produce pure white.
        let white = brighten_toward_white(base, 1.0);
        assert_eq!((white.r(), white.g(), white.b()), (255, 255, 255));
    }

    #[test]
    fn scale_brightness_dims_toward_black() {
        let base = Color32::from_rgb(200, 100, 50);
        let dim = scale_brightness(base, 0.5);
        assert!(
            dim.r() < base.r() && dim.g() < base.g() && dim.b() < base.b(),
            "expected all channels to dim; got {dim:?}"
        );
        // f=0 must be black.
        let black = scale_brightness(base, 0.0);
        assert_eq!((black.r(), black.g(), black.b()), (0, 0, 0));
        // f=1 must be unchanged.
        assert_eq!(scale_brightness(base, 1.0), base);
    }

    fn cell_with_flags(flags: Flags) -> alacritty_terminal::term::cell::Cell {
        // Build a cell with named-default fg/bg and the requested
        // attribute flags. The struct-update syntax keeps clippy's
        // `field_reassign_with_default` happy.
        alacritty_terminal::term::cell::Cell {
            c: 'X',
            flags,
            ..alacritty_terminal::term::cell::Cell::default()
        }
    }

    #[test]
    fn cell_colors_default_returns_default_fg_and_none_bg() {
        let cell = cell_with_flags(Flags::empty());
        let (fg, bg, paint) = cell_colors(&cell);
        assert_eq!(fg, DEFAULT_FG);
        assert!(bg.is_none(), "default-bg cell should not paint a per-cell rect");
        assert!(paint);
    }

    #[test]
    fn cell_colors_inverse_swaps_fg_and_bg() {
        let cell = cell_with_flags(Flags::INVERSE);
        let (fg, bg, _paint) = cell_colors(&cell);
        // Default fg + default bg, swapped: fg becomes DEFAULT_BG,
        // bg becomes DEFAULT_FG (and is now a real fill).
        assert_eq!(fg, DEFAULT_BG);
        assert_eq!(bg, Some(DEFAULT_FG));
    }

    #[test]
    fn cell_colors_bold_brightens_fg() {
        let plain = cell_colors(&cell_with_flags(Flags::empty())).0;
        let bold = cell_colors(&cell_with_flags(Flags::BOLD)).0;
        // Bold > plain on every channel (brightness moved toward white).
        assert!(bold.r() >= plain.r() && bold.g() >= plain.g() && bold.b() >= plain.b());
        assert!(
            bold.r() > plain.r() || bold.g() > plain.g() || bold.b() > plain.b(),
            "bold should brighten at least one channel"
        );
    }

    #[test]
    fn cell_colors_dim_darkens_fg() {
        let plain = cell_colors(&cell_with_flags(Flags::empty())).0;
        let dim = cell_colors(&cell_with_flags(Flags::DIM)).0;
        assert!(dim.r() <= plain.r() && dim.g() <= plain.g() && dim.b() <= plain.b());
        assert!(dim.r() < plain.r() || dim.g() < plain.g() || dim.b() < plain.b());
    }

    #[test]
    fn cell_colors_hidden_suppresses_glyph() {
        let (_fg, _bg, paint) = cell_colors(&cell_with_flags(Flags::HIDDEN));
        assert!(!paint, "HIDDEN flag should suppress glyph painting");
    }

    #[test]
    fn cell_colors_bold_dim_combined_picks_a_middle() {
        // BOLD + DIM together are a real (if unusual) terminal state.
        // The order in `cell_colors` applies DIM first, then BOLD,
        // so the result lands between the two extremes. The contract
        // is just "doesn't panic, returns something sensible" — we
        // assert the result differs from plain.
        let plain = cell_colors(&cell_with_flags(Flags::empty())).0;
        let combined = cell_colors(&cell_with_flags(Flags::BOLD | Flags::DIM)).0;
        assert_ne!(plain, combined);
    }
}
