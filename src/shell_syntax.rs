//! In-house tokenizer for shell-syntax highlighting in the prompt
//! editor. Pure logic; no egui, no terminal state.
//!
//! Implements the minimal v1 token set called out in
//! [spec/04 §"Syntax highlighting"](../spec/04-prompt-editor.md#syntax-highlighting):
//! command, strings, variables, pipes / redirects, flags, comments,
//! plus a default `Word` for everything else. Subshells (`$(...)`,
//! `` `...` ``) and proper here-doc handling defer to a later pass.
//!
//! The tokenizer is a single-pass byte walker — shell metacharacters
//! are all ASCII, so byte indexing is safe. Multi-byte UTF-8 inside
//! words is preserved because non-ASCII bytes never satisfy any of
//! the "word-break" or "metachar" predicates.

#![forbid(unsafe_code)]

use std::ops::Range;

/// Token classification. The renderer maps each kind to a colour;
/// `Word` falls back to `EDITOR_FG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// The leading token of a logical command — `ls` in `ls -la`,
    /// `grep` after a pipe, etc.
    Command,
    /// Single- or double-quoted string. When a `$var` appears inside
    /// a double-quoted region, the tokenizer splits the string into
    /// `String`, `Variable`, `String` runs so the variable retains
    /// its own colour.
    String,
    /// `$NAME`, `${expr}` (bare or inside double quotes).
    Variable,
    /// `|`, `||`, `&`, `&&`, `;` — anything that resets "expecting
    /// command" to true for the next non-whitespace token.
    Pipe,
    /// `>`, `>>`, `<` — file redirection. The next word is a
    /// filename, not a command.
    Redirect,
    /// `-x`, `--long` — option flag for the current command.
    Flag,
    /// `#` to end-of-line. Same word-boundary rule as a normal
    /// shell: the `#` must be at the start of a token (preceded by
    /// whitespace, start of line, or a metachar).
    Comment,
    /// Anything that didn't match a more specific kind.
    Word,
}

/// One token in `text`, identified by a byte range and its kind.
/// Ranges never overlap and are emitted in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
}

impl Token {
    pub fn new(kind: TokenKind, range: Range<usize>) -> Self {
        Self { kind, range }
    }
}

