//! The built-in review of code, which reviews with Claude Code's own `/code-review`, against the
//! real `claude` on a synthetic repo. A builder adds two functions, and the review has to read
//! what Keel asks from its file and answer as a review does, for the run to reach the person.
//! The fake `claude` of the tests can't show this: the real one says everything under the command
//! is a fork of the step, and its last line has no answer in it.
//! `cargo run -p keel-engine --example skill_review -- <scratch dir> [level]`

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{StepRole, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, StartRun};
use keel_harness::Tools;
use keel_store::Store;

fn git(repo: &std::path::Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn short(value: &serde_json::Value, limit: usize) -> String {
    let text = value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned);
    text.chars()
        .take(limit)
        .collect::<String>()
        .replace('\n', " ")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let work = PathBuf::from(args.next().expect("scratch dir"));
    let level = args.next();
    std::fs::create_dir_all(&work).unwrap();
    let repo = work.join("demo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Skill"]);
    git(&repo, &["config", "user.email", "skill@keel.invalid"]);
    std::fs::write(repo.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    let store = Arc::new(Store::open(&work.join("keel.db")).unwrap());
    let engine = Engine::new(
        store.clone(),
        Config::new(work.join("data")),
        Tools::detect(),
        |_| {},
    );
    let project = engine.add_project(&repo).unwrap();

    // The review of code exactly as the full workflow has it, with nothing else around it.
    let full = engine.workflows(&project.id, "en").unwrap().remove(0);
    let mut code = full
        .steps
        .iter()
        .find_map(|step| match step {
            WorkflowStep::Parallel { steps, .. } => {
                steps.iter().find(|spec| spec.skill.is_some()).cloned()
            }
            _ => None,
        })
        .expect("a built-in review with a skill");
    if let Some(level) = level {
        code.skill = Some(format!("code-review {level}"));
    }
    println!(
        "review: {}  skill: {:?}  model: {}",
        code.id, code.skill, code.model
    );
    let review = code.id.clone();
    let builder = StepSpec {
        id: "builder".into(),
        model: "haiku".into(),
        effort: None,
        tools: ToolProfile::Build,
        prompt: "Add two functions to calc.py, exactly as said here and with nothing else: `average(values)` returns `sum(values) / len(values)`, and `share(total, people)` returns `total / people`. Don't handle any special case and don't add tests. Commit it.".into(),
        max_turns: Some(15),
        max_budget_usd: None,
        paths: vec![],
        when: None,
        risks: vec![],
        model_when_risky: None,
        skill: None,
        role: StepRole::Worker,
    };
    let run = engine
        .start_run(StartRun {
            project_id: project.id.clone(),
            title: None,
            request: "Add average and share to calc.py.".into(),
            workflow: vec![
                WorkflowStep::Agent(builder),
                WorkflowStep::Parallel {
                    id: "reviews".into(),
                    steps: vec![code],
                },
                WorkflowStep::Approval { id: "human".into() },
            ],
            language: "en".into(),
            source_issue: None,
            saved_workflow: None,
            started_by: None,
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let attempts = store.attempts_of_run(&run).unwrap();
        if attempts.iter().any(|a| a.state == "waitingApproval") {
            break;
        }
        if let Some(stopped) = attempts.iter().find(|a| {
            ["failed", "blocked", "stopped", "stalled", "interrupted"].contains(&a.state.as_str())
        }) {
            println!(
                "stopped at {}: {} {:?}",
                stopped.step_id, stopped.state, stopped.reason
            );
            break;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(300));
    }

    for attempt in store.attempts_of_run(&run).unwrap() {
        println!(
            "\n=== {}  state: {}  reason: {:?}",
            attempt.step_id, attempt.state, attempt.reason
        );
        println!(
            "model: asked {} got {:?}  cost: {:?}  turns: {:?}  tokens in/out: {:?}/{:?}",
            attempt.model_requested,
            attempt.model_actual,
            attempt.cost_usd,
            attempt.turns,
            attempt.input_tokens,
            attempt.output_tokens
        );
        if attempt.step_id != review {
            continue;
        }
        for event in store.events(&attempt.id, 0).unwrap() {
            let detail = match event.kind.as_str() {
                "started" => format!("skill: {}", short(&event.data["skill"], 400)),
                "init" => format!("tools={}", event.data["tools"]),
                "toolCall" => format!(
                    "{} {}",
                    short(&event.data["name"], 40),
                    short(&event.data["input"], 160)
                ),
                "toolResult" => format!(
                    "error={} {}",
                    event.data["isError"],
                    short(&event.data["text"], 100)
                ),
                "text" => short(&event.data["text"], 160),
                "result" => format!(
                    "{} text={} answer in it: {}",
                    short(&event.data["subtype"], 40),
                    short(&event.data["text"], 60),
                    !event.data["structuredOutput"].is_null()
                ),
                "permissionDenied" => short(&event.data["detail"], 300),
                _ => String::new(),
            };
            println!("  {:<12} {detail}", event.kind);
        }
        println!(
            "\nwhat the review answered:\n{}",
            serde_json::to_string_pretty(&attempt.output).unwrap()
        );
    }
    println!("\nthe change:\n{}", engine.run_diff(&run).unwrap());
}
