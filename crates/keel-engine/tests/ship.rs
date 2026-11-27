//! A step that fixes what the reviews and the verifier found before the approver decides, and a
//! check that runs again if it did, against a fake `claude`, a fake `gh` and a local bare repo
//! standing in for GitHub. No workflow Keel ships has them, so each run here is a workflow of the
//! person's own: most are one Keel ships with the two steps put in.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, StepCondition, StepRole, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, Error, StartRun};
use keel_harness::Tools;
use keel_store::{Attempt, ProjectSettings, Store};

struct World {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
    queue: PathBuf,
    log: PathBuf,
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

/// What each one answers can be set in the queue: `verify-<n>.json` and `verdicts`, with one word
/// per review in the order they're asked, approving once it runs out. A step that edits runs
/// `edit-<n>.sh` in the run's copy, and says `said-<n>` if it's there.
fn fake_claude(queue: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true,"subscriptionType":"max","email":"a@example.com"}}'; exit 0;;
esac
prompt="$(cat)"
tools=""; schema=""; model=""; prev=""
for a in "$@"; do
  [ "$prev" = "--tools" ] && tools="$a"
  [ "$prev" = "--json-schema" ] && schema="$a"
  [ "$prev" = "--model" ] && model="$a"
  prev="$a"
done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
[ -n "$schema" ] && list="$list\",\"StructuredOutput"
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
result() {{
  printf '%s\n' "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"num_turns\":1,\"modelUsage\":{{\"$model\":{{}}}},\"structured_output\":$1}}"
}}
case "$schema" in
*unverified*)
  n=$(cat "{queue}/verified" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "{queue}/verified"
  output='{{"criteria":[{{"id":"C1","result":"met","evidence":"tests/calc.rs:1"}}],"summary":"Tests prove it"}}'
  [ -f "{queue}/verify-$n.json" ] && output="$(cat "{queue}/verify-$n.json")"
  result "$output"
  exit 0;;
*criteria*)
  output='{{"size":"normal","risks":[],"summary":"add returns the sum","criteria":[{{"id":"C1","when":"add(1, 2)","then":"it returns 3"}}],"changes":[],"limits":[],"decided":[],"questions":[]}}'
  [ -f "{queue}/spec.json" ] && output="$(cat "{queue}/spec.json")"
  result "$output"
  exit 0;;
*decision*)
  decision=approve
  if [ -s "{queue}/decisions" ]; then decision=$(head -1 "{queue}/decisions"); sed -i '' 1d "{queue}/decisions"; fi
  result "{{\"decision\":\"$decision\",\"risk\":\"low\",\"reasons\":\"because\",\"instructions\":\"FROM-THE-APPROVER\"}}"
  exit 0;;
*verdict*)
  mkdir "{queue}/lock" 2>/dev/null; while [ $? -ne 0 ]; do sleep 0.02; mkdir "{queue}/lock" 2>/dev/null; done
  verdict=approved
  if [ -s "{queue}/verdicts" ]; then verdict=$(head -1 "{queue}/verdicts"); sed -i '' 1d "{queue}/verdicts"; fi
  rmdir "{queue}/lock"
  result "{{\"verdict\":\"$verdict\",\"findings\":[{{\"file\":\"built.txt\",\"line\":1,\"text\":\"A-REAL-FINDING\",\"breaks\":null}}],\"summary\":\"Looked\",\"unread\":[],\"unchecked\":[]}}"
  exit 0;;
*learnings*)
  result '{{"learnings":[]}}'
  exit 0;;
esac
said="Built it."
if [ "$tools" != "Read,Grep,Glob" ]; then
  n=$(cat "{queue}/edited" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "{queue}/edited"
  if [ -f "{queue}/edit-$n.sh" ]; then . "{queue}/edit-$n.sh"; else echo "edit $n" >> built.txt; fi
  [ -f "{queue}/said-$n" ] && said="$(cat "{queue}/said-$n")"
  git add -A && git commit -qm "edit $n"
fi
printf '%s\n' "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"$said\",\"num_turns\":1}}"
"##,
        queue = queue.display(),
    )
}

