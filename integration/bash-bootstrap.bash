# Termica bash bootstrap, version 1.
#
# Bash launches this file via `bash --noprofile --norc --rcfile <this>`.
# The bash binary reads it during normal startup before drawing the
# first prompt. Termica regenerates the on-disk wrapper from the
# `include_str!` constant in src/integration.rs on every spawn.
#
# The wrapper:
#   1. Defines Termica helpers.
#   2. Sources the vendored bash-preexec (preexec/precmd hook arrays).
#   3. Sources the user's real ~/.bashrc.
#   4. Reasserts hooks via termica_ensure_hooks.
#   5. Emits integration_ready.
#
# Protocol: DCS-JSON over `ESC P Termica;{...}ESC \` — spec/03.

# Guard against double-bootstrap within THIS shell process. The flag
# is intentionally NOT exported: subprocesses (including a nested
# Termica binary launched from this shell) must run their own
# bootstrap fresh. Inheriting this flag would cause `cargo run` (or
# any other child process that itself spawns a managed shell) to
# skip integration in its children.
if [[ -n "${TERMICA_BOOTSTRAPPED:-}" ]]; then
    return 0 2>/dev/null || true
fi
TERMICA_BOOTSTRAPPED=1

# ----- helpers -----------------------------------------------------------

termica_escape_json() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    s="${s//$'\t'/\\t}"
    printf '%s' "$s"
}

termica_emit_raw() {
    local type="$1"
    local raw_value="$2"
    printf '\033PTermica;{"type":"%s","session":"%s","value":%s}\033\\' \
        "$type" "${TERMICA_SESSION_ID:-}" "$raw_value"
}

termica_emit_string() {
    local type="$1"
    local s
    s="$(termica_escape_json "$2")"
    termica_emit_raw "$type" "\"$s\""
}

termica_emit_int() {
    local type="$1"
    local n="$2"
    termica_emit_raw "$type" "$n"
}

# Emit the shell's current variable NAMES for Termica's `$VAR`
# tab-completion. NAMES ONLY — never values (they routinely hold secrets).
# This is what lets completion see the LIVE shell: non-exported variables
# and anything `export`ed after spawn, neither of which is in the
# spawn-time environment snapshot. Change-gated so a steady prompt loop
# costs nothing beyond building the list; state in `__termica_last_vars_sig`
# (excluded from the report, with our other `__termica*` internals).
__termica_last_vars_sig=""
termica_emit_vars() {
    # `compgen -v` lists every shell variable name (exported or not); they
    # are all identifier-shaped. Sort for a stable change signature.
    local names sig json first=1 n
    names="$(compgen -v 2>/dev/null | LC_ALL=C sort -u)"
    sig="$names"
    [[ "$sig" == "$__termica_last_vars_sig" ]] && return 0
    __termica_last_vars_sig="$sig"
    json="["
    while IFS= read -r n; do
        [[ -z "$n" ]] && continue
        case "$n" in __termica*) continue ;; esac
        if (( first )); then first=0; else json+=","; fi
        json+="\"$(termica_escape_json "$n")\""
    done <<< "$names"
    json+="]"
    termica_emit_raw "shell_vars" "$json"
}

# ----- live-shell completion ---------------------------------------------
#
# A bash pane at a prompt answers Tab-completion from its OWN process, so the
# user's runtime-defined aliases / functions / `complete` specs and every
# `bash-completion`-installed completion resolve (a fresh `bash -c` subprocess
# couldn't see them, and re-sourcing dotfiles per Tab would be unacceptably
# slow). Unlike zsh — whose `compadd` needs a real ZLE widget, forcing a warm
# captive child — bash completion functions just fill `COMPREPLY` and need no
# readline context, so the capture runs IN-PROCESS here.
#
# Dispatched as the guarded command `__termica_complete <id> <base64(line)>`
# (see src/submit_framing.rs). Kept INERT to the mode machine by the
# sentinel guards in `termica_preexec` / `termica_precmd`, and out of history
# by `__termica_complete` deleting its own entry. v1 emits VALUES ONLY.

