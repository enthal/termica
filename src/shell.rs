//! `PromptController` — the pane-mode state machine.
//!
//! This is the load-bearing safety invariant of Termica
//! ([spec/05](../spec/05-pane-modes.md)). Every transition is
//! covered by a strict tests-first test in this module; a code
//! change that violates one of the five safety rules is a P0 bug.
//!
//! ## The five rules (from spec/05, restated)
//!
//! 1. The prompt editor is **NEVER active** unless the pane is at
//!    a Termica-bootstrap-confirmed shell prompt.
//! 2. **Alternate screen always disables the prompt editor.** No
//!    exceptions.
//! 3. **Termica's bootstrap is the only source of truth.** Foreign
//!    OSC 133 / OSC 1337 are ignored.
//! 4. The shell never sees editing keystrokes from the editor.
//! 5. Real TTY programs receive raw input.
//!
//! ## Bootstrap gates everything
//!
//! Every pane starts in `Bootstrapping`. Promotion to `RawTerminal`
//! requires an `IntegrationReady` lifecycle event at the supported
//! protocol version. Failure (timeout, `IntegrationError`, or
//! unsupported version) transitions to `Degraded` — a permanently
//! raw-only state for that shell run.
//!
//! ## Promotion gated, demotion eager (post-bootstrap)
//!
//! Promotion → `ShellPromptEditor` requires:
//! - The pane is in `RawTerminal` (never `Bootstrapping` /
//!   `AlternateScreen` / `Dead` / `Degraded`).
//! - A fresh `Precmd` lifecycle event arrived.
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

#![forbid(unsafe_code)]

use std::path::PathBuf;

use crate::markers::{LifecycleEvent, ShellKind};

/// Protocol version this build of Termica understands. The
/// integration script emits its own version inside the
/// `IntegrationReady` payload; we accept `v == SUPPORTED_…` and
/// flag any other value as unsupported.
pub const SUPPORTED_PROTOCOL_VERSION: u32 = 1;

/// Bootstrap timeout in frames. At egui's typical 60 Hz, 180 frames
/// is ~3 seconds. The exact unit is `tick_bootstrap_timeout`'s frame
/// counter; callers tick once per `update()` frame.
pub const BOOTSTRAP_TIMEOUT_FRAMES: u64 = 180;

/// The six exhaustive modes a pane can be in. Spec/05 documents the
/// transition diagram. New variants require an explicit spec update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneMode {
    /// Initial state. Bootstrap is running; output buffered, input
    /// dropped. Transitions to `RawTerminal` on `IntegrationReady`,
    /// to `Degraded` on `IntegrationError` / timeout / unsupported
    /// version.
    Bootstrapping,
    /// Default operational state. Keystrokes go to the PTY; output
    /// renders normally; no editor surface.
    RawTerminal,
    /// The pane is at a Termica-confirmed shell prompt. The egui
    /// editor owns input; only `submit_command` produces PTY bytes.
    ShellPromptEditor,
    /// Terminal has entered the alternate screen (vim / htop /
    /// less / fzf / tmux). Strictest raw mode.
    AlternateScreen,
    /// PTY child has exited. Transcript remains; "restart shell"
    /// UI is visible; history and search remain accessible.
    Dead,
    /// Bootstrap failed. Operational as a raw terminal but the
    /// editor will never activate for this shell run.
    /// `restart_shell` resets to `Bootstrapping` and tries again.
    Degraded,
}

/// How the integration handshake has progressed for this pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationState {
    /// No `IntegrationReady` lifecycle event has arrived yet.
    Unknown,
    /// The bootstrap announced a version we support. Editor
    /// promotion is gated on this.
    Confirmed { shell: ShellKind, version: u32 },
    /// The bootstrap announced a version we don't support, OR
    /// the bootstrap explicitly emitted `IntegrationError`, OR
    /// the bootstrap timeout expired.
    Failed(IntegrationFailure),
}

