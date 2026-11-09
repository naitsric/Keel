//! A change across several projects, agreed on one card, against a fake `claude`: the run that
//! agrees on it, and the parts it puts on the board once a person approves.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, BoardColumn, CardLane, Reason, SpecProblem};
use keel_engine::{Answer, AnswerChoice, Config, Engine, Error, StartRun};
use keel_harness::Tools;
use keel_harness::github::Issue;
use keel_store::rusqlite_value::Value as Column;
use keel_store::{Attempt, Store};
use serde_json::{Value, json};

struct World {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    /// Where the card is, and where the change is agreed.
    app: String,
    valora: String,
    valora_path: PathBuf,
    log: PathBuf,
    queue: PathBuf,
}

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap()
            .status
            .success(),
        "git {args:?}"
    );
}

/// A specifier takes the next answer from a numbered file. Every call is logged with its schema.
fn fake(log: &Path, queue: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true}}'; exit 0;;
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
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
{{ echo "=== CALL"; echo "$prompt"; echo "--- schema"; echo "$schema"; echo "--- end"; }} >> "{log}/calls.log"
n=$(cat "{queue}/count" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "{queue}/count"
output="$(cat "{queue}/$n.json")"
printf '%s\n' "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"num_turns\":1,\"structured_output\":$output}}"
"##,
        log = log.display(),
        queue = queue.display(),
    )
}

fn repo(dir: &Path, name: &str) -> PathBuf {
    let repo = dir.join(name);
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "x\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let log = dir.path().join("log");
    let queue = dir.path().join("queue");
    for path in [&bin, &log, &queue] {
        std::fs::create_dir(path).unwrap();
    }
    let claude = bin.join("claude");
    std::fs::write(&claude, fake(&log, &queue)).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    config.max_agents = 5;
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let app = engine.add_project(&repo(dir.path(), "app")).unwrap().id;
    let valora_path = repo(dir.path(), "valora");
    let valora = engine.add_project(&valora_path).unwrap().id;
    World {
        _dir: dir,
        store,
        engine,
        app,
        valora,
        valora_path,
        log,
        queue,
    }
}

/// What it agrees on: `parts` names projects as the agent was given them.
fn agreed(parts: &[(&str, &str)]) -> Value {
    json!({
        "size": "normal",
        "risks": [],
        "summary": "Log in from Valora into back-adm",
        "criteria": [{ "id": "C1", "when": "someone logs in on Valora", "then": "back-adm lets them in" }],
        "changes": [],
        "limits": ["The /session route keeps its name"],
        "decided": [],
        "questions": [],
        "parts": parts.iter().map(|(project, text)| json!({ "project": project, "text": text })).collect::<Vec<_>>(),
        "together": ["Log in on Valora and land in back-adm"],
    })
}

fn both() -> Value {
    agreed(&[("valora", "Sends the token"), ("app", "Reads the token")])
}

