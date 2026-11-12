//! Adding, setting up and removing projects, with real repos, the real sandbox and a fake `claude`
//! that writes down every time an agent would have spent something.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{BoardColumn, CardLane, Reason, WorkflowStep};
use keel_engine::{
    AddOutcome, Config, Engine, Error, MAX_PROJECT_PROPOSALS, MapStart, ProjectProposal,
    ProposedProject, StartRun, Workflow,
};
use keel_harness::Tools;
use keel_store::{Attempt, NewBrainNote, ProjectSettings, Store};

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    repo: PathBuf,
    agent_log: PathBuf,
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

fn new_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "T"]);
    git(path, &["config", "user.email", "t@example.com"]);
    std::fs::write(path.join("README.md"), "x\n").unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-qm", "init"]);
}

fn world(signed_in: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let agent_log = dir.path().join("agents.log");
    let fake = format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":{signed_in}}}'; exit 0;;
esac
cat > /dev/null
echo agent >> "{log}"
echo '{{"type":"result","subtype":"success","is_error":false,"result":"done","num_turns":1}}'
"##,
        log = agent_log.display(),
    );
    let path = bin.join("claude");
    std::fs::write(&path, fake).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let repo = dir.path().join("app");
    new_repo(&repo);

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[
        bin,
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ]);
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    World {
        dir,
        store,
        engine,
        repo,
        agent_log,
    }
}

impl World {
    fn add(&self) -> String {
        self.engine.add_project(&self.repo).unwrap().id
    }

    fn run(&self, project_id: &str, workflow: Vec<WorkflowStep>) -> String {
        self.engine
            .start_run(StartRun {
                project_id: project_id.to_owned(),
                title: Some("the run".into()),
                request: "x".into(),
                workflow,
                language: "en".into(),
                source_issue: None,
                saved_workflow: None,
                started_by: None,
            })
            .unwrap()
    }

