//! What the engine can't act on, and nobody is there to hear, ends up in Keel's log. With a real
//! repo and a fake `claude`, and a database made to refuse the one write each test is about.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, Log, Reason, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, StartRun};
use keel_harness::Tools;
use keel_harness::process::process_start;
use keel_store::rusqlite_value::Value;
use keel_store::{Attempt, NewAttempt, NewRun, Store};

const FAKE_CLAUDE: &str = r#"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max","email":"a@example.com"}'; exit 0;;
esac
cat > /dev/null
echo '{"type":"system","subtype":"init","session_id":"s","model":"claude-fake-1","tools":["Read","Grep","Glob","Edit","Write","Bash"],"apiKeySource":"none","permissionMode":"dontAsk","claude_code_version":"2.1.318"}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Done.","num_turns":1,"duration_ms":10,"total_cost_usd":0.01,"usage":{"input_tokens":1,"output_tokens":1},"modelUsage":{"claude-fake-1":{}},"permission_denials":[]}'
"#;

struct Harness {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
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

fn new_repo(dir: &Path, name: &str) -> PathBuf {
    let repo = dir.join(name);
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Tester"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "demo\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

/// A process standing in for an agent that outlived the app. It never outlives its test.
struct Outlived(std::process::Child);

impl Outlived {
    fn start() -> Self {
        let child = Command::new("/bin/sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        Self(child)
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }

    fn has_ended_within(&mut self, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        while self.0.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }
}

impl Drop for Outlived {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn reason(attempt: &Attempt) -> Reason {
    serde_json::from_value(attempt.reason.clone().expect("has a reason")).unwrap()
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let repo = new_repo(dir.path(), "demo");

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let project_id = engine.add_project(&repo).unwrap().id;
    Harness {
        dir,
        store,
        engine,
        project_id,
    }
}

fn step(id: &str) -> StepSpec {
    StepSpec {
        id: id.into(),
        model: "fake".into(),
        effort: None,
        tools: ToolProfile::Build,
        prompt: "Fix the bug".into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        risks: vec![],
        model_when_risky: None,
        role: Default::default(),
    }
}

impl Harness {
    fn log(&self) -> String {
        let log = Log::in_dir(&self.dir.path().join("data"));
        std::fs::read_to_string(log.path()).unwrap_or_default()
    }

    /// Waits for the log to say `text`, since a step ends on its monitor's thread.
    fn wait_for_log(&self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let log = self.log();
            if log.contains(text) {
                return log;
            }
            assert!(Instant::now() < deadline, "the log never said it: {log:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Changes the database behind the engine's back.
    fn sql(&self, sql: &str) {
        let conn = rusqlite::Connection::open(self.dir.path().join("keel.db")).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn.execute_batch(sql).unwrap();
    }

    /// Has the database refuse a write from here on, the way a full disk or a damaged file would.
    fn refuse(&self, name: &str, when: &str) {
        self.sql(&format!(
            "create trigger {name} before {when} begin select raise(abort, 'made-up failure'); end;"
        ));
    }

    fn start(&self, steps: &[&str]) -> String {
        self.start_in(&self.project_id, steps)
    }

    fn start_in(&self, project_id: &str, steps: &[&str]) -> String {
        self.engine
            .start_run(StartRun {
                project_id: project_id.to_owned(),
                title: None,
                request: "Fix the bug".into(),
                workflow: steps
                    .iter()
                    .map(|id| WorkflowStep::Agent(step(id)))
                    .collect(),
                saved_workflow: None,
                language: "en".into(),
                source_issue: None,
                started_by: None,
            })
            .unwrap()
    }

    fn add_run(&self, id: &str) {
        self.store
            .add_run(&NewRun {
                id: id.into(),
                project_id: self.project_id.clone(),
                title: id.into(),
                request: "x".into(),
                workflow: serde_json::to_string(&[WorkflowStep::Agent(step("builder"))]).unwrap(),
                language: "en".into(),
                source_issue: None,
                request_written: None,
                workflow_id: None,
                workflow_name: None,
                started_by: None,
            })
            .unwrap();
    }

    /// A step the app was running when it closed, whose process is gone.
    fn add_running_step(&self, id: &str, run: &str, started_at: &str) {
        let workspace = self.dir.path().join(format!("copy-{id}"));
        std::fs::create_dir(&workspace).unwrap();
        let raw = self.dir.path().join(format!("{id}.jsonl"));
        std::fs::write(&raw, "").unwrap();
        self.store
            .add_attempt(&NewAttempt {
                id: id.into(),
                run_id: run.into(),
                step_id: "builder".into(),
                kind: "agent".into(),
                role: "worker".into(),
                round: 0,
                number: 1,
                state: AttemptState::Running,
                model_requested: "fake".into(),
                effort: None,
                tools: "build".into(),
                max_turns: None,
                max_budget_usd: None,
                language: "en".into(),
                prompt: "x".into(),
                system_prompt_sources: "[]".into(),
                settings: "{}".into(),
                session_id: format!("session-{id}"),
                resumed_from: None,
                workspace: workspace.to_string_lossy().into_owned(),
                raw_path: raw.to_string_lossy().into_owned(),
                started_at: started_at.into(),
            })
            .unwrap();
    }
}

#[test]
fn reopening_picks_up_the_other_steps_when_one_cant_be_and_the_log_says_which() {
    let h = harness();
    h.add_run("first-run");
    h.add_run("second-run");
    // The first to be picked up belongs to a run the database no longer has.
    h.add_running_step("orphan", "run-that-is-gone", "2026-01-01T00:00:00.000Z");
    h.add_running_step("lost-1", "first-run", "2026-01-01T00:00:01.000Z");
    h.add_running_step("lost-2", "second-run", "2026-01-01T00:00:02.000Z");

    h.engine.recover().unwrap();

    // Nothing is left showing as running with nobody watching it, the one that failed included.
    for id in ["orphan", "lost-1", "lost-2"] {
        let attempt = h.store.attempt(id).unwrap();
        assert_eq!(attempt.state, "interrupted", "{id}");
        assert_eq!(reason(&attempt), Reason::AgentLost, "{id}");
        assert!(attempt.finished_at.is_some(), "{id}");
    }
    assert_eq!(h.engine.active_count().unwrap(), 0);
    let log = h.log();
    assert!(
        log.contains("picking a step up again (orphan)"),
        "the log says which step: {log:?}"
    );
    assert!(log.contains("run-that-is-gone"), "and why: {log:?}");
    assert!(!log.contains("lost-1"), "and only that one: {log:?}");
}

#[test]
fn a_step_that_cant_be_picked_up_gives_back_its_place_and_its_process_is_ended() {
    keel_harness::needs!(sandbox, ps);
    let h = harness();
    // One agent at a time, so a place that isn't given back stops every other run.
    h.engine.set_max_agents(1).unwrap();
    let other = h
        .engine
        .add_project(&new_repo(h.dir.path(), "other"))
        .unwrap()
        .id;
    h.add_run("first-run");
    h.add_running_step("held", "first-run", "2026-01-01T00:00:00.000Z");
    // Its process outlived the app, as an agent does.
    let mut agent = Outlived::start();
    let pid = agent.pid();
    h.store
        .patch_attempt(
            "held",
            &[
                ("pid", Value::Integer(i64::from(pid))),
                ("pid_started", Value::Text(process_start(pid).unwrap())),
            ],
        )
        .unwrap();
    // Its project can't be read, which picking it up finds out only after it has taken a place
    // among the agents and the project's turn.
    h.sql(&format!(
        "delete from project where id = '{}'",
        h.project_id
    ));

    h.engine.recover().unwrap();

    let log = h.log();
    assert!(
        log.contains("picking a step up again (held)")
            && log.contains(&format!("project {}", h.project_id)),
        "{log:?}"
    );
    let held = h.store.attempt("held").unwrap();
    assert_eq!(held.state, "interrupted");
    assert_eq!(reason(&held), Reason::AgentLost);
    assert!(
        agent.has_ended_within(Duration::from_secs(5)),
        "its process went on with nothing watching it"
    );
    // The place is free: a run of another project gets it and ends.
    let run = h.start_in(&other, &["builder"]);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let attempt = h.store.attempts_of_run(&run).unwrap().pop().unwrap();
        if attempt.state == "succeeded" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stuck in {} ({:?})",
            attempt.state,
            attempt.reason
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Both at once, as the app reopens: a step that can't be picked up is ended and written down,
/// and a check that was over by then still gets its real result. Its run moves on afterwards, from
/// `recover` itself, once every step has been looked at.
#[test]
fn reopening_past_a_step_that_cant_be_picked_up_still_gives_a_check_that_was_over_its_result() {
    keel_harness::needs!(sandbox);
    let h = harness();
    // The app that starts the check never looks at it again, as one that was closed doesn't.
    let tools = Tools::from_dirs(&[
        h.dir.path().join("bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);
    let mut config = Config::new(h.dir.path().join("data"));
    config.tick = Duration::from_secs(3600);
    config.keep_awake = false;
    let closed = Engine::new(h.store.clone(), config, tools, |_| {});
    let run = closed
        .start_run(StartRun {
            project_id: h.project_id.clone(),
            title: None,
            request: "x".into(),
            workflow: vec![
                WorkflowStep::Check {
                    id: "check".into(),
                    run: "true".into(),
                    only_if_changed: false,
                },
                WorkflowStep::Approval { id: "human".into() },
            ],
            saved_workflow: None,
            language: "en".into(),
            source_issue: None,
            started_by: None,
        })
        .unwrap();
    let latest = || h.store.attempts_of_run(&run).unwrap().pop().unwrap();
    let until = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "{what} never happened");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    until("the check to start", &|| latest().state == "running");
    let check = latest();
    let exit = Path::new(&check.raw_path).with_extension("exit");
    until("the check to end", &|| exit.exists());
    // The first to be picked up belongs to a run the database no longer has.
    h.add_running_step("orphan", "run-that-is-gone", "2026-01-01T00:00:00.000Z");

    h.engine.recover().unwrap();

    let orphan = h.store.attempt("orphan").unwrap();
    assert_eq!(orphan.state, "interrupted");
    assert_eq!(reason(&orphan), Reason::AgentLost);
    let log = h.log();
    assert!(
        log.contains("picking a step up again (orphan)"),
        "the log says which step: {log:?}"
    );
    assert!(!log.contains(&check.id), "and only that one: {log:?}");
    // The check passed, and says so, and its run is where it would be had the app never closed.
    let check = h.store.attempt(&check.id).unwrap();
    assert_eq!(check.state, "succeeded", "{:?}", check.reason);
    assert_eq!(check.exit_code, Some(0));
    until("the run to move on", &|| {
        let next = latest();
        next.step_id == "human" && next.state == "waitingApproval"
    });
}

#[test]
fn a_step_whose_end_cant_be_written_says_so_in_the_log() {
    keel_harness::needs!(sandbox);
    let h = harness();
    h.refuse(
        "no_success",
        "update of state on attempt when new.state = 'succeeded'",
    );

    let run = h.start(&["builder"]);

    let log = h.wait_for_log("keeping that a step is succeeded");
    let attempt = h.store.attempts_of_run(&run).unwrap().pop().unwrap();
    assert!(
        log.contains(&format!("({}): ", attempt.id)),
        "it names the step: {log:?}"
    );
    assert!(log.contains("made-up failure"), "and says why: {log:?}");
    assert_ne!(attempt.state, "succeeded");
}

#[test]
fn a_run_that_cant_go_on_to_its_next_step_says_so_in_the_log() {
    keel_harness::needs!(sandbox);
    let h = harness();
    h.refuse(
        "no_second_step",
        "insert on attempt when new.step_id = 'second'",
    );

    let run = h.start(&["builder", "second"]);

    let log = h.wait_for_log("moving a run on to its next step");
    assert!(
        log.contains(&format!("({run}): ")),
        "it names the run: {log:?}"
    );
    assert!(log.contains("made-up failure"), "and says why: {log:?}");
    let attempts = h.store.attempts_of_run(&run).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].state, "succeeded");
}
