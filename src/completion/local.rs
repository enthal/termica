//! Local candidate generators: token detection + paths + `$PATH`
//! executables + history.
//!
//! Pure logic where the inputs cross any I/O boundary (filesystem
//! reads, history DB queries) — those are taken as arguments so
//! the unit tests can exercise the candidate-shaping logic in
//! isolation. The renderer is responsible for performing the I/O
//! and feeding results in.

use super::{CompletionCandidate, CompletionSource};

/// Find the completion token ending at `cursor` in `text`. Returns
/// `(byte_start, token_str)` where `byte_start` is the index in
/// `text` where the token begins (so the caller can replace the
/// range `byte_start..cursor` on accept).
///
/// A "token" here is the maximal run of non-separator chars ending
/// at `cursor`. Separators are whitespace and the shell-special
/// chars `|`, `&`, `;`, `<`, `>`, `(`, `)` — same set
/// [`crate::shell_syntax`] uses to terminate one token. `cursor`
/// is assumed to lie on a char boundary in `text`; out-of-bounds
/// or mid-multi-byte inputs are clamped down via
/// [`crate::prompt_editor::clamp_to_char_boundary`].
///
/// Returning `(cursor, "")` is the natural "empty token" answer
/// when the cursor sits on a separator with no preceding word
/// chars — e.g. immediately after `cd ` the token is empty and
/// the path completer will list the cwd's entries.
pub fn token_under_cursor(text: &str, cursor: usize) -> (usize, &str) {
    let cursor = crate::prompt_editor::clamp_to_char_boundary(text, cursor);
    let prefix = text.get(..cursor).unwrap_or("");
    // Walk char_indices in reverse to find the separator nearest
    // the cursor. Pure char-boundary iteration — no raw byte
    // arithmetic, per CLAUDE.md.
    for (b, c) in prefix.char_indices().rev() {
        if is_token_separator(c) {
            let start = b + c.len_utf8();
            // Same safe-slice pattern (str::get returns Option; we
            // already verified cursor is char-aligned by the clamp
            // above, so this is `Some`, but the explicit form keeps
            // the panic class out of the surface area).
            let token = text.get(start..cursor).unwrap_or("");
            return (start, token);
        }
    }
    // No separator found — the token runs from byte 0.
    (0, prefix)
}

/// True when `c` ends a completion token. Whitespace + shell-
/// special pipes / redirects / sequencers / parens. Punctuation
/// like `.` and `-` are NOT separators because they're common
/// inside paths, flags (`--long-name`), and identifiers.
pub fn is_token_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')')
}

/// True if the byte position `cursor` is in "command position"
/// in `text` — i.e. there is NO non-separator content before it
/// on this command-line stretch (the stretch before the cursor
/// up to the most recent `|`, `&`, `;`).
///
/// Used by the `$PATH` completer to fire only when the token is
/// the command name, not an argument. `cd <Tab>` is path-mode
/// (`cd` is in command position but `<Tab>` follows whitespace);
/// `<Tab>` at the start of the buffer is command-mode; `git ` is
/// command-mode followed by argument-mode for the next token.
pub fn is_command_position(text: &str, cursor: usize) -> bool {
    // The current TOKEN starts at `token_start`. Command position
    // means "between the most recent command-sequencer (`|`, `&`,
    // `;`) and the start of the current token, there is no non-
    // whitespace content." If there is, the current token is an
    // argument to that earlier content; otherwise it's a command
    // name.
    let (token_start, _) = token_under_cursor(text, cursor);
    let prefix = text.get(..token_start).unwrap_or("");
    let mut has_content_since_last_sequencer = false;
    for c in prefix.chars() {
        if matches!(c, '|' | '&' | ';') {
            has_content_since_last_sequencer = false;
        } else if !c.is_whitespace() {
            has_content_since_last_sequencer = true;
        }
    }
    !has_content_since_last_sequencer
}

/// Path-shaped triggers: the token is path-mode iff it starts
/// with `/`, `./`, `../`, `~/`, OR it contains a `/` anywhere.
/// Same rule v1 spec/04 §"Tab handling" calls out.
///
/// Returns `true` for tokens that should be completed by the
/// filesystem source. The `$PATH` and history sources still
/// produce candidates in parallel; the ranker decides which gets
/// priority.
pub fn token_is_pathish(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if token.contains('/') {
        return true;
    }
    matches!(token.chars().next(), Some('~'))
}