/// Tokenize `text` into a sequence of [`Token`]s for syntax
/// highlighting. Whitespace is not emitted (no `Whitespace` kind);
/// gaps between tokens in the returned ranges paint as the editor's
/// default foreground.
pub fn tokenize(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0;
    let mut expecting_command = true;

    while i < bytes.len() {
        // Skip horizontal whitespace within a line.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let b = bytes[i];

        if b == b'\n' {
            // A multi-line buffer (`Shift+Enter`-authored) resets
            // the command-expectation each physical line — the user
            // is starting a fresh shell statement.
            i += 1;
            expecting_command = true;
            continue;
        }

        if b == b'#' {
            // `#` only counts as a comment marker when it's at a
            // token boundary (start of text or after whitespace /
            // metachar). Inline `#` inside e.g. `foo#bar` is part
            // of the word.
            if i == 0 || is_word_break(bytes[i - 1]) {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                tokens.push(Token::new(TokenKind::Comment, start..i));
                continue;
            }
        }

        if b == b'\'' {
            // Single-quote string: opaque until the matching '.
            // Shell rule says nothing inside a single-quoted string
            // is interpreted, so we don't scan for variables here.
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'\'' && bytes[i] != b'\n' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'\'' {
                i += 1;
            }
            tokens.push(Token::new(TokenKind::String, start..i));
            expecting_command = false;
            continue;
        }

        if b == b'"' {
            // Double-quote string. `$var` inside becomes its own
            // Variable token; the literal pieces around it stay
            // String. The opening / closing quotes are absorbed
            // into the first / last String run.
            let opening = i;
            i += 1;
            let mut literal_start = opening;
            while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\n' {
                if bytes[i] == b'$' {
                    if literal_start < i {
                        tokens.push(Token::new(TokenKind::String, literal_start..i));
                    }
                    i = consume_variable(bytes, i, &mut tokens);
                    literal_start = i;
                } else {
                    i += 1;
                }
            }
            // Absorb the closing quote (if any) into a trailing
            // String run, or open a fresh one if the last token
            // wasn't a String of this literal segment.
            let close = i;
            if close < bytes.len() && bytes[close] == b'"' {
                i += 1;
            }
            if literal_start < i {
                tokens.push(Token::new(TokenKind::String, literal_start..i));
            }
            expecting_command = false;
            continue;
        }

        if b == b'$' {
            i = consume_variable(bytes, i, &mut tokens);
            expecting_command = false;
            continue;
        }

        if b == b'|' {
            let start = i;
            i += 1;
            if i < bytes.len() && bytes[i] == b'|' {
                i += 1;
            }
            tokens.push(Token::new(TokenKind::Pipe, start..i));
            expecting_command = true;
            continue;
        }

        if b == b'&' {
            let start = i;
            i += 1;
            if i < bytes.len() && bytes[i] == b'&' {
                i += 1;
            }
            tokens.push(Token::new(TokenKind::Pipe, start..i));
            expecting_command = true;
            continue;
        }

        if b == b';' {
            tokens.push(Token::new(TokenKind::Pipe, i..i + 1));
            i += 1;
            expecting_command = true;
            continue;
        }

        if b == b'>' {
            let start = i;
            i += 1;
            if i < bytes.len() && bytes[i] == b'>' {
                i += 1;
            }
            tokens.push(Token::new(TokenKind::Redirect, start..i));
            // After a redirect the next word is a filename, not
            // the next command.
            expecting_command = false;
            continue;
        }

        if b == b'<' {
            tokens.push(Token::new(TokenKind::Redirect, i..i + 1));
            i += 1;
            expecting_command = false;
            continue;
        }

        if b == b'-' && (i == 0 || is_word_break(bytes[i - 1])) {
            // Flag run: `-x`, `--long`, `--long=value`, etc. Goes
            // until next whitespace / metachar.
            let start = i;
            while i < bytes.len() && !is_word_break(bytes[i]) {
                i += 1;
            }
            tokens.push(Token::new(TokenKind::Flag, start..i));
            expecting_command = false;
            continue;
        }

        // Default: a plain word. First word after a fresh statement
        // boundary is the Command; subsequent words are args.
        let start = i;
        while i < bytes.len() && !is_word_break(bytes[i]) {
            i += 1;
        }
        if start == i {
            // Defensive: shouldn't happen because we checked for
            // metacharacters above; advance one byte so we don't
            // loop forever on a surprise byte.
            i += 1;
            continue;
        }
        let kind = if expecting_command { TokenKind::Command } else { TokenKind::Word };
        tokens.push(Token::new(kind, start..i));
        expecting_command = false;
    }

    tokens
}