    fn wait_finished(&self, run: &str) {
        let deadline = Instant::now() + Duration::from_secs(40);
        while self.store.run(run).unwrap().finished_at.is_none() {
            assert!(Instant::now() < deadline, "the run never finished");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn agents_run(&self) -> usize {
        std::fs::read_to_string(&self.agent_log)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    fn set_setup(&self, project_id: &str, setup: &str) {
        self.engine
            .update_project(
                project_id,
                &ProjectSettings {
                    setup_command: Some(setup.to_owned()),
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
    }

    fn output(&self, attempt: &Attempt) -> serde_json::Value {
        self.store
            .events(&attempt.id, 0)
            .unwrap()
            .into_iter()
            .find(|event| event.kind == "checkResult")
            .unwrap()
            .data
    }
}

fn check(id: &str, run: &str) -> WorkflowStep {
    WorkflowStep::Check {
        id: id.into(),
        run: run.into(),
    }
}

fn blocked_reason<T: std::fmt::Debug>(result: Result<T, Error>) -> Reason {
    match result {
        Err(Error::Blocked(reason)) => reason,
        other => panic!("expected it to be refused with a reason, got {other:?}"),
    }
}

#[test]
fn a_new_project_says_its_map_started_and_really_starts_it() {
    let w = world(true);
    let added = w.engine.add_project_with_map(&w.repo, "en").unwrap();
    assert_eq!(added.outcome, AddOutcome::New);
    assert_eq!(added.map, Some(MapStart::Started));
    assert_eq!(w.store.runs(10).unwrap().len(), 1);
    assert_eq!(
        serde_json::to_value(&added).unwrap()["map"]["status"],
        "started"
    );
}

#[test]
fn signed_out_the_project_is_added_and_its_map_says_why_not() {
    let w = world(false);
    let added = w.engine.add_project_with_map(&w.repo, "en").unwrap();
    assert_eq!(added.outcome, AddOutcome::New);
    assert_eq!(
        added.map,
        Some(MapStart::NotStarted {
            reason: Reason::ClaudeLoggedOut
        })
    );
    assert_eq!(w.store.projects().unwrap().len(), 1);
    assert!(w.store.runs(10).unwrap().is_empty());
    let shown = serde_json::to_value(&added).unwrap();
    assert_eq!(shown["map"]["status"], "notStarted");
    assert_eq!(shown["map"]["reason"]["code"], "claudeLoggedOut");
}

#[test]
fn what_a_map_failure_quotes_is_hidden_when_shown() {
    let shown = serde_json::to_value(MapStart::NotStarted {
        reason: Reason::PrepareFailed {
            detail: "API_KEY=made-up-Value-123 failed".into(),
        },
    })
    .unwrap();
    let text = shown["reason"]["detail"].as_str().unwrap();
    assert!(!text.contains("made-up-Value-123"), "{text}");
}

#[test]
fn a_folder_already_in_keel_makes_no_claim_and_starts_nothing() {
    let w = world(true);
    let id = w.add();
    let added = w.engine.add_project_with_map(&w.repo, "en").unwrap();
    assert_eq!(added.project.id, id);
    assert_eq!(added.outcome, AddOutcome::Existing);
    assert_eq!(added.map, None);
    assert!(
        w.store.runs(10).unwrap().is_empty(),
        "no map behind its back"
    );
    assert_eq!(w.store.projects().unwrap().len(), 1);
}

#[test]
fn a_subfolder_is_the_same_project_as_its_repo() {
    let w = world(true);
    let id = w.add();
    let inner = w.repo.join("inner");
    std::fs::create_dir(&inner).unwrap();
    assert_eq!(w.engine.add_project(&inner).unwrap().id, id);
}

#[test]
fn a_repo_published_later_is_noticed_when_its_dialog_opens() {
    let w = world(true);
    let id = w.add();
    assert_eq!(w.store.project(&id).unwrap().github_repo, None);
    assert_eq!(
        w.engine.project_setup(&id).unwrap().project.github_repo,
        None
    );

    git(
        &w.repo,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );
    let setup = w.engine.project_setup(&id).unwrap();
    assert_eq!(setup.project.github_repo.as_deref(), Some("acme/app"));
    assert_eq!(
        w.store.project(&id).unwrap().github_repo.as_deref(),
        Some("acme/app"),
        "and it stays known"
    );
}

#[test]
fn a_repo_published_later_is_noticed_when_its_folder_is_added_again() {
    let w = world(true);
    w.add();
    git(
        &w.repo,
        &["remote", "add", "origin", "https://github.com/acme/app.git"],
    );
    let added = w.engine.add_project_with_map(&w.repo, "en").unwrap();
    assert_eq!(added.project.github_repo.as_deref(), Some("acme/app"));
}

#[test]
fn a_repo_that_moved_or_left_github_is_followed() {
    let w = world(true);
    git(
        &w.repo,
        &["remote", "add", "origin", "https://github.com/acme/app.git"],
    );
    let id = w.add();
    git(
        &w.repo,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/renamed.git",
        ],
    );
    assert_eq!(
        w.engine
            .project_setup(&id)
            .unwrap()
            .project
            .github_repo
            .as_deref(),
        Some("acme/renamed")
    );
    git(
        &w.repo,
        &["remote", "set-url", "origin", "https://example.com/a/b.git"],
    );
    assert_eq!(
        w.engine.project_setup(&id).unwrap().project.github_repo,
        None
    );
}

#[test]
fn a_folder_that_is_gone_keeps_what_was_known() {
    let w = world(true);
    git(
        &w.repo,
        &["remote", "add", "origin", "https://github.com/acme/app.git"],
    );
    let id = w.add();
    std::fs::rename(&w.repo, w.dir.path().join("moved")).unwrap();
    let setup = w.engine.project_setup(&id).unwrap();
    assert_eq!(setup.project.github_repo.as_deref(), Some("acme/app"));
    assert_eq!(setup.guess.check, None);
}

#[test]
fn opening_the_dialog_guesses_and_saves_no_command() {
    let w = world(true);
    std::fs::write(w.repo.join("Makefile"), "test:\n\ttrue\n").unwrap();
    std::fs::write(w.repo.join("uv.lock"), "").unwrap();
    git(&w.repo, &["add", "-A"]);
    git(&w.repo, &["commit", "-qm", "commands"]);
    let id = w.add();
    let setup = w.engine.project_setup(&id).unwrap();
    assert_eq!(setup.guess.check.as_deref(), Some("make test"));
    assert_eq!(setup.guess.setup.as_deref(), Some("uv sync --frozen"));
    let saved = w.store.project(&id).unwrap();
    assert_eq!(saved.check_command, None);
    assert_eq!(saved.setup_command, None);
}

#[test]
fn trying_a_check_runs_setup_first_in_the_sandbox_and_spends_nothing() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    let run = w
        .engine
        .try_check(&id, "echo installed > ready.txt", "cat ready.txt", "en")
        .unwrap();
    w.wait_finished(&run);

    let attempts = w.store.attempts_of_run(&run).unwrap();
    assert_eq!(
        attempts.iter().map(|a| a.kind.as_str()).collect::<Vec<_>>(),
        ["setup", "check"]
    );
    assert!(attempts.iter().all(|a| a.state == "succeeded"));
    assert_eq!(w.output(&attempts[1])["stdout"], "installed\n");
    assert_eq!(w.agents_run(), 0, "no agent, so no tokens");
    assert!(attempts.iter().all(|a| {
        a.model_requested.is_empty()
            && a.cost_usd.is_none()
            && a.input_tokens.is_none()
            && a.output_tokens.is_none()
    }));
    // The same sandbox a real check gets.
    let started = w.store.events(&attempts[1].id, 0).unwrap();
    let profile = started.iter().find(|e| e.kind == "started").unwrap();
    assert!(
        profile.data["profile"]
            .as_str()
            .unwrap()
            .contains("(deny network*)")
    );
}

/// #120: the folders in the home folder where a project's programs are, for a person to let them be
/// read. What the commands start, and what the repo's own files say it uses, since `make test`
/// hides what it runs. Nothing runs from the repo and nothing is saved.
#[test]
fn keel_finds_where_in_the_home_folder_the_programs_a_project_uses_are() {
    let w = world(true);
    // Where a version manager would put them: in the home folder, as this build folder is.
    let place = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let bin = place.path().canonicalize().unwrap().join("bin");
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let Ok(inside) = bin.strip_prefix(&home) else {
        keel_harness::testing::skipped("this build folder isn't in the home folder");
        return;
    };
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["pnpm", "node"] {
        keel_harness::testing::write_script(&bin.join(name), "#!/bin/sh\nexit 0\n").unwrap();
    }
    w.engine.set_tools(Tools::from_dirs(&[
        bin.clone(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]));
    std::fs::write(w.repo.join("Makefile"), "test:\n\tpnpm test\n").unwrap();
    std::fs::write(
        w.repo.join("package.json"),
        r#"{"scripts":{"test":"node --test"}}"#,
    )
    .unwrap();
    std::fs::write(w.repo.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
    git(&w.repo, &["add", "-A"]);
    git(&w.repo, &["commit", "-qm", "commands"]);
    let id = w.add();

    let wanted = [keel_harness::toolchains::Found {
        folder: format!("~/{}", inside.display()),
        programs: vec!["pnpm".into(), "node".into()],
    }];
    assert_eq!(w.engine.tool_folders(&id, "", "make test").unwrap(), wanted);
    assert_eq!(
        w.engine
            .tool_folders(&id, "pnpm install", "pnpm test")
            .unwrap(),
        wanted
    );
    assert!(w.store.project(&id).unwrap().allowed_read.is_empty());
}

/// Trying commands with the folders the person is about to let them read, before saving any.
#[test]
fn a_trial_reads_the_folders_being_tried_and_saves_none_of_them() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    let home = std::env::var("HOME").unwrap();
    let rule = format!("(subpath \"{home}/.keel-trial-only\")");
    let profile = |attempt: &Attempt| {
        w.store
            .events(&attempt.id, 0)
            .unwrap()
            .into_iter()
            .find(|event| event.kind == "started")
            .unwrap()
            .data["profile"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let run = w
        .engine
        .try_check_reading(
            &id,
            "true",
            "true",
            &["~/.keel-trial-only".to_owned()],
            "en",
        )
        .unwrap();
    w.wait_finished(&run);
    let attempts = w.store.attempts_of_run(&run).unwrap();
    assert_eq!(attempts.len(), 2);
    for attempt in &attempts {
        assert_eq!(attempt.state, "succeeded", "{}", attempt.kind);
        assert!(profile(attempt).contains(&rule), "{}", attempt.kind);
        assert_eq!(
            attempt.settings["read"],
            serde_json::json!(["~/.keel-trial-only"])
        );
    }
    assert!(w.store.project(&id).unwrap().allowed_read.is_empty());

    let other = w.engine.try_check(&id, "true", "true", "en").unwrap();
    w.wait_finished(&other);
    for attempt in w.store.attempts_of_run(&other).unwrap() {
        assert!(!profile(&attempt).contains(&rule), "{}", attempt.kind);
    }
}

/// #124: trying commands with localhost open, as the setup dialog proposes for Gradle, before the
/// person saves it. Only that run's check gets it, and setup never does.
#[test]
fn a_trial_opens_localhost_to_its_check_and_saves_nothing() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    let profile = |attempt: &Attempt| {
        w.store
            .events(&attempt.id, 0)
            .unwrap()
            .into_iter()
            .find(|event| event.kind == "started")
            .unwrap()
            .data["profile"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let rule = "(local ip \"localhost:*\")";
    let run = w
        .engine
        .try_check_opening(&id, "true", "true", &[], true, "en")
        .unwrap();
    w.wait_finished(&run);
    let attempts = w.store.attempts_of_run(&run).unwrap();
    let kinds: Vec<&str> = attempts.iter().map(|a| a.kind.as_str()).collect();
    assert_eq!(kinds, ["setup", "check"]);
    assert!(!profile(&attempts[0]).contains(rule), "setup stays closed");
    assert!(profile(&attempts[1]).contains(rule));
    assert_eq!(attempts[1].settings["localhost"], true);
    assert!(!w.store.project(&id).unwrap().allow_localhost);

    let other = w.engine.try_check(&id, "true", "true", "en").unwrap();
    w.wait_finished(&other);
    for attempt in w.store.attempts_of_run(&other).unwrap() {
        assert!(!profile(&attempt).contains(rule), "{}", attempt.kind);
    }
}

#[test]
fn trying_saves_nothing() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    let run = w.engine.try_check(&id, "true", "true", "en").unwrap();
    w.wait_finished(&run);
    let project = w.store.project(&id).unwrap();
    assert_eq!(project.check_command, None);
    assert_eq!(project.setup_command, None);
}

#[test]
fn what_is_being_tried_replaces_the_saved_setup() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    w.set_setup(&id, "echo saved > which.txt");
    let run = w
        .engine
        .try_check(&id, "echo typed > which.txt", "cat which.txt", "en")
        .unwrap();
    w.wait_finished(&run);
    let attempts = w.store.attempts_of_run(&run).unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(w.output(attempts.last().unwrap())["stdout"], "typed\n");
}

#[test]
fn emptying_the_setup_field_means_no_setup_when_trying() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    w.set_setup(&id, "echo saved > which.txt");
    let run = w
        .engine
        .try_check(&id, "   ", "test ! -e which.txt", "en")
        .unwrap();
    w.wait_finished(&run);
    let attempts = w.store.attempts_of_run(&run).unwrap();
    assert!(
        attempts.iter().all(|a| a.state == "succeeded"),
        "{attempts:?}"
    );
}

#[test]
fn trying_a_check_works_with_claude_signed_out() {
    keel_harness::needs!(sandbox);
    let w = world(false);
    let id = w.add();
    let run = w.engine.try_check(&id, "", "true", "en").unwrap();
    w.wait_finished(&run);
    assert_eq!(w.store.attempts_of_run(&run).unwrap()[0].state, "succeeded");
}

#[test]
fn a_failed_try_says_why_and_leaves_no_run_open() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    let run = w.engine.try_check(&id, "", "exit 3", "en").unwrap();
    w.wait_finished(&run);
    let attempt = w.store.attempts_of_run(&run).unwrap().pop().unwrap();
    assert_eq!(attempt.state, "failed");
    assert_eq!(attempt.exit_code, Some(3));
    assert_eq!(
        attempt.reason.unwrap(),
        serde_json::json!({ "code": "checkFailed", "exitCode": 3 })
    );
    assert!(w.store.open_runs_of_project(&id).unwrap().is_empty());
    w.engine.remove_project(&id).unwrap();
}

#[test]
fn a_failed_setup_stops_the_try_before_the_check() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    let run = w.engine.try_check(&id, "exit 4", "true", "en").unwrap();
    w.wait_finished(&run);
    let attempts = w.store.attempts_of_run(&run).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].kind, "setup");
    assert_eq!(
        attempts[0].reason.clone().unwrap(),
        serde_json::json!({ "code": "setupFailed", "exitCode": 4 })
    );
}

