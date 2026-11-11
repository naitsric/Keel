//! The engine end to end, against a fake `claude` that behaves as M0 measured the real one.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, MASK, Masked, Reason, StepSpec, ToolProfile, mask_secrets};
use keel_engine::{Config, Engine, Error, RawCheck, StartRun};
use keel_harness::Tools;
use keel_store::Store;

const BUILD_TOOLS: &str = r#"["Read","Grep","Glob","Edit","Write","Bash"]"#;

struct Harness {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
    repo: PathBuf,
}

/// The fake's body runs for `claude -p`. The rest of its CLI answers like a signed-in session.
fn write_fake(bin: &Path, body: &str, auth: &str, version: &str) {
    std::fs::create_dir_all(bin).unwrap();
    let script = format!(
        "#!/bin/bash\ncase \"$1\" in\n--version) echo '{version} (Claude Code)'; exit 0;;\nauth) echo '{auth}'; exit 0;;\nesac\n{body}\n"
    );
    let path = bin.join("claude");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn init(tools: &str, source: &str) -> String {
    format!(
        r#"echo '{{"type":"system","subtype":"init","session_id":"s","model":"claude-fake-1","tools":{tools},"apiKeySource":"{source}","permissionMode":"dontAsk","claude_code_version":"2.1.318"}}'"#
    )
}

const OK_RESULT: &str = r#"echo '{"type":"result","subtype":"success","is_error":false,"result":"Fixed it and the tests pass.","num_turns":2,"duration_ms":1500,"total_cost_usd":0.25,"usage":{"input_tokens":100,"output_tokens":20},"modelUsage":{"claude-fake-1":{}},"permission_denials":[]}'"#;

const LOGGED_IN: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max","email":"a@example.com"}"#;

fn git(repo: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn harness_with(body: &str, auth: &str, version: &str, tune: impl FnOnce(&mut Config)) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    write_fake(&bin, body, auth, version);
    let repo = dir.path().join("demo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Tester"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("CLAUDE.md"), "Project rule: keep it simple.\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    config.stop_grace = Duration::from_millis(500);
    tune(&mut config);
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let project_id = engine.add_project(&repo).unwrap().id;
    Harness {
        dir,
        store,
        engine,
        project_id,
        repo,
    }
}

fn harness(body: &str) -> Harness {
    harness_with(body, LOGGED_IN, "2.1.318", |_| {})
}

fn step(tools: ToolProfile) -> StepSpec {
    StepSpec {
        id: "builder".into(),
        model: "fake".into(),
        effort: None,
        tools,
        prompt: "Fix the bug\nwith care".into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        role: Default::default(),
    }
}

impl Harness {
    fn start(&self, tools: ToolProfile) -> String {
        self.engine
            .start_run(StartRun::one_step(
                self.project_id.clone(),
                step(tools),
                "es".into(),
            ))
            .unwrap()
    }

    fn latest(&self, run: &str) -> keel_store::Attempt {
        self.store.attempts_of_run(run).unwrap().pop().unwrap()
    }

    fn wait_for(&self, run: &str, states: &[AttemptState]) -> keel_store::Attempt {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let attempt = self.latest(run);
            if states.iter().any(|s| s.as_str() == attempt.state) {
                return attempt;
            }
            assert!(
                Instant::now() < deadline,
                "stuck in {} ({:?})",
                attempt.state,
                attempt.reason
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// What a step waiting for the project's turn is told while the builder of `run` has it.
    fn waiting_for(&self, run: &str) -> Reason {
        Reason::WaitingForProject {
            step: "builder".into(),
            run_id: run.to_owned(),
            title: self.store.run(run).unwrap().title,
        }
    }

    fn wait_for_reason(&self, run: &str, expected: &Reason) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let attempt = self.latest(run);
            let found = attempt.reason.clone().map(|_| reason(&attempt));
            if found.as_ref() == Some(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "still {found:?} in {}",
                attempt.state
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn kinds(&self, attempt: &str) -> Vec<String> {
        self.store
            .events(attempt, 0)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }
}

fn reason(attempt: &keel_store::Attempt) -> Reason {
    serde_json::from_value(attempt.reason.clone().expect("has a reason")).unwrap()
}

#[test]
fn a_step_runs_and_everything_it_did_is_recorded() {
    keel_harness::needs!(sandbox);
    let body = format!(
        r#"cat > /dev/null
{}
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Working"}},{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"make test"}}}}]}}}}'
echo '{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"Exit code 3"}}]}}}}'
echo changed > fixed.txt
{OK_RESULT}"#,
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(
        &run,
        &[
            AttemptState::Succeeded,
            AttemptState::Failed,
            AttemptState::Blocked,
        ],
    );
    assert_eq!(a.state, "succeeded", "{:?}", a.reason);

    // Requested and real model are both there, with the account and the cost.
    assert_eq!(a.model_requested, "fake");
    assert_eq!(a.model_actual.as_deref(), Some("claude-fake-1"));
    assert_eq!(a.api_key_source.as_deref(), Some("none"));
    assert_eq!(a.account.as_deref(), Some("a@example.com (max)"));
    assert_eq!(a.cost_usd, Some(0.25));
    assert_eq!(a.turns, Some(2));
    assert_eq!(
        a.result_text.as_deref(),
        Some("Fixed it and the tests pass.")
    );

    // What it changed, before and after.
    assert_eq!(
        a.files_changed.as_ref().unwrap(),
        &serde_json::json!(["fixed.txt"])
    );
    assert_ne!(a.sha_before, a.sha_after);
    assert!(h.engine.diff(&a.id).unwrap().contains("+changed"));

    // The audit trail, in order, including the failed command's exit code.
    assert_eq!(
        h.kinds(&a.id),
        [
            "started",
            "init",
            "text",
            "toolCall",
            "toolResult",
            "result",
            "finished"
        ]
    );
    let events = h.store.events(&a.id, 0).unwrap();
    assert_eq!(events[3].data["input"]["command"], "make test");
    assert_eq!(events[4].data["exitCode"], 3);
    assert!(events[0].data["args"].to_string().contains("--restricted"));

    // The agent got the project's instructions and the closing request, in the app's language.
    let system = events[0].data["systemPrompt"].as_str().unwrap();
    assert!(system.contains("Project rule: keep it simple."));
    assert!(system.contains("Spanish"));
    assert_eq!(a.system_prompt_sources[0]["source"], "CLAUDE.md");
}

#[test]
fn the_users_repo_is_never_touched_and_the_clone_has_no_remote() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\necho x > new.txt\ngit add -A\ngit commit -qm work\n{OK_RESULT}",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let before = (
        git(&h.repo, &["rev-parse", "HEAD"]),
        git(&h.repo, &["for-each-ref"]),
        git(&h.repo, &["status", "--porcelain"]),
    );
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Succeeded]);
    assert_eq!(
        before,
        (
            git(&h.repo, &["rev-parse", "HEAD"]),
            git(&h.repo, &["for-each-ref"]),
            git(&h.repo, &["status", "--porcelain"])
        )
    );
    assert!(!h.repo.join("new.txt").exists());

    let workspace = Path::new(&a.workspace);
    assert!(workspace.join("new.txt").exists());
    assert_eq!(git(workspace, &["remote"]), "");
    assert!(git(workspace, &["branch", "--show-current"]).starts_with("keel/"));
    assert_eq!(a.files_changed.unwrap(), serde_json::json!(["new.txt"]));
}

#[test]
fn credentials_in_the_environment_do_not_reach_the_agent() {
    keel_harness::needs!(sandbox);
    // SAFETY: only this test touches these variables, and they aren't read concurrently elsewhere.
    unsafe {
        std::env::set_var("GH_TOKEN", "must-not-leak");
        std::env::set_var("ANTHROPIC_API_KEY", "must-not-leak");
    }
    let body = format!(
        "cat > /dev/null\n{}\nenv > env.txt\n{OK_RESULT}",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Succeeded]);
    let env = std::fs::read_to_string(Path::new(&a.workspace).join("env.txt")).unwrap();
    assert!(!env.contains("must-not-leak"));
    assert!(env.contains("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1"));
    assert!(
        h.engine
            .preflight()
            .ignored_env
            .contains(&"ANTHROPIC_API_KEY".to_owned())
    );
    unsafe {
        std::env::remove_var("GH_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
}

#[test]
fn an_unexpected_login_blocks_the_step() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\ntrap 'exit 0' INT\nsleep 30 & wait",
        init(BUILD_TOOLS, "ANTHROPIC_API_KEY")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Blocked]);
    assert_eq!(
        reason(&a),
        Reason::UnexpectedAuth {
            source: "ANTHROPIC_API_KEY".into()
        }
    );
}

