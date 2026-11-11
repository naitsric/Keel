//! A change across several projects, agreed on one card, against a fake `claude`: the run that
//! agrees on it, the parts it puts on the board once a person approves, and what each of those
//! reads of the other projects.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{
    AttemptState, BoardColumn, CardLane, PrState, Reason, SpecProblem, StepCondition, StepRole,
    StepSpec, ToolProfile, WorkflowStep,
};
use keel_engine::{
    Answer, AnswerChoice, Config, Engine, Error, Neighbour, PartOf, PartWarning, StartRun,
    Workflow, full_workflow,
};
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
    data: PathBuf,
    log: PathBuf,
    queue: PathBuf,
    flags: PathBuf,
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

/// Each call takes the next answer from a numbered file. Every call is logged with its schema, its
/// arguments, what Keel told it and its settings.
fn fake(log: &Path, queue: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true}}'; exit 0;;
esac
prompt="$(cat)"
tools=""; schema=""; system=""; settings=""; prev=""
for a in "$@"; do
  [ "$prev" = "--tools" ] && tools="$a"
  [ "$prev" = "--json-schema" ] && schema="$a"
  [ "$prev" = "--append-system-prompt-file" ] && system="$a"
  [ "$prev" = "--settings" ] && settings="$a"
  prev="$a"
done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
[ -n "$schema" ] && list="$list\",\"StructuredOutput"
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
{{ echo "=== CALL"; echo "$prompt"; echo "--- schema"; echo "$schema"; echo "--- args"; echo "$*"; echo "--- system"; cat "$system"; echo "--- settings"; cat "$settings"; echo; echo "--- end"; }} >> "{log}/calls.log"
if [ -f "{queue}/commits" ] && [ "$tools" != "Read,Grep,Glob" ]; then
  echo work >> "built-$RANDOM.txt"; git add -A && git commit -qm "build"
fi
n=$(cat "{queue}/count" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "{queue}/count"
output="$(cat "{queue}/$n.json")"
printf '%s\n' "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"num_turns\":1,\"structured_output\":$output}}"
"##,
        log = log.display(),
        queue = queue.display(),
    )
}

/// GitHub as far as Keel asks it, for any repo under `acme`. Pull requests are numbered from 7 in
/// the order they're opened, and each keeps its description. `pr view` answers with what's in
/// `flags/pr-view-<owner>-<name>`, and fails without it.
fn fake_gh(log: &Path, flags: &Path) -> String {
    format!(
        r##"#!/bin/bash
echo "gh $*" >> "{log}/gh.log"
repo=""; prev=""
for a in "$@"; do [ "$prev" = "--repo" ] && repo="$a"; prev="$a"; done
case "$1 $2" in
"auth token") echo "tok-SECRET"; exit 0;;
"repo view") echo '{{"defaultBranchRef":{{"name":"main"}},"viewerPermission":"WRITE","isPrivate":true}}'; exit 0;;
"api "*) echo false; exit 0;;
"pr create")
  n=$(cat "{log}/prs" 2>/dev/null || echo 6); n=$((n + 1)); echo "$n" > "{log}/prs"
  prev=""
  for a in "$@"; do [ "$prev" = "--body-file" ] && cp "$a" "{log}/pr-body-${{repo/\//-}}.md"; prev="$a"; done
  echo "https://github.com/$repo/pull/$n"; exit 0;;
"pr view")
  if [ -f "{flags}/pr-view-${{repo/\//-}}" ]; then cat "{flags}/pr-view-${{repo/\//-}}"; exit 0; fi
  echo "HTTP 502: Bad Gateway" >&2; exit 1;;
esac
echo "unexpected: $*" >&2; exit 1
"##,
        log = log.display(),
        flags = flags.display(),
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
    world_with(false)
}

/// Each project's folder points at its repo on GitHub, `acme/app` and `acme/valora`. Pushing goes
/// to a bare repo of each, and `gh` is the fake one.
fn world_on_github() -> World {
    world_with(true)
}