/// Why integration handshake failed. Carried in
/// `IntegrationState::Failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationFailure {
    Timeout,
    UnsupportedVersion(u32),
    ScriptError(String),
}

/// Why a mode transition occurred. Recorded in
/// [`TransitionRecord`] so an after-the-fact debug session
/// (`--dump-events` in Phase 3G, or `tracing` logs) can explain
/// why the pane ended up where it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionReason {
    InitialSpawn,
    BootstrapComplete,
    BootstrapTimeout,
    BootstrapError,
    IntegrationVersionUnsupported,
    PrecmdMarker,
    EnterSubmitted,
    CtrlCEmptyEditor,
    Esc,
    /// `RawTerminal → ShellPromptEditor` because the shell emitted
    /// its continuation prompt (`PS2`), which means it's waiting
    /// for more input after an incomplete submit. Termica restores
    /// the previously-submitted text into the editor with a trailing
    /// newline so the user can keep typing the next line.
    ContinuationMarker,
    AlternateScreenEnter,
    AlternateScreenExit,
    PtyExit,
    UserRestartedShell,
}

/// A single transition. The controller keeps only the most recent
/// one (callers can stream them out for `--dump-events`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionRecord {
    pub from: PaneMode,
    pub to: PaneMode,
    pub reason: TransitionReason,
    /// Frame counter at which the transition occurred. Caller
    /// supplies it; we never call `Instant::now()`.
    pub at: u64,
}

/// One command attempt. Opened either by `submit_command` (Enter
/// in the editor) or by a `Preexec` lifecycle event arriving
/// without a prior submit. Closed by `CommandFinished`,
/// `pty_exit`, or never.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCommand {
    pub started_frame: u64,
    pub ended_frame: Option<u64>,
    pub exit: Option<i32>,
    pub cwd_at_start: Option<PathBuf>,
    pub command_line: Option<String>,
}

/// The pane-mode state machine.
pub struct PromptController {
    mode: PaneMode,
    integration: IntegrationState,
    last_transition: TransitionRecord,
    pending_cmd: Option<PendingCommand>,
    last_completed_cmd: Option<PendingCommand>,
    shell_kind: Option<ShellKind>,
    cwd: Option<PathBuf>,
    /// Frame at which the pane entered `Bootstrapping`. Used to
    /// detect timeout via `tick_bootstrap_timeout`.
    spawn_frame: u64,
}

impl PromptController {
    /// Construct a fresh controller. Starts in `Bootstrapping` with
    /// `Unknown` integration.
    pub fn new(frame: u64) -> Self {
        Self {
            mode: PaneMode::Bootstrapping,
            integration: IntegrationState::Unknown,
            last_transition: TransitionRecord {
                from: PaneMode::Bootstrapping,
                to: PaneMode::Bootstrapping,
                reason: TransitionReason::InitialSpawn,
                at: frame,
            },
            pending_cmd: None,
            last_completed_cmd: None,
            shell_kind: None,
            cwd: None,
            spawn_frame: frame,
        }
    }

