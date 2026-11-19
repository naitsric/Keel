//! A step that stopped because the person's Claude usage ran out. When the limit says when it
//! resets, Keel continues the step by itself shortly after, the way a person's «Continue» does, and
//! a few times at most. All it goes by is on the record: the step's reason says when, and the steps
//! it continued say that Keel did. So it survives the app closing, and a person who got there first
//! leaves it nothing to do.

use std::sync::Arc;
use std::time::Duration;

use keel_core::{AttemptState, Reason, WorkflowStep};
use serde_json::json;

use crate::{Engine, Error, Inner, Result, Resume, locked, resumed_prompt, workflows};

/// How long after the reset Keel continues. Its clock and the limit's needn't agree to the second,
/// and a step continued too early only stops again.
pub(crate) const AFTER_RESET: Duration = Duration::from_secs(60);
/// A reset is hours away, so half a minute late is soon enough. It goes by the clock and not by a
/// timer, which would stand still while the Mac sleeps.
pub(crate) const LOOK_AGAIN: Duration = Duration::from_secs(20);
/// How many times Keel continues one step of a run by itself. A step that keeps running out of
/// usage waits for the person after that, as every such step did before.
pub const MOST_CONTINUED_BY_ITSELF: usize = 3;
/// Who the record says continued the step.
const BY_ITSELF: &str = "Keel";
/// The event a continuation that couldn't start leaves on the step.
const COULD_NOT_CONTINUE: &str = "continueFailed";

fn now_in_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

/// When Keel continues a step by itself, if this is why it's blocked.
fn continues_at(reason: Option<&serde_json::Value>) -> Option<i64> {
    match serde_json::from_value(reason?.clone()).ok()? {
        Reason::RateLimited { continues_at, .. } => continues_at,
        _ => None,
    }
}

impl Inner {
    /// When Keel will continue a step that a usage limit resetting at `resets_at` stopped, or
    /// `None` if it has continued that step as often as it does. That's what keeps it from going
    /// round for ever: each time it continues a step there is a new attempt, and the one that says
    /// it would go on a fourth time is never written. Never before now: a limit that says it
    /// already reset gets the same moment's grace.
    pub(crate) fn continues_at(&self, run_id: &str, step_id: &str, resets_at: i64) -> Option<i64> {
        let continued = self.continued_by_itself(run_id, step_id).ok()?;
        let margin = i64::try_from(self.config.continue_after_reset.as_secs()).ok()?;
        (continued < MOST_CONTINUED_BY_ITSELF)
            .then(|| resets_at.max(now_in_seconds()).saturating_add(margin))
    }

    /// How many times Keel has continued this step of the run by itself, going by what it wrote
    /// down each time.
    fn continued_by_itself(&self, run_id: &str, step_id: &str) -> Result<usize> {
        let attempts = self.store.attempts_of_run(run_id)?;
        let events = self.store.run_events(run_id, "retryRequested")?;
        Ok(events
            .iter()
            .filter(|event| event.data["automatic"] == json!(true))
            .filter(|event| {
                attempts
                    .iter()
                    .any(|a| a.id == event.attempt_id && a.step_id == step_id)
            })
            .count())
    }

    /// Continues every step whose time has come. Returns the attempts it started.
    fn continue_due(self: &Arc<Self>) -> Result<Vec<String>> {
        let now = now_in_seconds();
        // Nearly every look finds nothing, and this is one small question.
        let due = |reason: &Reason| matches!(reason, Reason::RateLimited { continues_at: Some(at), .. } if *at <= now);
        let blocked = self.store.blocked_reasons()?;
        // What keeps cards in Todo keeps these waiting too: starting one would end the same way.
        if !blocked.iter().any(due)
            || blocked.iter().any(|reason| reason.stops_every_card(now))
            || locked(&self.board_paused).is_some()
        {
            return Ok(Vec::new());
        }
        let mut started = Vec::new();
        for run in self.store.open_runs()? {
            let attempts = self.store.attempts_of_run(&run.id)?;
            for attempt in &attempts {
                if attempt.state == AttemptState::Blocked.as_str()
                    && continues_at(attempt.reason.as_ref()).is_some_and(|at| at <= now)
                    && workflows::is_latest_of_its_step(&attempts, attempt)
                {
                    // One that can't start mustn't hold up the rest.
                    match self.continue_by_itself(&attempt.id) {
                        Ok(new) => started.extend(new),
                        Err(error) => self.could_not_continue(attempt, &error),
                    }
                }
            }
        }
        Ok(started)
    }

