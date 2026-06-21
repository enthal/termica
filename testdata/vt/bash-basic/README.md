# bash-basic

A vanilla bash interaction: prompt, the user runs `ls`, output, new prompt.

Verifies:
- Plain text passes through unaltered.
- `\r\n` advances to a fresh row at column 0.
- Cursor lands at column 12 of row 2 (just past `"tim@host:~$ "`).
- No mode flags flip (this is the regression check for "nothing fancy in the byte stream should ever activate alt-screen, DECCKM, or bracketed paste").
