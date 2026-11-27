//! A builder or a check that can't go on for lack of network to a domain the project doesn't allow
//! waits for the person, against a fake `claude`. Its builder declares what `declare` says, one line
//! per call, and ends its work with the command output in `bash`, if there is one. The check itself
//! is a real command in the sandbox.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use keel_core::{
    AttemptState, NeededDomain, Reason, StepRole, StepSpec, ToolProfile, WorkflowStep,
};
use keel_engine::{Config, Connector, Engine, Error, NetworkAnswer, StartRun};
use keel_harness::Tools;
use keel_store::{Attempt, NewAttempt, NewRun, ProjectSettings, Store};

/// Fails as npm does when something isn't in its cache and it may not go online.
const OFFLINE_CHECK: &str = "echo 'npm error code ENOTCACHED' >&2; \
     echo 'npm error request to https://registry.npmjs.org/next failed: cache mode is only-if-cached' >&2; exit 1";

const NPM: &str = "registry.npmjs.org";

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
    /// The domains a check's proxy connected to, each with its port. They all lead to a server of
    /// this test's, so none of it needs the internet.
    reached: Arc<Mutex<Vec<(String, u16)>>>,
}

/// A server on this Mac that answers every request with `hello`, in place of whatever domain a
/// check's proxy lets through, and how the proxy gets to it. None where the sandbox this runs in
/// opens no sockets: only tests that say they need them go through a proxy.
fn registry(reached: &Arc<Mutex<Vec<(String, u16)>>>) -> Option<Connector> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut asked = Vec::new();
                let mut buffer = [0u8; 1024];
                while !asked.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(count) if count > 0 => asked.extend_from_slice(&buffer[..count]),
                        _ => return,
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                );
            });
        }
    });
    let reached = reached.clone();
    Some(Connector::new(move |host, port_asked| {
        reached.lock().unwrap().push((host.to_owned(), port_asked));
        TcpStream::connect(("127.0.0.1", port))
    }))
}

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

fn fake(dir: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true}}'; exit 0;;
esac
prompt="$(cat)"
tools=""; prev=""
for a in "$@"; do [ "$prev" = "--tools" ] && tools="$a"; prev="$a"; done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
case "$*" in *--json-schema*) list="$list\",\"StructuredOutput";; esac
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
role=$(printf '%s' "$prompt" | head -1)
# What each step that builds was given, one that goes on with a session included.
case "$tools" in *Bash*)
  while [ -f "{dir}/hold" ]; do sleep 0.05; done
  echo "[$npm_config_offline]" >> "{dir}/offline"
  echo "[$CARGO_NET_OFFLINE]" >> "{dir}/cargo";;
esac
case "$role" in
BUILD*)
  echo "work $(date +%s%N)" >> work.txt; git add -A && git commit -qm "build"
  if [ -f "{dir}/bash" ]; then
    echo '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"npm install"}}}}]}}}}'
    echo "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"is_error\":true,\"content\":\"$(cat "{dir}/bash")\"}}]}}}}"
  fi
  needed=""
  if [ -s "{dir}/declare" ]; then needed=$(head -1 "{dir}/declare"); sed -i '' 1d "{dir}/declare"; fi
  # What it wrote before it answered, and the message it put in its answer.
  said=""; message="Built it."
  if [ -f "{dir}/said" ]; then said=$(cat "{dir}/said"); fi
  if [ -f "{dir}/message" ]; then message=$(cat "{dir}/message"); fi
  result="{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"$said\",\"num_turns\":1,\"structured_output\":{{\"message\":\"$message\",\"neededDomains\":[$needed]}}}}";;
APPROVE*)
  result='{{"type":"result","subtype":"success","is_error":false,"result":"","num_turns":1,"structured_output":{{"decision":"approve","risk":"low","reasons":"because","instructions":""}}}}';;
*)
  result='{{"type":"result","subtype":"success","is_error":false,"result":"OK","num_turns":1}}';;
esac
echo "$result"
"##,
        dir = dir.display(),
    )
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    keel_harness::testing::write_script(&bin.join("claude"), &fake(dir.path())).unwrap();
    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "x\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    let reached = Arc::default();
    config.check_connects = registry(&reached);
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let project_id = engine.add_project(&repo).unwrap().id;
    World {
        dir,
        store,
        engine,
        project_id,
        reached,
    }
}

fn agent(id: &str, tools: ToolProfile, role: StepRole, prompt: &str) -> WorkflowStep {
    WorkflowStep::Agent(StepSpec {
        id: id.into(),
        model: "fake".into(),
        effort: None,
        tools,
        prompt: prompt.into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        risks: vec![],
        model_when_risky: None,
        skill: None,
        role,
    })
}

