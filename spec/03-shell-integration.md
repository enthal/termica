**← Previous:** [02 — Terminal engine](02-terminal-engine.md) | **Next:** [04 — Prompt editor](04-prompt-editor.md) →

# 03 — Shell integration

The shell-integration layer is the bridge between an opaque PTY byte stream and Termica's structured-shell behavior. Without integration, Termica is a perfectly good terminal that lacks the editor-driven prompt. With integration, the pane mode machine becomes safe to drive.

## The principle

> The shell emits **markers** that tell Termica what it's about to do. Termica never guesses.

If a marker hasn't arrived, Termica assumes the worst (we're not at a prompt; the editor is unavailable). This is the single biggest correctness win in the design.

## Marker protocol

Termica consumes two namespaces, in priority order:

1. **OSC 133** — the de-facto-standard FinalTerm / iTerm2 / WezTerm / kitty prompt markers. Many users already have these in their shell rc from prior terminals. We get them for free.
2. **OSC 1337 `Termica=…`** — Termica-private extensions for things OSC 133 doesn't cover (typed identifiers, version negotiation, command-id correlation).

### OSC 133 — the standard set

| Marker | Meaning | Emitted at |
|---|---|---|
| `OSC 133 ; A ST` | Prompt start | First byte of the prompt about to be drawn |
| `OSC 133 ; B ST` | Prompt end (= command edit begin) | Just before the shell reads the user's command |
| `OSC 133 ; C ST` | Command start (= shell about to run) | Right after the user presses Enter, before the command runs |
| `OSC 133 ; D ; <exit> ST` | Command end | After the command returns; `<exit>` is the integer exit status |

These are passed through the terminal layer to the marker stream and consumed; the bytes never appear on screen.

### OSC 1337 — Termica extensions

We follow the iTerm2-private OSC 1337 convention: `OSC 1337 ; key1=value1 ; key2=value2 ST`. Keys that begin with `Termica` are ours. We never collide with iTerm2 keys.

| Sequence | Meaning |
|---|---|
| `OSC 1337 ; TermicaVersion=1 ST` | Integration script protocol version. Required on first prompt; lets us refuse unknown versions cleanly. |
| `OSC 1337 ; TermicaShell=zsh ST` | Self-reported shell kind (`bash` / `zsh`). |
| `OSC 1337 ; TermicaCwd=<file-uri> ST` | Current working directory as a `file://` URL. Spaces/Unicode are URI-encoded. |
| `OSC 1337 ; TermicaCmdId=<uuid> ST` | Termica-assigned command id, echoed back to correlate command_start ↔ command_end across interleaved background output. |
| `OSC 1337 ; TermicaDuration=<ms> ST` | Optional, emitted alongside `D` if the shell can self-measure. |

`<file-uri>` matches RFC 8089 (e.g., `file:///Users/tim/git/termica`).

### Why two namespaces, not three

We considered inventing a Termica-private OSC number. We rejected it for two reasons:

1. **OSC 777 is taken** (urxvt / `dunstify` / notification daemons). We will not collide.
2. **OSC 133 already exists** and is widely deployed; consuming it gives users a working prompt editor even when they haven't installed Termica's integration yet, because they probably ran iTerm2 or WezTerm before.

By layering Termica extensions on top of the iTerm2-namespaced OSC 1337, we ride the convention without inventing one.

### Split-read robustness

OSC sequences can be split across PTY reads. Marker parsing happens inside `alacritty_terminal`'s VT parser, which already handles partial sequences correctly. Termica's marker consumer subscribes to fully-parsed OSC events, never to raw bytes.

This is normative: **any code that watches the raw byte stream for marker patterns is a bug**. The only correct place to recognize a marker is after the VT parser has assembled it.

### Marker stream

```rust
pub enum MarkerEvent {
    PromptStart,
    PromptEnd,
    CommandStart { cmd_id: Option<CmdId> },
    CommandEnd { cmd_id: Option<CmdId>, exit: i32, duration_ms: Option<u64> },
    Cwd(PathBuf),
    ShellAnnounce { kind: ShellKind, version: u32 },
}

pub struct MarkerStream { /* mpsc-style rx */ }
```

Order is preserved per-pane. The marker stream is the only input the `PromptController` ([05](05-pane-modes.md)) consumes besides PTY exit notifications.

## Shell integration scripts

Termica ships two scripts. They are bundled as compile-time strings (`include_str!`) so the binary is self-contained and `termica install-integration` can always write the current version.

### `bash` integration (`termica-integration.bash`)

```bash
# Termica integration, version 1.
# Loaded by ~/.bashrc inside fences written by `termica install-integration`.

# Bail if not running under Termica or already loaded.
[[ -n "${TERMICA:-}" ]] || return 0
[[ -n "${__TERMICA_LOADED:-}" ]] && return 0
__TERMICA_LOADED=1

__termica_osc() { printf '\033]%s\033\\' "$1"; }
__termica_uri() { python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1], safe="/"))' "$1"; }

__termica_prompt_start() {
    __termica_osc "133;A"
    __termica_osc "1337;TermicaVersion=1"
    __termica_osc "1337;TermicaShell=bash"
    __termica_osc "1337;TermicaCwd=file://$(hostname)$(__termica_uri "$PWD")"
}
__termica_prompt_end()   { __termica_osc "133;B"; }
__termica_cmd_end()      {
    local exit=$?
    __termica_osc "133;D;${exit}"
    return "$exit"
}
__termica_cmd_start()    { __termica_osc "133;C"; }

# Bash: PROMPT_COMMAND runs after every command, PS0 prints just before
# the command runs.
PS0='$(__termica_cmd_start)'
PROMPT_COMMAND='__termica_cmd_end; __termica_prompt_start'

# Replace PS1 with a *minimal* marker-only prompt so the visible prompt
# is owned by Termica's UI. The trailing `\$ ` is a fallback for when
# Termica is not driving the UI (e.g. ssh from another terminal).
PS1='\[$(__termica_prompt_end)\]\$ '
```

