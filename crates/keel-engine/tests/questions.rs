//! What a specifier answers, the questions it can ask, and a person sending work back, against a
//! fake `claude`. Whether the real one keeps a structured answer on `--resume` isn't proved here.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{
    AttemptState, BoardColumn, CardLane, Reason, SpecProblem, StepRole, StepSpec, ToolProfile,
    WorkflowStep,
};
use keel_engine::{
    Answer, AnswerChoice, Config, Engine, Error, StartRun, Workflow, request_from_issue,
};
use keel_harness::Tools;
use keel_harness::github::Issue;
use keel_store::rusqlite_value::Value as Column;
use keel_store::{Attempt, ProjectSettings, Store};
use serde_json::{Value, json};

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
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

/// A specifier takes the next answer from a numbered file, and says `fail` to fail. With none left
/// it has nothing to ask. Each answer reports what the session has cost in total, as a resumed
/// session does. Approvers take their decisions from a file, and approve when it runs out.
fn fake(log: &Path, queue: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true}}'; exit 0;;
esac
prompt="$(cat)"
tools=""; schema=""; resume=""; session=""; system=""; prev=""
for a in "$@"; do
  [ "$prev" = "--tools" ] && tools="$a"
  [ "$prev" = "--json-schema" ] && schema="$a"
  [ "$prev" = "--resume" ] && resume="$a"
  [ "$prev" = "--session-id" ] && session="$a"
  [ "$prev" = "--append-system-prompt-file" ] && system="$a"
  prev="$a"
done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
[ -n "$schema" ] && list="$list\",\"StructuredOutput"
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
{{ echo "=== CALL resume=$resume session=$session"; echo "$prompt"; echo "--- system"; cat "$system"; echo "--- end"; }} >> "{log}/calls.log"
result() {{
  printf '%s\n' "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"$2\",\"num_turns\":1,\"total_cost_usd\":$3,\"structured_output\":$1}}"
}}
case "$schema" in
*criteria*)
  n=$(cat "{queue}/count" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "{queue}/count"
  if [ "$(cat "{queue}/$n.json" 2>/dev/null)" = "fail" ]; then
    echo '{{"type":"result","subtype":"error_during_execution","is_error":true,"result":"it broke","num_turns":1}}'
    exit 1
  fi
  if [ -f "{queue}/$n.json" ]; then
    output="$(cat "{queue}/$n.json")"
  else
    output='{{"summary":"It gets fixed","criteria":[{{"id":"C1","when":"it runs","then":"it works"}}],"changes":[],"limits":[],"decided":[],"questions":[]}}'
  fi
  result "$output" "" "0.$n"
  exit 0;;
*verdict*)
  result '{{"verdict":"approved","findings":[],"summary":"Fine","unread":[],"unchecked":[]}}' "" 0
  exit 0;;
*decision*)
  decision=approve
  if [ -s "{queue}/decisions" ]; then decision=$(head -1 "{queue}/decisions"); sed -i '' 1d "{queue}/decisions"; fi
  result "{{\"decision\":\"$decision\",\"risk\":\"low\",\"reasons\":\"because\",\"instructions\":\"FROM-THE-APPROVER\"}}" "" 0
  exit 0;;
*learnings*)
  result '{{"learnings":[]}}' "" 0
  exit 0;;
esac
if [ "$tools" = "Read,Grep,Glob" ]; then
  printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"PLAIN-SPEC-TEXT","num_turns":1}}'
  exit 0
fi
echo built >> built.txt
git add -A && git commit -qm "build"
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"Built it.","num_turns":1}}'
"##,
        log = log.display(),
        queue = queue.display(),
    )
}

fn world() -> World {
    world_with(|_| {})
}

fn world_with(tune: impl FnOnce(&mut Config)) -> World {
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
    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "x\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    config.max_agents = 5;
    tune(&mut config);
    let engine = Engine::new(store.clone(), config, tools_in(&bin), |_| {});
    let project_id = engine.add_project(&repo).unwrap().id;
    World {
        dir,
        store,
        engine,
        project_id,
        log,
        queue,
    }
}

