//! Echo suppression for the submit path (Phase 4C).
//!
//! When [`crate::pane::PaneSession::submit_editor_command`] writes a
//! finished command to the PTY, the kernel's tty discipline echoes
//! the same bytes back through the master read end. Without
//! suppression, those bytes would land in the live `Term`'s grid as
//! a duplicate of the command that's already structurally captured
//! in the [`crate::block::Block::Running`] block. The duplicate
//! looks like a bug.
//!
//! Spec/04 §"Echo handling" enumerates the options and picks
//! **option (b)**: a small expected-bytes buffer primed at submit
//! time. Every incoming byte is matched against the head of the
//! buffer; matches are consumed (and dropped — never reaching the
//! VT parser or the grid); mismatches **immediately disengage**
//! suppression and pass the incoming bytes through unchanged. The
//! "never half-suppress" rule is the strict correctness invariant.
//!
//! Implementation notes:
//!
//! - Submit writes `<text>\r` to the PTY (CR, matching the existing
//!   `Enter`-key encoder in `input::encode_key`). The kernel's tty
//!   discipline in canonical mode with `ONLCR` echoes that back as
//!   `<text>\r\n`. So [`EchoSuppressor::expect`] transforms each
//!   `\r` byte in the primed sequence into `\r\n` at prime time.
//! - There is no time-based fallback in this 4C minimum: the
//!   prefix-match-on-mismatch-disengage rule prevents data loss in
//!   every realistic shell setup. A wall-clock 500 ms timeout (per
//!   spec/04) lands in a follow-up alongside the bootstrap timeout
//!   wall-clock work.
//! - There is no concurrent-priming: a second `expect` while the
//!   buffer is still armed *replaces* the buffer. Submit can't run
//!   while the pane is in `RawTerminal`, and submit always demotes
//!   to `RawTerminal` before the next prime, so a real session
//!   never primes twice in a row.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

/// Pending bytes the PTY is expected to echo back from the most
/// recent submit. Owned by [`crate::terminal::TerminalState`];
/// drained from inside [`crate::terminal::TerminalState::feed`].
#[derive(Debug, Default)]
pub struct EchoSuppressor {
    /// Bytes we still expect to see echoed. Empty when not armed.
    expected: VecDeque<u8>,
}

impl EchoSuppressor {
    /// New suppressor; nothing primed.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when at least one byte is still expected.
    pub fn is_armed(&self) -> bool {
        !self.expected.is_empty()
    }

    /// Length of the expected buffer (test/debug helper).
    pub fn pending_len(&self) -> usize {
        self.expected.len()
    }

    /// Drop any pending expectation. The next byte through
    /// [`Self::filter`] will pass through unchanged.
    pub fn disengage(&mut self) {
        self.expected.clear();
    }

    /// Prime the suppressor with the bytes we're about to write to
    /// the PTY. Replaces any prior expectation.
    ///
    /// Both `\r` and `\n` in `bytes` are translated to `\r\n` because
    /// the tty driver's `ONLCR` flag (on by default) maps each `\n`
    /// in echoed output to `\r\n`, and the kernel echoes typed `\r`
    /// as `\r\n` too. We don't emit `\r\n` literally from submit, so
    /// double-expansion isn't a risk in practice.
    pub fn expect(&mut self, bytes: &[u8]) {
        self.expected.clear();
        for &b in bytes {
            if b == b'\r' || b == b'\n' {
                self.expected.push_back(b'\r');
                self.expected.push_back(b'\n');
            } else {
                self.expected.push_back(b);
            }
        }
    }