/// A builder, a check, an approver that can send the work back, and a person.
fn steps(check: &str) -> Vec<WorkflowStep> {
    vec![
        agent("builder", ToolProfile::Build, StepRole::Worker, "BUILD"),
        WorkflowStep::Check {
            id: "check".into(),
            run: check.into(),
            only_if_changed: false,
        },
        agent(
            "approver",
            ToolProfile::ReadOnly,
            StepRole::Approver {
                rework_to: "builder".into(),
                max_rounds: 2,
            },
            "APPROVE",
        ),
        WorkflowStep::Approval { id: "human".into() },
    ]
}

fn declared(domain: &str) -> String {
    format!(r#"{{"domain":"{domain}","why":"to install next"}}"#)
}

impl World {
    /// What the builder declares, one line per call.
    fn declare(&self, calls: &[String]) {
        let text: String = calls.iter().map(|call| format!("{call}\n")).collect();
        std::fs::write(self.dir.path().join("declare"), text).unwrap();
    }

    fn allow(&self, domains: &[&str]) {
        self.engine
            .update_project(
                &self.project_id,
                &ProjectSettings {
                    allowed_domains: domains.iter().map(|d| (*d).to_owned()).collect(),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    fn domains(&self) -> Vec<String> {
        self.store
            .project(&self.project_id)
            .unwrap()
            .allowed_domains
    }

    fn start(&self, check: &str) -> String {
        self.engine
            .start_run(StartRun {
                project_id: self.project_id.clone(),
                title: None,
                request: "x".into(),
                workflow: steps(check),
                language: "en".into(),
                source_issue: None,
                saved_workflow: None,
                started_by: None,
            })
            .unwrap()
    }

    fn attempts(&self, run: &str, step: &str) -> Vec<Attempt> {
        self.store
            .attempts_of_run(run)
            .unwrap()
            .into_iter()
            .filter(|a| a.step_id == step)
            .collect()
    }

    /// The step's try number `number`, once it stands like this.
    fn wait(&self, run: &str, step: &str, number: i64, state: AttemptState) -> Attempt {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let found = self
                .attempts(run, step)
                .into_iter()
                .find(|a| a.number == number);
            if let Some(attempt) = &found
                && attempt.state == state.as_str()
            {
                return attempt.clone();
            }
            assert!(
                Instant::now() < deadline,
                "{step} {number} never reached {state:?}: {:?}",
                self.store
                    .attempts_of_run(run)
                    .unwrap()
                    .iter()
                    .map(|a| (&a.step_id, a.number, &a.state, &a.reason))
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn offline(&self) -> Vec<String> {
        self.noted("offline")
    }

    /// What the fake builder noted in this file, one line per call.
    fn noted(&self, file: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join(file))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn printed(&self, attempt: &Attempt) -> String {
        self.store
            .events(&attempt.id, 0)
            .unwrap()
            .iter()
            .find(|event| event.kind == "checkResult")
            .and_then(|event| event.data["stdout"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    fn profile(&self, attempt: &Attempt) -> String {
        self.store
            .events(&attempt.id, 0)
            .unwrap()
            .iter()
            .find(|event| event.kind == "started")
            .and_then(|event| event.data["profile"].as_str().map(str::to_owned))
            .expect("a started command")
    }

    fn round(&self, run: &str) -> i64 {
        self.store.run(run).unwrap().round
    }
}

fn needed(attempt: &Attempt) -> Vec<NeededDomain> {
    match serde_json::from_value(attempt.reason.clone().expect("has a reason")).unwrap() {
        Reason::NetworkNeeded { needed } => needed,
        other => panic!("not waiting for a domain: {other:?}"),
    }
}

fn domains_of(attempt: &Attempt) -> Vec<String> {
    needed(attempt).into_iter().map(|n| n.domain).collect()
}

#[test]
fn a_builder_that_declares_a_missing_domain_waits_for_the_person_and_spends_no_round() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared(NPM)]);
    let run = w.start("true");

    let built = w.wait(&run, "builder", 1, AttemptState::Blocked);

    assert_eq!(
        needed(&built),
        [NeededDomain {
            domain: NPM.into(),
            why: Some("to install next".into()),
            tool: None,
            frees_offline: false,
        }]
    );
    // What it said when it finished is what it said, and not the structure around it.
    assert_eq!(built.result_text.as_deref(), Some("Built it."));
    assert_eq!(built.output, None);
    std::thread::sleep(Duration::from_millis(300));
    assert!(w.attempts(&run, "check").is_empty());
    assert!(w.attempts(&run, "approver").is_empty());
    assert_eq!(w.round(&run), 0);
    assert!(w.store.run(&run).unwrap().finished_at.is_none());
}

#[test]
fn allowing_the_domain_adds_it_and_repeats_the_step_online_in_the_same_round() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.allow(&["example.com"]);
    w.declare(&[declared(NPM)]);
    let run = w.start("true");
    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);

    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Allow)
        .unwrap();

    w.wait(&run, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.domains(), ["example.com", NPM]);
    let again = w.wait(&run, "builder", 2, AttemptState::Succeeded);
    assert_eq!(again.round, 0);
    assert_eq!(again.resumed_from, None, "from the beginning");
    assert_eq!(
        again.settings["sandbox"]["network"]["allowedDomains"],
        serde_json::json!(["example.com", NPM])
    );
    assert_eq!(w.offline(), ["[true]", "[]"]);
    assert_eq!(w.round(&run), 0);
    // Answered once.
    assert!(matches!(
        w.engine.answer_network(&waiting.id, NetworkAnswer::Allow),
        Err(Error::InvalidState)
    ));
    // The person's own, which isn't a client's change.
    assert!(w.store.mcp_project_changes(10).unwrap().is_empty());
}

#[test]
fn going_on_without_it_moves_to_the_next_step_and_changes_no_domain() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared(NPM)]);
    let run = w.start("true");
    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);

    w.engine
        .answer_network(&waiting.id, NetworkAnswer::GoWithout)
        .unwrap();

    w.wait(&run, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.attempts(&run, "builder").len(), 1);
    w.wait(&run, "builder", 1, AttemptState::Succeeded);
    assert!(w.domains().is_empty());
    assert_eq!(w.round(&run), 0);
}

