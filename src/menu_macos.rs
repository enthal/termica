//! macOS application menu via `muda`. Installed from the eframe
//! creator callback because `init_for_nsapp` requires `NSApplication`
//! to already be initialized, which happens during the event loop's
//! `resumed` event — i.e. before the creator runs.

/// Build and install our macOS application menu via `muda`. Called
/// from the eframe creator callback because `init_for_nsapp` requires
/// `NSApplication` to already be initialized, which happens during
/// the event loop's `resumed` event — i.e. before the creator runs.
///
/// We keep the standard layout (About, Hide / Hide Others / Show
/// All, Services, Quit) but the Quit item is a custom `MenuItem`
/// whose action surfaces as a `MenuEvent` we handle in `update()`,
/// rather than calling `[NSApp terminate:]` directly. This is the
/// whole reason we install a menu at all — winit's default menu
/// would have terminated the process before our modal got to render.
///
/// The Rust-side menu handles MUST outlive the app: muda's
/// NSMenuItem subclass stores a raw `*const MenuChild` ivar with no
/// retain count, so dropping the `Menu` handle invalidates all the
/// pointers and any predefined-About / custom-MenuItem click
/// dereferences freed memory (EXC_BAD_ACCESS in `fire_menu_item_click`).
/// The predefined items that bind directly to AppKit selectors
/// (`hide:`, `hideOtherApplications:`, etc.) are unaffected — they
/// never read the ivar — which is why a leak-free version of this
/// function would look like it works.
///
/// We leak the menu via `Box::leak` to make this lifetime explicit.
/// It's a one-time app-singleton; the OS would own the NSMenu for
/// the process lifetime anyway.
pub fn install_macos_menu() {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    use muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};

    let menu = Box::leak(Box::new(Menu::new()));
    let app_menu = Submenu::new("Termica", true);
    menu.append(&app_menu).expect("muda: append app submenu");

    // Custom About item — fires `MenuEvent { id: "about" }` so we
    // can render our own egui modal instead of the standard macOS
    // about panel. Same plumbing as Quit; no accelerator (the
    // standard "About <App>" item is menu-clickable only).
    let about_item = MenuItem::with_id("about", "About Termica", true, None);

    // Custom Quit item: pressing Cmd+Q (or clicking it) fires a
    // `MenuEvent { id: "quit" }` which `update()` translates to
    // `quit_requested = true`, taking the normal "any pane running?"
    // branch (modal or immediate exit).
    let quit_item = MenuItem::with_id(
        "quit",
        "Quit Termica",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyQ)),
    );

    app_menu
        .append_items(&[
            &about_item,
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &quit_item,
        ])
        .expect("muda: append app menu items");

    menu.init_for_nsapp();
}
