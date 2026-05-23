//! Pane session: PTY + terminal + reader thread, glued together.
//!
//! Phase 1E-a: the data pipeline. A background thread drains the PTY
//! reader; the egui update loop pulls chunks from the channel and
//! feeds them into [`crate::terminal::TerminalState`]. The actual
//! cell renderer arrives in Phase 1E-b — this PR's UI surface is a
//! plain byte counter and a monospaced dump of `screen_text()`,
//! enough to prove the pipeline is alive.
//!
//! Splitting [`PaneSession`] (owns OS resources) from [`PaneView`]
//! (plain POD) keeps the UI testable: snapshot tests construct a
//! fixed [`PaneView`] without spawning a real process.

#![forbid(unsafe_code)]

use std::io::Read;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use crate::pty::{PtyConfig, PtyError, PtySession};
use crate::terminal::TerminalState;

/// Plain snapshot of what the UI needs to render this pane. No OS
/// handles, no channels, no threads — `Clone`/`Default` for trivial
/// test construction.
#[derive(Debug, Clone, Default)]
pub struct PaneView {
    /// Total bytes received from the PTY since session start.
    pub bytes_received: u64,
    /// Whether the terminal is currently in alternate-screen mode
    /// (vim / htop / less / fzf / tmux).
    pub alt_screen: bool,
    /// Cell-grid rows the PTY is currently sized to.
    pub rows: u16,
    /// Cell-grid columns the PTY is currently sized to.
    pub cols: u16,
    /// Visible grid rendered as multi-line text. Convenient for
    /// `--dump-events`-style debugging; the real renderer paints
    /// cells directly via [`crate::render::paint_terminal`].
    pub screen_text: String,
}

/// Live session: one PTY + one [`TerminalState`] + the reader thread
/// that bridges them via an mpsc channel.
///
/// Drop closes the PTY (master + writer); the reader thread sees EOF
/// from the kernel and exits on its own. We don't currently `join`
/// the reader thread on drop — that would block the UI for the time
/// the kernel takes to deliver EOF, which we don't want on window
/// close. The thread will exit once its read returns 0.
pub struct PaneSession {
    pty: PtySession,
    terminal: TerminalState,
    bytes_rx: mpsc::Receiver<Vec<u8>>,
    bytes_received: u64,
    // Held to keep the reader thread alive for the lifetime of this
    // session; we never `take` it. (See drop notes above.)
    _reader: JoinHandle<()>,
}

impl PaneSession {
    /// Spawn a shell, attach a freshly sized [`TerminalState`], and
    /// start the background reader thread.
    pub fn spawn(rows: u16, cols: u16, config: &PtyConfig) -> Result<Self, PtyError> {
        let mut pty = PtySession::spawn(config)?;
        let terminal = TerminalState::new(rows, cols);

        let mut reader = pty.take_reader()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: PTY closed (child exited / master dropped)
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver gone — session dropped, exit cleanly
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        Ok(Self { pty, terminal, bytes_rx: rx, bytes_received: 0, _reader: handle })
    }

    /// Pull every chunk the reader thread has queued and feed it into
    /// the terminal state. Returns the number of bytes consumed this
    /// call (0 if nothing was pending). Designed to be called once
    /// per UI frame.
    pub fn drain(&mut self) -> usize {
        let mut consumed = 0usize;
        while let Ok(chunk) = self.bytes_rx.try_recv() {
            consumed += chunk.len();
            self.bytes_received += chunk.len() as u64;
            self.terminal.feed(&chunk);
        }
        consumed
    }

    /// Snapshot the parts of the session the UI cares about, without
    /// borrowing anything OS-owned. Cheap to call once per frame.
    pub fn view(&self) -> PaneView {
        use alacritty_terminal::grid::Dimensions;
        let grid = self.terminal.grid();
        PaneView {
            bytes_received: self.bytes_received,
            alt_screen: self.terminal.is_alternate_screen(),
            rows: grid.screen_lines() as u16,
            cols: grid.columns() as u16,
            screen_text: self.terminal.screen_text(),
        }
    }

    /// Write bytes to the PTY (e.g. keyboard input). Passed through
    /// to [`PtySession::write`] verbatim.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        self.pty.write(bytes)
    }

    /// Resize the PTY and adjust the terminal's grid. Both must
    /// agree, so they live in a single call. The terminal state is
    /// resized in place (existing screen content is preserved); the
    /// kernel-side PTY size is updated so terminal-mode programs
    /// (vim, less, ...) see the new size on their next `read`.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.pty.resize(rows, cols)?;
        self.terminal.resize(rows, cols);
        Ok(())
    }

    /// Borrow the underlying terminal state. Useful for the future
    /// cell renderer in Phase 1E-b that wants to walk the grid.
    pub fn terminal(&self) -> &TerminalState {
        &self.terminal
    }
}