#[test]
fn stopping_leaves_the_step_stopped_and_the_run_for_the_person() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared(NPM)]);
    let run = w.start("true");
    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);

    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Stop)
        .unwrap();

    w.wait(&run, "builder", 1, AttemptState::Stopped);
    assert!(w.attempts(&run, "check").is_empty());
    assert!(w.domains().is_empty());
    assert!(w.store.run(&run).unwrap().finished_at.is_none());
}

#[test]
fn only_a_domain_the_project_lacks_and_that_is_one_host_pauses_a_run() {
    keel_harness::needs!(sandbox);
    for (allowed, declare) in [
        (vec![NPM], declared(NPM)),
        (vec!["*.npmjs.org"], declared(NPM)),
        (vec![], declared("https://registry.npmjs.org/next")),
        (vec![], declared("*.npmjs.org")),
        (vec![], String::new()),
    ] {
        let w = world();
        w.allow(&allowed);
        w.declare(std::slice::from_ref(&declare));
        let run = w.start("true");

        w.wait(&run, "human", 1, AttemptState::WaitingApproval);
        w.wait(&run, "builder", 1, AttemptState::Succeeded);
    }
}

#[test]
fn a_known_registry_by_its_other_name_asks_for_the_host_it_downloads_from() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared("npmjs.org")]);
    let run = w.start("true");

    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);

    assert_eq!(domains_of(&waiting), [NPM]);
}

#[test]
fn a_builder_whose_last_command_failed_offline_waits_even_if_it_declares_nothing() {
    keel_harness::needs!(sandbox);
    let w = world();
    std::fs::write(w.dir.path().join("bash"), "npm error code ENOTCACHED").unwrap();
    let run = w.start("true");

    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);

    assert_eq!(
        needed(&waiting),
        [NeededDomain {
            domain: NPM.into(),
            why: None,
            tool: Some("npm".into()),
            frees_offline: false,
        }]
    );
}

