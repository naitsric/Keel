//! The board: cards a person queues up, and the scheduler that starts them. Everything here lives
//! on this machine. Nothing in this file talks to GitHub.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::time::{Duration, Instant};

use keel_core::{
    AttemptState, BoardColumn, CardLane, CiState, PrState, Reason, StepRole, StepSpec, ToolProfile,
    WorkflowStep,
};
use keel_harness::github::Issue;
use keel_store::{Card, McpClient, NewCard, Project, Store};

use crate::notes::{self, split_assistant_notes};
use crate::prompts::part_request;
use crate::{
    Engine, Error, PartOf, Result, StartRun, Workflow, WorkflowRef, joint, locked, new_id,
    request_from_issue, title_from, title_from_issue,
};

const MAX_AGENTS_KEY: &str = "max_agents";
const MAX_AGENTS_RANGE: std::ops::RangeInclusive<usize> = 1..=10;
/// The same as the New Run dialog starts with.
const DEFAULT_WORKFLOW: &str = "builtin:full";
/// Signing in on Terminal or a usage limit resetting tell Keel nothing, so it looks again now and then.
const LOOK_AGAIN: Duration = Duration::from_secs(30);
/// Who a stop says asked for it when the app quits.
const QUITTING: &str = "Keel";
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
    /// What its CI said then, and the checks that failed.
    pub pr_ci: Option<CiState>,
    pub pr_ci_failed: Vec<String>,
    #[serde(serialize_with = "keel_core::serialize_masked")]
    pub rejection: Option<String>,
    #[serde(serialize_with = "keel_core::serialize_masked")]
    pub start_blocked: Option<Reason>,
    pub created_at: String,
    /// The change across several projects it's part of, as the card where it's agreed or as a part.
    pub joint_change_id: Option<String>,
    /// On the card where it's agreed: the other projects its request touches, as the person chose.
    pub also_touches: Vec<String>,
    /// On a part: which one it is, and what it waits for.
    pub part: Option<PartOf>,
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
            pr_ci: card.pr_ci,
            pr_ci_failed: card.pr_ci_failed,
            rejection: card.rejection,
            created_at: card.created_at,
            joint_change_id: card.joint_change_id,
            also_touches: Vec::new(),
            part: None,
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

/// What a run a client closed starts again with, worked out now as start_run or start_task would.
struct RunAgain {
    steps: Vec<WorkflowStep>,
    saved: Option<WorkflowRef>,
    /// A task's one agent is given the request itself.
    task: bool,
}

impl Engine {
    /// Whether a run a client closed could start again: its saved workflow is still there, or it
    /// was a task that only reads.
    pub fn can_run_again(&self, run_id: &str, language: &str) -> Result<bool> {
        Ok(self
            .run_again(&self.inner.store.run(run_id)?, language)?
            .is_some())
    }

    /// What a run a client closed starts again with, nothing copied from it: its saved workflow
    /// as it is today, a built-in one with the project's current commands, or a fresh agent that
    /// only reads for a task. None when the workflow is gone, or the run was neither.
    fn run_again(&self, run: &keel_store::Run, language: &str) -> Result<Option<RunAgain>> {
        if let Some(id) = &run.workflow_id {
            return Ok(self
                .workflow(&run.project_id, id, language)?
                .map(|workflow| RunAgain {
                    saved: Some(workflow.reference()),
                    steps: workflow.steps,
                    task: false,
                }));
        }
        let steps: Vec<WorkflowStep> =
            serde_json::from_value(run.workflow.clone()).unwrap_or_default();
        let mut own = steps
            .iter()
            .filter(|step| !matches!(step, WorkflowStep::Setup { .. }));
        Ok(match (own.next(), own.next()) {
            (Some(WorkflowStep::Agent(spec)), None)
                if spec.tools == ToolProfile::ReadOnly && spec.role == StepRole::Worker =>
            {
                Some(RunAgain {
                    steps: vec![WorkflowStep::Agent(StepSpec {
                        id: spec.id.clone(),
                        model: spec.model.clone(),
                        effort: spec.effort.clone(),
                        tools: ToolProfile::ReadOnly,
                        // What it was asked, until the card's new request takes its place.
                        prompt: run.request.clone(),
                        max_turns: spec.max_turns,
                        max_budget_usd: spec.max_budget_usd,
                        paths: Vec::new(),
                        when: None,
                        risks: Vec::new(),
                        model_when_risky: None,
                        role: StepRole::Worker,
                    })],
                    saved: None,
                    task: true,
                })
            }
            _ => None,
        })
    }

