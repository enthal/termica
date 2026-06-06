//! Git context for the block-header chips (4G-async-context): the
//! current branch, ahead/behind counts, and a dirty summary.
//!
//! This module is **pure parsing only** — it never spawns a process.
//! The async per-pane probe (a later slice) runs the `git` commands for
//! the pane's cwd off the UI thread, then feeds their raw stdout to the
//! parsers here. Keeping the fiddly porcelain parsing pure means it's
//! unit-testable without a git repo, and the probe stays a thin
//! spawn-and-channel wrapper. See [spec/04 §"Visual structure"](../spec/04-prompt-editor.md#visual-structure-the-block-model)
//! and [spec/10 4G-async-context](../spec/10-roadmap.md).

#![forbid(unsafe_code)]

/// How dirty the working tree is, relative to HEAD + index. `files_changed`
/// counts every status entry (staged, unstaged, unmerged, and untracked);
/// `lines_added` / `lines_removed` are summed from `git diff` numstat over
/// the tracked changes (untracked files have no diff and contribute only
/// to `files_changed`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirtySummary {
    pub files_changed: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
}

impl DirtySummary {
    /// True when the working tree is clean (nothing to show on a dirty
    /// chip).
    pub fn is_clean(&self) -> bool {
        self.files_changed == 0 && self.lines_added == 0 && self.lines_removed == 0
    }
}

/// Parsed git context for a directory: branch (or `None` on a detached
/// HEAD / outside any branch), upstream ahead/behind, and the dirty
/// summary. `None` for the whole `GitContext` (at the probe layer) means
/// "not a git repo"; an all-default `GitContext` here means "clean repo,
/// no upstream."
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitContext {
    /// Current branch name, or `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Commits ahead of the upstream (`0` when no upstream / in sync).
    pub ahead: u32,
    /// Commits behind the upstream.
    pub behind: u32,
    pub dirty: DirtySummary,
}

impl GitContext {
    /// Fold the line counts from a parsed numstat into the dirty
    /// summary. The probe calls this after [`parse_status_v2`] with the
    /// `(added, removed)` from [`parse_numstat`] (working tree + index).
    pub fn with_line_counts(mut self, lines_added: u32, lines_removed: u32) -> Self {
        self.dirty.lines_added = lines_added;
        self.dirty.lines_removed = lines_removed;
        self
    }

    /// Chip text for the upstream relationship, or `None` when in sync
    /// (or there's no upstream — both counts zero). Words, not glyphs,
    /// so the no-Unicode-icon rule holds and `+`/`-` stay reserved for
    /// the dirty chip's line counts: `ahead 2`, `behind 1`, or
    /// `ahead 2 behind 1`.
    pub fn sync_label(&self) -> Option<String> {
        match (self.ahead, self.behind) {
            (0, 0) => None,
            (a, 0) => Some(format!("ahead {a}")),
            (0, b) => Some(format!("behind {b}")),
            (a, b) => Some(format!("ahead {a} behind {b}")),
        }
    }

    /// Chip text for the working-tree dirtiness, or `None` when clean.
    /// `{n} file[s]`, plus ` +{added} -{removed}` when there are tracked
    /// line changes (untracked-only dirt shows just the file count).
    pub fn dirty_label(&self) -> Option<String> {
        if self.dirty.is_clean() {
            return None;
        }
        let DirtySummary { files_changed, lines_added, lines_removed } = self.dirty;
        let noun = if files_changed == 1 { "file" } else { "files" };
        let mut label = format!("{files_changed} {noun}");
        if lines_added > 0 || lines_removed > 0 {
            label.push_str(&format!(" +{lines_added} -{lines_removed}"));
        }
        Some(label)
    }
}

/// Parse `git status --porcelain=v2 --branch` stdout into a
/// [`GitContext`] — branch, ahead/behind, and the changed-file count.
/// Line counts are left zero (they come from [`parse_numstat`], merged
/// in via [`GitContext::with_line_counts`]).
///
/// Porcelain v2 format (the bits we read):
/// - `# branch.head <name>` — current branch, or `(detached)`.
/// - `# branch.ab +<ahead> -<behind>` — present only with an upstream.
/// - entry lines `1 …` / `2 …` (changed/renamed), `u …` (unmerged),
///   `? …` (untracked) — each is one changed file. `! …` (ignored) and
///   the other `#` headers are skipped.
pub fn parse_status_v2(output: &str) -> GitContext {
    let mut ctx = GitContext::default();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            let name = rest.trim();
            ctx.branch = if name == "(detached)" { None } else { Some(name.to_string()) };
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            // "+<ahead> -<behind>"
            let mut parts = rest.split_whitespace();
            if let Some(a) = parts.next() {
                ctx.ahead = a.trim_start_matches('+').parse().unwrap_or(0);
            }
            if let Some(b) = parts.next() {
                ctx.behind = b.trim_start_matches('-').parse().unwrap_or(0);
            }
        } else if is_changed_entry(line) {
            ctx.dirty.files_changed += 1;
        }
    }
    ctx
}

/// True for a porcelain-v2 entry line that represents one changed file:
/// `1 ` / `2 ` (ordinary / renamed-copied), `u ` (unmerged), `? `
/// (untracked). Ignored (`! `) and header (`# `) lines return false.
fn is_changed_entry(line: &str) -> bool {
    matches!(line.as_bytes().first(), Some(b'1' | b'2' | b'u' | b'?'))
        && line.as_bytes().get(1) == Some(&b' ')
}

