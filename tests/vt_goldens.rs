//! VT golden tests for the terminal engine.
//!
//! Phase 1E-n (Phase 1 close-out). Each scenario feeds a recorded /
//! synthesised PTY byte stream into [`termica::terminal::TerminalState`]
//! and snapshots two artefacts to `testdata/vt/<scenario>/`:
//!
//! - `grid.snap`  — the visible grid as `screen_text()` (one line per
//!   row, trailing whitespace preserved).
//! - `state.snap` — a few VT mode flags + cursor position, as
//!   key=value pairs (one per line) for easy diff readability.
//!
//! Regenerate baselines:
//!
//!     TERMICA_UPDATE_GOLDENS=1 cargo test --test vt_goldens
//!
//! Always inspect the resulting `git diff` before committing —
//! these snapshots ARE the regression net for the engine layer
//! (alacritty wrapper + parser feed). A change that silently flips
//! a snapshot is the canonical "we broke VT parsing" bug.
//!
//! ## Scenarios
//!
//! The eight scenarios match the day-one list in
//! [spec/09-testing.md](../spec/09-testing.md):
//!
//! - **bash-basic** / **zsh-basic**: prompt → command → output.
//! - **vim**: alt-screen enter, paint, exit.
//! - **less**: alt-screen + DECCKM (arrow-key mode).
//! - **htop**: dense SGR colour spans on alt-screen.
//! - **fzf**: bracketed paste (DECSET 2004) on alt-screen.
//! - **ssh**: passthrough — nothing Termica-specific.
//! - **split-reads**: same byte stream as bash-basic but each byte
//!   delivered as its own `feed()` call. The result MUST be byte-
//!   identical to bulk feed. This is the canonical regression test
//!   for the "escape sequence parsed across read boundaries" bug
//!   class — non-negotiable per the spec.
//!
//! ## Input authoring
//!
//! Inputs are hand-crafted byte streams that exercise the relevant
//! VT behaviour for each scenario, NOT recordings of real shell
//! sessions. The goal is to lock in the engine's interpretation of
//! known sequences; a future PR can replace any synthetic input
//! with a real `script(1)` capture and the test will still hold
//! (or report a snapshot mismatch documenting the difference).

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use termica::terminal::TerminalState;

// ---- runner -----------------------------------------------------

const SCENARIO_ROWS: u16 = 10;
const SCENARIO_COLS: u16 = 40;

fn goldens_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/vt")
}

fn update_mode() -> bool {
    std::env::var_os("TERMICA_UPDATE_GOLDENS").is_some()
}

/// Build a fresh [`TerminalState`], feed it `bytes` in one shot,
/// then assert the resulting grid + mode snapshot against the
/// committed baselines under `testdata/vt/<name>/`.
///
/// On `TERMICA_UPDATE_GOLDENS=1`, write the observed snapshots
/// instead of asserting.
fn assert_scenario(name: &str, bytes: &[u8]) {
    let mut term = TerminalState::new(SCENARIO_ROWS, SCENARIO_COLS);
    term.feed(bytes);
    assert_or_update_snapshot(name, &term);
}

/// Variant: feed `bytes` one byte at a time so the parser has to
/// keep state across calls. Used by the `split-reads` scenario.
fn assert_scenario_split(name: &str, bytes: &[u8]) {
    let mut term = TerminalState::new(SCENARIO_ROWS, SCENARIO_COLS);
    for b in bytes {
        term.feed(&[*b]);
    }
    assert_or_update_snapshot(name, &term);
}

fn assert_or_update_snapshot(name: &str, term: &TerminalState) {
    let dir = goldens_root().join(name);
    fs::create_dir_all(&dir).expect("create scenario dir");
    let grid_path = dir.join("grid.snap");
    let state_path = dir.join("state.snap");

    let grid = term.screen_text();
    let state = format_state(term);

    if update_mode() {
        fs::write(&grid_path, &grid).unwrap_or_else(|e| panic!("write {grid_path:?}: {e}"));
        fs::write(&state_path, &state).unwrap_or_else(|e| panic!("write {state_path:?}: {e}"));
        return;
    }

    assert_snap(&grid_path, &grid, "grid");
    assert_snap(&state_path, &state, "state");
}

