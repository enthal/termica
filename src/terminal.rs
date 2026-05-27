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

/// One cell of a frozen snapshot row. Carries the minimum state the
/// cell renderer needs to paint a sealed-block cell identically to a
/// live grid cell: glyph + colors + style flags. Used by the
/// [`crate::block`] model to freeze a finished command's output as
/// a [`Vec<StyledLine>`].
#[derive(Debug, Clone, PartialEq)]
pub struct StyledCell {
    pub c: char,
    pub fg: alacritty_terminal::vte::ansi::Color,
    pub bg: alacritty_terminal::vte::ansi::Color,
    pub flags: alacritty_terminal::term::cell::Flags,
}

/// One row of a frozen snapshot. Width matches the pane's cell
/// columns at the moment the snapshot was taken; sealed blocks do
/// not reflow on resize (see
/// [spec/04 §"Resize"](../spec/04-prompt-editor.md#resize-sealed-blocks-dont-reflow)).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StyledLine {
    pub cells: Vec<StyledCell>,
}

impl StyledLine {
    /// Iterate the line's char content, ignoring style. Test/debug
    /// helper for assertions like "does the snapshot contain `hello`?";
    /// the real renderer walks `cells` directly.
    pub fn text_chars(&self) -> impl Iterator<Item = char> + '_ {
        self.cells.iter().map(|c| c.c)
    }
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
/// the lower-level [`Self::grid`] / [`Self::cursor_position`] accessors
/// that the renderer uses.
pub struct TerminalState {
    term: Term<NopListener>,
    parser: Processor,
    /// Lightweight second pass over the same byte stream that watches
    /// for OSC sequences alacritty discards (notably OSC 7 — cwd).
    /// See [`crate::osc`].
    osc: crate::osc::OscSniffer,
    /// Echo suppression for the submit path (Phase 4C). Primed by
    /// [`Self::prime_echo_suppression`]; consulted at the head of
    /// [`Self::feed`] so the duplicate command bytes the kernel
    /// echoes after a submit never reach the grid.
    echo_suppress: crate::echo_suppress::EchoSuppressor,
}

impl TerminalState {
    /// Create a fresh terminal of the given dimensions in cells.
    pub fn new(rows: u16, cols: u16) -> Self {
        let size = TermSize::new(cols as usize, rows as usize);
        let config = Config::default();
        let term = Term::new(config, &size, NopListener);
        let parser = Processor::new();
        let osc = crate::osc::OscSniffer::new();
        let echo_suppress = crate::echo_suppress::EchoSuppressor::new();
        Self { term, parser, osc, echo_suppress }
    }

    /// Feed bytes into the VT parser. The grid mutates accordingly,
    /// and the OSC sniffer's state may update if an OSC 7 (or future
    /// Termica marker) is recognised.
    ///
    /// **Echo suppression** (Phase 4C): if the suppressor is armed
    /// (i.e. submit primed it with the bytes about to be echoed by
    /// the kernel), this method drops the matching prefix BEFORE
    /// the VT parser sees it. The suppression hook lives "above"
    /// the parser per spec/04 — suppressed bytes never reach the
    /// grid, never appear in OSC events, never count as input the
    /// shell emitted.
    ///
    /// Splitting a byte slice across multiple calls is safe — both
    /// parsers keep state across calls, so escape sequences split at
    /// any byte boundary still resolve correctly.
    pub fn feed(&mut self, bytes: &[u8]) {
        let surviving = if self.echo_suppress.is_armed() {
            self.echo_suppress.filter(bytes)
        } else {
            bytes.to_vec()
        };
        for byte in &surviving {
            self.parser.advance(&mut self.term, &[*byte]);
            self.osc.feed_byte(*byte);
        }
    }

    /// Prime the echo suppressor with the bytes about to be written
    /// to the PTY. Called by [`crate::pane::PaneSession::submit_editor_command`]
    /// immediately before the PTY write so the suppressor is armed
    /// before the kernel echo can land.
    pub fn prime_echo_suppression(&mut self, bytes: &[u8]) {
        self.echo_suppress.expect(bytes);
    }

    /// Read-only access to the suppressor — tests use this to
    /// assert "armed after submit" without reaching for private
    /// fields.
    pub fn echo_suppressor(&self) -> &crate::echo_suppress::EchoSuppressor {
        &self.echo_suppress
    }

