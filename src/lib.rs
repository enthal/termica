//! Termica library entry point.
//!
//! Phase 1B is the bare eframe skeleton: an empty native window. No
//! terminal pane yet. The renderer arrives in Phase 1E (#1).
//!
//! See [`SPEC.md`](../SPEC.md) and [`spec/01-architecture.md`](../spec/01-architecture.md)
//! for the layered architecture this crate grows into.

#![forbid(unsafe_code)]

pub mod pty;
pub mod terminal;

use eframe::egui;

/// Render the workspace's central panel content into `ui`.
///
/// Extracted as a free function (rather than living inside `eframe::App::update`)
/// so snapshot tests can drive it through `egui_kittest::Harness::new_ui`
/// without launching a real OS window. This is the CLAUDE.md rule "if logic
/// can be tested without a UI, it must not live inside a UI function"
/// applied to UI itself: the *content* of a UI function is a pure draw
/// over a `Ui`, which `egui_kittest` can render headlessly.
pub fn central_panel(ui: &mut egui::Ui) {
    ui.heading("Termica");
    ui.label(concat!(
        "Phase 1B — eframe skeleton (no terminal yet). ",
        "See SPEC.md for the design; the first PTY-backed pane arrives in Phase 1E."
    ));
}

/// The top-level eframe application. Today it has no state.
#[derive(Default)]
pub struct TermicaApp;

impl eframe::App for TermicaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, central_panel);
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
    eframe::run_native("termica", options, Box::new(|_cc| Ok(Box::new(TermicaApp))))
}
