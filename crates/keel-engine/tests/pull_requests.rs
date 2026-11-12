//! What a pull request says about what was asked, and finding out what became of it, against a fake
//! `claude`, a fake `gh` and a local bare repo standing in for GitHub.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use keel_core::{
    AttemptState, BoardColumn, CardLane, CiState, PrState, StepRole, StepSpec, ToolProfile,
    WorkflowStep,
};
use keel_engine::{Config, Engine, StartRun, Workflow};
use keel_harness::Tools;
use keel_store::rusqlite_value::Value;
use keel_store::{ProjectSettings, Store};

type Changes = Arc<Mutex<Vec<String>>>;

struct World {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
    log: PathBuf,
    flags: PathBuf,
    changes: Changes,
}

fn git(repo: &Path, args: &[&str]) {
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
}

fn write_script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Steps asked for a structure answer with what's in `spec-json` or `verify-json`, or with no
/// proposals. Other read-only steps answer with what's in `spec-text`, and build steps change a file,
/// and also build with `builds` there.
fn fake_claude(flags: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true,"subscriptionType":"max","email":"a@example.com"}}'; exit 0;;
esac
cat > /dev/null
tools=""; schema=""; prev=""
for a in "$@"; do
  [ "$prev" = "--tools" ] && tools="$a"
  [ "$prev" = "--json-schema" ] && schema="$a"
  prev="$a"
done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
[ -n "$schema" ] && list="$list\",\"StructuredOutput"
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
if [ -n "$schema" ]; then
  case "$schema" in
  *unverified*) output="$(cat "{flags}/verify-json")";;
  *criteria*) output="$(cat "{flags}/spec-json")";;
  *) output='{{"learnings":[]}}';;
  esac
  echo "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"num_turns\":1,\"structured_output\":$output}}"
  exit 0
fi
if [ "$tools" = "Read,Grep,Glob" ]; then
  text="$(cat "{flags}/spec-text")"
else
  text="Fixed it."
  if [ -f "{flags}/builds" ]; then
    mkdir -p target/debug node_modules/x && echo bin > target/debug/app && echo js > node_modules/x/index.js
  fi
  echo work >> fixed.txt
  git add -A && git commit -qm "fix"
fi
echo "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"$text\",\"num_turns\":1,\"total_cost_usd\":0.5,\"usage\":{{\"input_tokens\":10,\"output_tokens\":2}},\"modelUsage\":{{\"claude-fake\":{{}}}}}}"
"##,
        flags = flags.display(),
    )
}

/// `pr view` answers with what's in `pr-view`, and fails without it, or when asked about the checks
/// with `no-checks` there. `pr create` refuses a description GitHub would refuse.
fn fake_gh(log: &Path, flags: &Path) -> String {
    format!(
        r##"#!/bin/bash
echo "gh $*" >> "{log}/gh.log"
case "$1 $2" in
"auth token")
  if [ -f "{flags}/no-token" ]; then echo "no token for this account" >&2; exit 1; fi
  echo "tok-SECRET"; exit 0;;
"repo view") echo '{{"defaultBranchRef":{{"name":"main"}},"viewerPermission":"WRITE","isPrivate":true}}'; exit 0;;
"api repos/acme/app/branches/main") echo false; exit 0;;
"pr create")
  if [ -f "{flags}/pr-exists" ]; then echo "a pull request for branch already exists:" >&2; echo "https://github.com/acme/app/pull/99" >&2; exit 1; fi
  prev=""
  for a in "$@"; do [ "$prev" = "--body-file" ] && cp "$a" "{log}/pr-body.md"; prev="$a"; done
  if [ "$(wc -m < "{log}/pr-body.md")" -gt 65536 ]; then echo "Body is too long (maximum is 65536 characters)" >&2; exit 1; fi
  echo "https://github.com/acme/app/pull/12"; exit 0;;
"pr view")
  case "$*" in *statusCheckRollup*)
    if [ -f "{flags}/no-checks" ]; then echo "GraphQL: Resource not accessible by personal access token" >&2; exit 1; fi;;
  esac
  if [ -f "{flags}/pr-view" ]; then cat "{flags}/pr-view"; exit 0; fi
  echo "HTTP 502: Bad Gateway (https://api.github.com/graphql)" >&2; exit 1;;
esac
echo "unexpected: $*" >&2; exit 1
"##,
        log = log.display(),
        flags = flags.display(),
    )
}

fn world() -> World {
    world_with(|_| {})
}

