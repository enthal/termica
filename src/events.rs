//! `--dump-events` event recorder.
//!
//! When `TERMICA_DUMP_EVENTS=<path>` is set in the environment,
//! [`TermicaApp`](crate::TermicaApp) constructs an [`EventRecorder`]
//! that writes a human-readable record of every per-pane lifecycle
//! event and mode transition. Useful for diagnosing integration
//! failures end-to-end — `tail -f <path>` while reproducing the bug.
//!
//! Format (one record per line):
//!
//! ```text
//! [t=0.012s] pane=0 spawn shell=zsh argv=["zsh","-i"]
//! [t=0.150s] pane=0 transition Bootstrapping → RawTerminal (BootstrapComplete)
//! [t=0.151s] pane=0 lifecycle IntegrationReady { shell: zsh, version: 1 }
//! [t=2.453s] pane=0 lifecycle Precmd { cwd: "/Users/tim" }
//! [t=2.453s] pane=0 transition RawTerminal → ShellPromptEditor (PrecmdMarker)
//! ```
//!
//! Timestamps are seconds-since-recorder-start, not wall clock, so a
//! recording is comparable to itself regardless of when it ran.

#![forbid(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use crate::integration::ShellSpec;
use crate::markers::LifecycleEvent;
use crate::shell::TransitionRecord;

/// File-backed recorder for `--dump-events` diagnostics. Shared
/// across all panes (one file per Termica process). All writes
/// serialise through a [`Mutex`]; lines are flushed eagerly so a
/// crashing Termica still leaves the most recent records on disk.
pub struct EventRecorder {
    inner: Mutex<Inner>,
    started_at: Instant,
}

struct Inner {
    writer: BufWriter<File>,
}

