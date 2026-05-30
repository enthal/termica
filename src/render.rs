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

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Response, Stroke, Vec2};

use crate::block_selection::BlockCursor;
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

/// Cursor overlay color when the pane has keyboard focus. Semi-
/// transparent white so the character underneath stays legible
/// (block cursor over text reads as inverted-ish, not as a hole).
/// Phase 10 will make this themable.
pub const CURSOR_COLOR: Color32 = Color32::from_rgba_premultiplied(0x70, 0x70, 0x70, 0x70);

/// Cursor overlay color when the pane is **not** focused. Lower
/// alpha + greyer hue so the cursor reads as "this pane is
/// dormant" without disappearing entirely — matches the
/// "focused tab + ghost cursor in other tabs" idiom of
/// conventional terminal emulators.
pub const CURSOR_UNFOCUSED_COLOR: Color32 =
    Color32::from_rgba_premultiplied(0x55, 0x55, 0x55, 0x40);

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

/// Translucent blue overlay painted on the cells of a Cmd-hovered
/// URL or path so the hover affordance reads at a glance, not just
/// as a thin underline. Premultiplied so the on-screen colour is
/// the literal RGB — on the dark terminal background that produces
/// a clear blue tint without obscuring the glyphs. Same hue family
/// as [`LINK_UNDERLINE_COLOR`] so the two reinforce each other.
pub const LINK_HOVER_OVERLAY_COLOR: Color32 =
    Color32::from_rgba_premultiplied(0x3a, 0x55, 0x8a, 0x60);

/// Foreground for user-entered text inside the [`PromptEditor`].
/// Visually distinct from `DEFAULT_FG` (which paints shell output)
/// so a screenshot makes immediately clear which characters the user
/// typed vs. which the shell wrote back. A bright teal — the same
/// hue family as `ALT_SCREEN_BORDER_COLOR` so the "Termica owns
/// this region" cue is consistent. 4G's prompt chrome will likely
/// retune all of these together; the precise value is intentionally
/// not sacred.
pub const EDITOR_FG: Color32 = Color32::from_rgb(0x6e, 0xd0, 0xe8);

/// Caret colour inside the editor. The editor draws a thin
/// vertical-bar caret (not a block) that blinks at ~1.6 Hz; this is
/// the colour for the "visible" half of the blink cycle. Saturated
/// teal at higher alpha than the live-grid `CURSOR_COLOR` so the
/// user's input caret reads as the *active* cursor whenever the
/// editor is on screen. The live grid's cursor is suppressed by
/// `paint_terminal`'s `hide_cursor` flag in editor mode, so there
/// can never be two cursors painted at once.
pub const EDITOR_CURSOR_COLOR: Color32 = Color32::from_rgba_premultiplied(0x4a, 0xa8, 0xc0, 0xa0);

/// Stroke color for the focused-editor chrome — the rounded outline
/// that wraps the chip bar + editor body when the pane is focused
/// AND the window is the OS foreground app (same predicate as the
/// caret). Picked via `cargo run --example pick_focused_editor_chrome`
/// (variant `dim-white-round-rect`): a dim grey-white that reads as
/// "this is wired for input" without competing with the bright caret.
pub const FOCUSED_EDITOR_CHROME_COLOR: Color32 =
    Color32::from_rgba_premultiplied(0xa0, 0xa0, 0xa0, 0xb0);

/// Single source of truth for "should we draw a caret in this pane
/// right now?" per [spec/04](../spec/04-prompt-editor.md#when-is-the-caret-shown).
/// Returns `true` iff ALL three conditions hold:
///
///   - `mode_is_editor`: the pane is in `ShellPromptEditor` (only state
///     with a real editor caret).
///   - `pane_has_focus`: this pane currently holds in-app keyboard focus.
///   - `viewport_focused`: the Termica window is the OS foreground app
///     (i.e. the OS will route the next keystroke to us).
///
/// Used by the prompt-editor caret AND by the raw-terminal cell
/// cursor's "blinking solid" vs "dim hollow" choice — same principle,
/// different surface ([spec/02](../spec/02-terminal-engine.md)).
pub fn should_show_caret(
    mode_is_editor: bool,
    pane_has_focus: bool,
    viewport_focused: bool,
) -> bool {
    mode_is_editor && pane_has_focus && viewport_focused
}

/// Foreground for the dim header line above each block. Roughly
/// "muted grey" against `DEFAULT_BG` — readable but unmistakably
/// secondary to the command + output text below. 4G renders cwd
/// in this colour; Phase 5 will use the same hue family for the
/// status-header chips, so the visual identity stays consistent
/// when both areas exist on screen at once.
pub const BLOCK_HEADER_FG: Color32 = Color32::from_rgb(0x80, 0x80, 0x80);

