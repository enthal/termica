//! Keyboard input encoder.
//!
//! Translates egui input events into the byte sequences a real
//! terminal expects to receive from a keyboard. The shape mirrors
//! the table in [`spec/02-terminal-engine.md`](../spec/02-terminal-engine.md#input-encoding).
//!
//! Phase 1E-c scope:
//! - Printable text (`Event::Text`) → UTF-8 bytes.
//! - Pasted text (`Event::Paste`) → raw bytes (no bracketed paste yet).
//! - Enter, Backspace, Tab, Escape.
//! - Arrow keys, Home / End, PageUp / PageDown, Delete (normal mode).
//! - `Ctrl + <letter>` → control bytes 0x01..0x1A.
//! - Function keys F1–F12.
//!
//! Phase 1E-c-extra: pass DECCKM (application cursor keys mode)
//! through. Programs like `less`, `vim`, `htop` enable DECCKM via
//! the terminfo `smkx` sequence on entry, and their keymaps are
//! bound to the SS3 form of the arrow / Home / End keys. The
//! encoder picks between CSI and SS3 based on the [`TerminalModes`]
//! snapshot the caller passes.
//!
//! Out of scope here (later sub-PRs / Phase 3):
//! - Bracketed paste wrapping (needs hookup to a similar
//!   `bracketed_paste` flag on [`TerminalModes`]).
//! - Mouse reporting encoding.
//! - macOS-specific Cmd-handling beyond what egui already gives us.
//! - IME composition.
//!
//! Once the [`crate::pty::PtySession`] receives these bytes, the
//! terminal-side line discipline + the shell's line editor (or
//! Termica's editor in Phase 4) take over.

#![forbid(unsafe_code)]

use eframe::egui::{self, Key, Modifiers};

use crate::terminal::TerminalModes;

