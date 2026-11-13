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
        risks: vec![],
        model_when_risky: None,
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
    // In the project's home, and in the run's copy of it.
    assert!(
        setup_cache.trim().ends_with("/cache/home/.cache"),
        "{setup_cache}"
    );
    assert_eq!(
        check_cache.trim(),
        workspace.join(".keel-cache/home/.cache").to_string_lossy()
    );
    assert!(Path::new(setup_cache.trim()).join("from-setup").exists());
    assert!(!Path::new(setup_cache.trim()).join("from-check").exists());

    // What tools leave in the run's cache never gets committed.
    assert!(
        workspace
            .join(".keel-cache/home/.cache/from-check")
            .exists()
    );
    let tracked = Command::new("git")
        .current_dir(workspace)
        .args(["ls-files"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&tracked.stdout).contains(".keel-cache"));
}

#[test]
fn a_setup_once_an_agent_has_run_gets_no_more_than_the_agent() {
    keel_harness::needs!(sandbox);
    let w = world();
    // A workflow can set up again after its builder, as a retry with a new setup does. What it
    // runs then may be the agent's doing: a postinstall script, a Makefile, a build backend.
    let setup = |name: &str| WorkflowStep::Setup {
        id: name.into(),
        run: format!(
            "echo $HOME > {name}-home.txt; touch \"$HOME/from-{name}\"; \
             mkdir -p \"$XDG_CACHE_HOME\"; touch \"$XDG_CACHE_HOME/from-{name}\""
        ),
    };
    let run = w.run(vec![
        setup("first"),
        agent("builder", ToolProfile::Build),
        setup("again"),
        WorkflowStep::Approval { id: "human".into() },
    ]);
    assert_eq!(w.wait_done(&run, "human").state, "waitingApproval");
    let first = w.wait_done(&run, "first");
    let again = w.wait_done(&run, "again");
    assert_eq!(first.state, "succeeded", "{}", w.check_output(&first));
    assert_eq!(again.state, "succeeded", "{}", w.check_output(&again));

    let workspace = Path::new(&again.workspace);
    let read = |file: &str| {
        std::fs::read_to_string(workspace.join(file))
            .unwrap()
            .trim()
            .to_owned()
    };
    // The first fills the project's home, in the cache its runs share.
    let project_home = PathBuf::from(read("first-home.txt"));
    assert!(project_home.ends_with("cache/home"), "{project_home:?}");
    assert!(project_home.join("from-first").exists());
    assert!(project_home.join(".cache/from-first").exists());
    // The second works in the run's own copy of it, as a check does, and leaves nothing there.
    let run_home = workspace.join(".keel-cache/home");
    assert_eq!(read("again-home.txt"), run_home.to_string_lossy());
    assert!(run_home.join("from-first").exists());
    assert!(run_home.join("from-again").exists());
    assert!(!project_home.join("from-again").exists());
    assert!(!project_home.join(".cache/from-again").exists());

    // Its sandbox has no way to the shared cache, home included, so it can't leave anything there
    // for the next run to start from (`check_sandbox` tries, with the cache in the home folder as
    // it is in a real install: here it's in a temporary folder, which every command may write to).
    // And no network: an agent's Bash reaches only the allowed domains, and a command Keel runs
    // can't be held to those, so it gets none.
    let events = |attempt: &keel_store::Attempt| w.store.events(&attempt.id, 0).unwrap();
    let profile = |attempt: &keel_store::Attempt| {
        events(attempt)
            .into_iter()
            .find(|e| e.kind == "started")
            .map(|e| e.data["profile"].as_str().unwrap_or_default().to_owned())
            .unwrap()
    };
    let shared = project_home
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(profile(&first).contains(&shared));
    assert!(!profile(&again).contains(&shared));
    assert!(profile(&first).contains("(allow network*)"));
    assert!(profile(&again).contains("(deny network*)"));
    assert!(!profile(&again).contains("(allow network*)"));
    assert_eq!(first.settings["network"], "internet, nothing local");
    assert_eq!(again.settings["network"], "none");
    assert_eq!(again.settings["home"], "the run's own copy");
    // The person sees why, in its activity.
    let limited =
        |attempt: &keel_store::Attempt| events(attempt).iter().any(|e| e.kind == "setupLimited");
    assert!(!limited(&first));
    assert!(limited(&again));

    // The next run starts from the project's home, with what the first setup left and nothing
    // from the second.
    let next = w.run(vec![
        check("look", "ls -a \"$HOME\" \"$XDG_CACHE_HOME\" > home.txt"),
        WorkflowStep::Approval { id: "human".into() },
    ]);
    let look = w.wait_done(&next, "look");
    assert_eq!(look.state, "succeeded", "{}", w.check_output(&look));
    let listed = std::fs::read_to_string(Path::new(&look.workspace).join("home.txt")).unwrap();
    assert!(listed.contains("from-first"), "{listed}");
    assert!(!listed.contains("from-again"), "{listed}");
}

