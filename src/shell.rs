//! `PromptController` — the pane-mode state machine.
//!
//! This is the load-bearing safety invariant of Termica
//! ([spec/05](../spec/05-pane-modes.md)). Every transition is
//! covered by a strict tests-first test in this module; a code
//! change that violates one of the five safety rules is a P0 bug.
//!
//! ## The five rules (from spec/05, restated)
//!
//! 1. The prompt editor is **NEVER active** unless the pane is at a
//!    marker-confirmed shell prompt.
//! 2. **Alternate screen always disables the prompt editor.** No
//!    exceptions.
//! 3. **Shell integration markers are authoritative.** Heuristics
//!    may enhance but must not be required for correctness.
//! 4. The shell never sees editing keystrokes from the editor.
//!    (Phase 4 territory; this module guarantees the *mode* is
//!    correct.)
//! 5. Real TTY programs receive raw input. (Already true in
//!    Phase 1; this module preserves it by defaulting to
//!    `RawTerminal`.)
//!
//! ## Promotion gated, demotion eager
//!
//! Promotion → `ShellPromptEditor` requires:
//! - The pane is in `RawTerminal` (never `AlternateScreen` /
//!   `Dead`).
//! - The integration version handshake has confirmed a supported
//!   version.
//! - The most recent transition *out* of `ShellPromptEditor` was
//!   not on the current frame (debounce against the same prompt
//!   round-tripping).
//!
//! Demotion ← `ShellPromptEditor` happens eagerly via any of:
//! - [`PromptController::submit_command`] (the Enter path)
//! - [`PromptController::leave_editor_ctrl_c`]
//! - [`PromptController::leave_editor_esc`]
//! - alt-screen toggle (handled by `observe_alt_screen`)
//! - PTY exit
//! - integration-version becoming unsupported
//!
//! The frame counter is a caller-supplied `u64` (typically the
//! eframe frame index ticked in `update()`). Strict tests use
//! constants — never `Instant::now()` — per CLAUDE.md's
//! "no live timestamps in tests" rule.

#![forbid(unsafe_code)]

use crate::markers::{MarkerEvent, ShellKind};

/// Protocol version this build of Termica understands. The
/// integration script emits its own version via
/// `OSC 1337 ; TermicaVersion=N`; we accept `N == SUPPORTED_…` and
/// flag any other value as unsupported.
pub const SUPPORTED_PROTOCOL_VERSION: u32 = 1;

/// The four exhaustive modes a pane can be in. Spec/05 calls them
/// out as exactly four; we won't add a fifth without an explicit
/// spec update, because new sub-states tend to be the answer to
/// bug pressure that should have been fixed structurally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneMode {
    /// Default. Keystrokes go to the PTY; output renders normally;
    /// no editor surface; mouse selects the transcript.
    RawTerminal,
    /// The pane is at a marker-confirmed shell prompt. The egui
    /// editor owns input; only `submit_command` produces PTY bytes.
    /// Phase 4 wires the actual editor; Phase 3B just guarantees
    /// the *mode* is correct.
    ShellPromptEditor,
    /// Terminal has entered the alternate screen (vim / htop /
    /// less / fzf / tmux). Strictest raw mode.
    AlternateScreen,
    /// PTY child has exited. Transcript remains; "restart shell"
    /// UI is visible; history and search remain accessible.
    Dead,
}

/// How far along the integration version handshake is for this
/// pane. Updated by `MarkerEvent::ProtocolVersion` observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationState {
    /// No `ProtocolVersion` marker has arrived yet. The shell may
    /// not have installed Termica's integration script. The
    /// editor is unavailable in this state.
    Unknown,
    /// The shell announced a version we support. Editor promotion
    /// is gated on this.
    Confirmed(u32),
    /// The shell announced a version we don't support. We refuse
    /// to promote and surface a banner ("upgrade Termica or
    /// `termica install-integration`"). We never recover from
    /// this within a single shell run; a `restart_shell` resets.
    Unsupported,
}

