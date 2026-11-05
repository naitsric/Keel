//! Guesses how a project checks itself and what it installs first, from the files at its root. Only
//! ever reads them: anything in a repo can run code once something runs it, and this happens the
//! moment a folder is chosen. What it finds is a guess for a person to confirm, not a setting.

use std::path::Path;

use serde::Serialize;

/// Far more than a manifest takes. Something this large isn't one.
const LARGEST_FILE: u64 = 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Guess {
    pub check: Option<String>,
    pub setup: Option<String>,
    /// The files it came from, to say so.
    pub from: Vec<String>,
}

/// What one kind of project gives away. A check is left out when nothing says how it tests itself,
/// since a wrong one stops a run after the agents are paid for.
struct Stack {
    setup: Option<&'static str>,
    check: Option<String>,
    from: Vec<&'static str>,
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
fn has_test_script(package: &str) -> bool {
    let Ok(package) = serde_json::from_str::<serde_json::Value>(package) else {
        return false;
    };
    package["scripts"]["test"]
        .as_str()
        .is_some_and(|script| !script.trim().is_empty() && !script.contains("no test specified"))
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
        setup: Some(setup),
        check: has_test_script(&package).then(|| format!("{tool} test")),
        from: vec!["package.json", lock],
    })
}

/// `--frozen` also on the check, which has no network to compare the lock with.
fn uv(root: &Path) -> Option<Stack> {
    let lock = read(root, "uv.lock")?;
    Some(Stack {
        setup: Some("uv sync --frozen"),
        check: lock
            .contains("name = \"pytest\"")
            .then(|| "uv run --frozen pytest".to_owned()),
        from: vec!["uv.lock"],
    })
}

fn rust(root: &Path) -> Option<Stack> {
    is_file(root, "Cargo.toml").then(|| Stack {
        setup: Some("cargo fetch"),
        check: Some("cargo test".to_owned()),
        from: vec!["Cargo.toml"],
    })
}

fn go(root: &Path) -> Option<Stack> {
    is_file(root, "go.mod").then(|| Stack {
        setup: Some("go mod download"),
        check: Some("go test ./...".to_owned()),
        from: vec!["go.mod"],
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
        guess.setup = stack.setup.map(str::to_owned);
        if guess.check.is_none() {
            guess.check = stack.check;
        }
        guess.from.extend(stack.from.into_iter().map(str::to_owned));
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
