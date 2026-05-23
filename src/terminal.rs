//! VT/ANSI terminal state.
//!
//! Thin wrapper around `alacritty_terminal` that feeds bytes through
//! its parser, keeps the grid up to date, and exposes the minimum API
//! the renderer (Phase 1E) and the marker parser (Phase 3) will need.
//!
//! We never parse VT bytes ourselves — `alacritty_terminal` owns the
//! grid, cursor, alternate screen, and every escape sequence in
//! between. See [`spec/02-terminal-engine.md`](../spec/02-terminal-engine.md#terminal-state).

#![forbid(unsafe_code)]

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Grid;
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::Processor;

/// Plain snapshot of the VT mode flags the input encoder cares about.
///
/// Default is "fresh terminal" — application cursor mode off,
/// bracketed paste off, etc. Public so callers (notably
/// [`crate::input::encode_event`]) can hold the value across frames
/// without borrowing a [`TerminalState`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalModes {
    /// DECCKM. When `true`, arrow keys / Home / End should be encoded
    /// as SS3 sequences (`\eOA` etc.) instead of CSI (`\e[A` etc.).
    pub application_cursor: bool,
    /// Bracketed paste mode (DECSET 2004, `\e[?2004h`). When `true`,
    /// pasted text must be wrapped in `\e[200~` … `\e[201~` so the
    /// shell can tell pasted input apart from typed input (and skip
    /// completion / history expansion of the paste).
    pub bracketed_paste: bool,
}

/// No-op event listener. `alacritty_terminal` calls into this on bell,
/// title change, mouse cursor change, OSC events, etc. For Phase 1D
/// we discard everything; Phase 3 will replace this with a listener
/// that surfaces OSC markers onto our marker stream.
#[derive(Debug, Clone, Default)]
struct NopListener;

impl EventListener for NopListener {
    fn send_event(&self, _event: Event) {}
}

/// One terminal's grid + parser. Feed bytes via [`Self::feed`]; read
/// the grid back via [`Self::screen_text`] (testing helper) or via
/// the lower-level [`Self::with_grid`] callback (renderer / future
/// uses).
pub struct TerminalState {
    term: Term<NopListener>,
    parser: Processor,
}

impl TerminalState {
    /// Create a fresh terminal of the given dimensions in cells.
    pub fn new(rows: u16, cols: u16) -> Self {
        let size = TermSize::new(cols as usize, rows as usize);
        let config = Config::default();
        let term = Term::new(config, &size, NopListener);
        let parser = Processor::new();
        Self { term, parser }
    }

