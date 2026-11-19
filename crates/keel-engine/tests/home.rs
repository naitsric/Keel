//! The home folder setup, check and the agents' commands run in, through whole steps: a real repo,
//! the real `sandbox-exec` and a fake `claude`. Setup fills the project's own home, with the network,
//! and each run's check and builder work in a copy of it. The real home folder stays out of reach.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, StartRun};
use keel_harness::Tools;
use keel_harness::testing::{skipped, write_script};
use keel_store::{Attempt, ProjectSettings, Store};

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    repo: PathBuf,
    /// What the fake `claude` writes down, and the Bash command it runs.
    log: PathBuf,
}

/// Stands in for `claude`. It runs one Bash command, the one in `log/command`, the way Claude Code
/// does: after what the file in `CLAUDE_ENV_FILE` says, in a sandbox that lets it write only in the
/// project, reading from a pipe that stays open and that nobody writes to. It writes down its own `HOME`, which file it was given, and what the command printed,
/// in `log/bash-new.txt`, or `log/bash-resumed.txt` for a session it continues. With
/// `log/fail-first`, a session it starts ends without a result.
fn fake_claude(log: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true}}'; exit 0;;
esac
cat > /dev/null
tools=""; session=new; prev=""
for a in "$@"; do
  [ "$prev" = "--tools" ] && tools="$a"
  [ "$a" = "--resume" ] && session=resumed
  prev="$a"
done
out="{log}/bash-$session.txt"
{{ echo "claude=$HOME"; echo "envfile=$CLAUDE_ENV_FILE"; }} > "$out"
script="$([ -n "$CLAUDE_ENV_FILE" ] && cat "$CLAUDE_ENV_FILE")
$(cat "{log}/command")"
mkfifo "{log}/stdin-$$" && exec 3<>"{log}/stdin-$$"
/usr/bin/sandbox-exec -p "(version 1)(allow default)(deny file-write* (subpath \"$HOME\"))(allow file-write* (subpath \"$PWD\"))" \
  /bin/sh -c "$script" <&3 >> "$out" 2>&1
exec 3>&-
[ $session = new ] && [ -e "{log}/fail-first" ] && exit 3
list=$(printf '%s' "$tools" | sed 's/,/","/g')
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
echo '{{"type":"result","subtype":"success","is_error":false,"result":"done","num_turns":1}}'
"##,
        log = log.display(),
    )
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

/// A repo with these files, and Keel with its data beside it. Inside the build folder, which is in the
/// home folder like a real install, so the sandbox has the same folders to find its way through.
fn world(files: &[(&str, &str)]) -> World {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "x\n").unwrap();
    for (name, text) in files {
        let path = repo.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let bin = dir.path().join("bin");
    let log = dir.path().join("log");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&log).unwrap();
    write_script(&bin.join("claude"), &fake_claude(&log)).unwrap();
    // The commands find what this test finds, cargo included, and the fake `claude` first.
    let dirs: Vec<PathBuf> = std::iter::once(bin)
        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))
        .collect();
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    let engine = Engine::new(store.clone(), config, Tools::from_dirs(&dirs), |_| {});
    World {
        dir,
        store,
        engine,
        repo,
        log,
    }
}

