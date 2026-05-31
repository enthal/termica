**← Previous:** [08 — Persistence](08-persistence.md) | **Next:** [10 — Roadmap](10-roadmap.md) →

# 09 — Testing

Termica's correctness story rests on a small set of brutally simple invariants in the mode machine, the marker parser, the prompt-submit path, and the persistence layer. The testing strategy is built around making those invariants impossible to regress.

## The hybrid rule (recap)

[CLAUDE.md](../CLAUDE.md) defines the rule in full. Recap:

| Layer | Surface | Rule |
|---|---|---|
| Strict | Terminal engine; pane-mode state machine; OSC marker parser; prompt-editor submit path; persistence schemas; bug fixes anywhere | Failing test FIRST, same commit, on the pre-change tree. Same-commit ship. |
| Pragmatic | Tile chrome, status header rendering, theme application, command palette wiring, animations, keyboard binding glue | Same-commit ship; order doesn't matter. Extract testable logic out of UI fns. |

The rest of this doc is *how* to write those tests well.

## Test layers

### 1. Unit tests (`#[cfg(test)]` in-module)

Default and minimum bar. Use for:

- Marker parsing (input bytes → `MarkerEvent` events).
- `PromptController` mode transitions (input event → mode + reason).
- `PromptEditor` operations (cursor / selection / undo invariants).
- Echo suppression buffer state.
- History scope filtering.
- Persistence encode/decode.
- Pure shell tokenizer.
- Anything that does not require a real PTY, a real shell, or a real GPU.

These run in milliseconds; they are the test layer that fires on every save.

### 2. VT golden tests

The terminal layer's primary test layer. A `testdata/vt/<scenario>/` directory holds:

```
testdata/vt/bash-basic/
├── input.bytes              # raw PTY byte stream
├── grid.snap                # expected grid after replay (insta or custom)
├── markers.snap             # expected MarkerEvent sequence
└── README.md                # what this scenario covers
```

The test fixture replays `input.bytes` into the terminal engine and asserts both snapshots. Scenarios on day one:

- `bash-basic`: prompt → simple command → exit status.
- `zsh-basic`: same, but zsh.
- `vim`: alternate-screen enter, cursor movement, color, exit.
- `less`: alternate-screen + mouse reporting.
- `htop`: dense styled output, alternate-screen.
- `fzf`: bracketed paste + alternate-screen.
- `ssh`: passthrough, no Termica integration after the ssh.
- `split-reads`: same byte stream as `bash-basic`, but each byte is delivered as its own read; result must be identical.

Regenerate with `cargo test -- --update-goldens` (or `TERMICA_UPDATE_GOLDENS=1`). Always `git diff` before committing.

The `split-reads` scenario is non-negotiable: it's the canonical regression test for the marker-parse-across-read-boundaries bug class.

### 3. Integration tests (`tests/`)

These actually spawn `bash` and `zsh` (skipped on CI runners that lack one, with a clear `cfg`):

- Spawn `bash --rcfile <tmp-rc-with-our-integration>`.
- Drive it with a sequence of input bytes.
- Assert the resulting marker stream.
- Assert the resulting `CommandRun` rows after persistence.

The spawn helper lives in a `testkit` crate (post-MVP separation; same root crate today). It owns:

- TmpDir for `$HOME`, `$XDG_DATA_HOME`.
- Synthetic rcfile that sources our integration script.
- A `PtySession` wrapper that takes scripted input and emits captured marker events.

Integration tests run in seconds, not milliseconds; they live alongside unit tests but are tagged so a fast loop can skip them.

### 4. Snapshot tests via `egui_kittest`

Same setup that works well in knauty. Per-widget snapshots in `tests/snapshots/`:

- Empty pane.
- Pane in `RawTerminal` with output.
- Pane in `ShellPromptEditor` with empty editor.
- Pane in `ShellPromptEditor` with multiline editor.
- Pane with `Ctrl+R` history popup open.
- Pane with completion popup open.
- Pane with in-pane search overlay.
- Status header with each chip variant.
- Failed command block (red gutter).
- Collapsed command block.

Regenerate with `UPDATE_SNAPSHOTS=1 cargo test`. The snapshot review protocol from [CLAUDE.md](../CLAUDE.md) applies: read every changed `.png`, check `*.diff.png`, look for surprises, ask the user if anything is unexpected.

