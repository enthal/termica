//! Per-pane rendering: PTY resize, link / path scan, block stack,
//! terminal grid paint, mouse selection, scroll wheel, keyboard
//! input, app-shortcut detection, and fixed-footer editor. Same
//! code path runs for every visible pane regardless of layout
//! (single tab or split into multiple panes).

use std::path::Path;

use alacritty_terminal::grid::Dimensions;
use eframe::egui;

use crate::links::{self, LinkSpan};
use crate::pane_slot::PaneSlot;
use crate::paths;
use crate::render;
use crate::selection::{self, SelectionMode, pixel_to_grid_point};
use crate::shortcuts::match_pane_shortcut;
use crate::{MIN_COLS, MIN_ROWS, input};

/// Max gap (seconds) between mousedowns to register as a multi-click.
const MULTI_CLICK_WINDOW_SECS: f64 = 0.5;

/// Max pixel distance the pointer can drift between mousedowns and
/// still register as a multi-click.
const MULTI_CLICK_DISTANCE_PX: f32 = 8.0;

/// Color of the 1px border painted around the cell grid while a pane
/// is in alternate-screen mode (vim / htop / less / fzf / ssh-with-
/// TUI). A muted teal that's clearly visible against the dark
/// background but doesn't read as a warning — the signal is "this
/// pane is owned by a full-screen TTY program," not "something is
/// wrong." Tuned in unit tests so future tweaks land deliberately,
/// not by accident.
pub const ALT_SCREEN_BORDER_COLOR: egui::Color32 = egui::Color32::from_rgb(0x5b, 0xa3, 0xb8);

/// Stroke width of the alt-screen border, in egui logical pixels.
pub const ALT_SCREEN_BORDER_WIDTH: f32 = 1.0;

/// How long the visible-bell flash stays on screen after a `\a`
/// (BEL) byte reaches the terminal. Long enough that the user
/// notices it even if the next frame is idle; short enough that a
/// noisy stream of bells (some programs ring on every typo)
/// doesn't strobe the pane chrome.
pub const BELL_FLASH_SECS: f64 = 0.25;

/// Colour of the visible-bell border. Saturated warm orange —
/// reads as "attention" without the literal-alarm of red. Painted
/// at the listed alpha at the start of the flash; the alpha fades
/// linearly to zero across `BELL_FLASH_SECS`.
pub const BELL_FLASH_COLOR: egui::Color32 = egui::Color32::from_rgb(0xf2, 0xa0, 0x4a);

/// Width of the visible-bell border, in egui logical pixels.
pub const BELL_FLASH_WIDTH: f32 = 3.0;

/// Paint a visible-bell border around `pane_rect` with the linear-
/// fade alpha appropriate for the elapsed time since the flash
/// started. `alpha_factor` is `1.0` at flash start, `0.0` at end.
/// Public so a snapshot test can drive it directly without
/// constructing a real `PaneSession`.
pub fn paint_bell_flash_border(painter: &egui::Painter, pane_rect: egui::Rect, alpha_factor: f32) {
    if alpha_factor <= 0.0 {
        return;
    }
    let a = (alpha_factor.clamp(0.0, 1.0) * 255.0) as u8;
    let color = egui::Color32::from_rgba_unmultiplied(
        BELL_FLASH_COLOR.r(),
        BELL_FLASH_COLOR.g(),
        BELL_FLASH_COLOR.b(),
        a,
    );
    painter.rect_stroke(
        pane_rect,
        egui::CornerRadius::ZERO,
        egui::Stroke::new(BELL_FLASH_WIDTH, color),
        egui::StrokeKind::Inside,
    );
}

/// Paint the alt-screen indicator border around `grid_rect`.
///
/// The border is a 1px [`ALT_SCREEN_BORDER_COLOR`] stroke flush
/// against the grid edges. Painted on top of the cell content with
/// no rounding so it reads as a tight frame, not pane chrome.
///
/// `pub` so a snapshot test in [`tests/`] can drive this helper
/// directly against a known [`egui::Rect`] without instantiating a
/// real [`PaneSession`].
pub fn paint_alt_screen_border(painter: &egui::Painter, grid_rect: egui::Rect) {
    // egui's `rect_stroke` centres the stroke on the rect edge by
    // default. We want the stroke to read as a frame *inside* the
    // grid rect so the bottom-right glyphs aren't clipped by a
    // half-pixel of border bleed. `StrokeKind::Inside` does exactly
    // that.
    painter.rect_stroke(
        grid_rect,
        egui::CornerRadius::ZERO,
        egui::Stroke::new(ALT_SCREEN_BORDER_WIDTH, ALT_SCREEN_BORDER_COLOR),
        egui::StrokeKind::Inside,
    );
}

/// One word / line / document move within the editor, abstracted
/// away from the platform-specific key binding that triggered it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorMotion {
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
}

impl EditorMotion {
    /// Apply this motion to `editor`, extending the selection when
    /// `extending` is `true` (Shift was held).
    fn apply(self, editor: &mut crate::prompt_editor::PromptEditor, extending: bool) {
        match (self, extending) {
            (EditorMotion::WordLeft, false) => editor.move_word_left(),
            (EditorMotion::WordLeft, true) => editor.move_word_left_extending(),
            (EditorMotion::WordRight, false) => editor.move_word_right(),
            (EditorMotion::WordRight, true) => editor.move_word_right_extending(),
            (EditorMotion::LineStart, false) => editor.move_home(),
            (EditorMotion::LineStart, true) => editor.move_home_extending(),
            (EditorMotion::LineEnd, false) => editor.move_end(),
            (EditorMotion::LineEnd, true) => editor.move_end_extending(),
            (EditorMotion::DocStart, false) => editor.move_doc_start(),
            (EditorMotion::DocStart, true) => editor.move_doc_start_extending(),
            (EditorMotion::DocEnd, false) => editor.move_doc_end(),
            (EditorMotion::DocEnd, true) => editor.move_doc_end_extending(),
        }
    }
}

/// Extra vertical pixels added to the editor footer below the last
/// row, so glyph descenders (`p`, `g`, `y`, `q`) clear the bottom
/// of the pane instead of being clipped off. egui's `row_height`
/// from the monospace font sometimes returns just ascent + descent
/// with no leading, and the painter clips at the rect bottom — so
/// the visible footprint of the last row's descenders gets cut off
/// by one or two pixels. Empirically tuned; not a unit of the font.
pub const FOOTER_DESCENDER_PAD: f32 = 4.0;

/// Compute the fixed-footer height for the [`Block::Prompt`] block
/// per [spec/04 §"Layout"](../spec/04-prompt-editor.md#layout-fixed-footer-prompt-sticky-top-header).
///
/// The footer is the dedicated home for the editor: a cwd chip row
/// (if a cwd is known) on top of one row per editor line, plus
/// [`FOOTER_DESCENDER_PAD`] at the bottom so descenders don't clip.
/// When the tail is **not** a `Prompt` or the editor is inactive
/// (e.g., bootstrapping, alt-screen, mode demote), there is no
/// footer and the scroll area extends to the pane bottom — returns
/// `0.0`.
///
/// Pure function; takes `row_h` so the caller controls the
/// monospace row height it's currently painting with. The result
/// is in egui logical pixels.
pub fn compute_footer_height(
    tail_is_prompt: bool,
    editor_active: bool,
    editor_lines: usize,
    has_cwd: bool,
    row_h: f32,
) -> f32 {
    if !tail_is_prompt || !editor_active {
        return 0.0;
    }
    let editor_rows = editor_lines.max(1);
    let chrome_h = if has_cwd { row_h + 2.0 * render::CHIP_PAD_Y } else { 0.0 };
    chrome_h + editor_rows as f32 * row_h + FOOTER_DESCENDER_PAD
}

/// Translate a `(Key, Modifiers)` pair into an [`EditorMotion`] +
/// "extending" flag, honouring platform conventions:
///
/// - **macOS**: Option+arrow = word, Cmd+Left/Right = line, Cmd+Up/Down = doc.
/// - **Linux / Windows**: Ctrl+arrow = word, Ctrl+Home/End = doc.
///   Line-scope moves use the bare `Home` / `End` keys (already
///   handled by the editor's plain `move_home` / `move_end` path).
///
/// Bare `Shift` toggles "extending"; the result combines with the
/// motion. Returns `None` for any key/modifier combo that isn't a
/// boundary move.
fn classify_editor_motion(
    key: egui::Key,
    modifiers: egui::Modifiers,
    is_macos: bool,
) -> Option<(EditorMotion, bool)> {
    use egui::Key;
    let extending = modifiers.shift;
    if is_macos {
        // macOS: Option (alt) without Cmd → word. Cmd (command)
        // without Option → line / doc.
        if modifiers.alt && !modifiers.command {
            return match key {
                Key::ArrowLeft => Some((EditorMotion::WordLeft, extending)),
                Key::ArrowRight => Some((EditorMotion::WordRight, extending)),
                _ => None,
            };
        }
        if modifiers.command && !modifiers.alt {
            return match key {
                Key::ArrowLeft => Some((EditorMotion::LineStart, extending)),
                Key::ArrowRight => Some((EditorMotion::LineEnd, extending)),
                Key::ArrowUp => Some((EditorMotion::DocStart, extending)),
                Key::ArrowDown => Some((EditorMotion::DocEnd, extending)),
                _ => None,
            };
        }
    } else {
        // Linux / Windows: Ctrl drives both. Distinguish by key:
        // arrows = word, Home / End = doc.
        if modifiers.ctrl && !modifiers.alt {
            return match key {
                Key::ArrowLeft => Some((EditorMotion::WordLeft, extending)),
                Key::ArrowRight => Some((EditorMotion::WordRight, extending)),
                Key::Home => Some((EditorMotion::DocStart, extending)),
                Key::End => Some((EditorMotion::DocEnd, extending)),
                _ => None,
            };
        }
    }
    None
}

/// Is this event the EOF chord — Ctrl+D — the one keystroke that may
/// deliberately reach the PTY from an idle prompt editor (exit the
/// shell)? Pure so the routing rule is unit-testable without an egui
/// event loop.
///
/// Notably NOT Ctrl+C: in `ShellPromptEditor` the shell is idle at a
/// confirmed prompt, so there is no foreground program to interrupt —
/// a SIGINT would only print a cosmetic `^C`. Interrupting a running
/// program happens in `RawTerminal` (`editor_active = false`), where
/// every keystroke already passes.
///
/// `mac_cmd` is excluded so macOS `Cmd+D` never matches; `shift` is
/// excluded so a shifted combo never matches. On Linux/Windows egui
/// sets `command` for a Ctrl press but leaves `mac_cmd` false, so the
/// same check recognises `Ctrl+D` there too.
fn is_eof_chord(event: &egui::Event) -> bool {
    matches!(
        event,
        egui::Event::Key { key: egui::Key::D, pressed: true, modifiers, .. }
            if modifiers.ctrl && !modifiers.shift && !modifiers.alt && !modifiers.mac_cmd
    )
}

/// May this keystroke event be written to the PTY, given the pane's
/// editor state? Enforces spec/04's invariant at the input boundary:
/// while the editor owns the line (`ShellPromptEditor`, `editor_active`),
/// the ONLY keystroke that reaches the PTY is Ctrl+D (EOF) on an EMPTY
/// editor — to exit an idle shell. Every other keystroke belongs to the
/// editor and must not leak a raw byte to the shell: a stray control
/// byte (`Ctrl+X/Y/Z`, …) sits in the shell's input and resurfaces
/// prefixed to the next command's output, and `\x03` (Ctrl+C) aborts
/// the shell's line — neither has any business at an idle prompt.
///
/// When the editor is NOT active (`RawTerminal` / `AlternateScreen`)
/// every event passes — the program below owns stdin, and that is where
/// Ctrl+C → SIGINT interrupts a running program.
///
/// This is the single structural gate that replaces the former
/// per-letter consume list, which only covered a handful of chords
/// (`Ctrl+A/E/K/U/W/P/N/S/G`) and leaked the rest of the alphabet.
fn pty_passthrough_allowed(event: &egui::Event, editor_active: bool, editor_empty: bool) -> bool {
    if !editor_active {
        return true;
    }
    editor_empty && is_eof_chord(event)
}

