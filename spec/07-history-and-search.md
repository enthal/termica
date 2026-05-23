**← Previous:** [06 — Workspace & tiles](06-workspace-and-tiles.md) | **Next:** [08 — Persistence](08-persistence.md) →

# 07 — History & search

Command history and transcript search are core to what Termica claims to be ("as navigable as an IDE"). They are not bolt-ons. They are structural.

## Two stores, one shape

```
┌──────────────────────────┐         ┌──────────────────────────┐
│  Per-pane recent history │         │  Global command history  │
│  (in-memory ring + spill)│         │  (SQLite, persisted)     │
└────────────┬─────────────┘         └────────────┬─────────────┘
             │                                    │
             └──────────────┬─────────────────────┘
                            │
                   ┌────────▼────────┐
                   │  History UI:    │
                   │  Up-arrow walk  │
                   │  Ctrl-R popup   │
                   └─────────────────┘
```

Both stores live behind the same query trait so the UI is scope-agnostic.

```rust
pub trait HistoryQuery {
    fn search(&self, q: &HistorySearch) -> Vec<HistoryEntry>;
}

pub struct HistorySearch {
    pub text: String,
    pub fuzzy: bool,
    pub scope: HistoryScope,
    pub cwd: Option<PathBuf>,
    pub limit: usize,
}

pub enum HistoryScope {
    ThisPane(PaneId),
    ThisTab(TabId),
    ThisProject(PathBuf),   // typically the cwd's git root
    Global,
}
```

## `CommandRun` — the structured record

Every command Termica observes (whether submitted via editor or typed in raw mode) is recorded as a `CommandRun`. This is the unit of history, search, and command-block rendering.

```rust
pub struct CommandRun {
    pub id: CommandRunId,
    pub pane_id: PaneId,
    pub session_id: SessionId,
    pub command: String,
    pub cwd: PathBuf,
    pub shell: ShellKind,
    pub started_at: SystemTime,
    pub ended_at: Option<SystemTime>,
    pub duration: Option<Duration>,
    pub exit_status: Option<i32>,
    pub output_range: Option<ScrollbackRange>,  // see [08]
    pub context_snapshot: PaneContextSnapshot,  // git branch, etc, at start time
    pub origin: CommandOrigin,
}

pub enum CommandOrigin {
    EditorSubmit,             // from PromptEditor::submit() — the strong case
    RawObserved,              // command_start/command_end markers in raw mode (e.g. user typed without editor)
    Unknown,                  // no marker pair; best-effort
}
```

`CommandRun` lifecycle:

1. **Created** at `EditorSubmit` (cwd / shell / text known) OR at `MarkerEvent::CommandStart` in raw mode.
2. **Closed** at `MarkerEvent::CommandEnd` with `exit_status` and `duration_ms`.
3. **Closed open-ended** if `CommandEnd` never arrives (pane went to `Dead`, integration lapsed) — `exit_status = None`, `origin` annotated.

The pane's `CommandRunBuffer` tracks open runs; only one can be open at a time per pane.

## Pane-local history

In-memory, capped at `N` (config; default 5000). The most recent K are also held in memory; older spill to a per-pane history table in SQLite. UI:

- **Up arrow** in the editor: walks backwards through pane-local history. Down arrow walks forward.
- The popup is closed; this is the lightest-weight history surface.
- Multi-line history entries are reinserted exactly, including their newlines.

A walk does not record into history; only `submit()` does.

## Global history

SQLite-backed. Schema in [08](08-persistence.md). Every `CommandRun` writes one row at `started_at`; the row is updated at `ended_at`.

UI:

- **Ctrl+R** opens the history popup.
- Default scope: `Global`, sorted by recency, optionally cwd-biased.
- Fuzzy matcher: `nucleo` (v1 candidate) ranking by combined recency + cwd-proximity + match score.
- Scope toggles in the popup chrome: `[ this pane | this project | global ]`.
- Filters: `--this-project` mode shows only entries whose cwd is within the current git root.

```text
┌────────────────────────────────────────────────────────────────┐
│ Ctrl+R  search: cargo te█                                      │
├────────────────────────────────────────────────────────────────┤
│ ▸ cargo test --workspace                                       │
│   2m ago · ~/git/enthal/termica · exit 0                       │
│   cargo test --package termica-terminal                        │
│   17m ago · ~/git/enthal/termica · exit 0                      │
│   cargo test -p termica-shell --test markers                   │
│   yesterday · ~/git/enthal/termica · exit 0                    │
└────────────────────────────────────────────────────────────────┘
       scope: [ this pane | this project | global* ]
```