/// Why a mode transition occurred. Recorded in
/// [`TransitionRecord`] so an after-the-fact debug session
/// (`--dump-events` in Phase 3F, or `tracing` logs) can explain
/// why the pane ended up where it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionReason {
    InitialSpawn,
    MarkerPromptEnd,
    EnterSubmitted,
    CtrlCEmptyEditor,
    AlternateScreenEnter,
    AlternateScreenExit,
    PtyExit,
    UserRestartedShell,
    IntegrationVersionUnsupported,
    Esc,
}

/// A single transition. The controller keeps only the most recent
/// one (callers can stream them out for `--dump-events`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionRecord {
    pub from: PaneMode,
    pub to: PaneMode,
    pub reason: TransitionReason,
    /// Frame counter at which the transition occurred. Caller
    /// supplies it; we never call `Instant::now()`.
    pub at: u64,
}

/// One command attempt. Opened either by `submit_command` (Enter
/// in the editor — the strong case) or by a `CommandStart`
/// marker arriving without a prior submit. Closed by `CommandEnd`,
/// `pty_exit`, or never (the open-ended case).
///
/// Phase 7 will enrich this into the spec/07 `CommandRun`
/// (output_range, context_snapshot, etc). For 3B we keep it lean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCommand {
    pub started_frame: u64,
    /// `Some(frame)` once the command has been closed (by
    /// `CommandEnd` or `pty_exit`); `None` while still in flight.
    pub ended_frame: Option<u64>,
    /// Exit code from `CommandEnd`, or `None` if the shell didn't
    /// emit one or the pane died first.
    pub exit: Option<i32>,
    /// Optional duration from the `CommandEnd` extension. The
    /// integration scripts in spec/03 don't currently emit it.
    pub duration_ms: Option<u64>,
}

/// The pane-mode state machine.
///
/// All inputs flow through the `observe_*` methods (marker events,
/// alt-screen toggles, PTY exit) or the gesture methods
/// (`submit_command`, `leave_editor_*`, `restart_shell`). The
/// machine never reaches out for state; callers tell it what
/// happened.
pub struct PromptController {
    mode: PaneMode,
    integration: IntegrationState,
    last_transition: TransitionRecord,
    pending_cmd: Option<PendingCommand>,
    last_completed_cmd: Option<PendingCommand>,
    /// Most recent shell kind announced by `TermicaShell=…`.
    /// Informational only — no transition depends on it.
    shell_kind: Option<ShellKind>,
}

impl PromptController {
    /// Construct a fresh controller. Starts in `RawTerminal` with
    /// `Unknown` integration. The initial `TransitionRecord` is
    /// `InitialSpawn` so downstream consumers (--dump-events) have
    /// a well-formed first entry instead of a sentinel.
    pub fn new(frame: u64) -> Self {
        Self {
            mode: PaneMode::RawTerminal,
            integration: IntegrationState::Unknown,
            last_transition: TransitionRecord {
                from: PaneMode::RawTerminal,
                to: PaneMode::RawTerminal,
                reason: TransitionReason::InitialSpawn,
                at: frame,
            },
            pending_cmd: None,
            last_completed_cmd: None,
            shell_kind: None,
        }
    }

    // ---- observe_* (caller pushes external events in) ---------------