# Capture completion candidates for the command line $1 (up to the cursor)
# into `__termica_bc_rows` (values only, deduped, stable order). Best-effort:
# any failure leaves the rows empty and the popup falls back to locals.
__termica_bc_rows=()
# Split $1 into readline-style completion words, into the global array
# `__termica_bc_words`: breaks on whitespace AND on each non-whitespace
# `$COMP_WORDBREAKS` char (`=`, `:`, `@`, …), with the break char emitted as
# its own token — exactly how readline tokenises a line for completion. This
# is what lets `export FOO=ba`<Tab> complete `ba` (not `FOO=ba`), and what
# bash-completion's own `_get_comp_words_by_ref -n :` re-merges from for
# `scp host:pa`. Quote / backslash handling is a later refinement; the
# unquoted common case is what 99% of completions need. A fixed global array
# (not a `local -n` nameref) keeps this working on bash 4.0–4.2, which have
# bash-completion but not namerefs (4.3+).
__termica_bc_words=()
# bash's `-o filenames` for the last capture (see `__termica_bc_capture`): 1 ⇒
# filename completion (matches escaped on accept), 0 ⇒ insert verbatim (#178).
__termica_bc_filenames=0
__termica_bc_split_words() {
    __termica_bc_words=()
    # `s` is assigned in its OWN `local` first: referencing `${#s}` in the same
    # `local` that assigns `s` reads the OUTER scope's `s` (a bash gotcha), so
    # `n` would be 0 and the loop never runs.
    local s="$1"
    local i=0 n=${#s} ch cur="" breaks="$COMP_WORDBREAKS"
    while ((i < n)); do
        ch="${s:i:1}"
        if [[ "$ch" == [[:space:]] ]]; then
            [[ -n "$cur" ]] && {
                __termica_bc_words+=("$cur")
                cur=""
            }
        elif [[ "$breaks" == *"$ch"* ]]; then
            [[ -n "$cur" ]] && {
                __termica_bc_words+=("$cur")
                cur=""
            }
            __termica_bc_words+=("$ch")
        else
            cur+="$ch"
        fi
        ((i++))
    done
    [[ -n "$cur" ]] && __termica_bc_words+=("$cur")
}

__termica_bc_capture() {
    __termica_bc_rows=()
    # Reset the `-o filenames` signal; set from the static spec and a `compopt`
    # shim around the function call below (#178).
    __termica_bc_filenames=0
    local line="$1"
    # All COMP_* are `local`, so they are gone when this returns — nothing can
    # leak into the next real command's bash-preexec DEBUG trap, which SKIPS
    # preexec whenever COMP_POINT is set (a stuck COMP_POINT would silently
    # stop emitting preexec and break the block model).
    local COMP_LINE COMP_POINT COMP_CWORD cur prev cmd spec func c
    local -a COMP_WORDS COMPREPLY
    COMP_LINE="$line"
    COMP_POINT=${#line}
    __termica_bc_split_words "$line"
    COMP_WORDS=("${__termica_bc_words[@]}")
    # A trailing space (or empty line) means a fresh, empty word at the cursor.
    [[ -z "$line" || "$line" == *[[:space:]] ]] && COMP_WORDS+=("")
    COMP_CWORD=$(( ${#COMP_WORDS[@]} - 1 ))
    (( COMP_CWORD < 0 )) && COMP_CWORD=0
    # The command is the first whitespace-delimited word; with the readline
    # split that's still `COMP_WORDS[0]` for an ordinary command line.
    cmd="${COMP_WORDS[0]}"
    [[ -z "$cmd" ]] && return 0
    # Trigger bash-completion's lazy loader for this command, if loaded.
    if declare -F _comp_load >/dev/null 2>&1; then
        _comp_load -- "$cmd" 2>/dev/null
    elif declare -F __load_completion >/dev/null 2>&1; then
        __load_completion "$cmd" 2>/dev/null
    fi
    # The registered spec, or the dynamic `-D` default. We only drive the
    # `-F <function>` form (what bash-completion uses for everything that
    # matters); `-G`/`-W`/`-C`-only specs fall through to the local sources.
    spec="$(complete -p "$cmd" 2>/dev/null)" || spec="$(complete -p -D 2>/dev/null)"
    [[ "$spec" =~ -F[[:space:]]+([^[:space:]]+) ]] || return 0
    func="${BASH_REMATCH[1]}"
    declare -F "$func" >/dev/null 2>&1 || return 0
    # `-o filenames` can be set STATICALLY in the spec (`complete -o filenames
    # -F ...`); record that before the function runs.
    [[ "$spec" == *" -o filenames"* ]] && __termica_bc_filenames=1
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    (( COMP_CWORD > 0 )) && prev="${COMP_WORDS[COMP_CWORD-1]}"
    # Most completions request filenames DYNAMICALLY (`_filedir` calls `compopt
    # -o filenames`), invisible to `complete -p`. Shim `compopt` for the
    # duration of the call to observe it (and `+o filenames` turning it back
    # off), forwarding faithfully to the builtin so the completion behaves
    # exactly as it would unshimmed. Restored immediately after.
    compopt() {
        local __op="" __a
        for __a in "$@"; do
            case "$__a" in
                -o) __op="-o" ;;
                +o) __op="+o" ;;
                filenames)
                    if [[ "$__op" == "+o" ]]; then
                        __termica_bc_filenames=0
                    else
                        __termica_bc_filenames=1
                    fi
                    ;;
            esac
        done
        builtin compopt "$@"
    }
    # Completion functions are called as `func cmd cur prev`; they read the
    # COMP_* globals (set above) and fill COMPREPLY. Errors are swallowed.
    "$func" "$cmd" "$cur" "$prev" 2>/dev/null
    unset -f compopt
    local -A __seen=()
    for c in "${COMPREPLY[@]}"; do
        [[ -z "$c" ]] && continue
        [[ -n "${__seen[$c]:-}" ]] && continue
        __seen[$c]=1
        __termica_bc_rows+=("$c")
    done
}

# Emit the `completion` DCS marker carrying $1 (the correlation id) and the
# captured rows as a JSON string array — the same shape fish/zsh emit, so
# Rust's shared parser handles all three. The extra top-level `id` field means
# this can't reuse `termica_emit_raw`.
__termica_emit_completion() {
    local id="$1" json="[" first=1 c fn=false
    (( __termica_bc_filenames )) && fn=true
    for c in "${__termica_bc_rows[@]}"; do
        if (( first )); then first=0; else json+=","; fi
        json+="\"$(termica_escape_json "$c")\""
    done
    json+="]"
    printf '\033PTermica;{"type":"completion","session":"%s","id":%s,"filenames":%s,"value":%s}\033\\' \
        "${TERMICA_SESSION_ID:-}" "$id" "$fn" "$json"
}

# The completion sentinel dispatched by Termica as a real command:
# `__termica_complete <id> <base64(line)>`. Stays INERT to the mode machine
# (the `termica_preexec`/`termica_precmd` guards) and out of history (deletes
# its own entry — bash-preexec read it from history, so we can't use a leading
# space / `ignorespace`, which would also hide it from bash-preexec and
# corrupt the block model). Preserves the user's last `$?`.
__termica_complete() {
    local __exit=$?
    __termica_skip_next_precmd=1
    # Drop our own history entry: parse the index from `history 1` and delete
    # it. (bash adds the command to history before running it, so it's the
    # most recent entry here.)
    local __entry __idx
    __entry="$(builtin history 1)"
    __idx="${__entry#"${__entry%%[![:space:]]*}"}"
    __idx="${__idx%% *}"
    [[ "$__idx" == [0-9]* ]] && builtin history -d "$__idx" 2>/dev/null
    local id="$1" line
    line="$(printf '%s' "$2" | base64 -d 2>/dev/null)"
    __termica_bc_capture "$line"
    __termica_emit_completion "$id"
    return "$__exit"
}

# ----- lifecycle hooks ---------------------------------------------------

# True when bash-preexec's $1 (the running command line) is our completion
# sentinel. The sentinel has no leading space, so a simple prefix match is
# exact; the `__termica_` namespace makes a real user command here implausible.
__termica_is_completion_sentinel() {
    [[ "$1" == __termica_complete\ * ]]
}

termica_preexec() {
    # bash-preexec passes the command line as $1. The completion sentinel must
    # NOT announce a command (no preexec/command_finished/precmd) — set the
    # skip flag and stay silent so it's invisible to the block model.
    if __termica_is_completion_sentinel "$1"; then
        __termica_skip_next_precmd=1
        return
    fi
    termica_emit_string "preexec" "$1"
}

termica_precmd() {
    # bash-preexec runs precmd_functions AFTER capturing $?, so we
    # can safely read it from $? here without an immediate `local exit=$?`
    # at the very top — bash-preexec has already preserved it for us.
    # We still capture immediately for symmetry with the zsh script.
    local exit_status=$?
    # Inert tail of a completion sentinel: consume the skip flag and emit
    # nothing, so the synthetic request leaves the prompt cycle untouched.
    if [[ -n "${__termica_skip_next_precmd:-}" ]]; then
        unset __termica_skip_next_precmd
        return
    fi
    termica_emit_int "command_finished" "$exit_status"
    termica_emit_string "precmd" "$PWD"
    # Live `$VAR`-completion source (change-gated, names only).
    termica_emit_vars
}

# Idempotent hook installation. bash-preexec exposes hook arrays
# named preexec_functions and precmd_functions; registering the same
# function twice would fire it twice, so we guard.
termica_ensure_hooks() {
    local fn
    local found=0
    for fn in "${preexec_functions[@]}"; do
        if [[ "$fn" == "termica_preexec" ]]; then
            found=1
            break
        fi
    done
    if (( ! found )); then
        preexec_functions+=("termica_preexec")
    fi

    found=0
    for fn in "${precmd_functions[@]}"; do
        if [[ "$fn" == "termica_precmd" ]]; then
            found=1
            break
        fi
    done
    if (( ! found )); then
        precmd_functions+=("termica_precmd")
    fi
}

# ----- bootstrap sequence ------------------------------------------------

# Locate bash-preexec relative to this wrapper file. Termica writes
# both files into the same data directory on every spawn.
__termica_wrapper_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -r "$__termica_wrapper_dir/bash-preexec.sh" ]]; then
    # shellcheck disable=SC1091
    source "$__termica_wrapper_dir/bash-preexec.sh"
