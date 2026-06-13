//! Tab completion engine.
//!
//! [Spec/04a](../../spec/04a-completion.md) is the full design.
//! This module implements **slice 1** of that design:
//! the MVP local-only completion engine ([Phase 4I](../../spec/10-roadmap.md)),
//! covering the three local sources (paths, `$PATH` executables,
//! command history) behind a native egui popup.
//!
//! CLI-native drivers (kubectl __complete, etc.) and shell sidecars
//! (bash / zsh / fish) land in later slices and plug in behind the
//! same popup.
//!
//! ## Module layout
//!
//! - [`local`] — pure-logic candidate generators for the three v1
//!   sources, plus the token-under-cursor helper. All testable
//!   without spawning anything or touching the filesystem (the
//!   filesystem-reading parts take a directory listing as input).
//! - [`ranking`] — score-based merge of candidates from multiple
//!   sources into one popup list. Pure logic.
//! - [`popup`] — `CompletionPopup` UI state (the candidate list,
//!   the typed-prefix, the selected index) + egui paint.
//!
//! ## Wire types
//!
//! A `CompletionCandidate` is what each source produces and what
//! the popup displays. `value` is what gets inserted into the
//! editor on accept; `display` is what the popup shows (usually
//! the same as `value`); `description`, when present, renders on
//! the right of the popup row.

#![forbid(unsafe_code)]

pub mod drivers;
pub mod local;
pub mod popup;
pub mod ranking;

pub use drivers::DriverTool;
pub use popup::CompletionPopup;

use crate::integration::ShellSpec;
use std::path::{Path, PathBuf};

/// Orchestrate the three local sources and produce a popup, or
/// `None` when there's nothing to show.
///
/// This is the entry point the renderer calls on `Tab`. Filesystem
/// I/O and `$PATH` scanning happen here (synchronously — these are
/// fast local reads at the typical scrollback / cwd scale). The
/// history lookup is passed as a closure so callers can plug in
/// their `HistoryStore` query without coupling this module to the
/// DB type.
///
/// Trigger rules per spec/04 §"Tab handling":
/// - **Path source** fires when the token is "path-shaped" (starts
///   with `~`, `/`, `./`, `../`, or contains a `/`) OR the token
///   is empty / non-empty and we're not in command position.
/// - **`$PATH` source** fires when we're in command position
///   (typing the command name, not an argument) AND the token has
///   no `/` (path tokens use the filesystem source instead).
/// - **History as a completion source is intentionally OMITTED.**
///   The earlier version surfaced full historical command lines,
///   which the user found counter-productive — `↑` / `↓` arrow
///   walk and `Ctrl+R` overlay are the right tools for "recall a
///   past command" and they do that job. Word-level history
///   completion is a possible future addition; for now Tab
///   completion is purely structural (filesystem + `$PATH`).
///
/// `history_lookup` is called to fetch recent global history when
/// the `\$PATH` source runs — its only use is RE-RANKING the
/// `\$PATH` executables by recency-of-first-word-use. History
/// doesn't contribute candidates of its own.
pub fn open_completion_at(
    editor_text: &str,
    cursor: usize,
    cwd: Option<&Path>,
    home: Option<&Path>,
    env_var_names: &[String],
    history_lookup: impl FnOnce() -> Vec<String>,
) -> Option<CompletionPopup> {
    let (origin, token, candidates) =
        local_candidates_at(editor_text, cursor, cwd, home, env_var_names, history_lookup);
    CompletionPopup::new(origin, token, candidates)
}

