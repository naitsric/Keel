//! A project's services through whole steps: real sockets, the real `sandbox-exec`, and a fake
//! `claude` that records what it was given.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, EnvVar, Reason, Service, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, Error, StartRun};
use keel_harness::Tools;
use keel_store::{ProjectSettings, Store};

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
    log: PathBuf,
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

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let log = dir.path().join("log");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&log).unwrap();
    let fake = format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true}}'; exit 0;;
esac
cat > /dev/null
tools=""; settings=""; prev=""
for a in "$@"; do
  [ "$prev" = "--tools" ] && tools="$a"
  [ "$prev" = "--settings" ] && settings="$a"
  prev="$a"
done
{{ echo "=== tools=$tools DB_HOST=${{DB_HOST:-unset}}"; cat "$settings"; echo; }} >> "{log}/claude.log"
list=$(printf '%s' "$tools" | sed 's/,/","/g')
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
echo '{{"type":"result","subtype":"success","is_error":false,"result":"done","num_turns":1}}'
"##,
        log = log.display(),
    );
    let path = bin.join("claude");
    std::fs::write(&path, fake).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    keel_harness::testing::warm_up(&path, &["--version"]);

    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "x\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[
        bin,
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ]);
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let project_id = engine.add_project(&repo).unwrap().id;
    World {
        dir,
        store,
        engine,
        project_id,
        log,
    }
}

/// A TCP server standing in for a database in Docker. It answers `pong` to anything.
fn tcp_server() -> u16 {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = server.accept() {
            std::thread::spawn(move || {
                let mut buffer = [0u8; 16];
                if matches!(stream.read(&mut buffer), Ok(n) if n > 0) {
                    let _ = stream.write_all(b"pong");
                }
            });
        }
    });
    port
}

const CONNECT: &str = r#"python3 -c "import os, socket; c = socket.socket(socket.AF_UNIX); c.connect(os.environ['DB_SOCKET']); c.sendall(b'ping'); assert c.recv(4) == b'pong'""#;

impl World {
    fn configure(&self, services: Vec<Service>, env: Vec<EnvVar>) {
        self.engine
            .update_project(
                &self.project_id,
                &ProjectSettings {
                    github_account: None,
                    setup_command: None,
                    check_command: None,
                    allowed_domains: vec![],
                    allowed_read: vec![],
                    services,
                    env,
                    default_workflow: None,
                    sensitive_paths: vec![],
                },
            )
            .unwrap();
    }

    fn run(&self, workflow: Vec<WorkflowStep>) -> String {
        self.engine
            .start_run(StartRun {
                project_id: self.project_id.clone(),
                title: None,
                request: "x".into(),
                workflow,
                language: "en".into(),
                source_issue: None,
                saved_workflow: None,
                started_by: None,
            })
            .unwrap()
    }

    fn wait_done(&self, run: &str, id: &str) -> keel_store::Attempt {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            if let Some(a) = self
                .store
                .attempts_of_run(run)
                .unwrap()
                .into_iter()
                .rev()
                .find(|a| a.step_id == id)
                && !["queued", "preparing", "running", "stalled"].contains(&a.state.as_str())
            {
                return a;
            }
            assert!(Instant::now() < deadline, "{id} never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn check_output(&self, attempt: &keel_store::Attempt) -> String {
        let event = self
            .store
            .events(&attempt.id, 0)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "checkResult");
        event
            .map(|e| format!("{} {}", e.data["stdout"], e.data["stderr"]))
            .unwrap_or_default()
    }
}

fn check(id: &str, run: &str) -> WorkflowStep {
    WorkflowStep::Check {
        id: id.into(),
        run: run.into(),
    }
}

fn agent(id: &str, tools: ToolProfile) -> WorkflowStep {
    WorkflowStep::Agent(StepSpec {
        id: id.into(),
        model: "fake".into(),
        effort: None,
        tools,
        prompt: "x".into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        role: Default::default(),
    })
}

fn db_env() -> Vec<EnvVar> {
    vec![
        EnvVar {
            name: "DB_SOCKET".into(),
            value: "{{socket:db}}".into(),
        },
        EnvVar {
            name: "DB_HOST".into(),
            value: "{{socketDir:db}}".into(),
        },
    ]
}

