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
use egui_kittest::Harness;
use termica::render;
use termica::terminal::TerminalState;

/// Build a sealed-block snapshot from synthetic bytes: feed bytes
/// into a fresh `TerminalState`, then take a `snapshot_lines_all()`.
/// This is the exact path `BlockStack::seal_running` walks at
/// `CommandFinished` time, so the snapshot test paints what a real
/// sealed block would.
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
            render::paint_terminal(ui, &term, None, None, false);
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
            render::paint_terminal(ui, &term, None, None, false);
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
            render::paint_terminal(ui, &term, None, None, false);
        });
    harness.snapshot("terminal_cursor_visible");
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
            render::paint_terminal(ui, &term, None, None, false);
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
            render::paint_terminal(ui, &term, None, None, false);
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
            render::paint_terminal(ui, &term, None, None, false);
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
            render::paint_terminal(ui, &term, None, None, false);
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
            let rendered = render::paint_terminal(ui, &term, None, None, false);
            termica::paint_alt_screen_border(ui.painter(), rendered.response.rect);
        });
    harness.snapshot("terminal_alt_screen_with_border");
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
            render::paint_terminal(ui, &term, None, Some(&link), false);
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

// ---- whole-block snapshots (command + output together) -------------------

#[test]
fn snapshot_paint_sealed_block_echo() {
    // The simplest end-to-end shape of a finished command: type
    // `echo hello`, kernel runs it, snapshot freezes the output.
    let snapshot = sealed_snapshot(3, 40, b"hello\r\n");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 100.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(ui, "echo hello", &snapshot, None, None, Some(0));
        });
    harness.snapshot("paint_sealed_block_echo");
}

#[test]
fn snapshot_paint_sealed_block_ls_output() {
    // Multiple output rows under a single command label. Mirrors
    // what the user typically sees after `ls`.
    let snapshot = sealed_snapshot(6, 40, b"Cargo.toml\r\nREADME.md\r\nsrc\r\ntests\r\n");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 160.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(ui, "ls", &snapshot, None, None, Some(0));
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
                None,
                Some(0),
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
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(ui, Some(cwd.as_path()), None);
        });
    harness.snapshot("paint_block_header_cwd_only");
}

#[test]
fn snapshot_paint_block_header_zero_exit_hides_exit_chip() {
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(ui, Some(cwd.as_path()), Some(0));
        });
    harness.snapshot("paint_block_header_zero_exit");
}

#[test]
fn snapshot_paint_block_header_nonzero_exit_shows_red_annotation() {
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 40.0)).build_ui(move |ui| {
            let _ = render::paint_block_header(ui, Some(cwd.as_path()), Some(127));
        });
    harness.snapshot("paint_block_header_nonzero_exit");
}

#[test]
fn snapshot_paint_sealed_block_with_header_and_failed_exit() {
    let cwd = std::path::PathBuf::from("/Users/tim/git/enthal/termica");
    let snapshot = sealed_snapshot(3, 40, b"command not found: blarg\r\n");
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(700.0, 140.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(
                ui,
                "blarg",
                &snapshot,
                None,
                Some(cwd.as_path()),
                Some(127),
            );
        });
    harness.snapshot("paint_sealed_block_with_header_and_failed_exit");
}

#[test]
fn snapshot_paint_sealed_block_with_selection_inside_output() {
    use termica::block_selection::BlockCursor;
    let snapshot = sealed_snapshot(4, 40, b"Cargo.toml\r\nREADME.md\r\nsrc\r\n");
    let sel = Some((BlockCursor::new(0, 0), BlockCursor::new(1, 9)));
    let mut harness =
        Harness::builder().with_size(egui::Vec2::new(500.0, 140.0)).build_ui(move |ui| {
            let _ = render::paint_sealed_block(ui, "ls", &snapshot, sel, None, Some(0));
        });
    harness.snapshot("paint_sealed_block_with_selection");
}