fn world_with(tune: impl FnOnce(&mut Config)) -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let log = dir.path().join("log");
    let flags = dir.path().join("flags");
    for path in [&bin, &log, &flags] {
        std::fs::create_dir(path).unwrap();
    }
    write_script(&bin.join("claude"), &fake_claude(&flags));
    write_script(&bin.join("gh"), &fake_gh(&log, &flags));
    std::fs::write(flags.join("spec-text"), "add returns the sum").unwrap();

    let remote = dir.path().join("remotes/acme/app.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Tester"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "published\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    git(&repo, &["push", "-q", &remote.to_string_lossy(), "main"]);
    git(
        &repo,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    config.github_git_base = format!("file://{}/remotes", dir.path().display());
    tune(&mut config);
    let changes = Changes::default();
    let told = changes.clone();
    let engine = Engine::new(store.clone(), config, tools, move |run: &str| {
        told.lock().unwrap().push(run.to_owned());
    });
    let project = engine.add_project(&repo).unwrap();
    engine
        .update_project(&project.id, &account(Some("alice")))
        .unwrap();
    World {
        _dir: dir,
        store,
        engine,
        project_id: project.id,
        log,
        flags,
        changes,
    }
}

fn account(login: Option<&str>) -> ProjectSettings {
    ProjectSettings {
        github_account: login.map(str::to_owned),
        ..Default::default()
    }
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

fn delivering_steps() -> Vec<WorkflowStep> {
    vec![
        agent("spec", ToolProfile::ReadOnly),
        agent("builder", ToolProfile::Build),
        WorkflowStep::Approval { id: "human".into() },
        WorkflowStep::Deliver { id: "pr".into() },
    ]
}

fn pr_view(state: &str, draft: bool, at: &str) -> String {
    format!(
        r#"{{"state":"{state}","isDraft":{draft},"mergedAt":"{at}","closedAt":"{at}","updatedAt":"{at}"}}"#
    )
}

impl World {
    fn gh_log(&self) -> String {
        std::fs::read_to_string(self.log.join("gh.log")).unwrap_or_default()
    }

    /// Every call, including one asked again without the checks.
    fn asked_about_prs(&self) -> usize {
        self.gh_log().matches("gh pr view").count()
    }

    fn body(&self) -> String {
        std::fs::read_to_string(self.log.join("pr-body.md")).unwrap()
    }

    /// As a person would have saved it: its steps say nothing about any role.
    fn saved(&self, steps: Vec<WorkflowStep>) -> Workflow {
        self.engine
            .save_workflow(
                &self.project_id,
                &Workflow {
                    id: String::new(),
                    name: "Mine".into(),
                    steps,
                    builtin: false,
                },
            )
            .unwrap()
    }

    fn start(&self, workflow: &Workflow) -> String {
        self.engine
            .start_run(StartRun {
                project_id: self.project_id.clone(),
                title: None,
                request: "Fix the bug".into(),
                workflow: workflow.steps.clone(),
                saved_workflow: Some(workflow.reference()),
                language: "en".into(),
                source_issue: None,
                started_by: None,
            })
            .unwrap()
    }

    fn until(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "{what} never happened");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_step(&self, run: &str, id: &str, state: AttemptState) -> keel_store::Attempt {
        self.until(&format!("{id} reaching {state:?}"), || {
            self.step(run, id)
                .is_some_and(|a| a.state == state.as_str())
        });
        self.step(run, id).unwrap()
    }

    fn step(&self, run: &str, id: &str) -> Option<keel_store::Attempt> {
        self.store
            .attempts_of_run(run)
            .unwrap()
            .into_iter()
            .rev()
            .find(|a| a.step_id == id)
    }

    /// A whole run, up to its pull request and past the last change it reports.
    fn delivered(&self) -> String {
        let workflow = self.saved(delivering_steps());
        let run = self.start(&workflow);
        let human = self.wait_step(&run, "human", AttemptState::WaitingApproval);
        self.engine.answer_approval(&human.id, true, "").unwrap();
        self.until("the run finishing", || {
            self.store.run(&run).unwrap().finished_at.is_some()
        });
        std::thread::sleep(Duration::from_millis(100));
        run
    }

    /// As if the last look were long ago, or a person's opening the run would skip it.
    fn forget_looking(&self, run: &str) {
        self.store
            .patch_run(run, &[("pr_checked_at", Value::Null)])
            .unwrap();
    }

    fn set_pr_view(&self, state: &str, draft: bool, at: &str) {
        std::fs::write(self.flags.join("pr-view"), pr_view(state, draft, at)).unwrap();
    }

    fn wait_pr_state(&self, run: &str, state: PrState) {
        self.until(&format!("the pull request reading {state:?}"), || {
            self.store.run(run).unwrap().pr_state == Some(state)
        });
    }
}

#[test]
fn a_saved_workflow_with_a_plain_spec_step_still_gets_its_text_into_the_pull_request() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.delivered();
    let body = w.body();
    assert!(body.contains("Fixed it."), "{body}");
    assert!(
        body.contains(
            "<details>\n<summary>What was asked</summary>\n\nadd returns the sum\n\n</details>"
        ),
        "{body}"
    );
    // After what was done, and before the lists.
    let (done, asked, changed) = (
        body.find("Fixed it.").unwrap(),
        body.find("<details>").unwrap(),
        body.find("### What changed").unwrap(),
    );
    assert!(done < asked && asked < changed, "{body}");
}

const QUESTION: &str = r#"{"id":"Q1","text":"If whole is 0, what then?","options":[{"label":"Show an error","recommended":true},{"label":"Return 0","recommended":false}]}"#;

fn structured(questions: &str) -> String {
    format!(
        r#"{{"summary":"add returns the sum","criteria":[{{"id":"C1","when":"add(1, 2)","then":"it returns 3"}}],"changes":[{{"kind":"changes","text":"add stops subtracting"}}],"limits":["Its signature stays"],"decided":[{{"text":"Keeps the rounding"}}],"questions":[{questions}]}}"#
    )
}

impl World {
    /// The built-in one with nothing but a person between the work and the pull request.
    fn start_simple(&self) -> String {
        let workflow = self
            .engine
            .workflow(&self.project_id, "builtin:simple", "en")
            .unwrap()
            .unwrap();
        self.start(&workflow)
    }
}

#[test]
fn a_structured_answer_reads_as_text_and_the_pull_request_says_what_the_person_answered() {
    keel_harness::needs!(sandbox);
    let w = world();
    std::fs::write(w.flags.join("spec-json"), structured(QUESTION)).unwrap();
    let run = w.start_simple();
    let waiting = w.wait_step(&run, "spec", AttemptState::WaitingAnswers);
    assert_eq!(waiting.role, "specifier");

    // Put together here, so no secret scanner trips on this file.
    let secret = format!("{}{}", "ghp_", "x".repeat(36));
    std::fs::write(w.flags.join("spec-json"), structured("")).unwrap();
    w.engine
        .answer_questions(
            &waiting.id,
            &[keel_engine::Answer {
                question: "Q1".into(),
                choice: keel_engine::AnswerChoice::Own {
                    text: format!("Return 0 with {secret}"),
                },
                remember: false,
            }],
        )
        .unwrap();
    let human = w.wait_step(&run, "approval", AttemptState::WaitingApproval);
    w.engine.answer_approval(&human.id, true, "").unwrap();
    w.until("the run finishing", || {
        w.store.run(&run).unwrap().finished_at.is_some()
    });

    let body = w.body();
    assert!(
        body.starts_with("Fixed it."),
        "a specifier isn't what explains the run: {body}"
    );
    assert!(body.contains("<summary>What was asked</summary>"), "{body}");
    assert!(body.contains("add returns the sum"), "{body}");
    assert!(body.contains("**What has to be true**"), "{body}");
    assert!(body.contains("- C1: add(1, 2) → it returns 3"), "{body}");
    assert!(!body.contains("\"criteria\""), "no JSON: {body}");
    assert!(body.contains("### What the person answered"), "{body}");
    assert!(
        body.contains("- If whole is 0, what then? Return 0 with"),
        "{body}"
    );
    assert!(!body.contains(&secret), "masked: {body}");

    // The step after everything else took the structured answer in its stride.
    let learner = w.step(&run, "learnings").unwrap();
    assert_eq!(learner.state, "succeeded", "{:?}", learner.reason);
}

/// Up to its pull request, with a check that passes and a verifier that says this.
fn delivered_with_verifier(w: &World, verifier_says: &str, language: &str) -> String {
    let spec = r#"{"summary":"add returns the sum","criteria":[{"id":"C1","when":"add(1, 2)","then":"it returns 3"},{"id":"C2","when":"one is negative","then":"it still adds"}],"changes":[],"limits":[],"decided":[],"questions":[]}"#;
    std::fs::write(w.flags.join("spec-json"), spec).unwrap();
    std::fs::write(w.flags.join("verify-json"), verifier_says).unwrap();
    let specifier = WorkflowStep::Agent(StepSpec {
        id: "spec".into(),
        model: "fake".into(),
        effort: None,
        tools: ToolProfile::ReadOnly,
        prompt: "{{request}}".into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        role: StepRole::Specifier,
    });
    let verifier = WorkflowStep::Agent(StepSpec {
        id: "verify".into(),
        model: "fake".into(),
        effort: None,
        tools: ToolProfile::ReadOnly,
        prompt: "{{check}}".into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        role: StepRole::Verifier,
    });
    let steps = vec![
        specifier,
        agent("builder", ToolProfile::Build),
        WorkflowStep::Check {
            id: "check".into(),
            run: "test -f fixed.txt".into(),
        },
        verifier,
        WorkflowStep::Approval { id: "human".into() },
        WorkflowStep::Deliver { id: "pr".into() },
    ];
    let run = w
        .engine
        .start_run(StartRun {
            project_id: w.project_id.clone(),
            title: None,
            request: "Fix the bug".into(),
            workflow: steps,
            saved_workflow: None,
            language: language.into(),
            source_issue: None,
            started_by: None,
        })
        .unwrap();
    let human = w.wait_step(&run, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&human.id, true, "").unwrap();
    w.wait_step(&run, "pr", AttemptState::Succeeded);
    w.body()
}

const BOTH_MET: &str = r#"{"criteria":[{"id":"C1","result":"met","evidence":"tests/calc.rs:1"},{"id":"C2","result":"met","evidence":"tests/calc.rs:2"}],"summary":"Tests prove it"}"#;

#[test]
fn the_pull_request_says_when_everything_was_met() {
    keel_harness::needs!(sandbox);
    let w = world();
    let body = delivered_with_verifier(&w, BOTH_MET, "en");
    assert!(
        body.contains("### What had to be true\n\n2 of 2 met\n\n"),
        "{body}"
    );
    assert!(
        body.contains("- Met: add(1, 2) → it returns 3 (tests/calc.rs:1)\n"),
        "{body}"
    );
    assert!(
        body.contains("- Met: one is negative → it still adds (tests/calc.rs:2)\n"),
        "{body}"
    );
    // After how it was checked, and before the list of steps.
    let (checked, section, steps) = (
        body.find("### How it was checked").unwrap(),
        body.find("### What had to be true").unwrap(),
        body.find("### Steps").unwrap(),
    );
    assert!(checked < section && section < steps, "{body}");
    assert!(!body.contains("\"criteria\""), "no JSON: {body}");
}

#[test]
fn the_pull_request_says_what_is_not_verified_and_never_rounds_it_up() {
    keel_harness::needs!(sandbox);
    let w = world();
    let said = r#"{"criteria":[{"id":"C1","result":"met","evidence":"tests/calc.rs:1"},{"id":"C2","result":"unverified","evidence":""}],"summary":"One has no test"}"#;
    let body = delivered_with_verifier(&w, said, "en");
    assert!(
        body.contains("\n\n1 of 2 met, 1 not verified\n\n"),
        "{body}"
    );
    assert!(
        body.contains(
            "- Not verified: one is negative → it still adds (it found nothing that proves it)\n"
        ),
        "{body}"
    );
}

#[test]
fn the_pull_request_says_plainly_when_nothing_could_be_verified() {
    keel_harness::needs!(sandbox);
    let w = world();
    let said = r#"{"criteria":[{"id":"C1","result":"unverified","evidence":""},{"id":"C2","result":"unverified","evidence":""}],"summary":"No tests"}"#;
    let body = delivered_with_verifier(&w, said, "es");
    assert!(
        body.contains(
            "### Lo que tenía que cumplirse\n\nNo se pudo verificar ninguno de los 2\n\n"
        ),
        "{body}"
    );
    assert!(
        body.contains("- Sin verificar: add(1, 2) → it returns 3 (no encontró"),
        "{body}"
    );
}

#[test]
fn what_a_verifier_points_at_is_masked_like_the_rest_of_the_pull_request() {
    keel_harness::needs!(sandbox);
    let w = world();
    // Put together here, so no secret scanner trips on this file.
    let secret = format!("{}{}", "ghp_", "x".repeat(36));
    let said = format!(
        r#"{{"criteria":[{{"id":"C1","result":"met","evidence":"tests/calc.rs:1 with {secret}"}},{{"id":"C2","result":"unmet","evidence":"{secret}"}}],"summary":"x"}}"#
    );
    let body = delivered_with_verifier(&w, &said, "en");
    assert!(body.contains("- Met: add(1, 2) → it returns 3 ("), "{body}");
    assert!(!body.contains(&secret), "masked: {body}");
}

#[test]
fn delivering_frees_what_the_build_left_in_the_run_copy() {
    keel_harness::needs!(sandbox);
    let w = world();
    let (repo, remote) = (
        w._dir.path().join("app"),
        w._dir.path().join("remotes/acme/app.git"),
    );
    std::fs::write(repo.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "ignore builds"]);
    git(&repo, &["push", "-q", &remote.to_string_lossy(), "main"]);
    std::fs::write(w.flags.join("builds"), "").unwrap();

    let run = w.delivered();
    assert!(w.store.run(&run).unwrap().pr_url.is_some());
    let builder = w.step(&run, "builder").unwrap();
    let workspace = Path::new(&builder.workspace);
    w.until("the build being freed", || {
        ["target", "node_modules", ".keel-cache"]
            .iter()
            .all(|name| !workspace.join(name).exists())
    });
    assert!(workspace.join("fixed.txt").exists());
    assert!(Path::new(&builder.raw_path).exists());
}

