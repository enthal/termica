//! Block model — the unit of transcript content.
//!
//! Phase 4 (see [spec/04](../spec/04-prompt-editor.md) and
//! [spec/02](../spec/02-terminal-engine.md)) pivots Termica from a
//! single growing `alacritty_terminal::Term` per pane to a vertical
//! stack of **blocks**, each one being a single command plus its
//! decorations and output. This module is the data model for that
//! stack; rendering and the live-`Term` reset come in subsequent
//! sub-PRs (4A-render and beyond).
//!
//! ## State machine
//!
//! ```text
//!  Prompt  ──Preexec──▶  Running  ──CommandFinished──▶  Sealed
//!                                                        │
//!                                                        ▼
//!                                                       (a new Prompt block is pushed)
//! ```
//!
//! - The pane is born with exactly one `Prompt` block.
//! - When [`LifecycleEvent::Preexec`] arrives, the live tail's
//!   `Prompt` transforms into `Running`, carrying the command string.
//! - When [`LifecycleEvent::CommandFinished`] arrives, the live tail's
//!   `Running` seals: the whole live `Term` is snapshotted into a
//!   `Vec<StyledLine>`, the `Term` is reset for the next block, the
//!   exit code and duration are recorded, and a fresh `Prompt` block
//!   is pushed.
//!
//! ## Why this module is self-contained
//!
//! `BlockStack::observe_lifecycle_event` takes a `&mut TerminalState`
//! so it can snapshot-and-reset the grid at seal time, but it never
//! touches the renderer, never assumes egui, and never mutates state
//! outside its own `Vec<Block>` (plus the terminal it's handed). This
//! is the strict-layer engine surface; tests drive it without a PTY.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use crate::markers::LifecycleEvent;
use crate::prompt_editor::PromptEditor;
use crate::terminal::{StyledLine, TerminalState};

/// Per-pane monotonically increasing identifier for blocks. New
/// values come from [`BlockStack::next_id`]; never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u64);

/// Information about the shell environment at the moment a block
/// begins: the cwd, and (4G-async-context) the git context **captured
/// at command-start**. `git` is `None` on the live `Prompt` block —
/// the prompt header reads the pane's *current* git live instead — and
/// is stamped in at `Preexec` so `Running` / `Sealed` blocks show the
/// branch / dirtiness the command actually ran under, frozen as history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockHeader {
    pub cwd: Option<PathBuf>,
    pub git: Option<crate::git_context::GitContext>,
}

/// One block in the per-pane stack. The variants encode the three
/// lifecycle states described in [spec/04](../spec/04-prompt-editor.md):
///
/// - [`Block::Prompt`] — the shell is idle at a prompt. The live
///   `Term` belongs to this block until [`LifecycleEvent::Preexec`]
///   arrives.
/// - [`Block::Running`] — a command is executing. The same live
///   `Term` continues to be fed; bytes accumulate in the `Term`
///   until `CommandFinished` snapshots and resets it.
/// - [`Block::Sealed`] — the command has finished and its output is
///   a frozen `Vec<StyledLine>` snapshot. No live state remains in
///   the block. The live `Term` was reset after the snapshot was
///   taken, so subsequent bytes start a fresh block at row 0.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Prompt {
        id: BlockId,
        header: BlockHeader,
        /// Frame counter at which the block was created (the same
        /// monotonic counter the [`crate::shell::PromptController`]
        /// uses for transitions).
        started_at_frame: u64,
        /// Native editor for this prompt. Active when the pane is
        /// in `ShellPromptEditor` mode (spec/05); otherwise dormant
        /// (its text is `""`). 4C populates this with the user's
        /// in-progress command and submits it on Enter.
        editor: PromptEditor,
    },
    Running {
        id: BlockId,
        header: BlockHeader,
        command: String,
        /// Wall-clock time (Unix epoch ms) the command started running
        /// (`Preexec`). Sealed into a real `Duration` at
        /// `CommandFinished`. Sourced from the live pane clock; tests
        /// pass literal ms so the duration is deterministic.
        started_at_ms: i64,
    },
    Sealed {
        id: BlockId,
        header: BlockHeader,
        command: String,
        snapshot: Vec<StyledLine>,
        duration: Duration,
        exit: Option<i32>,
    },
}

impl Block {
    /// Build a `Sealed` block from restored scrollback (9F). A chunk
    /// stores only the *output* logical lines, not the command string,
    /// exit code, or git header (those live in `runs`, not linked to
    /// chunks today) — so a restored block carries an empty command and
    /// `exit: None`, showing the transcript without a command label.
    /// `duration` is recovered from the chunk's recorded time span.
    pub fn restored_sealed(
        id: BlockId,
        snapshot: Vec<StyledLine>,
        start_time_ms: i64,
        end_time_ms: i64,
    ) -> Self {
        Block::Sealed {
            id,
            header: BlockHeader::default(),
            command: String::new(),
            snapshot,
            duration: Duration::from_millis(end_time_ms.saturating_sub(start_time_ms).max(0) as u64),
            exit: None,
        }
    }

