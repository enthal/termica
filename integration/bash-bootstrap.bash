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

# ----- lifecycle hooks ---------------------------------------------------

termica_preexec() {
    # bash-preexec passes the command line as $1.
    termica_emit_string "preexec" "$1"
}

termica_precmd() {
    # bash-preexec runs precmd_functions AFTER capturing $?, so we
    # can safely read it from $? here without an immediate `local exit=$?`
    # at the very top — bash-preexec has already preserved it for us.
    # We still capture immediately for symmetry with the zsh script.
    local exit_status=$?
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

# Emit the gate-opening lifecycle message.
termica_emit_raw "integration_ready" "{\"shell\":\"bash\",\"version\":${TERMICA_INTEGRATION_VERSION:-1}}"
