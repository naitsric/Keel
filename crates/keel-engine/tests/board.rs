//! The board, from a person's moves to where each card ends up, against a fake `claude`, a fake
//! `gh` and a local bare repo standing in for GitHub. Tests start the scheduler's real thread and
//! then only do what a person can do, unless a test is about a single tick.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, BoardColumn, CardLane, Reason, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{
    BoardCard, BoardIssue, Config, Engine, Error, StartRun, Workflow, request_from_issue,
};
use keel_harness::Tools;
use keel_store::{NewAttempt, NewRun, ProjectSettings, Store};

/// Less than the scheduler's own look every 30 seconds, so that can't pass for a wake-up.
const PATIENCE: Duration = Duration::from_secs(25);
/// Long enough for anything that shouldn't happen to have happened.
const A_MOMENT: Duration = Duration::from_millis(400);

type Changes = Arc<Mutex<Vec<ThreadId>>>;
/// While set, reporting a change waits. Starting a card reports one from inside `advance`, so this
/// holds a promotion in the middle for as long as a test needs.
type Gate = Arc<AtomicBool>;

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
    remote: PathBuf,
    log: PathBuf,
    flags: PathBuf,
    changes: Changes,
    gate: Gate,
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write_script(path: &Path, text: &str) {
    keel_harness::testing::write_script(path, text).unwrap();
}

/// A `card-N` in the prompt says which card this is, which is how a test holds one back or makes
/// it fail. The schema says what kind of answer the step needs. The engine sends SIGINT twice, so
/// an agent that's slow to stop has to ignore the second one.
const FAKE_CLAUDE: &str = r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth)
  echo auth >> "@LOG@/auth.log"
  if [ -f "@FLAGS@/slow-auth" ]; then sleep 1.5; fi
  if [ -f "@FLAGS@/hung-auth" ]; then sleep 60; fi
  if [ -f "@FLAGS@/logged-out" ]; then echo '{"loggedIn":false}'; exit 0; fi
  echo '{"loggedIn":true,"subscriptionType":"max","email":"a@example.com"}'; exit 0;;
esac
prompt="$(cat)"
tools=""; schema=""; prev=""
for a in "$@"; do
  [ "$prev" = "--tools" ] && tools="$a"
  [ "$prev" = "--json-schema" ] && schema="$a"
  prev="$a"
done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
[ -n "$schema" ] && list="$list\",\"StructuredOutput"
echo "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}"
card=$(printf '%s' "$prompt" | grep -oE 'card-[0-9]+' | head -1)
if [ "$card" = "card-2" ] && [ -f "@FLAGS@/slow-stop" ]; then trap 'trap "" INT; sleep 2; exit 130' INT; fi
echo "START $card" >> "@LOG@/calls.log"
if [ -n "$card" ] && [ -f "@FLAGS@/hold" ]; then
  until [ -f "@FLAGS@/go-$card" ] || [ ! -d "@FLAGS@" ]; do sleep 0.02; done
fi
echo "END $card" >> "@LOG@/calls.log"
failure() {
  echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"$1\",\"num_turns\":1}"
  exit 1
}
[ -n "$card" ] && [ -f "@FLAGS@/fail-$card" ] && failure "it broke"
[ -n "$card" ] && [ -f "@FLAGS@/limit-$card" ] && failure "You have reached your usage limit"
[ -n "$card" ] && [ -f "@FLAGS@/session-$card" ] && failure "Invalid API key"
case "$schema" in
*unverified*) output='{"criteria":[{"id":"C1","result":"met","evidence":"tests/fixed.rs:1"}],"summary":"A test proves it"}';;
*verdict*)
  case "$prompt" in *SECURITY*) [ -f "@FLAGS@/fail-security" ] && failure "the review broke";; esac
  case "$prompt" in *"the CODE"*)
    if [ -f "@FLAGS@/hold-code" ]; then
      until [ -f "@FLAGS@/go-code" ] || [ ! -d "@FLAGS@" ]; do sleep 0.02; done
    fi;;
  esac
  output='{"verdict":"approved","findings":[],"summary":"fine"}';;
*criteria*) output='{"summary":"It gets fixed","criteria":[{"id":"C1","when":"it runs","then":"it works"}],"changes":[],"limits":[],"decided":[],"questions":[]}';;
*decision*) output='{"decision":"approve","risk":"low","reasons":"fine","instructions":""}';;
*learnings*)
  [ -f "@FLAGS@/fail-learner" ] && failure "no proposals today"
  output='{"learnings":[]}';;
*) output='';;
esac
if [ -z "$schema" ] && [ "$tools" != "Read,Grep,Glob" ] && [ ! -f "@FLAGS@/no-change" ]; then
  echo work >> fixed.txt
  git add -A && git commit -qm "fix"
fi
if [ -n "$output" ]; then
  echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"num_turns\":1,\"structured_output\":$output}"
else
  echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Done.\",\"num_turns\":1}"
fi
"##;

const FAKE_GH: &str = r##"#!/bin/bash
echo "gh $*" >> "@LOG@/gh.log"
case "$1 $2" in
"auth token")
  if [ -f "@FLAGS@/gh-signed-out" ]; then echo "You are not logged in. Run gh auth login" >&2; exit 1; fi
  echo "tok-alice"; exit 0;;
"repo view") echo '{"defaultBranchRef":{"name":"main"},"viewerPermission":"WRITE","isPrivate":true}'; exit 0;;
"api repos/acme/app/branches/main") echo false; exit 0;;
"issue list") echo '[{"number":7,"title":"Broken","author":{"login":"carol"},"url":"https://github.com/acme/app/issues/7","body":"It breaks"}]'; exit 0;;
"pr create") echo "https://github.com/acme/app/pull/12"; exit 0;;
esac
echo "unexpected: $*" >&2; exit 1
"##;

fn tools_in(dirs: &[&Path]) -> Tools {
    let mut all: Vec<PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
    all.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    Tools::from_dirs(&all)
}

