//! UI snapshot tests via `egui_kittest`.
//!
//! Cell-renderer tests (`snapshot_terminal_*`) construct a real
//! [`TerminalState`], feed it synthetic bytes, and snapshot the
//! rendered grid through `render::paint_terminal`. Block / editor /
//! sealed-snapshot tests (`snapshot_paint_*`) drive the helper
//! functions in `render` directly with fixed inputs.
//!
//! Update baselines with:
//!
//!     UPDATE_SNAPSHOTS=true cargo test --workspace
//!
//! After regenerating, inspect every changed `.png` and any
//! `*.diff.png` per [CLAUDE.md](../CLAUDE.md#snapshot-review).
//! Snapshots live at `tests/snapshots/<name>.png`; thresholds in
//! [`kittest.toml`](../kittest.toml) are looser on Linux than macOS
//! to absorb GPU/driver pixel differences.

#![forbid(unsafe_code)]

use eframe::egui;
use egui_kittest::{Harness, SnapshotOptions};
use termica::render;
use termica::terminal::TerminalState;

/// Snapshot options for views that contain **drawn icon glyphs**
/// (`icons.rs`: the find/completion arrows, the ⌘/⌥/^ key caps, the
/// filter and dropdown triangles). Thin vector strokes rasterize ~1px
/// differently across GPU backends — Metal on macOS vs lavapipe on the
/// CI Linux runner — so an otherwise-identical render trips
/// egui_kittest's default zero-pixel tolerance (observed: exactly 1
/// pixel on the completion-popup keybind hint). Allow a small pixel
/// budget: enough to absorb sub-pixel AA on a handful of glyph edges,
/// far below any real regression (a wrong or missing glyph/row moves
/// hundreds of pixels). Text- and cell-only snapshots keep the strict
/// default, since egui's CPU glyph rasterizer is cross-platform
/// deterministic.
fn drawn_glyph_snapshot_options() -> SnapshotOptions {
    SnapshotOptions::new().failed_pixel_count_threshold(16)
}

/// Build a sealed-block snapshot from synthetic bytes: feed bytes into
/// a fresh `TerminalState`, then take a `snapshot_lines_all()` — grid
/// rows at `cols` width. Production (Phase 9B) un-wraps these to logical
/// lines at seal and re-wraps them to the current width on render; these
/// renderer tests paint the grid rows directly as representative visual
/// rows (at a width where nothing wraps the result is identical). The
/// reflow path itself is covered by `reflow`'s unit tests and the
/// `*_reflow_narrow` snapshot below.
fn sealed_snapshot(rows: u16, cols: u16, bytes: &[u8]) -> Vec<termica::terminal::StyledLine> {
    let mut t = TerminalState::new(rows, cols);
    t.feed(bytes);
    t.snapshot_lines_all()
}

// ---- cell-renderer fixtures ----------------------------------------------

fn term_from_bytes(rows: u16, cols: u16, bytes: &[u8]) -> TerminalState {
    let mut t = TerminalState::new(rows, cols);
    t.feed(bytes);
    t
}

#[test]
fn snapshot_terminal_plain_text() {
    let term = term_from_bytes(
        6,
        40,
        b"hello, world\r\nthis is a test of the cell\r\nrenderer in Termica.",
    );
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 200.0)).build_ui(move |ui| {
            render::paint_terminal(ui, &term, None, None, false, true, false);
        });
    harness.snapshot("terminal_plain_text");
}

#[test]
fn snapshot_terminal_ansi_colors() {
    // One line of each base color: SGR 30-37 = fg colors. We also
    // light a backdrop bg on the cyan run with SGR 46 to exercise the
    // per-cell background-rect path.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"\x1b[31mred\x1b[0m   ");
    bytes.extend_from_slice(b"\x1b[32mgreen\x1b[0m   ");
    bytes.extend_from_slice(b"\x1b[33myellow\x1b[0m");
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(b"\x1b[34mblue\x1b[0m  ");
    bytes.extend_from_slice(b"\x1b[35mmag\x1b[0m  ");
    bytes.extend_from_slice(b"\x1b[36;46m cyan-on-cyan-bg \x1b[0m");
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(b"\x1b[91mbright-red\x1b[0m  \x1b[37mwhite\x1b[0m");

    let term = term_from_bytes(5, 50, &bytes);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 180.0)).build_ui(move |ui| {
            render::paint_terminal(ui, &term, None, None, false, true, false);
        });
    harness.snapshot("terminal_ansi_colors");
}

#[test]
fn snapshot_terminal_cursor_visible_at_end_of_text() {
    // Feed some text; the alacritty cursor advances cell-by-cell with
    // each character, so it ends up immediately after "hi" on row 0.
    // The renderer must paint the cursor block there.
    let term = term_from_bytes(4, 30, b"hi");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 160.0)).build_ui(move |ui| {
            render::paint_terminal(ui, &term, None, None, false, true, false);
        });
    harness.snapshot("terminal_cursor_visible");
}

#[test]
fn snapshot_terminal_cursor_dim_when_unfocused() {
    // Same setup as `cursor_visible` but with `focused = false`.
    // The cursor block should still render but in the muted
    // `CURSOR_UNFOCUSED_COLOR` so the user can see at a glance
    // that the pane isn't receiving input.
    let term = term_from_bytes(4, 30, b"hi");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 160.0)).build_ui(move |ui| {
            render::paint_terminal(ui, &term, None, None, false, false, false);
        });
    harness.snapshot("terminal_cursor_unfocused");
}

#[test]
fn snapshot_terminal_cursor_hidden_via_dectcem() {
    // `\e[?25l` hides the cursor (DECTCEM low). The renderer must
    // NOT draw the cursor block when this is set — programs use this
    // to indicate "I'm in the middle of a repaint, don't show me
    // until I'm done".
    let term = term_from_bytes(4, 30, b"hi\x1b[?25l");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 160.0)).build_ui(move |ui| {
            render::paint_terminal(ui, &term, None, None, false, true, false);
        });
    harness.snapshot("terminal_cursor_hidden");
}

#[test]
fn snapshot_terminal_cell_attributes() {
    // Three rows of attribute combos. Each is rendered against the
    // standard color palette so the visual differences from the
    // default cell are obvious in the diff.
    //
    // Row 0: SGR 1 (bold), SGR 2 (dim), SGR 22 (normal) — same word
    //        three times, so the rendered width / brightness diff
    //        across the three is exactly the attribute effect.
    // Row 1: SGR 4 (underline), SGR 9 (strikeout), both — line
    //        decorations under and through the text.
    // Row 2: SGR 7 (inverse) of plain text + SGR 8 (hidden) so the
    //        word disappears (the background sticks around).
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"\x1b[1mBOLD\x1b[0m  ");
    bytes.extend_from_slice(b"\x1b[2mDIM\x1b[0m  ");
    bytes.extend_from_slice(b"\x1b[22mNORMAL\x1b[0m");
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(b"\x1b[4munderline\x1b[0m  ");
    bytes.extend_from_slice(b"\x1b[9mstrikeout\x1b[0m  ");
    bytes.extend_from_slice(b"\x1b[4;9mboth\x1b[0m");
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(b"\x1b[7mINVERSE\x1b[0m  ");
    bytes.extend_from_slice(b"\x1b[8mHIDDEN\x1b[0m");

    let term = term_from_bytes(5, 50, &bytes);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 180.0)).build_ui(move |ui| {
            render::paint_terminal(ui, &term, None, None, false, true, false);
        });
    harness.snapshot("terminal_cell_attributes");
}

#[test]
fn snapshot_terminal_scrolled_into_scrollback() {
    // Emit enough numbered lines to push earlier ones into the
    // scrollback buffer, then scroll the display up by 5 rows. The
    // visible region should now show older rows instead of the
    // most-recent ones — that's the mouse-wheel-scroll experience.
    let mut term = TerminalState::new(5, 30);
    for i in 0..20 {
        let line = format!("row-{i:02}\r\n");
        term.feed(line.as_bytes());
    }
    term.scroll_display(5);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 180.0)).build_ui(move |ui| {
            render::paint_terminal(ui, &term, None, None, false, true, false);
        });
    harness.snapshot("terminal_scrolled_into_scrollback");
}