/// Split a path-shaped completion token into `(dir_part, file_prefix)`:
///
/// - `src/main`        → `("src", "main")`
/// - `/etc/pas`        → `("/etc", "pas")`
/// - `~/.zsh`          → `("~", ".zsh")`
/// - `./RE`            → `(".", "RE")`
/// - `Cargo`           → `("", "Cargo")`  (no slash → current dir)
/// - `src/`            → `("src", "")`    (listing dir contents)
/// - empty             → `("", "")`
///
/// `dir_part` retains its slashes intact except that a trailing
/// `/` is dropped (so the renderer can join `dir + "/" + file`
/// uniformly). A token of exactly `/` returns `("/", "")` — the
/// filesystem-root case.
pub fn split_path_token(token: &str) -> (&str, &str) {
    if token.is_empty() {
        return ("", "");
    }
    if token == "/" {
        return ("/", "");
    }
    match token.rfind('/') {
        Some(idx) => {
            // Keep the leading slash in dir for absolute paths.
            let dir = if idx == 0 { "/" } else { &token[..idx] };
            let file = &token[idx + 1..];
            (dir, file)
        }
        None => ("", token),
    }
}

/// Filter a directory listing into completion candidates.
///
/// `entries` is a list of `(filename, is_dir)` pairs as a
/// caller would build from `std::fs::read_dir`. The renderer does
/// the actual read; this function only does the prefix-filter +
/// sort + candidate-shape work so the unit tests don't need a
/// real filesystem.
///
/// `prefix` is the file-name portion of the token (everything
/// after the last `/`). Directory entries get a trailing `/`
/// appended to their display + value so the user can `Tab` again
/// to descend.
///
/// Sorted by: directories first (dirs sort before files), then
/// case-insensitive alphabetical. Hidden entries (`.`-prefixed)
/// are skipped UNLESS the typed prefix itself starts with `.`.
pub fn complete_path_entries(prefix: &str, entries: &[(String, bool)]) -> Vec<CompletionCandidate> {
    let show_hidden = prefix.starts_with('.');
    let mut out: Vec<CompletionCandidate> = entries
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .filter(|(name, _)| show_hidden || !name.starts_with('.') || prefix.starts_with('.'))
        .map(|(name, is_dir)| {
            let value = if *is_dir { format!("{name}/") } else { name.clone() };
            let description = if *is_dir { Some("dir".into()) } else { None };
            CompletionCandidate {
                display: value.clone(),
                value,
                description,
                source: CompletionSource::Path,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let a_is_dir = a.value.ends_with('/');
        let b_is_dir = b.value.ends_with('/');
        b_is_dir.cmp(&a_is_dir).then_with(|| a.value.to_lowercase().cmp(&b.value.to_lowercase()))
    });
    out
}

/// Filter a list of `$PATH`-discovered executable names into
/// completion candidates that start with `prefix`. Caller supplies
/// the executable list (built once per cwd with caching at the
/// renderer level).
///
/// Empty `prefix` returns an empty result — `Tab` on a bare
/// `$PATH` lookup with no typed prefix would dump thousands of
/// candidates, which is unhelpful UX. v1 keeps this simple; a
/// future polish pass can show a paged preview.
pub fn complete_path_executables(prefix: &str, executables: &[String]) -> Vec<CompletionCandidate> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<CompletionCandidate> = executables
        .iter()
        .filter(|name| name.starts_with(prefix))
        .filter(|name| seen.insert((*name).clone()))
        .map(|name| CompletionCandidate::simple(name.clone(), CompletionSource::PathExecutable))
        .collect();
    out.sort_by(|a, b| a.value.cmp(&b.value));
    out
}