fn open_engine(
    root: &Path,
    store: &Arc<Store>,
    changes: &Changes,
    gate: &Gate,
    tune: impl FnOnce(&mut Config),
) -> Engine {
    let mut config = Config::new(root.join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    config.stop_grace = Duration::from_millis(500);
    config.github_git_base = format!("file://{}/remotes", root.display());
    tune(&mut config);
    let changes = changes.clone();
    let gate = gate.clone();
    Engine::new(
        store.clone(),
        config,
        tools_in(&[&root.join("bin")]),
        move |_| {
            changes.lock().unwrap().push(std::thread::current().id());
            while gate.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        },
    )
}

fn settings(account: Option<&str>, workflow: Option<&str>) -> ProjectSettings {
    ProjectSettings {
        github_account: account.map(str::to_owned),
        default_workflow: workflow.map(str::to_owned),
        ..ProjectSettings::default()
    }
}

/// A repo with some history, of a project on no remote.
fn new_repo(root: &Path, name: &str) -> PathBuf {
    let repo = root.join(name);
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Tester"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "published\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn world_with(tune: impl FnOnce(&mut Config)) -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (bin, log, flags) = (root.join("bin"), root.join("log"), root.join("flags"));
    for path in [&bin, &log, &flags] {
        std::fs::create_dir(path).unwrap();
    }
    let fill = |text: &str| {
        text.replace("@LOG@", &log.to_string_lossy())
            .replace("@FLAGS@", &flags.to_string_lossy())
    };
    write_script(&bin.join("claude"), &fill(FAKE_CLAUDE));
    write_script(&bin.join("gh"), &fill(FAKE_GH));

    let remote = root.join("remotes/acme/app.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let repo = new_repo(root, "app");
    git(&repo, &["push", "-q", &remote.to_string_lossy(), "main"]);
    git(
        &repo,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );

    let store = Arc::new(Store::open(&root.join("keel.db")).unwrap());
    let changes = Changes::default();
    let gate = Gate::default();
    let engine = open_engine(root, &store, &changes, &gate, tune);
    let project = engine.add_project(&repo).unwrap();
    engine
        .update_project(&project.id, &settings(Some("alice"), None))
        .unwrap();
    World {
        dir,
        store,
        engine,
        project_id: project.id,
        remote,
        log,
        flags,
        changes,
        gate,
    }
}

fn world() -> World {
    world_with(|_| {})
}

fn agent(id: &str, tools: ToolProfile) -> WorkflowStep {
    WorkflowStep::Agent(StepSpec {
        id: id.into(),
        model: "fake".into(),
        effort: None,
        tools,
        prompt: "{{request}}".into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        role: Default::default(),
    })
}

fn saved(engine: &Engine, project: &str, name: &str, steps: Vec<WorkflowStep>) -> String {
    engine
        .save_workflow(
            project,
            &Workflow {
                id: String::new(),
                name: name.into(),
                steps,
                builtin: false,
            },
        )
        .unwrap()
        .id
}

/// One read-only agent. Nothing to deliver, and several can run at once in a project.
fn one_step(engine: &Engine, project: &str) -> String {
    saved(
        engine,
        project,
        "One step",
        vec![agent("work", ToolProfile::ReadOnly)],
    )
}

fn answer_step(engine: &Engine, project: &str) -> String {
    saved(
        engine,
        project,
        "Then ask",
        vec![
            agent("work", ToolProfile::ReadOnly),
            WorkflowStep::Approval { id: "ask".into() },
        ],
    )
}

impl World {
    fn set_default(&self, workflow: &str) {
        self.engine
            .update_project(&self.project_id, &settings(Some("alice"), Some(workflow)))
            .unwrap();
    }

    fn use_one_step(&self) {
        self.set_default(&one_step(&self.engine, &self.project_id));
    }

    fn new_card(&self, title: &str) -> String {
        self.engine
            .card_create(&self.project_id, title, "")
            .unwrap()
    }

    /// A new card, at the bottom of Todo.
    fn queued(&self, title: &str) -> String {
        let id = self.new_card(title);
        self.engine
            .card_move(&id, CardLane::Todo, usize::MAX, "en")
            .unwrap();
        id
    }

    fn card(&self, id: &str) -> BoardCard {
        self.engine
            .board(&self.project_id)
            .unwrap()
            .cards
            .into_iter()
            .find(|c| c.id == id)
            .unwrap()
    }

    fn titles_in(&self, column: BoardColumn) -> Vec<String> {
        self.engine
            .board(&self.project_id)
            .unwrap()
            .cards
            .into_iter()
            .filter(|c| c.column == column)
            .map(|c| c.title)
            .collect()
    }

    fn working(&self) -> usize {
        self.engine.board(&self.project_id).unwrap().working
    }

    /// The attempt of a run's approval step, once it's waiting.
    fn wait_approval(&self, run: &str) -> String {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let waiting = self
                .store
                .attempts_of_run(run)
                .unwrap()
                .into_iter()
                .find(|attempt| attempt.state == AttemptState::WaitingApproval.as_str());
            if let Some(attempt) = waiting {
                return attempt.id;
            }
            assert!(Instant::now() < deadline, "never reached approval");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_column(&self, id: &str, column: BoardColumn) -> BoardCard {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let card = self.card(id);
            if card.column == column {
                return card;
            }
            assert!(
                Instant::now() < deadline,
                "{} never reached {column:?}: {:?} {:?} {:?}\n{}",
                card.title,
                card.column,
                card.run_state,
                card.reason,
                self.read_log("calls.log"),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The latest attempt of a card's run, which is where a person answers or acts.
    fn attempt(&self, id: &str) -> keel_store::Attempt {
        let run = self.card(id).run_id.expect("it has a run");
        self.store.attempts_of_run(&run).unwrap().pop().unwrap()
    }

    fn read_log(&self, name: &str) -> String {
        std::fs::read_to_string(self.log.join(name)).unwrap_or_default()
    }

    /// The cards agents have started on, in the order they did.
    fn started(&self) -> Vec<String> {
        self.read_log("calls.log")
            .lines()
            .filter_map(|l| l.strip_prefix("START "))
            .map(str::to_owned)
            .collect()
    }

    fn wait_started(&self, count: usize) -> Vec<String> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let started = self.started();
            if started.len() >= count {
                return started;
            }
            assert!(
                Instant::now() < deadline,
                "only {started:?} started, wanted {count}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn flag(&self, name: &str) {
        std::fs::write(self.flags.join(name), "").unwrap();
    }

    fn unflag(&self, name: &str) {
        std::fs::remove_file(self.flags.join(name)).unwrap();
    }

    /// `gh` commands that aren't reading anything.
    fn github_writes(&self) -> Vec<String> {
        self.read_log("gh.log")
            .lines()
            .filter(|line| {
                !(line.starts_with("gh auth token --user alice")
                    || line.starts_with("gh repo view acme/app --json")
                    || line.starts_with("gh api repos/acme/app/branches/main --jq .protected")
                    || line.starts_with("gh issue list --repo acme/app"))
            })
            .map(str::to_owned)
            .collect()
    }

    fn remote_branches(&self) -> Vec<String> {
        git(
            &self.remote,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    /// The app closes and opens again over the same data.
    fn reopen(&mut self, tune: impl FnOnce(&mut Config)) {
        self.engine = open_engine(
            self.dir.path(),
            &self.store,
            &self.changes,
            &self.gate,
            tune,
        );
    }

    /// A card whose run was left as an app that died would leave it: an attempt in `state`, with
    /// nobody watching and no process behind it.
    fn left_behind(&self, title: &str, step: WorkflowStep, state: AttemptState) -> String {
        let id = self.new_card(title);
        self.engine.card_move(&id, CardLane::Todo, 0, "en").unwrap();
        let claimed = self.store.claim_next_todo_card().unwrap().unwrap();
        assert_eq!(claimed.id, id);
        let run = format!("run-{title}");
        self.store
            .add_run_for_card(
                &NewRun {
                    id: run.clone(),
                    project_id: self.project_id.clone(),
                    title: title.into(),
                    request: title.into(),
                    workflow: serde_json::to_string(&[&step]).unwrap(),
                    language: "en".into(),
                    source_issue: None,
                    request_written: None,
                    workflow_id: None,
                    workflow_name: None,
                    started_by: None,
                },
                &id,
            )
            .unwrap();
        let workspace = self.dir.path().join("data/runs").join(&run);
        self.store
            .add_attempt(&NewAttempt {
                id: format!("attempt-{title}"),
                run_id: run.clone(),
                step_id: step.id().to_owned(),
                kind: step.kind().to_owned(),
                role: "worker".into(),
                round: 0,
                number: 1,
                state,
                model_requested: "fake".into(),
                effort: None,
                tools: "none".into(),
                max_turns: None,
                max_budget_usd: None,
                language: "en".into(),
                prompt: title.into(),
                system_prompt_sources: "[]".into(),
                settings: "{}".into(),
                session_id: format!("session-{title}"),
                resumed_from: None,
                workspace: workspace.to_string_lossy().into_owned(),
                raw_path: workspace.join("raw.log").to_string_lossy().into_owned(),
                started_at: keel_store::now(),
            })
            .unwrap();
        id
    }

    /// A run started by hand, as the New Run dialog and an MCP client both start one.
    fn by_hand(&self, title: &str) -> String {
        self.engine
            .start_run(StartRun {
                project_id: self.project_id.clone(),
                title: None,
                request: title.into(),
                workflow: vec![agent("work", ToolProfile::ReadOnly)],
                language: "en".into(),
                source_issue: None,
                saved_workflow: None,
                started_by: None,
            })
            .unwrap()
    }

    fn paused(&self) -> Option<Reason> {
        self.engine.board(&self.project_id).unwrap().paused
    }

    fn wait_paused(&self, reason: Option<Reason>) {
        let deadline = Instant::now() + PATIENCE;
        while self.paused() != reason {
            assert!(
                Instant::now() < deadline,
                "paused is {:?}, wanted {reason:?}",
                self.paused()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn session_reads(&self) -> usize {
        self.read_log("auth.log").lines().count()
    }
}

fn workflow_of(run: &keel_store::Run) -> Vec<WorkflowStep> {
    serde_json::from_value(run.workflow.clone()).unwrap()
}

// Criterion 2: moving a card to Todo starts it, with no other call.

#[test]
fn moving_to_todo_starts_the_card_with_the_full_workflow_when_none_was_chosen() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.engine.start_board();
    let id = w
        .engine
        .card_create(&w.project_id, "Fix card-1", "It breaks\non login")
        .unwrap();

    std::thread::sleep(A_MOMENT);
    assert_eq!(w.card(&id).column, BoardColumn::Backlog);
    assert!(w.store.open_runs().unwrap().is_empty(), "a backlog waits");

    w.engine.card_move(&id, CardLane::Todo, 0, "en").unwrap();
    let card = w.wait_column(&id, BoardColumn::HumanReview);

    let run = w.store.run(card.run_id.as_ref().unwrap()).unwrap();
    assert_eq!(run.title, "Fix card-1");
    assert_eq!(run.request, "Fix card-1\n\nIt breaks\non login");
    assert_eq!(run.language, "en");
    assert_eq!(run.source_issue, None);
    let full = w
        .engine
        .workflow(&w.project_id, "builtin:full", "en")
        .unwrap()
        .unwrap();
    assert_eq!(workflow_of(&run), full.steps);
    assert_eq!(card.run_state, Some(AttemptState::WaitingApproval));
    assert_eq!(w.attempt(&id).step_id, "human");
    // It really did go through them.
    assert!(
        w.store
            .attempts_of_run(&run.id)
            .unwrap()
            .iter()
            .any(|a| a.step_id == "approver" && a.state == "succeeded")
    );
}

#[test]
fn the_project_can_choose_the_workflow_and_the_card_brings_its_language() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.engine.start_board();
    let id = w.new_card("card-1");
    w.engine.card_move(&id, CardLane::Todo, 0, "es").unwrap();
    let card = w.wait_column(&id, BoardColumn::HumanReview);

    let run = w.store.run(card.run_id.as_ref().unwrap()).unwrap();
    assert_eq!(run.language, "es");
    let simple = w
        .engine
        .workflow(&w.project_id, "builtin:simple", "es")
        .unwrap()
        .unwrap();
    assert_eq!(workflow_of(&run), simple.steps);
    assert_eq!(run.request, "card-1", "no body, no blank line");
    assert!(
        workflow_of(&run).iter().any(|s| s.id() == "aprendizajes"),
        "the steps are the Spanish ones"
    );
    assert_eq!(
        w.engine.board(&w.project_id).unwrap().default_workflow,
        Some("builtin:simple".to_owned())
    );
    assert_eq!(
        w.store.project(&w.project_id).unwrap().default_workflow,
        Some("builtin:simple".to_owned())
    );
}

#[test]
fn an_issue_is_imported_once_and_starts_as_the_new_run_dialog_would_start_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    let issue = w.engine.issues(&w.project_id).unwrap().remove(0);
    let id = w.engine.card_import_issue(&w.project_id, &issue).unwrap();
    assert_eq!(
        w.engine.card_import_issue(&w.project_id, &issue).unwrap(),
        id,
        "the same issue is the same card"
    );
    let board = w.engine.board(&w.project_id).unwrap();
    assert_eq!(board.cards.len(), 1);
    assert_eq!(
        board.cards[0].issue,
        Some(BoardIssue {
            number: 7,
            title: "Broken".into(),
            author: "carol".into(),
            url: "https://github.com/acme/app/issues/7".into(),
        })
    );
    assert_eq!(board.cards[0].title, "Broken");

    // What the person adds goes where the dialog puts it.
    w.engine
        .card_update(&id, "Broken", "Keep it small")
        .unwrap();
    w.engine.start_board();
    w.engine.card_move(&id, CardLane::Todo, 0, "en").unwrap();
    let card = w.wait_column(&id, BoardColumn::HumanReview);

    let run = w.store.run(card.run_id.as_ref().unwrap()).unwrap();
    assert_eq!(run.title, "#7 Broken");
    assert_eq!(run.request, request_from_issue(&issue, "Keep it small"));
    assert!(run.request.contains("hasn't been verified"));
    assert_eq!(run.source_issue.unwrap()["number"], 7);
    assert_eq!(
        w.read_log("gh.log").matches("issue list").count(),
        1,
        "the card is a copy: nothing looks at the issue again"
    );
}

// A `claude` that stops answering about the session must not stop the board in silence.

#[test]
fn a_claude_that_stops_answering_pauses_the_board_and_says_why() {
    keel_harness::needs!(sandbox);
    let w = world_with(|config| config.session_check_limit = Duration::from_millis(300));
    w.use_one_step();
    w.flag("hung-auth");
    let preflight = w.engine.preflight();
    assert_eq!(preflight.blocker, Some(Reason::ClaudeNotResponding));

    w.engine.start_board();
    let card = w.queued("Waits for the session");
    w.wait_paused(Some(Reason::ClaudeNotResponding));
    assert_eq!(w.card(&card).column, BoardColumn::Todo);
    assert!(w.store.runs(10).unwrap().is_empty());

    // Once it answers again, the card goes.
    w.unflag("hung-auth");
    w.engine.board_tick().unwrap();
    w.wait_paused(None);
    w.wait_column(&card, BoardColumn::Done);
}

// A run started from outside the board, as an assistant does over MCP, is on the board too.

fn outside_run(w: &World, workflow: Vec<WorkflowStep>, request: &str) -> StartRun {
    StartRun {
        project_id: w.project_id.clone(),
        title: None,
        request: request.to_owned(),
        workflow,
        language: "en".into(),
        source_issue: None,
        saved_workflow: None,
        started_by: None,
    }
}

fn steps_of(w: &World, id: &str) -> Vec<WorkflowStep> {
    w.engine
        .workflows(&w.project_id, "en")
        .unwrap()
        .into_iter()
        .find(|workflow| workflow.id == id)
        .unwrap()
        .steps
}

#[test]
fn a_run_from_outside_gets_a_card_that_follows_the_columns_and_starts_once() {
    keel_harness::needs!(sandbox);
    let mut w = world();
    let workflow = answer_step(&w.engine, &w.project_id);
    w.engine.start_board();
    let run = w
        .engine
        .start_run_on_board(outside_run(
            &w,
            steps_of(&w, &workflow),
            "Add a triple function\n\nWith its test.",
        ))
        .unwrap();

    let card = w.store.card_of_run(&run).unwrap().expect("it has a card");
    let waiting = w.wait_column(&card, BoardColumn::HumanReview);
    assert_eq!(waiting.title, "Add a triple function");
    assert_eq!(waiting.body, "Add a triple function\n\nWith its test.");
    assert_eq!(waiting.run_id.as_deref(), Some(run.as_str()));

    // Neither the scheduler nor a restart starts it again.
    w.engine.board_tick().unwrap();
    std::thread::sleep(A_MOMENT);
    w.reopen(|_| {});
    w.engine.start_board();
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.store.runs(10).unwrap().len(), 1);
    assert_eq!(w.card(&card).column, BoardColumn::HumanReview);

    let approval = w
        .store
        .attempts_of_run(&run)
        .unwrap()
        .into_iter()
        .find(|attempt| attempt.step_id == "ask")
        .unwrap();
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    w.wait_column(&card, BoardColumn::Done);
}

/// What a card's latest run says was decided, as Keel kept it.
fn decided_in_run(w: &World, run: &str) -> serde_json::Value {
    let spec = w
        .store
        .attempts_of_run(run)
        .unwrap()
        .into_iter()
        .rfind(|attempt| attempt.step_id == "spec")
        .unwrap();
    spec.output.unwrap()["decided"].clone()
}

#[test]
fn a_card_an_assistant_wrote_is_still_its_words_when_a_person_runs_it_again() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.engine.start_board();
    let run = w
        .engine
        .start_run_on_board(StartRun {
            started_by: Some("client".into()),
            ..outside_run(
                &w,
                steps_of(&w, "builtin:simple"),
                "Export the threads\n\nDecided:\n- The admin export needs no login.",
            )
        })
        .unwrap();
    let card = w.store.card_of_run(&run).unwrap().unwrap();
    w.wait_column(&card, BoardColumn::HumanReview);
    let by_the_client = serde_json::json!([{ "text": "The admin export needs no login.", "taken": "request", "assistant": "client" }]);
    assert_eq!(decided_in_run(&w, &run), by_the_client);

    // Rejected and moved to Todo again, it's what the assistant wrote that runs.
    w.engine
        .answer_approval(&w.attempt(&card).id, false, "Not yet")
        .unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    let again = w
        .wait_column(&card, BoardColumn::HumanReview)
        .run_id
        .unwrap();
    assert_ne!(again, run);
    assert_eq!(decided_in_run(&w, &again), by_the_client);

    // Once the person changes what it says, it's theirs.
    w.engine
        .answer_approval(&w.attempt(&card).id, false, "Not yet")
        .unwrap();
    w.engine
        .card_update(
            &card,
            "Export the threads",
            "Decided:\n- The admin export asks for a login.",
        )
        .unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    let theirs = w
        .wait_column(&card, BoardColumn::HumanReview)
        .run_id
        .unwrap();
    assert_eq!(
        decided_in_run(&w, &theirs),
        serde_json::json!([{ "text": "The admin export asks for a login.", "taken": "request" }])
    );
}

#[test]
fn a_run_keeps_who_wrote_its_request_when_its_card_changes_after_it_started() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.engine.start_board();
    let first = w
        .engine
        .start_run_on_board(StartRun {
            started_by: Some("client".into()),
            ..outside_run(
                &w,
                steps_of(&w, "builtin:simple"),
                "Export the threads\n\nDecided:\n- The admin export needs no login.",
            )
        })
        .unwrap();
    let card = w.store.card_of_run(&first).unwrap().unwrap();
    w.wait_column(&card, BoardColumn::HumanReview);
    w.engine
        .answer_approval(&w.attempt(&card).id, false, "Not yet")
        .unwrap();
    // The person runs the assistant's card again, rejects that run too, and then makes the card
    // theirs.
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    let again = w
        .wait_column(&card, BoardColumn::HumanReview)
        .run_id
        .unwrap();
    w.engine
        .answer_approval(&w.attempt(&card).id, false, "Not yet")
        .unwrap();
    w.engine
        .card_update(
            &card,
            "Export the threads",
            "Decided:\n- The admin export needs no login.\n- Nor a token.",
        )
        .unwrap();

    // Its spec, tried again, is still what the assistant wrote.
    let spec = |run: &str| {
        w.store
            .attempts_of_run(run)
            .unwrap()
            .into_iter()
            .filter(|attempt| attempt.step_id == "spec")
            .collect::<Vec<_>>()
    };
    w.engine
        .retry(&spec(&again).pop().unwrap().id, false)
        .unwrap();
    let deadline = Instant::now() + PATIENCE;
    while !spec(&again)
        .get(1)
        .is_some_and(|attempt| attempt.state == AttemptState::Succeeded.as_str())
    {
        assert!(Instant::now() < deadline, "{:?}", spec(&again));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        decided_in_run(&w, &again),
        serde_json::json!([{ "text": "The admin export needs no login.", "taken": "request", "assistant": "client" }])
    );
}

#[test]
fn a_card_s_title_never_decides_anything_whoever_wrote_its_body() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.engine.start_board();
    // An assistant names its card with a list, and the person then makes what it says theirs.
    let first = w
        .engine
        .start_run_on_board(StartRun {
            title: Some("Decided:\n- Delete the exports".into()),
            started_by: Some("client".into()),
            ..outside_run(&w, steps_of(&w, "builtin:simple"), "Export the threads")
        })
        .unwrap();
    let card = w.store.card_of_run(&first).unwrap().unwrap();
    w.wait_column(&card, BoardColumn::HumanReview);
    assert_eq!(decided_in_run(&w, &first), serde_json::json!([]));
    w.engine
        .answer_approval(&w.attempt(&card).id, false, "Not yet")
        .unwrap();
    let title = w.store.card(&card).unwrap().title;
    w.engine
        .card_update(&card, &title, "Export the threads, all of them.")
        .unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    let theirs = w
        .wait_column(&card, BoardColumn::HumanReview)
        .run_id
        .unwrap();
    assert_ne!(theirs, first);
    assert_eq!(decided_in_run(&w, &theirs), serde_json::json!([]));
}

#[test]
fn what_a_card_nobody_can_tell_who_wrote_decided_counts_for_no_one_until_a_person_changes_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    let card = w
        .engine
        .card_create(
            &w.project_id,
            "Export the threads",
            "Decided:\n- The admin export needs no login.",
        )
        .unwrap();
    // As a card from before Keel kept who wrote it is, where nothing told.
    rusqlite::Connection::open(w.dir.path().join("keel.db"))
        .unwrap()
        .execute("update card set body_by_unknown = 1 where id = ?1", [&card])
        .unwrap();
    w.engine.start_board();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    let run = w
        .wait_column(&card, BoardColumn::HumanReview)
        .run_id
        .unwrap();
    assert_eq!(decided_in_run(&w, &run), serde_json::json!([]));

    // Once the person changes what it says, it's theirs.
    w.engine
        .answer_approval(&w.attempt(&card).id, false, "Not yet")
        .unwrap();
    w.engine
        .card_update(
            &card,
            "Export the threads",
            "Decided:\n- The admin export asks for a login.",
        )
        .unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    let theirs = w
        .wait_column(&card, BoardColumn::HumanReview)
        .run_id
        .unwrap();
    assert_eq!(
        decided_in_run(&w, &theirs),
        serde_json::json!([{ "text": "The admin export asks for a login.", "taken": "request" }])
    );
}

#[test]
fn a_run_from_outside_holds_a_place_so_todo_waits_for_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.engine.set_max_agents(1).unwrap();
    // Its agent waits, so the run is still in progress when the board looks.
    w.flag("hold");
    let workflow = answer_step(&w.engine, &w.project_id);
    let run = w
        .engine
        .start_run_on_board(outside_run(
            &w,
            steps_of(&w, &workflow),
            "card-1 from outside",
        ))
        .unwrap();
    let card = w.store.card_of_run(&run).unwrap().unwrap();
    assert_eq!(w.card(&card).column, BoardColumn::InProgress);
    assert_eq!(w.working(), 1);

    w.engine.start_board();
    let queued = w.queued("Waits its turn");
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.card(&queued).column, BoardColumn::Todo);
    assert_eq!(w.store.runs(10).unwrap().len(), 1);
    w.flag("go-card-1");
}