fn fake_gh(log: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1 $2" in
"auth token") echo "tok-SECRET"; exit 0;;
"repo view") echo '{{"defaultBranchRef":{{"name":"main"}},"viewerPermission":"WRITE","isPrivate":true}}'; exit 0;;
"api repos/acme/app/branches/main") echo false; exit 0;;
"pr create")
  prev=""
  for a in "$@"; do [ "$prev" = "--body-file" ] && cp "$a" "{log}/pr-body.md"; prev="$a"; done
  echo "https://github.com/acme/app/pull/12"; exit 0;;
esac
echo "unexpected: $*" >&2; exit 1
"##,
        log = log.display(),
    )
}

/// Passes unless a step leaves a file called `broken`.
const CHECK: &str = "test -f built.txt && test ! -f broken";

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let queue = dir.path().join("queue");
    let log = dir.path().join("log");
    for path in [&bin, &queue, &log] {
        std::fs::create_dir(path).unwrap();
    }
    write_script(&bin.join("claude"), &fake_claude(&queue));
    write_script(&bin.join("gh"), &fake_gh(&log));

    let remotes = dir.path().join("remotes");
    let remote = remotes.join("acme/app.git");
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
    config.max_agents = 5;
    config.github_git_base = format!("file://{}", remotes.display());
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let project_id = engine.add_project(&repo).unwrap().id;
    engine
        .update_project(
            &project_id,
            &ProjectSettings {
                github_account: Some("alice".into()),
                check_command: Some(CHECK.into()),
                ..Default::default()
            },
        )
        .unwrap();
    World {
        _dir: dir,
        store,
        engine,
        project_id,
        queue,
        log,
    }
}

impl World {
    /// One of the workflows Keel ships with the two steps after its reviews, and an approver that
    /// reads what the fix said.
    fn start(&self, workflow: &str) -> String {
        let mut steps = self
            .engine
            .workflow(&self.project_id, workflow, "en")
            .unwrap()
            .unwrap()
            .steps;
        let reviews = steps
            .iter()
            .position(|step| step.id() == "reviews")
            .unwrap();
        let fix = WorkflowStep::Agent(StepSpec {
            when: Some(StepCondition::SomethingToFix),
            prompt: "ship {{steps.security.output}} {{rework}}".into(),
            ..agent("ship", ToolProfile::Build, StepRole::Worker)
        });
        steps.splice(reviews + 1..reviews + 1, [fix, check("check-ship", true)]);
        for step in &mut steps {
            if let WorkflowStep::Agent(spec) = step
                && matches!(spec.role, StepRole::Approver { .. })
            {
                spec.prompt.push_str("\n\nship: {{steps.ship.output}}");
            }
        }
        self.start_own(steps).unwrap()
    }

    /// Steps of a workflow of the person's own.
    fn start_own(&self, steps: Vec<WorkflowStep>) -> Result<String, Error> {
        self.engine.start_run(StartRun {
            project_id: self.project_id.clone(),
            title: None,
            request: "Fix add".into(),
            saved_workflow: None,
            workflow: steps,
            language: "en".into(),
            source_issue: None,
            started_by: None,
        })
    }

    fn set(&self, file: &str, text: &str) {
        std::fs::write(self.queue.join(file), text).unwrap();
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
        self.all(run, id)
            .pop()
            .unwrap_or_else(|| panic!("{id} never started"))
    }

    fn reason(&self, run: &str, id: &str) -> String {
        let step = self.step(run, id);
        assert_eq!(step.state, "skipped", "{id}: {:?}", step.reason);
        step.reason.unwrap()["code"].as_str().unwrap().to_owned()
    }