#[test]
fn a_check_that_fails_offline_waits_for_the_person_instead_of_going_back() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.start(OFFLINE_CHECK);

    let checked = w.wait(&run, "check", 1, AttemptState::Blocked);

    assert_eq!(domains_of(&checked), [NPM]);
    assert_eq!(needed(&checked)[0].tool.as_deref(), Some("npm"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(w.attempts(&run, "builder").len(), 1);
    assert!(w.attempts(&run, "approver").is_empty());
    assert_eq!(w.round(&run), 0);
}

/// Fails as npm does for as long as it's kept offline. Once it isn't, says what it has and what it
/// can reach: a domain the project allows, one it doesn't, and an address without the proxy. It
/// also shows the proxy's password every way it has it: as it comes, in what Java is given, and
/// in the header curl says it sent.
fn check_that_needs_npm() -> String {
    format!(
        "if [ -n \"$npm_config_offline\" ]; then {OFFLINE_CHECK}; fi; \
         code() {{ /usr/bin/curl -s -o /dev/null -m 8 -w '%{{http_code}}' \"$@\"; true; }}; \
         echo \"offline=[$npm_config_offline$pnpm_config_offline] cargo=[$CARGO_NET_OFFLINE]\"; \
         echo \"proxy=[$HTTPS_PROXY]\"; \
         echo \"java=[$JDK_JAVA_OPTIONS]\"; \
         echo \"sent=[$(/usr/bin/curl -sv -o /dev/null -m 8 http://{NPM}/ 2>&1 | grep -i '^> proxy-authorization' | tr -d '\\r')]\"; \
         echo \"allowed=$(code http://{NPM}/)\"; \
         echo \"other=$(code http://other.example/)\"; \
         echo \"direct=$(code --noproxy '*' http://192.0.2.1/)\""
    )
}

#[test]
fn allowing_from_a_check_repeats_that_check_in_the_same_round() {
    keel_harness::needs!(sandbox, sockets);
    let w = world();
    let run = w.start(&check_that_needs_npm());
    let waiting = w.wait(&run, "check", 1, AttemptState::Blocked);
    assert!(!w.profile(&waiting).contains("localhost:"));

    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Allow)
        .unwrap();

    let again = w.wait(&run, "check", 2, AttemptState::Succeeded);
    assert_eq!(again.round, 0);
    assert_eq!(w.domains(), [NPM]);
    w.wait(&run, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.attempts(&run, "builder").len(), 1);
    // What it had: its record says so, and its sandbox opens one port, the proxy's.
    assert_eq!(again.settings["network"], "allowed domains");
    assert_eq!(again.settings["allowedDomains"], serde_json::json!([NPM]));
    let profile = w.profile(&again);
    assert_eq!(
        profile
            .matches("(allow network-outbound (remote ip \"localhost:")
            .count(),
        1,
        "{profile}"
    );
    let printed = w.printed(&again);
    let line = |name: &str| {
        printed
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name} in {printed}"))
    };
    // npm's variables are gone, and those of a registry the project doesn't allow stay.
    assert_eq!(line("offline="), "[] cargo=[true]");
    // The proxy lets through what's allowed, which answers. Anything else it refuses, and
    // without it nothing gets out.
    assert_eq!(line("allowed="), "200", "{printed}");
    assert_eq!(line("other="), "403", "{printed}");
    assert_eq!(line("direct="), "000", "{printed}");
    let reached = w.reached.lock().unwrap().clone();
    assert!(!reached.is_empty(), "{printed}");
    for to in &reached {
        assert_eq!(*to, (NPM.to_owned(), 80), "{reached:?}");
    }
    // What Keel keeps of what the check printed has the proxy, and not its password, however
    // the check came to print it. Java is given nothing of it.
    let port = profile
        .split("localhost:")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the proxy's port");
    assert_eq!(
        line("proxy="),
        format!("[http://keel:***@127.0.0.1:{port}]"),
        "{printed}"
    );
    assert_eq!(
        line("sent="),
        "[> Proxy-Authorization: Basic ***]",
        "{printed}"
    );
    assert_eq!(
        line("java="),
        "[-Djava.net.preferIPv4Stack=true]",
        "{printed}"
    );
}

/// A builder that says what it did as it works, and leaves its answer's message empty.
#[test]
fn an_empty_message_in_the_builders_answer_does_not_replace_what_it_said() {
    keel_harness::needs!(sandbox);
    let w = world();
    for (run, message) in [("first", ""), ("second", "  ")] {
        std::fs::write(
            w.dir.path().join("said"),
            "I added the parser and its tests.",
        )
        .unwrap();
        std::fs::write(w.dir.path().join("message"), message).unwrap();
        let run_id = w.start("true");
        let built = w.wait(&run_id, "builder", 1, AttemptState::Succeeded);
        assert_eq!(
            built.result_text.as_deref(),
            Some("I added the parser and its tests."),
            "{run}"
        );
        w.wait(&run_id, "human", 1, AttemptState::WaitingApproval);
    }
    // A message that says something is what it said when it finished.
    std::fs::write(w.dir.path().join("message"), "Built it.").unwrap();
    let run_id = w.start("true");
    let built = w.wait(&run_id, "builder", 1, AttemptState::Succeeded);
    assert_eq!(built.result_text.as_deref(), Some("Built it."));
}

#[test]
fn only_the_check_a_person_repeats_gets_the_network() {
    keel_harness::needs!(sandbox, sockets);
    let w = world();
    // Fails whatever it has, so the work goes back and a check of a new round runs.
    let run = w.start(&format!(
        "echo \"proxy=[$HTTPS_PROXY] offline=[$npm_config_offline]\"; {OFFLINE_CHECK}"
    ));
    let waiting = w.wait(&run, "check", 1, AttemptState::Blocked);

    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Allow)
        .unwrap();

    let again = w.wait(&run, "check", 2, AttemptState::Returned);
    assert!(w.printed(&again).contains("offline=[]"));
    let later = w.wait(&run, "check", 3, AttemptState::Returned);
    assert_eq!(w.printed(&later).trim(), "proxy=[] offline=[true]");
    assert_eq!(later.settings["network"], "none");
    assert!(!w.profile(&later).contains("localhost:"));
}

#[test]
fn a_step_repeated_after_allowing_a_private_registry_is_no_longer_offline() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared("npm.mycorp.example")]);
    let run = w.start("true");
    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);

    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Allow)
        .unwrap();

    w.wait(&run, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.domains(), ["npm.mycorp.example"]);
    // Keel can't tell whose registry it is, nor which tool failed, so none is kept offline.
    assert_eq!(w.offline(), ["[true]", "[]"]);
    assert_eq!(w.noted("cargo"), ["[true]", "[]"]);
}