Snapshot threshold lives in `kittest.toml` ([knauty's pattern](../../knauty/kittest.toml)), with a higher tolerance on Linux than macOS because of font-rendering differences across GPUs.

### 4b. Synthetic `PaneSession` harness — multi-frame `render_pane` tests

The snapshot layer above asserts what a pane looks like in *one* frame. A whole class of bugs only manifests across frames: scroll-offset state poisoning that survives into the next paint, focus migration on submit, cursor-anchor drift after `clear_all`, alt-screen takeover sticking after the program exits. The two scroll regressions in PR #86 and the alt-screen-blank fallout were both exactly this shape — a single-frame test cannot catch them.

We need a synthetic `PaneSession` fixture so `render_pane` runs under `egui_kittest` with no real PTY in the loop. This is currently missing; the next bug fix in this class should land it as the infrastructure PR before the fix.

**What to build:**

- `PaneSession::synthetic_for_tests(rows, cols, pane_id)`: constructor that builds a `PaneSession` with no `PtySession`, no reader thread, no `wrapper_dir`, no recorder. The internal `bytes_rx` is a `mpsc::Receiver` whose `Sender` is exposed via a sibling helper so a test can push raw VT bytes deterministically; `drain()` consumes them the same way the production path does. `pty.write()` / `resize()` become no-ops on the synthetic variant (a `#[cfg(test)]` enum or a small trait, whichever is simpler).
- `PaneSession::inject_lifecycle(event)` (test-only): pushes a `LifecycleEvent` directly into the controller, bypassing the DCS-JSON parse. Lets tests drive `Preexec → CommandFinished → Precmd` without standing up a real shell or hand-crafting marker bytes.
- `PaneSlot::synthetic(pane_id)`: thin wrapper that pairs the synthetic session with a default `PaneUiState`.
- `kittest::Harness::builder().build_ui(|ui| render_pane(ui, ctx, &mut slot, None, false))` is the entry point. The test calls `harness.step()` to advance one frame; between steps it pushes bytes, injects lifecycle events, or sets `slot.ui.*` flags and asserts on the resulting `slot` / pane render.

**Canonical first test** (the one that would have caught #86 + the regression that lived briefly on its successor PR):

```rust
// fix/polish-22-scroll-to-bottom-without-infinity regression.
// pwd × 2 — second submit must not poison ScrollArea state.
let mut slot = PaneSlot::synthetic(PaneId(1));
let mut harness = Harness::builder().build_ui(|ui| {
    render_pane(ui, ui.ctx(), &mut slot, None, false);
});
push_bytes(&slot, b"$ "); harness.step();
submit_command(&mut slot, "pwd"); harness.step();
push_bytes(&slot, b"/home/test\n$ "); harness.step();
submit_command(&mut slot, "pwd"); harness.step();
push_bytes(&slot, b"/home/test\n$ "); harness.step();
assert_visible_text_contains(&harness, "/home/test");
assert!(scroll_offset_finite(&harness));
```

The assertion shape that matters: after two submits, the rendered block stack is still visible (content didn't vanish), the `ScrollArea` offset is finite (not poisoned by `f32::INFINITY`), and a third byte arrival still paints in the visible region. Each is one line; together they nail the class of bug.

**Where it sits in the layer table:**

| Layer | What it catches |
|---|---|
| Unit | Pure logic — no UI, no PTY |
| VT golden | Engine reaction to a byte stream |
| Integration | Real shell, real PTY |
| `egui_kittest` snapshot | Single-frame pane appearance |
| **Synthetic `PaneSession` harness** | **Multi-frame `render_pane` state — scroll, focus, anchor drift, alt-screen takeover** |
| Perf smoke | Throughput / latency budgets |

The synthetic harness sits between the snapshot layer (one frame) and integration (real shell). It is the *only* layer that can test the interaction of `render_pane`, `PaneSlot::ui`, and inter-frame ScrollArea/Memory state without a real PTY and without paying integration-test latency. Any bug fix whose root cause is "frame N's state survives into frame N+1 wrong" belongs here.

### 5. Perf smoke

A small `benches/` directory or test-mode benchmark for two scenarios:

- **Yes flood**: feed 100 MB of `y\ny\ny\n` style output. Assert: completes under N seconds, repaints under M total (not M per byte).
- **Editor latency**: in `ShellPromptEditor`, simulate 200 keystrokes-per-second. Assert: P99 keystroke-to-paint under K ms.

These run rarely (e.g. on release tag), but a regression on either is a release-block.

## Visual decision workflow (the picker)

Aesthetic choices that snapshot tests can't decide for us — match-highlight color, separator opacity, focus-affordance shape, panel sizes, dim red intensities — go through `src/visual_picker.rs`, a reusable eframe app that renders N side-by-side variants and writes the chosen variant id to `/tmp/termica-picker-choice.txt` on click.

A picker is a small binary in `examples/pick_*.rs` that:

1. Defines 2–6 `Variant`s (each: stable kebab-case id + display label + a `Fn(&mut egui::Ui)` painter).
2. Calls `visual_picker::run("decision title", variants, output_path)`.
3. The window stays open until the user clicks a "Pick this" button. Closing without picking writes nothing — file absence means "cancelled."

Pattern in this codebase:

- The agent (or developer) authors variants when a visual choice is open.
- The user runs the example (`cargo run --example pick_history_row_separator`), clicks, the file is written and the app exits.
- The chosen id flows into the production code (typically as a constant) with a comment naming the picker variant that won. Picker files stay in `examples/` as a record of the decision.

Picker-derived constants currently in the repo (see [`src/render.rs`](../src/render.rs) and [`src/history_overlay.rs`](../src/history_overlay.rs)):

- `MATCH_HIGHLIGHT` — warm-gold + underline for `^R` matched-substring runs.
- `ROW_GAP` — 6 px vertical breath between `^R` result rows.
- `MIN_TAB_TITLE_CHARS = 7` — tab strip min width (~48 px for `~`).
- `FOCUSED_EDITOR_CHROME_COLOR` — dim grey-white rounded outline around chip + editor.
- `BLOCK_SEPARATOR_GAP = 10.0`, `BLOCK_SEPARATOR_HAIRLINE` — between sealed blocks.
- `BLOCK_HEADER_CHIP_STROKE`, `FAILED_BLOCK_BG` — chip outline + dim red wash on non-zero exits.

**Premultiplied alpha gotcha.** `Color32` const constructors only accept premultiplied values. Tiny alphas (`0x08`, `0x10`, …) on a high-RGB base (`0xa0`) silently misrender in the premul form (RGB > alpha is invalid premul and clamps), making "dimmer" variants render at the same brightness. Picker examples use `from_rgba_unmultiplied` so authoring is natural; production constants store the precomputed premul values and the comment names the unmultiplied source for clarity.

## The five safety rules, tested

From [05](05-pane-modes.md), one test each:

| Rule | Test name (canonical) | Layer |
|---|---|---|
| 1. Prompt editor never active without marker | `prompt_editor_unavailable_without_integration` | Unit |
| 2. Alternate screen disables editor | `alt_screen_forces_raw` | VT golden |
| 3. Markers are authoritative | `heuristic_prompt_does_not_promote` | Unit |
| 4. Shell never sees editing keystrokes | `editor_keys_do_not_reach_pty` | Unit + integration |
| 5. TTY programs receive raw input | `raw_mode_round_trips_arrow_keys` | VT golden + integration |

Each follows the strict rule: write the failing test first.

## Determinism rules

- **No wall-clock time.** Never `Utc::now()` / `SystemTime::now()` / `Instant::now()` in tests. Use fixed `i64` constants for `started_at`, a `FakeClock` injected into anything time-sensitive (`PromptController` debounces, echo suppression timeouts, `Pane.context` staleness).
- **No live random.** Every seeded test takes a seed and logs it on failure.
- **No sleeps.** Drive deterministic time forward via the `FakeClock`. If a test feels like it needs `sleep`, it is testing an async race that the production code should have a synchronization point for.
- **No shared global state across tests.** Each test owns its own `Persistence`, `PtySession` (or fake), and `PromptController`.

## Flaky tests are never acceptable

Same rule as liquid-loom. A flaky test is a bug in the test or the code, not "the runner." Diagnostic pattern:

1. Reproduce: `for i in $(seq 1 50); do cargo test ... ; done` until you see the failure.
2. Identify the race / missed synchronization / leaked state. Name it.
3. Fix it structurally. If the production code has a race, fix the production code. If the test has an ordering assumption, fix the assumption.
4. Add a regression test that fails on the pre-fix tree and passes on the post-fix tree.

No retries, no `@ignore_flaky`, no "bump the threshold." Those push the probability down, they don't fix the bug.

## Coverage we don't pursue

- 100% line coverage. We pursue **invariant coverage**: every transition in the mode machine, every marker, every persistence migration, every command-run lifecycle path.
- Mutation testing. Maybe later; not foundational.
- Cross-shell-version matrix beyond bash and zsh. Out of scope for v1.

## Test commands at a glance

```sh
cargo test                                         # all fast tests
cargo test -- --update-goldens                     # regenerate VT goldens
UPDATE_SNAPSHOTS=1 cargo test                      # regenerate egui_kittest snapshots
cargo test --test integration_                     # integration suite (spawns shells)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## CI

The Phase 0 PR ships a `.github/workflows/ci.yml` mirroring liquid-loom's: `fmt`, `clippy`, `test --workspace --locked`, on `push` to `main`, `pull_request`, and `merge_group`. The Rust version is pinned in `rust-toolchain.toml` and echoed in the workflow file. We will run on `ubuntu-latest` and `macos-latest` from the first PR; Windows is not yet a target.

---

**← Previous:** [08 — Persistence](08-persistence.md) | **Next:** [10 — Roadmap](10-roadmap.md) →
