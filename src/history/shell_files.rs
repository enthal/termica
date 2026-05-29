//! Parsers for the shell-history files we replay into the `runs`
//! table on startup. One parser per supported shell — the format
//! difference is wide enough that a single combined parser would
//! be brittle.
//!
//! All parsers take raw bytes (these files are nominally UTF-8 but
//! some encode metacharacters as `\M-^I`-style escapes; we treat
//! them as best-effort UTF-8 with a lossy fallback) and emit a flat
//! `Vec<ParsedEntry>` in file order.
//!
//! Parsers do not deduplicate against the database; that's the
//! replay loop's job (PR 3). They also do not synthesize timestamps
//! for entries the file doesn't carry one for (`started_at_ms` is
//! `None`); the replay loop decides what to substitute (file mtime,
//! row-zero anchor, etc.).
//!
//! Strict-layer per [CLAUDE.md](../../CLAUDE.md) and
//! [spec/09](../../spec/09-testing.md): these are persistence-feeding
//! parsers, so tests come first.

/// One row parsed out of a shell-history file. The replay loop
/// turns this into a `runs` row via [`crate::history::HistoryStore::record_replayed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEntry {
    /// The command text, exactly as the user typed it (including
    /// embedded newlines for multi-line commands).
    pub text: String,
    /// Unix-epoch milliseconds when the file recorded the entry,
    /// when the format carries it. `None` for formats / configurations
    /// that don't (e.g. bash without `HISTTIMEFORMAT`).
    pub started_at_ms: Option<i64>,
}

/// Parse zsh's `~/.zsh_history` format.
///
/// Two on-disk shapes:
///
/// - **Extended history** (the default since the user enabled
///   `extended_history`, which most shipped configurations do):
///   `: <unix-ts>:<elapsed>;<command>`. Multi-line commands have
///   their inner newlines escaped as `\\` + newline in the file;
///   we de-escape them back to real newlines.
/// - **Plain history**: just `<command>` per line. Same backslash-
///   continuation rule.
///
/// Implementation reads line-by-line and folds continuation lines
/// (where the previous line ended in a literal backslash) into the
/// current entry. The `: <ts>:<el>;` prefix is sniffed by checking
/// for `b": "` at the start of an entry-starting line.
pub fn parse_zsh_history(bytes: &[u8]) -> Vec<ParsedEntry> {
    let text = String::from_utf8_lossy(bytes);
    let mut out: Vec<ParsedEntry> = Vec::new();
    let mut current: Option<String> = None;
    let mut current_ts: Option<i64> = None;
    for raw_line in text.split_inclusive('\n') {
        // Strip trailing newline if present (split_inclusive keeps it).
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        if let Some(prev) = current.as_mut()
            && prev.ends_with('\\')
        {
            // Continuation: the previous line ended in `\` so this
            // line is part of the same logical command. Drop the
            // backslash, re-insert a real newline.
            prev.pop(); // remove '\'
            prev.push('\n');
            prev.push_str(line);
            continue;
        }
        // Not a continuation. Flush whatever we accumulated.
        if let Some(t) = current.take() {
            out.push(ParsedEntry { text: t, started_at_ms: current_ts });
            current_ts = None;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(": ") {
            // Extended-history line: `: ts:elapsed;command`.
            if let Some((ts_elapsed, cmd)) = rest.split_once(';') {
                let ts = ts_elapsed
                    .split_once(':')
                    .and_then(|(ts, _el)| ts.parse::<i64>().ok())
                    .map(|s| s.saturating_mul(1_000));
                current = Some(cmd.to_string());
                current_ts = ts;
                continue;
            }
        }
        // Plain-history line.
        current = Some(line.to_string());
        current_ts = None;
    }
    if let Some(t) = current.take() {
        out.push(ParsedEntry { text: t, started_at_ms: current_ts });
    }
    out
}

/// Parse bash's `~/.bash_history` format.
///
/// Two on-disk shapes:
///
/// - **Plain** (no `HISTTIMEFORMAT` in the user's shell config):
///   one command per line. Multi-line commands written via
///   `cmdhist` are joined with `;` so each entry stays on its own
///   line — we don't need a backslash-continuation rule.
/// - **Timestamped**: each command is preceded by a `#<unix-ts>`
///   line. Bash writes these only when `HISTTIMEFORMAT` is set.
pub fn parse_bash_history(bytes: &[u8]) -> Vec<ParsedEntry> {
    let text = String::from_utf8_lossy(bytes);
    let mut out: Vec<ParsedEntry> = Vec::new();
    let mut pending_ts: Option<i64> = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#')
            && let Ok(ts) = rest.parse::<i64>()
        {
            pending_ts = Some(ts.saturating_mul(1_000));
            continue;
        }
        out.push(ParsedEntry { text: line.to_string(), started_at_ms: pending_ts.take() });
    }
    out
}

