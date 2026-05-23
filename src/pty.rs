//! PTY plumbing.
//!
//! Spawn a real pseudo-terminal, write bytes to it, read bytes from it,
//! resize it, observe child exit. Synchronous API; the tokio/async
//! layering arrives in Phase 1D once there's a consumer that drains
//! PTY bytes into `alacritty_terminal`.
//!
//! See [`spec/02-terminal-engine.md`](../spec/02-terminal-engine.md#pty-layer)
//! for the role this layer plays in the larger architecture.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::path::PathBuf;

use portable_pty::{
    Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem,
};

/// Description of the shell process Termica should spawn into a PTY.
///
/// Default values: `$SHELL` (or `/bin/sh`), no args, current env, 24×80
/// initial size. Resize after spawn via [`PtySession::resize`].
#[derive(Debug, Clone)]
pub struct PtyConfig {
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment variables to set on the child. Existing vars
    /// are preserved unless overridden here.
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub rows: u16,
    pub cols: u16,
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self {
            program: default_shell(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            rows: 24,
            cols: 80,
        }
    }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// Errors that can come out of the PTY layer. Wraps the
/// `portable_pty` error text (which is `anyhow`-flavored) into a flat
/// string so our public surface doesn't drag `anyhow` into the API.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    /// The PTY pair could not be opened, or the child could not be
    /// spawned. The wrapped text is `portable_pty`'s diagnostic.
    #[error("PTY operation failed: {0}")]
    Os(String),

    /// Underlying I/O error during read / write / wait.
    #[error("PTY I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// [`PtySession::take_reader`] called more than once on the same
    /// session.
    #[error("PTY reader already taken")]
    ReaderAlreadyTaken,
}

/// One live shell session running in a pseudo-terminal.
///
/// The reader is owned by `Option` so `take_reader` can move it into
/// the consumer (typically a background thread that feeds bytes into
/// `alacritty_terminal`). The writer stays on `self` so writes and
/// resizes can interleave with whatever the consumer is doing.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    reader: Option<Box<dyn Read + Send>>,
}

impl PtySession {
    /// Spawn the configured program inside a fresh PTY pair.
    pub fn spawn(config: &PtyConfig) -> Result<Self, PtyError> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: config.rows,
                cols: config.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Os(format!("openpty: {e}")))?;

        let mut cmd = CommandBuilder::new(&config.program);
        for arg in &config.args {
            cmd.arg(arg);
        }
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &config.cwd {
            cmd.cwd(cwd);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::Os(format!("spawn `{}`: {e}", config.program)))?;

        // The slave end is held by the child via dup2; we don't need
        // our copy. Dropping it here is what allows the slave fd to
        // close cleanly when the child exits.
        drop(pair.slave);

        let writer =
            pair.master.take_writer().map_err(|e| PtyError::Os(format!("take_writer: {e}")))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Os(format!("try_clone_reader: {e}")))?;
        let killer = child.clone_killer();

        Ok(Self { master: pair.master, writer, child, killer, reader: Some(reader) })
    }

    /// Move the slave-output reader out of this session. The consumer
    /// owns it from this point. Calling this twice is an error rather
    /// than a silent surprise.
    pub fn take_reader(&mut self) -> Result<Box<dyn Read + Send>, PtyError> {
        self.reader.take().ok_or(PtyError::ReaderAlreadyTaken)
    }

    /// Write bytes to the slave (i.e. into the child's stdin). Flushed
    /// before returning so keystrokes are not coalesced into a
    /// surprising backlog.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Tell the kernel the PTY is now `rows × cols` cells. Call this
    /// before delivering further input so terminal-mode programs
    /// (vim, less, ...) see the new size for the next read.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| PtyError::Os(format!("resize: {e}")))
    }

    /// Non-blocking check: if the child has exited, return its status.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, PtyError> {
        self.child.try_wait().map_err(|e| PtyError::Io(std::io::Error::other(e.to_string())))
    }

    /// Kill the child. Best-effort — if the child has already exited
    /// the platform layer may report a benign error.
    pub fn kill(&mut self) -> Result<(), PtyError> {
        self.killer.kill().map_err(|e| PtyError::Io(std::io::Error::other(e.to_string())))
    }
}

