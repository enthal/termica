//! Pane-scoped result cache for CLI-native completion drivers
//! ([spec/04a §Caching](../../../spec/04a-completion.md#caching)).
//!
//! A driver call is a ~50–150 ms subprocess. Re-opening completion for the
//! same command (close → reopen, or a repeated Tab) within a short window
//! should be instant instead of re-spawning. This cache keys parsed
//! results by `(tool, cwd, line)` with a 10 s TTL; a `cd` changes the key
//! and so invalidates naturally.
//!
//! Time is supplied by the caller as **monotonic milliseconds** rather
//! than read from `Instant::now()` here, so the TTL logic is pure and the
//! tests are deterministic (no clock in the unit tests, an injected
//! [`super::Clock`] in the engine).

use std::collections::HashMap;
use std::path::PathBuf;

use super::DriverTool;
use crate::completion::CompletionCandidate;

/// Time-to-live for a cached driver result, in milliseconds
/// ([spec/04a §Caching](../../../spec/04a-completion.md#caching) — 10 s).
pub const DRIVER_CACHE_TTL_MS: u64 = 10_000;

/// Cache key: the same triple that determines a driver call's output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DriverCacheKey {
    pub tool: DriverTool,
    pub cwd: PathBuf,
    pub line: String,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    stored_ms: u64,
    candidates: Vec<CompletionCandidate>,
}

/// A `(tool, cwd, line) → candidates` cache with TTL expiry. Pane-scoped
/// (owned by the per-pane engine); dropped when the pane closes.
#[derive(Debug, Default)]
pub struct DriverResultCache {
    entries_by_key: HashMap<DriverCacheKey, CacheEntry>,
}

impl DriverResultCache {
    /// Fresh candidates for `key`, or `None` on a miss or an entry older
    /// than `ttl_ms`. `now_ms` is monotonic milliseconds from the caller's
    /// clock.
    pub fn get(
        &self,
        key: &DriverCacheKey,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Option<&Vec<CompletionCandidate>> {
        let entry = self.entries_by_key.get(key)?;
        (now_ms.saturating_sub(entry.stored_ms) < ttl_ms).then_some(&entry.candidates)
    }

    /// Store `candidates` for `key` at `now_ms`, first sweeping any expired
    /// entries so the map can't grow without bound across a long session
    /// of `cd`s and partial lines.
    pub fn put(
        &mut self,
        key: DriverCacheKey,
        now_ms: u64,
        candidates: Vec<CompletionCandidate>,
        ttl_ms: u64,
    ) {
        self.entries_by_key.retain(|_, e| now_ms.saturating_sub(e.stored_ms) < ttl_ms);
        self.entries_by_key.insert(key, CacheEntry { stored_ms: now_ms, candidates });
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries_by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::CompletionSource;

    fn key(line: &str) -> DriverCacheKey {
        DriverCacheKey {
            tool: DriverTool::Git,
            cwd: PathBuf::from("/repo"),
            line: line.to_string(),
        }
    }

    fn cands(value: &str) -> Vec<CompletionCandidate> {
        vec![CompletionCandidate::simple(value, CompletionSource::Driver(DriverTool::Git))]
    }

    #[test]
    fn hit_within_ttl_then_miss_after_expiry() {
        let mut c = DriverResultCache::default();
        c.put(key("git che"), 1_000, cands("checkout"), DRIVER_CACHE_TTL_MS);
        // Within TTL: hit.
        let hit = c.get(&key("git che"), 1_000 + 9_999, DRIVER_CACHE_TTL_MS);
        assert_eq!(hit.map(|v| v[0].value.as_str()), Some("checkout"));
        // At/after TTL: miss.
        assert!(c.get(&key("git che"), 1_000 + 10_000, DRIVER_CACHE_TTL_MS).is_none());
    }

    #[test]
    fn distinct_tool_cwd_line_are_distinct_keys() {
        let mut c = DriverResultCache::default();
        c.put(key("git che"), 0, cands("checkout"), DRIVER_CACHE_TTL_MS);
        // Different line → miss.
        assert!(c.get(&key("git br"), 0, DRIVER_CACHE_TTL_MS).is_none());
        // Different cwd → miss.
        let other_cwd =
            DriverCacheKey { tool: DriverTool::Git, cwd: "/other".into(), line: "git che".into() };
        assert!(c.get(&other_cwd, 0, DRIVER_CACHE_TTL_MS).is_none());
        // Different tool → miss.
        let other_tool =
            DriverCacheKey { tool: DriverTool::Gh, cwd: "/repo".into(), line: "git che".into() };
        assert!(c.get(&other_tool, 0, DRIVER_CACHE_TTL_MS).is_none());
    }

    #[test]
    fn put_sweeps_expired_entries() {
        let mut c = DriverResultCache::default();
        c.put(key("old"), 0, cands("a"), DRIVER_CACHE_TTL_MS);
        assert_eq!(c.len(), 1);
        // A put far in the future sweeps the now-expired "old" entry as a
        // side effect, leaving only the fresh one.
        c.put(key("new"), 1_000_000, cands("b"), DRIVER_CACHE_TTL_MS);
        assert_eq!(c.len(), 1, "expired entry swept on put");
        assert!(c.get(&key("old"), 1_000_000, DRIVER_CACHE_TTL_MS).is_none());
        assert!(c.get(&key("new"), 1_000_000, DRIVER_CACHE_TTL_MS).is_some());
    }

    #[test]
    fn put_overwrites_same_key_with_fresh_timestamp() {
        let mut c = DriverResultCache::default();
        c.put(key("git che"), 0, cands("old"), DRIVER_CACHE_TTL_MS);
        c.put(key("git che"), 5_000, cands("new"), DRIVER_CACHE_TTL_MS);
        // Re-stored at 5_000, so still fresh at 14_999.
        let hit = c.get(&key("git che"), 14_999, DRIVER_CACHE_TTL_MS);
        assert_eq!(hit.map(|v| v[0].value.as_str()), Some("new"));
        assert_eq!(c.len(), 1);
    }
}