fn tools_in(bin: &Path) -> Tools {
    Tools::from_dirs(&[
        bin.to_path_buf(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
}

fn agent(id: &str, tools: ToolProfile, role: StepRole, prompt: &str) -> WorkflowStep {
    WorkflowStep::Agent(StepSpec {
        id: id.into(),
        model: "fake".into(),
        effort: None,
        tools,
        prompt: prompt.into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        role,
    })
}

fn specifier() -> WorkflowStep {
    agent(
        "spec",
        ToolProfile::ReadOnly,
        StepRole::Specifier,
        "SPEC {{request}}\n{{answers}}",
    )
}

fn builder() -> WorkflowStep {
    agent(
        "builder",
        ToolProfile::Build,
        StepRole::Worker,
        "BUILD spec={{steps.spec.output}}\n{{answers}}\n{{rework}}",
    )
}

fn check() -> WorkflowStep {
    WorkflowStep::Check {
        id: "check".into(),
        run: "test -f built.txt".into(),
    }
}

fn human() -> WorkflowStep {
    WorkflowStep::Approval { id: "human".into() }
}

fn approver(max_rounds: u32) -> WorkflowStep {
    agent(
        "approver",
        ToolProfile::ReadOnly,
        StepRole::Approver {
            rework_to: "builder".into(),
            max_rounds,
        },
        "APPROVE\n{{rework}}",
    )
}

fn no_approver() -> Vec<WorkflowStep> {
    vec![specifier(), builder(), check(), human()]
}

fn with_approver(max_rounds: u32) -> Vec<WorkflowStep> {
    vec![
        specifier(),
        builder(),
        check(),
        approver(max_rounds),
        human(),
    ]
}

fn good() -> Value {
    json!({
        "summary": "add returns the sum",
        "criteria": [{ "id": "C1", "when": "add(1, 2)", "then": "it returns 3" }],
        "changes": [{ "kind": "changes", "text": "add stops subtracting" }],
        "limits": ["Its signature stays the same"],
        "decided": [{ "text": "Keeps the rounding as it is" }],
        "questions": [],
    })
}

fn asking(id: &str, text: &str) -> Value {
    let mut output = good();
    output["questions"] = json!([{
        "id": id,
        "text": text,
        "options": [
            { "label": "Show an error", "recommended": true },
            { "label": "Return 0", "recommended": false },
        ],
    }]);
    output
}

/// A question only the person can answer: nothing says which option it should be.
fn only_they_know(id: &str, text: &str) -> Value {
    let mut output = good();
    output["questions"] = json!([{
        "id": id,
        "text": text,
        "options": [
            { "label": "5 %", "recommended": false },
            { "label": "10 %", "recommended": false },
        ],
    }]);
    output
}

fn too_long() -> Value {
    let mut output = good();
    output["summary"] = json!("x".repeat(201));
    output
}

fn option(question: &str, label: &str) -> Answer {
    Answer {
        question: question.into(),
        choice: AnswerChoice::Option {
            label: label.into(),
        },
        remember: false,
    }
}

fn own(question: &str, text: &str) -> Answer {
    Answer {
        question: question.into(),
        choice: AnswerChoice::Own { text: text.into() },
        remember: false,
    }
}

fn remembered(mut answer: Answer) -> Answer {
    answer.remember = true;
    answer
}

impl World {
    fn start(&self, steps: Vec<WorkflowStep>) -> String {
        self.start_asking("THE-REQUEST", steps)
    }

    fn start_asking(&self, request: &str, steps: Vec<WorkflowStep>) -> String {
        self.start_from(request, None, None, steps)
    }

    fn start_from(
        &self,
        request: &str,
        issue: Option<Issue>,
        started_by: Option<&str>,
        steps: Vec<WorkflowStep>,
    ) -> String {
        self.engine
            .start_run(StartRun {
                project_id: self.project_id.clone(),
                title: None,
                request: request.into(),
                workflow: steps,
                language: "en".into(),
                source_issue: issue,
                saved_workflow: None,
                started_by: started_by.map(str::to_owned),
            })
            .unwrap()
    }

    /// What the specifier answers, in order.
    fn answers(&self, outputs: &[Value]) {
        for (index, output) in outputs.iter().enumerate() {
            let text = match output {
                Value::String(word) => word.clone(),
                other => other.to_string(),
            };
            std::fs::write(self.queue.join(format!("{}.json", index + 1)), text).unwrap();
        }
    }

    /// What the run's spec says was decided, as Keel kept it.
    fn decided(&self, run: &str) -> Value {
        self.step(run, "spec").output.unwrap()["decided"].clone()
    }

    /// Sets the project's GitHub account.
    fn account(&self, account: &str) {
        self.engine
            .update_project(
                &self.project_id,
                &ProjectSettings {
                    github_account: Some(account.into()),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    fn decisions(&self, lines: &[&str]) {
        std::fs::write(self.queue.join("decisions"), lines.join("\n") + "\n").unwrap();
    }

    fn all(&self, run: &str, id: &str) -> Vec<Attempt> {
        self.store
            .attempts_of_run(run)
            .unwrap()
            .into_iter()
            .filter(|a| a.step_id == id)
            .collect()
    }

    fn step(&self, run: &str, id: &str) -> Attempt {
        self.all(run, id).pop().unwrap()
    }

    fn wait_for(&self, run: &str, id: &str, state: AttemptState) -> Attempt {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(found) = self.all(run, id).pop()
                && found.state == state.as_str()
            {
                return found;
            }
            let everything: Vec<_> = self
                .store
                .attempts_of_run(run)
                .unwrap()
                .into_iter()
                .map(|a| (a.step_id, a.state, a.round, a.reason))
                .collect();
            assert!(
                Instant::now() < deadline,
                "{id} never reached {state:?}: {everything:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_attempts(&self, run: &str, id: &str, count: usize, state: AttemptState) -> Attempt {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let found = self.all(run, id);
            if found.len() == count && found[count - 1].state == state.as_str() {
                return found[count - 1].clone();
            }
            assert!(
                Instant::now() < deadline,
                "{id} never reached {count} attempts ending {state:?}: {:?}",
                found
                    .iter()
                    .map(|a| (a.state.clone(), a.reason.clone()))
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.log.join("calls.log")).unwrap_or_default()
    }

    fn call_count(&self) -> usize {
        self.calls().matches("=== CALL").count()
    }

    fn kinds(&self, attempt: &str) -> Vec<String> {
        self.store
            .events(attempt, 0)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }

    fn event(&self, attempt: &str, kind: &str) -> Value {
        self.store
            .events(attempt, 0)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} event"))
            .data
    }
}

fn reason_of(attempt: &Attempt) -> Reason {
    serde_json::from_value(attempt.reason.clone().expect("has a reason")).unwrap()
}

fn invalid(problem: &str, question: &str) -> Reason {
    Reason::InvalidAnswer {
        problem: problem.into(),
        question: question.into(),
    }
}

fn blocked_with(result: Result<(), Error>) -> Reason {
    match result {
        Err(Error::Blocked(reason)) => reason,
        other => panic!("{other:?}"),
    }
}

#[test]
fn with_nothing_to_ask_it_goes_straight_on() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[good()]);
    let run = w.start(no_approver());
    w.wait_for(&run, "human", AttemptState::WaitingApproval);

    let spec = w.step(&run, "spec");
    assert_eq!(spec.state, "succeeded");
    assert_eq!(spec.role, "specifier");
    assert_eq!(w.all(&run, "spec").len(), 1);
    assert!(!w.kinds(&spec.id).contains(&"questionsAsked".to_owned()));
    let built = w.step(&run, "builder").prompt;
    assert!(built.contains("add returns the sum"), "{built}");
    // No answers, no words about answers.
    assert!(!built.contains("already answered"), "{built}");
}

#[test]
fn questions_wait_and_hold_no_agent_slot() {
    keel_harness::needs!(sandbox);
    let w = world_with(|config| config.max_agents = 1);
    w.answers(&[asking("Q1", "If `whole` is 0, what should happen?")]);
    let run = w.start(no_approver());
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);

    assert!(w.all(&run, "builder").is_empty());
    assert_eq!(w.engine.active_count().unwrap(), 0);
    assert_eq!(w.store.working_runs().unwrap(), 0);
    assert_eq!(w.store.totals().unwrap().blocked, 1, "it's waiting for you");
    assert_eq!(w.store.runs(10).unwrap()[0].state, "waitingAnswers");
    assert!(w.store.run(&run).unwrap().finished_at.is_none());
    assert!(waiting.finished_at.is_some(), "its process is over");
    let asked = w.event(&waiting.id, "questionsAsked");
    assert_eq!(asked["questions"][0]["id"], "Q1");

    // The only slot there is goes to another run.
    let other = w.start(vec![agent(
        "other",
        ToolProfile::ReadOnly,
        StepRole::Worker,
        "ANYTHING",
    )]);
    w.wait_for(&other, "other", AttemptState::Succeeded);
}

#[test]
fn using_the_recommended_options_runs_no_agent_and_keeps_what_it_wrote() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[asking("Q1", "If `whole` is 0, what should happen?")]);
    let run = w.start(no_approver());
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    let calls = w.call_count();

    w.engine
        .answer_questions(&waiting.id, &[option("Q1", "Show an error")])
        .unwrap();

    // It moves on at once, with the same attempt.
    w.wait_for(&run, "builder", AttemptState::Succeeded);
    assert_eq!(w.all(&run, "spec").len(), 1, "no agent was asked again");
    assert_eq!(w.call_count(), calls + 1, "only the builder");
    assert!(
        w.calls()
            .lines()
            .filter(|line| line.starts_with("=== CALL"))
            .all(|line| line.starts_with("=== CALL resume= ")),
        "nothing resumed: {}",
        w.calls()
    );

    let settled = w.step(&run, "spec");
    assert_eq!(settled.state, "succeeded");
    let output = settled.output.unwrap();
    assert_eq!(output["questions"], json!([]));
    assert_eq!(output["criteria"][0]["id"], "C1", "the criteria stay");
    assert_eq!(
        output["decided"][1]["text"],
        "If `whole` is 0, what should happen? Show an error"
    );
    assert_eq!(output["decided"][1]["taken"], "person");
    let answers = w.event(&settled.id, "answers");
    assert_eq!(
        (answers["total"].as_i64(), answers["recommended"].as_i64()),
        (Some(1), Some(1))
    );
    // What's built knows.
    let built = w.step(&run, "builder").prompt;
    assert!(
        built.contains("- If `whole` is 0, what should happen? Show an error"),
        "{built}"
    );
}

/// The prompt a built-in reviewer gets, as the step of a worker that reads only.
fn built_in_review(w: &World) -> WorkflowStep {
    let full = w
        .engine
        .workflows(&w.project_id, "en")
        .unwrap()
        .into_iter()
        .find(|workflow| workflow.id == "builtin:full")
        .unwrap();
    let prompt = full
        .steps
        .iter()
        .find_map(|step| match step {
            WorkflowStep::Parallel { steps, .. } => steps
                .iter()
                .find(|spec| spec.role == StepRole::Reviewer)
                .map(|spec| spec.prompt.clone()),
            _ => None,
        })
        .unwrap();
    agent("review", ToolProfile::ReadOnly, StepRole::Worker, &prompt)
}

#[test]
fn what_the_request_already_decided_reaches_the_reviewers_as_the_persons() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[good()]);
    let review = built_in_review(&w);
    let run = w.start_asking(
        // As in Markdown, a list from 6 starts after a blank line, not right under a line of text.
        "Count the threads that got fixed.\n\nDecisions already made (don't ask them):\n\n6. If the author resolves their own\n   critical thread, it counts for now.",
        vec![specifier(), builder(), check(), review, human()],
    );
    w.wait_for(&run, "human", AttemptState::WaitingApproval);

    assert_eq!(
        w.decided(&run),
        json!([
            { "text": "If the author resolves their own critical thread, it counts for now.", "taken": "request" },
            { "text": "Keeps the rounding as it is" },
        ])
    );
    let reviewed = w.step(&run, "review").prompt;
    assert!(reviewed.contains(r#""taken":"request""#), "{reviewed}");
    assert!(
        reviewed.contains("Lines of `decided` whose `taken` is `request` were already decided"),
        "{reviewed}"
    );
}

#[test]
fn what_an_assistant_sends_back_against_the_person_s_decision_does_not_win_over_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    let review = built_in_review(&w);
    let run = w.start_asking(
        "Export the threads.\n\nDecided:\n- The admin export needs no login.",
        vec![specifier(), builder(), check(), review, human()],
    );
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    w.engine
        .send_back_by(&approval.id, "Add a login check.", "Claude Code")
        .unwrap();
    let reviewed = w
        .wait_for_attempts(&run, "review", 2, AttemptState::Succeeded)
        .prompt;

    // The reviewer sees the person's decision, that an assistant sent this back and not the
    // person, and that only what the person sends back changes what they decided.
    assert!(
        reviewed.contains(
            r#"{"number":1,"taken":"request","text":"The admin export needs no login."}"#
        ),
        "{reviewed}"
    );
    assert!(
        reviewed.contains(
            "Claude Code, an assistant the person allowed, sent this work back. This is the only thing to change in this round, and it counts as part of the scope:\nAdd a login check."
        ),
        "{reviewed}"
    );
    assert!(
        reviewed.contains("Only what a person sent back can change one of these, and then it wins. Where what the approver or an assistant sent back goes against one, the decision stands: report the conflict, for the person to decide."),
        "{reviewed}"
    );
}

