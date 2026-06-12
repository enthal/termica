# Termica fish bootstrap, version 1.
#
# Termica spawns fish with:
#   fish --no-config -c "<BOOTSTRAP>"
#
# Fish is run **non-interactively** (no `-i`): its line editor (the
# `reader`) never starts, so fish draws no prompt and performs no
# echo. Termica's `PromptEditor` is the only line editor, exactly as
# with zsh (`unsetopt zle`) and bash (`--noediting`). The bootstrap
# then runs a read-eval loop that reads finished command lines from
# stdin and executes them, emitting the lifecycle markers the
# interactive `fish_prompt` / `fish_preexec` / `fish_postexec` hooks
# would have (those events don't fire in a non-interactive shell).
#
# The tty stays in its default cooked (canonical + echo) mode, so the
# kernel echoes each submitted command back as `<text>\r\n` and
# Termica's `EchoSuppressor` drops it — the same mechanism zsh/bash
# use. No fish reader means no duplicated echo and no leaked prompt.
#
# Protocol: DCS-JSON over `ESC P Termica;{...}ESC \` — spec/03.

# Guard against double-bootstrap within THIS shell process. The flag
# is intentionally NOT exported: subprocesses (including a nested
# Termica binary launched from this shell) must run their own
# bootstrap fresh. `-g` (global) instead of `-gx` (global + export).
if set -q TERMICA_BOOTSTRAPPED
    exit 0
end
set -g TERMICA_BOOTSTRAPPED 1

# ----- helpers -----------------------------------------------------------

# Escape a string for JSON-string embedding.
function termica_escape_json
    set -l s $argv[1]
    # Order matters: escape backslash first, then quote, then control chars.
    set s (string replace -a '\\' '\\\\' -- $s)
    set s (string replace -a '"' '\\"' -- $s)
    set s (string replace -a \n '\\n' -- $s)
    set s (string replace -a \r '\\r' -- $s)
    set s (string replace -a \t '\\t' -- $s)
    echo -n -- $s
end

function termica_emit_raw
    set -l type $argv[1]
    set -l raw_value $argv[2]
    set -l session $TERMICA_SESSION_ID
    test -z "$session"; and set session ""
    printf '\033PTermica;{"type":"%s","session":"%s","value":%s}\033\\' \
        $type $session $raw_value
end

function termica_emit_string
    set -l type $argv[1]
    set -l s (termica_escape_json $argv[2])
    termica_emit_raw $type "\"$s\""
end

function termica_emit_int
    set -l type $argv[1]
    termica_emit_raw $type $argv[2]
end

# Emit the shell's current variable NAMES for Termica's `$VAR`
# tab-completion. NAMES ONLY — never values (they routinely hold secrets).
# Lets completion see the LIVE shell (variables defined after spawn, etc.),
# not just the spawn-time environment snapshot. Change-gated via
# `__termica_last_vars_sig` (excluded from the report, with our other
# `__termica*` internals) so a steady prompt loop is cheap.
set -g __termica_last_vars_sig ""
function termica_emit_vars
    # `set --names` lists every fish variable name; they're identifier-
    # shaped. Sort for a stable change signature.
    set -l names (set --names | sort -u)
    set -l sig (string join " " $names)
    test "$sig" = "$__termica_last_vars_sig"; and return 0
    set -g __termica_last_vars_sig $sig
    set -l json "["
    set -l first 1
    for n in $names
        string match -q '__termica*' -- $n; and continue
        if test $first -eq 1
            set first 0
        else
            set json "$json,"
        end
        set -l esc (termica_escape_json $n)
        set json "$json\"$esc\""
    end
    set json "$json]"
    termica_emit_raw shell_vars $json
end

# ----- bootstrap sequence ------------------------------------------------

set -l __termica_user_config_dir "$HOME/.config/fish"

# Source the user's config.fish if it exists.
if test -r "$__termica_user_config_dir/config.fish"
    source "$__termica_user_config_dir/config.fish"
end

# Source each conf.d/*.fish in order. fish would normally do this
# automatically as part of startup, but --no-config skips it.
if test -d "$__termica_user_config_dir/conf.d"
    for f in $__termica_user_config_dir/conf.d/*.fish
        if test -r "$f"
            source "$f"
        end
    end
end

set -e __termica_user_config_dir

# Survive Ctrl+C. While a command runs, Termica sends SIGINT to our
# process group; without help that would kill this non-interactive
# bootstrap loop — and, being the pane's only process, take the whole
# pane (and a single-tab window) down with it. A no-op
# `--on-signal SIGINT` handler makes fish CATCH SIGINT and keep looping,
# while the running child — which resets to the default SIGINT
# disposition on exec — still dies (exit 130), exactly like Ctrl+C at an
# interactive prompt. Job control (best-effort) additionally separates
# child process groups so background jobs / fg / bg have a chance to work.
status job-control full 2>/dev/null
function __termica_on_sigint --on-signal SIGINT
    # Intentionally empty: swallow SIGINT so the loop survives while the
    # interrupted foreground command exits.
end

# Determine integration protocol version (env-provided, default 1).
set -l __termica_version $TERMICA_INTEGRATION_VERSION
test -z "$__termica_version"; and set __termica_version 1

# Emit the gate-opening lifecycle message.
termica_emit_raw integration_ready "{\"shell\":\"fish\",\"version\":$__termica_version}"

set -e __termica_version

# We are now sitting at a prompt: tell Termica (this promotes the pane
# into `ShellPromptEditor` so the editor opens) and report the live
# variable names for `$VAR` completion.
termica_emit_string precmd "$PWD"
termica_emit_vars

# ----- read-eval loop ----------------------------------------------------
#
# Termica writes each finished command line to our stdin, terminated by
# CR (`\r`, mapped to NL by the tty's ICRNL). We read it, announce
# preexec, run it, announce command_finished + the next precmd. This is
# the non-interactive equivalent of fish's interactive prompt cycle.
#
# We deliberately do NOT use fish's `read` builtin: on a tty it runs
# fish's own line editor (a `read>` prompt + per-keystroke echo) — the
# very reader we're avoiding. Instead a one-shot POSIX `sh` reads one
# raw line in the tty's cooked mode (no editor, no prompt; the kernel
# echo is dropped by Termica's `EchoSuppressor`) and signals EOF
# (Ctrl+D) via its exit status so we can leave the loop and exit fish.
#
# (v1 handles single-line commands; a multi-line command built with
# Shift+Enter in the editor is a known follow-up — it needs sentinel
# framing so the loop reads it as one unit instead of line-by-line.)
while true
    set -l __termica_cmd (sh -c 'IFS= read -r line; rc=$?; printf %s "$line"; exit $rc')
    test $status -ne 0; and break
    termica_emit_string preexec "$__termica_cmd"
    eval $__termica_cmd
    set -l __termica_status $status
    termica_emit_int command_finished $__termica_status
    termica_emit_string precmd "$PWD"
    termica_emit_vars
end
