# fzf

fzf turns on alt-screen AND bracketed-paste mode (`\e[?2004h`) so the
shell-side line discipline doesn't mangle a pasted query. Both flags
need to live in [`TerminalModes`](../../../src/terminal.rs) for the
input encoder to do the right thing on paste events.

The scenario leaves both flags on at the end, so the snapshot pins
the dual-mode state.

Verifies:
- `alt_screen` flag flips to `true`.
- `bracketed_paste` flag flips to `true` ([#23](https://github.com/enthal/termica/pull/23)
  is the test fixture this guards against regressing).
- Selection highlight (reverse video on the matching row) lands.
