//! Guesses how a project checks itself and what it installs first, from the files at its root. Only
//! ever reads them: anything in a repo can run code once something runs it, and this happens the
//! moment a folder is chosen. What it finds is a guess for a person to confirm, not a setting.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use toml::Value;

mod calls;
mod ci;

/// Far more than a manifest takes. Something this large isn't one.
const LARGEST_FILE: u64 = 1024 * 1024;
/// A repo decides how many of these there are, and a person has to read them.
const MOST_OPTIONS: usize = 12;
const LONGEST_NAME: usize = 64;
/// How many names in one folder are looked at, in a folder that has no end of them.
const MOST_ENTRIES: usize = 4096;

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
    /// It's what the project's CI runs, since nothing else here says how it installs and tests.
    pub from_ci: bool,
    /// What the check runs because the project's CI does, though nothing else here names it, as
    /// Keel runs it with no network.
    pub ci_checks: Vec<String>,
    /// What else the CI checks, as it runs it, that Keel doesn't know how to run with no network.
    pub ci_left_out: Vec<String>,
    /// What else the CI checks, as it runs it, though it lets it fail, so the check doesn't require it.
    pub ci_may_fail: Vec<String>,
}

/// What a project's CI checks and its saved check doesn't run.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CiGap {
    /// As Keel runs them, with no network, for a person to add.
    pub commands: Vec<String>,
    /// As the CI runs them, since Keel doesn't know how to run them with no network.
    pub left_out: Vec<String>,
    /// As the CI runs them, though it lets them fail, so a check needn't run them.
    pub may_fail: Vec<String>,
    /// The CI's files they're in.
    pub from: Vec<String>,
}

/// Only reads the repo, like `guess`.
pub fn ci_gap(root: &Path, check: &str) -> CiGap {
    let missing = ci::missing_from(root, check);
    CiGap {
        commands: missing.add,
        left_out: missing.left_out,
        may_fail: missing.may_fail,
        from: missing.from,
    }
}

/// What one kind of project gives away. A check is left out when nothing says how it tests itself,
/// since a wrong one stops a run after the agents are paid for.
#[derive(Default)]
struct Stack {
    setup: Option<String>,
    check: Option<String>,
    from: Vec<String>,
    setup_options: Vec<String>,
    check_options: Vec<String>,
}

/// Regular files only, so a link or a pipe in the repo can't make this hang or read elsewhere.
fn is_file(root: &Path, name: &str) -> bool {
    root.join(name)
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_file())
}

/// A real folder, not a link to one somewhere else.
fn is_dir(root: &Path, name: &str) -> bool {
    root.join(name)
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_dir())
}

fn read(root: &Path, name: &str) -> Option<String> {
    let path = root.join(name);
    let meta = path.symlink_metadata().ok()?;
    (meta.is_file() && meta.len() <= LARGEST_FILE)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
}

/// The regular files in a folder, by name.
fn file_names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .take(MOST_ENTRIES)
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// An alias repeats what its anchor holds, so a few lines of them can grow into more than fits in
/// memory. A `*` where a value starts is one, and that's enough to leave the file unread.
fn has_alias(text: &str) -> bool {
    text.lines().any(|line| {
        let mut before = None;
        line.chars().any(|c| {
            let alias = c == '*' && before.is_none_or(|b| ":-[{,?".contains(b));
            if !c.is_whitespace() {
                before = Some(c);
            }
            alias
        })
    })
}

fn yaml(text: &str) -> Option<serde_yaml_ng::Value> {
    if has_alias(text) {
        return None;
    }
    serde_yaml_ng::from_str(text).ok()
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

/// A project's scripts, or Deno's tasks, by name.
type Scripts = serde_json::Map<String, serde_json::Value>;

fn scripts(config: &str, key: &str) -> Scripts {
    serde_json::from_str::<serde_json::Value>(config)
        .ok()
        .and_then(|config| config.get(key)?.as_object().cloned())
        .unwrap_or_default()
}

/// A test script `npm init` wrote is a placeholder that always fails.
fn is_real_script(script: &str) -> bool {
    !script.trim().is_empty() && !script.contains("no test specified")
}

fn is_real(scripts: &Scripts, name: &str) -> bool {
    scripts
        .get(name)
        .and_then(serde_json::Value::as_str)
        .is_some_and(is_real_script)
}

/// Words in a name that say it checks something.
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
/// `dev` and `test:watch` never end, and a check that never ends holds a run until someone stops
/// it. `lint:fix` changes what it was meant to check.
const NOT_CHECKS: [&str; 9] = [
    "watch", "dev", "ui", "serve", "start", "open", "debug", "fix", "update",
];

/// Going by its name.
fn reads_as_a_check(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let words: Vec<&str> = name.split([':', '-', '_', '.']).collect();
    words.iter().any(|word| CHECKS.contains(word))
        && !words.iter().any(|word| NOT_CHECKS.contains(word))
}

fn script_checks(scripts: &Scripts, run: &impl Fn(&str) -> String) -> Vec<String> {
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
        .map(|name| run(name))
        .collect()
}

/// The script that type-checks, which is quick and catches what the tests don't. Lint, e2e and the
/// rest stay as options, for being slow or fragile.
fn typecheck_script(scripts: &Scripts) -> Option<&'static str> {
    ["typecheck", "types", "tsc"]
        .into_iter()
        .find(|name| is_real(scripts, name))
}

