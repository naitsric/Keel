//! Which of a CI's checks Keel can run with no network, and what a check already runs. A command
//! is proposed only when it's one simple command, its tool and mode are on a short list of checks
//! that work offline, and the project's own setup installs that tool. Everything else is only
//! mentioned. Only reads the repo, and each file once.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::{
    CiWhy, dotnet, is_file, is_plain_name, node_tool, normalized, poetry_installs, read,
    requirement_name, scripts, uv_sync,
};

/// Programs whose first word after them says what they do: `cargo clippy`, `ruff format`.
const SUBCOMMANDS: [&str; 19] = [
    "cargo", "go", "ruff", "mix", "dotnet", "make", "zig", "deno", "biome", "gradle", "gradlew",
    "mvn", "mvnw", "swift", "flutter", "dart", "composer", "bun", "rake",
];
/// Tools that check one way whatever they're given: `mypy src` and `mypy .` are one check.
const ONE_MODE: [&str; 16] = [
    "pytest", "mypy", "pyright", "flake8", "pylint", "black", "isort", "eslint", "tsc", "prettier",
    "jest", "vitest", "mocha", "rspec", "phpunit", "rubocop",
];
/// For anything else, the first words that aren't options say what it's told to do:
/// `python manage.py test` and `python manage.py makemigrations` are two checks, and so are
/// `bash scripts/test.sh` and `bash scripts/lint.sh`.
const MODE_WORDS: usize = 2;
/// Options of `npx` that only say whether it may download what isn't installed. The offline form
/// runs what is.
const NPX_ASKS: [&str; 4] = ["--yes", "-y", "--no", "--no-install"];
/// Python's packaging tools themselves, which a CI upgrades before installing anything.
const PACKAGING: [&str; 4] = ["pip", "setuptools", "wheel", "uv"];
/// The machine's package managers.
const SYSTEM_PACKAGES: [&str; 10] = [
    "apt", "apt-get", "brew", "choco", "yum", "dnf", "zypper", "snap", "port", "apk",
];
/// A binary whose package has another name.
const PACKAGES: [(&str, &str); 3] = [
    ("tsc", "typescript"),
    ("biome", "@biomejs/biome"),
    ("ng", "@angular/cli"),
];
/// Options of `uv run` and `poetry run` that take a value, when it isn't written with `=`.
const VALUED: [&str; 25] = [
    "--with",
    "--with-editable",
    "--with-requirements",
    "--extra",
    "--extras",
    "--no-extra",
    "--group",
    "--only-group",
    "--no-group",
    "--package",
    "--python",
    "--env-file",
    "--index",
    "--default-index",
    "--index-url",
    "--extra-index-url",
    "--find-links",
    "--directory",
    "--project",
    "--config-file",
    "--cache-dir",
    "-p",
    "-C",
    "-P",
    "-E",
];
/// Options of `npx` that take a value.
const NPX_VALUED: [&str; 2] = ["-p", "--package"];
/// Scripts and Makefile targets read in all, to tell what a check runs.
const MOST_EXPANSIONS: usize = 50;
/// Lines read of each.
const MOST_LINES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Called {
    /// A Python package's command, or its module with `module`.
    Python { tool: String, module: bool },
    /// One of the scripts in `package.json`.
    Script(String),
    /// A binary of a JavaScript package.
    Node(String),
    /// Anything else, as the command names it.
    Program(String),
}

struct Call {
    called: Called,
    args: Vec<String>,
    /// What runs it, like `uv run` or `npx`, and the options given to that, like `--with pytest-cov`.
    runner: String,
    options: Vec<String>,
    /// `FOO=1` in front sets a variable for it.
    sets_env: bool,
}

