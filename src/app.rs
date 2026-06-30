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

/// How long to wait after the last structural layout change before
/// persisting the layout blob (spec/08 §"Written on change"). A burst of
/// edits (open three tabs, drag a split) coalesces into one debounced
/// write rather than one write per frame. Measured against egui's
/// monotonic input-time (`ctx.input(|i| i.time)`), not `Instant::now()`,
/// so the debounce is deterministic and testable.
const LAYOUT_SAVE_DEBOUNCE_SECS: f64 = 1.0;

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
    /// Explicit CLI path (resolved dir), `Some` only when the user named
    /// one. Consumed once by [`Self::bootstrap`] to open a tab there even
    /// over a restored workspace. See `TermicaAppOptions`.
    requested_workspace_path: Option<PathBuf>,
    /// Structural fingerprint of the layout (tile topology + the
    /// `db_pane_by_app` mapping) as of the last computed frame. Compared
    /// each frame against a freshly-computed fingerprint to detect a
    /// structural change worth persisting — see [`layout_fingerprint`].
    /// `0` until the first frame computes one.
    layout_fingerprint: u64,
    /// egui input-time (`ctx.input(|i| i.time)`, monotonic seconds) at
    /// which a pending debounced layout save should flush. `Some` while a
    /// save is queued (a structural change happened &lt; debounce ago);
    /// `None` when nothing is pending. The layout is persisted **on
    /// change**, not on quit (spec/08) — the quit flush is only a
    /// backstop, because the process can vanish with no teardown.
    layout_save_deadline: Option<f64>,
    /// The OS window's last-sampled size + position (egui logical points),
    /// captured each frame from the viewport. Persisted in the layout blob
    /// so a relaunch reopens where the window was (spec/08); folded into
    /// the layout fingerprint so a resize/move arms the same debounced
    /// save. `None` until the first frame reports a viewport rect.
    last_window_geometry: Option<crate::persist::layout::WindowGeometry>,
    /// One-shot guard: the restored window has been clamped to fit the
    /// current monitor. The saved geometry is applied by the
    /// `ViewportBuilder` before the monitor size is known, so on the first
    /// frame that reports a monitor we re-fit the window to it (a
    /// workspace saved on a large display opens usably on a smaller one).
    /// Set once and never re-fit, so the user can freely resize after.
    window_restore_clamped: bool,
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
            requested_workspace_path: opts.requested_workspace_path,
            layout_fingerprint: 0,
            layout_save_deadline: None,
            last_window_geometry: None,
            window_restore_clamped: false,
        };
        app.bootstrap();
        // Scrollback gc runs AFTER restore, never concurrently. Both probe
        // the same per-session `flock`s for liveness; `flock` contends even
        // within one process (per open-file-description), so a gc probe that
        // momentarily holds session-N's lock while restore is checking
        // session-N would make restore see "held" → conclude the workspace
        // is owned by a live process → bail to a fresh pane. Serializing
        // them (restore first, then gc) removes that race entirely.
        app.spawn_gc();
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

    /// Spawn the scrollback gc (9E) on a background thread so launch never
    /// blocks on it. It enforces the disk + `runs` growth caps and skips
    /// live sessions (lock-held) — see [`crate::persist::gc`]. Real
    /// wall-clock `now` is fine here (production, not a test).
    ///
    /// **Spawned only after restore** ([`Self::bootstrap`]): gc and restore
    /// both probe per-session locks, and `flock` contends within one
    /// process, so running gc concurrently with restore would race restore
    /// into a spurious "workspace is live elsewhere" verdict.
    fn spawn_gc(&self) {
        let (Some(store), Some(root)) = (self.history.as_ref(), self.persist_root.as_ref()) else {
            return;
        };
        let persist = crate::persist::store::Persistence::new(root.clone(), store.clone());
        let now_ms = now_unix_ms();
        std::thread::spawn(move || {
            match crate::persist::gc::gc(&persist, now_ms, &crate::persist::gc::GcCaps::default()) {
                Ok(r) if r.chunks_deleted > 0 || r.runs_trimmed > 0 || r.tmp_files_deleted > 0 => {
                    eprintln!(
                        "termica: gc reclaimed {} chunks ({} bytes), trimmed {} runs, removed {} temps",
                        r.chunks_deleted, r.bytes_reclaimed, r.runs_trimmed, r.tmp_files_deleted
                    );
                }
                Ok(_) => {}
                Err(e) => eprintln!("termica: gc failed: {e}"),
            }
        });
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
    /// Drain every pane's background scrollback writer before the process
    /// exits, so all queued chunk writes and Cmd+K/close `Clear` deletes
    /// are applied (not lost to a fast quit). Must run *before* the store
    /// lock is taken in [`Self::save_layout_on_quit`] — the writer threads
    /// need that lock to finish draining.
    fn flush_persisted_writers(&mut self) {
        for slot in self.panes.values_mut() {
            slot.session.flush_chunk_writer();
        }
    }

    /// Map every live pane's app id → its durable `pane` row id. A pane in
    /// degraded / not-yet-persisted state (no row) is absent — the save
    /// path prunes such leaves from the written tree so the blob stays
    /// self-consistent (spec/08 §"The saved blob is self-consistent").
    fn build_db_pane_by_app(&self) -> HashMap<u64, i64> {
        let mut db_pane_by_app = HashMap::new();
        for (pane_id, slot) in &self.panes {
            if let Some(db) = slot.session.persist_pane_row() {
                db_pane_by_app.insert(pane_id.0, db);
            }
        }
        db_pane_by_app
    }

    /// Persist the current window layout (spec/08 §"Written on change").
    /// Best-effort: a missing store, serialization failure, or DB error
    /// degrades to "no restore next time", never a crash. Called both on
    /// every debounced structural change and as the quit backstop.
    ///
    /// The written tree contains **only** leaves that map to a durable db
    /// row: any unmapped (degraded) leaf is pruned first, so restore never
    /// sees an unmapped leaf of our own making and the blob round-trips
    /// self-consistently.
    /// One-shot on launch: fit the restored window to the *current*
    /// monitor (spec/08 "fit to the new screen"). The saved geometry was
    /// applied by the `ViewportBuilder` before the monitor size was known,
    /// so once egui reports a monitor we clamp the live window to it. Runs
    /// at most once — afterward the user resizes freely. A window that
    /// already fits is left exactly alone (no resize flash).
    fn fit_restored_window_to_monitor(&mut self, ctx: &egui::Context) {
        if self.window_restore_clamped {
            return;
        }
        let geom = sample_window_geometry(ctx);
        let monitor = ctx.input(|i| i.viewport().monitor_size);
        // Wait until both the window rect and the monitor size are known.
        let (Some(geom), Some(mon)) = (geom, monitor) else { return };
        self.window_restore_clamped = true;
        let fitted = geom.clamp_to_monitor(Some((mon.x, mon.y)));
        if (fitted.inner_width - geom.inner_width).abs() >= 1.0
            || (fitted.inner_height - geom.inner_height).abs() >= 1.0
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                fitted.inner_width,
                fitted.inner_height,
            )));
        }
        if (fitted.pos_x - geom.pos_x).abs() >= 1.0 || (fitted.pos_y - geom.pos_y).abs() >= 1.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                fitted.pos_x,
                fitted.pos_y,
            )));
        }
    }

    fn persist_layout_now(&self) {
        let Some(store) = self.history.as_ref() else { return };
        let db_pane_by_app = self.build_db_pane_by_app();
        save_layout_self_consistent(
            store,
            &self.tree,
            db_pane_by_app,
            self.last_window_geometry,
            now_unix_ms(),
        );
    }

    /// Quit backstop: persist the layout one final time and stamp every
    /// live session ended. The layout is already saved on change, so this
    /// is belt-and-suspenders — but the `end_session` stamping (which only
    /// makes sense at teardown) lives here, not in [`Self::persist_layout_now`].
    fn save_layout_on_quit(&self) {
        self.persist_layout_now();
        let Some(store) = self.history.as_ref() else { return };
        let now = now_unix_ms();
        if let Ok(store) = store.lock() {
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
            // A restored workspace brings its own panes (each with its own
            // cwd), so it ignores `startup_cwd`. But an EXPLICIT path arg
            // still means "give me a pane here" — honor it as a new,
            // focused tab alongside the restored panes (spec/06).
            if let Some(path) = self.requested_workspace_path.take() {
                self.open_tab_at_cwd(path);
            }
            return;
        }
        // Fresh start consumes `startup_cwd` (which already folded in any
        // path arg) for the first pane, so no separate tab is needed.
        self.bootstrap_fresh();
    }

    /// Open a new, focused tab whose shell starts in `cwd`. Used when a
    /// path is named on the command line over a restored workspace. The
    /// tab is added to the root `Tabs` container; if the restored root is
    /// not a `Tabs` (a split or a lone pane), it is wrapped in one so the
    /// new pane becomes a sibling tab rather than being lost.
    fn open_tab_at_cwd(&mut self, cwd: PathBuf) {
        let pane_id = self.mint_pane_id();
        let shell = resolve_shell_from_env();
        let recorder = self.event_recorder.clone();
        let history = self.history_ctx();
        let persist = self.persist();
        let session = match PaneSession::spawn_managed(
            MIN_ROWS.max(24),
            MIN_COLS.max(80),
            shell,
            Some(cwd),
            pane_id.0,
            recorder,
            history,
            persist,
            None, // a fresh pane, not a resume
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("termica: failed to open the requested path as a tab: {e}");
                return;
            }
        };
        self.panes.insert(pane_id, PaneSlot { session, ui: PaneUiState::default() });
        let pane_tile = self.tree.tiles.insert_pane(pane_id);

        let root_is_tabs = self
            .tree
            .root
            .and_then(|r| self.tree.tiles.get(r))
            .is_some_and(|t| matches!(t, Tile::Container(egui_tiles::Container::Tabs(_))));
        if root_is_tabs {
            if let Some(root) = self.tree.root
                && let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) =
                    self.tree.tiles.get_mut(root)
            {
                tabs.add_child(pane_tile);
                tabs.set_active(pane_tile);
            }
        } else if let Some(old_root) = self.tree.root {
            // Wrap the existing root so the new pane is a sibling tab.
            let new_root = self.tree.tiles.insert_tab_tile(vec![old_root, pane_tile]);
            if let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) =
                self.tree.tiles.get_mut(new_root)
            {
                tabs.set_active(pane_tile);
            }
            self.tree.root = Some(new_root);
        } else {
            // Empty tree (not expected post-restore) — make this the root.
            let new_root = self.tree.tiles.insert_tab_tile(vec![pane_tile]);
            self.tree.root = Some(new_root);
        }

        // Focus the new tab so the user lands in their requested dir.
        self.focused_pane = Some(pane_id);
        if let Some(slot) = self.panes.get_mut(&pane_id) {
            slot.ui.needs_focus = true;
        }
    }

    /// Rebuild the saved tile layout, recreating each pane in `Dead`
    /// mode with its persisted scrollback. Returns `false` (so the caller
    /// spawns a fresh pane) only when there is **nothing restorable** — no
    /// saved layout, it can't be parsed, it has no mapped leaves, or the
    /// workspace is still owned by a live process (don't steal it).
    ///
    /// Restore is **resilient, not all-or-nothing** (spec/08 §"Restore
    /// semantics"): a leaf the blob maps to nothing is pruned from the
    /// tree and the rest still restore; a pane whose chunks are all
    /// missing/corrupt restores as an empty `Dead` pane. One bad leaf
    /// never costs the user their whole workspace.
    fn try_restore_workspace(&mut self) -> bool {
        let Some(persist) = self.persist() else { return false };
        let Some(store) = self.history.clone() else { return false };
        let Some(plan) = plan_restore(&persist, &store) else { return false };

        // Build a Dead pane per surviving leaf, reusing the saved app
        // PaneIds so the tree's leaves resolve. Block loading (which
        // touches the filesystem) is done here, off the decision path.
        for p in &plan.panes {
            let blocks = crate::persist::restore::restore_blocks_for_pane(&persist, p.db_pane);
            let stack = crate::block::BlockStack::with_restored_sealed(blocks);
            let session = PaneSession::restored(
                MIN_ROWS.max(24),
                MIN_COLS.max(80),
                stack,
                p.pane_id.0,
                p.db_pane,
                p.cwd.clone(),
            );
            self.panes.insert(p.pane_id, PaneSlot { session, ui: PaneUiState::default() });
        }
        self.tree = plan.tree;
        self.next_pane_id = plan.next_pane_id;
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

        // One-shot on launch: fit the restored window to the current
        // monitor once egui reports its size (spec/08). No-op if the
        // window already fits or nothing was restored.
        self.fit_restored_window_to_monitor(ctx);

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
            // Flush every pane's scrollback writer FIRST: a Cmd+K / close
            // queues an async transcript-delete on the writer thread, and
            // pending chunk writes are async too. Draining them here is
            // the teardown flush (spec/08 §Teardown) — without it, a quit
            // right after Cmd+K exits before the delete lands and the
            // "cleared" transcript resurrects on next launch.
            self.flush_persisted_writers();
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

        // Defensive: every Tabs container must have an `active` that is a
        // real child of its own (see [`revalidate_tabs_active`] for the
        // three ways this invariant gets violated). Restore runs the same
        // pass after pruning unrestorable leaves.
        revalidate_tabs_active(&mut self.tree);

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
        // A pane leaves the tree only by being explicitly CLOSED (close
        // tab / close pane). That is an explicit discard, so — like Cmd+K
        // — delete its persisted transcript (keeping its `runs` history)
        // before the slot is dropped. Quit does NOT pass through here (it
        // tears down panes wholesale via app drop), so quit keeps
        // everything for restore.
        let closed: Vec<PaneId> =
            self.panes.keys().copied().filter(|id| !live_panes.contains(id)).collect();
        for id in closed {
            if let Some(slot) = self.panes.get_mut(&id) {
                slot.session.discard_persisted_transcript();
            }
        }
        self.panes.retain(|id, _| live_panes.contains(id));

        // Persist the layout on structural change (spec/08 §"Written on
        // change"), debounced so a burst of edits (open three tabs, drag a
        // split) coalesces into one write. The tree is settled for this
        // frame by now (post-`ui`, post-reconcile). The quit path saves
        // separately, so skip here while quitting. The first frame's `0 ->
        // real` fingerprint transition arms a save ~1s after launch, so
        // even an untouched session is durable against a later kill —
        // exactly the on-quit-only failure this replaces.
        if !self.should_quit {
            // Sample the OS window geometry this frame; folded into the
            // fingerprint so a resize/move arms the same debounced layout
            // save and is persisted for the next launch (spec/08).
            if let Some(g) = sample_window_geometry(ctx) {
                self.last_window_geometry = Some(g);
            }
            let db_pane_by_app = self.build_db_pane_by_app();
            let fp = layout_fingerprint(&self.tree, &db_pane_by_app, self.last_window_geometry);
            let now_s = ctx.input(|i| i.time);
            let fp_changed = fp != self.layout_fingerprint;
            if fp_changed {
                self.layout_fingerprint = fp;
                // Ensure the deadline-firing frame happens even if every
                // pane is idle (no PTY traffic to trigger a repaint).
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                    LAYOUT_SAVE_DEBOUNCE_SECS,
                ));
            }
            let (save_now, new_deadline) =
                debounce_decide(now_s, self.layout_save_deadline, fp_changed);
            self.layout_save_deadline = new_deadline;
            if save_now {
                self.persist_layout_now();
            }
        }
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
    // `APP_STORAGE_NAME` ("termica") is the on-disk storage namespace,
    // deliberately distinct from the reverse-DNS GUI `APP_ID` — so the
    // data dir path is unchanged by the desktop-identity work (#161).
    let dirs = directories::ProjectDirs::from("", "", crate::APP_STORAGE_NAME)?;
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

