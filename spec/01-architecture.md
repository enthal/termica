**← Previous:** [00 — Overview](00-overview.md) | **Next:** [02 — Terminal engine](02-terminal-engine.md) →

# 01 — Architecture

## The three layers

Termica is one product made of three sharply separated layers. Confusing them is how every "modern terminal" project fails.

```
┌────────────────────────────────────────────────────────────────────┐
│                       NATIVE WORKSPACE LAYER                       │
│  egui app · egui_tiles · tabs · splits · status header · editor    │
│  command history · search · command blocks · persistence           │
└──────────────────┬─────────────────────────────────────────────────┘
                   │  pane modes / markers / commands / events
┌──────────────────▼─────────────────────────────────────────────────┐
│                     STRUCTURED SHELL LAYER                         │
│  PromptController state machine · OSC marker stream · CWD/exit     │
│  command-run lifecycle · history capture · echo suppression        │
└──────────────────┬─────────────────────────────────────────────────┘
                   │  bytes in / bytes out / grid state / events
┌──────────────────▼─────────────────────────────────────────────────┐
│                    TERMINAL COMPATIBILITY LAYER                    │
│  portable-pty · alacritty_terminal grid/escape state · custom      │
│  egui cell renderer · input encoding · alternate screen            │
└────────────────────────────────────────────────────────────────────┘
```

### Layer responsibilities

| Layer | Owns | Does NOT own |
|---|---|---|
| Terminal compatibility | PTY lifecycle; VT/ANSI interpretation; grid + scrollback state; alternate screen; cursor; selection geometry; cell painting; keyboard/mouse encoding | Prompt detection, command lifecycle, mode decisions, persistence, history |
| Structured shell | Marker stream parsing; `PromptController` mode machine; `CommandRun` lifecycle; cwd/exit/duration tracking; echo suppression buffer; integration installer | Grid state; rendering; pixel layout; egui widgets |
| Native workspace | Window/tab/pane topology; editor widget; status header; history UI; search UI; persistence orchestration; settings | PTY bytes; escape interpretation; prompt detection logic |

Each lower layer is a library to the layer above. Information flows up through events and accessors; control flows down through method calls. No layer reaches across more than one boundary.

## Data flow

```
PTY bytes ──► alacritty_terminal ──► grid state ──► cell renderer ──► egui paint
                    │
                    ├──► intercepted OSC markers ──► marker stream
                    │
                    └──► transcript lines ──► scrollback store

egui input ──► input encoder ──► PTY bytes  (when pane in RawTerminal/AlternateScreen)
egui input ──► PromptEditor                  (when pane in ShellPromptEditor)
PromptEditor.submit() ──► PTY bytes + newline
```

Two things matter about this picture:

1. **OSC markers are intercepted in the terminal layer**, before they become visible output. The structured-shell layer subscribes to a marker stream, not to text. The user never sees a stray `^[]133;A\` glyph.
2. **Input has two destinations**, chosen by the `PromptController`. There is no shared input pipeline that "both" the editor and the PTY snoop on — that would invite mode bugs.

## Components

### Per-pane

```rust
struct Pane {
    id: PaneId,
    pty: PtySession,                 // Layer 1: PTY handle
    terminal: TerminalState,         // Layer 1: alacritty_terminal grid + scrollback
    transcript: TranscriptStore,     // Layer 2/3: normalized lines + style spans
    markers: MarkerStreamRx,         // Layer 2: parsed OSC marker events
    prompt: PromptController,        // Layer 2: mode state machine
    editor: PromptEditor,            // Layer 3: native editor when prompt is active
    history: PaneHistory,            // Layer 3: pane-local recent commands
    search: PaneSearchState,         // Layer 3: in-pane search
    context: PaneContext,            // Layer 3: cwd, git, last exit, duration
    layout: PaneLayout,              // Layer 3: cell metrics, scroll offset
}
```

### Workspace

```rust
struct App {
    workspace: Workspace,
    global_history: GlobalHistoryStore,   // SQLite-backed; [07]
    persistence: PersistenceHandle,       // SQLite + chunk store; [08]
    integration_installer: IntegrationInstaller, // shell rc patcher; [03]
    config: Config,                        // TOML; tiny in v1
}