#[test]
fn tools_other_than_requested_block_the_step() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\ntrap 'exit 0' INT\nsleep 30 & wait",
        init(r#"["Read","Bash","Agent"]"#, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Blocked]);
    assert!(matches!(reason(&a), Reason::UnexpectedTools { .. }));
}

#[test]
fn a_rejected_session_is_blocked_even_with_subtype_success() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":true,\"result\":\"Invalid API key\"}}'\nexit 1",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Blocked, AttemptState::Succeeded]);
    assert_eq!(a.state, "blocked");
    assert!(matches!(reason(&a), Reason::SessionRejected { .. }));
}

#[test]
fn a_usage_limit_blocks_with_its_reset_time() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\necho '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"resetsAt\":1800000000}}}}'\nexit 1",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Blocked]);
    assert_eq!(
        reason(&a),
        Reason::RateLimited {
            resets_at: Some(1800000000)
        }
    );
}

#[test]
fn stopping_sends_sigint_and_keeps_the_result() {
    keel_harness::needs!(sandbox);
    let interrupted = r#"echo '{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":1,"output_tokens":1}}'"#;
    let body = format!(
        "cat > /dev/null\non_int() {{ {interrupted}; exit 0; }}\ntrap on_int INT\n{}\nsleep 60 & wait",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let running = h.wait_for(&run, &[AttemptState::Running]);
    h.wait_until_kind(&running.id, "init");
    h.engine.stop(&running.id).unwrap();
    let a = h.wait_for(&run, &[AttemptState::Stopped]);
    assert_eq!(a.cost_usd, Some(0.01));
    assert!(h.kinds(&a.id).contains(&"stopRequested".to_owned()));
    assert!(h.kinds(&a.id).contains(&"result".to_owned()));
}

#[test]
fn a_step_that_ignores_sigint_is_escalated() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\ntrap '' INT\n{}\nwhile true; do sleep 0.1; done",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let running = h.wait_for(&run, &[AttemptState::Running]);
    h.wait_until_kind(&running.id, "init");
    h.engine.stop(&running.id).unwrap();
    h.wait_for(&run, &[AttemptState::Stopped]);
}

