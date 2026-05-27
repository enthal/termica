//! `--dump-events` event recorder.
//!
//! When `TERMICA_DUMP_EVENTS=<path>` is set in the environment,
//! [`TermicaApp`](crate::TermicaApp) constructs an [`EventRecorder`]
//! that writes a human-readable record of every per-pane lifecycle
//! event and mode transition. Useful for diagnosing integration
//! failures end-to-end — `tail -f <path>` while reproducing the bug.
//!
//! Two output formats are supported, selected by file extension:
//!
//! - `.json` or `.jsonl` → **JSON Lines** (one JSON object per line).
//!   Trivially parsable in tests, `jq`, and tools.
//! - Any other extension → **human-readable text**.
//!
//! Text example:
//!
//! ```text
//! [t=0.012s] pane=0 spawn shell=zsh argv=["zsh","-i"]
//! [t=0.150s] pane=0 transition Bootstrapping → RawTerminal (BootstrapComplete)
//! [t=0.151s] pane=0 lifecycle IntegrationReady { shell: zsh, version: 1 }
//! [t=2.453s] pane=0 lifecycle Precmd { cwd: "/Users/tim" }
//! [t=2.453s] pane=0 transition RawTerminal → ShellPromptEditor (PrecmdMarker)
//! ```
//!
//! JSON Lines example:
//!
//! ```text
//! {"t":0.012,"pane":0,"kind":"spawn","shell":"zsh","argv":["zsh","-i"]}
//! {"t":0.150,"pane":0,"kind":"transition","from":"Bootstrapping","to":"RawTerminal","reason":"BootstrapComplete"}
//! {"t":0.151,"pane":0,"kind":"lifecycle","event":"IntegrationReady","shell":"Zsh","version":1}
//! ```
//!
//! Timestamps are seconds-since-recorder-start, not wall clock, so a
//! recording is comparable to itself regardless of when it ran. The
//! full schema is normative in `spec/03-shell-integration.md`.

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
    format: Format,
}

struct Inner {
    writer: BufWriter<File>,
}

/// Output format for the recorder. Selected once at construction
/// from the file extension and fixed for the life of the recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    JsonLines,
}

impl Format {
    fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") | Some("jsonl") => Format::JsonLines,
            _ => Format::Text,
        }
    }
}

