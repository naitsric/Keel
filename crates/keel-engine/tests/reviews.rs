//! Parallel reviews, the approver, and rounds of rework, against a fake `claude` that answers as
//! each step's prompt asks. The structured answers have the shape M0 captured from the real one.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, Reason, StepRole, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, Error, StartRun, Workflow};
use keel_harness::Tools;
use keel_store::Store;

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
    log: PathBuf,
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

/// The prompt says who this is. Approvers take their decisions from a file, one line per call,
/// and approve when it runs out. A reviewer's `sleep` proves reviews really run at once.
fn fake(log: &Path, answers: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true}}'; exit 0;;
esac
prompt="$(cat)"
tools=""; prev=""
for a in "$@"; do [ "$prev" = "--tools" ] && tools="$a"; prev="$a"; done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
case "$*" in *--json-schema*) list="$list\",\"StructuredOutput";; esac
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
role=$(printf '%s' "$prompt" | head -1)
echo "$(date +%s%N) START $role" >> "{log}/calls.log"
{{ echo "=== $role"; echo "$prompt"; }} >> "{log}/prompts.log"
case "$role" in
BROKEN-GIT*)
  printf '[filter "bad"]\\n\\tclean = false\\n\\trequired = true\\n' >> .git/config
  echo '* filter=bad' > .gitattributes
  echo changed >> README.md
  result='{{"type":"result","subtype":"success","is_error":false,"result":"Done.","num_turns":1}}';;
REVERT*)
  git rm -q built.tsx && git commit -qm "revert"
  result='{{"type":"result","subtype":"success","is_error":false,"result":"Took it back.","num_turns":1}}';;
BUILD*)
  echo "$role" >> built.tsx
  git add -A && git commit -qm "build: $role"
  result='{{"type":"result","subtype":"success","is_error":false,"result":"Built it.","num_turns":1,"total_cost_usd":0.01}}';;
REVIEW-NOANSWER*)
  result='{{"type":"result","subtype":"success","is_error":false,"result":"I looked.","num_turns":1}}';;
REVIEW*)
  sleep 0.4
  result='{{"type":"result","subtype":"success","is_error":false,"result":"","num_turns":1,"total_cost_usd":0.01,"structured_output":{{"verdict":"changes","findings":["a.py:1 finding"],"summary":"one thing to fix"}}}}';;
APPROVE*)
  decision=approve
  if [ -s "{answers}" ]; then decision=$(head -1 "{answers}"); sed -i '' 1d "{answers}"; fi
  result="{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"num_turns\":1,\"structured_output\":{{\"decision\":\"$decision\",\"risk\":\"low\",\"reasons\":\"because\",\"instructions\":\"FIX-THE-FINDING\"}}}}";;
*)
  result='{{"type":"result","subtype":"success","is_error":false,"result":"OK","num_turns":1}}';;
esac
echo "$(date +%s%N) END $role" >> "{log}/calls.log"
echo "$result"
"##,
        log = log.display(),
        answers = answers.display(),
    )
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let log = dir.path().join("log");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&log).unwrap();
    let path = bin.join("claude");
    std::fs::write(&path, fake(&log, &dir.path().join("answers"))).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "x\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    config.max_agents = 5;
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let project_id = engine.add_project(&repo).unwrap().id;
    World {
        dir,
        store,
        engine,
        project_id,
        log,
    }
}

fn spec(id: &str, tools: ToolProfile, role: StepRole, paths: &[&str], prompt: &str) -> StepSpec {
    StepSpec {
        id: id.into(),
        model: "fake".into(),
        effort: None,
        tools,
        prompt: prompt.into(),
        max_turns: None,
        max_budget_usd: None,
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        role,
    }
}

const SCREENS: &[&str] = &["**/*.tsx"];
const NO_MATCH: &[&str] = &["**/*.nothing"];

