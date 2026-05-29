//! Ranking + filtering for the `^R` history overlay.
//!
//! Substring matching with two soft boosts:
//!   1. **cwd proximity** — entries whose recorded cwd is the same
//!      as the pane's current cwd rank higher. Encourages "what
//!      did I run last in this project" without filtering it out
//!      of the result set.
//!   2. **recency** — newer matches break ties before older ones.
//!
//! Case-insensitive. The query string is matched against the
//! command text only (not cwd) — matching against cwd was
//! considered but rejected because users expect `^R` to filter
//! by what they typed, not by where they were.
//!
//! `nucleo` (the fuzzy matcher spec/07 lists as the v1 candidate)
//! arrives in a follow-up. The interface here is small enough that
//! the swap is contained.

use crate::history::Entry;

/// Rank `entries` against `query`, optionally biasing toward
/// matches whose `cwd` equals `current_cwd`. Returns indices into
/// the input slice, best-first, dropped to `limit` rows.
///
/// Two-pass: filter (drop non-matches), then sort by
/// `(cwd_match desc, started_at desc)`. Stable so equal-scored
/// rows preserve input order — the caller passes them
/// already-sorted-by-recency, so the result is recency-correct.
pub fn rank(entries: &[Entry], query: &str, current_cwd: Option<&str>, limit: usize) -> Vec<usize> {
    let needle = query.to_lowercase();
    let mut hits: Vec<(usize, bool, i64)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            if !needle.is_empty() && !e.text.to_lowercase().contains(&needle) {
                return None;
            }
            let cwd_match = matches!((current_cwd, e.cwd.as_deref()), (Some(a), Some(b)) if a == b);
            Some((i, cwd_match, e.started_at_ms))
        })
        .collect();
    hits.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
    hits.into_iter().take(limit).map(|(i, _, _)| i).collect()
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
}
