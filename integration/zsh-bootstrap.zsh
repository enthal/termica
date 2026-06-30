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

# Emit the shell's current variable NAMES for Termica's `$VAR`
# tab-completion. NAMES ONLY — never values, which routinely hold secrets
# (API tokens, AWS creds). This is what lets completion see the LIVE shell:
# non-exported parameters like `HISTFILE` / `PS1`, and anything `export`ed
# after spawn, none of which are in the spawn-time environment snapshot.
#
# Change-gated: emits only when the name set differs from the last
# emission, so a steady prompt loop (same vars every prompt) costs nothing
# beyond building the list. State is held in `__termica_last_vars_sig`
# (excluded from the report, along with our other `__termica*` internals,
# so it never perturbs the signature).
__termica_last_vars_sig=""
termica_emit_vars() {
    setopt local_options extended_glob
    # `${(k)parameters}` is every parameter name. Keep only shell-
    # identifier-shaped names (drops zsh specials like `?`, `#`, `argv`)
    # and drop our own `__termica*` internals.
    local -a names
    names=(${(M)${(@k)parameters}:#[A-Za-z_][A-Za-z0-9_]#})
    names=(${names:#__termica*})
    local sig="${(j: :)${(o)names}}"
    [[ "$sig" == "$__termica_last_vars_sig" ]] && return 0
    __termica_last_vars_sig="$sig"
    # Build a JSON array of the (sorted) names. Names are identifiers, but
    # escape defensively through the same helper the other emitters use.
    local json="[" first=1 n
    for n in ${(o)names}; do
        if (( first )); then first=0; else json+=","; fi
        json+="\"$(termica_escape_json "$n")\""
    done
    json+="]"
    termica_emit_raw "shell_vars" "$json"
}

# ----- lifecycle hooks ---------------------------------------------------

# True if `$1` is Termica's own completion sentinel command
# (`__termica_complete <id> <b64>`, optionally leading-spaced). The sentinel
# is dispatched as a real command (zsh has no read-eval loop to field a
# framed request — see the live-completion machinery below), so the hooks
# must recognise and IGNORE it: emitting a preexec / command_finished /
# precmd for it would corrupt the block model and — via precmd, the
# promotion trigger — risk a spurious mode transition (spec/05). It is thus
# inert to the pane-mode machine, exactly like fish's read-loop request.
termica_is_completion_sentinel() {
    [[ "${1## }" == __termica_complete\ * ]]
}

termica_preexec() {
    # $1 is the expanded command line zsh is about to run.
    if termica_is_completion_sentinel "$1"; then
        # Skip the marker AND tell precmd to stay quiet for this command.
        __termica_skip_next_precmd=1
        return
    fi
    termica_emit_string "preexec" "$1"
}

termica_precmd() {
    # CAPTURE EXIT STATUS FIRST — must run before any helper command
    # would clobber $?.
    local exit_status=$?
    # Stay inert after the completion sentinel (flag set in preexec, and
    # defensively in `__termica_complete` itself in case preexec didn't run).
    if [[ -n "${__termica_skip_next_precmd:-}" ]]; then
        unset __termica_skip_next_precmd
        return
    fi
    termica_emit_int "command_finished" "$exit_status"
    termica_emit_string "precmd" "$PWD"
    # Live `$VAR`-completion source (change-gated, names only).
    termica_emit_vars
    # Release the warm completion child if it's gone idle (frees ~5 MB; the
    # next Tab respawns it). Cheap: one arithmetic test per prompt.
    __termica_zc_idle_check
}

# Keep the completion sentinel out of the user's shell history (in-memory
# AND the history file): a `zshaddhistory` hook returning non-zero drops the
# line before it's ever recorded. More surgical than toggling a global
# option like `hist_ignore_space`, which would change the user's own
# space-prefixed-command behaviour.
termica_zshaddhistory() {
    termica_is_completion_sentinel "${1%%$'\n'}" && return 1
    return 0
}

# Idempotent hook installation. Registers our hooks if (and only if)
# they aren't already in the relevant hook arrays.
termica_ensure_hooks() {
    autoload -Uz add-zsh-hook
    # add-zsh-hook is itself idempotent: registering the same function
    # twice is a no-op.
    add-zsh-hook preexec termica_preexec
    add-zsh-hook precmd termica_precmd
    add-zsh-hook zshaddhistory termica_zshaddhistory
    # Remove the completion scratch files when this shell exits cleanly.
    add-zsh-hook zshexit __termica_zc_cleanup
}

# ----- live-shell Tab completion -----------------------------------------
#
# zsh has no clean `complete -C <line>` like fish, and its completion system
# (`_main_complete` → `compadd`) only produces matches inside a REAL ZLE
# widget — which the pane's own shell can't host (it runs with `unsetopt
# zle`). So Termica answers completion from a WARM captive child:
#
#   - A persistent `zsh/zpty` child (`zsh -f -i`, ZLE enabled) is spawned
#     lazily on the first Tab and reused. It is seeded ONCE with the user's
#     `$fpath` + `compinit` (so their tools' completions work) — NEVER by
#     re-sourcing dotfiles, which on a slow `.zshrc` would make every Tab as
#     slow as shell startup. Spawning `zsh -f` + `compinit -C` is cheap.
#   - Each request replays the live shell's `alias` table into the child, so
#     config AND runtime-defined aliases complete (the headline capability).
#   - A `compadd` wrapper records the would-be matches via `compadd -O`
#     (zsh extracts them itself — no fragile option-parsing); a one-shot
#     widget drives `complete-word` for the requested buffer.
#
# v1 emits **values only** (zsh descriptions are gated behind `verbose` /
# format zstyles and are config-dependent and fragile). The raw value lines
# go out in a `completion` DCS marker, parsed by the SAME Rust parser as
# fish's reply (spec/04a §"Zsh sidecar"). The child's lifetime is tied to
# this bootstrap process, so it dies with the pane — no explicit teardown.

# The captive child's setup program. Delivered by writing it to a file and
# `source`-ing that file in the child (a SHORT command line): the child runs
# `zsh -f -i` with ZLE enabled, and a long command WRITTEN to its line editor
# wraps at the pty width and never executes. `source <file>` sidesteps the
# line editor entirely AND keeps our `TZ*` sentinels out of the pty's input
# echo (file contents aren't echoed), so the reader stays in sync. `$fpath`
# is prepended per-spawn by `__termica_zc_ensure`.
typeset -g __TERMICA_ZC_PROG
read -r -d '' __TERMICA_ZC_PROG <<'TZPROG'
PS1='' PS2='' RPS1='' PROMPT='' RPROMPT=''
setopt no_beep no_always_last_prompt
autoload -Uz compinit
compinit -u -d "${TMPDIR:-/tmp}/termica-zsh-compdump" 2>/dev/null
typeset -ga TZCAP
# Record the would-be matches: `compadd -O` asks zsh to extract just the
# match words (matchers applied) into an array, with no side effects; then
# delegate to the real builtin so completion still proceeds.
compadd() {
  local -a __m
  builtin compadd -O __m "$@" 2>/dev/null
  TZCAP+=("${__m[@]}")
  builtin compadd "$@"
}
zstyle ':completion:*' completer _complete
zstyle ':completion:*' menu no
# Drive completion for $TZBUF inside a real widget, then abort the line.
# Trigger is `^X^X` — NOT `^T`, which BSD/macOS ttys intercept as VSTATUS
# (SIGINFO) so it never reaches ZLE. `^X` is not a tty control char.
_tzgo() { TZCAP=(); BUFFER="$TZBUF"; CURSOR=$#BUFFER; { zle complete-word } always { : }; BUFFER=''; zle send-break }
zle -N _tzgo
bindkey '^X^X' _tzgo
# Dump the recorded matches (deduped) bracketed by sentinels, tab-joined —
# the shape the Rust parser reads. Defined here (sourced, never echoed) so
# the literal sentinels never enter the reader's input stream.
_tzdump() {
  local t=$'\t' __r
  print -r -- TZSTART
  for __r in "${(u)TZCAP[@]}"; do print -r -- "TZROW$t$__r"; done
  print -r -- TZEND
}
print -r -- TZREADY
TZPROG

# Read from the captive child until a line CONTAINS `$1`, collecting any
# `TZROW\t<value>` lines into `__termica_zc_rows`. Non-blocking polls
# (`zpty -r -t`) bounded so a wedged child can never hang the prompt; the
# ~600ms cap matches the Rust-side live-completion timeout.
typeset -ga __termica_zc_rows
__termica_zc_read_until() {
    local token="$1" i chunk l
    for (( i = 0; i < 120; i++ )); do
        if zpty -r -t __termica_zc chunk 2>/dev/null; then
            for l in "${(@f)chunk}"; do
                l="${l%$'\r'}"
                [[ "$l" == TZROW$'\t'* ]] && __termica_zc_rows+=("${l#TZROW$'\t'}")
                [[ "$l" == *"$token"* ]] && return 0
            done
        else
            sleep 0.005
        fi
    done
    return 1
}

# Per-pane scratch files (sourced into the child). `$$` is the bootstrap
# pid, so panes don't collide; removed on shell exit by `__termica_zc_cleanup`.
typeset -g __termica_zc_setup_file="${TMPDIR:-/tmp}/termica-zc-$$-setup.zsh"
typeset -g __termica_zc_req_file="${TMPDIR:-/tmp}/termica-zc-$$-req.zsh"

__termica_zc_cleanup() {
    rm -f "$__termica_zc_setup_file" "$__termica_zc_req_file" 2>/dev/null
}

# Spawn-and-seed the warm captive child if it isn't already running. Returns
# non-zero if zpty is unavailable or the child won't come up (→ the pane
# silently has no shell completion, still a usable terminal).
__termica_zc_ensure() {
    zmodload zsh/zpty 2>/dev/null || return 1
    zpty -t __termica_zc 2>/dev/null && return 0   # already alive
    zpty -d __termica_zc 2>/dev/null               # clear any dead remnant
    # Write the setup file with the user's live $fpath prepended (so their
    # installed completions load), then have the child source it. The fpath
    # is emitted as a MULTI-LINE array assignment (one q-quoted entry per
    # line) — `"fpath=(${(q)fpath})"` in double quotes would collapse the
    # array into one escaped-space blob (a single bogus entry).
    {
        print -rl -- "fpath=(" ${(q)fpath} ")"
        print -r -- "$__TERMICA_ZC_PROG"
    } > "$__termica_zc_setup_file" 2>/dev/null || return 1
    zpty __termica_zc 'zsh -f -i' 2>/dev/null || return 1
    zpty -w __termica_zc "source ${(q)__termica_zc_setup_file}" 2>/dev/null
    if ! __termica_zc_read_until "TZREADY"; then
        zpty -d __termica_zc 2>/dev/null
        return 1
    fi
    return 0
}

# Idle-drop bookkeeping for the warm completion child. `$SECONDS` (zsh's
# built-in seconds-since-shell-start counter) of the last completion request;
# the child holds ~5 MB, so we release it after a spell of no Tab use.
typeset -gi __termica_zc_last_use=0
typeset -gi __TERMICA_ZC_IDLE_SECS=300

# Drop the warm completion child if it has been idle longer than
# `__TERMICA_ZC_IDLE_SECS`, freeing its memory; the next Tab lazily respawns
# it (`__termica_zc_ensure`). Called from `termica_precmd` — i.e. once per real
# prompt — so a pane that keeps running commands but has stopped using Tab
# completion releases the child. (precmd is the only in-shell clock we have, so
# a pane left truly idle AT a prompt keeps the child until its next command;
# the common "used completion a while ago, still working" case is covered.)
# No-op when there's no live child. A live child always implies a prior
# `__termica_zc_capture` ran (the only caller of `__termica_zc_ensure`), which
# stamped `__termica_zc_last_use` — so the alive check alone gates the drop;
# we don't second-guess `last_use` (guarding `> 0` would wrongly spare a child
# first used at second 0 of the shell's life).
__termica_zc_idle_check() {
    zpty -t __termica_zc 2>/dev/null || return 0   # no live child → nothing to drop
    if (( SECONDS - __termica_zc_last_use > __TERMICA_ZC_IDLE_SECS )); then
        zpty -d __termica_zc 2>/dev/null
    fi
}

# Capture completion candidates for `$1` (the command line up to the
# cursor) into `__termica_zc_rows` (values only, deduped). Best-effort: on
# any failure the rows are left empty and the popup falls back to locals.
__termica_zc_capture() {
    local line="$1"
    __termica_zc_rows=()
    __termica_zc_ensure || return 1
    # Timestamp this use so `__termica_zc_idle_check` can release an idle child.
    __termica_zc_last_use=$SECONDS
    # Per-request file (sourced, not typed): replay the live shell's aliases
    # — config AND runtime-defined — so they complete, and set the buffer.
    # A long / multi-line value here would jam the child's line editor; a
    # file bypasses it.
    {
        alias -L 2>/dev/null
        print -r -- "TZBUF=${(qqq)line}"
    } > "$__termica_zc_req_file" 2>/dev/null || return 1
    zpty -w __termica_zc "source ${(q)__termica_zc_req_file}" 2>/dev/null
    # Fire the completion widget (^X^X), then call the dump function.
    zpty -w -n __termica_zc $'\C-x\C-x' 2>/dev/null
    __termica_zc_rows=()
    zpty -w __termica_zc '_tzdump' 2>/dev/null
    if ! __termica_zc_read_until "TZEND"; then
        zpty -d __termica_zc 2>/dev/null   # wedged → respawn next time
    fi
    return 0
}

# Emit the `completion` DCS marker carrying `$1` (the correlation id) and
# the captured rows as a JSON string array — the same shape fish emits, so
# Rust's shared parser handles both. Note the extra top-level `id` field, so
# this can't reuse `termica_emit_raw`.
__termica_emit_completion() {
    local id="$1"
    local json="[" first=1 c
    for c in "${__termica_zc_rows[@]}"; do
        if (( first )); then first=0; else json+=","; fi
        json+="\"$(termica_escape_json "$c")\""
    done
    json+="]"
    local session="${TERMICA_SESSION_ID:-}"
    printf '\033PTermica;{"type":"completion","session":"%s","id":%s,"value":%s}\033\\' \
        "$session" "$id" "$json"
}

# The completion sentinel dispatched by Termica as a real command:
# `__termica_complete <id> <base64(line)>`. Decodes the line, captures
# candidates from the warm child, and emits the reply marker. Kept INERT to
# the mode machine by `termica_preexec`/`termica_precmd` (which recognise the
# sentinel) and out of history by `termica_zshaddhistory`. Preserves the
# user's last `$?` so a later real `precmd` reports the right status; sets the
# precmd-skip flag defensively in case preexec didn't run.
__termica_complete() {
    local __exit=$?
    __termica_skip_next_precmd=1
    emulate -L zsh
    local id="$1" line
    line=$(printf '%s' "$2" | base64 -d 2>/dev/null)
    __termica_zc_capture "$line"
    __termica_emit_completion "$id"
    return $__exit
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

# 2c. Source the user's real .zprofile. Termica spawns zsh as a LOGIN
#     shell (`zsh -i -l`) so `/etc/zprofile` runs `path_helper` (the macOS
#     PATH builder reading `/etc/paths` + `/etc/paths.d/*`). But the ZDOTDIR
#     redirect aims the per-user profile lookup at our wrapper dir, so zsh
#     looked for `$ZDOTDIR/.zprofile` (absent) and skipped the user's — the
#     same skip-and-recover dance as .zshenv / .zshrc. Vanilla login order
#     sources .zprofile BEFORE .zshrc; we honour that. Without this the user
#     loses every PATH entry set in .zprofile (`brew shellenv`, manual
#     prepends, `~/.local/bin`).
[[ -r "$__termica_user_zdotdir/.zprofile" ]] && source "$__termica_user_zdotdir/.zprofile"

# 3. Source the user's real .zshrc. Same skip-and-recover dance:
#    zsh would normally have sourced `$ZDOTDIR/.zshrc` automatically,
#    but its ZDOTDIR pointed at our wrapper dir (and this IS that
#    file). Source the user's instead.
[[ -r "$__termica_user_zdotdir/.zshrc" ]] && source "$__termica_user_zdotdir/.zshrc"

# 3b. Source the user's real .zlogin — sourced AFTER .zshrc in a vanilla
#     login zsh, skipped here for the same ZDOTDIR-redirect reason.
[[ -r "$__termica_user_zdotdir/.zlogin" ]] && source "$__termica_user_zdotdir/.zlogin"

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