fn workflow(max_rounds: u32) -> Vec<WorkflowStep> {
    vec![
        WorkflowStep::Agent(spec(
            "builder",
            ToolProfile::Build,
            StepRole::Worker,
            &[],
            "BUILD\n{{rework}}",
        )),
        WorkflowStep::Parallel {
            id: "reviews".into(),
            steps: vec![
                spec(
                    "security",
                    ToolProfile::ReadOnly,
                    StepRole::Reviewer,
                    &[],
                    "REVIEW security\nDIFF:{{diff}}\n{{rework}}",
                ),
                spec(
                    "ux",
                    ToolProfile::ReadOnly,
                    StepRole::Reviewer,
                    SCREENS,
                    "REVIEW ux",
                ),
                spec(
                    "nothing",
                    ToolProfile::ReadOnly,
                    StepRole::Reviewer,
                    NO_MATCH,
                    "REVIEW nothing",
                ),
            ],
        },
        WorkflowStep::Agent(spec(
            "approver",
            ToolProfile::ReadOnly,
            StepRole::Approver {
                rework_to: "builder".into(),
                max_rounds,
            },
            &[],
            "APPROVE\nsecurity={{steps.security.output}}\nnothing={{steps.nothing.output}}",
        )),
        WorkflowStep::Approval { id: "human".into() },
    ]
}

impl World {
    fn start(&self, steps: Vec<WorkflowStep>) -> String {
        self.engine
            .start_run(StartRun {
                project_id: self.project_id.clone(),
                title: None,
                request: "x".into(),
                workflow: steps,
                language: "en".into(),
                source_issue: None,
            })
            .unwrap()
    }

    fn answers(&self, lines: &[&str]) {
        std::fs::write(self.dir.path().join("answers"), lines.join("\n") + "\n").unwrap();
    }

    fn all(&self, run: &str, id: &str) -> Vec<keel_store::Attempt> {
        self.store
            .attempts_of_run(run)
            .unwrap()
            .into_iter()
            .filter(|a| a.step_id == id)
            .collect()
    }

