//! Persistence (Phase 9) — scrollback chunk files + the SQLite index
//! that points at them. See [spec/08](../../spec/08-persistence.md).
//!
//! This module is built bottom-up, one reviewable slice at a time:
//!
//! - [`chunk`] (9A) — the on-disk chunk *format*: encode/decode of a
//!   pane's frozen transcript as **logical lines** (width-independent)
//!   plus run-length style runs, with optional zstd compression, and
//!   the grid-rows → logical-lines un-wrap that feeds it. Pure bytes:
//!   no SQLite, no filesystem, no UI.
//!
//! Later slices (the writer thread, the `scrollback_chunk` index, the
//! residency window, restore + session ownership) layer on top.

#![forbid(unsafe_code)]

pub mod chunk;
pub mod store;
pub mod writer;
