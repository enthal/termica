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
use crate::completion::drivers::{CompletionDriverEngine, DriverResponse, DriverTool};
use crate::events::{CompletionEvent, EventRecorder};
use crate::gh_probe::GhProbe;
use crate::git_context::GitContext;
use crate::git_probe::GitProbe;
use crate::history::{
    CaptureState, HistoryContext, RecallOutcome, RecallState, Scope, capture_on_event,
};
use crate::integration::{ManagedSpawn, ShellSpec, managed_spawn_for, new_session_id};
use crate::pane_selection::{PaneCursor, PaneSelection};
use crate::pr_context::PrContext;
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
    /// The live PTY. `None` for a **restored** pane (9F): a pane rebuilt
    /// from persisted scrollback has no shell until "Restart" spawns one.
    /// All PTY-touching paths (`write`, `resize`, `drain`) guard on this;
    /// `drain` early-returns when it's `None`.
    pty: Option<PtySession>,
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
    /// Phase 4F-cross-block: pane-spanning selection over sealed
    /// blocks. The anchor and head each carry their own [`BlockId`]
    /// so a drag started in block A and extended into block B
    /// produces a selection whose `anchor.block_id == A`,
    /// `head.block_id == B`. Independent of [`Self::selection`]
    /// (which covers the live alacritty grid). At most one of the
    /// two is populated at any moment — the setters clear the other
    /// so the user sees a single "current selection" across the pane.
    pane_selection: Option<PaneSelection>,
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
    /// 4G-async-context: off-thread git probe for the pane's cwd. Fed a
    /// directory whenever the cwd changes or a command finishes; its
    /// result populates [`Self::git_context`]. Dropped (and so torn
    /// down) with the pane.
    git_probe: GitProbe,
    /// Latest parsed git context for the pane's cwd, or `None` outside a
    /// repo / before the first probe returns. Drives the branch / dirty
    /// chips on the live prompt + running block headers.
    git_context: Option<GitContext>,
    /// 4G-async-context: off-thread GitHub PR probe (`gh pr view`). Fed
    /// the cwd when the cwd or the branch changes; its result populates
    /// [`Self::pr_context`]. Dropped (torn down) with the pane.
    gh_probe: GhProbe,
    /// Latest parsed PR context for the pane's branch, or `None` when the
    /// branch has no open PR / before the first probe returns. Drives the
    /// PR chip on the live prompt header.
    pr_context: Option<PrContext>,
    /// CLI-native completion driver engine ([spec/04a §"Source 1"](../spec/04a-completion.md)),
    /// lazily spawned on the first Tab in a driver-eligible command
    /// (it needs an `egui::Context` for repaint-on-result, which only the
    /// renderer has). Dropped (torn down) with the pane.
    completion_driver: Option<CompletionDriverEngine>,
    /// Last git branch seen from [`Self::git_context`], to detect a
    /// branch change (e.g. `git checkout`) and re-probe the PR even when
    /// the cwd is unchanged.
    last_git_branch: Option<String>,
    /// Live variable names reported by the shell via the `shell_vars`
    /// marker (emitted from the precmd hook, change-gated shell-side).
    /// `Some` once the shell has reported at least once; supersedes the
    /// spawn-time [`crate::pty::PtySession::env_var_names`] snapshot as the
    /// `$VAR`-completion source because it reflects the LIVE shell —
    /// non-exported parameters (`HISTFILE`, …) and runtime `export`s
    /// included. `None` before the first report (and when integration is
    /// absent), so completion falls back to the spawn snapshot.
    shell_var_names: Option<Vec<String>>,
    /// Background scrollback writer (9D). `Some` when persistence is
    /// available (managed spawn with an open `termica.sqlite`); each
    /// sealed block's snapshot is forwarded to it for durable chunk
    /// writing. `None` on the low-level `spawn` path and in degraded
    /// mode. Dropped with the pane → its thread exits (RAII teardown).
    chunk_writer: Option<crate::persist::writer::ChunkWriter>,
    /// This session's ownership lock (9F). Held for the pane's lifetime
    /// so no other Termica process adopts the session while it's live;
    /// released on pane drop / process death. `Some` exactly when
    /// `chunk_writer` is. See [spec/08 §"Concurrent processes"].
    /// Held purely for its `Drop` (lock release); never read.
    #[allow(dead_code)]
    session_lock: Option<crate::persist::lock::SessionLock>,
    /// This pane's durable `pane` row id (9D/9F), `Some` when
    /// persistence is active. Saved into the layout blob so a relaunch
    /// can reconnect the restored pane to its scrollback chunks (which
    /// are keyed by this db id, not the ephemeral app `PaneId`). Also
    /// the `session.id` we stamp `ended_at` on at teardown.
    persist_pane_row: Option<i64>,
    /// This pane's current `session` row id (its live PTY spawn), for
    /// stamping `ended_at` on close/quit.
    persist_session: Option<i64>,
    /// Which managed shell this pane runs. Drives shell-specific command
    /// submission framing ([`crate::submit_framing`]) — notably base64 for
    /// fish, whose non-interactive read-eval loop needs the whole command
    /// on one tty line. Defaults to `Zsh` (verbatim framing) for the bare
    /// [`Self::spawn`] path used by tests; [`Self::spawn_managed`] sets the
    /// real shell.
    shell: ShellSpec,
    /// In-flight **live-shell** completion request, if any. A fish or zsh
    /// pane at a prompt answers completion from its OWN shell (so runtime-
    /// defined aliases / functions complete — a one-shot subprocess can't
    /// see them) via a PTY request; this holds the correlation `id` (echoed
    /// in the reply marker, so a superseded request's late reply is dropped),
    /// the wall-clock send time (for the timeout fallback), and the
    /// originating tool (so the reply's candidates get the right source tag,
    /// `fish` / `zsh`). At most one at a time — a newer Tab/keystroke
    /// supersedes it with a fresh id.
    live_completion: Option<LiveCompletion>,
    /// A resolved live-shell reply, awaiting [`Self::completion_driver_poll`].
    /// Set when the correlated `completion` marker lands in [`Self::drain`].
    live_completion_response: Option<DriverResponse>,
    /// Monotonic id source for live-shell completion requests.
    next_live_completion_id: u64,
}

/// Bookkeeping for an in-flight live-shell completion request
/// ([`PaneSession::live_completion`]).
struct LiveCompletion {
    /// Correlation id, echoed back in the `completion` marker.
    id: u64,
    /// Wall-clock send time (ms), for the timeout fallback.
    sent_ms: i64,
    /// The tool that issued the request ([`DriverTool::FishComplete`] /
    /// [`DriverTool::ZshComplete`]), so the reply candidates are tagged with
    /// the right source.
    tool: DriverTool,
}

/// Wall-clock deadline for a live-shell completion reply. fish's `complete -C`
/// and zsh's warm completion child both answer in ~ms; this only guards a
/// hung shell or a dropped marker — on expiry we fall back to the local
/// candidates rather than spin forever.
const LIVE_COMPLETION_TIMEOUT_MS: i64 = 600;