    /// Stable id, regardless of variant.
    pub fn id(&self) -> BlockId {
        match self {
            Block::Prompt { id, .. } | Block::Running { id, .. } | Block::Sealed { id, .. } => *id,
        }
    }
}

/// What a seal produced, handed up to the caller so it can be
/// persisted. The block stack stays free of I/O and threads: it
/// *returns* this value from [`BlockStack::observe_lifecycle_event`]
/// and the pane forwards it to the background chunk writer (9D). The
/// `lines` are the same width-independent logical lines stored in the
/// resulting `Block::Sealed` (a clone — a future optimization may
/// share them via `Arc`); the writer encodes them straight into a
/// chunk file.
#[derive(Debug, Clone, PartialEq)]
pub struct SealedSnapshot {
    pub block_id: BlockId,
    pub lines: Vec<StyledLine>,
    /// Pane width when the block sealed. Diagnostic only — logical
    /// lines re-wrap to any width — but recorded in `scrollback_chunk`.
    pub emit_cols: u32,
    pub start_time_ms: i64,
    pub end_time_ms: i64,
}

/// The per-pane vertical stack of [`Block`]s. The last element is
/// always the **live tail**: either a `Prompt` (the shell is idle)
/// or a `Running` (a command is executing). Older blocks are always
/// `Sealed`.
///
/// ### Invariants (enforced by the constructor and event handlers)
///
/// 1. The stack is never empty after [`BlockStack::new`].
/// 2. The last element is `Prompt` or `Running`; never `Sealed`.
/// 3. All earlier elements are `Sealed`.
/// 4. Block ids are monotonically increasing in order of creation.
#[derive(Debug)]
pub struct BlockStack {
    blocks: Vec<Block>,
    next_id: u64,
    /// Wall-clock time (Unix epoch ms) to stamp the next observed
    /// lifecycle event with. Set by the pane via
    /// [`Self::set_event_clock_ms`] right before each
    /// [`Self::observe_lifecycle_event`]; read by `start_running` /
    /// `seal_running` to compute a command's real duration. `0` until
    /// the pane sets it (and in duration-agnostic tests).
    event_clock_ms: i64,
    /// The pane's current git context, stamped into a block's header at
    /// `Preexec` so `Running` / `Sealed` blocks freeze the git state the
    /// command ran under (4G-async-context). Set by the pane via
    /// [`Self::set_current_git`] right before
    /// [`Self::observe_lifecycle_event`]; `None` outside a repo / before
    /// the first probe returns (and in git-agnostic tests).
    current_git: Option<crate::git_context::GitContext>,
}

impl BlockStack {
    /// Build a fresh stack containing exactly one [`Block::Prompt`]
    /// with id `0`. The shell hasn't run anything yet, but the
    /// pane is conceptually "at a prompt about to be drawn."
    pub fn new(started_at_frame: u64) -> Self {
        let mut stack = Self {
            blocks: Vec::with_capacity(8),
            next_id: 0,
            event_clock_ms: 0,
            current_git: None,
        };
        let id = stack.alloc_id();
        stack.blocks.push(Block::Prompt {
            id,
            header: BlockHeader::default(),
            started_at_frame,
            editor: PromptEditor::new(),
        });
        stack
    }

    /// Build a stack pre-populated with restored `Sealed` blocks (9F),
    /// then a fresh live `Prompt` tail so the invariants hold (last is
    /// always live; ids are monotonic). The blocks must all be
    /// `Block::Sealed` and ordered oldest→newest; their ids are
    /// reassigned `0..n` so the tail `Prompt` gets `n`.
    pub fn with_restored_sealed(sealed: Vec<Block>) -> Self {
        debug_assert!(
            sealed.iter().all(|b| matches!(b, Block::Sealed { .. })),
            "with_restored_sealed takes only Sealed blocks"
        );
        let next_id = sealed.len() as u64;
        let mut stack = Self { blocks: sealed, next_id, event_clock_ms: 0, current_git: None };
        let id = stack.alloc_id();
        stack.blocks.push(Block::Prompt {
            id,
            header: BlockHeader::default(),
            started_at_frame: 0,
            editor: PromptEditor::new(),
        });
        stack
    }

    fn alloc_id(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Total number of blocks in the stack (sealed + live tail).
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// True if `len() == 0`. Cannot actually happen by invariant;
    /// included for `clippy::len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Iterate blocks top-to-bottom (oldest first, live tail last).
    pub fn iter(&self) -> std::slice::Iter<'_, Block> {
        self.blocks.iter()
    }