#[cfg(test)]
mod tests {
    //! Integration-style tests against a real shell. Each test owns
    //! its own session and never shares state.

    use super::*;
    use std::time::{Duration, Instant};

    fn sh_c(cmd: &str) -> PtyConfig {
        PtyConfig {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), cmd.into()],
            ..PtyConfig::default()
        }
    }

    /// Spin on `drain()` until either `predicate(&PaneView)` is true
    /// or the deadline expires. Returns the final view either way.
    fn wait_for<F: Fn(&PaneView) -> bool>(
        session: &mut PaneSession,
        deadline: Duration,
        predicate: F,
    ) -> PaneView {
        let stop_at = Instant::now() + deadline;
        loop {
            session.drain();
            let view = session.view();
            if predicate(&view) {
                return view;
            }
            if Instant::now() >= stop_at {
                return view;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn pipeline_routes_echo_output_into_the_terminal() {
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("printf hello-pipeline")).expect("spawn /bin/sh");
        let view = wait_for(&mut session, Duration::from_secs(2), |v| {
            v.screen_text.contains("hello-pipeline")
        });
        assert!(
            view.screen_text.contains("hello-pipeline"),
            "expected the terminal to contain the echoed text; got: {:?}\nbytes_received: {}",
            view.screen_text,
            view.bytes_received
        );
        assert!(
            view.bytes_received >= "hello-pipeline".len() as u64,
            "bytes_received seems too low: {}",
            view.bytes_received
        );
    }

    #[test]
    fn bytes_received_accumulates_across_frames() {
        let mut session = PaneSession::spawn(5, 40, &sh_c("printf one; printf two; printf three"))
            .expect("spawn");
        let view =
            wait_for(&mut session, Duration::from_secs(2), |v| v.screen_text.contains("three"));
        let expected_min = "onetwothree".len() as u64;
        assert!(
            view.bytes_received >= expected_min,
            "expected at least {} bytes, got {}\nscreen: {:?}",
            expected_min,
            view.bytes_received,
            view.screen_text
        );
    }

    #[test]
    fn write_sends_input_to_the_child() {
        // `cat` echoes its stdin back to its stdout, so writing a
        // line that we then see on the screen proves the write path.
        let mut session = PaneSession::spawn(5, 40, &sh_c("cat")).expect("spawn cat");
        session.write(b"pipeline-write-marker\n").expect("write");
        let view = wait_for(&mut session, Duration::from_secs(2), |v| {
            v.screen_text.contains("pipeline-write-marker")
        });
        assert!(
            view.screen_text.contains("pipeline-write-marker"),
            "expected cat's echo to land; got: {:?}",
            view.screen_text
        );
    }

    #[test]
    fn view_alt_screen_starts_false() {
        let session = PaneSession::spawn(5, 40, &sh_c("sleep 0.1")).expect("spawn");
        let view = session.view();
        assert!(!view.alt_screen);
    }

    #[test]
    fn drain_on_idle_session_returns_zero() {
        let mut session = PaneSession::spawn(5, 40, &sh_c("sleep 0.2")).expect("spawn");
        // sleep gives no output; first drain should see nothing.
        assert_eq!(session.drain(), 0);
        assert_eq!(session.view().bytes_received, 0);
    }

    #[test]
    fn resize_keeps_existing_output_on_screen() {
        // Spawn a shell that prints a marker and then idles, so we
        // can resize without races against the child exiting.
        let mut session =
            PaneSession::spawn(5, 40, &sh_c("printf survives-resize; sleep 1")).expect("spawn");
        let _ = wait_for(&mut session, Duration::from_secs(2), |v| {
            v.screen_text.contains("survives-resize")
        });
        // Now resize up and down. The screen text must still contain
        // the marker afterwards — that's the whole point of the
        // in-place resize over the previous "rebuild the terminal"
        // placeholder.
        session.resize(20, 100).expect("resize up");
        assert!(
            session.view().screen_text.contains("survives-resize"),
            "marker lost after resize up; screen: {:?}",
            session.view().screen_text
        );
        session.resize(8, 30).expect("resize down");
        assert!(
            session.view().screen_text.contains("survives-resize"),
            "marker lost after resize down; screen: {:?}",
            session.view().screen_text
        );
        let _ = session.write(b"\x03"); // Ctrl-C the sleep so the test ends fast
    }
}