/// What a reviewer's prompt says in a workflow saved before Keel read what the request decided: it
/// only keeps what a person chose.
const SAVED_REVIEW: &str = "You're reviewing a change you didn't write. Don't question what a person chose: a decision whose `taken` is `person`.\n{{steps.spec.output}}\n{{rework}}\n{{diff}}";

#[test]
fn every_reviewer_and_approver_keeps_to_what_the_request_decided_whatever_its_prompt_says() {
    keel_harness::needs!(sandbox);
    let w = world();
    let WorkflowStep::Agent(built_in) = built_in_review(&w) else {
        panic!()
    };
    let steps = || {
        vec![
            specifier(),
            builder(),
            check(),
            agent(
                "review",
                ToolProfile::ReadOnly,
                StepRole::Reviewer,
                SAVED_REVIEW,
            ),
            agent(
                "built-in",
                ToolProfile::ReadOnly,
                StepRole::Reviewer,
                &built_in.prompt,
            ),
            approver(2),
            human(),
        ]
    };
    let run = w.start_asking(
        "Export the threads.\n\nDecided:\n- The admin export needs no login.",
        steps(),
    );
    let plain = w.start(steps());
    w.wait_for(&run, "human", AttemptState::WaitingApproval);
    w.wait_for(&plain, "human", AttemptState::WaitingApproval);

    let reviewed = w.step(&run, "review").prompt;
    assert!(
        reviewed.ends_with("\n\nLines of `decided` whose `taken` is `request` were already decided in the request. One without an `assistant` or a `githubAccount` is the person's: don't question it, nor one of the specification's that says the same, though you can say the change doesn't follow it. One with either may not be the person's own words: report it, or one of the specification's that says the same, only if it puts security or data at risk. Only what a person sent back can change one of these, and then it wins. Where what the approver or an assistant sent back goes against one, the decision stands: report the conflict, for the person to decide."),
        "{reviewed}"
    );
    let approved = w.step(&run, "approver").prompt;
    assert!(
        approved.ends_with("\n\nLines of `decided` whose `taken` is `request` were already decided in the request. One without an `assistant` or a `githubAccount` is the person's: discard a finding that questions it, or one of the specification's that says the same, which is neither rework nor `questioned`, but not one that says the change doesn't follow it. One with either may not be the person's own words: a finding that it, or one of the specification's that says the same, puts security or data at risk goes in `questioned`, never as rework. Only what a person sent back can change one of these, and then it wins. Where what the approver or an assistant sent back goes against one, the decision stands: put the conflict in `questioned`, never as rework."),
        "{approved}"
    );
    // A prompt that says it already gets it once.
    let built_in = w.step(&run, "built-in").prompt;
    assert_eq!(
        built_in
            .matches("Lines of `decided` whose `taken` is `request`")
            .count(),
        1,
        "{built_in}"
    );
    // Where the request decided nothing, nothing is added.
    for step in ["review", "approver"] {
        let prompt = w.step(&plain, step).prompt;
        assert!(!prompt.contains("`taken` is `request`"), "{prompt}");
    }
}

const DECIDED: &str = "Decided:\n- Old threads stay as they are.";

#[test]
fn what_an_assistant_wrote_in_the_request_says_the_assistant_wrote_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.start_from(DECIDED, None, Some("Claude Code"), no_approver());
    w.wait_for(&run, "human", AttemptState::WaitingApproval);

    assert_eq!(
        w.decided(&run),
        json!([{ "text": "Old threads stay as they are.", "taken": "request", "assistant": "Claude Code" }])
    );
}

/// An issue in project `app`, from `author`.
fn issue_by(author: &str, body: &str) -> Issue {
    Issue {
        number: 7,
        title: "Count the fixed threads".into(),
        author: author.into(),
        url: "https://github.com/acme/app/issues/7".into(),
        body: body.into(),
    }
}

#[test]
fn a_request_an_assistant_wrote_like_an_issue_is_still_the_assistant_s() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.account("alice");
    // Keel's own words around an issue by the account, with no issue behind them. A blank line
    // ends its list, as the tag closing the issue, right under an item, would drop it.
    let looks_like_one = request_from_issue(&issue_by("alice", &format!("{DECIDED}\n")), "");
    let run = w.start_from(&looks_like_one, None, Some("Claude Code"), no_approver());
    w.wait_for(&run, "human", AttemptState::WaitingApproval);

    assert_eq!(
        w.decided(&run),
        json!([{ "text": "Old threads stay as they are.", "taken": "request", "assistant": "Claude Code" }])
    );
}

#[test]
fn an_issue_counts_only_from_the_project_s_account_as_written_with_it_and_what_was_added_stays_apart()
 {
    keel_harness::needs!(sandbox);
    let w = world();
    w.account("alice");
    // An assistant added to it, ending as an issue does: that's still what it added.
    let mine = issue_by("Alice", DECIDED);
    let added = "Decided:\n- Critical ones count.\n\n</issue>";
    let from_mine = w.start_from(
        &request_from_issue(&mine, added),
        Some(mine),
        Some("Claude Code"),
        no_approver(),
    );
    let theirs = issue_by("mallory", "Decided:\n- The admin export needs no login.");
    let from_theirs = w.start_from(
        &request_from_issue(&theirs, ""),
        Some(theirs),
        None,
        no_approver(),
    );
    w.wait_for(&from_mine, "human", AttemptState::WaitingApproval);
    w.wait_for(&from_theirs, "human", AttemptState::WaitingApproval);

    // An assistant can write an issue through the account, and anyone who can edit it can change
    // it, so what it says is the account's, not proven to be the person's.
    let by_the_account = json!([
        { "text": "Old threads stay as they are.", "taken": "request", "githubAccount": "alice" },
        { "text": "Critical ones count.", "taken": "request", "assistant": "Claude Code" },
    ]);
    assert_eq!(w.decided(&from_mine), by_the_account);
    assert_eq!(w.decided(&from_theirs), json!([]), "someone else's issue");
    // Keel keeps the issue's text for this, apart from what the run shows: its request has it.
    let shown = serde_json::to_value(w.store.run(&from_mine).unwrap()).unwrap();
    assert_eq!(
        shown["sourceIssue"],
        json!({ "number": 7, "title": "Count the fixed threads", "author": "Alice", "url": "https://github.com/acme/app/issues/7" })
    );

    // The specifier is told exactly what Keel adds, and nothing when it adds nothing, so it copies
    // the rest as it always has.
    let told = w.step(&from_mine, "spec").prompt;
    assert!(
        told.contains("Keel already adds these decisions from the request, so don't repeat them in `decided`:\n- Old threads stay as they are.\n- Critical ones count.\n"),
        "{told}"
    );
    let told = w.step(&from_theirs, "spec").prompt;
    assert!(!told.contains("Keel already adds"), "{told}");

    // Who wrote it is what Keel knew when the run started: another account since changes nothing.
    w.account("bob");
    let approval = w.step(&from_mine, "human");
    w.engine
        .answer_approval(&approval.id, false, "Not yet")
        .unwrap();
    w.engine
        .retry(&w.step(&from_mine, "spec").id, false)
        .unwrap();
    w.wait_for_attempts(&from_mine, "spec", 2, AttemptState::Succeeded);
    assert_eq!(w.decided(&from_mine), by_the_account);
}

