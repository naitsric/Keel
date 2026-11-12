//! Sample repos, one for each stack, through what Keel does with a new project: it guesses the
//! commands, finds where their programs are in the home folder, and then tries them as the setup
//! dialog does, setup with the network and the check without it, in the real sandbox, with Keel's
//! own variables and only the folders it found. The repos are in `tests/samples`. Then a check of
//! its own tries something setup didn't download, which has to fail in seconds (#121). A sample
//! whose tests start a server on localhost passes with its project's setting for that on, and with
//! it off fails saying so (#123). So does one whose tool needs localhost, which Keel proposes to
//! open, as the setup dialog does, and tries before anything is saved (#124).
//!
//! They download dependencies and take minutes, so they only run when asked:
//! `KEEL_SAMPLE_REPOS=all cargo test -p keel-engine --test samples -- --nocapture`, or a list such
//! as `KEEL_SAMPLE_REPOS=pnpm,uv`. `keel` is this repo itself, built from an empty `target/` as
//! its own check does, and isn't part of `all`: it takes the longest by far. A sample whose tools
//! this Mac doesn't have says so and stops. .NET, Dart, Elixir, Deno and Bun have none yet.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{EnvVar, Reason};
use keel_engine::{Config, Engine};
use keel_harness::Tools;
use keel_harness::testing::skipped;
use keel_store::{Attempt, ProjectSettings, Store};

const VARIABLE: &str = "KEEL_SAMPLE_REPOS";
/// What `all` runs.
const STACKS: [&str; 14] = [
    "npm",
    "npm-server",
    "pnpm",
    "yarn",
    "pip",
    "uv",
    "poetry",
    "rust",
    "go",
    "maven",
    "gradle",
    "bundler",
    "composer",
    "swift",
];

