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
//!   only the newest queued request before each subprocess. The renderer
//!   only fires on a fresh Tab or an actual buffer change, so this bounds
//!   concurrency to ~one subprocess in flight without any timer.
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
//! - **Cached**: parsed results are kept in a 10 s TTL [`cache`] keyed by
//!   `(tool, cwd, line)`, so a re-open of the same command within the
//!   window is served synchronously by [`CompletionDriverEngine::request`]
//!   with no subprocess. TTL is measured against an injected [`Clock`]
//!   (monotonic ms) so the logic is deterministic in tests.

#![forbid(unsafe_code)]

pub mod cache;
pub mod parse;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::CompletionCandidate;
use cache::{DRIVER_CACHE_TTL_MS, DriverCacheKey, DriverResultCache};
use parse::{aws_completer_env, cobra_complete_args, parse_for};

/// Hard wall-clock cap on a single driver subprocess. A tool that hangs
/// (no cluster reachable, a credential prompt) is killed and yields no
/// candidates — the popup keeps its instant local rows. Set at 2 s so a
/// real-but-slow endpoint (e.g. `kubectl --context <remote> get` round-
/// tripping to a cold cluster) still completes; the popup shows a
/// "searching…" spinner during the wait so the delay is legible.
const DRIVER_TIMEOUT: Duration = Duration::from_millis(2000);

/// Poll granularity while waiting for a driver subprocess to exit.
const DRIVER_POLL: Duration = Duration::from_millis(5);

/// Monotonic-millisecond clock behind the result cache's TTL. Injected so
/// the engine's cache behavior is deterministic in tests — production uses
/// [`SystemClock`]; tests use a fake that advances on demand. Only the
/// *difference* between readings matters, never the absolute value.
pub trait Clock: Send {
    fn now_ms(&self) -> u64;
}

/// Production [`Clock`]: elapsed milliseconds since the engine was spawned.
struct SystemClock {
    base: Instant,
}

impl SystemClock {
    fn new() -> Self {
        Self { base: Instant::now() }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.base.elapsed().as_millis() as u64
    }
}

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
    /// The fish **shell sidecar**: `fish -c 'complete -C <line>'`. Unlike
    /// the other variants this isn't a single CLI tool but the user's
    /// whole shell — it completes any command fish knows (built-ins,
    /// installed completions, the user's aliases and `complete`
    /// functions), so in a fish pane it supersedes the per-tool drivers.
    /// It rides the same engine: a one-shot subprocess whose stdout is
    /// the same `value\tdescription` per line as cobra's `__complete`
    /// ([spec/04a §"Source 2"](../../../spec/04a-completion.md)).
    FishComplete,
    /// The zsh **live-shell** completion source. Unlike [`Self::FishComplete`]
    /// there is NO one-shot subprocess form (a fresh `zsh -i -c` would
    /// re-source the user's dotfiles on every Tab — unacceptably slow). It
    /// exists ONLY as a live-shell tool: the pane's warm completion child
    /// answers a PTY request and replies with a `completion` marker, parsed
    /// the same way as fish's. So it never reaches the one-shot engine's
    /// `invocation` / worker; those arms are defensive no-ops. v1 emits
    /// **values only** (zsh descriptions are config-gated and fragile) — see
    /// [spec/04a §"Zsh sidecar"](../../../spec/04a-completion.md).
    ZshComplete,
    /// The bash **live-shell** completion source. Like [`Self::ZshComplete`]
    /// there is NO one-shot form; it exists only as a live-shell tool, and
    /// its reply rides the same `completion` marker parsed the same way. The
    /// capture is simpler than zsh's — bash completion functions just fill
    /// `COMPREPLY` and need no real readline context, so the managed bash
    /// completes **in-process** (like fish's `complete -C`), no captive
    /// child. v1 emits **values only** — see
    /// [spec/04a §"Bash sidecar"](../../../spec/04a-completion.md).
    BashComplete,
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
            DriverTool::FishComplete => "fish",
            DriverTool::ZshComplete => "zsh",
            DriverTool::BashComplete => "bash",
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
    /// `true` when this response was served from the result cache rather
    /// than a fresh subprocess. Diagnostic only (drives `TERMICA_DUMP_EVENTS`).
    pub from_cache: bool,
}

