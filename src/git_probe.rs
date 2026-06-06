//! Async per-pane git-context probe (4G-async-context).
//!
//! The pane never runs `git` on the UI thread. Instead each
//! [`PaneSession`](crate::pane::PaneSession) owns a [`GitProbe`]: a
//! background worker thread fed a `cwd` whenever the pane's directory
//! changes or a command finishes, that shells out to `git` and ships a
//! parsed [`GitContext`] back over a channel. The pane drains the
//! channel once per frame (non-blocking) and caches the latest result
//! for the block-header chips.
//!
//! Design notes (per [spec/01 "Do not block the UI on probes"](../spec/01-architecture.md)
//! and the [4G-async-context roadmap slice](../spec/10-roadmap.md)):
//!
//! - **Async**: the worker runs `git` off-thread; `git` can be slow on a
//!   cold cache or a huge repo.
//! - **Debounced / coalesced**: rapid `cwd` changes collapse — the worker
//!   takes only the most recent queued request before each `git` run, and
//!   [`GitProbe::request`] suppresses duplicate requests for an unchanged
//!   `cwd` (unless `force`d by a command finishing).
//! - **Cancellable on teardown**: dropping the [`GitProbe`] drops the
//!   request `Sender`; the worker's `recv` returns `Err` and the thread
//!   exits on its own. We don't `join` it (the same rationale as the PTY
//!   reader thread — never block the UI on a child's exit).
//!
//! The fiddly porcelain parsing lives in [`crate::git_context`] and is
//! unit-tested without a repo; this module is the thin spawn-and-channel
//! wrapper plus the pure [`build_context`] that folds the two `git`
//! outputs together.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use crate::git_context::{GitContext, parse_numstat, parse_status_v2};

/// A completed probe: the `cwd` it ran for, and the parsed context —
/// `None` when `cwd` is not inside a git work tree (or `git` is absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProbeResult {
    pub cwd: PathBuf,
    pub context: Option<GitContext>,
}

/// Runs the two `git` queries for a directory. Abstracted behind a trait
/// so the worker loop is testable with a fake that never spawns a
/// process. `Send` because it crosses into the worker thread.
pub trait GitRunner: Send {
    /// Probe `cwd`. `None` means "not a git repo" (or `git` unavailable).
    fn probe(&self, cwd: &Path) -> Option<GitContext>;
}

/// Args for `git status --porcelain=v2 --branch` — branch, ahead/behind,
/// and the changed-file entries. Newline-delimited (no `-z`) so the
/// [`parse_status_v2`] line parser reads it directly.
pub fn status_args() -> [&'static str; 3] {
    ["status", "--porcelain=v2", "--branch"]
}

/// Args for `git diff HEAD --numstat` — added/removed line counts across
/// tracked changes (staged + unstaged) versus the last commit. Untracked
/// files aren't in `HEAD`, so they contribute to the file count (via
/// status) but not to line counts, matching [`crate::git_context::DirtySummary`].
pub fn numstat_args() -> [&'static str; 3] {
    ["diff", "HEAD", "--numstat"]
}

/// Fold the two `git` outputs into a [`GitContext`]. Pure (no spawning),
/// so the combine logic is unit-tested without a repo. `status_ok` is
/// whether `git status` exited zero; a non-zero exit means "not a git
/// repo" and yields `None`.
pub fn build_context(status_ok: bool, status_out: &str, numstat_out: &str) -> Option<GitContext> {
    if !status_ok {
        return None;
    }
    let (added, removed) = parse_numstat(numstat_out);
    Some(parse_status_v2(status_out).with_line_counts(added, removed))
}

/// The production [`GitRunner`]: actually shells out to `git`.
struct CommandGitRunner;

impl GitRunner for CommandGitRunner {
    fn probe(&self, cwd: &Path) -> Option<GitContext> {
        // `git status` first: a non-zero exit (not a repo) short-circuits
        // to `None` without running the diff.
        let (status_ok, status_out) = run_git(cwd, &status_args())?;
        if !status_ok {
            return None;
        }
        // The numstat is best-effort: an unborn HEAD (`git diff HEAD` on a
        // repo with no commits) exits non-zero, leaving line counts at
        // zero while the file count from status still stands.
        let numstat_out = run_git(cwd, &numstat_args())
            .filter(|(ok, _)| *ok)
            .map(|(_, out)| out)
            .unwrap_or_default();
        build_context(true, &status_out, &numstat_out)
    }
}

/// Run one `git` invocation in `cwd`, capturing stdout. Returns
/// `(exit_success, stdout)`, or `None` if the process couldn't be spawned
/// at all (e.g. `git` isn't installed). stdin is nulled and stderr is
/// inherited-suppressed so a prompt for credentials can never hang the
/// worker.
fn run_git(cwd: &Path, args: &[&str]) -> Option<(bool, String)> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// A per-pane git-context probe: a background worker plus the channels to
/// drive it and read its results. See the [module docs](self).
pub struct GitProbe {
    request_tx: mpsc::Sender<PathBuf>,
    result_rx: mpsc::Receiver<GitProbeResult>,
    /// The most recent `cwd` we asked the worker to probe — used to
    /// suppress duplicate requests for an unchanged directory.
    last_requested: Option<PathBuf>,
    /// Held so the worker thread's lifetime is tied to this struct. Not
    /// joined on drop (see module docs); the worker exits when the
    /// request `Sender` drops.
    _worker: JoinHandle<()>,
}

impl GitProbe {
    /// Spawn a probe backed by the real `git`-spawning runner.
    pub fn spawn() -> Self {
        Self::spawn_with_runner(Box::new(CommandGitRunner))
    }