    /// Most recent CWD reported via OSC 7, if any. Updates as the
    /// shell `cd`s (zsh emits OSC 7 by default; bash needs a hook).
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.osc.state().cwd.as_deref()
    }

    /// Drain all DCS-JSON lifecycle events that have been extracted
    /// from the byte stream since the last drain. Empty if no events
    /// are queued. Caller is the per-pane `PromptController`.
    pub fn drain_lifecycle_events(&mut self) -> Vec<crate::markers::LifecycleEvent> {
        self.osc.drain_events()
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

    /// Full reset of the visible state: blank the viewport, drop
    /// all scrollback, move the cursor to the top-left. The shell
    /// process itself is untouched — it will redraw the prompt on
    /// its next prompt cycle (PROMPT_COMMAND / precmd) or when the
    /// user presses Enter.
    ///
    /// This is the action behind the Cmd+K / Ctrl+Shift+K
    /// shortcut. Three alacritty Handler calls in order:
    /// `ClearMode::All` blanks the viewport (pushing old content
    /// into scrollback as it goes), `ClearMode::Saved` then
    /// purges that scrollback, and `goto` parks the cursor at
    /// (0, 0) so the next keystroke / redraw lands at the top.
    pub fn clear_all(&mut self) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler};
        Handler::clear_screen(&mut self.term, ClearMode::All);
        Handler::clear_screen(&mut self.term, ClearMode::Saved);
        Handler::goto(&mut self.term, 0, 0);
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

    /// Position in the cumulative-line space the
    /// [`crate::block::BlockStack`] uses to anchor block boundaries.
    ///
    /// Returns the cursor's row index measured from the **top of the
    /// scrollback** downward: `history_size + cursor_viewport_row`.
    /// As new lines are emitted, the value grows by 1 per line. As
    /// content scrolls into bounded scrollback the relationship to
    /// alacritty grid lines drifts (history_size saturates at the
    /// configured cap), so this is best-effort for very long
    /// command runs; 4A-render's seal-and-reset model retires the
    /// drift entirely.
    pub fn total_lines_seen(&self) -> usize {
        use alacritty_terminal::grid::Dimensions;
        let grid = self.term.grid();
        let scrollback = grid.history_size();
        let cursor_in_viewport = grid.cursor.point.line.0.max(0) as usize;
        scrollback + cursor_in_viewport
    }

    /// Capture a snapshot of the grid rows added since the line index
    /// `start` was recorded via [`Self::total_lines_seen`]. Returns
    /// rows `[start, current]` inclusive, each as a [`StyledLine`].
    ///
    /// Used by the block model to seal a finished command: at
    /// `Preexec`, record [`Self::total_lines_seen`]; at
    /// `CommandFinished`, call this with that recorded value to
    /// freeze the command's output as a [`Vec<StyledLine>`].
    ///
    /// Empty result when `start >= current` (no new content).
    pub fn snapshot_lines_since(&self, start: usize) -> Vec<StyledLine> {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line, Point};

        let grid = self.term.grid();
        let current = self.total_lines_seen();
        if start >= current {
            return Vec::new();
        }
        let history = grid.history_size() as i32;
        let cols = grid.columns();
        let screen_lines = grid.screen_lines() as i32;

        let mut out = Vec::with_capacity(current - start + 1);
        // Iterate the cumulative-line range and map back to alacritty
        // grid coordinates (`grid_line = cumulative - history_size`).
        // Negative grid lines are scrollback; non-negative are
        // viewport. Skip indices outside the grid's bounds — they can
        // only appear if the scrollback rotated past `start`.
        for line in (start as i32)..=(current as i32) {
            let grid_line = line - history;
            if grid_line < -history || grid_line >= screen_lines {
                continue;
            }
            let mut cells = Vec::with_capacity(cols);
            for col in 0..cols {
                let p = Point::new(Line(grid_line), Column(col));
                let cell = &grid[p];
                cells.push(StyledCell { c: cell.c, fg: cell.fg, bg: cell.bg, flags: cell.flags });
            }
            out.push(StyledLine { cells });
        }
        out
    }

    /// Capture every row the live `Term` has emitted since its last
    /// reset, top-to-bottom, as a [`Vec<StyledLine>`]. Used by the
    /// block model to freeze a finished command's output before
    /// resetting the `Term` for the next block.
    ///
    /// Rows iterated: the entire scrollback (`history_size` rows
    /// above the viewport) followed by the visible rows up to and
    /// including the cursor row. Trailing blank rows below the
    /// cursor are excluded — those are "the area the next prompt
    /// will draw into," not part of the just-finished command.
    pub fn snapshot_lines_all(&self) -> Vec<StyledLine> {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line, Point};

        let grid = self.term.grid();
        let cols = grid.columns();
        let history = grid.history_size() as i32;
        let cursor_row = grid.cursor.point.line.0;
        // Last row to include: the row the cursor is currently on.
        // Anything below it is uninitialised "next-prompt area" we
        // do not want in the sealed snapshot.
        let last_line = cursor_row.max(-history);

        let mut out = Vec::with_capacity((history + 1 + last_line.max(0)) as usize);
        for line in (-history)..=last_line {
            let mut cells = Vec::with_capacity(cols);
            for col in 0..cols {
                let p = Point::new(Line(line), Column(col));
                let cell = &grid[p];
                cells.push(StyledCell { c: cell.c, fg: cell.fg, bg: cell.bg, flags: cell.flags });
            }
            out.push(StyledLine { cells });
        }
        // Trim trailing fully-blank rows. After `command\r\n` the
        // shell's cursor sits on the row below, which is blank and
        // belongs to the next block — not this command's output.
        // Without this trim, every sealed block carries an extra
        // empty row at the bottom.
        while out.last().is_some_and(|l| l.cells.iter().all(|c| c.c == ' ')) {
            out.pop();
        }
        out
    }

    /// Drop everything in the grid and reset the cursor to (0, 0).
    /// Called by the block model when a [`crate::block::Block::Running`]
    /// seals: the snapshot is taken first via [`Self::snapshot_lines_all`],
    /// then the `Term` is reset so the next block starts with a clean
    /// slate.
    ///
    /// This is a wrapper over [`Self::clear_all`] for now; the name
    /// expresses block-model intent and lets the impl evolve later.
    pub fn reset_for_new_block(&mut self) {
        self.clear_all();
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

    // ---- block-model snapshot APIs --------------------------------

    #[test]
    fn total_lines_seen_starts_at_zero_for_fresh_terminal() {
        let state = TerminalState::new(5, 20);
        assert_eq!(state.total_lines_seen(), 0);
    }

    #[test]
    fn total_lines_seen_grows_with_emitted_newlines() {
        let mut state = TerminalState::new(5, 20);
        assert_eq!(state.total_lines_seen(), 0);
        state.feed(b"one\r\n");
        assert_eq!(state.total_lines_seen(), 1, "after one \\r\\n");
        state.feed(b"two\r\n");
        assert_eq!(state.total_lines_seen(), 2);
        state.feed(b"three\r\n");
        assert_eq!(state.total_lines_seen(), 3);
    }

    #[test]
    fn snapshot_lines_since_returns_empty_when_start_at_or_past_current() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"hello\r\n");
        let current = state.total_lines_seen();
        assert!(state.snapshot_lines_since(current).is_empty());
        assert!(state.snapshot_lines_since(current + 1).is_empty());
    }

    #[test]
    fn snapshot_lines_since_captures_rows_emitted_after_start() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"old-prompt$ \r\n"); // cursor moves to row 1
        let start = state.total_lines_seen();
        state.feed(b"hello\r\n"); // hello on row 1; cursor to row 2
        let lines = state.snapshot_lines_since(start);
        let joined: String =
            lines.iter().map(|l| l.text_chars().collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("hello"), "snapshot missing command output: {joined:?}");
        assert!(!joined.contains("old-prompt$"), "snapshot leaked pre-Preexec content: {joined:?}");
    }

    #[test]
    fn snapshot_lines_since_preserves_cell_count_at_pane_width() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"\r\n");
        let start = state.total_lines_seen();
        state.feed(b"xyz\r\n");
        let lines = state.snapshot_lines_since(start);
        assert!(!lines.is_empty(), "should have captured at least one row");
        for line in &lines {
            assert_eq!(line.cells.len(), 20, "row should be padded to pane width");
        }
    }

    // ---- whole-Term snapshot + reset (Phase 4A-render) ------------

    #[test]
    fn snapshot_lines_all_captures_visible_content_up_to_cursor() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"alpha\r\nbeta\r\n");
        let snap = state.snapshot_lines_all();
        // Cursor is now on row 2 after two \r\n. Should capture rows
        // 0, 1, 2 (alpha, beta, and the empty cursor row).
        let joined: String =
            snap.iter().map(|l| l.text_chars().collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("alpha"), "missing first row: {joined:?}");
        assert!(joined.contains("beta"), "missing second row: {joined:?}");
    }

    #[test]
    fn snapshot_lines_all_trims_trailing_blank_row_after_command_newline() {
        // After `printf "hi\r\n"` the cursor sits on row 1, col 0 —
        // a blank row that should NOT appear in the sealed snapshot.
        let mut state = TerminalState::new(5, 20);
        state.feed(b"hi\r\n");
        let snap = state.snapshot_lines_all();
        assert_eq!(snap.len(), 1, "trailing blank row should be trimmed: got {} rows", snap.len());
        let joined: String =
            snap.iter().map(|l| l.text_chars().collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("hi"));
    }

    #[test]
    fn snapshot_lines_all_excludes_uninitialised_rows_below_cursor() {
        let mut state = TerminalState::new(10, 20);
        state.feed(b"only-row\r\n");
        // Cursor now on row 1; rows 2..=9 are blank "next area".
        let snap = state.snapshot_lines_all();
        assert!(snap.len() <= 2, "should only capture rows up to cursor; got {} rows", snap.len());
        let joined: String =
            snap.iter().map(|l| l.text_chars().collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("only-row"));
    }

    #[test]
    fn reset_for_new_block_clears_grid_and_cursor() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"content-to-erase\r\nmore-content\r\n");
        state.reset_for_new_block();
        let text = state.screen_text();
        for (i, row) in text.lines().enumerate() {
            assert!(
                row.chars().all(|c| c == ' '),
                "row {i} should be blank after reset, got: {row:?}"
            );
        }
    }

    #[test]
    fn snapshot_then_reset_leaves_term_empty_for_next_block() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"command-output\r\n");
        let snap = state.snapshot_lines_all();
        state.reset_for_new_block();
        let joined: String =
            snap.iter().map(|l| l.text_chars().collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("command-output"), "snapshot lost: {joined:?}");
        // After reset, feeding new content starts from row 0 again.
        state.feed(b"next");
        assert_row_contains(&state, 0, "next");
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

    // ---- clear_all (Cmd+K full reset) -------------------------------

    #[test]
    fn clear_all_blanks_the_visible_screen() {
        let mut state = TerminalState::new(5, 20);
        state.feed(b"hello\r\nworld\r\nmore output");
        // Sanity: rows have content before clearing.
        assert!(state.screen_text().contains("hello"));
        state.clear_all();
        // After clear_all the visible grid is all spaces.
        let text = state.screen_text();
        for (i, line) in text.lines().enumerate() {
            assert!(
                line.chars().all(|c| c == ' '),
                "row {i} should be blank after clear_all but was {line:?}"
            );
        }
    }

    #[test]
    fn clear_all_empties_the_scrollback() {
        let mut state = TerminalState::new(5, 20);
        // Push lots of output so it spills into scrollback.
        feed_overflow(&mut state, 20);
        // Scroll up so we can see scrollback exists.
        state.scroll_display(10);
        assert!(state.display_offset() > 0);
        // Snap back to live bottom and clear.
        state.scroll_display(-10_000);
        state.clear_all();
        // No scrollback left to scroll into.
        state.scroll_display(10_000);
        assert_eq!(state.display_offset(), 0);
    }

    #[test]
    fn clear_all_moves_cursor_to_origin() {
        let mut state = TerminalState::new(5, 20);
        // Print enough that the cursor lands well below row 0.
        state.feed(b"line0\r\nline1\r\nline2\r\nline3");
        let (before_row, _before_col) = state.cursor_position().expect("cursor visible");
        assert!(before_row > 0, "precondition: cursor should be below row 0");
        state.clear_all();
        let (after_row, after_col) = state.cursor_position().expect("cursor still visible");
        assert_eq!((after_row, after_col), (0, 0));
    }
}
