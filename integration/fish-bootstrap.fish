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
#
# `string collect` after every substitution is REQUIRED: fish's command
# substitution `(...)` splits its output on newlines, so without it the
# newlines in a multi-line command would be dropped by the FIRST
# substitution below (and re-joined with spaces by `printf`) before the
# `\n`-escaping step ever ran — silently flattening a multi-line command
# in the `preexec` marker (and thus in history).
function termica_escape_json
    set -l s $argv[1]
    # Order matters: escape backslash first, then quote, then control chars.
    set s (string replace -a '\\' '\\\\' -- $s | string collect)
    set s (string replace -a '"' '\\"' -- $s | string collect)
    set s (string replace -a \n '\\n' -- $s | string collect)
    set s (string replace -a \r '\\r' -- $s | string collect)
    set s (string replace -a \t '\\t' -- $s | string collect)
    printf '%s' $s
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

# Answer a live-shell completion request: run `complete -C` IN THIS
# process (so the user's runtime-defined aliases / functions /
# abbreviations are visible — a fresh `fish -c` subprocess can't see them)
# and emit a `completion` marker carrying the request `id` and the RAW
# `complete -C` output lines as a JSON string array. The split into
# value/description is done once, in Rust (`parse_fish_complete`), so the
# live path and the one-shot subprocess path can't diverge — important
# because some tools (kubectl) emit space-padded columns, not tabs.
#
# This is INERT to Termica's mode machine: no preexec / command_finished /
# precmd is emitted, and the caller does not `eval` — the loop just emits
# this and reads again, staying at the prompt (spec/05).
function termica_emit_completion
    set -l id $argv[1]
    set -l line $argv[2]
    set -l json "["
    set -l first 1
    # `complete -C $line`: one variable → one arg, matching the one-shot
    # driver invocation. Command substitution splits the output on
    # newlines, so each candidate line becomes one list element; emit each
    # verbatim (JSON-escaped) for Rust to parse.
    for c in (complete -C $line)
        set -l esc (termica_escape_json $c)
        if test $first -eq 1
            set first 0
        else
            set json "$json,"
        end
        set json "$json\"$esc\""
    end
    set json "$json]"
    set -l session $TERMICA_SESSION_ID
    test -z "$session"; and set session ""
    # Note the extra top-level `id` field (beyond type/session/value), so
    # this can't reuse `termica_emit_raw`.
    printf '\033PTermica;{"type":"completion","session":"%s","id":%s,"value":%s}\033\\' \
        $session $id $json
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
# Termica sends each command **base64-encoded** (one tty line, no embedded
# newlines), so a multi-line command (Shift+Enter, a `for`…`end` block, a
# trailing `&&`) arrives intact on a single line and `eval`s as one unit —
# instead of being split line-by-line by the cooked-mode tty. Decode with
# `base64 -d` (portable across macOS/Linux) and `string collect
# --allow-empty` to keep a multi-line value (and the empty command) as a
# single argument. base64's alphabet has no tty-special bytes, so the
# kernel echo is dropped by `EchoSuppressor` exactly like plain text.
# A line can be a COMMAND (pure base64) or a live-shell COMPLETION REQUEST
# (`complete\t<id>\t<base64-line>`). They're disambiguated by a leading
# TAB-delimited `complete` field — a base64 command never contains a TAB.
# A completion request runs `complete -C` in THIS live shell and emits a
# `completion` marker WITHOUT executing anything (no preexec/command_finished/
# precmd), so it's inert to the pane-mode machine (spec/03 §completion).
while true
    set -l __termica_line (sh -c 'IFS= read -r line; rc=$?; printf %s "$line"; exit $rc')
    test $status -ne 0; and break

    # Split on the FIRST tab. A completion request is `complete\t<id>\t<b64>`;
    # a command is a single field (no tab).
    set -l __termica_head (string split -m1 \t -- $__termica_line)
    if test (count $__termica_head) -gt 1; and test "$__termica_head[1]" = complete
        # `__termica_head[2]` is `<id>\t<b64-line>`; split off the id.
        set -l __termica_req (string split -m1 \t -- $__termica_head[2])
        set -l __termica_cid $__termica_req[1]
        set -l __termica_cline (printf '%s' "$__termica_req[2]" | base64 -d 2>/dev/null | string collect --allow-empty)
        termica_emit_completion $__termica_cid "$__termica_cline"
        continue
    end

    # NOTE: `printf '%s'` WITHOUT `--`: fish's `printf` does not treat `--`
    # as end-of-options, it prints it literally (corrupting the base64).
    # The payload is base64 (`A–Za–z0–9+/=`), so it never starts with `-`.
    set -l __termica_cmd (printf '%s' "$__termica_line" | base64 -d 2>/dev/null | string collect --allow-empty)
    termica_emit_string preexec "$__termica_cmd"
    eval $__termica_cmd
    set -l __termica_status $status
    termica_emit_int command_finished $__termica_status
    termica_emit_string precmd "$PWD"
    termica_emit_vars
end
