//! The board: cards a person queues up, and the scheduler that starts them. Everything here lives
//! on this machine. Nothing in this file talks to GitHub.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::time::{Duration, Instant};

use keel_core::{AttemptState, BoardColumn, CardLane, PrState, Reason};
use keel_harness::github::Issue;
use keel_store::{Card, NewCard, Project, Store};

use crate::{
    Engine, Error, Result, StartRun, Workflow, locked, new_id, request_from_issue, title_from,
    title_from_issue,
};

const MAX_AGENTS_KEY: &str = "max_agents";
const MAX_AGENTS_RANGE: std::ops::RangeInclusive<usize> = 1..=10;
/// The same as the New Run dialog starts with.
const DEFAULT_WORKFLOW: &str = "builtin:full";
/// Signing in on Terminal or a usage limit resetting tell Keel nothing, so it looks again now and then.
const LOOK_AGAIN: Duration = Duration::from_secs(30);
const BLOCKED_PAUSE: Duration = Duration::from_secs(5);

/// A note for the scheduler that something changed. Leaving it never blocks on anything but its own
/// lock, which is why `on_change` can do it while holding others. Any number of notes before the
/// thread gets to them count as one.
#[derive(Default)]
pub(crate) struct Wake {
    pending: Mutex<bool>,
    signal: Condvar,
}

impl Wake {
    pub(crate) fn notify(&self) {
        *locked(&self.pending) = true;
        self.signal.notify_one();
    }

    /// Waits for a note, or for `timeout`, and takes the note if there is one.
    fn wait(&self, timeout: Duration) -> bool {
        let pending = locked(&self.pending);
        let (mut pending, _) = self
            .signal
            .wait_timeout_while(pending, timeout, |pending| !*pending)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *pending)
    }
}

pub(crate) fn stored_max_agents(store: &Store) -> Option<usize> {
    store
        .setting(MAX_AGENTS_KEY)
        .ok()
        .flatten()?
        .parse()
        .ok()
        .filter(|n| MAX_AGENTS_RANGE.contains(n))
}

/// What the board says about an issue. The body stays in the copy the run reads.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardIssue {
    pub number: i64,
    pub title: String,
    pub author: String,
    pub url: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardCard {
    pub id: String,
    pub project_id: String,
    /// Not masked, like the body: the edit dialog saves back what it gets, and the agent gets them
    /// as written.
    pub title: String,
    pub body: String,
    #[serde(serialize_with = "keel_core::serialize_masked")]
    pub issue: Option<BoardIssue>,
    pub column: BoardColumn,
    pub position: i64,
    pub run_id: Option<String>,
    pub run_state: Option<AttemptState>,
    /// These can quote what git, `gh` or an agent said.
    #[serde(serialize_with = "keel_core::serialize_masked")]
    pub reason: Option<Reason>,
    pub pr_url: Option<String>,
    /// What became of that pull request, as of the last time Keel looked. The column doesn't
    /// depend on it: a card is done when its run is, whatever happens on GitHub later.
    pub pr_state: Option<PrState>,
    pub pr_state_at: Option<String>,
    #[serde(serialize_with = "keel_core::serialize_masked")]
    pub rejection: Option<String>,
    #[serde(serialize_with = "keel_core::serialize_masked")]
    pub start_blocked: Option<Reason>,
    pub created_at: String,
}