/// The local sources' contribution as raw parts: `(origin_byte, token,
/// candidates)`, with `candidates` possibly **empty**. Unlike
/// [`open_completion_at`] this never collapses an empty result to `None`,
/// because the driver-aware flow ([`plan_completion`]) needs the origin /
/// token / locals even when the locals are empty (so a driver result can
/// still open a popup — `git ch<Tab>` has no local match but a real
/// `checkout` candidate).
fn local_candidates_at(
    editor_text: &str,
    cursor: usize,
    cwd: Option<&Path>,
    home: Option<&Path>,
    env_var_names: &[String],
    history_lookup: impl FnOnce() -> Vec<String>,
) -> (usize, String, Vec<CompletionCandidate>) {
    // Quote / escape-aware: an opening quote bounds the token (so a
    // quoted filename with spaces completes), `token` is the unescaped
    // literal to match, and `quote` drives how the substituted value is
    // re-escaped so it round-trips into the same context.
    let ctx = local::completion_context(editor_text, cursor);
    let token_start = ctx.start;
    let token = ctx.prefix.as_str();
    let quote = ctx.quote;
    let pathish = local::token_is_pathish(token);
    let cmd_pos = local::is_command_position(editor_text, cursor);

    // ---- $VAR env-var name source (exclusive) ---------------------
    //
    // A `$`-prefixed token with no `/` is an environment-variable
    // reference being typed: `$` lists every var, `$HO` narrows to
    // `$HOME` / `$HOSTNAME`, … This is exclusive — a `$foo` token is
    // neither a path nor a command, so the other sources don't run.
    if let Some(var_prefix) = local::env_var_prefix(token) {
        // Source the names from the *shell's* environment (passed in by
        // the caller — what Termica spawned the child with), not the GUI
        // process's own `std::env::vars()`, which omits `TERMICA_*` and
        // anything else set only on the child.
        let cands = local::complete_env_vars(var_prefix, env_var_names);
        return (token_start, token.to_string(), cands);
    }

    let mut sources: Vec<Vec<CompletionCandidate>> = Vec::new();

    // ---- Path source ----------------------------------------------
    //
    // Fires when the token is path-shaped OR we're in argument
    // position (`ls C<Tab>` should suggest `./Cargo.toml` even
    // though `C` alone isn't pathish). For the non-pathish arg-
    // position case, prefix the accepted value with `./` so the
    // shell unambiguously reads it as a relative path.
    let want_path = pathish || !cmd_pos;
    if want_path {
        let (dir_part, file_prefix) = local::split_path_token(token);
        if let Some(entries) = read_dir_entries(dir_part, cwd, home) {
            let mut entries_cands = local::complete_path_entries(file_prefix, &entries);
            // Rewrite each candidate's value so accepting it inserts
            // the full path the user expects:
            //
            // - `src/Ca<Tab>` accepts `src/Cargo.toml` — the
            //   dir prefix the user already typed is preserved.
            // - `C<Tab>` (non-pathish, arg position) accepts
            //   `./CLAUDE.md` — `./` prefix added so the shell
            //   reads it as a path, not a command.
            // - `/etc/pas<Tab>` accepts `/etc/passwd` — leading
            //   slash kept, no extra `./`.
            // Suppressed inside an explicit quote (`"my fi<Tab>`): the
            // user already signalled "this is a filename", so a quoted
            // bare name needs no `./` disambiguation.
            let prefix_with_dot_slash =
                !pathish && !cmd_pos && dir_part.is_empty() && quote.is_none();
            if !dir_part.is_empty() {
                for c in &mut entries_cands {
                    let full = if dir_part == "/" {
                        format!("/{}", c.value)
                    } else {
                        format!("{}/{}", dir_part, c.value)
                    };
                    c.display = full.clone();
                    c.value = full;
                }
            } else if prefix_with_dot_slash {
                for c in &mut entries_cands {
                    let full = format!("./{}", c.value);
                    c.display = full.clone();
                    c.value = full;
                }
            }
            // Escape the SUBSTITUTED value for the quote context so a
            // filename with spaces / metacharacters round-trips into the
            // shell (Bug 3). The `display` set above stays the plain,
            // human-readable name — escaping is invisible in the menu.
            for c in &mut entries_cands {
                c.value = local::escape_for_context(&c.value, quote);
            }
            sources.push(entries_cands);
        }
    }

    // ---- $PATH executable source ----------------------------------
    if cmd_pos && !token.contains('/') && !token.is_empty() {
        let exes = scan_path_executables();
        let history = history_lookup();
        sources.push(local::complete_path_executables_with_history(token, &exes, &history));
    }

    // Intentionally NO history-as-lines source — see the
    // doc-comment on this function. History is used as a RANKING
    // signal for the `$PATH` source above (boosts recently-used
    // command names to the top); it doesn't contribute candidates
    // of its own.

    let merged = ranking::merge_ranked(sources, 200);
    (token_start, token.to_string(), merged)
}

