**← Previous:** [04 — Prompt editor](04-prompt-editor.md) | **Next:** [06 — Workspace & tiles](06-workspace-and-tiles.md) →

# 05 — Pane modes

This is the load-bearing safety invariant of the entire product. Read it twice.

## The five safety and correctness rules (normative)

1. **The prompt editor is NEVER active unless the pane is at a Termica-controlled, lifecycle-confirmed shell prompt.**
2. **Alternate screen always disables the prompt editor.** No exceptions.
3. **Lifecycle messages from Termica's own bootstrap are authoritative.** Foreign OSC 133 / OSC 1337 sequences from other tools' shell integration scripts are ignored. Heuristics may enhance but must not be required for correctness.
4. **The shell never sees editing keystrokes from the editor.** Only the final command bytes plus a newline.
5. **Real TTY programs receive raw input.** Encoded promptly, never coalesced.

A code change that violates one of these is a P0 bug, not a design preference. The hybrid testing rule in [CLAUDE.md](../CLAUDE.md) applies in its **strict** form to this entire document's surface area.

## The six modes

```rust
pub enum PaneMode {
    /// Initial state. PTY has been spawned but Termica's bootstrap
    /// has not yet emitted `integration_ready`. All PTY output is
    /// buffered and not displayed; all user keystrokes are dropped.
    /// Transitions to RawTerminal on `integration_ready` or to
    /// Degraded on `integration_error` or the bootstrap timeout.
    Bootstrapping,

    /// Default operational state. Keystrokes go to the PTY; output
    /// renders normally; no editor surface; mouse selects the
    /// transcript.
    RawTerminal,

    /// The pane is at a Termica-confirmed shell prompt. The egui
    /// editor owns input; only `submit()` produces PTY bytes.
    ShellPromptEditor,

    /// Terminal has entered the alternate screen (vim/htop/less/fzf/
    /// tmux). Strictest raw mode; mouse and bracketed-paste honor
    /// the program's reporting state.
    AlternateScreen,

    /// PTY child has exited. Transcript remains; "restart shell" UI
    /// is visible; history and search remain accessible.
    Dead,

    /// Bootstrap failed (timeout, integration_error, or unsupported
    /// version). The pane is operational as a raw terminal — input
    /// goes to the PTY, output renders — but the editor will never
    /// activate for this shell run. `restart_shell` resets to
    /// `Bootstrapping` and tries again.
    Degraded,
}
```

The set is closed. New sub-states tend to be the answer to bug pressure that should have been fixed structurally; adding a seventh mode requires an explicit spec update.

`Bootstrapping` and `Degraded` are new in the managed-shell-integration design ([03](03-shell-integration.md)). They make the previously-implicit "before/after the integration has confirmed itself" window explicit in the type system, which is the only honest place for it.

## Transition diagram

```text
                                       ┌──────────────────────────────────┐
                                       │                                  │
   spawn ──► [Bootstrapping] ──integration_ready──► [RawTerminal] ──precmd──► [ShellPromptEditor]
                  │                                       │   ▲                       │
                  │  timeout OR                           │   │ submit() / Ctrl-C /   │
                  │  integration_error                    │   │ Esc / mode reset      │
                  ▼                                       │   │                       │
              [Degraded]                                  │   └───────────────────────┘
                  │                                       │
                  │  restart_shell                        │
                  └──────► [Bootstrapping] (new shell)    │
                                                          │
                                              ┌───────────┴──exit alt screen──┐
                                              ▼                               │
                                  ┌─►[AlternateScreen]◄──────enter alt screen─┴───────┐
                                  │         │                                          │
                                  │  pty exit                                          │
                                  │         ▼                                          │
                                  └────► [Dead] ◄─────────pty exit (from any mode)─────┘
                                          │
                                          │  restart_shell
                                          └──────► [Bootstrapping]
```

Walk it carefully:

- **Start** in `Bootstrapping`. Termica's bootstrap script runs.
- **`integration_ready` → `RawTerminal`** when the bootstrap completes successfully (DCS-JSON message with supported version).
- **Bootstrap timeout (3 s) OR `integration_error` OR unsupported version → `Degraded`.** Banner is shown; pane is still operational as a raw terminal.
- **`precmd` (DCS-JSON) → `ShellPromptEditor`**, only when:
  - the pane is currently in `RawTerminal` (never `Bootstrapping`, `AlternateScreen`, `Degraded`, `Dead`);
  - the most recent transition out of `ShellPromptEditor` was at least one frame ago (debounce against the same prompt round-tripping).
- **`Enter` submit → `RawTerminal`**, eagerly, before the PTY write happens ([04](04-prompt-editor.md)).
- **Alternate-screen ON → `AlternateScreen`** from `RawTerminal` or `ShellPromptEditor`. From `Bootstrapping` it is impossible (PTY output is buffered, not interpreted by the renderer); from `Degraded` it is allowed.
- **Alternate-screen OFF → `RawTerminal`** (not directly to `ShellPromptEditor`; only a fresh `precmd` promotes).
- **PTY exit → `Dead`** from any mode except `Dead`.
- **`restart_shell` from `Dead` or `Degraded` → `Bootstrapping`** with a fresh PTY session.