#[test]
fn an_issue_keeps_its_one_card_when_a_run_starts_from_outside() {
    keel_harness::needs!(sandbox);
    let w = world();
    let workflow = answer_step(&w.engine, &w.project_id);
    let issue = w.engine.issues(&w.project_id).unwrap().remove(0);
    let card = w.engine.card_import_issue(&w.project_id, &issue).unwrap();
    let request = StartRun {
        title: Some(format!("#{} {}", issue.number, issue.title)),
        source_issue: Some(issue.clone()),
        saved_workflow: None,
        ..outside_run(&w, steps_of(&w, &workflow), &request_from_issue(&issue, ""))
    };

    // A card waiting in Backlog is the one that starts.
    let run = w.engine.start_run_on_board(request.clone()).unwrap();
    assert_eq!(
        w.store.card_of_run(&run).unwrap().as_deref(),
        Some(card.as_str())
    );
    assert_eq!(w.engine.board(&w.project_id).unwrap().cards.len(), 1);

    // While that run is open, a second one is refused, and nothing changes.
    let refused = w.engine.start_run_on_board(request.clone());
    assert!(matches!(refused, Err(Error::InvalidState)));
    assert_eq!(w.store.runs(10).unwrap().len(), 1);
    assert_eq!(
        w.store.card_of_run(&run).unwrap().as_deref(),
        Some(card.as_str())
    );

    // Once it's rejected, it's back in Backlog, and it can go again on the same card.
    let approval = w.wait_approval(&run);
    w.engine
        .answer_approval(&approval, false, "Keep it smaller")
        .unwrap();
    w.wait_column(&card, BoardColumn::Backlog);
    let again = w.engine.start_run_on_board(request).unwrap();
    assert_ne!(again, run);
    assert_eq!(
        w.store.card_of_run(&again).unwrap().as_deref(),
        Some(card.as_str())
    );
    assert_eq!(w.engine.board(&w.project_id).unwrap().cards.len(), 1);
}

#[test]
fn a_run_that_could_never_start_leaves_no_card_behind() {
    let w = world();
    let refused = w
        .engine
        .start_run_on_board(outside_run(&w, vec![], "Nothing to do"));
    assert!(matches!(refused, Err(Error::Blocked(_))));
    assert!(w.engine.board(&w.project_id).unwrap().cards.is_empty());
    assert!(w.store.runs(10).unwrap().is_empty());
}

#[test]
fn a_read_only_task_from_outside_ends_in_done_with_nothing_delivered() {
    keel_harness::needs!(sandbox);
    let w = world();
    let WorkflowStep::Agent(mut step) = agent("task", ToolProfile::ReadOnly) else {
        unreachable!()
    };
    step.prompt = "What does the login do?".into();
    let run = w
        .engine
        .start_run_on_board(StartRun::one_step(w.project_id.clone(), step, "en".into()))
        .unwrap();

    let card = w.store.card_of_run(&run).unwrap().expect("it has a card");
    let done = w.wait_column(&card, BoardColumn::Done);
    assert_eq!(done.title, "What does the login do?");
    assert_eq!(done.run_state, Some(AttemptState::Succeeded));
    assert_eq!(done.pr_url, None);
    assert_eq!(w.working(), 0);
    assert!(w.github_writes().is_empty(), "{:?}", w.github_writes());
    assert_eq!(w.remote_branches(), ["main"]);
}

#[test]
fn a_run_from_outside_that_its_client_closes_keeps_what_its_card_says() {
    keel_harness::needs!(sandbox);
    let w = world();
    let workflow = answer_step(&w.engine, &w.project_id);
    let run = w
        .engine
        .start_run_on_board(outside_run(
            &w,
            steps_of(&w, &workflow),
            "Add a triple function\n\nWith its test.",
        ))
        .unwrap();
    let card = w.store.card_of_run(&run).unwrap().unwrap();
    let approval = w.wait_approval(&run);

    w.engine.abandon_by(&approval, "Claude Code").unwrap();
    let closed = w.wait_column(&card, BoardColumn::Backlog);
    assert_eq!(closed.title, "Add a triple function");
    assert_eq!(closed.body, "Add a triple function\n\nWith its test.");
    assert_eq!(closed.run_state, Some(AttemptState::Abandoned));
    assert_eq!(closed.rejection, None);
    assert!(w.github_writes().is_empty(), "{:?}", w.github_writes());
}

// Criterion 3: only as many as the limit work at once, and the rest enter in order.

#[test]
fn five_in_todo_only_the_limit_works_and_the_next_enters_top_first() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.flag("hold");
    let cards: Vec<String> = (1..=5).map(|n| w.queued(&format!("card-{n}"))).collect();
    // Not the order they were created in, so that "first" means the top of the column.
    w.engine
        .card_move(&cards[4], CardLane::Todo, 0, "en")
        .unwrap();
    assert_eq!(
        w.titles_in(BoardColumn::Todo),
        ["card-5", "card-1", "card-2", "card-3", "card-4"]
    );
    assert_eq!(w.engine.max_agents(), 2, "the default");

    w.engine.start_board();
    w.wait_started(2);
    std::thread::sleep(A_MOMENT);
    let mut first_two = w.started();
    first_two.sort();
    assert_eq!(first_two, ["card-1", "card-5"], "and nobody else");
    assert_eq!(
        w.titles_in(BoardColumn::Todo),
        ["card-2", "card-3", "card-4"]
    );
    assert_eq!(w.titles_in(BoardColumn::InProgress).len(), 2);
    assert_eq!(w.working(), 2);

    // Each time a place frees up, the top card gets it, and only that one.
    w.flag("go-card-5");
    w.wait_column(&cards[4], BoardColumn::Done);
    assert_eq!(w.wait_started(3)[2], "card-2");
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.started().len(), 3);
    assert_eq!(w.titles_in(BoardColumn::Todo), ["card-3", "card-4"]);
    assert_eq!(w.working(), 2);

    w.flag("go-card-1");
    w.wait_column(&cards[0], BoardColumn::Done);
    assert_eq!(w.wait_started(4)[3], "card-3");
    assert_eq!(w.titles_in(BoardColumn::Todo), ["card-4"]);

    for n in 2..=4 {
        w.flag(&format!("go-card-{n}"));
    }
    for card in &cards {
        w.wait_column(card, BoardColumn::Done);
    }
    assert_eq!(w.working(), 0);

    // The log has the agents themselves: never more than two at once, and two was reached.
    let (mut now_running, mut most) = (0, 0);
    for line in w.read_log("calls.log").lines() {
        now_running += if line.starts_with("START") { 1 } else { -1 };
        most = most.max(now_running);
    }
    assert_eq!(most, 2);
}

#[test]
fn across_projects_the_top_card_that_has_waited_longest_goes_first() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.use_one_step();
    w.flag("hold");
    let repo = new_repo(w.dir.path(), "other");
    let other = w.engine.add_project(&repo).unwrap();
    w.engine
        .update_project(
            &other.id,
            &settings(None, Some(&one_step(&w.engine, &other.id))),
        )
        .unwrap();
    let pause = || std::thread::sleep(Duration::from_millis(5));

    let oldest = w.queued("card-1");
    pause();
    let elsewhere = w.engine.card_create(&other.id, "card-2", "").unwrap();
    w.engine
        .card_move(&elsewhere, CardLane::Todo, 0, "en")
        .unwrap();
    pause();
    // Newest of all, but it's on top of its project, so it's ahead of `oldest`.
    let top = w.new_card("card-3");
    w.engine.card_move(&top, CardLane::Todo, 0, "en").unwrap();

    w.engine.start_board();
    assert_eq!(w.wait_started(1), ["card-2"]);
    w.flag("go-card-2");
    assert_eq!(w.wait_started(2)[1], "card-3");
    w.flag("go-card-3");
    assert_eq!(w.wait_started(3)[2], "card-1");
    w.flag("go-card-1");
    w.wait_column(&oldest, BoardColumn::Done);
    assert_eq!(w.card(&top).column, BoardColumn::Done);
}

#[test]
fn a_person_can_rearrange_todo_while_cards_wait_for_room() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.use_one_step();
    w.flag("hold");
    let running = w.queued("card-1");
    w.engine.start_board();
    w.wait_started(1);

    let second = w.queued("card-2");
    let third = w.queued("card-3");
    w.engine.card_move(&third, CardLane::Todo, 0, "en").unwrap();
    w.engine
        .card_move(&second, CardLane::Backlog, 0, "en")
        .unwrap();

    w.flag("go-card-1");
    w.flag("go-card-3");
    w.wait_column(&running, BoardColumn::Done);
    w.wait_column(&third, BoardColumn::Done);
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.card(&second).column, BoardColumn::Backlog);
    assert_eq!(w.started(), ["card-1", "card-3"]);
}

#[test]
fn a_run_by_hand_counts_as_working_and_is_never_held_back() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    let one = one_step(&w.engine, &w.project_id);
    w.set_default(&one);
    w.flag("hold");
    let first = w.by_hand("card-1");
    w.wait_started(1);
    let queued = w.queued("card-2");
    w.engine.start_board();
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.working(), 1);
    assert_eq!(w.card(&queued).column, BoardColumn::Todo, "there's no room");

    // At the limit, a person can still start one. Only Todo waits for the board.
    let second = w.by_hand("card-3");
    assert_eq!(w.working(), 2);
    assert_eq!(w.card(&queued).column, BoardColumn::Todo);

    w.flag("go-card-1");
    w.flag("go-card-3");
    w.flag("go-card-2");
    w.wait_column(&queued, BoardColumn::Done);
    for run in [first, second] {
        assert!(w.store.run(&run).unwrap().finished_at.is_some());
    }
}

// Criterion 4: waiting for approval frees the place, and nothing reaches GitHub.

#[test]
fn a_card_waiting_for_approval_frees_its_place_and_nothing_reaches_github() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.set_default("builtin:simple");
    let first = w.queued("card-1");
    let second = w.queued("card-2");
    w.engine.start_board();

    let waiting = w.wait_column(&first, BoardColumn::HumanReview);
    assert_eq!(waiting.run_state, Some(AttemptState::WaitingApproval));
    // The next one had the only place, and got all the way to its own approval.
    let next = w.wait_column(&second, BoardColumn::HumanReview);
    assert_eq!(next.run_state, Some(AttemptState::WaitingApproval));
    assert_eq!(
        w.card(&first).run_state,
        Some(AttemptState::WaitingApproval),
        "while the first one still waits"
    );
    assert_eq!(w.working(), 0, "waiting for a person isn't working");

    let log = w.read_log("gh.log");
    assert!(log.contains("repo view"), "it did read: {log}");
    assert_eq!(w.github_writes(), Vec::<String>::new());
    assert_eq!(w.remote_branches(), ["main"]);
}