#[test]
fn a_setup_after_an_agent_that_only_reads_still_installs_with_the_network() {
    keel_harness::needs!(sandbox);
    let w = world();
    // A saved workflow with a setup of its own gets none put first, so its spec, which only reads,
    // comes before the setup on every run, not only on a retry. It has no way to change a file.
    let run = w.run(vec![
        agent("spec", ToolProfile::ReadOnly),
        WorkflowStep::Setup {
            id: "install".into(),
            run: "echo $HOME > install-home.txt; touch \"$HOME/from-install\"".into(),
        },
        agent("builder", ToolProfile::Build),
        check("after", "test -f \"$HOME/from-install\""),
        WorkflowStep::Approval { id: "human".into() },
    ]);
    assert_eq!(w.wait_done(&run, "human").state, "waitingApproval");
    let install = w.wait_done(&run, "install");
    assert_eq!(install.state, "succeeded", "{}", w.check_output(&install));
    let after = w.wait_done(&run, "after");
    assert_eq!(after.state, "succeeded", "{}", w.check_output(&after));
    let attempts = w.store.attempts_of_run(&run).unwrap();
    assert_eq!(
        attempts
            .iter()
            .map(|a| a.step_id.as_str())
            .collect::<Vec<_>>(),
        ["spec", "install", "builder", "after", "human"]
    );

    // It's a setup as any first one is: the internet, and the project's home, which it fills.
    let workspace = Path::new(&install.workspace);
    let home = std::fs::read_to_string(workspace.join("install-home.txt")).unwrap();
    let home = PathBuf::from(home.trim());
    assert!(home.ends_with("cache/home"), "{home:?}");
    assert!(home.join("from-install").exists());
    assert_eq!(install.settings["network"], "internet, nothing local");
    assert_eq!(install.settings["home"], "the project's");
    let events = w.store.events(&install.id, 0).unwrap();
    assert!(!events.iter().any(|e| e.kind == "setupLimited"));
    let profile = events
        .iter()
        .find(|e| e.kind == "started")
        .and_then(|e| e.data["profile"].as_str())
        .unwrap();
    assert!(profile.contains("(allow network*)"));
}

#[test]
fn what_setup_downloads_stays_in_the_project_cache_and_each_run_gets_its_own_copy() {
    keel_harness::needs!(sandbox);
    let w = world();
    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                // Where cargo and Go keep what they download, in the home folder.
                setup_command: Some(
                    "echo $HOME > setup-home.txt; mkdir -p ~/.cargo/registry ~/go/pkg; \
                     test -f ~/.cargo/registry/dep || echo downloaded >> ~/.cargo/registry/count; \
                     touch ~/.cargo/registry/dep ~/go/pkg/mod"
                        .into(),
                ),
                ..ProjectSettings::default()
            },
        )
        .unwrap();
    let probe = "echo $HOME > check-home.txt; echo \"[$CARGO_NET_OFFLINE][$GOPROXY]\" > check-offline.txt; \
                 test -f ~/.cargo/registry/dep && test -f ~/go/pkg/mod || exit 7; \
                 touch ~/.cargo/registry/changed-by-check";
    let first = w.run(vec![check("after", probe)]);
    let after = w.wait_done(&first, "after");
    assert_eq!(after.state, "succeeded", "{}", w.check_output(&after));
    let workspace = Path::new(&after.workspace);
    let shared = std::fs::read_to_string(workspace.join("setup-home.txt")).unwrap();
    let shared = Path::new(shared.trim()).join(".cargo");
    let own = std::fs::read_to_string(workspace.join("check-home.txt")).unwrap();
    assert!(
        shared.starts_with(w.dir.path()) && shared.ends_with("cache/home/.cargo"),
        "{shared:?}"
    );
    assert!(
        own.trim().starts_with(&*workspace.to_string_lossy()),
        "{own}"
    );
    // A check has no network, so cargo and Go fail at once.
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
