**← Previous:** [05 — Pane modes](05-pane-modes.md) | **Next:** [07 — History & search](07-history-and-search.md) →

# 06 — Workspace & tiles

The native workspace layer. The product's job here is to make a multi-pane terminal feel like a modern IDE without losing the immediacy of a single terminal.

## Topology

```
App
└── Workspace
    └── Window (exactly one in v1)
        ├── TabBar
        └── TileTree (egui_tiles::Tree<PaneId>)
            ├── Tab "main"
            │   └── leaf PaneId(1)
            └── Tab "logs"
                ├── horizontal split
                │   ├── leaf PaneId(2)
                │   └── leaf PaneId(3)
                └── leaf PaneId(4)
```

- **Window**: exactly one OS window in v1. Multi-viewport (multiple OS windows) is post-MVP — egui's multi-viewport support is real but has rough edges we'd rather not adopt early.
- **Tab**: a named container with its own `egui_tiles::Tree`. The tab bar sits at the top of the workspace.
- **Tile tree**: `egui_tiles::Tree<PaneId>` owns layout. Leaves are pane IDs; the actual `Pane` lives in a registry owned by `App` (see [01](01-architecture.md)).
- **Pane**: one PTY session. The smallest closeable unit.

## Why split tile / pane / registry

- The `Tree<PaneId>` is the **layout**. It moves panes between tabs without touching their state.
- The `Pane` is the **state** (PTY, terminal, editor, history, scrollback).
- The `PaneRegistry` is the **owner**, keyed by `PaneId`.

This split is the same lesson as Knauty's tile/pane separation: layout operations (split, close-tab, drag-and-drop) must not rebuild pane state. A reparented pane must keep its PTY alive and its scrollback in place.

## Pane operations

| Operation | Notes |
|---|---|
| Spawn in current cwd | If active pane has a cwd, new pane inherits it; else `$HOME` |
| Split right / down | `egui_tiles::Tree::split_*` on the active leaf |
| Close pane | Confirm if PTY child is alive and unattended; kill PTY, drop registry entry, prune empty parents |
| Duplicate shell in same cwd | New pane with the same `cwd` and `ShellKind` |
| New pane in same "project" | Same cwd; reuses git/kube context |
| Rename | Pane title; overrides auto-title derived from cwd/shell |
| Zoom | Hide other panes in the tab; same data, larger viewport |
| Move to new tab | Detach leaf, create tab, attach |
| Move to other tab | Detach from current tab's tree, attach to target's |

A "duplicate shell" sets `cwd` but does **not** copy scrollback or transcript. Each pane is its own session.

## Tab operations

- New tab (default: one pane in current cwd).
- Close tab (confirms if any pane has a live unattended child).
- Rename tab.
- Reorder tabs via drag.
- Persisted as part of layout ([08](08-persistence.md)).

### Tab title and minimum width

Tab titles default to the home-relative cwd (`~/git/enthal/termica`, `~/projects/foo`, …) and are tracked in [`src/tab_title.rs`](../src/tab_title.rs). When the cwd collapses to bare `~` (the default startup cwd, so the worst case is also the first thing the user sees), the natural-width tab is ~16 px — barely clickable.

