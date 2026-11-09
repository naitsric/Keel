//! Guesses how a project checks itself and what it installs first, from the files at its root. Only
//! ever reads them: anything in a repo can run code once something runs it, and this happens the
//! moment a folder is chosen. What it finds is a guess for a person to confirm, not a setting.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use toml::Value;

/// Far more than a manifest takes. Something this large isn't one.
const LARGEST_FILE: u64 = 1024 * 1024;
/// A repo decides how many of these there are, and a person has to read them.
const MOST_OPTIONS: usize = 12;
const LONGEST_NAME: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Guess {
    pub check: Option<String>,
    pub setup: Option<String>,
    /// The files it came from, to say so.
    pub from: Vec<String>,
    /// What else the setup command can be told to install, for a person to add.
    pub setup_options: Vec<String>,
    /// Other commands that check something here, for a person to add.
    pub check_options: Vec<String>,
}

/// What one kind of project gives away. A check is left out when nothing says how it tests itself,
/// since a wrong one stops a run after the agents are paid for.
#[derive(Default)]
struct Stack {
    setup: Option<String>,
    check: Option<String>,
    from: Vec<&'static str>,
    setup_options: Vec<String>,
    check_options: Vec<String>,
}

/// Regular files only, so a link or a pipe in the repo can't make this hang or read elsewhere.
fn is_file(root: &Path, name: &str) -> bool {
    root.join(name)
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_file())
}

fn read(root: &Path, name: &str) -> Option<String> {
    let path = root.join(name);
    let meta = path.symlink_metadata().ok()?;
    (meta.is_file() && meta.len() <= LARGEST_FILE)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
}

fn has_test_target(makefile: &str) -> bool {
    makefile.lines().any(|line| {
        if line.starts_with(['\t', ' ', '#']) {
            return false;
        }
        let Some((targets, rest)) = line.split_once(':') else {
            return false;
        };
        // `test := 1` and `x = test: 1` set a variable.
        !targets.contains('=')
            && !rest.starts_with('=')
            && !rest.starts_with(":=")
            && targets.split_whitespace().any(|target| target == "test")
    })
}

/// A test script `npm init` wrote is a placeholder that always fails.
fn is_real_script(script: &str) -> bool {
    !script.trim().is_empty() && !script.contains("no test specified")
}

fn has_test_script(package: &str) -> bool {
    let Ok(package) = serde_json::from_str::<serde_json::Value>(package) else {
        return false;
    };
    package["scripts"]["test"]
        .as_str()
        .is_some_and(is_real_script)
}

/// Going by its name. `dev` and `test:watch` never end, and a check that never ends holds a run until
/// someone stops it. `lint:fix` changes what it was meant to check.
fn reads_as_a_check(name: &str) -> bool {
    const CHECKS: [&str; 12] = [
        "test",
        "tests",
        "spec",
        "e2e",
        "lint",
        "typecheck",
        "types",
        "tsc",
        "check",
        "verify",
        "validate",
        "ci",
    ];
    const NOT_CHECKS: [&str; 9] = [
        "watch", "dev", "ui", "serve", "start", "open", "debug", "fix", "update",
    ];
    let name = name.to_ascii_lowercase();
    let words: Vec<&str> = name.split([':', '-', '_', '.']).collect();
    words.iter().any(|word| CHECKS.contains(word))
        && !words.iter().any(|word| NOT_CHECKS.contains(word))
}

fn script_checks(tool: &str, package: &str) -> Vec<String> {
    let Ok(package) = serde_json::from_str::<serde_json::Value>(package) else {
        return Vec::new();
    };
    let Some(scripts) = package["scripts"].as_object() else {
        return Vec::new();
    };
    let mut names: Vec<&String> = scripts
        .iter()
        .filter(|(name, script)| {
            // A script's name can also have colons.
            is_plain_name(&name.replace(':', "-"))
                && reads_as_a_check(name)
                && script.as_str().is_some_and(is_real_script)
        })
        .map(|(name, _)| name)
        .collect();
    names.sort();
    names
        .into_iter()
        .take(MOST_OPTIONS)
        .map(|name| match name.as_str() {
            "test" => format!("{tool} test"),
            name => format!("{tool} run {name}"),
        })
        .collect()
}