impl Harness {
    fn wait_for_attempt(&self, id: &str, state: AttemptState) -> keel_store::Attempt {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let attempt = self.store.attempt(id).unwrap();
            if attempt.state == state.as_str() {
                return attempt;
            }
            assert!(
                Instant::now() < deadline,
                "stuck in {} ({:?})",
                attempt.state,
                attempt.reason
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_until_kind(&self, attempt: &str, kind: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.kinds(attempt).iter().any(|k| k == kind) {
            assert!(Instant::now() < deadline, "no {kind} event");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn a_quiet_idle_step_is_reported_as_stalled_and_never_killed_on_its_own() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\nsleep 2345\n{OK_RESULT}",
        init(BUILD_TOOLS, "none")
    );
    let h = harness_with(&body, LOGGED_IN, "2.1.318", |c| {
        c.stall_after = Duration::from_millis(300)
    });
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Stalled]);
    let Reason::Stalled { command, pid, .. } = reason(&a) else {
        panic!("wrong reason")
    };
    assert_eq!(command.as_deref(), Some("sleep 2345"));
    assert!(pid.is_some());

    // It stays stalled, with the process alive, until a person chooses.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(h.latest(&run).state, "stalled");

    // Killing the stuck command lets the agent carry on.
    h.engine.kill_command(&a.id).unwrap();
    let done = h.wait_for(&run, &[AttemptState::Succeeded]);
    let kinds = h.kinds(&done.id);
    assert!(kinds.iter().any(|k| k == "stalled"));
    assert!(kinds.iter().any(|k| k == "commandKilled"));
    assert!(kinds.iter().any(|k| k == "stallCleared"));
}

/// A fake whose only output after init is a `tool_progress` heartbeat every second, as Claude
/// Code writes while a command runs. `command` runs in the background below it. The loop uses
/// only builtins so the command stays the leaf of the process tree.
fn heartbeat_only(command: &str) -> String {
    format!(
        "cat > /dev/null\n{}\n{command} &\nbg=$!\ntrap 'kill $bg; exit 0' INT\nmkfifo hb\nexec 3<>hb\nwhile true; do\necho '{{\"type\":\"tool_progress\",\"tool_name\":\"Bash\",\"elapsed_time_seconds\":30}}'\nread -t 1 -u 3\ndone",
        init(BUILD_TOOLS, "none")
    )
}

#[test]
fn a_hung_command_is_stalled_even_while_heartbeats_keep_coming() {
    keel_harness::needs!(sandbox);
    let h = harness_with(&heartbeat_only("sleep 2345"), LOGGED_IN, "2.1.318", |c| {
        c.stall_after = Duration::from_millis(300)
    });
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Stalled]);
    let Reason::Stalled { command, pid, .. } = reason(&a) else {
        panic!("wrong reason")
    };
    assert_eq!(command.as_deref(), Some("sleep 2345"));
    assert!(pid.is_some());

    // More heartbeats arrive and the warning stays: they don't clear it.
    let before = h.store.attempt(&a.id).unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    let after = h.store.attempt(&a.id).unwrap();
    assert_eq!(after.state, "stalled");
    assert!(after.raw_offset > before.raw_offset, "heartbeats were read");
    assert_eq!(
        after.last_activity_at, before.last_activity_at,
        "and they don't move the last activity"
    );
    assert!(!h.kinds(&a.id).iter().any(|k| k == "stallCleared"));

    h.engine.stop(&a.id).unwrap();
    h.wait_for(&run, &[AttemptState::Stopped, AttemptState::Succeeded]);
}

