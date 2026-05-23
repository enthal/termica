**← Previous:** [01 — Architecture](01-architecture.md) | **Next:** [03 — Shell integration](03-shell-integration.md) →

# 02 — Terminal engine

The terminal engine is Layer 1. It does one thing extremely well: be a correct terminal. Everything above it depends on this layer being boring.

## Background: alternate screen

This document and others reference the **alternate screen** throughout, so a quick anchor: VT terminals have two cell grids — the **main screen** (the normal one your shell prints into, with scrollback) and the **alternate screen** (a second buffer the same size, with no scrollback of its own). A program switches to the alt screen by emitting `CSI ? 1049 h` and back with `CSI ? 1049 l`; the older `CSI ? 1047` and `CSI ? 47` variants behave similarly. On entry the cursor is saved and the alt grid is cleared; on exit the alt grid is discarded and the main screen reappears exactly as it was before.

Full-screen TTY programs use this feature: `vim`, `nvim`, `emacs -nw`, `less`, `more`, `top`, `htop`, `tmux`, `screen`, `fzf`, `man`, `ssh -t` into a TUI. Without alt screen, opening `vim` would scroll your shell history off the screen and leave vim's last frame permanently embedded in your scrollback when you quit.

For Termica this matters in two ways:

- `alacritty_terminal` tracks the current screen via `Term::mode().contains(TermMode::ALT_SCREEN)`; we never look at the escape bytes ourselves.
- Alt-screen-on is a hard signal that the program below owns every keystroke. The pane unconditionally transitions to `AlternateScreen` mode ([05](05-pane-modes.md)) regardless of any other state.

## Choice of components