fn wanted(name: &str) -> bool {
    std::env::var(VARIABLE).is_ok_and(|list| {
        list.split(',')
            .map(str::trim)
            .any(|item| item == name || (item == "all" && STACKS.contains(&name)))
    })
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn copy_files(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_files(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The commands a step of this sample runs, or what Keel should guess for it.
struct Commands {
    setup: &'static str,
    check: &'static str,
}

struct Sample {
    name: &'static str,
    /// What it can't do without, looked for where Keel looks.
    programs: &'static [&'static str],
    /// Folders in front of the PATH, as a person who uses this toolchain has them. Homebrew keeps
    /// its Java and Ruby off the PATH, and macOS has a Ruby of its own that's too old.
    path: &'static [&'static str],
    /// The folder Keel has to find for a program, when that program is in the home folder. Every
    /// project could read these before #120, so a check that passes here shows it doesn't need to.
    finds: &'static [(&'static str, &'static str)],
    commands: Commands,
    /// Whether Keel has to guess exactly these, or is given them.
    guessed: bool,
    /// Where its tool keeps what it downloads, in the home folder, unless Keel told it otherwise.
    /// Setup has to have left it in the project's home. `None` for a sample with nothing to download.
    downloads: Option<&'static str>,
    /// A command that needs something setup didn't download, and what its tool says as it fails.
    /// In a check, with no network, that has to happen in seconds and not after minutes of retries.
    needs_download: Option<(&'static str, &'static str)>,
    /// How many seconds that may take, for a tool with no way to work offline.
    fails_within: u64,
    /// What the person's own setup gives its commands, in the project's settings.
    env: &'static [(&'static str, &'static str)],
    /// Its tests start a server on localhost, so its project opens localhost to them (#123).
    starts_servers: bool,
    /// Its tool talks to the processes it starts on localhost, so Keel proposes opening it.
    proposes_localhost: bool,
    /// Its tool reads what's in the folders above the repo, and above the build folder is Keel's
    /// own repo, which the sandbox refuses. A real install has nothing there, so this runs in the
    /// system's temp folder instead, outside the home folder.
    reads_above: bool,
}

/// What took a check from when Keel started its command to when it ended.
fn took(store: &Store, attempt: &Attempt) -> Duration {
    let events = store.events(&attempt.id, 0).unwrap();
    let at = |kind: &str| {
        let event = events.iter().find(|event| event.kind == kind).unwrap();
        chrono::DateTime::parse_from_rfc3339(&event.at).unwrap()
    };
    (at("checkResult") - at("started")).to_std().unwrap()
}

fn wait_for(store: &Store, run: &str, deadline: Duration) {
    let started = Instant::now();
    while store.run(run).unwrap().finished_at.is_none() {
        assert!(started.elapsed() < deadline, "the run never finished");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn try_sample(sample: Sample) {
    if !wanted(sample.name) {
        skipped(&format!(
            "it downloads dependencies, so it runs only with {VARIABLE} naming it or `all`"
        ));
        return;
    }
    keel_harness::needs!(sandbox);
    // The PATH the app gives commands, from the login shell.
    let mut tools = Tools::detect();
    let mut dirs: Vec<PathBuf> = sample.path.iter().map(PathBuf::from).collect();
    dirs.extend(std::env::split_paths(&tools.path_env));
    tools.path_env = std::env::join_paths(&dirs)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    for program in sample.programs {
        if !dirs.iter().any(|dir| dir.join(program).is_file()) {
            skipped(&format!("{program} isn't installed"));
            return;
        }
    }
    for (name, value) in sample.env {
        if value.starts_with('/') && !Path::new(value).exists() {
            skipped(&format!("{value}, for {name}, isn't there"));
            return;
        }
    }

    // In the build folder, which is in the home folder like a real install.
    let dir = if sample.reads_above {
        tempfile::tempdir().unwrap()
    } else {
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
    };
    let repo = dir.path().join(sample.name);
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    if sample.name == "keel" {
        let root = manifest.join("../..");
        git(
            dir.path(),
            &[
                "clone",
                "-q",
                "--no-hardlinks",
                &root.to_string_lossy(),
                &repo.to_string_lossy(),
            ],
        );
    } else {
        copy_files(&manifest.join("tests/samples").join(sample.name), &repo);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "T"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "sample"]);
    }

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let mut config = Config::new(dir.path().join("data"));
    config.keep_awake = false;
    let path_env = tools.path_env.clone();
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let id = engine.add_project(&repo).unwrap().id;
    let Commands { setup, check } = sample.commands;
    // Localhost as the setup dialog has it: ticked when Keel proposes it, and tried with it open
    // before anything is saved.
    let mut proposed = false;
    if sample.guessed {
        let guess = engine.project_setup(&id).unwrap().guess;
        assert_eq!(guess.setup.as_deref(), Some(setup), "{guess:?}");
        assert_eq!(guess.check.as_deref(), Some(check), "{guess:?}");
        assert_eq!(guess.localhost, sample.proposes_localhost, "{guess:?}");
        proposed = guess.localhost;
    }
    let localhost = sample.starts_servers || proposed;
    // As many jobs as this test was given, if it was told, as a project would set it, what the
    // person's setup gives, and localhost open to tests that start servers there.
    let settings = |allow_localhost: bool| ProjectSettings {
        env: std::env::var("CARGO_BUILD_JOBS")
            .map(|jobs| ("CARGO_BUILD_JOBS", jobs))
            .into_iter()
            .chain(
                sample
                    .env
                    .iter()
                    .map(|(name, value)| (*name, (*value).to_owned())),
            )
            .map(|(name, value)| EnvVar {
                name: name.into(),
                value,
            })
            .collect(),
        allow_localhost,
        ..ProjectSettings::default()
    };
    engine
        .update_project(&id, &settings(sample.starts_servers))
        .unwrap();

    // As the setup dialog does: Keel finds where in the home folder the programs are, and lets the
    // commands read that, with nothing else from the home folder.
    let found = engine.tool_folders(&id, setup, check).unwrap();
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    for (program, folder) in sample.finds {
        let located = keel_harness::toolchains::locate(&[(*program).to_owned()], &path_env);
        if located.first().is_some_and(|(_, at)| at.starts_with(&home)) {
            assert!(
                found.iter().any(|found| found.folder == *folder),
                "{program} needs {folder}: {found:?}"
            );
        }
    }
    let read: Vec<String> = found.iter().map(|found| found.folder.clone()).collect();

    let started = Instant::now();
    let run = engine
        .try_check_opening(&id, setup, check, &read, proposed, "en")
        .unwrap();
    let mut setup_took = None;
    let deadline = Duration::from_secs(if sample.name == "keel" { 3600 } else { 1200 });
    while store.run(&run).unwrap().finished_at.is_none() {
        assert!(started.elapsed() < deadline, "the run never finished");
        if setup_took.is_none()
            && store
                .attempts_of_run(&run)
                .unwrap()
                .first()
                .is_some_and(|setup| setup.finished_at.is_some())
        {
            setup_took = Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let total = started.elapsed();
    let setup_took = setup_took.unwrap_or(total);
    let attempts = store.attempts_of_run(&run).unwrap();
    let output = |attempt: &Attempt| {
        let data = store
            .events(&attempt.id, 0)
            .unwrap()
            .into_iter()
            .find(|event| event.kind == "checkResult")
            .map(|event| event.data)
            .unwrap_or_default();
        // Where it went wrong is often at the start, and how it ended at the end.
        let tail = |text: &str| {
            let lines: Vec<&str> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            if lines.len() <= 60 {
                return lines.join("\n");
            }
            format!(
                "{}\n…\n{}",
                lines[..20].join("\n"),
                lines[lines.len() - 40..].join("\n")
            )
        };
        format!(
            "{}\n{}",
            tail(data["stdout"].as_str().unwrap_or("")),
            tail(data["stderr"].as_str().unwrap_or(""))
        )
    };
    for attempt in &attempts {
        assert_eq!(
            attempt.state,
            "succeeded",
            "{} `{}` failed:\n{}",
            attempt.kind,
            if attempt.kind == "setup" {
                setup
            } else {
                check
            },
            output(attempt)
        );
    }
    let kinds: Vec<&str> = attempts.iter().map(|a| a.kind.as_str()).collect();
    assert_eq!(kinds, ["setup", "check"]);
    // The check had no network, and only what setup left.
    let profile = store
        .events(&attempts[1].id, 0)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "started")
        .unwrap()
        .data["profile"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(profile.contains("(deny network*)"), "{profile}");
    assert_eq!(
        profile.contains("(local ip \"localhost:*\")"),
        localhost,
        "{profile}"
    );
    // What setup downloaded is in the project's home, where the tool keeps it by itself.
    let project_home = dir
        .path()
        .join("data/projects")
        .join(&id)
        .join("cache/home");
    let left = listing(&project_home, 3);
    if let Some(downloads) = sample.downloads {
        assert!(
            project_home.join(downloads).is_dir(),
            "no {downloads} in the project's home: {left:?}",
        );
    }
    eprintln!(
        "sample {}: Keel found {found:?}\n\
         setup `{setup}` took {:.1}s, check `{check}` took {:.1}s\n\
         setup left in the project's home: {left:?}\n{}",
        sample.name,
        setup_took.as_secs_f64(),
        (total - setup_took).as_secs_f64(),
        output(&attempts[1])
    );

    // With localhost closed, as every project has it until a person opens it, the same check fails,
    // and says it was localhost.
    if localhost {
        engine.update_project(&id, &settings(false)).unwrap();
        let run = engine
            .try_check_reading(&id, setup, check, &read, "en")
            .unwrap();
        wait_for(&store, &run, Duration::from_secs(600));
        let attempts = store.attempts_of_run(&run).unwrap();
        let kinds: Vec<&str> = attempts.iter().map(|a| a.kind.as_str()).collect();
        assert_eq!(kinds, ["setup", "check"]);
        assert_eq!(attempts[0].state, "succeeded", "{}", output(&attempts[0]));
        let closed = &attempts[1];
        let said = output(closed);
        eprintln!("sample {}: with localhost closed:\n{said}", sample.name);
        assert_eq!(closed.state, "failed", "{said}");
        let reason: Reason = serde_json::from_value(closed.reason.clone().unwrap()).unwrap();
        assert!(
            matches!(
                reason,
                Reason::CheckFailed {
                    localhost_refused: true,
                    ..
                }
            ),
            "{reason:?}: {said}"
        );
        assert!(
            said.contains("EPERM") || said.contains("Operation not permitted"),
            "{said}"
        );
        engine
            .update_project(&id, &settings(sample.starts_servers))
            .unwrap();
    }

    // A run of its own, whose setup finds everything already downloaded. Some of it, such as pip's
    // `.venv`, is in the run's copy of the repo.
    let Some((command, says)) = sample.needs_download else {
        return;
    };
    let run = engine
        .try_check_opening(&id, setup, command, &read, proposed, "en")
        .unwrap();
    wait_for(&store, &run, Duration::from_secs(600));
    let attempts = store.attempts_of_run(&run).unwrap();
    let kinds: Vec<&str> = attempts.iter().map(|a| a.kind.as_str()).collect();
    assert_eq!(kinds, ["setup", "check"]);
    assert_eq!(attempts[0].state, "succeeded", "{}", output(&attempts[0]));
    let check = &attempts[1];
    let (said, took) = (output(check), took(&store, check));
    eprintln!(
        "sample {}: `{command}` failed in {:.1}s:\n{said}",
        sample.name,
        took.as_secs_f64()
    );
    assert_eq!(check.state, "failed", "`{command}`: {said}");
    assert!(said.contains(says), "`{command}`: {said}");
    assert!(
        took < Duration::from_secs(sample.fails_within),
        "`{command}` took {took:?}: {said}"
    );
}

/// The folders in `dir`, this many levels down.
fn listing(dir: &Path, levels: usize) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let inside = if levels > 1 {
                listing(&entry.path(), levels - 1)
            } else {
                Vec::new()
            };
            if inside.is_empty() {
                found.push(name);
            } else {
                found.extend(inside.into_iter().map(|path| format!("{name}/{path}")));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn npm() {
    try_sample(Sample {
        name: "npm",
        programs: &["npm", "node"],
        path: &[],
        finds: &[],
        commands: Commands {
            setup: "npm ci",
            check: "npm test",
        },
        guessed: true,
        downloads: Some(".npm/_cacache"),
        needs_download: Some(("npm install left-pad", "ENOTCACHED")),
        fails_within: 5,
        env: &[],
        starts_servers: false,
        proposes_localhost: false,
        reads_above: false,
    });
}

/// #123: a suite that starts its own server on localhost and talks to it, with nothing to download.
#[test]
fn npm_server() {
    try_sample(Sample {
        name: "npm-server",
        programs: &["npm", "node"],
        path: &[],
        finds: &[],
        commands: Commands {
            setup: "npm ci",
            check: "npm test",
        },
        guessed: true,
        downloads: None,
        needs_download: None,
        fails_within: 5,
        env: &[],
        starts_servers: true,
        proposes_localhost: false,
        reads_above: false,
    });
}

#[test]
fn pnpm() {
    try_sample(Sample {
        name: "pnpm",
        programs: &["pnpm", "node"],
        path: &[],
        finds: &[("pnpm", "~/Library/pnpm")],
        commands: Commands {
            setup: "pnpm install --frozen-lockfile",
            check: "pnpm test",
        },
        guessed: true,
        downloads: Some(".local/share/pnpm/store"),
        needs_download: Some(("pnpm add left-pad", "ERR_PNPM_")),
        fails_within: 5,
        env: &[],
        starts_servers: false,
        proposes_localhost: false,
        reads_above: false,
    });
}

#[test]
fn pip() {
    try_sample(Sample {
        name: "pip",
        programs: &["python3"],
        path: &[],
        finds: &[],
        commands: Commands {
            setup: "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt",
            check: ".venv/bin/python -m pytest",
        },
        guessed: true,
        downloads: Some("Library/Caches/pip"),
        needs_download: Some((
            ".venv/bin/pip install six",
            "No matching distribution found for six",
        )),
        fails_within: 5,
        env: &[],
        starts_servers: false,
        proposes_localhost: false,
        reads_above: false,
    });
}

#[test]
fn uv() {
    try_sample(Sample {
        name: "uv",
        programs: &["uv"],
        path: &[],
        finds: &[],
        commands: Commands {
            setup: "uv sync --frozen",
            check: "uv run --frozen pytest",
        },
        guessed: true,
        downloads: Some(".cache/uv"),
        needs_download: Some(("uv add six", "network was disabled")),
        fails_within: 5,
        env: &[],
        starts_servers: false,
        proposes_localhost: false,
        reads_above: false,
    });
}

#[test]
fn rust() {
    try_sample(Sample {
        name: "rust",
        programs: &["cargo"],
        path: &[],
        finds: &[("cargo", "~/.cargo/bin"), ("cargo", "~/.rustup")],
        commands: Commands {
            setup: "cargo fetch",
            check: "cargo test --workspace",
        },
        guessed: true,
        downloads: Some(".cargo/registry"),
        needs_download: Some(("cargo add memchr", "could not be found in registry index")),
        fails_within: 5,
        env: &[],
        starts_servers: false,
        proposes_localhost: false,
        reads_above: false,
    });
}

#[test]
fn go() {
    try_sample(Sample {
        name: "go",
        programs: &["go"],
        path: &[],
        finds: &[],
        commands: Commands {
            setup: "go mod download",
            check: "go test ./...",
        },
        guessed: true,
        downloads: Some("go/pkg/mod"),
        needs_download: Some((
            "go get golang.org/x/text@v0.20.0",
            "module lookup disabled by GOPROXY=off",
        )),
        fails_within: 5,
        env: &[],
        starts_servers: false,
        proposes_localhost: false,
        reads_above: false,
    });
}

/// #94: Keel's own check, from a clone with no `target/`. The app needs its frontend built to compile,
/// as in CI.
#[test]
fn keel() {
    try_sample(Sample {
        name: "keel",
        programs: &["pnpm", "node", "cargo"],
        path: &[],
        finds: &[
            ("pnpm", "~/Library/pnpm"),
            ("cargo", "~/.cargo/bin"),
            ("cargo", "~/.rustup"),
        ],
        commands: Commands {
            setup: "pnpm install --frozen-lockfile && cargo fetch",
            check: "test ! -e target && pnpm build && cargo test --workspace",
        },
        guessed: false,
        downloads: Some(".cargo/registry"),
        needs_download: None,
        fails_within: 5,
        env: &[],
        starts_servers: false,
        proposes_localhost: false,
        reads_above: false,
    });
}