#[test]
fn retrying_a_failed_try_opens_it_again_and_it_ends_again() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    let run = w
        .engine
        .try_check(&id, "", "test -e later.txt", "en")
        .unwrap();
    w.wait_finished(&run);
    let failed = w.store.attempts_of_run(&run).unwrap().pop().unwrap();
    w.engine.retry(&failed.id, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    while w.store.attempts_of_run(&run).unwrap().len() < 2
        || w.store.run(&run).unwrap().finished_at.is_none()
    {
        assert!(Instant::now() < deadline, "it should end again");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_failed_try_frees_what_a_retry_rebuilds_and_keeps_what_setup_installed() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    std::fs::write(
        w.repo.join(".gitignore"),
        "target/\nnode_modules/\n.venv/\n",
    )
    .unwrap();
    git(&w.repo, &["add", "-A"]);
    git(&w.repo, &["commit", "-qm", "ignore builds"]);
    let id = w.add();
    let run = w
        .engine
        .try_check(
            &id,
            "mkdir -p node_modules/pkg .venv/bin && echo js > node_modules/pkg/index.js && echo py > .venv/bin/python",
            "mkdir -p target/debug && echo bin > target/debug/app && test -f node_modules/pkg/index.js && test -e passes.txt",
            "en",
        )
        .unwrap();
    w.wait_finished(&run);
    let failed = w.store.attempts_of_run(&run).unwrap().pop().unwrap();
    assert_eq!(failed.state, "failed");
    let workspace = PathBuf::from(&failed.workspace);
    let trash = workspace.with_file_name("trash");
    until("the build being freed", || {
        !workspace.join("target").exists()
            && !workspace.join(".keel-cache").exists()
            && !trash.exists()
    });
    // A retry doesn't run setup again, so what it installed stays for the check.
    assert!(workspace.join("node_modules/pkg/index.js").exists());
    assert!(workspace.join(".venv/bin/python").exists());

    std::fs::write(workspace.join("passes.txt"), "").unwrap();
    w.engine.retry(&failed.id, false).unwrap();
    until("the retry ending", || {
        w.store.attempts_of_run(&run).unwrap().len() == 3
            && w.store.run(&run).unwrap().finished_at.is_some()
    });
    let passed = w.store.attempts_of_run(&run).unwrap().pop().unwrap();
    assert_eq!(passed.state, "succeeded", "{:?}", passed.reason);
    // A try that passed is done with all of it.
    until("everything being freed", || {
        ["target", "node_modules", ".venv", ".keel-cache"]
            .iter()
            .all(|name| !workspace.join(name).exists())
            && !trash.exists()
    });
    assert!(workspace.join("passes.txt").exists());
}

#[test]
fn only_a_try_closes_itself_when_it_fails() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    let run = w.run(&id, vec![check("check", "exit 1")]);
    let deadline = Instant::now() + Duration::from_secs(40);
    while w.store.attempts_of_run(&run).unwrap()[0].state != "failed" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(w.store.run(&run).unwrap().finished_at.is_none());
}