#[test]
fn a_workflow_with_no_verifier_has_no_such_section() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.delivered();
    assert!(!w.body().contains("What had to be true"), "{}", w.body());
}

#[test]
fn a_run_where_nobody_was_asked_lists_no_answers() {
    keel_harness::needs!(sandbox);
    let w = world();
    std::fs::write(w.flags.join("spec-json"), structured("")).unwrap();
    let run = w.start_simple();
    let human = w.wait_step(&run, "approval", AttemptState::WaitingApproval);
    w.engine.answer_approval(&human.id, true, "").unwrap();
    w.wait_step(&run, "pull-request", AttemptState::Succeeded);
    let body = w.body();
    assert!(!body.contains("What the person answered"), "{body}");
    assert!(body.contains("add returns the sum"), "{body}");
}

#[test]
fn the_simple_built_in_workflow_can_send_work_back_though_it_has_no_approver() {
    keel_harness::needs!(sandbox);
    let w = world();
    std::fs::write(w.flags.join("spec-json"), structured("")).unwrap();
    let run = w.start_simple();
    let first = w.wait_step(&run, "approval", AttemptState::WaitingApproval);

    w.engine.send_back(&first.id, "Return 0 instead").unwrap();
    w.until("the approval coming round again", || {
        w.step(&run, "approval")
            .is_some_and(|a| a.id != first.id && a.state == "waitingApproval")
    });
    let builder = w.step(&run, "builder").unwrap();
    assert_eq!(builder.round, 1);
    assert!(
        builder.prompt.contains("Return 0 instead"),
        "{}",
        builder.prompt
    );
    assert_eq!(w.store.run(&run).unwrap().round, 1);
    let specs = w
        .store
        .attempts_of_run(&run)
        .unwrap()
        .into_iter()
        .filter(|a| a.step_id == "spec")
        .count();
    assert_eq!(specs, 1, "what was asked stands");

    let again = w.step(&run, "approval").unwrap();
    w.engine.answer_approval(&again.id, true, "").unwrap();
    w.wait_step(&run, "pull-request", AttemptState::Succeeded);
    let body = w.body();
    assert!(body.contains("**Round 2**"), "{body}");
    assert!(w.engine.decisions(&w.project_id).unwrap().is_empty());
}