/// Yarn 2 and later need a global folder that's in the home folder. Nobody has tried them in the
/// sandbox, so a repo with a `.yarnrc.yml` gets no guess.
fn javascript(root: &Path) -> Option<Stack> {
    let package = read(root, "package.json")?;
    let (tool, setup, lock) = if is_file(root, "pnpm-lock.yaml") {
        ("pnpm", "pnpm install --frozen-lockfile", "pnpm-lock.yaml")
    } else if is_file(root, "yarn.lock") && !is_file(root, ".yarnrc.yml") {
        ("yarn", "yarn install --frozen-lockfile", "yarn.lock")
    } else if is_file(root, "package-lock.json") {
        ("npm", "npm ci", "package-lock.json")
    } else {
        return None;
    };
    Some(Stack {
        setup: Some(setup.to_owned()),
        check: has_test_script(&package).then(|| format!("{tool} test")),
        from: vec!["package.json", lock],
        check_options: script_checks(tool, &package),
        ..Stack::default()
    })
}

fn normalized(name: &str) -> String {
    name.to_ascii_lowercase().replace(['_', '.'], "-")
}

/// `pytest~=9.1`, `pytest[psutil]` or `pytest ; python_version < "3.13"`, but not `pytest-cov`.
fn is_pytest(requirement: &str) -> bool {
    let name = requirement
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric() && !"-_.".contains(c))
        .next()
        .unwrap_or_default();
    normalized(name) == "pytest"
}

fn names_pytest(requirements: &Value) -> bool {
    requirements
        .as_array()
        .is_some_and(|list| list.iter().any(|r| r.as_str().is_some_and(is_pytest)))
}

/// Without recursion: a group can include another, and a repo can make that chain as long as it likes.
fn any_reaches_pytest(groups: &HashMap<String, &Value>, mut waiting: Vec<String>) -> bool {
    let mut seen = HashSet::new();
    while let Some(name) = waiting.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let entries = groups.get(&name).and_then(|group| group.as_array());
        for entry in entries.into_iter().flatten() {
            match entry {
                Value::String(requirement) if is_pytest(requirement) => return true,
                Value::Table(include) => waiting.extend(
                    include
                        .get("include-group")
                        .and_then(Value::as_str)
                        .map(normalized),
                ),
                _ => {}
            }
        }
    }
    false
}