#[test]
fn trying_needs_a_check_command() {
    let w = world(true);
    let id = w.add();
    assert!(matches!(
        blocked_reason(w.engine.try_check(&id, "true", "  ", "en")),
        Reason::InvalidSetting { .. }
    ));
}

#[test]
fn a_project_with_a_run_waiting_for_approval_stays_until_it_ends() {
    let w = world(true);
    let id = w.add();
    let run = w.run(&id, vec![WorkflowStep::Approval { id: "human".into() }]);
    let attempt = w.store.attempts_of_run(&run).unwrap().pop().unwrap();
    assert_eq!(attempt.state, "waitingApproval");

    assert_eq!(
        blocked_reason(w.engine.remove_project(&id)),
        Reason::ProjectHasOpenRuns {
            count: 1,
            run_id: run.clone(),
            title: "the run".into(),
        }
    );
    assert_eq!(w.store.projects().unwrap().len(), 1);

    w.engine.answer_approval(&attempt.id, true, "").unwrap();
    w.wait_finished(&run);
    w.engine.remove_project(&id).unwrap();
    assert!(w.store.projects().unwrap().is_empty());
}

#[test]
fn a_failed_run_also_stays_until_someone_abandons_it() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    let run = w.run(&id, vec![check("check", "exit 1")]);
    let deadline = Instant::now() + Duration::from_secs(40);
    while w.store.attempts_of_run(&run).unwrap()[0].state != "failed" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(matches!(
        blocked_reason(w.engine.remove_project(&id)),
        Reason::ProjectHasOpenRuns { count: 1, .. }
    ));
    let attempt = w.store.attempts_of_run(&run).unwrap().pop().unwrap();
    w.engine.abandon(&attempt.id).unwrap();
    w.engine.remove_project(&id).unwrap();
}