Selecting an entry replaces the editor buffer. The popup never sends to the PTY directly.

## Command blocks (transcript view)

The transcript view ([02](02-terminal-engine.md), [08](08-persistence.md)) is enriched with command-block decorations. A command block is one `CommandRun` plus its output range.

Visual structure:

```
┌── cargo test --workspace ────────────────────────── exit 0 · 2.3s ─┐
│ (output lines, ANSI-styled)                                        │
│ ...                                                                │
└────────────────────────────────────────────────────────────────────┘
```

Affordances:

- Click the header to collapse / expand the output.
- Context menu / hover icons: copy command, copy output, copy command+output, rerun, pin, jump-to-output.
- Failed (non-zero exit) blocks get a subtle red gutter mark.
- Blocks are non-modal — the transcript still flows; output below the block is the next prompt cycle.

`Rerun` puts the command back in the editor and submits it. It is **not** a "rerun-in-place" — the original block stays; the new run is a new block.

## Transcript model

```rust
pub enum TranscriptItem {
    /// Terminal-grid output not associated with any command (e.g. between
    /// shell launch and first prompt, or during raw mode without integration).
    TerminalLines(ScrollbackRange),

    /// A command block. References a CommandRun + output range.
    CommandBlock(CommandRunId),

    /// A session marker (e.g. shell exit, restart, integration version change).
    Marker(SessionMarker),
}
```

The transcript is a sequence of `TranscriptItem`s; rendering walks the sequence. The underlying scrollback chunks ([08](08-persistence.md)) are unchanged — `TranscriptItem` is a structuring view on top.

## Search

```rust
pub enum SearchScope {
    Pane(PaneId),
    Tab(TabId),
    Window(WindowId),
    Workspace,
    Global,
    CommandHistoryOnly,
    OutputOnly,
}

pub struct SearchQuery {
    pub text: String,
    pub mode: SearchMode,   // Literal / CaseInsensitive / Regex / Fuzzy
    pub scope: SearchScope,
}

pub struct SearchResult {
    pub pane_id: PaneId,
    pub command_run_id: Option<CommandRunId>,
    pub scrollback_position: ScrollbackPosition,
    pub matched_line: String,
    pub match_span: Range<usize>,
}
```

### v1 scopes (shipping)

- `Pane(PaneId)` — current pane's visible buffer + full in-memory scrollback. **Phase 8 deliverable.**
- `CommandHistoryOnly` — `Ctrl+R` popup. **Phase 6 deliverable.**

### Post-MVP scopes

- `Tab` / `Window` / `Workspace` — across multiple panes.
- `Global` — across all persisted sessions, including ones that aren't currently open. Indexed.
- `OutputOnly` — exclude command lines.

Search engine: literal + case-insensitive in v1 (Phase 8). Regex via `regex`; fuzzy via `nucleo`. Tantivy for global persisted output search is post-MVP.

## Result navigation

In-pane search opens a small overlay at the top of the pane:

```
┌─ find ─────────────────────────────────────────────────────────────┐
│ "error" · 3 of 14 · [Aa] [.*] [⇡] [⇣] [Esc]                        │
└────────────────────────────────────────────────────────────────────┘
```

- ⇡ / ⇣ jump to previous / next match, scrolling the transcript.
- Match highlights paint as overlays in the cell renderer.
- Esc dismisses.

## What's deliberately not in v1

- A "command palette" combining history + actions. Post-MVP.
- Semantic output highlighting (Rust diagnostics, JSON, stack traces). Post-MVP, possibly far post-MVP.
- Tag / bookmark / annotate command blocks. Post-MVP.
- Sharing or exporting a command block as Markdown. Post-MVP.

Each of those is a real, good idea. v1 stays disciplined.

## Testing

- **Unit (strict)**: `CommandRun` lifecycle transitions (open / close / open-ended-close).
- **Unit (strict)**: history record happens at `submit()` not at editor walk.
- **Unit**: scope filter behavior (this-project derives from git root; this-pane respects PaneId).
- **Snapshot (egui_kittest)**: `Ctrl+R` popup, command-block rendering (collapsed / expanded / failed-exit gutter), in-pane search overlay.
- **Integration**: a scripted session of submit / search / select-from-history through the real `PromptController` produces the expected `CommandRun` rows in SQLite.

---

**← Previous:** [06 — Workspace & tiles](06-workspace-and-tiles.md) | **Next:** [08 — Persistence](08-persistence.md) →
