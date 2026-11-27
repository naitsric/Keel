//! A real agent and a real check reaching a real Postgres on a local port, through the socket Keel
//! opens, while direct TCP stays blocked. Uses the real `claude`. No GitHub involved.
//! `cargo run -p keel-engine --example services_e2e -- <scratch dir> <python with psycopg> <port> [model]`

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{EnvVar, Service, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, StartRun};
use keel_harness::Tools;
use keel_store::{ProjectSettings, Store};

const TEST: &str = r#"import os, psycopg
with psycopg.connect(host=os.environ["DB_HOST"], user="postgres", dbname="postgres", connect_timeout=5) as c:
    c.execute("create table if not exists keel_probe (n int)")
    c.execute("insert into keel_probe values (1)")
    print("rows:", c.execute("select count(*) from keel_probe").fetchone()[0], "over", c.info.host)
"#;

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
    let python = args.next().expect("python with psycopg");
    let port: u16 = args.next().expect("port").parse().unwrap();
    let model = args.next().unwrap_or_else(|| "haiku".into());
    let repo = work.join("demo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Smoke"]);
    git(&repo, &["config", "user.email", "smoke@keel.invalid"]);
    std::fs::write(repo.join("db_test.py"), TEST).unwrap();
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
    engine
        .update_project(
            &project.id,
            &ProjectSettings {
                services: vec![Service::Bridge {
                    name: "db".into(),
                    port,
                    socket: Some(".s.PGSQL.5432".into()),
                }],
                env: vec![EnvVar {
                    name: "DB_HOST".into(),
                    value: "{{socketDir:db}}".into(),
                }],
                ..ProjectSettings::default()
            },
        )
        .unwrap();

    let prompt = format!(
        "Run exactly these two Bash commands, one after the other, without changing any file:\n1. {python} db_test.py\n2. {python} -c \"import socket; socket.create_connection(('127.0.0.1', {port}), 3); print('TCP-REACHED')\"\nThen say what each one printed. The second is expected to fail."
    );
    let workflow = vec![
        WorkflowStep::Agent(StepSpec {
            id: "agent".into(),
            model,
            effort: None,
            tools: ToolProfile::Build,
            prompt: prompt.clone(),
            max_turns: Some(6),
            max_budget_usd: None,
            paths: vec![],
            when: None,
            risks: vec![],
            model_when_risky: None,
            skill: None,
            role: Default::default(),
        }),
        WorkflowStep::Check {
            id: "check".into(),
            run: format!("{python} db_test.py"),
            only_if_changed: false,
        },
    ];
    let run = engine
        .start_run(StartRun {
            project_id: project.id,
            title: None,
            request: prompt,
            workflow,
            language: "en".into(),
            source_issue: None,
            saved_workflow: None,
            started_by: None,
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        if store.run(&run).unwrap().finished_at.is_some() {
            break;
        }
        let latest = store.attempts_of_run(&run).unwrap().pop();
        if let Some(a) = latest
            && !["queued", "preparing", "running", "stalled", "succeeded"]
                .contains(&a.state.as_str())
        {
            println!("stopped at {}: {} {:?}", a.step_id, a.state, a.reason);
            break;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(300));
    }
    for a in store.attempts_of_run(&run).unwrap() {
        println!(
            "\n== {} {} exit={:?} reason={:?}",
            a.step_id, a.state, a.exit_code, a.reason
        );
        for e in store.events(&a.id, 0).unwrap() {
            match e.kind.as_str() {
                "services" => println!("services env: {}", e.data["env"]),
                "toolCall" if e.data["name"] == "Bash" => println!(
                    "bash: {}",
                    e.data["input"]["command"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(90)
                        .collect::<String>()
                ),
                "toolResult" => println!(
                    "  -> error={} {}",
                    e.data["isError"],
                    e.data["text"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .last()
                        .unwrap_or("")
                        .chars()
                        .take(110)
                        .collect::<String>()
                ),
                "checkResult" => println!(
                    "check stdout: {} stderr: {}",
                    e.data["stdout"],
                    e.data["stderr"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .last()
                        .unwrap_or("")
                ),
                _ => {}
            }
        }
        if let Some(text) = a.result_text {
            println!(
                "says: {}",
                text.lines()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join(" | ")
                    .chars()
                    .take(320)
                    .collect::<String>()
            );
        }
    }
}
