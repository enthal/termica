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

/// Quote / escape-aware completion context at `cursor`.
///
/// Unlike [`token_under_cursor`], this understands shell quoting so a
/// quoted filename completes correctly: an opening quote bounds the
/// token (and `start` points just past it, so accept replaces the
/// quoted body), spaces inside a quote — or backslash-escaped — do
/// NOT break the token, and `prefix` is the **unescaped, unquoted**
/// literal to match candidates against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContext {
    /// Byte index where the replaced region begins. For an open
    /// quote this is the byte *after* the quote so the substitution
    /// lands inside it; otherwise the start of the current word.
    pub start: usize,
    /// The literal content to match against candidates: quotes
    /// removed, backslash escapes resolved.
    pub prefix: String,
    /// The active unclosed quote at the cursor, if any. Drives how
    /// the substituted value is escaped ([`escape_for_context`]).
    pub quote: Option<char>,
}

/// Scan the buffer up to `cursor`, tracking shell quote + escape
/// state, and return the [`CompletionContext`] for the word ending at
/// `cursor`. Pure; the heart of quote-aware Tab completion.
pub fn completion_context(text: &str, cursor: usize) -> CompletionContext {
    let cursor = crate::prompt_editor::clamp_to_char_boundary(text, cursor);
    let prefix = text.get(..cursor).unwrap_or("");
    let mut quote: Option<char> = None;
    let mut quote_open_at: Option<usize> = None;
    let mut word_start: Option<usize> = None;
    let mut escaped = false;
    let mut literal = String::new();
    for (b, c) in prefix.char_indices() {
        if escaped {
            // The previous backslash escapes this char (unquoted or
            // inside `"…"`): it's literal, never a quote / separator.
            literal.push(c);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                // Single quotes: everything literal until the next `'`.
                if c == '\'' {
                    quote = None;
                } else {
                    literal.push(c);
                }
            }
            Some('"') => {
                if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    quote = None;
                } else {
                    literal.push(c);
                }
            }
            _ => {
                if c == '\\' {
                    if word_start.is_none() {
                        word_start = Some(b);
                    }
                    escaped = true;
                } else if c == '\'' || c == '"' {
                    if word_start.is_none() {
                        word_start = Some(b);
                    }
                    quote = Some(c);
                    quote_open_at = Some(b);
                } else if is_token_separator(c) {
                    // Unquoted separator ends the current word.
                    word_start = None;
                    quote_open_at = None;
                    literal.clear();
                } else {
                    if word_start.is_none() {
                        word_start = Some(b);
                    }
                    literal.push(c);
                }
            }
        }
    }
    let start = match (quote, quote_open_at) {
        // Inside an open quote: replace the quoted body, after the quote.
        (Some(_), Some(open)) => open + c_len(prefix, open),
        _ => word_start.unwrap_or(cursor),
    };
    CompletionContext { start, prefix: literal, quote }
}

/// Byte length of the char at `idx` in `s` (quotes are always ASCII
/// `'`/`"` = 1 byte, but stay boundary-safe rather than assuming).
fn c_len(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
}