#[test]
fn a_check_reaches_a_database_on_a_local_port_only_through_its_socket() {
    keel_harness::needs!(sandbox, sockets);
    let w = world();
    let port = tcp_server();
    w.configure(
        vec![Service::Bridge {
            name: "db".into(),
            port,
            socket: None,
        }],
        db_env(),
    );
    let direct = format!(
        r#"python3 -c "import socket; socket.create_connection(('127.0.0.1', {port}), 2)""#
    );
    let run = w.run(vec![
        check("through-socket", CONNECT),
        check("direct-tcp", &direct),
    ]);

    let through = w.wait_done(&run, "through-socket");
    assert_eq!(through.state, "succeeded", "{}", w.check_output(&through));
    let tcp = w.wait_done(&run, "direct-tcp");
    assert_eq!(tcp.state, "failed", "direct TCP has to stay blocked");

    let event = w
        .store
        .events(&through.id, 0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "services")
        .unwrap();
    assert_eq!(event.data["services"][0]["name"], "db");
    assert_eq!(event.data["env"][0]["name"], "DB_SOCKET");
}

#[test]
fn the_socket_is_gone_once_the_step_ends() {
    keel_harness::needs!(sandbox, sockets);
    let w = world();
    let port = tcp_server();
    w.configure(
        vec![Service::Bridge {
            name: "db".into(),
            port,
            socket: None,
        }],
        db_env(),
    );
    let run = w.run(vec![check("first", CONNECT)]);
    let first = w.wait_done(&run, "first");
    assert_eq!(first.state, "succeeded");
    let event = w
        .store
        .events(&first.id, 0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "services")
        .unwrap();
    let socket = PathBuf::from(event.data["sockets"][0].as_str().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "socket still there");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_command_service_runs_for_the_step_and_stops_after_it() {
    keel_harness::needs!(sandbox, sockets);
    let w = world();
    let script = w.dir.path().join("server.py");
    let pid_file = w.dir.path().join("server.pid");
    std::fs::write(
        &script,
        format!(
            "import os, socket\nopen('{}', 'a').write(str(os.getpid()) + '\\n')\ns = socket.socket(socket.AF_UNIX)\ns.bind(os.environ['KEEL_SOCKET'])\ns.listen(8)\nwhile True:\n    c, _ = s.accept()\n    try:\n        c.recv(16)\n        c.sendall(b'pong')\n    except OSError:\n        pass\n    c.close()\n",
            pid_file.display()
        ),
    )
    .unwrap();
    w.configure(
        vec![Service::Command {
            name: "db".into(),
            start: format!("exec python3 {}", script.display()),
            socket: None,
        }],
        db_env(),
    );
    let run = w.run(vec![
        agent("reader", ToolProfile::ReadOnly),
        check("uses-it", CONNECT),
        check("uses-it-again", CONNECT),
    ]);
    let second = w.wait_done(&run, "uses-it-again");
    let first = w.wait_done(&run, "uses-it");
    assert_eq!(first.state, "succeeded", "{}", w.check_output(&first));
    assert_eq!(second.state, "succeeded", "{}", w.check_output(&second));

    // Started once per step that runs commands, and never for the read-only one.
    let pids = std::fs::read_to_string(&pid_file).unwrap();
    assert_eq!(pids.lines().count(), 2, "{pids}");
    let reader = w.wait_done(&run, "reader");
    assert!(
        !w.store
            .events(&reader.id, 0)
            .unwrap()
            .iter()
            .any(|e| e.kind == "services")
    );
    for pid in pids.lines() {
        let alive = Command::new("/bin/kill")
            .args(["-0", pid])
            .output()
            .unwrap()
            .status
            .success();
        assert!(!alive, "server {pid} should have stopped");
    }
}

#[test]
fn agents_that_run_commands_get_the_socket_and_read_only_ones_do_not() {
    keel_harness::needs!(sandbox, sockets);
    let w = world();
    let port = tcp_server();
    w.configure(
        vec![Service::Bridge {
            name: "db".into(),
            port,
            socket: Some(".s.PGSQL.5432".into()),
        }],
        db_env(),
    );
    let run = w.run(vec![
        agent("reader", ToolProfile::ReadOnly),
        agent("builder", ToolProfile::Build),
    ]);
    assert_eq!(w.wait_done(&run, "builder").state, "succeeded");
    let log = std::fs::read_to_string(w.log.join("claude.log")).unwrap();
    let mut sections = log.split("=== tools=").skip(1);
    let reader = sections.next().unwrap();
    let builder = sections.next().unwrap();
    assert!(
        reader.starts_with("Read,Grep,Glob DB_HOST=unset"),
        "{reader}"
    );
    assert!(!reader.contains("allowUnixSockets"), "{reader}");
    assert!(builder.contains("allowUnixSockets"), "{builder}");
    assert!(!builder.contains("DB_HOST=unset"), "{builder}");
    assert!(!builder.contains("allowLocalBinding"));
    let attempt = w.wait_done(&run, "builder");
    let dirs = attempt
        .settings
        .pointer("/sandbox/network/allowUnixSockets")
        .unwrap()
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].as_str().unwrap().ends_with("/db"));
}