    fn wait(&self, run: &str, id: &str, state: AttemptState, number: i64) -> Attempt {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(a) = self.all(run, id).pop()
                && a.state == state.as_str()
                && a.number >= number
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

    fn waits_for_a_person(&self, run: &str) -> Attempt {
        self.wait(run, "human", AttemptState::WaitingApproval, 1)
    }

    /// The description of the pull request, once a person approves.
    fn delivered(&self, run: &str) -> String {
        let approval = self.waits_for_a_person(run);
        self.engine.answer_approval(&approval.id, true, "").unwrap();
        self.wait(run, "pull-request", AttemptState::Succeeded, 1);
        std::fs::read_to_string(self.log.join("pr-body.md")).unwrap()
    }
}

fn agent(id: &str, tools: ToolProfile, role: StepRole) -> StepSpec {
    StepSpec {
        id: id.into(),
        model: "fake".into(),
        effort: None,
        tools,
        prompt: format!("{id} {{{{rework}}}}"),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        role,
        when: None,
        risks: vec![],
        model_when_risky: None,
        skill: None,
    }
}

fn builder() -> WorkflowStep {
    WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker))
}

fn review(id: &str) -> StepSpec {
    agent(id, ToolProfile::ReadOnly, StepRole::Reviewer)
}

fn ship() -> WorkflowStep {
    WorkflowStep::Agent(StepSpec {
        when: Some(StepCondition::SomethingToFix),
        ..agent("ship", ToolProfile::Build, StepRole::Worker)
    })
}

fn check(id: &str, only_if_changed: bool) -> WorkflowStep {
    WorkflowStep::Check {
        id: id.into(),
        run: CHECK.into(),
        only_if_changed,
    }
}

fn approver(rework_to: &str) -> WorkflowStep {
    WorkflowStep::Agent(agent(
        "approver",
        ToolProfile::ReadOnly,
        StepRole::Approver {
            rework_to: rework_to.into(),
            max_rounds: 2,
        },
    ))
}

fn human() -> WorkflowStep {
    WorkflowStep::Approval { id: "human".into() }
}

fn deliver() -> WorkflowStep {
    WorkflowStep::Deliver {
        id: "pull-request".into(),
    }
}

const UNMET: &str =
    r#"{"criteria":[{"id":"C1","result":"unmet","evidence":"calc.py:1"}],"summary":"Not yet"}"#;

#[test]
fn with_nothing_to_fix_neither_it_nor_its_check_runs_and_the_check_still_counts_as_passed() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.start("builtin:full");
    w.waits_for_a_person(&run);
    assert_eq!(w.reason(&run, "ship"), "nothingToFix");
    assert_eq!(w.reason(&run, "check-ship"), "nothingChangedSinceCheck");
    assert_eq!(w.step(&run, "ship").cost_usd, None, "nobody ran");
    let approver = w.step(&run, "approver");
    assert!(
        approver
            .prompt
            .contains("ship: (skipped: no review of this round asked for changes"),
        "{}",
        approver.prompt
    );

    let body = w.delivered(&run);
    let checks: Vec<&str> = body
        .lines()
        .filter(|line| line.contains("test -f built.txt"))
        .collect();
    assert_eq!(
        checks,
        ["- `test -f built.txt && test ! -f broken`: passed"],
        "{body}"
    );
    assert!(!body.contains("changed the code after this"), "{body}");
    assert!(
        !body.contains("**`builder`**"),
        "one step spoke, so it isn't named: {body}"
    );
}