/// Parse fish's `~/.local/share/fish/fish_history` format.
///
/// YAML-ish, one entry per stanza:
///
/// ```text
/// - cmd: ls -la
///   when: 1700000000
///   paths:
///     - /home/me/proj
/// ```
///
/// We only need `cmd` + `when`. The `paths:` block is ignored.
/// fish escapes embedded newlines in `cmd` as `\n` (the two-byte
/// sequence), which we de-escape.
pub fn parse_fish_history(bytes: &[u8]) -> Vec<ParsedEntry> {
    let text = String::from_utf8_lossy(bytes);
    let mut out: Vec<ParsedEntry> = Vec::new();
    let mut current_cmd: Option<String> = None;
    let mut current_when: Option<i64> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("- cmd: ") {
            if let Some(cmd) = current_cmd.take() {
                out.push(ParsedEntry {
                    text: cmd,
                    started_at_ms: current_when.take().map(|s| s.saturating_mul(1_000)),
                });
            }
            current_cmd = Some(unescape_fish(rest));
        } else if let Some(rest) = line.trim_start().strip_prefix("when: ")
            && current_cmd.is_some()
        {
            current_when = rest.parse::<i64>().ok();
        }
    }
    if let Some(cmd) = current_cmd.take() {
        out.push(ParsedEntry {
            text: cmd,
            started_at_ms: current_when.take().map(|s| s.saturating_mul(1_000)),
        });
    }
    out
}