    pub fn observe_marker(&mut self, event: MarkerEvent, frame: u64) {
        match event {
            MarkerEvent::PromptStart => {
                // PromptStart is informational. The actual gate is
                // PromptEnd. We deliberately don't promote here —
                // a partial prompt sequence (PromptStart with no
                // PromptEnd to follow because the shell crashed)
                // must NEVER leave us in ShellPromptEditor.
            }
            MarkerEvent::PromptEnd => {
                self.try_promote_to_editor(frame);
            }
            MarkerEvent::CommandStart => {
                // Opens a pending command if none is in flight.
                // (If `submit_command` already opened one at Enter,
                // this is the confirming marker — leave the
                // existing pending alone.)
                if self.pending_cmd.is_none() && self.mode != PaneMode::Dead {
                    self.pending_cmd = Some(PendingCommand {
                        started_frame: frame,
                        ended_frame: None,
                        exit: None,
                        duration_ms: None,
                    });
                }
            }
            MarkerEvent::CommandEnd { exit, duration_ms } => {
                self.close_pending_cmd(frame, exit, duration_ms);
            }
            MarkerEvent::ProtocolVersion(v) => {
                if v == SUPPORTED_PROTOCOL_VERSION {
                    self.integration = IntegrationState::Confirmed(v);
                } else {
                    self.integration = IntegrationState::Unsupported;
                    // If we're somehow already in editor mode (shouldn't
                    // be: promotion is gated on Confirmed), demote.
                    if self.mode == PaneMode::ShellPromptEditor {
                        self.transition(
                            PaneMode::RawTerminal,
                            TransitionReason::IntegrationVersionUnsupported,
                            frame,
                        );
                    }
                }
            }
            MarkerEvent::Shell(kind) => {
                self.shell_kind = Some(kind);
            }
            MarkerEvent::Cwd(_) => {
                // We don't track cwd in the controller — the
                // OscSniffer state owns that snapshot. Phase 7's
                // CommandRun will record cwd at command-start
                // time by reading from the sniffer.
            }
        }
    }

    pub fn observe_alt_screen(&mut self, on: bool, frame: u64) {
        match (on, self.mode) {
            (true, m) if m != PaneMode::AlternateScreen && m != PaneMode::Dead => {
                self.transition(
                    PaneMode::AlternateScreen,
                    TransitionReason::AlternateScreenEnter,
                    frame,
                );
            }
            (false, PaneMode::AlternateScreen) => {
                // Spec: never directly to ShellPromptEditor; only a
                // fresh marker promotes.
                self.transition(
                    PaneMode::RawTerminal,
                    TransitionReason::AlternateScreenExit,
                    frame,
                );
            }
            _ => {}
        }
    }

    pub fn observe_pty_exit(&mut self, frame: u64) {
        if self.mode == PaneMode::Dead {
            return;
        }
        // Close any in-flight command without an exit code — the
        // shell didn't get to report.
        if self.pending_cmd.is_some() {
            self.close_pending_cmd(frame, None, None);
        }
        self.transition(PaneMode::Dead, TransitionReason::PtyExit, frame);
    }

    // ---- gestures (caller drives user-initiated transitions) --------

    /// Enter was pressed in the editor: demote *eagerly*, before
    /// any PTY write. Per spec/05 §"Sequencing around submit()",
    /// this must run before the bytes go out so a same-frame
    /// Ctrl-C lands in the PTY, not in a closed editor.
    pub fn submit_command(&mut self, frame: u64) {
        if self.mode != PaneMode::ShellPromptEditor {
            return;
        }
        self.transition(PaneMode::RawTerminal, TransitionReason::EnterSubmitted, frame);
        // Open the pending command speculatively. CommandStart
        // marker (if it arrives) is treated as confirming, not
        // triggering; CommandEnd closes the loop. If no markers
        // arrive (shell crashed), the open-ended pending stays
        // until pty_exit closes it with exit=None.
        if self.pending_cmd.is_none() {
            self.pending_cmd = Some(PendingCommand {
                started_frame: frame,
                ended_frame: None,
                exit: None,
                duration_ms: None,
            });
        }
    }

    pub fn leave_editor_ctrl_c(&mut self, frame: u64) {
        if self.mode == PaneMode::ShellPromptEditor {
            self.transition(PaneMode::RawTerminal, TransitionReason::CtrlCEmptyEditor, frame);
        }
    }

    pub fn leave_editor_esc(&mut self, frame: u64) {
        if self.mode == PaneMode::ShellPromptEditor {
            self.transition(PaneMode::RawTerminal, TransitionReason::Esc, frame);
        }
    }