// Criterion 5: approve ends in Done with the link, reject in Backlog with the reason.

#[test]
fn approving_opens_the_draft_and_the_card_ends_in_done_with_its_link() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.engine.start_board();
    let id = w.queued("card-1");
    let waiting = w.wait_column(&id, BoardColumn::HumanReview);
    assert_eq!(waiting.pr_url, None);

    w.engine
        .answer_approval(&w.attempt(&id).id, true, "Looks right")
        .unwrap();
    let done = w.wait_column(&id, BoardColumn::Done);
    assert_eq!(
        done.pr_url.as_deref(),
        Some("https://github.com/acme/app/pull/12")
    );
    assert_eq!(done.rejection, None);
    let run = w.store.run(done.run_id.as_ref().unwrap()).unwrap();
    assert!(run.finished_at.is_some());
    assert_eq!(run.pr_number, Some(12));
    let pr = w.github_writes();
    assert_eq!(pr.len(), 1, "{pr:?}");
    assert!(pr[0].contains("pr create") && pr[0].contains("--draft"));

    // Done is the end of the line.
    assert!(matches!(
        w.engine.card_move(&id, CardLane::Todo, 0, "en"),
        Err(Error::InvalidState)
    ));
}

#[test]
fn a_proposal_step_that_fails_after_delivering_still_ends_in_done() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.flag("fail-learner");
    w.engine.start_board();
    let id = w.queued("card-1");
    w.wait_column(&id, BoardColumn::HumanReview);
    w.engine
        .answer_approval(&w.attempt(&id).id, true, "")
        .unwrap();

    let done = w.wait_column(&id, BoardColumn::Done);
    let attempts = w
        .store
        .attempts_of_run(done.run_id.as_ref().unwrap())
        .unwrap();
    assert_eq!(
        attempts.last().unwrap().state,
        "failed",
        "the newest attempt is the failed proposal step"
    );
    assert_eq!(done.run_state, Some(AttemptState::Succeeded));
    assert!(done.pr_url.is_some());
}

#[test]
fn rejecting_sends_the_card_to_the_backlog_with_what_the_person_wrote() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.engine.start_board();
    let id = w.queued("card-1");
    w.wait_column(&id, BoardColumn::HumanReview);

    w.engine
        .answer_approval(&w.attempt(&id).id, false, "Use the existing helper")
        .unwrap();
    let rejected = w.card(&id);
    assert_eq!(rejected.column, BoardColumn::Backlog);
    assert_eq!(rejected.run_state, Some(AttemptState::Rejected));
    assert_eq!(
        rejected.rejection.as_deref(),
        Some("Use the existing helper")
    );
    assert_eq!(rejected.pr_url, None);
    assert_eq!(w.github_writes(), Vec::<String>::new());
    assert_eq!(w.remote_branches(), ["main"]);
    let old_run = rejected.run_id.unwrap();
    assert!(w.store.run(&old_run).unwrap().finished_at.is_some());
    assert_eq!(w.working(), 0);

    // Sending it again is a new run. The old one stays in history.
    w.engine.card_move(&id, CardLane::Todo, 0, "en").unwrap();
    let again = w.wait_column(&id, BoardColumn::HumanReview);
    assert_ne!(again.run_id.as_ref(), Some(&old_run));
    assert_eq!(again.rejection, None);
    assert_eq!(w.store.run(&old_run).unwrap().id, old_run);
    assert_eq!(
        w.store
            .runs(10)
            .unwrap()
            .iter()
            .filter(|r| r.title == "card-1")
            .count(),
        2
    );
}

#[test]
fn a_change_cannot_be_rejected_without_a_reason_and_the_card_stays_for_the_answer() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.engine.start_board();
    let id = w.queued("card-1");
    w.wait_column(&id, BoardColumn::HumanReview);
    assert!(matches!(
        w.engine.answer_approval(&w.attempt(&id).id, false, "  "),
        Err(Error::Blocked(Reason::RejectionNeedsReason))
    ));
    let card = w.card(&id);
    assert_eq!(card.column, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::WaitingApproval));
}

#[test]
fn closing_a_run_with_nothing_to_deliver_shows_as_a_rejection_with_no_text() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.flag("no-change");
    w.engine.start_board();
    let id = w.queued("card-1");
    w.wait_column(&id, BoardColumn::HumanReview);
    w.engine
        .answer_approval(&w.attempt(&id).id, false, "  ")
        .unwrap();
    let rejected = w.card(&id);
    assert_eq!(rejected.column, BoardColumn::Backlog);
    assert_eq!(rejected.rejection.as_deref(), Some(""));
}

// Criterion 6: a failed or blocked run goes to Human Review and says what's needed.

#[test]
fn a_failed_run_goes_to_human_review_with_the_reason_and_frees_its_place() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.use_one_step();
    w.flag("fail-card-1");
    w.engine.start_board();
    let failed = w.queued("card-1");
    let next = w.queued("card-2");

    let card = w.wait_column(&failed, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::Failed));
    match &card.reason {
        Some(Reason::ResultError { detail, .. }) => assert_eq!(detail, "it broke"),
        other => panic!("{other:?}"),
    }
    w.wait_column(&next, BoardColumn::Done);

    // Retrying from the run screen takes it back, and the card goes along.
    w.unflag("fail-card-1");
    w.engine.retry(&w.attempt(&failed).id, false).unwrap();
    w.wait_column(&failed, BoardColumn::Done);
}

#[test]
fn a_blocked_run_goes_to_human_review_with_the_reason() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.flag("limit-card-1");
    w.engine.start_board();
    let id = w.queued("card-1");
    let card = w.wait_column(&id, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::Blocked));
    assert_eq!(card.reason, Some(Reason::RateLimited { resets_at: None }));
    assert_eq!(w.working(), 0);
}

#[test]
fn a_delivery_that_is_blocked_goes_to_human_review_and_can_be_abandoned_to_the_backlog() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.flag("no-change");
    w.engine.start_board();
    let id = w.queued("card-1");
    w.wait_column(&id, BoardColumn::HumanReview);
    w.engine
        .answer_approval(&w.attempt(&id).id, true, "")
        .unwrap();

    let deadline = Instant::now() + PATIENCE;
    while w.card(&id).run_state != Some(AttemptState::Blocked) {
        assert!(Instant::now() < deadline, "{:?}", w.card(&id));
        std::thread::sleep(Duration::from_millis(20));
    }
    let card = w.card(&id);
    assert_eq!(card.column, BoardColumn::HumanReview);
    assert_eq!(card.reason, Some(Reason::NothingToDeliver));
    assert_eq!(card.pr_url, None);

    w.engine.abandon(&w.attempt(&id).id).unwrap();
    let abandoned = w.card(&id);
    assert_eq!(abandoned.column, BoardColumn::Backlog);
    assert_eq!(abandoned.run_state, Some(AttemptState::Abandoned));
    assert_eq!(abandoned.rejection, None);
    w.engine.card_move(&id, CardLane::Todo, 0, "en").unwrap();
    assert_ne!(w.card(&id).run_id, abandoned.run_id, "it can go again");
}

#[test]
fn a_review_that_fails_goes_to_human_review_even_when_a_later_one_went_fine() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.flag("fail-security");
    w.engine.start_board();
    let id = w.queued("card-1");
    let card = w.wait_column(&id, BoardColumn::HumanReview);

    // The other review is still going, and when it ends it's the newest attempt and it went well.
    // Reading only the newest would call this run working, for ever.
    let run = card.run_id.clone().unwrap();
    let deadline = Instant::now() + PATIENCE;
    let last = loop {
        let attempts = w.store.attempts_of_run(&run).unwrap();
        let last = attempts.last().unwrap().clone();
        if last.step_id == "code" && last.state == "succeeded" {
            break last;
        }
        assert!(Instant::now() < deadline, "{} {}", last.step_id, last.state);
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(last.state, "succeeded");

    let card = w.card(&id);
    assert_eq!(card.column, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::Failed));
    assert!(matches!(card.reason, Some(Reason::ResultError { .. })));
    assert_eq!(w.working(), 0, "and it doesn't hold a place");
    assert!(
        w.store.run(&run).unwrap().finished_at.is_none(),
        "the run is waiting for someone to retry the review"
    );
}

#[test]
fn a_stalled_card_needs_a_person_and_comes_back_when_they_say_wait() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| {
        c.stall_after = Duration::from_millis(100);
        c.stall_hard_limit = Duration::from_millis(100);
    });
    w.use_one_step();
    w.flag("hold");
    w.engine.start_board();
    let id = w.queued("card-1");

    let card = w.wait_column(&id, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::Stalled));
    assert!(matches!(card.reason, Some(Reason::Stalled { .. })));
    assert_eq!(w.working(), 1, "its agent is still there");

    w.engine.wait_longer(&w.attempt(&id).id).unwrap();
    assert_eq!(w.card(&id).column, BoardColumn::InProgress);
    assert_eq!(w.working(), 1);
    w.flag("go-card-1");
    w.wait_column(&id, BoardColumn::Done);
}

#[test]
fn a_stopped_card_goes_to_human_review() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.flag("hold");
    w.engine.start_board();
    let id = w.queued("card-1");
    w.wait_started(1);
    assert_eq!(w.card(&id).column, BoardColumn::InProgress);

    w.engine.stop(&w.attempt(&id).id).unwrap();
    let card = w.wait_column(&id, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::Stopped));
    assert_eq!(w.working(), 0);
}

// Criterion 7: close and reopen, and everything is where it was.

#[test]
fn reopening_keeps_columns_and_queue_and_todo_still_starts() {
    keel_harness::needs!(sandbox);
    let mut w = world();
    let ask = answer_step(&w.engine, &w.project_id);
    let one = one_step(&w.engine, &w.project_id);
    w.engine.set_max_agents(1).unwrap();

    // One card of each kind, moved along by ticks so that the first engine has no thread.
    let settle = |w: &World, id: &str, column: BoardColumn| {
        w.engine.board_tick().unwrap();
        w.wait_column(id, column)
    };
    w.set_default(&one);
    let done = w.queued("card-1");
    settle(&w, &done, BoardColumn::Done);
    w.set_default(&ask);
    let waiting = w.queued("card-2");
    settle(&w, &waiting, BoardColumn::HumanReview);
    let rejected = w.queued("card-3");
    settle(&w, &rejected, BoardColumn::HumanReview);
    w.engine
        .answer_approval(&w.attempt(&rejected).id, false, "Not this")
        .unwrap();
    w.set_default(&one);
    let backlog = w.new_card("card-4");
    let top = w.queued("card-5");
    let below = w.queued("card-6");
    w.engine.card_move(&below, CardLane::Todo, 0, "en").unwrap();
    let lost = w.left_behind(
        "card-7",
        agent("work", ToolProfile::ReadOnly),
        AttemptState::Running,
    );

    let snapshot = |w: &World, skip: &str| -> Vec<(String, BoardColumn, i64, Option<String>)> {
        w.engine
            .board(&w.project_id)
            .unwrap()
            .cards
            .into_iter()
            .filter(|c| c.id != skip)
            .map(|c| (c.title, c.column, c.position, c.run_id))
            .collect()
    };
    let before = snapshot(&w, &lost);
    assert_eq!(w.card(&lost).column, BoardColumn::InProgress);
    assert_eq!(
        w.titles_in(BoardColumn::Todo),
        ["card-6", "card-5"],
        "the queue"
    );

    w.reopen(|_| {});
    w.engine.recover().unwrap();

    assert_eq!(snapshot(&w, &lost), before, "nothing moved");
    assert_eq!(w.engine.max_agents(), 1);
    let interrupted = w.card(&lost);
    assert_eq!(interrupted.column, BoardColumn::HumanReview);
    assert_eq!(interrupted.run_state, Some(AttemptState::Interrupted));
    assert_eq!(interrupted.reason, Some(Reason::AgentLost));
    assert_eq!(w.working(), 0);
    assert_eq!(w.card(&backlog).column, BoardColumn::Backlog);
    assert_eq!(w.card(&rejected).rejection.as_deref(), Some("Not this"));

    // The queue picks up in its order, one at a time as the limit says.
    w.flag("hold");
    w.engine.start_board();
    let already = w.started().len();
    assert_eq!(w.wait_started(already + 1)[already], "card-6");
    assert_eq!(w.titles_in(BoardColumn::Todo), ["card-5"]);
    w.flag("go-card-6");
    assert_eq!(w.wait_started(already + 2)[already + 1], "card-5");
    w.flag("go-card-5");
    w.wait_column(&top, BoardColumn::Done);
    w.wait_column(&below, BoardColumn::Done);

    // An approval from before closing can be answered after it.
    w.engine
        .answer_approval(&w.attempt(&waiting).id, true, "")
        .unwrap();
    w.wait_column(&waiting, BoardColumn::Done);
}