/// Per-[`TokenKind`](crate::shell_syntax::TokenKind) colours for
/// the editor's syntax highlighting (Phase 4H). Picked so the
/// command word (amber) stands out as "the action," strings (green)
/// and variables (lighter teal than `EDITOR_FG`) read as conventional
/// syntax highlighting, separators and redirects (magenta) read as
/// "structure," and comments (dim grey) clearly de-emphasise. Themable
/// in Phase 10.
pub const TOKEN_COMMAND_FG: Color32 = Color32::from_rgb(0xe8, 0xc6, 0x6e);
pub const TOKEN_STRING_FG: Color32 = Color32::from_rgb(0xa3, 0xd5, 0x9a);
pub const TOKEN_VARIABLE_FG: Color32 = Color32::from_rgb(0xb8, 0xe8, 0xf5);
pub const TOKEN_PIPE_FG: Color32 = Color32::from_rgb(0xd6, 0x8f, 0xd8);
pub const TOKEN_REDIRECT_FG: Color32 = Color32::from_rgb(0xd6, 0x8f, 0xd8);
pub const TOKEN_FLAG_FG: Color32 = Color32::from_rgb(0xe8, 0xb5, 0x6e);
pub const TOKEN_COMMENT_FG: Color32 = Color32::from_rgb(0x70, 0x70, 0x70);
/// Bright yellow for the `=` inside a shell `KEY=value` var def.
/// Sits between the `Variable`-coloured name on its left and the
/// `String`-coloured value on its right, so the assignment reads
/// as a three-toned phrase at a glance.
pub const TOKEN_EQUALS_FG: Color32 = Color32::from_rgb(0xf5, 0xd0, 0x3a);

/// Map a [`TokenKind`](crate::shell_syntax::TokenKind) to the
/// foreground colour the editor should paint with. `Word` falls
/// back to [`EDITOR_FG`] — the default user-input teal.
pub fn color_for_token_kind(kind: crate::shell_syntax::TokenKind) -> Color32 {
    use crate::shell_syntax::TokenKind;
    match kind {
        TokenKind::Command => TOKEN_COMMAND_FG,
        TokenKind::String => TOKEN_STRING_FG,
        TokenKind::Variable => TOKEN_VARIABLE_FG,
        TokenKind::Pipe => TOKEN_PIPE_FG,
        TokenKind::Redirect => TOKEN_REDIRECT_FG,
        TokenKind::Flag => TOKEN_FLAG_FG,
        TokenKind::Comment => TOKEN_COMMENT_FG,
        TokenKind::Equals => TOKEN_EQUALS_FG,
        TokenKind::Word => EDITOR_FG,
    }
}

/// Foreground for a non-zero exit code rendered on a sealed block's
/// header line. Saturated red so failed commands are unmistakable
/// in a long transcript. Theme polish lands in Phase 10.
pub const BLOCK_HEADER_EXIT_FAIL_FG: Color32 = Color32::from_rgb(0xe0, 0x70, 0x70);

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