fn world_with(on_github: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let log = dir.path().join("log");
    let queue = dir.path().join("queue");
    let flags = dir.path().join("flags");
    for path in [&bin, &log, &queue, &flags] {
        std::fs::create_dir(path).unwrap();
    }
    let claude = bin.join("claude");
    std::fs::write(&claude, fake(&log, &queue)).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    if on_github {
        let gh = bin.join("gh");
        std::fs::write(&gh, fake_gh(&log, &flags)).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let data = dir.path().join("data");
    let mut config = Config::new(data.clone());
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    config.max_agents = 5;
    config.github_git_base = format!("file://{}/remotes", dir.path().display());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let project = |name: &str| {
        let path = repo(dir.path(), name);
        if on_github {
            let remote = dir.path().join("remotes/acme").join(format!("{name}.git"));
            std::fs::create_dir_all(&remote).unwrap();
            git(&remote, &["init", "-q", "--bare", "-b", "main"]);
            git(&path, &["push", "-q", &remote.to_string_lossy(), "main"]);
            git(
                &path,
                &[
                    "remote",
                    "add",
                    "origin",
                    &format!("git@github.com:acme/{name}.git"),
                ],
            );
        }
        let id = engine.add_project(&path).unwrap().id;
        (id, path)
    };
    let (app, _) = project("app");
    let (valora, valora_path) = project("valora");
    World {
        _dir: dir,
        store,
        engine,
        app,
        valora,
        valora_path,
        data,
        log,
        queue,
        flags,
    }
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Adds a file to a project's folder, as its own commit.
fn commit(repo: &Path, file: &str, text: &str) -> String {
    std::fs::write(repo.join(file), text).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", file]);
    git_out(repo, &["rev-parse", "HEAD"])
}

fn agent(id: &str, role: StepRole, when: Option<StepCondition>, prompt: &str) -> WorkflowStep {
    agent_with(id, ToolProfile::ReadOnly, role, when, prompt)
}

fn agent_with(
    id: &str,
    tools: ToolProfile,
    role: StepRole,
    when: Option<StepCondition>,
    prompt: &str,
) -> WorkflowStep {
    WorkflowStep::Agent(StepSpec {
        id: id.into(),
        model: "fake".into(),
        effort: None,
        tools,
        prompt: prompt.into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when,
        risks: vec![],
        model_when_risky: None,
        role,
    })
}

/// A spec of a part, which changes only its own project.
fn plain(summary: &str) -> Value {
    json!({
        "size": "normal",
        "risks": [],
        "summary": summary,
        "criteria": [{ "id": "C1", "when": "it's asked", "then": "it answers" }],
        "changes": [],
        "limits": [],
        "decided": [],
        "questions": [],
    })
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

    /// One call, from the first.
    fn call(&self, index: usize) -> String {
        self.calls()
            .split("=== CALL")
            .nth(index + 1)
            .unwrap_or_else(|| panic!("no call {index}: {}", self.calls()))
            .to_owned()
    }

    fn mirror(&self, project: &str) -> PathBuf {
        self.data.join("projects").join(project).join("mirror.git")
    }

    /// Where a run reads a copy of a project.
    fn copy(&self, run: &str, project: &str) -> PathBuf {
        self.data
            .join("runs")
            .join(run)
            .join("context")
            .join(project)
    }

    /// Both projects' cards run a specifier, a builder, an agent only a change that isn't small
    /// gets, and then wait for a person.
    fn short_workflows(&self) {
        for project in [&self.app, &self.valora] {
            let workflow = self
                .engine
                .save_workflow(
                    project,
                    &Workflow {
                        id: String::new(),
                        name: "Short".into(),
                        steps: vec![
                            agent("spec", StepRole::Specifier, None, "{{request}}"),
                            agent_with(
                                "build",
                                ToolProfile::Build,
                                StepRole::Worker,
                                None,
                                "Build it",
                            ),
                            agent(
                                "more",
                                StepRole::Worker,
                                Some(StepCondition::NotSmall),
                                "Look again",
                            ),
                            WorkflowStep::Approval {
                                id: "approval".into(),
                            },
                        ],
                        builtin: false,
                    },
                )
                .unwrap();
            self.engine
                .update_project(
                    project,
                    &keel_store::ProjectSettings {
                        default_workflow: Some(workflow.id),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
    }

    /// Both projects run a specifier and a builder that commits, wait for a person, and open a
    /// pull request from the account `alice`.
    fn delivering_workflows(&self) {
        std::fs::write(self.queue.join("commits"), "").unwrap();
        for project in [&self.app, &self.valora] {
            let workflow = self
                .engine
                .save_workflow(
                    project,
                    &Workflow {
                        id: String::new(),
                        name: "Delivering".into(),
                        steps: vec![
                            agent("spec", StepRole::Specifier, None, "{{request}}"),
                            agent_with(
                                "build",
                                ToolProfile::Build,
                                StepRole::Worker,
                                None,
                                "Build it",
                            ),
                            WorkflowStep::Approval {
                                id: "approval".into(),
                            },
                            WorkflowStep::Deliver { id: "pr".into() },
                        ],
                        builtin: false,
                    },
                )
                .unwrap();
            self.engine
                .update_project(
                    project,
                    &keel_store::ProjectSettings {
                        github_account: Some("alice".into()),
                        default_workflow: Some(workflow.id),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
    }

    fn gh_log(&self) -> String {
        std::fs::read_to_string(self.log.join("gh.log")).unwrap_or_default()
    }

    /// The description of the pull request opened in `acme/<repo>`.
    fn pr_body(&self, repo: &str) -> String {
        std::fs::read_to_string(self.log.join(format!("pr-body-acme-{repo}.md"))).unwrap()
    }

    /// What `gh pr view` says about the pull requests in `acme/<repo>`.
    fn pr_view(&self, repo: &str, view: Value) {
        std::fs::write(
            self.flags.join(format!("pr-view-acme-{repo}")),
            view.to_string(),
        )
        .unwrap();
    }

    fn until(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "{what} never happened");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Where a part stands, as the board shows it.
    fn on_board(&self, project: &str, card: &str) -> PartOf {
        self.engine
            .board(project)
            .unwrap()
            .cards
            .into_iter()
            .find(|shown| shown.id == card)
            .and_then(|shown| shown.part)
            .expect("a part")
    }

    /// What Keel wrote down about the copies an attempt made.
    fn copies_made(&self, attempt: &str) -> Vec<Value> {
        self.store
            .events(attempt, 0)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "readCopies")
            .flat_map(|event| event.data["copies"].as_array().cloned().unwrap_or_default())
            .collect()
    }

    /// Why a run's one decision says it isn't small, as codes.
    fn why_not_small(&self, run: &str) -> Vec<String> {
        let mut found = Vec::new();
        for attempt in self.store.attempts_of_run(run).unwrap() {
            for event in self.store.events(&attempt.id, 0).unwrap() {
                if event.kind == "sizeDecided" {
                    found.extend(
                        event.data["because"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                            .iter()
                            .map(|why| why["code"].as_str().unwrap_or_default().to_owned()),
                    );
                }
            }
        }
        found
    }

    /// A card in `app` whose request touches Valora, started by the board.
    fn agreeing(&self) -> (String, String) {
        let card = self
            .engine
            .card_create(&self.app, "Log in through Valora", "")
            .unwrap();
        self.agree_on(card)
    }

    fn agree_on(&self, card: String) -> (String, String) {
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
    keel_harness::needs!(sandbox);
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
    keel_harness::needs!(sandbox);
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
    keel_harness::needs!(sandbox);
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
    keel_harness::needs!(sandbox);
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
    keel_harness::needs!(sandbox);
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

#[test]
fn agreeing_reads_a_copy_of_each_other_project_and_can_change_none() {
    keel_harness::needs!(sandbox);
    let w = world();
    // A run of Valora's own started from its first commit, and Valora has moved on since.
    w.answers(&[json!({}), both()]);
    let first = git_out(&w.valora_path, &["rev-parse", "HEAD"]);
    let own = w
        .engine
        .start_run(StartRun::one_step(
            w.valora.clone(),
            match agent("look", StepRole::Worker, None, "Look") {
                WorkflowStep::Agent(spec) => spec,
                _ => unreachable!(),
            },
            "en".into(),
        ))
        .unwrap();
    w.wait_for(&own, "look", AttemptState::Succeeded);
    let now = commit(&w.valora_path, "token.ts", "export const token = 1;\n");

    let (_card, run) = w.agreeing();
    w.wait_for(&run, "human", AttemptState::WaitingApproval);
    let copy = w.copy(&run, &w.valora);

    // A copy of Valora as it is now, and none of the project it's in.
    assert_eq!(
        std::fs::read_to_string(copy.join("token.ts")).unwrap(),
        "export const token = 1;\n"
    );
    assert!(!w.copy(&run, &w.app).exists());
    let mirror = w.mirror(&w.valora);
    assert_eq!(
        git_out(&mirror, &["rev-parse", "refs/heads/keel-read"]),
        now
    );
    assert_eq!(
        git_out(&mirror, &["rev-parse", "refs/heads/keel-base"]),
        first,
        "what Valora's own runs start from stays put"
    );

    // The file tools can reach it, and nothing can change it.
    let call = w.call(1);
    let copy_text = copy.to_string_lossy();
    assert!(call.contains(&format!("--add-dir {copy_text}")), "{call}");
    let spec = &w.attempts(&run, "spec")[0];
    let allow = spec.settings["sandbox"]["filesystem"]["allowRead"]
        .as_array()
        .unwrap();
    assert!(allow.contains(&json!(copy_text)));
    let deny = spec.settings["permissions"]["deny"].as_array().unwrap();
    assert!(deny.contains(&json!(format!("Edit(/{copy_text}/**)"))));
    assert!(call.contains(&format!("Edit(/{copy_text}/**)")), "{call}");

    // It's told where, and what it's reading.
    assert!(
        call.contains(&format!(
            "- valora, in `{copy_text}`: its default branch, at commit {}.",
            &now[..7]
        )),
        "{call}"
    );
    let made = w.copies_made(&spec.id);
    assert_eq!(made.len(), 1);
    assert_eq!(made[0]["from"], "folder");
    assert_eq!(made[0]["commit"], now.as_str());
    assert_eq!(made[0]["failed"], Value::Null);
}

#[test]
fn each_part_reads_what_was_agreed_and_the_branch_of_the_part_before() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.short_workflows();
    let mut asks = both();
    asks["questions"] = json!([{
        "id": "Q1",
        "text": "Which channel carries the token?",
        "options": [
            { "label": "A header", "recommended": true },
            { "label": "A cookie", "recommended": false },
        ],
    }]);
    w.answers(&[
        asks,
        both(),
        plain("Valora sends the token"),
        json!({}),
        json!({}),
        json!({}),
        plain("back-adm reads the token"),
        json!({}),
        json!({}),
    ]);
    let issue = Issue {
        number: 434,
        title: "Log in through Valora".into(),
        author: "someone".into(),
        url: "https://github.com/acme/back/issues/434".into(),
        body: "Valora logs in, back-adm lets them in.".into(),
    };
    let card = w.engine.card_import_issue(&w.app, &issue).unwrap();
    let (card, run) = w.agree_on(card);
    let asking = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(
            &asking.id,
            &[Answer {
                question: "Q1".into(),
                choice: AnswerChoice::Option {
                    label: "A cookie".into(),
                },
                remember: false,
            }],
        )
        .unwrap();
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    let parts = w.parts(&card);

    // The first part, in Valora, reads the project where it was agreed as it is.
    w.engine.board_tick().unwrap();
    let first = w.store.card(&parts[0].id).unwrap().run_id.unwrap();
    w.wait_for(&first, "approval", AttemptState::WaitingApproval);
    let first_run = w.store.run(&first).unwrap();
    assert_eq!(first_run.title, "Log in through Valora", "no #434 here");
    let call = w.call(2);
    assert!(
        call.starts_with(
            "\nThis is one part of a change across several projects. In this project: Sends the token\n"
        ),
        "{call}"
    );
    assert!(call.contains("hasn't been verified"), "still data: {call}");
    for told in [
        "This run is part 1 of 2 of a change that touches several projects",
        "this one is for valora. Do only this part.",
        "Log in from Valora into back-adm",
        "1. valora: Sends the token",
        "The /session route keeps its name",
        "Which channel carries the token? A cookie",
        "Don't say any of it is tested or works",
    ] {
        assert!(call.contains(told), "{told}: {call}");
    }
    assert!(
        call.contains(&format!(
            "- app, in `{}`: its default branch",
            w.copy(&first, &w.app).display()
        )),
        "{call}"
    );
    // However small it looks, it's never treated as small.
    assert!(
        w.why_not_small(&first)
            .contains(&"partOfJointChange".to_owned()),
        "{:?}",
        w.why_not_small(&first)
    );
    // Valora's own #434 is a different issue, and an assistant can still start it there.
    let own = w.engine.start_run_on_board(StartRun {
        project_id: w.valora.clone(),
        title: None,
        request: "Do it".into(),
        saved_workflow: None,
        workflow: vec![agent("work", StepRole::Worker, None, "{{request}}")],
        language: "en".into(),
        source_issue: Some(Issue {
            url: "https://github.com/acme/valora/issues/434".into(),
            ..issue.clone()
        }),
        started_by: Some("client".into()),
    });
    let own = own.unwrap();
    w.wait_for(&own, "work", AttemptState::Succeeded);

    // Valora's part is delivered: its branch reaches the mirror, and it has a pull request.
    let workspace = w.data.join("runs").join(&first).join("workspace");
    let delivered = commit(&workspace, "send.ts", "send(token);\n");
    let branch = first_run.branch.unwrap();
    git(
        &w.mirror(&w.valora),
        &[
            "fetch",
            "-q",
            &workspace.to_string_lossy(),
            &format!("+refs/heads/{branch}:refs/heads/{branch}"),
        ],
    );
    w.store
        .patch_run(
            &first,
            &[(
                "pr_url",
                Column::Text("https://github.com/acme/valora/pull/7".into()),
            )],
        )
        .unwrap();

    // The second reads Valora with that change in it.
    w.engine.board_tick().unwrap();
    let second = w.store.card(&parts[1].id).unwrap().run_id.unwrap();
    w.wait_for(&second, "approval", AttemptState::WaitingApproval);
    let copy = w.copy(&second, &w.valora);
    assert_eq!(
        std::fs::read_to_string(copy.join("send.ts")).unwrap(),
        "send(token);\n"
    );
    let call = w.call(6);
    assert!(call.contains("This run is part 2 of 2"), "{call}");
    assert!(call.contains("this one is for app."), "{call}");
    assert!(
        call.contains(&format!(
            "- valora, in `{}`: the branch of part 1, at commit {}.",
            copy.display(),
            &delivered[..7]
        )),
        "{call}"
    );
    let basis = w.store.card(&parts[1].id).unwrap().basis;
    assert_eq!(basis.get(&first), Some(&delivered));
    // With no GitHub account, what Keel delivered is the newest it can have, and it says so.
    let spec = &w.attempts(&second, "spec")[0];
    let made = w.copies_made(&spec.id);
    assert_eq!(made[0]["from"], "earlier");
    assert!(
        made[0]["failed"]
            .as_str()
            .unwrap()
            .contains("no GitHub account"),
        "{made:?}"
    );
    // A step after it reads the same copy, and nothing makes it again.
    let more = &w.attempts(&second, "more")[0];
    assert!(w.copies_made(&more.id).is_empty());
    assert!(w.call(8).contains(&format!("--add-dir {}", copy.display())));
}

#[test]
fn each_part_reads_the_whole_request_and_not_only_its_own_line() {
    keel_harness::needs!(sandbox);
    // Only the request says what the field is called: what was agreed sums it up without it.
    let asked = "Log in through Valora\n\nThe token travels in the field `x_valora_token`.";
    for from_a_card in [true, false] {
        let w = world();
        let mut steps = full_workflow("en", "fake", "fake", None);
        steps.retain(|step| !matches!(step, WorkflowStep::Deliver { .. }));
        for project in [&w.app, &w.valora] {
            let workflow = w
                .engine
                .save_workflow(
                    project,
                    &Workflow {
                        id: String::new(),
                        name: "Default".into(),
                        steps: steps.clone(),
                        builtin: false,
                    },
                )
                .unwrap();
            w.engine
                .update_project(
                    project,
                    &keel_store::ProjectSettings {
                        default_workflow: Some(workflow.id),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        w.answers(&[
            both(),
            plain("Valora sends the token"),
            json!({}),
            plain("back-adm reads the token"),
            json!({}),
        ]);
        let (card, run) = if from_a_card {
            let (title, body) = asked.split_once("\n\n").unwrap();
            let card = w.engine.card_create(&w.app, title, body).unwrap();
            w.agree_on(card)
        } else {
            let run = w
                .engine
                .propose_change_on_board(
                    StartRun {
                        request: asked.into(),
                        ..proposal(&w, None)
                    },
                    std::slice::from_ref(&w.valora),
                )
                .unwrap();
            (w.store.card_of_run(&run).unwrap().unwrap(), run)
        };
        let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
        w.engine.answer_approval(&approval.id, true, "").unwrap();
        let parts = w.parts(&card);

        w.engine.board_tick().unwrap();
        let first = w.store.card(&parts[0].id).unwrap().run_id.unwrap();
        w.wait_for(&first, "approval", AttemptState::WaitingApproval);
        w.engine.card_stop_waiting(&parts[1].id).unwrap();
        w.engine.board_tick().unwrap();
        let second = w.store.card(&parts[1].id).unwrap().run_id.unwrap();
        w.wait_for(&second, "approval", AttemptState::WaitingApproval);

        // Valora's specifier and builder, then app's.
        for (call, line) in [
            (1, "Sends the token"),
            (2, "Sends the token"),
            (3, "Reads the token"),
            (4, "Reads the token"),
        ] {
            let call = w.call(call);
            assert!(
                call.contains(&format!("In this project: {line}")),
                "{from_a_card}: {call}"
            );
            assert!(call.contains("`x_valora_token`"), "{from_a_card}: {call}");
        }
        assert!(w.call(2).contains("Original request:"), "the builder's own");
        assert!(w.call(4).contains("Original request:"), "the builder's own");
    }
}

#[test]
fn a_project_that_cannot_be_read_holds_only_that_step_until_it_can() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[both(), both()]);
    let away = w.valora_path.with_file_name("valora-away");
    std::fs::rename(&w.valora_path, &away).unwrap();

    // Never read before, and its folder is gone: nothing to read from.
    let (card, run) = w.agreeing();
    let held = w.wait_for(&run, "spec", AttemptState::Blocked);
    match reason_of(&held) {
        Reason::ReadCopyFailed { project, .. } => assert_eq!(project, "valora"),
        other => panic!("{other:?}"),
    }
    assert!(w.calls().is_empty(), "no agent started without it");
    assert!(
        !Reason::ReadCopyFailed {
            project: String::new(),
            detail: String::new()
        }
        .stops_every_card(0)
    );

    // Back where it was, the person tries again.
    std::fs::rename(&away, &w.valora_path).unwrap();
    w.engine.retry(&held.id, false).unwrap();
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    w.engine
        .answer_approval(&approval.id, false, "Once more")
        .unwrap();

    // Gone again, the next agreement reads what Keel read of it before, and says so.
    std::fs::rename(&w.valora_path, &away).unwrap();
    let (_, again) = w.agree_on(card);
    assert_ne!(again, run);
    w.wait_for(&again, "human", AttemptState::WaitingApproval);
    let spec = &w.attempts(&again, "spec")[0];
    let made = w.copies_made(&spec.id);
    assert_eq!(made[0]["from"], "earlier");
    assert_eq!(made[0]["branch"], "keel-read");
    assert!(made[0]["failed"].is_string(), "{made:?}");
    assert!(w.copy(&again, &w.valora).join("README.md").exists());
}

#[test]
fn each_part_opens_its_own_pull_request_saying_which_part_it_is() {
    keel_harness::needs!(sandbox);
    let w = world_on_github();
    w.delivering_workflows();
    w.answers(&[
        both(),
        plain("Valora sends the token"),
        json!({}),
        plain("back-adm reads the token"),
        json!({}),
    ]);
    let issue = Issue {
        number: 434,
        title: "Log in through Valora".into(),
        author: "someone".into(),
        url: "https://github.com/acme/app/issues/434".into(),
        body: "Valora logs in, back-adm lets them in.".into(),
    };
    let card = w.engine.card_import_issue(&w.app, &issue).unwrap();
    let (card, run) = w.agree_on(card);
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    let parts = w.parts(&card);
    let agreeing = w.engine.board(&w.app).unwrap().cards;
    let agreeing = agreeing.iter().find(|shown| shown.id == card).unwrap();
    assert_eq!(agreeing.also_touches, std::slice::from_ref(&w.valora));
    assert_eq!(agreeing.joint_change_id.as_deref(), Some(card.as_str()));
    assert!(agreeing.part.is_none());
    assert_eq!(
        w.on_board(&w.app, &parts[1].id),
        PartOf {
            change_id: card.clone(),
            position: 2,
            total: 2,
            waits_for: Some(Neighbour {
                card_id: parts[0].id.clone(),
                position: Some(1),
                project: "valora".into(),
            }),
            warnings: vec![],
        }
    );

    // The first part is in Valora, and the issue is in another repo.
    w.engine.board_tick().unwrap();
    let first = w.store.card(&parts[0].id).unwrap().run_id.unwrap();
    let approval = w.wait_for(&first, "approval", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    w.wait_for(&first, "pr", AttemptState::Succeeded);
    let body = w.pr_body("valora");
    for says in [
        "### Part 1 of 2 of a change across several projects\n\nRefs acme/app#434\n",
        "- Part 1, **valora**: this pull request\n- Part 2, **app**: opens after this one\n",
        "**To try together.**",
        "- [ ] Log in on Valora and land in back-adm",
    ] {
        assert!(body.contains(says), "{says}: {body}");
    }
    assert!(!body.contains("Refs #434"), "{body}");
    assert!(!body.contains("Read, and not changed"), "{body}");
    let first_run = w.store.run(&first).unwrap();
    assert_eq!(
        first_run.pr_url.as_deref(),
        Some("https://github.com/acme/valora/pull/7")
    );
    let workspace = w.data.join("runs").join(&first).join("workspace");
    let pushed = git_out(&workspace, &["rev-parse", "HEAD"]);
    assert_eq!(first_run.pr_head.as_deref(), Some(pushed.as_str()));

    // The second reads that branch on GitHub. When it waits for a person, Keel looks at the first
    // part's pull request again, however recently it looked in the background.
    w.store
        .patch_run(&first, &[("pr_checked_at", Column::Null)])
        .unwrap();
    w.engine.board_tick().unwrap();
    let second = w.store.card(&parts[1].id).unwrap().run_id.unwrap();
    let approval = w.wait_for(&second, "approval", AttemptState::WaitingApproval);
    let spec = &w.attempts(&second, "spec")[0];
    assert_eq!(w.copies_made(&spec.id)[0]["from"], "github");
    assert_eq!(
        w.store.card(&parts[1].id).unwrap().basis.get(&first),
        Some(&pushed)
    );
    w.until("looking at the first part", || {
        w.gh_log().contains("gh pr view 7 --repo acme/valora")
    });
    let part = w.on_board(&w.app, &parts[1].id);
    assert_eq!((part.waits_for, part.warnings), (None, vec![]));

    // Someone pushes to the first part's pull request, and then closes it.
    let later = "0123456789abcdef0123456789abcdef01234567";
    w.pr_view(
        "valora",
        json!({ "state": "OPEN", "isDraft": true, "updatedAt": "2026-11-10T10:00:00Z", "headRefOid": later }),
    );
    w.store
        .patch_run(&first, &[("pr_checked_at", Column::Null)])
        .unwrap();
    w.engine.look_at_pr(&second);
    w.until("the new commit", || {
        w.store.run(&first).unwrap().pr_head.as_deref() == Some(later)
    });
    let changed = PartWarning::Changed {
        position: 1,
        project: "valora".into(),
    };
    assert_eq!(w.on_board(&w.app, &parts[1].id).warnings, [changed]);
    w.pr_view(
        "valora",
        json!({ "state": "CLOSED", "isDraft": true, "closedAt": "2026-11-10T11:00:00Z", "headRefOid": later }),
    );
    w.store
        .patch_run(&first, &[("pr_checked_at", Column::Null)])
        .unwrap();
    w.engine.look_at_pr(&second);
    w.until("the pull request closing", || {
        w.store.run(&first).unwrap().pr_state == Some(PrState::Closed)
    });
    let closed = PartWarning::Closed {
        position: 1,
        project: "valora".into(),
    };
    assert_eq!(w.on_board(&w.app, &parts[1].id).warnings, [closed]);

    // The person approves it anyway. The issue is in this repo, and the first part is linked.
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    w.wait_for(&second, "pr", AttemptState::Succeeded);
    let body = w.pr_body("app");
    for says in [
        "### Part 2 of 2 of a change across several projects\n\nRefs #434\n",
        "- Part 1, **valora**: acme/valora#7\n- Part 2, **app**: this pull request\n",
    ] {
        assert!(body.contains(says), "{says}: {body}");
    }

    // After opening them, Keel wrote nothing to either pull request, nor to the issue.
    let log = w.gh_log();
    let writes: Vec<&str> = log
        .lines()
        .filter(|line| !line.starts_with("gh pr view") && !line.starts_with("gh auth token"))
        .filter(|line| !line.starts_with("gh repo view") && !line.starts_with("gh api repos/"))
        .collect();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert!(
        writes.iter().all(|line| line.starts_with("gh pr create")),
        "{writes:?}"
    );
}

#[test]
fn a_person_sees_the_whole_change_and_only_they_say_what_works_together() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[both()]);
    let (card, run) = w.agreeing();

    // Before it's agreed, there are only the projects.
    let before = w.engine.joint_change(&card).unwrap();
    assert_eq!(
        before.agreed_in.as_ref().map(|c| c.card_id.as_str()),
        Some(card.as_str())
    );
    let names: Vec<&str> = before.projects.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["app", "valora"]);
    assert!(before.agreed.is_none() && before.parts.is_empty() && before.read_only.is_empty());
    let joint = w.engine.run_joint(&run).unwrap().unwrap();
    assert_eq!(
        (joint.change_id.as_str(), joint.agrees, joint.part),
        (card.as_str(), true, None)
    );

    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    let change = w.engine.joint_change(&card).unwrap();
    assert_eq!(
        change.agreed.as_ref().unwrap()["summary"],
        "Log in from Valora into back-adm"
    );
    let parts: Vec<(&str, Option<i64>)> = change
        .parts
        .iter()
        .map(|part| {
            (
                part.project.as_str(),
                part.part.as_ref().map(|p| p.position),
            )
        })
        .collect();
    assert_eq!(parts, [("valora", Some(1)), ("app", Some(2))]);
    assert_eq!(change.parts[0].column, BoardColumn::Todo);
    assert!(change.parts[1].part.as_ref().unwrap().waits_for.is_some());
    assert!(change.read_only.is_empty(), "both change");
    assert_eq!(change.checks.len(), 1);

    // Only a person says it works with every part in place, and can take it back.
    let seq = change.checks[0].seq;
    w.engine.joint_check_set(&card, seq, true).unwrap();
    assert!(
        w.engine.joint_change(&card).unwrap().checks[0]
            .confirmed_at
            .is_some()
    );
    w.engine.joint_check_set(&card, seq, false).unwrap();
    assert!(
        w.engine.joint_change(&card).unwrap().checks[0]
            .confirmed_at
            .is_none()
    );
    assert!(matches!(
        w.engine.joint_check_set(&card, seq + 10, true),
        Err(Error::InvalidState)
    ));

    // The second part can be told not to wait, once.
    let second = &change.parts[1].card_id;
    w.engine.card_stop_waiting(second).unwrap();
    assert!(w.on_board(&w.app, second).waits_for.is_none());
    assert!(!w.store.card(second).unwrap().waiting);
    assert!(matches!(
        w.engine.card_stop_waiting(second),
        Err(Error::InvalidState)
    ));
}

fn proposal(w: &World, issue: Option<Issue>) -> StartRun {
    StartRun {
        project_id: w.app.clone(),
        title: issue.as_ref().map(|issue| issue.title.clone()),
        request: "Log in through Valora".into(),
        // Whatever it comes with is ignored: an agreement is always the same.
        saved_workflow: None,
        workflow: Vec::new(),
        language: "en".into(),
        source_issue: issue,
        started_by: Some("client".into()),
    }
}

#[test]
fn a_change_proposed_from_outside_the_board_is_agreed_like_one_started_from_a_card() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[both()]);
    // The project's own and repeats are left out, as on the board.
    let run = w
        .engine
        .propose_change_on_board(
            proposal(&w, None),
            &[w.valora.clone(), w.app.clone(), w.valora.clone()],
        )
        .unwrap();
    let card = w.store.card_of_run(&run).unwrap().unwrap();
    let stored = w.store.card(&card).unwrap();
    assert_eq!(stored.project_id, w.app);
    assert_eq!(stored.joint_change_id.as_deref(), Some(card.as_str()));
    assert_eq!(
        w.store.joint_change(&card).unwrap().projects,
        std::slice::from_ref(&w.valora)
    );
    assert_eq!(
        w.store.run(&run).unwrap().started_by.as_deref(),
        Some("client")
    );
    let spec = &w.attempts(&run, "spec")[0];
    assert_eq!(spec.tools, "readOnly");
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    assert!(
        w.parts(&card).is_empty(),
        "nothing opens before it's approved"
    );

    w.engine.answer_approval(&approval.id, true, "").unwrap();
    let parts = w.parts(&card);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].project_id, w.valora);
}

#[test]
fn a_change_that_names_no_other_project_or_one_that_is_missing_creates_nothing() {
    let w = world();
    let own = std::slice::from_ref(&w.app);
    assert!(matches!(
        w.engine.propose_change_on_board(proposal(&w, None), own),
        Err(Error::InvalidState)
    ));
    assert!(
        w.engine
            .propose_change_on_board(proposal(&w, None), &["missing".into()])
            .is_err()
    );
    assert!(w.store.cards(&w.app).unwrap().is_empty());
    assert!(w.store.runs(10).unwrap().is_empty());
    assert!(w.calls().is_empty());
}

#[test]
fn an_issue_that_is_already_a_change_across_projects_is_not_proposed_again() {
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
    let proposed = w
        .engine
        .propose_change_on_board(proposal(&w, Some(issue)), std::slice::from_ref(&w.valora));
    assert!(
        matches!(proposed, Err(Error::Blocked(Reason::StartsOnTheBoard))),
        "{proposed:?}"
    );
    assert_eq!(w.store.cards(&w.app).unwrap().len(), 1, "no second card");
    assert_eq!(w.store.card(&card).unwrap().column(), BoardColumn::Backlog);
    assert!(w.calls().is_empty());
}
