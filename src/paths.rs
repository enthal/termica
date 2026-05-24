//! On-disk path detection over the visible terminal grid.
//!
//! Phase 1E-m: walk each viewport row, find whitespace-delimited
//! tokens that resolve to an existing file or directory (relative
//! to the shell's current working directory from OSC 7, or absolute,
//! or `~/`-rooted), and return them as [`crate::links::LinkSpan`]s.
//! The `url` field carries the resolved absolute path string — both
//! `open` (macOS) and `xdg-open` (Linux) accept paths the same way
//! they accept URLs, so the click dispatch is unchanged from
//! Phase 1E-l.
//!
//! ## False-positive control
//!
//! The two filters that keep this from underlining every English
//! word in the terminal:
//!
//! 1. **Token-shape filter**: a candidate must contain at least
//!    one alphanumeric character (or `_`). This rejects pure
//!    punctuation tokens like `|`, `-->`, `<<` but keeps bare
//!    filenames like `Makefile`, `README`, and bare directory
//!    names like `src`.
//! 2. **Existence check**: the resolved path must actually exist
//!    on disk. So `not_a_real_file.txt` in shell output won't
//!    underline, but `Cargo.toml` or `Makefile` in the project
//!    root will. This is what saves us from underlining every
//!    word — `echo hello` produces two stat calls per frame, both
//!    returning false, both invisible.
//!
//! URL-scheme tokens are skipped entirely — they belong to
//! [`crate::links`] and would otherwise stat-spuriously.
//!
//! ## Out of scope for v1
//!
//! - Paths with spaces (e.g. `~/My Documents/file.txt`) — we
//!   tokenise on whitespace, so we'd miss the second half.
//!   Phase 11 polish.
//! - Paths spanning a wrapped line — same WRAPLINE story as URLs.
//! - Grep-style `file.rs:42:7` line/column markers — would need a
//!   secondary parser plus an editor-launching open command.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Cell;

use crate::links::LinkSpan;

/// Scan the currently visible viewport rows for existing-file paths.
///
/// `cwd` is typically the OSC-7-reported shell directory (see
/// [`crate::terminal::TerminalState::cwd`]); `home` is the user's
/// home directory for `~/...` expansion (typically `$HOME`). Both
/// are optional — without `cwd` we still pick up absolute paths,
/// without `home` we still pick up cwd-relative paths.
///
/// The on-disk check is `Path::exists()`. Each scan does one stat
/// per surviving candidate; the path-shape filter (see module
/// docs) keeps that count small in practice.
pub fn scan_visible_paths(
    grid: &Grid<Cell>,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Vec<LinkSpan> {
    scan_visible_paths_with(grid, cwd, home, |p| p.exists())
}

/// Variant of [`scan_visible_paths`] that lets callers inject the
/// existence check. Tests pass a closure that returns `true` for a
/// fixed set of paths so we don't need a tempdir; the production
/// path uses `Path::exists`.
pub fn scan_visible_paths_with<F>(
    grid: &Grid<Cell>,
    cwd: Option<&Path>,
    home: Option<&Path>,
    is_existing: F,
) -> Vec<LinkSpan>
where
    F: Fn(&Path) -> bool,
{
    let display_offset = grid.display_offset() as i32;
    let screen_lines = grid.screen_lines() as i32;
    let cols = grid.columns();
    let mut out = Vec::new();
    for vrow in 0..screen_lines {
        let line = Line(vrow - display_offset);
        let row_chars = collect_row_chars(grid, line, cols);
        for (col_start, col_end, resolved) in
            find_paths_in_chars(&row_chars, cwd, home, &is_existing)
        {
            out.push(LinkSpan {
                start: Point::new(line, Column(col_start)),
                end: Point::new(line, Column(col_end)),
                url: resolved.to_string_lossy().into_owned(),
            });
        }
    }
    out
}

fn collect_row_chars(grid: &Grid<Cell>, line: Line, cols: usize) -> Vec<char> {
    let mut v = Vec::with_capacity(cols);
    for c in 0..cols {
        v.push(grid[Point::new(line, Column(c))].c);
    }
    v
}

/// Pure path scanner.
///
/// Tokenises `chars` on whitespace, filters to plausible path-
/// shaped tokens (must contain at least one alphanumeric / `_`, or
/// start with `~`), skips URL-scheme tokens (those belong to the
/// link engine), trims a few trailing punctuation chars (`,`, `:`,
/// `)`, ...), and finally asks `is_existing` whether the resolved
/// path is real. Returns `(col_start, col_end_inclusive,
/// resolved_path)` for each existing path.
///
/// We trim trailing `.` only as a fallback — many filenames end in
/// `.` (e.g. `.bashrc.`? no, but extensions like `.rs` do) and
/// stripping it blindly would break those. So we try the token
/// with the `.` first; if that's not on disk, we try one shorter.
pub fn find_paths_in_chars<F>(
    chars: &[char],
    cwd: Option<&Path>,
    home: Option<&Path>,
    is_existing: &F,
) -> Vec<(usize, usize, PathBuf)>
where
    F: Fn(&Path) -> bool,
{
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // Skip whitespace.
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // Collect one whitespace-delimited token.
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let token = &chars[start..i];

        // Skip URL-scheme tokens — `https://...` is a link, not a
        // path candidate. Repeating the URL_SCHEMES list here
        // keeps `paths` independent of `links`' internals.
        let token_str_for_scheme: String = token.iter().take(10).collect();
        if starts_with_url_scheme(&token_str_for_scheme) {
            continue;
        }

        // Strip clearly-junk trailing punctuation. We leave `.`
        // alone here — extensions are too important to risk
        // mis-trimming on the first pass.
        let mut end = trim_trailing_junk(token);
        if end == 0 {
            continue;
        }

        // Path-shape pre-filter: skip tokens that are obviously
        // not paths so we don't `stat` every whitespace word.
        if !is_path_shaped(&token[..end]) {
            continue;
        }

        // First try: the trimmed-junk form (e.g. `foo.rs` from
        // `foo.rs,`).
        if let Some(resolved) = resolve_and_check(&token[..end], cwd, home, is_existing) {
            out.push((start, start + end - 1, resolved));
            continue;
        }

        // Fallback: peel one more trailing `.` if the token ended
        // in one. This catches `see foo.rs.` (sentence-final dot)
        // without breaking `foo.rs` (extension dot).
        if end >= 2 && token[end - 1] == '.' {
            end -= 1;
            if !is_path_shaped(&token[..end]) {
                continue;
            }
            if let Some(resolved) = resolve_and_check(&token[..end], cwd, home, is_existing) {
                out.push((start, start + end - 1, resolved));
            }
        }
    }
    out
}

