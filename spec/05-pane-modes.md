**← Previous:** [04 — Prompt editor](04-prompt-editor.md) | **Next:** [06 — Workspace & tiles](06-workspace-and-tiles.md) →

# 05 — Pane modes

This is the load-bearing safety invariant of the entire product. Read it twice.

## The five safety and correctness rules (normative)

1. **The prompt editor is NEVER active unless the pane is at a trusted, marker-confirmed shell prompt.**
2. **Alternate screen always disables the prompt editor.** No exceptions.
3. **Shell integration markers are authoritative.** Heuristics may enhance but must not be required for correctness.
4. **The shell never sees editing keystrokes from the editor.** Only the final command bytes plus a newline.
5. **Real TTY programs receive raw input.** Encoded promptly, never coalesced.

A code change that violates one of these is a P0 bug, not a design preference. The hybrid testing rule in [CLAUDE.md](../CLAUDE.md) applies in its **strict** form to this entire document's surface area.

## The four modes

```rust
pub enum PaneMode {
    /// Default. Keystrokes go to the PTY; output renders normally;
    /// no editor surface; mouse selects the transcript.
    RawTerminal,

    /// The pane is at a marker-confirmed shell prompt. The egui editor
    /// owns input; only `submit()` produces PTY bytes.
    ShellPromptEditor,

    /// Terminal has entered the alternate screen (vim/htop/less/fzf/tmux).
    /// Strictest raw mode; mouse and bracketed-paste honor the program's
    /// reporting state.
    AlternateScreen,

    /// PTY child has exited. Transcript remains; "restart shell" UI is
    /// visible; history and search remain accessible.
    Dead,
}
```

There are exactly four. We will not add a fifth without an explicit spec update; new sub-states tend to be the answer to bug pressure that should have been fixed structurally.

## Transition diagram

```
                         ┌───────────────────────────────────┐
                         │                                   │
   spawn ──► [RawTerminal] ──────marker: prompt_end────► [ShellPromptEditor]
                  │   ▲              ◄────────────────       │
                  │   │              submit() / Ctrl-C       │
                  │   │              / Esc / mode reset      │
                  │   │                                      │
                  │   └──exit alt screen──┐                  │
                  ▼                       │                  ▼
        ┌─►[AlternateScreen]◄─────────────┴────enter alt screen◄┘
        │         │
        │  pty exit │
        │         ▼
        └────► [Dead]
```

Walk it carefully:

- **Start** in `RawTerminal`.
- **`prompt_end` (OSC 133 B) → `ShellPromptEditor`**, only when:
  - the pane is currently in `RawTerminal` (never `AlternateScreen`, never `Dead`);
  - the version handshake (`TermicaVersion=N`) has been confirmed at least once and `N` is supported;
  - the most recent transition out of `ShellPromptEditor` was at least one frame ago (debounce against the same prompt round-tripping).
- **`Enter` submit → `RawTerminal`**, eagerly, before the PTY write happens ([04](04-prompt-editor.md)).
- **Alternate-screen ON → `AlternateScreen`** from any mode except `Dead`.
- **Alternate-screen OFF → `RawTerminal`** (not directly to `ShellPromptEditor`; only a fresh marker promotes).
- **PTY exit → `Dead`** from any mode.
- **User restarts shell from `Dead` → `RawTerminal`** with a fresh `PtySession`.

## Why the default is `RawTerminal`

Because the cost of being wrong is asymmetric. If the editor is unavailable when it could have been, the user types into the shell as in any other terminal: zero data loss, zero corruption, mild ergonomic loss. If the editor is available when it shouldn't be, the user types into the editor while a program below is expecting raw keystrokes: dropped input, broken UI, possible destructive action.

When in doubt, raw.

## Promotion is gated. Demotion is eager.

| Direction | Gate |
|---|---|
| → `ShellPromptEditor` | Requires the explicit confirmed prompt_end marker, version handshake completed, current mode = `RawTerminal`, frame debounce satisfied. |
| ← `ShellPromptEditor` | Any of: Enter submit; Ctrl-C on empty editor (sends interrupt); window blur in modes where that matters; pty exit; alternate-screen toggle; explicit Esc-leave (config). Eager — no waiting on marker confirmation. |

Demotion can happen "speculatively": for example, the user pressing Enter demotes immediately and the next `command_start` marker is treated as a confirming event, not a triggering one. If the marker never arrives (shell crashed mid-Enter), the pane stays in `RawTerminal` and the command block is annotated as "no command_end received" — but the pane is correct.

## `PromptController` shape

