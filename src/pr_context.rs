//! GitHub pull-request context for the block-header PR chip
//! (4G-async-context): the open PR number for the pane's branch and a
//! rolled-up CI status to color the chip.
//!
//! Like [`crate::git_context`], this module is **pure parsing only** — it
//! never spawns `gh`. The async [`GhProbe`](crate::gh_probe) runs
//! `gh pr view --json number,state,statusCheckRollup` off the UI thread
//! and feeds the raw JSON to [`parse_pr_view`] here. Keeping the JSON +
//! roll-up logic pure means it's unit-testable without a network or a
//! GitHub repo.
//!
//! JSON is walked via [`serde_json::Value`] (no derive), matching the
//! lifecycle-marker parsing in [`crate::markers`].

#![forbid(unsafe_code)]

use serde_json::Value;

/// Rolled-up CI state for a PR's checks, used to color the chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    /// No checks are configured / reported for the PR.
    None,
    /// At least one check is still running or queued, none failing.
    Pending,
    /// Every reported check succeeded (or was neutral / skipped).
    Passing,
    /// At least one check failed / errored / was cancelled.
    Failing,
}

/// Parsed GitHub context for the pane's branch: the open PR number and
/// its rolled-up CI status. Only produced for an **open** PR — a merged
/// or closed PR is not the "current" PR and yields `None` at the parse
/// layer (so no chip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrContext {
    pub number: u32,
    pub ci: CiStatus,
}

/// Parse `gh pr view --json number,state,statusCheckRollup` JSON into a
/// [`PrContext`], or `None` when the JSON doesn't parse, lacks a numeric
/// `number`, or the PR isn't open. The CI status is rolled up from
/// `statusCheckRollup`.
pub fn parse_pr_view(json: &str) -> Option<PrContext> {
    let value: Value = serde_json::from_str(json).ok()?;
    // Only an open PR is the branch's "current" PR; merged/closed → no chip.
    if value.get("state").and_then(Value::as_str) != Some("OPEN") {
        return None;
    }
    let number = u32::try_from(value.get("number")?.as_u64()?).ok()?;
    let checks = value.get("statusCheckRollup").and_then(Value::as_array);
    Some(PrContext { number, ci: rollup_ci(checks.map(Vec::as_slice).unwrap_or(&[])) })
}

/// Per-check classification before the roll-up fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckClass {
    Passing,
    Pending,
    Failing,
}

/// Read a non-empty string field off a check entry.
fn field<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Classify one `statusCheckRollup` entry. GitHub mixes two shapes: a
/// `CheckRun` is judged by its `conclusion` (authoritative once present),
/// a legacy `StatusContext` by its `state`; a CheckRun that hasn't
/// completed yet has neither and is pending.
fn classify(entry: &Value) -> CheckClass {
    if let Some(conclusion) = field(entry, "conclusion") {
        return match conclusion {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => CheckClass::Passing,
            // FAILURE, TIMED_OUT, CANCELLED, ACTION_REQUIRED, STARTUP_FAILURE, STALE, …
            _ => CheckClass::Failing,
        };
    }
    if let Some(state) = field(entry, "state") {
        return match state {
            "SUCCESS" => CheckClass::Passing,
            "FAILURE" | "ERROR" => CheckClass::Failing,
            // PENDING, EXPECTED, …
            _ => CheckClass::Pending,
        };
    }
    CheckClass::Pending
}

/// Fold per-check classes into one [`CiStatus`]: any failure dominates,
/// then any pending, then all-passing; an empty list is [`CiStatus::None`].
fn rollup_ci(checks: &[Value]) -> CiStatus {
    if checks.is_empty() {
        return CiStatus::None;
    }
    let mut any_pending = false;
    for entry in checks {
        match classify(entry) {
            CheckClass::Failing => return CiStatus::Failing,
            CheckClass::Pending => any_pending = true,
            CheckClass::Passing => {}
        }
    }
    if any_pending { CiStatus::Pending } else { CiStatus::Passing }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_pr_all_checks_pass_is_passing() {
        let json = r#"{"number":124,"state":"OPEN","statusCheckRollup":[
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"NEUTRAL"}
        ]}"#;
        let pr = parse_pr_view(json).expect("open PR");
        assert_eq!(pr.number, 124);
        assert_eq!(pr.ci, CiStatus::Passing);
    }

    #[test]
    fn any_failure_dominates() {
        let json = r#"{"number":7,"state":"OPEN","statusCheckRollup":[
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"},
            {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":null}
        ]}"#;
        assert_eq!(parse_pr_view(json).unwrap().ci, CiStatus::Failing);
    }

    #[test]
    fn in_progress_without_failure_is_pending() {
        let json = r#"{"number":7,"state":"OPEN","statusCheckRollup":[
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":null}
        ]}"#;
        assert_eq!(parse_pr_view(json).unwrap().ci, CiStatus::Pending);
    }

    #[test]
    fn legacy_status_context_state_is_read() {
        let json = r#"{"number":9,"state":"OPEN","statusCheckRollup":[
            {"__typename":"StatusContext","state":"PENDING"},
            {"__typename":"StatusContext","state":"SUCCESS"}
        ]}"#;
        assert_eq!(parse_pr_view(json).unwrap().ci, CiStatus::Pending);
        let failing = r#"{"number":9,"state":"OPEN","statusCheckRollup":[
            {"__typename":"StatusContext","state":"ERROR"}
        ]}"#;
        assert_eq!(parse_pr_view(failing).unwrap().ci, CiStatus::Failing);
    }

    #[test]
    fn no_checks_is_ci_none() {
        let json = r#"{"number":3,"state":"OPEN","statusCheckRollup":[]}"#;
        assert_eq!(parse_pr_view(json).unwrap().ci, CiStatus::None);
    }

    #[test]
    fn merged_or_closed_pr_yields_none() {
        let merged = r#"{"number":124,"state":"MERGED","statusCheckRollup":[]}"#;
        assert_eq!(parse_pr_view(merged), None);
        let closed = r#"{"number":5,"state":"CLOSED","statusCheckRollup":[]}"#;
        assert_eq!(parse_pr_view(closed), None);
    }

    #[test]
    fn garbage_json_is_none() {
        assert_eq!(parse_pr_view("not json"), None);
        assert_eq!(parse_pr_view(""), None);
    }
}
