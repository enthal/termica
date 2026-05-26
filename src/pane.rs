//! Pane session: PTY + terminal + reader thread, glued together.
//!
//! Phase 1E-a: the data pipeline. A background thread drains the PTY
//! reader; the egui update loop pulls chunks from the channel and
//! feeds them into [`crate::terminal::TerminalState`]. The actual
//! cell renderer arrives in Phase 1E-b — this PR's UI surface is a
//! plain byte counter and a monospaced dump of `screen_text()`,
//! enough to prove the pipeline is alive.
//!
//! Splitting [`PaneSession`] (owns OS resources) from [`PaneView`]
//! (plain POD) keeps the UI testable: snapshot tests construct a
//! fixed [`PaneView`] without spawning a real process.

#![forbid(unsafe_code)]

use std::io::Read;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use alacritty_terminal::index::Point;

use crate::integration::{ManagedSpawn, ShellSpec, managed_spawn_for, new_session_id};
use crate::pty::{PtyConfig, PtyError, PtySession};
use crate::selection::{self, Selection, SelectionMode};
use crate::shell::{PaneMode, PromptController};
use crate::terminal::TerminalState;

/// Plain snapshot of what the UI needs to render this pane. No OS
/// handles, no channels, no threads — `Clone`/`Default` for trivial
/// test construction.
#[derive(Debug, Clone, Default)]
pub struct PaneView {
    /// Total bytes received from the PTY since session start.
    pub bytes_received: u64,
    /// Whether the terminal is currently in alternate-screen mode
    /// (vim / htop / less / fzf / tmux).
    pub alt_screen: bool,
    /// Cell-grid rows the PTY is currently sized to.
    pub rows: u16,
    /// Cell-grid columns the PTY is currently sized to.
    pub cols: u16,
    /// Most recent CWD reported via OSC 7 / `Precmd` / `Cwd`
    /// lifecycle events. `None` if the shell hasn't emitted one yet.
    pub cwd: Option<std::path::PathBuf>,
    /// Visible grid rendered as multi-line text. Convenient for
    /// `--dump-events`-style debugging; the real renderer paints
    /// cells directly via [`crate::render::paint_terminal`].
    pub screen_text: String,
    /// Current pane mode per the `PromptController` state machine.
    /// `None` only for `PaneView::default()` (snapshot tests); a
    /// real `PaneSession::view()` always populates it.
    pub mode: Option<PaneMode>,
    /// `true` while the pane is in `Bootstrapping` mode — the
    /// renderer should suppress the cell grid and show a placeholder
    /// instead.
    pub is_bootstrapping: bool,
}

/// Live session: one PTY + one [`TerminalState`] + the reader thread
/// that bridges them via an mpsc channel.
///
/// Drop closes the PTY (master + writer); the reader thread sees EOF
/// from the kernel and exits on its own. We don't currently `join`
/// the reader thread on drop — that would block the UI for the time
/// the kernel takes to deliver EOF, which we don't want on window
/// close. The thread will exit once its read returns 0.
pub struct PaneSession {
    pty: PtySession,
    terminal: TerminalState,
    bytes_rx: mpsc::Receiver<Vec<u8>>,
    bytes_received: u64,
    selection: Option<Selection>,
    exited: bool,
    _reader: JoinHandle<()>,
    /// Pane-mode state machine. Drives `Bootstrapping → RawTerminal`
    /// on `IntegrationReady`, `RawTerminal → ShellPromptEditor` on
    /// `Precmd`, etc. See [`crate::shell::PromptController`].
    controller: PromptController,
    /// Termica session ID, set in the spawned shell's environment as
    /// `TERMICA_SESSION_ID` and echoed back in every lifecycle
    /// message. Used today only for diagnostics; future work may
    /// compare it against the `session` field on incoming events to
    /// drop messages from stale children.
    #[allow(dead_code)]
    session_id: String,
    /// Frame counter used by the `PromptController` for its
    /// bootstrap timeout and frame-debounce logic. Incremented once
    /// per `drain()` call.
    frame: u64,
    /// Last-seen alt-screen flag — debounces the `observe_alt_screen`
    /// call on the controller so we only notify on transitions.
    last_alt_screen: bool,
    /// Owned per-spawn temp directory holding the wrapper file(s) the
    /// shell sourced at startup (zsh's `.zshrc`, bash's `--rcfile`).
    /// Kept here so its `Drop` fires when the pane closes — the
    /// wrapper exists exactly as long as the pane that needs it.
    /// `None` for fish (inline `--init-command`) and for `spawn()`
    /// (test / low-level path).
    #[allow(dead_code)]
    wrapper_dir: Option<tempfile::TempDir>,
}