#[test]
fn going_on_with_the_session_of_a_repeated_step_keeps_it_online() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared("npm.mycorp.example")]);
    let run = w.start("true");
    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);
    std::fs::write(w.dir.path().join("hold"), "").unwrap();
    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Allow)
        .unwrap();
    let again = w.wait(&run, "builder", 2, AttemptState::Running);
    w.engine.stop(&again.id).unwrap();
    w.wait(&run, "builder", 2, AttemptState::Stopped);
    std::fs::remove_file(w.dir.path().join("hold")).unwrap();

    w.engine.retry(&again.id, true).unwrap();

    w.wait(&run, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.offline(), ["[true]", "[]"]);
}

#[test]
fn two_waits_in_a_row_keep_what_each_one_allowed() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared("npm.mycorp.example"), declared("pypi.org")]);
    let run = w.start("true");
    let first = w.wait(&run, "builder", 1, AttemptState::Blocked);
    w.engine
        .answer_network(&first.id, NetworkAnswer::Allow)
        .unwrap();
    let second = w.wait(&run, "builder", 2, AttemptState::Blocked);
    w.engine
        .answer_network(&second.id, NetworkAnswer::Allow)
        .unwrap();

    w.wait(&run, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.offline(), ["[true]", "[]", "[]"]);
    assert_eq!(w.noted("cargo"), ["[true]", "[]", "[]"]);
}

#[test]
fn trying_a_repeated_step_again_from_the_beginning_keeps_it_online() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared("npm.mycorp.example")]);
    let run = w.start("true");
    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);
    std::fs::write(w.dir.path().join("hold"), "").unwrap();
    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Allow)
        .unwrap();
    let again = w.wait(&run, "builder", 2, AttemptState::Running);
    w.engine.stop(&again.id).unwrap();
    w.wait(&run, "builder", 2, AttemptState::Stopped);
    std::fs::remove_file(w.dir.path().join("hold")).unwrap();

    w.engine.retry(&again.id, false).unwrap();

    w.wait(&run, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.offline(), ["[true]", "[]"]);
}

#[test]
fn a_later_round_and_a_later_run_stay_online_for_a_private_registry() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared("npm.mycorp.example")]);
    // The builder adds a line each time, so this fails until it has been back once.
    let check = "test $(wc -l < work.txt) -ge 3";
    let run = w.start(check);
    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);
    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Allow)
        .unwrap();
    w.wait(&run, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.round(&run), 1);
    assert_eq!(w.offline(), ["[true]", "[]", "[]"]);

    let next = w.start("true");

    w.wait(&next, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.offline(), ["[true]", "[]", "[]", "[]"]);
}

#[test]
fn a_repeated_check_that_was_stopped_reaches_the_network_when_tried_again() {
    keel_harness::needs!(sandbox, sockets);
    let w = world();
    // Once it isn't offline, it hangs the first time.
    let check = format!(
        "if [ -n \"$npm_config_offline\" ]; then {OFFLINE_CHECK}; fi; \
         if [ ! -f .tried ]; then touch .tried; sleep 60; fi; \
         echo \"proxy=[$HTTPS_PROXY] offline=[$npm_config_offline]\""
    );
    let run = w.start(&check);
    let waiting = w.wait(&run, "check", 1, AttemptState::Blocked);
    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Allow)
        .unwrap();
    let again = w.wait(&run, "check", 2, AttemptState::Running);
    let tried = Path::new(&again.workspace).join(".tried");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !tried.exists() {
        assert!(Instant::now() < deadline, "the check never hung");
        std::thread::sleep(Duration::from_millis(20));
    }
    w.engine.stop(&again.id).unwrap();
    w.wait(&run, "check", 2, AttemptState::Stopped);

    w.engine.retry(&again.id, false).unwrap();

    let last = w.wait(&run, "check", 3, AttemptState::Succeeded);
    assert_eq!(last.settings["network"], "allowed domains");
    let printed = w.printed(&last);
    assert!(printed.contains("@127.0.0.1:"), "{printed}");
    assert!(printed.contains("offline=[]"), "{printed}");
}

#[test]
fn a_check_after_a_step_repeated_for_a_private_registry_is_as_offline_as_any() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared("npm.mycorp.example")]);
    let run = w.start("echo \"offline=[$npm_config_offline$CARGO_NET_OFFLINE]\"");
    let built = w.wait(&run, "builder", 1, AttemptState::Blocked);
    w.engine
        .answer_network(&built.id, NetworkAnswer::Allow)
        .unwrap();

    // The check wasn't repeated for it, so it's as offline as any.
    let checked = w.wait(&run, "check", 1, AttemptState::Succeeded);
    assert_eq!(w.printed(&checked).trim(), "offline=[truetrue]");
}