    /// Spawn a probe with an injected runner (the test seam).
    fn spawn_with_runner(runner: Box<dyn GitRunner>) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<PathBuf>();
        let (result_tx, result_rx) = mpsc::channel::<GitProbeResult>();
        let worker = thread::spawn(move || run_worker(request_rx, result_tx, runner));
        Self { request_tx, result_rx, last_requested: None, _worker: worker }
    }

    /// Ask the worker to probe `cwd`. Suppressed when `cwd` equals the
    /// last requested directory and `force` is false; pass `force = true`
    /// after a command finishes (the working tree may have changed even
    /// though `cwd` did not).
    pub fn request(&mut self, cwd: &Path, force: bool) {
        if !should_request(self.last_requested.as_deref(), cwd, force) {
            return;
        }
        self.last_requested = Some(cwd.to_path_buf());
        // A send error means the worker died; nothing we can do, and the
        // chips simply stop updating — never a panic.
        let _ = self.request_tx.send(cwd.to_path_buf());
    }

    /// Drain the result channel and return the freshest completed probe,
    /// if any. Stale intermediate results (a burst of `cwd` changes that
    /// each produced a result) are discarded — only the newest matters.
    pub fn poll(&self) -> Option<GitProbeResult> {
        let mut latest = None;
        while let Ok(result) = self.result_rx.try_recv() {
            latest = Some(result);
        }
        latest
    }
}

/// Pure decision behind [`GitProbe::request`]: send a probe when forced,
/// or when `cwd` differs from the last directory we requested.
fn should_request(last_requested: Option<&Path>, cwd: &Path, force: bool) -> bool {
    force || last_requested != Some(cwd)
}

/// The worker loop. Blocks on the request channel; for each request,
/// coalesces any further queued requests down to the most recent `cwd`,
/// runs the probe, and ships the result. Exits when the request `Sender`
/// drops (pane teardown) or the result `Receiver` is gone.
fn run_worker(
    request_rx: mpsc::Receiver<PathBuf>,
    result_tx: mpsc::Sender<GitProbeResult>,
    runner: Box<dyn GitRunner>,
) {
    while let Ok(mut cwd) = request_rx.recv() {
        // Coalesce a burst: if several `cwd` updates queued while we were
        // busy, only the newest is worth probing.
        while let Ok(newer) = request_rx.try_recv() {
            cwd = newer;
        }
        let context = runner.probe(&cwd);
        if result_tx.send(GitProbeResult { cwd, context }).is_err() {
            break; // pane dropped — stop working.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_context::DirtySummary;

    #[test]
    fn status_and_numstat_args_are_stable() {
        assert_eq!(status_args(), ["status", "--porcelain=v2", "--branch"]);
        assert_eq!(numstat_args(), ["diff", "HEAD", "--numstat"]);
    }

    #[test]
    fn build_context_none_when_status_failed() {
        // Non-zero `git status` exit → not a repo → None, regardless of
        // any stray stdout.
        assert_eq!(build_context(false, "# branch.head main\n", ""), None);
    }

    #[test]
    fn build_context_folds_status_and_numstat() {
        let status =
            "# branch.head main\n# branch.ab +1 -0\n? new.txt\n1 .M N... x x x a b src/a.rs\n";
        let numstat = "3\t1\tsrc/a.rs\n";
        let ctx = build_context(true, status, numstat).expect("repo");
        assert_eq!(ctx.branch.as_deref(), Some("main"));
        assert_eq!((ctx.ahead, ctx.behind), (1, 0));
        assert_eq!(ctx.dirty, DirtySummary { files_changed: 2, lines_added: 3, lines_removed: 1 });
    }

    #[test]
    fn should_request_dedups_unchanged_cwd_unless_forced() {
        let a = Path::new("/repo/a");
        let b = Path::new("/repo/b");
        // First request for a dir always fires.
        assert!(should_request(None, a, false));
        // Same dir, not forced → suppressed.
        assert!(!should_request(Some(a), a, false));
        // Same dir, forced (command finished) → fires.
        assert!(should_request(Some(a), a, true));
        // Different dir → fires.
        assert!(should_request(Some(a), b, false));
    }

    /// A fake runner that returns a canned context and records the cwds
    /// it was asked about, so the worker round-trip is deterministic
    /// (one request in → exactly one result out, no sleeps / clocks).
    struct FakeRunner {
        context: Option<GitContext>,
    }
    impl GitRunner for FakeRunner {
        fn probe(&self, _cwd: &Path) -> Option<GitContext> {
            self.context.clone()
        }
    }

    #[test]
    fn worker_runs_runner_and_returns_result() {
        let ctx = GitContext { branch: Some("main".into()), ..Default::default() };
        let mut probe =
            GitProbe::spawn_with_runner(Box::new(FakeRunner { context: Some(ctx.clone()) }));
        probe.request(Path::new("/repo"), false);
        // Blocking recv: the worker is guaranteed to produce exactly one
        // result for the single request, so there's no flake here.
        let result = probe.result_rx.recv().expect("worker should send a result");
        assert_eq!(result.cwd, PathBuf::from("/repo"));
        assert_eq!(result.context, Some(ctx));
    }

    #[test]
    fn worker_reports_none_for_non_repo() {
        let mut probe = GitProbe::spawn_with_runner(Box::new(FakeRunner { context: None }));
        probe.request(Path::new("/tmp/not-a-repo"), false);
        let result = probe.result_rx.recv().expect("worker should send a result");
        assert_eq!(result.context, None);
    }
}
