**← Previous:** [02 — Terminal engine](02-terminal-engine.md) | **Next:** [04 — Prompt editor](04-prompt-editor.md) →

# 03 — Shell integration

The shell-integration layer is the bridge between an opaque PTY byte stream and Termica's structured-shell behavior. Without integration, Termica is a perfectly good terminal that lacks the editor-driven prompt. With integration, the pane mode machine becomes safe to drive.

## The principle

> The shell emits **lifecycle messages** that tell Termica what it's about to do. Termica never guesses.

If a lifecycle message hasn't arrived, Termica assumes the worst (we're not at a prompt; the editor is unavailable). This is the single biggest correctness win in the design.

A second, equally normative principle:

> **Termica installs the integration. Every time. On every shell start.**

Termica does not require, ask, or rely on the user editing their dotfiles. Termica does not ride along on whatever foreign shell-integration scripts the user happens to have installed from iTerm2 / WezTerm / kitty / Ghostty. Termica's bootstrap is the only source of integration metadata it trusts. The byte stream may carry foreign OSC 133 sequences from other tools' scripts; Termica's parser ignores them.

## The protocol: DCS-JSON

Termica's shell-integration messages travel over **DCS** (Device Control String) escape sequences with a Termica-tagged JSON payload:

```text
ESC P Termica;<json> ESC \
```

- `ESC P` opens DCS.
- `Termica;` is our literal namespace tag (a fixed ASCII prefix).
- `<json>` is a single JSON object describing the event.
- `ESC \` (ST) terminates.

DCS rather than OSC because OSC is heavily contested (OSC 0/2 = title, OSC 4 = palette, OSC 7 = cwd, OSC 8 = hyperlinks, OSC 52 = clipboard, OSC 133 = FinalTerm prompts, OSC 1337 = iTerm2 private, OSC 777 = urxvt). DCS is genuinely device-metadata and we don't conflict with anyone.

### Message schema

Every Termica message is a JSON object with these fields:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `type` | string | yes | The event kind (table below). |
| `session` | string | yes | Termica session ID (UUID-shaped), echoed from `TERMICA_SESSION_ID` so messages from a stale spawned shell can be discriminated. |
| `value` | string / number / object | depends on `type` | Payload. |

Defined `type` values:

| `type` | `value` shape | When the shell emits it |
|---|---|---|
| `integration_ready` | `{"shell":"zsh", "version":1}` | End of bootstrap. The signal that ends `Bootstrapping` mode. |
| `integration_error` | `{"reason":"<short>"}` | Bootstrap detected a problem and chose to fail loud rather than continue. Pane transitions to `Degraded`. |
| `preexec` | command string | About to execute a command. |
| `command_finished` | exit code (integer) | Foreground command returned. |
| `precmd` | cwd path string | About to draw the next prompt. The signal that promotes to `ShellPromptEditor` ([05](05-pane-modes.md)). |
| `cwd` | path string | Optional standalone cwd update outside the normal precmd flow (e.g. on prompt-side updates that don't go through precmd). |
| `prompt_vars` | object | Optional structured prompt metadata (git branch, virtualenv, etc.) for the native status header. |
| `command_aborted` | reason string | User cancelled input before execution (Ctrl-C on empty editor, etc.). |
| `shell_vars` | array of name strings | From the precmd hook (change-gated). The shell's current variable **names** — the source for `$VAR` tab completion. |

`prompt_vars` is intentionally open-ended — the shell sends whatever it can cheaply derive; Termica consumes the keys it knows about (`cwd`, `git_branch`, `git_dirty`, `venv`, etc.) and ignores the rest.

#### `shell_vars` — live `$VAR`-completion source

The precmd hook emits the names of the shell's currently-defined variables (zsh `${(k)parameters}`, bash `compgen -v`, fish `set --names`), filtered to shell-identifier-shaped names and excluding the integration's own `__termica*` internals. This drives `$VAR` tab completion, so it reflects the **live shell** — including **non-exported** parameters (`HISTFILE`, `PS1`, …) and anything `export`ed after spawn — none of which appear in the spawn-time environment snapshot ([`PtySession::env_var_names`](../src/pty.rs)) that completion falls back to before the first report.

Two invariants are normative:

- **Names only, never values.** Values routinely hold secrets (API tokens, AWS credentials); they must never enter the byte stream, Termica's memory, scrollback, or persistence. The payload is an array of bare names.
- **Change-gated.** The hook tracks a signature of the last-emitted name set and emits only when it changes, so a steady prompt loop (the common case) costs nothing beyond building the list. `ShellVars` is inert for the pane-mode machine — a variable-name report never triggers a mode transition.

Consumed by [`PaneSession`](../src/pane.rs): the reported names supersede the spawn snapshot as the completion source ([`PaneSession::env_var_names`](../src/pane.rs)).

### Parser requirements

The DCS parser is hosted in [`crate::markers`](../src/markers.rs) and consumed by [`crate::osc`](../src/osc.rs)'s `vte::Perform` impl (the same parallel-parser scaffolding used for OSC 7). The parser MUST:

- Recognize `ESC P Termica;…ESC \` sequences in the byte stream.
- Remove them from display output. The bytes never reach the cell grid.
- Parse the JSON payload; route to the lifecycle event stream.
- Tolerate partial reads / chunk boundaries: `vte::Parser` already buffers DCS across `advance()` calls.
- Tolerate malformed JSON without panicking: drop the bad message, log via `tracing`, continue.
- Preserve unrelated escape sequences for the renderer.

**Strict rule:** no code anywhere in Termica scans the raw byte stream for marker patterns. The DCS framing and the JSON parsing are done by the same VT parser pipeline that drives the terminal grid.

### Lifecycle event stream

```rust
pub enum LifecycleEvent {
    IntegrationReady { shell: ShellKind, version: u32 },
    IntegrationError { reason: String },
    Preexec { command: String },
    CommandFinished { exit: i32 },
    Precmd { cwd: PathBuf },
    Cwd { cwd: PathBuf },
    PromptVars { vars: serde_json::Map<String, serde_json::Value> },
    CommandAborted { reason: String },
}

pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Unknown,
}
```

Order is preserved per-pane. The lifecycle event stream is the only input the `PromptController` ([05](05-pane-modes.md)) consumes besides PTY exit notifications.

### What about OSC 133?

Foreign OSC 133 sequences may appear in the byte stream if the user has iTerm2 / WezTerm / kitty / Ghostty integration scripts loaded by their `.zshrc`. Termica's parser **ignores them**.

Reason: shell-integration safety depends on the integration script being one Termica controls. Foreign OSC 133 could be emitted by anything — a buggy script, a typo in a dotfile, a `cat` of a log file containing escape sequences. Trusting them would mean the prompt editor's safety depends on what a stranger's `.zshrc` happens to emit. CLAUDE.md's "Shell integration is the only source of truth for 'are we at a prompt?'" rule forbids the heuristic.

Termica's own bootstrap (described below) runs on every shell start. The DCS-JSON event stream it produces is the only one we trust.

### What about OSC 7?

OSC 7 (cwd reporting) is **kept** as a parallel signal, used only by the status header and clickable paths. It does not feed the `PromptController` and never participates in mode transitions. It exists because pre-bootstrap (during `Bootstrapping` mode) we still want a cwd snapshot in case the user has system-wide OSC 7 emission, and because the existing zsh OSC 7 hook is harmless.

## The bootstrap

Termica controls integration via **managed shell startup**: the shell is launched with automatic rc-file loading disabled (or replaced), and Termica's bootstrap runs the user's real configuration manually in a known order, then installs Termica's lifecycle hooks.

The mechanism is shell-specific because the available CLI surface differs.

### Environment variables set by Termica before spawn

```sh
TERM_PROGRAM=Termica
TERMICA_SESSION_ID=<uuid>
TERMICA_INTEGRATION_VERSION=1
TERMICA_SHELL_INTEGRATION=1
TERMICA_INITIAL_WORKING_DIR=<cwd>
```

Optional:

```sh
TERMICA_NO_SHELL_INTEGRATION=1   # opt-out; bootstrap is skipped entirely
```

Env vars are how the **integration script** detects it's running inside Termica and selects behaviour. They cannot themselves install hooks — only shell code does that.

### Env vars set by the bootstrap (not exported)

The bootstrap also sets a local flag inside the spawned shell to guard against double-bootstrap of *that shell*:

- `TERMICA_BOOTSTRAPPED=1` — set as a shell-local variable, **not exported**.

This is **deliberately non-exported**. Two scenarios drive the choice:

1. **Nested Termica.** A user running `cargo run` (or any other process that itself spawns a managed Termica) from inside an already-bootstrapped Termica shell must get a fresh, fully-bootstrapped inner Termica. If `TERMICA_BOOTSTRAPPED` propagated to child processes, the inner Termica's spawned shell would see the flag, hit the bootstrap's double-bootstrap guard, return early, and skip integration entirely. The result: the inner Termica looks like a degraded terminal even though everything is configured correctly.
2. **The flag's actual meaning.** `TERMICA_BOOTSTRAPPED` answers "did *this* shell process run our bootstrap?" — a per-shell-instance fact. It is not a "we're inside Termica" flag; `TERM_PROGRAM=Termica` already serves that purpose and is correctly inherited by children.

A reader inspecting `TERMICA_BOOTSTRAPPED=1` and finding it true can therefore trust that the current shell has Termica's hooks installed. A reader looking for "am I in Termica?" should consult `TERM_PROGRAM`.

### zsh — managed startup via ZDOTDIR-with-immediate-unset

zsh has no `--init-file <path>` flag. Two realistic local bootstrap mechanisms exist:

1. A startup-file wrapper such as `$ZDOTDIR`. **Chosen.**
2. A hidden first-command bootstrap via PTY-write. **Rejected** after empirical testing: zsh's Zsh Line Editor (ZLE) intercepts PTY-stdin bytes as keystrokes once the shell is interactive, so a multi-line bootstrap written to the PTY ends up character-shuffled through line editing instead of executed as commands.

We do use ZDOTDIR, but we mitigate the "visible env-var mutation" concern by **unsetting ZDOTDIR immediately** at the top of our wrapper `.zshrc`, before any user code runs. The mutation window is the duration of zsh's own startup-file phase, during which only Termica-controlled code executes.

Procedure:

```text
1. Materialise a wrapper directory under Termica's data dir
   (e.g. ~/Library/Application Support/Termica/zsh-wrapper/).
   Write Termica's bootstrap as `.zshrc` and an empty `.zshenv`.

