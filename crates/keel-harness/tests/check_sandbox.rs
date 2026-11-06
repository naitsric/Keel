//! The sandbox for `check` steps, for real. Tests and Makefiles can be edited by an agent, so
//! what runs here gets no more trust than the agent.
#![cfg(target_os = "macos")]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use keel_harness::clean_env;
use keel_harness::settings::{check_profile, setup_profile, with_own_output};

struct Sandbox {
    canary_dir: tempfile::TempDir,
    canary: PathBuf,
    workspace: tempfile::TempDir,
    home: PathBuf,
}

fn sandbox() -> Sandbox {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    // A canary in the real home folder, and a workspace beside it, as in a real install.
    let canary_dir = tempfile::Builder::new()
        .prefix(".keel-sandbox-test")
        .tempdir_in(&home)
        .unwrap();
    let canary = canary_dir.path().join("secret.txt");
    std::fs::write(&canary, "secret").unwrap();
    let workspace = tempfile::Builder::new()
        .prefix(".keel-sandbox-workspace")
        .tempdir_in(&home)
        .unwrap();
    std::fs::write(workspace.path().join("ok.txt"), "fine").unwrap();
    Sandbox {
        canary_dir,
        canary,
        workspace,
        home,
    }
}

impl Sandbox {
    fn run(&self, script: &str) -> std::process::Output {
        let profile = check_profile(self.workspace.path(), &self.home, &[], &[]);
        // The environment a real check gets.
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/sh", "-c", script])
            .current_dir(self.workspace.path())
            .env_clear()
            .envs(clean_env(&std::env::var("PATH").unwrap()))
            .output()
            .unwrap()
    }

    fn allowed(&self, script: &str) -> bool {
        self.run(script).status.success()
    }
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy())
}

#[test]
fn exit_codes_come_through() {
    let s = sandbox();
    assert_eq!(s.run("exit 0").status.code(), Some(0));
    assert_eq!(s.run("exit 7").status.code(), Some(7));
    assert_eq!(
        String::from_utf8_lossy(&s.run("echo hello").stdout).trim(),
        "hello"
    );
}

#[test]
fn the_workspace_is_readable_and_writable() {
    let s = sandbox();
    assert!(s.allowed("cat ok.txt"));
    assert!(s.allowed("echo x > new.txt && cat new.txt && mkdir -p a/b && rm new.txt"));
}

#[test]
fn the_home_folder_is_not() {
    let s = sandbox();
    assert!(!s.allowed(&format!("cat {}", quoted(&s.canary))));
    assert!(!s.allowed(&format!("cat {}", quoted(&s.home.join(".ssh/known_hosts")))));
    assert!(!s.allowed(&format!(
        "cat {}",
        quoted(&s.home.join(".config/gh/hosts.yml"))
    )));
    assert!(!s.allowed(&format!(
        "touch {}",
        quoted(&s.home.join(".keel-sandbox-write"))
    )));
    assert!(!s.allowed(&format!("rm {}", quoted(&s.canary))));
    assert!(s.canary.exists());
}

