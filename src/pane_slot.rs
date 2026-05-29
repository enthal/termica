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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// "Force the scroll area to the bottom on next render." Set by
    /// `render_pane` when the editor's `Enter` submit fires; the
    /// ScrollArea normally only sticks to the bottom when the user
    /// is already there, which means a submit while scrolled up
    /// would leave the new output off-screen. Cleared after one
    /// frame's render consumes it.
    pub(crate) scroll_to_bottom_pending: bool,
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
