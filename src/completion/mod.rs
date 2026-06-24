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
        /// The quote context at the cursor ([`local::CompletionContext::quote`]),
        /// carried so the driver result's values can be escaped for the same
        /// context the local source already escaped for ([`resolve_driver`]).
        quote: Option<char>,
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
    /// Quote context at the cursor, threaded to [`resolve_driver`] so the
    /// driver values escape into the same context as the local candidates.
    pub quote: Option<char>,
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
        // Recompute the (pure, cheap) quote context so the driver result can
        // be escaped for the same context the local source escaped for.
        let quote = local::completion_context(editor_text, cursor).quote;
        return CompletionPlan::AwaitDriver { origin_byte: origin, token, locals, quote, target };
    }
    match CompletionPopup::new(origin, token, locals) {
        Some(popup) => CompletionPlan::Open(popup),
        None => CompletionPlan::Closed,
    }
}

/// Pick the async completion target for the editor state, by shell.
///
/// In a **fish** pane, fish's `complete -C` is a superset of the per-tool
/// CLI drivers (it covers built-ins, installed completions, and the user's
/// aliases / `complete` functions), so we route *any* completion — in both
/// command and argument position — to the fish sidecar and never also fire
/// a per-tool driver. Routing the command **name** to fish is what lets
/// aliases / functions / abbreviations complete (the local `$PATH` source
/// only knows on-disk executables); the driver result merges with the
/// local sources so a command on both collapses to one row.
///
/// A **zsh** pane keeps the per-tool cobra drivers AUTHORITATIVE for the
/// commands that have a robust `__complete` endpoint (`gh` / `git` /
/// `kubectl` / …) — those are reliable and already shipped, and we don't
/// want to route them through the comparatively fragile shell-capture. For
/// everything else — the long tail of aliases, functions, built-ins, and
/// tools with shell-installed completions — zsh routes to its live-shell
/// completion (command *and* argument position, like fish), so the user's
/// own completions work. The capture is values-only in v1.
///
/// **bash** keeps the per-tool driver path only (no sidecar yet).
fn driver_target_for_shell(
    editor_text: &str,
    cursor: usize,
    shell: ShellSpec,
) -> Option<(DriverTool, String, usize)> {
    match shell {
        ShellSpec::Fish => {
            let (line, point) = drivers::parse::fish_segment(editor_text, cursor)?;
            Some((DriverTool::FishComplete, line, point))
        }
        ShellSpec::Zsh => {
            // Cobra drivers win for the tools that have them.
            if let Some(target) = drivers::parse::driver_target(editor_text, cursor) {
                return Some(target);
            }
            // Long tail → the live zsh shell (command + argument position).
            let (line, point) = drivers::parse::fish_segment(editor_text, cursor)?;
            Some((DriverTool::ZshComplete, line, point))
        }
        ShellSpec::Bash => drivers::parse::driver_target(editor_text, cursor),
    }
}

/// Build the popup for a resolved driver result: merge the driver
/// candidates over the carried locals and rank them. `None` when the
/// merged list is empty (driver returned nothing and there were no
/// locals — e.g. the tool isn't installed and the token matched no file).
pub fn resolve_driver(
    origin_byte: usize,
    token: &str,
    quote: Option<char>,
    locals: Vec<CompletionCandidate>,
    driver: Vec<CompletionCandidate>,
) -> Option<CompletionPopup> {
    let driver = realign_driver_path_candidates(token, quote, &locals, driver);
    let merged = ranking::merge_ranked(vec![locals, driver], 200);
    CompletionPopup::new(origin_byte, token, merged)
}

