**← Previous:** [03 — Shell integration](03-shell-integration.md) | **Next:** [05 — Pane modes](05-pane-modes.md) →

# 04 — Prompt editor

The headline feature. When a pane is in `ShellPromptEditor` ([05](05-pane-modes.md)), the user is no longer typing "into the shell." They are typing into a native egui editor that sends the **finished command** to the PTY when they press Enter.

## Why this is the entire product

Every other terminal makes the shell line editor (readline / ZLE) own the input experience. That experience is from 1985. It is bad. Termica replaces it with:

- click-to-place cursor;
- drag-to-select; double-click word selection; triple-click line selection;
- system clipboard;
- undo/redo;
- multiline editing;
- syntax highlighting;
- a real history popup with fuzzy search;
- a real completion popup;
- decoration overlays (errors, suggestions later).

…without losing the actual shell underneath.

## The editor model

```rust
pub struct PromptEditor {
    text: String,
    cursor: usize,                // UTF-8 byte index; char-boundary invariant
    selection: Option<TextRange>, // byte-index range; cursor sits at one end
    history_scope: HistoryScope,  // see [07]
    cwd: PathBuf,
    shell: ShellKind,
    completion: Option<CompletionPopup>,
    history_popup: Option<HistoryPopup>,
    syntax: ShellSyntaxState,
    undo: UndoStack,
    dirty_since_history_set: bool,
}
```

All mutations go through a small set of operations (`insert`, `delete`, `move_cursor`, `set_selection`, `submit`, `set_from_history`, `clear`, `undo`, `redo`) so the undo stack and the dirty flag can't drift.

### Cursor / selection invariant

The cursor is a UTF-8 **byte index** that always lies on a `char` boundary. Selections are byte ranges with the same invariant. Use `str::char_indices()` and `floor_char_boundary` / `ceil_char_boundary`; never raw byte arithmetic. (Same rule as the other Rust projects in this org — see [CLAUDE.md](../CLAUDE.md).)

### When is the caret shown?

Single principle: **a visible (and flashing) caret means "your next keypress will be inserted here."** If that statement isn't true, the caret must not be drawn.

The caret is shown for a pane if and only if **all three** of the following hold:

1. The pane is in `ShellPromptEditor` mode (only state with a real editor).
2. The pane currently holds in-app keyboard focus (split-screen: at most one pane per window).
3. The Termica window is the OS's **foreground application** — i.e. the OS will route the next keystroke to us.

If condition 3 is false (the user clicked another app, the window lost focus, the user is in another Space), the caret is hidden across **every** pane in the window. Showing a flashing caret in our window while keypresses land elsewhere is the exact UX mismatch this rule eliminates.

The same principle applies to the raw-terminal cell cursor in [02](02-terminal-engine.md): when the app is not foreground the cell cursor renders dim / hollow, not as a blinking solid block, because keypresses aren't reaching the PTY either.

eframe surfaces foreground state via `egui::ViewportInfo::focused` (the boolean is true exactly when the OS window is key/active). Read it through `ctx.input(|i| i.viewport().focused.unwrap_or(true))` and treat `None` as "assume foreground" so a backend that doesn't report the flag never silently hides the caret.

A separate **focused-but-not-editing-yet** visual (e.g. a subtle rounded indicator over the editor area) is allowed and complementary to the caret rule: it says "this pane is wired to receive keypresses if you start typing," even though the editor isn't currently in `ShellPromptEditor` mode or hasn't grown a caret yet. The caret itself still follows the three conditions above.

Tests:

- Unit: pure boolean `should_show_caret(mode, has_focus, viewport_focused) -> bool`. Cover every cell of the 2×2×2 cube.
- Snapshot: prompt editor with (focused + foreground) renders the caret; prompt editor with (focused + NOT foreground) renders without the caret. Same fixture otherwise so the only pixel delta is the caret.