#[test]
fn a_workflow_with_nothing_that_specifies_adds_no_section() {
    keel_harness::needs!(sandbox);
    let w = world();
    let workflow = w.saved(vec![
        agent("builder", ToolProfile::Build),
        WorkflowStep::Approval { id: "human".into() },
        WorkflowStep::Deliver { id: "pr".into() },
    ]);
    let run = w.start(&workflow);
    let human = w.wait_step(&run, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&human.id, true, "").unwrap();
    w.wait_step(&run, "pr", AttemptState::Succeeded);
    let body = w.body();
    assert!(!body.contains("<details>"), "{body}");
    assert!(!body.contains("What was asked"), "{body}");
}

#[test]
fn a_description_too_long_for_github_still_gets_there_and_says_what_was_cut() {
    keel_harness::needs!(sandbox);
    let w = world();
    std::fs::write(w.flags.join("spec-text"), "word ".repeat(20_000)).unwrap();
    let run = w.delivered();
    assert_eq!(
        w.store.run(&run).unwrap().pr_number,
        Some(12),
        "{}",
        w.gh_log()
    );
    let body = w.body();
    assert!(body.chars().count() < 65_536, "{}", body.chars().count());
    assert!(body.contains("Fixed it."), "what the agents wrote stays");
    assert!(body.contains("### What changed"), "and so does the rest");
    assert!(
        body.contains("Cut here to fit GitHub's limit"),
        "it says so"
    );
    assert!(body.contains("</details>"));
}

