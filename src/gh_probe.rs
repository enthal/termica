//! Async per-pane GitHub PR probe (4G-async-context): the open PR for
//! the pane's branch + its rolled-up CI status, for the block-header PR
//! chip.
//!
//! Mirrors [`crate::git_probe`] — a background worker fed the pane's cwd,
//! shelling out (here to `gh pr view`) off the UI thread and shipping a
//! parsed [`PrContext`] back over a channel — with one addition: because
//! CI status changes over time on an unchanged branch, the worker
//! **self-re-polls while CI is pending** (and only then), so a chip
//! watching CI go yellow → green updates without any UI involvement.
//! Settled (passing / failing / none) results don't self-poll, so an
//! idle branch costs nothing.
//!
//! Triggering (from the pane): a probe is requested when the cwd changes
//! or the branch changes (a `git checkout` keeps cwd but swaps the PR).
//! `gh` is slow and networked, so we deliberately do **not** re-probe on
//! every command finish. JSON parsing + CI roll-up are pure in
//! [`crate::pr_context`].

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::pr_context::{CiStatus, PrContext, parse_pr_view};

/// How long the worker waits for a new request before self-re-polling a
/// branch whose CI is still pending. Short enough to feel live, long
/// enough not to hammer the GitHub API.
const PENDING_POLL_INTERVAL: Duration = Duration::from_secs(20);

/// A completed PR probe: the cwd it ran for and the parsed context —
/// `None` when the branch has no open PR (or `gh` is unavailable /
/// unauthenticated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhProbeResult {
    pub cwd: PathBuf,
    pub pr: Option<PrContext>,
}

/// Runs the `gh` query for a directory. Behind a trait so the worker is
/// testable with a fake that never spawns a process.
pub trait GhRunner: Send {
    /// Probe `cwd`. `None` means no open PR for the branch (or `gh`
    /// unavailable).
    fn probe(&self, cwd: &Path) -> Option<PrContext>;
}

/// Args for `gh pr view` restricted to the JSON fields the parser reads.
pub fn pr_view_args() -> [&'static str; 4] {
    ["pr", "view", "--json", "number,state,statusCheckRollup"]
}

/// The production [`GhRunner`]: shells out to `gh pr view` for the
/// branch checked out in `cwd`.
struct CommandGhRunner;

impl GhRunner for CommandGhRunner {
    fn probe(&self, cwd: &Path) -> Option<PrContext> {
        let out = Command::new("gh")
            .args(pr_view_args())
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        // Non-zero exit: no PR for the branch, not a GitHub repo, or `gh`
        // not authenticated — all "no chip".
        if !out.status.success() {
            return None;
        }
        parse_pr_view(&String::from_utf8_lossy(&out.stdout))
    }
}

/// A per-pane GitHub PR probe: a background worker plus the channels to
/// drive it and read its results. See the [module docs](self).
pub struct GhProbe {
    request_tx: mpsc::Sender<PathBuf>,
    result_rx: mpsc::Receiver<GhProbeResult>,
    /// The most recent cwd we asked the worker to probe — suppresses
    /// duplicate requests for an unchanged directory.
    last_requested: Option<PathBuf>,
    /// Held so the worker thread's lifetime is tied to this struct. Not
    /// joined on drop (the worker exits when the request `Sender` drops).
    _worker: JoinHandle<()>,
}

impl GhProbe {
    /// Spawn a probe backed by the real `gh`-spawning runner.
    pub fn spawn() -> Self {
        Self::spawn_with_runner(Box::new(CommandGhRunner))
    }

    /// Spawn a probe with an injected runner (the test seam).
    fn spawn_with_runner(runner: Box<dyn GhRunner>) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<PathBuf>();
        let (result_tx, result_rx) = mpsc::channel::<GhProbeResult>();
        let worker = thread::spawn(move || run_worker(request_rx, result_tx, runner));
        Self { request_tx, result_rx, last_requested: None, _worker: worker }
    }

    /// Ask the worker to probe `cwd`. Suppressed when `cwd` equals the
    /// last requested directory and `force` is false; pass `force = true`
    /// when the branch changed (a `git checkout` in the same cwd swaps
    /// the PR).
    pub fn request(&mut self, cwd: &Path, force: bool) {
        if !should_request(self.last_requested.as_deref(), cwd, force) {
            return;
        }
        self.last_requested = Some(cwd.to_path_buf());
        let _ = self.request_tx.send(cwd.to_path_buf());
    }

    /// Drain the result channel and return the freshest completed probe,
    /// if any. Older queued results are discarded.
    pub fn poll(&self) -> Option<GhProbeResult> {
        let mut latest = None;
        while let Ok(result) = self.result_rx.try_recv() {
            latest = Some(result);
        }
        latest
    }
}