```rust
pub struct PromptController {
    pane_id: PaneId,
    mode: PaneMode,
    last_transition: TransitionRecord,    // for debounce + tracing
    integration: IntegrationState,        // version handshake state
    pending_cmd: Option<PendingCommand>,  // open command_run between submit and command_end
}

pub struct TransitionRecord {
    pub from: PaneMode,
    pub to: PaneMode,
    pub reason: TransitionReason,
    pub at: FrameOrTick,                  // monotonic; not wall clock
    pub marker_seq: Option<u64>,
}

pub enum TransitionReason {
    InitialSpawn,
    MarkerPromptEnd,
    EnterSubmitted,
    CtrlCEmptyEditor,
    AlternateScreenEnter,
    AlternateScreenExit,
    PtyExit,
    UserRestartedShell,
    IntegrationVersionUnsupported,
    Esc,
}
```

The `TransitionRecord` is what lets us debug a bad transition after the fact. We log it via `tracing` and surface the last K transitions in a `--dump-events` output ([01](01-architecture.md)).

## What lives in each mode

| Mode | Input goes to | Mouse | Editor visible? | Status header visible? | Notes |
|---|---|---|---|---|---|
| `RawTerminal` | PTY (input encoder) | App selection unless terminal mouse reporting on | No | Yes | The "boring" default |
| `ShellPromptEditor` | `PromptEditor` | App selection always | Yes | Yes | `❯` glyph painted by Termica |
| `AlternateScreen` | PTY (input encoder) | Per program (mouse reporting honored) | No | Optional (config; default minimal) | Header is intrusive in fullscreen apps; minimal or hidden by default |
| `Dead` | "Restart shell" UI | App selection on transcript | No | Yes ("dead" indicator) | History and search still work |

## Sequencing around `submit()`

The order in [04 — Prompt editor](04-prompt-editor.md) is normative because of mode safety:

1. **Demote to `RawTerminal`** before any PTY write. The editor is closed; further keystrokes route to the PTY.
2. Open command-run record.
3. Prime echo suppression.
4. Write bytes + newline.
5. Record history.
6. Reset undo + completion.

If step 1 happens after step 4, the user pressing Ctrl-C immediately after Enter will land in a closed editor instead of in the PTY — a corruption-class bug.

## Markers without a prompt: graceful degradation

If shell integration is not installed:

- No `prompt_end` ever fires.
- `PromptController` stays in `RawTerminal` forever.
- Status header degrades to local probes only (cwd inferred from process info; git status from a debounced probe in the current cwd).
- The user has a perfectly normal terminal.

If shell integration is installed but the version is unsupported:

- `TermicaVersion=N` fires once.
- `PromptController` transitions to `RawTerminal` permanently (`IntegrationVersionUnsupported`) and surfaces a banner: "Termica integration version N is newer than this build supports — upgrade Termica or `termica install-integration` to write a compatible script."

## Markers without command_end

If `prompt_end` → `Enter` → no `command_end` ever fires (the shell crashed, `exec`d into something else, or never returned):

- The pane stays in `RawTerminal`.
- The open command_run record is closed with `exit = Unknown` and `duration = until_now`.
- The next `prompt_end` (if any) promotes normally.

We never guess our way back to `ShellPromptEditor`.

## Per-rule mapping to tests

Every safety rule must have at least one test that fails before the rule is implemented and passes after. These are the canonical entries; new tests join the same list.

| Rule | Canonical test |
|---|---|
| 1. Prompt editor never active without marker | `prompt_editor_unavailable_without_integration` — drive a synthetic marker-free byte stream; assert `PaneMode == RawTerminal` throughout. |
| 2. Alternate screen disables editor | `alt_screen_forces_raw` — drive a `vim`-like byte stream; assert that `prompt_end` arriving while in `AlternateScreen` does NOT promote. |
| 3. Markers are authoritative | `heuristic_prompt_does_not_promote` — drive `$ ` repeatedly in the output; assert no promotion. |
| 4. Shell never sees editing keystrokes | `editor_keys_do_not_reach_pty` — drive editor input; assert `PtySession::write` is never called except via `submit()`. |
| 5. TTY programs receive raw input | `raw_mode_round_trips_arrow_keys` — in `RawTerminal`, arrow-key egui input produces correct VT escape bytes at the PTY. |

Each lives under the strict tests-first rule in [CLAUDE.md](../CLAUDE.md).

---

**← Previous:** [04 — Prompt editor](04-prompt-editor.md) | **Next:** [06 — Workspace & tiles](06-workspace-and-tiles.md) →