2. Spawn `zsh -i` with `ZDOTDIR=<wrapper-dir>` in the child env.
   The pane enters Bootstrapping (spec/05): display suppressed,
   user keystrokes dropped.

3. zsh follows its normal startup:
     - Reads /etc/zshenv (system-wide; out of v1 scope, see below).
     - Reads $ZDOTDIR/.zshenv → our empty file → no-op.
     - For interactive: reads $ZDOTDIR/.zshrc → our wrapper.

4. Our wrapper .zshrc:
     a. Sets a TERMICA_BOOTSTRAPPED guard against double-load.
     b. Immediately `unset ZDOTDIR` so user code never sees the
        mutation.
     c. Defines Termica helpers (DCS-JSON emit, JSON escape).
     d. Sources `~/.zshenv` if it exists (zsh would normally have
        done this but it sourced our empty wrapper .zshenv instead).
     e. Re-evaluates effective ZDOTDIR — the user's .zshenv may have
        set it. From here on, user config lives at
        `${ZDOTDIR:-$HOME}/.zshrc`.
     f. Sources `$ZDOTDIR/.zshrc` if it exists (similarly skipped by
        zsh because of our ZDOTDIR override).
     g. Defines preexec / precmd hook functions and calls
        `termica_ensure_hooks` — idempotent reassertion after any
        prompt framework (oh-my-zsh, p10k, etc.) has had its say.
     h. Emits `integration_ready` over DCS-JSON.

5. The pane transitions out of Bootstrapping. The first real prompt
   is now drawn and the pane is in RawTerminal.
