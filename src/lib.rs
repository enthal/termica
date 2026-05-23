//! Termica library entry point.
//!
//! Phase 1E-d: the eframe app now resizes its PTY + `TerminalState`
//! to match the window's available cell grid. Dragging the window
//! tells the shell about the new size on the next frame; vim / less
//! / htop redraw themselves accordingly.
//!
//! See [`SPEC.md`](../SPEC.md) and [`spec/01-architecture.md`](../spec/01-architecture.md)
//! for the layered architecture this crate grows into.

#![forbid(unsafe_code)]

pub mod input;
pub mod osc;
pub mod pane;
pub mod pty;
pub mod render;
pub mod terminal;

use eframe::egui;

use pane::{PaneSession, PaneView};
use pty::PtyConfig;

/// Minimum cell grid Termica will ever ask a PTY for. Below this,
/// shells and full-screen TTY programs behave erratically. The
/// window's `min_inner_size` is also clamped so a user can't drag
/// below the equivalent cells.
const MIN_ROWS: u16 = 5;
const MIN_COLS: u16 = 20;

/// Render the workspace's status header into `ui`.
///
/// Pure UI: takes a plain [`PaneView`] snapshot, never touches OS
/// resources, safe to drive from `egui_kittest` snapshot tests.
/// The cell grid is painted separately by [`render::paint_terminal`]
/// directly below this header.
pub fn central_panel(ui: &mut egui::Ui, view: &PaneView) {
    ui.heading("Termica");
    ui.label(format!(
        "Phase 1E-h — OSC 7 cwd live. \
         Bytes: {}   ·   alt-screen: {}   ·   grid: {}×{}",
        view.bytes_received, view.alt_screen, view.rows, view.cols
    ));
    if let Some(cwd) = &view.cwd {
        // Whole path on a separate line so we don't have to fight
        // for status-bar width. Phase 5 will turn this into a proper
        // clickable chip with a `Painter`-drawn icon (CLAUDE.md
        // forbids Unicode-font icons — they render as tofu on Linux).
        ui.label(format!("cwd: {}", cwd.display()));
    }
    ui.separator();
}

/// Compute how many `(rows, cols)` fit into `avail` at the given cell
/// metrics, clamped to [`MIN_ROWS`] × [`MIN_COLS`].
///
/// Pure function so the rounding / clamp policy is unit-testable
/// without an egui context.
pub fn cells_from_pixels(avail: egui::Vec2, cell_w: f32, row_h: f32) -> (u16, u16) {
    // `floor` so we never advertise more cells than physically fit.
    // The `max(MIN_*)` clamps protect against pathological tiny
    // windows during initial layout, where `avail` may be (0, 0) for
    // one frame.
    let cols = if cell_w > 0.0 { (avail.x / cell_w).floor().max(0.0) as u16 } else { MIN_COLS };
    let rows = if row_h > 0.0 { (avail.y / row_h).floor().max(0.0) as u16 } else { MIN_ROWS };
    (rows.max(MIN_ROWS), cols.max(MIN_COLS))
}

/// The top-level eframe application.
///
/// Holds one [`PaneSession`] for now (multi-pane workspace lives in
/// [Phase 2 (#2)](https://github.com/enthal/termica/issues/2)). On
/// spawn failure the pane is `Err` and the UI renders an error
/// banner — the app stays alive so the user can see what went wrong
/// instead of silently exiting.
pub struct TermicaApp {
    pane: Result<PaneSession, String>,
    /// Last `(rows, cols)` we told the PTY about. Caching this means
    /// we only call `PaneSession::resize` when the cell-grid size
    /// actually changes, not every frame.
    last_size: Option<(u16, u16)>,
}

impl TermicaApp {
    /// Construct an app with a freshly spawned shell pane sized to a
    /// reasonable starting default. The PTY/grid will resize to the
    /// real window dimensions on the first `update` pass.
    pub fn new() -> Self {
        let config = PtyConfig::default();
        let pane = PaneSession::spawn(MIN_ROWS.max(24), MIN_COLS.max(80), &config)
            .map_err(|e| format!("{e}"));
        Self { pane, last_size: None }
    }
}