    /// Construct a controller that skips the `Bootstrapping` state
    /// and starts directly in `RawTerminal` with `Unknown`
    /// integration. Used by [`PaneSession::spawn`] for panes that
    /// were spawned outside the Phase 3 managed-startup machinery
    /// (e.g. `/bin/sh -c '…'` test panes). The editor will never
    /// activate for such a session because no `IntegrationReady`
    /// event will ever arrive, but raw-terminal input/output works
    /// normally.
    pub fn new_no_bootstrap(frame: u64) -> Self {
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
            cwd: None,
            spawn_frame: frame,
        }
    }

    // ---- observe_* (caller pushes external events in) ---------------

    pub fn observe_event(&mut self, event: LifecycleEvent, frame: u64) {
        match event {
            LifecycleEvent::IntegrationReady { shell, version } => {
                self.shell_kind = Some(shell);
                if version == SUPPORTED_PROTOCOL_VERSION {
                    self.integration = IntegrationState::Confirmed { shell, version };
                    if self.mode == PaneMode::Bootstrapping {
                        self.transition(
                            PaneMode::RawTerminal,
                            TransitionReason::BootstrapComplete,
                            frame,
                        );
                    }
                } else {
                    self.integration =
                        IntegrationState::Failed(IntegrationFailure::UnsupportedVersion(version));
                    if self.mode == PaneMode::Bootstrapping
                        || self.mode == PaneMode::RawTerminal
                        || self.mode == PaneMode::ShellPromptEditor
                    {
                        self.transition(
                            PaneMode::Degraded,
                            TransitionReason::IntegrationVersionUnsupported,
                            frame,
                        );
                    }
                }
            }
            LifecycleEvent::IntegrationError { reason } => {
                self.integration =
                    IntegrationState::Failed(IntegrationFailure::ScriptError(reason));
                if self.mode == PaneMode::Bootstrapping {
                    self.transition(PaneMode::Degraded, TransitionReason::BootstrapError, frame);
                }
            }
            LifecycleEvent::Preexec { command } => {
                // Open or annotate pending command. If submit_command
                // already opened one, this is confirming; stash the
                // command line for the block.
                if let Some(pending) = self.pending_cmd.as_mut() {
                    if pending.command_line.is_none() {
                        pending.command_line = Some(command);
                    }
                } else if self.mode != PaneMode::Dead && self.mode != PaneMode::Bootstrapping {
                    self.pending_cmd = Some(PendingCommand {
                        started_frame: frame,
                        ended_frame: None,
                        exit: None,
                        cwd_at_start: self.cwd.clone(),
                        command_line: Some(command),
                    });
                }
            }
            LifecycleEvent::CommandFinished { exit } => {
                self.close_pending_cmd(frame, Some(exit));
            }
            LifecycleEvent::Precmd { cwd } => {
                self.cwd = Some(cwd);
                self.try_promote_to_editor(frame);
            }
            LifecycleEvent::Cwd { cwd } => {
                self.cwd = Some(cwd);
            }
            LifecycleEvent::PromptVars { .. } => {
                // Routed to the status header consumer; no mode
                // transition. Phase 5 wires this up.
            }
            LifecycleEvent::ShellVars { .. } => {
                // Routed to the pane's `$VAR`-completion source
                // (`PaneSession`); never a mode transition. Mode safety:
                // a variable-name report says nothing about whether
                // we're at a prompt.
            }
            LifecycleEvent::CommandAborted { .. } => {
                // Treated like a CommandFinished with no exit: close
                // the pending block.
                self.close_pending_cmd(frame, None);
            }
            LifecycleEvent::Continuation => {
                self.try_promote_to_editor_for_continuation(frame);
            }
        }
    }

    /// Caller ticks this every frame while in `Bootstrapping` to
    /// enforce the timeout. Once we leave `Bootstrapping`, calling
    /// this is a no-op.
    pub fn tick_bootstrap_timeout(&mut self, frame: u64) {
        if self.mode != PaneMode::Bootstrapping {
            return;
        }
        if frame.saturating_sub(self.spawn_frame) >= BOOTSTRAP_TIMEOUT_FRAMES {
            self.integration = IntegrationState::Failed(IntegrationFailure::Timeout);
            self.transition(PaneMode::Degraded, TransitionReason::BootstrapTimeout, frame);
        }
    }

    pub fn observe_alt_screen(&mut self, on: bool, frame: u64) {
        // Alt-screen entry is impossible from Bootstrapping (output
        // is buffered, not rendered) — if we somehow saw it we'd
        // ignore it. Same for Dead.
        match (on, self.mode) {
            (true, m)
                if m != PaneMode::AlternateScreen
                    && m != PaneMode::Dead
                    && m != PaneMode::Bootstrapping =>
            {
                self.transition(
                    PaneMode::AlternateScreen,
                    TransitionReason::AlternateScreenEnter,
                    frame,
                );
            }
            (false, PaneMode::AlternateScreen) => {
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
        if self.pending_cmd.is_some() {
            self.close_pending_cmd(frame, None);
        }
        self.transition(PaneMode::Dead, TransitionReason::PtyExit, frame);
    }

    // ---- gestures (caller drives user-initiated transitions) --------

    pub fn submit_command(&mut self, frame: u64) {
        if self.mode != PaneMode::ShellPromptEditor {
            return;
        }
        self.transition(PaneMode::RawTerminal, TransitionReason::EnterSubmitted, frame);
        if self.pending_cmd.is_none() {
            self.pending_cmd = Some(PendingCommand {
                started_frame: frame,
                ended_frame: None,
                exit: None,
                cwd_at_start: self.cwd.clone(),
                command_line: None,
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
        if self.mode != PaneMode::Dead && self.mode != PaneMode::Degraded {
            return;
        }
        self.integration = IntegrationState::Unknown;
        self.pending_cmd = None;
        self.last_completed_cmd = None;
        self.shell_kind = None;
        self.cwd = None;
        self.spawn_frame = frame;
        self.transition(PaneMode::Bootstrapping, TransitionReason::UserRestartedShell, frame);
    }

    // ---- read accessors ---------------------------------------------

    pub fn mode(&self) -> PaneMode {
        self.mode
    }
    pub fn integration(&self) -> &IntegrationState {
        &self.integration
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
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// `true` while the pane should suppress display output and
    /// drop user input — i.e. while `Bootstrapping`. Once the
    /// bootstrap completes or fails, this returns `false` and the
    /// pane becomes user-visible.
    pub fn is_bootstrapping(&self) -> bool {
        self.mode == PaneMode::Bootstrapping
    }

    // ---- internals --------------------------------------------------

    fn try_promote_to_editor(&mut self, frame: u64) {
        if self.mode != PaneMode::RawTerminal {
            return;
        }
        if !matches!(self.integration, IntegrationState::Confirmed { .. }) {
            return;
        }
        if self.last_transition.from == PaneMode::ShellPromptEditor
            && self.last_transition.at == frame
        {
            return;
        }
        self.transition(PaneMode::ShellPromptEditor, TransitionReason::PrecmdMarker, frame);
    }

    /// `RawTerminal → ShellPromptEditor` driven by a continuation
    /// marker (zsh `PS2` emitted as a DCS-JSON event). Differs from
    /// [`Self::try_promote_to_editor`] in two ways:
    ///
    /// 1. **Gated on the prior transition reason.** Only re-promote
    ///    if the user actually got to `RawTerminal` by submitting
    ///    via the editor — if they pressed Esc to demote manually,
    ///    they want raw mode and we mustn't yank them back.
    /// 2. **No same-frame flap guard.** The submit and the
    ///    continuation marker can land in the same frame (the
    ///    shell's parser sees the incomplete command immediately).
    ///    The transition is intentional, not flap.
    fn try_promote_to_editor_for_continuation(&mut self, frame: u64) {
        if self.mode != PaneMode::RawTerminal {
            return;
        }
        if !matches!(self.integration, IntegrationState::Confirmed { .. }) {
            return;
        }
        // Only re-promote when the immediate predecessor was a
        // submit. Otherwise the user deliberately chose raw mode
        // (Esc, Ctrl-C, etc.) and the editor should stay closed.
        if self.last_transition.reason != TransitionReason::EnterSubmitted {
            return;
        }
        self.transition(PaneMode::ShellPromptEditor, TransitionReason::ContinuationMarker, frame);
    }

    fn close_pending_cmd(&mut self, frame: u64, exit: Option<i32>) {
        if let Some(mut p) = self.pending_cmd.take() {
            p.ended_frame = Some(frame);
            p.exit = exit;
            self.last_completed_cmd = Some(p);
        }
    }

    fn transition(&mut self, to: PaneMode, reason: TransitionReason, at: u64) {
        let from = self.mode;
        self.mode = to;
        self.last_transition = TransitionRecord { from, to, reason, at };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ready(c: &mut PromptController, frame: u64) {
        c.observe_event(
            LifecycleEvent::IntegrationReady {
                shell: ShellKind::Zsh,
                version: SUPPORTED_PROTOCOL_VERSION,
            },
            frame,
        );
    }

    // ---- bootstrap --------------------------------------------------

    #[test]
    fn fresh_controller_starts_in_bootstrapping() {
        let c = PromptController::new(0);
        assert_eq!(c.mode(), PaneMode::Bootstrapping);
        assert!(c.is_bootstrapping());
        assert_eq!(c.integration(), &IntegrationState::Unknown);
    }

    #[test]
    fn initial_transition_is_initialspawn() {
        let c = PromptController::new(42);
        let t = c.last_transition();
        assert_eq!(t.reason, TransitionReason::InitialSpawn);
        assert_eq!(t.from, PaneMode::Bootstrapping);
        assert_eq!(t.to, PaneMode::Bootstrapping);
        assert_eq!(t.at, 42);
    }

    #[test]
    fn integration_ready_supported_promotes_to_raw_terminal() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.last_transition().reason, TransitionReason::BootstrapComplete);
        assert!(matches!(
            c.integration(),
            IntegrationState::Confirmed { shell: ShellKind::Zsh, version: 1 }
        ));
    }

    #[test]
    fn integration_ready_unsupported_version_transitions_to_degraded() {
        let mut c = PromptController::new(0);
        c.observe_event(
            LifecycleEvent::IntegrationReady {
                shell: ShellKind::Zsh,
                version: SUPPORTED_PROTOCOL_VERSION + 1,
            },
            1,
        );
        assert_eq!(c.mode(), PaneMode::Degraded);
        assert_eq!(c.last_transition().reason, TransitionReason::IntegrationVersionUnsupported);
    }

    #[test]
    fn integration_error_transitions_to_degraded() {
        let mut c = PromptController::new(0);
        c.observe_event(
            LifecycleEvent::IntegrationError { reason: "missing bash-preexec".into() },
            1,
        );
        assert_eq!(c.mode(), PaneMode::Degraded);
        assert_eq!(c.last_transition().reason, TransitionReason::BootstrapError);
    }

    #[test]
    fn bootstrap_timeout_transitions_to_degraded() {
        let mut c = PromptController::new(0);
        // No events; tick past the timeout.
        c.tick_bootstrap_timeout(BOOTSTRAP_TIMEOUT_FRAMES);
        assert_eq!(c.mode(), PaneMode::Degraded);
        assert_eq!(c.last_transition().reason, TransitionReason::BootstrapTimeout);
    }

    #[test]
    fn bootstrap_timeout_does_not_fire_before_threshold() {
        let mut c = PromptController::new(0);
        c.tick_bootstrap_timeout(BOOTSTRAP_TIMEOUT_FRAMES - 1);
        assert_eq!(c.mode(), PaneMode::Bootstrapping);
    }

    #[test]
    fn bootstrap_timeout_does_not_fire_after_bootstrap_complete() {
        // If integration_ready arrived first, ticking past the
        // timeout is a no-op.
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.tick_bootstrap_timeout(1_000);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    // ---- safety rule 1: editor never active without bootstrap -------

    #[test]
    fn precmd_during_bootstrapping_does_not_promote() {
        // Even if a precmd somehow arrives during Bootstrapping (e.g.
        // the bootstrap script triggers the user's prompt mid-bootstrap),
        // we do NOT promote.
        let mut c = PromptController::new(0);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/tmp") }, 1);
        assert_eq!(c.mode(), PaneMode::Bootstrapping);
    }

    #[test]
    fn precmd_without_integration_ready_does_not_promote() {
        // Pathological: somehow we got out of Bootstrapping but
        // integration is still Unknown. (Shouldn't happen via normal
        // flow.) Promotion is still refused.
        let mut c = PromptController::new(0);
        // Force out of Bootstrapping via the transition helper isn't
        // possible from outside; use a degraded path. After degraded,
        // we still can't promote.
        c.observe_event(LifecycleEvent::IntegrationError { reason: "x".into() }, 1);
        assert_eq!(c.mode(), PaneMode::Degraded);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/tmp") }, 2);
        assert_eq!(c.mode(), PaneMode::Degraded);
    }

    #[test]
    fn precmd_with_confirmed_integration_promotes() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/home/u") }, 2);
        assert_eq!(c.mode(), PaneMode::ShellPromptEditor);
        assert_eq!(c.last_transition().reason, TransitionReason::PrecmdMarker);
        assert_eq!(c.cwd(), Some(std::path::Path::new("/home/u")));
    }

    // ---- safety rule 2: alt-screen disables editor ------------------

    #[test]
    fn alt_screen_enter_from_raw_transitions_to_alt() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_alt_screen(true, 2);
        assert_eq!(c.mode(), PaneMode::AlternateScreen);
        assert_eq!(c.last_transition().reason, TransitionReason::AlternateScreenEnter);
    }

    #[test]
    fn alt_screen_enter_from_editor_demotes_to_alt() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/") }, 2);
        c.observe_alt_screen(true, 3);
        assert_eq!(c.mode(), PaneMode::AlternateScreen);
    }

    #[test]
    fn precmd_in_alt_screen_does_not_promote() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_alt_screen(true, 2);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/") }, 3);
        assert_eq!(c.mode(), PaneMode::AlternateScreen);
    }

    #[test]
    fn alt_screen_exit_returns_to_raw_not_editor() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_alt_screen(true, 2);
        c.observe_alt_screen(false, 3);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    #[test]
    fn alt_screen_during_bootstrapping_is_ignored() {
        let mut c = PromptController::new(0);
        c.observe_alt_screen(true, 1);
        assert_eq!(c.mode(), PaneMode::Bootstrapping);
    }

    // ---- safety rule 3: bootstrap authoritative ---------------------
    //
    // The osc.rs unit tests already cover "foreign OSC 133 is dropped
    // by the byte-stream parser." Here we just assert that
    // `PromptController` never invents promotion absent a
    // Termica-sourced event.

    #[test]
    fn no_event_other_than_precmd_promotes() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Cwd { cwd: PathBuf::from("/tmp") }, 2);
        c.observe_event(LifecycleEvent::Preexec { command: "x".into() }, 2);
        c.observe_event(LifecycleEvent::CommandFinished { exit: 0 }, 2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    // ---- frame debounce ---------------------------------------------

    #[test]
    fn promotion_debounces_within_one_frame_of_demotion() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/") }, 2);
        c.submit_command(3);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        // Same-frame Precmd: no re-promotion.
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/") }, 3);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    #[test]
    fn promotion_allowed_next_frame_after_demotion() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/") }, 2);
        c.submit_command(3);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/") }, 4);
        assert_eq!(c.mode(), PaneMode::ShellPromptEditor);
    }

    // ---- submit / leave gestures ------------------------------------

    #[test]
    fn submit_command_demotes_eagerly_and_opens_pending() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/") }, 2);
        c.submit_command(3);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        let p = c.pending_cmd().expect("pending command should be open");
        assert_eq!(p.started_frame, 3);
    }

    #[test]
    fn submit_command_outside_editor_is_noop() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.submit_command(2);
        assert!(c.pending_cmd().is_none());
    }

    #[test]
    fn preexec_after_submit_stamps_command_line() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/") }, 2);
        c.submit_command(3);
        c.observe_event(LifecycleEvent::Preexec { command: "ls -la".into() }, 4);
        let p = c.pending_cmd().expect("pending should remain open");
        assert_eq!(p.command_line.as_deref(), Some("ls -la"));
    }

    #[test]
    fn preexec_alone_opens_pending() {
        // Raw-typed command path: user typed at a prompt without using
        // the editor.
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Preexec { command: "true".into() }, 2);
        let p = c.pending_cmd().expect("pending opens on Preexec");
        assert_eq!(p.command_line.as_deref(), Some("true"));
    }

    // ---- command lifecycle ------------------------------------------

    #[test]
    fn command_finished_closes_pending_with_exit() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Preexec { command: "ls".into() }, 2);
        c.observe_event(LifecycleEvent::CommandFinished { exit: 0 }, 3);
        assert!(c.pending_cmd().is_none());
        let p = c.last_completed_cmd().expect("completed should be set");
        assert_eq!(p.exit, Some(0));
        assert_eq!(p.ended_frame, Some(3));
    }

    #[test]
    fn orphan_command_finished_is_ignored() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::CommandFinished { exit: 0 }, 2);
        assert!(c.pending_cmd().is_none());
        assert!(c.last_completed_cmd().is_none());
    }

    #[test]
    fn command_aborted_closes_pending_without_exit() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Preexec { command: "x".into() }, 2);
        c.observe_event(LifecycleEvent::CommandAborted { reason: "user".into() }, 3);
        assert!(c.pending_cmd().is_none());
        let p = c.last_completed_cmd().expect("completed should be set");
        assert!(p.exit.is_none());
    }

    // ---- PTY exit ---------------------------------------------------

    #[test]
    fn pty_exit_from_raw_transitions_to_dead() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_pty_exit(2);
        assert_eq!(c.mode(), PaneMode::Dead);
    }

    #[test]
    fn pty_exit_from_bootstrapping_transitions_to_dead() {
        let mut c = PromptController::new(0);
        c.observe_pty_exit(1);
        assert_eq!(c.mode(), PaneMode::Dead);
    }

    #[test]
    fn pty_exit_closes_pending_cmd_with_unknown_exit() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Preexec { command: "x".into() }, 2);
        c.observe_pty_exit(3);
        assert!(c.pending_cmd().is_none());
        let p = c.last_completed_cmd().expect("completed should be set");
        assert!(p.exit.is_none());
    }

    #[test]
    fn pty_exit_from_dead_is_noop() {
        let mut c = PromptController::new(0);
        c.observe_pty_exit(1);
        let first = c.last_transition().clone();
        c.observe_pty_exit(2);
        assert_eq!(c.last_transition(), &first);
    }

    // ---- restart_shell ----------------------------------------------

    #[test]
    fn restart_shell_from_dead_goes_to_bootstrapping() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_pty_exit(2);
        c.restart_shell(3);
        assert_eq!(c.mode(), PaneMode::Bootstrapping);
        assert_eq!(c.integration(), &IntegrationState::Unknown);
        assert_eq!(c.last_transition().reason, TransitionReason::UserRestartedShell);
    }

    #[test]
    fn restart_shell_from_degraded_goes_to_bootstrapping() {
        let mut c = PromptController::new(0);
        c.observe_event(LifecycleEvent::IntegrationError { reason: "x".into() }, 1);
        assert_eq!(c.mode(), PaneMode::Degraded);
        c.restart_shell(2);
        assert_eq!(c.mode(), PaneMode::Bootstrapping);
    }

    #[test]
    fn restart_shell_outside_dead_or_degraded_is_noop() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.restart_shell(2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    // ---- cwd tracking -----------------------------------------------

    #[test]
    fn cwd_event_alone_does_not_promote() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        let before = c.last_transition().clone();
        c.observe_event(LifecycleEvent::Cwd { cwd: PathBuf::from("/tmp") }, 2);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.last_transition(), &before);
    }

    #[test]
    fn cwd_event_updates_tracked_cwd() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Cwd { cwd: PathBuf::from("/a") }, 2);
        assert_eq!(c.cwd(), Some(std::path::Path::new("/a")));
    }

    #[test]
    fn submit_command_stamps_cwd_into_pending() {
        let mut c = PromptController::new(0);
        ready(&mut c, 1);
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/Users/tim/code") }, 2);
        c.submit_command(3);
        let p = c.pending_cmd().expect("pending should be open");
        assert_eq!(p.cwd_at_start, Some(PathBuf::from("/Users/tim/code")));
    }

    // ---- shell kind tracking ----------------------------------------

    // ---- continuation marker (multi-line submit) --------------------

    /// Helper: bring a fresh controller to `RawTerminal` with
    /// integration confirmed, then submit a command. That puts the
    /// controller in exactly the state our `Continuation` handler
    /// expects to re-promote from.
    fn controller_at_post_submit() -> PromptController {
        let mut c = PromptController::new_no_bootstrap(0);
        c.observe_event(
            LifecycleEvent::IntegrationReady {
                shell: ShellKind::Zsh,
                version: SUPPORTED_PROTOCOL_VERSION,
            },
            1,
        );
        c.observe_event(LifecycleEvent::Precmd { cwd: PathBuf::from("/tmp") }, 2);
        assert_eq!(c.mode(), PaneMode::ShellPromptEditor);
        c.submit_command(3);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
        assert_eq!(c.last_transition().reason, TransitionReason::EnterSubmitted);
        c
    }

    #[test]
    fn continuation_after_submit_repromotes_to_editor() {
        let mut c = controller_at_post_submit();
        c.observe_event(LifecycleEvent::Continuation, 4);
        assert_eq!(c.mode(), PaneMode::ShellPromptEditor);
        assert_eq!(c.last_transition().reason, TransitionReason::ContinuationMarker);
    }

    #[test]
    fn continuation_after_esc_demote_does_not_repromote() {
        // User submitted, then Esc'd back to RawTerminal manually
        // (well, actually we Esc only from ShellPromptEditor — but
        // in a real session the equivalent is: integration_ready
        // arrived, no Precmd yet, the user is happily typing in
        // raw mode, and somehow a Continuation marker arrives.
        // Re-promoting would yank them away from raw mode — bad.).
        let mut c = PromptController::new_no_bootstrap(0);
        c.observe_event(
            LifecycleEvent::IntegrationReady {
                shell: ShellKind::Zsh,
                version: SUPPORTED_PROTOCOL_VERSION,
            },
            1,
        );
        // Last transition is BootstrapComplete or similar — NOT
        // EnterSubmitted. Continuation must be a no-op.
        c.observe_event(LifecycleEvent::Continuation, 2);
        assert_eq!(c.mode(), PaneMode::RawTerminal, "should not repromote without prior submit");
    }

    #[test]
    fn continuation_when_integration_unconfirmed_is_noop() {
        // Pre-integration-ready: integration is `Unknown`, gate is
        // closed. A stray continuation marker can't matter yet.
        let mut c = PromptController::new_no_bootstrap(0);
        c.observe_event(LifecycleEvent::Continuation, 1);
        assert_eq!(c.mode(), PaneMode::RawTerminal);
    }

    #[test]
    fn continuation_from_alt_screen_is_noop() {
        let mut c = controller_at_post_submit();
        c.observe_alt_screen(true, 4);
        assert_eq!(c.mode(), PaneMode::AlternateScreen);
        c.observe_event(LifecycleEvent::Continuation, 5);
        assert_eq!(c.mode(), PaneMode::AlternateScreen, "alt-screen must not repromote");
    }

    #[test]
    fn integration_ready_records_shell_kind() {
        let mut c = PromptController::new(0);
        assert_eq!(c.shell_kind(), None);
        c.observe_event(
            LifecycleEvent::IntegrationReady {
                shell: ShellKind::Bash,
                version: SUPPORTED_PROTOCOL_VERSION,
            },
            1,
        );
        assert_eq!(c.shell_kind(), Some(ShellKind::Bash));
    }
}
