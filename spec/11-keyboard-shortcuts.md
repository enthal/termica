**← Previous:** [10 — Roadmap](10-roadmap.md) | **Up:** [SPEC index](../SPEC.md) | **Next:** [12 — Distribution & releases](12-distribution.md) →

# 11 — Keyboard shortcuts

Single source of truth for every key combo Termica recognises as an *app-level* shortcut. Keys that go to the PTY (typing, arrow keys, Ctrl+letter, etc.) are covered in [02 — Terminal engine](02-terminal-engine.md) and the input encoder ([`src/input.rs`](../src/input.rs)).

## Conventions

- **macOS** uses `Cmd` (`modifiers.mac_cmd`) for app-level shortcuts. `Cmd` is reserved for the app: `Ctrl` in a terminal goes to the shell as control characters and never to us.
- **Linux / Windows** uses `Ctrl+Shift`. Plain `Ctrl+letter` is a shell binding on every well-known shell (`Ctrl+T` is `transpose-chars`, `Ctrl+W` is `backward-kill-word`, `Ctrl+Q` is XON flow control, etc.), so we layer Shift on to disambiguate "app shortcut" from "shell binding." This is the gnome-terminal / konsole / xterm convention.
- **Bracket-pair shortcuts** carry Shift on macOS too (the chord includes Shift; without Shift you don't get `[` or `]` from the physical key in some layouts).
- The encoder ([`src/input.rs::encode_key`](../src/input.rs)) **rejects every unrecognised modifier+key combination**. An unmapped chord does nothing — it never falls through as a bare-key keystroke to the PTY. The same invariant holds in the editor: [`apply_event_to_editor`](../src/render_pane.rs) rejects modified `Event::Key`s it doesn't claim, **and** gates `Event::Key`'s sibling `Event::Text`. egui-winit suppresses `Event::Text` under Ctrl/Cmd but **not** under Alt, so on Linux/Windows an `Alt+letter` press would otherwise arrive as bare text and get typed; we drop it (`alt && !ctrl`, non-macOS) so `Alt+letter` is inert. macOS is exempt — `Option+letter` is intentional compose input (`Option+E E → é`); `!ctrl` keeps Windows `AltGr` (= Ctrl+Alt) working.

### Per-mode keyboard routing

Keystrokes are dispatched in **two passes** ([`src/render_pane.rs`](../src/render_pane.rs)), and only the second is mode-dependent:

- **Layer 1 — app-level shortcuts.** Intercepted *before* any mode routing ([`render_pane.rs` ~`pending_action` / popup-switch block](../src/render_pane.rs)), gated only on pane focus or an open popup — **not** on [pane mode](05-pane-modes.md). Everything in [Shipped](#shipped-phase-1--phase-2) (copy/cut/paste, tab + window chords, find, cheat-sheet, scroll-jumps, zoom) lives here, so it fires in every live mode. Their key events never leak to the PTY because the encoder rejects the modifier combo (see the encoder bullet above). Two are *present but visually inert* in `AlternateScreen`: the find overlay and the scroll-jumps don't paint while a fullscreen program owns the grid.
- **Layer 2 — per-mode routing of the rest** (via `editor_is_active()` + the boundary gate [`pty_passthrough_allowed`](../src/render_pane.rs)):

There is **no separate "running command" mode** — submitting a command *eagerly demotes* `ShellPromptEditor → RawTerminal` ([05 §Sequencing around `submit()`](05-pane-modes.md#sequencing-around-submit)) before the PTY write, so a live/running command **is** `RawTerminal`. The editor chords below exist *only* at the prompt; while a program runs (or in alt-screen) Termica is a transparent byte pipe and those same keys are raw input to the program.

| Key | `ShellPromptEditor` (prompt) | `RawTerminal` (running a command) | `AlternateScreen` (vim / htop / less) |
|---|---|---|---|
| `Ctrl+C` | **Inert at an idle prompt** — nothing sent; **but** aborts a pending PS2 continuation (`0x03` + clears the editor — see [04 §Recovering from a stuck continuation](04-prompt-editor.md#recovering-from-a-stuck-continuation)) | `0x03` **SIGINT** to the foreground program | `0x03` **SIGINT** |
| `Ctrl+A` / `E` / `T` | Termica editor: line start / line end / transpose | C0 byte to the program (`readline` may act on it) | C0 byte to the program |
| `Tab` | Completion popup | `0x09` to the program | `0x09` to the program |
| Arrows / `Home` / `End` | Editor caret motion | VT escape to the program | VT escape (SS3 form under DECCKM) |
| `PageUp` / `PageDown` | Caret to buffer start / end (Shift extends) | `ESC[5~` / `ESC[6~` to the program | `ESC[5~` / `ESC[6~` |
| `Ctrl+Home/End/PageUp/PageDown` | Scroll the pane's scrollback (ends / by-page) — app-level, fires here too | Same — scrolls the block stack | No-op (program owns the viewport) |
| `Esc` | Inert no-op | `0x1b` to the program | `0x1b` to the program |
| `Ctrl+R` | History overlay (gated on `editor_is_active`) | `0x12` to the program (e.g. shell reverse-search) | `0x12` to the program |
| `Ctrl+D` on empty | EOF to the shell (the *one* chord the boundary gate passes) | `0x04` to the program | `0x04` to the program |

`AlternateScreen` routes the keyboard exactly like `RawTerminal`; it adds **mouse-wheel → arrow-key** forwarding (`input::compose_alt_screen_frame_bytes`) so the wheel pages the TUI, honors mouse reporting if the program enabled it, and suppresses block-stack scrollback. `Bootstrapping` drops input; `Dead` shows the restart UI (history/search still work); `Degraded` behaves as `RawTerminal` (no markers, so the editor never promotes).

### Physical keys not on a Mac keyboard

PC keyboards have keys Apple keyboards lack. Behavior:

- **`Home` / `End`** — editor: caret to line start / end (bare). `Ctrl+Home/End` scroll the pane's scrollback to top / bottom (app-level, both platforms — *not* an editor caret move). `RawTerminal`: bare keys encode to the shell (`ESC[H` / `ESC[F`, or the SS3 form under DECCKM).
- **`Delete`** (forward delete) — editor: delete the char to the right (or the word to the right with `Ctrl`). `RawTerminal`: `ESC[3~`.
- **`PageUp` / `PageDown`** — editor: move the caret to the buffer start / end (Shift extends); `Ctrl+PageUp/PageDown` scroll the scrollback by a page (app-level, both platforms). `RawTerminal`: bare keys go to the program (`ESC[5~` / `ESC[6~`) so `vim` / `less` / `htop` page normally.
- **`Insert`** — `RawTerminal`: `ESC[2~`. Editor: inert (no overwrite mode).
- **`PrintScreen` / `Pause` / `ScrollLock`** — egui exposes no `Key` variant for these, so they never reach Termica; they are no-ops everywhere (the OS / desktop environment handles `PrintScreen` for screenshots).

## Shipped (Phase 1 + Phase 2)

| Shortcut (macOS) | Shortcut (Linux / Windows) | Action | Source |
|---|---|---|---|
| `Cmd+T` | `Ctrl+Shift+T` | New tab in the focused pane's Tabs container. Inherits cwd from the active sibling pane. | Phase 2A / 2B |
| `Cmd+W` | `Ctrl+Shift+W` | Close the focused tab. Modal confirmation if a program is in alt-screen. On the last tab, routes to Quit. | Phase 2A |
| `Cmd+Q` | `Ctrl+Shift+Q` | Quit the app. Modal confirmation with a 60-second countdown if any program is in alt-screen. | Phase 2A |
| `Cmd+Shift+]` | `Ctrl+Shift+]` | Next tab in the parent Tabs container (wraps). | Phase 2A |
| `Cmd+Shift+[` | `Ctrl+Shift+[` | Previous tab in the parent Tabs container (wraps). | Phase 2A |
| `Cmd+K` | `Ctrl+Shift+K` | Clear scrollback **and** blank viewport on the focused pane. Cursor moves home. Shell is untouched; it redraws on next prompt cycle. | Phase 2 polish |
| `Cmd+Option+Up` / `Cmd+Option+Down` | `Ctrl+Alt+Up` / `Ctrl+Alt+Down` | Jump the focused pane's scroll position to the top / bottom of the sealed-block stack. No-op in alt-screen mode. On macOS, Cmd+Up / Cmd+Down alone (no Option) stay reserved for editor caret-to-doc-start / -end — the Option / Alt modifier disambiguates the scrollback jump. | Phase 4 polish |
| `Ctrl+Home` / `Ctrl+End` | `Ctrl+Home` / `Ctrl+End` | Scroll the scrollback to top / bottom (alias of the `…+Up/Down` jump above; same on both platforms). No-op in alt-screen. Bare `Home`/`End` stay with the editor caret / PTY. | ✅ |
| `Ctrl+PageUp` / `Ctrl+PageDown` | `Ctrl+PageUp` / `Ctrl+PageDown` | Scroll the scrollback **by a page** (toward older / newer output). Relative move via `Ui::scroll_with_delta`; releases stick-to-bottom like a wheel scroll. No-op in alt-screen. Bare `PageUp`/`PageDown` move the editor caret to the buffer ends. | ✅ |
| `Cmd+Shift+C` | `Ctrl+Shift+C` | Copy current selection to clipboard if non-empty; otherwise no-op. | Phase 1E-k |
| `Cmd+C` | `Ctrl+C` | Copy if selection is non-empty; otherwise SIGINT to the PTY. | spec/02:157 |
| `Cmd+F` | `Ctrl+Shift+F` | Open the in-pane find overlay. | Phase 8 |
| `Cmd+/` | `Ctrl+/` | Open the keyboard-shortcuts cheat-sheet (this list, platform-local; modifier key-caps are painter-drawn, ⌘ for Command). | ✅ |
| `Cmd++` / `Cmd+-` / `Cmd+0` | `Ctrl++` / `Ctrl+-` / `Ctrl+0` | Zoom the whole UI in / out / reset. **Inherited** from egui's built-in `zoom_with_keyboard` (on by default) — it scales the egui zoom factor (`pixels_per_point`), which reflows the terminal grid. We do not wire our own; the chords are consumed by egui at end-of-frame and never reach the PTY. | egui default |

**Popups are mutually exclusive.** The find overlay, the `Ctrl/Cmd+R` history overlay, the `Tab` completion popup, and the `Cmd+/` cheat-sheet never show two at once: invoking one closes whichever is open and opens the requested one. Each is modal — while it's up, keystrokes route to the popup, not the editor or PTY.

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
| `Esc` | `Esc` | **No-op** in the editor: consumed, nothing reaches the PTY, mode unchanged. (Dismisses an open popup first if one is showing.) The demote-to-`RawTerminal` path (`PromptController::leave_editor_esc` / `DemoteReason::Esc`) is implemented but **currently unbound** — dropping into raw I/O on an Esc was confusing and solved no problem; the machinery is retained for a future gesture. See [04 §Keymap](04-prompt-editor.md) and [05 ← `ShellPromptEditor`](05-pane-modes.md). | ✅ |
| `Cmd+A` | `Cmd+A` | Select all editor contents. On Linux this is the platform "command" modifier; plain `Ctrl+A` is reassigned to caret-to-line-start (below). | ✅ Phase 4D-poly ([#59](https://github.com/enthal/termica/pull/59)) |
| `Ctrl+A` | `Ctrl+A` | Caret to line start (readline / emacs tradition). Redundant with `Cmd+←` / `Home`. | ✅ |
| `Ctrl+E` | `Ctrl+E` | Caret to line end. Redundant with `Cmd+→` / `End`. | ✅ |
| `Ctrl+T` | `Ctrl+T` | Transpose the two characters around the caret (emacs `transpose-chars`); at line end, the last two. | ✅ |
| `Cmd+C / V / X` | `Ctrl+C / V / X` | Copy / paste / cut from the editor's selection. Cut records one undo entry so `select → cut → undo` restores the cut text **selected** ([04 §Undo/redo](04-prompt-editor.md#undo--redo)). | ✅ Phase 4D-poly ([#59](https://github.com/enthal/termica/pull/59)) |
| `Cmd+Z` / `Cmd+Shift+Z` | `Ctrl+Z` / `Ctrl+Shift+Z` | Undo / redo, scoped to the current editing session (reset on submit). Restores text **and** selection — see [04 §Undo/redo](04-prompt-editor.md#undo--redo). | ✅ Phase 4 polish |
| Arrow ← / → | Arrow ← / → | Move caret one char (with `Shift` extends selection). | ✅ Phase 4B |
| `Option+ArrowLeft` / `Option+ArrowRight` | `Ctrl+ArrowLeft` / `Ctrl+ArrowRight` | Move caret by word boundary (with `Shift` extends selection). | ✅ Phase 4D-poly ([#60](https://github.com/enthal/termica/pull/60)) |
| `Option+Backspace` / `Option+Delete` | `Ctrl+Backspace` / `Ctrl+Delete` | Delete the word to the left / right of the caret. | ✅ |
| `Cmd+Backspace` | `Ctrl+U` | Delete from the caret to line start (joins onto the previous line when the caret is at column 0 of a non-first line; no-op at buffer start). | ✅ |
| `Cmd+ArrowLeft` / `Cmd+ArrowRight` | `Home` / `End` | Move caret to line start / end (with `Shift` extends selection). | ✅ Phase 4D-poly ([#60](https://github.com/enthal/termica/pull/60)) |
| `Cmd+ArrowUp` / `Cmd+ArrowDown`, or `PageUp` / `PageDown` | `PageUp` / `PageDown` | Move caret to buffer start / end (with `Shift` extends selection). On Linux this is PageUp/PageDown — `Ctrl+Home`/`Ctrl+End` are reassigned to scrollback navigation (see Shipped). | ✅ |
| `Ctrl+C` on empty editor | `Ctrl+C` on empty editor | Leave editor; send SIGINT. | ⏳ Phase 4 polish |
| `Ctrl+D` on empty editor | `Ctrl+D` on empty editor | Send EOF to the shell (`exit` semantics). | ⏳ Phase 4 polish |
| `Up` / `Down` | `Up` / `Down` | Multiline-aware history walk per [04 §History walk (Up/Down)](04-prompt-editor.md#history-walk-updown). `↑` on row > 0 moves to the previous editor line; on row 0 it steps back through pane-scope history (saving `{text, cursor}` as an in-progress snapshot on first press). `↓` on row < last moves to the next line; on the last row it walks forward, restoring the snapshot — **text and caret** — at the head. Any non-arrow edit abandons the walk. Inert while the completion or `^R` overlay is open. | ✅ Phase 4J PR 5 (single-line + buffer save). Multiline + caret-restore: ⏳ |
| `Tab` | `Tab` | Local completion popup. Sources: paths under cursor, `$PATH` executables (in command position), command history (prefix-matched). `↑`/`↓` navigate, `Tab`/`Enter` accept, `Esc` cancels, any other key dismisses + falls through to the editor. | ✅ Phase 4I slice 1 |
| `Ctrl+R` / `Cmd+R` | `Ctrl+R` | Open the history overlay. Substring filter over the `runs` table; `↑`/`↓` to walk results, `Enter` to substitute, `Esc` to cancel, `Tab` to toggle scope (`global*` ↔ `this pane*`). Modal — all keystrokes route to the overlay until it closes. `Cmd+R` is the macOS addition; both work. | ✅ Phase 4J PR 6 + |
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
| Continuation marker re-entry | After a submit, a DCS-JSON `continuation` event from the shell (`PS2` fired) re-promotes the editor and restores `last_submitted + "\n"`; only the suffix beyond `last_submitted` is sent on the next submit. **Recovery** when the continuation is unwanted (e.g. `echo "!"`): Ctrl+C aborts the dangling line + clears the editor, and clearing/retyping a *different* command aborts the line while keeping the retyped text for a clean second Enter (2-Enter heal). See [04 §Recovering from a stuck continuation](04-prompt-editor.md#recovering-from-a-stuck-continuation). | ✅ Phase 4C polish ([#58](https://github.com/enthal/termica/pull/58)) |
| `Cmd+F` / `Ctrl+Shift+F` | In-pane find overlay over the focused pane's sealed blocks (literal / case-insensitive / regex; `Aa` + `.*` toggles; All/Commands/Outputs filter). `Ctrl+Shift+F` on Linux because plain `Ctrl+F` is readline `forward-char`. Fuzzy is post-MVP. | ✅ Phase 8 ([`feat/in-pane-search`](https://github.com/enthal/termica/tree/feat/in-pane-search)) |
| `↑` / `↓` (in find field) | Walk find-query history; `▾` opens the recent-query dropdown. | ✅ Phase 8 |
| `Enter` / `Shift+Enter` (in find field) | Find searches from the bottom: `Enter` steps **up** (previous / older match), `Shift+Enter` steps **down** (next / newer). `Esc` dismisses. | ✅ Phase 8 |
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