/// The check, and the scripts that check something for a person to add, given how the tool runs one.
fn script_stack(scripts: &Scripts, run: impl Fn(&str) -> String) -> (Option<String>, Vec<String>) {
    let mut options = script_checks(scripts, &run);
    // One that runs every script that checks something, for a person to pick.
    if options.len() > 1 {
        options.push(options.join(" && "));
    }
    let check = is_real(scripts, "test").then(|| match typecheck_script(scripts) {
        Some(name) => format!("{} && {}", run(name), run("test")),
        None => run("test"),
    });
    (check, options)
}

/// The tool a JavaScript project installs with, how, and the lock that says so. Yarn 2 and later
/// need a global folder that's in the home folder. Nobody has tried them in the sandbox, so a repo
/// with a `.yarnrc.yml` gets no guess.
fn node_tool(root: &Path) -> Option<(&'static str, &'static str, &'static str)> {
    if is_file(root, "pnpm-lock.yaml") {
        Some(("pnpm", "pnpm install --frozen-lockfile", "pnpm-lock.yaml"))
    } else if is_file(root, "yarn.lock") && !is_file(root, ".yarnrc.yml") {
        Some(("yarn", "yarn install --frozen-lockfile", "yarn.lock"))
    } else if is_file(root, "package-lock.json") {
        Some(("npm", "npm ci", "package-lock.json"))
    } else {
        let lock = ["bun.lock", "bun.lockb"]
            .into_iter()
            .find(|name| is_file(root, name))?;
        Some(("bun", "bun install --frozen-lockfile", lock))
    }
}

/// How that tool runs one of the project's scripts.
fn run_script(tool: &str, name: &str) -> String {
    match (tool, name) {
        // `bun test` is Bun's own runner, which doesn't run the script.
        ("bun", _) => format!("bun run {name}"),
        (_, "test") => format!("{tool} test"),
        _ => format!("{tool} run {name}"),
    }
}

fn javascript(root: &Path) -> Option<Stack> {
    let package = read(root, "package.json")?;
    let (tool, setup, lock) = node_tool(root)?;
    let (check, check_options) =
        script_stack(&scripts(&package, "scripts"), |name| run_script(tool, name));
    Some(Stack {
        setup: Some(setup.to_owned()),
        check,
        from: vec!["package.json".to_owned(), lock.to_owned()],
        check_options,
        ..Stack::default()
    })
}

/// Deno has a runner of its own, but a bare `deno test` fails where there are no tests, or where they
/// need permissions. A `test` task is how a project says it. JSON with comments doesn't parse, and
/// then there are no tasks to go by.
fn deno(root: &Path) -> Option<Stack> {
    let (name, config) = ["deno.json", "deno.jsonc"]
        .into_iter()
        .find_map(|name| Some((name, read(root, name)?)))?;
    // A task can also be `{ "command": … }`.
    let tasks: Scripts = scripts(&config, "tasks")
        .into_iter()
        .filter_map(|(task, value)| {
            let command = value.as_str().or_else(|| value["command"].as_str())?;
            Some((task, command.into()))
        })
        .collect();
    let (check, check_options) = script_stack(&tasks, |task| format!("deno task {task}"));
    Some(Stack {
        setup: Some("deno install".to_owned()),
        check,
        from: vec![name.to_owned()],
        check_options,
        ..Stack::default()
    })
}

fn normalized(name: &str) -> String {
    name.to_ascii_lowercase().replace(['_', '.'], "-")
}

/// `pytest` in `pytest~=9.1`, `pytest[psutil]` or `pytest ; python_version < "3.13"`.
fn requirement_name(requirement: &str) -> String {
    let name = requirement
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric() && !"-_.".contains(c))
        .next()
        .unwrap_or_default();
    normalized(name)
}

/// But not `pytest-cov`.
fn is_pytest(requirement: &str) -> bool {
    requirement_name(requirement) == "pytest"
}

fn names_pytest(requirements: &Value) -> bool {
    requirements
        .as_array()
        .is_some_and(|list| list.iter().any(|r| r.as_str().is_some_and(is_pytest)))
}

