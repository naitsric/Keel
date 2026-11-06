//! What a pull request says about what was asked, and finding out what became of it, against a fake
//! `claude`, a fake `gh` and a local bare repo standing in for GitHub.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use keel_core::{
    AttemptState, BoardColumn, CardLane, PrState, StepSpec, ToolProfile, WorkflowStep,
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

/// Read-only steps answer with what's in `spec-text`. Build steps change a file.
fn fake_claude(flags: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true,"subscriptionType":"max","email":"a@example.com"}}'; exit 0;;
esac
cat > /dev/null
tools=""; prev=""
for a in "$@"; do [ "$prev" = "--tools" ] && tools="$a"; prev="$a"; done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
if [ "$tools" = "Read,Grep,Glob" ]; then
  text="$(cat "{flags}/spec-text")"
else
  text="Fixed it."
  echo work >> fixed.txt
  git add -A && git commit -qm "fix"
fi
echo "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"$text\",\"num_turns\":1,\"total_cost_usd\":0.5,\"usage\":{{\"input_tokens\":10,\"output_tokens\":2}},\"modelUsage\":{{\"claude-fake\":{{}}}}}}"
"##,
        flags = flags.display(),
    )
}

/// `pr view` answers with what's in `pr-view`, and fails without it. `pr create` refuses a
/// description GitHub would refuse.
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

#[test]
fn a_workflow_with_nothing_that_specifies_adds_no_section() {
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
            "gh pr view 12 --repo acme/app --json state,isDraft,mergedAt,closedAt,updatedAt"
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
    assert_eq!(w.store.runs_with_pr_due("0", 10).unwrap().len(), 1);

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
fn the_background_look_follows_a_pull_request_until_it_is_merged() {
    let w = world_with(|config| config.pr_look_again = Duration::from_millis(30));
    let run = w.delivered();
    assert_eq!(w.asked_about_prs(), 0, "nothing looks until it's started");
    w.set_pr_view("CLOSED", false, "2026-11-06T09:00:00Z");
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
fn the_background_look_survives_a_github_that_keeps_failing() {
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

#[test]
fn a_run_remembers_which_workflow_it_used_and_results_count_by_it() {
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
                role: Default::default(),
            },
            "en".into(),
        ))
        .unwrap();
    assert_eq!(w.store.run(&single).unwrap().workflow_id, None);
}