Rule: titles are space-padded to **`MIN_TAB_TITLE_CHARS = 7` characters** before being handed to `egui_tiles`. egui_tiles has no min-tab-width knob; widening via the title string is the cleanest cross-Behavior approach. Padding is split symmetrically with the extra character going to the right when the shortfall is odd (matches egui_tiles' `LEFT_CENTER` paint origin so the text stays visually centered). The constant was picked via `cargo run --example pick_tab_min_width` (variant `3x` ≈ 48 px, 3× the natural `~` width). Implemented as a pure `pad_to_min_chars` helper in [`src/behavior.rs`](../src/behavior.rs) with strict-layer tests.

## Active pane / focus

Exactly one pane is "active" per workspace. Focus follows mouse click or keyboard navigation; egui's focus system drives it. The active pane:

- receives input;
- shows a thin focus ring;
- has its status header tinted slightly to differentiate.

Background panes still **render** (their PTY keeps emitting; their grid updates) and still **persist** (scrollback writes); they just don't receive input.

## Status header

The header is a row of structured widgets above each pane's editor/transcript area. It replaces most of what `$PS1` used to carry.

### v1 chip set

Only these. Everything else is post-MVP.

| Chip | Source | Click action |
|---|---|---|
| 📂 cwd | OSC marker `TermicaCwd` → falls back to local probe | Copy path; reveal in Finder/Files |
| ⎇ git branch | `git symbolic-ref --short HEAD` (async, debounced) | Copy branch name |
| ± dirty count | `git status --porcelain` (async, debounced) | Open `git status` summary popover (later) |
| ✓ / ✗ last exit | `MarkerEvent::CommandEnd` from `PromptController` | Show command + exit popover (later) |
| ⏱ last duration | `MarkerEvent::CommandEnd.duration_ms` or computed delta | — |

```rust
pub struct PaneContext {
    pub cwd: Option<PathBuf>,
    pub git: Option<GitContext>,
    pub last_command: Option<LastCommandStatus>,
}

pub struct GitContext {
    pub branch: String,
    pub dirty_count: u32,
    pub ahead: u32,
    pub behind: u32,
    pub computed_at: monotonic::Instant,
}

pub struct LastCommandStatus {
    pub exit: i32,
    pub duration: Duration,
    pub command_summary: String, // first line, truncated
}
```

### Async probes

All header data updates asynchronously off the UI thread:

- **cwd**: marker-driven (cheap, instant). If markers are absent, debounced 200ms probe via `/proc` (Linux) or `lsof -p` (macOS) — best-effort.
- **git**: a single async task per pane, debounced 200ms after cwd changes. Cancel previous if a new cwd lands. Use `gix` or `git2`; pick post-MVP, in v1 shell out to `git` for simplicity.
- **last command**: free; comes from `PromptController`.

The UI never blocks on these. If a probe is in-flight, the chip shows its last known value with a subtle "stale" indicator.

### Iconography

Following the [CLAUDE.md](../CLAUDE.md) rule: **no Unicode pictographic icons** in widget code. The 📂 / ⎇ / ± / ✓ characters in this doc are placeholders for prose. The real implementation draws each icon via `egui::Painter` in an `icons.rs` module (knauty pattern). Specific glyphs and their painter routines are designed in the Phase 5 PR.

## Keyboard shortcuts (defaults)

Provisional; configurable post-MVP.

| Shortcut (macOS / Linux) | Action |
|---|---|
| Cmd+T / Ctrl+T | New tab |
| Cmd+W / Ctrl+W | Close pane (close tab if last pane) |
| Cmd+D / Ctrl+D (in pane) | Split right |
| Cmd+Shift+D / Ctrl+Shift+D | Split down |
| Cmd+] / Cmd+[ | Next / previous tab |
| Cmd+F / Ctrl+F | In-pane search |
| Cmd+Shift+F | Workspace search ([07](07-history-and-search.md)) |
| Cmd+P / Ctrl+P | Command palette (post-MVP) |
| Cmd+K / Ctrl+K | Clear visible transcript |
| Esc | Dismiss popups; never alone changes mode |

Default conflicts with shell behavior: Cmd+D / Ctrl+D in a pane that's already at an empty editor sends EOF (terminal-mode parity); in a non-empty editor it splits. We accept the slight ambiguity.

## Drag and drop

`egui_tiles` provides drag-to-rearrange. We add:

- Visual drop-zone hints on the tile boundaries.
- Drag pane title between tabs to detach/reattach.
- A pane registry that survives drag (because reparenting is layout-only).

## Theming

A single dark theme in v1. Minimal, polished, high-contrast for terminal text. Light theme post-MVP. Colors loaded from a small TOML file at `~/.config/termica/theme.toml`; missing file uses bundled defaults.

## What lives where

| Concern | Owner |
|---|---|
| Tile topology | `WorkspaceWindow.tiles: egui_tiles::Tree<PaneId>` |
| Pane state | `PaneRegistry: HashMap<PaneId, Pane>` |
| Active pane id | `WorkspaceWindow.active_pane` |
| Tab structure | `WorkspaceWindow.tab_bar: TabBarState` |
| Mode | `Pane.prompt: PromptController` ([05](05-pane-modes.md)) |
| Editor | `Pane.editor: PromptEditor` ([04](04-prompt-editor.md)) |
| Status header data | `Pane.context: PaneContext` |
| Persistence | `App.persistence: PersistenceHandle` ([08](08-persistence.md)) |

## Testing

- **egui_kittest snapshots** of: empty pane, populated pane with selection, pane in `ShellPromptEditor` mode with editor and header, alternate-screen pane (renders no header chrome by default).
- **Layout tests** (pure logic): split/close/reparent operations on the tile tree preserve pane state across reparents (no `Drop` of registered panes during tree mutation).
- **Focus tests**: focus traversal across panes / tabs is deterministic.

---

**← Previous:** [05 — Pane modes](05-pane-modes.md) | **Next:** [07 — History & search](07-history-and-search.md) →
