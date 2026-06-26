**← Previous:** [06 — Workspace & tiles](06-workspace-and-tiles.md) | **Next:** [08 — Persistence](08-persistence.md) →

# 07 — History & search

Command history and transcript search are core to what Termica claims to be ("as navigable as an IDE"). They are not bolt-ons. They are structural.

## One store, two scopes

```
┌──────────────────────────────────────────────────────────────────┐
│   runs (SQLite)                                                  │
│   - every Termica submit                                         │
│   - every replayed shell-history-file entry                      │
│     (~/.zsh_history, ~/.bash_history, fish)                      │
└────────────────────────────┬─────────────────────────────────────┘
                             │
                ┌────────────┴────────────┐
                │                         │
       Pane scope                   Global scope
       WHERE pane_id = ?            (no filter)
        AND  app_run_id = ?
                │                         │
        ↑ / ↓ recall              ^R popup default
```

A single `runs` table backs both surfaces. The query scope — pane vs global — is just a `WHERE` clause; the schema, ranking, and UI shape are otherwise identical. See [08](08-persistence.md) for the schema.

### Hybrid: shell-history files feed the same table

Termica replays each shell's own history file (`~/.zsh_history`, `~/.bash_history`, fish) into `runs` on app start with `source = '<shell>'` and no `pane_id` / `app_run_id`. The replay is idempotent (text + started_at + source is the dedup key); the user gets their pre-Termica history immediately and earned entries from every other terminal session.

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
    /// Only this pane in this Termica process.
    /// Matches `WHERE pane_id = ? AND app_run_id = ?`. Default
    /// for `↑` / `↓` recall — it's what a "pane history" intuition
    /// should give the user.
    Pane { pane_id: PaneId, app_run_id: AppRunId },
    /// Every row. Default for `^R`. cwd-proximity boosts the
    /// ranker but does not filter.
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

## Pane-scope recall (↑ / ↓)

Queries `runs WHERE pane_id = <this> AND app_run_id = <this process>`, ordered by `started_at DESC`. UI:

- **Up arrow** in the editor: walks backwards through pane-scope history. Down arrow walks forward.
- The popup is closed; this is the lightest-weight history surface.
- Multi-line history entries are reinserted exactly, including their newlines.

A walk does not record into history; only `submit()` does.

The `app_run_id` filter is what makes pane scope meaningful across restarts: pane numeric ids are minted by an in-memory counter and reuse freely, so without the UUID a fresh pane would inherit a closed pane's typing. When workspace restore eventually lands, restored panes can drop the `app_run_id` filter to recover their pre-restart history.

## Global history (^R)

SQLite-backed, identical schema (see [08](08-persistence.md)). Every Termica submit writes one row at `started_at`; the row is updated at `finished_at`. Replayed shell-history-file entries land alongside with their own `source`.

### Open + close

