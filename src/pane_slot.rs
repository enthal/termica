//! Pane identity + per-pane state.
//!
//! Split out of `lib.rs` so the data types are independent of the
//! eframe app loop, the egui_tiles `Behavior` shim, and the rendering
//! code that drives them. All four are owned by `lib.rs` (`TermicaApp`,
//! `TabBehavior`, `render_pane`); they consume `PaneId` / `PaneSlot` /
//! `PaneUiState` by reference.

use eframe::egui;

use crate::block::BlockId;
use crate::block_selection::BlockCursor;
use crate::pane::PaneSession;

/// Typed pane identifier. Used both as the leaf payload inside
/// [`egui_tiles::Tree`] and as the key into `TermicaApp::panes`.
///
/// Newtype around `u64` so the type system distinguishes pane IDs
/// from other identifier flavours that arrive in later phases
/// (`CommandRunId`, `HistoryEntryId`, …). `Copy + Eq + Hash` so it
/// works as both a tree payload and a HashMap key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PaneId(pub u64);

/// App-level intents staged by `render_pane` when the focused pane
/// saw a shortcut keystroke. Applied by `TermicaApp::update` after
/// `tree.ui()` returns (because mutating the tree from inside a
/// Behavior callback is unsupported).
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
    /// Cmd+K (macOS) / Ctrl+Shift+K (Linux): full reset of the
    /// focused pane — blank the viewport, drop scrollback, move
    /// the cursor home. The shell process is untouched and will
    /// redraw its prompt on the next prompt cycle.
    ClearScrollback,
    /// Cmd+Option+Up (macOS) / Ctrl+Alt+Up (Linux/Windows), or
    /// `Ctrl+Home` (both): jump the pane's scroll position to the
    /// very top of the scrollback (the oldest sealed block becomes
    /// visible at the pane viewport top). No-op in alt-screen mode —
    /// the running program owns the viewport there.
    ScrollToTop,
    /// Cmd+Option+Down (macOS) / Ctrl+Alt+Down (Linux/Windows), or
    /// `Ctrl+End` (both): jump to the live tail (editor / running
    /// grid). Same chord family as `ScrollToTop`; mirror direction.
    ScrollToBottom,
    /// `Ctrl+PageUp` (both platforms): scroll the pane's scrollback
    /// viewport up by one page (toward older output). No-op in
    /// alt-screen mode. Distinct from bare `PageUp`, which moves the
    /// editor caret to the buffer start.
    ScrollPageUp,
    /// `Ctrl+PageDown` (both platforms): scroll the pane's scrollback
    /// viewport down by one page (toward the live tail). Mirror of
    /// `ScrollPageUp`.
    ScrollPageDown,
    /// Cmd+F (macOS) / Ctrl+Shift+F (Linux/Windows): open the in-pane
    /// find overlay (Phase 8). No-op in alt-screen mode — the running
    /// full-screen program owns the viewport, and the transcript it
    /// would search isn't visible.
    OpenFind,
    /// Restart a `Dead` pane (9F): spawn a fresh shell into it, keeping
    /// the restored scrollback. Staged when the user clicks the
    /// "Restart shell" affordance on a restored/exited pane.
    RestartShell,
}

