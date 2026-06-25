//! VT byte-stream sniffer.
//!
//! `alacritty_terminal` already drives a full VT parser to maintain
//! the grid, but it discards sequences it doesn't surface (OSC 7
//! cwd-reporting, our DCS-JSON lifecycle messages). We need those
//! for the status header, clickable paths, and the
//! `PromptController` mode machine.
//!
//! Rather than fork or extend alacritty, we run a second thin VT
//! parser in parallel. Each byte the terminal sees is also fed to
//! our [`OscSniffer`], whose [`vte::Perform`] impl extracts the
//! sequences we care about. The sniffer never paints, never changes
//! the grid; alacritty stays the source of truth for everything
//! else.
//!
//! What we extract:
//!
//! - **OSC 7** — `OSC 7 ; <file-url> ST`. Updates the [`OscState::cwd`]
//!   snapshot consumed by the status header and clickable paths.
//!   This is a v1-era signal kept for graceful degradation (it works
//!   even before Termica's bootstrap completes, and works even when
//!   the user's `.zshrc` happens to emit OSC 7 from a foreign shell-
//!   integration script).
//!
//! - **DCS-JSON** — `ESC P Termica;<json> ESC \`. Termica's own
//!   shell-integration protocol per spec/03. We buffer the DCS body
//!   between `vte::Perform::hook` and `unhook` and hand the
//!   completed body to [`crate::markers::parse_dcs_body`].
//!
//! Foreign OSC 133 / OSC 1337 sequences are **deliberately ignored**.
//! See spec/05 safety rule 3.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::path::PathBuf;

use crate::markers::{self, LifecycleEvent};

/// Snapshot of the latest signals the [`OscSniffer`] has seen.
#[derive(Debug, Default, Clone)]
pub struct OscState {
    /// Most recent `OSC 7 ; <file-url-or-path> ST` payload, decoded
    /// to an absolute path. `None` until the shell emits one.
    pub cwd: Option<PathBuf>,
}

/// VT byte-stream peeker. Holds a small [`OscState`] and a FIFO of
/// [`LifecycleEvent`]s extracted from Termica DCS-JSON messages.
pub struct OscSniffer {
    parser: vte::Parser,
    state: OscState,
    /// FIFO of lifecycle events extracted from DCS-JSON dispatches.
    /// Drained by [`Self::drain_events`]; size is bounded only by
    /// how rarely consumers drain (in practice every frame).
    events: VecDeque<LifecycleEvent>,
    /// DCS body buffer. Accumulated between `vte::Perform::hook`
    /// (start of DCS) and `unhook` (end of DCS). Always cleared on
    /// hook so an incomplete prior sequence cannot bleed in.
    ///
    /// The DCS "final byte" (action character — `T` for our
    /// `Termica;…` framing) is consumed by vte and reported via
    /// `hook`'s `action` argument; we prepend it to the buffer
    /// in `hook` so the body the parser sees starts with `Termica;`.
    dcs_buf: Vec<u8>,
    /// `true` while we are inside a DCS sequence (between `hook` and
    /// `unhook`). Used to gate `put` byte accumulation.
    in_dcs: bool,
}

impl OscSniffer {
    /// Construct a sniffer with default state.
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            state: OscState::default(),
            events: VecDeque::new(),
            dcs_buf: Vec::new(),
            in_dcs: false,
        }
    }

    /// Feed a single byte.
    pub fn feed_byte(&mut self, byte: u8) {
        self.feed(&[byte]);
    }

    /// Feed a slice.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut perform = Performer {
            state: &mut self.state,
            events: &mut self.events,
            dcs_buf: &mut self.dcs_buf,
            in_dcs: &mut self.in_dcs,
        };
        self.parser.advance(&mut perform, bytes);
    }

    /// Borrow the current state.
    pub fn state(&self) -> &OscState {
        &self.state
    }

    /// Seed the cwd directly (no OSC 7 byte stream), used by restore to
    /// recover a Dead pane's last-known directory from its persisted
    /// `pane` row so its tab title reads the path instead of `pane N`.
    pub fn seed_cwd(&mut self, cwd: std::path::PathBuf) {
        self.state.cwd = Some(cwd);
    }

    /// Drain all queued lifecycle events. Order is preserved.
    pub fn drain_events(&mut self) -> Vec<LifecycleEvent> {
        self.events.drain(..).collect()
    }
}

impl Default for OscSniffer {
    fn default() -> Self {
        Self::new()
    }
}

/// `vte::Perform` shim that pokes into the borrowed sniffer state
/// fields.
struct Performer<'a> {
    state: &'a mut OscState,
    events: &'a mut VecDeque<LifecycleEvent>,
    dcs_buf: &'a mut Vec<u8>,
    in_dcs: &'a mut bool,
}