/// Pre-read the saved window geometry (size + position) *before* the
/// window is built, so the `ViewportBuilder` can reopen it where it was
/// (spec/08). A standalone, **replay-free** open of the same
/// `termica.sqlite` the app re-opens normally in [`TermicaApp::new`] — a
/// single quick `SELECT`, then dropped. `None` (no data dir, open failed,
/// no saved layout, or a pre-geometry blob) → the window opens at its
/// default size and the OS picks the position.
pub fn read_saved_window_geometry() -> Option<crate::persist::layout::WindowGeometry> {
    let dirs = directories::ProjectDirs::from("", "", crate::APP_STORAGE_NAME)?;
    let path = dirs.data_dir().join("termica.sqlite");
    let store = HistoryStore::open(&path).ok()?;
    let blob = store.latest_layout().ok().flatten()?;
    crate::persist::layout::SavedLayout::from_blob(&blob).ok()?.window_geometry
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

/// Sample the OS window's size + position from the egui viewport for this
/// frame. `inner_rect` is the drawable area; `outer_rect` (title-bar
/// inclusive) gives the on-screen position — fall back to `inner_rect`
/// when the platform doesn't report an outer rect. `None` until the
/// platform first reports a viewport rect (the very first frame), so the
/// caller simply keeps the previous geometry until one arrives.
fn sample_window_geometry(ctx: &egui::Context) -> Option<crate::persist::layout::WindowGeometry> {
    ctx.input(|i| {
        let vp = i.viewport();
        let inner = vp.inner_rect?;
        let outer = vp.outer_rect.unwrap_or(inner);
        Some(crate::persist::layout::WindowGeometry {
            inner_width: inner.width(),
            inner_height: inner.height(),
            pos_x: outer.min.x,
            pos_y: outer.min.y,
        })
    })
}

/// A deterministic structural fingerprint of a window's layout: the tile
/// **topology** (container kinds + ordered children + leaf `PaneId`s +
/// each Tabs container's active child) plus the `db_pane_by_app` mapping.
///
/// Used to detect a *structural* change worth persisting (spec/08
/// §"Written on change"): a new/closed tab, a split, a drag-reorder, an
/// active-tab switch, or a pane gaining/losing its durable db row all
/// move the fingerprint. It deliberately **excludes** cwd (persisted
/// independently by each pane's writer thread) and transient focus, so an
/// idle pane churning output doesn't trigger layout writes.
///
/// Walks from the root (not the tiles map), so it is independent of
/// `TileId` allocation order — two trees with the same shape but
/// different internal ids fingerprint equal.
fn layout_fingerprint(
    tree: &Tree<PaneId>,
    db_pane_by_app: &HashMap<u64, i64>,
    window_geometry: Option<crate::persist::layout::WindowGeometry>,
) -> u64 {
    use std::hash::{Hash, Hasher};

    fn hash_tile(
        tiles: &Tiles<PaneId>,
        tile_id: TileId,
        h: &mut std::collections::hash_map::DefaultHasher,
    ) {
        match tiles.get(tile_id) {
            Some(Tile::Pane(p)) => {
                0u8.hash(h); // leaf tag
                p.0.hash(h);
            }
            Some(Tile::Container(c)) => {
                1u8.hash(h); // container tag
                (c.kind() as u8).hash(h);
                let children = c.children_vec();
                children.len().hash(h);
                for child in &children {
                    hash_tile(tiles, *child, h);
                }
                // Active-tab position (Tabs only) — switching tabs is a
                // structural change we persist.
                if let egui_tiles::Container::Tabs(tabs) = c {
                    let active_idx =
                        tabs.active.and_then(|a| children.iter().position(|&t| t == a));
                    2u8.hash(h);
                    active_idx.hash(h);
                }
            }
            None => 3u8.hash(h), // dangling reference
        }
    }

    let mut h = std::collections::hash_map::DefaultHasher::new();
    match tree.root {
        Some(root) => hash_tile(&tree.tiles, root, &mut h),
        None => 4u8.hash(&mut h), // empty tree
    }
    // db_pane_by_app, in a canonical (sorted) order — HashMap iteration
    // order is otherwise nondeterministic and would flap the fingerprint.
    let mut pairs: Vec<(u64, i64)> = db_pane_by_app.iter().map(|(k, v)| (*k, *v)).collect();
    pairs.sort_unstable();
    pairs.hash(&mut h);
    // Window geometry, quantized to whole points so sub-pixel jitter
    // doesn't re-arm the save every frame. A real resize/move (≥1 pt)
    // moves the fingerprint and arms the debounced write.
    5u8.hash(&mut h);
    match window_geometry {
        Some(g) => {
            6u8.hash(&mut h);
            (g.inner_width.round() as i32).hash(&mut h);
            (g.inner_height.round() as i32).hash(&mut h);
            (g.pos_x.round() as i32).hash(&mut h);
            (g.pos_y.round() as i32).hash(&mut h);
        }
        None => 7u8.hash(&mut h),
    }
    h.finish()
}

/// Pure debounce decision for the layout save (spec/08 §"Written on
/// change"). Given the current monotonic time `now_s`, the pending save
/// `deadline` (if any), and whether the layout fingerprint changed this
/// frame, returns `(should_save_now, new_deadline)`.
///
/// A structural change (re)arms the deadline to `now + debounce`, so a
/// burst of edits coalesces into a single save once the dust settles. The
/// save fires (and the deadline disarms) on the first frame at or past
/// the deadline. Time is passed in — no `Instant::now()` — so the logic
/// is unit-tested with fixed constants.
fn debounce_decide(now_s: f64, deadline: Option<f64>, fp_changed: bool) -> (bool, Option<f64>) {
    let deadline = if fp_changed { Some(now_s + LAYOUT_SAVE_DEBOUNCE_SECS) } else { deadline };
    match deadline {
        Some(d) if now_s >= d => (true, None),
        other => (false, other),
    }
}

/// Repair every Tabs container's `active` so it points at a real child of
/// its own. We've observed three ways this invariant gets violated:
///
///   1. `active = None` — fresh container with no selection.
///   2. `active = Some(t)` where `t` was removed from `tiles`.
///   3. `active = Some(t)` where `t` exists but is NOT in this
///      container's `children` (e.g. a sibling Tabs container's tile —
///      egui_tiles 0.14 leaves the source container's `active` pointing
///      at the drop target after a drag-split).
///
/// In all three cases, adopt the first live child as active so the pane
/// area paints something rather than nothing. Run every frame from
/// `update()` and once by restore after pruning unrestorable leaves
/// (spec/08 §"Restore semantics") — pruning can leave a Tabs `active`
/// dangling exactly like case 2/3.
fn revalidate_tabs_active(tree: &mut Tree<PaneId>) {
    let live_tile_ids: HashSet<TileId> = tree.tiles.tile_ids().collect();
    for tile in tree.tiles.tiles_mut() {
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
}

/// Remove every leaf whose `PaneId` is not in `keep` from `tree`, then
/// simplify and re-validate Tabs `active`. Returns the set of `PaneId`s
/// that survived.
///
/// Shared by the save path (drop a live pane that has no durable db row,
/// so the written blob is self-consistent) and the restore path (drop a
/// leaf the blob maps to nothing). The simplify pass — driven by the same
/// [`crate::behavior::simplification_options`] the live `tree.ui()` uses
/// — collapses now-empty / single-child containers, re-wraps a lone pane
/// in Tabs, and nulls the root when nothing is left. So pruning the only
/// pane out of a split leaves a valid, minimal tree rather than an empty
/// container with a dangling root.
fn prune_tree_to_mapped(tree: &mut Tree<PaneId>, keep: &HashSet<PaneId>) -> HashSet<PaneId> {
    // Collect the tiles to drop first — don't mutate while iterating.
    let drop_tiles: Vec<TileId> = tree
        .tiles
        .iter()
        .filter_map(|(id, tile)| match tile {
            Tile::Pane(p) if !keep.contains(p) => Some(*id),
            _ => None,
        })
        .collect();
    for tile_id in drop_tiles {
        tree.remove_recursively(tile_id);
    }
    tree.simplify(&crate::behavior::simplification_options());
    revalidate_tabs_active(tree);
    // Recompute survivors from the simplified tree.
    tree.tiles
        .tiles()
        .filter_map(|tile| match tile {
            Tile::Pane(p) => Some(*p),
            Tile::Container(_) => None,
        })
        .collect()
}

/// Write a **self-consistent** layout blob (spec/08 §"The saved blob is
/// self-consistent") and return the surviving `PaneId`s, or `None` if
/// there was nothing to persist.
///
/// The written tree contains only leaves present in `db_pane_by_app`: any
/// unmapped (degraded / not-yet-persisted) leaf is pruned first, and map
/// entries for pruned leaves are dropped, so restore is never handed an
/// unmapped leaf of our own making. A free function (not a method) so the
/// save core is unit-testable against a tempdir store without an app.
fn save_layout_self_consistent(
    store: &Mutex<HistoryStore>,
    tree: &Tree<PaneId>,
    db_pane_by_app: HashMap<u64, i64>,
    window_geometry: Option<crate::persist::layout::WindowGeometry>,
    now_ms: i64,
) -> Option<HashSet<PaneId>> {
    if db_pane_by_app.is_empty() {
        return None; // nothing persistable (degraded mode)
    }
    let mut tree = tree.clone();
    let keep: HashSet<PaneId> = db_pane_by_app.keys().map(|app_id| PaneId(*app_id)).collect();
    let survivors = prune_tree_to_mapped(&mut tree, &keep);
    if survivors.is_empty() {
        return None; // pruned to nothing
    }
    let db_pane_by_app: HashMap<u64, i64> = db_pane_by_app
        .into_iter()
        .filter(|(app_id, _)| survivors.contains(&PaneId(*app_id)))
        .collect();
    let blob = match (crate::persist::layout::SavedLayout { tree, db_pane_by_app, window_geometry })
        .to_blob()
    {
        Ok(b) => b,
        Err(e) => {
            eprintln!("termica: layout serialize failed: {e}");
            return None;
        }
    };
    if let Ok(store) = store.lock()
        && let Err(e) = store.save_layout(&blob, now_ms)
    {
        eprintln!("termica: layout save failed: {e}");
    }
    Some(survivors)
}

/// What [`plan_restore`] resolves: the (pruned) tile tree to install plus,
/// per surviving leaf, the durable `pane` row and last-known cwd the app
/// needs to rebuild a `Dead` pane. Block loading is deliberately left to
/// the caller (it touches the filesystem and produces heavy `Block`s).
struct RestorePlan {
    tree: Tree<PaneId>,
    panes: Vec<RestoredPanePlan>,
    next_pane_id: u64,
}

struct RestoredPanePlan {
    pane_id: PaneId,
    db_pane: i64,
    cwd: Option<PathBuf>,
}

/// Decide what to restore from the most recent saved layout, or `None`
/// when there is nothing restorable (spec/08 §"Restore semantics"):
///
/// - No saved blob / unparseable / no leaves → `None` (fresh start).
/// - **Liveness (conservative, workspace-level):** if any mapped pane's
///   latest session lock is currently held, the workspace belongs to a
///   running Termica → `None`, never partially adopt it. This is the
///   safety rule (point 1), distinct from the leaf pruning below.
/// - **Resilience:** keep only leaves the blob maps to a durable db row,
///   prune the rest (collapsing emptied containers). One unmapped leaf
///   never aborts the whole restore. `None` only when *nothing* survives.
///
/// A free function so the full restore decision — including liveness —
/// is unit-testable against a tempdir `Persistence` without standing up
/// an eframe app or spawning a PTY.
/// Is `session_id`'s lock held by a *live* process? Used by restore's
/// liveness gate: a held lock means a running Termica owns that session,
/// so the workspace must not be adopted.
///
/// Retries briefly because a held lock can look held *transiently* even
/// when no process truly owns it — `flock` lives on the open
/// file-description, so a sibling probe (a concurrently-spawning shell
/// that inherits the fd across `fork` until CLOEXEC, or a gc liveness
/// check) can hold it for an instant. A genuinely-live session stays held
/// across every retry. Mirrors gc's `session_is_live`, but a probe ERROR
/// (missing / inaccessible session dir) resolves to **not live** — a gone
/// dir is a dead session, and restore must not be blocked by it.
fn session_held_by_live_process(
    persist: &crate::persist::store::Persistence,
    session_id: i64,
) -> bool {
    const ATTEMPTS: u32 = 5;
    let dir = persist.session_dir(session_id);
    for attempt in 0..ATTEMPTS {
        match crate::persist::lock::SessionLock::try_acquire(&dir) {
            Ok(Some(_guard)) => return false, // free -> not live (guard dropped here)
            Ok(None) => {}                    // held -> maybe transient; retry
            Err(_) => return false, // can't probe -> not a live owner; don't block restore
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    true // persistently held -> genuinely live
}

fn plan_restore(
    persist: &crate::persist::store::Persistence,
    store: &Arc<Mutex<HistoryStore>>,
) -> Option<RestorePlan> {
    let blob = store.lock().ok().and_then(|s| s.latest_layout().ok().flatten())?;
    let mut layout = match crate::persist::layout::SavedLayout::from_blob(&blob) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("termica: layout parse failed, starting fresh: {e}");
            return None;
        }
    };
    let leaves = layout.pane_ids();
    if leaves.is_empty() {
        return None;
    }

    // Liveness: any session lock held by a *live* process → the workspace
    // is live elsewhere, leave it alone. The probe retries briefly so a
    // sub-millisecond transient hold doesn't spuriously decline a restore.
    for &db_pane in layout.db_pane_by_app.values() {
        let Some(sid) =
            store.lock().ok().and_then(|s| s.latest_session_for_pane(db_pane).ok().flatten())
        else {
            continue;
        };
        if session_held_by_live_process(persist, sid) {
            return None; // held by a live process -> don't steal it
        }
    }

    // Resilience: keep only mapped leaves; prune the rest.
    let keep: HashSet<PaneId> =
        leaves.iter().copied().filter(|p| layout.db_pane_by_app.contains_key(&p.0)).collect();
    if keep.is_empty() {
        eprintln!("termica: saved layout has no mapped panes; starting fresh");
        return None;
    }
    let survivors = prune_tree_to_mapped(&mut layout.tree, &keep);
    if survivors.is_empty() || layout.tree.root.is_none() {
        return None;
    }
    if survivors.len() < leaves.len() {
        eprintln!(
            "termica: restored {} of {} saved panes; pruned {} unmapped leaf/leaves",
            survivors.len(),
            leaves.len(),
            leaves.len() - survivors.len()
        );
    }

    let mut panes = Vec::with_capacity(survivors.len());
    let mut next_pane_id = 0u64;
    for pane_id in &survivors {
        let db_pane = layout.db_pane_by_app[&pane_id.0];
        let cwd =
            store.lock().ok().and_then(|s| s.pane_cwd(db_pane).ok().flatten()).map(PathBuf::from);
        panes.push(RestoredPanePlan { pane_id: *pane_id, db_pane, cwd });
        next_pane_id = next_pane_id.max(pane_id.0 + 1);
    }
    Some(RestorePlan { tree: layout.tree, panes, next_pane_id })
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
    /// An *explicit* directory requested on the command line (the resolved
    /// positional path), or `None` if none was given. Unlike `startup_cwd`
    /// — which is always populated (falling back to `current_dir`) and
    /// seeds the first *fresh* pane — this is `Some` only when the user
    /// actually named a path. Launching with a path opens a pane there
    /// even when a saved workspace is restored: restore + a new tab at the
    /// path (spec/06). Consumed once by `bootstrap`.
    pub requested_workspace_path: Option<PathBuf>,
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
/// 3. Else `std::env::current_dir()` — but only when it is a real,
///    meaningful directory. A bare `/` is treated as "knowing nothing":
///    a GUI launch (Finder/Dock/`.dmg` via LaunchServices on macOS, or
///    some Linux desktop launchers) hands the process a cwd of `/`,
///    which is never what the user means. So `current_dir()` is honored
///    only when it is not `/`, *or* when stdin is a TTY (a deliberate
///    `cd / && termica` from a terminal is still honored).
/// 4. Else `$HOME` if set.
/// 5. Else `/`.
///
/// The ambient state (`current_dir`, `$HOME`, is-stdin-a-tty) is read
/// here and handed to [`resolve_startup_cwd_inner`], which is pure so
/// the fallback matrix is testable without depending on the test
/// runner's cwd or tty.
pub fn resolve_startup_cwd(positional_path: Option<&std::path::Path>) -> PathBuf {
    use std::io::IsTerminal;
    resolve_startup_cwd_inner(
        positional_path,
        std::env::current_dir().ok().as_deref(),
        std::env::var_os("HOME"),
        std::io::stdin().is_terminal(),
    )
}

/// Pure core of [`resolve_startup_cwd`] — all ambient state is passed in.
/// See that function's doc comment for the fallback chain.
fn resolve_startup_cwd_inner(
    positional_path: Option<&std::path::Path>,
    current_dir: Option<&std::path::Path>,
    home: Option<std::ffi::OsString>,
    stdin_is_tty: bool,
) -> PathBuf {
    if let Some(dir) = positional_path.and_then(resolve_positional_dir) {
        return dir;
    }
    // Honor `current_dir()` only when it is meaningful: not the bare
    // filesystem root `/` (the GUI-launch sentinel), unless stdin is a
    // TTY, in which case a deliberate `cd / && termica` is respected.
    if let Some(cwd) = current_dir
        && (cwd != std::path::Path::new("/") || stdin_is_tty)
    {
        return cwd.to_path_buf();
    }
    if let Some(home) = home {
        return PathBuf::from(home);
    }
    PathBuf::from("/")
}

/// Resolve an *explicit* positional path to a starting directory: the
/// path itself if it's a directory, or the parent directory of a file.
/// `None` when the path doesn't exist or has no usable parent.
///
/// Returning `Option` (rather than a fallback) lets a caller distinguish
/// "the user asked for this directory" from "no path was given" — the
/// distinction that decides whether launching with a path opens a new
/// tab on top of a restored workspace (spec/06).
pub fn resolve_positional_dir(path: &std::path::Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_dir() {
        return Some(path.to_path_buf());
    }
    let parent = path.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.to_path_buf())
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

    // ---- layout_fingerprint: detect structural changes (spec/08) --

    fn full_map() -> HashMap<u64, i64> {
        // db rows for the 5 panes the split helper builds.
        (1u64..=5).map(|i| (i, i as i64 + 100)).collect()
    }

    #[test]
    fn fingerprint_is_stable_for_an_unchanged_tree() {
        let (tree, _) = split_with_two_tab_containers();
        let map = full_map();
        assert_eq!(
            layout_fingerprint(&tree, &map, None),
            layout_fingerprint(&tree, &map, None),
            "same tree + map → same fingerprint"
        );
    }

    #[test]
    fn fingerprint_changes_when_a_pane_is_added_or_removed() {
        let (tree, _) = split_with_two_tab_containers();
        let map = full_map();
        let base = layout_fingerprint(&tree, &map, None);

        // Remove a pane (close): different leaf set → different fp.
        let (mut tree2, pairs) = split_with_two_tab_containers();
        let p3_tile = pairs.iter().find(|(p, _)| *p == PaneId(3)).unwrap().1;
        tree2.remove_recursively(p3_tile);
        let mut map2 = map.clone();
        map2.remove(&3);
        assert_ne!(
            base,
            layout_fingerprint(&tree2, &map2, None),
            "removing a pane moves the fingerprint"
        );
    }

    #[test]
    fn fingerprint_changes_when_active_tab_switches() {
        let (mut tree, pairs) = split_with_two_tab_containers();
        let map = full_map();
        let base = layout_fingerprint(&tree, &map, None);
        // Flip the left container's active tab from p1 to p2.
        let p2_tile = pairs.iter().find(|(p, _)| *p == PaneId(2)).unwrap().1;
        let parent = tree.tiles.parent_of(p2_tile).expect("p2 has a parent");
        if let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) = tree.tiles.get_mut(parent)
        {
            tabs.set_active(p2_tile);
        } else {
            panic!("expected a Tabs parent");
        }
        assert_ne!(
            base,
            layout_fingerprint(&tree, &map, None),
            "switching the active tab moves the fingerprint"
        );
    }

    #[test]
    fn fingerprint_changes_when_db_mapping_changes_but_not_on_unrelated_cwd() {
        // The fingerprint includes the db mapping (so a pane gaining its
        // durable row triggers a save) but NOT cwd (persisted separately).
        let (tree, _) = split_with_two_tab_containers();
        let map = full_map();
        let base = layout_fingerprint(&tree, &map, None);

        let mut remapped = map.clone();
        remapped.insert(3, 999); // pane 3 now maps to a different db row
        assert_ne!(
            base,
            layout_fingerprint(&tree, &remapped, None),
            "db mapping change moves the fingerprint"
        );

        // cwd is not part of the fingerprint at all — there is no cwd
        // input, which documents the exclusion: an idle pane changing
        // directories never triggers a layout write.
        assert_eq!(
            base,
            layout_fingerprint(&tree, &map, None),
            "identical inputs → identical fingerprint"
        );
    }

    #[test]
    fn fingerprint_tracks_window_geometry_but_ignores_subpixel_jitter() {
        use crate::persist::layout::WindowGeometry;
        let (tree, _) = split_with_two_tab_containers();
        let map = full_map();
        let g =
            WindowGeometry { inner_width: 1200.0, inner_height: 800.0, pos_x: 10.0, pos_y: 20.0 };
        let base = layout_fingerprint(&tree, &map, Some(g));

        // A real move (≥ 1 pt) moves the fingerprint → arms a save.
        let moved = WindowGeometry { pos_x: 40.0, ..g };
        assert_ne!(
            base,
            layout_fingerprint(&tree, &map, Some(moved)),
            "a window move is persisted"
        );

        // Sub-point jitter (< 0.5 pt, rounds to the same whole point) does
        // NOT move it — otherwise every frame of a HiDPI drag re-arms.
        let jitter = WindowGeometry { pos_x: 10.3, inner_width: 1200.4, ..g };
        assert_eq!(
            base,
            layout_fingerprint(&tree, &map, Some(jitter)),
            "sub-point jitter is quantized away"
        );

        // Presence/absence of geometry is itself a change.
        assert_ne!(base, layout_fingerprint(&tree, &map, None), "gaining/losing geometry counts");
    }

    // ---- debounce_decide: coalesce a burst into one save ----------

    #[test]
    fn debounce_arms_on_change_and_fires_after_the_window() {
        // t=0: a structural change arms the deadline at 0 + 1.0; no save yet.
        let (save, deadline) = debounce_decide(0.0, None, true);
        assert!(!save, "no save on the frame the change happens");
        assert_eq!(deadline, Some(LAYOUT_SAVE_DEBOUNCE_SECS));

        // t=0.5: no change, still before the deadline → no save, deadline kept.
        let (save, deadline) = debounce_decide(0.5, deadline, false);
        assert!(!save);
        assert_eq!(deadline, Some(LAYOUT_SAVE_DEBOUNCE_SECS));

        // t=1.0: at the deadline → fire and disarm.
        let (save, deadline) = debounce_decide(1.0, deadline, false);
        assert!(save, "save fires at the deadline");
        assert_eq!(deadline, None, "deadline disarms after firing");
    }

    #[test]
    fn debounce_coalesces_a_later_change_by_resetting_the_deadline() {
        // Change at t=0 arms deadline 1.0.
        let (_, deadline) = debounce_decide(0.0, None, true);
        assert_eq!(deadline, Some(1.0));
        // A second change at t=0.5 pushes the deadline to 1.5 (coalescing).
        let (save, deadline) = debounce_decide(0.5, deadline, true);
        assert!(!save);
        assert_eq!(deadline, Some(1.5), "a later change resets the debounce window");
        // At t=1.0 the (reset) deadline hasn't passed → still no save.
        let (save, deadline) = debounce_decide(1.0, deadline, false);
        assert!(!save, "the earlier deadline was superseded");
        assert_eq!(deadline, Some(1.5));
        // At t=1.5 it fires once.
        let (save, deadline) = debounce_decide(1.5, deadline, false);
        assert!(save);
        assert_eq!(deadline, None);
    }

    #[test]
    fn debounce_idle_with_no_pending_save_does_nothing() {
        let (save, deadline) = debounce_decide(42.0, None, false);
        assert!(!save);
        assert_eq!(deadline, None);
    }

    // ---- prune_tree_to_mapped: self-consistent layout (spec/08) ---

    /// Every Tabs container's `active` must reference one of its own
    /// children — the invariant `revalidate_tabs_active` guarantees.
    fn assert_tabs_active_valid(tree: &Tree<PaneId>) {
        for (_, tile) in tree.tiles.iter() {
            if let Tile::Container(egui_tiles::Container::Tabs(tabs)) = tile
                && let Some(active) = tabs.active
            {
                assert!(
                    tabs.children.contains(&active),
                    "Tabs.active {active:?} is not one of its own children {:?}",
                    tabs.children
                );
            }
        }
    }

    fn leaf_set(tree: &Tree<PaneId>) -> HashSet<PaneId> {
        tree.tiles
            .tiles()
            .filter_map(|t| match t {
                Tile::Pane(p) => Some(*p),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn prune_keeps_mapped_leaves_and_drops_the_rest() {
        let (mut tree, _) = split_with_two_tab_containers();
        let keep: HashSet<PaneId> = [PaneId(1), PaneId(3)].into_iter().collect();
        let survivors = prune_tree_to_mapped(&mut tree, &keep);
        assert_eq!(survivors, keep, "exactly the kept panes survive");
        assert_eq!(leaf_set(&tree), keep, "the tree's leaves match the survivors");
        assert!(tree.root.is_some(), "a non-empty prune leaves a valid root");
        assert_tabs_active_valid(&tree);
    }

    #[test]
    fn prune_collapses_an_emptied_split_side() {
        // Drop the whole right container {p3,p4,p5}: the right Tabs empties
        // and is pruned, the horizontal split is left single-child and
        // collapses, so the surviving panes still form a valid tree.
        let (mut tree, _) = split_with_two_tab_containers();
        let keep: HashSet<PaneId> = [PaneId(1), PaneId(2)].into_iter().collect();
        let survivors = prune_tree_to_mapped(&mut tree, &keep);
        assert_eq!(survivors, keep);
        assert_eq!(leaf_set(&tree), keep);
        assert!(tree.root.is_some());
        assert_tabs_active_valid(&tree);
    }

    #[test]
    fn prune_to_nothing_empties_the_tree() {
        let (mut tree, _) = split_with_two_tab_containers();
        let survivors = prune_tree_to_mapped(&mut tree, &HashSet::new());
        assert!(survivors.is_empty(), "no survivors when nothing is kept");
        assert!(
            tree.root.is_none(),
            "an emptied tree has no root → restore falls through to fresh"
        );
    }

    #[test]
    fn pruned_tree_round_trips_with_no_unmapped_leaf() {
        use crate::persist::layout::SavedLayout;
        let (mut tree, _) = split_with_two_tab_containers();
        let keep: HashSet<PaneId> = [PaneId(2), PaneId(4)].into_iter().collect();
        let survivors = prune_tree_to_mapped(&mut tree, &keep);
        // Build a self-consistent SavedLayout: map ONLY the survivors.
        let db_pane_by_app: HashMap<u64, i64> =
            survivors.iter().map(|p| (p.0, p.0 as i64 + 100)).collect();
        let blob = SavedLayout { tree, db_pane_by_app, window_geometry: None }.to_blob().unwrap();
        let back = SavedLayout::from_blob(&blob).unwrap();
        for p in back.pane_ids() {
            assert!(
                back.db_pane_by_app.contains_key(&p.0),
                "every restored leaf maps to a db row — no unmapped leaf in our own blob"
            );
        }
        assert_eq!(leaf_set(&back.tree), keep);
    }

    // ---- plan_restore / save_layout_self_consistent (spec/08) -----

    fn persistence()
    -> (tempfile::TempDir, crate::persist::store::Persistence, Arc<Mutex<HistoryStore>>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store =
            Arc::new(Mutex::new(HistoryStore::open(&tmp.path().join("termica.sqlite")).unwrap()));
        let persist =
            crate::persist::store::Persistence::new(tmp.path().to_path_buf(), store.clone());
        (tmp, persist, store)
    }

    fn tabs_tree(ids: &[u64]) -> Tree<PaneId> {
        let mut tiles: Tiles<PaneId> = Tiles::default();
        let panes: Vec<TileId> = ids.iter().map(|i| tiles.insert_pane(PaneId(*i))).collect();
        let root = tiles.insert_tab_tile(panes);
        Tree::new("test", root, tiles)
    }

    /// Persist a layout blob verbatim (no self-consistency prune) — used to
    /// simulate an older / externally-mangled blob whose tree references a
    /// leaf the map doesn't resolve, which restore must survive.
    fn save_raw_layout(
        store: &Arc<Mutex<HistoryStore>>,
        tree: Tree<PaneId>,
        map: HashMap<u64, i64>,
    ) {
        let blob = crate::persist::layout::SavedLayout {
            tree,
            db_pane_by_app: map,
            window_geometry: None,
        }
        .to_blob()
        .unwrap();
        store.lock().unwrap().save_layout(&blob, 1000).unwrap();
    }

    #[test]
    fn plan_restore_keeps_mapped_panes_and_prunes_unmapped() {
        let (_tmp, persist, store) = persistence();
        // Two real persisted panes; their session locks released (the
        // writing process has exited) so the workspace is adoptable.
        let row_a = persist.begin_session(Some("/work/a"), "zsh", 1).unwrap().pane_row.0;
        let row_b = persist.begin_session(Some("/work/b"), "zsh", 2).unwrap().pane_row.0;
        // An INCONSISTENT blob: leaf p3 has no db mapping.
        let map: HashMap<u64, i64> = [(1u64, row_a), (2, row_b)].into_iter().collect();
        save_raw_layout(&store, tabs_tree(&[1, 2, 3]), map);

        let plan = plan_restore(&persist, &store).expect("a partially-mapped layout is restorable");
        let got: HashSet<PaneId> = plan.panes.iter().map(|p| p.pane_id).collect();
        assert_eq!(
            got,
            [PaneId(1), PaneId(2)].into_iter().collect(),
            "mapped panes restore; the unmapped p3 is pruned — NOT a whole-workspace abort"
        );
        assert_eq!(leaf_set(&plan.tree), got, "the installed tree's leaves match the survivors");
        assert_eq!(plan.next_pane_id, 3, "next pane id is max(survivor) + 1");
        // cwd is recovered from the pane row (a pane with no chunks still
        // restores — as an empty Dead pane carrying its cwd).
        let cwd_b = plan.panes.iter().find(|p| p.pane_id == PaneId(2)).unwrap().cwd.clone();
        assert_eq!(cwd_b, Some(PathBuf::from("/work/b")));
    }

    #[test]
    fn plan_restore_with_no_mapped_panes_starts_fresh() {
        let (_tmp, persist, store) = persistence();
        save_raw_layout(&store, tabs_tree(&[1, 2]), HashMap::new());
        assert!(plan_restore(&persist, &store).is_none(), "no mapped panes → fresh start");
    }

    #[test]
    fn session_held_by_live_process_distinguishes_held_free_and_missing() {
        let (_tmp, persist, _store) = persistence();
        // Held: keep the SessionRecord (and its lock) alive.
        let rec = persist.begin_session(None, "zsh", 1).unwrap();
        let sid = rec.session.0;
        assert!(session_held_by_live_process(&persist, sid), "a held session lock reads as live");
        drop(rec); // process exits -> lock released
        assert!(
            !session_held_by_live_process(&persist, sid),
            "after the lock releases the session reads as not live"
        );
        // A missing session dir is a dead session, not a live owner — it
        // must not block restore (would otherwise force a fresh start).
        assert!(
            !session_held_by_live_process(&persist, 999_999),
            "a missing session dir resolves to not-live"
        );
    }

    #[test]
    fn plan_restore_declines_a_live_workspace_then_adopts_after_release() {
        let (_tmp, persist, store) = persistence();
        // Hold the session lock: the workspace is live in "another" process.
        let rec = persist.begin_session(Some("/work/a"), "zsh", 1).unwrap();
        let row_a = rec.pane_row.0;
        save_raw_layout(&store, tabs_tree(&[1]), [(1u64, row_a)].into_iter().collect());
        assert!(
            plan_restore(&persist, &store).is_none(),
            "a workspace whose session lock is held is never adopted (liveness is conservative)"
        );
        drop(rec); // process exits / crashes -> kernel releases the lock
        assert!(
            plan_restore(&persist, &store).is_some(),
            "after the lock releases, the orphaned workspace is adopted (crash recovery)"
        );
    }

    #[test]
    fn save_layout_self_consistent_persists_without_quit_and_prunes() {
        // The headline regression: the layout is persisted by a path that
        // is NOT the quit hook, and the persisted blob is self-consistent
        // (a degraded/unmapped leaf is pruned before write).
        let (_tmp, _persist, store) = persistence();
        let (tree, _) = split_with_two_tab_containers(); // 5 panes
        let map: HashMap<u64, i64> = [(1u64, 101i64), (3, 103)].into_iter().collect();
        let survivors = save_layout_self_consistent(&store, &tree, map, None, 1000)
            .expect("a layout was persisted");
        assert_eq!(survivors, [PaneId(1), PaneId(3)].into_iter().collect());

        let blob = store
            .lock()
            .unwrap()
            .latest_layout()
            .unwrap()
            .expect("a layout is on disk without any quit having happened");
        let back = crate::persist::layout::SavedLayout::from_blob(&blob).unwrap();
        assert_eq!(leaf_set(&back.tree), [PaneId(1), PaneId(3)].into_iter().collect());
        for p in back.pane_ids() {
            assert!(
                back.db_pane_by_app.contains_key(&p.0),
                "no unmapped leaf in the persisted blob — self-consistent"
            );
        }
    }

    // ---- resolve_startup_cwd: spec/06 fallback chain --------------

    #[test]
    fn resolve_positional_dir_directory_file_and_missing() {
        // The explicit-path resolver returns the dir for a directory, the
        // PARENT dir for a file, and `None` when there is no usable path —
        // distinct from "no arg given", so the caller can tell an explicit
        // request apart from the cwd fallback (it decides whether to open
        // a new tab on restore).
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(resolve_positional_dir(dir.path()), Some(dir.path().to_path_buf()));

        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"x").expect("write");
        assert_eq!(resolve_positional_dir(&file), Some(dir.path().to_path_buf()));

        let bogus = std::path::Path::new("/no/such/path/ever-termica-xyz");
        assert_eq!(resolve_positional_dir(bogus), None, "non-existent path → None, not a fallback");
    }

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

    // ---- resolve_startup_cwd_inner: the `/` GUI-launch carve-out ----

    #[test]
    fn startup_cwd_slash_from_gui_launch_falls_to_home() {
        // The reported bug: a Finder/.dmg launch hands the process cwd
        // `/`, which must NOT become the first pane's cwd. With no
        // positional path and a non-TTY stdin, `/` falls through to $HOME.
        let home = std::ffi::OsString::from("/home/u");
        let got = resolve_startup_cwd_inner(
            None,
            Some(std::path::Path::new("/")),
            Some(home),
            false, // not a tty → GUI launch
        );
        assert_eq!(got, PathBuf::from("/home/u"));
    }

    #[test]
    fn startup_cwd_slash_with_home_unset_is_root() {
        let got = resolve_startup_cwd_inner(None, Some(std::path::Path::new("/")), None, false);
        assert_eq!(got, PathBuf::from("/"), "only `/` when $HOME is also unset");
    }

    #[test]
    fn startup_cwd_real_dir_is_used_as_is() {
        // A meaningful current_dir (terminal launch in a project) is
        // honored regardless of $HOME.
        let got = resolve_startup_cwd_inner(
            None,
            Some(std::path::Path::new("/work/proj")),
            Some(std::ffi::OsString::from("/home/u")),
            false,
        );
        assert_eq!(got, PathBuf::from("/work/proj"));
    }

    #[test]
    fn startup_cwd_slash_from_terminal_is_honored() {
        // A deliberate `cd / && termica` from a shell (stdin is a TTY)
        // keeps `/` rather than overriding it with $HOME.
        let got = resolve_startup_cwd_inner(
            None,
            Some(std::path::Path::new("/")),
            Some(std::ffi::OsString::from("/home/u")),
            true, // tty
        );
        assert_eq!(got, PathBuf::from("/"));
    }

    #[test]
    fn startup_cwd_positional_dir_wins_over_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let got = resolve_startup_cwd_inner(
            Some(dir.path()),
            Some(std::path::Path::new("/")),
            Some(std::ffi::OsString::from("/home/u")),
            false,
        );
        assert_eq!(got, dir.path().to_path_buf(), "an explicit path always wins");
    }
}