/// Format the VT mode flags + cursor position into a small
/// key=value record. One line per field — diffs read like config
/// changes rather than blob diffs.
fn format_state(term: &TerminalState) -> String {
    let modes = term.modes();
    let (row, col) = term.cursor_position().unwrap_or((0, 0));
    let mut s = String::new();
    s.push_str(&format!("alt_screen={}\n", term.is_alternate_screen()));
    s.push_str(&format!("application_cursor={}\n", modes.application_cursor));
    s.push_str(&format!("bracketed_paste={}\n", modes.bracketed_paste));
    s.push_str(&format!("cursor_visible={}\n", term.is_cursor_visible()));
    s.push_str(&format!("cursor_row={row}\n"));
    s.push_str(&format!("cursor_col={col}\n"));
    s
}

fn assert_snap(path: &Path, actual: &str, label: &str) {
    let expected = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => panic!(
            "missing baseline {path:?}\n\
             Run `TERMICA_UPDATE_GOLDENS=1 cargo test --test vt_goldens` to create it,\n\
             then commit the result."
        ),
    };
    assert_eq!(
        actual, expected,
        "{label} snapshot mismatch at {path:?}\n\
         If this change is intentional, regenerate with\n\
         `TERMICA_UPDATE_GOLDENS=1 cargo test --test vt_goldens`."
    );
}

// ---- scenario byte streams -------------------------------------
//
// Each `*_bytes()` function builds the input stream for one
// scenario from byte literals. Inline so a reader can audit the
// exact VT sequences being exercised; comments call out the
// non-printables.

/// `bash-basic`: a plain bash prompt, the user types `ls`, output
/// arrives, a new prompt appears.
fn bash_basic_bytes() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"tim@host:~$ ls\r\n");
    v.extend_from_slice(b"Cargo.toml  README.md  src\r\n");
    v.extend_from_slice(b"tim@host:~$ ");
    v
}

/// `zsh-basic`: zsh's `%` prompt convention; same shape as bash.
fn zsh_basic_bytes() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"tim@host ~ % ls\r\n");
    v.extend_from_slice(b"Cargo.toml  README.md  src\r\n");
    v.extend_from_slice(b"tim@host ~ % ");
    v
}

/// `vim`: enter the alternate screen, paint a tilde column + a
/// status line. Captures the *mid-edit* state — we don't issue
/// the exit sequence so the snapshot pins the alt-screen content
/// the user actually sees while editing. The clean-exit property
/// (alt screen flag → false, main-screen content restored) is
/// covered by [`crate::terminal::TerminalState`]'s unit tests.
fn vim_bytes() -> Vec<u8> {
    let mut v = Vec::new();
    // Enter alt screen.
    v.extend_from_slice(b"\x1b[?1049h");
    // Home cursor, write tildes down the left margin (vim's empty-
    // buffer indicator) — leave the last row for the status line.
    v.extend_from_slice(b"\x1b[H");
    for _ in 0..(SCENARIO_ROWS as usize - 1) {
        v.extend_from_slice(b"~\r\n");
    }
    // Status line on the last row: reverse video + filename text.
    v.extend_from_slice(b"\x1b[7m");
    v.extend_from_slice(b"\"draft.txt\" [New File]                  ");
    v.extend_from_slice(b"\x1b[0m");
    v
}

/// `less`: alt-screen + DECCKM (`\e[?1h`) so arrow keys would
/// produce SS3 sequences. We don't actually press arrows here —
/// we just verify the mode flag flips and grid content lands.
fn less_bytes() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"\x1b[?1049h");
    v.extend_from_slice(b"\x1b[?1h"); // DECCKM on (application cursor keys)
    v.extend_from_slice(b"\x1b[H");
    v.extend_from_slice(b"line one\r\n");
    v.extend_from_slice(b"line two\r\n");
    v.extend_from_slice(b"line three\r\n");
    // less puts a `:` prompt on the bottom row.
    v.extend_from_slice(b"\x1b[10;1H:");
    v
}

