//! Shell-integration markers.
//!
//! Parses pre-assembled OSC parameter slices into [`MarkerEvent`]s.
//! Two namespaces, per [spec/03](../spec/03-shell-integration.md):
//!
//! 1. **OSC 133** — the FinalTerm / iTerm2 prompt-state convention.
//! 2. **OSC 1337 ; Termica…=…** — Termica's extensions (riding the
//!    iTerm2-private OSC namespace).
//!
//! This module is **only the parser**. The byte-stream → OSC-param
//! routing lives in [`crate::osc`] (a `vte::Perform` impl that
//! dispatches the same params to both OSC 7 handling and to us
//! via [`parse_osc_params`]). The strict "no raw-byte pattern
//! matching" rule from the spec is enforced by giving the
//! parser pre-assembled params it can't pretend are raw bytes.
//!
//! Phase 3A scope: this file + integration into [`crate::osc`].
//! Phase 3B will consume [`MarkerEvent`]s in the `PromptController`
//! state machine.

#![forbid(unsafe_code)]

use std::path::PathBuf;

/// Anything the shell told us via a marker OSC. The
/// `PromptController` ([spec/05](../spec/05-pane-modes.md))
/// consumes these in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerEvent {
    /// `OSC 133 ; A` — shell is about to draw the prompt.
    PromptStart,
    /// `OSC 133 ; B` — prompt is drawn, shell is ready to read the
    /// command line. This is the gate that promotes a pane into
    /// `ShellPromptEditor` mode.
    PromptEnd,
    /// `OSC 133 ; C` — user pressed Enter; the shell is about to
    /// run the command.
    CommandStart,
    /// `OSC 133 ; D ; <exit>` (with optional duration extension) —
    /// command finished. `exit` is the integer exit status; the
    /// shell may also append a duration in milliseconds.
    CommandEnd { exit: i32, duration_ms: Option<u64> },
    /// `OSC 1337 ; TermicaCwd=<file-uri>` — the shell's current
    /// working directory at the moment of emission. `OSC 7` is
    /// parsed elsewhere (in [`crate::osc`]) but ultimately produces
    /// the same event so the consumer doesn't have to care which
    /// source it came from.
    Cwd(PathBuf),
    /// `OSC 1337 ; TermicaVersion=<u32>` — the integration script's
    /// protocol version. The `PromptController` uses this to refuse
    /// integration handshakes it can't speak.
    ProtocolVersion(u32),
    /// `OSC 1337 ; TermicaShell=<bash|zsh>` — which shell the
    /// integration script announced. Decoupled from
    /// `ProtocolVersion` because the script emits the two OSCs
    /// separately and they don't arrive atomically.
    Shell(ShellKind),
}

/// The shell kinds we recognise from `TermicaShell=…`. `Unknown`
/// is reserved for values our parser doesn't know about yet, so
/// callers can pattern-match exhaustively without losing information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    /// `TermicaShell=` with a value we don't have an enum variant
    /// for. Carrying the raw bytes here would balloon the type;
    /// the value is the protocol/identity signal anyway, and the
    /// `Unknown` arm lets consumers fall back to safe defaults.
    Unknown,
}

/// Parse a single dispatched OSC parameter list into a marker event.
///
/// `params[0]` is the OSC number as bytes (e.g. `b"133"`). Subsequent
/// elements are the ` ; `-separated payload pieces. Returns `None`
/// for OSCs we don't recognise — including OSC 1337 sequences whose
/// key isn't ours (those belong to iTerm2 and are not our concern).
pub fn parse_osc_params(params: &[&[u8]]) -> Option<MarkerEvent> {
    if params.is_empty() {
        return None;
    }
    match params[0] {
        b"133" => parse_osc_133(&params[1..]),
        b"1337" => parse_osc_1337_termica(&params[1..]),
        _ => None,
    }
}