/// Realign each driver/sidecar value to the whole-token convention the local
/// path source uses ([`align_driver_value_to_token`]) and, for a **path-
/// shaped** token, suppress bad sidecar path candidates so the completion
/// never extends to a path that isn't real.
///
/// The key asymmetry: the local path source is the **authoritative, complete
/// listing** of the directory under the cursor, while zsh's path completion
/// is noisy — it emits the typed path's own ancestor components (`cd /usr/`
/// → `usr`), and *alternative names for ambiguous intermediate components*
/// (`cd /usr/lib/dtrace/arm/` → `libexec`, `arm64` — from `lib`/`libexec`
/// and `arm`/`arm64`), all verified against the live captive child. Aligned,
/// those become paths that don't exist (`/usr/lib/dtrace/arm/libexec`).
///
/// So **when the local listing succeeded** (path-shaped token, `locals`
/// non-empty), every driver path candidate is dropped: a match is redundant
/// with the local row (which carries the trailing `/` and the full-path
/// display), and a non-match is junk. The local rows are exactly what we
/// want. **When there is no local listing** (the directory couldn't be read,
/// or it isn't a filesystem path at all), fall back to two cheap heuristics
/// that cull the self-evident junk: the value must extend the token, and its
/// leaf must not be one of the token's own directory components.
///
/// Every surviving value is **canonicalised then escaped for the shell
/// context** (`quote`), mirroring the local path source: zsh's capture emits
/// the SAME match both raw (`Application Support`) and pre-escaped
/// (`Application\ Support`), so unescaping to the literal collapses the two
/// (otherwise the pre-escaped form double-escapes), and re-escaping makes a
/// name with a space round-trip.
///
/// Non-path tokens (command names, subcommands, flags — no `/`) skip the path
/// suppression entirely — fuzzy/substring command completions must survive —
/// but are still canonicalised + escaped, so an argument-position filename
/// with a space (`vim my fi<Tab>`) round-trips too.
fn realign_driver_path_candidates(
    token: &str,
    quote: Option<char>,
    locals: &[CompletionCandidate],
    driver: Vec<CompletionCandidate>,
) -> Vec<CompletionCandidate> {
    let path_shaped = token.contains('/');
    // A successful local listing is the authoritative directory contents.
    let local_listing_present = path_shaped && !locals.is_empty();
    // The token's own directory components (`/usr/bin/af` → {usr, bin}), for
    // the no-local-listing fallback's ancestor-component check.
    let token_components: std::collections::HashSet<&str> = if path_shaped {
        token
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .unwrap_or("")
            .split('/')
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    let token_lower = token.to_lowercase();
    driver
        .into_iter()
        .filter_map(|mut c| {
            // Canonicalise first: zsh's capture emits the SAME match both raw
            // (`Application Scripts`) and pre-escaped (`Application\ Scripts`);
            // unescaping both to the literal lets them collapse and re-escape
            // to one value (otherwise the pre-escaped one double-escapes).
            let literal = local::unescape(&c.value);
            c.display = literal.clone();
            // Filters run on the UNESCAPED aligned value (the token is also
            // unescaped, so prefix / component checks line up).
            let aligned = align_driver_value_to_token(token, &literal);
            if path_shaped {
                if local_listing_present {
                    // Authoritative local listing — drop every driver path
                    // candidate (redundant matches and junk non-matches alike).
                    return None;
                }
                // No local listing: cull the self-evident junk. Must extend
                // the typed token (case-insensitive — tolerant, only ever
                // keeping more legitimate matches).
                if !aligned.to_lowercase().starts_with(&token_lower) {
                    return None;
                }
                // Drop an ancestor-component match (`/usr/` → `usr`): its leaf
                // is a directory the token already names, never a real child.
                let leaf = aligned.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                if token_components.contains(leaf) {
                    return None;
                }
            }
            // Escape for the shell context, mirroring the local path source.
            c.value = local::escape_for_context(&aligned, quote);
            Some(c)
        })
        .collect()
}

/// Rewrite a driver candidate's `value` so it is the full replacement for
/// the whole completion `token`, matching the convention the local path
/// source uses.
///
/// Shell sidecars complete the **last path segment**: for the word `~/Lib`
/// a sidecar returns `Library`, not `~/Library`. The popup replaces the
/// entire token on accept, so a bare-segment value would drop the `~/`
/// directory prefix the user typed (`cd ~/Lib<Tab>` → `cd Library`, which
/// points at the wrong directory when cwd isn't `~`). Prepend the token's
/// directory prefix — everything up to and including its last `/` — so the
/// value only effectively replaces the partial last segment.
///
/// Leaves the value untouched when:
/// - the token has no `/` (the whole token IS the segment: command names,
///   subcommands, flags, git branches — `git che` → `checkout`); or
/// - the value already carries the dir prefix (fish's `complete -C`
///   returns the full word `~/Library/`) or is itself absolute (a path the
///   sidecar already resolved) — both already full-token replacements.
fn align_driver_value_to_token(token: &str, value: &str) -> String {
    let Some(slash) = token.rfind('/') else {
        return value.to_string();
    };
    // Up to and including the last `/` (`~/`, `/`, `src/`). `slash` is a
    // byte index at an ASCII `/`, so `..=slash` is on a char boundary.
    let dir_prefix = &token[..=slash];
    if value.starts_with(dir_prefix) || value.starts_with('/') {
        value.to_string()
    } else {
        format!("{dir_prefix}{value}")
    }
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
        // no waiting. Uses a **bash** pane: bash keeps the per-tool driver
        // path only (no sidecar), so a non-cobra command opens locals
        // immediately. (In a zsh pane `ls ` now routes to the live shell —
        // covered by the zsh routing tests.)
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"").unwrap();
        let plan =
            plan_completion("ls ", 3, Some(dir.path()), None, &[], ShellSpec::Bash, Vec::new);
        match plan {
            CompletionPlan::Open(popup) => {
                assert!(popup.candidates.iter().any(|c| c.display.ends_with("notes.txt")));
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn plan_non_driver_no_local_match_is_closed() {
        // bash pane, non-cobra command, no local match → nothing. (zsh would
        // instead await the live shell — see the zsh routing tests.)
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_completion(
            "ls zzz_no_such",
            14,
            Some(dir.path()),
            None,
            &[],
            ShellSpec::Bash,
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

        // Same input in a zsh pane: `frobnicate` has no cobra driver, so it
        // routes to the zsh LIVE shell (the long tail) — `ZshComplete`, not
        // `FishComplete`. The routing is shell-specific.
        let zsh = plan_completion(
            "frobnicate --wi",
            15,
            Some(dir.path()),
            None,
            &[],
            ShellSpec::Zsh,
            Vec::new,
        );
        match zsh {
            CompletionPlan::AwaitDriver { target, .. } => {
                assert_eq!(target.0, DriverTool::ZshComplete);
                assert_eq!(target.1, "frobnicate --wi");
            }
            other => panic!("expected AwaitDriver(ZshComplete) for zsh long tail, got {other:?}"),
        }
    }

    #[test]
    fn plan_zsh_cobra_tool_stays_authoritative_over_shell_capture() {
        // A zsh pane keeps the robust cobra driver for `git` — it must NOT
        // route `git ch` through the fragile shell capture. (Decision: cobra
        // authoritative, zsh shell for the long tail only.)
        let dir = tempfile::tempdir().unwrap();
        let plan =
            plan_completion("git ch", 6, Some(dir.path()), None, &[], ShellSpec::Zsh, Vec::new);
        match plan {
            CompletionPlan::AwaitDriver { target, .. } => {
                assert_eq!(target.0, DriverTool::Git, "cobra git driver wins, not ZshComplete");
            }
            other => panic!("expected AwaitDriver(Git), got {other:?}"),
        }
    }

    #[test]
    fn plan_zsh_command_position_long_tail_routes_to_shell_capture() {
        // Typing a command NAME with no cobra driver in a zsh pane awaits the
        // live zsh shell — that's what lets runtime aliases / functions
        // complete (the local `$PATH` source can't see them).
        let dir = tempfile::tempdir().unwrap();
        let plan =
            plan_completion("greethe", 7, Some(dir.path()), None, &[], ShellSpec::Zsh, Vec::new);
        match plan {
            CompletionPlan::AwaitDriver { target, .. } => {
                assert_eq!(target.0, DriverTool::ZshComplete);
                assert_eq!(target.1, "greethe");
            }
            other => {
                panic!("expected AwaitDriver(ZshComplete) for the command name, got {other:?}")
            }
        }

        // But an empty editor must NOT fire a capture request.
        let empty = plan_completion("", 0, Some(dir.path()), None, &[], ShellSpec::Zsh, Vec::new);
        assert!(
            !matches!(empty, CompletionPlan::AwaitDriver { .. }),
            "empty editor must not flood the zsh shell, got {empty:?}"
        );
    }

    #[test]
    fn plan_fish_pane_command_position_routes_to_sidecar() {
        // Typing the command NAME in a fish pane awaits the sidecar too —
        // that's what lets aliases / functions / abbreviations complete,
        // which the local `$PATH` source can't see. (A zsh pane would stay
        // local here, since `frobni` maps to no per-tool driver.)
        let dir = tempfile::tempdir().unwrap();
        let plan =
            plan_completion("frobni", 6, Some(dir.path()), None, &[], ShellSpec::Fish, Vec::new);
        match plan {
            CompletionPlan::AwaitDriver { target, .. } => {
                assert_eq!(target.0, DriverTool::FishComplete);
                assert_eq!(target.1, "frobni");
            }
            other => {
                panic!("expected AwaitDriver(FishComplete) for the command name, got {other:?}")
            }
        }

        // But an empty editor must NOT fire `complete -C ""`.
        let empty = plan_completion("", 0, Some(dir.path()), None, &[], ShellSpec::Fish, Vec::new);
        assert!(
            !matches!(empty, CompletionPlan::AwaitDriver { .. }),
            "empty editor must not flood the sidecar, got {empty:?}"
        );
    }

    #[test]
    fn resolve_driver_merges_driver_over_locals() {
        let locals = vec![CompletionCandidate::simple("notes.txt", CompletionSource::Path)];
        let driver = vec![CompletionCandidate::simple(
            "checkout",
            CompletionSource::Driver(DriverTool::Git),
        )];
        let popup = resolve_driver(4, "ch", None, locals, driver).expect("merged popup");
        assert_eq!(popup.candidates[0].value, "checkout", "driver ranks above locals");
    }

    #[test]
    fn resolve_driver_empty_everything_is_none() {
        assert!(resolve_driver(0, "", None, vec![], vec![]).is_none());
    }

    // ---- bare-segment driver values get the token's dir prefix --------
    //
    // Shell sidecars complete the LAST path segment: `complete -C` / zsh
    // capture return `Library` for the word `~/Lib`, not `~/Library`. The
    // popup replaces the whole token on accept, so an un-prefixed value
    // would drop the `~/` the user typed (`cd ~/Lib<Tab>` → `cd Library`,
    // a path that doesn't exist relative to cwd). `resolve_driver` must
    // realign such values to the convention the local path source already
    // uses: the value is the full replacement for the whole token.

    fn driver(value: &str) -> CompletionCandidate {
        CompletionCandidate::simple(value, CompletionSource::Driver(DriverTool::ZshComplete))
    }

    #[test]
    fn resolve_driver_prefixes_bare_segment_with_tilde_dir() {
        // Token `~/Lib`; sidecar returns the bare segment `Library`.
        let popup =
            resolve_driver(3, "~/Lib", None, vec![], vec![driver("Library")]).expect("popup");
        assert_eq!(popup.candidates[0].value, "~/Library", "dir prefix `~/` prepended");
        // The menu still shows the bare segment — nicer, and matches a shell.
        assert_eq!(popup.candidates[0].display, "Library");
    }

    #[test]
    fn resolve_driver_prefixes_bare_segment_under_absolute_dir() {
        // The same generalises to `/us<Tab>` → `usr` (broken) vs `/usr`.
        let popup = resolve_driver(3, "/us", None, vec![], vec![driver("usr")]).expect("popup");
        assert_eq!(popup.candidates[0].value, "/usr");
    }

    #[test]
    fn resolve_driver_prefixes_bare_segment_under_relative_dir() {
        let popup =
            resolve_driver(3, "src/Ca", None, vec![], vec![driver("Cargo.toml")]).expect("popup");
        assert_eq!(popup.candidates[0].value, "src/Cargo.toml");
    }

    #[test]
    fn resolve_driver_leaves_full_word_value_untouched() {
        // fish's `complete -C` returns the FULL word (`~/Library/`); it
        // already carries the dir prefix, so it must not be double-prefixed.
        let popup =
            resolve_driver(3, "~/Lib", None, vec![], vec![driver("~/Library/")]).expect("popup");
        assert_eq!(popup.candidates[0].value, "~/Library/");
    }

    #[test]
    fn resolve_driver_leaves_non_path_token_untouched() {
        // No `/` in the token: command names, subcommands, flags, branches
        // — the whole token IS the segment. `git che` → `checkout`.
        let popup =
            resolve_driver(4, "che", None, vec![], vec![driver("checkout")]).expect("popup");
        assert_eq!(popup.candidates[0].value, "checkout");
    }

    #[test]
    fn resolve_driver_realigned_value_accepts_to_correct_path() {
        // End-to-end: the reported bug. `cd ~/Lib<Tab>`, accept the sidecar
        // candidate → the `~/` prefix survives, not `cd Library`. (No local
        // twin here, so the sidecar row is what's accepted.)
        let mut e = crate::prompt_editor::PromptEditor::new();
        e.insert_str("cd ~/Lib");
        let popup =
            resolve_driver(3, "~/Lib", None, vec![], vec![driver("Library")]).expect("popup");
        // Token `~/Lib` starts at byte 3, length 5.
        popup.accept(&mut e, 5);
        assert_eq!(e.text(), "cd ~/Library ");
    }

    // ---- spurious / redundant sidecar path candidates -----------------
    //
    // zsh's `cd` (directories-only) completion, when no directory matches
    // the typed partial segment, falls back to emitting the ANCESTOR path
    // components: `cd /usr/bin/af<Tab>` captures `usr` and `bin`, not the
    // real `af*` leaves (verified against the live captive child). Aligned
    // to the token those become `/usr/bin/usr`, `/usr/bin/bin` — garbage
    // rows. A real completion of a path token must EXTEND it, so drop any
    // driver value that doesn't.

    fn local_path(value: &str) -> CompletionCandidate {
        CompletionCandidate::simple(value, CompletionSource::Path)
    }

    #[test]
    fn resolve_driver_drops_path_values_that_do_not_extend_the_token() {
        // `usr` / `bin` don't extend `/usr/bin/af`; `afclip` does.
        let popup = resolve_driver(
            3,
            "/usr/bin/af",
            None,
            vec![],
            vec![driver("usr"), driver("bin"), driver("afclip")],
        )
        .expect("popup");
        let values: Vec<&str> = popup.candidates.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["/usr/bin/afclip"], "only the extending leaf survives");
    }

    #[test]
    fn resolve_driver_keeps_non_extending_values_for_non_path_tokens() {
        // The extend filter is path-only: a non-path token (no `/`) routes
        // every candidate through the normal merge — fuzzy/substring command
        // and flag completions must not be culled.
        let popup =
            resolve_driver(0, "co", None, vec![], vec![driver("checkout"), driver("commit")])
                .expect("popup");
        assert_eq!(popup.candidates.len(), 2);
    }

    #[test]
    fn resolve_driver_dedupes_sidecar_path_value_against_local_dir() {
        // The local path source is authoritative for on-disk paths: it
        // already produced `~/Library/` (trailing `/`, known directory). The
        // sidecar's slash-less `~/Library` twin is redundant — drop it so the
        // single surviving row carries the `/` (and accepting a directory
        // ends in `/`, not a space).
        let locals = vec![local_path("~/Library/")];
        let popup =
            resolve_driver(3, "~/Lib", None, locals, vec![driver("Library")]).expect("popup");
        let values: Vec<&str> = popup.candidates.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["~/Library/"], "the slash-bearing local dir row wins");
    }

    #[test]
    fn resolve_driver_drops_all_driver_path_rows_when_local_listing_present() {
        // `cd /usr/<Tab>`: zsh captures `usr` (parent component) plus the real
        // children (verified live). With the authoritative local listing
        // present, every driver path row is dropped — the children are
        // redundant, `usr` is junk — leaving the local dir rows.
        let locals = vec![local_path("/usr/bin/"), local_path("/usr/lib/")];
        let popup = resolve_driver(
            3,
            "/usr/",
            None,
            locals,
            vec![driver("usr"), driver("bin"), driver("lib")],
        )
        .expect("popup");
        let values: Vec<&str> = popup.candidates.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["/usr/bin/", "/usr/lib/"], "ancestor `usr` gone, children deduped");
    }

    #[test]
    fn resolve_driver_drops_ambiguous_intermediate_component_junk() {
        // `cd /usr/lib/dtrace/arm/<Tab>`: zsh emits alternative names for
        // ambiguous intermediate components (`lib`/`libexec`, `arm`/`arm64`)
        // → `libexec`, `arm64`. These are NOT ancestor components and DO
        // vacuously extend the trailing-slash token, so only the authoritative
        // local listing can reject them. With a real local entry present,
        // every driver candidate is dropped.
        let locals = vec![local_path("/usr/lib/dtrace/arm/swift_arm.d")];
        let popup = resolve_driver(
            3,
            "/usr/lib/dtrace/arm/",
            None,
            locals,
            vec![driver("libexec"), driver("arm64")],
        )
        .expect("popup");
        let values: Vec<&str> = popup.candidates.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["/usr/lib/dtrace/arm/swift_arm.d"], "junk gone, local row only");
    }

    #[test]
    fn resolve_driver_drops_ancestor_component_fallback_without_local_listing() {
        // No local listing (the dir couldn't be read): the ancestor-component
        // heuristic still fires. `/usr/` → `usr` aligns to `/usr/usr` (leaf is
        // the token's own component) → dropped; a real-looking leaf survives.
        let popup = resolve_driver(3, "/usr/", None, vec![], vec![driver("usr"), driver("share")])
            .expect("popup");
        let values: Vec<&str> = popup.candidates.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["/usr/share"], "ancestor `usr` culled, real leaf kept");
    }

    // ---- escaping driver path values (spaces / metacharacters) --------
    //
    // The local path source escapes its values (`Application Support` →
    // `Application\ Support`) but the sidecar returns the raw name. Driver
    // values must be escaped the same way so (a) a name with a space round-
    // trips into the shell and (b) the redundancy check compares like-for-
    // like against the escaped local value.

    #[test]
    fn resolve_driver_escapes_path_value_with_space() {
        // No local twin: the surviving sidecar value must be shell-escaped.
        let popup = resolve_driver(
            3,
            "~/Library/Application",
            None,
            vec![],
            vec![driver("Application Support")],
        )
        .expect("popup");
        assert_eq!(popup.candidates[0].value, "~/Library/Application\\ Support");
    }

    #[test]
    fn resolve_driver_dedupes_escaped_path_value_against_local_dir() {
        // The reported case: `cd ~/Library/Application<Tab>` — the local
        // source already produced the escaped, slash-terminated directory, so
        // the sidecar's raw twin is redundant and the path row (with its `/`)
        // wins. Without escaping the driver value, the slash-insensitive dedup
        // would miss (`Application Support` vs `Application\ Support`).
        let locals = vec![local_path("~/Library/Application\\ Support/")];
        let popup = resolve_driver(
            3,
            "~/Library/Application",
            None,
            locals,
            vec![driver("Application Support")],
        )
        .expect("popup");
        let values: Vec<&str> = popup.candidates.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["~/Library/Application\\ Support/"], "escaped local dir row wins");
    }

    #[test]
    fn resolve_driver_canonicalises_mixed_raw_and_preescaped_values() {
        // zsh's capture emits the SAME match BOTH raw and pre-escaped
        // (verified live: `Application Support` AND `Application\ Support`).
        // Both must canonicalise to one value — the pre-escaped one must NOT
        // double-escape (`Application\\\ Support`) — and dedupe to a single
        // row, which (with a local twin) is the slash-bearing path row.
        let locals = vec![local_path("~/Library/Application\\ Support/")];
        let popup = resolve_driver(
            3,
            "~/Library/Application",
            None,
            locals,
            vec![driver("Application Support"), driver("Application\\ Support")],
        )
        .expect("popup");
        let values: Vec<&str> = popup.candidates.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(
            values,
            vec!["~/Library/Application\\ Support/"],
            "both forms collapse, path wins"
        );
    }

    #[test]
    fn resolve_driver_canonicalises_preescaped_value_without_local_twin() {
        // No local twin: the two forms still collapse to ONE single-escaped
        // row (not a double-escaped duplicate).
        let popup = resolve_driver(
            3,
            "~/Library/Application",
            None,
            vec![],
            vec![driver("Application Support"), driver("Application\\ Support")],
        )
        .expect("popup");
        let values: Vec<&str> = popup.candidates.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["~/Library/Application\\ Support"]);
    }

    #[test]
    fn resolve_driver_escapes_argument_filename_with_space_non_path_token() {
        // A non-path token (`my fi`, no `/`) skips the path filters but still
        // gets escaped — an argument-position filename with a space must round-
        // trip (`vim my fi<Tab>` → `my\ file.txt`).
        let popup =
            resolve_driver(0, "my fi", None, vec![], vec![driver("my file.txt")]).expect("popup");
        assert_eq!(popup.candidates[0].value, "my\\ file.txt");
    }

    #[test]
    fn resolve_driver_dedupes_directory_accepts_with_trailing_slash() {
        // End-to-end of the dedup: accepting the surviving directory row ends
        // in `/` (keep completing into the path), never a space.
        let mut e = crate::prompt_editor::PromptEditor::new();
        e.insert_str("cd ~/Lib");
        let locals = vec![local_path("~/Library/")];
        let popup =
            resolve_driver(3, "~/Lib", None, locals, vec![driver("Library")]).expect("popup");
        popup.accept(&mut e, 5);
        assert_eq!(e.text(), "cd ~/Library/");
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