#[test]
fn snapshot_terminal_alt_screen() {
    // Enter alt screen, write some content (with explicit cursor home
    // because alacritty does NOT reposition the cursor on `\e[?1049h`).
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"main-screen-content\r\n");
    bytes.extend_from_slice(b"\x1b[?1049h"); // alt screen on
    bytes.extend_from_slice(b"\x1b[H"); // cursor home
    bytes.extend_from_slice(b"-- alt screen active --\r\n");
    bytes.extend_from_slice(b"line 2 on alt screen");

    let term = term_from_bytes(5, 40, &bytes);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 180.0)).build_ui(move |ui| {
            render::paint_terminal(ui, &term, None, None, false, true, false);
        });
    harness.snapshot("terminal_alt_screen");
}

#[test]
fn snapshot_terminal_alt_screen_with_border() {
    // Same alt-screen content as `snapshot_terminal_alt_screen` but
    // includes the 1px alt-screen indicator border that
    // `render_pane::render_pane` paints around the grid when
    // `view.alt_screen` is true. Snapshots the border colour and
    // exact placement so future tweaks land deliberately.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"\x1b[?1049h"); // alt screen on
    bytes.extend_from_slice(b"\x1b[H"); // cursor home
    bytes.extend_from_slice(b"-- alt screen active --\r\n");
    bytes.extend_from_slice(b"line 2 on alt screen");

    let term = term_from_bytes(5, 40, &bytes);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 180.0)).build_ui(move |ui| {
            let rendered = render::paint_terminal(ui, &term, None, None, false, true, false);
            termica::paint_alt_screen_border(ui.painter(), rendered.response.rect);
        });
    harness.snapshot("terminal_alt_screen_with_border");
}

#[test]
fn paint_terminal_with_include_history_renders_scrollback_rows() {
    // A 3-row grid that has had 10 lines printed pushes 7 lines into
    // history. With `include_history = true` the painter should
    // allocate (history + screen_lines) row-heights worth of space
    // so a running command's earlier output stays scrollable in the
    // outer `ScrollArea` instead of being clipped to the visible
    // viewport.
    use alacritty_terminal::grid::Dimensions;
    let mut bytes = Vec::new();
    for i in 0..10 {
        bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    let history_size = term_from_bytes(3, 12, &bytes).grid().history_size();
    let screen_lines = term_from_bytes(3, 12, &bytes).grid().screen_lines();
    assert!(history_size > 0, "fixture should have pushed lines into history");

    fn measure(include_history: bool, bytes: &[u8]) -> f32 {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_closure = captured.clone();
        let term = term_from_bytes(3, 12, bytes);
        let _harness =
            Harness::builder().with_size(egui::Vec2::new(400.0, 600.0)).build_ui(move |ui| {
                let rendered =
                    render::paint_terminal(ui, &term, None, None, false, true, include_history);
                *captured_for_closure.lock().unwrap() = Some(rendered.response.rect.height());
            });
        captured.lock().unwrap().expect("paint_terminal ran")
    }

    let h_full = measure(true, &bytes);
    let h_viewport = measure(false, &bytes);
    assert!(
        h_full > h_viewport,
        "include_history must grow the painted region (full={h_full}, viewport={h_viewport})",
    );
    // row_h drops out of the ratio so the assertion holds across
    // platforms / DPIs without hard-coding pixel counts.
    let row_h = h_viewport / (screen_lines as f32);
    let expected_h = (history_size + screen_lines) as f32 * row_h;
    assert!(
        (h_full - expected_h).abs() < 1.0,
        "painted height {h_full} should match {expected_h} (rows={}, row_h={row_h})",
        history_size + screen_lines,
    );
}

#[test]
fn paint_terminal_with_include_history_stops_at_cursor_when_output_fits_grid() {
    // Regression for the "panel-sized black void below the live
    // output" bug. When a Running command's output fits the grid
    // (e.g. `while true; do sleep 1; date; done` after a few
    // seconds in a tall pane), the cursor sits at row N << screen_
    // lines and the rows below it are empty. The previous
    // `include_history=true` path painted ALL `(history_size +
    // screen_lines)` rows, so the user saw the date lines at the
    // top and `screen_lines - N - 1` empty rows of `DEFAULT_BG`
    // below — a panel-tall black panel that snapped away the
    // moment `CommandFinished` reset the grid.
    //
    // The fix clamps the painted region to `history_size +
    // cursor_row + 1` viewport rows. The viewport rows above the
    // cursor still paint (so a `tput cup` style program that
    // writes above the cursor doesn't lose its top-half output);
    // the rows below disappear because they have no content
    // anyway.
    use alacritty_terminal::grid::Dimensions;
    let bytes = b"line a\r\nline b\r\nline c\r\n";
    let term = term_from_bytes(20, 12, bytes);
    let screen_lines = term.grid().screen_lines();
    assert_eq!(screen_lines, 20, "test scaffolding");
    let cursor_row = term.grid().cursor.point.line.0;
    assert_eq!(cursor_row, 3, "test scaffolding: 3 CRLFs land cursor at row 3");
    let history_size = term.grid().history_size();
    assert_eq!(history_size, 0, "3 lines fit in a 20-row screen — no scrollback");

    fn measure(include_history: bool, bytes: &[u8]) -> f32 {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_closure = captured.clone();
        let term = term_from_bytes(20, 12, bytes);
        let _harness =
            Harness::builder().with_size(egui::Vec2::new(400.0, 800.0)).build_ui(move |ui| {
                let rendered =
                    render::paint_terminal(ui, &term, None, None, false, true, include_history);
                *captured_for_closure.lock().unwrap() = Some(rendered.response.rect.height());
            });
        captured.lock().unwrap().expect("paint_terminal ran")
    }

    let h_full_viewport = measure(false, bytes);
    let h_clamped = measure(true, bytes);
    // Derive row_h from the no-history measurement: it paints
    // exactly `screen_lines` rows tall, so row_h = h/screen_lines.
    let row_h = h_full_viewport / (screen_lines as f32);

    // The clamped paint must cover EXACTLY `cursor_row + 1` rows
    // (plus the zero-sized history) — not `screen_lines`.
    let expected_h = (cursor_row as u32 + 1) as f32 * row_h;
    assert!(
        (h_clamped - expected_h).abs() < 1.0,
        "clamped paint height {h_clamped} should match {expected_h} \
         (cursor_row+1 = {}, row_h = {row_h}); previous bug would have \
         given {} ({} × row_h)",
        cursor_row + 1,
        h_full_viewport,
        screen_lines,
    );
    // Sanity: the clamped paint is strictly shorter than the
    // no-clamp (full-viewport) paint when output doesn't fill the
    // grid.
    assert!(
        h_clamped < h_full_viewport,
        "clamped paint must be shorter than the full viewport when output \
         leaves rows below the cursor; got clamped={h_clamped}, full={h_full_viewport}",
    );
}

#[test]
fn snapshot_terminal_link_underline() {
    // A URL on row 0 with the Cmd/Ctrl-hover underline rendered. The
    // call site only passes a `Some(link)` when the modifier is held,
    // so this snapshot stands in for "user is hovering with Cmd held".
    use alacritty_terminal::index::{Column, Line, Point};
    use termica::links::scan_visible_links;

    let term = term_from_bytes(3, 40, b"open https://example.com please");
    // Resolve the link via the same scanner the app uses, so the
    // snapshot validates both the renderer AND the detector.
    let spans = scan_visible_links(term.grid());
    assert_eq!(spans.len(), 1, "scanner should find the URL");
    let link = spans.into_iter().next().expect("one link");
    // Sanity: it should be on row 0.
    assert_eq!(link.start, Point::new(Line(0), Column(5)));

    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 120.0)).build_ui(move |ui| {
            render::paint_terminal(ui, &term, None, Some(&link), false, true, false);
        });
    harness.snapshot("terminal_link_underline");
}

