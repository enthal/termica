//! Capture Termica's own command submits into the `runs` table.
//!
//! Sits between [`crate::pane::PaneSession`] and
//! [`crate::history::HistoryStore`]: when the integration script
//! emits a `Preexec` marker the shell is about to run the command,
//! so we INSERT a row; when `CommandFinished` arrives we UPDATE the
//! same row with the exit code and timestamp.
//!
//! Extracted as a free function so the strict-layer tests can drive
//! it directly with synthetic `LifecycleEvent`s — `PaneSession`
//! still owns OS resources (PTY + reader thread) that are too
//! expensive to stand up in a per-test fixture.

use std::sync::{Arc, Mutex};

use crate::history::HistoryStore;
use crate::markers::LifecycleEvent;

/// Bundle of identifiers a `PaneSession` carries for its lifetime
/// once history capture is enabled. Cheap to clone (the store is
/// behind an `Arc<Mutex<…>>`); each pane carries one.
#[derive(Clone)]
pub struct HistoryContext {
    pub store: Arc<Mutex<HistoryStore>>,
    pub app_run_id: String,
}

impl std::fmt::Debug for HistoryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryContext").field("app_run_id", &self.app_run_id).finish()
    }
}

/// Per-pane mutable state the capture path owns: the row id of the
/// currently-running command, so `CommandFinished` can stamp it.
/// `None` between commands. Defaults to `None` for fresh panes.
#[derive(Debug, Default, Clone, Copy)]
pub struct CaptureState {
    pub current_run_id: Option<i64>,
}

/// Apply one lifecycle event to the history store. No-op if `ctx`
/// is `None` (history disabled) or the event isn't one of the two
/// we care about (`Preexec`, `CommandFinished`).
///
/// `now_ms` is injected (not read from the system clock) so tests
/// stay deterministic. `pane_id` is the same `u64` the pane carries
/// for diagnostics; `cwd` is the most recent cwd from the terminal
/// or the integration script.
pub fn on_event(
    ctx: Option<&HistoryContext>,
    state: &mut CaptureState,
    event: &LifecycleEvent,
    pane_id: u64,
    cwd: Option<&str>,
    now_ms: i64,
) {
    let Some(ctx) = ctx else { return };
    match event {
        LifecycleEvent::Preexec { command } => {
            let Ok(store) = ctx.store.lock() else { return };
            if let Ok(id) =
                store.record_submit(command, cwd, pane_id as i64, &ctx.app_run_id, now_ms)
            {
                state.current_run_id = Some(id);
            }
        }
        LifecycleEvent::CommandFinished { exit } => {
            let Some(id) = state.current_run_id.take() else { return };
            let Ok(store) = ctx.store.lock() else { return };
            let _ = store.record_finish(id, now_ms, Some(*exit));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Scope;

    fn ctx() -> HistoryContext {
        HistoryContext {
            store: Arc::new(Mutex::new(HistoryStore::in_memory().unwrap())),
            app_run_id: "test-app-run".to_string(),
        }
    }

    #[test]
    fn preexec_inserts_a_run_with_command_and_metadata() {
        let c = ctx();
        let mut state = CaptureState::default();
        on_event(
            Some(&c),
            &mut state,
            &LifecycleEvent::Preexec { command: "ls -la".to_string() },
            7,
            Some("/tmp/proj"),
            1_700_000_000_000,
        );
        assert!(state.current_run_id.is_some());
        let store = c.store.lock().unwrap();
        let rows = store.recent(&Scope::Global, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "ls -la");
        assert_eq!(rows[0].cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(rows[0].pane_id, Some(7));
        assert_eq!(rows[0].app_run_id.as_deref(), Some("test-app-run"));
        assert_eq!(rows[0].started_at_ms, 1_700_000_000_000);
        assert_eq!(rows[0].finished_at_ms, None);
        assert_eq!(rows[0].source, "termica");
    }

    #[test]
    fn command_finished_stamps_exit_on_the_pending_run() {
        let c = ctx();
        let mut state = CaptureState::default();
        on_event(
            Some(&c),
            &mut state,
            &LifecycleEvent::Preexec { command: "ls".to_string() },
            1,
            None,
            100,
        );
        let id = state.current_run_id.expect("preexec set the id");
        on_event(Some(&c), &mut state, &LifecycleEvent::CommandFinished { exit: 7 }, 1, None, 200);
        assert!(state.current_run_id.is_none(), "finish clears the pending id");
        let store = c.store.lock().unwrap();
        let row = store.get(id).unwrap().unwrap();
        assert_eq!(row.finished_at_ms, Some(200));
        assert_eq!(row.exit_code, Some(7));
    }

    #[test]
    fn command_finished_without_pending_preexec_is_silent_noop() {
        // Bug-defensive: if the integration script emits a
        // CommandFinished without a matching Preexec (markers
        // dropped, shell restart, …) we must not panic or insert
        // a row.
        let c = ctx();
        let mut state = CaptureState::default();
        on_event(Some(&c), &mut state, &LifecycleEvent::CommandFinished { exit: 0 }, 1, None, 100);
        let rows = c.store.lock().unwrap().recent(&Scope::Global, 10).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn unrelated_events_are_ignored() {
        let c = ctx();
        let mut state = CaptureState::default();
        for event in [
            LifecycleEvent::Precmd { cwd: std::path::PathBuf::from("/tmp") },
            LifecycleEvent::Cwd { cwd: std::path::PathBuf::from("/tmp") },
            LifecycleEvent::Continuation,
        ] {
            on_event(Some(&c), &mut state, &event, 1, None, 100);
        }
        assert!(state.current_run_id.is_none());
        let rows = c.store.lock().unwrap().recent(&Scope::Global, 10).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn no_history_context_is_total_noop() {
        // The function must be safe to call with ctx = None — that's
        // how panes without persistence behave.
        let mut state = CaptureState::default();
        on_event(
            None,
            &mut state,
            &LifecycleEvent::Preexec { command: "ls".to_string() },
            1,
            None,
            100,
        );
        assert!(state.current_run_id.is_none());
    }

    #[test]
    fn two_preexecs_in_a_row_overwrite_pending_id() {
        // Edge case: if a CommandFinished is missed between two
        // Preexecs (e.g. signal-driven abort that didn't fire the
        // shell's exit hook), the SECOND Preexec wins. We lose the
        // exit code on the first run, which is correct — it never
        // got one. We do NOT stamp the second exit onto the first.
        let c = ctx();
        let mut state = CaptureState::default();
        on_event(
            Some(&c),
            &mut state,
            &LifecycleEvent::Preexec { command: "first".to_string() },
            1,
            None,
            100,
        );
        let first_id = state.current_run_id.unwrap();
        on_event(
            Some(&c),
            &mut state,
            &LifecycleEvent::Preexec { command: "second".to_string() },
            1,
            None,
            200,
        );
        let second_id = state.current_run_id.unwrap();
        assert_ne!(first_id, second_id);
        on_event(Some(&c), &mut state, &LifecycleEvent::CommandFinished { exit: 0 }, 1, None, 300);
        let store = c.store.lock().unwrap();
        let first_row = store.get(first_id).unwrap().unwrap();
        let second_row = store.get(second_id).unwrap().unwrap();
        assert_eq!(first_row.exit_code, None, "first row never got an exit");
        assert_eq!(second_row.exit_code, Some(0), "exit lands on the second row");
    }
}
