//! OSC sniffer.
//!
//! `alacritty_terminal` already drives a full VT parser to maintain
//! the grid, but it discards OSC sequences it doesn't know about
//! (notably OSC 7 — "current working directory", which most shells
//! emit on `cd`). We need those for clickable paths and the status
//! header.
//!
//! Rather than fork or extend alacritty, we run a second thin VT
//! parser in parallel. Each byte the terminal sees is also fed to
//! our [`OscSniffer`], whose [`vte::Perform`] impl extracts the
//! sequences we care about. The sniffer never paints, never changes
//! the grid; alacritty stays the source of truth for everything
//! else.
//!
//! Today the sniffer only watches OSC 7. Phase 3 will plug Termica's
//! own marker sequences (OSC 133 + OSC 1337 ; Termica=…) onto the
//! same pipe.

#![forbid(unsafe_code)]

use std::path::PathBuf;

/// Snapshot of the latest signals the [`OscSniffer`] has seen.
#[derive(Debug, Default, Clone)]
pub struct OscState {
    /// Most recent `OSC 7 ; <file-url-or-path> ST` payload, decoded
    /// to an absolute path. `None` until the shell emits one (which
    /// in zsh happens automatically on `cd`; in bash requires a
    /// hook — see the Phase 3 integration script).
    pub cwd: Option<PathBuf>,
}

/// VT byte-stream peeker.
///
/// Fed every byte the PTY produces, *before or after* alacritty's
/// full parser — the order doesn't matter because we never touch
/// the grid. Holds a small [`OscState`] that callers query each
/// frame.
pub struct OscSniffer {
    parser: vte::Parser,
    state: OscState,
}

impl OscSniffer {
    /// Construct a sniffer with default state (no cwd yet).
    pub fn new() -> Self {
        Self { parser: vte::Parser::new(), state: OscState::default() }
    }

    /// Feed a single byte. Updates `self.state` if the byte
    /// completes a recognised OSC sequence.
    pub fn feed_byte(&mut self, byte: u8) {
        self.feed(&[byte]);
    }

    /// Feed a slice. `vte::Parser::advance` itself takes a slice; we
    /// briefly wrap our state in a [`Performer`] for the call, then
    /// read the result back.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut perform = Performer { state: &mut self.state };
        self.parser.advance(&mut perform, bytes);
    }

    /// Borrow the current state.
    pub fn state(&self) -> &OscState {
        &self.state
    }
}

impl Default for OscSniffer {
    fn default() -> Self {
        Self::new()
    }
}

/// `vte::Perform` shim that pokes into a borrowed [`OscState`].
struct Performer<'a> {
    state: &'a mut OscState,
}

impl<'a> vte::Perform for Performer<'a> {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }
        match params[0] {
            // OSC 7 — current working directory. Conventional payload:
            //   `file://<host>/<path>` (most shells)
            //   `<path>` (rarer; some emit a bare absolute path)
            b"7" if params.len() >= 2 => {
                let payload = match std::str::from_utf8(params[1]) {
                    Ok(s) => s,
                    Err(_) => return, // bytes weren't UTF-8; ignore quietly
                };
                if let Some(path) = parse_osc7_cwd(payload) {
                    self.state.cwd = Some(path);
                }
            }
            _ => {}
        }
    }
}

/// Decode an OSC 7 payload into a `PathBuf`.
///
/// Accepts:
/// - `file://hostname/abs/path`  → strips `file://` + hostname.
/// - `file:///abs/path`          → strips `file://`.
/// - `/abs/path`                 → returns as-is.
///
/// Percent-encoded bytes (`%20` etc.) are decoded so paths with
/// spaces / Unicode round-trip correctly. Returns `None` only when
/// the payload is empty after decoding.
pub fn parse_osc7_cwd(s: &str) -> Option<PathBuf> {
    let s = s.strip_prefix("file://").unwrap_or(s);
    // If the prefix had `file://`, the hostname portion runs up to
    // the first `/`. If there's no `file://` prefix at all, we
    // already point at the path.
    let path = match s.find('/') {
        Some(idx) => &s[idx..],
        None => s,
    };
    let decoded = percent_decode_lossy(path);
    if decoded.is_empty() { None } else { Some(PathBuf::from(decoded)) }
}