#[test]
fn a_run_from_before_keel_kept_who_wrote_its_request_decides_nothing_with_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.start_asking(DECIDED, no_approver());
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    assert_eq!(
        w.decided(&run),
        json!([{ "text": "Old threads stay as they are.", "taken": "request" }])
    );

    // As a run that started before Keel kept it: nothing tells who wrote what it asks.
    w.store
        .patch_run(&run, &[("request_written", Column::Null)])
        .unwrap();
    w.engine
        .answer_approval(&approval.id, false, "Not yet")
        .unwrap();
    w.engine.retry(&w.step(&run, "spec").id, false).unwrap();
    let spec = w.wait_for_attempts(&run, "spec", 2, AttemptState::Succeeded);
    assert_eq!(w.decided(&run), json!([]));
    assert!(
        !spec.prompt.contains("Keel already adds"),
        "{}",
        spec.prompt
    );
}

#[test]
fn an_answer_that_could_not_go_ahead_leaves_nothing_behind_and_can_be_given_again() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[asking("Q1", "If it's 0, what then?"), good()]);
    let run = w.start(no_approver());
    let first = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    let workflow = w.store.run(&run).unwrap().workflow;
    let answer = [remembered(option("Q1", "Return 0"))];

    // With no such step in the run any more, the agent can't start.
    w.store
        .patch_run(&run, &[("workflow", Column::Text("[]".into()))])
        .unwrap();
    assert!(w.engine.answer_questions(&first.id, &answer).is_err());
    let after = w.step(&run, "spec");
    assert_eq!(after.state, "waitingAnswers");
    assert!(after.output.unwrap().get("answered").is_none());
    assert!(!w.kinds(&first.id).contains(&"answers".to_owned()));
    assert!(w.engine.decisions(&w.project_id).unwrap().is_empty());

    w.store
        .patch_run(&run, &[("workflow", Column::Text(workflow.to_string()))])
        .unwrap();
    w.engine.answer_questions(&first.id, &answer).unwrap();
    w.wait_for(&run, "builder", AttemptState::Succeeded);
    assert_eq!(w.engine.decisions(&w.project_id).unwrap().len(), 1);
}

#[test]
fn a_different_answer_resumes_the_same_session_and_the_builder_sees_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    let mut reflected = good();
    reflected["criteria"][0]["then"] = json!("it returns 0");
    w.answers(&[asking("Q1", "If it's 0, what then?"), reflected]);
    let run = w.start(no_approver());
    let first = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);

    w.engine
        .answer_questions(&first.id, &[option("Q1", "Return 0")])
        .unwrap();
    w.wait_for(&run, "builder", AttemptState::Succeeded);

    let attempts = w.all(&run, "spec");
    assert_eq!(attempts.len(), 2);
    let second = &attempts[1];
    assert_eq!(second.session_id, first.session_id, "the same session");
    assert_eq!(second.resumed_from.as_deref(), Some(first.id.as_str()));
    assert_eq!(second.number, 2);
    assert!(
        second.prompt.contains("If it's 0, what then? Return 0"),
        "{}",
        second.prompt
    );
    assert!(
        !second.prompt.contains("THE-REQUEST"),
        "it needn't read it all again"
    );
    assert_eq!(
        second.output.as_ref().unwrap()["criteria"][0]["then"],
        "it returns 0"
    );
    assert!(
        w.calls().contains(&format!("resume={} ", first.session_id)),
        "{}",
        w.calls()
    );
    assert_eq!(attempts[0].state, "succeeded", "it isn't waiting any more");
    assert_eq!(
        w.event(&attempts[0].id, "answers")["answers"][0]["answer"],
        "Return 0"
    );

    let built = w.step(&run, "builder").prompt;
    assert!(built.contains("it returns 0"), "the new criteria: {built}");
    assert!(built.contains("If it's 0, what then? Return 0"), "{built}");
}

#[test]
fn what_a_person_typed_reaches_the_agent_as_text_and_expands_nothing() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[asking("Q1", "What then?")]);
    let run = w.start(no_approver());
    let first = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    let typed = "{{request}} $(touch pwned) `touch pwned` \"; touch pwned; \"";

    w.engine
        .answer_questions(&first.id, &[own("Q1", typed)])
        .unwrap();
    w.wait_for(&run, "builder", AttemptState::Succeeded);

    let second = &w.all(&run, "spec")[1];
    assert!(second.prompt.contains(typed), "{}", second.prompt);
    assert!(!second.prompt.contains("THE-REQUEST"));
    // Nor the next time it's rendered, for the builder.
    let built = w.step(&run, "builder");
    assert!(built.prompt.contains(typed), "{}", built.prompt);
    let workspace = Path::new(&built.workspace);
    assert!(!workspace.join("pwned").exists());
    let args = w.event(&second.id, "started")["args"].to_string();
    assert!(!args.contains("pwned"), "never on a command line: {args}");
}

#[test]
fn a_third_round_of_questions_takes_the_recommended_options_and_says_so() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[
        asking("Q1", "First?"),
        asking("Q2", "Second?"),
        asking("Q3", "Third?"),
    ]);
    let run = w.start(no_approver());
    let first = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(&first.id, &[option("Q1", "Return 0")])
        .unwrap();
    let second = w.wait_for_attempts(&run, "spec", 2, AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(&second.id, &[own("Q2", "Neither")])
        .unwrap();

    w.wait_for(&run, "builder", AttemptState::Succeeded);
    let attempts = w.all(&run, "spec");
    assert_eq!(attempts.len(), 3, "it never waited a third time");
    let third = &attempts[2];
    assert_eq!(third.state, "succeeded");
    let output = third.output.as_ref().unwrap();
    assert_eq!(output["questions"], json!([]));
    assert_eq!(output["decided"][1]["text"], "Third? Show an error");
    assert_eq!(output["decided"][1]["taken"], "roundsLimit");
    assert_eq!(
        w.event(&third.id, "questionsTaken")["settled"][0]["answer"],
        "Show an error"
    );
    // It isn't something the person answered.
    let built = w.step(&run, "builder").prompt;
    assert!(
        built.contains("First? Return 0") && built.contains("Second? Neither"),
        "{built}"
    );
    assert!(!built.contains("- Third?"), "{built}");
}

#[test]
fn what_only_a_person_can_answer_is_asked_once_more_and_then_the_step_stops() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[
        asking("Q1", "First?"),
        asking("Q2", "Second?"),
        only_they_know("Q3", "How much?"),
        only_they_know("Q4", "And how much now?"),
    ]);
    let run = w.start(no_approver());
    let first = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(&first.id, &[option("Q1", "Return 0")])
        .unwrap();
    let second = w.wait_for_attempts(&run, "spec", 2, AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(&second.id, &[own("Q2", "Neither")])
        .unwrap();

    // Past the rounds, Keel doesn't pick one of these for the person: it asks them.
    let third = w.wait_for_attempts(&run, "spec", 3, AttemptState::WaitingAnswers);
    let output = third.output.as_ref().unwrap();
    assert_eq!(output["questions"][0]["id"], "Q3");
    assert!(
        !output.to_string().contains("roundsLimit"),
        "nothing taken: {output}"
    );
    w.engine
        .answer_questions(&third.id, &[option("Q3", "10 %")])
        .unwrap();
    assert!(
        w.step(&run, "spec").prompt.contains("How much? 10 %"),
        "the answer reached it"
    );

    // And asking again after that ends the step, rather than going round forever.
    let fourth = w.wait_for_attempts(&run, "spec", 4, AttemptState::Blocked);
    assert_eq!(reason_of(&fourth), Reason::KeptAsking { rounds: 3 });
    assert_eq!(
        fourth.output.as_ref().unwrap()["questions"][0]["id"],
        "Q4",
        "what it wanted to ask can still be read"
    );
    assert!(w.all(&run, "builder").is_empty());
    assert_eq!(w.call_count(), 4);
}