/// Every package these groups name, and the groups they include. Without recursion: a group can
/// include another, and a repo can make that chain as long as it likes.
fn group_names(groups: &HashMap<String, &Value>, mut waiting: Vec<String>) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut names = HashSet::new();
    while let Some(name) = waiting.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let entries = groups.get(&name).and_then(|group| group.as_array());
        for entry in entries.into_iter().flatten() {
            match entry {
                Value::String(requirement) => {
                    names.insert(requirement_name(requirement));
                }
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
    names
}

fn any_reaches_pytest(groups: &HashMap<String, &Value>, waiting: Vec<String>) -> bool {
    group_names(groups, waiting).contains("pytest")
}

/// A pyproject's dependency groups by name, and those `uv sync` installs unless told otherwise:
/// `dev`, or what `[tool.uv] default-groups` says.
fn uv_groups(pyproject: &toml::Table) -> (HashMap<String, &Value>, Vec<String>) {
    let uv = pyproject.get("tool").and_then(|tool| tool.get("uv"));
    let groups: HashMap<String, &Value> = pyproject
        .get("dependency-groups")
        .and_then(Value::as_table)
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
    (groups, default_groups)
}

/// What a plain `uv sync` installs, by name: the project's dependencies, its old dev-dependencies
/// and its default groups.
fn uv_installs(pyproject: &str) -> HashSet<String> {
    let Ok(pyproject) = toml::from_str::<toml::Table>(pyproject) else {
        return HashSet::new();
    };
    let listed = |list: Option<&Value>| -> Vec<String> {
        list.and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(requirement_name)
            .collect()
    };
    let project = pyproject.get("project");
    let uv = pyproject.get("tool").and_then(|tool| tool.get("uv"));
    let (groups, default_groups) = uv_groups(&pyproject);
    let mut names = group_names(&groups, default_groups);
    names.extend(listed(project.and_then(|p| p.get("dependencies"))));
    names.extend(listed(uv.and_then(|uv| uv.get("dev-dependencies"))));
    names
}

/// The groups a plain `uv sync` installs, by name.
fn uv_default_groups(pyproject: &str) -> HashSet<String> {
    let Ok(pyproject) = toml::from_str::<toml::Table>(pyproject) else {
        return HashSet::new();
    };
    let (groups, default_groups) = uv_groups(&pyproject);
    default_groups
        .into_iter()
        .filter(|name| groups.contains_key(name))
        .collect()
}

/// What `poetry install` installs, by name: every group that isn't optional.
fn poetry_installs(pyproject: &str) -> HashSet<String> {
    let Ok(pyproject) = toml::from_str::<toml::Table>(pyproject) else {
        return HashSet::new();
    };
    let poetry = pyproject.get("tool").and_then(|tool| tool.get("poetry"));
    let keys = |table: Option<&Value>| -> Vec<String> {
        table
            .and_then(Value::as_table)
            .into_iter()
            .flatten()
            .map(|(name, _)| normalized(name))
            .collect()
    };
    let mut names: HashSet<String> = keys(poetry.and_then(|p| p.get("dependencies")))
        .into_iter()
        .chain(keys(poetry.and_then(|p| p.get("dev-dependencies"))))
        .collect();
    let groups = poetry
        .and_then(|p| p.get("group"))
        .and_then(Value::as_table);
    for group in groups.into_iter().flatten().map(|(_, group)| group) {
        if group.get("optional").and_then(Value::as_bool) != Some(true) {
            names.extend(keys(group.get("dependencies")));
        }
    }
    names
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
    let (groups, default_groups) = uv_groups(&pyproject);
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

fn locks_pytest(lock: &str) -> bool {
    lock.contains("name = \"pytest\"")
}

/// One tool per project: a uv or poetry project often keeps a list for pip too, and both would fill
/// the same `.venv`.
fn python(root: &Path) -> Option<Stack> {
    uv(root).or_else(|| poetry(root)).or_else(|| pip(root))
}

/// `--frozen` also on the check, which has no network to compare the lock with. A check needs pytest
/// to be installed, or it fails after the agents are paid for, but only setup can install it.
fn uv(root: &Path) -> Option<Stack> {
    let lock = read(root, "uv.lock")?;
    let has_pytest = locks_pytest(&lock);
    let (setup_options, for_pytest) = read(root, "pyproject.toml")
        .map(|pyproject| uv_options(&pyproject))
        .unwrap_or_default();
    let mut from = vec!["uv.lock".to_owned()];
    if !setup_options.is_empty() {
        from.push("pyproject.toml".to_owned());
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

/// `poetry install` installs every group that isn't optional, which is where pytest usually is.
fn poetry(root: &Path) -> Option<Stack> {
    let lock = read(root, "poetry.lock")?;
    is_file(root, "pyproject.toml").then(|| Stack {
        setup: Some("poetry install".to_owned()),
        check: locks_pytest(&lock).then(|| "poetry run pytest".to_owned()),
        from: vec!["pyproject.toml".to_owned(), "poetry.lock".to_owned()],
        ..Stack::default()
    })
}

/// Into a `.venv` of its own, since pip outside one installs for the whole Mac, if it's let.
/// `requirements.txt` is the project's, and the first other list with pytest is for testing.
fn pip(root: &Path) -> Option<Stack> {
    let lists: Vec<String> = file_names(root)
        .into_iter()
        .filter(|name| {
            name.starts_with("requirements") && name.ends_with(".txt") && is_plain_name(name)
        })
        .take(MOST_OPTIONS)
        .collect();
    let has_pytest = |name: &str| read(root, name).is_some_and(|text| text.lines().any(is_pytest));
    let main = lists.iter().find(|name| *name == "requirements.txt");
    let main_has_pytest = main.is_some_and(|name| has_pytest(name));
    let for_tests = lists
        .iter()
        .find(|name| !main_has_pytest && has_pytest(name));
    let install: Vec<String> = main.into_iter().chain(for_tests).cloned().collect();
    if install.is_empty() {
        return None;
    }
    let lists: String = install.iter().map(|name| format!(" -r {name}")).collect();
    Some(Stack {
        setup: Some(format!(
            "python3 -m venv .venv && .venv/bin/pip install{lists}"
        )),
        check: (main_has_pytest || for_tests.is_some())
            .then(|| ".venv/bin/python -m pytest".to_owned()),
        from: install,
        ..Stack::default()
    })
}

/// `cargo test` alone only tests the root package of a workspace.
fn rust(root: &Path) -> Option<Stack> {
    if !is_file(root, "Cargo.toml") {
        return None;
    }
    let is_workspace = read(root, "Cargo.toml")
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .is_some_and(|manifest| manifest.contains_key("workspace"));
    Some(Stack {
        setup: Some("cargo fetch".to_owned()),
        check: Some(
            if is_workspace {
                "cargo test --workspace"
            } else {
                "cargo test"
            }
            .to_owned(),
        ),
        from: vec!["Cargo.toml".to_owned()],
        ..Stack::default()
    })
}

fn go(root: &Path) -> Option<Stack> {
    is_file(root, "go.mod").then(|| Stack {
        setup: Some("go mod download".to_owned()),
        check: Some("go test ./...".to_owned()),
        from: vec!["go.mod".to_owned()],
        ..Stack::default()
    })
}

/// The wrapper a project brings, if it does, pins the version it builds with.
fn with_wrapper(root: &Path, manifest: &str, wrapper: &str, tool: &str) -> (String, Vec<String>) {
    if is_file(root, wrapper) {
        (
            format!("./{wrapper}"),
            vec![manifest.to_owned(), wrapper.to_owned()],
        )
    } else {
        (tool.to_owned(), vec![manifest.to_owned()])
    }
}

fn maven(root: &Path) -> Option<Stack> {
    is_file(root, "pom.xml").then(|| {
        let (tool, from) = with_wrapper(root, "pom.xml", "mvnw", "mvn");
        Stack {
            setup: Some(format!("{tool} dependency:go-offline")),
            check: Some(format!("{tool} test")),
            from,
            ..Stack::default()
        }
    })
}

/// `testClasses` compiles the code and its tests, which downloads what both need.
fn gradle(root: &Path) -> Option<Stack> {
    let build = [
        "build.gradle.kts",
        "build.gradle",
        "settings.gradle.kts",
        "settings.gradle",
    ]
    .into_iter()
    .find(|name| is_file(root, name))?;
    let (tool, from) = with_wrapper(root, build, "gradlew", "gradle");
    Some(Stack {
        setup: Some(format!("{tool} testClasses")),
        check: Some(format!("{tool} test")),
        from,
        ..Stack::default()
    })
}

/// `rspec-core (3.13.0)` in a lock, or `gem "rspec-rails"` in a Gemfile, but not `rspecial`.
fn uses_gem(gems: &str, gem: &str) -> bool {
    gems.lines().any(|line| {
        let line = line.trim_start();
        let name = line.strip_prefix("gem ").map_or(line, |rest| {
            rest.trim_start().trim_start_matches(['"', '\''])
        });
        name.strip_prefix(gem).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with(['-', '_', ' ', '(', '"', '\''])
        })
    })
}

/// RSpec wins, since Rails brings minitest along either way. Minitest has no command of its own, so
/// it takes Rails or a Rake task to say how it runs.
fn ruby(root: &Path) -> Option<Stack> {
    let gemfile = read(root, "Gemfile")?;
    let lock = read(root, "Gemfile.lock");
    let gems = lock.as_deref().unwrap_or(&gemfile);
    let mut from = vec!["Gemfile".to_owned()];
    if lock.is_some() {
        from.push("Gemfile.lock".to_owned());
    }
    let check = if uses_gem(gems, "rspec") {
        Some("bundle exec rspec")
    } else if is_file(root, "bin/rails") {
        from.push("bin/rails".to_owned());
        Some("bin/rails test")
    } else if uses_gem(gems, "minitest")
        && read(root, "Rakefile").is_some_and(|rakefile| rakefile.contains("TestTask"))
    {
        from.push("Rakefile".to_owned());
        Some("bundle exec rake test")
    } else {
        None
    };
    Some(Stack {
        setup: Some("bundle install".to_owned()),
        check: check.map(str::to_owned),
        from,
        ..Stack::default()
    })
}

/// The project's own `test` script wins over PHPUnit, since it can have flags PHPUnit needs.
fn php(root: &Path) -> Option<Stack> {
    let composer = read(root, "composer.json")?;
    let composer = serde_json::from_str::<serde_json::Value>(&composer).unwrap_or_default();
    let script = &composer["scripts"]["test"];
    let has_script = script.as_str().is_some_and(|s| !s.trim().is_empty())
        || script.as_array().is_some_and(|list| !list.is_empty());
    let has_phpunit = ["require-dev", "require"]
        .into_iter()
        .any(|key| composer[key].get("phpunit/phpunit").is_some());
    let mut from = vec!["composer.json".to_owned()];
    if is_file(root, "composer.lock") {
        from.push("composer.lock".to_owned());
    }
    let check = if has_script {
        Some("composer test")
    } else if has_phpunit {
        Some("vendor/bin/phpunit")
    } else {
        None
    };
    Some(Stack {
        setup: Some("composer install".to_owned()),
        check: check.map(str::to_owned),
        from,
        ..Stack::default()
    })
}

/// Told which solution, or which project when there's no solution: with more than one in the
/// folder `dotnet` stops, and a guess could pick the wrong one.
fn dotnet(root: &Path) -> Option<Stack> {
    let files = file_names(root);
    let ending = |ends: &[&str]| -> Vec<&String> {
        files
            .iter()
            .filter(|name| ends.iter().any(|end| name.ends_with(end)))
            .collect()
    };
    let solutions = ending(&[".sln", ".slnx"]);
    let found = if solutions.is_empty() {
        ending(&[".csproj", ".fsproj", ".vbproj"])
    } else {
        solutions
    };
    let [one] = found.as_slice() else {
        return None;
    };
    is_plain_name(one).then(|| Stack {
        setup: Some(format!("dotnet restore {one}")),
        check: Some(format!("dotnet test {one}")),
        from: vec![(*one).clone()],
        ..Stack::default()
    })
}

/// `swift test` fails on a package with no test target.
fn swift(root: &Path) -> Option<Stack> {
    let manifest = read(root, "Package.swift")?;
    Some(Stack {
        setup: Some("swift package resolve".to_owned()),
        check: manifest
            .contains(".testTarget(")
            .then(|| "swift test".to_owned()),
        from: vec!["Package.swift".to_owned()],
        ..Stack::default()
    })
}

/// A Flutter app depends on Flutter itself. Each tests with a package the project has to name.
fn dart(root: &Path) -> Option<Stack> {
    let pubspec = yaml(&read(root, "pubspec.yaml")?).unwrap_or_default();
    let flutter = pubspec["dependencies"]["flutter"]["sdk"].as_str() == Some("flutter");
    let (tool, runner) = if flutter {
        ("flutter", "flutter_test")
    } else {
        ("dart", "test")
    };
    let names_runner = ["dev_dependencies", "dependencies"]
        .into_iter()
        .any(|key| pubspec[key].get(runner).is_some());
    Some(Stack {
        setup: Some(format!("{tool} pub get")),
        check: names_runner.then(|| format!("{tool} test")),
        from: vec!["pubspec.yaml".to_owned()],
        ..Stack::default()
    })
}

fn elixir(root: &Path) -> Option<Stack> {
    is_file(root, "mix.exs").then(|| Stack {
        setup: Some("mix deps.get".to_owned()),
        check: Some("mix test".to_owned()),
        from: vec!["mix.exs".to_owned()],
        ..Stack::default()
    })
}

/// A Makefile's `test` target is what the project itself calls testing, so it wins. Every stack found
/// adds its setup and its check, chained so that each one has to pass, and then what else the CI
/// checks, like a linter, goes after them. When nothing here is known, what the project's CI runs is
/// the guess. Without that either, an unknown stack gets nothing rather than something plausible.
pub fn guess(root: &Path) -> Guess {
    let mut guess = Guess::default();
    let makefile = ["Makefile", "GNUmakefile", "makefile"]
        .into_iter()
        .find(|name| read(root, name).is_some_and(|text| has_test_target(&text)));
    if let Some(name) = makefile {
        guess.check = Some("make test".to_owned());
        guess.from.push(name.to_owned());
    }
    let stacks: Vec<Stack> = [
        javascript, deno, python, rust, go, maven, gradle, ruby, php, dotnet, swift, dart, elixir,
    ]
    .iter()
    .filter_map(|find| find(root))
    .collect();
    let chained = |parts: Vec<String>| (!parts.is_empty()).then(|| parts.join(" && "));
    if stacks.is_empty() && makefile.is_none() {
        let ci = ci::commands(root);
        return Guess {
            setup: chained(ci.setup),
            check: chained(ci.check),
            from_ci: !ci.from.is_empty(),
            from: ci.from,
            ci_may_fail: ci.may_fail,
            ..Guess::default()
        };
    }
    let setup = chained(stacks.iter().filter_map(|s| s.setup.clone()).collect());
    let check = chained(stacks.iter().filter_map(|s| s.check.clone()).collect());
    // With a single stack and no Makefile its check is the guess itself, or what it already offers.
    let offer_checks = stacks.len() > 1 || makefile.is_some();
    guess.setup = setup;
    if guess.check.is_none() {
        guess.check = check;
    }
    let ci = ci::missing_from(root, guess.check.as_deref().unwrap_or_default());
    if !ci.add.is_empty() {
        let own = guess.check.take().into_iter();
        guess.check = chained(own.chain(ci.add.iter().cloned()).collect());
    }
    guess.ci_checks = ci.add;
    guess.ci_left_out = ci.left_out;
    guess.ci_may_fail = ci.may_fail;
    for stack in stacks {
        guess.from.extend(stack.from);
        guess.setup_options.extend(stack.setup_options);
        let own = stack.check.filter(|_| offer_checks);
        for option in stack.check_options.into_iter().chain(own) {
            if !guess.check_options.contains(&option) {
                guess.check_options.push(option);
            }
        }
    }
    // Only what the commands came from: what's left out is only mentioned.
    if !guess.ci_checks.is_empty() {
        guess.from.extend(ci.from);
    }
    guess
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_TEST: &str = r#"{"scripts":{"test":"vitest run"}}"#;

    pub(super) fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    pub(super) fn found(files: &[(&str, &str)]) -> (Option<String>, Option<String>) {
        let guess = guess(repo(files).path());
        (guess.check, guess.setup)
    }

    pub(super) fn both(check: &str, setup: &str) -> (Option<String>, Option<String>) {
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
        // The scripts, and the one that joins them.
        assert_eq!(found.check_options.len(), MOST_OPTIONS + 1);
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
        assert_eq!(
            found.check.as_deref(),
            Some("pnpm run typecheck && pnpm test")
        );
        assert_eq!(
            found.check_options,
            [
                "pnpm run check:i18n",
                "pnpm run format:check",
                "pnpm run lint",
                "pnpm test",
                "pnpm run typecheck",
                "pnpm run check:i18n && pnpm run format:check && pnpm run lint && pnpm test && pnpm run typecheck",
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

    fn setup_only(setup: &str) -> (Option<String>, Option<String>) {
        (None, Some(setup.to_owned()))
    }

    const PIP_SETUP: &str = "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt";

    #[test]
    fn pip_installs_into_a_folder_of_its_own_and_tests_with_pytest() {
        assert_eq!(
            found(&[(
                "requirements.txt",
                "flask==3.1\npytest>=8 ; python_version > '3.9'\n"
            )]),
            both(".venv/bin/python -m pytest", PIP_SETUP)
        );
        // pytest in a list of its own is installed as well. The docs aren't.
        let guess = guess(
            repo(&[
                ("requirements.txt", "flask\n"),
                ("requirements-dev.txt", "-r requirements.txt\npytest\n"),
                ("requirements-docs.txt", "sphinx\n"),
            ])
            .path(),
        );
        assert_eq!(
            guess.setup.as_deref(),
            Some(&*format!("{PIP_SETUP} -r requirements-dev.txt"))
        );
        assert_eq!(guess.check.as_deref(), Some(".venv/bin/python -m pytest"));
        assert_eq!(guess.from, ["requirements.txt", "requirements-dev.txt"]);
        // Once is enough when the main list has it.
        assert_eq!(
            found(&[
                ("requirements.txt", "pytest\n"),
                ("requirements-dev.txt", "pytest\n")
            ])
            .1
            .as_deref(),
            Some(PIP_SETUP)
        );
        assert_eq!(
            found(&[("requirements.txt", "flask\npytest-cov\n")]),
            setup_only(PIP_SETUP)
        );
        // A name that could put something else in the command isn't read.
        assert_eq!(found(&[("requirements;x.txt", "pytest\n")]), (None, None));
    }

    #[test]
    fn poetry_tests_with_pytest_when_its_lock_has_it() {
        let guess = guess(
            repo(&[
                ("pyproject.toml", "[tool.poetry]\nname = \"a\"\n"),
                ("poetry.lock", LOCK_WITH_PYTEST),
            ])
            .path(),
        );
        assert_eq!(guess.check.as_deref(), Some("poetry run pytest"));
        assert_eq!(guess.setup.as_deref(), Some("poetry install"));
        assert_eq!(guess.from, ["pyproject.toml", "poetry.lock"]);
        assert_eq!(
            found(&[
                ("pyproject.toml", ""),
                ("poetry.lock", "[[package]]\nname = \"pytest-cov\"\n")
            ]),
            setup_only("poetry install")
        );
        // A lock without the project it locks says nothing.
        assert_eq!(found(&[("poetry.lock", LOCK_WITH_PYTEST)]), (None, None));
    }

    #[test]
    fn one_python_tool_per_project() {
        // A uv or poetry project often keeps a list for pip too, and both would fill `.venv`.
        assert_eq!(
            found(&[
                ("uv.lock", LOCK_WITH_PYTEST),
                ("requirements.txt", "pytest\n")
            ]),
            both("uv run --frozen pytest", "uv sync --frozen")
        );
        assert_eq!(
            found(&[
                ("pyproject.toml", ""),
                ("poetry.lock", LOCK_WITH_PYTEST),
                ("requirements.txt", "pytest\n")
            ]),
            both("poetry run pytest", "poetry install")
        );
    }

    #[test]
    fn maven_and_gradle_use_the_wrapper_they_bring() {
        assert_eq!(
            found(&[("pom.xml", "<project/>")]),
            both("mvn test", "mvn dependency:go-offline")
        );
        let maven = guess(repo(&[("pom.xml", "<project/>"), ("mvnw", "#!/bin/sh\n")]).path());
        assert_eq!(maven.check.as_deref(), Some("./mvnw test"));
        assert_eq!(maven.setup.as_deref(), Some("./mvnw dependency:go-offline"));
        assert_eq!(maven.from, ["pom.xml", "mvnw"]);

        assert_eq!(
            found(&[("build.gradle", "")]),
            both("gradle test", "gradle testClasses")
        );
        let gradle = guess(
            repo(&[
                ("settings.gradle.kts", ""),
                ("build.gradle.kts", ""),
                ("gradlew", "#!/bin/sh\n"),
            ])
            .path(),
        );
        assert_eq!(gradle.check.as_deref(), Some("./gradlew test"));
        assert_eq!(gradle.setup.as_deref(), Some("./gradlew testClasses"));
        assert_eq!(gradle.from, ["build.gradle.kts", "gradlew"]);
    }

    fn gem_lock(gems: &str) -> String {
        format!("GEM\n  remote: https://rubygems.org/\n  specs:\n{gems}\nPLATFORMS\n  ruby\n")
    }

    #[test]
    fn bundler_tests_with_rspec_or_minitest() {
        let rspec = gem_lock("    rspec-core (3.13.0)\n");
        assert_eq!(
            found(&[
                ("Gemfile", "source 'https://rubygems.org'\n"),
                ("Gemfile.lock", &rspec)
            ]),
            both("bundle exec rspec", "bundle install")
        );
        assert_eq!(
            found(&[("Gemfile", "gem 'rspec', '~> 3'\n")]),
            both("bundle exec rspec", "bundle install")
        );
        // Rails brings minitest even when the tests are in RSpec.
        let both_runners = gem_lock("    minitest (5.25.0)\n    rspec-rails (7.0.0)\n");
        assert_eq!(
            found(&[
                ("Gemfile", ""),
                ("Gemfile.lock", &both_runners),
                ("bin/rails", "")
            ])
            .0,
            Some("bundle exec rspec".to_owned())
        );
        let rails = gem_lock("    minitest (5.25.0)\n    railties (8.0.0)\n");
        assert_eq!(
            found(&[("Gemfile", ""), ("Gemfile.lock", &rails), ("bin/rails", "")]),
            both("bin/rails test", "bundle install")
        );
        let guess = guess(
            repo(&[
                ("Gemfile", "gem \"minitest\"\n"),
                ("Rakefile", "Minitest::TestTask.create\n"),
            ])
            .path(),
        );
        assert_eq!(guess.check.as_deref(), Some("bundle exec rake test"));
        assert_eq!(guess.from, ["Gemfile", "Rakefile"]);
        // minitest with nothing that runs it, or a gem that only starts like one, says nothing.
        assert_eq!(
            found(&[("Gemfile", "gem 'minitest'\n")]),
            setup_only("bundle install")
        );
        assert_eq!(
            found(&[("Gemfile", "gem 'rspecial'\n")]),
            setup_only("bundle install")
        );
    }

    #[test]
    fn composer_tests_with_its_own_script_or_phpunit() {
        let phpunit = r#"{"require-dev":{"phpunit/phpunit":"^11"}}"#;
        assert_eq!(
            found(&[("composer.json", phpunit)]),
            both("vendor/bin/phpunit", "composer install")
        );
        let script = r#"{"require-dev":{"phpunit/phpunit":"^11"},"scripts":{"test":["@phpunit"]}}"#;
        assert_eq!(
            found(&[("composer.json", script), ("composer.lock", "{}")]),
            both("composer test", "composer install")
        );
        for nothing in ["{}", "not json", r#"{"scripts":{"test":""}}"#] {
            assert_eq!(
                found(&[("composer.json", nothing)]),
                setup_only("composer install"),
                "{nothing}"
            );
        }
    }

    #[test]
    fn dotnet_is_told_which_solution_or_project() {
        assert_eq!(
            found(&[("Shop.sln", ""), ("Shop.csproj", "")]),
            both("dotnet test Shop.sln", "dotnet restore Shop.sln")
        );
        assert_eq!(
            found(&[("Tool.fsproj", "")]),
            both("dotnet test Tool.fsproj", "dotnet restore Tool.fsproj")
        );
        // With several there's no telling which, and some names can't go in a command.
        assert_eq!(found(&[("A.sln", ""), ("B.slnx", "")]), (None, None));
        assert_eq!(found(&[("a b.sln", "")]), (None, None));
    }

    #[test]
    fn swift_tests_only_a_package_with_test_targets() {
        let package = r#"let package = Package(name: "A", targets: [.target(name: "A"), .testTarget(name: "ATests")])"#;
        assert_eq!(
            found(&[("Package.swift", package)]),
            both("swift test", "swift package resolve")
        );
        assert_eq!(
            found(&[("Package.swift", r#"let package = Package(name: "A")"#)]),
            setup_only("swift package resolve")
        );
    }

    #[test]
    fn dart_and_flutter_test_with_the_package_they_name() {
        assert_eq!(
            found(&[(
                "pubspec.yaml",
                "name: a\ndev_dependencies:\n  test: ^1.25.0\n"
            )]),
            both("dart test", "dart pub get")
        );
        let flutter = "name: app\ndependencies:\n  flutter:\n    sdk: flutter\n";
        assert_eq!(
            found(&[(
                "pubspec.yaml",
                &format!("{flutter}dev_dependencies:\n  flutter_test:\n    sdk: flutter\n")
            )]),
            both("flutter test", "flutter pub get")
        );
        assert_eq!(
            found(&[("pubspec.yaml", flutter)]),
            setup_only("flutter pub get")
        );
        assert_eq!(
            found(&[("pubspec.yaml", "name: a\n")]),
            setup_only("dart pub get")
        );
    }

    #[test]
    fn mix_is_recognized() {
        assert_eq!(found(&[("mix.exs", "")]), both("mix test", "mix deps.get"));
    }

    #[test]
    fn deno_tests_with_its_test_task() {
        let config = r#"{"tasks":{"test":"deno test -A","lint":"deno lint","dev":"deno run --watch main.ts"}}"#;
        let guess = guess(repo(&[("deno.json", config)]).path());
        assert_eq!(guess.check.as_deref(), Some("deno task test"));
        assert_eq!(guess.setup.as_deref(), Some("deno install"));
        assert_eq!(
            guess.check_options,
            [
                "deno task lint",
                "deno task test",
                "deno task lint && deno task test"
            ]
        );
        assert_eq!(guess.from, ["deno.json"]);
        // The longer way of writing a task.
        assert_eq!(
            found(&[("deno.json", r#"{"tasks":{"test":{"command":"deno test"}}}"#)]).0,
            Some("deno task test".to_owned())
        );
        // Without a task nothing says how, and a bare `deno test` fails where there are no tests.
        assert_eq!(
            found(&[("deno.jsonc", "// comment\n{}")]),
            setup_only("deno install")
        );
    }

    #[test]
    fn bun_runs_the_test_script_and_not_its_own_runner() {
        // `bun test` is Bun's runner, which doesn't run the script.
        assert_eq!(
            found(&[("package.json", WITH_TEST), ("bun.lock", "")]),
            both("bun run test", "bun install --frozen-lockfile")
        );
        let guess = guess(repo(&[("package.json", PNPM_BOTH), ("bun.lockb", "")]).path());
        assert_eq!(
            guess.check.as_deref(),
            Some("bun run typecheck && bun run test")
        );
        assert_eq!(
            guess.check_options,
            [
                "bun run test",
                "bun run typecheck",
                "bun run test && bun run typecheck"
            ]
        );
        assert_eq!(guess.from, ["package.json", "bun.lockb"]);
    }

    #[test]
    fn the_new_stacks_chain_like_the_others() {
        let guess = guess(
            repo(&[
                ("package.json", WITH_TEST),
                ("pnpm-lock.yaml", ""),
                ("pom.xml", ""),
                ("mix.exs", ""),
            ])
            .path(),
        );
        assert_eq!(
            guess.setup.as_deref(),
            Some("pnpm install --frozen-lockfile && mvn dependency:go-offline && mix deps.get")
        );
        assert_eq!(
            guess.check.as_deref(),
            Some("pnpm test && mvn test && mix test")
        );
        assert!(!guess.from_ci);
    }

    const PNPM_BOTH: &str = r#"{"scripts":{"test":"vitest","typecheck":"tsc"}}"#;

    #[test]
    fn every_stack_found_is_chained() {
        let guess = guess(
            repo(&[
                ("package.json", PNPM_BOTH),
                ("pnpm-lock.yaml", ""),
                ("Cargo.toml", "[workspace]\nmembers = [\"a\"]\n"),
            ])
            .path(),
        );
        assert_eq!(
            guess.check.as_deref(),
            Some("pnpm run typecheck && pnpm test && cargo test --workspace")
        );
        assert_eq!(
            guess.setup.as_deref(),
            Some("pnpm install --frozen-lockfile && cargo fetch")
        );
        assert_eq!(guess.from, ["package.json", "pnpm-lock.yaml", "Cargo.toml"]);
        assert!(
            guess
                .check_options
                .contains(&"cargo test --workspace".to_owned())
        );
        assert!(guess.check_options.contains(&"pnpm test".to_owned()));
    }

    #[test]
    fn one_language_and_one_check_propose_what_they_did_before() {
        assert_eq!(
            found(&[("Cargo.toml", "[package]\nname = \"a\"\n")]),
            both("cargo test", "cargo fetch")
        );
        assert_eq!(
            found(&[("Cargo.toml", "not toml [[")]),
            both("cargo test", "cargo fetch")
        );
        assert_eq!(
            found(&[("package.json", WITH_TEST), ("pnpm-lock.yaml", "")]),
            both("pnpm test", "pnpm install --frozen-lockfile")
        );
        let guess = guess(repo(&[("uv.lock", LOCK_WITH_PYTEST)]).path());
        assert_eq!(guess.check_options, Vec::<String>::new());
    }

    #[test]
    fn the_check_joins_only_typecheck_and_test() {
        for (script, name) in [("types", "types"), ("tsc", "tsc")] {
            let package = format!(r#"{{"scripts":{{"test":"x","{script}":"y"}}}}"#);
            let found = found(&[("package.json", &package), ("package-lock.json", "{}")]);
            assert_eq!(found.0, Some(format!("npm run {name} && npm test")),);
        }
        // Without a test script nothing is claimed, as before.
        let package = r#"{"scripts":{"typecheck":"tsc"}}"#;
        assert_eq!(
            found(&[("package.json", package), ("pnpm-lock.yaml", "")]).0,
            None
        );
    }

    #[test]
    fn a_makefile_still_wins_and_the_stacks_are_on_offer() {
        let guess = guess(
            repo(&[
                ("Makefile", "test:\n\ttrue\n"),
                ("package.json", WITH_TEST),
                ("package-lock.json", "{}"),
                ("Cargo.toml", ""),
            ])
            .path(),
        );
        assert_eq!(guess.check.as_deref(), Some("make test"));
        assert_eq!(guess.setup.as_deref(), Some("npm ci && cargo fetch"));
        assert!(guess.check_options.contains(&"npm test".to_owned()));
        assert!(guess.check_options.contains(&"cargo test".to_owned()));
    }

    #[test]
    fn javascript_without_a_lockfile_is_still_ignored_next_to_rust() {
        assert_eq!(
            found(&[("package.json", PNPM_BOTH), ("Cargo.toml", "")]),
            both("cargo test", "cargo fetch")
        );
    }

    #[test]
    fn uv_and_another_language_chain_and_keep_their_flags() {
        let pyproject = "[dependency-groups]\nstubs = [\"x\"]\n";
        let guess = guess(
            repo(&[
                ("uv.lock", LOCK_WITH_PYTEST),
                ("pyproject.toml", pyproject),
                ("Cargo.toml", ""),
            ])
            .path(),
        );
        assert_eq!(
            guess.setup.as_deref(),
            Some("uv sync --frozen && cargo fetch")
        );
        assert_eq!(
            guess.check.as_deref(),
            Some("uv run --frozen pytest && cargo test")
        );
        assert_eq!(guess.setup_options, ["--group stubs"]);
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