else
    # If bash-preexec isn't where we expect, emit integration_error
    # and bail. Termica will transition the pane to Degraded.
    termica_emit_string "integration_error" "bash-preexec.sh not found alongside wrapper"
    return 1 2>/dev/null || true
fi
unset __termica_wrapper_dir

# Source the user's real ~/.bashrc if it exists.
if [[ -r "$HOME/.bashrc" ]]; then
    # shellcheck disable=SC1091
    source "$HOME/.bashrc"
fi

# Load bash-completion if the user's rc didn't, so the live-completion source
# (`__termica_bc_capture`) sees the installed completions. This is the bash
# analog of zsh's `compinit`. Interactive-only (bash-completion guards on `$-`),
# and `--noprofile` skipped the profile.d loader, so we load it here. Guarded
# on the version var so a user who already sourced it isn't double-loaded.
if [[ -z "${BASH_COMPLETION_VERSINFO:-}" ]]; then
    for __termica_bc in \
        /opt/homebrew/etc/profile.d/bash_completion.sh \
        /usr/local/etc/profile.d/bash_completion.sh \
        /etc/profile.d/bash_completion.sh \
        /usr/share/bash-completion/bash_completion \
        /etc/bash_completion; do
        if [[ -r "$__termica_bc" ]]; then
            # shellcheck disable=SC1090
            source "$__termica_bc" 2>/dev/null
            break
        fi
    done
    unset __termica_bc