#[test]
fn a_check_cannot_change_what_git_will_run() {
    let s = sandbox();
    let git = |script: &str| {
        Command::new("/bin/sh")
            .current_dir(s.workspace.path())
            .args(["-c", script])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(git(
        "git init -q -b main && git config user.name T && git config user.email t@e.c"
    ));
    let config = std::fs::read_to_string(s.workspace.path().join(".git/config")).unwrap();

    for attack in [
        "echo '[core]' >> .git/config",
        "rm .git/config",
        "mv .git/config .git/old",
        "echo x > .git/config.worktree",
        "echo /tmp > .git/commondir",
        "echo x > .git/hooks/pre-commit",
        "mv .git .git-old",
        "git config diff.external /tmp/x",
    ] {
        assert!(!s.allowed(attack), "{attack}");
    }
    assert_eq!(
        std::fs::read_to_string(s.workspace.path().join(".git/config")).unwrap(),
        config
    );
    assert!(s.workspace.path().join(".git/HEAD").exists());

    // Ordinary work still happens, and that's why the rest of `.git` stays open.
    assert!(s.allowed("git add -A && git commit -qm work && git branch other && git status"));
}

#[test]
fn git_works_when_the_home_folder_has_a_git_config() {
    // Most Macs have one. The sandbox hides it, and git gives up when it can't read it.
    let s = sandbox();
    std::fs::write(
        s.canary_dir.path().join(".gitconfig"),
        "[user]\n\tname = X\n",
    )
    .unwrap();
    // Keel makes the clone, outside the sandbox. A check can't create a `.git` of its own.
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(s.workspace.path())
        .status()
        .unwrap();
    assert!(init.success());
    let profile = check_profile(s.workspace.path(), &s.home, &[], &[]);
    let path = std::env::var("PATH").unwrap();
    let git_status = |env: Vec<(String, String)>| {
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/sh", "-c", "git status"])
            .current_dir(s.workspace.path())
            .env_clear()
            .envs(env)
            .env("HOME", s.canary_dir.path())
            .output()
            .unwrap()
    };
    let without = git_status(vec![("PATH".to_owned(), path.clone())]);
    assert!(
        !without.status.success(),
        "if this passes, the test proves nothing"
    );
    assert!(
        String::from_utf8_lossy(&without.stderr).contains(".gitconfig"),
        "{}",
        String::from_utf8_lossy(&without.stderr)
    );
    let with = git_status(clean_env(&path));
    assert!(
        with.status.success(),
        "{}",
        String::from_utf8_lossy(&with.stderr)
    );
}

/// Where a real install keeps a run: folders down inside the home folder, with its log beside the
/// workspace. Node and uv stopped dead there, and a workspace right in the home folder hid it.
struct DeepRun {
    root: tempfile::TempDir,
    runs: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
}

fn deep_run() -> DeepRun {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let root = tempfile::Builder::new()
        .prefix(".keel-sandbox-deep")
        .tempdir_in(&home)
        .unwrap();
    let runs = root.path().join("Application Support/runs/r1");
    let workspace = runs.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    DeepRun {
        root,
        runs,
        workspace,
        home,
    }
}

#[test]
fn a_command_can_find_its_way_to_a_workspace_deep_in_the_home_folder() {
    let d = deep_run();
    let other = d.runs.with_file_name("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(d.runs.join("secret.txt"), "secret").unwrap();
    let profile = check_profile(&d.workspace, &d.home, &[], &[]);
    let allowed = |script: &str| {
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/sh", "-c", script])
            .current_dir(&d.workspace)
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(allowed(&format!("ls -ld {}", quoted(&d.runs))));
    assert!(allowed(&format!("ls -ld {}", quoted(d.root.path()))));
    assert!(allowed(
        "/usr/bin/perl -MCwd -e 'exit(defined(Cwd::realpath(\".\")) ? 0 : 1)'"
    ));

    // That they exist, and nothing else.
    assert!(!allowed(&format!("ls {}", quoted(&d.runs))));
    assert!(!allowed(&format!(
        "cat {}",
        quoted(&d.runs.join("secret.txt"))
    )));
    assert!(!allowed(&format!("ls -ld {}", quoted(&other))));
    assert!(!allowed(&format!(
        "ls -ld {}",
        quoted(&d.runs.join("secret.txt"))
    )));
    assert!(!allowed(&format!(
        "touch {}",
        quoted(&d.runs.join("new.txt"))
    )));
}

#[test]
fn a_command_can_ask_about_its_own_output_files() {
    let d = deep_run();
    let log = d.runs.join("step.log");
    let asks = |profile: &str| {
        let output = std::fs::File::create(&log).unwrap();
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile, "/bin/sh", "-c", "/usr/bin/stat -f %z <&1"])
            .current_dir(&d.workspace)
            .stdout(output)
            .status()
            .unwrap()
            .success()
    };
    let without = check_profile(&d.workspace, &d.home, &[], &[]);
    assert!(!asks(&without), "if this passes, the test proves nothing");
    let with = with_own_output(without, &[&log]);
    assert!(asks(&with));
    // Only the file itself: what's written to it still gets there, and reading it doesn't work.
    assert!(
        !Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &with, "/bin/cat", &log.to_string_lossy()])
            .current_dir(&d.workspace)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn toolchains_can_be_read_and_cargo_credentials_cannot() {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let s = sandbox();
    // `~/.cargo/bin` opens up, and the file right next to it doesn't.
    let credentials = home.join(".cargo/credentials.toml");
    if credentials.exists() {
        assert!(!s.allowed(&format!("cat {}", quoted(&credentials))));
    }
    let profile = check_profile(s.workspace.path(), &home, &[], &[]);
    for dir in ["Library/pnpm", ".cargo/bin", ".rustup"] {
        assert!(
            profile.contains(&format!("(subpath \"{}\")", home.join(dir).display())),
            "{dir}"
        );
    }
    assert!(!profile.contains(&format!("(subpath \"{}\")", home.join(".cargo").display())));
}

#[test]
fn temp_folders_work_because_test_suites_need_them() {
    let s = sandbox();
    assert!(s.allowed("f=$(mktemp) && echo x > \"$f\" && rm \"$f\""));
}

#[test]
fn the_keychain_is_out_of_reach() {
    let s = sandbox();
    assert!(!s.allowed("security find-generic-password -s gh:github.com -w >/dev/null 2>&1"));
    assert!(
        !s.allowed(
            "security find-generic-password -s 'Claude Code-credentials' -w >/dev/null 2>&1"
        )
    );
}

#[test]
fn there_is_no_network() {
    let s = sandbox();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(!s.allowed(&format!(
        "curl -fsS -m 3 -o /dev/null http://127.0.0.1:{port}/"
    )));
    assert!(!s.allowed("curl -fsS -m 4 -o /dev/null https://example.com"));
}

#[test]
fn listed_paths_can_be_read() {
    let s = sandbox();
    let allowed = s.canary_dir.path().to_string_lossy().into_owned();
    let profile = check_profile(s.workspace.path(), &s.home, &[allowed], &[]);
    let output = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile, "/bin/cat", &s.canary.to_string_lossy()])
        .current_dir(s.workspace.path())
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn local_unix_sockets_are_out_of_reach_too() {
    // The Docker socket is one of these, and reaching it would undo the whole sandbox.
    let s = sandbox();
    let path = s.canary_dir.path().join("s.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let script = format!(
        "python3 -c \"import socket; c = socket.socket(socket.AF_UNIX); c.connect('{}')\"",
        path.to_string_lossy()
    );
    assert!(!s.allowed(&script));
}