```

The bootstrap is a regular sourced file — comments work, multi-line constructs work, no shell tricks needed. ZLE is not involved (we are not writing to the PTY's stdin).

`.zshenv` semantics are preserved by this flow:

- Our managed interactive shell sources `~/.zshenv` (step 4b), matching what vanilla interactive zsh would do.
- Non-interactive children of our managed shell (`zsh -c '…'` from a tool or agent) source `~/.zshenv` automatically via zsh's own startup. We do not touch this; zsh handles it.
- `.zshrc` is sourced only for our interactive shell. Agent / tool children get `.zshenv` but not `.zshrc`, exactly as on a stock zsh install.

**System files.** `/etc/zshenv` and `/etc/zshrc` are NOT sourced by the bootstrap in v1. They are normally read by zsh as part of startup but `--no_rcs` skips them. On macOS, `/etc/zshrc` runs `path_helper` and sets some default behaviour; this is a known gap for v1 and tracked in [10](10-roadmap.md). Users who need them can opt into a later compatibility flag. The most common reason `/etc/zshrc` matters — Apple's `TERM_PROGRAM=Apple_Terminal` gating — is irrelevant to us because we set `TERM_PROGRAM=Termica`.

**Login shell.** v1 emulates interactive **non-login** zsh. Login-shell emulation (sourcing `.zprofile`, `.zlogin`, `.zlogout`) is a later compatibility mode.

### bash — managed startup via `--rcfile`

bash provides `--rcfile <path>`, which **replaces** `~/.bashrc` for that shell. We exploit this directly: generate a wrapper rcfile that sources Termica helpers, then sources the user's real `~/.bashrc`, then reasserts hooks.

```text
1. Generate a wrapper rcfile under Termica's data directory (per `directories`
   crate: `~/Library/Application Support/Termica/` on macOS, `$XDG_DATA_HOME/
   termica/` on Linux). The file content is deterministic and idempotent.

2. Spawn `bash --noprofile --norc --rcfile <wrapper> -i`.

