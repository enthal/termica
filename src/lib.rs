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
pub mod integration;
pub mod links;
pub mod osc;
pub mod pane;
pub mod pty;
pub mod render;
pub mod selection;
pub mod terminal;

use alacritty_terminal::grid::Dimensions;
use eframe::egui;

use links::LinkSpan;
use pane::{PaneSession, PaneView};
use pty::PtyConfig;
use selection::{SelectionMode, pixel_to_grid_point};

/// Minimum cell grid Termica will ever ask a PTY for. Below this,
/// shells and full-screen TTY programs behave erratically. The
/// window's `min_inner_size` is also clamped so a user can't drag
/// below the equivalent cells.
const MIN_ROWS: u16 = 5;
const MIN_COLS: u16 = 20;

/// Max gap (seconds) between mousedowns to register as a multi-click.
/// Tuned to feel close to the OS default — anything quicker than this
/// counts as a continuation of the previous click.
const MULTI_CLICK_WINDOW_SECS: f64 = 0.5;

/// Max pixel distance the pointer can drift between mousedowns and
/// still register as a multi-click. Tighter than a drag threshold so
/// a slow "click, slight nudge, click" doesn't accidentally trigger
/// word selection.
const MULTI_CLICK_DISTANCE_PX: f32 = 8.0;

/// Spawn the OS's "open this URL" handler.
///
/// macOS: `open <url>`. Linux/BSD: `xdg-open <url>`. Windows would
/// use `cmd /c start <url>`, but we don't run there yet. The URL is
/// passed as a single `arg`, never interpolated into a shell string,
/// so PTY-controlled content cannot inject extra arguments.
///
/// Best-effort: a failure (e.g. `xdg-open` not installed) is logged
/// but otherwise silent. The grid keeps working.
fn open_url(url: &str) {
    let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if let Err(e) = std::process::Command::new(cmd).arg(url).spawn() {
        eprintln!("termica: failed to open {url:?}: {e}");
    }
}

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
    /// egui input-time of the most recent mousedown inside the grid.
    /// Combined with [`Self::last_press_pos`] this lets us turn a
    /// stream of presses into a 1/2/3 click counter — the OS-level
    /// double/triple-click event only fires at click *completion*,
    /// which is too late for "double-click then drag" selection.
    last_press_time: f64,
    /// Pointer position of the most recent mousedown.
    last_press_pos: egui::Pos2,
    /// How many consecutive presses we've accumulated within the
    /// multi-click window. Caps at 3.
    click_count: u8,
}

impl TermicaApp {
    /// Construct an app with a freshly spawned shell pane sized to a
    /// reasonable starting default. The PTY/grid will resize to the
    /// real window dimensions on the first `update` pass.
    pub fn new() -> Self {
        let config = PtyConfig::default();
        let pane = PaneSession::spawn(MIN_ROWS.max(24), MIN_COLS.max(80), &config)
            .map_err(|e| format!("{e}"));
        Self {
            pane,
            last_size: None,
            last_press_time: f64::NEG_INFINITY,
            last_press_pos: egui::Pos2::ZERO,
            click_count: 0,
        }
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
        //
        // Copy-to-clipboard is platform-conditional:
        //
        //   - macOS:   we trust egui's high-level `Event::Copy`
        //              (synthesised from Cmd+C). The raw Key event
        //              isn't reliably emitted alongside Event::Copy
        //              on macOS — that was the original Cmd+C bug.
        //   - Linux /  egui *would* also emit `Event::Copy` on plain
        //     Windows: Ctrl+C, but Ctrl+C must remain SIGINT for the
        //              shell. So we ignore Event::Copy off-Mac and
        //              instead match the Ctrl+Shift+C Key event,
        //              which is the universal terminal convention.
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());
        let is_macos = cfg!(target_os = "macos");

