//! Which of a CI's checks Keel can run with no network, and what a check already runs. A command
//! is proposed only when it's one simple command, its tool and mode are on a short list of checks
//! that work offline, and the project's own setup installs that tool. Everything else is only
//! mentioned. Only reads the repo, and each file once.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::{
    dotnet, is_file, node_tool, normalized, pip, poetry_installs, read, requirement_name, scripts,
    uv_installs,
};

/// Programs whose first word after them says what they do: `cargo clippy`, `ruff format`.
const SUBCOMMANDS: [&str; 19] = [
    "cargo", "go", "ruff", "mix", "dotnet", "make", "zig", "deno", "biome", "gradle", "gradlew",
    "mvn", "mvnw", "swift", "flutter", "dart", "composer", "bun", "rake",
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

/// The words after a runner's options: `--flag`, `--flag=value`, and `--flag value` for those that
/// take one.
fn after_options<'a, 'b>(words: &'a [&'b str], valued: &[&str]) -> &'a [&'b str] {
    let mut at = 0;
    while let Some(word) = words.get(at) {
        if !word.starts_with('-') {
            break;
        }
        at += if valued.contains(word) { 2 } else { 1 };
    }
    &words[at.min(words.len())..]
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
        }
    }

    /// What its first words say it calls. `FOO=1` in front only sets a variable for it.
    fn parse(command: &str) -> Option<Self> {
        let words: Vec<&str> = command
            .trim_matches(['(', ')'])
            .split_whitespace()
            .skip_while(|word| word.contains('=') && !word.starts_with('-'))
            .collect();
        let script = |name: &str, args: &[&str]| Some(Self::new(Called::Script(name.into()), args));
        match words.as_slice() {
            ["uv" | "poetry" | "pipenv" | "pdm", "run", rest @ ..] => {
                python(after_options(rest, &VALUED))
            }
            ["pipx", "run", rest @ ..] => python(after_options(rest, &VALUED)),
            ["uvx", rest @ ..] => python(after_options(rest, &VALUED)),
            [
                "python" | "python3" | ".venv/bin/python",
                "-m",
                tool,
                args @ ..,
            ] => python(&[&["python", "-m", *tool][..], args].concat()),
            ["npx" | "bunx", rest @ ..] => {
                // `-c` runs a line of its own.
                if rest.iter().any(|word| *word == "-c" || *word == "--call") {
                    return None;
                }
                node(after_options(rest, &NPX_VALUED))
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
        let mode = if SUBCOMMANDS.contains(&tool.as_str()) {
            self.subcommand().to_owned()
        } else {
            String::new()
        };
        Sig { tool, mode }
    }
}

/// What a line runs one after the other: split at new lines, `;` and `&&`. A line in parentheses
/// as a whole runs the same.
pub(super) fn simple_commands(line: &str) -> Vec<&str> {
    let line = line.trim();
    let line = match line
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
    {
        Some(inner) if !inner.contains(['(', ')']) => inner,
        _ => line,
    };
    line.split(['\n', ';'])
        .flat_map(|part| part.split("&&"))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

/// Nothing a shell does besides running it: no pipe, redirection, substitution, variable,
/// background, `||`, subshell, escape or home folder, and quotes that close.
fn is_plain(command: &str) -> bool {
    !command.contains(['|', '<', '>', '$', '`', '&', '(', ')', ';', '\\', '~'])
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

/// How a project's Python setup runs what it installs, and what that is when nothing more is asked.
struct PythonSetup {
    /// `uv run --frozen` or `poetry run`. None for the `.venv` that pip installs into.
    run: Option<&'static str>,
    installs: HashSet<String>,
}

impl PythonSetup {
    /// One tool per project, in the order `python` picks it.
    fn of(root: &Path) -> Option<Self> {
        let pyproject = || read(root, "pyproject.toml").unwrap_or_default();
        if read(root, "uv.lock").is_some() {
            return Some(Self {
                run: Some("uv run --frozen"),
                installs: uv_installs(&pyproject()),
            });
        }
        if read(root, "poetry.lock").is_some() && is_file(root, "pyproject.toml") {
            return Some(Self {
                run: Some("poetry run"),
                installs: poetry_installs(&pyproject()),
            });
        }
        let installs = pip(root)?
            .from
            .iter()
            .filter_map(|list| read(root, list))
            .flat_map(|text| text.lines().map(requirement_name).collect::<Vec<_>>())
            .collect();
        Some(Self {
            run: None,
            installs,
        })
    }

    fn run(&self, tool: &str, module: bool) -> String {
        match (self.run, module) {
            (Some(run), true) => format!("{run} python -m {tool}"),
            (Some(run), false) => format!("{run} {tool}"),
            (None, true) => format!(".venv/bin/python -m {tool}"),
            (None, false) => format!(".venv/bin/{tool}"),
        }
    }
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
    pub(super) fn read(root: &Path) -> Self {
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
            python: PythonSetup::of(root),
            node: node_tool(root).map(|_| node_packages(&package)),
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
    pub(super) fn offline(&self, command: &str) -> Option<String> {
        let changes =
            |word: &str| word.starts_with('+') || word.starts_with("--fix") || word == "--write";
        if !is_plain(command) || command.split_whitespace().any(changes) {
            return None;
        }
        let call = Call::parse(command)?;
        if matches!(call.called, Called::Script(_)) {
            return None;
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
        match on_the_list(&call)? {
            Installer::Python => {
                let python = self.python.as_ref()?;
                let module = matches!(call.called, Called::Python { module: true, .. });
                python
                    .installs
                    .contains(&normalized(&name))
                    .then(|| with(python.run(&name, module)))
            }
            Installer::Node => self
                .node
                .as_ref()?
                .contains(package_of(&name))
                .then(|| with(format!("node_modules/.bin/{name}"))),
            Installer::Own => (matches!(call.called, Called::Program(_))
                && self.own.contains(&name.as_str()))
            .then(|| with(name.clone())),
        }
    }
}

/// A command is checked if its own tool and mode are, or if everything its script runs is.
pub(super) fn is_checked(checked: &HashSet<Sig>, sigs: &[Sig]) -> bool {
    let Some((own, inner)) = sigs.split_first() else {
        return false;
    };
    checked.contains(own) || (!inner.is_empty() && inner.iter().all(|sig| checked.contains(sig)))
}