#[test]
fn a_command_using_cpu_is_not_stalled_by_heartbeats_alone() {
    keel_harness::needs!(sandbox);
    let h = harness_with(
        &heartbeat_only("while :; do :; done"),
        LOGGED_IN,
        "2.1.318",
        |c| c.stall_after = Duration::from_millis(300),
    );
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Running]);
    h.wait_until_kind(&a.id, "init");
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(h.latest(&run).state, "running");
    assert!(!h.kinds(&a.id).iter().any(|k| k == "stalled"));
    h.engine.stop(&a.id).unwrap();
    h.wait_for(&run, &[AttemptState::Stopped, AttemptState::Succeeded]);
}

#[test]
fn waiting_silences_the_stall_warning() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\ntrap 'exit 0' INT\nsleep 3456 & wait",
        init(BUILD_TOOLS, "none")
    );
    let h = harness_with(&body, LOGGED_IN, "2.1.318", |c| {
        c.stall_after = Duration::from_millis(200)
    });
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Stalled]);
    h.engine.wait_longer(&a.id).unwrap();
    assert_eq!(h.latest(&run).state, "running");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(h.latest(&run).state, "running", "snoozed");
    h.engine.stop(&a.id).unwrap();
    h.wait_for(&run, &[AttemptState::Stopped]);
}

#[test]
fn a_project_runs_one_command_step_at_a_time_and_reviews_are_not_held_back() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\nread_only=0; case \"$*\" in *Read,Grep,Glob\\ *) read_only=1;; esac\n\
         if [ \"$read_only\" = 1 ]; then {}; {OK_RESULT}; exit 0; fi\n{}\nsleep 1\n{OK_RESULT}",
        init(r#"["Read","Grep","Glob"]"#, "none"),
        init(BUILD_TOOLS, "none"),
    );
    let h = harness_with(&body, LOGGED_IN, "2.1.318", |c| c.max_agents = 5);
    let first = h.start(ToolProfile::Build);
    h.wait_for(&first, &[AttemptState::Running]);
    let second = h.start(ToolProfile::Build);
    h.wait_for(&second, &[AttemptState::Queued]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.latest(&second).reason.is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(reason(&h.latest(&second)), h.waiting_for(&first));

    // A read-only step can't run commands, so it doesn't wait.
    let review = h.start(ToolProfile::ReadOnly);
    h.wait_for(&review, &[AttemptState::Succeeded]);
    assert_eq!(
        h.latest(&second).state,
        "queued",
        "still waiting for the first"
    );

    h.wait_for(&first, &[AttemptState::Succeeded]);
    let a = h.wait_for(&second, &[AttemptState::Succeeded]);
    assert!(a.reason.is_none());
}

#[test]
fn a_step_waiting_for_its_turn_names_the_one_that_has_it_as_the_turn_passes() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\nsleep 1\n{OK_RESULT}",
        init(BUILD_TOOLS, "none")
    );
    let h = harness_with(&body, LOGGED_IN, "2.1.318", |c| c.max_agents = 5);
    let first = h.start(ToolProfile::Build);
    h.wait_for(&first, &[AttemptState::Running]);
    let second = h.start(ToolProfile::Build);
    let third = h.start(ToolProfile::Build);
    h.wait_for_reason(&second, &h.waiting_for(&first));
    h.wait_for_reason(&third, &h.waiting_for(&first));

    // The first ends, one of the others takes the turn, and the last one now names that one.
    h.wait_for(&first, &[AttemptState::Succeeded]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (next, last) = loop {
        if h.latest(&second).state != "queued" {
            break (&second, &third);
        }
        if h.latest(&third).state != "queued" {
            break (&third, &second);
        }
        assert!(Instant::now() < deadline, "nobody took the turn");
        std::thread::sleep(Duration::from_millis(10));
    };
    h.wait_for_reason(last, &h.waiting_for(next));

    h.wait_for(next, &[AttemptState::Succeeded]);
    h.wait_for(last, &[AttemptState::Succeeded]);
}