/// Consume `$NAME` or `${expr}` starting at `bytes[i]` (which must
/// equal `b'$'`). Emits one `Variable` token and returns the
/// position past it.
fn consume_variable(bytes: &[u8], i: usize, tokens: &mut Vec<Token>) -> usize {
    debug_assert_eq!(bytes[i], b'$');
    let start = i;
    let mut j = i + 1;
    if j < bytes.len() && bytes[j] == b'{' {
        j += 1;
        while j < bytes.len() && bytes[j] != b'}' && bytes[j] != b'\n' {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'}' {
            j += 1;
        }
    } else if j < bytes.len() && is_var_char(bytes[j]) {
        while j < bytes.len() && is_var_char(bytes[j]) {
            j += 1;
        }
    } else {
        // Bare `$` not followed by a name — treat just the `$` as
        // a variable so the user still sees their intent coloured.
    }
    tokens.push(Token::new(TokenKind::Variable, start..j));
    j
}

fn is_var_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_word_break(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\n' | b'|' | b'&' | b';' | b'<' | b'>' | b'\'' | b'"' | b'$' | b'#'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(tokens: &[Token]) -> Vec<TokenKind> {
        tokens.iter().map(|t| t.kind).collect()
    }

    fn slices<'a>(text: &'a str, tokens: &[Token]) -> Vec<&'a str> {
        tokens.iter().map(|t| &text[t.range.clone()]).collect()
    }

    // ---- empty / no-op cases ------------------------------------

    #[test]
    fn empty_text_yields_no_tokens() {
        assert_eq!(tokenize(""), Vec::<Token>::new());
    }

    #[test]
    fn whitespace_only_yields_no_tokens() {
        assert_eq!(tokenize("   \t  "), Vec::<Token>::new());
    }

    // ---- command + arg basics -----------------------------------

    #[test]
    fn lone_command_word_is_classified_as_command() {
        let t = tokenize("ls");
        assert_eq!(kinds(&t), vec![TokenKind::Command]);
        assert_eq!(slices("ls", &t), vec!["ls"]);
    }

    #[test]
    fn second_word_is_arg_not_command() {
        let t = tokenize("ls foo");
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Word]);
        assert_eq!(slices("ls foo", &t), vec!["ls", "foo"]);
    }

    #[test]
    fn command_with_flag() {
        let t = tokenize("ls -la");
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Flag]);
        assert_eq!(slices("ls -la", &t), vec!["ls", "-la"]);
    }

    #[test]
    fn long_flag_with_equals() {
        let t = tokenize("git --no-pager log");
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Flag, TokenKind::Word]);
    }

    // ---- pipes / redirects --------------------------------------

    #[test]
    fn pipe_starts_a_new_command_scope() {
        let s = "ls | grep foo";
        let t = tokenize(s);
        assert_eq!(
            kinds(&t),
            vec![TokenKind::Command, TokenKind::Pipe, TokenKind::Command, TokenKind::Word]
        );
        assert_eq!(slices(s, &t), vec!["ls", "|", "grep", "foo"]);
    }

    #[test]
    fn double_pipe_logical_or_starts_command_scope() {
        let s = "false || true";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Pipe, TokenKind::Command]);
        assert_eq!(slices(s, &t), vec!["false", "||", "true"]);
    }

    #[test]
    fn double_amp_logical_and_starts_command_scope() {
        let s = "cd /tmp && ls";
        let t = tokenize(s);
        assert_eq!(
            kinds(&t),
            vec![TokenKind::Command, TokenKind::Word, TokenKind::Pipe, TokenKind::Command]
        );
    }

    #[test]
    fn semicolon_starts_command_scope() {
        let s = "a; b";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Pipe, TokenKind::Command]);
    }

    #[test]
    fn append_redirect_is_two_chars() {
        let s = "echo hi >> out";
        let t = tokenize(s);
        assert_eq!(
            kinds(&t),
            vec![TokenKind::Command, TokenKind::Word, TokenKind::Redirect, TokenKind::Word]
        );
        assert_eq!(slices(s, &t)[2], ">>");
    }

    #[test]
    fn redirect_followed_by_filename_is_word_not_command() {
        // The filename after `>` mustn't be classified as the next
        // command — that would imply a new pipeline scope.
        let t = tokenize("echo > out");
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Redirect, TokenKind::Word]);
    }

    // ---- strings -------------------------------------------------

    #[test]
    fn single_quoted_string_is_one_token_including_quotes() {
        let s = "echo 'hi there'";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::String]);
        assert_eq!(slices(s, &t)[1], "'hi there'");
    }

    #[test]
    fn single_quoted_string_does_not_interpolate_variables() {
        // Inside '…' nothing is interpreted — $USER stays string.
        let s = "echo '$USER'";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::String]);
        assert_eq!(slices(s, &t)[1], "'$USER'");
    }

    #[test]
    fn double_quoted_string_with_no_variable_is_one_token() {
        let s = "echo \"hi\"";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::String]);
        assert_eq!(slices(s, &t)[1], "\"hi\"");
    }

    #[test]
    fn double_quoted_string_splits_around_variable() {
        let s = "echo \"hi $USER\"";
        let t = tokenize(s);
        // Expected: Command(echo), String("\"hi "), Variable($USER), String("\"").
        assert_eq!(
            kinds(&t),
            vec![TokenKind::Command, TokenKind::String, TokenKind::Variable, TokenKind::String]
        );
        let slices = slices(s, &t);
        assert_eq!(slices[1], "\"hi ");
        assert_eq!(slices[2], "$USER");
        assert_eq!(slices[3], "\"");
    }

    #[test]
    fn unterminated_single_quote_runs_to_end_of_line() {
        let s = "echo 'oops";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::String]);
        assert_eq!(slices(s, &t)[1], "'oops");
    }

    // ---- variables -----------------------------------------------

    #[test]
    fn bare_dollar_name_is_variable() {
        let s = "echo $HOME";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Variable]);
        assert_eq!(slices(s, &t)[1], "$HOME");
    }

    #[test]
    fn braced_variable_is_variable() {
        let s = "echo ${PATH}";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Variable]);
        assert_eq!(slices(s, &t)[1], "${PATH}");
    }

    // ---- comments -------------------------------------------------

    #[test]
    fn hash_at_start_is_comment_to_end_of_line() {
        let s = "# this is a comment";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Comment]);
        assert_eq!(slices(s, &t)[0], "# this is a comment");
    }

    #[test]
    fn hash_after_whitespace_is_comment() {
        let s = "ls # list";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Comment]);
        assert_eq!(slices(s, &t)[1], "# list");
    }

    #[test]
    fn hash_inside_word_is_not_a_comment() {
        let s = "foo#bar";
        let t = tokenize(s);
        // `foo#bar` is one Command word because `#` doesn't appear
        // after whitespace / metachar. The exact internal split
        // isn't fixed by the spec — we just guarantee it isn't a
        // Comment.
        assert!(t.iter().all(|tk| tk.kind != TokenKind::Comment));
    }

    // ---- multi-line ----------------------------------------------

    #[test]
    fn newline_resets_command_scope() {
        // `Shift+Enter` produces a literal '\n' in the buffer; each
        // line gets its own Command.
        let s = "ls\necho hi";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Command, TokenKind::Word]);
    }

    // ---- UTF-8 safety --------------------------------------------

    #[test]
    fn multibyte_utf8_inside_word_is_preserved() {
        // The Greek lambda is two UTF-8 bytes. The tokenizer must
        // keep the word intact and the byte range must align.
        let s = "echo λ";
        let t = tokenize(s);
        assert_eq!(kinds(&t), vec![TokenKind::Command, TokenKind::Word]);
        let lambda = &s[t[1].range.clone()];
        assert_eq!(lambda, "λ");
    }

    // ---- non-overlapping invariant --------------------------------

    #[test]
    fn token_ranges_are_non_overlapping_and_in_order() {
        // Tokens should always be emitted in source order, with no
        // overlap (gaps OK — those are whitespace).
        let inputs = [
            "ls",
            "ls -la",
            "echo 'hi'",
            "git status | grep foo",
            "echo $USER > /tmp/out",
            "echo \"hi $USER bye\"",
            "# comment only",
            "a && b || c; d",
        ];
        for s in inputs {
            let t = tokenize(s);
            let mut prev_end = 0;
            for tk in &t {
                assert!(
                    tk.range.start >= prev_end,
                    "overlap in {s:?}: token {tk:?} after prev_end={prev_end}"
                );
                assert!(tk.range.end > tk.range.start, "empty range in {s:?}: {tk:?}");
                assert!(tk.range.end <= s.len());
                prev_end = tk.range.end;
            }
        }
    }
}