#[test]
fn a_cut_leaves_no_half_a_secret_in_the_pull_request() {
    keel_harness::needs!(sandbox);
    let w = world();
    // Long enough that the cut has to land somewhere among them.
    let token = format!("{}{}", "ghp_", "x".repeat(16));
    std::fs::write(
        w.flags.join("spec-text"),
        format!("{token} ").repeat(12_000),
    )
    .unwrap();
    w.delivered();
    let body = w.body();
    assert!(
        body.contains("Cut here to fit GitHub's limit"),
        "it was cut"
    );
    assert!(!body.contains("ghp_"), "{}", &body[..200]);
}

#[test]
fn what_it_just_opened_reads_as_a_draft_without_asking_github() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    let found = w.store.run(&run).unwrap();
    assert_eq!(found.pr_state, Some(PrState::Draft));
    assert!(found.pr_state_at.is_some());
    assert!(found.pr_checked_at.is_some());
    assert_eq!(w.asked_about_prs(), 0);
}

#[test]
fn opening_a_run_finds_out_what_became_of_its_pull_request_and_only_reads() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    w.forget_looking(&run);
    // The folder points elsewhere by now, where that number is someone else's pull request.
    w.store
        .set_project_repo(&w.project_id, Some("other/repo"))
        .unwrap();
    w.set_pr_view("MERGED", false, "2026-11-06T10:00:00Z");
    let told = w.changes.lock().unwrap().len();

    w.engine.look_at_pr(&run);
    w.wait_pr_state(&run, PrState::Merged);

    let found = w.store.run(&run).unwrap();
    assert_eq!(found.pr_state_at.as_deref(), Some("2026-11-06T10:00:00Z"));
    w.until("the app hearing of it", || {
        w.changes.lock().unwrap()[told..].contains(&run)
    });
    let gh = w.gh_log();
    assert!(
        gh.contains(
            "gh pr view 12 --repo acme/app --json state,isDraft,mergedAt,closedAt,updatedAt,headRefOid"
        ),
        "{gh}"
    );
    let on_prs: Vec<&str> = gh.lines().filter(|l| l.starts_with("gh pr ")).collect();
    assert!(
        on_prs
            .iter()
            .all(|l| l.starts_with("gh pr view") || l.starts_with("gh pr create")),
        "after opening it, nothing writes to it: {on_prs:?}"
    );
}

#[test]
fn opening_a_run_again_right_away_or_once_merged_asks_nothing_more() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    w.set_pr_view("OPEN", false, "2026-11-06T09:00:00Z");

    // It only just opened it.
    w.engine.look_at_pr(&run);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(w.asked_about_prs(), 0);

    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.wait_pr_state(&run, PrState::Open);
    w.engine.look_at_pr(&run);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(w.asked_about_prs(), 1, "it looked a moment ago");

    w.set_pr_view("MERGED", false, "2026-11-06T10:00:00Z");
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.wait_pr_state(&run, PrState::Merged);
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(w.asked_about_prs(), 2, "merged can't change");
}

#[test]
fn a_failed_look_stays_quiet_and_keeps_what_was_known() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    let before = w.store.run(&run).unwrap();
    let attempts_before = w.store.attempts_of_run(&run).unwrap();
    let told = w.changes.lock().unwrap().len();

    // GitHub answers with an error. The fake has no `pr-view` yet.
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("it asking", || w.asked_about_prs() == 1);
    w.until("it counting as having looked", || {
        w.store.run(&run).unwrap().pr_checked_at.is_some()
    });
    std::thread::sleep(Duration::from_millis(100));

    // Then the answer makes no sense, the token can't be had, there's no account, and no `gh`.
    std::fs::write(w.flags.join("pr-view"), "<html>Unicorn!</html>").unwrap();
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("it asking again", || w.asked_about_prs() == 2);
    std::fs::remove_file(w.flags.join("pr-view")).unwrap();
    std::fs::write(w.flags.join("no-token"), "").unwrap();
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("it counting as having looked", || {
        w.store.run(&run).unwrap().pr_checked_at.is_some()
    });
    w.engine
        .update_project(&w.project_id, &account(None))
        .unwrap();
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("it counting as having looked", || {
        w.store.run(&run).unwrap().pr_checked_at.is_some()
    });
    w.engine.set_tools(Tools::default());
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("it counting as having looked", || {
        w.store.run(&run).unwrap().pr_checked_at.is_some()
    });
    std::thread::sleep(Duration::from_millis(100));

    let after = w.store.run(&run).unwrap();
    assert_eq!(after.pr_state, before.pr_state);
    assert_eq!(after.pr_state_at, before.pr_state_at);
    assert_eq!(after.pr_url, before.pr_url);
    assert_eq!(after.finished_at, before.finished_at);
    let states = |attempts: &[keel_store::Attempt]| -> Vec<(String, Option<serde_json::Value>)> {
        attempts
            .iter()
            .map(|a| (a.state.clone(), a.reason.clone()))
            .collect()
    };
    assert_eq!(
        states(&w.store.attempts_of_run(&run).unwrap()),
        states(&attempts_before),
        "nothing about the run changed"
    );
    assert_eq!(w.store.blocked_reasons().unwrap(), vec![]);
    assert_eq!(w.changes.lock().unwrap().len(), told, "and nobody was told");
    let board = w.engine.board(&w.project_id).unwrap();
    assert_eq!(board.paused, None);
}

