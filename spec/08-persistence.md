**← Previous:** [07 — History & search](07-history-and-search.md) | **Next:** [09 — Testing](09-testing.md) →

# 08 — Persistence

Termica must survive a crash and a restart without losing what the user typed, what the shell ran, or the transcript. Persistence is structural; it is not best-effort.

## Two stores

| Store | Purpose | Format |
|---|---|---|
| **SQLite** | Sessions, panes, layout, command runs, history entries, scrollback chunk index | Single file: `<data-dir>/termica.sqlite` |
| **Chunk files** | Transcript content (logical lines + style); sealed, append-only | `<data-dir>/scrollback/<session>/<pane>/<NNNNNNNN>.chunk[.zst]` |

Splitting them is deliberate: SQLite is bad at holding large blobs; the filesystem is bad at indexing. We use each for what it's good at. The bulky transcript payload (potentially many MiB per chunk) lives on the filesystem; SQLite holds only a small index row per chunk (path, logical-line range, byte size, compressed flag) — never the chunk bytes themselves.

**One database, named `termica.sqlite`.** Everything durable that is *not* a chunk file — layout, sessions, command runs, the chunk index — lives in this single DB. Keeping `runs` in the same file as `scrollback_chunk` means "show me the output block for this historical command" is a plain `JOIN`, not a cross-database `ATTACH`; any future table can cheap-join to any existing one. The pre-1.0 code shipped this DB under the name `history.sqlite` (it then held only `runs`); the rename to `termica.sqlite` is a **one-time manual `mv`** on the developer's own machines, not an in-app migration — so the app carries no crash-safe-rename code, it simply opens `termica.sqlite`. A missing `termica.sqlite` is created fresh; the `runs` table self-re-seeds from the user's shell-history files on the next start ([§"Shell-history-file replay"](#shell-history-file-replay)), so a forgotten rename loses only the handful of `source = 'termica'` rows typed directly into Termica, never the shell's own history.

```
<data-dir>/
├── termica.sqlite
├── termica.sqlite-wal              (SQLite WAL mode)
├── termica.sqlite-shm
└── scrollback/
    └── session-<sid>/
        └── pane-<pid>/
            ├── 00000001.chunk
            ├── 00000002.chunk.zst
            └── 00000003.chunk
```

`<data-dir>` is `$XDG_DATA_HOME/termica/` on Linux and `~/Library/Application Support/termica/` on macOS — whatever `directories::ProjectDirs::from("", "", "termica").data_dir()` resolves to (the basename is lowercase `termica`, matching the crate's output, not the display-cased product name).

## SQLite schema

Versioned via `PRAGMA user_version`. Migrations are forward-only and live in a `termica-persist::migrations` module; each migration ships with a unit test ([09](09-testing.md)).

### Schema v1

```sql
-- workspaces, windows, tabs, panes: enough to restore layout
CREATE TABLE workspace (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  INTEGER NOT NULL          -- unix epoch ms
);

CREATE TABLE window (
    id           INTEGER PRIMARY KEY,
    workspace_id INTEGER NOT NULL REFERENCES workspace(id),
    title        TEXT,
    layout_blob  BLOB NOT NULL             -- bincode-serialized egui_tiles::Tree<PaneId>
);

CREATE TABLE tab (
    id         INTEGER PRIMARY KEY,
    window_id  INTEGER NOT NULL REFERENCES window(id),
    name       TEXT,
    ord        INTEGER NOT NULL            -- position in tab bar
);

CREATE TABLE pane (
    id            INTEGER PRIMARY KEY,
    tab_id        INTEGER NOT NULL REFERENCES tab(id),
    title         TEXT,
    cwd           TEXT,                     -- last known cwd (URL-decoded path)
    shell_kind    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    last_open     INTEGER NOT NULL,
    -- Cmd-K (clear scrollback) watermark, in the pane's cumulative
    -- LOGICAL-line space. Restore and scrollback skip logical lines
    -- below this; the chunks themselves stay on disk (gc-aged,
    -- purge-deletable) so Cmd-K is a non-destructive "stop showing me
    -- this", not an `rm`. NULL = nothing cleared. See
    -- §"Clearing: Cmd-K vs close-pane vs purge".
    cleared_before_line INTEGER
);

-- one row per shell session (PTY spawn). Multiple sessions over the
-- lifetime of a pane (e.g. restart from Dead) accumulate here.
CREATE TABLE session (
    id          INTEGER PRIMARY KEY,
    pane_id     INTEGER NOT NULL REFERENCES pane(id),
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    exit_code   INTEGER
);

-- one row per command invocation. Backs BOTH the per-pane recent
-- history (↑ / ↓) AND the global cross-pane history (^R). Replayed
-- shell-history-file entries (~/.zsh_history, ~/.bash_history, fish)
-- also land here with `source` set to the shell name.
--
-- Earlier drafts split this into `command_run` (engine record) and
-- `history_entry` (UI quick-walk tail). The split added churn — the
-- same row needed two writes and the UI had to UNION across them —
-- without buying anything: a single `runs` table with the right
-- indexes serves both surfaces. `(pane_id, app_run_id)` is the
-- pane-scope key; the `app_run_id` UUID distinguishes a fresh pane
-- from a closed pane that happens to reuse the same numeric id.
CREATE TABLE runs (
    id            INTEGER PRIMARY KEY,
    text          TEXT NOT NULL,
    started_at    INTEGER NOT NULL,
    finished_at   INTEGER,
    exit_code     INTEGER,
    cwd           TEXT,
    app_run_id    TEXT,                      -- UUIDv4, one per Termica process lifetime
    pane_id       INTEGER,                   -- not yet a FK; becomes one when `pane` lands
    source        TEXT NOT NULL              -- 'termica' | 'zsh' | 'bash' | 'fish'
);

-- scrollback chunk index. one row per sealed chunk file.
--
-- `start_line` / `end_line` are LOGICAL-line indices into the pane's
-- cumulative transcript (see §"Logical lines, not grid rows"), NOT
-- grid rows. Logical lines are width-independent, so this index is
-- stable across resize — a chunk "covers logical lines 1000..1500"
-- no matter how the window is sized. The chunk's logical height is
-- therefore just `end_line - start_line`; that is the durable,
-- navigable size (you can lay out / seek the scrollback without
-- decoding a single chunk). The chunk's *visual* height in rows is
-- NOT stored here: it is a function of the current pane width and is
-- derived by re-wrapping on load (cached per-width in memory). The
-- width the chunk was emitted at is recorded in `emit_cols` for
-- diagnostics and is not required to reflow.
CREATE TABLE scrollback_chunk (
    id           INTEGER PRIMARY KEY,
    session_id   INTEGER NOT NULL REFERENCES session(id),
    pane_id      INTEGER NOT NULL REFERENCES pane(id),
    path         TEXT NOT NULL,
    start_line   INTEGER NOT NULL,          -- logical line, inclusive
    end_line     INTEGER NOT NULL,          -- logical line, exclusive
    emit_cols    INTEGER NOT NULL,          -- pane width when emitted (diagnostic)
    start_time   INTEGER NOT NULL,
    end_time     INTEGER NOT NULL,
    compressed   INTEGER NOT NULL,          -- 0 / 1
    byte_size    INTEGER NOT NULL
);

CREATE INDEX idx_runs_started_at        ON runs(started_at DESC);
CREATE INDEX idx_runs_pane_started      ON runs(pane_id, app_run_id, started_at DESC);
CREATE INDEX idx_runs_text              ON runs(text);
CREATE INDEX idx_scrollback_chunk_pane  ON scrollback_chunk(pane_id, start_line);
```

### Schema v2: idempotent shell-history replay

Termica replays the user's shell-history files (`~/.zsh_history`, `~/.bash_history`, fish) into `runs` on every app start (see [§"Shell-history-file replay"](#shell-history-file-replay)). Idempotency is structural — a `UNIQUE` index excludes captured rows so re-running the replay inserts each shell-file line at most once, but `source = 'termica'` rows stay distinct (running `ls` ten times produces ten rows):

```sql
CREATE UNIQUE INDEX idx_runs_replay_unique
    ON runs(source, text, started_at)
    WHERE source != 'termica';
```

The replay loop uses `INSERT OR IGNORE` against this index. Returns `Ok(Option<i64>)` from `record_replayed`: `Some(id)` for the inserted row, `None` for an existing-row no-op. The schema migration ladder lives in [`src/history/db.rs`](../src/history/db.rs); `PRAGMA user_version` tracks current version.

### Schema state vs. this document

The schema above is the *target*. The ladder is at **v3**: v1 created `runs`, v2 added the replay index (all Phase 4J needed), and v3 — the Phase 9C slice — added the `workspace` / `window` / `tab` / `pane` / `session` / `scrollback_chunk` tables and the chunk index. The ladder grows forward-only; v3 only *adds* tables and never rewrites v1/v2, so existing `runs` data is untouched. (The tables are created empty — the writer that populates them, and the restore that reads them, land in 9D–9F.)

### Shell-history-file replay

On `TermicaApp::new`, before the first pane spawns, the app:

1. Resolves the data dir via `directories::ProjectDirs::from("", "", "termica")`.
2. Opens (or creates) `<data-dir>/termica.sqlite` and migrates to the current schema.
3. For each known shell-history-file location, reads bytes and runs the matching parser, then writes every entry via `record_replayed`.

Locations + formats (in [`src/history/shell_files.rs`](../src/history/shell_files.rs)):

- **zsh**: `$HISTFILE` or `~/.zsh_history`. Extended (`: ts:elapsed;cmd`) or plain; backslash-at-EOL continuation folds multi-line commands.
- **bash**: `$HISTFILE` or `~/.bash_history`. Plain or `#<ts>` + command (`HISTTIMEFORMAT` mode). Orphan timestamps drop silently.
- **fish**: `${XDG_DATA_HOME:-~/.local/share}/fish/fish_history`. YAML-ish `- cmd:` / `  when:`; `\n` and `\\` decoded.

When the file format carries no per-entry timestamp (bash without `HISTTIMEFORMAT`), the replay loop synthesizes `started_at_ms = -(position + 1)`. Negative values sort below real positive unix-ms timestamps, are stable across replays of an append-only file, and are detected by the UI ([07](07-history-and-search.md#result-rows)) to suppress the age slot.

Replay is **never allowed to block startup**. If the data dir can't be resolved or the DB can't be opened, `TermicaApp.history` stays `None`, the app continues normally, and the prompt-editor UIs that consume it just degrade to "no history" (arrow keys are no-ops, `^R` doesn't open).

### What lives in the layout blob

`window.layout_blob` is a bincode-serialized `egui_tiles::Tree<PaneId>` plus minimal egui state (active leaf, split fractions). The format is owned by Termica; we ship a migration when the layout serialization changes.

### Why no foreign-key cascades for chunks

A scrollback chunk on disk has a longer life than its row should imply: if SQLite is corrupted or rolled back, the chunk file is still good. We do not let SQLite cascade-delete chunks. Cleanup is a separate `Persistence::gc()` pass that:

1. Reads alive chunk rows.
2. Walks `<data-dir>/scrollback/` and lists chunk files.
3. Deletes chunks that no row references **and** are older than the retention threshold.

This is intentionally conservative.

## Chunk format

A chunk is an append-only file holding a contiguous range of **logical lines**. v1 format:

```
header (16 bytes):
  magic       : "TMCK"        (4 bytes)
  version     : u32 (le)      (4 bytes)
  flags       : u32 (le)      (4 bytes)  -- bit 0 = compressed (zstd)
  reserved    : u32 (le)      (4 bytes)

body (the whole body is zstd-compressed iff flags bit 0 is set;
      the 16-byte header is never compressed):
  a sequence of length-prefixed logical-line records:
    line_len    : u32 (le)    -- byte length of line_bytes
    style_len   : u32 (le)    -- byte length of style_bytes
    line_bytes  : UTF-8       -- the logical line's chars, concatenated,
                                 trailing blank cells trimmed
    style_bytes : packed style runs (below)
```

`line_bytes` is the UTF-8 of the logical line's characters in order; the **cell count** is recovered from the style runs (one cell per run-length unit), not from the byte length, since a char may be multi-byte. `line_bytes` and the expanded style runs must describe the same number of cells, or decode fails — this equality is the round-trip invariant the strict tests assert.

Sealed chunks may be zstd-compressed; live (in-progress) chunks are never compressed. Compression happens on seal, off the UI thread.

### Style runs

We store text + style, not raw VT bytes — the right level for search and rendering; a raw-byte replay format is a debug nice-to-have, never the source of truth. Style is a run-length encoding of `(fg, bg, flags)` across the logical line's cells:

```
style_bytes:
  run_count : u32 (le)
  run_count × {
    run_len : u32 (le)        -- cells this run covers
    fg      : color           -- (below)
    bg      : color
    flags   : u16 (le)        -- alacritty cell::Flags bits, MINUS
                                 presentation-only wrap markers (WRAPLINE,
                                 the WIDE_CHAR_SPACER family) which are
                                 re-derived at wrap time, never stored
  }

color (tag byte + payload):
  0x00  Named   : u16 (le)     -- NamedColor discriminant (0..15, 256..)
  0x01  Rgb     : u8 u8 u8     -- r, g, b
  0x02  Indexed : u8           -- 256-color palette index
```

The color encoding is **Termica's own**, deliberately decoupled from `alacritty_terminal::vte::ansi::Color` so an upstream change to that enum can't silently break stored chunks; the encode/decode path converts between the two and is exhaustively round-trip-tested over every `NamedColor` variant. `WRAPLINE` and the wide-char *spacer* flags are stripped on store because wrapping is a function of the render width, recomputed on load (see below) — keeping them would bake a width into the chunk.

### Logical lines, not grid rows

A chunk stores **logical lines** — what the program emitted between hard newlines — not grid rows at a fixed width. This is the decision that makes resize and restore look right, and it reverses an earlier draft that stored fixed-width rows and declared reflow out of scope ("the same choice WezTerm and Alacritty make" — which was wrong: WezTerm, kitty, and modern Alacritty all reflow).

- A **logical line** is width-independent and may be arbitrarily long.
- A **grid row** is one visual line at a specific width; a 200-column logical line occupies 2 rows at width 100, 3 at width 67.
- Alacritty marks a soft wrap by setting `Flags::WRAPLINE` on the **last cell of the row that continues**. Our snapshot already copies every cell's flags, so the wrap structure is present at seal time and we lose nothing by un-wrapping.

**At seal** (grid rows → logical lines): consecutive rows are joined where the earlier row's last cell carries `WRAPLINE`; a row whose last cell lacks it ends a logical line. Trailing blank cells are trimmed once, on a logical line's *final* row only — a soft-wrapped row is full-width by construction and has no trailing blanks to trim. Hard newlines, `\r`-overwrites, and cursor addressing have already collapsed into the grid before we snapshot, and full-screen programs live in the alternate screen (never in scrollback), so the reconstruction is faithful.

**On render / restore** (logical lines → grid rows): each logical line is re-wrapped to the *current* pane width by a pure `wrap_logical_line(width)` function. It is wide-char aware — a `WIDE_CHAR` (CJK, etc.) may not straddle a row boundary, so a wide char that would land in the last column forces an early wrap and a spacer cell, exactly as the live grid does. Results are cached per-width in memory so steady-state rendering doesn't re-wrap each frame.

Because storage is logical, the chunk is the *same bytes* whether displayed at width 80 or 200, whether on the machine that wrote it or restored into a differently-sized window. Selection coordinates are logical too (a `PaneCursor` indexes logical line + logical column), so a selection survives a resize instead of pointing at the wrong cell.

## Consistency model

The hard question: SQLite and chunk files can diverge under a crash. We use a write-ahead pattern that makes this safe:

1. Live (unsealed) chunks live in `<data-dir>/scrollback/<session>/<pane>/current.chunk`.
2. As the `Running` block streams output, rows that have **stabilized** — scrolled out of the live viewport into scrollback, so they can no longer be overwritten by `\r` or cursor moves — append to `current.chunk`; an fsync periodicity (default 1s, configurable) bounds loss of the still-on-screen tail. Stabilized rows are stored row-shaped (with their `WRAPLINE` markers intact) in the live `current.chunk`; the un-wrap into logical lines happens when the chunk is **finalized at seal**, the one moment the line structure is complete. (Rationale: an actively-written line is not yet a stable logical line, so logical-lineifying mid-stream would be wrong; rows already in scrollback are immutable and safe to persist immediately.)
3. When `current.chunk` exceeds the seal threshold (default 8 MiB) or a session ends:
   1. Finalize: un-wrap the stabilized rows into logical lines and write them, in the chunk format above, to a temp file, then atomically rename it to `NNNNNNNN.chunk` (rename is the commit point — a crash leaves either the old `current.chunk` or the complete sealed chunk, never a torn one).
   2. Optionally compress to `NNNNNNNN.chunk.zst` (off the UI thread).
   3. Insert the `scrollback_chunk` row in SQLite (with the logical-line range and `emit_cols`).
   4. Open a new `current.chunk`.
4. On startup, for each pane: query the highest `scrollback_chunk` row, then scan for an unsealed `current.chunk` newer than that; if present, seal it offline (no row insert if duplicates would result).

If SQLite is missing a row but a chunk exists on disk: recover the row (chunk wins). If a row exists pointing at a missing chunk: log a warning, drop the row, surface a "scrollback chunk missing" marker in the transcript at that position.

The contract: **we never lose more than `fsync_period` seconds of unsealed content** to a crash. With `fsync_period = 1s`, that's bounded and explicit.

## Restore semantics

On launch:

1. Read the most recent `workspace` row whose sessions are **not owned by a live process** (see §"Concurrent processes and session ownership" — a workspace still held by another running Termica is left alone, never adopted).
2. For each window: deserialize `layout_blob`, restore tile tree.
3. For each pane: create a `Pane` in `Dead` mode, attach its transcript view to its persisted chunks. Chunks are logical lines, so they re-wrap to the *current* (restore-time) pane width — a workspace saved on a wide monitor restores cleanly onto a narrow one. The pane's `cleared_before_line` watermark is honoured: logical lines below it are not shown.
4. Show a per-pane "Restart shell" affordance. Click → spawn fresh PTY in the persisted cwd; pane transitions to `RawTerminal`.

We do **not** restore live PTYs. Process-survival across app restart is a session-daemon problem ([10](10-roadmap.md)).

If more than one orphaned workspace exists (e.g. two Termica processes both exited cleanly), restore the most-recently-active by default; the others remain on disk, searchable and gc-aged, reachable via a "reopen previous session" affordance. (Which-to-restore is a product policy, not a correctness rule.)

## Bounding growth

Nothing may grow without a bound — on disk *or* in memory. Each axis has an explicit ceiling and an enforcer; "it's append-only" is not an excuse for unbounded.

| Axis | Bound (default) | Enforced by |
|---|---|---|
| Chunk bytes per session (disk) | 200 MB | `gc()` deletes oldest chunks of an over-cap session |
| Total scrollback (disk, all sessions) | 2 GB **or** 30-day age, whichever first | `gc()` |
| `runs` rows (global history) | 50,000 entries | `gc()` trims oldest |
| `session` / `pane` rows (metadata) | pruned when their chunks are gone **and** older than retention | `gc()` |
| Resident sealed lines per pane (**memory**) | last 50,000 logical lines | residency window (below) |

All caps are configurable. `gc()` runs on startup and on demand (and may be scheduled while idle). It is the conservative pass from §"Why no foreign-key cascades": it never deletes a chunk an alive row still references.

### Memory residency

This is the bound that did **not** exist before persistence: today a pane's `BlockStack` holds every sealed block's snapshot in RAM for the life of the pane, so a long-lived pane grows without limit. Persistence is what lets us fix it — once a sealed block's bytes are durably on disk, the block can be dropped from memory and **re-paged from its chunk on demand** when the user scrolls back to it (the same decode path restore uses).

The load-bearing invariant: **a sealed block may be evicted from memory only by the operation that has confirmed its chunk is on disk.** These are not two steps a caller sequences — eviction *returns* the block only after the durable write, so an un-persisted-but-evicted block is unrepresentable. Per-pane resident memory is therefore flat regardless of session length: the last N logical lines stay hot; older ones live on disk and page in when scrolled to.

### Clearing: Cmd-K vs close-pane vs purge

Three distinct operations; conflating them is where data loss or leaks hide.

- **Cmd-K (clear scrollback)** is a *non-destructive visual* clear. It drops the pane's resident sealed blocks and resets the live grid, and it advances the pane's `cleared_before_line` watermark — but it does **not** delete chunk files or rows. Cleared content stays searchable and gc-recoverable until it ages out; "no silent data loss" forbids Cmd-K from silently `rm`-ing durably-written history. (True deletion is `purge`.)
- **Close-pane** ends the session: seal `current.chunk`, stamp `session.ended_at`, release the session's ownership lock, free all in-RAM resources. Chunks stay on disk (searchable, gc-aged). A closed pane is *not* restored on next launch — closing is intentional dismissal — but its commands remain in `runs` and its output remains searchable until retention. Close is **not** purge.
- **`termica purge`** is the only destructive path: `--pane <id>` deletes that pane's chunks + rows; `--all` removes the entire `<data-dir>`.

**Teardown — how we know we freed everything.** Not by a cleanup checklist at the call site (forget-one-step is the classic bug). By **ownership**: every resource a pane must release — the PTY child, the reader-thread `JoinHandle`, the git/gh probe request `Sender`s, the open chunk writer, the session lock — is a *field of `PaneSession`*, released in `Drop`. RAII *is* the checklist, enforced by the compiler; if a resource isn't owned by the struct, that's the bug. Because close must **seal-then-flush-then-drop**, the only way to remove a pane is an operation that flushes the writer first, so a pane cannot be dropped with an unflushed `current.chunk`. A leak test (spawn N panes, close them, assert thread + fd counts return to baseline) and the cross-OS "no zombie child on close" integration test prove it empirically.

### Privacy

- **Secrets**: shell environment / tokens / arg lists are not redacted. If you `aws sts get-session-token`, the output goes in scrollback like any other shell. We do not parse output to redact secrets in v1; the user is responsible for their shell history hygiene.

## Concurrent processes and session ownership

Two Termica processes may share one `<data-dir>`. SQLite WAL keeps the *database* safe (concurrent readers, one writer via `busy_timeout`), but logical ownership needs more. Split it into two independent guarantees:

**(a) No collision while both run.** A session directory is `session-<id>`, where `id` is the **SQLite-allocated `session.id`** — handed out under the WAL writer lock, so it is globally unique across every process sharing the DB. Each process therefore writes only its own `session-<id>/…` directories and two live processes never touch the same chunk files. (Separately, the per-process `app_run_id` UUID disambiguates `runs` rows when a closed pane's numeric id is reused.) Combined with WAL, there is no corruption.

**(b) No second process may continue another's *live* work.** Each live session holds an **OS advisory lock on its session directory** for the session's lifetime (via a safe wrapper — no `unsafe`). Restore-on-launch adopts a session only if it can **acquire** that lock:

- A still-running process holds the lock → a newcomer's `try_lock` fails → it skips that session. A live session cannot be stolen. *This is the guarantee.*
- The kernel **releases the lock automatically on process death** — clean exit or crash. So a crashed process leaves its sessions unlocked → the next launch acquires them → restores them. **The same primitive that prevents stealing a live session is the crash detector for restore** — no heartbeat interval to tune, no staleness threshold to get wrong.

This makes "is this session continuable?" answerable by *trying to take its lock*, not by a flag someone must remember to clear — wrong states are unrepresentable. Caveat: advisory locks are local-filesystem only (the data dir is local; NFS data dirs are out of scope) and the macOS/Linux `flock` behaviours go in the cross-OS integration suite.

## Async write path

The UI thread never blocks on disk:

```
PTY read task ──► transcript line ──► in-memory ring buffer ──► seal threshold
                                                                  │
                                                                  ▼
                                                          background writer
                                                          (per-pane thread)
                                                                  │
                                                                  ▼
                                                          chunk file + SQLite row
```

The writer is a background **thread** per the same pattern as the git/gh probes ([spec/00 §"Do not block the UI on probes"](00-overview.md)) — Termica is thread-based, not `tokio`-based, and persistence introduces no async runtime. The SQLite connection runs in WAL mode with a single writer; readers are concurrent. Writes batch into transactions every `write_batch_period` (default 250 ms).

## Testing

- **Unit (strict)**: chunk encode → decode round-trip preserves every char, color, and flag; the cell-count equality (`line_bytes` chars == expanded style runs) holds.
- **Unit (strict)**: color encoding round-trips every `NamedColor` variant plus `Rgb` and `Indexed`, so an upstream enum change is caught here, not in stored data.
- **Unit (strict)**: un-wrap then re-wrap is the identity on rows — `wrap_logical_line(unwrap(rows), width)` equals the original rows at that width; covered for soft wraps, hard newlines, and a wide char forced to wrap at the right margin (spacer inserted).
- **Property (strict)**: random logical-line sequences chunked at random boundaries, sealed, and re-loaded equal the original; and re-wrapping a chunk at a sequence of widths never drops or duplicates a cell.
- **Unit (strict)**: migration vN → vN+1 applied to a frozen v_N fixture produces a known v_{N+1} state.
- **Crash injection (strict)**: simulate a panic between "wrote chunk file" and "inserted row"; `Persistence::recover()` produces a consistent state with one warning.
- **Unit (strict)**: a sealed block is evicted from memory only after its chunk write is confirmed (the residency invariant); a scroll-back past the window re-pages it byte-identical.
- **Unit**: `gc()` deletes orphan chunks but never an alive one; enforces each growth cap in the table above.
- **Integration**: a full submit → output → ended cycle persists exactly one `runs` row and the expected `scrollback_chunk` row(s) with the correct logical-line range.
- **Integration (cross-OS)**: a second process cannot adopt a session the first still holds the lock on; after the first exits (clean *or* killed), the second adopts it. No zombie PTY child after close-pane.

---

**← Previous:** [07 — History & search](07-history-and-search.md) | **Next:** [09 — Testing](09-testing.md) →