#[test]
fn a_listed_socket_works_and_no_other_does() {
    let s = sandbox();
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let allowed_dir = root_path.join("db");
    let other_dir = root_path.join("other");
    std::fs::create_dir_all(&allowed_dir).unwrap();
    std::fs::create_dir_all(&other_dir).unwrap();
    let allowed = allowed_dir.join("db.sock");
    let other = other_dir.join("other.sock");
    let listener = std::os::unix::net::UnixListener::bind(&allowed).unwrap();
    let other_listener = std::os::unix::net::UnixListener::bind(&other).unwrap();
    std::thread::spawn(move || for _ in listener.incoming() {});
    std::thread::spawn(move || for _ in other_listener.incoming() {});
    let profile = check_profile(
        s.workspace.path(),
        &s.home,
        &[],
        &[allowed_dir.to_string_lossy().into_owned()],
    );
    let connect = |path: &Path| {
        Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                &profile,
                "python3",
                "-c",
                &format!(
                    "import socket; c = socket.socket(socket.AF_UNIX); c.connect('{}')",
                    path.display()
                ),
            ])
            .current_dir(s.workspace.path())
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(connect(&allowed), "the listed socket");
    assert!(!connect(&other), "any other socket");
}

fn run_setup(s: &Sandbox, cache: &Path, script: &str) -> bool {
    let profile = setup_profile(s.workspace.path(), &s.home, &[], &[], cache);
    Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile, "/bin/sh", "-c", script])
        .current_dir(s.workspace.path())
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn setup_can_reach_the_internet_and_its_cache() {
    let s = sandbox();
    let cache = tempfile::Builder::new()
        .prefix(".keel-sandbox-cache")
        .tempdir_in(&s.home)
        .unwrap();
    assert!(
        run_setup(
            &s,
            cache.path(),
            "curl -fsS -m 10 -o /dev/null https://pypi.org/pypi/pip/json"
        ),
        "the internet, including DNS"
    );
    assert!(
        run_setup(
            &s,
            cache.path(),
            &format!(
                "echo x > {}/f && cat {}/f",
                quoted(cache.path()),
                quoted(cache.path())
            )
        ),
        "its cache"
    );
    assert!(
        run_setup(&s, cache.path(), "echo x > installed.txt"),
        "the workspace"
    );
}