    pub fn board(&self, project_id: &str) -> Result<Board> {
        let store = &self.inner.store;
        let project = store.project(project_id)?;
        let mut cards = Vec::new();
        for card in store.cards(project_id)? {
            // What it says about a change across projects is extra. Not finding it out mustn't keep
            // the person from their board.
            let part = self.inner.part_of(&card).unwrap_or_default();
            let also_touches = if joint::agrees(&card) {
                store
                    .joint_change(&card.id)
                    .map(|change| change.projects)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            cards.push(BoardCard {
                part,
                also_touches,
                ..BoardCard::from(card)
            });
        }
        Ok(Board {
            cards,
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

    /// Adds a note from a client over MCP, `client_id` and named `by`, at the end of a card it may
    /// add to: one it put in Todo, a part waiting there as its change opened it, or one in the
    /// Backlog whose run it closed itself. Never one the person put in Todo or ended. The note is
    /// marked where it starts and ends, with which client and when, in the person's language, and
    /// what the card says stays as it is. `InvalidState` for any other card, looked at as the
    /// note is written, and for a note with nothing in it.
    pub fn card_append_by(
        &self,
        card_id: &str,
        text: &str,
        client_id: &str,
        by: &str,
        language: &str,
    ) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::InvalidState);
        }
        let note = notes::note_by(text, by, language, chrono::Local::now().naive_local());
        if !self.inner.store.append_to_card(card_id, &note, client_id)? {
            return Err(Error::InvalidState);
        }
        self.inner.changed("");
        Ok(())
    }

    /// A client over MCP, by its id, puts a card back at the bottom of Todo to run its last run
    /// again, as start_run or start_task would start it today: see `run_again`. Only a card
    /// waiting in the Backlog because the client closed that run itself, so nothing the person
    /// ended or held back, looked at as the card moves: `InvalidState` for any other, Todo
    /// included, for the card where a change across projects is agreed, which propose_change
    /// starts, and when there's nothing left to run again.
    pub fn card_queue_by(&self, card_id: &str, language: &str, client_id: &str) -> Result<()> {
        let store = &self.inner.store;
        let card = store.card(card_id)?;
        let last = card.run_id.as_deref().ok_or(Error::InvalidState)?;
        let again = self
            .run_again(&store.run(last)?, language)?
            .ok_or(Error::InvalidState)?;
        self.check_can_run(&store.project(&card.project_id)?, &again.steps)?;
        if !store.move_card_as(
            card_id,
            CardLane::Todo,
            usize::MAX,
            language,
            Some(client_id),
        )? {
            return Err(Error::InvalidState);
        }
        self.inner.changed("");
        Ok(())
    }

    /// Revokes a client over MCP. What it put back in Todo goes back to the person, in the
    /// Backlog and saying why, since Keel won't start it.
    pub fn revoke_mcp_client(&self, id: &str) -> Result<McpClient> {
        let client = self.inner.store.revoke_mcp_client(id)?;
        self.inner.changed("");
        Ok(client)
    }

    /// Whether a client over MCP may start runs. Taking that away hands back what it put in Todo,
    /// as revoking it does.
    pub fn set_mcp_client_can_run(&self, id: &str, can_run: bool) -> Result<McpClient> {
        let client = self.inner.store.set_mcp_client_can_run(id, can_run)?;
        self.inner.changed("");
        Ok(client)
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
            let workflow = if joint::agrees(&card) {
                joint::plan_workflow(language)
            } else {
                self.default_workflow(&project, language)?.steps
            };
            self.check_can_run(&project, &workflow)?;
        }
        if !store.move_card(card_id, lane, index, language)? {
            return Err(Error::InvalidState);
        }
        self.inner.changed("");
        Ok(())
    }

