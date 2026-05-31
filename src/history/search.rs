//! Ranking + filtering for the `^R` history overlay.
//!
//! Word-split substring matching, case-insensitive. The query is
//! whitespace-split: `"echo that"` requires both `echo` AND `that`
//! to appear in the command text (any order, anywhere). Within
//! that filter, results sort by, in order:
//!
//!   1. **cwd proximity** — entries whose recorded cwd equals the
//!      pane's current cwd come first.
//!   2. **Whole-word match count** — query words that land at a
//!      word boundary (ASCII alphanumeric `_`-aware) rank above
//!      ones that only matched as a substring. More hits is better.
//!   3. **In-order appearance** — when the query words appear in
//!      the text in the same order as in the query, that's a small
//!      additional boost.
//!   4. **Recency** — `started_at_ms` descending; final tiebreak.
//!
//! Cwd is not searched against the query — `^R` filters by what
//! the user typed, not by where they were.
//!
//! `nucleo` (spec/07's "v1 candidate") arrives in a follow-up; the
//! interface here is small enough that the swap is contained.

use std::cmp::Ordering;

use crate::history::Entry;
use crate::history_overlay::split_query_words;

/// Rank `entries` against `query`, optionally biasing toward
/// matches whose `cwd` equals `current_cwd`. Returns indices into
/// the input slice, best-first, dropped to `limit` rows.
pub fn rank(entries: &[Entry], query: &str, current_cwd: Option<&str>, limit: usize) -> Vec<usize> {
    let words = split_query_words(query);
    let mut hits: Vec<(usize, Score, i64)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let text_lc = e.text.to_lowercase();
            // Every query word must appear somewhere (AND).
            if !words.iter().all(|w| text_lc.contains(w.as_str())) {
                return None;
            }
            let cwd_match = matches!(
                (current_cwd, e.cwd.as_deref()),
                (Some(a), Some(b)) if a == b
            );
            let whole_word_count: i32 =
                words.iter().map(|w| count_whole_word_hits(&text_lc, w) as i32).sum();
            let in_order = words_appear_in_order(&text_lc, &words);
            Some((i, Score { cwd_match, whole_word_count, in_order }, e.started_at_ms))
        })
        .collect();
    hits.sort_by(|a, b| score_cmp(&a.1, &b.1).then(b.2.cmp(&a.2)));
    hits.into_iter().take(limit).map(|(i, _, _)| i).collect()
}

/// Ranking dimensions other than recency. Each field is a "more
/// is better" / "true is better" axis; [`score_cmp`] threads them
/// in priority order with `b.cmp(&a)` so higher values sort earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Score {
    cwd_match: bool,
    whole_word_count: i32,
    in_order: bool,
}

fn score_cmp(a: &Score, b: &Score) -> Ordering {
    b.cwd_match
        .cmp(&a.cwd_match)
        .then(b.whole_word_count.cmp(&a.whole_word_count))
        .then(b.in_order.cmp(&a.in_order))
}

/// Count occurrences of `word` in `text_lc` where both edges sit
/// on a word boundary — i.e. the char before the start (or
/// start-of-string) AND the char after the end (or end-of-string)
/// are NOT ASCII word characters. Lowercase-only call site, so we
/// don't case-fold here.
fn count_whole_word_hits(text_lc: &str, word: &str) -> usize {
    if word.is_empty() {
        return 0;
    }
    let bytes = text_lc.as_bytes();
    let w = word.as_bytes();
    let mut count = 0usize;
    let mut i = 0;
    while i + w.len() <= bytes.len() {
        if bytes[i..i + w.len()] == *w {
            let left_ok = i == 0 || !is_ascii_word_byte(bytes[i - 1]);
            let right_ok = i + w.len() == bytes.len() || !is_ascii_word_byte(bytes[i + w.len()]);
            if left_ok && right_ok {
                count += 1;
            }
            i += w.len();
        } else {
            i += 1;
        }
    }
    count
}