/// Trim trailing punctuation that is far more often "glue" between
/// the path and surrounding prose than legitimate filename content.
/// `.` is *not* in this set (it's an extension separator); the
/// caller does a fallback pass for sentence-final `.`.
fn trim_trailing_junk(chars: &[char]) -> usize {
    let mut end = chars.len();
    while end > 0
        && matches!(
            chars[end - 1],
            ',' | ';' | ':' | ')' | ']' | '}' | '\'' | '"' | '!' | '?' | '`' | '>'
        )
    {
        end -= 1;
    }
    end
}

/// `true` for tokens that *could* plausibly be a path. Conservative:
/// the token starts with `~` (home-dir expansion) OR contains at
/// least one alphanumeric or `_` character. The real filter is the
/// on-disk existence check; this exists only to skip pure-
/// punctuation tokens like `|`, `-->`, `&&`, `<<` so we don't stat
/// them.
///
/// Crucially, this does NOT require a `/` or `.` — extensionless
/// filenames (`Makefile`, `README`, `LICENSE`) and bare directory
/// names (`src`, `tests`) are valid path tokens, and the existence
/// check is what decides whether they're real.
fn is_path_shaped(chars: &[char]) -> bool {
    if chars.first() == Some(&'~') {
        return true;
    }
    chars.iter().any(|c| c.is_alphanumeric() || *c == '_')
}

/// URL-scheme prefixes to skip. Kept in sync with [`crate::links`]
/// by convention — the path engine simply ignores URL-shaped
/// tokens to avoid double-claiming the same cells.
const URL_SCHEMES: &[&str] = &["https://", "http://", "ftp://", "file://", "mailto:"];

fn starts_with_url_scheme(s: &str) -> bool {
    URL_SCHEMES.iter().any(|p| s.starts_with(p))
}

/// Resolve a textual path token to an absolute `PathBuf`, then ask
/// the existence-check closure whether it's real.
///
/// `/foo` → absolute; `~/foo` → joined onto `home`; `~` → `home`;
/// anything else → joined onto `cwd`. Returns `None` if the
/// required base is unset, or if the resolved path doesn't exist.
fn resolve_and_check<F>(
    chars: &[char],
    cwd: Option<&Path>,
    home: Option<&Path>,
    is_existing: &F,
) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    if chars.is_empty() {
        return None;
    }
    let s: String = chars.iter().collect();
    let p = if s.starts_with('/') {
        PathBuf::from(s)
    } else if let Some(rest) = s.strip_prefix("~/") {
        home?.join(rest)
    } else if s == "~" {
        home?.to_path_buf()
    } else {
        cwd?.join(&s)
    };
    if is_existing(&p) { Some(p) } else { None }
}

