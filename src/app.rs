//! The top-level eframe application.
//!
//! Owns the layout tree + every live pane. egui_tiles owns the
//! topology; we own the pane data so the tree only stores cheap
//! [`PaneId`] values. The per-frame Behavior shim lives in
//! [`crate::behavior`]; the rendering callback lives in
//! [`crate::render_pane`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui_tiles::{Tile, TileId, Tiles, Tree};

use crate::behavior::TabBehavior;
use crate::events::EventRecorder;
use crate::history::{HistoryStore, ReplayPaths, replay_into};
use crate::integration::ShellSpec;
use crate::pane::PaneSession;
use crate::pane_slot::{PaneAction, PaneId, PaneSlot, PaneUiState};
use crate::tab_title::active_pane_in_tabs;
use crate::{MIN_COLS, MIN_ROWS};

/// Cmd+Q confirmation timeout: after this many seconds with no
/// response from the user, the modal auto-confirms and the app
/// exits. Sized at one minute per the user's UX preference: long
/// enough to walk away and let it close, short enough that an
/// accidentally-triggered modal doesn't pin the app open forever.
const QUIT_CONFIRM_TIMEOUT_SECS: f64 = 60.0;

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
    /// [`crate::render_pane::render_pane`] each frame and consumed
    /// by [`TabBehavior`]'s styling hooks to highlight the focused
    /// pane's tab. One-frame lag is acceptable — the focus styling
    /// catches up next frame.
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
    /// Diagnostic event sink shared across all panes in this
    /// process. `Some` when `TERMICA_DUMP_EVENTS=<path>` was set at
    /// startup; passed to each [`PaneSession`] on spawn. `None`
    /// disables dump-events entirely with zero per-pane cost.
    event_recorder: Option<Arc<EventRecorder>>,
    /// Per-process command-history store. `Some` once the on-disk
    /// SQLite at `<data-dir>/history.sqlite` opens successfully;
    /// `None` if the data dir couldn't be resolved or the DB
    /// failed to open — in which case the app degrades gracefully
    /// to "no persisted history" and continues running.
    ///
    /// Used for: (a) recording Termica's own captured submits via
    /// `PaneSession` (PR 4); (b) feeding the ↑/↓ recall (PR 5) and
    /// ^R overlay (PR 6). PR 3 populates it from `~/.zsh_history`
    /// & friends at construction time.
    ///
    /// `Arc<Mutex<…>>` because `rusqlite::Connection` is `Send` but
    /// not `Sync` — future code paths (PaneSession capture in PR 4,
    /// background `gc()` later) will need to hold a clone. The mutex
    /// cost is negligible: no contention today, and history writes
    /// are infrequent compared to PTY traffic.
    #[allow(dead_code)]
    pub(crate) history: Option<Arc<Mutex<HistoryStore>>>,
    /// Per-process UUID tagging every captured submit in the
    /// `runs` table. The `↑`/`↓` recall filters by this so a fresh
    /// pane never inherits a closed pane's typing — see
    /// [spec/07 §"Pane-scope recall"](../spec/07-history-and-search.md#pane-scope-recall--).
    #[allow(dead_code)]
    pub(crate) app_run_id: String,
    /// Live-tunable focused-editor chrome variant. The main pane
    /// renderer consults this every frame; the picker viewport
    /// (a second OS window opened via `--pick-chrome`) writes into
    /// the same `Arc<Mutex<…>>` so a click in the picker shows the
    /// new chrome in the main window immediately.
    pub(crate) chrome_variant: Arc<Mutex<crate::focused_chrome::ChromeVariant>>,
    /// True while the picker viewport is meant to be alive. Set by
    /// `--pick-chrome` and cleared when the user closes the
    /// picker window. The picker keeps its own ViewportId so egui
    /// can route close events back to us.
    pub(crate) picker_viewport_open: Arc<AtomicBool>,
}

impl TermicaApp {
    /// Construct an app with one initial pane in a single Tabs
    /// container at the root of the tree.
    pub fn new() -> Self {
        Self::new_with_options(TermicaAppOptions::default())
    }

