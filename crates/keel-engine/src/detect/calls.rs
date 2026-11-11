//! What a command from a CI calls once its wrappers are off: a Python package's tool, one of the
//! project's scripts, a binary of its JavaScript packages, or a program. Two commands that call the
//! same tool in the same mode check the same thing, and one whose tool the project's own setup
//! installs runs with no network, in the form its stack runs it. Only reads the repo.

use std::collections::HashSet;
use std::path::Path;

use super::{
    dotnet, is_file, is_real_script, node_tool, normalized, pip, read, requirement_name,
    run_script, scripts,
};

/// Words of tools that reach the network, drive a browser, or build environments of their own.
const NEEDS_NETWORK: [&str; 20] = [
    "e2e",
    "playwright",
    "cypress",
    "puppeteer",
    "selenium",
    "lighthouse",
    "chromatic",
    "semgrep",
    "terraform",
    "tox",
    "nox",
    "codecov",
    "coveralls",
    "sonar",
    "snyk",
    "trivy",
    "audit",
    "deny",
    "outdated",
    "pre-commit",
];
/// Programs whose first word after them says what they do: `cargo clippy`, `ruff format`.
const SUBCOMMANDS: [&str; 19] = [
    "cargo", "go", "ruff", "mix", "dotnet", "make", "zig", "deno", "biome", "gradle", "gradlew",
    "mvn", "mvnw", "swift", "flutter", "dart", "composer", "bun", "rake",
];
/// What each stack's own program runs with what its setup fetched, and the file that says the
/// stack is there. dotnet has no one file.
const OWN: [(&str, &str, &[&str]); 4] = [
    (
        "cargo",
        "Cargo.toml",
        &["test", "check", "clippy", "fmt", "doc"],
    ),
    ("go", "go.mod", &["test", "vet"]),
    ("mix", "mix.exs", &["test", "format"]),
    ("dotnet", "", &["test", "format", "build"]),
];
/// A binary whose package has another name.
const PACKAGES: [(&str, &str); 3] = [
    ("tsc", "typescript"),
    ("biome", "@biomejs/biome"),
    ("ng", "@angular/cli"),
];
/// `uv run` flags that don't change what runs.
const UV_RUN: [&str; 4] = ["--frozen", "--locked", "--no-sync", "--offline"];
/// `npx` flags that don't change what runs.
const NPX: [&str; 5] = ["--no-install", "--no", "--yes", "-y", "--offline"];
/// A script that runs a script can't go on for ever.
const MOST_DEPTH: usize = 3;

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

/// The words after these flags, or None for a flag it doesn't know, which can change what runs.
fn after_flags<'a, 'b>(words: &'a [&'b str], known: &[&str]) -> Option<&'a [&'b str]> {
    let at = words
        .iter()
        .position(|word| !word.starts_with('-'))
        .unwrap_or(words.len());
    words[..at]
        .iter()
        .all(|flag| known.contains(flag))
        .then(|| &words[at..])
}

