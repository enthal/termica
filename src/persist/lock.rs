//! Per-session OS advisory lock (Phase 9F).
//!
//! Each live session holds an exclusive `flock` on its session
//! directory for the session's lifetime. This gives two guarantees from
//! one primitive (spec/08 §"Concurrent processes and session
//! ownership"):
//!
//! - **No stealing a live session.** A second process's `try_acquire`
//!   returns `None` while the first still holds the lock, so restore
//!   never adopts a session another process is actively using.
//! - **Crash recovery for free.** The kernel releases the lock on
//!   process death — clean exit *or* crash — so the next launch's
//!   `try_acquire` succeeds and adopts the orphaned session. There is no
//!   heartbeat to tune and no stale-flag to clear: "is this session
//!   continuable?" is answered by *trying to take its lock*.
//!
//! The `unsafe` `flock`/`LockFileEx` calls live inside the `fs4` crate;
//! this module (and the whole crate) stays `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use std::fs::File;
use std::path::Path;

use fs4::fs_std::FileExt;

/// An acquired exclusive advisory lock on a session directory. Holding
/// this value holds the lock; dropping it (or the process dying)
/// releases it.
#[derive(Debug)]
pub struct SessionLock {
    /// The locked sentinel file at `<session_dir>/.lock`. Keeping the
    /// `File` alive keeps the `flock` held. We never delete the sentinel
    /// — unlinking a locked file is a footgun (a second process could
    /// `create` a *new* inode at the same path and lock that instead, so
    /// both would think they hold "the" lock). It is a zero-byte file.
    _file: File,
}

impl SessionLock {
    /// Try to take the exclusive lock for `session_dir`.
    ///
    /// - `Ok(Some(lock))` — acquired; this process now owns the session.
    /// - `Ok(None)` — another live process holds it; do **not** adopt.
    /// - `Err(_)` — a real IO failure (dir missing, permissions, …).
    ///
    /// Non-blocking: it never waits on a held lock, it reports the
    /// contention so the caller can skip that session and move on.
    pub fn try_acquire(session_dir: &Path) -> std::io::Result<Option<SessionLock>> {
        let lock_path = session_dir.join(".lock");
        let file = File::create(&lock_path)?;
        // `flock` is per open-file-description, so even within one
        // process two separate `File::create`s of the same path contend
        // — which is what makes this testable in-process.
        //
        // `flock(2)` can be interrupted by a signal (`EINTR`) — common
        // under load when sibling work reaps children (`SIGCHLD`). That
        // is NOT contention, so retry rather than surface it as an error
        // (which a caller like `gc` would conservatively read as "live"
        // and skip — a real, load-dependent flake we hit in CI).
        loop {
            match file.try_lock_exclusive() {
                Ok(true) => return Ok(Some(SessionLock { _file: file })),
                Ok(false) => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_on_a_held_dir_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let first = SessionLock::try_acquire(tmp.path()).unwrap();
        assert!(first.is_some(), "first acquire succeeds");
        let second = SessionLock::try_acquire(tmp.path()).unwrap();
        assert!(second.is_none(), "a second acquire while the first is held fails — no steal");
        // `first` is still alive here, holding the lock.
        drop(first);
    }

    #[test]
    fn dropping_the_lock_releases_it_for_the_next_acquire() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let held = SessionLock::try_acquire(tmp.path()).unwrap();
            assert!(held.is_some());
        } // dropped here — like a process exiting / crashing
        let again = SessionLock::try_acquire(tmp.path()).unwrap();
        assert!(again.is_some(), "after release the session is adoptable again (crash recovery)");
    }

    #[test]
    fn distinct_session_dirs_lock_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("session-1");
        let b = tmp.path().join("session-2");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let la = SessionLock::try_acquire(&a).unwrap();
        let lb = SessionLock::try_acquire(&b).unwrap();
        assert!(la.is_some() && lb.is_some(), "different sessions never contend");
    }
}