    pub fn restart_shell(&mut self, frame: u64) {
        if self.mode != PaneMode::Dead {
            return;
        }
        // Reset integration state — a new shell process will
        // re-do the version handshake.
        self.integration = IntegrationState::Unknown;
        self.pending_cmd = None;
        self.last_completed_cmd = None;
        self.shell_kind = None;
        self.transition(PaneMode::RawTerminal, TransitionReason::UserRestartedShell, frame);
    }

    // ---- read accessors ---------------------------------------------

    pub fn mode(&self) -> PaneMode {
        self.mode
    }
    pub fn integration(&self) -> IntegrationState {
        self.integration
    }
    pub fn last_transition(&self) -> &TransitionRecord {
        &self.last_transition
    }
    pub fn pending_cmd(&self) -> Option<&PendingCommand> {
        self.pending_cmd.as_ref()
    }
    pub fn last_completed_cmd(&self) -> Option<&PendingCommand> {
        self.last_completed_cmd.as_ref()
    }
    pub fn shell_kind(&self) -> Option<ShellKind> {
        self.shell_kind
    }

    // ---- internals --------------------------------------------------

    fn try_promote_to_editor(&mut self, frame: u64) {
        // Rule 1: only promote from RawTerminal. AlternateScreen +
        // Dead must never promote.
        if self.mode != PaneMode::RawTerminal {
            return;
        }
        // Rule (promotion-gating): integration handshake must be
        // confirmed at a supported version.
        if !matches!(self.integration, IntegrationState::Confirmed(_)) {
            return;
        }
        // Frame debounce: if we just left ShellPromptEditor on
        // this very frame, don't re-enter immediately. Guards
        // against same-frame round-tripping.
        if self.last_transition.from == PaneMode::ShellPromptEditor
            && self.last_transition.at == frame
        {
            return;
        }
        self.transition(PaneMode::ShellPromptEditor, TransitionReason::MarkerPromptEnd, frame);
    }

    fn close_pending_cmd(&mut self, frame: u64, exit: Option<i32>, duration_ms: Option<u64>) {
        if let Some(mut p) = self.pending_cmd.take() {
            p.ended_frame = Some(frame);
            p.exit = exit;
            p.duration_ms = duration_ms;
            self.last_completed_cmd = Some(p);
        }
        // Orphan CommandEnd with no pending: ignored. The shell
        // may have emitted CommandEnd from outside our tracking
        // (rare).
    }

    fn transition(&mut self, to: PaneMode, reason: TransitionReason, at: u64) {
        let from = self.mode;
        self.mode = to;
        self.last_transition = TransitionRecord { from, to, reason, at };
    }
}

#[cfg(test)]
mod tests {
    //! Strict-layer tests per CLAUDE.md. These cover every
    //! transition in the state machine and the five safety rules
    //! from spec/05. Each test passes on the implementation and
    //! would have failed on the pre-change tree (since this
    //! module didn't exist).
    //!
    //! No `Instant::now()`, no live timestamps — frame counters
    //! are passed in as constants.

    use super::*;
    use std::path::PathBuf;

    fn confirmed() -> PromptController {
        // Common bootstrap: a controller whose integration has
        // already been confirmed at the supported version. Used by
        // most promotion-path tests.
        let mut c = PromptController::new(0);
        c.observe_marker(MarkerEvent::ProtocolVersion(SUPPORTED_PROTOCOL_VERSION), 0);
        c
    }

    // ---- bootstrap --------------------------------------------------