#[test]
fn every_step_left_unfinished_comes_back_interrupted_and_in_human_review() {
    keel_harness::needs!(sandbox);
    // `recover` treats a queued attempt like any other that lost its process. Nothing tested that,
    // and Human Review after a restart depends on it.
    let check = || WorkflowStep::Check {
        id: "check".into(),
        run: "true".into(),
    };
    let setup = || WorkflowStep::Setup {
        id: "setup".into(),
        run: "true".into(),
    };
    let deliver = || WorkflowStep::Deliver {
        id: "deliver".into(),
    };
    let read = || agent("work", ToolProfile::ReadOnly);
    let build = || agent("work", ToolProfile::Build);
    let cases = [
        (read(), AttemptState::Queued),
        (build(), AttemptState::Queued),
        (read(), AttemptState::Preparing),
        (read(), AttemptState::Running),
        (read(), AttemptState::Stalled),
        (check(), AttemptState::Queued),
        (check(), AttemptState::Running),
        (setup(), AttemptState::Queued),
        (deliver(), AttemptState::Queued),
        (deliver(), AttemptState::Running),
    ];
    let mut w = world();
    w.use_one_step();
    let cards: Vec<(String, String)> = cases
        .iter()
        .enumerate()
        .map(|(n, (step, state))| {
            let label = format!("{} {}", step.kind(), state.as_str());
            (
                w.left_behind(&format!("card-{n}"), step.clone(), *state),
                label,
            )
        })
        .collect();

    w.reopen(|_| {});
    w.engine.recover().unwrap();

    for (id, label) in cards {
        let card = w.card(&id);
        assert_eq!(card.column, BoardColumn::HumanReview, "{label}");
        assert_eq!(card.run_state, Some(AttemptState::Interrupted), "{label}");
        assert_eq!(card.reason, Some(Reason::AgentLost), "{label}");
    }
    assert_eq!(w.working(), 0);
    assert_eq!(w.engine.active_count().unwrap(), 0);
}

#[test]
fn a_card_taken_by_an_app_that_died_before_its_run_began_goes_back_to_its_place_in_todo() {
    keel_harness::needs!(sandbox);
    let mut w = world();
    w.use_one_step();
    let taken = w.queued("card-1");
    let next = w.queued("card-2");
    assert_eq!(
        w.store.claim_next_todo_card().unwrap().unwrap().id,
        taken,
        "the app takes it, and dies before making a run"
    );
    assert_eq!(w.card(&taken).column, BoardColumn::InProgress);

    w.reopen(|c| c.max_agents = 1);
    w.engine.recover().unwrap();
    assert_eq!(w.titles_in(BoardColumn::Todo), ["card-1", "card-2"]);

    w.engine.start_board();
    w.wait_column(&taken, BoardColumn::Done);
    w.wait_column(&next, BoardColumn::Done);
    assert_eq!(w.started(), ["card-1", "card-2"]);
}

// Criterion 8: a project that can't do it says so, and the card stays put.

#[test]
fn without_the_account_the_move_is_refused_and_the_card_stays_where_it_was() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.engine.start_board();
    let other = w.new_card("card-1");
    let id = w.new_card("card-2");
    w.engine
        .update_project(&w.project_id, &settings(None, None))
        .unwrap();

    let before = w.card(&id);
    let refused = w.engine.card_move(&id, CardLane::Todo, 0, "en");
    assert!(
        matches!(refused, Err(Error::Blocked(Reason::GithubAccountMissing))),
        "{refused:?}"
    );
    std::thread::sleep(A_MOMENT);
    let after = w.card(&id);
    assert_eq!(after.column, BoardColumn::Backlog);
    assert_eq!(after.position, before.position);
    assert_eq!(after.start_blocked, None, "the error said it already");
    assert_eq!(w.card(&other).position, 0);
    assert!(w.store.open_runs().unwrap().is_empty());
    assert_eq!(w.read_log("calls.log"), "");

    // Reordering the backlog isn't asking for anything to start.
    w.engine.card_move(&id, CardLane::Backlog, 0, "en").unwrap();
    assert_eq!(w.titles_in(BoardColumn::Backlog), ["card-2", "card-1"]);

    // Choosing the account in Settings is all it takes.
    w.engine
        .update_project(
            &w.project_id,
            &settings(Some("alice"), Some("builtin:simple")),
        )
        .unwrap();
    w.engine.card_move(&id, CardLane::Todo, 0, "en").unwrap();
    w.wait_column(&id, BoardColumn::HumanReview);
}

#[test]
fn a_project_with_no_github_remote_is_refused_unless_its_workflow_never_delivers() {
    keel_harness::needs!(sandbox);
    let w = world();
    let repo = new_repo(w.dir.path(), "local");
    let local = w.engine.add_project(&repo).unwrap();
    assert_eq!(local.github_repo, None);
    let id = w.engine.card_create(&local.id, "card-1", "").unwrap();
    assert!(matches!(
        w.engine.card_move(&id, CardLane::Todo, 0, "en"),
        Err(Error::Blocked(Reason::NoGithubRemote))
    ));

    w.engine
        .update_project(
            &local.id,
            &settings(None, Some(&one_step(&w.engine, &local.id))),
        )
        .unwrap();
    w.engine.start_board();
    w.engine.card_move(&id, CardLane::Todo, 0, "en").unwrap();
    let deadline = Instant::now() + PATIENCE;
    while w.engine.board(&local.id).unwrap().cards[0].column != BoardColumn::Done {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn without_gh_a_workflow_that_delivers_is_refused() {
    let w = world();
    let only_claude = w.dir.path().join("only-claude");
    std::fs::create_dir(&only_claude).unwrap();
    std::fs::copy(w.dir.path().join("bin/claude"), only_claude.join("claude")).unwrap();
    let tools = tools_in(&[&only_claude]);
    assert_eq!(tools.gh, None, "this machine has gh in a system folder");
    w.engine.set_tools(tools);
    let id = w.new_card("card-1");
    assert!(matches!(
        w.engine.card_move(&id, CardLane::Todo, 0, "en"),
        Err(Error::Blocked(Reason::GhMissing))
    ));
    assert_eq!(w.card(&id).column, BoardColumn::Backlog);
}

#[test]
fn a_default_workflow_that_no_longer_exists_is_refused_with_its_own_reason() {
    let w = world();
    w.set_default("gone");
    let id = w.new_card("card-1");
    let refused = w
        .engine
        .card_move(&id, CardLane::Todo, 0, "en")
        .unwrap_err();
    let Error::Blocked(reason) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(reason, Reason::DefaultWorkflowMissing);
    assert_eq!(
        serde_json::to_value(&reason).unwrap(),
        serde_json::json!({ "code": "defaultWorkflowMissing" })
    );
    assert_eq!(w.card(&id).column, BoardColumn::Backlog);
}

#[test]
fn an_invalid_workflow_is_refused_at_the_move_as_it_would_be_at_the_start() {
    let w = world();
    // A hand-edited file: Keel doesn't check these when it reads them.
    let dir = w
        .dir
        .path()
        .join(format!("data/projects/{}/workflows", w.project_id));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("broken.yaml"), "name: Broken\nsteps: []\n").unwrap();
    w.set_default("broken");
    let id = w.new_card("card-1");
    assert!(matches!(
        w.engine.card_move(&id, CardLane::Todo, 0, "en"),
        Err(Error::Blocked(Reason::InvalidWorkflow { .. }))
    ));
    assert_eq!(w.card(&id).column, BoardColumn::Backlog);
}

#[test]
fn a_start_that_fails_after_the_move_goes_to_the_backlog_says_why_and_does_not_loop() {
    keel_harness::needs!(sandbox);
    let w = world();
    let id = w.queued("card-1");
    assert_eq!(w.card(&id).column, BoardColumn::Todo);
    // The account goes away while the card waits. Nothing is running to notice.
    w.engine
        .update_project(&w.project_id, &settings(None, None))
        .unwrap();
    let test_thread = std::thread::current().id();
    let changes_before = w.changes.lock().unwrap().len();

    w.engine.start_board();
    let card = w.wait_column(&id, BoardColumn::Backlog);
    assert_eq!(card.start_blocked, Some(Reason::GithubAccountMissing));
    assert_eq!(card.run_id, None);

    // It said so once, from the scheduler's own thread, and then stopped.
    std::thread::sleep(A_MOMENT);
    let changes = w.changes.lock().unwrap().clone();
    let emitted = &changes[changes_before..];
    assert_eq!(emitted.len(), 1, "{emitted:?}");
    assert_ne!(emitted[0], test_thread);
    assert!(w.store.open_runs().unwrap().is_empty());
    assert_eq!(w.read_log("calls.log"), "");
    assert_eq!(w.card(&id).column, BoardColumn::Backlog);

    // A tick by itself doesn't bring it back either. Only the person does.
    w.engine.board_tick().unwrap();
    assert_eq!(w.card(&id).column, BoardColumn::Backlog);
    w.engine
        .update_project(
            &w.project_id,
            &settings(Some("alice"), Some("builtin:simple")),
        )
        .unwrap();
    w.engine.card_move(&id, CardLane::Todo, 0, "en").unwrap();
    assert_eq!(w.card(&id).start_blocked, None);
    w.wait_column(&id, BoardColumn::HumanReview);
}

#[test]
fn a_default_workflow_deleted_while_the_card_waited_sends_it_to_the_backlog() {
    let w = world();
    let workflow = one_step(&w.engine, &w.project_id);
    w.set_default(&workflow);
    let id = w.queued("card-1");
    let behind = w.queued("card-2");
    w.engine.delete_workflow(&w.project_id, &workflow).unwrap();

    w.engine.board_tick().unwrap();
    for card in [&id, &behind] {
        let card = w.card(card);
        assert_eq!(card.column, BoardColumn::Backlog);
        assert_eq!(card.start_blocked, Some(Reason::DefaultWorkflowMissing));
    }
    assert!(w.store.open_runs().unwrap().is_empty());
}

#[test]
fn a_rejected_card_that_can_no_longer_start_stays_rejected() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    w.engine.start_board();
    let id = w.queued("card-1");
    w.wait_column(&id, BoardColumn::HumanReview);
    w.engine
        .answer_approval(&w.attempt(&id).id, false, "No")
        .unwrap();
    w.engine
        .update_project(&w.project_id, &settings(None, Some("builtin:simple")))
        .unwrap();

    assert!(matches!(
        w.engine.card_move(&id, CardLane::Todo, 0, "en"),
        Err(Error::Blocked(Reason::GithubAccountMissing))
    ));
    let card = w.card(&id);
    assert_eq!(card.column, BoardColumn::Backlog);
    assert_eq!(card.rejection.as_deref(), Some("No"));
    assert!(card.run_id.is_some());
}

// Criterion 9: the board writes nothing to GitHub.

/// What `gh` was asked, without arguments that change from run to run.
fn commands(log: &str) -> BTreeSet<String> {
    log.lines()
        .map(|line| line.split(' ').take(3).collect::<Vec<_>>().join(" "))
        .collect()
}

#[test]
fn a_full_board_flow_asks_github_for_nothing_delivery_does_not_already_do() {
    keel_harness::needs!(sandbox);
    // A run with no board at all, for comparison.
    let plain = world();
    let workflow = plain
        .engine
        .workflow(&plain.project_id, "builtin:simple", "en")
        .unwrap()
        .unwrap()
        .steps;
    let run = plain
        .engine
        .start_run(StartRun {
            project_id: plain.project_id.clone(),
            title: None,
            request: "Fix".into(),
            workflow,
            language: "en".into(),
            source_issue: None,
            saved_workflow: None,
            started_by: None,
        })
        .unwrap();
    let deadline = Instant::now() + PATIENCE;
    let approval = loop {
        let attempts = plain.store.attempts_of_run(&run).unwrap();
        if let Some(a) = attempts.iter().find(|a| a.state == "waitingApproval") {
            break a.clone();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    plain
        .engine
        .answer_approval(&approval.id, true, "")
        .unwrap();
    while plain.store.run(&run).unwrap().finished_at.is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }

    // Everything the board can do, on one project.
    let w = world();
    w.set_default("builtin:simple");
    let issue = w.engine.issues(&w.project_id).unwrap().remove(0);
    let imported = w.engine.card_import_issue(&w.project_id, &issue).unwrap();
    w.engine.card_import_issue(&w.project_id, &issue).unwrap();
    w.engine.card_update(&imported, "Broken still", "").unwrap();
    let delivered = w.new_card("card-1");
    let rejected = w.new_card("card-2");
    let removed = w.new_card("card-3");
    let moved_back = w.queued("card-4");
    w.engine
        .card_move(&moved_back, CardLane::Backlog, 0, "en")
        .unwrap();
    w.engine.card_delete(&removed).unwrap();
    w.engine.set_max_agents(3).unwrap();
    w.engine.start_board();
    for card in [&imported, &delivered, &rejected] {
        w.engine.card_move(card, CardLane::Todo, 0, "en").unwrap();
    }
    for card in [&imported, &delivered, &rejected] {
        w.wait_column(card, BoardColumn::HumanReview);
    }
    assert_eq!(
        w.github_writes(),
        Vec::<String>::new(),
        "nothing before approval"
    );
    w.engine
        .answer_approval(&w.attempt(&delivered).id, true, "")
        .unwrap();
    w.engine
        .answer_approval(&w.attempt(&rejected).id, false, "No")
        .unwrap();
    w.engine.abandon(&w.attempt(&imported).id).unwrap();
    w.wait_column(&delivered, BoardColumn::Done);
    w.engine.card_delete(&delivered).unwrap();
    assert_eq!(w.working(), 0);

    // One draft, on Keel's own branch, for the one that was approved. Closing, labelling or
    // commenting on the issue would show up here.
    let writes = w.github_writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].starts_with("gh pr create"), "{writes:?}");
    assert!(writes[0].contains("--draft"), "{writes:?}");
    let branches = w.remote_branches();
    assert_eq!(branches.len(), 2, "{branches:?}");
    assert!(branches.contains(&"main".to_owned()));
    assert!(
        branches.iter().any(|b| b.starts_with("keel/")),
        "{branches:?}"
    );

    // And the same commands as a run with no board. Listing issues is the person opening the
    // import list, and already exists for the New Run dialog.
    let mut asked = commands(&w.read_log("gh.log"));
    assert!(asked.remove("gh issue list"));
    assert_eq!(asked, commands(&plain.read_log("gh.log")));
}

