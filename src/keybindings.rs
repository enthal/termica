//! `Cmd+/` keyboard-shortcuts cheat-sheet.
//!
//! A read-only popup listing every app + editor shortcut, rendered
//! platform-local (the macOS column uses `Cmd` / `Option`, the Linux
//! column `Ctrl` / `Alt`). Modifier key-caps are drawn — the Command
//! key gets the looped-square ⌘ glyph from [`crate::icons`] rather than
//! a Unicode character (CLAUDE.md "no Unicode symbols for icons").
//!
//! The shortcut list ([`shortcut_groups`]) is pure data so it's
//! unit-testable and a single source the popup renders from.

#![forbid(unsafe_code)]

use eframe::egui;

/// One key-cap in a shortcut chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    /// macOS Command — drawn as the ⌘ looped square.
    Cmd,
    Ctrl,
    Shift,
    /// macOS Option / Linux Alt — label set per platform by the builder.
    Alt(&'static str),
    /// A literal key label ("T", "Enter", "Up", …).
    Key(&'static str),
}

/// One shortcut: the chord plus what it does.
#[derive(Debug, Clone)]
pub struct Shortcut {
    pub keys: Vec<Cap>,
    pub desc: &'static str,
}

/// A titled group of shortcuts.
#[derive(Debug, Clone)]
pub struct ShortcutGroup {
    pub title: &'static str,
    pub items: Vec<Shortcut>,
}

fn sc(keys: Vec<Cap>, desc: &'static str) -> Shortcut {
    Shortcut { keys, desc }
}

/// The full, platform-local shortcut list. `is_macos` selects the
/// primary modifier (`Cmd` vs `Ctrl+Shift` for app shortcuts) and the
/// Option/Alt label.
pub fn shortcut_groups(is_macos: bool) -> Vec<ShortcutGroup> {
    // App-level "command" chord: Cmd on macOS, Ctrl+Shift on Linux.
    let app = |k: &'static str| -> Vec<Cap> {
        if is_macos {
            vec![Cap::Cmd, Cap::Key(k)]
        } else {
            vec![Cap::Ctrl, Cap::Shift, Cap::Key(k)]
        }
    };
    // Editor "command" chord: Cmd on macOS, Ctrl on Linux.
    let edit = |k: &'static str| -> Vec<Cap> {
        if is_macos { vec![Cap::Cmd, Cap::Key(k)] } else { vec![Cap::Ctrl, Cap::Key(k)] }
    };
    let opt = if is_macos { Cap::Alt("Option") } else { Cap::Alt("Alt") };

    vec![
        ShortcutGroup {
            title: "Window & tabs",
            items: vec![
                sc(app("T"), "New tab"),
                sc(app("W"), "Close tab"),
                sc(
                    if is_macos {
                        vec![Cap::Cmd, Cap::Shift, Cap::Key("]")]
                    } else {
                        vec![Cap::Ctrl, Cap::Shift, Cap::Key("]")]
                    },
                    "Next tab",
                ),
                sc(
                    if is_macos {
                        vec![Cap::Cmd, Cap::Shift, Cap::Key("[")]
                    } else {
                        vec![Cap::Ctrl, Cap::Shift, Cap::Key("[")]
                    },
                    "Previous tab",
                ),
                sc(app("K"), "Clear scrollback"),
                sc(app("Q"), "Quit"),
            ],
        },
        ShortcutGroup {
            title: "Scroll & select",
            items: vec![
                sc(
                    if is_macos {
                        vec![Cap::Cmd, opt, Cap::Key("Up")]
                    } else {
                        vec![Cap::Ctrl, opt, Cap::Key("Up")]
                    },
                    "Scroll to top",
                ),
                sc(
                    if is_macos {
                        vec![Cap::Cmd, opt, Cap::Key("Down")]
                    } else {
                        vec![Cap::Ctrl, opt, Cap::Key("Down")]
                    },
                    "Scroll to bottom",
                ),
                sc(
                    if is_macos {
                        vec![Cap::Cmd, Cap::Shift, Cap::Key("C")]
                    } else {
                        vec![Cap::Ctrl, Cap::Shift, Cap::Key("C")]
                    },
                    "Copy selection",
                ),
            ],
        },
        ShortcutGroup {
            title: "Find & history",
            items: vec![
                sc(app("F"), "Find in pane"),
                sc(
                    if is_macos {
                        vec![Cap::Cmd, Cap::Key("R")]
                    } else {
                        vec![Cap::Ctrl, Cap::Key("R")]
                    },
                    "History search",
                ),
                sc(vec![Cap::Key("Up")], "Recall previous command"),
                sc(vec![Cap::Key("Down")], "Recall next command"),
                sc(vec![Cap::Key("Tab")], "Completion popup"),
                sc(edit("/"), "This shortcuts list"),
            ],
        },
        ShortcutGroup {
            title: "Prompt editor",
            items: vec![
                sc(vec![Cap::Key("Enter")], "Run command"),
                sc(vec![Cap::Shift, Cap::Key("Enter")], "Insert newline"),
                sc(vec![Cap::Key("Esc")], "Leave editor"),
                sc(vec![Cap::Ctrl, Cap::Key("A")], "Caret to line start"),
                sc(vec![Cap::Ctrl, Cap::Key("E")], "Caret to line end"),
                sc(vec![Cap::Ctrl, Cap::Key("T")], "Transpose characters"),
                sc(vec![opt, Cap::Key("Left")], "Caret one word left"),
                sc(vec![opt, Cap::Key("Right")], "Caret one word right"),
                sc(edit("Z"), "Undo"),
                sc(
                    if is_macos {
                        vec![Cap::Cmd, Cap::Shift, Cap::Key("Z")]
                    } else {
                        vec![Cap::Ctrl, Cap::Shift, Cap::Key("Z")]
                    },
                    "Redo",
                ),
            ],
        },
    ]
}

