//! Runs one real step through the engine with the real `claude`, on a synthetic repo.
//! `cargo run -p keel-engine --example smoke -- <scratch dir> [model]`

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{StepSpec, ToolProfile};
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

fn main() {
    let mut args = std::env::args().skip(1);
    let work = PathBuf::from(args.next().expect("scratch dir"));
    let model = args.next().unwrap_or_else(|| "haiku".into());
    std::fs::create_dir_all(&work).unwrap();
    let repo = work.join("demo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Smoke"]);
    git(&repo, &["config", "user.email", "smoke@keel.invalid"]);
    std::fs::write(repo.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
    std::fs::write(
        repo.join("test_calc.py"),
        "from calc import add\nassert add(2, 3) == 5\nprint('ok')\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("CLAUDE.md"),
        "Run the tests with: python3 test_calc.py\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    let store = Arc::new(Store::open(&work.join("keel.db")).unwrap());
    let tools = Tools::detect();
    println!("claude: {:?}", tools.claude);
    let engine = Engine::new(store.clone(), Config::new(work.join("data")), tools, |_| {});
    let preflight = engine.preflight();
    println!("version: {:?}", preflight.version);
    println!(
        "logged in: {:?}  blocker: {:?}",
        preflight.auth.as_ref().map(|a| a.logged_in),
        preflight.blocker
    );
    let project = engine.add_project(&repo).unwrap();
    let run = engine
        .start_run(StartRun {
            project_id: project.id,
            step: StepSpec {
                id: "builder".into(),
                model,
                effort: None,
                tools: ToolProfile::Build,
                prompt: "The add function in calc.py is wrong. Fix it, then run the tests.".into(),
                max_turns: Some(10),
                max_budget_usd: None,
            },
            language: "es".into(),
            allowed_domains: vec![],
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(180);
    let attempt = loop {
        let attempt = store.attempts_of_run(&run).unwrap().pop().unwrap();
        if !["queued", "preparing", "running", "stalled"].contains(&attempt.state.as_str()) {
            break attempt;
        }
        assert!(Instant::now() < deadline, "timed out in {}", attempt.state);
        std::thread::sleep(Duration::from_millis(200));
    };
    println!("\nstate: {}  reason: {:?}", attempt.state, attempt.reason);
    println!(
        "model: asked {} got {:?}  auth: {:?}",
        attempt.model_requested, attempt.model_actual, attempt.api_key_source
    );
    println!(
        "cost: {:?}  turns: {:?}  files: {:?}",
        attempt.cost_usd, attempt.turns, attempt.files_changed
    );
    println!("raw check: {:?}", engine.verify_raw(&attempt.id).unwrap());
    for event in store.events(&attempt.id, 0).unwrap() {
        let detail = match event.kind.as_str() {
            "toolCall" => format!(
                "{} {}",
                event.data["name"],
                event.data["input"]
                    .to_string()
                    .chars()
                    .take(100)
                    .collect::<String>()
            ),
            "toolResult" => format!(
                "error={} exit={} {}",
                event.data["isError"],
                event.data["exitCode"],
                event.data["text"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>()
                    .replace('\n', " ")
            ),
            "text" => event.data["text"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect::<String>()
                .replace('\n', " "),
            "init" => format!(
                "tools={} source={}",
                event.data["tools"], event.data["apiKeySource"]
            ),
            "finished" => format!("{} files={}", event.data["state"], event.data["files"]),
            _ => String::new(),
        };
        println!("  {:<12} {detail}", event.kind);
    }
    println!("\nsummary:\n{}", attempt.result_text.unwrap_or_default());
    println!("\ndiff:\n{}", engine.diff(&attempt.id).unwrap());

    // Continuing a finished session, the way "Continue" does after an interruption.
    let resumed = engine.retry(&attempt.id, true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let resumed = loop {
        let current = store.attempt(&resumed).unwrap();
        if !["queued", "preparing", "running", "stalled"].contains(&current.state.as_str()) {
            break current;
        }
        assert!(Instant::now() < deadline, "resume timed out");
        std::thread::sleep(Duration::from_millis(200));
    };
    println!(
        "\nresume: state={} reason={:?} number={} cost={:?}",
        resumed.state, resumed.reason, resumed.number, resumed.cost_usd
    );
    println!("{}", resumed.result_text.unwrap_or_default());
}