    /// Says which other projects a card's request touches. Then it runs as the place where the
    /// change is agreed, and only once that's approved does each project get its part. None makes
    /// it an ordinary card again. Only while the card waits in Backlog or Todo, and never on a part.
    pub fn card_set_projects(&self, card_id: &str, projects: &[String]) -> Result<()> {
        if !self.inner.store.set_joint_projects(card_id, projects)? {
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
                    // Keel asks, not the person, who chose to quit and not to stop this run.
                    let _ = self.stop_by(&attempt.id, QUITTING);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Starts a run from outside the board and puts it there, so all the work is in one place. It
    /// starts now rather than waiting for a place: whoever asked is waiting for a run, and it counts
    /// as working, so Todo cards wait for it. `InvalidState` if its issue's card has a run still open.
    pub fn start_run_on_board(&self, request: StartRun) -> Result<String> {
        self.start_on_board(request, &[])
    }

    /// Starts the run where a change across several projects is agreed, from outside the board: a
    /// card in the request's project, with `also_touches` as the other projects the change
    /// touches, as «Also touches» would on the board. The run only reads and waits for a person to
    /// approve; each part opens after that, as it does from a card. Whatever workflow the request
    /// came with is ignored, since the agreement is always the same. The project's own and repeats
    /// are left out as on the board. `InvalidState` if that leaves none, or if the issue's card has
    /// a run still open; `NotFound` for a project that's missing or was removed, with no card
    /// created; `Blocked(StartsOnTheBoard)` if the issue already is a change across projects.
    pub fn propose_change_on_board(
        &self,
        mut request: StartRun,
        also_touches: &[String],
    ) -> Result<String> {
        if also_touches
            .iter()
            .all(|project| *project == request.project_id)
        {
            return Err(Error::InvalidState);
        }
        request.workflow = joint::plan_workflow(&request.language);
        request.saved_workflow = None;
        self.start_on_board(request, also_touches)
    }

    fn start_on_board(&self, request: StartRun, joint_projects: &[String]) -> Result<String> {
        if let Some(reason) = self.preflight().blocker {
            return Err(Error::Blocked(reason));
        }
        let store = &self.inner.store;
        let project = store.project(&request.project_id)?;
        let issue = request.source_issue.as_ref();
        // Starting it here would skip agreeing on it, and every project but this one.
        if let Some(issue) = issue
            && store.cards(&project.id)?.iter().any(|card| {
                card.issue_number == Some(issue.number) && card.joint_change_id.is_some()
            })
        {
            return Err(Error::Blocked(Reason::StartsOnTheBoard));
        }
        self.check_can_run(&project, &request.workflow)?;
        // The card says what the run is called, so the person finds it by the same name.
        let title = match issue {
            Some(issue) => issue.title.clone(),
            None => request
                .title
                .clone()
                .unwrap_or_else(|| title_from(&request.request)),
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
                joint_projects,
            )
            .map_err(|error| match error {
                // It became one after the look above: «Also touches» was marked in between.
                keel_store::Error::AcrossProjects => Error::Blocked(Reason::StartsOnTheBoard),
                other => other.into(),
            })?
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
                    // Its client lost the leave to run after the card was taken from Todo.
                    Error::Store(keel_store::Error::ClientMayNotRun(client)) => {
                        Reason::AssistantMayNotRun { client }
                    }
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
        // A client that put the card back in Todo runs its last run again, as it was.
        // Worked out now, as start_run would: the project's commands, or its workflow, may have
        // changed since that run, or the workflow may be gone.
        let rerun = match (&card.queued_by_client, &card.rerun_of) {
            (Some(_), Some(run)) => {
                let run = self.inner.store.run(run)?;
                match (self.run_again(&run, &language)?, run.workflow_id) {
                    (Some(again), _) => Some(again),
                    (None, Some(id)) => {
                        return Err(Error::Blocked(Reason::RerunWorkflowMissing {
                            workflow: run.workflow_name.unwrap_or(id),
                        }));
                    }
                    // It was put in Todo only if it could run again, and what it was can't change.
                    (None, None) => return Err(Error::InvalidState),
                }
            }
            _ => None,
        };
        let (mut workflow, saved_workflow) = match &rerun {
            Some(again) => (again.steps.clone(), again.saved.clone()),
            None if joint::agrees(card) => (joint::plan_workflow(&language), None),
            None => {
                let workflow = self.default_workflow(&project, &language)?;
                let reference = workflow.reference();
                (workflow.steps, Some(reference))
            }
        };
        let issue = issue_of(card);
        // What an assistant added goes after what the person wrote, or what was agreed, and apart.
        let (said, notes) = split_assistant_notes(&card.body);
        let (title, request) = match &issue {
            // A part may be in another repo, where the issue's number would point at something else.
            _ if card.joint_position.is_some() => (
                card.title.clone(),
                part_request(
                    &card.title,
                    &said,
                    self.inner.asked_of(card)?.as_deref(),
                    issue.as_ref(),
                ),
            ),
            Some(issue) => (
                title_from_issue(issue.number, &card.title),
                request_from_issue(issue, &said),
            ),
            None if said.is_empty() => (card.title.clone(), card.title.clone()),
            None => (card.title.clone(), format!("{}\n\n{said}", card.title)),
        };
        let request = notes::with_notes(&request, &notes, &language);
        // A task's one agent is given the request itself, as start_task gives it the prompt.
        if rerun.as_ref().is_some_and(|again| again.task) {
            for step in &mut workflow {
                if let WorkflowStep::Agent(spec) = step {
                    spec.prompt.clone_from(&request);
                }
            }
        }
        // A client over MCP that put the card in Todo is who the run says started it, as when it
        // starts one itself.
        let started_by = match &card.queued_by_client {
            Some(client) => Some(self.inner.store.mcp_client(client)?.name),
            None => None,
        };
        Ok(StartRun {
            project_id: project.id,
            title: Some(title),
            request,
            saved_workflow,
            workflow,
            language,
            source_issue: issue,
            started_by,
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

    /// An engine whose `claude` answers at once, with a project in `dir`.
    fn engine_in(dir: &Path) -> (Engine, Project) {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("claude"), FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let repo = dir.join("demo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        std::fs::write(repo.join("README.md"), "x\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);

        let store = Arc::new(Store::open(&dir.join("keel.db")).unwrap());
        let mut config = Config::new(dir.join("data"));
        config.tick = Duration::from_millis(20);
        config.keep_awake = false;
        let tools = Tools::from_dirs(&[bin, "/usr/bin".into(), "/bin".into()]);
        let engine = Engine::new(store, config, tools, |_| {});
        let project = engine.add_project(&repo).unwrap();
        (engine, project)
    }

    fn read_only(id: &str) -> WorkflowStep {
        WorkflowStep::Agent(StepSpec {
            id: id.into(),
            model: "fake".into(),
            effort: None,
            tools: ToolProfile::ReadOnly,
            prompt: "{{request}}".into(),
            max_turns: None,
            max_budget_usd: None,
            paths: vec![],
            when: None,
            risks: vec![],
            model_when_risky: None,
            role: Default::default(),
        })
    }

    #[test]
    fn a_card_taken_from_todo_starts_no_run_once_its_client_may_not_run() {
        keel_harness::needs!(sandbox);
        let dir = tempfile::tempdir().unwrap();
        let (engine, project) = engine_in(dir.path());
        let store = engine.inner.store.clone();
        let workflow = engine
            .save_workflow(
                &project.id,
                &Workflow {
                    id: String::new(),
                    name: "Then ask".into(),
                    steps: vec![
                        read_only("work"),
                        WorkflowStep::Approval { id: "ask".into() },
                    ],
                    builtin: false,
                },
            )
            .unwrap();
        let client = store
            .add_mcp_client("c1", "Claude Code", &[7u8; 32], "keel_abc", true)
            .unwrap();
        // The client's run, which it closes itself and puts back in Todo.
        let run = engine
            .start_run_on_board(StartRun {
                project_id: project.id.clone(),
                title: None,
                request: "Fix it".into(),
                workflow: workflow.steps.clone(),
                language: "en".into(),
                source_issue: None,
                saved_workflow: Some(workflow.reference()),
                started_by: Some(client.name.clone()),
            })
            .unwrap();
        let card = store.card_of_run(&run).unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(25);
        let approval = loop {
            let waiting = store
                .attempts_of_run(&run)
                .unwrap()
                .into_iter()
                .find(|a| a.state == AttemptState::WaitingApproval.as_str());
            if let Some(approval) = waiting {
                break approval.id;
            }
            assert!(Instant::now() < deadline, "never reached the approval");
            std::thread::sleep(Duration::from_millis(20));
        };
        engine.abandon_by(&approval, &client.name).unwrap();
        for tool in ["start_run", "close_run"] {
            store
                .add_mcp_call(&keel_store::NewMcpCall {
                    client: &client,
                    tool,
                    args: "{}",
                    error: None,
                    run_id: Some(&run),
                    duration_ms: 1,
                })
                .unwrap();
        }
        while store.card(&card).unwrap().column() != BoardColumn::Backlog {
            assert!(Instant::now() < deadline, "the run never closed");
            std::thread::sleep(Duration::from_millis(20));
        }
        engine.card_queue_by(&card, "en", &client.id).unwrap();

        // Keel takes it from Todo, and the person takes away the client's leave to run before
        // its run is added: none is, and the card is the person's again, saying why.
        let taken = store.claim_next_todo_card().unwrap().unwrap();
        engine.set_mcp_client_can_run(&client.id, false).unwrap();
        engine.start_card(&taken).unwrap();
        let back = store.card(&card).unwrap();
        assert_eq!(back.column(), BoardColumn::Backlog);
        assert_eq!(back.run_id, None);
        assert_eq!(
            serde_json::from_value::<Reason>(back.start_blocked.unwrap()).unwrap(),
            Reason::AssistantMayNotRun {
                client: "Claude Code".into()
            }
        );
        assert_eq!(
            store.runs(10).unwrap().len(),
            1,
            "only the client's first run"
        );
    }

    #[test]
    fn a_change_while_advancing_is_held_returns_and_the_card_starts_once_it_is_free() {
        keel_harness::needs!(sandbox);
        let dir = tempfile::tempdir().unwrap();
        let (engine, project) = engine_in(dir.path());
        let workflow = engine
            .save_workflow(
                &project.id,
                &Workflow {
                    id: String::new(),
                    name: "One step".into(),
                    steps: vec![read_only("work")],
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