/// `htop`: dense SGR-coloured output on alt-screen — a CPU bar, a
/// memory bar, and a header row. Exercises 256-colour mode and
/// inline SGR switching.
fn htop_bytes() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"\x1b[?1049h\x1b[H");
    // Header row in reverse video.
    v.extend_from_slice(b"\x1b[7m");
    v.extend_from_slice(b"  CPU  MEM  PID COMMAND               ");
    v.extend_from_slice(b"\x1b[0m\r\n");
    // CPU bar: 256-colour green block then bg.
    v.extend_from_slice(b"\x1b[38;5;46m||||||\x1b[0m\x1b[38;5;240m......\x1b[0m  CPU\r\n");
    // Memory bar.
    v.extend_from_slice(b"\x1b[38;5;33m||||||||\x1b[0m\x1b[38;5;240m....\x1b[0m  MEM\r\n");
    // A row.
    v.extend_from_slice(b"  3.2  1.5  101 some-process\r\n");
    v
}

/// `fzf`: bracketed paste on + alt-screen, then off + alt-screen
/// off. Exercises the two mode flags the input encoder cares about.
fn fzf_bytes() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"\x1b[?1049h"); // alt screen on
    v.extend_from_slice(b"\x1b[?2004h"); // bracketed paste on
    v.extend_from_slice(b"\x1b[H");
    v.extend_from_slice(b"> \r\n");
    v.extend_from_slice(b"  3/3\r\n");
    v.extend_from_slice(b"\x1b[7m> alpha\x1b[0m\r\n");
    v.extend_from_slice(b"  beta\r\n");
    v.extend_from_slice(b"  gamma\r\n");
    // Leave bracketed paste / alt screen on so the snapshot
    // captures both flags as `true`.
    v
}

/// `ssh`: just a remote prompt + a command, no alt-screen, no
/// DECCKM, no bracketed paste. Termica's engine should pass
/// everything through untouched.
fn ssh_bytes() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"Last login: Wed Jan  1 00:00:00\r\n");
    v.extend_from_slice(b"remote:~$ whoami\r\n");
    v.extend_from_slice(b"alice\r\n");
    v.extend_from_slice(b"remote:~$ ");
    v
}

// ---- tests ------------------------------------------------------

#[test]
fn vt_golden_bash_basic() {
    assert_scenario("bash-basic", &bash_basic_bytes());
}

#[test]
fn vt_golden_zsh_basic() {
    assert_scenario("zsh-basic", &zsh_basic_bytes());
}

#[test]
fn vt_golden_vim() {
    assert_scenario("vim", &vim_bytes());
}

#[test]
fn vt_golden_less() {
    assert_scenario("less", &less_bytes());
}

#[test]
fn vt_golden_htop() {
    assert_scenario("htop", &htop_bytes());
}

#[test]
fn vt_golden_fzf() {
    assert_scenario("fzf", &fzf_bytes());
}

#[test]
fn vt_golden_ssh() {
    assert_scenario("ssh", &ssh_bytes());
}

/// The canonical regression test for "escape sequence parsed
/// across read boundaries". Feeds the `bash-basic` byte stream one
/// byte at a time — the resulting snapshot MUST match the bulk-
/// feed result. If this ever diverges, the parser has a latent
/// state-bleed bug.
#[test]
fn vt_golden_split_reads() {
    assert_scenario_split("split-reads", &bash_basic_bytes());
}

/// Hard equality check between `bash-basic` (bulk feed) and
/// `split-reads` (byte-by-byte feed). The snapshot-comparison
/// tests above already cover this indirectly via the file system,
/// but this in-process check makes the invariant explicit and
/// independent of file I/O.
#[test]
fn vt_split_reads_matches_bulk_for_bash_basic() {
    let bytes = bash_basic_bytes();

    let mut bulk = TerminalState::new(SCENARIO_ROWS, SCENARIO_COLS);
    bulk.feed(&bytes);

    let mut split = TerminalState::new(SCENARIO_ROWS, SCENARIO_COLS);
    for b in &bytes {
        split.feed(&[*b]);
    }

    assert_eq!(
        bulk.screen_text(),
        split.screen_text(),
        "byte-by-byte feed produced a different grid than bulk feed — \
         this is the canonical 'escape sequence split across reads' bug"
    );
    assert_eq!(
        bulk.modes(),
        split.modes(),
        "mode flags diverged between bulk and byte-by-byte feed"
    );
    assert_eq!(
        bulk.is_alternate_screen(),
        split.is_alternate_screen(),
        "alt-screen state diverged between bulk and byte-by-byte feed"
    );
}