/// Everything about a repo's folder that could change, down to file contents and refs.
fn folder_state(repo: &Path) -> String {
    let mut lines = Vec::new();
    let mut pending = vec![repo.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                lines.push(format!(
                    "{} {}",
                    path.display(),
                    keel_harness::process::sha256_file(&path).unwrap()
                ));
            }
        }
    }
    lines.sort();
    lines.join("\n")
}

#[test]
fn removing_deletes_nothing_and_a_new_run_is_refused_until_it_comes_back() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    std::fs::write(w.repo.join("mine.txt"), "uncommitted work\n").unwrap();
    let id = w.add();
    let run = w.run(&id, vec![check("check", "echo fine")]);
    w.wait_finished(&run);

    let attempt = w.store.attempts_of_run(&run).unwrap().pop().unwrap();
    w.store
        .add_decision(&id, &run, &attempt.id, "approve", "keep it small")
        .unwrap();
    let paths = vec!["README.md".to_owned()];
    let note = w
        .store
        .add_brain_note(&NewBrainNote {
            project_id: &id,
            kind: "learning",
            state: "active",
            text: "the readme matters",
            evidence: None,
            paths: &paths,
            commit_sha: None,
            run_id: &run,
            attempt_id: &attempt.id,
        })
        .unwrap();
    w.engine
        .update_project(
            &id,
            &ProjectSettings {
                check_command: Some("make test".into()),
                allowed_domains: vec!["pypi.org".into()],
                ..ProjectSettings::default()
            },
        )
        .unwrap();

    let folder_before = folder_state(&w.repo);
    let events_before = w.store.events(&attempt.id, 0).unwrap().len();
    let raw_before = std::fs::read(&attempt.raw_path).unwrap();
    let mirror = w
        .dir
        .path()
        .join("data/projects")
        .join(&id)
        .join("mirror.git");
    assert!(mirror.join("HEAD").exists());

    w.engine.remove_project(&id).unwrap();

    assert!(w.store.projects().unwrap().is_empty());
    assert_eq!(folder_state(&w.repo), folder_before, "their folder");
    assert_eq!(
        std::fs::read_to_string(w.repo.join("mine.txt")).unwrap(),
        "uncommitted work\n"
    );
    assert!(mirror.join("HEAD").exists(), "the mirror");
    assert_eq!(
        std::fs::read(&attempt.raw_path).unwrap(),
        raw_before,
        "the raw log"
    );
    assert_eq!(w.store.events(&attempt.id, 0).unwrap().len(), events_before);
    assert_eq!(w.store.decisions(&id).unwrap().len(), 1);
    assert_eq!(w.store.brain_note(note).unwrap().text, "the readme matters");
    assert_eq!(w.engine.brain(&id).unwrap().learnings.len(), 1);
    assert_eq!(w.store.run(&run).unwrap().title, "the run");
    assert_eq!(w.store.runs(10).unwrap().len(), 1, "history stays on Home");
    assert_eq!(
        w.store.project(&id).unwrap().name,
        "app",
        "old runs still open"
    );

    let refused = w.engine.start_run(StartRun {
        project_id: id.clone(),
        title: None,
        request: "x".into(),
        workflow: vec![check("check", "true")],
        language: "en".into(),
        source_issue: None,
        saved_workflow: None,
        started_by: None,
    });
    assert_eq!(blocked_reason(refused), Reason::ProjectRemoved);
    assert_eq!(
        blocked_reason(w.engine.try_check(&id, "", "true", "en")),
        Reason::ProjectRemoved
    );
    assert_eq!(
        blocked_reason(w.engine.generate_map(&id, "sonnet", "en")),
        Reason::ProjectRemoved
    );
    assert_eq!(
        blocked_reason(w.engine.retry(&attempt.id, false)),
        Reason::ProjectRemoved,
        "an old run can't go on working on it either"
    );
    assert_eq!(w.store.runs(10).unwrap().len(), 1);

    // Back, with everything, in the same row.
    let added = w.engine.add_project_with_map(&w.repo, "en").unwrap();
    assert_eq!(added.outcome, AddOutcome::Restored);
    assert_eq!(added.project.id, id);
    assert_eq!(added.map, None, "it already had its chance at a map");
    assert_eq!(added.project.check_command.as_deref(), Some("make test"));
    assert_eq!(added.project.allowed_domains, ["pypi.org"]);
    assert_eq!(w.store.projects().unwrap().len(), 1);
    assert_eq!(w.store.runs(10).unwrap().len(), 1, "no map started");
    assert_eq!(w.store.decisions(&id).unwrap().len(), 1);
    assert_eq!(w.store.events(&attempt.id, 0).unwrap().len(), events_before);

    let again = w.run(&id, vec![check("check", "echo again")]);
    w.wait_finished(&again);
    assert_eq!(w.store.runs(10).unwrap().len(), 2);
}