#[cfg(test)]
mod tests {
    //! Integration-style tests that exercise the real OS PTY layer.
    //!
    //! These spawn actual `/bin/sh` invocations. Each test owns its
    //! own session and never shares state — concurrent test execution
    //! is fine.
    //!
    //! macOS and Linux runners both ship `/bin/sh`; if these tests
    //! ever need to run on Windows, they must be gated on `cfg(unix)`
    //! and replaced with `cmd.exe` invocations.

    use super::*;
    use std::time::{Duration, Instant};

    /// Drain a reader to EOF or until a deadline. Returns whatever
    /// arrived. The deadline is generous so flaky scheduling on CI
    /// runners doesn't cause spurious test failures.
    fn drain_until_quiet(mut reader: Box<dyn Read + Send>, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF — child closed its tty
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        out
    }

    fn sh_c(cmd: &str) -> PtyConfig {
        PtyConfig {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), cmd.into()],
            ..PtyConfig::default()
        }
    }

    #[test]
    fn output_of_echo_reaches_the_reader() {
        let mut session = PtySession::spawn(&sh_c("printf hello-from-pty"))
            .expect("spawn /bin/sh -c 'printf hello-from-pty'");
        let reader = session.take_reader().expect("take_reader");
        let bytes = drain_until_quiet(reader, Duration::from_secs(2));
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("hello-from-pty"),
            "expected reader to contain 'hello-from-pty'; got: {s:?}"
        );
    }

    #[test]
    fn writes_round_trip_through_the_slave() {
        // `cat` echoes what it receives. We send a line and expect
        // either the echoed input (kernel line discipline) or the
        // cat's stdout echo back. Both contain the marker text.
        let mut session = PtySession::spawn(&PtyConfig {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "cat".into()],
            ..PtyConfig::default()
        })
        .expect("spawn cat");
        session.write(b"round-trip-marker\n").expect("write to PTY");

        // Give cat a moment, then kill it so the reader hits EOF.
        std::thread::sleep(Duration::from_millis(200));
        session.kill().expect("kill cat");

        let reader = session.take_reader().expect("take_reader");
        let bytes = drain_until_quiet(reader, Duration::from_secs(2));
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("round-trip-marker"),
            "expected reader to contain 'round-trip-marker'; got: {s:?}"
        );
    }

    #[test]
    fn child_exits_with_zero_status() {
        let mut session = PtySession::spawn(&sh_c("exit 0")).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(2);
        let exit = loop {
            if let Some(status) = session.try_wait().expect("try_wait") {
                break status;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for /bin/sh -c 'exit 0' to exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(exit.success(), "exit was non-zero: {exit:?}");
    }

    #[test]
    fn child_exits_with_nonzero_status() {
        let mut session = PtySession::spawn(&sh_c("exit 42")).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(2);
        let exit = loop {
            if let Some(status) = session.try_wait().expect("try_wait") {
                break status;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(!exit.success(), "expected non-zero exit; got {exit:?}");
    }

    #[test]
    fn resize_after_spawn_does_not_error() {
        let mut session = PtySession::spawn(&sh_c("sleep 0.5")).expect("spawn");
        session.resize(40, 120).expect("resize up");
        session.resize(24, 80).expect("resize down");
        // best-effort cleanup — the sleep will exit shortly anyway
        let _ = session.kill();
    }

    #[test]
    fn take_reader_twice_returns_already_taken_error() {
        let mut session = PtySession::spawn(&sh_c("sleep 0.1")).expect("spawn");
        let _first = session.take_reader().expect("first take");
        // `Result::expect_err` requires the Ok variant to be `Debug`,
        // but `Box<dyn Read + Send>` is not. Match the second call by
        // hand instead.
        match session.take_reader() {
            Ok(_) => panic!("second take_reader should have failed"),
            Err(PtyError::ReaderAlreadyTaken) => { /* expected */ }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
        let _ = session.kill();
    }
}
