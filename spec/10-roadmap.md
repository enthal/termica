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

### Phase 3 — Managed shell integration + mode machine ✅

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

**Status:** ✅ complete — [#3](https://github.com/enthal/termica/issues/3) closed. 3A–3G all shipped; the managed-shell mode machine is the source of truth for prompt detection across zsh / bash / fish.

**Known gaps tracked for later:**

- System-wide rc files (`/etc/zshrc`, `/etc/bashrc`) not sourced in v1.
- Login-shell emulation (sourcing `.zprofile`, `.zlogin`) not in v1.
- Nested shells (running `zsh` / `bash` / `fish` inside a Termica-managed shell) do not get integration in v1 — the nested shell is un-integrated; pane mode degrades naturally because no lifecycle messages arrive.
- Remote integration (ssh, docker exec, kubectl exec) is post-MVP.

### Phase 4 — Editor at prompt (block-model pivot)

**Design pivot ([discussion preceded the spec rewrite](https://github.com/enthal/termica/pulls)):** the original Phase 4 was sketched as "drop an editor widget into the existing single-grid pane." Empirical UX prototyping revealed that a block model — each command is its own self-contained block with header chrome + output area, stacked vertically — is the right shape and pulls Phase 7's command-block work forward. See [04 §"Visual structure: the block model"](04-prompt-editor.md#visual-structure-the-block-model) and [02 §"The block model"](02-terminal-engine.md#the-block-model-one-live-term-many-sealed-snapshots).

Implementation choice: **one live `Term` for the bottom block (`Running` or `Prompt`); sealed blocks are frozen `Vec<StyledLine>` snapshots.** Sealed blocks do not reflow on resize. Selection is pane-coordinate (`PaneCursor { block_id, line, col }`), not grid-coordinate; copy concatenates block contents and skips chrome.

Sub-PRs:

- ✅ **4A-data — Block data model + lifecycle wiring** ([#51](https://github.com/enthal/termica/pull/51)). `Block` enum (`Prompt` / `Running` / `Sealed`), `BlockId`, `BlockHeader`, `StyledCell` / `StyledLine`, and a `BlockStack` in `PaneSession`. `PromptController` lifecycle events drive transitions (`Preexec` → `Running`, `CommandFinished` → seal + new `Prompt`). Snapshot was a line-offset slice; `Term` not yet reset. Renderer unchanged.
- ✅ **4A-render — Walk blocks in the renderer + reset `Term` on seal** ([#52](https://github.com/enthal/termica/pull/52)). Wraps the pane in a vertical `ScrollArea` (`stick_to_bottom`); paints sealed-block snapshots top-to-bottom inside the scroll, then the live `Term` at the bottom. New `render::paint_styled_lines` primitive shares `cell_colors_for` with the live grid so colour / flag handling stays identical. `CommandFinished` now calls `terminal.snapshot_lines_all()` then `terminal.reset_for_new_block()` — sealed-block snapshots become the canonical history; alacritty's internal scrollback is no longer used outside the live block. Mouse-wheel scrolls the `ScrollArea` natively in the non-alt-screen path; alt-screen still forwards as arrow keys.
- ✅ **4B — Editor widget** inside the `Prompt` block ([#53](https://github.com/enthal/termica/pull/53)). `PromptEditor` struct + basic cursor / insert / delete / multiline editing (Shift+Enter), char-boundary invariant under UTF-8. Esc demotes via `leave_editor_esc` and clears the editor. Enter is a placeholder. Renderer paints the editor below the live `Term` inside the `Prompt` block (4D will lock it to the viewport bottom). Keystrokes route to the editor when `ShellPromptEditor` mode is active **and** the tail is a `Prompt`; Cmd/Alt-modified events bypass the editor so the app-level shortcut handlers continue to fire. Up/Down/Tab are *consumed* (so they don't reach the PTY) but no-ops in 4B — history walk (4J) and completion (4I) land later.
- ✅ **4C — Submit path + echo suppression** ([#54](https://github.com/enthal/termica/pull/54)). `PaneSession::submit_editor_command` follows the spec/04 ordering: take editor text, **eager demote** to `RawTerminal`, prime echo suppression, write `<text>\r` to the PTY. New `EchoSuppressor` (`src/echo_suppress.rs`) keeps a prefix-match expected-bytes buffer; mismatches disengage immediately so we never half-suppress. `TerminalState::feed` runs incoming bytes through it before the parser — the duplicate kernel echo never reaches the grid. Layout fix: editor is overlaid on the live `Term`'s painted rect at `(term_rect.x, term_rect.y + (cursor_row + 1) * row_h)` so it visually continues the shell's prompt line (4B painted it as a flow item below the live `Term`, which pushed it off-screen). 4D's fixed-footer model replaces this overlay with a proper viewport-bottom anchor.
- ✅ **4D — Fixed-footer `Prompt` block + multiline expand** (PR follow-up). The `Prompt` block's editor + cwd chip live in a fixed footer at the pane bottom; the scroll area is height-constrained to `available_height − footer_height` so older blocks scroll above it. `footer_height` recomputes each frame from the editor's current line count (multi-line grows the footer down; shrinking back as the user deletes lines). The footer collapses to height `0` when the tail is `Running` / alt-screen / editor inactive — same code path, no special-casing for those modes. Editor overlay-on-Term-cursor-row is gone; the live `Term`'s cursor is still hidden via `hide_term_cursor` so the empty `PS1=''` row doesn't show a stray block. Live-Term rendering stays inside the scroll area so its `stick_to_bottom` continues to follow streaming command output. **Follow-up polish:** the empty live `Term` rows above the editor when tail is `Prompt` leave a visible gap; deferred to a "hide live Term when tail is Prompt" PR that needs to thread a synthetic `Response` through the hit-testing path.
- ✅ **4E — Sticky-top block header.** When a block's body is visible but its header is scrolled above the viewport, the header pins to the top edge of the scroll area until the body fully scrolls past; the next block's header pushes it off the top (iOS section-header handoff). Pure decision in `render_pane::compute_sticky_header` (unit-tested across walked scroll positions); paint via `render_pane::paint_sticky_header` (opaque strip + `paint_block_header`, clipped to the viewport; snapshot-tested).
- ✅ **4F — Sealed-block selection + copy.** Split into:
    - ✅ **4F-within-block — within-block selection + copy** (this PR). `BlockSelection { block_id, anchor, head: BlockCursor }` ([`src/block_selection.rs`](../src/block_selection.rs)); single-click drags by char, double-click drags by word (rolling-union), triple-click drags by line; Cmd+C / Ctrl+Shift+C copies the snapshot rows (trimmed of trailing space-padding). Selection is confined to one block — pointer dragged into a different block doesn't extend it.
    - ✅ **4F-cross-block — cross-block selection + copy** ([#101](https://github.com/enthal/termica/pull/101)). Drag across boundaries; copy concatenates block text, skipping chrome. Built on the pane-level `PaneSelection { anchor, head: PaneCursor }` ([`src/pane_selection.rs`](../src/pane_selection.rs)) per [04 §"Cross-block selection"](04-prompt-editor.md#cross-block-selection), with per-row highlight clipping across multiple painted blocks.
- ⏳ **4G — Block header chrome.** Split into:
    - ✅ **4G-cwd-and-exit — dim cwd header + exit annotation** (this PR). `BlockHeader.cwd` is populated from `LifecycleEvent::Precmd` / `LifecycleEvent::Cwd` and inherits through `Preexec` → `Running` → `CommandFinished` (the `Running` header is locked at start-time even if the program re-emits `Cwd` mid-execution). `render::paint_block_header` paints a dim cwd line above each block and a red `exit N` annotation for sealed blocks with non-zero exit.
    - ✅ **4G-duration — wall-clock command duration (sealed blocks).** Replaces the frame-counter placeholder with a real `Duration`: the pane stamps each lifecycle event with `wall_clock_ms()` (`BlockStack::set_event_clock_ms`), so a `Preexec` → `CommandFinished` pair seals the block with its true elapsed time (`Block::Running.started_at_ms`, clamped against clock skew). `render::format_duration` renders it as its own chip (`0.034s` / `11s` / `2m 3s` / `1h 2m`) on the header row, after the cwd / exit chips — `paint_block_header` lays the row out as a uniform left→right chip list so the git / dirty chips slot in next. Pure timing math is unit-tested with literal ms; the formatter and header are snapshot-tested.
    - ✅ **4G-live-duration — ticking timer for `Running` blocks.** The running block's header (and its sticky-pinned copy) shows elapsed time live: `BlockStack::running_elapsed_at(now_ms)` (pure, unit-tested) computes `now - started_at_ms`, surfaced as `PaneSession::running_elapsed()` reading `wall_clock_ms()`; the renderer samples it once per frame and schedules a 500 ms `request_repaint_after` while a command runs so the chip keeps counting without input.
    - ⏳ **4G-async-context — git branch + dirty chips.** Async-probe surface per spec/00 §"Do not block the UI on probes"; co-locates with Phase 5's `termica-context` work since the same probes power both surfaces. Sliced:
        - ✅ **PR 1 — pure git-status parser** ([#123](https://github.com/enthal/termica/pull/123)). [`src/git_context.rs`](../src/git_context.rs): `GitContext` / `DirtySummary` types + `parse_status_v2` (porcelain v2 → branch / ahead-behind / changed-file count) + `parse_numstat` (diff line counts). No process spawning, no UI — pure parsing, unit-tested without a repo.
        - ✅ **PR 2 — async probe + chips.** [`src/git_probe.rs`](../src/git_probe.rs): a per-pane `GitProbe` background thread runs `git status` + `git diff HEAD --numstat` for the pane's cwd off the UI thread, debounced (coalesced + cwd-dedup) and re-triggered on cwd change / command finish, cancelled on pane teardown (drop the request `Sender` → worker exits). `PaneSession` caches the latest `GitContext`; `render::paint_block_header` renders the branch / `ahead N behind N` / amber dirty (`N files +A -R`) chips after the cwd chip. Label strings are pure helpers on `GitContext` (unit-tested); chips are snapshot-tested.
        - ✅ **PR 3 — capture git at run-time.** `BlockHeader` gains `git: Option<GitContext>`; `BlockStack::set_current_git` + `start_running` freeze the pane's current git into the block at `Preexec`, so running / sealed blocks show the branch / dirtiness the command **actually ran under** (frozen as history, like cwd / duration) while the live `Prompt` header still reads current git. Corrects PR 2's interim "live-only, never on sealed" rendering. Capture is unit-tested (strict-layer block lifecycle); sealed-with-git is snapshot-tested.
- ✅ **4H — Local syntax highlighting** (in-house tokenizer per [04 §"Syntax highlighting"](04-prompt-editor.md#syntax-highlighting)). New [`src/shell_syntax.rs`](../src/shell_syntax.rs) emits `Token { kind, range }` for command (first word in each pipeline scope), strings (single + double, with `$var` splitting double-quoted runs), variables (`$NAME` / `${expr}`), pipes / redirects / `;` / `&&` / `||`, flags (`-x`, `--long=value`), comments (`#` to end-of-line at token boundary), and `Word` for everything else. `paint_prompt_editor_at` walks the token list per row and paints each token in its kind's colour; the editor's `EDITOR_FG` is the `Word` default. Subshells (`$(…)`, backticks) and proper here-doc parsing defer to a follow-up.
- ✅ **4I — Local completion (Tab)** ([#107](https://github.com/enthal/termica/pull/107)). Path + history + `$PATH` per [04 §"Tab handling"](04-prompt-editor.md#tab-handling): the local popup with the three sources, smart-Tab extension, and live filtering. (CLI-native drivers + per-pane shell sidecars are the separate "Tab completion engine" item below.)
- ✅ **4J — History walk (Up/Down) + Ctrl+R popup** ([Ctrl+R overlay #96](https://github.com/enthal/termica/pull/96); [multiline-aware Up/Down #105](https://github.com/enthal/termica/pull/105)).

4A–4C is the load-bearing trio: at the end of 4C the user can type a command in the editor, press Enter, see it execute in a `Running` block, and see the result in a `Sealed` block. 4D–4G are the UX polish that makes it feel block-oriented. 4H–4J are independently-sliceable polish.

**Acceptance:** at a `bash`/`zsh` prompt with managed integration, the user types into the editor inside the `Prompt` block; Enter executes the command in a `Running` block that seals on `CommandFinished`; the duplicate echo never appears in the transcript; vim still works (alt-screen runs inside a `Running` block, sealed empty-transcript on exit).

**Phase 7 (command blocks) is largely subsumed by Phase 4G's block-header chrome work.** The collapse / expand / context-menu affordances that Phase 7 was going to add layer cleanly on top of 4A's block infrastructure; they become a small Phase-7 polish PR rather than the foundational work the original spec assumed.

### Phase 5 — Status header

- `termica-context`: cwd / git-branch / dirty-count / last-exit / last-duration providers.
- Async debounced probes; never block the UI thread.
- `icons.rs` module with `Painter`-drawn glyphs.
- Click actions: copy path, copy branch.

**Acceptance:** the header replaces most of what `PS1` carried; updates feel instant; never blocks paint.

### Phase 6 — History (local + global) + Ctrl+R ✅

- ✅ `termica-history`: SQLite schema for command runs; pane-local recall + shell-history-file replay on startup.
- ✅ Up-arrow walk (multiline-aware, [#105](https://github.com/enthal/termica/pull/105)); Ctrl+R popup ([#96](https://github.com/enthal/termica/pull/96)).
- ✅ Scope toggle in the Ctrl+R overlay (this pane / global).
- ⏳ Fuzzy match via `nucleo` + cwd-biased ranking — the current matcher is a placeholder; tracked in [#119](https://github.com/enthal/termica/issues/119).

Shipped under the Phase 4J slices ([#91](https://github.com/enthal/termica/pull/91)–[#97](https://github.com/enthal/termica/pull/97), [#105](https://github.com/enthal/termica/pull/105)) since the editor needed history to be useful.

**Acceptance:** Ctrl+R returns relevant historical commands across panes and previous sessions in under 50 ms for a 50k-entry history.

**Status:** ✅ complete — [#6](https://github.com/enthal/termica/issues/6) closed. History storage, ↑/↓ recall, and the scoped Ctrl+R overlay are in daily use; `nucleo` ranking is the one remaining follow-up ([#119](https://github.com/enthal/termica/issues/119)).

### Phase 7 — Command blocks ✅

- ✅ `CommandRun` lifecycle wiring: open on submit / `Preexec`; seal on `CommandFinished`.
- ✅ Transcript renders command blocks with header chrome (cwd, exit, duration; sticky-top header).
- ✅ Failed exits are visually distinct (red `exit N`); within- and cross-block selection + copy.
- ⏳ Collapse / expand, copy-output / rerun, context menu — tracked in [#120](https://github.com/enthal/termica/issues/120).

The foundational block model was pulled forward into Phase 4 (the block-model pivot, [#51](https://github.com/enthal/termica/pull/51)–[#116](https://github.com/enthal/termica/pull/116)); as noted above, Phase 7 is "largely subsumed by Phase 4G."

**Acceptance:** the transcript becomes navigable as a sequence of command blocks. Failed exits are visually distinct. Click → collapse works.

**Status:** ✅ complete — [#7](https://github.com/enthal/termica/issues/7) closed. The block model, header chrome, and selection/copy shipped under Phase 4; the remaining collapse/expand/rerun affordances are tracked in [#120](https://github.com/enthal/termica/issues/120).

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

- **Tab completion engine** (CLI-native drivers + per-pane shell sidecar for bash / zsh / fish) per [04a](04a-completion.md). Supersedes the earlier "OSC request/response shell-completion bridge" stub — the same goal, cleaner mechanism (private stdio JSON sidecar, not an OSC channel over the PTY). Implementation slices: drivers → fish sidecar → bash sidecar → zsh sidecar, in that order.
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