#[test]
fn a_question_with_none_recommended_always_runs_the_agent_with_the_answer() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[only_they_know("Q1", "How much?"), good()]);
    let run = w.start(no_approver());
    let asked = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    // Every answer is one the person chose, so none of them is the recommended one.
    w.engine
        .answer_questions(&asked.id, &[option("Q1", "5 %")])
        .unwrap();
    assert_eq!(w.event(&asked.id, "answers")["recommended"], 0);
    w.wait_for(&run, "builder", AttemptState::Succeeded);
    assert_eq!(w.all(&run, "spec").len(), 2, "the same session went on");
}

#[test]
fn going_over_the_limits_once_gets_corrected_and_twice_ends_blocked() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[too_long(), good()]);
    let run = w.start(no_approver());
    w.wait_for(&run, "builder", AttemptState::Succeeded);
    let attempts = w.all(&run, "spec");
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].state, "failed");
    assert_eq!(attempts[1].session_id, attempts[0].session_id, "resumed");
    assert!(
        attempts[1]
            .prompt
            .contains("`summary` has 201 characters, and the limit is 200."),
        "exactly what to fix: {}",
        attempts[1].prompt
    );
    assert!(
        w.kinds(&attempts[0].id)
            .contains(&"specCorrected".to_owned())
    );

    let w = world();
    w.answers(&[too_long(), too_long(), good()]);
    let run = w.start(no_approver());
    let blocked = w.wait_for_attempts(&run, "spec", 2, AttemptState::Blocked);
    assert_eq!(
        reason_of(&blocked),
        Reason::SpecNotUsable {
            problems: vec![SpecProblem::SummaryTooLong {
                chars: 201,
                limit: 200
            }]
        }
    );
    assert!(w.all(&run, "builder").is_empty());
    assert_eq!(w.call_count(), 2, "and it stops there");
    assert_eq!(w.store.totals().unwrap().blocked, 1);
}

#[test]
fn fixing_a_limit_does_not_use_up_the_rounds_of_questions() {
    keel_harness::needs!(sandbox);
    let w = world();
    let mut broken_with_a_question = asking("Q1", "First?");
    broken_with_a_question["summary"] = json!("x".repeat(201));
    w.answers(&[
        broken_with_a_question,
        asking("Q1", "First?"),
        asking("Q2", "Second?"),
        good(),
    ]);
    let run = w.start(no_approver());
    let first = w.wait_for_attempts(&run, "spec", 2, AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(&first.id, &[option("Q1", "Return 0")])
        .unwrap();
    let second = w.wait_for_attempts(&run, "spec", 3, AttemptState::WaitingAnswers);
    assert!(
        second.output.unwrap()["questions"][0]["id"] == "Q2",
        "it still gets to ask again"
    );
    w.engine
        .answer_questions(&second.id, &[option("Q2", "Return 0")])
        .unwrap();
    w.wait_for(&run, "builder", AttemptState::Succeeded);
    assert_eq!(w.all(&run, "spec").len(), 4);
}

#[test]
fn a_result_that_came_from_answers_gets_its_own_chance_to_fix_a_limit() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[too_long(), asking("Q1", "First?"), too_long(), good()]);
    let run = w.start(no_approver());
    let waiting = w.wait_for_attempts(&run, "spec", 2, AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(&waiting.id, &[option("Q1", "Return 0")])
        .unwrap();
    w.wait_for(&run, "builder", AttemptState::Succeeded);
    let states: Vec<String> = w.all(&run, "spec").into_iter().map(|a| a.state).collect();
    assert_eq!(states, ["failed", "succeeded", "failed", "succeeded"]);
}

#[test]
fn resuming_by_hand_says_again_what_to_fix() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[too_long(), too_long(), good(), good()]);
    let run = w.start(no_approver());
    let blocked = w.wait_for_attempts(&run, "spec", 2, AttemptState::Blocked);

    w.engine.retry(&blocked.id, true).unwrap();
    w.wait_for(&run, "builder", AttemptState::Succeeded);
    let attempts = w.all(&run, "spec");
    assert_eq!(attempts.len(), 3);
    assert_eq!(attempts[2].session_id, blocked.session_id);
    assert!(
        attempts[2].prompt.contains("`summary` has 201 characters"),
        "{}",
        attempts[2].prompt
    );
}

#[test]
fn answers_that_do_not_belong_are_refused_and_change_nothing() {
    keel_harness::needs!(sandbox);
    let w = world();
    let mut output = asking("Q1", "First?");
    output["questions"].as_array_mut().unwrap().push(json!({
        "id": "Q2",
        "text": "Second?",
        "options": [{ "label": "Up", "recommended": true }, { "label": "Down", "recommended": false }],
    }));
    w.answers(&[output]);
    let run = w.start(no_approver());
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    let events = w.kinds(&waiting.id).len();
    let calls = w.call_count();

    let answer = |answers: &[Answer]| blocked_with(w.engine.answer_questions(&waiting.id, answers));
    assert_eq!(
        answer(&[
            option("Q1", "Show an error"),
            option("Q2", "Up"),
            option("Q9", "Up")
        ]),
        invalid("unknownQuestion", "Q9")
    );
    assert_eq!(
        answer(&[remembered(option("Q1", "Show an error"))]),
        invalid("unanswered", "Q2")
    );
    assert_eq!(
        answer(&[option("Q1", "Something else"), option("Q2", "Up")]),
        invalid("optionNotOffered", "Q1")
    );
    assert_eq!(
        answer(&[own("Q1", " \n "), option("Q2", "Up")]),
        invalid("emptyText", "Q1")
    );
    assert_eq!(
        answer(&[
            option("Q1", "Show an error"),
            option("Q1", "Return 0"),
            option("Q2", "Up")
        ]),
        invalid("repeatedQuestion", "Q1")
    );
    assert_eq!(answer(&[]), invalid("unanswered", "Q1"));

    let after = w.step(&run, "spec");
    assert_eq!(after.state, "waitingAnswers");
    assert_eq!(w.kinds(&waiting.id).len(), events, "not even on the record");
    assert_eq!(w.call_count(), calls);
    assert!(w.engine.decisions(&w.project_id).unwrap().is_empty());
}

#[test]
fn only_the_first_of_two_answers_counts_however_close_together() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[asking("Q1", "First?"), good()]);
    let run = w.start(no_approver());
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);

    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (engine, id) = (w.engine.clone(), waiting.id.clone());
                scope.spawn(move || engine.answer_questions(&id, &[option("Q1", "Return 0")]))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let worked = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(worked, 1, "{results:?}");
    assert!(
        results
            .iter()
            .all(|r| r.is_ok() || matches!(r, Err(Error::InvalidState)))
    );

    w.wait_for(&run, "builder", AttemptState::Succeeded);
    assert_eq!(w.all(&run, "spec").len(), 2, "one resumed attempt");
    assert_eq!(
        w.kinds(&waiting.id)
            .iter()
            .filter(|k| *k == "answers")
            .count(),
        1
    );
    assert!(matches!(
        w.engine
            .answer_questions(&waiting.id, &[option("Q1", "Return 0")]),
        Err(Error::InvalidState)
    ));
}

