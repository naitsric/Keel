//! What the engine's unit tests share.

use keel_core::AttemptState;
use keel_store::{Attempt, NewAttempt, Store, now};

/// Attempts as the store reads them back, one per row: step, kind, tools and state.
pub(crate) fn recorded(rows: &[(&str, &str, &str, AttemptState)]) -> Vec<Attempt> {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("keel.db")).unwrap();
    for (index, (step, kind, tools, state)) in rows.iter().enumerate() {
        let id = format!("a{index}");
        store
            .add_attempt(&NewAttempt {
                id: id.clone(),
                run_id: "r".into(),
                step_id: (*step).into(),
                kind: (*kind).into(),
                role: "worker".into(),
                round: 0,
                number: index as i64 + 1,
                state: *state,
                model_requested: String::new(),
                effort: None,
                tools: (*tools).into(),
                max_turns: None,
                max_budget_usd: None,
                language: "en".into(),
                prompt: String::new(),
                system_prompt_sources: "[]".into(),
                settings: "{}".into(),
                session_id: id.clone(),
                resumed_from: None,
                workspace: String::new(),
                raw_path: String::new(),
                started_at: now(),
            })
            .unwrap();
    }
    store.attempts_of_run("r").unwrap()
}