/// Paint `term`'s grid into the current cursor position.
///
/// `include_history`:
/// - `false` — paint exactly the visible viewport (rows
///   `display_offset..display_offset + screen_lines`). The legacy
///   path used by alt-screen mode (which has no scrollback anyway)
///   and by snapshot tests.
/// - `true` — paint **all** rows the grid currently holds: the
///   `history_size` scrollback rows followed by the `screen_lines`
///   viewport rows. `display_offset` is ignored; the rendered
///   height grows to `(history_size + screen_lines) × row_h`. This
///   is what running commands need: every line emitted since
///   `Preexec` stays on-screen and the outer ScrollArea handles
///   navigation. The returned `GridGeometry.display_offset` is set
///   to `history_size` so pixel→grid hit-testing places (row 0,
///   col 0) at grid line `-history_size`.
///
/// `selection`, if `Some`, is painted as a semi-transparent overlay
/// over the cells covered by the selection range. The hit-testing /
/// drag tracking lives in the caller — this function only renders.
pub fn paint_terminal(
    ui: &mut egui::Ui,
    term: &TerminalState,
    selection: Option<&Selection>,
    hover_link: Option<&LinkSpan>,
    hide_cursor: bool,
    focused: bool,
    include_history: bool,
) -> TerminalRender {
    let font_id = FontId::monospace(DEFAULT_FONT_SIZE);
    // `glyph_width` / `row_height` mutate the font cache as they go,
    // so the `_mut` access is the correct one — `fonts()` (read-only)
    // refuses to call them.
    let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let row_h = ui.fonts_mut(|f| f.row_height(&font_id));

    let grid = term.grid();
    let cols = grid.columns();
    let screen_lines = grid.screen_lines();
    let history_size = if include_history { grid.history_size() } else { 0 };
    let rows = history_size + screen_lines;
    // Translate viewport rows (`0..rows`) to grid `Line` indices.
    // In the legacy (no-history) path, viewport row `r` maps to
    // grid line `r - display_offset`. When `include_history` is on
    // we ignore `display_offset` and pin the top of the painted
    // region at grid line `-history_size` so the oldest scrollback
    // row sits at viewport row 0; `effective_display_offset` is the
    // value we'd substitute for alacritty's `display_offset` to make
    // the same formula keep working.
    let effective_display_offset =
        if include_history { history_size as i32 } else { grid.display_offset() as i32 };

    let size = Vec2::new(cols as f32 * cell_w, rows as f32 * row_h);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    // One full-pane background draw is cheaper than `rows * cols`
    // default-color fills, so we only paint per-cell backgrounds for
    // cells that actually differ from the pane default.
    painter.rect_filled(rect, 0.0, DEFAULT_BG);

    for row in 0..rows {
        for col in 0..cols {
            let grid_line = (row as i32) - effective_display_offset;
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
        display_offset: effective_display_offset,
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
        paint_selection_overlay(&painter, grid, start, end, geometry);
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
    //
    // `cursor_position()` returns the *viewport* row. With
    // `include_history` the painted area starts `history_size` rows
    // earlier, so shift the cursor's painted row by `history_size`
    // to land it in the right place.
    if !hide_cursor
        && term.is_cursor_visible()
        && let Some((row, col)) = term.cursor_position()
    {
        let painted_row = row + history_size;
        let x = rect.min.x + col as f32 * cell_w;
        let y = rect.min.y + painted_row as f32 * row_h;
        let cursor_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, row_h));
        let cursor_color = if focused { CURSOR_COLOR } else { CURSOR_UNFOCUSED_COLOR };
        painter.rect_filled(cursor_rect, 0.0, cursor_color);
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
pub fn paint_styled_lines(
    ui: &mut egui::Ui,
    lines: &[StyledLine],
    selection: Option<(BlockCursor, BlockCursor)>,
) -> Response {
    let font_id = FontId::monospace(DEFAULT_FONT_SIZE);
    let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let row_h = ui.fonts_mut(|f| f.row_height(&font_id));
    let cols = lines.iter().map(|l| l.cells.len()).max().unwrap_or(0);
    let rows = lines.len();
    let size = Vec2::new(cols as f32 * cell_w, rows as f32 * row_h);

    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
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
    if let Some((start, end)) = selection {
        paint_block_selection_overlay(&painter, lines, rect, cell_w, row_h, start, end);
    }
    response
}

/// Paint the teal selection overlay across a sealed block's rows.
///
/// `start` and `end` are already in reading order. Splits a
/// multi-row selection into a partial first row, full middle rows,
/// and a partial last row — the same shape as the live-grid
/// [`paint_selection_overlay`] but in (row, col) space with no
/// `display_offset` to translate.
fn paint_block_selection_overlay(
    painter: &egui::Painter,
    lines: &[StyledLine],
    rect: Rect,
    cell_w: f32,
    row_h: f32,
    start: BlockCursor,
    end: BlockCursor,
) {
    if start == end || lines.is_empty() {
        return;
    }
    let last_row = end.row.min(lines.len() - 1);
    if start.row > last_row {
        return;
    }
    for (row, line) in lines.iter().enumerate().take(last_row + 1).skip(start.row) {
        let row_len = line.cells.len();
        let (col_lo, col_hi) = if start.row == end.row {
            (start.col, end.col)
        } else if row == start.row {
            (start.col, row_len.max(start.col))
        } else if row == end.row {
            (0, end.col)
        } else {
            (0, row_len)
        };
        let col_hi = col_hi.min(row_len.max(col_lo));
        if col_hi <= col_lo {
            continue;
        }
        let x0 = rect.min.x + col_lo as f32 * cell_w;
        let x1 = rect.min.x + col_hi as f32 * cell_w;
        let y0 = rect.min.y + row as f32 * row_h;
        let y1 = y0 + row_h;
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1)),
            0.0,
            SELECTION_COLOR,
        );
    }
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
    // Snapshot tests always paint the cursor — they pin the rendered
    // state of the editor including its caret. The blink phase only
    // applies to the live render path.
    paint_prompt_editor_at(&painter, editor, rect.min, cell_w, row_h, &font_id, true);
    response
}

