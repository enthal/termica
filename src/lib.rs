//! Termica library entry point.
//!
//! Phase 2A: multi-pane workspace via [`egui_tiles`]. The tree's
//! leaves are typed [`PaneId`]s; the actual [`PaneSession`] data
//! lives in a separate [`HashMap`] so the tree only sees value-
//! types. Splits land in Phase 2B; for now the tree is a single
//! root [`Tabs`](egui_tiles::Container::Tabs) container with one or
//! more pane children.
//!
//! See [`SPEC.md`](../SPEC.md) and [`spec/01-architecture.md`](../spec/01-architecture.md)
//! for the layered architecture this crate grows into.

#![forbid(unsafe_code)]

pub mod input;
pub mod integration;
pub mod links;
pub mod markers;
pub mod osc;
pub mod pane;
pub mod paths;
pub mod pty;
pub mod render;
pub mod selection;
pub mod terminal;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use alacritty_terminal::grid::Dimensions;
use eframe::egui;
use egui_tiles::{Behavior, Tile, TileId, Tiles, Tree, UiResponse};

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
const MULTI_CLICK_WINDOW_SECS: f64 = 0.5;

/// Max pixel distance the pointer can drift between mousedowns and
/// still register as a multi-click.
const MULTI_CLICK_DISTANCE_PX: f32 = 8.0;

/// Cmd+Q confirmation timeout: after this many seconds with no
/// response from the user, the modal auto-confirms and the app
/// exits. Sized at one minute per the user's UX preference: long
/// enough to walk away and let it close, short enough that an
/// accidentally-triggered modal doesn't pin the app open forever.
const QUIT_CONFIRM_TIMEOUT_SECS: f64 = 60.0;

// =====================================================================
//  Pane identity + per-pane state
// =====================================================================

/// Typed pane identifier. Used both as the leaf payload inside
/// [`egui_tiles::Tree`] and as the key into [`TermicaApp::panes`].
///
/// Newtype around `u64` so the type system distinguishes pane IDs
/// from other identifier flavours that arrive in later phases
/// (`CommandRunId`, `HistoryEntryId`, …). `Copy + Eq + Hash` so it
/// works as both a tree payload and a HashMap key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

/// App-level intents staged by [`render_pane`] when the focused
/// pane saw a shortcut keystroke. Applied by [`TermicaApp::update`]
/// after `tree.ui()` returns (because mutating the tree from
/// inside a Behavior callback is unsupported).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAction {
    /// Cmd+T (macOS) / Ctrl+Shift+T (Linux): spawn a new pane in
    /// the focused pane's Tabs container, focused immediately.
    NewTab,
    /// Cmd+W (macOS) / Ctrl+Shift+W (Linux): close the focused
    /// pane's tab. If a program is running (heuristic: alt-screen
    /// is on), the close is gated behind a confirmation banner;
    /// otherwise it happens immediately.
    CloseTab,
    /// Cmd+Q (macOS) / Ctrl+Shift+Q (Linux): exit the whole app.
    /// If any pane has alt-screen on, gated behind a modal
    /// confirmation; otherwise exits immediately. The confirmation
    /// auto-exits after 60 seconds of no response.
    Quit,
    /// Cmd+Shift+] (macOS) / Ctrl+Shift+] (Linux): next tab in the
    /// parent Tabs container (wraps).
    NextTab,
    /// Cmd+Shift+[ (macOS) / Ctrl+Shift+[ (Linux): previous tab.
    PrevTab,
}

/// Per-pane UI interaction state. Each pane gets its own multi-
/// click counter + last-press snapshot — a press in pane A and a
/// press in pane B must never chain into a "double-click".
#[derive(Debug, Default)]
pub struct PaneUiState {
    last_press_time: f64,
    last_press_pos: egui::Pos2,
    click_count: u8,
    /// Last `(rows, cols)` we resized this PTY to. Cached so we
    /// only issue a resize when the cell grid actually changes,
    /// not every frame.
    last_size: Option<(u16, u16)>,
    /// Did this pane hold keyboard focus on the most-recently
    /// completed frame? Written by [`render_pane`] right after the
    /// terminal grid is painted; read by [`TermicaApp::update`]
    /// after `tree.ui()` returns so the focused tab can be styled
    /// on the next frame.
    pub focused: bool,
    /// "Grab keyboard focus on the next render". Set by
    /// [`TermicaApp::update`] when an event makes this pane newly
    /// active (drag-drop, new-tab spawn, tab click). Consumed by
    /// [`render_pane`] which calls `request_focus` on the grid
    /// widget and clears the flag.
    pub needs_focus: bool,
    /// App-level shortcut intent staged this frame (Cmd+T,
    /// Cmd+Shift+], …). Drained by [`TermicaApp::update`] after
    /// `tree.ui()` returns; [`render_pane`] sets it when it sees
    /// the matching shortcut in the focused pane's event stream.
    pub pending_action: Option<PaneAction>,
}

/// One pane's full state: the OS-resource [`PaneSession`] (PTY +
/// reader thread + terminal grid) plus the [`PaneUiState`].
pub struct PaneSlot {
    pub session: PaneSession,
    pub ui: PaneUiState,
}

// =====================================================================
//  Open URL / path
// =====================================================================

/// Spawn the OS's "open this URL or path" handler.
///
/// macOS: `open <arg>`. Linux/BSD: `xdg-open <arg>`. The argument
/// is passed as a single `arg`, never interpolated into a shell
/// string, so PTY-controlled content cannot inject extra arguments.
fn open_url(url: &str) {
    let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if let Err(e) = std::process::Command::new(cmd).arg(url).spawn() {
        eprintln!("termica: failed to open {url:?}: {e}");
    }
}

// =====================================================================
//  Status header + cell-fit math
// =====================================================================

/// Render the workspace's status header into `ui`.
///
/// Pure UI: takes a plain [`PaneView`] snapshot, never touches OS
/// resources, safe to drive from `egui_kittest` snapshot tests.
/// The cell grid is painted separately by [`render::paint_terminal`]
/// directly below this header.
pub fn central_panel(ui: &mut egui::Ui, view: &PaneView) {
    ui.heading("Termica");
    ui.label(format!(
        "Phase 2A — tabs live. \
         Bytes: {}   ·   alt-screen: {}   ·   grid: {}×{}",
        view.bytes_received, view.alt_screen, view.rows, view.cols
    ));
    if let Some(cwd) = &view.cwd {
        ui.label(format!("cwd: {}", cwd.display()));
    }
    ui.separator();
}

/// Compute how many `(rows, cols)` fit into `avail` at the given
/// cell metrics, clamped to [`MIN_ROWS`] × [`MIN_COLS`]. Pure
/// function so the rounding / clamp policy is unit-testable.
pub fn cells_from_pixels(avail: egui::Vec2, cell_w: f32, row_h: f32) -> (u16, u16) {
    let cols = if cell_w > 0.0 { (avail.x / cell_w).floor().max(0.0) as u16 } else { MIN_COLS };
    let rows = if row_h > 0.0 { (avail.y / row_h).floor().max(0.0) as u16 } else { MIN_ROWS };
    (rows.max(MIN_ROWS), cols.max(MIN_COLS))
}

// =====================================================================
//  Per-pane rendering
// =====================================================================