impl EventRecorder {
    /// Open `path` for write (creating or truncating it) and return
    /// a recorder anchored at `Instant::now()`. The output format is
    /// chosen from the file extension: `.json` / `.jsonl` ⇒ JSON
    /// Lines; anything else ⇒ human-readable text.
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
        Ok(Self {
            inner: Mutex::new(Inner { writer: BufWriter::new(file) }),
            started_at: Instant::now(),
            format: Format::from_path(path),
        })
    }

    fn t_seconds(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    /// Build the envelope (`t`, `pane`, `kind`) shared by every JSON
    /// record. Field order is `t` first, then `pane`, then `kind` —
    /// `preserve_order` on `serde_json` (see `Cargo.toml`) keeps that
    /// insertion order through serialisation. Per-kind fields are
    /// appended by the caller after this returns.
    fn jsonl_envelope(
        &self,
        pane_id: u64,
        kind: &str,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut obj = serde_json::Map::with_capacity(8);
        obj.insert("t".into(), serde_json::json!(round_ts(self.t_seconds())));
        obj.insert("pane".into(), serde_json::json!(pane_id));
        obj.insert("kind".into(), serde_json::json!(kind));
        obj
    }

    /// Record a pane spawn. `pane_id` is the `PaneId.0` field;
    /// `argv` is the program + flags handed to the OS.
    pub fn record_spawn(&self, pane_id: u64, shell: ShellSpec, argv: &[String]) {
        match self.format {
            Format::Text => {
                let line = format!(
                    "[t={:.3}s] pane={} spawn shell={} argv={:?}\n",
                    self.t_seconds(),
                    pane_id,
                    shell.name(),
                    argv,
                );
                self.write_line(&line);
            }
            Format::JsonLines => {
                let mut obj = self.jsonl_envelope(pane_id, "spawn");
                obj.insert("shell".into(), serde_json::json!(shell.name()));
                obj.insert("argv".into(), serde_json::json!(argv));
                self.write_jsonl(&serde_json::Value::Object(obj));
            }
        }
    }

    /// Record a mode transition observed by [`PromptController`].
    pub fn record_transition(&self, pane_id: u64, record: &TransitionRecord) {
        match self.format {
            Format::Text => {
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
            Format::JsonLines => {
                let mut obj = self.jsonl_envelope(pane_id, "transition");
                obj.insert("from".into(), serde_json::json!(format!("{:?}", record.from)));
                obj.insert("to".into(), serde_json::json!(format!("{:?}", record.to)));
                obj.insert("reason".into(), serde_json::json!(format!("{:?}", record.reason)));
                self.write_jsonl(&serde_json::Value::Object(obj));
            }
        }
    }

    /// Record a single [`LifecycleEvent`] consumed by the controller.
    pub fn record_lifecycle(&self, pane_id: u64, event: &LifecycleEvent) {
        match self.format {
            Format::Text => {
                let line = format!(
                    "[t={:.3}s] pane={} lifecycle {:?}\n",
                    self.t_seconds(),
                    pane_id,
                    event,
                );
                self.write_line(&line);
            }
            Format::JsonLines => {
                let mut obj = self.jsonl_envelope(pane_id, "lifecycle");
                lifecycle_to_json(event, &mut obj);
                self.write_jsonl(&serde_json::Value::Object(obj));
            }
        }
    }

    /// Record a PTY-exit notification.
    pub fn record_pty_exit(&self, pane_id: u64) {
        match self.format {
            Format::Text => {
                let line = format!("[t={:.3}s] pane={} pty_exit\n", self.t_seconds(), pane_id);
                self.write_line(&line);
            }
            Format::JsonLines => {
                let obj = self.jsonl_envelope(pane_id, "pty_exit");
                self.write_jsonl(&serde_json::Value::Object(obj));
            }
        }
    }

    fn write_line(&self, line: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            // Best-effort: a recorder is purely diagnostic, never
            // fatal. If the write fails we drop the record.
            let _ = inner.writer.write_all(line.as_bytes());
            let _ = inner.writer.flush();
        }
    }

    fn write_jsonl(&self, value: &serde_json::Value) {
        if let Ok(mut inner) = self.inner.lock()
            && let Ok(mut s) = serde_json::to_string(value)
        {
            s.push('\n');
            let _ = inner.writer.write_all(s.as_bytes());
            let _ = inner.writer.flush();
        }
    }
}

/// Round timestamps to millisecond precision for JSON output. Keeps
/// recorded files short and `jq`-readable while staying well above
/// the per-event jitter floor.
fn round_ts(t: f64) -> f64 {
    (t * 1000.0).round() / 1000.0
}