#[test]
fn setup_still_cannot_reach_anything_on_this_machine() {
    let s = sandbox();
    let cache = tempfile::Builder::new()
        .prefix(".keel-sandbox-cache")
        .tempdir_in(&s.home)
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(
        !run_setup(
            &s,
            cache.path(),
            &format!("curl -fsS -m 3 -o /dev/null http://127.0.0.1:{port}/")
        ),
        "local ports"
    );
    assert!(
        !run_setup(&s, cache.path(), &format!("cat {}", quoted(&s.canary))),
        "the home folder"
    );
    assert!(
        !run_setup(
            &s,
            cache.path(),
            &format!("touch {}", quoted(&s.home.join(".keel-sandbox-write")))
        ),
        "writes to home"
    );
    assert!(
        !run_setup(
            &s,
            cache.path(),
            "security find-generic-password -s gh:github.com -w >/dev/null 2>&1"
        ),
        "the keychain"
    );
    let path = s.canary_dir.path().join("s.sock");
    let _unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
    assert!(
        !run_setup(
            &s,
            cache.path(),
            &format!(
                "python3 -c \"import socket; c = socket.socket(socket.AF_UNIX); c.connect('{}')\"",
                path.to_string_lossy()
            )
        ),
        "local sockets"
    );
}

/// Where a real install keeps a project: its cache beside its mirror, apart from the run, with
/// other projects next door. uv stopped dead there, and a cache right in the home folder hid it.
#[test]
fn setup_can_find_its_way_to_its_cache_and_learns_nothing_else_on_the_way() {
    let d = deep_run();
    let projects = d.root.path().join("Application Support/projects");
    let project = projects.join("p1");
    let cache = project.join("cache");
    let mirror = project.join("mirror.git");
    let other_project = projects.join("p2");
    let other_cache = other_project.join("cache");
    for folder in [&cache, &mirror, &other_cache] {
        std::fs::create_dir_all(folder).unwrap();
    }
    std::fs::write(mirror.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(other_cache.join("wheel"), "theirs").unwrap();

    let run = |profile: &str, script: &str| {
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile, "/bin/sh", "-c", script])
            .current_dir(&d.workspace)
            .output()
            .unwrap()
    };
    let profile = setup_profile(&d.workspace, &d.home, &[], &[], &cache);
    let allowed = |script: &str| {
        let output = run(&profile, script);
        assert!(
            output.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    // `realpath` needs what uv needs here: to ask about every folder on the way down.
    let real_path = format!("/bin/realpath {}", quoted(&cache));
    assert_eq!(
        String::from_utf8_lossy(&allowed(&real_path).stdout).trim(),
        cache.canonicalize().unwrap().to_string_lossy()
    );
    let without = [&projects, &project]
        .iter()
        .fold(profile.clone(), |rules, folder| {
            rules.replace(&format!(" (literal \"{}\")", folder.display()), "")
        });
    assert_ne!(without, profile);
    assert!(
        !run(&without, &real_path).status.success(),
        "if this passes, the test proves nothing"
    );
    let inside = quoted(&cache.join("uv/sdists-v10"));
    allowed(&format!(
        "mkdir -p {inside} && echo x > {inside}/f && cat {inside}/f && mv {inside}/f {inside}/g"
    ));

    // That the folders on the way exist, and nothing else. Each of these is there, so being
    // refused can't be mistaken for not finding it.
    allowed(&format!("ls -ld {}", quoted(&projects)));
    allowed(&format!("ls -ld {}", quoted(&project)));
    for script in [
        format!("ls {}", quoted(&projects)),
        format!("ls {}", quoted(&project)),
        format!("ls {}", quoted(&mirror)),
        format!("ls -ld {}", quoted(&mirror)),
        format!("cat {}", quoted(&mirror.join("HEAD"))),
        format!("ls -ld {}", quoted(&other_project)),
        format!("ls {}", quoted(&other_cache)),
        format!("ls -ld {}", quoted(&other_cache)),
        format!("cat {}", quoted(&other_cache.join("wheel"))),
        format!("touch {}", quoted(&projects.join("new"))),
        format!("touch {}", quoted(&project.join("new"))),
        format!("echo x >> {}", quoted(&mirror.join("HEAD"))),
        format!("touch {}", quoted(&other_cache.join("poison"))),
    ] {
        let output = run(&profile, &script);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{script}");
        assert!(
            stderr.contains("Operation not permitted"),
            "{script}: {stderr}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(mirror.join("HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
    assert!(!other_cache.join("poison").exists());
    assert!(!project.join("new").exists());
    assert!(!projects.join("new").exists());
}

#[test]
fn the_cache_is_only_writable_during_setup() {
    let s = sandbox();
    let cache = tempfile::Builder::new()
        .prefix(".keel-sandbox-cache")
        .tempdir_in(&s.home)
        .unwrap();
    assert!(
        !s.allowed(&format!("touch {}/poison", quoted(cache.path()))),
        "a check can't write to the shared cache"
    );
    assert!(!cache.path().join("poison").exists());
}
