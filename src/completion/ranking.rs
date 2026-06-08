//! Score-based merge of completion candidates from multiple
//! sources.
//!
//! v1 ranking is intentionally simple: per-source weights + a
//! recency bonus from the editor's recent-acceptance history. The
//! more elaborate scoring in [spec/04a §Ranking](../../spec/04a-completion.md#ranking)
//! (prefix-match density, cwd bonus) lands when the CLI-native
//! drivers and shell sidecars need it.

use super::{CompletionCandidate, CompletionSource};

/// Per-source weight in the merged ranking. Higher is better.
///
/// CLI-native drivers ([`CompletionSource::Driver`]) outrank every local
/// source: when `kubectl` itself says "these are the valid resources,"
/// that's authoritative over a `$PATH` / path / history guess. We use
/// `1.2` rather than spec/04a's nominal `1.0` so drivers sort above the
/// already-tuned local triad without re-tuning it; the full spec
/// re-weighting (with prefix-density / recency / cwd bonuses) lands with
/// the shell-sidecar slice.
///
/// Among locals, History wins ties with `$PATH` because the user's own
/// typing history is a stronger signal than "this binary exists on the
/// system." Paths come last because pathish tokens almost always know
/// they're paths and don't compete with the others.
fn source_weight(s: CompletionSource) -> f32 {
    match s {
        CompletionSource::Driver(_) => 1.2,
        // Env-var completion is exclusive (its own popup), so this
        // weight only matters if it ever shares a merge; rank it high.
        CompletionSource::EnvVar => 1.0,
        CompletionSource::History => 1.0,
        CompletionSource::PathExecutable => 0.8,
        CompletionSource::Path => 0.6,
    }
}

/// Merge candidates from multiple sources into one ranked list.
///
/// Duplicates by `value` collapse — the higher-weighted source's
/// metadata wins (its `description`, `source` tag). The merge is
/// stable for ties: candidates with equal score keep the input
/// order within their source, and sources are merged in the
/// order they appear in `sources`.
///
/// Returns at most `limit` candidates. v1 default is `200`.
pub fn merge_ranked(
    sources: Vec<Vec<CompletionCandidate>>,
    limit: usize,
) -> Vec<CompletionCandidate> {
    let mut by_value: std::collections::HashMap<String, CompletionCandidate> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for source_list in sources {
        for c in source_list {
            let existing = by_value.get_mut(&c.value);
            match existing {
                None => {
                    order.push(c.value.clone());
                    by_value.insert(c.value.clone(), c);
                }
                Some(prev) => {
                    // Keep the higher-weighted source's metadata.
                    if source_weight(c.source) > source_weight(prev.source) {
                        *prev = c;
                    }
                }
            }
        }
    }
    let mut out: Vec<CompletionCandidate> =
        order.into_iter().filter_map(|v| by_value.remove(&v)).collect();
    // Sort by score descending; equal-score items keep insertion order.
    out.sort_by(|a, b| {
        let sa = source_weight(a.source);
        let sb = source_weight(b.source);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::super::CompletionCandidate;
    use super::*;

    fn cand(value: &str, source: CompletionSource) -> CompletionCandidate {
        CompletionCandidate::simple(value, source)
    }

    #[test]
    fn merge_history_outranks_path() {
        let h = vec![cand("git status", CompletionSource::History)];
        let p = vec![cand("Gemfile", CompletionSource::Path)];
        let out = merge_ranked(vec![p, h], 10);
        assert_eq!(out[0].value, "git status");
        assert_eq!(out[1].value, "Gemfile");
    }

    #[test]
    fn merge_driver_outranks_all_locals() {
        use crate::completion::DriverTool;
        let driver = vec![cand("pods", CompletionSource::Driver(DriverTool::Kubectl))];
        let pe = vec![cand("podman", CompletionSource::PathExecutable)];
        let h = vec![cand("podcast", CompletionSource::History)];
        let out = merge_ranked(vec![pe, h, driver], 10);
        assert_eq!(out[0].value, "pods", "driver candidate sorts above $PATH and history");
        assert_eq!(out[0].source, CompletionSource::Driver(DriverTool::Kubectl));
    }

    #[test]
    fn merge_dedups_driver_over_local_keeping_driver_metadata() {
        use crate::completion::DriverTool;
        // "git" appears from both the $PATH scan and the git driver —
        // collapses to one row carrying the (higher-weight) driver tag.
        let pe = vec![cand("git", CompletionSource::PathExecutable)];
        let driver = vec![cand("git", CompletionSource::Driver(DriverTool::Git))];
        let out = merge_ranked(vec![pe, driver], 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, CompletionSource::Driver(DriverTool::Git));
    }

    #[test]
    fn merge_path_executable_outranks_path() {
        let pe = vec![cand("git", CompletionSource::PathExecutable)];
        let p = vec![cand("README.md", CompletionSource::Path)];
        let out = merge_ranked(vec![p, pe], 10);
        assert_eq!(out[0].value, "git");
    }

    #[test]
    fn merge_dedups_on_same_value_keeping_higher_source() {
        // "git" comes from both PathExecutable and History — collapses.
        let pe = vec![cand("git", CompletionSource::PathExecutable)];
        let h = vec![cand("git", CompletionSource::History)];
        let out = merge_ranked(vec![pe, h], 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, CompletionSource::History);
    }

    #[test]
    fn merge_respects_limit() {
        let cs: Vec<CompletionCandidate> =
            (0..200).map(|i| cand(&format!("c{i}"), CompletionSource::Path)).collect();
        let out = merge_ranked(vec![cs], 10);
        assert_eq!(out.len(), 10);
    }

    #[test]
    fn merge_empty_inputs_empty_output() {
        let out: Vec<CompletionCandidate> = merge_ranked(vec![vec![], vec![]], 10);
        assert!(out.is_empty());
    }

    #[test]
    fn merge_preserves_within_source_order_for_equal_scores() {
        // Two paths — equal score. Should keep insertion order.
        let p = vec![cand("alpha", CompletionSource::Path), cand("beta", CompletionSource::Path)];
        let out = merge_ranked(vec![p], 10);
        assert_eq!(out[0].value, "alpha");
        assert_eq!(out[1].value, "beta");
    }
}