#[test]
fn the_agent_limit_applies_across_projects() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\nsleep 1\n{OK_RESULT}",
        init(r#"["Read","Grep","Glob"]"#, "none")
    );
    let h = harness_with(&body, LOGGED_IN, "2.1.318", |c| c.max_agents = 1);
    let first = h.start(ToolProfile::ReadOnly);
    h.wait_for(&first, &[AttemptState::Running]);
    let second = h.start(ToolProfile::ReadOnly);
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.latest(&second).reason.is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(reason(&h.latest(&second)), Reason::WaitingForCapacity);
    h.wait_for(&second, &[AttemptState::Succeeded]);
}

#[test]
fn stopping_a_queued_step_means_it_never_starts() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\ntrap 'exit 0' INT\nsleep 30 & wait",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let first = h.start(ToolProfile::Build);
    let running = h.wait_for(&first, &[AttemptState::Running]);
    let second = h.start(ToolProfile::Build);
    let queued = h.latest(&second);
    h.engine.stop(&queued.id).unwrap();
    let a = h.wait_for(&second, &[AttemptState::Stopped]);
    assert!(a.pid.is_none());
    h.wait_until_kind(&running.id, "init");
    h.engine.stop(&running.id).unwrap();
    h.wait_for(&first, &[AttemptState::Stopped]);
}

#[test]
fn exiting_without_a_result_is_a_failure_with_the_exit_code() {
    keel_harness::needs!(sandbox);
    let body = format!("cat > /dev/null\n{}\nexit 7", init(BUILD_TOOLS, "none"));
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Failed]);
    assert_eq!(
        reason(&a),
        Reason::ExitedWithoutResult { exit_code: Some(7) }
    );
}

#[test]
fn showing_a_step_hides_its_secrets_and_leaves_the_record_as_it_was() {
    keel_harness::needs!(sandbox);
    // Put together here, so no secret scanner trips on this file.
    let secret = format!("{}{}", "ghp_", "x".repeat(36));
    let body = format!(
        r#"cat > /dev/null
{}
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Reading .env"}},{{"type":"tool_use","id":"t1","name":"Read","input":{{"file_path":".env"}}}}]}}}}'
echo '{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"API_KEY={secret}"}}]}}}}'
echo 'API_KEY={secret}' > settings.txt
echo '{{"type":"result","subtype":"success","is_error":false,"result":"Found {secret} in .env","num_turns":2,"duration_ms":1500,"total_cost_usd":0.25,"usage":{{"input_tokens":100,"output_tokens":20}},"modelUsage":{{"claude-fake-1":{{}}}},"permission_denials":[]}}'"#,
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Succeeded]);
    let raw = std::fs::read(&a.raw_path).unwrap();
    assert!(String::from_utf8_lossy(&raw).contains(&secret));

    // What the app shows.
    let attempts = serde_json::to_string(&Masked(h.store.attempts_of_run(&run).unwrap())).unwrap();
    let events = serde_json::to_string(&Masked(h.store.events(&a.id, 0).unwrap())).unwrap();
    let diff = mask_secrets(&h.engine.diff(&a.id).unwrap());
    let run_diff = mask_secrets(&h.engine.run_diff(&run).unwrap());
    for shown in [&attempts, &events, &diff, &run_diff] {
        assert!(!shown.contains(&secret), "{shown}");
        assert!(shown.contains(MASK), "{shown}");
    }

    // The record behind it hasn't moved, and still checks out.
    assert_eq!(h.engine.verify_raw(&a.id).unwrap(), RawCheck::Intact);
    assert_eq!(std::fs::read(&a.raw_path).unwrap(), raw);
    let stored = h.store.events(&a.id, 0).unwrap();
    let result = stored.iter().find(|e| e.kind == "toolResult").unwrap();
    assert_eq!(result.data["text"], format!("API_KEY={secret}"));
    let attempt = h.store.attempt(&a.id).unwrap();
    assert_eq!(
        attempt.result_text.unwrap(),
        format!("Found {secret} in .env")
    );
    assert!(h.engine.diff(&a.id).unwrap().contains(&secret));
    assert!(h.engine.run_diff(&run).unwrap().contains(&secret));
}