#[cfg(test)]
mod tests {
    //! Pure tests — the existence check is injected, so no tempdir
    //! or real I/O is involved. The grid-walking layer is exercised
    //! by a small in-memory `Term` fixture.

    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// Build an `is_existing` closure backed by a fixed set of
    /// "existing" absolute paths.
    fn exists_predicate(paths: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |p: &Path| set.contains(p)
    }

    fn cwd() -> PathBuf {
        PathBuf::from("/Users/tim/code")
    }
    fn home() -> PathBuf {
        PathBuf::from("/Users/tim")
    }

    // --- absolute paths -------------------------------------------

    #[test]
    fn detects_absolute_path() {
        let exists = exists_predicate(&["/etc/hosts"]);
        let r = find_paths_in_chars(
            &chars("edit /etc/hosts please"),
            Some(&cwd()),
            Some(&home()),
            &exists,
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn absolute_path_with_trailing_comma_trimmed() {
        let exists = exists_predicate(&["/etc/hosts"]);
        let r = find_paths_in_chars(&chars("/etc/hosts,"), Some(&cwd()), Some(&home()), &exists);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn absolute_path_with_sentence_dot_trimmed() {
        let exists = exists_predicate(&["/etc/hosts"]);
        let r =
            find_paths_in_chars(&chars("see /etc/hosts."), Some(&cwd()), Some(&home()), &exists);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, PathBuf::from("/etc/hosts"));
    }

    // --- cwd-relative paths ---------------------------------------

    #[test]
    fn cwd_relative_path_resolves_against_cwd() {
        // "src/lib.rs" resolves to "/Users/tim/code/src/lib.rs".
        let exists = exists_predicate(&["/Users/tim/code/src/lib.rs"]);
        let r =
            find_paths_in_chars(&chars("edit src/lib.rs"), Some(&cwd()), Some(&home()), &exists);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, PathBuf::from("/Users/tim/code/src/lib.rs"));
    }

    #[test]
    fn bare_filename_in_cwd_resolves() {
        // "Cargo.toml" with no `/` — but it has a `.`, so it
        // survives the path-shape filter.
        let exists = exists_predicate(&["/Users/tim/code/Cargo.toml"]);
        let r = find_paths_in_chars(
            &chars("see Cargo.toml here"),
            Some(&cwd()),
            Some(&home()),
            &exists,
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, PathBuf::from("/Users/tim/code/Cargo.toml"));
    }

    #[test]
    fn cwd_relative_path_requires_cwd() {
        let exists = exists_predicate(&["/Users/tim/code/Cargo.toml"]);
        // No cwd → can't resolve a relative path. Should silently
        // skip rather than panic.
        let r = find_paths_in_chars(&chars("Cargo.toml"), None, Some(&home()), &exists);
        assert!(r.is_empty());
    }

    // --- ~/ expansion ---------------------------------------------

    #[test]
    fn tilde_expands_to_home() {
        let exists = exists_predicate(&["/Users/tim/.zshrc"]);
        let r = find_paths_in_chars(&chars("vim ~/.zshrc"), Some(&cwd()), Some(&home()), &exists);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, PathBuf::from("/Users/tim/.zshrc"));
    }

