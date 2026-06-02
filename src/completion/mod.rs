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

pub mod local;
pub mod popup;
pub mod ranking;

pub use popup::CompletionPopup;

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
///   is empty and we're not in command position (typing the next
///   argument with nothing typed yet).
/// - **`$PATH` source** fires when we're in command position
///   (typing the command name, not an argument) AND the token has
///   no `/` (path tokens use the filesystem source instead).
/// - **History source** always fires when there's a non-empty
///   buffer prefix — `git st<Tab>` should surface `git status …`
///   from past sessions regardless of where in the line the
///   cursor lives.
pub fn open_completion_at(
    editor_text: &str,
    cursor: usize,
    cwd: Option<&Path>,
    home: Option<&Path>,
    history_lookup: impl FnOnce() -> Vec<String>,
) -> Option<CompletionPopup> {
    let (token_start, token) = local::token_under_cursor(editor_text, cursor);
    let pathish = local::token_is_pathish(token);
    let cmd_pos = local::is_command_position(editor_text, cursor);

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
            let prefix_with_dot_slash = !pathish && !cmd_pos && dir_part.is_empty();
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
            sources.push(entries_cands);
        }
    }

    // ---- $PATH executable source ----------------------------------
    if cmd_pos && !token.contains('/') && !token.is_empty() {
        let exes = scan_path_executables();
        sources.push(local::complete_path_executables(token, &exes));
    }

    // ---- History source -------------------------------------------
    let buffer_prefix = editor_text.get(..cursor).unwrap_or("");
    if !buffer_prefix.trim_start().is_empty() {
        let entries = history_lookup();
        sources.push(local::complete_from_history(buffer_prefix, &entries, 50));
    }

    let merged = ranking::merge_ranked(sources, 200);
    CompletionPopup::new(token_start, token, merged)
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

    #[test]
    fn open_completion_at_no_history_no_match_returns_none() {
        // Empty buffer, command position, history empty, no
        // executables match the empty token (correctly — we don't
        // dump $PATH on a bare Tab). Result: no popup.
        let p = open_completion_at("", 0, None, None, Vec::new);
        assert!(p.is_none());
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
}

impl CompletionSource {
    /// Short tag for the popup's right-edge source label.
    pub fn tag(self) -> &'static str {
        match self {
            CompletionSource::Path => "path",
            CompletionSource::PathExecutable => "$PATH",
            CompletionSource::History => "history",
        }
    }
}