/// Choose the `$VAR`-completion name source: the shell's live report
/// (`shell`) when it has arrived, else the spawn-time `snapshot`. Pure so
/// the precedence is testable without spawning a shell. Kept as a borrow
/// (no allocation) — the caller already owns both slices.
fn effective_var_names<'a>(shell: Option<&'a [String]>, snapshot: &'a [String]) -> &'a [String] {
    shell.unwrap_or(snapshot)
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
            pty: Some(pty),
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
            pane_selection: None,
            history: None,
            capture_state: CaptureState::default(),
            recall: RecallState::default(),
            git_probe: GitProbe::spawn(),
            git_context: None,
            gh_probe: GhProbe::spawn(),
            pr_context: None,
            completion_driver: None,
            last_git_branch: None,
            shell_var_names: None,
            chunk_writer: None,
            session_lock: None,
            persist_pane_row: None,
            persist_session: None,
            shell: ShellSpec::Zsh,
            live_completion: None,
            live_completion_response: None,
            next_live_completion_id: 0,
        })
    }

    /// Construct a **restored** pane (9F): no live PTY, a `BlockStack`
    /// pre-populated with the persisted scrollback, and the controller
    /// already in `Dead`. The renderer draws the sealed blocks above an
    /// empty live grid; "Restart" later spawns a real shell into it
    /// (9F-restart). `pane_row_id` is the durable `pane` row this was
    /// restored from, kept so a re-quit re-saves the layout.
    pub fn restored(
        rows: u16,
        cols: u16,
        blocks: BlockStack,
        pane_id: u64,
        pane_row_id: i64,
        cwd: Option<std::path::PathBuf>,
    ) -> Self {
        // No PTY: a disconnected byte channel + an immediately-returning
        // reader thread stand in for the live reader so the struct's
        // ownership shape is unchanged. `drain()` early-returns on a
        // `None` pty, so this channel is never actually read.
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        drop(tx); // rx is now disconnected
        let handle = thread::spawn(|| {});
        // Start past bootstrap, then drive straight to `Dead`.
        let mut controller = PromptController::new_no_bootstrap(0);
        controller.observe_pty_exit(0);
        let mut terminal = TerminalState::new(rows, cols);
        // Recover the last-known cwd so the tab title reads the path.
        if let Some(cwd) = cwd {
            terminal.seed_cwd(cwd);
        }
        Self {
            pty: None,
            terminal,
            bytes_rx: rx,
            bytes_received: 0,
            selection: None,
            exited: false,
            _reader: handle,
            controller,
            blocks,
            session_id: String::new(),
            frame: 0,
            last_alt_screen: false,
            last_transition_at: 0,
            wrapper_dir: None,
            pane_id,
            recorder: None,
            last_submitted: None,
            pane_selection: None,
            history: None,
            capture_state: CaptureState::default(),
            recall: RecallState::default(),
            git_probe: GitProbe::spawn(),
            git_context: None,
            gh_probe: GhProbe::spawn(),
            pr_context: None,
            completion_driver: None,
            last_git_branch: None,
            shell_var_names: None,
            chunk_writer: None,
            session_lock: None,
            persist_pane_row: Some(pane_row_id),
            persist_session: None,
            // A restored pane is `Dead`: no live shell, so it never issues
            // completion requests. Defaults mirror the bare `spawn` path;
            // `Restart` rebuilds a real pane with the right shell.
            shell: ShellSpec::Zsh,
            live_completion: None,
            live_completion_response: None,
            next_live_completion_id: 0,
        }
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
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_managed(
        rows: u16,
        cols: u16,
        shell: ShellSpec,
        cwd: Option<std::path::PathBuf>,
        pane_id: u64,
        recorder: Option<Arc<EventRecorder>>,
        history: Option<HistoryContext>,
        persist: Option<crate::persist::store::Persistence>,
        // `Some(db_pane_id)` to RESUME an existing pane (restart, 9F):
        // the pane's durable identity is reused so its chunks accumulate
        // across restarts. `None` begins a fresh pane.
        resume_pane_row: Option<i64>,
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
        // Capture the starting cwd as a string before `cwd` moves into
        // the PtyConfig — the persisted `pane` row records it.
        let cwd_str = cwd.as_ref().map(|p| p.display().to_string());
        let config = PtyConfig { program, args, env, cwd, rows, cols };
        let mut session = Self::spawn(rows, cols, &config, session_id, pane_id, recorder)?;
        session.shell = shell;
        session.history = history;
        // Persistence (9D): allocate this session's pane + session rows
        // and its on-disk scrollback directory, then spawn the background
        // chunk writer. Best-effort — a failure here leaves the pane fully
        // usable, just without durable scrollback (same posture as
        // history). `None` persist (degraded mode / low-level spawn) skips
        // it entirely.
        if let Some(persist) = persist {
            // Restart reuses the pane row (chunks accumulate); a fresh
            // pane begins a new one.
            let begun = match resume_pane_row {
                Some(pane_row) => persist.resume_session(pane_row, wall_clock_ms()),
                None => persist.begin_session(cwd_str.as_deref(), shell.name(), wall_clock_ms()),
            };
            match begun {
                Ok(record) => {
                    session.chunk_writer = Some(crate::persist::writer::ChunkWriter::spawn(
                        record.dir,
                        persist.store_handle(),
                        record.session,
                        record.pane_row,
                        record.start_line,
                    ));
                    // Hold the session-ownership lock for the pane's life.
                    session.session_lock = Some(record.lock);
                    session.persist_pane_row = Some(record.pane_row.0);
                    session.persist_session = Some(record.session.0);
                }
                Err(e) => eprintln!("termica: persistence session start failed: {e}"),
            }
        }
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
            if let Some(pty) = session.pty.as_mut() {
                pty.write(bootstrap.as_bytes())?;
            }
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
        // A restored pane (9F) has no live PTY: nothing to drain, and we
        // must NOT let the disconnected byte channel latch `exited` (which
        // would auto-close the pane). It stays `Dead` until "Restart".
        if self.pty.is_none() {
            return 0;
        }
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

        // Answer terminal queries the feed above produced. Programs
        // probe the terminal (Primary/Secondary Device Attributes,
        // cursor-position reports) and BLOCK on the reply; alacritty
        // generated the response bytes during `feed`, and they must be
        // written back to the PTY master or the program hangs until
        // its own timeout (e.g. `gh` via termenv waits ~10s). A failed
        // write means the child already went away, in which case the
        // reply is moot — drop it silently rather than surfacing an
        // error from this per-frame drain.
        let responses = self.terminal.drain_pty_responses();
        if !responses.is_empty()
            && let Some(pty) = self.pty.as_mut()
        {
            let _ = pty.write(&responses);
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

        // Time out a live-shell completion request the shell never
        // answered (hung / dropped marker), so the popup falls back to
        // locals instead of spinning. No-op when none is in flight.
        self.tick_live_completion_timeout(wall_clock_ms());

        // Sync the controller's alt-screen view BEFORE any
        // lifecycle event runs. The bytes above may have toggled
        // alacritty's alt-screen flag (`1049h` / `1049l`) — without
        // catching that here, a `Precmd` event later in this drain
        // would call `try_promote_to_editor` while the controller
        // is still in stale `AlternateScreen` mode and refuse to
        // promote (it requires `RawTerminal`). The post-loop
        // edge-detect below still runs to catch any `1049h` that
        // arrived without a matching event in this batch.
        let alt = self.terminal.is_alternate_screen();
        if alt != self.last_alt_screen {
            self.controller.observe_alt_screen(alt, self.frame);
            self.last_alt_screen = alt;
            self.record_pending_transitions();
        }

        // Feed lifecycle events extracted from the byte stream into
        // the controller AND the block stack. Order is preserved
        // per spec/03. The block stack reads the terminal snapshot
        // at seal time, so it must run AFTER the bytes that produced
        // this event have been fed into the grid (they were, above).
        // 4G-async-context: a command finishing this frame is a forced
        // re-probe trigger — the working tree may now be dirty even
        // though the cwd is unchanged.
        let mut command_finished = false;
        for event in self.terminal.drain_lifecycle_events() {
            if let Some(rec) = self.recorder.as_ref() {
                rec.record_lifecycle(self.pane_id, &event);
            }
            if matches!(event, crate::markers::LifecycleEvent::CommandFinished { .. }) {
                command_finished = true;
            }
            // Live `$VAR`-completion source: the shell just reported its
            // current variable names (precmd hook, change-gated shell-side).
            // Supersede the spawn-time snapshot. Cloned out before
            // `observe_event` consumes the event below.
            if let crate::markers::LifecycleEvent::ShellVars { names } = &event {
                self.shell_var_names = Some(names.clone());
            }
            // Live-shell completion reply (fish or zsh): correlate to the
            // in-flight request and stash the candidates for
            // `completion_driver_poll`. Cloned out before `observe_event`
            // consumes the event below; it's inert to the mode machine (spec/05).
            if let crate::markers::LifecycleEvent::Completion { id, lines } = &event {
                self.ingest_live_completion(*id, lines);
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
            // `Precmd` ALSO clears it as a backstop for the case
            // where Preexec is missed (shell hook race under load,
            // dropped marker, etc.) but the next prompt cycle does
            // emit Precmd. Without this backstop, a stale
            // `last_submitted` from the previous command would
            // make a subsequent submit whose text starts with that
            // stale value (including identical re-submits) send
            // only the empty / wrong diff — the command appears
            // to "vanish": editor clears, nothing reaches the
            // shell, no Preexec → no Running block → nothing in
            // history. Precmd never fires for the PS2 continuation
            // prompt, so the legitimate continuation flow is
            // unaffected: between a `Submit` and a `Continuation`
            // marker (PS2 → `last_submitted` consulted to restore
            // the editor + the next-submit diff path), Precmd
            // hasn't fired and the prefix is intact.
            if matches!(
                event,
                crate::markers::LifecycleEvent::Preexec { .. }
                    | crate::markers::LifecycleEvent::Precmd { .. }
            ) {
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
            // normal way (clears flag, restores grid), THEN notify
            // the controller of the alt-screen drop synchronously —
            // this is the load-bearing detail: the controller is
            // still in `AlternateScreen` mode at this point, and the
            // `try_promote_to_editor` call inside `observe_event`
            // below requires `RawTerminal`. Without the synchronous
            // edge notification, the post-loop edge-detect would
            // drop alt-screen mode AFTER the Precmd was already
            // consumed, leaving the pane stranded in `RawTerminal`
            // with no editor. No-op when alt-screen is already off.
            if matches!(event, crate::markers::LifecycleEvent::Precmd { .. })
                && self.terminal.is_alternate_screen()
            {
                self.terminal.feed(b"\x1b[?1049l");
                self.controller.observe_alt_screen(false, self.frame);
                self.last_alt_screen = false;
                self.record_pending_transitions();
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
            // Stamp the event with the same wall-clock time used for
            // history, so a Preexec → CommandFinished pair seals the
            // block with its real duration (4G).
            self.blocks.set_event_clock_ms(now_ms);
            // Freeze the current git context into the block if this event
            // starts a command (`Preexec`), so a sealed block shows the
            // branch / dirty it ran under (4G-async-context). `git_context`
            // here is last frame's probe result — the state just before
            // the command ran, which is exactly what we want.
            self.blocks.set_current_git(self.git_context.clone());
            // A `CommandFinished` seals the running block; forward its
            // snapshot (logical lines) to the background writer for
            // durable chunk persistence (9D). Other events return `None`.
            if let Some(sealed) =
                self.blocks.observe_lifecycle_event(&event, &mut self.terminal, self.frame)
                && let Some(writer) = &self.chunk_writer
            {
                writer.submit(sealed);
            }
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

        // 4G-async-context: drive the off-thread git probe. Request a
        // fresh probe when the cwd changed (the probe dedups internally)
        // or a command just finished (forced — same cwd, newly dirty),
        // then fold in whatever the worker has finished. Both are
        // non-blocking; the chips update on a later frame when the
        // result lands (idle frames repaint every ~300 ms — see
        // `app::update`).
        if let Some(cwd) = self.current_cwd() {
            self.git_probe.request(&cwd, command_finished);
        }
        if let Some(result) = self.git_probe.poll() {
            self.git_context = result.context;
        }

        // 4G-async-context: drive the off-thread GitHub PR probe. `gh` is
        // slow + networked, so we re-probe only on cwd change (dedup'd by
        // the probe) or a branch change — a `git checkout` keeps the cwd
        // but swaps the PR. The probe self-refreshes while CI is pending,
        // so a chip watching CI go green updates without our help.
        if let Some(cwd) = self.current_cwd() {
            let branch = self.git_context.as_ref().and_then(|g| g.branch.clone());
            let branch_changed = branch != self.last_git_branch;
            self.last_git_branch = branch;
            self.gh_probe.request(&cwd, branch_changed);
        }
        if let Some(result) = self.gh_probe.poll() {
            self.pr_context = result.pr;
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
            cwd: self.current_cwd(),
            screen_text: self.terminal.screen_text(),
            mode: Some(mode),
            is_bootstrapping: mode == PaneMode::Bootstrapping,
        }
    }

    /// The pane's current working directory: the controller's cwd
    /// (OSC 7 / DCS-JSON `Precmd`) if known, else the terminal's. The
    /// view snapshot and the git probe both resolve cwd this way.
    fn current_cwd(&self) -> Option<std::path::PathBuf> {
        self.controller
            .cwd()
            .map(|p| p.to_path_buf())
            .or_else(|| self.terminal.cwd().map(|p| p.to_path_buf()))
    }

    /// Latest parsed git context for the pane's cwd, or `None` outside a
    /// repo / before the first async probe returns. The renderer reads
    /// this for the live prompt + running block-header branch/dirty
    /// chips (4G-async-context).
    pub fn git_context(&self) -> Option<&GitContext> {
        self.git_context.as_ref()
    }

    /// Latest GitHub PR context for the pane's branch, or `None` when the
    /// branch has no open PR / before the first probe returns. The
    /// renderer reads this for the live prompt header's PR chip
    /// (4G-async-context).
    pub fn pr_context(&self) -> Option<&PrContext> {
        self.pr_context.as_ref()
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

    /// Live elapsed time of the currently-running command (the tail
    /// `Running` block), or `None` when the shell is idle at a prompt.
    /// Read each frame by the renderer to paint the ticking duration
    /// chip; reads the wall clock here so [`BlockStack::running_elapsed_at`]
    /// stays pure / testable.
    pub fn running_elapsed(&self) -> Option<std::time::Duration> {
        self.blocks.running_elapsed_at(wall_clock_ms())
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

    /// This pane's durable `pane` row id, if persistence is active.
    /// Saved into the layout blob so a restored pane reconnects to its
    /// scrollback chunks.
    pub fn persist_pane_row(&self) -> Option<i64> {
        self.persist_pane_row
    }

    /// This pane's live `session` row id, if persistence is active.
    /// Stamped with `ended_at` when the pane closes or the app quits.
    pub fn persist_session(&self) -> Option<i64> {
        self.persist_session
    }

    /// Whether the pane is in `Dead` mode — its shell has exited or it
    /// was restored without one. Drives the "Restart shell" affordance.
    pub fn is_dead(&self) -> bool {
        self.controller.mode() == crate::shell::PaneMode::Dead
    }

    /// Consume the pane and return just its `Sealed` scrollback blocks.
    /// Restart (9F) uses this to carry a restored pane's transcript into
    /// the freshly-spawned shell via [`Self::adopt_restored_scrollback`].
    pub fn into_sealed_blocks(self) -> Vec<crate::block::Block> {
        self.blocks.into_sealed()
    }

    /// Replace this (freshly-spawned) pane's empty block stack with one
    /// carrying `sealed` scrollback under a fresh live `Prompt` tail, so
    /// a restarted shell's output appends *below* the restored
    /// transcript. Called right after `spawn_managed` during restart.
    pub fn adopt_restored_scrollback(&mut self, sealed: Vec<crate::block::Block>) {
        self.blocks = BlockStack::with_restored_sealed(sealed);
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
        let current_cursor = self.blocks.editor_on_tail().map(|e| e.cursor()).unwrap_or_default();
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
            current_cursor,
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

    /// Pull recent commands from history for the Tab-completion
    /// popup. Returns at most `limit` strings, newest first. Empty
    /// when persistence is unavailable or the query fails — the
    /// caller treats that as "no history candidates" gracefully.
    ///
    /// Scope: global (cross-pane). Tab completion prefers global
    /// history because typing the same command across sessions is
    /// the common case; pane-scoped history is for `↑/↓` walk.
    pub fn history_for_completion(&self, limit: usize) -> Vec<String> {
        let Some(ctx) = &self.history else { return Vec::new() };
        let Ok(store) = ctx.store.lock() else { return Vec::new() };
        store
            .recent(&crate::history::Scope::Global, limit)
            .map(|rows| rows.into_iter().map(|r| r.text).collect())
            .unwrap_or_default()
    }

    /// Fire an async CLI-native completion driver request for the current
    /// command's arguments ([spec/04a §"Source 1"](../spec/04a-completion.md)).
    /// Lazily spawns the per-pane engine (it needs `ctx` to repaint when a
    /// result lands). A no-op when the pane has no known cwd — drivers run
    /// in the pane's directory, so without one there's nothing meaningful
    /// to complete against.
    pub fn completion_driver_request(
        &mut self,
        ctx: &egui::Context,
        target: (DriverTool, String, usize),
    ) {
        let (tool, line) = (target.0, target.1.clone());
        // Live-shell path: a fish or zsh pane sitting at a prompt with
        // confirmed integration answers from its OWN shell (so it sees
        // aliases / functions defined at runtime), via a PTY request rather
        // than a one-shot subprocess. The reply lands as a `completion`
        // marker. `ZshComplete` has no one-shot form at all, so it MUST take
        // this path; `FishComplete` falls through to the subprocess engine
        // when the fish pane isn't live-capable (degraded integration).
        if matches!(tool, DriverTool::FishComplete | DriverTool::ZshComplete)
            && self.live_completion_capable()
        {
            self.live_completion_request(tool, line);
            return;
        }
        // Fallback: the one-shot subprocess engine — non-fish panes, or a
        // fish pane whose integration is degraded / not at a prompt.
        let Some(cwd) = self.current_cwd() else { return };
        let engine = self
            .completion_driver
            .get_or_insert_with(|| CompletionDriverEngine::spawn(ctx.clone()));
        let cache_hit = engine.request(cwd, target);
        self.record_completion(&CompletionEvent::DriverRequest { tool, line, cache_hit });
    }

    /// Drain freshly-arrived driver candidates for the current in-flight
    /// request, if any. The renderer merges them into the open popup.
    /// A resolved live-shell reply takes precedence over the one-shot
    /// engine (for a fish / zsh pane at a prompt the engine is never spawned).
    pub fn completion_driver_poll(&mut self) -> Option<DriverResponse> {
        if let Some(resp) = self.live_completion_response.take() {
            // The `DriverResult` event was already recorded when the marker
            // landed (`ingest_live_completion`), so don't double-record.
            return Some(resp);
        }
        let resp = self.completion_driver.as_mut().and_then(|engine| engine.poll());
        if let Some(r) = &resp {
            self.record_completion(&CompletionEvent::DriverResult {
                tool: r.tool,
                candidates: r.candidates.len(),
                cache_hit: r.from_cache,
            });
        }
        resp
    }

    /// True when this pane can answer completion from its **live** shell:
    /// it's a fish OR zsh pane, sitting at a prompt (`ShellPromptEditor`),
    /// with shell integration confirmed. Only then is the bootstrap ready to
    /// service a completion request (fish's read-eval loop; zsh's
    /// `__termica_complete` sentinel + warm completion child).
    fn live_completion_capable(&self) -> bool {
        matches!(self.shell, ShellSpec::Fish | ShellSpec::Zsh)
            && self.controller.mode() == PaneMode::ShellPromptEditor
            && matches!(
                self.controller.integration(),
                crate::shell::IntegrationState::Confirmed { .. }
            )
    }

    /// Write a live-shell completion request for `line` to the PTY, framed
    /// for the pane's shell (`tool` is [`DriverTool::FishComplete`] or
    /// [`DriverTool::ZshComplete`]). Assigns a fresh correlation id
    /// (superseding any in-flight request — its late reply is dropped on
    /// id-mismatch), primes echo suppression for the request bytes (so the
    /// tty's echo of them never reaches the grid, exactly like a submitted
    /// command), and writes. On PTY-write failure it leaves no in-flight
    /// request, so the popup falls back to locals.
    fn live_completion_request(&mut self, tool: DriverTool, line: String) {
        let id = self.next_live_completion_id;
        self.next_live_completion_id = self.next_live_completion_id.wrapping_add(1);
        let bytes = crate::submit_framing::completion_request_bytes_for(self.shell, id, &line);
        self.terminal.prime_echo_suppression(&bytes);
        // A restored / `Dead` pane has no live PTY; it is never in
        // `ShellPromptEditor` mode either, so `live_completion_capable`
        // already excludes it — guard anyway and fall back to locals.
        let Some(pty) = self.pty.as_mut() else {
            return;
        };
        if pty.write(&bytes).is_err() {
            return;
        }
        self.live_completion = Some(LiveCompletion { id, sent_ms: wall_clock_ms(), tool });
        self.record_completion(&CompletionEvent::DriverRequest { tool, line, cache_hit: false });
    }

    /// Correlate a `completion` reply marker to the in-flight request and
    /// stash its candidates for [`Self::completion_driver_poll`]. A reply
    /// whose `id` doesn't match the current request (a superseded request's
    /// late answer) is dropped. The reply carries the **raw** shell lines;
    /// parsing uses the one shared parser tagged with the originating tool,
    /// so the fish and zsh paths handle tabs / padded columns identically.
    fn ingest_live_completion(&mut self, id: u64, lines: &[String]) {
        let Some(req) = self.live_completion.as_ref().filter(|f| f.id == id) else {
            return;
        };
        let tool = req.tool;
        self.live_completion = None;
        let candidates =
            crate::completion::drivers::parse::parse_shell_complete(&lines.join("\n"), tool);
        self.record_completion(&CompletionEvent::DriverResult {
            tool,
            candidates: candidates.len(),
            cache_hit: false,
        });
        self.live_completion_response = Some(DriverResponse::live(tool, candidates));
    }

    /// Drop an in-flight live-shell request the shell never answered within
    /// [`LIVE_COMPLETION_TIMEOUT_MS`], synthesizing an empty result so the
    /// popup resolves to its local candidates instead of spinning. `now_ms`
    /// is passed in (not read from a clock) so the logic is deterministic in
    /// tests. No-op when nothing is in flight.
    fn tick_live_completion_timeout(&mut self, now_ms: i64) {
        if let Some(f) = &self.live_completion
            && now_ms.saturating_sub(f.sent_ms) >= LIVE_COMPLETION_TIMEOUT_MS
        {
            let tool = f.tool;
            self.live_completion = None;
            self.live_completion_response = Some(DriverResponse::live(tool, Vec::new()));
        }
    }

    /// Emit a tab-[`CompletionEvent`] to `TERMICA_DUMP_EVENTS`, if enabled.
    /// Best-effort and side-effect-free otherwise.
    pub fn record_completion(&self, event: &CompletionEvent) {
        if let Some(rec) = self.recorder.as_ref() {
            rec.record_completion(self.pane_id(), event);
        }
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
        // 1. Peek the editor text (without consuming yet). If there's no
        //    editor (the tail isn't a `Prompt`), submit is a no-op. If
        //    the editor is empty, we still send a bare `\r` so the shell
        //    sees a blank line and emits the next prompt — that's what
        //    pressing Enter on an empty shell prompt does in every
        //    terminal.
        let text = match self.blocks.editor_on_tail() {
            Some(editor) => editor.text().to_string(),
            None => return Ok(()),
        };

        // 2-Enter continuation heal. We're mid-continuation — the shell
        // is still waiting to finish a previously-submitted, incomplete
        // line (e.g. the unmatched quote `echo "!"` leaves behind) — and
        // the editor text no longer EXTENDS that line: the user cleared
        // it and/or retyped something different. Submitting it as-is
        // would feed the dangling line and re-lock the pane (every later
        // submit just continues the same broken line). So abort the
        // stuck line with SIGINT and reset the continuation state, but
        // KEEP the editor text: the next Enter, now at a fresh prompt,
        // submits it as a clean first command. (An empty editor counts
        // as abandonment too — clearing the line and pressing Enter once
        // is the "just get me out" gesture.) The legitimate multi-line
        // case (the user APPENDED, so `text` still starts with
        // `last_submitted`) falls through to the normal diff-submit.
        if self.awaiting_continuation()
            && !self.last_submitted.as_deref().is_some_and(|prev| text.starts_with(prev))
        {
            return self.abort_continuation_line();
        }

        // Consume the editor now that we're committing to a real submit.
        if let Some(editor) = self.blocks.editor_on_tail_mut() {
            editor.clear();
            // Per spec/04 §"Undo / redo" reset-on-submit: the previous
            // command's undo history doesn't follow into the next prompt.
            // Done AFTER `clear` because `clear` pushes one undo entry
            // (the pre-clear state), which we promptly throw away.
            editor.reset_undo();
        }

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
        // Frame per shell: zsh/bash get the command verbatim; fish gets it
        // base64-encoded onto one tty line so its single-line read-eval
        // loop receives a multi-line command intact ([`crate::submit_framing`]).
        let bytes = crate::submit_framing::submission_bytes(&to_send, self.shell);
        self.terminal.prime_echo_suppression(&bytes);
        if let Some(pty) = self.pty.as_mut() {
            pty.write(&bytes)?;
        }
        Ok(())
    }

    /// True when a multi-line continuation is pending: we're back in the
    /// editor after a submit the shell considered incomplete (PS2 fired,
    /// `last_submitted` still set — it's cleared on `Preexec`/`Precmd`).
    /// In this sub-state the shell is mid-line-read, NOT idle at a
    /// confirmed prompt, so Ctrl+C must reach it as SIGINT to abort the
    /// dangling line rather than being swallowed as inert.
    pub fn awaiting_continuation(&self) -> bool {
        self.editor_is_active() && self.last_submitted.is_some()
    }

    /// Abort a pending PS2 continuation and clear the editor — the
    /// explicit "give up entirely" gesture, bound to Ctrl+C. The shell is
    /// mid-line-read after a submit it deemed incomplete (e.g. the
    /// unmatched quote zsh derives from `echo "!"`), so a plain submit
    /// can't recover — it only feeds more text into the dangling line,
    /// and Termica's continuation-restore masks that the pane is stuck.
    /// This clears the restored editor text, then aborts the stuck line
    /// (see [`Self::abort_continuation_line`]). No-op when no continuation
    /// is pending, so Ctrl+C stays inert at an idle prompt (spec/04). The
    /// "abort but keep my retyped text" variant is the 2-Enter heal in
    /// [`Self::submit_editor_command`], which reuses the same primitive.
    pub fn abort_continuation(&mut self) -> Result<(), PtyError> {
        if !self.awaiting_continuation() {
            return Ok(());
        }
        if let Some(editor) = self.blocks.editor_on_tail_mut() {
            editor.clear();
            editor.reset_undo();
        }
        self.abort_continuation_line()
    }

    /// Send SIGINT (`\x03`) to abort the shell's dangling continuation
    /// line and reset the continuation state. The shell prints `^C` and a
    /// fresh prompt — exactly like Ctrl+C at a PS2 prompt in any
    /// terminal. Resets `last_submitted` BEFORE the write so the pane
    /// can't be observed mid-abort with a stale value (which would
    /// corrupt the next submit's diff) — the same
    /// make-wrong-states-unrepresentable ordering as `submit`. Leaves the
    /// editor buffer untouched; callers decide whether to clear it
    /// (Ctrl+C does; the 2-Enter heal keeps the retyped text).
    fn abort_continuation_line(&mut self) -> Result<(), PtyError> {
        self.last_submitted = None;
        self.recall.abandon();
        self.write(b"\x03")
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
        match self.pty.as_mut() {
            Some(pty) => pty.write(bytes),
            None => Ok(()), // restored pane, no shell to write to
        }
    }

    /// Resize the PTY and adjust the terminal's grid. Both must
    /// agree, so they live in a single call. The terminal state is
    /// resized in place (existing screen content is preserved); the
    /// kernel-side PTY size is updated so terminal-mode programs
    /// (vim, less, ...) see the new size on their next `read`.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), PtyError> {
        // Resize the grid regardless (sealed blocks reflow to the current
        // width); only push SIGWINCH when there's a live PTY.
        if let Some(pty) = self.pty.as_mut() {
            pty.resize(rows, cols)?;
        }
        self.terminal.resize(rows, cols);
        Ok(())
    }

    /// Borrow the underlying terminal state read-only. The renderer
    /// uses this to walk the grid each frame.
    pub fn terminal(&self) -> &TerminalState {
        &self.terminal
    }

    /// Variable names for `$VAR` tab-completion. Prefers the LIVE names
    /// the shell reports via the `shell_vars` marker (which include
    /// non-exported parameters like `HISTFILE` and runtime `export`s);
    /// before the shell has reported — or when integration is absent —
    /// falls back to the spawn-time environment snapshot
    /// ([`crate::pty::PtySession::env_var_names`], inherited + built-ins +
    /// `TERMICA_*`).
    pub fn env_var_names(&self) -> &[String] {
        let spawn_names = self.pty.as_ref().map(|p| p.env_var_names()).unwrap_or(&[]);
        effective_var_names(self.shell_var_names.as_deref(), spawn_names)
    }

    /// The shell this pane is running. Drives shell-specific completion
    /// routing (a fish pane completes via the `complete -C` sidecar rather
    /// than the per-tool CLI drivers — see
    /// [`crate::completion::plan_completion`]).
    pub fn shell(&self) -> ShellSpec {
        self.shell
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
        self.pane_selection = None;
    }

    /// Begin a fresh `Word`-mode selection whose anchor is glued to
    /// a URL's bounds. Used when a double-click lands inside a
    /// [`crate::links::LinkSpan`] — the user expects the whole URL
    /// to be selected, not the punctuation-bounded word their
    /// pointer is on. See [`Selection::with_url_anchor`].
    pub fn start_url_selection(&mut self, link_start: Point, link_end: Point) {
        self.selection = Some(Selection::with_url_anchor(link_start, link_end));
        self.pane_selection = None;
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

    /// Current pane-spanning sealed-block selection
    /// ([Phase 4F-cross-block](../spec/04-prompt-editor.md#cross-block-selection)),
    /// or `None` when the user hasn't started one — or when a more
    /// recent live-grid selection cleared it.
    pub fn pane_selection(&self) -> Option<&PaneSelection> {
        self.pane_selection.as_ref()
    }

    /// Install a pane-spanning sealed-block selection, replacing any
    /// previous one and clearing the live-grid selection so the pane
    /// only ever shows a single "current selection."
    pub fn set_pane_selection(&mut self, sel: PaneSelection) {
        self.selection = None;
        self.pane_selection = Some(sel);
    }

    /// Drop the pane selection without touching live-grid state.
    /// Called when a click lands outside any sealed block, or before
    /// starting a new live-grid drag.
    pub fn clear_pane_selection(&mut self) {
        self.pane_selection = None;
    }

    /// Replace the head of the current pane selection. The anchor
    /// stays pinned (per spec/04 §"Cross-block selection": the
    /// anchor is the press position; the head follows the pointer).
    /// No-op if no pane selection is active.
    pub fn update_pane_selection_head(&mut self, head: PaneCursor) {
        if let Some(sel) = self.pane_selection.as_mut() {
            sel.head = head;
        }
    }

    /// Replace BOTH endpoints of the current pane selection. Used by
    /// multi-click + drag where the rolling-union of anchor word ∪
    /// head word lands on both endpoints. No-op if no selection.
    pub fn update_pane_selection_endpoints(&mut self, anchor: PaneCursor, head: PaneCursor) {
        if let Some(sel) = self.pane_selection.as_mut() {
            sel.anchor = anchor;
            sel.head = head;
        }
    }

    /// Per-block range to overlay for `block_id`, clipped from the
    /// pane selection's start/end. Returns `None` when no selection,
    /// when `block_id` is outside the selection's block range, or
    /// when the selection is empty.
    pub fn pane_selection_range_for_block(
        &self,
        block_id: crate::block::BlockId,
        block_total_rows: usize,
    ) -> Option<(crate::block_selection::BlockCursor, crate::block_selection::BlockCursor)> {
        self.pane_selection.as_ref()?.block_range_for(block_id, block_total_rows)
    }

    /// Materialise the text currently under the pane selection.
    /// Walks every sealed block in pane reading order; each block in
    /// `[start.block_id, end.block_id]` contributes its clipped piece
    /// to the payload, joined by `\n`. Returns `None` when there is
    /// no active pane selection, when every covered block has gone
    /// away, or when the resulting payload is empty.
    pub fn pane_selection_text(&self) -> Option<String> {
        let sel = self.pane_selection.as_ref()?;
        if sel.is_empty() {
            return None;
        }
        let mut slices: Vec<crate::pane_selection::BlockSlice<'_>> = Vec::new();
        for block in self.blocks.iter() {
            if let Block::Sealed { id, command, snapshot, .. } = block {
                slices.push(crate::pane_selection::BlockSlice::new(*id, command, snapshot));
            }
        }
        let text = crate::pane_selection::pane_selection_text(&slices, sel);
        if text.is_empty() { None } else { Some(text) }
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
        RecallOutcome::Replace { new_text, new_cursor } => {
            editor.clear();
            editor.insert_str(&new_text);
            // Per spec/04 §"History walk (Up/Down)" caret-restore
            // rule: when returning to the in-progress buffer (the
            // head of the walk), restore the caret to its saved
            // position. For history entries, `new_cursor` is `None`
            // and the caret stays at end-of-text (the convention
            // every shell history walker uses).
            if let Some(c) = new_cursor {
                editor.set_cursor(c);
            }
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

    #[test]
    fn effective_var_names_prefers_shell_report_then_falls_back_to_snapshot() {
        let snapshot = vec!["HOME".to_string(), "PATH".to_string()];
        // No shell report yet → use the spawn snapshot.
        assert_eq!(effective_var_names(None, &snapshot), &snapshot[..]);
        // Shell has reported (its list supersedes — includes non-exported
        // params the snapshot can't have) → use it, even if shorter/empty.
        let shell = vec!["HISTFILE".to_string()];
        assert_eq!(effective_var_names(Some(&shell), &snapshot), &shell[..]);
        let empty: Vec<String> = vec![];
        assert_eq!(effective_var_names(Some(&empty), &snapshot), &empty[..]);
    }

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

    // ---- live-shell completion wiring (fish + zsh) -------------------

    #[test]
    fn fish_live_reply_correlates_by_id() {
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("sleep 0.1"), "t".into(), 0, None).expect("spawn");
        // Simulate an in-flight fish request with id 5.
        session.live_completion =
            Some(LiveCompletion { id: 5, sent_ms: 0, tool: DriverTool::FishComplete });

        // A reply for a DIFFERENT id (a superseded request) is dropped.
        session.ingest_live_completion(9, &["stale".to_string()]);
        assert!(session.live_completion_response.is_none(), "mismatched id is ignored");
        assert!(session.live_completion.is_some(), "in-flight request still pending");

        // The matching reply resolves and clears the in-flight marker. The
        // raw `complete -C` lines are parsed by the shared parser
        // (here fish's normal `value\tdescription` form, including a
        // runtime alias's description).
        session.ingest_live_completion(
            5,
            &["hello\talias hello=echo HI".to_string(), "help\tDisplay help".to_string()],
        );
        assert!(session.live_completion.is_none(), "in-flight cleared on a correlated reply");
        let resp = session.completion_driver_poll().expect("a response is available");
        assert_eq!(resp.tool, DriverTool::FishComplete);
        assert_eq!(resp.candidates.len(), 2);
        assert_eq!(resp.candidates[0].value, "hello");
        assert_eq!(resp.candidates[0].description.as_deref(), Some("alias hello=echo HI"));
        assert_eq!(
            resp.candidates[0].source,
            crate::completion::CompletionSource::Driver(DriverTool::FishComplete)
        );
        assert_eq!(resp.candidates[1].value, "help");
        // Polled once, then gone.
        assert!(session.completion_driver_poll().is_none());
    }

    #[test]
    fn zsh_live_reply_correlates_by_id_and_tags_zsh() {
        // The same live-completion plumbing serves a zsh pane: a reply
        // carrying the warm child's raw value-only lines is correlated by id
        // and the candidates are tagged `ZshComplete` (so the popup shows the
        // `zsh` source chip), NOT `FishComplete`.
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("sleep 0.1"), "t".into(), 0, None).expect("spawn");
        session.live_completion =
            Some(LiveCompletion { id: 2, sent_ms: 0, tool: DriverTool::ZshComplete });

        // Mismatched id dropped.
        session.ingest_live_completion(1, &["nope".to_string()]);
        assert!(session.live_completion.is_some(), "in-flight still pending after mismatch");

        // zsh v1 emits VALUES ONLY (no `\t` descriptions) — runtime alias
        // included.
        session.ingest_live_completion(
            2,
            &["greethere".to_string(), "grep".to_string(), "groups".to_string()],
        );
        assert!(session.live_completion.is_none());
        let resp = session.completion_driver_poll().expect("a response is available");
        assert_eq!(resp.tool, DriverTool::ZshComplete);
        assert_eq!(resp.candidates.len(), 3);
        assert_eq!(resp.candidates[0].value, "greethere", "runtime alias completes");
        assert_eq!(resp.candidates[0].description, None, "values-only in v1");
        assert_eq!(
            resp.candidates[0].source,
            crate::completion::CompletionSource::Driver(DriverTool::ZshComplete)
        );
    }

    #[test]
    fn live_completion_timeout_falls_back_to_empty_response_tagged_by_tool() {
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("sleep 0.1"), "t".into(), 0, None).expect("spawn");
        session.live_completion =
            Some(LiveCompletion { id: 1, sent_ms: 1_000, tool: DriverTool::ZshComplete });

        // Before the deadline: still in flight, no fallback.
        session.tick_live_completion_timeout(1_000 + LIVE_COMPLETION_TIMEOUT_MS - 1);
        assert!(session.live_completion.is_some(), "not yet timed out");
        assert!(session.live_completion_response.is_none());

        // At the deadline: drop the request and synthesize an empty result so
        // the popup resolves to its locals instead of spinning. The empty
        // fallback keeps the originating tool's tag.
        session.tick_live_completion_timeout(1_000 + LIVE_COMPLETION_TIMEOUT_MS);
        assert!(session.live_completion.is_none(), "timed-out request dropped");
        let resp = session.completion_driver_poll().expect("empty fallback response");
        assert!(resp.candidates.is_empty(), "fallback carries no candidates");
        assert_eq!(resp.tool, DriverTool::ZshComplete);
    }

    // ---- block stack wiring ------------------------------------------
    //
    // Phase 4A-data: spawning a pane must build a `BlockStack` and
    // every drained lifecycle event must reach it. Unit-level state
    // machine coverage lives in `src/block.rs`; these tests prove
    // the wiring at the `PaneSession::drain` boundary.

    #[test]
    fn restored_pane_is_dead_with_no_pty_and_seeded_cwd() {
        use crate::block::BlockStack;
        let session = PaneSession::restored(
            24,
            80,
            BlockStack::new(0),
            7,
            42,
            Some(std::path::PathBuf::from("/work/proj")),
        );
        // Dead, not exited (so the app won't auto-close it), no PTY.
        assert_eq!(session.controller.mode(), crate::shell::PaneMode::Dead);
        assert!(!session.is_exited(), "a restored pane must not auto-close");
        assert!(session.pty.is_none());
        // cwd seeded -> tab title will show the path, not `pane N`.
        assert_eq!(session.terminal().cwd(), Some(std::path::Path::new("/work/proj")));
        assert_eq!(session.persist_pane_row(), Some(42));
        // drain is inert (no panic, returns 0, stays not-exited).
        let mut session = session;
        assert_eq!(session.drain(), 0);
        assert!(!session.is_exited());
    }

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
    fn ctrl_c_aborts_pending_continuation_and_unlocks_the_pane() {
        // Repro for the `echo "!"` lock. zsh's history expansion turns
        // the double-quoted `!` into an unmatched quote, so the shell
        // emits PS2 (our `continuation` marker) and stays mid-line-read.
        // Termica restores the editor and re-promotes — but the shell is
        // NOT idle at a confirmed prompt, it's waiting to finish the
        // line, and a gate-swallowed Ctrl+C left no way out: every later
        // submit just fed more text into the dangling line. Ctrl+C while
        // a continuation is pending must abort it — SIGINT to unstick the
        // shell, plus a reset of the continuation state so the next
        // prompt starts clean.
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(&dcs_promote_to_editor_cmd()), "t".into(), 0, None)
                .expect("spawn");
        // Reach ShellPromptEditor.
        let stop = Instant::now() + Duration::from_secs(3);
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
        // Submit a command the shell deems incomplete, then inject the
        // continuation marker the shell's PS2 would emit (direct parser
        // feed — same path real shell-emitted bytes hit; see the sibling
        // continuation test for why we can't get `/bin/sh` to emit PS2).
        session.editor_mut().unwrap().insert_str("echo \"!\"");
        session.submit_editor_command().expect("submit");
        assert!(session.last_submitted.is_some());
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
                panic!("continuation never observed");
            }
            thread::sleep(Duration::from_millis(20));
        }
        // The pane is now in the continuation sub-state: editor active,
        // restored text, `last_submitted` still set.
        assert!(session.awaiting_continuation(), "should be awaiting continuation");
        assert_eq!(session.blocks.editor_on_tail().unwrap().text(), "echo \"!\"\n");

        // Ctrl+C aborts. State resets so the next submit is a clean
        // first-submit, not a stale-prefix diff.
        session.abort_continuation().expect("abort");
        assert!(!session.awaiting_continuation(), "continuation must be cleared");
        assert!(session.last_submitted.is_none(), "last_submitted must be cleared");
        assert!(
            session.blocks.editor_on_tail().unwrap().is_empty(),
            "editor must be cleared back to a fresh prompt"
        );

        // The `\x03` actually reached the PTY and interrupted the
        // foreground line: the shell child takes the SIGINT and exits.
        // This is what proves the abort unsticks the shell, not just our
        // local state.
        let stop = Instant::now() + Duration::from_secs(3);
        while !session.is_exited() && Instant::now() < stop {
            session.drain();
            thread::sleep(Duration::from_millis(20));
        }
        assert!(session.is_exited(), "SIGINT from abort should have ended the shell line");
    }

    #[test]
    fn retyping_a_different_command_during_continuation_heals_the_pane() {
        // 2-Enter recovery. After `echo "!"` leaves the shell stuck at
        // PS2, clearing the editor and submitting DIFFERENT text (text
        // that no longer EXTENDS the half-sent line) must NOT feed the
        // dangling line — that only re-locks the pane. Instead it aborts
        // the stuck line (SIGINT), resets the continuation state, and
        // KEEPS the retyped text in the editor so the next Enter — now at
        // a fresh prompt — runs it as a clean first submit.
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(&dcs_promote_to_editor_cmd()), "t".into(), 0, None)
                .expect("spawn");
        let stop = Instant::now() + Duration::from_secs(3);
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
        session.editor_mut().unwrap().insert_str("echo \"!\"");
        session.submit_editor_command().expect("submit");
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
                panic!("continuation never observed");
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(session.awaiting_continuation());

        // Abandon the half-sent line: clear and retype something else.
        let editor = session.editor_mut().unwrap();
        editor.clear();
        editor.insert_str("ls");
        session.submit_editor_command().expect("heal submit");

        // The continuation is aborted and reset; crucially the retyped
        // text is KEPT (not consumed) so the next Enter submits it, and
        // `last_submitted` is cleared so that next submit sends the full
        // `ls` rather than a stale-prefix diff.
        assert!(!session.awaiting_continuation(), "continuation must be reset");
        assert!(session.last_submitted.is_none(), "last_submitted must be cleared, not diffed");
        assert_eq!(
            session.blocks.editor_on_tail().unwrap().text(),
            "ls",
            "retyped text must be kept in the editor for the 2nd Enter"
        );
        assert_eq!(
            session.controller.mode(),
            PaneMode::ShellPromptEditor,
            "abort stays in the editor at the fresh prompt — no demote"
        );

        // The `\x03` reached the shell and aborted the foreground line.
        let stop = Instant::now() + Duration::from_secs(3);
        while !session.is_exited() && Instant::now() < stop {
            session.drain();
            thread::sleep(Duration::from_millis(20));
        }
        assert!(session.is_exited(), "SIGINT from the heal should have ended the shell line");
    }

    #[test]
    fn abort_continuation_is_a_noop_at_an_idle_prompt() {
        // No continuation pending (`last_submitted` is `None` at a fresh
        // prompt), so abort must NOT send a stray `\x03` or clear the
        // user's in-progress line — Ctrl+C stays inert at an idle prompt
        // per spec/04.
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(&dcs_promote_to_editor_cmd()), "t".into(), 0, None)
                .expect("spawn");
        let stop = Instant::now() + Duration::from_secs(3);
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
        session.editor_mut().unwrap().insert_str("echo keep me");
        assert!(!session.awaiting_continuation());
        session.abort_continuation().expect("abort");
        assert_eq!(
            session.blocks.editor_on_tail().unwrap().text(),
            "echo keep me",
            "abort at an idle prompt must not discard the typed line"
        );
        let _ = session.write(b"\x03"); // bring the shell down quickly
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
    fn precmd_after_unrecovered_alt_screen_clears_alt_screen_flag() {
        // Strict-layer regression: when a foreground program enters
        // the alt-screen (`\e[?1049h`) and exits WITHOUT emitting the
        // matching `\e[?1049l` (real-world: `less q` on macOS, vim
        // crash, ssh disconnect), the shell's next `precmd` marker
        // MUST be enough to drop the zombie alt-screen flag AND
        // promote back to the editor. Otherwise the editor footer
        // paints over a stale alt-grid and `Esc` unmasks it.
        //
        // The order of bytes from the shell, with a deliberate sleep
        // between the alt-screen entry and the second precmd so the
        // pane-mode machine has time to observe the alt-screen edge
        // (this is the realistic ordering: a frame tick happens
        // while less is running):
        //   1. integration_ready  →  RawTerminal
        //   2. precmd             →  ShellPromptEditor (first prompt)
        //   3. \e[?1049h          →  alt-screen flag goes ON; the
        //                            edge-detect in drain() then
        //                            transitions controller →
        //                            AlternateScreen (THIS is what
        //                            breaks naive Precmd-time
        //                            recovery: try_promote_to_editor
        //                            won't fire from AlternateScreen)
        //   4. sleep 0.2          →  give the polling loop multiple
        //                            drain cycles in alt-screen mode
        //   5. precmd AGAIN       →  must (a) clear the alt-screen
        //                            flag, (b) drop controller back
        //                            to RawTerminal, (c) re-promote
        //                            to ShellPromptEditor
        // Three printf bursts separated by short sleeps mirror the
        // real shell flow:
        //   burst 1: integration_ready + first precmd  (→ editor)
        //   burst 2: preexec + 1049h                   (→ alt-screen)
        //   burst 3: second precmd, NO matching 1049l  (→ zombie)
        // Without sleeps the bursts collapse into a single drain
        // and the alt-screen excursion is hidden by the recovery,
        // which doesn't reproduce the bug at all.
        // Three printf bursts separated by short sleeps mirror the
        // real shell flow:
        //   burst 1: integration_ready + first precmd  (→ editor)
        //   burst 2: preexec + 1049h                   (→ alt-screen)
        //   burst 3: command_finished + second precmd, NO 1049l
        //                                              (→ zombie)
        // Without sleeps the bursts collapse into a single drain
        // and the alt-screen excursion is hidden by the recovery,
        // which doesn't reproduce the bug at all.
        let cmd = "printf '\\033PTermica;{\"type\":\"integration_ready\",\
                   \"session\":\"t\",\"value\":{\"shell\":\"zsh\",\"version\":1}}\\033\\\\\
                   \\033PTermica;{\"type\":\"precmd\",\
                   \"session\":\"t\",\"value\":\"/tmp\"}\\033\\\\'; \
                   sleep 0.2; \
                   printf '\\033PTermica;{\"type\":\"preexec\",\
                   \"session\":\"t\",\"value\":\"less foo\"}\\033\\\\\
                   \\033[?1049h'; \
                   sleep 0.2; \
                   printf '\\033PTermica;{\"type\":\"command_finished\",\
                   \"session\":\"t\",\"value\":0}\\033\\\\\
                   \\033PTermica;{\"type\":\"precmd\",\
                   \"session\":\"t\",\"value\":\"/tmp\"}\\033\\\\'; \
                   sleep 5";
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(cmd), "t".into(), 0, None).expect("spawn");

        // Confirm the realistic intermediate state: the controller
        // entered AlternateScreen mode after 1049h. If we ever skip
        // this state in the test (because the second precmd arrived
        // in the same drain as 1049h), the test isn't exercising the
        // bug.
        let intermediate_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            session.drain();
            if session.controller.mode() == PaneMode::AlternateScreen {
                break;
            }
            if Instant::now() >= intermediate_deadline {
                panic!(
                    "test scaffolding: never reached AlternateScreen mode after 1049h; \
                     got mode={:?}",
                    session.controller.mode(),
                );
            }
            thread::sleep(Duration::from_millis(10));
        }

        // Now spin drain() until the second precmd has been observed
        // AND we are back to the editor with the alt-screen flag
        // cleared.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            session.drain();
            if session.editor_is_active() && !session.terminal.is_alternate_screen() {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "after two precmds with a hung 1049h in between, expected \
                     editor_is_active=true and alt_screen=false; got \
                     editor_is_active={}, alt_screen={}, mode={:?}",
                    session.editor_is_active(),
                    session.terminal.is_alternate_screen(),
                    session.controller.mode(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
        // Belt + braces — the view snapshot the renderer consumes
        // must also report alt_screen=false (it reads
        // is_alternate_screen() inline, so this is a sanity check on
        // the snapshot path).
        assert!(
            !session.view().alt_screen,
            "view.alt_screen should be false after Precmd recovery"
        );
    }

    #[test]
    fn precmd_in_same_burst_as_1049l_still_promotes_to_editor() {
        // Companion to the no-1049l zombie test, gating the
        // intermittent variant the user reported: even when the
        // foreground program DID emit `\e[?1049l` cleanly, if those
        // bytes arrive in the same PTY chunk as `command_finished
        // + precmd + PS1`, alacritty's alt-screen flag goes off
        // mid-feed BUT the controller is still in
        // `AlternateScreen` mode when the Precmd event runs
        // `try_promote_to_editor` (the post-loop edge-detect
        // hasn't run yet). Promotion was refused; editor never
        // came back. The fix is to sync the controller's alt-
        // screen view BEFORE the event loop so Precmd promotion
        // sees the up-to-date mode.
        //
        // Burst sequence:
        //   burst 1: integration_ready + first precmd  (→ editor)
        //   burst 2: preexec + 1049h                   (→ alt-screen)
        //   burst 3: 1049l + command_finished + precmd (→ should
        //                                               restore
        //                                               editor)
        let cmd = "printf '\\033PTermica;{\"type\":\"integration_ready\",\
                   \"session\":\"t\",\"value\":{\"shell\":\"zsh\",\"version\":1}}\\033\\\\\
                   \\033PTermica;{\"type\":\"precmd\",\
                   \"session\":\"t\",\"value\":\"/tmp\"}\\033\\\\'; \
                   sleep 0.2; \
                   printf '\\033PTermica;{\"type\":\"preexec\",\
                   \"session\":\"t\",\"value\":\"less foo\"}\\033\\\\\
                   \\033[?1049h'; \
                   sleep 0.2; \
                   printf '\\033[?1049l\
                   \\033PTermica;{\"type\":\"command_finished\",\
                   \"session\":\"t\",\"value\":0}\\033\\\\\
                   \\033PTermica;{\"type\":\"precmd\",\
                   \"session\":\"t\",\"value\":\"/tmp\"}\\033\\\\'; \
                   sleep 5";
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(cmd), "t".into(), 0, None).expect("spawn");

        // Confirm we entered AlternateScreen mode (mirror the
        // other zombie test's scaffolding so a chunking surprise
        // can't silently degrade coverage).
        let intermediate_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            session.drain();
            if session.controller.mode() == PaneMode::AlternateScreen {
                break;
            }
            if Instant::now() >= intermediate_deadline {
                panic!(
                    "test scaffolding: never reached AlternateScreen mode after 1049h; \
                     got mode={:?}",
                    session.controller.mode(),
                );
            }
            thread::sleep(Duration::from_millis(10));
        }

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            session.drain();
            if session.editor_is_active() && !session.terminal.is_alternate_screen() {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "after a clean 1049l burst with command_finished+precmd in the \
                     same chunk, expected editor_is_active=true and alt_screen=false; \
                     got editor_is_active={}, alt_screen={}, mode={:?}",
                    session.editor_is_active(),
                    session.terminal.is_alternate_screen(),
                    session.controller.mode(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn precmd_clears_last_submitted_as_backstop_for_missed_preexec() {
        // Bug-fix regression: `last_submitted` was only cleared on
        // `Preexec`. If Preexec was missed (shell hook race under
        // load, dropped marker, integration script edge case), the
        // stale prefix would make the NEXT submit run the diff
        // path (`text.starts_with(prev)`) and silently send only
        // the delta. For an identical re-submit the delta is
        // empty — the user types a command, hits Enter, the
        // editor clears, but the shell receives just `\r`. The
        // command appears to vanish: no Preexec, no Running
        // block, no history entry, nothing.
        //
        // Fix: also clear `last_submitted` on `Precmd`. Precmd
        // fires before each PS1 cycle and never for PS2
        // continuation prompts, so the legitimate continuation
        // path (between Submit and the Continuation event) is
        // unaffected — Precmd does not fire there.
        //
        // Burst sequence the test drives:
        //   1. integration_ready + first precmd → editor.
        //   2. (no Preexec emitted — simulating the race.)
        //   3. second precmd → must clear `last_submitted` even
        //      though Preexec never arrived.
        let cmd = "printf '\\033PTermica;{\"type\":\"integration_ready\",\
                   \"session\":\"t\",\"value\":{\"shell\":\"zsh\",\"version\":1}}\\033\\\\\
                   \\033PTermica;{\"type\":\"precmd\",\
                   \"session\":\"t\",\"value\":\"/tmp\"}\\033\\\\'; \
                   sleep 0.2; \
                   printf '\\033PTermica;{\"type\":\"precmd\",\
                   \"session\":\"t\",\"value\":\"/tmp\"}\\033\\\\'; \
                   sleep 5";
        let mut session =
            PaneSession::spawn(5, 40, &sh_c(cmd), "t".into(), 0, None).expect("spawn");

        // 1) Wait until the FIRST precmd promotes us to editor.
        let stop = Instant::now() + Duration::from_secs(2);
        loop {
            session.drain();
            if session.editor_is_active() {
                break;
            }
            if Instant::now() >= stop {
                panic!("editor never active");
            }
            thread::sleep(Duration::from_millis(10));
        }

        // 2) Simulate the user submitting a command. After this
        // call `last_submitted` should hold the text.
        session.editor_mut().unwrap().insert_str("echo a");
        session.submit_editor_command().expect("submit");
        assert_eq!(
            session.last_submitted.as_deref(),
            Some("echo a"),
            "test scaffolding: submit must set last_submitted"
        );

        // 3) Wait for the SECOND precmd to arrive. NO Preexec
        // ever fires (the shell command in this test doesn't
        // emit one). The backstop clear on Precmd must zero
        // `last_submitted`.
        let stop = Instant::now() + Duration::from_secs(3);
        loop {
            session.drain();
            if session.last_submitted.is_none() {
                break;
            }
            if Instant::now() >= stop {
                panic!(
                    "after second precmd with no Preexec in between, expected \
                     last_submitted=None; still set to {:?} — Precmd backstop \
                     is missing or broken",
                    session.last_submitted
                );
            }
            thread::sleep(Duration::from_millis(20));
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

    fn dummy_pane_sel() -> crate::pane_selection::PaneSelection {
        use crate::pane_selection::{PaneCursor, PaneSelection};
        PaneSelection::new(
            PaneCursor::new(crate::block::BlockId(99), 0, 0),
            PaneCursor::new(crate::block::BlockId(99), 0, 5),
        )
    }

    fn dummy_pane_sel_cross_block() -> crate::pane_selection::PaneSelection {
        // Anchor in BlockId(50), head in BlockId(99) — a cross-block
        // selection.
        use crate::pane_selection::{PaneCursor, PaneSelection};
        PaneSelection::new(
            PaneCursor::new(crate::block::BlockId(50), 1, 2),
            PaneCursor::new(crate::block::BlockId(99), 3, 4),
        )
    }

    #[test]
    fn set_pane_selection_clears_live_grid_selection() {
        let mut session = quiet_session();
        session.start_selection(
            alacritty_terminal::index::Point {
                line: alacritty_terminal::index::Line(0),
                column: alacritty_terminal::index::Column(0),
            },
            crate::selection::SelectionMode::Char,
        );
        assert!(session.selection().is_some(), "precondition: live grid selection set");

        session.set_pane_selection(dummy_pane_sel());

        assert!(session.selection().is_none(), "live-grid selection should be cleared");
        assert!(session.pane_selection().is_some(), "pane selection should be set");
    }

    #[test]
    fn start_selection_clears_pane_selection() {
        let mut session = quiet_session();
        session.set_pane_selection(dummy_pane_sel());
        assert!(session.pane_selection().is_some(), "precondition");

        session.start_selection(
            alacritty_terminal::index::Point {
                line: alacritty_terminal::index::Line(0),
                column: alacritty_terminal::index::Column(0),
            },
            crate::selection::SelectionMode::Char,
        );

        assert!(session.pane_selection().is_none(), "pane selection should be cleared");
        assert!(session.selection().is_some(), "live-grid selection should be set");
    }

    #[test]
    fn pane_selection_text_returns_none_when_blocks_do_not_exist() {
        let mut session = quiet_session();
        // dummy_pane_sel points at BlockId(99) — the fresh session
        // only has a Prompt block at id 0, so the lookup misses.
        session.set_pane_selection(dummy_pane_sel());
        assert_eq!(session.pane_selection_text(), None);
    }

    #[test]
    fn update_pane_selection_head_preserves_anchor() {
        use crate::pane_selection::PaneCursor;
        let mut session = quiet_session();
        session.set_pane_selection(dummy_pane_sel());
        let original_anchor = session.pane_selection().unwrap().anchor;

        session.update_pane_selection_head(PaneCursor::new(crate::block::BlockId(99), 4, 7));

        let sel = session.pane_selection().expect("still set");
        assert_eq!(sel.anchor, original_anchor, "anchor stays pinned");
        assert_eq!(sel.head, PaneCursor::new(crate::block::BlockId(99), 4, 7));
    }

    #[test]
    fn update_pane_selection_endpoints_can_cross_blocks() {
        // After a multi-click rolling word/line union: both anchor and
        // head shift, possibly into the same or another block.
        use crate::pane_selection::PaneCursor;
        let mut session = quiet_session();
        session.set_pane_selection(dummy_pane_sel());
        session.update_pane_selection_endpoints(
            PaneCursor::new(crate::block::BlockId(99), 0, 0),
            PaneCursor::new(crate::block::BlockId(99), 5, 10),
        );
        let sel = session.pane_selection().expect("still set");
        assert_eq!(sel.anchor.col, 0);
        assert_eq!(sel.head.col, 10);
    }

    #[test]
    fn pane_selection_accepts_cross_block_anchor_and_head() {
        // Spec/04 §Cross-block: a single selection's anchor and head
        // may live in different blocks. The setter must accept this.
        let mut session = quiet_session();
        session.set_pane_selection(dummy_pane_sel_cross_block());
        let sel = session.pane_selection().expect("set");
        assert_eq!(sel.anchor.block_id, crate::block::BlockId(50));
        assert_eq!(sel.head.block_id, crate::block::BlockId(99));
        assert!(!sel.is_within_one_block());
    }

    #[test]
    fn clear_pane_selection_leaves_live_grid_alone() {
        let mut session = quiet_session();
        session.start_selection(
            alacritty_terminal::index::Point {
                line: alacritty_terminal::index::Line(0),
                column: alacritty_terminal::index::Column(0),
            },
            crate::selection::SelectionMode::Char,
        );
        session.clear_pane_selection();
        assert!(session.selection().is_some(), "live-grid selection untouched");
    }
}
