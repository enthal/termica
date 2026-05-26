**← Previous:** [09 — Testing](09-testing.md) | **Next:** [SPEC index](../SPEC.md) ↑

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

### Phase 3 — Markers + mode machine

- `termica-markers`: OSC 133 + OSC 1337 `Termica=…` parser sitting on top of `alacritty_terminal`'s OSC events.
- `termica-shell`: `PromptController` state machine with all four modes and every transition tested.
- `termica-integration`: bash + zsh scripts as `include_str!`; `termica install-integration` CLI subcommand with idempotent fenced-block rcfile mutation.
- Strict tests for the five safety rules ([05](05-pane-modes.md)).
- `--dump-events` flag writes marker stream + mode transitions to a file.

**Acceptance:** marker events flow end-to-end; mode transitions are observably correct; installer is idempotent. Editor is **not** wired up yet.

### Phase 4 — Editor at prompt

- `termica-editor`: `PromptEditor` widget with all operations from [04](04-prompt-editor.md).
- Submit path with eager demotion + echo suppression.
- Multiline + Shift+Enter.
- Local syntax highlighting (in-house tokenizer).
- Local completion (paths + history + `$PATH`) on Tab.
- Up/Down: pane-local history walk.
- Esc / Ctrl+C / Ctrl+D edge cases.

**Acceptance:** at a `bash`/`zsh` prompt with integration installed, the editor is active; commands run; the duplicate echo never appears; vim still works exactly as in Phase 1.

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

**← Previous:** [09 — Testing](09-testing.md) | **Next:** [SPEC index](../SPEC.md) ↑
