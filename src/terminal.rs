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

    /// True when the terminal is in alternate-screen mode (vim/htop/
    /// less/fzf/tmux territory). The pane mode machine in Phase 3
    /// uses this as a hard signal that the program below owns every
    /// keystroke.
    pub fn is_alternate_screen(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::ALT_SCREEN)
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
}