impl EventRecorder {
    /// Open `path` for write (creating or truncating it) and return
    /// a recorder anchored at `Instant::now()`.
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
        Ok(Self {
            inner: Mutex::new(Inner { writer: BufWriter::new(file) }),
            started_at: Instant::now(),
        })
    }

    fn t_seconds(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    /// Record a pane spawn. `pane_id` is the `PaneId.0` field;
    /// `argv` is the program + flags handed to the OS.
    pub fn record_spawn(&self, pane_id: u64, shell: ShellSpec, argv: &[String]) {
        let line = format!(
            "[t={:.3}s] pane={} spawn shell={} argv={:?}\n",
            self.t_seconds(),
            pane_id,
            shell.name(),
            argv,
        );
        self.write_line(&line);
    }

    /// Record a mode transition observed by [`PromptController`].
    pub fn record_transition(&self, pane_id: u64, record: &TransitionRecord) {
        let line = format!(
            "[t={:.3}s] pane={} transition {:?} → {:?} ({:?})\n",
            self.t_seconds(),
            pane_id,
            record.from,
            record.to,
            record.reason,
        );
        self.write_line(&line);
    }

    /// Record a single [`LifecycleEvent`] consumed by the controller.
    pub fn record_lifecycle(&self, pane_id: u64, event: &LifecycleEvent) {
        let line = format!("[t={:.3}s] pane={} lifecycle {:?}\n", self.t_seconds(), pane_id, event);
        self.write_line(&line);
    }

    /// Record a PTY-exit notification.
    pub fn record_pty_exit(&self, pane_id: u64) {
        let line = format!("[t={:.3}s] pane={} pty_exit\n", self.t_seconds(), pane_id);
        self.write_line(&line);
    }

    fn write_line(&self, line: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            // Best-effort: a recorder is purely diagnostic, never
            // fatal. If the write fails we drop the record.
            let _ = inner.writer.write_all(line.as_bytes());
            let _ = inner.writer.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::ShellKind;
    use crate::shell::{PaneMode, TransitionReason};

    fn read_to_string(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read recorded file")
    }

    fn tmp_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("termica-events-{}-{}.log", std::process::id(), n));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn record_spawn_writes_a_readable_line() {
        let path = tmp_path();
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_spawn(0, ShellSpec::Zsh, &["zsh".into(), "-i".into()]);
        drop(rec); // flush
        let body = read_to_string(&path);
        assert!(body.contains("pane=0"), "no pane id in: {body:?}");
        assert!(body.contains("spawn"), "no `spawn` in: {body:?}");
        assert!(body.contains("shell=zsh"), "no shell name in: {body:?}");
        assert!(body.contains("zsh"), "no argv element in: {body:?}");
    }

    #[test]
    fn record_transition_includes_from_to_and_reason() {
        let path = tmp_path();
        let rec = EventRecorder::new(&path).expect("open recorder");
        let t = TransitionRecord {
            from: PaneMode::Bootstrapping,
            to: PaneMode::RawTerminal,
            reason: TransitionReason::BootstrapComplete,
            at: 1,
        };
        rec.record_transition(7, &t);
        drop(rec);
        let body = read_to_string(&path);
        assert!(body.contains("pane=7"), "no pane id in: {body:?}");
        assert!(body.contains("transition"), "no `transition` keyword in: {body:?}");
        assert!(body.contains("Bootstrapping"), "no `from` mode in: {body:?}");
        assert!(body.contains("RawTerminal"), "no `to` mode in: {body:?}");
        assert!(body.contains("BootstrapComplete"), "no reason in: {body:?}");
    }

    #[test]
    fn record_lifecycle_names_the_event_kind() {
        let path = tmp_path();
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_lifecycle(
            3,
            &LifecycleEvent::IntegrationReady { shell: ShellKind::Bash, version: 1 },
        );
        rec.record_lifecycle(3, &LifecycleEvent::Preexec { command: "ls -la".into() });
        rec.record_lifecycle(3, &LifecycleEvent::CommandFinished { exit: 0 });
        drop(rec);
        let body = read_to_string(&path);
        assert!(body.contains("IntegrationReady"), "missing kind: {body:?}");
        assert!(body.contains("Preexec"), "missing kind: {body:?}");
        assert!(body.contains("ls -la"), "missing payload: {body:?}");
        assert!(body.contains("CommandFinished"), "missing kind: {body:?}");
    }

    #[test]
    fn record_pty_exit_writes_a_line() {
        let path = tmp_path();
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_pty_exit(0);
        drop(rec);
        let body = read_to_string(&path);
        assert!(body.contains("pty_exit"), "missing pty_exit: {body:?}");
    }

    #[test]
    fn records_appear_in_order() {
        let path = tmp_path();
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_spawn(0, ShellSpec::Zsh, &["zsh".into()]);
        rec.record_lifecycle(
            0,
            &LifecycleEvent::IntegrationReady { shell: ShellKind::Zsh, version: 1 },
        );
        rec.record_transition(
            0,
            &TransitionRecord {
                from: PaneMode::Bootstrapping,
                to: PaneMode::RawTerminal,
                reason: TransitionReason::BootstrapComplete,
                at: 1,
            },
        );
        drop(rec);
        let body = read_to_string(&path);
        let spawn_idx = body.find("spawn").expect("spawn line");
        let lifecycle_idx = body.find("IntegrationReady").expect("lifecycle line");
        let transition_idx = body.find("transition").expect("transition line");
        assert!(spawn_idx < lifecycle_idx, "spawn should precede lifecycle");
        assert!(lifecycle_idx < transition_idx, "lifecycle should precede transition");
    }

    #[test]
    fn timestamps_are_monotonic() {
        let path = tmp_path();
        let rec = EventRecorder::new(&path).expect("open recorder");
        for i in 0..5 {
            rec.record_spawn(i, ShellSpec::Zsh, &[]);
        }
        drop(rec);
        let body = read_to_string(&path);
        let mut ts: Vec<f64> = body
            .lines()
            .filter_map(|l| l.strip_prefix("[t="))
            .filter_map(|l| l.split_once('s').map(|(t, _)| t))
            .filter_map(|t| t.parse().ok())
            .collect();
        assert_eq!(ts.len(), 5, "should have 5 timestamps; got body:\n{body}");
        let sorted = {
            ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
            ts.clone()
        };
        assert_eq!(ts, sorted, "timestamps must be monotonic");
    }
}
