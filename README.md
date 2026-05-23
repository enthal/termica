# Termica

> **🚧 Status: WIP — pre-implementation.** This repository currently contains only the design spec, license, toolchain pin, and a placeholder `main` so `cargo build` succeeds. No application code exists yet. The first feature PR will land the Phase 1 terminal pane described in [spec/02-terminal-engine.md](spec/02-terminal-engine.md). Watch this banner — it will track the actual phase as work lands.

Termica is a native terminal workspace built with Rust and [egui](https://github.com/emilk/egui). It combines a real terminal emulator with an editor-driven shell experience, persistent command history, searchable transcripts, structured command execution, and modern pane-based workflows.

![Rust](https://img.shields.io/badge/Rust-2024_edition-orange) [![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](LICENSE-MIT)

## North star

Three properties drive every design choice:

1. **Terminal correctness first.** vim, htop, less, fzf, ssh, tmux behave exactly like they do in any modern terminal. The editor experience is a layer on top of a boring, correct terminal — never the other way around.
2. **Mode safety is the product.** The pane-mode state machine ([spec/05-pane-modes.md](spec/05-pane-modes.md)) is what makes editor-driven prompt editing safe to ship. Default to raw terminal whenever there's any ambiguity. A wrong-mode dropped keystroke is a corruption-class bug.
3. **No silent data loss.** What you typed, what the shell ran, the exit status, the transcript — all persist across crashes.

## What it is

- A real PTY-backed terminal emulator with [alacritty_terminal](https://crates.io/crates/alacritty_terminal) as the state engine, painted into [egui](https://github.com/emilk/egui) with a custom cell-grid renderer.
- A workspace of tabs and splits powered by [egui_tiles](https://github.com/rerun-io/egui_tiles), each pane a real PTY session.
- An **editor-driven prompt**: at a known shell prompt (detected via OSC 133 + Termica-private markers), the shell line editor is replaced by a native egui editor with click-to-place cursor, drag selection, multiline editing, undo/redo, history search, and syntax highlighting. Enter sends the final command to the PTY.
- A **structured command lifecycle**: every executed command becomes a record with cwd, exit status, duration, and an output range — collapsible, copyable, searchable, rerunnable.
- Pane-local and global **command history** backed by SQLite, with fuzzy search across scopes.
- Persistent transcripts: large scrollbacks spill to disk; sessions restore on restart (as transcripts — live PTYs do not survive a restart in v1).
- Bash and zsh **shell integration** installed by one command.

## What it isn't (yet)

- A shell. Termica embeds your existing `bash` or `zsh`.
- A terminal multiplexer. tmux still works inside Termica; we don't replace it.
- A Windows terminal. macOS and Linux first; Windows is later or never.
- A REPL-aware editor. `psql` / `python` / `node` get raw-terminal behavior (good enough). Deep REPL integrations are out of scope for v1.
- A shell completion reimplementation. v1 ships local completion (paths, history, PATH executables); deep zsh/bash completion bridges are post-MVP.

See [spec/00-overview.md](spec/00-overview.md) for the full in-scope / out-of-scope breakdown.

## Architecture in one paragraph

Three layers, sharply separated. The **terminal layer** is `alacritty_terminal` driving grid state from PTY bytes, painted to egui through a custom cell renderer. The **structured-shell layer** consumes a stream of OSC markers emitted by an installable bash/zsh integration script and drives a pane-mode state machine (`RawTerminal` / `ShellPromptEditor` / `AlternateScreen` / `Dead`); a default of `RawTerminal` is the safety invariant. The **workspace layer** owns tabs, splits, the editor, the status header, history, search, and persistence on top. Full diagrams and component tables in [spec/01-architecture.md](spec/01-architecture.md).

## Reading order

The full spec index is in [SPEC.md](SPEC.md). If you read three documents:

1. [01 — Architecture](spec/01-architecture.md) — the spine.
2. [05 — Pane modes](spec/05-pane-modes.md) — the load-bearing safety invariant.
3. [09 — Testing](spec/09-testing.md) — how we know it works.

## Getting started

### Toolchain

Termica builds on stable Rust **1.95** or newer, pinned in [rust-toolchain.toml](rust-toolchain.toml). If you don't already have Rust, install it via [rustup](https://rustup.rs/):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add rustfmt clippy
```

### Clone and build

```sh
git clone https://github.com/enthal/termica
cd termica
cargo build
```

Today this builds the placeholder binary, which prints a pre-implementation notice and exits. Real entry points arrive with Phase 1.

### Run the tests

```sh
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

### Install git hooks (recommended, once available)

A pre-commit hook mirrors the CI bar (fmt + clippy + test) so a red CI on push is a surprise, not a routine. The installer is `scripts/install-git-hooks.sh` (arriving in the first feature PR alongside CI).

## Working in this repo

See [CLAUDE.md](CLAUDE.md) for the working agreement — most importantly: **the spec is the source of truth**, the testing rule is hybrid (strict for the engine, mode machine, marker parser, prompt path, and persistence; pragmatic for tile and theme chrome), all changes after commit zero land via a feature branch → PR → squash merge, and every normative spec change ships with the code change in the same commit.

## License

Dual-licensed under either of:

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option. This matches the standard Rust ecosystem licensing.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Termica by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
