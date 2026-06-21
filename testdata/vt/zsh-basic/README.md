# zsh-basic

Same shape as `bash-basic` but with zsh's `%` prompt convention.

Two-baseline coverage of the same flow protects against an accidental "bash works, zsh doesn't" divergence — we're not aware of any plausible parser bug that would behave that way, but the cost of carrying the extra snapshot is trivial and the failure mode would be hard to debug without it.