    /// Feed bytes into the VT parser. The grid mutates accordingly.
    /// Splitting a byte slice across multiple calls is safe — the
    /// parser keeps state across calls, so escape sequences split at
    /// any byte boundary still resolve correctly.
    pub fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.parser.advance(&mut self.term, &[*byte]);
        }
    }

    /// Resize the grid in place. Unlike `TerminalState::new`, this
    /// preserves the existing screen contents — the cells in the
    /// overlap region keep their characters and styles. New cells
    /// (when growing) are blank; clipped cells (when shrinking) are
    /// dropped. `alacritty_terminal::Term::resize` handles both the
    /// main and alternate screens correctly.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let size = TermSize::new(cols as usize, rows as usize);
        self.term.resize(size);
    }

    /// Shift the displayed viewport through the scrollback buffer.
    /// Positive `lines` scrolls toward older content (up); negative
    /// scrolls toward newer (down). The current view becomes
    /// `min(history_size, max(0, display_offset + lines))` lines
    /// back from the live bottom; alacritty handles the clamping.
    ///
    /// Has no effect on the alternate screen (which has no scrollback
    /// of its own), and never affects the kernel-side PTY size.
    pub fn scroll_display(&mut self, lines: i32) {
        use alacritty_terminal::grid::Scroll;
        self.term.scroll_display(Scroll::Delta(lines));
    }

    /// How many lines back from the live bottom of the scrollback we
    /// are currently viewing. `0` means "tracking the latest output";
    /// any positive value means the user scrolled up.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// True when the terminal is in alternate-screen mode (vim/htop/
    /// less/fzf/tmux territory). The pane mode machine in Phase 3
    /// uses this as a hard signal that the program below owns every
    /// keystroke.
    pub fn is_alternate_screen(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// True when the program has enabled DECCKM (application cursor
    /// keys mode, `\e[?1h`). When on, arrow keys / Home / End must be
    /// encoded as SS3 sequences (`\eOA` etc.) instead of CSI
    /// (`\e[A` etc.). Full-screen TTY programs like `less`, `vim`,
    /// `htop` rely on this to interpret arrow keys at all — the
    /// terminfo entry `kcuu1` for `xterm-256color` is `\EOA` (the
    /// SS3 form), and `less`'s keymap is bound to that, not the CSI.
    pub fn application_cursor_mode(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// True when the program has enabled bracketed paste mode
    /// (DECSET 2004, `\e[?2004h`). When on, pasted text must be wrapped
    /// in `\e[200~` … `\e[201~` so the shell can distinguish a paste
    /// from typed input.
    pub fn bracketed_paste_mode(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Snapshot the input-relevant mode flags so the input encoder
    /// can choose the correct byte sequences for the current frame.
    pub fn modes(&self) -> TerminalModes {
        TerminalModes {
            application_cursor: self.application_cursor_mode(),
            bracketed_paste: self.bracketed_paste_mode(),
        }
    }

    /// True when the cursor should be visible to the user (DECTCEM —
    /// `\e[?25h` / `\e[?25l`). When `false`, the renderer must not
    /// draw a cursor block: many full-screen programs hide the
    /// cursor while they're repainting their UI.
    pub fn is_cursor_visible(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::SHOW_CURSOR)
    }

    /// Cursor position as `(viewport_row, col)` zero-indexed into the
    /// currently displayed region. Returns `None` if the cursor's
    /// grid line falls outside the visible viewport — that happens
    /// when the user has scrolled into the scrollback far enough
    /// that the cursor's line is no longer on screen.
    ///
    /// The translation is the same as alacritty's `viewport_to_point`
    /// run in reverse: `viewport_row = grid_line + display_offset`.
    pub fn cursor_position(&self) -> Option<(usize, usize)> {
        use alacritty_terminal::grid::Dimensions;
        let grid = self.term.grid();
        let cur = grid.cursor.point;
        let display_offset = grid.display_offset() as i32;
        let viewport_row = cur.line.0 + display_offset;
        let col = cur.column.0;
        if viewport_row < 0 {
            return None;
        }
        let row = viewport_row as usize;
        if row >= grid.screen_lines() || col >= grid.columns() {
            return None;
        }
        Some((row, col))
    }

    /// Borrow the current cell grid. The renderer walks this directly
    /// to paint cells; the listener type is intentionally not in the
    /// signature so callers don't depend on our private listener.
    pub fn grid(&self) -> &Grid<Cell> {
        self.term.grid()
    }

    /// Render the current visible grid as plain UTF-8 text, one row
    /// per line, trailing whitespace preserved. Test-and-debug
    /// helper; the real renderer paints cells directly and never
    /// allocates a string per frame.
    pub fn screen_text(&self) -> String {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line, Point};

        let grid = self.term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let mut out = String::with_capacity(rows * (cols + 1));
        for row in 0..rows {
            for col in 0..cols {
                let point = Point::new(Line(row as i32), Column(col));
                let cell = &grid[point];
                let c = cell.c;
                // alacritty represents empty cells as ' ' (default).
                out.push(c);
            }
            if row + 1 < rows {
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the VT state. No PTY required — we feed
    //! synthetic byte streams and assert what the grid says.
    //!
    //! Strict-layer rule: these tests exist BEFORE the wrapper's
    //! shape is set in stone. A new behavior gets a test first.

    use super::*;

    fn assert_row_contains(state: &TerminalState, row: usize, expected: &str) {
        let text = state.screen_text();
        let line = text.lines().nth(row).unwrap_or("");
        assert!(
            line.contains(expected),
            "row {row} did not contain {expected:?}; row was: {line:?}\nfull screen:\n{text}"
        );
    }

    #[test]
    fn fresh_terminal_has_blank_screen() {
        let state = TerminalState::new(5, 20);
        let text = state.screen_text();
        // 5 rows separated by 4 newlines; every cell is the default
        // space character.
        assert_eq!(text.lines().count(), 5);
        for (i, line) in text.lines().enumerate() {
            assert!(
                line.chars().all(|c| c == ' '),
                "row {i} should be all spaces but was {line:?}"
            );
        }
    }

    #[test]
    fn plain_text_lands_on_the_first_row() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"hello world");
        assert_row_contains(&state, 0, "hello world");
    }

    #[test]
    fn cr_lf_moves_to_the_next_row() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"one\r\ntwo\r\nthree");
        assert_row_contains(&state, 0, "one");
        assert_row_contains(&state, 1, "two");
        assert_row_contains(&state, 2, "three");
    }

    #[test]
    fn ansi_sgr_does_not_corrupt_text() {
        // Red text "fail" wrapped in CSI 31m ... CSI 0m. The
        // sequences are invisible; the text remains.
        let mut state = TerminalState::new(5, 20);
        state.feed(b"\x1b[31mfail\x1b[0m ok");
        assert_row_contains(&state, 0, "fail ok");
    }

    #[test]
    fn alternate_screen_toggles_with_csi_1049() {
        let mut state = TerminalState::new(5, 20);
        assert!(!state.is_alternate_screen());

        // Plant content on the main screen. We will assert it
        // survives a round trip through the alternate screen.
        state.feed(b"main-text\r\n");
        assert_row_contains(&state, 0, "main-text");

        // Enter alt screen. `CSI ? 1049 h` saves the cursor and
        // switches buffers; it does NOT reposition the cursor on
        // alacritty (this matches the de-facto interpretation —
        // programs that want a clean start typically send `CSI H`
        // themselves). So we issue an explicit cursor-home before
        // writing so the test reads from a known position.
        state.feed(b"\x1b[?1049h"); // alt-screen on
        state.feed(b"\x1b[H"); // cursor home
        assert!(state.is_alternate_screen());

        state.feed(b"alt-text");
        assert_row_contains(&state, 0, "alt-text");

        // Leave alt screen — the main screen comes back exactly as it
        // was before. (This is the property vim/less/htop rely on.)
        state.feed(b"\x1b[?1049l"); // alt-screen off
        assert!(!state.is_alternate_screen());
        assert_row_contains(&state, 0, "main-text");
    }

    #[test]
    fn bytes_split_across_feed_calls_still_render() {
        // The renderer-side guarantee: an escape sequence that
        // straddles a read boundary still resolves correctly. We
        // simulate this by chunking byte-by-byte.
        let stream = b"\x1b[31mred\x1b[0m";
        let mut state = TerminalState::new(5, 20);
        for byte in stream {
            state.feed(&[*byte]);
        }
        assert_row_contains(&state, 0, "red");
    }

    #[test]
    fn carriage_return_returns_cursor_to_column_zero() {
        let mut state = TerminalState::new(5, 20);
        // "abc\rXY" => column 0 overwritten with X, column 1 with Y,
        // column 2 still 'c'.
        state.feed(b"abc\rXY");
        assert_row_contains(&state, 0, "XYc");
    }

    // --- resize ----------------------------------------------------
    //
    // Window-drag resize must NOT clobber what's already on screen.
    // The previous Phase-1E-a code in `PaneSession::resize` rebuilt
    // the whole terminal — that was a temporary placeholder; the
    // in-place path here is the real one.

    #[test]
    fn resize_preserves_existing_content_when_growing() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"keep-me");
        state.resize(10, 40);
        // Existing text is still on row 0 inside the new larger grid.
        assert_row_contains(&state, 0, "keep-me");
        // And we now have 10 rows.
        assert_eq!(state.screen_text().lines().count(), 10);
    }

    #[test]
    fn resize_preserves_existing_content_when_shrinking() {
        let mut state = TerminalState::new(10, 40);
        state.feed(b"survives");
        state.resize(5, 20);
        // The first row still contains our text even though the grid
        // got smaller in both dimensions.
        assert_row_contains(&state, 0, "survives");
        assert_eq!(state.screen_text().lines().count(), 5);
    }

    #[test]
    fn alt_screen_flag_survives_resize() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"\x1b[?1049h");
        assert!(state.is_alternate_screen());
        state.resize(10, 40);
        // Resizing must not accidentally drop us out of alt screen —
        // vim et al would never recover if it did.
        assert!(state.is_alternate_screen());
    }

    // --- scrollback ------------------------------------------------
    //
    // The renderer paints whatever rows the grid says are visible.
    // After a `scroll_display` the visible window slides into the
    // scrollback. The mouse-wheel handler in `TermicaApp::update` is
    // the only production caller; tests verify the underlying state
    // moves.

    /// Feed enough lines to push earlier content into the scrollback
    /// buffer. `rows` here is the grid height; we emit `rows * 3`
    /// numbered lines so older lines are forced offscreen.
    fn feed_overflow(state: &mut TerminalState, rows: u16) {
        for i in 0..(rows as usize * 3) {
            let line = format!("row-{i}\r\n");
            state.feed(line.as_bytes());
        }
    }

    #[test]
    fn fresh_terminal_has_zero_display_offset() {
        let state = TerminalState::new(5, 20);
        assert_eq!(state.display_offset(), 0);
    }

    #[test]
    fn scroll_display_up_increases_display_offset() {
        let mut state = TerminalState::new(5, 20);
        feed_overflow(&mut state, 5);
        assert_eq!(state.display_offset(), 0, "fresh state should track live bottom");
        state.scroll_display(3);
        assert_eq!(state.display_offset(), 3);
    }

    #[test]
    fn scroll_display_down_returns_to_bottom() {
        let mut state = TerminalState::new(5, 20);
        feed_overflow(&mut state, 5);
        state.scroll_display(5);
        assert!(state.display_offset() > 0);
        // A large negative delta should clamp to 0 (live bottom).
        state.scroll_display(-100);
        assert_eq!(state.display_offset(), 0);
    }

    #[test]
    fn scroll_display_clamps_to_history_size() {
        let mut state = TerminalState::new(5, 20);
        feed_overflow(&mut state, 5);
        // Try to scroll way past the top — alacritty clamps to the
        // history size; we never panic and never overshoot.
        state.scroll_display(10_000);
        let offset = state.display_offset();
        assert!(offset > 0, "should have scrolled into history");
        assert!(offset <= 1_000, "offset should be bounded by history size: {offset}");
    }
}
