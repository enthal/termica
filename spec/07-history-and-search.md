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

The transcript view is structured as a vertical stack of **command blocks**, each block being one command + its output area + its header chrome. The block data model lives in Phase 4 / [spec/04](04-prompt-editor.md#visual-structure-the-block-model); the underlying live-`Term` / sealed-snapshot architecture lives in [spec/02](02-terminal-engine.md#the-block-model-one-live-term-many-sealed-snapshots). What this document adds is the *enrichment* of those blocks with command-run metadata and the per-block search affordances.

Each `Sealed` block ([04](04-prompt-editor.md)) carries a `BlockHeader` (cwd, git branch, dirty summary) and a `command + duration + exit` summary; these are the same fields populated into the persisted `CommandRun` record described below.

Per-block affordances (Phase 7+ polish):

- Click the header to collapse / expand the output.
- Context menu / hover icons: copy command, copy output, copy command+output, rerun, pin, jump-to-output.
- Failed (non-zero exit) blocks get a subtle red gutter mark.
- Blocks are non-modal — scrolling moves them as a stack, but each is independently selectable / collapsible.

`Rerun` puts the command back in the editor and submits it. It is **not** a "rerun-in-place" — the original sealed block stays; the new run is a fresh `Running` → `Sealed` block at the bottom of the stack.

## Transcript model

```rust
pub enum TranscriptItem {
    /// A command block (Sealed / Running / Prompt — see spec/04).
    /// References the in-memory block by id; persisted via CommandRun.
    CommandBlock(BlockId),

    /// A session marker (e.g. shell exit, restart, integration version change).
    Marker(SessionMarker),
}
```

The transcript is a sequence of `TranscriptItem`s; rendering walks the sequence. In v1 (Phase 4) every block is a `CommandBlock` — there is no "terminal lines not associated with any command" item, because the block model in [02](02-terminal-engine.md) makes every byte the property of *some* block (the one whose live `Term` was alive when the byte arrived). `Marker` items land in Phase 8+ as cross-cutting session events.

## Search

The find feature is **block-oriented**: every scope below is expressed
in terms of [command blocks](#what-is-a-block), and a match is reported
with the block it lives in. This makes "next match" navigation step
through blocks rather than raw scrollback rows, and makes scopes like
`SelectedBlocks` natural.

### What is a "block"?

A **block** is one [`CommandRun`](#command-runs): the prompt + the
command (whether the user is still editing it, has just submitted it,
or finished it long ago) + every byte of stdout / stderr / OSC marker
output that arrived between its `command_start` and `command_end`
markers. Equivalently, it's what a single header chrome wraps in the
transcript view ([command blocks](#command-blocks-in-the-transcript)).

A block is the unit the user thinks in — "the command I just ran",
"the previous build", "the failed deploy" — and the unit `find`
operates over.

### Scope model

```rust
pub enum SearchScope {
    /// The block(s) the user has explicitly selected in the
    /// transcript. Empty → no-op.
    SelectedBlocks,

    /// The most recently opened block in the focused pane. If the
    /// user is mid-edit at the prompt, that's "the current block".
    /// Otherwise it's the block that was just closed.
    LastBlock,

    /// **Default.** Every block in every pane of the currently
    /// active tab. Across-split scopes naturally fall out of this
    /// once Phase 2B splits land — a tab is a sub-tree of the
    /// `egui_tiles::Tree`.
    CurrentTab,

    /// Every block across every tab in the current window.
    AllTabsInWindow,

    /// Every block in every open window. Multi-window arrives
    /// post-MVP; this scope is wired through ahead of time so the
    /// `find` UI doesn't need a schema change later.
    AllWindows,

    /// Persisted-but-not-currently-open sessions, via the global
    /// search index. Post-MVP (see below).
    Global,
}

pub struct SearchQuery {
    pub text: String,
    pub mode: SearchMode,   // Literal / CaseInsensitive / Regex / Fuzzy
    pub scope: SearchScope,
    /// Optional filter on top of the scope: `CommandOnly` matches
    /// only the prompt-and-command portion of each block;
    /// `OutputOnly` matches only its I/O.
    pub filter: SearchFilter,
}

pub enum SearchFilter {
    Both,           // default
    CommandOnly,    // exclude output
    OutputOnly,     // exclude the command line
}

pub struct SearchResult {
    pub pane_id: PaneId,
    pub command_run_id: CommandRunId,
    pub scrollback_position: ScrollbackPosition,
    pub matched_line: String,
    pub match_span: Range<usize>,
}
```

### Default + scope-switcher UX

- `Cmd/Ctrl+F` opens the overlay with `scope = CurrentTab`. This is
  what users expect from "find in this terminal" — every block they
  can see by switching splits / tabs-within-the-current-tab is in
  range, but unrelated tabs aren't.
- The overlay has a scope chip (e.g. `[Tab]`) the user can click to
  cycle through `SelectedBlocks` → `LastBlock` → `CurrentTab` →
  `AllTabsInWindow` → `AllWindows`. `Global` is hidden until the
  global index ships.
- Modes (`Literal` / `CaseInsensitive` / `Regex` / `Fuzzy`) and
  filters (`Both` / `CommandOnly` / `OutputOnly`) are independent
  toggles in the overlay.

Search engine: literal + case-insensitive in v1 (Phase 8). Regex via
`regex`; fuzzy via `nucleo`. Tantivy for global persisted output
search is post-MVP.

### v1 scopes (shipping)

- `SelectedBlocks` / `LastBlock` / `CurrentTab` — **Phase 8 deliverable.**
- `CommandHistoryOnly` is *not* a scope of `find`; that's the
  `Ctrl+R` popup, which is its own UI and ships in Phase 6.

### Post-MVP scopes

- `AllTabsInWindow` — straightforward extension; left out of v1 only
  because the UX of cross-tab match navigation needs a design pass
  (do we switch the active tab on `⇣`? Do we open a results panel?).
- `AllWindows` — needs multi-window first.
- `Global` — across all persisted sessions, including ones that
  aren't currently open. Tantivy-indexed.

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
