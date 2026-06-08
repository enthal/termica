//! CLI-native completion drivers — [spec/04a §"Source 1"](../../../spec/04a-completion.md).
//!
//! Slice 2 of the completion engine. Modern CLIs expose a
//! "complete this command line" endpoint independent of any shell
//! (`kubectl __complete`, `gh __complete`, `docker __complete`,
//! `aws_completer`, `git --list-cmds`); this module calls them and folds
//! their candidates into the same popup the local heuristics
//! ([`super::local`]) populate instantly.
//!
//! ## Async, like the git probe
//!
//! A driver call is a one-shot subprocess (~50–150 ms), so it must never
//! run on the UI thread. Each pane owns a [`CompletionDriverEngine`]: a
//! background worker fed [`DriverRequest`]s over a channel, shipping
//! parsed [`DriverResponse`]s back. The structure mirrors
//! [`crate::git_probe::GitProbe`] (worker thread + `mpsc` + a
//! [`DriverRunner`] trait test seam), with one addition: the worker
//! holds a cloned [`egui::Context`] and calls `request_repaint()` the
//! instant a result lands, so streamed candidates appear without waiting
//! on the idle repaint cadence.
//!
//! - **Coalesced**: a burst of keystrokes collapses — the worker takes
//!   only the newest queued request before each subprocess, and
//!   [`CompletionDriverEngine::request`] dedups an unchanged
//!   `(tool, line)`. Together these bound concurrency to ~one subprocess
//!   in flight without any timer.
//! - **Superseded responses dropped**: [`CompletionDriverEngine::poll`]
//!   keeps only the response matching the newest in-flight request id.
//! - **Detection is implicit**: there is no separate probe for "is
//!   kubectl installed". A request for an absent tool simply fails to
//!   spawn on the worker and yields zero candidates — silent, off the UI
//!   thread, and self-correcting (the local sources still populate the
//!   popup).
//! - **Cancellable on teardown**: dropping the engine drops the request
//!   `Sender`; the worker's `recv` returns `Err` and the thread exits.
//!
//! Result caching ([spec/04a §"Caching"](../../../spec/04a-completion.md#caching))
//! is a deliberate fast-follow in its own PR — it is an optimization that
//! needs an injectable clock, and dedup + coalescing already prevent a
//! subprocess fork-bomb while typing.

#![forbid(unsafe_code)]

pub mod parse;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::CompletionCandidate;
use parse::{aws_completer_env, cobra_complete_args, parse_for};

/// Hard wall-clock cap on a single driver subprocess. A tool that hangs
/// (no cluster reachable, a credential prompt) is killed and yields no
/// candidates — the popup keeps its instant local rows.
const DRIVER_TIMEOUT: Duration = Duration::from_millis(250);

/// Poll granularity while waiting for a driver subprocess to exit.
const DRIVER_POLL: Duration = Duration::from_millis(5);

/// Which CLI-native tool a request targets. `Copy` so it rides inside
/// [`super::CompletionSource::Driver`] without disturbing that enum's
/// `Copy + Eq + Hash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverTool {
    Kubectl,
    Gh,
    Docker,
    Aws,
    Git,
}

impl DriverTool {
    /// Short source tag shown on the right edge of a popup row.
    pub fn tag(self) -> &'static str {
        match self {
            DriverTool::Kubectl => "k8s",
            DriverTool::Gh => "gh",
            DriverTool::Docker => "docker",
            DriverTool::Aws => "aws",
            DriverTool::Git => "git",
        }
    }
}

/// Correlates a request with its response so the engine can discard
/// stale responses from superseded requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DriverRequestId(u64);

/// A completion request handed to the worker.
#[derive(Debug, Clone)]
pub struct DriverRequest {
    pub id: DriverRequestId,
    pub tool: DriverTool,
    pub cwd: PathBuf,
    /// The command segment up to the cursor (binary + args).
    pub line: String,
    /// Cursor byte offset within `line` (for `aws_completer`).
    pub point: usize,
}

/// A worker's parsed result for one request.
#[derive(Debug, Clone)]
pub struct DriverResponse {
    pub id: DriverRequestId,
    pub tool: DriverTool,
    pub candidates: Vec<CompletionCandidate>,
}

