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
    if is_macos {
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
            (egui::Key::CloseBracket | egui::Key::CloseCurlyBracket, true) => {
                Some(PaneAction::NextTab)
            }
            (egui::Key::OpenBracket | egui::Key::OpenCurlyBracket, true) => {
                Some(PaneAction::PrevTab)
            }
            _ => None,
        }
    } else {
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
}