impl From<Card> for BoardCard {
    fn from(card: Card) -> Self {
        let reason =
            |value: Option<serde_json::Value>| value.and_then(|v| serde_json::from_value(v).ok());
        Self {
            column: card.column(),
            issue: issue_of(&card).map(|issue| BoardIssue {
                number: issue.number,
                title: issue.title,
                author: issue.author,
                url: issue.url,
            }),
            reason: reason(card.run_reason),
            start_blocked: reason(card.start_blocked),
            id: card.id,
            project_id: card.project_id,
            title: card.title,
            body: card.body,
            position: card.position,
            run_id: card.run_id,
            run_state: card.run_state,
            pr_url: card.pr_url,
            pr_state: card.pr_state,
            pr_state_at: card.pr_state_at,
            rejection: card.rejection,
            created_at: card.created_at,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Board {
    /// By column, and in each one the way a person reads it. See `Store::cards`.
    pub cards: Vec<BoardCard>,
    /// Runs holding a place now, across the whole app.
    pub working: usize,
    pub max_agents: usize,
    pub default_workflow: Option<String>,
    /// Why Todo cards aren't starting, whichever project they're in: the session can't work, or a
    /// run is blocked on something that would stop any other card too.
    #[serde(serialize_with = "keel_core::serialize_masked")]
    pub paused: Option<Reason>,
}

fn issue_of(card: &Card) -> Option<Issue> {
    serde_json::from_value(card.issue.clone()?).ok()
}

impl Engine {
    pub fn board(&self, project_id: &str) -> Result<Board> {
        let store = &self.inner.store;
        let project = store.project(project_id)?;
        Ok(Board {
            cards: store
                .cards(project_id)?
                .into_iter()
                .map(BoardCard::from)
                .collect(),
            working: store.working_runs()?,
            max_agents: self.max_agents(),
            default_workflow: project.default_workflow,
            // What the scheduler last saw. Looking now would take launching two processes.
            paused: locked(&self.inner.board_paused).clone(),
        })
    }

    pub fn card_create(&self, project_id: &str, title: &str, body: &str) -> Result<String> {
        let title = title.trim();
        if title.is_empty() {
            return Err(Error::InvalidState);
        }
        self.inner.store.project(project_id)?;
        let id = self.inner.store.add_card(&NewCard {
            id: new_id(),
            project_id: project_id.to_owned(),
            title: title.to_owned(),
            body: body.trim().to_owned(),
            issue: None,
            issue_number: None,
        })?;
        self.inner.changed("");
        Ok(id)
    }

    /// Puts an issue on the board, or finds it there already. It's a copy: whatever happens to the
    /// issue later doesn't change the card. The card's own text starts empty and is whatever the
    /// person adds to it, the same as in the New Run dialog.
    pub fn card_import_issue(&self, project_id: &str, issue: &Issue) -> Result<String> {
        self.inner.store.project(project_id)?;
        let id = self.inner.store.add_card(&NewCard {
            id: new_id(),
            project_id: project_id.to_owned(),
            title: issue.title.clone(),
            body: String::new(),
            issue: serde_json::to_string(issue).ok(),
            issue_number: Some(issue.number),
        })?;
        self.inner.changed("");
        Ok(id)
    }

    pub fn card_update(&self, card_id: &str, title: &str, body: &str) -> Result<()> {
        let title = title.trim();
        if title.is_empty() || !self.inner.store.update_card(card_id, title, body.trim())? {
            return Err(Error::InvalidState);
        }
        self.inner.changed("");
        Ok(())
    }

    /// Moves a card within Backlog and Todo. Entering Todo is what asks for it to start, so that's
    /// when a project that could never run it says so, with the card still where it was. A session
    /// that can't work isn't a reason to refuse: the card waits in Todo for it.
    pub fn card_move(
        &self,
        card_id: &str,
        lane: CardLane,
        index: usize,
        language: &str,
    ) -> Result<()> {
        let store = &self.inner.store;
        let card = store.card(card_id)?;
        let waiting = matches!(card.column(), BoardColumn::Backlog | BoardColumn::Todo);
        if !waiting {
            return Err(Error::InvalidState);
        }
        if lane == CardLane::Todo && card.column() != BoardColumn::Todo {
            let project = store.project(&card.project_id)?;
            let workflow = self.default_workflow(&project, language)?;
            self.check_can_run(&project, &workflow.steps)?;
        }
        if !store.move_card(card_id, lane, index, language)? {
            return Err(Error::InvalidState);
        }
        self.inner.changed("");
        Ok(())
    }

    pub fn card_delete(&self, card_id: &str) -> Result<()> {
        if !self.inner.store.delete_card(card_id)? {
            return Err(Error::InvalidState);
        }
        self.inner.changed("");
        Ok(())
    }

    pub fn max_agents(&self) -> usize {
        self.inner.max_agents.load(Ordering::SeqCst)
    }

    /// Changes how many agents work at once, for the whole app. Lowering it stops nothing: what's
    /// running finishes, and nothing new starts until there's room again.
    pub fn set_max_agents(&self, value: usize) -> Result<()> {
        if !MAX_AGENTS_RANGE.contains(&value) {
            return Err(Error::InvalidState);
        }
        self.inner
            .store
            .set_setting(MAX_AGENTS_KEY, &value.to_string())?;
        self.inner.max_agents.store(value, Ordering::SeqCst);
        self.inner.slot_freed.notify_all();
        self.inner.changed("");
        Ok(())
    }

    /// Starts the thread that moves cards out of Todo. Call it once, after `recover`.
    pub fn start_board(&self) {
        // It doesn't keep the engine alive, so one that's gone doesn't go on starting runs.
        let inner = Arc::downgrade(&self.inner);
        let wake = self.inner.board_wake.clone();
        wake.notify();
        std::thread::spawn(move || {
            loop {
                wake.wait(LOOK_AGAIN);
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let session_blocked = Engine { inner }.start_todo_cards().unwrap_or(false);
                // Reading the session takes two processes, and another run's stream can wake this
                // many times a second. Notes left meanwhile are still there afterwards.
                if session_blocked {
                    std::thread::sleep(BLOCKED_PAUSE);
                }
            }
        });
    }

    /// Whether the person has to answer before the app can close: agents are running, or one is
    /// about to. If so, the board carries on, since they may well cancel. If not, it stops here, so
    /// that nothing starts on the way out. On the app's main thread, so it never waits.
    pub fn must_ask_before_quitting(&self) -> bool {
        let mut stopped = match self.inner.board_stopped.try_lock() {
            Ok(stopped) => stopped,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return true,
        };
        let running = self.active_count().unwrap_or(0) > 0;
        if !running {
            *stopped = true;
        }
        running
    }

    /// What the app does once a person has decided to quit. The board stops first, because stopping
    /// agents frees places and Todo cards would take them. Waiting for its lock means a card being
    /// started right now is finished, and is among the agents stopped. Looks again until none are
    /// left, since a run that was between two steps can still add one.
    pub fn shut_down(&self, stop_runs: bool, patience: Duration) {
        *locked(&self.inner.board_stopped) = true;
        if !stop_runs {
            return;
        }
        let deadline = Instant::now() + patience;
        let mut asked = HashSet::new();
        while let Ok(active) = self.inner.store.active_attempts() {
            if active.is_empty() || Instant::now() >= deadline {
                return;
            }
            for attempt in active {
                if asked.insert(attempt.id.clone()) {
                    let _ = self.stop(&attempt.id);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Starts a run from outside the board and puts it there, so all the work is in one place. It
    /// starts now rather than waiting for a place: whoever asked is waiting for a run, and it counts
    /// as working, so Todo cards wait for it. `InvalidState` if its issue's card has a run still open.
    pub fn start_run_on_board(&self, request: StartRun) -> Result<String> {
        if let Some(reason) = self.preflight().blocker {
            return Err(Error::Blocked(reason));
        }
        let store = &self.inner.store;
        let project = store.project(&request.project_id)?;
        self.check_can_run(&project, &request.workflow)?;
        let issue = request.source_issue.as_ref();
        let title = match issue {
            Some(issue) => issue.title.clone(),
            None => title_from(&request.request),
        };
        // If this run is rejected, the card goes back to Backlog, and what's on it is what runs next.
        let body = match issue {
            Some(_) => String::new(),
            None if request.request.trim() == title => String::new(),
            None => request.request.trim().to_owned(),
        };
        let card = store
            .add_started_card(
                &NewCard {
                    id: new_id(),
                    project_id: project.id,
                    title,
                    body,
                    issue: issue.and_then(|issue| serde_json::to_string(issue).ok()),
                    issue_number: issue.map(|issue| issue.number),
                },
                &request.language,
            )?
            .ok_or(Error::InvalidState)?;
        match self.start_run_in_session(request, Some(&card)) {
            Ok(run_id) => {
                self.inner.changed(&run_id);
                Ok(run_id)
            }
            Err(error) => {
                let reason = match &error {
                    Error::Blocked(reason) => reason.clone(),
                    other => Reason::PrepareFailed {
                        detail: other.to_string(),
                    },
                };
                store.card_start_failed(&card, &reason)?;
                self.inner.changed("");
                Err(error)
            }
        }
    }

    /// Starts as many Todo cards as there's room for, on this thread.
    pub fn board_tick(&self) -> Result<()> {
        self.start_todo_cards().map(|_| ())
    }

    /// Says whether the session kept cards from starting. The session is read outside the lock: it
    /// takes two processes, and quitting must not wait for them.
    fn start_todo_cards(&self) -> Result<bool> {
        let store = &self.inner.store;
        if *locked(&self.inner.board_stopped) {
            return Ok(false);
        }
        let now = chrono::Utc::now().timestamp();
        if let Some(reason) = store
            .blocked_reasons()?
            .into_iter()
            .find(|reason| reason.stops_every_card(now))
        {
            self.set_paused(Some(reason));
            return Ok(false);
        }
        // Most ticks come from a stream batch and find nothing to do, so the session is only looked
        // at when a card could actually start.
        if !store.has_todo_cards()? || store.working_runs()? >= self.max_agents() {
            self.set_paused(None);
            return Ok(false);
        }
        if let Some(reason) = self.preflight().blocker {
            self.set_paused(Some(reason));
            return Ok(true);
        }
        self.set_paused(None);

        let stopped = locked(&self.inner.board_stopped);
        if *stopped {
            return Ok(false);
        }
        // Which card, and what it says, is read only now: the session can take a second or two, and
        // a person may have moved or edited it meanwhile.
        while store.working_runs()? < self.max_agents() {
            let Some(card) = store.claim_next_todo_card()? else {
                break;
            };
            self.start_card(&card)?;
        }
        Ok(false)
    }

    fn set_paused(&self, reason: Option<Reason>) {
        let changed = {
            let mut paused = locked(&self.inner.board_paused);
            let changed = *paused != reason;
            *paused = reason;
            changed
        };
        if changed {
            self.inner.changed("");
        }
    }

    /// Starts one card that has been claimed. If it can't start, that goes back to the person:
    /// trying again would only fail the same way.
    fn start_card(&self, card: &Card) -> Result<()> {
        let started = self
            .card_request(card)
            .and_then(|request| self.start_run_in_session(request, Some(&card.id)));
        let run_id = match started {
            Ok(run_id) => run_id,
            Err(error) => {
                let reason = match error {
                    Error::Blocked(reason) => reason,
                    other => Reason::PrepareFailed {
                        detail: other.to_string(),
                    },
                };
                self.inner.store.card_start_failed(&card.id, &reason)?;
                String::new()
            }
        };
        self.inner.changed(&run_id);
        Ok(())
    }

    fn card_request(&self, card: &Card) -> Result<StartRun> {
        let project = self.inner.store.project(&card.project_id)?;
        let language = card.language.clone().unwrap_or_else(|| "en".to_owned());
        let workflow = self.default_workflow(&project, &language)?;
        let issue = issue_of(card);
        let (title, request) = match &issue {
            Some(issue) => (
                title_from_issue(issue.number, &card.title),
                request_from_issue(issue, &card.body),
            ),
            None if card.body.is_empty() => (card.title.clone(), card.title.clone()),
            None => (
                card.title.clone(),
                format!("{}\n\n{}", card.title, card.body),
            ),
        };
        Ok(StartRun {
            project_id: project.id,
            title: Some(title),
            request,
            saved_workflow: Some(workflow.reference()),
            workflow: workflow.steps,
            language,
            source_issue: issue,
            started_by: None,
        })
    }

    fn default_workflow(&self, project: &Project, language: &str) -> Result<Workflow> {
        let id = project
            .default_workflow
            .as_deref()
            .unwrap_or(DEFAULT_WORKFLOW);
        self.workflow(&project.id, id, language)?
            .ok_or(Error::Blocked(Reason::DefaultWorkflowMissing))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Instant;

    use keel_core::{StepSpec, ToolProfile, WorkflowStep};
    use keel_harness::Tools;

    use super::*;
    use crate::Config;

    const FAKE_CLAUDE: &str = r#"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{"loggedIn":true}'; exit 0;;
esac
cat > /dev/null
echo '{"type":"system","subtype":"init","model":"fake","tools":["Read","Grep","Glob"],"apiKeySource":"none"}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Done.","num_turns":1}'
"#;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
    }

    #[test]
    fn any_number_of_notes_before_the_thread_looks_count_as_one() {
        let wake = Wake::default();
        for _ in 0..100 {
            wake.notify();
        }
        assert!(wake.wait(Duration::from_millis(1)));
        assert!(!wake.wait(Duration::from_millis(1)), "that was all of them");
    }

    #[test]
    fn a_change_while_advancing_is_held_returns_and_the_card_starts_once_it_is_free() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("claude"), FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let repo = dir.path().join("demo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        std::fs::write(repo.join("README.md"), "x\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);

        let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
        let mut config = Config::new(dir.path().join("data"));
        config.tick = Duration::from_millis(20);
        config.keep_awake = false;
        let tools = Tools::from_dirs(&[bin, "/usr/bin".into(), "/bin".into()]);
        let engine = Engine::new(store, config, tools, |_| {});
        let project = engine.add_project(&repo).unwrap();
        let workflow = engine
            .save_workflow(
                &project.id,
                &Workflow {
                    id: String::new(),
                    name: "One step".into(),
                    steps: vec![WorkflowStep::Agent(StepSpec {
                        id: "work".into(),
                        model: "fake".into(),
                        effort: None,
                        tools: ToolProfile::ReadOnly,
                        prompt: "{{request}}".into(),
                        max_turns: None,
                        max_budget_usd: None,
                        paths: vec![],
                        when: None,
                        role: Default::default(),
                    })],
                    builtin: false,
                },
            )
            .unwrap();
        engine
            .update_project(
                &project.id,
                &keel_store::ProjectSettings {
                    default_workflow: Some(workflow.id),
                    ..Default::default()
                },
            )
            .unwrap();
        let card = engine.card_create(&project.id, "Fix it", "").unwrap();
        engine.start_board();

        // A move reports a change, and `advance` reports them with this lock held. Reporting has
        // to come back regardless. If the scheduler's work ran on the reporting thread, it would
        // reach `advance` and wait here for a lock that isn't released until it returns.
        let held = locked(&engine.inner.advancing);
        let (sent, received) = mpsc::channel();
        let mover = engine.clone();
        let moved = card.clone();
        std::thread::spawn(move || {
            let result = mover.card_move(&moved, CardLane::Todo, 0, "en");
            mover.inner.changed("run");
            sent.send(result.is_ok()).unwrap();
        });
        let returned = received
            .recv_timeout(Duration::from_secs(10))
            .expect("reporting a change waited for `advancing`");
        assert!(returned);
        drop(held);

        // The scheduler's thread does it, as soon as it can have the lock.
        let deadline = Instant::now() + Duration::from_secs(25);
        while engine.inner.store.card(&card).unwrap().column() != BoardColumn::Done {
            assert!(Instant::now() < deadline, "the card never started");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