#[test]
fn a_review_that_asks_for_changes_gets_them_fixed_and_checked_again_before_the_approver() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set("verdicts", "changes\n");
    w.set("said-2", "FIXED-IT in built.txt:1");
    let run = w.start("builtin:full");
    w.waits_for_a_person(&run);

    let ship = w.step(&run, "ship");
    assert_eq!(ship.state, "succeeded");
    assert_eq!(ship.tools, "build");
    assert!(ship.prompt.contains("A-REAL-FINDING"), "{}", ship.prompt);
    assert_ne!(ship.sha_before, ship.sha_after, "it left a commit");
    assert_eq!(
        w.step(&run, "check").number,
        1,
        "the first one isn't repeated"
    );
    assert_eq!(w.step(&run, "check-ship").state, "succeeded");
    assert_eq!(w.step(&run, "check-ship").kind, "check");
    let approver = w.step(&run, "approver");
    assert!(
        approver.prompt.contains("FIXED-IT in built.txt:1"),
        "{}",
        approver.prompt
    );
    let order: Vec<String> = w
        .store
        .attempts_of_run(&run)
        .unwrap()
        .into_iter()
        .map(|a| a.step_id)
        .collect();
    let at = |id: &str| order.iter().position(|step| step == id).unwrap();
    assert!(at("ship") < at("check-ship") && at("check-ship") < at("approver"));
    assert_eq!(
        w.store.run(&run).unwrap().round,
        0,
        "without a round for it"
    );

    let body = w.delivered(&run);
    assert!(body.contains("**`builder`**\n\nBuilt it."), "{body}");
    assert!(
        body.contains("**`ship`**\n\nFIXED-IT in built.txt:1"),
        "{body}"
    );
    assert_eq!(
        body.matches("test -f built.txt").count(),
        1,
        "the command once, as it ended: {body}"
    );
    // Under what the verifier said, and under the reviews.
    assert_eq!(
        body.matches("`ship` changed the code after this").count(),
        2,
        "{body}"
    );
}

#[test]
fn something_the_verifier_found_unmet_is_fixed_even_if_every_review_approves() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set("verify-1.json", UNMET);
    let run = w.start("builtin:full");
    w.waits_for_a_person(&run);
    assert_eq!(w.step(&run, "ship").state, "succeeded");
    assert_eq!(w.step(&run, "check-ship").state, "succeeded");
}

#[test]
fn a_fix_that_changes_nothing_still_gets_the_check_again() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set("verdicts", "changes\n");
    w.set("edit-2.sh", "true");
    let run = w.start("builtin:full");
    w.waits_for_a_person(&run);
    let ship = w.step(&run, "ship");
    assert_eq!(ship.sha_before, ship.sha_after);
    // It could have left something uncommitted, so what passed before it says nothing.
    assert_eq!(w.step(&run, "check-ship").state, "succeeded");
    let body = w.delivered(&run);
    assert!(
        !body.contains("changed the code after this"),
        "it didn't: {body}"
    );
}

#[test]
fn a_fix_that_breaks_the_check_goes_back_to_the_builder_with_what_it_printed() {
    keel_harness::needs!(sandbox);
    let w = world();
    // Only the first round's reviews ask for anything.
    w.set("verdicts", "changes\n");
    w.set("edit-2.sh", "touch broken");
    w.set("edit-3.sh", "git rm -q broken");
    let run = w.start("builtin:full");
    w.waits_for_a_person(&run);

    let checks = w.all(&run, "check-ship");
    assert_eq!(checks[0].state, "returned");
    let builders = w.all(&run, "builder");
    assert_eq!(builders.len(), 2);
    assert!(
        builders[1].prompt.contains(
            "Keel ran `test -f built.txt && test ! -f broken` and it ended with exit code 1"
        ),
        "{}",
        builders[1].prompt
    );
    // The next round has nothing to fix, and what passed in it is the first check.
    assert_eq!(w.store.run(&run).unwrap().round, 1);
    assert_eq!(w.reason(&run, "ship"), "nothingToFix");
    assert_eq!(w.reason(&run, "check-ship"), "nothingChangedSinceCheck");
    let body = w.delivered(&run);
    assert!(
        body.contains("- `test -f built.txt && test ! -f broken`: passed"),
        "{body}"
    );
    assert!(!body.contains("did not pass"), "{body}");
}

