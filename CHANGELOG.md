# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-06-30

### Added

- Restore the OS window's size and position on relaunch, clamped to the current monitor so a workspace saved on a large display reopens usably on a smaller one ([#193](https://github.com/enthal/termica/pull/193)).
- Seal shell-init (dotfile) output into its own chip-less, output-only block instead of prepending it to the first command; the blank-pane watermark now lingers behind that init output until the first command runs ([#195](https://github.com/enthal/termica/pull/195)).
- Pin the "Restart shell" affordance to the bottom of a dead pane so the restored transcript stays maximally visible ([#192](https://github.com/enthal/termica/pull/192)).
- Per-platform download panel and stable README download links on the site ([#190](https://github.com/enthal/termica/pull/190)).
- Version-less, arch-only release asset names and stable `releases/latest/download/<asset>` URLs ([#189](https://github.com/enthal/termica/pull/189)).

### Fixed

- Restore **all** panes on restart (not just one) and stop a fresh pane opening at `/`: default a fresh pane's cwd to `$HOME`, persist layout on change rather than only on quit, and make restore resilient to a single bad pane ([#191](https://github.com/enthal/termica/pull/191)).
- Opaque command-area card: backing fill, flush clip, headroom, and padded pinned header ([#194](https://github.com/enthal/termica/pull/194)).
- Create the GitHub Release once per tag, not once per build-matrix job ([#188](https://github.com/enthal/termica/pull/188)).

[0.2.0]: https://github.com/enthal/termica/compare/v0.1.0...v0.2.0