#[test]
fn removing_twice_is_fine_and_a_project_that_never_was_is_not() {
    let w = world(true);
    let id = w.add();
    w.engine.remove_project(&id).unwrap();
    w.engine.remove_project(&id).unwrap();
    assert!(matches!(
        w.engine.remove_project("nope"),
        Err(Error::Store(keel_store::Error::NotFound(_)))
    ));
}

#[test]
fn a_new_project_at_another_path_is_another_project() {
    let w = world(true);
    let id = w.add();
    w.engine.remove_project(&id).unwrap();
    let other = w.dir.path().join("other");
    new_repo(&other);
    let added = w.engine.add_project_with_map(&other, "en").unwrap();
    assert_eq!(added.outcome, AddOutcome::New);
    assert_ne!(added.project.id, id);
    assert!(w.store.project_is_removed(&id).unwrap());
    assert_eq!(w.store.projects().unwrap().len(), 1);
}

/// Cards that start something quick, by way of a check, so no model is involved.
fn with_cards(w: &World, id: &str, steps: Vec<WorkflowStep>) {
    let workflow = w
        .engine
        .save_workflow(
            id,
            &Workflow {
                id: String::new(),
                name: "For cards".into(),
                steps,
                builtin: false,
            },
        )
        .unwrap()
        .id;
    w.engine
        .update_project(
            id,
            &ProjectSettings {
                default_workflow: Some(workflow),
                ..ProjectSettings::default()
            },
        )
        .unwrap();
}

fn column(w: &World, card: &str) -> BoardColumn {
    w.store.card(card).unwrap().column()
}

fn wait_column(w: &World, card: &str, wanted: BoardColumn) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while column(w, card) != wanted {
        assert!(
            Instant::now() < deadline,
            "{card} stayed in {:?}",
            column(w, card)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_removed_projects_cards_stay_and_start_nothing_until_it_comes_back() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    with_cards(&w, &id, vec![check("check", "echo card")]);
    let waiting = w.engine.card_create(&id, "in the backlog", "").unwrap();
    let asked = w.engine.card_create(&id, "asked to start", "").unwrap();
    w.engine.card_move(&asked, CardLane::Todo, 0, "en").unwrap();

    w.engine.remove_project(&id).unwrap();

    w.engine.board_tick().unwrap();
    assert!(!w.store.has_todo_cards().unwrap());
    assert!(w.store.claim_next_todo_card().unwrap().is_none());
    assert_eq!(column(&w, &asked), BoardColumn::Todo, "it stays asked for");
    assert_eq!(column(&w, &waiting), BoardColumn::Backlog);
    assert!(w.store.runs(10).unwrap().is_empty(), "nothing started");
    assert_eq!(
        blocked_reason(w.engine.card_move(&waiting, CardLane::Todo, 0, "en")),
        Reason::ProjectRemoved,
        "and nothing else can be asked to"
    );

    let added = w.engine.add_project_with_map(&w.repo, "en").unwrap();
    assert_eq!(added.outcome, AddOutcome::Restored);
    assert_eq!(column(&w, &asked), BoardColumn::Todo, "as they were");
    assert_eq!(column(&w, &waiting), BoardColumn::Backlog);

    w.engine.board_tick().unwrap();
    wait_column(&w, &asked, BoardColumn::Done);
    assert_eq!(column(&w, &waiting), BoardColumn::Backlog);
    assert_eq!(w.store.runs(10).unwrap()[0].title, "asked to start");
}

#[test]
fn coming_back_wakes_the_board_by_itself() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let id = w.add();
    with_cards(&w, &id, vec![check("check", "true")]);
    let asked = w.engine.card_create(&id, "asked to start", "").unwrap();
    w.engine.card_move(&asked, CardLane::Todo, 0, "en").unwrap();
    w.engine.remove_project(&id).unwrap();
    w.engine.start_board();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(column(&w, &asked), BoardColumn::Todo);

    w.engine.add_project_with_map(&w.repo, "en").unwrap();
    wait_column(&w, &asked, BoardColumn::Done);
}