/// Paint the [`PromptEditor`] at an explicit origin using
/// pre-computed monospace metrics. No `ui` allocation — the caller
/// owns the layout. Used by the live `render_pane` path to overlay
/// the editor on top of the live `Term`'s painted area starting at
/// `(term_rect.min.x, term_rect.min.y + (cursor_row + 1) * row_h)`
/// so the editor's first line sits immediately below the row holding
/// the shell's prompt + cursor.
/// Width of the editor's blinking line caret, in egui logical
/// pixels. Two pixels reads as a thin bar without being invisible
/// at low DPI.
const EDITOR_CARET_WIDTH: f32 = 2.0;

pub fn paint_prompt_editor_at(
    painter: &egui::Painter,
    editor: &PromptEditor,
    origin: Pos2,
    cell_w: f32,
    row_h: f32,
    font_id: &FontId,
    caret_visible: bool,
) {
    let selection_bytes = editor.selection_range();
    let lines = editor.lines_with_cursor();
    let full_text = editor.text();
    let tokens = crate::shell_syntax::tokenize(full_text);

    let mut row_byte_starts = Vec::with_capacity(lines.len());
    {
        let mut byte = 0usize;
        for line in &lines {
            row_byte_starts.push(byte);
            byte += line.text.len() + 1; // +1 for the \n
        }
    }

    for (row_idx, line) in lines.iter().enumerate() {
        let y = origin.y + row_idx as f32 * row_h;
        let row_byte_start = row_byte_starts[row_idx];
        let row_byte_end = row_byte_start + line.text.len();

        // Selection overlay UNDER the glyphs so text stays legible.
        if let Some((sel_start, sel_end)) = selection_bytes {
            let clip_start = sel_start.max(row_byte_start);
            let clip_end = sel_end.min(row_byte_end);
            let extends_past = sel_end > row_byte_end;
            if clip_start < clip_end || extends_past {
                let sel_start_chars = line.text[..clip_start - row_byte_start].chars().count();
                let sel_end_chars = if extends_past {
                    line.text.chars().count() + 1 // +1 cell for the \n
                } else {
                    line.text[..clip_end - row_byte_start].chars().count()
                };
                let x_start = origin.x + sel_start_chars as f32 * cell_w;
                let x_end = origin.x + sel_end_chars as f32 * cell_w;
                let rect = Rect::from_min_max(
                    Pos2::new(x_start, y),
                    Pos2::new(x_end.max(x_start + cell_w * 0.25), y + row_h),
                );
                painter.rect_filled(rect, 0.0, SELECTION_COLOR);
            }
        }

        // Paint tokens that intersect this row. Tokens are emitted
        // in source order with no overlap, so we can iterate them
        // once and skip out-of-row entries. Whitespace gaps between
        // tokens don't need painting (they're spaces / tabs with no
        // glyphs to draw).
        if !line.text.is_empty() {
            // Fallback: if no token covers a character of this row,
            // paint the whole row in EDITOR_FG so we never lose
            // glyphs (e.g. a tokenizer regression on exotic input).
            // We track whether any token painted; if not, paint the
            // whole row. Token painting suffices in practice because
            // every non-whitespace byte gets emitted as some kind.
            let mut painted_any = false;
            for token in &tokens {
                if token.range.end <= row_byte_start || token.range.start >= row_byte_end {
                    continue;
                }
                let clip_start = token.range.start.max(row_byte_start);
                let clip_end = token.range.end.min(row_byte_end);
                let text_slice = &full_text[clip_start..clip_end];
                if text_slice.is_empty() {
                    continue;
                }
                let col_chars = line.text[..clip_start - row_byte_start].chars().count();
                let x = origin.x + col_chars as f32 * cell_w;
                let color = color_for_token_kind(token.kind);
                painter.text(
                    Pos2::new(x, y),
                    egui::Align2::LEFT_TOP,
                    text_slice,
                    font_id.clone(),
                    color,
                );
                painted_any = true;
            }
            if !painted_any {
                painter.text(
                    Pos2::new(origin.x, y),
                    egui::Align2::LEFT_TOP,
                    line.text,
                    font_id.clone(),
                    EDITOR_FG,
                );
            }
        }

        if caret_visible && line.cursor_on_line {
            // Thin vertical-bar caret (line cursor), not a block —
            // the user asked for a familiar text-editor caret. The
            // live render path drives `caret_visible` on a wall-
            // clock blink cycle; snapshot tests pass `true`.
            let cursor_x = origin.x + line.cursor_col_chars as f32 * cell_w;
            let cursor_rect =
                Rect::from_min_size(Pos2::new(cursor_x, y), Vec2::new(EDITOR_CARET_WIDTH, row_h));
            painter.rect_filled(cursor_rect, 0.0, EDITOR_CURSOR_COLOR);
        }
    }
}