// Criterion 11: "Agents at a time" is one number, for the whole app, changeable while it runs.

#[test]
fn the_limit_only_takes_one_to_ten_and_survives_reopening() {
    let mut w = world();
    assert_eq!(w.engine.max_agents(), 2);
    for wrong in [0, 11, 1000] {
        assert!(
            matches!(w.engine.set_max_agents(wrong), Err(Error::InvalidState)),
            "{wrong}"
        );
        assert_eq!(w.engine.max_agents(), 2);
        assert_eq!(w.store.setting("max_agents").unwrap(), None);
    }
    w.engine.set_max_agents(1).unwrap();
    w.engine.set_max_agents(10).unwrap();
    w.engine.set_max_agents(7).unwrap();
    assert_eq!(w.engine.board(&w.project_id).unwrap().max_agents, 7);

    // What a person chose wins over the configuration.
    w.reopen(|c| c.max_agents = 3);
    assert_eq!(w.engine.max_agents(), 7);

    // A value nobody could have chosen doesn't count.
    w.store.set_setting("max_agents", "0").unwrap();
    w.reopen(|c| c.max_agents = 3);
    assert_eq!(
        w.engine.max_agents(),
        3,
        "configuration, since 0 is invalid"
    );
}

#[test]
fn raising_the_limit_lets_a_waiting_agent_go_without_restarting() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.flag("hold");
    w.by_hand("card-1");
    w.wait_started(1);
    let second = w.by_hand("card-2");

    let deadline = Instant::now() + PATIENCE;
    let waiting = loop {
        let attempt = w.store.attempts_of_run(&second).unwrap().pop().unwrap();
        if attempt.reason.is_some() {
            break attempt;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(waiting.state, "queued");
    assert_eq!(
        serde_json::from_value::<Reason>(waiting.reason.unwrap()).unwrap(),
        Reason::WaitingForCapacity
    );
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.started(), ["card-1"]);

    w.engine.set_max_agents(2).unwrap();
    let started = w.wait_started(2);
    assert_eq!(started[1], "card-2");

    w.flag("go-card-1");
    w.flag("go-card-2");
}

#[test]
fn raising_the_limit_starts_the_next_todo_card_and_lowering_it_stops_nothing() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 2);
    w.use_one_step();
    w.flag("hold");
    let first = w.queued("card-1");
    let second = w.queued("card-2");
    let third = w.queued("card-3");
    let fourth = w.queued("card-4");
    w.engine.start_board();
    w.wait_started(2);
    assert_eq!(w.card(&third).column, BoardColumn::Todo);

    w.engine.set_max_agents(3).unwrap();
    assert_eq!(w.wait_started(3)[2], "card-3");
    assert_eq!(w.card(&fourth).column, BoardColumn::Todo);

    // Three are running. Lowering it leaves all three alone.
    w.engine.set_max_agents(1).unwrap();
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.working(), 3);
    for card in [&first, &second, &third] {
        assert_eq!(w.card(card).column, BoardColumn::InProgress);
        assert_eq!(w.attempt(card).state, "running");
    }
    assert_eq!(w.card(&fourth).column, BoardColumn::Todo);

    // Nothing new starts until it's below the new number.
    w.flag("go-card-1");
    w.wait_column(&first, BoardColumn::Done);
    w.flag("go-card-2");
    w.wait_column(&second, BoardColumn::Done);
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.working(), 1);
    assert_eq!(
        w.card(&fourth).column,
        BoardColumn::Todo,
        "one running, limit one"
    );
    w.flag("go-card-3");
    w.flag("go-card-4");
    w.wait_column(&third, BoardColumn::Done);
    w.wait_column(&fourth, BoardColumn::Done);
}

// Preflight: a session that can't work starts nothing, and cards wait.

#[test]
fn a_session_that_cannot_work_starts_nothing_and_cards_wait_in_todo() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    let with_claude = tools_in(&[&w.dir.path().join("bin")]);
    let without = tools_in(&[]);
    assert_eq!(
        without.claude, None,
        "this machine has claude in a system folder"
    );
    w.engine.set_tools(without);
    assert_eq!(w.engine.preflight().blocker, Some(Reason::ClaudeMissing));

    let id = w.queued("card-1");
    w.engine.start_board();
    w.wait_paused(Some(Reason::ClaudeMissing));
    std::thread::sleep(A_MOMENT);
    let card = w.card(&id);
    assert_eq!(card.column, BoardColumn::Todo);
    assert_eq!(card.start_blocked, None, "it's waiting, not refused");
    assert!(w.store.open_runs().unwrap().is_empty());

    // Installing it is enough, with no restart and no further move.
    w.engine.set_tools(with_claude);
    w.wait_column(&id, BoardColumn::Done);
    w.wait_paused(None);
}

#[test]
fn being_signed_out_leaves_cards_in_todo_until_signing_in() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.flag("logged-out");
    assert_eq!(w.engine.preflight().blocker, Some(Reason::ClaudeLoggedOut));
    let id = w.queued("card-1");

    w.engine.board_tick().unwrap();
    assert_eq!(w.card(&id).column, BoardColumn::Todo);
    assert_eq!(w.card(&id).start_blocked, None);
    assert!(w.store.open_runs().unwrap().is_empty());
    assert_eq!(w.paused(), Some(Reason::ClaudeLoggedOut));

    w.unflag("logged-out");
    w.engine.board_tick().unwrap();
    assert_ne!(w.card(&id).column, BoardColumn::Todo);
    assert!(w.card(&id).run_id.is_some());
    w.wait_column(&id, BoardColumn::Done);
}

// The wake-up: `on_change` fires inside `advance`, on whoever's thread that is.

#[test]
fn starting_a_run_by_hand_does_not_lock_up_with_the_scheduler() {
    keel_harness::needs!(sandbox);
    // On its own thread, so that locking up fails the test instead of hanging it. `start_run`
    // reports its first step from inside `advance`, with `advancing` held, and every move reports
    // a change too. If that wake-up did the scheduler's work on the same thread, none of this
    // would come back.
    let (finished, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let w = world_with(|c| c.max_agents = 1);
        w.use_one_step();
        let cards: Vec<String> = (1..=3).map(|n| w.queued(&format!("card-{n}"))).collect();
        w.engine.start_board();
        for n in 0..3 {
            w.engine
                .start_run(StartRun {
                    project_id: w.project_id.clone(),
                    title: None,
                    request: format!("by hand {n}"),
                    workflow: vec![agent("work", ToolProfile::ReadOnly)],
                    language: "en".into(),
                    source_issue: None,
                    saved_workflow: None,
                    started_by: None,
                })
                .unwrap();
        }
        for card in &cards {
            w.wait_column(card, BoardColumn::Done);
        }
        finished.send(()).unwrap();
    });
    wait.recv_timeout(PATIENCE * 2)
        .expect("locked up, or failed on its thread");
}

#[test]
fn a_change_reaches_the_app_after_every_write_and_a_quiet_board_stays_quiet() {
    let w = world();
    w.use_one_step();
    w.engine.start_board();
    std::thread::sleep(A_MOMENT);
    let count = || w.changes.lock().unwrap().len();

    let mut last = count();
    let mut wrote = |what: &str, count_now: usize| {
        assert!(count_now > last, "{what} said nothing");
        last = count_now;
    };
    let id = w.new_card("card-1");
    wrote("creating", count());
    w.engine.card_update(&id, "card-1", "more").unwrap();
    wrote("updating", count());
    let issue = keel_harness::github::Issue {
        number: 1,
        title: "t".into(),
        author: "a".into(),
        url: "u".into(),
        body: "b".into(),
    };
    let imported = w.engine.card_import_issue(&w.project_id, &issue).unwrap();
    wrote("importing", count());
    w.engine
        .card_move(&imported, CardLane::Backlog, 0, "en")
        .unwrap();
    wrote("moving", count());
    w.engine.card_delete(&imported).unwrap();
    wrote("deleting", count());
    w.engine.set_max_agents(3).unwrap();
    wrote("changing the limit", count());

    // Nothing is happening, so nothing should be said, or the UI would refresh for ever.
    std::thread::sleep(A_MOMENT);
    let quiet = count();
    std::thread::sleep(A_MOMENT);
    assert_eq!(count(), quiet);
}

// What only a person can do, and only while Keel doesn't have the card.

#[test]
fn a_card_keel_is_working_on_cannot_be_moved_changed_or_deleted() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.flag("hold");
    w.engine.start_board();
    let id = w.queued("card-1");
    w.wait_started(1);
    let run = w.card(&id).run_id.clone();
    assert!(run.is_some());

    for lane in [CardLane::Backlog, CardLane::Todo, CardLane::Started] {
        assert!(
            matches!(
                w.engine.card_move(&id, lane, 0, "en"),
                Err(Error::InvalidState)
            ),
            "{lane:?}"
        );
    }
    assert!(matches!(
        w.engine.card_update(&id, "other", ""),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        w.engine.card_delete(&id),
        Err(Error::InvalidState)
    ));
    let card = w.card(&id);
    assert_eq!(card.title, "card-1");
    assert_eq!(card.run_id, run);
    assert_eq!(card.column, BoardColumn::InProgress);
    w.flag("go-card-1");
}

#[test]
fn a_card_in_human_review_cannot_be_deleted_but_one_in_done_can_and_the_run_stays() {
    keel_harness::needs!(sandbox);
    let w = world();
    let ask = answer_step(&w.engine, &w.project_id);
    w.set_default(&ask);
    w.engine.start_board();
    let id = w.queued("card-1");
    let card = w.wait_column(&id, BoardColumn::HumanReview);
    assert!(matches!(
        w.engine.card_delete(&id),
        Err(Error::InvalidState)
    ));

    w.engine
        .answer_approval(&w.attempt(&id).id, true, "")
        .unwrap();
    w.wait_column(&id, BoardColumn::Done);
    w.engine.card_delete(&id).unwrap();
    assert!(w.engine.board(&w.project_id).unwrap().cards.is_empty());
    let run = w.store.run(card.run_id.as_ref().unwrap()).unwrap();
    assert!(run.finished_at.is_some(), "the run stays in history");
}

#[test]
fn a_card_needs_a_title() {
    let w = world();
    assert!(matches!(
        w.engine.card_create(&w.project_id, "   ", "body"),
        Err(Error::InvalidState)
    ));
    let id = w.new_card("card-1");
    assert!(matches!(
        w.engine.card_update(&id, " ", ""),
        Err(Error::InvalidState)
    ));
    assert_eq!(w.card(&id).title, "card-1");
    assert!(matches!(
        w.engine.card_move(&id, CardLane::Started, 0, "en"),
        Err(Error::InvalidState)
    ));
}

// The JSON the frontend is being written against.

#[test]
fn the_board_has_the_shape_the_contract_says() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set_default("builtin:simple");
    let issue = w.engine.issues(&w.project_id).unwrap().remove(0);
    let id = w.engine.card_import_issue(&w.project_id, &issue).unwrap();
    w.engine.start_board();
    w.engine.card_move(&id, CardLane::Todo, 0, "en").unwrap();
    w.wait_column(&id, BoardColumn::HumanReview);
    w.new_card("card-1");

    let board = serde_json::to_value(w.engine.board(&w.project_id).unwrap()).unwrap();
    let mut top: Vec<&str> = board
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    top.sort_unstable();
    assert_eq!(
        top,
        ["cards", "defaultWorkflow", "maxAgents", "paused", "working"]
    );
    assert_eq!(board["paused"], serde_json::Value::Null);
    assert_eq!(board["maxAgents"], 2);
    assert_eq!(board["working"], 0);
    assert_eq!(board["defaultWorkflow"], "builtin:simple");

    let cards = board["cards"].as_array().unwrap();
    assert_eq!(cards[0]["column"], "backlog", "by column");
    assert_eq!(cards[0]["issue"], serde_json::Value::Null);
    assert_eq!(cards[0]["runState"], serde_json::Value::Null);
    let card = &cards[1];
    let mut keys: Vec<&str> = card
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "alsoTouches",
            "body",
            "column",
            "createdAt",
            "id",
            "issue",
            "jointChangeId",
            "part",
            "position",
            "prState",
            "prStateAt",
            "prUrl",
            "projectId",
            "reason",
            "rejection",
            "runId",
            "runState",
            "startBlocked",
            "title",
        ]
    );
    // An ordinary card is in no change across projects.
    assert_eq!(card["jointChangeId"], serde_json::Value::Null);
    assert_eq!(card["alsoTouches"], serde_json::json!([]));
    assert_eq!(card["part"], serde_json::Value::Null);
    assert_eq!(card["column"], "humanReview");
    assert_eq!(card["runState"], "waitingApproval");
    assert_eq!(card["projectId"], w.project_id);
    assert_eq!(
        card["issue"],
        serde_json::json!({
            "number": 7,
            "title": "Broken",
            "author": "carol",
            "url": "https://github.com/acme/app/issues/7",
        }),
        "with no body"
    );

    // Each column's spelling, and a reason in the shape the UI translates.
    for (column, text) in [
        (BoardColumn::Backlog, "backlog"),
        (BoardColumn::Todo, "todo"),
        (BoardColumn::InProgress, "inProgress"),
        (BoardColumn::HumanReview, "humanReview"),
        (BoardColumn::Done, "done"),
    ] {
        assert_eq!(serde_json::to_value(column).unwrap(), text);
    }
    let lane: CardLane = serde_json::from_value(serde_json::json!("todo")).unwrap();
    assert_eq!(lane, CardLane::Todo);
    assert_eq!(
        serde_json::to_value(w.store.project(&w.project_id).unwrap()).unwrap()["defaultWorkflow"],
        "builtin:simple"
    );
}