/// Parse `git diff --numstat` stdout, summing added / removed line counts
/// across all files. Each line is `<added>\t<removed>\t<path>`; binary
/// files render as `-\t-\t<path>` and contribute nothing. Returns
/// `(lines_added, lines_removed)`.
pub fn parse_numstat(output: &str) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in output.lines() {
        let mut fields = line.split('\t');
        let a = fields.next().and_then(|s| s.parse::<u32>().ok());
        let r = fields.next().and_then(|s| s.parse::<u32>().ok());
        // Both must be numeric (binary files are `-`); a malformed or
        // binary line is skipped wholesale.
        if let (Some(a), Some(r)) = (a, r) {
            added += a;
            removed += r;
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_clean_branch_with_upstream() {
        let out = "# branch.oid abc123\n\
                   # branch.head main\n\
                   # branch.upstream origin/main\n\
                   # branch.ab +0 -0\n";
        let ctx = parse_status_v2(out);
        assert_eq!(ctx.branch.as_deref(), Some("main"));
        assert_eq!((ctx.ahead, ctx.behind), (0, 0));
        assert!(ctx.dirty.is_clean());
    }

    #[test]
    fn status_counts_every_changed_entry() {
        let out = "# branch.head feat/x\n\
                   # branch.ab +2 -1\n\
                   1 .M N... 100644 100644 100644 aaa bbb src/a.rs\n\
                   1 M. N... 100644 100644 100644 ccc ddd src/b.rs\n\
                   2 R. N... 100644 100644 100644 eee fff R100 new.rs\told.rs\n\
                   u UU N... 100644 100644 100644 100644 ggg hhh iii merge.rs\n\
                   ? untracked.txt\n";
        let ctx = parse_status_v2(out);
        assert_eq!(ctx.branch.as_deref(), Some("feat/x"));
        assert_eq!((ctx.ahead, ctx.behind), (2, 1));
        // 2 ordinary + 1 renamed + 1 unmerged + 1 untracked.
        assert_eq!(ctx.dirty.files_changed, 5);
    }

    #[test]
    fn status_ignored_and_headers_dont_count() {
        let out = "# branch.head main\n\
                   ! ignored.log\n\
                   # branch.ab +0 -0\n";
        let ctx = parse_status_v2(out);
        assert_eq!(ctx.dirty.files_changed, 0);
    }

    #[test]
    fn status_detached_head_has_no_branch() {
        let out = "# branch.oid abc123\n# branch.head (detached)\n";
        let ctx = parse_status_v2(out);
        assert_eq!(ctx.branch, None);
    }

    #[test]
    fn status_no_upstream_means_zero_ahead_behind() {
        // A branch with no upstream emits no `# branch.ab` line.
        let out = "# branch.head local-only\n1 .M N... x x x a b src/a.rs\n";
        let ctx = parse_status_v2(out);
        assert_eq!(ctx.branch.as_deref(), Some("local-only"));
        assert_eq!((ctx.ahead, ctx.behind), (0, 0));
        assert_eq!(ctx.dirty.files_changed, 1);
    }

    #[test]
    fn status_empty_output_is_default() {
        assert_eq!(parse_status_v2(""), GitContext::default());
    }

    #[test]
    fn numstat_sums_added_and_removed() {
        let out = "3\t1\tsrc/a.rs\n0\t5\tsrc/b.rs\n12\t0\tsrc/c.rs\n";
        assert_eq!(parse_numstat(out), (15, 6));
    }

    #[test]
    fn numstat_skips_binary_dash_lines() {
        let out = "-\t-\timage.png\n2\t3\tsrc/a.rs\n";
        assert_eq!(parse_numstat(out), (2, 3));
    }

    #[test]
    fn numstat_empty_is_zero() {
        assert_eq!(parse_numstat(""), (0, 0));
    }

    #[test]
    fn with_line_counts_folds_into_dirty() {
        let ctx = parse_status_v2("# branch.head main\n? a.txt\n").with_line_counts(7, 2);
        assert_eq!(ctx.dirty.files_changed, 1);
        assert_eq!(ctx.dirty.lines_added, 7);
        assert_eq!(ctx.dirty.lines_removed, 2);
    }

    #[test]
    fn dirty_summary_is_clean_only_when_all_zero() {
        assert!(DirtySummary::default().is_clean());
        assert!(!DirtySummary { files_changed: 1, ..Default::default() }.is_clean());
        assert!(!DirtySummary { lines_added: 1, ..Default::default() }.is_clean());
    }

    #[test]
    fn sync_label_words_by_relationship() {
        let mk = |ahead, behind| GitContext { ahead, behind, ..Default::default() };
        assert_eq!(mk(0, 0).sync_label(), None);
        assert_eq!(mk(2, 0).sync_label().as_deref(), Some("ahead 2"));
        assert_eq!(mk(0, 1).sync_label().as_deref(), Some("behind 1"));
        assert_eq!(mk(2, 1).sync_label().as_deref(), Some("ahead 2 behind 1"));
    }

    #[test]
    fn dirty_label_clean_is_none() {
        assert_eq!(GitContext::default().dirty_label(), None);
    }

    #[test]
    fn dirty_label_files_and_lines() {
        let ctx = GitContext {
            dirty: DirtySummary { files_changed: 3, lines_added: 120, lines_removed: 8 },
            ..Default::default()
        };
        assert_eq!(ctx.dirty_label().as_deref(), Some("3 files +120 -8"));
    }

    #[test]
    fn dirty_label_untracked_only_omits_lines() {
        // One untracked file: counted, but no diff line counts.
        let ctx = GitContext {
            dirty: DirtySummary { files_changed: 1, lines_added: 0, lines_removed: 0 },
            ..Default::default()
        };
        assert_eq!(ctx.dirty_label().as_deref(), Some("1 file"));
    }
}
