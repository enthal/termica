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
    /// One-shot guard for pinning the *native window* appearance to
    /// dark. egui's `set_theme(Dark)` only colors the egui content; the
    /// OS window chrome (the macOS title bar) follows the system theme
    /// until we tell winit otherwise. We send the `SetTheme(Dark)`
    /// viewport command once on the first `update` (when the window
    /// definitely exists), then latch this so we don't re-send it every
    /// frame. Dark-only is the product — see the `set_theme` call in
    /// [`crate::run`].
    native_dark_theme_applied: bool,
    /// Diagnostic event sink shared across all panes in this
    /// process. `Some` when `TERMICA_DUMP_EVENTS=<path>` was set at
    /// startup; passed to each [`PaneSession`] on spawn. `None`
    /// disables dump-events entirely with zero per-pane cost.
    event_recorder: Option<Arc<EventRecorder>>,
    /// Per-process command-history store. `Some` once the on-disk
    /// SQLite at `<data-dir>/termica.sqlite` opens successfully;
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
    /// Persistence root (`<data-dir>`) — where scrollback chunk files
    /// live (`<root>/scrollback/…`). `Some` exactly when `history` is
    /// (both come from the same `init_history_store`); paired with the
    /// store to build a [`crate::persist::store::Persistence`] per pane.
    persist_root: Option<std::path::PathBuf>,
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
    /// Live-tunable blank-pane watermark appearance. The pane renderer
    /// reads a per-frame snapshot; the `--pick-watermark` viewport
    /// writes into this same `Arc<Mutex<…>>` so slider drags show up
    /// in the main window immediately. Same pattern as `chrome_variant`.
    pub(crate) watermark: Arc<Mutex<crate::watermark::WatermarkSettings>>,
    /// True while the watermark-tuner viewport is meant to be alive.
    /// Set by `--pick-watermark`, cleared when its window is closed.
    pub(crate) watermark_picker_open: Arc<AtomicBool>,
    /// Most-recently-sent OS window title. We compute the desired
    /// title each frame (`<active-tab-title> | Termica`) and only
    /// dispatch a `ViewportCommand::Title` when it changes — the
    /// command crosses an OS boundary and a no-op call per frame
    /// would be wasteful.
    last_window_title: String,
    /// Resolved first-pane starting cwd, set from
    /// `TermicaAppOptions.startup_cwd`. Consumed once by
    /// [`Self::bootstrap`] (via `take`) and never read again.
    startup_cwd: Option<PathBuf>,
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
        let (history, persist_root) = match init_history_store(home.as_deref()) {
            Some((store, root)) => (Some(store), Some(root)),
            None => (None, None),
        };
        // Scrollback gc (9E): enforce the disk + `runs` growth caps on a
        // background thread so launch never blocks on it. It skips live
        // sessions (lock-held) and is conservative — see
        // [`crate::persist::gc`]. Real wall-clock `now` is fine here
        // (production, not a test).
        if let (Some(store), Some(root)) = (history.as_ref(), persist_root.as_ref()) {
            let persist = crate::persist::store::Persistence::new(root.clone(), store.clone());
            let now_ms = now_unix_ms();
            std::thread::spawn(move || {
                match crate::persist::gc::gc(
                    &persist,
                    now_ms,
                    &crate::persist::gc::GcCaps::default(),
                ) {
                    Ok(r)
                        if r.chunks_deleted > 0
                            || r.runs_trimmed > 0
                            || r.tmp_files_deleted > 0 =>
                    {
                        eprintln!(
                            "termica: gc reclaimed {} chunks ({} bytes), trimmed {} runs, removed {} temps",
                            r.chunks_deleted,
                            r.bytes_reclaimed,
                            r.runs_trimmed,
                            r.tmp_files_deleted
                        );
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("termica: gc failed: {e}"),
                }
            });
        }
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
            native_dark_theme_applied: false,
            pending_close_confirm: None,
            quit_confirm_started_at: None,
            should_quit: false,
            about_open: false,
            event_recorder,
            history,
            persist_root,
            app_run_id,
            chrome_variant: Arc::new(Mutex::new(opts.initial_chrome_variant)),
            picker_viewport_open: Arc::new(AtomicBool::new(opts.open_chrome_picker)),
            watermark: Arc::new(Mutex::new(opts.initial_watermark)),
            watermark_picker_open: Arc::new(AtomicBool::new(opts.open_watermark_picker)),
            last_window_title: String::new(),
            startup_cwd: opts.startup_cwd,
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

    /// Build a per-pane [`Persistence`] handle from the shared store +
    /// data-dir root. `None` in degraded mode (no DB) — panes spawn
    /// without scrollback persistence and the app stays usable.
    fn persist(&self) -> Option<crate::persist::store::Persistence> {
        let store = self.history.clone()?;
        let root = self.persist_root.clone()?;
        Some(crate::persist::store::Persistence::new(root, store))
    }

    /// Save the current window layout + stamp live sessions ended (9F),
    /// so the next launch can restore. Best-effort: a missing store,
    /// serialization failure, or DB error degrades to "no restore next
    /// time", never a crash on quit.
    fn save_layout_on_quit(&self) {
        let Some(store) = self.history.as_ref() else { return };
        // Map every live pane's app id -> its durable db pane row id.
        let mut db_pane_by_app = std::collections::HashMap::new();
        for (pane_id, slot) in &self.panes {
            if let Some(db) = slot.session.persist_pane_row() {
                db_pane_by_app.insert(pane_id.0, db);
            }
        }
        if db_pane_by_app.is_empty() {
            return; // nothing persistable (degraded mode)
        }
        let layout =
            crate::persist::layout::SavedLayout { tree: self.tree.clone(), db_pane_by_app };
        let blob = match layout.to_blob() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("termica: layout serialize failed: {e}");
                return;
            }
        };
        let now = now_unix_ms();
        if let Ok(store) = store.lock() {
            if let Err(e) = store.save_layout(&blob, now) {
                eprintln!("termica: layout save failed: {e}");
            }
            for slot in self.panes.values() {
                if let Some(sid) = slot.session.persist_session() {
                    let _ = store.end_session(sid, now, None);
                }
            }
        }
    }

    fn bootstrap(&mut self) {
        // 9F: try to restore a saved workspace first (panes come back in
        // `Dead` mode showing their persisted scrollback). Falls through
        // to a fresh single pane if there's nothing to restore, the
        // layout is unreadable, or another process still owns it.
        if self.try_restore_workspace() {
            return;
        }
        self.bootstrap_fresh();
    }

    /// Rebuild the saved tile layout, recreating each pane in `Dead`
    /// mode with its persisted scrollback. Returns `false` (so the
    /// caller spawns a fresh pane) when there's no saved layout, it
    /// can't be parsed, it has no panes, or any pane's session is still
    /// locked by a live process (don't steal it).
    fn try_restore_workspace(&mut self) -> bool {
        let Some(persist) = self.persist() else { return false };
        let Some(store) = self.history.clone() else { return false };

        let blob = match store.lock() {
            Ok(s) => s.latest_layout().ok().flatten(),
            Err(_) => None,
        };
        let Some(blob) = blob else { return false };
        let layout = match crate::persist::layout::SavedLayout::from_blob(&blob) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("termica: layout parse failed, starting fresh: {e}");
                return false;
            }
        };
        let leaves = layout.pane_ids();
        if leaves.is_empty() {
            return false;
        }
        if !leaves.iter().all(|p| layout.db_pane_by_app.contains_key(&p.0)) {
            eprintln!("termica: saved layout has unmapped panes; starting fresh");
            return false;
        }

        // Liveness: if any pane's latest session is still lock-held, the
        // workspace belongs to a live process — don't adopt it.
        for &db_pane in layout.db_pane_by_app.values() {
            let Some(sid) =
                store.lock().ok().and_then(|s| s.latest_session_for_pane(db_pane).ok().flatten())
            else {
                continue;
            };
            // `matches!` evaluates and immediately drops any acquired
            // guard — we only want the verdict, not to hold the lock.
            if matches!(
                crate::persist::lock::SessionLock::try_acquire(&persist.session_dir(sid)),
                Ok(None)
            ) {
                return false; // held by a live process -> don't steal it
            }
        }

        // Build a Dead pane per leaf, reusing the saved app PaneIds so the
        // tree's leaves resolve.
        let mut max_app_id = 0u64;
        for pane_id in &leaves {
            let db_pane = layout.db_pane_by_app[&pane_id.0];
            let blocks = crate::persist::restore::restore_blocks_for_pane(&persist, db_pane);
            let stack = crate::block::BlockStack::with_restored_sealed(blocks);
            let cwd = store
                .lock()
                .ok()
                .and_then(|s| s.pane_cwd(db_pane).ok().flatten())
                .map(std::path::PathBuf::from);
            let session = PaneSession::restored(
                MIN_ROWS.max(24),
                MIN_COLS.max(80),
                stack,
                pane_id.0,
                db_pane,
                cwd,
            );
            self.panes.insert(*pane_id, PaneSlot { session, ui: PaneUiState::default() });
            max_app_id = max_app_id.max(pane_id.0);
        }
        self.tree = layout.tree;
        self.next_pane_id = max_app_id + 1;
        true
    }

    /// Spawn a single fresh managed pane and a one-tab tree — the
    /// no-restore startup path.
    fn bootstrap_fresh(&mut self) {
        let pane_id = self.mint_pane_id();
        let shell = resolve_shell_from_env();
        let recorder = self.event_recorder.clone();
        let history = self.history_ctx();
        let persist = self.persist();
        // First pane's starting cwd. Resolved per
        // spec/06 "Startup cwd and positional argument" — caller
        // (typically `run()`) computes it via `resolve_startup_cwd`
        // from the CLI positional arg + environment and passes it
        // in `TermicaAppOptions.startup_cwd`. `None` falls back to
        // `current_dir()` here, kept so test callers that don't
        // care about cwd don't have to thread the option through.
        let cwd = self.startup_cwd.take().or_else(|| std::env::current_dir().ok());
        let session = PaneSession::spawn_managed(
            MIN_ROWS.max(24),
            MIN_COLS.max(80),
            shell,
            cwd,
            pane_id.0,
            recorder,
            history,
            persist,
            None, // fresh pane
        )
        .expect("spawn initial pane");
        self.panes.insert(pane_id, PaneSlot { session, ui: PaneUiState::default() });

        let mut tiles = Tiles::default();
        let pane_tile = tiles.insert_pane(pane_id);
        let tabs_tile = tiles.insert_tab_tile(vec![pane_tile]);
        self.tree = Tree::new("termica-tree", tabs_tile, tiles);
    }

    /// Restart a `Dead` pane (9F): spawn a fresh managed shell in the
    /// pane's last-known cwd and transplant the restored scrollback into
    /// it, so the new shell's output appends below the old transcript.
    /// The pane keeps its `PaneId` (and tree slot); only its live half is
    /// replaced. No-op if the pane is gone or not actually dead.
    fn restart_pane(&mut self, pane_id: PaneId) {
        let Some(slot) = self.panes.get(&pane_id) else { return };
        if !slot.session.is_dead() {
            return;
        }
        let cwd = slot.session.terminal().cwd().map(|p| p.to_path_buf());
        // Reuse the pane's durable db row so its chunks accumulate across
        // restarts (the new shell's output continues the same pane's
        // logical-line sequence, rather than orphaning the old scrollback).
        let resume_pane_row = slot.session.persist_pane_row();

        // Gather spawn inputs before the mutable borrow of `panes`.
        let shell = resolve_shell_from_env();
        let recorder = self.event_recorder.clone();
        let history = self.history_ctx();
        let persist = self.persist();
        let fresh = match PaneSession::spawn_managed(
            MIN_ROWS.max(24),
            MIN_COLS.max(80),
            shell,
            cwd,
            pane_id.0,
            recorder,
            history,
            persist,
            resume_pane_row,
        ) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("termica: restart shell failed: {e}");
                return;
            }
        };

        // Swap the live session in, then move the dead pane's restored
        // scrollback into the fresh one (no clone — the snapshots move).
        if let Some(slot) = self.panes.get_mut(&pane_id) {
            let old = std::mem::replace(&mut slot.session, fresh);
            slot.session.adopt_restored_scrollback(old.into_sealed_blocks());
            slot.ui.needs_focus = true;
        }
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
                self.cycle_pane_global(pane_id, 1);
            }
            PaneAction::PrevTab => {
                self.cycle_pane_global(pane_id, -1);
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
            PaneAction::ScrollToTop => {
                // Cmd+Option+Up (macOS) / Ctrl+Alt+Up (Linux/Windows):
                // jump the pane's scroll position to the very top of
                // the sealed-block stack. The render path consumes
                // `scroll_to_top_pending` and calls
                // `scroll_to_cursor(TOP)` at the start of the scroll
                // closure. No-op in alt-screen mode (the live program
                // owns the viewport).
                if let Some(slot) = self.panes.get_mut(&pane_id) {
                    slot.ui.scroll_to_top_pending = true;
                }
            }
            PaneAction::ScrollToBottom => {
                // Cmd+Option+Down / Ctrl+Alt+Down: jump to the live
                // tail (editor / running grid). Reuses the existing
                // `scroll_to_bottom_pending` flag the editor submit
                // path also sets.
                if let Some(slot) = self.panes.get_mut(&pane_id) {
                    slot.ui.scroll_to_bottom_pending = true;
                }
            }
            PaneAction::ScrollPageUp => {
                // Ctrl+PageUp: page the scrollback viewport up (toward
                // older output). Relative move — accumulate so repeated
                // presses before a render all count. `render_pane`
                // applies it via `scroll_with_delta` and resets to 0.
                if let Some(slot) = self.panes.get_mut(&pane_id) {
                    slot.ui.scroll_page_pending += 1;
                }
            }
            PaneAction::ScrollPageDown => {
                // Ctrl+PageDown: page the scrollback viewport down.
                if let Some(slot) = self.panes.get_mut(&pane_id) {
                    slot.ui.scroll_page_pending -= 1;
                }
            }
            PaneAction::OpenFind => {
                // Cmd+F / Ctrl+Shift+F: open the in-pane find overlay
                // (Phase 8). Skip it in alt-screen mode — a full-screen
                // program (vim/less/htop) owns the viewport and there's
                // no scrollback transcript to search. Reopening carries
                // this pane's prior query history forward, and closes
                // the Ctrl+R overlay so the two never fight for focus.
                if let Some(slot) = self.panes.get_mut(&pane_id)
                    && !slot.session.terminal().is_alternate_screen()
                {
                    // Popups are mutually exclusive — close the others.
                    slot.ui.history_overlay = None;
                    slot.ui.completion_popup = None;
                    slot.ui.keybindings_open = false;
                    // Seed from the pane's persisted query history so a
                    // reopen after Esc still has the dropdown populated.
                    let history = slot.ui.find_history.clone();
                    slot.ui.find_overlay = Some(crate::find::FindOverlay::open(history));
                    slot.ui.needs_focus = true;
                }
            }
            PaneAction::RestartShell => self.restart_pane(pane_id),
        }
    }

    /// Cycle keyboard focus to the next/prev pane GLOBALLY across
    /// every `Container` in the workspace, in DFS tree order
    /// (left→right, top→bottom). Used for Cmd+Shift+] / [.
    ///
    /// The previous behaviour confined cycling to the focused
    /// pane's parent `Container::Tabs` — a Cmd+Shift+] from the
    /// rightmost tab in the left container of a horizontal split
    /// wrapped back to the leftmost tab of the SAME container,
    /// never reaching the right container. The user wants the
    /// shortcut to walk every tab everywhere, so this version
    /// flattens the tree and steps through that list.
    ///
    /// For the destination pane, we set its parent `Tabs`'s
    /// active child (so the tab becomes visible) and flag
    /// `slot.ui.needs_focus` so `render_pane` claims keyboard
    /// focus on its next render. Panes whose parent isn't a Tabs
    /// container (e.g., the only child of a SplitH) still get the
    /// focus flag — they're already visible.
    fn cycle_pane_global(&mut self, from: PaneId, delta: i32) {
        let panes = collect_panes_in_tree_order(&self.tree);
        if panes.len() <= 1 {
            return;
        }
        let Some(current_idx) = panes.iter().position(|(p, _)| *p == from) else {
            return;
        };
        let next_idx = ((current_idx as i32 + delta).rem_euclid(panes.len() as i32)) as usize;
        let (new_pane, new_tile) = panes[next_idx];
        if let Some(parent) = self.tree.tiles.parent_of(new_tile)
            && let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) =
                self.tree.tiles.get_mut(parent)
        {
            tabs.set_active(new_tile);
        }
        if let Some(slot) = self.panes.get_mut(&new_pane) {
            slot.ui.needs_focus = true;
        }
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
        let persist = self.persist();
        let session = match PaneSession::spawn_managed(
            24, 80, shell, cwd, pane_id.0, recorder, history, persist, None,
        ) {
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
        // Pin the *native* window chrome to dark, once. `set_theme(Dark)`
        // in `run()` only themes the egui content — the macOS title bar
        // (and Windows/Linux client-side decorations) follow the system
        // theme until winit is told otherwise, which is why a light-mode
        // Mac shows a white title bar above our black grid. This maps to
        // winit's `Window::set_theme(Dark)` (→ dark `NSAppearance`).
        // Latched so it isn't re-sent every frame.
        if !self.native_dark_theme_applied {
            ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
            self.native_dark_theme_applied = true;
        }

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

        // Watermark tuner viewport (second OS window), same lifecycle
        // as the chrome picker above.
        if self.watermark_picker_open.load(Ordering::Relaxed) {
            show_watermark_picker_viewport(
                ctx,
                self.watermark.clone(),
                self.watermark_picker_open.clone(),
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
        // Edge-to-edge: drop the CentralPanel's default ~8px inner
        // margin so panes (and the failed-block left stripe) sit flush
        // against the window edges, like a normal terminal.
        let central_frame =
            egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::ZERO);
        egui::CentralPanel::default().frame(central_frame).show(ctx, |ui| {
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
                watermark: *self.watermark.lock().expect("watermark mutex"),
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
            // Persist the layout (9F) so the next launch can restore it,
            // and stamp the live sessions ended. Best-effort; never
            // blocks the close.
            self.save_layout_on_quit();
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

        // OS window title: `<active-pane-title> | Termica` where
        // the active-pane title is the same string the tab strip
        // shows (running program ⇒ that program's name; else OSC
        // shell-set ⇒ that; else cwd-derived). Only dispatched when
        // it changes — `ViewportCommand::Title` crosses an OS
        // boundary and a no-op call per frame would be wasteful.
        let pane_for_title =
            self.focused_pane.or_else(|| self.focus_history.first().copied()).or_else(|| {
                self.tree
                    .tiles
                    .iter()
                    .find_map(|(_, t)| if let Tile::Pane(id) = t { Some(*id) } else { None })
            });
        let desired_title = pane_for_title
            .and_then(|id| {
                let slot = self.panes.get(&id)?;
                let osc = slot.session.terminal().osc_title();
                let cwd = slot.session.terminal().cwd();
                let running = crate::behavior::running_command_for(Some(slot));
                Some(crate::tab_title::window_title_for_with_osc(
                    id,
                    osc.as_deref(),
                    cwd,
                    self.home.as_deref(),
                    running.as_deref(),
                ))
            })
            .unwrap_or_default();
        let new_window_title = if desired_title.is_empty() {
            "Termica".to_string()
        } else {
            format!("{desired_title} — Termica")
        };
        if new_window_title != self.last_window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(new_window_title.clone()));
            self.last_window_title = new_window_title;
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
/// Wall-clock Unix-epoch milliseconds. Production only (startup gc
/// timestamp); tests inject a fixed `now` instead, per the determinism
/// rule. A pre-epoch clock clamps to 0.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn init_history_store(
    home: Option<&std::path::Path>,
) -> Option<(Arc<Mutex<HistoryStore>>, std::path::PathBuf)> {
    let dirs = directories::ProjectDirs::from("", "", "termica")?;
    // One database for everything durable that is not a chunk file —
    // layout, sessions, runs, the chunk index. The pre-1.0 builds
    // shipped this as `history.sqlite` (runs-only); the rename is a
    // one-time manual `mv` on the developer's own machines, NOT an
    // in-app migration, so we simply open `termica.sqlite`. A missing
    // file is created fresh and `runs` re-seeds from shell history on
    // the next start (see spec/08-persistence.md §"One database").
    let data_dir = dirs.data_dir().to_path_buf();
    let path = data_dir.join("termica.sqlite");
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
    // The data dir is also the persistence root: chunk files live under
    // `<data-dir>/scrollback/…` (9D), index rows in the DB above.
    Some((Arc::new(Mutex::new(store)), data_dir))
}

/// Walk the tile tree DFS, collecting `(PaneId, TileId)` for every
/// leaf pane in tree order (left→right, top→bottom). Pure helper
/// used by [`TermicaApp::cycle_pane_global`] to give a deterministic
/// "next pane" relative to the current focus, across all
/// `Container`s in the workspace.
///
/// Returns an empty `Vec` when the tree has no root.
fn collect_panes_in_tree_order(tree: &egui_tiles::Tree<PaneId>) -> Vec<(PaneId, TileId)> {
    fn walk(tiles: &egui_tiles::Tiles<PaneId>, tile_id: TileId, out: &mut Vec<(PaneId, TileId)>) {
        match tiles.get(tile_id) {
            Some(Tile::Pane(p)) => out.push((*p, tile_id)),
            Some(Tile::Container(c)) => {
                for child in c.children_vec() {
                    walk(tiles, child, out);
                }
            }
            None => {}
        }
    }
    let mut out = Vec::new();
    if let Some(root) = tree.root {
        walk(&tree.tiles, root, &mut out);
    }
    out
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
    /// Open the watermark-tuner viewport (second OS window) on
    /// startup. Shares an `Arc<Mutex<WatermarkSettings>>` with the
    /// main window so slider drags live-update the blank-pane overlay.
    pub open_watermark_picker: bool,
    /// Initial value of [`TermicaApp::watermark`].
    pub initial_watermark: crate::watermark::WatermarkSettings,
    /// First pane's starting cwd. See
    /// [spec/06 §"Startup cwd and positional argument"](../spec/06-workspace-and-tiles.md#startup-cwd-and-positional-argument)
    /// for the resolution chain (positional arg → `current_dir`
    /// → `$HOME` → `/`). Use [`resolve_startup_cwd`] to compute it
    /// from a CLI positional arg + the environment. `None` falls
    /// back to whatever `current_dir()` returns inside `bootstrap`
    /// — the pre-spec behaviour, kept for tests that don't care.
    pub startup_cwd: Option<PathBuf>,
}

/// Resolve the first pane's starting cwd per
/// [spec/06 §"Startup cwd and positional argument"](../spec/06-workspace-and-tiles.md#startup-cwd-and-positional-argument).
///
/// Pure helper — takes the positional path (if any) and the env
/// vars + filesystem queries it needs as ambient state. Returns
/// the resolved `PathBuf`; never panics, never fails.
///
/// Fallback chain:
/// 1. `positional_path` is a directory → it.
/// 2. `positional_path` is a non-directory file with a parent →
///    that parent directory.
/// 3. Else `std::env::current_dir()` if it succeeds.
/// 4. Else `$HOME` if set.
/// 5. Else `/`.
pub fn resolve_startup_cwd(positional_path: Option<&std::path::Path>) -> PathBuf {
    if let Some(p) = positional_path {
        let meta = std::fs::metadata(p);
        if let Ok(m) = meta {
            if m.is_dir() {
                return p.to_path_buf();
            }
            if let Some(parent) = p.parent()
                && !parent.as_os_str().is_empty()
            {
                return parent.to_path_buf();
            }
        }
        // p doesn't exist or has no usable parent → fall through.
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd;
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    PathBuf::from("/")
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

/// Paint the blank-pane watermark tuner into a deferred Viewport
/// (second OS window, opened via `--pick-watermark`). Reads + writes
/// the shared `Arc<Mutex<WatermarkSettings>>` so a slider drag here
/// updates the main window's watermark on the next frame.
///
/// Closing the window clears `watermark_picker_open` so subsequent
/// frames stop scheduling the viewport. Mirrors
/// [`show_chrome_picker_viewport`].
pub(crate) fn show_watermark_picker_viewport(
    ctx: &egui::Context,
    settings: Arc<Mutex<crate::watermark::WatermarkSettings>>,
    open: Arc<AtomicBool>,
) {
    use crate::watermark::{MAX_SIZE_FRAC, MIN_SIZE_FRAC};

    let viewport_id = egui::ViewportId::from_hash_of("termica-watermark-picker");
    let builder = egui::ViewportBuilder::default()
        .with_title("Termica · tune blank-pane watermark")
        .with_inner_size([420.0, 320.0])
        .with_min_inner_size([320.0, 240.0]);
    ctx.show_viewport_deferred(viewport_id, builder, move |ctx, _class| {
        if ctx.input(|i| i.viewport().close_requested()) {
            open.store(false, Ordering::Relaxed);
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Blank-pane watermark");
            ui.weak(
                "Shows the app icon faintly behind a pane until its first command \
                 runs. Drag a slider — the main window updates immediately.",
            );
            ui.separator();

            // Snapshot, mutate locally, write back only on change so we
            // don't hold the lock across the whole frame or churn the
            // shared value when nothing moved.
            let mut s = *settings.lock().expect("watermark mutex");
            let before = s;

            ui.checkbox(&mut s.enabled, "Enabled");
            ui.add_enabled_ui(s.enabled, |ui| {
                ui.add(egui::Slider::new(&mut s.alpha, 0..=255).text("Opacity (alpha)"));
                ui.add(
                    egui::Slider::new(&mut s.size_frac, MIN_SIZE_FRAC..=MAX_SIZE_FRAC)
                        .text("Size (fraction of short side)"),
                );
                ui.checkbox(&mut s.grayscale, "Grayscale");
            });

            ui.add_space(8.0);
            ui.weak(format!(
                "current: alpha={} size={:.2} grayscale={} enabled={}",
                s.alpha, s.size_frac, s.grayscale, s.enabled
            ));

            if s != before {
                *settings.lock().expect("watermark mutex") = s;
                // NB: unlike the chrome picker we deliberately do NOT
                // steal focus back to the main window here — these are
                // sliders, and a per-frame Focus command would yank the
                // pointer-grab away mid-drag and break dragging.
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_tiles::{Tiles, Tree};

    /// Build a tree with two side-by-side Tabs containers:
    ///   SplitH(
    ///     Tabs(p1, p2),       <- left container
    ///     Tabs(p3, p4, p5),   <- right container
    ///   )
    /// Returns the tree and the inserted (PaneId → TileId) mapping
    /// in insertion order.
    fn split_with_two_tab_containers() -> (Tree<PaneId>, Vec<(PaneId, TileId)>) {
        let mut tiles: Tiles<PaneId> = Tiles::default();
        let p1 = tiles.insert_pane(PaneId(1));
        let p2 = tiles.insert_pane(PaneId(2));
        let p3 = tiles.insert_pane(PaneId(3));
        let p4 = tiles.insert_pane(PaneId(4));
        let p5 = tiles.insert_pane(PaneId(5));
        let left = tiles.insert_tab_tile(vec![p1, p2]);
        let right = tiles.insert_tab_tile(vec![p3, p4, p5]);
        let root = tiles.insert_horizontal_tile(vec![left, right]);
        let tree = Tree::new("test", root, tiles);
        let pairs = vec![
            (PaneId(1), p1),
            (PaneId(2), p2),
            (PaneId(3), p3),
            (PaneId(4), p4),
            (PaneId(5), p5),
        ];
        (tree, pairs)
    }

    #[test]
    fn collect_panes_walks_horizontal_split_left_to_right() {
        let (tree, _) = split_with_two_tab_containers();
        let got: Vec<PaneId> =
            collect_panes_in_tree_order(&tree).into_iter().map(|(p, _)| p).collect();
        assert_eq!(got, vec![PaneId(1), PaneId(2), PaneId(3), PaneId(4), PaneId(5)]);
    }

    #[test]
    fn collect_panes_returns_empty_on_empty_tree() {
        let tiles: Tiles<PaneId> = Tiles::default();
        // egui_tiles::Tree::empty creates a rootless tree.
        let tree: Tree<PaneId> = Tree::empty("empty");
        assert!(tree.root.is_none(), "scaffolding: empty tree should have no root");
        assert!(collect_panes_in_tree_order(&tree).is_empty());
        // Touch `tiles` so the unused-binding lint doesn't complain
        // in CI; the empty-tree assertion above is the meat.
        let _ = tiles;
    }

    #[test]
    fn collect_panes_pane_id_to_tile_id_mapping_is_correct() {
        let (tree, pairs) = split_with_two_tab_containers();
        let got = collect_panes_in_tree_order(&tree);
        // Same length, same ordering.
        assert_eq!(got.len(), pairs.len());
        for (got_pair, expected_pair) in got.iter().zip(pairs.iter()) {
            assert_eq!(got_pair, expected_pair);
        }
    }

    // ---- resolve_startup_cwd: spec/06 fallback chain --------------

    #[test]
    fn resolve_startup_cwd_positional_directory_used_as_is() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_startup_cwd(Some(dir.path()));
        assert_eq!(resolved, dir.path().to_path_buf());
    }

    #[test]
    fn resolve_startup_cwd_positional_file_uses_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("a-file.txt");
        std::fs::write(&file_path, b"x").expect("write");
        let resolved = resolve_startup_cwd(Some(&file_path));
        assert_eq!(resolved, dir.path().to_path_buf());
    }

    #[test]
    fn resolve_startup_cwd_missing_path_falls_back_to_current_dir() {
        // The path doesn't exist → fall through to current_dir().
        // We only assert that the fallback is NOT the bogus path,
        // since current_dir() depends on the test runner's cwd.
        let bogus = std::path::PathBuf::from("/this/path/should/not/exist/ever-termica");
        let resolved = resolve_startup_cwd(Some(&bogus));
        assert_ne!(resolved, bogus);
        // And it must be either current_dir() or $HOME or /; all
        // of those are absolute paths.
        assert!(resolved.is_absolute(), "fallback cwd must be absolute, got {resolved:?}");
    }

    #[test]
    fn resolve_startup_cwd_no_positional_falls_back_to_current_dir() {
        let resolved = resolve_startup_cwd(None);
        // current_dir() should succeed in the test environment.
        assert_eq!(resolved, std::env::current_dir().expect("current_dir in test"));
    }
}