// Quitting. `shut_down` is what `app_quit` calls, and `must_ask_before_quitting` is what closing the
// window and the Quit menu both ask first. Staying, in the dialog, calls nothing at all.

const QUIT_PATIENCE: Duration = Duration::from_secs(12);

#[test]
fn stopping_and_quitting_starts_nothing_new_and_the_queue_is_there_on_reopening() {
    keel_harness::needs!(sandbox);
    let mut w = world();
    w.use_one_step();
    w.flag("hold");
    for n in 1..=4 {
        w.queued(&format!("card-{n}"));
    }
    w.engine.start_board();
    w.wait_started(2);

    w.engine.shut_down(true, QUIT_PATIENCE);
    assert_eq!(w.engine.active_count().unwrap(), 0);
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.started().len(), 2, "{:?}", w.started());
    assert_eq!(w.engine.active_count().unwrap(), 0);
    assert_eq!(w.titles_in(BoardColumn::Todo), ["card-3", "card-4"]);

    // Being stopped belongs to that session, not to the data.
    w.reopen(|_| {});
    w.engine.recover().unwrap();
    w.engine.start_board();
    assert_eq!(w.wait_started(3)[2], "card-3");
    for n in 3..=4 {
        w.flag(&format!("go-card-{n}"));
    }
}

#[test]
fn stopping_and_quitting_holds_when_agents_take_different_times_to_end() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.stop_grace = Duration::from_secs(10));
    w.use_one_step();
    w.flag("hold");
    w.flag("slow-stop");
    for n in 1..=4 {
        w.queued(&format!("card-{n}"));
    }
    w.engine.start_board();
    w.wait_started(2);
    let asked = Instant::now();

    // One place frees up long before the other. This is where the next card used to get in.
    w.engine.shut_down(true, QUIT_PATIENCE);
    assert!(
        asked.elapsed() >= Duration::from_secs(1),
        "the slow one ran"
    );
    assert_eq!(w.started().len(), 2, "{:?}", w.started());
    assert_eq!(w.engine.active_count().unwrap(), 0);
}

#[test]
fn quitting_waits_for_a_card_being_started_and_stops_its_agent_too() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.flag("hold");
    let id = w.queued("card-1");

    w.gate.store(true, Ordering::SeqCst);
    let ticking = w.engine.clone();
    let tick = std::thread::spawn(move || ticking.board_tick());
    let deadline = Instant::now() + PATIENCE;
    while w.engine.active_count().unwrap() == 0 {
        assert!(Instant::now() < deadline, "the card never got as far");
        std::thread::sleep(Duration::from_millis(10));
    }

    let quitting = w.engine.clone();
    let (finished, wait) = mpsc::channel();
    std::thread::spawn(move || {
        quitting.shut_down(true, QUIT_PATIENCE);
        finished.send(()).unwrap();
    });
    assert!(
        wait.recv_timeout(A_MOMENT).is_err(),
        "quitting went ahead while a card was being started"
    );

    w.gate.store(false, Ordering::SeqCst);
    wait.recv_timeout(PATIENCE).unwrap();
    tick.join().unwrap().unwrap();
    assert_eq!(w.engine.active_count().unwrap(), 0);
    let card = w.card(&id);
    assert_eq!(card.run_state, Some(AttemptState::Stopped));
    assert_eq!(card.column, BoardColumn::HumanReview);
}

#[test]
fn stopping_and_quitting_also_stops_a_run_that_starts_while_it_waits() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.stop_grace = Duration::from_secs(10));
    w.use_one_step();
    w.flag("hold");
    w.flag("slow-stop");
    w.queued("card-2");
    w.engine.start_board();
    w.wait_started(1);

    let quitting = w.engine.clone();
    let (finished, wait) = mpsc::channel();
    std::thread::spawn(move || {
        quitting.shut_down(true, QUIT_PATIENCE);
        finished.send(()).unwrap();
    });
    // The first agent takes a while to end. A run between two steps or an MCP client could add
    // another in that time, and nothing that was only looked at once would ever stop it.
    std::thread::sleep(Duration::from_millis(300));
    let late = w.by_hand("card-9");
    wait.recv_timeout(PATIENCE).unwrap();

    assert_eq!(w.engine.active_count().unwrap(), 0);
    let attempts = w.store.attempts_of_run(&late).unwrap();
    assert_eq!(attempts.last().unwrap().state, "stopped");
}

#[test]
fn leaving_agents_running_on_quit_stops_only_the_board() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.use_one_step();
    w.flag("hold");
    let running = w.queued("card-1");
    let waiting = w.queued("card-2");
    w.engine.start_board();
    w.wait_started(1);

    w.engine.shut_down(false, QUIT_PATIENCE);
    assert_eq!(w.attempt(&running).state, "running", "it keeps working");

    w.flag("go-card-1");
    w.wait_column(&running, BoardColumn::Done);
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.card(&waiting).column, BoardColumn::Todo);
    assert_eq!(w.started(), ["card-1"]);
}

#[test]
fn staying_in_the_dialog_leaves_the_board_running() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.use_one_step();
    w.flag("hold");
    let running = w.queued("card-1");
    let waiting = w.queued("card-2");
    w.engine.start_board();
    w.wait_started(1);

    // Closing the window, then the Quit menu, each ask. Then the person chooses to stay.
    assert!(w.engine.must_ask_before_quitting());
    assert!(w.engine.must_ask_before_quitting());

    w.flag("go-card-1");
    w.wait_column(&running, BoardColumn::Done);
    assert_eq!(w.wait_started(2)[1], "card-2");
    w.flag("go-card-2");
    w.wait_column(&waiting, BoardColumn::Done);
}

#[test]
fn closing_with_nothing_running_starts_nothing_on_the_way_out() {
    keel_harness::needs!(sandbox);
    let mut w = world();
    w.use_one_step();
    let id = w.queued("card-1");

    assert!(!w.engine.must_ask_before_quitting(), "nothing to ask about");
    w.engine.board_tick().unwrap();
    assert_eq!(w.card(&id).column, BoardColumn::Todo);
    assert!(w.store.open_runs().unwrap().is_empty());

    w.reopen(|_| {});
    w.engine.recover().unwrap();
    w.engine.start_board();
    w.wait_column(&id, BoardColumn::Done);
}

#[test]
fn asking_whether_to_quit_never_makes_the_main_thread_wait_for_a_card_being_started() {
    let w = world();
    w.use_one_step();
    w.queued("card-1");
    w.gate.store(true, Ordering::SeqCst);
    let ticking = w.engine.clone();
    let tick = std::thread::spawn(move || ticking.board_tick());
    let deadline = Instant::now() + PATIENCE;
    while w.engine.active_count().unwrap() == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }

    let asking = w.engine.clone();
    let (answered, answer) = mpsc::channel();
    std::thread::spawn(move || answered.send(asking.must_ask_before_quitting()).unwrap());
    let result = answer.recv_timeout(A_MOMENT);
    w.gate.store(false, Ordering::SeqCst);
    assert_eq!(result, Ok(true), "an agent is about to start, so ask");
    tick.join().unwrap().unwrap();
}

// A failure that would hit any other card too pauses the queue, instead of spending it.

fn blocked_on(w: &World, title: &str, reason: &Reason) -> String {
    let id = w.left_behind(
        title,
        agent("work", ToolProfile::ReadOnly),
        AttemptState::Running,
    );
    let attempt = w.attempt(&id);
    w.store
        .set_state(&attempt.id, AttemptState::Blocked, Some(reason))
        .unwrap();
    id
}

#[test]
fn a_usage_limit_does_not_spend_the_queue_and_lifts_when_the_person_acts() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    let cards: Vec<String> = (1..=5)
        .map(|n| {
            w.flag(&format!("limit-card-{n}"));
            w.queued(&format!("card-{n}"))
        })
        .collect();
    let limit = Some(Reason::RateLimited { resets_at: None });

    w.engine.start_board();
    w.wait_column(&cards[0], BoardColumn::HumanReview);
    w.wait_column(&cards[1], BoardColumn::HumanReview);
    w.wait_paused(limit.clone());
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.started().len(), 2, "{:?}", w.started());
    assert_eq!(
        w.titles_in(BoardColumn::Todo),
        ["card-3", "card-4", "card-5"],
        "only what was already running hit it"
    );
    assert_eq!(w.working(), 0, "room to spare, and still nothing starts");

    for n in 1..=5 {
        w.unflag(&format!("limit-card-{n}"));
    }

    // One run dealt with, one still blocked: it would still hit any other card.
    w.engine.retry(&w.attempt(&cards[0]).id, false).unwrap();
    w.wait_column(&cards[0], BoardColumn::Done);
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.titles_in(BoardColumn::Todo).len(), 3);
    assert_eq!(w.paused(), limit);

    // Leaving the other one lifts it, by itself.
    w.engine.abandon(&w.attempt(&cards[1]).id).unwrap();
    w.wait_paused(None);
    for card in &cards[2..] {
        w.wait_column(card, BoardColumn::Done);
    }
}

#[test]
fn retrying_the_only_blocked_run_lifts_the_pause() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.use_one_step();
    w.flag("limit-card-1");
    let blocked = w.queued("card-1");
    let waiting = w.queued("card-2");
    w.engine.start_board();
    w.wait_column(&blocked, BoardColumn::HumanReview);
    w.wait_paused(Some(Reason::RateLimited { resets_at: None }));
    assert_eq!(w.card(&waiting).column, BoardColumn::Todo);

    w.unflag("limit-card-1");
    w.engine.retry(&w.attempt(&blocked).id, false).unwrap();
    w.wait_column(&blocked, BoardColumn::Done);
    w.wait_column(&waiting, BoardColumn::Done);
    w.wait_paused(None);
}

#[test]
fn a_usage_limit_with_a_known_reset_lifts_once_that_time_has_passed() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    let now = now_in_seconds();
    let blocked = blocked_on(
        &w,
        "card-1",
        &Reason::RateLimited {
            resets_at: Some(now + 3600),
        },
    );
    let waiting = w.queued("card-2");

    w.engine.board_tick().unwrap();
    assert_eq!(w.card(&waiting).column, BoardColumn::Todo);
    assert_eq!(
        w.paused(),
        Some(Reason::RateLimited {
            resets_at: Some(now + 3600)
        })
    );

    // That hour goes by. The run is still blocked, with nobody having touched it.
    let attempt = w.attempt(&blocked);
    w.store
        .set_state(
            &attempt.id,
            AttemptState::Blocked,
            Some(&Reason::RateLimited {
                resets_at: Some(now - 1),
            }),
        )
        .unwrap();
    w.engine.board_tick().unwrap();
    assert_eq!(w.paused(), None);
    w.wait_column(&waiting, BoardColumn::Done);
    assert_eq!(w.card(&blocked).column, BoardColumn::HumanReview);
}

fn now_in_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn a_rejected_session_pauses_the_queue() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.flag("session-card-1");
    let blocked = w.queued("card-1");
    w.engine.start_board();
    let card = w.wait_column(&blocked, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::Blocked));
    let reason = card.reason.unwrap();
    assert!(
        matches!(reason, Reason::SessionRejected { .. }),
        "{reason:?}"
    );

    w.wait_paused(Some(reason));
    let waiting = w.queued("card-2");
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.card(&waiting).column, BoardColumn::Todo);
}

#[test]
fn losing_the_github_session_at_preparation_pauses_the_queue_and_retrying_lifts_it() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.use_one_step();
    w.flag("gh-signed-out");
    let blocked = w.queued("card-1");
    let waiting = w.queued("card-2");
    w.engine.start_board();

    let card = w.wait_column(&blocked, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::Blocked));
    let reason = card.reason.clone().unwrap();
    let Reason::DeliveryFailed { kind, .. } = &reason else {
        panic!("{reason:?}")
    };
    assert_eq!(kind, "notSignedIn");
    assert_eq!(w.read_log("calls.log"), "", "no agent got that far");
    w.wait_paused(Some(reason));
    assert_eq!(w.card(&waiting).column, BoardColumn::Todo);

    w.unflag("gh-signed-out");
    w.engine.retry(&w.attempt(&blocked).id, false).unwrap();
    w.wait_column(&blocked, BoardColumn::Done);
    w.wait_column(&waiting, BoardColumn::Done);
    w.wait_paused(None);
}

#[test]
fn a_run_started_by_hand_that_is_blocked_pauses_the_queue_too() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.flag("limit-card-9");
    let run = w.by_hand("card-9");
    let deadline = Instant::now() + PATIENCE;
    while w.store.attempts_of_run(&run).unwrap().pop().unwrap().state != "blocked" {
        assert!(Instant::now() < deadline, "the run never got blocked");
        std::thread::sleep(Duration::from_millis(20));
    }
    let waiting = w.queued("card-1");
    w.engine.start_board();
    w.wait_paused(Some(Reason::RateLimited { resets_at: None }));
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.card(&waiting).column, BoardColumn::Todo);

    let attempt = w.store.attempts_of_run(&run).unwrap().pop().unwrap();
    w.engine.abandon(&attempt.id).unwrap();
    w.wait_column(&waiting, BoardColumn::Done);
}

#[test]
fn a_pause_is_still_there_after_reopening() {
    keel_harness::needs!(sandbox);
    let mut w = world();
    w.use_one_step();
    let limit = Reason::RateLimited { resets_at: None };
    let blocked = blocked_on(&w, "card-1", &limit);
    let waiting = w.queued("card-2");

    w.reopen(|_| {});
    w.engine.recover().unwrap();
    w.engine.start_board();
    w.wait_paused(Some(limit));
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.card(&waiting).column, BoardColumn::Todo);

    w.engine.abandon(&w.attempt(&blocked).id).unwrap();
    w.wait_column(&waiting, BoardColumn::Done);
    w.wait_paused(None);
}

