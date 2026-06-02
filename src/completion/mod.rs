//! Tab completion engine.
//!
//! [Spec/04a](../../spec/04a-completion.md) is the full design.
//! This module implements **slice 1** of that design:
//! the MVP local-only completion engine ([Phase 4I](../../spec/10-roadmap.md)),
//! covering the three local sources (paths, `$PATH` executables,
//! command history) behind a native egui popup.
//!
//! CLI-native drivers (kubectl __complete, etc.) and shell sidecars
//! (bash / zsh / fish) land in later slices and plug in behind the
//! same popup.
//!
//! ## Module layout
//!
//! - [`local`] — pure-logic candidate generators for the three v1
//!   sources, plus the token-under-cursor helper. All testable
//!   without spawning anything or touching the filesystem (the
//!   filesystem-reading parts take a directory listing as input).
//! - [`ranking`] — score-based merge of candidates from multiple
//!   sources into one popup list. Pure logic.
//! - [`popup`] — `CompletionPopup` UI state (the candidate list,
//!   the typed-prefix, the selected index) + egui paint.
//!
//! ## Wire types
//!
//! A `CompletionCandidate` is what each source produces and what
//! the popup displays. `value` is what gets inserted into the
//! editor on accept; `display` is what the popup shows (usually
//! the same as `value`); `description`, when present, renders on
//! the right of the popup row.

#![forbid(unsafe_code)]

pub mod local;
pub mod popup;
pub mod ranking;

pub use popup::CompletionPopup;

/// One candidate in the completion popup.
///
/// `value` is the bytes inserted into the editor when this
/// candidate is accepted (replaces the typed token). `display` is
/// what the popup shows in its row (usually identical, but a
/// driver / sidecar source may want a different visible label).
/// `description` is the optional one-line annotation rendered on
/// the right edge of the row.
///
/// `source` tags the origin so the popup can show a source chip
/// and the ranker can apply per-source weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub value: String,
    pub display: String,
    pub description: Option<String>,
    pub source: CompletionSource,
}

impl CompletionCandidate {
    pub fn simple(value: impl Into<String>, source: CompletionSource) -> Self {
        let value = value.into();
        Self { display: value.clone(), value, description: None, source }
    }

    pub fn with_description(
        value: impl Into<String>,
        description: impl Into<String>,
        source: CompletionSource,
    ) -> Self {
        let value = value.into();
        Self { display: value.clone(), value, description: Some(description.into()), source }
    }
}

/// Where a [`CompletionCandidate`] came from. v1 only has the three
/// local sources; CLI-native drivers and shell sidecars add their
/// own variants in later slices ([spec/04a](../../spec/04a-completion.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionSource {
    /// A filesystem path under the user's cwd or an absolute
    /// path. Triggered when the token under the cursor looks
    /// pathish (`./`, `/`, `~/`, or contains `/`).
    Path,
    /// An executable found on `$PATH`. Triggered when the token
    /// is the first token of the buffer (the command position)
    /// and doesn't contain `/`.
    PathExecutable,
    /// A previous command from `runs` history, prefix-matching the
    /// typed token.
    History,
}

impl CompletionSource {
    /// Short tag for the popup's right-edge source label.
    pub fn tag(self) -> &'static str {
        match self {
            CompletionSource::Path => "path",
            CompletionSource::PathExecutable => "$PATH",
            CompletionSource::History => "history",
        }
    }
}