#[test]
fn a_fix_after_the_check_passed_again_isnt_told_the_check_failed() {
    keel_harness::needs!(sandbox);
    let w = world();
    // Each round has one review that asks for something, of the two that look at this change.
    w.set("verdicts", "changes\napproved\nchanges\n");
    w.set("edit-2.sh", "touch broken");
    w.set("edit-3.sh", "git rm -q broken");
    let run = w.start("builtin:full");
    w.waits_for_a_person(&run);
    let ships: Vec<Attempt> = w
        .all(&run, "ship")
        .into_iter()
        .filter(|a| a.state == "succeeded")
        .collect();
    assert_eq!(ships.len(), 2, "one each round");
    assert!(
        !ships[1].prompt.contains("which is why it's back with you"),
        "{}",
        ships[1].prompt
    );
    assert!(
        w.all(&run, "builder")[1]
            .prompt
            .contains("which is why it's back with you")
    );
}

#[test]
fn what_it_said_in_a_round_isnt_told_as_the_next_rounds_to_whoever_judges_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set("verdicts", "changes\n");
    w.set("said-1", "BUILT-FIRST");
    w.set("said-2", "FIXED-FIRST");
    w.set("said-3", "BUILT-AGAIN");
    w.set("decisions", "rework\n");
    let run = w.start("builtin:full");
    w.waits_for_a_person(&run);
    assert_eq!(w.store.run(&run).unwrap().round, 1);
    let verifiers = w.all(&run, "verify");
    assert_eq!(verifiers.len(), 2);
    assert!(verifiers[1].prompt.contains("BUILT-AGAIN"));
    assert!(
        !verifiers[1].prompt.contains("FIXED-FIRST"),
        "{}",
        verifiers[1].prompt
    );
    assert!(!w.step(&run, "security").prompt.contains("FIXED-FIRST"));
}

#[test]
fn a_small_change_skips_it_too_and_asking_for_the_reviews_lets_it_fix_what_they_find() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set(
        "spec.json",
        r#"{"size":"small","risks":[],"summary":"add returns the sum","criteria":[{"id":"C1","when":"add(1, 2)","then":"it returns 3"}],"changes":[],"limits":[],"decided":[],"questions":[]}"#,
    );
    let run = w.start("builtin:organic");
    let approval = w.waits_for_a_person(&run);
    assert_eq!(w.reason(&run, "security"), "treatedAsSmall");
    assert_eq!(w.reason(&run, "ship"), "nothingToFix");
    assert_eq!(w.reason(&run, "check-ship"), "nothingChangedSinceCheck");
    assert_eq!(
        w.reason(&run, "approver"),
        "treatedAsSmall",
        "a check that only repeats another doesn't keep a change from being small"
    );

    w.set("verdicts", "changes\n");
    w.engine.run_skipped(&approval.id).unwrap();
    w.wait(&run, "human", AttemptState::WaitingApproval, 2);
    let ship = w.step(&run, "ship");
    assert_eq!(ship.state, "succeeded", "{:?}", ship.reason);
    assert_eq!(w.step(&run, "check-ship").state, "succeeded");
    assert_eq!(w.step(&run, "approver").state, "succeeded");
}

#[test]
fn what_a_later_step_judges_isnt_for_it_to_fix() {
    keel_harness::needs!(sandbox);
    let w = world();
    // The first review approves, so it's passed over, and one after it asks for changes.
    w.set("verdicts", "approved\nchanges\n");
    let run = w
        .start_own(vec![
            builder(),
            check("check", false),
            WorkflowStep::Agent(review("first")),
            ship(),
            check("check-ship", true),
            WorkflowStep::Parallel {
                id: "second".into(),
                steps: vec![review("quick"), review("slow")],
            },
            approver("builder"),
            human(),
        ])
        .unwrap();
    w.waits_for_a_person(&run);
    assert_eq!(w.reason(&run, "ship"), "nothingToFix");
    assert_eq!(
        w.all(&run, "ship").len(),
        1,
        "nor is it looked at again while they read the same copy"
    );
    assert_eq!(w.all(&run, "check-ship").len(), 1);
}