/// Render one pane into `ui`: status header, resize-PTY-to-cells,
/// link scan + hover, terminal grid, mouse selection, scroll wheel,
/// input encoding, copy shortcut.
///
/// Pulled out of the eframe `update` loop so the same code path
/// runs for every visible pane — Phase 2A's "one pane in one tab"
/// AND Phase 2B's "multiple panes across splits". The Behavior
/// callback per visible pane invokes this.
pub fn render_pane(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    slot: &mut PaneSlot,
    home: Option<&Path>,
    modal_open: bool,
) {
    // Input routing in a multi-pane world:
    //
    // - Keyboard events go to the **focused** pane only. Clicking a
    //   pane gives it focus. Without this gate, every visible pane
    //   would write the same keystrokes to its own PTY — typing
    //   would duplicate across splits.
    // - Mouse wheel goes to whatever pane the pointer is **over**
    //   (NOT necessarily the focused one). This matches iTerm /
    //   tmux: you can scroll a non-focused pane just by hovering
    //   and turning the wheel.
    //
    // Both gates live below, after `paint_terminal` returns the
    // response we check.
    //
    // `modal_open` is the third gate: when a modal (close-confirm or
    // quit-confirm) is showing, the pane is **inert** — no keystrokes
    // to the PTY, no wheel scrolling, no mouse selection, no focus
    // grabs. `egui::Modal` renders later in the frame, so we cannot
    // rely on it to consume events that `ctx.input` reads here; we
    // have to gate explicitly.

    // ---- status header ------------------------------------------
    let view = slot.session.view();
    central_panel(ui, &view);

    // ---- resize PTY to fit available space ----------------------
    let avail = ui.available_size();
    let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
    let (cell_w, row_h) = ui.fonts_mut(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
    let (rows, cols) = cells_from_pixels(avail, cell_w, row_h);
    if slot.ui.last_size != Some((rows, cols)) {
        let _ = slot.session.resize(rows, cols);
        slot.ui.last_size = Some((rows, cols));
    }

    // ---- clickable URLs + paths ---------------------------------
    //
    // Pre-compute geometry so the hover hit-test happens in the
    // SAME frame as the paint — no one-frame lag on the
    // Cmd-hover underline.
    let display_offset = slot.session.terminal().display_offset() as i32;
    let grid_rows = slot.session.terminal().grid().screen_lines();
    let grid_cols = slot.session.terminal().grid().columns();
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

    let grid_ref = slot.session.terminal().grid();
    let mut links_in_view = links::scan_visible_links(grid_ref);
    let cwd = slot.session.terminal().cwd().map(|p| p.to_path_buf());
    let path_spans = paths::scan_visible_paths(grid_ref, cwd.as_deref(), home);
    links_in_view.extend(path_spans);

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

    // ---- terminal grid ------------------------------------------
    let selection = slot.session.selection().copied();
    let rendered =
        render::paint_terminal(ui, slot.session.terminal(), selection.as_ref(), highlighted_link);
    if highlighted_link.is_some() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    // ---- mouse selection / link click + focus-on-press ----------
    //
    // Clicking inside the grid does three things at once:
    //   1. Grabs keyboard focus (so the keyboard gate below lets
    //      this pane's events through).
    //   2. Either opens a Cmd/Ctrl-clicked URL or starts a selection
    //      (multi-click counter for char/word/line mode).
    //   3. Resets click-counter timing.
    let geom_after_paint = rendered.geometry;
    let to_point = |pos: egui::Pos2| pixel_to_grid_point(pos.x, pos.y, geom_after_paint);
    let (primary_pressed, press_origin, now) =
        ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.press_origin(), i.time));

    if !modal_open
        && primary_pressed
        && let Some(pos) = press_origin
        && rendered.response.rect.contains(pos)
    {
        rendered.response.request_focus();

        let press_pt = to_point(pos);
        let link_under_press = links_in_view.iter().find(|l| l.contains(press_pt)).cloned();

        if modifier_held && let Some(link) = link_under_press {
            open_url(&link.url);
        } else {
            let dt = now - slot.ui.last_press_time;
            let dist = (pos - slot.ui.last_press_pos).length();
            if dt < MULTI_CLICK_WINDOW_SECS && dist < MULTI_CLICK_DISTANCE_PX {
                slot.ui.click_count = (slot.ui.click_count + 1).min(3);
            } else {
                slot.ui.click_count = 1;
            }
            slot.ui.last_press_time = now;
            slot.ui.last_press_pos = pos;

            let mode = match slot.ui.click_count {
                1 => SelectionMode::Char,
                2 => SelectionMode::Word,
                _ => SelectionMode::Line,
            };
            if mode == SelectionMode::Word
                && let Some(link) = link_under_press
            {
                slot.session.start_url_selection(link.start, link.end);
            } else {
                slot.session.start_selection(to_point(pos), mode);
            }
        }
    } else if !modal_open
        && rendered.response.dragged()
        && let Some(pos) = rendered.response.interact_pointer_pos()
    {
        slot.session.extend_selection(to_point(pos));
    }

    // ---- focus transfer & bootstrap -----------------------------
    //
    // Three reasons to claim focus here:
    //   1. `slot.ui.needs_focus` was set by the app: the user just
    //      clicked this pane's tab, spawned a new tab via [+], or
    //      dragged into a new region. Honour it on this render.
    //   2. Nothing in the whole app holds focus yet (cold launch).
    //      Whichever visible pane renders first grabs focus so
    //      typing works without an explicit click.
    //
    // The press handler above ALSO calls `request_focus` on click —
    // that path covers clicking inside a pane's grid (rather than
    // its tab title).
    let needs_focus = std::mem::take(&mut slot.ui.needs_focus);
    let nothing_focused = ctx.memory(|m| m.focused().is_none());
    if !modal_open && (needs_focus || nothing_focused) {
        rendered.response.request_focus();
    }

    // While this pane has keyboard focus, claim Tab and the arrow
    // keys + Escape for the terminal. Without this, egui's built-in
    // focus navigation eats Tab (cycles focus between widgets) and
    // some arrow paths (focus-based directional nav), so they never
    // reach the encoder + PTY — visible as focus bouncing into the
    // tab strip's [+] button instead of typing into the shell.
    //
    // `set_focus_lock_filter` only takes effect once the widget has
    // BOTH `had_focus_last_frame(id)` AND `has_focus(id)`; new
    // panes spend exactly one frame "warming up" before the filter
    // is active. That's fine — a one-frame Tab-eats-focus on the
    // first frame of a new pane is invisible in practice.
    ctx.memory_mut(|m| {
        m.set_focus_lock_filter(
            rendered.response.id,
            egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            },
        );
    });

    // Record focus state for the next frame's tab styling.
    // `has_focus` is read by [`TermicaApp::update`] after
    // `tree.ui()` returns; the focused pane's tab title is then
    // rendered bold via the Behavior.
    slot.ui.focused = rendered.response.has_focus();

    // ---- keyboard input, focus-gated ----------------------------
    if !modal_open && rendered.response.has_focus() {
        let is_macos = cfg!(target_os = "macos");
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());

        // Copy shortcut: macOS trusts Event::Copy from Cmd+C; off-
        // Mac we look for Ctrl+Shift+C in the Key event (plain
        // Ctrl+C must remain SIGINT for the shell, so we cannot
        // trust Event::Copy there).
        let copy_pressed = events.iter().any(|e| match e {
            egui::Event::Copy => is_macos,
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                !is_macos && input::is_copy_shortcut(*key, *modifiers, false)
            }
            _ => false,
        });
        if copy_pressed && let Some(text) = slot.session.selection_text() {
            ctx.copy_text(text);
        }

        // App-level shortcuts: Cmd+T new-tab, Cmd+Shift+]/[ next/prev
        // tab. `modifiers.command` is egui's platform-aware "primary"
        // modifier (Cmd on macOS, Ctrl elsewhere). The encoder's new
        // "any modifier → unmapped" rule means these events would
        // already produce no PTY bytes; we just intercept here to
        // stage the app intent.
        for event in &events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = event
                && let Some(action) = match_pane_shortcut(*key, *modifiers, is_macos)
            {
                slot.ui.pending_action = Some(action);
                break;
            }
        }

        let modes = slot.session.terminal().modes();
        for event in &events {
            // Belt and braces: skip the Ctrl+Shift+C key event so the
            // encoder never sees it (the encoder wouldn't emit bytes
            // for that modifier combo anyway, but be explicit).
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = event
                && !is_macos
                && input::is_copy_shortcut(*key, *modifiers, false)
            {
                continue;
            }
            if let Some(bytes) = input::encode_event(event, modes) {
                let _ = slot.session.write(&bytes);
            }
        }
    }

    // ---- mouse wheel, hover-gated -------------------------------
    if !modal_open && rendered.response.hovered() {
        let scroll_delta_y = ctx.input(|i| i.smooth_scroll_delta.y);
        if scroll_delta_y.abs() > 0.0 {
            let lines = (scroll_delta_y / 50.0 * 3.0).round() as i32;
            let alt_screen = slot.session.terminal().is_alternate_screen();
            let modes = slot.session.terminal().modes();
            match input::classify_wheel(lines, alt_screen, modes) {
                Some(input::WheelOutcome::ScrollDisplay(lines)) => {
                    slot.session.terminal_mut().scroll_display(lines);
                }
                Some(input::WheelOutcome::SendBytes(bytes)) => {
                    let _ = slot.session.write(&bytes);
                }
                None => {}
            }
        }
    }
}

// =====================================================================
//  Tab title helper
// =====================================================================