impl Default for TermicaApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for TermicaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Liveness: ask for a redraw soon so PTY output keeps flowing
        // even when the user is idle. (~ every 50 ms is more than
        // enough for the current debug visuals.)
        ctx.request_repaint_after(std::time::Duration::from_millis(50));

        // ---- input: read every event, encode, send to the PTY -------
        //
        // Multi-pane focus arrives with Phase 2 (#2). Until then,
        // there's exactly one pane and no other input consumers, so
        // pulling from `ctx.input` is correct.
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());
        if let Ok(pane) = &mut self.pane {
            // Snapshot the VT mode flags once per frame so the encoder
            // can pick CSI vs SS3 for arrow keys etc.
            let modes = pane.terminal().modes();
            for event in &events {
                if let Some(bytes) = input::encode_event(event, modes) {
                    // Best-effort: a write failure means the PTY went
                    // away (child exit or master closed). The reader
                    // thread will hit EOF next and the pane mode will
                    // become `Dead` in Phase 3. For now, swallow.
                    let _ = pane.write(&bytes);
                }
            }
        }

        // ---- output: drain the reader queue into the terminal -------
        if let Ok(pane) = &mut self.pane {
            pane.drain();
        }

        // ---- mouse wheel ---------------------------------------------
        //
        // In the main screen we scroll our local scrollback. In the
        // alternate screen (vim / less / htop / fzf) there is no
        // scrollback of our own; instead we forward the wheel as N
        // arrow keystrokes so those programs receive their native
        // scroll commands. `input::classify_wheel` picks the right
        // branch.
        //
        // Three lines per ~50 points keeps the feel close to
        // Alacritty / iTerm2's default. egui's positive `delta.y` is
        // up; alacritty's positive `Scroll::Delta` is also up.
        let scroll_delta_y = ctx.input(|i| i.smooth_scroll_delta.y);
        if scroll_delta_y.abs() > 0.0
            && let Ok(pane) = &mut self.pane
        {
            let lines = (scroll_delta_y / 50.0 * 3.0).round() as i32;
            let alt_screen = pane.terminal().is_alternate_screen();
            let modes = pane.terminal().modes();
            match input::classify_wheel(lines, alt_screen, modes) {
                Some(input::WheelOutcome::ScrollDisplay(lines)) => {
                    pane.terminal_mut().scroll_display(lines);
                }
                Some(input::WheelOutcome::SendBytes(bytes)) => {
                    let _ = pane.write(&bytes);
                }
                None => {}
            }
        }
        let view = match &self.pane {
            Ok(pane) => pane.view(),
            Err(_) => PaneView::default(),
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Err(err) = &self.pane {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 80, 80),
                    format!("Failed to spawn PTY session: {err}"),
                );
                ui.separator();
            }
            central_panel(ui, &view);

            // ---- resize: compute target cell grid from the space
            // remaining inside this panel, and tell the PTY if it
            // changed.
            let avail = ui.available_size();
            let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
            let (cell_w, row_h) =
                ui.fonts_mut(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
            let (rows, cols) = cells_from_pixels(avail, cell_w, row_h);

            if let Ok(pane) = &mut self.pane
                && self.last_size != Some((rows, cols))
            {
                let _ = pane.resize(rows, cols);
                self.last_size = Some((rows, cols));
            }

            // Cell-grid renderer. Painted right below the status
            // header so the on-screen pane reads top-to-bottom.
            if let Ok(pane) = &self.pane {
                render::paint_terminal(ui, pane.terminal());
            }
        });
    }
}

/// Run the native window. Used by `main` and any future end-to-end harness.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([400.0, 200.0])
            .with_title("Termica"),
        ..Default::default()
    };
    eframe::run_native("termica", options, Box::new(|_cc| Ok(Box::new(TermicaApp::new()))))
}

#[cfg(test)]
mod tests {
    //! Pure-logic tests for the resize math. The wiring itself is
    //! exercised by the snapshot tests in `tests/snapshots.rs`.

    use super::*;

    #[test]
    fn cells_from_pixels_floors_dimensions() {
        // A space of 800×400 at 10×20 cell metrics fits exactly
        // 80×20 cells.
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(800.0, 400.0), 10.0, 20.0);
        assert_eq!((rows, cols), (20, 80));
    }

    #[test]
    fn cells_from_pixels_ignores_fractional_remainder() {
        // 805×405 still only fits 80×20.
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(805.0, 405.0), 10.0, 20.0);
        assert_eq!((rows, cols), (20, 80));
    }

    #[test]
    fn cells_from_pixels_clamps_to_minimum() {
        // A tiny / zero rect during initial layout must not produce
        // a 0×0 PTY size.
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(1.0, 1.0), 10.0, 20.0);
        assert_eq!((rows, cols), (MIN_ROWS, MIN_COLS));
    }

    #[test]
    fn cells_from_pixels_handles_zero_metrics() {
        // Defensive: a misconfigured font setup could report 0 width
        // or height. We must not divide by zero; we fall back to the
        // minimum.
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(800.0, 400.0), 0.0, 20.0);
        assert_eq!(cols, MIN_COLS);
        let (rows2, _cols2) = cells_from_pixels(egui::Vec2::new(800.0, 400.0), 10.0, 0.0);
        assert_eq!(rows2, MIN_ROWS);
        let _ = rows;
    }
}