| Concern | Choice | Why |
|---|---|---|
| PTY spawn / read / write / resize | [`portable-pty`](https://crates.io/crates/portable-pty) | Cross-platform; well-tested by Wezterm; clean async-friendly API |
| VT/ANSI interpretation + grid state | [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) | Decades of correctness work; same engine that powers Alacritty |
| Rendering | Custom egui `Painter` cell renderer | Direct paint for performance; styled-run batching; correct fixed-width metrics |

We do not roll our own VT parser. We do not render via egui's `RichText`. Both are dead-ends for performance and correctness.

`alacritty_terminal` provides grid state, not pixels — that part we own. The crate is a `no_std`-ish library; we depend on it directly, not on the Alacritty application.

## PTY layer

```rust
pub struct PtySession {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    size: PtySize,
    shell: ShellKind,
    cwd: PathBuf,
}

pub enum ShellKind { Bash, Zsh, Other(String) }
```

Responsibilities:

- Spawn a shell with the right rcfile / init args so our shell integration loads (see [03](03-shell-integration.md)).
- Read bytes asynchronously into a buffer that the terminal-state task drains.
- Write input bytes promptly. No coalescing on the input side.
- Honor resize: when the egui pane's cell-grid dimensions change, set the PTY size before any further input is delivered.
- Detect child exit; signal Layer 2 to transition the pane to `Dead`.
- Clean shutdown: SIGHUP + close master fd.

### Read path

The PTY produces bytes. A dedicated background task — one per pane — drains the read end and forwards bytes into the `alacritty_terminal::Term`'s parser. Batching:

- Read into a bounded buffer (e.g. 16 KiB).
- Feed bytes into the parser, which mutates grid state and emits OSC events.
- After draining the current readable batch, request **one** egui repaint.
- Backpressure: if the UI thread can't keep up, the read task waits for a frame to complete before the next read. This is how we avoid per-byte repaints under `yes`-style output.

### Write path

Two writers:

- The `PromptController` writing a finished command on Enter (`ShellPromptEditor` → submit).
- The input encoder when the pane is in `RawTerminal` / `AlternateScreen`.

Both go through `PtySession::write_bytes`. There is no third writer.

## Terminal state

```rust
pub struct TerminalState {
    term: alacritty_terminal::Term<EventListener>,
    parser: alacritty_terminal::vte::Parser,
    on_alternate_screen: bool,
    cursor_visible: bool,
    mouse_reporting: MouseReportingMode,
    bracketed_paste: bool,
    /// Markers extracted from OSC sequences before they reach the grid.
    marker_tx: MarkerStreamTx,
}
```

`Term` owns the grid, the cursor, the alternate screen buffer, and the selection model. We do **not** subclass or fork it — we wrap it.

### Marker interception (preview, full detail in [03](03-shell-integration.md))

`alacritty_terminal` lets us inspect OSC sequences before they affect the grid. Our flow:

1. Parser emits an OSC.
2. We match against:
   - **OSC 133** (`A`, `B`, `C`, `D`) — FinalTerm/iTerm2 prompt markers; consume.
   - **OSC 1337 ; Termica=...** — Termica-private extensions; consume.
   - Anything else — passthrough to `Term` normal handling.
3. Consumed OSCs emit a `MarkerEvent` on the marker stream and do not contribute to the grid.

"Consume" means: do not let the bytes reach the screen, do not let them affect the cursor, do not record them in the transcript.

### Alternate screen

When `Term` reports `is_alternate_screen() = true`:

- The pane mode transitions to `AlternateScreen` ([05](05-pane-modes.md)).
- The main scrollback is preserved untouched.
- Input routing changes per [05](05-pane-modes.md).
- On exit, mode returns to `RawTerminal` (never directly to `ShellPromptEditor` — only a fresh prompt marker can promote).

## Rendering

The renderer paints the visible portion of the grid into an `egui::Painter`. Performance matters: the renderer must handle ≥ a screen of output per frame at 60 fps under load.

### Cell metrics

```rust
pub struct CellMetrics {
    pub cell_w: f32,
    pub cell_h: f32,
    pub line_h: f32,
    pub baseline_offset: f32,
    pub font: egui::FontId,
}
```

Computed once per font/size change, cached. The grid renderer assumes fixed-width cells; non-monospace fonts are not supported.

DPI: `egui::Context::pixels_per_point` is honored. Metrics are recomputed when DPI changes.

### Paint pipeline

Per visible row:

1. Walk the row's cells and **batch styled runs** — consecutive cells with the same foreground/background/style become one paint call.
2. Background rectangles paint first.
3. Glyphs paint on top.
4. Cursor, selection highlights, and search highlights paint last.
5. The marker-derived **command-block decorations** ([07](07-history-and-search.md)) paint as overlays.

We avoid `egui::RichText` entirely. Glyphs are laid out via `egui::FontId` and `epaint::Galley` with one-glyph layouts cached.

### Cursor

- Shape: block / underline / bar — driven by the terminal's cursor shape escape, with a config default.
- Blink: optional, driven by a wall-clock-independent frame counter; off in tests.
- Visibility: respect DECTCEM (`CSI ? 25 h/l`). Render nothing when invisible.

### Selection

`alacritty_terminal` has a selection model; we drive it from egui mouse events when the pane is in a mode where mouse-as-selection makes sense (always except when the terminal has enabled mouse reporting in `RawTerminal` / `AlternateScreen`).

- Click: clear selection, place cursor (no-op in raw mode; cosmetic).
- Drag: extend selection.
- Double-click: word selection.
- Triple-click: line selection.
- Cmd+C / Ctrl+C: copy selection if non-empty, else send `^C` to PTY in raw mode.

### Colors

- 16-color named palette: configurable via theme.
- 256-color cube and grayscale ramp: standard mapping.
- 24-bit truecolor: passed through directly.
- The renderer applies a configurable foreground/background base when terminal doesn't specify one.

## Input encoding

When the pane is in `RawTerminal` / `AlternateScreen`, egui input is encoded into terminal byte sequences:

| egui input | Encoded as |
|---|---|
| Printable text | UTF-8 bytes |
| Enter | `\r` (CR), or `\n` per application keypad mode |
| Backspace | `\x7f` (DEL) by default; `\x08` if shell expects it |
| Tab | `\t` |
| Arrows | `\x1b[A`/`B`/`C`/`D` or DECCKM `SS3` variant when application cursor mode active |
| F-keys | Per termcap; we ship a `xterm-256color` profile |
| Modifiers | Encoded per xterm conventions |
| Paste | Wrapped in `\x1b[200~` … `\x1b[201~` if bracketed paste is enabled; otherwise raw bytes |
| Mouse | Encoded per the terminal's active mouse reporting mode (SGR preferred), only when reporting is enabled |

When the pane is in `ShellPromptEditor`, **none of this happens**. Input goes to the editor; only the editor's `submit()` produces PTY bytes.

## TERM and termcap

Termica advertises itself as `xterm-256color` by default. A future `termica` termcap entry can ship as a side artifact but is not required for v1. The shell sees a perfectly normal xterm-class terminal.

## What we do NOT support in v1

- Image protocols (Kitty graphics, iTerm2 inline images).
- OSC 8 hyperlinks (clickable URLs in output) — Phase 11+.
- Sixel.
- `CSI ? 2004 l/h` toggling is honored (bracketed paste); IME composition forwarding is best-effort and should not break under Latin input.

Each of these is a defensible v1 omission, listed for the avoidance of doubt.

## Testing strategy specifics

See [09 — Testing](09-testing.md) for the full strategy. For Layer 1 specifically:

- **VT golden tests**: a recorded PTY byte stream is fed into the engine; assertions on the resulting grid and on the marker event stream. Cases:
  - `bash --rcfile our-integration`: prompt, simple command, exit status.
  - `vim` smoke: alternate screen enter/exit, cursor movement, color.
  - `less` smoke: alternate screen with mouse reporting.
  - `htop` smoke: dense styled output, alternate screen.
  - `fzf` smoke: bracketed paste, mouse, alternate screen.
  - **Split-read robustness**: the same byte stream chunked across read boundaries at every offset should produce identical grid + marker output.
- **Renderer perf smoke**: 100 MB of `yes`-style output is consumed in under N seconds at sensible CPU. Repaints are coalesced (no per-byte frames).
- **Resize tests**: PTY size changes mid-stream do not corrupt the grid.

---

**← Previous:** [01 — Architecture](01-architecture.md) | **Next:** [03 — Shell integration](03-shell-integration.md) →
