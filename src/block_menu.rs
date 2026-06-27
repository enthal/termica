//! The block right-click context menu (spec/07 §"Per-block affordances").
//!
//! Right-clicking a sealed block offers **Copy block**, **Copy command**,
//! and **Copy output**. When the click landed on one of the block's header
//! chips, a **Copy <chip>** item plus a separator are *prepended*, so the
//! user can lift just that chip's value (e.g. the git branch) without
//! selecting it by hand.
//!
//! The menu's *content* — which entries, in what order, and what each one
//! copies — is computed by the pure [`block_context_menu_entries`] so it can
//! be unit-tested without egui. [`show_block_context_menu`] is the thin egui
//! rendering shim that turns those entries into buttons; the chip hit-test
//! that decides whether a chip item is prepended lives in
//! [`crate::render::chip_at`].

#![forbid(unsafe_code)]

use crate::terminal::StyledLine;

/// One row of a block's right-click context menu.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockMenuEntry {
    /// A clickable item: `label` is shown; choosing it copies `clipboard`
    /// to the system clipboard.
    Action { label: String, clipboard: String },
    /// A horizontal divider between groups of items.
    Separator,
}

/// Join a sealed block's output snapshot into clipboard text: one line per
/// logical row, trailing whitespace trimmed (the grid's space-padding never
/// reaches the clipboard — the same rule
/// [`crate::block_selection::block_selection_text`] applies to selections),
/// and trailing blank lines dropped.
pub fn block_output_text(snapshot: &[StyledLine]) -> String {
    let mut lines: Vec<String> = snapshot
        .iter()
        .map(|l| l.text_chars().collect::<String>().trim_end().to_string())
        .collect();
    while matches!(lines.last(), Some(s) if s.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// The whole block as clipboard text: the command line(s) followed by the
/// output, separated by a single newline. Either half is omitted when empty
/// so a command with no output (or output with no command) copies cleanly
/// without a stray leading / trailing blank line.
pub fn block_full_text(command: &str, snapshot: &[StyledLine]) -> String {
    let output = block_output_text(snapshot);
    match (command.is_empty(), output.is_empty()) {
        (true, _) => output,
        (_, true) => command.to_string(),
        (false, false) => format!("{command}\n{output}"),
    }
}

/// Prepend the optional `Copy <chip>` action plus a divider — the chip the
/// right-click landed on, if any. Shared by the sealed and running block menus.
fn prepend_chip(entries: &mut Vec<BlockMenuEntry>, chip: Option<(&str, &str)>) {
    if let Some((name, value)) = chip {
        entries.push(BlockMenuEntry::Action {
            label: format!("Copy {name}"),
            clipboard: value.to_string(),
        });
        entries.push(BlockMenuEntry::Separator);
    }
}

/// Build the context-menu entries for a sealed block. `chip` is the
/// `(name, value)` of the header chip the right-click landed on, if any —
/// when present, a `Copy <name>` action and a separator are prepended before
/// the always-present block / command / output items.
pub fn block_context_menu_entries(
    command: &str,
    snapshot: &[StyledLine],
    chip: Option<(&str, &str)>,
) -> Vec<BlockMenuEntry> {
    let mut entries = Vec::new();
    prepend_chip(&mut entries, chip);
    entries.push(BlockMenuEntry::Action {
        label: "Copy block".to_string(),
        clipboard: block_full_text(command, snapshot),
    });
    entries.push(BlockMenuEntry::Action {
        label: "Copy command".to_string(),
        clipboard: command.to_string(),
    });
    entries.push(BlockMenuEntry::Action {
        label: "Copy output".to_string(),
        clipboard: block_output_text(snapshot),
    });
    entries
}

/// Build the context-menu entries for a *running* block. The command is still
/// executing, so `output` is a best-effort snapshot of the live grid taken at
/// click time (not a frozen sealed snapshot). A prepended `Copy <chip>` +
/// separator is added when the right-click landed on a header chip.
///
/// - **Normal output** (`alt_screen == false`): the usual **Copy block** /
///   **Copy command** / **Copy output**, where block / output reflect the
///   bytes printed so far.
/// - **Alternate screen** (`alt_screen == true`, e.g. vim / htop / less): the
///   grid is a full-screen TUI, not a command transcript, so there is no
///   meaningful "block". The menu offers **Copy command** and **Copy screen**
///   (the visible grid) — no Copy block.
pub fn running_context_menu_entries(
    command: &str,
    output: &[StyledLine],
    alt_screen: bool,
    chip: Option<(&str, &str)>,
) -> Vec<BlockMenuEntry> {
    let mut entries = Vec::new();
    prepend_chip(&mut entries, chip);
    if alt_screen {
        entries.push(BlockMenuEntry::Action {
            label: "Copy command".to_string(),
            clipboard: command.to_string(),
        });
        entries.push(BlockMenuEntry::Action {
            label: "Copy screen".to_string(),
            clipboard: block_output_text(output),
        });
    } else {
        entries.push(BlockMenuEntry::Action {
            label: "Copy block".to_string(),
            clipboard: block_full_text(command, output),
        });
        entries.push(BlockMenuEntry::Action {
            label: "Copy command".to_string(),
            clipboard: command.to_string(),
        });
        entries.push(BlockMenuEntry::Action {
            label: "Copy output".to_string(),
            clipboard: block_output_text(output),
        });
    }
    entries
}

/// Render the entries as an egui context menu. Each
/// [`BlockMenuEntry::Action`] is a button that copies its payload and closes
/// the menu; a [`BlockMenuEntry::Separator`] is a divider.
pub fn show_block_context_menu(ui: &mut egui::Ui, entries: &[BlockMenuEntry]) {
    for entry in entries {
        match entry {
            BlockMenuEntry::Separator => {
                ui.separator();
            }
            BlockMenuEntry::Action { label, clipboard } => {
                if ui.button(label).clicked() {
                    ui.ctx().copy_text(clipboard.clone());
                    ui.close();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::StyledCell;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    fn cell(c: char) -> StyledCell {
        StyledCell {
            c,
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }

    fn line(s: &str) -> StyledLine {
        StyledLine { cells: s.chars().map(cell).collect() }
    }

    fn snap(rows: &[&str]) -> Vec<StyledLine> {
        rows.iter().map(|r| line(r)).collect()
    }

    /// Helper: pull the clipboard payload of the `Action` with this label.
    fn clip<'a>(entries: &'a [BlockMenuEntry], label: &str) -> &'a str {
        entries
            .iter()
            .find_map(|e| match e {
                BlockMenuEntry::Action { label: l, clipboard } if l == label => {
                    Some(clipboard.as_str())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no entry labelled {label:?}"))
    }

    #[test]
    fn output_text_trims_trailing_space_padding_per_row() {
        // Grid rows carry trailing space padding; it must not reach the
        // clipboard.
        let s = snap(&["hello     ", "world  "]);
        assert_eq!(block_output_text(&s), "hello\nworld");
    }

    #[test]
    fn output_text_drops_trailing_blank_lines_but_keeps_interior_ones() {
        let s = snap(&["one", "", "two", "", "   "]);
        assert_eq!(block_output_text(&s), "one\n\ntwo");
    }

    #[test]
    fn full_text_joins_command_and_output_with_one_newline() {
        let s = snap(&["line-a", "line-b"]);
        assert_eq!(block_full_text("ls -la", &s), "ls -la\nline-a\nline-b");
    }

    #[test]
    fn full_text_omits_empty_halves() {
        // Output only (empty command) → no leading newline.
        assert_eq!(block_full_text("", &snap(&["only-output"])), "only-output");
        // Command only (empty output) → no trailing newline.
        assert_eq!(block_full_text("echo hi", &snap(&["", "  "])), "echo hi");
    }

    #[test]
    fn multiline_command_is_preserved_in_block_and_command_copies() {
        let s = snap(&["out"]);
        let entries = block_context_menu_entries("for x in 1 2\ndo echo $x\ndone", &s, None);
        assert_eq!(clip(&entries, "Copy command"), "for x in 1 2\ndo echo $x\ndone");
        assert_eq!(clip(&entries, "Copy block"), "for x in 1 2\ndo echo $x\ndone\nout");
    }

    #[test]
    fn entries_without_chip_are_block_command_output_in_order() {
        let s = snap(&["the-output"]);
        let entries = block_context_menu_entries("the-cmd", &s, None);
        let labels: Vec<&str> = entries
            .iter()
            .map(|e| match e {
                BlockMenuEntry::Action { label, .. } => label.as_str(),
                BlockMenuEntry::Separator => "<sep>",
            })
            .collect();
        assert_eq!(labels, ["Copy block", "Copy command", "Copy output"]);
        assert_eq!(clip(&entries, "Copy block"), "the-cmd\nthe-output");
        assert_eq!(clip(&entries, "Copy command"), "the-cmd");
        assert_eq!(clip(&entries, "Copy output"), "the-output");
    }

    #[test]
    fn entries_with_chip_prepend_copy_chip_then_a_separator() {
        let s = snap(&["out"]);
        let entries = block_context_menu_entries("cmd", &s, Some(("git branch", "feat/x")));
        // First item is the chip copy, second is the divider, then the
        // standard three follow — exactly the order the user specified.
        let labels: Vec<&str> = entries
            .iter()
            .map(|e| match e {
                BlockMenuEntry::Action { label, .. } => label.as_str(),
                BlockMenuEntry::Separator => "<sep>",
            })
            .collect();
        assert_eq!(
            labels,
            ["Copy git branch", "<sep>", "Copy block", "Copy command", "Copy output"]
        );
        assert_eq!(clip(&entries, "Copy git branch"), "feat/x");
    }

    /// Collect entry labels (with `<sep>` for dividers) for order assertions.
    fn labels(entries: &[BlockMenuEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|e| match e {
                BlockMenuEntry::Action { label, .. } => label.as_str(),
                BlockMenuEntry::Separator => "<sep>",
            })
            .collect()
    }

    #[test]
    fn running_normal_output_offers_block_command_output_from_the_live_snapshot() {
        let out = snap(&["building...", "linking"]);
        let entries = running_context_menu_entries("make build", &out, false, None);
        assert_eq!(labels(&entries), ["Copy block", "Copy command", "Copy output"]);
        assert_eq!(clip(&entries, "Copy block"), "make build\nbuilding...\nlinking");
        assert_eq!(clip(&entries, "Copy command"), "make build");
        assert_eq!(clip(&entries, "Copy output"), "building...\nlinking");
    }

    #[test]
    fn running_alt_screen_offers_copy_command_and_copy_screen_no_block() {
        // vim / htop: the grid is a TUI, not a transcript — "Copy screen",
        // and no "Copy block".
        let screen = snap(&["~  vim buffer", "~"]);
        let entries = running_context_menu_entries("vim notes.md", &screen, true, None);
        assert_eq!(labels(&entries), ["Copy command", "Copy screen"]);
        assert!(!labels(&entries).contains(&"Copy block"), "alt-screen has no Copy block");
        assert_eq!(clip(&entries, "Copy command"), "vim notes.md");
        assert_eq!(clip(&entries, "Copy screen"), "~  vim buffer\n~");
    }

    #[test]
    fn running_prepends_copy_chip_then_a_separator_in_both_modes() {
        let out = snap(&["x"]);
        let normal = running_context_menu_entries("c", &out, false, Some(("git branch", "feat/y")));
        assert_eq!(
            labels(&normal),
            ["Copy git branch", "<sep>", "Copy block", "Copy command", "Copy output"]
        );
        let alt = running_context_menu_entries("c", &out, true, Some(("git branch", "feat/y")));
        assert_eq!(labels(&alt), ["Copy git branch", "<sep>", "Copy command", "Copy screen"]);
        assert_eq!(clip(&alt, "Copy git branch"), "feat/y");
    }
}