#[test]
fn answering_and_sending_back_refuse_what_is_not_waiting_for_them() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[asking("Q1", "First?")]);
    let run = w.start(no_approver());
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);

    assert!(matches!(
        w.engine.send_back(&waiting.id, "Do it differently"),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        w.engine.answer_approval(&waiting.id, true, ""),
        Err(Error::InvalidState)
    ));
    // Nothing is running to stop, and retrying would step over the person.
    assert!(matches!(
        w.engine.stop(&waiting.id),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        w.engine.retry(&waiting.id, false),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        w.engine.retry(&waiting.id, true),
        Err(Error::InvalidState)
    ));
    assert_eq!(w.step(&run, "spec").state, "waitingAnswers");

    w.engine
        .answer_questions(&waiting.id, &[option("Q1", "Show an error")])
        .unwrap();
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    assert!(matches!(
        w.engine
            .answer_questions(&approval.id, &[option("Q1", "Show an error")]),
        Err(Error::InvalidState)
    ));
    let builder = w.step(&run, "builder");
    assert!(matches!(
        w.engine.answer_questions(&builder.id, &[]),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        w.engine.send_back(&builder.id, "x"),
        Err(Error::InvalidState)
    ));

    w.engine.send_back(&approval.id, "Return 0").unwrap();
    assert!(matches!(
        w.engine.send_back(&approval.id, "Again"),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        w.engine.answer_approval(&approval.id, true, ""),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        w.engine.retry(&approval.id, false),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        w.engine.abandon(&approval.id),
        Err(Error::InvalidState)
    ));
}

#[test]
fn cancelling_sends_the_card_to_the_backlog_and_writes_no_decision() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[asking("Q1", "First?")]);
    let workflow = w
        .engine
        .save_workflow(
            &w.project_id,
            &Workflow {
                id: String::new(),
                name: "Mine".into(),
                steps: no_approver(),
                builtin: false,
            },
        )
        .unwrap();
    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                default_workflow: Some(workflow.id),
                ..Default::default()
            },
        )
        .unwrap();
    let card = w.engine.card_create(&w.project_id, "Fix it", "").unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    w.engine.board_tick().unwrap();
    let run = w.store.card(&card).unwrap().run_id.unwrap();
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    assert_eq!(
        w.store.card(&card).unwrap().column(),
        BoardColumn::HumanReview
    );

    w.engine.abandon(&waiting.id).unwrap();

    assert_eq!(w.store.card(&card).unwrap().column(), BoardColumn::Backlog);
    assert_eq!(w.step(&run, "spec").state, "abandoned");
    assert!(w.store.run(&run).unwrap().finished_at.is_some());
    assert!(w.engine.decisions(&w.project_id).unwrap().is_empty());
    assert!(w.all(&run, "builder").is_empty());
    assert!(matches!(
        w.engine
            .answer_questions(&waiting.id, &[option("Q1", "Show an error")]),
        Err(Error::InvalidState)
    ));
}

#[test]
fn the_wait_survives_a_restart_and_can_be_answered_after_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[asking("Q1", "First?")]);
    let run = w.start(no_approver());
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    let calls = w.call_count();
    let answered_before = w.kinds(&waiting.id);

    // As the app does when it comes back.
    let mut config = Config::new(w.dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    let again = Engine::new(
        w.store.clone(),
        config,
        tools_in(&w.dir.path().join("bin")),
        |_| {},
    );
    again.recover().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    assert_eq!(w.step(&run, "spec").state, "waitingAnswers");
    assert_eq!(w.call_count(), calls, "no agent started");
    assert_eq!(w.kinds(&waiting.id), answered_before, "nothing looked lost");
    assert!(w.all(&run, "builder").is_empty());

    again
        .answer_questions(&waiting.id, &[option("Q1", "Show an error")])
        .unwrap();
    w.wait_for(&run, "builder", AttemptState::Succeeded);
}

#[test]
fn remembering_an_answer_makes_it_a_decision_and_not_remembering_makes_none() {
    keel_harness::needs!(sandbox);
    let w = world();
    let mut output = asking("Q1", "First?");
    output["questions"].as_array_mut().unwrap().push(json!({
        "id": "Q2",
        "text": "Second?",
        "options": [{ "label": "Up", "recommended": true }, { "label": "Down", "recommended": false }],
    }));
    w.answers(&[output]);
    let run = w.start(no_approver());
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);

    w.engine
        .answer_questions(
            &waiting.id,
            &[
                remembered(option("Q1", "Show an error")),
                option("Q2", "Up"),
            ],
        )
        .unwrap();
    w.wait_for(&run, "human", AttemptState::WaitingApproval);

    let decisions = w.engine.decisions(&w.project_id).unwrap();
    assert_eq!(decisions.len(), 1, "only the one they said to remember");
    assert_eq!(decisions[0].answer, "answered");
    assert_eq!(decisions[0].question.as_deref(), Some("First?"));
    assert_eq!(decisions[0].text, "Show an error");
    assert_eq!(decisions[0].attempt_id, waiting.id);

    // Later agents get it, in a form that says who said what.
    let second = w.start(vec![builder()]);
    w.wait_for(&second, "builder", AttemptState::Succeeded);
    let brain = w.engine.brain(&w.project_id).unwrap();
    assert_eq!(brain.decisions.len(), 1);
    assert!(
        brain
            .prompt
            .contains("a person was asked First? and answered Show an error"),
        "{}",
        brain.prompt
    );
    assert!(
        w.calls()
            .contains("a person was asked First? and answered Show an error")
    );
}

#[test]
fn an_answer_a_person_typed_stays_on_one_line_when_it_is_remembered() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[asking("Q1", "First?"), good()]);
    let run = w.start(no_approver());
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(
            &waiting.id,
            &[remembered(own("Q1", "line one\n\n# line two"))],
        )
        .unwrap();
    w.wait_for(&run, "human", AttemptState::WaitingApproval);
    assert_eq!(
        w.engine.decisions(&w.project_id).unwrap()[0].text,
        "line one # line two"
    );
}

#[test]
fn sending_back_with_an_approver_runs_it_all_again_and_does_not_read_as_approved() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[good()]);
    let run = w.start(with_approver(2));
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);

    w.engine
        .send_back(&approval.id, "  Return 0 when it's empty  ")
        .unwrap();

    let again = w.wait_for_attempts(&run, "human", 2, AttemptState::WaitingApproval);
    assert_eq!(again.round, 1);
    assert_eq!(w.store.run(&run).unwrap().round, 1);
    let rounds = |id: &str| w.all(&run, id).iter().map(|a| a.round).collect::<Vec<_>>();
    assert_eq!(rounds("builder"), [0, 1]);
    assert_eq!(rounds("check"), [0, 1], "the check runs again");
    assert_eq!(rounds("approver"), [0, 1]);
    assert_eq!(rounds("spec"), [0], "not what came before");

    // It says what happened to it, and it isn't "approved".
    let returned = w.store.attempt(&approval.id).unwrap();
    assert_eq!(returned.state, "returned");
    assert!(returned.finished_at.is_some());
    let kinds = w.kinds(&returned.id);
    assert!(kinds.contains(&"returned".to_owned()), "{kinds:?}");
    assert!(!kinds.contains(&"decision".to_owned()), "{kinds:?}");
    assert_eq!(
        w.event(&returned.id, "returned")["instructions"],
        "Return 0 when it's empty"
    );

    // It reaches who works and who reviews, and it stays in the run.
    let fixed = w.step(&run, "builder").prompt;
    assert!(fixed.contains("A person sent this work back."), "{fixed}");
    assert!(fixed.contains("Return 0 when it's empty"), "{fixed}");
    assert!(!fixed.contains("FROM-THE-APPROVER"), "it approved: {fixed}");
    assert!(
        w.step(&run, "approver")
            .prompt
            .contains("Return 0 when it's empty")
    );
    assert!(w.engine.decisions(&w.project_id).unwrap().is_empty());
    assert_eq!(w.store.runs(10).unwrap()[0].state, "waitingApproval");

    // And the person can still say yes.
    w.engine.answer_approval(&again.id, true, "").unwrap();
    assert!(w.store.run(&run).unwrap().finished_at.is_some());
}