/// Escape `value` so it round-trips literally when inserted into the
/// editor at the given quote context. Applied to the SUBSTITUTED text
/// only — the popup's `display` keeps the human-readable name.
///
/// - Unquoted (`None`): backslash-escape whitespace and shell
///   metacharacters (`my file` → `my\ file`). Path separators (`/`)
///   are left intact so a completed path stays a path.
/// - Inside double quotes (`Some('"')`): only `"`, `$`, `` ` `` and
///   `\` need escaping; spaces and globs are literal inside `"…"`.
/// - Inside single quotes (`Some('\'')`): everything is literal, so
///   the value passes through unchanged (a `'` in the name can't be
///   represented inside `'…'`; that rare case is left as-is).
pub fn escape_for_context(value: &str, quote: Option<char>) -> String {
    match quote {
        Some('\'') => value.to_string(),
        Some('"') => {
            let mut out = String::with_capacity(value.len());
            for c in value.chars() {
                if matches!(c, '"' | '$' | '`' | '\\') {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
        _ => {
            let mut out = String::with_capacity(value.len());
            for c in value.chars() {
                if needs_unquoted_escape(c) {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
    }
}

/// Shell-special chars that must be backslash-escaped in an unquoted
/// completion value. `/` is deliberately excluded so a completed path
/// keeps its separators; `.`, `-`, `_`, `~`, `#` etc. are ordinary
/// inside a word and need no escape.
fn needs_unquoted_escape(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '\\' | '\''
                | '"'
                | '$'
                | '`'
                | '|'
                | '&'
                | ';'
                | '('
                | ')'
                | '<'
                | '>'
                | '*'
                | '?'
                | '['
                | ']'
                | '{'
                | '}'
        )
}

/// Collapse backslash escapes in `value`, returning the literal text:
/// `Application\ Scripts` → `Application Scripts`, `a\\b` → `a\b`. The
/// inverse of [`escape_for_context`] for the unquoted (backslash) style.
///
/// Used to **canonicalize sidecar values**, which arrive in mixed forms —
/// zsh's `compadd` capture emits the SAME match both raw (`Application
/// Scripts`) and pre-escaped (`Application\ Scripts`). Unescaping both to
/// the literal lets them collapse and then re-escape to one consistent
/// value, instead of one being double-escaped (`Application\\\ Scripts`).
///
/// A trailing lone backslash is kept as-is (nothing follows to unescape).
pub fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
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
    // The current TOKEN starts at `token_start`. Command position means
    // "in the command stretch before this token (since the most recent
    // `|` / `&` / `;`), every word is an inline env assignment
    // (`NAME=value`)" — so the token is the command name. A leading
    // `a=b echo` keeps `echo` in command position (mirrors the syntax
    // highlighter), while `git status` puts `status` in argument
    // position.
    let (token_start, _) = token_under_cursor(text, cursor);
    let prefix = text.get(..token_start).unwrap_or("");
    let stretch_start = prefix.rfind(['|', '&', ';']).map(|i| i + 1).unwrap_or(0);
    let stretch = prefix.get(stretch_start..).unwrap_or("");
    stretch.split_whitespace().all(is_env_assignment)
}

/// True if `word` is a shell inline env assignment — `NAME=...` where
/// `NAME` is a valid identifier (`[A-Za-z_][A-Za-z0-9_]*`). The value
/// part is unrestricted.
fn is_env_assignment(word: &str) -> bool {
    let Some(eq) = word.find('=') else { return false };
    let name = &word[..eq];
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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

/// If `token` is an environment-variable reference being typed — `$`,
/// `$NAME` — return the (possibly empty) name prefix to complete. A `/`
/// in the token means it's a path expansion (`$HOME/sub`), handled by
/// the filesystem source instead, so this returns `None`. Anything not
/// shaped like a `$`-prefixed identifier returns `None`.
pub fn env_var_prefix(token: &str) -> Option<&str> {
    let rest = token.strip_prefix('$')?;
    if rest.contains('/') {
        return None;
    }
    if rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { Some(rest) } else { None }
}

/// Env-var name candidates whose name starts with `prefix` (case-
/// sensitive, like the shell). Each candidate's value is the full
/// `$NAME` so accepting it replaces the typed `$pre` token. `names` is
/// the list of variable names (production passes `std::env::vars`);
/// taking it as a parameter keeps this pure + testable.
pub fn complete_env_vars(prefix: &str, names: &[String]) -> Vec<CompletionCandidate> {
    let mut out: Vec<CompletionCandidate> = names
        .iter()
        .filter(|n| n.starts_with(prefix))
        .map(|n| CompletionCandidate::simple(format!("${n}"), CompletionSource::EnvVar))
        .collect();
    out.sort_by(|a, b| a.value.cmp(&b.value));
    out.dedup_by(|a, b| a.value == b.value);
    out
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
    // `~` alone is "the home directory" — treat it as a dir
    // with empty file prefix so `resolve_dir` can expand it
    // and we list home contents. Without this special case the
    // token falls through the `rfind('/')` arm as
    // `("", "~")` and we end up listing cwd filtered for files
    // named `~` (always empty).
    if token == "~" {
        return ("~", "");
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

/// Like [`complete_path_executables`] but **sorts by global
/// history recency**: executables whose name has been used as
/// the FIRST WORD of a past command line rank above those that
/// haven't, with the most recently-used ones first. Within each
/// group the secondary sort is alphabetical.
///
/// "First word" is important — `echo git` in history boosts
/// `echo`, NOT `git`. A user's earlier `git status` boosts `git`
/// (used at position 0), but the `git` token in `echo git` is an
/// argument and doesn't count.
///
/// `history` is in newest-first order. The first time we see an
/// executable's name as a history entry's first word becomes its
/// recency rank — earlier in the list = more recent = better.
///
/// Per the user's directive: only candidates that are also on
/// `$PATH` appear — history doesn't add candidates, only re-
/// orders the structural list. A history-only command that the
/// user has since uninstalled (no longer on `$PATH`) vanishes
/// from completion.
pub fn complete_path_executables_with_history(
    prefix: &str,
    executables: &[String],
    history: &[String],
) -> Vec<CompletionCandidate> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    let exe_names: Vec<&String> = executables
        .iter()
        .filter(|name| name.starts_with(prefix))
        .filter(|name| seen.insert((*name).clone()))
        .collect();
    let mut recency: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, line) in history.iter().enumerate() {
        if let Some(first_word) = line.split_whitespace().next() {
            recency.entry(first_word).or_insert(i);
        }
    }
    let mut with_score: Vec<(&String, Option<usize>)> =
        exe_names.into_iter().map(|n| (n, recency.get(n.as_str()).copied())).collect();
    with_score.sort_by(|a, b| match (a.1, b.1) {
        (Some(ai), Some(bi)) => ai.cmp(&bi).then_with(|| a.0.cmp(b.0)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.0.cmp(b.0),
    });
    with_score
        .into_iter()
        .map(|(name, _)| {
            CompletionCandidate::simple(name.clone(), CompletionSource::PathExecutable)
        })
        .collect()
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

    // ---- completion_context (quote / escape aware) -----------------

    #[test]
    fn ctx_unquoted_word_matches_plain_token() {
        let c = completion_context("ls foo", 6);
        assert_eq!(c.start, 3);
        assert_eq!(c.prefix, "foo");
        assert_eq!(c.quote, None);
    }

    #[test]
    fn ctx_open_double_quote_starts_after_quote_and_keeps_spaces() {
        // `ls "my fi` — the opening `"` bounds the token; the space
        // inside it does NOT break the token.
        let text = "ls \"my fi";
        let c = completion_context(text, text.len());
        assert_eq!(c.start, 4); // right after the opening quote
        assert_eq!(c.prefix, "my fi");
        assert_eq!(c.quote, Some('"'));
    }

    #[test]
    fn ctx_open_single_quote_starts_after_quote() {
        let text = "cat 'my fi";
        let c = completion_context(text, text.len());
        assert_eq!(c.start, 5);
        assert_eq!(c.prefix, "my fi");
        assert_eq!(c.quote, Some('\''));
    }

    #[test]
    fn ctx_cursor_right_after_opening_quote_is_empty_prefix() {
        let text = "ls \"";
        let c = completion_context(text, text.len());
        assert_eq!(c.start, 4);
        assert_eq!(c.prefix, "");
        assert_eq!(c.quote, Some('"'));
    }

    #[test]
    fn ctx_unquoted_escaped_space_is_one_token_unescaped_prefix() {
        // `ls my\ fi` — the escaped space keeps the token together;
        // the matching prefix is unescaped.
        let text = "ls my\\ fi";
        let c = completion_context(text, text.len());
        assert_eq!(c.start, 3); // at 'm', includes the whole escaped token
        assert_eq!(c.prefix, "my fi");
        assert_eq!(c.quote, None);
    }

    #[test]
    fn ctx_closed_quote_then_new_unquoted_token() {
        // `ls "a b" c` — the quote is closed; the current token is the
        // fresh unquoted `c`.
        let text = "ls \"a b\" c";
        let c = completion_context(text, text.len());
        assert_eq!(c.prefix, "c");
        assert_eq!(c.quote, None);
        assert_eq!(c.start, text.len() - 1);
    }

    #[test]
    fn ctx_quoted_path_splits_on_inner_slash() {
        let text = "ls \"my dir/fi";
        let c = completion_context(text, text.len());
        assert_eq!(c.prefix, "my dir/fi");
        assert_eq!(c.quote, Some('"'));
        assert_eq!(c.start, 4);
    }

    // ---- escape_for_context ----------------------------------------

    #[test]
    fn escape_unquoted_backslashes_spaces() {
        assert_eq!(escape_for_context("my file.txt", None), "my\\ file.txt");
    }

    #[test]
    fn escape_unquoted_leaves_plain_name_alone() {
        assert_eq!(escape_for_context("simple.txt", None), "simple.txt");
    }

    #[test]
    fn escape_unquoted_keeps_path_separators() {
        assert_eq!(escape_for_context("src/my file", None), "src/my\\ file");
    }

    #[test]
    fn escape_unquoted_escapes_shell_metachars() {
        assert_eq!(escape_for_context("a(b)*", None), "a\\(b\\)\\*");
    }

    #[test]
    fn escape_double_quote_keeps_spaces_literal() {
        assert_eq!(escape_for_context("my file.txt", Some('"')), "my file.txt");
    }

    #[test]
    fn escape_double_quote_escapes_dollar_and_quote() {
        assert_eq!(escape_for_context("a\"b$c", Some('"')), "a\\\"b\\$c");
    }

    #[test]
    fn escape_single_quote_passes_through() {
        assert_eq!(escape_for_context("my file.txt", Some('\'')), "my file.txt");
    }

    // ---- unescape (canonicalising sidecar values) ------------------

    #[test]
    fn unescape_collapses_backslash_space() {
        assert_eq!(unescape("Application\\ Scripts"), "Application Scripts");
    }

    #[test]
    fn unescape_is_noop_on_plain_value() {
        assert_eq!(unescape("Application Scripts"), "Application Scripts");
    }

    #[test]
    fn unescape_collapses_doubled_backslash_to_one() {
        assert_eq!(unescape("a\\\\b"), "a\\b");
    }

    #[test]
    fn unescape_then_escape_is_idempotent_on_escaped_input() {
        // The mixed-form problem: raw and pre-escaped must both canonicalise
        // to the same literal, so re-escaping yields ONE consistent value.
        let raw = "Application Support";
        let pre = "Application\\ Support";
        assert_eq!(unescape(raw), unescape(pre));
        assert_eq!(escape_for_context(&unescape(pre), None), "Application\\ Support");
    }

    #[test]
    fn unescape_keeps_trailing_lone_backslash() {
        assert_eq!(unescape("foo\\"), "foo\\");
    }

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

    #[test]
    fn inline_env_assignment_keeps_command_position() {
        // `a=b ec<TAB>` — `ec` is the command, not an argument to `a=b`.
        assert!(is_command_position("a=b ec", 6));
        // Multiple assignments.
        assert!(is_command_position("A=1 B=2 ec", 10));
        // Assignment with a path value still leaves the next word a command.
        assert!(is_command_position("PATH=/x/y gi", 12));
    }

    #[test]
    fn argument_after_env_prefixed_command_is_not_command_position() {
        // `a=b echo ar<TAB>` — `ar` is an argument to `echo`.
        assert!(!is_command_position("a=b echo ar", 11));
    }

    #[test]
    fn bare_word_with_no_equals_is_not_an_assignment() {
        assert!(!is_command_position("notassign ec", 12));
    }

    // ---- env-var completion ---------------------------------------

    #[test]
    fn env_var_prefix_detects_dollar_tokens() {
        assert_eq!(env_var_prefix("$"), Some(""));
        assert_eq!(env_var_prefix("$HO"), Some("HO"));
        assert_eq!(env_var_prefix("$HOME"), Some("HOME"));
        // A path expansion is NOT name completion.
        assert_eq!(env_var_prefix("$HOME/sub"), None);
        // Not a `$`-token.
        assert_eq!(env_var_prefix("HOME"), None);
        assert_eq!(env_var_prefix("./x"), None);
    }

    #[test]
    fn complete_env_vars_filters_sorts_and_prefixes_dollar() {
        let names = vec![
            "HOME".to_string(),
            "HOSTNAME".to_string(),
            "PATH".to_string(),
            "HOME".to_string(), // dup
        ];
        // `$` → all (deduped, sorted, each `$`-prefixed).
        let all = complete_env_vars("", &names);
        let values: Vec<&str> = all.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["$HOME", "$HOSTNAME", "$PATH"]);
        assert!(all.iter().all(|c| c.source == CompletionSource::EnvVar));

        // `HO` → HOME, HOSTNAME only.
        let ho = complete_env_vars("HO", &names);
        let values: Vec<&str> = ho.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["$HOME", "$HOSTNAME"]);
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
    fn split_path_token_tilde_alone_treats_as_home() {
        // `~` alone is "list home dir" — same shape as `~/`
        // but without the trailing slash.
        assert_eq!(split_path_token("~"), ("~", ""));
        assert_eq!(split_path_token("~/foo"), ("~", "foo"));
    }

    #[test]
    fn split_path_token_env_var_alone_falls_through_as_filename() {
        // `$HOME` without a trailing `/` is the user still typing
        // the var name; we don't trigger expansion until they
        // commit it with `/`.
        assert_eq!(split_path_token("$HOME"), ("", "$HOME"));
    }

    #[test]
    fn split_path_token_env_var_with_trailing_slash_is_a_dir_token() {
        assert_eq!(split_path_token("$HOME/"), ("$HOME", ""));
        assert_eq!(split_path_token("$HOME/Doc"), ("$HOME", "Doc"));
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

    // ---- complete_path_executables_with_history -----------------

    #[test]
    fn history_recency_brings_recently_used_command_to_top() {
        // exes [git, gh, grep]; history (newest first):
        // "gh status" — gh used.
        // "git push"  — git used.
        // "grep foo"  — grep used.
        // Expected order (newest first): gh, git, grep.
        let exes: Vec<String> = vec!["git".into(), "gh".into(), "grep".into()];
        let history: Vec<String> = vec!["gh status".into(), "git push".into(), "grep foo".into()];
        let out = complete_path_executables_with_history("g", &exes, &history);
        let names: Vec<&str> = out.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(names, vec!["gh", "git", "grep"]);
    }

    #[test]
    fn history_first_word_only_args_dont_count() {
        // "echo git" should boost `echo`, NOT `git`. Here history
        // only contains "echo git", and only `git` is on $PATH
        // matching the prefix — so git gets no history rank and
        // sorts purely alphabetical (just git here).
        let exes: Vec<String> = vec!["git".into()];
        let history: Vec<String> = vec!["echo git".into()];
        let out = complete_path_executables_with_history("g", &exes, &history);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, "git");
    }

    #[test]
    fn history_ranked_above_non_ranked() {
        // exes [git, gh]; only git appears in history. git should
        // sort above gh even though alphabetically gh < git.
        let exes: Vec<String> = vec!["gh".into(), "git".into()];
        let history: Vec<String> = vec!["git status".into()];
        let out = complete_path_executables_with_history("g", &exes, &history);
        let names: Vec<&str> = out.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(names, vec!["git", "gh"]);
    }

    #[test]
    fn history_includes_only_path_executables() {
        // History mentions "mycmd" first-word but it's not on
        // $PATH. The completion popup must NOT include it.
        let exes: Vec<String> = vec!["git".into()];
        let history: Vec<String> = vec!["mycmd arg".into()];
        let out = complete_path_executables_with_history("m", &exes, &history);
        assert!(out.is_empty());
    }

    #[test]
    fn history_repeated_uses_first_occurrence_index() {
        // exes [git, gh]; history: "gh", "git", "gh" again.
        // The repeated `gh` shouldn't push it lower — first
        // occurrence is index 0 (most recent).
        let exes: Vec<String> = vec!["git".into(), "gh".into()];
        let history: Vec<String> = vec!["gh status".into(), "git log".into(), "gh pr list".into()];
        let out = complete_path_executables_with_history("g", &exes, &history);
        let names: Vec<&str> = out.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(names, vec!["gh", "git"]);
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