// ---- sealed-block snapshots (Phase 4A-render) ----------------------------
//
// When a command finishes, its output is frozen into a
// `Vec<StyledLine>` and painted via `render::paint_styled_lines`.
// These tests exercise that path directly so a regression in
// sealed-block rendering shows up independently of the live-grid
// `paint_terminal` snapshots.

#[test]
fn snapshot_paint_styled_lines_plain_text() {
    let snapshot = sealed_snapshot(4, 40, b"$ echo hello\r\nhello\r\n");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 140.0)).build_ui(move |ui| {
            let _ = render::paint_styled_lines(ui, &snapshot, None);
        });
    harness.snapshot("paint_styled_lines_plain_text");
}

#[test]
fn snapshot_paint_styled_lines_with_ansi_colors() {
    // Same colour exercise as the live-grid test, captured separately
    // so the sealed-block path's colour resolution stays pinned to
    // match the live path. They share `cell_colors_for`; this test
    // proves that shared helper is wired in correctly here too.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"\x1b[31mred\x1b[0m  ");
    bytes.extend_from_slice(b"\x1b[32mgreen\x1b[0m  ");
    bytes.extend_from_slice(b"\x1b[36;46m cyan-on-cyan-bg \x1b[0m");
    bytes.extend_from_slice(b"\r\n");
    let snapshot = sealed_snapshot(3, 50, &bytes);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 100.0)).build_ui(move |ui| {
            let _ = render::paint_styled_lines(ui, &snapshot, None);
        });
    harness.snapshot("paint_styled_lines_ansi_colors");
}

// ---- editor snapshots (Phase 4B) ----------------------------------------

#[test]
fn snapshot_paint_prompt_editor_empty() {
    let editor = termica::prompt_editor::PromptEditor::new();
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(400.0, 80.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_empty");
}

#[test]
fn snapshot_paint_prompt_editor_with_text_and_cursor_at_end() {
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("git status");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(400.0, 80.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_typed");
}

// Spec/04 "When is the caret shown?" — paired snapshots verifying
// the caret is drawn IFF the window is foreground. Same editor
// content; the only pixel delta is the caret column.

#[test]
fn snapshot_prompt_editor_caret_when_foreground() {
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("git status");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(400.0, 80.0)).build_ui(move |ui| {
            let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
            let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
            let row_h = ui.fonts_mut(|f| f.row_height(&font_id));
            let (rect, _) = ui.allocate_exact_size(egui::vec2(400.0, row_h), egui::Sense::hover());
            render::paint_prompt_editor_at(
                ui.painter(),
                &editor,
                rect.min,
                cell_w,
                row_h,
                &font_id,
                render::should_show_caret(true, true, true),
            );
        });
    harness.snapshot("prompt_editor_caret_foreground");
}

#[test]
fn snapshot_prompt_editor_caret_when_window_not_foreground() {
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("git status");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(400.0, 80.0)).build_ui(move |ui| {
            let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
            let cell_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'M'));
            let row_h = ui.fonts_mut(|f| f.row_height(&font_id));
            let (rect, _) = ui.allocate_exact_size(egui::vec2(400.0, row_h), egui::Sense::hover());
            // Same pane focus, but the app's window is NOT the OS
            // foreground — the caret must be hidden.
            render::paint_prompt_editor_at(
                ui.painter(),
                &editor,
                rect.min,
                cell_w,
                row_h,
                &font_id,
                render::should_show_caret(true, true, false),
            );
        });
    harness.snapshot("prompt_editor_caret_not_foreground");
}

// ---- whole-block snapshots (command + output together) -------------------

#[test]
fn snapshot_paint_sealed_block_echo() {
    // The simplest end-to-end shape of a finished command: type
    // `echo hello`, kernel runs it, snapshot freezes the output.
    let snapshot = sealed_snapshot(3, 40, b"hello\r\n");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 100.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(
                ui,
                "echo hello",
                &snapshot,
                None,
                render::BlockHeader {
                    exit: Some(0),
                    duration: Some(std::time::Duration::from_millis(123)),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_sealed_block_echo");
}

// ---- focused-editor chrome: opaque backing -------------------------

#[test]
fn snapshot_focused_chrome_glow_backing_is_opaque() {
    // Regression guard for "scrollback shows through the glow": a vivid
    // background stands in for scrollback behind the prompt. The opaque
    // backing must cover it INSIDE the border so only the band outside
    // the glow keeps the bright color. If the backing ever stops
    // reaching the inner ring, those bright pixels reappear in the gap
    // next to the cursor and this snapshot moves.
    use termica::focused_chrome::{self, ChromeVariant};
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(400.0, 120.0)).build_ui(move |ui| {
            let painter = ui.painter();
            painter.rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(0xb0, 0x10, 0x10));
            // Chip + editor footprint, inset so the outer glow rings have
            // room to land inside the viewport.
            let body = egui::Rect::from_min_max(egui::pos2(40.0, 40.0), egui::pos2(360.0, 90.0));
            focused_chrome::paint_backing(painter, None, body, ChromeVariant::I, 1.0);
            focused_chrome::paint(painter, None, body, ChromeVariant::I, 1.0);
        });
    harness.snapshot_options("focused_chrome_glow_backing_opaque", &drawn_glyph_snapshot_options());
}

#[test]
fn snapshot_paint_sealed_block_reflows_long_line_at_narrow_width() {
    // Phase 9B: a logical line longer than the pane wraps into several
    // visual rows at render time. Build logical lines the way production
    // stores them (un-wrapped), then reflow to a narrow width before
    // painting — the result must show the line broken across rows.
    let logical = termica::persist::chunk::unwrap_rows(&sealed_snapshot(
        3,
        80,
        b"the quick brown fox jumps over the lazy dog\r\n",
    ));
    let visual = termica::reflow::ReflowMap::build(&logical, 20).visual_rows().to_vec();
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(320.0, 160.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(
                ui,
                "echo fox",
                &visual,
                None,
                render::BlockHeader { exit: Some(0), ..Default::default() },
            );
        });
    harness.snapshot("paint_sealed_block_reflow_narrow");
}

#[test]
fn snapshot_paint_sealed_block_ls_output() {
    // Multiple output rows under a single command label. Mirrors
    // what the user typically sees after `ls`.
    let snapshot = sealed_snapshot(6, 40, b"Cargo.toml\r\nREADME.md\r\nsrc\r\ntests\r\n");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 160.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(
                ui,
                "ls",
                &snapshot,
                None,
                render::BlockHeader {
                    exit: Some(0),
                    duration: Some(std::time::Duration::from_millis(1_500)),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_sealed_block_ls_output");
}

#[test]
fn snapshot_paint_sealed_block_multiline_command() {
    // A `Shift+Enter`-authored multi-line command above its single-
    // line output.
    let snapshot = sealed_snapshot(3, 40, b"1\r\n2\r\n");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 140.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(
                ui,
                "for i in 1 2; do\n  echo $i\ndone",
                &snapshot,
                None,
                render::BlockHeader {
                    exit: Some(0),
                    duration: Some(std::time::Duration::from_millis(34)),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_sealed_block_multiline_command");
}

#[test]
fn snapshot_paint_command_label_single_line() {
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_command_label(ui, "git status");
        });
    harness.snapshot("paint_command_label_single_line");
}

#[test]
fn snapshot_paint_command_label_multiline() {
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 80.0)).build_ui(move |ui| {
            let _ = render::paint_command_label(ui, "for f in *.rs; do\n  cat \"$f\"\ndone");
        });
    harness.snapshot("paint_command_label_multiline");
}