impl DriverResponse {
    /// Build a response for a result that did NOT pass through the engine's
    /// worker thread — the fish **live-shell** completion path, where
    /// `PaneSession` correlates the reply marker itself and hands the
    /// candidates straight to the popup. There's no engine request to echo,
    /// so a placeholder id is used; this never re-enters the engine's
    /// `poll` id-filter. See [spec/04a §"Fish sidecar"](../../../spec/04a-completion.md).
    pub fn live(tool: DriverTool, candidates: Vec<CompletionCandidate>) -> Self {
        DriverResponse { id: DriverRequestId(0), tool, candidates, from_cache: false }
    }
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
        // `fish -c 'complete -C $argv[1]' <line>`: fish completes the
        // command line passed as `$argv[1]` and prints `value\tdescription`
        // per line. We pass the line as a positional arg (not interpolated
        // into the script) so embedded quotes/spaces can't break out. No
        // `--no-config`: we WANT the user's config (their aliases and
        // `complete` definitions are the whole point of the sidecar).
        DriverTool::FishComplete => (
            "fish",
            vec!["-c".to_string(), "complete -C $argv[1]".to_string(), req.line.clone()],
            Vec::new(),
        ),
        // Defensive only: `ZshComplete` is a live-shell tool with no one-shot
        // form, so routing never sends it to the worker (it goes to the pane's
        // PTY request path instead). If it somehow arrives here — a zsh pane
        // whose integration isn't confirmed — `false` yields no stdout, hence
        // zero candidates, gracefully (shell integration is the only source of
        // truth for live completion; without it, none).
        DriverTool::ZshComplete => ("false", Vec::new(), Vec::new()),
        // Defensive only, same as `ZshComplete`: bash live completion runs
        // in the managed shell and replies via the PTY marker path, never
        // the worker. `false` → no stdout → zero candidates if it ever lands.
        DriverTool::BashComplete => ("false", Vec::new(), Vec::new()),
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
    /// The cache key of the in-flight (cache-miss) request, so [`Self::poll`]
    /// can store the worker's response under it.
    inflight_key: Option<DriverCacheKey>,
    /// A response synthesized from a cache hit, returned by the next
    /// [`Self::poll`] without ever touching the worker.
    ready: Option<DriverResponse>,
    next_id: u64,
    /// 10 s TTL result cache so a re-open of the same command is instant
    /// instead of re-spawning the subprocess ([spec/04a §Caching]).
    cache: DriverResultCache,
    clock: Box<dyn Clock>,
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
        Self::spawn_with(ctx, Box::new(CommandDriverRunner), Box::new(SystemClock::new()))
    }

    /// Spawn with an injected runner and clock (the test seam).
    fn spawn_with(
        ctx: egui::Context,
        runner: Box<dyn DriverRunner>,
        clock: Box<dyn Clock>,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<DriverRequest>();
        let (result_tx, result_rx) = mpsc::channel::<DriverResponse>();
        let worker = thread::spawn(move || run_worker(request_rx, result_tx, runner, ctx));
        Self {
            request_tx,
            result_rx,
            inflight: None,
            inflight_key: None,
            ready: None,
            next_id: 0,
            cache: DriverResultCache::default(),
            clock,
            _worker: worker,
        }
    }

    /// Fire a driver request for `(tool, line, point)` in `cwd`.
    ///
    /// A fresh result for `(tool, cwd, line)` within the cache TTL is
    /// served immediately (stashed for the next [`Self::poll`]) without
    /// spawning. Otherwise the request goes to the worker and its key is
    /// remembered so `poll` can cache the response.
    ///
    /// Every miss bumps the in-flight id; the renderer only calls this on a
    /// fresh Tab or an actual buffer change (never nav-only frames), so
    /// there are no redundant identical fires to suppress. We deliberately
    /// do NOT dedup on `(tool, line)`: a persistent dedup would suppress a
    /// re-open of the *same* command after its one response was already
    /// consumed. Worker-side coalescing bounds concurrency instead.
    /// Returns `true` when the request was a **cache hit** (served without
    /// spawning), `false` on a miss that went to the worker — surfaced so
    /// the caller can record it for `TERMICA_DUMP_EVENTS`.
    pub fn request(&mut self, cwd: PathBuf, target: (DriverTool, String, usize)) -> bool {
        let (tool, line, point) = target;
        let id = DriverRequestId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.inflight = Some(id);

        let key = DriverCacheKey { tool, cwd: cwd.clone(), line: line.clone() };
        let now = self.clock.now_ms();
        if let Some(candidates) = self.cache.get(&key, now, DRIVER_CACHE_TTL_MS) {
            // Cache hit: serve immediately, skip the worker. The next
            // `poll` (same render frame) returns this and the popup opens
            // with no subprocess and no flicker.
            self.ready =
                Some(DriverResponse { id, tool, candidates: candidates.clone(), from_cache: true });
            self.inflight_key = None;
            return true;
        }
        // Miss: remember the key so `poll` can cache the worker's response.
        self.inflight_key = Some(key);
        // A send error means the worker died; nothing to do — the popup
        // keeps its local candidates. Never a panic.
        let _ = self.request_tx.send(DriverRequest { id, tool, cwd, line, point });
        false
    }

    /// Return the freshest result for the in-flight request — a cache hit
    /// stashed by [`Self::request`], or the worker's latest response
    /// (stale superseded ones discarded). A non-empty worker response is
    /// cached under the in-flight key on the way out so the next re-open is
    /// instant. Empty results are not cached: an absent tool re-fails
    /// cheaply and a transient timeout is free to retry.
    pub fn poll(&mut self) -> Option<DriverResponse> {
        if let Some(ready) = self.ready.take() {
            return Some(ready);
        }
        let mut latest = None;
        while let Ok(resp) = self.result_rx.try_recv() {
            if Some(resp.id) == self.inflight {
                latest = Some(resp);
            }
        }
        if let Some(resp) = &latest
            && !resp.candidates.is_empty()
            && let Some(key) = self.inflight_key.take()
        {
            let now = self.clock.now_ms();
            self.cache.put(key, now, resp.candidates.clone(), DRIVER_CACHE_TTL_MS);
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
        let resp = DriverResponse { id: req.id, tool: req.tool, candidates, from_cache: false };
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

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

    /// Like [`FakeRunner`] but counts invocations, so a test can prove a
    /// cache hit did NOT spawn.
    struct CountingRunner {
        stdout: Option<String>,
        calls: Arc<AtomicUsize>,
    }
    impl DriverRunner for CountingRunner {
        fn run(&self, _req: &DriverRequest) -> Option<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.stdout.clone()
        }
    }

    /// A controllable monotonic clock for deterministic TTL tests (no
    /// `Instant::now`). `set` advances it; the engine reads it via [`Clock`].
    #[derive(Clone)]
    struct FakeClock(Arc<AtomicU64>);
    impl FakeClock {
        fn new(ms: u64) -> Self {
            Self(Arc::new(AtomicU64::new(ms)))
        }
        fn set(&self, ms: u64) {
            self.0.store(ms, Ordering::Relaxed);
        }
    }
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    /// Block-poll until the worker's response lands (the fake runner is
    /// instant, so this returns after a few spins; no sleeps / clocks).
    fn poll_until(engine: &mut CompletionDriverEngine) -> DriverResponse {
        for _ in 0..1_000_000 {
            if let Some(resp) = engine.poll() {
                return resp;
            }
        }
        panic!("worker never responded");
    }

    fn engine(stdout: Option<String>) -> CompletionDriverEngine {
        CompletionDriverEngine::spawn_with(
            egui::Context::default(),
            Box::new(FakeRunner { stdout }),
            Box::new(FakeClock::new(0)),
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
    fn fish_complete_invocation_passes_line_as_positional_arg() {
        // The command line must go through `$argv[1]` (a single positional
        // arg), NOT be interpolated into the `-c` script — otherwise quotes
        // or spaces in the line could break out of the script. No
        // `--no-config`: the sidecar deliberately loads the user's config.
        let req = DriverRequest {
            id: DriverRequestId(1),
            tool: DriverTool::FishComplete,
            cwd: PathBuf::from("/repo"),
            line: "git che".to_string(),
            point: 7,
        };
        let (program, args, envs) = invocation(&req);
        assert_eq!(program, "fish");
        assert_eq!(args, vec!["-c", "complete -C $argv[1]", "git che"]);
        assert!(envs.is_empty());
        assert!(!args.iter().any(|a| a == "--no-config"), "sidecar loads user config");
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

    #[test]
    fn cache_hit_serves_without_spawning() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut e = CompletionDriverEngine::spawn_with(
            egui::Context::default(),
            Box::new(CountingRunner { stdout: Some("x\t\n".into()), calls: calls.clone() }),
            Box::new(FakeClock::new(1_000)),
        );
        // Pre-warm the cache (same-module access to the private field).
        let key =
            DriverCacheKey { tool: DriverTool::Git, cwd: "/r".into(), line: "git che".into() };
        e.cache.put(
            key,
            1_000,
            vec![CompletionCandidate::simple(
                "checkout",
                CompletionSource::Driver(DriverTool::Git),
            )],
            DRIVER_CACHE_TTL_MS,
        );
        e.request(PathBuf::from("/r"), (DriverTool::Git, "git che".into(), 7));
        let resp = e.poll().expect("cache hit is served immediately");
        assert_eq!(resp.candidates[0].value, "checkout");
        assert_eq!(calls.load(Ordering::Relaxed), 0, "no subprocess on a cache hit");
    }

    #[test]
    fn miss_caches_then_hits_then_reexpires() {
        let calls = Arc::new(AtomicUsize::new(0));
        let clock = FakeClock::new(0);
        let mut e = CompletionDriverEngine::spawn_with(
            egui::Context::default(),
            Box::new(CountingRunner { stdout: Some("checkout\t\n".into()), calls: calls.clone() }),
            Box::new(clock.clone()),
        );
        let target = (DriverTool::Git, "git che".to_string(), 7);

        // 1) Cold cache → miss → worker runs; `poll` caches the response.
        e.request(PathBuf::from("/r"), target.clone());
        assert_eq!(poll_until(&mut e).candidates[0].value, "checkout");
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // 2) Same command within TTL → cache hit, no second subprocess.
        clock.set(5_000);
        e.request(PathBuf::from("/r"), target.clone());
        assert_eq!(e.poll().expect("served from cache").candidates[0].value, "checkout");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "no second subprocess within TTL");

        // 3) Past the TTL → miss → re-spawn.
        clock.set(20_000);
        e.request(PathBuf::from("/r"), target);
        assert_eq!(poll_until(&mut e).candidates[0].value, "checkout");
        assert_eq!(calls.load(Ordering::Relaxed), 2, "re-spawned after expiry");
    }

    #[test]
    fn empty_result_is_not_cached() {
        // A missing tool (runner returns None → empty candidates) must not
        // be cached, so a later retry still attempts the subprocess.
        let calls = Arc::new(AtomicUsize::new(0));
        let mut e = CompletionDriverEngine::spawn_with(
            egui::Context::default(),
            Box::new(CountingRunner { stdout: None, calls: calls.clone() }),
            Box::new(FakeClock::new(0)),
        );
        let target = (DriverTool::Kubectl, "kubectl get ".to_string(), 12);
        e.request(PathBuf::from("/r"), target.clone());
        assert!(poll_until(&mut e).candidates.is_empty());
        e.request(PathBuf::from("/r"), target);
        assert!(poll_until(&mut e).candidates.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 2, "empty results re-fire (not cached)");
    }
}
