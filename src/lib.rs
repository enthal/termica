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
pub mod block_links;
pub mod block_selection;
pub mod completion;
pub mod echo_suppress;
pub mod events;
pub mod find;
pub mod focused_chrome;
pub mod gh_probe;
pub mod git_context;
pub mod git_probe;
pub mod history;
pub mod history_overlay;
pub mod icons;
pub mod input;
pub mod integration;
pub mod keybindings;
pub mod links;
pub mod markers;
pub mod osc;
pub mod pane;
pub mod pane_selection;
pub mod paths;
pub mod persist;
pub mod pr_context;
pub mod prompt_editor;
pub mod pty;
pub mod reflow;
pub mod render;
pub mod selection;
pub mod shell;
pub mod shell_syntax;
pub mod terminal;
pub mod visual_picker;
pub mod watermark;

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
    ALT_SCREEN_BORDER_COLOR, ALT_SCREEN_BORDER_WIDTH, StickyHeaderContent, StickyHeaderRender,
    cells_from_pixels, paint_alt_screen_border, paint_sticky_header, render_pane,
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

/// Stable application identifier. Used as the eframe app name, the
/// Wayland/X11 `app_id` (which becomes the window's `StartupWMClass`
/// for icon matching), and the basename of the Linux `.desktop` +
/// icon files we install. Must stay in sync with `StartupWMClass`
/// in [`desktop_entry_contents`].
const APP_ID: &str = "termica";

/// The window/dock/taskbar icon, embedded in the binary so there is
/// no runtime file dependency. Decoded by [`load_app_icon`]; written
/// verbatim to the XDG icon path by `install_desktop_entry` on Linux.
pub(crate) const APP_ICON_PNG: &[u8] = include_bytes!("../assets/app_icon.png");

/// Decode the embedded PNG into the RGBA buffer eframe wants for the
/// window icon. Returns `None` if the bytes fail to decode — a
/// missing window icon is a cosmetic loss, never worth panicking the
/// whole app over, so we degrade rather than `expect`.
fn load_app_icon() -> Option<egui::IconData> {
    let image =
        image::load_from_memory_with_format(APP_ICON_PNG, image::ImageFormat::Png).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData { rgba: image.into_raw(), width, height })
}

/// Contents of the freedesktop `.desktop` entry installed on Linux so
/// desktop environments (GNOME, KDE, Sway, …) can match the running
/// window to a name and icon in task switchers and docks.
///
/// Kept pure — no `cfg`, no filesystem — so it is unit-testable on
/// every platform. `StartupWMClass` MUST equal the `app_id` we set on
/// the viewport (both [`APP_ID`]); that string is what the compositor
/// reports for the live window and what it keys the icon lookup on.
// Non-Linux production builds have no caller (the installer is
// Linux-gated); the tests below still exercise it on every platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn desktop_entry_contents() -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Termica\n\
         Comment=Native terminal workspace\n\
         Exec={APP_ID}\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         StartupWMClass={APP_ID}\n\
         Categories=Development;System;TerminalEmulator;\n"
    )
}

/// On Linux, install the icon and a `.desktop` entry into the user's
/// XDG data dir (`$XDG_DATA_HOME` or `~/.local/share`) so the app is
/// identifiable in the launcher and its window carries our icon. Both
/// writes are skipped if the target already exists, and every step is
/// best-effort: a failure here never blocks startup, it just means
/// the desktop environment falls back to a generic icon.
#[cfg(target_os = "linux")]
fn install_desktop_entry() {
    let Some(base) = directories::BaseDirs::new() else {
        return;
    };
    let data_home = base.data_dir();

    let icon_dir = data_home.join("icons/hicolor/256x256/apps");
    let icon_path = icon_dir.join(format!("{APP_ID}.png"));
    if !icon_path.exists() && std::fs::create_dir_all(&icon_dir).is_ok() {
        let _ = std::fs::write(&icon_path, APP_ICON_PNG);
    }

    let app_dir = data_home.join("applications");
    let desktop_path = app_dir.join(format!("{APP_ID}.desktop"));
    if !desktop_path.exists() && std::fs::create_dir_all(&app_dir).is_ok() {
        let _ = std::fs::write(&desktop_path, desktop_entry_contents());
    }
}

/// Run the native window. Used by `main` and any future end-to-end
/// harness.
pub fn run() -> eframe::Result<()> {
    // Cheap arg sniff — no full clap dep needed for the current
    // surface (one boolean flag + one optional positional path).
    //
    // `--pick-chrome` opens the focused-editor chrome picker
    // viewport (a second OS window) on startup. Clicks in that
    // window live-update the chrome of the main window.
    //
    // Positional path argument: see
    // [spec/06 §"Startup cwd and positional argument"](spec/06-workspace-and-tiles.md#startup-cwd-and-positional-argument).
    // At most one positional arg is allowed; extras print to
    // stderr and exit non-zero.
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let pick_chrome = raw_args.iter().any(|a| a == "--pick-chrome");
    // `--pick-watermark` opens the blank-pane watermark tuner in a
    // second viewport (alpha / size / grayscale sliders), live-updating
    // the main window. Mirrors `--pick-chrome`.
    let pick_watermark = raw_args.iter().any(|a| a == "--pick-watermark");
    let positionals: Vec<&str> =
        raw_args.iter().filter(|a| !a.starts_with("--")).map(|s| s.as_str()).collect();
    if positionals.len() > 1 {
        eprintln!(
            "termica: too many positional arguments ({} given); expected at most one path",
            positionals.len()
        );
        std::process::exit(2);
    }
    let positional_path = positionals.first().map(std::path::Path::new);
    let startup_cwd = crate::app::resolve_startup_cwd(positional_path);
    let app_opts = crate::app::TermicaAppOptions {
        open_chrome_picker: pick_chrome,
        open_watermark_picker: pick_watermark,
        startup_cwd: Some(startup_cwd),
        ..Default::default()
    };
    // Linux: register the icon + launcher entry under XDG before we
    // open the window, so the compositor can match it on first map.
    #[cfg(target_os = "linux")]
    install_desktop_entry();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([400.0, 200.0])
        .with_title("Termica")
        // `app_id` drives the Wayland app_id / X11 WM_CLASS, which is
        // how Linux desktops link the live window to the installed
        // `.desktop` entry and its icon. Harmless on other platforms.
        .with_app_id(APP_ID);
    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
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
        APP_ID,
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
            Ok(Box::new(TermicaApp::new_with_options(app_opts.clone())))
        }),
    )
}

#[cfg(test)]
mod app_icon_tests {
    use super::*;

    #[test]
    fn embedded_icon_decodes_to_square_rgba() {
        let icon = load_app_icon().expect("embedded app icon must decode");
        assert_eq!(icon.width, icon.height, "app icon should be square");
        assert!(icon.width > 0, "app icon must have nonzero dimensions");
        // RGBA: four bytes per pixel, exactly width*height pixels.
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
    }

    #[test]
    fn desktop_entry_wm_class_matches_app_id() {
        let entry = desktop_entry_contents();
        // The compositor keys icon lookup on StartupWMClass; it must
        // equal the app_id we set on the viewport, or Linux desktops
        // fall back to a generic icon.
        assert!(entry.contains(&format!("StartupWMClass={APP_ID}\n")));
        assert!(entry.contains(&format!("Exec={APP_ID}\n")));
        assert!(entry.contains(&format!("Icon={APP_ID}\n")));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Type=Application\n"));
    }
}