#[test]
fn sending_back_with_no_approver_starts_again_at_the_last_worker_that_builds() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[good()]);
    let run = w.start(no_approver());
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);

    w.engine.send_back(&approval.id, "Return 0").unwrap();
    let again = w.wait_for_attempts(&run, "human", 2, AttemptState::WaitingApproval);
    assert_eq!(again.round, 1);
    let rounds = |id: &str| w.all(&run, id).iter().map(|a| a.round).collect::<Vec<_>>();
    assert_eq!(rounds("spec"), [0]);
    assert_eq!(rounds("builder"), [0, 1]);
    assert_eq!(rounds("check"), [0, 1], "the check runs again");
    assert!(w.step(&run, "builder").prompt.contains("Return 0"));
    assert_eq!(w.store.attempt(&approval.id).unwrap().state, "returned");
}

#[test]
fn a_round_a_person_asked_for_does_not_count_against_the_approver() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[good()]);
    let run = w.start(with_approver(1));
    let first = w.wait_for(&run, "human", AttemptState::WaitingApproval);

    w.decisions(&["rework", "rework"]);
    w.engine.send_back(&first.id, "Change it").unwrap();

    // The approver still has its one round. Its second answer is what goes over.
    let blocked = w.wait_for_attempts(&run, "approver", 3, AttemptState::Blocked);
    assert_eq!(reason_of(&blocked), Reason::ReworkLimit { rounds: 1 });
    assert_eq!(w.store.run(&run).unwrap().round, 2);
    assert_eq!(w.all(&run, "builder").len(), 3);
    let from_approver = w.step(&run, "builder").prompt;
    assert!(
        from_approver.contains("The approver sent this work back."),
        "{from_approver}"
    );
    assert!(
        from_approver.contains("FROM-THE-APPROVER"),
        "{from_approver}"
    );
}

#[test]
fn a_saved_prompt_that_leaves_out_the_rework_still_gets_what_a_person_asked() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[good()]);
    let run = w.start(vec![
        agent("builder", ToolProfile::Build, StepRole::Worker, "BUILD"),
        human(),
    ]);
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    assert_eq!(w.step(&run, "builder").prompt, "BUILD");
    w.engine.send_back(&approval.id, "Return 0").unwrap();
    w.wait_for_attempts(&run, "human", 2, AttemptState::WaitingApproval);
    let prompt = w.step(&run, "builder").prompt;
    assert!(
        prompt.starts_with("BUILD\n\nA person sent this work back."),
        "{prompt}"
    );
    assert!(prompt.ends_with("Return 0"), "{prompt}");
}

#[test]
fn sending_back_needs_instructions_and_something_to_go_back_to() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.start(vec![
        agent("reader", ToolProfile::ReadOnly, StepRole::Worker, "READ"),
        human(),
    ]);
    let approval = w.wait_for(&run, "human", AttemptState::WaitingApproval);
    assert_eq!(
        blocked_with(w.engine.send_back(&approval.id, "  \n ")),
        Reason::SendBackNeedsInstructions
    );
    assert_eq!(
        blocked_with(w.engine.send_back(&approval.id, "Change it")),
        Reason::NothingToSendBackTo
    );
    assert_eq!(
        w.store.attempt(&approval.id).unwrap().state,
        "waitingApproval"
    );
    assert_eq!(w.store.run(&run).unwrap().round, 0);

    // An approval before anything was built has nothing to send back either.
    let gate = w.start(vec![human(), builder()]);
    let approval = w.wait_for(&gate, "human", AttemptState::WaitingApproval);
    assert_eq!(
        blocked_with(w.engine.send_back(&approval.id, "Change it")),
        Reason::NothingToSendBackTo
    );
}

// A client over MCP can send work back, retry and close a run, and the app has to say who did it.

#[test]
fn a_client_that_sends_work_back_or_closes_the_run_is_named_and_a_person_is_not() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[good()]);
    let run = w.start(no_approver());
    let first = w.wait_for(&run, "human", AttemptState::WaitingApproval);

    // From the app, it's kept just as it always was.
    w.engine.send_back(&first.id, "Return 0").unwrap();
    let second = w.wait_for_attempts(&run, "human", 2, AttemptState::WaitingApproval);
    assert_eq!(
        w.store.attempt(&first.id).unwrap().output,
        Some(json!({ "instructions": "Return 0" }))
    );
    assert_eq!(w.event(&first.id, "returned").get("by"), None);

    w.engine
        .send_back_by(&second.id, "  Return 1  ", "Claude Code")
        .unwrap();
    let third = w.wait_for_attempts(&run, "human", 3, AttemptState::WaitingApproval);
    assert_eq!(
        w.store.attempt(&second.id).unwrap().output,
        Some(json!({ "instructions": "Return 1", "by": "Claude Code" }))
    );
    assert_eq!(
        w.event(&second.id, "returned"),
        json!({ "instructions": "Return 1", "round": 2, "by": "Claude Code" })
    );
    assert!(w.step(&run, "builder").prompt.contains("Return 1"));

    w.engine.abandon_by(&third.id, "Claude Code").unwrap();
    assert_eq!(w.store.attempt(&third.id).unwrap().state, "abandoned");
    assert_eq!(
        w.event(&third.id, "abandoned"),
        json!({ "by": "Claude Code" })
    );
    assert!(w.store.run(&run).unwrap().finished_at.is_some());

    let other = w.start(no_approver());
    let waiting = w.wait_for(&other, "human", AttemptState::WaitingApproval);
    w.engine.abandon(&waiting.id).unwrap();
    assert_eq!(w.event(&waiting.id, "abandoned"), json!({}));
}

#[test]
fn a_client_that_retries_a_step_is_named_on_it_before_the_new_try_starts() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[json!("fail"), json!("fail"), json!("fail")]);
    let run = w.start(no_approver());
    let failed = w.wait_for(&run, "spec", AttemptState::Failed);

    // From the app, nothing new on the step.
    let kinds = w.kinds(&failed.id);
    w.engine.retry(&failed.id, false).unwrap();
    let again = w.wait_for_attempts(&run, "spec", 2, AttemptState::Failed);
    assert_eq!(w.kinds(&failed.id), kinds);

    w.engine.retry_by(&again.id, true, "Claude Code").unwrap();
    let asked = w
        .store
        .events(&again.id, 0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "retryRequested")
        .expect("it says who asked");
    assert_eq!(asked.data, json!({ "by": "Claude Code", "resume": true }));
    let resumed = w.wait_for_attempts(&run, "spec", 3, AttemptState::Failed);
    assert_eq!(resumed.session_id, again.session_id);
    let first_of_it = w.store.events(&resumed.id, 0).unwrap()[0].seq;
    assert!(
        asked.seq < first_of_it,
        "it was said before the try started"
    );

    w.engine
        .retry_by(&resumed.id, false, "Claude Code")
        .unwrap();
    assert_eq!(
        w.event(&resumed.id, "retryRequested"),
        json!({ "by": "Claude Code", "resume": false })
    );
    w.wait_for(&run, "human", AttemptState::WaitingApproval);
}

fn invalid_state<T: std::fmt::Debug>(result: Result<T, Error>) -> bool {
    matches!(result, Err(Error::InvalidState))
}

#[test]
fn a_step_already_tried_again_never_starts_twice() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[json!("fail"), json!("fail")]);
    let run = w.start(no_approver());
    let failed = w.wait_for(&run, "spec", AttemptState::Failed);

    // The person and a client, or a client twice, pressing it at the same moment.
    let (engine, id) = (&w.engine, failed.id.as_str());
    let tries: Vec<_> = std::thread::scope(|scope| {
        let tries: Vec<_> = (0..8)
            .map(|n| {
                scope.spawn(move || match n % 2 {
                    0 => engine.retry(id, false),
                    _ => engine.retry_by(id, false, "Claude Code"),
                })
            })
            .collect();
        tries.into_iter().map(|t| t.join().unwrap()).collect()
    });
    assert_eq!(tries.iter().filter(|t| t.is_ok()).count(), 1, "{tries:?}");
    assert!(tries.into_iter().filter(Result::is_err).all(invalid_state));
    let again = w.wait_for_attempts(&run, "spec", 2, AttemptState::Failed);
    assert_eq!(again.number, 2);

    // Later too: that one has had its next try, so only the latest can have another.
    assert!(invalid_state(w.engine.retry(&failed.id, false)));
    assert!(invalid_state(w.engine.retry_by(
        &failed.id,
        true,
        "Claude Code"
    )));
    assert_eq!(w.all(&run, "spec").len(), 2);
    w.engine.retry_by(&again.id, false, "Claude Code").unwrap();
    w.wait_for(&run, "human", AttemptState::WaitingApproval);
}

