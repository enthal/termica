# Termica zsh OSC 7 hook.
#
# Emits OSC 7 (current working directory) so Termica can display the
# shell's cwd in its status header and resolve relative paths in
# clickable links. The protocol:
#
#   ESC ] 7 ; file://<host><url-encoded-path> ESC \
#
# Same shape Apple_Terminal / iTerm2 / WezTerm use, so other terminals
# that source this file will read it as a no-op (OSC 7 is widely
# understood). Termica's Phase 3 integration script will provide more
# (prompt markers, command lifecycle), but OSC 7 is the only thing
# this file does.

__termica_emit_osc7() {
    local URL_PATH=""
    local i ch hexch
    # Re-set LC_* inside the loop to keep `printf '%02X' "'$ch"`
    # deterministic across multibyte locales.
    local LC_CTYPE=C LC_COLLATE=C
    for ((i = 1; i <= ${#PWD}; ++i)); do
        ch="$PWD[i]"
        if [[ "$ch" =~ [/._~A-Za-z0-9-] ]]; then
            URL_PATH+="$ch"
        else
            hexch=$(printf "%02X" "'$ch")
            URL_PATH+="%$hexch"
        fi
    done
    printf '\e]7;file://%s%s\e\\' "${HOST:-localhost}" "$URL_PATH"
}

# Wire the hook to run after every `cd`, and emit once at shell
# startup so Termica gets the initial cwd without waiting for a
# directory change.
autoload -Uz add-zsh-hook
add-zsh-hook -Uz chpwd __termica_emit_osc7
__termica_emit_osc7