    /// Construct with options — used by `--pick-chrome` to open the
    /// chrome picker viewport on startup.
    pub fn new_with_options(opts: TermicaAppOptions) -> Self {
        let home = home::home_dir();
        let event_recorder = init_event_recorder();
        let history = init_history_store(home.as_deref());
        let app_run_id = uuid::Uuid::new_v4().to_string();
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
            event_recorder,
            history,
            app_run_id,
            chrome_variant: Arc::new(Mutex::new(opts.initial_chrome_variant)),
            picker_viewport_open: Arc::new(AtomicBool::new(opts.open_chrome_picker)),
        };
        app.bootstrap();
        app
    }

    /// Build the per-pane `HistoryContext` from the app's
    /// `history` + `app_run_id`. Returns `None` if history couldn't
    /// be opened (degraded mode) — panes spawn without capture and
    /// the app stays usable.
    fn history_ctx(&self) -> Option<crate::history::HistoryContext> {
        let store = self.history.clone()?;
        Some(crate::history::HistoryContext { store, app_run_id: self.app_run_id.clone() })
    }

    fn bootstrap(&mut self) {
        let pane_id = self.mint_pane_id();
        let shell = resolve_shell_from_env();
        let recorder = self.event_recorder.clone();
        let history = self.history_ctx();
        let session = PaneSession::spawn_managed(
            MIN_ROWS.max(24),
            MIN_COLS.max(80),
            shell,
            None,
            pane_id.0,
            recorder,
            history,
        )
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
            PaneAction::ClearScrollback => {
                // Cmd+K / Ctrl+Shift+K: drop the sealed-block
                // history AND blank the live terminal grid. The
                // shell process is untouched — it'll redraw its
                // prompt on the next prompt cycle (or when the
                // user presses Enter). Previously this only cleared
                // the alacritty grid, leaving the block stack
                // visually intact, so the user saw nothing change.
                if let Some(slot) = self.panes.get_mut(&pane_id) {
                    slot.session.clear_scrollback();
                }
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
        let shell = resolve_shell_from_env();
        let recorder = self.event_recorder.clone();
        let history = self.history_ctx();
        let session =
            match PaneSession::spawn_managed(24, 80, shell, cwd, pane_id.0, recorder, history) {
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
        // Drain every pane up front so this frame decides the next
        // repaint cadence from actual activity. Drained panes also
        // reset their `slot.ui.focused` mirror.
        //
        // Idle (no PTY bytes arrived this frame): 300 ms = ~3 fps —
        // enough to keep the channel from backing up but cheap on
        // CPU. egui is reactive: any input event (mouse, key,
        // viewport command) repaints immediately regardless. The
        // caret-blink and bell-flash paths schedule their own
        // shorter timers when those features are active.
        //
        // Active (any pane consumed bytes): 50 ms = 20 fps so a
        // streaming command still feels live.
        //
        // Previous behaviour was an unconditional 50 ms repaint
        // request — 20 fps forever, regardless of activity. With
        // the chrome-picker viewport open as a second window, that
        // doubled per-frame work and pushed CPU to ~100% even when
        // the shell was idle.
        let mut had_activity = false;
        for slot in self.panes.values_mut() {
            if slot.session.drain() > 0 {
                had_activity = true;
            }
            slot.ui.focused = false;
        }
        let next = if had_activity { 50 } else { 300 };
        ctx.request_repaint_after(std::time::Duration::from_millis(next));

        // Chrome picker viewport (second OS window). Stays open
        // as long as `picker_viewport_open` is true; the picker
        // clears the flag on its own close-request so subsequent
        // frames don't keep scheduling it.
        if self.picker_viewport_open.load(Ordering::Relaxed) {
            show_chrome_picker_viewport(
                ctx,
                self.chrome_variant.clone(),
                self.picker_viewport_open.clone(),
            );
        }

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
        // (The per-pane `drain()` + `slot.ui.focused = false` loop
        // moved to the top of `update()` so the result of the drain
        // can drive the per-frame repaint cadence. See the comment
        // above `had_activity`.)

        // Auto-close panes whose shell process has exited. The
        // shell `exit`ing (user typed exit, Ctrl+D, or `exit N`)
        // closes the PTY; the reader thread sees EOF, drops its
        // mpsc Sender, and `drain` above latches `is_exited`.
        // Route those panes through the existing close path — no
        // modal needed (nothing to confirm, the shell is gone).
        // If this empties the workspace, fall through to Quit so
        // we don't leave a tab-less window.
        let exited_panes: Vec<PaneId> = self
            .panes
            .iter()
            .filter_map(|(id, slot)| slot.session.is_exited().then_some(*id))
            .collect();
        if !exited_panes.is_empty() {
            let live_after_close = self.panes.len().saturating_sub(exited_panes.len());
            for pane_id in &exited_panes {
                if let Some(tile) = self.tile_for_pane(*pane_id) {
                    self.pending_closes.push(tile);
                }
            }
            if live_after_close == 0 {
                self.quit_requested = true;
            }
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
                chrome_variant: *self.chrome_variant.lock().expect("chrome variant mutex"),
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

/// Resolve the user's preferred shell from `$SHELL`. Falls back to
/// zsh if the env var isn't set (macOS default; sensible on Linux
/// too since the managed-startup machinery handles bash & fish
/// equivalently if the user has set `$SHELL`).
fn resolve_shell_from_env() -> ShellSpec {
    std::env::var("SHELL").map(|s| ShellSpec::from_shell_path(&s)).unwrap_or(ShellSpec::Zsh)
}

/// Build an [`EventRecorder`] from `TERMICA_DUMP_EVENTS=<path>` if
/// the env var is set; otherwise return `None` so dump-events is a
/// zero-cost opt-in. If opening the file fails we report on stderr
/// and disable dump-events for the session — a diagnostic feature
/// is never allowed to abort startup.
fn init_event_recorder() -> Option<Arc<EventRecorder>> {
    let path = std::env::var_os("TERMICA_DUMP_EVENTS")?;
    let path = std::path::PathBuf::from(path);
    match EventRecorder::new(&path) {
        Ok(rec) => {
            eprintln!("termica: dump-events recording to {}", path.display());
            Some(Arc::new(rec))
        }
        Err(e) => {
            eprintln!("termica: failed to open TERMICA_DUMP_EVENTS path {}: {e}", path.display());
            None
        }
    }
}

/// Open the on-disk `HistoryStore` and replay the user's shell-
/// history files into it. Returns `None` (and logs to stderr) if
/// the data dir can't be resolved or the DB fails to open —
/// history is a nice-to-have, never allowed to block startup.
///
/// Replay is idempotent (see [`crate::history::replay`]) so the
/// "run on every startup" model is safe: re-reading the same file
/// doesn't duplicate rows.
fn init_history_store(home: Option<&std::path::Path>) -> Option<Arc<Mutex<HistoryStore>>> {
    let dirs = directories::ProjectDirs::from("", "", "termica")?;
    let path = dirs.data_dir().join("history.sqlite");
    let store = match HistoryStore::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("termica: failed to open history db {}: {e}", path.display());
            return None;
        }
    };
    if let Some(h) = home {
        let _stats = replay_into(&store, &ReplayPaths::from_home(h));
    }
    Some(Arc::new(Mutex::new(store)))
}

/// Optional knobs for `TermicaApp::new_with_options`. Defaults
/// match what `TermicaApp::new` produced before — no behaviour
/// change unless the caller asks for it.
#[derive(Debug, Clone, Default)]
pub struct TermicaAppOptions {
    /// Open the chrome-picker viewport (second OS window) on
    /// startup. The picker shares an `Arc<Mutex<ChromeVariant>>`
    /// with the main window so clicks live-update the chrome.
    pub open_chrome_picker: bool,
    /// Initial value of [`TermicaApp::chrome_variant`].
    pub initial_chrome_variant: crate::focused_chrome::ChromeVariant,
}

/// Paint the focused-editor chrome picker into a deferred
/// Viewport (second OS window). Reads + writes the shared
/// `Arc<Mutex<ChromeVariant>>` so a click in this window
/// updates the main window's chrome on the next frame.
///
/// Closing the window via the OS close button clears
/// `picker_viewport_open` so subsequent frames stop scheduling
/// the viewport.
pub(crate) fn show_chrome_picker_viewport(
    ctx: &egui::Context,
    variant: Arc<Mutex<crate::focused_chrome::ChromeVariant>>,
    open: Arc<AtomicBool>,
) {
    let viewport_id = egui::ViewportId::from_hash_of("termica-chrome-picker");
    let builder = egui::ViewportBuilder::default()
        .with_title("Termica · pick focused-editor chrome")
        .with_inner_size([460.0, 720.0])
        .with_min_inner_size([320.0, 360.0]);
    ctx.show_viewport_deferred(viewport_id, builder, move |ctx, _class| {
        if ctx.input(|i| i.viewport().close_requested()) {
            open.store(false, Ordering::Relaxed);
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Focused-editor chrome");
            ui.weak("Click a variant — the main window updates immediately and reclaims keyboard focus so you can keep typing.");
            ui.weak("Close this window when you're done.");
            ui.separator();
            let current = *variant.lock().expect("chrome variant mutex");
            let mut picked = false;
            for (v, _id, label) in crate::focused_chrome::ChromeVariant::ALL {
                let is_selected = *v == current;
                let resp = ui.selectable_label(is_selected, *label);
                if resp.clicked() {
                    *variant.lock().expect("chrome variant mutex") = *v;
                    picked = true;
                }
            }
            ui.add_space(8.0);
            ui.weak(format!("current: {} ({})", current.label(), current.id()));
            // Return OS focus to the main Termica window after every
            // pick so the user's typing lands in the prompt editor
            // instead of getting stuck in the picker. Without this,
            // clicking a variant grabs window focus per OS convention
            // and the user has to manually re-click the main window.
            if picked {
                ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
            }
        });
    });
}
