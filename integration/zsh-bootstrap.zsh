# Termica zsh bootstrap, version 1.
#
# Loaded as the spawned zsh's .zshrc via ZDOTDIR redirect (the wrapper
# is materialised under Termica's data directory; Termica spawns zsh
# with `ZDOTDIR=<wrapper-dir>` and `zsh -i`). zsh sources this file as
# part of normal interactive startup, AFTER `/etc/zshenv` but instead
# of the user's `$HOME/.zshrc`.
#
# We immediately unset ZDOTDIR so user code never sees Termica's
# mutation, then manually source the user's real config in the order
# zsh would normally have followed.
#
# This file is NEVER sourced from a user's dotfiles. It is NEVER
# installed on disk by the user. Termica regenerates it on every spawn
# from the `include_str!` constant in src/integration.rs.
#
# Protocol emitted: DCS-JSON over `ESC P Termica;{...}ESC \`
# (see spec/03-shell-integration.md).

# Guard against double-bootstrap within THIS shell process (e.g. our
# .zshrc accidentally gets sourced twice). The flag is intentionally
# NOT exported: subprocesses (including a nested Termica binary
# launched from this shell) must run their own bootstrap fresh.
# Inheriting this flag would cause `cargo run` (or any other child
# process that itself spawns a managed shell) to skip integration in
# its children.
if [[ -n "${TERMICA_BOOTSTRAPPED:-}" ]]; then
    return 0 2>/dev/null || true
fi
TERMICA_BOOTSTRAPPED=1

# Remember the wrapper ZDOTDIR before we drop it. Files that zsh sourced
# BEFORE this one (notably macOS's `/etc/zshrc`) ran while ZDOTDIR still
# pointed here, so anything they derived from `${ZDOTDIR:-$HOME}` captured
# the wrapper temp dir. We repair the one that matters — HISTFILE — below.
local __termica_wrapper_zdotdir="$ZDOTDIR"

# Drop our ZDOTDIR mutation immediately so user code that inspects
# ZDOTDIR sees its real value (typically unset → $HOME). Note: zsh
# already finished sourcing `/etc/zshenv` and our `$ZDOTDIR/.zshenv`
# (which doesn't exist in the wrapper dir, so a no-op) before invoking
# this file — we are early enough that almost all user code runs after.
unset ZDOTDIR

# ----- helpers -----------------------------------------------------------

# Escape a string for safe JSON-string embedding. Handles backslash,
# double-quote, newline, tab, carriage return, and other control
# characters. Output is the inside of a JSON string (no surrounding
# quotes).
termica_escape_json() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    s="${s//$'\t'/\\t}"
    print -r -- "$s"
}

# Emit a Termica DCS-JSON lifecycle message. Framing:
#   ESC P Termica;{"type":...,"session":...,"value":...} ESC \
termica_emit_raw() {
    local type="$1"
    local raw_value="$2"
    printf '\033PTermica;{"type":"%s","session":"%s","value":%s}\033\\' \
        "$type" "${TERMICA_SESSION_ID:-}" "$raw_value"
}

# Emit a Termica DCS-JSON message with a string value (JSON-escaped).
termica_emit_string() {
    local type="$1"
    local s
    s="$(termica_escape_json "$2")"
    termica_emit_raw "$type" "\"$s\""
}

# Emit an integer value (no quoting).
termica_emit_int() {
    local type="$1"
    local n="$2"
    termica_emit_raw "$type" "$n"
}

# ----- lifecycle hooks ---------------------------------------------------

termica_preexec() {
    # $1 is the expanded command line zsh is about to run.
    termica_emit_string "preexec" "$1"
}

termica_precmd() {
    # CAPTURE EXIT STATUS FIRST — must run before any helper command
    # would clobber $?.
    local exit_status=$?
    termica_emit_int "command_finished" "$exit_status"
    termica_emit_string "precmd" "$PWD"
}

# Idempotent hook installation. Registers our hooks if (and only if)
# they aren't already in the relevant hook arrays.
termica_ensure_hooks() {
    autoload -Uz add-zsh-hook
    # add-zsh-hook is itself idempotent: registering the same function
    # twice is a no-op.
    add-zsh-hook preexec termica_preexec
    add-zsh-hook precmd termica_precmd
}

# ----- bootstrap sequence ------------------------------------------------

# 1. Source the user's real .zshenv. zsh would normally source this
#    at startup, but it looked for `$ZDOTDIR/.zshenv` (our wrapper
#    dir's, which doesn't exist) and skipped the user's. Do it now.
[[ -r "$HOME/.zshenv" ]] && source "$HOME/.zshenv"

