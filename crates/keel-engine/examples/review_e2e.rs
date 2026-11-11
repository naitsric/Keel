//! The user's whole diagram against the real `claude` and the real GitHub, with a different model
//! per step. It opens a real draft pull request. The approver is told to send the first attempt
//! back, so a real round of rework happens.
//! `cargo run -p keel-engine --example review_e2e -- <scratch dir> <path to a clone> <gh account>`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{StepSpec, WorkflowStep};
use keel_engine::{Config, Engine, StartRun};
use keel_harness::Tools;
use keel_store::{ProjectSettings, Store};

const CHECK: &str = "python3 test_calc.py";

fn model_for(id: &str) -> &'static str {
    match id {
        "builder" | "codigo" | "aprobador" => "sonnet",
        _ => "haiku",
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let work = PathBuf::from(args.next().expect("scratch dir"));
    let repo = PathBuf::from(args.next().expect("clone"));
    let account = args.next().expect("gh account");
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
                github_account: Some(account),
                setup_command: None,
                check_command: Some(CHECK.into()),
                allowed_domains: vec![],
                allowed_read: vec![],
                services: vec![],
                env: vec![],
                default_workflow: None,
                sensitive_paths: vec![],
                allow_localhost: false,
            },
        )
        .unwrap();

    // The map first, as when a project is added.
    let map_run = engine.generate_map(&project.id, "sonnet", "es").unwrap();
    let deadline = Instant::now() + Duration::from_secs(300);
    while store.run(&map_run).unwrap().finished_at.is_none() {
        let attempt = store.attempts_of_run(&map_run).unwrap().remove(0);
        if ["failed", "blocked", "stopped", "interrupted"].contains(&attempt.state.as_str()) {
            panic!("the map stopped: {} {:?}", attempt.state, attempt.reason);
        }
        assert!(Instant::now() < deadline, "the map timed out");
        std::thread::sleep(Duration::from_millis(300));
    }
    println!("--- the map");
    let map_attempt = store.attempts_of_run(&map_run).unwrap().remove(0);
    println!(
        "model: {:?}  cost: {:?}",
        map_attempt.model_actual, map_attempt.cost_usd
    );
    for note in engine.brain(&project.id).unwrap().map {
        println!(
            "- {}  {:?} @{}",
            note.note.text,
            note.note.paths,
            note.note
                .commit_sha
                .as_deref()
                .unwrap_or("")
                .get(..7)
                .unwrap_or("")
        );
    }

    let mut steps = engine.workflows(&project.id, "es").unwrap().remove(0).steps;
    let choose = |spec: &mut StepSpec| {
        spec.model = model_for(&spec.id).to_owned();
        if spec.id == "aprobador" {
            spec.prompt.push_str("\n\nRegla adicional de esta prueba: si CHANGELOG.md no aparece en el cambio, decision = rework e instructions = «Agrega a CHANGELOG.md una línea que explique el arreglo». Si aparece, ignora esta regla.");
        }
    };
    for step in &mut steps {
        match step {
            WorkflowStep::Agent(spec) => choose(spec),
            WorkflowStep::Parallel { steps: group, .. } => group.iter_mut().for_each(choose),
            _ => {}
        }
    }

    let run = engine
        .start_run(StartRun {
            project_id: project.id.clone(),
            title: None,
            request: "La función add de calc.py devuelve un resultado equivocado. Corrígela para que pasen las pruebas de test_calc.py.".into(),
            workflow: steps,
            language: "es".into(),
            source_issue: None,
            saved_workflow: None,
        started_by: None, })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(480);
    let mut approved = false;
    loop {
        let attempts = store.attempts_of_run(&run).unwrap();
        if let Some(waiting) = attempts.iter().find(|a| a.state == "waitingApproval")
            && !approved
        {
            println!("approval reached, round {}", store.run(&run).unwrap().round);
            engine
                .answer_approval(&waiting.id, true, "Mantén los cambios pequeños")
                .unwrap();
            approved = true;
        }
        if store.run(&run).unwrap().finished_at.is_some() {
            break;
        }
        if let Some(stopped) = attempts.iter().rev().find(|a| {
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

    println!(
        "{:<14} {:<6} {:<11} {:<8} {:<26} {:<9} detail",
        "step", "round", "state", "asked", "actual", "cost"
    );
    for a in store.attempts_of_run(&run).unwrap() {
        let detail = a
            .output
            .as_ref()
            .map(|o| {
                format!(
                    "{} {} {}",
                    o["verdict"].as_str().unwrap_or(""),
                    o["decision"].as_str().unwrap_or(""),
                    o["risk"].as_str().unwrap_or("")
                )
            })
            .or(a.reason.as_ref().map(ToString::to_string))
            .unwrap_or_default();
        println!(
            "{:<14} {:<6} {:<11} {:<8} {:<26} {:<9} {}",
            a.step_id,
            a.round,
            a.state,
            a.model_requested,
            a.model_actual.as_deref().unwrap_or("-"),
            a.cost_usd.map_or("-".to_owned(), |c| format!("${c:.3}")),
            detail.split_whitespace().collect::<Vec<_>>().join(" "),
        );
    }
    let finished = store.run(&run).unwrap();
    println!(
        "round: {}  branch: {:?}  PR: {:?}",
        finished.round, finished.branch, finished.pr_url
    );
    let builder = store
        .attempts_of_run(&run)
        .unwrap()
        .into_iter()
        .find(|a| a.step_id == "builder")
        .unwrap();
    println!(
        "the builder got from the Brain: {}",
        builder.system_prompt_sources
    );
    println!("--- proposals for the Brain");
    for note in engine.proposals_of_run(&run).unwrap() {
        println!(
            "- {}\n  evidence: {}\n  paths: {:?}",
            note.text,
            note.evidence.unwrap_or_default(),
            note.paths
        );
    }
    println!(
        "--- the diff of the whole run\n{}",
        engine.run_diff(&run).unwrap()
    );
}