fi

# Reassert hooks after user config has had its chance.
termica_ensure_hooks

# Hand the prompt-drawing role to Termica. Per spec/04 ("The
# integration script intentionally minimises PS1"). bash is already
# launched with `--noediting` (see src/integration.rs) so readline
# is off and the tty stays in canonical mode — kernel ECHO does the
# echoing, Termica's `EchoSuppressor` filters the kernel echo, and
# Termica's `PromptEditor` (Phase 4B) is the only line editor on
# screen.
PS1=''
# PS2 is bash's continuation prompt — emitted when the parser sees
# an incomplete command. Mirror the zsh-bootstrap treatment: have
# bash emit a Termica DCS-JSON `continuation` marker so the editor
# re-promotes with the submitted text restored. `\[...\]` is bash's
# equivalent of zsh's `%{...%}` — wraps non-printing sequences so
# prompt-width math doesn't count them. `printf` builds the literal
# bytes once at bootstrap time and bakes them into PS2.
PS2=$(printf '\[\033PTermica;{"type":"continuation","session":"%s","value":""}\033\\\]' "${TERMICA_SESSION_ID:-}")

# Whether live Tab-completion is fully available: bash-completion 2.x needs
# bash 4.1+, and must have loaded. On an old bash (e.g. macOS's stock 3.2) it
# won't, so `__termica_bc_capture` only resolves the few manually-registered
# `complete` specs — the terminal and prompt editor still work fully. The flag
# rides `integration_ready` so Termica can surface a one-time UI notice.
if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 1) )) \
    && [[ -n "${BASH_COMPLETION_VERSINFO:-}" ]]; then
    __termica_completion=true
else
    __termica_completion=false
fi

# Emit the gate-opening lifecycle message.
termica_emit_raw "integration_ready" \
    "{\"shell\":\"bash\",\"version\":${TERMICA_INTEGRATION_VERSION:-1},\"bash_major\":${BASH_VERSINFO[0]},\"completion\":${__termica_completion}}"
unset __termica_completion
