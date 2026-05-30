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
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use alacritty_terminal::index::Point;

use crate::block::{Block, BlockStack};
use crate::block_selection::BlockSelection;
use crate::events::EventRecorder;
use crate::history::{
    CaptureState, HistoryContext, RecallOutcome, RecallState, Scope, capture_on_event,
};
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
    /// Phase 4A: per-pane block stack. Starts with one `Prompt` block;
    /// transitions to `Running` on `Preexec` and seals + pushes a new
    /// `Prompt` on `CommandFinished`. The data model lands first
    /// (this PR); the renderer learns to walk it in 4A-render.
    blocks: BlockStack,
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
    /// Last-seen `last_transition.at` value, used to detect new
    /// transitions to dispatch to the [`EventRecorder`]. Updated
    /// after each emission.
    last_transition_at: u64,
    /// Owned per-spawn temp directory holding the wrapper file(s) the
    /// shell sourced at startup (zsh's `.zshrc`, bash's `--rcfile`).
    /// Kept here so its `Drop` fires when the pane closes — the
    /// wrapper exists exactly as long as the pane that needs it.
    /// `None` for fish (inline `--init-command`) and for `spawn()`
    /// (test / low-level path).
    #[allow(dead_code)]
    wrapper_dir: Option<tempfile::TempDir>,
    /// Stable per-pane id (the `PaneId.0` field), recorded once at
    /// spawn so the [`EventRecorder`] can tag emitted lines without
    /// the pane depending on the `pane_slot` module.
    pane_id: u64,
    /// Shared diagnostic event sink. `Some` when
    /// `TERMICA_DUMP_EVENTS=<path>` was set at startup. All panes
    /// in the same Termica process write to the same file.
    recorder: Option<Arc<EventRecorder>>,
    /// Text of the most recently submitted editor command, stashed
    /// at submit time and consumed at most once when the shell emits
    /// a [`LifecycleEvent::Continuation`] (its parser saw an
    /// incomplete command and wants more input). When that happens
    /// we re-promote the pane to `ShellPromptEditor` and restore
    /// this text into the editor with a trailing `\n` so the user
    /// can keep typing the rest of the multi-line command. Cleared
    /// on the next normal `Preexec` (command completed without a
    /// continuation) — see [`Self::drain`].
    last_submitted: Option<String>,
    /// Phase 4F: selection over one of the sealed blocks in
    /// [`Self::blocks`]. Independent of [`Self::selection`] (which
    /// covers the live alacritty grid). At most one of the two is
    /// populated at any moment — the setters clear the other so the
    /// user sees a single "current selection" across the pane.
    block_selection: Option<BlockSelection>,
    /// Phase 4J: handle on the per-process `runs` store + the
    /// `app_run_id` UUID. `None` for panes spawned without history
    /// (the low-level `spawn` path, used in tests). The capture
    /// path no-ops on `None`.
    history: Option<HistoryContext>,
    /// Per-pane mutable state for history capture: the row id of
    /// the currently-running command so the matching
    /// `CommandFinished` can stamp the exit code. Reset on every
    /// `Preexec` and every `CommandFinished`.
    capture_state: CaptureState,
    /// `↑`/`↓` history-recall state. Persists across the in-flight
    /// walk (saved buffer + cached entries + cursor) and is reset
    /// on the first non-recall edit. See [`crate::history::recall`].
    recall: RecallState,
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
        pane_id: u64,
        recorder: Option<Arc<EventRecorder>>,
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
            blocks: BlockStack::new(0),
            session_id,
            frame: 0,
            last_alt_screen: false,
            last_transition_at: 0,
            wrapper_dir: None,
            pane_id,
            recorder,
            last_submitted: None,
            block_selection: None,
            history: None,
            capture_state: CaptureState::default(),
            recall: RecallState::default(),
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
        pane_id: u64,
        recorder: Option<Arc<EventRecorder>>,
        history: Option<HistoryContext>,
    ) -> Result<Self, PtyError> {
        let session_id = new_session_id();
        let ManagedSpawn { argv, pty_bootstrap, env, wrapper_dir } =
            managed_spawn_for(shell, &session_id)
                .map_err(|e| PtyError::Os(format!("build managed spawn plan: {e}")))?;
        if let Some(rec) = recorder.as_ref() {
            rec.record_spawn(pane_id, shell, &argv);
        }
        let program = argv[0].clone();
        let args: Vec<String> = argv[1..].to_vec();
        let config = PtyConfig { program, args, env, cwd, rows, cols };
        let mut session = Self::spawn(rows, cols, &config, session_id, pane_id, recorder)?;
        session.history = history;
        // Tie the wrapper TempDir's lifetime to the pane session.
        // When the pane closes, the directory under $TMPDIR is
        // recursively removed.
        session.wrapper_dir = wrapper_dir;
        // Override the default no-bootstrap controller with one that
        // starts in `Bootstrapping`. The renderer will suppress the
        // pane until `integration_ready` arrives or the timeout fires.
        session.controller = PromptController::new(0);
        // Record the initial Bootstrapping `InitialSpawn` transition
        // so the dump file shows the pane state from t=0.
        if let Some(rec) = session.recorder.as_ref() {
            rec.record_transition(session.pane_id, session.controller.last_transition());
        }
        session.last_transition_at = session.controller.last_transition().at;

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
        self.record_pending_transitions();

        // Feed lifecycle events extracted from the byte stream into
        // the controller AND the block stack. Order is preserved
        // per spec/03. The block stack reads the terminal snapshot
        // at seal time, so it must run AFTER the bytes that produced
        // this event have been fed into the grid (they were, above).
        for event in self.terminal.drain_lifecycle_events() {
            if let Some(rec) = self.recorder.as_ref() {
                rec.record_lifecycle(self.pane_id, &event);
            }
            // Multi-line continuation: if the shell's parser saw an
            // incomplete command after submit and emitted `PS2` (as
            // our DCS marker), restore the previously-submitted text
            // into the editor with a trailing `\n` so the user can
            // keep typing the next line. The controller will flip
            // mode back to `ShellPromptEditor` for us in
            // `observe_event` below. Order matters: restore first,
            // then transition — the editor must be populated before
            // any frame sees `editor_is_active()` go true.
            //
            // We DON'T clear `last_submitted` here, because a single
            // multi-line command may yield several `Continuation`
            // events as the user keeps adding more lines that are
            // also incomplete. Cleared on `Preexec` (full command
            // about to run) instead.
            if matches!(event, crate::markers::LifecycleEvent::Continuation)
                && let Some(text) = self.last_submitted.as_ref()
                && let Some(editor) = self.blocks.editor_on_tail_mut()
            {
                editor.clear();
                editor.insert_str(text);
                editor.insert_newline();
            }
            // `Preexec` = the shell actually started executing the
            // command, so any pending continuation state is moot.
            if matches!(event, crate::markers::LifecycleEvent::Preexec { .. }) {
                self.last_submitted = None;
            }
            // Zombie alt-screen recovery. `less`, `vim`, etc. enter
            // the alternate screen via `\e[?1049h` and SHOULD restore
            // via `\e[?1049l` on exit. In practice (less `q` on
            // macOS, vim crash, ssh-disconnect-mid-program) the exit
            // sequence can be skipped — alacritty's alt-screen flag
            // stays true, the editor footer paints OVER a stale alt-
            // grid, and an Esc / focus change unmasks the zombie.
            // A fresh `Precmd` from the shell means: shell has
            // resumed control, there is no longer a foreground
            // program owning the screen, so any latent alt-screen
            // must be cleared. Feed the exit sequence into the
            // alacritty parser so it processes the transition the
            // normal way (clears flag, restores grid). No-op when
            // alt-screen is already off.
            if matches!(event, crate::markers::LifecycleEvent::Precmd { .. })
                && self.terminal.is_alternate_screen()
            {
                self.terminal.feed(b"\x1b[?1049l");
            }
            // Phase 4J: persist Preexec → record_submit and
            // CommandFinished → record_finish. No-op if history is
            // disabled (the low-level `spawn` path or a missing
            // `<data-dir>/history.sqlite`). The cwd comes from
            // OSC 7 / DCS-JSON Precmd, whichever fired most recently.
            let now_ms = wall_clock_ms();
            let cwd_str = self.terminal.cwd().map(|p| p.display().to_string());
            capture_on_event(
                self.history.as_ref(),
                &mut self.capture_state,
                &event,
                self.pane_id,
                cwd_str.as_deref(),
                now_ms,
            );
            self.blocks.observe_lifecycle_event(&event, &mut self.terminal, self.frame);
            self.controller.observe_event(event, self.frame);
            self.record_pending_transitions();
        }

        // Track alt-screen transitions. The terminal flag is the
        // source of truth (alacritty maintains it); we only notify
        // the controller on edges.
        let alt = self.terminal.is_alternate_screen();
        if alt != self.last_alt_screen {
            self.controller.observe_alt_screen(alt, self.frame);
            self.last_alt_screen = alt;
            self.record_pending_transitions();
        }

        // PTY exit is observed via `self.exited` (latched above)
        // rather than as a one-shot signal here — the parent app
        // already polls `is_exited` per frame to route pane close,
        // so we notify the controller on the same edge.
        if self.exited && self.controller.mode() != PaneMode::Dead {
            if let Some(rec) = self.recorder.as_ref() {
                rec.record_pty_exit(self.pane_id);
            }
            self.controller.observe_pty_exit(self.frame);
            self.record_pending_transitions();
        }

        consumed
    }

    /// If the controller has recorded a new transition since we last
    /// checked, emit it to the [`EventRecorder`]. Centralises the
    /// "diff the last transition and dispatch" check so all the call
    /// sites in [`Self::drain`] stay readable.
    fn record_pending_transitions(&mut self) {
        let latest = self.controller.last_transition().at;
        if latest != self.last_transition_at
            && let Some(rec) = self.recorder.as_ref()
        {
            rec.record_transition(self.pane_id, self.controller.last_transition());
        }
        self.last_transition_at = latest;
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

    /// Borrow the pane's [`BlockStack`] (read-only). Renderer (4A-render)
    /// walks this to paint sealed blocks above the live tail; tests use it
    /// to assert the block-lifecycle wiring (Phase 4A-data).
    pub fn blocks(&self) -> &BlockStack {
        &self.blocks
    }

    /// Cmd+K / Ctrl+Shift+K: drop the sealed block scrollback AND
    /// blank the live terminal grid. The shell process is
    /// untouched — it'll redraw its prompt on the next prompt
    /// cycle, or when the user presses Enter.
    pub fn clear_scrollback(&mut self) {
        self.blocks.clear_sealed();
        self.terminal.clear_all();
    }

    /// Stable per-pane id. Used to salt egui widget IDs that may
    /// otherwise collide when multiple panes are visible at once
    /// (e.g. each pane's `ScrollArea` needs its own state — see the
    /// "Duplicate widget IDs are a critical bug" callout in
    /// CLAUDE.md).
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// Mutable handle to the editor on the live `Prompt` block, if
    /// the tail is a `Prompt`. Returns `None` when a `Running`
    /// command is executing. Phase 4B's keystroke router uses this
    /// to apply editor edits without leaking the `frame` counter
    /// out of `PaneSession`.
    pub fn editor_mut(&mut self) -> Option<&mut crate::prompt_editor::PromptEditor> {
        self.blocks.editor_on_tail_mut()
    }

    /// The pane is in `ShellPromptEditor` mode AND the tail block
    /// is a `Prompt` with an active editor. The single switch the
    /// keystroke router needs to decide whether to route input to
    /// the editor vs the PTY.
    pub fn editor_is_active(&self) -> bool {
        self.controller.mode() == PaneMode::ShellPromptEditor
            && self.blocks.editor_on_tail().is_some()
    }

    /// `↑`: substitute the previous (older) pane-scope history
    /// entry into the editor. First call saves the current buffer
    /// so `↓` can restore it. Returns `true` if the editor changed
    /// (so the caller's auto-scroll/auto-redraw machinery can
    /// react). No-op if no editor, no history, or no entries.
    pub fn editor_history_prev(&mut self) -> bool {
        let Some(ctx) = self.history.clone() else { return false };
        if self.blocks.editor_on_tail().is_none() {
            return false;
        }
        let pane_id = self.pane_id;
        let current_text =
            self.blocks.editor_on_tail().map(|e| e.text().to_string()).unwrap_or_default();
        let app_run_id = ctx.app_run_id.clone();
        let outcome = self.recall.step_back(
            || {
                let store = match ctx.store.lock() {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                store
                    .recent(&Scope::Pane { pane_id: pane_id as i64, app_run_id: &app_run_id }, 500)
                    .map(|rows| rows.into_iter().map(|r| r.text).collect())
                    .unwrap_or_default()
            },
            &current_text,
        );
        apply_recall_outcome(self.blocks.editor_on_tail_mut(), outcome)
    }

    /// `↓`: walk toward newer entries; returns to the saved buffer
    /// at the head. Returns `true` if the editor changed.
    pub fn editor_history_next(&mut self) -> bool {
        if self.blocks.editor_on_tail().is_none() {
            return false;
        }
        let outcome = self.recall.step_forward();
        apply_recall_outcome(self.blocks.editor_on_tail_mut(), outcome)
    }

    /// Abandon any in-progress `↑`/`↓` walk so the next `↑`
    /// re-queries and re-saves the editor buffer. Idempotent.
    /// Call from edit paths (text insert, backspace, …) so a
    /// keystroke other than an arrow exits recall.
    pub fn clear_history_recall(&mut self) {
        self.recall.abandon();
    }

    /// Borrow the per-process history context for surfaces that
    /// need to open queries directly (the `^R` overlay). Returns
    /// `None` if persistence was disabled or unreachable at
    /// startup.
    pub fn history_ctx(&self) -> Option<&HistoryContext> {
        self.history.as_ref()
    }

    /// Insert `text` into the editor as the buffer (replacing any
    /// existing content) and place the caret at the end. Used by
    /// the `^R` overlay after the user picks an entry.
    pub fn replace_editor_buffer(&mut self, text: &str) {
        if let Some(editor) = self.blocks.editor_on_tail_mut() {
            editor.clear();
            editor.insert_str(text);
        }
    }

    /// Submit the editor's current text to the PTY (spec/04
    /// §"Submission semantics"). The order is load-bearing:
    ///
    /// 1. Take the editor text + clear the editor.
    /// 2. **Eagerly demote** the controller to `RawTerminal` —
    ///    from this instant onward, keystrokes go to the PTY (so a
    ///    user's immediate Ctrl-C reaches the running command, not
    ///    the closed editor).
    /// 3. **Prime echo suppression** with the bytes about to be
    ///    written, so the kernel's echo of the same bytes never
    ///    reaches the grid.
    /// 4. Write `<text>\r` to the PTY. CR (not LF) matches the
    ///    [`crate::input::encode_key`] convention for `Enter`; the
    ///    kernel's tty discipline translates CR→NL on the input
    ///    side and echoes back with CRLF on the output side.
    ///
    /// Returns `Ok(())` on any path that completed, including the
    /// "no editor" / "editor empty" no-ops. A `Err(PtyError)` is
    /// only returned when the PTY write itself fails.
    pub fn submit_editor_command(&mut self) -> Result<(), PtyError> {
        // 1. Take the editor text. If there's no editor (the tail
        //    isn't a `Prompt`), submit is a no-op. If the editor is
        //    empty, we still send a bare `\r` so the shell sees a
        //    blank line and emits the next prompt — that's what
        //    pressing Enter on an empty shell prompt does in every
        //    terminal.
        let text = match self.blocks.editor_on_tail_mut() {
            Some(editor) => {
                let t = editor.text().to_string();
                editor.clear();
                t
            }
            None => return Ok(()),
        };

        // Reset history-recall state: the next `↑` should start a
        // fresh walk from the most-recent entry, not from wherever
        // the previous walk left the cursor. Without this, after
        // submitting a recalled command, `↑` would resume from the
        // old cursor position and skip the just-submitted entry.
        self.recall.abandon();

        // 2. Eager demote BEFORE the PTY write.
        self.controller.submit_command(self.frame);
        // Record the transition into the dump-events file so the
        // submit gesture shows up in diagnostics.
        if let Some(rec) = self.recorder.as_ref() {
            let latest = self.controller.last_transition().at;
            if latest != self.last_transition_at {
                rec.record_transition(self.pane_id, self.controller.last_transition());
                self.last_transition_at = latest;
            }
        }

        // 3 & 4. Build the byte sequence, prime suppression, write.
        // Two cases:
        //
        // - First submit (no prior `last_submitted`): write the full
        //   editor text + `\r`. Standard.
        //
        // - Submit after a `Continuation`: `last_submitted` holds the
        //   text the shell already received (via earlier submit) and
        //   the editor was re-populated with that text + `\n` so the
        //   user could keep editing. Now the editor's text is
        //   `<prev_sent>\n<new_lines>`. The shell ALREADY has
        //   `<prev_sent>\n` buffered (it received that on the prior
        //   submit and is waiting for more); we only need to send
        //   `<new_lines>\r` — sending the whole thing would feed
        //   `<prev_sent>` to the shell TWICE.
        let to_send: String = match self.last_submitted.as_deref() {
            Some(prev) if text.starts_with(prev) => {
                // Strip the already-sent prefix + the separator \n
                // the restore-on-continuation logic inserted.
                let after = &text[prev.len()..];
                after.strip_prefix('\n').unwrap_or(after).to_string()
            }
            _ => text.clone(),
        };
        // Track the full editor text (not `to_send`) so the next
        // Continuation restore can put the right cumulative text
        // back into the editor.
        self.last_submitted = Some(text);
        let mut bytes = to_send.into_bytes();
        bytes.push(b'\r');
        self.terminal.prime_echo_suppression(&bytes);
        self.pty.write(&bytes)?;
        Ok(())
    }

    /// Demote the pane out of `ShellPromptEditor` back to
    /// `RawTerminal` (the canonical "Esc on the editor" gesture per
    /// spec/05). Preserves the editor buffer so a stray Esc doesn't
    /// nuke what the user just typed — they can promote back into
    /// the editor (the next `Precmd`) and pick up where they left
    /// off. No-op when not in `ShellPromptEditor`.
    pub fn leave_editor_esc(&mut self) {
        self.controller.leave_editor_esc(self.frame);
        // Record the transition into the dump-events file so the
        // user-initiated demote shows up in diagnostics.
        if let Some(rec) = self.recorder.as_ref() {
            let latest = self.controller.last_transition().at;
            if latest != self.last_transition_at {
                rec.record_transition(self.pane_id, self.controller.last_transition());
                self.last_transition_at = latest;
            }
        }
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
        self.block_selection = None;
    }

    /// Begin a fresh `Word`-mode selection whose anchor is glued to
    /// a URL's bounds. Used when a double-click lands inside a
    /// [`crate::links::LinkSpan`] — the user expects the whole URL
    /// to be selected, not the punctuation-bounded word their
    /// pointer is on. See [`Selection::with_url_anchor`].
    pub fn start_url_selection(&mut self, link_start: Point, link_end: Point) {
        self.selection = Some(Selection::with_url_anchor(link_start, link_end));
        self.block_selection = None;
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

    /// Current sealed-block selection (Phase 4F), or `None` when the
    /// user hasn't started one — or when a more recent live-grid
    /// selection cleared it.
    pub fn block_selection(&self) -> Option<&BlockSelection> {
        self.block_selection.as_ref()
    }

    /// Install a sealed-block selection, replacing any previous one
    /// and clearing the live-grid selection so the pane only ever
    /// shows a single "current selection."
    pub fn set_block_selection(&mut self, sel: BlockSelection) {
        self.selection = None;
        self.block_selection = Some(sel);
    }

    /// Drop the sealed-block selection without touching live-grid
    /// state. Called when a click lands outside any sealed block, or
    /// before starting a new live-grid drag.
    pub fn clear_block_selection(&mut self) {
        self.block_selection = None;
    }

    /// Replace the endpoints of the current sealed-block selection
    /// without changing `block_id`. No-op if no sealed-block
    /// selection is active. Used by the drag handler so the
    /// originating `block_id` stays pinned regardless of where the
    /// pointer roams.
    pub fn update_block_selection_endpoints(
        &mut self,
        anchor: crate::block_selection::BlockCursor,
        head: crate::block_selection::BlockCursor,
    ) {
        if let Some(sel) = self.block_selection.as_mut() {
            sel.anchor = anchor;
            sel.head = head;
        }
    }

    /// Materialise the text currently under the sealed-block
    /// selection. Walks [`Self::blocks`] for the matching `block_id`
    /// and slices its `snapshot`. Returns `None` when there is no
    /// active block selection, the matching block has gone away, or
    /// the selection is empty / falls outside the snapshot. Each row
    /// is trimmed of trailing whitespace (the grid pads with spaces).
    pub fn block_selection_text(&self) -> Option<String> {
        let sel = self.block_selection.as_ref()?;
        if sel.is_empty() {
            return None;
        }
        for block in self.blocks.iter() {
            if let Block::Sealed { id, command, snapshot, .. } = block
                && *id == sel.block_id
            {
                let text = crate::block_selection::block_selection_text(command, snapshot, sel);
                return if text.is_empty() { None } else { Some(text) };
            }
        }
        None
    }

    /// Scan a sealed block (command + snapshot) for URLs and
    /// existing file-path tokens. Returns `(row, col_start,
    /// col_end_inclusive, url)` spans in the block's unified row
    /// space — same indexing as the selection / cursor helpers.
    /// `None` if no sealed block has that id.
    ///
    /// Walks the block on every call; sealed blocks don't mutate
    /// after seal, so caching is a viable future optimisation, but
    /// at typical visible-block counts the scan is cheap and the
    /// data path stays simple.
    pub fn sealed_block_links(
        &self,
        block_id: crate::block::BlockId,
        home: Option<&std::path::Path>,
    ) -> Option<Vec<crate::block_links::BlockLinkSpan>> {
        for block in self.blocks.iter() {
            if let Block::Sealed { id, command, snapshot, header, .. } = block
                && *id == block_id
            {
                return Some(crate::block_links::scan_block_links(
                    command,
                    snapshot,
                    header.cwd.as_deref(),
                    home,
                ));
            }
        }
        None
    }

    /// Number of rows in a sealed block: `(command_lines, snapshot_lines)`.
    /// The unified row space is `0..command_lines` for the command
    /// label and `command_lines..(command_lines + snapshot_lines)`
    /// for the output snapshot. `None` if no sealed block has that
    /// id.
    pub fn sealed_block_rows(&self, block_id: crate::block::BlockId) -> Option<(usize, usize)> {
        for block in self.blocks.iter() {
            if let Block::Sealed { id, command, snapshot, .. } = block
                && *id == block_id
            {
                let cmd_lines = if command.is_empty() { 0 } else { command.split('\n').count() };
                return Some((cmd_lines, snapshot.len()));
            }
        }
        None
    }

    /// Length of `row` in the sealed block's unified row space. The
    /// row is checked against the command label first, then the
    /// snapshot. For snapshot rows the length stops at the last
    /// typed cell (trailing-space padding is excluded) so drag-
    /// clamping does not let the selection extend into the grid's
    /// imaginary right-margin spaces. `None` if no sealed block has
    /// that id or the row is past the end.
    pub fn sealed_row_len(&self, block_id: crate::block::BlockId, row: usize) -> Option<usize> {
        for block in self.blocks.iter() {
            if let Block::Sealed { id, command, snapshot, .. } = block
                && *id == block_id
            {
                let cmd_lines: Vec<&str> =
                    if command.is_empty() { Vec::new() } else { command.split('\n').collect() };
                if row < cmd_lines.len() {
                    return Some(cmd_lines[row].chars().count());
                }
                let snap_row = row - cmd_lines.len();
                return snapshot
                    .get(snap_row)
                    .map(|l| crate::block_selection::effective_row_len(&l.cells));
            }
        }
        None
    }

    /// Word range around `cursor` inside `block_id`'s unified row
    /// space. Word ranges never cross the command/snapshot boundary
    /// — words in the command label resolve against the command
    /// text; words in the snapshot resolve against the snapshot
    /// cells. Both endpoints share `cursor.row`. Returns `None`
    /// when the block doesn't exist or the row is past the end.
    pub fn sealed_word_range(
        &self,
        block_id: crate::block::BlockId,
        cursor: crate::block_selection::BlockCursor,
    ) -> Option<(crate::block_selection::BlockCursor, crate::block_selection::BlockCursor)> {
        use crate::block_selection::{BlockCursor, cell_word_range};
        use crate::prompt_editor::is_word_char;
        for block in self.blocks.iter() {
            if let Block::Sealed { id, command, snapshot, .. } = block
                && *id == block_id
            {
                let cmd_lines: Vec<&str> =
                    if command.is_empty() { Vec::new() } else { command.split('\n').collect() };
                if cursor.row < cmd_lines.len() {
                    let line = cmd_lines[cursor.row];
                    let chars: Vec<char> = line.chars().collect();
                    let (a, b) = word_range_in_chars(&chars, cursor.col, is_word_char);
                    return Some((
                        BlockCursor::new(cursor.row, a),
                        BlockCursor::new(cursor.row, b),
                    ));
                }
                let snap_row = cursor.row - cmd_lines.len();
                let line = snapshot.get(snap_row)?;
                let (a, b) = cell_word_range(&line.cells, cursor.col);
                return Some((BlockCursor::new(cursor.row, a), BlockCursor::new(cursor.row, b)));
            }
        }
        None
    }

    /// Line range for the row under `cursor` inside `block_id`'s
    /// unified row space. Full row width — trailing whitespace
    /// included; copy trims. Both endpoints share `cursor.row`.
    pub fn sealed_line_range(
        &self,
        block_id: crate::block::BlockId,
        cursor: crate::block_selection::BlockCursor,
    ) -> Option<(crate::block_selection::BlockCursor, crate::block_selection::BlockCursor)> {
        use crate::block_selection::{BlockCursor, cell_line_range};
        for block in self.blocks.iter() {
            if let Block::Sealed { id, command, snapshot, .. } = block
                && *id == block_id
            {
                let cmd_lines: Vec<&str> =
                    if command.is_empty() { Vec::new() } else { command.split('\n').collect() };
                if cursor.row < cmd_lines.len() {
                    let len = cmd_lines[cursor.row].chars().count();
                    return Some((
                        BlockCursor::new(cursor.row, 0),
                        BlockCursor::new(cursor.row, len),
                    ));
                }
                let snap_row = cursor.row - cmd_lines.len();
                let line = snapshot.get(snap_row)?;
                let (a, b) = cell_line_range(&line.cells);
                return Some((BlockCursor::new(cursor.row, a), BlockCursor::new(cursor.row, b)));
            }
        }
        None
    }
}

/// Replace the editor's contents with `outcome`'s `new_text` and
/// move the caret to the end. No-op (and `false` return) on
/// `Unchanged`. Shared between the `↑` / `↓` recall paths in
/// `PaneSession::editor_history_*`.
fn apply_recall_outcome(
    editor: Option<&mut crate::prompt_editor::PromptEditor>,
    outcome: RecallOutcome,
) -> bool {
    let Some(editor) = editor else { return false };
    match outcome {
        RecallOutcome::Replace { new_text } => {
            editor.clear();
            editor.insert_str(&new_text);
            true
        }
        RecallOutcome::Unchanged => false,
    }
}

/// Wall-clock unix-epoch milliseconds. Used by the history
/// capture path. Determinism rule from [spec/09](../spec/09-testing.md):
/// tests inject the value into `capture_on_event` directly, so
/// this only runs in production drain() calls.
fn wall_clock_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Word range for `col` (a char index) in `chars`, using the
/// shared `is_word_char` predicate. Used by command-label
/// selection where we have a `&str` rather than `&[StyledCell]`.
/// Returns `(col, col)` for non-word positions or past the end.
fn word_range_in_chars(
    chars: &[char],
    col: usize,
    is_word_char: fn(char) -> bool,
) -> (usize, usize) {
    if col >= chars.len() {
        return (col, col);
    }
    if !is_word_char(chars[col]) {
        return (col, col);
    }
    let mut start = col;
    while start > 0 && is_word_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_word_char(chars[end]) {
        end += 1;
    }
    (start, end)
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
        let mut session = PaneSession::spawn(
            5,
            40,
            &sh_c("printf hello-pipeline"),
            "test-session".into(),
            0,
            None,
        )
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
            0,
            None,
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
        let mut session = PaneSession::spawn(5, 40, &sh_c("cat"), "test-session".into(), 0, None)
            .expect("spawn cat");
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
        let session = PaneSession::spawn(5, 40, &sh_c("sleep 0.1"), "test-session".into(), 0, None)
            .expect("spawn");
        let view = session.view();
        assert!(!view.alt_screen);
    }

    // ---- block stack wiring ------------------------------------------
    //
    // Phase 4A-data: spawning a pane must build a `BlockStack` and
    // every drained lifecycle event must reach it. Unit-level state
    // machine coverage lives in `src/block.rs`; these tests prove
    // the wiring at the `PaneSession::drain` boundary.

    #[test]
    fn fresh_pane_has_one_prompt_block() {
        let session = PaneSession::spawn(5, 40, &sh_c("sleep 0.1"), "test-session".into(), 0, None)
            .expect("spawn");
        assert_eq!(session.blocks().len(), 1);
        assert!(
            matches!(session.blocks().last(), Some(crate::block::Block::Prompt { .. })),
            "fresh pane's tail block should be Prompt, got {:?}",
            session.blocks().last()
        );
    }

    #[test]
    fn lifecycle_preexec_seen_in_drain_promotes_tail_to_running() {
        // Emit a Termica-tagged DCS sequence with a `preexec` payload
        // into the pane's PTY. The terminal-state parser consumes it
        // as a lifecycle event; `drain()` then routes the event to
        // the block stack, which should turn the tail Prompt into
        // a Running block with the captured command.
        //
        // The DCS framing is `ESC P Termica;<json> ESC \`. The
        // `session` field is required by the schema but the parser
        // currently doesn't validate it, so any string works.
        let cmd = "printf '\\033PTermica;{\"type\":\"preexec\",\
                   \"session\":\"t\",\"value\":\"ls -la\"}\\033\\\\'; sleep 0.5";
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(cmd), "t".into(), 0, None).expect("spawn");

        // Poll drain() until the block stack has flipped to Running
        // (or we time out — which would mean the wiring is broken).
        let stop = Instant::now() + Duration::from_secs(2);
        loop {
            session.drain();
            if matches!(session.blocks().last(), Some(crate::block::Block::Running { .. })) {
                break;
            }
            if Instant::now() >= stop {
                panic!(
                    "DCS-JSON Preexec did not promote tail to Running within 2s; \
                     tail still: {:?}; bytes received: {}",
                    session.blocks().last(),
                    session.view().bytes_received
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
        match session.blocks().last().unwrap() {
            crate::block::Block::Running { command, .. } => assert_eq!(command, "ls -la"),
            other => panic!("expected Running, got {other:?}"),
        }
    }

    // ---- submit + echo suppression (Phase 4C) ----------------------
    //
    // The submit-path tests drive a real `cat` PTY: a DCS-JSON
    // `integration_ready` + `precmd` marker pair flips the pane into
    // `ShellPromptEditor` mode, then we type into the editor and
    // `submit_editor_command`. The kernel echoes the submitted bytes
    // back; cat ALSO echoes them as its stdout. The suppressor must
    // strip the kernel echo so the grid shows the bytes only ONCE
    // (from cat's output).

    /// Compose the synthetic DCS-JSON sequence that gets the
    /// controller into `ShellPromptEditor` from a fresh
    /// `new_no_bootstrap` spawn: `integration_ready` confirms
    /// integration, `precmd` promotes the mode.
    fn dcs_promote_to_editor_cmd() -> String {
        "printf '\\033PTermica;{\"type\":\"integration_ready\",\
                  \"session\":\"t\",\"value\":{\"shell\":\"zsh\",\"version\":1}}\\033\\\\\
                  \\033PTermica;{\"type\":\"precmd\",\
                  \"session\":\"t\",\"value\":\"/tmp\"}\\033\\\\'; cat"
            .to_string()
    }

    #[test]
    fn submit_editor_command_demotes_mode_clears_editor_arms_suppressor() {
        let cmd = dcs_promote_to_editor_cmd();
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(&cmd), "t".into(), 0, None).expect("spawn");
        // Wait for the Precmd → ShellPromptEditor transition.
        let stop = Instant::now() + Duration::from_secs(2);
        loop {
            session.drain();
            if session.editor_is_active() {
                break;
            }
            if Instant::now() >= stop {
                panic!(
                    "never reached ShellPromptEditor; controller mode={:?}, integration={:?}",
                    session.controller.mode(),
                    session.controller.integration(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
        // Type a command and submit.
        session.editor_mut().unwrap().insert_str("hello");
        session.submit_editor_command().expect("submit");
        // Eager demote: mode flipped to RawTerminal before the
        // PTY write returned.
        assert_eq!(session.controller.mode(), PaneMode::RawTerminal);
        // Editor cleared.
        assert!(
            session.blocks.editor_on_tail().unwrap().is_empty(),
            "editor should be empty after submit"
        );
        // Suppressor armed with the bytes we wrote (8 = "hello\r"
        // → 7 bytes after CR→CRLF expansion).
        assert!(session.terminal.echo_suppressor().is_armed());
        assert_eq!(session.terminal.echo_suppressor().pending_len(), 7);
    }

    #[test]
    fn echo_suppression_prevents_duplicate_echo_in_grid() {
        let cmd = dcs_promote_to_editor_cmd();
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(&cmd), "t".into(), 0, None).expect("spawn");
        // Reach ShellPromptEditor.
        let stop = Instant::now() + Duration::from_secs(2);
        loop {
            session.drain();
            if session.editor_is_active() {
                break;
            }
            if Instant::now() >= stop {
                panic!("editor never active");
            }
            thread::sleep(Duration::from_millis(20));
        }
        session.editor_mut().unwrap().insert_str("hello");
        session.submit_editor_command().expect("submit");
        // Wait for cat's output to come back (the only "hello"
        // that should appear in the grid).
        let view =
            wait_for(&mut session, Duration::from_secs(2), |v| v.screen_text.contains("hello"));
        // Count occurrences of "hello" — must be exactly one.
        // Without suppression, we'd see TWO: the kernel echo and
        // cat's output.
        let count = view.screen_text.matches("hello").count();
        assert_eq!(
            count, 1,
            "expected exactly one 'hello' in grid; got {} occurrences:\n{}",
            count, view.screen_text
        );
    }

    #[test]
    fn continuation_event_restores_editor_text_with_newline() {
        // Drive the full continuation flow: get to ShellPromptEditor,
        // submit `echo 1 &&` (which the SHELL would parse as
        // incomplete), then synthesise a `continuation` DCS event
        // arriving from the PTY. The pane's drain must:
        //   1. Re-populate the editor with `echo 1 &&\n`.
        //   2. Flip mode back to `ShellPromptEditor`.
        let printf_continuation = "printf '\\033PTermica;{\"type\":\"continuation\",\
                                    \"session\":\"t\",\"value\":\"\"}\\033\\\\'";
        // The integration markers, then sleep (so the test process
        // is alive), then on stdin we'll later submit + drive the
        // continuation marker via a separate write.
        let cmd = format!("{}; {}; sleep 5", dcs_promote_to_editor_cmd(), printf_continuation);
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(&cmd), "t".into(), 0, None).expect("spawn");

        // Wait for the integration/precmd markers AND the
        // continuation marker (the printf above) — all delivered as
        // PTY output before sleep blocks.
        //
        // After both, the controller should:
        //   - Be in ShellPromptEditor (precmd) initially.
        //   - Stay or return to ShellPromptEditor after continuation.
        let stop = Instant::now() + Duration::from_secs(3);
        loop {
            session.drain();
            // Once we've received the continuation, mode is editor
            // and the controller's last transition reason is
            // ContinuationMarker — but we only assert that after we
            // do a manual submit. Here, just wait for the printfs
            // to land in the byte stream.
            if session.view().bytes_received >= 100 || Instant::now() >= stop {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // Now type into the editor and submit, then artificially
        // inject another continuation via the shell's stdin (we
        // can't easily get zsh to emit PS2 in this test harness
        // because zsh isn't actually our spawned process — we
        // spawned `/bin/sh -c`. So we test the wiring end-to-end
        // by injecting the DCS bytes through the PTY directly).
        //
        // Make the editor active and put a fake "submitted" state:
        assert!(session.editor_is_active(), "editor should be active after precmd");
        session.editor_mut().unwrap().insert_str("echo 1 &&");
        session.submit_editor_command().expect("submit");
        assert_eq!(session.controller.mode(), PaneMode::RawTerminal);
        assert!(session.last_submitted.as_deref() == Some("echo 1 &&"));

        // Inject a continuation DCS marker by feeding bytes
        // straight into the terminal parser. (Writing to the PTY
        // master sends bytes to the shell's stdin; in canonical
        // mode the kernel only echoes complete lines, and our
        // continuation marker has no trailing `\n` — so the echo
        // wouldn't surface back through the master read end for
        // drain to see. Direct feed exercises the same code path
        // that real shell-emitted continuation bytes would hit.)
        let cont_bytes =
            b"\x1bPTermica;{\"type\":\"continuation\",\"session\":\"t\",\"value\":\"\"}\x1b\\";
        session.terminal_mut().feed(cont_bytes);
        let stop = Instant::now() + Duration::from_secs(3);
        loop {
            session.drain();
            if session.controller.last_transition().reason
                == crate::shell::TransitionReason::ContinuationMarker
            {
                break;
            }
            if Instant::now() >= stop {
                panic!(
                    "ContinuationMarker transition never recorded; mode={:?}, last_transition={:?}",
                    session.controller.mode(),
                    session.controller.last_transition(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
        // Editor is now back, populated with the submitted text + \n.
        assert!(session.editor_is_active(), "editor should be active after continuation");
        let editor = session.blocks.editor_on_tail().expect("editor");
        assert_eq!(editor.text(), "echo 1 &&\n", "editor should hold submitted text + \\n");

        // ---- Second submit after continuation: only the SUFFIX
        // beyond `last_submitted` should reach the PTY. We don't
        // have a clean handle on "what bytes were written" here, so
        // we verify via the suppressor state: the suppressor is
        // primed with the bytes we wrote + CRLF expansion. If the
        // pane re-sent the full editor text, the pending-len would
        // be much larger.
        session.editor_mut().unwrap().insert_str("echo 2");
        assert_eq!(session.blocks.editor_on_tail().unwrap().text(), "echo 1 &&\necho 2");
        session.submit_editor_command().expect("second submit");
        // We sent only "echo 2\r" (7 bytes). With \r → \r\n expansion
        // the suppressor's pending length is 8 (e c h o ' ' 2 \r \n).
        assert_eq!(
            session.terminal.echo_suppressor().pending_len(),
            8,
            "second submit must send only the suffix beyond last_submitted (was 'echo 2\\r' → \
             suppressor primed for 8 bytes), not the cumulative editor text"
        );
        // And `last_submitted` should now hold the full editor text
        // so the next continuation restore picks up correctly.
        assert_eq!(session.last_submitted.as_deref(), Some("echo 1 &&\necho 2"));

        // Bring the shell down quickly.
        let _ = session.write(b"\x03");
    }

    #[test]
    fn drain_on_idle_session_returns_zero() {
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("sleep 0.2"), "test-session".into(), 0, None)
                .expect("spawn");
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
            0,
            None,
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
            PaneSession::spawn(5, 40, &sh_c("sleep 5"), "test-session".into(), 0, None)
                .expect("spawn");
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
            PaneSession::spawn(5, 40, &sh_c("printf bye; exit 0"), "test-session".into(), 0, None)
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
            PaneSession::spawn(5, 40, &sh_c("printf done; exit 0"), "test-session".into(), 0, None)
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

    #[test]
    fn drain_emits_pty_exit_to_event_recorder() {
        // End-to-end check that the recorder receives a `pty_exit`
        // line when the shell child exits. We spawn a short-lived
        // `printf; exit` and poll drain() until is_exited() flips.
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("events.log");
        let recorder = Arc::new(EventRecorder::new(&log_path).expect("recorder"));
        let mut session = PaneSession::spawn(
            5,
            40,
            &sh_c("printf hi; exit 0"),
            "test-session".into(),
            42,
            Some(recorder.clone()),
        )
        .expect("spawn");

        // Spin drain until the reader observes EOF (latches `exited`)
        // and the recorder writes the pty_exit line. Bounded by 2s.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            session.drain();
            if session.is_exited() {
                // One more drain to ensure the transition lines flush.
                session.drain();
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // Drop the session to flush the recorder's BufWriter via
        // its own Drop (the recorder is shared so we drop our handle
        // separately below).
        drop(session);
        drop(recorder);

        let body = std::fs::read_to_string(&log_path).expect("read log");
        assert!(body.contains("pane=42"), "log should tag pane id; got:\n{body}");
        assert!(body.contains("pty_exit"), "log should contain pty_exit; got:\n{body}");
        assert!(
            body.contains("Dead") && body.contains("PtyExit"),
            "log should record the transition to Dead; got:\n{body}"
        );
    }

    // ---- Phase 4F sealed-block selection: cross-clearing invariants -

    fn quiet_session() -> PaneSession {
        PaneSession::spawn(5, 40, &sh_c("printf x"), "test-session".into(), 0, None)
            .expect("spawn /bin/sh")
    }

    fn dummy_block_sel() -> crate::block_selection::BlockSelection {
        use crate::block_selection::{BlockCursor, BlockSelection};
        BlockSelection::new(
            crate::block::BlockId(99),
            BlockCursor::new(0, 0),
            BlockCursor::new(0, 5),
        )
    }

    #[test]
    fn set_block_selection_clears_live_grid_selection() {
        let mut session = quiet_session();
        session.start_selection(
            alacritty_terminal::index::Point {
                line: alacritty_terminal::index::Line(0),
                column: alacritty_terminal::index::Column(0),
            },
            crate::selection::SelectionMode::Char,
        );
        assert!(session.selection().is_some(), "precondition: live grid selection set");

        session.set_block_selection(dummy_block_sel());

        assert!(session.selection().is_none(), "live-grid selection should be cleared");
        assert!(session.block_selection().is_some(), "block selection should be set");
    }

    #[test]
    fn start_selection_clears_block_selection() {
        let mut session = quiet_session();
        session.set_block_selection(dummy_block_sel());
        assert!(session.block_selection().is_some(), "precondition");

        session.start_selection(
            alacritty_terminal::index::Point {
                line: alacritty_terminal::index::Line(0),
                column: alacritty_terminal::index::Column(0),
            },
            crate::selection::SelectionMode::Char,
        );

        assert!(session.block_selection().is_none(), "block selection should be cleared");
        assert!(session.selection().is_some(), "live-grid selection should be set");
    }

    #[test]
    fn block_selection_text_returns_none_when_block_id_does_not_exist() {
        let mut session = quiet_session();
        // dummy_block_sel points at BlockId(99) — the fresh session
        // only has a Prompt block at id 0, so the lookup misses.
        session.set_block_selection(dummy_block_sel());
        assert_eq!(session.block_selection_text(), None);
    }

    #[test]
    fn update_block_selection_endpoints_preserves_block_id() {
        use crate::block_selection::BlockCursor;
        let mut session = quiet_session();
        session.set_block_selection(dummy_block_sel());
        let original_id = session.block_selection().unwrap().block_id;

        session.update_block_selection_endpoints(BlockCursor::new(2, 3), BlockCursor::new(4, 7));

        let sel = session.block_selection().expect("still set");
        assert_eq!(sel.block_id, original_id, "block_id stays pinned");
        assert_eq!(sel.anchor, BlockCursor::new(2, 3));
        assert_eq!(sel.head, BlockCursor::new(4, 7));
    }

    #[test]
    fn clear_block_selection_leaves_live_grid_alone() {
        let mut session = quiet_session();
        session.start_selection(
            alacritty_terminal::index::Point {
                line: alacritty_terminal::index::Line(0),
                column: alacritty_terminal::index::Column(0),
            },
            crate::selection::SelectionMode::Char,
        );
        session.clear_block_selection();
        assert!(session.selection().is_some(), "live-grid selection untouched");
    }
}
