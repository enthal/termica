//! Framing of a submitted editor command into the bytes written to the
//! PTY, per shell ([spec/03 §"fish"](../spec/03-shell-integration.md)).
//!
//! For zsh / bash the shell's own parser reads the command (its line
//! editor is disabled but it still parses multi-line continuation across
//! reads), so Termica writes the command text verbatim + `\r`.
//!
//! **fish** runs non-interactively via a read-eval loop that reads ONE
//! line per command (it can't use fish's reader — that's the prompt/echo
//! leak we removed). A multi-line command (Shift+Enter) would therefore be
//! split into separate lines, the first of which (`for …`, a trailing
//! `&&`, …) is incomplete and errors. So for fish we **base64-encode** the
//! whole command onto a single line; the bootstrap loop decodes it back to
//! the full (possibly multi-line) command and `eval`s it as one unit.
//! base64's alphabet (`A–Za–z0–9+/=`) has no tty-special bytes, so it
//! crosses the cooked-mode tty cleanly and its kernel echo is dropped by
//! `EchoSuppressor` exactly like plain text.

#![forbid(unsafe_code)]

use crate::integration::ShellSpec;

/// Standard base64 alphabet (RFC 4648).
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as standard (padded) base64. Hand-rolled to avoid a
/// dependency for ~20 lines; the fish bootstrap decodes with `base64 -d`.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(BASE64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 { BASE64_ALPHABET[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// The exact bytes to write to the PTY to submit `to_send` for `shell`.
///
/// - zsh / bash: `to_send` verbatim, then `\r` (the shell's parser handles
///   multi-line continuation itself).
/// - fish: `base64(to_send)`, then `\r` — one tty line the bootstrap loop
///   decodes back to the full command (multi-line safe).
///
/// `\r` (not `\n`) is the submit terminator everywhere; the tty's `ICRNL`
/// maps it to `\n` on the input side.
pub fn submission_bytes(to_send: &str, shell: ShellSpec) -> Vec<u8> {
    let mut bytes = match shell {
        ShellSpec::Fish => base64_encode(to_send.as_bytes()).into_bytes(),
        ShellSpec::Zsh | ShellSpec::Bash => to_send.as_bytes().to_vec(),
    };
    bytes.push(b'\r');
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        // RFC 4648 §10 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encodes_newlines_and_utf8() {
        assert_eq!(base64_encode("a\nb".as_bytes()), "YQpi");
        // Multi-byte UTF-8 round-trips byte-for-byte.
        assert_eq!(base64_encode("é".as_bytes()), "w6k=");
    }

    #[test]
    fn zsh_bash_send_command_verbatim_with_cr() {
        assert_eq!(submission_bytes("ls -la", ShellSpec::Zsh), b"ls -la\r");
        assert_eq!(submission_bytes("ls -la", ShellSpec::Bash), b"ls -la\r");
        // A multi-line command is left for the shell's parser — verbatim.
        assert_eq!(
            submission_bytes("for x in 1 2\ndo echo $x\ndone", ShellSpec::Zsh).last(),
            Some(&b'\r')
        );
        assert!(submission_bytes("a\nb", ShellSpec::Zsh).starts_with(b"a\nb"));
    }

    #[test]
    fn fish_base64_encodes_onto_one_line() {
        // Single line.
        assert_eq!(submission_bytes("ls", ShellSpec::Fish), b"bHM=\r");
        // Multi-line collapses to ONE tty line (no embedded \n) + \r.
        let bytes = submission_bytes("a\nb", ShellSpec::Fish);
        assert_eq!(bytes, b"YQpi\r");
        assert!(!bytes[..bytes.len() - 1].contains(&b'\n'), "no embedded newline before the CR");
    }

    #[test]
    fn fish_empty_command_is_bare_cr() {
        // Pressing Enter on an empty editor: base64("") == "" → just \r,
        // same as zsh/bash. The loop decodes "" → eval no-op → next prompt.
        assert_eq!(submission_bytes("", ShellSpec::Fish), b"\r");
        assert_eq!(submission_bytes("", ShellSpec::Zsh), b"\r");
    }
}
