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
        started_at_frame: u64,
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
        stack.blocks.push(Block::Prompt {
            id,
            header: BlockHeader::default(),
            started_at_frame,
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
    pub fn observe_lifecycle_event(
        &mut self,
        event: &LifecycleEvent,
        terminal: &mut TerminalState,
        frame: u64,
    ) {
        match event {
            LifecycleEvent::Preexec { command } => self.start_running(command.clone(), frame),
            LifecycleEvent::CommandFinished { exit } => self.seal_running(*exit, terminal, frame),
            LifecycleEvent::Precmd { cwd } | LifecycleEvent::Cwd { cwd } => {
                self.update_tail_cwd(cwd.clone());
            }
            _ => {}
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
    fn start_running(&mut self, command: String, frame: u64) {
        let Some(tail) = self.blocks.last_mut() else { return };
        if !matches!(tail, Block::Prompt { .. }) {
            // Already running, or sealed (impossible by invariant
            // since the last is always live). Drop the event; the
            // shell is in an unexpected state and we'd rather lose
            // a block boundary than corrupt the stack.
            return;
        }
        let (id, header) = match tail {
            Block::Prompt { id, header, .. } => (*id, header.clone()),
            _ => unreachable!(),
        };
        // The editor's content is implicitly discarded here. In
        // Phase 4C, `submit_command` will have moved the text out
        // before `Preexec` arrives; for 4B with no submit wired yet,
        // any half-typed buffer is dropped — same as if the user had
        // pressed Esc and typed the command through the PTY instead.
        *tail = Block::Running { id, header, command, started_at_frame: frame };
    }

    /// `CommandFinished` arrived. If the tail is `Running`, snapshot
    /// the whole live `Term`, seal the tail, reset the `Term` for the
    /// next block, and push a fresh `Prompt`. If the tail is `Prompt`
    /// (a stray finish with no prior `Preexec`), ignore: there's no
    /// command to seal.
    ///
    /// Order matters: snapshot first, then reset. Otherwise the
    /// sealed snapshot is empty.
    fn seal_running(&mut self, exit: i32, terminal: &mut TerminalState, frame: u64) {
        let Some(tail) = self.blocks.last() else { return };
        let Block::Running { id, header, command, started_at_frame } = tail else {
            return;
        };
        let id = *id;
        let header = header.clone();
        let command = command.clone();
        let started = *started_at_frame;

        let snapshot = terminal.snapshot_lines_all();
        terminal.reset_for_new_block();
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
        self.blocks.push(Block::Prompt {
            id: new_id,
            header,
            started_at_frame: frame,
            editor: PromptEditor::new(),
        });
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
        stack.observe_lifecycle_event(
            &LifecycleEvent::Preexec { command: "ls -la".into() },
            &mut term,
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