#[test]
fn a_look_that_changes_nothing_tells_nobody() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    w.set_pr_view("OPEN", false, "2026-11-06T09:00:00Z");
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.wait_pr_state(&run, PrState::Open);
    std::thread::sleep(Duration::from_millis(100));
    let told = w.changes.lock().unwrap().len();

    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("it asking again", || w.asked_about_prs() == 2);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(w.changes.lock().unwrap().len(), told);
}

#[test]
fn a_run_with_no_pull_request_asks_github_nothing() {
    keel_harness::needs!(sandbox);
    let w = world();
    let workflow = w.saved(delivering_steps());
    let run = w.start(&workflow);
    w.wait_step(&run, "human", AttemptState::WaitingApproval);
    let before = w.gh_log();
    w.engine.look_at_pr(&run);
    w.engine.look_at_pr("no-such-run");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(w.gh_log(), before);
    assert_eq!(w.store.run(&run).unwrap().pr_checked_at, None);
}

#[test]
fn a_pull_request_that_was_already_there_is_unknown_until_keel_looks() {
    keel_harness::needs!(sandbox);
    let w = world();
    std::fs::write(w.flags.join("pr-exists"), "").unwrap();
    let run = w.delivered();
    let found = w.store.run(&run).unwrap();
    assert_eq!(found.pr_number, Some(99));
    assert_eq!(found.pr_state, None, "someone may have moved it on already");
    assert_eq!(
        found.pr_checked_at, None,
        "so it's the first to be looked at"
    );
    assert_eq!(w.store.runs_with_pr_due("0", "0", 10).unwrap().len(), 1);

    w.set_pr_view("OPEN", false, "2026-11-06T09:00:00Z");
    w.engine.look_at_pr(&run);
    w.wait_pr_state(&run, PrState::Open);
    assert!(
        w.gh_log().contains("gh pr view 99 --repo acme/app"),
        "{}",
        w.gh_log()
    );
}