#[test]
fn a_tampered_raw_file_is_detected() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\n{OK_RESULT}",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Succeeded]);
    assert_eq!(h.engine.verify_raw(&a.id).unwrap(), RawCheck::Intact);
    let mode = Path::new(&a.raw_path)
        .metadata()
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    std::fs::write(&a.raw_path, "edited").unwrap();
    assert_eq!(h.engine.verify_raw(&a.id).unwrap(), RawCheck::Changed);
    std::fs::remove_file(&a.raw_path).unwrap();
    assert_eq!(h.engine.verify_raw(&a.id).unwrap(), RawCheck::Missing);
}

#[test]
fn no_session_means_the_run_does_not_start() {
    let h = harness_with("exit 0", r#"{"loggedIn":false}"#, "2.1.318", |_| {});
    let error = h
        .engine
        .start_run(StartRun::one_step(
            h.project_id.clone(),
            step(ToolProfile::Build),
            "en".into(),
        ))
        .unwrap_err();
    assert!(matches!(error, Error::Blocked(Reason::ClaudeLoggedOut)));
    assert!(h.store.runs(10).unwrap().is_empty());
}

#[test]
fn a_cli_outside_the_tested_range_is_refused_with_the_reason() {
    for (version, check) in [("2.1.300", "old"), ("2.2.0", "new")] {
        let h = harness_with("exit 0", LOGGED_IN, version, |_| {});
        let error = h
            .engine
            .start_run(StartRun::one_step(
                h.project_id.clone(),
                step(ToolProfile::Build),
                "en".into(),
            ))
            .unwrap_err();
        match (check, error) {
            ("old", Error::Blocked(Reason::CliVersionTooOld { .. }))
            | ("new", Error::Blocked(Reason::CliVersionUntested { .. })) => {}
            (_, other) => panic!("{version}: {other:?}"),
        }
    }
}

#[test]
fn a_folder_without_git_is_refused() {
    let h = harness("exit 0");
    let plain = h.dir.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    assert!(matches!(
        h.engine.add_project(&plain),
        Err(Error::NotARepo(_))
    ));
    // The same repo isn't added twice, even from a subfolder.
    std::fs::create_dir(h.repo.join("sub")).unwrap();
    assert_eq!(
        h.engine.add_project(&h.repo.join("sub")).unwrap().id,
        h.project_id
    );
}

fn restart(h: &Harness) -> Engine {
    let tools = Tools::from_dirs(&[
        h.dir.path().join("bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);
    let mut config = Config::new(h.dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    Engine::new(h.store.clone(), config, tools, |_| {})
}

#[test]
fn reopening_the_app_reconnects_to_a_step_still_running() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\nsleep 2\n{OK_RESULT}",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let running = h.wait_for(&run, &[AttemptState::Running]);
    h.wait_until_kind(&running.id, "init");

    // A second engine stands in for the app after a crash. The agent never noticed.
    let reopened = restart(&h);
    reopened.recover().unwrap();
    let a = h.wait_for(&run, &[AttemptState::Succeeded]);
    assert_eq!(a.cost_usd, Some(0.25));
    assert!(h.kinds(&a.id).contains(&"recovered".to_owned()));
}

#[test]
fn reopening_after_the_agent_died_marks_it_interrupted_and_keeps_the_audit() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\n{OK_RESULT}",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let done = h.wait_for(&run, &[AttemptState::Succeeded]);

    // A second attempt whose process is gone and left only part of a stream.
    let dir = Path::new(&done.raw_path).parent().unwrap();
    let raw = dir.join("builder-9.jsonl");
    std::fs::write(
        &raw,
        format!(
            "{}\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Halfway"}]}}"#
        ),
    )
    .unwrap();
    h.store
        .add_attempt(&keel_store::NewAttempt {
            id: "lost".into(),
            run_id: run.clone(),
            step_id: "builder".into(),
            kind: "agent".into(),
            role: "worker".into(),
            round: 0,
            number: 9,
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
            session_id: "gone".into(),
            resumed_from: None,
            workspace: done.workspace.clone(),
            raw_path: raw.to_string_lossy().into_owned(),
            started_at: keel_store::now(),
        })
        .unwrap();

    restart(&h).recover().unwrap();
    let lost = h.store.attempt("lost").unwrap();
    assert_eq!(lost.state, "interrupted");
    assert_eq!(reason(&lost), Reason::AgentLost);
    let kinds = h.kinds("lost");
    assert!(
        kinds.contains(&"text".to_owned()),
        "what it wrote before dying is kept: {kinds:?}"
    );
    assert!(lost.raw_sha256.is_some());
}

