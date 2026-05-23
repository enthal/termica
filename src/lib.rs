//! Termica library entry point.
//!
//! Phase 1E-a: the eframe app owns one [`pane::PaneSession`] that
//! drains a real PTY into a [`terminal::TerminalState`] in the
//! background and renders a debug view (byte counter + monospaced
//! screen text). The custom cell renderer arrives in Phase 1E-b.
//!
//! See [`SPEC.md`](../SPEC.md) and [`spec/01-architecture.md`](../spec/01-architecture.md)
//! for the layered architecture this crate grows into.

#![forbid(unsafe_code)]

pub mod pane;
pub mod pty;
pub mod terminal;

use eframe::egui;

use pane::{PaneSession, PaneView};
use pty::PtyConfig;

/// Render the workspace's central panel content into `ui`.
///
/// Pure UI: takes a plain [`PaneView`] snapshot, never touches OS
/// resources, safe to drive from `egui_kittest` snapshot tests.
pub fn central_panel(ui: &mut egui::Ui, view: &PaneView) {
    ui.heading("Termica");
    ui.label(format!(
        "Phase 1E-a — PTY pipeline live. \
         Bytes received: {}   ·   alt-screen: {}",
        view.bytes_received, view.alt_screen
    ));
    ui.separator();
    ui.monospace(&view.screen_text);
}

/// The top-level eframe application.
///
/// Holds one [`PaneSession`] for now (multi-pane workspace lives in
/// [Phase 2 (#2)](https://github.com/enthal/termica/issues/2)). On
/// spawn failure the pane is `None` and the UI renders an error
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
        // Liveness: ask for a redraw next frame so PTY output keeps
        // flowing even when the user is idle. (~ every 50 ms is more
        // than enough for a debug view; the renderer PR will tune.)
        ctx.request_repaint_after(std::time::Duration::from_millis(50));

        let view = match &mut self.pane {
            Ok(pane) => {
                pane.drain();
                pane.view()
            }
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