/// Background fill for the cwd / exit chip painted above each block.
/// A near-black grey that sits clearly above `DEFAULT_BG` without
/// shouting — it reads as "label affordance" rather than as content.
pub const BLOCK_HEADER_CHIP_BG: Color32 = Color32::from_rgb(0x22, 0x22, 0x22);

/// 1px stroke around each chip in the block header. "Quite dim"
/// per user direction — visible enough to outline the chip against
/// the (very similar) block background, not so loud that it
/// competes with the chip text inside.
pub const BLOCK_HEADER_CHIP_STROKE: Color32 = Color32::from_rgb(0x44, 0x44, 0x44);

/// Background wash painted behind a sealed block whose command
/// finished with a non-zero exit code. Translucent — picked via
/// `cargo run --example pick_failed_block_bg` (variant `a18` =
/// alpha 0x18 ≈ 9%). Reads as a warm shadow, not a red fill, so
/// the styled snapshot text on top stays fully legible.
//
// Stored premultiplied: each channel × (alpha / 255). Source is
// unmultiplied rgba(0x80, 0x20, 0x20, 0x18):
//   r = 128 * 0x18/255 ≈ 0x0c
//   g =  32 * 0x18/255 ≈ 0x03
//   b =  32 * 0x18/255 ≈ 0x03
// → premul (0x0c, 0x03, 0x03, 0x18).
pub const FAILED_BLOCK_BG: Color32 = Color32::from_rgba_premultiplied(0x0c, 0x03, 0x03, 0x18);

/// Vertical space (top AND bottom of the hairline) inserted
/// between sealed blocks. Picked via `pick_block_separator`,
/// adjusted to 10 px per the user's preference between the 8px
/// and 12px variants. Total inter-block "breath" is
/// `2 * BLOCK_SEPARATOR_GAP + 1` px with the hairline centered.
pub const BLOCK_SEPARATOR_GAP: f32 = 10.0;

/// 1px hairline between sealed blocks. Picked variant `h8-18` =
/// alpha 0x18 (~9%, "barely" visible). Stored in premultiplied
/// form: each RGB channel must be ≤ alpha. unmultiplied
/// `rgba(0xa0, 0xa0, 0xa0, 0x18)` premultiplies to roughly
/// `(0x10, 0x10, 0x10, 0x18)` — a barely-perceptible warm dim grey.
pub const BLOCK_SEPARATOR_HAIRLINE: Color32 =
    Color32::from_rgba_premultiplied(0x10, 0x10, 0x10, 0x18);

/// Padding inside each chip, in logical pixels. Affects both the
/// horizontal padding around the text and the chip's `corner_radius`
/// proportionally. Empirically tuned against the monospace font.
pub const CHIP_PAD_X: f32 = 6.0;
pub const CHIP_PAD_Y: f32 = 2.0;
pub const CHIP_CORNER_RADIUS: f32 = 4.0;
pub const CHIP_GAP: f32 = 4.0;