#[test]
fn another_projects_cards_keep_starting_while_one_is_removed() {
    keel_harness::needs!(sandbox);
    let w = world(true);
    let gone = w.add();
    with_cards(&w, &gone, vec![check("check", "true")]);
    let other_repo = w.dir.path().join("other");
    new_repo(&other_repo);
    let other = w.engine.add_project(&other_repo).unwrap().id;
    with_cards(&w, &other, vec![check("check", "true")]);

    let held = w.engine.card_create(&gone, "held", "").unwrap();
    let goes = w.engine.card_create(&other, "goes", "").unwrap();
    w.engine.card_move(&held, CardLane::Todo, 0, "en").unwrap();
    w.engine.card_move(&goes, CardLane::Todo, 0, "en").unwrap();
    w.engine.remove_project(&gone).unwrap();

    assert!(w.store.has_todo_cards().unwrap());
    w.engine.board_tick().unwrap();
    wait_column(&w, &goes, BoardColumn::Done);
    assert_eq!(column(&w, &held), BoardColumn::Todo);
}

#[test]
fn a_card_with_an_unfinished_run_stops_the_removal_like_any_run() {
    let w = world(true);
    let id = w.add();
    with_cards(&w, &id, vec![WorkflowStep::Approval { id: "ask".into() }]);
    let card = w.engine.card_create(&id, "the card", "").unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    w.engine.board_tick().unwrap();
    wait_column(&w, &card, BoardColumn::HumanReview);

    let run = w.store.card(&card).unwrap().run_id.unwrap();
    assert_eq!(
        blocked_reason(w.engine.remove_project(&id)),
        Reason::ProjectHasOpenRuns {
            count: 1,
            run_id: run.clone(),
            title: "the card".into(),
        }
    );
    assert_eq!(w.store.projects().unwrap().len(), 1);

    let attempt = w.store.attempts_of_run(&run).unwrap().pop().unwrap();
    w.engine.answer_approval(&attempt.id, true, "").unwrap();
    wait_column(&w, &card, BoardColumn::Done);
    w.engine.remove_project(&id).unwrap();
}

#[test]
fn a_card_being_started_right_now_also_stops_the_removal() {
    let w = world(true);
    let id = w.add();
    with_cards(&w, &id, vec![check("check", "true")]);
    let card = w.engine.card_create(&id, "the card", "").unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    assert_eq!(w.store.claim_next_todo_card().unwrap().unwrap().id, card);

    assert_eq!(
        blocked_reason(w.engine.remove_project(&id)),
        Reason::ProjectHasOpenRuns {
            count: 1,
            run_id: String::new(),
            title: "the card".into(),
        }
    );
    w.store.release_claimed_cards().unwrap();
    w.engine.remove_project(&id).unwrap();
    assert_eq!(column(&w, &card), BoardColumn::Todo);
}

impl World {
    fn propose(&self, path: &Path) -> ProjectProposal {
        match self.engine.propose_project(path, "Claude Code").unwrap() {
            ProposedProject::Waiting(proposal) => proposal,
            other => panic!("expected it to wait for the person, got {other:?}"),
        }
    }