- **Ctrl+R** opens the popup. If the editor has text, that text **prefills the search field** so a partially-typed command pre-narrows results (same UX as zsh's `history-search-backward`).
- The popup is modal: while open, every key + click event routes to the popup; the editor and PTY stay frozen.
- **Enter** substitutes the selected entry into the editor buffer (caret at end) and closes the popup. **Esc** closes without changing the editor. **Clicking** a row submits it.
- On Submit OR Cancel the focus is explicitly re-claimed by the pane that owned the popup. Without this, in split-screen layouts focus migrates to the first tile's active tab.

### Scope

- Default scope: `Global`. Toggle via **Tab** to `Pane` (the same `(pane_id, app_run_id)` slice the arrow keys walk). The popup's `lock_focus(true)` on its `TextEdit` lets Tab reach the popup instead of triggering egui focus navigation.
- Cwd proximity is a soft *boost* in the ranker, not a filter, so `^R` from a different directory still surfaces matches.

### Query semantics

The query is **whitespace-split** before matching. Each whitespace-separated word is its own substring matcher; results must contain **every** word (AND semantics), in any order. Ranking, in priority order:

1. **cwd_match** (the row's recorded cwd equals the pane's current cwd).
2. **whole_word_count** — query words that land at a word boundary (ASCII alphanumeric + `_`-aware) score above ones that only matched as a substring. More hits beats fewer.
3. **in_order** — when the query words appear in the text in the same order as in the query, that's a small additional boost.
4. **recency** — `started_at_ms` descending; final tiebreak.

So `echo that` matches `echo this that the other` (both words at word boundaries, in order) above `that thing echo more` (same words, different order) above `echo gotthat now` (`that` is part of `gotthat`).

### Match highlighting

Each row highlights **every occurrence of every query word** in the displayed command text. Highlight color is a saturated warm gold (`Color32(255, 215, 90)`) with a 1.5px underline at the same color — chosen because the egui-selection cool teal got washed out against the bright monospace command text. Case-insensitive byte-substring on the lowercase form; non-ASCII text that changes byte length on lowercase falls back to plain rendering instead of risking a non-char-boundary slice.

### Result rows

Each row is two lines: the command text (with matched runs highlighted), then a meta line that begins with a compact relative age:

- `now` (<60s), `4m`, `3h`, `1d` (= yesterday), `3d`, `2w`, `3mo`, `1y`. Hovering the age shows the long form (`just now`, `4 minutes ago`, `yesterday`, …) via `on_hover_text`.
- Replayed shell-history-file entries that had no per-entry timestamp (`started_at_ms ≤ 0`) **skip the age slot entirely** — rendering "56y ago" against epoch 0 was just noise.
- Age is followed by `cwd · exit code · source` (each segment present only when known). `source = 'termica'` is the captured-here default and is not displayed; `zsh` / `bash` / `fish` tags are shown so the user knows a row came from the file replay.

Multi-line commands (here-docs, multi-line for-loops, anything with embedded `\n`) collapse to **one visual line** with the `↲` glyph (U+21B2) in place of newlines. The query matches and highlight ranges run on the collapsed display text so the visible matches always line up.

The row list is deduped by command text (keeping the most-recent occurrence) so a `ls` run 200 times doesn't produce 200 rows.

### Visual chrome

- Panel width: 70% of the viewport, clamped to `[560, 1100]`. Result list has a `min_scrolled_height` floor of 45% of the viewport (clamped to `[280, 520]`) so the panel always has visual mass even with two matches.
- Rows separated by an explicit 6px gap (picked via `pick_history_row_separator` → `more-spacing`).
- Header layout: `Ctrl-R · scope: <scope*>` left-justified, `(Tab toggles scope · Enter submits · Esc cancels)` right-justified.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ Ctrl-R  scope: global*           (Tab toggles scope · Enter submits · Esc)   │
│ ┌────────────────────────────────────────────────────────────────────────┐   │
│ │ cargo te█                                                              │   │
│ └────────────────────────────────────────────────────────────────────────┘   │
│ ▸ cargo test --workspace                                                     │
│   2m · ~/git/enthal/termica · exit 0                                         │
│                                                                              │
│   cargo run --release                                                        │
│   45m · ~/git/enthal/termica · exit 0                                        │
│                                                                              │
│   ls                                                                         │
│         · zsh                                                                │
└──────────────────────────────────────────────────────────────────────────────┘
```

(The `ls` row's meta line begins with the cwd-bullet because the replayed `zsh` entry has no age slot.)

Selecting an entry replaces the editor buffer; the popup never sends to the PTY directly.

## Command blocks (transcript view)

The transcript view is structured as a vertical stack of **command blocks**, each block being one command + its output area + its header chrome. The block data model lives in Phase 4 / [spec/04](04-prompt-editor.md#visual-structure-the-block-model); the underlying live-`Term` / sealed-snapshot architecture lives in [spec/02](02-terminal-engine.md#the-block-model-one-live-term-many-sealed-snapshots). What this document adds is the *enrichment* of those blocks with command-run metadata and the per-block search affordances.

Each `Sealed` block ([04](04-prompt-editor.md)) carries a `BlockHeader` (cwd, git branch, dirty summary) and a `command + duration + exit` summary; these are the same fields populated into the persisted `CommandRun` record described below.

Per-block affordances:

- **Right-click context menu** (shipped). Right-clicking anywhere on a `Sealed` block — its header chips, command label, or output — opens a context menu with three always-present items, in this order:
  - **Copy block** — the command line(s) followed by the output, joined by a single newline (either half omitted when empty).
  - **Copy command** — the command line(s) only.
  - **Copy output** — the output only.

  All three copy width-independent text with each line's trailing space-padding trimmed and trailing blank lines dropped — the same clipboard rule as a block selection ([spec/04](04-prompt-editor.md)).

  When the right-click lands on one of the block's header chips, a **Copy `<chip>`** item plus a divider are *prepended* before those three, where `<chip>` names the chip under the pointer (`path`, `git branch`, `git sync`, `git changes`, `exit code`, `duration`) and the item copies that chip's displayed text. This is the per-block counterpart to the Phase 5 status-header "copy branch / copy path" click actions ([spec/10](10-roadmap.md)).

  A `Running` block — a command still executing, whose output is streaming in the live grid and is not yet frozen into a snapshot — copies a **best-effort snapshot of the live grid taken at click time** (it may be mid-stream). When the grid shows normal scrolling output, the menu is the same **Copy block / Copy command / Copy output** as a sealed block, reflecting the bytes printed so far. When the running command is in **alternate screen** (vim / htop / less / fzf — a full-screen TUI rather than a transcript), the menu instead offers **Copy command** and **Copy screen** (the visible grid) and omits **Copy block**, since there is no meaningful command-plus-output block. The prepended **Copy `<chip>`** behaves the same in both cases.
- Failed (non-zero exit) blocks get a subtle red gutter mark.
- Blocks are non-modal — scrolling moves them as a stack, but each is independently selectable / collapsible.

Deferred (Phase 7+ polish):

- Click the header to collapse / expand the output.
- Further context-menu / hover-icon actions: rerun, pin, jump-to-output.

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

The find feature is **block-oriented**: every scope below is expressed in terms of [command blocks](#what-is-a-block), and a match is reported with the block it lives in. This makes "next match" navigation step through blocks rather than raw scrollback rows, and makes scopes like `SelectedBlocks` natural.

### What is a "block"?

A **block** is one [`CommandRun`](#command-runs): the prompt + the command (whether the user is still editing it, has just submitted it, or finished it long ago) + every byte of stdout / stderr / OSC marker output that arrived between its `command_start` and `command_end` markers. Equivalently, it's what a single header chrome wraps in the transcript view ([command blocks](#command-blocks-in-the-transcript)).

A block is the unit the user thinks in — "the command I just ran", "the previous build", "the failed deploy" — and the unit `find` operates over.

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

- `Cmd/Ctrl+F` opens the overlay. The eventual default is `scope = CurrentTab` — every block the user can reach by switching splits / tabs-within-the-current-tab — but **Phase 8 shipped a focused-pane scope only** (see "v1 scopes" below); the cross-pane scope chip is deferred.
- The overlay will gain a scope chip (e.g. `[Tab]`) to cycle through `SelectedBlocks` → `LastBlock` → `CurrentTab` → `AllTabsInWindow` → `AllWindows`. `Global` is hidden until the global index ships.
- `Aa` (match case) and `.*` (regex) are independent toggles; together they cover the `Literal` / `CaseInsensitive` / `Regex` modes (regex itself honours the `Aa` toggle). The `Both` / `CommandOnly` / `OutputOnly` filter is a single chip cycling **All → Commands → Outputs**.
- **Find-query history (Phase 8, Termica addition).** While the query field has the caret, `↑` / `↓` walk previously-submitted searches; a `▾` button opens a dropdown of recent queries to click. The list is per-pane and in-memory for the session (a future PR can promote it to app-wide / SQLite-persisted alongside command history).

Search engine: literal + case-insensitive + regex all ship in v1 (Phase 8) — regex via the `regex` crate, literal / case-insensitive via an in-house char-column scanner (so highlight columns line up with the cell grid). Fuzzy via `nucleo` and Tantivy for global persisted output search remain post-MVP.

### v1 scopes (shipping)

- **`FocusedPane`** — Phase 8 shipped search over the focused pane's **sealed** blocks (command lines + frozen output snapshots). The live `Prompt` / `Running` tail is excluded (its output isn't frozen yet).
- `LastBlock` / `CurrentTab` / `SelectedBlocks` — **deferred.** `CurrentTab` needs the cross-pane match-navigation UX design pass; `SelectedBlocks` additionally needs block-object selection ([#120](https://github.com/enthal/termica/issues/120)). The `SearchScope` enum above is the forward design; the focused-pane v1 implements an implicit scope and grows the chip later without a rethink.
- `CommandHistoryOnly` is *not* a scope of `find`; that's the `Ctrl+R` popup, which is its own UI and ships in Phase 6.

### Post-MVP scopes

- `AllTabsInWindow` — straightforward extension; left out of v1 only because the UX of cross-tab match navigation needs a design pass (do we switch the active tab on `⇣`? Do we open a results panel?).
- `AllWindows` — needs multi-window first.
- `Global` — across all persisted sessions, including ones that aren't currently open. Tantivy-indexed.

## Result navigation

In-pane search opens a small overlay bar at the top of the pane:

```
┌─ find ─────────────────────────────────────────────────────────────┐
│ find [ error        ▾] 3 of 14  Aa  .*  All  Prev  Next  Done       │
└────────────────────────────────────────────────────────────────────┘
```

- Find **searches from the bottom**: a fresh query (or any toggle) homes on the match nearest the live tail. **Enter** / **Prev** step *up* (toward older output); **Shift+Enter** / **Next** step *down*. The transcript scrolls so the current match is centered and the count (`3 of 14`) updates live.
- Match highlights paint as overlays in the cell renderer: all matches in translucent amber, the current match brighter.
- `Aa` toggles match-case; `.*` toggles regex (a bad pattern shows `(bad regex)` instead of a count). The filter chip cycles **All → Commands → Outputs**.
- `↑` / `↓` walk the find-query history; `▾` opens the recent-query dropdown.
- **Esc** or **Done** dismisses.

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
