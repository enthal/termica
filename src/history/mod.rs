//! Termica's command-history store.
//!
//! Phase 4J. SQLite-backed durable history (`runs` table) that
//! both Termica-captured commands AND replayed shell history-file
//! entries (`~/.zsh_history`, `~/.bash_history`, fish) write into.
//! The UI layers — `↑`/`↓` recall in the editor and the `^R`
//! reverse-search overlay — query this single source.
//!
//! Per [spec/08 §"SQLite schema"](../spec/08-persistence.md#sqlite-schema)
//! the full v1 persistence schema also covers workspaces, windows,
//! tabs, panes, sessions, and scrollback chunks. **This phase
//! implements only the history slice (`runs`).** The other tables
//! land alongside the features that need them; the `runs.pane_id`
//! column is recorded as a plain `INTEGER` for now and will FK to
//! `pane(id)` once that table exists.
//!
//! ## Design choices
//!
//! - **One row per invocation, dedup at query time.** Keeps every
//!   command run individually addressable (for analytics, per-cwd
//!   filtering, exit-code search) while letting the UI's "unique
//!   command list" reduce via `GROUP BY text`.
//! - **`(pane_id, app_run_id)` for "pane scope".** Pane IDs are
//!   minted by an in-memory counter and reset across Termica runs;
//!   filtering by `pane_id` alone would surface a closed pane's
//!   history under a new pane that reuses the same numeric id.
//!   `app_run_id` is a UUID generated once per Termica process, so
//!   `WHERE pane_id = ? AND app_run_id = ?` means exactly "this
//!   pane in this session." When workspace restore eventually
//!   lands, restored panes can drop the `app_run_id` filter to
//!   recover their pre-restart history.
//! - **`source` column.** `'termica'` for entries Termica captured
//!   from its own editor's submit path; `'zsh'` / `'bash'` /
//!   `'fish'` for entries replayed from a shell's history file.
//!   Lets the UI mark "from history file" rows differently if it
//!   wants, and supports future selective purging.

#![forbid(unsafe_code)]

pub mod db;
pub mod replay;
pub mod shell_files;

pub use db::{Entry, HistoryStore, Scope};
pub use replay::{ReplayPaths, ReplayStats, replay_into};
pub use shell_files::{ParsedEntry, parse_bash_history, parse_fish_history, parse_zsh_history};