/// Background of a drawn key-cap.
const CAP_BG: egui::Color32 = egui::Color32::from_rgb(0x2c, 0x2c, 0x2c);

/// Paint one key-cap (a small rounded rect with the modifier glyph or
/// the key label). Advances layout by allocating the cap's space.
fn paint_cap(ui: &mut egui::Ui, cap: Cap) {
    let h = ui.spacing().interact_size.y;
    let label = match cap {
        Cap::Cmd => None,
        Cap::Ctrl => Some("Ctrl".to_string()),
        Cap::Shift => Some("Shift".to_string()),
        Cap::Alt(s) => Some(s.to_string()),
        Cap::Key(s) => Some(s.to_string()),
    };
    let text_w = label.as_ref().map(|s| 7.0 * s.chars().count() as f32).unwrap_or(0.0);
    let w = (text_w + 12.0).max(h);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        4.0,
        CAP_BG,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
    let fg = ui.visuals().strong_text_color();
    match label {
        None => crate::icons::paint_command_symbol(painter, rect, fg),
        Some(s) => {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                s,
                egui::FontId::proportional(12.0),
                fg,
            );
        }
    }
}

/// Plain-text label of a cap, for search matching ("Cmd" matches the
/// ⌘ key even though it's drawn as a glyph).
fn cap_text(cap: &Cap) -> &str {
    match cap {
        Cap::Cmd => "Cmd",
        Cap::Ctrl => "Ctrl",
        Cap::Shift => "Shift",
        Cap::Alt(s) => s,
        Cap::Key(s) => s,
    }
}

/// Does `shortcut` match the (already-lowercased) `query`? Matches on
/// the description or any key-cap label, substring + case-insensitive.
pub fn shortcut_matches(shortcut: &Shortcut, query_lower: &str) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    shortcut.desc.to_lowercase().contains(query_lower)
        || shortcut.keys.iter().any(|c| cap_text(c).to_lowercase().contains(query_lower))
}

