//! Pictures a step leaves of how the change looks reach its approval, each with the step it came
//! from, and never its branch. With a real repo, the real sandbox for the check, and a fake
//! `claude`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, EnvVar, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, StartRun};
use keel_harness::Tools;
use keel_store::{Attempt, ProjectSettings, Store};

const BUILD_TOOLS: &str = r#"["Read","Grep","Glob","Edit","Write","Bash"]"#;

/// Says whether the folder was empty when it started, then saves a picture dated long ago and a
/// note. The date is the step's to set, so it must not matter.
const SCREENS: &str = r#"[ -z "$(ls -A "$KEEL_SCREENSHOTS")" ] && echo started-empty; printf '\211PNG\r\n\032\ncheck' > "$KEEL_SCREENSHOTS/screen.png" && touch -t 200001010000 "$KEEL_SCREENSHOTS/screen.png" && echo notes > "$KEEL_SCREENSHOTS/notes.txt""#;

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

/// The builder changes a file and saves a picture of its own, dated far ahead, as a project's
/// script run by an agent would: `agent-1.png` the first time, `agent-2.png` the next. The first
/// time it also saves eight that sort before it, more than an approval shows.
fn engine(dir: &Path) -> (Arc<Store>, Engine, String) {
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let count = dir.join("count");
    let fake = format!(
        r#"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max","email":"a@example.com"}}'; exit 0;;
esac
cat > /dev/null
echo '{{"type":"system","subtype":"init","session_id":"s","model":"claude-fake-1","tools":{BUILD_TOOLS},"apiKeySource":"none","permissionMode":"dontAsk","claude_code_version":"2.1.318"}}'
n=$(( $(cat "{count}" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "{count}"
echo "fixed $n" > fixed.txt
if [ "$n" = 1 ]; then for i in 1 2 3 4 5 6 7 8; do printf '\211PNG\r\n\032\nold' > "$KEEL_SCREENSHOTS/aa-old-$i.png"; done; fi
printf '\211PNG\r\n\032\nagent' > "$KEEL_SCREENSHOTS/agent-$n.png"
touch -t 203001010000 "$KEEL_SCREENSHOTS/agent-$n.png"
echo '{{"type":"result","subtype":"success","is_error":false,"result":"Fixed it.","num_turns":1}}'
"#,
        count = count.display()
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

fn check(id: &str, run: &str) -> WorkflowStep {
    WorkflowStep::Check {
        id: id.into(),
        run: run.into(),
    }
}

fn start(engine: &Engine, project_id: &str, workflow: Vec<WorkflowStep>) -> String {
    engine
        .start_run(StartRun {
            project_id: project_id.to_owned(),
            title: Some("the run".into()),
            request: "Fix it".into(),
            workflow,
            language: "en".into(),
            source_issue: None,
            saved_workflow: None,
            started_by: None,
        })
        .unwrap()
}

/// The latest attempt of `step` once it's in `state`.
fn wait_step(store: &Store, run: &str, step: &str, state: AttemptState) -> Attempt {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let attempts = store.attempts_of_run(run).unwrap();
        if let Some(found) = attempts
            .iter()
            .rev()
            .find(|a| a.step_id == step)
            .filter(|a| a.state == state.as_str())
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

fn check_result(store: &Store, attempt: &Attempt) -> serde_json::Value {
    store
        .events(&attempt.id, 0)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "checkResult")
        .unwrap()
        .data
}

fn shown(engine: &Engine, run: &str) -> Vec<(String, String, bool)> {
    engine
        .run_screenshots(run)
        .unwrap()
        .shown
        .into_iter()
        .map(|shot| (shot.name, shot.step, shot.before_last_change))
        .collect()
}

fn shot(name: &str, step: &str, before_last_change: bool) -> (String, String, bool) {
    (name.to_owned(), step.to_owned(), before_last_change)
}

#[test]
fn what_a_step_saves_reaches_the_approval_with_its_step_and_not_the_branch() {
    keel_harness::needs!(sandbox);
    let dir = tempfile::tempdir().unwrap();
    let (store, engine, project_id) = engine(dir.path());
    let run = start(
        &engine,
        &project_id,
        vec![
            builder(),
            check("screens", SCREENS),
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    wait_step(&store, &run, "human", AttemptState::WaitingApproval);

    let shots = engine.run_screenshots(&run).unwrap();
    // The latest step's first. The builder may have edited after saving its own, whatever date it
    // gave it; nothing changed the code after the check, however old it says its picture is.
    assert_eq!(
        shown(&engine, &run),
        [shot("screen.png", "screens", false)]
            .into_iter()
            .chain((1..=7).map(|i| shot(&format!("aa-old-{i}.png"), "builder", true)))
            .collect::<Vec<_>>()
    );
    assert_eq!(shots.shown[0].mime, "image/png");
    assert_eq!(shots.shown[0].data, b"\x89PNG\r\n\x1a\ncheck");
    assert_eq!(
        shots.left_out,
        ["notes.txt", "aa-old-8.png", "agent-1.png"],
        "the check's note, then the builder's past the limit"
    );
    assert_eq!(shots.more_left_out, 0);

    // The builder's pictures were taken away when it ended: the check started with none.
    let screens = wait_step(&store, &run, "screens", AttemptState::Succeeded);
    let result = check_result(&store, &screens);
    assert!(
        result["stdout"].as_str().unwrap().contains("started-empty"),
        "{result}"
    );

    // Git never sees them: not in what the builder changed, nor left behind by the check.
    let built = wait_step(&store, &run, "builder", AttemptState::Succeeded);
    assert_eq!(
        built.files_changed.as_ref().unwrap(),
        &serde_json::json!(["fixed.txt"])
    );
    assert_eq!(result["leftWorkspaceDirty"], false, "{result}");
    assert!(!engine.run_diff(&run).unwrap().contains("keel-screenshots"));
}

#[test]
fn a_new_round_shows_its_own_pictures_and_not_the_last_ones() {
    keel_harness::needs!(sandbox);
    let dir = tempfile::tempdir().unwrap();
    let (store, engine, project_id) = engine(dir.path());
    let run = start(
        &engine,
        &project_id,
        vec![
            builder(),
            check("screens", SCREENS),
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    let first = wait_step(&store, &run, "human", AttemptState::WaitingApproval);
    engine.send_back(&first.id, "Again").unwrap();
    let second = wait_step(&store, &run, "human", AttemptState::WaitingApproval);
    assert_ne!(first.id, second.id);

    // The last round's nine take no place, and aren't listed as left out.
    let shots = engine.run_screenshots(&run).unwrap();
    assert_eq!(
        shown(&engine, &run),
        [
            shot("screen.png", "screens", false),
            shot("agent-2.png", "builder", true)
        ]
    );
    assert_eq!(shots.left_out, ["notes.txt"]);
    assert_eq!(shots.more_left_out, 0);
}

#[test]
fn the_folder_is_keels_whatever_the_projects_variables_say() {
    keel_harness::needs!(sandbox);
    let dir = tempfile::tempdir().unwrap();
    let (store, engine, project_id) = engine(dir.path());
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    // Settings refuse the name, so it goes straight in, as an older project might have it.
    store
        .update_project(
            &project_id,
            &ProjectSettings {
                env: vec![EnvVar {
                    name: "KEEL_SCREENSHOTS".into(),
                    value: elsewhere.to_string_lossy().into_owned(),
                }],
                ..ProjectSettings::default()
            },
        )
        .unwrap();
    let run = start(
        &engine,
        &project_id,
        vec![
            builder(),
            check("screens", SCREENS),
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    wait_step(&store, &run, "human", AttemptState::WaitingApproval);
    let steps: Vec<_> = shown(&engine, &run)
        .into_iter()
        .map(|(name, step, _)| (name, step))
        .collect();
    assert!(steps.contains(&("screen.png".to_owned(), "screens".to_owned())));
    assert!(steps.iter().any(|(_, step)| step == "builder"), "{steps:?}");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn what_a_step_leaves_running_ends_with_it() {
    keel_harness::needs!(sandbox);
    let dir = tempfile::tempdir().unwrap();
    let (store, engine, project_id) = engine(dir.path());
    let run = start(
        &engine,
        &project_id,
        vec![
            check("leaves", "sleep 300 > /dev/null 2>&1 & echo $!"),
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    wait_step(&store, &run, "human", AttemptState::WaitingApproval);
    let leaves = wait_step(&store, &run, "leaves", AttemptState::Succeeded);
    let pid: i32 = check_result(&store, &leaves)["stdout"]
        .as_str()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while keel_harness::process::signal_process(pid, 0) {
        assert!(Instant::now() < deadline, "{pid} is still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_run_without_pictures_has_none_to_show() {
    keel_harness::needs!(sandbox);
    let dir = tempfile::tempdir().unwrap();
    let (store, engine, project_id) = engine(dir.path());
    let run = start(
        &engine,
        &project_id,
        vec![
            check("quiet", "true"),
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    wait_step(&store, &run, "human", AttemptState::WaitingApproval);
    let shots = engine.run_screenshots(&run).unwrap();
    assert!(shots.shown.is_empty() && shots.left_out.is_empty());
    assert_eq!(shots.more_left_out, 0);
}