    fn wait(&self, run: &str, id: &str, state: AttemptState) -> keel_store::Attempt {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(a) = self.all(run, id).pop()
                && a.state == state.as_str()
            {
                return a;
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

    fn calls(&self) -> String {
        std::fs::read_to_string(self.log.join("calls.log")).unwrap_or_default()
    }

    fn prompts(&self) -> String {
        std::fs::read_to_string(self.log.join("prompts.log")).unwrap_or_default()
    }
}

#[test]
fn reviews_run_at_once_and_the_approver_gets_their_answers() {
    let w = world();
    let run = w.start(workflow(2));
    w.wait(&run, "human", AttemptState::WaitingApproval);

    // They overlap: the second started before the first ended.
    let calls = w.calls();
    let order: Vec<&str> = calls
        .lines()
        .filter(|l| l.contains("REVIEW"))
        .map(|l| l.split_whitespace().nth(1).unwrap())
        .collect();
    assert_eq!(&order[..2], ["START", "START"], "{calls}");

    // Each answer is kept, and reaches the approver.
    let security = w.all(&run, "security").pop().unwrap();
    assert_eq!(security.role, "reviewer");
    assert_eq!(security.output.as_ref().unwrap()["verdict"], "changes");
    let prompts = w.prompts();
    assert!(
        prompts.contains(r#"security={"findings":["a.py:1 finding"]"#),
        "{prompts}"
    );
    let approver = w.all(&run, "approver").pop().unwrap();
    assert_eq!(approver.output.unwrap()["decision"], "approve");

    // Reviewers see the change itself.
    assert!(prompts.contains("+BUILD"), "{prompts}");
}

#[test]
fn a_reviewer_with_nothing_to_look_at_is_skipped_on_the_record() {
    let w = world();
    let run = w.start(workflow(2));
    w.wait(&run, "human", AttemptState::WaitingApproval);

    // The change has a .tsx file, so UX runs. Nothing matches the other one.
    assert_eq!(w.all(&run, "ux").pop().unwrap().state, "succeeded");
    let skipped = w.all(&run, "nothing").pop().unwrap();
    assert_eq!(skipped.state, "skipped");
    let reason: Reason = serde_json::from_value(skipped.reason.clone().unwrap()).unwrap();
    assert_eq!(
        reason,
        Reason::NoMatchingPaths {
            paths: vec!["**/*.nothing".into()]
        }
    );
    assert!(
        w.store
            .events(&skipped.id, 0)
            .unwrap()
            .iter()
            .any(|e| e.kind == "skipped")
    );
    assert!(!w.prompts().contains("=== REVIEW nothing"), "no agent ran");
    assert!(
        w.prompts()
            .contains("nothing=(skipped: nothing it covers changed)")
    );
}

#[test]
fn sending_work_back_fixes_it_and_reviews_everything_again() {
    let w = world();
    w.answers(&["rework"]);
    let run = w.start(workflow(2));
    w.wait(&run, "human", AttemptState::WaitingApproval);

    assert_eq!(w.store.run(&run).unwrap().round, 1);
    let builders = w.all(&run, "builder");
    assert_eq!(builders.iter().map(|a| a.round).collect::<Vec<_>>(), [0, 1]);
    assert!(
        builders[1].prompt.contains("FIX-THE-FINDING"),
        "{}",
        builders[1].prompt
    );
    assert!(!builders[0].prompt.contains("FIX-THE-FINDING"));
    assert_eq!(
        w.all(&run, "security")
            .iter()
            .map(|a| a.round)
            .collect::<Vec<_>>(),
        [0, 1],
        "every review runs again"
    );
    // Reviewers see what was asked, so they don't send back what the approver just requested.
    let reviews = w.all(&run, "security");
    assert!(!reviews[0].prompt.contains("FIX-THE-FINDING"));
    assert!(
        reviews[1].prompt.contains("FIX-THE-FINDING"),
        "{}",
        reviews[1].prompt
    );
    assert_eq!(
        w.all(&run, "ux")
            .iter()
            .map(|a| a.round)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(
        w.all(&run, "approver")
            .iter()
            .map(|a| a.round)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    let first = &w.all(&run, "approver")[0];
    assert!(
        w.store
            .events(&first.id, 0)
            .unwrap()
            .iter()
            .any(|e| e.kind == "rework")
    );
}

#[test]
fn a_step_before_the_rework_point_does_not_run_again() {
    let w = world();
    w.answers(&["rework"]);
    let mut steps = workflow(2);
    steps.insert(
        0,
        WorkflowStep::Agent(spec(
            "spec",
            ToolProfile::ReadOnly,
            StepRole::Worker,
            &[],
            "SPEC",
        )),
    );
    let run = w.start(steps);
    w.wait(&run, "human", AttemptState::WaitingApproval);
    assert_eq!(w.all(&run, "spec").len(), 1);
    assert_eq!(w.all(&run, "builder").len(), 2);
}

#[test]
fn at_the_limit_a_person_decides() {
    let w = world();
    w.answers(&["rework", "rework", "rework", "approve"]);
    let run = w.start(workflow(1));
    let blocked = w.wait(&run, "approver", AttemptState::Blocked);
    let reason: Reason = serde_json::from_value(blocked.reason.clone().unwrap()).unwrap();
    assert_eq!(reason, Reason::ReworkLimit { rounds: 1 });
    assert_eq!(
        w.all(&run, "builder").len(),
        2,
        "one original and one rework"
    );
    assert!(w.all(&run, "human").is_empty());

    // Another round, because the person asked for it.
    w.engine.resolve_rework_limit(&blocked.id, true).unwrap();
    let again = {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let approvers = w.all(&run, "approver");
            if approvers.len() == 3 && approvers[2].state == "blocked" {
                break approvers[2].clone();
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    assert_eq!(w.all(&run, "builder").len(), 3);

    // Or on to their own approval. The approver's answer stays on record.
    w.engine.resolve_rework_limit(&again.id, false).unwrap();
    w.wait(&run, "human", AttemptState::WaitingApproval);
    let overridden = w.store.attempt(&again.id).unwrap();
    assert_eq!(
        overridden.output.as_ref().unwrap()["overriddenByPerson"],
        true
    );
    assert!(
        w.store
            .events(&again.id, 0)
            .unwrap()
            .iter()
            .any(|e| e.kind == "reworkLimitAnswer")
    );
}

#[test]
fn only_that_situation_can_be_resolved_that_way() {
    let w = world();
    let run = w.start(workflow(2));
    let approver = w.wait(&run, "approver", AttemptState::Succeeded);
    assert!(matches!(
        w.engine.resolve_rework_limit(&approver.id, true),
        Err(Error::InvalidState)
    ));
}

#[test]
fn a_reviewer_without_its_answer_stops_the_run() {
    let w = world();
    let steps = vec![
        WorkflowStep::Agent(spec(
            "builder",
            ToolProfile::Build,
            StepRole::Worker,
            &[],
            "BUILD",
        )),
        WorkflowStep::Parallel {
            id: "reviews".into(),
            steps: vec![spec(
                "silent",
                ToolProfile::ReadOnly,
                StepRole::Reviewer,
                &[],
                "REVIEW-NOANSWER",
            )],
        },
        WorkflowStep::Approval { id: "human".into() },
    ];
    let run = w.start(steps);
    let failed = w.wait(&run, "silent", AttemptState::Failed);
    let reason: Reason = serde_json::from_value(failed.reason.unwrap()).unwrap();
    assert_eq!(reason, Reason::NoStructuredOutput);
    assert!(w.all(&run, "human").is_empty());
}

#[test]
fn an_invalid_workflow_is_refused_before_anything_runs() {
    let w = world();
    let error = w
        .engine
        .start_run(StartRun {
            project_id: w.project_id.clone(),
            title: None,
            request: "x".into(),
            workflow: vec![WorkflowStep::Deliver { id: "pr".into() }],
            language: "en".into(),
            source_issue: None,
        })
        .unwrap_err();
    assert!(matches!(
        error,
        Error::Blocked(Reason::InvalidWorkflow { .. })
    ));
    assert!(w.store.runs(10).unwrap().is_empty());
}

#[test]
fn the_run_diff_covers_every_round() {
    let w = world();
    w.answers(&["rework"]);
    let run = w.start(workflow(2));
    w.wait(&run, "human", AttemptState::WaitingApproval);
    let diff = w.engine.run_diff(&run).unwrap();
    assert!(diff.contains("+BUILD"));
    assert!(diff.matches("+BUILD").count() >= 2, "{diff}");
}

#[test]
fn a_project_keeps_its_own_workflows_beside_the_built_in_ones() {
    let w = world();
    let builtins = w.engine.workflows(&w.project_id, "es").unwrap();
    assert_eq!(
        builtins.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        ["builtin:full", "builtin:simple"]
    );

    let mut mine = builtins[0].clone();
    mine.name = "Mi flujo".into();
    let saved = w.engine.save_workflow(&w.project_id, &mine).unwrap();
    assert_eq!(saved.id, "mi-flujo");
    let all = w.engine.workflows(&w.project_id, "es").unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[2].steps, mine.steps);

    let broken = Workflow {
        id: String::new(),
        name: "x".into(),
        steps: vec![WorkflowStep::Deliver { id: "pr".into() }],
        builtin: false,
    };
    assert!(w.engine.save_workflow(&w.project_id, &broken).is_err());

    w.engine.delete_workflow(&w.project_id, "mi-flujo").unwrap();
    assert_eq!(w.engine.workflows(&w.project_id, "es").unwrap().len(), 2);
}

#[test]
fn retrying_one_failed_review_leaves_the_others() {
    let w = world();
    let steps = vec![
        WorkflowStep::Agent(spec(
            "builder",
            ToolProfile::Build,
            StepRole::Worker,
            &[],
            "BUILD",
        )),
        WorkflowStep::Parallel {
            id: "reviews".into(),
            steps: vec![
                spec(
                    "good",
                    ToolProfile::ReadOnly,
                    StepRole::Reviewer,
                    &[],
                    "REVIEW good",
                ),
                spec(
                    "silent",
                    ToolProfile::ReadOnly,
                    StepRole::Reviewer,
                    &[],
                    "REVIEW-NOANSWER",
                ),
            ],
        },
    ];
    let run = w.start(steps);
    let failed = w.wait(&run, "silent", AttemptState::Failed);
    w.wait(&run, "good", AttemptState::Succeeded);
    w.engine.retry(&failed.id, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while w.all(&run, "silent").len() < 2 || w.all(&run, "silent")[1].state != "failed" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(w.all(&run, "good").len(), 1, "the good one stays");
}

#[test]
fn a_group_can_open_a_workflow_without_stepping_on_itself() {
    let w = world();
    let steps = vec![WorkflowStep::Parallel {
        id: "reviews".into(),
        steps: ["a", "b", "c", "d"]
            .iter()
            .map(|id| {
                spec(
                    id,
                    ToolProfile::ReadOnly,
                    StepRole::Reviewer,
                    &[],
                    "REVIEW opening",
                )
            })
            .collect(),
    }];
    let run = w.start(steps);
    for id in ["a", "b", "c", "d"] {
        w.wait(&run, id, AttemptState::Succeeded);
    }
}

#[test]
fn a_step_whose_changes_cannot_be_read_does_not_count_as_a_success() {
    let w = world();
    let steps = vec![
        WorkflowStep::Agent(spec(
            "builder",
            ToolProfile::Build,
            StepRole::Worker,
            &[],
            "BROKEN-GIT",
        )),
        WorkflowStep::Approval { id: "human".into() },
    ];
    let run = w.start(steps);
    let failed = w.wait(&run, "builder", AttemptState::Failed);
    let reason: Reason = serde_json::from_value(failed.reason.clone().unwrap()).unwrap();
    assert_eq!(reason, Reason::SnapshotFailed);
    assert!(
        w.store
            .events(&failed.id, 0)
            .unwrap()
            .iter()
            .any(|e| e.kind == "snapshotFailed")
    );
    assert!(w.all(&run, "human").is_empty());
}

#[test]
fn a_file_added_and_then_removed_is_not_part_of_what_changed() {
    let w = world();
    let steps = vec![
        WorkflowStep::Agent(spec(
            "builder",
            ToolProfile::Build,
            StepRole::Worker,
            &[],
            "BUILD",
        )),
        WorkflowStep::Agent(spec(
            "cleanup",
            ToolProfile::Build,
            StepRole::Worker,
            &[],
            "REVERT",
        )),
        WorkflowStep::Parallel {
            id: "reviews".into(),
            steps: vec![spec(
                "ux",
                ToolProfile::ReadOnly,
                StepRole::Reviewer,
                SCREENS,
                "REVIEW ux",
            )],
        },
    ];
    let run = w.start(steps);
    let ux = w.wait(&run, "ux", AttemptState::Skipped);
    assert_eq!(
        ux.state, "skipped",
        "nothing on a screen changed, in the end"
    );

    let builder = w.all(&run, "builder").pop().unwrap();
    assert_eq!(builder.run_files, Some(serde_json::json!(["built.tsx"])));
    let cleanup = w.all(&run, "cleanup").pop().unwrap();
    assert_eq!(
        cleanup.files_changed,
        Some(serde_json::json!(["built.tsx"])),
        "that step did touch it"
    );
    assert_eq!(
        cleanup.run_files,
        Some(serde_json::json!([])),
        "the run, as a whole, didn't"
    );
}

#[test]
fn a_step_named_setup_does_not_clash_with_the_projects_own() {
    let w = world();
    let settings = keel_store::ProjectSettings {
        setup_command: Some("true".into()),
        ..Default::default()
    };
    w.engine.update_project(&w.project_id, &settings).unwrap();
    let run = w.start(vec![WorkflowStep::Agent(spec(
        "setup",
        ToolProfile::ReadOnly,
        StepRole::Worker,
        &[],
        "ANYTHING",
    ))]);
    w.wait(&run, "project-setup", AttemptState::Succeeded);
    w.wait(&run, "setup", AttemptState::Succeeded);
}
