**← Previous:** [07 — History & search](07-history-and-search.md) | **Next:** [09 — Testing](09-testing.md) →

# 08 — Persistence

Termica must survive a crash and a restart without losing what the user typed, what the shell ran, or the transcript. Persistence is structural; it is not best-effort.

## Two stores

| Store | Purpose | Format |
|---|---|---|
| **SQLite** | Sessions, panes, layout, command runs, history entries, scrollback chunk index | Single file: `<data-dir>/termica.sqlite` |
| **Chunk files** | Raw transcript content; sealed, append-only | `<data-dir>/scrollback/<session>/<pane>/<NNNNNNNN>.chunk[.zst]` |

Splitting them is deliberate: SQLite is bad at holding large blobs; the filesystem is bad at indexing. We use each for what it's good at.

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

`<data-dir>` is `$XDG_DATA_HOME/termica/` on Linux and `~/Library/Application Support/Termica/` on macOS.

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
    id          INTEGER PRIMARY KEY,
    tab_id      INTEGER NOT NULL REFERENCES tab(id),
    title       TEXT,
    cwd         TEXT,                       -- last known cwd (URL-decoded path)
    shell_kind  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    last_open   INTEGER NOT NULL
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
CREATE TABLE scrollback_chunk (
    id           INTEGER PRIMARY KEY,
    session_id   INTEGER NOT NULL REFERENCES session(id),
    pane_id      INTEGER NOT NULL REFERENCES pane(id),
    path         TEXT NOT NULL,
    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,          -- exclusive
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

### Shell-history-file replay

On `TermicaApp::new`, before the first pane spawns, the app:

1. Resolves the data dir via `directories::ProjectDirs::from("", "", "termica")`.
2. Opens (or creates) `<data-dir>/history.sqlite` and migrates to the current schema.
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

A chunk is an append-only file holding a contiguous range of transcript lines. v1 format:

```
header (16 bytes):
  magic       : "TMCK"        (4 bytes)
  version     : u32 (le)      (4 bytes)
  flags       : u32 (le)      (4 bytes)  -- bit 0 = compressed (zstd)
  reserved    : u32 (le)      (4 bytes)

body:
  uncompressed length-prefixed records:
    line_len    : u32 (le)
    style_len   : u32 (le)
    line_bytes  : UTF-8
    style_bytes : packed StyleSpan run-length, format TBD in PR
```

Sealed chunks may be zstd-compressed; live (in-progress) chunks are never compressed. Compression happens on seal.

### Style spans

We store text + style spans, not raw VT bytes. Style is a run-length representation of `(fg, bg, attrs)` over a line. This is the right level for search and rendering. A "raw byte" replay format is a debug nice-to-have but not the source of truth.

### Lines, not bytes

Lines are logical (post-VT) terminal grid rows. Re-wrapping at a different width is intentionally **not** supported in v1; a chunk's lines were emitted at one PTY width and stay that way. This is the same choice WezTerm and Alacritty make.

## Consistency model

The hard question: SQLite and chunk files can diverge under a crash. We use a write-ahead pattern that makes this safe:

1. Live (unsealed) chunks live in `<data-dir>/scrollback/<session>/<pane>/current.chunk`.
2. Lines append to `current.chunk` in-process; an fsync periodicity (default 1s, configurable) bounds loss.
3. When `current.chunk` exceeds the seal threshold (default 8 MiB) or a session ends:
   1. Rename to `NNNNNNNN.chunk`.
   2. Optionally compress to `NNNNNNNN.chunk.zst` (off the UI thread).
   3. Insert the `scrollback_chunk` row in SQLite.
   4. Open a new `current.chunk`.
4. On startup, for each pane: query the highest `scrollback_chunk` row, then scan for an unsealed `current.chunk` newer than that; if present, seal it offline (no row insert if duplicates would result).

If SQLite is missing a row but a chunk exists on disk: recover the row (chunk wins). If a row exists pointing at a missing chunk: log a warning, drop the row, surface a "scrollback chunk missing" marker in the transcript at that position.

The contract: **we never lose more than `fsync_period` seconds of unsealed content** to a crash. With `fsync_period = 1s`, that's bounded and explicit.

## Restore semantics

On launch:

1. Read the most recent `workspace` row.
2. For each window: deserialize `layout_blob`, restore tile tree.
3. For each pane: create a `Pane` in `Dead` mode, attach its transcript view to its persisted chunks.
4. Show a per-pane "Restart shell" affordance. Click → spawn fresh PTY in the persisted cwd; pane transitions to `RawTerminal`.

We do **not** restore live PTYs. Process-survival across app restart is a session-daemon problem ([10](10-roadmap.md)).

## Privacy and retention

- **Secrets**: shell environment / tokens / arg lists are not redacted. If you `aws sts get-session-token`, the output goes in scrollback like any other shell. We do not parse output to redact secrets in v1; the user is responsible for their shell history hygiene.
- **Retention**: configurable per-pane scrollback cap (default 200 MB per session); configurable global history cap (default 50,000 entries). `gc()` enforces caps on startup and on demand.
- **Wipe**: `termica purge --pane <id>` deletes that pane's scrollback chunks and history; `termica purge --all` removes the entire `<data-dir>`.

## Async write path

The UI thread never blocks on disk:

```
PTY read task ──► transcript line ──► in-memory ring buffer ──► seal threshold
                                                                  │
                                                                  ▼
                                                          background writer
                                                          (tokio task)
                                                                  │
                                                                  ▼
                                                          chunk file + SQLite row
```

`SqlitePool` is configured with WAL mode and a single writer; readers are concurrent. Writes batch into transactions every `write_batch_period` (default 250 ms).

## Testing

- **Unit (strict)**: chunk encode → decode round-trip preserves every byte and style span.
- **Unit (strict)**: migration vN → vN+1 applied to a frozen v_N fixture produces a known v_{N+1} state.
- **Property (strict)**: random line sequences chunked at random boundaries, sealed, and re-loaded equal the original sequence.
- **Crash injection (strict)**: simulate a panic between "wrote chunk file" and "inserted row"; `Persistence::recover()` produces a consistent state with one warning.
- **Unit**: `gc()` deletes orphan chunks but never an alive one.
- **Integration**: a full submit → output → ended cycle persists exactly one `command_run` row with correct `output_start` / `output_end`.

---

**← Previous:** [07 — History & search](07-history-and-search.md) | **Next:** [09 — Testing](09-testing.md) →