#[test]
fn retry_starts_fresh_in_the_same_workspace_and_resume_continues_the_session() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\nif [ -f first-done ]; then {OK_RESULT}; exit 0; fi\ntouch first-done\nexit 1",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let failed = h.wait_for(&run, &[AttemptState::Failed]);

    let second = h.engine.retry(&failed.id, false).unwrap();
    let a = h.wait_for_attempt(&second, AttemptState::Succeeded);
    assert_eq!(a.number, 2);
    assert_eq!(a.workspace, failed.workspace);
    assert_ne!(a.session_id, failed.session_id, "a retry is a new session");
    assert_eq!(a.prompt, failed.prompt);

    let third = h.engine.retry(&a.id, true).unwrap();
    let c = h.wait_for_attempt(&third, AttemptState::Succeeded);
    assert_eq!(c.resumed_from.as_deref(), Some(a.id.as_str()));
    let recorded = h.store.events(&c.id, 0).unwrap()[0].data["args"].to_string();
    assert!(
        recorded.contains("--resume") && recorded.contains(&a.session_id),
        "{recorded}"
    );
    // A resumed session reports totals for the whole session, so only what's new counts.
    assert_eq!(c.cost_usd, Some(0.0));
}

#[test]
fn abandon_closes_an_interrupted_run() {
    keel_harness::needs!(sandbox);
    let body = format!("cat > /dev/null\n{}\nexit 1", init(BUILD_TOOLS, "none"));
    let h = harness(&body);
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Failed]);
    h.engine.abandon(&a.id).unwrap();
    assert_eq!(h.latest(&run).state, "abandoned");
    assert!(h.kinds(&a.id).contains(&"abandoned".to_owned()));
}

/// What tools build in a run's copy: Rust's, Node's and Python's, and Go's modules, which Go makes
/// read-only.
const BUILDS: &str =
    "mkdir -p target/debug node_modules/left-pad .venv/bin .keel-cache/go/pkg/mod/m@v1
echo bin > target/debug/app
echo js > node_modules/left-pad/index.js
echo py > .venv/bin/python
echo mod > .keel-cache/go/pkg/mod/m@v1/go.mod
chmod -R a-w .keel-cache/go/pkg/mod";

const REBUILT: [&str; 4] = ["target", "node_modules", ".venv", ".keel-cache"];

fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn closing_a_run_frees_what_its_tools_rebuild_and_keeps_its_record() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\n{BUILDS}\nexit 1",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    std::fs::write(
        h.repo.join(".gitignore"),
        "target/\nnode_modules/\n.venv/\n",
    )
    .unwrap();
    git(&h.repo, &["add", "-A"]);
    git(&h.repo, &["commit", "-qm", "ignore builds"]);
    let run = h.start(ToolProfile::Build);
    let failed = h.wait_for(&run, &[AttemptState::Failed]);
    let workspace = PathBuf::from(&failed.workspace);
    // A step that failed can be tried again while the run is open, so nothing goes yet.
    for name in REBUILT {
        assert!(workspace.join(name).is_dir(), "{name} went too early");
    }

    h.engine.abandon(&failed.id).unwrap();
    until("the builds being freed", || {
        REBUILT.iter().all(|name| !workspace.join(name).exists())
    });
    // The copy and its commits stay, and so does everything the run has to show.
    assert!(workspace.join(".git").is_dir());
    assert!(workspace.join("CLAUDE.md").exists());
    let failed = h.store.attempt(&failed.id).unwrap();
    let raw = Path::new(&failed.raw_path);
    assert!(raw.exists());
    assert!(raw.with_file_name("builder-1.stderr.txt").exists());
    assert!(Path::new(failed.diff_path.as_deref().unwrap()).exists());
}

#[test]
fn a_run_that_ends_frees_them_too_but_not_what_the_project_keeps_in_git() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\nmkdir -p target/debug\necho bin > target/debug/app\n{OK_RESULT}",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    // Like a GitHub Action, which keeps its packages in the repo.
    std::fs::write(h.repo.join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir(h.repo.join("node_modules")).unwrap();
    std::fs::write(h.repo.join("node_modules/action.js"), "kept\n").unwrap();
    git(&h.repo, &["add", "-A"]);
    git(&h.repo, &["commit", "-qm", "vendored"]);
    let run = h.start(ToolProfile::Build);
    let done = h.wait_for(&run, &[AttemptState::Succeeded]);
    let workspace = PathBuf::from(&done.workspace);
    until("the build being freed", || {
        !workspace.join("target").exists() && !workspace.join(".keel-cache").exists()
    });
    assert_eq!(
        std::fs::read_to_string(workspace.join("node_modules/action.js")).unwrap(),
        "kept\n"
    );
}