impl PaneSession {
    /// Spawn a shell, attach a freshly sized [`TerminalState`], and
    /// start the background reader thread. Low-level constructor;
    /// most callers want [`Self::spawn_managed`] instead, which
    /// routes through the Phase 3 managed-startup machinery.
    ///
    /// `session_id` is exposed as the pane's `TERMICA_SESSION_ID`
    /// for lifecycle-event diagnostics. Synthesise one via
    /// [`crate::integration::new_session_id`] when a session ID
    /// isn't already in hand.
    pub fn spawn(
        rows: u16,
        cols: u16,
        config: &PtyConfig,
        session_id: String,
    ) -> Result<Self, PtyError> {
        let mut pty = PtySession::spawn(config)?;
        let terminal = TerminalState::new(rows, cols);

        let mut reader = pty.take_reader()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: PTY closed (child exited / master dropped)
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver gone — session dropped, exit cleanly
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            pty,
            terminal,
            bytes_rx: rx,
            bytes_received: 0,
            selection: None,
            exited: false,
            _reader: handle,
            // Low-level spawn: no managed bootstrap is in flight, so
            // start the controller past `Bootstrapping` directly in
            // `RawTerminal`. Callers using `spawn_managed` flip this
            // back to the Bootstrapping start after construction.
            controller: PromptController::new_no_bootstrap(0),
            session_id,
            frame: 0,
            last_alt_screen: false,
            wrapper_dir: None,
        })
    }

    /// Spawn a managed shell session per spec/03: build a
    /// [`ManagedSpawn`] for `shell`, translate it to a [`PtyConfig`]
    /// (with `cwd` from the caller if any), spawn via
    /// [`Self::spawn`], then — for zsh — write the bootstrap to the
    /// PTY's master end so the shell evaluates it before the first
    /// user-visible prompt. Bash and fish embed the bootstrap via
    /// their CLI flags and need no PTY-write.
    ///
    /// The pane enters `Bootstrapping` mode immediately; the
    /// renderer should suppress display until `is_bootstrapping()`
    /// reads `false` (i.e. the bootstrap has emitted
    /// `integration_ready` or `Bootstrapping` has timed out into
    /// `Degraded`).
    pub fn spawn_managed(
        rows: u16,
        cols: u16,
        shell: ShellSpec,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<Self, PtyError> {
        let session_id = new_session_id();
        let ManagedSpawn { argv, pty_bootstrap, env, wrapper_dir } =
            managed_spawn_for(shell, &session_id)
                .map_err(|e| PtyError::Os(format!("build managed spawn plan: {e}")))?;
        let program = argv[0].clone();
        let args: Vec<String> = argv[1..].to_vec();
        let config = PtyConfig { program, args, env, cwd, rows, cols };
        let mut session = Self::spawn(rows, cols, &config, session_id)?;
        // Tie the wrapper TempDir's lifetime to the pane session.
        // When the pane closes, the directory under $TMPDIR is
        // recursively removed.
        session.wrapper_dir = wrapper_dir;
        // Override the default no-bootstrap controller with one that
        // starts in `Bootstrapping`. The renderer will suppress the
        // pane until `integration_ready` arrives or the timeout fires.
        session.controller = PromptController::new(0);

        if let Some(bootstrap) = pty_bootstrap {
            // Write the bootstrap to the PTY as the first input the
            // shell sees. zsh -g --no_rcs is interactive but has no
            // init files loaded; it reads our bootstrap line-by-line,
            // executes it (sources user's .zshenv → .zshrc → installs
            // hooks → emits integration_ready), then waits for normal
            // user input. The `PromptController` observes
            // `IntegrationReady` and transitions out of Bootstrapping.
            session.pty.write(bootstrap.as_bytes())?;
        }

        Ok(session)
    }

    /// Pull every chunk the reader thread has queued and feed it into
    /// the terminal state. Returns the number of bytes consumed this
    /// call (0 if nothing was pending). Designed to be called once
    /// per UI frame.
    ///
    /// Also latches `self.exited` if the reader's `Sender` has been
    /// dropped (which happens when the reader thread exits — only
    /// after the PTY closes / the shell exits). Consumers route
    /// exited panes to close on the next frame via
    /// [`Self::is_exited`].
    pub fn drain(&mut self) -> usize {
        let mut consumed = 0usize;
        loop {
            match self.bytes_rx.try_recv() {
                Ok(chunk) => {
                    consumed += chunk.len();
                    self.bytes_received += chunk.len() as u64;
                    self.terminal.feed(&chunk);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Reader thread has exited — PTY is closed.
                    // Whatever bytes were already in the queue have
                    // been drained above (try_recv returns Empty
                    // before Disconnected when both apply); nothing
                    // more is coming.
                    self.exited = true;
                    break;
                }
            }
        }

        // Advance the per-pane frame counter. Used by the controller
        // for bootstrap timeout + frame-debounce. We tick once per
        // drain regardless of whether bytes arrived — the controller
        // measures elapsed time in frame ticks, not bytes seen.
        self.frame = self.frame.saturating_add(1);

        // Tick the bootstrap timeout. No-op once we leave
        // Bootstrapping.
        self.controller.tick_bootstrap_timeout(self.frame);

        // Feed lifecycle events extracted from the byte stream into
        // the controller. Order is preserved per spec/03.
        for event in self.terminal.drain_lifecycle_events() {
            self.controller.observe_event(event, self.frame);
        }

        // Track alt-screen transitions. The terminal flag is the
        // source of truth (alacritty maintains it); we only notify
        // the controller on edges.
        let alt = self.terminal.is_alternate_screen();
        if alt != self.last_alt_screen {
            self.controller.observe_alt_screen(alt, self.frame);
            self.last_alt_screen = alt;
        }

        // PTY exit is observed via `self.exited` (latched above)
        // rather than as a one-shot signal here — the parent app
        // already polls `is_exited` per frame to route pane close,
        // so we notify the controller on the same edge.
        if self.exited && self.controller.mode() != PaneMode::Dead {
            self.controller.observe_pty_exit(self.frame);
        }

        consumed
    }

    /// `true` once the reader thread has observed PTY EOF and
    /// exited — i.e. the shell process has terminated. Latched:
    /// once true, stays true. Drained once per frame in
    /// [`Self::drain`].
    pub fn is_exited(&self) -> bool {
        self.exited
    }

    /// Snapshot the parts of the session the UI cares about, without
    /// borrowing anything OS-owned. Cheap to call once per frame.
    pub fn view(&self) -> PaneView {
        use alacritty_terminal::grid::Dimensions;
        let grid = self.terminal.grid();
        let mode = self.controller.mode();
        PaneView {
            bytes_received: self.bytes_received,
            alt_screen: self.terminal.is_alternate_screen(),
            rows: grid.screen_lines() as u16,
            cols: grid.columns() as u16,
            cwd: self
                .controller
                .cwd()
                .map(|p| p.to_path_buf())
                .or_else(|| self.terminal.cwd().map(|p| p.to_path_buf())),
            screen_text: self.terminal.screen_text(),
            mode: Some(mode),
            is_bootstrapping: mode == PaneMode::Bootstrapping,
        }
    }

    /// Current pane mode per the `PromptController`. Reflects the
    /// state machine in spec/05.
    pub fn pane_mode(&self) -> PaneMode {
        self.controller.mode()
    }

    /// `true` while the renderer should suppress this pane's display
    /// (bootstrap noise) and the input layer should drop keystrokes.
    /// Becomes `false` as soon as `integration_ready` arrives or the
    /// bootstrap times out.
    pub fn is_bootstrapping(&self) -> bool {
        self.controller.is_bootstrapping()
    }

    /// Borrow the pane's `PromptController` (read-only).
    pub fn controller(&self) -> &PromptController {
        &self.controller
    }

    /// Write bytes to the PTY (e.g. keyboard input). Refuses to
    /// write while the pane is in `Bootstrapping` — keystrokes would
    /// race with the bootstrap script's own commands and corrupt
    /// integration. Bootstrap delivery itself goes through
    /// [`Self::spawn_managed`] which writes directly via the PTY
    /// before `Bootstrapping` is observable.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        if self.controller.is_bootstrapping() {
            return Ok(());
        }
        self.pty.write(bytes)
    }

    /// Resize the PTY and adjust the terminal's grid. Both must
    /// agree, so they live in a single call. The terminal state is
    /// resized in place (existing screen content is preserved); the
    /// kernel-side PTY size is updated so terminal-mode programs
    /// (vim, less, ...) see the new size on their next `read`.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.pty.resize(rows, cols)?;
        self.terminal.resize(rows, cols);
        Ok(())
    }

    /// Borrow the underlying terminal state read-only. The renderer
    /// uses this to walk the grid each frame.
    pub fn terminal(&self) -> &TerminalState {
        &self.terminal
    }

    /// Borrow the underlying terminal state mutably. Used by the
    /// update loop for VT-state operations that don't go through the
    /// PTY — currently only [`TerminalState::scroll_display`] (mouse
    /// wheel through scrollback).
    pub fn terminal_mut(&mut self) -> &mut TerminalState {
        &mut self.terminal
    }

    /// Current selection (anchor + head in absolute grid coordinates),
    /// or `None` when there is no selection.
    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    /// Begin a fresh selection at `p` with the given granularity mode.
    /// Replaces any existing selection. Called on mousedown from the
    /// pointer handler — single click → [`SelectionMode::Char`], double
    /// click → [`SelectionMode::Word`], triple click →
    /// [`SelectionMode::Line`].
    pub fn start_selection(&mut self, p: Point, mode: SelectionMode) {
        self.selection = Some(Selection::with_mode(p, mode));
    }

    /// Begin a fresh `Word`-mode selection whose anchor is glued to
    /// a URL's bounds. Used when a double-click lands inside a
    /// [`crate::links::LinkSpan`] — the user expects the whole URL
    /// to be selected, not the punctuation-bounded word their
    /// pointer is on. See [`Selection::with_url_anchor`].
    pub fn start_url_selection(&mut self, link_start: Point, link_end: Point) {
        self.selection = Some(Selection::with_url_anchor(link_start, link_end));
    }

    /// Move the head of the current selection. No-op if there is no
    /// active selection (the caller should have called
    /// [`Self::start_selection`] first on drag-start).
    pub fn extend_selection(&mut self, p: Point) {
        if let Some(sel) = &mut self.selection {
            sel.extend_to(p);
        }
    }

    /// Drop the current selection. Called on a click without drag and
    /// after a successful copy.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Materialise the text currently under the selection from the
    /// live grid. Returns `None` when there is no selection or when
    /// the selection is degenerate (single click, no drag).
    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        if sel.is_empty() {
            return None;
        }
        Some(selection::selection_text(self.terminal.grid(), sel))
    }
}

