//! Per-pane rendering: status header, PTY resize, link / path scan,
//! terminal grid paint, mouse selection, scroll wheel, keyboard
//! input, and app-shortcut detection. Same code path runs for every
//! visible pane regardless of layout (Phase 2A's "one pane in one
//! tab" or Phase 2B's "multiple panes across splits").

use std::path::Path;

use alacritty_terminal::grid::Dimensions;
use eframe::egui;

use crate::links::{self, LinkSpan};
use crate::pane::PaneView;
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

/// Compute the fixed-footer height for the [`Block::Prompt`] block
/// per [spec/04 §"Layout"](../spec/04-prompt-editor.md#layout-fixed-footer-prompt-sticky-top-header).
///
/// The footer is the dedicated home for the editor: a cwd chip row
/// (if a cwd is known) on top of one row per editor line. When the
/// tail is **not** a `Prompt` or the editor is inactive (e.g.,
/// bootstrapping, alt-screen, mode demote), there is no footer and
/// the scroll area extends to the pane bottom — returns `0.0`.
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
    let chrome_rows = if has_cwd { 1 } else { 0 };
    (chrome_rows + editor_rows) as f32 * row_h
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
        // If a selection is active, `insert_str` deletes it first.
        Event::Text(s) => {
            if let Some(editor) = slot.session.editor_mut() {
                editor.insert_str(s);
            }
            true
        }
        // Cmd+V (and middle-click paste) — egui pre-resolves the
        // clipboard contents into this event. Editor consumes; the
        // (unused-here) PTY paste path stays for `RawTerminal`.
        Event::Paste(s) => {
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
            // Word / line / doc boundary moves. Bindings differ by
            // OS — `classify_editor_motion` encodes both conventions.
            if let Some((motion, extending)) =
                classify_editor_motion(*key, *modifiers, cfg!(target_os = "macos"))
                && let Some(editor) = slot.session.editor_mut()
            {
                motion.apply(editor, extending);
                return true;
            }
            // Alt + Cmd combos (other than the moves above) bypass
            // the editor — they belong to app-level shortcuts. Plain
            // Cmd combos (Cmd+C/X) are handled in render_pane proper
            // because they need `ctx` for clipboard access.
            if modifiers.command || modifiers.alt {
                return false;
            }
            // Consume the Ctrl combos that are Termica-editor
            // territory but not yet wired (history walk → 4J,
            // completion → 4I). Without consuming, ZLE-off shells
            // receive the raw `\x12` / `\x10` / `\x0e` bytes and the
            // kernel echoes them as `^R` / `^P` / `^N` literals into
            // the live `Term` — visible noise. `Ctrl+C` (`\x03`,
            // SIGINT) and `Ctrl+D` (`\x04`, EOF) deliberately stay
            // PTY-bound per spec/04.
            if modifiers.ctrl
                && !modifiers.shift
                && matches!(key, Key::R | Key::P | Key::N | Key::S | Key::G)
            {
                return true;
            }
            // The keys that own `slot.session` mutably (submit,
            // demote) are handled BEFORE the editor borrow so the
            // borrow checker sees a clean window.
            match key {
                Key::Enter if !modifiers.shift => {
                    let _ = slot.session.submit_editor_command();
                    return true;
                }
                Key::Escape => {
                    slot.session.leave_editor_esc();
                    return true;
                }
                _ => {}
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
                Key::ArrowUp | Key::ArrowDown => true,
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

/// Render the workspace's status header into `ui`.
///
/// Pure UI: takes a plain [`PaneView`] snapshot, never touches OS
/// resources, safe to drive from `egui_kittest` snapshot tests.
/// The cell grid is painted separately by [`render::paint_terminal`]
/// directly below this header.
pub fn central_panel(ui: &mut egui::Ui, view: &PaneView) {
    ui.heading("Termica");
    ui.label(format!(
        "Phase 2A — tabs live. \
         Bytes: {}   ·   alt-screen: {}   ·   grid: {}×{}",
        view.bytes_received, view.alt_screen, view.rows, view.cols
    ));
    if let Some(cwd) = &view.cwd {
        ui.label(format!("cwd: {}", cwd.display()));
    }
    ui.separator();
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

    // ---- status header ------------------------------------------
    let view = slot.session.view();
    central_panel(ui, &view);

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
    // Read the sealed-block selection up front. The paint pass below
    // gets a `Some((start, end))` for the matching sealed block; the
    // click / drag handler farther down updates it on user input. The
    // visual reflects last frame's state — a one-frame lag is invisible.
    let block_sel = slot.session.block_selection().copied();

    // Alt-screen mode (vim / less / htop / fzf / ssh-with-TUI):
    // the program below owns the whole grid and manages its own
    // scrolling. Termica's block model and editor have no role
    // here — both are hidden so the user sees what they'd see in
    // any other modern terminal. The flag is re-evaluated per
    // frame; on exit we fall back to the normal block layout.
    let in_alt_screen = view.alt_screen;
    // Capture each sealed block's painted rect during the loop so the
    // click / drag handler below can hit-test against them. Empty in
    // alt-screen mode (we don't paint blocks at all there).
    let mut sealed_rects: Vec<(crate::block::BlockId, egui::Rect)> = Vec::new();

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

    let scroll_inner = egui::ScrollArea::vertical()
        .id_salt(("pane-blocks", slot.session.pane_id()))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .max_height(scroll_max_h)
        .show(ui, |ui| {
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
                        crate::block::Block::Sealed {
                            id, command, snapshot, header, exit, ..
                        } => {
                            let sel_for_this = block_sel
                                .filter(|s| s.block_id == *id && !s.is_empty())
                                .map(|s| s.ordered());
                            let resp = render::paint_sealed_block(
                                ui,
                                command,
                                snapshot,
                                sel_for_this,
                                header.cwd.as_deref(),
                                *exit,
                            );
                            sealed_rects.push((*id, resp.rect));
                            // Thin separator gap — spec/04 §"Visual
                            // structure" calls for "non-text (thin
                            // separator + space)".
                            ui.add_space(4.0);
                        }
                        crate::block::Block::Running { command, header, .. } => {
                            let _ = render::paint_block_header(ui, header.cwd.as_deref(), None);
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
            let rendered = render::paint_terminal(
                ui,
                slot.session.terminal(),
                selection.as_ref(),
                highlighted_link,
                hide_term_cursor,
            );

            // (Phase 4D) The editor + cwd chip have moved out of
            // this scroll area; they paint in the fixed footer below
            // it. The live `Term`'s cursor is still hidden by
            // `hide_term_cursor` above so the empty `PS1=''` cursor
            // row doesn't show a stray block.

            (rendered, links_in_view, highlighted_link.cloned())
        });
    let (rendered, links_in_view, highlighted_link) = scroll_inner.inner;
    let highlighted_link = highlighted_link.as_ref();
    if highlighted_link.is_some() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    // ---- Phase 4D fixed-footer: cwd chip + editor ----------------
    //
    // The ScrollArea above was constrained to leave `footer_h` of
    // vertical space at the bottom. Paint the prompt's cwd chip
    // (when known) and then the editor here. `footer_rect` is the
    // exact rect the editor occupies — used by the click / drag
    // handlers below to translate pointer pixels to byte offsets.
    let footer_rect: Option<egui::Rect> = if footer_h > 0.0 {
        let chip_h = if has_prompt_cwd { row_h } else { 0.0 };
        let editor_h = footer_h - chip_h;
        let footer_origin = ui.next_widget_position();

        if has_prompt_cwd
            && let Some(crate::block::Block::Prompt { header, .. }) = slot.session.blocks().last()
        {
            let _ = render::paint_block_header(ui, header.cwd.as_deref(), None);
        }

        // Allocate the editor strip and paint into it. We use an
        // explicit rect rather than `paint_prompt_editor` (which
        // allocates its own size) so the click / drag hit-test rect
        // matches exactly what we painted.
        let editor_rect = egui::Rect::from_min_size(
            egui::Pos2::new(footer_origin.x, footer_origin.y + chip_h),
            egui::Vec2::new(ui.available_width(), editor_h),
        );
        let _ = ui.allocate_rect(editor_rect, egui::Sense::hover());

        if let Some(editor) = slot.session.blocks().editor_on_tail() {
            let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
            // Caret blink: ~1.6 Hz square wave; request a repaint at
            // the next half-cycle so it keeps toggling on idle.
            let time = ctx.input(|i| i.time);
            let caret_visible = (time * 1.6) as i64 % 2 == 0;
            ctx.request_repaint_after(std::time::Duration::from_millis(312));
            let painter = ui.painter_at(editor_rect);
            render::paint_prompt_editor_at(
                &painter,
                editor,
                editor_rect.min,
                cell_w,
                row_h,
                &font_id,
                caret_visible,
            );
        }
        Some(editor_rect)
    } else {
        None
    };

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
    // Clicking inside the grid does three things at once:
    //   1. Grabs keyboard focus (so the keyboard gate below lets
    //      this pane's events through).
    //   2. Either opens a Cmd/Ctrl-clicked URL or starts a selection
    //      (multi-click counter for char/word/line mode).
    //   3. Resets click-counter timing.
    let geom_after_paint = rendered.geometry;
    let to_point = |pos: egui::Pos2| pixel_to_grid_point(pos.x, pos.y, geom_after_paint);
    let (primary_pressed, press_origin, primary_down, now) = ctx.input(|i| {
        (i.pointer.primary_pressed(), i.pointer.press_origin(), i.pointer.primary_down(), i.time)
    });

    // Editor hit-area: the fixed footer below the scroll area.
    // Phase 4D anchors the editor to the viewport bottom, so the
    // hit-test rect is exactly the footer rect we painted above.
    // `None` when no footer was painted (alt-screen, tail not
    // Prompt, or editor inactive).
    let editor_rect: Option<egui::Rect> = footer_rect;

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

    let click_in_editor = editor_rect.is_some_and(|r| press_origin.is_some_and(|p| r.contains(p)));

    // Sealed-block hit-test: which block (if any) did the press land
    // inside? Used by the click / drag handler below to start or
    // extend a `BlockSelection`. Computed before the alacritty branch
    // so a press on a sealed snapshot doesn't fall through to the
    // live-grid path. The editor overlay takes precedence (it's
    // painted on top of the live `Term`'s prompt row).
    let press_in_sealed_block: Option<(crate::block::BlockId, egui::Rect)> =
        if !click_in_editor && let Some(pos) = press_origin {
            sealed_rects.iter().find(|(_, r)| r.contains(pos)).copied()
        } else {
            None
        };

    /// Translate a pointer pixel position inside a sealed-block rect
    /// to a `BlockCursor` (row, col). Clamps to the block's bounds —
    /// pointer above → (0, 0); below → (last_row, last_col); to the
    /// left → col 0; to the right → row_len. Out-of-rows clamps the
    /// row; per-row col is clamped by the caller using
    /// `sealed_row_len`.
    fn sealed_cursor_for_pos(
        rect: egui::Rect,
        pos: egui::Pos2,
        cell_w: f32,
        row_h: f32,
        block_rows: usize,
    ) -> crate::block_selection::BlockCursor {
        let dy = (pos.y - rect.min.y).max(0.0);
        let dx = (pos.x - rect.min.x).max(0.0);
        let row = ((dy / row_h) as usize).min(block_rows.saturating_sub(1));
        let col = (dx / cell_w) as usize;
        crate::block_selection::BlockCursor::new(row, col)
    }

    if !modal_open
        && primary_pressed
        && let Some(pos) = press_origin
        && (rendered.response.rect.contains(pos) || press_in_sealed_block.is_some())
    {
        rendered.response.request_focus();

        if let Some((block_id, block_rect)) = press_in_sealed_block {
            // Sealed-block press. Single → place an empty selection
            // at the clicked cell (drag extends it); double → select
            // the word; triple → select the line. Same multi-click
            // counter that drives editor + live grid.
            let dt = now - slot.ui.last_press_time;
            let dist = (pos - slot.ui.last_press_pos).length();
            if dt < MULTI_CLICK_WINDOW_SECS && dist < MULTI_CLICK_DISTANCE_PX {
                slot.ui.click_count = (slot.ui.click_count + 1).min(3);
            } else {
                slot.ui.click_count = 1;
            }
            slot.ui.last_press_time = now;
            slot.ui.last_press_pos = pos;

            let rows = slot.session.sealed_snapshot_rows(block_id).unwrap_or(0);
            let mut cursor = sealed_cursor_for_pos(block_rect, pos, cell_w, row_h, rows);
            // Clamp col to the per-row length so an empty / short row
            // doesn't yield a col past the end.
            if let Some(row_len) = slot.session.sealed_row_len(block_id, cursor.row) {
                cursor.col = cursor.col.min(row_len);
            }

            match slot.ui.click_count {
                2 => {
                    if let Some((a, b)) = slot.session.sealed_word_range(block_id, cursor) {
                        slot.ui.sealed_drag_anchor = Some((block_id, a, b));
                        slot.session.set_block_selection(
                            crate::block_selection::BlockSelection::new(block_id, a, b),
                        );
                    }
                }
                3 => {
                    if let Some((a, b)) = slot.session.sealed_line_range(block_id, cursor) {
                        slot.ui.sealed_drag_anchor = Some((block_id, a, b));
                        slot.session.set_block_selection(
                            crate::block_selection::BlockSelection::new(block_id, a, b),
                        );
                    }
                }
                _ => {
                    slot.ui.sealed_drag_anchor = None;
                    slot.session.set_block_selection(crate::block_selection::BlockSelection::new(
                        block_id, cursor, cursor,
                    ));
                }
            }
        } else if click_in_editor && let Some(rect) = editor_rect {
            // Pressing in the editor (or in the live grid below)
            // ends any sealed-block selection — only one "current
            // selection" per pane.
            slot.session.clear_block_selection();
            slot.ui.sealed_drag_anchor = None;
            // Editor click. Single → place cursor; double → select
            // word; triple → select line. Same multi-click counter
            // the alacritty path uses.
            let dt = now - slot.ui.last_press_time;
            let dist = (pos - slot.ui.last_press_pos).length();
            if dt < MULTI_CLICK_WINDOW_SECS && dist < MULTI_CLICK_DISTANCE_PX {
                slot.ui.click_count = (slot.ui.click_count + 1).min(3);
            } else {
                slot.ui.click_count = 1;
            }
            slot.ui.last_press_time = now;
            slot.ui.last_press_pos = pos;

            let editor_text = slot
                .session
                .blocks()
                .editor_on_tail()
                .map(|e| e.text().to_string())
                .unwrap_or_default();
            let byte = editor_byte_for_pos(rect, pos, &editor_text, cell_w, row_h);
            // Compute and remember the original anchor range so the
            // drag handler can extend selection by word / line.
            slot.ui.editor_drag_anchor = match slot.ui.click_count {
                2 => Some(crate::prompt_editor::word_range_at(&editor_text, byte)),
                3 => Some(crate::prompt_editor::line_range_at(&editor_text, byte)),
                _ => None,
            };
            if let Some(editor) = slot.session.editor_mut() {
                match slot.ui.click_count {
                    1 => editor.set_cursor(byte),
                    2 => editor.select_word_at(byte),
                    _ => editor.select_line_at(byte),
                }
            }
        } else {
            let press_pt = to_point(pos);
            let link_under_press = links_in_view.iter().find(|l| l.contains(press_pt)).cloned();

            if modifier_held && let Some(link) = link_under_press {
                open_url(&link.url);
            } else {
                let dt = now - slot.ui.last_press_time;
                let dist = (pos - slot.ui.last_press_pos).length();
                if dt < MULTI_CLICK_WINDOW_SECS && dist < MULTI_CLICK_DISTANCE_PX {
                    slot.ui.click_count = (slot.ui.click_count + 1).min(3);
                } else {
                    slot.ui.click_count = 1;
                }
                slot.ui.last_press_time = now;
                slot.ui.last_press_pos = pos;

                let mode = match slot.ui.click_count {
                    1 => SelectionMode::Char,
                    2 => SelectionMode::Word,
                    _ => SelectionMode::Line,
                };
                if mode == SelectionMode::Word
                    && let Some(link) = link_under_press
                {
                    slot.session.start_url_selection(link.start, link.end);
                } else {
                    slot.session.start_selection(to_point(pos), mode);
                }
            }
        }
    } else if !modal_open
        && rendered.response.dragged()
        && let Some(pos) = rendered.response.interact_pointer_pos()
    {
        // Drag extends whichever selection was started on press —
        // editor or alacritty, depending on where the press landed.
        if click_in_editor && let Some(rect) = editor_rect {
            let editor_text = slot
                .session
                .blocks()
                .editor_on_tail()
                .map(|e| e.text().to_string())
                .unwrap_or_default();
            let byte = editor_byte_for_pos(rect, pos, &editor_text, cell_w, row_h);
            // Mode of extension is determined by the click_count
            // at press time (which `slot.ui.click_count` still
            // holds during this drag — it's only reset on the next
            // press):
            //
            // - 1 (single click): char-by-char extension via
            //   `set_cursor_extending` (existing).
            // - 2 (double click): selection grows / shrinks by
            //   WORDS — always cover the originally-clicked word
            //   plus every word the pointer touches.
            // - 3 (triple click): same idea for lines.
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
        } else {
            slot.session.extend_selection(to_point(pos));
        }
    } else if !modal_open
        && primary_down
        && let Some((block_id, block_rect)) = press_in_sealed_block
        && let Some(sel) = slot.session.block_selection().copied()
        && sel.block_id == block_id
        && let Some(pos) = ctx.input(|i| i.pointer.interact_pos())
    {
        // Sealed-block drag. `rendered.response.dragged()` is false
        // here because the press landed on a different widget (a
        // sealed block's response, not the live `Term`'s); we drive
        // the extension off `primary_down` + the active
        // `BlockSelection`'s `block_id` to make sure we're still
        // tracking the block where the press began. Cross-block drag
        // is deferred to a follow-up.
        let rows = slot.session.sealed_snapshot_rows(block_id).unwrap_or(0);
        let mut cursor = sealed_cursor_for_pos(block_rect, pos, cell_w, row_h, rows);
        if let Some(row_len) = slot.session.sealed_row_len(block_id, cursor.row) {
            cursor.col = cursor.col.min(row_len);
        }
        match (slot.ui.click_count, slot.ui.sealed_drag_anchor) {
            (2, Some((anchor_block, a_start, a_end))) if anchor_block == block_id => {
                // Word-mode extension: union of (anchor word) ∪
                // (word under pointer). Anchor stays fixed; head
                // moves to the far end of the union.
                if let Some((c_start, c_end)) = slot.session.sealed_word_range(block_id, cursor) {
                    let start = a_start.min(c_start);
                    let end = a_end.max(c_end);
                    slot.session.update_block_selection_endpoints(start, end);
                }
            }
            (3, Some((anchor_block, a_start, a_end))) if anchor_block == block_id => {
                // Line-mode extension: union of (anchor row) ∪
                // (row under pointer).
                if let Some((c_start, c_end)) = slot.session.sealed_line_range(block_id, cursor) {
                    let start = a_start.min(c_start);
                    let end = a_end.max(c_end);
                    slot.session.update_block_selection_endpoints(start, end);
                }
            }
            _ => {
                // Single-click drag: anchor is where the press
                // landed (preserved on the `BlockSelection`); head
                // tracks the pointer.
                slot.session.update_block_selection_endpoints(sel.anchor, cursor);
            }
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
    let needs_focus = std::mem::take(&mut slot.ui.needs_focus);
    let nothing_focused = ctx.memory(|m| m.focused().is_none());
    if !modal_open && (needs_focus || nothing_focused) {
        rendered.response.request_focus();
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
            rendered.response.id,
            egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            },
        );
    });

    // Record focus state for the next frame's tab styling.
    // `has_focus` is read by `TermicaApp::update` after
    // `tree.ui()` returns; the focused pane's tab title is then
    // rendered bold via the Behavior.
    slot.ui.focused = rendered.response.has_focus();

    // ---- keyboard input, focus-gated ----------------------------
    if !modal_open && rendered.response.has_focus() {
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

        // Editor-mode clipboard ops: copy / cut act on the editor's
        // selection when the editor is active. Outside editor mode
        // the existing alacritty selection path stays in charge.
        // Sealed-block selection (Phase 4F) wins over the live-grid
        // selection when both somehow exist (the setters keep them
        // mutually exclusive, but the ordering here makes the
        // priority explicit).
        if slot.session.editor_is_active() {
            if (copy_pressed || cut_pressed)
                && let Some(text) =
                    slot.session.blocks().editor_on_tail().and_then(|e| e.selected_text())
            {
                let owned = text.to_string();
                ctx.copy_text(owned);
                if cut_pressed && let Some(editor) = slot.session.editor_mut() {
                    editor.delete_selection();
                }
            }
        } else if copy_pressed {
            if let Some(text) = slot.session.block_selection_text() {
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
            if editor_active && apply_event_to_editor(event, slot) {
                continue;
            }
            if let Some(bytes) = input::encode_event(event, modes) {
                let _ = slot.session.write(&bytes);
            }
        }
    }

    // ---- mouse wheel, hover-gated -------------------------------
    //
    // Non-alt-screen scrolling is handled by the outer `ScrollArea`
    // natively (4A-render replaced alacritty's internal scrollback
    // with the block-stack history). The alt-screen case still
    // intercepts wheel events here and forwards them as arrow
    // keystrokes — that's what full-screen TTY programs (vim, less,
    // htop, fzf) expect.
    if !modal_open && rendered.response.hovered() {
        let alt_screen = slot.session.terminal().is_alternate_screen();
        if alt_screen {
            let scroll_delta_y = ctx.input(|i| i.smooth_scroll_delta.y);
            if scroll_delta_y.abs() > 0.0 {
                let lines = (scroll_delta_y / 50.0 * 3.0).round() as i32;
                let modes = slot.session.terminal().modes();
                if let Some(input::WheelOutcome::SendBytes(bytes)) =
                    input::classify_wheel(lines, true, modes)
                {
                    let _ = slot.session.write(&bytes);
                }
            }
        }
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
    fn single_line_editor_with_cwd_yields_two_rows() {
        // 1 chrome row (cwd chip) + 1 editor row = 2 rows.
        assert_eq!(compute_footer_height(true, true, 1, true, 20.0), 40.0);
    }

    #[test]
    fn single_line_editor_without_cwd_yields_one_row() {
        // No chrome row when cwd is unknown.
        assert_eq!(compute_footer_height(true, true, 1, false, 20.0), 20.0);
    }

    #[test]
    fn three_line_editor_with_cwd_yields_four_rows() {
        assert_eq!(compute_footer_height(true, true, 3, true, 20.0), 80.0);
    }

    #[test]
    fn zero_editor_lines_treated_as_one() {
        // An empty editor still needs a caret row.
        assert_eq!(compute_footer_height(true, true, 0, true, 20.0), 40.0);
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