/// Paint the cheat-sheet over `pane_rect`. `query` is the live search
/// filter (owned by the caller so it persists). Returns `true` when the
/// user asked to close it (Esc, the ✕, or a click on the dimmed
/// backdrop).
pub fn paint(
    ui: &mut egui::Ui,
    pane_id: u64,
    pane_rect: egui::Rect,
    is_macos: bool,
    query: &mut String,
) -> bool {
    let mut close = false;
    if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
        close = true;
    }
    // Dimmed backdrop; a click on it closes.
    let backdrop = egui::Area::new(ui.id().with(("keys-backdrop", pane_id)))
        .order(egui::Order::Foreground)
        .fixed_pos(pane_rect.left_top())
        .show(ui.ctx(), |ui| {
            let (rect, resp) = ui.allocate_exact_size(pane_rect.size(), egui::Sense::click());
            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_black_alpha(120));
            resp
        });
    if backdrop.inner.clicked() {
        close = true;
    }

    let panel_w = (pane_rect.width() * 0.9).min(560.0);
    let query_lower = query.to_lowercase();

    egui::Area::new(ui.id().with(("keys-panel", pane_id)))
        .order(egui::Order::Tooltip)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(panel_w);
                ui.horizontal(|ui| {
                    ui.heading("Keyboard shortcuts");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::icons::close_button(ui, "Close (Esc)").clicked() {
                            close = true;
                        }
                        // Search field fills the gap between heading and ✕.
                        let resp = ui.add(
                            egui::TextEdit::singleline(query)
                                .desired_width(ui.available_width())
                                .id_salt(("keys-search", pane_id))
                                .hint_text("Filter…"),
                        );
                        if !resp.has_focus() {
                            resp.request_focus();
                        }
                    });
                });
                ui.separator();
                // Full-width scroll area (auto_shrink false on x) so the
                // scrollbar sits at the right edge of the whole popup.
                egui::ScrollArea::vertical()
                    .max_height(pane_rect.height() * 0.7)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for group in shortcut_groups(is_macos) {
                            let items: Vec<Shortcut> = group
                                .items
                                .into_iter()
                                .filter(|s| shortcut_matches(s, &query_lower))
                                .collect();
                            if items.is_empty() {
                                continue;
                            }
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new(group.title).strong().size(13.0));
                            for item in items {
                                ui.horizontal(|ui| {
                                    // Description on the LEFT; key-caps
                                    // right-justified. In a right-to-left
                                    // layout the first-added widget is
                                    // rightmost, so iterate the caps in
                                    // reverse to keep their visual order.
                                    ui.label(item.desc);
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.spacing_mut().item_spacing.x = 4.0;
                                            for cap in item.keys.iter().rev() {
                                                paint_cap(ui, *cap);
                                            }
                                        },
                                    );
                                });
                            }
                        }
                    });
            });
        });

    close
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_cmd_linux_uses_ctrl_shift_for_new_tab() {
        let mac = &shortcut_groups(true)[0].items[0];
        assert_eq!(mac.desc, "New tab");
        assert_eq!(mac.keys, vec![Cap::Cmd, Cap::Key("T")]);

        let linux = &shortcut_groups(false)[0].items[0];
        assert_eq!(linux.keys, vec![Cap::Ctrl, Cap::Shift, Cap::Key("T")]);
    }

    #[test]
    fn option_label_is_platform_specific() {
        // The word-left editor chord uses Option on macOS, Alt on Linux.
        let find_groups = |mac: bool| {
            shortcut_groups(mac)
                .into_iter()
                .flat_map(|g| g.items)
                .find(|s| s.desc == "Caret one word left")
                .unwrap()
        };
        assert!(find_groups(true).keys.contains(&Cap::Alt("Option")));
        assert!(find_groups(false).keys.contains(&Cap::Alt("Alt")));
    }

    #[test]
    fn shortcut_matches_desc_keys_and_modifier_names() {
        let s = sc(vec![Cap::Cmd, Cap::Key("T")], "New tab");
        assert!(shortcut_matches(&s, "")); // empty → everything
        assert!(shortcut_matches(&s, "tab")); // desc substring
        assert!(shortcut_matches(&s, "cmd")); // ⌘ matches "cmd" by name
        assert!(shortcut_matches(&s, "t")); // key label
        assert!(!shortcut_matches(&s, "quit"));
    }

    #[test]
    fn every_shortcut_has_keys_and_desc() {
        for mac in [true, false] {
            for group in shortcut_groups(mac) {
                assert!(!group.title.is_empty());
                for item in group.items {
                    assert!(!item.keys.is_empty(), "{} has no keys", item.desc);
                    assert!(!item.desc.is_empty());
                }
            }
        }
    }
}
