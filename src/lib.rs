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

    if primary_pressed
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
    } else if rendered.response.dragged()
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
    if needs_focus || nothing_focused {
        rendered.response.request_focus();
    }

    // Record focus state for the next frame's tab styling.
    // `has_focus` is read by [`TermicaApp::update`] after
    // `tree.ui()` returns; the focused pane's tab title is then
    // rendered bold via the Behavior.
    slot.ui.focused = rendered.response.has_focus();

    // ---- keyboard input, focus-gated ----------------------------
    if rendered.response.has_focus() {
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
    if rendered.response.hovered() {
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

/// Compute the tab title for a pane: cwd basename if the shell has
/// reported an OSC-7 cwd, else a fallback `pane N`. Pure function
/// so the rule is unit-testable without any egui plumbing.
pub fn tab_title_for(pane_id: PaneId, cwd: Option<&Path>) -> String {
    if let Some(c) = cwd
        && let Some(name) = c.file_name()
    {
        return name.to_string_lossy().into_owned();
    }
    format!("pane {}", pane_id.0)
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
}

impl TermicaApp {
    /// Construct an app with one initial pane in a single Tabs
    /// container at the root of the tree.
    pub fn new() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut app = Self {
            panes: HashMap::new(),
            tree: Tree::empty("termica-tree"),
            next_pane_id: 0,
            home,
            new_tab_requested_in: None,
            pending_closes: Vec::new(),
            focused_pane: None,
            prev_active_panes: HashSet::new(),
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

    /// Spawn a new pane and add it as a tab inside the given Tabs
    /// container. Active-tab focus moves to the new tab so the user
    /// sees the result of clicking [+].
    fn spawn_new_pane_in_tabs(&mut self, tabs_tile: TileId) {
        let pane_id = self.mint_pane_id();
        let session = match PaneSession::spawn(24, 80, &PtyConfig::default()) {
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

        // Drain every pane every frame (visible or not) so background
        // tabs keep consuming PTY output rather than blocking the
        // reader thread on a full mpsc channel.
        for slot in self.panes.values_mut() {
            slot.session.drain();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let mut behavior = TabBehavior {
                panes: &mut self.panes,
                home: self.home.as_deref(),
                ctx,
                focused_pane: self.focused_pane,
                new_tab_requested_in: None,
                pending_closes: &mut self.pending_closes,
            };
            self.tree.ui(&mut behavior, ui);
            self.new_tab_requested_in = behavior.new_tab_requested_in;
        });

        // Apply staged "new tab" requests.
        if let Some(tabs_tile) = self.new_tab_requested_in.take() {
            self.spawn_new_pane_in_tabs(tabs_tile);
        }

        // Apply staged tab-close requests via `remove_recursively`,
        // which (unlike `tiles.remove()`) cleans up the parent Tabs
        // container's `children` and `active` references. The
        // default egui_tiles close path leaves a stale `active`
        // pointing at the just-removed tile, which blanks the pane
        // area — hence we intercept on_tab_close and finish it here.
        for tile_id in std::mem::take(&mut self.pending_closes) {
            self.tree.remove_recursively(tile_id);
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
}

impl<'a> Behavior<PaneId> for TabBehavior<'a> {
    fn tab_title_for_pane(&mut self, pane_id: &PaneId) -> egui::WidgetText {
        let cwd = self.panes.get(pane_id).and_then(|s| s.session.terminal().cwd());
        tab_title_for(*pane_id, cwd).into()
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
        ui.push_id(("pane", pane_id.0), |ui| {
            render_pane(ui, self.ctx, slot, self.home);
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

    fn on_tab_close(&mut self, _tiles: &mut Tiles<PaneId>, tile_id: TileId) -> bool {
        // **Don't** let egui_tiles call `tiles.remove(tile_id)` —
        // that path doesn't clean up the parent Tabs container's
        // `children` / `active` references, which leaves a stale
        // pointer that blanks the pane area. Defer the removal to
        // [`TermicaApp::update`] which uses
        // [`Tree::remove_recursively`] (the cleanup-correct path).
        self.pending_closes.push(tile_id);
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
        let title = tab_title_for(*pane_id, cwd);

        // Is this tile the active tab in its parent Tabs container?
        let is_active = tiles
            .parent_of(tile_id)
            .and_then(|p| tiles.get(p))
            .is_some_and(|t| matches!(t, Tile::Container(egui_tiles::Container::Tabs(tabs)) if tabs.active == Some(tile_id)));
        let is_focused = self.focused_pane == Some(*pane_id);

        let mut rich = egui::RichText::new(title);
        if !is_active {
            rich = rich.italics();
        }
        if is_focused {
            rich = rich.strong();
        }
        rich.into()
    }
}

/// Run the native window. Used by `main` and any future end-to-end
/// harness.
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
    fn tab_title_uses_cwd_basename_when_known() {
        let cwd = PathBuf::from("/Users/tim/git/enthal/termica");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd)), "termica");
    }

    #[test]
    fn tab_title_falls_back_to_pane_n_when_cwd_unknown() {
        assert_eq!(tab_title_for(PaneId(3), None), "pane 3");
    }

    #[test]
    fn tab_title_falls_back_when_cwd_has_no_basename() {
        // Filesystem root — file_name() returns None. We fall back.
        let root = PathBuf::from("/");
        assert_eq!(tab_title_for(PaneId(7), Some(&root)), "pane 7");
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