/// Paint the dim header line above a block as one or two rounded
/// chips: the cwd on the left, an optional `exit N` annotation
/// (red text) on the right. The cwd is shown with `$HOME` substituted
/// for `~` per [`crate::home_relative_cwd`], matching the tab-title
/// convention.
///
/// The first piece of [4G](../spec/10-roadmap.md#phase-4--editor-at-prompt-block-model-pivot)
/// block chrome. Renders only the cwd today; the git branch and
/// dirty-summary chips called for in spec/04 §"Visual structure"
/// live in Phase 5's async-probe surface (`termica-context`). Live
/// duration timers for `Running` blocks also defer to that phase —
/// the wall-clock plumbing isn't in place yet.
///
/// When `cwd` is `None` *and* there's nothing to show on the right
/// (no non-zero `exit`), nothing is painted at all — the header
/// row is skipped entirely so the block looks identical to the
/// pre-4G layout.
pub fn paint_block_header(
    ui: &mut egui::Ui,
    cwd: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
    exit: Option<i32>,
) -> Option<Response> {
    let font_id = FontId::monospace(DEFAULT_FONT_SIZE);
    let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let row_h = ui.fonts_mut(|f| f.row_height(&font_id));

    let cwd_text = cwd.map(|p| crate::home_relative_cwd(p, home)).unwrap_or_default();
    let show_exit = matches!(exit, Some(n) if n != 0);
    let exit_text = match exit {
        Some(n) if n != 0 => format!("exit {n}"),
        _ => String::new(),
    };

    if cwd_text.is_empty() && !show_exit {
        // Nothing to render. We deliberately allocate **nothing** —
        // not even a zero-sized rect — because egui inserts an
        // `item_spacing` gap after every allocated widget, and a
        // gratuitous gap above each block (cwd is None until shell
        // integration confirms) would shift the entire visual layout.
        return None;
    }

    // Layout the row first so we know the total width to allocate.
    // Each chip is `text_width + 2 * CHIP_PAD_X` wide and
    // `row_h + 2 * CHIP_PAD_Y` tall; chips are spaced `CHIP_GAP`
    // apart horizontally.
    let cwd_chip_w = if cwd_text.is_empty() {
        0.0
    } else {
        cwd_text.chars().count() as f32 * cell_w + 2.0 * CHIP_PAD_X
    };
    let exit_chip_w =
        if show_exit { exit_text.chars().count() as f32 * cell_w + 2.0 * CHIP_PAD_X } else { 0.0 };
    let between_gap = if !cwd_text.is_empty() && show_exit { CHIP_GAP } else { 0.0 };
    let total_w = cwd_chip_w + between_gap + exit_chip_w;
    let chip_h = row_h + 2.0 * CHIP_PAD_Y;
    let size = Vec2::new(total_w.max(cell_w), chip_h);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    let radius = CHIP_CORNER_RADIUS as u8;
    let chip_stroke = egui::Stroke::new(1.0, BLOCK_HEADER_CHIP_STROKE);
    if !cwd_text.is_empty() {
        let chip_rect = Rect::from_min_size(rect.min, Vec2::new(cwd_chip_w, chip_h));
        painter.rect_filled(chip_rect, radius, BLOCK_HEADER_CHIP_BG);
        painter.rect_stroke(chip_rect, radius, chip_stroke, egui::StrokeKind::Inside);
        painter.text(
            Pos2::new(chip_rect.min.x + CHIP_PAD_X, chip_rect.min.y + CHIP_PAD_Y),
            egui::Align2::LEFT_TOP,
            &cwd_text,
            font_id.clone(),
            BLOCK_HEADER_FG,
        );
    }
    if show_exit {
        let chip_x = rect.min.x + cwd_chip_w + between_gap;
        let chip_rect =
            Rect::from_min_size(Pos2::new(chip_x, rect.min.y), Vec2::new(exit_chip_w, chip_h));
        painter.rect_filled(chip_rect, radius, BLOCK_HEADER_CHIP_BG);
        painter.rect_stroke(chip_rect, radius, chip_stroke, egui::StrokeKind::Inside);
        painter.text(
            Pos2::new(chip_rect.min.x + CHIP_PAD_X, chip_rect.min.y + CHIP_PAD_Y),
            egui::Align2::LEFT_TOP,
            &exit_text,
            font_id,
            BLOCK_HEADER_EXIT_FAIL_FG,
        );
    }
    Some(response)
}

/// Paint a one-or-more-line command label in editor colors above the
/// snapshot of the block that ran it.
///
/// With ZLE/readline disabled per spec/04, the kernel echo of the
/// submitted bytes is suppressed by [`crate::echo_suppress::EchoSuppressor`]
/// — which is correct for the live `Term`, but means the typed
/// command never appears anywhere visible to the user. The sealed
/// block stores the command string (`Block::Sealed.command`); this
/// helper paints it as a teal header line so the user can see which
/// command produced the output below. Same idea for `Running` —
/// without this label the user sees output streaming but doesn't
/// know which command is producing it.
///
/// The dim cwd / exit header above the command label is painted
/// separately by [`paint_block_header`]; this helper handles the
/// command line itself.
pub fn paint_command_label(ui: &mut egui::Ui, command: &str) -> Response {
    paint_command_label_with_selection(ui, command, None)
}