/// Map a `(key, modifiers, is_macos)` triple to a [`PaneAction`]
/// shortcut, or `None` if it's not one we recognise.
///
/// # Platform conventions
///
/// - **macOS** uses `Cmd` (`modifiers.mac_cmd`) for app-level
///   shortcuts because `Cmd` is reserved for the app — `Ctrl` in a
///   terminal goes to the shell as control characters. The
///   "letter" shortcuts don't carry Shift; the bracket-pair tab-
///   nav shortcuts do (it's part of the physical chord).
/// - **Linux / Windows** uses `Ctrl+Shift` because plain `Ctrl+X`
///   in a terminal almost always conflicts with a shell binding
///   (`Ctrl+T` is `transpose-chars`, `Ctrl+W` is `backward-kill-word`,
///   `Ctrl+Q` is XON flow control, etc.). The `Shift` suffix is the
///   gnome-terminal / konsole / xterm convention to disambiguate
///   "app shortcut" from "shell binding".
///
/// `is_macos` is passed in (not `cfg!`-checked inline) so the
/// matcher is testable for both platforms regardless of where the
/// test runs.
///
/// On a US keyboard, `Shift+]` produces `}` and egui reports the
/// key as `Key::CloseCurlyBracket` rather than `CloseBracket`. We
/// accept either variant for the bracket-pair shortcuts so the
/// physical `Shift+]` chord works regardless of how the OS
/// transliterates it.
pub fn match_pane_shortcut(
    key: egui::Key,
    modifiers: egui::Modifiers,
    is_macos: bool,
) -> Option<PaneAction> {
    if is_macos {
        // Mac: Cmd held, no Ctrl, no Alt. Shift varies per action.
        if !modifiers.mac_cmd || modifiers.ctrl || modifiers.alt {
            return None;
        }
        let shift = modifiers.shift;
        match (key, shift) {
            (egui::Key::T, false) => Some(PaneAction::NewTab),
            (egui::Key::W, false) => Some(PaneAction::CloseTab),
            (egui::Key::Q, false) => Some(PaneAction::Quit),
            (egui::Key::CloseBracket | egui::Key::CloseCurlyBracket, true) => {
                Some(PaneAction::NextTab)
            }
            (egui::Key::OpenBracket | egui::Key::OpenCurlyBracket, true) => {
                Some(PaneAction::PrevTab)
            }
            _ => None,
        }
    } else {
        // Linux / Windows: Ctrl+Shift held, no Cmd, no Alt. Shift is
        // required for every action; with the bracket pair it's
        // also the modifier that produces the curly-bracket key
        // code from the same physical key.
        if !modifiers.ctrl || !modifiers.shift || modifiers.alt || modifiers.mac_cmd {
            return None;
        }
        match key {
            egui::Key::T => Some(PaneAction::NewTab),
            egui::Key::W => Some(PaneAction::CloseTab),
            egui::Key::Q => Some(PaneAction::Quit),
            egui::Key::CloseBracket | egui::Key::CloseCurlyBracket => Some(PaneAction::NextTab),
            egui::Key::OpenBracket | egui::Key::OpenCurlyBracket => Some(PaneAction::PrevTab),
            _ => None,
        }
    }
}

/// Compute the tab title for a pane.
///
/// Rule, in order:
/// 1. If the shell hasn't yet reported an OSC-7 cwd, fall back to
///    `pane N` so the user has *something* to recognise the tab by.
/// 2. Otherwise, return the cwd as an absolute path, with `$HOME`
///    replaced by `~`:
///    - `cwd == $HOME` → `~`
///    - `cwd` is strictly inside `$HOME` → `~/relative/path`
///    - cwd doesn't sit under `$HOME` (or `$HOME` is unknown) →
///      the cwd as-is.
///
/// Edge cases that bit us in earlier drafts:
/// - `$HOME` may or may not have a trailing slash (`/Users/tim`
///   vs `/Users/tim/`); both should yield the same `~`-substitution.
/// - `cwd` from OSC 7 may or may not have a trailing slash; we
///   normalize both ends so `/Users/tim/` produces `~`, not `~/`.
/// - `/Users/timjones` must NOT match the prefix `/Users/tim` —
///   that would render `~jones` for an unrelated user's dir. The
///   `rest.starts_with('/')` guard prevents that.
///
/// Pure function so the rule is unit-testable without any egui
/// plumbing.
pub fn tab_title_for(pane_id: PaneId, cwd: Option<&Path>, home: Option<&Path>) -> String {
    let Some(c) = cwd else {
        return format!("pane {}", pane_id.0);
    };
    let cwd_s = c.to_string_lossy();
    // Normalize trailing slash on the cwd (but keep `/` itself).
    let cwd_norm: &str = if cwd_s.as_ref() == "/" { "/" } else { cwd_s.trim_end_matches('/') };

    let raw = if let Some(h) = home {
        let home_s = h.to_string_lossy();
        let home_norm = home_s.trim_end_matches('/');
        if !home_norm.is_empty() {
            if cwd_norm == home_norm {
                "~".to_string()
            } else if let Some(rest) = cwd_norm.strip_prefix(home_norm)
                && rest.starts_with('/')
            {
                format!("~{rest}")
            } else {
                cwd_norm.to_string()
            }
        } else {
            cwd_norm.to_string()
        }
    } else {
        cwd_norm.to_string()
    };

    truncate_tab_title(&raw)
}

/// Maximum rendered width of a tab title, in chars. Titles longer
/// than this collapse to `..` + their trailing portion, since the
/// suffix (a leaf dir like `termica`, or a project subdir) is the
/// part the user is actually trying to disambiguate by. Picked by
/// eyeballing: 25 chars fits comfortably without forcing the tab
/// strip to scroll on a default-size window.
const MAX_TAB_TITLE_CHARS: usize = 25;

/// Truncate a tab title to [`MAX_TAB_TITLE_CHARS`], preserving the
/// *tail* of the string with a `..` prefix when truncation kicks
/// in. Counting is in `char`s, not bytes, so a directory whose name
/// happens to contain multi-byte UTF-8 doesn't get cut mid-codepoint.
fn truncate_tab_title(s: &str) -> String {
    let count = s.chars().count();
    if count <= MAX_TAB_TITLE_CHARS {
        return s.to_string();
    }
    const ELLIPSIS: &str = "..";
    let keep = MAX_TAB_TITLE_CHARS - ELLIPSIS.len();
    let suffix: String = s.chars().skip(count - keep).collect();
    format!("{ELLIPSIS}{suffix}")
}

/// Resolve the active pane id inside a Tabs container, if any. Pure
/// Tree navigation; lifted out of [`TermicaApp::spawn_new_pane_in_tabs`]
/// so it can be exercised without bringing up a real PTY.
///
/// Returns `None` if `tabs_tile` doesn't refer to a Tabs container,
/// the container has no active child, the active child isn't a leaf
/// pane, or the tile id is dangling.
pub fn active_pane_in_tabs(tree: &Tree<PaneId>, tabs_tile: TileId) -> Option<PaneId> {
    let Tile::Container(egui_tiles::Container::Tabs(tabs)) = tree.tiles.get(tabs_tile)? else {
        return None;
    };
    let active = tabs.active?;
    match tree.tiles.get(active)? {
        Tile::Pane(pane_id) => Some(*pane_id),
        _ => None,
    }
}

// =====================================================================
//  The eframe app
// =====================================================================

/// The top-level eframe application.
///
/// Owns the layout tree + every live pane. egui_tiles owns the
/// topology; we own the pane data so the tree only stores cheap
/// [`PaneId`] values.
pub struct TermicaApp {
    /// All live panes, by id. Drained every frame (visible or not)
    /// so background tabs accumulate output rather than blocking
    /// the PTY reader thread on a full mpsc channel.
    panes: HashMap<PaneId, PaneSlot>,
    /// Layout tree: leaves are PaneIds, internal nodes are Tabs /
    /// Linear containers (the Linear case lands in Phase 2B).
    tree: Tree<PaneId>,
    /// Monotonic counter for [`PaneId`] minting. IDs are never
    /// reused even after a pane closes.
    next_pane_id: u64,
    /// `$HOME` captured once at startup for `~/...` path expansion
    /// in the clickable-path scanner.
    home: Option<PathBuf>,
    /// "User clicked [+] in this tab strip" — staged by the
    /// Behavior callback, applied after `tree.ui()` returns so we
    /// don't mutate the tree from inside the callback.
    new_tab_requested_in: Option<TileId>,
    /// Tab close requests staged by the Behavior callback. We defer
    /// the actual removal to `update()` so we can call
    /// [`Tree::remove_recursively`] — which cleans up the parent
    /// container's `children` and `active` references. egui_tiles'
    /// default close path calls `tiles.remove()` directly, leaving
    /// a stale `active` pointing at a now-removed tile and
    /// blanking the pane area.
    pending_closes: Vec<TileId>,
    /// Which pane currently holds keyboard focus. Updated by
    /// [`render_pane`] each frame and consumed by [`TabBehavior`]'s
    /// styling hooks to highlight the focused pane's tab. One-frame
    /// lag is acceptable — the focus styling catches up next frame.
    focused_pane: Option<PaneId>,
    /// Set of pane ids that were the active tab in their respective
    /// Tabs container on the **previous** frame. Used to detect
    /// when the user (or a drag-drop / new-tab spawn) makes a
    /// different pane active — that pane then grabs keyboard focus
    /// on the next render via `slot.ui.needs_focus`.
    prev_active_panes: HashSet<PaneId>,
    /// Most-recently-focused panes, front = most recent. Updated
    /// at the end of each frame from `focused_pane`. Consulted when
    /// a focused tab is closed: instead of letting `egui_tiles`'
    /// default fallback pick the first child in the parent Tabs,
    /// we restore focus to the **most-recently-focused live** pane
    /// — i.e. the one the user was on before they switched to the
    /// tab they just closed.
    focus_history: Vec<PaneId>,
    /// "Cmd+Q was pressed this frame" — set by `apply_pane_action`
    /// and consumed in `update` *after* the pane mutations have
    /// settled. The any-pane-running check happens at consumption
    /// time so the alt-screen state reflects this frame's tree
    /// (in particular: post-close, in case Cmd+W on the last tab
    /// routes through here).
    quit_requested: bool,
    /// `Some(tile_id)` when a close-tab gesture (Cmd+W or the tab
    /// strip's X button) hit a tab whose pane is running a program
    /// (alt-screen on). The app renders a modal asking the user to
    /// confirm; Cancel / Esc / backdrop click clears, Close pushes
    /// `tile_id` onto `pending_closes`.
    pending_close_confirm: Option<TileId>,
    /// egui input-time the quit-confirmation modal opened. The
    /// modal renders each frame from this; if `ctx.time` ever
    /// exceeds `started_at + QUIT_CONFIRM_TIMEOUT_SECS`, we
    /// auto-confirm and exit. `None` when no quit is pending.
    quit_confirm_started_at: Option<f64>,
    /// "Exit the app on the next frame", set by the immediate-quit
    /// path (no program running) or by the modal's Quit button /
    /// timeout. Kept as a flag rather than a direct method call
    /// so the only place we send `ViewportCommand::Close` is the
    /// tail of `update`.
    should_quit: bool,
    /// `true` while the in-app "About Termica" modal is showing.
    /// Driven by the `MenuEvent { id: "about" }` from our macOS
    /// menubar (and, in the future, any other surface that wants
    /// to open it). Closed by Esc / backdrop / OK button.
    about_open: bool,
}