/// Pure decision behind [`GhProbe::request`]: send when forced, or when
/// `cwd` differs from the last requested directory.
fn should_request(last_requested: Option<&Path>, cwd: &Path, force: bool) -> bool {
    force || last_requested != Some(cwd)
}

/// The worker loop. Blocks on the request channel, but once it has
/// produced a *pending*-CI result it instead waits only up to
/// [`PENDING_POLL_INTERVAL`] for a new request before re-polling that
/// same cwd — so a watched CI run refreshes itself. Coalesces request
/// bursts to the most recent cwd. Exits when the request `Sender` drops
/// (teardown) or the result `Receiver` is gone.
fn run_worker(
    request_rx: mpsc::Receiver<PathBuf>,
    result_tx: mpsc::Sender<GhProbeResult>,
    runner: Box<dyn GhRunner>,
) {
    // The cwd to self-re-poll while its CI is pending; `None` when the
    // last result was settled (so we block indefinitely for a request).
    // `next_cwd` returns `None` once the request `Sender` drops (teardown).
    let mut pending_cwd: Option<PathBuf> = None;
    while let Some(mut cwd) = next_cwd(&request_rx, pending_cwd.take()) {
        // Coalesce a burst down to the newest queued cwd.
        while let Ok(newer) = request_rx.try_recv() {
            cwd = newer;
        }
        let pr = runner.probe(&cwd);
        // Self-re-poll only while CI is actively pending.
        pending_cwd = match &pr {
            Some(p) if p.ci == CiStatus::Pending => Some(cwd.clone()),
            _ => None,
        };
        if result_tx.send(GhProbeResult { cwd, pr }).is_err() {
            break; // pane dropped.
        }
    }
}

/// Get the next cwd to probe: if we have a `pending` cwd to re-poll, wait
/// only up to [`PENDING_POLL_INTERVAL`] for a fresh request (falling back
/// to the pending cwd on timeout); otherwise block until a request
/// arrives. `None` signals the request channel is disconnected.
fn next_cwd(request_rx: &mpsc::Receiver<PathBuf>, pending: Option<PathBuf>) -> Option<PathBuf> {
    match pending {
        Some(p) => match request_rx.recv_timeout(PENDING_POLL_INTERVAL) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => Some(p),
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
        },
        None => request_rx.recv().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_view_args_are_stable() {
        assert_eq!(pr_view_args(), ["pr", "view", "--json", "number,state,statusCheckRollup"]);
    }

    #[test]
    fn should_request_dedups_unchanged_cwd_unless_forced() {
        let a = Path::new("/repo/a");
        let b = Path::new("/repo/b");
        assert!(should_request(None, a, false));
        assert!(!should_request(Some(a), a, false));
        assert!(should_request(Some(a), a, true)); // branch changed → force
        assert!(should_request(Some(a), b, false));
    }

    struct FakeRunner {
        pr: Option<PrContext>,
    }
    impl GhRunner for FakeRunner {
        fn probe(&self, _cwd: &Path) -> Option<PrContext> {
            self.pr
        }
    }

    #[test]
    fn worker_runs_runner_and_returns_result() {
        let pr = PrContext { number: 124, ci: CiStatus::Passing };
        let mut probe = GhProbe::spawn_with_runner(Box::new(FakeRunner { pr: Some(pr) }));
        probe.request(Path::new("/repo"), false);
        // A settled (Passing) result → exactly one send, no self-poll, so
        // this blocking recv is deterministic.
        let result = probe.result_rx.recv().expect("worker should send a result");
        assert_eq!(result.cwd, PathBuf::from("/repo"));
        assert_eq!(result.pr, Some(pr));
    }

    #[test]
    fn worker_reports_none_for_no_pr() {
        let mut probe = GhProbe::spawn_with_runner(Box::new(FakeRunner { pr: None }));
        probe.request(Path::new("/repo"), false);
        let result = probe.result_rx.recv().expect("worker should send a result");
        assert_eq!(result.pr, None);
    }
}
