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
///
/// # Modifier policy
///
/// Modifiers may NEVER be silently dropped. The only modifier combo
/// this encoder produces output for is `Ctrl + letter` (with no
/// other modifier held); everything else falls through to a
/// per-key table that is only valid for **unmodified** keys. Any
/// other modifier combination — `Cmd+ArrowUp`, `Shift+Enter`,
/// `Alt+T`, … — returns `None`, leaving the key combo *unmapped*
/// (no PTY write). App-level shortcuts (Cmd+T new-tab,
/// Cmd+Shift+]/[ tab nav, etc.) intercept these events at the
/// call site in [`crate::render_pane`], not here.
pub fn encode_key(key: Key, modifiers: Modifiers, modes: TerminalModes) -> Option<Vec<u8>> {
    // Ctrl + letter takes precedence over per-key encodings: even
    // though Ctrl+M would otherwise be Return, the standard terminal
    // contract says Ctrl+letter → C0 control byte.
    if modifiers.ctrl
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.mac_cmd
        && let Some(byte) = ctrl_letter_byte(key)
    {
        return Some(vec![byte]);
    }

    // Modifier gate: the per-key tables below are only valid for
    // unmodified keys. If ANY modifier is held at this point, the
    // combo is unmapped — return None and let the caller (or no
    // one) handle it. This is what makes Cmd+Up ≠ Up.
    if modifiers.ctrl || modifiers.alt || modifiers.shift || modifiers.mac_cmd {
        return None;
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

/// Outcome of a mouse-wheel tick.
///
/// In the **main** screen we move our local scrollback view (no
/// bytes flow to the shell). In **alternate** screen — `vim`,
/// `less`, `htop`, `fzf` etc. — there is no scrollback of our own;
/// instead we forward the wheel as N arrow keystrokes, which is
/// what every modern terminal does (iTerm2's "Scroll wheel sends
/// arrow keys when in alternate screen" is the canonical name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WheelOutcome {
    /// Adjust the renderer's display offset by `lines` (positive =
    /// up / toward older content). Pure UI action; nothing reaches
    /// the PTY.
    ScrollDisplay(i32),
    /// Write these bytes to the PTY. Already encoded for the
    /// current cursor-key mode (CSI vs SS3).
    SendBytes(Vec<u8>),
}

/// Build the ordered PTY payload for a single frame of input,
/// combining alt-screen wheel motion with raw keystroke events.
///
/// **Invariant:** wheel-derived bytes ALWAYS precede key-derived
/// bytes in the returned `Vec`. Foreground programs (`less`, `vim`,
/// `htop`, `fzf`) read PTY stdin sequentially. A quit keystroke
/// (`q` in `less`, `:q` in `vim`) causes the program to exit and
/// stop reading; any wheel bytes queued AFTER the quit byte stay
/// in the PTY input buffer and arrive at the SHELL after the
/// program exits, where they land as stray characters in the
/// user's next command. Trackpad momentum scroll is the realistic
/// trigger — the user lifts their fingers, the smoothed delta
/// keeps fading for several frames, and they press `q` mid-fade.
///
/// Pure helper so the ordering invariant is unit-testable without
/// an egui context. `key_events` may include non-key events
/// (`MouseMoved`, `Scroll`, …) — `encode_event` returns `None` for
/// those and they are skipped. Editor-bound key events should be
/// filtered out by the caller before passing them in here (they
/// must not reach the PTY at all).
pub fn compose_alt_screen_frame_bytes(
    wheel_lines: i32,
    wheel_alt_screen: bool,
    key_events: &[&egui::Event],
    modes: TerminalModes,
) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(WheelOutcome::SendBytes(bytes)) =
        classify_wheel(wheel_lines, wheel_alt_screen, modes)
    {
        out.extend(bytes);
    }
    for event in key_events {
        if let Some(bytes) = encode_event(event, modes) {
            out.extend(bytes);
        }
    }
    out
}

