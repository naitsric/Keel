//! Sample repos, one for each stack, through what Keel does with a new project: it guesses the
//! commands, and then tries them as the setup dialog does, setup with the network and the check
//! without it, in the real sandbox, with Keel's own variables. The repos are in `tests/samples`.
//!
//! They download dependencies and take minutes, so they only run when asked:
//! `KEEL_SAMPLE_REPOS=all cargo test -p keel-engine --test samples -- --nocapture`, or a list such
//! as `KEEL_SAMPLE_REPOS=pnpm,uv`. `keel` is this repo itself, built from an empty `target/` as
//! its own check does, and isn't part of `all`: it takes the longest by far. A sample whose tools
//! this Mac doesn't have says so and stops.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::EnvVar;
use keel_engine::{Config, Engine};
use keel_harness::Tools;
use keel_harness::testing::skipped;
use keel_store::{Attempt, ProjectSettings, Store};

const VARIABLE: &str = "KEEL_SAMPLE_REPOS";
/// What `all` runs.
const STACKS: [&str; 4] = ["pnpm", "pip", "uv", "rust"];

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
    commands: Commands,
    /// Whether Keel has to guess exactly these, or is given them.
    guessed: bool,
    /// Where its tool keeps what it downloads, in the home folder, unless Keel told it otherwise.
    /// Setup has to have left it in the project's home.
    downloads: &'static str,
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
    let tools = Tools::detect();
    let dirs: Vec<PathBuf> = std::env::split_paths(&tools.path_env).collect();
    for program in sample.programs {
        if !dirs.iter().any(|dir| dir.join(program).is_file()) {
            skipped(&format!("{program} isn't installed"));
            return;
        }
    }

    // In the build folder, which is in the home folder like a real install.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
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
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let id = engine.add_project(&repo).unwrap().id;
    let Commands { setup, check } = sample.commands;
    if sample.guessed {
        let guess = engine.project_setup(&id).unwrap().guess;
        assert_eq!(guess.setup.as_deref(), Some(setup), "{guess:?}");
        assert_eq!(guess.check.as_deref(), Some(check), "{guess:?}");
    }
    // As many jobs as this test was given, if it was told, as a project would set it.
    if let Ok(jobs) = std::env::var("CARGO_BUILD_JOBS") {
        engine
            .update_project(
                &id,
                &ProjectSettings {
                    env: vec![EnvVar {
                        name: "CARGO_BUILD_JOBS".into(),
                        value: jobs,
                    }],
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
    }

    let started = Instant::now();
    let run = engine.try_check(&id, setup, check, "en").unwrap();
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
        let tail = |text: &str| {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(40)..].join("\n")
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
    // What setup downloaded is in the project's home, where the tool keeps it by itself.
    let project_home = dir
        .path()
        .join("data/projects")
        .join(&id)
        .join("cache/home");
    let left = listing(&project_home, 3);
    assert!(
        project_home.join(sample.downloads).is_dir(),
        "no {} in the project's home: {left:?}",
        sample.downloads
    );
    eprintln!(
        "sample {}: setup `{setup}` took {:.1}s, check `{check}` took {:.1}s\n\
         setup left in the project's home: {left:?}\n{}",
        sample.name,
        setup_took.as_secs_f64(),
        (total - setup_took).as_secs_f64(),
        output(&attempts[1])
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
fn pnpm() {
    try_sample(Sample {
        name: "pnpm",
        programs: &["pnpm", "node"],
        commands: Commands {
            setup: "pnpm install --frozen-lockfile",
            check: "pnpm test",
        },
        guessed: true,
        downloads: ".local/share/pnpm/store",
    });
}

#[test]
fn pip() {
    try_sample(Sample {
        name: "pip",
        programs: &["python3"],
        commands: Commands {
            setup: "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt",
            check: ".venv/bin/python -m pytest",
        },
        guessed: true,
        downloads: "Library/Caches/pip",
    });
}

#[test]
fn uv() {
    try_sample(Sample {
        name: "uv",
        programs: &["uv"],
        commands: Commands {
            setup: "uv sync --frozen",
            check: "uv run --frozen pytest",
        },
        guessed: true,
        downloads: ".cache/uv",
    });
}

#[test]
fn rust() {
    try_sample(Sample {
        name: "rust",
        programs: &["cargo"],
        commands: Commands {
            setup: "cargo fetch",
            check: "cargo test --workspace",
        },
        guessed: true,
        downloads: ".cargo/registry",
    });
}

/// #94: Keel's own check, from a clone with no `target/`. The app needs its frontend built to compile,
/// as in CI.
#[test]
fn keel() {
    try_sample(Sample {
        name: "keel",
        programs: &["pnpm", "node", "cargo"],
        commands: Commands {
            setup: "pnpm install --frozen-lockfile && cargo fetch",
            check: "test ! -e target && pnpm build && cargo test --workspace",
        },
        guessed: false,
        downloads: ".cargo/registry",
    });
}