    #[test]
    fn fresh_controller_starts_in_raw_terminal() {
        let c = PromptController::new(0);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.integration(), IntegrationState::Unknown);
        assert!(c.pending_cmd().is_none());
        assert!(c.last_completed_cmd().is_none());
    }

    #[test]
    fn fresh_controllers_initial_transition_is_initialspawn() {
        let c = PromptController::new(42);
        let t = c.last_transition();
        assert_eq!(t.reason, TransitionReason::InitialSpawn);
        assert_eq!(t.from, PaneMode::RawTerminal);
        assert_eq!(t.to, PaneMode::RawTerminal);
        assert_eq!(t.at, 42);
    }

    // ---- safety rule 1: editor never active without marker ----------

    #[test]
    fn promptend_without_integration_does_not_promote() {
        // Canonical test from spec/05:
        // `prompt_editor_unavailable_without_integration`.
        let mut c = PromptController::new(0);
        // Many PromptEnds, no ProtocolVersion ever: stays Raw.
        for f in 1..=10 {
            c.observe_marker(MarkerEvent::PromptEnd, f);
            assert_eq!(c.mode(), PaneMode::RawTerminal);
        }
    }

    #[test]
    fn promptend_with_confirmed_integration_promotes() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        assert_eq!(c.mode(), PaneMode::ShellPromptEditor);
        assert_eq!(c.last_transition().reason, TransitionReason::MarkerPromptEnd);
    }

    #[test]
    fn cwd_event_alone_does_not_promote() {
        // Pure Cwd/Shell/CommandStart events must not trip the
        // promotion gate — only PromptEnd does.
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::Cwd(PathBuf::from("/tmp")), 1);
        c.observe_marker(MarkerEvent::Shell(ShellKind::Zsh), 1);
        c.observe_marker(MarkerEvent::CommandStart, 1);
        c.observe_marker(MarkerEvent::CommandEnd { exit: Some(0), duration_ms: None }, 1);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    // ---- safety rule 2: alt-screen disables editor ------------------

    #[test]
    fn alt_screen_enter_from_raw_transitions_to_alt() {
        let mut c = PromptController::new(0);
        c.observe_alt_screen(true, 1);
        assert_eq!(c.mode(), PaneMode::AlternateScreen);
        assert_eq!(c.last_transition().reason, TransitionReason::AlternateScreenEnter);
    }

    #[test]
    fn alt_screen_enter_from_editor_demotes_to_alt() {
        // Canonical test: spec/05 `alt_screen_forces_raw` —
        // alt-screen ON while in ShellPromptEditor must NOT leave
        // us in the editor.
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        assert_eq!(c.mode(), PaneMode::ShellPromptEditor);
        c.observe_alt_screen(true, 2);
        assert_eq!(c.mode(), PaneMode::AlternateScreen);
    }

    #[test]
    fn promptend_in_alt_screen_does_not_promote() {
        // The headline safety guarantee: even with confirmed
        // integration, a PromptEnd arriving while we're in
        // AlternateScreen does NOT escape to ShellPromptEditor.
        let mut c = confirmed();
        c.observe_alt_screen(true, 1);
        assert_eq!(c.mode(), PaneMode::AlternateScreen);
        c.observe_marker(MarkerEvent::PromptEnd, 2);
        assert_eq!(c.mode(), PaneMode::AlternateScreen);
    }

    #[test]
    fn alt_screen_exit_returns_to_raw_not_editor() {
        // Spec/05: "Alternate-screen OFF → RawTerminal (not
        // directly to ShellPromptEditor; only a fresh marker
        // promotes)."
        let mut c = confirmed();
        c.observe_alt_screen(true, 1);
        c.observe_alt_screen(false, 2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.last_transition().reason, TransitionReason::AlternateScreenExit);
    }

    // ---- safety rule 3: markers authoritative (no heuristics) -------

    #[test]
    fn no_observation_other_than_promptend_promotes() {
        // Drive every non-PromptEnd marker variant + alt-screen
        // toggles + observe_pty_exit-paths; none should promote
        // to ShellPromptEditor (the only thing that can is a
        // PromptEnd while confirmed in RawTerminal).
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptStart, 1);
        c.observe_marker(MarkerEvent::CommandStart, 1);
        c.observe_marker(MarkerEvent::CommandEnd { exit: Some(0), duration_ms: None }, 1);
        c.observe_marker(MarkerEvent::Cwd(PathBuf::from("/tmp")), 1);
        c.observe_marker(MarkerEvent::Shell(ShellKind::Bash), 1);
        c.observe_marker(MarkerEvent::ProtocolVersion(SUPPORTED_PROTOCOL_VERSION), 1);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    // ---- integration version handshake ------------------------------

    #[test]
    fn supported_version_marks_integration_confirmed() {
        let mut c = PromptController::new(0);
        c.observe_marker(MarkerEvent::ProtocolVersion(SUPPORTED_PROTOCOL_VERSION), 1);
        assert_eq!(c.integration(), IntegrationState::Confirmed(SUPPORTED_PROTOCOL_VERSION));
    }

    #[test]
    fn unsupported_version_marks_integration_unsupported() {
        let mut c = PromptController::new(0);
        c.observe_marker(MarkerEvent::ProtocolVersion(SUPPORTED_PROTOCOL_VERSION + 1), 1);
        assert_eq!(c.integration(), IntegrationState::Unsupported);
    }

    #[test]
    fn unsupported_version_disables_promotion() {
        let mut c = PromptController::new(0);
        c.observe_marker(MarkerEvent::ProtocolVersion(SUPPORTED_PROTOCOL_VERSION + 1), 1);
        c.observe_marker(MarkerEvent::PromptEnd, 2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    #[test]
    fn unsupported_version_arriving_in_editor_demotes() {
        // Pathological case: we're somehow in editor mode and the
        // shell announces an unsupported version (e.g. user upgrades
        // their integration script mid-session to a newer one we
        // don't speak). We must demote.
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        assert_eq!(c.mode(), PaneMode::ShellPromptEditor);
        c.observe_marker(MarkerEvent::ProtocolVersion(999), 2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.last_transition().reason, TransitionReason::IntegrationVersionUnsupported);
    }

    // ---- frame debounce ---------------------------------------------

    #[test]
    fn promotion_debounces_within_one_frame_of_demotion() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        assert_eq!(c.mode(), PaneMode::ShellPromptEditor);
        // Submit demotes at frame 2.
        c.submit_command(2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        // A same-frame PromptEnd must NOT re-promote.
        c.observe_marker(MarkerEvent::PromptEnd, 2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    #[test]
    fn promotion_allowed_next_frame_after_demotion() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        c.submit_command(2);
        // Next frame: a fresh PromptEnd may promote.
        c.observe_marker(MarkerEvent::PromptEnd, 3);
        assert_eq!(c.mode(), PaneMode::ShellPromptEditor);
    }

    // ---- submit / leave gestures ------------------------------------

    #[test]
    fn submit_command_demotes_eagerly_and_opens_pending() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        c.submit_command(2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.last_transition().reason, TransitionReason::EnterSubmitted);
        let p = c.pending_cmd().expect("pending command should be open");
        assert_eq!(p.started_frame, 2);
        assert!(p.ended_frame.is_none());
    }

    #[test]
    fn submit_command_outside_editor_is_noop() {
        let mut c = PromptController::new(0);
        c.submit_command(1);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert!(c.pending_cmd().is_none());
    }

    #[test]
    fn leave_editor_esc_demotes_with_esc_reason() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        c.leave_editor_esc(2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.last_transition().reason, TransitionReason::Esc);
        // Esc-leave must NOT open a pending command (nothing was
        // submitted).
        assert!(c.pending_cmd().is_none());
    }

    #[test]
    fn leave_editor_ctrlc_demotes_without_pending() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        c.leave_editor_ctrl_c(2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.last_transition().reason, TransitionReason::CtrlCEmptyEditor);
        assert!(c.pending_cmd().is_none());
    }

    // ---- command lifecycle ------------------------------------------

    #[test]
    fn commandstart_marker_opens_pending_when_none() {
        // The "raw observed" case: user typed at a prompt without
        // the editor (e.g. integration installed but mode happened
        // to be RawTerminal at that instant). The CommandStart
        // marker opens a pending command.
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::CommandStart, 1);
        let p = c.pending_cmd().expect("pending should be open");
        assert_eq!(p.started_frame, 1);
    }

    #[test]
    fn commandstart_after_submit_does_not_overwrite_pending() {
        // submit_command opens the pending command speculatively
        // at frame 2. A later CommandStart marker (frame 3) is the
        // confirming event — must leave the existing pending alone.
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        c.submit_command(2);
        c.observe_marker(MarkerEvent::CommandStart, 3);
        let p = c.pending_cmd().expect("pending should still be open");
        assert_eq!(p.started_frame, 2); // not 3
    }

    #[test]
    fn commandend_closes_pending_with_exit() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::CommandStart, 1);
        c.observe_marker(MarkerEvent::CommandEnd { exit: Some(0), duration_ms: Some(500) }, 2);
        assert!(c.pending_cmd().is_none());
        let p = c.last_completed_cmd().expect("completed command should be set");
        assert_eq!(p.started_frame, 1);
        assert_eq!(p.ended_frame, Some(2));
        assert_eq!(p.exit, Some(0));
        assert_eq!(p.duration_ms, Some(500));
    }

    #[test]
    fn orphan_commandend_is_ignored() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::CommandEnd { exit: Some(0), duration_ms: None }, 1);
        assert!(c.pending_cmd().is_none());
        assert!(c.last_completed_cmd().is_none());
    }

    // ---- PTY exit ---------------------------------------------------

    #[test]
    fn pty_exit_from_raw_transitions_to_dead() {
        let mut c = PromptController::new(0);
        c.observe_pty_exit(1);
        assert_eq!(c.mode(), PaneMode::Dead);
        assert_eq!(c.last_transition().reason, TransitionReason::PtyExit);
    }

    #[test]
    fn pty_exit_from_editor_transitions_to_dead() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::PromptEnd, 1);
        c.observe_pty_exit(2);
        assert_eq!(c.mode(), PaneMode::Dead);
    }

    #[test]
    fn pty_exit_from_alt_screen_transitions_to_dead() {
        let mut c = PromptController::new(0);
        c.observe_alt_screen(true, 1);
        c.observe_pty_exit(2);
        assert_eq!(c.mode(), PaneMode::Dead);
    }

    #[test]
    fn pty_exit_closes_pending_cmd_with_unknown_exit() {
        let mut c = confirmed();
        c.observe_marker(MarkerEvent::CommandStart, 1);
        c.observe_pty_exit(2);
        assert!(c.pending_cmd().is_none());
        let p = c.last_completed_cmd().expect("completed command on pty exit");
        assert_eq!(p.exit, None);
        assert_eq!(p.ended_frame, Some(2));
    }

    #[test]
    fn pty_exit_from_dead_is_noop() {
        let mut c = PromptController::new(0);
        c.observe_pty_exit(1);
        let first = *c.last_transition();
        c.observe_pty_exit(2);
        // Second pty_exit should not record a new transition.
        assert_eq!(*c.last_transition(), first);
    }

    // ---- restart_shell ----------------------------------------------

    #[test]
    fn restart_shell_from_dead_goes_to_raw_with_unknown_integration() {
        let mut c = confirmed();
        c.observe_pty_exit(1);
        assert_eq!(c.mode(), PaneMode::Dead);
        c.restart_shell(2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.integration(), IntegrationState::Unknown);
        assert_eq!(c.last_transition().reason, TransitionReason::UserRestartedShell);
    }

    #[test]
    fn restart_shell_outside_dead_is_noop() {
        let mut c = PromptController::new(0);
        c.restart_shell(1);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        // No transition recorded beyond InitialSpawn.
        assert_eq!(c.last_transition().reason, TransitionReason::InitialSpawn);
    }

    // ---- shell kind tracking ----------------------------------------

    #[test]
    fn shell_marker_records_kind() {
        let mut c = PromptController::new(0);
        assert_eq!(c.shell_kind(), None);
        c.observe_marker(MarkerEvent::Shell(ShellKind::Zsh), 1);
        assert_eq!(c.shell_kind(), Some(ShellKind::Zsh));
    }
}