#[test]
fn a_service_that_cannot_start_blocks_the_step_before_any_agent_runs() {
    keel_harness::needs!(sandbox, sockets);
    let w = world();
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    w.configure(
        vec![Service::Bridge {
            name: "db".into(),
            port: closed,
            socket: None,
        }],
        vec![],
    );
    let run = w.run(vec![agent("builder", ToolProfile::Build)]);
    let attempt = w.wait_done(&run, "builder");
    assert_eq!(attempt.state, AttemptState::Blocked.as_str());
    let reason: Reason = serde_json::from_value(attempt.reason.unwrap()).unwrap();
    let Reason::ServiceFailed { name, detail } = reason else {
        panic!("wrong reason")
    };
    assert_eq!(name, "db");
    assert!(detail.contains(&closed.to_string()));
    assert!(
        !w.log.join("claude.log").exists(),
        "the agent never started"
    );
}

#[test]
fn settings_that_could_reach_a_credential_are_refused() {
    let w = world();
    let error = w
        .engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                services: vec![],
                env: vec![EnvVar {
                    name: "GH_TOKEN".into(),
                    value: "x".into(),
                }],
                ..ProjectSettings::default()
            },
        )
        .unwrap_err();
    assert!(matches!(
        error,
        Error::Blocked(Reason::InvalidSetting { .. })
    ));
    assert!(w.store.project(&w.project_id).unwrap().env.is_empty());
}

#[test]
fn setup_runs_first_and_later_steps_use_their_own_cache() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings { setup_command: Some("echo $XDG_CACHE_HOME > setup-cache.txt; mkdir -p \"$XDG_CACHE_HOME\"; touch \"$XDG_CACHE_HOME/from-setup\"".into()), ..ProjectSettings::default() },
        )
        .unwrap();
    // The approval keeps the run open, since a run that ends frees its copy of the cache.
    let run = w.run(vec![
        check("after", "echo $XDG_CACHE_HOME > check-cache.txt; mkdir -p \"$XDG_CACHE_HOME\"; touch \"$XDG_CACHE_HOME/from-check\""),
        agent("builder", ToolProfile::Build),
        WorkflowStep::Approval { id: "human".into() },
    ]);
    assert_eq!(w.wait_done(&run, "human").state, "waitingApproval");
    let builder = w.wait_done(&run, "builder");
    assert_eq!(builder.state, "succeeded");
    let attempts = w.store.attempts_of_run(&run).unwrap();
    assert_eq!(
        attempts.iter().map(|a| a.kind.as_str()).collect::<Vec<_>>(),
        ["setup", "check", "agent", "approval"]
    );
    assert_eq!(
        attempts[0].state,
        "succeeded",
        "{}",
        w.check_output(&attempts[0])
    );

    let workspace = Path::new(&builder.workspace);
    let setup_cache = std::fs::read_to_string(workspace.join("setup-cache.txt")).unwrap();
    let check_cache = std::fs::read_to_string(workspace.join("check-cache.txt")).unwrap();
    assert!(setup_cache.trim().ends_with("/cache"), "{setup_cache}");
    assert!(
        check_cache
            .trim()
            .starts_with(&*workspace.to_string_lossy()),
        "{check_cache}"
    );
    assert!(Path::new(setup_cache.trim()).join("from-setup").exists());
    assert!(!Path::new(setup_cache.trim()).join("from-check").exists());

    // What tools leave in the run's cache never gets committed.
    assert!(workspace.join(".keel-cache/from-check").exists());
    let tracked = Command::new("git")
        .current_dir(workspace)
        .args(["ls-files"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&tracked.stdout).contains(".keel-cache"));
}