#[test]
fn a_client_retries_only_what_went_wrong_and_never_reopens_a_run() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[json!("fail")]);
    let run = w.start(no_approver());
    let failed = w.wait_for(&run, "spec", AttemptState::Failed);
    let by_client = |attempt: &str| w.engine.retry_by(attempt, false, "Claude Code");

    // However it ended. As when a close on another step of the run ended it.
    let ended = Column::Text("2026-11-09T10:00:00Z".into());
    w.store.patch_run(&run, &[("finished_at", ended)]).unwrap();
    assert!(invalid_state(by_client(&failed.id)));
    w.store
        .patch_run(&run, &[("finished_at", Column::Null)])
        .unwrap();

    // What the person closed stays closed.
    w.engine.abandon(&failed.id).unwrap();
    assert!(invalid_state(by_client(&failed.id)));
    assert!(w.store.run(&run).unwrap().finished_at.is_some());
    assert_eq!(w.all(&run, "spec").len(), 1);
    assert!(!w.kinds(&failed.id).contains(&"retryRequested".to_owned()));

    // The person can open it again.
    w.engine.retry(&failed.id, false).unwrap();
    w.wait_for(&run, "human", AttemptState::WaitingApproval);
    assert!(w.store.run(&run).unwrap().finished_at.is_none());

    // What went through, or waits for the person, isn't a client's to retry.
    assert!(invalid_state(by_client(&w.step(&run, "builder").id)));
    assert!(invalid_state(by_client(&w.step(&run, "human").id)));
    assert_eq!(w.all(&run, "builder").len(), 1);
}

#[test]
fn a_client_s_retry_and_close_at_once_never_leave_a_step_working_in_a_run_that_ended() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&vec![json!("fail"); 12]);
    for trial in 0..4 {
        let run = w.start(no_approver());
        let failed = w.wait_for(&run, "spec", AttemptState::Failed);
        let (engine, id) = (&w.engine, failed.id.as_str());
        let (retried, closed) = std::thread::scope(|scope| {
            let retry = scope.spawn(move || engine.retry_by(id, false, "Claude Code"));
            let close = scope.spawn(move || engine.abandon_by(id, "Claude Code"));
            (retry.join().unwrap(), close.join().unwrap())
        });
        assert!(
            retried.is_ok() != closed.is_ok(),
            "trial {trial}: {retried:?} {closed:?}"
        );
        assert_eq!(
            w.store.run(&run).unwrap().finished_at.is_some(),
            closed.is_ok()
        );
        if retried.is_ok() {
            w.wait_for_attempts(&run, "spec", 2, AttemptState::Failed);
        } else {
            assert_eq!(w.all(&run, "spec").len(), 1, "trial {trial}");
        }
    }
}

#[test]
fn a_saved_workflow_with_a_plain_worker_spec_behaves_exactly_as_before() {
    keel_harness::needs!(sandbox);
    let w = world();
    let plain = || {
        agent(
            "spec",
            ToolProfile::ReadOnly,
            StepRole::Worker,
            "SPEC {{request}}",
        )
    };
    let builder = agent(
        "builder",
        ToolProfile::Build,
        StepRole::Worker,
        "BUILD spec={{steps.spec.output}}",
    );
    let run = w.start(vec![plain(), builder, human()]);
    w.wait_for(&run, "human", AttemptState::WaitingApproval);

    let spec = w.step(&run, "spec");
    assert_eq!(spec.role, "worker");
    assert_eq!(spec.output, None);
    assert_eq!(spec.result_text.as_deref(), Some("PLAIN-SPEC-TEXT"));
    assert_eq!(w.all(&run, "spec").len(), 1);
    assert!(
        w.step(&run, "builder")
            .prompt
            .contains("spec=PLAIN-SPEC-TEXT")
    );
    // It never gets asked for a structure, so it isn't judged by one.
    assert_eq!(
        w.event(&spec.id, "started")["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|arg| *arg == "--json-schema")
            .count(),
        0
    );
    assert!(!w.calls().contains("Ask only when"));
}

#[test]
fn a_fresh_try_after_answers_still_knows_them_and_a_resumed_one_gets_them_again() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.answers(&[asking("Q1", "First?"), json!("fail"), json!("fail"), good()]);
    let run = w.start(no_approver());
    let waiting = w.wait_for(&run, "spec", AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(&waiting.id, &[option("Q1", "Return 0")])
        .unwrap();
    let failed = w.wait_for_attempts(&run, "spec", 2, AttemptState::Failed);

    // It may never have got them, so a bare "go on" could lose them.
    w.engine.retry(&failed.id, true).unwrap();
    let again = w.wait_for_attempts(&run, "spec", 3, AttemptState::Failed);
    assert_eq!(again.prompt, failed.prompt);
    assert_eq!(again.session_id, waiting.session_id);

    // A new session starts from the request, with what they said.
    w.engine.retry(&again.id, false).unwrap();
    w.wait_for(&run, "builder", AttemptState::Succeeded);
    let fresh = &w.all(&run, "spec")[3];
    assert_ne!(fresh.session_id, waiting.session_id);
    assert!(fresh.prompt.contains("THE-REQUEST"), "{}", fresh.prompt);
    assert!(fresh.prompt.contains("First? Return 0"), "{}", fresh.prompt);
}

#[test]
fn each_resumed_attempt_only_counts_what_it_added_to_the_session() {
    keel_harness::needs!(sandbox);
    let w = world();
    // The fake reports 0.1, 0.2, 0.3 as what the session has cost by then.
    w.answers(&[too_long(), asking("Q1", "First?"), good()]);
    let run = w.start(no_approver());
    let waiting = w.wait_for_attempts(&run, "spec", 2, AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(&waiting.id, &[option("Q1", "Return 0")])
        .unwrap();
    w.wait_for(&run, "builder", AttemptState::Succeeded);
    let costs: Vec<f64> = w
        .all(&run, "spec")
        .iter()
        .map(|a| (a.cost_usd.unwrap() * 100.0).round() / 100.0)
        .collect();
    assert_eq!(costs, [0.1, 0.1, 0.1]);
}

#[test]
fn the_results_count_who_stopped_to_ask_what_matched_and_what_was_sent_back() {
    keel_harness::needs!(sandbox);
    let w = world();
    let workflow = w
        .engine
        .save_workflow(
            &w.project_id,
            &Workflow {
                id: String::new(),
                name: "Mine".into(),
                steps: no_approver(),
                builtin: false,
            },
        )
        .unwrap();
    let start = || {
        w.engine
            .start_run(StartRun {
                project_id: w.project_id.clone(),
                title: None,
                request: "x".into(),
                workflow: workflow.steps.clone(),
                saved_workflow: Some(workflow.reference()),
                language: "en".into(),
                source_issue: None,
                started_by: None,
            })
            .unwrap()
    };
    w.answers(&[asking("Q1", "First?"), good(), good()]);
    let asked = start();
    let waiting = w.wait_for(&asked, "spec", AttemptState::WaitingAnswers);
    w.engine
        .answer_questions(&waiting.id, &[option("Q1", "Return 0")])
        .unwrap();
    let approval = w.wait_for(&asked, "human", AttemptState::WaitingApproval);
    w.engine.send_back(&approval.id, "Change it").unwrap();
    w.wait_for_attempts(&asked, "human", 2, AttemptState::WaitingApproval);

    let quiet = start();
    w.wait_for(&quiet, "human", AttemptState::WaitingApproval);

    let results = w.engine.workflow_results(&w.project_id).unwrap();
    assert_eq!(results[0].runs, 2);
    assert_eq!(results[0].stopped_to_ask, 1);
    assert_eq!(results[0].answers, 1);
    assert_eq!(results[0].answers_recommended, 0);
    assert_eq!(results[0].sent_back, 1);
}