impl TermicaApp {
    /// Construct an app with one initial pane in a single Tabs
    /// container at the root of the tree.
    pub fn new() -> Self {
        let home = home::home_dir();
        let mut app = Self {
            panes: HashMap::new(),
            tree: Tree::empty("termica-tree"),
            next_pane_id: 0,
            home,
            new_tab_requested_in: None,
            pending_closes: Vec::new(),
            focused_pane: None,
            prev_active_panes: HashSet::new(),
            focus_history: Vec::new(),
            quit_requested: false,
            pending_close_confirm: None,
            quit_confirm_started_at: None,
            should_quit: false,
            about_open: false,
        };
        app.bootstrap();
        app
    }

    fn bootstrap(&mut self) {
        let pane_id = self.mint_pane_id();
        let session = PaneSession::spawn(MIN_ROWS.max(24), MIN_COLS.max(80), &PtyConfig::default())
            .expect("spawn initial pane");
        self.panes.insert(pane_id, PaneSlot { session, ui: PaneUiState::default() });

        let mut tiles = Tiles::default();
        let pane_tile = tiles.insert_pane(pane_id);
        let tabs_tile = tiles.insert_tab_tile(vec![pane_tile]);
        self.tree = Tree::new("termica-tree", tabs_tile, tiles);
    }

    fn mint_pane_id(&mut self) -> PaneId {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        id
    }

    /// Find the [`TileId`] of the leaf tile that carries `pane_id`,
    /// or `None` if the pane has been removed from the tree.
    fn tile_for_pane(&self, pane_id: PaneId) -> Option<TileId> {
        for (tile_id, tile) in self.tree.tiles.iter() {
            if let Tile::Pane(id) = tile
                && *id == pane_id
            {
                return Some(*tile_id);
            }
        }
        None
    }

    /// Find the parent [`TileId`] (a Tabs container) of a leaf
    /// tile, walking the tree by `parent_of`. Returns `None` if
    /// the tile is the root, doesn't exist, or has a non-Tabs
    /// parent (the tab-nav shortcuts only operate on Tabs).
    fn parent_tabs_of(&self, tile_id: TileId) -> Option<TileId> {
        let parent = self.tree.tiles.parent_of(tile_id)?;
        match self.tree.tiles.get(parent)? {
            Tile::Container(egui_tiles::Container::Tabs(_)) => Some(parent),
            _ => None,
        }
    }

    /// Apply a [`PaneAction`] originating from `pane_id`'s focused
    /// shortcut handler. CloseTab / Quit may stage a confirmation
    /// instead of acting immediately; the rest mutate the tree.
    fn apply_pane_action(&mut self, pane_id: PaneId, action: PaneAction) {
        match action {
            PaneAction::NewTab => {
                if let Some(parent_tabs) =
                    self.tile_for_pane(pane_id).and_then(|t| self.parent_tabs_of(t))
                {
                    self.spawn_new_pane_in_tabs(parent_tabs);
                }
            }
            PaneAction::NextTab => {
                if let Some(parent_tabs) =
                    self.tile_for_pane(pane_id).and_then(|t| self.parent_tabs_of(t))
                {
                    self.cycle_active_tab(parent_tabs, 1);
                }
            }
            PaneAction::PrevTab => {
                if let Some(parent_tabs) =
                    self.tile_for_pane(pane_id).and_then(|t| self.parent_tabs_of(t))
                {
                    self.cycle_active_tab(parent_tabs, -1);
                }
            }
            PaneAction::CloseTab => {
                // Closing the *last* tab is equivalent to quitting:
                // without a pane there's nothing left to do, and we
                // don't want to leave behind a tab-less empty
                // window. Route through the Quit path so the alt-
                // screen confirmation modal applies.
                if self.panes.len() <= 1 {
                    self.quit_requested = true;
                    return;
                }
                let Some(tile) = self.tile_for_pane(pane_id) else {
                    return;
                };
                // Heuristic for "is a program running?": alt-screen
                // is on. Phase 3's marker stream will give us a
                // more accurate "command in flight" signal; until
                // then alt-screen is the cleanest available proxy
                // (vim, less, htop, fzf all enter it).
                let running = self
                    .panes
                    .get(&pane_id)
                    .is_some_and(|s| s.session.terminal().is_alternate_screen());
                if running {
                    self.pending_close_confirm = Some(tile);
                } else {
                    self.pending_closes.push(tile);
                }
            }
            PaneAction::Quit => {
                self.quit_requested = true;
            }
        }
    }