#[test]
fn delivering_to_a_pull_request_already_there_forgets_what_its_ci_said() {
    keel_harness::needs!(sandbox);
    let w = world();
    std::fs::write(w.flags.join("pr-exists"), "").unwrap();
    let workflow = w.saved(delivering_steps());
    let run = w.start(&workflow);
    let human = w.wait_step(&run, "human", AttemptState::WaitingApproval);
    // What a look said about the commit before this one.
    w.store
        .patch_run(
            &run,
            &[
                ("pr_ci", Value::Text("failed".into())),
                ("pr_ci_failed", Value::Text(r#"["lint"]"#.into())),
            ],
        )
        .unwrap();
    w.engine.answer_approval(&human.id, true, "").unwrap();
    w.until("the run finishing", || {
        w.store.run(&run).unwrap().finished_at.is_some()
    });
    let found = w.store.run(&run).unwrap();
    assert_eq!(found.pr_number, Some(99));
    assert_eq!(found.pr_ci, None, "it was about another commit");
    assert!(found.pr_ci_failed.is_empty());
}

#[test]
fn the_background_look_follows_a_pull_request_until_it_is_merged() {
    keel_harness::needs!(sandbox);
    let w = world_with(|config| config.pr_look_again = Duration::from_millis(30));
    let run = w.delivered();
    assert_eq!(w.asked_about_prs(), 0, "nothing looks until it's started");
    // Closed just now: one closed long ago is no longer watched.
    let closed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    w.set_pr_view("CLOSED", false, &closed_at);
    w.engine.start_pr_watch();
    w.wait_pr_state(&run, PrState::Closed);
    // A closed one can be reopened, so it stays watched.
    w.set_pr_view("OPEN", true, "2026-11-06T09:30:00Z");
    w.wait_pr_state(&run, PrState::Draft);
    w.set_pr_view("OPEN", false, "2026-11-06T09:40:00Z");
    w.wait_pr_state(&run, PrState::Open);
    w.set_pr_view("MERGED", false, "2026-11-06T10:00:00Z");
    w.wait_pr_state(&run, PrState::Merged);

    std::thread::sleep(Duration::from_millis(100));
    let asked = w.asked_about_prs();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(w.asked_about_prs(), asked, "merged is the end of it");
    let found = w.store.run(&run).unwrap();
    assert_eq!(found.pr_state_at.as_deref(), Some("2026-11-06T10:00:00Z"));
}

#[test]
fn one_closed_long_ago_is_left_alone_until_a_person_opens_its_run() {
    keel_harness::needs!(sandbox);
    let w = world_with(|config| config.pr_look_again = Duration::from_millis(30));
    let run = w.delivered();
    w.set_pr_view("CLOSED", false, "2020-01-01T00:00:00Z");
    w.engine.start_pr_watch();
    w.wait_pr_state(&run, PrState::Closed);

    std::thread::sleep(Duration::from_millis(100));
    let asked = w.asked_about_prs();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(w.asked_about_prs(), asked, "it stopped asking");

    // Opening it still looks, and that's how a reopened one comes back.
    w.set_pr_view("OPEN", false, "2026-11-06T09:00:00Z");
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.wait_pr_state(&run, PrState::Open);
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        w.asked_about_prs() > asked + 1,
        "open again, so the background looks again"
    );
}

#[test]
fn the_background_look_survives_a_github_that_keeps_failing() {
    keel_harness::needs!(sandbox);
    let w = world_with(|config| config.pr_look_again = Duration::from_millis(30));
    let run = w.delivered();
    w.engine.start_pr_watch();
    w.until("it asking a few times", || w.asked_about_prs() >= 3);
    assert_eq!(w.store.run(&run).unwrap().pr_state, Some(PrState::Draft));
    w.set_pr_view("MERGED", false, "2026-11-06T10:00:00Z");
    w.wait_pr_state(&run, PrState::Merged);
}

#[test]
fn a_card_shows_what_became_of_its_pull_request_and_stays_done_either_way() {
    keel_harness::needs!(sandbox);
    let w = world();
    let workflow = w.saved(delivering_steps());
    let run = w
        .engine
        .start_run_on_board(StartRun {
            project_id: w.project_id.clone(),
            title: None,
            request: "Fix the bug".into(),
            workflow: workflow.steps.clone(),
            saved_workflow: Some(workflow.reference()),
            language: "en".into(),
            source_issue: None,
            started_by: None,
        })
        .unwrap();
    let human = w.wait_step(&run, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&human.id, true, "").unwrap();
    w.until("the run finishing", || {
        w.store.run(&run).unwrap().finished_at.is_some()
    });

    let card = || w.engine.board(&w.project_id).unwrap().cards.remove(0);
    assert_eq!(card().column, BoardColumn::Done);
    assert_eq!(card().pr_state, Some(PrState::Draft));

    for (github, state) in [("CLOSED", PrState::Closed), ("MERGED", PrState::Merged)] {
        w.set_pr_view(github, false, "2026-11-06T10:00:00Z");
        w.forget_looking(&run);
        w.engine.look_at_pr(&run);
        w.wait_pr_state(&run, state);
        assert_eq!(card().column, BoardColumn::Done);
        assert_eq!(card().pr_state, Some(state));
        assert_eq!(card().pr_state_at.as_deref(), Some("2026-11-06T10:00:00Z"));
    }
    let as_sent = serde_json::to_value(card()).unwrap();
    assert_eq!(as_sent["prState"], "merged");
    assert_eq!(as_sent["prStateAt"], "2026-11-06T10:00:00Z");
}

/// An open pull request whose CI says this, as `gh pr view --json …,statusCheckRollup` does.
fn pr_view_with_checks(checks: &str) -> String {
    format!(
        r#"{{"state":"OPEN","isDraft":false,"mergedAt":null,"closedAt":null,"updatedAt":"2026-11-06T09:00:00Z","statusCheckRollup":[{checks}]}}"#
    )
}

const LINT_FAILED: &str = r#"{"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"FAILURE"},{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS"}"#;

#[test]
fn a_pull_request_whose_ci_fails_says_so_on_its_run_and_its_card() {
    keel_harness::needs!(sandbox);
    let w = world();
    let workflow = w.saved(delivering_steps());
    let run = w
        .engine
        .start_run_on_board(StartRun {
            project_id: w.project_id.clone(),
            title: None,
            request: "Fix the bug".into(),
            workflow: workflow.steps.clone(),
            saved_workflow: Some(workflow.reference()),
            language: "en".into(),
            source_issue: None,
            started_by: None,
        })
        .unwrap();
    let human = w.wait_step(&run, "human", AttemptState::WaitingApproval);
    w.engine.answer_approval(&human.id, true, "").unwrap();
    w.until("the run finishing", || {
        w.store.run(&run).unwrap().finished_at.is_some()
    });
    assert_eq!(w.store.run(&run).unwrap().pr_ci, None, "nobody has looked");
    let ci = || {
        let found = w.store.run(&run).unwrap();
        (found.pr_ci, found.pr_ci_failed)
    };

    std::fs::write(w.flags.join("pr-view"), pr_view_with_checks(LINT_FAILED)).unwrap();
    w.forget_looking(&run);
    let told = w.changes.lock().unwrap().len();
    w.engine.look_at_pr(&run);
    w.until("the CI reading failed", || ci().0 == Some(CiState::Failed));
    assert_eq!(ci().1, ["lint"]);
    w.until("the app hearing of it", || {
        w.changes.lock().unwrap()[told..].contains(&run)
    });
    let card = w.engine.board(&w.project_id).unwrap().cards.remove(0);
    assert_eq!(card.pr_ci, Some(CiState::Failed));
    let as_sent = serde_json::to_value(card).unwrap();
    assert_eq!(as_sent["prCi"], "failed");
    assert_eq!(as_sent["prCiFailed"], serde_json::json!(["lint"]));

    // Fixed and pushed: the next look says so.
    let passed = LINT_FAILED.replace("FAILURE", "SUCCESS");
    std::fs::write(w.flags.join("pr-view"), pr_view_with_checks(&passed)).unwrap();
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("the CI reading passed", || ci().0 == Some(CiState::Passed));
    assert!(ci().1.is_empty());

    // An answer that doesn't say what the CI says leaves it unknown, since it may be about another
    // commit by now.
    w.set_pr_view("OPEN", false, "2026-11-06T09:30:00Z");
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("the new time arriving", || {
        w.store.run(&run).unwrap().pr_state_at.as_deref() == Some("2026-11-06T09:30:00Z")
    });
    assert_eq!(ci(), (None, Vec::new()));
}

#[test]
fn a_new_commit_read_without_its_checks_leaves_the_ci_unknown() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    let view = |head: &str, checks: &str| {
        format!(
            r#"{{"state":"OPEN","isDraft":false,"updatedAt":"2026-11-06T09:00:00Z","headRefOid":"{head}"{checks}}}"#
        )
    };
    std::fs::write(
        w.flags.join("pr-view"),
        view("aaa", &format!(r#","statusCheckRollup":[{LINT_FAILED}]"#)),
    )
    .unwrap();
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("the CI reading failed", || {
        w.store.run(&run).unwrap().pr_ci == Some(CiState::Failed)
    });

    // Someone pushes, and this time the checks can't be read.
    std::fs::write(w.flags.join("no-checks"), "").unwrap();
    std::fs::write(w.flags.join("pr-view"), view("bbb", "")).unwrap();
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.until("the new commit arriving", || {
        w.store.run(&run).unwrap().pr_head.as_deref() == Some("bbb")
    });
    let found = w.store.run(&run).unwrap();
    assert_eq!(found.pr_ci, None, "what failed was another commit");
    assert!(found.pr_ci_failed.is_empty());
}

#[test]
fn a_token_that_cannot_read_the_checks_still_follows_the_pull_request() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    std::fs::write(w.flags.join("no-checks"), "").unwrap();
    w.set_pr_view("MERGED", false, "2026-11-06T10:00:00Z");
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.wait_pr_state(&run, PrState::Merged);
    let found = w.store.run(&run).unwrap();
    assert_eq!(found.pr_state_at.as_deref(), Some("2026-11-06T10:00:00Z"));
    assert_eq!(found.pr_ci, None, "what its CI says stays unknown");
    let asked: Vec<String> = w
        .gh_log()
        .lines()
        .filter(|line| line.starts_with("gh pr view"))
        .map(str::to_owned)
        .collect();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(!asked[1].contains("statusCheckRollup"), "{asked:?}");
}

#[test]
fn a_run_remembers_which_workflow_it_used_and_results_count_by_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    let found = w.store.run(&run).unwrap();
    let workflow_id = found.workflow_id.clone().unwrap();
    assert_eq!(found.workflow_name.as_deref(), Some("Mine"));

    let results = w.engine.workflow_results(&w.project_id).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].workflow_id, workflow_id);
    assert_eq!(results[0].runs, 1);
    assert_eq!(results[0].finished, 1);
    assert_eq!(results[0].merged, 0, "it's still a draft");
    assert_eq!(results[0].cost_per_merged_usd, None);

    w.set_pr_view("MERGED", false, "2026-11-06T10:00:00Z");
    w.forget_looking(&run);
    w.engine.look_at_pr(&run);
    w.wait_pr_state(&run, PrState::Merged);
    let results = w.engine.workflow_results(&w.project_id).unwrap();
    assert_eq!(results[0].merged, 1);
    assert_eq!(
        results[0].cost_per_merged_usd,
        Some(1.0),
        "two agents at 0.5"
    );
    assert_eq!(results[0].average_rounds, Some(0.0));
}

