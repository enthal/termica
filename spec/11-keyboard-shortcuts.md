**← Previous:** [10 — Roadmap](10-roadmap.md) | **Up:** [SPEC index](../SPEC.md) | **Next:** [12 — Distribution & releases](12-distribution.md) →

# 11 — Keyboard shortcuts

Single source of truth for every key combo Termica recognises as an *app-level* shortcut. Keys that go to the PTY (typing, arrow keys, Ctrl+letter, etc.) are covered in [02 — Terminal engine](02-terminal-engine.md) and the input encoder ([`src/input.rs`](../src/input.rs)).

## Conventions

- **macOS** uses `Cmd` (`modifiers.mac_cmd`) for app-level shortcuts. `Cmd` is reserved for the app: `Ctrl` in a terminal goes to the shell as control characters and never to us.
- **Linux / Windows** uses `Ctrl+Shift`. Plain `Ctrl+letter` is a shell binding on every well-known shell (`Ctrl+T` is `transpose-chars`, `Ctrl+W` is `backward-kill-word`, `Ctrl+Q` is XON flow control, etc.), so we layer Shift on to disambiguate "app shortcut" from "shell binding." This is the gnome-terminal / konsole / xterm convention.
- **Bracket-pair shortcuts** carry Shift on macOS too (the chord includes Shift; without Shift you don't get `[` or `]` from the physical key in some layouts).
- The encoder ([`src/input.rs::encode_key`](../src/input.rs)) **rejects every unrecognised modifier+key combination**. An unmapped chord does nothing — it never falls through as a bare-key keystroke to the PTY.

## Shipped (Phase 1 + Phase 2)

| Shortcut (macOS) | Shortcut (Linux / Windows) | Action | Source |
|---|---|---|---|
| `Cmd+T` | `Ctrl+Shift+T` | New tab in the focused pane's Tabs container. Inherits cwd from the active sibling pane. | Phase 2A / 2B |
| `Cmd+W` | `Ctrl+Shift+W` | Close the focused tab. Modal confirmation if a program is in alt-screen. On the last tab, routes to Quit. | Phase 2A |
| `Cmd+Q` | `Ctrl+Shift+Q` | Quit the app. Modal confirmation with a 60-second countdown if any program is in alt-screen. | Phase 2A |
| `Cmd+Shift+]` | `Ctrl+Shift+]` | Next tab in the parent Tabs container (wraps). | Phase 2A |
| `Cmd+Shift+[` | `Ctrl+Shift+[` | Previous tab in the parent Tabs container (wraps). | Phase 2A |
| `Cmd+K` | `Ctrl+Shift+K` | Clear scrollback **and** blank viewport on the focused pane. Cursor moves home. Shell is untouched; it redraws on next prompt cycle. | Phase 2 polish |
| `Cmd+Option+Up` / `Cmd+Option+Down` | `Ctrl+Alt+Up` / `Ctrl+Alt+Down` | Jump the focused pane's scroll position to the top / bottom of the sealed-block stack. No-op in alt-screen mode. Cmd+Up / Cmd+Down alone (no Option) stay reserved for editor caret-to-doc-start / -end — the Option / Alt modifier disambiguates the scrollback jump. | Phase 4 polish |
| `Cmd+Shift+C` | `Ctrl+Shift+C` | Copy current selection to clipboard if non-empty; otherwise no-op. | Phase 1E-k |
| `Cmd+C` | `Ctrl+C` | Copy if selection is non-empty; otherwise SIGINT to the PTY. | spec/02:157 |

Mouse:

| Gesture | Action | Phase |
|---|---|---|
| `Cmd+Click` / `Ctrl+Click` on a URL or path | Open in the OS default handler (URLs) or follow the path. | Phase 1E-l / 1E-m |
| Click + drag in the grid | Character selection. | Phase 1E-k |
| Double-click | Word selection. | Phase 1E-k |
| Triple-click | Line selection. | Phase 1E-k |

Window controls (via the macOS menubar; Phase 2A):

| Shortcut (macOS) | Action |
|---|---|
| `Cmd+H` | Hide Termica. |
| `Cmd+Opt+H` | Hide other applications. |

## Editor (Phase 4, shipped piece by piece)

These chords are only active when the focused pane is in `ShellPromptEditor` and the live block is `Prompt`. They are owned by [`src/render_pane.rs::apply_event_to_editor`](../src/render_pane.rs) and routed through [`src/prompt_editor.rs`](../src/prompt_editor.rs); the OS-specific motion keys are classified by [`classify_editor_motion`](../src/render_pane.rs) and unit-tested with both `is_macos = true` and `is_macos = false` paths.

| Shortcut (macOS) | Shortcut (Linux / Windows) | Action | Status |
|---|---|---|---|
| `Enter` | `Enter` | Submit the command. Eager demote, then PTY write. | ✅ Phase 4C ([#54](https://github.com/enthal/termica/pull/54)) |
| `Shift+Enter` | `Shift+Enter` | Insert a newline (multiline edit). | ✅ Phase 4B ([#53](https://github.com/enthal/termica/pull/53)) |
| `Esc` | `Esc` | Leave editor; demote to `RawTerminal`. | ✅ Phase 4B |
| `Cmd+A` | `Ctrl+A` | Select all editor contents. | ✅ Phase 4D-poly ([#59](https://github.com/enthal/termica/pull/59)) |
| `Cmd+C / V / X` | `Ctrl+C / V / X` | Copy / paste / cut from the editor's selection. Cut records one undo entry so `select → cut → undo` restores the cut text **selected** ([04 §Undo/redo](04-prompt-editor.md#undo--redo)). | ✅ Phase 4D-poly ([#59](https://github.com/enthal/termica/pull/59)) |
| `Cmd+Z` / `Cmd+Shift+Z` | `Ctrl+Z` / `Ctrl+Shift+Z` | Undo / redo, scoped to the current editing session (reset on submit). Restores text **and** selection — see [04 §Undo/redo](04-prompt-editor.md#undo--redo). | ✅ Phase 4 polish |
| Arrow ← / → | Arrow ← / → | Move caret one char (with `Shift` extends selection). | ✅ Phase 4B |
| `Option+ArrowLeft` / `Option+ArrowRight` | `Ctrl+ArrowLeft` / `Ctrl+ArrowRight` | Move caret by word boundary (with `Shift` extends selection). | ✅ Phase 4D-poly ([#60](https://github.com/enthal/termica/pull/60)) |
| `Cmd+ArrowLeft` / `Cmd+ArrowRight` | `Home` / `End` | Move caret to line start / end (with `Shift` extends selection). | ✅ Phase 4D-poly ([#60](https://github.com/enthal/termica/pull/60)) |
| `Cmd+ArrowUp` / `Cmd+ArrowDown` | `Ctrl+Home` / `Ctrl+End` | Move caret to document start / end (with `Shift` extends selection). | ✅ Phase 4D-poly ([#60](https://github.com/enthal/termica/pull/60)) |
| `Ctrl+C` on empty editor | `Ctrl+C` on empty editor | Leave editor; send SIGINT. | ⏳ Phase 4 polish |
| `Ctrl+D` on empty editor | `Ctrl+D` on empty editor | Send EOF to the shell (`exit` semantics). | ⏳ Phase 4 polish |
| `Up` / `Down` | `Up` / `Down` | Multiline-aware history walk per [04 §History walk (Up/Down)](04-prompt-editor.md#history-walk-updown). `↑` on row > 0 moves to the previous editor line; on row 0 it steps back through pane-scope history (saving `{text, cursor}` as an in-progress snapshot on first press). `↓` on row < last moves to the next line; on the last row it walks forward, restoring the snapshot — **text and caret** — at the head. Any non-arrow edit abandons the walk. Inert while the completion or `^R` overlay is open. | ✅ Phase 4J PR 5 (single-line + buffer save). Multiline + caret-restore: ⏳ |
| `Tab` | `Tab` | Local completion popup. Sources: paths under cursor, `$PATH` executables (in command position), command history (prefix-matched). `↑`/`↓` navigate, `Tab`/`Enter` accept, `Esc` cancels, any other key dismisses + falls through to the editor. | ✅ Phase 4I slice 1 |
| `Ctrl+R` | `Ctrl+R` | Open the history overlay. Substring filter over the `runs` table; `↑`/`↓` to walk results, `Enter` to substitute, `Esc` to cancel, `Tab` to toggle scope (`global*` ↔ `this pane*`). Modal — all keystrokes route to the overlay until it closes. | ✅ Phase 4J PR 6 |
| `Ctrl+P` / `Ctrl+N` / `Ctrl+S` / `Ctrl+G` | same | Consumed by the editor (no PTY leak) but no-op until follow-up PRs wire emacs-style navigation. | ✅ Phase 4 polish ([#57](https://github.com/enthal/termica/pull/57)) |

Mouse (editor):

| Gesture | Action | Status |
|---|---|---|
| Click in editor | Place caret at the hit byte; clear selection. | ✅ ([#59](https://github.com/enthal/termica/pull/59)) |
| Drag in editor | Extend selection by **character**. | ✅ ([#59](https://github.com/enthal/termica/pull/59)) |
| Double-click | Select word under pointer (uses `word_range_at`). | ✅ ([#60](https://github.com/enthal/termica/pull/60)) |
| Triple-click | Select line under pointer (uses `line_range_at`). | ✅ ([#60](https://github.com/enthal/termica/pull/60)) |
| Double-click + drag | Extend selection by **word**: rolling union of anchor word ∪ word under pointer. | ✅ ([#61](https://github.com/enthal/termica/pull/61)) |
| Triple-click + drag | Same as double-click + drag but by **line**. | ✅ ([#61](https://github.com/enthal/termica/pull/61)) |

## Planned (everything else)

| Shortcut | Action | Phase |
|---|---|---|
| Continuation marker re-entry | After a submit, a DCS-JSON `continuation` event from the shell (`PS2` fired) re-promotes the editor and restores `last_submitted + "\n"`; only the suffix beyond `last_submitted` is sent on the next submit. | ✅ Phase 4C polish ([#58](https://github.com/enthal/termica/pull/58)) |
| `Cmd+F` / `Ctrl+F` | In-pane find overlay (literal / case-insensitive / regex / fuzzy). | Phase 8 |
| `Cmd+P` / `Ctrl+P` | Command palette. | Post-MVP |

## How a shortcut gets wired

For implementors. The pattern below is the one the matcher in [`src/lib.rs::match_pane_shortcut`](../src/lib.rs) enforces.

1. **Add a `PaneAction` variant** describing the intent (`NewTab`, `ClearScrollback`, etc.). The variant name reflects the user-visible action, not the keystroke.
2. **Extend `match_pane_shortcut`** with the macOS + Linux/Windows branches. The matcher's first guard is *modifiers* — reject every combination that isn't the canonical chord for this platform.
3. **Handle the action in [`TermicaApp::apply_pane_action`](../src/lib.rs)**. This is where tree mutations happen; the matcher only stages intent.
4. **Tests, same commit:**
   - Matcher tests for macOS and Linux/Windows variants.
   - Behavior tests for the action itself (where pure logic is extractable; UI-glue gets a snapshot test).
5. **Document here.** A new shortcut without a row in this table is a smell — it means future-you (or a contributor) won't be able to discover it.

## Configurability

Configurable keybindings are a [Phase 10 polish item](10-roadmap.md#phase-10--polish-and-stop). Until then the chords above are fixed. Be sparing about adding new shortcuts: every chord crowds the discoverability space, and once shipped they're hard to change without surprising users.

---

**← Previous:** [10 — Roadmap](10-roadmap.md) | **Up:** [SPEC index](../SPEC.md) | **Next:** [12 — Distribution & releases](12-distribution.md) →