fn is_ascii_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True when each query word appears in `text_lc` and they appear
/// in the same order as `words`. Implementation: walk forward,
/// `find` each word in the remaining tail.
fn words_appear_in_order(text_lc: &str, words: &[String]) -> bool {
    let mut cursor = 0;
    for w in words {
        match text_lc[cursor..].find(w.as_str()) {
            Some(rel) => cursor += rel + w.len(),
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, text: &str, ts: i64, cwd: Option<&str>) -> Entry {
        Entry {
            id,
            text: text.to_string(),
            started_at_ms: ts,
            finished_at_ms: None,
            exit_code: None,
            cwd: cwd.map(|s| s.to_string()),
            app_run_id: None,
            pane_id: None,
            source: "termica".to_string(),
        }
    }

    #[test]
    fn empty_query_passes_everything_through_in_input_order() {
        let entries = vec![entry(1, "ls", 300, None), entry(2, "cd", 200, None)];
        let got = rank(&entries, "", None, 10);
        assert_eq!(got, vec![0, 1]);
    }

    #[test]
    fn substring_match_is_case_insensitive() {
        let entries =
            vec![entry(1, "cargo TEST --workspace", 100, None), entry(2, "ls", 200, None)];
        let got = rank(&entries, "test", None, 10);
        assert_eq!(got, vec![0]);
    }

    #[test]
    fn non_matches_drop_out() {
        let entries = vec![entry(1, "ls", 100, None), entry(2, "cd ..", 200, None)];
        let got = rank(&entries, "cargo", None, 10);
        assert!(got.is_empty());
    }

    #[test]
    fn cwd_match_outranks_more_recent_non_cwd_match() {
        // Two entries both match "cargo"; the older one is in the
        // current cwd; without the boost, the newer one would win.
        let entries = vec![
            entry(1, "cargo build", 100, Some("/proj")),
            entry(2, "cargo build", 200, Some("/elsewhere")),
        ];
        let got = rank(&entries, "cargo", Some("/proj"), 10);
        assert_eq!(got, vec![0, 1]);
    }

    #[test]
    fn within_same_cwd_class_recency_breaks_ties() {
        let entries = vec![
            entry(1, "cargo a", 100, Some("/p")),
            entry(2, "cargo b", 200, Some("/p")),
            entry(3, "cargo c", 50, Some("/p")),
        ];
        let got = rank(&entries, "cargo", Some("/p"), 10);
        // Newer first.
        let texts: Vec<&str> = got.iter().map(|i| entries[*i].text.as_str()).collect();
        assert_eq!(texts, vec!["cargo b", "cargo a", "cargo c"]);
    }

    #[test]
    fn limit_is_honored() {
        let entries: Vec<_> =
            (0..20).map(|i| entry(i, &format!("cmd {i}"), 100 + i, None)).collect();
        let got = rank(&entries, "cmd", None, 5);
        assert_eq!(got.len(), 5);
    }

    #[test]
    fn no_current_cwd_means_no_cwd_boost() {
        // Without a current_cwd, both entries score equally on
        // cwd_match (both `false`); recency wins.
        let entries = vec![entry(1, "x", 100, Some("/a")), entry(2, "x", 200, Some("/a"))];
        let got = rank(&entries, "x", None, 10);
        assert_eq!(got, vec![1, 0]);
    }

    #[test]
    fn query_does_not_match_against_cwd() {
        // Defensive: even though cwd is set to "/myproj", a query
        // of "myproj" must not surface an unrelated text.
        let entries = vec![entry(1, "ls", 100, Some("/myproj"))];
        let got = rank(&entries, "myproj", None, 10);
        assert!(got.is_empty());
    }

    // ---- word-split queries ---------------------------------------

    #[test]
    fn word_split_query_requires_every_word_to_match() {
        // "echo that" → both `echo` and `that` must appear.
        let entries = vec![
            entry(1, "echo this that the other", 300, None),
            entry(2, "echo this", 200, None),
            entry(3, "that thing", 100, None),
        ];
        let got = rank(&entries, "echo that", None, 10);
        assert_eq!(got, vec![0]);
    }

    #[test]
    fn whole_word_match_outranks_partial() {
        // Both contain "echo" as a substring; the first matches at
        // a word boundary, the second does not. Whole-word wins.
        let entries = vec![entry(1, "echoes the noise", 200, None), entry(2, "echo hi", 100, None)];
        let got = rank(&entries, "echo", None, 10);
        let texts: Vec<&str> = got.iter().map(|i| entries[*i].text.as_str()).collect();
        assert_eq!(texts, vec!["echo hi", "echoes the noise"]);
    }

    #[test]
    fn in_order_words_outrank_out_of_order() {
        // Both rows contain both words; the in-order one wins.
        let entries = vec![
            entry(1, "that thing echo more", 200, None),
            entry(2, "echo thing that more", 100, None),
        ];
        let got = rank(&entries, "echo that", None, 10);
        let texts: Vec<&str> = got.iter().map(|i| entries[*i].text.as_str()).collect();
        assert_eq!(texts, vec!["echo thing that more", "that thing echo more"]);
    }

    #[test]
    fn whole_word_hits_beat_partial_hits() {
        // Both rows satisfy the AND filter for "echo that".
        // Row 1: both words are at word boundaries → 2 whole-word
        //        hits + in-order.
        // Row 2: `echo` is whole-word but `that` is part of the
        //        bigger token `gotthat` → 1 whole-word hit; still
        //        in-order.
        // Whole-word count wins.
        let entries =
            vec![entry(1, "echo that more", 100, None), entry(2, "echo gotthat now", 200, None)];
        let got = rank(&entries, "echo that", None, 10);
        let texts: Vec<&str> = got.iter().map(|i| entries[*i].text.as_str()).collect();
        assert_eq!(texts, vec!["echo that more", "echo gotthat now"]);
    }

    #[test]
    fn extra_query_whitespace_is_ignored() {
        let entries = vec![entry(1, "cargo run", 200, None), entry(2, "cargo test", 100, None)];
        let got = rank(&entries, "  cargo   ", None, 10);
        assert_eq!(got.len(), 2);
    }
}
