//! Integration test for pane-local history surviving a real restart
//! (spec/07, spec/08 — the `DurablePane` recall scope added in #177).
//!
//! The unit tests in `history/db.rs` and `history_overlay.rs` exercise
//! the `DurablePane` scope against a single in-memory store. This test
//! goes one level up: it drives the *real* restart path end-to-end on an
//! on-disk SQLite file —
//!
//!   1. open the DB, `begin_session` a fresh pane, record two commands
//!      stamped with that pane's durable row;
//!   2. drop the store + `Persistence` (simulating process exit);
//!   3. reopen the SAME file (so `migrate()` runs a second time) and
//!      `resume_session` the pane;
//!   4. assert recall scoped to the durable row surfaces both
//!      prior-session commands, while the ephemeral `(pane_id,
//!      app_run_id)` slice under the new run is empty.
//!
//! This is the behaviour a user sees as "↑ still has my last session's
//! commands after I quit and relaunch." It would not even compile before
//! #177 (no `Scope::DurablePane`, no `record_submit_with_pane`).

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use termica::history::{HistoryStore, Scope};
use termica::persist::store::Persistence;

#[test]
fn pane_local_history_survives_a_real_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("termica.sqlite");
    let now = 1_700_000_000_000;

    // --- Session 1: a fresh pane records two commands, then "exits". ---
    let pane_row_id = {
        let store = HistoryStore::open(&db_path).expect("store opens on disk");
        let store = Arc::new(Mutex::new(store));
        let p = Persistence::new(tmp.path().to_path_buf(), Arc::clone(&store));
        let rec = p.begin_session(Some("/work/proj"), "zsh", now).expect("begin session");
        let pane = rec.pane_row.0;

        {
            let s = store.lock().unwrap();
            s.record_submit_with_pane("echo one", Some("/work/proj"), 11, "RUN-1", now, Some(pane))
                .unwrap();
            s.record_submit_with_pane(
                "echo two",
                Some("/work/proj"),
                11,
                "RUN-1",
                now + 1,
                Some(pane),
            )
            .unwrap();
        }
        pane
        // `store`, `p`, and the session lock drop here — process exit.
    };

    // --- Session 2: a NEW process reopens the same DB and resumes. ---
    // Reopening re-runs `migrate()`; the pane reuses its durable row even
    // though a fresh process would mint a new ephemeral PaneId + app_run_id.
    let store = HistoryStore::open(&db_path).expect("store reopens the same file");
    let store = Arc::new(Mutex::new(store));
    let p = Persistence::new(tmp.path().to_path_buf(), Arc::clone(&store));
    let rec = p.resume_session(pane_row_id, now + 100).expect("resume session");
    assert_eq!(rec.pane_row.0, pane_row_id, "restart reuses the same durable pane row");

    let s = store.lock().unwrap();

    // Durable-scoped recall surfaces BOTH prior-session commands,
    // newest-first — exactly what `↑` and the Cmd+R "this pane" tab walk.
    let durable = s.recent(&Scope::DurablePane { db_pane_id: pane_row_id }, 10).unwrap();
    let texts: Vec<&str> = durable.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(texts, vec!["echo two", "echo one"], "durable row carries history across restart");

    // The ephemeral slice under the NEW app_run_id is empty — proof that
    // the durable row, not the per-process run, is what spans the restart.
    let ephemeral = s.recent(&Scope::Pane { pane_id: 11, app_run_id: "RUN-2" }, 10).unwrap();
    assert!(ephemeral.is_empty(), "a fresh app_run_id sees nothing without the durable row");
}