/// A tool and what it's told to do. For a script, the tool is `script`, since every JavaScript tool
/// runs the same ones.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Sig {
    tool: String,
    mode: String,
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A runner's options, and the words after them: `--flag`, `--flag=value`, and `--flag value` for
/// those that take one.
fn split_options<'a, 'b>(words: &'a [&'b str], valued: &[&str]) -> (&'a [&'b str], &'a [&'b str]) {
    let mut at = 0;
    while let Some(word) = words.get(at) {
        if !word.starts_with('-') {
            break;
        }
        at += if valued.contains(word) { 2 } else { 1 };
    }
    words.split_at(at.min(words.len()))
}

/// What a runner runs, and the options it's given.
fn run_by(
    runner: &str,
    words: &[&str],
    valued: &[&str],
    then: fn(&[&str]) -> Option<Call>,
) -> Option<Call> {
    let (options, rest) = split_options(words, valued);
    let mut call = then(rest)?;
    call.runner = runner.to_owned();
    call.options = options.iter().map(|option| (*option).to_owned()).collect();
    Some(call)
}

fn sets_variable(word: &str) -> bool {
    word.contains('=') && !word.starts_with('-')
}

/// mypy's `--install-types` installs the stubs it misses with pip, even cut short like
/// `--install`, since Python's tools take that. go's `-mod=mod` downloads what go.mod lacks.
fn fetches(word: &str) -> bool {
    let flag = word.split('=').next().unwrap_or(word);
    (flag.len() >= "--ins".len() && "--install-types".starts_with(flag))
        || matches!(word, "-mod=mod" | "--mod=mod")
}

fn python(words: &[&str]) -> Option<Call> {
    match words {
        ["python" | "python3", "-m", tool, args @ ..] => Some(Call::new(
            Called::Python {
                tool: (*tool).to_owned(),
                module: true,
            },
            args,
        )),
        [tool, args @ ..] => Some(Call::new(
            Called::Python {
                tool: base(tool).to_owned(),
                module: false,
            },
            args,
        )),
        [] => None,
    }
}

fn node(words: &[&str]) -> Option<Call> {
    let [bin, args @ ..] = words else {
        return None;
    };
    Some(Call::new(Called::Node((*bin).to_owned()), args))
}

impl Call {
    fn new(called: Called, args: &[&str]) -> Self {
        Self {
            called,
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            runner: String::new(),
            options: Vec::new(),
            sets_env: false,
        }
    }

    /// What its first words say it calls. `FOO=1` in front only sets a variable for it.
    fn parse(command: &str) -> Option<Self> {
        let all: Vec<&str> = command
            .trim_matches(['(', ')'])
            .split_whitespace()
            .collect();
        let words: Vec<&str> = all
            .iter()
            .copied()
            .skip_while(|word| sets_variable(word))
            .collect();
        let mut call = Self::called(&words)?;
        call.sets_env = words.len() < all.len();
        Some(call)
    }

    fn called(words: &[&str]) -> Option<Self> {
        let script = |name: &str, args: &[&str]| Some(Self::new(Called::Script(name.into()), args));
        match words {
            [
                runner @ ("uv" | "poetry" | "pipenv" | "pdm" | "pipx"),
                "run",
                rest @ ..,
            ] => run_by(&format!("{runner} run"), rest, &VALUED, python),
            ["uvx", rest @ ..] => run_by("uvx", rest, &VALUED, python),
            [
                "python" | "python3" | ".venv/bin/python",
                "-m",
                tool,
                args @ ..,
            ] => python(&[&["python", "-m", *tool][..], args].concat()),
            [runner @ ("npx" | "bunx"), rest @ ..] => {
                // Its own `-c` runs a line of its own. The tool's `-c` is the tool's.
                let (options, _) = split_options(rest, &NPX_VALUED);
                if options
                    .iter()
                    .any(|word| *word == "-c" || *word == "--call")
                {
                    return None;
                }
                run_by(runner, rest, &NPX_VALUED, node)
            }
            ["pnpm" | "yarn" | "bun", "exec" | "x" | "dlx", rest @ ..] => node(rest),
            ["npm", "test" | "t", args @ ..] => script("test", args),
            [
                "npm" | "pnpm" | "yarn" | "bun",
                "run" | "run-script",
                name,
                args @ ..,
            ] => script(name, args),
            ["pnpm" | "yarn", name, args @ ..] if !name.starts_with('-') => script(name, args),
            [first, args @ ..] => {
                let path = first.trim_start_matches("./");
                if path.starts_with("node_modules/.bin/") {
                    node(&[&[base(path)][..], args].concat())
                } else if path.starts_with(".venv/bin/") {
                    python(&[&[base(path)][..], args].concat())
                } else {
                    Some(Self::new(Called::Program((*first).to_owned()), args))
                }
            }
            [] => None,
        }
    }