/// Parse the params *after* the `133` number. `args[0]` is the
/// subcommand letter (`A` / `B` / `C` / `D`); for `D` the next
/// element is the exit code. We tolerate ASCII or UTF-8 byte
/// payloads but the actual content is always ASCII per the spec.
fn parse_osc_133(args: &[&[u8]]) -> Option<MarkerEvent> {
    let letter = args.first()?;
    match *letter {
        b"A" => Some(MarkerEvent::PromptStart),
        b"B" => Some(MarkerEvent::PromptEnd),
        b"C" => Some(MarkerEvent::CommandStart),
        b"D" => {
            // `D` carries an exit code in `args[1]`. Missing or
            // unparseable → -1 (the "unknown" sentinel), since a
            // default of 0 would silently lie that the command
            // succeeded. The spec mentions optional duration; the
            // sample integration scripts in spec/03 don't currently
            // emit it, so we leave it `None` until they do.
            let exit = args
                .get(1)
                .and_then(|b| std::str::from_utf8(b).ok())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(-1);
            Some(MarkerEvent::CommandEnd { exit, duration_ms: None })
        }
        _ => None,
    }
}

/// Parse the params *after* the `1337` number. We accept only the
/// `key=value` shapes we own (`TermicaVersion=`, `TermicaShell=`,
/// `TermicaCwd=`). Anything else is an iTerm2-private payload (or
/// some other tool's extension) and we ignore it.
fn parse_osc_1337_termica(args: &[&[u8]]) -> Option<MarkerEvent> {
    // Each arg should be a `key=value` pair. We only ever emit one
    // marker event per dispatch — the integration scripts emit one
    // OSC 1337 per piece of data, so in practice `args.len() == 1`,
    // but we scan defensively in case a shell ever batches.
    for arg in args {
        let s = std::str::from_utf8(arg).ok()?;
        if let Some(v) = s.strip_prefix("TermicaVersion=") {
            return v.parse::<u32>().ok().map(MarkerEvent::ProtocolVersion);
        }
        if let Some(v) = s.strip_prefix("TermicaShell=") {
            let kind = match v {
                "bash" => ShellKind::Bash,
                "zsh" => ShellKind::Zsh,
                _ => ShellKind::Unknown,
            };
            return Some(MarkerEvent::Shell(kind));
        }
        if let Some(v) = s.strip_prefix("TermicaCwd=") {
            return crate::osc::parse_osc7_cwd(v).map(MarkerEvent::Cwd);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build the `params` shape vte::Perform receives so the
    // test reads like "OSC 133; A".
    fn osc(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    fn params_ref(parts: &[Vec<u8>]) -> Vec<&[u8]> {
        parts.iter().map(|p| p.as_slice()).collect()
    }

    // ---- OSC 133 -----------------------------------------------------

    #[test]
    fn osc_133_a_is_prompt_start() {
        let parts = osc(&["133", "A"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), Some(MarkerEvent::PromptStart));
    }

    #[test]
    fn osc_133_b_is_prompt_end() {
        let parts = osc(&["133", "B"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), Some(MarkerEvent::PromptEnd));
    }

    #[test]
    fn osc_133_c_is_command_start() {
        let parts = osc(&["133", "C"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), Some(MarkerEvent::CommandStart));
    }

    #[test]
    fn osc_133_d_with_zero_exit() {
        let parts = osc(&["133", "D", "0"]);
        let p = params_ref(&parts);
        assert_eq!(
            parse_osc_params(&p),
            Some(MarkerEvent::CommandEnd { exit: 0, duration_ms: None })
        );
    }

    #[test]
    fn osc_133_d_with_nonzero_exit() {
        let parts = osc(&["133", "D", "127"]);
        let p = params_ref(&parts);
        assert_eq!(
            parse_osc_params(&p),
            Some(MarkerEvent::CommandEnd { exit: 127, duration_ms: None })
        );
    }

    #[test]
    fn osc_133_d_without_exit_treats_as_unknown_status() {
        // Shells *should* always emit the exit; some implementations
        // miss it. Falling back to `exit: 0` would be wrong (silently
        // pretending the command succeeded); we treat the absence as
        // a -1 sentinel since `Option<i32>` would balloon downstream
        // consumers.
        let parts = osc(&["133", "D"]);
        let p = params_ref(&parts);
        assert_eq!(
            parse_osc_params(&p),
            Some(MarkerEvent::CommandEnd { exit: -1, duration_ms: None })
        );
    }

    #[test]
    fn osc_133_d_with_garbage_exit_is_unknown_status() {
        // Defensive: a malformed exit code shouldn't crash the
        // parser. Mark as unknown (-1) and move on.
        let parts = osc(&["133", "D", "not-a-number"]);
        let p = params_ref(&parts);
        assert_eq!(
            parse_osc_params(&p),
            Some(MarkerEvent::CommandEnd { exit: -1, duration_ms: None })
        );
    }

    #[test]
    fn osc_133_unknown_letter_is_ignored() {
        // FinalTerm has additional letters (e.g. `E`) we don't
        // recognise. Ignore rather than guess.
        let parts = osc(&["133", "E"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), None);
    }

    // ---- OSC 1337 Termica… ------------------------------------------

    #[test]
    fn osc_1337_termica_version() {
        let parts = osc(&["1337", "TermicaVersion=1"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), Some(MarkerEvent::ProtocolVersion(1)));
    }

    #[test]
    fn osc_1337_termica_version_garbage_is_ignored() {
        let parts = osc(&["1337", "TermicaVersion=not-a-number"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), None);
    }

    #[test]
    fn osc_1337_termica_shell_bash() {
        let parts = osc(&["1337", "TermicaShell=bash"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), Some(MarkerEvent::Shell(ShellKind::Bash)));
    }

    #[test]
    fn osc_1337_termica_shell_zsh() {
        let parts = osc(&["1337", "TermicaShell=zsh"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), Some(MarkerEvent::Shell(ShellKind::Zsh)));
    }

    #[test]
    fn osc_1337_termica_shell_unknown_kind_carries_through() {
        let parts = osc(&["1337", "TermicaShell=fish"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), Some(MarkerEvent::Shell(ShellKind::Unknown)));
    }

    #[test]
    fn osc_1337_termica_cwd_with_file_uri() {
        let parts = osc(&["1337", "TermicaCwd=file:///Users/tim/code"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), Some(MarkerEvent::Cwd(PathBuf::from("/Users/tim/code"))));
    }

    #[test]
    fn osc_1337_termica_cwd_percent_decodes() {
        // Spaces and Unicode must round-trip — the bash integration
        // script percent-encodes them on the way out.
        let parts = osc(&["1337", "TermicaCwd=file:///Users/tim/A%20space"]);
        let p = params_ref(&parts);
        assert_eq!(
            parse_osc_params(&p),
            Some(MarkerEvent::Cwd(PathBuf::from("/Users/tim/A space")))
        );
    }

    #[test]
    fn osc_1337_non_termica_key_is_ignored() {
        // iTerm2's own keys ride OSC 1337 too. We don't claim them.
        let parts = osc(&["1337", "RemoteHost=user@host"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), None);
    }

    #[test]
    fn osc_1337_empty_payload_is_ignored() {
        let parts = osc(&["1337"]);
        let p = params_ref(&parts);
        assert_eq!(parse_osc_params(&p), None);
    }

    // ---- top-level dispatch -----------------------------------------

    #[test]
    fn unknown_osc_number_is_none() {
        // OSC 0 (window title), OSC 2 (icon title), OSC 4 (palette)
        // are all common; we must not claim them.
        for n in ["0", "2", "4", "8", "52"] {
            let parts = osc(&[n, "anything"]);
            let p = params_ref(&parts);
            assert_eq!(parse_osc_params(&p), None, "OSC {n} should not produce a marker");
        }
    }

    #[test]
    fn empty_params_is_none() {
        let p: Vec<&[u8]> = Vec::new();
        assert_eq!(parse_osc_params(&p), None);
    }
}
