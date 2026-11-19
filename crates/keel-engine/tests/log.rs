//! What the engine can't act on, and nobody is there to hear, ends up in Keel's log. With a real
//! repo and a fake `claude`, and a database made to refuse the one write each test is about.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, Log, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, StartRun};
use keel_harness::Tools;
use keel_store::{NewAttempt, NewRun, Store};

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

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let repo = dir.path().join("demo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Tester"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "demo\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

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

    /// Has the database refuse a write from here on, the way a full disk or a damaged file would.
    fn refuse(&self, name: &str, when: &str) {
        let conn = rusqlite::Connection::open(self.dir.path().join("keel.db")).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn.execute_batch(&format!(
            "create trigger {name} before {when} begin select raise(abort, 'made-up failure'); end;"
        ))
        .unwrap();
    }

    fn start(&self, steps: &[&str]) -> String {
        self.engine
            .start_run(StartRun {
                project_id: self.project_id.clone(),
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

    // Nothing is left showing as running with nobody watching it.
    for id in ["lost-1", "lost-2"] {
        assert_eq!(h.store.attempt(id).unwrap().state, "interrupted", "{id}");
    }
    let log = h.log();
    assert!(
        log.contains("picking a step up again (orphan)"),
        "the log says which step: {log:?}"
    );
    assert!(log.contains("run-that-is-gone"), "and why: {log:?}");
    assert!(!log.contains("lost-1"), "and only that one: {log:?}");
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