/// Spawns driver subprocesses and returns their raw stdout. Behind a
/// trait so the engine round-trip is testable with a fake that never
/// forks (mirrors [`crate::git_probe::GitRunner`]). `Send` because it
/// crosses into the worker thread.
pub trait DriverRunner: Send {
    /// Run the driver for `req`; `None` on spawn failure (tool not
    /// installed) or timeout — both yield zero candidates silently.
    fn run(&self, req: &DriverRequest) -> Option<String>;
}

/// The production runner: actually shells out.
struct CommandDriverRunner;

impl DriverRunner for CommandDriverRunner {
    fn run(&self, req: &DriverRequest) -> Option<String> {
        let (program, args, envs) = invocation(req);
        let mut cmd = Command::new(program);
        cmd.args(&args)
            .current_dir(&req.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in &envs {
            cmd.env(key, value);
        }
        // Spawn (not `output()`) so a hung tool can be killed at the
        // deadline. Completion output is small (a few KB), well under the
        // pipe buffer, so the child never blocks on a full pipe while we
        // poll for exit.
        let mut child = cmd.spawn().ok()?;
        let deadline = Instant::now() + DRIVER_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    thread::sleep(DRIVER_POLL);
                }
                Err(_) => {
                    let _ = child.kill();
                    return None;
                }
            }
        }
        let out = child.wait_with_output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Build `(program, args, env)` for a driver request.
fn invocation(req: &DriverRequest) -> (&'static str, Vec<String>, Vec<(String, String)>) {
    match req.tool {
        DriverTool::Kubectl => ("kubectl", cobra_complete_args(&req.line), Vec::new()),
        DriverTool::Gh => ("gh", cobra_complete_args(&req.line), Vec::new()),
        DriverTool::Docker => ("docker", cobra_complete_args(&req.line), Vec::new()),
        DriverTool::Aws => {
            ("aws_completer", Vec::new(), aws_completer_env(&req.line, req.point).to_vec())
        }
        DriverTool::Git => {
            ("git", vec!["--list-cmds=builtins,others,nohelpers".to_string()], Vec::new())
        }
    }
}

/// Per-pane CLI-native driver engine: a worker thread plus the channels
/// to drive it. See the [module docs](self).
pub struct CompletionDriverEngine {
    request_tx: mpsc::Sender<DriverRequest>,
    result_rx: mpsc::Receiver<DriverResponse>,
    /// Newest request id; responses with an older id (superseded by a
    /// later keystroke) are discarded on [`Self::poll`].
    inflight: Option<DriverRequestId>,
    next_id: u64,
    /// Held so the worker's lifetime is tied to this struct. Not joined
    /// on drop (same rationale as the git probe); the worker exits when
    /// the request `Sender` drops.
    _worker: JoinHandle<()>,
}

impl CompletionDriverEngine {
    /// Spawn an engine backed by the real subprocess-spawning runner.
    /// `ctx` is cloned into the worker so it can wake the UI when a
    /// result lands.
    pub fn spawn(ctx: egui::Context) -> Self {
        Self::spawn_with_runner(ctx, Box::new(CommandDriverRunner))
    }

    /// Spawn with an injected runner (the test seam).
    fn spawn_with_runner(ctx: egui::Context, runner: Box<dyn DriverRunner>) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<DriverRequest>();
        let (result_tx, result_rx) = mpsc::channel::<DriverResponse>();
        let worker = thread::spawn(move || run_worker(request_rx, result_tx, runner, ctx));
        Self { request_tx, result_rx, inflight: None, next_id: 0, _worker: worker }
    }

    /// Fire a driver request for `(tool, line, point)` in `cwd`. Every call
    /// sends and bumps the in-flight id; the renderer only calls this on a
    /// fresh Tab or an actual buffer change (never nav-only frames), so
    /// there are no redundant identical fires to suppress. We deliberately
    /// do NOT dedup on `(tool, line)`: a persistent dedup would suppress a
    /// re-open of the *same* command after its one response was already
    /// consumed, leaving the popup permanently un-openable until the line
    /// changed. Worker-side coalescing bounds concurrency instead.
    pub fn request(&mut self, cwd: PathBuf, target: (DriverTool, String, usize)) {
        let (tool, line, point) = target;
        let id = DriverRequestId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.inflight = Some(id);
        // A send error means the worker died; nothing to do — the popup
        // keeps its local candidates. Never a panic.
        let _ = self.request_tx.send(DriverRequest { id, tool, cwd, line, point });
    }

    /// Drain the result channel and return the freshest response for the
    /// current in-flight request, discarding stale superseded ones.
    pub fn poll(&self) -> Option<DriverResponse> {
        let mut latest = None;
        while let Ok(resp) = self.result_rx.try_recv() {
            if Some(resp.id) == self.inflight {
                latest = Some(resp);
            }
        }
        latest
    }
}