#[test]
fn what_setup_downloads_stays_in_the_project_cache_and_each_run_gets_its_own_copy() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                setup_command: Some(
                    "echo $CARGO_HOME > setup-cargo.txt; mkdir -p \"$CARGO_HOME/registry\" \"$GOPATH/pkg\"; \
                     test -f \"$CARGO_HOME/registry/dep\" || echo downloaded >> \"$CARGO_HOME/registry/count\"; \
                     touch \"$CARGO_HOME/registry/dep\" \"$GOPATH/pkg/mod\""
                        .into(),
                ),
                ..ProjectSettings::default()
            },
        )
        .unwrap();
    let probe = "echo $CARGO_HOME > check-cargo.txt; echo \"[$CARGO_NET_OFFLINE][$GOPROXY]\" > check-offline.txt; \
                 test -f \"$CARGO_HOME/registry/dep\" && test -f \"$GOPATH/pkg/mod\" || exit 7; \
                 touch \"$CARGO_HOME/registry/changed-by-check\"";
    let first = w.run(vec![check("after", probe)]);
    let after = w.wait_done(&first, "after");
    assert_eq!(after.state, "succeeded", "{}", w.check_output(&after));
    let workspace = Path::new(&after.workspace);
    let shared = std::fs::read_to_string(workspace.join("setup-cargo.txt")).unwrap();
    let shared = Path::new(shared.trim());
    let own = std::fs::read_to_string(workspace.join("check-cargo.txt")).unwrap();
    assert!(
        shared.starts_with(w.dir.path()) && shared.ends_with("cache/cargo"),
        "{shared:?}"
    );
    assert!(
        own.trim().starts_with(&*workspace.to_string_lossy()),
        "{own}"
    );
    // Without crates.io among the project's domains, cargo and Go fail at once.
    let offline = std::fs::read_to_string(workspace.join("check-offline.txt")).unwrap();
    assert_eq!(offline.trim(), "[true][off]");
    // What the run changed in its copy never reached the project's cache.
    assert!(shared.join("registry/dep").exists());
    assert!(!shared.join("registry/changed-by-check").exists());

    // A second run copies the same cache, without the first run's change, and downloads nothing.
    let second = w.run(vec![check("after", probe)]);
    let after = w.wait_done(&second, "after");
    assert_eq!(after.state, "succeeded", "{}", w.check_output(&after));
    let attempts = w.store.attempts_of_run(&second).unwrap();
    assert_eq!(
        std::fs::read_to_string(shared.join("registry/count"))
            .unwrap()
            .lines()
            .count(),
        1,
        "the second setup found it already there"
    );
    assert!(attempts.iter().any(|a| a.kind == "setup"));
    assert!(!shared.join("registry/changed-by-check").exists());
}

#[test]
fn a_project_that_allows_crates_io_is_not_forced_offline() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                allowed_domains: vec!["index.crates.io".into(), "proxy.golang.org".into()],
                ..ProjectSettings::default()
            },
        )
        .unwrap();
    let run = w.run(vec![check(
        "after",
        "echo \"[$CARGO_NET_OFFLINE][$GOPROXY]\" > offline.txt",
    )]);
    let after = w.wait_done(&run, "after");
    assert_eq!(after.state, "succeeded", "{}", w.check_output(&after));
    let text = std::fs::read_to_string(Path::new(&after.workspace).join("offline.txt")).unwrap();
    assert_eq!(text.trim(), "[][]");
}

#[test]
fn a_failed_setup_stops_the_run_before_any_agent_and_can_be_retried() {
    keel_harness::needs!(sandbox);
    let w = world();
    let flag = w.dir.path().join("setup-may-pass");
    // The flag is outside the workspace but in temp, which sandboxed commands can read.
    let marker = format!("/tmp/keel-setup-test-{}", std::process::id());
    let _ = std::fs::remove_file(&marker);
    let _ = flag;
    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                setup_command: Some(format!("test -f {marker} || exit 4")),
                ..ProjectSettings::default()
            },
        )
        .unwrap();
    let run = w.run(vec![agent("builder", ToolProfile::Build)]);
    let setup = w.wait_done(&run, "setup");
    assert_eq!(setup.state, "failed");
    let reason: Reason = serde_json::from_value(setup.reason.clone().unwrap()).unwrap();
    assert_eq!(reason, Reason::SetupFailed { exit_code: Some(4) });
    assert!(!w.log.join("claude.log").exists(), "no agent ran");

    std::fs::write(&marker, "").unwrap();
    w.engine.retry(&setup.id, false).unwrap();
    assert_eq!(w.wait_done(&run, "builder").state, "succeeded");
    let _ = std::fs::remove_file(&marker);
}