/// What a `Tab` (or a live re-filter) should do, given the editor state.
/// Pure decision so the popup lifecycle is testable without the renderer
/// — the bugs this fixes lived in untested render glue ([the async-UI
/// testing note](../../CLAUDE.md)).
#[derive(Debug, Clone)]
pub enum CompletionPlan {
    /// Open the popup immediately with these local candidates (non-empty).
    /// Used when the command has no CLI-native driver.
    Open(CompletionPopup),
    /// The command is driver-eligible: fire `target` and **wait** for the
    /// result before (re)opening the popup, rather than flashing the local
    /// candidates first. `locals` is carried so the driver result can be
    /// merged against it when it lands ([`resolve_driver`]). Holds even
    /// when `locals` is empty.
    AwaitDriver {
        origin_byte: usize,
        token: String,
        locals: Vec<CompletionCandidate>,
        target: (DriverTool, String, usize),
    },
    /// Nothing to show (no driver, no local candidates).
    Closed,
}

/// The carried state of an awaited driver result (see
/// [`CompletionPlan::AwaitDriver`]). Stored on the pane while a driver
/// subprocess is in flight; [`resolve_driver`] turns it plus the driver
/// candidates into the popup when the result lands.
#[derive(Debug, Clone)]
pub struct PendingCompletion {
    pub origin_byte: usize,
    pub token: String,
    pub locals: Vec<CompletionCandidate>,
    /// `true` when this completion session was opened by an explicit `Tab`
    /// press, so a lone result may auto-accept. `false` for a live
    /// re-filter driven by typing — there the user stays in control and
    /// must press Tab / Enter to accept, even if the list narrows to one
    /// (otherwise the completion would double up the chars they're still
    /// typing).
    pub from_tab: bool,
}

/// Decide what to do for the editor state at `cursor`. A driver-eligible
/// command always yields [`CompletionPlan::AwaitDriver`] (even with no
/// local matches); otherwise the local candidates open immediately, or
/// [`CompletionPlan::Closed`] when there are none.
pub fn plan_completion(
    editor_text: &str,
    cursor: usize,
    cwd: Option<&Path>,
    home: Option<&Path>,
    env_var_names: &[String],
    shell: ShellSpec,
    history_lookup: impl FnOnce() -> Vec<String>,
) -> CompletionPlan {
    let (origin, token, locals) =
        local_candidates_at(editor_text, cursor, cwd, home, env_var_names, history_lookup);
    if let Some(target) = driver_target_for_shell(editor_text, cursor, shell) {
        return CompletionPlan::AwaitDriver { origin_byte: origin, token, locals, target };
    }
    match CompletionPopup::new(origin, token, locals) {
        Some(popup) => CompletionPlan::Open(popup),
        None => CompletionPlan::Closed,
    }
}

/// Pick the async completion target for the editor state, by shell.
///
/// In a **fish** pane, fish's `complete -C` is a superset of the
/// per-tool CLI drivers (it covers built-ins, installed completions, and
/// the user's aliases / `complete` functions), so we route *any*
/// argument-position completion to the fish sidecar and never also fire a
/// per-tool driver. Every other shell keeps the per-tool driver path,
/// which fires only for the handful of commands with a known `__complete`
/// endpoint. Command-position completion (the command name itself) stays
/// with the local `$PATH` source in both cases.
fn driver_target_for_shell(
    editor_text: &str,
    cursor: usize,
    shell: ShellSpec,
) -> Option<(DriverTool, String, usize)> {
    match shell {
        ShellSpec::Fish => {
            let (line, point) = drivers::parse::arg_segment(editor_text, cursor)?;
            Some((DriverTool::FishComplete, line, point))
        }
        ShellSpec::Zsh | ShellSpec::Bash => drivers::parse::driver_target(editor_text, cursor),
    }
}

