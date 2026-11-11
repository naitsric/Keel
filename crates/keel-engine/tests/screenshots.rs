//! Pictures a step leaves of how the change looks reach its approval, and never its branch. With a
//! real repo, the real sandbox for the check, and a fake `claude`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, StartRun};
use keel_harness::Tools;
use keel_store::{Attempt, Store};

const BUILD_TOOLS: &str = r#"["Read","Grep","Glob","Edit","Write","Bash"]"#;

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

/// The builder changes a file and saves a picture of its own, as a project's script run by an agent
/// would.
fn engine(dir: &Path) -> (Arc<Store>, Engine, String) {
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = format!(
        r#"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max","email":"a@example.com"}}'; exit 0;;
esac
cat > /dev/null
echo '{{"type":"system","subtype":"init","session_id":"s","model":"claude-fake-1","tools":{BUILD_TOOLS},"apiKeySource":"none","permissionMode":"dontAsk","claude_code_version":"2.1.318"}}'
echo fixed > fixed.txt
printf '\211PNG\r\n\032\nagent' > "$KEEL_SCREENSHOTS/agent.png"
echo '{{"type":"result","subtype":"success","is_error":false,"result":"Fixed it.","num_turns":1}}'
"#
    );
    let path = bin.join("claude");
    std::fs::write(&path, fake).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let repo = dir.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "x\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    let store = Arc::new(Store::open(&dir.join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let project_id = engine.add_project(&repo).unwrap().id;
    (store, engine, project_id)
}

fn builder() -> WorkflowStep {
    WorkflowStep::Agent(StepSpec {
        id: "builder".into(),
        model: "fake".into(),
        effort: None,
        tools: ToolProfile::Build,
        prompt: "Fix it".into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        role: Default::default(),
    })
}

fn wait_step(store: &Store, run: &str, step: &str, state: AttemptState) -> Attempt {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let attempts = store.attempts_of_run(run).unwrap();
        if let Some(found) = attempts
            .iter()
            .find(|a| a.step_id == step && a.state == state.as_str())
        {
            return found.clone();
        }
        let states: Vec<_> = attempts
            .iter()
            .map(|a| format!("{} {} {:?}", a.step_id, a.state, a.reason))
            .collect();
        assert!(Instant::now() < deadline, "stuck: {states:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn what_a_step_saves_reaches_the_approval_and_not_the_branch() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine, project_id) = engine(dir.path());
    let run = engine
        .start_run(StartRun {
            project_id,
            title: Some("the run".into()),
            request: "Fix it".into(),
            workflow: vec![
                builder(),
                WorkflowStep::Check {
                    id: "screens".into(),
                    run: r#"printf '\211PNG\r\n\032\ncheck' > "$KEEL_SCREENSHOTS/screen.png" && echo notes > "$KEEL_SCREENSHOTS/notes.txt""#.into(),
                },
                WorkflowStep::Approval { id: "human".into() },
            ],
            language: "en".into(),
            source_issue: None,
            saved_workflow: None,
            started_by: None,
        })
        .unwrap();
    wait_step(&store, &run, "human", AttemptState::WaitingApproval);

    let shots = engine.run_screenshots(&run).unwrap();
    let shown: Vec<_> = shots
        .shown
        .iter()
        .map(|shot| (shot.name.as_str(), shot.mime, shot.before_last_change))
        .collect();
    // The builder's own picture may come from before its last edit, so it says so. The check's
    // came after the code last changed.
    assert_eq!(
        shown,
        [
            ("agent.png", "image/png", true),
            ("screen.png", "image/png", false)
        ]
    );
    assert_eq!(shots.shown[1].data, b"\x89PNG\r\n\x1a\ncheck");
    assert_eq!(shots.left_out, ["notes.txt"]);

    // Git never sees them: not in what the builder changed, nor left behind by the check.
    let attempts = store.attempts_of_run(&run).unwrap();
    let built = attempts.iter().find(|a| a.step_id == "builder").unwrap();
    assert_eq!(
        built.files_changed.as_ref().unwrap(),
        &serde_json::json!(["fixed.txt"])
    );
    let check = attempts.iter().find(|a| a.step_id == "screens").unwrap();
    let result = store
        .events(&check.id, 0)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "checkResult")
        .unwrap();
    assert_eq!(result.data["leftWorkspaceDirty"], false, "{}", result.data);
    assert!(!engine.run_diff(&run).unwrap().contains("keel-screenshots"));
}

#[test]
fn a_run_without_pictures_has_none_to_show() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine, project_id) = engine(dir.path());
    let run = engine
        .start_run(StartRun {
            project_id,
            title: Some("the run".into()),
            request: "Ask".into(),
            workflow: vec![WorkflowStep::Approval { id: "human".into() }],
            language: "en".into(),
            source_issue: None,
            saved_workflow: None,
            started_by: None,
        })
        .unwrap();
    wait_step(&store, &run, "human", AttemptState::WaitingApproval);
    let shots = engine.run_screenshots(&run).unwrap();
    assert!(shots.shown.is_empty() && shots.left_out.is_empty());
}
