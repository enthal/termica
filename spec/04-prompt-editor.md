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

**For renderer code that translates byte indices into column counts**, use the safe-slice helpers rather than direct slicing:

- [`crate::render::chars_before_byte`] returns the char count of the prefix up to a given byte, with boundary-down degradation for any input (past-end, mid-multi-byte). Never panics.
- For loading a substring to render, use `str::get(..)` (returns `Option<&str>`) instead of `&s[..]`. The `None` path degrades to "skip this paint" rather than panicking the whole frame.

The renderer panicked twice during the Phase 4 polish push for exactly this reason — once on multi-row editor selection (selection start byte fell past one of the rows' lengths), once on the syntax-token paint loop next to it. Both used `line.text[..n].chars().count()` with `n` computed from cross-row byte arithmetic that wasn't guaranteed in-bounds. `chars_before_byte` is the structurally safe replacement; the renderer no longer indexes `&str` by raw byte. See [`src/render.rs::paint_prompt_editor_at`](../src/render.rs) for the canonical pattern.

**Inside `PromptEditor`** the byte arithmetic on `self.text[..self.cursor]` etc. is safe-by-construction because every public method preserves the cursor invariant (set_cursor / set_cursor_extending clamp via [`clamp_to_char_boundary`](../src/prompt_editor.rs); move_* and delete_* walk char boundaries). The invariant is the safety net; any new method that touches `self.cursor` MUST preserve it, and a `debug_assert!(self.text.is_char_boundary(self.cursor))` at the end of the method is the right way to pin it.

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
| Move to buffer start / end | `Cmd + ↑` / `Cmd + ↓`, or `PageUp` / `PageDown` | `PageUp` / `PageDown` |
| Select all | `Cmd + A` | `Ctrl + A` |
| Undo / redo (Phase 4 polish) | `Cmd + Z` / `Cmd + Shift + Z` | `Ctrl + Z` / `Ctrl + Shift + Z` |
| Copy / paste / cut | `Cmd + C / V / X` | `Ctrl + C / V / X` |
| Delete / Backspace | Per-grapheme delete (not per-byte) — same on both | same |
| Enter | Submit (see below) | same |
| Shift + Enter | Insert newline (multiline) | same |
| Tab | Local completion popup ([Phase 4I](10-roadmap.md#phase-4--editor-at-prompt-block-model-pivot)) | same |
| Ctrl + R | History popup (fuzzy search; [Phase 4J](10-roadmap.md#phase-4--editor-at-prompt-block-model-pivot)) | same |
| Up / Down | Multiline-aware history walk (see [§History walk (Up/Down)](#history-walk-updown) below). Inert while the completion or history popup is open — those widgets consume `↑`/`↓` for their own list navigation. | same |
| Esc | Dismiss popup; if no popup, **no-op** (consumed, nothing sent to PTY) | same |
| Ctrl + C (in the editor) | **No-op** — the shell is idle at a confirmed prompt, so there is nothing to interrupt and a SIGINT would only print a cosmetic `^C`. Editor keeps its text; nothing reaches the PTY. (Interrupting a *running* program happens in `RawTerminal`, where the editor is inactive and Ctrl+C → SIGINT passes through.) | same |
| Ctrl + D on empty editor | Send EOF to PTY (exit an idle shell) | same |

The matching matrix lives in [`classify_editor_motion`](../src/render_pane.rs); both branches are unit-tested with the `is_macos` flag flipped explicitly so each OS's convention is verified on every CI run, not only on the host that runs CI. New motion keys are added there first, with tests, before any new row appears above.

**Note on caret motion vs scrollback nav.** A pane has both an *editor* caret and a *scrollback* viewport that each need "go to start / end" and "page" gestures, so they're split by modifier:

- **Editor caret** — `Cmd+↑/↓` (macOS) and bare `PageUp`/`PageDown` (both platforms) move the caret to the buffer start / end; `Shift` extends.
- **Scrollback viewport** — `Ctrl+Home`/`Ctrl+End` jump to the top / bottom of the sealed-block stack and `Ctrl+PageUp`/`Ctrl+PageDown` page it (both platforms); `Cmd+Option+↑/↓` (macOS) / `Ctrl+Alt+↑/↓` (Linux) are the equivalent jump chords. These are *app-level* — they fire in `RawTerminal` (a running command) too, and are no-ops in alt-screen. See [spec/11 §Shipped](11-keyboard-shortcuts.md#shipped-phase-1--phase-2) and [§Per-mode keyboard routing](11-keyboard-shortcuts.md#per-mode-keyboard-routing).

`Ctrl+Home`/`Ctrl+End` are deliberately *not* editor caret motions (an earlier draft mapped them to caret-to-doc-start/-end on Linux); the caret reaches the buffer ends via `PageUp`/`PageDown` instead, leaving `Ctrl+Home/End` free for the scrollback.

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
| Shift+click | Extend the existing selection to the click, keeping the anchor and the current char / word / line mode (see [§Cross-block selection](#cross-block-selection)). With no selection yet, behaves like a plain click. |

The word / line range helpers (`word_range_at`, `line_range_at` in [`src/prompt_editor.rs`](../src/prompt_editor.rs)) are pure functions, unit-tested, and shared between the press handler and the drag handler so single-click → drag and double/triple-click → drag agree on what a "word" or a "line" is. The per-pane anchor range (`PaneUiState::editor_drag_anchor`) is cleared on a single click so the next drag starts in character mode again.

### Multiline editing

Multiline is first-class. `Shift+Enter` inserts a newline. Enter submits. The renderer wraps soft lines at the pane's cell width and shows a continuation gutter glyph.

For multi-line commands sent to a shell that supports them (`{}` block, here-doc), the whole buffer is sent verbatim followed by a final newline. We do not auto-quote, auto-escape, or pre-validate shell syntax — that's the shell's job.

### Undo / redo

Undo and redo are scoped to **one editing session per command** — the editor's undo stack starts fresh after every submit and grows until the user presses Enter again. There is no cross-command undo; the previous command is gone the moment it's sent. The stack lives on the editor (per-pane), not on the pane or workspace.

**`Cmd+Z` / `Cmd+Shift+Z` (macOS) — `Ctrl+Z` / `Ctrl+Shift+Z` (Linux/Windows)** are the bindings. Shift+Z is the convention for redo on macOS; Linux follows the same chord because it's well-understood.

#### What gets captured

Every undo entry is a snapshot of three values, taken **before** a mutating operation:

```rust
struct UndoEntry {
    text: String,
    cursor: usize,                    // byte index, char-boundary invariant
    selection_anchor: Option<usize>,  // byte index, char-boundary invariant
}
```

Selection is included because the user expects undo to restore **what they were looking at**, not just the text. Specifically:

- **select → cut → undo** restores the text AND re-selects exactly what was cut, so the same range is highlighted again. A second cut (`Cmd+X`) re-cuts it, a paste pastes over it, a keystroke replaces it. This is the round-trip the user reaches for after a mistaken cut.
- **select → paste → undo** restores the text that was replaced AND re-selects it, so the next paste does the same replacement again. Without selection restore, the user has to re-select the original range by hand — every paste mistake costs them double.
- **type → undo** restores the buffer to its pre-typing state with the cursor where it was. Selection state is whatever it was pre-typing (usually `None`).

#### Coalescing rule (which operations push a new entry)

A naive "one entry per character" undo would force the user to mash Cmd+Z 15 times to recover from a fat-finger paste-then-type. We coalesce **single-character typing runs** but break the coalesce on anything that's NOT continuing typing:

- **Typing a single character at the cursor** with the previous op also a single-char insertion ⇒ append to the current run, **do not** push a new entry. The existing top entry (capturing pre-run state) is what `undo` restores to.
- **Backspace** with the previous op also a single-char backspace ⇒ append to the current backspace run.
- **Forward delete** with the previous op also a single-char forward delete ⇒ append.
- **Anything else** breaks the run and pushes a new entry capturing pre-op state. Specifically:
  - Multi-char insert (paste, set from history).
  - Cut.
  - Selection-replacement (insert / backspace / delete forward against a non-empty selection).
  - Word-delete (`delete_word_left` / `delete_word_right`).
  - Delete-to-line-start.
  - Any cursor move or selection change (these are not undo points themselves but they end the current coalesce run; the next typing keystroke pushes a fresh entry).
  - Programmatic `set_from_history` (history walk) — see [§History walk (Up/Down)](#history-walk-updown). Each history substitution is its own undo entry.

The coalescing state is one `Option<OpKind>` field on the undo stack; it's cleared when the buffer is cleared, when the editor is reset after submit, and on every cursor / selection change. Two consecutive identical ops only coalesce when the cursor moved by **exactly one char** in the expected direction (insert → advanced by one char; backspace → retreated by one char). A pasted run of "abc" that arrives as `insert_str("abc")` is one undo entry — not three — because it's a single multi-char insert.

#### Reset on submit

On `submit_editor_command` ([§Submission semantics](#submission-semantics-enter), step 6 "reset_after_submit"), the undo stack AND the redo stack are cleared, and the coalesce state is cleared. The previous command is sealed; its editor history doesn't follow into the next prompt.

#### Redo stack interaction

`undo()` pops the top entry, captures the current state as a redo entry, and applies the popped entry to the editor. `redo()` does the inverse: pops from redo, captures current as undo, applies. **Any mutating operation that pushes a new undo entry clears the redo stack** — once you start editing again from an intermediate undo point, the forward path is gone. Standard editor convention.

#### Test surface (strict layer)

- Fresh editor: undo / redo are no-ops.
- `type abc → undo` ⇒ buffer "", cursor 0, no selection.
- `type abc → undo → redo` ⇒ buffer "abc", cursor 3.
- `type ab → backspace → undo` ⇒ buffer "ab", cursor 2.
- `type abc → move_left → type x → undo` ⇒ buffer "abc", cursor 2 (the cursor position right before the `x` was inserted; the move_left was not an undo point but ended the prior typing run).
- `select_all "abc" → type x → undo` ⇒ buffer "abc", selection covering "abc", cursor at "abc".len().
- `select "ab" → cut → undo` ⇒ buffer "abc", cursor at 2, selection covering "ab".
- `select "ab" → paste "xyz" → undo` ⇒ buffer "abc", cursor at 2, selection covering "ab".
- Typing 10 chars produces 1 undo entry (coalesce).
- 5 backspaces produce 1 undo entry (coalesce).
- Typing then backspacing produces 2 entries (the backspace break the coalesce).
- `submit` clears both stacks.
- Redo cleared after any new mutation past the current undo position.

Coalescing decisions are pure functions of `(prev_op, current_op, cursor_delta)` — extract as a free helper for direct test coverage rather than only exercising it through the editor.

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

When the user presses Enter on a command the shell considers incomplete (`echo 1 &&` → shell wants more; here-docs; unclosed quotes), Termica re-opens the editor on the next line instead of erroring. How the shell signals "incomplete" differs by shell, but all three converge on the same DCS-JSON **continuation marker**:

```jsonc
// emitted whenever the shell's parser wants more input
{ "type": "continuation", "session": "<id>" }
```

- **bash / zsh** print a **continuation prompt** (`PS2`) and stay in line-reading mode; Termica's integration script overrides `PS2` to emit the marker instead of the conventional `>` glyph. The shell holds the partial command buffered at the tty.
- **fish** has no `PS2` and runs non-interactively (its reader is off), so the bootstrap parse-checks each submitted command with `fish -n` (no-execute) BEFORE running it: an EOF-while-expecting-more error (trailing `&&`/`|`, open block, unbalanced quote, trailing `\`) means "incomplete" → emit the marker and loop without executing; a genuine syntax error executes (so fish surfaces it — the safe default, never trap the user). fish holds **no** partial buffer — nothing is sent until the command is complete.

The parser surfaces this as `LifecycleEvent::Continuation` ([`src/markers.rs`](../src/markers.rs)). On reception the `PromptController` calls `try_promote_to_editor_for_continuation`, which re-promotes to `ShellPromptEditor` **only if** `last_transition.reason == EnterSubmitted` — i.e. the immediately-preceding demote came from a real submit, not from an `Esc` or any other path. (If the user explicitly `Esc`'d out of the editor, a continuation marker must not yank them back in.)

When the editor reopens, the `PaneSession` restores the editor's text to `last_submitted + "\n"` and places the caret at the end so the user can resume typing on the next line. This effectively turns the "shell wanted more" path into "we hand the editor back, now multi-line."

The next submit's framing depends on whether the shell **buffered the prefix** — this is the `continuation_to_send` decision ([`src/pane.rs`](../src/pane.rs)):

- **bash / zsh: suffix-only.** They hold `<prev>\n` buffered at the tty (waiting at `PS2`), so only the **suffix beyond `last_submitted`** is written (with the restore's leading `\n` stripped). Resending the prefix would duplicate every line, and the `EchoSuppressor` (primed for the first half) would mismatch and disengage.
- **fish: whole buffer.** fish executed nothing on the incomplete submit and its read-eval loop is back at a fresh `read`, so the **entire cumulative buffer** is resent (base64-framed, one tty line); the bootstrap re-checks completeness of the whole command each time. Sending only the suffix would run `echo 2` alone and lose the `echo 1 &&` being continued.
- `submit_editor_command` remembers the full text in `last_submitted: Option<String>` for both paths.
- `last_submitted` is cleared on the next `Preexec` **or** `Precmd` lifecycle event, whichever arrives first. `Preexec` is the canonical clear — the shell has accepted a complete command and is about to execute it. `Precmd` is a backstop: if a `Preexec` is never observed (the shell aborted the line, an integration script swallowed the marker, the read boundary fell wrong and the parser dropped it), the next prompt redraw still resets the suffix-only-submit state so the user's next command can't "vanish" into an empty-suffix send. Without this backstop a single missing `Preexec` leaves the editor in suffix mode indefinitely; the next typed command appears to disappear because only the empty bytes beyond `last_submitted` are sent. The covering strict-layer test lives in [`src/pane.rs`](../src/pane.rs) (`precmd_clears_last_submitted_as_backstop_for_missed_preexec`).

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

Tab is local completion. It does **not** send `\t` to the PTY.

**v1 (Phase 4I)** ships three local sources:

1. **Path completion** for the token under the cursor that looks like a path or starts with `/`, `./`, `../`, or `~/`.
2. **Command history** for the entire buffer when cursor is past whitespace (suggests previous matching commands).
3. **`$PATH` executable lookup** for the first token.

Ranking: prefer recent / same-cwd / non-zero-exit-history matches. The popup is a native egui widget with arrow-key navigation and Tab/Enter to accept.

**Post-MVP**, the engine grows two more sources behind the same popup: **CLI-native drivers** (`kubectl __complete`, `gh __complete`, cobra `__complete`, `aws_completer`, `git --list-cmds`, …) — *shipped*; candidates stream in off-thread and merge into the open popup without disturbing the user's selection — and a **per-pane shell sidecar** (bash / zsh / fish loaded with the user's rc, talking to Termica over a private stdio JSON protocol). The full design — protocol, lifecycle, ranking, caching, failure modes — lives in [04a — Tab completion](04a-completion.md). Slices land as separate PRs: CLI-native drivers (done), then fish (the easiest sidecar), then bash, then zsh.

## Syntax highlighting

A minimal shell tokenizer in-house for v1:

- commands (first token, highlighted as command kind)
- strings (single-quote / double-quote, with embedded `$var` inside double-quote)
- variables (`$NAME`, `${expr}`)
- pipes / redirects (`|`, `>`, `>>`, `<`, `&`)
- subshells (`$(...)`, `` `...` ``)
- flags (`-x`, `--long`)
- comments (`#`)

Backslash escapes are honoured the way the shell reads them: outside quotes `\X` is a literal `X`, so a backslash-escaped quote / space / metacharacter (`what\'s`, `foo\ bar`, `a\|b`) stays part of its word instead of starting a string or breaking the token. Inside double quotes a backslash escapes the next byte too (`"a\"b"` is one string; `"\$x"` is not a variable). Single-quoted strings are literal — a backslash inside `'…'` is an ordinary character, matching the shell.

Tree-sitter is overkill for v1. We will reach for it if and when the in-house tokenizer can't keep up with what we want to highlight.

## History walk (Up/Down)

`↑` and `↓` walk pane-scope history when the buffer is single-line and grow into a precise, multiline-aware rule when it isn't. The shape:

```
    multiline buffer (caret somewhere inside)

    ┌─ row 0 ────────── line A
    │  row 1 ────────── line B  ← caret
    └─ row 2 ────────── line C

    ↑ on row 0 → step back through history (save buffer + caret first time)
    ↑ on row > 0 → move caret to row − 1 within the editor
    ↓ on last row → walk forward toward newer entries (restore at the head)
    ↓ on row < last → move caret to row + 1 within the editor
```

Precise rule:

- **`↑` with caret on row > 0** (i.e. there is a previous line in the editor): move the caret to that previous line, preserving the desired column (the column of the caret when the user first started moving vertically — same convention as every other text editor). Do **not** step into history.
- **`↑` with caret on row 0** (no previous line in the editor; this is the single-line case by definition): step back to the previous pane-scope history entry. On the first such step from a non-empty (or empty) buffer, save the **current text** *and* the **current caret byte index** as the in-progress snapshot. Replace the buffer with the history entry; caret goes to the end of the substituted text (existing convention).
- **`↓` with caret on row < last row**: move the caret to the next line, preserving the desired column. Do **not** step in history.
- **`↓` with caret on the last row** (no next line in the editor): walk toward newer entries. If already at the newest entry, returning to the "head" restores the in-progress snapshot — **text *and* caret position** — exactly as they were when `↑` first stepped away. If no snapshot was ever taken (e.g. `↓` pressed without any prior `↑`), `↓` is a no-op.
- **Any non-arrow edit** (text input, paste, cut, backspace, delete, undo, drag-select) **abandons the walk**: the in-progress snapshot is dropped, history navigation resets, and the next `↑` starts a fresh save.
- **Modifier combinations** that already have other meanings (`Cmd+↑/↓`, `Shift+↑/↓`, `Option+↑/↓`) take their existing meaning per the keymap above and never step in history. Only the bare arrow keys participate.
- **When the completion popup or `^R` history overlay is open**, `↑`/`↓` route to that widget's list navigation. The editor doesn't see them.

The in-progress snapshot is the single piece of state this section adds beyond plain history step-back/step-forward: `Option<{ text: String, cursor: usize }>`. It lives on the `PromptController`'s recall state ([`src/shell.rs`](../src/shell.rs) / [`src/pane.rs`](../src/pane.rs)) so the editor can be reconstituted exactly. The caret restore is what makes the round-trip feel right: a user who started typing `git push origin` and pressed `↑` once to glance at the last command expects `↓` to put them back with the caret right where it was — not at the end of `git push origin`, where every other terminal puts it.

Strict-layer tests cover: the multiline move-by-line motion (caret tracking with a 4-row buffer); the row-0/row-last edge that steps into history; the desired-column preservation through repeated `↑`/`↓` across lines of varying length; the snapshot save on first `↑` (text + caret); the snapshot restore on return-to-head (text + caret); abandonment on any non-arrow edit.

## History popup

`Ctrl+R` opens a popup over the editor:

- Default scope: global history, filtered by current cwd → broader.
- Fuzzy matcher (e.g. `nucleo` or `skim`'s matcher) ranks by score, recency, and cwd proximity.
- Arrow keys / `Tab` to walk results; `Enter` to accept (replaces editor buffer); `Esc` to dismiss.
- Scope toggles (this pane / this project / global) in the popup chrome.

See [07](07-history-and-search.md) for the history storage and scopes.

## What the editor never does

- **Never** stores a control character in the buffer. The insert primitives (`insert_char`, `insert_str`) drop every C0/C1 control code and DEL before it reaches the buffer; **newline is the sole exception** (the multiline line separator). A raw control byte would poison the submitted command — on submit the shell sees e.g. `\x03` (ETX) and aborts the line, so the typed command silently vanishes (never reaching scrollback or history) and the stale byte echoes as a `^C`-style literal in front of the next command. Filtering at the insert primitives makes that state unrepresentable for every caller. (Strict layer — tested.)
- **Never** sends a keystroke to the PTY while in `ShellPromptEditor` mode, except via `submit()` and one edge case — Ctrl+D (EOF) on an **empty** editor, to exit an idle shell. This is enforced structurally at the input boundary by a single gate (`pty_passthrough_allowed`), not by a per-chord allow-list: every other keystroke — including Ctrl+C and unrecognised control chords like Ctrl+X/Y/Z — is swallowed. A leaked raw control byte sits in the shell's input and resurfaces prefixed to the next command's output (`^X^Y^Z…`), and a leaked `\x03` (Ctrl+C) aborts the shell's line so the submitted command vanishes. Ctrl+C is deliberately inert here: at an idle prompt there is no program to interrupt (that happens in `RawTerminal`), so a SIGINT would only print a cosmetic `^C`. The earlier per-letter consume list only covered a handful of chords and leaked the rest of the alphabet; the boundary gate makes the leak unrepresentable. (Strict layer — tested.)
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
    cwd: Option<PathBuf>,
    git: Option<GitContext>,        // captured at command-start; None on a live Prompt
}

pub struct GitContext {
    pub branch: Option<String>,     // None on a detached HEAD
    pub ahead: u32,                 // vs upstream (0 if none / in sync)
    pub behind: u32,
    pub dirty: DirtySummary,
}

pub struct DirtySummary {
    pub files_changed: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
}
```

**Git context: live on the prompt, captured-at-run-time on running / sealed blocks.** The `PaneSession` holds the pane's *current* `Option<GitContext>`, refreshed off-thread by a [`GitProbe`](../src/git_probe.rs). Two surfaces consume it:

- The **live `Prompt` header** reads the pane's current git directly, so it updates as you `cd` / dirty the tree. The block's own `header.git` stays `None` here.
- At **`Preexec`** the pane stamps its current git into the new `Running` block's `header.git` (alongside the start-time cwd and clock), and the seal carries it into `Sealed`. So a running / sealed block shows the branch / dirtiness the command **actually ran under**, frozen as history — not whatever is current now (which would be anachronistic on scroll-back). This mirrors how `cwd` and `duration` lock at command-start.

The probe runs `git status --porcelain=v2 --branch` + `git diff HEAD --numstat` for the pane's cwd on a background thread, re-triggered when the cwd changes or a command finishes, debounced and cancelled on pane teardown (per [01](01-architecture.md) "Do not block the UI on probes"). Parsing is pure ([`src/git_context.rs`](../src/git_context.rs)); the capture is in `BlockStack::start_running` (unit-tested).

A `PaneSession` owns `Vec<Block>` plus an `active: Option<BlockId>` pointing at the live one (always the last; `None` very briefly between command_finished and the next precmd).

### Visual structure (three captures' worth of UI)

Each block paints differently per state:

Each chip is a rounded pill; `[…]` below stands in for one. The git chips slot in after cwd: branch, an optional `ahead N behind N` chip, then an amber dirty chip (`N files +A -R`, files-only when the dirt is untracked). Sealed / running show the git **captured at command-start**; the prompt shows **live** current git. The branch chip is green (the headline of the git chips); on **sealed** (historical) blocks every chip is rendered muted — desaturated toward grey but still slightly tinted — so finished blocks read as past-tense while the live prompt / running chips stay vivid (`fade_chip_color` in [`src/render.rs`](../src/render.rs)). The one exception is the failed-`exit` chip: a non-zero exit stays vivid red even on a sealed block, so failures don't fade into scroll-back. After the git chips, the **live prompt only** adds a `PR #NN` chip for the branch's open GitHub PR, colored by its rolled-up CI status (green passing / yellow pending / red failing) — sourced from an async [`GhProbe`](../src/gh_probe.rs) (`gh pr view`). It's prompt-only because a finished command's CI status is meaningless on scroll-back; you want *current* CI, where you're about to act.

```
┌─────────────────────────── Sealed ─────────────────────────────┐
│ [~/git/enthal/termica] [main] [1 file +3 -0] [0.034s]          │  ← header chips, git frozen at run-time
│ git status                                                     │  ← bold command
│ On branch main                                                 │  ← frozen output
│ Your branch is up to date with 'origin/main'.                  │
└────────────────────────────────────────────────────────────────┘
┌─────────────────────────── Running ────────────────────────────┐
│ [~/git/enthal/termica] [main] [3 files +120 -8] [11s]          │  ← git frozen at start; live duration
│ while true; do sleep 1; date; done                             │  ← bold command, frozen
│ Tue May 26 10:07:52 PDT 2026                                   │  ← live output
│ Tue May 26 10:07:53 PDT 2026                                   │
│ ▌                                                              │  ← running-cursor glyph
└────────────────────────────────────────────────────────────────┘
┌─────────────────────────── Prompt ─────────────────────────────┐  ← glued to viewport bottom
│ [~/git/enthal/termica] [main] [3 files +120 -8] [PR #124]      │  ← live git + PR chips (PR colored by CI)
│ ❯ git status_                                                  │  ← editor (multiline expands here)
└────────────────────────────────────────────────────────────────┘
```

The chrome between blocks is non-text (thin separator + space). Selection (below) passes through it; copy-to-clipboard does not include it.

The `❯` and the chips are decorative — painted by Termica, not the shell's `PS1`. The integration script intentionally minimises `PS1` so the shell's own prompt drawing doesn't visually conflict with Termica's chrome ([03](03-shell-integration.md)).

### Block visual chrome (shipped)

Concrete visual rules that landed via `examples/pick_*` visual-picker decisions (see [09](09-testing.md)). Each is documented here as a constant in [`src/render.rs`](../src/render.rs); the comment on each constant names the picker variant that won.

**Chips (block header).** Each chip (cwd, exit code) is a dark-grey rounded rectangle with a **1 px dim stroke** (`BLOCK_HEADER_CHIP_STROKE = #444`). The stroke is just enough to read the chip's edge against the very-similar panel background without competing with the chip text. `exit N` for non-zero `N` renders the text in red.

**Failed-block background wash.** Sealed blocks whose command exited non-zero are painted on a warm-dark red wash (`FAILED_BLOCK_BG`, unmultiplied rgba≈`#80, #20, #20, alpha 0x18` ≈ 9%). The wash is implemented via the `painter.add(Shape::Noop) + painter.set()` pattern: a shape index is reserved BEFORE the chip + label + snapshot paint, and the rect is filled in after layout settles, so the wash sits underneath the content. Translucent enough that the styled snapshot text on top is fully legible.

**Block separator.** Between sealed blocks: `BLOCK_SEPARATOR_GAP = 10 px` of vertical space, then a 1 px hairline (`BLOCK_SEPARATOR_HAIRLINE`, unmultiplied rgba `#a0, #a0, #a0, alpha 0x18` ≈ 9%) running the full pane width, then another 10 px. Total inter-block breath is ~21 px with the hairline centered.

**Focused-editor chrome.** When the prompt-editor caret would be drawn ([§"When is the caret shown?"](#when-is-the-caret-shown)), a 1 px rounded outline (`FOCUSED_EDITOR_CHROME_COLOR`, premul rgba `#a0, #a0, #a0, alpha 0xb0`) is drawn around BOTH the chip bar AND the editor body together, 2 px outside the combined rect, 6 px corner radius. The outline says "this whole prompt surface is wired for input"; same predicate as the caret. (Visual tuning is open: see this branch's `pick_*` follow-ups.)

**Color helper.** The const Color32 constructors require premultiplied values, so source colors written here as "unmultiplied" are pre-computed: each channel × (alpha / 255), rounded. The picker examples use `from_rgba_unmultiplied` at the call site for natural authoring; production code uses the precomputed `Color32::from_rgba_premultiplied` constants and the comment names the unmultiplied source.

## Layout: fixed-footer prompt, sticky-top header

Three rules govern the per-frame layout pass:

1. **The `Prompt` block is a fixed footer.** It's glued to the bottom of the pane viewport and does not scroll. Its height varies with the editor's line count (multiline grows it down; the scrollable area shrinks correspondingly). When the bottom block is `Running` or there is no bottom block, the footer area is unused and the scroll area extends to the pane bottom.

2. **Older blocks scroll under the footer.** The remaining vertical (pane height minus footer height) is the scroll area. Mouse-wheel / arrow-key scroll moves blocks within this area. Blocks above the viewport top are clipped; the scroll position is pane-level, not per-block.

3. **The top-most partially-visible block's header pins to the top edge.** If a block's body is visible but its header has scrolled above the viewport, the renderer paints that header pinned to the top edge of the scroll area for as long as the block's body intersects the viewport. Only one sticky header is shown at a time; once the body fully scrolls past, the *next* block's header takes its place. This is the same affordance iOS section headers use.

The pinned region is what identifies the block: its cwd / exit chip **and its command label**, the latter capped at 4 lines (a longer multiline command is truncated with a trailing `…`) so the strip can't grow without bound. Just the cwd would be ambiguous — many blocks share a cwd; the command is the discriminator.

The layout helper that decides which block is "sticky-eligible" and computes the paint offset is the unit-testable boundary: pure math, no egui dependency, covered by tests that walk scroll positions through a synthetic block list. It ships as [`render_pane::compute_sticky_header`](../src/render_pane.rs) — given each block's screen-y extent + the height of its pinned region (chip + capped command) and the viewport top, it returns the block to pin and the y to paint it at (flush at the top, or pushed up by the next block during the handoff). The actual paint is [`render_pane::paint_sticky_header`](../src/render_pane.rs): an opaque strip (so content scrolling underneath is occluded) plus the reused `paint_block_header` chrome and the block's command label, clipped to the viewport so a pushed-up header slides under the top edge. The pinned command label is **interactive** (selectable, like its inline twin): because the strip is a foreground overlay, a non-interactive label let presses fall through to the output scrolling underneath, so double-clicking the pinned command selected the wrong text. `paint_sticky_header` returns the command label's `Response`, and the pane routes a press on it into a selection of the pinned block — confined to the command rows the strip shows, since the strip overlays output that has scrolled off-screen and is no longer at the pixel the real block would map it to. The pinned label also paints the block's command-region selection (clipped to the shown rows) so the highlight stays in sync with the inline block. Only a sealed block's pinned command is selectable; a running block's command label is not selectable inline either, so its pinned copy merely absorbs the press.

## Cross-block selection

Selection coordinates are **pane-level**, not grid-level. The actual types live in [`src/pane_selection.rs`](../src/pane_selection.rs):

```rust
pub struct PaneSelection {
    pub anchor: PaneCursor,
    pub head:   PaneCursor,
}

pub struct PaneCursor {
    pub block_id: BlockId,
    pub row:      usize,         // row within the block's unified row space
    pub col:      usize,         // column on that row
}
```

A block's **unified row space** is `command_lines + snapshot_rows`: rows `0..command_lines` come from the command label split on `\n`; rows `command_lines..` come from the sealed `Vec<StyledLine>`. This is the same row space [`crate::block_selection`] uses for within-block selection, so the per-block helpers (`cell_word_range`, `cell_line_range`, `block_selection_text`) compose without translation.

Ordering of `PaneCursor` is lexicographic on `(block_id, row, col)`. Because [`BlockId`]s are allocated monotonically in creation order (the [`BlockStack`] invariant), this IS the visual top-to-bottom reading order of a pane — a lower-id block sits above a higher-id block.

When the user drags across a block boundary, the selection logically spans all text content between anchor and head. Each block clips its piece via `PaneSelection::block_range_for(block_id, total_rows)`, which returns `Option<(BlockCursor, BlockCursor)>`:
- Inside the selection's `[start.block_id, end.block_id]`: clipped per-block range.
- Outside that range: `None`.
- For the start block: `start_cursor .. end_of_block`. For the end block: `start_of_block .. end_cursor`. For interior blocks: full block.

`Selection → text` (`pane_selection_text`) walks the sealed-block list in pane order, asks each block in range for its clipped slice via the within-block helper, and joins per-block payloads with `\n`. Block chrome (cwd / exit chips, separators) is visually unhighlighted even when the selection passes over it, and is not included in the copy payload.

**Multi-click rule (Word / Line) — unified far-edge rule.** Double-click + drag and triple-click + drag work the same way whether the drag stays in one block or crosses block boundaries. The source-block anchor (the originally-double/triple-clicked word or line bounds) lives in `PaneUiState::sealed_drag_anchor` as `(BlockId, BlockCursor, BlockCursor)`. On every drag move, the renderer computes the word / line bounds under the pointer in the head's block, then calls [`crate::pane_selection::extend_multiclick_selection_endpoints`] with both endpoint's word/line bounds. The rule it applies:

- **Each endpoint uses the far edge of its word/line within its block** — the edge facing AWAY from the other endpoint.
- **Same block**: collapses to `(min(a_start, h_start), max(a_end, h_end))` — the rolling union the within-block drag has always done.
- **Cross block, head AFTER anchor**: anchor cursor = `a_start` (upper edge of the upper block); head cursor = `h_end` (lower edge of the lower block). Both blocks' words/lines are FULLY highlighted.
- **Cross block, head BEFORE anchor**: anchor cursor = `a_end`; head cursor = `h_start`. Same property — both endpoints' full words/lines highlighted.

This unified rule replaces an earlier "stay in source block" carve-out. The carve-out's failure modes were two: (a) forward cross-block drag degraded to char-precision in the head block, (b) backward cross-block drag "lost" the anchor word in the source block because `PaneSelection::ordered()` flipped the endpoints and the anchor at the word's left edge became the high end of the selection, which made `block_range_for(anchor_block)` clip BEFORE the word. The far-edge rule eliminates both because the anchor cursor adapts to which end it represents in pane order.

`PaneSelection` deliberately does NOT carry a `SelectionMode` field because mode lives at the gesture layer (the multi-click anchor in `PaneUiState`), not the selection-data layer. The clipped per-block ranges always describe character-precise endpoints — once a Word / Line anchor has snapped its endpoints outward, the `PaneCursor` values are already at word/line bounds.

**Shift+click extend (Chrome-style).** A primary click with **Shift** held does not start a fresh selection — it **extends** the existing one to the click, keeping the original anchor and re-using the same machinery a drag does. The decision is the pure [`render_pane::press_extends_selection`]`(primary_pressed, shift_held, has_selection)`: it extends only on a Shift+click when a selection already exists to anchor to; with no selection there's nothing to extend from, so it falls through to a normal start. Because a plain click leaves a (collapsed) selection behind, the usual flow — click to place the caret, Shift+click to select to there — works.

The extend **preserves the selection mode** the gesture established, because Shift+click routes through the very same extend branch as a drag, which reads the retained `PaneUiState::click_count` + `sealed_drag_anchor` (sealed blocks) / `editor_drag_anchor` (editor) / the live grid's own `SelectionMode`:

- after a single click → char-precise extend (`update_pane_selection_head` / `set_cursor_extending` / `Selection::extend_to`);
- after a double-click → **word**-granular extend, snapping the head to the Shift+clicked word via the same far-edge rule above;
- after a triple-click → **line**-granular extend.

This holds in all three selection domains and, in the sealed domain, **across block boundaries and into / out of a block's command vs. output** — Shift+click reuses `find_head_block_for_pos` + `extend_multiclick_selection_endpoints`, so the head can land in a different block (or the pinned sticky header) than the anchor. Shift+click never opens a Cmd-clickable link (link-open lives only on the fresh-start path).

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
