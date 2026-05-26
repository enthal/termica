**← Previous:** [09 — Testing](09-testing.md) | **Next:** [11 — Keyboard shortcuts](11-keyboard-shortcuts.md) →

# 10 — Roadmap

This is the order we build Termica. Every phase ships behind a feature branch, lands as a squash-merge PR, and closes a GitHub Issue with the stated acceptance criteria.

## MVP definition

The MVP is the smallest configuration of Termica that is **defensibly its own product** rather than "a Rust terminal." It must include:

- eframe + egui app with one OS window and a `egui_tiles` workspace.
- Tabs and splits; each pane is a real PTY-backed bash or zsh session.
- `alacritty_terminal`-backed terminal correctness: vim, htop, less, fzf, ssh, tmux all work as in any other modern terminal.
- Shell integration scripts installable with `termica install-integration`.
- `PromptController` mode machine driven by OSC 133 + Termica-private markers.
- Editor at the prompt: click-to-place cursor, drag selection, multiline editing, undo/redo, Enter-submit with echo suppression, basic syntax highlighting.
- Pane status header with cwd, git branch, dirty count, last exit, duration. Async, debounced.
- Pane-local + global command history (SQLite). Up-arrow walk + Ctrl-R fuzzy popup.
- In-pane search.
- Scrollback persistence: chunk files + SQLite metadata. Transcripts restore on launch (PTYs do not).
- Dark theme, baseline keyboard shortcuts.

What the MVP does **not** need:

- Deep shell completion (zsh/bash bridges) — local completion is enough.
- Workspace-wide search across persisted sessions.
- Command palette.
- Themes / icons / animations beyond a baseline.
- REPL integrations.
- Session daemon.
- Multi-window (multi-viewport).
- Windows support.
- Hyperlinks (OSC 8), image protocols.

## Phases

Each phase is a separate PR, with its own GH Issue and acceptance criteria. Phases are roughly sequential, but adjacent phases that touch disjoint code may interleave.

### Phase 0 — Skeleton (commit zero) ✅

- ✅ Cargo workspace stub + placeholder `main`.
- ✅ `CLAUDE.md`, `SPEC.md`, `spec/*.md` (all ten).
- ✅ `README.md` with WIP banner.
- ✅ Dual MIT / Apache-2.0 licenses.
- ✅ `.gitignore`, `rust-toolchain.toml`, `rustfmt.toml`.
- ✅ `cargo build` succeeds; the binary prints a pre-implementation notice.

**Status:** complete. Spec is reviewable on `main`.

### Phase 1 — Real terminal pane ✅

Sub-PRs:

