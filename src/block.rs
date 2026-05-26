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
//!   `Prompt` transforms into `Running`, carrying the command string
//!   and the line offset at which the command's output begins.
//! - When [`LifecycleEvent::CommandFinished`] arrives, the live tail's
//!   `Running` seals: its output (line range captured at start to the
//!   current grid total-lines count) is snapshotted as a
//!   `Vec<StyledLine>`, the exit code and duration are recorded, and
//!   a fresh `Prompt` block is pushed.
//!
//! ## Why this module is self-contained
//!
//! `BlockStack::observe_lifecycle_event` takes a [`TerminalState`]
//! reference so it can read line offsets and capture snapshots, but
//! it never touches the renderer, never assumes egui, and never
//! mutates state outside its own `Vec<Block>` (and the snapshot
//! capture it reads). This is the strict-layer engine surface; tests
//! drive it without a PTY.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use crate::markers::LifecycleEvent;
use crate::terminal::{StyledLine, TerminalState};

/// Per-pane monotonically increasing identifier for blocks. New
/// values come from [`BlockStack::next_id`]; never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u64);

/// Information about the shell environment at the moment a block
/// begins. Phase 4A populates only `cwd`; git branch / dirty
/// summary land in [4G](../spec/10-roadmap.md).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockHeader {
    pub cwd: Option<PathBuf>,
}

/// One block in the per-pane stack. The variants encode the three
/// lifecycle states described in [spec/04](../spec/04-prompt-editor.md):
///
/// - [`Block::Prompt`] — the shell is idle at a prompt. The live
///   `Term` belongs to this block until [`LifecycleEvent::Preexec`]
///   arrives.
/// - [`Block::Running`] — a command is executing. The same live
///   `Term` continues to be fed. `output_start_line` records the
///   grid total-line count at `Preexec` so the seal snapshot can
///   slice exactly the lines this command produced.
/// - [`Block::Sealed`] — the command has finished and its output is
///   a frozen `Vec<StyledLine>` snapshot. No live state remains in
///   the block.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Prompt {
        id: BlockId,
        header: BlockHeader,
        /// Frame counter at which the block was created (the same
        /// monotonic counter the [`crate::shell::PromptController`]
        /// uses for transitions).
        started_at_frame: u64,
    },
    Running {
        id: BlockId,
        header: BlockHeader,
        command: String,
        started_at_frame: u64,
        /// Total-line count in the live `Term`'s grid at the moment
        /// `Preexec` arrived. Used at seal time to slice "just the
        /// lines this command produced."
        output_start_line: usize,
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
    /// Stable id, regardless of variant.
    pub fn id(&self) -> BlockId {
        match self {
            Block::Prompt { id, .. } | Block::Running { id, .. } | Block::Sealed { id, .. } => *id,
        }
    }
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
}

impl BlockStack {
    /// Build a fresh stack containing exactly one [`Block::Prompt`]
    /// with id `0`. The shell hasn't run anything yet, but the
    /// pane is conceptually "at a prompt about to be drawn."
    pub fn new(started_at_frame: u64) -> Self {
        let mut stack = Self { blocks: Vec::with_capacity(8), next_id: 0 };
        let id = stack.alloc_id();
        stack.blocks.push(Block::Prompt { id, header: BlockHeader::default(), started_at_frame });
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

    /// Borrow the live tail block. Always present by invariant; only
    /// returns `Option` so the API stays defensive in case a future
    /// caller manages to clear the stack out from under us (it
    /// shouldn't, but we don't want to `panic!` here).
    pub fn last(&self) -> Option<&Block> {
        self.blocks.last()
    }

    /// Apply one lifecycle event from the [`crate::shell::PromptController`].
    /// `terminal` is read at the seal call so the snapshot reflects
    /// the grid state at the instant `CommandFinished` was parsed.
    ///
    /// Events that don't drive a block transition (e.g. `Precmd`,
    /// `IntegrationReady`, `PromptVars`, `Cwd`) are ignored here —
    /// they are the controller's concern, not the stack's. The
    /// stack tracks the *command-run* lifecycle, which is just
    /// `Preexec` → `CommandFinished`.
    pub fn observe_lifecycle_event(
        &mut self,
        event: &LifecycleEvent,
        terminal: &TerminalState,
        frame: u64,
    ) {
        match event {
            LifecycleEvent::Preexec { command } => {
                self.start_running(command.clone(), terminal, frame)
            }
            LifecycleEvent::CommandFinished { exit } => self.seal_running(*exit, terminal, frame),
            _ => {}
        }
    }

    /// `Preexec` arrived. If the tail is a `Prompt`, transform it
    /// into a `Running` carrying the command string and the current
    /// total-line offset. If the tail is already `Running` (e.g.
    /// a stray `Preexec` from a nested context — see
    /// [spec/03 §"Nested shells"](../spec/03-shell-integration.md)),
    /// ignore: only one command can be running at a time per pane.
    fn start_running(&mut self, command: String, terminal: &TerminalState, frame: u64) {
        let Some(tail) = self.blocks.last_mut() else { return };
        if !matches!(tail, Block::Prompt { .. }) {
            // Already running, or sealed (impossible by invariant
            // since the last is always live). Drop the event; the
            // shell is in an unexpected state and we'd rather lose
            // a block boundary than corrupt the stack.
            return;
        }
        let (id, header, _) = match tail {
            Block::Prompt { id, header, started_at_frame } => {
                (*id, header.clone(), *started_at_frame)
            }
            _ => unreachable!(),
        };
        *tail = Block::Running {
            id,
            header,
            command,
            started_at_frame: frame,
            output_start_line: terminal.total_lines_seen(),
        };
    }

    /// `CommandFinished` arrived. If the tail is `Running`, snapshot
    /// the lines produced since `Preexec`, seal it, and push a fresh
    /// `Prompt` block. If the tail is `Prompt` (a stray finish with
    /// no prior `Preexec`), ignore: there's no command to seal.
    fn seal_running(&mut self, exit: i32, terminal: &TerminalState, frame: u64) {
        let Some(tail) = self.blocks.last() else { return };
        let Block::Running { id, header, command, started_at_frame, output_start_line } = tail
        else {
            return;
        };
        let id = *id;
        let header = header.clone();
        let command = command.clone();
        let started = *started_at_frame;
        let start_line = *output_start_line;

        let snapshot = terminal.snapshot_lines_since(start_line);
        let duration = Duration::from_secs(frame.saturating_sub(started));

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
        self.blocks.push(Block::Prompt { id: new_id, header, started_at_frame: frame });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::ShellKind;

    fn fresh_stack() -> BlockStack {
        BlockStack::new(0)
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
        let term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "ls -la".into() },
            &term,
            10,
        );
        assert_eq!(stack.len(), 1);
        match stack.last().unwrap() {
            Block::Running { id, command, started_at_frame, .. } => {
                assert_eq!(*id, BlockId(0), "id preserved across Prompt → Running");
                assert_eq!(command, "ls -la");
                assert_eq!(*started_at_frame, 10);
            }
            other => panic!("expected Running after Preexec, got {other:?}"),
        }
    }

