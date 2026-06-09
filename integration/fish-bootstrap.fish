# Termica fish bootstrap, version 1.
#
# Termica spawns fish with:
#   fish --no-config --init-command "$BOOTSTRAP" -i
#
# The init-command runs before the first prompt. The bootstrap:
#   1. Defines Termica helpers (fish syntax, not POSIX).
#   2. Sources ~/.config/fish/config.fish if it exists.
#   3. Sources each ~/.config/fish/conf.d/*.fish in name order.
#   4. Defines lifecycle event handlers.
#   5. Emits integration_ready.
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

# ----- lifecycle handlers ------------------------------------------------

function termica_preexec --on-event fish_preexec
    termica_emit_string preexec "$argv"
end

function termica_postexec --on-event fish_postexec
    # Capture $status FIRST. fish puts the exit status of the just-
    # finished command in $status.
    set -l exit_status $status
    termica_emit_int command_finished $exit_status
end

function termica_prompt_hook --on-event fish_prompt
    termica_emit_string precmd "$PWD"
    # Live `$VAR`-completion source (change-gated, names only).
    termica_emit_vars
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

# TODO: Phase 4C polish. Equivalent to the zsh/bash treatment —
# clear `fish_prompt` / `fish_right_prompt` and disable fish's
# built-in line editor so Termica's `PromptEditor` is the only line
# editor active and our `EchoSuppressor` works against the kernel
# echo. fish's edit model is more idiomatic than bash readline /
# zsh ZLE so this needs a small design pass. Until then, fish panes
# work but display zsh-style duplicated echo from fish's own line
# editor.

# Determine integration protocol version (env-provided, default 1).
set -l __termica_version $TERMICA_INTEGRATION_VERSION
test -z "$__termica_version"; and set __termica_version 1

# Emit the gate-opening lifecycle message.
termica_emit_raw integration_ready "{\"shell\":\"fish\",\"version\":$__termica_version}"

set -e __termica_version