fn unescape_fish(s: &str) -> String {
    // fish escapes the two characters `\n` (backslash + 'n') for an
    // embedded newline, and `\\` for a literal backslash. Decode in
    // a single pass without allocating an intermediate Vec.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // zsh
    // ---------------------------------------------------------------

    #[test]
    fn zsh_extended_single_entry() {
        let bytes = b": 1700000000:0;ls -la\n";
        let got = parse_zsh_history(bytes);
        assert_eq!(
            got,
            vec![ParsedEntry {
                text: "ls -la".to_string(),
                started_at_ms: Some(1_700_000_000_000)
            }]
        );
    }

    #[test]
    fn zsh_extended_two_entries() {
        let bytes = b": 1700000000:0;ls\n: 1700000005:0;cd ..\n";
        let got = parse_zsh_history(bytes);
        assert_eq!(
            got,
            vec![
                ParsedEntry { text: "ls".to_string(), started_at_ms: Some(1_700_000_000_000) },
                ParsedEntry { text: "cd ..".to_string(), started_at_ms: Some(1_700_000_005_000) },
            ]
        );
    }

    #[test]
    fn zsh_plain_single_entry() {
        let bytes = b"ls -la\n";
        let got = parse_zsh_history(bytes);
        assert_eq!(got, vec![ParsedEntry { text: "ls -la".to_string(), started_at_ms: None }]);
    }

    #[test]
    fn zsh_multiline_command_via_backslash_continuation() {
        // Real zsh history for `for i in 1 2 3; do\n  echo $i\ndone`
        // looks like: `: ts:0;for i in 1 2 3; do\\\n  echo $i\\\ndone`.
        let bytes = b": 1700000000:0;for i in 1 2 3; do\\\n  echo $i\\\ndone\n";
        let got = parse_zsh_history(bytes);
        assert_eq!(
            got,
            vec![ParsedEntry {
                text: "for i in 1 2 3; do\n  echo $i\ndone".to_string(),
                started_at_ms: Some(1_700_000_000_000),
            }]
        );
    }

    #[test]
    fn zsh_handles_empty_lines() {
        let bytes = b"\n: 1700000000:0;ls\n\n";
        let got = parse_zsh_history(bytes);
        assert_eq!(
            got,
            vec![ParsedEntry { text: "ls".to_string(), started_at_ms: Some(1_700_000_000_000) }]
        );
    }

    #[test]
    fn zsh_handles_missing_final_newline() {
        let bytes = b": 1700000000:0;ls";
        let got = parse_zsh_history(bytes);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "ls");
    }

    #[test]
    fn zsh_handles_unparseable_extended_prefix_as_plain() {
        // Looks like an extended line but no `;` separator.
        let bytes = b": broken-line-no-semicolon\n";
        let got = parse_zsh_history(bytes);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, ": broken-line-no-semicolon");
        assert_eq!(got[0].started_at_ms, None);
    }

    // ---------------------------------------------------------------
    // bash
    // ---------------------------------------------------------------

    #[test]
    fn bash_plain_one_entry_per_line() {
        let bytes = b"ls\ncd ..\necho hi\n";
        let got = parse_bash_history(bytes);
        let texts: Vec<_> = got.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["ls", "cd ..", "echo hi"]);
        assert!(got.iter().all(|e| e.started_at_ms.is_none()));
    }

    #[test]
    fn bash_timestamped_pairs_hash_lines_with_commands() {
        let bytes = b"#1700000000\nls\n#1700000005\ncd ..\n";
        let got = parse_bash_history(bytes);
        assert_eq!(
            got,
            vec![
                ParsedEntry { text: "ls".to_string(), started_at_ms: Some(1_700_000_000_000) },
                ParsedEntry { text: "cd ..".to_string(), started_at_ms: Some(1_700_000_005_000) },
            ]
        );
    }

    #[test]
    fn bash_orphan_timestamp_is_dropped_silently() {
        // `#1700000000` with no following command — bash never
        // writes this, but if a file is corrupted we should not panic.
        let bytes = b"#1700000000\n";
        let got = parse_bash_history(bytes);
        assert!(got.is_empty());
    }

    #[test]
    fn bash_command_starting_with_hash_is_not_treated_as_timestamp() {
        // `#` followed by non-digits is just a comment-looking
        // command. Bash itself never writes a non-numeric `#` line;
        // be tolerant.
        let bytes = b"#not-a-ts\nreal\n";
        let got = parse_bash_history(bytes);
        let texts: Vec<_> = got.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["#not-a-ts", "real"]);
    }

    // ---------------------------------------------------------------
    // fish
    // ---------------------------------------------------------------

    #[test]
    fn fish_two_entries_with_when() {
        let bytes = b"- cmd: ls -la\n  when: 1700000000\n- cmd: cd ..\n  when: 1700000005\n";
        let got = parse_fish_history(bytes);
        assert_eq!(
            got,
            vec![
                ParsedEntry { text: "ls -la".to_string(), started_at_ms: Some(1_700_000_000_000) },
                ParsedEntry { text: "cd ..".to_string(), started_at_ms: Some(1_700_000_005_000) },
            ]
        );
    }

    #[test]
    fn fish_ignores_paths_block() {
        let bytes = b"- cmd: ls\n  when: 1700000000\n  paths:\n    - /home/me\n    - /tmp\n";
        let got = parse_fish_history(bytes);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "ls");
        assert_eq!(got[0].started_at_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn fish_decodes_escaped_newline() {
        // fish writes embedded newlines as literal `\n` (backslash + 'n').
        let bytes = b"- cmd: line1\\nline2\n  when: 1700000000\n";
        let got = parse_fish_history(bytes);
        assert_eq!(got[0].text, "line1\nline2");
    }

    #[test]
    fn fish_decodes_escaped_backslash() {
        let bytes = b"- cmd: echo \\\\n\n  when: 1700000000\n";
        let got = parse_fish_history(bytes);
        assert_eq!(got[0].text, "echo \\n");
    }

    #[test]
    fn fish_entry_without_when_still_recorded() {
        // Defensive: a file with a `- cmd:` and no `when:` should
        // still yield the entry, with `started_at_ms = None`.
        let bytes = b"- cmd: ls\n";
        let got = parse_fish_history(bytes);
        assert_eq!(got, vec![ParsedEntry { text: "ls".to_string(), started_at_ms: None }]);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(parse_zsh_history(b"").is_empty());
        assert!(parse_bash_history(b"").is_empty());
        assert!(parse_fish_history(b"").is_empty());
    }
}