/// Where what's freed waits to be deleted, next to the run's copy.
fn trash_of(workspace: &Path) -> PathBuf {
    workspace.with_file_name("trash")
}

#[test]
fn a_folder_the_project_keeps_stays_whatever_its_capitals() {
    keel_harness::needs!(sandbox);
    // The Mac's disk doesn't tell `Target` from `target`. Git does, and still keeps `Target`.
    let body = format!(
        "cat > /dev/null\n{}\nmv Target t && mv t target\n{OK_RESULT}",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    for own in ["Target", "Node_Modules"] {
        std::fs::create_dir(h.repo.join(own)).unwrap();
        std::fs::write(h.repo.join(own).join("kept.txt"), "kept\n").unwrap();
    }
    git(&h.repo, &["add", "-A"]);
    git(&h.repo, &["commit", "-qm", "own folders"]);
    let run = h.start(ToolProfile::Build);
    let done = h.wait_for(&run, &[AttemptState::Succeeded]);
    let workspace = PathBuf::from(&done.workspace);
    until("the run's cache being freed", || {
        !workspace.join(".keel-cache").exists() && !trash_of(&workspace).exists()
    });
    for kept in ["target/kept.txt", "Node_Modules/kept.txt"] {
        assert_eq!(
            std::fs::read_to_string(workspace.join(kept)).unwrap(),
            "kept\n",
            "{kept}"
        );
    }
}

#[test]
fn reopening_the_app_frees_what_ended_runs_still_hold() {
    keel_harness::needs!(sandbox);
    let body = format!(
        "cat > /dev/null\n{}\n{BUILDS}\nexit 1",
        init(BUILD_TOOLS, "none")
    );
    let h = harness(&body);
    std::fs::write(
        h.repo.join(".gitignore"),
        "target/\nnode_modules/\n.venv/\n",
    )
    .unwrap();
    git(&h.repo, &["add", "-A"]);
    git(&h.repo, &["commit", "-qm", "ignore builds"]);
    let run = h.start(ToolProfile::Build);
    let failed = h.wait_for(&run, &[AttemptState::Failed]);
    let workspace = PathBuf::from(&failed.workspace);

    // Closed by an app that hasn't found git yet, which can't tell what the project keeps.
    let mut config = Config::new(h.dir.path().join("data"));
    config.keep_awake = false;
    let early = Engine::new(
        h.store.clone(),
        config,
        Tools::from_dirs(&[h.dir.path().join("bin")]),
        |_| {},
    );
    early.abandon(&failed.id).unwrap();
    // And an earlier freeing the app quit in the middle of.
    let half = trash_of(&workspace).join("half-deleted");
    std::fs::create_dir_all(half.join("debug")).unwrap();
    std::fs::write(half.join("debug/app"), "bin").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    for name in REBUILT {
        assert!(workspace.join(name).is_dir(), "{name} went without git");
    }

    restart(&h).recover().unwrap();
    until("the ended run being freed", || {
        REBUILT.iter().all(|name| !workspace.join(name).exists()) && !trash_of(&workspace).exists()
    });
    assert!(workspace.join("CLAUDE.md").exists());
}

#[test]
fn a_new_run_works_in_runs_dir_not_in_the_data_folder() {
    keel_harness::needs!(sandbox);
    let h = harness_with("echo hi > new.txt\n", LOGGED_IN, "2.1.318", |c| {
        c.runs_dir = std::env::temp_dir().join(format!("keel-runs-{}", std::process::id()));
    });
    let run = h.start(ToolProfile::Build);
    let a = h.wait_for(&run, &[AttemptState::Succeeded, AttemptState::Failed]);
    let workspace = Path::new(&a.workspace);
    assert!(
        workspace
            .starts_with(std::env::temp_dir().join(format!("keel-runs-{}", std::process::id()))),
        "{workspace:?}"
    );
    assert!(!workspace.to_string_lossy().contains(' '));
    assert!(!h.dir.path().join("data/runs").join(&run).exists());
    let _ = std::fs::remove_dir_all(workspace.parent().unwrap().parent().unwrap());
}
