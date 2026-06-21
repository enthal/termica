//! App-level keyboard shortcuts. Pure key+modifier → [`PaneAction`]
//! mapping, lifted out of `render_pane` so it's unit-testable for
//! both platforms regardless of where the test runs.

use eframe::egui;

use crate::pane_slot::PaneAction;

/// Map a `(key, modifiers, is_macos)` triple to a [`PaneAction`]
/// shortcut, or `None` if it's not one we recognise.
///
/// # Platform conventions
///
/// - **macOS** uses `Cmd` (`modifiers.mac_cmd`) for app-level
///   shortcuts because `Cmd` is reserved for the app — `Ctrl` in a
///   terminal goes to the shell as control characters. The
///   "letter" shortcuts don't carry Shift; the bracket-pair tab-
///   nav shortcuts do (it's part of the physical chord).
/// - **Linux / Windows** uses `Ctrl+Shift` because plain `Ctrl+X`
///   in a terminal almost always conflicts with a shell binding
///   (`Ctrl+T` is `transpose-chars`, `Ctrl+W` is `backward-kill-word`,
///   `Ctrl+Q` is XON flow control, etc.). The `Shift` suffix is the
///   gnome-terminal / konsole / xterm convention to disambiguate
///   "app shortcut" from "shell binding".
///
/// `is_macos` is passed in (not `cfg!`-checked inline) so the
/// matcher is testable for both platforms regardless of where the
/// test runs.
///
/// On a US keyboard, `Shift+]` produces `}` and egui reports the
/// key as `Key::CloseCurlyBracket` rather than `CloseBracket`. We
/// accept either variant for the bracket-pair shortcuts so the
/// physical `Shift+]` chord works regardless of how the OS
/// transliterates it.
pub fn match_pane_shortcut(
    key: egui::Key,
    modifiers: egui::Modifiers,
    is_macos: bool,
) -> Option<PaneAction> {
    // Scrollback navigation — same on both platforms: `Ctrl` + Home /
    // End / PageUp / PageDown moves the pane's scrollback viewport
    // (Home/End jump to the ends; PageUp/PageDown page). These keys are
    // not terminal control characters and the input encoder drops them
    // when modified, so claiming them here steals nothing from a
    // running program. Bare Home/End/PageUp/PageDown are left for the
    // editor caret (or the PTY in `RawTerminal`).
    if modifiers.ctrl && !modifiers.alt && !modifiers.shift && !modifiers.mac_cmd {
        match key {
            egui::Key::Home => return Some(PaneAction::ScrollToTop),
            egui::Key::End => return Some(PaneAction::ScrollToBottom),
            egui::Key::PageUp => return Some(PaneAction::ScrollPageUp),
            egui::Key::PageDown => return Some(PaneAction::ScrollPageDown),
            _ => {}
        }
    }
    if is_macos {
        // macOS Cmd+Option+Up/Down is the scrollback-jump chord —
        // distinct from the Cmd-only family below because Cmd+Up /
        // Cmd+Down are editor caret-to-doc-start / -end. Spec/04
        // §"Editing keystrokes" reserves Cmd+↑/↓ for the editor; the
        // scrollback jump adds Option to disambiguate.
        if modifiers.mac_cmd
            && modifiers.alt
            && !modifiers.ctrl
            && !modifiers.shift
            && matches!(key, egui::Key::ArrowUp)
        {
            return Some(PaneAction::ScrollToTop);
        }
        if modifiers.mac_cmd
            && modifiers.alt
            && !modifiers.ctrl
            && !modifiers.shift
            && matches!(key, egui::Key::ArrowDown)
        {
            return Some(PaneAction::ScrollToBottom);
        }
        // Mac: Cmd held, no Ctrl, no Alt. Shift varies per action.
        if !modifiers.mac_cmd || modifiers.ctrl || modifiers.alt {
            return None;
        }
        let shift = modifiers.shift;
        match (key, shift) {
            (egui::Key::T, false) => Some(PaneAction::NewTab),
            (egui::Key::W, false) => Some(PaneAction::CloseTab),
            (egui::Key::Q, false) => Some(PaneAction::Quit),
            (egui::Key::K, false) => Some(PaneAction::ClearScrollback),
            (egui::Key::F, false) => Some(PaneAction::OpenFind),
            (egui::Key::CloseBracket | egui::Key::CloseCurlyBracket, true) => {
                Some(PaneAction::NextTab)
            }
            (egui::Key::OpenBracket | egui::Key::OpenCurlyBracket, true) => {
                Some(PaneAction::PrevTab)
            }
            _ => None,
        }
    } else {
        // Linux / Windows Ctrl+Alt+Up/Down: scrollback-jump. The
        // bracket-pair tab-nav family uses Ctrl+Shift, so the Alt
        // modifier disambiguates this chord from the editor's
        // Ctrl+Home / Ctrl+End document moves.
        if modifiers.ctrl
            && modifiers.alt
            && !modifiers.shift
            && !modifiers.mac_cmd
            && matches!(key, egui::Key::ArrowUp)
        {
            return Some(PaneAction::ScrollToTop);
        }
        if modifiers.ctrl
            && modifiers.alt
            && !modifiers.shift
            && !modifiers.mac_cmd
            && matches!(key, egui::Key::ArrowDown)
        {
            return Some(PaneAction::ScrollToBottom);
        }
        // Linux / Windows: Ctrl+Shift held, no Cmd, no Alt. Shift is
        // required for every action; with the bracket pair it's
        // also the modifier that produces the curly-bracket key
        // code from the same physical key.
        if !modifiers.ctrl || !modifiers.shift || modifiers.alt || modifiers.mac_cmd {
            return None;
        }
        match key {
            egui::Key::T => Some(PaneAction::NewTab),
            egui::Key::W => Some(PaneAction::CloseTab),
            egui::Key::Q => Some(PaneAction::Quit),
            egui::Key::K => Some(PaneAction::ClearScrollback),
            egui::Key::F => Some(PaneAction::OpenFind),
            egui::Key::CloseBracket | egui::Key::CloseCurlyBracket => Some(PaneAction::NextTab),
            egui::Key::OpenBracket | egui::Key::OpenCurlyBracket => Some(PaneAction::PrevTab),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // macOS conventions: Cmd held; Shift only on bracket-nav.
    fn mac_cmd_only() -> egui::Modifiers {
        egui::Modifiers { mac_cmd: true, command: true, ..egui::Modifiers::default() }
    }
    fn mac_cmd_shift() -> egui::Modifiers {
        egui::Modifiers { mac_cmd: true, command: true, shift: true, ..egui::Modifiers::default() }
    }

    // Linux/Windows conventions: Ctrl+Shift held for all app shortcuts.
    fn linux_ctrl_shift() -> egui::Modifiers {
        egui::Modifiers { ctrl: true, shift: true, command: true, ..egui::Modifiers::default() }
    }

    #[test]
    fn macos_cmd_t_maps_to_new_tab() {
        assert_eq!(
            match_pane_shortcut(egui::Key::T, mac_cmd_only(), true),
            Some(PaneAction::NewTab)
        );
    }

    #[test]
    fn macos_cmd_w_maps_to_close_tab() {
        assert_eq!(
            match_pane_shortcut(egui::Key::W, mac_cmd_only(), true),
            Some(PaneAction::CloseTab)
        );
    }

    #[test]
    fn macos_cmd_q_maps_to_quit() {
        assert_eq!(match_pane_shortcut(egui::Key::Q, mac_cmd_only(), true), Some(PaneAction::Quit));
    }

    #[test]
    fn macos_cmd_k_maps_to_clear_scrollback() {
        assert_eq!(
            match_pane_shortcut(egui::Key::K, mac_cmd_only(), true),
            Some(PaneAction::ClearScrollback)
        );
    }

    #[test]
    fn macos_cmd_f_maps_to_open_find() {
        assert_eq!(
            match_pane_shortcut(egui::Key::F, mac_cmd_only(), true),
            Some(PaneAction::OpenFind)
        );
    }

    #[test]
    fn linux_ctrl_shift_f_maps_to_open_find() {
        assert_eq!(
            match_pane_shortcut(egui::Key::F, linux_ctrl_shift(), false),
            Some(PaneAction::OpenFind)
        );
    }

    #[test]
    fn linux_plain_ctrl_f_is_not_a_shortcut() {
        // Plain Ctrl+F is readline `forward-char`; it must pass through
        // to the shell, not open the find overlay. Only Ctrl+Shift+F is
        // ours on Linux.
        let ctrl_only = egui::Modifiers { ctrl: true, command: true, ..egui::Modifiers::default() };
        assert_eq!(match_pane_shortcut(egui::Key::F, ctrl_only, false), None);
    }

    #[test]
    fn macos_cmd_shift_k_does_not_map() {
        // Shift on top of Cmd+K is a different chord; we reserve
        // it for future use. The matcher must not accept it as
        // ClearScrollback.
        assert_eq!(match_pane_shortcut(egui::Key::K, mac_cmd_shift(), true), None);
    }

    #[test]
    fn macos_cmd_shift_close_bracket_accepts_either_bracket_variant() {
        // With Shift held on a US keyboard, `]` becomes `}` and
        // egui reports `CloseCurlyBracket`. Both variants should
        // map to NextTab so the physical chord works regardless
        // of layout transliteration.
        assert_eq!(
            match_pane_shortcut(egui::Key::CloseBracket, mac_cmd_shift(), true),
            Some(PaneAction::NextTab)
        );
        assert_eq!(
            match_pane_shortcut(egui::Key::CloseCurlyBracket, mac_cmd_shift(), true),
            Some(PaneAction::NextTab)
        );
    }

    #[test]
    fn macos_cmd_shift_open_bracket_accepts_either_variant() {
        assert_eq!(
            match_pane_shortcut(egui::Key::OpenBracket, mac_cmd_shift(), true),
            Some(PaneAction::PrevTab)
        );
        assert_eq!(
            match_pane_shortcut(egui::Key::OpenCurlyBracket, mac_cmd_shift(), true),
            Some(PaneAction::PrevTab)
        );
    }

    #[test]
    fn macos_cmd_only_brackets_do_not_navigate_tabs() {
        // Cmd+] without Shift isn't ours. Must be None.
        assert_eq!(match_pane_shortcut(egui::Key::CloseBracket, mac_cmd_only(), true), None);
    }

    #[test]
    fn macos_plain_t_is_not_a_shortcut() {
        assert_eq!(match_pane_shortcut(egui::Key::T, egui::Modifiers::default(), true), None);
    }

    #[test]
    fn macos_cmd_t_with_extra_modifier_is_not_recognised() {
        let cmd_alt = egui::Modifiers {
            mac_cmd: true,
            command: true,
            alt: true,
            ..egui::Modifiers::default()
        };
        assert_eq!(match_pane_shortcut(egui::Key::T, cmd_alt, true), None);
    }

    // --- Linux / Windows shortcuts ---------------------------------

    #[test]
    fn linux_ctrl_shift_t_maps_to_new_tab() {
        assert_eq!(
            match_pane_shortcut(egui::Key::T, linux_ctrl_shift(), false),
            Some(PaneAction::NewTab)
        );
    }

    #[test]
    fn linux_ctrl_shift_w_maps_to_close_tab() {
        assert_eq!(
            match_pane_shortcut(egui::Key::W, linux_ctrl_shift(), false),
            Some(PaneAction::CloseTab)
        );
    }

    #[test]
    fn linux_ctrl_shift_q_maps_to_quit() {
        assert_eq!(
            match_pane_shortcut(egui::Key::Q, linux_ctrl_shift(), false),
            Some(PaneAction::Quit)
        );
    }

    #[test]
    fn linux_ctrl_shift_k_maps_to_clear_scrollback() {
        assert_eq!(
            match_pane_shortcut(egui::Key::K, linux_ctrl_shift(), false),
            Some(PaneAction::ClearScrollback)
        );
    }

    #[test]
    fn linux_ctrl_shift_brackets_navigate_tabs() {
        assert_eq!(
            match_pane_shortcut(egui::Key::CloseCurlyBracket, linux_ctrl_shift(), false),
            Some(PaneAction::NextTab)
        );
        assert_eq!(
            match_pane_shortcut(egui::Key::OpenCurlyBracket, linux_ctrl_shift(), false),
            Some(PaneAction::PrevTab)
        );
    }

    #[test]
    fn linux_plain_ctrl_t_is_not_a_shortcut() {
        // Critical: plain Ctrl+T must NOT spawn a tab on Linux.
        // It's a legitimate readline binding (`transpose-chars`)
        // and must pass through to the shell.
        let ctrl_only = egui::Modifiers { ctrl: true, command: true, ..egui::Modifiers::default() };
        assert_eq!(match_pane_shortcut(egui::Key::T, ctrl_only, false), None);
    }

    #[test]
    fn linux_plain_ctrl_q_is_not_a_shortcut() {
        // Same reasoning — plain Ctrl+Q is shell flow control (XON).
        let ctrl_only = egui::Modifiers { ctrl: true, command: true, ..egui::Modifiers::default() };
        assert_eq!(match_pane_shortcut(egui::Key::Q, ctrl_only, false), None);
    }

    #[test]
    fn linux_cmd_t_is_not_recognised() {
        // Cross-platform safety: on Linux, the Mac-only `mac_cmd`
        // flag (which winit doesn't emit) shouldn't accidentally
        // activate an app shortcut.
        let mac_cmd =
            egui::Modifiers { mac_cmd: true, command: true, ..egui::Modifiers::default() };
        assert_eq!(match_pane_shortcut(egui::Key::T, mac_cmd, false), None);
    }

    #[test]
    fn macos_ctrl_shift_t_is_not_recognised() {
        // Cross-platform safety: on macOS, the Linux combo should
        // NOT also trigger (avoid double-binding).
        let ctrl_shift = egui::Modifiers {
            ctrl: true,
            shift: true,
            command: true,
            ..egui::Modifiers::default()
        };
        assert_eq!(match_pane_shortcut(egui::Key::T, ctrl_shift, true), None);
    }

    // --- Scrollback jump (Cmd+Option / Ctrl+Alt arrows) -----------

    fn mac_cmd_alt() -> egui::Modifiers {
        egui::Modifiers { mac_cmd: true, command: true, alt: true, ..egui::Modifiers::default() }
    }

    fn linux_ctrl_alt() -> egui::Modifiers {
        egui::Modifiers { ctrl: true, alt: true, command: true, ..egui::Modifiers::default() }
    }

    #[test]
    fn macos_cmd_option_up_maps_to_scroll_to_top() {
        assert_eq!(
            match_pane_shortcut(egui::Key::ArrowUp, mac_cmd_alt(), true),
            Some(PaneAction::ScrollToTop)
        );
    }

    #[test]
    fn macos_cmd_option_down_maps_to_scroll_to_bottom() {
        assert_eq!(
            match_pane_shortcut(egui::Key::ArrowDown, mac_cmd_alt(), true),
            Some(PaneAction::ScrollToBottom)
        );
    }

    #[test]
    fn macos_cmd_up_alone_does_not_map_to_scrollback_jump() {
        // Cmd+Up alone is editor caret-to-doc-start (per spec/04).
        // The Option modifier is what claims it for scrollback.
        let cmd_only = mac_cmd_only();
        assert_eq!(match_pane_shortcut(egui::Key::ArrowUp, cmd_only, true), None);
    }

    #[test]
    fn linux_ctrl_alt_up_maps_to_scroll_to_top() {
        assert_eq!(
            match_pane_shortcut(egui::Key::ArrowUp, linux_ctrl_alt(), false),
            Some(PaneAction::ScrollToTop)
        );
    }

    #[test]
    fn linux_ctrl_alt_down_maps_to_scroll_to_bottom() {
        assert_eq!(
            match_pane_shortcut(egui::Key::ArrowDown, linux_ctrl_alt(), false),
            Some(PaneAction::ScrollToBottom)
        );
    }

    // Ctrl (no Alt/Shift/Cmd). On Linux egui also reports `command`.
    fn ctrl_only(is_macos: bool) -> egui::Modifiers {
        egui::Modifiers { ctrl: true, command: !is_macos, ..egui::Modifiers::default() }
    }

    #[test]
    fn ctrl_home_end_page_keys_map_to_scrollback_both_platforms() {
        // Ctrl + Home/End/PageUp/PageDown navigate the scrollback
        // viewport on both platforms (Home/End jump to the ends,
        // PageUp/PageDown page). Bare versions stay with the editor
        // caret / PTY, so they must carry Ctrl here.
        for is_macos in [true, false] {
            let m = ctrl_only(is_macos);
            assert_eq!(
                match_pane_shortcut(egui::Key::Home, m, is_macos),
                Some(PaneAction::ScrollToTop)
            );
            assert_eq!(
                match_pane_shortcut(egui::Key::End, m, is_macos),
                Some(PaneAction::ScrollToBottom)
            );
            assert_eq!(
                match_pane_shortcut(egui::Key::PageUp, m, is_macos),
                Some(PaneAction::ScrollPageUp)
            );
            assert_eq!(
                match_pane_shortcut(egui::Key::PageDown, m, is_macos),
                Some(PaneAction::ScrollPageDown)
            );
        }
    }

    #[test]
    fn bare_page_keys_are_not_pane_shortcuts() {
        // Without Ctrl, PageUp/PageDown/Home/End belong to the editor
        // caret (or the PTY in RawTerminal) — the app matcher must not
        // claim them.
        let none = egui::Modifiers::default();
        for key in [egui::Key::PageUp, egui::Key::PageDown, egui::Key::Home, egui::Key::End] {
            assert_eq!(match_pane_shortcut(key, none, true), None);
            assert_eq!(match_pane_shortcut(key, none, false), None);
        }
    }

    #[test]
    fn macos_cmd_option_left_right_are_not_scrollback() {
        // Only ArrowUp/ArrowDown participate. Left/Right with the
        // same modifiers should NOT trigger.
        assert_eq!(match_pane_shortcut(egui::Key::ArrowLeft, mac_cmd_alt(), true), None);
        assert_eq!(match_pane_shortcut(egui::Key::ArrowRight, mac_cmd_alt(), true), None);
    }
}