/// Populate a JSON object with the lifecycle event's variant tag and
/// per-variant fields, per the schema in `spec/03-shell-integration.md`.
fn lifecycle_to_json(event: &LifecycleEvent, obj: &mut serde_json::Map<String, serde_json::Value>) {
    use serde_json::json;
    match event {
        LifecycleEvent::IntegrationReady { shell, version } => {
            obj.insert("event".into(), json!("IntegrationReady"));
            obj.insert("shell".into(), json!(format!("{shell:?}")));
            obj.insert("version".into(), json!(version));
        }
        LifecycleEvent::IntegrationError { reason } => {
            obj.insert("event".into(), json!("IntegrationError"));
            obj.insert("reason".into(), json!(reason));
        }
        LifecycleEvent::Preexec { command } => {
            obj.insert("event".into(), json!("Preexec"));
            obj.insert("command".into(), json!(command));
        }
        LifecycleEvent::CommandFinished { exit } => {
            obj.insert("event".into(), json!("CommandFinished"));
            obj.insert("exit".into(), json!(exit));
        }
        LifecycleEvent::Precmd { cwd } => {
            obj.insert("event".into(), json!("Precmd"));
            obj.insert("cwd".into(), json!(cwd.to_string_lossy()));
        }
        LifecycleEvent::Cwd { cwd } => {
            obj.insert("event".into(), json!("Cwd"));
            obj.insert("cwd".into(), json!(cwd.to_string_lossy()));
        }
        LifecycleEvent::PromptVars { vars } => {
            obj.insert("event".into(), json!("PromptVars"));
            obj.insert("vars".into(), serde_json::Value::Object(vars.clone()));
        }
        LifecycleEvent::CommandAborted { reason } => {
            obj.insert("event".into(), json!("CommandAborted"));
            obj.insert("reason".into(), json!(reason));
        }
        LifecycleEvent::Continuation => {
            obj.insert("event".into(), json!("Continuation"));
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
        tmp_path_with_ext("log")
    }

    fn tmp_path_with_ext(ext: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("termica-events-{}-{}.{}", std::process::id(), n, ext));
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

    // ---- JSON Lines format ----------------------------------------

    fn parse_jsonl(body: &str) -> Vec<serde_json::Value> {
        body.lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .unwrap_or_else(|e| panic!("invalid JSON line {l:?}: {e}"))
            })
            .collect()
    }

    #[test]
    fn format_is_json_lines_when_extension_is_json() {
        let path = tmp_path_with_ext("json");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_pty_exit(0);
        drop(rec);
        let body = read_to_string(&path);
        assert!(body.trim_start().starts_with('{'), "not JSON-shaped: {body:?}");
    }

    #[test]
    fn format_is_json_lines_when_extension_is_jsonl() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_pty_exit(0);
        drop(rec);
        let body = read_to_string(&path);
        assert!(body.trim_start().starts_with('{'), "not JSON-shaped: {body:?}");
    }

    #[test]
    fn format_is_text_when_extension_unknown() {
        let path = tmp_path_with_ext("log");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_pty_exit(0);
        drop(rec);
        let body = read_to_string(&path);
        assert!(body.starts_with("[t="), "expected text format: {body:?}");
    }

    #[test]
    fn json_lines_spawn_record_has_required_fields() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_spawn(0, ShellSpec::Zsh, &["zsh".into(), "-i".into()]);
        drop(rec);
        let body = read_to_string(&path);
        let rows = parse_jsonl(&body);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert!(r["t"].is_number(), "missing t: {r}");
        assert_eq!(r["pane"], serde_json::json!(0));
        assert_eq!(r["kind"], "spawn");
        assert_eq!(r["shell"], "zsh");
        assert_eq!(r["argv"], serde_json::json!(["zsh", "-i"]));
    }

    #[test]
    fn json_lines_transition_record_has_from_to_reason() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_transition(
            7,
            &TransitionRecord {
                from: PaneMode::Bootstrapping,
                to: PaneMode::RawTerminal,
                reason: TransitionReason::BootstrapComplete,
                at: 1,
            },
        );
        drop(rec);
        let rows = parse_jsonl(&read_to_string(&path));
        let r = &rows[0];
        assert_eq!(r["pane"], 7);
        assert_eq!(r["kind"], "transition");
        assert_eq!(r["from"], "Bootstrapping");
        assert_eq!(r["to"], "RawTerminal");
        assert_eq!(r["reason"], "BootstrapComplete");
    }

    #[test]
    fn json_lines_lifecycle_integration_ready() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_lifecycle(
            0,
            &LifecycleEvent::IntegrationReady { shell: ShellKind::Bash, version: 1 },
        );
        drop(rec);
        let rows = parse_jsonl(&read_to_string(&path));
        let r = &rows[0];
        assert_eq!(r["kind"], "lifecycle");
        assert_eq!(r["event"], "IntegrationReady");
        assert_eq!(r["shell"], "Bash");
        assert_eq!(r["version"], 1);
    }

    #[test]
    fn json_lines_lifecycle_preexec_carries_command() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_lifecycle(0, &LifecycleEvent::Preexec { command: "ls -la".into() });
        drop(rec);
        let rows = parse_jsonl(&read_to_string(&path));
        let r = &rows[0];
        assert_eq!(r["event"], "Preexec");
        assert_eq!(r["command"], "ls -la");
    }

    #[test]
    fn json_lines_lifecycle_command_finished_carries_exit() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_lifecycle(0, &LifecycleEvent::CommandFinished { exit: 0 });
        rec.record_lifecycle(0, &LifecycleEvent::CommandFinished { exit: 127 });
        drop(rec);
        let rows = parse_jsonl(&read_to_string(&path));
        assert_eq!(rows[0]["event"], "CommandFinished");
        assert_eq!(rows[0]["exit"], 0);
        assert_eq!(rows[1]["exit"], 127);
    }

    #[test]
    fn json_lines_lifecycle_precmd_and_cwd_carry_path() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_lifecycle(0, &LifecycleEvent::Precmd { cwd: "/Users/tim".into() });
        rec.record_lifecycle(0, &LifecycleEvent::Cwd { cwd: "/tmp".into() });
        drop(rec);
        let rows = parse_jsonl(&read_to_string(&path));
        assert_eq!(rows[0]["event"], "Precmd");
        assert_eq!(rows[0]["cwd"], "/Users/tim");
        assert_eq!(rows[1]["event"], "Cwd");
        assert_eq!(rows[1]["cwd"], "/tmp");
    }

    #[test]
    fn json_lines_lifecycle_integration_error_and_command_aborted_carry_reason() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_lifecycle(0, &LifecycleEvent::IntegrationError { reason: "boom".into() });
        rec.record_lifecycle(0, &LifecycleEvent::CommandAborted { reason: "ctrl-c".into() });
        drop(rec);
        let rows = parse_jsonl(&read_to_string(&path));
        assert_eq!(rows[0]["event"], "IntegrationError");
        assert_eq!(rows[0]["reason"], "boom");
        assert_eq!(rows[1]["event"], "CommandAborted");
        assert_eq!(rows[1]["reason"], "ctrl-c");
    }

    #[test]
    fn json_lines_lifecycle_prompt_vars_carries_vars_object() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        let mut vars = serde_json::Map::new();
        vars.insert("git_branch".into(), serde_json::json!("main"));
        vars.insert("dirty".into(), serde_json::json!(true));
        rec.record_lifecycle(0, &LifecycleEvent::PromptVars { vars });
        drop(rec);
        let rows = parse_jsonl(&read_to_string(&path));
        let r = &rows[0];
        assert_eq!(r["event"], "PromptVars");
        assert_eq!(r["vars"]["git_branch"], "main");
        assert_eq!(r["vars"]["dirty"], true);
    }

    #[test]
    fn json_lines_pty_exit_is_minimal() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_pty_exit(3);
        drop(rec);
        let rows = parse_jsonl(&read_to_string(&path));
        let r = &rows[0];
        assert_eq!(r["pane"], 3);
        assert_eq!(r["kind"], "pty_exit");
        let obj = r.as_object().expect("object");
        // Only envelope fields — no per-kind extras.
        let extras: Vec<_> =
            obj.keys().filter(|k| !matches!(k.as_str(), "t" | "pane" | "kind")).collect();
        assert!(extras.is_empty(), "unexpected extras: {extras:?}");
    }

    /// Find each substring; return their byte offsets if all present.
    /// Helper for the ordering assertions below.
    fn positions_of(haystack: &str, needles: &[&str]) -> Vec<usize> {
        needles
            .iter()
            .map(|n| {
                haystack.find(n).unwrap_or_else(|| panic!("needle {n:?} missing from {haystack:?}"))
            })
            .collect()
    }

    /// Every JSON Lines record must start with `t`, `pane`, `kind` in
    /// that order, before any per-kind fields. Diagnostic readability
    /// depends on the envelope showing first — and the spec example
    /// in spec/03 fixes that order.
    #[test]
    fn json_lines_record_starts_with_t_pane_kind_in_order() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_spawn(0, ShellSpec::Zsh, &["zsh".into(), "-i".into()]);
        rec.record_transition(
            0,
            &TransitionRecord {
                from: PaneMode::Bootstrapping,
                to: PaneMode::RawTerminal,
                reason: TransitionReason::BootstrapComplete,
                at: 1,
            },
        );
        rec.record_lifecycle(
            0,
            &LifecycleEvent::IntegrationReady { shell: ShellKind::Zsh, version: 1 },
        );
        rec.record_pty_exit(0);
        drop(rec);
        let body = read_to_string(&path);
        for line in body.lines().filter(|l| !l.is_empty()) {
            let p = positions_of(line, &["\"t\":", "\"pane\":", "\"kind\":"]);
            assert!(
                p[0] < p[1] && p[1] < p[2],
                "envelope fields out of order in {line:?} (positions: {p:?})"
            );
        }
    }

    /// The envelope must precede every per-kind field. Otherwise `jq`
    /// users have to scan every line to find the envelope, and visual
    /// diffing of `--dump-events` output gets noisy.
    #[test]
    fn json_lines_envelope_precedes_per_kind_fields() {
        let path = tmp_path_with_ext("jsonl");
        let rec = EventRecorder::new(&path).expect("open recorder");
        rec.record_spawn(0, ShellSpec::Zsh, &["zsh".into()]);
        rec.record_transition(
            0,
            &TransitionRecord {
                from: PaneMode::Bootstrapping,
                to: PaneMode::RawTerminal,
                reason: TransitionReason::BootstrapComplete,
                at: 1,
            },
        );
        rec.record_lifecycle(0, &LifecycleEvent::Preexec { command: "ls".into() });
        rec.record_lifecycle(0, &LifecycleEvent::CommandFinished { exit: 0 });
        drop(rec);
        let body = read_to_string(&path);
        let lines: Vec<&str> = body.lines().filter(|l| !l.is_empty()).collect();
        // spawn: kind must precede shell + argv
        let p = positions_of(lines[0], &["\"kind\":", "\"shell\":", "\"argv\":"]);
        assert!(p[0] < p[1] && p[0] < p[2], "spawn envelope must precede per-kind fields: {p:?}");
        // transition: kind must precede from/to/reason
        let p = positions_of(lines[1], &["\"kind\":", "\"from\":", "\"to\":", "\"reason\":"]);
        assert!(
            p[0] < p[1] && p[0] < p[2] && p[0] < p[3],
            "transition envelope must precede per-kind fields: {p:?}"
        );
        // lifecycle Preexec: kind before event before command
        let p = positions_of(lines[2], &["\"kind\":", "\"event\":", "\"command\":"]);
        assert!(p[0] < p[1] && p[1] < p[2], "lifecycle preexec field order wrong: {p:?}");
        // lifecycle CommandFinished: kind before event before exit
        let p = positions_of(lines[3], &["\"kind\":", "\"event\":", "\"exit\":"]);
        assert!(p[0] < p[1] && p[1] < p[2], "lifecycle command_finished field order wrong: {p:?}");
    }

    #[test]
    fn json_lines_one_object_per_line() {
        let path = tmp_path_with_ext("jsonl");
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
        rec.record_pty_exit(0);
        drop(rec);
        let body = read_to_string(&path);
        let lines: Vec<&str> = body.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 4, "expected 4 records, got: {body}");
        for l in &lines {
            let v: serde_json::Value =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("bad line {l}: {e}"));
            assert!(v["t"].is_number());
            assert!(v["pane"].is_number());
            assert!(v["kind"].is_string());
        }
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