struct Workspace {
    window: WorkspaceWindow,               // exactly one in v1
}

struct WorkspaceWindow {
    title: String,
    tiles: egui_tiles::Tree<PaneId>,
    active_pane: Option<PaneId>,
    tab_bar: TabBarState,
}
```

The `Workspace` has its own draft above any pane; the `Tree<PaneId>` is the `egui_tiles` topology and the `PaneRegistry` (held inside `App`) maps `PaneId` → `Pane`. Splitting registry from tree is what lets a pane move between tabs without rebuilding state.

## Crate layout (target)

Commit zero is a single crate. As phases land, the workspace expands. The intended shape — codified here so we don't drift — is:

```
termica/                   <-- workspace root
├── Cargo.toml             (workspace)
├── crates/
│   ├── termica-app/       Phase 0–2: eframe entry point, workspace, tiles
│   ├── termica-terminal/  Phase 1: alacritty_terminal + portable_pty + renderer
│   ├── termica-markers/   Phase 3: OSC parser, marker event stream
│   ├── termica-shell/     Phase 3–4: PromptController state machine
│   ├── termica-editor/    Phase 4: PromptEditor widget, syntax, completion
│   ├── termica-history/   Phase 6: pane-local + global history + search
│   ├── termica-context/   Phase 5: cwd/git/last-exit chip providers
│   ├── termica-persist/   Phase 9: SQLite schema + chunked scrollback
│   ├── termica-integration/   Phase 3: bash + zsh scripts + installer
│   └── termica-types/     shared typed IDs and small enums
└── testdata/
    ├── vt/                Phase 1+: recorded PTY byte streams for golden tests
    └── markers/           Phase 3+: marker stream fixtures
```

Dependency direction is one-way: app → editor → shell → markers → terminal → types. Persistence and history are leaves that the app depends on but that don't depend on each other. Integration is a leaf (scripts + installer) that shell may depend on for the script's expected protocol version, but never vice versa.

**Until the workspace expands, all of this lives in the single root crate as modules with the same names.** When a module crosses ~500 LoC or grows its own tests directory, it earns its own crate in a follow-up PR.

## What runs where

Everything runs in one OS process. Inside that process:

- One **egui main thread** drives `update()` calls, paints, and owns the workspace.
- A **Tokio multi-thread runtime** runs:
  - one PTY-read task per pane (drains the PTY into the terminal state under a per-pane lock or via an SPSC channel — to be decided in [02](02-terminal-engine.md));
  - async context probes (`git status`, etc.) on a debounced pool;
  - async search/indexing for global history.
- A **persistence task** drains a write-ahead queue for SQLite + chunk-file writes; UI never blocks on disk.

There is no separate process for the shell, no separate process for persistence. A multi-process / daemon model is post-MVP and is called out as such in [10](10-roadmap.md).

## Cross-cutting concerns

### Typed IDs

```rust
pub struct PaneId(u64);
pub struct SessionId(u64);
pub struct WindowId(u64);
pub struct TabId(u64);
pub struct CommandRunId(u64);
pub struct HistoryEntryId(u64);
pub struct ScrollbackChunkId(u64);
```

All durable. Newtypes, not type aliases. The compiler refuses to mix a `PaneId` and a `SessionId`. See [CLAUDE.md](../CLAUDE.md).

### Tracing

`tracing` from the first commit. Spans:

- `pane=<id>` on every per-pane log line.
- `mode=<RawTerminal|ShellPromptEditor|...>` on transitions.
- `marker=<event>` on every parsed marker.
- `command_run=<id>` for the lifetime of a command.

The CLI accepts `RUST_LOG` and a `--dump-events` flag that writes the marker stream + mode transitions to a file for offline debugging. We will lean on this.

### Error handling

- `thiserror` for crate-level error enums.
- No `unwrap` / `expect` in non-test code except where a contract makes failure impossible. `expect` messages describe the invariant, not the call.
- Errors that abort a pane (PTY death) transition it to `Dead` and surface a restartable UI — they do not crash the app.

---

**← Previous:** [00 — Overview](00-overview.md) | **Next:** [02 — Terminal engine](02-terminal-engine.md) →