        let copy_pressed = events.iter().any(|e| match e {
            egui::Event::Copy => is_macos,
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                !is_macos && input::is_copy_shortcut(*key, *modifiers, false)
            }
            _ => false,
        });
        if copy_pressed
            && let Ok(pane) = &mut self.pane
            && let Some(text) = pane.selection_text()
        {
            ctx.copy_text(text);
            // Leave the highlight visible so the user sees what was
            // copied — many terminals do this. The next click in
            // the grid will replace the selection.
        }

        if let Ok(pane) = &mut self.pane {
            // Snapshot the VT mode flags once per frame so the encoder
            // can pick CSI vs SS3 for arrow keys etc.
            let modes = pane.terminal().modes();
            for event in &events {
                // Skip the off-Mac Ctrl+Shift+C key event so the
                // encoder never sees it — without Shift held the
                // encoder would have produced 0x03 (SIGINT), but
                // Shift kills that branch anyway. Belt and braces.
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = event
                    && !is_macos
                    && input::is_copy_shortcut(*key, *modifiers, false)
                {
                    continue;
                }
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
            //
            // The paint call returns the bounding rect + cell metrics
            // + display offset that the mouse code below needs to
            // translate pixel positions into grid `Point`s.
            if let Ok(pane) = &mut self.pane {
                let selection = pane.selection().copied();

                // ---- clickable URLs --------------------------------
                //
                // We rescan the visible viewport for URLs every frame
                // and look up the link under the pointer. Cost is
                // negligible at terminal sizes (a few KB of chars
                // per scan) and rescanning is simpler than tracking
                // incremental grid changes — the viewport scrolls
                // and reflows for many reasons.
                //
                // To paint the underline in the SAME frame as the
                // hover (no one-frame lag), we pre-compute the
                // geometry that `paint_terminal` will use, do the
                // hit-test against it, and pass the result in.
                let display_offset = pane.terminal().display_offset() as i32;
                let grid_rows = pane.terminal().grid().screen_lines();
                let grid_cols = pane.terminal().grid().columns();
                let origin = ui.next_widget_position();
                let geom = selection::GridGeometry {
                    origin_x: origin.x,
                    origin_y: origin.y,
                    cell_w,
                    row_h,
                    display_offset,
                    screen_lines: grid_rows,
                    cols: grid_cols,
                };

                let links_in_view = links::scan_visible_links(pane.terminal().grid());
                let modifier_held = ctx.input(|i| i.modifiers.command);
                let pointer_pos = ctx.input(|i| i.pointer.latest_pos());

                let hover_link: Option<LinkSpan> = pointer_pos.and_then(|pos| {
                    let in_grid = pos.x >= origin.x
                        && pos.x < origin.x + grid_cols as f32 * cell_w
                        && pos.y >= origin.y
                        && pos.y < origin.y + grid_rows as f32 * row_h;
                    if !in_grid {
                        return None;
                    }
                    let pt = pixel_to_grid_point(pos.x, pos.y, geom);
                    links_in_view.iter().find(|l| l.contains(pt)).cloned()
                });
                let highlighted_link = if modifier_held { hover_link.as_ref() } else { None };

                let rendered = render::paint_terminal(
                    ui,
                    pane.terminal(),
                    selection.as_ref(),
                    highlighted_link,
                );

                // Pointing-hand cursor matches the underline: same
                // gate so the visual and tactile cues stay in sync.
                if highlighted_link.is_some() {
                    ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                }

                // ---- mouse selection ---------------------------------
                //
                // We drive selection off `primary_pressed` (rising edge
                // of the mousedown) rather than `drag_started`, because
                // the multi-click counter has to fire at the press —
                // by drag-time the user may already be a few cells in,
                // and `double_clicked` / `triple_clicked` don't fire
                // until click *completion*, which is too late.
                //
                // Each press picks a [`SelectionMode`]:
                //   - 1 press   → Char (drag selects cell-by-cell)
                //   - 2 presses → Word (anchor + head snap to word bounds)
                //   - 3 presses → Line (each end snaps to whole-row)
                // The mode is preserved across the drag, so a
                // double-click+drag continues word-by-word and a
                // triple-click+drag continues line-by-line.
                let geom = rendered.geometry;
                let to_point = |pos: egui::Pos2| pixel_to_grid_point(pos.x, pos.y, geom);

                let (primary_pressed, press_origin, now) =
                    ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.press_origin(), i.time));

                if primary_pressed
                    && let Some(pos) = press_origin
                    && rendered.response.rect.contains(pos)
                {
                    let press_pt = to_point(pos);
                    let link_under_press =
                        links_in_view.iter().find(|l| l.contains(press_pt)).cloned();

                    if modifier_held && let Some(link) = link_under_press {
                        // Cmd/Ctrl-click on a URL: open it, do NOT
                        // start a selection. The user expects the
                        // grid to feel unchanged after the click.
                        open_url(&link.url);
                    } else {
                        let dt = now - self.last_press_time;
                        let dist = (pos - self.last_press_pos).length();
                        if dt < MULTI_CLICK_WINDOW_SECS && dist < MULTI_CLICK_DISTANCE_PX {
                            self.click_count = (self.click_count + 1).min(3);
                        } else {
                            self.click_count = 1;
                        }
                        self.last_press_time = now;
                        self.last_press_pos = pos;

                        let mode = match self.click_count {
                            1 => SelectionMode::Char,
                            2 => SelectionMode::Word,
                            _ => SelectionMode::Line,
                        };
                        pane.start_selection(to_point(pos), mode);
                    }
                } else if rendered.response.dragged()
                    && let Some(pos) = rendered.response.interact_pointer_pos()
                {
                    // Mode is preserved in the existing selection —
                    // `extend_to` just moves the head; the renderer /
                    // text extractor consult `Selection::mode` and
                    // snap the head outward to word / line bounds.
                    pane.extend_selection(to_point(pos));
                }
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