/// Build the popup for a resolved driver result: merge the driver
/// candidates over the carried locals and rank them. `None` when the
/// merged list is empty (driver returned nothing and there were no
/// locals — e.g. the tool isn't installed and the token matched no file).
pub fn resolve_driver(
    origin_byte: usize,
    token: &str,
    locals: Vec<CompletionCandidate>,
    driver: Vec<CompletionCandidate>,
) -> Option<CompletionPopup> {
    let merged = ranking::merge_ranked(vec![locals, driver], 200);
    CompletionPopup::new(origin_byte, token, merged)
}

/// Resolve a path-token's `dir_part` into a real filesystem path
/// using `cwd` for relative paths, `home` for `~` expansion, and
/// `env_lookup` for `$VAR` expansion. Returns `None` for inputs
/// we can't interpret.
///
/// `env_lookup` is a function (not a closure capturing
/// `std::env::var_os`) so tests can substitute a synthetic env
/// without mutating the process. Production passes
/// `std::env::var_os`. The lookup operates on **termica's**
/// process environment, which inherits from the user's login
/// shell — so `$HOME`, `$PATH`, anything they exported in their
/// rc, etc. are all visible. Vars set inside the live PTY shell
/// (e.g. `export FOO=bar` typed at the prompt) won't be visible
/// because they live inside the child process; that's an
/// acceptable v1 limitation.
fn resolve_dir(dir_part: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    resolve_dir_with(dir_part, cwd, home, |name| std::env::var_os(name))
}

fn resolve_dir_with<F>(
    dir_part: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
    env_lookup: F,
) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    Some(match dir_part {
        "" | "." => cwd?.to_path_buf(),
        "/" => PathBuf::from("/"),
        "~" => home?.to_path_buf(),
        s if s.starts_with("~/") => home?.join(s.strip_prefix("~/")?),
        s if s.starts_with('/') => PathBuf::from(s),
        s if s.starts_with("./") => cwd?.join(s.strip_prefix("./").unwrap_or("")),
        s if s.starts_with("../") => cwd?.parent()?.to_path_buf().join(s.strip_prefix("../")?),
        s if s.starts_with('$') => {
            // `$VAR` or `$VAR/sub/dir`. Split into var name and
            // the rest; look the name up; join the value with
            // the rest.
            let body = s.strip_prefix('$')?;
            let (name, rest) = match body.find('/') {
                Some(idx) => (&body[..idx], &body[idx + 1..]),
                None => (body, ""),
            };
            if name.is_empty() {
                return None;
            }
            let value = env_lookup(name)?;
            let base = PathBuf::from(value);
            if rest.is_empty() { base } else { base.join(rest) }
        }
        s => cwd?.join(s),
    })
}

/// Read a directory and return `(filename, is_dir)` for each
/// entry. `None` on any I/O error so the caller skips the path
/// source gracefully (typo in path, no read permission, etc.).
fn read_dir_entries(
    dir_part: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<Vec<(String, bool)>> {
    let path = resolve_dir(dir_part, cwd, home)?;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&path).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push((name, is_dir));
    }
    Some(out)
}