    /// Apply suppression to a chunk of incoming PTY bytes. Returns
    /// only the bytes that survive — i.e. the bytes after any
    /// matching prefix has been consumed, **plus** every byte after
    /// the first mismatch (because that's where suppression
    /// disengages).
    ///
    /// Behavior in detail:
    ///
    /// - If not armed, returns `incoming` verbatim.
    /// - If armed, walks `incoming` byte-by-byte against the head
    ///   of the expected buffer:
    ///   - Match → consume both (suppressed byte does not survive).
    ///   - Mismatch → disengage; the mismatching byte AND every
    ///     subsequent byte in the chunk pass through unchanged.
    /// - If the chunk is fully consumed but the expected buffer
    ///   still has bytes, suppression stays armed for the next
    ///   chunk.
    pub fn filter(&mut self, incoming: &[u8]) -> Vec<u8> {
        if !self.is_armed() {
            return incoming.to_vec();
        }
        let mut survivors = Vec::new();
        let mut it = incoming.iter();
        for &b in it.by_ref() {
            match self.expected.front() {
                Some(&head) if head == b => {
                    self.expected.pop_front();
                    if self.expected.is_empty() {
                        // Suppression complete; the rest of the
                        // chunk passes through unchanged.
                        survivors.extend(it.copied());
                        return survivors;
                    }
                }
                Some(_) => {
                    // Mismatch — disengage and pass this byte plus
                    // the rest through.
                    self.disengage();
                    survivors.push(b);
                    survivors.extend(it.copied());
                    return survivors;
                }
                None => unreachable!("checked is_armed above"),
            }
        }
        survivors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_suppressor_is_not_armed() {
        let s = EchoSuppressor::new();
        assert!(!s.is_armed());
        assert_eq!(s.pending_len(), 0);
    }

    #[test]
    fn unarmed_filter_passes_bytes_through() {
        let mut s = EchoSuppressor::new();
        let out = s.filter(b"hello");
        assert_eq!(out, b"hello");
    }

    #[test]
    fn expect_arms_and_disengage_clears() {
        let mut s = EchoSuppressor::new();
        s.expect(b"foo");
        assert!(s.is_armed());
        assert_eq!(s.pending_len(), 3);
        s.disengage();
        assert!(!s.is_armed());
    }

    #[test]
    fn matching_prefix_is_suppressed() {
        let mut s = EchoSuppressor::new();
        s.expect(b"echo hi");
        let out = s.filter(b"echo hi");
        assert_eq!(out, b"", "fully matching incoming should be fully suppressed");
        assert!(!s.is_armed(), "buffer drained");
    }

    #[test]
    fn matched_prefix_then_post_echo_passes_through() {
        let mut s = EchoSuppressor::new();
        s.expect(b"echo hi");
        // Echo of "echo hi" followed by the shell's "hi\r\n" output.
        let out = s.filter(b"echo hihi\r\n");
        assert_eq!(out, b"hi\r\n", "post-echo bytes survive verbatim");
        assert!(!s.is_armed());
    }

    #[test]
    fn mismatch_disengages_and_passes_remaining() {
        let mut s = EchoSuppressor::new();
        s.expect(b"echo hi");
        // Suppose the kernel mangled the bytes (or the user typed
        // ahead before the echo); the suppressor must NOT half-
        // suppress.
        let out = s.filter(b"echOMUTANT\r");
        assert_eq!(out, b"OMUTANT\r", "mismatch byte and rest pass through");
        assert!(!s.is_armed());
    }

    #[test]
    fn newline_is_expected_as_crlf() {
        // Submit "L1\nL2\r" (multi-line via Shift+Enter then bare
        // Enter to submit); kernel ONLCR makes the echo "L1\r\nL2\r\n".
        let mut s = EchoSuppressor::new();
        s.expect(b"L1\nL2\r");
        let out = s.filter(b"L1\r\nL2\r\n");
        assert_eq!(out, b"", "embedded \\n must round-trip via \\r\\n");
        assert!(!s.is_armed());
    }

    #[test]
    fn carriage_return_is_expected_as_crlf() {
        // Submit writes "cmd\r"; tty echo with ONLCR produces
        // "cmd\r\n" back.
        let mut s = EchoSuppressor::new();
        s.expect(b"cmd\r");
        assert_eq!(s.pending_len(), 5, "\\r expanded to \\r\\n");
        let out = s.filter(b"cmd\r\n");
        assert_eq!(out, b"");
        assert!(!s.is_armed());
    }

    #[test]
    fn multibyte_utf8_byte_compare_works() {
        // Submit is byte-level — multi-byte UTF-8 characters round-
        // trip as their bytes.
        let mut s = EchoSuppressor::new();
        s.expect("éàü".as_bytes());
        let out = s.filter("éàü extra".as_bytes());
        assert_eq!(out, b" extra");
    }

    #[test]
    fn suppression_survives_split_across_chunks() {
        let mut s = EchoSuppressor::new();
        s.expect(b"echo hello");
        // First chunk: first half of the expected echo.
        let out = s.filter(b"echo");
        assert_eq!(out, b"");
        assert!(s.is_armed(), "still expecting the second half");
        // Second chunk: the rest plus a trailing newline.
        let out = s.filter(b" hello\r\n");
        assert_eq!(out, b"\r\n", "post-echo content survives");
        assert!(!s.is_armed());
    }

    #[test]
    fn second_expect_replaces_pending_buffer() {
        let mut s = EchoSuppressor::new();
        s.expect(b"first");
        s.expect(b"second");
        assert_eq!(s.pending_len(), 6, "buffer replaced, not appended");
    }

    #[test]
    fn empty_expect_disengages() {
        let mut s = EchoSuppressor::new();
        s.expect(b"abc");
        s.expect(b"");
        assert!(!s.is_armed(), "empty expect is the same as disengaging");
    }

    #[test]
    fn mismatch_in_middle_of_match_disengages_immediately() {
        // Real scenario: shell echo got truncated for some weird
        // reason. We must NOT eat the first byte of the actual
        // command output.
        let mut s = EchoSuppressor::new();
        s.expect(b"ls -la");
        // Partial match "ls -" then mismatch (suppose the echo
        // got cut off and the actual output starts with "file1").
        let out = s.filter(b"ls -file1\r\n");
        assert_eq!(out, b"file1\r\n");
        assert!(!s.is_armed());
    }
}