/// Same as [`paint_command_label`] but with optional selection
/// overlay (rows in `selection.0.row..=selection.1.row` are
/// indexed against the command's own `split('\n')` lines, **not**
/// the unified block-row space — the caller is expected to have
/// already clipped any unified selection to the command's row
/// range and shifted it to 0-based). Endpoints must be in reading
/// order. Highlight rectangles are painted **under** the glyphs.
pub fn paint_command_label_with_selection(
    ui: &mut egui::Ui,
    command: &str,
    selection: Option<(BlockCursor, BlockCursor)>,
) -> Response {
    let font_id = FontId::monospace(DEFAULT_FONT_SIZE);
    let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let row_h = ui.fonts_mut(|f| f.row_height(&font_id));
    let lines: Vec<&str> = command.split('\n').collect();
    let cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(1);
    let rows = lines.len();
    let size = Vec2::new(cols as f32 * cell_w, rows as f32 * row_h);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    // Selection overlay first (under the glyphs).
    if let Some((start, end)) = selection
        && start != end
    {
        for (i, line) in lines.iter().enumerate() {
            if i < start.row || i > end.row {
                continue;
            }
            let line_chars = line.chars().count();
            let (col_lo, col_hi) = if start.row == end.row {
                (start.col, end.col)
            } else if i == start.row {
                (start.col, line_chars)
            } else if i == end.row {
                (0, end.col)
            } else {
                (0, line_chars)
            };
            let col_hi = col_hi.min(line_chars.max(col_lo));
            if col_hi <= col_lo {
                continue;
            }
            let x0 = rect.min.x + col_lo as f32 * cell_w;
            let x1 = rect.min.x + col_hi as f32 * cell_w;
            let y0 = rect.min.y + i as f32 * row_h;
            let y1 = y0 + row_h;
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1)),
                0.0,
                SELECTION_COLOR,
            );
        }
    }

    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        painter.text(
            Pos2::new(rect.min.x, rect.min.y + i as f32 * row_h),
            egui::Align2::LEFT_TOP,
            line,
            font_id.clone(),
            EDITOR_FG,
        );
    }
    response
}

/// Paint one finished command block: a teal command-label line above
/// its frozen `Vec<StyledLine>` snapshot. The same helper is used by
/// `render_pane` for live rendering and by snapshot tests so visual
/// regressions in either path show up in the kittest goldens.
///
/// The caller is responsible for adding the inter-block separator
/// (e.g. `ui.add_space(4.0)`); a sealed block on its own has no
/// trailing gap.
///
/// `selection`, when `Some`, paints a teal overlay across the
/// covered cells in the snapshot. The caller is responsible for
/// passing `Some` only when the selection belongs to *this* block;
/// `paint_sealed_block` does not know its own [`crate::block::BlockId`].
/// Endpoints must already be in reading order (start ≤ end).
///
/// Returns the [`Response`] over the *snapshot* region — i.e. the
/// hit-testable rect for sealed-block selection. The command label
/// region is excluded so that clicks on the label don't begin a
/// selection that would visually start above the highlight.
/// Result of painting a sealed block: the union [`Rect`] over the
/// command label + snapshot regions (excluding the header chip),
/// plus the row count of the command label. Used by
/// [`crate::render_pane`] to translate pointer-pixel positions
/// into a [`BlockCursor`] in the block's unified row space
/// (`0..command_lines` = command rows, the rest = snapshot rows).
#[derive(Debug, Clone, Copy)]
pub struct SealedBlockRender {
    /// Hit-test rect covering both command-label and snapshot
    /// regions. The header chip is NOT included so clicks on the
    /// chip don't start a selection.
    pub rect: Rect,
    /// Number of rows the command label occupies. Snapshot rows
    /// start at this index in the unified row space.
    pub command_lines: usize,
}

pub fn paint_sealed_block(
    ui: &mut egui::Ui,
    command: &str,
    snapshot: &[StyledLine],
    selection: Option<(BlockCursor, BlockCursor)>,
    cwd: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
    exit: Option<i32>,
) -> SealedBlockRender {
    // Reserve a backing shape index BEFORE any paint so the
    // failed-block bg wash can be slotted underneath the chip +
    // command + snapshot. We don't know the full block rect until
    // after layout, so we paint chip + content first and then set
    // the shape via `painter.set()` once `rect` is known.
    let failed = matches!(exit, Some(n) if n != 0);
    let bg_idx = if failed { Some(ui.painter().add(egui::Shape::Noop)) } else { None };

    let _ = paint_block_header(ui, cwd, home, exit);

    let cmd_lines = if command.is_empty() { 0 } else { command.split('\n').count() };
    // Split the unified selection into command and snapshot pieces.
    let (cmd_sel, snap_sel) = split_selection_at_row(selection, cmd_lines);

    let cmd_rect = if !command.is_empty() {
        Some(paint_command_label_with_selection(ui, command, cmd_sel).rect)
    } else {
        None
    };

    let snap_rect = paint_styled_lines(ui, snapshot, snap_sel).rect;
    let rect = match cmd_rect {
        Some(c) => c.union(snap_rect),
        None => snap_rect,
    };

    if let Some(idx) = bg_idx {
        // Extend the wash slightly outside the content's tight rect
        // so it reads as a "block background" rather than text-hugging.
        let wash = rect.expand2(egui::vec2(4.0, 2.0));
        ui.painter().set(idx, egui::Shape::rect_filled(wash, 4.0, FAILED_BLOCK_BG));
    }

    SealedBlockRender { rect, command_lines: cmd_lines }
}

