# htop

Dense SGR-coloured output on the alternate screen: a header row in
reverse video, a CPU bar using the 256-colour palette
(`\e[38;5;46m` = bright green, `\e[38;5;240m` = dim grey), a memory
bar in a similar style, and a process row.

The snapshot in `grid.snap` is text-only — it doesn't carry colour
because `screen_text()` discards SGR state. The point of this scenario
is to verify the engine **doesn't corrupt cell contents** when SGR
codes are flying around. Visual colour fidelity is covered by the
`tests/snapshots/terminal_ansi_colors.png` snapshot test.

Verifies:
- 256-colour `\e[38;5;Nm` sequences parse without spilling into the
  cell stream.
- SGR resets (`\e[0m`) cleanly stop attribute runs.
- Reverse-video lands and resets correctly.