/// Decode `%XX` triples to their raw byte values; pass non-encoded
/// bytes through. Invalid `%XX` sequences are kept verbatim rather
/// than dropped — better to display a slightly-mangled path than to
/// silently lose data.
fn percent_decode_lossy(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! OSC 7 is the only sequence the sniffer recognises today; these
    //! tests pin the parse rules so Phase 3 marker work can extend
    //! the dispatch without breaking what's there.

    use super::*;

    #[test]
    fn parse_osc7_strips_file_scheme_and_hostname() {
        let p = parse_osc7_cwd("file://myhost.local/Users/tim/code").unwrap();
        assert_eq!(p, PathBuf::from("/Users/tim/code"));
    }

    #[test]
    fn parse_osc7_strips_file_scheme_with_empty_hostname() {
        let p = parse_osc7_cwd("file:///Users/tim/code").unwrap();
        assert_eq!(p, PathBuf::from("/Users/tim/code"));
    }

    #[test]
    fn parse_osc7_accepts_bare_absolute_path() {
        let p = parse_osc7_cwd("/Users/tim/code").unwrap();
        assert_eq!(p, PathBuf::from("/Users/tim/code"));
    }

    #[test]
    fn parse_osc7_percent_decodes_spaces() {
        let p = parse_osc7_cwd("file:///Users/tim/A%20space").unwrap();
        assert_eq!(p, PathBuf::from("/Users/tim/A space"));
    }

    #[test]
    fn parse_osc7_percent_decodes_unicode() {
        // U+00E9 (é) is `%C3%A9` in URL encoding.
        let p = parse_osc7_cwd("file:///Users/tim/caf%C3%A9").unwrap();
        assert_eq!(p, PathBuf::from("/Users/tim/café"));
    }

    #[test]
    fn parse_osc7_returns_none_on_empty_payload() {
        assert!(parse_osc7_cwd("").is_none());
        // After stripping file:// + empty hostname there's nothing
        // left — same outcome.
        assert!(parse_osc7_cwd("file://").is_none());
    }

    #[test]
    fn parse_osc7_keeps_invalid_percent_escapes_verbatim() {
        // `%ZZ` isn't valid hex; the bytes should pass through.
        let p = parse_osc7_cwd("file:///oops%ZZ").unwrap();
        assert_eq!(p, PathBuf::from("/oops%ZZ"));
    }

    #[test]
    fn sniffer_picks_up_osc7_via_full_byte_stream() {
        // Synthesise a complete OSC 7 sequence terminated by BEL.
        let mut sniffer = OscSniffer::new();
        sniffer.feed(b"\x1b]7;file:///tmp/seen\x07");
        assert_eq!(sniffer.state().cwd, Some(PathBuf::from("/tmp/seen")));
    }

    #[test]
    fn sniffer_picks_up_osc7_terminated_by_st() {
        // OSC sequences can also be terminated by ESC \ (ST). vte
        // handles both.
        let mut sniffer = OscSniffer::new();
        sniffer.feed(b"\x1b]7;file:///tmp/st-terminated\x1b\\");
        assert_eq!(sniffer.state().cwd, Some(PathBuf::from("/tmp/st-terminated")));
    }

    #[test]
    fn sniffer_handles_osc7_split_across_feed_calls() {
        // Byte-by-byte feed must produce the same final state as one
        // big slice. This is the canonical "marker bytes split across
        // PTY reads" regression test, scoped to OSC 7.
        let bytes = b"\x1b]7;file:///tmp/split\x07";
        let mut sniffer = OscSniffer::new();
        for b in bytes {
            sniffer.feed_byte(*b);
        }
        assert_eq!(sniffer.state().cwd, Some(PathBuf::from("/tmp/split")));
    }

    #[test]
    fn sniffer_ignores_unrelated_osc_codes() {
        let mut sniffer = OscSniffer::new();
        // OSC 0 / OSC 2 are window-title; OSC 4 is palette. None
        // should populate `cwd`.
        sniffer.feed(b"\x1b]0;some title\x07");
        sniffer.feed(b"\x1b]2;another title\x07");
        sniffer.feed(b"\x1b]4;1;rgb:ff/00/00\x07");
        assert!(sniffer.state().cwd.is_none());
    }

    #[test]
    fn later_osc7_overwrites_earlier() {
        let mut sniffer = OscSniffer::new();
        sniffer.feed(b"\x1b]7;file:///one\x07");
        sniffer.feed(b"\x1b]7;file:///two\x07");
        assert_eq!(sniffer.state().cwd, Some(PathBuf::from("/two")));
    }
}
