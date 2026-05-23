**← Previous:** [SPEC index](../SPEC.md) | **Next:** [01 — Architecture](01-architecture.md) →

# 00 — Overview

## Goal

Build a native, Rust+egui terminal workspace where:

- the **terminal layer is a real terminal** (correct under vim/htop/less/fzf/ssh/tmux), and
- the **command-line prompt is a real editor** (click, drag, select, multiline, undo, history search), and
- the **mode-machine in between** decides safely which of those two layers receives the user's next keystroke.

The product wins by being both — not by trading one for the other.

## In scope (v1 / MVP)

- Native eframe + egui application; one OS window for v1; multi-viewport later if it earns its keep.
- Workspace of tabs and splits via `egui_tiles`; each pane owns a PTY session.
- Real PTY-backed shells (`bash`, `zsh`) via `portable-pty`.
- Terminal correctness via `alacritty_terminal` (grid, cursor, escapes, alternate screen, 24-bit color).
- Custom egui cell renderer (we paint cells directly; egui's text widgets are not load-bearing here).
- OSC-marker-based shell integration (OSC 133 + Termica-private extensions); one-shot installer.
- Pane-mode state machine: `RawTerminal` / `ShellPromptEditor` / `AlternateScreen` / `Dead`.
- Editor-driven prompt at known shell prompts: click-to-place cursor, drag selection, multiline editing, undo/redo, syntax highlighting, history popup, local completion.
- Structured command-run lifecycle: id, command, cwd, exit, duration, output range.
- Pane-local + global command history, SQLite-backed, fuzzy-searchable.
- In-pane search.
- Pane status header with cwd, git branch, dirty count, last exit, duration. Async, debounced.
- Scrollback that spills to disk; transcripts (not live PTYs) survive a restart.
- Dual MIT / Apache-2.0 license, Rust-ecosystem standard.

## Out of scope (v1)

- A Windows terminal. macOS and Linux first. Windows is later or never.
- A terminal multiplexer. tmux still works inside Termica.
- A shell. Termica embeds your existing `bash` or `zsh`.
- A REPL-aware editor for `psql` / `python` / `node`. They get raw-terminal behavior, which is good enough.
- A shell completion reimplementation. v1 ships local completion (paths, history, PATH executables). Deep zsh/bash completion bridges are post-MVP.
- A session daemon that keeps PTYs alive across app restart. v1 restores transcripts; the pane is `Dead` until the user restarts the shell.
- A command palette, custom themes, icons, animations, image protocols (kitty / iTerm2), OSC 8 hyperlinks. All post-MVP.
- Configuration UI. v1 reads a small TOML config file; UI for editing it is post-MVP.

## Non-goals (and why)

- **A new VT/ANSI interpreter.** `alacritty_terminal` has years of correctness work; we use it. The `vte` crate alone is not enough — we need state, not just a parser.
- **Editor-driven prompt without shell integration.** Heuristic prompt detection is unsafe; we will not ship it. If integration isn't installed, the app is still a usable terminal, but the prompt editor is unavailable.
- **A clever readline replacement that hooks into the shell line editor.** We never edit "in" the shell; we **edit in our app** and **send a finished command** to the shell. That is the entire safety story.
- **Best-effort persistence.** A "we usually save things" terminal is worse than a "we always save things" terminal. Persistence is structural.

## The five safety and correctness rules (normative)

These are repeated in [05 — Pane modes](05-pane-modes.md) but worth knowing up front. Code that violates one of these is a P0 bug, not a design preference.

1. **The prompt editor is NEVER active unless the pane is at a trusted, marker-confirmed shell prompt.** Default mode is `RawTerminal`. Unknown state is `RawTerminal`.
2. **Alternate screen always disables the prompt editor.** No exceptions. No "smart" detection of safe alternate-screen apps.
3. **Shell integration markers are authoritative.** Heuristics can enhance but must not be required for correctness.
4. **The shell never sees editing keystrokes from the editor.** Only the final command bytes plus a newline.
5. **Real TTY programs receive raw input.** Terminal mouse reporting, bracketed paste, raw keys — encoded correctly, forwarded promptly.

## Glossary

| Term | Meaning |
|---|---|
| **Pane** | One PTY session plus its terminal state, prompt controller, scrollback, and UI. |
| **Tab** | A named collection of panes within a window's tile tree. |
| **Window** | A top-level OS window. v1: exactly one. |
| **Tile** | A node in the `egui_tiles` layout tree (a leaf is a pane). |
| **Pane mode** | The state-machine state: `RawTerminal` / `ShellPromptEditor` / `AlternateScreen` / `Dead`. |
| **Marker** | An OSC-emitted shell-integration event (prompt start/end, command start/end, cwd, exit status). |
| **Prompt controller** | The per-pane state machine that consumes markers and decides mode. |
| **Command run** | A structured record of one executed command (cwd, command text, exit, duration, output range). |
| **Scrollback chunk** | An append-only file holding a contiguous range of transcript lines. |
| **Transcript** | The history of what the terminal grid displayed, normalized into text + style spans. |
| **Echo suppression** | The Enter-time mechanism that prevents the shell's local echo of a Termica-submitted command from appearing twice in the transcript. See [04](04-prompt-editor.md). |
| **Alternate screen** | A second VT cell grid (no scrollback of its own) that programs enter with `CSI ? 1049 h` and leave with `CSI ? 1049 l`. While the alternate screen is active, the main screen contents are preserved untouched; on exit they reappear. Used by full-screen TTY programs like `vim`, `htop`, `less`, `fzf`, `tmux`. See [02](02-terminal-engine.md) for how Termica handles it. |
| **Main screen** | The normal cell grid: shell prompts, command output, and scrollback live here. Restored unchanged when a program exits the alternate screen. |

## Conventions used in this spec

- **MUST / SHOULD / MAY** follow RFC 2119 meanings.
- Code blocks are illustrative Rust unless otherwise noted; **trait shapes are normative, bodies are not**.
- Diagrams use ASCII or Mermaid; keep them greppable.
- Cross-references use relative links so the spec is navigable on GitHub.
- "Alacritty does X" / "Warp does X" is shorthand for "the design we're echoing has been validated by that project's experience"; we are not bound by their choices.

## Design tenets (in priority order)

1. **Mode safety is the product.** When in doubt, the pane is `RawTerminal`. Editor surface area is a privilege the pane earns by reaching a confirmed prompt.
2. **Terminal correctness first.** Boring and right. vim/htop/less must be indistinguishable from any other modern terminal.
3. **Same code paths in every environment.** Headless tests, debug builds, and release builds all go through the same `PromptController`. No "test mode" inside the safety machine.
4. **Make wrong states unrepresentable.** If an invariant requires "always do X before Y," encapsulate both in a single function. Don't rely on comments or discipline.
5. **No silent data loss.** Persistence is structural, not best-effort. Confirmed-written means survives a crash.
6. **Boring tools where they matter most.** `alacritty_terminal`, `portable-pty`, SQLite. Save novelty for where it pays off: the editor, the structured command lifecycle, the renderer.
7. **Operability matters from the first commit.** Tracing, structured logs, a `--dump-events` path for offline marker-stream debugging.
8. **No `unsafe` code in Termica's own crates.** Every crate sets `#![forbid(unsafe_code)]` at the crate root. OS-level concerns (PTY handles, VT state) are delegated to `portable-pty` and `alacritty_terminal`, which contain `unsafe` themselves but expose safe APIs. If a future change feels like it needs `unsafe`, that is a signal to find a different abstraction or a different dependency, not to disable the lint.

---

**← Previous:** [SPEC index](../SPEC.md) | **Next:** [01 — Architecture](01-architecture.md) →