#[test]
fn work_sent_back_goes_to_whoever_builds_and_not_to_what_only_fixes() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set("verdicts", "changes\n");
    let run = w
        .start_own(vec![
            builder(),
            check("check", false),
            WorkflowStep::Agent(review("security")),
            ship(),
            check("check-ship", true),
            human(),
        ])
        .unwrap();
    let approval = w.waits_for_a_person(&run);
    w.engine.send_back(&approval.id, "Use the helper").unwrap();
    w.wait(&run, "human", AttemptState::WaitingApproval, 2);
    let builders = w.all(&run, "builder");
    assert_eq!(builders.len(), 2);
    assert!(
        builders[1].prompt.contains("Use the helper"),
        "{}",
        builders[1].prompt
    );

    let refused = w.start_own(vec![
        builder(),
        WorkflowStep::Agent(review("security")),
        ship(),
        approver("ship"),
        human(),
    ]);
    assert!(matches!(
        refused,
        Err(Error::Blocked(keel_core::Reason::InvalidWorkflow { problem, .. })) if problem == "reworkToFixer"
    ));
}

#[test]
fn a_fix_that_failed_to_end_and_was_tried_again_still_says_the_code_changed() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set("verdicts", "changes\n");
    // It commits its fix and then ends badly. Tried again, it has nothing left to do.
    w.set(
        "edit-2.sh",
        "echo fixed >> built.txt; git add -A; git commit -qm fixed; exit 1",
    );
    w.set("edit-3.sh", "true");
    let run = w.start("builtin:full");
    let failed = w.wait(&run, "ship", AttemptState::Failed, 1);
    w.engine.retry(&failed.id, false).unwrap();
    let body = w.delivered(&run);
    assert_eq!(
        body.matches("`ship` changed the code after this").count(),
        2,
        "{body}"
    );
}

#[test]
fn a_check_with_nothing_after_the_fix_says_it_ran_before_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set("verdicts", "changes\n");
    w.set("edit-2.sh", "touch broken");
    let run = w
        .start_own(vec![
            builder(),
            check("check", false),
            WorkflowStep::Agent(review("security")),
            ship(),
            WorkflowStep::Agent(StepSpec {
                when: Some(StepCondition::SomethingToFix),
                ..review("again")
            }),
            human(),
            deliver(),
        ])
        .unwrap();
    let body = w.delivered(&run);
    assert!(
        body.contains(&format!(
            "- `{CHECK}`: passed, before `ship` changed the code"
        )),
        "{body}"
    );
}

#[test]
fn a_review_left_out_for_having_nothing_to_fix_says_so() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w
        .start_own(vec![
            builder(),
            WorkflowStep::Agent(review("security")),
            WorkflowStep::Agent(StepSpec {
                when: Some(StepCondition::SomethingToFix),
                ..review("again")
            }),
            human(),
            deliver(),
        ])
        .unwrap();
    let body = w.delivered(&run);
    assert!(
        body.contains("- `again`: skipped: there was nothing to fix"),
        "{body}"
    );
}

#[test]
fn a_builder_that_changed_nothing_after_the_second_check_failed_doesnt_go_round_again() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.set("verdicts", "changes\n");
    w.set("edit-2.sh", "touch broken");
    w.set("edit-3.sh", "true");
    let run = w.start("builtin:full");
    // The same command, so it's the same check that failed on the same files.
    let check = w.wait(&run, "check", AttemptState::Failed, 2);
    assert_eq!(check.reason.unwrap()["unchanged"], true);
    assert_eq!(w.store.run(&run).unwrap().round, 1);
}