impl<'a> vte::Perform for Performer<'a> {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }
        // OSC 7: cwd reporting. We accept it from any source — it's
        // a state-snapshot signal for the status header / clickable
        // paths and does not participate in mode transitions.
        if params[0] == b"7" && params.len() >= 2 {
            let payload = match std::str::from_utf8(params[1]) {
                Ok(s) => s,
                Err(_) => return,
            };
            if let Some(path) = parse_osc7_cwd(payload) {
                self.state.cwd = Some(path);
            }
        }
        // All other OSC numbers (133, 1337, etc.) are deliberately
        // ignored. See spec/05 safety rule 3: Termica's bootstrap
        // is the only source of truth for prompt-state transitions,
        // and our protocol travels over DCS, not OSC.
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, action: char) {
        // Start of a DCS sequence. Reset buffer so a prior incomplete
        // sequence cannot leak in, then seed with the action character
        // (the "final byte" of the DCS introducer that vte consumes
        // before invoking `put`). For our `ESC P Termica;…` framing
        // the action is `T` — without prepending it, the body the
        // parser sees would start with `ermica;` and our prefix check
        // would silently fail. Putting it back lets `parse_dcs_body`
        // see the full `Termica;` prefix it expects.
        self.dcs_buf.clear();
        let mut buf = [0u8; 4];
        let s = action.encode_utf8(&mut buf);
        self.dcs_buf.extend_from_slice(s.as_bytes());
        *self.in_dcs = true;
    }

    fn put(&mut self, byte: u8) {
        if *self.in_dcs {
            self.dcs_buf.push(byte);
        }
    }

    fn unhook(&mut self) {
        if *self.in_dcs {
            if let Some(event) = markers::parse_dcs_body(self.dcs_buf) {
                // Lifecycle events that carry a cwd update the
                // OscState snapshot too so consumers reading
                // `state.cwd` don't have to know which channel it
                // came from.
                match &event {
                    LifecycleEvent::Cwd { cwd } | LifecycleEvent::Precmd { cwd } => {
                        self.state.cwd = Some(cwd.clone());
                    }
                    _ => {}
                }
                self.events.push_back(event);
            }
            self.dcs_buf.clear();
            *self.in_dcs = false;
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
    let path = match s.find('/') {
        Some(idx) => &s[idx..],
        None => s,
    };
    let decoded = percent_decode_lossy(path);
    if decoded.is_empty() { None } else { Some(PathBuf::from(decoded)) }
}

/// Decode `%XX` triples to their raw byte values; pass non-encoded
/// bytes through. Invalid `%XX` sequences are kept verbatim rather
/// than dropped.
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
    use super::*;
    use crate::markers::ShellKind;

    // ---- OSC 7 parser (Phase 1E-h) ----------------------------------

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
        let p = parse_osc7_cwd("file:///Users/tim/caf%C3%A9").unwrap();
        assert_eq!(p, PathBuf::from("/Users/tim/café"));
    }

    #[test]
    fn parse_osc7_returns_none_on_empty_payload() {
        assert!(parse_osc7_cwd("").is_none());
        assert!(parse_osc7_cwd("file://").is_none());
    }

    #[test]
    fn parse_osc7_keeps_invalid_percent_escapes_verbatim() {
        let p = parse_osc7_cwd("file:///oops%ZZ").unwrap();
        assert_eq!(p, PathBuf::from("/oops%ZZ"));
    }

    // ---- OSC 7 byte-stream wiring -----------------------------------

    #[test]
    fn sniffer_picks_up_osc7_via_full_byte_stream() {
        let mut sniffer = OscSniffer::new();
        sniffer.feed(b"\x1b]7;file:///tmp/seen\x07");
        assert_eq!(sniffer.state().cwd, Some(PathBuf::from("/tmp/seen")));
    }

    #[test]
    fn sniffer_picks_up_osc7_terminated_by_st() {
        let mut sniffer = OscSniffer::new();
        sniffer.feed(b"\x1b]7;file:///tmp/st-terminated\x1b\\");
        assert_eq!(sniffer.state().cwd, Some(PathBuf::from("/tmp/st-terminated")));
    }

    #[test]
    fn sniffer_handles_osc7_split_across_feed_calls() {
        // Byte-by-byte feed must produce the same final state as one
        // big slice.
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
        sniffer.feed(b"\x1b]0;some title\x07");
        sniffer.feed(b"\x1b]2;another title\x07");
        sniffer.feed(b"\x1b]4;1;rgb:ff/00/00\x07");
        assert!(sniffer.state().cwd.is_none());
        assert!(sniffer.drain_events().is_empty());
    }

    #[test]
    fn sniffer_ignores_foreign_osc_133() {
        // Safety rule 3 (spec/05): we never trust OSC 133 — that's
        // someone else's integration script and our prompt-editor
        // safety must not depend on it.
        let mut sniffer = OscSniffer::new();
        sniffer.feed(b"\x1b]133;A\x07");
        sniffer.feed(b"\x1b]133;B\x07");
        sniffer.feed(b"\x1b]133;C\x07");
        sniffer.feed(b"\x1b]133;D;0\x07");
        assert!(sniffer.drain_events().is_empty());
    }

    #[test]
    fn sniffer_ignores_foreign_osc_1337() {
        // iTerm2-private namespace; not ours.
        let mut sniffer = OscSniffer::new();
        sniffer.feed(b"\x1b]1337;ShellIntegrationVersion=12\x07");
        assert!(sniffer.drain_events().is_empty());
    }

    // ---- DCS-JSON byte-stream wiring (Phase 3C) ---------------------

    #[test]
    fn sniffer_emits_integration_ready_via_full_byte_stream() {
        let mut sniffer = OscSniffer::new();
        sniffer.feed(
            b"\x1bPTermica;{\"type\":\"integration_ready\",\"session\":\"x\",\"value\":{\"shell\":\"zsh\",\"version\":1}}\x1b\\",
        );
        assert_eq!(
            sniffer.drain_events(),
            vec![LifecycleEvent::IntegrationReady { shell: ShellKind::Zsh, version: 1 }]
        );
    }

    #[test]
    fn sniffer_emits_full_command_lifecycle_in_order() {
        // Walk a complete precmd → preexec → command_finished → precmd
        // sequence and assert event ordering.
        let mut sniffer = OscSniffer::new();
        sniffer.feed(
            b"\x1bPTermica;{\"type\":\"precmd\",\"session\":\"x\",\"value\":\"/home/u\"}\x1b\\",
        );
        sniffer
            .feed(b"\x1bPTermica;{\"type\":\"preexec\",\"session\":\"x\",\"value\":\"ls\"}\x1b\\");
        sniffer.feed(
            b"\x1bPTermica;{\"type\":\"command_finished\",\"session\":\"x\",\"value\":0}\x1b\\",
        );
        sniffer.feed(
            b"\x1bPTermica;{\"type\":\"precmd\",\"session\":\"x\",\"value\":\"/home/u\"}\x1b\\",
        );
        assert_eq!(
            sniffer.drain_events(),
            vec![
                LifecycleEvent::Precmd { cwd: PathBuf::from("/home/u") },
                LifecycleEvent::Preexec { command: "ls".into() },
                LifecycleEvent::CommandFinished { exit: 0 },
                LifecycleEvent::Precmd { cwd: PathBuf::from("/home/u") },
            ]
        );
    }

    #[test]
    fn sniffer_precmd_updates_both_event_stream_and_osc_state_cwd() {
        // Precmd carries the cwd; we mirror it into OscState so the
        // status header doesn't need two sources.
        let mut sniffer = OscSniffer::new();
        sniffer.feed(
            b"\x1bPTermica;{\"type\":\"precmd\",\"session\":\"x\",\"value\":\"/Users/tim/code\"}\x1b\\",
        );
        assert_eq!(sniffer.state().cwd, Some(PathBuf::from("/Users/tim/code")));
        assert_eq!(
            sniffer.drain_events(),
            vec![LifecycleEvent::Precmd { cwd: PathBuf::from("/Users/tim/code") }]
        );
    }

    #[test]
    fn sniffer_dcs_split_across_feed_calls() {
        // Split-read robustness — DCS sequence broken across reads
        // must still produce exactly one event.
        let bytes =
            b"\x1bPTermica;{\"type\":\"integration_ready\",\"session\":\"x\",\"value\":{\"shell\":\"zsh\",\"version\":1}}\x1b\\";
        let mut sniffer = OscSniffer::new();
        for b in bytes {
            sniffer.feed_byte(*b);
        }
        assert_eq!(
            sniffer.drain_events(),
            vec![LifecycleEvent::IntegrationReady { shell: ShellKind::Zsh, version: 1 }]
        );
    }

    #[test]
    fn sniffer_ignores_foreign_dcs_sequences() {
        // Sixel graphics, Kitty graphics, and other DCS users will
        // emit `ESC P ... ESC \` sequences without the Termica prefix.
        // We must ignore them.
        let mut sniffer = OscSniffer::new();
        sniffer.feed(b"\x1bPq#0;2;0;0;0not-termica\x1b\\");
        assert!(sniffer.drain_events().is_empty());
    }

    #[test]
    fn drain_events_yields_then_empties_the_queue() {
        let mut sniffer = OscSniffer::new();
        sniffer
            .feed(b"\x1bPTermica;{\"type\":\"precmd\",\"session\":\"x\",\"value\":\"/tmp\"}\x1b\\");
        assert_eq!(
            sniffer.drain_events(),
            vec![LifecycleEvent::Precmd { cwd: PathBuf::from("/tmp") }]
        );
        // Second drain returns nothing.
        assert!(sniffer.drain_events().is_empty());
    }

    #[test]
    fn later_osc7_overwrites_earlier() {
        let mut sniffer = OscSniffer::new();
        sniffer.feed(b"\x1b]7;file:///one\x07");
        sniffer.feed(b"\x1b]7;file:///two\x07");
        assert_eq!(sniffer.state().cwd, Some(PathBuf::from("/two")));
    }
}