#[test]
fn a_domain_allowed_while_the_builder_runs_still_pauses_it() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.declare(&[declared(NPM)]);
    std::fs::write(w.dir.path().join("hold"), "").unwrap();
    let run = w.start("true");
    w.wait(&run, "builder", 1, AttemptState::Running);

    // It started without the domain, so it didn't have it, whatever the project allows by now.
    w.allow(&[NPM]);
    std::fs::remove_file(w.dir.path().join("hold")).unwrap();

    let waiting = w.wait(&run, "builder", 1, AttemptState::Blocked);
    assert_eq!(domains_of(&waiting), [NPM]);
    w.engine
        .answer_network(&waiting.id, NetworkAnswer::Allow)
        .unwrap();
    w.wait(&run, "human", 1, AttemptState::WaitingApproval);
    assert_eq!(w.offline(), ["[true]", "[]"]);
    assert_eq!(w.domains(), [NPM]);
}

#[test]
fn a_check_still_offline_once_the_domain_is_allowed_goes_back_to_the_builder() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.allow(&[NPM]);
    let run = w.start(OFFLINE_CHECK);

    w.wait(&run, "check", 1, AttemptState::Returned);
}

#[test]
fn going_on_without_it_from_a_check_sends_the_work_back_and_does_not_ask_again() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.start(OFFLINE_CHECK);
    let waiting = w.wait(&run, "check", 1, AttemptState::Blocked);

    w.engine
        .answer_network(&waiting.id, NetworkAnswer::GoWithout)
        .unwrap();

    w.wait(&run, "check", 1, AttemptState::Returned);
    w.wait(&run, "builder", 2, AttemptState::Succeeded);
    assert!(w.domains().is_empty());
    // Failing the same way again is a failed check like any other, up to the run's rounds.
    let last = w.wait(&run, "check", 3, AttemptState::Failed);
    assert_eq!(last.reason.unwrap()["code"], "checkFailed");
    assert_eq!(w.round(&run), 2);
}

/// A form saved after a person allowed a domain it never had.
#[test]
fn saving_a_form_keeps_a_domain_allowed_from_a_wait_while_it_was_open() {
    let w = world();
    w.allow(&["example.com"]);
    w.recorded("r", &check_step(), &[waits_for(NPM)]);
    w.engine
        .answer_network("r-1", NetworkAnswer::Allow)
        .unwrap();

    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                allowed_domains: vec!["pypi.org".into()],
                domains_seen: Some(vec!["example.com".into()]),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(w.domains(), ["pypi.org", NPM]);
}

/// How a try of a check ended, for a record written by hand.
struct Ended {
    state: AttemptState,
    reason: Option<Reason>,
    event: Option<&'static str>,
    /// The command it ran, if not the one the step has now.
    ran: Option<&'static str>,
}

fn ended(state: AttemptState) -> Ended {
    Ended {
        state,
        reason: None,
        event: None,
        ran: None,
    }
}

/// The same try, of when the step had another command.
fn running(command: &'static str, ended: Ended) -> Ended {
    Ended {
        ran: Some(command),
        ..ended
    }
}

fn check_failed(code: i32, event: Option<&'static str>) -> Ended {
    Ended {
        state: AttemptState::Failed,
        reason: Some(Reason::CheckFailed {
            exit_code: Some(code),
            localhost_refused: false,
            failed_times: None,
            unchanged: false,
        }),
        event,
        ran: None,
    }
}

impl World {
    /// A run of one step, and its tries as they ended, written by hand: what Keel decides as it
    /// starts the next one needs no sandbox to be seen.
    fn recorded(&self, run: &str, step: &WorkflowStep, tries: &[Ended]) {
        self.recorded_from(run, step, tries, None);
    }

    /// The same, of a run started from this saved workflow.
    fn recorded_from(
        &self,
        run: &str,
        step: &WorkflowStep,
        tries: &[Ended],
        workflow: Option<&str>,
    ) {
        let command = match step {
            WorkflowStep::Check { run, .. } => run.as_str(),
            _ => "p",
        };
        self.store
            .add_run(&NewRun {
                id: run.into(),
                project_id: self.project_id.clone(),
                title: "x".into(),
                request: "x".into(),
                workflow: serde_json::to_string(std::slice::from_ref(step)).unwrap(),
                language: "en".into(),
                source_issue: None,
                request_written: None,
                workflow_id: workflow.map(str::to_owned),
                workflow_name: None,
                started_by: None,
            })
            .unwrap();
        for (index, ended) in tries.iter().enumerate() {
            let number = index as i64 + 1;
            let id = format!("{run}-{number}");
            let agent = step.kind() == "agent";
            self.store
                .add_attempt(&NewAttempt {
                    id: id.clone(),
                    run_id: run.into(),
                    step_id: step.id().into(),
                    kind: step.kind().into(),
                    role: "worker".into(),
                    round: 0,
                    number,
                    state: ended.state,
                    model_requested: if agent { "fake".into() } else { String::new() },
                    effort: None,
                    tools: if agent { "build" } else { step.kind() }.into(),
                    max_turns: None,
                    max_budget_usd: None,
                    language: "en".into(),
                    prompt: ended.ran.unwrap_or(command).into(),
                    system_prompt_sources: "[]".into(),
                    settings: "{}".into(),
                    session_id: id.clone(),
                    resumed_from: None,
                    workspace: self.dir.path().join("nowhere").display().to_string(),
                    raw_path: self.dir.path().join("nowhere/raw").display().to_string(),
                    started_at: keel_store::now(),
                })
                .unwrap();
            self.store
                .set_state(&id, ended.state, ended.reason.as_ref())
                .unwrap();
            if let Some(kind) = ended.event {
                // As Keel writes it, with the domains it's about.
                let data = match (&ended.reason, kind) {
                    (Some(Reason::NetworkNeeded { needed }), "networkAllowed") => {
                        let domains: Vec<&str> = needed.iter().map(|n| n.domain.as_str()).collect();
                        serde_json::json!({ "domains": domains })
                    }
                    _ => serde_json::json!({}),
                };
                self.store.append_event(&id, kind, &data).unwrap();
            }
        }
    }
}