    /// Move the active tab of `tabs_tile` by `delta` positions
    /// (wraps around). Used for Cmd+Shift+] / [.
    fn cycle_active_tab(&mut self, tabs_tile: TileId, delta: i32) {
        let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) =
            self.tree.tiles.get_mut(tabs_tile)
        else {
            return;
        };
        if tabs.children.is_empty() {
            return;
        }
        let len = tabs.children.len() as i32;
        let current =
            tabs.active.and_then(|a| tabs.children.iter().position(|c| *c == a)).unwrap_or(0)
                as i32;
        let next = ((current + delta).rem_euclid(len)) as usize;
        let new_active = tabs.children[next];
        tabs.set_active(new_active);
    }

    /// Spawn a new pane and add it as a tab inside the given Tabs
    /// container. Active-tab focus moves to the new tab so the user
    /// sees the result of clicking [+].
    ///
    /// The new pane's cwd is inherited from the Tabs container's
    /// currently active pane (its OSC 7-tracked cwd, if any). This
    /// is what users almost always want: `cd somewhere`, then
    /// Cmd+T or click `[+]` to open a sibling tab "here". If we
    /// can't resolve a cwd (the active tile isn't a pane, no OSC 7
    /// has arrived yet, …) we fall back to the termica process's
    /// cwd by leaving `PtyConfig.cwd = None`.
    fn spawn_new_pane_in_tabs(&mut self, tabs_tile: TileId) {
        let cwd = active_pane_in_tabs(&self.tree, tabs_tile)
            .and_then(|id| self.panes.get(&id))
            .and_then(|slot| slot.session.terminal().cwd().map(|p| p.to_path_buf()));

        let pane_id = self.mint_pane_id();
        let config = PtyConfig { cwd, ..PtyConfig::default() };
        let session = match PaneSession::spawn(24, 80, &config) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("termica: failed to spawn new pane: {e}");
                return;
            }
        };
        self.panes.insert(pane_id, PaneSlot { session, ui: PaneUiState::default() });

        let pane_tile = self.tree.tiles.insert_pane(pane_id);
        if let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) =
            self.tree.tiles.get_mut(tabs_tile)
        {
            tabs.add_child(pane_tile);
            tabs.set_active(pane_tile);
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
        // Ask for a redraw soon so PTY output keeps flowing even
        // when the user is idle.
        ctx.request_repaint_after(std::time::Duration::from_millis(50));

        // macOS' Cmd+Q (and the red traffic-light close button on any
        // OS) is delivered by winit as a *viewport close request*,
        // not a `Key::Q` event. Our shortcut matcher never sees it,
        // so without intercepting here the window closes immediately
        // and the quit-confirm modal never gets a chance to render.
        // We always route a close request through `quit_requested`;
        // the standard "any running?" check downstream then either
        // opens the modal or re-issues `ViewportCommand::Close` via
        // `should_quit`. The `!self.should_quit` guard prevents an
        // infinite cancel loop on the frame we re-issue Close.
        if !self.should_quit && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_requested = true;
        }

        // Drain `muda` menu events. On macOS our custom Quit menu
        // item fires `id = "quit"` here instead of calling
        // `[NSApp terminate:]`, so we get to route through the
        // standard quit-confirm flow. On other OSes the receiver
        // simply never produces an event — no menu is installed.
        #[cfg(target_os = "macos")]
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            match event.id().as_ref() {
                "quit" => self.quit_requested = true,
                "about" => self.about_open = true,
                _ => {}
            }
        }

        // Drain every pane every frame (visible or not) so background
        // tabs keep consuming PTY output rather than blocking the
        // reader thread on a full mpsc channel.
        //
        // Also reset `slot.ui.focused = false` here. `render_pane`
        // sets it from `rendered.response.has_focus()` — but a pane
        // that ISN'T the active tab in its container doesn't render
        // at all, so its stored `focused` would otherwise stay at
        // whatever the *last time it rendered* observed. End result
        // without this reset: the focused-pane snapshot at the end
        // of `update` returns a stale id, the blue tab underline
        // sticks to a pane that no longer has keyboard focus.
        // (Real egui focus is unaffected — that's stored in egui's
        // memory; only our mirror needs the reset.)
        for slot in self.panes.values_mut() {
            slot.session.drain();
            slot.ui.focused = false;
        }

        // Compute *before* `tree.ui()` so the modal's existence
        // gates this frame's pane input (otherwise keys leak to the
        // PTY before the modal renders below).
        let modal_open = self.pending_close_confirm.is_some()
            || self.quit_confirm_started_at.is_some()
            || self.about_open;
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut behavior = TabBehavior {
                panes: &mut self.panes,
                home: self.home.as_deref(),
                ctx,
                focused_pane: self.focused_pane,
                new_tab_requested_in: None,
                pending_closes: &mut self.pending_closes,
                pending_close_confirm: &mut self.pending_close_confirm,
                modal_open,
            };
            self.tree.ui(&mut behavior, ui);
            self.new_tab_requested_in = behavior.new_tab_requested_in;
        });

        // Apply staged "new tab" requests.
        if let Some(tabs_tile) = self.new_tab_requested_in.take() {
            self.spawn_new_pane_in_tabs(tabs_tile);
        }

        // Apply app-level shortcuts from this frame's focused pane
        // (Cmd+T, Cmd+W, Cmd+Q, Cmd+Shift+]/[). Drain the per-slot
        // intent first so we don't hold a `&mut self.panes` borrow
        // when we then dispatch into other `&mut self` methods.
        let pending_actions: Vec<(PaneId, PaneAction)> = self
            .panes
            .iter_mut()
            .filter_map(|(id, slot)| slot.ui.pending_action.take().map(|a| (*id, a)))
            .collect();
        for (pane_id, action) in pending_actions {
            self.apply_pane_action(pane_id, action);
        }

        // Close-tab confirmation modal: a running tab whose close
        // was requested (via Cmd+W *or* the tab strip's X button)
        // is queued here; the modal renders below, Yes pushes the
        // tile into `pending_closes`, No clears the field.
        if let Some(tile_id) = self.pending_close_confirm {
            let mut decision: Option<bool> = None;
            let modal = egui::Modal::new(egui::Id::new(("termica-close-confirm", tile_id))).show(
                ctx,
                |ui| {
                    ui.set_min_width(360.0);
                    ui.vertical_centered(|ui| {
                        ui.heading("Close this tab?");
                        ui.add_space(6.0);
                        ui.label("A program is still running in this tab.");
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.add_space(60.0);
                            if ui.button("Cancel").clicked() {
                                decision = Some(false);
                            }
                            ui.add_space(8.0);
                            if ui.button(egui::RichText::new("Close").strong()).clicked() {
                                decision = Some(true);
                            }
                        });
                    });
                },
            );

            if modal.should_close() {
                decision = Some(false);
            }

            match decision {
                Some(true) => {
                    self.pending_closes.push(tile_id);
                    self.pending_close_confirm = None;
                }
                Some(false) => {
                    self.pending_close_confirm = None;
                }
                None => {}
            }
        }

        // Two-phase quit:
        //
        // 1. `apply_pane_action` set `self.quit_requested = true`.
        //    Here we decide whether to exit immediately or open
        //    the modal — *after* the tree mutations (pending closes
        //    etc.) have settled, so the alt-screen check sees the
        //    real current state. Notably: Cmd+W on the last tab
        //    routes through `quit_requested`, so this is where the
        //    "no panes left → just exit" branch lives.
        //
        // 2. If a confirmation is needed, `quit_confirm_started_at`
        //    is stamped with `ctx.time` and the modal renders from
        //    then on, counting down.
        if self.quit_requested {
            self.quit_requested = false;
            let any_running =
                self.panes.values().any(|s| s.session.terminal().is_alternate_screen());
            if !any_running {
                self.should_quit = true;
            } else if self.quit_confirm_started_at.is_none() {
                self.quit_confirm_started_at = Some(ctx.input(|i| i.time));
            }
        }

        // Quit-confirmation modal: countdown + Cancel / Quit
        // buttons; Esc or backdrop click cancels (via egui::Modal's
        // `should_close`). Auto-exits after QUIT_CONFIRM_TIMEOUT_SECS
        // so a forgotten dialog doesn't pin the app open.
        if let Some(started_at) = self.quit_confirm_started_at {
            let now = ctx.input(|i| i.time);
            let elapsed = (now - started_at).max(0.0);
            let remaining = (QUIT_CONFIRM_TIMEOUT_SECS - elapsed).max(0.0);

            let mut decision: Option<bool> = None;
            let modal = egui::Modal::new(egui::Id::new("termica-quit-confirm")).show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.vertical_centered(|ui| {
                    ui.heading("Quit Termica?");
                    ui.add_space(6.0);
                    ui.label("A program is still running in one or more panes.");
                    ui.label(format!("Auto-quit in {:.0}s if there's no response.", remaining));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.add_space(60.0);
                        if ui.button("Cancel").clicked() {
                            decision = Some(false);
                        }
                        ui.add_space(8.0);
                        if ui.button(egui::RichText::new("Quit").strong()).clicked() {
                            decision = Some(true);
                        }
                    });
                });
            });

            // Esc or backdrop click → cancel.
            if modal.should_close() {
                decision = Some(false);
            }

            // Keep ticking so the countdown updates ~4× per second.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));

            if remaining <= 0.0 {
                decision = Some(true);
            }

            match decision {
                Some(true) => {
                    self.quit_confirm_started_at = None;
                    self.should_quit = true;
                }
                Some(false) => {
                    self.quit_confirm_started_at = None;
                }
                None => {}
            }
        }

        if self.should_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // About modal: opened by the menubar's About item (macOS)
        // and by any future surface that sets `about_open = true`.
        // Esc / backdrop / OK dismisses. Pane input is already gated
        // because `about_open` feeds into the `modal_open` flag
        // computed at the top of this frame.
        if self.about_open {
            let modal = egui::Modal::new(egui::Id::new("termica-about")).show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.vertical_centered(|ui| {
                    ui.heading("Termica");
                    ui.add_space(4.0);
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(8.0);
                    ui.label("A native terminal workspace.");
                    ui.add_space(12.0);
                    if ui.button("OK").clicked() {
                        self.about_open = false;
                    }
                });
            });
            if modal.should_close() {
                self.about_open = false;
            }
        }

        // Apply staged tab-close requests via `remove_recursively`,
        // which (unlike `tiles.remove()`) cleans up the parent Tabs
        // container's `children` and `active` references. The
        // default egui_tiles close path leaves a stale `active`
        // pointing at the just-removed tile, which blanks the pane
        // area — hence we intercept on_tab_close and finish it here.
        //
        // Refocus rule: if the pane that held keyboard focus was
        // among those just closed, focus moves to the **most-
        // recently-focused live** pane (via `focus_history`). The
        // pane's parent Tabs.active is updated and its
        // `needs_focus` flag set so `render_pane` claims focus on
        // its next render. Without this, the defensive validation
        // below would set the parent's active to the *first* live
        // child of the Tabs container — typically not what the user
        // wants.
        let closed_pane_ids: HashSet<PaneId> = self
            .pending_closes
            .iter()
            .filter_map(|tid| match self.tree.tiles.get(*tid) {
                Some(Tile::Pane(p)) => Some(*p),
                _ => None,
            })
            .collect();
        let focused_was_closed = self.focused_pane.is_some_and(|p| closed_pane_ids.contains(&p));
        for tile_id in std::mem::take(&mut self.pending_closes) {
            self.tree.remove_recursively(tile_id);
        }
        self.focus_history.retain(|p| !closed_pane_ids.contains(p));
        if focused_was_closed && let Some(&new_focus) = self.focus_history.first() {
            if let Some(pane_tile) = self.tile_for_pane(new_focus)
                && let Some(parent) = self.tree.tiles.parent_of(pane_tile)
                && let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) =
                    self.tree.tiles.get_mut(parent)
            {
                tabs.set_active(pane_tile);
            }
            if let Some(slot) = self.panes.get_mut(&new_focus) {
                slot.ui.needs_focus = true;
            }
        }

        // Defensive: every Tabs container must have an `active`
        // that is **a real child of its own**. We've observed three
        // ways this invariant can be violated:
        //
        //   1. `active = None` — fresh container with no selection.
        //   2. `active = Some(t)` where `t` was removed from `tiles`.
        //   3. `active = Some(t)` where `t` exists but is NOT in
        //      this container's `children` (e.g. it's a sibling
        //      Tabs container's tile). This one bites after a
        //      drag-split: egui_tiles 0.14 leaves the source
        //      container's `active` pointing at the drop target.
        //
        // In all three cases, adopt the first live child as active
        // so the pane area paints something rather than nothing.
        let live_tile_ids: HashSet<TileId> = self.tree.tiles.tile_ids().collect();
        for tile in self.tree.tiles.tiles_mut() {
            if let Tile::Container(egui_tiles::Container::Tabs(tabs)) = tile {
                let active_is_own_live_child = tabs
                    .active
                    .is_some_and(|t| live_tile_ids.contains(&t) && tabs.children.contains(&t));
                if !active_is_own_live_child
                    && let Some(first) = tabs.children.iter().find(|t| live_tile_ids.contains(t))
                {
                    tabs.set_active(*first);
                }
            }
        }

        // Detect which panes are the active tab in their respective
        // Tabs container *this* frame. Any pane that's newly active
        // (wasn't active last frame) gets `needs_focus = true` so
        // it grabs keyboard focus on its next render. This covers
        // three gestures uniformly:
        //   - user clicks a tab title,
        //   - drag-drop creates a new region whose active pane is
        //     fresh,
        //   - [+] spawns a new tab that becomes active.
        let mut now_active: HashSet<PaneId> = HashSet::new();
        for (_, tile) in self.tree.tiles.iter() {
            if let Tile::Container(egui_tiles::Container::Tabs(tabs)) = tile
                && let Some(active_tile) = tabs.active
                && let Some(Tile::Pane(pid)) = self.tree.tiles.get(active_tile)
            {
                now_active.insert(*pid);
            }
        }
        for pid in now_active.difference(&self.prev_active_panes) {
            if let Some(slot) = self.panes.get_mut(pid) {
                slot.ui.needs_focus = true;
            }
        }
        self.prev_active_panes = now_active;

        // Snapshot which pane (if any) holds keyboard focus for the
        // next frame's tab styling. `slot.ui.focused` was written
        // by `render_pane` during this frame.
        self.focused_pane = self.panes.iter().find(|(_, s)| s.ui.focused).map(|(id, _)| *id);

        // Maintain `focus_history` — front is most-recently-focused.
        // Read by the close-and-refocus block above on the *next*
        // frame: when a focused tab is closed, we restore focus to
        // the second entry here (the pane the user was on before
        // they switched to the tab they just closed).
        if let Some(focused) = self.focused_pane
            && self.focus_history.first() != Some(&focused)
        {
            self.focus_history.retain(|p| *p != focused);
            self.focus_history.insert(0, focused);
        }

        // Garbage-collect panes whose tiles are no longer in the
        // tree (removed via `pending_closes`). Drops their
        // PaneSession, closing the PTY and ending the reader thread.
        let mut live_panes: HashSet<PaneId> = HashSet::new();
        for (_, tile) in self.tree.tiles.iter() {
            if let Tile::Pane(id) = tile {
                live_panes.insert(*id);
            }
        }
        self.panes.retain(|id, _| live_panes.contains(id));
    }
}