    fn root(&self) -> String {
        std::fs::canonicalize(&self.repo)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}

#[test]
fn a_proposal_adds_nothing_and_spends_nothing_until_the_person_adds_it() {
    let w = world(true);
    let proposal = w.propose(&w.repo);
    assert_eq!(proposal.path, w.root());
    assert_eq!(proposal.proposed_by, "Claude Code");
    assert!(w.store.projects().unwrap().is_empty());
    assert!(
        w.store.runs(10).unwrap().is_empty(),
        "no map behind their back"
    );
    assert_eq!(w.agents_run(), 0);

    let added = w
        .engine
        .accept_project_proposal(&proposal.id, "en")
        .unwrap();
    assert_eq!(added.outcome, AddOutcome::New);
    assert_eq!(
        added.map,
        Some(MapStart::Started),
        "the same as adding it by hand"
    );
    assert_eq!(added.project.path, w.root());
    assert_eq!(added.project.github_account, None, "the person chooses it");
    assert_eq!(added.project.check_command, None, "the person chooses it");
    assert!(w.engine.project_proposals().unwrap().is_empty());
}

#[test]
fn discarding_a_proposal_leaves_nothing_behind() {
    let w = world(true);
    let proposal = w.propose(&w.repo);
    w.engine.discard_project_proposal(&proposal.id).unwrap();
    assert!(w.engine.project_proposals().unwrap().is_empty());
    assert!(w.store.projects().unwrap().is_empty());
    assert!(matches!(
        w.engine.accept_project_proposal(&proposal.id, "en"),
        Err(Error::InvalidState)
    ));
}

#[test]
fn the_same_folder_proposed_twice_asks_the_person_once() {
    let w = world(true);
    let first = w.propose(&w.repo);
    let inner = w.repo.join("inner");
    std::fs::create_dir(&inner).unwrap();
    // From a subfolder it's the repo that gets proposed, as that's what adding it would add.
    assert_eq!(w.propose(&inner), first);
    assert_eq!(w.engine.project_proposals().unwrap().len(), 1);
}

#[test]
fn a_folder_that_is_no_repo_is_not_left_for_the_person() {
    let w = world(true);
    let plain = w.dir.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    for path in [plain.clone(), w.dir.path().join("nowhere")] {
        assert!(matches!(
            w.engine.propose_project(&path, "Claude Code"),
            Err(Error::NotARepo(_))
        ));
    }
    assert!(w.engine.project_proposals().unwrap().is_empty());
}

#[test]
fn a_project_that_is_already_in_keel_is_answered_and_not_proposed() {
    let w = world(true);
    let id = w.add();
    match w.engine.propose_project(&w.repo, "Claude Code").unwrap() {
        ProposedProject::InKeel(project) => assert_eq!(project.id, id),
        other => panic!("expected the project, got {other:?}"),
    }
    assert!(w.engine.project_proposals().unwrap().is_empty());
}

#[test]
fn a_removed_project_can_be_proposed_and_says_it_comes_back() {
    let w = world(true);
    let id = w.add();
    w.engine.remove_project(&id).unwrap();
    let proposal = w.propose(&w.repo);
    let review = w.engine.project_proposal_reviews().unwrap().remove(0);
    assert!(review.returning);
    assert!(!review.gone);

    let added = w
        .engine
        .accept_project_proposal(&proposal.id, "en")
        .unwrap();
    assert_eq!(added.outcome, AddOutcome::Restored);
    assert_eq!(added.project.id, id);
    assert_eq!(added.map, None);
}

#[test]
fn adding_a_folder_by_hand_answers_its_proposal() {
    let w = world(true);
    w.propose(&w.repo);
    w.engine.add_project(&w.repo).unwrap();
    assert!(w.engine.project_proposals().unwrap().is_empty());
}

#[test]
fn a_review_says_what_the_folder_is_now() {
    let w = world(true);
    let proposal = w.propose(&w.repo);
    git(
        &w.repo,
        &["remote", "add", "origin", "https://github.com/octo/app.git"],
    );
    let review = w.engine.project_proposal_reviews().unwrap().remove(0);
    assert_eq!(review.name, "app");
    assert_eq!(review.github_repo.as_deref(), Some("octo/app"));
    assert!(!review.returning);
    assert!(!review.gone);
    assert_eq!(review.proposal, proposal);
    let shown = serde_json::to_value(&review).unwrap();
    assert_eq!(shown["proposedBy"], "Claude Code");
    assert_eq!(shown["githubRepo"], "octo/app");
}

#[test]
fn a_folder_that_changed_since_it_was_proposed_cannot_be_added() {
    let w = world(true);
    let proposal = w.propose(&w.repo);
    std::fs::remove_dir_all(w.repo.join(".git")).unwrap();
    let review = w.engine.project_proposal_reviews().unwrap().remove(0);
    assert!(review.gone);
    assert!(matches!(
        w.engine.accept_project_proposal(&proposal.id, "en"),
        Err(Error::NotARepo(_))
    ));
    assert!(w.store.projects().unwrap().is_empty());
    assert_eq!(
        w.engine.project_proposals().unwrap().len(),
        1,
        "still theirs to discard"
    );

    // Inside another repo now, adding it would add something they never saw.
    git(w.dir.path(), &["init", "-q", "-b", "main"]);
    git(w.dir.path(), &["config", "user.name", "T"]);
    git(w.dir.path(), &["config", "user.email", "t@example.com"]);
    git(w.dir.path(), &["add", "app/README.md"]);
    git(w.dir.path(), &["commit", "-qm", "around it"]);
    assert!(w.engine.project_proposal_reviews().unwrap().remove(0).gone);
    assert!(matches!(
        w.engine.accept_project_proposal(&proposal.id, "en"),
        Err(Error::NotARepo(_))
    ));
    assert!(w.store.projects().unwrap().is_empty());
}

#[test]
fn only_so_many_proposals_may_wait_and_asking_again_for_one_still_answers() {
    let w = world(true);
    let mut first = None;
    for i in 0..MAX_PROJECT_PROPOSALS {
        let repo = w.dir.path().join(format!("repo-{i}"));
        new_repo(&repo);
        first.get_or_insert(w.propose(&repo));
    }
    let another = w.dir.path().join("another");
    new_repo(&another);
    assert!(matches!(
        w.engine.propose_project(&another, "Claude Code").unwrap(),
        ProposedProject::Full
    ));
    assert_eq!(
        w.engine.project_proposals().unwrap().len(),
        MAX_PROJECT_PROPOSALS
    );
    let first = first.unwrap();
    assert_eq!(w.propose(Path::new(&first.path)), first);

    // Once the person deals with one, there's room again.
    w.engine.discard_project_proposal(&first.id).unwrap();
    w.propose(&another);
}

#[test]
fn a_path_that_could_hide_what_it_is_is_not_left_for_the_person() {
    let w = world(true);
    let repo = w.dir.path().join("cod\u{202E}gpj");
    new_repo(&repo);
    assert!(matches!(
        w.engine.propose_project(&repo, "Claude Code").unwrap(),
        ProposedProject::UnreadablePath
    ));
    assert!(w.engine.project_proposals().unwrap().is_empty());
}