# 2. Re-evaluate effective ZDOTDIR. The user's .zshenv may have set or
#    changed it. From here on, user config lives at
#    `${ZDOTDIR:-$HOME}/.zshrc`.
local __termica_user_zdotdir="${ZDOTDIR:-$HOME}"

# 2b. Repair HISTFILE if a startup file sourced before us pinned it inside
#     the wrapper temp dir. macOS's `/etc/zshrc` does exactly this:
#         HISTFILE=${ZDOTDIR:-$HOME}/.zsh_history
#     evaluated while ZDOTDIR still pointed at our wrapper. Left as-is, the
#     shell's NATIVE history (up-arrow, `!!`, `fc`, the `history` builtin)
#     would read and write a throwaway temp file that Termica deletes when
#     the pane closes — silently losing history and never touching the
#     user's real `~/.zsh_history`, which their other terminals share.
#     Rebase it onto the user's real config home (preserving any subpath).
#     The user's own `.zshrc`, sourced next, can still override HISTFILE.
if [[ -n "$__termica_wrapper_zdotdir" && "$HISTFILE" == "$__termica_wrapper_zdotdir"/* ]]; then
    HISTFILE="$__termica_user_zdotdir/${HISTFILE#$__termica_wrapper_zdotdir/}"
fi
unset __termica_wrapper_zdotdir

# 3. Source the user's real .zshrc. Same skip-and-recover dance:
#    zsh would normally have sourced `$ZDOTDIR/.zshrc` automatically,
#    but its ZDOTDIR pointed at our wrapper dir (and this IS that
#    file). Source the user's instead.
[[ -r "$__termica_user_zdotdir/.zshrc" ]] && source "$__termica_user_zdotdir/.zshrc"

# 4. Reassert hooks after user config has had its chance. If any
#    framework cleared `precmd_functions` or `preexec_functions`,
#    our hooks would be gone; add-zsh-hook puts them back.
termica_ensure_hooks

# 5. Hand the line-editor and prompt-drawing role to Termica.
#
#    Per spec/04 ("Visual structure: the block model" + "The integration
#    script intentionally minimises PS1 so the shell's own prompt
#    drawing doesn't visually conflict with Termica's chrome"):
#
#    - PS1 / PS2 / RPROMPT cleared so zsh prints nothing where the
#      shell prompt would be. Termica draws all prompt chrome via the
#      block model (4G adds the cwd / branch / dirty chips).
#    - `unsetopt zle` disables zsh's built-in line editor. Without
#      ZLE, zsh leaves the tty in canonical mode (ICANON + ECHO):
#      the kernel echoes each submitted line and zsh reads complete
#      lines from stdin. That's the only setup where Termica's echo
#      suppression in `EchoSuppressor` works deterministically —
#      ZLE would otherwise redraw the command with terminal escapes
#      that our prefix-match buffer can't anticipate. The Termica
#      `PromptEditor` (Phase 4B) replaces the editing UX that ZLE
#      was providing.
#
#    These are normative integration choices, not optional polish.
#    Bash / fish equivalents (TODO in their respective bootstraps)
#    will follow the same pattern.
PS1=''
# PS2 is zsh's continuation prompt — emitted every time the parser
# sees an incomplete command (e.g. `echo 1 &&` with no right-hand
# side, an unterminated quote, an open `{`). We piggyback on it as
# a "shell is waiting for more input" signal: Termica catches the
# DCS-JSON `continuation` marker and re-promotes the pane back to
# `ShellPromptEditor`, restoring the submitted text + `\n` into the
# editor so the user can keep editing the multi-line command.
#
# `%{...%}` wraps non-printing sequences so zsh's prompt-width math
# stays correct. We build the literal byte sequence once via
# `printf` (which deterministically interprets the `\033` and `\\`
# escapes) and bake it into PS2; using inline `$'...'` quoting was
# fragile under string concatenation (the second `'...'` chunk
# wasn't ANSI-C quoted, so the escapes there came through literal).
PS2=$(printf '%%{\033PTermica;{"type":"continuation","session":"%s","value":""}\033\\%%}' "${TERMICA_SESSION_ID:-}")
RPROMPT=''
unsetopt zle 2>/dev/null || true

# zsh's `prompt_sp` (default ON) detects when a command's output ends
# without a trailing newline and prints a reverse-color `%` to indicate
# the missing newline. We INTENTIONALLY leave it on: it's a real signal
# users want to see in their output ("did my program forget to print
# `\n`?"). `prompt_cr` is the sibling that emits `\r` before each
# prompt; also intentionally left on for the same reason.

# 6. Emit the gate-opening lifecycle message. Termica observes this
#    and transitions the pane out of Bootstrapping.
termica_emit_raw "integration_ready" "{\"shell\":\"zsh\",\"version\":${TERMICA_INTEGRATION_VERSION:-1}}"