// =====================================================================
//  egui_tiles::Behavior impl
// =====================================================================

/// Per-frame Behavior shim. Holds borrows into [`TermicaApp`] so
/// the Behavior callbacks can read pane state, draw pane UI, and
/// stage tree mutations (deferred to after `tree.ui()`).
struct TabBehavior<'a> {
    panes: &'a mut HashMap<PaneId, PaneSlot>,
    home: Option<&'a Path>,
    ctx: &'a egui::Context,
    /// The pane id whose keyboard focus we should advertise via tab
    /// styling. Set by [`TermicaApp::update`] from the previous
    /// frame's `slot.ui.focused`; one-frame lag is acceptable.
    focused_pane: Option<PaneId>,
    /// Set when the user clicks [+] in a tab strip. Read by
    /// [`TermicaApp::update`] after `tree.ui()` returns.
    new_tab_requested_in: Option<TileId>,
    /// Tab close requests staged by `on_tab_close`. Drained and
    /// applied by [`TermicaApp::update`] via [`Tree::remove_recursively`]
    /// so the parent's `children` and `active` references get
    /// cleaned up properly.
    pending_closes: &'a mut Vec<TileId>,
    /// Same idea, but for tiles whose pane is running a program —
    /// the close goes through the confirmation modal first.
    pending_close_confirm: &'a mut Option<TileId>,
    /// True when ANY modal (close-confirm or quit-confirm) is
    /// currently showing. Forwarded to [`render_pane`] which uses it
    /// to render the pane as inert: no keyboard to the PTY, no
    /// wheel, no mouse selection, no focus grab. Without this, keys
    /// the user thought were going to the modal (Enter, Esc, arrows)
    /// reach the PTY because pane input is read before the modal
    /// renders later in the frame.
    modal_open: bool,
}

impl<'a> Behavior<PaneId> for TabBehavior<'a> {
    fn tab_title_for_pane(&mut self, pane_id: &PaneId) -> egui::WidgetText {
        let cwd = self.panes.get(pane_id).and_then(|s| s.session.terminal().cwd());
        tab_title_for(*pane_id, cwd, self.home).into()
    }

    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile_id: TileId, pane_id: &mut PaneId) -> UiResponse {
        let Some(slot) = self.panes.get_mut(pane_id) else {
            ui.label(format!("(pane {} gone)", pane_id.0));
            return UiResponse::None;
        };
        // Salt every widget id under this pane by `PaneId` so a
        // pane that ends up referenced from multiple tree slots —
        // egui_tiles is permissive about this; we don't think it
        // happens in our flow but the cost of being safe is zero —
        // never collides on auto-generated ids. CLAUDE.md spells
        // out the rule: assume any UI element can render twice in
        // a frame and salt accordingly.
        let modal_open = self.modal_open;
        ui.push_id(("pane", pane_id.0), |ui| {
            render_pane(ui, self.ctx, slot, self.home, modal_open);
        });
        UiResponse::None
    }

    fn is_tab_closable(&self, tiles: &Tiles<PaneId>, _tile_id: TileId) -> bool {
        // Don't allow closing the very last tab — that would leave
        // the app with no pane and no obvious way for the user to
        // recover. A future PR can change this to either auto-spawn
        // a new pane or quit the app on last-close.
        tiles.iter().filter(|(_, t)| matches!(t, Tile::Pane(_))).count() > 1
    }

    fn on_tab_close(&mut self, tiles: &mut Tiles<PaneId>, tile_id: TileId) -> bool {
        // **Don't** let egui_tiles call `tiles.remove(tile_id)` —
        // that path doesn't clean up the parent Tabs container's
        // `children` / `active` references, which leaves a stale
        // pointer that blanks the pane area. Either route into the
        // modal (if a program is running) or defer the removal to
        // [`TermicaApp::update`] which uses
        // [`Tree::remove_recursively`] (the cleanup-correct path).
        let running = matches!(tiles.get(tile_id), Some(Tile::Pane(pid))
            if self.panes.get(pid).is_some_and(|s| s.session.terminal().is_alternate_screen()));
        if running {
            *self.pending_close_confirm = Some(tile_id);
        } else {
            self.pending_closes.push(tile_id);
        }
        false
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        // By default egui_tiles prunes Tabs containers with only one
        // child, which silently removes our tab strip whenever there's
        // exactly one pane. We want the tab strip to ALWAYS be visible
        // (so the `[+]` button is reachable and the user has a clear
        // affordance for tabs even from the empty state) — hence
        // `all_panes_must_have_tabs = true`, which the docs say wins
        // over `prune_single_child_tabs`.
        egui_tiles::SimplificationOptions { all_panes_must_have_tabs: true, ..Default::default() }
    }

    fn top_bar_right_ui(
        &mut self,
        _tiles: &Tiles<PaneId>,
        ui: &mut egui::Ui,
        tile_id: TileId,
        _tabs: &egui_tiles::Tabs,
        _scroll_offset: &mut f32,
    ) {
        // Stage the "new tab in this Tabs container" intent. The
        // app applies it after `tree.ui()` so we don't mutate the
        // tree from inside a Behavior callback.
        if ui.button("+").on_hover_text("New tab").clicked() {
            self.new_tab_requested_in = Some(tile_id);
        }
    }

    // ---- Knauty-style tab styling --------------------------------
    //
    // Stay close to egui_tiles' defaults so the chrome doesn't
    // shout. The only departures:
    //
    //   - Inactive tabs render their title in italic. Mirrors
    //     Knauty's convention and makes "this is not the active
    //     tab in this region" immediately legible.
    //   - The active tab WHOSE PANE HOLDS KEYBOARD FOCUS renders
    //     its title in bold. Same idea — subtle, no extra paint,
    //     just font weight. With multiple split regions, the bold
    //     tab tells you where your typing will land.
    //
    // No accent-colored outline / background tint. The previous
    // version was too loud per UX review.

    fn tab_title_for_tile(&mut self, tiles: &Tiles<PaneId>, tile_id: TileId) -> egui::WidgetText {
        let Some(Tile::Pane(pane_id)) = tiles.get(tile_id) else {
            return "?".into();
        };
        let cwd = self.panes.get(pane_id).and_then(|s| s.session.terminal().cwd());
        // No styling on the text itself any more. Active-in-container
        // is already differentiated by egui_tiles' default `tab_ui`
        // (brighter bg + connecting hline). The focused-for-the-whole-
        // app indicator is the blue bottom border painted in
        // [`on_tab_button`] below.
        egui::RichText::new(tab_title_for(*pane_id, cwd, self.home)).into()
    }

    fn on_tab_button(
        &mut self,
        tiles: &Tiles<PaneId>,
        tile_id: TileId,
        response: egui::Response,
    ) -> egui::Response {
        // Tab click → focus its pane. egui_tiles changes the parent
        // Tabs.active only when the clicked tab is a *different*
        // tile, so clicking the already-active tab would otherwise
        // be a no-op — but the user expects clicking any tab to
        // bring keyboard focus to its pane, exactly like clicking
        // the pane's visible body does. We can't rely on the
        // `now_active.difference(prev_active)` diff in `update`
        // for this case because active didn't change.
        if response.clicked()
            && let Some(Tile::Pane(pane_id)) = tiles.get(tile_id)
            && let Some(slot) = self.panes.get_mut(pane_id)
        {
            slot.ui.needs_focus = true;
        }

        // Paint a blue underline on the tab that owns app-wide
        // keyboard focus. With multiple split regions each showing
        // their own active tab, this is what tells the user "your
        // typing lands HERE" at a glance. We paint into the same
        // layer the tab was drawn on (via `layer_painter`) and on
        // top of egui_tiles' own painted geometry — `on_tab_button`
        // runs after the default `tab_ui` returns.
        let is_focused = matches!(tiles.get(tile_id), Some(Tile::Pane(pid))
            if self.focused_pane == Some(*pid));
        if is_focused {
            paint_focused_tab_underline(&response);
        }
        response
    }
}

