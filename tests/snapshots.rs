//! UI snapshot tests via `egui_kittest`.
//!
//! Two groups:
//! 1. Status-header tests (`snapshot_central_panel_*`) drive
//!    `central_panel(ui, &PaneView)` with fixed [`PaneView`]
//!    fixtures. No grid, no PTY, no OS handles.
//! 2. Cell-renderer tests (`snapshot_terminal_*`) construct a real
//!    [`TerminalState`], feed it synthetic bytes, and snapshot the
//!    rendered grid through `render::paint_terminal`.
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
use termica::pane::PaneView;
use termica::render;
use termica::terminal::TerminalState;

// ---- status header fixtures ----------------------------------------------

fn empty_view() -> PaneView {
    PaneView::default()
}

fn typical_view() -> PaneView {
    PaneView { bytes_received: 84, alt_screen: false, ..PaneView::default() }
}

#[test]
fn snapshot_central_panel_empty() {
    let view = empty_view();
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(800.0, 120.0))
        .build_ui(move |ui| termica::central_panel(ui, &view));
    harness.snapshot("central_panel_empty");
}

#[test]
fn snapshot_central_panel_with_typical_output() {
    let view = typical_view();
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(800.0, 120.0))
        .build_ui(move |ui| termica::central_panel(ui, &view));
    harness.snapshot("central_panel_typical");
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
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(700.0, 200.0))
        .build_ui(move |ui| render::paint_terminal(ui, &term));
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
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(700.0, 180.0))
        .build_ui(move |ui| render::paint_terminal(ui, &term));
    harness.snapshot("terminal_ansi_colors");
}

#[test]
fn snapshot_terminal_cursor_visible_at_end_of_text() {
    // Feed some text; the alacritty cursor advances cell-by-cell with
    // each character, so it ends up immediately after "hi" on row 0.
    // The renderer must paint the cursor block there.
    let term = term_from_bytes(4, 30, b"hi");
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(700.0, 160.0))
        .build_ui(move |ui| render::paint_terminal(ui, &term));
    harness.snapshot("terminal_cursor_visible");
}

#[test]
fn snapshot_terminal_cursor_hidden_via_dectcem() {
    // `\e[?25l` hides the cursor (DECTCEM low). The renderer must
    // NOT draw the cursor block when this is set — programs use this
    // to indicate "I'm in the middle of a repaint, don't show me
    // until I'm done".
    let term = term_from_bytes(4, 30, b"hi\x1b[?25l");
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(700.0, 160.0))
        .build_ui(move |ui| render::paint_terminal(ui, &term));
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
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(700.0, 180.0))
        .build_ui(move |ui| render::paint_terminal(ui, &term));
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
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(700.0, 180.0))
        .build_ui(move |ui| render::paint_terminal(ui, &term));
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
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(700.0, 180.0))
        .build_ui(move |ui| render::paint_terminal(ui, &term));
    harness.snapshot("terminal_alt_screen");
}