3. Bash reads the wrapper at startup as if it were `~/.bashrc`. The wrapper:
     a. Defines Termica helpers.
     b. Includes the vendored bash-preexec compatibility layer (handles
        DEBUG trap chaining, array-valued PROMPT_COMMAND, and the "no
        preexec for the integration's own commands" filter).
     c. Sources the user's real `~/.bashrc` if it exists.
     d. Calls `termica_ensure_hooks` — reasserts `PROMPT_COMMAND` and
        the `precmd` / `preexec` arrays after any user-side mutation.
     e. Emits `integration_ready`.

4. The wrapper runs as part of normal bash startup. No PTY-write needed.
   The Bootstrapping pane-mode window is near-zero (the time between
   bash starting and the wrapper finishing).
```

The wrapper rcfile is regenerated on every Termica launch (Termica owns the path; users don't edit it). The wrapper does not modify `~/.bashrc`.

**bash-preexec.** We vendor [`bash-preexec.sh`](https://github.com/rcaloras/bash-preexec) (MIT-licensed, ~150 lines) as an `include_str!` constant. Reimplementing the DEBUG-trap recursion / command-substitution suppression logic is a known tar pit; vendoring is cleaner and the license is permissive.

**Bash `/etc/bashrc`.** Same TBD as zsh's `/etc/zshrc` — not sourced in v1.

### fish — managed startup with `--init-command`

fish provides `--init-command 'shell code'` (`-C` short form) which runs before the first prompt. The mechanism is clean:

```text
1. Spawn `fish --no-config --init-command "$BOOTSTRAP" -i`.

2. fish runs the init command, which:
     a. Defines Termica helpers (fish syntax, not POSIX).
     b. Sources `~/.config/fish/config.fish` if it exists.
     c. Sources each `~/.config/fish/conf.d/*.fish` in the order fish
        would have loaded them.
     d. Defines event handlers via `function … --on-event fish_preexec`
        (and `fish_postexec`, `fish_prompt`).
     e. Emits `integration_ready`.

3. As with bash, the Bootstrapping window is near-zero.
```

fish's event system is first-class; no DEBUG-trap acrobatics. The only fish-specific worry is JSON-escaping in fish syntax (different quoting rules), which gets its own helper.

### Failure modes and timeouts

The `Bootstrapping` window has a fixed timeout: **3 seconds**.

If `integration_ready` does not arrive within 3 seconds of shell spawn:

1. The pane transitions from `Bootstrapping` → `Degraded` ([05](05-pane-modes.md)).
2. The display-suppression window ends. Whatever the shell printed during bootstrap is revealed to the user (so they can diagnose the failure).
3. The `PromptController` stays in `RawTerminal` forever for that pane.
4. A non-intrusive banner appears: "Termica integration unavailable; running as raw terminal."
5. `restart_shell` clears `Degraded` and tries again.

`integration_error` (emitted by the bootstrap itself on a detected problem) transitions to `Degraded` immediately, with the error reason in the banner.

`TERMICA_NO_SHELL_INTEGRATION=1` skips the bootstrap entirely: the shell launches with normal rc loading (no `--no_rcs` / `--norc` / `--no-config`), the pane goes straight to `RawTerminal`, and integration is permanently unavailable for that pane. Provided as an escape hatch for debugging or for users whose rc files are too unusual to manage.

### Naming and namespacing

All shell functions, variables, and DCS payload markers use the `termica` namespace:

```sh
TERMICA_SESSION_ID
TERMICA_INTEGRATION_VERSION
TERMICA_SHELL_INTEGRATION
termica_preexec
termica_precmd
termica_postexec
termica_send_json_message
termica_escape_json
termica_ensure_hooks
```

Helpers are exported only where they need to be visible to subshells. Internal helpers stay unexported to keep the namespace inside spawned `zsh -c '…'` children clean.

## What the scripts deliberately don't do

- **No PS1 art / fancy prompts.** The visible prompt belongs to Termica's status header ([06](06-workspace-and-tiles.md)).
- **No completion forwarding.** Tab completion in `ShellPromptEditor` is local in v1 ([04](04-prompt-editor.md)).
- **No history capture from the shell.** Termica records its own history at submit time ([07](07-history-and-search.md)).
- **No dotfile mutation.** Ever. The bootstrap mechanisms above are all in-memory or under Termica's data directory.
- **No background telemetry, no probe loops.** The shell does the minimum required to make the protocol work, and nothing else.

## Version negotiation

`integration_ready` carries a `version` field. The bootstrap script's version is the integration-script protocol version, not the Termica app version.

Forward compatibility rules:

- If `version` is unknown (too new for this Termica build): pane goes to `Degraded`; banner suggests upgrading Termica.
- If `version` is too old: pane goes to `Degraded`; banner suggests reinstalling Termica (or running an upgrade subcommand once one exists).
- If `version` is supported but minor: full functionality, possibly with a debounced "newer integration available" hint.

The current version is `1`. It bumps on any normative change to script behaviour.

## Native editor flow

Termica's editor owns local command editing in `ShellPromptEditor` mode. The flow on Enter:

```text
1. Termica finalizes the editor buffer.
2. Termica creates a pending command block (PromptController.submit_command,
   demoting to RawTerminal first per spec/05).
3. Termica writes the command text + newline to the PTY.
4. The shell's `preexec` hook fires, emitting `preexec` over DCS-JSON.
5. Output bytes are appended to the block.
6. The shell's `precmd` hook fires after the command returns, emitting
   `command_finished` then `precmd` over DCS-JSON.
7. PromptController closes the pending command, promotes back to
   ShellPromptEditor on the next precmd.
```

Termica knows when Enter was pressed and demotes the editor eagerly (spec/05). It still **waits for shell lifecycle confirmation** before treating the shell as idle again. This handles:

- Rejected / incomplete multiline commands.
- Syntax errors at the shell level.
- Shell-side command rewriting (aliases, functions, shell widgets).
- Pasted commands containing newlines.

## Multiline commands

Termica submits the entire editor buffer as one logical submission. The shell may not execute until the command is syntactically complete:

```sh
if true; then
  echo hello
fi
```

Expected behaviour:

- One command block.
- One execution event from the shell.
- One `command_finished`.

If the shell decides the input is incomplete (e.g. unclosed `if`), it emits a `command_aborted` and the block closes with no exit code. The next prompt re-opens normally.

## Exit status

The shell-side hook MUST capture `$?` immediately at the top of the precmd hook, before any helper command:

```zsh
termica_precmd() {
    local exit_status=$?
    termica_send_json_message "command_finished" "$exit_status"
    # … further work after this is safe
}
```

Same pattern in bash and fish. A test runs `true; false; sh -c 'exit 42'` and asserts the shell reports `0`, `1`, `42` in order.

## Subshells, SSH, containers

v1 scope: local zsh / bash / fish only. PTY-injection into nested shells is **not** v1.

Behaviour when the user runs a recognised shell command (`zsh`, `bash`, `fish`) inside an already-managed Termica session:

- The nested shell is detected by the absence of `TERMICA_BOOTSTRAPPED=1` (set by the outer shell's bootstrap).
- The nested shell does not get Termica integration. Its prompts won't promote.
- The pane mode-machine handles this naturally: nested shell prompts produce no DCS-JSON, so no promotion occurs; the user has a working but un-integrated nested shell.
- Returning to the outer shell (via `exit`) restores integration because the outer shell's hooks were never affected.

Post-MVP: detect nested-shell entry, send a fresh bootstrap into the nested shell over the PTY. Same hidden-bootstrap-state mechanism, just applied mid-session.

## Debug surface

Setting `TERMICA_DUMP_EVENTS=<path>` in the environment before launching Termica turns on the lifecycle-event recorder (Phase 3G). The named file is opened with create-or-truncate semantics; every pane writes its spawn, lifecycle events, mode transitions, PTY-exit, and tab-**completion** events to the same file in arrival order. Timestamps are seconds since the recorder started, so a recording is comparable to itself regardless of when it ran.

**Format selection is by file extension:**

- `<path>.json` or `<path>.jsonl` → **JSON Lines** (one JSON object per line). Each record has a `t` (float seconds), `pane` (integer), `kind` (string discriminator) envelope plus per-`kind` fields. Trivially deserializable in tests and tools; `jq` works out of the box.
- Any other extension → **human-readable text** (the example below).

The format is fixed at recorder construction; a single Termica process writes one format for the life of the recording.

### JSON Lines schema

```jsonc
{"t":0.012,"pane":0,"kind":"spawn","shell":"zsh","argv":["zsh","-i"]}
{"t":0.012,"pane":0,"kind":"transition","from":"Bootstrapping","to":"Bootstrapping","reason":"InitialSpawn"}
{"t":0.187,"pane":0,"kind":"lifecycle","event":"IntegrationReady","shell":"Zsh","version":1}
{"t":0.188,"pane":0,"kind":"transition","from":"Bootstrapping","to":"RawTerminal","reason":"BootstrapComplete"}
{"t":0.512,"pane":0,"kind":"lifecycle","event":"Precmd","cwd":"/Users/tim"}
{"t":3.401,"pane":0,"kind":"lifecycle","event":"Preexec","command":"ls -la"}
{"t":3.452,"pane":0,"kind":"lifecycle","event":"CommandFinished","exit":0}
{"t":4.700,"pane":0,"kind":"completion","event":"plan","decision":"AwaitDriver","tool":"Kubectl","line":"kubectl get ","locals":0,"from_tab":true}
{"t":4.701,"pane":0,"kind":"completion","event":"driver_request","tool":"Kubectl","line":"kubectl get ","cache_hit":false}
{"t":4.760,"pane":0,"kind":"completion","event":"driver_result","tool":"Kubectl","candidates":17,"cache_hit":false}
{"t":4.760,"pane":0,"kind":"completion","event":"popup","action":"Open","candidates":17}
{"t":4.812,"pane":0,"kind":"pty_exit"}
```

Per-`kind` fields:

| `kind` | extra fields |
|---|---|
| `spawn` | `shell` (string), `argv` (array of strings) |
| `transition` | `from`, `to` (mode names), `reason` (reason name) |
| `lifecycle` | `event` (variant name) + variant-specific fields: `shell` + `version` for `IntegrationReady`; `command` for `Preexec`; `exit` for `CommandFinished`; `cwd` for `Precmd` / `Cwd`; `reason` for `IntegrationError` / `CommandAborted`; `vars` (object) for `PromptVars` |
| `pty_exit` | none |
| `completion` | `event` discriminator + per-event fields ([04a §"Source 1"](04a-completion.md)): **`plan`** → `decision` (`Open`/`AwaitDriver`/`Closed`), `tool` (driver name or `null`), `line`, `locals`, `from_tab`; **`driver_request`** → `tool`, `line`, `cache_hit`; **`driver_result`** → `tool`, `candidates`, `cache_hit` (`candidates":0` ⇒ absent tool / timeout / no match); **`popup`** → `action` (`Open`/`Accept`/`AutoAccept`/`Dismiss`), `candidates`. A dead `<Tab>` is legible end-to-end: e.g. a `plan decision=AwaitDriver` followed by `driver_result candidates=0` and **no** `popup` line means the driver subprocess returned nothing (timeout / absent tool). |

### Human-readable example

```text
[t=0.012s] pane=0 spawn shell=zsh argv=["zsh", "-i"]
[t=0.012s] pane=0 transition Bootstrapping → Bootstrapping (InitialSpawn)
[t=0.187s] pane=0 lifecycle IntegrationReady { shell: Zsh, version: 1 }
[t=0.188s] pane=0 transition Bootstrapping → RawTerminal (BootstrapComplete)
[t=0.512s] pane=0 lifecycle Precmd { cwd: "/Users/tim" }
[t=0.512s] pane=0 transition RawTerminal → ShellPromptEditor (PrecmdMarker)
[t=3.401s] pane=0 lifecycle Preexec { command: "ls -la" }
[t=3.452s] pane=0 lifecycle CommandFinished { exit: 0 }
[t=3.453s] pane=0 lifecycle Precmd { cwd: "/Users/tim" }
[t=4.700s] pane=0 completion plan decision=AwaitDriver tool=Kubectl line="kubectl get " locals=0 from_tab=true
[t=4.701s] pane=0 completion driver_request tool=Kubectl line="kubectl get " cache_hit=false
[t=4.760s] pane=0 completion driver_result tool=Kubectl candidates=17 cache_hit=false
[t=4.760s] pane=0 completion popup action=Open candidates=17
```

This is the diagnostic surface for debugging integration failures and is the primary tool for the test infrastructure ([09](09-testing.md)). Each line is a single record; `tail -f $TERMICA_DUMP_EVENTS` while reproducing a bug gives a real-time view of the state machine. Opening the file is best-effort: if the path is invalid or unwritable, Termica reports on stderr at startup and disables the recorder for the session.

## Testing

- **Unit tests** ([`src/markers.rs`](../src/markers.rs)): DCS-JSON parser; every defined `type` value; malformed payloads; unknown `type` ignored; split-read robustness.
- **Bootstrap script unit tests** ([`integration/*/tests/`](../integration/)): each shell's bootstrap script run in isolation with a captured "PTY" (pipe); assert the DCS-JSON sequence emitted matches the expected lifecycle for a scripted sequence of inputs.
- **Integration tests** ([`tests/`](../tests/)): spawn real `zsh -g --no_rcs`, `bash --rcfile …`, `fish --no-config …`; run a scripted command sequence; assert the lifecycle event stream is exactly what we expect.
- **Lifecycle fixtures** ([`testdata/lifecycle/<shell>/<scenario>.bytes`](../testdata/lifecycle/)): recorded byte streams from real shells with our bootstrap loaded; replayed against the parser; expected `LifecycleEvent` sequence asserted.

---

**← Previous:** [02 — Terminal engine](02-terminal-engine.md) | **Next:** [04 — Prompt editor](04-prompt-editor.md) →