    fn name(&self) -> String {
        match &self.called {
            Called::Python { tool, .. } => tool.to_ascii_lowercase(),
            Called::Script(name) => name.clone(),
            Called::Node(bin) => bin.to_ascii_lowercase(),
            Called::Program(program) => base(program).to_ascii_lowercase(),
        }
    }

    /// Its first word that isn't a flag or a toolchain, like `fmt` in `cargo +nightly fmt --check`.
    fn subcommand(&self) -> &str {
        self.args
            .iter()
            .find(|arg| !arg.starts_with('-') && !arg.starts_with('+'))
            .map_or("", String::as_str)
    }

    fn has(&self, flag: &str) -> bool {
        self.args.iter().any(|arg| arg == flag)
    }

    fn sig(&self) -> Sig {
        if let Called::Script(name) = &self.called {
            return Sig {
                tool: "script".to_owned(),
                mode: name.clone(),
            };
        }
        let tool = self.name();
        if SUBCOMMANDS.contains(&tool.as_str()) {
            let mode = self.subcommand().to_owned();
            return Sig { tool, mode };
        }
        if ONE_MODE.contains(&tool.as_str()) {
            return Sig {
                tool,
                mode: String::new(),
            };
        }
        let mode: Vec<&str> = self
            .args
            .iter()
            .filter(|arg| !arg.starts_with('-'))
            .take(MODE_WORDS)
            .map(String::as_str)
            .collect();
        // A script is where it is: `ci/test.sh` isn't `scripts/test.sh`.
        let tool = match &self.called {
            Called::Program(program) if program.contains('/') => {
                program.trim_start_matches("./").to_owned()
            }
            _ => tool,
        };
        Sig {
            tool,
            mode: mode.join(" "),
        }
    }
}