#[test]
fn snapshot_paint_prompt_editor_with_selection() {
    // User typed text, then Shift+Home (or Cmd+A) to select all —
    // selection rect should highlight the text behind the glyphs.
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("git status");
    editor.select_all();
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(400.0, 80.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_with_selection");
}

#[test]
fn snapshot_paint_prompt_editor_partial_selection() {
    // Cursor at byte 5, selection from byte 0 — should highlight
    // "hello" but not " world".
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("hello world");
    editor.set_cursor(0);
    editor.set_cursor_extending(5);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(400.0, 80.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_partial_selection");
}

#[test]
fn snapshot_paint_prompt_editor_multiline_selection() {
    // Multi-line selection — selection extends past the first row's
    // text + through to mid-second-row.
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("for f in *.rs; do");
    editor.insert_newline();
    editor.insert_str("  cat \"$f\"");
    // Select from byte 4 ("f" in "for f") through byte ~22 ("cat" on
    // line 2). Approximate; exact byte indices depend on string len.
    editor.set_cursor(4);
    editor.set_cursor_extending(22);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 100.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_multiline_selection");
}

#[test]
fn snapshot_paint_prompt_editor_multiline() {
    // Shift+Enter authored: two lines, cursor at the end of the
    // second line.
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("for f in *.rs; do");
    editor.insert_newline();
    editor.insert_str("  cat \"$f\"");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 100.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_multiline");
}

#[test]
fn snapshot_paint_prompt_editor_cursor_mid_text() {
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("alpha bravo charlie");
    // Move cursor back four chars (into "charlie").
    for _ in 0..4 {
        editor.move_left();
    }
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 80.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_cursor_mid_text");
}

// ---- syntax-highlighting snapshots (Phase 4H) ---------------------------
//
// The editor now tokenizes its text via `crate::shell_syntax` and
// paints each token in a kind-specific colour. These tests pin the
// visual identity for the v1 token set so colour regressions surface.

#[test]
fn snapshot_paint_prompt_editor_command_with_flag() {
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("ls -la /tmp");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 60.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_command_with_flag");
}

#[test]
fn snapshot_paint_prompt_editor_pipe_and_quotes() {
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("grep -i \"hello world\" foo.txt | wc -l");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(800.0, 60.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_pipe_and_quotes");
}

#[test]
fn snapshot_paint_prompt_editor_variable_and_redirect() {
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("echo $HOME > /tmp/out");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(600.0, 60.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_variable_and_redirect");
}

#[test]
fn snapshot_paint_prompt_editor_comment_at_end_of_line() {
    let mut editor = termica::prompt_editor::PromptEditor::new();
    editor.insert_str("ls -la # list everything");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(600.0, 60.0)).build_ui(move |ui| {
            let _ = render::paint_prompt_editor(ui, &editor);
        });
    harness.snapshot("paint_prompt_editor_comment_at_end_of_line");
}

// ---- sealed-block selection snapshots (Phase 4F) -------------------------
//
// `paint_styled_lines` and `paint_sealed_block` now accept an
// optional `(BlockCursor, BlockCursor)` selection in reading order.
// These tests pin the teal selection overlay across single-row,
// partial-row, and multi-row shapes so changes to the overlay
// rendering surface visually.

#[test]
fn snapshot_paint_styled_lines_with_single_row_selection() {
    use termica::block_selection::BlockCursor;
    let snapshot = sealed_snapshot(3, 40, b"hello world\r\n");
    let sel = Some((BlockCursor::new(0, 6), BlockCursor::new(0, 11)));
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 80.0)).build_ui(move |ui| {
            let _ = render::paint_styled_lines(ui, &snapshot, sel);
        });
    harness.snapshot("paint_styled_lines_single_row_selection");
}

#[test]
fn snapshot_paint_styled_lines_with_multi_row_selection() {
    use termica::block_selection::BlockCursor;
    let snapshot = sealed_snapshot(5, 40, b"alpha\r\nbravo\r\ncharlie\r\n");
    // Partial first row (col 2 → end), full middle row, partial last
    // row (0 → col 4).
    let sel = Some((BlockCursor::new(0, 2), BlockCursor::new(2, 4)));
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 140.0)).build_ui(move |ui| {
            let _ = render::paint_styled_lines(ui, &snapshot, sel);
        });
    harness.snapshot("paint_styled_lines_multi_row_selection");
}

// ---- block-header chrome (Phase 4G) -------------------------------------
//
// `paint_block_header` renders a dim cwd line plus optional red
// "exit N" annotation for non-zero exits. `paint_sealed_block` now
// stacks header → command label → snapshot.