    /// Drop every `Sealed` block, keeping the live tail (whichever
    /// of `Prompt` / `Running` it currently is). The new stack
    /// preserves the in-flight command's id and editor state — the
    /// user sees the scrollback erased but their current prompt
    /// untouched. Used by Cmd+K / Ctrl+Shift+K (clear scrollback).
    pub fn clear_sealed(&mut self) {
        self.blocks.retain(|b| !matches!(b, Block::Sealed { .. }));
    }

    /// True if any command has finished and been sealed into
    /// scrollback. A freshly-spawned pane (or one cleared via
    /// `clear_sealed`) has none — that "no scrollback blocks yet"
    /// state is what gates the blank-pane watermark
    /// ([`crate::watermark`]).
    pub fn has_sealed_blocks(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::Sealed { .. }))
    }

    /// Borrow the live tail block. Always present by invariant; only
    /// returns `Option` so the API stays defensive in case a future
    /// caller manages to clear the stack out from under us (it
    /// shouldn't, but we don't want to `panic!` here).
    pub fn last(&self) -> Option<&Block> {
        self.blocks.last()
    }

    /// Apply one lifecycle event from the [`crate::shell::PromptController`].
    ///
    /// `terminal` is mutated only at the seal call (`CommandFinished`),
    /// where we snapshot the whole grid into the sealed block and
    /// reset the `Term` so the next block starts with a clean slate.
    /// At `Preexec` we just flip the tail variant; no terminal access.
    ///
    /// `Precmd` and `Cwd` events update the **tail block's** header
    /// cwd (Phase 4G) — only when the tail is a `Prompt`. A `Running`
    /// block's header is the cwd at command-start and is never
    /// rewritten mid-execution.
    ///
    /// Other lifecycle events (`IntegrationReady`, `PromptVars`,
    /// `IntegrationError`, `CommandAborted`) are the controller's
    /// concern, not the stack's; they leave the stack alone.
    /// Returns the [`SealedSnapshot`] of a block that just sealed (only
    /// the `CommandFinished` path produces one), for the pane to forward
    /// to the chunk writer; `None` for every other event.
    pub fn observe_lifecycle_event(
        &mut self,
        event: &LifecycleEvent,
        terminal: &mut TerminalState,
        frame: u64,
    ) -> Option<SealedSnapshot> {
        match event {
            LifecycleEvent::Preexec { command } => {
                self.start_running(command.clone());
                None
            }
            LifecycleEvent::CommandFinished { exit } => self.seal_running(*exit, terminal, frame),
            LifecycleEvent::Precmd { cwd } | LifecycleEvent::Cwd { cwd } => {
                self.update_tail_cwd(cwd.clone());
                None
            }
            _ => None,
        }
    }

    /// Stamp the wall-clock time (Unix epoch ms) of the *next* lifecycle
    /// event the stack will observe. The pane sets this from its clock
    /// immediately before [`Self::observe_lifecycle_event`], so a
    /// `Preexec` records the command's real start time and a
    /// `CommandFinished` records its real end time — the difference is
    /// the command's wall-clock duration. Defaults to `0`; tests that
    /// assert on duration set it explicitly, the rest leave it.
    pub fn set_event_clock_ms(&mut self, now_ms: i64) {
        self.event_clock_ms = now_ms;
    }

    /// Stamp the pane's current git context to freeze into the *next*
    /// block that starts running. The pane sets this from its async
    /// probe immediately before [`Self::observe_lifecycle_event`], so a
    /// `Preexec` captures the branch / dirty state the command ran under
    /// (4G-async-context). `Running` / `Sealed` headers keep that frozen
    /// copy; the live `Prompt` header is unaffected (it reads the pane's
    /// current git directly). Defaults to `None`.
    pub fn set_current_git(&mut self, git: Option<crate::git_context::GitContext>) {
        self.current_git = git;
    }

    /// Live elapsed time of the currently-`Running` command, measured
    /// against the wall-clock `now_ms` the caller supplies. `None` when
    /// the tail is a `Prompt` (nothing running). Clamps backwards clock
    /// skew to zero. Pure (time is a parameter) so the live-timer logic
    /// is deterministic in tests; the renderer passes `wall_clock_ms()`.
    pub fn running_elapsed_at(&self, now_ms: i64) -> Option<Duration> {
        match self.blocks.last() {
            Some(Block::Running { started_at_ms, .. }) => {
                Some(Duration::from_millis(now_ms.saturating_sub(*started_at_ms).max(0) as u64))
            }
            _ => None,
        }
    }

    /// Update the live tail block's `BlockHeader.cwd` when the shell
    /// reports a new working directory.
    ///
    /// Applied to `Prompt` (the next command will run here) and
    /// **ignored** for `Running` blocks: a running command's cwd is
    /// the cwd that was current at `Preexec`, even if the program
    /// re-emits a `Cwd` marker mid-execution. Sealed headers are
    /// frozen and never updated.
    fn update_tail_cwd(&mut self, cwd: PathBuf) {
        let Some(tail) = self.blocks.last_mut() else { return };
        if let Block::Prompt { header, .. } = tail {
            header.cwd = Some(cwd);
        }
    }

    /// `Preexec` arrived. If the tail is a `Prompt`, transform it
    /// into a `Running` carrying the command string. If the tail is
    /// already `Running` (e.g. a stray `Preexec` from a nested
    /// context — see
    /// [spec/03 §"Nested shells"](../spec/03-shell-integration.md)),
    /// ignore: only one command can be running at a time per pane.
    fn start_running(&mut self, command: String) {
        let now_ms = self.event_clock_ms;
        let Some(tail) = self.blocks.last_mut() else { return };
        if !matches!(tail, Block::Prompt { .. }) {
            // Already running, or sealed (impossible by invariant
            // since the last is always live). Drop the event; the
            // shell is in an unexpected state and we'd rather lose
            // a block boundary than corrupt the stack.
            return;
        }
        let (id, mut header) = match tail {
            Block::Prompt { id, header, .. } => (*id, header.clone()),
            _ => unreachable!(),
        };
        // Freeze the git state the command runs under into the block
        // header (4G-async-context). The `Prompt` header's own `git` is
        // always `None` (the live prompt reads current git separately);
        // the captured value comes from the pane's current probe result.
        header.git = self.current_git.clone();
        // The editor's content is implicitly discarded here. In
        // Phase 4C, `submit_command` will have moved the text out
        // before `Preexec` arrives; for 4B with no submit wired yet,
        // any half-typed buffer is dropped — same as if the user had
        // pressed Esc and typed the command through the PTY instead.
        *tail = Block::Running { id, header, command, started_at_ms: now_ms };
    }

    /// `CommandFinished` arrived. If the tail is `Running`, snapshot
    /// the whole live `Term`, seal the tail, reset the `Term` for the
    /// next block, and push a fresh `Prompt`. If the tail is `Prompt`
    /// (a stray finish with no prior `Preexec`), ignore: there's no
    /// command to seal.
    ///
    /// Order matters: snapshot first, then reset. Otherwise the
    /// sealed snapshot is empty.
    fn seal_running(
        &mut self,
        exit: i32,
        terminal: &mut TerminalState,
        frame: u64,
    ) -> Option<SealedSnapshot> {
        use alacritty_terminal::grid::Dimensions;

        let now_ms = self.event_clock_ms;
        let tail = self.blocks.last()?;
        let Block::Running { id, header, command, started_at_ms } = tail else {
            return None;
        };
        let id = *id;
        let header = header.clone();
        let command = command.clone();
        let started_ms = *started_at_ms;

        // Pane width at seal, before the reset clears the grid. Logical
        // lines are width-independent; this is recorded for diagnostics.
        let emit_cols = terminal.grid().columns() as u32;
        // Store the sealed output as width-independent LOGICAL lines
        // (un-wrapped from the grid rows), not fixed-width grid rows, so
        // the block can reflow to the current pane width on render
        // (Phase 9B; see [`crate::reflow`] and spec/08 §"Logical lines").
        let snapshot = crate::persist::chunk::unwrap_rows(&terminal.snapshot_lines_all());
        terminal.reset_for_new_block();
        // Real wall-clock duration; clamp negative clock skew to zero.
        let duration = Duration::from_millis(now_ms.saturating_sub(started_ms).max(0) as u64);

        // What the writer persists. Cloning the lines keeps the block
        // stack and the writer decoupled (the Sealed block keeps its own
        // copy for rendering); a future optimization may share via `Arc`.
        let produced = SealedSnapshot {
            block_id: id,
            lines: snapshot.clone(),
            emit_cols,
            start_time_ms: started_ms,
            end_time_ms: now_ms,
        };

        let sealed = Block::Sealed {
            id,
            header: header.clone(),
            command,
            snapshot,
            duration,
            exit: Some(exit),
        };
        // Replace the live tail with the sealed block, then push a
        // fresh Prompt. Two steps, in this order, so the invariant
        // "last is always live" never observes "last is Sealed" mid-
        // operation as seen from outside this function.
        let last = self.blocks.last_mut().expect("checked above");
        *last = sealed;
        let new_id = self.alloc_id();
        self.blocks.push(Block::Prompt {
            id: new_id,
            header,
            started_at_frame: frame,
            editor: PromptEditor::new(),
        });
        Some(produced)
    }

    /// Mutable access to the editor on the live tail block, if any.
    /// `None` when the tail is not a `Prompt` (e.g. a `Running`
    /// command is executing) — input routing in `render_pane`
    /// already checks the mode and tail variant before reaching for
    /// this, so a `None` return is a no-op for editor input.
    pub fn editor_on_tail_mut(&mut self) -> Option<&mut PromptEditor> {
        match self.blocks.last_mut()? {
            Block::Prompt { editor, .. } => Some(editor),
            _ => None,
        }
    }

    /// Read-only access to the same editor.
    pub fn editor_on_tail(&self) -> Option<&PromptEditor> {
        match self.blocks.last()? {
            Block::Prompt { editor, .. } => Some(editor),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::ShellKind;

    fn fresh_stack() -> BlockStack {
        BlockStack::new(0)
    }

    // ---- editor wiring (Phase 4B) ------------------------------------

    #[test]
    fn fresh_stack_exposes_an_empty_editor_on_the_tail() {
        let stack = fresh_stack();
        let editor = stack.editor_on_tail().expect("Prompt tail has an editor");
        assert!(editor.is_empty(), "fresh editor should have no text");
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn editor_persists_typing_on_the_prompt_tail() {
        let mut stack = fresh_stack();
        let editor = stack.editor_on_tail_mut().expect("Prompt tail");
        editor.insert_str("git st");
        assert_eq!(stack.editor_on_tail().unwrap().text(), "git st");
    }

    #[test]
    fn running_tail_has_no_editor_handle() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "ls".into() },
            &mut term,
            1,
        );
        assert!(stack.editor_on_tail().is_none(), "Running tail has no editor");
        assert!(stack.editor_on_tail_mut().is_none());
    }

    #[test]
    fn has_sealed_blocks_tracks_command_lifecycle() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        // Fresh pane: just the initial Prompt, nothing sealed yet.
        assert!(!stack.has_sealed_blocks(), "fresh stack has no sealed blocks");

        // Run a command to completion — seals one block.
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "ls".into() },
            &mut term,
            1,
        );
        assert!(!stack.has_sealed_blocks(), "a running command is not yet sealed");
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 2);
        assert!(stack.has_sealed_blocks(), "finished command seals a scrollback block");

        // Clearing scrollback returns to the pristine, watermark-eligible state.
        stack.clear_sealed();
        assert!(!stack.has_sealed_blocks(), "clear_sealed drops sealed blocks");
    }

    #[test]
    fn seal_then_new_prompt_has_a_fresh_empty_editor() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        // Pre-populate the editor — should not survive Preexec
        // (4B's note: editor content is dropped at this boundary).
        stack.editor_on_tail_mut().unwrap().insert_str("typed-but-dropped");

        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "ls".into() },
            &mut term,
            1,
        );
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 2);

        let editor = stack.editor_on_tail().expect("new Prompt has editor");
        assert!(editor.is_empty(), "new Prompt's editor starts fresh");
    }

    #[test]
    fn new_stack_starts_with_one_prompt_block_id_zero() {
        let stack = fresh_stack();
        assert_eq!(stack.len(), 1);
        match stack.last().unwrap() {
            Block::Prompt { id, header, .. } => {
                assert_eq!(*id, BlockId(0));
                assert!(header.cwd.is_none(), "header.cwd default is None");
            }
            other => panic!("expected initial Prompt, got {other:?}"),
        }
    }

    #[test]
    fn preexec_transitions_tail_prompt_to_running() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.set_event_clock_ms(7_777);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "ls -la".into() },
            &mut term,
            10,
        );
        assert_eq!(stack.len(), 1);
        match stack.last().unwrap() {
            Block::Running { id, command, started_at_ms, .. } => {
                assert_eq!(*id, BlockId(0), "id preserved across Prompt → Running");
                assert_eq!(command, "ls -la");
                assert_eq!(*started_at_ms, 7_777);
            }
            other => panic!("expected Running after Preexec, got {other:?}"),
        }
    }

    #[test]
    fn preexec_when_tail_already_running_is_ignored() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "first".into() },
            &mut term,
            10,
        );
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "second-should-be-ignored".into() },
            &mut term,
            11,
        );
        match stack.last().unwrap() {
            Block::Running { command, .. } => assert_eq!(command, "first"),
            other => panic!("expected Running, got {other:?}"),
        }
        assert_eq!(stack.len(), 1, "no extra block created");
    }

    #[test]
    fn command_finished_seals_running_and_pushes_new_prompt() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "echo hi".into() },
            &mut term,
            10,
        );
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 20);

        assert_eq!(stack.len(), 2, "sealed + new prompt");
        // First (older) is sealed.
        match stack.iter().next().unwrap() {
            Block::Sealed { id, command, exit, .. } => {
                assert_eq!(*id, BlockId(0));
                assert_eq!(command, "echo hi");
                assert_eq!(*exit, Some(0));
            }
            other => panic!("expected Sealed in position 0, got {other:?}"),
        }
        // Tail is a fresh Prompt with a new id.
        match stack.last().unwrap() {
            Block::Prompt { id, .. } => assert_eq!(*id, BlockId(1)),
            other => panic!("expected Prompt tail, got {other:?}"),
        }
    }

    #[test]
    fn seal_computes_wall_clock_duration_from_ms() {
        // Duration is real wall-clock time (Preexec → CommandFinished),
        // NOT a frame-counter delta. Preexec at t=1.000s, finish at
        // t=3.500s → 2500ms, regardless of the frame numbers.
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.set_event_clock_ms(1_000);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "sleep 2.5".into() },
            &mut term,
            10,
        );
        stack.set_event_clock_ms(3_500); // 2.5s of wall-clock later
        stack.observe_lifecycle_event(
            &LifecycleEvent::CommandFinished { exit: 0 },
            &mut term,
            11, // only one frame later
        );
        match stack.iter().next().unwrap() {
            Block::Sealed { duration, .. } => {
                assert_eq!(*duration, std::time::Duration::from_millis(2_500));
            }
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    #[test]
    fn preexec_captures_current_git_into_running_and_seal_retains_it() {
        // 4G capture-at-run-time: the git context current when the
        // command starts (`Preexec`) is frozen into the block header and
        // survives the seal, so a sealed block shows the branch / dirty
        // it actually ran under — not whatever is current later.
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        let git = crate::git_context::GitContext {
            branch: Some("feat/x".into()),
            ahead: 1,
            behind: 0,
            dirty: crate::git_context::DirtySummary {
                files_changed: 2,
                lines_added: 5,
                lines_removed: 1,
            },
        };
        stack.set_current_git(Some(git.clone()));
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "cargo build".into() },
            &mut term,
            10,
        );
        match stack.last().unwrap() {
            Block::Running { header, .. } => {
                assert_eq!(header.git.as_ref(), Some(&git), "Running captures git at Preexec");
            }
            other => panic!("expected Running, got {other:?}"),
        }
        // Change the current git AFTER the command started — the running
        // block must not pick it up (it's frozen at start).
        stack.set_current_git(Some(crate::git_context::GitContext {
            branch: Some("main".into()),
            ..Default::default()
        }));
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 20);
        match stack.iter().next().unwrap() {
            Block::Sealed { header, .. } => {
                assert_eq!(header.git.as_ref(), Some(&git), "Sealed retains the run-time git");
            }
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    #[test]
    fn running_elapsed_is_live_for_running_tail_and_none_otherwise() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        // Fresh Prompt tail: nothing running.
        assert_eq!(stack.running_elapsed_at(9_999), None);
        // Start a command at t=1.000s.
        stack.set_event_clock_ms(1_000);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "sleep 5".into() },
            &mut term,
            10,
        );
        // Live elapsed = now - started.
        assert_eq!(stack.running_elapsed_at(3_500), Some(Duration::from_millis(2_500)));
        // Clock skew clamps to zero.
        assert_eq!(stack.running_elapsed_at(500), Some(Duration::ZERO));
        // After sealing, the tail is a Prompt again → no live elapsed.
        stack.set_event_clock_ms(4_000);
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 11);
        assert_eq!(stack.running_elapsed_at(9_999), None);
    }

    #[test]
    fn seal_clamps_negative_clock_skew_to_zero() {
        // A backwards clock (finish ms < start ms) must not underflow;
        // the duration clamps to zero.
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.set_event_clock_ms(5_000);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "x".into() },
            &mut term,
            10,
        );
        stack.set_event_clock_ms(4_000); // earlier than start — skew
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 11);
        match stack.iter().next().unwrap() {
            Block::Sealed { duration, .. } => assert_eq!(*duration, std::time::Duration::ZERO),
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    #[test]
    fn command_finished_with_nonzero_exit_is_recorded() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "false".into() },
            &mut term,
            0,
        );
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 127 }, &mut term, 1);
        match stack.iter().next().unwrap() {
            Block::Sealed { exit, .. } => assert_eq!(*exit, Some(127)),
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    #[test]
    fn command_finished_without_prior_running_is_ignored() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 5);
        assert_eq!(stack.len(), 1, "stack unchanged");
        assert!(matches!(stack.last().unwrap(), Block::Prompt { .. }));
    }

    #[test]
    fn block_ids_are_monotonic_across_seal_cycles() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        for i in 0..5 {
            stack.observe_lifecycle_event(
                &LifecycleEvent::Preexec { command: format!("cmd{i}") },
                &mut term,
                (i * 2) as u64,
            );
            stack.observe_lifecycle_event(
                &LifecycleEvent::CommandFinished { exit: 0 },
                &mut term,
                (i * 2 + 1) as u64,
            );
        }
        let ids: Vec<BlockId> = stack.iter().map(Block::id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "ids in iter order are monotonic");
        // Five cycles produced five sealed blocks + one tail Prompt.
        assert_eq!(stack.len(), 6);
    }

    // ---- Phase 4G header chrome: cwd tracking ------------------------

    #[test]
    fn precmd_event_populates_prompt_header_cwd() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        // Default header has no cwd.
        match stack.last().unwrap() {
            Block::Prompt { header, .. } => assert!(header.cwd.is_none()),
            _ => panic!("Prompt tail"),
        }
        stack.observe_lifecycle_event(
            &LifecycleEvent::Precmd { cwd: "/Users/tim/code".into() },
            &mut term,
            1,
        );
        match stack.last().unwrap() {
            Block::Prompt { header, .. } => {
                assert_eq!(header.cwd.as_deref(), Some(std::path::Path::new("/Users/tim/code")));
            }
            _ => panic!("Prompt tail"),
        }
    }

    #[test]
    fn cwd_event_updates_tail_header_after_cd_during_prompt() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        // User typed `cd /tmp` in a prior block; shell emits Cwd
        // immediately after to announce the new dir.
        stack.observe_lifecycle_event(&LifecycleEvent::Cwd { cwd: "/tmp".into() }, &mut term, 1);
        match stack.last().unwrap() {
            Block::Prompt { header, .. } => {
                assert_eq!(header.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
            }
            _ => panic!("Prompt tail"),
        }
    }

    #[test]
    fn prompt_cwd_inherits_through_preexec_to_running() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Precmd { cwd: "/Users/tim".into() },
            &mut term,
            1,
        );
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "ls".into() },
            &mut term,
            2,
        );
        match stack.last().unwrap() {
            Block::Running { header, .. } => {
                assert_eq!(header.cwd.as_deref(), Some(std::path::Path::new("/Users/tim")));
            }
            _ => panic!("Running tail"),
        }
    }

    #[test]
    fn running_cwd_carries_into_sealed_at_command_finished() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(&LifecycleEvent::Precmd { cwd: "/x".into() }, &mut term, 1);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "true".into() },
            &mut term,
            2,
        );
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 3);
        // Sealed (first), then new Prompt (tail).
        match stack.iter().next().unwrap() {
            Block::Sealed { header, .. } => {
                assert_eq!(header.cwd.as_deref(), Some(std::path::Path::new("/x")));
            }
            _ => panic!("first should be Sealed"),
        }
    }

    #[test]
    fn cwd_update_during_running_does_not_mutate_the_running_header() {
        // While a command is executing it may print bytes that re-
        // emit a Cwd marker (rare, but possible). The Running header
        // is the cwd at command-start and shouldn't drift; only the
        // *next* Prompt should reflect any new cwd.
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(&LifecycleEvent::Precmd { cwd: "/a".into() }, &mut term, 1);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "any".into() },
            &mut term,
            2,
        );
        stack.observe_lifecycle_event(&LifecycleEvent::Cwd { cwd: "/b".into() }, &mut term, 3);
        match stack.last().unwrap() {
            Block::Running { header, .. } => {
                assert_eq!(
                    header.cwd.as_deref(),
                    Some(std::path::Path::new("/a")),
                    "Running header keeps the cwd that was current at Preexec"
                );
            }
            _ => panic!("Running tail"),
        }
    }

    #[test]
    fn unrelated_lifecycle_events_dont_change_the_stack() {
        let mut stack = fresh_stack();
        let mut term = TerminalState::new(5, 20);
        let before = stack.len();
        for ev in [
            LifecycleEvent::IntegrationReady { shell: ShellKind::Zsh, version: 1 },
            LifecycleEvent::Precmd { cwd: "/tmp".into() },
            LifecycleEvent::Cwd { cwd: "/tmp".into() },
            LifecycleEvent::PromptVars { vars: serde_json::Map::new() },
            LifecycleEvent::IntegrationError { reason: "boom".into() },
            LifecycleEvent::CommandAborted { reason: "ctrl-c".into() },
        ] {
            stack.observe_lifecycle_event(&ev, &mut term, 1);
        }
        assert_eq!(stack.len(), before, "non-command events leave the stack alone");
    }

    #[test]
    fn sealed_block_carries_a_snapshot_of_command_output() {
        let mut term = TerminalState::new(5, 20);
        let mut stack = fresh_stack();

        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "echo hello".into() },
            &mut term,
            10,
        );

        // Bytes produced by the running command.
        term.feed(b"hello\r\n");

        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 11);

        match stack.iter().next().unwrap() {
            Block::Sealed { snapshot, .. } => {
                let joined: String = snapshot
                    .iter()
                    .map(|line| line.text_chars().collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(joined.contains("hello"), "snapshot missing command output: {joined:?}");
            }
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    /// Phase 9B: the sealed snapshot stores width-independent LOGICAL
    /// lines (un-wrapped), not fixed-width grid rows. A line longer than
    /// the terminal width soft-wraps into two grid rows but must seal as
    /// one logical line. Fails on the pre-9B tree, which stored the raw
    /// grid rows.
    #[test]
    fn sealed_snapshot_stores_unwrapped_logical_lines() {
        let mut term = TerminalState::new(5, 10);
        let mut stack = fresh_stack();
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "x".into() },
            &mut term,
            1,
        );
        // 15 chars at width 10 -> alacritty soft-wraps into two rows.
        term.feed(b"0123456789abcde");
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 2);

        match stack.iter().next().unwrap() {
            Block::Sealed { snapshot, .. } => {
                assert_eq!(snapshot.len(), 1, "soft-wrapped output is one logical line");
                let text: String = snapshot[0].text_chars().collect();
                assert_eq!(text, "0123456789abcde");
            }
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    #[test]
    fn with_restored_sealed_preserves_blocks_and_keeps_invariants() {
        let sealed: Vec<Block> = (0..3)
            .map(|i| Block::restored_sealed(BlockId(i), vec![StyledLine::default()], 0, 100))
            .collect();
        let stack = BlockStack::with_restored_sealed(sealed);
        // 3 sealed + 1 fresh Prompt tail.
        assert_eq!(stack.len(), 4);
        let mut it = stack.iter();
        for _ in 0..3 {
            assert!(matches!(it.next().unwrap(), Block::Sealed { .. }));
        }
        // Invariant: last is always live (a Prompt here), with the next id.
        match stack.last().unwrap() {
            Block::Prompt { id, .. } => assert_eq!(*id, BlockId(3)),
            other => panic!("tail must be a live Prompt, got {other:?}"),
        }
    }

    #[test]
    fn seal_returns_snapshot_for_the_writer() {
        // The pane forwards this returned snapshot to the background
        // chunk writer (9D). It must carry the same logical lines stored
        // in the Sealed block, the emit width, and the wall-clock span.
        let mut term = TerminalState::new(5, 10);
        let mut stack = fresh_stack();
        stack.set_event_clock_ms(1000);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "x".into() },
            &mut term,
            1,
        );
        term.feed(b"0123456789abcde"); // 15 chars @ width 10 -> 1 logical line
        stack.set_event_clock_ms(2500);
        let produced = stack
            .observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 2)
            .expect("CommandFinished returns a SealedSnapshot");

        assert_eq!(produced.lines.len(), 1, "soft-wrapped output is one logical line");
        let text: String = produced.lines[0].text_chars().collect();
        assert_eq!(text, "0123456789abcde");
        assert_eq!(produced.emit_cols, 10, "emit width = the grid width at seal");
        assert_eq!((produced.start_time_ms, produced.end_time_ms), (1000, 2500));

        // And it matches what the Sealed block kept for rendering.
        match stack.iter().next().unwrap() {
            Block::Sealed { snapshot, .. } => assert_eq!(&produced.lines, snapshot),
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    #[test]
    fn non_finish_events_return_no_snapshot() {
        // Only CommandFinished produces a snapshot; Preexec / Cwd / etc.
        // return None so the pane has nothing to forward.
        let mut term = TerminalState::new(5, 10);
        let mut stack = fresh_stack();
        assert!(
            stack
                .observe_lifecycle_event(
                    &LifecycleEvent::Preexec { command: "x".into() },
                    &mut term,
                    1
                )
                .is_none(),
            "Preexec produces no snapshot"
        );
        assert!(
            stack
                .observe_lifecycle_event(&LifecycleEvent::Cwd { cwd: "/tmp".into() }, &mut term, 2)
                .is_none(),
            "Cwd produces no snapshot"
        );
    }

    /// The whole-Term-snapshot-then-reset model: after `CommandFinished`,
    /// the live `Term` must be empty so the next block starts clean.
    /// Spec/02 §"The block model: one live Term, many sealed snapshots".
    #[test]
    fn term_is_empty_after_seal() {
        let mut term = TerminalState::new(5, 20);
        let mut stack = fresh_stack();
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "ls".into() },
            &mut term,
            1,
        );
        term.feed(b"a-file\r\nb-file\r\n");
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &mut term, 2);

        let text = term.screen_text();
        for (i, row) in text.lines().enumerate() {
            assert!(
                row.chars().all(|c| c == ' '),
                "row {i} should be blank after seal+reset, got: {row:?}\nfull screen:\n{text}"
            );
        }
        // And of course the next block's content lands at row 0.
        term.feed(b"new-prompt$");
        let text = term.screen_text();
        assert!(
            text.starts_with("new-prompt$"),
            "next block content should land at row 0: {text:?}"
        );
    }
}