    /// A continuation that couldn't start goes on the step's record, and to stderr until Keel
    /// keeps a log. After as many as Keel continues a step, it stops trying: the step no longer
    /// says it will go on by itself, and waits for the person.
    fn could_not_continue(&self, attempt: &keel_store::Attempt, error: &Error) {
        eprintln!(
            "keel: could not continue step {} of run {} after its usage limit: {error}",
            attempt.step_id, attempt.run_id
        );
        let store = &self.store;
        let _ = store.append_event(
            &attempt.id,
            COULD_NOT_CONTINUE,
            &json!({ "error": error.to_string() }),
        );
        let failed = store.events(&attempt.id, 0).map_or(usize::MAX, |events| {
            events
                .iter()
                .filter(|event| event.kind == COULD_NOT_CONTINUE)
                .count()
        });
        if failed < MOST_CONTINUED_BY_ITSELF {
            return;
        }
        // Held as for a retry, and only if it's still as it was: a person may have just acted.
        let _guard = locked(&self.advancing);
        let Ok(now) = store.attempt(&attempt.id) else {
            return;
        };
        let reason = now
            .reason
            .clone()
            .and_then(|reason| serde_json::from_value(reason).ok());
        if now.state == AttemptState::Blocked.as_str()
            && let Some(Reason::RateLimited { resets_at, .. }) = reason
        {
            let waits = Reason::RateLimited {
                resets_at,
                continues_at: None,
            };
            let _ = store.set_state(&attempt.id, AttemptState::Blocked, Some(&waits));
            self.changed(&attempt.run_id);
        }
    }

    /// Continues one step's session, as the person's «Continue» would, if it's still where the
    /// limit left it: blocked on that, with nothing tried since, in a run that's still open. A
    /// person who retried it, left it or closed the run decided already, and Keel does nothing.
    /// The step it continues says that Keel did, on the same record as the new attempt.
    fn continue_by_itself(self: &Arc<Self>, attempt_id: &str) -> Result<Option<String>> {
        // As for any retry: see `Engine::retry_with`.
        let _guard = locked(&self.advancing);
        let store = &self.store;
        let attempt = store.attempt(attempt_id)?;
        let run = store.run(&attempt.run_id)?;
        let attempts = store.attempts_of_run(&attempt.run_id)?;
        let still_waiting = attempt.state == AttemptState::Blocked.as_str()
            && continues_at(attempt.reason.as_ref()).is_some_and(|at| at <= now_in_seconds())
            && attempt.kind == "agent"
            && run.finished_at.is_none()
            && workflows::is_latest_of_its_step(&attempts, &attempt)
            && !store.project_is_removed(&run.project_id)?;
        if !still_waiting {
            return Ok(None);
        }
        let steps: Vec<WorkflowStep> = serde_json::from_value(run.workflow).unwrap_or_default();
        let step = workflows::find_step(&steps, &attempt.step_id).ok_or(Error::InvalidState)?;
        let requested = json!({ "by": BY_ITSELF, "resume": true, "automatic": true });
        let retry = keel_store::Retry {
            run_id: &attempt.run_id,
            retried: attempt_id,
            found: AttemptState::Blocked,
            requested: Some(&requested),
            reopens: false,
            commands: None,
            put_off: None,
        };
        let resume = Resume {
            session: attempt.session_id.clone(),
            from: attempt.id.clone(),
            prompt: resumed_prompt(&attempt),
        };
        let number = workflows::next_number(&attempts, &attempt.step_id);
        self.start_step_with(
            &attempt.run_id,
            &step,
            number,
            Some(resume),
            Some(&retry),
            None,
        )
        .map(Some)
    }
}

impl Engine {
    /// Continues, on this thread, the steps a usage limit stopped whose limit has reset. Returns
    /// the attempts it started. Nothing starts once the app is on its way out.
    pub fn continue_after_limits(&self) -> Result<Vec<String>> {
        let stopped = locked(&self.inner.board_stopped);
        if *stopped {
            return Ok(Vec::new());
        }
        self.inner.continue_due()
    }

    /// Starts the thread that does it now and then. Nothing is armed and nothing is kept: each look
    /// reads the record, so a step the limit stopped before the app closed is continued just the
    /// same once it opens again.
    pub(crate) fn start_limit_watch(&self) {
        // It doesn't keep the engine alive, so one that's gone doesn't go on starting steps.
        let inner = Arc::downgrade(&self.inner);
        let pause = self.inner.config.limit_look_again;
        std::thread::spawn(move || {
            while let Some(inner) = inner.upgrade() {
                let _ = Engine { inner }.continue_after_limits();
                std::thread::sleep(pause);
            }
        });
    }
}