#[test]
fn snapshot_paint_block_header_cwd_only() {
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(
                ui,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_block_header_cwd_only");
}

#[test]
fn snapshot_paint_block_header_zero_exit_hides_exit_chip() {
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(
                ui,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    exit: Some(0),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_block_header_zero_exit");
}

#[test]
fn snapshot_paint_block_header_substitutes_home_with_tilde() {
    // When the cwd is under $HOME, the chip should show `~/…` not
    // the absolute path — matches the tab-title convention.
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(
                ui,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_block_header_tilde_substitution");
}

#[test]
fn snapshot_paint_block_header_nonzero_exit_shows_red_annotation() {
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(
                ui,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    exit: Some(127),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_block_header_nonzero_exit");
}

#[test]
fn snapshot_paint_block_header_with_duration() {
    // The 4G duration: a dim `(…)` timer at the right of the header
    // row, after the cwd and (here) the "exit 1" chip.
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(
                ui,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    exit: Some(1),
                    duration: Some(std::time::Duration::from_secs(125)),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_block_header_with_duration");
}

#[test]
fn snapshot_paint_block_header_with_git_branch_clean() {
    // 4G-async-context: a clean repo on `main` — just the branch chip
    // after the cwd, no dirty / ahead-behind chips.
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let git =
        termica::git_context::GitContext { branch: Some("main".to_string()), ..Default::default() };
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(
                ui,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    git: Some(&git),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_block_header_git_branch_clean");
}

#[test]
fn snapshot_paint_block_header_with_git_dirty_and_sync() {
    // 4G-async-context: a feature branch, ahead 2 / behind 1, with a
    // dirty working tree — cwd, branch, `ahead 2 behind 1`, then the
    // amber dirty chip (`3 files +120 -8`).
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let git = termica::git_context::GitContext {
        branch: Some("feat/git-context-probe".to_string()),
        ahead: 2,
        behind: 1,
        dirty: termica::git_context::DirtySummary {
            files_changed: 3,
            lines_added: 120,
            lines_removed: 8,
        },
    };
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(
                ui,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    git: Some(&git),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_block_header_git_dirty_and_sync");
}

#[test]
fn snapshot_paint_block_header_with_pr_chip_pending() {
    // 4G-async-context PR chip: the live prompt header shows cwd, branch,
    // dirty, then a `PR #NN` chip colored by CI status (here yellow =
    // pending). This is the prompt-only "now, about to act" surface.
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let git = termica::git_context::GitContext {
        branch: Some("feat/pr-status-chip".to_string()),
        dirty: termica::git_context::DirtySummary {
            files_changed: 1,
            lines_added: 8,
            lines_removed: 0,
        },
        ..Default::default()
    };
    let pr =
        termica::pr_context::PrContext { number: 127, ci: termica::pr_context::CiStatus::Pending };
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(760.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(
                ui,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    git: Some(&git),
                    pr: Some(&pr),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_block_header_pr_chip_pending");
}

#[test]
fn snapshot_paint_block_header_with_pr_chip_passing() {
    // Same surface, CI passing (green chip), clean tree.
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let git =
        termica::git_context::GitContext { branch: Some("main".to_string()), ..Default::default() };
    let pr =
        termica::pr_context::PrContext { number: 124, ci: termica::pr_context::CiStatus::Passing };
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(760.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(
                ui,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    git: Some(&git),
                    pr: Some(&pr),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_block_header_pr_chip_passing");
}

#[test]
fn snapshot_paint_sealed_block_with_header_and_failed_exit() {
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let snapshot = sealed_snapshot(3, 40, b"command not found: blarg\r\n");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 140.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(
                ui,
                "blarg",
                &snapshot,
                None,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    exit: Some(127),
                    duration: Some(std::time::Duration::from_millis(42)),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_sealed_block_with_header_and_failed_exit");
}

#[test]
fn snapshot_paint_sealed_block_with_captured_git() {
    // 4G-async-context capture-at-run-time: a sealed block shows the
    // branch / dirty it ran under, frozen as history — cwd, branch, then
    // the amber dirty chip, then the duration.
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let home = std::path::PathBuf::from("/Users/tim");
    let git = termica::git_context::GitContext {
        branch: Some("feat/git-capture-at-runtime".to_string()),
        ahead: 0,
        behind: 0,
        dirty: termica::git_context::DirtySummary {
            files_changed: 2,
            lines_added: 40,
            lines_removed: 3,
        },
    };
    let snapshot = sealed_snapshot(3, 40, b"build succeeded\r\n");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 120.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(
                ui,
                "cargo build",
                &snapshot,
                None,
                render::BlockHeader {
                    cwd: Some(cwd.as_path()),
                    home: Some(home.as_path()),
                    exit: Some(0),
                    duration: Some(std::time::Duration::from_secs(12)),
                    git: Some(&git),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_sealed_block_with_captured_git");
}

#[test]
fn snapshot_paint_sealed_block_with_selection_spans_command_and_output() {
    // Selection starts at (row 0, col 0) — the command "ls" — and
    // ends at (row 1, col 9). Row 0 is the command label; rows 1+
    // are snapshot output. Highlight should cover both regions.
    use termica::block_selection::BlockCursor;
    let snapshot = sealed_snapshot(4, 40, b"Cargo.toml\r\nREADME.md\r\nsrc\r\n");
    let sel = Some((BlockCursor::new(0, 0), BlockCursor::new(1, 9)));
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 140.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(
                ui,
                "ls",
                &snapshot,
                sel,
                render::BlockHeader {
                    exit: Some(0),
                    duration: Some(std::time::Duration::from_millis(200)),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_sealed_block_with_selection");
}

#[test]
fn snapshot_paint_sealed_block_with_selection_in_command_only() {
    // Selection entirely within the command label (row 0).
    use termica::block_selection::BlockCursor;
    let snapshot = sealed_snapshot(3, 40, b"hello\r\n");
    let sel = Some((BlockCursor::new(0, 5), BlockCursor::new(0, 10)));
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 100.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(
                ui,
                "echo hello",
                &snapshot,
                sel,
                render::BlockHeader {
                    exit: Some(0),
                    duration: Some(std::time::Duration::from_millis(200)),
                    ..Default::default()
                },
            );
        });
    harness.snapshot("paint_sealed_block_with_selection_in_command_only");
}

// ---- Ctrl+R history overlay (Phase 4J PR 6) -----------------------------

/// Fixed `now` used by every overlay snapshot — every entry's
/// `started_at_ms` is expressed as an offset from this so the age
/// formatter renders deterministically. Concrete value chosen to
/// keep readers from squinting at a 13-digit epoch.
const SNAP_NOW_MS: i64 = 1_700_000_000_000;

const SEC: i64 = 1_000;
const MIN: i64 = 60 * SEC;
const HOUR: i64 = 60 * MIN;
const DAY: i64 = 24 * HOUR;

/// Build a synthetic [`HistoryOverlay`] with deterministic entries
/// + initial state. Pure POD construction; no DB, no filesystem.
fn overlay_with_entries(
    query: &str,
    selected: usize,
    scope: termica::history_overlay::OverlayScope,
    entries: Vec<termica::history::Entry>,
) -> termica::history_overlay::HistoryOverlay {
    let mut overlay = termica::history_overlay::HistoryOverlay {
        query: query.to_string(),
        scope,
        selected,
        cached_entries: entries,
        ranked: Vec::new(),
    };
    overlay.rerank(None);
    overlay.selected = selected.min(overlay.ranked.len().saturating_sub(1));
    overlay
}

fn entry(
    text: &str,
    ts: i64,
    cwd: Option<&str>,
    exit_code: Option<i32>,
    source: &str,
) -> termica::history::Entry {
    termica::history::Entry {
        id: ts,
        text: text.to_string(),
        started_at_ms: ts,
        finished_at_ms: None,
        exit_code,
        cwd: cwd.map(|s| s.to_string()),
        app_run_id: None,
        pane_id: None,
        source: source.to_string(),
    }
}

#[test]
fn snapshot_history_overlay_empty_query_shows_all_entries() {
    // Timestamps chosen to exercise every age-formatter branch:
    // now / minutes / hours / yesterday / days / months / years.
    let mut overlay = overlay_with_entries(
        "",
        0,
        termica::history_overlay::OverlayScope::Global,
        vec![
            entry(
                "cargo test --workspace",
                SNAP_NOW_MS - 30 * SEC,
                Some("~/git/enthal/termica"),
                Some(0),
                "termica",
            ),
            entry(
                "git status",
                SNAP_NOW_MS - 4 * MIN,
                Some("~/git/enthal/termica"),
                Some(0),
                "termica",
            ),
            entry("ls -la", SNAP_NOW_MS - 3 * HOUR, None, None, "zsh"),
            entry("cd src", SNAP_NOW_MS - DAY, None, None, "zsh"),
            entry("vim CLAUDE.md", SNAP_NOW_MS - 3 * DAY, None, None, "bash"),
            entry("rustup update", SNAP_NOW_MS - 60 * DAY, None, None, "bash"),
            entry("brew install fish", SNAP_NOW_MS - 400 * DAY, None, None, "bash"),
        ],
    );
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(1100.0, 720.0)).build_ui(move |ui| {
            let _ = termica::history_overlay::paint_overlay(ui, &mut overlay, 1, None, SNAP_NOW_MS);
        });
    harness.snapshot("history_overlay_empty_query");
}

#[test]
fn snapshot_history_overlay_with_filter_typed() {
    // Query "cargo" narrows the list to two entries; the matched
    // substring renders in selection color + underline in each row.
    let mut overlay = overlay_with_entries(
        "cargo",
        0,
        termica::history_overlay::OverlayScope::Global,
        vec![
            entry(
                "cargo test --workspace",
                SNAP_NOW_MS - 2 * MIN,
                Some("~/git/enthal/termica"),
                Some(0),
                "termica",
            ),
            entry("git status", SNAP_NOW_MS - 8 * MIN, None, None, "termica"),
            entry(
                "cargo run --release",
                SNAP_NOW_MS - 45 * MIN,
                Some("~/git/enthal/termica"),
                Some(0),
                "termica",
            ),
            entry("ls", SNAP_NOW_MS - 2 * HOUR, None, None, "zsh"),
        ],
    );
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(1100.0, 720.0)).build_ui(move |ui| {
            let _ = termica::history_overlay::paint_overlay(ui, &mut overlay, 1, None, SNAP_NOW_MS);
        });
    harness.snapshot("history_overlay_with_filter");
}

#[test]
fn snapshot_history_overlay_word_split_and_replayed_and_multiline() {
    // One snapshot covers three behaviors at once so a future
    // reader sees them composed:
    //   - Word-split query "echo that" highlights BOTH words.
    //   - The replayed `zsh` row has `started_at_ms < 0` so no age
    //     column renders — just `zsh`.
    //   - A multi-line command is folded to a single line with
    //     "↲" glyphs replacing the embedded newlines.
    let mut overlay = overlay_with_entries(
        "echo that",
        0,
        termica::history_overlay::OverlayScope::Global,
        vec![
            entry(
                "echo this that the other",
                SNAP_NOW_MS - 3 * MIN,
                Some("~/git/enthal/termica"),
                Some(0),
                "termica",
            ),
            entry("echo that\necho more", SNAP_NOW_MS - 12 * MIN, None, Some(0), "termica"),
            entry("echo something else that exists", -2, None, None, "zsh"),
        ],
    );
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(1100.0, 720.0)).build_ui(move |ui| {
            let _ = termica::history_overlay::paint_overlay(ui, &mut overlay, 1, None, SNAP_NOW_MS);
        });
    harness.snapshot("history_overlay_word_split_replayed_multiline");
}

#[test]
fn snapshot_history_overlay_match_highlight_inside_command() {
    // Pwd-in-PWD case from spec: query "pwd" matches "echo $PWD"
    // case-insensitively. The "PWD" run renders in selection
    // color + underline; the surrounding text stays plain.
    let mut overlay = overlay_with_entries(
        "pwd",
        0,
        termica::history_overlay::OverlayScope::Global,
        vec![
            entry("echo $PWD", SNAP_NOW_MS - 30 * SEC, None, None, "termica"),
            entry("pwd", SNAP_NOW_MS - 5 * MIN, None, None, "termica"),
            entry("echo \"pwd is $PWD\"", SNAP_NOW_MS - 12 * MIN, None, None, "termica"),
        ],
    );
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(1100.0, 720.0)).build_ui(move |ui| {
            let _ = termica::history_overlay::paint_overlay(ui, &mut overlay, 1, None, SNAP_NOW_MS);
        });
    harness.snapshot("history_overlay_match_highlight");
}

#[test]
fn snapshot_history_overlay_selected_second_row() {
    let mut overlay = overlay_with_entries(
        "",
        1,
        termica::history_overlay::OverlayScope::Global,
        vec![
            entry(
                "cargo test --workspace",
                SNAP_NOW_MS - 30 * SEC,
                Some("~/git/enthal/termica"),
                Some(0),
                "termica",
            ),
            entry(
                "git status",
                SNAP_NOW_MS - 4 * MIN,
                Some("~/git/enthal/termica"),
                Some(0),
                "termica",
            ),
            entry("ls -la", SNAP_NOW_MS - 2 * HOUR, None, None, "zsh"),
        ],
    );
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(1100.0, 720.0)).build_ui(move |ui| {
            let _ = termica::history_overlay::paint_overlay(ui, &mut overlay, 1, None, SNAP_NOW_MS);
        });
    harness.snapshot("history_overlay_selected_second_row");
}

#[test]
fn snapshot_history_overlay_pane_scope_no_matches() {
    // Scope = Pane, but the query "xyz" doesn't match anything.
    // Renders the "(no matches)" placeholder + the pane-scope
    // label in the header.
    let mut overlay = overlay_with_entries(
        "xyz",
        0,
        termica::history_overlay::OverlayScope::Pane,
        vec![
            entry("cargo test", SNAP_NOW_MS - 3 * MIN, None, None, "termica"),
            entry("ls", SNAP_NOW_MS - 10 * MIN, None, None, "termica"),
        ],
    );
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(1100.0, 600.0)).build_ui(move |ui| {
            let _ = termica::history_overlay::paint_overlay(ui, &mut overlay, 1, None, SNAP_NOW_MS);
        });
    harness.snapshot("history_overlay_pane_scope_no_matches");
}

// ---- Tab-completion popup (Phase 4I slice 1) --------------------------
//
// The popup paints via `egui::Area` with a LEFT_BOTTOM pivot at the
// editor's top-left, so it grows UPWARD and never occludes the editor.
// These snapshots pin the rendered widget — the data-model logic
// (selection, tab_extend, accept) is unit-tested in `completion::popup`.
// Pointer is never moved in the harness, so `hovered()` is false and the
// selected row is exactly what we set — fully deterministic.

use termica::completion::{CompletionCandidate, CompletionPopup, CompletionSource};

/// Build a popup from `(value, source)` pairs and select `selected`.
fn completion_popup(selected: usize, candidates: &[(&str, CompletionSource)]) -> CompletionPopup {
    let cands: Vec<CompletionCandidate> =
        candidates.iter().map(|(v, s)| CompletionCandidate::simple(*v, *s)).collect();
    let mut popup = CompletionPopup::new(0, "", cands).expect("non-empty candidate list");
    popup.selected_index = selected.min(popup.candidates.len().saturating_sub(1));
    popup
}

/// Paint a completion popup into a snapshot harness. `anchor` is the
/// editor top-left (the popup's bottom-left pivot); the popup grows up
/// from there. `max_rows` mirrors the production value (10) unless a
/// case wants to force scrolling.
fn snapshot_completion_popup(
    name: &str,
    mut popup: CompletionPopup,
    anchor: egui::Pos2,
    max_rows: usize,
    size: egui::Vec2,
) {
    let mut harness = Harness::builder().with_size(size).build_ui(move |ui| {
        let ctx = ui.ctx().clone();
        let _ = termica::completion::popup::paint(&ctx, &mut popup, anchor, 1, max_rows);
    });
    // Drawn ↑/↓ keybind-hint glyphs — tolerate cross-platform AA.
    harness.snapshot_options(name, &drawn_glyph_snapshot_options());
}

#[test]
fn snapshot_completion_popup_first_row_selected() {
    // Command-position completion for "g": $PATH executables plus one
    // history-ranked entry, so all three source tags can't appear here
    // but the common $PATH + history mix does. First row selected (the
    // default on open).
    let popup = completion_popup(
        0,
        &[
            ("git", CompletionSource::PathExecutable),
            ("gh", CompletionSource::PathExecutable),
            ("grep", CompletionSource::PathExecutable),
            ("gradle", CompletionSource::History),
            ("gzip", CompletionSource::PathExecutable),
        ],
    );
    snapshot_completion_popup(
        "completion_popup_first_row_selected",
        popup,
        egui::Pos2::new(24.0, 232.0),
        10,
        egui::Vec2::new(540.0, 260.0),
    );
}

#[test]
fn snapshot_completion_popup_lower_row_selected() {
    // Same list, but the selection has moved down to row 3 ("gradle").
    // The highlight band + selection text color move with it; every
    // other row renders in the default text color.
    let popup = completion_popup(
        3,
        &[
            ("git", CompletionSource::PathExecutable),
            ("gh", CompletionSource::PathExecutable),
            ("grep", CompletionSource::PathExecutable),
            ("gradle", CompletionSource::History),
            ("gzip", CompletionSource::PathExecutable),
        ],
    );
    snapshot_completion_popup(
        "completion_popup_lower_row_selected",
        popup,
        egui::Pos2::new(24.0, 232.0),
        10,
        egui::Vec2::new(540.0, 260.0),
    );
}

#[test]
fn snapshot_completion_popup_narrowed_list() {
    // After typing "gi" the list narrows to two candidates — the popup
    // shrinks to fit (no fixed height) and stays bottom-anchored.
    let popup = completion_popup(
        0,
        &[("git", CompletionSource::PathExecutable), ("git-lfs", CompletionSource::PathExecutable)],
    );
    snapshot_completion_popup(
        "completion_popup_narrowed_list",
        popup,
        egui::Pos2::new(24.0, 232.0),
        10,
        egui::Vec2::new(540.0, 260.0),
    );
}

#[test]
fn snapshot_completion_popup_scrolls_growing_upward_near_bottom() {
    // Path completion for "src/" with more candidates than
    // `max_rows`, anchored near the bottom edge of the surface: the
    // popup scrolls (only `max_rows` rows visible) AND grows upward
    // from the anchor, leaving the editor row (at the anchor) clear.
    let popup = completion_popup(
        0,
        &[
            ("src/block.rs", CompletionSource::Path),
            ("src/completion/", CompletionSource::Path),
            ("src/history_overlay.rs", CompletionSource::Path),
            ("src/input.rs", CompletionSource::Path),
            ("src/lib.rs", CompletionSource::Path),
            ("src/main.rs", CompletionSource::Path),
            ("src/pane.rs", CompletionSource::Path),
            ("src/render.rs", CompletionSource::Path),
            ("src/render_pane.rs", CompletionSource::Path),
            ("src/terminal.rs", CompletionSource::Path),
        ],
    );
    snapshot_completion_popup(
        "completion_popup_scrolls_growing_upward_near_bottom",
        popup,
        egui::Pos2::new(24.0, 408.0),
        6,
        egui::Vec2::new(540.0, 420.0),
    );
}

#[test]
fn snapshot_completion_popup_driver_rows_with_tags_and_descriptions() {
    // Slice 2: CLI-native driver candidates merged with locals. The
    // driver rows carry a `k8s` source tag (right edge) and a one-line
    // description; the trailing `$PATH` local has neither. Pins the new
    // `CompletionSource::Driver` tag + description rendering path.
    use termica::completion::DriverTool;
    let k8s = CompletionSource::Driver(DriverTool::Kubectl);
    let cands = vec![
        CompletionCandidate::with_description(
            "pods",
            "Pods are the smallest deployable units",
            k8s,
        ),
        CompletionCandidate::with_description("podtemplates", "PodTemplate objects", k8s),
        CompletionCandidate::simple("podman", CompletionSource::PathExecutable),
    ];
    let mut popup = CompletionPopup::new(0, "pod", cands).expect("non-empty candidate list");
    popup.selected_index = 0;
    snapshot_completion_popup(
        "completion_popup_driver_rows",
        popup,
        egui::Pos2::new(24.0, 232.0),
        10,
        egui::Vec2::new(540.0, 260.0),
    );
}

#[test]
fn snapshot_completion_popup_tabular_columns() {
    // A tabular completion (e.g. `kubectl get <resource>` via the fish
    // sidecar): each description is space-padded multi-column data. The
    // popup splits on 2+ spaces and paints every column at a shared
    // tab-stop, so the rows line up into a table and the popup widens to
    // fit. Pins the column-alignment layout (`ColumnLayout`).
    // Description cells are `\t`-joined (as the driver parsers emit them);
    // `deviceclasses` has an EMPTY short-name cell (leading `\t`), which the
    // popup must keep aligned in its own column rather than shifting the
    // rest of the row left.
    use termica::completion::DriverTool;
    let fish = CompletionSource::Driver(DriverTool::FishComplete);
    let cands = vec![
        CompletionCandidate::with_description("daemonsets", "ds\tapps/v1\ttrue\tDaemonSet", fish),
        CompletionCandidate::with_description(
            "deployments",
            "deploy\tapps/v1\ttrue\tDeployment",
            fish,
        ),
        CompletionCandidate::with_description(
            "deviceclasses",
            "\tresource.k8s.io/v1\tfalse\tDeviceClass",
            fish,
        ),
    ];
    let mut popup = CompletionPopup::new(0, "d", cands).expect("non-empty candidate list");
    popup.selected_index = 0;
    snapshot_completion_popup(
        "completion_popup_tabular_columns",
        popup,
        egui::Pos2::new(24.0, 232.0),
        10,
        egui::Vec2::new(900.0, 260.0),
    );
}

// ---- Sticky-top block header (4E) -------------------------------------
//
// The sticky-eligibility / paint-position math is unit-tested in
// `render_pane::compute_sticky_header`. These snapshots pin the rendered
// pinned header: an opaque strip occluding the content scrolling under it
// plus the cwd / exit chips, clipped to the viewport so a pushed-up header
// slides under the top edge.

/// Paint simulated scrolled output rows so the sticky strip has
/// something to occlude in the snapshot.
fn paint_fake_output_rows(ui: &egui::Ui, size: egui::Vec2) {
    let painter = ui.painter().clone();
    let font = egui::FontId::monospace(14.0);
    let rows = (size.y / 20.0) as usize;
    for i in 0..rows {
        painter.text(
            egui::pos2(8.0, 4.0 + i as f32 * 20.0),
            egui::Align2::LEFT_TOP,
            format!("output line {i:02} scrolling under the pinned header"),
            font.clone(),
            egui::Color32::from_gray(0xA0),
        );
    }
}

#[test]
fn snapshot_sticky_header_pinned_at_top() {
    // Header flush at the viewport top, occluding the output rows
    // behind it: cwd chip + "exit 1" chip + the command label (teal)
    // that identifies the block.
    let size = egui::Vec2::new(560.0, 220.0);
    let mut harness = Harness::builder().with_size(size).build_ui(move |ui| {
        paint_fake_output_rows(ui, size);
        let ctx = ui.ctx().clone();
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        termica::paint_sticky_header(
            &ctx,
            viewport,
            0.0,
            termica::StickyHeaderContent {
                cwd: Some(std::path::Path::new("/Users/tim/git/enthal/termica")),
                home: Some(std::path::Path::new("/Users/tim")),
                exit: Some(1),
                duration: Some(std::time::Duration::from_millis(123)),
                command: "ls -la --color=always",
                ..Default::default()
            },
            1,
        );
    });
    harness.snapshot("sticky_header_pinned_at_top");
}

#[test]
fn snapshot_sticky_header_multiline_command_capped() {
    // A multiline command shows up to 4 lines; a 6-line command is
    // capped to 4 with a "…" truncation hint on the last shown line.
    let size = egui::Vec2::new(560.0, 260.0);
    let mut harness = Harness::builder().with_size(size).build_ui(move |ui| {
        paint_fake_output_rows(ui, size);
        let ctx = ui.ctx().clone();
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        termica::paint_sticky_header(
            &ctx,
            viewport,
            0.0,
            termica::StickyHeaderContent {
                cwd: Some(std::path::Path::new("/Users/tim/git/enthal/termica")),
                home: Some(std::path::Path::new("/Users/tim")),
                exit: Some(0),
                duration: Some(std::time::Duration::from_secs(125)),
                command: "for f in *.rs; do\n  echo \"$f\"\n  wc -l \"$f\"\n  head -1 \"$f\"\n  tail -1 \"$f\"\ndone",
                ..Default::default()
            },
            1,
        );
    });
    harness.snapshot("sticky_header_multiline_command_capped");
}

#[test]
fn snapshot_sticky_header_pushed_up_clips_at_top() {
    // The next block has pushed the pinned header up: paint_y is above
    // the viewport top, so only the lower part shows (clipped at the
    // top edge), sliding off as the next header rises.
    let size = egui::Vec2::new(560.0, 220.0);
    let mut harness = Harness::builder().with_size(size).build_ui(move |ui| {
        paint_fake_output_rows(ui, size);
        let ctx = ui.ctx().clone();
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        termica::paint_sticky_header(
            &ctx,
            viewport,
            -12.0,
            termica::StickyHeaderContent {
                cwd: Some(std::path::Path::new("/Users/tim/git/enthal/termica")),
                home: Some(std::path::Path::new("/Users/tim")),
                duration: Some(std::time::Duration::from_millis(34)),
                command: "cargo test --workspace",
                ..Default::default()
            },
            1,
        );
    });
    harness.snapshot("sticky_header_pushed_up");
}

/// Regression: the pinned command label must be **interactive**
/// (`click_and_drag`), not hover-only. A hover-only overlay let presses
/// fall through to the output scrolling underneath, so double-clicking
/// the pinned command selected the wrong text (it was not selectable).
/// The returned `Response` is what the pane routes into a selection of
/// the pinned block, so it must sense both click and drag.
#[test]
fn sticky_header_command_label_is_selectable() {
    let size = egui::Vec2::new(560.0, 220.0);
    let mut command_sense = None;
    let _ = Harness::builder().with_size(size).build_ui(|ui| {
        let ctx = ui.ctx().clone();
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        let sticky = termica::paint_sticky_header(
            &ctx,
            viewport,
            0.0,
            termica::StickyHeaderContent {
                cwd: Some(std::path::Path::new("/Users/tim/git/enthal/termica")),
                home: Some(std::path::Path::new("/Users/tim")),
                exit: Some(0),
                duration: Some(std::time::Duration::from_millis(123)),
                command: "git commit -m \"fix pinned header\"",
                ..Default::default()
            },
            1,
        );
        command_sense = sticky.command.map(|r| r.sense);
    });
    let sense = command_sense.expect("pinned header has a command label");
    assert!(sense.senses_click(), "pinned command label must sense clicks");
    assert!(sense.senses_drag(), "pinned command label must sense drags (for drag-select)");
}

/// The pinned command label paints the block's selection highlight so a
/// selection started on the pinned copy is visible there, in sync with
/// the inline block. Snapshot guards the highlighted-glyph rendering.
#[test]
fn snapshot_sticky_header_with_command_selection() {
    let size = egui::Vec2::new(560.0, 220.0);
    let mut harness = Harness::builder().with_size(size).build_ui(move |ui| {
        paint_fake_output_rows(ui, size);
        let ctx = ui.ctx().clone();
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        // Select "commit" (cols 4..10) on the single command row.
        let sel = Some((
            termica::block_selection::BlockCursor::new(0, 4),
            termica::block_selection::BlockCursor::new(0, 10),
        ));
        termica::paint_sticky_header(
            &ctx,
            viewport,
            0.0,
            termica::StickyHeaderContent {
                cwd: Some(std::path::Path::new("/Users/tim/git/enthal/termica")),
                home: Some(std::path::Path::new("/Users/tim")),
                exit: Some(0),
                duration: Some(std::time::Duration::from_millis(123)),
                command: "git commit -m wip",
                command_selection: sel,
                ..Default::default()
            },
            1,
        );
    });
    harness.snapshot("sticky_header_with_command_selection");
}

#[test]
fn snapshot_watermark_centered_in_narrow_pane() {
    // Regression guard for the blank-pane watermark centering: the logo
    // must sit in the horizontal + vertical center of the rect it is
    // given and stay fully inside it, even in a narrow pane. The live
    // bug centered on the pane_ui's egui_tiles-inflated `max_rect`
    // instead of its `clip_rect`, shoving the logo right and letting
    // the clip chop it. Alpha is bumped above the product default so
    // the baseline is clearly reviewable by eye per the CLAUDE.md
    // snapshot-review step.
    use termica::watermark::{WatermarkSettings, paint};
    let settings =
        WatermarkSettings { enabled: true, alpha: 140, size_frac: 0.5, grayscale: false };
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(280.0, 680.0)).build_ui(move |ui| {
            // Opaque dark fill like the terminal, then the watermark
            // centered on the ui's clip rect (the true pane bounds).
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(10));
            paint(ui.ctx(), ui.painter(), ui.clip_rect(), settings, 1.0);
        });
    harness.snapshot("watermark_narrow_pane");
}

// ---- Phase 8: in-pane find overlay + match highlights --------------------

/// Build a synthetic [`termica::find::FindOverlay`] with a fixed query,
/// toggles, and a given number of matches so the count chip ("N of M")
/// renders deterministically without a real block stack.
fn find_overlay(
    query: &str,
    n_matches: usize,
    selected: usize,
    case_sensitive: bool,
    regex: bool,
    filter: termica::find::SearchFilter,
) -> termica::find::FindOverlay {
    use termica::block::BlockId;
    use termica::find::{FindOverlay, LineKind, SearchMatch};
    let mut o = FindOverlay::open(vec![]);
    o.query = query.to_string();
    o.case_sensitive = case_sensitive;
    o.regex = regex;
    o.filter = filter;
    o.selected = selected;
    o.matches = (0..n_matches)
        .map(|i| SearchMatch {
            block_id: BlockId(0),
            kind: LineKind::Output,
            row: i,
            col_start: 0,
            col_end: query.chars().count(),
        })
        .collect();
    o
}

#[test]
fn snapshot_find_overlay_with_matches() {
    let mut overlay = find_overlay("error", 14, 2, false, false, termica::find::SearchFilter::Both);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(900.0, 200.0)).build_ui(move |ui| {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 200.0));
            let _ = termica::find::paint_overlay(ui, &mut overlay, 1, rect);
        });
    harness.snapshot_options("find_overlay_with_matches", &drawn_glyph_snapshot_options());
}

#[test]
fn snapshot_find_overlay_commands_filter_case_sensitive() {
    // `Commands` filter + `Aa` (match case) both engaged so the toggle
    // chips show their active state.
    let mut overlay =
        find_overlay("Cargo", 3, 0, true, false, termica::find::SearchFilter::CommandOnly);
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(900.0, 200.0)).build_ui(move |ui| {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 200.0));
            let _ = termica::find::paint_overlay(ui, &mut overlay, 1, rect);
        });
    harness.snapshot_options(
        "find_overlay_commands_filter_case_sensitive",
        &drawn_glyph_snapshot_options(),
    );
}

