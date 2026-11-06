//! The workflow that fits the change, against the real `claude`, on a synthetic repo. It doesn't touch
//! GitHub. Any question is answered with something other than the recommended option, so a real session
//! resumes. It stops at nothing else: approvals are approved.
//! `cargo run -p keel-engine --example fits_e2e -- <scratch dir> [asks|decides|small]`

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::WorkflowStep;
use keel_engine::{Answer, AnswerChoice, Config, Engine, StartRun};
use keel_harness::Tools;
use keel_store::{ProjectSettings, Store};
use serde_json::Value;

const CHECK: &str = "python3 test_calc.py";
/// What each request is there to show. Nothing in the repo says how much the discount is, so only a
/// person can say. The other two can be decided by reading the code.
const REQUESTS: [(&str, &str); 3] = [
    (
        "asks",
        "Agrega a calc.py una función discount(price) que aplique el descuento de los clientes frecuentes.",
    ),
    (
        "decides",
        "La función percent de calc.py se rompe cuando whole es 0. Arréglala.",
    ),
    (
        "small",
        "Agrega a calc.py una función triple(x) que devuelva el triple de x.",
    ),
];

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn make_repo(work: &Path) -> PathBuf {
    let repo = work.join("demo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Fits"]);
    git(&repo, &["config", "user.email", "fits@keel.invalid"]);
    std::fs::write(
        repo.join("calc.py"),
        "def add(a, b):\n    return a + b\n\n\ndef percent(part, whole):\n    return part / whole * 100\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("test_calc.py"),
        "from calc import add, percent\n\nassert add(2, 3) == 5\nassert percent(1, 4) == 25\nprint('ok')\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

fn print_spec(output: &Value) {
    let summary = text(output, "summary");
    println!("  summary ({} chars): {summary}", summary.chars().count());
    println!(
        "  size: {}  risks: {}",
        text(output, "size"),
        output["risks"]
    );
    for criterion in output["criteria"].as_array().into_iter().flatten() {
        println!(
            "  {}: when {} / then {}",
            text(criterion, "id"),
            text(criterion, "when"),
            text(criterion, "then")
        );
    }
    for decided in output["decided"].as_array().into_iter().flatten() {
        println!("  decided: {}", text(decided, "text"));
    }
}

/// Answers every question with the option that wasn't recommended.
fn answer(engine: &Engine, attempt: &keel_store::Attempt) {
    let output = attempt.output.clone().unwrap_or_default();
    println!("--- stopped to ask");
    print_spec(&output);
    let mut answers = Vec::new();
    for question in output["questions"].as_array().into_iter().flatten() {
        println!(
            "  question ({} chars): {}",
            text(question, "text").chars().count(),
            text(question, "text")
        );
        let options = question["options"].as_array().cloned().unwrap_or_default();
        for option in &options {
            println!(
                "    {} {}",
                if option["recommended"] == true {
                    "*"
                } else {
                    "-"
                },
                text(option, "label")
            );
        }
        let chosen = options
            .iter()
            .find(|option| option["recommended"] != true)
            .expect("a question has more than one option");
        println!("  answering: {}", text(chosen, "label"));
        answers.push(Answer {
            question: text(question, "id").to_owned(),
            choice: AnswerChoice::Option {
                label: text(chosen, "label").to_owned(),
            },
            remember: false,
        });
    }
    engine.answer_questions(&attempt.id, &answers).unwrap();
}

fn run_request(engine: &Engine, store: &Store, project_id: &str, request: &str) {
    println!("\n===== {request}");
    let mut steps = engine
        .workflows(project_id, "es")
        .unwrap()
        .into_iter()
        .find(|workflow| workflow.id == "builtin:organic")
        .expect("the built-in that fits the change")
        .steps;
    steps.retain(|step| !matches!(step, WorkflowStep::Deliver { .. }));
    let run = engine
        .start_run(StartRun {
            project_id: project_id.to_owned(),
            title: None,
            request: request.to_owned(),
            workflow: steps,
            saved_workflow: None,
            language: "es".into(),
            source_issue: None,
            started_by: None,
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        if store.run(&run).unwrap().finished_at.is_some() {
            break;
        }
        let attempts = store.attempts_of_run(&run).unwrap();
        if let Some(waiting) = attempts.iter().find(|a| a.state == "waitingAnswers") {
            answer(engine, waiting);
        } else if let Some(waiting) = attempts.iter().find(|a| a.state == "waitingApproval") {
            println!("--- reached a person's approval, round {}", waiting.round);
            engine.answer_approval(&waiting.id, true, "").unwrap();
        } else if let Some(stopped) = attempts.iter().rev().find(|a| {
            ["failed", "blocked", "stopped", "stalled", "interrupted"].contains(&a.state.as_str())
        }) {
            println!(
                "STOPPED at {}: {} {:?}",
                stopped.step_id, stopped.state, stopped.reason
            );
            break;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(300));
    }

    let attempts = store.attempts_of_run(&run).unwrap();
    println!("--- what happened");
    println!(
        "{:<14} {:<6} {:<11} {:<8} {:<9} detail",
        "step", "round", "state", "model", "cost"
    );
    for attempt in &attempts {
        let detail = attempt
            .reason
            .as_ref()
            .map_or_else(String::new, ToString::to_string);
        println!(
            "{:<14} {:<6} {:<11} {:<8} {:<9} {detail}",
            attempt.step_id,
            attempt.round,
            attempt.state,
            attempt.model_requested,
            attempt
                .cost_usd
                .map_or("-".to_owned(), |cost| format!("${cost:.3}")),
        );
    }
    if let Some(spec) = attempts.iter().rev().find(|a| a.role == "specifier")
        && let Some(output) = &spec.output
    {
        println!("--- what it understood, in the end");
        print_spec(output);
        println!("  answered: {}", output["answered"]);
    }
    if let Some(output) = attempts
        .iter()
        .rev()
        .find(|a| a.role == "verifier")
        .and_then(|a| a.output.as_ref())
    {
        println!("--- what it checked");
        for item in output["criteria"].as_array().into_iter().flatten() {
            println!(
                "  {}: {} ({})",
                text(item, "id"),
                text(item, "result"),
                text(item, "evidence")
            );
        }
    }
    if let Some(builder) = attempts.iter().rev().find(|a| a.step_id == "builder") {
        println!("--- the change");
        println!("  files: {:?}", builder.files_changed);
    }
}

fn main() {
    let work = PathBuf::from(std::env::args().nth(1).expect("scratch dir"));
    std::fs::create_dir_all(&work).unwrap();
    let repo = make_repo(&work);
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
                github_account: None,
                setup_command: None,
                check_command: Some(CHECK.into()),
                allowed_domains: vec![],
                allowed_read: vec![],
                services: vec![],
                env: vec![],
                default_workflow: None,
                sensitive_paths: vec![],
            },
        )
        .unwrap();

    let only = std::env::args().nth(2);
    for (name, request) in REQUESTS {
        if only.as_deref().is_none_or(|only| only == name) {
            run_request(&engine, &store, &project.id, request);
        }
    }
}
