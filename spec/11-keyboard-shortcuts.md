**← Previous:** [10 — Roadmap](10-roadmap.md) | **Up:** [SPEC index](../SPEC.md)

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

## Planned (Phase 3 onward)

The full list with phase ownership. **None of these are wired today.**

| Shortcut | Action | Phase |
|---|---|---|
| `Enter` (in editor) | Submit the command. Eager demote, then PTY write. | Phase 4 |
| `Shift+Enter` (in editor) | Insert a newline (multiline edit). | Phase 4 |
| `Esc` (in editor) | Leave editor; demote to `RawTerminal`. | Phase 4 |
| `Ctrl+C` (in editor, empty) | Leave editor; send SIGINT. | Phase 4 |
| `Ctrl+D` (in editor, empty) | Send EOF to the shell (`exit` semantics). | Phase 4 |
| `Up` / `Down` (in editor) | Walk pane-local history. | Phase 4 / 6 |
| `Tab` (in editor) | Local completion (paths + history + `$PATH`). | Phase 4 |
| `Ctrl+R` | Global command-history popup. Fuzzy match. | Phase 6 |
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

**← Previous:** [10 — Roadmap](10-roadmap.md) | **Up:** [SPEC index](../SPEC.md)
