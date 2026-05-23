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

### Editing keystrokes (in `ShellPromptEditor`)

Standard text-editor mapping, OS-aware:

| Key | macOS / Linux |
|---|---|
| Arrow / Home / End | Move cursor; with Shift extends selection |
| Cmd/Ctrl + Arrow | Word / line jumps |
| Cmd/Ctrl + A | Select all |
| Cmd/Ctrl + Z / Shift+Z | Undo / redo |
| Cmd/Ctrl + C / V / X | Copy / paste / cut |
| Delete / Backspace | Per-grapheme delete (not per-byte) |
| Enter | Submit (see below) |
| Shift+Enter | Insert newline (multiline) |
| Tab | Local completion popup |
| Ctrl+R | History popup (fuzzy search) |
| Up / Down | History walk (pane-local first), unless completion popup is open |
| Esc | Dismiss popup; if no popup, no-op |
| Ctrl+C on empty editor | Send SIGINT to PTY (terminal-mode parity) |
| Ctrl+D on empty editor | Send EOF to PTY (terminal-mode parity) |

`Ctrl+L` is a config decision: clear visible transcript ("editor convention") or send the bytes (`\f`) to the shell ("terminal convention"). Default: clear visible transcript, configurable.

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

## Visual structure

```
┌─────────────────────────────────────────────────────────────────┐
│ [📂 ~/git/enthal/termica] [⎇ main] [±1] [✓ 0 in 2.3s]           │  ← status header [06]
├─────────────────────────────────────────────────────────────────┤
│ ❯ cargo test --workspace_                                       │  ← editor (when ShellPromptEditor)
│                                                                 │
│   ┌─────────────────────────────────────────────────────────┐   │
│   │ Phase 1 transcript                                      │   │
│   │ (last command's output, ANSI-styled, search-able)       │   │
│   └─────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

The `❯` is decorative. It is painted by Termica; it is **not** part of the editor text. The status header is a structured widget, not a `PS1` string ([06](06-workspace-and-tiles.md)).

## Testing

- **Unit (strict)**: every `PromptEditor` operation has a unit test asserting cursor / selection / undo / dirty-flag state.
- **Unit (strict)**: `submit()` ordering — eager mode transition occurs before PTY write; echo suppression is primed before the write; history recording happens after.
- **Snapshot (egui_kittest)**: editor at various states (empty, mid-edit, with selection, with completion popup, multiline) renders deterministically.
- **Integration**: with a real shell, submit a command and assert the duplicate echo never appears in the transcript.

---

**← Previous:** [03 — Shell integration](03-shell-integration.md) | **Next:** [05 — Pane modes](05-pane-modes.md) →