    #[test]
    fn preexec_when_tail_already_running_is_ignored() {
        let mut stack = fresh_stack();
        let term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "first".into() },
            &term,
            10,
        );
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "second-should-be-ignored".into() },
            &term,
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
        let term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "echo hi".into() },
            &term,
            10,
        );
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &term, 20);

        assert_eq!(stack.len(), 2, "sealed + new prompt");
        // First (older) is sealed.
        match stack.iter().next().unwrap() {
            Block::Sealed { id, command, exit, duration, .. } => {
                assert_eq!(*id, BlockId(0));
                assert_eq!(command, "echo hi");
                assert_eq!(*exit, Some(0));
                assert_eq!(duration.as_secs(), 10, "20 - 10 frames");
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
    fn command_finished_with_nonzero_exit_is_recorded() {
        let mut stack = fresh_stack();
        let term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "false".into() },
            &term,
            0,
        );
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 127 }, &term, 1);
        match stack.iter().next().unwrap() {
            Block::Sealed { exit, .. } => assert_eq!(*exit, Some(127)),
            other => panic!("expected Sealed, got {other:?}"),
        }
    }

    #[test]
    fn command_finished_without_prior_running_is_ignored() {
        let mut stack = fresh_stack();
        let term = TerminalState::new(5, 20);
        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &term, 5);
        assert_eq!(stack.len(), 1, "stack unchanged");
        assert!(matches!(stack.last().unwrap(), Block::Prompt { .. }));
    }

    #[test]
    fn block_ids_are_monotonic_across_seal_cycles() {
        let mut stack = fresh_stack();
        let term = TerminalState::new(5, 20);
        for i in 0..5 {
            stack.observe_lifecycle_event(
                &LifecycleEvent::Preexec { command: format!("cmd{i}") },
                &term,
                (i * 2) as u64,
            );
            stack.observe_lifecycle_event(
                &LifecycleEvent::CommandFinished { exit: 0 },
                &term,
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

    #[test]
    fn unrelated_lifecycle_events_dont_change_the_stack() {
        let mut stack = fresh_stack();
        let term = TerminalState::new(5, 20);
        let before = stack.len();
        for ev in [
            LifecycleEvent::IntegrationReady { shell: ShellKind::Zsh, version: 1 },
            LifecycleEvent::Precmd { cwd: "/tmp".into() },
            LifecycleEvent::Cwd { cwd: "/tmp".into() },
            LifecycleEvent::PromptVars { vars: serde_json::Map::new() },
            LifecycleEvent::IntegrationError { reason: "boom".into() },
            LifecycleEvent::CommandAborted { reason: "ctrl-c".into() },
        ] {
            stack.observe_lifecycle_event(&ev, &term, 1);
        }
        assert_eq!(stack.len(), before, "non-command events leave the stack alone");
    }

    #[test]
    fn sealed_block_carries_a_snapshot_of_lines_produced_during_run() {
        // Plant the previous prompt on row 0 and `\r\n` so the cursor
        // sits at row 1 when Preexec fires — that's the typical shell
        // shape (the user pressed Enter, the shell emitted a newline
        // before invoking the preexec hook). The sealed snapshot must
        // include the command's output but not the prior prompt on
        // row 0.
        let mut term = TerminalState::new(5, 20);
        term.feed(b"old-prompt$ \r\n");
        let mut stack = fresh_stack();

        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "echo hello".into() },
            &term,
            10,
        );

        // Now bytes produced *by the command*.
        term.feed(b"hello\r\n");

        stack.observe_lifecycle_event(&LifecycleEvent::CommandFinished { exit: 0 }, &term, 11);

        match stack.iter().next().unwrap() {
            Block::Sealed { snapshot, .. } => {
                // The snapshot should contain "hello" somewhere, and
                // should NOT contain "old-prompt$".
                let joined: String = snapshot
                    .iter()
                    .map(|line| line.text_chars().collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(joined.contains("hello"), "snapshot missing command output: {joined:?}");
                assert!(
                    !joined.contains("old-prompt$"),
                    "snapshot leaked pre-Preexec content: {joined:?}"
                );
            }
            other => panic!("expected Sealed, got {other:?}"),
        }
    }
}
