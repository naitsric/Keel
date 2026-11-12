//! For a project whose files say nothing Keel recognizes: what its CI already runs to install and to
//! test. For one whose files do: what else its CI checks, like a linter or a type checker. A workflow
//! is the project's own script, so this only reads it, and what's taken from it is for a person to
//! confirm like any other guess. Only what runs for every pull request, and in Keel as it runs in
//! CI, is taken: anything Keel can't be sure of is only said, with why.

use std::collections::HashSet;
use std::path::Path;

use serde_yaml_ng::Value;

use super::{
    CHECKS, CiLeftOut, CiWhy, MOST_OPTIONS, NOT_CHECKS, calls, file_names, is_dir, read, yaml,
};

/// A repo decides how many workflows it has.
const MOST_WORKFLOWS: usize = 12;
/// More than a person reads at a glance before saving it.
const LONGEST_COMMAND: usize = 200;
/// Runners and linters whose names don't say they check something.
const RUNNERS: [&str; 16] = [
    "pytest", "rspec", "phpunit", "jest", "vitest", "mocha", "tox", "nox", "ctest", "unittest",
    "mypy", "pyright", "flake8", "eslint", "clippy", "ruff",
];
/// `uv sync`, `go mod download`, `pip install pytest` or `uv add types-requests` install, even
/// when they name a runner or a word like `types`.
const INSTALLS: [&str; 8] = [
    "install", "sync", "restore", "fetch", "download", "get", "resolve", "add",
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
/// Templates a GitLab job is looked up through, since one can extend another.
const MOST_EXTENDS: usize = 8;
/// Variables that change how output looks, or how CI checks the repo out, and never what a check
/// finds.
const HARMLESS_ENV: [&str; 12] = [
    "CARGO_TERM_COLOR",
    "FORCE_COLOR",
    "TERM",
    "CI",
    "PY_COLORS",
    "PYTHONUNBUFFERED",
    "NO_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "GIT_DEPTH",
    "GIT_STRATEGY",
    "GIT_SUBMODULE_STRATEGY",
];

/// What makes Keel unsure a command runs in Keel as it does in CI.
#[derive(Clone, Copy, Default)]
struct Doubts {
    sometimes: bool,
    template: bool,
    may_fail: bool,
    env: bool,
}

impl Doubts {
    /// The one to say, when there are several.
    fn why(self) -> Option<CiWhy> {
        [
            (self.sometimes, CiWhy::Sometimes),
            (self.template, CiWhy::Template),
            (self.may_fail, CiWhy::MayFail),
            (self.env, CiWhy::Env),
        ]
        .into_iter()
        .find_map(|(doubt, why)| doubt.then_some(why))
    }
}

/// A command of a job, and why Keel can't take it, if it can't.
struct Line {
    command: String,
    doubt: Option<CiWhy>,
}

/// A job's commands in the order they run, and its scripts as written, to see what it installs.
#[derive(Default)]
struct Job {
    lines: Vec<Line>,
    scripts: Vec<String>,
}

#[derive(Default)]
pub(super) struct Found {
    pub setup: Vec<String>,
    pub check: Vec<String>,
    /// What the CI checks, and why the check doesn't.
    pub left_out: Vec<CiLeftOut>,
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
    /// what comes after, like reports, uploads and deploys, is left out. So is what Keel can't be
    /// sure runs as in CI, like what CI lets fail. Each command of a line counts on its own, so
    /// `just fmt && just test` tests, except in a line that runs another when one fails.
    fn add(&mut self, job: &Job) -> bool {
        let mut commands: Vec<(&str, Kind)> = Vec::new();
        for line in &job.lines {
            let parts: Vec<&str> = if line.command.contains(['(', ';', '|']) {
                vec![line.command.as_str()]
            } else {
                line.command.split("&&").map(str::trim).collect()
            };
            for part in parts.into_iter().filter(|part| runs_here(part)) {
                let kind = match (kind(part), line.doubt) {
                    (Kind::Checks, Some(why)) => {
                        say(&mut self.left_out, part, why);
                        Kind::Neither
                    }
                    (_, Some(_)) => Kind::Neither,
                    (kind, None) => kind,
                };
                commands.push((part, kind));
            }
        }
        let Some(last) = commands.iter().rposition(|(_, kind)| *kind == Kind::Checks) else {
            return false;
        };
        for (at, (command, kind)) in commands.iter().enumerate() {
            match kind {
                Kind::Checks => push_new(&mut self.check, command),
                Kind::Prepares if at < last => push_new(&mut self.setup, command),
                _ => {}
            }
        }
        true
    }
}

fn push_new(list: &mut Vec<String>, command: &str) {
    if list.len() < MOST_OPTIONS && !list.iter().any(|known| known == command) {
        list.push(command.to_owned());
    }
}

fn say(list: &mut Vec<CiLeftOut>, command: &str, why: CiWhy) {
    if list.len() < MOST_OPTIONS && !list.iter().any(|known| known.command == command) {
        list.push(CiLeftOut {
            command: command.to_owned(),
            why,
        });
    }
}

/// What the CI checks that `check` doesn't run yet, in the order it runs it.
#[derive(Default)]
pub(super) struct Missing {
    /// As Keel runs them with no network.
    pub add: Vec<String>,
    /// As the CI runs them, and why the check doesn't.
    pub left_out: Vec<CiLeftOut>,
    /// The files they're in.
    pub from: Vec<String>,
}

/// Each simple command of each line is judged on its own, so `ruff check . && curl …` gives only
/// `ruff check .`. Two that run the same tool in the same mode are one, and a script or a Makefile
/// target counts as what it runs. What Keel can't be sure of is only said, and looked at last, so
/// the same tool that another job runs plainly is proposed rather than said. A job that installs
/// more than the project's setup has its checks said: they may need what it installs.
pub(super) fn missing_from(root: &Path, setup: &str, check: &str) -> Missing {
    let mut repo = calls::Repo::read(root, setup);
    let mut checked = repo.checked(check);
    let mut said = HashSet::new();
    let mut left_out: Vec<(Vec<calls::Sig>, CiLeftOut)> = Vec::new();
    let mut missing = Missing::default();
    let files: Vec<(String, Vec<Job>)> = github(root).into_iter().chain(gitlab(root)).collect();
    let mut used = vec![false; files.len()];
    for doubtful in [false, true] {
        for (at, (_, jobs)) in files.iter().enumerate() {
            for job in jobs {
                let installs = job.scripts.iter().any(|script| repo.installs_more(script));
                for line in &job.lines {
                    let doubt = line.doubt.or_else(|| installs.then_some(CiWhy::Installs));
                    if doubt.is_some() != doubtful {
                        continue;
                    }
                    for command in calls::simple_commands(&line.command) {
                        if !runs_here(command)
                            || (kind(command) != Kind::Checks && !calls::listed(command))
                        {
                            continue;
                        }
                        let sigs = repo.sigs(command);
                        if calls::is_checked(&checked, &sigs) {
                            continue;
                        }
                        used[at] = true;
                        let why = match doubt.map_or_else(|| repo.offline(command), Err) {
                            Ok(form) => {
                                checked.extend(repo.sigs(&form));
                                if missing.add.len() < MOST_OPTIONS {
                                    missing.add.push(form);
                                }
                                continue;
                            }
                            Err(why) => why,
                        };
                        let new = sigs.first().is_none_or(|own| said.insert(own.clone()));
                        if new
                            && left_out.len() < MOST_OPTIONS
                            && !left_out.iter().any(|(_, known)| known.command == command)
                        {
                            let command = command.to_owned();
                            left_out.push((sigs, CiLeftOut { command, why }));
                        }
                    }
                }
            }
        }
    }
    // What another job let Keel propose isn't left out too.
    missing.left_out = left_out
        .into_iter()
        .filter(|(sigs, _)| !calls::is_checked(&checked, sigs))
        .map(|(_, left)| left)
        .collect();
    missing.from = files
        .into_iter()
        .zip(used)
        .filter_map(|((file, _), used)| used.then_some(file))
        .collect();
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

/// When a workflow runs for a pull request's commits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Runs {
    Never,
    /// Only for some files, or some of its events.
    Sometimes,
    Always,
}

/// On `pull_request`, or on `push` to any branch, as Keel's is. Only for some paths, or for kinds of
/// pull request event that leave out a new commit, is only sometimes. With no trigger, never.
fn runs_for_pull_requests(on: &Value) -> Runs {
    let events: Vec<(&str, &Value)> = match on {
        Value::String(event) => vec![(event.as_str(), &Value::Null)],
        Value::Sequence(events) => events
            .iter()
            .filter_map(Value::as_str)
            .map(|event| (event, &Value::Null))
            .collect(),
        Value::Mapping(events) => events
            .iter()
            .filter_map(|(event, filters)| Some((event.as_str()?, filters)))
            .collect(),
        _ => Vec::new(),
    };
    let names = |filters: &Value, key: &str| -> Option<Vec<String>> {
        let listed = filters.get(key)?;
        Some(
            listed
                .as_sequence()
                .into_iter()
                .flatten()
                .chain(Some(listed))
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        )
    };
    let only_some_paths =
        |filters: &Value| filters.get("paths").is_some() || filters.get("paths-ignore").is_some();
    events
        .into_iter()
        .map(|(event, filters)| match event {
            "pull_request" => {
                let types = names(filters, "types");
                let every_commit = types.is_none_or(|types| {
                    ["opened", "synchronize"]
                        .iter()
                        .all(|kind| types.iter().any(|t| t == kind))
                });
                if only_some_paths(filters) || !every_commit {
                    Runs::Sometimes
                } else {
                    Runs::Always
                }
            }
            "push" => {
                let any_branch = match names(filters, "branches") {
                    Some(branches) => branches.iter().any(|branch| branch == "**"),
                    None => filters.get("tags").is_none(),
                };
                if !any_branch {
                    Runs::Never
                } else if only_some_paths(filters) {
                    Runs::Sometimes
                } else {
                    Runs::Always
                }
            }
            _ => Runs::Never,
        })
        .max()
        .unwrap_or(Runs::Never)
}

/// An `if:` that may not hold. One that always does, like `always()`, isn't.
fn conditional(condition: &Value) -> bool {
    match condition {
        Value::Null | Value::Bool(true) => false,
        Value::String(text) => {
            let text = text.trim();
            let inside = text
                .strip_prefix("${{")
                .and_then(|text| text.strip_suffix("}}"))
                .map_or(text, str::trim);
            !matches!(inside, "always()" | "success()" | "true")
        }
        _ => true,
    }
}

/// Variables that can change what a command checks: any but the harmless ones, and anything an
/// expression fills in.
fn changes_env(env: &Value) -> bool {
    match env {
        Value::Null => false,
        Value::Mapping(variables) => variables.keys().any(|name| {
            name.as_str()
                .is_none_or(|name| !HARMLESS_ENV.contains(&name))
        }),
        _ => true,
    }
}

/// Only workflows that run for a pull request's commits count. Jobs that run on Windows are left
/// out, and so are steps in another shell or another folder. What runs only under a condition,
/// what may fail, and what runs with variables of its own, is said.
fn github(root: &Path) -> Vec<(String, Vec<Job>)> {
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
            let runs = runs_for_pull_requests(&workflow["on"]);
            if runs == Runs::Never {
                return None;
            }
            let jobs = workflow["jobs"]
                .as_mapping()?
                .values()
                .filter(|job| {
                    here(&workflow["defaults"]["run"])
                        && here(&job["defaults"]["run"])
                        && !on_windows(&job["runs-on"])
                })
                .map(|job| {
                    let of_job = Doubts {
                        sometimes: runs == Runs::Sometimes || conditional(&job["if"]),
                        template: false,
                        may_fail: lets_fail(&job["continue-on-error"]),
                        env: changes_env(&workflow["env"]) || changes_env(&job["env"]),
                    };
                    let mut found = Job::default();
                    for step in job["steps"].as_sequence().into_iter().flatten() {
                        let Some(script) = step["run"].as_str().filter(|_| here(step)) else {
                            continue;
                        };
                        found.scripts.push(script.to_owned());
                        let of_step = Doubts {
                            sometimes: of_job.sometimes || conditional(&step["if"]),
                            may_fail: of_job.may_fail || lets_fail(&step["continue-on-error"]),
                            env: of_job.env || changes_env(&step["env"]),
                            ..of_job
                        };
                        // Each step is a shell of its own, which stops at the first failure.
                        found.lines.extend(in_lines(script, of_step, &mut true));
                    }
                    found
                })
                .collect();
            Some((file, jobs))
        })
        .collect()
}

