//! Termica library entry point.
//!
//! Phase 2A: multi-pane workspace via [`egui_tiles`]. The tree's
//! leaves are typed [`PaneId`]s; the actual [`PaneSlot`] data
//! lives in a separate [`std::collections::HashMap`] so the tree
//! only sees value-types. Splits land in Phase 2B; for now the tree
//! is a single root [`Tabs`](egui_tiles::Container::Tabs) container
//! with one or more pane children.
//!
//! See [`SPEC.md`](../SPEC.md) and [`spec/01-architecture.md`](../spec/01-architecture.md)
//! for the layered architecture this crate grows into.
//!
//! ## Module layout
//!
//! The crate's internal surface is split by concern:
//!
//! - [`app`] — [`TermicaApp`], the eframe app loop and its frame-by-
//!   frame mode/modal/focus bookkeeping.
//! - [`behavior`] — `TabBehavior`, the per-frame [`egui_tiles::Behavior`]
//!   shim that draws tabs and routes Behavior callbacks.
//! - [`render_pane`] — [`render_pane`], the per-pane render path
//!   (resize, link scan, mouse, keyboard, scroll).
//! - [`pane_slot`] — [`PaneId`], [`PaneAction`], [`PaneSlot`],
//!   [`PaneUiState`].
//! - [`shortcuts`] — [`match_pane_shortcut`], the app-level keyboard
//!   shortcut matcher.
//! - [`tab_title`] — [`tab_title_for`], [`active_pane_in_tabs`].

#![forbid(unsafe_code)]

pub mod block;
pub mod block_selection;
pub mod echo_suppress;
pub mod events;
pub mod input;
pub mod integration;
pub mod links;
pub mod markers;
pub mod osc;
pub mod pane;
pub mod paths;
pub mod prompt_editor;
pub mod pty;
pub mod render;
pub mod selection;
pub mod shell;
pub mod shell_syntax;
pub mod terminal;

mod app;
mod behavior;
mod pane_slot;
mod render_pane;
mod shortcuts;
mod tab_title;

#[cfg(target_os = "macos")]
mod menu_macos;

// Public API re-exports. `main.rs`, `tests/snapshots.rs` and
// `tests/split_snapshots.rs` are the external consumers; everything
// they import from `termica::*` must be surfaced here.
pub use app::TermicaApp;
pub use behavior::paint_focused_tab_underline;
pub use pane_slot::{PaneAction, PaneId, PaneSlot, PaneUiState};
pub use render_pane::{
    ALT_SCREEN_BORDER_COLOR, ALT_SCREEN_BORDER_WIDTH, cells_from_pixels, paint_alt_screen_border,
    render_pane,
};
pub use shortcuts::match_pane_shortcut;
pub use tab_title::{active_pane_in_tabs, home_relative_cwd, tab_title_for};

use eframe::egui;

/// Minimum cell grid Termica will ever ask a PTY for. Below this,
/// shells and full-screen TTY programs behave erratically. The
/// window's `min_inner_size` is also clamped so a user can't drag
/// below the equivalent cells.
pub(crate) const MIN_ROWS: u16 = 5;
pub(crate) const MIN_COLS: u16 = 20;

/// Run the native window. Used by `main` and any future end-to-end
/// harness.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([400.0, 200.0])
            .with_title("Termica"),
        // macOS: suppress winit's default application menu. Its
        // Quit item calls `[NSApplication terminate:]` directly,
        // exiting before `update()` can render the quit-confirm
        // modal. Our own `muda` menu (installed below from the
        // creator callback, after `NSApplication` is initialized)
        // takes its place — same standard items, but with a custom
        // Quit whose action we route through `quit_requested`.
        #[cfg(target_os = "macos")]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::macos::EventLoopBuilderExtMacOS;
            builder.with_default_menu(false);
        })),
        ..Default::default()
    };
    eframe::run_native(
        "termica",
        options,
        Box::new(|cc| {
            // Force dark theme always, regardless of the system
            // light/dark preference. Termica is a terminal — a
            // light tab strip and panel chrome around a black
            // shell grid is visually jarring. The Phase 10 polish
            // pass can introduce a config-driven theme; for now
            // dark-only is the product.
            //
            // `set_theme` (not `set_visuals`) is the right call:
            // it pins `Memory::theme_preference = Dark`, which is
            // what egui resolves every frame. Using `set_visuals`
            // works for one frame and then the system-theme
            // follower overwrites it back.
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            #[cfg(target_os = "macos")]
            menu_macos::install_macos_menu();
            Ok(Box::new(TermicaApp::new()))
        }),
    )
}