- ✅ **1A — CI workflow + pre-commit hook** ([#11](https://github.com/enthal/termica/pull/11), with the [#12](https://github.com/enthal/termica/pull/12) hotfix to keep tests-first compatible).
- ✅ **1B — eframe skeleton + first snapshot test** ([#13](https://github.com/enthal/termica/pull/13)).
- ✅ **1C — `portable-pty` plumbing** ([#14](https://github.com/enthal/termica/pull/14)).
- ✅ **1D — `alacritty_terminal` wrapper** ([#15](https://github.com/enthal/termica/pull/15)).
- ✅ **1E-a — PTY ↔ TerminalState pipeline + status header** ([#16](https://github.com/enthal/termica/pull/16)).
- ✅ **1E-b — custom egui cell renderer** ([#17](https://github.com/enthal/termica/pull/17)).
- ✅ **1E-c — keyboard input encoder + DECCKM (CSI/SS3 arrows) + env inheritance** ([#19](https://github.com/enthal/termica/pull/19), [#20](https://github.com/enthal/termica/pull/20), [#21](https://github.com/enthal/termica/pull/21)).
- ✅ **1E-d — window resize → PTY resize + visible cursor (DECTCEM)** ([#22](https://github.com/enthal/termica/pull/22)).
- ✅ **1E-e — bracketed paste wrapping** ([#23](https://github.com/enthal/termica/pull/23)).
- ✅ **1E-f — cell attributes: bold / dim / inverse / hidden / underline / strikethrough** ([#25](https://github.com/enthal/termica/pull/25)).
- ✅ **1E-g — mouse-wheel scrollback + scroll-aware renderer** ([#27](https://github.com/enthal/termica/pull/27)).
- ✅ **1E-h — OSC 7 cwd tracking via parallel vte sniffer** ([#28](https://github.com/enthal/termica/pull/28)).
- ✅ **1E-i — mouse-wheel in alt-screen → arrow keystrokes** ([#29](https://github.com/enthal/termica/pull/29)).
- ✅ **1E-j — auto-install minimal zsh OSC 7 hook (ZDOTDIR override)** ([#30](https://github.com/enthal/termica/pull/30)).
- ✅ **1E-k — mouse selection + Cmd/Ctrl+Shift+C copy to clipboard** ([#31](https://github.com/enthal/termica/pull/31)).
- ✅ **1E-l — clickable URLs + smarter word boundaries** ([#32](https://github.com/enthal/termica/pull/32)).
- ✅ **1E-m — clickable on-disk paths relative to cwd** ([#33](https://github.com/enthal/termica/pull/33)).
- ✅ **1E-n — VT golden tests** (this PR): `bash-basic`, `zsh-basic`, `vim`, `less`, `htop`, `fzf`, `ssh`, `split-reads` under [`testdata/vt/`](../testdata/vt/). The `split-reads` scenario locks in the "escape sequence parsed across read boundaries" invariant.

**Acceptance:** open the app, run `vim ~/.zshrc`, edit it, save it, quit it. The user experience matches Alacritty.

✅ Met — `vim`, `less`, `htop`, `fzf`, `ssh` all run interactively; arrow keys scroll in `less`; window resizes propagate to the shell; mouse selection + copy + clickable URLs + clickable paths all live. The terminal-pane fundamentals are complete and locked in by the VT golden suite.

### Phase 2 — Workspace

Sub-PRs:

- ✅ **2A — Tabs + drag-splits + app shortcuts + close/quit modals + macOS menubar** (this PR).
  - `egui_tiles::Tree<PaneId>` topology with the actual `PaneSession` (PTY, reader thread, grid) held in a side `HashMap` so the tree carries only value-type `PaneId`s.
  - Tab strip per `Tabs` container with `[+]` to spawn and `×` to close; drag a tab to an edge to split.
  - Per-tab keyboard focus; the focused pane's tab title renders bold; click-to-focus and tab-click-to-focus both work; egui's built-in Tab/arrow focus nav is locked out so keystrokes always reach the PTY.
  - App-level shortcuts: Cmd+T new tab, Cmd+W close tab (routes to Quit on the last tab), Cmd+Shift+] / [ next/previous tab, Cmd+Q quit. Linux/Windows use Ctrl+Shift equivalents. The encoder rejects all unrecognised modifier+key combos so a chord can never accidentally land in the PTY as a bare key.
  - Close-tab confirmation modal when the pane's PTY is in alt-screen (vim / less / htop / fzf / ssh-with-TUI); same modal whether the close was a `×` click or Cmd+W.
  - Quit-confirmation modal with a 60-second auto-quit countdown; Cancel / Esc / backdrop cancels.
  - Pane input is gated while any modal is up — keys, wheel, clicks, and focus grabs are suppressed so keystrokes intended for the modal don't leak to the PTY.
  - macOS: custom menubar via `muda`; winit's default Quit menu is disabled so its `[NSApp terminate:]` action can't bypass the quit-confirm modal. Custom About item opens a small in-app modal.
- ✅ **2B — Spawn in cwd**: a new tab inherits its parent Tabs container's active pane's OSC 7 cwd; falls back to the termica process's cwd if none is known yet.
- ✅ **2C — Split snapshot tests**: `egui_kittest` snapshots for tabs + 2-pane horizontal split and tabs + 3-pane T-split (see [`tests/split_snapshots.rs`](../tests/split_snapshots.rs)). Pane bodies are stub rects so the tests don't need real PTYs; tab titles and the focus underline go through the same production helpers (`tab_title_for`, `paint_focused_tab_underline`) so chrome regressions land in the snapshots.

**Acceptance:** multi-pane terminal workspace; layout operations don't drop pane state.

**Status:** ✅ Phase 2 complete.

> A "duplicate-here" pane op was originally listed alongside spawn-in-cwd in the pre-breakout Phase 2 description (see the original [#2](https://github.com/enthal/termica/issues/2) acceptance criteria). After 2B shipped, the gap between "Cmd+T" and a hypothetical "duplicate this pane" collapsed — both produce a fresh shell in the same cwd, and shells can't share runtime env across forks anyway. Dropped from the roadmap; reframing as a keyboard split shortcut (à la iTerm2 Cmd+D) is the only flavor that would still be a distinct gesture, but drag-to-split already works and a hotkey can land later as needed.

### Phase 3 — Managed shell integration + mode machine

**Design pivot ([#45](https://github.com/enthal/termica/pull/45)):** the Phase 3A/3B work shipped an OSC 133 + OSC 1337-Termica marker pipeline and a four-mode `PromptController`. We pivoted in [spec/03](03-shell-integration.md) to a managed-shell-integration design: Termica controls bootstrap on every shell spawn via `ZDOTDIR` (zsh) / `--rcfile` (bash) / `--no-config --init-command` (fish), uses a DCS-JSON protocol it owns end-to-end, and ignores foreign OSC 133. The pane mode machine grows two new states (`Bootstrapping`, `Degraded` — see [05](05-pane-modes.md)). The "fenced-block dotfile installer" approach is dropped entirely.

Code from 3A (`MarkerEvent`, OSC 133/1337 parser) and 3B (`PromptController`) is partially reused — the controller's state-machine shape survives; the event source switches from OSC markers to DCS-JSON lifecycle messages.

Sub-PRs (reshaped):

- ✅ **3A — Marker parser (legacy)**: OSC 133 + OSC 1337-`Termica=…` parsing in [`src/markers.rs`](../src/markers.rs); wired into [`OscSniffer`](../src/osc.rs). Replaced by 3C.
- ✅ **3B — `PromptController` (legacy)**: four-mode state machine in [`src/shell.rs`](../src/shell.rs). State-machine shape preserved; expanded to six modes in 3C.
- ✅ **3C — DCS-JSON parser + extended `PromptController`** ([#45](https://github.com/enthal/termica/pull/45)): replaced OSC 133/1337 in [`src/markers.rs`](../src/markers.rs) + [`src/osc.rs`](../src/osc.rs) with a DCS-JSON parser; added `Bootstrapping` + `Degraded` modes to [`src/shell.rs`](../src/shell.rs).
- ✅ **3D — Bootstrap scripts** ([#45](https://github.com/enthal/termica/pull/45)): zsh / bash / fish bootstrap scripts under [`integration/`](../integration/) as `include_str!` constants; vendored bash-preexec.sh.
- ✅ **3E — Managed-startup wrappers** ([#45](https://github.com/enthal/termica/pull/45)): replaced `$ZDOTDIR` mechanism with managed-startup machinery in [`src/integration.rs`](../src/integration.rs): per-spawn `tempfile::TempDir` wrappers, `ZDOTDIR` for zsh, `--rcfile` for bash, `--init-command` for fish.
- ✅ **3F — Wire `PromptController` into `PaneSession`** ([#45](https://github.com/enthal/termica/pull/45)): mode machine is the source of truth for "is this pane at a prompt?". Bootstrap suppression integrates with the renderer.
- ✅ **3G — `TERMICA_DUMP_EVENTS` env var** (this PR): per-pane spawn / lifecycle / mode-transition / pty-exit stream written to a file for debugging. See [spec/03 "Debug surface"](03-shell-integration.md#debug-surface).

The strict tests-first rule from [CLAUDE.md](../CLAUDE.md) applies to **the entire Phase 3 surface area** — every commit lands with tests that failed on the pre-change tree.

**Acceptance:** Termica spawns its own managed shells; DCS-JSON lifecycle messages flow end-to-end; mode transitions are observably correct including `Bootstrapping` → `RawTerminal` and `Bootstrapping` → `Degraded`; bootstrap suppression hides bootstrap noise from the user. Editor is **not** wired up yet (Phase 4).

**Known gaps tracked for later:**

- System-wide rc files (`/etc/zshrc`, `/etc/bashrc`) not sourced in v1.
- Login-shell emulation (sourcing `.zprofile`, `.zlogin`) not in v1.
- Nested shells (running `zsh` / `bash` / `fish` inside a Termica-managed shell) do not get integration in v1 — the nested shell is un-integrated; pane mode degrades naturally because no lifecycle messages arrive.
- Remote integration (ssh, docker exec, kubectl exec) is post-MVP.

### Phase 4 — Editor at prompt (block-model pivot)

**Design pivot ([discussion preceded the spec rewrite](https://github.com/enthal/termica/pulls)):** the original Phase 4 was sketched as "drop an editor widget into the existing single-grid pane." Empirical UX prototyping revealed that a block model — each command is its own self-contained block with header chrome + output area, stacked vertically — is the right shape and pulls Phase 7's command-block work forward. See [04 §"Visual structure: the block model"](04-prompt-editor.md#visual-structure-the-block-model) and [02 §"The block model"](02-terminal-engine.md#the-block-model-one-live-term-many-sealed-snapshots).

Implementation choice: **one live `Term` for the bottom block (`Running` or `Prompt`); sealed blocks are frozen `Vec<StyledLine>` snapshots.** Sealed blocks do not reflow on resize. Selection is pane-coordinate (`PaneCursor { block_id, line, col }`), not grid-coordinate; copy concatenates block contents and skips chrome.

Sub-PRs:

- ⏳ **4A-data — Block data model + lifecycle wiring.** `Block` enum (`Prompt` / `Running` / `Sealed`), `BlockId`, `BlockHeader`, `StyledCell` / `StyledLine`, and a `BlockStack` in `PaneSession`. `PromptController` lifecycle events drive transitions (`Preexec` → `Running`, `CommandFinished` → seal + new `Prompt`). `Sealed` blocks capture the live `Term`'s snapshot of just the lines the command produced (line-offset slice; no `Term` reset yet). Renderer is unchanged — the data model lands first, behind the existing UI, so the structural lift can ship reviewably. No user-visible change.
- ⏳ **4A-render — Walk blocks in the renderer + reset `Term` on seal.** Replace the single-grid paint with a top-to-bottom walk of `BlockStack`; sealed blocks paint their styled-line snapshot, the live tail paints the live `Term` as today. `CommandFinished` resets the live `Term` (clears + drops scrollback) so sealed-block snapshots are the canonical history. `Prompt` block paints a placeholder where the editor will go.
- ⏳ **4B — Editor widget** inside the `Prompt` block. `PromptEditor` struct + basic cursor / insert / delete / multiline editing (Shift+Enter). Esc demotes via `leave_editor_esc`. No submit yet — Enter is a placeholder.
- ⏳ **4C — Submit path + echo suppression.** Enter sends bytes; the `Prompt` block transitions to `Sealed` (or rather, its successor `Running` block is born) per [04 §"Submission semantics"](04-prompt-editor.md#submission-semantics-enter). Echo suppression buffer per [04 §"Echo handling"](04-prompt-editor.md#echo-handling) option (b). The hard part.
- ⏳ **4D — Fixed-footer `Prompt` block + multiline expand.** The `Prompt` block is glued to the viewport bottom; older blocks scroll under it; multiline grows the footer down and shrinks the scroll area. Big layout shift, but bounded.
- ⏳ **4E — Sticky-top block header.** When a block's body is visible but its header is scrolled above the viewport, paint the header pinned to the top edge of the scroll area until the body fully scrolls past.
- ⏳ **4F — Cross-block selection + copy.** `PaneSelection { anchor, head: PaneCursor }`; drag across boundaries; copy concatenates block text, skips chrome.
- ⏳ **4G — Block header chrome.** Chips above the `Prompt` editor (cwd, branch, dirty); dim header line for `Running` (live duration timer) and `Sealed` (final duration + exit). Pulls forward most of what Phase 5 (status header) was going to do.
- ⏳ **4H — Local syntax highlighting** (in-house tokenizer per [04 §"Syntax highlighting"](04-prompt-editor.md#syntax-highlighting)).
- ⏳ **4I — Local completion (Tab).** Path + history + `$PATH` per [04 §"Tab handling"](04-prompt-editor.md#tab-handling). Depends partially on Phase 6 history.
- ⏳ **4J — History walk (Up/Down) + Ctrl+R popup.** Depends on Phase 6 storage.

4A–4C is the load-bearing trio: at the end of 4C the user can type a command in the editor, press Enter, see it execute in a `Running` block, and see the result in a `Sealed` block. 4D–4G are the UX polish that makes it feel block-oriented. 4H–4J are independently-sliceable polish.

**Acceptance:** at a `bash`/`zsh` prompt with managed integration, the user types into the editor inside the `Prompt` block; Enter executes the command in a `Running` block that seals on `CommandFinished`; the duplicate echo never appears in the transcript; vim still works (alt-screen runs inside a `Running` block, sealed empty-transcript on exit).

**Phase 7 (command blocks) is largely subsumed by Phase 4G's block-header chrome work.** The collapse / expand / context-menu affordances that Phase 7 was going to add layer cleanly on top of 4A's block infrastructure; they become a small Phase-7 polish PR rather than the foundational work the original spec assumed.

### Phase 5 — Status header

- `termica-context`: cwd / git-branch / dirty-count / last-exit / last-duration providers.
- Async debounced probes; never block the UI thread.
- `icons.rs` module with `Painter`-drawn glyphs.
- Click actions: copy path, copy branch.

**Acceptance:** the header replaces most of what `PS1` carried; updates feel instant; never blocks paint.

### Phase 6 — History (local + global) + Ctrl+R

- `termica-history`: SQLite schema for `command_run` + `history_entry`; pane-local in-memory ring with disk spill.
- Up-arrow walk; Ctrl+R popup with fuzzy match (`nucleo`).
- Scope toggles: this pane / this project / global.
- Cwd-biased ranking.

**Acceptance:** Ctrl+R returns relevant historical commands across panes and previous sessions in under 50 ms for a 50k-entry history.

### Phase 7 — Command blocks

- `CommandRun` lifecycle wiring: open on submit / `command_start`; close on `command_end`.
- Transcript view renders command blocks with header chrome.
- Collapse / expand.
- Copy command / copy output / rerun.

**Acceptance:** the transcript becomes navigable as a sequence of command blocks. Failed exits are visually distinct. Click → collapse works.

### Phase 8 — In-pane search

- Cmd/Ctrl+F overlay with literal + case-insensitive + regex modes.
- Match highlights paint over the cell grid.
- ⇡/⇣ navigation; Esc dismiss.

**Acceptance:** find-in-pane works on the in-memory scrollback + sealed chunks.

### Phase 9 — Scrollback persistence + restore

- `termica-persist`: chunk file format (header + length-prefixed records + style spans); zstd on seal; `scrollback_chunk` table; layout blob storage.
- Async writer task; SQLite WAL.
- Restore on launch: layout + transcripts; PTY is `Dead` until user restarts shell.
- `Persistence::gc()` retention enforcer.
- Property tests for chunk round-trip and crash injection.

**Acceptance:** kill the app, relaunch, the workspace comes back. Transcripts up to ≤1 second pre-crash are present.

### Phase 10 — Polish (and stop)

- A small TOML config file in `~/.config/termica/`.
- Configurable theme (one light + one dark, no custom themes yet).
- Configurable keyboard bindings.
- Onboarding screen on first launch suggesting `termica install-integration`.
- Performance pass (the two perf-smoke tests from [09](09-testing.md) pass with margin).
- v1.0 release.

**Acceptance:** Termica is the user's daily-driver terminal on macOS and Linux.

## Post-MVP (probably-yes)

In rough priority order; each is its own future PR / Issue, none committed.

- **Shell completion bridge** (zsh + bash) via a private OSC request/response.
- **Workspace search** across panes and persisted sessions.
- **Command palette** (Cmd/Ctrl+P).
- **Multi-window** (egui multi-viewport).
- **Hyperlinks** (OSC 8).
- **Themes** with a `~/.config/termica/themes/*.toml` directory.
- **Configuration UI**.
- **Image protocols** (Kitty graphics, iTerm2 inline images).
- **Per-REPL helpers** for `psql` / `python` / `node` that capture structured input.
- **Session daemon** that keeps PTYs alive across app restart.
- **Windows support** (paths, fonts, paths-with-spaces, CRLF, console host APIs).

## Maybe never

- A custom shell.
- Reimplementing readline / ZLE.
- A SQL frontend for command history.
- Cross-host federation ("my Termica history follows me to another machine").

## Risks

- **Mode-machine bugs**. The whole product hinges on it. Mitigation: the strict tests-first rule on the entire `termica-shell` crate, plus the canonical five-rule tests.
- **Echo handling under unusual `stty` states**. Users with non-default tty configurations may break echo suppression. Mitigation: explicit timeout fallback; gracefully degrade to "raw echo visible" rather than corrupting.
- **`alacritty_terminal` upstream changes**. The crate is the rendering brain. Mitigation: pin a version; treat upgrades as their own PRs with VT golden suite re-runs.
- **`egui_tiles` evolves**. We rely on the `Tree<TileId>` API shape. Mitigation: thin adapter; treat egui_tiles minor-version bumps as their own PRs.
- **macOS Secure Keyboard Entry quirks**. Mitigation: defer until Phase 10; document the issue if it appears.
- **PTY behavior differences between macOS and Linux** (especially around process group / `SIGHUP` semantics on close). Mitigation: integration tests on both platforms in CI from Phase 1.
- **The "users haven't installed integration" path stays usable**. Mitigation: every UI element degrades cleanly when the marker stream is silent.

## Definition of done for the spec phase (this PR)

- All ten `spec/*.md` documents reviewable on GitHub.
- `SPEC.md` indexes them.
- `CLAUDE.md` cross-references them.
- `README.md` mentions the spec as the source of truth.
- `cargo build` succeeds; binary prints the pre-implementation notice.
- GitHub Issues exist for Phases 1–10 with the acceptance criteria above.
- This document, [10](10-roadmap.md), is updated when each Issue closes.

---

**← Previous:** [09 — Testing](09-testing.md) | **Next:** [11 — Keyboard shortcuts](11-keyboard-shortcuts.md) →