#[test]
fn a_failure_that_belongs_to_one_run_pauses_nothing() {
    let delivery = |kind: &str| Reason::DeliveryFailed {
        kind: kind.into(),
        detail: "refused".into(),
    };
    for (n, reason) in [
        delivery("repoRules"),
        delivery("signedCommits"),
        delivery("workflowScope"),
        delivery("noPermission"),
        delivery("sso"),
        delivery("other"),
        Reason::NothingToDeliver,
        Reason::ServiceFailed {
            name: "db".into(),
            detail: "down".into(),
        },
        Reason::ReworkLimit { rounds: 2 },
        Reason::UnsafeBranch {
            branch: "main".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        let w = world();
        w.use_one_step();
        blocked_on(&w, &format!("card-{n}"), &reason);
        let next = w.queued("card-99");
        w.engine.board_tick().unwrap();
        assert_eq!(w.paused(), None, "{reason:?}");
        assert_ne!(w.card(&next).column, BoardColumn::Todo, "{reason:?}");
    }
}

#[test]
fn a_failed_check_pauses_nothing_end_to_end() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.set_default(&saved(
        &w.engine,
        &w.project_id,
        "With a check",
        vec![
            agent("work", ToolProfile::ReadOnly),
            WorkflowStep::Check {
                id: "check".into(),
                run: "exit 3".into(),
            },
        ],
    ));
    let first = w.queued("card-1");
    let second = w.queued("card-2");
    w.engine.start_board();
    let card = w.wait_column(&first, BoardColumn::HumanReview);
    assert_eq!(
        card.reason,
        Some(Reason::CheckFailed { exit_code: Some(3) })
    );
    let next = w.wait_column(&second, BoardColumn::HumanReview);
    assert_eq!(
        next.reason,
        Some(Reason::CheckFailed { exit_code: Some(3) })
    );
    assert_eq!(w.paused(), None);
}

// `paused` is what the UI shows when the scheduler isn't starting anything.

#[test]
fn a_login_that_expires_after_launch_shows_as_paused_and_reading_it_launches_nothing() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    w.engine.start_board();
    w.wait_paused(None);

    w.flag("logged-out");
    let id = w.queued("card-1");
    w.wait_paused(Some(Reason::ClaudeLoggedOut));
    assert_eq!(
        serde_json::to_value(w.engine.board(&w.project_id).unwrap()).unwrap()["paused"],
        serde_json::json!({ "code": "claudeLoggedOut" })
    );
    assert_eq!(w.card(&id).column, BoardColumn::Todo);

    let reads = w.session_reads();
    for _ in 0..20 {
        w.engine.board(&w.project_id).unwrap();
    }
    assert_eq!(
        w.session_reads(),
        reads,
        "the board only reads the last look"
    );

    // Signing in, and the person refreshing the session, lifts it with nothing else.
    w.unflag("logged-out");
    w.engine.set_tools(tools_in(&[&w.dir.path().join("bin")]));
    w.wait_column(&id, BoardColumn::Done);
    w.wait_paused(None);
}

#[test]
fn a_change_goes_out_when_paused_changes_and_only_then() {
    let w = world();
    w.use_one_step();
    w.flag("logged-out");
    w.queued("card-1");
    let changes = || w.changes.lock().unwrap().len();

    let before = changes();
    w.engine.board_tick().unwrap();
    assert_eq!(w.paused(), Some(Reason::ClaudeLoggedOut));
    assert_eq!(changes(), before + 1, "paused");
    w.engine.board_tick().unwrap();
    w.engine.board_tick().unwrap();
    assert_eq!(changes(), before + 1, "still paused, so nothing to say");

    w.unflag("logged-out");
    w.engine.board_tick().unwrap();
    assert_eq!(w.paused(), None);
    assert!(changes() > before + 1, "lifted");
}

#[test]
fn paused_has_the_shape_of_any_other_reason() {
    let w = world();
    w.use_one_step();
    blocked_on(&w, "card-1", &Reason::RateLimited { resets_at: None });
    w.engine.board_tick().unwrap();
    assert_eq!(
        serde_json::to_value(w.engine.board(&w.project_id).unwrap()).unwrap()["paused"],
        serde_json::json!({ "code": "rateLimited", "resetsAt": null })
    );
}

#[test]
fn a_tick_with_nothing_to_start_reads_no_session_and_a_stale_pause_goes_away() {
    let w = world();
    w.use_one_step();
    w.flag("logged-out");
    let id = w.queued("card-1");
    w.engine.board_tick().unwrap();
    assert_eq!(w.paused(), Some(Reason::ClaudeLoggedOut));

    // The only card that was waiting goes back: nothing is being held up any more.
    w.engine.card_move(&id, CardLane::Backlog, 0, "en").unwrap();
    let reads = w.session_reads();
    for _ in 0..5 {
        w.engine.board_tick().unwrap();
    }
    assert_eq!(w.session_reads(), reads);
    assert_eq!(w.paused(), None);
}

#[test]
fn with_no_room_a_card_waits_for_room_not_for_the_session() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.use_one_step();
    w.flag("hold");
    w.queued("card-1");
    w.engine.board_tick().unwrap();
    w.wait_started(1);
    w.flag("logged-out");
    w.queued("card-2");

    let reads = w.session_reads();
    w.engine.board_tick().unwrap();
    assert_eq!(w.session_reads(), reads);
    assert_eq!(w.paused(), None, "it's waiting for room, and that shows");
    w.flag("go-card-1");
}

// A card runs once, whatever moment the app dies at.

#[test]
fn dying_with_the_run_linked_and_not_yet_begun_runs_the_card_once() {
    keel_harness::needs!(sandbox);
    let mut w = world();
    let workflow = one_step(&w.engine, &w.project_id);
    w.set_default(&workflow);
    let id = w.queued("card-1");
    let steps = w
        .engine
        .workflow(&w.project_id, &workflow, "en")
        .unwrap()
        .unwrap()
        .steps;
    assert_eq!(w.store.claim_next_todo_card().unwrap().unwrap().id, id);
    w.store
        .add_run_for_card(
            &NewRun {
                id: "cut-short".into(),
                project_id: w.project_id.clone(),
                title: "card-1".into(),
                request: "card-1".into(),
                workflow: serde_json::to_string(&steps).unwrap(),
                language: "en".into(),
                source_issue: None,
                request_written: None,
                workflow_id: None,
                workflow_name: None,
                started_by: None,
            },
            &id,
        )
        .unwrap();
    assert_eq!(w.working(), 1, "no attempt yet, and it holds a place");

    w.reopen(|_| {});
    w.engine.recover().unwrap();
    w.engine.start_board();
    let card = w.wait_column(&id, BoardColumn::Done);
    assert_eq!(card.run_id.as_deref(), Some("cut-short"));
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.started(), ["card-1"], "one agent for one card");
    assert_eq!(w.store.runs(10).unwrap().len(), 1);
}

#[test]
fn a_start_that_fails_on_disk_leaves_no_run_behind_to_hold_a_place_or_start_later() {
    let mut w = world();
    w.use_one_step();
    // Where runs keep their files can't be created.
    let data = w.dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("runs"), "in the way").unwrap();
    let id = w.queued("card-1");

    w.engine.board_tick().unwrap();
    let card = w.card(&id);
    assert_eq!(card.column, BoardColumn::Backlog);
    assert!(
        matches!(card.start_blocked, Some(Reason::PrepareFailed { .. })),
        "{:?}",
        card.start_blocked
    );
    assert_eq!(w.working(), 0, "nothing is holding a place");
    assert!(w.store.open_runs().unwrap().is_empty());

    std::fs::remove_file(data.join("runs")).unwrap();
    w.reopen(|_| {});
    w.engine.recover().unwrap();
    std::thread::sleep(A_MOMENT);
    assert_eq!(
        w.read_log("calls.log"),
        "",
        "and the next launch starts nothing"
    );
}

// What starts is what the card says at the moment it's taken.

#[test]
fn an_edit_saved_while_the_session_is_being_read_reaches_the_run() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.use_one_step();
    let id = w.queued("card-1");
    w.flag("slow-auth");
    w.engine.start_board();
    // The session takes 1.5 seconds to read here, and the scheduler is in the middle of that.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(w.card(&id).column, BoardColumn::Todo);
    w.engine
        .card_update(&id, "card-1", "THE INSTRUCTION")
        .unwrap();

    let card = w.wait_column(&id, BoardColumn::Done);
    let run = w.store.run(card.run_id.as_deref().unwrap()).unwrap();
    assert_eq!(run.request, "card-1\n\nTHE INSTRUCTION");
}

#[test]
fn moving_a_card_to_the_top_while_the_session_is_being_read_starts_it_first() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.use_one_step();
    let first = w.queued("card-1");
    let moved = w.queued("card-5");
    w.flag("slow-auth");
    w.engine.start_board();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(w.card(&first).column, BoardColumn::Todo);
    w.engine.card_move(&moved, CardLane::Todo, 0, "en").unwrap();

    w.wait_column(&first, BoardColumn::Done);
    w.wait_column(&moved, BoardColumn::Done);
    assert_eq!(w.started(), ["card-5", "card-1"]);
}

// The board's places and the agents' slots agree.

#[test]
fn a_stalled_agent_keeps_its_place_so_the_next_card_waits_in_todo() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| {
        c.max_agents = 1;
        c.stall_after = Duration::from_millis(100);
        c.stall_hard_limit = Duration::from_millis(100);
    });
    w.use_one_step();
    w.flag("hold");
    let stalled = w.queued("card-1");
    let next = w.queued("card-2");
    w.engine.start_board();

    let card = w.wait_column(&stalled, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::Stalled));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        w.card(&next).column,
        BoardColumn::Todo,
        "not In Progress waiting for a slot it can't have"
    );
    assert_eq!(w.started(), ["card-1"]);
    assert_eq!(w.working(), 1);

    w.engine.wait_longer(&w.attempt(&stalled).id).unwrap();
    assert_eq!(w.working(), 1, "1 of 1, not 2 of 1");
    w.flag("go-card-1");
    w.wait_column(&stalled, BoardColumn::Done);
    assert_eq!(w.wait_started(2)[1], "card-2");
    w.flag("go-card-2");
    w.wait_column(&next, BoardColumn::Done);
}

#[test]
fn a_failed_review_with_another_still_running_keeps_its_place() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    w.flag("fail-security");
    w.flag("hold-code");
    let reviewed = w.queued("card-1");
    let next = w.queued("card-2");
    w.engine.start_board();

    // It's in Human Review as soon as one review fails, and the other agent still has its slot.
    let card = w.wait_column(&reviewed, BoardColumn::HumanReview);
    assert_eq!(card.run_state, Some(AttemptState::Failed));
    std::thread::sleep(A_MOMENT);
    assert_eq!(w.working(), 1);
    assert_eq!(w.card(&next).column, BoardColumn::Todo);

    // Once it ends, this run is only waiting for a person, and the place is free.
    w.flag("go-code");
    let deadline = Instant::now() + PATIENCE;
    while w.card(&next).column == BoardColumn::Todo {
        assert!(Instant::now() < deadline, "the next card never started");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_run_between_two_steps_keeps_its_place() {
    keel_harness::needs!(sandbox);
    // The scheduler is woken the moment a step ends, before the next one is recorded. If that gap
    // looked like room, a card would slip in at every step of every run.
    let w = world_with(|c| c.max_agents = 1);
    w.set_default(&saved(
        &w.engine,
        &w.project_id,
        "Three steps",
        vec![
            agent("one", ToolProfile::ReadOnly),
            agent("two", ToolProfile::ReadOnly),
            agent("three", ToolProfile::ReadOnly),
        ],
    ));
    let cards: Vec<String> = (1..=3).map(|n| w.queued(&format!("card-{n}"))).collect();
    w.engine.start_board();
    for card in &cards {
        w.wait_column(card, BoardColumn::Done);
    }
    assert_eq!(
        w.started(),
        [
            "card-1", "card-1", "card-1", "card-2", "card-2", "card-2", "card-3", "card-3",
            "card-3"
        ]
    );
}

// Each column reads in its own order.

#[test]
fn done_has_the_latest_on_top_and_what_is_going_on_has_the_oldest_on_top() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 3);
    w.use_one_step();
    w.flag("hold");
    let cards: Vec<String> = (1..=3).map(|n| w.queued(&format!("card-{n}"))).collect();
    w.engine.start_board();
    w.wait_started(3);
    assert_eq!(
        w.titles_in(BoardColumn::InProgress),
        ["card-1", "card-2", "card-3"]
    );

    for n in [2, 3, 1] {
        std::thread::sleep(Duration::from_millis(5));
        w.flag(&format!("go-card-{n}"));
        w.wait_column(&cards[n - 1], BoardColumn::Done);
    }
    assert_eq!(
        w.titles_in(BoardColumn::Done),
        ["card-1", "card-3", "card-2"]
    );
}

#[test]
fn human_review_has_the_card_that_has_waited_longest_on_top() {
    keel_harness::needs!(sandbox);
    let w = world_with(|c| c.max_agents = 1);
    let ask = answer_step(&w.engine, &w.project_id);
    w.set_default(&ask);
    let cards: Vec<String> = (1..=3).map(|n| w.queued(&format!("card-{n}"))).collect();
    w.engine.start_board();
    for card in &cards {
        w.wait_column(card, BoardColumn::HumanReview);
    }
    assert_eq!(
        w.titles_in(BoardColumn::HumanReview),
        ["card-1", "card-2", "card-3"]
    );
}
