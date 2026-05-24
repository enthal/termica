# ssh

Once an SSH connection is up, all bytes are passthrough — Termica's
engine should treat the remote shell's output identically to a local
shell's. No alt-screen, no DECCKM, no bracketed paste.

Verifies:
- A login banner + remote prompt + command output + new prompt
  produce the obvious 4-row grid.
- None of the mode flags flip — the engine has no special "ssh mode"
  and shouldn't pretend to.