/// Decide what to do with `lines` worth of wheel motion. Positive
/// `lines` is "scroll up" (toward older content); negative is down.
/// Returns `None` when the motion is too small to act on.
///
/// Pure function — lifts the wheel-routing decision out of the
/// app's update loop so it's unit-testable without an egui context.
pub fn classify_wheel(lines: i32, alt_screen: bool, modes: TerminalModes) -> Option<WheelOutcome> {
    if lines == 0 {
        return None;
    }
    if !alt_screen {
        return Some(WheelOutcome::ScrollDisplay(lines));
    }

    // Alt-screen: forward as `|lines|` arrow keystrokes. The key
    // direction matches the user's natural "wheel up = move up"
    // expectation: positive `lines` (the user wheeled up) means
    // ArrowUp.
    let (key, count) = if lines > 0 {
        (Key::ArrowUp, lines as usize)
    } else {
        (Key::ArrowDown, (-lines) as usize)
    };
    let one_keystroke = encode_key(key, Modifiers::default(), modes)?;
    let mut payload = Vec::with_capacity(one_keystroke.len() * count);
    for _ in 0..count {
        payload.extend_from_slice(&one_keystroke);
    }
    Some(WheelOutcome::SendBytes(payload))
}

/// True when `(key, mods)` is the platform's "copy selection to
/// clipboard" shortcut.
///
/// - **macOS**: `Cmd+C`, nothing else held.
/// - **Linux / Windows**: `Ctrl+Shift+C`, nothing else held. The plain
///   `Ctrl+C` is the universal shell SIGINT (ETX, `0x03`) and must
///   continue to reach the PTY untouched — that's why the *Shift*
///   modifier is mandatory off-macOS. (xterm, gnome-terminal, konsole,
///   alacritty all settle on Ctrl+Shift+C for the same reason.)
///
/// Pure; `is_macos` is the caller's compile-time choice
/// (`cfg!(target_os = "macos")`) so this helper is testable on any
/// host.
pub fn is_copy_shortcut(key: Key, mods: Modifiers, is_macos: bool) -> bool {
    if key != Key::C {
        return false;
    }
    if is_macos {
        mods.mac_cmd && !mods.ctrl && !mods.alt && !mods.shift
    } else {
        mods.ctrl && mods.shift && !mods.alt && !mods.mac_cmd
    }
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

    // --- mouse wheel routing ---------------------------------------
    //
    // Main screen → ScrollDisplay (no PTY traffic).
    // Alternate screen → encode as N arrow keystrokes, using the
    // current cursor-key mode (CSI vs SS3).

    #[test]
    fn wheel_zero_lines_is_none() {
        assert!(classify_wheel(0, false, default_modes()).is_none());
        assert!(classify_wheel(0, true, default_modes()).is_none());
    }

    #[test]
    fn wheel_main_screen_scrolls_display() {
        assert_eq!(classify_wheel(3, false, default_modes()), Some(WheelOutcome::ScrollDisplay(3)));
        assert_eq!(
            classify_wheel(-5, false, default_modes()),
            Some(WheelOutcome::ScrollDisplay(-5))
        );
    }

    #[test]
    fn wheel_alt_screen_emits_arrow_up_keystrokes_csi() {
        // Plain (non-DECCKM) alt screen: emit CSI arrow ups.
        // 3 lines up = 3 × `\x1b[A`.
        let got = classify_wheel(3, true, default_modes()).expect("alt screen wheel");
        assert_eq!(got, WheelOutcome::SendBytes(b"\x1b[A\x1b[A\x1b[A".to_vec()));
    }

    #[test]
    fn wheel_alt_screen_emits_arrow_down_keystrokes_csi() {
        let got = classify_wheel(-2, true, default_modes()).expect("alt screen wheel");
        assert_eq!(got, WheelOutcome::SendBytes(b"\x1b[B\x1b[B".to_vec()));
    }

    #[test]
    fn wheel_alt_screen_emits_ss3_when_application_cursor_on() {
        // This is the actual `less` case: less sets DECCKM, so we
        // must emit `\eOA` not `\e[A`. Without this every wheel tick
        // would land in less's "unknown ESC sequence" handler.
        let got = classify_wheel(2, true, app_cursor_modes()).expect("ss3 wheel");
        assert_eq!(got, WheelOutcome::SendBytes(b"\x1bOA\x1bOA".to_vec()));
    }

    // --- ordered frame payload (wheel-before-keys invariant) -------
    //
    // Regression for the `q`-during-momentum-scroll bug: less reads
    // PTY stdin in order, `q` makes it exit; any wheel bytes that
    // were queued AFTER `q` end up at the shell as stray input.

    #[test]
    fn compose_frame_orders_wheel_bytes_before_quit_keystroke() {
        // 3 lines of momentum scroll + a `q` press in the same
        // frame. Output must be: <wheel arrow-ups> then `q`. egui
        // delivers a printable char as `Event::Text` alongside the
        // `Event::Key` — `encode_event` only emits bytes for the
        // `Text` path for unmodified letters, so that's what the
        // test uses.
        let q_text = egui::Event::Text("q".to_string());
        let bytes = compose_alt_screen_frame_bytes(3, true, &[&q_text], app_cursor_modes());
        // less is in DECCKM, so wheel-up = ArrowUp = `\x1bOA`.
        // q = `q` (single byte).
        assert_eq!(bytes, b"\x1bOA\x1bOA\x1bOAq".to_vec());
        // And explicitly: q is the very last byte.
        assert_eq!(*bytes.last().unwrap(), b'q', "q must be the last byte");
        // And the wheel prefix is the whole sequence minus the last
        // byte.
        assert!(
            bytes.starts_with(b"\x1bOA\x1bOA\x1bOA"),
            "wheel bytes must precede the keystroke; got {:?}",
            bytes
        );
    }

    #[test]
    fn compose_frame_with_no_wheel_returns_just_key_bytes() {
        let q_text = egui::Event::Text("q".to_string());
        let bytes = compose_alt_screen_frame_bytes(0, true, &[&q_text], default_modes());
        assert_eq!(bytes, b"q".to_vec());
    }

    #[test]
    fn compose_frame_with_no_keys_returns_just_wheel_bytes() {
        let bytes = compose_alt_screen_frame_bytes(2, true, &[], default_modes());
        assert_eq!(bytes, b"\x1b[A\x1b[A".to_vec());
    }

    #[test]
    fn compose_frame_with_main_screen_wheel_emits_no_wheel_bytes() {
        // Non-alt-screen wheel goes to ScrollDisplay, not PTY.
        // Only the keystroke bytes should appear.
        let q_text = egui::Event::Text("q".to_string());
        let bytes = compose_alt_screen_frame_bytes(3, false, &[&q_text], default_modes());
        assert_eq!(bytes, b"q".to_vec());
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

    // --- copy shortcut ----------------------------------------------

    fn mods_mac_cmd() -> Modifiers {
        Modifiers { mac_cmd: true, ..Modifiers::default() }
    }
    fn mods_ctrl_shift() -> Modifiers {
        Modifiers { ctrl: true, shift: true, ..Modifiers::default() }
    }

    #[test]
    fn copy_shortcut_macos_is_cmd_c() {
        assert!(is_copy_shortcut(Key::C, mods_mac_cmd(), true));
    }

    #[test]
    fn copy_shortcut_linux_is_ctrl_shift_c() {
        assert!(is_copy_shortcut(Key::C, mods_ctrl_shift(), false));
    }

    #[test]
    fn copy_shortcut_plain_ctrl_c_is_not_copy_offmac() {
        // Critical: plain Ctrl+C must stay SIGINT, NEVER be hijacked
        // for copy. Hijacking would silently break every shell on
        // every Linux runner.
        assert!(!is_copy_shortcut(Key::C, just_ctrl(), false));
    }

    #[test]
    fn copy_shortcut_cmd_c_on_linux_is_not_copy() {
        // Cross-OS rules must NOT bleed through: Cmd+C is not a copy
        // shortcut on a Linux build.
        assert!(!is_copy_shortcut(Key::C, mods_mac_cmd(), false));
    }

    #[test]
    fn copy_shortcut_ctrl_shift_c_on_mac_is_not_copy() {
        // Same in reverse: on macOS the binding is Cmd+C, not the
        // X11/Windows shortcut. Keeps the platform feel correct.
        assert!(!is_copy_shortcut(Key::C, mods_ctrl_shift(), true));
    }

    #[test]
    fn copy_shortcut_wrong_key_is_never_copy() {
        for key in [Key::A, Key::V, Key::X, Key::Enter] {
            assert!(!is_copy_shortcut(key, mods_mac_cmd(), true));
            assert!(!is_copy_shortcut(key, mods_ctrl_shift(), false));
        }
    }

    // --- unmapped modifier combos return None ---------------------

    #[test]
    fn cmd_plus_arrow_is_unmapped() {
        // The user's canonical case: Cmd+ArrowUp must NOT collapse
        // down to plain ArrowUp. The encoder's per-key tables only
        // apply to unmodified keys.
        assert!(encode_key(Key::ArrowUp, mods_mac_cmd(), default_modes()).is_none());
        assert!(encode_key(Key::ArrowDown, mods_mac_cmd(), default_modes()).is_none());
        assert!(encode_key(Key::ArrowLeft, mods_mac_cmd(), default_modes()).is_none());
        assert!(encode_key(Key::ArrowRight, mods_mac_cmd(), default_modes()).is_none());
    }

    #[test]
    fn cmd_plus_special_keys_are_unmapped() {
        // Same rule for Enter, Home, End, PageUp, PageDown,
        // Tab — under Cmd, none of these emit their unmodified
        // byte. App-level shortcuts (Cmd+T, Cmd+Shift+], …) live
        // outside the encoder.
        for key in [Key::Enter, Key::Home, Key::End, Key::Tab, Key::Escape] {
            assert!(
                encode_key(key, mods_mac_cmd(), default_modes()).is_none(),
                "Cmd+{key:?} should be unmapped, not collapse to plain {key:?}"
            );
        }
    }

    #[test]
    fn shift_plus_arrow_is_unmapped() {
        // Shift+arrows would be select-extend in some terminals; we
        // don't implement that. Return None rather than secretly
        // emit a plain arrow keystroke.
        let shift = Modifiers { shift: true, ..Modifiers::default() };
        assert!(encode_key(Key::ArrowUp, shift, default_modes()).is_none());
        assert!(encode_key(Key::ArrowLeft, shift, default_modes()).is_none());
    }

    #[test]
    fn alt_plus_arrow_is_unmapped() {
        let alt = Modifiers { alt: true, ..Modifiers::default() };
        assert!(encode_key(Key::ArrowUp, alt, default_modes()).is_none());
    }

    #[test]
    fn ctrl_plus_arrow_is_unmapped() {
        // Even Ctrl+arrow is unmapped today. Some shells map this
        // to word-motion, but we don't have a wire encoding for it
        // and don't want to silently encode plain CSI/SS3 ArrowUp.
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        assert!(encode_key(Key::ArrowUp, ctrl, default_modes()).is_none());
    }

    #[test]
    fn ctrl_plus_letter_still_encodes_with_my_new_gate() {
        // Regression: the modifier gate must NOT swallow the
        // Ctrl+letter branch. Ctrl+C → 0x03 (SIGINT) still works.
        assert_eq!(encode_key(Key::C, just_ctrl(), default_modes()).unwrap(), vec![0x03]);
        assert_eq!(encode_key(Key::A, just_ctrl(), default_modes()).unwrap(), vec![0x01]);
    }

    #[test]
    fn ctrl_plus_letter_with_any_other_modifier_is_unmapped() {
        // Ctrl+Shift+C / Cmd+Ctrl+C / Alt+Ctrl+C all skip the
        // Ctrl+letter branch (which requires NO other modifier).
        let ctrl_shift = Modifiers { ctrl: true, shift: true, ..Modifiers::default() };
        assert!(encode_key(Key::C, ctrl_shift, default_modes()).is_none());
        let ctrl_alt = Modifiers { ctrl: true, alt: true, ..Modifiers::default() };
        assert!(encode_key(Key::C, ctrl_alt, default_modes()).is_none());
        let ctrl_cmd = Modifiers { ctrl: true, mac_cmd: true, ..Modifiers::default() };
        assert!(encode_key(Key::C, ctrl_cmd, default_modes()).is_none());
    }

    #[test]
    fn plain_keys_still_encode_normally() {
        // Sanity that the gate doesn't break the no-modifier case.
        assert_eq!(encode_key(Key::Enter, no_mods(), default_modes()).unwrap(), b"\r");
        assert_eq!(encode_key(Key::ArrowUp, no_mods(), default_modes()).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(Key::Tab, no_mods(), default_modes()).unwrap(), b"\t");
    }
}