#[cfg(test)]
mod tests {
    //! Integration-style tests against a real shell. Each test owns
    //! its own session and never shares state.

    use super::*;
    use std::time::{Duration, Instant};

    fn sh_c(cmd: &str) -> PtyConfig {
        PtyConfig {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), cmd.into()],
            ..PtyConfig::default()
        }
    }

    /// Spin on `drain()` until either `predicate(&PaneView)` is true
    /// or the deadline expires. Returns the final view either way.
    fn wait_for<F: Fn(&PaneView) -> bool>(
        session: &mut PaneSession,
        deadline: Duration,
        predicate: F,
    ) -> PaneView {
        let stop_at = Instant::now() + deadline;
        loop {
            session.drain();
            let view = session.view();
            if predicate(&view) {
                return view;
            }
            if Instant::now() >= stop_at {
                return view;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn pipeline_routes_echo_output_into_the_terminal() {
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("printf hello-pipeline"), "test-session".into())
                .expect("spawn /bin/sh");
        let view = wait_for(&mut session, Duration::from_secs(2), |v| {
            v.screen_text.contains("hello-pipeline")
        });
        assert!(
            view.screen_text.contains("hello-pipeline"),
            "expected the terminal to contain the echoed text; got: {:?}\nbytes_received: {}",
            view.screen_text,
            view.bytes_received
        );
        assert!(
            view.bytes_received >= "hello-pipeline".len() as u64,
            "bytes_received seems too low: {}",
            view.bytes_received
        );
    }

    #[test]
    fn bytes_received_accumulates_across_frames() {
        let mut session = PaneSession::spawn(
            5,
            40,
            &sh_c("printf one; printf two; printf three"),
            "test-session".into(),
        )
        .expect("spawn");
        let view =
            wait_for(&mut session, Duration::from_secs(2), |v| v.screen_text.contains("three"));
        let expected_min = "onetwothree".len() as u64;
        assert!(
            view.bytes_received >= expected_min,
            "expected at least {} bytes, got {}\nscreen: {:?}",
            expected_min,
            view.bytes_received,
            view.screen_text
        );
    }

    #[test]
    fn write_sends_input_to_the_child() {
        // `cat` echoes its stdin back to its stdout, so writing a
        // line that we then see on the screen proves the write path.
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("cat"), "test-session".into()).expect("spawn cat");
        session.write(b"pipeline-write-marker\n").expect("write");
        let view = wait_for(&mut session, Duration::from_secs(2), |v| {
            v.screen_text.contains("pipeline-write-marker")
        });
        assert!(
            view.screen_text.contains("pipeline-write-marker"),
            "expected cat's echo to land; got: {:?}",
            view.screen_text
        );
    }

    #[test]
    fn view_alt_screen_starts_false() {
        let session =
            PaneSession::spawn(5, 40, &sh_c("sleep 0.1"), "test-session".into()).expect("spawn");
        let view = session.view();
        assert!(!view.alt_screen);
    }

    #[test]
    fn drain_on_idle_session_returns_zero() {
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("sleep 0.2"), "test-session".into()).expect("spawn");
        // sleep gives no output; first drain should see nothing.
        assert_eq!(session.drain(), 0);
        assert_eq!(session.view().bytes_received, 0);
    }

    #[test]
    fn resize_keeps_existing_output_on_screen() {
        // Spawn a shell that prints a marker and then idles, so we
        // can resize without races against the child exiting.
        let mut session = PaneSession::spawn(
            5,
            40,
            &sh_c("printf survives-resize; sleep 1"),
            "test-session".into(),
        )
        .expect("spawn");
        let _ = wait_for(&mut session, Duration::from_secs(2), |v| {
            v.screen_text.contains("survives-resize")
        });
        // Now resize up and down. The screen text must still contain
        // the marker afterwards — that's the whole point of the
        // in-place resize over the previous "rebuild the terminal"
        // placeholder.
        session.resize(20, 100).expect("resize up");
        assert!(
            session.view().screen_text.contains("survives-resize"),
            "marker lost after resize up; screen: {:?}",
            session.view().screen_text
        );
        session.resize(8, 30).expect("resize down");
        assert!(
            session.view().screen_text.contains("survives-resize"),
            "marker lost after resize down; screen: {:?}",
            session.view().screen_text
        );
        let _ = session.write(b"\x03"); // Ctrl-C the sleep so the test ends fast
    }

    // ---- is_exited ---------------------------------------------------

    #[test]
    fn is_exited_is_false_before_pty_exits() {
        // A still-running child means the reader thread is still
        // attached; nothing has disconnected, so the flag stays
        // false even after a drain or two.
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("sleep 5"), "test-session".into()).expect("spawn");
        assert!(!session.is_exited(), "fresh session must not be exited");
        for _ in 0..3 {
            session.drain();
            assert!(!session.is_exited(), "should still be running");
        }
        let _ = session.write(b"\x03"); // Ctrl-C so the sleep ends quickly
    }

    #[test]
    fn is_exited_flips_true_after_child_exits() {
        // A child that prints something and exits will cause the
        // PTY to close, the reader thread to break the loop and
        // drop its `Sender`, and the next `drain` to observe a
        // `Disconnected` from `try_recv`.
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("printf bye; exit 0"), "test-session".into())
                .expect("spawn");
        // Drain repeatedly until the flag latches or the deadline
        // expires. We don't use `wait_for` here because we're
        // asserting on `is_exited` rather than on a screen-text
        // predicate.
        let stop_at = Instant::now() + Duration::from_secs(2);
        while !session.is_exited() && Instant::now() < stop_at {
            session.drain();
            thread::sleep(Duration::from_millis(20));
        }
        assert!(session.is_exited(), "PTY should have been observed closed by now");
    }

    #[test]
    fn is_exited_stays_latched_across_drains() {
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("printf done; exit 0"), "test-session".into())
                .expect("spawn");
        let stop_at = Instant::now() + Duration::from_secs(2);
        while !session.is_exited() && Instant::now() < stop_at {
            session.drain();
            thread::sleep(Duration::from_millis(20));
        }
        assert!(session.is_exited());
        // Subsequent drains must not flip the flag back off.
        for _ in 0..5 {
            session.drain();
            assert!(session.is_exited());
        }
    }
}