### Editing keystrokes (in `ShellPromptEditor`)

Standard text-editor mapping, OS-aware. macOS uses `Option`/`Cmd`; Linux/Windows uses `Ctrl`. The `Shift` modifier on any motion below extends the current selection from the existing anchor to the new cursor position rather than collapsing it.

| Action | macOS | Linux / Windows |
|---|---|---|
| Move cursor one char | Arrow ← / → | Arrow ← / → |
| Move cursor one line | Arrow ↑ / ↓ | Arrow ↑ / ↓ |
| Move to line start / end | `Home` / `End` (also `Cmd + ←` / `Cmd + →`) | `Home` / `End` |
| Move by word | `Option + ← / →` | `Ctrl + ← / →` |
| Move to document start / end | `Cmd + ↑` / `Cmd + ↓` | `Ctrl + Home` / `Ctrl + End` |
| Select all | `Cmd + A` | `Ctrl + A` |
| Undo / redo (Phase 4 polish) | `Cmd + Z` / `Cmd + Shift + Z` | `Ctrl + Z` / `Ctrl + Shift + Z` |
| Copy / paste / cut | `Cmd + C / V / X` | `Ctrl + C / V / X` |
| Delete / Backspace | Per-grapheme delete (not per-byte) — same on both | same |
| Enter | Submit (see below) | same |
| Shift + Enter | Insert newline (multiline) | same |
| Tab | Local completion popup ([Phase 4I](10-roadmap.md#phase-4--editor-at-prompt-block-model-pivot)) | same |
| Ctrl + R | History popup (fuzzy search; [Phase 4J](10-roadmap.md#phase-4--editor-at-prompt-block-model-pivot)) | same |
| Up / Down | History walk (pane-local first), unless completion popup is open | same |
| Esc | Dismiss popup; if no popup, leave editor → demote to `RawTerminal` | same |
| Ctrl + C on empty editor | Send SIGINT to PTY (terminal-mode parity) | same |
| Ctrl + D on empty editor | Send EOF to PTY (terminal-mode parity) | same |

The matching matrix lives in [`classify_editor_motion`](../src/render_pane.rs); both branches are unit-tested with the `is_macos` flag flipped explicitly so each OS's convention is verified on every CI run, not only on the host that runs CI. New motion keys are added there first, with tests, before any new row appears above.

Shell-binding keys (`Ctrl+R`, `Ctrl+P`, `Ctrl+N`, `Ctrl+S`, `Ctrl+G`) are **consumed without effect** in the editor today: they don't reach the PTY (the editor swallows them) and they don't fire any app behaviour either, until [4J](10-roadmap.md#phase-4--editor-at-prompt-block-model-pivot) ships history walk and Ctrl+R popup. This is deliberate: forwarding them would leak literal `^R` glyphs into the editor while the user's muscle memory hasn't been wired up yet.

`Ctrl+L` is a config decision: clear visible transcript ("editor convention") or send the bytes (`\f`) to the shell ("terminal convention"). Default: clear visible transcript, configurable.

### Mouse in the editor

| Gesture | Effect |
|---|---|
| Click | Place caret; clear selection |
| Drag | Extend selection by **character** |
| Double-click | Select the word under the pointer |
| Double-click + drag | Extend selection by **word**: each word the pointer enters is added to (or trimmed from) the selection; the anchor is the original double-clicked word, so the selection always covers `min(anchor_start, current_start) … max(anchor_end, current_end)` |
| Triple-click | Select the line under the pointer |
| Triple-click + drag | Same as double-click + drag but by **line** |

The word / line range helpers (`word_range_at`, `line_range_at` in [`src/prompt_editor.rs`](../src/prompt_editor.rs)) are pure functions, unit-tested, and shared between the press handler and the drag handler so single-click → drag and double/triple-click → drag agree on what a "word" or a "line" is. The per-pane anchor range (`PaneUiState::editor_drag_anchor`) is cleared on a single click so the next drag starts in character mode again.

### Multiline editing

Multiline is first-class. `Shift+Enter` inserts a newline. Enter submits. The renderer wraps soft lines at the pane's cell width and shows a continuation gutter glyph.

For multi-line commands sent to a shell that supports them (`{}` block, here-doc), the whole buffer is sent verbatim followed by a final newline. We do not auto-quote, auto-escape, or pre-validate shell syntax — that's the shell's job.

## Submission semantics (Enter)

This is the single most subtle operation in the codebase. The order matters.

```rust
fn submit(pane: &mut Pane) {
    let text = std::mem::take(&mut pane.editor.text);

    // 1. Eagerly demote BEFORE writing to PTY. From this instant the
    //    editor is closed; any further keystrokes go to the PTY.
    pane.prompt.transition_to(Mode::RawTerminal, Reason::EnterSubmitted);

    // 2. Open a command run record (cwd/shell/text/start_time) and pin
    //    its id; it stays open until the matching command_end marker.
    let cmd_id = pane.command_runs.begin(text.clone(), pane.context.snapshot());

    // 3. Prime echo suppression with the bytes about to be sent.
    pane.echo_suppress.expect(&text);

    // 4. Send the command. CRLF or LF per terminal mode.
    pane.pty.write_bytes(text.as_bytes());
    pane.pty.write_bytes(line_terminator(&pane.terminal));

    // 5. Record in pane-local and global history.
    pane.history.record(&text, &pane.context);
    pane.global_history.record(&text, &pane.context, cmd_id);

    // 6. Clear undo + completion state.
    pane.editor.reset_after_submit();
}
```

The eager demotion in step 1 is the safety invariant: if the user immediately mashes Ctrl-C, it MUST reach the running command, not the editor.

### Multi-line continuation (PS2 marker)

When the user presses Enter on a command the shell considers incomplete (`echo 1 &&` → shell wants more; here-docs; unclosed quotes), the shell prints its **continuation prompt** (`PS2` in bash/zsh) and stays in line-reading mode. Termica's integration script overrides `PS2` to emit a DCS-JSON **continuation marker** instead of the conventional `>` glyph:

```jsonc
// emitted by the shell whenever PS2 fires
{ "type": "continuation", "session": "<id>" }
```

The parser surfaces this as `LifecycleEvent::Continuation` ([`src/markers.rs`](../src/markers.rs)). On reception the `PromptController` calls `try_promote_to_editor_for_continuation`, which re-promotes to `ShellPromptEditor` **only if** `last_transition.reason == EnterSubmitted` — i.e. the immediately-preceding demote came from a real submit, not from an `Esc` or any other path. (If the user explicitly `Esc`'d out of the editor, a continuation marker must not yank them back in.)

When the editor reopens, the `PaneSession` restores the editor's text to `last_submitted + "\n"` and places the caret at the end so the user can resume typing on the next line. This effectively turns the "shell wanted more" path into "we hand the editor back, now multi-line."

The submit path is **suffix-only on the second submit**:

- `submit_editor_command` remembers the full text in `last_submitted: Option<String>`.
- On the next submit, if the new editor text begins with `last_submitted`, only the **suffix beyond it** is written to the PTY (with a leading `\n` stripped, since the restore added one). The shell already received the prefix on the first submit; resending it would duplicate every line, and the `EchoSuppressor` (which was primed for the first half) would mismatch and disengage.
- `last_submitted` is cleared on the next `Preexec` lifecycle event — i.e., the shell has actually started executing a complete command, so the multi-line dance is over.

`EchoSuppressor::expect` treats both `\n` and `\r` in the expected stream as `\r\n` because the kernel's tty discipline applies ONLCR to **every** echoed newline, not only the trailing one. Without this rule the second multi-line submit would have a partially-matching prefix and disengage suppression mid-stream, leaking the second segment as duplicate echo into the running block.

The two PR references for this surface area are [#58](https://github.com/enthal/termica/pull/58) (continuation event + restore) and [#54](https://github.com/enthal/termica/pull/54) (the underlying eager-demote + echo-suppression scaffolding).

## Echo handling

When Termica writes `git status\n` to the PTY, the shell's tty discipline echoes those bytes back. Three options were considered; v1 takes (b).

| Option | Description | Verdict |
|---|---|---|
| (a) `stty -echo` while in `ShellPromptEditor` | Mutates terminal state on every transition. Restored on demote. Clean but invasive; interacts badly with shell-level overrides. | Rejected — too easy for the state to leak |
| **(b) Pending-send buffer with prefix-match echo suppression** | When we send N bytes, we record them in a small ring buffer. The terminal layer's "about-to-grid" hook checks each incoming byte against the head of this buffer; matches are consumed (and the grid never sees them). On mismatch (or after a timeout — 500 ms), the buffer is discarded and bytes pass through as normal. | **v1 choice** |
| (c) Live with the duplicate echo | Render command in the structured command-block UI, accept the duplicate in the raw transcript. | Rejected — looks unprofessional, fights with command-block UI |

### Echo suppression rules

- The buffer holds at most one pending submission. A second submit before the buffer drains is impossible because the editor is closed during raw mode.
- A timeout (500 ms) ensures that if the shell is slow or never echoes (rare; e.g. `stty -echo` was set by user), suppression silently disengages.
- The suppression buffer matches the **bytes**, not characters, so multi-byte UTF-8 works.
- A mismatch immediately disengages — never half-suppress.
- The suppression hook lives **above** `alacritty_terminal`'s parser, so the bytes never reach the grid in the first place. (See [02](02-terminal-engine.md).)

Echo suppression is one of the strict-layer tests ([CLAUDE.md](../CLAUDE.md)): write a failing test first.

## Tab handling

Tab is local completion. It does **not** send `\t` to the PTY. Sources:

1. **Path completion** for the token under the cursor that looks like a path or starts with `/`, `./`, `../`, or `~/`.
2. **Command history** for the entire buffer when cursor is past whitespace (suggests previous matching commands).
3. **`$PATH` executable lookup** for the first token.

Ranking: prefer recent / same-cwd / non-zero-exit-history matches. The popup is a native egui widget with arrow-key navigation and Tab/Enter to accept.

Deep shell completion (`compgen`, ZLE menu) is **post-MVP** ([10](10-roadmap.md)). The argument: most users have moved completion guidance into their own muscle memory; we ship a competent local default and revisit if real-world usage shows it's insufficient.

## Syntax highlighting

A minimal shell tokenizer in-house for v1:

- commands (first token, highlighted as command kind)
- strings (single-quote / double-quote, with embedded `$var` inside double-quote)
- variables (`$NAME`, `${expr}`)
- pipes / redirects (`|`, `>`, `>>`, `<`, `&`)
- subshells (`$(...)`, `` `...` ``)
- flags (`-x`, `--long`)
- comments (`#`)

Tree-sitter is overkill for v1. We will reach for it if and when the in-house tokenizer can't keep up with what we want to highlight.

## History popup

`Ctrl+R` opens a popup over the editor:

- Default scope: global history, filtered by current cwd → broader.
- Fuzzy matcher (e.g. `nucleo` or `skim`'s matcher) ranks by score, recency, and cwd proximity.
- Arrow keys / `Tab` to walk results; `Enter` to accept (replaces editor buffer); `Esc` to dismiss.
- Scope toggles (this pane / this project / global) in the popup chrome.

See [07](07-history-and-search.md) for the history storage and scopes.

## What the editor never does

- **Never** sends a keystroke to the PTY while in `ShellPromptEditor` mode, except via `submit()`, Ctrl+C, or Ctrl+D edge cases above.
- **Never** writes to history mid-edit. Recording happens at submit only.
- **Never** auto-corrects or auto-completes silently. The user always confirms.
- **Never** opens or closes itself based on heuristics. The `PromptController` ([05](05-pane-modes.md)) owns visibility.

## Visual structure: the block model

The pane is **not** one big cell grid. It is a vertical stack of **blocks**, each block being one command + its decorations + its output area. The user reads / scrolls / selects through the stack; only the bottom block is "live" at any moment.

### The three block states

A block is one of three states, decided by the `PromptController` ([05](05-pane-modes.md)):

```rust
pub enum Block {
    /// Shell is idle at a prompt. Editor active inside; chips
    /// above the editor; lives glued to the viewport bottom.
    Prompt { editor: PromptEditor, header: BlockHeader, started_at: Instant },

    /// A command is executing. Dim header line with live duration
    /// timer; bold command line; output streams in below.
    Running { live_grid: TerminalState, header: BlockHeader, command: String, started_at: Instant },

    /// Command finished. Frozen snapshot of styled lines, dim
    /// header line with final duration + exit code; bold command.
    Sealed {
        header: BlockHeader,
        command: String,
        snapshot: Vec<StyledLine>,
        duration: Duration,
        exit: Option<i32>,
    },
}

pub struct BlockHeader {
    cwd: PathBuf,
    git_branch: Option<String>,
    git_dirty: Option<DirtySummary>,    // 1 • +3 -0 etc.
}

pub struct DirtySummary {
    files_changed: u32,
    lines_added: u32,
    lines_removed: u32,
}
```

A `PaneSession` owns `Vec<Block>` plus an `active: Option<BlockId>` pointing at the live one (always the last; `None` very briefly between command_finished and the next precmd).

### Visual structure (three captures' worth of UI)

Each block paints differently per state:

```
┌─────────────────────────── Sealed ─────────────────────────────┐
│ ~/git/enthal/termica git:(main) 1 • +3 -0 (0.034s)             │  ← dim header line
│ git status                                                     │  ← bold command
│ On branch main                                                 │  ← frozen output
│ Your branch is up to date with 'origin/main'.                  │
└────────────────────────────────────────────────────────────────┘
┌─────────────────────────── Running ────────────────────────────┐
│ ~/git/enthal/termica git:(main) 1 • +3 -0 (11s)                │  ← dim header, live duration
│ while true; do sleep 1; date; done                             │  ← bold command, frozen
│ Tue May 26 10:07:52 PDT 2026                                   │  ← live output
│ Tue May 26 10:07:53 PDT 2026                                   │
│ ▌                                                              │  ← running-cursor glyph
└────────────────────────────────────────────────────────────────┘
┌─────────────────────────── Prompt ─────────────────────────────┐  ← glued to viewport bottom
│ [📁 ~/git/enthal/termica] [ main] [1 • +3]                     │  ← decoration chips
│ ❯ git status_                                                  │  ← editor (multiline expands here)
└────────────────────────────────────────────────────────────────┘
```

The chrome between blocks is non-text (thin separator + space). Selection (below) passes through it; copy-to-clipboard does not include it.

The `❯` and the chips are decorative — painted by Termica, not the shell's `PS1`. The integration script intentionally minimises `PS1` so the shell's own prompt drawing doesn't visually conflict with Termica's chrome ([03](03-shell-integration.md)).

## Layout: fixed-footer prompt, sticky-top header

Three rules govern the per-frame layout pass:

1. **The `Prompt` block is a fixed footer.** It's glued to the bottom of the pane viewport and does not scroll. Its height varies with the editor's line count (multiline grows it down; the scrollable area shrinks correspondingly). When the bottom block is `Running` or there is no bottom block, the footer area is unused and the scroll area extends to the pane bottom.

2. **Older blocks scroll under the footer.** The remaining vertical (pane height minus footer height) is the scroll area. Mouse-wheel / arrow-key scroll moves blocks within this area. Blocks above the viewport top are clipped; the scroll position is pane-level, not per-block.

3. **The top-most partially-visible block's header pins to the top edge.** If a block's body is visible but its header has scrolled above the viewport, the renderer paints that header pinned to the top edge of the scroll area for as long as the block's body intersects the viewport. Only one sticky header is shown at a time; once the body fully scrolls past, the *next* block's header takes its place. This is the same affordance iOS section headers use.

The layout helper that decides which block is "sticky-eligible" and computes the inner-block scroll offset is the unit-testable boundary: pure math, no egui dependency, covered by tests that walk scroll positions through a synthetic block list.

## Cross-block selection

Selection coordinates are **pane-level**, not grid-level:

```rust
pub struct PaneSelection {
    pub anchor: PaneCursor,
    pub head:   PaneCursor,
    pub mode:   SelectionMode,   // Char / Word / Line
}

pub struct PaneCursor {
    pub block_id: BlockId,
    pub line:     usize,         // line within the block's content (0-indexed)
    pub col:      usize,         // grapheme cluster index on that line
}
```

When the user drags across a block boundary, the selection logically spans all text content between anchor and head. Each block translates "is this cell within my piece of the selection?" to a per-block highlight. Block chrome (header + separator) is visually unhighlighted even when the selection passes over it.

`Selection → text` walks blocks in anchor→head order, concatenating each block's `command + output` slice with a separating newline between blocks. Chrome is not included.

## Resize: sealed blocks don't reflow

Per-pane resize is asymmetric:

- **`Running` and `Prompt` blocks** track the pane's current cell width, exactly like a normal terminal would. The live alacritty grid (in `Running`) re-flows; the editor (in `Prompt`) re-wraps.
- **`Sealed` blocks keep their original cell width.** Their `Vec<StyledLine>` snapshot is frozen at the width the command saw. Resize is cheap (no re-rendering thousands of stored lines) and matches iTerm / Warp behaviour.

A sealed block narrower than the current pane is left-aligned within its strip. A sealed block wider than the current pane horizontally scrolls (or hard-truncates — TBD by Phase 8's polish pass).

The editor model and submit semantics described earlier in this document apply to the editor that lives **inside** the `Prompt` block. Everything else (the live cell grid, the sealed snapshots) belongs to the surrounding block infrastructure.

## Testing

- **Unit (strict)**: every `PromptEditor` operation has a unit test asserting cursor / selection / undo / dirty-flag state. `classify_editor_motion` is tested with both `is_macos = true` and `is_macos = false`. `word_range_at` and `line_range_at` are tested independently so the press and drag handlers can rely on the same boundary rules.
- **Unit (strict)**: `submit()` ordering — eager mode transition occurs before PTY write; echo suppression is primed before the write; history recording happens after. The continuation flow (`Continuation` lifecycle event re-promotes only when the prior reason was `EnterSubmitted`; restored text is `last_submitted + "\n"`; subsequent submit sends only the suffix beyond `last_submitted`) is covered by its own strict-layer tests in [`src/pane.rs`](../src/pane.rs) and [`src/shell.rs`](../src/shell.rs).
- **Snapshot (egui_kittest)**: editor at various states (empty, mid-edit, with full selection, with partial selection, with multiline selection, with completion popup, multiline command) renders deterministically. Sealed-block paint (command label + frozen output) and prompt-editor caret (blinking 2px line caret in `EDITOR_CURSOR_COLOR`) have their own snapshot tests.
- **Integration**: with a real shell, submit a command and assert the duplicate echo never appears in the transcript. Multi-line commands (`echo 1 &&` → `echo 2`) assert that the second submit sends only the suffix and the sealed block contains no duplicate prefix.

---

**← Previous:** [03 — Shell integration](03-shell-integration.md) | **Next:** [05 — Pane modes](05-pane-modes.md) →