impl World {
    /// What the specifier answers, in order.
    fn answers(&self, outputs: &[Value]) {
        for (index, output) in outputs.iter().enumerate() {
            std::fs::write(
                self.queue.join(format!("{}.json", index + 1)),
                output.to_string(),
            )
            .unwrap();
        }
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.log.join("calls.log")).unwrap_or_default()
    }

    /// A card in `app` whose request touches Valora, started by the board.
    fn agreeing(&self) -> (String, String) {
        let card = self
            .engine
            .card_create(&self.app, "Log in through Valora", "")
            .unwrap();
        self.engine
            .card_set_projects(&card, std::slice::from_ref(&self.valora))
            .unwrap();
        self.engine
            .card_move(&card, CardLane::Todo, 0, "en")
            .unwrap();
        self.engine.board_tick().unwrap();
        let run = self.store.card(&card).unwrap().run_id.expect("it started");
        (card, run)
    }

    fn attempts(&self, run: &str, step: &str) -> Vec<Attempt> {
        self.store
            .attempts_of_run(run)
            .unwrap()
            .into_iter()
            .filter(|a| a.step_id == step)
            .collect()
    }

    fn wait_for(&self, run: &str, step: &str, state: AttemptState) -> Attempt {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(found) = self.attempts(run, step).pop()
                && found.state == state.as_str()
            {
                return found;
            }
            let everything: Vec<_> = self
                .store
                .attempts_of_run(run)
                .unwrap()
                .into_iter()
                .map(|a| (a.step_id, a.state, a.reason))
                .collect();
            assert!(
                Instant::now() < deadline,
                "{step} never reached {state:?}: {everything:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The parts on the board, first to last.
    fn parts(&self, card: &str) -> Vec<keel_store::Card> {
        self.store
            .joint_change(card)
            .unwrap()
            .parts
            .iter()
            .map(|id| self.store.card(id).unwrap())
            .collect()
    }
}

fn reason_of(attempt: &Attempt) -> Reason {
    serde_json::from_value(attempt.reason.clone().expect("has a reason")).unwrap()
}

#[test]
fn an_approved_agreement_opens_a_part_for_each_project_in_order() {
    let w = world();
    w.answers(&[both()]);
    let (card, run) = w.agreeing();
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);

    // It agrees on what changes where, reading only, and with the names it was given.
    let calls = w.calls();
    assert_eq!(calls.matches("=== CALL").count(), 1);
    assert!(calls.contains("\"parts\""), "the agreement's schema");
    assert!(
        calls.contains("- app (the one you're in)\n- valora"),
        "{calls}"
    );
    assert!(w.store.run(&run).unwrap().workflow_id.is_none());
    let spec = &w.attempts(&run, "spec")[0];
    assert_eq!(spec.tools, "readOnly");
    assert!(
        w.parts(&card).is_empty(),
        "nothing opens before it's approved"
    );

    w.engine
        .answer_approval(&approval.id, true, "Go ahead")
        .unwrap();
    let run_now = w.store.run(&run).unwrap();
    assert!(run_now.finished_at.is_some());
    assert!(run_now.pr_url.is_none(), "an agreement delivers nothing");

    let parts = w.parts(&card);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].project_id, w.valora);
    assert_eq!(parts[0].body, "Sends the token");
    assert_eq!(parts[0].column(), BoardColumn::Todo);
    assert!(!parts[0].waiting);
    assert_eq!(parts[1].project_id, w.app);
    assert_eq!(
        parts[1].waits_for_card.as_deref(),
        Some(parts[0].id.as_str())
    );
    assert!(parts[1].waiting);
    for part in &parts {
        assert_eq!(part.title, "Log in through Valora");
        assert_eq!(part.language.as_deref(), Some("en"));
    }
    let change = w.store.joint_change(&card).unwrap();
    assert_eq!(change.agreed.unwrap()["parts"][0]["project"], "valora");
    assert_eq!(
        change.checks[0].text,
        "Log in on Valora and land in back-adm"
    );
    assert_eq!(change.checks[0].confirmed_at, None);
    assert_eq!(
        w.store.card(&card).unwrap().column(),
        BoardColumn::Done,
        "the agreement itself is done"
    );

    // It's no workflow, so it doesn't count in how they compare.
    assert!(w.engine.workflow_results(&w.app).unwrap().is_empty());
    // What was approved stays what was approved.
    assert!(matches!(
        w.engine.retry(&spec.id, false),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        w.engine.retry(&w.attempts(&run, "human")[0].id, false),
        Err(Error::InvalidState)
    ));
    // The approval note is about this project only.
    assert!(w.store.decisions(&w.valora).unwrap().is_empty());
}

#[test]
fn an_answer_remembered_while_agreeing_reaches_every_project_with_a_part() {
    let w = world();
    let mut asks = both();
    asks["questions"] = json!([{
        "id": "Q1",
        "text": "Which channel carries the token?",
        "options": [
            { "label": "A header", "recommended": true },
            { "label": "A cookie", "recommended": false },
        ],
    }]);
    w.answers(&[asks]);
    let (card, run) = w.agreeing();
    let asking = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    assert!(
        w.parts(&card).is_empty(),
        "nothing opens while it's undecided"
    );
    w.engine
        .answer_questions(
            &asking.id,
            &[Answer {
                question: "Q1".into(),
                choice: AnswerChoice::Option {
                    label: "A header".into(),
                },
                remember: true,
            }],
        )
        .unwrap();
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();

    let elsewhere = w.store.decisions(&w.valora).unwrap();
    assert_eq!(elsewhere.len(), 1);
    assert_eq!(elsewhere[0].text, "A header");
    let agreed = w.store.joint_change(&card).unwrap().agreed.unwrap();
    assert!(
        agreed["decided"][0]["text"]
            .as_str()
            .unwrap()
            .contains("A header"),
        "every part gets it in what was agreed: {agreed}"
    );
}

