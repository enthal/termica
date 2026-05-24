# vim

Alternate-screen lifecycle as `vim` would drive it: CSI `?1049h`
swaps to the alternate buffer, the engine paints a tilde column down
the left margin plus a reverse-video status line on the last row.

The scenario deliberately does NOT exit the alt-screen at the end —
the snapshot pins the *mid-edit* visual state the user actually sees.
Clean-exit behaviour (alt-screen flag → false, main-screen content
restored byte-for-byte) is covered by unit tests in
[src/terminal.rs](../../../src/terminal.rs).

Verifies:
- `alt_screen` flag flips to `true`.
- Tildes land on rows 0..=8 (the engine's `screen_lines - 1`).
- The reverse-video status line lands on row 9.
- Cursor parks at the end of the status line.
