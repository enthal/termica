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

### 5. Perf smoke

A small `benches/` directory or test-mode benchmark for two scenarios:

- **Yes flood**: feed 100 MB of `y\ny\ny\n` style output. Assert: completes under N seconds, repaints under M total (not M per byte).
- **Editor latency**: in `ShellPromptEditor`, simulate 200 keystrokes-per-second. Assert: P99 keystroke-to-paint under K ms.

These run rarely (e.g. on release tag), but a regression on either is a release-block.

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
