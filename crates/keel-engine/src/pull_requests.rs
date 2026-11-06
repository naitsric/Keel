//! Finds out what became of the pull requests Keel opened. It only reads: after opening one, Keel
//! writes nothing to it. It runs outside any sandbox with the person's token, like delivering does,
//! and no agent gets anywhere near it.
//!
//! Nothing here is worth stopping for. If GitHub can't be asked, what Keel last knew stays, nobody
//! is told, and it asks again when its turn comes round.

use std::sync::Arc;
use std::time::Duration;

use keel_core::PrState;
use keel_harness::github;
use keel_store::rusqlite_value::Value;
use keel_store::{Run, now};

use crate::{Engine, Inner};

/// How long what Keel knows about a pull request is good for, in the background.
pub(crate) const LOOK_AGAIN: Duration = Duration::from_secs(30 * 60);
/// How long the thread waits between looking for pull requests that are due.
const PASS: Duration = Duration::from_secs(5 * 60);
/// How many it asks about per pass. A long history gets through a bit at a time.
const AT_ONCE: i64 = 20;
/// Opening a run looks right away, unless Keel looked this recently.
const WHEN_OPENED_LOOK_AGAIN: Duration = Duration::from_secs(60);

/// The same format as `now`, so the store can compare the two as text.
fn time_ago(duration: Duration) -> String {
    let then = chrono::Utc::now() - chrono::Duration::from_std(duration).unwrap_or_default();
    then.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Engine {
    /// Starts the thread that looks at pull requests now and then. Call it once, after `recover`.
    pub fn start_pr_watch(&self) {
        // It doesn't keep the engine alive, so one that's gone doesn't go on asking GitHub.
        let inner = Arc::downgrade(&self.inner);
        let pause = self.inner.config.pr_look_again.min(PASS);
        std::thread::spawn(move || {
            while let Some(inner) = inner.upgrade() {
                inner.look_at_due_prs();
                drop(inner);
                std::thread::sleep(pause);
            }
        });
    }

    /// A person opened this run, so they may be wondering. Returns at once: what it finds arrives as
    /// a change, and a run with no pull request, or a merged one, has nothing to find.
    pub fn look_at_pr(&self, run_id: &str) {
        let Ok(run) = self.inner.store.run(run_id) else {
            return;
        };
        let looked_lately = run
            .pr_checked_at
            .as_deref()
            .is_some_and(|at| at >= time_ago(WHEN_OPENED_LOOK_AGAIN).as_str());
        if run.pr_url.is_none() || run.pr_state == Some(PrState::Merged) || looked_lately {
            return;
        }
        let inner = self.inner.clone();
        std::thread::spawn(move || inner.look_at_pr(&run));
    }
}

impl Inner {
    fn look_at_due_prs(&self) {
        let before = time_ago(self.config.pr_look_again);
        let Ok(runs) = self.store.runs_with_pr_due(&before, AT_ONCE) else {
            return;
        };
        for run in runs {
            self.look_at_pr(&run);
        }
    }

    /// Whatever the answer, it counts as having looked. Marked before asking, so that a `gh` taking
    /// its time isn't asked again meanwhile, and one that fails waits its turn like any other.
    fn look_at_pr(&self, run: &Run) {
        let marked = self
            .store
            .patch_run(&run.id, &[("pr_checked_at", Value::Text(now()))]);
        let (Ok(()), Some((repo, number))) = (
            marked,
            run.pr_url.as_deref().and_then(github::pull_request_of),
        ) else {
            return;
        };
        let Some(account) = self
            .store
            .project(&run.project_id)
            .ok()
            .and_then(|project| project.github_account)
        else {
            return;
        };
        let Ok(gh) = self.gh() else {
            return;
        };
        let Ok(status) = gh
            .token(&account)
            .and_then(|token| gh.pr_status(&token, &repo, number))
        else {
            return;
        };
        if run.pr_state == Some(status.state) && run.pr_state_at == status.at {
            return;
        }
        let saved = self.store.patch_run(
            &run.id,
            &[
                ("pr_state", Value::Text(status.state.as_str().to_owned())),
                ("pr_state_at", status.at.map_or(Value::Null, Value::Text)),
            ],
        );
        if saved.is_ok() {
            self.changed(&run.id);
        }
    }
}