fn waits_for(domain: &str) -> Ended {
    Ended {
        state: AttemptState::Blocked,
        reason: Some(Reason::NetworkNeeded {
            needed: vec![NeededDomain {
                domain: domain.into(),
                why: None,
                tool: None,
                frees_offline: false,
            }],
        }),
        event: None,
        ran: None,
    }
}

fn allowed_wait(domain: &str) -> Ended {
    Ended {
        event: Some("networkAllowed"),
        ..waits_for(domain)
    }
}

fn check_step() -> WorkflowStep {
    WorkflowStep::Check {
        id: "check".into(),
        run: "true".into(),
        only_if_changed: false,
    }
}

#[test]
fn trying_again_a_repeated_check_that_did_not_finish_keeps_the_network() {
    let w = world();
    w.allow(&[NPM]);
    let unfinished: Vec<(&str, Vec<Ended>)> = vec![
        ("interrupted", vec![ended(AttemptState::Interrupted)]),
        ("stopped", vec![ended(AttemptState::Stopped)]),
        ("lost", vec![check_failed(1, Some("networkLost"))]),
        ("killed", vec![check_failed(1, Some("commandKilled"))]),
        ("signal", vec![check_failed(137, None)]),
        ("services", vec![ended(AttemptState::Blocked)]),
        // However many tries it takes, and one put off for a new setup among them.
        (
            "several",
            vec![
                ended(AttemptState::Interrupted),
                ended(AttemptState::Postponed),
                ended(AttemptState::Stopped),
            ],
        ),
        // Waiting again, for another domain, and stopped there.
        ("again", vec![waits_for("pypi.org")]),
    ];
    for (name, after) in unfinished {
        let mut tries = vec![allowed_wait(NPM)];
        tries.extend(after);
        w.recorded(name, &check_step(), &tries);
        let last = format!("{name}-{}", tries.len());

        let id = w.engine.retry(&last, false).unwrap();

        let next = w.store.attempt(&id).unwrap();
        assert_eq!(next.settings["network"], "allowed domains", "{name}");
        assert_eq!(
            next.settings["allowedDomains"],
            serde_json::json!([NPM]),
            "{name}"
        );
    }
}

#[test]
fn trying_again_a_check_with_no_network_to_inherit_gets_none() {
    let w = world();
    w.allow(&[NPM]);
    for (name, tries) in [
        // It ran to its end with the network, so it did its work.
        ("failed", vec![allowed_wait(NPM), check_failed(1, None)]),
        (
            "after-one-that-finished",
            vec![
                allowed_wait(NPM),
                check_failed(1, None),
                ended(AttemptState::Stopped),
            ],
        ),
        // Nobody allowed anything.
        ("never-allowed", vec![waits_for(NPM)]),
        ("plain", vec![ended(AttemptState::Interrupted)]),
    ] {
        w.recorded(name, &check_step(), &tries);
        let last = format!("{name}-{}", tries.len());

        let id = w.engine.retry(&last, false).unwrap();

        let next = w.store.attempt(&id).unwrap();
        assert_eq!(next.settings["network"], "none", "{name}");
        assert!(next.settings.get("allowedDomains").is_none(), "{name}");
    }
}

/// What a try of the check gets when its last try is tried again.
fn retried(w: &World, run: &str, tries: usize) -> Attempt {
    let id = w.engine.retry(&format!("{run}-{tries}"), false).unwrap();
    w.store.attempt(&id).unwrap()
}