/// Walk `$PATH` and return every file we find. v1 doesn't
/// validate executable bits (mode != 0o100 || st_mode & 0o111);
/// users rarely have non-executable garbage on `$PATH` and the
/// filter would add Unix-only syscalls. If this turns into noise,
/// gate it behind a config flag.
///
/// Hidden entries (`.`-prefixed) are skipped — those are rarely
/// commands anyway and would just bloat the candidate list.
fn scan_path_executables() -> Vec<String> {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let mut out = Vec::new();
    for dir in std::env::split_paths(&path_env) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                out.push(name);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The orchestrator's pure-routing logic (which sources fire
    // for which token / position) is tested via the individual
    // `local::*` predicates already. These tests cover the
    // resolve_dir / read_dir_entries seams that ARE pure of
    // their inputs.

    #[test]
    fn resolve_dir_empty_uses_cwd() {
        let cwd = PathBuf::from("/home/user");
        let home = PathBuf::from("/home/user");
        let resolved = resolve_dir("", Some(&cwd), Some(&home));
        assert_eq!(resolved.as_deref(), Some(Path::new("/home/user")));
    }

    #[test]
    fn resolve_dir_root_slash_returns_root() {
        let cwd = PathBuf::from("/home/user");
        let resolved = resolve_dir("/", Some(&cwd), None);
        assert_eq!(resolved.as_deref(), Some(Path::new("/")));
    }

    #[test]
    fn resolve_dir_absolute_keeps_path() {
        let cwd = PathBuf::from("/home/user");
        let resolved = resolve_dir("/etc", Some(&cwd), None);
        assert_eq!(resolved.as_deref(), Some(Path::new("/etc")));
    }

    #[test]
    fn resolve_dir_tilde_alone_returns_home() {
        let home = PathBuf::from("/home/user");
        let resolved = resolve_dir("~", None, Some(&home));
        assert_eq!(resolved.as_deref(), Some(Path::new("/home/user")));
    }

    #[test]
    fn resolve_dir_tilde_slash_expands_home() {
        let home = PathBuf::from("/home/user");
        let resolved = resolve_dir("~/projects", None, Some(&home));
        assert_eq!(resolved.as_deref(), Some(Path::new("/home/user/projects")));
    }

    #[test]
    fn resolve_dir_relative_joins_cwd() {
        let cwd = PathBuf::from("/home/user");
        let resolved = resolve_dir("src", Some(&cwd), None);
        assert_eq!(resolved.as_deref(), Some(Path::new("/home/user/src")));
    }

    #[test]
    fn resolve_dir_dot_slash_joins_cwd() {
        let cwd = PathBuf::from("/home/user");
        let resolved = resolve_dir("./src", Some(&cwd), None);
        assert_eq!(resolved.as_deref(), Some(Path::new("/home/user/src")));
    }

    #[test]
    fn resolve_dir_parent_with_dotdot_slashes() {
        let cwd = PathBuf::from("/home/user");
        let resolved = resolve_dir("../etc", Some(&cwd), None);
        assert_eq!(resolved.as_deref(), Some(Path::new("/home/etc")));
    }

    #[test]
    fn resolve_dir_without_cwd_or_home_returns_none() {
        // Relative path but cwd is None — can't resolve.
        let resolved = resolve_dir("src", None, None);
        assert!(resolved.is_none());
    }

    fn fake_env_lookup(name: &str) -> Option<std::ffi::OsString> {
        // Hand-curated minimal env for tests. Production calls
        // `std::env::var_os` instead.
        match name {
            "HOME" => Some("/home/user".into()),
            "TMPDIR" => Some("/tmp".into()),
            _ => None,
        }
    }

    #[test]
    fn resolve_dir_dollar_var_expands_to_lookup_value() {
        let r = resolve_dir_with("$HOME", None, None, fake_env_lookup);
        assert_eq!(r.as_deref(), Some(Path::new("/home/user")));
        let r = resolve_dir_with("$TMPDIR", None, None, fake_env_lookup);
        assert_eq!(r.as_deref(), Some(Path::new("/tmp")));
    }

    #[test]
    fn resolve_dir_dollar_var_with_subpath_joins() {
        let r = resolve_dir_with("$HOME/projects", None, None, fake_env_lookup);
        assert_eq!(r.as_deref(), Some(Path::new("/home/user/projects")));
        let r = resolve_dir_with("$HOME/projects/foo", None, None, fake_env_lookup);
        assert_eq!(r.as_deref(), Some(Path::new("/home/user/projects/foo")));
    }

    #[test]
    fn resolve_dir_dollar_var_undefined_returns_none() {
        assert!(resolve_dir_with("$NOPE", None, None, fake_env_lookup).is_none());
    }

    #[test]
    fn resolve_dir_lone_dollar_returns_none() {
        // `$` alone is not a valid var reference.
        assert!(resolve_dir_with("$", None, None, fake_env_lookup).is_none());
    }

    // ---- driver-aware planner (slice-2 lifecycle bug fixes) ---------

    #[test]
    fn plan_driver_command_with_empty_locals_awaits_driver_not_closed() {
        // `git ch` in an empty dir: no local match, but git IS a driver.
        // Pre-fix this produced no popup at all (locals empty → Closed →
        // driver never fired). Now it must AwaitDriver so the driver's
        // `checkout`/`cherry` can open the popup.
        let dir = tempfile::tempdir().unwrap();
        let plan =
            plan_completion("git ch", 6, Some(dir.path()), None, &[], ShellSpec::Zsh, Vec::new);
        match plan {
            CompletionPlan::AwaitDriver { target, locals, .. } => {
                assert_eq!(target.0, DriverTool::Git);
                assert!(locals.is_empty(), "no file in cwd starts with 'ch'");
            }
            other => panic!("expected AwaitDriver, got {other:?}"),
        }
    }

    #[test]
    fn plan_driver_command_does_not_open_locals_first() {
        // `gh ` with a file in cwd: pre-fix opened a popup of FILES
        // instantly (then flashed driver results over them). Now it must
        // AwaitDriver, carrying the files as locals to merge later —
        // never an `Open` of files.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"").unwrap();
        let plan = plan_completion("gh ", 3, Some(dir.path()), None, &[], ShellSpec::Zsh, Vec::new);
        match plan {
            CompletionPlan::AwaitDriver { target, locals, .. } => {
                assert_eq!(target.0, DriverTool::Gh);
                assert!(
                    locals.iter().any(|c| c.display.ends_with("notes.txt")),
                    "files are carried as locals, not shown as the popup"
                );
            }
            other => panic!("expected AwaitDriver (not Open-with-files), got {other:?}"),
        }
    }

    #[test]
    fn plan_non_driver_command_opens_locals_immediately() {
        // `ls ` (not a driver) with a file → open the local list now,
        // no waiting.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"").unwrap();
        let plan = plan_completion("ls ", 3, Some(dir.path()), None, &[], ShellSpec::Zsh, Vec::new);
        match plan {
            CompletionPlan::Open(popup) => {
                assert!(popup.candidates.iter().any(|c| c.display.ends_with("notes.txt")));
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn plan_non_driver_no_local_match_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_completion(
            "ls zzz_no_such",
            14,
            Some(dir.path()),
            None,
            &[],
            ShellSpec::Zsh,
            Vec::new,
        );
        assert!(matches!(plan, CompletionPlan::Closed), "no driver, no match → nothing");
    }

    #[test]
    fn plan_fish_pane_routes_any_command_to_fish_sidecar() {
        // In a fish pane, an *unknown* command in argument position must
        // AwaitDriver against the fish sidecar (`complete -C`) — not fall
        // through to a local-only popup. zsh would never fire here (no
        // per-tool driver for `frobnicate`).
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_completion(
            "frobnicate --wi",
            15,
            Some(dir.path()),
            None,
            &[],
            ShellSpec::Fish,
            Vec::new,
        );
        match plan {
            CompletionPlan::AwaitDriver { target, .. } => {
                assert_eq!(target.0, DriverTool::FishComplete);
                assert_eq!(target.1, "frobnicate --wi");
            }
            other => panic!("expected AwaitDriver(FishComplete), got {other:?}"),
        }

        // Same input in a zsh pane: no driver for `frobnicate`, so it does
        // NOT await — the routing is shell-specific.
        let zsh = plan_completion(
            "frobnicate --wi",
            15,
            Some(dir.path()),
            None,
            &[],
            ShellSpec::Zsh,
            Vec::new,
        );
        assert!(
            !matches!(zsh, CompletionPlan::AwaitDriver { .. }),
            "zsh has no driver for an unknown command; got {zsh:?}"
        );
    }

    #[test]
    fn plan_fish_pane_command_position_stays_local() {
        // Typing the command name itself in a fish pane uses the local
        // `$PATH` source, not the sidecar.
        let dir = tempfile::tempdir().unwrap();
        let plan =
            plan_completion("frobni", 6, Some(dir.path()), None, &[], ShellSpec::Fish, Vec::new);
        assert!(
            !matches!(plan, CompletionPlan::AwaitDriver { .. }),
            "command-position completion is local, got {plan:?}"
        );
    }

    #[test]
    fn resolve_driver_merges_driver_over_locals() {
        let locals = vec![CompletionCandidate::simple("notes.txt", CompletionSource::Path)];
        let driver = vec![CompletionCandidate::simple(
            "checkout",
            CompletionSource::Driver(DriverTool::Git),
        )];
        let popup = resolve_driver(4, "ch", locals, driver).expect("merged popup");
        assert_eq!(popup.candidates[0].value, "checkout", "driver ranks above locals");
    }

    #[test]
    fn resolve_driver_empty_everything_is_none() {
        assert!(resolve_driver(0, "", vec![], vec![]).is_none());
    }

    #[test]
    fn open_completion_at_no_history_no_match_returns_none() {
        // Empty buffer, command position, history empty, no
        // executables match the empty token (correctly — we don't
        // dump $PATH on a bare Tab). Result: no popup.
        let p = open_completion_at("", 0, None, None, &[], Vec::new);
        assert!(p.is_none());
    }

    #[test]
    fn env_var_completion_uses_supplied_env_not_process() {
        // Regression (PR #141 follow-up): `$VAR` completion must reflect
        // the *child shell's* environment — what Termica spawned it with,
        // including `TERMICA_*` — not the GUI process's own
        // `std::env::vars()`. The supplied names stand in for the shell
        // env; pre-fix the source was hard-wired to the process env and a
        // var present only in the child never appeared.
        let env =
            ["TERMICA_SESSION_ID".to_string(), "TERMICA_SHELL".to_string(), "PATH".to_string()];
        let text = "echo $TERMI";
        let popup = open_completion_at(text, text.len(), None, None, &env, Vec::new)
            .expect("popup for $TERMI");
        let values: Vec<&str> = popup.candidates.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"$TERMICA_SESSION_ID"), "got {values:?}");
        assert!(values.contains(&"$TERMICA_SHELL"), "got {values:?}");
        assert!(!values.contains(&"$PATH"), "PATH doesn't match the $TERMI prefix: {values:?}");
    }

    // ---- quote-aware completion (Bug 2 + Bug 3) ------------------

    #[test]
    fn open_completion_quoted_filename_with_space_is_recognized() {
        // `ls "my fi<Tab>` — the opening quote bounds the token and
        // the space inside it does NOT break it, so "my file.txt"
        // matches. Inside double quotes the space is literal, so the
        // substituted value is NOT escaped, and no `./` is added.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my file.txt"), b"").unwrap();
        std::fs::write(dir.path().join("other.txt"), b"").unwrap();
        let text = "ls \"my fi";
        let popup = open_completion_at(text, text.len(), Some(dir.path()), None, &[], Vec::new)
            .expect("quoted token should produce a popup");
        assert_eq!(popup.origin_byte, 4, "replace region starts after the opening quote");
        let cand = popup
            .candidates
            .iter()
            .find(|c| c.display == "my file.txt")
            .expect("the spacey file matches the quoted prefix");
        assert_eq!(cand.value, "my file.txt", "no escape / no ./ inside an explicit quote");
    }

    #[test]
    fn open_completion_unquoted_escaped_space_token_is_recognized() {
        // `ls my\ fi<Tab>` — the escaped space keeps the token whole;
        // matching uses the unescaped "my fi". (Pre-fix the token
        // broke at the space to just "fi" and matched nothing.)
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my file.txt"), b"").unwrap();
        let text = "ls my\\ fi";
        let popup = open_completion_at(text, text.len(), Some(dir.path()), None, &[], Vec::new)
            .expect("escaped-space token should produce a popup");
        assert!(
            popup.candidates.iter().any(|c| c.display.ends_with("my file.txt")),
            "escaped-space token matches the spacey file"
        );
    }

    #[test]
    fn open_completion_unquoted_space_filename_escapes_value_not_display() {
        // `ls my<Tab>` completing to "my file.txt": the popup menu
        // shows the plain name, but the substituted value escapes the
        // space so the shell doesn't word-split it (Bug 3).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my file.txt"), b"").unwrap();
        let text = "ls my";
        let popup = open_completion_at(text, text.len(), Some(dir.path()), None, &[], Vec::new)
            .expect("popup");
        let cand =
            popup.candidates.iter().find(|c| c.display.ends_with("my file.txt")).expect("match");
        assert!(!cand.display.contains('\\'), "menu shows the plain, human-readable name");
        assert!(
            cand.value.contains("my\\ file.txt"),
            "substituted value escapes the space: got {:?}",
            cand.value
        );
    }
}