#[test]
fn snapshot_find_overlay_regex_error() {
    let mut overlay =
        find_overlay("(unclosed", 0, 0, false, true, termica::find::SearchFilter::Both);
    overlay.regex_error = true;
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(900.0, 200.0)).build_ui(move |ui| {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 200.0));
            let _ = termica::find::paint_overlay(ui, &mut overlay, 1, rect);
        });
    harness.snapshot_options("find_overlay_regex_error", &drawn_glyph_snapshot_options());
}

#[test]
fn snapshot_find_overlay_history_dropdown() {
    let mut overlay = find_overlay("err", 5, 0, false, false, termica::find::SearchFilter::Both);
    overlay.history = vec!["error".to_string(), "cargo test".to_string(), "TODO".to_string()];
    overlay.dropdown_open = true;
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(900.0, 360.0)).build_ui(move |ui| {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 360.0));
            let _ = termica::find::paint_overlay(ui, &mut overlay, 1, rect);
        });
    harness.snapshot_options("find_overlay_history_dropdown", &drawn_glyph_snapshot_options());
}

#[test]
fn snapshot_find_match_highlights_over_snapshot() {
    // Paint a sealed snapshot, then lay find highlights over it: two
    // plain matches + one "current" match (brighter), to lock in the
    // highlighter-over-glyphs look.
    let lines = sealed_snapshot(4, 40, b"error: build failed\r\nwarning: error here\r\nall good");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 180.0)).build_ui(move |ui| {
            let resp = render::paint_styled_lines(ui, &lines, None);
            let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
            let (cell_w, row_h) =
                ui.fonts_mut(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
            // "error" at row0 col0 (current), row1 col9, and "all" row2.
            let ranges = [(0usize, 0usize, 5usize, true), (1, 9, 14, false), (2, 0, 3, false)];
            render::paint_match_highlights(ui.painter(), resp.rect.min, cell_w, row_h, &ranges);
        });
    harness.snapshot("find_match_highlights");
}

// ---- keybindings cheat-sheet (Cmd+/) -------------------------------------

#[test]
fn snapshot_keybindings_macos() {
    let mut q = String::new();
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(720.0, 760.0)).build_ui(move |ui| {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(720.0, 760.0));
            let _ = termica::keybindings::paint(ui, 1, rect, true, &mut q);
        });
    harness.snapshot_options("keybindings_macos", &drawn_glyph_snapshot_options());
}

#[test]
fn snapshot_keybindings_linux() {
    let mut q = String::new();
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(720.0, 760.0)).build_ui(move |ui| {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(720.0, 760.0));
            let _ = termica::keybindings::paint(ui, 1, rect, false, &mut q);
        });
    harness.snapshot_options("keybindings_linux", &drawn_glyph_snapshot_options());
}

#[test]
fn snapshot_keybindings_filtered() {
    // The search field filters to matching rows (here "tab").
    let mut q = "tab".to_string();
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(720.0, 760.0)).build_ui(move |ui| {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(720.0, 760.0));
            let _ = termica::keybindings::paint(ui, 1, rect, true, &mut q);
        });
    harness.snapshot_options("keybindings_filtered", &drawn_glyph_snapshot_options());
}