/// What a line runs one after the other: split at new lines, `;` and `&&`. A line in parentheses
/// as a whole runs the same. A chain with `||` is one: in `a && b || c`, `c` runs when `a` fails,
/// so `a` doesn't have to pass.
pub(super) fn simple_commands(line: &str) -> Vec<&str> {
    let line = line.trim();
    let line = match line
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
    {
        Some(inner) if !inner.contains(['(', ')']) => inner,
        _ => line,
    };
    let mut commands = Vec::new();
    for part in line.split(['\n', ';']) {
        if part.contains("||") {
            commands.push(part);
        } else {
            commands.extend(part.split("&&"));
        }
    }
    commands
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

/// Nothing a shell does besides running it: no pipe, redirection, substitution, variable,
/// background, `||`, subshell, escape, home folder or comment, and quotes that close. Chained after
/// another with `&&`, a comment would hide every command after it.
fn is_plain(command: &str) -> bool {
    !command.contains(['|', '<', '>', '$', '`', '&', '(', ')', ';', '\\', '~', '#'])
        && command.matches('"').count().is_multiple_of(2)
        && command.matches('\'').count().is_multiple_of(2)
}

#[derive(Clone, Copy)]
enum Installer {
    Python,
    Node,
    /// The stack's own program: cargo, go, mix or dotnet.
    Own,
}

/// The checks Keel proposes from a CI: each lints, checks types or tests, and works offline once
/// what it needs is installed. In a mode that only reads the code.
fn on_the_list(call: &Call) -> Option<Installer> {
    use Installer::{Node, Own, Python};
    let sub = call.subcommand();
    let watches = call.args.iter().any(|arg| arg.starts_with("--watch"));
    let listed = match call.name().as_str() {
        "ruff" => sub == "check" || (sub == "format" && call.has("--check")),
        "black" => call.has("--check"),
        "isort" => call.has("--check-only") || call.has("--check") || call.has("-c"),
        "flake8" | "pylint" | "mypy" | "pyright" | "pytest" => true,
        "eslint" => true,
        "tsc" => call.has("--noEmit"),
        "prettier" => call.has("--check") || call.has("-c"),
        "jest" => !watches,
        "vitest" => sub == "run" && !watches,
        "cargo" => {
            matches!(sub, "clippy" | "test" | "check") || (sub == "fmt" && call.has("--check"))
        }
        "go" => matches!(sub, "vet" | "test"),
        "mix" => sub == "test" || (sub == "format" && call.has("--check-formatted")),
        "dotnet" => sub == "test" || (sub == "format" && call.has("--verify-no-changes")),
        _ => false,
    };
    let installer = match call.name().as_str() {
        "eslint" | "tsc" | "prettier" | "jest" | "vitest" => Node,
        "cargo" | "go" | "mix" | "dotnet" => Own,
        _ => Python,
    };
    listed.then_some(installer)
}

/// Whether a command runs a tool on the list, whatever its name says, like `go vet` or `pylint`.
pub(super) fn listed(command: &str) -> bool {
    Call::parse(command).is_some_and(|call| on_the_list(&call).is_some())
}

/// Whether a command rewrites the code: a formatter that isn't told only to check, like
/// `cargo fmt`, `ruff format .`, `black .` or `prettier --write`, or a script or recipe named for
/// one, like `npm run format` or `just fmt`. `eslint --format stylish` only picks how it prints.
pub(super) fn rewrites(command: &str) -> bool {
    let Some(call) = Call::parse(command) else {
        return false;
    };
    let only_checks = call.args.iter().any(|arg| {
        let flag = arg.trim_start_matches('-');
        arg.starts_with('-')
            && (flag.starts_with("check")
                || flag.starts_with("verify")
                || flag.starts_with("set-exit-if-changed")
                || matches!(flag, "diff" | "dry-run" | "l" | "list-different"))
    });
    let formats = |name: &str| {
        let stem = name.split(['.', ':', '-', '_']).next().unwrap_or(name);
        matches!(stem, "fmt" | "format") && !name.contains("check") && !name.contains("verify")
    };
    match &call.called {
        Called::Script(name) => formats(name),
        _ => match call.name().as_str() {
            "black" | "isort" | "rustfmt" => !only_checks && !call.has("-c"),
            "prettier" => call.has("--write") || call.has("-w"),
            "gofmt" | "goimports" => call.has("-w"),
            // A tool that only checks takes files where others take a subcommand.
            name => {
                let told = !ONE_MODE.contains(&name) && formats(call.subcommand());
                (told || formats(name)) && !only_checks
            }
        },
    }
}

/// The commands of a setup, each as its words, without the variables it sets.
fn steps(setup: &str) -> Vec<Vec<&str>> {
    simple_commands(setup)
        .into_iter()
        .map(|command| {
            command
                .split_whitespace()
                .skip_while(|word| sets_variable(word))
                .collect()
        })
        .collect()
}

/// A path in the repo, not one that leaves it.
fn in_repo(path: &str) -> bool {
    !path.starts_with('/') && path.split('/').all(|part| !part.is_empty() && part != "..")
}

/// The project itself, which installing changes nothing a check can use.
fn is_the_project(what: &str) -> bool {
    matches!(what, "." | "./") || what.starts_with(".[")
}

/// What the project's own Python setup installs, and how it runs that. Only what the setup itself
/// says counts: a setup that doesn't install the project's packages, or is told something this
/// doesn't follow, installs nothing a check can count on.
struct PythonSetup {
    /// `uv run --frozen` or `poetry run`. None for the `.venv` that pip installs into.
    run: Option<&'static str>,
    installs: HashSet<String>,
    /// For uv: its project file, and the groups and extras the setup installs.
    uv: Option<(String, HashSet<String>, HashSet<String>)>,
    /// For pip: the lists of requirements the setup installs.
    pip_lists: Vec<String>,
}

impl PythonSetup {
    /// One tool per project, in the order `python` picks it.
    fn of(root: &Path, setup: &[Vec<&str>]) -> Option<Self> {
        let pyproject = || read(root, "pyproject.toml").unwrap_or_default();
        if read(root, "uv.lock").is_some() {
            let args = setup.iter().find_map(|words| match words.as_slice() {
                ["uv", "sync", args @ ..] => Some(args),
                _ => None,
            })?;
            let pyproject = pyproject();
            let sync = uv_sync(&pyproject, args)?;
            return Some(Self {
                run: Some("uv run --frozen"),
                installs: sync.names,
                uv: Some((pyproject, sync.groups, sync.extras)),
                pip_lists: Vec::new(),
            });
        }
        if read(root, "poetry.lock").is_some() && is_file(root, "pyproject.toml") {
            let args = setup.iter().find_map(|words| match words.as_slice() {
                ["poetry", "install", args @ ..] => Some(args),
                _ => None,
            })?;
            let understood = |arg: &&str| {
                matches!(
                    *arg,
                    "--no-root" | "--sync" | "-q" | "--quiet" | "-n" | "--no-interaction"
                )
            };
            return args.iter().all(understood).then(|| Self {
                run: Some("poetry run"),
                installs: poetry_installs(&pyproject()),
                uv: None,
                pip_lists: Vec::new(),
            });
        }
        let mut lists = Vec::new();
        for words in setup {
            let args = match words.as_slice() {
                [".venv/bin/pip" | ".venv/bin/pip3", "install", args @ ..]
                | [
                    ".venv/bin/python" | ".venv/bin/python3",
                    "-m",
                    "pip",
                    "install",
                    args @ ..,
                ] => args,
                _ => continue,
            };
            let mut args = args.iter();
            while let Some(arg) = args.next() {
                if matches!(*arg, "-r" | "--requirement") {
                    lists.extend(args.next().map(|list| (*list).to_owned()));
                }
            }
        }
        let installs: HashSet<String> = lists
            .iter()
            .filter(|list| in_repo(list))
            .filter_map(|list| read(root, list))
            .flat_map(|text| text.lines().map(requirement_name).collect::<Vec<_>>())
            .collect();
        (!lists.is_empty()).then_some(Self {
            run: None,
            installs,
            uv: None,
            pip_lists: lists,
        })
    }

    /// The options a CI gives its runner, as this setup's runner takes them, or None when one can't
    /// come along: it may fetch, or change what the tool gets, like `--with pytest-cov` adds what
    /// `--cov` needs. `--frozen` and `--locked` are what the setup's runner does anyway, and uv's
    /// `--group` stays when the setup installs that group.
    fn options(&self, call: &Call) -> Option<Vec<String>> {
        let group = |name: &str| {
            let installed = self
                .uv
                .as_ref()
                .is_some_and(|(_, groups, _)| groups.contains(&normalized(name)));
            (call.runner == "uv run" && installed && is_plain_name(name))
                .then(|| format!("--group {name}"))
        };
        let mut kept = Vec::new();
        let mut options = call.options.iter();
        while let Some(option) = options.next() {
            match option.as_str() {
                "--frozen" | "--locked" | "--" => {}
                "--group" => kept.push(group(options.next()?)?),
                _ => kept.push(group(option.strip_prefix("--group=")?)?),
            }
        }
        Some(kept)
    }

    fn run(&self, tool: &str, module: bool, options: &[String]) -> String {
        let run = self.run.map(|run| {
            std::iter::once(run)
                .chain(options.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        });
        match (run, module) {
            (Some(run), true) => format!("{run} python -m {tool}"),
            (Some(run), false) => format!("{run} {tool}"),
            (None, true) => format!(".venv/bin/python -m {tool}"),
            (None, false) => format!(".venv/bin/{tool}"),
        }
    }
}

/// Whether a setup installs a JavaScript project's packages, and only those.
fn installs_node(setup: &[Vec<&str>]) -> bool {
    setup.iter().any(|words| match words.as_slice() {
        ["npm", "ci", ..] | ["yarn"] => true,
        ["npm" | "pnpm" | "bun", "install" | "i", args @ ..] | ["yarn", "install", args @ ..] => {
            args.iter().all(|arg| arg.starts_with('-'))
        }
        _ => false,
    })
}

/// The packages `package.json` depends on, which its lock installs.
fn node_packages(package: &str) -> HashSet<String> {
    let package: serde_json::Value = serde_json::from_str(package).unwrap_or_default();
    ["dependencies", "devDependencies"]
        .into_iter()
        .filter_map(|key| package[key].as_object())
        .flat_map(|names| names.keys().cloned())
        .collect()
}

fn package_of(bin: &str) -> &str {
    PACKAGES
        .iter()
        .find(|(name, _)| *name == bin)
        .map_or(bin, |(_, package)| package)
}

/// Each target of a Makefile, and the commands it runs without the `@` and `-` that only change how.
fn targets(makefile: &str) -> HashMap<String, Vec<String>> {
    let mut targets: HashMap<String, Vec<String>> = HashMap::new();
    let mut current: Vec<String> = Vec::new();
    for line in makefile.lines() {
        if let Some(command) = line.strip_prefix('\t') {
            let command = command.trim_start_matches(['@', '-']).trim();
            for name in &current {
                let lines = targets.entry(name.clone()).or_default();
                if lines.len() < MOST_LINES {
                    lines.push(command.to_owned());
                }
            }
        } else if line.starts_with(['#', ' ']) || line.trim().is_empty() {
            continue;
        } else {
            current = match line.split_once(':') {
                // `test := 1` and `x = test: 1` set a variable.
                Some((names, rest))
                    if !names.contains('=')
                        && !rest.starts_with('=')
                        && !rest.starts_with(":=") =>
                {
                    names.split_whitespace().map(str::to_owned).collect()
                }
                _ => Vec::new(),
            };
        }
    }
    targets
}

/// What a repo says about running its checks, read once.
pub(super) struct Repo {
    scripts: HashMap<String, String>,
    targets: HashMap<String, Vec<String>>,
    python: Option<PythonSetup>,
    node: Option<HashSet<String>>,
    own: Vec<&'static str>,
    /// What each script or target runs, by tool and mode, once read.
    expanded: HashMap<(bool, String), Vec<Sig>>,
}

impl Repo {
    /// What the repo says, and what the project's setup installs for its checks.
    pub(super) fn read(root: &Path, setup: &str) -> Self {
        let setup = steps(setup);
        let package = read(root, "package.json").unwrap_or_default();
        let scripts = scripts(&package, "scripts")
            .into_iter()
            .filter_map(|(name, text)| Some((name, text.as_str()?.to_owned())))
            .collect();
        let makefile = ["Makefile", "GNUmakefile", "makefile"]
            .into_iter()
            .find_map(|name| read(root, name))
            .unwrap_or_default();
        let own = [
            ("cargo", is_file(root, "Cargo.toml")),
            ("go", is_file(root, "go.mod")),
            ("mix", is_file(root, "mix.exs")),
            ("dotnet", dotnet(root).is_some()),
        ]
        .into_iter()
        .filter_map(|(program, there)| there.then_some(program))
        .collect();
        Self {
            scripts,
            targets: targets(&makefile),
            python: PythonSetup::of(root, &setup),
            node: node_tool(root)
                .filter(|_| installs_node(&setup))
                .map(|_| node_packages(&package)),
            own,
            expanded: HashMap::new(),
        }
    }

    /// Its own tool and mode first, then, for a script or a Makefile target, those of each command
    /// it runs, one level down: `npm test` is also the jest it runs.
    pub(super) fn sigs(&mut self, command: &str) -> Vec<Sig> {
        let Some(call) = Call::parse(command) else {
            return Vec::new();
        };
        let key = match &call.called {
            Called::Script(name) => Some((true, name.clone())),
            Called::Program(_) if call.name() == "make" => {
                Some((false, call.subcommand().to_owned()))
            }
            _ => None,
        };
        let mut sigs = vec![call.sig()];
        if let Some(key) = key {
            sigs.extend(self.expand(key));
        }
        sigs
    }

    fn expand(&mut self, key: (bool, String)) -> Vec<Sig> {
        if let Some(sigs) = self.expanded.get(&key) {
            return sigs.clone();
        }
        if self.expanded.len() >= MOST_EXPANSIONS {
            return Vec::new();
        }
        let lines: Vec<String> = if key.0 {
            self.scripts.get(&key.1).cloned().into_iter().collect()
        } else {
            self.targets.get(&key.1).cloned().unwrap_or_default()
        };
        let mut sigs: Vec<Sig> = Vec::new();
        for sig in lines
            .iter()
            .flat_map(|line| simple_commands(line))
            .filter_map(Call::parse)
            .map(|call| call.sig())
        {
            if !sigs.contains(&sig) {
                sigs.push(sig);
            }
        }
        self.expanded.insert(key, sigs.clone());
        sigs
    }

    /// What a check runs, every command of it.
    pub(super) fn checked(&mut self, check: &str) -> HashSet<Sig> {
        simple_commands(check)
            .into_iter()
            .flat_map(|command| self.sigs(command))
            .collect()
    }

    /// How Keel runs a simple command from a CI with no network: one on the list, whose tool the
    /// project's setup installs, in the form its stack runs it, like `uv run --frozen ruff check .`.
    /// Never one that sets a variable, whose runner's options can't come along, since what the tool
    /// is told can depend on them, or that runs a version of its own, as `uvx` does.
    pub(super) fn offline(&self, command: &str) -> Result<String, CiWhy> {
        let not = CiWhy::Offline;
        let changes =
            |word: &str| word.starts_with('+') || word.starts_with("--fix") || word == "--write";
        let words: Vec<&str> = command.split_whitespace().collect();
        if !is_plain(command)
            || words.iter().any(|word| changes(word) || fetches(word))
            || words.windows(2).any(|pair| pair == ["-mod", "mod"])
        {
            return Err(not);
        }
        let call = Call::parse(command).ok_or(not)?;
        if call.sets_env {
            return Err(CiWhy::Env);
        }
        if matches!(call.called, Called::Script(_))
            || matches!(call.runner.as_str(), "uvx" | "pipx run")
        {
            return Err(not);
        }
        let name = call.name();
        let args = call.args.join(" ");
        let with = |head: String| {
            if args.is_empty() {
                head
            } else {
                format!("{head} {args}")
            }
        };
        let form = match on_the_list(&call).ok_or(not)? {
            Installer::Python => {
                let python = self.python.as_ref().ok_or(not)?;
                let module = matches!(call.called, Called::Python { module: true, .. });
                let options = python.options(&call).ok_or(not)?;
                python
                    .installs
                    .contains(&normalized(&name))
                    .then(|| with(python.run(&name, module, &options)))
            }
            Installer::Node => (call
                .options
                .iter()
                .all(|option| NPX_ASKS.contains(&option.as_str()))
                && self
                    .node
                    .as_ref()
                    .is_some_and(|node| node.contains(package_of(&name))))
            .then(|| with(format!("node_modules/.bin/{name}"))),
            Installer::Own => (matches!(call.called, Called::Program(_))
                && self.own.contains(&name.as_str()))
            .then(|| with(name.clone())),
        };
        form.ok_or(not)
    }

    /// Whether a job's script installs more than the project's setup does: a package by name, a
    /// tool for the whole machine, or a group or extra the setup leaves out. A check may need it,
    /// and Keel's runs without it.
    pub(super) fn installs_more(&self, script: &str) -> bool {
        script
            .lines()
            .flat_map(|line| line.split(['&', ';', '|']))
            .any(|command| self.installs_beyond(command))
    }

    fn installs_beyond(&self, command: &str) -> bool {
        let words: Vec<&str> = command
            .split_whitespace()
            .skip_while(|word| {
                sets_variable(word)
                    || word.starts_with('-')
                    || matches!(*word, "sudo" | "then" | "do" | "else" | "{" | "(" | "!")
            })
            .collect();
        let Some((first, rest)) = words.split_first() else {
            return false;
        };
        let program = base(first);
        if SYSTEM_PACKAGES.contains(&program) {
            return rest.iter().any(|word| matches!(*word, "install" | "add"));
        }
        match (program, rest) {
            ("pip" | "pip3", ["install", args @ ..]) | ("uv", ["pip", "install", args @ ..]) => {
                self.pip_beyond(args)
            }
            (python, ["-m", "pip", "install", args @ ..]) if python.starts_with("python") => {
                self.pip_beyond(args)
            }
            ("uv", ["sync", args @ ..]) => self.uv_sync_beyond(args),
            ("poetry", ["install", args @ ..]) => args.iter().any(|arg| {
                ["--with", "-E", "--extras", "--all-extras", "--all-groups"]
                    .iter()
                    .any(|flag| arg.starts_with(flag))
            }),
            ("npm", ["install" | "i" | "add", args @ ..])
            | ("pnpm" | "bun", ["install" | "i", args @ ..]) => args
                .iter()
                .any(|arg| !arg.starts_with('-') || matches!(*arg, "-g" | "--global")),
            ("uv", ["add" | "tool", ..])
            | ("pipx", ["install" | "inject", ..])
            | ("poetry", ["add" | "self", ..])
            | ("pnpm" | "yarn" | "bun", ["add", ..])
            | ("yarn", ["global", ..])
            | ("cargo", ["install" | "binstall", ..])
            | ("go", ["install" | "get", ..])
            | ("gem", ["install", ..])
            | ("dotnet", ["tool", "install", ..])
            | ("composer", ["require" | "global", ..]) => true,
            _ => false,
        }
    }

    /// pip told to install anything but the setup's own lists, the project itself, or pip and
    /// the like.
    fn pip_beyond(&self, args: &[&str]) -> bool {
        let lists = self
            .python
            .as_ref()
            .map_or(&[][..], |python| &python.pip_lists[..]);
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let beyond = match *arg {
                "-r" | "--requirement" => args
                    .next()
                    .is_none_or(|list| !lists.iter().any(|known| known == list)),
                "-e" | "--editable" => args.next().is_none_or(|what| !is_the_project(what)),
                "-c" | "--constraint" | "-i" | "--index-url" | "--extra-index-url" | "-f"
                | "--find-links" => {
                    args.next();
                    false
                }
                flag if flag.starts_with('-') => false,
                what => {
                    !is_the_project(what) && !PACKAGING.contains(&requirement_name(what).as_str())
                }
            };
            if beyond {
                return true;
            }
        }
        false
    }

    /// `uv sync` told to install a group or an extra the setup's doesn't, or told something this
    /// doesn't follow.
    fn uv_sync_beyond(&self, args: &[&str]) -> bool {
        let Some((pyproject, groups, extras)) =
            self.python.as_ref().and_then(|python| python.uv.as_ref())
        else {
            return true;
        };
        uv_sync(pyproject, args)
            .is_none_or(|sync| !sync.groups.is_subset(groups) || !sync.extras.is_subset(extras))
    }
}

/// A command is checked if its own tool and mode are, or if everything its script runs is.
pub(super) fn is_checked(checked: &HashSet<Sig>, sigs: &[Sig]) -> bool {
    let Some((own, inner)) = sigs.split_first() else {
        return false;
    };
    checked.contains(own) || (!inner.is_empty() && inner.iter().all(|sig| checked.contains(sig)))
}