#[test]
fn a_cards_run_remembers_the_workflow_it_started_with_and_a_single_step_remembers_none() {
    keel_harness::needs!(sandbox);
    let w = world();
    let workflow = w.saved(vec![agent("spec", ToolProfile::ReadOnly)]);
    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                github_account: Some("alice".into()),
                default_workflow: Some(workflow.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let card = w.engine.card_create(&w.project_id, "Fix it", "").unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    w.engine.board_tick().unwrap();
    let run = w.store.card(&card).unwrap().run_id.unwrap();
    let found = w.store.run(&run).unwrap();
    assert_eq!(found.workflow_id.as_deref(), Some(workflow.id.as_str()));
    assert_eq!(found.workflow_name.as_deref(), Some("Mine"));
    w.wait_step(&run, "spec", AttemptState::Succeeded);

    let single = w
        .engine
        .start_run(StartRun::one_step(
            w.project_id.clone(),
            StepSpec {
                id: "one".into(),
                model: "fake".into(),
                effort: None,
                tools: ToolProfile::ReadOnly,
                prompt: "Look".into(),
                max_turns: None,
                max_budget_usd: None,
                paths: vec![],
                when: None,
                role: Default::default(),
            },
            "en".into(),
        ))
        .unwrap();
    assert_eq!(w.store.run(&single).unwrap().workflow_id, None);
}

impl World {
    /// Someone merges a commit on GitHub, as happens while a run waits for a person.
    fn merged_upstream(&self) -> String {
        let repo = self._dir.path().join("app");
        std::fs::write(repo.join("later.md"), "merged meanwhile\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "merged meanwhile"]);
        let remote = self._dir.path().join("remotes/acme/app.git");
        git(&repo, &["push", "-q", &remote.to_string_lossy(), "main"]);
        git_out(&repo, &["rev-parse", "HEAD"])
    }

    fn keel_base(&self) -> String {
        let mirror = self
            ._dir
            .path()
            .join("data/projects")
            .join(&self.project_id)
            .join("mirror.git");
        git_out(&mirror, &["rev-parse", "refs/heads/keel-base"])
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

#[test]
fn a_run_still_delivers_after_another_run_of_its_project_starts_from_newer_code() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.start(&w.saved(delivering_steps()));
    let human = w.wait_step(&run, "human", AttemptState::WaitingApproval);

    let newer = w.merged_upstream();
    let other = w.start(&w.saved(vec![agent("look", ToolProfile::ReadOnly)]));
    w.until("the other run finishing", || {
        w.store.run(&other).unwrap().finished_at.is_some()
    });
    assert_eq!(
        w.keel_base(),
        newer,
        "the other run started from what's on GitHub now"
    );

    w.engine.answer_approval(&human.id, true, "").unwrap();
    w.until("delivery ending", || {
        w.step(&run, "pr").is_some_and(|a| a.finished_at.is_some())
    });
    let pr = w.step(&run, "pr").unwrap();
    assert_eq!(pr.state, "succeeded", "{:?}", pr.reason);
    assert!(w.store.run(&run).unwrap().pr_url.is_some());
    // Its title still comes from its own first commit.
    assert!(w.gh_log().contains("--title fix "), "{}", w.gh_log());
}