#[test]
fn a_rejected_agreement_opens_nothing_and_can_be_agreed_again() {
    let w = world();
    w.answers(&[both(), both()]);
    let (card, run) = w.agreeing();
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    w.engine
        .answer_approval(&approval.id, false, "Valora shouldn't change")
        .unwrap();
    assert!(w.parts(&card).is_empty());
    assert_eq!(w.store.card(&card).unwrap().column(), BoardColumn::Backlog);
    assert!(w.store.joint_change(&card).unwrap().agreed.is_none());

    // It can still change what it touches, and go again.
    w.engine
        .card_set_projects(&card, std::slice::from_ref(&w.valora))
        .unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    w.engine.board_tick().unwrap();
    let again = w.store.card(&card).unwrap().run_id.unwrap();
    assert_ne!(again, run);
    let approval = w.wait_for(&again, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    assert_eq!(w.parts(&card).len(), 2);
}

#[test]
fn a_part_in_a_project_it_was_not_given_is_sent_back_to_fix() {
    let w = world();
    w.answers(&[
        agreed(&[
            ("dupplaBack", "Reads the session"),
            ("app", "Reads the token"),
        ]),
        both(),
    ]);
    let (card, run) = w.agreeing();
    w.wait_for(&run, "human", AttemptState::WaitingApproval);
    let spec = w.attempts(&run, "spec");
    assert_eq!(spec.len(), 2);
    assert_eq!(
        reason_of(&spec[0]),
        Reason::SpecNotUsable {
            problems: vec![SpecProblem::UnknownProject {
                project: "dupplaBack".into()
            }]
        }
    );
    assert!(w.calls().contains("`parts` names dupplaBack"));
    assert!(w.parts(&card).is_empty());
}

#[test]
fn parts_that_cannot_open_hold_the_approval_until_tried_again_and_open_once() {
    let w = world();
    w.answers(&[both()]);
    let (card, run) = w.agreeing();
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);

    // Valora left Keel while this waited.
    w.engine.remove_project(&w.valora).unwrap();
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    let held = w.wait_for(&run, "human", AttemptState::Blocked);
    assert_eq!(
        reason_of(&held),
        Reason::PartsNotOpened {
            detail: "valora is no longer in Keel".into()
        }
    );
    assert!(w.store.run(&run).unwrap().finished_at.is_none());
    assert!(w.parts(&card).is_empty());

    // Back again, the person tries once more.
    w.engine.add_project(&w.valora_path).unwrap();
    w.engine.retry(&held.id, false).unwrap();
    let again = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&again.id, true, "").unwrap();
    assert_eq!(w.parts(&card).len(), 2);
    assert!(w.store.run(&run).unwrap().finished_at.is_some());

    // As if the app had stopped between opening them and closing the run.
    w.store
        .patch_run(&run, &[("finished_at", Column::Null)])
        .unwrap();
    w.engine.recover().unwrap();
    assert!(w.store.run(&run).unwrap().finished_at.is_some());
    assert_eq!(w.parts(&card).len(), 2, "nothing opened twice");
    let everything: usize = [&w.app, &w.valora]
        .iter()
        .map(|p| w.store.cards(p).unwrap().len())
        .sum();
    assert_eq!(
        everything, 3,
        "the card where it was agreed and its two parts"
    );
}

#[test]
fn an_assistant_cannot_start_the_issue_of_a_change_across_projects() {
    let w = world();
    let issue = Issue {
        number: 434,
        title: "Log in through Valora".into(),
        author: "someone".into(),
        url: "https://github.com/acme/back/issues/434".into(),
        body: "Valora logs in, back-adm lets them in.".into(),
    };
    let card = w.engine.card_import_issue(&w.app, &issue).unwrap();
    w.engine
        .card_set_projects(&card, std::slice::from_ref(&w.valora))
        .unwrap();
    let workflow = w
        .engine
        .workflow(&w.app, "builtin:simple", "en")
        .unwrap()
        .unwrap();
    let started = w.engine.start_run_on_board(StartRun {
        project_id: w.app.clone(),
        title: None,
        request: "Do it".into(),
        saved_workflow: Some(workflow.reference()),
        workflow: workflow.steps,
        language: "en".into(),
        source_issue: Some(issue),
        started_by: Some("client".into()),
    });
    assert!(
        matches!(started, Err(Error::Blocked(Reason::StartsOnTheBoard))),
        "{started:?}"
    );
    assert_eq!(w.store.card(&card).unwrap().column(), BoardColumn::Backlog);
    assert!(w.calls().is_empty());
}