/// Route one egui event to the pane's [`PromptEditor`](crate::prompt_editor).
/// Returns `true` when the editor consumed the event (so the caller
/// skips the PTY encoder path); `false` lets the caller try other
/// routes. Phase 4B handles a minimal set: text insertion, basic
/// cursor moves, `Backspace` / `Delete`, multiline `Shift+Enter`,
/// the placeholder `Enter`, and `Esc` (which demotes via
/// `PaneSession::leave_editor_esc`). History walk (Up/Down) and
/// shift-selection are deferred to 4F/4J.
fn apply_event_to_editor(event: &egui::Event, slot: &mut PaneSlot) -> bool {
    use egui::{Event, Key};
    match event {
        // Plain printable text from the OS IME / keyboard layout.
        // If a selection is active, the editor's insert paths
        // delete it first.
        //
        // Undo coalescing: single-char text (the typical "user
        // pressed one key") routes through `insert_char` so the
        // run of typed characters folds into one undo entry (spec/
        // 04 §"Undo / redo" coalescing rule). Multi-char text (IME
        // composition commits, batched delivery from system text
        // replacement, etc.) goes through `insert_str` as a single
        // `OpKind::Other` entry — one paste, one undo.
        Event::Text(s) => {
            slot.session.clear_history_recall();
            if let Some(editor) = slot.session.editor_mut() {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => editor.insert_char(c),
                    _ => editor.insert_str(s),
                }
            }
            true
        }
        // Cmd+V (and middle-click paste) — egui pre-resolves the
        // clipboard contents into this event. Editor consumes; the
        // (unused-here) PTY paste path stays for `RawTerminal`.
        Event::Paste(s) => {
            slot.session.clear_history_recall();
            if let Some(editor) = slot.session.editor_mut() {
                editor.insert_str(s);
            }
            true
        }
        Event::Key { key, pressed: true, modifiers, .. } => {
            // Cmd+A — select all. Handled BEFORE the generic
            // "any modifier → bypass" gate.
            if modifiers.command && !modifiers.alt && matches!(key, Key::A) {
                if let Some(editor) = slot.session.editor_mut() {
                    editor.select_all();
                }
                return true;
            }
            // Cmd+Z / Cmd+Shift+Z — undo / redo. Per spec/04
            // §"Undo / redo": Cmd is the "command" modifier on both
            // platforms (egui maps Ctrl→command on Linux/Windows),
            // so a single `modifiers.command` check covers both.
            // Editor consumes; the chord doesn't reach the PTY.
            // History recall is cleared because undo/redo is a
            // mutating op from the recall machinery's perspective.
            if modifiers.command && !modifiers.alt && matches!(key, Key::Z) {
                slot.session.clear_history_recall();
                if let Some(editor) = slot.session.editor_mut() {
                    if modifiers.shift {
                        editor.redo();
                    } else {
                        editor.undo();
                    }
                }
                return true;
            }
            // Ctrl+C is deliberately NOT handled here, and is fully
            // inert in the editor: the boundary gate
            // ([`pty_passthrough_allowed`]) swallows it whether the
            // editor is empty or not, so no `\x03` ever reaches the
            // shell. In `ShellPromptEditor` the shell is idle at a
            // confirmed prompt — there's nothing to interrupt, and a
            // SIGINT would only print a cosmetic `^C`. (Interrupting a
            // running program happens in `RawTerminal`, where the
            // editor is inactive and Ctrl+C passes straight through.)
            // It also never mutates the buffer — Ctrl+C never discards
            // a typed line.
            // Word / line / doc boundary moves. Bindings differ by
            // OS — `classify_editor_motion` encodes both conventions.
            if let Some((motion, extending)) =
                classify_editor_motion(*key, *modifiers, cfg!(target_os = "macos"))
                && let Some(editor) = slot.session.editor_mut()
            {
                motion.apply(editor, extending);
                return true;
            }
            // Word-grained delete. macOS: Option+Backspace deletes
            // left, Option+Fn+Delete deletes right. Linux: Ctrl
            // +Backspace / Ctrl+Delete. Handled BEFORE the generic
            // "alt / cmd → bypass" gate below so the editor still
            // sees the delete.
            let is_macos = cfg!(target_os = "macos");
            let word_delete_left = if is_macos {
                modifiers.alt && !modifiers.command && matches!(key, Key::Backspace)
            } else {
                modifiers.ctrl && !modifiers.alt && matches!(key, Key::Backspace)
            };
            let word_delete_right = if is_macos {
                modifiers.alt && !modifiers.command && matches!(key, Key::Delete)
            } else {
                modifiers.ctrl && !modifiers.alt && matches!(key, Key::Delete)
            };
            // Delete-to-line-start: macOS Cmd+Backspace (= the
            // platform's `cmd+delete` per Apple's nomenclature),
            // Linux/Windows Ctrl+U (readline / emacs convention).
            // Joins onto the previous line when the caret is at the
            // start of a non-first line; no-op at byte 0 of the
            // buffer.
            let delete_to_line_start = if is_macos {
                modifiers.command && !modifiers.alt && matches!(key, Key::Backspace)
            } else {
                modifiers.ctrl && !modifiers.alt && !modifiers.shift && matches!(key, Key::U)
            };
            if word_delete_left {
                slot.session.clear_history_recall();
                if let Some(editor) = slot.session.editor_mut() {
                    editor.delete_word_left();
                }
                return true;
            }
            if word_delete_right {
                slot.session.clear_history_recall();
                if let Some(editor) = slot.session.editor_mut() {
                    editor.delete_word_right();
                }
                return true;
            }
            if delete_to_line_start {
                slot.session.clear_history_recall();
                if let Some(editor) = slot.session.editor_mut() {
                    editor.delete_to_line_start();
                }
                return true;
            }
            // Alt + Cmd combos (other than the moves above) bypass
            // the editor — they belong to app-level shortcuts. Plain
            // Cmd combos (Cmd+C/X) are handled in render_pane proper
            // because they need `ctx` for clipboard access.
            if modifiers.command || modifiers.alt {
                return false;
            }
            // `Ctrl+R` opens the history overlay. If the editor
            // already has text, prefill the search box with it so
            // a partial command pre-narrows the results — same UX
            // as zsh's incremental history-search-backward.
            if modifiers.ctrl && !modifiers.shift && matches!(key, Key::R) {
                // Ctrl+R replaces a live completion popup with the
                // history overlay — having both up at once is
                // visually confusing and the overlay is the
                // user's clear intent.
                slot.ui.completion_popup = None;
                if let Some(history) = slot.session.history_ctx().cloned()
                    && let Some(mut overlay) = crate::history_overlay::HistoryOverlay::open(
                        &history,
                        slot.session.pane_id(),
                    )
                {
                    let prefill =
                        slot.session.editor_mut().map(|e| e.text().to_string()).unwrap_or_default();
                    if !prefill.is_empty() {
                        overlay.query = prefill;
                        let cwd = slot.session.terminal().cwd().map(|p| p.display().to_string());
                        overlay.rerank(cwd.as_deref());
                    }
                    slot.ui.history_overlay = Some(overlay);
                }
                return true;
            }
            // Emacs-style editing chords explicitly swallowed here so
            // they read as "editor territory, not yet wired" rather
            // than falling through. This is belt-and-braces: the
            // boundary gate ([`pty_passthrough_allowed`]) already
            // stops every non-signal chord from reaching the PTY, so
            // even chords NOT in this list (Ctrl+X/Y/Z, …) no longer
            // leak. `Ctrl+C` is absent: the gate swallows it
            // unconditionally (inert at an idle prompt). `Ctrl+D` is
            // absent too: the gate lets the encoder send EOF on an
            // empty editor.
            if modifiers.ctrl
                && !modifiers.shift
                && matches!(
                    key,
                    Key::A | Key::E | Key::K | Key::U | Key::W | Key::P | Key::N | Key::S | Key::G
                )
            {
                return true;
            }
            // The keys that own `slot.session` mutably (submit,
            // demote, history recall) are handled BEFORE the editor
            // borrow so the borrow checker sees a clean window.
            match key {
                Key::Enter if !modifiers.shift => {
                    let _ = slot.session.submit_editor_command();
                    // The user just submitted — force the scroll
                    // area to the bottom for several frames so the
                    // command's first output is visible even if the
                    // user had scrolled up to read older blocks.
                    // The multi-frame snap (vs single-frame) walks
                    // through the Preexec → Running transition,
                    // which adds a command-label row to the block
                    // stack one frame AFTER the submit. A single-
                    // frame snap landed before that transition and
                    // stick_to_bottom didn't reliably hold through
                    // it, leaving the user at the TOP of the new
                    // Running block. 6 frames covers the typical
                    // shell-roundtrip latency.
                    slot.ui.scroll_to_bottom_frames = 6;
                    return true;
                }
                Key::Escape => {
                    // Esc is intentionally inert in the editor: it just
                    // consumes the keystroke (so it never reaches the
                    // PTY) and does nothing else. It USED to demote
                    // `ShellPromptEditor → RawTerminal` via
                    // `leave_editor_esc`, but dropping into raw I/O on
                    // an Esc was confusing and looked like a bug, and
                    // it solved no problem in practice. The demote
                    // machinery (`PaneSession::leave_editor_esc` →
                    // `PromptController::leave_editor_esc`, spec/05) is
                    // deliberately left in place — unbound — in case a
                    // future gesture wants it. (Popups are dismissed by
                    // their own Esc interception earlier in the loop;
                    // this arm only runs when none is open.)
                    return true;
                }
                Key::ArrowUp if !modifiers.shift && !modifiers.alt && !modifiers.command => {
                    // Multiline-aware: if there's a previous editor
                    // line, move the caret up within the buffer
                    // (preserving column). Only on row 0 do we step
                    // into history. Per spec/04 §"History walk
                    // (Up/Down)".
                    let moved_within_editor =
                        slot.session.editor_mut().map(|e| e.move_up()).unwrap_or(false);
                    if !moved_within_editor {
                        slot.session.editor_history_prev();
                    }
                    return true;
                }
                Key::ArrowDown if !modifiers.shift && !modifiers.alt && !modifiers.command => {
                    let moved_within_editor =
                        slot.session.editor_mut().map(|e| e.move_down()).unwrap_or(false);
                    if !moved_within_editor {
                        slot.session.editor_history_next();
                    }
                    return true;
                }
                _ => {}
            }
            // From this point on, the keys mutate the editor. Any
            // edit cancels an in-progress `↑`/`↓` walk so the next
            // `↑` re-saves the (just-edited) buffer.
            let recall_clearing = matches!(key, Key::Backspace | Key::Delete | Key::Enter);
            if recall_clearing {
                slot.session.clear_history_recall();
            }
            let Some(editor) = slot.session.editor_mut() else { return false };
            match key {
                Key::Backspace => {
                    editor.backspace();
                    true
                }
                Key::Delete => {
                    editor.delete_forward();
                    true
                }
                Key::ArrowLeft => {
                    if modifiers.shift {
                        editor.move_left_extending();
                    } else {
                        editor.move_left();
                    }
                    true
                }
                Key::ArrowRight => {
                    if modifiers.shift {
                        editor.move_right_extending();
                    } else {
                        editor.move_right();
                    }
                    true
                }
                Key::Home => {
                    if modifiers.shift {
                        editor.move_home_extending();
                    } else {
                        editor.move_home();
                    }
                    true
                }
                Key::End => {
                    if modifiers.shift {
                        editor.move_end_extending();
                    } else {
                        editor.move_end();
                    }
                    true
                }
                Key::Enter => {
                    editor.insert_newline();
                    true
                }
                Key::Tab => true,
                _ => false,
            }
        }
        _ => false,
    }
}

/// Spawn the OS's "open this URL or path" handler.
///
/// macOS: `open <arg>`. Linux/BSD: `xdg-open <arg>`. The argument
/// is passed as a single `arg`, never interpolated into a shell
/// string, so PTY-controlled content cannot inject extra arguments.
fn open_url(url: &str) {
    let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if let Err(e) = std::process::Command::new(cmd).arg(url).spawn() {
        eprintln!("termica: failed to open {url:?}: {e}");
    }
}

/// Compute how many `(rows, cols)` fit into `avail` at the given
/// cell metrics, clamped to [`MIN_ROWS`] × [`MIN_COLS`]. Pure
/// function so the rounding / clamp policy is unit-testable.
pub fn cells_from_pixels(avail: egui::Vec2, cell_w: f32, row_h: f32) -> (u16, u16) {
    let cols = if cell_w > 0.0 { (avail.x / cell_w).floor().max(0.0) as u16 } else { MIN_COLS };
    let rows = if row_h > 0.0 { (avail.y / row_h).floor().max(0.0) as u16 } else { MIN_ROWS };
    (rows.max(MIN_ROWS), cols.max(MIN_COLS))
}