/// What a person allowed was for the command that asked for it.
#[test]
fn trying_a_repeated_check_again_with_another_command_gets_no_network() {
    let w = world();
    w.allow(&[NPM]);
    // The step runs `true` now, and it was `make test` that waited and was repeated.
    let before = |ended: Ended| running("make test", ended);
    w.recorded(
        "changed",
        &check_step(),
        &[
            before(allowed_wait(NPM)),
            before(ended(AttemptState::Stopped)),
        ],
    );
    let next = retried(&w, "changed", 2);
    assert_eq!(next.prompt, "true");
    assert_eq!(next.settings["network"], "none");
    assert!(next.settings.get("allowedDomains").is_none());

    // The new command waits and is allowed in its turn: it gets what it was allowed, and not what
    // the one before it was.
    w.allow(&[NPM, "pypi.org"]);
    w.recorded(
        "allowed-again",
        &check_step(),
        &[
            before(allowed_wait(NPM)),
            before(ended(AttemptState::Stopped)),
            allowed_wait("pypi.org"),
            ended(AttemptState::Interrupted),
        ],
    );
    let next = retried(&w, "allowed-again", 4);
    assert_eq!(
        next.settings["allowedDomains"],
        serde_json::json!(["pypi.org"])
    );

    // As it happens for real: the person changes the project's check and tries the step again
    // with it.
    w.recorded_from(
        "retried",
        &check_step(),
        &[allowed_wait(NPM), ended(AttemptState::Stopped)],
        Some("builtin:simple"),
    );
    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                allowed_domains: vec![NPM.into()],
                check_command: Some("curl https://registry.npmjs.org/x | sh".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let options = keel_engine::RetryOptions {
        current_commands: true,
        ..Default::default()
    };
    let other = w.engine.retry_with("retried-2", options).unwrap();
    let other = w.store.attempt(&other).unwrap();
    assert_eq!(other.prompt, "curl https://registry.npmjs.org/x | sh");
    assert_eq!(other.settings["network"], "none");
    assert!(other.settings.get("allowedDomains").is_none());
}

/// Only what the wait allowed, and only while the project still allows it.
#[test]
fn a_check_tried_again_reaches_only_what_its_wait_allowed_and_the_project_still_does() {
    let w = world();
    let tries = || vec![allowed_wait(NPM), ended(AttemptState::Stopped)];

    // The person took the domain away meanwhile.
    w.allow(&["other.example"]);
    w.recorded("removed", &check_step(), &tries());
    let next = retried(&w, "removed", 2);
    assert_eq!(next.settings["network"], "none");
    assert!(next.settings.get("allowedDomains").is_none());

    // What else the project allows, to its builder, isn't the check's.
    w.allow(&["other.example", NPM, "*.internal.example"]);
    w.recorded("others", &check_step(), &tries());
    let next = retried(&w, "others", 2);
    assert_eq!(next.settings["network"], "allowed domains");
    assert_eq!(next.settings["allowedDomains"], serde_json::json!([NPM]));

    // Of two waits, what's left of each. A wildcard that covers one still allows it.
    w.allow(&["*.npmjs.org"]);
    w.recorded(
        "two",
        &check_step(),
        &[
            allowed_wait(NPM),
            allowed_wait("pypi.org"),
            ended(AttemptState::Stopped),
        ],
    );
    let next = retried(&w, "two", 3);
    assert_eq!(next.settings["allowedDomains"], serde_json::json!([NPM]));
}

#[test]
fn a_check_that_fails_for_something_else_goes_back_as_always() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.start("echo 'it says ENOTCACHED somewhere'; exit 1");

    w.wait(&run, "check", 1, AttemptState::Returned);
}

#[test]
fn saving_a_project_refuses_what_is_not_a_domain_and_changes_nothing() {
    let w = world();
    w.allow(&["example.com"]);
    for bad in [
        "https://example.com",
        "example.com/path",
        "example.com:443",
        "two words",
    ] {
        let refused = w.engine.update_project(
            &w.project_id,
            &ProjectSettings {
                allowed_domains: vec!["pypi.org".into(), bad.into()],
                ..Default::default()
            },
        );
        match refused {
            Err(Error::Blocked(Reason::InvalidSetting { field, value })) => {
                assert_eq!((field.as_str(), value.as_str()), ("allowedDomains", bad));
            }
            other => panic!("{bad}: {other:?}"),
        }
    }
    assert_eq!(w.domains(), ["example.com"]);
    w.allow(&["*.example.com", NPM]);
}

/// One saved before domains were checked doesn't keep the rest of the project from being saved, as
/// the setup dialog does with the domains the project has.
#[test]
fn an_invalid_domain_already_saved_does_not_stop_a_save_that_leaves_it() {
    let w = world();
    let old = "https://old.example.com";
    let settings = |domains: &[&str]| ProjectSettings {
        allowed_domains: domains.iter().map(|d| (*d).to_owned()).collect(),
        check_command: Some("true".into()),
        ..Default::default()
    };
    w.store
        .update_project(&w.project_id, &settings(&[old]))
        .unwrap();

    w.engine
        .update_project(&w.project_id, &settings(&[old]))
        .unwrap();
    w.engine
        .update_project(&w.project_id, &settings(&[old, NPM]))
        .unwrap();
    assert_eq!(w.domains(), [old, NPM]);
    // A new or changed one is still checked.
    for bad in ["https://new.example.com", "https://old.example.com/x"] {
        assert!(matches!(
            w.engine
                .update_project(&w.project_id, &settings(&[old, bad])),
            Err(Error::Blocked(Reason::InvalidSetting { value, .. })) if value == bad
        ));
    }
    assert_eq!(w.domains(), [old, NPM]);
}