fn python(words: &[&str]) -> Option<Call> {
    let (called, args) = match words {
        ["python" | "python3", "-m", tool, args @ ..] => (
            Called::Python {
                tool: (*tool).to_owned(),
                module: true,
            },
            args,
        ),
        [tool, args @ ..] => (
            Called::Python {
                tool: base(tool).to_owned(),
                module: false,
            },
            args,
        ),
        [] => return None,
    };
    Some(Call::new(called, args))
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
            .split_whitespace()
            .skip_while(|word| word.contains('=') && !word.starts_with('-'))
            .collect();
        let script = |name: &str, args: &[&str]| Some(Self::new(Called::Script(name.into()), args));
        match words.as_slice() {
            ["uv", "run", rest @ ..] => python(after_flags(rest, &UV_RUN)?),
            ["poetry" | "pipenv" | "pdm", "run", rest @ ..] | ["pipx", "run", rest @ ..] => {
                python(rest)
            }
            ["uvx", rest @ ..] => python(rest),
            [
                "python" | "python3" | ".venv/bin/python",
                "-m",
                tool,
                args @ ..,
            ] => {
                let tool = (*tool).to_owned();
                Some(Self::new(Called::Python { tool, module: true }, args))
            }
            ["npx" | "bunx", rest @ ..] => node(after_flags(rest, &NPX)?),
            ["pnpm" | "yarn" | "bun", "exec" | "x" | "dlx", rest @ ..] => node(rest),
            ["npm", "test" | "t", args @ ..] => script("test", args),
            [
                "npm" | "pnpm" | "yarn" | "bun",
                "run" | "run-script",
                name,
                args @ ..,
            ] => script(name, args),
            ["pnpm" | "yarn", name, args @ ..] if !name.starts_with('-') => script(name, args),
            [first, args @ ..] if first.starts_with("node_modules/.bin/") => {
                Some(Self::new(Called::Node(base(first).to_owned()), args))
            }
            [first, args @ ..] if first.starts_with(".venv/bin/") => {
                let tool = base(first).to_owned();
                Some(Self::new(
                    Called::Python {
                        tool,
                        module: false,
                    },
                    args,
                ))
            }
            [first, args @ ..] => Some(Self::new(Called::Program((*first).to_owned()), args)),
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

/// The commands in a line a shell runs one after the other.
fn chain(text: &str) -> Vec<&str> {
    text.split(['\n', ';'])
        .flat_map(|line| line.split("&&"))
        .flat_map(|line| line.split("||"))
        .map(|part| part.trim().trim_matches(['(', ')']).trim())
        .filter(|part| !part.is_empty())
        .collect()
}

fn script_text(root: &Path, name: &str) -> Option<String> {
    let package = read(root, "package.json")?;
    scripts(&package, "scripts")
        .get(name)?
        .as_str()
        .map(str::to_owned)
}

/// What a Makefile's target runs, a line each, without the `@` and `-` that only change how.
fn recipe(root: &Path, target: &str) -> Option<String> {
    let makefile = ["Makefile", "GNUmakefile", "makefile"]
        .into_iter()
        .find_map(|name| read(root, name))?;
    let mut lines = makefile.lines();
    lines.find(|line| {
        !line.starts_with(['\t', ' ', '#'])
            && line.split_once(':').is_some_and(|(targets, rest)| {
                !rest.starts_with('=') && targets.split_whitespace().any(|name| name == target)
            })
    })?;
    let commands: Vec<&str> = lines
        .map_while(|line| line.strip_prefix('\t'))
        .map(|line| line.trim_start_matches(['@', '-']))
        .collect();
    Some(commands.join("\n"))
}

/// What a command checks: its own tool and mode first, then those of what its script or Makefile
/// target runs, so `npm test` is also the jest it runs.
pub(super) fn sigs(root: &Path, command: &str) -> Vec<Sig> {
    sigs_within(root, command, 0)
}

fn sigs_within(root: &Path, command: &str, depth: usize) -> Vec<Sig> {
    let Some(call) = Call::parse(command) else {
        return Vec::new();
    };
    let mut found = vec![call.sig()];
    if depth < MOST_DEPTH {
        let inner = match &call.called {
            Called::Script(name) => script_text(root, name),
            Called::Program(_) if call.name() == "make" => recipe(root, call.subcommand()),
            _ => None,
        };
        for part in inner.iter().flat_map(|text| chain(text)) {
            found.extend(sigs_within(root, part, depth + 1));
        }
    }
    found
}

/// What a check runs, every command of it.
pub(super) fn checked(root: &Path, check: &str) -> HashSet<Sig> {
    chain(check)
        .into_iter()
        .flat_map(|part| sigs(root, part))
        .collect()
}

/// A command is checked if its own tool and mode are, or if everything its script runs is.
pub(super) fn is_checked(root: &Path, checked: &HashSet<Sig>, command: &str) -> bool {
    let sigs = sigs(root, command);
    let Some((own, inner)) = sigs.split_first() else {
        return false;
    };
    checked.contains(own) || (!inner.is_empty() && inner.iter().all(|sig| checked.contains(sig)))
}

pub(super) fn own_sig(command: &str) -> Option<Sig> {
    Call::parse(command).map(|call| call.sig())
}

fn needs_network(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("pre-commit")
        || lower
            .split(|c: char| c.is_whitespace() || ":-_./=()@".contains(c))
            .any(|word| NEEDS_NETWORK.contains(&word))
}

/// How a project's Python setup runs what it installed, and what that is.
struct PythonSetup {
    /// `uv run --frozen` or `poetry run`. None for the `.venv` that pip installs into.
    run: Option<&'static str>,
    installs: HashSet<String>,
}

impl PythonSetup {
    /// One tool per project, in the order `python` picks it.
    fn of(root: &Path) -> Option<Self> {
        let locked = |lock: String, run| {
            let installs = lock
                .lines()
                .filter_map(|line| line.strip_prefix("name = \""))
                .filter_map(|rest| rest.split('"').next())
                .map(normalized)
                .collect();
            Some(Self {
                run: Some(run),
                installs,
            })
        };
        if let Some(lock) = read(root, "uv.lock") {
            return locked(lock, "uv run --frozen");
        }
        if let (Some(lock), true) = (read(root, "poetry.lock"), is_file(root, "pyproject.toml")) {
            return locked(lock, "poetry run");
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

/// The packages `package.json` names, which its lock installs.
fn node_packages(root: &Path) -> HashSet<String> {
    let package = read(root, "package.json").unwrap_or_default();
    let package: serde_json::Value = serde_json::from_str(&package).unwrap_or_default();
    ["dependencies", "devDependencies", "optionalDependencies"]
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

/// How Keel runs a CI command with no network: a tool the project's setup installs, in the form
/// its stack runs it, like `uv run --frozen ruff check .`, or one of the stack's own commands, like
/// `cargo clippy`. None for anything else, and for what needs the network or another toolchain.
pub(super) fn offline(root: &Path, command: &str) -> Option<String> {
    if needs_network(command) || command.split_whitespace().any(|word| word.starts_with('+')) {
        return None;
    }
    let call = Call::parse(command)?;
    let args = call.args.join(" ");
    let with = |head: String| {
        if args.is_empty() {
            head
        } else {
            format!("{head} {args}")
        }
    };
    let name = call.name();
    if let Called::Script(script) = &call.called {
        let (tool, ..) = node_tool(root)?;
        let text = script_text(root, script)?;
        return (is_real_script(&text) && !needs_network(&text))
            .then(|| with(run_script(tool, script)));
    }
    let module = matches!(call.called, Called::Python { module: true, .. });
    if matches!(call.called, Called::Python { .. } | Called::Program(_))
        && let Some(setup) = PythonSetup::of(root)
        && setup.installs.contains(&normalized(&name))
    {
        return Some(with(setup.run(&name, module)));
    }
    if matches!(call.called, Called::Node(_) | Called::Program(_))
        && node_tool(root).is_some()
        && node_packages(root).contains(package_of(&name))
    {
        return Some(with(format!("node_modules/.bin/{name}")));
    }
    let Called::Program(_) = call.called else {
        return None;
    };
    let (program, _, does) = OWN.iter().find(|(program, file, _)| {
        *program == name
            && if file.is_empty() {
                dotnet(root).is_some()
            } else {
                is_file(root, file)
            }
    })?;
    does.contains(&call.subcommand())
        .then(|| with((*program).to_owned()))
}