/// Filter a list of historical commands into candidates that
/// prefix-match the typed buffer.
///
/// `buffer_prefix` is the editor buffer from the start of the
/// command up to the cursor — i.e., the typed prefix of the WHOLE
/// command (not just the token under the cursor). Matching by
/// whole-buffer prefix means typing `git st <Tab>` surfaces
/// historical `git status …` lines, not just commands starting
/// with `st`.
///
/// `entries` is newest-first (the renderer queries history with a
/// recency ORDER BY). De-duplicated as a side-effect of the
/// dedup HashSet.
pub fn complete_from_history(
    buffer_prefix: &str,
    entries: &[String],
    limit: usize,
) -> Vec<CompletionCandidate> {
    if buffer_prefix.is_empty() {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .filter(|h| h.starts_with(buffer_prefix))
        .filter(|h| h.as_str() != buffer_prefix) // hide the exact-match no-op
        .filter(|h| seen.insert((*h).clone()))
        .take(limit)
        .map(|h| CompletionCandidate::simple(h.clone(), CompletionSource::History))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- token_under_cursor ----------------------------------------

    #[test]
    fn token_at_buffer_start_returns_full_prefix() {
        let (start, token) = token_under_cursor("ls", 2);
        assert_eq!(start, 0);
        assert_eq!(token, "ls");
    }

    #[test]
    fn token_after_space_picks_up_just_the_word() {
        let (start, token) = token_under_cursor("ls -la", 6);
        assert_eq!(start, 3);
        assert_eq!(token, "-la");
    }

    #[test]
    fn token_after_pipe_resets() {
        let (start, token) = token_under_cursor("ls | gr", 7);
        assert_eq!(start, 5);
        assert_eq!(token, "gr");
    }

    #[test]
    fn empty_token_when_cursor_on_separator() {
        // Cursor sits right after a space — token is empty.
        let (start, token) = token_under_cursor("cd ", 3);
        assert_eq!(start, 3);
        assert_eq!(token, "");
    }

    #[test]
    fn token_with_path_separator_kept_whole() {
        // Slashes are NOT token separators — they're part of paths.
        let (start, token) = token_under_cursor("cat src/main.rs", 15);
        assert_eq!(start, 4);
        assert_eq!(token, "src/main.rs");
    }

    #[test]
    fn token_handles_multibyte_chars() {
        // Token at end of "echo é" — 'é' is 2 bytes; cursor at byte 7.
        let text = "echo é";
        let (start, token) = token_under_cursor(text, text.len());
        assert_eq!(start, 5);
        assert_eq!(token, "é");
    }

    #[test]
    fn token_cursor_past_end_clamps_to_end() {
        let (start, token) = token_under_cursor("ls", 999);
        assert_eq!(start, 0);
        assert_eq!(token, "ls");
    }

    // ---- is_command_position --------------------------------------

    #[test]
    fn empty_buffer_is_command_position() {
        assert!(is_command_position("", 0));
    }

    #[test]
    fn leading_whitespace_is_command_position() {
        assert!(is_command_position("   ", 3));
    }

    #[test]
    fn first_token_is_command_position() {
        assert!(is_command_position("git", 3));
    }

    #[test]
    fn second_token_is_not_command_position() {
        // After "git " — typing the first argument.
        assert!(!is_command_position("git ", 4));
    }

    #[test]
    fn after_pipe_resets_to_command_position() {
        // `ls | gr<TAB>` — the second command IS in command position.
        assert!(is_command_position("ls | gr", 7));
    }

    #[test]
    fn after_semicolon_resets_to_command_position() {
        assert!(is_command_position("cd /tmp; ls", 11));
    }

    // ---- token_is_pathish -----------------------------------------

    #[test]
    fn token_starting_with_slash_is_pathish() {
        assert!(token_is_pathish("/etc"));
    }

    #[test]
    fn token_with_dot_slash_is_pathish() {
        assert!(token_is_pathish("./README"));
    }

    #[test]
    fn token_with_tilde_is_pathish() {
        assert!(token_is_pathish("~/projects"));
    }

    #[test]
    fn token_with_embedded_slash_is_pathish() {
        assert!(token_is_pathish("src/main"));
    }

    #[test]
    fn plain_word_is_not_pathish() {
        assert!(!token_is_pathish("kubectl"));
    }

    #[test]
    fn empty_token_is_not_pathish() {
        assert!(!token_is_pathish(""));
    }

    // ---- split_path_token ----------------------------------------

    #[test]
    fn split_path_token_no_slash_yields_empty_dir() {
        assert_eq!(split_path_token("Cargo"), ("", "Cargo"));
    }

    #[test]
    fn split_path_token_with_slash_keeps_dir_intact() {
        assert_eq!(split_path_token("src/main"), ("src", "main"));
        assert_eq!(split_path_token("a/b/c"), ("a/b", "c"));
    }

    #[test]
    fn split_path_token_absolute_keeps_leading_slash() {
        assert_eq!(split_path_token("/etc/pas"), ("/etc", "pas"));
    }

    #[test]
    fn split_path_token_root_slash_alone() {
        assert_eq!(split_path_token("/"), ("/", ""));
    }

    #[test]
    fn split_path_token_trailing_slash_lists_dir() {
        assert_eq!(split_path_token("src/"), ("src", ""));
    }

    #[test]
    fn split_path_token_empty_returns_empty_pair() {
        assert_eq!(split_path_token(""), ("", ""));
    }

    #[test]
    fn split_path_token_tilde_alone_keeps_as_dir() {
        // `~/foo` splits to `(~, foo)`; `~` alone is degenerate.
        assert_eq!(split_path_token("~/foo"), ("~", "foo"));
    }

    // ---- complete_path_entries -----------------------------------

    fn entry(name: &str, is_dir: bool) -> (String, bool) {
        (name.into(), is_dir)
    }

    #[test]
    fn path_entries_filtered_by_prefix() {
        let entries = vec![entry("src", true), entry("Cargo.toml", false), entry("scripts", true)];
        let out = complete_path_entries("s", &entries);
        let values: Vec<&str> = out.iter().map(|c| c.value.as_str()).collect();
        // Directories first, alphabetical within each group.
        assert_eq!(values, vec!["scripts/", "src/"]);
    }

    #[test]
    fn path_entries_hides_dotfiles_unless_prefix_starts_with_dot() {
        let entries = vec![entry(".env", false), entry("env.toml", false), entry(".git", true)];
        let out = complete_path_entries("", &entries);
        let names: Vec<&str> = out.iter().map(|c| c.value.as_str()).collect();
        assert!(!names.iter().any(|n| n.starts_with('.')));

        let out_dot = complete_path_entries(".", &entries);
        let names_dot: Vec<&str> = out_dot.iter().map(|c| c.value.as_str()).collect();
        assert!(names_dot.iter().any(|n| n.starts_with(".env")));
        assert!(names_dot.iter().any(|n| n.starts_with(".git/")));
    }

    #[test]
    fn path_entries_dirs_get_trailing_slash() {
        let entries = vec![entry("src", true)];
        let out = complete_path_entries("", &entries);
        assert_eq!(out[0].value, "src/");
        assert_eq!(out[0].description.as_deref(), Some("dir"));
    }

    // ---- complete_path_executables -------------------------------

    #[test]
    fn path_executables_empty_prefix_returns_nothing() {
        let exes: Vec<String> = vec!["git".into(), "ls".into()];
        assert!(complete_path_executables("", &exes).is_empty());
    }

    #[test]
    fn path_executables_filter_by_prefix() {
        let exes: Vec<String> = vec!["git".into(), "gh".into(), "ls".into()];
        let out = complete_path_executables("g", &exes);
        let names: Vec<&str> = out.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(names, vec!["gh", "git"]);
    }

    #[test]
    fn path_executables_dedup() {
        // Same binary on $PATH twice (e.g., /usr/bin + /usr/local/bin)
        // — keep one copy.
        let exes: Vec<String> = vec!["git".into(), "git".into(), "git".into()];
        let out = complete_path_executables("gi", &exes);
        assert_eq!(out.len(), 1);
    }

    // ---- complete_from_history -----------------------------------

    #[test]
    fn history_empty_prefix_returns_nothing() {
        let h = vec!["ls".into(), "cd /tmp".into()];
        assert!(complete_from_history("", &h, 10).is_empty());
    }

    #[test]
    fn history_matches_by_buffer_prefix() {
        let h: Vec<String> = vec![
            "git status".into(),
            "git push origin main".into(),
            "ls -la".into(),
            "git log".into(),
        ];
        let out = complete_from_history("git ", &h, 10);
        let values: Vec<&str> = out.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["git status", "git push origin main", "git log"]);
    }

    #[test]
    fn history_excludes_exact_match() {
        // Typing `git status` exactly and pressing Tab — the
        // history entry that matches identically would render as a
        // no-op, so we skip it.
        let h: Vec<String> = vec!["git status".into(), "git status --short".into()];
        let out = complete_from_history("git status", &h, 10);
        let values: Vec<&str> = out.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["git status --short"]);
    }

    #[test]
    fn history_deduplicates() {
        let h: Vec<String> = vec!["git status".into(), "git status".into(), "git status".into()];
        let out = complete_from_history("git", &h, 10);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn history_respects_limit() {
        let h: Vec<String> = (0..20).map(|i| format!("cmd-{i}")).collect();
        let out = complete_from_history("cmd-", &h, 5);
        assert_eq!(out.len(), 5);
    }
}