/// Per-pane UI interaction state. Each pane gets its own multi-
/// click counter + last-press snapshot — a press in pane A and a
/// press in pane B must never chain into a "double-click".
#[derive(Debug, Default)]
pub struct PaneUiState {
    pub(crate) last_press_time: f64,
    pub(crate) last_press_pos: egui::Pos2,
    pub(crate) click_count: u8,
    /// Last `(rows, cols)` we resized this PTY to. Cached so we
    /// only issue a resize when the cell grid actually changes,
    /// not every frame.
    pub(crate) last_size: Option<(u16, u16)>,
    /// Debounce state for PTY resizes: the `(rows, cols)` target the
    /// pane is currently settling toward, plus the egui-time at which
    /// that target was first seen. An interactive window drag changes
    /// the computed size every frame; pushing a SIGWINCH on each one
    /// storms the child (a TUI like Claude Code repaints per SIGWINCH,
    /// and the interleaved partial repaints pile orphaned frames into
    /// scrollback). We only commit the resize once the target has held
    /// steady for a short debounce. `None` when the size is settled.
    /// See [`crate::render_pane::plan_resize`].
    pub(crate) pending_size: Option<((u16, u16), f64)>,
    /// Did this pane hold keyboard focus on the most-recently
    /// completed frame? Written by `render_pane` right after the
    /// terminal grid is painted; read by `TermicaApp::update`
    /// after `tree.ui()` returns so the focused tab can be styled
    /// on the next frame.
    pub focused: bool,
    /// "Grab keyboard focus on the next render". Set by
    /// `TermicaApp::update` when an event makes this pane newly
    /// active (drag-drop, new-tab spawn, tab click). Consumed by
    /// `render_pane` which calls `request_focus` on the grid
    /// widget and clears the flag.
    pub needs_focus: bool,
    /// App-level shortcut intent staged this frame (Cmd+T,
    /// Cmd+Shift+], …). Drained by `TermicaApp::update` after
    /// `tree.ui()` returns; `render_pane` sets it when it sees
    /// the matching shortcut in the focused pane's event stream.
    pub pending_action: Option<PaneAction>,
    /// Anchor byte range remembered when a double or triple click
    /// landed in the editor: the original word / line that the
    /// user clicked on. Subsequent drag expands the selection so
    /// it always covers the original anchor range PLUS every word
    /// (or line) the pointer has touched since. `None` between
    /// drags. Cleared on a single click. Used only by
    /// `render_pane`'s editor-drag handler.
    pub(crate) editor_drag_anchor: Option<(usize, usize)>,
    /// Same idea as `editor_drag_anchor`, but for a double/triple
    /// click that landed inside a sealed block. `BlockId` pins the
    /// drag to its origin block — pointer movements over other
    /// blocks don't extend it (cross-block selection lands in a
    /// follow-up). The two `BlockCursor`s are the start and end of
    /// the originally-selected word / line on the anchor row, so
    /// the drag handler can union them with the word / line under
    /// the current pointer.
    pub(crate) sealed_drag_anchor: Option<(BlockId, BlockCursor, BlockCursor)>,
    /// Phase 9B: per-width reflow of each sealed block's logical lines
    /// into the visual rows painted at the current pane width, plus the
    /// logical↔visual coordinate map. Rebuilt by `render_pane` when the
    /// width or the set of sealed blocks changes; read by paint,
    /// hit-testing, and find-highlight. See [`crate::reflow`].
    pub(crate) reflow_cache: crate::reflow::ReflowCache,
    /// True for the duration of a pointer gesture that opened a link
    /// (a modifier-click on a URL / file path). Such a gesture must not
    /// also extend a selection on the drag frames that follow the
    /// press, so the drag-extend handlers consult this. Set when the
    /// press opens a link; reset on every fresh primary press. Read
    /// only by `render_pane`'s grid / sealed drag handlers via
    /// [`drag_extends_selection`](crate::render_pane).
    pub(crate) gesture_opened_link: bool,
    /// "Force the scroll area to the bottom on next render." Set by
    /// `render_pane` when the editor's `Enter` submit fires; the
    /// ScrollArea normally only sticks to the bottom when the user
    /// is already there, which means a submit while scrolled up
    /// would leave the new output off-screen. Cleared after one
    /// frame's render consumes it. Also set by `PaneAction::ScrollToBottom`
    /// (Cmd+Option+Down / Ctrl+Alt+Down).
    pub(crate) scroll_to_bottom_pending: bool,
    /// Number of consecutive frames to keep forcing the scroll to
    /// bottom after a submit. The Preexec → Running transition
    /// adds a command-label row to the block stack on a LATER
    /// frame than the submit, and stick_to_bottom alone wasn't
    /// reliably keeping the live tail pinned through that
    /// transition (user landed at the TOP of the new Running
    /// block). A short multi-frame snap window walks through the
    /// transition and lands at the actual bottom. Decremented
    /// each frame in `render_pane`; the snap fires while > 0.
    pub(crate) scroll_to_bottom_frames: u8,
    /// "Force the scroll area to the top on next render." Set by
    /// `PaneAction::ScrollToTop` (Cmd+Option+Up / Ctrl+Alt+Up) so
    /// the user can jump to the oldest sealed block. Mirror of
    /// `scroll_to_bottom_pending`. Cleared after one frame.
    pub(crate) scroll_to_top_pending: bool,
    /// Pending page-scroll of the scrollback viewport, in pages.
    /// Positive = scroll up (toward older output, `Ctrl+PageUp`),
    /// negative = scroll down (`Ctrl+PageDown`). Set by
    /// `PaneAction::ScrollPage{Up,Down}`; `render_pane` turns it into a
    /// `Ui::scroll_with_delta` (one page = the measured viewport height
    /// minus one row) and resets to 0. Accumulates if several fire
    /// before a render consumes them.
    pub(crate) scroll_page_pending: i32,
    /// Countdown of frames to keep `stick_to_bottom` disabled after a
    /// page-scroll. `scroll_with_delta` *animates* the offset over ~0.1–
    /// 0.3 s; if `stick_to_bottom` re-engages mid-animation while we
    /// started pinned at the tail, egui re-snaps to the bottom and the
    /// page-scroll is cancelled (the same hazard the find overlay
    /// documents). Holding stick off until the animation settles lets it
    /// play out; reaching the bottom on a page-down still re-pins via
    /// egui's `offset == max` check. Decremented each frame in
    /// `render_pane`.
    pub(crate) scroll_page_frames: u8,
    /// The scroll area's visible viewport height as of the last render
    /// (`inner_rect.height()`), cached so the next `Ctrl+PageUp/PageDown`
    /// pages by the *actual* visible height (one line of overlap) rather
    /// than the area's max-height bound, which overshoots.
    pub(crate) last_scroll_viewport_h: f32,
    /// Last `bell_count` we observed on this pane's terminal. When
    /// the live count exceeds this on a render, a new bell happened
    /// since last frame and the visible flash kicks off.
    pub(crate) bell_last_seen: u64,
    /// Wall-clock time (`ctx.input(|i| i.time)`) at which the most
    /// recent bell-driven flash started. `None` when no flash is
    /// active; the flash fades out over `BELL_FLASH_SECS`.
    pub(crate) bell_flash_started_at: Option<f64>,
    /// `Some` while the Ctrl+R history overlay is open. Holds the
    /// query string the user is typing, the active scope, the
    /// current selection index, and the cached result snapshot.
    /// Cleared on Esc / Enter / pane refocus.
    pub history_overlay: Option<crate::history_overlay::HistoryOverlay>,
    /// `Some` while the Tab completion popup is open. Holds the
    /// merged candidate list, the original typed token, the byte
    /// range in the editor that gets replaced on accept, and the
    /// currently-highlighted row. Cleared on Esc, accept, or any
    /// non-navigation keystroke. See [`crate::completion`] +
    /// [spec/04a](../spec/04a-completion.md).
    pub completion_popup: Option<crate::completion::CompletionPopup>,
    /// `Some` while a CLI-native driver result is awaited before the popup
    /// (re)opens — the "wait, don't flash locals" state ([spec/04a §"Source
    /// priority"](../spec/04a-completion.md)). Carries the origin / token /
    /// local candidates to merge when the driver result lands. A driver
    /// command sets this instead of opening a locals-only popup; the
    /// per-frame poll resolves it into [`Self::completion_popup`]. While
    /// awaiting a *refresh*, any already-open `completion_popup` stays
    /// visible so the list never blinks out between keystrokes.
    pub completion_pending: Option<crate::completion::PendingCompletion>,
    /// `Some` while the `Cmd/Ctrl+F` find overlay is open (Phase 8).
    /// Holds the query, the `Aa` / `.*` / filter toggles, the ordered
    /// match list + current selection, and this pane's query history.
    /// Survives close/reopen only via its `history` (taken across the
    /// `OpenFind` action). Cleared on Esc / the Done button.
    pub find_overlay: Option<crate::find::FindOverlay>,
    /// This pane's find-query history, kept on the pane (not the
    /// overlay) so it **survives Esc → Cmd+F reopen**. Seeded into a
    /// fresh `FindOverlay` on open and written back when the overlay
    /// closes. Newest first, in-memory for the session.
    pub(crate) find_history: Vec<String>,
    /// "Scroll the current find match into view on the next render."
    /// Set when the selection (or query) changes in the overlay; the
    /// match rects are only known after the scroll area lays out, so
    /// the scroll is applied one frame later. Consumed in `render_pane`.
    pub(crate) find_scroll_pending: bool,
    /// `true` while the `Cmd+/` keyboard-shortcuts cheat-sheet popup is
    /// open. A reference overlay; Esc / click / another popup-open
    /// combo closes it. Mutually exclusive with the other popups —
    /// opening any one closes the rest.
    pub(crate) keybindings_open: bool,
    /// Live filter text for the keybindings cheat-sheet's search field.
    /// Persists across reopen within the pane.
    pub(crate) keybindings_query: String,
    /// Editor cursor (byte index) observed on the previous frame.
    /// Used by `render_pane` to detect caret motion and reset the
    /// blink cycle to "visible" so a moved caret never lands during
    /// an "off" half-cycle. `None` when the editor wasn't active
    /// last frame.
    pub(crate) last_cursor_byte: Option<usize>,
    /// Wall-clock time (`ctx.input(|i| i.time)`) at which the
    /// current caret blink cycle started. Bumped to `now` on each
    /// detected caret motion; the visible/invisible square wave
    /// is computed relative to this anchor.
    pub(crate) caret_blink_anchor: f64,
    /// Current opacity of the focused-editor chrome, in `[0, 1]`.
    /// Driven toward `1.0` while the chrome should be shown
    /// (caret-active conditions hold), toward `0.0` otherwise.
    /// Animated over `CHROME_FADE_SECS` per direction so the chrome
    /// fades in / out instead of popping. Persisted in `slot.ui`
    /// because the chrome lives outside the painter's view of any
    /// individual widget.
    pub(crate) chrome_opacity: f32,
    /// The user dismissed the "Tab completion is limited" notice (old bash
    /// with no bash-completion) for this pane. Once dismissed it stays
    /// dismissed for the pane's life; a fresh pane shows it again.
    pub(crate) completion_notice_dismissed: bool,
}

/// One pane's full state: the OS-resource [`PaneSession`] (PTY +
/// reader thread + terminal grid) plus the [`PaneUiState`].
pub struct PaneSlot {
    pub session: PaneSession,
    pub ui: PaneUiState,
}

#[cfg(test)]
mod tests {
    use super::*;

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
