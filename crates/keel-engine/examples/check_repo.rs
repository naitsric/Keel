//! Runs one check step on a repo through the engine, with no model, to see how a project behaves
//! under the sandbox. Reads the repo and changes nothing in it.
//! `cargo run -p keel-engine --example check_repo -- <scratch dir> <repo> <settings.json> <command>`
//! `settings.json` holds `{ "setup": "...", "services": [...], "env": [...], "read": [...] }`, and
//! optionally `"agentPrompt"` and `"model"` to run a real agent before the check.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, StartRun};
use keel_harness::Tools;
use keel_store::{ProjectSettings, Store};

fn main() {
    let mut args = std::env::args().skip(1);
    let work = PathBuf::from(args.next().expect("scratch dir"));
    let repo = PathBuf::from(args.next().expect("repo"));
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(args.next().expect("settings")).unwrap())
            .unwrap();
    let command = args.next().expect("command");
    std::fs::create_dir_all(&work).unwrap();

    let store = Arc::new(Store::open(&work.join("keel.db")).unwrap());
    let engine = Engine::new(
        store.clone(),
        Config::new(work.join("data")),
        Tools::detect(),
        |_| {},
    );
    let project = engine.add_project(&repo).unwrap();
    engine
        .update_project(
            &project.id,
            &ProjectSettings {
                setup_command: settings["setup"].as_str().map(str::to_owned),
                services: serde_json::from_value(settings["services"].clone()).unwrap_or_default(),
                env: serde_json::from_value(settings["env"].clone()).unwrap_or_default(),
                allowed_read: serde_json::from_value(settings["read"].clone()).unwrap_or_default(),
                ..ProjectSettings::default()
            },
        )
        .unwrap();
    let started = Instant::now();
    let mut workflow = vec![WorkflowStep::Check {
        id: "check".into(),
        run: command.clone(),
    }];
    if let Some(prompt) = settings["agentPrompt"].as_str() {
        workflow.insert(
            0,
            WorkflowStep::Agent(StepSpec {
                id: "builder".into(),
                model: settings["model"].as_str().unwrap_or("haiku").to_owned(),
                effort: None,
                tools: ToolProfile::Build,
                prompt: prompt.to_owned(),
                max_turns: Some(15),
                max_budget_usd: None,
                paths: vec![],
                role: Default::default(),
            }),
        );
    }
    let run = engine
        .start_run(StartRun {
            project_id: project.id,
            title: None,
            request: command.clone(),
            workflow,
            language: "en".into(),
            source_issue: None,
            saved_workflow: None,
        })
        .unwrap();
    loop {
        if store.run(&run).unwrap().finished_at.is_some() {
            break;
        }
        let latest = store.attempts_of_run(&run).unwrap().pop();
        if latest.is_some_and(|a| {
            !["queued", "preparing", "running", "stalled", "succeeded"].contains(&a.state.as_str())
        }) {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(900), "timed out");
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("total seconds: {}", started.elapsed().as_secs());
    for attempt in store.attempts_of_run(&run).unwrap() {
        println!(
            "\n== {} [{}] {} exit={:?} reason={:?} cost={:?} files={:?}",
            attempt.step_id,
            attempt.kind,
            attempt.state,
            attempt.exit_code,
            attempt.reason,
            attempt.cost_usd,
            attempt.files_changed
        );
        for event in store.events(&attempt.id, 0).unwrap() {
            match event.kind.as_str() {
                "checkResult" => {
                    let tail = |text: &str| {
                        text.lines()
                            .rev()
                            .take(8)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    println!("dirty: {}", event.data["leftWorkspaceDirty"]);
                    println!("{}", tail(event.data["stdout"].as_str().unwrap_or("")));
                    println!("{}", tail(event.data["stderr"].as_str().unwrap_or("")));
                }
                "toolCall" if event.data["name"] == "Bash" => println!(
                    "bash: {}",
                    event.data["input"]["command"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(100)
                        .collect::<String>()
                ),
                "toolResult" => println!(
                    "  -> error={} {}",
                    event.data["isError"],
                    event.data["text"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .last()
                        .unwrap_or("")
                        .chars()
                        .take(120)
                        .collect::<String>()
                ),
                _ => {}
            }
        }
        if attempt.kind == "agent" {
            println!(
                "says: {}",
                attempt
                    .result_text
                    .unwrap_or_default()
                    .chars()
                    .take(500)
                    .collect::<String>()
            );
            let workspace = std::path::Path::new(&attempt.workspace);
            println!(
                "cache in workspace: {}",
                workspace.join(".keel-cache").exists()
            );
        }
    }
}
