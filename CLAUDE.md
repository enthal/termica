# Claude Code Instructions

## The project

Termica is a native terminal workspace written in Rust on top of [egui](https://github.com/emilk/egui), [egui_tiles](https://github.com/rerun-io/egui_tiles), and [alacritty_terminal](https://crates.io/crates/alacritty_terminal). It is a real PTY-backed terminal emulator at the bottom and a modern editor-driven prompt experience on top, mediated by a strict pane-mode state machine. See [SPEC.md](SPEC.md) and the docs under [spec/](spec/) for the full design.

The repo is **pre-implementation**: commit zero contains only the spec, license, toolchain pin, and a placeholder `main`. Expect the first feature PR to deliver the Phase 1 terminal pane described in [spec/02-terminal-engine.md](spec/02-terminal-engine.md). Work in conversation with the user — what the spec says is the source of truth.

## The spec is the source of truth

- [SPEC.md](SPEC.md) and [spec/*.md](spec/) are canonical. Read the relevant section before writing code that touches an area.
- When code and spec disagree, **raise the conflict with the user** and decide whether to change the code or the spec. Never silently drift from the spec.
- Any normative change (trait shape, protocol, OSC marker payload, pane-mode rule, wire format, persistence schema) MUST update the spec in the same commit.
- The five "safety and correctness rules" in [spec/05-pane-modes.md](spec/05-pane-modes.md) are not negotiable; they are the product. A correctness regression there is a P0 bug.

## Discussions with the user

When mentioning a GitHub Issue or PR, always use the Issue/PR number, as a clickable link (e.g. `[#42](https://github.com/enthal/termica/issues/42)`).

## Tests and discipline — hybrid rule

Termica's testing rule is intentionally split by layer because the cost of getting it wrong is not uniform.

### Strict layer — tests written FIRST, same commit

For everything in this list, **write a failing test before any implementation code lands.** The test must fail (or not compile) on the pre-change tree; that's how you know the test actually validates the behavior.

- The terminal engine (anything that touches grid state, escape interpretation, cursor, scrollback, alternate screen).
- The pane-mode state machine and any transition rule ([spec/05-pane-modes.md](spec/05-pane-modes.md)).
- The OSC marker parser and shell-integration event stream ([spec/03-shell-integration.md](spec/03-shell-integration.md)).
- The prompt-editor command-submission path (Enter, multiline, echo suppression — [spec/04-prompt-editor.md](spec/04-prompt-editor.md)).
- Persistence: scrollback chunk format, SQLite schemas, restore semantics ([spec/08-persistence.md](spec/08-persistence.md)).
- Any bug fix anywhere in the codebase: reproduce with a test that fails on the pre-fix tree, fix it, watch it go green.

### Pragmatic layer — tests in the same commit, order doesn't matter

For the rest (tile layout chrome, status-header rendering, theme application, command palette wiring, animations, keyboard binding glue), tests ship in the same commit but don't have to be written first. **If the logic can be tested without a UI, it must not live inside a UI function** — extract it into a pure function and test that. Indicators that something belongs in a pure function: index arithmetic, string manipulation, state transitions, any `if/else` on computed values.

### What never moves layers

- "I'll add tests later" means "there will be no tests." Both layers ship tests in the same commit.
- A passing test that was written after the code is a smell — it may be asserting whatever the code happens to do, not what the code should do.
- Determinism: never `Instant::now()` / `SystemTime::now()` / live timestamps / unseeded random in tests. Use fixed constants or an injected fake clock. A flaky test is a bug in the test or the code, never "just the runner".

### Test layers (which tool for which job)

Map new work to the appropriate layer (the full strategy lives in [spec/09-testing.md](spec/09-testing.md)):

- **Unit tests** (`#[cfg(test)]` in-module): pure logic — marker parsing, mode-transition decisions, history filtering, search ranking, persistence encoding. Default and minimum bar.
- **VT golden tests** (`testdata/vt/<scenario>/`): feed a recorded PTY byte sequence into the terminal engine; snapshot the grid state and the `PromptController` event trace. Cases: plain bash, vim, less, htop, fzf, ssh, alternate-screen enter/exit, bracketed paste, marker streams split across read boundaries. Regenerate with `cargo test -- --update-goldens` (or `TERMICA_UPDATE_GOLDENS=1`) and `git diff` the result before committing.
- **Integration tests** (`tests/`): spawn a real `bash --rcfile` or `zsh -f` with our integration script installed in a tempdir; run a sequence; assert the marker event stream. This is how we know the integration actually works on a real shell, not just on synthetic bytes.
- **Snapshot tests** ([`egui_kittest`](https://crates.io/crates/egui_kittest), like knauty's setup): render a pane / status header / popup with deterministic state and compare to a saved `.png`. Regenerate with `UPDATE_SNAPSHOTS=1 cargo test`.
- **Perf smoke** (`benches/`, criterion or a small custom harness): feed N MB of output through the engine and renderer; assert that we do not repaint per byte. Catch regressions where a future change accidentally triggers a frame per glyph.

### Snapshot review

After regenerating `egui_kittest` snapshots (`UPDATE_SNAPSHOTS=1 cargo test`):

1. **Visually inspect every changed `.png`.** Use the Read tool to view each updated snapshot image.
2. **Check for `*.diff.png` files** in the same directory. These show pixel differences between old and new snapshots. Read them to understand what changed.
3. **Look for surprises.** Confirm the visual changes match the intent of the code change. Watch for unintended side effects like missing elements, broken alignment, color regressions, or rendering artifacts.
4. **Ask the user** if anything looks unexpected before committing.

## Build & test

- **Build:** `cargo build --workspace`
- **Test all:** `cargo test --workspace`
- **Lint:** `cargo clippy --workspace --all-targets -- -D warnings`
- **Format:** `cargo fmt --all`
- **Docs:** `cargo doc --workspace --no-deps`

Run `cargo fmt`, `cargo clippy`, and `cargo test --workspace` before every commit. Treat clippy warnings as errors.

A developer-installable pre-commit hook lives at `scripts/git-hooks/pre-commit` (install it with `scripts/install-git-hooks.sh`, which symlinks `scripts/git-hooks/*` into `.git/hooks/`). It runs **fmt + clippy + the markdown hard-wrap lint** (`cargo test --test markdown_no_hardwrap`) locally so style regressions can't slip into a commit. It deliberately does **not** run the general `cargo test` suite, because the project's tests-first discipline requires committing a failing behavioural test *before* the implementation that makes it pass — a pre-commit that blocked failing tests would block the workflow itself. The markdown lint is the one exception: it's a deterministic lint, never committed red-first, so it sits with fmt/clippy. CI runs the full suite and gates merge, so behavioural test failures still block the world; they just block it at the right boundary. Bypass the hook once with `git commit --no-verify` only if the user explicitly says so.

## Design principles

These are operational reminders. The full rationale is in the spec.

- **Mode safety is the product.** The pane-mode state machine in [spec/05-pane-modes.md](spec/05-pane-modes.md) is the load-bearing invariant. Default to `RawTerminal` whenever a transition would otherwise be ambiguous. Promotion to `ShellPromptEditor` is gated on a confirmed marker; demotion on Enter is eager. **A wrong-mode dropped keystroke is a corruption-class bug.**
- **Terminal correctness comes first.** vim, htop, less, fzf, ssh, tmux must behave exactly like in any other modern terminal. The editor UX is a layer on top of a boring, correct terminal — never the other way around.
- **Make wrong states unrepresentable.** If an invariant requires "always do X before Y," encapsulate both in a single function so callers cannot forget. Don't rely on comments or discipline. Things that must happen together should be impossible to do separately.
- **Fix bugs structurally, not with guards.** When a bug involves stale or inconsistent state across a transition, don't add a check at the call site — replace the loose state with a struct that is updated atomically. The Knauty playbook applies here too.
- **Shell integration is the only source of truth for "are we at a prompt?"** Heuristics can enhance; they must never be required for correctness. If markers aren't installed, the prompt editor is simply unavailable and the app is still a usable terminal.
- **Do not block the UI on probes.** Git status, cwd, environment, kube/aws context — all async, all debounced, all cancellable on pane teardown.
- **No silent data loss.** Anything the user typed or the shell printed must survive a crash if it was confirmed-written. Persistence is structural, not best-effort.
- **Same code paths in every environment.** A test run, a release build, and a debug session all go through the same `PromptController`. There is no "test mode" inside the mode machine.

## Code style

- Prefer structurally safe code over code that is correct by coincidence (e.g., `char_indices()` over byte-level string slicing; typed newtypes over bare `u64`s for IDs).
- Use typed IDs for everything durable: `PaneId`, `SessionId`, `CommandRunId`, `ScrollbackChunkId`, `HistoryEntryId`, `WindowId`, `TabId`. These are not interchangeable; the type system should say so.
- **Do not use Unicode symbols for icons.** They render as tofu on some systems. Draw icons using egui's `Painter` API in an `icons.rs` (knauty pattern). For terminal output, Unicode is the shell's problem and is rendered cell-by-cell by the terminal renderer; we are not "drawing icons" in that path.
- **Map naming:** `things_by_key` for `Map<key, thing>`. For nested: `things_by_inner_by_outer` means `Map<outer, Map<inner, thing>>` (read right-to-left). For collection values, include the container: `thing_vecs_by_key` for `Map<key, Vec<thing>>`.
- **Duplicate widget IDs are a critical bug.** Any UI element may be displayed in multiple panes simultaneously. Never assume a widget will only render once per frame. Rules:
  - `Id::new("string")` and `egui::TopBottomPanel::top("string")` create **global** IDs that are NOT scoped by `ui.push_id()`. Always salt them with a pane-specific value (e.g., `pane_id`).
  - For widgets inside a pane's UI, prefer `ui.id().with("key")` over `Id::new("key")` — the former inherits the pane's ID scope automatically.
  - When creating egui widgets with auto-generated IDs (e.g., `ScrollArea`, `CollapsingHeader`, `ComboBox`) inside a loop or in pane code, pass a unique `id_salt`.
  - Before adding any new ID, ask: "Could two instances of this widget exist on screen at the same time?" If yes, the ID must include a disambiguator.
- Trait shapes in the spec are normative. If you need to deviate, update the spec in the same commit with the reason.
- **No `unsafe` code.** Every Termica crate sets `#![forbid(unsafe_code)]` at the crate root. PTY and OS-handle interaction is mediated through `portable-pty` and `alacritty_terminal`, which contain `unsafe` themselves but expose safe APIs we consume. If you find yourself wanting `unsafe`, stop and ask — it almost certainly means a different dependency, a different abstraction, or a redesign is the right answer.
- No `unwrap()` or `expect()` in non-test code except where a contract makes failure impossible — and even then, prefer a typed `Result`. `expect` messages must describe the invariant, not restate the operation.
- **Markdown is never hard-wrapped.** One logical line per paragraph, list item, and block-quote — let the editor/renderer soft-wrap. Do not insert newlines mid-paragraph to hit a column width (it carries no meaning and churns diffs). Applies to every `.md` file. Enforced by `tests/markdown_no_hardwrap.rs` (run by CI and the pre-commit hook).

## Git commit protocol

Before every commit:

1. **Tests first where the strict layer applies.** If you are committing engine, mode-machine, marker-parser, prompt-editor command path, or persistence code, the test(s) MUST be present and MUST have failed on the pre-change tree. Mention the test in the commit body when useful.
2. **Format.** `cargo fmt --all`; stage the formatting changes.
3. **Lint.** `cargo clippy --workspace --all-targets -- -D warnings` must pass.
4. **Test.** `cargo test --workspace` must pass. If snapshot tests fail due to intentional visual changes, regenerate with `UPDATE_SNAPSHOTS=1 cargo test --workspace` and include the updated snapshots in the commit; also review every `*.diff.png`.
5. **Spec sync.** If the commit changes a spec-defined trait, OSC marker, pane-mode rule, persistence schema, or any other normative item, update the corresponding [spec/*.md](spec/) file in the same commit.
6. **Include config changes.** If `CLAUDE.md`, `.claude/settings.json`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, or `rustfmt.toml` changed, include them.

Commits should be small and reviewable. A commit that touches >1 crate without a clear reason is a signal to split.

## Branches and pull requests

- **Never commit directly to `main` after commit zero.** All changes land via a feature branch → PR → squash merge. Commit zero (this file, the spec, the license, the placeholder) is the one exception.
- **Branch naming:** `<kind>/<slug>` where `kind` is one of `feat`, `fix`, `refactor`, `spec`, `chore`, `test`. Examples: `feat/terminal-pane`, `spec/sharpen-mode-machine`, `fix/marker-split-across-reads`. Dashes in the slug, not underscores.
- **Start the branch first.** `git switch -c <kind>/<slug>` from an up-to-date `main`. If you catch yourself having already committed on local `main`: `git switch -c <kind>/<slug>` (takes the commits with you), then `git switch main && git reset --hard origin/main`.
- **One PR per logical change.** Small and reviewable.
- If a GitHub Issue exists, include `Closes #<n>` in the PR description.
- The PR summary reflects all commits on the branch, not just the latest.
- Before merging a PR, ask yourself and present a list of items (if any found) to the user:
    - _For the code/`fn`s changed on this branch, what concerns do you have about correctness, safety, readability, DRYness? What's one change you would make if you had another hour?_
- Before merging, update [README.md](README.md) to reflect any user-facing changes and the spec to reflect any normative changes.
- **Squash on merge**; keep the branch. After merging, switch to `main` and pull.

## Watching CI on open PRs

- **Use the `Monitor` tool**, not polling loops. After `gh pr create`, set up a persistent `Monitor` task that watches `gh pr list` / `gh pr checks` and emits one line per state change — the harness then notifies you when CI completes, when a check turns red, or when a PR is merged. Keep working in parallel; never sit idle waiting on CI.
- **Do not write `until` loops over `gh pr checks`.** They block the agent, burn context with retries, and reproduce exactly what `Monitor` already does correctly. The same goes for `sleep N && gh pr checks` chains — they are forbidden by the harness in any case.
- Acceptable one-shot pattern: `Bash` with `run_in_background: true` running a command that exits when a single condition is true (e.g. `gh pr checks N --watch --fail-fast`). Use this when you specifically need a single completion notification for one PR; use `Monitor` when you want continuous events across the work session or across multiple PRs.
- Don't `gh pr merge` inside a polling loop either — merge only after the monitor (or a deliberate manual user instruction) tells you a PR is green.
- The same rule applies to anything else with discrete events: log tails, CI for non-PR commits, external job state. If you're tempted to poll, reach for `Monitor` instead.

## Command governance

- Use relative paths in shell commands, not absolute paths.
- Avoid `git -C <abs-path>`; it breaks project-level Claude permissions.
- Don't skip hooks (`--no-verify`) or bypass signing unless the user explicitly asks. If a hook fails, fix the underlying issue.
- **Only kill processes you started.** When you launch a process (e.g. `termica` for validation), capture its PID and kill *only* that PID. Never `pkill`/`kill` by name or pattern — you will hit instances the user (or another agent) started, and a name pattern can even match your own shell. Keep the PID in your working context, not a shared file like `/tmp/foo.pid` — a concurrent session can overwrite it and you'd kill the wrong process.
- **Worktrees live under `./.claude/worktrees/<slug>`.** Create them there (`git worktree add .claude/worktrees/<slug> …`), not in sibling directories. When operating inside a worktree, use its explicit path since the shell cwd may reset between commands.