## Why the default after bootstrap is `RawTerminal`

Because the cost of being wrong is asymmetric. If the editor is unavailable when it could have been, the user types into the shell as in any other terminal: zero data loss, zero corruption, mild ergonomic loss. If the editor is available when it shouldn't be, the user types into the editor while a program below is expecting raw keystrokes: dropped input, broken UI, possible destructive action.

When in doubt, raw.

The `Bootstrapping` initial state is stricter still: not even raw keystrokes go through, because the shell isn't yet in a state where any keystroke makes sense. The bootstrap script is reading our injected commands; the user pressing keys during that window would interleave with the bootstrap.

## Promotion is gated. Demotion is eager.

| Direction | Gate |
|---|---|
| → `ShellPromptEditor` | Requires `precmd` lifecycle message; current mode = `RawTerminal`; frame debounce satisfied. |
| ← `ShellPromptEditor` | Any of: Enter submit; window blur in modes where that matters; PTY exit; alternate-screen toggle. Eager — no waiting on lifecycle confirmation. (Ctrl+C is **not** a demote/interrupt trigger here — it's inert at an idle prompt per spec/04; interrupting a running program happens in `RawTerminal`. An explicit Esc-leave demote is **implemented but currently unbound** — `PromptController::leave_editor_esc` / `DemoteReason::Esc` remain, but Esc in the editor is a no-op per spec/04: dropping into raw I/O on an Esc was confusing and solved no problem. The machinery is retained for a future gesture.) |

Demotion can happen "speculatively": for example, the user pressing Enter demotes immediately and the next `preexec` lifecycle message is treated as a confirming event, not a triggering one. If the message never arrives (shell crashed mid-Enter), the pane stays in `RawTerminal` and the command block is annotated as "no command_finished received" — but the pane is correct.

## Orthogonal: command-block lifecycle

The `PaneMode` machine decides **where keystrokes go**. A separate, orthogonal state machine decides **what state a command block is in** as it flows through its lifecycle. Both can advance independently — a pane in `RawTerminal` can have a `BlockState::Running` command block from output that arrived before the user pressed Enter; a pane in `ShellPromptEditor` always has `BlockState::Idle`.

```rust
pub enum BlockState {
    /// Shell is idle at a prompt. Editor (if active) is accepting input.
    Idle,
    /// Termica has written the command to the PTY but hasn't yet seen
    /// the shell-side `preexec` confirmation.
    Submitting,
    /// `preexec` arrived. Command is executing; output bytes are appended
    /// to the block.
    Running,
    /// `command_finished` arrived but the next `precmd` hasn't yet —
    /// transient window between command completion and prompt redraw.
    CommandFinished,
    /// `precmd` arrived. Next block is ready to open.
    PromptReady,
}
```

Transitions:

```text
Idle ──submit_command──► Submitting ──preexec──► Running ──command_finished──► CommandFinished ──precmd──► PromptReady ──► Idle (next block)
```

`BlockState` is tolerant: messages may arrive with surprising timing (preexec before any user-visible output; precmd before the visible prompt bytes are fully flushed). Block-state advancement is monotonic within a block; rolling back is a bug.

For now, only the `PaneMode` machine is implemented; `BlockState` ships when Phase 7 (command blocks) lands.

## `PromptController` shape

```rust
pub struct PromptController {
    pane_id: PaneId,
    mode: PaneMode,
    last_transition: TransitionRecord,    // for debounce + tracing
    integration: IntegrationState,        // version handshake state
    pending_cmd: Option<PendingCommand>,  // open command_run between submit and command_finished
    spawn_frame: u64,                     // for Bootstrapping timeout calculation
}

pub struct TransitionRecord {
    pub from: PaneMode,
    pub to: PaneMode,
    pub reason: TransitionReason,
    pub at: u64,                          // monotonic frame counter; not wall clock
    pub event_seq: Option<u64>,
}

pub enum TransitionReason {
    InitialSpawn,
    BootstrapComplete,
    BootstrapTimeout,
    BootstrapError,
    IntegrationVersionUnsupported,
    PrecmdMarker,
    EnterSubmitted,
    CtrlCEmptyEditor,
    Esc,
    AlternateScreenEnter,
    AlternateScreenExit,
    PtyExit,
    UserRestartedShell,
}
```

The `TransitionRecord` is what lets us debug a bad transition after the fact. We log it via `tracing` and surface the last K transitions in a `--dump-events` output ([03](03-shell-integration.md)).

## What lives in each mode

| Mode | Input goes to | Output rendered? | Mouse | Editor visible? | Status header visible? | Notes |
|---|---|---|---|---|---|---|
| `Bootstrapping` | Dropped | No (buffered) | Spinner cursor | No | "Starting…" indicator | Window: spawn → `integration_ready`, max 3 s |
| `RawTerminal` | PTY (input encoder) | Yes | App selection unless terminal mouse reporting on | No | Yes | The "boring" default |
| `ShellPromptEditor` | `PromptEditor` | Yes | App selection always | Yes | Yes | `❯` glyph painted by Termica |
| `AlternateScreen` | PTY (input encoder) | Yes | Per program (mouse reporting honored) | No | Optional (config; default minimal) | Header is intrusive in fullscreen apps |
| `Dead` | "Restart shell" UI | Frozen | App selection on transcript | No | Yes ("dead" indicator) | History and search still work |
| `Degraded` | PTY (input encoder) | Yes | App selection unless terminal mouse reporting on | No | Yes + banner | "Integration unavailable" banner; otherwise functions as `RawTerminal` |

## Sequencing around `submit()`

The order in [04 — Prompt editor](04-prompt-editor.md) is normative because of mode safety:

1. **Demote to `RawTerminal`** before any PTY write. The editor is closed; further keystrokes route to the PTY.
2. Open command-run record (block state → `Submitting`).
3. Prime echo suppression.
4. Write bytes + newline.
5. Record history.
6. Reset undo + completion.

If step 1 happens after step 4, the user pressing Ctrl-C immediately after Enter will land in a closed editor instead of in the PTY — a corruption-class bug.

## Bootstrap-time guarantees

While in `Bootstrapping`:

- **No PTY output is rendered.** Bytes flow into the OSC sniffer / VT parser (so the bootstrap's DCS-JSON messages are detected) but the visible cell grid is hidden behind a "Starting…" placeholder.
- **No keystrokes are written to the PTY.** Any user input during this window is dropped. The window is short (typically <100 ms; bounded at 3 s).
- **The terminal-side parser still tracks alternate-screen state** so an interleaved alt-screen sequence emitted during bootstrap (unlikely but possible) doesn't desynchronize. The mode does not transition to `AlternateScreen` during `Bootstrapping` — that's a violation; we transition on `integration_ready` first and any alt-screen handling resumes from `RawTerminal`.

After `integration_ready`:

- The buffered output is discarded — it's bootstrap noise the user doesn't need to see. (Future iteration may keep it accessible via `--dump-events` for debugging.)
- The pane reveals: status header, prompt, normal rendering.

## Lifecycle messages without prompt promotion

If Termica's bootstrap is installed and confirmed (`integration_ready` received) but `precmd` never arrives:

- `PromptController` stays in `RawTerminal`. No assumption that "the shell must be at a prompt by now" — that's the heuristic Rule 3 forbids.
- The user has a perfectly normal terminal. They can type commands. Shell-side `preexec` / `command_finished` still fire and update block state for any commands the user runs raw.

If `command_finished` arrives without a prior `preexec`:

- The pane closes any pending block with the reported exit code.
- The "orphan" lifecycle is logged via `tracing` but does not corrupt state.

**Mode-inert lifecycle messages.** Some messages carry information for other consumers but must **never** drive a transition. `prompt_vars` (status header), `shell_vars` (`$VAR` completion source), and `completion` (the [live-shell completion](03-shell-integration.md#completion--live-shell-completion) reply) are all inert: `PromptController::observe_event` handles them as no-ops. In particular a `completion` reply says nothing about whether we're at a prompt — the request is only ever issued while already in `ShellPromptEditor`, and the shell emits the reply and loops straight back to `read` without a `precmd`. Promotion is gated solely on `precmd` and demotion on the eager triggers above; `completion` is in neither set, so it is provably non-transitioning (`completion_reply_is_inert_to_the_mode_machine`).

## Per-rule mapping to tests

Every safety rule must have at least one test that fails before the rule is implemented and passes after. These are the canonical entries; new tests join the same list.

| Rule | Canonical test |
|---|---|
| 1. Prompt editor never active without bootstrap | `prompt_editor_unavailable_without_integration_ready` — drive a synthetic Termica-bootstrap-free byte stream (including foreign OSC 133 noise); assert `PaneMode` is never `ShellPromptEditor`. |
| 2. Alternate screen disables editor | `alt_screen_forces_raw` — drive a `vim`-like byte stream; assert that `precmd` arriving while in `AlternateScreen` does NOT promote. |
| 3. Termica's bootstrap is authoritative | `foreign_osc_133_does_not_promote` — drive `ESC]133;B ESC\` repeatedly in the output without our bootstrap having fired; assert no promotion. |
| 4. Shell never sees editing keystrokes | `editor_keys_do_not_reach_pty` — drive editor input; assert `PtySession::write` is never called except via `submit()`. |
| 5. TTY programs receive raw input | `raw_mode_round_trips_arrow_keys` — in `RawTerminal`, arrow-key egui input produces correct VT escape bytes at the PTY. |
| Bootstrap | `bootstrap_timeout_transitions_to_degraded` — drive a synthetic shell that never emits `integration_ready`; tick the controller past the timeout; assert `PaneMode::Degraded`. |

Each lives under the strict tests-first rule in [CLAUDE.md](../CLAUDE.md).

---

**← Previous:** [04 — Prompt editor](04-prompt-editor.md) | **Next:** [06 — Workspace & tiles](06-workspace-and-tiles.md) →