    #[test]
    fn bare_tilde_resolves_to_home() {
        let exists = exists_predicate(&["/Users/tim"]);
        let r = find_paths_in_chars(&chars("cd ~"), Some(&cwd()), Some(&home()), &exists);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, PathBuf::from("/Users/tim"));
    }

    // --- false-positive control -----------------------------------

    #[test]
    fn plain_identifier_is_not_a_path_when_nonexistent() {
        // `echo` and `hello` are alphanumeric so they DO survive the
        // token-shape filter; the existence predicate is what rules
        // them out (nothing in the predicate's allow-list). This is
        // the path the scanner takes for every English word in shell
        // output.
        let exists = exists_predicate(&[]);
        let r = find_paths_in_chars(&chars("echo hello"), Some(&cwd()), Some(&home()), &exists);
        assert!(r.is_empty(), "expected no paths; got {r:?}");
    }

    #[test]
    fn extensionless_filename_in_cwd_is_detected() {
        // `Makefile`, `README`, `LICENSE` are real files without an
        // extension. The token-shape filter used to require a `.`
        // or `/` and silently dropped these; v2 of the filter only
        // requires an alphanumeric char.
        let exists = exists_predicate(&["/Users/tim/code/Makefile"]);
        let r =
            find_paths_in_chars(&chars("see Makefile here"), Some(&cwd()), Some(&home()), &exists);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, PathBuf::from("/Users/tim/code/Makefile"));
    }

    #[test]
    fn bare_dirname_in_cwd_is_detected() {
        // `src` — directory name, no `.` or `/`. Should still get
        // picked up.
        let exists = exists_predicate(&["/Users/tim/code/src"]);
        let r = find_paths_in_chars(&chars("ls src"), Some(&cwd()), Some(&home()), &exists);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, PathBuf::from("/Users/tim/code/src"));
    }

    #[test]
    fn pure_punctuation_token_is_skipped() {
        // `|`, `-->`, `&&` are pure punctuation. The token-shape
        // filter saves the stat call for these so a terminal full
        // of shell-pipe glyphs doesn't thrash the filesystem.
        // (We can't easily verify "no stat happened" with the
        // closure-based predicate, but a non-empty allow-list is
        // intentionally provided to prove that even *if* one of
        // these resolved to an existing path we wouldn't return it.)
        let exists = exists_predicate(&["/Users/tim/code/|", "/Users/tim/code/-->"]);
        let r = find_paths_in_chars(&chars("a | b --> c"), Some(&cwd()), Some(&home()), &exists);
        assert!(r.is_empty(), "pure-punctuation tokens should be skipped; got {r:?}");
    }

    #[test]
    fn nonexistent_path_is_not_detected() {
        let exists = exists_predicate(&[]); // nothing exists
        let r =
            find_paths_in_chars(&chars("ls /does/not/exist"), Some(&cwd()), Some(&home()), &exists);
        assert!(r.is_empty());
    }

    #[test]
    fn url_scheme_token_is_skipped() {
        // `https://...` is a URL, not a path — the path scanner
        // should not stat the literal-string version.
        let exists = exists_predicate(&["/Users/tim/code/https://example.com"]);
        let r = find_paths_in_chars(
            &chars("see https://example.com"),
            Some(&cwd()),
            Some(&home()),
            &exists,
        );
        assert!(r.is_empty(), "URL token should not be path-detected");
    }

    // --- multiple paths on one row --------------------------------

    #[test]
    fn detects_two_paths_on_same_row() {
        let exists =
            exists_predicate(&["/Users/tim/code/src/lib.rs", "/Users/tim/code/Cargo.toml"]);
        let r = find_paths_in_chars(
            &chars("diff src/lib.rs Cargo.toml"),
            Some(&cwd()),
            Some(&home()),
            &exists,
        );
        assert_eq!(r.len(), 2);
    }

    // --- column ranges -------------------------------------------

    #[test]
    fn column_range_covers_token_only() {
        let exists = exists_predicate(&["/etc/hosts"]);
        // "edit /etc/hosts please"
        //  0123456789...
        //       ^col 5, ends at col 14 (inclusive). `e` at 0, `d`
        //       at 1, `i` at 2, `t` at 3, ` ` at 4, `/` at 5.
        let r = find_paths_in_chars(
            &chars("edit /etc/hosts please"),
            Some(&cwd()),
            Some(&home()),
            &exists,
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 5);
        assert_eq!(r[0].1, 14);
    }

    // --- grid-walking integration --------------------------------

    use alacritty_terminal::Term;
    use alacritty_terminal::event::{Event, EventListener};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    #[derive(Default)]
    struct NopListener;
    impl EventListener for NopListener {
        fn send_event(&self, _e: Event) {}
    }

    fn term_with(text: &[u8], rows: u16, cols: u16) -> Term<NopListener> {
        let size = TermSize::new(cols as usize, rows as usize);
        let mut term = Term::new(Config::default(), &size, NopListener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        for b in text {
            parser.advance(&mut term, &[*b]);
        }
        term
    }

    #[test]
    fn scan_visible_paths_finds_existing_path_in_grid() {
        let term = term_with(b"edit src/lib.rs please", 5, 60);
        let exists = exists_predicate(&["/Users/tim/code/src/lib.rs"]);
        let spans = scan_visible_paths_with(term.grid(), Some(&cwd()), Some(&home()), &exists);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "/Users/tim/code/src/lib.rs");
        // It's on row 0.
        assert_eq!(spans[0].start.line.0, 0);
    }

    #[test]
    fn scan_visible_paths_returns_empty_when_path_does_not_exist() {
        let term = term_with(b"edit src/missing.rs", 5, 60);
        let exists = exists_predicate(&[]); // nothing exists
        let spans = scan_visible_paths_with(term.grid(), Some(&cwd()), Some(&home()), &exists);
        assert!(spans.is_empty());
    }
}