/// Anything but `false` may let it fail, like `true` or `${{ matrix.experimental }}`.
fn lets_fail(setting: &Value) -> bool {
    !matches!(setting, Value::Null | Value::Bool(false))
}

/// A script's commands, with what makes Keel unsure of each. After `set +e` the shell no longer
/// stops at a failure, so what follows may fail.
fn in_lines(script: &str, doubts: Doubts, stops: &mut bool) -> Vec<Line> {
    lines(script, stops)
        .into_iter()
        .map(|(command, goes_on)| Line {
            command,
            doubt: Doubts {
                may_fail: doubts.may_fail || goes_on,
                ..doubts
            }
            .why(),
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

/// A GitLab job and the templates it extends, in the order their keys win: its own, then the last
/// template it names, and each template before the ones it extends.
struct Chain<'a> {
    order: Vec<&'a Value>,
    /// It extends a template that isn't in this file, like one from an `include`, which can say
    /// anything.
    unknown: bool,
}

impl<'a> Chain<'a> {
    fn of(pipeline: &'a Value, job: &'a Value) -> Self {
        let mut chain = Self {
            order: Vec::new(),
            unknown: false,
        };
        chain.walk(pipeline, job);
        chain
    }

    fn walk(&mut self, pipeline: &'a Value, job: &'a Value) {
        if self.order.len() > MOST_EXTENDS {
            self.unknown = true;
            return;
        }
        self.order.push(job);
        let names: Vec<&str> = scripts(&job["extends"]).collect();
        for name in names.into_iter().rev() {
            match pipeline.get(name) {
                Some(template) => self.walk(pipeline, template),
                None => self.unknown = true,
            }
        }
    }

    /// A key as the job ends up with it.
    fn get(&self, key: &str) -> Option<&'a Value> {
        self.order.iter().find_map(|job| job.get(key))
    }

    /// Rules, `only` or `except` that may not hold, or a job that waits for a failure or never
    /// runs. A rule with no condition holds.
    fn sometimes(&self) -> bool {
        let mut rules = self
            .get("rules")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten();
        rules.any(|rule| {
            ["if", "changes", "exists"]
                .iter()
                .any(|key| rule.get(key).is_some())
        }) || self.get("only").is_some()
            || self.get("except").is_some()
            || self
                .get("when")
                .and_then(Value::as_str)
                .is_some_and(|when| matches!(when, "never" | "on_failure"))
    }

    /// `allow_failure` on one of its rules, or as the job ends up with it. Anything but `false`
    /// lets it fail, like a list of exit codes. A manual job may fail unless it says otherwise.
    fn may_fail(&self) -> bool {
        let mut rules = self
            .get("rules")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten();
        if rules.any(|rule| lets_fail(&rule["allow_failure"])) {
            return true;
        }
        match self.get("allow_failure") {
            Some(allow) => lets_fail(allow),
            None => self.get("when").and_then(Value::as_str) == Some("manual"),
        }
    }
}

/// A job is any entry with a `script`, except a hidden one, whose name starts with a dot. Its own
/// `before_script` replaces the one in `default`, and both run in one shell with its `script`.
fn gitlab(root: &Path) -> Option<(String, Vec<Job>)> {
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
            let chain = Chain::of(&pipeline, job);
            let doubts = Doubts {
                sometimes: chain.sometimes(),
                template: chain.unknown,
                may_fail: chain.may_fail(),
                env: changes_env(&pipeline["variables"])
                    || chain.order.iter().any(|job| changes_env(&job["variables"])),
            };
            let before = chain.get("before_script").unwrap_or(before);
            let mut found = Job::default();
            let mut stops = true;
            for script in scripts(before).chain(scripts(&job["script"])) {
                found.scripts.push(script.to_owned());
                found.lines.extend(in_lines(script, doubts, &mut stops));
            }
            found
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

/// A line without the comment at its end. After a quote, a `#` may be inside it, so the line stays
/// as it is, and the command is never taken as plain.
fn without_comment(line: &str) -> &str {
    let mut before = ' ';
    for (at, c) in line.char_indices() {
        if c == '\'' || c == '"' {
            return line;
        }
        if c == '#' && before.is_whitespace() {
            return line[..at].trim_end();
        }
        before = c;
    }
    line
}

/// What `set` does to whether the shell stops at a failure: `+e` and `+o errexit` turn it off,
/// `-e` and `-o errexit` on.
fn set_stops(command: &str, stops: &mut bool) {
    let words: Vec<&str> = command.split_whitespace().skip(1).collect();
    for (at, word) in words.iter().enumerate() {
        let errexit = words.get(at + 1) == Some(&"errexit");
        match *word {
            "-o" if errexit => *stops = true,
            "+o" if errexit => *stops = false,
            flags if flags.starts_with('-') && flags.contains('e') && flags != "-o" => {
                *stops = true;
            }
            flags if flags.starts_with('+') && flags.contains('e') && flags != "+o" => {
                *stops = false;
            }
            _ => {}
        }
    }
}

/// The commands in a script, a line each, as its shell runs them, and whether the shell goes on
/// past a failure when each runs, after `set +e`. While it stops at the first failure each has
/// to pass for the next to run, so they chain with `&&`. A script that moves to another folder, or
/// whose lines only mean something together, is left out whole. Comments at the end of a line are
/// not part of its command.
fn lines(script: &str, stops: &mut bool) -> Vec<(String, bool)> {
    let mut commands = Vec::new();
    let mut current = String::new();
    for line in script.lines().map(str::trim) {
        if current.is_empty() && (line.is_empty() || line.starts_with('#')) {
            continue;
        }
        let line = without_comment(line);
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
    let mut found = Vec::new();
    for command in commands {
        if command.split_whitespace().next() == Some("set") {
            set_stops(&command, stops);
            continue;
        }
        // `a; b` or `a || b` chained with `&&` would change what has to pass.
        let command = if command.contains(';') || command.contains("||") {
            format!("({command})")
        } else {
            command
        };
        found.push((command, !*stops));
    }
    if together { Vec::new() } else { found }
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
/// One that rewrites the code, like a formatter, or never ends, is neither.
fn kind(command: &str) -> Kind {
    // `--fix`, `test:watch` or `deps.get`, but not `test_get.py`, which is a file, nor
    // `--watch=false`.
    let words: Vec<String> = command
        .split(|c: char| c.is_whitespace() || ":.".contains(c))
        .map(|word| word.trim_start_matches('-').to_ascii_lowercase())
        .collect();
    let installs = words.iter().any(|word| INSTALLS.contains(&word.as_str()))
        || words.windows(2).any(|pair| pair == ["npm", "ci"]);
    // `run-tests.sh` or `vendor/bin/phpunit`.
    let checks = command
        .split(|c: char| c.is_whitespace() || ":-_./=()".contains(c))
        .map(str::to_ascii_lowercase)
        .any(|part| CHECKS.contains(&part.as_str()) || RUNNERS.contains(&part.as_str()));
    if calls::rewrites(command) || words.iter().any(|word| NOT_CHECKS.contains(&word.as_str())) {
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
    use crate::detect::{
        CiGap, CiLeftOut, CiWhy, Guess, LARGEST_FILE, MOST_OPTIONS, ci_gap, guess,
    };
    use std::path::Path;

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
        let build_only = "on: pull_request\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: npm install\n      - run: npm run build\n";
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
on: pull_request
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
on: pull_request
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
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: |\n          ./configure &&\n          make\n      - run: make test\n";
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
        let workflow = "on:\n  pull_request:\n  schedule:\n    - cron: '*/15 * * * *'\njobs:\n  t:\n    steps:\n      - run: rm -f build/* && ./run-tests.sh\n";
        assert_eq!(
            found(&[(".github/workflows/ci.yml", workflow)]),
            both("./run-tests.sh", "rm -f build/*")
        );
    }

    const ONE_TEST: &str =
        "on: pull_request\njobs:\n  t:\n    steps:\n      - run: ./run-tests.sh\n";

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
                    format!(
                        "on: pull_request\njobs:\n  t:\n    steps:\n      - run: ./test-{n}.sh\n"
                    ),
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
        let workflow = format!("on: pull_request\njobs:\n  t:\n    steps:\n{steps}");
        let check = found(&[(".github/workflows/ci.yml", &workflow)]).0.unwrap();
        assert_eq!(check.split(" && ").count(), MOST_OPTIONS);
    }

    const PYTHON_CI: &str = "\
on: pull_request
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

    /// What the CI checks that `check` doesn't, with the setup Keel guesses for the repo.
    fn gap_of(root: &Path, check: &str) -> CiGap {
        ci_gap(root, &guess(root).setup.unwrap_or_default(), check)
    }

    /// The commands left out, as the CI runs them.
    fn left(list: &[CiLeftOut]) -> Vec<&str> {
        list.iter().map(|left| left.command.as_str()).collect()
    }

    /// The commands left out, and why.
    fn said(list: &[CiLeftOut]) -> Vec<(&str, CiWhy)> {
        list.iter()
            .map(|left| (left.command.as_str(), left.why))
            .collect()
    }

    fn all(commands: &[&'static str], why: CiWhy) -> Vec<(&'static str, CiWhy)> {
        commands.iter().map(|command| (*command, why)).collect()
    }

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
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: uv sync --frozen\n      - run: uv pip install pytest\n      - run: uv run pytest\n";
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
        assert_eq!(left(&guess.ci_left_out), ["uv run pytest"]);
    }

    #[test]
    fn a_tool_only_in_a_group_the_setup_leaves_out_is_left_out() {
        let workflow =
            "on: pull_request\njobs:\n  t:\n    steps:\n      - run: uv run ruff check .\n";
        let only_lint = "[dependency-groups]\ndev = [\"pytest\"]\nlint = [\"ruff\"]\n";
        let gap = |pyproject: &str| {
            let dir = repo(&[
                ("uv.lock", &lock(&["pytest", "ruff"])),
                ("pyproject.toml", pyproject),
                (CI_FILE, workflow),
            ]);
            gap_of(dir.path(), "uv run --frozen pytest")
        };
        let lint_aside = gap(only_lint);
        assert!(lint_aside.commands.is_empty());
        assert_eq!(left(&lint_aside.left_out), ["uv run ruff check ."]);
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
            gap_of(dir.path(), "poetry run pytest")
        };
        assert_eq!(poetry("false").commands, ["poetry run ruff check ."]);
        assert_eq!(left(&poetry("true").left_out), ["uv run ruff check ."]);
    }

    #[test]
    fn only_checks_on_the_list_are_proposed_and_the_rest_is_left_out() {
        let package = r#"{"scripts":{"test":"vitest run","lint":"eslint .","test:e2e":"playwright test","verify":"pnpm lint && vercel deploy --prod"},
            "devDependencies":{"vitest":"1","eslint":"9","typescript":"5","@playwright/test":"1"}}"#;
        let workflow = "\
on: pull_request
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
            left(&node.ci_left_out),
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
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: uv run danger ci\n      - run: uv run markdown-link-check README.md\n      - run: uv run safety check\n      - run: uv run alembic check\n";
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

        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: cargo +nightly fmt --check\n      - run: cargo audit\n      - run: cargo test\n";
        let rust =
            guess(repo(&[("Cargo.toml", ""), (".github/workflows/ci.yml", workflow)]).path());
        assert_eq!(rust.check.as_deref(), Some("cargo test"));
        assert_eq!(left(&rust.ci_left_out), ["cargo +nightly fmt --check"]);
    }

    #[test]
    fn what_the_check_already_runs_is_not_added_from_ci() {
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: cargo test --all\n      - run: cargo clippy --all-targets -- -D warnings\n      - run: cargo fmt --all --check\n";
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
        let workflow = "on: pull_request\njobs:\n  a:\n    steps:\n      - run: uv run mypy src\n  b:\n    steps:\n      - run: mypy .\n";
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
on: pull_request
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
            left(&guess.ci_left_out),
            ["uv run pyright $(git ls-files '*.py') | tee >(nc example.com 80)"]
        );
        let check = guess.check.unwrap();
        for never in ["curl", "publish", "$", "|", "ruff format ."] {
            assert!(!check.contains(never), "{never} in {check}");
        }

        // The issue's own example: each one on its own.
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: uv run ruff check . && uv run pyright\n";
        let dir = repo(&[
            ("uv.lock", &lock(&tools)),
            ("pyproject.toml", &dev_group(&tools)),
            (CI_FILE, workflow),
        ]);
        assert_eq!(
            gap_of(
                dir.path(),
                "uv run --frozen pytest && uv run --frozen ruff check"
            )
            .commands,
            ["uv run --frozen pyright"]
        );
    }

    #[test]
    fn options_of_uv_run_in_a_saved_check_are_understood() {
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: uv run pytest -x\n";
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
            assert_eq!(gap_of(dir.path(), check), CiGap::default(), "{check}");
        }
    }

    #[test]
    fn a_makefile_that_runs_itself_cannot_hold_it_up() {
        let mut makefile = String::from("test:\n");
        makefile.push_str(&"\tmake test\n".repeat(500));
        makefile.push_str("lint:\n");
        makefile.push_str(&"\tmake lint\n".repeat(500));
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: make lint\n      - run: make test\n";
        let dir = repo(&[("Makefile", &makefile), (CI_FILE, workflow)]);
        let started = std::time::Instant::now();
        let guess = guess(dir.path());
        let gap = gap_of(dir.path(), "make test");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(guess.check.as_deref(), Some("make test"));
        assert_eq!(left(&gap.left_out), ["make lint"]);
    }

    #[test]
    fn a_tool_counts_as_checked_only_in_the_same_mode() {
        let workflow = "\
on: pull_request
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
        let gap = gap_of(
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
        assert_eq!(left(&gap.left_out), ["npm test"]);
    }

    #[test]
    fn a_script_or_a_make_target_counts_as_what_it_runs() {
        // npm test is jest, so CI's own jest is already in the check.
        let package = r#"{"scripts":{"test":"jest"},"devDependencies":{"jest":"29"}}"#;
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: npm ci\n      - run: npx jest --ci --coverage\n";
        let dir = repo(&[
            ("package.json", package),
            ("package-lock.json", "{}"),
            (".github/workflows/ci.yml", workflow),
        ]);
        let node = guess(dir.path());
        assert_eq!(node.check.as_deref(), Some("npm test"));
        assert!(node.ci_checks.is_empty() && node.ci_left_out.is_empty());
        // And CI's script is what a check that runs jest itself already runs.
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: npm test\n";
        let dir = repo(&[
            ("package.json", package),
            ("package-lock.json", "{}"),
            (".github/workflows/ci.yml", workflow),
        ]);
        assert_eq!(gap_of(dir.path(), "npx jest"), CiGap::default());

        let makefile = "test:\n\t@uv run pytest -q\nlint:\n\tuv run ruff check .\n";
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: uv run pytest\n      - run: uv run ruff check .\n";
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
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: mix format --check-formatted\n      - run: mix test\n";
        let mix = guess(repo(&[("mix.exs", ""), (".github/workflows/ci.yml", workflow)]).path());
        assert_eq!(
            mix.check.as_deref(),
            Some("mix test && mix format --check-formatted")
        );
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: dotnet format app.sln --verify-no-changes\n";
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
            gap_of(dir.path(), "uv run --frozen pytest"),
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
        assert_eq!(gap_of(dir.path(), whole), CiGap::default());
        assert_eq!(
            gap_of(repo(&[("uv.lock", "")]).path(), "uv run --frozen pytest"),
            CiGap::default()
        );
        // Without the lock there's no uv setup, so nothing it runs is installed.
        let gap = gap_of(
            repo(&[(".github/workflows/ci.yml", PYTHON_CI)]).path(),
            "uv run --frozen pytest",
        );
        assert!(gap.commands.is_empty());
        assert_eq!(
            left(&gap.left_out),
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
            "on: pull_request\njobs:\n  t:\n    steps:\n      - run: touch {0}\n      - run: touch {0} && ./run-tests.sh\n",
            canary.display()
        );
        std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
        std::fs::write(dir.path().join(".github/workflows/ci.yml"), workflow).unwrap();
        assert!(guess(dir.path()).from_ci);
        assert!(!canary.exists());
    }

    #[test]
    fn checks_the_ci_lets_fail_are_said_and_never_required() {
        let tools = [
            "pytest", "ruff", "mypy", "pyright", "pylint", "flake8", "black",
        ];
        let python = |files: &[(&str, &str)]| {
            let lock = lock(&tools);
            let pyproject = dev_group(&tools);
            let mut all = vec![
                ("uv.lock", lock.as_str()),
                ("pyproject.toml", pyproject.as_str()),
            ];
            all.extend_from_slice(files);
            repo(&all)
        };
        let workflow = r#"
on: pull_request
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: uv run ruff check .
      - run: uv run mypy .
        continue-on-error: true
      - run: uv run ruff format --check .
        continue-on-error: false
  types:
    runs-on: ubuntu-latest
    continue-on-error: true
    steps:
      - run: uv run pyright
  experimental:
    runs-on: ubuntu-latest
    continue-on-error: ${{ matrix.experimental }}
    steps:
      - run: uv run flake8 src
"#;
        let found = guess(python(&[(CI_FILE, workflow)]).path());
        assert_eq!(
            found.ci_checks,
            [
                "uv run --frozen ruff check .",
                "uv run --frozen ruff format --check ."
            ]
        );
        assert_eq!(
            said(&found.ci_left_out),
            all(
                &["uv run mypy .", "uv run pyright", "uv run flake8 src"],
                CiWhy::MayFail
            )
        );
        let check = found.check.unwrap();
        for never in ["mypy", "pyright", "flake8"] {
            assert!(!check.contains(never), "{never} in {check}");
        }

        // The same tool required in another job is still proposed, and not said twice.
        let workflow = "on: pull_request\njobs:\n  a:\n    continue-on-error: true\n    steps:\n      - run: uv run mypy .\n  b:\n    steps:\n      - run: uv run mypy src\n";
        let gap = gap_of(python(&[(CI_FILE, workflow)]).path(), "");
        assert_eq!(gap.commands, ["uv run --frozen mypy src"]);
        assert!(gap.left_out.is_empty(), "{:?}", gap.left_out);

        let pipeline = r#"
lint:
  script:
    - uv run ruff check .
types:
  allow_failure: true
  script:
    - uv run mypy .
pyright:
  allow_failure:
    exit_codes: [137]
  script:
    - uv run pyright
.optional:
  allow_failure: true
slow:
  extends: .optional
  script:
    - uv run pytest -m slow
flake8:
  when: manual
  script:
    - uv run flake8
black:
  rules:
    - allow_failure: true
  script:
    - uv run black --check .
format:
  allow_failure: false
  script:
    - uv run ruff format --check .
"#;
        let gap = gap_of(python(&[(".gitlab-ci.yml", pipeline)]).path(), "");
        assert_eq!(
            gap.commands,
            [
                "uv run --frozen ruff check .",
                "uv run --frozen ruff format --check ."
            ]
        );
        assert_eq!(
            said(&gap.left_out),
            all(
                &[
                    "uv run mypy .",
                    "uv run pyright",
                    "uv run pytest -m slow",
                    "uv run flake8",
                    "uv run black --check .",
                ],
                CiWhy::MayFail
            )
        );
        assert_eq!(gap.from, [".gitlab-ci.yml"]);

        // With nothing else known, what CI lets fail isn't the check, nor what only it prepares.
        let workflow = "\
on: pull_request
jobs:
  t:
    steps:
      - run: zig build
      - run: zig build test
      - run: zig build test-flaky
        continue-on-error: true
  nightly:
    continue-on-error: true
    steps:
      - run: zig fetch --nightly
      - run: zig build test-nightly
";
        let zig = guess(repo(&[("build.zig", ""), (CI_FILE, workflow)]).path());
        assert_eq!(zig.check.as_deref(), Some("zig build test"));
        assert_eq!(zig.setup.as_deref(), Some("zig build"));
        assert_eq!(
            said(&zig.ci_left_out),
            all(
                &["zig build test-flaky", "zig build test-nightly"],
                CiWhy::MayFail
            )
        );
    }

    #[test]
    fn a_tool_told_to_install_what_it_misses_is_left_out() {
        for command in [
            "uv run mypy --install-types --non-interactive src",
            "uv run mypy --install-types=yes src",
            "uv run mypy --install-ty src",
            "go test -mod=mod ./...",
        ] {
            let workflow =
                format!("on: pull_request\njobs:\n  t:\n    steps:\n      - run: {command}\n");
            let dir = repo(&[
                ("uv.lock", &lock(&["mypy"])),
                ("pyproject.toml", &dev_group(&["mypy"])),
                ("go.mod", "module x\n"),
                (CI_FILE, &workflow),
            ]);
            let gap = gap_of(dir.path(), "");
            assert!(gap.commands.is_empty(), "{command}: {:?}", gap.commands);
            assert_eq!(left(&gap.left_out), [command]);
        }
    }

    #[test]
    fn a_runners_options_stay_only_when_the_setup_installs_what_they_name() {
        let tools = ["pytest", "pytest-cov", "ruff"];
        let pyproject = format!(
            "{}lint = [\"ruff\"]\ncoverage = [\"pytest-cov\"]\n[tool.uv]\ndefault-groups = [\"dev\", \"coverage\"]\n",
            dev_group(&tools)
        );
        let gap = |command: &str| {
            let workflow =
                format!("on: pull_request\njobs:\n  t:\n    steps:\n      - run: {command}\n");
            let dir = repo(&[
                ("uv.lock", &lock(&tools)),
                ("pyproject.toml", &pyproject),
                ("package.json", r#"{"devDependencies":{"typescript":"5"}}"#),
                ("package-lock.json", "{}"),
                (CI_FILE, &workflow),
            ]);
            gap_of(dir.path(), "")
        };
        // What they add, what they install from, or where they run, has to come along, and can't.
        for command in [
            "uv run --with pytest-cov pytest --cov",
            "uv run --with=pytest-cov pytest --cov",
            "uv run --extra cov pytest --cov",
            "uv run --all-extras pytest",
            "uv run --group lint ruff check .",
            "uv run --python 3.12 pytest",
            "uv run --directory app pytest",
            "uv run --env-file .env pytest",
            "poetry run -q pytest",
            "COVERAGE_CORE=sysmon uv run pytest --cov",
            "PYTHONPATH=src pytest",
            "npx -p typescript tsc --noEmit",
        ] {
            let gap = gap(command);
            assert!(gap.commands.is_empty(), "{command}: {:?}", gap.commands);
            assert_eq!(left(&gap.left_out), [command]);
        }
        for (command, offline) in [
            ("uv run --frozen pytest", "uv run --frozen pytest"),
            ("uv run --locked pytest -x", "uv run --frozen pytest -x"),
            (
                "uv run --group coverage pytest --cov",
                "uv run --frozen --group coverage pytest --cov",
            ),
            (
                "uv run --group=coverage --frozen pytest --cov",
                "uv run --frozen --group coverage pytest --cov",
            ),
            ("npx --yes tsc --noEmit", "node_modules/.bin/tsc --noEmit"),
        ] {
            let gap = gap(command);
            assert_eq!(gap.commands, [offline], "{command}");
            assert!(gap.left_out.is_empty(), "{command}: {:?}", gap.left_out);
        }
    }

    #[test]
    fn a_chain_with_an_or_is_left_out_whole() {
        let tools = ["ruff", "mypy"];
        for line in [
            "uv run mypy src && echo ok || true",
            "uv run ruff check . && uv run mypy src || true",
        ] {
            let workflow =
                format!("on: pull_request\njobs:\n  t:\n    steps:\n      - run: {line}\n");
            let dir = repo(&[
                ("uv.lock", &lock(&tools)),
                ("pyproject.toml", &dev_group(&tools)),
                (CI_FILE, &workflow),
            ]);
            let gap = gap_of(dir.path(), "");
            assert!(gap.commands.is_empty(), "{line}: {:?}", gap.commands);
            assert_eq!(left(&gap.left_out), [line]);
        }
    }

    #[test]
    fn a_script_run_by_an_interpreter_is_told_apart_from_another() {
        let workflow = "\
on: pull_request
jobs:
  t:
    steps:
      - run: python manage.py test --keepdb
      - run: python manage.py makemigrations --check
      - run: bash scripts/test.sh
      - run: bash scripts/lint.sh
      - run: uv run python scripts/check_types.py
      - run: ./ci/test.sh
";
        let dir = repo(&[(CI_FILE, workflow)]);
        let gap = gap_of(
            dir.path(),
            "python manage.py test && bash scripts/test.sh && ./test.sh",
        );
        assert_eq!(
            left(&gap.left_out),
            [
                "python manage.py makemigrations --check",
                "bash scripts/lint.sh",
                "uv run python scripts/check_types.py",
                "./ci/test.sh",
            ]
        );
        // What the script is told to do is what tells it apart, not which files or how.
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: python manage.py test app.tests --parallel\n";
        let dir = repo(&[(CI_FILE, workflow)]);
        assert_eq!(
            gap_of(dir.path(), "uv run python manage.py test"),
            CiGap::default()
        );
    }

    fn uv_repo(tools: &[&str], files: &[(&str, &str)]) -> tempfile::TempDir {
        let lock = lock(tools);
        let pyproject = dev_group(tools);
        let mut all = vec![
            ("uv.lock", lock.as_str()),
            ("pyproject.toml", pyproject.as_str()),
        ];
        all.extend_from_slice(files);
        repo(&all)
    }

    /// A workflow for pull requests with one job of these steps.
    fn steps(runs: &[&str]) -> String {
        let steps: String = runs
            .iter()
            .map(|run| format!("      - run: {run}\n"))
            .collect();
        format!("on: pull_request\njobs:\n  t:\n    steps:\n{steps}")
    }

    #[test]
    fn a_comment_after_a_command_is_not_part_of_it() {
        let tools = ["pytest", "mypy", "pyright"];
        // The review's probe: joined with `&&` under `sh -c`, the comment hid pyright.
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: |\n          uv run mypy .  # strict settings live in mypy.ini\n          uv run pyright\n";
        let found = guess(uv_repo(&tools, &[(CI_FILE, workflow)]).path());
        assert_eq!(
            found.check.as_deref(),
            Some("uv run --frozen pytest && uv run --frozen mypy . && uv run --frozen pyright")
        );
        assert_eq!(
            found.ci_checks,
            ["uv run --frozen mypy .", "uv run --frozen pyright"]
        );
        // After a quote a `#` may be quoted, so the command is left out rather than cut.
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: |\n          uv run mypy --exclude \"a #b\" .\n";
        let gap = gap_of(uv_repo(&tools, &[(CI_FILE, workflow)]).path(), "");
        assert!(gap.commands.is_empty(), "{:?}", gap.commands);
        assert_eq!(left(&gap.left_out), [r#"uv run mypy --exclude "a #b" ."#]);
    }

    #[test]
    fn what_ci_sets_variables_for_is_left_out() {
        let tools = ["pytest", "mypy", "ruff"];
        let python = |workflow: &str| gap_of(uv_repo(&tools, &[(CI_FILE, workflow)]).path(), "");
        // The review's probe, with variables for the workflow, the job and the step.
        let workflow = "env:\n  MYPYPATH: stubs\non: pull_request\njobs:\n  t:\n    env:\n      FOO: bar\n    steps:\n      - run: uv run mypy .\n        env:\n          MYPY_CACHE_DIR: /x\n";
        let found = guess(uv_repo(&tools, &[(CI_FILE, workflow)]).path());
        assert!(found.ci_checks.is_empty(), "{:?}", found.ci_checks);
        assert_eq!(said(&found.ci_left_out), [("uv run mypy .", CiWhy::Env)]);
        for workflow in [
            "env:\n  MYPYPATH: stubs\non: pull_request\njobs:\n  t:\n    steps:\n      - run: uv run mypy .\n",
            "on: pull_request\njobs:\n  t:\n    env:\n      MYPYPATH: stubs\n    steps:\n      - run: uv run mypy .\n",
            "on: pull_request\njobs:\n  t:\n    steps:\n      - run: uv run mypy .\n        env:\n          MYPYPATH: stubs\n",
            &steps(&["MYPYPATH=stubs uv run mypy ."]),
        ] {
            let gap = python(workflow);
            assert!(gap.commands.is_empty(), "{workflow}");
            assert_eq!(gap.left_out.len(), 1, "{workflow}");
            assert_eq!(gap.left_out[0].why, CiWhy::Env, "{workflow}");
        }
        // The review's other probe: warnings made errors for clippy.
        let workflow = "on: [push, pull_request]\nenv:\n  RUSTFLAGS: -Dwarnings\njobs:\n  clippy:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: cargo clippy --all-targets --all-features\n";
        let rust = guess(
            repo(&[
                ("Cargo.toml", "[package]\nname=\"a\"\n"),
                (CI_FILE, workflow),
            ])
            .path(),
        );
        assert_eq!(rust.check.as_deref(), Some("cargo test"));
        assert_eq!(
            said(&rust.ci_left_out),
            [("cargo clippy --all-targets --all-features", CiWhy::Env)]
        );
        // Colors and the like change nothing that's checked.
        let workflow = "on: pull_request\nenv:\n  CARGO_TERM_COLOR: always\n  CI: true\njobs:\n  clippy:\n    env:\n      TERM: xterm\n    steps:\n      - run: cargo clippy --all-targets\n";
        let rust = guess(
            repo(&[
                ("Cargo.toml", "[package]\nname=\"a\"\n"),
                (CI_FILE, workflow),
            ])
            .path(),
        );
        assert_eq!(rust.ci_checks, ["cargo clippy --all-targets"]);

        // GitLab's variables, for the pipeline, the job or a template, count the same.
        let pipeline = |text: &str| gap_of(uv_repo(&tools, &[(".gitlab-ci.yml", text)]).path(), "");
        for text in [
            "variables:\n  MYPYPATH: stubs\nlint:\n  script:\n    - uv run ruff check .\n",
            "lint:\n  variables:\n    MYPYPATH: stubs\n  script:\n    - uv run ruff check .\n",
            ".vars:\n  variables:\n    MYPYPATH: stubs\nlint:\n  extends: .vars\n  script:\n    - uv run ruff check .\n",
        ] {
            assert_eq!(
                said(&pipeline(text).left_out),
                [("uv run ruff check .", CiWhy::Env)],
                "{text}"
            );
        }
        let colors =
            "variables:\n  FORCE_COLOR: \"1\"\nlint:\n  script:\n    - uv run ruff check .\n";
        assert_eq!(pipeline(colors).commands, ["uv run --frozen ruff check ."]);

        // With nothing else known, a check run with variables isn't the guess either.
        let workflow = "on: pull_request\nenv:\n  ZIG_FLAGS: -Drelease\njobs:\n  t:\n    steps:\n      - run: zig build test\n";
        let zig = guess(repo(&[("build.zig", ""), (CI_FILE, workflow)]).path());
        assert_eq!(zig.check, None);
        assert_eq!(said(&zig.ci_left_out), [("zig build test", CiWhy::Env)]);
        // Nothing was taken from it, so it isn't said to be where the guess came from.
        assert!(!zig.from_ci && zig.from.is_empty(), "{:?}", zig.from);
    }

    #[test]
    fn every_tool_on_the_list_is_seen_whatever_its_name_says() {
        // The review's probes: none of these names says "test" or "lint".
        let go = guess(
            repo(&[
                ("go.mod", "module x\n"),
                (CI_FILE, &steps(&["go vet ./...", "go test ./..."])),
            ])
            .path(),
        );
        assert_eq!(go.check.as_deref(), Some("go test ./... && go vet ./..."));
        assert_eq!(go.ci_checks, ["go vet ./..."]);
        let tools = ["pytest", "pylint", "isort"];
        let python = guess(
            uv_repo(
                &tools,
                &[(CI_FILE, &steps(&["uv run pylint src", "uv run isort -c ."]))],
            )
            .path(),
        );
        assert_eq!(
            python.ci_checks,
            ["uv run --frozen pylint src", "uv run --frozen isort -c ."]
        );
        let node = guess(
            repo(&[
                (
                    "package.json",
                    r#"{"scripts":{"test":"jest"},"devDependencies":{"jest":"29","prettier":"3"}}"#,
                ),
                ("package-lock.json", "{}"),
                (CI_FILE, &steps(&["npx prettier -c ."])),
            ])
            .path(),
        );
        assert_eq!(node.ci_checks, ["node_modules/.bin/prettier -c ."]);
    }

    #[test]
    fn only_what_runs_for_every_pull_request_is_proposed() {
        let tools = ["pytest", "ruff"];
        let gap = |workflow: &str| gap_of(uv_repo(&tools, &[(CI_FILE, workflow)]).path(), "");
        let job = "jobs:\n  t:\n    steps:\n      - run: uv run ruff check .\n";
        // The review's probe: a job that never runs, in a workflow that only runs on a schedule.
        let workflow = "on: schedule\njobs:\n  t:\n    if: false\n    steps:\n      - run: uv run ruff check .\n";
        assert_eq!(gap(workflow), CiGap::default());
        // A workflow that never runs for a pull request's commits isn't its CI.
        for on in [
            "on: schedule",
            "on: [workflow_dispatch, release]",
            "on:\n  push:\n    branches: [main]",
            "on:\n  push:\n    tags: ['v*']",
            "name: no trigger",
        ] {
            assert_eq!(gap(&format!("{on}\n{job}")), CiGap::default(), "{on}");
        }
        for on in [
            "on: pull_request",
            "on: push",
            "on: [push, pull_request]",
            "on:\n  pull_request:\n    branches: [main]",
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened]",
            "on:\n  push:\n    branches: ['**']",
            "on:\n  push:\n    branches-ignore: [gh-pages]",
            "on:\n  push:\n    branches: [main]\n  pull_request:",
        ] {
            assert_eq!(
                gap(&format!("{on}\n{job}")).commands,
                ["uv run --frozen ruff check ."],
                "{on}"
            );
        }
        // Only for some files, or some kinds of event: not every time.
        for on in [
            "on:\n  pull_request:\n    paths: ['src/**']",
            "on:\n  pull_request:\n    types: [labeled]",
            "on:\n  push:\n    paths-ignore: ['docs/**']",
        ] {
            assert_eq!(
                said(&gap(&format!("{on}\n{job}")).left_out),
                [("uv run ruff check .", CiWhy::Sometimes)],
                "{on}"
            );
        }
        // A job or a step under a condition. The review's probe is the step's.
        for workflow in [
            "on: pull_request\njobs:\n  t:\n    if: false\n    steps:\n      - run: uv run ruff check .\n",
            "on: pull_request\njobs:\n  t:\n    if: github.event_name == 'push'\n    steps:\n      - run: uv run ruff check .\n",
            "on: pull_request\njobs:\n  t:\n    steps:\n      - if: github.event_name == 'schedule'\n        run: uv run ruff check .\n",
        ] {
            assert_eq!(
                said(&gap(workflow).left_out),
                [("uv run ruff check .", CiWhy::Sometimes)],
                "{workflow}"
            );
        }
        // Conditions that hold every time.
        for condition in ["always()", "success()", "${{ always() }}", "true"] {
            let workflow = format!(
                "on: pull_request\njobs:\n  t:\n    if: {condition}\n    steps:\n      - run: uv run ruff check .\n"
            );
            assert_eq!(
                gap(&workflow).commands,
                ["uv run --frozen ruff check ."],
                "{condition}"
            );
        }

        // GitLab's rules, only and except. The review's probe is the schedule rule.
        let pipeline = |text: &str| gap_of(uv_repo(&tools, &[(".gitlab-ci.yml", text)]).path(), "");
        for text in [
            "lint:\n  rules:\n    - if: $CI_PIPELINE_SOURCE == \"schedule\"\n  script:\n    - uv run ruff check .\n",
            "lint:\n  rules:\n    - changes: [src/**]\n  script:\n    - uv run ruff check .\n",
            "lint:\n  only: [main]\n  script:\n    - uv run ruff check .\n",
            "lint:\n  except: [tags]\n  script:\n    - uv run ruff check .\n",
            "lint:\n  when: never\n  script:\n    - uv run ruff check .\n",
        ] {
            assert_eq!(
                said(&pipeline(text).left_out),
                [("uv run ruff check .", CiWhy::Sometimes)],
                "{text}"
            );
        }
        let always =
            "lint:\n  rules:\n    - when: on_success\n  script:\n    - uv run ruff check .\n";
        assert_eq!(pipeline(always).commands, ["uv run --frozen ruff check ."]);
    }

    #[test]
    fn what_follows_set_plus_e_may_fail() {
        let tools = ["pytest", "mypy", "ruff"];
        let gap = |workflow: &str| {
            gap_of(
                uv_repo(&tools, &[(CI_FILE, workflow)]).path(),
                "uv run --frozen pytest",
            )
        };
        // The review's probe.
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: |\n          set +e\n          uv run mypy .\n          uv run pytest\n";
        let found = gap(workflow);
        assert!(found.commands.is_empty(), "{:?}", found.commands);
        assert_eq!(said(&found.left_out), [("uv run mypy .", CiWhy::MayFail)]);
        // Until `set -e` again, and only in its own step.
        let workflow = "on: pull_request\njobs:\n  t:\n    steps:\n      - run: |\n          set +o errexit\n          uv run mypy .\n          set -e\n          uv run ruff check .\n      - run: uv run ruff format --check .\n";
        let found = gap(workflow);
        assert_eq!(
            found.commands,
            [
                "uv run --frozen ruff check .",
                "uv run --frozen ruff format --check ."
            ]
        );
        assert_eq!(said(&found.left_out), [("uv run mypy .", CiWhy::MayFail)]);
        // In GitLab, before_script and script are one shell.
        let pipeline = "lint:\n  before_script:\n    - set +e\n  script:\n    - uv run mypy .\n";
        let found = gap_of(
            uv_repo(&tools, &[(".gitlab-ci.yml", pipeline)]).path(),
            "uv run --frozen pytest",
        );
        assert_eq!(said(&found.left_out), [("uv run mypy .", CiWhy::MayFail)]);
    }

    #[test]
    fn a_job_that_installs_more_than_the_setup_has_its_checks_left_out() {
        let tools = ["pytest", "mypy"];
        let gap = |workflow: &str| {
            gap_of(
                uv_repo(&tools, &[(CI_FILE, workflow)]).path(),
                "uv run --frozen pytest",
            )
        };
        // The review's probe: the stubs aren't in the lock, so mypy fails in Keel without them.
        let found = gap(&steps(&["uv pip install types-requests", "uv run mypy ."]));
        assert!(found.commands.is_empty(), "{:?}", found.commands);
        assert_eq!(said(&found.left_out), [("uv run mypy .", CiWhy::Installs)]);
        for install in [
            "pip install types-requests",
            "python -m pip install -r requirements-dev.txt",
            "sudo apt-get install -y libpq-dev",
            "npm install -g pyright",
            "cargo install cargo-nextest",
            "go install golang.org/x/tools/cmd/goimports@latest",
            "uv tool install ruff",
            "pipx install ruff",
            "uv add types-requests",
            "uv sync --group lint",
            "uv sync --python 3.12",
        ] {
            assert_eq!(
                said(&gap(&steps(&[install, "uv run mypy ."])).left_out),
                [("uv run mypy .", CiWhy::Installs)],
                "{install}"
            );
        }
        // What the setup installs anyway, or only the tools around it.
        for same in [
            "uv sync --frozen",
            "uv sync",
            "pip install --upgrade pip",
            "python -m pip install -U pip",
            "rustup component add clippy",
        ] {
            assert_eq!(
                gap(&steps(&[same, "uv run mypy ."])).commands,
                ["uv run --frozen mypy ."],
                "{same}"
            );
        }
        // Another job's install is that job's.
        let workflow = "on: pull_request\njobs:\n  a:\n    steps:\n      - run: pip install types-requests\n      - run: uv run mypy src\n  b:\n    steps:\n      - run: uv run mypy .\n";
        assert_eq!(gap(workflow).commands, ["uv run --frozen mypy ."]);
    }

    #[test]
    fn a_gitlab_template_from_another_file_leaves_its_job_out() {
        let tools = ["pytest", "ruff"];
        // The review's probe: what lets the job fail is in a file Keel doesn't read.
        let pipeline = "include:\n  - local: ci/t.yml\nlint:\n  extends: .may-fail\n  script:\n    - uv run ruff check .\n";
        let gap = gap_of(
            uv_repo(
                &tools,
                &[
                    (".gitlab-ci.yml", pipeline),
                    ("ci/t.yml", ".may-fail:\n  allow_failure: true\n"),
                ],
            )
            .path(),
            "",
        );
        assert!(gap.commands.is_empty(), "{:?}", gap.commands);
        assert_eq!(
            said(&gap.left_out),
            [("uv run ruff check .", CiWhy::Template)]
        );
    }

    #[test]
    fn a_gitlab_jobs_own_settings_win_over_its_templates() {
        let tools = ["pytest", "ruff"];
        let gap = |text: &str| gap_of(uv_repo(&tools, &[(".gitlab-ci.yml", text)]).path(), "");
        let proposed = ["uv run --frozen ruff check ."];
        // The review's probe: the job says when it runs, so the template's manual doesn't count.
        let text = ".base:\n  when: manual\nlint:\n  extends: .base\n  when: on_success\n  script:\n    - uv run ruff check .\n";
        assert_eq!(gap(text).commands, proposed);
        // Its own rules replace the template's.
        let text = ".base:\n  rules:\n    - if: $CI_COMMIT_TAG\nlint:\n  extends: .base\n  rules:\n    - when: on_success\n  script:\n    - uv run ruff check .\n";
        assert_eq!(gap(text).commands, proposed);
        // What it doesn't say comes from its templates, the last one first.
        let text = ".a:\n  allow_failure: true\n.b:\n  allow_failure: false\nlint:\n  extends: [.a, .b]\n  script:\n    - uv run ruff check .\n";
        assert_eq!(gap(text).commands, proposed);
        let text = ".a:\n  allow_failure: false\n.b:\n  allow_failure: true\nlint:\n  extends: [.a, .b]\n  script:\n    - uv run ruff check .\n";
        assert_eq!(
            said(&gap(text).left_out),
            [("uv run ruff check .", CiWhy::MayFail)]
        );
    }

    #[test]
    fn format_as_a_word_is_not_a_formatter() {
        // The review's probes.
        let package = r#"{"scripts":{"test":"jest"},"devDependencies":{"jest":"1","eslint":"9"}}"#;
        let node = guess(
            repo(&[
                ("package.json", package),
                ("package-lock.json", "{}"),
                (CI_FILE, &steps(&["npx eslint . --format stylish"])),
            ])
            .path(),
        );
        assert_eq!(
            node.ci_checks,
            ["node_modules/.bin/eslint . --format stylish"]
        );
        let unknown = guess(repo(&[(CI_FILE, &steps(&["just fmt && just test"]))]).path());
        assert_eq!(unknown.check.as_deref(), Some("just test"));
        // A tool that only checks takes files, and a file can be named `format`.
        let tools = ["pytest", "mypy"];
        let python =
            guess(uv_repo(&tools, &[(CI_FILE, &steps(&["uv run mypy format.py"]))]).path());
        assert_eq!(python.ci_checks, ["uv run --frozen mypy format.py"]);
        let unknown = guess(repo(&[(CI_FILE, &steps(&["uv run mypy format.py"]))]).path());
        assert_eq!(unknown.check.as_deref(), Some("uv run mypy format.py"));
        // What does rewrite the code is never a check, nor left out as one.
        for rewrites in [
            "cargo fmt --all",
            "npm run format",
            "just fmt",
            "ruff format .",
            "uv run ruff format .",
            "npx prettier --write .",
            "uv run black .",
            "gofmt -w .",
        ] {
            let dir = uv_repo(
                &["pytest", "ruff", "black"],
                &[
                    ("Cargo.toml", "[package]\nname=\"a\"\n"),
                    (CI_FILE, &steps(&[rewrites])),
                ],
            );
            assert_eq!(gap_of(dir.path(), ""), CiGap::default(), "{rewrites}");
        }
    }

    #[test]
    fn a_tool_run_in_a_version_of_its_own_is_left_out() {
        let tools = ["pytest", "ruff"];
        // The review's probe: uvx runs ruff apart from the project, in whatever version it finds.
        for command in ["uvx ruff check .", "pipx run ruff check ."] {
            let gap = gap_of(uv_repo(&tools, &[(CI_FILE, &steps(&[command]))]).path(), "");
            assert!(gap.commands.is_empty(), "{command}: {:?}", gap.commands);
            assert_eq!(said(&gap.left_out), [(command, CiWhy::Offline)]);
        }
    }

    #[test]
    fn what_another_job_lets_keel_propose_is_not_left_out_too() {
        let tools = ["pytest", "ruff"];
        // The review's probe.
        let workflow = "on: pull_request\njobs:\n  a:\n    steps:\n      - run: uv run ruff check . | tee out.txt\n  b:\n    steps:\n      - run: uv run ruff check .\n";
        let gap = gap_of(uv_repo(&tools, &[(CI_FILE, workflow)]).path(), "");
        assert_eq!(gap.commands, ["uv run --frozen ruff check ."]);
        assert!(gap.left_out.is_empty(), "{:?}", gap.left_out);
    }

    #[test]
    fn what_the_projects_own_setup_installs_is_what_a_command_can_use() {
        // The review's probe: mypy is only in the `test` group, which the guessed setup installs.
        let pyproject = "[dependency-groups]\ntest = [\"pytest\", \"mypy\"]\n";
        let workflow = steps(&["uv run --group test mypy ."]);
        let dir = repo(&[
            ("uv.lock", &lock(&["pytest", "mypy"])),
            ("pyproject.toml", pyproject),
            (CI_FILE, &workflow),
        ]);
        let found = guess(dir.path());
        assert_eq!(
            found.setup.as_deref(),
            Some("uv sync --frozen --group test")
        );
        assert_eq!(found.ci_checks, ["uv run --frozen --group test mypy ."]);
        // A setup saved without that group doesn't install mypy, and no setup installs nothing.
        for setup in ["uv sync --frozen", "", "make deps"] {
            let gap = ci_gap(dir.path(), setup, "uv run --frozen pytest");
            assert!(gap.commands.is_empty(), "{setup}: {:?}", gap.commands);
            assert_eq!(
                said(&gap.left_out),
                [("uv run --group test mypy .", CiWhy::Offline)],
                "{setup}"
            );
        }
        let gap = ci_gap(
            dir.path(),
            "uv sync --frozen --all-groups",
            "uv run --frozen pytest",
        );
        assert_eq!(gap.commands, ["uv run --frozen --group test mypy ."]);
    }

    #[test]
    fn the_yaml_key_on_is_read_as_a_word() {
        let workflow: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("on: pull_request\njobs: {}\n").unwrap();
        assert_eq!(workflow["on"].as_str(), Some("pull_request"));
    }
}