Notes:

- The script is bash-only by feature use (`PROMPT_COMMAND`, `PS0`). Fish and other shells are out of scope for v1.
- The `python3` URL-quoter is a fallback; we will replace it with pure shell quoting in the real script.
- Hostname is included in the file URI for ssh-aware future use; the local resolver ignores it for v1.
- The `\$ ` fallback means: if a user runs `bash` inside Termica without integration, or starts the same bash in iTerm2, they still see something usable.

### `zsh` integration (`termica-integration.zsh`)

```zsh
# Termica integration, version 1.
[[ -n "${TERMICA:-}" ]] || return 0
[[ -n "${__TERMICA_LOADED:-}" ]] && return 0
__TERMICA_LOADED=1

__termica_osc() { printf '\033]%s\033\\' "$1"; }

__termica_precmd() {
    local exit=$?
    __termica_osc "133;D;${exit}"
    __termica_osc "133;A"
    __termica_osc "1337;TermicaVersion=1"
    __termica_osc "1337;TermicaShell=zsh"
    __termica_osc "1337;TermicaCwd=file://${HOST}${PWD}"   # zsh handles encoding adequately
}
__termica_preexec() { __termica_osc "133;C"; }

autoload -Uz add-zsh-hook
add-zsh-hook precmd  __termica_precmd
add-zsh-hook preexec __termica_preexec

# Marker-only prompt; visible prompt is Termica's UI.
PROMPT=$'%{\e]133;B\a%}%# '
```

Zsh is cleaner here because `precmd` / `preexec` are first-class hooks.

### What the scripts deliberately don't do

- No PS1 art. The visible prompt belongs to Termica's status header ([06](06-workspace-and-tiles.md)).
- No completion forwarding. Tab completion in `ShellPromptEditor` is local in v1 ([04](04-prompt-editor.md)).
- No history capture from the shell. Termica records its own history at the prompt-submit moment ([07](07-history-and-search.md)).
- No background telemetry, no probe loops. The shell does nothing extra outside of prompt hooks.

## The installer

`termica install-integration` is a CLI subcommand on the main binary.

Responsibilities:

1. Detect bash / zsh by inspecting `$SHELL` and offer both if both are present.
2. For each shell, locate the canonical rc file (`~/.bashrc`, `~/.zshrc`); never `bash_profile` (it's not loaded for non-login terminals).
3. Write the integration script to `$XDG_CONFIG_HOME/termica/integration.{bash,zsh}` (defaulting to `~/.config/termica/`).
4. Append a fenced block to the rc file:

    ```bash
    # >>> termica integration (do not edit; managed by `termica install-integration`)
    [ -n "$TERMICA" ] && [ -r "$HOME/.config/termica/integration.bash" ] && \
        . "$HOME/.config/termica/integration.bash"
    # <<< termica integration
    ```

   The fence comments are exact strings; we detect existing fences and replace the block atomically. Upgrades are idempotent and visible in `git diff` of the rc file (good for users who version-control dotfiles).

5. Print what was changed and exit 0.

When Termica spawns a shell, it sets `TERMICA=1` in the environment. The rc-side guard means the integration is a no-op for shells launched outside Termica.

## What if the script isn't installed?

The pane is still a real terminal. The prompt editor is simply never available — the `PromptController` stays in `RawTerminal` forever ([05](05-pane-modes.md)). The status header degrades to showing only what we can probe locally (cwd via `OSC 7` if the shell happens to emit it; otherwise inferred from process info).

This is fine. The app is still useful. Installation upgrades it.

## Version negotiation

`TermicaVersion=N` on every prompt start lets future Termica releases reject incompatible integration scripts cleanly. Forward compatibility rules:

- If `N` is unknown (too new): pane stays in `RawTerminal`; a notification suggests upgrading Termica.
- If `N` is too old: pane stays in `RawTerminal`; the installer can write the current script.
- If `N` is supported but minor: full functionality, possibly with a debounced "upgrade available" hint.

The version is just a `u32`. It bumps on any normative change to script behavior.

## Testing

- **Integration tests** in `tests/` spawn `bash --rcfile <our-tmp-rc>` and `zsh -f -d` plus an env-injected source line; assert the resulting marker stream is exactly what we expect for a scripted sequence of inputs.
- **Snapshot the rc-file mutation**: `termica install-integration` against a fixture rc file produces a deterministic diff; re-running it twice is a no-op.
- **Marker fixtures** in `testdata/markers/<scenario>.bytes` — recorded byte streams from real shells with our integration loaded; replayed against the marker parser; expected `MarkerEvent` sequence asserted.

---

**← Previous:** [02 — Terminal engine](02-terminal-engine.md) | **Next:** [04 — Prompt editor](04-prompt-editor.md) →