/// This name ends up in a command, so one a shell or uv could read as anything else doesn't count.
fn is_plain_name(name: &str) -> bool {
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    name.len() <= LONGEST_NAME
        && edge(name.chars().next())
        && edge(name.chars().last())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// What `uv sync` can be told to install beyond what it installs by itself: the project's dependencies
/// and, unless the project says otherwise, the `dev` group. And which of those it has to be told for
/// pytest to be there, when pytest wouldn't come by itself.
fn uv_options(pyproject: &str) -> (Vec<String>, Option<String>) {
    let Ok(pyproject) = toml::from_str::<toml::Table>(pyproject) else {
        return (Vec::new(), None);
    };
    let project = pyproject.get("project");
    let uv = pyproject.get("tool").and_then(|tool| tool.get("uv"));
    let group_table = pyproject.get("dependency-groups").and_then(Value::as_table);
    let groups: HashMap<String, &Value> = group_table
        .into_iter()
        .flatten()
        .map(|(name, group)| (normalized(name), group))
        .collect();
    let default_groups: Vec<String> = match uv.and_then(|uv| uv.get("default-groups")) {
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .map(normalized)
            .collect(),
        // The only other thing it can be is "all".
        Some(_) => groups.keys().cloned().collect(),
        None => vec!["dev".to_owned()],
    };
    let installed = |list: Option<&Value>| list.is_some_and(names_pytest);
    let pytest_comes_by_itself = installed(project.and_then(|p| p.get("dependencies")))
        || installed(uv.and_then(|uv| uv.get("dev-dependencies")))
        || any_reaches_pytest(&groups, default_groups.clone());
    let extras = project
        .and_then(|p| p.get("optional-dependencies"))
        .and_then(Value::as_table);
    let offered: Vec<(String, &Value)> = group_table
        .into_iter()
        .flatten()
        .filter(|(name, _)| !default_groups.contains(&normalized(name)))
        .filter(|(name, _)| is_plain_name(name))
        .map(|(name, list)| (format!("--group {name}"), list))
        .chain(
            extras
                .into_iter()
                .flatten()
                .filter(|(name, _)| is_plain_name(name))
                .map(|(name, list)| (format!("--extra {name}"), list)),
        )
        .collect();
    let for_pytest = offered
        .iter()
        .find(|(_, list)| !pytest_comes_by_itself && names_pytest(list))
        .map(|(option, _)| option.clone());
    let options = offered
        .into_iter()
        .take(MOST_OPTIONS)
        .map(|(option, _)| option)
        .collect();
    (options, for_pytest)
}

/// `--frozen` also on the check, which has no network to compare the lock with. A check needs pytest
/// to be installed, or it fails after the agents are paid for, but only setup can install it.
fn uv(root: &Path) -> Option<Stack> {
    let lock = read(root, "uv.lock")?;
    let has_pytest = lock.contains("name = \"pytest\"");
    let (setup_options, for_pytest) = read(root, "pyproject.toml")
        .map(|pyproject| uv_options(&pyproject))
        .unwrap_or_default();
    let mut from = vec!["uv.lock"];
    if !setup_options.is_empty() {
        from.push("pyproject.toml");
    }
    Some(Stack {
        setup: Some(match for_pytest.filter(|_| has_pytest) {
            Some(option) => format!("uv sync --frozen {option}"),
            None => "uv sync --frozen".to_owned(),
        }),
        check: has_pytest.then(|| "uv run --frozen pytest".to_owned()),
        from,
        setup_options,
        ..Stack::default()
    })
}

fn rust(root: &Path) -> Option<Stack> {
    is_file(root, "Cargo.toml").then(|| Stack {
        setup: Some("cargo fetch".to_owned()),
        check: Some("cargo test".to_owned()),
        from: vec!["Cargo.toml"],
        ..Stack::default()
    })
}

fn go(root: &Path) -> Option<Stack> {
    is_file(root, "go.mod").then(|| Stack {
        setup: Some("go mod download".to_owned()),
        check: Some("go test ./...".to_owned()),
        from: vec!["go.mod"],
        ..Stack::default()
    })
}

/// A Makefile's `test` target is what the project itself calls testing, so it wins. What to install
/// comes from the first stack found. An unknown stack gets nothing rather than something plausible.
pub fn guess(root: &Path) -> Guess {
    let mut guess = Guess::default();
    let makefile = ["Makefile", "GNUmakefile", "makefile"]
        .into_iter()
        .find(|name| read(root, name).is_some_and(|text| has_test_target(&text)));
    if let Some(name) = makefile {
        guess.check = Some("make test".to_owned());
        guess.from.push(name.to_owned());
    }
    let stack = [javascript, uv, rust, go]
        .iter()
        .find_map(|find| find(root));
    if let Some(stack) = stack {
        guess.setup = stack.setup;
        if guess.check.is_none() {
            guess.check = stack.check;
        }
        guess.from.extend(stack.from.into_iter().map(str::to_owned));
        guess.setup_options = stack.setup_options;
        guess.check_options = stack.check_options;
    }
    guess
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_TEST: &str = r#"{"scripts":{"test":"vitest run"}}"#;

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    fn found(files: &[(&str, &str)]) -> (Option<String>, Option<String>) {
        let guess = guess(repo(files).path());
        (guess.check, guess.setup)
    }

    fn both(check: &str, setup: &str) -> (Option<String>, Option<String>) {
        (Some(check.to_owned()), Some(setup.to_owned()))
    }

    #[test]
    fn each_lockfile_names_its_tool() {
        assert_eq!(
            found(&[("package.json", WITH_TEST), ("pnpm-lock.yaml", "")]),
            both("pnpm test", "pnpm install --frozen-lockfile")
        );
        assert_eq!(
            found(&[("package.json", WITH_TEST), ("package-lock.json", "{}")]),
            both("npm test", "npm ci")
        );
        assert_eq!(
            found(&[("package.json", WITH_TEST), ("yarn.lock", "")]),
            both("yarn test", "yarn install --frozen-lockfile")
        );
    }

    #[test]
    fn a_package_without_a_lockfile_says_nothing_about_its_tool() {
        assert_eq!(found(&[("package.json", WITH_TEST)]), (None, None));
    }

    #[test]
    fn yarn_with_its_own_config_is_not_guessed() {
        assert_eq!(
            found(&[
                ("package.json", WITH_TEST),
                ("yarn.lock", ""),
                (".yarnrc.yml", "")
            ]),
            (None, None)
        );
    }

    #[test]
    fn the_placeholder_from_npm_init_is_no_test_at_all() {
        let placeholder = r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#;
        assert_eq!(
            found(&[("package.json", placeholder), ("package-lock.json", "{}")]),
            (None, Some("npm ci".to_owned()))
        );
        assert_eq!(
            found(&[("package.json", "{}"), ("package-lock.json", "{}")]),
            (None, Some("npm ci".to_owned()))
        );
        assert_eq!(
            found(&[("package.json", "not json"), ("package-lock.json", "{}")]),
            (None, Some("npm ci".to_owned()))
        );
    }

    #[test]
    fn a_makefile_counts_only_with_a_test_target() {
        for makefile in [
            "test:\n\tpytest\n",
            "test: deps\n\tpytest\n",
            "lint test:\n\tpytest\n",
            ".PHONY: test\nall:\n\ttrue\ntest::\n\ttrue\n",
        ] {
            assert_eq!(
                found(&[("Makefile", makefile)]).0.as_deref(),
                Some("make test"),
                "{makefile}"
            );
        }
        for makefile in [
            "all:\n\ttrue\n",
            ".PHONY: test\nall:\n\ttrue\n",
            "test := 1\n",
            "test ::= 1\n",
            "x = test: 1\n",
            "# test:\n",
            "\ttest: not a target\n",
            "testing:\n\ttrue\n",
        ] {
            assert_eq!(found(&[("Makefile", makefile)]).0, None, "{makefile}");
        }
    }

    #[test]
    fn a_makefile_wins_and_the_lockfile_still_says_what_to_install() {
        let guess = guess(
            repo(&[
                ("Makefile", "test:\n\ttrue\n"),
                ("uv.lock", "name = \"pytest\"\n"),
            ])
            .path(),
        );
        assert_eq!(guess.check.as_deref(), Some("make test"));
        assert_eq!(guess.setup.as_deref(), Some("uv sync --frozen"));
        assert_eq!(guess.from, ["Makefile", "uv.lock"]);
    }

    #[test]
    fn uv_only_claims_a_test_runner_it_can_see() {
        assert_eq!(
            found(&[("uv.lock", "[[package]]\nname = \"pytest\"\n")]),
            both("uv run --frozen pytest", "uv sync --frozen")
        );
        assert_eq!(
            found(&[("uv.lock", "[[package]]\nname = \"pytest-cov\"\n")]),
            (None, Some("uv sync --frozen".to_owned()))
        );
    }

    const LOCK_WITH_PYTEST: &str = "[[package]]\nname = \"pytest\"\n";

    fn uv_setup(pyproject: &str) -> String {
        found(&[("uv.lock", LOCK_WITH_PYTEST), ("pyproject.toml", pyproject)])
            .1
            .unwrap()
    }

    #[test]
    fn uv_installs_the_group_with_pytest_when_it_would_not_come_by_default() {
        // The shape of a real project, whose check failed with `Failed to spawn: pytest`.
        let pyproject = r#"
            [project]
            name = "app"
            dependencies = ["fastapi"]

            [dependency-groups]
            notebook = ["ipykernel"]
            stubs = ["ruff"]
            test = ["faker", "pytest~=9.1.0", "pytest-cov"]
            test-uv = [{ include-group = "test" }, "tox-uv"]
        "#;
        let guess =
            guess(repo(&[("uv.lock", LOCK_WITH_PYTEST), ("pyproject.toml", pyproject)]).path());
        assert_eq!(
            guess.setup.as_deref(),
            Some("uv sync --frozen --group test")
        );
        assert_eq!(guess.check.as_deref(), Some("uv run --frozen pytest"));
        assert_eq!(guess.from, ["uv.lock", "pyproject.toml"]);
    }

    #[test]
    fn uv_is_told_nothing_more_when_pytest_comes_by_default() {
        for pyproject in [
            "[dependency-groups]\ndev = [\"pytest>=8\"]\n",
            "[dependency-groups]\ndev = [\"PyTest[psutil] ; python_version < '3.14'\"]\n",
            "[dependency-groups]\ndev = [{ include-group = \"test\" }]\ntest = [\"pytest\"]\n",
            "[dependency-groups]\nDev = [{ include-group = \"my_tests\" }]\nmy-tests = [\"pytest\"]\n",
            "[project]\ndependencies = [\"pytest\"]\n",
            "[tool.uv]\ndev-dependencies = [\"pytest\"]\n",
            "[tool.uv]\ndefault-groups = [\"test\"]\n[dependency-groups]\ntest = [\"pytest\"]\n",
            "[tool.uv]\ndefault-groups = \"all\"\n[dependency-groups]\ntest = [\"pytest\"]\n",
        ] {
            assert_eq!(uv_setup(pyproject), "uv sync --frozen", "{pyproject}");
        }
    }

    #[test]
    fn uv_reads_extras_and_a_dev_group_that_was_taken_off_the_defaults() {
        assert_eq!(
            uv_setup("[project.optional-dependencies]\ntest = [\"pytest\"]\n"),
            "uv sync --frozen --extra test"
        );
        assert_eq!(
            uv_setup("[tool.uv]\ndefault-groups = []\n[dependency-groups]\ndev = [\"pytest\"]\n"),
            "uv sync --frozen --group dev"
        );
    }

    #[test]
    fn a_group_name_cannot_put_anything_else_in_the_command() {
        for name in [
            "x; touch /tmp/owned",
            "$(touch /tmp/owned)",
            "--all-packages",
            "a b",
            "x\\n",
            "",
        ] {
            let pyproject = format!("[dependency-groups]\n\"{name}\" = [\"pytest\"]\n");
            assert_eq!(uv_setup(&pyproject), "uv sync --frozen", "{name}");
            let found = guess(
                repo(&[
                    ("uv.lock", LOCK_WITH_PYTEST),
                    ("pyproject.toml", &pyproject),
                ])
                .path(),
            );
            assert_eq!(found.setup_options, Vec::<String>::new(), "{name}");
            // And it doesn't hide a group that's fine.
            let pyproject = format!("{pyproject}test = [\"pytest\"]\n");
            assert_eq!(
                uv_setup(&pyproject),
                "uv sync --frozen --group test",
                "{name}"
            );
        }
    }

    #[test]
    fn uv_keeps_the_plain_guess_when_the_project_file_does_not_help() {
        for pyproject in [
            "not toml [[",
            "",
            "[dependency-groups]\ntest = [\"pytest-cov\"]\n",
        ] {
            assert_eq!(uv_setup(pyproject), "uv sync --frozen", "{pyproject}");
        }
        // Without pytest in the lock, naming it wouldn't install it.
        assert_eq!(
            found(&[
                ("uv.lock", ""),
                (
                    "pyproject.toml",
                    "[dependency-groups]\ntest = [\"pytest\"]\n"
                )
            ]),
            (None, Some("uv sync --frozen".to_owned()))
        );
    }

    #[test]
    fn uv_offers_everything_it_would_not_install_by_itself() {
        let pyproject = r#"
            [project.optional-dependencies]
            docs = ["sphinx"]

            [dependency-groups]
            dev = ["ruff"]
            stubs = ["pyright"]
            test = ["pytest"]
        "#;
        let found =
            guess(repo(&[("uv.lock", LOCK_WITH_PYTEST), ("pyproject.toml", pyproject)]).path());
        // The one for pytest too: a project that's already saved may not have it.
        assert_eq!(
            found.setup_options,
            ["--group stubs", "--group test", "--extra docs"]
        );
        assert_eq!(found.check_options, Vec::<String>::new());

        let all = format!("[tool.uv]\ndefault-groups = \"all\"\n{pyproject}");
        let found = guess(repo(&[("uv.lock", LOCK_WITH_PYTEST), ("pyproject.toml", &all)]).path());
        assert_eq!(found.setup_options, ["--extra docs"]);

        // Without pytest in the lock nothing is guessed, and what's there is still on offer.
        let found = guess(repo(&[("uv.lock", ""), ("pyproject.toml", pyproject)]).path());
        assert_eq!(found.setup.as_deref(), Some("uv sync --frozen"));
        assert_eq!(found.setup_options.len(), 3);
        assert_eq!(found.from, ["uv.lock", "pyproject.toml"]);
    }

    #[test]
    fn a_repo_cannot_flood_the_list_of_options() {
        let mut groups = String::from("[dependency-groups]\n");
        let mut scripts = serde_json::Map::new();
        for n in 0..100 {
            groups.push_str(&format!("g{n} = [\"x\"]\n"));
            scripts.insert(format!("test:{n}"), "true".into());
        }
        let long = "g".repeat(LONGEST_NAME + 1);
        groups.push_str(&format!("{long} = [\"x\"]\n"));
        let found = guess(repo(&[("uv.lock", ""), ("pyproject.toml", &groups)]).path());
        assert_eq!(found.setup_options.len(), MOST_OPTIONS);
        assert!(!found.setup_options.iter().any(|o| o.contains(&long)));

        let package = serde_json::json!({ "scripts": scripts }).to_string();
        let found = guess(repo(&[("package.json", &package), ("pnpm-lock.yaml", "")]).path());
        assert_eq!(found.check_options.len(), MOST_OPTIONS);
    }

    #[test]
    fn scripts_that_check_something_are_on_offer_and_the_rest_are_not() {
        let package = r#"{"scripts":{
            "test": "vitest run",
            "typecheck": "tsc --noEmit",
            "check:i18n": "node check.mjs",
            "lint": "eslint .",
            "format:check": "prettier --check .",
            "dev": "vite",
            "build": "vite build",
            "tauri": "tauri",
            "pretest": "true",
            "test:watch": "vitest",
            "test:ui": "vitest --ui",
            "lint:fix": "eslint --fix .",
            "check:empty": "",
            "verify": 3,
            "test; touch /tmp/owned": "true",
            "--check": "true",
            "@scope/test": "true"
        }}"#;
        let found = guess(repo(&[("package.json", package), ("pnpm-lock.yaml", "")]).path());
        assert_eq!(found.check.as_deref(), Some("pnpm test"));
        assert_eq!(
            found.check_options,
            [
                "pnpm run check:i18n",
                "pnpm run format:check",
                "pnpm run lint",
                "pnpm test",
                "pnpm run typecheck"
            ]
        );
        assert_eq!(found.setup_options, Vec::<String>::new());

        let npm = guess(repo(&[("package.json", package), ("package-lock.json", "{}")]).path());
        assert!(npm.check_options.contains(&"npm run lint".to_owned()));
        for broken in ["not json", "[]", r#"{"scripts":[]}"#, "{}"] {
            let found = guess(repo(&[("package.json", broken), ("pnpm-lock.yaml", "")]).path());
            assert_eq!(found.check_options, Vec::<String>::new(), "{broken}");
        }
    }

    #[test]
    fn the_placeholder_test_script_is_not_on_offer() {
        let package = r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#;
        let found = guess(repo(&[("package.json", package), ("pnpm-lock.yaml", "")]).path());
        assert_eq!(found.check_options, Vec::<String>::new());
    }

    #[test]
    fn groups_that_include_each_other_cannot_hang_or_overflow_it() {
        assert_eq!(
            uv_setup(
                "[dependency-groups]\n\
                 dev = [{ include-group = \"a\" }]\n\
                 a = [{ include-group = \"dev\" }]\n\
                 test = [\"pytest\"]\n"
            ),
            "uv sync --frozen --group test"
        );
        // As long a chain as fits in the largest file it reads.
        let mut chain = String::from("[dependency-groups]\ndev = [{ include-group = \"g0\" }]\n");
        for n in 0..20_000 {
            chain.push_str(&format!("g{n} = [{{ include-group = \"g{}\" }}]\n", n + 1));
        }
        chain.push_str("g20000 = [\"pytest\"]\n");
        assert!(chain.len() < LARGEST_FILE as usize);
        assert_eq!(uv_setup(&chain), "uv sync --frozen");
    }

    #[test]
    fn rust_and_go_are_recognized() {
        assert_eq!(
            found(&[("Cargo.toml", "")]),
            both("cargo test", "cargo fetch")
        );
        assert_eq!(
            found(&[("go.mod", "")]),
            both("go test ./...", "go mod download")
        );
    }

    #[test]
    fn an_unknown_stack_gets_nothing() {
        assert_eq!(guess(repo(&[("README.md", "hi")]).path()), Guess::default());
        assert_eq!(guess(repo(&[]).path()), Guess::default());
    }

    #[test]
    fn it_says_what_it_read() {
        let guess = guess(repo(&[("package.json", WITH_TEST), ("pnpm-lock.yaml", "")]).path());
        assert_eq!(guess.from, ["package.json", "pnpm-lock.yaml"]);
    }

    #[test]
    fn nothing_in_the_repo_runs_while_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let canary = dir.path().join("ran");
        let script = format!("touch {}", canary.display());
        std::fs::write(
            dir.path().join("Makefile"),
            format!("$(shell {script})\ntest:\n\t{script}\n"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            format!(r#"{{"scripts":{{"test":"{script}","preinstall":"{script}"}}}}"#),
        )
        .unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        let found = guess(dir.path());
        assert_eq!(found.check.as_deref(), Some("make test"));
        assert!(!canary.exists());
    }

    #[test]
    fn links_and_huge_files_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = outside.path().join("Makefile");
        std::fs::write(&elsewhere, "test:\n\ttrue\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("Makefile")).unwrap();
        assert_eq!(guess(dir.path()), Guess::default());

        let huge = format!("test:\n{}", "#\n".repeat(LARGEST_FILE as usize));
        assert_eq!(guess(repo(&[("Makefile", &huge)]).path()), Guess::default());
    }

    #[test]
    fn a_folder_that_is_gone_gets_nothing() {
        assert_eq!(guess(Path::new("/nowhere/at/all")), Guess::default());
    }
}