/// Paint the blue underline that marks the app-wide focused tab.
/// Public + free-standing so snapshot tests can apply the same
/// paint to a synthetic `egui_tiles::Tree` without standing up a
/// full [`TermicaApp`]; the production caller is
/// [`TabBehavior::on_tab_button`]. Single source of truth for the
/// focus visual.
pub fn paint_focused_tab_underline(response: &egui::Response) {
    let painter = response.ctx.layer_painter(response.layer_id);
    let rect = response.rect;
    let color = response.ctx.style().visuals.selection.bg_fill;
    let stroke = egui::Stroke::new(2.5, color);
    let y = rect.bottom() - 1.25;
    painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], stroke);
}

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
            install_macos_menu();
            Ok(Box::new(TermicaApp::new()))
        }),
    )
}

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
#[cfg(target_os = "macos")]
fn install_macos_menu() {
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

#[cfg(test)]
mod tests {
    //! Pure-logic tests for the resize math + tab-title rule. The
    //! UI wiring is exercised by snapshot tests in `tests/snapshots.rs`.

    use super::*;

    #[test]
    fn cells_from_pixels_floors_dimensions() {
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(800.0, 400.0), 10.0, 20.0);
        assert_eq!((rows, cols), (20, 80));
    }

    #[test]
    fn cells_from_pixels_ignores_fractional_remainder() {
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(805.0, 405.0), 10.0, 20.0);
        assert_eq!((rows, cols), (20, 80));
    }

    #[test]
    fn cells_from_pixels_clamps_to_minimum() {
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(1.0, 1.0), 10.0, 20.0);
        assert_eq!((rows, cols), (MIN_ROWS, MIN_COLS));
    }

    #[test]
    fn cells_from_pixels_handles_zero_metrics() {
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(800.0, 400.0), 0.0, 20.0);
        assert_eq!(cols, MIN_COLS);
        let (rows2, _cols2) = cells_from_pixels(egui::Vec2::new(800.0, 400.0), 10.0, 0.0);
        assert_eq!(rows2, MIN_ROWS);
        let _ = rows;
    }

    // --- tab title rule ----------------------------------------------

    #[test]
    fn tab_title_falls_back_to_pane_n_when_cwd_unknown() {
        assert_eq!(tab_title_for(PaneId(3), None, None), "pane 3");
    }

    #[test]
    fn tab_title_uses_full_cwd_when_outside_home() {
        let cwd = PathBuf::from("/tmp");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "/tmp");
    }

    #[test]
    fn tab_title_uses_full_cwd_when_home_unknown() {
        // Short cwd so the truncation rule doesn't kick in and
        // obscure what this test is asserting.
        let cwd = PathBuf::from("/Users/tim/git");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), None), "/Users/tim/git");
    }

    #[test]
    fn tab_title_substitutes_tilde_when_cwd_is_home() {
        let cwd = PathBuf::from("/Users/tim");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "~");
    }

    #[test]
    fn tab_title_substitutes_tilde_with_subpath() {
        let cwd = PathBuf::from("/Users/tim/git/enthal/termica");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "~/git/enthal/termica");
    }

    #[test]
    fn tab_title_tolerates_trailing_slash_on_home() {
        let cwd = PathBuf::from("/Users/tim/git/enthal/termica");
        let home = PathBuf::from("/Users/tim/");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "~/git/enthal/termica");
    }

    #[test]
    fn tab_title_tolerates_trailing_slash_on_cwd_at_home() {
        let cwd = PathBuf::from("/Users/tim/");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "~");
    }

    #[test]
    fn tab_title_does_not_match_sibling_user_dir_as_home() {
        // The bug we're guarding against: HOME=/Users/tim must NOT
        // match `/Users/timjones` as a "tim/jones" subdir and yield
        // `~jones`. The next char after the prefix must be a `/`.
        let cwd = PathBuf::from("/Users/timjones/work");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "/Users/timjones/work");
    }

    #[test]
    fn tab_title_returns_root_as_root() {
        // `/` as the cwd renders as `/`, not as a fallback.
        let cwd = PathBuf::from("/");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(7), Some(&cwd), Some(&home)), "/");
    }

    #[test]
    fn tab_title_unchanged_when_at_max_length() {
        // Exactly 25 chars: `~/` (2) + 23-char dir name.
        let cwd = PathBuf::from("/Users/tim/abcdefghijklmnopqrstuvw");
        let home = PathBuf::from("/Users/tim");
        let title = tab_title_for(PaneId(0), Some(&cwd), Some(&home));
        assert_eq!(title.chars().count(), MAX_TAB_TITLE_CHARS);
        assert_eq!(title, "~/abcdefghijklmnopqrstuvw");
    }

    #[test]
    fn tab_title_truncates_long_titles_with_leading_ellipsis() {
        // 42-char `~`-substituted title → expect 25 chars with `..`
        // prefix and the last 23 chars retained.
        let cwd = PathBuf::from("/Users/tim/very/long/path/with/lots/of/subdirs/here");
        let home = PathBuf::from("/Users/tim");
        let title = tab_title_for(PaneId(0), Some(&cwd), Some(&home));
        assert_eq!(title.chars().count(), MAX_TAB_TITLE_CHARS);
        assert!(title.starts_with(".."));
        // Tail preserved — the user cares about the deepest dir.
        assert!(title.ends_with("/here"));
    }

    #[test]
    fn tab_title_truncates_unsubstituted_paths_too() {
        // Outside `$HOME`, same rule applies.
        let cwd = PathBuf::from("/var/log/very/long/path/under/system/dirs/please");
        let home = PathBuf::from("/Users/tim");
        let title = tab_title_for(PaneId(0), Some(&cwd), Some(&home));
        assert_eq!(title.chars().count(), MAX_TAB_TITLE_CHARS);
        assert!(title.starts_with(".."));
    }

    #[test]
    fn tab_title_truncation_is_char_safe_for_multibyte() {
        // A name longer than 25 with multi-byte chars must not be
        // sliced mid-codepoint. Using accented chars: each `é` is
        // 2 bytes but 1 char; `truncate_tab_title` counts in chars.
        let s = "/Users/tim/répertoire-très-long-avec-accents-partout/";
        let cwd = PathBuf::from(s);
        let home = PathBuf::from("/Users/tim");
        let title = tab_title_for(PaneId(0), Some(&cwd), Some(&home));
        assert_eq!(title.chars().count(), MAX_TAB_TITLE_CHARS);
        // If we'd sliced bytes, this would have panicked above.
    }

    // --- active pane resolution (Phase 2B spawn-in-cwd) -------------
    //
    // These exercise the pure Tree navigation under
    // [`active_pane_in_tabs`]. The "spawn in cwd" wiring on top of
    // it (turn a pane id into its OSC 7 cwd, hand to PtyConfig) is
    // covered by the OSC 7 tracking tests and end-to-end UX use.

    #[test]
    fn active_pane_in_tabs_returns_default_active_for_single_child() {
        let mut tree = Tree::<PaneId>::empty("test-single");
        let pane_tile = tree.tiles.insert_pane(PaneId(1));
        let tabs_tile = tree.tiles.insert_tab_tile(vec![pane_tile]);
        // Tabs::new sets active = first child.
        assert_eq!(active_pane_in_tabs(&tree, tabs_tile), Some(PaneId(1)));
    }

    #[test]
    fn active_pane_in_tabs_returns_explicit_active() {
        let mut tree = Tree::<PaneId>::empty("test-multi");
        let pane_a = tree.tiles.insert_pane(PaneId(10));
        let pane_b = tree.tiles.insert_pane(PaneId(20));
        let tabs_tile = tree.tiles.insert_tab_tile(vec![pane_a, pane_b]);
        if let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) =
            tree.tiles.get_mut(tabs_tile)
        {
            tabs.set_active(pane_b);
        }
        assert_eq!(active_pane_in_tabs(&tree, tabs_tile), Some(PaneId(20)));
    }

    #[test]
    fn active_pane_in_tabs_returns_none_for_empty_tabs() {
        let mut tree = Tree::<PaneId>::empty("test-empty");
        let tabs_tile = tree.tiles.insert_tab_tile(vec![]);
        assert_eq!(active_pane_in_tabs(&tree, tabs_tile), None);
    }

    #[test]
    fn active_pane_in_tabs_returns_none_when_tile_is_not_tabs() {
        let mut tree = Tree::<PaneId>::empty("test-non-tabs");
        let pane_tile = tree.tiles.insert_pane(PaneId(5));
        // The pane tile itself isn't a Tabs container.
        assert_eq!(active_pane_in_tabs(&tree, pane_tile), None);
    }

    #[test]
    fn active_pane_in_tabs_returns_none_for_dangling_tile_id() {
        // `Tree::empty` gives us a tree with no tiles; any `TileId`
        // we manufacture from `Default` won't exist in `tiles`.
        let tree = Tree::<PaneId>::empty("test-dangling");
        assert_eq!(active_pane_in_tabs(&tree, TileId::from_u64(9999)), None);
    }

    // --- app-level shortcut mapping ---------------------------------

    // macOS conventions: Cmd held; Shift only on bracket-nav.
    fn mac_cmd_only() -> egui::Modifiers {
        egui::Modifiers { mac_cmd: true, command: true, ..egui::Modifiers::default() }
    }
    fn mac_cmd_shift() -> egui::Modifiers {
        egui::Modifiers { mac_cmd: true, command: true, shift: true, ..egui::Modifiers::default() }
    }

    // Linux/Windows conventions: Ctrl+Shift held for all app shortcuts.
    fn linux_ctrl_shift() -> egui::Modifiers {
        egui::Modifiers { ctrl: true, shift: true, command: true, ..egui::Modifiers::default() }
    }

    #[test]
    fn macos_cmd_t_maps_to_new_tab() {
        assert_eq!(
            match_pane_shortcut(egui::Key::T, mac_cmd_only(), true),
            Some(PaneAction::NewTab)
        );
    }

    #[test]
    fn macos_cmd_w_maps_to_close_tab() {
        assert_eq!(
            match_pane_shortcut(egui::Key::W, mac_cmd_only(), true),
            Some(PaneAction::CloseTab)
        );
    }

    #[test]
    fn macos_cmd_q_maps_to_quit() {
        assert_eq!(match_pane_shortcut(egui::Key::Q, mac_cmd_only(), true), Some(PaneAction::Quit));
    }

    #[test]
    fn macos_cmd_shift_close_bracket_accepts_either_bracket_variant() {
        // With Shift held on a US keyboard, `]` becomes `}` and
        // egui reports `CloseCurlyBracket`. Both variants should
        // map to NextTab so the physical chord works regardless
        // of layout transliteration.
        assert_eq!(
            match_pane_shortcut(egui::Key::CloseBracket, mac_cmd_shift(), true),
            Some(PaneAction::NextTab)
        );
        assert_eq!(
            match_pane_shortcut(egui::Key::CloseCurlyBracket, mac_cmd_shift(), true),
            Some(PaneAction::NextTab)
        );
    }

    #[test]
    fn macos_cmd_shift_open_bracket_accepts_either_variant() {
        assert_eq!(
            match_pane_shortcut(egui::Key::OpenBracket, mac_cmd_shift(), true),
            Some(PaneAction::PrevTab)
        );
        assert_eq!(
            match_pane_shortcut(egui::Key::OpenCurlyBracket, mac_cmd_shift(), true),
            Some(PaneAction::PrevTab)
        );
    }

    #[test]
    fn macos_cmd_only_brackets_do_not_navigate_tabs() {
        // Cmd+] without Shift isn't ours. Must be None.
        assert_eq!(match_pane_shortcut(egui::Key::CloseBracket, mac_cmd_only(), true), None);
    }

    #[test]
    fn macos_plain_t_is_not_a_shortcut() {
        assert_eq!(match_pane_shortcut(egui::Key::T, egui::Modifiers::default(), true), None);
    }

    #[test]
    fn macos_cmd_t_with_extra_modifier_is_not_recognised() {
        let cmd_alt = egui::Modifiers {
            mac_cmd: true,
            command: true,
            alt: true,
            ..egui::Modifiers::default()
        };
        assert_eq!(match_pane_shortcut(egui::Key::T, cmd_alt, true), None);
    }

    // --- Linux / Windows shortcuts ---------------------------------

    #[test]
    fn linux_ctrl_shift_t_maps_to_new_tab() {
        assert_eq!(
            match_pane_shortcut(egui::Key::T, linux_ctrl_shift(), false),
            Some(PaneAction::NewTab)
        );
    }

    #[test]
    fn linux_ctrl_shift_w_maps_to_close_tab() {
        assert_eq!(
            match_pane_shortcut(egui::Key::W, linux_ctrl_shift(), false),
            Some(PaneAction::CloseTab)
        );
    }

    #[test]
    fn linux_ctrl_shift_q_maps_to_quit() {
        assert_eq!(
            match_pane_shortcut(egui::Key::Q, linux_ctrl_shift(), false),
            Some(PaneAction::Quit)
        );
    }

    #[test]
    fn linux_ctrl_shift_brackets_navigate_tabs() {
        assert_eq!(
            match_pane_shortcut(egui::Key::CloseCurlyBracket, linux_ctrl_shift(), false),
            Some(PaneAction::NextTab)
        );
        assert_eq!(
            match_pane_shortcut(egui::Key::OpenCurlyBracket, linux_ctrl_shift(), false),
            Some(PaneAction::PrevTab)
        );
    }

    #[test]
    fn linux_plain_ctrl_t_is_not_a_shortcut() {
        // Critical: plain Ctrl+T must NOT spawn a tab on Linux.
        // It's a legitimate readline binding (`transpose-chars`)
        // and must pass through to the shell.
        let ctrl_only = egui::Modifiers { ctrl: true, command: true, ..egui::Modifiers::default() };
        assert_eq!(match_pane_shortcut(egui::Key::T, ctrl_only, false), None);
    }

    #[test]
    fn linux_plain_ctrl_q_is_not_a_shortcut() {
        // Same reasoning — plain Ctrl+Q is shell flow control (XON).
        let ctrl_only = egui::Modifiers { ctrl: true, command: true, ..egui::Modifiers::default() };
        assert_eq!(match_pane_shortcut(egui::Key::Q, ctrl_only, false), None);
    }

    #[test]
    fn linux_cmd_t_is_not_recognised() {
        // Cross-platform safety: on Linux, the Mac-only `mac_cmd`
        // flag (which winit doesn't emit) shouldn't accidentally
        // activate an app shortcut.
        let mac_cmd =
            egui::Modifiers { mac_cmd: true, command: true, ..egui::Modifiers::default() };
        assert_eq!(match_pane_shortcut(egui::Key::T, mac_cmd, false), None);
    }

    #[test]
    fn macos_ctrl_shift_t_is_not_recognised() {
        // Cross-platform safety: on macOS, the Linux combo should
        // NOT also trigger (avoid double-binding).
        let ctrl_shift = egui::Modifiers {
            ctrl: true,
            shift: true,
            command: true,
            ..egui::Modifiers::default()
        };
        assert_eq!(match_pane_shortcut(egui::Key::T, ctrl_shift, true), None);
    }

    // --- pane id uniqueness ------------------------------------------

    #[test]
    fn pane_ids_are_monotonic_and_unique() {
        // Direct test of the mint policy: each call returns the
        // next u64, IDs never reuse.
        let mut next = 0u64;
        let mut mint = || {
            let id = PaneId(next);
            next += 1;
            id
        };
        let a = mint();
        let b = mint();
        let c = mint();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_eq!(a.0 + 1, b.0);
        assert_eq!(b.0 + 1, c.0);
    }
}