/// The worker loop. Blocks on the request channel; for each request,
/// coalesces any further queued requests down to the most recent, runs
/// the driver, parses, ships the result, and wakes the UI. Exits when
/// the request `Sender` drops (pane teardown) or the result `Receiver`
/// is gone.
fn run_worker(
    request_rx: mpsc::Receiver<DriverRequest>,
    result_tx: mpsc::Sender<DriverResponse>,
    runner: Box<dyn DriverRunner>,
    ctx: egui::Context,
) {
    while let Ok(mut req) = request_rx.recv() {
        // Coalesce a burst: only the newest queued request is worth
        // running; the rest are superseded keystrokes.
        while let Ok(newer) = request_rx.try_recv() {
            req = newer;
        }
        let candidates = match runner.run(&req) {
            Some(stdout) => parse_for(req.tool, &stdout, &req.line),
            None => Vec::new(),
        };
        let resp = DriverResponse { id: req.id, tool: req.tool, candidates };
        if result_tx.send(resp).is_err() {
            break; // pane dropped — stop working.
        }
        // Wake the UI so the streamed candidates merge into the open
        // popup on the next frame instead of waiting for an idle repaint.
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::CompletionSource;

    /// A fake runner returning canned stdout (or `None` to simulate a
    /// missing tool / timeout), so the worker round-trip is deterministic
    /// — no processes, no clocks.
    struct FakeRunner {
        stdout: Option<String>,
    }
    impl DriverRunner for FakeRunner {
        fn run(&self, _req: &DriverRequest) -> Option<String> {
            self.stdout.clone()
        }
    }

    fn engine(stdout: Option<String>) -> CompletionDriverEngine {
        CompletionDriverEngine::spawn_with_runner(
            egui::Context::default(),
            Box::new(FakeRunner { stdout }),
        )
    }

    #[test]
    fn worker_parses_runner_stdout_into_candidates() {
        let mut e = engine(Some("checkout\tCheck out\n:4\n".to_string()));
        e.request(PathBuf::from("/repo"), (DriverTool::Gh, "gh pr ".to_string(), 6));
        // Blocking recv: the worker produces exactly one result for the
        // single request, so there's no flake.
        let resp = e.result_rx.recv().expect("worker should send a response");
        assert_eq!(resp.tool, DriverTool::Gh);
        assert_eq!(resp.candidates.len(), 1);
        assert_eq!(resp.candidates[0].value, "checkout");
        assert_eq!(resp.candidates[0].source, CompletionSource::Driver(DriverTool::Gh));
    }

    #[test]
    fn worker_yields_empty_on_runner_failure() {
        let mut e = engine(None);
        e.request(PathBuf::from("/r"), (DriverTool::Kubectl, "kubectl get ".to_string(), 12));
        let resp = e.result_rx.recv().expect("worker should send a response");
        assert!(resp.candidates.is_empty(), "missing tool → no candidates, no panic");
    }

    #[test]
    fn repeated_identical_request_still_fires_a_fresh_response() {
        // Re-opening the SAME command must re-fire (no persistent dedup),
        // otherwise the popup becomes permanently un-openable after the
        // first response is consumed. Each request bumps the in-flight id
        // and produces its own response.
        let mut e = engine(Some("checkout\t\n".to_string()));
        let target = (DriverTool::Git, "git che".to_string(), 7);
        e.request(PathBuf::from("/r"), target.clone());
        let first = e.inflight;
        let r1 = e.result_rx.recv().expect("first response");
        e.request(PathBuf::from("/r"), target); // identical line, second open
        assert_ne!(e.inflight, first, "a repeat request still bumps the in-flight id");
        let r2 = e.result_rx.recv().expect("second response");
        assert_ne!(r1.id, r2.id, "the repeat produced a distinct, fresh response");
    }
}