fn agent(id: &str) -> WorkflowStep {
    WorkflowStep::Agent(StepSpec {
        id: id.into(),
        model: "fake".into(),
        effort: None,
        tools: ToolProfile::Build,
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

fn check(id: &str, run: &str) -> WorkflowStep {
    WorkflowStep::Check {
        id: id.into(),
        run: run.into(),
    }
}

impl World {
    fn add(&self) -> String {
        self.engine.add_project(&self.repo).unwrap().id
    }

    /// Setup and then the check, as the setup dialog tries them: with the folders Keel finds for
    /// them, which the dialog lets them read unless the person says otherwise. Both, as they ended.
    fn try_check(&self, id: &str, setup: &str, check: &str) -> (Attempt, Attempt) {
        let read: Vec<String> = self
            .engine
            .tool_folders(id, setup, check)
            .unwrap()
            .into_iter()
            .map(|found| found.folder)
            .collect();
        let run = self
            .engine
            .try_check_reading(id, setup, check, &read, "en")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(300);
        while self.store.run(&run).unwrap().finished_at.is_none() {
            assert!(Instant::now() < deadline, "the run never finished");
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut attempts = self.store.attempts_of_run(&run).unwrap().into_iter();
        let setup = attempts.next().unwrap();
        let Some(check) = attempts.next() else {
            panic!("the check never ran: {}", self.output(&setup));
        };
        assert_eq!(
            (setup.kind.as_str(), check.kind.as_str()),
            ("setup", "check")
        );
        (setup, check)
    }

    fn output(&self, attempt: &Attempt) -> String {
        let data = self
            .store
            .events(&attempt.id, 0)
            .unwrap()
            .into_iter()
            .find(|event| event.kind == "checkResult")
            .unwrap()
            .data;
        format!(
            "{}{}",
            data["stdout"].as_str().unwrap_or(""),
            data["stderr"].as_str().unwrap_or("")
        )
    }

    fn passed(&self, attempt: &Attempt) -> String {
        let output = self.output(attempt);
        assert_eq!(
            attempt.state, "succeeded",
            "{} failed: {output}",
            attempt.kind
        );
        output
    }

    /// The home setup fills for the project.
    fn project_home(&self, id: &str) -> PathBuf {
        self.dir
            .path()
            .join("data/projects")
            .join(id)
            .join("cache/home")
    }

    /// Starts a run whose builder's Bash runs `command`.
    fn run(&self, id: &str, command: &str, workflow: Vec<WorkflowStep>) -> String {
        std::fs::write(self.log.join("command"), command).unwrap();
        self.engine
            .start_run(StartRun {
                project_id: id.to_owned(),
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

    /// The last try of a step, once it's over or waits for someone.
    fn wait_done(&self, run: &str, step: &str) -> Attempt {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(attempt) = self
                .store
                .attempts_of_run(run)
                .unwrap()
                .into_iter()
                .rev()
                .find(|attempt| attempt.step_id == step)
                && !["queued", "preparing", "running", "stalled"].contains(&attempt.state.as_str())
            {
                return attempt;
            }
            assert!(Instant::now() < deadline, "{step} never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// What the fake `claude` wrote down, for a session it started or one it continued.
    fn bash(&self, session: &str) -> String {
        std::fs::read_to_string(self.log.join(format!("bash-{session}.txt"))).unwrap()
    }

    /// Lets the project's builder reach these domains.
    fn allow(&self, id: &str, domains: &[&str]) {
        self.engine
            .update_project(
                id,
                &ProjectSettings {
                    allowed_domains: domains.iter().map(|d| (*d).to_owned()).collect(),
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
    }

    /// How long a command took, from when Keel started it to when it ended.
    fn took(&self, attempt: &Attempt) -> Duration {
        let events = self.store.events(&attempt.id, 0).unwrap();
        let at = |kind: &str| {
            let event = events.iter().find(|event| event.kind == kind).unwrap();
            chrono::DateTime::parse_from_rfc3339(&event.at).unwrap()
        };
        (at("checkResult") - at("started")).to_std().unwrap()
    }
}

/// This test's own home folder, which the fake `claude` gets as Keel's does.
fn real_home() -> String {
    std::env::var("HOME").unwrap()
}

/// What a command says about where it is, one `name=value` a line.
const WHERE: &str = "echo \"HOME=$HOME\" && echo \"XDG_CONFIG_HOME=$XDG_CONFIG_HOME\" && \
                     echo \"XDG_CACHE_HOME=$XDG_CACHE_HOME\" && echo \"XDG_DATA_HOME=$XDG_DATA_HOME\" && \
                     echo \"XDG_STATE_HOME=$XDG_STATE_HOME\"";

fn said<'a>(output: &'a str, name: &str) -> &'a str {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {output}"))
}

#[test]
fn what_setup_leaves_in_its_home_the_check_finds_in_its_own() {
    keel_harness::needs!(sandbox);
    let w = world(&[]);
    let id = w.add();
    // A tool that keeps what it downloads in a folder of its own in the home folder.
    let (setup, check) = w.try_check(
        &id,
        &format!("mkdir -p ~/.algo && echo downloaded > ~/.algo/dependency && {WHERE}"),
        &format!("cat ~/.algo/dependency && {WHERE}"),
    );
    let installed = w.passed(&setup);
    let checked = w.passed(&check);
    assert!(checked.starts_with("downloaded\n"), "{checked}");

    // Setup's home is the project's, and the check's is a copy inside its run.
    let project_home = w.project_home(&id);
    assert_eq!(said(&installed, "HOME"), project_home.to_string_lossy());
    let workspace = PathBuf::from(&check.workspace);
    assert_eq!(
        said(&checked, "HOME"),
        workspace.join(".keel-cache/home").to_string_lossy()
    );
    assert_eq!(
        std::fs::read_to_string(project_home.join(".algo/dependency")).unwrap(),
        "downloaded\n"
    );
    // Where tools keep their configuration, caches and data is in there too, never the real one.
    for output in [&installed, &checked] {
        let home = said(output, "HOME");
        for (name, folder) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_STATE_HOME", ".local/state"),
        ] {
            assert_eq!(said(output, name), format!("{home}/{folder}"), "{name}");
        }
    }
}

#[test]
fn what_a_check_writes_in_its_home_reaches_neither_the_project_nor_the_next_run() {
    keel_harness::needs!(sandbox);
    let w = world(&[]);
    let id = w.add();
    let (setup, check) = w.try_check(
        &id,
        "mkdir -p ~/.algo && echo setup > ~/.algo/state",
        "echo check > ~/.algo/state && echo mine > ~/.algo/mine && rm -rf ~/.config",
    );
    w.passed(&setup);
    w.passed(&check);
    let project_home = w.project_home(&id);
    assert_eq!(
        std::fs::read_to_string(project_home.join(".algo/state")).unwrap(),
        "setup\n"
    );
    assert!(!project_home.join(".algo/mine").exists());

    // The next run's setup leaves the home as it is, and its check starts from setup's.
    let (setup, check) = w.try_check(&id, "true", "cat ~/.algo/state && test ! -e ~/.algo/mine");
    w.passed(&setup);
    assert_eq!(w.passed(&check), "setup\n");
}

#[test]
fn a_test_that_commits_has_someone_to_commit_as_and_the_run_keeps_its_own() {
    keel_harness::needs!(sandbox);
    let w = world(&[]);
    let id = w.add();
    // Test suites make repos of their own and commit to them, often counting on a git identity
    // in the home folder.
    let (setup, check) = w.try_check(
        &id,
        "true",
        "repo=$(mktemp -d) && git -C \"$repo\" init -q && \
         git -C \"$repo\" commit -q --allow-empty -m test && \
         echo \"test=$(git -C \"$repo\" log -1 --format='%an <%ae>')\" && \
         echo \"run=$(git var GIT_AUTHOR_IDENT | sed 's/>.*/>/')\"",
    );
    w.passed(&setup);
    let output = w.passed(&check);
    assert_eq!(said(&output, "test"), "Keel <keel@localhost>");
    // The run's own repo says who its commits are from, and that wins.
    assert_eq!(said(&output, "run"), "T <t@example.com>");
}

/// #94: cargo lists a package's files to know whether to run its build script again, and to do
/// that it asks git which files are ignored, which takes git's own ignore file in the home folder.
/// The run's clone names an empty one in its config, but any other repo doesn't: one a test makes,
/// a submodule, a plain clone. In the real home folder the sandbox refused that file, so cargo
/// failed there before it built anything, and git complained about it on every command.
#[test]
fn a_crate_with_a_build_script_compiles_from_an_empty_target_in_any_repo() {
    keel_harness::needs!(sandbox);
    let has_cargo = Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !has_cargo {
        keel_harness::testing::skipped("cargo isn't installed");
        return;
    }
    let w = world(&[
        (
            "probe/Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             # Its own workspace, so cargo doesn't look for one in the folders above.\n[workspace]\n",
        ),
        // Without `rerun-if-changed`, so cargo goes by every file in the package.
        ("probe/build.rs", "fn main() {}\n"),
        (
            "probe/src/lib.rs",
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() {\n        assert_eq!(1 + 1, 2);\n    }\n}\n",
        ),
    ]);
    let id = w.add();
    let (setup, check) = w.try_check(
        &id,
        "true",
        "cd probe && git init -q && git add -A && git commit -qm probe && test ! -e target && \
         git status --porcelain 2>&1 && cargo test",
    );
    w.passed(&setup);
    let output = w.passed(&check);
    assert!(output.contains("test tests::adds ... ok"), "{output}");
    assert!(!output.contains("not permitted"), "{output}");
}

/// #119: the builder's commands run in the same home as the check of its run, so what passes in one
/// passes in the other. `claude` itself keeps the real home folder, where its login and sessions are.
#[test]
fn what_the_builders_commands_leave_in_their_home_the_check_of_the_run_finds() {
    keel_harness::needs!(sandbox);
    let w = world(&[]);
    let id = w.add();
    let run = w.run(
        &id,
        &format!("mkdir -p ~/.algo && echo built > ~/.algo/dependency && {WHERE}"),
        vec![
            agent("builder"),
            check("check", &format!("cat ~/.algo/dependency && {WHERE}")),
            // Keeps the run open, and its copy with it.
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    let builder = w.wait_done(&run, "builder");
    assert_eq!(builder.state, "succeeded");
    let built = w.bash("new");
    let check = w.wait_done(&run, "check");
    assert_eq!(check.state, "succeeded", "builder: {built}");
    let checked = w.passed(&check);
    assert!(
        checked.starts_with("built\n"),
        "{checked}\nbuilder: {built}"
    );
    assert_eq!(w.wait_done(&run, "human").state, "waitingApproval");

    let workspace = PathBuf::from(&builder.workspace);
    assert_eq!(
        said(&built, "HOME"),
        workspace.join(".keel-cache/home").to_string_lossy()
    );
    for name in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
    ] {
        assert_eq!(said(&built, name), said(&checked, name), "{name}");
    }
    assert_eq!(said(&built, "claude"), real_home());
    // What says so is Keel's, outside the project, where the agent can't change it.
    let file = PathBuf::from(said(&built, "envfile"));
    assert!(
        file.is_absolute() && !file.starts_with(&workspace),
        "{}",
        file.display()
    );
}

/// What a command says about who could answer it, and whether reading with nothing to read ended by
/// itself. The read gives up after 5 seconds, so a test never leaves one waiting.
const WHO_ANSWERS: &str = "perl -e 'alarm 5; exec @ARGV' head -1; echo \"read=$?\"; \
                           echo \"TERM=$TERM\"; echo \"GIT_EDITOR=$GIT_EDITOR\"; echo \"EDITOR=$EDITOR\"; \
                           echo \"PAGER=$PAGER\"; echo \"GIT_PAGER=$GIT_PAGER\"; \
                           echo \"GIT_TERMINAL_PROMPT=$GIT_TERMINAL_PROMPT\"";

/// A builder's `git checkout -p` and a `head` with no file each left a step waiting, for as long as
/// 26 minutes, for someone to type. Nobody can, so a command that reads finds nothing to read, and
/// git and the usual tools open no editor, no pager and no prompt.
#[test]
fn a_command_that_waits_for_someone_finds_nobody_and_goes_on() {
    keel_harness::needs!(sandbox);
    let w = world(&[]);
    let id = w.add();
    let run = w.run(
        &id,
        WHO_ANSWERS,
        vec![agent("builder"), check("check", WHO_ANSWERS)],
    );
    let builder = w.wait_done(&run, "builder");
    assert_eq!(builder.state, "succeeded");
    let built = w.bash("new");
    let checked = w.passed(&w.wait_done(&run, "check"));
    for (who, output) in [("builder", &built), ("check", &checked)] {
        assert_eq!(said(output, "read"), "0", "{who}: {output}");
        for (name, value) in [
            ("TERM", "dumb"),
            ("GIT_EDITOR", "true"),
            ("EDITOR", "true"),
            ("PAGER", "cat"),
            ("GIT_PAGER", "cat"),
            ("GIT_TERMINAL_PROMPT", "0"),
        ] {
            assert_eq!(said(output, name), value, "{who}: {name}");
        }
    }
}

/// A builder that goes on with its session, as when a person retries it that way, runs its commands
/// in the same home, and finds there what it left before.
#[test]
fn a_resumed_builder_runs_its_commands_in_the_same_home() {
    keel_harness::needs!(sandbox);
    let w = world(&[]);
    let id = w.add();
    std::fs::write(w.log.join("fail-first"), "").unwrap();
    let run = w.run(
        &id,
        &format!(
            "mkdir -p ~/.algo && echo once >> ~/.algo/tries && \
             echo \"tries=$(wc -l < ~/.algo/tries | tr -d ' ')\" && {WHERE}"
        ),
        vec![agent("builder")],
    );
    let first = w.wait_done(&run, "builder");
    assert_eq!(first.state, "failed", "{}", w.bash("new"));
    w.engine.retry(&first.id, true).unwrap();
    let resumed = w.wait_done(&run, "builder");
    assert_eq!(resumed.state, "succeeded");
    assert_eq!(resumed.resumed_from.as_deref(), Some(first.id.as_str()));

    let home = PathBuf::from(&resumed.workspace).join(".keel-cache/home");
    let (before, after) = (w.bash("new"), w.bash("resumed"));
    assert_eq!(said(&before, "HOME"), home.to_string_lossy(), "{before}");
    assert_eq!(said(&after, "HOME"), home.to_string_lossy(), "{after}");
    assert_eq!(said(&after, "tries"), "2", "{after}");
    assert_eq!(said(&after, "claude"), real_home());
}

/// The registries of cargo, Go, npm and pnpm, and pip and uv.
const REGISTRIES: [&str; 5] = [
    "index.crates.io",
    "proxy.golang.org",
    "registry.npmjs.org",
    "pypi.org",
    "repo.maven.apache.org",
];

/// What a command is told about the network, tool by tool: cargo, Go, npm, pnpm, pip, uv and
/// Maven.
const OFFLINE: &str = "echo \"offline=[$CARGO_NET_OFFLINE][$GOPROXY][$npm_config_offline]\
                       [$pnpm_config_offline][$PIP_NO_INDEX][$UV_OFFLINE][$MAVEN_ARGS]\"";
const ALL_OFFLINE: &str = "[true][off][true][true][1][1][--offline]";
const NONE_OFFLINE: &str = "[][][][][][][]";

/// #121: a check has no network, so every tool works offline there, even in a project whose builder
/// may reach the registries. That builder is left as it was.
#[test]
fn a_check_works_offline_even_where_the_builder_may_download() {
    keel_harness::needs!(sandbox);
    let w = world(&[]);
    let id = w.add();
    w.allow(&id, &REGISTRIES);
    let run = w.run(
        &id,
        OFFLINE,
        vec![
            agent("builder"),
            check("check", OFFLINE),
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    assert_eq!(w.wait_done(&run, "builder").state, "succeeded");
    let built = w.bash("new");
    assert_eq!(said(&built, "offline"), NONE_OFFLINE, "{built}");
    let checked = w.passed(&w.wait_done(&run, "check"));
    assert_eq!(said(&checked, "offline"), ALL_OFFLINE, "{checked}");
}

/// #121: a builder that can't reach a registry works offline with it too. Setup has the whole
/// network, and nothing tells it otherwise.
#[test]
fn a_builder_with_no_way_to_a_registry_works_offline_and_setup_never_does() {
    keel_harness::needs!(sandbox);
    let w = world(&[]);
    let id = w.add();
    let run = w.run(&id, OFFLINE, vec![agent("builder")]);
    assert_eq!(w.wait_done(&run, "builder").state, "succeeded");
    let built = w.bash("new");
    assert_eq!(said(&built, "offline"), ALL_OFFLINE, "{built}");

    let (setup, check) = w.try_check(&id, OFFLINE, OFFLINE);
    assert_eq!(said(&w.passed(&setup), "offline"), NONE_OFFLINE);
    assert_eq!(said(&w.passed(&check), "offline"), ALL_OFFLINE);
}

/// A command that needs something downloaded: the program it needs, the files it runs on, and what
/// its tool says as it fails.
struct Download {
    program: &'static str,
    files: &'static [(&'static str, &'static str)],
    command: &'static str,
    says: &'static str,
}

const PACKAGE: &str = "{ \"name\": \"probe\", \"version\": \"1.0.0\", \"private\": true }\n";

/// One for each tool of #121 that may be on a Mac, each in a folder of its own.
const DOWNLOADS: [Download; 8] = [
    Download {
        program: "npm",
        files: &[("npm/package.json", PACKAGE)],
        command: "cd npm && npm install left-pad",
        says: "ENOTCACHED",
    },
    Download {
        program: "pnpm",
        files: &[("pnpm/package.json", PACKAGE)],
        command: "cd pnpm && pnpm add left-pad",
        says: "ERR_PNPM_",
    },
    Download {
        program: "python3",
        files: &[],
        command: "python3 -m pip install --target pip-packages six",
        says: "No matching distribution found for six",
    },
    Download {
        program: "uv",
        files: &[(
            "uv/pyproject.toml",
            "[project]\nname = \"probe\"\nversion = \"0.1.0\"\nrequires-python = \">=3.9\"\n\
             dependencies = []\n",
        )],
        command: "cd uv && uv add six",
        says: "network was disabled",
    },
    // Bundler needs nothing to give up at once, and this shows it still does. Into the project, as
    // the gems of the Ruby macOS comes with are where no check may write.
    Download {
        program: "bundle",
        files: &[(
            "bundler/Gemfile",
            "source \"https://rubygems.org\"\ngem \"rainbow\", \"3.1.1\"\n",
        )],
        command: "cd bundler && BUNDLE_PATH=vendor/bundle bundle install",
        says: "Could not reach host",
    },
    Download {
        program: "cargo",
        files: &[
            (
                "cargo/Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 # Its own workspace, so cargo doesn't look for one in the folders above.\n\
                 [workspace]\n",
            ),
            ("cargo/src/lib.rs", ""),
        ],
        command: "cd cargo && cargo add memchr",
        says: "could not be found in registry index",
    },
    Download {
        program: "go",
        files: &[("go/go.mod", "module probe\n\ngo 1.22\n")],
        command: "cd go && go get golang.org/x/text@v0.20.0",
        says: "GOPROXY=off",
    },
    // #124: with an empty home it doesn't even have the plugin that would download it, so the
    // plugin is named in full: by its prefix alone Maven would give up before it looked.
    Download {
        program: "mvn",
        files: &[(
            "maven/pom.xml",
            "<project><modelVersion>4.0.0</modelVersion><groupId>p</groupId>\
             <artifactId>probe</artifactId><version>1</version></project>\n",
        )],
        command: "cd maven && mvn -q org.apache.maven.plugins:maven-dependency-plugin:3.8.1:get \
                  -Dartifact=org.apache.commons:commons-lang3:3.17.0",
        says: "in offline mode",
    },
];

fn installed(program: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap()).any(|dir| dir.join(program).is_file())
}

/// #121: inside a check, a command that would download something fails in seconds, whichever tool
/// it is, instead of retrying for a minute or more. The project lets its builder reach every
/// registry, and the check works offline all the same. A tool this Mac doesn't have is skipped.
#[test]
fn inside_a_check_a_command_that_needs_a_download_fails_in_seconds() {
    keel_harness::needs!(sandbox);
    let files: Vec<(&str, &str)> = DOWNLOADS
        .iter()
        .flat_map(|download| download.files.iter().copied())
        .collect();
    let w = world(&files);
    let id = w.add();
    w.allow(&id, &REGISTRIES);
    // Every tool is tried before any is judged, so one run says how long each took.
    let mut wrong = Vec::new();
    for Download {
        program,
        command,
        says,
        ..
    } in DOWNLOADS
    {
        if !installed(program) {
            skipped(&format!("{program} isn't installed"));
            continue;
        }
        let (setup, check) = w.try_check(&id, "true", command);
        w.passed(&setup);
        let output = w.output(&check);
        let took = w.took(&check);
        eprintln!(
            "`{command}` ended {} in {:.1}s",
            check.state,
            took.as_secs_f64()
        );
        if check.state != "failed" || !output.contains(says) || took >= Duration::from_secs(5) {
            wrong.push(format!(
                "`{command}` ended {} in {took:?}, without saying {says:?}? {output}",
                check.state
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
}