/// Render one pane into `ui`: status header, resize-PTY-to-cells,
/// link scan + hover, terminal grid, mouse selection, scroll wheel,
/// input encoding, copy shortcut.
///
/// Pulled out of the eframe `update` loop so the same code path
/// runs for every visible pane — Phase 2A's "one pane in one tab"
/// AND Phase 2B's "multiple panes across splits". The Behavior
/// callback per visible pane invokes this.
pub fn render_pane(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    slot: &mut PaneSlot,
    home: Option<&Path>,
    modal_open: bool,
    chrome_variant: crate::focused_chrome::ChromeVariant,
) {
    // Input routing in a multi-pane world:
    //
    // - Keyboard events go to the **focused** pane only. Clicking a
    //   pane gives it focus. Without this gate, every visible pane
    //   would write the same keystrokes to its own PTY — typing
    //   would duplicate across splits.
    // - Mouse wheel goes to whatever pane the pointer is **over**
    //   (NOT necessarily the focused one). This matches iTerm /
    //   tmux: you can scroll a non-focused pane just by hovering
    //   and turning the wheel.
    //
    // Both gates live below, after `paint_terminal` returns the
    // response we check.
    //
    // `modal_open` is the third gate: when a modal (close-confirm or
    // quit-confirm) is showing, the pane is **inert** — no keystrokes
    // to the PTY, no wheel scrolling, no mouse selection, no focus
    // grabs. `egui::Modal` renders later in the frame, so we cannot
    // rely on it to consume events that `ctx.input` reads here; we
    // have to gate explicitly.

    let view = slot.session.view();

    // ---- bootstrap suppression ----------------------------------
    //
    // While the pane is in `Bootstrapping` (spec/05), the integration
    // script is running inside the shell. Its output is parsed (so
    // the DCS-JSON lifecycle messages are observed) but NOT rendered:
    // it's noise the user doesn't need to see. Input is dropped too;
    // `PaneSession::write` enforces that side independently.
    //
    // We still resize the PTY so that when bootstrap completes the
    // shell already has the right dimensions for the first prompt.
    if view.is_bootstrapping {
        let avail = ui.available_size();
        let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
        let (cell_w, row_h) =
            ui.fonts_mut(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
        let (rows, cols) = cells_from_pixels(avail, cell_w, row_h);
        if slot.ui.last_size != Some((rows, cols)) {
            let _ = slot.session.resize(rows, cols);
            slot.ui.last_size = Some((rows, cols));
        }
        ui.allocate_ui(avail, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(avail.y / 3.0);
                ui.label(
                    egui::RichText::new("Starting shell…")
                        .size(14.0)
                        .color(egui::Color32::from_gray(140)),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("(Termica is loading shell integration.)")
                        .size(11.0)
                        .color(egui::Color32::from_gray(100)),
                );
            });
        });
        return;
    }

    // ---- resize PTY to fit available space ----------------------
    let avail = ui.available_size();
    let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
    let (cell_w, row_h) = ui.fonts_mut(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
    let (rows, cols) = cells_from_pixels(avail, cell_w, row_h);
    if slot.ui.last_size != Some((rows, cols)) {
        let _ = slot.session.resize(rows, cols);
        slot.ui.last_size = Some((rows, cols));
    }

    // ---- pane content: sealed blocks + live terminal -------------
    //
    // The pane is a stack of `Block`s ([`crate::block`]). Older
    // blocks are `Sealed` (frozen `Vec<StyledLine>` snapshots);
    // the tail block is the live `Term` (rendered by
    // [`render::paint_terminal`] exactly as before). Everything is
    // wrapped in a vertical `ScrollArea` that sticks to the bottom
    // so the live terminal is in view by default; the user scrolls
    // up to see older sealed blocks. ScrollArea handles mouse-wheel
    // scrolling natively in the non-alt-screen path; the alt-screen
    // path below intercepts the wheel and sends arrow keystrokes
    // instead.
    let modifier_held = ctx.input(|i| i.modifiers.command);
    let pointer_pos = ctx.input(|i| i.pointer.latest_pos());
    let selection = slot.session.selection().copied();
    // Read the pane-spanning sealed-block selection up front. The
    // paint pass below queries it per-block via
    // `PaneSelection::block_range_for` to derive the clipped overlay
    // range for each sealed block. The click / drag handler farther
    // down updates the selection on user input. The visual reflects
    // last frame's state — a one-frame lag is invisible.
    let pane_sel = slot.session.pane_selection().copied();

    // Alt-screen mode (vim / less / htop / fzf / ssh-with-TUI):
    // the program below owns the whole grid and manages its own
    // scrolling. Termica's block model and editor have no role
    // here — both are hidden so the user sees what they'd see in
    // any other modern terminal. The flag is re-evaluated per
    // frame; on exit we fall back to the normal block layout.
    let in_alt_screen = view.alt_screen;

    // Pane-background hover sentinel for focus-on-background-click.
    //
    // A `Sense::hover()` widget covering the full pane rect — NO
    // click sense, NO drag sense. This deliberately stays out of
    // egui's drag-candidate bookkeeping so it can't slow drags
    // down, can't compete with inner widgets for press
    // ownership, and can't form a focus-claim feedback loop with
    // the chrome-opacity animation (cf. the 2ad7eb2 → 9dad5ca
    // revert: a `Sense::click_and_drag` version of this widget
    // sent CPU to 100%).
    //
    // `Response::contains_pointer()` is egui-doc-defined as "true
    // if the pointer is over this widget AND no other widget is
    // covering this response rectangle" — so it's z-order aware.
    // When the user clicks on a sealed block, that block covers
    // this background and `contains_pointer()` is false here;
    // the inner-widget focus claim handles that case. When the
    // user clicks on truly empty pane space, no inner widget
    // covers the spot and `contains_pointer()` is true.
    //
    // Pairing this with `ctx.input.pointer.primary_pressed()`
    // (pure timing — "a primary press happened this frame
    // somewhere") gives "a press happened this frame, on an
    // uncovered area of this pane". Per spec/06 "Pointer
    // routing", `primary_pressed` is allowed as a pure timing
    // signal when paired with a per-widget Response signal that
    // does the routing. `contains_pointer()` is the routing
    // signal here.
    let pane_background_hover = ui.interact(
        ui.max_rect(),
        ui.id().with(("pane-background-hover", slot.session.pane_id())),
        egui::Sense::hover(),
    );
    // Capture each sealed block's painted sub-widget Responses
    // during the loop. The click / drag handler below routes
    // through these Responses (`response.clicked()`,
    // `response.dragged()`, `response.is_pointer_button_down_on()`,
    // `response.interact_pointer_pos()`) so egui's interaction
    // layer owns the routing — z-order, exclusive drag ownership,
    // overlap resolution. Reading global `ctx.input.primary_pressed`
    // + intersecting it with a stored rect would give the wrong
    // answer when a higher widget (egui_tiles' splitter resize
    // handle, a tab being dragged) covers the same pixel, because
    // the rect test doesn't know which widget egui assigned the
    // press to. Empty in alt-screen mode (we don't paint blocks at
    // all there).
    let mut sealed_block_renders: Vec<(crate::block::BlockId, render::SealedBlockRender)> =
        Vec::new();

    // Phase 4D — fixed-footer Prompt block. When the tail is a
    // `Prompt` AND the editor is active, reserve a strip at the
    // bottom for the editor + cwd chip; the scroll area shrinks
    // correspondingly. Per spec/04 §"Layout: fixed-footer prompt".
    let editor_lines = slot
        .session
        .blocks()
        .editor_on_tail()
        .map(|e| e.text().split('\n').count().max(1))
        .unwrap_or(1);
    let tail_is_prompt =
        matches!(slot.session.blocks().last(), Some(crate::block::Block::Prompt { .. }));
    let has_prompt_cwd = matches!(
        slot.session.blocks().last(),
        Some(crate::block::Block::Prompt { header, .. }) if header.cwd.is_some()
    );
    let footer_h = compute_footer_height(
        tail_is_prompt && !in_alt_screen,
        slot.session.editor_is_active() && !in_alt_screen,
        editor_lines,
        has_prompt_cwd,
        row_h,
    );
    let scroll_max_h = (ui.available_height() - footer_h).max(0.0);

    // Estimate the natural height of the block stack + (conditionally
    // painted) live `Term`, so the loop can prepend a top spacer that
    // bottom-aligns the content when it's shorter than the scroll
    // area. Without this, a fresh pane with one or two sealed blocks
    // shows those blocks at the *top* of the viewport with a tall
    // empty gap above the editor footer. Approximate: doesn't account
    // for every egui item-spacing pixel, but close enough that the
    // visual reads as "stuck to bottom" rather than "floating."
    // Whether the live `Term` will be painted in this frame. Mirrors
    // the `skip_live_term` decision inside the scroll closure: the
    // grid contributes its full row count to `content_h` whenever
    // it's actually painted, so the bottom-align spacer matches.
    let tail_is_prompt =
        matches!(slot.session.blocks().last(), Some(crate::block::Block::Prompt { .. }));
    let will_paint_live_term =
        in_alt_screen || !(tail_is_prompt && slot.session.editor_is_active());
    // Per-block chrome row (the cwd / exit chip) is taller than a
    // plain text row because the chip has vertical padding.
    let chip_h = row_h + 2.0 * render::CHIP_PAD_Y;
    let mut content_h: f32 = 0.0;
    if !in_alt_screen {
        for block in slot.session.blocks().iter() {
            match block {
                crate::block::Block::Sealed { command, snapshot, header, exit, .. } => {
                    let has_header = header.cwd.is_some() || matches!(exit, Some(n) if *n != 0);
                    let header_h = if has_header { chip_h } else { 0.0 };
                    let command_rows = command.split('\n').count();
                    let snap_rows = snapshot.len();
                    content_h += header_h + (command_rows + snap_rows) as f32 * row_h + 4.0;
                }
                crate::block::Block::Running { command, header, .. } => {
                    let header_h = if header.cwd.is_some() { chip_h } else { 0.0 };
                    let command_rows =
                        if !command.is_empty() { command.split('\n').count() } else { 0 };
                    content_h += header_h + command_rows as f32 * row_h;
                }
                crate::block::Block::Prompt { .. } => {}
            }
        }
    }
    // Live `Term` contributes its grid height whenever it's
    // actually painted — including alt-screen mode. The block loop
    // is gated on `!in_alt_screen` because no blocks paint in alt-
    // screen, but the live grid does and the spacer must account
    // for it so vim / less / htop don't get pushed offscreen.
    // Outside alt-screen mode the grid is rendered with its full
    // history inline so a running command's earlier output stays
    // visible; the spacer must include those rows too.
    //
    // The non-alt-screen height MUST match
    // `paint_terminal`'s `include_history=true` clamp at
    // `history + cursor_row + 1` — otherwise the bottom-align
    // spacer over-allocates by `screen_lines - cursor_row - 1`
    // rows and the live output floats well above the editor
    // footer with empty space below it.
    if will_paint_live_term {
        let grid = slot.session.terminal().grid();
        use alacritty_terminal::grid::Dimensions;
        let paint_rows = if in_alt_screen {
            grid.screen_lines()
        } else {
            let cursor_row = grid.cursor.point.line.0.max(0) as usize;
            grid.history_size() + (cursor_row + 1).min(grid.screen_lines())
        };
        content_h += paint_rows as f32 * row_h;
    }
    let top_spacer = (scroll_max_h - content_h).max(0.0);

    // Consume any pending "submit just happened — go to bottom" flag.
    // The post-content `ui.scroll_to_cursor` (below) does the actual
    // snap; we just need the bool here to decide whether to call it.
    //
    // Earlier versions called `ScrollArea::vertical_scroll_offset(f32::INFINITY)`
    // hoping egui would clamp it to "bottom of content." It doesn't:
    // egui assigns the raw offset into the persisted scroll state
    // BEFORE its scroll-bar code runs the clamp, and the clamp is
    // gated behind `if show_factor == 0.0 { continue; }` — so when
    // the scroll bars aren't visible (content fits the viewport,
    // very common right after submit) the clamp is skipped and
    // `state.offset.y` is persisted as `f32::INFINITY` for the next
    // frame. The viewport / content-rect math then propagates that
    // infinity (NaN-prone) and the whole ScrollArea renders blank.
    // That was the "scrollback vanishes / alt-screen blank after the
    // 2nd command in a pane" regression.
    // `force_to_bottom` fires for two paths:
    // - One-shot `scroll_to_bottom_pending` (Cmd+Option+Down,
    //   chrome picker, etc.) — take and clear.
    // - Multi-frame `scroll_to_bottom_frames > 0` from submit —
    //   decrement each frame, snap while non-zero. See the field
    //   doc on `PaneUiState::scroll_to_bottom_frames`.
    let one_shot_bottom = std::mem::take(&mut slot.ui.scroll_to_bottom_pending);
    let multi_frame_bottom = slot.ui.scroll_to_bottom_frames > 0;
    if multi_frame_bottom {
        slot.ui.scroll_to_bottom_frames -= 1;
        ctx.request_repaint();
    }
    let force_to_bottom = one_shot_bottom || multi_frame_bottom;
    let force_to_top = std::mem::take(&mut slot.ui.scroll_to_top_pending);
    // `stick_to_bottom` re-snaps the offset to max every frame the
    // user is at the bottom — which fights any top-snap attempt
    // and silently swallows a Cmd+Option+Up jump while the user is
    // glued to the live tail (the typical case while typing
    // commands). Two-layer fix:
    //  1. Disable stick-to-bottom for this frame when `force_to_top`
    //     — the user is asking to leave the bottom, don't pin them.
    //  2. Override the persisted scroll offset directly via
    //     `vertical_scroll_offset(0.0)`. `scroll_to_cursor(TOP)`
    //     writes into a frame-state hint that ScrollArea consumes
    //     at the end of `show()`; it doesn't reliably win against
    //     ScrollArea's internal "was at end" state. A direct offset
    //     override at the builder level always wins.
    //  Using `0.0` (not `f32::INFINITY`) avoids the NaN persistence
    //  trap the bottom-snap path hit earlier — see the existing
    //  `force_to_bottom` comment below.
    let scroll_area = egui::ScrollArea::vertical()
        .id_salt(("pane-blocks", slot.session.pane_id()))
        .stick_to_bottom(!force_to_top)
        .auto_shrink([false, false])
        .max_height(scroll_max_h);
    // `force_to_top` uses a direct offset override (0.0) because
    // `scroll_to_cursor(TOP)` got swallowed by ScrollArea's "we're
    // at the end" cache. `force_to_bottom` keeps using
    // `scroll_to_cursor(BOTTOM)` inside the closure (line ~1060)
    // because the bottom-snap needs egui's accurate post-layout
    // measurement of where content ends; our pre-layout
    // `content_h` estimate was sometimes off, sending the snap to
    // the wrong place.
    let scroll_area =
        if force_to_top { scroll_area.vertical_scroll_offset(0.0) } else { scroll_area };
    let scroll_inner = scroll_area.show(ui, |ui| {
        // Scrollback-jump-to-top (Cmd+Option+Up / Ctrl+Alt+Up): snap
        // the next-widget-position (= top of content) to the TOP
        // edge of the viewport BEFORE the spacer + blocks are laid
        // out. Counterpart to the bottom-snap call at the end of
        // this closure. No-op in alt-screen mode because no content
        // is laid out there.
        if force_to_top && !in_alt_screen {
            ui.scroll_to_cursor(Some(egui::Align::TOP));
        }
        // Top spacer bottom-aligns short content (computed
        // above). In alt-screen mode `content_h` is 0 and the
        // alt-screen paint_terminal below claims the whole pane,
        // so the spacer is harmless there too.
        if top_spacer > 0.0 {
            ui.add_space(top_spacer);
        }
        if in_alt_screen {
            // Don't paint blocks or the editor — let
            // paint_terminal below claim the full pane.
        } else {
            // ---- Block stack: command labels + snapshots ----------
            //
            // Walk every block. For `Sealed`: paint the command line
            // first (teal — the user's typed command, which kernel-
            // echo-suppression hid from the snapshot), then the
            // frozen `Vec<StyledLine>` snapshot of the output, then
            // a thin separator gap. For `Running` (only at the tail
            // by invariant): paint the command label — the live
            // `Term` painted after the loop carries the output as
            // it streams in. For `Prompt` (only at the tail): paint
            // nothing here; the live `Term` shows the shell's idle
            // area (empty with `PS1=''`) and the editor overlay
            // paints on top after `paint_terminal`.
            //
            // Phase 4G renders a dim cwd header line above each
            // block; sealed blocks add a red "exit N" annotation
            // for non-zero exits. Git branch / dirty / live
            // duration chips defer to Phase 5's async-probe surface.
            for block in slot.session.blocks().iter() {
                match block {
                    crate::block::Block::Sealed { id, command, snapshot, header, exit, .. } => {
                        let total_rows =
                            (if command.is_empty() { 0 } else { command.split('\n').count() })
                                + snapshot.len();
                        let sel_for_this =
                            pane_sel.as_ref().and_then(|s| s.block_range_for(*id, total_rows));
                        let sealed_render = render::paint_sealed_block(
                            ui,
                            command,
                            snapshot,
                            sel_for_this,
                            header.cwd.as_deref(),
                            home,
                            *exit,
                        );
                        sealed_block_renders.push((*id, sealed_render));
                        // Block separator — picked via
                        // `cargo run --example pick_block_separator`
                        // (variant `h10-18`). 10px gap + a barely-
                        // there 1px hairline at alpha 0x18 + another
                        // 10px gap. Total ~21px of "breath" between
                        // blocks with the hairline centered.
                        ui.add_space(render::BLOCK_SEPARATOR_GAP);
                        let avail_w = ui.available_width();
                        let (sep_rect, _) =
                            ui.allocate_exact_size(egui::vec2(avail_w, 1.0), egui::Sense::hover());
                        ui.painter().line_segment(
                            [sep_rect.left_center(), sep_rect.right_center()],
                            egui::Stroke::new(1.0, render::BLOCK_SEPARATOR_HAIRLINE),
                        );
                        ui.add_space(render::BLOCK_SEPARATOR_GAP);
                    }
                    crate::block::Block::Running { command, header, .. } => {
                        let _ = render::paint_block_header(ui, header.cwd.as_deref(), home, None);
                        if !command.is_empty() {
                            let _ = render::paint_command_label(ui, command);
                        }
                    }
                    crate::block::Block::Prompt { .. } => {
                        // The `Prompt` block's chrome lives in
                        // the fixed footer below the scroll area
                        // (Phase 4D); the editor itself paints
                        // there too. Nothing to draw here.
                    }
                }
            }
        } // end of `if !in_alt_screen { ... }`

        // ---- Live terminal ------------------------------------
        //
        // Pre-compute hover geometry so the hit-test happens in
        // the same frame as the paint — no one-frame lag on the
        // Cmd-hover underline.
        let display_offset = slot.session.terminal().display_offset() as i32;
        let grid_rows = slot.session.terminal().grid().screen_lines();
        let grid_cols = slot.session.terminal().grid().columns();
        let origin = ui.next_widget_position();
        let geom = selection::GridGeometry {
            origin_x: origin.x,
            origin_y: origin.y,
            cell_w,
            row_h,
            display_offset,
            screen_lines: grid_rows,
            cols: grid_cols,
        };

        let grid_ref = slot.session.terminal().grid();
        let mut links_in_view = links::scan_visible_links(grid_ref);
        let cwd = slot.session.terminal().cwd().map(|p| p.to_path_buf());
        let path_spans = paths::scan_visible_paths(grid_ref, cwd.as_deref(), home);
        links_in_view.extend(path_spans);

        let hover_link: Option<LinkSpan> = pointer_pos.and_then(|pos| {
            let in_grid = pos.x >= origin.x
                && pos.x < origin.x + grid_cols as f32 * cell_w
                && pos.y >= origin.y
                && pos.y < origin.y + grid_rows as f32 * row_h;
            if !in_grid {
                return None;
            }
            let pt = pixel_to_grid_point(pos.x, pos.y, geom);
            links_in_view.iter().find(|l| l.contains(pt)).cloned()
        });
        let highlighted_link = if modifier_held { hover_link.as_ref() } else { None };

        // Hide the live `Term`'s cursor when the editor is
        // active — otherwise the user sees TWO cursors (the
        // shell's one and the editor's overlay). The editor's
        // cursor is the real input caret in this mode.
        let hide_term_cursor = slot.session.editor_is_active();
        // Use the `will_paint_live_term` decision computed
        // before the closure (see comment up there for the
        // full reasoning). The two paths must agree: the
        // bottom-align spacer's `content_h` only includes the
        // grid height when we actually paint it.
        let rendered = if will_paint_live_term {
            // `slot.ui.focused` reflects the previous frame's
            // focus state — close enough for cursor-tint, which
            // would otherwise force two paint passes per frame.
            // A one-frame lag on the focus tint is invisible.
            //
            // `include_history = !in_alt_screen`: outside alt-screen
            // mode, render the entire grid (history + viewport) so
            // a running command's output stays end-to-end visible
            // — the outer ScrollArea handles navigation. In alt-
            // screen mode there's no scrollback to include and the
            // running program owns the screen at fixed size.
            // Cell-cursor focus state per spec/02 (cross-ref to
            // spec/04's caret-visibility rule): the bright `CURSOR_COLOR`
            // is reserved for "this is where your next keypress
            // lands." Both pane focus AND OS window foreground must
            // be true; otherwise we render the dim/hollow color.
            let cell_cursor_focused = slot.ui.focused && ctx.input(|i| i.focused);
            render::paint_terminal(
                ui,
                slot.session.terminal(),
                selection.as_ref(),
                highlighted_link,
                hide_term_cursor,
                cell_cursor_focused,
                !in_alt_screen,
            )
        } else {
            // Synthetic empty `TerminalRender` for the editor-
            // active case. `Sense::hover()` — NOT click/drag — so
            // the placeholder cannot accidentally claim focus.
            // Focus belongs to the editor footer (built below)
            // and `focus_response` selects it explicitly; if this
            // widget were focusable, egui's auto-id machinery
            // could route focus here instead and the caret would
            // never appear in the footer.
            let origin = ui.next_widget_position();
            let (_rect, response) =
                ui.allocate_exact_size(egui::Vec2::new(1.0, 1.0), egui::Sense::hover());
            render::TerminalRender {
                response,
                geometry: selection::GridGeometry {
                    origin_x: origin.x,
                    origin_y: origin.y,
                    cell_w,
                    row_h,
                    display_offset: 0,
                    screen_lines: 0,
                    cols: 0,
                },
            }
        };

        // If the user just submitted a command, snap to the bottom
        // of the now-laid-out content. `scroll_to_cursor` aligns the
        // *next* widget position (which is past everything we've
        // added above) with the requested edge of the viewport —
        // equivalent to "scroll all the way down" but using a
        // finite, well-defined offset that egui clamps correctly.
        if force_to_bottom {
            ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
        }

        (rendered, links_in_view, highlighted_link.cloned())
    });
    let (rendered, links_in_view, highlighted_link) = scroll_inner.inner;
    let highlighted_link = highlighted_link.as_ref();
    if highlighted_link.is_some() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    // Sealed-block link hover: when Cmd is held and the pointer is
    // over a URL / file-path inside a sealed block, switch to the
    // pointing-hand icon so the affordance matches what Cmd-click
    // would do. We ask each block's snapshot Response whether it
    // contains the pointer — egui resolves z-order so a higher
    // widget (the splitter handle, a tab strip) correctly wins and
    // none of our blocks fire `contains_pointer`.
    if modifier_held
        && let Some(pos) = pointer_pos
        && let Some((block_id, block_rect)) = sealed_block_renders.iter().find_map(|(id, r)| {
            if r.snapshot.contains_pointer()
                || r.command.as_ref().is_some_and(|c| c.contains_pointer())
            {
                Some((*id, r.bounding_rect()))
            } else {
                None
            }
        })
    {
        let (cl, sl) = slot.session.sealed_block_rows(block_id).unwrap_or((0, 0));
        let total_rows = cl + sl;
        let mut cursor_pt = sealed_cursor_for_pos(block_rect, pos, cell_w, row_h, total_rows);
        if let Some(row_len) = slot.session.sealed_row_len(block_id, cursor_pt.row) {
            cursor_pt.col = cursor_pt.col.min(row_len);
        }
        if let Some(links) = slot.session.sealed_block_links(block_id, home)
            && let Some(link) = links.iter().find(|l| l.contains(cursor_pt.row, cursor_pt.col))
        {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            // Paint the Cmd-hover affordance over the link's cells
            // inside the sealed block — same translucent-blue
            // overlay + underline that `paint_terminal` uses on the
            // live grid, so the visual reads consistently across
            // the live and frozen paths.
            let x0 = block_rect.min.x + link.col_start as f32 * cell_w;
            let x1 = block_rect.min.x + (link.col_end as f32 + 1.0) * cell_w;
            let row_top = block_rect.min.y + link.row as f32 * row_h;
            let row_bottom = row_top + row_h;
            let underline_y = row_bottom - 1.5;
            let painter = ui.painter();
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::Pos2::new(x0, row_top),
                    egui::Pos2::new(x1, row_bottom),
                ),
                0.0,
                render::LINK_HOVER_OVERLAY_COLOR,
            );
            painter.line_segment(
                [egui::Pos2::new(x0, underline_y), egui::Pos2::new(x1, underline_y)],
                egui::Stroke::new(1.5, render::LINK_UNDERLINE_COLOR),
            );
        }
    }

    // ---- Phase 4D fixed-footer: cwd chip + editor ----------------
    //
    // The ScrollArea above was constrained to leave `footer_h` of
    // vertical space at the bottom. Paint the prompt's cwd chip
    // (when known) and then the editor here. `footer_rect` is the
    // exact rect the editor occupies — used by the click / drag
    // handlers below to translate pointer pixels to byte offsets.
    // Focus lives on the per-pane focus anchor allocated AFTER this
    // block; the editor footer's own `Response` is the routing
    // surface for click / drag events.
    let mut editor_response: Option<egui::Response> = None;
    if footer_h > 0.0 {
        let chip_h = if has_prompt_cwd { row_h + 2.0 * render::CHIP_PAD_Y } else { 0.0 };
        let editor_h = footer_h - chip_h;
        let footer_origin = ui.next_widget_position();

        // Caret visibility per spec/04 "When is the caret shown?":
        // mode-is-editor + pane keyboard focus + OS window foreground.
        // Lifted to here (above the chip + editor paint) so the
        // focused-editor chrome (the rounded outline that wraps BOTH
        // the chip and the editor) can use the same predicate without
        // recomputing it.
        let editor_in_mode = slot.session.blocks().editor_on_tail().is_some();
        let pane_focused = ctx
            .memory(|m| m.has_focus(egui::Id::new(("termica-pane-focus", slot.session.pane_id()))));
        let viewport_focused = ctx.input(|i| i.focused);
        let caret_active =
            render::should_show_caret(editor_in_mode, pane_focused, viewport_focused);

        if has_prompt_cwd
            && let Some(crate::block::Block::Prompt { header, .. }) = slot.session.blocks().last()
        {
            let _ = render::paint_block_header(ui, header.cwd.as_deref(), home, None);
        }

        // Allocate the editor strip and paint into it. The widget
        // ID is pane-scoped and content-independent so it stays
        // stable across frames AND across the Prompt→Running→Prompt
        // transition (each command run creates a new editor; the
        // ID must not depend on its identity). Without this stability
        // egui's auto-id can shift between frames when adjacent
        // layout changes, focus is silently dropped, and the caret
        // never appears. `Sense::click_and_drag` is required so
        // mouse presses register here at all.
        let editor_rect = egui::Rect::from_min_size(
            egui::Pos2::new(footer_origin.x, footer_origin.y + chip_h),
            egui::Vec2::new(ui.available_width(), editor_h),
        );
        // Register the editor footer as a widget. The Response is
        // the SINGLE source of truth for routing pointer events to
        // the editor — click handlers below ask
        // `editor_response.is_pointer_button_down_on()`,
        // `.interact_pointer_pos()`, `.clicked()`, etc., never
        // `editor_rect.contains(global_press_origin)`. Focus is
        // held by the per-pane focus anchor; this response only
        // routes pointer events.
        editor_response = Some(ui.interact(
            editor_rect,
            ui.id().with(("editor-footer", slot.session.pane_id())),
            egui::Sense::click_and_drag(),
        ));

        if let Some(editor) = slot.session.blocks().editor_on_tail() {
            let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
            // Detect caret motion across frames and reset the blink
            // anchor so the "visible" half-cycle starts AT the
            // moment of the move. Without this, a caret motion
            // landing in the middle of an "off" half-cycle is
            // invisible until the next blink — the user briefly
            // can't see where the caret is. Standard editor UX.
            let time = ctx.input(|i| i.time);
            let current_cursor = editor.cursor();
            if slot.ui.last_cursor_byte != Some(current_cursor) {
                slot.ui.caret_blink_anchor = time;
                slot.ui.last_cursor_byte = Some(current_cursor);
            }
            let elapsed = (time - slot.ui.caret_blink_anchor).max(0.0);
            let caret_visible = caret_active && (elapsed * 1.6) as i64 % 2 == 0;
            if caret_active {
                ctx.request_repaint_after(std::time::Duration::from_millis(312));
            }
            // Use `ui.painter()` (unclipped to the current ui) rather
            // than `painter_at(editor_rect)` so descender pixels
            // (`p`, `g`, `y`, `q`) clear the rect's bottom edge
            // instead of being clipped off. The footer reserves
            // `FOOTER_DESCENDER_PAD` extra pixels below the last row
            // for exactly this overflow.
            render::paint_prompt_editor_at(
                ui.painter(),
                editor,
                editor_rect.min,
                cell_w,
                row_h,
                &font_id,
                caret_visible,
            );

            // Completion popup paints just above the editor when
            // open. Routed through `egui::Area` (top-level
            // overlay), so the call order here doesn't affect
            // z-order; the position pin is `editor_rect.min` so
            // the popup's bottom edge aligns with the editor's
            // top.
            if let Some(popup) = slot.ui.completion_popup.as_mut() {
                let clicked = crate::completion::popup::paint(
                    ctx,
                    popup,
                    editor_rect.min,
                    slot.session.pane_id(),
                    10,
                );
                // Click-to-accept: a row click sets the selected
                // candidate AND accepts it, same as Tab/Enter.
                if let Some(row_idx) = clicked {
                    let Some(mut popup_taken) = slot.ui.completion_popup.take() else {
                        unreachable!("just confirmed popup is Some via as_mut above")
                    };
                    popup_taken.selected_index = row_idx;
                    let cursor =
                        slot.session.blocks().editor_on_tail().map(|e| e.cursor()).unwrap_or(0);
                    let current_token_len = cursor.saturating_sub(popup_taken.origin_byte);
                    if let Some(editor) = slot.session.editor_mut() {
                        popup_taken.accept(editor, current_token_len);
                    }
                    slot.session.clear_history_recall();
                }
            }
        } else {
            // Editor not on tail this frame — forget the cursor
            // tracker so re-opening the editor in a new prompt
            // starts a fresh blink cycle.
            slot.ui.last_cursor_byte = None;
            // Editor gone — popup must go too.
            slot.ui.completion_popup = None;
        }

        // Focused-editor chrome. Dispatched via the
        // [`crate::focused_chrome::paint`] table so the second
        // OS window (chrome picker) can swap the variant live.
        // The chip bar + editor are wrapped as a single visual
        // unit — the user asked for "include the chip bar."
        //
        // Wraparound fix: use `ctx.layer_painter(layer_id)` to
        // get a painter on the SAME layer (so the chrome paints
        // on top of the chip + editor that landed there earlier)
        // with `Rect::EVERYTHING` for the clip rect.
        // `Painter::with_clip_rect` *intersects* with the existing
        // clip rather than replacing it, so the previous
        // `clone().with_clip_rect(viewport_rect)` didn't widen
        // the pane's tight clip and three sides of the stroke got
        // cut. Same trick the focused-tab underline uses
        // (`behavior::paint_focused_tab_underline`).
        // Animate the focused-editor chrome's opacity toward the
        // caret-active target. Half-second fade either direction so
        // the chrome breathes in/out instead of popping. While the
        // value is in transit we request a fast repaint so the
        // animation runs smoothly even if the PTY is idle.
        const CHROME_FADE_SECS: f32 = 0.5;
        let target_opacity: f32 = if caret_active { 1.0 } else { 0.0 };
        let dt = ctx.input(|i| i.stable_dt);
        let step = (dt / CHROME_FADE_SECS).clamp(0.0, 1.0);
        slot.ui.chrome_opacity = if (slot.ui.chrome_opacity - target_opacity).abs() <= step {
            target_opacity
        } else if slot.ui.chrome_opacity < target_opacity {
            slot.ui.chrome_opacity + step
        } else {
            slot.ui.chrome_opacity - step
        };
        if slot.ui.chrome_opacity != target_opacity {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }

        if slot.ui.chrome_opacity > 0.0 {
            // Body width comes from the ui's CLIP rect, not its
            // available-width / layout rect. egui_tiles can give
            // the pane_ui a layout rect that's wider than the
            // pane and rely on the clip_rect to keep paint inside
            // the pane — which is exactly why the editor's text
            // content clips correctly at the pane's right edge
            // while a decoration drawn at `available_width`
            // overshoots into the divider or off-screen.
            let pane_clip = ui.clip_rect();
            let body_w = (pane_clip.right() - footer_origin.x).max(0.0);
            let chip_rect = if has_prompt_cwd {
                Some(egui::Rect::from_min_size(footer_origin, egui::vec2(body_w, chip_h)))
            } else {
                None
            };
            let combined =
                egui::Rect::from_min_size(footer_origin, egui::vec2(body_w, chip_h + editor_h));
            // Chrome painter: layer-painter (so we're not clipped
            // to the footer's tight vertical clip — variants paint
            // a few px above + below the body, which `ui.painter()`
            // would chop), but with a CLIP RECT that includes a few
            // pixels OUTSIDE the pane's horizontal bounds so the
            // chrome's outer L/R strokes (glow expands by 5+ px)
            // actually render. Without this overhang, the side
            // strokes are clipped exactly at the pane edge and only
            // the top + bottom edges show. egui_tiles' gap_width is
            // bumped to 16 px in `behavior.rs` so this 8 px overhang
            // sits inside the splitter gap and never bleeds into a
            // neighbor pane. The vertical infinity keeps the
            // bottom/top edges intact (footer's natural clip would
            // chop them).
            const CHROME_OUTER_OVERHANG: f32 = 8.0;
            let chrome_clip = egui::Rect::from_min_max(
                egui::pos2(pane_clip.left() - CHROME_OUTER_OVERHANG, -f32::INFINITY),
                egui::pos2(pane_clip.right() + CHROME_OUTER_OVERHANG, f32::INFINITY),
            );
            let painter = ctx.layer_painter(ui.layer_id()).with_clip_rect(chrome_clip);
            crate::focused_chrome::paint(
                &painter,
                chip_rect,
                combined,
                chrome_variant,
                slot.ui.chrome_opacity,
            );
        }
    }

    // Stable per-pane focus anchor.
    //
    // Previously `focus_response` was either the editor footer (when
    // editor mode) or the live `Term` (otherwise). Submitting a
    // command would switch modes mid-frame: the editor footer would
    // stop being allocated, its widget id would drop out of egui's
    // `used_ids`, and egui's end-of-frame dead-mans-switch would
    // release focus. On the next frame every visible pane would
    // observe `nothing_focused = true` and race to call
    // `request_focus()` — whichever pane rendered last in egui_tiles'
    // iteration order kept focus, which is why the user saw focus
    // hop to "the last tab on the left collection" after pressing
    // Enter in a right-collection pane.
    //
    // The fix: allocate a 0×0 focusable widget with a pane-stable
    // id every frame, regardless of mode. All focus interactions
    // (`request_focus` / `has_focus` / `set_focus_lock_filter` / the
    // keyboard-gate read) go through this single anchor, so the id
    // never changes mid-session and the dead-mans-switch never fires
    // on it. The editor footer and live `Term` keep their own
    // responses for hit-testing (rect + drag detection); they no
    // longer hold focus themselves.
    let pane_focus_id = egui::Id::new(("termica-pane-focus", slot.session.pane_id()));
    let focus_anchor = ui.interact(
        egui::Rect::from_min_size(ui.max_rect().min, egui::Vec2::ZERO),
        pane_focus_id,
        egui::Sense::focusable_noninteractive(),
    );
    let focus_response = &focus_anchor;

    // ---- alt-screen border --------------------------------------
    //
    // When the pane is running a full-screen TTY program (vim, htop,
    // less, fzf, ssh-with-TUI), paint a 1px border around the cell
    // grid so the user can see at a glance that input is going
    // verbatim to that program — different mode, different look.
    // The border sits flush against the grid rect, not the pane
    // chrome, so it tracks the actual area where keystrokes are
    // routed differently.
    if view.alt_screen {
        paint_alt_screen_border(ui.painter(), rendered.response.rect);
    }

    // ---- mouse selection / link click + focus-on-press ----------
    //
    // **Routing rule:** all pointer events are routed through each
    // widget's own [`egui::Response`]. We do NOT poll global
    // pointer state (`ctx.input.pointer.press_origin()`,
    // `primary_down`, etc.) to figure out WHICH widget got a press
    // — egui's interaction layer already did that, with full
    // z-order awareness. A higher widget (the `egui_tiles`
    // splitter resize handle, a tab strip widget, a modal overlay)
    // wins exclusively, and only its `Response` reflects the
    // press. Asking each widget gives a correct answer even when
    // multiple widgets cover the same pixel; reading global state
    // + intersecting it with stored rects does not.
    //
    // `ctx.input` is only consulted for state that isn't a press
    // event: the current `time` (for the multi-click counter),
    // and modifier keys at click time (for Cmd-click on links).
    let geom_after_paint = rendered.geometry;
    let to_point = |pos: egui::Pos2| pixel_to_grid_point(pos.x, pos.y, geom_after_paint);
    let now = ctx.input(|i| i.time);
    // `primary_just_pressed` is timing-only — it tells us "a press
    // happened somewhere this frame", which lets us distinguish a
    // start-of-gesture from a continuing drag. It does NOT tell us
    // WHICH widget got the press; that comes from per-widget
    // `Response::is_pointer_button_down_on()`.
    let primary_just_pressed = ctx.input(|i| i.pointer.primary_pressed());

    /// Translate a pointer pixel position inside the editor rect to
    /// a byte index in the editor's text. Out-of-rows clamps to end.
    fn editor_byte_for_pos(
        rect: egui::Rect,
        pos: egui::Pos2,
        editor_text: &str,
        cell_w: f32,
        row_h: f32,
    ) -> usize {
        let row = ((pos.y - rect.min.y) / row_h).max(0.0) as usize;
        let col_chars = ((pos.x - rect.min.x) / cell_w).max(0.0).floor() as usize;
        crate::prompt_editor::byte_index_for_row_col(editor_text, row, col_chars)
    }

    /// Translate a pointer pixel position inside a sealed-block rect
    /// to a [`BlockCursor`] in the block's unified row space (rows
    /// `0..command_lines` = command label; rest = snapshot). Clamps
    /// to the block's bounds — pointer above → row 0; below → last
    /// row; left → col 0; right → row_len. Per-row col is clamped
    /// by the caller via `sealed_row_len`.
    fn sealed_cursor_for_pos(
        rect: egui::Rect,
        pos: egui::Pos2,
        cell_w: f32,
        row_h: f32,
        total_rows: usize,
    ) -> crate::block_selection::BlockCursor {
        let dy = (pos.y - rect.min.y).max(0.0);
        let dx = (pos.x - rect.min.x).max(0.0);
        let row = ((dy / row_h) as usize).min(total_rows.saturating_sub(1));
        let col = (dx / cell_w) as usize;
        crate::block_selection::BlockCursor::new(row, col)
    }

    /// Resolve a screen-space pointer position to a sealed block + a
    /// [`BlockCursor`] in that block's row space. Used by the
    /// cross-block drag handler: when a press lands on block A and
    /// the pointer is now over block B (or in the gap between
    /// blocks, or above / below the painted list), this picks the
    /// right target block for the selection's head cursor.
    ///
    /// `sealed_block_renders` is the list of currently-painted
    /// sealed blocks in pane reading order (top to bottom). The
    /// caller has already verified the press is on some sealed
    /// block (`sealed_pressed_id.is_some()`); we never see an empty
    /// list in practice.
    ///
    /// Resolution:
    /// 1. **Above all blocks** (`pos.y < first.top`): clamp to first
    ///    block, `(0, 0)`.
    /// 2. **Below all blocks** (`pos.y >= last.bottom`): clamp to
    ///    last block, last row, `col = usize::MAX` (gets clamped to
    ///    row length by the caller's `sealed_row_len` lookup).
    /// 3. **Inside a block's rect**: map to that block's cursor via
    ///    [`sealed_cursor_for_pos`].
    /// 4. **In a gap between two consecutive blocks**: snap to the
    ///    end of the upper block (matches the "selection extends to
    ///    end of last touched block" intuition for downward drags).
    fn find_head_block_for_pos(
        pos: egui::Pos2,
        sealed_block_renders: &[(crate::block::BlockId, render::SealedBlockRender)],
        cell_w: f32,
        row_h: f32,
        session: &crate::pane::PaneSession,
    ) -> (crate::block::BlockId, crate::block_selection::BlockCursor) {
        // Falls-through for empty list; callers only reach this when
        // the press is on a sealed block, so the list is non-empty in
        // practice. The fallback `BlockId(0)` is defensive.
        let Some((first_id, first_render)) = sealed_block_renders.first() else {
            return (crate::block::BlockId(0), crate::block_selection::BlockCursor::new(0, 0));
        };
        let first_rect = first_render.bounding_rect();
        if pos.y < first_rect.top() {
            return (*first_id, crate::block_selection::BlockCursor::new(0, 0));
        }

        let total_for = |bid: crate::block::BlockId| -> usize {
            let (c, s) = session.sealed_block_rows(bid).unwrap_or((0, 0));
            c + s
        };

        // Inside a block, or in a gap immediately above one.
        let mut prev_block_end: Option<crate::block::BlockId> = None;
        for (bid, render) in sealed_block_renders {
            let rect = render.bounding_rect();
            if pos.y < rect.top() {
                // Gap between previous block and this one. Snap to
                // the END of the previous block (which is the one
                // visually above pos).
                if let Some(prev_id) = prev_block_end {
                    let prev_total = total_for(prev_id);
                    let last_row = prev_total.saturating_sub(1);
                    return (
                        prev_id,
                        crate::block_selection::BlockCursor::new(last_row, usize::MAX),
                    );
                }
                // No previous block (pointer just above the first
                // block's top — covered by the early-return above
                // but defensive).
                return (*bid, crate::block_selection::BlockCursor::new(0, 0));
            }
            if pos.y < rect.bottom() {
                // Inside this block.
                let total_rows = total_for(*bid);
                let mut c = sealed_cursor_for_pos(rect, pos, cell_w, row_h, total_rows);
                if let Some(row_len) = session.sealed_row_len(*bid, c.row) {
                    c.col = c.col.min(row_len);
                }
                return (*bid, c);
            }
            prev_block_end = Some(*bid);
        }

        // Below all blocks: snap to end of the last painted block.
        let (last_id, last_render) = sealed_block_renders.last().unwrap();
        let last_total = total_for(*last_id);
        let last_rect = last_render.bounding_rect();
        let _ = last_rect;
        let last_row = last_total.saturating_sub(1);
        (*last_id, crate::block_selection::BlockCursor::new(last_row, usize::MAX))
    }

    /// Update the multi-click counter (click_count, last_press_time,
    /// last_press_pos) for a press that just landed at `pos`.
    /// Returns the new click count (1, 2, or 3).
    fn bump_multi_click(
        slot_ui: &mut crate::pane_slot::PaneUiState,
        now: f64,
        pos: egui::Pos2,
    ) -> u8 {
        let dt = now - slot_ui.last_press_time;
        let dist = (pos - slot_ui.last_press_pos).length();
        if dt < MULTI_CLICK_WINDOW_SECS && dist < MULTI_CLICK_DISTANCE_PX {
            slot_ui.click_count = (slot_ui.click_count + 1).min(3);
        } else {
            slot_ui.click_count = 1;
        }
        slot_ui.last_press_time = now;
        slot_ui.last_press_pos = pos;
        slot_ui.click_count
    }

    // Does any widget inside this pane currently hold the pointer
    // press? Each `is_pointer_button_down_on()` is exclusive — at
    // most one widget on screen returns true. If ANY of ours
    // returns true, the press lives inside this pane; focus
    // belongs here. If none does, the press is on something else
    // (splitter, tab strip, another pane) and we must not touch
    // focus or start a selection.
    let live_grid_pressed = rendered.response.is_pointer_button_down_on();
    let editor_pressed = editor_response.as_ref().is_some_and(|r| r.is_pointer_button_down_on());
    let sealed_pressed_id: Option<crate::block::BlockId> =
        sealed_block_renders.iter().find_map(|(id, r)| {
            if r.snapshot.is_pointer_button_down_on()
                || r.command.as_ref().is_some_and(|c| c.is_pointer_button_down_on())
            {
                Some(*id)
            } else {
                None
            }
        });
    let any_pane_widget_pressed =
        live_grid_pressed || editor_pressed || sealed_pressed_id.is_some();

    // Focus-on-background-click: if a primary press happened this
    // frame AND the pointer is currently over an uncovered area of
    // this pane (no inner widget covers the spot at z-order), the
    // user clicked on the pane's gray background — claim focus.
    // `contains_pointer()` is z-order aware (see egui Response
    // docs), so this never fires when the press lands on an inner
    // widget; those go through `any_pane_widget_pressed` above.
    let background_press = primary_just_pressed && pane_background_hover.contains_pointer();

    if !modal_open && (any_pane_widget_pressed || background_press) {
        focus_response.request_focus();
    }

    // ---- Sealed-block click / drag routing ----------------------
    //
    // Per-widget: the snapshot OR command label of one specific
    // sealed block is receiving the press. Egui's interaction
    // layer guarantees this is exclusive — at most one block fires
    // per frame, never both panes during a splitter drag. egui's
    // exclusive drag ownership also keeps that block's
    // `is_pointer_button_down_on()` true even when the pointer
    // moves OFF the block's rect, so this branch keeps running
    // through a cross-block drag.
    if !modal_open && let Some(origin_block_id) = sealed_pressed_id {
        // Find the response set for the origin (press) block. The
        // press may be on either sub-widget (command label or
        // snapshot).
        if let Some((_, sealed_origin)) =
            sealed_block_renders.iter().find(|(id, _)| *id == origin_block_id)
        {
            let pos = sealed_origin
                .snapshot
                .interact_pointer_pos()
                .or_else(|| sealed_origin.command.as_ref().and_then(|c| c.interact_pointer_pos()));
            if let Some(pos) = pos {
                // Origin-block cursor: where the press / multi-click
                // landed, in the origin block's row space.
                let origin_rect = sealed_origin.bounding_rect();
                let (origin_cmd_lines, origin_snap_lines) =
                    slot.session.sealed_block_rows(origin_block_id).unwrap_or((0, 0));
                let origin_total_rows = origin_cmd_lines + origin_snap_lines;
                let mut origin_cursor =
                    sealed_cursor_for_pos(origin_rect, pos, cell_w, row_h, origin_total_rows);
                if let Some(row_len) =
                    slot.session.sealed_row_len(origin_block_id, origin_cursor.row)
                {
                    origin_cursor.col = origin_cursor.col.min(row_len);
                }

                if primary_just_pressed {
                    // -------- START a selection in the press block ----
                    // Anchor + head both land in the origin block;
                    // multi-click expands within that block.
                    let sealed_link =
                        slot.session.sealed_block_links(origin_block_id, home).and_then(|links| {
                            links
                                .iter()
                                .find(|l| l.contains(origin_cursor.row, origin_cursor.col))
                                .cloned()
                        });
                    if modifier_held && let Some(link) = &sealed_link {
                        // Cmd-click on a URL / path: open it, don't
                        // start a selection.
                        open_url(&link.url);
                    } else {
                        let click_count = bump_multi_click(&mut slot.ui, now, pos);
                        let mk_anchor_head =
                            |a: crate::block_selection::BlockCursor,
                             b: crate::block_selection::BlockCursor|
                             -> crate::pane_selection::PaneSelection {
                                crate::pane_selection::PaneSelection::new(
                                    crate::pane_selection::PaneCursor::in_block(origin_block_id, a),
                                    crate::pane_selection::PaneCursor::in_block(origin_block_id, b),
                                )
                            };
                        match click_count {
                            2 => {
                                if let Some(link) = &sealed_link {
                                    let a = crate::block_selection::BlockCursor::new(
                                        link.row,
                                        link.col_start,
                                    );
                                    let b = crate::block_selection::BlockCursor::new(
                                        link.row,
                                        link.col_end + 1,
                                    );
                                    slot.ui.sealed_drag_anchor = Some((origin_block_id, a, b));
                                    slot.session.set_pane_selection(mk_anchor_head(a, b));
                                } else if let Some((a, b)) =
                                    slot.session.sealed_word_range(origin_block_id, origin_cursor)
                                {
                                    slot.ui.sealed_drag_anchor = Some((origin_block_id, a, b));
                                    slot.session.set_pane_selection(mk_anchor_head(a, b));
                                }
                            }
                            3 => {
                                if let Some((a, b)) =
                                    slot.session.sealed_line_range(origin_block_id, origin_cursor)
                                {
                                    slot.ui.sealed_drag_anchor = Some((origin_block_id, a, b));
                                    slot.session.set_pane_selection(mk_anchor_head(a, b));
                                }
                            }
                            _ => {
                                slot.ui.sealed_drag_anchor = None;
                                slot.session.set_pane_selection(mk_anchor_head(
                                    origin_cursor,
                                    origin_cursor,
                                ));
                            }
                        }
                    }
                } else if let Some(sel) = slot.session.pane_selection().copied() {
                    // -------- EXTEND the selection -------------------
                    //
                    // Cross-block: the pointer may be over a different
                    // sealed block than the origin. Find whichever
                    // block the pointer is currently over (or clamp to
                    // top / bottom of the painted list) and translate
                    // `pos` to a cursor in that block's row space.
                    let (head_block_id, head_cursor) = find_head_block_for_pos(
                        pos,
                        &sealed_block_renders,
                        cell_w,
                        row_h,
                        &slot.session,
                    );
                    match (slot.ui.click_count, slot.ui.sealed_drag_anchor) {
                        (2, Some((anchor_block, a_start, a_end))) => {
                            // Word-mode drag, same-block OR cross-block:
                            // the unified far-edge rule in
                            // `extend_multiclick_selection_endpoints`
                            // handles both. Pre-fix this branch ran
                            // only when `anchor_block == head_block_id`
                            // — cross-block drag fell through to
                            // char mode in the head block and "lost"
                            // the anchor word when dragged upward.
                            if let Some(head_bounds) =
                                slot.session.sealed_word_range(head_block_id, head_cursor)
                            {
                                let (anc_pc, head_pc) =
                                    crate::pane_selection::extend_multiclick_selection_endpoints(
                                        anchor_block,
                                        (a_start, a_end),
                                        head_block_id,
                                        head_bounds,
                                    );
                                slot.session.update_pane_selection_endpoints(anc_pc, head_pc);
                            }
                        }
                        (3, Some((anchor_block, a_start, a_end))) => {
                            // Line-mode drag — same rule, line bounds.
                            if let Some(head_bounds) =
                                slot.session.sealed_line_range(head_block_id, head_cursor)
                            {
                                let (anc_pc, head_pc) =
                                    crate::pane_selection::extend_multiclick_selection_endpoints(
                                        anchor_block,
                                        (a_start, a_end),
                                        head_block_id,
                                        head_bounds,
                                    );
                                slot.session.update_pane_selection_endpoints(anc_pc, head_pc);
                            }
                        }
                        _ => {
                            // Char mode (no multi-click anchor): just
                            // move the head. Anchor stays pinned
                            // wherever the press landed.
                            let _ = sel; // anchor is still fine
                            slot.session.update_pane_selection_head(
                                crate::pane_selection::PaneCursor::in_block(
                                    head_block_id,
                                    head_cursor,
                                ),
                            );
                        }
                    }
                }
            }
        }
    }

    // ---- Editor footer click / drag routing ---------------------
    if !modal_open
        && editor_pressed
        && let Some(editor_resp) = editor_response.as_ref()
        && let Some(pos) = editor_resp.interact_pointer_pos()
    {
        let rect = editor_resp.rect;
        let editor_text = slot
            .session
            .blocks()
            .editor_on_tail()
            .map(|e| e.text().to_string())
            .unwrap_or_default();
        let byte = editor_byte_for_pos(rect, pos, &editor_text, cell_w, row_h);

        if primary_just_pressed {
            // Press in the editor ends any sealed-block selection.
            slot.session.clear_pane_selection();
            slot.ui.sealed_drag_anchor = None;
            let click_count = bump_multi_click(&mut slot.ui, now, pos);
            slot.ui.editor_drag_anchor = match click_count {
                2 => Some(crate::prompt_editor::word_range_at(&editor_text, byte)),
                3 => Some(crate::prompt_editor::line_range_at(&editor_text, byte)),
                _ => None,
            };
            if let Some(editor) = slot.session.editor_mut() {
                match click_count {
                    1 => editor.set_cursor(byte),
                    2 => editor.select_word_at(byte),
                    _ => editor.select_line_at(byte),
                }
            }
        } else {
            // EXTEND while button is held.
            match (slot.ui.click_count, slot.ui.editor_drag_anchor) {
                (2, Some(anchor)) => {
                    let cur = crate::prompt_editor::word_range_at(&editor_text, byte);
                    let start = anchor.0.min(cur.0);
                    let end = anchor.1.max(cur.1);
                    if let Some(editor) = slot.session.editor_mut() {
                        editor.set_selection(start, end);
                    }
                }
                (3, Some(anchor)) => {
                    let cur = crate::prompt_editor::line_range_at(&editor_text, byte);
                    let start = anchor.0.min(cur.0);
                    let end = anchor.1.max(cur.1);
                    if let Some(editor) = slot.session.editor_mut() {
                        editor.set_selection(start, end);
                    }
                }
                _ => {
                    if let Some(editor) = slot.session.editor_mut() {
                        editor.set_cursor_extending(byte);
                    }
                }
            }
        }
    }

    // ---- Live-grid click / drag routing -------------------------
    if !modal_open
        && live_grid_pressed
        && let Some(pos) = rendered.response.interact_pointer_pos()
    {
        let press_pt = to_point(pos);
        let link_under_press = links_in_view.iter().find(|l| l.contains(press_pt)).cloned();

        if primary_just_pressed {
            if modifier_held && let Some(link) = link_under_press {
                open_url(&link.url);
            } else {
                let click_count = bump_multi_click(&mut slot.ui, now, pos);
                let mode = match click_count {
                    1 => SelectionMode::Char,
                    2 => SelectionMode::Word,
                    _ => SelectionMode::Line,
                };
                if mode == SelectionMode::Word
                    && let Some(link) = link_under_press
                {
                    slot.session.start_url_selection(link.start, link.end);
                } else {
                    slot.session.start_selection(press_pt, mode);
                }
            }
        } else if rendered.response.dragged() {
            // EXTEND. Egui's `dragged()` is the canonical
            // "this widget is being dragged" signal once the
            // drag threshold is met.
            slot.session.extend_selection(press_pt);
        }
    }

    // ---- focus transfer & bootstrap -----------------------------
    //
    // Three reasons to claim focus here:
    //   1. `slot.ui.needs_focus` was set by the app: the user just
    //      clicked this pane's tab, spawned a new tab via [+], or
    //      dragged into a new region. Honour it on this render.
    //   2. Nothing in the whole app holds focus yet (cold launch).
    //      Whichever visible pane renders first grabs focus so
    //      typing works without an explicit click.
    //
    // The press handler above ALSO calls `request_focus` on click —
    // that path covers clicking inside a pane's grid (rather than
    // its tab title).
    //
    // **`had_focus_at_frame_start`** is captured BEFORE the
    // `request_focus()` call below because the keyboard gate at
    // the bottom of this function must use the focus state that
    // existed at the *start* of this frame, not whatever
    // `request_focus()` set mid-frame. egui's
    // `Memory::request_focus()` updates `focused_widget`
    // synchronously — so any subsequent `has_focus(id)` read
    // returns `true` immediately, in the same frame. Without
    // pinning the gate to the start-of-frame state, this race
    // bites:
    //
    //   1. The user presses Esc while a TextEdit (the Ctrl+R
    //      overlay's search box) has focus. egui's input
    //      processor sees the TextEdit's `event_filter.escape`
    //      is false, treats Esc as a focus-direction key, and
    //      clears `focused_widget` to `None` BEFORE any render.
    //   2. The leftmost pane renders first. Its focus-claim
    //      branch sees `nothing_focused = true` → calls
    //      `request_focus()` → its anchor immediately has focus.
    //   3. The same frame's keyboard gate now opens — even
    //      though the *user* never directed input at this pane —
    //      and the same event runs `apply_event_to_editor`, landing
    //      in the wrong pane's editor (e.g. a typed character, or an
    //      Enter that submits this pane's buffer).
    //   4. The rightmost pane (the one the user was actually on)
    //      then renders, requests focus via `needs_focus = true`
    //      from the overlay-close path, and ends up focused.
    //
    // The leftmost pane just consumed a key the user never sent at
    // it. By using `had_focus_at_frame_start`
    // for the gate, focus claims (whether via `needs_focus`,
    // `nothing_focused`, or a click) take effect on the NEXT
    // frame's keyboard processing — never the same frame's. The
    // current frame's events stay with whoever had focus at
    // input-processing time.
    let had_focus_at_frame_start = focus_response.has_focus();
    let needs_focus = std::mem::take(&mut slot.ui.needs_focus);
    let nothing_focused = ctx.memory(|m| m.focused().is_none());
    if !modal_open && (needs_focus || nothing_focused) {
        focus_response.request_focus();
    }

    // While this pane has keyboard focus, claim Tab and the arrow
    // keys + Escape for the terminal. Without this, egui's built-in
    // focus navigation eats Tab (cycles focus between widgets) and
    // some arrow paths (focus-based directional nav), so they never
    // reach the encoder + PTY — visible as focus bouncing into the
    // tab strip's [+] button instead of typing into the shell.
    //
    // `set_focus_lock_filter` only takes effect once the widget has
    // BOTH `had_focus_last_frame(id)` AND `has_focus(id)`; new
    // panes spend exactly one frame "warming up" before the filter
    // is active. That's fine — a one-frame Tab-eats-focus on the
    // first frame of a new pane is invisible in practice.
    ctx.memory_mut(|m| {
        m.set_focus_lock_filter(
            focus_response.id,
            egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            },
        );
    });

    // Record focus state for the next frame's tab styling and
    // caret blink. `has_focus` is read by `TermicaApp::update` after
    // `tree.ui()` returns; the focused pane's tab title is rendered
    // bold via the Behavior.
    slot.ui.focused = focus_response.has_focus();

    // ---- keyboard input, focus-gated ----------------------------
    //
    // Use `had_focus_at_frame_start` rather than
    // `focus_response.has_focus()` here. See the long comment on
    // `had_focus_at_frame_start` above for the failure mode this
    // dodges (Ctrl+R overlay Esc → leftmost pane grabs focus
    // mid-frame → leftmost pane processes the same Esc → silently
    // demotes to RawTerminal even though the user was on a
    // different pane).
    if !modal_open && had_focus_at_frame_start {
        let is_macos = cfg!(target_os = "macos");
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());

        // Copy shortcut: macOS trusts Event::Copy from Cmd+C; off-
        // Mac we look for Ctrl+Shift+C in the Key event (plain
        // Ctrl+C must remain SIGINT for the shell, so we cannot
        // trust Event::Copy there).
        let copy_pressed = events.iter().any(|e| match e {
            egui::Event::Copy => is_macos,
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                !is_macos && input::is_copy_shortcut(*key, *modifiers, false)
            }
            _ => false,
        });
        // Cut shortcut: macOS sends `Event::Cut` from Cmd+X. Off-Mac
        // we look for Ctrl+Shift+X. Cmd+X in the editor cuts the
        // selection (copy then delete); outside editor mode it's a
        // no-op for now (alacritty selection has no notion of cut).
        let cut_pressed = events.iter().any(|e| match e {
            egui::Event::Cut => is_macos,
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                !is_macos
                    && *key == egui::Key::X
                    && modifiers.ctrl
                    && modifiers.shift
                    && !modifiers.alt
                    && !modifiers.command
            }
            _ => false,
        });

        // Clipboard priority, highest to lowest:
        //   1. The editor's own selection (only relevant in editor
        //      mode; supports cut).
        //   2. A sealed-block selection (Phase 4F).
        //   3. The live-grid (alacritty) selection.
        //
        // The editor being active does NOT mask the block / grid
        // selections — the user can have focus in the editor while
        // a sealed-block selection is visible, and Cmd+C should
        // copy that block selection. Only the editor's *own*
        // selection takes priority, and only when it actually
        // exists.
        let editor_selection: Option<String> = if slot.session.editor_is_active() {
            slot.session
                .blocks()
                .editor_on_tail()
                .and_then(|e| e.selected_text())
                .map(str::to_string)
        } else {
            None
        };
        if (copy_pressed || cut_pressed)
            && let Some(text) = editor_selection
        {
            ctx.copy_text(text);
            if cut_pressed && let Some(editor) = slot.session.editor_mut() {
                // Use `cut()` not `delete_selection()` so the cut is
                // a real undo point that captures the selection. Per
                // spec/04 §"Undo / redo": `select → cut → undo`
                // restores the cut text AND re-selects it.
                let _ = editor.cut();
            }
        } else if copy_pressed {
            if let Some(text) = slot.session.pane_selection_text() {
                ctx.copy_text(text);
            } else if let Some(text) = slot.session.selection_text() {
                ctx.copy_text(text);
            }
        }

        // App-level shortcuts: Cmd+T new-tab, Cmd+Shift+]/[ next/prev
        // tab. `modifiers.command` is egui's platform-aware "primary"
        // modifier (Cmd on macOS, Ctrl elsewhere). The encoder's new
        // "any modifier → unmapped" rule means these events would
        // already produce no PTY bytes; we just intercept here to
        // stage the app intent.
        for event in &events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = event
                && let Some(action) = match_pane_shortcut(*key, *modifiers, is_macos)
            {
                slot.ui.pending_action = Some(action);
                break;
            }
        }

        let modes = slot.session.terminal().modes();
        let editor_active = slot.session.editor_is_active();

        // ---- mouse wheel, hover-gated ---------------------------
        //
        // Non-alt-screen scrolling is handled by the outer
        // `ScrollArea` natively (4A-render replaced alacritty's
        // internal scrollback with the block-stack history). The
        // alt-screen case still intercepts wheel events here and
        // forwards them as arrow keystrokes — that's what full-
        // screen TTY programs (vim, less, htop, fzf) expect.
        //
        // **Wheel bytes are written BEFORE the per-event key loop
        // below.** Foreground programs read PTY stdin sequentially;
        // a quit keystroke like `q` makes the program exit and stop
        // reading, so any wheel bytes queued AFTER it stay in the
        // PTY buffer and land at the SHELL as stray characters of
        // the user's next command. Trackpad momentum scroll fading
        // for several frames is the realistic trigger — the user
        // lifts their fingers, presses `q` mid-fade, and gets
        // arrow-key garbage on the next command line. See
        // `input::compose_alt_screen_frame_bytes` for the ordering
        // invariant + unit tests.
        if !modal_open && rendered.response.hovered() {
            let alt_screen = slot.session.terminal().is_alternate_screen();
            if alt_screen {
                let scroll_delta_y = ctx.input(|i| i.smooth_scroll_delta.y);
                if scroll_delta_y.abs() > 0.0 {
                    let lines = (scroll_delta_y / 50.0 * 3.0).round() as i32;
                    if let Some(input::WheelOutcome::SendBytes(bytes)) =
                        input::classify_wheel(lines, true, modes)
                    {
                        let _ = slot.session.write(&bytes);
                    }
                }
            }
        }

        // Snapshot the editor's text + cursor BEFORE event
        // processing so the after-loop live-filter pass can tell
        // whether the buffer actually changed (a text edit) vs
        // navigation-only frames (Up/Down in popup, or no
        // editor events at all). Without this guard the live-
        // filter would clobber `selected_index` after every
        // Up/Down press, defeating popup navigation.
        let editor_state_before: Option<(String, usize)> =
            slot.session.blocks().editor_on_tail().map(|e| (e.text().to_string(), e.cursor()));

        for event in &events {
            // Belt and braces: skip the Ctrl+Shift+C key event so the
            // encoder never sees it (the encoder wouldn't emit bytes
            // for that modifier combo anyway, but be explicit).
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = event
                && !is_macos
                && input::is_copy_shortcut(*key, *modifiers, false)
            {
                continue;
            }
            // Phase 4B: in `ShellPromptEditor` mode, keystrokes go to
            // the native editor inside the `Prompt` block instead of
            // straight to the PTY. The encoder is bypassed for these
            // events — typing into the editor must NOT also echo to
            // the shell. `apply_event_to_editor` returns `true` when
            // it owned the event.
            // While the Ctrl+R overlay is open it is modal: events
            // are owned by the overlay's `TextEdit` + the `ctx.input`
            // navigation checks in `crate::history_overlay::paint`.
            // We just skip the editor / PTY paths so nothing leaks.
            if slot.ui.history_overlay.is_some() {
                continue;
            }
            // ---- Tab completion popup interception -----------------
            //
            // If the completion popup is open, navigation keys
            // (Up/Down/Tab/Enter/Esc) drive it instead of the
            // editor. Any other key dismisses the popup AND falls
            // through to the editor, so the user's typing
            // continues to land naturally — re-pressing Tab
            // reopens with fresh candidates.
            //
            // If the popup is closed, a bare Tab keystroke opens
            // it by calling the orchestrator. Tab with any
            // modifier passes through to the editor's existing
            // "consume Tab as no-op" path so we don't fight
            // future Tab-modifier features.
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
                use egui::Key;
                let no_mods =
                    !modifiers.shift && !modifiers.alt && !modifiers.command && !modifiers.ctrl;
                if slot.ui.completion_popup.is_some() && no_mods {
                    let mut handled = true;
                    match key {
                        Key::Escape => {
                            slot.ui.completion_popup = None;
                        }
                        Key::Enter => {
                            // Enter always commits the selected
                            // candidate and closes the popup.
                            if let Some(popup) = slot.ui.completion_popup.take() {
                                let cursor = slot
                                    .session
                                    .blocks()
                                    .editor_on_tail()
                                    .map(|e| e.cursor())
                                    .unwrap_or(0);
                                let current_token_len = cursor.saturating_sub(popup.origin_byte);
                                if let Some(editor) = slot.session.editor_mut() {
                                    popup.accept(editor, current_token_len);
                                }
                                slot.session.clear_history_recall();
                            }
                        }
                        Key::Tab => {
                            // Smart-Tab: extend the typed token to
                            // the longest prefix shared by selected
                            // AND at least one other candidate.
                            // - extension == selected.value → commit.
                            // - extension.len() > current → extend,
                            //   leave popup open, live-filter next
                            //   frame.
                            // - extension == current → no-op; user
                            //   picks via Up/Down + Enter.
                            let Some(popup_ref) = slot.ui.completion_popup.as_ref() else {
                                continue;
                            };
                            let cursor = slot
                                .session
                                .blocks()
                                .editor_on_tail()
                                .map(|e| e.cursor())
                                .unwrap_or(0);
                            let current_token_len = cursor.saturating_sub(popup_ref.origin_byte);
                            let extension = popup_ref.tab_extend(current_token_len).to_string();
                            let selected_full = popup_ref.selected().value.clone();
                            if extension == selected_full {
                                if let Some(popup) = slot.ui.completion_popup.take()
                                    && let Some(editor) = slot.session.editor_mut()
                                {
                                    popup.accept(editor, current_token_len);
                                }
                                slot.session.clear_history_recall();
                            } else if extension.len() > current_token_len {
                                if let Some(editor) = slot.session.editor_mut() {
                                    let origin = popup_ref.origin_byte;
                                    editor.replace_range(origin, cursor, &extension);
                                }
                                slot.session.clear_history_recall();
                                // Popup stays; live filter next frame.
                            } else {
                                // No further extension possible —
                                // the visible candidates diverge
                                // past the current token. User has
                                // already narrowed the list; same
                                // muscle (Tab) should now commit
                                // the selected candidate instead
                                // of being a no-op.
                                if let Some(popup) = slot.ui.completion_popup.take()
                                    && let Some(editor) = slot.session.editor_mut()
                                {
                                    popup.accept(editor, current_token_len);
                                }
                                slot.session.clear_history_recall();
                            }
                        }
                        Key::ArrowDown => {
                            if let Some(popup) = slot.ui.completion_popup.as_mut() {
                                popup.move_selection(1);
                            }
                        }
                        Key::ArrowUp => {
                            if let Some(popup) = slot.ui.completion_popup.as_mut() {
                                popup.move_selection(-1);
                            }
                        }
                        _ => {
                            // Non-navigation key. Let it fall
                            // through to the editor for normal
                            // edit handling; the after-loop live-
                            // filter pass below will recompute
                            // candidates with the new buffer.
                            handled = false;
                        }
                    }
                    if handled {
                        continue;
                    }
                } else if editor_active
                    && slot.ui.completion_popup.is_none()
                    && matches!(key, Key::Tab)
                    && no_mods
                {
                    // Tab in editor with no popup → open the popup.
                    let editor_text;
                    let cursor;
                    {
                        let editor = slot.session.blocks().editor_on_tail();
                        editor_text = editor.map(|e| e.text().to_string()).unwrap_or_default();
                        cursor = editor.map(|e| e.cursor()).unwrap_or(0);
                    }
                    let cwd = slot.session.terminal().cwd().map(|p| p.to_path_buf());
                    let history_entries = slot.session.history_for_completion(200);
                    let popup = crate::completion::open_completion_at(
                        &editor_text,
                        cursor,
                        cwd.as_deref(),
                        home,
                        || history_entries,
                    );
                    if popup.is_some() {
                        slot.ui.completion_popup = popup;
                    }
                    // Consume Tab whether or not the popup opened.
                    continue;
                }
            }
            if editor_active && apply_event_to_editor(event, slot) {
                continue;
            }
            // Boundary gate (spec/04): while the editor owns the line,
            // the editor consumed everything above except the EOF edge
            // case. Anything else reaching here — a control chord the
            // editor didn't claim (Ctrl+C, Ctrl+X/Y/Z, …) — must NOT
            // leak a raw C0 byte to the shell, or it buffers and
            // resurfaces prefixed to the next command (and `\x03` would
            // print a cosmetic `^C` at the idle prompt).
            // `pty_passthrough_allowed` permits only Ctrl+D on an EMPTY
            // editor (EOF, to exit an idle shell).
            let editor_empty =
                slot.session.blocks().editor_on_tail().map(|e| e.is_empty()).unwrap_or(true);
            if !pty_passthrough_allowed(event, editor_active, editor_empty) {
                continue;
            }
            if let Some(bytes) = input::encode_event(event, modes) {
                let _ = slot.session.write(&bytes);
            }
        }

        // ---- Completion popup live-filter ----------------------
        //
        // Only refresh when the editor's text/cursor actually
        // changed this frame. Navigation-only frames (Up/Down in
        // popup, idle frames) leave the buffer alone, and we
        // MUST NOT replace the popup in those cases or
        // `move_selection`'s state (selected_index +
        // scroll_to_selected_pending) gets clobbered.
        //
        // When text DID change: if the caret went past
        // `origin_byte` (user backspaced through the token start),
        // dismiss. Otherwise recompute the candidate list with
        // the new buffer state. `selected_index` resets to 0
        // (preserving it across refilter is a polish item).
        if slot.ui.completion_popup.is_some() {
            let editor_state_after: Option<(String, usize)> =
                slot.session.blocks().editor_on_tail().map(|e| (e.text().to_string(), e.cursor()));
            let buffer_changed = editor_state_before != editor_state_after;
            if buffer_changed && let Some((editor_text, cursor)) = editor_state_after {
                let origin = slot.ui.completion_popup.as_ref().map(|p| p.origin_byte).unwrap_or(0);
                if cursor < origin {
                    slot.ui.completion_popup = None;
                } else {
                    let cwd = slot.session.terminal().cwd().map(|p| p.to_path_buf());
                    let history_entries = slot.session.history_for_completion(200);
                    let new_popup = crate::completion::open_completion_at(
                        &editor_text,
                        cursor,
                        cwd.as_deref(),
                        home,
                        || history_entries,
                    );
                    slot.ui.completion_popup = new_popup;
                }
            }
        }
    }

    // ---- visible bell ------------------------------------------
    //
    // The shell can ring the terminal bell at any time (the BEL
    // byte, `\a`). The alacritty event listener (`TerminalEventTracker`)
    // captures it; we compare the live count against the last
    // value we observed for this pane to detect a new bell. On
    // detection the flash starts; it then linearly fades over
    // `BELL_FLASH_SECS` and clears itself. We also schedule a
    // repaint while the flash is active so the fade animates
    // even when the user isn't producing input events.
    let now = ctx.input(|i| i.time);
    let current_bell = slot.session.terminal().bell_count();
    if current_bell > slot.ui.bell_last_seen {
        slot.ui.bell_last_seen = current_bell;
        slot.ui.bell_flash_started_at = Some(now);
    }
    if let Some(started_at) = slot.ui.bell_flash_started_at {
        let elapsed = (now - started_at).max(0.0);
        if elapsed >= BELL_FLASH_SECS {
            slot.ui.bell_flash_started_at = None;
        } else {
            let alpha_factor = 1.0 - (elapsed / BELL_FLASH_SECS) as f32;
            paint_bell_flash_border(ui.painter(), ui.max_rect(), alpha_factor);
            let remaining_ms = ((BELL_FLASH_SECS - elapsed) * 1000.0).max(0.0) as u64;
            ctx.request_repaint_after(std::time::Duration::from_millis(remaining_ms.min(16)));
        }
    }

    // Phase 4J PR 6: paint the Ctrl+R history overlay on top of
    // everything else if it's open, and apply its returned action
    // (Submit / Cancel / ToggleScope). The overlay's `TextEdit` +
    // `ctx.input` navigation checks own the input; the event loop
    // above just blocked the editor / PTY paths while it's open.
    if let Some(action) = crate::history_overlay::paint(ui, slot) {
        use crate::history_overlay::OverlayAction;
        match action {
            OverlayAction::Submit(text) => {
                slot.session.replace_editor_buffer(&text);
                slot.ui.history_overlay = None;
                // Submit closes the overlay; the keyboard belongs
                // to THIS pane's editor (we just dropped a command
                // into it). Without this, egui's focus migrates
                // to whichever widget gets activated next — in a
                // split-screen layout that's typically the first
                // tile's active tab, NOT the pane the user just
                // submitted into.
                slot.ui.needs_focus = true;
            }
            OverlayAction::Cancel => {
                slot.ui.history_overlay = None;
                slot.ui.needs_focus = true;
            }
            OverlayAction::ToggleScope => {
                if let Some(history) = slot.session.history_ctx().cloned()
                    && let Some(overlay) = slot.ui.history_overlay.as_mut()
                {
                    overlay.toggle_scope();
                    overlay.refresh_entries(&history, slot.session.pane_id());
                    let cwd = slot.session.terminal().cwd().map(|p| p.display().to_string());
                    overlay.rerank(cwd.as_deref());
                }
            }
        }
    }

    // Debug overlay: when `TERMICA_DEBUG_PANE_STATE=1` is set in
    // the environment, paint a small text box in the top-left of
    // each pane showing its mode-machine + block-stack state.
    // Used to diagnose the "tab 1 enters RawTerminal when Ctrl+R
    // closes in tab 2" intermittent bug and similar focus-perturbation
    // edge cases. Costs nothing when the env var is unset (just a
    // single env_var lookup per pane per frame, which is cached
    // after the first call). Not part of the production UI.
    if std::env::var("TERMICA_DEBUG_PANE_STATE").is_ok_and(|v| v == "1") {
        let mode = slot.session.pane_mode();
        let tail = match slot.session.blocks().last() {
            Some(crate::block::Block::Prompt { .. }) => "Prompt",
            Some(crate::block::Block::Running { .. }) => "Running",
            Some(crate::block::Block::Sealed { .. }) => "Sealed",
            None => "<empty>",
        };
        let editor = slot.session.blocks().editor_on_tail().is_some();
        let focused = focus_response.has_focus();
        let pid = slot.session.pane_id();
        let alt_flag = slot.session.terminal().is_alternate_screen();
        let last = slot.session.controller().last_transition().clone();
        let text = format!(
            "pane={pid:?} focus={focused} mode={mode:?} tail={tail} editor={editor} \
             altflag={alt_flag} last: {:?}\u{2192}{:?} {:?}@{}",
            last.from, last.to, last.reason, last.at,
        );
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new(("pane-debug", pid)),
        ));
        let origin = ui.max_rect().min + egui::vec2(4.0, 4.0);
        // Black-on-yellow chip so it can't be mistaken for shell output.
        let galley =
            painter.layout_no_wrap(text, egui::FontId::monospace(11.0), egui::Color32::BLACK);
        let bg = egui::Rect::from_min_size(origin, galley.size() + egui::vec2(6.0, 4.0));
        painter.rect_filled(bg, 2.0, egui::Color32::from_rgb(0xff, 0xe8, 0x80));
        painter.galley(origin + egui::vec2(3.0, 2.0), galley, egui::Color32::BLACK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- compute_footer_height (Phase 4D) -----------------------------

    #[test]
    fn footer_is_zero_when_tail_is_not_prompt() {
        assert_eq!(compute_footer_height(false, true, 1, true, 20.0), 0.0);
    }

    #[test]
    fn footer_is_zero_when_editor_inactive() {
        assert_eq!(compute_footer_height(true, false, 1, true, 20.0), 0.0);
    }

    #[test]
    fn single_line_editor_with_cwd_includes_chip_padding_and_descender_pad() {
        // chrome (row_h + 2 * CHIP_PAD_Y) + 1 editor row of row_h
        // + descender pad.
        let chip_pad = 2.0 * render::CHIP_PAD_Y;
        assert_eq!(
            compute_footer_height(true, true, 1, true, 20.0),
            40.0 + chip_pad + FOOTER_DESCENDER_PAD
        );
    }

    #[test]
    fn single_line_editor_without_cwd_yields_one_row_plus_descender_pad() {
        // No chrome row when cwd is unknown — no chip padding either.
        assert_eq!(compute_footer_height(true, true, 1, false, 20.0), 20.0 + FOOTER_DESCENDER_PAD);
    }

    #[test]
    fn three_line_editor_with_cwd_includes_chip_padding_and_descender_pad() {
        let chip_pad = 2.0 * render::CHIP_PAD_Y;
        assert_eq!(
            compute_footer_height(true, true, 3, true, 20.0),
            80.0 + chip_pad + FOOTER_DESCENDER_PAD
        );
    }

    #[test]
    fn zero_editor_lines_treated_as_one() {
        // An empty editor still needs a caret row.
        let chip_pad = 2.0 * render::CHIP_PAD_Y;
        assert_eq!(
            compute_footer_height(true, true, 0, true, 20.0),
            40.0 + chip_pad + FOOTER_DESCENDER_PAD
        );
    }

    // ---- classify_editor_motion (per-OS keybindings) -----------------

    fn mods(ctrl: bool, alt: bool, shift: bool, command: bool) -> egui::Modifiers {
        egui::Modifiers { ctrl, alt, shift, command, mac_cmd: false }
    }

    #[test]
    fn classify_macos_option_arrow_is_word_move() {
        let m = mods(false, true, false, false);
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowLeft, m, true),
            Some((EditorMotion::WordLeft, false))
        );
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowRight, m, true),
            Some((EditorMotion::WordRight, false))
        );
    }

    #[test]
    fn classify_macos_cmd_arrow_is_line_or_doc_move() {
        let m = mods(false, false, false, true);
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowLeft, m, true),
            Some((EditorMotion::LineStart, false))
        );
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowRight, m, true),
            Some((EditorMotion::LineEnd, false))
        );
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowUp, m, true),
            Some((EditorMotion::DocStart, false))
        );
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowDown, m, true),
            Some((EditorMotion::DocEnd, false))
        );
    }

    #[test]
    fn classify_macos_shift_toggles_extending() {
        let m = mods(false, true, true, false); // Option+Shift
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowLeft, m, true),
            Some((EditorMotion::WordLeft, true))
        );
    }

    #[test]
    fn classify_linux_ctrl_arrow_is_word_move() {
        let m = mods(true, false, false, false);
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowLeft, m, false),
            Some((EditorMotion::WordLeft, false))
        );
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowRight, m, false),
            Some((EditorMotion::WordRight, false))
        );
    }

    #[test]
    fn classify_linux_ctrl_home_end_is_doc_move() {
        let m = mods(true, false, false, false);
        assert_eq!(
            classify_editor_motion(egui::Key::Home, m, false),
            Some((EditorMotion::DocStart, false))
        );
        assert_eq!(
            classify_editor_motion(egui::Key::End, m, false),
            Some((EditorMotion::DocEnd, false))
        );
    }

    #[test]
    fn classify_linux_ctrl_shift_extends() {
        let m = mods(true, false, true, false);
        assert_eq!(
            classify_editor_motion(egui::Key::ArrowRight, m, false),
            Some((EditorMotion::WordRight, true))
        );
    }

    #[test]
    fn classify_bare_arrow_returns_none() {
        let m = mods(false, false, false, false);
        assert_eq!(classify_editor_motion(egui::Key::ArrowLeft, m, true), None);
        assert_eq!(classify_editor_motion(egui::Key::ArrowLeft, m, false), None);
    }

    #[test]
    fn classify_macos_option_and_cmd_together_returns_none() {
        // Option+Cmd+arrow is reserved for OS-level / app-level
        // shortcuts on macOS; editor shouldn't claim it.
        let m = mods(false, true, false, true);
        assert_eq!(classify_editor_motion(egui::Key::ArrowLeft, m, true), None);
    }

    // ---- pty_passthrough_allowed (editor owns the line) --------------
    //
    // spec/04: while the editor owns the line (`ShellPromptEditor`) the
    // shell is idle at a confirmed prompt — there is no foreground
    // program to interrupt — so the ONLY keystroke that may reach the
    // PTY is Ctrl+D (EOF) on an EMPTY editor (exit an idle shell).
    // Ctrl+C is fully inert here: a SIGINT would only print a cosmetic
    // `^C`. (Interrupting a running program happens in `RawTerminal`,
    // `editor_active = false`, which passes everything.) Every other
    // chord is the editor's; leaking a raw C0 byte to the shell buffers
    // it and surfaces it prefixed to the next command (`^X^Y^Z…`).

    fn key_ev(key: egui::Key, m: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: m }
    }

    #[test]
    fn editor_swallows_ctrl_c_and_other_non_eof_chords() {
        let ctrl = mods(true, false, false, false);
        // Ctrl+C is included now: it never reaches the PTY from the
        // editor, empty or not (no cosmetic `^C` at an idle prompt).
        for key in [egui::Key::C, egui::Key::X, egui::Key::Y, egui::Key::Z, egui::Key::B] {
            assert!(
                !pty_passthrough_allowed(&key_ev(key, ctrl), true, true),
                "Ctrl+{key:?} leaked to the PTY (empty editor)"
            );
            assert!(
                !pty_passthrough_allowed(&key_ev(key, ctrl), true, false),
                "Ctrl+{key:?} leaked to the PTY (non-empty editor)"
            );
        }
    }

    #[test]
    fn editor_passes_eof_only_when_empty() {
        let ctrl = mods(true, false, false, false);
        // Ctrl+D (EOF) on an empty editor exits the idle shell.
        assert!(pty_passthrough_allowed(&key_ev(egui::Key::D, ctrl), true, true));
        // On a typed line it's swallowed — the editor owns the line.
        assert!(!pty_passthrough_allowed(&key_ev(egui::Key::D, ctrl), true, false));
    }

    #[test]
    fn linux_ctrl_d_sets_command_but_still_passes_when_empty() {
        // egui maps Ctrl→`command` on Linux/Windows; `mac_cmd` stays
        // false, so the EOF chord check must still recognise it. Ctrl+C
        // (also `command`) stays inert.
        let linux_ctrl = mods(true, false, false, true); // ctrl + command
        assert!(pty_passthrough_allowed(&key_ev(egui::Key::D, linux_ctrl), true, true));
        assert!(!pty_passthrough_allowed(&key_ev(egui::Key::C, linux_ctrl), true, true));
    }

    #[test]
    fn ctrl_shift_d_is_not_an_eof_chord() {
        // Shift must exclude the chord (avoid accidental EOF combos).
        let ctrl_shift = mods(true, false, true, false);
        assert!(!pty_passthrough_allowed(&key_ev(egui::Key::D, ctrl_shift), true, true));
    }

    #[test]
    fn raw_terminal_passes_every_keystroke() {
        // editor_active = false → the program below owns stdin; every
        // event (including Ctrl+C, the interrupt path) goes straight to
        // the PTY.
        let ctrl = mods(true, false, false, false);
        assert!(pty_passthrough_allowed(&key_ev(egui::Key::X, ctrl), false, true));
        assert!(pty_passthrough_allowed(&key_ev(egui::Key::C, ctrl), false, false));
    }

    // ---- Ctrl+C on the editor ----------------------------------------
    //
    // Behavioral test through `apply_event_to_editor` on a real
    // editor-active session: Ctrl+C is a no-op — on a typed line the
    // text is untouched, on an empty editor nothing is sent (the
    // boundary gate swallows it either way; no cosmetic `^C`).

    fn spawn_editor_active_slot() -> PaneSlot {
        use crate::pane::PaneSession;
        use crate::pane_slot::PaneUiState;
        use crate::pty::PtyConfig;
        use std::time::{Duration, Instant};

        // Same DCS promote sequence the pane tests use: integration_ready
        // confirms integration, precmd promotes the mode; `cat` keeps the
        // PTY open afterwards.
        let cmd = "printf '\\033PTermica;{\"type\":\"integration_ready\",\
                   \"session\":\"t\",\"value\":{\"shell\":\"zsh\",\"version\":1}}\\033\\\\\
                   \\033PTermica;{\"type\":\"precmd\",\
                   \"session\":\"t\",\"value\":\"/tmp\"}\\033\\\\'; cat";
        let config = PtyConfig {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), cmd.into()],
            ..PtyConfig::default()
        };
        let mut session = PaneSession::spawn(5, 40, &config, "t".into(), 0, None).expect("spawn");
        let stop = Instant::now() + Duration::from_secs(2);
        while !session.editor_is_active() {
            session.drain();
            if Instant::now() >= stop {
                panic!("never reached ShellPromptEditor");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        PaneSlot { session, ui: PaneUiState::default() }
    }

    #[test]
    fn ctrl_c_on_typed_line_leaves_text_untouched() {
        let mut slot = spawn_editor_active_slot();
        slot.session.editor_mut().unwrap().insert_str("echo too bad");
        let ctrl_c = key_ev(egui::Key::C, mods(true, false, false, false));
        // The editor does NOT consume it (falls through to the gate,
        // which swallows it because the editor is non-empty) and the
        // typed line is left exactly as it was — Ctrl+C is a no-op on a
        // typed line, never a line-discard.
        assert!(!apply_event_to_editor(&ctrl_c, &mut slot));
        assert_eq!(slot.session.blocks().editor_on_tail().unwrap().text(), "echo too bad");
    }

    #[test]
    fn ctrl_c_on_empty_editor_is_inert() {
        let mut slot = spawn_editor_active_slot();
        assert!(slot.session.blocks().editor_on_tail().unwrap().is_empty());
        let ctrl_c = key_ev(egui::Key::C, mods(true, false, false, false));
        // apply_event_to_editor doesn't consume it; the boundary gate
        // then swallows it (Ctrl+C is not the EOF chord), so no `\x03`
        // reaches the shell — no cosmetic `^C` at an idle prompt. The
        // editor is left empty and active.
        assert!(!apply_event_to_editor(&ctrl_c, &mut slot));
        assert!(!pty_passthrough_allowed(&ctrl_c, true, true));
        assert!(slot.session.editor_is_active());
        assert!(slot.session.blocks().editor_on_tail().unwrap().is_empty());
    }

    #[test]
    fn esc_is_inert_and_keeps_editor_mode() {
        // Esc used to demote ShellPromptEditor → RawTerminal; it is now
        // a consumed no-op. The pane stays in the editor and the typed
        // line is preserved. (The demote machinery still exists, just
        // unbound — see the Key::Escape arm.)
        let mut slot = spawn_editor_active_slot();
        slot.session.editor_mut().unwrap().insert_str("echo hi");
        assert!(slot.session.editor_is_active());
        let esc = key_ev(egui::Key::Escape, mods(false, false, false, false));
        assert!(apply_event_to_editor(&esc, &mut slot)); // consumed, no PTY leak
        assert!(slot.session.editor_is_active(), "Esc must not demote out of the editor");
        assert_eq!(slot.session.blocks().editor_on_tail().unwrap().text(), "echo hi");
    }

    #[test]
    fn cells_from_pixels_floors_dimensions() {
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(800.0, 400.0), 10.0, 20.0);
        assert_eq!((rows, cols), (20, 80));
    }

    #[test]
    fn cells_from_pixels_ignores_fractional_remainder() {
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(805.0, 405.0), 10.0, 20.0);
        assert_eq!((rows, cols), (20, 80));
    }

    #[test]
    fn cells_from_pixels_clamps_to_minimum() {
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(1.0, 1.0), 10.0, 20.0);
        assert_eq!((rows, cols), (MIN_ROWS, MIN_COLS));
    }

    #[test]
    fn cells_from_pixels_handles_zero_metrics() {
        let (rows, cols) = cells_from_pixels(egui::Vec2::new(800.0, 400.0), 0.0, 20.0);
        assert_eq!(cols, MIN_COLS);
        let (rows2, _cols2) = cells_from_pixels(egui::Vec2::new(800.0, 400.0), 10.0, 0.0);
        assert_eq!(rows2, MIN_ROWS);
        let _ = rows;
    }
}
