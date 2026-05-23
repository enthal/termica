//! Termica library entry point.
//!
//! Phase 1E-b: the eframe app owns one [`pane::PaneSession`] that
//! drains a real PTY into a [`terminal::TerminalState`] in the
//! background. The custom cell renderer ([`render::paint_terminal`])
//! paints the grid directly into the central panel. Keyboard input
//! still goes nowhere — the input encoder is Phase 1E-c.
//!
//! See [`SPEC.md`](../SPEC.md) and [`spec/01-architecture.md`](../spec/01-architecture.md)
//! for the layered architecture this crate grows into.

#![forbid(unsafe_code)]

pub mod input;
pub mod pane;
pub mod pty;
pub mod render;
pub mod terminal;

use eframe::egui;

use pane::{PaneSession, PaneView};
use pty::PtyConfig;

/// Render the workspace's status header into `ui`.
///
/// Pure UI: takes a plain [`PaneView`] snapshot, never touches OS
/// resources, safe to drive from `egui_kittest` snapshot tests.
/// The cell grid is painted separately by [`render::paint_terminal`]
/// directly below this header.
pub fn central_panel(ui: &mut egui::Ui, view: &PaneView) {
    ui.heading("Termica");
    ui.label(format!(
        "Phase 1E-b — cell renderer live. \
         Bytes received: {}   ·   alt-screen: {}",
        view.bytes_received, view.alt_screen
    ));
    ui.separator();
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
}

impl TermicaApp {
    /// Construct an app with a freshly spawned shell pane sized to a
    /// generous default. The eframe `update` loop will resize the
    /// PTY as the window resizes in a later sub-PR.
    pub fn new() -> Self {
        let config = PtyConfig::default();
        let pane = PaneSession::spawn(24, 80, &config).map_err(|e| format!("{e}"));
        Self { pane }
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