/// One candidate in the completion popup.
///
/// `value` is the bytes inserted into the editor when this
/// candidate is accepted (replaces the typed token). `display` is
/// what the popup shows in its row (usually identical, but a
/// driver / sidecar source may want a different visible label).
/// `description` is the optional one-line annotation rendered on
/// the right edge of the row.
///
/// `source` tags the origin so the popup can show a source chip
/// and the ranker can apply per-source weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub value: String,
    pub display: String,
    pub description: Option<String>,
    pub source: CompletionSource,
}

impl CompletionCandidate {
    pub fn simple(value: impl Into<String>, source: CompletionSource) -> Self {
        let value = value.into();
        Self { display: value.clone(), value, description: None, source }
    }

    pub fn with_description(
        value: impl Into<String>,
        description: impl Into<String>,
        source: CompletionSource,
    ) -> Self {
        let value = value.into();
        Self { display: value.clone(), value, description: Some(description.into()), source }
    }
}

/// Where a [`CompletionCandidate`] came from. v1 only has the three
/// local sources; CLI-native drivers and shell sidecars add their
/// own variants in later slices ([spec/04a](../../spec/04a-completion.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionSource {
    /// A filesystem path under the user's cwd or an absolute
    /// path. Triggered when the token under the cursor looks
    /// pathish (`./`, `/`, `~/`, or contains `/`).
    Path,
    /// An executable found on `$PATH`. Triggered when the token
    /// is the first token of the buffer (the command position)
    /// and doesn't contain `/`.
    PathExecutable,
    /// A previous command from `runs` history, prefix-matching the
    /// typed token.
    History,
    /// An environment-variable name, completing a `$`-prefixed token
    /// (`$` → all vars, `$HO` → `$HOME`, `$HOSTNAME`, …).
    EnvVar,
    /// A candidate from a CLI-native completion driver — the tool's own
    /// `__complete` endpoint ([`drivers`]). Carries the tool so the popup
    /// can tag the row (`k8s` / `gh` / …) and the ranker can weight it.
    Driver(DriverTool),
}

impl CompletionSource {
    /// Short tag for the popup's right-edge source label.
    pub fn tag(self) -> &'static str {
        match self {
            CompletionSource::Path => "path",
            CompletionSource::PathExecutable => "$PATH",
            CompletionSource::History => "history",
            CompletionSource::EnvVar => "env",
            CompletionSource::Driver(tool) => tool.tag(),
        }
    }
}
