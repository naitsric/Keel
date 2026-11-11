//! The home folder setup and check run in, through whole steps: a real repo, the real `sandbox-exec`
//! and no agent. Setup fills the project's own home, with the network, and each run's check works in
//! a copy of it. The real home folder stays out of reach.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_engine::{Config, Engine};
use keel_harness::Tools;
use keel_store::{Attempt, Store};

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    repo: PathBuf,
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
    // The commands find what this test finds, cargo included.
    let dirs: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap()).collect();
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    let engine = Engine::new(store.clone(), config, Tools::from_dirs(&dirs), |_| {});
    World {
        dir,
        store,
        engine,
        repo,
    }
}

impl World {
    fn add(&self) -> String {
        self.engine.add_project(&self.repo).unwrap().id
    }

    /// Setup and then the check, as the setup dialog tries them. Both of them, as they ended.
    fn try_check(&self, id: &str, setup: &str, check: &str) -> (Attempt, Attempt) {
        let run = self.engine.try_check(id, setup, check, "en").unwrap();
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