/// Clip a unified-row [`BlockSelection`] range to the command
/// region (rows `0..cmd_lines`) and the snapshot region (rows
/// `cmd_lines..`), returning each piece in its own 0-based row
/// space. Either piece is `None` when no part of the selection
/// lands in that region.
type SelectionRange = (BlockCursor, BlockCursor);
fn split_selection_at_row(
    selection: Option<SelectionRange>,
    cmd_lines: usize,
) -> (Option<SelectionRange>, Option<SelectionRange>) {
    let Some((start, end)) = selection else { return (None, None) };
    if start == end {
        return (None, None);
    }
    // Both endpoints in command region.
    if end.row < cmd_lines {
        return (Some((start, end)), None);
    }
    // Both endpoints in snapshot region.
    if start.row >= cmd_lines {
        let s = BlockCursor::new(start.row - cmd_lines, start.col);
        let e = BlockCursor::new(end.row - cmd_lines, end.col);
        return (None, Some((s, e)));
    }
    // Spans both regions.
    let cmd_end = BlockCursor::new(cmd_lines - 1, usize::MAX);
    let snap_start = BlockCursor::new(0, 0);
    let snap_end = BlockCursor::new(end.row - cmd_lines, end.col);
    (Some((start, cmd_end)), Some((snap_start, snap_end)))
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
fn paint_selection_overlay(
    painter: &egui::Painter,
    grid: &Grid<Cell>,
    start: Point,
    end: Point,
    g: GridGeometry,
) {
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

        // Stop the highlight at the last typed cell on this row so
        // the user can't visually "select" the grid's imaginary
        // right-margin space padding. `last_typed_col` returns
        // `None` for an all-space row → skip the row entirely.
        let line = Line(vrow - g.display_offset);
        let Some(last_typed) = crate::selection::last_typed_col(grid, line, g.cols) else {
            continue;
        };
        let col_hi = col_hi.min(last_typed);
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

/// Paint the Cmd-hover affordance for `link`: a translucent blue
/// rectangle over the link's cells and a 1.5 px underline 1.5 px
/// above the cell bottom (matches the regular underline decoration
/// in [`paint_terminal`]).
///
/// The overlay alpha is low enough (≈ 28 %) that the glyphs remain
/// fully legible; the underline reinforces the link affordance at a
/// glance. Single-row only — multi-row URL stitching is deferred
/// (see [`crate::links`]).
fn paint_link_underline(painter: &egui::Painter, link: &LinkSpan, g: GridGeometry) {
    let viewport_row = link.start.line.0 + g.display_offset;
    if viewport_row < 0 || viewport_row >= g.screen_lines as i32 {
        return;
    }

    let col_lo = link.start.column.0.min(g.cols.saturating_sub(1));
    let col_hi = link.end.column.0.min(g.cols.saturating_sub(1));

    let x0 = g.origin_x + col_lo as f32 * g.cell_w;
    let x1 = g.origin_x + (col_hi as f32 + 1.0) * g.cell_w;
    let row_top = g.origin_y + viewport_row as f32 * g.row_h;
    let row_bottom = row_top + g.row_h;
    let underline_y = row_bottom - 1.5;

    painter.rect_filled(
        Rect::from_min_max(Pos2::new(x0, row_top), Pos2::new(x1, row_bottom)),
        0.0,
        LINK_HOVER_OVERLAY_COLOR,
    );
    painter.line_segment(
        [Pos2::new(x0, underline_y), Pos2::new(x1, underline_y)],
        Stroke::new(1.5, LINK_UNDERLINE_COLOR),
    );
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

    /// 2×2×2 truth table for the caret-visibility rule. Spec/04
    /// says the caret is shown iff `mode_is_editor && pane_has_focus
    /// && viewport_focused`; everything else hides it.
    #[test]
    fn should_show_caret_is_three_way_and() {
        for &mode in &[false, true] {
            for &pane in &[false, true] {
                for &vp in &[false, true] {
                    let got = should_show_caret(mode, pane, vp);
                    let want = mode && pane && vp;
                    assert_eq!(got, want, "mode={mode}, pane_focus={pane}, viewport_focused={vp}",);
                }
            }
        }
    }

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
