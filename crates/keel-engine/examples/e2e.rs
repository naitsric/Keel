//! The whole flow against the real `claude` and the real GitHub. It opens a real draft pull request.
//! `cargo run -p keel-engine --example e2e -- <scratch dir> <path to a clone> <gh account> [model]`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_engine::{Config, Engine, StartRun, full_workflow};
use keel_harness::Tools;
use keel_store::{ProjectSettings, Store};

fn main() {
    let mut args = std::env::args().skip(1);
    let work = PathBuf::from(args.next().expect("scratch dir"));
    let repo = PathBuf::from(args.next().expect("clone"));
    let account = args.next().expect("gh account");
    let model = args.next().unwrap_or_else(|| "haiku".into());
    std::fs::create_dir_all(&work).unwrap();

    let store = Arc::new(Store::open(&work.join("keel.db")).unwrap());
    let engine = Engine::new(
        store.clone(),
        Config::new(work.join("data")),
        Tools::detect(),
        |_| {},
    );
    let project = engine.add_project(&repo).unwrap();
    println!("repo: {:?}", project.github_repo);
    engine
        .update_project(
            &project.id,
            &ProjectSettings {
                github_account: Some(account.clone()),
                setup_command: None,
                check_command: Some("python3 test_calc.py".into()),
                allowed_domains: vec![],
                allowed_read: vec![],
                services: vec![],
                env: vec![],
                default_workflow: None,
                sensitive_paths: vec![],
                allow_localhost: false,
                domains_seen: None,
            },
        )
        .unwrap();
    let status = engine.github_status(&project.id, Some(&account)).unwrap();
    println!(
        "accounts: {:?}",
        status
            .accounts
            .iter()
            .map(|a| (&a.login, a.active))
            .collect::<Vec<_>>()
    );
    println!(
        "info: {:?}  excess: {:?}  error: {:?}",
        status.info, status.excess_scopes, status.error
    );

    let run = engine
        .start_run(StartRun {
            project_id: project.id.clone(),
            title: None,
            request: "The add function in calc.py returns the wrong result. Fix it so the tests in test_calc.py pass.".into(),
            workflow: full_workflow("es", &model, &model, Some("python3 test_calc.py")),
            language: "es".into(),
            source_issue: None,
            saved_workflow: None,
        started_by: None, })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(400);
    let mut approved = false;
    loop {
        let attempts = store.attempts_of_run(&run).unwrap();
        let latest = attempts.last().unwrap();
        if latest.state == "waitingApproval" && !approved {
            println!(
                "approval reached. pr so far: {:?}",
                store.run(&run).unwrap().pr_url
            );
            engine
                .answer_approval(&latest.id, true, "Mantén los cambios pequeños")
                .unwrap();
            approved = true;
        }
        if store.run(&run).unwrap().finished_at.is_some() {
            break;
        }
        if ![
            "queued",
            "preparing",
            "running",
            "stalled",
            "waitingApproval",
            "succeeded",
        ]
        .contains(&latest.state.as_str())
        {
            println!(
                "stopped at {}: {} {:?}",
                latest.step_id, latest.state, latest.reason
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out at {} in {}",
            latest.step_id,
            latest.state
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    for a in store.attempts_of_run(&run).unwrap() {
        println!(
            "{:<14} {:<10} model={:?} exit={:?} cost={:?} files={:?} reason={:?}",
            a.step_id, a.state, a.model_actual, a.exit_code, a.cost_usd, a.files_changed, a.reason
        );
    }
    let finished = store.run(&run).unwrap();
    println!(
        "base: {:?}  branch: {:?}",
        finished.base_ref, finished.branch
    );
    println!("PR: {:?}", finished.pr_url);
}
