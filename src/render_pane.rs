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
        Event::Text(s) => {
            if let Some(editor) = slot.session.editor_mut() {
                editor.insert_str(s);
            }
            true
        }
        Event::Key { key, pressed: true, modifiers, .. } => {
            // App-level shortcuts (Cmd+T etc.) and the copy shortcut
            // are already handled earlier in the keyboard loop; here
            // we only act on bare keys (no command/alt modifier).
            // `Shift` is still legal — used for Shift+Enter.
            if modifiers.command || modifiers.alt {
                return false;
            }
            // The keys that own `slot.session` mutably (submit,
            // demote) are handled BEFORE the editor borrow so the
            // borrow checker sees a clean window.
            match key {
                Key::Enter if !modifiers.shift => {
                    // Plain Enter = submit (Phase 4C).
                    let _ = slot.session.submit_editor_command();
                    return true;
                }
                Key::Escape => {
                    // Esc demotes back to `RawTerminal` per spec/05;
                    // the editor's buffer is dropped.
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
                    editor.move_left();
                    true
                }
                Key::ArrowRight => {
                    editor.move_right();
                    true
                }
                // Up / Down: history walk lands in 4J; multi-line
                // cursor row-nav also belongs there since it shares
                // the column-preserving logic. No-op in 4B but
                // *consumed* so the keys don't escape to the PTY.
                Key::ArrowUp | Key::ArrowDown => true,
                Key::Home => {
                    editor.move_home();
                    true
                }
                Key::End => {
                    editor.move_end();
                    true
                }
                // Shift+Enter — bare Enter is already handled above.
                Key::Enter => {
                    editor.insert_newline();
                    true
                }
                // Tab is local completion in 4I; for now we consume
                // it to keep it out of the PTY (no-op).
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

    let scroll_inner = egui::ScrollArea::vertical()
        .id_salt(("pane-blocks", slot.session.pane_id()))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // ---- Sealed blocks ------------------------------------
            //
            // Painted top-to-bottom in flow. Each block is a frozen
            // `Vec<StyledLine>` snapshot of the live `Term`'s content
            // at the moment `CommandFinished` fired. Sealed blocks
            // are not interactive in Phase 4A-render — cross-block
            // selection lands in 4F.
            for block in slot.session.blocks().iter() {
                if let crate::block::Block::Sealed { snapshot, .. } = block {
                    let _ = render::paint_styled_lines(ui, snapshot);
                    // Thin separator gap between blocks; spec/04
                    // §"Visual structure" calls for "non-text (thin
                    // separator + space)".
                    ui.add_space(4.0);
                }
            }

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

            let rendered = render::paint_terminal(
                ui,
                slot.session.terminal(),
                selection.as_ref(),
                highlighted_link,
            );

            // ---- Prompt-block editor overlay (Phase 4C layout fix) ----
            //
            // The editor is conceptually part of the `Prompt` block.
            // Phase 4B painted it below the live `Term` as a flow
            // item; that pushed it off-screen because the live
            // `Term` always allocates its full `screen_lines × cols`
            // rect. Fix: overlay the editor on top of the live
            // `Term`'s painted area, starting at the row immediately
            // below the shell's cursor row. The rows below the
            // cursor are alacritty-uninitialised blanks (the shell
            // hasn't written there yet), so painting the editor on
            // top doesn't hide any meaningful content. The result is
            // the editor visually continues the shell's prompt line.
            //
            // 4D's fixed-footer layout will replace this overlay with
            // a proper viewport-bottom anchor.
            if slot.session.editor_is_active()
                && let Some(editor) = slot.session.blocks().editor_on_tail()
            {
                let font_id = egui::FontId::monospace(render::DEFAULT_FONT_SIZE);
                let cursor_row =
                    slot.session.terminal().cursor_position().map(|(row, _col)| row).unwrap_or(0);
                let origin = egui::Pos2::new(
                    rendered.response.rect.min.x,
                    rendered.response.rect.min.y + (cursor_row + 1) as f32 * row_h,
                );
                let painter = ui.painter_at(rendered.response.rect);
                render::paint_prompt_editor_at(&painter, editor, origin, cell_w, row_h, &font_id);
            }

            (rendered, links_in_view, highlighted_link.cloned())
        });
    let (rendered, links_in_view, highlighted_link) = scroll_inner.inner;
    let highlighted_link = highlighted_link.as_ref();
    if highlighted_link.is_some() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }

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
    let (primary_pressed, press_origin, now) =
        ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.press_origin(), i.time));

    if !modal_open
        && primary_pressed
        && let Some(pos) = press_origin
        && rendered.response.rect.contains(pos)
    {
        rendered.response.request_focus();

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
    } else if !modal_open
        && rendered.response.dragged()
        && let Some(pos) = rendered.response.interact_pointer_pos()
    {
        slot.session.extend_selection(to_point(pos));
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
        if copy_pressed && let Some(text) = slot.session.selection_text() {
            ctx.copy_text(text);
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
