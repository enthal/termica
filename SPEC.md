# Termica — Design Specification

A native terminal workspace built with Rust and [egui](https://github.com/emilk/egui). A real PTY-backed terminal emulator at the bottom; a modern editor-driven prompt experience on top; a strict pane-mode state machine in the middle that decides which of those two layers gets the user's next keystroke.

## North star

> **A terminal that finally learned how a computer mouse works — without compromising on being a real terminal.**

Three properties drive every design choice:

1. **Terminal correctness first.** vim, htop, less, fzf, ssh, tmux behave exactly like they do in any modern terminal. The editor experience is a layer on top of a boring, correct terminal — never the other way around.
2. **Mode safety is the product.** The pane-mode state machine in [05](spec/05-pane-modes.md) is the load-bearing safety invariant. Default to `RawTerminal` whenever a transition would otherwise be ambiguous. Promotion to `ShellPromptEditor` is gated on a confirmed marker; demotion is eager.
3. **No silent data loss.** What the user typed, what the shell ran, the exit status, the transcript — all persist across crashes.

## Document map

| # | Document | What it covers |
|---|---|---|
| 00 | [Overview](spec/00-overview.md) | Goals, non-goals, glossary, conventions, MVP scope |
| 01 | [Architecture](spec/01-architecture.md) | Three-layer split, components, crate layout, data flow |
| 02 | [Terminal engine](spec/02-terminal-engine.md) | PTY, `alacritty_terminal`, the egui cell renderer, input encoding |
| 03 | [Shell integration](spec/03-shell-integration.md) | OSC 133 + Termica markers, bash & zsh scripts, the installer |
| 04 | [Prompt editor](spec/04-prompt-editor.md) | Editor model, multiline, Enter semantics, echo handling, Tab |
| 05 | [Pane modes](spec/05-pane-modes.md) | The state machine — transitions, invariants, the five safety rules |
| 06 | [Workspace & tiles](spec/06-workspace-and-tiles.md) | Windows, tabs, panes via `egui_tiles`, status header |
| 07 | [History & search](spec/07-history-and-search.md) | Pane-local + global history, search scopes, command blocks |
| 08 | [Persistence](spec/08-persistence.md) | SQLite metadata, chunked scrollback, restore semantics |
| 09 | [Testing](spec/09-testing.md) | Unit / VT golden / integration / `egui_kittest` / perf, the hybrid rule |
| 10 | [Roadmap](spec/10-roadmap.md) | MVP definition, phases 0–10, post-MVP, risks |
| 11 | [Keyboard shortcuts](spec/11-keyboard-shortcuts.md) | Single source of truth for every app-level chord |

## Tech stack at a glance

| Layer | Choice | Rationale |
|---|---|---|
| UI framework | `eframe` / `egui` | Native Rust GUI; immediate-mode keeps state explicit |
| Workspace layout | `egui_tiles` | Tabs, splits, drag/drop — battle-tested by Rerun |
| Terminal state | `alacritty_terminal` | Grid, cursor, escape interpretation, alternate screen — decades of correctness |
| Terminal rendering | Custom egui cell renderer | Direct `Painter` access for performance; styled-run batching |
| PTY | `portable-pty` (provisional) | Cross-platform PTY spawn / resize / read / write |
| Persistence (metadata) | SQLite via `rusqlite` (provisional) | Sessions, panes, command runs, history, chunk index |
| Persistence (scrollback) | Append-only chunk files | Compressed sealed chunks; streamed from disk for search |
| Async runtime | `tokio` (multi-thread) | PTY reads, async chip probes, search indexing |
| Snapshot tests | `egui_kittest` | Same setup that works well in `knauty` |

## Reading order

If you read in order, each document assumes the previous ones. If you only read three:

1. [01 — Architecture](spec/01-architecture.md) — the spine.
2. [05 — Pane modes](spec/05-pane-modes.md) — the load-bearing safety invariant.
3. [09 — Testing](spec/09-testing.md) — how we know it works.

## How the spec is used

- The spec is **the source of truth**. Code that disagrees with the spec is a bug in the code or the spec; raise it.
- Any normative change (trait shape, OSC marker payload, mode-machine rule, persistence schema, wire format, public CLI surface) MUST update the spec in the same commit as the code.
- "MUST / SHOULD / MAY" follow RFC 2119 meanings throughout.
- Code blocks are illustrative Rust unless otherwise noted; trait shapes are normative, bodies are not.
- Cross-references use relative links so the spec is greppable and navigable on GitHub.