/// Encode a single egui input event into the bytes that should be
/// written to the PTY. Returns `None` for events that aren't part of
/// keyboard input (mouse, focus, scroll, etc.) and for key *releases*
/// (we only encode key-down events).
///
/// `modes` carries the current VT mode flags (e.g. DECCKM) — programs
/// like `less` / `vim` / `htop` switch arrow-key encoding via these,
/// so the encoder needs the current snapshot once per frame.
///
/// Callers iterate `ctx.input(|i| i.events.clone())` and pipe each
/// result through this function into [`crate::pane::PaneSession::write`].
pub fn encode_event(event: &egui::Event, modes: TerminalModes) -> Option<Vec<u8>> {
    match event {
        egui::Event::Text(s) => {
            // egui delivers Enter as a `Key::Enter` event, NOT as
            // `Text("\n")`. So `Text` is always genuine text input —
            // we can pass it through verbatim.
            if s.is_empty() { None } else { Some(s.as_bytes().to_vec()) }
        }
        egui::Event::Paste(s) => {
            if s.is_empty() {
                None
            } else if modes.bracketed_paste {
                // Bracketed paste: wrap the payload in `\e[200~` and
                // `\e[201~`. The shell uses these markers to skip
                // completion / history expansion of the paste — a
                // raw multi-line paste would otherwise run lines
                // halfway, leaving the shell stuck mid-edit.
                let mut out = Vec::with_capacity(s.len() + 12);
                out.extend_from_slice(b"\x1b[200~");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\x1b[201~");
                Some(out)
            } else {
                Some(s.as_bytes().to_vec())
            }
        }
        egui::Event::Key { key, pressed: true, modifiers, .. } => {
            encode_key(*key, *modifiers, modes)
        }
        _ => None,
    }
}

/// Encode a single key-down event. Public so the snapshot/unit test
/// can call it without faking a full `egui::Event`.
pub fn encode_key(key: Key, modifiers: Modifiers, modes: TerminalModes) -> Option<Vec<u8>> {
    // Ctrl + letter takes precedence over per-key encodings: even
    // though Ctrl+M would otherwise be Return, the standard terminal
    // contract says Ctrl+letter → C0 control byte.
    if modifiers.ctrl
        && !modifiers.alt
        && !modifiers.shift
        && let Some(byte) = ctrl_letter_byte(key)
    {
        return Some(vec![byte]);
    }

    // Arrow keys, Home, End: pick CSI vs SS3 form based on DECCKM.
    // The `xterm-256color` terminfo entry `kcuu1 = \EOA` etc. — so
    // any program using terminfo (less, vim, htop, …) sets DECCKM via
    // `smkx` on entry and then expects the SS3 form. We must match.
    let cursor_key = if modes.application_cursor {
        match key {
            Key::ArrowUp => Some(b"\x1bOA".as_slice()),
            Key::ArrowDown => Some(b"\x1bOB".as_slice()),
            Key::ArrowRight => Some(b"\x1bOC".as_slice()),
            Key::ArrowLeft => Some(b"\x1bOD".as_slice()),
            Key::Home => Some(b"\x1bOH".as_slice()),
            Key::End => Some(b"\x1bOF".as_slice()),
            _ => None,
        }
    } else {
        None
    };
    if let Some(b) = cursor_key {
        return Some(b.to_vec());
    }

    let bytes: &[u8] = match key {
        Key::Enter => b"\r",
        Key::Backspace => b"\x7f",
        Key::Tab => b"\t",
        Key::Escape => b"\x1b",

        // Arrow keys — normal (cursor-key) mode fallback.
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowRight => b"\x1b[C",
        Key::ArrowLeft => b"\x1b[D",

        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
        Key::Delete => b"\x1b[3~",
        Key::Insert => b"\x1b[2~",

        // Function keys — F1..F4 use the "VT220" `\x1bOP/Q/R/S`
        // form; F5..F12 use the `\x1b[<num>~` form, again per VT220.
        Key::F1 => b"\x1bOP",
        Key::F2 => b"\x1bOQ",
        Key::F3 => b"\x1bOR",
        Key::F4 => b"\x1bOS",
        Key::F5 => b"\x1b[15~",
        Key::F6 => b"\x1b[17~",
        Key::F7 => b"\x1b[18~",
        Key::F8 => b"\x1b[19~",
        Key::F9 => b"\x1b[20~",
        Key::F10 => b"\x1b[21~",
        Key::F11 => b"\x1b[23~",
        Key::F12 => b"\x1b[24~",

        _ => return None,
    };
    Some(bytes.to_vec())
}

/// Map a key (A..Z) to its C0 control byte for `Ctrl + key`.
/// Returns `None` for keys that don't form a Ctrl combo this layer
/// recognises.
fn ctrl_letter_byte(key: Key) -> Option<u8> {
    Some(match key {
        Key::A => 0x01,
        Key::B => 0x02,
        Key::C => 0x03, // ETX — SIGINT to the foreground process
        Key::D => 0x04, // EOT — EOF
        Key::E => 0x05,
        Key::F => 0x06,
        Key::G => 0x07,
        Key::H => 0x08,
        Key::I => 0x09,
        Key::J => 0x0a,
        Key::K => 0x0b,
        Key::L => 0x0c, // FF — typical clear/redraw bind
        Key::M => 0x0d,
        Key::N => 0x0e,
        Key::O => 0x0f,
        Key::P => 0x10,
        Key::Q => 0x11,
        Key::R => 0x12,
        Key::S => 0x13,
        Key::T => 0x14,
        Key::U => 0x15,
        Key::V => 0x16,
        Key::W => 0x17,
        Key::X => 0x18,
        Key::Y => 0x19,
        Key::Z => 0x1a,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    //! Strict-layer tests: any wrong-encoding bug means typed bytes
    //! never reach the shell correctly, which is a corruption-class
    //! issue. Each documented mapping has at least one test.

    use super::*;

    fn no_mods() -> Modifiers {
        Modifiers::default()
    }

    fn just_ctrl() -> Modifiers {
        Modifiers { ctrl: true, ..Modifiers::default() }
    }

    /// "Default" modes: fresh terminal, no DECCKM. Most tests run
    /// against this — Termica's initial state.
    fn default_modes() -> TerminalModes {
        TerminalModes::default()
    }

    /// Modes after a program (less, vim, htop) has issued `\e[?1h`
    /// (DECCKM on). Arrow keys / Home / End must encode as SS3.
    fn app_cursor_modes() -> TerminalModes {
        TerminalModes { application_cursor: true, ..TerminalModes::default() }
    }

    /// Modes after a shell has issued `\e[?2004h` (bracketed paste
    /// on). Pasted text must be wrapped in `\e[200~` … `\e[201~`.
    fn bracketed_paste_modes() -> TerminalModes {
        TerminalModes { bracketed_paste: true, ..TerminalModes::default() }
    }

    fn text_event(s: &str) -> egui::Event {
        egui::Event::Text(s.to_string())
    }

    fn key_event(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    fn key_release(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: no_mods(),
        }
    }

    // --- text input -------------------------------------------------

    #[test]
    fn text_event_encodes_as_utf8() {
        let bytes = encode_event(&text_event("hello"), default_modes()).expect("text encodes");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn text_event_passes_through_non_ascii() {
        let bytes = encode_event(&text_event("café"), default_modes()).expect("text encodes");
        assert_eq!(bytes, "café".as_bytes());
    }

    #[test]
    fn empty_text_event_returns_none() {
        assert!(encode_event(&text_event(""), default_modes()).is_none());
    }

    #[test]
    fn paste_event_encodes_as_raw_bytes_when_bracketed_paste_off() {
        // Default modes => no wrapping; the paste payload reaches the
        // shell verbatim, same as if the user had typed it.
        let bytes = encode_event(&egui::Event::Paste("ls -la".into()), default_modes())
            .expect("paste encodes");
        assert_eq!(bytes, b"ls -la");
    }

    #[test]
    fn paste_event_wraps_in_brackets_when_bracketed_paste_on() {
        // After the shell turns on DECSET 2004 (`\e[?2004h`), pastes
        // must be wrapped in `\e[200~` / `\e[201~`. The shell uses
        // these markers to skip completion / history expansion of
        // the paste.
        let bytes = encode_event(&egui::Event::Paste("ls -la".into()), bracketed_paste_modes())
            .expect("paste encodes");
        assert_eq!(bytes, b"\x1b[200~ls -la\x1b[201~");
    }

    #[test]
    fn empty_paste_event_returns_none() {
        // No payload, no bytes. Guards against empty `\e[200~\e[201~`
        // markers in bracketed-paste mode, which would be harmless
        // but pointless.
        assert!(encode_event(&egui::Event::Paste(String::new()), default_modes()).is_none());
        assert!(
            encode_event(&egui::Event::Paste(String::new()), bracketed_paste_modes()).is_none()
        );
    }

    #[test]
    fn multiline_paste_wraps_payload_once_when_bracketed_paste_on() {
        // The whole pasted block — including embedded newlines — sits
        // inside ONE pair of markers; the shell treats it as a single
        // chunk. Without bracketed paste, those embedded `\n`s would
        // execute each line immediately and leave the shell stuck
        // mid-edit on the final line. This test pins the contract.
        let payload = "echo one\necho two\necho three";
        let bytes = encode_event(&egui::Event::Paste(payload.into()), bracketed_paste_modes())
            .expect("paste encodes");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x1b[200~");
        expected.extend_from_slice(payload.as_bytes());
        expected.extend_from_slice(b"\x1b[201~");
        assert_eq!(bytes, expected);
    }

    // --- key-release suppression -----------------------------------

    #[test]
    fn key_release_returns_none() {
        assert!(encode_event(&key_release(Key::A), default_modes()).is_none());
        assert!(encode_event(&key_release(Key::Enter), default_modes()).is_none());
    }

    // --- structural keys --------------------------------------------

    #[test]
    fn enter_encodes_as_cr() {
        assert_eq!(
            encode_event(&key_event(Key::Enter, no_mods()), default_modes()).unwrap(),
            b"\r"
        );
    }

    #[test]
    fn backspace_encodes_as_del() {
        // 0x7f — what xterm-class terminals expect by default. Some
        // shells will translate to ^H themselves if their stty says so.
        assert_eq!(
            encode_event(&key_event(Key::Backspace, no_mods()), default_modes()).unwrap(),
            b"\x7f"
        );
    }

    #[test]
    fn tab_encodes_as_tab() {
        assert_eq!(encode_event(&key_event(Key::Tab, no_mods()), default_modes()).unwrap(), b"\t");
    }

    #[test]
    fn escape_encodes_as_esc() {
        assert_eq!(
            encode_event(&key_event(Key::Escape, no_mods()), default_modes()).unwrap(),
            b"\x1b"
        );
    }

    // --- arrow keys -------------------------------------------------

    #[test]
    fn arrow_keys_encode_as_csi() {
        assert_eq!(
            encode_event(&key_event(Key::ArrowUp, no_mods()), default_modes()).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode_event(&key_event(Key::ArrowDown, no_mods()), default_modes()).unwrap(),
            b"\x1b[B"
        );
        assert_eq!(
            encode_event(&key_event(Key::ArrowRight, no_mods()), default_modes()).unwrap(),
            b"\x1b[C"
        );
        assert_eq!(
            encode_event(&key_event(Key::ArrowLeft, no_mods()), default_modes()).unwrap(),
            b"\x1b[D"
        );
    }

    // --- navigation keys -------------------------------------------

    #[test]
    fn home_end_pageup_pagedown_delete_encode_as_csi() {
        assert_eq!(
            encode_event(&key_event(Key::Home, no_mods()), default_modes()).unwrap(),
            b"\x1b[H"
        );
        assert_eq!(
            encode_event(&key_event(Key::End, no_mods()), default_modes()).unwrap(),
            b"\x1b[F"
        );
        assert_eq!(
            encode_event(&key_event(Key::PageUp, no_mods()), default_modes()).unwrap(),
            b"\x1b[5~"
        );
        assert_eq!(
            encode_event(&key_event(Key::PageDown, no_mods()), default_modes()).unwrap(),
            b"\x1b[6~"
        );
        assert_eq!(
            encode_event(&key_event(Key::Delete, no_mods()), default_modes()).unwrap(),
            b"\x1b[3~"
        );
        assert_eq!(
            encode_event(&key_event(Key::Insert, no_mods()), default_modes()).unwrap(),
            b"\x1b[2~"
        );
    }

    // --- Ctrl + letter ----------------------------------------------

    #[test]
    fn ctrl_a_through_z_map_to_c0_control_bytes() {
        let cases: &[(Key, u8)] = &[
            (Key::A, 0x01),
            (Key::B, 0x02),
            (Key::C, 0x03),
            (Key::D, 0x04),
            (Key::L, 0x0c),
            (Key::M, 0x0d),
            (Key::Z, 0x1a),
        ];
        for (key, want) in cases {
            let got =
                encode_event(&key_event(*key, just_ctrl()), default_modes()).expect("ctrl letter");
            assert_eq!(got, vec![*want], "Ctrl+{key:?}");
        }
    }

    #[test]
    fn ctrl_c_sends_etx_signal_byte() {
        // The single most safety-critical control byte we encode:
        // when typed at a foreground process this becomes SIGINT.
        assert_eq!(
            encode_event(&key_event(Key::C, just_ctrl()), default_modes()).unwrap(),
            vec![0x03]
        );
    }

    // --- function keys ----------------------------------------------

    #[test]
    fn function_keys_f1_to_f4_use_ss3_form() {
        assert_eq!(
            encode_event(&key_event(Key::F1, no_mods()), default_modes()).unwrap(),
            b"\x1bOP"
        );
        assert_eq!(
            encode_event(&key_event(Key::F4, no_mods()), default_modes()).unwrap(),
            b"\x1bOS"
        );
    }

    #[test]
    fn function_keys_f5_and_up_use_tilde_form() {
        assert_eq!(
            encode_event(&key_event(Key::F5, no_mods()), default_modes()).unwrap(),
            b"\x1b[15~"
        );
        assert_eq!(
            encode_event(&key_event(Key::F12, no_mods()), default_modes()).unwrap(),
            b"\x1b[24~"
        );
    }

    // --- application cursor mode (DECCKM) --------------------------
    //
    // Regression coverage for the `less` arrow-key bug: when a program
    // sets DECCKM via `\e[?1h`, arrow keys / Home / End must encode as
    // SS3 (`\eOA` etc.), not CSI (`\e[A`). The terminfo entry
    // `xterm-256color` has `kcuu1 = \EOA`, and `less`'s keymap is
    // bound to that.

    #[test]
    fn arrow_keys_use_ss3_form_in_application_cursor_mode() {
        assert_eq!(
            encode_event(&key_event(Key::ArrowUp, no_mods()), app_cursor_modes()).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            encode_event(&key_event(Key::ArrowDown, no_mods()), app_cursor_modes()).unwrap(),
            b"\x1bOB"
        );
        assert_eq!(
            encode_event(&key_event(Key::ArrowRight, no_mods()), app_cursor_modes()).unwrap(),
            b"\x1bOC"
        );
        assert_eq!(
            encode_event(&key_event(Key::ArrowLeft, no_mods()), app_cursor_modes()).unwrap(),
            b"\x1bOD"
        );
    }

    #[test]
    fn home_and_end_use_ss3_form_in_application_cursor_mode() {
        assert_eq!(
            encode_event(&key_event(Key::Home, no_mods()), app_cursor_modes()).unwrap(),
            b"\x1bOH"
        );
        assert_eq!(
            encode_event(&key_event(Key::End, no_mods()), app_cursor_modes()).unwrap(),
            b"\x1bOF"
        );
    }

    #[test]
    fn pageup_pagedown_unchanged_by_application_cursor_mode() {
        // Only arrows / Home / End flip form. PgUp / PgDn / Delete /
        // Insert keep their CSI tilde form regardless of DECCKM.
        assert_eq!(
            encode_event(&key_event(Key::PageUp, no_mods()), app_cursor_modes()).unwrap(),
            b"\x1b[5~"
        );
        assert_eq!(
            encode_event(&key_event(Key::PageDown, no_mods()), app_cursor_modes()).unwrap(),
            b"\x1b[6~"
        );
        assert_eq!(
            encode_event(&key_event(Key::Delete, no_mods()), app_cursor_modes()).unwrap(),
            b"\x1b[3~"
        );
    }

    #[test]
    fn enter_and_text_unchanged_by_application_cursor_mode() {
        // Enter stays \r; printable text stays UTF-8 bytes. DECCKM
        // doesn't touch the rest of the keyboard.
        assert_eq!(
            encode_event(&key_event(Key::Enter, no_mods()), app_cursor_modes()).unwrap(),
            b"\r"
        );
        assert_eq!(encode_event(&text_event("hi"), app_cursor_modes()).unwrap(), b"hi");
    }

    // --- unsupported events -----------------------------------------

    #[test]
    fn pointer_events_return_none() {
        let evt = egui::Event::PointerMoved(egui::Pos2::ZERO);
        assert!(encode_event(&evt, default_modes()).is_none());
    }
}
